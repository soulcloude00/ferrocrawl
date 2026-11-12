//! Public scrape benchmarks, run the way their authors run them.
//!
//! - **TinyFish 45**: TinyFish's fetch-quality set (docs, pricing, repos, articles, news). Usable =
//!   at least 1,000 characters and an LLM-judged signal ratio above 30%.
//! - **WebCrawlerAPI**: Firecrawl's public `scrape-content-dataset-v1` (1,000 rows) with each row's
//!   `truth_text` / `lie_text` token overlap: coverage, content recall and an F1 proxy. Fully
//!   deterministic, scored here.
//! - **TRACE**: context.dev's weekly sample (2,527 pages on 2026-11-06). An LLM judge decides
//!   `substantive` + `match`; the score is the mean of per-category success rates.
//!
//! Every run saves each page's Markdown, so the LLM-judged suites can be graded afterwards and the
//! verdicts merged with [`summarize`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{Engine, Format, ScrapeOptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Suite {
    Tinyfish,
    Webcrawlerapi,
    Trace,
}

impl Suite {
    pub const ALL: [Suite; 3] = [Suite::Tinyfish, Suite::Webcrawlerapi, Suite::Trace];

    pub fn name(self) -> &'static str {
        match self {
            Suite::Tinyfish => "TinyFish 45",
            Suite::Webcrawlerapi => "WebCrawlerAPI",
            Suite::Trace => "TRACE",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Suite::Tinyfish => "tinyfish",
            Suite::Webcrawlerapi => "webcrawlerapi",
            Suite::Trace => "trace",
        }
    }

    pub fn parse(s: &str) -> Option<Suite> {
        Suite::ALL.into_iter().find(|x| x.slug() == s)
    }

    /// The suite's headline quality metric needs an LLM judge.
    pub fn judged(self) -> bool {
        !matches!(self, Suite::Webcrawlerapi)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Case {
    pub id: String,
    pub url: String,
    pub category: String,
    /// WebCrawlerAPI: ~100 words that must appear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truth: Option<String>,
    /// WebCrawlerAPI: ~10 words of page chrome that must not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lie: Option<String>,
    /// Rows the dataset itself marks broken are skipped, as the original runner does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CaseResult {
    pub index: usize,
    pub id: String,
    pub url: String,
    pub category: String,
    /// `ok`, `error` or `skipped`.
    pub status: String,
    pub ms: u64,
    pub chars: usize,
    pub rendered: bool,
    pub final_url: Option<String>,
    pub title: Option<String>,
    pub error: Option<String>,
    pub recall: Option<f64>,
    pub f1: Option<f64>,
    pub covered: Option<bool>,
    pub lie_found: Option<bool>,
    /// Network (or browser) time and extraction time, as the engine reports them.
    pub fetch_ms: u64,
    pub extract_ms: u64,
}

/// An LLM judge's verdict on one saved page (TRACE's rubric, also used for TinyFish's signal ratio).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verdict {
    pub id: String,
    /// substantive | thin | blocked | error | unclear
    pub content: String,
    /// match | wrong_target | unknown
    pub identity: String,
    /// Share of the text that is the page's own content, 0–100 (TinyFish's signal ratio).
    #[serde(default)]
    pub signal: Option<f64>,
}

pub enum BenchEvent {
    Loaded { total: usize },
    Case(Box<CaseResult>),
}

// ------------------------------------------------------------------ datasets

const WCA_DATASET: &str = "https://huggingface.co/datasets/firecrawl/scrape-content-dataset-v1/raw/main/1-0-0.csv";
pub const TRACE_RUN: &str = "scraping-bench-v5:2026-11-06";

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("snarf").join("bench")
}

async fn cached_download(engine: &Engine, url: &str, name: &str) -> Result<String> {
    let path = cache_dir().join(name);
    if let Ok(s) = tokio::fs::read_to_string(&path).await {
        return Ok(s);
    }
    let resp = engine.http().get(url).send().await.with_context(|| format!("downloading {url}"))?;
    if !resp.status().is_success() {
        bail!("downloading {url}: HTTP {}", resp.status());
    }
    let body = resp.text().await?;
    tokio::fs::create_dir_all(cache_dir()).await?;
    tokio::fs::write(&path, &body).await?;
    Ok(body)
}

