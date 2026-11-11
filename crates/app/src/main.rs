//! ferrocrawl desktop: type a URL, scrape, crawl or map it, and watch pages land live.
//! The scraping runs on a Tokio runtime beside the UI; results arrive through an inbox that
//! `poll` drains before each frame.

mod bench;
mod bench_ui;
mod intro;
mod intro_ui;
mod ui;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrocrawl::{CrawlEvent, CrawlOptions, Document, Engine, EngineConfig, Format, MapOptions, Render, ScrapeOptions};
use rune::{AppSpec, Key, KeyEvent, TextEdit, Tx, Waker};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub const MODES: [&str; 5] = ["Scrape", "Crawl", "Map", "Intro", "Benchmark"];
pub const RENDERS: [&str; 3] = ["Auto", "HTTP", "Browser"];
pub const LIMITS: [usize; 4] = [25, 100, 500, 2000];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Scrape,
    Crawl,
    Map,
    Intro,
    Bench,
}

impl Mode {
    pub fn index(self) -> usize {
        self as usize
    }
    fn from_index(i: usize) -> Self {
        [Mode::Scrape, Mode::Crawl, Mode::Map, Mode::Intro, Mode::Bench][i.min(4)]
    }
}

/// One finished page (or failure) in the results list.
pub struct Row {
    pub url: String,
    pub title: String,
    pub ok: bool,
    pub ms: u64,
    pub kb: usize,
    pub words: usize,
    pub rendered: bool,
    pub cached: bool,
    pub depth: usize,
    /// The Markdown, or the error.
    pub body: Arc<str>,
}

impl Row {
    fn page(doc: Document, depth: usize) -> Self {
        let m = doc.metadata;
        let body: Arc<str> = doc.markdown.unwrap_or_default().into();
        Row {
            title: m.title.unwrap_or_else(|| m.url.clone()),
            url: m.url,
            ok: true,
            ms: m.fetch_ms + m.extract_ms,
            kb: m.bytes / 1024,
            words: body.split_whitespace().count(),
            rendered: m.rendered,
            cached: m.cache_hit,
            depth,
            body,
        }
    }
    fn error(url: String, error: String) -> Self {
        Row {
            title: url.clone(),
            url,
            ok: false,
            ms: 0,
            kb: 0,
            words: 0,
            rendered: false,
            cached: false,
            depth: 0,
            body: error.into(),
        }
    }
}

enum Msg {
    Row(Row),
    Links(Vec<String>),
    Done(Option<String>),
}

pub enum Run {
    Idle,
    Running { started: Instant },
    Finished { elapsed: Duration, note: Option<String> },
}

pub struct App {
    pub url: TextEdit,
    pub mode: Mode,
    pub render: usize,
    pub limit: usize,
    pub run: Run,
    pub rows: Vec<Row>,
    pub links: Vec<String>,
    pub selected: Option<usize>,
    pub raw: bool,
    /// Bumped on every start, so a stopped job's late messages are dropped.
    job: u64,
    inbox: Arc<Mutex<Vec<(u64, Msg)>>>,
    waker: Option<Waker>,
    rt: Arc<Runtime>,
    engine: Arc<Engine>,
    task: Option<AbortHandle>,
    pub bench: bench::Bench,
    pub intro: intro::Intro,
}

