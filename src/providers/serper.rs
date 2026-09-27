//! Keyed Serper (Google SERP) search provider.
//!
//! Serper's `/search` endpoint wraps Google's organic results in a JSON API
//! authenticated by an `X-API-KEY` header. Unlike the keyless HTML scrapers
//! (DuckDuckGo/Bing) it has a real free tier, but it still requires an API
//! key, so it slots alongside the other keyed providers. Search-only: Serper
//! has no page-fetch or site-map endpoint, so `web_fetch` / `web_map` fall
//! through to fetch-capable providers. Domain filters become `site:` /
//! `-site:` operators in the query; recency becomes a `tbs:qdr:…` bucket.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::error::NovaVeilSearchError;
use crate::error::Result;
use crate::model::search::SearchFilters;
use crate::model::source::{FetchedPage, Source};
use crate::providers::http::{build_client, post_json_with_header_auth_status, rotate_keys};
use crate::providers::keyring::KeyRing;

/// Serper auth header — `X-API-KEY`, not a bearer token.
const AUTH_HEADER: &str = "X-API-KEY";
/// Serper caps `num` at 100 results per request.
const MAX_NUM_RESULTS: usize = 100;

#[derive(Clone)]
pub struct SerperProvider {
    client: Client,
    /// Per-key `{account}` proxy clients (exact key → client): the request
    /// egresses through the proxy account derived from whichever key in the
    /// ring signs it. Empty when no keyed proxy template names `{account}`;
    /// every request then uses `client`.
    per_key_clients: Arc<HashMap<String, Client>>,
    api_url: String,
    keys: Arc<KeyRing>,
}

impl SerperProvider {
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
        Self::with_clients(client, HashMap::new(), api_url, api_key)
    }

    /// [`with_client`] plus the per-key `{account}` proxy clients. Each
    /// rotating request looks up the client for the exact key it signs with,
    /// so one template lands every key on its own proxy account.
    pub fn with_clients(
        client: Client,
        per_key_clients: HashMap<String, Client>,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client,
            per_key_clients: Arc::new(per_key_clients),
            api_url: api_url.into().trim_end_matches('/').to_string(),
            keys: Arc::new(KeyRing::parse(&api_key.into())),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/search", self.api_url)
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        let body = serper_search_request_body(query, max_results, filters);
        let endpoint = self.endpoint();
        let raw = rotate_keys(&self.keys, "Serper", |key| {
            let endpoint = endpoint.clone();
            let body = body.clone();
            async move {
                let client = self.per_key_clients.get(&key).unwrap_or(&self.client);
                post_json_with_header_auth_status(
                    client,
                    &endpoint,
                    (AUTH_HEADER, &key),
                    &body,
                    "Serper",
                )
                .await
            }
        })
        .await
        .map_err(|failure| failure.error)?;
        Ok(normalize_serper_results(&raw))
    }

    /// No page-fetch endpoint: Serper is search-only. `web_fetch` falls
    /// through to fetch-capable providers instead.
    pub async fn fetch(&self, url: &str) -> Result<FetchedPage> {
        Err(NovaVeilSearchError::Provider(format!(
            "Serper has no page-fetch endpoint (cannot fetch {url}); configure TAVILY_API_KEY, EXA_API_KEY, TINYFISH_API_KEY or FIRECRAWL_API_KEY for web_fetch"
        )))
    }

    pub async fn map(&self, _url: &str, _max_results: usize) -> Result<Vec<Source>> {
        Err(NovaVeilSearchError::Provider(
            "Serper has no site-map endpoint".to_string(),
        ))
    }
}

pub fn serper_search_request_body(
    query: &str,
    max_results: usize,
    filters: &SearchFilters,
) -> Value {
    let q = scoped_query(query, filters);
    let mut body = json!({
        "q": q,
        "num": max_results.clamp(1, MAX_NUM_RESULTS),
    });
    if let Some(tbs) = filters.recency_days.and_then(serper_tbs_param) {
        body["tbs"] = json!(tbs);
    }
    body
}