pub async fn load(engine: &Engine, suite: Suite) -> Result<Vec<Case>> {
    match suite {
        Suite::Tinyfish => Ok(TINYFISH
            .iter()
            .enumerate()
            .map(|(i, (cat, url))| Case {
                id: format!("{i:02}"),
                url: url.to_string(),
                category: cat.to_string(),
                truth: None,
                lie: None,
                skip: None,
            })
            .collect()),
        Suite::Webcrawlerapi => {
            let csv_text = cached_download(engine, WCA_DATASET, "webcrawlerapi-1-0-0.csv").await?;
            let mut rd = csv::ReaderBuilder::new().flexible(true).from_reader(csv_text.as_bytes());
            let mut out = Vec::new();
            for rec in rd.records() {
                let rec = rec?;
                let get = |i: usize| rec.get(i).unwrap_or("").to_string();
                let err = get(4);
                out.push(Case {
                    id: get(0),
                    url: get(1),
                    category: "all".into(),
                    truth: Some(get(2)),
                    lie: Some(get(3)),
                    skip: (!err.trim().is_empty()).then_some(err),
                });
            }
            Ok(out)
        }
        Suite::Trace => {
            let rows = trace_rows(engine).await?;
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for r in rows {
                if seen.insert(r.url.clone()) {
                    out.push(Case { id: r.id.clone(), url: r.url, category: r.category, truth: None, lie: None, skip: None });
                }
            }
            Ok(out)
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct TraceRow {
    id: String,
    url: String,
    category: String,
    provider: String,
    success: Option<bool>,
    #[serde(rename = "elapsedMs")]
    elapsed_ms: Option<u64>,
}

async fn trace_rows(engine: &Engine) -> Result<Vec<TraceRow>> {
    let url = format!("https://trace.context.dev/api/download?run={}", TRACE_RUN.replace(':', "%3A"));
    let text = cached_download(engine, &url, "trace-v5-2026-11-06.ndjson").await?;
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| Ok(serde_json::from_str(l)?)).collect()
}

// ------------------------------------------------------------------ runner

pub struct RunOptions {
    pub concurrency: usize,
    pub limit: Option<usize>,
    /// Where pages and results are written.
    pub out_dir: PathBuf,
    /// Only cases in this category (re-running one slice of a suite).
    pub category: Option<String>,
}

/// Runs `suite`, streaming each finished case to `events`. Returns the results in dataset order.
pub async fn run(engine: Arc<Engine>, suite: Suite, opts: RunOptions, events: mpsc::Sender<BenchEvent>) -> Result<Vec<CaseResult>> {
    let mut cases = load(&engine, suite).await?;
    if let Some(c) = &opts.category {
        cases.retain(|x| &x.category == c);
    }
    if let Some(n) = opts.limit {
        cases.truncate(n);
    }
    let pages = opts.out_dir.join("pages");
    tokio::fs::create_dir_all(&pages).await?;
    tokio::fs::create_dir_all(opts.out_dir.join("raw")).await?;
    tokio::fs::write(opts.out_dir.join("cases.json"), serde_json::to_vec(&cases)?).await?;
    let _ = events.send(BenchEvent::Loaded { total: cases.len() }).await;

    let scrape = ScrapeOptions {
        // Raw HTML is kept beside the Markdown so extraction can be re-scored offline.
        formats: vec![Format::Markdown, Format::RawHtml],
        max_age_ms: 0,
        timeout_ms: 60_000,
        ..Default::default()
    };
    let cases = Arc::new(cases);
    let mut results: Vec<Option<CaseResult>> = vec![None; cases.len()];
    let mut inflight = FuturesUnordered::new();
    let mut next = 0;
    let mut log = String::new();
    let started = Instant::now();
    loop {
        while inflight.len() < opts.concurrency.max(1) && next < cases.len() {
            let (i, engine, cases, scrape, pages) = (next, engine.clone(), cases.clone(), scrape.clone(), pages.clone());
            next += 1;
            inflight.push(async move { run_case(&engine, i, &cases[i], &scrape, &pages).await });
        }
        let Some(r) = inflight.next().await else { break };
        log.push_str(&serde_json::to_string(&r)?);
        log.push('\n');
        let i = r.index;
        results[i] = Some(r.clone());
        if events.send(BenchEvent::Case(Box::new(r))).await.is_err() {
            break;
        }
    }
    tokio::fs::write(opts.out_dir.join("results.jsonl"), log).await?;
    let meta = serde_json::json!({ "elapsed_ms": started.elapsed().as_millis() as u64, "concurrency": opts.concurrency });
    tokio::fs::write(opts.out_dir.join("meta.json"), meta.to_string()).await?;
    Ok(results.into_iter().flatten().collect())
}

async fn run_case(engine: &Engine, index: usize, case: &Case, scrape: &ScrapeOptions, pages: &Path) -> CaseResult {
    let mut r = CaseResult { index, id: case.id.clone(), url: case.url.clone(), category: case.category.clone(), ..Default::default() };
    if let Some(e) = &case.skip {
        r.status = "skipped".into();
        r.error = Some(e.clone());
        return r;
    }
    let t0 = Instant::now();
    match engine.scrape(&case.url, scrape).await {
        Ok(doc) => {
            r.ms = t0.elapsed().as_millis() as u64;
            let md = doc.markdown.unwrap_or_default();
            r.status = "ok".into();
            r.chars = md.len();
            r.rendered = doc.metadata.rendered;
            r.final_url = Some(doc.metadata.url);
            r.title = doc.metadata.title;
            r.fetch_ms = doc.metadata.fetch_ms;
            r.extract_ms = doc.metadata.extract_ms;
            if let Some(raw) = doc.raw_html.filter(|h| !h.is_empty()) {
                let _ = tokio::fs::write(pages.with_file_name("raw").join(format!("{index:05}.html")), raw).await;
            }
            if let Some(truth) = &case.truth {
                let s = score_overlap(&md, truth, case.lie.as_deref().unwrap_or(""));
                r.recall = Some(s.recall);
                r.f1 = Some(s.f1);
                r.covered = Some(s.recall >= 0.1);
                r.lie_found = Some(s.lie_found);
            }
            let _ = tokio::fs::write(pages.join(format!("{index:05}.md")), md).await;
        }
        Err(e) => {
            r.ms = t0.elapsed().as_millis() as u64;
            r.status = "error".into();
            r.error = Some(format!("{e:#}"));
        }
    }
    r
}

// ------------------------------------------------------------------ WebCrawlerAPI scoring (a port of benchmark.mjs)

static MD_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap());
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}]+").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