impl App {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
        let engine = rt.block_on(async { Engine::new(EngineConfig::default()) });
        App {
            url: TextEdit::new("https://doc.rust-lang.org/book/"),
            mode: Mode::Crawl,
            render: 0,
            limit: 1,
            run: Run::Idle,
            rows: Vec::new(),
            links: Vec::new(),
            selected: None,
            raw: false,
            job: 0,
            inbox: Arc::new(Mutex::new(Vec::new())),
            waker: None,
            rt: Arc::new(rt),
            engine,
            task: None,
            bench: bench::Bench::new(),
            intro: intro::Intro::new(),
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.run, Run::Running { .. })
    }

    pub fn pick_suite(&mut self, suite: ferrocrawl::bench::Suite) {
        self.bench.select(suite, &self.rt, &self.engine, &self.waker);
    }

    pub fn start_bench(&mut self) {
        self.bench.start(&self.rt, &self.engine, &self.waker);
    }

    pub fn set_mode(&mut self, i: usize) {
        self.mode = Mode::from_index(i);
        if self.mode == Mode::Bench && !self.bench.running() && self.bench.results.is_empty() {
            let suite = self.bench.suite;
            self.pick_suite(suite);
        }
    }

    pub fn elapsed(&self) -> Duration {
        match &self.run {
            Run::Idle => Duration::ZERO,
            Run::Running { started } => started.elapsed(),
            Run::Finished { elapsed, .. } => *elapsed,
        }
    }

    pub fn errors(&self) -> usize {
        self.rows.iter().filter(|r| !r.ok).count()
    }

    pub fn pages(&self) -> usize {
        self.rows.len() - self.errors()
    }

    pub fn copy_selected(&self, tx: &mut Tx) {
        if let Some(r) = self.selected.and_then(|i| self.rows.get(i)) {
            tx.copy(r.body.to_string());
        } else if self.mode == Mode::Map && !self.links.is_empty() {
            tx.copy(self.links.join("\n"));
        }
    }

    pub fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.job += 1;
        if let Run::Running { started } = self.run {
            self.run = Run::Finished { elapsed: started.elapsed(), note: Some("Stopped".into()) };
        }
    }

    pub fn intro_find(&mut self, q: String) {
        self.intro.find(q, &self.rt, &self.engine, &self.waker);
    }

    pub fn start(&mut self) {
        if self.mode == Mode::Bench {
            return self.start_bench();
        }
        if self.mode == Mode::Intro {
            return self.intro.start(&self.rt, &self.engine, &self.waker);
        }
        self.stop();
        let url = self.url.text.trim().to_string();
        if url.is_empty() {
            return;
        }
        self.rows.clear();
        self.links.clear();
        self.selected = None;
        self.run = Run::Running { started: Instant::now() };

        let job = self.job;
        let (inbox, waker) = (self.inbox.clone(), self.waker.clone());
        let send = move |m: Msg| {
            inbox.lock().unwrap().push((job, m));
            if let Some(w) = &waker {
                w();
            }
        };
        let scrape = ScrapeOptions {
            formats: vec![Format::Markdown],
            render: [Render::Auto, Render::Never, Render::Always][self.render],
            // Every run in the app should measure the network, not the cache.
            max_age_ms: 0,
            ..Default::default()
        };
        let limit = LIMITS[self.limit];
        let engine = self.engine.clone();
        let handle = match self.mode {
            Mode::Bench | Mode::Intro => unreachable!("benchmarks and intros start on their own"),
            Mode::Scrape => self.rt.spawn(async move {
                match engine.scrape(&url, &scrape).await {
                    Ok(doc) => send(Msg::Row(Row::page(doc, 0))),
                    Err(e) => send(Msg::Row(Row::error(url, format!("{e:#}")))),
                }
                send(Msg::Done(None));
            }),
            Mode::Crawl => self.rt.spawn(async move {
                let opts = CrawlOptions { limit, max_depth: 50, concurrency: 32, scrape, ..Default::default() };
                let (tx, mut rx) = mpsc::channel(256);
                let crawl = tokio::spawn(ferrocrawl::crawl(engine, url, opts, tx));
                while let Some(ev) = rx.recv().await {
                    send(Msg::Row(match ev {
                        CrawlEvent::Page { depth, document } => Row::page(*document, depth),
                        CrawlEvent::Error { url, error } => Row::error(url, error),
                    }));
                }
                let note = match crawl.await {
                    Ok(Ok(s)) => Some(format!("{} URLs discovered", s.discovered)),
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(e) => Some(e.to_string()),
                };
                send(Msg::Done(note));
            }),
            Mode::Map => self.rt.spawn(async move {
                let opts = MapOptions { limit: limit.max(500) * 10, ..Default::default() };
                match ferrocrawl::map(&engine, &url, &opts).await {
                    Ok(links) => {
                        send(Msg::Links(links));
                        send(Msg::Done(None));
                    }
                    Err(e) => send(Msg::Done(Some(format!("{e:#}")))),
                }
            }),
        };
        self.task = Some(handle.abort_handle());
    }
}

