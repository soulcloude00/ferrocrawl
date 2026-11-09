//! The Benchmark view: run a public suite and watch the score, latency and leaderboard move.
//!
//! ┌ toolbar: suite · concurrency · sample · Run ───────────────────────────────┐
//! │ hero tiles: progress, headline score, recall/F1 or judged, p50/p95, pages/s │
//! ├ leaderboard ─────────────┬ results (filter) ──────────────────────────────┤
//! │ by category              │ virtual rows                                     │
//! │ latency histogram        ├ detail: the selected page's numbers + Markdown   │
//! │ run info                 │                                                  │
//! └──────────────────────────┴──────────────────────────────────────────────────┘

use ferrocrawl::bench::{Entry, Suite};
use rune::{Color, El, GrowthSpec, ScrollSpec, TextStyle, Tx, col, el, icon, memo, row, spacer, text};
use rune_kit::{Button, ButtonVariant, Kit, Progress, SegmentedControl, Spinner};

use crate::bench::{CONCURRENCY, FILTERS, Phase, SAMPLES};
use crate::ui::{self, BOLT, LABEL, MONO, Pal, ROW_TITLE, SMALL, badge, fmt_n};
use crate::{App, MODES};

const SUITES: [Suite; 3] = [Suite::Webcrawlerapi, Suite::Trace, Suite::Tinyfish];
const HERO: TextStyle = TextStyle::new(26.0, 700.0).tracking(-0.6).features("'tnum' 1");
const CARD_HEAD: TextStyle = TextStyle::new(12.5, 650.0);
const BAR_W: f32 = 170.0;

// ------------------------------------------------------------------ toolbar controls

pub fn controls(s: &App, p: &Pal, k: &Kit) -> El {
    let b = &s.bench;
    let names: Vec<&str> = SUITES.iter().map(|x| x.name()).collect();
    let suites = SegmentedControl::new(&names, SUITES.iter().position(|x| *x == b.suite))
        .label("Suite")
        .fit()
        .on_pick(|s: &mut App, i, _: &mut Tx| s.pick_suite(SUITES[i]))
        .build(k);
    let conc: Vec<String> = CONCURRENCY.iter().map(|n| n.to_string()).collect();
    let concurrency = SegmentedControl::new(&conc, Some(b.concurrency))
        .label("Concurrency")
        .on_pick(|s: &mut App, i, _: &mut Tx| s.bench.concurrency = i)
        .build(k);
    let samp: Vec<String> = SAMPLES.iter().map(|n| n.map(|n| n.to_string()).unwrap_or_else(|| "All".into())).collect();
    let sample = SegmentedControl::new(&samp, Some(b.sample))
        .label("Pages")
        .on_pick(|s: &mut App, i, _: &mut Tx| s.bench.sample = i)
        .build(k);
    row()
        .gap(10.0)
        .center_y()
        .child(suites)
        .child(text("Concurrency").font(SMALL).color(p.faint))
        .child(concurrency)
        .child(text("Pages").font(SMALL).color(p.faint))
        .child(sample)
}

pub fn run_button(s: &App, k: &Kit) -> El {
    if s.bench.running() {
        Button::new("Stop").variant(ButtonVariant::Danger).on_press(|s: &mut App| s.bench.stop()).build(k)
    } else {
        Button::new(format!("Run {}", MODES[3].to_lowercase())).variant(ButtonVariant::Submit).on_press(|s: &mut App| s.start_bench()).build(k)
    }
}

pub fn suite_blurb(suite: Suite) -> &'static str {
    match suite {
        Suite::Webcrawlerapi => "Firecrawl's public dataset · 1,000 rows · truth/lie token overlap, scored locally",
        Suite::Trace => "context.dev TRACE · 2,527 fresh pages (Nov 6) · LLM-judged useful content, category-weighted",
        Suite::Tinyfish => "TinyFish fetch set · 45 URLs · ≥1,000 chars and >30% signal (LLM-judged)",
    }
}

// ------------------------------------------------------------------ body

pub fn body(s: &App, p: &Pal, k: &Kit) -> El {
    col()
        .key("bench")
        .w_full()
        .grow(1.0)
        .shrink()
        .min_h(0.0)
        .child(hero(s, p))
        .child(
            row()
                .key("bench-main")
                .w_full()
                .grow(1.0)
                .shrink()
                .min_h(0.0)
                .child(side(s, p))
                .child(results(s, p, k)),
        )
}