/// Fold include/exclude domain filters into the query as `site:` /
/// `-site:` operators — the only scoping primitive Serper exposes.
fn scoped_query(query: &str, filters: &SearchFilters) -> String {
    let mut terms: Vec<String> = Vec::new();
    terms.push(query.trim().to_string());
    for domain in &filters.include_domains {
        terms.push(format!("site:{domain}"));
    }
    for domain in &filters.exclude_domains {
        terms.push(format!("-site:{domain}"));
    }
    terms.join(" ")
}

/// Map a recency window in days onto Google/Serper's coarse `tbs` buckets.
fn serper_tbs_param(days: u32) -> Option<String> {
    let bucket = match days {
        0..=1 => "d",
        2..=7 => "w",
        8..=31 => "m",
        _ => "y",
    };
    Some(format!("qdr:{bucket}"))
}

/// Normalize a Serper `search` response into `Source`s from its `organic`
/// array. `link` is the URL, `snippet` the description, and `date` (or
/// `publishedDate` where present) an approximate published date. Items without
/// a `link` are dropped.
pub fn normalize_serper_results(raw: &Value) -> Vec<Source> {
    raw.get("organic")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let url = item.get("link").and_then(Value::as_str)?;
            if url.trim().is_empty() {
                return None;
            }
            let mut source = Source::new(url, "serper");
            if let Some(title) = item.get("title").and_then(Value::as_str) {
                if !title.trim().is_empty() {
                    source = source.with_title(title);
                }
            }
            if let Some(snippet) = item.get("snippet").and_then(Value::as_str) {
                if !snippet.trim().is_empty() {
                    source = source.with_description(snippet);
                }
            }
            if let Some(date) = item.get("date").and_then(Value::as_str) {
                source = source.with_published_date(date);
            } else if let Some(date) = item.get("publishedDate").and_then(Value::as_str) {
                source = source.with_published_date(date);
            }
            Some(source)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_organic_results() {
        let raw = json!({
            "organic": [
                {
                    "position": 1,
                    "title": "Rust Programming Language",
                    "link": "https://www.rust-lang.org/",
                    "snippet": "A language empowering everyone to build reliable and efficient software.",
                    "date": "2024-01-15",
                    "domain": "rust-lang.org"
                },
                {
                    "position": 2,
                    "title": "The rustup book",
                    "link": "https://rust-lang.github.io/rustup/",
                    "snippet": null
                },
                { "position": 3, "link": "https://example.com/no-title", "snippet": "" }
            ]
        });
        let sources = normalize_serper_results(&raw);
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[0].url, "https://www.rust-lang.org/");
        assert_eq!(sources[0].provider, "serper");
        assert_eq!(
            sources[0].title.as_deref(),
            Some("Rust Programming Language")
        );
        assert_eq!(
            sources[0].description.as_deref(),
            Some("A language empowering everyone to build reliable and efficient software.")
        );
        assert_eq!(sources[0].published_date.as_deref(), Some("2024-01-15"));
        assert_eq!(sources[1].title.as_deref(), Some("The rustup book"));
        assert_eq!(sources[1].description, None);
        assert_eq!(sources[2].title, None);
    }

    #[test]
    fn empty_or_missing_organic_is_no_results() {
        assert!(normalize_serper_results(&json!({})).is_empty());
        assert!(normalize_serper_results(&json!({ "organic": [] })).is_empty());
        assert!(normalize_serper_results(&json!({ "organic": [{"title": "no link"}] })).is_empty());
    }

    #[test]
    fn scoped_query_appends_domain_operators() {
        let filters = SearchFilters {
            recency_days: None,
            include_domains: vec!["example.com".to_string()],
            exclude_domains: vec!["spam.net".to_string()],
        };
        assert_eq!(
            scoped_query("rust mcp", &filters),
            "rust mcp site:example.com -site:spam.net"
        );
    }

    #[test]
    fn tbs_param_buckets_recency() {
        assert_eq!(serper_tbs_param(1).as_deref(), Some("qdr:d"));
        assert_eq!(serper_tbs_param(7).as_deref(), Some("qdr:w"));
        assert_eq!(serper_tbs_param(31).as_deref(), Some("qdr:m"));
        assert_eq!(serper_tbs_param(365).as_deref(), Some("qdr:y"));
        assert_eq!(serper_tbs_param(0).as_deref(), Some("qdr:d"));
    }
}
