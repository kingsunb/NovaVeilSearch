//! Native Streamable HTTP transport (Cargo feature `http`).
//!
//! Exposes the same MCP tools as the stdio transport over a single
//! `POST /mcp` endpoint. There is exactly one credential model — **server
//! keys only** — and authentication is mandatory:
//!
//! - Provider keys always come from the server's own environment or its
//!   `config.toml` (`GROK_*`, `TAVILY_API_KEY`, `EXA_API_KEY`,
//!   `TINYFISH_API_KEY`, `FIRECRAWL_API_KEY`, …; environment wins). Callers can
//!   no longer supply their own keys via `X-*-Api-Key` request headers; those
//!   headers are ignored.
//! - Every request must authenticate with `Authorization: Bearer <token>` —
//!   either the master token `GROK_MCP_API_TOKEN` (constant-time match,
//!   fail-closed 401 otherwise) or a short-lived session token issued by
//!   `POST /login`.
//! - `POST /login` (enabled by setting `NOVA_ADMIN_PASSWORD`) verifies the
//!   admin username/password and returns a session token with a sliding TTL
//!   (`NOVA_SESSION_TTL_SECONDS`, default 12 h) held in an in-memory store.
//!
//! A fully-credentialed [`SearchService`] is built per request via
//! [`SearchService::for_request`], reusing one shared HTTP client and one
//! process-wide source cache (so `get_sources` continuation still works across
//! requests). The whole module is gated behind the `http` feature so the
//! default stdio build never links axum.
//!
//! TLS terminates upstream (Caddy); this server binds loopback only.
//!
//! A separate `POST /messages` endpoint additionally serves an
//! Anthropic-compatible Messages API (native `web_search_20250305` server tool)
//! so the official DeepSeek web-search backend can point its `baseURL` at nova
//! — the zero-plugin way to plug nova in as DSH’s search provider with nothing
//! but a URL and a key.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};
use tokio_stream::wrappers::ReceiverStream;

use crate::cache::SourceCache;
use crate::config::Config;
use crate::error::NovaVeilSearchError;
use crate::mcp::{error_response, handle_message};
use crate::model::tool::{WebSearchInput, WebSearchOutput};
use crate::service::SearchService;

/// Max JSON request body. MCP tool calls are tiny; anything larger is abuse.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Max concurrent in-flight requests; excess gets 429 to protect the 1 GB box.
const MAX_CONCURRENT_REQUESTS: usize = 32;

/// Max concurrent host resolutions (caller-gateway + fetch-tool SSRF
/// validation); excess gets 503. Bounds how many (potentially hung) blocking
/// getaddrinfo calls can run at once.
const MAX_DNS_LOOKUPS: usize = 8;

/// Protocol revisions the Streamable HTTP transport implements. Excludes
/// 2024-11-05 (the deprecated HTTP+SSE transport this endpoint does not serve)
/// and 2025-03-26 (which still mandates JSON-RPC batching — removed only in
/// 2025-06-18; since this endpoint rejects batches, it declares 2025-06-18+ only).
const HTTP_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18"];

/// Operator env var holding the master bearer token. Every request to `/mcp`
/// must authenticate with `Authorization: Bearer <this token or a login-issued
/// session token>`. Provider keys always come from the server's own
/// environment — callers never supply their own keys.
const API_TOKEN_ENV: &str = "GROK_MCP_API_TOKEN";

/// Admin username / password for the `POST /login` endpoint that issues
/// short-lived session tokens for the web UI. An unset/empty
/// `NOVA_ADMIN_PASSWORD` disables `/login` (master-token auth still applies).
const ADMIN_USER_ENV: &str = "NOVA_ADMIN_USER";
const ADMIN_PASSWORD_ENV: &str = "NOVA_ADMIN_PASSWORD";
const DEFAULT_ADMIN_USER: &str = "admin";

/// Session-token TTL override (seconds). Defaults to 12 hours.
const SESSION_TTL_ENV: &str = "NOVA_SESSION_TTL_SECONDS";
const DEFAULT_SESSION_TTL_SECONDS: u64 = 12 * 60 * 60;

/// Opt-in flag for the settings/config frontend (`GET /` + `/api/config`) and
/// the per-request `config.toml` read. OFF by default: pure backend.
const CONFIG_UI_ENV: &str = "NOVA_CONFIG_UI";

/// Max login-form body. Two short fields; anything larger is abuse.
const MAX_LOGIN_BODY_BYTES: usize = 4 * 1024;

/// Cadence (seconds) of the SSE heartbeat frames emitted while a long-running
/// tool call is still working. The first frame goes out immediately, keeping
/// the stream from ever being idle long enough for a fronting proxy to give
/// up — Cloudflare's 524 fires after ~100 s with no bytes, so 15 s leaves a
/// wide margin no matter how slow the search.
const SSE_KEEPALIVE_SECONDS: u64 = 15;

/// SSE comment frame. Comments (a line starting with `:`) are ignored by every
/// SSE client, so a heartbeat is data on the wire without being a protocol
/// event.
const SSE_HEARTBEAT: &[u8] = b": keep-alive\n\n";

