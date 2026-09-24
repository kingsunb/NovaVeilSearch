use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::error::{NovaVeilSearchError, Result};
use crate::oauth::constants::{
    DEFAULT_XAI_OAUTH_BASE_URL, XAI_OAUTH_CLIENT_ID, XAI_OAUTH_DISCOVERY_URL, XAI_OAUTH_ISSUER,
    XAI_OAUTH_REDIRECT_HOST, XAI_OAUTH_REDIRECT_PATH, XAI_OAUTH_REDIRECT_PORT, XAI_OAUTH_SCOPE,
};
use crate::oauth::pkce::{pkce_pair, random_url_token};
use crate::oauth::token_store::{save_token_store, TokenStore};

#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

pub async fn login(auth_path: &Path, open_browser: bool) -> Result<TokenStore> {
    eprintln!("WARNING: OAuth mode reuses Hermes' xAI OAuth client_id.");
    eprintln!("This may violate xAI terms or affect your account. Use at your own risk.");

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_client_failed: {err}")))?;
    let discovery = discover(&client).await?;
    validate_auth_endpoint(&discovery.authorization_endpoint)?;
    validate_token_endpoint(&discovery.token_endpoint)?;

    let redirect_uri = format!(
        "http://{}:{}{}",
        XAI_OAUTH_REDIRECT_HOST, XAI_OAUTH_REDIRECT_PORT, XAI_OAUTH_REDIRECT_PATH
    );
    let listener =
        TcpListener::bind((XAI_OAUTH_REDIRECT_HOST, XAI_OAUTH_REDIRECT_PORT)).map_err(|err| {
            NovaVeilSearchError::OAuth(format!(
                "oauth_callback_bind_failed: cannot listen on {redirect_uri}: {err}"
            ))
        })?;
    listener
        .set_nonblocking(true)
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_callback_failed: {err}")))?;

    let (verifier, challenge) = pkce_pair();
    let state = random_url_token(16);
    let nonce = random_url_token(16);
    let authorize_url = authorize_url(
        &discovery.authorization_endpoint,
        &redirect_uri,
        &challenge,
        &state,
        &nonce,
    )?;

    println!("Open this URL to authorize with xAI:");
    println!("{authorize_url}");
    println!();
    println!("Waiting for callback on {redirect_uri}");

    if open_browser {
        match open_authorize_url(authorize_url.as_str()) {
            Ok(_) => println!("Browser opened for xAI authorization."),
            Err(_) => println!("Could not open a browser automatically; open the URL above."),
        }
    }

    let callback = wait_for_callback(&listener, Duration::from_secs(180))?;
    if callback.state.as_deref() != Some(state.as_str()) {
        return Err(NovaVeilSearchError::OAuth(
            "oauth_state_mismatch: callback state did not match".to_string(),
        ));
    }
    let code = callback.code.ok_or_else(|| {
        NovaVeilSearchError::OAuth("oauth_code_missing: callback did not include code".to_string())
    })?;
    let store = exchange_code(&client, &discovery, &code, &verifier, &redirect_uri).await?;
    save_token_store(auth_path, &store)?;
    Ok(store)
}

async fn discover(client: &Client) -> Result<Discovery> {
    let response = client
        .get(XAI_OAUTH_DISCOVERY_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_discovery_failed: {err}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(NovaVeilSearchError::OAuth(format!(
            "oauth_discovery_failed: HTTP {status}"
        )));
    }
    response
        .json::<Discovery>()
        .await
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_discovery_parse_failed: {err}")))
}

fn authorize_url(
    authorization_endpoint: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
    nonce: &str,
) -> Result<Url> {
    let mut url = Url::parse(authorization_endpoint)
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_authorize_url_invalid: {err}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", XAI_OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", XAI_OAUTH_SCOPE)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("plan", "generic")
        .append_pair("referrer", "nova-veil-search");
    Ok(url)
}

async fn exchange_code(
    client: &Client,
    discovery: &Discovery,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenStore> {
    let response = client
        .post(&discovery.token_endpoint)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", XAI_OAUTH_CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_token_exchange_failed: {err}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_token_body_failed: {err}")))?;
    if !status.is_success() {
        return Err(NovaVeilSearchError::OAuth(format!(
            "oauth_token_exchange_failed: upstream returned HTTP {status}"
        )));
    }
    let payload = serde_json::from_str::<Value>(&text)
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_token_parse_failed: {err}")))?;
    let access_token = string_field(&payload, "access_token").ok_or_else(|| {
        NovaVeilSearchError::OAuth("oauth_token_missing_access_token".to_string())
    })?;
    let refresh_token = string_field(&payload, "refresh_token").ok_or_else(|| {
        NovaVeilSearchError::OAuth("oauth_token_missing_refresh_token".to_string())
    })?;
    Ok(TokenStore {
        access_token,
        refresh_token,
        id_token: string_field(&payload, "id_token").unwrap_or_default(),
        token_endpoint: discovery.token_endpoint.clone(),
        base_url: DEFAULT_XAI_OAUTH_BASE_URL.to_string(),
        last_refresh: unix_now().to_string(),
    })
}

#[derive(Debug)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

fn wait_for_callback(listener: &TcpListener, timeout: Duration) -> Result<Callback> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                stream.set_nonblocking(false).map_err(|err| {
                    NovaVeilSearchError::OAuth(format!("oauth_callback_stream_failed: {err}"))
                })?;
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).map_err(|err| {
                    NovaVeilSearchError::OAuth(format!("oauth_callback_read_failed: {err}"))
                })?;
                let request = String::from_utf8_lossy(&buf[..n]);
                let line = request.lines().next().unwrap_or_default();
                let callback = parse_callback_line(line)?;
                let body = "xAI login complete. You can close this tab.";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                return Ok(callback);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(NovaVeilSearchError::OAuth(
                        "oauth_callback_timeout: no callback received within 180 seconds"
                            .to_string(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                return Err(NovaVeilSearchError::OAuth(format!(
                    "oauth_callback_accept_failed: {err}"
                )));
            }
        }
    }
}

fn parse_callback_line(line: &str) -> Result<Callback> {
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" || target.is_empty() {
        return Err(NovaVeilSearchError::OAuth(
            "oauth_callback_invalid_request".to_string(),
        ));
    }
    let url = Url::parse(&format!("http://localhost{target}"))
        .map_err(|err| NovaVeilSearchError::OAuth(format!("oauth_callback_url_invalid: {err}")))?;
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.to_string());
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.to_string());
    Ok(Callback { code, state })
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn validate_auth_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.starts_with(&format!("{XAI_OAUTH_ISSUER}/")) {
        Ok(())
    } else {
        Err(NovaVeilSearchError::OAuth(
            "oauth_discovery_invalid_authorization_endpoint".to_string(),
        ))
    }
}

fn validate_token_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.starts_with(&format!("{XAI_OAUTH_ISSUER}/")) {
        Ok(())
    } else {
        Err(NovaVeilSearchError::OAuth(
            "oauth_discovery_invalid_token_endpoint".to_string(),
        ))
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn open_authorize_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn()?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opening a browser is not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_line_extracts_code_and_state() {
        let callback = parse_callback_line("GET /callback?code=abc&state=xyz HTTP/1.1").unwrap();
        assert_eq!(callback.code.as_deref(), Some("abc"));
        assert_eq!(callback.state.as_deref(), Some("xyz"));
    }
}