fn hero(s: &App, p: &Pal) -> El {
    let b = &s.bench;
    let sm = &b.summary;
    let secs = b.elapsed().as_secs_f64();
    let rate = if secs > 0.05 { sm.done as f64 / secs } else { 0.0 };
    let frac = if sm.total > 0 { sm.done as f32 / sm.total as f32 } else { 0.0 };

    let tile = |key: &'static str, label: &'static str, value: String, sub: String, color: Color| {
        col()
            .key(key)
            .gap(3.0)
            .min_w(118.0)
            .child(text(label).font(LABEL).color(p.faint))
            .child(text(value).font(HERO).color(color))
            .child(text(sub).font(SMALL).color(p.faint).truncate())
    };
    let pct = |v: Option<f64>| v.map(|x| format!("{x:.1}%")).unwrap_or_else(|| "—".into());

    let (score_label, score_sub) = match b.suite {
        Suite::Webcrawlerapi => ("COVERAGE", "≥10% of truth text found"),
        Suite::Trace => ("USEFUL CONTENT", "substantive + right page"),
        Suite::Tinyfish => ("USABLE", "≥1k chars, >30% signal"),
    };
    let score_sub = if b.suite.judged() && sm.judged == 0 { "awaiting LLM judge".to_string() } else { score_sub.to_string() };

    let (third_label, third_value, third_sub) = match b.suite {
        Suite::Webcrawlerapi => ("RECALL", pct(sm.recall), "truth tokens returned".to_string()),
        _ => ("JUDGED", fmt_n(sm.judged), format!("of {} returned pages", fmt_n(sm.ok))),
    };
    let (fourth_label, fourth_value, fourth_sub) = match b.suite {
        Suite::Webcrawlerapi => ("F1", sm.f1.map(|f| format!("{f:.3}")).unwrap_or_else(|| "—".into()), "recall × noise avoided".to_string()),
        _ => {
            let attempted = sm.done.saturating_sub(sm.skipped).max(1);
            ("RETURNED", format!("{:.1}%", 100.0 * sm.ok as f64 / attempted as f64), "pages with any content".to_string())
        }
    };
    let end_to_end = sm.categories.get("end-to-end").map(|(c, n)| format!("end-to-end {:.1}%", 100.0 * *c as f64 / (*n).max(1) as f64));

    let status = match (&b.phase, &b.error) {
        (Phase::Running, _) => row().gap(8.0).center_y().child(Spinner::new().size(13.0).color(p.accent).build(&ui::kit())).child(text("Running").font(SMALL).color(p.accent)),
        (Phase::Done, Some(e)) => row().child(text(e.clone()).font(SMALL).color(p.bad).truncate()),
        (Phase::Done, None) if b.from_disk => row().child(text("Saved run").font(SMALL).color(p.info)),
        (Phase::Done, None) => row().child(text("Finished").font(SMALL).color(p.good)),
        (Phase::Idle, _) => row().child(text("Ready").font(SMALL).color(p.dim)),
    };

    let progress = col()
        .key("bt-progress")
        .gap(6.0)
        .w(230.0)
        .child(row().w_full().child(text("PROGRESS").font(LABEL).color(p.faint)).child(spacer()).child(status.max_w(150.0)))
        .child(text(format!("{} / {}", fmt_n(sm.done), fmt_n(sm.total))).font(HERO).color(p.text))
        .child(Progress::new(Some(frac)).build(&ui::kit()).w_full());

    row()
        .key("bench-hero")
        .w_full()
        .px(22.0)
        .py(14.0)
        .gap(28.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(progress)
        .child(tile("bt-score", score_label, pct(sm.score), end_to_end.unwrap_or(score_sub), p.accent))
        .child(tile("bt-third", third_label, third_value, third_sub, p.text))
        .child(tile("bt-fourth", fourth_label, fourth_value, fourth_sub, p.text))
        .child(tile("bt-p50", "P50 LATENCY", ms(sm.latency.p50), format!("p90 {} · mean {}", ms(sm.latency.p90), ms(sm.latency.mean)), p.text))
        .child(tile("bt-p95", "P95 LATENCY", ms(sm.latency.p95), format!("p99 {}", ms(sm.latency.p99)), p.text))
        .child(tile("bt-rate", "PAGES / SEC", format!("{rate:.1}"), format!("{:.1}s elapsed", secs), p.accent))
        .child(tile(
            "bt-err",
            "ERRORS",
            fmt_n(sm.errors),
            format!("{} browser · {} skipped", fmt_n(sm.rendered), fmt_n(sm.skipped)),
            if sm.errors > 0 { p.bad } else { p.text },
        ))
}

fn ms(v: u64) -> String {
    if v >= 10_000 { format!("{:.1}s", v as f64 / 1000.0) } else { format!("{v} ms") }
}

// ------------------------------------------------------------------ left column

fn side(s: &App, p: &Pal) -> El {
    let b = &s.bench;
    let rev = b.revision;
    let content = col()
        .w_full()
        .px(18.0)
        .py(16.0)
        .gap(16.0)
        .child(text(suite_blurb(b.suite)).font(SMALL).color(p.dim))
        .child(memo(format!("board-{}", b.suite.slug()), (rev, b.suite.slug()), || leaderboard(s)))
        .child(memo(format!("cats-{}", b.suite.slug()), (rev, b.suite.slug()), || categories(s)))
        .child(memo(format!("hist-{}", b.suite.slug()), (rev, b.suite.slug()), || histogram(s)))
        .child(run_info(s, p));
    col()
        .key("bench-side")
        .w(500.0)
        .h_full()
        .min_h(0.0)
        .fill(p.panel)
        .border_sides([0.0, 1.0, 0.0, 0.0], p.line)
        .child(col().key("bench-side-scroll").w_full().grow(1.0).shrink().min_h(0.0).child(content).scroll(ScrollSpec { stick_bottom: false }).scroll_edge_fade(12.0))
}

fn card(p: &Pal, key: String, title: &str, aside: String, body: El) -> El {
    col()
        .key(key)
        .w_full()
        .px(14.0)
        .py(14.0)
        .gap(12.0)
        .radius(12.0)
        .fill(p.well.alpha(0.55))
        .border(1.0, p.line)
        .child(row().w_full().center_y().child(text(title.to_string()).font(CARD_HEAD).color(p.text)).child(spacer()).child(text(aside).font(SMALL).color(p.faint)))
        .child(body)
}

fn bar(p: &Pal, frac: f64, color: Color) -> El {
    let w = (BAR_W as f64 * frac.clamp(0.0, 1.0)) as f32;
    el().w(BAR_W).h(8.0).radius(4.0).fill(p.line).child(el().w(w.max(2.0)).h(8.0).radius(4.0).fill(color))
}

fn leaderboard(s: &App) -> El {
    let p = ui::pal();
    let b = &s.bench;
    let sm = &b.summary;
    let mut board: Vec<Entry> = sm.board.clone();
    board.push(Entry {
        name: "ferrocrawl".into(),
        ours: true,
        score: sm.score,
        recall: sm.recall,
        f1: sm.f1,
        p50_ms: (sm.done > 0).then_some(sm.latency.p50),
        p95_ms: (sm.done > 0).then_some(sm.latency.p95),
    });
    board.sort_by(|a, b| b.score.unwrap_or(-1.0).total_cmp(&a.score.unwrap_or(-1.0)));
    let rank = board.iter().position(|e| e.ours).map(|i| i + 1);
    let max_score = board.iter().filter_map(|e| e.score).fold(0.0_f64, f64::max).max(1.0);
    let fastest = board.iter().filter_map(|e| e.p50_ms).min();

    let mut rows = col().w_full().gap(8.0);
    rows = rows.child(
        row()
            .w_full()
            .gap(10.0)
            .child(text("PROVIDER").font(LABEL).color(p.faint).w(150.0))
            .child(text(score_head(b.suite)).font(LABEL).color(p.faint).w(BAR_W + 52.0))
            .child(text("P50").font(LABEL).color(p.faint)),
    );
    for e in &board {
        let ink = if e.ours { p.accent } else { p.text };
        let score = e.score.map(|v| format!("{v:.1}%")).unwrap_or_else(|| if e.ours && b.suite.judged() { "judge".into() } else { "—".into() });
        let latency = e.p50_ms.map(ms).unwrap_or_else(|| "—".into());
        let fast = e.p50_ms.is_some() && e.p50_ms == fastest;
        let mut r = row()
            .w_full()
            .gap(10.0)
            .py(4.0)
            .px(6.0)
            .radius(6.0)
            .center_y()
            .child(text(e.name.clone()).font(if e.ours { ROW_TITLE } else { SMALL }).color(ink).truncate().w(144.0))
            .child(bar(&p, e.score.unwrap_or(0.0) / max_score, if e.ours { p.accent } else { p.dim.alpha(0.55) }))
            .child(text(score).font(MONO).color(ink).w(46.0))
            .child(text(latency).font(MONO).color(if fast { p.good } else { p.dim }));
        if e.ours {
            r = r.fill(p.selected);
        }
        rows = rows.child(r);
    }
    let aside = match rank {
        Some(r) if sm.score.is_some() => format!("ferrocrawl #{r} of {}", board.len()),
        _ => format!("{} providers", board.len()),
    };
    let mut body = col().w_full().gap(10.0).child(rows);
    if b.suite == Suite::Webcrawlerapi {
        let f1s: Vec<(String, f64, bool)> = board.iter().filter_map(|e| e.f1.map(|f| (e.name.clone(), f, e.ours))).collect();
        let mut f1_rows = col().w_full().gap(6.0).child(text("EXTRACTION F1 (higher = less page chrome leaked)").font(LABEL).color(p.faint));
        let mut sorted = f1s.clone();
        sorted.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (name, f, ours) in sorted {
            f1_rows = f1_rows.child(
                row()
                    .gap(10.0)
                    .center_y()
                    .child(text(name).font(SMALL).color(if ours { p.accent } else { p.dim }).truncate().w(144.0))
                    .child(bar(&p, f, if ours { p.accent } else { p.info.alpha(0.6) }))
                    .child(text(format!("{f:.3}")).font(MONO).color(if ours { p.accent } else { p.dim })),
            );
        }
        body = body.child(el().h(1.0).w_full().fill(p.line)).child(f1_rows);
    }
    card(&p, "card-board".into(), "Leaderboard", aside, body)
}

fn score_head(suite: Suite) -> &'static str {
    match suite {
        Suite::Webcrawlerapi => "COVERAGE",
        Suite::Trace => "USEFUL CONTENT",
        Suite::Tinyfish => "USABLE",
    }
}