fn normalize(text: &str) -> String {
    let t = MD_LINK.replace_all(text, "$1").to_lowercase();
    SPACES.replace_all(&t, " ").trim().to_string()
}

fn tokens(text: &str) -> Vec<String> {
    NON_WORD.split(&normalize(text)).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

fn overlap(haystack: &HashSet<String>, needle: &str) -> f64 {
    let n = tokens(needle);
    if n.is_empty() {
        return 0.0;
    }
    n.iter().filter(|t| haystack.contains(*t)).count() as f64 / n.len() as f64
}

pub struct Overlap {
    pub recall: f64,
    pub f1: f64,
    pub lie_found: bool,
}

pub fn score_overlap(markdown: &str, truth: &str, lie: &str) -> Overlap {
    let hay: HashSet<String> = tokens(markdown).into_iter().collect();
    let recall = overlap(&hay, truth);
    let precision = 1.0 - overlap(&hay, lie);
    let f1 = if recall + precision > 0.0 { 2.0 * recall * precision / (recall + precision) } else { 0.0 };
    let lie_found = !lie.is_empty() && normalize(markdown).contains(&normalize(lie));
    Overlap { recall, f1, lie_found }
}

// ------------------------------------------------------------------ summaries

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Latency {
    pub p50: u64,
    pub p90: u64,
    pub p95: u64,
    pub p99: u64,
    pub mean: u64,
}

impl Latency {
    pub fn of(mut v: Vec<u64>) -> Self {
        if v.is_empty() {
            return Self::default();
        }
        v.sort_unstable();
        // Nearest-rank, as benchmark.mjs computes it.
        let p = |q: f64| v[((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
        Latency { p50: p(0.5), p90: p(0.9), p95: p(0.95), p99: p(0.99), mean: v.iter().sum::<u64>() / v.len() as u64 }
    }
}

/// One row of a leaderboard: a provider's published numbers, or ours.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub ours: bool,
    /// Headline score in percent (TRACE useful content, WebCrawlerAPI coverage, TinyFish usable).
    pub score: Option<f64>,
    pub recall: Option<f64>,
    pub f1: Option<f64>,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub suite: Option<Suite>,
    pub total: usize,
    pub done: usize,
    pub ok: usize,
    pub errors: usize,
    pub skipped: usize,
    pub rendered: usize,
    pub latency: Latency,
    /// Pages with an LLM verdict (judged suites).
    pub judged: usize,
    pub score: Option<f64>,
    pub recall: Option<f64>,
    pub f1: Option<f64>,
    /// Category → (passed, attempted).
    pub categories: BTreeMap<String, (usize, usize)>,
    pub board: Vec<Entry>,
}

/// Scores results the suite's way, merging LLM verdicts when there are any.
pub fn summarize(suite: Suite, total: usize, results: &[CaseResult], verdicts: &HashMap<String, Verdict>) -> Summary {
    let mut s = Summary { suite: Some(suite), total, done: results.len(), ..Default::default() };
    for r in results {
        match r.status.as_str() {
            "ok" => s.ok += 1,
            "error" => s.errors += 1,
            _ => s.skipped += 1,
        }
        s.rendered += usize::from(r.rendered);
    }
    // Every timed request counts, failures included, as TRACE and WebCrawlerAPI do.
    s.latency = Latency::of(results.iter().filter(|r| r.status != "skipped").map(|r| r.ms).collect());
    let verdict = |r: &CaseResult| verdicts.get(&r.id);
    s.judged = results.iter().filter(|r| verdict(r).is_some()).count();

    match suite {
        Suite::Webcrawlerapi => {
            let scored: Vec<&CaseResult> = results.iter().filter(|r| r.status == "ok").collect();
            let n = scored.len().max(1) as f64;
            if !scored.is_empty() {
                s.score = Some(100.0 * scored.iter().filter(|r| r.covered == Some(true)).count() as f64 / n);
                s.recall = Some(100.0 * scored.iter().filter_map(|r| r.recall).sum::<f64>() / n);
                s.f1 = Some(scored.iter().filter_map(|r| r.f1).sum::<f64>() / n);
            }
            let attempted = results.iter().filter(|r| r.status != "skipped").count();
            let covered = results.iter().filter(|r| r.covered == Some(true)).count();
            s.categories.insert("end-to-end".into(), (covered, attempted));
        }
        Suite::Trace => {
            for r in results {
                let pass = r.status == "ok" && verdict(r).is_some_and(|v| v.content == "substantive" && v.identity == "match");
                let e = s.categories.entry(r.category.clone()).or_default();
                e.0 += usize::from(pass);
                e.1 += 1;
            }
            if s.judged > 0 && s.done == s.total {
                s.score = Some(mean_rate(&s.categories));
            }
        }
        Suite::Tinyfish => {
            for r in results {
                let signal_ok = verdict(r).and_then(|v| v.signal).is_some_and(|x| x > 30.0);
                let pass = r.status == "ok" && r.chars >= 1000 && signal_ok;
                let e = s.categories.entry(r.category.clone()).or_default();
                e.0 += usize::from(pass);
                e.1 += 1;
            }
            if s.judged > 0 {
                let (p, n) = s.categories.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                s.score = Some(100.0 * p as f64 / n.max(1) as f64);
            }
        }
    }
    s
}

fn mean_rate(cats: &BTreeMap<String, (usize, usize)>) -> f64 {
    let rates: Vec<f64> = cats.values().filter(|(_, n)| *n > 0).map(|(p, n)| *p as f64 / *n as f64).collect();
    100.0 * rates.iter().sum::<f64>() / rates.len().max(1) as f64
}

/// The published numbers of the other providers, for the leaderboard.
pub async fn board(engine: &Engine, suite: Suite) -> Vec<Entry> {
    let e = |name: &str, score: f64, recall: Option<f64>, f1: Option<f64>, p50: Option<u64>, p95: Option<u64>| Entry {
        name: name.into(),
        ours: false,
        score: Some(score),
        recall,
        f1,
        p50_ms: p50,
        p95_ms: p95,
    };
    match suite {
        // From github.com/WebCrawlerAPI/scraping-benchmark results/*/results.json (default settings,
        // cache off; context.dev also with its main-content option).
        Suite::Webcrawlerapi => vec![
            e("context.dev", 94.92, Some(82.54), Some(0.1753), Some(3065), Some(17154)),
            e("context.dev (main content)", 92.62, Some(78.76), Some(0.5669), Some(3561), Some(29805)),
            e("WebCrawlerAPI", 92.6, Some(72.59), Some(0.6218), Some(8295), Some(49277)),
            e("Crawl4AI Cloud", 92.26, Some(75.66), Some(0.5605), Some(1604), Some(31010)),
            e("Firecrawl", 89.03, Some(74.94), Some(0.515), Some(3548), Some(15957)),
            e("Spider", 88.7, Some(73.04), Some(0.5202), Some(5755), Some(94763)),
            e("Jina Reader", 87.43, Some(73.53), Some(0.1783), Some(5388), Some(20505)),
            e("Crawlbase", 74.87, Some(46.55), Some(0.4646), Some(9841), Some(130494)),
        ],
        // Recomputed from TRACE's page-level download, which reproduces its published table.
        Suite::Trace => {
            let Ok(rows) = trace_rows(engine).await else { return Vec::new() };
            let mut out = Vec::new();
            for (key, name) in [("context", "context.dev"), ("keenable", "Keenable"), ("firecrawl", "Firecrawl"), ("exa", "Exa")] {
                let mut cats: BTreeMap<String, (usize, usize)> = BTreeMap::new();
                let mut ms = Vec::new();
                for r in rows.iter().filter(|r| r.provider == key) {
                    let c = cats.entry(r.category.clone()).or_default();
                    c.0 += usize::from(r.success == Some(true));
                    c.1 += 1;
                    if let Some(m) = r.elapsed_ms {
                        ms.push(m);
                    }
                }
                let lat = Latency::of(ms);
                out.push(e(name, mean_rate(&cats), None, None, Some(lat.p50), Some(lat.p95)));
            }
            out
        }
        // TinyFish's own published run (coverage only; they publish no fetch latency per page).
        Suite::Tinyfish => vec![
            e("TinyFish", 93.3, None, None, None, None),
            e("Tavily", 80.0, None, None, None, None),
            e("Exa", 73.3, None, None, None, None),
            e("Firecrawl", 62.2, None, None, None, None),
            e("Parallel", 57.8, None, None, None, None),
        ],
    }
}

/// Re-extracts a run's saved raw HTML with the current extractor and re-scores it, offline.
/// Returns (main-content results, full-page results) for rows that have raw HTML.
pub fn rescore(dir: &Path) -> Result<(Vec<CaseResult>, Vec<CaseResult>)> {
    let fresh = dir.join("pages-rescored");
    std::fs::create_dir_all(&fresh)?;
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(dir.join("cases.json"))?)?;
    let text = std::fs::read_to_string(dir.join("results.jsonl"))?;
    let (mut main, mut full) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let Ok(r) = serde_json::from_str::<CaseResult>(line) else { continue };
        let raw = std::fs::read_to_string(dir.join("raw").join(format!("{:05}.html", r.index)));
        let (Ok(raw), Some(case)) = (raw, cases.get(r.index)) else {
            // No raw HTML (errors, PDFs): keep the row as it was in both views.
            main.push(r.clone());
            full.push(r);
            continue;
        };
        let url = r.final_url.clone().unwrap_or_else(|| r.url.clone());
        for (only_main, out) in [(true, &mut main), (false, &mut full)] {
            let ex = crate::extract::extract(&raw, &url, only_main, &[]);
            if only_main {
                let _ = std::fs::write(fresh.join(format!("{:05}.md", r.index)), &ex.markdown);
            }
            let mut n = r.clone();
            n.chars = ex.markdown.len();
            if let Some(truth) = &case.truth {
                let s = score_overlap(&ex.markdown, truth, case.lie.as_deref().unwrap_or(""));
                n.recall = Some(s.recall);
                n.f1 = Some(s.f1);
                n.covered = Some(s.recall >= 0.1);
                n.lie_found = Some(s.lie_found);
            }
            out.push(n);
        }
    }
    Ok((main, full))
}

/// Where runs are saved: `~/.local/share/snarf/bench/<suite>-<name>`.
pub fn runs_root() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("snarf").join("bench")
}

