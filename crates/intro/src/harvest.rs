//! People and relationships from public web pages: team pages, portfolio pages, speaker lists.
//!
//! Two readers:
//! - schema.org JSON-LD `Person` items (and an `Organization`'s employees, founders, members)
//! - repeated cards: a name-shaped element that repeats with the same markup three or more times,
//!   which is how team and speaker pages are built, with the short line after it as the title
//!
//! Portfolio pages give companies instead: the outbound links in the page body.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use anyhow::{Result, bail};
use ferrocrawl::{CrawlEvent, CrawlOptions, Engine, Format, ScrapeOptions};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use url::Url;

use crate::{EdgeKind, Graph, PersonIn, names};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Relation {
    /// Guess from the URL: /portfolio → portfolio, /speakers → people, otherwise team.
    #[default]
    Auto,
    /// People on the page work at the organization.
    Team,
    /// Companies linked from the page are the organization's investments.
    Portfolio,
    /// People are listed by the organization (speakers, mentors, advisors, members).
    People,
}

impl Relation {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "auto" => Self::Auto,
            "team" => Self::Team,
            "portfolio" | "investments" => Self::Portfolio,
            "people" | "speakers" | "event" | "members" => Self::People,
            _ => return None,
        })
    }

    fn for_url(self, u: &str) -> Self {
        if self != Self::Auto {
            return self;
        }
        let p = Url::parse(u).map(|u| u.path().to_lowercase()).unwrap_or_default();
        if ["portfolio", "companies", "investments", "startups", "founders-we-back"].iter().any(|k| p.contains(k)) {
            Self::Portfolio
        } else if ["speaker", "agenda", "mentor", "advisor", "members", "judges", "faculty", "guests", "episodes"].iter().any(|k| p.contains(k)) {
            Self::People
        } else {
            Self::Team
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Extracted {
    pub site_name: Option<String>,
    pub people: Vec<PersonIn>,
    /// (name, homepage) of companies linked from the page body.
    pub companies: Vec<(String, String)>,
}

static LD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<script[^>]*type\s*=\s*["']?application/ld\+json["']?[^>]*>(.*?)</script>"#).unwrap());
static TITLE_ORG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(.{2,60}?)\s*(?:\bat\b|@|,|\||·|—|–| - )\s*(.{2,60})$").unwrap());

fn sel(s: &str) -> Selector {
    Selector::parse(s).unwrap()
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_of(e: ElementRef) -> String {
    squash(&e.text().collect::<Vec<_>>().join(" "))
}

const BLOCKS: &[&str] = &["p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "li", "ul", "ol", "section", "article", "figure", "figcaption", "dt", "dd", "td", "tr", "table", "br", "img"];

/// True when `e` holds other blocks with text: it's a card, not the name in it.
fn is_container(e: ElementRef) -> bool {
    e.descendants().skip(1).filter_map(ElementRef::wrap).any(|d| BLOCKS.contains(&d.value().name()) && !text_of(d).is_empty())
}

/// "Partner at Sequoia" → ("Partner", "Sequoia"). "CEO, Acme Inc" → ("CEO", "Acme Inc").
pub fn split_title(t: &str) -> (String, Option<String>) {
    match TITLE_ORG.captures(t.trim()) {
        Some(c) if !names::looks_like_name(&c[2]) => (c[1].trim().to_string(), Some(c[2].trim().to_string())),
        _ => (t.trim().to_string(), None),
    }
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) => Some(squash(s)),
        Value::Object(o) => o.get("name").and_then(Value::as_str).map(squash),
        Value::Array(a) => a.iter().find_map(|x| x.as_str().map(squash).or_else(|| x.get("name").and_then(Value::as_str).map(squash))),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn same_as(v: &Value) -> Vec<String> {
    match v.get("sameAs") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn types(v: &Value) -> Vec<String> {
    match v.get("@type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn ld_people(raw: &str, out: &mut Vec<PersonIn>, site: &mut Option<String>) {
    fn walk(v: &Value, org: Option<&str>, out: &mut Vec<PersonIn>, site: &mut Option<String>, depth: usize) {
        if depth > 6 {
            return;
        }
        match v {
            Value::Array(a) => a.iter().for_each(|x| walk(x, org, out, site, depth + 1)),
            Value::Object(o) => {
                let t = types(v);
                if t.iter().any(|t| t == "Person") {
                    if let Some(name) = str_field(v, "name").filter(|n| names::person_key(n).contains(' ')) {
                        let links = same_as(v);
                        out.push(PersonIn {
                            name,
                            title: str_field(v, "jobTitle"),
                            org: str_field(v, "worksFor").or_else(|| str_field(v, "affiliation")).or_else(|| org.map(str::to_string)),
                            email: str_field(v, "email").map(|e| e.trim_start_matches("mailto:").to_string()),
                            linkedin: links.iter().find(|l| l.contains("linkedin.com/in/")).cloned(),
                            url: str_field(v, "url"),
                        });
                    }
                    return;
                }
                let is_org = t.iter().any(|t| t.contains("Organization") || t == "Corporation" || t == "WebSite");
                let name = str_field(v, "name");
                if is_org && site.is_none() && t.iter().any(|t| t != "WebSite") {
                    *site = name.clone();
                }
                let here = if is_org { name.as_deref().or(org) } else { org };
                for (k, child) in o {
                    if k != "@context" {
                        walk(child, here, out, site, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
    for m in LD.captures_iter(raw).take(30) {
        if let Ok(v) = serde_json::from_str::<Value>(m[1].trim()) {
            walk(&v, None, out, site, 0);
        }
    }
}

const SKIP_HOSTS: &[&str] = &[
    "twitter.com", "x.com", "linkedin.com", "facebook.com", "instagram.com", "youtube.com", "youtu.be", "github.com", "medium.com",
    "google.com", "apple.com", "crunchbase.com", "angel.co", "wellfound.com", "tiktok.com", "substack.com", "bit.ly", "t.co",
    "wikipedia.org", "vimeo.com", "spotify.com", "notion.so", "calendly.com", "typeform.com", "w3.org", "schema.org", "cloudflare.com",
    "goo.gl", "apps.apple.com", "play.google.com", "threads.net", "bsky.app", "discord.gg", "discord.com", "t.me", "wa.me",
];

fn in_chrome(e: ElementRef) -> bool {
    e.ancestors().filter_map(ElementRef::wrap).any(|a| {
        let n = a.value().name();
        matches!(n, "nav" | "footer" | "header" | "script" | "style" | "noscript" | "form" | "button" | "select")
            || a.value().attr("role").is_some_and(|r| r == "navigation" || r == "contentinfo")
    })
}

fn signature(e: ElementRef) -> String {
    let one = |e: ElementRef| {
        let cls = e.value().attr("class").unwrap_or("");
        // Drop per-item classes like "card-3" or hashed utility names that differ between cards.
        let cls: Vec<&str> = cls.split_whitespace().filter(|c| !c.chars().any(|ch| ch.is_ascii_digit())).take(4).collect();
        format!("{}.{}", e.value().name(), cls.join("."))
    };
    let parent = e.parent().and_then(ElementRef::wrap).map(one).unwrap_or_default();
    format!("{}<{}", one(e), parent)
}

/// The short line describing the person: the next sibling, or what the card says besides the name.
fn title_near(e: ElementRef, name: &str) -> Option<String> {
    let ok = |t: &str| (2..=110).contains(&t.len()) && !names::looks_like_name(t) && !t.eq_ignore_ascii_case(name) && !t.contains("http");
    for sib in e.next_siblings().filter_map(ElementRef::wrap).take(2) {
        let t = text_of(sib);
        if ok(&t) {
            return Some(t);
        }
    }
    let mut cur = e;
    for _ in 0..2 {
        let p = cur.parent().and_then(ElementRef::wrap)?;
        let all = text_of(p);
        if all.len() > 200 {
            return None;
        }
        let rest = squash(&all.replacen(name, " ", 1)).trim_matches(|c: char| c == ',' || c == '-' || c == '|' || c.is_whitespace()).to_string();
        if ok(&rest) {
            return Some(rest);
        }
        cur = p;
    }
    None
}

fn linkedin_near(e: ElementRef) -> Option<String> {
    let a_li = sel(r#"a[href*="linkedin.com/in/"]"#);
    let me = signature(e);
    let mut cur = e;
    for _ in 0..4 {
        let p = cur.parent().and_then(ElementRef::wrap)?;
        // Past the card: this container holds other people's names too.
        if p.descendants().filter_map(ElementRef::wrap).filter(|d| d.value().name() == e.value().name() && signature(*d) == me).count() > 1 {
            return None;
        }
        let links: Vec<_> = p.select(&a_li).filter_map(|a| a.value().attr("href")).collect();
        match links.len() {
            0 => cur = p,
            1 => return Some(links[0].to_string()),
            _ => return None, // this container holds several people
        }
    }
    None
}

fn card_people(doc: &Html) -> Vec<PersonIn> {
    let s = sel("h1,h2,h3,h4,h5,h6,strong,b,a,p,span,div,li,figcaption,dt,td,em");
    let mut groups: HashMap<String, Vec<PersonIn>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for e in doc.select(&s) {
        let t = text_of(e);
        if t.len() > 48 || !names::looks_like_name(&t) {
            continue;
        }
        // Prefer the innermost element carrying the name.
        if is_container(e) || e.children().filter_map(ElementRef::wrap).any(|c| text_of(c) == t) {
            continue;
        }
        if in_chrome(e) {
            continue;
        }
        let sig = signature(e);
        if !groups.contains_key(&sig) {
            order.push(sig.clone());
        }
        let title = title_near(e, &t);
        groups.entry(sig).or_default().push(PersonIn { name: t, title, linkedin: linkedin_near(e), ..Default::default() });
    }
    let mut out = Vec::new();
    for sig in order {
        let g = &groups[&sig];
        let with_li = g.iter().filter(|p| p.linkedin.is_some()).count();
        if g.len() >= 3 || (g.len() >= 2 && with_li == g.len()) {
            out.extend(g.iter().cloned());
        }
    }
    out
}

static PROFILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^/(?:[a-z-]+/)?(?:companies|company|portfolio|investments|startups)/([a-z0-9][a-z0-9_-]{1,60})/?$").unwrap());

fn titlecase(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The leading words of `text` that spell `slug`: ("Stripe Payments infrastructure", "stripe") → "Stripe".
fn name_spelling(text: &str, slug: &str) -> Option<String> {
    let want: String = slug.chars().filter(char::is_ascii_alphanumeric).collect();
    let mut got = String::new();
    let mut words = Vec::new();
    for w in text.split_whitespace().take(6) {
        words.push(w);
        got.extend(w.to_lowercase().chars().filter(char::is_ascii_alphanumeric));
        if got == want {
            return Some(words.join(" "));
        }
        if !want.starts_with(&got) {
            return None;
        }
    }
    None
}

/// Names a page might give itself that aren't the organization's name.
fn generic_site_name(s: &str) -> bool {
    let k = names::org_key(s);
    k.len() < 2 || ["team", "our team", "home", "about", "about us", "people", "our people", "portfolio", "companies", "speakers", "leadership"].contains(&k.as_str())
}

fn companies(doc: &Html, page: &Url) -> Vec<(String, String)> {
    let page_host = page.host_str().unwrap_or_default().trim_start_matches("www.").to_string();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for a in doc.select(&sel("a[href]")) {
        if in_chrome(a) {
            continue;
        }
        let Some(u) = a.value().attr("href").and_then(|h| page.join(h).ok()) else { continue };
        let Some(host) = u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()) else { continue };
        if !matches!(u.scheme(), "http" | "https") {
            continue;
        }
        if host == page_host || host.ends_with(&format!(".{page_host}")) {
            // The fund's own profile page for a company: /companies/stripe.
            if let Some(c) = PROFILE.captures(u.path()) {
                let slug = c[1].to_lowercase();
                if seen.insert(format!("{page_host}/{slug}")) {
                    let name = name_spelling(&text_of(a), &slug).unwrap_or_else(|| titlecase(&slug.replace(['-', '_'], " ")));
                    out.push((name, u.to_string()));
                }
            }
            continue;
        }
        if SKIP_HOSTS.iter().any(|s| host == *s || host.ends_with(&format!(".{s}"))) || !seen.insert(host.clone()) {
            continue;
        }
        let text = text_of(a);
        let alt = a.select(&sel("img[alt]")).find_map(|i| i.value().attr("alt").map(squash)).filter(|s| !s.is_empty());
        let generic = |s: &str| ["website", "visit", "learn more", "read more", "link", "logo", "site", "here"].iter().any(|g| s.to_lowercase().contains(g));
        let name = [Some(text), alt]
            .into_iter()
            .flatten()
            .map(|s| s.trim_end_matches(" logo").trim_end_matches(" Logo").to_string())
            .find(|s| (2..=40).contains(&s.len()) && !generic(s))
            .unwrap_or_else(|| {
                let label = host.split('.').next().unwrap_or(&host);
                let mut c = label.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            });
        out.push((name, format!("{}://{}/", u.scheme(), u.host_str().unwrap_or_default())));
    }
    out
}

/// Everything usable on one page.
pub fn extract(raw: &str, page_url: &str) -> Extracted {
    let mut ex = Extracted::default();
    ld_people(raw, &mut ex.people, &mut ex.site_name);
    let doc = Html::parse_document(raw);
    if ex.site_name.is_none() {
        ex.site_name = doc.select(&sel(r#"meta[property="og:site_name"]"#)).find_map(|m| m.value().attr("content").map(squash)).filter(|s| !s.is_empty());
    }
    if ex.site_name.is_none() {
        // "Our Team | Sequoia Capital": the brand is the last segment.
        let title = doc.select(&sel("title")).next().map(text_of).unwrap_or_default();
        let parts: Vec<&str> = title.split(['|', '—', '–', '·']).flat_map(|p| p.split(" - ")).map(str::trim).filter(|p| !p.is_empty()).collect();
        if parts.len() >= 2 && (2..=40).contains(&parts[parts.len() - 1].len()) && !generic_site_name(parts[parts.len() - 1]) {
            ex.site_name = Some(parts[parts.len() - 1].to_string());
        }
    }
    ex.site_name = ex.site_name.filter(|n| !generic_site_name(n));
    let mut seen: HashSet<String> = ex.people.iter().map(|p| names::person_key(&p.name)).collect();
    for p in card_people(&doc) {
        if seen.insert(names::person_key(&p.name)) {
            ex.people.push(p);
        }
    }
    if let Ok(u) = Url::parse(page_url) {
        ex.companies = companies(&doc, &u);
    }
    ex
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HarvestOptions {
    /// The organization the page belongs to. Default: the site's own name.
    pub org: Option<String>,
    pub relation: Relation,
    /// Pages to read: 1 reads just this page; more crawls below it.
    pub limit: usize,
    pub include_paths: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarvestReport {
    pub org: String,
    pub pages: usize,
    pub errors: usize,
    pub people: usize,
    pub companies: usize,
    pub new_edges: usize,
    pub sample: Vec<String>,
    pub elapsed_ms: u64,
}

fn org_from_host(u: &str) -> String {
    let host = Url::parse(u).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let host = host.trim_start_matches("www.");
    let label = host.split('.').next().unwrap_or(host);
    let mut c = label.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_else(|| host.to_string())
}

/// Records one page's people and companies under `org`.
pub fn ingest(graph: &Graph, ex: &Extracted, org: &str, relation: Relation, page_url: &str, rep: &mut HarvestReport) -> Result<()> {
    let relation = relation.for_url(page_url);
    let source = format!("web:{}", Url::parse(page_url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default());
    let home = Url::parse(page_url).ok().map(|u| format!("{}://{}/", u.scheme(), u.host_str().unwrap_or_default()));
    graph.batch(|t| {
        let org_id = t.org_at(org, home.as_deref())?;
        for p in &ex.people {
            let (title, title_org) = p.title.as_deref().map(split_title).map(|(a, b)| (Some(a), b)).unwrap_or((None, None));
            // On a team page everyone works at the org; elsewhere their own employer is in the title.
            let employer = match relation {
                Relation::Team => p.org.clone().or(Some(org.to_string())),
                _ => p.org.clone().or(title_org.clone()),
            };
            let person = PersonIn { title: title.clone().or(p.title.clone()), org: employer.clone(), ..p.clone() };
            let Ok(id) = t.person(&person) else { continue };
            rep.people += 1;
            if rep.sample.len() < 12 {
                rep.sample.push(match &person.title {
                    Some(tl) => format!("{} — {tl}", person.name),
                    None => person.name.clone(),
                });
            }
            let evidence = format!("{} on {page_url}", person.title.as_deref().unwrap_or("listed"));
            let kind = if relation == Relation::Team { EdgeKind::WorksAt } else { EdgeKind::ListedOn };
            rep.new_edges += t.link(id, org_id, kind, None, &source, Some(&evidence))? as usize;
            if relation != Relation::Team
                && let Some(e) = &employer
                && names::org_key(e) != names::org_key(org)
            {
                let o = t.org(e)?;
                rep.new_edges += t.link(id, o, EdgeKind::WorksAt, None, &source, Some(&evidence))? as usize;
            }
        }
        if relation == Relation::Portfolio {
            for (name, home) in &ex.companies {
                // A fund's own /companies/x page says nothing about the company's domain.
                let own = Url::parse(home).ok().and_then(|u| u.host_str().map(str::to_string)) == Url::parse(page_url).ok().and_then(|u| u.host_str().map(str::to_string));
                let c = t.org_at(name, (!own).then_some(home.as_str()))?;
                rep.companies += 1;
                rep.new_edges += t.link(org_id, c, EdgeKind::InvestedIn, None, &source, Some(&format!("{org} portfolio: {page_url}")))? as usize;
            }
        }
        Ok(())
    })
}

/// Reads `url` (and, with `limit` > 1, the pages below it) and adds who it lists to the graph.
pub async fn harvest(engine: Arc<Engine>, graph: &Graph, url: &str, opts: &HarvestOptions) -> Result<HarvestReport> {
    let t0 = std::time::Instant::now();
    if Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.ends_with("linkedin.com"))).unwrap_or(false) {
        bail!("LinkedIn pages aren't harvested (it breaks LinkedIn's terms and gets accounts restricted). Import the Connections.csv from LinkedIn's data export instead: `ferrocrawl intro import <file>`");
    }
    let scrape = ScrapeOptions { formats: vec![Format::RawHtml], max_age_ms: 600_000, ..Default::default() };
    let mut rep = HarvestReport::default();
    let mut org = opts.org.clone();
    let mut handle = |raw: &str, page: &str, rep: &mut HarvestReport| -> Result<()> {
        let ex = extract(raw, page);
        let o = org.get_or_insert_with(|| ex.site_name.clone().unwrap_or_else(|| org_from_host(page))).clone();
        rep.org = o.clone();
        rep.pages += 1;
        ingest(graph, &ex, &o, opts.relation, page, rep)
    };
    if opts.limit <= 1 {
        let doc = engine.scrape(url, &scrape).await?;
        handle(doc.raw_html.as_deref().unwrap_or_default(), &doc.metadata.url, &mut rep)?;
    } else {
        let copts = CrawlOptions { limit: opts.limit, include_paths: opts.include_paths.clone(), scrape, ..Default::default() };
        let (tx, mut rx) = mpsc::channel(64);
        let job = tokio::spawn(ferrocrawl::crawl(engine, url.to_string(), copts, tx));
        while let Some(ev) = rx.recv().await {
            match ev {
                CrawlEvent::Page { document, .. } => {
                    if let Err(e) = handle(document.raw_html.as_deref().unwrap_or_default(), &document.metadata.url, &mut rep) {
                        tracing::warn!("{}: {e:#}", document.metadata.url);
                    }
                }
                CrawlEvent::Error { .. } => rep.errors += 1,
            }
        }
        job.await??;
    }
    rep.elapsed_ms = t0.elapsed().as_millis() as u64;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &str = r#"<html><head><meta property="og:site_name" content="Acme Ventures"></head><body>
      <nav><a href="/">Home</a><span>Our Team</span></nav>
      <main><h1>Our Team</h1>
        <div class="card"><h3 class="name">Jane Doe</h3><p class="role">Managing Partner</p><a href="https://www.linkedin.com/in/janedoe">in</a></div>
        <div class="card"><h3 class="name">Raj Patel</h3><p class="role">Partner</p></div>
        <div class="card"><h3 class="name">Ana María López</h3><p class="role">Principal</p></div>
        <p>Read More</p>
        <a href="https://bolt.example.io/">Bolt</a><a href="https://twitter.com/acme">Twitter</a>
      </main><footer><span>Privacy Policy</span></footer></body></html>"#;

    #[test]
    fn reads_team_cards() {
        let ex = extract(TEAM, "https://acme.vc/team");
        assert_eq!(ex.site_name.as_deref(), Some("Acme Ventures"));
        let names: Vec<_> = ex.people.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Jane Doe", "Raj Patel", "Ana María López"]);
        assert_eq!(ex.people[0].title.as_deref(), Some("Managing Partner"));
        assert_eq!(ex.people[0].linkedin.as_deref(), Some("https://www.linkedin.com/in/janedoe"));
        assert_eq!(ex.people[1].linkedin, None);
        assert_eq!(ex.companies, [("Bolt".to_string(), "https://bolt.example.io/".to_string())]);
    }

    #[test]
    fn reads_portfolio_profile_links() {
        let html = r#"<title>Team</title><main><a href="/companies/stripe">Stripe Payments infrastructure</a><a href="/companies/door-dash">DoorDash</a><a href="/about">About</a></main>"#;
        let ex = extract(html, "https://fund.vc/our-companies/");
        let names: Vec<_> = ex.companies.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(names, ["Stripe", "DoorDash"]);
        assert_eq!(ex.site_name, None);
    }

    #[test]
    fn reads_json_ld() {
        let html = r#"<script type="application/ld+json">{"@type":"Organization","name":"Beta Fund","employee":[
            {"@type":"Person","name":"Kim Chan","jobTitle":"GP","sameAs":["https://linkedin.com/in/kimchan"]}]}</script>"#;
        let ex = extract(html, "https://beta.fund/about");
        assert_eq!(ex.site_name.as_deref(), Some("Beta Fund"));
        assert_eq!(ex.people[0].name, "Kim Chan");
        assert_eq!(ex.people[0].org.as_deref(), Some("Beta Fund"));
    }

    #[test]
    fn titles() {
        assert_eq!(split_title("Partner at Sequoia"), ("Partner".into(), Some("Sequoia".into())));
        assert_eq!(split_title("CEO, Acme Inc"), ("CEO".into(), Some("Acme Inc".into())));
        assert_eq!(split_title("Managing Partner"), ("Managing Partner".into(), None));
    }
}