#[derive(Clone)]
pub(crate) struct AppState {
    /// Shared across every request: one connection pool, operator timeout.
    http_client: reqwest::Client,
    /// One process-wide cache so `get_sources` continuation survives requests.
    cache: Arc<Mutex<SourceCache>>,
    /// Operator defaults that seed every request's config: the server's own
    /// provider keys (callers supply no keys).
    pub(crate) base_env: Arc<HashMap<String, String>>,
    /// Allowed `Origin` values; `None` means no allowlist configured (allow).
    pub(crate) allowed_origins: Arc<Option<HashSet<String>>>,
    /// Bounds concurrent in-flight requests (DoS protection on a small box).
    limiter: Arc<Semaphore>,
    /// Caps concurrent fetch-tool host resolutions so hung blocking getaddrinfo
    /// calls (which outlive their timeout) can't pile up threads.
    dns_limiter: Arc<Semaphore>,
    /// Master bearer token (`GROK_MCP_API_TOKEN`). Every request must send this
    /// or a login-issued session token.
    api_token: String,
    /// In-memory session tokens issued by `POST /login`.
    sessions: Arc<Mutex<SessionStore>>,
    /// Admin username for `/login` (not secret; compared constant-time anyway).
    admin_user: String,
    /// Admin password for `/login`. `None` disables the endpoint.
    admin_password: Option<String>,
    /// Lifetime of a `/login`-issued session token.
    session_ttl: std::time::Duration,
    /// Whether the settings/config frontend (`GET /`, `/api/config`) and the
    /// per-request `config.toml` read are enabled (`NOVA_CONFIG_UI`). Off by
    /// default: no frontend routes, no per-request disk I/O.
    config_ui: bool,
}

/// In-memory store of login-issued session tokens with sliding expiry.
struct SessionStore {
    tokens: HashMap<String, std::time::Instant>,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }

    /// Issue a fresh session token valid for `ttl` from now, pruning expired
    /// tokens opportunistically.
    fn issue(&mut self, ttl: std::time::Duration) -> String {
        self.retain_active();
        let token = uuid::Uuid::new_v4().to_string();
        self.tokens
            .insert(token.clone(), std::time::Instant::now() + ttl);
        token
    }

    /// `true` -> the token is known and unexpired (its expiry slides by `ttl`).
    /// `false` -> unknown or expired (expired entries are removed).
    fn validate(&mut self, token: &str, ttl: std::time::Duration) -> bool {
        let now = std::time::Instant::now();
        match self.tokens.get(token) {
            Some(expires) if *expires > now => {}
            Some(_) => {
                self.tokens.remove(token);
                return false;
            }
            None => return false,
        }
        self.tokens.insert(token.to_string(), now + ttl);
        true
    }

    fn retain_active(&mut self) {
        let now = std::time::Instant::now();
        self.tokens.retain(|_, expires| *expires > now);
    }
}

/// Run the HTTP transport, binding `bind` (loopback in production, behind
/// Caddy). `base_env` is the operator process environment; its entries seed
/// every request's config. Keys are always built-in (server-side) — callers
/// supply none — and every request must authenticate with a bearer token.
pub async fn run_http(base_env: HashMap<String, String>, bind: SocketAddr) -> anyhow::Result<()> {
    // Operator config (timeout, cache sizing, chain order) drives the shared
    // client + cache.
    let operator_cfg = Config::from_env_map(base_env.clone());
    // Fail before binding on a bad GROK_SEARCH_SOURCE_PROVIDERS: the chain is
    // operator-fixed, and deferring the error to per-request service
    // construction would leave a listener up that rejects every call.
    crate::service::validate_source_providers(&operator_cfg)?;
    // Restricted client: rejects redirects to non-public IP-literal targets.
    let http_client = crate::providers::http::build_restricted_client(operator_cfg.timeout);
    let cache = Arc::new(Mutex::new(SourceCache::new(operator_cfg.cache_size)));
    let allowed_origins = parse_allowed_origins(&base_env);

    // Auth is mandatory: the bring-your-own-key mode is gone, so the master
    // token must be set (fail closed rather than serving an open endpoint).
    let api_token = base_env
        .get(API_TOKEN_ENV)
        .map(|value| value.trim())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{API_TOKEN_ENV} is required: this transport only serves built-in \
                 server keys and must be protected by a bearer token"
            )
        })?;

    // Optional admin login issuing short-lived session tokens for the web UI.
    let admin_user = base_env
        .get(ADMIN_USER_ENV)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_string());
    let admin_password = base_env
        .get(ADMIN_PASSWORD_ENV)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let session_ttl = base_env
        .get(SESSION_TTL_ENV)
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(DEFAULT_SESSION_TTL_SECONDS));

    let request_base = request_base_env(&base_env);
    let login_enabled = admin_password.is_some();
    let config_ui_enabled = env_is_true(&base_env, CONFIG_UI_ENV);

    let state = AppState {
        http_client,
        cache,
        base_env: Arc::new(request_base),
        allowed_origins: Arc::new(allowed_origins),
        limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        dns_limiter: Arc::new(Semaphore::new(MAX_DNS_LOOKUPS)),
        api_token,
        sessions: Arc::new(Mutex::new(SessionStore::new())),
        admin_user,
        admin_password,
        session_ttl,
        config_ui: config_ui_enabled,
    };

    // Body sizes are capped inside the handlers (after the concurrency permit is
    // held for /mcp), so no DefaultBodyLimit layer is needed.
    let mut app = Router::new()
        .route("/mcp", post(mcp_post))
        .route("/messages", post(messages_post))
        .route("/login", post(login));
    // The settings/config frontend is opt-in (`NOVA_CONFIG_UI`): off by default
    // it registers no frontend routes and triggers no per-request config.toml
    // read — the process is a pure backend (MCP + login auth) with no overhead.
    if config_ui_enabled {
        app = app.route("/", get(crate::web::serve_index)).route(
            "/api/config",
            get(crate::web::get_config).put(crate::web::put_config),
        );
    }

    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("nova-veil-search: Streamable HTTP transport listening on http://{bind}/mcp");
    eprintln!(
        "nova-veil-search: Anthropic-compatible Messages endpoint for the DeepSeek web-search backend on http://{bind}/messages"
    );
    eprintln!(
        "nova-veil-search: server-config mode — requests must send `Authorization: Bearer <token>`; provider keys come from this server's environment"
    );
    eprintln!(
        "nova-veil-search: login endpoint {}",
        if login_enabled {
            "enabled at POST /login"
        } else {
            "disabled (set NOVA_ADMIN_PASSWORD to enable)"
        }
    );
    eprintln!(
        "nova-veil-search: settings/config frontend {}",
        if config_ui_enabled {
            "enabled at GET / + /api/config (NOVA_CONFIG_UI); per-request config.toml reads active"
        } else {
            "disabled (set NOVA_CONFIG_UI=true to enable GET / + /api/config)"
        }
    );
    axum::serve(listener, app.with_state(state)).await?;
    Ok(())
}

