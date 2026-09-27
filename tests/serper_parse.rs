use nova_veil_search::model::search::SearchFilters;
use nova_veil_search::providers::serper::{normalize_serper_results, serper_search_request_body};
use serde_json::json;

#[test]
fn organic_results_parse_title_snippet_link_and_date() {
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
                "publishedDate": "2023-03-01"
            },
            {
                "position": 3,
                "title": "no link, dropped",
                "snippet": "orphan"
            }
        ]
    });
    let sources = normalize_serper_results(&raw);
    assert_eq!(sources.len(), 2);
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
    assert_eq!(sources[1].published_date.as_deref(), Some("2023-03-01"));
}

#[test]
fn empty_or_missing_organic_is_no_results() {
    assert!(normalize_serper_results(&json!({})).is_empty());
    assert!(normalize_serper_results(&json!({ "organic": [] })).is_empty());
    assert!(normalize_serper_results(&json!({ "organic": [ { "title": "no link" } ] })).is_empty());
}

// Serper has no dedicated domain-filter params: include/exclude domains are
// folded into `q` as `site:` / `-site:` operators, matching Google's syntax.
#[test]
fn domain_filters_fold_into_query_as_site_operators() {
    let filters = SearchFilters {
        recency_days: None,
        include_domains: vec!["docs.rs".to_string()],
        exclude_domains: vec!["pinterest.com".to_string()],
    };
    let body = serper_search_request_body("rust http client", 10, &filters);
    assert_eq!(
        body["q"],
        json!("rust http client site:docs.rs -site:pinterest.com")
    );
    assert_eq!(body["num"], json!(10));
    assert!(body.get("tbs").is_none());
}

#[test]
fn recency_days_map_to_tbs_qdr_buckets() {
    for (days, expected) in [(1u32, "qdr:d"), (7, "qdr:w"), (31, "qdr:m"), (365, "qdr:y")] {
        let filters = SearchFilters {
            recency_days: Some(days),
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
        };
        let body = serper_search_request_body("query", 10, &filters);
        assert_eq!(body["tbs"], json!(expected));
    }
}

#[test]
fn num_is_clamped_to_serper_cap() {
    let body = serper_search_request_body("query", 0, &SearchFilters::default());
    assert_eq!(body["num"], json!(1));
    let body = serper_search_request_body("query", 10_000, &SearchFilters::default());
    assert_eq!(body["num"], json!(100));
}
