use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::header::USER_AGENT;
use reqwest::Client;
use url::Url;

use crate::error::{NovaVeilSearchError, Result};
use crate::sources::{get_text, SourceCaps, SourceExtractor, SourceType};

const UA: &str = "nova-veil-search/0.1 (https://github.com/kingsunb/NovaVeilSearch)";

#[derive(Debug, Clone)]
pub struct ArxivRaw {
    pub title: String,
    pub authors: Vec<String>,
    pub categories: Vec<String>,
    pub summary: String,
    pub abs_link: String,
    pub pdf_link: String,
}

pub struct ArxivExtractor;

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

fn extract_id(url: &Url) -> Option<String> {
    let path = url.path();
    for prefix in ["/abs/", "/pdf/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            if !rest.is_empty() {
                // PDF links carry a `.pdf` extension the arXiv API rejects in
                // `id_list`; strip it so `/pdf/<id>.pdf` resolves the same paper
                // as `/abs/<id>`.
                return Some(rest.strip_suffix(".pdf").unwrap_or(rest).to_string());
            }
        }
    }
    None
}

impl ArxivExtractor {
    /// D-11: parse an arXiv Atom feed with quick-xml. `<category>`/`<link>` are
    /// self-closing (Event::Empty). Text fields are collected only while inside
    /// `<entry>` so the feed-level `<title>` is ignored. quick-xml does not
    /// resolve external entities or load DTDs → no billion-laughs / XXE risk.
    pub fn parse_atom(xml: &str) -> Result<ArxivRaw> {
        #[derive(PartialEq)]
        enum Field {
            None,
            Title,
            Summary,
            Name,
        }

        let mut reader = Reader::from_str(xml);
        let mut in_entry = false;
        let mut in_author = false;
        let mut field = Field::None;
        let mut buf = String::new();

        let mut title = String::new();
        let mut summary = String::new();
        let mut authors: Vec<String> = Vec::new();
        let mut categories: Vec<String> = Vec::new();
        let mut abs_link = String::new();
        let mut pdf_link = String::new();

        let parse_err =
            |e: quick_xml::Error| NovaVeilSearchError::Parse(format!("arxiv XML parse error: {e}"));

        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(Event::Start(e)) => match e.name().as_ref() {
                    b"entry" => in_entry = true,
                    b"author" if in_entry => in_author = true,
                    b"title" if in_entry => {
                        field = Field::Title;
                        buf.clear();
                    }
                    b"summary" if in_entry => {
                        field = Field::Summary;
                        buf.clear();
                    }
                    b"name" if in_author => {
                        field = Field::Name;
                        buf.clear();
                    }
                    _ => {}
                },
                Ok(Event::Empty(e)) if in_entry => match e.name().as_ref() {
                    b"category" => {
                        if let Some(term) = attr(&e, b"term") {
                            categories.push(term);
                        }
                    }
                    b"link" => {
                        let href = attr(&e, b"href").unwrap_or_default();
                        let typ = attr(&e, b"type").unwrap_or_default();
                        let rel = attr(&e, b"rel").unwrap_or_default();
                        if typ == "application/pdf" {
                            pdf_link = href;
                        } else if rel == "alternate" {
                            abs_link = href;
                        }
                    }
                    _ => {}
                },
                Ok(Event::Text(e)) if field != Field::None => {
                    let t = e.unescape().map_err(parse_err)?;
                    buf.push_str(t.as_ref());
                }
                Ok(Event::End(e)) => match e.name().as_ref() {
                    b"title" if field == Field::Title => {
                        title = buf.trim().to_string();
                        field = Field::None;
                    }
                    b"summary" if field == Field::Summary => {
                        summary = buf.trim().to_string();
                        field = Field::None;
                    }
                    b"name" if field == Field::Name => {
                        authors.push(buf.trim().to_string());
                        field = Field::None;
                    }
                    b"author" => in_author = false,
                    b"entry" => {
                        in_entry = false;
                        in_author = false;
                        field = Field::None;
                    }
                    _ => {}
                },
                Err(e) => return Err(parse_err(e)),
                _ => {}
            }
        }

        if title.is_empty() || summary.is_empty() {
            return Err(NovaVeilSearchError::Parse(
                "arxiv: missing title or summary".into(),
            ));
        }
        Ok(ArxivRaw {
            title,
            authors,
            categories,
            summary,
            abs_link,
            pdf_link,
        })
    }
}

pub(crate) async fn fetch(client: &Client, url: &Url) -> Result<ArxivRaw> {
    let id = extract_id(url)
        .ok_or_else(|| NovaVeilSearchError::Parse("arxiv: cannot extract paper id".into()))?;
    let api_url = format!("https://export.arxiv.org/api/query?id_list={id}");
    let headers = [(USER_AGENT, UA)];
    let xml = get_text(client, &api_url, &headers, "arxiv").await?;
    ArxivExtractor::parse_atom(&xml)
}

pub fn render(raw: &ArxivRaw, _caps: &SourceCaps) -> String {
    format!(
        "# {}\n\n**Authors:** {}\n\n**Categories:** {}\n\n**Links:** [Abstract]({}) | [PDF]({})\n\n## Abstract\n\n{}\n",
        raw.title,
        raw.authors.join(", "),
        raw.categories.join(", "),
        raw.abs_link,
        raw.pdf_link,
        raw.summary,
    )
}

#[async_trait]
impl SourceExtractor for ArxivExtractor {
    fn matches(&self, url: &Url) -> bool {
        if url.host_str() != Some("arxiv.org") {
            return false;
        }
        let p = url.path();
        (p.starts_with("/abs/") && p.len() > "/abs/".len())
            || (p.starts_with("/pdf/") && p.len() > "/pdf/".len())
    }
    fn kind(&self) -> SourceType {
        SourceType::Arxiv
    }
    async fn fetch_render(&self, client: &Client, url: &Url, caps: &SourceCaps) -> Result<String> {
        let raw = fetch(client, url).await?;
        Ok(render(&raw, caps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_id_strips_pdf_suffix() {
        // arXiv PDF links carry a .pdf extension the API rejects in id_list.
        let url = Url::parse("https://arxiv.org/pdf/1706.03762.pdf").unwrap();
        assert_eq!(extract_id(&url).as_deref(), Some("1706.03762"));
    }

    #[test]
    fn extract_id_handles_abs_and_extensionless_pdf() {
        let abs = Url::parse("https://arxiv.org/abs/1706.03762").unwrap();
        assert_eq!(extract_id(&abs).as_deref(), Some("1706.03762"));
        let pdf = Url::parse("https://arxiv.org/pdf/2310.06825").unwrap();
        assert_eq!(extract_id(&pdf).as_deref(), Some("2310.06825"));
    }
}