async fn mcp_post(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    // 0. Concurrency cap FIRST — acquire the permit BEFORE buffering the body,
    //    so slow or oversized request bodies can't tie up memory/connections
    //    past the cap without ever hitting 429.
    let permit = match state.limiter.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return (StatusCode::TOO_MANY_REQUESTS, "server at capacity").into_response(),
    };

    let (parts, body) = request.into_parts();
    let headers = parts.headers;

    // Every request must authenticate first — before any body read or DNS — so
    // an unauthenticated caller can never reach the resolver or spend work on a
    // request that would be denied. Accepts the master token or a login-issued
    // session token.
    if !authorize(&headers, &state).await {
        return unauthorized_response();
    }

    // Read the body under a hard size cap, now that the permit is held.
    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };

    // 1. Origin validation (DNS-rebinding defense). Absent Origin (non-browser
    //    clients) is allowed; a present Origin must be on the allowlist when one
    //    is configured. Enforced server-side, not via CORS alone.
    if !origin_allowed(&headers, &state.allowed_origins) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }

    // 2. Parse the JSON-RPC body ourselves so we control the error shape.
    let mut request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return json_rpc_error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                -32700,
                format!("parse error: {err}"),
            );
        }
    };

    // 3. Batching was removed in protocol 2025-06-18+; reject arrays.
    if request.is_array() {
        return json_rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            -32600,
            "batch requests are not supported".to_string(),
        );
    }

    let id = request.get("id").cloned().unwrap_or(Value::Null);

    // 3b. Streamable HTTP protocol-version handling. This endpoint implements
    //     only Streamable HTTP (not the deprecated 2024-11-05 HTTP+SSE
    //     transport): a non-initialize request with an unsupported
    //     MCP-Protocol-Version header -> 400; an initialize that asks for a
    //     non-Streamable-HTTP version is never echoed back (we declare our
    //     latest instead), so clients can't negotiate a transport we don't serve.
    if request.get("method").and_then(Value::as_str) == Some("initialize") {
        let requested = request
            .pointer("/params/protocolVersion")
            .and_then(Value::as_str);
        if requested.is_some_and(|version| !HTTP_PROTOCOL_VERSIONS.contains(&version)) {
            if let Some(params) = request.get_mut("params").and_then(Value::as_object_mut) {
                params.insert(
                    "protocolVersion".to_string(),
                    Value::from(crate::mcp::LATEST_PROTOCOL_VERSION),
                );
            }
        }
    } else if let Some(version) = header_str(&headers, "mcp-protocol-version") {
        if !HTTP_PROTOCOL_VERSIONS.contains(&version) {
            return json_rpc_error(
                StatusCode::BAD_REQUEST,
                id,
                -32600,
                format!("unsupported MCP-Protocol-Version: {version}"),
            );
        }
    }

    // 4. Derive a per-request config from operator defaults (the server's own
    //    keys) and build a request-scoped, fully-credentialed service. Missing
    //    required key -> 401 (fail-closed); OAuth -> 400.
    let config = request_config(&state.base_env, state.config_ui);
    let service =
        match SearchService::for_request(state.http_client.clone(), state.cache.clone(), config) {
            Ok(service) => service,
            Err(err) => return for_request_error(id.clone(), err),
        };

    // 5. Clamp abusable numeric args (DoS) — HTTP path only; stdio is untouched.
    clamp_request_args(&mut request);

    // 6. SSRF guard: for URL-fetching tools, block non-public / bad-scheme
    //    targets before any server-side request is made. HTTP path only, so
    //    local stdio users keep full fetch capability (e.g. localhost). The
    //    hostname lookup takes a DNS permit too: a timed-out getaddrinfo keeps
    //    its blocking thread alive, so the same semaphore that bounds gateway
    //    lookups must bound fetch-validation lookups.
    if let Some(url) = fetch_tool_url(&request) {
        let dns_permit = match state.dns_limiter.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return json_rpc_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    id,
                    -32603,
                    "resolver busy".to_string(),
                )
            }
        };
        if let Err((status, message)) = validate_public_url(url, Some(dns_permit)).await {
            return json_rpc_error(status, id, -32602, message);
        }
    }

    // 7. Dispatch through the shared, transport-agnostic handler. A request
    //    (has an `id`) from an SSE-accepting client is streamed: the response
    //    starts immediately with a heartbeat, so a slow search can never leave
    //    the connection idle long enough for Cloudflare to 524 it (the final
    //    `event: message` frame is byte-identical to the single-shot path).
    //    Non-SSE clients get one application/json body; notifications (`None`)
    //    get 202 with an empty body.
    if request.get("id").is_some() && wants_sse(&headers) {
        sse_stream_response(service, request, permit)
    } else {
        match handle_message(&service, request).await {
            Some(response) if wants_sse(&headers) => sse_response(&response),
            Some(response) => (StatusCode::OK, Json(response)).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic-compatible Messages endpoint (`POST /messages`).
// ---------------------------------------------------------------------------
// The official DeepSeek web-search backend (`@deepseek-ai/dsh-web-search-deepseek`)
// is the only *zero-plugin* way to plug a custom search URL + key into DSH: its
// `baseURL` / `apiKey` settings make it issue `POST {baseURL}/messages` with a
// native `web_search_20250305` server tool and then read the reply's
// `web_search_tool_result` blocks. This endpoint speaks that protocol on top of
// nova's own search pipeline, so nova can replace the official backend with NO
// DSH plugin — just `baseURL: http://host` + `apiKey: <GROK_MCP_API_TOKEN>`.
//
// Authentication reuses the same master bearer token as `/mcp` (server-config
// mode). Provider keys still come from the server's own environment.

/// The exact prompt prefix the DeepSeek provider prefixes every query with.
const ANTHROPIC_QUERY_PREFIX: &str = "Perform a web search for the query: ";

/// Extract the user's query from a Messages request body. DSH sends exactly one
/// user turn whose `content` is a single `text` block; a string content or an
/// array of text blocks are both accepted, joined, and the known prefix (when
/// present) is stripped.
fn extract_messages_query(body: &Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    let mut texts: Vec<String> = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        match message.get("content") {
            Some(Value::String(text)) => texts.push(text.clone()),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            texts.push(text.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let joined = texts.join("\n");
    let query = joined
        .strip_prefix(ANTHROPIC_QUERY_PREFIX)
        .unwrap_or(joined.as_str())
        .trim();
    (!query.is_empty()).then(|| query.to_string())
}

/// Map a nova [`WebSearchOutput`] onto the Messages reply shape the DeepSeek
/// provider reads: a `web_search_tool_result` block (url/title/page_age) plus a
/// `text` block whose `citations[]` carry each source's excerpt as `cited_text`
/// (the provider's snippet source — it never reads `web_search_result` items for
/// snippets, only `text.citations`). The synthesized answer rides in the text
/// block's `text` for any human reader; the provider itself consumes citations.
fn messages_response(output: &WebSearchOutput) -> Value {
    let mut items = Vec::new();
    let mut citations = Vec::new();
    for source in &output.sources {
        if source.url.trim().is_empty() {
            continue;
        }
        let mut item = serde_json::Map::new();
        item.insert("type".to_string(), Value::from("web_search_result"));
        item.insert("url".to_string(), Value::from(source.url.clone()));
        if let Some(title) = source.title.as_deref().filter(|t| !t.trim().is_empty()) {
            item.insert("title".to_string(), Value::from(title.to_string()));
        }
        if let Some(date) = source
            .published_date
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            item.insert("page_age".to_string(), Value::from(date.to_string()));
        }
        items.push(Value::Object(item));

        if let Some(excerpt) = source
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
        {
            citations.push(serde_json::json!({
                "type": "char_location",
                "cited_text": excerpt,
                "url": source.url,
                "start_char_index": 0,
                "end_char_index": 0,
            }));
        }
    }

    serde_json::json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4()),
        "type": "message",
        "role": "assistant",
        "model": "nova-veil-search",
        "content": [
            {
                "type": "web_search_tool_result",
                "tool_use_id": format!("toolu_{}", uuid::Uuid::new_v4()),
                "content": items,
            },
            {
                "type": "text",
                "text": output.content,
                "citations": citations,
            },
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": { "input_tokens": 0, "output_tokens": 0 },
    })
}

/// An Anthropic Messages API error body (not JSON-RPC): the DeepSeek provider
/// parses `error.message` from a non-2xx response.
fn anthropic_error(status: StatusCode, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": message },
        })),
    )
        .into_response()
}

