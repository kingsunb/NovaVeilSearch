use reqwest::{Client, Proxy, Response};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

use crate::error::{NovaVeilSearchError, Result};

use super::keyring::{is_key_scoped_status, KeyRing};

/// Parse a proxy URL (`http://`, `https://`, `socks5://`, `socks5h://`,
/// optionally with embedded credentials) into a reqwest [`Proxy`] that applies
/// to every protocol. `None` when the value is empty/whitespace. An unparsable
/// URL — or one with an unsupported scheme — logs a warning and degrades to
/// `None` (direct) rather than failing the whole process: proxy config is an
/// operator hint, and a typo there should not take a working search setup
/// offline.
pub fn proxy_from_url(url: &str) -> Option<Proxy> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    match reqwest::Url::parse(trimmed) {
        Ok(parsed) => match parsed.scheme() {
            "http" | "https" | "socks5" | "socks5h" => match Proxy::all(parsed) {
                Ok(proxy) => Some(proxy),
                Err(err) => {
                    eprintln!(
                        "nova-veil-search: could not apply proxy: {}; connecting directly",
                        err.without_url()
                    );
                    None
                }
            },
            other => {
                eprintln!(
                    "nova-veil-search: unsupported proxy scheme \"{other}\" (expected http, https, socks5, socks5h); connecting directly"
                );
                None
            }
        },
        Err(err) => {
            eprintln!("nova-veil-search: ignoring invalid proxy URL: {err}; connecting directly");
            None
        }
    }
}

/// Placeholder inside a proxy template's username that is replaced with a
/// per-key derived account alias (parity with NovaVeil's `{account}` contract).
pub const PROXY_ACCOUNT_PLACEHOLDER: &str = "{account}";

/// Look only in the decoded username. Encoded placeholders are equivalent to
/// literal ones; a placeholder in the password must not trigger account routing.
fn has_account_placeholder(template: &str) -> bool {
    reqwest::Url::parse(template.trim()).is_ok_and(|url| {
        percent_encoding::percent_decode_str(url.username())
            .decode_utf8_lossy()
            .contains(PROXY_ACCOUNT_PLACEHOLDER)
    })
}

/// Deterministic, non-reversible 8-hex alias identifying one provider's key,
/// mirroring NovaVeil's `AccountAliasFor`: `sha256("{provider}:{key}")`
/// truncated to 8 hex chars. Stable across requests and restarts (so a proxy
/// vendor can track one key), distinct for every (provider, key) pair, and
/// revealing nothing about the key itself.
pub fn account_alias(provider: &str, key: &str) -> String {
    let material = format!("{provider}:{key}");
    let digest = ring::digest::digest(&ring::digest::SHA256, material.as_bytes());
    let mut alias = String::with_capacity(8);
    for byte in digest.as_ref().iter().take(4) {
        alias.push_str(&format!("{byte:02x}"));
    }
    alias
}

/// Resolve the `{account}` placeholder in a proxy template's username only,
/// following NovaVeil's contract: the password and the rest of the URL are
/// preserved, and the URL is rebuilt through `reqwest::Url`'s userinfo API
/// (never string concatenation) so special characters in the alias are escaped
/// safely. Returns `Some(template)` unchanged when the template has no
/// placeholder or no userinfo; `None` when the template is empty or unparsable.
pub fn resolve_proxy_template(template: &str, account: &str) -> Option<String> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parsed = match reqwest::Url::parse(trimmed) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("nova-veil-search: invalid proxy template: {err}; connecting directly");
            return None;
        }
    };
    // Decode exactly once for substitution. Url::set_username escapes reserved
    // characters but preserves '%', so escape literal percent signs ourselves
    // to avoid turning e.g. "%2540" into "@" in the SOCKS authentication bytes.
    let username = percent_encoding::percent_decode_str(parsed.username())
        .decode_utf8_lossy()
        .into_owned();
    if !username.contains(PROXY_ACCOUNT_PLACEHOLDER) {
        return Some(trimmed.to_string());
    }
    let username = username
        .replace(PROXY_ACCOUNT_PLACEHOLDER, account)
        .replace('%', "%25");
    if parsed.set_username(&username).is_err() {
        eprintln!(
            "nova-veil-search: could not rewrite proxy template username; connecting directly"
        );
        return None;
    }
    Some(parsed.to_string())
}

/// Resolve the keyed proxy URL for one provider's key. When the template names
/// `{account}`, substitute that provider's derived alias; otherwise the template
/// is returned verbatim (a fixed proxy shared by every keyed provider). `None`
/// means "connect directly": no template, an empty template, or an `{account}`
/// template paired with an empty key (keyless mode has no account to bind to).
pub fn resolve_keyed_proxy(
    template: Option<&str>,
    provider: &str,
    key: Option<&str>,
) -> Option<String> {
    let template = template.map(str::trim).filter(|t| !t.is_empty())?;
    if !has_account_placeholder(template) {
        return Some(template.to_string());
    }
    let key = key.map(str::trim).filter(|k| !k.is_empty())?;
    resolve_proxy_template(template, &account_alias(provider, key))
}

/// Split a comma-separated key list the same way `KeyRing::parse` does: trim
/// each segment, drop empties. A keyring of `"k1, k2"` therefore produces two
/// proxy accounts, one per key.
fn split_keys(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .collect()
}

/// `scheme://host:port` of a proxy URL with userinfo and path stripped, so the
/// startup summary can name the proxy without ever printing credentials.
fn redacted_proxy_endpoint(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?;
    Some(match parsed.port_or_known_default() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    })
}

/// Build a tuned `reqwest::Client` with an optional outbound proxy. The same
/// client is shared across providers so TLS sessions and keep-alive connections
/// can be reused between providers that hit different hosts. Falls back to a
/// bare `Client::new()` if the builder errors (preserves prior behavior for
/// tests that construct providers without env-driven config).
pub fn build_client_with_proxy(timeout: Duration, proxy: Option<&str>) -> Client {
    let mut builder = Client::builder()
        .timeout(timeout)
        .gzip(true)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .tcp_nodelay(true);
    if let Some(url) = proxy {
        if let Some(p) = proxy_from_url(url) {
            builder = builder.proxy(p);
        }
    }
    builder.build().unwrap_or_else(|_| Client::new())
}

