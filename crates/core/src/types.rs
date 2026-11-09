use serde::{Deserialize, Serialize};

/// When to use a headless browser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Render {
    /// Plain HTTP first; fall back to the browser only when the page looks client-rendered.
    #[default]
    Auto,
    /// Always render in headless Chromium.
    Always,
    /// Never start a browser.
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    Markdown,
    Html,
    RawHtml,
    Links,
    Text,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ScrapeOptions {
    pub formats: Vec<Format>,
    /// Drop navigation, headers, footers, sidebars and cookie banners, and keep `<main>`/`<article>`.
    pub only_main_content: bool,
    pub render: Render,
    pub timeout_ms: u64,
    /// Extra wait after the page loads, when rendering in the browser.
    pub wait_for_ms: u64,
    /// Serve from the in-memory cache when the cached copy is younger than this. 0 skips the cache.
    pub max_age_ms: u64,
    /// CSS selectors to remove before converting.
    pub exclude_tags: Vec<String>,
}

impl Default for ScrapeOptions {
    fn default() -> Self {
        Self {
            formats: vec![Format::Markdown],
            only_main_content: true,
            render: Render::Auto,
            timeout_ms: 30_000,
            wait_for_ms: 0,
            max_age_ms: 300_000,
            exclude_tags: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub title: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub canonical: Option<String>,
    pub og_image: Option<String>,
    pub source_url: String,
    pub url: String,
    pub status_code: u16,
    pub content_type: Option<String>,
    /// True when the page came from headless Chromium.
    pub rendered: bool,
    pub cache_hit: bool,
    /// Time spent fetching (network or browser).
    pub fetch_ms: u64,
    /// Time spent cleaning and converting.
    pub extract_ms: u64,
    pub bytes: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub metadata: Metadata,
}