/// A saved run: its suite, case count and results.
pub fn load_run(dir: &Path) -> Result<(Suite, usize, Vec<CaseResult>)> {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let suite = Suite::ALL.into_iter().find(|s| name.starts_with(s.slug())).context("run directory name must start with the suite")?;
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(dir.join("cases.json"))?)?;
    let text = std::fs::read_to_string(dir.join("results.jsonl"))?;
    let results = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    Ok((suite, cases.len(), results))
}

/// Wall time of a saved run, from its `meta.json`.
pub fn run_elapsed_ms(dir: &Path) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()?;
    v.get("elapsed_ms")?.as_u64()
}

/// The most recently finished run of `suite`, if any.
pub fn latest_run(suite: Suite) -> Option<PathBuf> {
    let entries = std::fs::read_dir(runs_root()).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&format!("{}-", suite.slug())))
                && p.join("results.jsonl").exists()
        })
        .max_by_key(|p| std::fs::metadata(p.join("results.jsonl")).and_then(|m| m.modified()).ok())
}

pub fn read_verdicts(dir: &Path) -> HashMap<String, Verdict> {
    let Ok(text) = std::fs::read_to_string(dir.join("verdicts.jsonl")) else { return HashMap::new() };
    text.lines().filter_map(|l| serde_json::from_str::<Verdict>(l).ok()).map(|v| (v.id.clone(), v)).collect()
}