fn categories(s: &App) -> El {
    let p = ui::pal();
    let b = &s.bench;
    let sm = &b.summary;
    let mut rows = col().w_full().gap(8.0);
    let judged_ready = !b.suite.judged() || sm.judged > 0;
    // Before judging, show what came back per category; after, what passed.
    let mut cats: Vec<(String, usize, usize)> = Vec::new();
    if b.suite == Suite::Webcrawlerapi || !judged_ready {
        let mut m: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        for r in &b.results {
            let e = m.entry(r.category.clone()).or_default();
            e.0 += usize::from(r.status == "ok" && (b.suite != Suite::Webcrawlerapi || r.covered == Some(true)));
            e.1 += usize::from(r.status != "skipped");
        }
        cats.extend(m.into_iter().map(|(c, (a, n))| (c, a, n)));
    } else {
        cats.extend(sm.categories.iter().map(|(c, (a, n))| (c.clone(), *a, *n)));
    }
    for (c, a, n) in &cats {
        let frac = *a as f64 / (*n).max(1) as f64;
        let color = if frac >= 0.85 { p.good } else if frac >= 0.6 { p.accent } else { p.bad };
        rows = rows.child(
            row()
                .gap(10.0)
                .center_y()
                .child(text(c.replace('_', " ")).font(SMALL).color(p.dim).truncate().w(144.0))
                .child(bar(&p, frac, color))
                .child(text(format!("{:.0}%", 100.0 * frac)).font(MONO).color(p.text).w(40.0))
                .child(text(format!("{a}/{n}")).font(MONO).color(p.faint)),
        );
    }
    if cats.is_empty() {
        rows = rows.child(text("Run the suite to fill this in.").font(SMALL).color(p.faint));
    }
    let title = match (b.suite, judged_ready) {
        (Suite::Webcrawlerapi, _) => "Covered by category",
        (_, true) => "Passed by category",
        (_, false) => "Returned by category (not judged yet)",
    };
    card(&p, "card-cats".into(), title, format!("{} categories", cats.len()), rows)
}

