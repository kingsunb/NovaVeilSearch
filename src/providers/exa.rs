//! Exa semantic search provider.
//!
//! Exa is an embeddings-first engine: strong on descriptive queries, papers,
//! official domains, and low-noise discovery, with native support for the
//! whole `SearchFilters` contract (domain include/exclude lists and a
//! published-date lower bound). Fetch goes through `/contents`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{json, Value};

use crate::error::{NovaVeilSearchError, Result};
use crate::model::search::SearchFilters;
use crate::model::source::{FetchedPage, Source};

use super::http::{build_client, post_json_with_header_auth, post_raw_json};

/// Exa's canonical auth channel. The docs also describe `Authorization:
/// Bearer` as accepted, but every reference example uses this header — stick
/// to the primary documented path rather than the alternate one.
const AUTH_HEADER: &str = "x-api-key";

/// Exa's public `numResults` ceiling.
const MAX_NUM_RESULTS: usize = 100;

/// Exa's hosted public MCP endpoint. Anonymous (rate-limited) when no key is
/// supplied, which is how keyless mode reaches Exa.
const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";

#[derive(Clone)]
pub struct ExaProvider {
    client: Client,
    api_url: String,
    api_key: String,
    /// Keyless anonymous mode: route search through Exa's public MCP
    /// (`web_search_exa`) with no API key.
    keyless: bool,
    mcp_url: String,
}

impl ExaProvider {
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>, timeout: Duration) -> Self {
        Self::with_client(build_client(timeout), api_url, api_key)
    }

    /// Construct with an externally provided `reqwest::Client`. Used by
    /// `SearchService` to share one tuned client across providers.
    pub fn with_client(
        client: Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self::with_client_mode(client, api_url, api_key, false)
    }

    /// [`with_client`] plus a keyless flag. Keyless + empty key = Exa MCP
    /// anonymous search; a present key always takes the keyed REST path.
    pub fn with_client_mode(
        client: Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
        keyless: bool,
    ) -> Self {
        Self {
            client,
            api_url: api_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            keyless,
            mcp_url: EXA_MCP_URL.to_string(),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.api_url, path.trim_start_matches('/'))
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        if self.keyless && self.api_key.is_empty() {
            if !filters.is_empty() {
                return Err(NovaVeilSearchError::Provider(
                    "Exa keyless (MCP) mode cannot honor domain/recency filters; set EXA_API_KEY for filtered Exa search".to_string(),
                ));
            }
            return self.search_exa_mcp(query, max_results).await;
        }

        let body = exa_search_request_body(query, max_results, filters, now_unix_seconds());
        let raw = post_json_with_header_auth(
            &self.client,
            &self.endpoint("search"),
            (AUTH_HEADER, &self.api_key),
            &body,
            "Exa",
        )
        .await?;
        Ok(normalize_exa_results(&raw))
    }

    pub async fn fetch(&self, url: &str) -> Result<FetchedPage> {
        if self.keyless && self.api_key.is_empty() {
            return Err(NovaVeilSearchError::Provider(
                "Exa keyless (MCP) mode supports search only; set EXA_API_KEY for Exa page fetch"
                    .to_string(),
            ));
        }
        let raw = post_json_with_header_auth(
            &self.client,
            &self.endpoint("contents"),
            (AUTH_HEADER, &self.api_key),
            &json!({ "urls": [url], "text": true }),
            "Exa",
        )
        .await?;
        parse_exa_contents(&raw, url)
    }

    /// Anonymous search via Exa's hosted MCP `web_search_exa` tool. The MCP
    /// endpoint answers JSON-RPC over SSE; the response carries an array of
    /// text blocks formatted as `Title: / URL: / Published: / Highlights:`.
    async fn search_exa_mcp(&self, query: &str, max_results: usize) -> Result<Vec<Source>> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": { "query": query, "numResults": max_results.clamp(1, MAX_NUM_RESULTS) }
            }
        });
        let raw = post_raw_json(
            &self.client,
            &self.mcp_url,
            &[
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
            &body,
            "Exa MCP",
        )
        .await?;
        parse_exa_mcp(&raw)
    }
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Search body without contents options: results carry metadata only
/// (title/url/publishedDate), keeping per-call cost at the base search rate —
/// inline page text comes from the shared enrichment pipeline, not from Exa.
pub fn exa_search_request_body(
    query: &str,
    max_results: usize,
    filters: &SearchFilters,
    now_unix: u64,
) -> Value {
    let mut body = json!({
        "query": query,
        "numResults": max_results.clamp(1, MAX_NUM_RESULTS),
    });
    let object = body.as_object_mut().expect("literal object");
    if !filters.include_domains.is_empty() {
        object.insert("includeDomains".into(), json!(filters.include_domains));
    }
    if !filters.exclude_domains.is_empty() {
        object.insert("excludeDomains".into(), json!(filters.exclude_domains));
    }
    if let Some(days) = filters.recency_days {
        object.insert(
            "startPublishedDate".into(),
            json!(start_published_date(days, now_unix)),
        );
    }
    body
}

