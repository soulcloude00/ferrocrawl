//! The Intro tab's state: the warm-intro graph, and jobs (find, harvest, import, GitHub) that run
//! on the Tokio runtime and report back through an inbox, like the crawl does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use snarf::Engine;
use intro::{Graph, HarvestOptions, PathResult, Relation, Stats};
use rune::{TextEdit, Waker};
use tokio::runtime::Runtime;

pub const ACTIONS: [&str; 4] = ["Find", "Harvest", "Import", "GitHub"];
pub const GO: [&str; 4] = ["Find path", "Harvest", "Import", "Read GitHub"];
pub const PLACEHOLDER: [&str; 4] = [
    "Who do you want to reach? A person, a firm, an email…",
    "A public team, portfolio or speakers page: https://fund.vc/team",
    "A file: Connections.csv, contacts.vcf, mail.mbox, calendar.ics",
    "owner/repo, an org, or a user on GitHub",
];

pub enum Msg {
    Paths(Result<PathResult, String>),
    Log(bool, String),
    Stats(Stats),
    Done,
}

pub struct Intro {
    pub action: usize,
    pub input: TextEdit,
    pub graph: Option<Arc<Graph>>,
    pub db_path: PathBuf,
    pub result: Option<PathResult>,
    pub query: String,
    pub selected: usize,
    pub stats: Stats,
    /// What each import or harvest did, newest last.
    pub log: Vec<(bool, String)>,
    pub busy: Option<Instant>,
    job: u64,
    inbox: Arc<Mutex<Vec<(u64, Msg)>>>,
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|_| p.into()),
        None => p.into(),
    }
}

impl Intro {
    pub fn new() -> Self {
        let db_path = Graph::default_path();
        let (graph, log) = match Graph::open(&db_path) {
            Ok(g) => (Some(Arc::new(g)), Vec::new()),
            Err(e) => (None, vec![(false, format!("can't open the graph at {}: {e:#}", db_path.display()))]),
        };
        let stats = graph.as_ref().and_then(|g| g.stats().ok()).unwrap_or_default();
        Intro {
            action: 0,
            input: TextEdit::new(""),
            graph,
            db_path,
            result: None,
            query: String::new(),
            selected: 0,
            stats,
            log,
            busy: None,
            job: 0,
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn running(&self) -> bool {
        self.busy.is_some()
    }

    /// Runs Find for `q` (a candidate picked from an ambiguous search, say).
    pub fn find(&mut self, q: String, rt: &Runtime, engine: &Arc<Engine>, waker: &Option<Waker>) {
        self.input = TextEdit::new(q);
        self.action = 0;
        self.start(rt, engine, waker);
    }

    pub fn start(&mut self, rt: &Runtime, engine: &Arc<Engine>, waker: &Option<Waker>) {
        let Some(g) = self.graph.clone() else { return };
        let input = self.input.text.trim().to_string();
        if input.is_empty() || self.running() {
            return;
        }
        self.job += 1;
        let job = self.job;
        let (inbox, waker) = (self.inbox.clone(), waker.clone());
        let send = move |m: Msg| {
            inbox.lock().unwrap().push((job, m));
            if let Some(w) = &waker {
                w();
            }
        };
        self.busy = Some(Instant::now());
        let engine = engine.clone();
        match self.action {
            0 => {
                self.query = input.clone();
                rt.spawn_blocking(move || {
                    send(Msg::Paths(g.paths(&input, 8).map_err(|e| format!("{e:#}"))));
                    send(Msg::Done);
                });
            }
            1 => {
                rt.spawn(async move {
                    let opts = HarvestOptions { relation: Relation::Auto, limit: 1, ..Default::default() };
                    match intro::harvest(engine, &g, &input, &opts).await {
                        Ok(r) => send(Msg::Log(true, format!("{}: {} people, {} companies, {} new links ({} ms)", r.org, r.people, r.companies, r.new_edges, r.elapsed_ms))),
                        Err(e) => send(Msg::Log(false, format!("{input}: {e:#}"))),
                    }
                    if let Ok(s) = g.stats() {
                        send(Msg::Stats(s));
                    }
                    send(Msg::Done);
                });
            }
            2 => {
                rt.spawn_blocking(move || {
                    let path = expand(&input);
                    match intro::sources::import_any(&g, &path, None, &[]) {
                        Ok(r) => send(Msg::Log(true, r.line())),
                        Err(e) => send(Msg::Log(false, format!("{}: {e:#}", path.display()))),
                    }
                    if let Ok(s) = g.stats() {
                        send(Msg::Stats(s));
                    }
                    send(Msg::Done);
                });
            }
            _ => {
                rt.spawn(async move {
                    match intro::sources::github(engine.http(), &g, &input, false).await {
                        Ok(r) => send(Msg::Log(true, format!("{input} — {}", r.line()))),
                        Err(e) => send(Msg::Log(false, format!("{input}: {e:#}"))),
                    }
                    if let Ok(s) = g.stats() {
                        send(Msg::Stats(s));
                    }
                    send(Msg::Done);
                });
            }
        }
    }

    pub fn drain(&mut self) -> bool {
        let msgs: Vec<(u64, Msg)> = std::mem::take(&mut *self.inbox.lock().unwrap());
        let mut changed = false;
        for (job, m) in msgs {
            if job != self.job {
                continue;
            }
            changed = true;
            match m {
                Msg::Paths(Ok(r)) => {
                    self.result = Some(r);
                    self.selected = 0;
                }
                Msg::Paths(Err(e)) => {
                    self.result = None;
                    self.log.push((false, e));
                }
                Msg::Log(ok, line) => self.log.push((ok, line)),
                Msg::Stats(s) => self.stats = s,
                Msg::Done => self.busy = None,
            }
        }
        changed
    }

    /// A ready-to-send message asking the introducer for the intro.
    pub fn draft(&self) -> Option<String> {
        let r = self.result.as_ref()?;
        let p = r.paths.get(self.selected)?;
        let target = p.hops.last()?;
        let who = match (&target.node.title, &target.node.org) {
            (Some(t), Some(o)) => format!("{} ({t} at {o})", target.node.name),
            (None, Some(o)) => format!("{} at {o}", target.node.name),
            _ => target.node.name.clone(),
        };
        let Some(ask) = &p.ask else {
            return Some(format!("Hi {},\n\nI hope you're doing well! I'm working on [your project] and would love to get your thoughts on [funding / a sponsorship / a partnership]. Would you have 20 minutes in the next couple of weeks?\n\nThanks,\n[you]", first(&target.node.name)));
        };
        let link = p.hops.iter().position(|h| &h.node.name == ask).and_then(|i| p.hops.get(i + 1)).map(|h| h.node.name.clone()).unwrap_or_default();
        Some(format!(
            "Hi {},\n\nI hope you're doing well! I'm working on [your project] and I'm hoping to connect with {who} about [funding / a sponsorship / a partnership]. I noticed you know {}, and a short intro from you would mean a lot.\n\nIf you're open to it, I can send a two-line blurb you can forward. No worries at all if it's not a fit.\n\nThanks,\n[you]",
            first(ask),
            if link == target.node.name || link.is_empty() { first(&target.node.name).to_string() } else { link },
        ))
    }
}

fn first(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}
