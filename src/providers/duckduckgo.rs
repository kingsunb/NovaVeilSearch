//! Keyless DuckDuckGo search provider (no API key required).
//!
//! Scrapes DuckDuckGo's public HTML and Lite endpoints — the same pages an
//! ordinary browser renders — so the server can answer zero-config. Results
//! are best-effort: the engine may rate-limit or A/B-test its markup, at which
//! point the provider yields an error and the source chain falls through to
//! the next engine. Supplied as a supplemental source rather than a primary,
//! and it honors domain/recency filters via `site:` / `-site:` operators and
//! the `df` time parameter.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use reqwest::Client;

use crate::error::{NovaVeilSearchError, Result};
use crate::model::search::SearchFilters;
use crate::model::source::{FetchedPage, Source};

use super::http::{build_client, get_html};

const HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const LITE_ENDPOINT: &str = "https://lite.duckduckgo.com/lite/";
/// Results returned per DuckDuckGo HTML page.
const PAGE_LEN: usize = 30;
/// Hard cap on pages fetched for one query (politeness + latency).
const MAX_PAGES: usize = 3;
/// Absolute ceiling on results across pages.
const MAX_RESULTS_CAP: usize = 60;

#[derive(Clone)]
pub struct DuckduckgoProvider {
    client: Client,
    html_endpoint: String,
    lite_endpoint: String,
    /// Optional `kl` (region/ad) parameter, e.g. `us-en`, `cn-zh`, `wt-wt`.
    region: Option<String>,
}

impl DuckduckgoProvider {
    pub fn new(timeout: Duration) -> Self {
        Self::with_client(build_client(timeout), None)
    }

    /// Construct with an externally provided `reqwest::Client` and an optional
    /// region (`kl`) override. The endpoint URLs are fixed for keyless scraping
    /// but parameterized for tests.
    pub fn with_client(client: Client, region: Option<String>) -> Self {
        Self {
            client,
            html_endpoint: HTML_ENDPOINT.to_string(),
            lite_endpoint: LITE_ENDPOINT.to_string(),
            region,
        }
    }

    pub async fn search(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        let mut sources = match self.search_html(query, max_results, filters).await {
            Ok(sources) if !sources.is_empty() => sources,
            _ => self.search_lite(query, max_results, filters).await?,
        };
        sources.truncate(max_results.max(1));
        Ok(sources)
    }

    /// No page-fetch endpoint: keyless DuckDuckGo is search-only. `web_fetch`
    /// falls through to fetch-capable providers instead.
    pub async fn fetch(&self, url: &str) -> Result<FetchedPage> {
        Err(NovaVeilSearchError::Provider(format!(
            "DuckDuckGo has no page-fetch endpoint (cannot fetch {url}); configure TAVILY_API_KEY, EXA_API_KEY, TINYFISH_API_KEY or FIRECRAWL_API_KEY for web_fetch"
        )))
    }

    pub async fn map(&self, _url: &str, _max_results: usize) -> Result<Vec<Source>> {
        Err(NovaVeilSearchError::Provider(
            "DuckDuckGo has no site-map endpoint".to_string(),
        ))
    }

    async fn search_html(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        let needed = max_results.clamp(1, MAX_RESULTS_CAP);
        let pages = needed.div_ceil(PAGE_LEN).clamp(1, MAX_PAGES);
        let mut all = Vec::new();

        for page in 0..pages {
            let mut params = self.base_params(query, filters);
            if page > 0 {
                params.push(("s", (page * PAGE_LEN).to_string()));
            }
            let html = get_html(&self.client, &self.html_endpoint, &params, "DuckDuckGo").await?;
            let parsed = parse_ddg_html(&html);
            if parsed.is_empty() {
                break;
            }
            all.extend(parsed);
            if all.len() >= needed {
                break;
            }
        }
        Ok(all)
    }

    async fn search_lite(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        let needed = max_results.clamp(1, MAX_RESULTS_CAP);
        let pages = needed.div_ceil(PAGE_LEN).clamp(1, MAX_PAGES);
        let mut all = Vec::new();

        for page in 0..pages {
            let mut params = self.base_params(query, filters);
            if page > 0 {
                params.push(("s", (page * PAGE_LEN).to_string()));
            }
            let html = get_html(
                &self.client,
                &self.lite_endpoint,
                &params,
                "DuckDuckGo Lite",
            )
            .await?;
            let parsed = parse_ddg_lite(&html);
            if parsed.is_empty() {
                break;
            }
            all.extend(parsed);
            if all.len() >= needed {
                break;
            }
        }
        Ok(all)
    }

    fn base_params<'a>(
        &self,
        query: &'a str,
        filters: &'a SearchFilters,
    ) -> Vec<(&'static str, String)> {
        let mut params = vec![("q", scoped_query(query, filters))];
        // adlt=-1 disables safe-search so adult-adjacent technical queries
        // still return results; DDG lite/lts have no adult filter anyway.
        params.push(("adlt", "-1".to_string()));
        if let Some(region) = &self.region {
            params.push(("kl", region.clone()));
        }
        if let Some(param) = filters.recency_days.and_then(ddg_time_param) {
            params.push(("df", param.to_string()));
        }
        params
    }
}

