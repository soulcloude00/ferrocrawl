//! Benchmark mode: runs a public suite on the engine, streams each page in, and keeps the summary
//! and leaderboard current. A finished (and judged) run is loaded from disk when nothing is live.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrocrawl::Engine;
use ferrocrawl::bench::{self, BenchEvent, CaseResult, Entry, RunOptions, Suite, Summary, Verdict};
use rune::Waker;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

pub const CONCURRENCY: [usize; 4] = [8, 16, 32, 64];
pub const SAMPLES: [Option<usize>; 4] = [Some(25), Some(100), Some(500), None];
pub const FILTERS: [&str; 4] = ["All", "Failed", "Browser", "Slowest"];

pub enum Msg {
    Loaded { job: u64, total: usize },
    Case { job: u64, result: Box<CaseResult> },
    Done { job: u64, error: Option<String> },
    Board { suite: Suite, entries: Vec<Entry> },
    Saved { suite: Suite, dir: PathBuf, total: usize, results: Vec<CaseResult>, verdicts: HashMap<String, Verdict>, elapsed_ms: Option<u64> },
}

#[derive(PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    Done,
}

pub struct Bench {
    pub suite: Suite,
    pub concurrency: usize,
    pub sample: usize,
    pub filter: usize,
    pub phase: Phase,
    pub started: Option<Instant>,
    pub elapsed: Duration,
    pub total: usize,
    pub results: Vec<CaseResult>,
    pub verdicts: HashMap<String, Verdict>,
    pub boards: HashMap<Suite, Vec<Entry>>,
    pub summary: Summary,
    /// Where the shown results live: a saved run, or the live run's folder.
    pub dir: Option<PathBuf>,
    /// The shown results came from disk, not a live run.
    pub from_disk: bool,
    pub selected: Option<usize>,
    /// The selected page's saved Markdown, read once on selection.
    pub detail: Option<Arc<str>>,
    pub error: Option<String>,
    /// Bumped per summary change, so memoized panels rebuild only when the numbers move.
    pub revision: u64,
    job: u64,
    task: Option<AbortHandle>,
    inbox: Arc<Mutex<Vec<Msg>>>,
}

impl Bench {
    pub fn new() -> Self {
        Bench {
            suite: Suite::Webcrawlerapi,
            concurrency: 1,
            sample: 3,
            filter: 0,
            phase: Phase::Idle,
            started: None,
            elapsed: Duration::ZERO,
            total: 0,
            results: Vec::new(),
            verdicts: HashMap::new(),
            boards: HashMap::new(),
            summary: Summary::default(),
            dir: None,
            from_disk: false,
            selected: None,
            detail: None,
            error: None,
            revision: 0,
            job: 0,
            task: None,
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn running(&self) -> bool {
        self.phase == Phase::Running
    }

    pub fn elapsed(&self) -> Duration {
        match (self.phase == Phase::Running, self.started) {
            (true, Some(t)) => t.elapsed(),
            _ => self.elapsed,
        }
    }

    fn sender(&self, waker: &Option<Waker>) -> impl Fn(Msg) + Send + Clone + 'static {
        let (inbox, waker) = (self.inbox.clone(), waker.clone());
        move |m: Msg| {
            inbox.lock().unwrap().push(m);
            if let Some(w) = &waker {
                w();
            }
        }
    }

    /// Picks a suite: fetches its leaderboard and, when idle, shows its last saved run.
    pub fn select(&mut self, suite: Suite, rt: &Runtime, engine: &Arc<Engine>, waker: &Option<Waker>) {
        if self.running() {
            return;
        }
        self.suite = suite;
        self.results.clear();
        self.verdicts.clear();
        self.selected = None;
        self.detail = None;
        self.dir = None;
        self.from_disk = false;
        self.phase = Phase::Idle;
        self.error = None;
        self.recompute();
        let send = self.sender(waker);
        if !self.boards.contains_key(&suite) {
            let (engine, send) = (engine.clone(), send.clone());
            rt.spawn(async move {
                let entries = bench::board(&engine, suite).await;
                send(Msg::Board { suite, entries });
            });
        }
        rt.spawn_blocking(move || {
            let Some(dir) = bench::latest_run(suite) else { return };
            if let Ok((s, total, results)) = bench::load_run(&dir) {
                let verdicts = bench::read_verdicts(&dir);
                let elapsed_ms = bench::run_elapsed_ms(&dir);
                send(Msg::Saved { suite: s, dir, total, results, verdicts, elapsed_ms });
            }
        });
    }

    pub fn start(&mut self, rt: &Runtime, engine: &Arc<Engine>, waker: &Option<Waker>) {
        self.stop();
        self.job += 1;
        let job = self.job;
        let suite = self.suite;
        self.results.clear();
        self.verdicts.clear();
        self.selected = None;
        self.detail = None;
        self.total = 0;
        self.from_disk = false;
        self.error = None;
        self.phase = Phase::Running;
        self.started = Some(Instant::now());
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let dir = bench::runs_root().join(format!("{}-app-{stamp}", suite.slug()));
        self.dir = Some(dir.clone());
        self.recompute();
        let opts = RunOptions { concurrency: CONCURRENCY[self.concurrency], limit: SAMPLES[self.sample], out_dir: dir, category: None };
        let send = self.sender(waker);
        let engine = engine.clone();
        let handle = rt.spawn(async move {
            let (tx, mut rx) = mpsc::channel(512);
            let run = tokio::spawn(bench::run(engine, suite, opts, tx));
            while let Some(ev) = rx.recv().await {
                send(match ev {
                    BenchEvent::Loaded { total } => Msg::Loaded { job, total },
                    BenchEvent::Case(result) => Msg::Case { job, result },
                });
            }
            let error = match run.await {
                Ok(Ok(_)) => None,
                Ok(Err(e)) => Some(format!("{e:#}")),
                Err(e) => Some(e.to_string()),
            };
            send(Msg::Done { job, error });
        });
        self.task = Some(handle.abort_handle());
    }

    pub fn stop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        if self.phase == Phase::Running {
            self.elapsed = self.started.map(|t| t.elapsed()).unwrap_or_default();
            self.phase = Phase::Done;
            self.error = Some("Stopped".into());
        }
        self.job += 1;
    }

