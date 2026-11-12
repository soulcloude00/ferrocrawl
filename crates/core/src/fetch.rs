//! The fast path: one pooled HTTP/2 client with compression and browser-like headers.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, HeaderMap, HeaderValue};

pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// Bodies over this are cut off; nobody wants a 200 MB "page".
const MAX_BODY: usize = 20 * 1024 * 1024;

pub struct Fetched {
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
    /// The raw bytes, when the response is a PDF (`body` is then empty).
    pub pdf: Option<Vec<u8>>,
    /// Seconds the server asked us to wait (`Retry-After`), when it did.
    pub retry_after: Option<u64>,
}

/// PDFs can be big; allow more than an HTML page.
const MAX_PDF: usize = 60 * 1024 * 1024;

pub fn client() -> reqwest::Client {
    let mut h = HeaderMap::new();
    h.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    h.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(h)
        .pool_max_idle_per_host(64)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("http client")
}

// ------------------------------------------------------------------ declared-bot sites
//
// Some sites publish a fair-access policy for automated clients instead of blocking them: SEC
// EDGAR asks for a User-Agent naming the operator and a contact email, and at most 10 requests a
// second (https://www.sec.gov/os/accessing-edgar-data). For those hosts we say who we are and
// keep to their rate. The contact comes from `SNARF_CONTACT` or
// `~/.config/snarf/contact` ("Name, email"); without one, requests go out as before.

/// Hosts with a published declared-bot policy. `SNARF_DECLARED_HOSTS` (comma-separated)
/// adds more.
const DECLARED_HOSTS: &[&str] = &["sec.gov"];
/// Requests per second allowed on a declared host (SEC's published limit is 10).
const DECLARED_RATE: u32 = 8;

fn contact() -> Option<&'static str> {
    static CONTACT: OnceLock<Option<String>> = OnceLock::new();
    CONTACT
        .get_or_init(|| {
            let from_env = std::env::var("SNARF_CONTACT").ok();
            let from_file = || {
                let home = std::env::var_os("XDG_CONFIG_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
                std::fs::read_to_string(home.join("snarf").join("contact")).ok()
            };
            from_env.or_else(from_file).map(|c| c.trim().to_string()).filter(|c| c.contains('@'))
        })
        .as_deref()
}

fn declared_hosts() -> &'static [String] {
    static HOSTS: OnceLock<Vec<String>> = OnceLock::new();
    HOSTS.get_or_init(|| {
        let mut v: Vec<String> = DECLARED_HOSTS.iter().map(|h| h.to_string()).collect();
        if let Ok(extra) = std::env::var("SNARF_DECLARED_HOSTS") {
            v.extend(extra.split(',').map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()));
        }
        v
    })
}

/// The site this host falls under, when it has a declared-bot policy.
fn declared_site(url: &str) -> Option<&'static str> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    declared_hosts().iter().find(|d| host == **d || host.ends_with(&format!(".{d}"))).map(String::as_str)
}

/// Spaces requests to a declared site so they stay under its published rate.
async fn pace(site: &str) {
    static NEXT: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);
    let gap = Duration::from_secs(1) / DECLARED_RATE;
    let wait = {
        let mut next = NEXT.lock().unwrap();
        let now = Instant::now();
        let slot = next.get(site).copied().filter(|t| *t > now).unwrap_or(now);
        next.insert(site.to_string(), slot + gap);
        slot.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

pub async fn get(client: &reqwest::Client, url: &str, timeout: Duration) -> Result<Fetched> {
    let mut req = client.get(url).timeout(timeout);
    if let (Some(site), Some(who)) = (declared_site(url), contact()) {
        pace(site).await;
        req = req.header(reqwest::header::USER_AGENT, format!("snarf/{} ({who})", env!("CARGO_PKG_VERSION")));
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase());
    let is_pdf = content_type.as_deref().is_some_and(|c| c.contains("application/pdf"))
        || (content_type.as_deref().is_none_or(|c| c.contains("octet-stream")) && final_url.to_ascii_lowercase().split('?').next().is_some_and(|p| p.ends_with(".pdf")));
    if is_pdf {
        if resp.content_length().is_some_and(|n| n as usize > MAX_PDF) {
            bail!("PDF larger than {} MB", MAX_PDF / 1024 / 1024);
        }
        let bytes = resp.bytes().await.context("reading PDF")?;
        if bytes.len() > MAX_PDF {
            bail!("PDF larger than {} MB", MAX_PDF / 1024 / 1024);
        }
        return Ok(Fetched { url: final_url, status, content_type, body: String::new(), pdf: Some(bytes.to_vec()), retry_after });
    }
    if let Some(ct) = &content_type {
        let textual = ct.contains("html") || ct.contains("xml") || ct.starts_with("text/") || ct.contains("json");
        if !textual {
            bail!("unsupported content type {ct}");
        }
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_BODY) {
        bail!("body larger than {} MB", MAX_BODY / 1024 / 1024);
    }
    let bytes = resp.bytes().await.context("reading body")?;
    // Servers that send PDFs as text/html or octet-stream still start with the magic.
    if bytes.starts_with(b"%PDF-") {
        return Ok(Fetched { url: final_url, status, content_type, body: String::new(), pdf: Some(bytes.to_vec()), retry_after });
    }
    let mut body = decode(&bytes, content_type.as_deref());
    if body.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
    }
    Ok(Fetched { url: final_url, status, content_type, body, pdf: None, retry_after })
}

/// Decodes a body by its declared charset (UTF-8 when none or unknown).
fn decode(bytes: &[u8], content_type: Option<&str>) -> String {
    let label = content_type.and_then(|c| c.split("charset=").nth(1)).map(|c| c.trim_matches(|ch: char| ch == '"' || ch.is_whitespace()));
    match label.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        Some(enc) if enc != encoding_rs::UTF_8 => enc.decode(bytes).0.into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}