/// Fold include/exclude domain filters into the query as `site:` /
/// `-site:` operators — the only scoping primitive the HTML endpoints expose.
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

/// Map a recency window in days onto DuckDuckGo's coarse `df` buckets.
fn ddg_time_param(days: u32) -> Option<&'static str> {
    match days {
        0..=1 => Some("d"),
        2..=7 => Some("w"),
        8..=31 => Some("m"),
        _ => Some("y"),
    }
}

fn result_a_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("valid result__a regex")
    })
}

fn snippet_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#)
            .expect("valid result__snippet regex")
    })
}

fn lite_link_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<a[^>]*class=['"]result-link['"][^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("valid lite result-link regex")
    })
}

fn lite_snippet_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)class=['"]result-snippet['"][^>]*>([\s\S]*?)</td>"#)
            .expect("valid lite result-snippet regex")
    })
}

/// Parse the full-fat HTML endpoint. Titles and snippets are matched by two
/// independent patterns and paired by index; a result missing its snippet keeps
/// a null description rather than borrowing a neighbor's.
pub fn parse_ddg_html(html: &str) -> Vec<Source> {
    let titles: Vec<(String, String)> = result_a_regex()
        .captures_iter(html)
        .map(|cap| (cap[1].to_string(), cap[2].to_string()))
        .collect();
    let snippets: Vec<String> = snippet_regex()
        .captures_iter(html)
        .map(|cap| clean_text(&cap[1]))
        .collect();

    titles
        .into_iter()
        .enumerate()
        .filter_map(|(index, (raw_url, raw_title))| {
            let url = extract_ddg_url(&raw_url)?;
            let title = clean_text(&raw_title);
            if title.is_empty() {
                return None;
            }
            let mut source = Source::new(url, "duckduckgo").with_title(title);
            if let Some(snippet) = snippets
                .get(index)
                .filter(|snippet| !snippet.is_empty())
                .cloned()
            {
                source = source.with_description(truncate_snippet(snippet));
            }
            Some(source)
        })
        .collect()
}

pub fn parse_ddg_lite(html: &str) -> Vec<Source> {
    let titles: Vec<(String, String)> = lite_link_regex()
        .captures_iter(html)
        .map(|cap| (cap[1].to_string(), cap[2].to_string()))
        .collect();
    let snippets: Vec<String> = lite_snippet_regex()
        .captures_iter(html)
        .map(|cap| clean_text(&cap[1]))
        .collect();

    titles
        .into_iter()
        .enumerate()
        .filter_map(|(index, (raw_url, raw_title))| {
            let url = extract_ddg_url(&raw_url)?;
            let title = clean_text(&raw_title);
            if title.is_empty() {
                return None;
            }
            let mut source = Source::new(url, "duckduckgo").with_title(title);
            if let Some(snippet) = snippets
                .get(index)
                .filter(|snippet| !snippet.is_empty())
                .cloned()
            {
                source = source.with_description(truncate_snippet(snippet));
            }
            Some(source)
        })
        .collect()
}

/// Resolve a DuckDuckGo result href to its destination URL. Result links carry
/// a redirect inside `//duckduckgo.com/l/?uddg=<percent-encoded target>&…`; the
/// target is decoded and kept only when it looks like a web URL.
fn extract_ddg_url(raw: &str) -> Option<String> {
    let raw = super::http::decode_entities(raw);
    let target = if let Some(pos) = raw.find("uddg=") {
        let after = &raw[pos + "uddg=".len()..];
        let value = after.split('&').next().unwrap_or(after);
        percent_encoding::percent_decode_str(value)
            .decode_utf8_lossy()
            .into_owned()
    } else if let Some(stripped) = raw.strip_prefix("//") {
        format!("https:{stripped}")
    } else {
        raw
    };
    let target = target.trim();
    if target.starts_with("https://") || target.starts_with("http://") {
        Some(target.to_string())
    } else {
        None
    }
}

/// Strip markup, decode entities, and collapse whitespace in one pass.
fn clean_text(fragment: &str) -> String {
    super::http::squash_whitespace(&super::http::decode_entities(&super::http::strip_tags(
        fragment,
    )))
}

fn truncate_snippet(snippet: String) -> String {
    snippet.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_html_results() {
        let html = r#"
            <div class="result results_links results_links_deep web-result">
              <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">Example <b>Page</b></a>
              <a class="result__snippet" href="https://example.com">This is the &lt;snippet&gt; text.</a>
            </div>
        "#;
        let sources = parse_ddg_html(html);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "https://example.com/page");
        assert_eq!(sources[0].title.as_deref(), Some("Example Page"));
        assert_eq!(
            sources[0].description.as_deref(),
            Some("This is the <snippet> text.")
        );
    }

    #[test]
    fn ignores_non_result_links() {
        let html = r#"<a class="result__a-other" href="//nope">ignored</a>"#;
        assert!(parse_ddg_html(html).is_empty());
    }

    #[test]
    fn recency_maps_to_buckets() {
        assert_eq!(ddg_time_param(0), Some("d"));
        assert_eq!(ddg_time_param(7), Some("w"));
        assert_eq!(ddg_time_param(30), Some("m"));
        assert_eq!(ddg_time_param(365), Some("y"));
    }
}