fn setup(s: &mut App, waker: Waker) {
    s.waker = Some(waker);
    if std::env::var("FERROCRAWL_MODE").is_ok_and(|m| m.eq_ignore_ascii_case("intro")) {
        s.mode = Mode::Intro;
    }
    // `FERROCRAWL_BENCH=<suite>` opens in Benchmark mode on that suite;
    // `FERROCRAWL_BENCH_RUN=1` also starts it.
    if let Some(suite) = std::env::var("FERROCRAWL_BENCH").ok().and_then(|v| ferrocrawl::bench::Suite::parse(&v)) {
        s.mode = Mode::Bench;
        s.pick_suite(suite);
        if std::env::var_os("FERROCRAWL_BENCH_RUN").is_some() {
            s.start_bench();
        }
    }
}

impl App {
    /// Lands what the worker sent since the last frame. True when anything changed.
    fn drain(&mut self) -> bool {
        let msgs: Vec<(u64, Msg)> = std::mem::take(&mut *self.inbox.lock().unwrap());
        let mut changed = false;
        for (job, m) in msgs {
            if job != self.job {
                continue;
            }
            changed = true;
            match m {
                Msg::Row(r) => {
                    self.rows.push(r);
                    if self.selected.is_none() {
                        self.selected = Some(0);
                    }
                }
                Msg::Links(l) => self.links = l,
                Msg::Done(note) => {
                    self.task = None;
                    self.run = Run::Finished { elapsed: self.elapsed(), note };
                }
            }
        }
        changed
    }
}

fn poll(s: &mut App, _: f64, tx: &mut Tx) -> bool {
    let changed = s.drain() | s.bench.drain() | s.intro.drain();
    // Keep the clock ticking while a job runs; an idle window draws nothing.
    if s.running() || s.bench.running() || s.intro.running() {
        tx.wake_after(Duration::from_millis(100));
        return true;
    }
    changed
}

fn on_key(s: &mut App, e: &KeyEvent, tx: &mut Tx) -> bool {
    match &e.key {
        Key::Escape if s.bench.running() => s.bench.stop(),
        Key::Escape if s.running() => s.stop(),
        Key::Down | Key::Up if !s.rows.is_empty() => {
            let last = s.rows.len() - 1;
            let i = s.selected.unwrap_or(0);
            let next = if matches!(e.key, Key::Down) { (i + 1).min(last) } else { i.saturating_sub(1) };
            s.selected = Some(next);
            tx.scroll_into_view(&ui::row_key(next));
        }
        _ => return false,
    }
    true
}

fn demo(s: &mut App) {
    s.mode = Mode::Crawl;
    let md = "## What Is Ownership?\n\n*Ownership* is a set of rules that govern how a Rust program manages memory.\n\n### The Stack and the Heap\n\nBoth the stack and the heap are parts of memory available to your code to use at runtime.\n\n```rust\nlet s = String::from(\"hello\");\n```\n";
    for (i, t) in ["The Rust Programming Language", "Getting Started", "What Is Ownership?", "References and Borrowing", "The Slice Type"].iter().enumerate() {
        s.rows.push(Row {
            url: format!("https://doc.rust-lang.org/book/ch0{i}-00.html"),
            title: t.to_string(),
            ok: true,
            ms: 40 + i as u64 * 17,
            kb: 22 + i * 5,
            words: 900 + i * 130,
            rendered: i == 3,
            cached: false,
            depth: i.min(2),
            body: md.into(),
        });
    }
    s.rows.push(Row::error("https://doc.rust-lang.org/book/missing.html".into(), "HTTP 404".into()));
    s.selected = Some(2);
    s.run = Run::Finished { elapsed: Duration::from_millis(760), note: Some("113 URLs discovered".into()) };
}