    /// Lands worker messages. True when anything changed.
    pub fn drain(&mut self) -> bool {
        let msgs: Vec<Msg> = std::mem::take(&mut *self.inbox.lock().unwrap());
        if msgs.is_empty() {
            return false;
        }
        let mut changed = false;
        for m in msgs {
            match m {
                Msg::Loaded { job, total } if job == self.job => {
                    self.total = total;
                    changed = true;
                }
                Msg::Case { job, result } if job == self.job => {
                    self.results.push(*result);
                    changed = true;
                }
                Msg::Done { job, error } if job == self.job => {
                    self.task = None;
                    self.elapsed = self.started.map(|t| t.elapsed()).unwrap_or_default();
                    self.phase = Phase::Done;
                    self.error = error;
                    changed = true;
                }
                Msg::Board { suite, entries } => {
                    self.boards.insert(suite, entries);
                    changed = true;
                }
                Msg::Saved { suite, dir, total, results, verdicts, elapsed_ms }
                    if suite == self.suite && self.phase != Phase::Running && self.results.is_empty() =>
                {
                    self.elapsed = Duration::from_millis(elapsed_ms.unwrap_or(0));
                    self.total = total;
                    self.results = results;
                    self.verdicts = verdicts;
                    self.dir = Some(dir);
                    self.from_disk = true;
                    self.phase = Phase::Done;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.recompute();
        }
        changed
    }

    pub fn boards_mut(&mut self) -> &mut HashMap<Suite, Vec<Entry>> {
        &mut self.boards
    }

    /// Shows `results` as a finished run (the headless `bench` scene).
    pub fn load_demo(&mut self, total: usize, results: Vec<CaseResult>, elapsed: Duration) {
        self.total = total;
        self.results = results;
        self.elapsed = elapsed;
        self.phase = Phase::Done;
        self.recompute();
    }

    fn recompute(&mut self) {
        let mut s = bench::summarize(self.suite, self.total, &self.results, &self.verdicts);
        s.board = self.boards.get(&self.suite).cloned().unwrap_or_default();
        self.summary = s;
        self.revision += 1;
    }

    /// Indices of the results the current filter shows, newest first while running.
    pub fn visible(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.results.len())
            .filter(|&i| {
                let r = &self.results[i];
                match self.filter {
                    1 => r.status == "error" || self.failed_quality(r),
                    2 => r.rendered,
                    _ => true,
                }
            })
            .collect();
        if self.filter == 3 {
            v.sort_by_key(|&i| std::cmp::Reverse(self.results[i].ms));
        } else if self.running() {
            v.reverse();
        }
        v
    }

    /// The page came back but the suite's quality bar wasn't met.
    pub fn failed_quality(&self, r: &CaseResult) -> bool {
        match self.suite {
            Suite::Webcrawlerapi => r.covered == Some(false),
            Suite::Trace => self.verdicts.get(&r.id).is_some_and(|v| !(v.content == "substantive" && v.identity == "match")),
            Suite::Tinyfish => {
                r.status == "ok" && (r.chars < 1000 || self.verdicts.get(&r.id).and_then(|v| v.signal).is_some_and(|x| x <= 30.0))
            }
        }
    }

    /// Selects a result and reads its saved Markdown for the detail panel.
    pub fn pick(&mut self, i: usize) {
        self.selected = Some(i);
        self.detail = (|| {
            let dir = self.dir.as_ref()?;
            let r = self.results.get(i)?;
            std::fs::read_to_string(dir.join("pages").join(format!("{:05}.md", r.index))).ok()
        })()
        .map(Arc::from);
    }
}
