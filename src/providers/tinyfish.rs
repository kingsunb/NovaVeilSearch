//! TinyFish Search & Fetch provider (issue #12).
//!
//! Two independent endpoints share one API key: search is a GET service at
//! `api.search.tinyfish.ai`, fetch a POST service at `api.fetch.tinyfish.ai`.
//! Both authenticate with an `X-API-Key` header rather than a bearer token,
//! and neither consumes account credits (rate limits still apply).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::error::{NovaVeilSearchError, Result};
use crate::model::search::SearchFilters;
use crate::model::source::{FetchedPage, Source};
use crate::providers::keyring::KeyRing;

use super::http::{
    build_client, get_json_with_header_auth_status, post_json_with_header_auth_status, rotate_keys,
};

const AUTH_HEADER: &str = "X-API-Key";
/// TinyFish caps `recency_minutes` at ten years.
const MAX_RECENCY_MINUTES: u64 = 5_256_000;

#[derive(Clone)]
pub struct TinyfishProvider {
    client: Client,
    /// Per-key `{account}` proxy clients (exact key → client): the request
    /// egresses through the proxy account derived from whichever key in the
    /// ring signs it. Empty when no keyed proxy template names `{account}`.
    per_key_clients: Arc<HashMap<String, Client>>,
    search_api_url: String,
    fetch_api_url: String,
    keys: Arc<KeyRing>,
}

impl TinyfishProvider {
    pub fn new(
        search_api_url: impl Into<String>,
        fetch_api_url: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self::with_client(
            build_client(timeout),
            search_api_url,
            fetch_api_url,
            api_key,
        )
    }

    /// Construct with an externally provided `reqwest::Client`. Used by
    /// `SearchService` to share one tuned client across providers.
    ///
    /// `api_key` accepts a single key or a comma-separated list; multiple
    /// keys rotate round-robin per request with automatic failover on
    /// key-scoped errors (401/403/429/432/433).
    pub fn with_client(
        client: Client,
        search_api_url: impl Into<String>,
        fetch_api_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self::with_clients(
            client,
            HashMap::new(),
            search_api_url,
            fetch_api_url,
            api_key,
        )
    }

    /// [`with_client`] plus the per-key `{account}` proxy clients. Each
    /// rotating request looks up the client for the exact key it signs with,
    /// so one template lands every key on its own proxy account.
    pub fn with_clients(
        client: Client,
        per_key_clients: HashMap<String, Client>,
        search_api_url: impl Into<String>,
        fetch_api_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client,
            per_key_clients: Arc::new(per_key_clients),
            search_api_url: search_api_url.into().trim_end_matches('/').to_string(),
            fetch_api_url: fetch_api_url.into().trim_end_matches('/').to_string(),
            keys: Arc::new(KeyRing::parse(&api_key.into())),
        }
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        let params = tinyfish_search_params(query, filters);
        let raw = rotate_keys(&self.keys, "TinyFish", |key| {
            let params = params.clone();
            async move {
                let client = self.per_key_clients.get(&key).unwrap_or(&self.client);
                get_json_with_header_auth_status(
                    client,
                    &self.search_api_url,
                    &params,
                    (AUTH_HEADER, &key),
                    "TinyFish",
                )
                .await
            }
        })
        .await
        .map_err(|failure| failure.error)?;
        // The API has no result-count parameter (only pagination), so the
        // caller's budget is applied client-side.
        let mut sources = normalize_tinyfish_results(&raw);
        sources.truncate(max_results);
        Ok(sources)
    }

    pub async fn fetch(&self, url: &str) -> Result<FetchedPage> {
        let body = json!({ "urls": [url], "format": "markdown" });
        let raw = rotate_keys(&self.keys, "TinyFish", |key| {
            let body = body.clone();
            async move {
                let client = self.per_key_clients.get(&key).unwrap_or(&self.client);
                post_json_with_header_auth_status(
                    client,
                    &self.fetch_api_url,
                    (AUTH_HEADER, &key),
                    &body,
                    "TinyFish",
                )
                .await
            }
        })
        .await
        .map_err(|failure| failure.error)?;
        parse_tinyfish_fetch(&raw, url)
    }
}

/// Build the GET query parameters. Domain scoping uses TinyFish's dedicated
/// comma-separated `include_domains` / `exclude_domains` parameters: the
/// `site:` / `-site:` query operators still work but are documented as
/// deprecated for domain filtering precisely because they collide with other
/// query syntax — and the caller's query is arbitrary user text, so it can
/// carry that syntax. Recency maps onto `recency_minutes`.
pub fn tinyfish_search_params(query: &str, filters: &SearchFilters) -> Vec<(&'static str, String)> {
    let mut params = vec![("query", query.to_string())];
    if !filters.include_domains.is_empty() {
        params.push(("include_domains", filters.include_domains.join(",")));
    }
    if !filters.exclude_domains.is_empty() {
        params.push(("exclude_domains", filters.exclude_domains.join(",")));
    }
    if let Some(days) = filters.recency_days {
        let minutes = u64::from(days)
            .saturating_mul(24 * 60)
            .clamp(1, MAX_RECENCY_MINUTES);
        params.push(("recency_minutes", minutes.to_string()));
    }
    params
}

pub fn normalize_tinyfish_results(raw: &Value) -> Vec<Source> {
    raw.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            let mut source = Source::new(url, "tinyfish");
            if let Some(title) = item.get("title").and_then(Value::as_str) {
                source = source.with_title(title);
            }
            if let Some(snippet) = item.get("snippet").and_then(Value::as_str) {
                source = source.with_description(snippet);
            }
            if let Some(date) = item.get("date").and_then(Value::as_str) {
                source = source.with_published_date(date);
            }
            Some(source)
        })
        .collect()
}

/// Per-URL failures (timeouts, anti-bot blocks) arrive in `errors[]` beside a
/// 200 response, so an empty `results` is inspected for a reason before the
/// generic "no content" verdict.
pub fn parse_tinyfish_fetch(raw: &Value, url: &str) -> Result<FetchedPage> {
    if let Some(result) = raw
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
    {
        let content = result
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !content.trim().is_empty() {
            return Ok(FetchedPage {
                content,
                title: result
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                published_date: None,
            });
        }
    }
    let detail = raw
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(|error| error.get("error").and_then(Value::as_str))
        .unwrap_or("no content returned");
    Err(NovaVeilSearchError::Provider(format!(
        "TinyFish fetch failed for {url}: {detail}"
    )))
}