fn histogram(s: &App) -> El {
    let p = ui::pal();
    const EDGES: [(u64, &str); 8] = [(250, "<250ms"), (500, "<500"), (1000, "<1s"), (2000, "<2s"), (4000, "<4s"), (8000, "<8s"), (16000, "<16s"), (u64::MAX, "16s+")];
    let mut counts = [0usize; 8];
    for r in s.bench.results.iter().filter(|r| r.status != "skipped") {
        let i = EDGES.iter().position(|(e, _)| r.ms < *e).unwrap_or(7);
        counts[i] += 1;
    }
    let max = counts.iter().copied().max().unwrap_or(0).max(1);
    let mut bars = row().w_full().gap(8.0).items_end().h(120.0);
    for (i, (_, label)) in EDGES.iter().enumerate() {
        let h = 86.0 * counts[i] as f32 / max as f32;
        let color = if i <= 2 { p.good } else if i <= 4 { p.accent } else { p.bad };
        bars = bars.child(
            col()
                .grow(1.0)
                .gap(4.0)
                .items_center()
                .child(text(fmt_n(counts[i])).font(MONO).color(p.faint))
                .child(el().w_full().h(h.max(2.0)).radius(3.0).fill(color.alpha(0.75)))
                .child(text(*label).font(MONO).color(p.faint)),
        );
    }
    let fast = counts[..3].iter().sum::<usize>();
    let total = counts.iter().sum::<usize>().max(1);
    card(&p, "card-hist".into(), "Latency distribution", format!("{:.0}% under 1s", 100.0 * fast as f64 / total as f64), bars)
}

