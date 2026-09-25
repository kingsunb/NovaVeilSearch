//! Keyless Bing search provider (no API key required).
//!
//! Scrapes Bing's public web SERP (`https://www.bing.com/search`) the way a
//! browser would, then parses the `<li class="b_algo">` organic-result blocks.
//! Bing answers a zero-result query with an unrelated cached SERP, so a query /
//! result token-overlap guard drops those pages instead of feeding garbage to
//! the model (upstream issue #38). Domain filters map onto `site:` operators,
//! which is the only scoping the public page exposes; recency maps onto Bing's
//! `qft=interval` buckets.

use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use reqwest::Client;

use crate::error::{NovaVeilSearchError, Result};
use crate::model::search::SearchFilters;
use crate::model::source::{FetchedPage, Source};

use super::http::{build_client, get_html};

const BING_ENDPOINT: &str = "https://www.bing.com/search";
/// Roughly the number of organic results Bing serves per page.
const PAGE_LEN: usize = 10;
const MAX_PAGES: usize = 3;
const MAX_RESULTS_CAP: usize = 30;
const DEFAULT_MARKET: &str = "en-US";

#[derive(Clone)]
pub struct BingProvider {
    client: Client,
    endpoint: String,
    market: String,
}

impl BingProvider {
    pub fn new(timeout: Duration) -> Self {
        Self::with_client(build_client(timeout), None)
    }

    pub fn with_client(client: Client, market: Option<String>) -> Self {
        Self {
            client,
            endpoint: BING_ENDPOINT.to_string(),
            market: market
                .filter(|market| !market.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_MARKET.to_string()),
        }
    }

    pub async fn search(
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
                // Bing paginates with a 1-based `first` cursor in steps of 10.
                params.push(("first", (1 + page * PAGE_LEN).to_string()));
            }
            let html = get_html(&self.client, &self.endpoint, &params, "Bing").await?;
            let parsed = parse_bing(&html);
            if parsed.is_empty() {
                break;
            }
            all.extend(parsed);
            if all.len() >= needed {
                break;
            }
        }

        // Zero-result queries return an unrelated cached SERP; discard it.
        if !all.is_empty() && !looks_relevant(query, &all) {
            return Ok(Vec::new());
        }
        Ok(all)
    }

    /// Bing has no public page-fetch (scrape) endpoint: search-only.
    pub async fn fetch(&self, url: &str) -> Result<FetchedPage> {
        Err(NovaVeilSearchError::Provider(format!(
            "Bing has no page-fetch endpoint (cannot fetch {url}); configure TAVILY_API_KEY, EXA_API_KEY, TINYFISH_API_KEY or FIRECRAWL_API_KEY for web_fetch"
        )))
    }

    pub async fn map(&self, _url: &str, _max_results: usize) -> Result<Vec<Source>> {
        Err(NovaVeilSearchError::Provider(
            "Bing has no site-map endpoint".to_string(),
        ))
    }

    fn base_params<'a>(
        &self,
        query: &'a str,
        filters: &'a SearchFilters,
    ) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("q", scoped_query(query, filters)),
            ("mkt", self.market.clone()),
        ];
        // SafeSearch off, mirroring the no-filter behavior of the keyless
        // engines so technical/adult-adjacent queries aren't silently censored.
        params.push(("adlt", "off".to_string()));
        if let Some(interval) = filters.recency_days.and_then(bing_time_interval) {
            params.push(("qft", format!("interval=\"{interval}\"")));
        }
        params
    }
}

/// Fold include/exclude domain filters into the query with `site:` /
/// `-site:` operators (Bing's public SERP has no dedicated domain parameter).
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

/// Map a recency window in days onto Bing's `qft=interval` buckets.
fn bing_time_interval(days: u32) -> Option<u32> {
    match days {
        0..=1 => Some(1),
        2..=7 => Some(7),
        8..=30 => Some(30),
        31..=90 => Some(90),
        91..=180 => Some(180),
        _ => Some(365),
    }
}

fn block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<li class="b_algo"[^>]*>.*?</li>"#).expect("valid b_algo regex")
    })
}

fn href_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<a[^>]*href="(https?://[^"]+)""#).expect("valid href regex"))
}

