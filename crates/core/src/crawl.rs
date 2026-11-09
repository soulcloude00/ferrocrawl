//! Breadth-first crawl with a fixed number of pages in flight. Results stream out as each page
//! finishes, so the first page arrives in about one round trip.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use regex::Regex;
use serde::{Deserialize, Serialize};
use texting_robots::Robot;
use tokio::sync::mpsc;
use url::Url;

use crate::types::{Document, ScrapeOptions};
use crate::{Engine, fetch, map, urls};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CrawlOptions {
    /// Pages to scrape at most.
    pub limit: usize,
    /// Link hops from the start page.
    pub max_depth: usize,
    /// Pages in flight at once.
    pub concurrency: usize,
    /// Regexes a URL's path must match (any of them).
    pub include_paths: Vec<String>,
    /// Regexes that rule a URL's path out.
    pub exclude_paths: Vec<String>,
    /// Crawl the whole site, not only pages under the start URL's path.
    pub entire_domain: bool,
    pub allow_subdomains: bool,
    pub ignore_robots: bool,
    /// Seed the queue from the site's sitemaps.
    pub use_sitemap: bool,
    pub scrape: ScrapeOptions,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            limit: 100,
            max_depth: 5,
            concurrency: 24,
            include_paths: Vec::new(),
            exclude_paths: Vec::new(),
            entire_domain: false,
            allow_subdomains: false,
            ignore_robots: false,
            use_sitemap: true,
            scrape: ScrapeOptions::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CrawlEvent {
    Page { depth: usize, document: Box<Document> },
    Error { url: String, error: String },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrawlSummary {
    pub pages: usize,
    pub errors: usize,
    pub discovered: usize,
    pub elapsed_ms: u64,
    pub pages_per_sec: f64,
}

pub(crate) struct Scope {
    host: String,
    prefix: String,
    subdomains: bool,
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

impl Scope {
    pub(crate) fn new(start: &Url, entire_domain: bool, subdomains: bool, include: &[String], exclude: &[String]) -> Result<Self> {
        let compile = |v: &[String]| v.iter().map(|p| Regex::new(p)).collect::<Result<Vec<_>, _>>();
        let path = start.path();
        let prefix = if entire_domain { "/".into() } else { path[..=path.rfind('/').unwrap_or(0)].to_string() };
        Ok(Self {
            host: start.host_str().unwrap_or_default().to_string(),
            prefix,
            subdomains,
            include: compile(include)?,
            exclude: compile(exclude)?,
        })
    }

    pub(crate) fn allows(&self, u: &Url) -> bool {
        if !matches!(u.scheme(), "http" | "https") || urls::is_asset(u) {
            return false;
        }
        let Some(host) = u.host_str() else { return false };
        if !urls::same_site(&self.host, host, self.subdomains) {
            return false;
        }
        let path = u.path();
        if !path.starts_with(&self.prefix) && format!("{path}/") != self.prefix {
            return false;
        }
        if self.exclude.iter().any(|r| r.is_match(path)) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|r| r.is_match(path))
    }
}

async fn robots(engine: &Engine, start: &Url) -> Option<Robot> {
    let url = start.join("/robots.txt").ok()?;
    let got = fetch::get(engine.http(), url.as_str(), std::time::Duration::from_secs(10)).await.ok()?;
    if got.status >= 400 {
        return None;
    }
    Robot::new("ferrocrawl", got.body.as_bytes()).ok()
}

/// Crawls from `start`, sending each page to `out` as it lands. Stops early when `out` is closed.
pub async fn crawl(engine: Arc<Engine>, start: String, opts: CrawlOptions, out: mpsc::Sender<CrawlEvent>) -> Result<CrawlSummary> {
    let t0 = Instant::now();
    let start = urls::parse_http(&start)?;
    let scope = Scope::new(&start, opts.entire_domain, opts.allow_subdomains, &opts.include_paths, &opts.exclude_paths)?;
    let robot = if opts.ignore_robots { None } else { robots(&engine, &start).await };
    let allowed = |u: &Url| robot.as_ref().is_none_or(|r| r.allowed(u.as_str()));

    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(Url, usize)> = VecDeque::new();
    seen.insert(urls::normalize(&start));
    queue.push_back((start.clone(), 0));

    if opts.use_sitemap && opts.max_depth > 0 {
        let found = map::sitemap_urls(&engine, &start, opts.limit.saturating_mul(4).max(500)).await;
        for u in found {
            if scope.allows(&u) && seen.insert(urls::normalize(&u)) {
                queue.push_back((u, 1));
            }
        }
    }

    let mut summary = CrawlSummary::default();
    let mut inflight = FuturesUnordered::new();
    let mut scheduled = 0usize;
    let concurrency = opts.concurrency.clamp(1, 512);

    loop {
        while inflight.len() < concurrency && scheduled < opts.limit {
            let Some((url, depth)) = queue.pop_front() else { break };
            if !allowed(&url) {
                continue;
            }
            scheduled += 1;
            let (engine, scrape) = (engine.clone(), &opts.scrape);
            inflight.push(async move {
                let r = engine.page(url.as_str(), scrape).await;
                (url, depth, r)
            });
        }
        let Some((url, depth, result)) = inflight.next().await else { break };
        let event = match result {
            Ok((page, hit)) => {
                if depth < opts.max_depth {
                    for link in &page.links {
                        if let Ok(u) = Url::parse(link)
                            && scope.allows(&u)
                            && seen.insert(urls::normalize(&u))
                        {
                            queue.push_back((u, depth + 1));
                        }
                    }
                }
                summary.pages += 1;
                CrawlEvent::Page { depth, document: Box::new(page.document(&opts.scrape.formats, hit)) }
            }
            Err(e) => {
                summary.errors += 1;
                CrawlEvent::Error { url: url.to_string(), error: format!("{e:#}") }
            }
        };
        if out.send(event).await.is_err() {
            break;
        }
    }

    summary.discovered = seen.len();
    summary.elapsed_ms = t0.elapsed().as_millis() as u64;
    summary.pages_per_sec = summary.pages as f64 / t0.elapsed().as_secs_f64().max(1e-3);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_keeps_to_the_start_path() {
        let start = Url::parse("https://www.x.dev/docs/intro").unwrap();
        let s = Scope::new(&start, false, false, &[], &["/docs/old".into()]).unwrap();
        let ok = |u: &str| s.allows(&Url::parse(u).unwrap());
        assert!(ok("https://x.dev/docs/guide"));
        assert!(ok("https://x.dev/docs"));
        assert!(!ok("https://x.dev/blog/post"));
        assert!(!ok("https://x.dev/docs/old/thing"));
        assert!(!ok("https://x.dev/docs/logo.png"));
        assert!(!ok("https://other.dev/docs/guide"));
    }
}
