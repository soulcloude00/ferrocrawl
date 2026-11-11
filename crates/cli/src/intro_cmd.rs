//! `ferrocrawl intro …`: build the warm-intro graph and ask it who can get you to someone.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Subcommand;
use ferrocrawl::Engine;
use intro::{EdgeKind, Graph, HarvestOptions, PersonIn, Relation};

#[derive(Subcommand)]
pub enum IntroCmd {
    /// Set your own name (the start of every path).
    Me { name: String },
    /// Import a network: LinkedIn Connections.csv or contacts .csv, phone/Google/iCloud contacts
    /// .vcf, email .mbox (Gmail via Google Takeout), calendar .ics. With --owner, the file is
    /// someone else's network that they shared with you.
    Import {
        file: PathBuf,
        #[arg(long)]
        owner: Option<String>,
        /// Your email address(es), for .mbox and .ics (default: the one on the most messages).
        #[arg(long = "me-email")]
        me_email: Vec<String>,
    },
    /// Read GitHub: owner/repo (its contributors), an org (its members) or a user (contributors
    /// to their repos). --mine: the user is you. Set GITHUB_TOKEN for higher rate limits.
    Github {
        target: String,
        #[arg(long)]
        mine: bool,
    },
    /// Record that two people know each other ("me" for you).
    Know {
        a: String,
        b: String,
        /// 0–1; default 0.85.
        #[arg(long)]
        strength: Option<f64>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Read people (team pages, speakers) or portfolio companies from public pages.
    Harvest {
        url: String,
        /// The organization the page belongs to (default: the site's name).
        #[arg(long)]
        org: Option<String>,
        /// auto, team, portfolio or people.
        #[arg(long, default_value = "auto")]
        relation: String,
        /// Pages to read; more than 1 crawls below the URL.
        #[arg(short, long, default_value_t = 1)]
        limit: usize,
        #[arg(long)]
        include: Vec<String>,
    },
    /// Rank the ways to reach a person or an organization.
    Path {
        target: String,
        #[arg(short, default_value_t = 5)]
        k: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show a person or organization and everyone attached to them.
    Show { query: String },
    /// Counts by kind and source.
    Stats,
}

pub async fn main(engine: Arc<Engine>, db: Option<PathBuf>, cmd: IntroCmd) -> Result<()> {
    let path = db.unwrap_or_else(Graph::default_path);
    let g = Graph::open(&path)?;
    match cmd {
        IntroCmd::Me { name } => {
            g.set_me(&name)?;
            eprintln!("you are {name} ({})", path.display());
        }
        IntroCmd::Import { file, owner, me_email } => {
            let r = intro::sources::import_any(&g, &file, owner.as_deref(), &me_email)?;
            eprintln!("{}", r.line());
        }
        IntroCmd::Github { target, mine } => {
            let r = intro::sources::github(engine.http(), &g, &target, mine).await?;
            eprintln!("{}", r.line());
        }
        IntroCmd::Know { a, b, strength, note } => {
            let ids = g.batch(|t| {
                let id = |n: &str| if n.eq_ignore_ascii_case("me") { t.me() } else { t.person(&PersonIn { name: n.into(), ..Default::default() }) };
                let (x, y) = (id(&a)?, id(&b)?);
                t.link(x, y, EdgeKind::Knows, strength, "manual", note.as_deref())?;
                Ok((x, y))
            })?;
            eprintln!("{a} (#{}) knows {b} (#{})", ids.0, ids.1);
        }
        IntroCmd::Harvest { url, org, relation, limit, include } => {
            let Some(relation) = Relation::parse(&relation) else { bail!("relation is auto, team, portfolio or people") };
            let r = intro::harvest(engine, &g, &url, &HarvestOptions { org, relation, limit, include_paths: include }).await?;
            for s in &r.sample {
                eprintln!("  {s}");
            }
            eprintln!(
                "{}: {} people, {} companies, {} new links from {} pages in {} ms",
                r.org, r.people, r.companies, r.new_edges, r.pages, r.elapsed_ms
            );
        }
        IntroCmd::Path { target, k, json } => {
            let r = g.paths(&target, k)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            print_paths(&target, &r);
        }
        IntroCmd::Show { query } => {
            let (targets, cands) = g.resolve(&query)?;
            for n in targets.iter().chain(&cands).take(5) {
                println!("#{} {} [{:?}]{}{}", n.id, n.name, n.kind, n.title.as_ref().map(|t| format!(" — {t}")).unwrap_or_default(), n.org.as_ref().map(|o| format!(" @ {o}")).unwrap_or_default());
                for (m, kind, s, ev) in g.neighbors(n.id)?.into_iter().take(40) {
                    println!("    {s:.2} {kind:<12} #{} {}{}", m.id, m.name, ev.map(|e| format!("  ({e})")).unwrap_or_default());
                }
            }
            if targets.is_empty() && cands.is_empty() {
                println!("nothing matches {query:?}");
            }
        }
        IntroCmd::Stats => {
            let s = g.stats()?;
            println!("{}: {} people, {} organizations, {} links (you: {})", path.display(), s.people, s.orgs, s.edges, s.me.as_deref().unwrap_or("not set"));
            for (k, n) in &s.by_kind {
                println!("  {n:>7} {k}");
            }
            for (k, n) in &s.sources {
                println!("  {n:>7} from {k}");
            }
        }
    }
    Ok(())
}

pub fn print_paths(target: &str, r: &intro::PathResult) {
    if r.targets.is_empty() {
        if r.candidates.is_empty() {
            println!("nobody called {target:?} in the graph. Harvest a page that lists them, or import a network that includes them.");
        } else {
            println!("{target:?} matches several; pick one (by #id):");
            for c in &r.candidates {
                println!("  #{} {}{}", c.id, c.name, c.org.as_ref().map(|o| format!(" @ {o}")).unwrap_or_default());
            }
        }
        return;
    }
    if r.paths.is_empty() {
        println!("no route to {target:?} yet. Import more networks (friends' Connections.csv) or harvest pages that link them.");
        return;
    }
    for (i, p) in r.paths.iter().enumerate() {
        match &p.ask {
            Some(a) => println!("{}. ask {a}  (score {:.2})", i + 1, p.score),
            None => println!("{}. you're directly connected  (score {:.2})", i + 1, p.score),
        }
        println!("   {}", p.summary);
        for h in &p.hops[1..] {
            if let Some(e) = &h.evidence {
                println!("     · {}: {e}", h.node.name);
            }
        }
    }
}