fn run_info(s: &App, p: &Pal) -> El {
    let b = &s.bench;
    let mut c = col().w_full().gap(5.0);
    let line = |k: &str, v: String| row().gap(8.0).child(text(k.to_string()).font(SMALL).color(p.faint).w(90.0)).child(text(v).font(MONO).color(p.dim).truncate());
    c = c.child(line("source", if b.from_disk { "saved run".into() } else if b.running() || b.phase == Phase::Done { "live".into() } else { "—".into() }));
    if let Some(d) = &b.dir {
        c = c.child(line("folder", d.display().to_string()));
    }
    c = c.child(line("judged", format!("{} pages", fmt_n(b.summary.judged))));
    c = c.child(line("network", "this machine, direct (no proxy)".into()));
    card(p, "card-run".into(), "Run", String::new(), c)
}

// ------------------------------------------------------------------ results

fn results(s: &App, p: &Pal, k: &Kit) -> El {
    let b = &s.bench;
    let visible = b.visible();
    let filter = SegmentedControl::new(&FILTERS, Some(b.filter))
        .label("Filter")
        .fit()
        .on_pick(|s: &mut App, i, _: &mut Tx| s.bench.filter = i)
        .build(k);
    let header = row()
        .w_full()
        .px(16.0)
        .py(10.0)
        .gap(12.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(text("Pages").font(ROW_TITLE).color(p.text))
        .child(text(fmt_n(visible.len())).font(MONO).color(p.faint))
        .child(spacer())
        .child(filter);

    let list = if visible.is_empty() {
        ui::empty(p, "No pages yet", "Pick a suite and press Run. A saved run loads by itself.")
    } else {
        let version = (b.results.len(), b.filter, b.revision);
        col().key("bench-rows").w_full().growth(GrowthSpec::default()).virtual_rows_version(version).virtual_rows(
            visible.len(),
            30.0,
            move |s: &App, i| {
                let v = s.bench.visible();
                format!("br-{}", v.get(i).copied().unwrap_or(i)).into()
            },
            move |s: &App, i| {
                let v = s.bench.visible();
                let idx = v.get(i).copied().unwrap_or(0);
                result_row(s, idx)
            },
        )
    };

    col()
        .key("bench-results")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .child(header)
        .child(col().key("bench-results-scroll").w_full().grow(1.0).shrink().min_h(0.0).child(list).scroll(ScrollSpec { stick_bottom: false }).scroll_edge_fade(10.0))
        .child(detail(s, p))
}

fn result_row(s: &App, idx: usize) -> El {
    let p = ui::pal();
    let b = &s.bench;
    let r = &b.results[idx];
    let on = b.selected == Some(idx);
    let failed = r.status == "error" || b.failed_quality(r);
    let dot = if r.status == "skipped" { p.faint } else if failed { p.bad } else { p.good };
    let url = r.url.split("://").nth(1).unwrap_or(&r.url).to_string();
    let quality = match b.suite {
        Suite::Webcrawlerapi => r.recall.map(|x| format!("rec {:.0}%", 100.0 * x)).unwrap_or_default(),
        _ => b.verdicts.get(&r.id).map(|v| format!("{} · {}", v.content, v.identity)).unwrap_or_default(),
    };
    let line = row()
        .key(format!("br-{idx}"))
        .w_full()
        .px(14.0)
        .py(6.0)
        .gap(10.0)
        .center_y()
        .fill(if on { p.selected } else { Color::TRANSPARENT })
        .hover(if on { p.selected } else { p.hover })
        .on_press(move |s: &mut App| s.bench.pick(idx))
        .child(el().size(7.0, 7.0).radius(3.5).fill(dot))
        .child(
            text(if b.suite == Suite::Webcrawlerapi { format!("#{}", r.id) } else { r.category.replace('_', " ") })
                .font(MONO)
                .color(p.faint)
                .truncate()
                .w(if b.suite == Suite::Webcrawlerapi { 48.0 } else { 120.0 }),
        )
        .child(text(url).font(SMALL).color(p.text).truncate_middle().grow(1.0).shrink().min_w(0.0))
        .child(text(quality).font(MONO).color(if failed { p.bad } else { p.dim }).w(170.0).truncate())
        // A fixed slot keeps the columns lined up whether or not the badge shows.
        .child(row().w(30.0).child(if r.rendered { badge("JS", p.info) } else { el() }));
    line.child(text(if r.status == "skipped" { "skip".into() } else { ms(r.ms) }).font(MONO).color(if r.ms > 8000 { p.bad } else { p.dim }).w(64.0))
        .child(text(if r.chars > 0 { format!("{}k", (r.chars + 500) / 1000) } else { "—".into() }).font(MONO).color(p.faint).w(44.0))
}

fn detail(s: &App, p: &Pal) -> El {
    let b = &s.bench;
    let Some(r) = b.selected.and_then(|i| b.results.get(i)) else {
        return row()
            .key("bench-detail")
            .w_full()
            .h(44.0)
            .px(16.0)
            .center_y()
            .border_sides([1.0, 0.0, 0.0, 0.0], p.line)
            .child(icon(&BOLT, 13.0).color(p.faint))
            .child(el().w(8.0))
            .child(text("Select a page to see its numbers and the Markdown that came back.").font(SMALL).color(p.faint));
    };
    let stat = |k: &str, v: String| col().gap(2.0).child(text(k.to_string()).font(LABEL).color(p.faint)).child(text(v).font(MONO).color(p.text));
    let mut stats = row().gap(22.0).child(stat("STATUS", r.status.clone())).child(stat("TIME", ms(r.ms))).child(stat("CHARS", fmt_n(r.chars)));
    if let Some(x) = r.recall {
        stats = stats.child(stat("RECALL", format!("{:.1}%", 100.0 * x))).child(stat("F1", format!("{:.3}", r.f1.unwrap_or(0.0))));
    }
    if let Some(v) = b.verdicts.get(&r.id) {
        stats = stats.child(stat("CONTENT", v.content.clone())).child(stat("IDENTITY", v.identity.clone()));
        if let Some(sig) = v.signal {
            stats = stats.child(stat("SIGNAL", format!("{sig:.0}%")));
        }
    }
    stats = stats.child(stat("RENDER", if r.rendered { "browser".into() } else { "http".into() }));
    let body_text: String = match (&r.error, &b.detail) {
        (Some(e), _) => e.clone(),
        (None, Some(md)) => md.chars().take(4000).collect(),
        _ => "(no saved Markdown)".into(),
    };
    let color = if r.error.is_some() { p.bad } else { p.dim };
    let idx = b.selected.unwrap_or(0);
    let excerpt = memo(format!("bd-{idx}"), (idx, body_text.len()), move || text(body_text).font(MONO.lh(17.0)).color(color).selectable(true).w_full());
    col()
        .key("bench-detail")
        .w_full()
        .h(270.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], p.line)
        .fill(p.panel)
        .child(
            col()
                .w_full()
                .px(16.0)
                .pt(12.0)
                .pb(10.0)
                .gap(8.0)
                .child(text(r.url.clone()).font(SMALL).color(p.accent).truncate_middle().selectable(true))
                .child(stats),
        )
        .child(col().key(format!("bench-detail-scroll-{idx}")).w_full().grow(1.0).shrink().min_h(0.0).px(16.0).pb(12.0).child(excerpt).scroll(ScrollSpec { stick_bottom: false }))
}