/// `POST /messages`: adapt the DeepSeek backend's Messages request to a nova
/// `web_search` call, then adapt the result back. Plain JSON in/out (the DSH
/// provider never speaks SSE), so it stays separate from the /mcp streaming path.
async fn messages_post(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    // Concurrency cap first, exactly like /mcp.
    let _permit = match state.limiter.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return (StatusCode::TOO_MANY_REQUESTS, "server at capacity").into_response(),
    };

    let (parts, body) = request.into_parts();
    let headers = parts.headers;

    // Same master token as /mcp. DSH sends both `authorization: Bearer` and
    // `x-api-key`; accept either so a bare Anthropic client also works.
    let presented = bearer_token(&headers).map(str::to_owned).or_else(|| {
        header_str(&headers, "x-api-key")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    });
    let authorized = presented.is_some_and(|value| constant_time_eq(&value, &state.api_token));
    if !authorized {
        return unauthorized_response();
    }

    let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };

    let body: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return anthropic_error(StatusCode::BAD_REQUEST, format!("parse error: {error}"))
        }
    };

    let Some(query) = extract_messages_query(&body) else {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "messages[].content had no user text".to_string(),
        );
    };

    // Reuse the per-request config + service exactly as /mcp does (server keys).
    let config = request_config(&state.base_env, state.config_ui);
    let service =
        match SearchService::for_request(state.http_client.clone(), state.cache.clone(), config) {
            Ok(service) => service,
            Err(error) => {
                let status = match &error {
                    NovaVeilSearchError::MissingConfig(_) => StatusCode::UNAUTHORIZED,
                    _ => StatusCode::BAD_REQUEST,
                };
                return anthropic_error(status, error.to_string());
            }
        };

    let input = WebSearchInput {
        query,
        // The provider renders only sources (url/title/snippet/publishedAt), so
        // "concise" (answer + source metadata, no inline content) is sufficient
        // and the smallest transfer.
        response_format: Some("concise".to_string()),
        ..WebSearchInput::default()
    };
    let output = match service.web_search(input).await {
        Ok(output) => output,
        Err(error) => {
            return anthropic_error(StatusCode::BAD_GATEWAY, error.to_string());
        }
    };

    (StatusCode::OK, Json(messages_response(&output))).into_response()
}

