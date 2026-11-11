//! ferrocrawl: a fast web scraper. Plain HTTP and a streaming HTML rewriter do almost all the
//! work; headless Chromium starts only for pages that need JavaScript.

pub mod bench;
pub mod crawl;
pub mod extract;
pub mod fetch;
pub mod map;
pub mod pdf;
pub mod render;
pub mod structured;
pub mod types;
pub mod urls;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use moka::future::Cache;

pub use crawl::{CrawlEvent, CrawlOptions, CrawlSummary, crawl};
pub use map::{MapOptions, map};
pub use types::{Document, Format, Metadata, Render, ScrapeOptions};

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Headless browser tabs open at once.
    pub max_tabs: usize,
    /// In-memory cache budget in bytes.
    pub cache_bytes: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { max_tabs: 6, cache_bytes: 256 * 1024 * 1024 }
    }
}

/// Everything about a page, before it's cut down to the formats a caller asked for.
#[derive(Debug)]
pub struct Page {
    pub markdown: String,
    pub html: String,
    pub raw_html: String,
    pub text: String,
    pub links: Vec<String>,
    pub metadata: Metadata,
    fetched_at: Instant,
}

impl Page {
    pub fn document(&self, formats: &[Format], cache_hit: bool) -> Document {
        let has = |f: Format| formats.contains(&f);
        let mut metadata = self.metadata.clone();
        metadata.cache_hit = cache_hit;
        Document {
            markdown: has(Format::Markdown).then(|| self.markdown.clone()),
            html: has(Format::Html).then(|| self.html.clone()),
            raw_html: has(Format::RawHtml).then(|| self.raw_html.clone()),
            links: has(Format::Links).then(|| self.links.clone()),
            text: has(Format::Text).then(|| self.text.clone()),
            metadata,
        }
    }
}