/// `recency_days` → ISO-8601 lower bound for Exa's `startPublishedDate`:
/// midnight UTC `days` ago.
pub fn start_published_date(days: u32, now_unix: u64) -> String {
    let day_index = (now_unix / 86_400) as i64 - i64::from(days);
    let (year, month, day) = civil_from_days(day_index);
    format!("{year:04}-{month:02}-{day:02}T00:00:00.000Z")
}

/// Days-since-epoch → proleptic-Gregorian (year, month, day), after Howard
/// Hinnant's `civil_from_days`. One date subtraction does not justify a
/// calendar dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub fn normalize_exa_results(raw: &Value) -> Vec<Source> {
    raw.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let url = item.get("url").and_then(Value::as_str)?;
            let mut source = Source::new(url, "exa");
            if let Some(title) = item.get("title").and_then(Value::as_str) {
                source = source.with_title(title);
            }
            if let Some(summary) = item.get("summary").and_then(Value::as_str) {
                source = source.with_description(summary);
            }
            if let Some(date) = item.get("publishedDate").and_then(Value::as_str) {
                source = source.with_published_date(date);
            }
            Some(source)
        })
        .collect()
}

/// `/contents` reports per-URL failures in `statuses[]` beside a 200
/// response; an empty result set is inspected for that reason before the
/// generic "no content" verdict.
pub fn parse_exa_contents(raw: &Value, url: &str) -> Result<FetchedPage> {
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
                published_date: result
                    .get("publishedDate")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    let detail = raw
        .get("statuses")
        .and_then(Value::as_array)
        .and_then(|statuses| statuses.first())
        .and_then(|status| {
            status
                .get("error")
                .and_then(|error| error.get("tag"))
                .or_else(|| status.get("status"))
        })
        .and_then(Value::as_str)
        .unwrap_or("no content returned");
    Err(NovaVeilSearchError::Provider(format!(
        "Exa contents failed for {url}: {detail}"
    )))
}

/// Parse an Exa MCP JSON-RPC response. The transport may hand back plain JSON
/// or an SSE framing (`data: {...}` lines); both are accepted, and the first
/// JSON value that looks like a JSON-RPC reply is used.
pub fn parse_exa_mcp(raw: &str) -> Result<Vec<Source>> {
    let mut reply: Option<Value> = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        let data = trimmed
            .strip_prefix("data:")
            .map(str::trim)
            .unwrap_or(trimmed);
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            if value.get("result").is_some()
                || value.get("error").is_some()
                || value.get("id").is_some()
            {
                reply = Some(value);
                break;
            }
        }
    }

    let Some(reply) = reply else {
        return Err(NovaVeilSearchError::Parse(
            "Exa MCP returned no JSON-RPC reply".to_string(),
        ));
    };
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(NovaVeilSearchError::Provider(format!(
            "Exa MCP error: {message}"
        )));
    }
    let content = reply.pointer("/result/content").and_then(Value::as_array);
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    let text_blocks: Vec<&str> = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    Ok(parse_exa_mcp_text(&text_blocks.join("\n")))
}

/// Parse the `web_search_exa` text output: blocks of `Title:` / `URL:` /
/// `Published:` / `Highlights:` lines.
pub fn parse_exa_mcp_text(text: &str) -> Vec<Source> {
    let mut sources = Vec::new();
    let mut lines = text.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed_start = line.trim_start();
        if !trimmed_start.starts_with("Title:") {
            continue;
        }
        let title = trimmed_start
            .strip_prefix("Title:")
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);

        let mut url: Option<String> = None;
        let mut published: Option<String> = None;
        let mut highlights: Vec<String> = Vec::new();
        let mut in_highlights = false;

        while let Some(next) = lines.peek() {
            if next.trim_start().starts_with("Title:") {
                break;
            }
            let next = lines.next().unwrap_or_default();
            let trimmed = next.trim();
            if let Some(value) = trimmed.strip_prefix("URL:") {
                url = Some(value.trim().to_string());
            } else if let Some(value) = trimmed.strip_prefix("Published:") {
                published = Some(value.trim().to_string());
            } else if trimmed.starts_with("Highlights:") {
                in_highlights = true;
            } else if in_highlights {
                let highlight = trimmed.trim_start_matches(['-', ' ']).trim();
                if !highlight.is_empty() && !highlight.starts_with("...") && highlights.len() < 3 {
                    highlights.push(highlight.to_string());
                }
            }
        }

        let Some(url) = url.filter(|url| !url.is_empty()) else {
            continue;
        };
        let mut source = Source::new(url, "exa");
        if let Some(title) = title {
            source = source.with_title(title);
        }
        if !highlights.is_empty() {
            let snippet: String = highlights.join(" ").chars().take(300).collect();
            source = source.with_description(snippet);
        }
        if let Some(published) = published {
            let date_prefix = published
                .chars()
                .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
                .collect::<String>();
            if date_prefix.len() >= 10 {
                source = source.with_published_date(date_prefix);
            }
        }
        sources.push(source);
    }
    sources
}
