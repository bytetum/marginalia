//! URL -> title + Markdown. Firecrawl when a key is set, otherwise local Readability (dom_smoothie).

use std::fmt::Display;

use serde_json::{Value, json};

/// Why a page could not be turned into an article.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ExtractError(pub String);

type Result<T> = std::result::Result<T, ExtractError>;

fn err(e: impl Display) -> ExtractError {
    ExtractError(e.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Via {
    Firecrawl,
    Readability,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Firecrawl => "firecrawl",
            Via::Readability => "readability",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Extracted {
    pub title: String,
    pub markdown: String,
    pub via: Via,
    /// Raw page HTML, when the Readability path fetched it (worth keeping as a blob).
    pub html: Option<String>,
}

pub async fn extract(client: &reqwest::Client, url: &str, firecrawl_key: Option<&str>) -> Result<Extracted> {
    match firecrawl_key {
        Some(key) => via_firecrawl(client, url, key).await,
        None => via_readability(client, url).await,
    }
}

async fn via_firecrawl(client: &reqwest::Client, url: &str, key: &str) -> Result<Extracted> {
    let res = client
        .post("https://api.firecrawl.dev/v2/scrape")
        .bearer_auth(key)
        .json(&json!({ "url": url, "formats": ["markdown"], "onlyMainContent": true }))
        .send()
        .await
        .map_err(err)?;
    let status = res.status();
    let body: Value = res.json().await.map_err(err)?;
    if !status.is_success() {
        return Err(err(format!("Firecrawl {status}: {}", body.to_string().chars().take(200).collect::<String>())));
    }
    let markdown = body["data"]["markdown"].as_str().unwrap_or("").trim().to_string();
    if markdown.is_empty() {
        return Err(err("Firecrawl returned no markdown"));
    }
    let title = body["data"]["metadata"]["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(url).to_string();
    Ok(Extracted { title, markdown, via: Via::Firecrawl, html: None })
}

/// Pure: HTML page -> main article as Markdown (`html` is left `None`).
pub fn html_to_article(html: &str, url: &str) -> Result<Extracted> {
    let mut readability = dom_smoothie::Readability::new(html, Some(url), None).map_err(err)?;
    let article = readability.parse().map_err(|e| err(format!("Readability could not find an article: {e}")))?;
    // Images cost tokens and carry no text for the agent.
    let converter = htmd::HtmlToMarkdown::builder().skip_tags(vec!["img", "picture", "figure", "script", "style"]).build();
    let markdown = strip_empty_links(&converter.convert(&article.content).map_err(err)?).trim().to_string();
    if markdown.is_empty() {
        return Err(err("Readability found no article text"));
    }
    let title = if article.title.trim().is_empty() { url.to_string() } else { article.title.trim().to_string() };
    Ok(Extracted { title, markdown, via: Via::Readability, html: None })
}

/// Links whose only content was an image become `[](url)` once images are skipped; drop them.
fn strip_empty_links(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(start) = rest.find("[](") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        match after.find(')') {
            Some(end) if !after[..end].contains(char::is_whitespace) => rest = &after[end + 1..],
            _ => {
                out.push_str("[](");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

async fn via_readability(client: &reqwest::Client, url: &str) -> Result<Extracted> {
    let res = client.get(url).header("accept", "text/html").send().await.map_err(err)?;
    if !res.status().is_success() {
        return Err(err(format!("Fetch {} for {url}", res.status())));
    }
    let final_url = res.url().to_string();
    let html = res.text().await.map_err(err)?;
    let mut extracted = html_to_article(&html, &final_url)?;
    extracted.html = Some(html);
    Ok(extracted)
}