/// Build a tuned `reqwest::Client` with no proxy. See [`build_client_with_proxy`].
pub fn build_client(timeout: Duration) -> Client {
    build_client_with_proxy(timeout, None)
}

/// The outbound HTTP clients the search pipeline needs, differentiated by which
/// upstream they serve so each category can route through its own proxy (or
/// none):
///
/// * `keyed`   — the shared keyed-provider client (Tavily / Exa / TinyFish /
///   Firecrawl). Carries a fixed `NOVA_PROXY_KEY` when that value is
///   a plain URL; when it names `{account}`, per-key clients
///   live in the override map instead.
/// * `keyless` — DuckDuckGo / Bing + specialist extractors + generic fetch
///   (`NOVA_PROXY_KEYLESS`)
/// * `grok`    — the Grok engine's own proxy (`NOVA_PROXY_GROK`); unset means
///   direct, and it may name `{account}` like the keyed template
#[derive(Clone)]
pub struct HttpClients {
    pub keyed: Client,
    pub keyless: Client,
    pub grok: Client,
    /// Per-key `{account}`-resolved clients, keyed by (provider, exact key).
    /// One entry per key of a provider's comma-separated key list, so a
    /// rotating provider (Tavily's `KeyRing`) can egress through the proxy
    /// account derived from whichever key signed the request. Only populated
    /// when `NOVA_PROXY_KEY` names the placeholder; keyed providers with an
    /// empty key list (keyless mode) fall back to `keyed` (direct).
    keyed_overrides: std::collections::HashMap<(&'static str, String), Client>,
}

impl HttpClients {
    /// Keep both transports on the same provider/proxy registry.
    pub(crate) fn from_config(config: &crate::config::Config, restricted: bool) -> Self {
        let keyed_keys = [
            ("tavily", config.tavily_api_key.as_deref()),
            ("exa", config.exa_api_key.as_deref()),
            ("tinyfish", config.tinyfish_api_key.as_deref()),
            ("firecrawl", config.firecrawl_api_key.as_deref()),
            ("serper", config.serper_api_key.as_deref()),
        ];
        Self::build_inner(
            config.timeout,
            config.proxy_key.as_deref(),
            config.proxy_keyless.as_deref(),
            config.proxy_grok.as_deref(),
            &keyed_keys,
            match config.transport {
                crate::config::Transport::Responses => config.grok_api_key.as_deref(),
                crate::config::Transport::ChatCompletions => {
                    config.openai_compatible_api_key.as_deref()
                }
            },
            restricted,
        )
    }

