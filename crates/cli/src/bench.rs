//! `ferrocrawl bench run|prep-judge|report`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use ferrocrawl::Engine;
use ferrocrawl::bench::{self, BenchEvent, CaseResult, RunOptions, Suite};
use tokio::sync::mpsc;

#[derive(Subcommand)]
pub enum BenchCmd {
    /// Scrape every page of a suite and score it.
    Run {
        /// tinyfish | webcrawlerapi | trace
        suite: String,
        #[arg(short, long, default_value_t = 16)]
        concurrency: usize,
        #[arg(short, long)]
        limit: Option<usize>,
        /// Only cases in this category.
        #[arg(long)]
        category: Option<String>,
        /// Run directory (default: ~/.local/share/ferrocrawl/bench/<suite>-<time>).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Write judge batches (TRACE's evidence rule) for an LLM judge to grade.
    PrepJudge {
        dir: PathBuf,
        #[arg(long, default_value_t = 100)]
        batch: usize,
    },
    /// Re-extract a run's saved raw HTML with the current code and re-score it (no network).
    Rescore {
        dir: PathBuf,
        /// Print the N worst main-content rows.
        #[arg(long, default_value_t = 0)]
        worst: usize,
    },
    /// Score a finished run, merging `verdicts.jsonl` when present, and print the leaderboard.
    Report { dir: PathBuf },
}

pub async fn main(engine: Arc<Engine>, cmd: BenchCmd) -> Result<()> {
    match cmd {
        BenchCmd::Run { suite, concurrency, limit, category, out } => {
            let suite = Suite::parse(&suite).with_context(|| format!("unknown suite {suite}"))?;
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
            let dir = out.unwrap_or_else(|| bench::runs_root().join(format!("{}-{stamp}", suite.slug())));
            eprintln!("{} → {}", suite.name(), dir.display());
            let (tx, mut rx) = mpsc::channel(256);
            let job = tokio::spawn(bench::run(engine.clone(), suite, RunOptions { concurrency, limit, out_dir: dir.clone(), category }, tx));
            let t0 = Instant::now();
            let (mut total, mut done, mut ok) = (0, 0, 0);
            while let Some(ev) = rx.recv().await {
                match ev {
                    BenchEvent::Loaded { total: t } => total = t,
                    BenchEvent::Case(r) => {
                        done += 1;
                        ok += usize::from(r.status == "ok");
                        if done % 50 == 0 || done == total {
                            eprintln!("{done}/{total}  ok {ok}  {:.1} pages/s", done as f64 / t0.elapsed().as_secs_f64());
                        }
                    }
                }
            }
            let results = job.await??;
            report(&engine, suite, &dir, total, &results).await?;
        }
        BenchCmd::PrepJudge { dir, batch } => prep_judge(&dir, batch)?,
        BenchCmd::Rescore { dir, worst } => {
            let (suite, total, _) = bench::load_run(&dir)?;
            let (main, full) = bench::rescore(&dir)?;
            let none = Default::default();
            for (name, rows) in [("main content", &main), ("full page", &full)] {
                let s = bench::summarize(suite, total, rows, &none);
                println!(
                    "{name:<13} coverage {:.2}%  recall {:.2}%  F1 {:.4}  ({} scored)",
                    s.score.unwrap_or(0.0),
                    s.recall.unwrap_or(0.0),
                    s.f1.unwrap_or(0.0),
                    s.ok
                );
            }
            let mut rows: Vec<(&ferrocrawl::bench::CaseResult, &ferrocrawl::bench::CaseResult)> =
                main.iter().zip(full.iter()).filter(|(m, _)| m.status == "ok").collect();
            rows.sort_by(|a, b| a.0.recall.unwrap_or(0.0).total_cmp(&b.0.recall.unwrap_or(0.0)));
            // One JSON line per scored row, for analysis scripts.
            let dump: String = rows
                .iter()
                .map(|(m, f)| {
                    serde_json::json!({"index": m.index, "id": m.id, "url": m.url, "main": m.recall, "full": f.recall,
                        "main_chars": m.chars, "full_chars": f.chars, "rendered": m.rendered, "main_f1": m.f1})
                    .to_string()
                        + "\n"
                })
                .collect();
            std::fs::write(dir.join("rescore.jsonl"), dump)?;
            for (m, f) in rows.iter().take(worst) {
                println!("{:.2} (full {:.2}) {:>7}ch #{} {}", m.recall.unwrap_or(0.0), f.recall.unwrap_or(0.0), m.chars, m.index, m.url);
            }
        }
        BenchCmd::Report { dir } => {
            let (suite, total, results) = bench::load_run(&dir)?;
            report(&engine, suite, &dir, total, &results).await?;
        }
    }
    Ok(())
}

async fn report(engine: &Engine, suite: Suite, dir: &std::path::Path, total: usize, results: &[CaseResult]) -> Result<()> {
    let verdicts = bench::read_verdicts(dir);
    let mut s = bench::summarize(suite, total, results, &verdicts);
    s.board = bench::board(engine, suite).await;
    std::fs::write(dir.join("summary.json"), serde_json::to_vec_pretty(&s)?)?;

    let mut o = std::io::stdout().lock();
    writeln!(o, "\n{} · {} pages · {} ok · {} errors · {} skipped · {} rendered", suite.name(), s.total, s.ok, s.errors, s.skipped, s.rendered)?;
    writeln!(o, "latency p50 {} ms · p90 {} · p95 {} · p99 {} · mean {}", s.latency.p50, s.latency.p90, s.latency.p95, s.latency.p99, s.latency.mean)?;
    if suite.judged() && s.judged == 0 {
        writeln!(o, "quality: not judged yet (run `ferrocrawl bench prep-judge {}`, then grade)", dir.display())?;
    }
    writeln!(o, "\n{:<28} {:>8} {:>8} {:>7} {:>8} {:>8}", "provider", "score", "recall", "F1", "p50", "p95")?;
    let mut board = s.board.clone();
    board.push(bench::Entry {
        name: "ferrocrawl".into(),
        ours: true,
        score: s.score,
        recall: s.recall,
        f1: s.f1,
        p50_ms: Some(s.latency.p50),
        p95_ms: Some(s.latency.p95),
    });
    board.sort_by(|a, b| b.score.unwrap_or(-1.0).total_cmp(&a.score.unwrap_or(-1.0)));
    for e in &board {
        let f = |v: Option<f64>, d: usize| v.map(|x| format!("{x:.d$}")).unwrap_or_else(|| "—".into());
        let ms = |v: Option<u64>| v.map(|x| format!("{x}ms")).unwrap_or_else(|| "—".into());
        writeln!(
            o,
            "{:<28} {:>8} {:>8} {:>7} {:>8} {:>8}{}",
            e.name,
            f(e.score, 1),
            f(e.recall, 1),
            f(e.f1, 3),
            ms(e.p50_ms),
            ms(e.p95_ms),
            if e.ours { "  ◀" } else { "" }
        )?;
    }
    if !s.categories.is_empty() {
        writeln!(o)?;
        for (c, (p, n)) in &s.categories {
            writeln!(o, "  {c:<20} {p}/{n}")?;
        }
    }
    Ok(())
}

/// Splits a run's successful pages into JSONL batches of judge evidence.
fn prep_judge(dir: &std::path::Path, batch: usize) -> Result<()> {
    let (suite, _, results) = bench::load_run(dir)?;
    if !suite.judged() {
        bail!("{} is scored without a judge", suite.name());
    }
    let out = dir.join("judge");
    std::fs::create_dir_all(&out)?;
    let mut items = Vec::new();
    for r in results.iter().filter(|r| r.status == "ok") {
        let md = std::fs::read_to_string(dir.join("pages").join(format!("{:05}.md", r.index))).unwrap_or_default();
        items.push(serde_json::json!({
            "id": r.id,
            "target_url": r.url,
            "final_url": r.final_url,
            "title": r.title,
            "category": r.category,
            "chars": md.len(),
            "evidence": bench::excerpts(&md),
        }));
    }
    let n = items.len().div_ceil(batch.max(1));
    for (i, chunk) in items.chunks(batch.max(1)).enumerate() {
        let body: String = chunk.iter().map(|v| v.to_string() + "\n").collect();
        std::fs::write(out.join(format!("batch-{i:03}.jsonl")), body)?;
    }
    eprintln!("{} pages in {n} batches → {}", items.len(), out.display());
    Ok(())
}