/// TRACE's evidence rule: the whole text up to 12,000 characters, else three 4,000-character
/// excerpts from the start, middle and end.
pub fn excerpts(text: &str) -> String {
    if text.len() <= 12_000 {
        return text.to_string();
    }
    let at = |i: usize| {
        let mut a = i.min(text.len());
        while !text.is_char_boundary(a) {
            a -= 1;
        }
        let mut b = (a + 4000).min(text.len());
        while !text.is_char_boundary(b) {
            b -= 1;
        }
        format!("[Offset {a}]\n{}", &text[a..b])
    };
    [0, (text.len() - 4000) / 2, text.len() - 4000].map(at).join("\n")
}

const TINYFISH: [(&str, &str); 45] = [
    ("docs", "https://developers.openai.com/api/docs/guides/streaming-responses"),
    ("docs", "https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview"),
    ("docs", "https://ai-sdk.dev/docs/ai-sdk-core/tools-and-tool-calling"),
    ("docs", "https://playwright.dev/docs/locators"),
    ("docs", "https://nextjs.org/docs/app/building-your-application/routing/route-handlers"),
    ("docs", "https://developers.cloudflare.com/durable-objects/"),
    ("docs", "https://docs.stripe.com/webhooks/signature"),
    ("docs", "https://fastapi.tiangolo.com/tutorial/background-tasks/"),
    ("docs", "https://www.postgresql.org/docs/current/datatype-json.html"),
    ("docs", "https://www.python-httpx.org/advanced/timeouts/"),
    ("pricing", "https://openai.com/api/pricing/"),
    ("pricing", "https://www.anthropic.com/pricing"),
    ("pricing", "https://vercel.com/pricing"),
    ("pricing", "https://stripe.com/pricing"),
    ("pricing", "https://supabase.com/pricing"),
    ("pricing", "https://github.com/features/copilot/plans"),
    ("repo", "https://github.com/langchain-ai/langgraph"),
    ("repo", "https://github.com/vercel/ai"),
    ("repo", "https://github.com/microsoft/playwright"),
    ("repo", "https://github.com/fastapi/fastapi"),
    ("repo", "https://github.com/duckdb/duckdb"),
    ("article", "https://www.cloudflare.com/learning/cdn/what-is-a-cdn/"),
    ("article", "https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API"),
    ("article", "https://en.wikipedia.org/wiki/Web_scraping"),
    ("article", "https://openai.com/index/hello-gpt-4o/"),
    ("article", "https://www.anthropic.com/news/claude-3-5-sonnet"),
    ("simple", "https://quotes.toscrape.com/"),
    ("simple", "https://books.toscrape.com/"),
    ("status", "https://status.openai.com/"),
    ("status", "https://www.githubstatus.com/"),
    ("news", "https://www.dailymail.co.uk/news/article-15789623/Obama-Trump-marriage-Michelle-issues.html"),
    ("news", "https://www.dailymail.co.uk/news/article-15791231/Meghan-Markle-habit-falling-Hollywoods-power-brokers-SNLs-terrorist-takedown-Met-Gala-absence-proof-duchess-burning-bridges-US-experts-say.html"),
    ("news", "https://www.dailymail.co.uk/news/article-15791581/canada-actress-claire-brosseau-assisted-suicide.html"),
    ("news", "https://www.hindustantimes.com/cricket/axar-patel-comes-straight-to-the-point-name-drops-kuldeep-yadav-after-delhi-capitals-stand-on-cusp-of-elimination-101777998122728.html"),
    ("news", "https://www.hindustantimes.com/cricket/bcci-controls-the-icc-south-africa-spinner-makes-big-charge-reveals-how-narrative-can-be-changed-101777989002691.html"),
    ("news", "https://www.hindustantimes.com/cricket/exindia-cricketer-claims-tmc-denied-him-ticket-after-he-refused-to-pay-inr-5-crore-the-chapter-is-over-101777982797802.html"),
    ("news", "https://www.scmp.com/news/china/diplomacy/article/3352532/irans-top-diplomat-abbas-araghchi-visit-china-days-ahead-donald-trump"),
    ("news", "https://www.scmp.com/news/china/article/3352539/panama-minister-blasts-chinas-ship-crackdown-tells-deputies-demand-answers-beijing"),
    ("news", "https://www.scmp.com/tech/big-tech/article/3352200/supersized-and-scaling-china-pushes-10000-card-computing-clusters-ai-race"),
    ("news", "https://www.theguardian.com/film/2026/may/04/blake-lively-justin-baldoni-settlement-it-ends-with-us"),
    ("news", "https://www.theguardian.com/lifeandstyle/2026/may/05/kitten-rescued-glue-bucket-texas-adoption"),
    ("news", "https://www.theguardian.com/sport/2026/may/05/liv-golf-funding-cam-smith-australian"),
    ("news", "https://www.nytimes.com/2026/04/28/opinion/ezra-klein-podcast-thompson-dunkelman.html"),
    ("news", "https://www.nytimes.com/2026/04/29/technology/ai-artificial-intelligence-ad-boom.html"),
    ("news", "https://www.nytimes.com/2026/04/29/technology/ai-spending-tech-data-centers.html"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_matches_benchmark_mjs() {
        let md = "Intro [the **guide**](https://x.dev) explains Rust ownership.\n\nFooter: Privacy · Terms";
        let s = score_overlap(md, "The guide explains Rust ownership rules", "Privacy Terms");
        assert!((s.recall - 5.0 / 6.0).abs() < 1e-9, "{}", s.recall);
        assert!(!s.lie_found, "exact-phrase check, as benchmark.mjs: the · breaks the phrase");
        assert!(s.f1 < 0.01, "precision proxy is 0 when all lie tokens leak");
    }

    #[test]
    fn latency_is_nearest_rank() {
        let l = Latency::of((1..=100).collect());
        assert_eq!((l.p50, l.p95, l.p99), (50, 95, 99));
    }

    #[test]
    fn trace_score_is_mean_of_category_rates() {
        let mut c = BTreeMap::new();
        c.insert("a".to_string(), (1, 2));
        c.insert("b".to_string(), (10, 10));
        assert!((mean_rate(&c) - 75.0).abs() < 1e-9);
    }
}