    /// Plain (non-restricted) clients for the stdio / local path.
    pub fn build(
        timeout: Duration,
        proxy_key: Option<&str>,
        proxy_keyless: Option<&str>,
        proxy_grok: Option<&str>,
        keyed_keys: &[(&'static str, Option<&str>)],
        grok_key: Option<&str>,
    ) -> Self {
        Self::build_inner(
            timeout,
            proxy_key,
            proxy_keyless,
            proxy_grok,
            keyed_keys,
            grok_key,
            false,
        )
    }

    /// SSRF-restricted clients (redirects to non-public targets rejected) for
    /// the public HTTP transport. Only compiled with the `http` feature.
    #[cfg(feature = "http")]
    pub fn build_restricted(
        timeout: Duration,
        proxy_key: Option<&str>,
        proxy_keyless: Option<&str>,
        proxy_grok: Option<&str>,
        keyed_keys: &[(&'static str, Option<&str>)],
        grok_key: Option<&str>,
    ) -> Self {
        Self::build_inner(
            timeout,
            proxy_key,
            proxy_keyless,
            proxy_grok,
            keyed_keys,
            grok_key,
            true,
        )
    }

    /// Client for a keyed-provider request that will be signed with `key`: the
    /// per-key `{account}` client when one was built for this (provider, key),
    /// else the shared keyed client (fixed proxy or direct). Non-rotating
    /// providers pass their raw config value through, which may still be a
    /// comma list (only Tavily rotates) — the account then binds to the list's
    /// first key, the same alias `build_inner` derived for it.
    pub fn keyed_client_for(&self, provider: &'static str, key: Option<&str>) -> &Client {
        let key = key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .and_then(|key| {
                key.split(',')
                    .map(str::trim)
                    .find(|segment| !segment.is_empty())
            })
            .and_then(|first| self.keyed_overrides.get(&(provider, first.to_string())));
        key.unwrap_or(&self.keyed)
    }

    /// Every per-key client built for one provider (exact key → client). For
    /// providers that rotate through several keys (Tavily): the request must
    /// egress through the proxy account of whichever key signs it.
    pub fn keyed_provider_clients(&self, provider: &'static str) -> HashMap<String, Client> {
        self.keyed_overrides
            .iter()
            .filter(|((entry_provider, _), _)| *entry_provider == provider)
            .map(|((_, key), client)| (key.clone(), client.clone()))
            .collect()
    }

    fn build_inner(
        timeout: Duration,
        proxy_key: Option<&str>,
        proxy_keyless: Option<&str>,
        proxy_grok: Option<&str>,
        keyed_keys: &[(&'static str, Option<&str>)],
        grok_key: Option<&str>,
        restricted: bool,
    ) -> Self {
        // A template naming `{account}` cannot be applied verbatim to a shared
        // client: every key resolves to its own proxy account. The shared
        // `keyed` client therefore carries only a fixed (placeholder-free)
        // proxy; the placeholder case produces per-key overrides below.
        let placeholder_key = proxy_key.is_some_and(has_account_placeholder);
        let fixed_keyed: Option<String> = match proxy_key {
            Some(t) if !t.trim().is_empty() && !has_account_placeholder(t) => {
                Some(t.trim().to_string())
            }
            _ => None,
        };
        // Keyless engines have no key, so an `{account}` in the keyless
        // template could never be substituted — applying it verbatim would
        // send the literal `{account}` as the proxy username. Degrade to
        // direct with one startup note instead.
        let keyless_proxy: Option<&str> = match proxy_keyless {
            Some(t) if has_account_placeholder(t) => {
                eprintln!(
                    "nova-veil-search: keyless proxy names {{account}} but keyless engines have no key to bind an account to; connecting directly"
                );
                None
            }
            other => other,
        };
        let grok_proxy = resolve_keyed_proxy(proxy_grok, "grok", grok_key);

        let mut keyed_overrides = std::collections::HashMap::new();
        if placeholder_key {
            for &(provider, keys) in keyed_keys {
                for key in split_keys(keys) {
                    let resolved = resolve_proxy_template(
                        proxy_key.unwrap_or_default(),
                        &account_alias(provider, &key),
                    );
                    if let Some(resolved) = resolved {
                        keyed_overrides.insert(
                            (provider, key),
                            build_one(timeout, Some(&resolved), restricted),
                        );
                    }
                }
            }
        }
        proxy_startup_summary(
            proxy_key,
            placeholder_key,
            keyed_keys,
            &grok_proxy,
            proxy_grok,
            grok_key,
            keyless_proxy,
        );

        Self {
            keyed: build_one(timeout, fixed_keyed.as_deref(), restricted),
            keyless: build_one(timeout, keyless_proxy, restricted),
            grok: build_one(timeout, grok_proxy.as_deref(), restricted),
            keyed_overrides,
        }
    }
}

/// One stderr line per configured proxy category, so `docker logs` states the
/// actual routing (endpoint without credentials, per-key account aliases —
/// non-reversible 8-hex digests, safe to print) exactly once at startup.
/// Successes and per-request activity stay silent; only failures log later.
fn proxy_startup_summary(
    proxy_key: Option<&str>,
    placeholder_key: bool,
    keyed_keys: &[(&'static str, Option<&str>)],
    grok_proxy: &Option<String>,
    proxy_grok: Option<&str>,
    grok_key: Option<&str>,
    keyless_proxy: Option<&str>,
) {
    if let Some(template) = proxy_key.map(str::trim).filter(|t| !t.is_empty()) {
        // A placeholder template parses by construction; a fixed template that
        // does not parse is reported by `proxy_from_url` at client-build time.
        if let Some(endpoint) = redacted_proxy_endpoint(template) {
            if placeholder_key {
                let mut accounts = Vec::new();
                for &(provider, keys) in keyed_keys {
                    let keys = split_keys(keys);
                    if keys.is_empty() {
                        continue;
                    }
                    let alias = account_alias(provider, &keys[0]);
                    let account = if keys.len() > 1 {
                        format!("{provider}={alias}+{}", keys.len() - 1)
                    } else {
                        format!("{provider}={alias}")
                    };
                    accounts.push(account);
                }
                if accounts.is_empty() {
                    eprintln!(
                        "nova-veil-search: keyed proxy {endpoint} names {{account}} but no provider key is configured; keyed providers connect directly"
                    );
                } else {
                    eprintln!(
                        "nova-veil-search: keyed proxy {endpoint}, per-key accounts: {}",
                        accounts.join(" ")
                    );
                }
            } else {
                eprintln!("nova-veil-search: keyed providers route through proxy {endpoint}");
            }
        }
    }
    if let Some(template) = keyless_proxy.map(str::trim).filter(|t| !t.is_empty()) {
        // An unparsable URL is already reported when the client builds it;
        // the summary only states healthy routing.
        if let Some(endpoint) = redacted_proxy_endpoint(template) {
            eprintln!("nova-veil-search: keyless engines route through proxy {endpoint}");
        }
    }
    if let Some(template) = proxy_grok.map(str::trim).filter(|t| !t.is_empty()) {
        let placeholder_grok = has_account_placeholder(template);
        match grok_proxy {
            Some(_) => {
                if let Some(endpoint) = redacted_proxy_endpoint(template) {
                    if placeholder_grok {
                        let alias = grok_key
                            .map(str::trim)
                            .filter(|key| !key.is_empty())
                            .map(|key| account_alias("grok", key))
                            .unwrap_or_default();
                        eprintln!("nova-veil-search: grok proxy {endpoint}, account {alias}");
                    } else {
                        eprintln!("nova-veil-search: grok routes through proxy {endpoint}");
                    }
                }
            }
            None if placeholder_grok => {
                eprintln!(
                    "nova-veil-search: grok proxy template names {{account}} but no Grok key is configured; Grok connects directly"
                );
            }
            // None without a placeholder: an unparsable URL, already reported
            // when the grok client was built.
            None => {}
        }
    }
}

fn build_one(timeout: Duration, proxy: Option<&str>, restricted: bool) -> Client {
    #[cfg(feature = "http")]
    if restricted {
        return build_restricted_client_with_proxy(timeout, proxy);
    }
    let _ = restricted;
    build_client_with_proxy(timeout, proxy)
}

/// True if `ip` is a globally-routable public address. Rejects loopback,
/// private, link-local (incl. cloud metadata 169.254.169.254), CGNAT,
/// unspecified, multicast, broadcast, and documentation ranges — for both IPv4
/// and IPv6 (including IPv4-mapped IPv6). Used by the public HTTP transport's
/// SSRF guard; gated to that build so it is never dead code in the stdio build.
#[cfg(feature = "http")]
pub(crate) fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // CGNAT 100.64.0.0/10
                || (o[0] == 100 && (o[1] & 0xc0) == 64))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(&IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // unique local fc00::/7
                || (seg0 & 0xfe00) == 0xfc00
                // link-local fe80::/10
                || (seg0 & 0xffc0) == 0xfe80)
        }
    }
}

/// Shared builder for the restricted (SSRF-aware) HTTP-transport client: the
/// tuning knobs plus a redirect policy that rejects redirects to non-public
/// targets. Callers finish with `.build()`, optionally after pinning DNS.
#[cfg(feature = "http")]
fn restricted_client_builder(timeout: Duration) -> reqwest::ClientBuilder {
    use reqwest::redirect::Policy;
    Client::builder()
        .timeout(timeout)
        .gzip(true)
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .tcp_nodelay(true)
        .redirect(Policy::custom(|attempt| {
            use std::net::ToSocketAddrs;
            if attempt.previous().len() > 5 {
                return attempt.error("too many redirects");
            }
            let Some(host) = attempt.url().host_str() else {
                return attempt.follow();
            };
            // Reject redirects that target — or resolve to — a non-public
            // address. Covers IP literals AND hostnames, closing redirect-based
            // SSRF / DNS-rebinding (a public URL that 30x's to an internal name).
            let ips: Vec<std::net::IpAddr> = match host.parse::<std::net::IpAddr>() {
                Ok(ip) => vec![ip],
                Err(_) => {
                    let port = attempt.url().port_or_known_default().unwrap_or(443);
                    match (host, port).to_socket_addrs() {
                        Ok(addrs) => addrs.map(|addr| addr.ip()).collect(),
                        Err(_) => return attempt.error("redirect host did not resolve"),
                    }
                }
            };
            if ips.iter().any(|ip| !is_public_ip(ip)) {
                return attempt.error("redirect to a non-public address is blocked");
            }
            attempt.follow()
        }))
}

/// Like [`build_client`] but rejects redirects to private/loopback IP-literal
/// targets (defense-in-depth against redirect-based SSRF) and caps redirect
/// depth. Used only by the public HTTP transport.
#[cfg(feature = "http")]
pub fn build_restricted_client(timeout: Duration) -> Client {
    build_restricted_client_with_proxy(timeout, None)
}

/// [`build_restricted_client`] with an optional outbound proxy.
#[cfg(feature = "http")]
pub fn build_restricted_client_with_proxy(timeout: Duration, proxy: Option<&str>) -> Client {
    let mut builder = restricted_client_builder(timeout);
    if let Some(url) = proxy {
        if let Some(p) = proxy_from_url(url) {
            builder = builder.proxy(p);
        }
    }
    builder.build().unwrap_or_else(|_| Client::new())
}

/// Failure from [`post_json_with_status`]. `status` is the upstream HTTP
/// status when the request reached the server and came back non-2xx; `None`
/// for transport, timeout, body-read, and parse failures. Lets callers make
/// status-driven retry decisions (e.g. API-key rotation) without parsing
/// error strings.
pub struct HttpFailure {
    pub status: Option<u16>,
    pub error: NovaVeilSearchError,
}

impl HttpFailure {
    fn transport(error: NovaVeilSearchError) -> Self {
        Self {
            status: None,
            error,
        }
    }
}

/// Three retries after the initial attempt. HTTP status and response-format
/// errors are not network failures and do not consume this retry budget.
const NETWORK_RETRIES: usize = 3;

enum AttemptFailure {
    Network(reqwest::Error, &'static str),
    Response(HttpFailure),
}

impl From<NovaVeilSearchError> for AttemptFailure {
    fn from(error: NovaVeilSearchError) -> Self {
        Self::Response(HttpFailure::transport(error))
    }
}

/// Include the underlying DNS/TLS/SOCKS/IO cause, which reqwest's Display alone
/// omits. Remove URLs so credentials and search query strings cannot enter logs.
fn network_error_detail(error: reqwest::Error) -> String {
    use std::error::Error;
    let error = error.without_url();
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    crate::logging::redact_urls(&detail)
}

/// Retry the complete exchange, including response-body/SSE reads. A successful
/// retry emits no log; exhausted network errors and other terminal failures emit
/// one stderr line (visible in docker logs). All callers use replayable bodies.
async fn send_with_retry<T, F, Fut>(
    mut request: reqwest::RequestBuilder,
    label: &str,
    read_response: F,
) -> std::result::Result<T, HttpFailure>
where
    F: Fn(Response) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, AttemptFailure>>,
{
    let target = request
        .try_clone()
        .and_then(|r| r.build().ok())
        .map(|r| r.url().origin().ascii_serialization())
        .unwrap_or_else(|| "invalid URL".to_string());
    let mut attempt = 1;
    loop {
        let replay = request.try_clone();
        let failure = match request.send().await {
            Ok(response) => match read_response(response).await {
                Ok(value) => return Ok(value),
                Err(failure) => failure,
            },
            Err(error) => AttemptFailure::Network(error, "request"),
        };
        let (failure, detail) = match failure {
            AttemptFailure::Network(error, stage) => {
                let retryable = !error.is_builder() && !error.is_redirect() && !error.is_status();
                if retryable && attempt <= NETWORK_RETRIES {
                    if let Some(next) = replay {
                        request = next;
                        tokio::time::sleep(Duration::from_millis(250 << (attempt - 1))).await;
                        attempt += 1;
                        continue;
                    }
                }
                let timed_out = error.is_timeout();
                let detail = format!("{stage} failed: {}", network_error_detail(error));
                let message = format!("{label} {detail} (after {attempt} attempts)");
                let error = if timed_out {
                    NovaVeilSearchError::Timeout(message)
                } else {
                    NovaVeilSearchError::Provider(message)
                };
                (HttpFailure::transport(error), detail)
            }
            AttemptFailure::Response(failure) => {
                // Response bodies can echo tokens or request data. Keep those
                // out of process logs; callers still receive the upstream error.
                let detail = match failure.status {
                    Some(status) => format!("upstream returned HTTP {status}"),
                    None => match &failure.error {
                        NovaVeilSearchError::Parse(_) => "invalid upstream response".to_string(),
                        _ => "upstream response rejected".to_string(),
                    },
                };
                (failure, detail)
            }
        };
        eprintln!(
            "nova-veil-search: ERROR provider={label:?} target={target:?} attempts={attempt} retries={} error={detail:?}",
            attempt - 1
        );
        return Err(failure);
    }
}

async fn read_response_bytes(
    response: Response,
    label: &str,
) -> std::result::Result<Vec<u8>, AttemptFailure> {
    let status = response.status();
    if !status.is_success() {
        let bytes = response.bytes().await.unwrap_or_default();
        let text = truncate_for_error(&String::from_utf8_lossy(&bytes));
        return Err(AttemptFailure::Response(HttpFailure {
            status: Some(status.as_u16()),
            error: NovaVeilSearchError::Provider(format!("{label} returned HTTP {status}: {text}")),
        }));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| AttemptFailure::Network(error, "body read"))
}

/// Shared by specialist fetchers and raw/SSE text endpoints as well as JSON
/// providers, so all provider traffic has the same network retry policy.
pub(crate) async fn send_bytes(request: reqwest::RequestBuilder, label: &str) -> Result<Vec<u8>> {
    send_with_retry(request, label, |response| {
        read_response_bytes(response, label)
    })
    .await
    .map_err(|failure| failure.error)
}

/// Issue an authenticated JSON POST and normalize transport / status / parse
/// errors into `NovaVeilSearchError`. `label` appears in error messages to
/// distinguish upstream providers (e.g. "Tavily", "Firecrawl", "Grok Responses").
pub async fn post_json(
    client: &Client,
    endpoint: &str,
    api_key: &str,
    body: &Value,
    label: &str,
) -> Result<Value> {
    post_json_with_status(client, endpoint, api_key, body, label)
        .await
        .map_err(|failure| failure.error)
}

/// Status-aware variant of [`post_json`]: identical behavior, but non-2xx
/// responses carry their HTTP status alongside the normalized error.
pub async fn post_json_with_status(
    client: &Client,
    endpoint: &str,
    api_key: &str,
    body: &Value,
    label: &str,
) -> std::result::Result<Value, HttpFailure> {
    send_json(client.post(endpoint).bearer_auth(api_key).json(body), label).await
}

/// Like [`post_json`], but authenticated with a provider-specific header
/// (e.g. TinyFish's `X-API-Key`) instead of a bearer `Authorization`.
pub async fn post_json_with_header_auth(
    client: &Client,
    endpoint: &str,
    header: (&str, &str),
    body: &Value,
    label: &str,
) -> Result<Value> {
    send_json(
        client.post(endpoint).header(header.0, header.1).json(body),
        label,
    )
    .await
    .map_err(|failure| failure.error)
}

/// Authenticated JSON GET with query parameters and a provider-specific auth
/// header, for GET-style search APIs (e.g. TinyFish). Same error
/// normalization as [`post_json`].
pub async fn get_json_with_header_auth(
    client: &Client,
    endpoint: &str,
    query: &[(&str, String)],
    header: (&str, &str),
    label: &str,
) -> Result<Value> {
    send_json(
        client.get(endpoint).query(query).header(header.0, header.1),
        label,
    )
    .await
    .map_err(|failure| failure.error)
}

/// Browser-like `User-Agent` used by keyless HTML-scraping providers
/// (DuckDuckGo, Bing). Free search engines reject or challenge default
/// CLI-style user agents.
pub const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Issue an authenticated JSON POST whose bearer key is optional: an empty or
/// absent key sends no `Authorization` header at all (keyless anonymous mode).
pub async fn post_json_optional_auth(
    client: &Client,
    endpoint: &str,
    api_key: Option<&str>,
    body: &Value,
    label: &str,
) -> Result<Value> {
    let request = client.post(endpoint).json(body);
    let request = match api_key.filter(|key| !key.is_empty()) {
        Some(key) => request.bearer_auth(key),
        None => request,
    };
    send_json(request, label)
        .await
        .map_err(|failure| failure.error)
}

/// Status-aware variant of [`post_json_with_header_auth`]: identical request,
/// but non-2xx responses carry their HTTP status alongside the normalized
/// error so multi-key providers can make key-rotation decisions.
pub async fn post_json_with_header_auth_status(
    client: &Client,
    endpoint: &str,
    header: (&str, &str),
    body: &Value,
    label: &str,
) -> std::result::Result<Value, HttpFailure> {
    send_json(
        client.post(endpoint).header(header.0, header.1).json(body),
        label,
    )
    .await
}

/// Status-aware variant of [`get_json_with_header_auth`], for the GET-style
/// search APIs. Same error shape as [`post_json_with_header_auth_status`].
pub async fn get_json_with_header_auth_status(
    client: &Client,
    endpoint: &str,
    query: &[(&str, String)],
    header: (&str, &str),
    label: &str,
) -> std::result::Result<Value, HttpFailure> {
    send_json(
        client.get(endpoint).query(query).header(header.0, header.1),
        label,
    )
    .await
}

/// Run one attempt per key of a [`KeyRing`], starting at the ring's shared
/// round-robin cursor and rotating to the next key on key-scoped failures
/// (401/403/429/432/433 — the statuses that indict the key, not the request).
/// Network retries happen inside the shared HTTP helper per attempt; the
/// closure receives the exact key to sign with and must route through its
/// matching per-key `{account}` proxy client when one exists.
pub(crate) async fn rotate_keys<T, F, Fut>(
    keys: &KeyRing,
    provider: &str,
    mut attempt: F,
) -> std::result::Result<T, HttpFailure>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, HttpFailure>>,
{
    let attempts = keys.len();
    let start = keys.start();
    let mut last_error = None;
    for offset in 0..attempts {
        let key = keys.key(start + offset).to_string();
        match attempt(key).await {
            Ok(value) => return Ok(value),
            Err(failure) => {
                let key_scoped = failure.status.is_some_and(is_key_scoped_status);
                if key_scoped && offset + 1 < attempts {
                    eprintln!(
                        "nova-veil-search: {provider} key {}/{} hit HTTP {}; rotating to next key",
                        (start + offset) % attempts + 1,
                        attempts,
                        failure.status.unwrap_or_default(),
                    );
                    last_error = Some(failure.error);
                    continue;
                }
                return Err(failure);
            }
        }
    }
    // Unreachable: the loop always returns on the final attempt. Kept as a
    // defensive fallback instead of unwrap/panic.
    Err(HttpFailure::transport(last_error.unwrap_or_else(|| {
        NovaVeilSearchError::Provider(format!("{provider} request failed with no attempts"))
    })))
}

/// Fetch a plain HTML page with browser-like headers and normalize transport /
/// status failures. `query` becomes the URL query string. Used by the keyless
/// DuckDuckGo and Bing scrapers, which return ordinary `text/html`.
pub async fn get_html(
    client: &Client,
    endpoint: &str,
    query: &[(&str, String)],
    label: &str,
) -> Result<String> {
    let request = client
        .get(endpoint)
        .query(query)
        .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
        .header(
            reqwest::header::ACCEPT_LANGUAGE,
            "en-US,en;q=0.9,zh-CN;q=0.8",
        );
    send_with_retry(request, label, |response| async move {
        let bytes = read_response_bytes(response, label).await?;
        let html = String::from_utf8_lossy(&bytes).into_owned();
        if is_anti_bot_challenge(&html) {
            return Err(NovaVeilSearchError::Provider(format!(
                "{label} is rate-limited (anti-bot challenge; usually temporary)"
            ))
            .into());
        }
        Ok(html)
    })
    .await
    .map_err(|failure| failure.error)
}

/// POST a JSON-RPC body with arbitrary headers (no auth) and return the raw
/// response body as text. Used for the Exa MCP endpoint, which answers with an
/// SSE stream that the caller parses itself.
pub async fn post_raw_json(
    client: &Client,
    endpoint: &str,
    headers: &[(&str, &str)],
    body: &Value,
    label: &str,
) -> Result<String> {
    let mut request = client.post(endpoint);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let bytes = send_bytes(request.json(body), label).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Cap an error-body excerpt so a hostile/inflated upstream can't bloat notes.
fn truncate_for_error(text: &str) -> String {
    let excerpt: String = text.chars().take(200).collect();
    if text.chars().count() > 200 {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

/// Heuristic detector for search-engine anti-bot interstitial pages (CAPTCHA /
/// anomaly checks) that still return HTTP 200.
pub(crate) fn is_anti_bot_challenge(html: &str) -> bool {
    let head = html.get(..8_000).unwrap_or(html).to_ascii_lowercase();
    [
        "anomaly detection",
        "captcha",
        "unusual traffic",
        "robot check",
    ]
    .iter()
    .any(|marker| head.contains(marker))
}

/// Strip every `<...>` tag from an HTML fragment. Non-nesting-safe on purpose:
/// search-result fields never contain `<` except as tag delimiters.
pub(crate) fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        match rest[open..].find('>') {
            Some(close) => rest = &rest[open + close + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode the HTML entities search engines actually emit (`&amp;`, `&lt;`,
/// `&gt;`, `&quot;`, `&apos;`, `&nbsp;`, and numeric `&#…;` / `&#x…;`).
/// Unknown named entities are left untouched.
pub(crate) fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        match after.find(';') {
            Some(end) => {
                let entity = &after[1..end];
                let decoded = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => decode_numeric_entity(entity),
                };
                match decoded {
                    Some(ch) => out.push(ch),
                    None => out.push_str(&after[..=end]), // unknown: keep verbatim
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let stripped = entity.strip_prefix('#')?;
    let code = if let Some(hex) = stripped
        .strip_prefix('x')
        .or_else(|| stripped.strip_prefix('X'))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        stripped.parse::<u32>().ok()?
    };
    char::from_u32(code)
}

/// Collapse runs of whitespace into single spaces and trim the ends. Applied to
/// every scraped title/snippet so inline markup and pretty-printed HTML don't
/// leak extra blanks into results.
pub(crate) fn squash_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Send a prepared JSON request and normalize transport / status / parse
/// failures into `HttpFailure`. Shared by the bearer- and header-auth helpers
/// so every provider gets identical error handling (including the SSE path).
pub(crate) async fn send_json(
    request: reqwest::RequestBuilder,
    label: &str,
) -> std::result::Result<Value, HttpFailure> {
    send_with_retry(request, label, |response| {
        read_json_response(response, label)
    })
    .await
}

async fn read_json_response(
    mut response: Response,
    label: &str,
) -> std::result::Result<Value, AttemptFailure> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if response.status().is_success() && content_type.starts_with("text/event-stream") {
        return read_sse_json(&mut response, label).await;
    }

    let bytes = read_response_bytes(response, label).await?;

    serde_json::from_slice(&bytes)
        .map_err(|err| NovaVeilSearchError::Parse(format!("invalid {label} JSON: {err}")).into())
}

async fn read_sse_json(
    response: &mut Response,
    label: &str,
) -> std::result::Result<Value, AttemptFailure> {
    let mut buffer = Vec::new();
    let mut output_text = String::new();
    let mut chat_content = String::new();
    let mut last_json = None;
    let mut chat_metadata = None;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| AttemptFailure::Network(err, "stream read"))?
    {
        buffer.extend_from_slice(&chunk);

        let mut consumed = 0;
        while let Some((event, rest)) = split_sse_event(&buffer[consumed..]) {
            consumed = buffer.len() - rest.len();
            if let Some(value) = process_sse_event(
                event,
                label,
                &mut last_json,
                &mut chat_metadata,
                &mut output_text,
                &mut chat_content,
            )? {
                return Ok(value);
            }
        }
        // Compact once per network chunk, rather than copying the remaining
        // buffer after every event in a chunk containing many deltas.
        if consumed > 0 {
            drop(buffer.drain(..consumed));
        }
    }

    if !buffer.is_empty() {
        let event = std::mem::take(&mut buffer);
        if let Some(value) = process_sse_event(
            &event,
            label,
            &mut last_json,
            &mut chat_metadata,
            &mut output_text,
            &mut chat_content,
        )? {
            return Ok(value);
        }
    }

    finish_sse_json(label, last_json, chat_metadata, output_text, chat_content)
        .map_err(AttemptFailure::from)
}

struct SseEvent {
    name: Option<String>,
    data: Option<String>,
}

fn split_sse_event(buffer: &[u8]) -> Option<(&[u8], &[u8])> {
    let delimiter = [
        b"\n\n".as_slice(),
        b"\r\n\r\n".as_slice(),
        b"\r\r".as_slice(),
    ]
    .into_iter()
    .filter_map(|delimiter| find_bytes(buffer, delimiter).map(|index| (index, delimiter.len())))
    .min_by_key(|(index, _)| *index)?;
    Some((&buffer[..delimiter.0], &buffer[delimiter.0 + delimiter.1..]))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_sse_event(event: &[u8], label: &str) -> Result<SseEvent> {
    let event = std::str::from_utf8(event)
        .map_err(|err| NovaVeilSearchError::Parse(format!("invalid {label} SSE UTF-8: {err}")))?;
    let event = event.strip_prefix('\u{feff}').unwrap_or(event);
    let mut name = None;
    let mut lines = Vec::new();
    for line in event.split(['\n', '\r']) {
        if let Some(event_name) = line.strip_prefix("event:") {
            name = Some(event_name.trim_start().to_string());
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            lines.push(data.trim_start());
        }
    }

    Ok(SseEvent {
        name,
        data: if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        },
    })
}

fn is_completion_event_name(name: &str) -> bool {
    matches!(
        name,
        "done" | "end" | "complete" | "completed" | "response.completed"
    )
}

fn process_sse_event(
    event: &[u8],
    label: &str,
    last_json: &mut Option<Value>,
    chat_metadata: &mut Option<Value>,
    output_text: &mut String,
    chat_content: &mut String,
) -> Result<Option<Value>> {
    let event = parse_sse_event(event, label)?;
    let named_completion = event.name.as_deref().is_some_and(is_completion_event_name);
    let data = event.data.as_deref().map(str::trim);

    if named_completion && data.map(str::is_empty).unwrap_or(true) {
        return finish_sse_state(label, last_json, chat_metadata, output_text, chat_content)
            .map(Some);
    }

    let Some(data) = data else {
        return Ok(None);
    };
    if data.is_empty() {
        return Ok(None);
    }
    if data == "[DONE]" {
        return finish_sse_state(label, last_json, chat_metadata, output_text, chat_content)
            .map(Some);
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|err| NovaVeilSearchError::Parse(format!("invalid {label} SSE JSON: {err}")))?;
    collect_stream_delta(&value, output_text, chat_content);
    accumulate_chat_metadata(chat_metadata, &value);

    if let Some(kind) = response_terminal_error_type(&value) {
        return Err(NovaVeilSearchError::Provider(format!(
            "{label} stream ended with {kind}: {}",
            response_terminal_error_detail(&value)
        )));
    }

    if value.get("type").and_then(Value::as_str) == Some("response.completed") {
        if let Some(response) = value.get("response") {
            return Ok(Some(response.clone()));
        }
        if !output_text.is_empty() {
            return Ok(Some(
                serde_json::json!({ "output_text": output_text.clone() }),
            ));
        }
        return Ok(Some(value));
    }

    if named_completion {
        *last_json = Some(value);
        return finish_sse_state(label, last_json, chat_metadata, output_text, chat_content)
            .map(Some);
    }

    *last_json = Some(value);
    Ok(None)
}

fn finish_sse_state(
    label: &str,
    last_json: &mut Option<Value>,
    chat_metadata: &mut Option<Value>,
    output_text: &mut String,
    chat_content: &mut String,
) -> Result<Value> {
    finish_sse_json(
        label,
        last_json.take(),
        chat_metadata.take(),
        std::mem::take(output_text),
        std::mem::take(chat_content),
    )
}

fn response_terminal_error_type(value: &Value) -> Option<&str> {
    match value.get("type").and_then(Value::as_str) {
        Some("response.failed" | "response.incomplete") => {
            value.get("type").and_then(Value::as_str)
        }
        _ => None,
    }
}

fn response_terminal_error_detail(value: &Value) -> String {
    value
        .pointer("/error/message")
        .or_else(|| value.pointer("/response/error/message"))
        .or_else(|| value.get("error"))
        .map(|detail| match detail {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| value.to_string())
}

fn accumulate_chat_metadata(acc: &mut Option<Value>, value: &Value) {
    if value.get("choices").is_none()
        && value.get("citations").is_none()
        && value.get("search_sources").is_none()
    {
        return;
    }

    if acc.is_none() {
        *acc = Some(serde_json::json!({}));
    }
    let Some(raw) = acc.as_mut().and_then(Value::as_object_mut) else {
        return;
    };

    for key in ["citations", "search_sources"] {
        if let Some(items) = value.get(key) {
            append_json_array(raw, key, items);
        }
    }

    for pointer in [
        "/choices/0/message/annotations",
        "/choices/0/message/citations",
        "/choices/0/delta/annotations",
        "/choices/0/delta/citations",
    ] {
        let Some(items) = value.pointer(pointer) else {
            continue;
        };
        let key = pointer.rsplit('/').next().unwrap_or_default();
        let choices = raw
            .entry("choices".to_string())
            .or_insert_with(|| serde_json::json!([{ "delta": {} }]));
        if choices.as_array().map(Vec::is_empty).unwrap_or(true) {
            *choices = serde_json::json!([{ "delta": {} }]);
        }
        if let Some(delta) = choices
            .pointer_mut("/0/delta")
            .and_then(Value::as_object_mut)
        {
            append_json_array(delta, key, items);
        }
    }
}

fn append_json_array(map: &mut serde_json::Map<String, Value>, key: &str, value: &Value) {
    let entry = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    let Some(out) = entry.as_array_mut() else {
        return;
    };
    match value {
        Value::Array(items) => out.extend(items.iter().cloned()),
        other => out.push(other.clone()),
    }
}

fn synthesize_chat_json(
    last_json: Option<Value>,
    chat_metadata: Option<Value>,
    chat_content: String,
) -> Value {
    let mut raw = chat_metadata
        .or(last_json)
        .unwrap_or_else(|| serde_json::json!({}));
    if !raw.is_object() {
        raw = serde_json::json!({});
    }

    let message = raw
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let delta = raw
        .pointer("/choices/0/delta")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let mut message = match message {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if let Value::Object(delta) = delta {
        for key in ["annotations", "citations"] {
            if !message.contains_key(key) {
                if let Some(value) = delta.get(key) {
                    message.insert(key.to_string(), value.clone());
                }
            }
        }
    }
    message.insert("content".to_string(), Value::String(chat_content));

    let choice = serde_json::json!({ "message": Value::Object(message) });
    if let Some(object) = raw.as_object_mut() {
        object.insert("choices".to_string(), Value::Array(vec![choice]));
        raw
    } else {
        serde_json::json!({ "choices": [choice] })
    }
}

fn collect_stream_delta(value: &Value, output_text: &mut String, chat_content: &mut String) {
    if value.get("type").and_then(Value::as_str) == Some("response.output_text.delta") {
        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
            output_text.push_str(delta);
        }
    }

    if let Some(content) = value.pointer("/choices/0/delta/content") {
        match content {
            Value::String(text) => chat_content.push_str(text),
            Value::Array(parts) => {
                for part in parts {
                    if let Some(text) = part
                        .get("text")
                        .or_else(|| part.get("content"))
                        .and_then(Value::as_str)
                    {
                        chat_content.push_str(text);
                    }
                }
            }
            _ => {}
        }
    }
}

fn finish_sse_json(
    label: &str,
    last_json: Option<Value>,
    chat_metadata: Option<Value>,
    output_text: String,
    chat_content: String,
) -> Result<Value> {
    if !output_text.is_empty() {
        return Ok(serde_json::json!({ "output_text": output_text }));
    }
    if !chat_content.is_empty() {
        return Ok(synthesize_chat_json(last_json, chat_metadata, chat_content));
    }
    if chat_metadata.is_some() {
        return Ok(synthesize_chat_json(last_json, chat_metadata, chat_content));
    }
    last_json.ok_or_else(|| {
        NovaVeilSearchError::Parse(format!("{label} SSE stream ended without JSON data"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_alias_is_stable_8_hex_and_key_distinct() {
        let a = account_alias("tavily", "sk-A");
        assert_eq!(a.len(), 8);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));

        // Stable for the same inputs…
        assert_eq!(a, account_alias("tavily", "sk-A"));
        // …but distinct per key and per provider.
        assert_ne!(a, account_alias("tavily", "sk-B"));
        assert_ne!(a, account_alias("exa", "sk-A"));
        // Non-reversible: the raw key never appears.
        assert!(!a.contains("sk-A"));
    }

    #[test]
    fn resolve_proxy_template_username_only_password_untouched() {
        // NovaVeil's contract: only the username's `{account}` is substituted;
        // the password and the rest of the URL are preserved verbatim.
        let resolved =
            resolve_proxy_template("socks5h://Default.{account}:123@resin:2260", "a1b2c3d4");
        assert_eq!(
            resolved.as_deref(),
            Some("socks5h://Default.a1b2c3d4:123@resin:2260")
        );
    }

    #[test]
    fn resolve_proxy_template_no_placeholder_passthrough() {
        let url = "socks5h://Default.fixed:123@resin:2260";
        assert_eq!(resolve_proxy_template(url, "unused").as_deref(), Some(url));
        // Without userinfo there is nothing to substitute.
        assert_eq!(
            resolve_proxy_template("socks5h://resin:2260", "unused").as_deref(),
            Some("socks5h://resin:2260")
        );
    }

    #[test]
    fn encoded_placeholder_is_resolved_only_in_the_username() {
        for placeholder in ["{account}", "%7Baccount%7D", "%7baccount%7d"] {
            let template = format!("socks5h://Default.{placeholder}:p%40ss@proxy:1080");
            let alias = account_alias("tavily", "test-key");
            assert_eq!(
                resolve_keyed_proxy(Some(&template), "tavily", Some("test-key")),
                Some(format!("socks5h://Default.{alias}:p%40ss@proxy:1080"))
            );
        }
        let fixed = "socks5://fixed:%7Baccount%7D@proxy:1080";
        assert_eq!(
            resolve_keyed_proxy(Some(fixed), "tavily", None).as_deref(),
            Some(fixed)
        );
    }

    #[test]
    fn template_preserves_literal_percent_and_escapes_alias() {
        let resolved = resolve_proxy_template(
            "socks5h://u%2540.{account}:p%2540%40ss@proxy:1080",
            "a@b:c/d%40",
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&resolved).unwrap();
        assert_eq!(
            percent_encoding::percent_decode_str(parsed.username()).decode_utf8_lossy(),
            "u%40.a@b:c/d%40"
        );
        assert_eq!(parsed.password(), Some("p%2540%40ss"));
        assert_eq!(parsed.host_str(), Some("proxy"));
        assert_eq!(parsed.port(), Some(1080));
    }

    #[test]
    fn resolve_proxy_template_invalid_or_empty_is_none() {
        assert_eq!(resolve_proxy_template("", "x"), None);
        assert_eq!(resolve_proxy_template("  ", "x"), None);
        assert_eq!(resolve_proxy_template("not a url", "x"), None);
    }

    #[test]
    fn resolve_keyed_proxy_account_fixed_and_direct() {
        // No template → direct.
        assert_eq!(resolve_keyed_proxy(None, "tavily", Some("k")), None);

        // Fixed (placeholder-free, trimmed) URL → shared proxy, key ignored.
        assert_eq!(
            resolve_keyed_proxy(Some(" socks5h://u:p@h:1080 "), "tavily", None),
            Some("socks5h://u:p@h:1080".to_string())
        );

        // Placeholder + key → per-key alias in the username.
        let template = "socks5h://Default.{account}:123@resin:2260";
        let expected_alias = account_alias("tavily", "KEY");
        assert_eq!(
            resolve_keyed_proxy(Some(template), "tavily", Some("KEY")),
            Some(format!("socks5h://Default.{expected_alias}:123@resin:2260"))
        );

        // Placeholder + no/empty key (keyless mode) → direct.
        assert_eq!(resolve_keyed_proxy(Some(template), "exa", None), None);
        assert_eq!(resolve_keyed_proxy(Some(template), "exa", Some("")), None);
    }

    #[test]
    fn placeholder_template_builds_one_client_per_key() {
        let template = "socks5h://Default.{account}:123@resin:2260";
        let keys = [
            ("tavily", Some("k1, k2 ,")),
            ("exa", Some("e1")),
            ("tinyfish", None),
            ("firecrawl", Some("")),
        ];
        let clients = HttpClients::build(
            Duration::from_secs(5),
            Some(template),
            None,
            None,
            &keys,
            None,
        );
        // Tavily's two keys each get their own proxy-account client; empty
        // key lists (tinyfish / firecrawl) get none and fall back to shared.
        let tavily = clients.keyed_provider_clients("tavily");
        assert_eq!(tavily.len(), 2);
        assert!(tavily.contains_key("k1") && tavily.contains_key("k2"));
        assert!(clients.keyed_provider_clients("exa").contains_key("e1"));
        assert!(clients.keyed_provider_clients("tinyfish").is_empty());
        assert!(clients.keyed_provider_clients("firecrawl").is_empty());
        // (Whether the matched or shared client is returned for a given key
        // is verified on the wire by tests/proxy_http.rs.)
    }

    #[test]
    fn fixed_keyed_template_has_no_per_key_clients() {
        let keys = [
            ("tavily", Some("k1")),
            ("exa", None),
            ("tinyfish", None),
            ("firecrawl", None),
        ];
        let clients = HttpClients::build(
            Duration::from_secs(5),
            Some("socks5h://fixed:123@resin:2260"),
            None,
            None,
            &keys,
            None,
        );
        assert!(clients.keyed_provider_clients("tavily").is_empty());
        assert!(std::ptr::eq(
            clients.keyed_client_for("tavily", Some("k1")),
            &clients.keyed
        ));
    }
}
