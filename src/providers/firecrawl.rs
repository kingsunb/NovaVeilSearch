use crate::error::{NovaVeilSearchError, Result};
use crate::model::source::{FetchedPage, Source};
use crate::providers::http::{build_client, post_json_optional_auth, post_json_with_status, rotate_keys};
use crate::providers::keyring::KeyRing;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct FirecrawlProvider {
    client: Client,
    /// Per-key `{account}` proxy clients (exact key → client): the request
    /// egresses through the proxy account derived from whichever key in the
    /// ring signs it. Empty when no keyed proxy template names `{account}`.
    per_key_clients: Arc<HashMap<String, Client>>,
    api_url: String,
    keys: Arc<KeyRing>,
    /// Keyless anonymous mode: send no `Authorization` header (Firecrawl's
    /// hosted `/v2` search/scrape works anonymously, rate-limited).
    keyless: bool,
}

impl FirecrawlProvider {
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>, timeout: Duration) -> Self {
        Self::with_client(build_client(timeout), api_url, api_key)
    }

    /// Construct with an externally provided `reqwest::Client`. Used by
    /// `SearchService::new` to share one tuned client across providers.
    ///
    /// `api_key` accepts a single key or a comma-separated list; multiple
    /// keys rotate round-robin per request with automatic failover on
    /// key-scoped errors (401/403/429/432/433).
    pub fn with_client(
        client: Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self::with_clients_mode(client, HashMap::new(), api_url, api_key, false)
    }

    /// [`with_client`] plus a keyless flag. Keyless + no key = no auth
    /// header at all.
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

    pub async fn search(&self, query: &str, max_results: usize) -> Result<Vec<Source>> {
        let raw = self
            .post("search", &json!({ "query": query, "limit": max_results }))
            .await?;
        Ok(normalize_firecrawl_results(&raw))
    }

    pub async fn scrape(&self, url: &str) -> Result<FetchedPage> {
        let raw = self
            .post("scrape", &json!({ "url": url, "formats": ["markdown"] }))
            .await?;
        parse_firecrawl_scrape(&raw)
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let endpoint = format!("{}/{}", self.api_url, path.trim_start_matches('/'));
        if !self.keys.has_any_key() {
            // No usable key (keyless anonymous mode, or an empty key value):
            // send no `Authorization` header at all. A present key always
            // wins over the keyless flag, exactly as before.
            return post_json_optional_auth(&self.client, &endpoint, None, body, "Firecrawl").await;
        }
        rotate_keys(&self.keys, "Firecrawl", |key| async move {
            let client = self.per_key_clients.get(&key).unwrap_or(&self.client);
            post_json_with_status(client, &endpoint, &key, body, "Firecrawl").await
        })
        .await
        .map_err(|failure| failure.error)
    }
}

/// Parse a Firecrawl scrape response into content + metadata. Scrape responses
/// carry a rich `metadata` object (`title`, `publishedTime`,
/// `article:published_time`, OG tags, …) next to the markdown; both the
/// wrapped (`data.markdown`) and flat (`markdown`) response shapes are
/// accepted, mirroring the content lookup.
pub fn parse_firecrawl_scrape(raw: &Value) -> Result<FetchedPage> {
    let content = raw
        .get("data")
        .and_then(|data| data.get("markdown").or_else(|| data.get("content")))
        .or_else(|| raw.get("markdown"))
        .or_else(|| raw.get("content"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty());

    let Some(content) = content else {
        return Err(NovaVeilSearchError::Provider(
            "Firecrawl scrape returned empty content".to_string(),
        ));
    };
    let metadata = raw
        .get("data")
        .and_then(|data| data.get("metadata"))
        .or_else(|| raw.get("metadata"));
    let non_empty = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|text| !text.trim().is_empty())
    };
    let title = non_empty(metadata.and_then(|m| m.get("title")));
    let published_date = non_empty(metadata.and_then(|m| {
        m.get("publishedTime")
            .or_else(|| m.get("article:published_time"))
    }));
    Ok(FetchedPage {
        content,
        title,
        published_date,
    })
}

pub fn normalize_firecrawl_results(raw: &Value) -> Vec<Source> {
    // v2 groups search results by source type; this provider consumes web
    // results only. Keep accepting v1's data array and flat gateway results.
    raw.pointer("/data/web")
        .or_else(|| raw.get("data"))
        .or_else(|| raw.get("results"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if let Some(url) = item.as_str() {
                return Some(Source::new(url, "firecrawl"));
            }
            let url = item.get("url").and_then(Value::as_str)?;
            let mut source = Source::new(url, "firecrawl");
            if let Some(title) = item.get("title").and_then(Value::as_str) {
                source = source.with_title(title);
            }
            if let Some(description) = item
                .get("description")
                .or_else(|| item.get("markdown"))
                .or_else(|| item.get("content"))
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