/// Whether the client accepts an SSE stream (Streamable HTTP streaming mode).
/// Streamable HTTP clients send `Accept: application/json, text/event-stream`.
fn wants_sse(headers: &HeaderMap) -> bool {
    header_str(headers, "accept")
        .map(|accept| accept.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Frame a single JSON-RPC response as a one-event SSE stream that then closes,
/// per the Streamable HTTP transport: one `message` event carrying the JSON-RPC
/// response, after which the stream ends (these tools are request/response).
fn sse_response(response: &Value) -> Response {
    sse_headers(Response::new(axum::body::Body::from(sse_message_frame(
        response,
    ))))
}

/// Build a streaming SSE response that keeps the connection warm while a tool
/// call runs, then ends on the same `message` frame as [`sse_response`]. The
/// search runs on a spawned task; heartbeat comment frames are written once
/// immediately and then every [`SSE_KEEPALIVE_SECONDS`], so the wire never sits
/// silent — the exact failure mode Cloudflare turns into a 524.
fn sse_stream_response(
    service: SearchService,
    request: Value,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Infallible>>(8);

    tokio::spawn(async move {
        // Hold the concurrency permit until the search (not just the response
        // hand-off) is done, so streaming can't bypass MAX_CONCURRENT_REQUESTS.
        let _permit = permit;

        // Start the stream immediately: the first bytes leave before any
        // upstream work, which is what keeps a fronting proxy from timing out.
        if tx.send(Ok(SSE_HEARTBEAT.to_vec())).await.is_err() {
            return; // client gone before we even started
        }

        let mut handle = tokio::spawn(async move { handle_message(&service, request).await });

        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SSE_KEEPALIVE_SECONDS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The first interval tick fires immediately; skip it, since the initial
        // heartbeat above already put bytes on the wire.
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if tx.send(Ok(SSE_HEARTBEAT.to_vec())).await.is_err() {
                        break; // client disconnected mid-search
                    }
                }
                result = &mut handle => {
                    // `Ok(None)` = a notification (unreachable here: the caller
                    // only streams requests with an `id`); `Err` = the handler
                    // task panicked. Either way just end.
                    if let Ok(Some(response)) = result {
                        let _ = tx.send(Ok(sse_message_frame(&response))).await;
                    }
                    break;
                }
            }
        }
        // `tx` drops here -> the body stream ends -> the connection closes.
    });

    sse_headers(Response::new(axum::body::Body::from_stream(
        ReceiverStream::new(rx),
    )))
}

/// The `message` event frame carrying one JSON-RPC response — shared verbatim
/// by the single-shot and streaming SSE paths.
fn sse_message_frame(response: &Value) -> Vec<u8> {
    let data = serde_json::to_string(response).unwrap_or_else(|_| "{}".to_string());
    format!("event: message\ndata: {data}\n\n").into_bytes()
}

/// Shared SSE response headers (`text/event-stream`, `no-cache`).
fn sse_headers(mut resp: Response) -> Response {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    resp.headers_mut().insert(
        CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut().insert(
        CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    resp
}

/// If `request` is a tools/call for a URL-fetching tool (`web_fetch`/`web_map`),
/// return its `url` argument for SSRF validation.
fn fetch_tool_url(request: &Value) -> Option<&str> {
    if request.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let params = request.get("params")?;
    match params.get("name").and_then(Value::as_str)? {
        "web_fetch" | "web_map" => params.get("arguments")?.get("url")?.as_str(),
        _ => None,
    }
}

/// Reject a URL that could drive a server-side request at a non-public target
/// (SSRF): bad scheme, or a host that is / resolves to a private, loopback,
/// link-local (incl. cloud metadata), or CGNAT address. On success returns the
/// validated public IP(s) the host resolves to, so the caller can pin the
/// actual connection to them (closing DNS-rebinding between check and use).
async fn validate_public_url(
    raw: &str,
    dns_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Vec<std::net::IpAddr>, (StatusCode, String)> {
    let parsed = url::Url::parse(raw)
        .map_err(|err| (StatusCode::BAD_REQUEST, format!("invalid url: {err}")))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unsupported url scheme: {other}"),
            ))
        }
    }
    let host = parsed
        .host_str()
        .ok_or((StatusCode::BAD_REQUEST, "url has no host".to_string()))?;

    // IP literal (strip IPv6 brackets): check directly, no DNS lookup.
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<std::net::IpAddr>() {
        return if crate::providers::http::is_public_ip(&ip) {
            Ok(vec![ip])
        } else {
            Err((
                StatusCode::FORBIDDEN,
                "url targets a non-public address".to_string(),
            ))
        };
    }

    // Hostname: resolve (under a hard deadline so a slow resolver can't hold a
    // concurrency permit) and require every resolved address to be public.
    let port = parsed.port_or_known_default().unwrap_or(443);
    let host_owned = host.to_string();
    let resolved = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            // Hold the resolver permit until getaddrinfo actually returns: the
            // 5s timeout abandons this task but cannot cancel it, so the permit
            // (not the timeout) is what bounds concurrent hung lookups.
            let _dns_permit = dns_permit;
            use std::net::ToSocketAddrs;
            (host_owned.as_str(), port)
                .to_socket_addrs()
                .map(|iter| iter.map(|addr| addr.ip()).collect::<Vec<_>>())
        }),
    )
    .await
    .map_err(|_| {
        (
            StatusCode::GATEWAY_TIMEOUT,
            "host resolution timed out".to_string(),
        )
    })?
    .map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("resolver task failed: {err}"),
        )
    })?
    .map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            format!("cannot resolve host: {err}"),
        )
    })?;

    if resolved.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "host did not resolve".to_string()));
    }
    for ip in &resolved {
        if !crate::providers::http::is_public_ip(ip) {
            return Err((
                StatusCode::FORBIDDEN,
                "url resolves to a non-public address".to_string(),
            ));
        }
    }
    Ok(resolved)
}