/// The Intro tab on whatever graph `FERROCRAWL_INTRO_DB` points at, searching `FERROCRAWL_INTRO_Q`.
fn demo_intro(s: &mut App) {
    s.mode = Mode::Intro;
    let q = std::env::var("FERROCRAWL_INTRO_Q").unwrap_or_else(|_| "Sequoia".into());
    s.intro.input = TextEdit::new(q.clone());
    s.intro.query = q.clone();
    if let Some(g) = s.intro.graph.clone() {
        s.intro.result = g.paths(&q, 8).ok();
        s.intro.stats = g.stats().unwrap_or_default();
    }
    s.intro.log.push((true, "GitHub — cloudflare/lol-html: 1 items, 29 people, 29 new links".into()));
}

fn demo_bench(s: &mut App) {
    use ferrocrawl::bench::{CaseResult, Suite};
    s.mode = Mode::Bench;
    s.bench.suite = Suite::Webcrawlerapi;
    let board = s.rt.block_on(ferrocrawl::bench::board(&s.engine, Suite::Webcrawlerapi));
    s.bench.boards_mut().insert(Suite::Webcrawlerapi, board);
    let hosts = ["edimark.fr/revues", "bbc.com/news/world", "docs.rs/tokio", "lemonde.fr/economie", "github.com/rust-lang", "nature.com/articles"];
    let mut results = Vec::new();
    for i in 0..120usize {
        let ok = i % 17 != 3;
        let recall = ((i * 37) % 100) as f64 / 100.0;
        results.push(CaseResult {
            index: i,
            id: i.to_string(),
            url: format!("https://{}/page-{i}", hosts[i % hosts.len()]),
            category: "all".into(),
            status: if i % 29 == 7 { "skipped".into() } else if ok { "ok".into() } else { "error".into() },
            ms: [180, 240, 420, 650, 900, 1300, 2600, 5200][i % 8] + (i as u64 * 7) % 90,
            chars: 3000 + (i * 911) % 40000,
            rendered: i % 11 == 0,
            recall: ok.then_some(0.55 + 0.45 * recall),
            f1: ok.then_some(0.4 + 0.5 * recall),
            covered: ok.then_some(recall > 0.08),
            error: (!ok).then(|| "GET https://example: operation timed out".into()),
            ..Default::default()
        });
    }
    s.bench.load_demo(1000, results, Duration::from_secs(41));
    s.bench.pick(4);
}

fn main() {
    rune::prewarm_gpu();
    let app = rune::App::new(AppSpec {
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: &[("demo", demo), ("bench", demo_bench), ("intro", demo_intro)],
        min_size: Some((900.0, 560.0)),
        ..AppSpec::new("ferrocrawl", (1280.0, 820.0), App::new, ui::view)
    });
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(app: &mut App) {
        let t0 = Instant::now();
        while app.running() && t0.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(50));
            app.drain();
        }
        assert!(!app.running(), "job finished");
    }

    /// Hits the network: `cargo test -p ferrocrawl-app -- --ignored`.
    #[test]
    #[ignore]
    fn crawl_and_map_land_in_the_ui_state() {
        let mut app = App::new();
        app.url = TextEdit::new("https://docs.python.org/3/library/");
        app.mode = Mode::Crawl;
        app.limit = 0;
        app.start();
        wait(&mut app);
        assert_eq!(app.pages(), LIMITS[0], "errors: {}", app.errors());
        assert!(app.rows.iter().all(|r| r.words > 0));
        assert_eq!(app.selected, Some(0));

        app.mode = Mode::Map;
        app.start();
        wait(&mut app);
        assert!(app.rows.is_empty() && app.links.len() > 100, "{} links", app.links.len());
    }

    #[test]
    #[ignore]
    fn stop_drops_late_results() {
        let mut app = App::new();
        app.url = TextEdit::new("https://docs.python.org/3/library/");
        app.mode = Mode::Crawl;
        app.limit = 2;
        app.start();
        std::thread::sleep(Duration::from_millis(300));
        app.stop();
        let n = { app.drain(); app.rows.len() };
        std::thread::sleep(Duration::from_millis(500));
        app.drain();
        assert_eq!(app.rows.len(), n, "nothing lands after Stop");
        assert!(matches!(app.run, Run::Finished { .. }));
    }
}