pub struct Engine {
    client: reqwest::Client,
    renderer: render::Renderer,
    cache: Cache<String, Arc<Page>>,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Arc<Self> {
        let cache = Cache::builder()
            .max_capacity(cfg.cache_bytes)
            .weigher(|k: &String, p: &Arc<Page>| {
                (k.len() + p.markdown.len() + p.html.len() + p.raw_html.len() + p.text.len()).try_into().unwrap_or(u32::MAX)
            })
            .time_to_live(Duration::from_secs(3600))
            .build();
        Arc::new(Self { client: fetch::client(), renderer: render::Renderer::new(cfg.max_tabs), cache })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    pub async fn scrape(&self, url: &str, opts: &ScrapeOptions) -> Result<Document> {
        let (page, hit) = self.page(url, opts).await?;
        Ok(page.document(&opts.formats, hit))
    }

    /// The full page, from the cache when it's fresh enough. Identical requests in flight at the
    /// same time share one fetch.
    pub async fn page(&self, url: &str, opts: &ScrapeOptions) -> Result<(Arc<Page>, bool)> {
        let url = urls::parse_http(url)?.to_string();
        if opts.max_age_ms == 0 {
            return Ok((Arc::new(self.load(&url, opts).await?), false));
        }
        let key = format!("{:?}|{}|{:?}|{url}", opts.render, opts.only_main_content, opts.exclude_tags);
        if let Some(p) = self.cache.get(&key).await {
            if p.fetched_at.elapsed() <= Duration::from_millis(opts.max_age_ms) {
                return Ok((p, true));
            }
            self.cache.invalidate(&key).await;
        }
        let mut fresh = false;
        let page = self
            .cache
            .try_get_with(key, async {
                fresh = true;
                self.load(&url, opts).await.map(Arc::new)
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        Ok((page, !fresh))
    }

    /// Loads the page and refuses to pass off a failure (a block screen, an error page, an empty
    /// shell) as a success.
    async fn load(&self, url: &str, opts: &ScrapeOptions) -> Result<Page> {
        let page = self.load_inner(url, opts).await?;
        if !page.metadata.content_type.as_deref().is_some_and(|c| c.contains("html")) {
            return Ok(page);
        }
        match extract::failure_page(&page.text) {
            Some(what) => bail!("{what}{}", if page.metadata.status_code >= 400 { format!(" (HTTP {})", page.metadata.status_code) } else { String::new() }),
            None => Ok(page),
        }
    }

    async fn load_inner(&self, url: &str, opts: &ScrapeOptions) -> Result<Page> {
        let timeout = Duration::from_millis(opts.timeout_ms.max(1000));
        if opts.render == Render::Always {
            return self.load_rendered(url, opts, timeout).await;
        }
        let t0 = Instant::now();
        // The plain fetch gets a shorter leash in auto mode: a site that stalls non-browser clients
        // (Akamai-fronted news sites do) should fall through to the browser quickly.
        let http_timeout = if opts.render == Render::Auto { timeout.min(Duration::from_secs(8)) } else { timeout };
        let got = match fetch::get(&self.client, url, http_timeout).await {
            Ok(g) => g,
            // No DNS record, no TCP connection: the browser can't reach the host either, and
            // trying costs up to 40 s (queueing for a tab, then a render that times out).
            Err(e) if unreachable(&e) => return Err(e),
            Err(e) if opts.render == Render::Auto => {
                tracing::debug!("plain fetch of {url} failed ({e:#}); trying the browser");
                return self.load_fallback(url, opts, Fallback::Only).await.map_err(|re| e.context(format!("browser fallback: {re:#}")));
            }
            Err(e) => return Err(e),
        };
        // Rate limited: wait as asked (a few seconds at most) and try once more.
        let got = if got.status == 429 {
            tokio::time::sleep(Duration::from_secs(got.retry_after.unwrap_or(1).clamp(1, 4))).await;
            fetch::get(&self.client, url, http_timeout).await.unwrap_or(got)
        } else {
            got
        };
        let fetch_ms = t0.elapsed().as_millis() as u64;
        if let Some(bytes) = got.pdf {
            return pdf_page(url, got.url, got.status, bytes, fetch_ms).await;
        }
        let is_html = got.content_type.as_deref().is_none_or(|c| c.contains("html") || c.contains("xml"));
        if !is_html {
            // An error status with a short body ("Not Found", "Forbidden") is the error, not content.
            if got.status >= 400 && extract::words(&got.body) < 50 {
                bail!("HTTP {}", got.status);
            }
            // JSON, plain text, Markdown: the body is the content.
            let metadata = Metadata {
                source_url: url.to_string(),
                url: got.url.clone(),
                status_code: got.status,
                content_type: got.content_type.clone(),
                fetch_ms,
                bytes: got.body.len(),
                ..Default::default()
            };
            return Ok(Page {
                markdown: got.body.clone(),
                html: String::new(),
                text: got.body.clone(),
                raw_html: got.body,
                links: Vec::new(),
                metadata,
                fetched_at: Instant::now(),
            });
        }
        // A challenge screen is a wall. A 4xx status alone is not: plenty of sites send the real
        // page with a 403 or 404, so the body decides.
        let lower = got.body.to_ascii_lowercase();
        let challenge = extract::is_challenge(&lower);
        let blocked = challenge || matches!(got.status, 401 | 403 | 429 | 503);
        // A hard block (a captcha wall, a WAF deny page) answers a browser from this machine the
        // same way; only JavaScript challenges are worth a render.
        let hard = blocked && got.status >= 400 && extract::is_hard_block(&lower);
        drop(lower);
        let (page, needs_js) = build(url, &got.url, got.status, got.content_type, got.body, false, fetch_ms, opts).await?;
        if hard {
            bail!("blocked by the site (HTTP {})", page.metadata.status_code);
        }
        if opts.render == Render::Auto && (needs_js || blocked) {
            // An app shell has nothing without the browser, so it may queue for a tab. So may a
            // blocked page whose response is only a block screen (Shopify answers bots with a 429):
            // the browser is its one chance. A blocked page that still sent real text only gets a
            // tab if one is free right now.
            let wait = if needs_js {
                Fallback::Only
            } else if extract::failure_page(&page.text).is_some() || extract::words(&page.text) < 50 {
                Fallback::Blocked
            } else {
                Fallback::Optional
            };
            match self.load_fallback(url, opts, wait).await {
                // Use the rendered page only when it is better: real content, and more of it.
                Ok(p) if extract::failure_page(&p.text).is_none() && p.text.len() > page.text.len() => return Ok(p),
                Ok(_) => {}
                Err(e) => tracing::debug!("render fallback for {url} failed: {e:#}"),
            }
        }
        // Only a challenge screen or an empty body is a failure; a 4xx page with real text is kept.
        let words = extract::words(&page.text);
        if page.metadata.status_code >= 400 && (challenge || words < 20) {
            bail!("HTTP {}", page.metadata.status_code);
        }
        // An app shell we couldn't render is a failure too, not an empty success.
        if needs_js && words < 10 {
            bail!("empty page: its content is loaded by JavaScript and the browser could not render it");
        }
        Ok(page)
    }

    /// The browser as a fallback in auto mode: a render of up to 15 s, after waiting for a tab
    /// as long as `how` says it's worth (see [`Fallback`]).
    async fn load_fallback(&self, url: &str, opts: &ScrapeOptions, how: Fallback) -> Result<Page> {
        let timeout = Duration::from_millis(opts.timeout_ms.max(1000));
        let tab_wait = match how {
            Fallback::Only => timeout.min(Duration::from_secs(25)),
            Fallback::Blocked => timeout.min(Duration::from_secs(8)),
            Fallback::Optional => Duration::from_millis(300),
        };
        // The budget covers the render itself; time spent queueing for a tab comes on top.
        self.load_rendered_with(url, opts, Some(tab_wait), tab_wait + timeout.min(Duration::from_secs(15))).await
    }

    async fn load_rendered(&self, url: &str, opts: &ScrapeOptions, timeout: Duration) -> Result<Page> {
        self.load_rendered_with(url, opts, None, timeout).await
    }

    async fn load_rendered_with(&self, url: &str, opts: &ScrapeOptions, tab_wait: Option<Duration>, timeout: Duration) -> Result<Page> {
        let t0 = Instant::now();
        let r = self.renderer.render_within(url, Duration::from_millis(opts.wait_for_ms), tab_wait, timeout).await?;
        // Chrome's own "This site can't be reached" page is a failure, not content.
        if r.url.starts_with("chrome-error://") || r.html.contains("id=\"main-frame-error\"") || r.html.contains("neterror") {
            let code = regex::Regex::new(r"ERR_[A-Z_]+").ok().and_then(|re| re.find(&r.html).map(|m| m.as_str().to_string()));
            bail!("browser could not load the page ({})", code.unwrap_or_else(|| "network error".into()));
        }
        let fetch_ms = t0.elapsed().as_millis() as u64;
        Ok(build(url, &r.url, 200, Some("text/html".into()), r.html, true, fetch_ms, opts).await?.0)
    }
}

/// How much a browser render is worth queueing for.
#[derive(Clone, Copy)]
enum Fallback {
    /// The plain fetch got nothing usable (an app shell, no response at all).
    Only,
    /// The plain fetch got a block screen: the browser is the only way in, but most blocks hold.
    Blocked,
    /// The plain fetch got real text; a render only if a tab is free now.
    Optional,
}

/// True when the host can't be reached at all (DNS failure, refused or timed-out TCP connect).
/// TLS and HTTP/2 errors don't count: Chromium's network stack sometimes gets through those.
fn unreachable(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}").to_ascii_lowercase();
    if ["tls", "ssl", "certificate", "handshake", "http2"].iter().any(|k| m.contains(k)) {
        return false;
    }
    ["dns error", "failed to lookup address", "tcp connect error", "connection refused", "no route to host", "network is unreachable"]
        .iter()
        .any(|k| m.contains(k))
        || (m.contains("(connect)") && m.contains("timed out"))
}

/// A PDF's text, converted on the blocking pool.
async fn pdf_page(source: &str, final_url: String, status: u16, bytes: Vec<u8>, fetch_ms: u64) -> Result<Page> {
    let size = bytes.len();
    let t0 = Instant::now();
    let markdown = tokio::task::spawn_blocking(move || pdf::to_markdown(&bytes)).await??;
    let title = markdown.lines().map(str::trim).find(|l| !l.is_empty() && l.len() < 200).map(str::to_string);
    let metadata = Metadata {
        title,
        source_url: source.to_string(),
        url: final_url,
        status_code: status,
        content_type: Some("application/pdf".into()),
        fetch_ms,
        extract_ms: t0.elapsed().as_millis() as u64,
        bytes: size,
        ..Default::default()
    };
    Ok(Page { text: markdown.clone(), markdown, html: String::new(), raw_html: String::new(), links: Vec::new(), metadata, fetched_at: Instant::now() })
}

/// Extract on the blocking pool so big pages don't stall the async workers.
#[allow(clippy::too_many_arguments)]
async fn build(
    source: &str,
    final_url: &str,
    status: u16,
    content_type: Option<String>,
    raw: String,
    rendered: bool,
    fetch_ms: u64,
    opts: &ScrapeOptions,
) -> Result<(Page, bool)> {
    let (source, final_url) = (source.to_string(), final_url.to_string());
    let (only_main, exclude) = (opts.only_main_content, opts.exclude_tags.clone());
    tokio::task::spawn_blocking(move || {
        let t0 = Instant::now();
        let ex = extract::extract(&raw, &final_url, only_main, &exclude);
        let needs_js = !rendered && extract::looks_client_rendered(&raw, &ex);
        let metadata = Metadata {
            title: ex.title,
            description: ex.description,
            language: ex.language,
            canonical: ex.canonical,
            og_image: ex.og_image,
            source_url: source,
            url: final_url,
            status_code: status,
            content_type,
            rendered,
            cache_hit: false,
            fetch_ms,
            extract_ms: t0.elapsed().as_millis() as u64,
            bytes: raw.len(),
        };
        let page = Page {
            markdown: ex.markdown,
            html: ex.html,
            text: ex.text,
            links: ex.links,
            raw_html: raw,
            metadata,
            fetched_at: Instant::now(),
        };
        Ok((page, needs_js))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::unreachable;

    #[test]
    fn unreachable_hosts_skip_the_browser() {
        let e = |m: &str| anyhow::anyhow!("{m}");
        assert!(unreachable(&e("GET http://x/: error sending request for url (http://x/): client error (Connect): operation timed out")));
        assert!(unreachable(&e("GET http://x/: client error (Connect): tcp connect error: deadline has elapsed")));
        assert!(unreachable(&e("GET http://x/: client error (Connect): dns error: failed to lookup address information")));
        // The browser may get through these.
        assert!(!unreachable(&e("GET http://x/: error sending request for url (http://x/): operation timed out")));
        assert!(!unreachable(&e("GET http://x/: client error (SendRequest): http2 error: stream error received")));
        assert!(!unreachable(&e("GET https://x/: client error (Connect): invalid peer certificate: UnknownIssuer")));
    }
}