/// Map a `SearchService::for_request` construction error to a JSON-RPC HTTP
/// response: a missing key -> 401 (fail-closed), OAuth / other -> 400.
fn for_request_error(id: Value, err: NovaVeilSearchError) -> Response {
    let status = match err {
        NovaVeilSearchError::MissingConfig(_) => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_REQUEST,
    };
    json_rpc_error(status, id, err.code() as i64, err.to_string())
}

/// Clamp abusable numeric tool arguments to sane upper bounds so a single
/// public request cannot ask for unbounded work.
fn clamp_request_args(request: &mut Value) {
    if request.get("method").and_then(Value::as_str) != Some("tools/call") {
        return;
    }
    let Some(params) = request.get_mut("params") else {
        return;
    };
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(args) = params.get_mut("arguments").and_then(Value::as_object_mut) else {
        return;
    };
    match name.as_deref() {
        Some("web_search") => {
            clamp_u64(args, "extra_sources", 50);
            clamp_u64(args, "recency_days", 3650);
        }
        // Enforce the fetch cap even when max_chars is absent or non-integer, so
        // a public caller can't bypass it by omitting the argument (the operator
        // default fetch cap is unbounded).
        Some("web_fetch") => cap_u64(args, "max_chars", 5_000_000),
        Some("web_map") => clamp_u64(args, "max_results", 100),
        _ => {}
    }
}

fn clamp_u64(args: &mut serde_json::Map<String, Value>, key: &str, max: u64) {
    if let Some(value) = args.get(key).and_then(Value::as_u64) {
        if value > max {
            args.insert(key.to_string(), Value::from(max));
        }
    }
}

/// Like [`clamp_u64`], but also enforces `max` when the argument is absent or
/// non-integer — so a caller cannot bypass the cap by omitting it.
fn cap_u64(args: &mut serde_json::Map<String, Value>, key: &str, max: u64) {
    match args.get(key).and_then(Value::as_u64) {
        Some(value) if value <= max => {}
        _ => {
            args.insert(key.to_string(), Value::from(max));
        }
    }
}

/// Build a per-request [`Config`] from the server's own environment; keys never
/// come from request headers. With `read_file` (settings frontend enabled via
/// `NOVA_CONFIG_UI`) the full precedence chain (env > config.toml > defaults)
/// applies, so persisted `config.toml` edits take effect on the next request
/// without a restart. Otherwise env-only — the default, zero per-request disk I/O.
fn request_config(base_env: &HashMap<String, String>, read_file: bool) -> Config {
    if read_file {
        Config::load_from(base_env.clone())
    } else {
        Config::from_env_map(base_env.clone())
    }
}

pub(crate) fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Shared origin allowlist check (DNS-rebinding defense) for every browser-facing
/// endpoint. Absent `Origin` (curl, non-browser) is allowed; a present `Origin`
/// must be on the allowlist when one is configured.
pub(crate) fn origin_allowed(headers: &HeaderMap, allowed: &Option<HashSet<String>>) -> bool {
    if let Some(origin) = header_str(headers, "origin") {
        if let Some(allowed) = allowed.as_ref() {
            if !allowed.contains(origin) {
                return false;
            }
        }
    }
    true
}

/// Extract the bearer token from an `Authorization` header for server-config
/// mode. The scheme is matched case-insensitively (RFC 7235); anything other
/// than `Bearer <non-empty token>` -> `None`.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = header_str(headers, "authorization")?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Constant-time comparison so a token mismatch cannot be timed over the
/// network. Length is public by necessity (the lengths are compared first), but
/// the byte loop runs the same number of steps regardless of where the bytes
/// differ.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Authorize a bearer token against the master token (constant-time) or the
/// login-issued session store (sliding expiry).
pub(crate) async fn authorize(headers: &HeaderMap, state: &AppState) -> bool {
    let Some(presented) = bearer_token(headers) else {
        return false;
    };
    if constant_time_eq(presented, &state.api_token) {
        return true;
    }
    state
        .sessions
        .lock()
        .await
        .validate(presented, state.session_ttl)
}

/// `POST /login` — verifies admin credentials and issues a short-lived session
/// token the web UI sends on subsequent `/mcp` calls. Body:
/// `{"username": "...", "password": "..."}`. Disabled (404) when no
/// `NOVA_ADMIN_PASSWORD` is configured.
async fn login(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    let Some(password) = state.admin_password.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let body = match axum::body::to_bytes(request.into_body(), MAX_LOGIN_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    let creds: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return unauthorized_response(),
    };
    let user = creds.get("username").and_then(Value::as_str).unwrap_or("");
    let pass = creds.get("password").and_then(Value::as_str).unwrap_or("");

    let user_ok = constant_time_eq(user, &state.admin_user);
    let pass_ok = password_ok(pass, password);
    if !(user_ok && pass_ok) {
        // Slow the failure a touch to blunt brute-force attempts.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        return unauthorized_response();
    }

    let token = state.sessions.lock().await.issue(state.session_ttl);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "token": token,
            "expires_in_seconds": state.session_ttl.as_secs(),
        })),
    )
        .into_response()
}

/// Constant-time password compare, length-independent so the stored password's
/// length is not revealed by a short-circuit.
fn password_ok(presented: &str, stored: &str) -> bool {
    let (presented, stored) = (presented.as_bytes(), stored.as_bytes());
    let mut diff = presented.len() ^ stored.len();
    let n = presented.len().max(stored.len());
    for i in 0..n {
        let p = presented.get(i).copied().unwrap_or(0);
        let s = stored.get(i).copied().unwrap_or(0);
        diff |= (p ^ s) as usize;
    }
    diff == 0
}