fn title_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<h2[^>]*>.*?<a[^>]*>(.*?)</a>.*?</h2>"#).expect("valid bing title regex")
    })
}

fn snippet_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<p[^>]*>(.*?)</p>"#).expect("valid bing snippet regex"))
}

/// Parse the `<li class="b_algo">` organic-result blocks out of a Bing SERP.
pub fn parse_bing(html: &str) -> Vec<Source> {
    block_regex()
        .find_iter(html)
        .filter_map(|block| {
            let block = block.as_str();
            let url = href_regex().captures(block)?.get(1)?.as_str();
            let url = super::http::decode_entities(url);
            if url.trim().is_empty() {
                return None;
            }
            let title = title_regex().captures(block).map(|cap| clean_text(&cap[1]));
            let snippet = snippet_regex()
                .captures(block)
                .map(|cap| clean_text(&cap[1]));
            let mut source = Source::new(url.trim().to_string(), "bing");
            if let Some(title) = title.filter(|title| !title.is_empty()) {
                source = source.with_title(title);
            }
            if let Some(snippet) = snippet.filter(|snippet| !snippet.is_empty()) {
                source = source.with_description(truncate_snippet(snippet));
            }
            Some(source)
        })
        .collect()
}

/// Query-token overlap guard. Bing returns an unrelated cached SERP for queries
/// with no results; requiring at least one query token to appear in some
/// result's title/snippet/url discards those pages. CJK queries use 1–2 char
/// bigrams, Latin/numeric queries use whole words of length ≥ 2.
fn looks_relevant(query: &str, sources: &[Source]) -> bool {
    let tokens = query_overlap_tokens(query);
    if tokens.is_empty() {
        return true; // pure-symbol query: cannot judge, do not block
    }
    sources.iter().any(|source| {
        let hay = format!(
            "{} {} {}",
            source.title.as_deref().unwrap_or_default(),
            source.description.as_deref().unwrap_or_default(),
            source.url
        )
        .to_ascii_lowercase();
        tokens.iter().any(|token| hay.contains(token))
    })
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch) || ('\u{3040}'..='\u{30ff}').contains(&ch)
}

fn query_overlap_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();

    // CJK/kana runs: keep 1–2 char runs whole, then add bigrams within each run.
    let flush = |run: &mut String, tokens: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        if run.chars().count() <= 2 {
            tokens.push(run.clone());
        }
        for pair in run.chars().collect::<Vec<_>>().windows(2) {
            tokens.push(pair.iter().collect());
        }
        run.clear();
    };

    let mut run = String::new();
    for ch in query.chars() {
        if is_cjk(ch) {
            run.push(ch);
        } else {
            flush(&mut run, &mut tokens);
        }
    }
    flush(&mut run, &mut tokens);

    // Latin / numeric words of length >= 2.
    for word in query
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
    {
        if word.chars().count() >= 2 {
            tokens.push(word.to_string());
        }
    }
    tokens.dedup();
    tokens
}

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
    fn parses_bing_blocks() {
        let html = r#"
        <li class="b_algo" data-id="1">
            <h2><a href="https://example.com/a">Example <b>Title</b></a></h2>
            <p>The first &amp; best snippet.</p>
        </li>
        <li class="b_algo" data-id="2">
            <h2><a href="https://example.org/b">Second Title</a></h2>
        </li>
        "#;
        let sources = parse_bing(html);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].url, "https://example.com/a");
        assert_eq!(sources[0].title.as_deref(), Some("Example Title"));
        assert_eq!(
            sources[0].description.as_deref(),
            Some("The first & best snippet.")
        );
        assert_eq!(sources[1].url, "https://example.org/b");
        assert_eq!(sources[1].description, None);
    }

    #[test]
    fn relevance_guard_uses_query_tokens() {
        let sources = vec![Source::new("https://lorem.com", "bing")
            .with_title("unrelated placid lake")
            .with_description("nothing here")];
        assert!(!looks_relevant("rust async tokio", &sources));
        assert!(looks_relevant("placid", &sources));
    }

    #[test]
    fn cjk_bigram_tokens_match() {
        let sources = vec![Source::new("https://a.b", "bing").with_title("人工智能")];
        assert!(looks_relevant("人工智能", &sources));
    }
}
