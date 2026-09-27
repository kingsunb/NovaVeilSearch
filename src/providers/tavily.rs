use crate::error::Result;
use crate::model::search::SearchFilters;
use crate::model::source::{FetchedPage, Source};
use crate::providers::http::{
    build_client, post_json_with_header_auth, post_json_with_status, rotate_keys,
};
use crate::providers::keyring::KeyRing;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::error::NovaVeilSearchError;

#[derive(Clone)]
pub struct TavilyProvider {
    client: Client,
    /// Per-key `{account}` proxy clients (exact key → client): the request
    /// egresses through the proxy account derived from whichever key in the
    /// ring signs it. Empty when no keyed proxy template names `{account}`;
    /// every request then uses `client`.
    per_key_clients: Arc<HashMap<String, Client>>,
    api_url: String,
    keys: Arc<KeyRing>,
    /// Keyless anonymous mode (`x-tavily-access-mode: keyless`), used when no
    /// API key is configured. Anonymous access is rate-limited.
    keyless: bool,
}

impl TavilyProvider {
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>, timeout: Duration) -> Self {
        Self::with_client(build_client(timeout), api_url, api_key)
    }

    /// Construct with an externally provided `reqwest::Client`. Used by
    /// `SearchService::new` to share one tuned client across providers.
    ///
    /// `api_key` accepts a single key or a comma-separated list; multiple
    /// keys are used round-robin with automatic failover on key-scoped
    /// errors (401/403/429/432/433).
    pub fn with_client(
        client: Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self::with_clients_mode(client, HashMap::new(), api_url, api_key, false)
    }

    /// [`with_client`] plus a keyless flag. When `keyless` is set and no key is
    /// present, requests are sent anonymously via the `x-tavily-access-mode:
    /// keyless` header instead of a bearer token.
    pub fn with_client_mode(
        client: Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        keyless: bool,
    ) -> Self {
        Self::with_clients_mode(client, HashMap::new(), api_url, api_key, keyless)
    }

    /// [`with_client_mode`] plus the per-key `{account}` proxy clients. Each
    /// rotating request looks up the client for the exact key it signs with,
    /// so one template lands every key on its own proxy account.
    pub fn with_clients_mode(
        client: Client,
        per_key_clients: HashMap<String, Client>,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        keyless: bool,
    ) -> Self {
        Self {
            client,
            per_key_clients: Arc::new(per_key_clients),
            api_url: api_url.into().trim_end_matches('/').to_string(),
            keys: Arc::new(KeyRing::parse(&api_key.into())),
            keyless,
        }
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        let raw = self
            .post(
                "search",
                &tavily_search_request_body(query, max_results, filters),
            )
            .await?;
        Ok(normalize_tavily_results(&raw))
    }

    pub async fn extract(&self, url: &str) -> Result<FetchedPage> {
        let raw = self
            .post("extract", &json!({ "urls": [url], "format": "markdown" }))
            .await?;
        parse_tavily_extract(&raw)
    }

    pub async fn map(&self, url: &str, max_results: usize) -> Result<Vec<Source>> {
        let raw = self
            .post("map", &tavily_map_request_body(url, max_results))
            .await?;
        Ok(limit_tavily_results(
            normalize_tavily_results(&raw),
            max_results,
        ))
    }

    /// POST with round-robin key selection: each request starts at the shared
    /// cursor's next key, and a key-scoped failure (401/403/429/432/433)
    /// retries once per remaining key. Network retries happen within the
    /// shared HTTP helper using the same key; other failures (5xx, parse)
    /// return without rotating keys.
    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let endpoint = format!("{}/{}", self.api_url, path.trim_start_matches('/'));
        if self.keyless && !self.keys.has_any_key() {
            return post_json_with_header_auth(
                &self.client,
                &endpoint,
                ("x-tavily-access-mode", "keyless"),
                body,
                "Tavily",
            )
            .await;
        }
        rotate_keys(&self.keys, "Tavily", |key| async move {
            // The proxy account is bound to the signing key: a rotated request
            // egresses through its own key's `{account}` client when one exists.
            let client = self.per_key_clients.get(&key).unwrap_or(&self.client);
            post_json_with_status(client, &endpoint, &key, body, "Tavily").await
        })
        .await
        .map_err(|failure| failure.error)
    }
}