/// Plain `401` for a missing/incorrect bearer token. Carries `WWW-Authenticate`
/// so standards-compliant clients and debug tools know to present a token.
pub(crate) fn unauthorized_response() -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    response.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Bearer"),
    );
    response
}

fn json_rpc_error(status: StatusCode, id: Value, code: i64, message: String) -> Response {
    (status, Json(error_response(id, code, message))).into_response()
}

/// Per-request base env: the operator env minus non-config entries (the master
/// token and the admin/session credentials). Keys stay — they always come from
/// the server's own environment.
fn request_base_env(base_env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = base_env.clone();
    for key in [
        API_TOKEN_ENV,
        ADMIN_USER_ENV,
        ADMIN_PASSWORD_ENV,
        SESSION_TTL_ENV,
    ] {
        env.remove(key);
    }
    env
}

/// Parse a boolean env flag (`1`/`true`/`yes`, case-insensitive). Absent or any
/// other value is OFF.
fn env_is_true(env: &HashMap<String, String>, key: &str) -> bool {
    matches!(
        env.get(key).map(|value| value.trim()),
        Some(value)
            if value.eq_ignore_ascii_case("1")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
    )
}

/// Parse `GROK_MCP_ALLOWED_ORIGINS` (comma-separated) into an allowlist.
/// Unset/empty -> `None` (no browser-origin restriction).
fn parse_allowed_origins(env: &HashMap<String, String>) -> Option<HashSet<String>> {
    let raw = env.get("GROK_MCP_ALLOWED_ORIGINS")?;
    let set: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HashMap<String, String> {
        HashMap::new()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn request_config_uses_server_env_only() {
        // Caller headers never carry keys: config seeds from the server's own
        // env, and an absent model falls back to the built-in default.
        let mut env = base();
        env.insert("GROK_SEARCH_API_KEY".to_string(), "xai-server".to_string());
        env.insert("TAVILY_API_KEY".to_string(), "tvly-server".to_string());
        let cfg = request_config(&env, false);
        assert_eq!(cfg.grok_api_key.as_deref(), Some("xai-server"));
        assert_eq!(cfg.tavily_api_key.as_deref(), Some("tvly-server"));
        assert_eq!(cfg.grok_model, "grok-4-1-fast-reasoning");
    }

    #[test]
    fn request_base_env_keeps_keys_but_drops_non_config_entries() {
        let mut env = base();
        env.insert("TAVILY_API_KEY".to_string(), "tvly-server".to_string());
        env.insert("GROK_SEARCH_TIMEOUT_SECONDS".to_string(), "30".to_string());
        env.insert("GROK_MCP_API_TOKEN".to_string(), "s3cret".to_string());
        env.insert("NOVA_ADMIN_USER".to_string(), "admin".to_string());
        env.insert("NOVA_ADMIN_PASSWORD".to_string(), "hunter2".to_string());
        env.insert("NOVA_SESSION_TTL_SECONDS".to_string(), "3600".to_string());
        let out = request_base_env(&env);
        assert_eq!(
            out.get("TAVILY_API_KEY").map(String::as_str),
            Some("tvly-server"),
            "server key must survive into the request config"
        );
        assert_eq!(
            out.get("GROK_SEARCH_TIMEOUT_SECONDS").map(String::as_str),
            Some("30")
        );
        for key in [
            "GROK_MCP_API_TOKEN",
            "NOVA_ADMIN_USER",
            "NOVA_ADMIN_PASSWORD",
            "NOVA_SESSION_TTL_SECONDS",
        ] {
            assert!(!out.contains_key(key), "{key} must not be a config key");
        }
    }

    #[test]
    fn env_is_true_parses_opt_in_flag() {
        let mut env = base();
        assert!(!env_is_true(&env, "NOVA_CONFIG_UI"));
        for value in ["1", "true", "yes", "TRUE", "Yes"] {
            env.insert("NOVA_CONFIG_UI".to_string(), value.to_string());
            assert!(
                env_is_true(&env, "NOVA_CONFIG_UI"),
                "{value:?} should be on"
            );
        }
        for value in ["0", "false", "no", "on", "", "2", "random"] {
            env.insert("NOVA_CONFIG_UI".to_string(), value.to_string());
            assert!(
                !env_is_true(&env, "NOVA_CONFIG_UI"),
                "{value:?} should be off"
            );
        }
    }

    #[test]
    fn request_config_gates_file_read_on_config_ui() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, "grok_model = \"from-file\"\n").unwrap();

        let mut env = base();
        env.insert(
            "GROK_SEARCH_CONFIG".to_string(),
            cfg_path.to_string_lossy().to_string(),
        );

        // Default (config UI OFF): env-only — the file is never opened.
        let env_only = request_config(&env, false);
        assert_eq!(
            env_only.config_file_state,
            crate::config::ConfigFileState::Absent
        );
        assert_ne!(env_only.grok_model, "from-file");

        // Config UI ON: file is merged (env > file > defaults).
        let merged = request_config(&env, true);
        assert_eq!(
            merged.config_file_state,
            crate::config::ConfigFileState::Loaded
        );
        assert_eq!(merged.grok_model, "from-file");
    }

    #[test]
    fn password_ok_compares_length_independently() {
        assert!(password_ok("hunter2", "hunter2"));
        assert!(!password_ok("hunter2", "hunter3"));
        assert!(!password_ok("hunter", "hunter2"));
        assert!(!password_ok("", "hunter2"));
        assert!(password_ok("", ""));
    }

    #[test]
    fn session_store_issues_and_validates_tokens() {
        let ttl = std::time::Duration::from_secs(3600);
        let mut store = SessionStore::new();
        let token = store.issue(ttl);
        assert!(store.validate(&token, ttl));
        assert!(!store.validate("not-issued", ttl));
        // Expired tokens are rejected and pruned.
        let expired = store.issue(std::time::Duration::ZERO);
        assert!(!store.validate(&expired, ttl));
    }

    #[test]
    fn bearer_token_parses_and_rejects() {
        assert_eq!(
            bearer_token(&headers(&[("Authorization", "Bearer abc")])),
            Some("abc")
        );
        assert_eq!(
            bearer_token(&headers(&[("Authorization", "bearer abc")])),
            Some("abc")
        );
        assert_eq!(
            bearer_token(&headers(&[("Authorization", "Bearer   abc  ")])),
            Some("abc")
        );
        assert_eq!(
            bearer_token(&headers(&[("Authorization", "Basic abc")])),
            None
        );
        assert_eq!(
            bearer_token(&headers(&[("Authorization", "Bearer  ")])),
            None
        );
        assert_eq!(bearer_token(&headers(&[])), None);
    }

    #[test]
    fn constant_time_eq_compares_exactly() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn parse_allowed_origins_handles_unset_and_list() {
        assert!(parse_allowed_origins(&base()).is_none());
        let mut env = HashMap::new();
        env.insert(
            "GROK_MCP_ALLOWED_ORIGINS".to_string(),
            "https://a.example, https://b.example".to_string(),
        );
        let set = parse_allowed_origins(&env).expect("allowlist");
        assert!(set.contains("https://a.example"));
        assert!(set.contains("https://b.example"));
    }

    #[tokio::test]
    async fn validate_public_url_blocks_ssrf_targets() {
        for bad in [
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "http://127.0.0.1/",                        // loopback
            "http://10.0.0.5/",                         // private
            "http://192.168.1.1/",                      // private
            "http://100.64.0.1/",                       // CGNAT
            "https://[::1]/",                           // IPv6 loopback
            "file:///etc/passwd",                       // bad scheme
            "gopher://example.com/",                    // bad scheme
        ] {
            assert!(
                validate_public_url(bad, None).await.is_err(),
                "expected {bad} to be rejected"
            );
        }
    }

    #[tokio::test]
    async fn validate_public_url_allows_public_ip_literal() {
        // Public IP literals pass with no DNS lookup, returning the pinned IP.
        assert_eq!(
            validate_public_url("https://1.1.1.1/", None).await.unwrap(),
            vec!["1.1.1.1".parse::<std::net::IpAddr>().unwrap()]
        );
        assert!(validate_public_url("http://8.8.8.8/", None).await.is_ok());
    }

    #[test]
    fn clamp_request_args_caps_numeric_inputs() {
        let mut request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "web_search",
                "arguments": { "query": "q", "extra_sources": 9999, "recency_days": 999999 }
            }
        });
        clamp_request_args(&mut request);
        assert_eq!(request["params"]["arguments"]["extra_sources"], 50);
        assert_eq!(request["params"]["arguments"]["recency_days"], 3650);
    }

    #[test]
    fn clamp_request_args_enforces_web_fetch_cap() {
        // Absent max_chars -> cap injected (cannot bypass by omitting it).
        let mut absent = serde_json::json!({
            "method": "tools/call",
            "params": { "name": "web_fetch", "arguments": { "url": "https://example.com" } }
        });
        clamp_request_args(&mut absent);
        assert_eq!(absent["params"]["arguments"]["max_chars"], 5_000_000);
        // Oversized -> clamped down.
        let mut big = serde_json::json!({
            "method": "tools/call",
            "params": { "name": "web_fetch", "arguments": { "url": "x", "max_chars": 999_999_999_u64 } }
        });
        clamp_request_args(&mut big);
        assert_eq!(big["params"]["arguments"]["max_chars"], 5_000_000);
        // Smaller value -> kept.
        let mut small = serde_json::json!({
            "method": "tools/call",
            "params": { "name": "web_fetch", "arguments": { "url": "x", "max_chars": 1000 } }
        });
        clamp_request_args(&mut small);
        assert_eq!(small["params"]["arguments"]["max_chars"], 1000);
    }

    #[test]
    fn fetch_tool_url_targets_fetch_tools_only() {
        let fetch = serde_json::json!({
            "method": "tools/call",
            "params": { "name": "web_fetch", "arguments": { "url": "https://example.com" } }
        });
        assert_eq!(fetch_tool_url(&fetch), Some("https://example.com"));
        let search = serde_json::json!({
            "method": "tools/call",
            "params": { "name": "web_search", "arguments": { "query": "x" } }
        });
        assert_eq!(fetch_tool_url(&search), None);
    }

    #[test]
    fn wants_sse_reads_accept_header() {
        assert!(wants_sse(&headers(&[(
            "Accept",
            "application/json, text/event-stream"
        )])));
        assert!(!wants_sse(&headers(&[("Accept", "application/json")])));
        assert!(!wants_sse(&headers(&[])));
    }

    #[tokio::test]
    async fn sse_response_frames_a_single_message_event() {
        let resp = sse_response(&serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}));
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/event-stream"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.starts_with("event: message\ndata: "));
        assert!(text.ends_with("\n\n"));
        assert!(text.contains("\"jsonrpc\":\"2.0\""));
    }

    #[tokio::test]
    async fn sse_stream_emits_keepalive_then_final_message() {
        let service = SearchService::fake_with_sources();
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let resp = sse_stream_response(
            service,
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            permit,
        );
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/event-stream"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        // The first heartbeat is sent before any search work, so it is always
        // present even when the request resolves instantly.
        assert!(
            text.contains(": keep-alive"),
            "stream must start with a keepalive frame: {text:?}"
        );
        assert!(
            text.contains("event: message"),
            "stream must end with the JSON-RPC message frame: {text:?}"
        );
        assert!(text.contains("\"jsonrpc\":\"2.0\""));
    }
}
