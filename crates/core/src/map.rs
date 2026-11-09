//! Lists a site's URLs fast: sitemaps (found through robots.txt and the usual paths, fetched in
//! parallel, following sitemap indexes) plus the links on the start page.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use regex::Regex;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::crawl::Scope;
use crate::types::{Format, Render, ScrapeOptions};
use crate::{Engine, fetch, urls};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MapOptions {
    pub limit: usize,
    /// Keep only URLs containing this (case-insensitive).
    pub search: Option<String>,
    pub include_subdomains: bool,
    pub use_sitemap: bool,
    /// When sitemaps come up short, follow links this many hops from the start page.
    pub depth: usize,
}

impl Default for MapOptions {
    fn default() -> Self {
        Self { limit: 5000, search: None, include_subdomains: false, use_sitemap: true, depth: 1 }
    }
}

static LOC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<loc>\s*(?:<!\[CDATA\[)?\s*(.*?)\s*(?:\]\]>)?\s*</loc>").unwrap());
static SITEMAP_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?im)^\s*sitemap:\s*(\S+)").unwrap());

const MAX_SITEMAPS: usize = 64;

/// Page URLs from the site's sitemaps, at most `limit`.
pub async fn sitemap_urls(engine: &Engine, start: &Url, limit: usize) -> Vec<Url> {
    let timeout = Duration::from_secs(15);
    let mut queue: Vec<String> = Vec::new();
    if let Ok(robots) = start.join("/robots.txt")
        && let Ok(got) = fetch::get(engine.http(), robots.as_str(), timeout).await
        && got.status < 400
    {
        queue.extend(SITEMAP_LINE.captures_iter(&got.body).map(|c| c[1].to_string()));
    }
    if queue.is_empty() {
        for p in ["/sitemap.xml", "/sitemap_index.xml"] {
            if let Ok(u) = start.join(p) {
                queue.push(u.to_string());
            }
        }
    }

    let mut seen_maps: HashSet<String> = HashSet::new();
    let mut pages: Vec<Url> = Vec::new();
    let mut seen_pages: HashSet<String> = HashSet::new();
    let mut inflight = FuturesUnordered::new();
    loop {
        while inflight.len() < 8 && seen_maps.len() < MAX_SITEMAPS {
            let Some(m) = queue.pop() else { break };
            if !seen_maps.insert(m.clone()) {
                continue;
            }
            inflight.push(async move { fetch::get(engine.http(), &m, timeout).await });
        }
        let Some(got) = inflight.next().await else { break };
        let Ok(got) = got else { continue };
        if got.status >= 400 {
            continue;
        }
        let is_index = got.body.contains("<sitemapindex");
        for c in LOC.captures_iter(&got.body) {
            let loc = c[1].replace("&amp;", "&");
            if is_index || loc.ends_with(".xml") {
                queue.push(loc);
            } else if let Ok(u) = Url::parse(&loc)
                && seen_pages.insert(urls::normalize(&u))
            {
                pages.push(u);
                if pages.len() >= limit {
                    return pages;
                }
            }
        }
    }
    pages
}

pub async fn map(engine: &Engine, start: &str, opts: &MapOptions) -> Result<Vec<String>> {
    let start = urls::parse_http(start)?;
    let scope = Scope::new(&start, true, opts.include_subdomains, &[], &[])?;
    let scrape = ScrapeOptions { formats: vec![Format::Links], render: Render::Never, ..Default::default() };
    let (from_sitemaps, from_page) = tokio::join!(
        async {
            if opts.use_sitemap { sitemap_urls(engine, &start, opts.limit.saturating_mul(2)).await } else { Vec::new() }
        },
        engine.page(start.as_str(), &scrape),
    );
    let mut page_links = from_page.map(|(p, _)| p.links.clone()).unwrap_or_default();

    // Few URLs from sitemaps: walk links a hop or two, every page of a hop at once.
    let mut frontier: Vec<String> = page_links.clone();
    let mut visited: HashSet<String> = HashSet::from([urls::normalize(&start)]);
    for _ in 0..opts.depth {
        if from_sitemaps.len() + page_links.len() >= opts.limit {
            break;
        }
        let next: Vec<String> = frontier
            .iter()
            .filter_map(|l| Url::parse(l).ok())
            .filter(|u| scope.allows(u) && visited.insert(urls::normalize(u)))
            .take(200)
            .map(|u| u.to_string())
            .collect();
        let found: Vec<Vec<String>> = futures::stream::iter(next)
            .map(|u| {
                let scrape = &scrape;
                async move { engine.page(&u, scrape).await.map(|(p, _)| p.links.clone()).unwrap_or_default() }
            })
            .buffer_unordered(32)
            .collect()
            .await;
        frontier = found.into_iter().flatten().collect();
        page_links.extend(frontier.iter().cloned());
    }

    let needle = opts.search.as_ref().map(|s| s.to_lowercase());
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let all = std::iter::once(start.clone())
        .chain(from_sitemaps)
        .chain(page_links.iter().filter_map(|l| Url::parse(l).ok()));
    for u in all {
        if !scope.allows(&u) || !seen.insert(urls::normalize(&u)) {
            continue;
        }
        if let Some(n) = &needle
            && !u.as_str().to_lowercase().contains(n)
        {
            continue;
        }
        out.push(u.to_string());
        if out.len() >= opts.limit {
            break;
        }
    }
    Ok(out)
}