pub fn tavily_search_request_body(
    query: &str,
    max_results: usize,
    filters: &SearchFilters,
) -> Value {
    #[derive(serde::Serialize)]
    struct TavilySearchBody<'a> {
        query: &'a str,
        max_results: usize,
        include_answer: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        days: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        topic: Option<&'static str>,
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        include_domains: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        exclude_domains: &'a [String],
    }

    let body = TavilySearchBody {
        query,
        max_results,
        include_answer: false,
        days: filters.recency_days,
        topic: filters.recency_days.map(|_| "news"),
        include_domains: filters.include_domains.as_slice(),
        exclude_domains: filters.exclude_domains.as_slice(),
    };

    serde_json::to_value(&body).expect("tavily search body must serialize")
}

pub fn tavily_map_request_body(url: &str, max_results: usize) -> Value {
    json!({
        "url": url,
        "max_depth": 1,
        "limit": max_results
    })
}

pub fn limit_tavily_results(mut sources: Vec<Source>, max_results: usize) -> Vec<Source> {
    sources.truncate(max_results);
    sources
}

/// Parse a Tavily extract response into content + metadata. The extract
/// endpoint returns `title` alongside `raw_content` (verified live; the docs'
/// sample response omits it) but no published-date field, so `published_date`
/// is always `None` here.
pub fn parse_tavily_extract(raw: &Value) -> Result<FetchedPage> {
    let result = raw
        .get("results")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let content = result
        .and_then(|item| item.get("raw_content").or_else(|| item.get("content")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty());

    let Some(content) = content else {
        return Err(NovaVeilSearchError::Provider(
            "Tavily extract returned empty content".to_string(),
        ));
    };
    let title = result
        .and_then(|item| item.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty());
    Ok(FetchedPage {
        content,
        title,
        published_date: None,
    })
}

/// Search results whose Tavily relevance `score` falls below this are junk,
/// not evidence. Long natural-language queries can drift Tavily onto one
/// generic word — observed live with "latest rmcp Rust MCP SDK release
/// version and what changed": dictionary and news-portal pages for "latest"
/// all scored ≤ 0.04, while on-topic results for answerable queries score
/// ≥ 0.49. 0.1 clears the junk band ~2.5x with ~5x headroom below on-topic
/// results. Items with no score (map results, API drift) always pass.
const MIN_SEARCH_SCORE: f64 = 0.1;

/// Normalize a Tavily `search`/`map` response into `Source`s. Search items
/// scoring below [`MIN_SEARCH_SCORE`] are dropped so keyword-drift junk never
/// reaches the enrichment/fallback source lists; score-less items (the map
/// endpoint returns bare URL strings) are kept unconditionally.
pub fn normalize_tavily_results(raw: &Value) -> Vec<Source> {
    raw.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if let Some(url) = item.as_str() {
                return Some(Source::new(url, "tavily"));
            }
            let url = item.get("url").and_then(Value::as_str)?;
            if item
                .get("score")
                .and_then(Value::as_f64)
                .is_some_and(|score| score < MIN_SEARCH_SCORE)
            {
                return None;
            }
            let mut source = Source::new(url, "tavily");
            if let Some(title) = item.get("title").and_then(Value::as_str) {
                source = source.with_title(title);
            }
            if let Some(description) = item
                .get("content")
                .or_else(|| item.get("description"))
                .and_then(Value::as_str)
            {
                source = source.with_description(description);
            }
            if let Some(published_date) = item.get("published_date").and_then(Value::as_str) {
                source = source.with_published_date(published_date);
            }
            Some(source)
        })
        .collect()
}

#[cfg(test)]
mod key_ring_tests {
    use super::*;

    #[test]
    fn rotation_cursor_is_shared_across_provider_clones() {
        let provider =
            TavilyProvider::with_client(Client::new(), "https://api.tavily.com", "tvly-a,tvly-b");
        let clone = provider.clone();
        // The cursor is shared (Arc): the clone continues the sequence rather
        // than restarting, regardless of the randomized starting offset.
        let a = provider.keys.start();
        assert_eq!(clone.keys.start(), (a + 1) % 2);
        assert_eq!(provider.keys.start(), (a + 2) % 2);
    }
}
