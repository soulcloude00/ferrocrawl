//! The Intro view.
//!
//! ┌ toolbar: who to reach / URL / file / repo · Find | Harvest | Import | GitHub ─┐
//! │ stats: people · organizations · links · you · status                          │
//! ├ routes: "ask Guy A" ranked ──────┬ the selected route, hop by hop ────────────┤
//! │ (or: sources to add, and the log)│ evidence for each hop · a drafted message  │
//! └──────────────────────────────────┴────────────────────────────────────────────┘

use intro::{Node, NodeKind};
use rune::{Color, El, ScrollSpec, Tx, col, el, icon, row, spacer, text};
use rune_kit::{Button, ButtonVariant, Kit, Spinner};

use crate::App;
use crate::ui::{self, BOLT, HEAD, LABEL, MONO, Pal, ROW_TITLE, SMALL, STAT, badge, fmt_n};

pub fn body(s: &App, p: &Pal, k: &Kit) -> El {
    col()
        .key("intro")
        .w_full()
        .grow(1.0)
        .shrink()
        .min_h(0.0)
        .child(stats(s, p))
        .child(row().key("intro-main").w_full().grow(1.0).shrink().min_h(0.0).child(left(s, p)).child(right(s, p, k)))
}

fn stats(s: &App, p: &Pal) -> El {
    let st = &s.intro.stats;
    let tile = |key: &'static str, label: &'static str, value: String, color: Color| {
        col().key(key).gap(2.0).min_w(96.0).child(text(label).font(LABEL).color(p.faint)).child(text(value).font(STAT.features("'tnum' 1")).color(color))
    };
    let mut status = row().gap(8.0).center_y();
    if s.intro.running() {
        status = status.child(Spinner::new().size(14.0).color(p.accent).build(&ui::kit())).child(text("Working").font(SMALL).color(p.accent));
    } else if let Some((ok, line)) = s.intro.log.last() {
        status = status.child(text(line.clone()).font(SMALL).color(if *ok { p.good } else { p.bad }).truncate());
    } else {
        status = status.child(text(s.intro.db_path.display().to_string()).font(SMALL).color(p.faint).truncate_middle());
    }
    let sources = st.by_kind.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(" · ");
    row()
        .key("intro-stats")
        .w_full()
        .px(22.0)
        .py(12.0)
        .gap(26.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(tile("is-people", "PEOPLE", fmt_n(st.people), p.text))
        .child(tile("is-orgs", "ORGANIZATIONS", fmt_n(st.orgs), p.text))
        .child(tile("is-links", "LINKS", fmt_n(st.edges), p.accent))
        .child(col().key("is-kinds").gap(2.0).shrink().min_w(0.0).max_w(420.0).child(text("BY KIND").font(LABEL).color(p.faint)).child(text(if sources.is_empty() { "—".into() } else { sources }).font(SMALL).color(p.dim).truncate()))
        .child(spacer())
        .child(status.max_w(360.0))
}

// ------------------------------------------------------------------ left: routes or sources

fn left(s: &App, p: &Pal) -> El {
    let i = &s.intro;
    let (title, count) = match &i.result {
        Some(r) if !r.paths.is_empty() => ("Routes", r.paths.len()),
        Some(r) if !r.candidates.is_empty() => ("Did you mean", r.candidates.len()),
        _ => ("Add your network", 0),
    };
    let header = row()
        .w_full()
        .px(16.0)
        .py(11.0)
        .center_y()
        .child(text(title).font(ROW_TITLE).color(p.text))
        .child(spacer())
        .child(text(if count > 0 { count.to_string() } else { String::new() }).font(MONO).color(p.faint));

    let mut list = col().key("intro-list").w_full().gap(2.0).pb(12.0);
    match &i.result {
        Some(r) if !r.paths.is_empty() => {
            for (n, path) in r.paths.iter().enumerate() {
                list = list.child(route_row(s, n, path));
            }
        }
        Some(r) if !r.candidates.is_empty() => {
            for c in &r.candidates {
                list = list.child(candidate_row(c, p));
            }
        }
        Some(r) => {
            let msg = if r.targets.is_empty() {
                format!("Nobody called \"{}\" yet. Harvest a page that lists them (their team page) or import a network that includes them.", i.query)
            } else {
                format!("\"{}\" is in the graph, but no route reaches them yet. Import more of your network, or a friend's.", i.query)
            };
            list = list.child(col().w_full().px(16.0).py(10.0).child(text(msg).font(SMALL).color(p.dim).wrap()));
            list = list.child(sources(p));
        }
        None => list = list.child(sources(p)),
    }
    if !i.log.is_empty() {
        list = list.child(row().w_full().px(16.0).pt(14.0).pb(4.0).child(text("ACTIVITY").font(LABEL).color(p.faint)));
        for (ok, line) in i.log.iter().rev().take(12) {
            list = list.child(
                row()
                    .w_full()
                    .px(16.0)
                    .py(3.0)
                    .gap(8.0)
                    .child(text(if *ok { "✓" } else { "✗" }).font(MONO).color(if *ok { p.good } else { p.bad }))
                    .child(text(line.clone()).font(SMALL).color(p.dim).wrap().grow(1.0).shrink().min_w(0.0)),
            );
        }
    }

    col()
        .key("intro-left")
        .w(430.0)
        .h_full()
        .min_h(0.0)
        .fill(p.panel)
        .border_sides([0.0, 1.0, 0.0, 0.0], p.line)
        .child(header)
        .child(col().key("intro-left-scroll").w_full().grow(1.0).shrink().min_h(0.0).child(list).scroll(ScrollSpec { stick_bottom: false }).scroll_edge_fade(12.0))
}

fn route_row(s: &App, n: usize, path: &intro::IntroPath) -> El {
    let p = ui::pal();
    let on = s.intro.selected == n;
    let head = match &path.ask {
        Some(a) => format!("Ask {a}"),
        None => "You're directly connected".to_string(),
    };
    let hops = path.hops.len() - 1;
    let via = path.hops.get(1).and_then(|h| h.via.clone()).unwrap_or_default();
    row().key(format!("route-{n}")).w_full().px(10.0).py(2.0).child(
        row()
            .w_full()
            .px(10.0)
            .py(9.0)
            .gap(12.0)
            .radius(8.0)
            .fill(if on { p.selected } else { Color::TRANSPARENT })
            .hover(if on { p.selected } else { p.hover })
            .on_press(move |s: &mut App| s.intro.selected = n)
            .child(score_ring(path.score, &p))
            .child(
                col()
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0)
                    .gap(3.0)
                    .child(text(head).font(ROW_TITLE).color(p.text).truncate())
                    .child(text(path.hops[1..].iter().map(|h| h.node.name.as_str()).collect::<Vec<_>>().join(" → ")).font(SMALL).color(p.faint).truncate())
                    .child(row().gap(8.0).child(badge_s(format!("{hops} hop{}", if hops == 1 { "" } else { "s" }), p.info)).child(text(via).font(MONO).color(p.faint))),
            ),
    )
}

fn badge_s(label: String, c: Color) -> El {
    el().px(5.0).py(1.0).radius(4.0).fill(c.alpha(0.16)).child(text(label).font(LABEL).color(c))
}

fn score_ring(score: f64, p: &Pal) -> El {
    let c = if score >= 0.3 { p.good } else if score >= 0.12 { p.accent } else { p.faint };
    el().size(40.0, 40.0).radius(20.0).fill(c.alpha(0.14)).border(1.5, c.alpha(0.5)).center().child(text(format!("{:.0}", score * 100.0)).font(MONO).color(c))
}

fn candidate_row(c: &Node, p: &Pal) -> El {
    let id = c.id;
    let sub = [c.title.clone(), c.org.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    row().w_full().px(10.0).py(2.0).child(
        row()
            .w_full()
            .px(10.0)
            .py(8.0)
            .gap(10.0)
            .radius(8.0)
            .hover(p.hover)
            .on_press(move |s: &mut App| s.intro_find(format!("#{id}")))
            .child(text(if c.kind == NodeKind::Org { "▣" } else { "●" }).font(MONO).color(p.accent))
            .child(col().grow(1.0).shrink().min_w(0.0).gap(2.0).child(text(c.name.clone()).font(ROW_TITLE).color(p.text).truncate()).child(text(sub).font(SMALL).color(p.faint).truncate()))
            .child(text(format!("{} links", c.degree)).font(MONO).color(p.faint)),
    )
}

/// Where the data comes from, and how to get each file.
fn sources(p: &Pal) -> El {
    let item = |name: &'static str, how: &'static str, why: &'static str| {
        col()
            .w_full()
            .px(12.0)
            .py(10.0)
            .gap(3.0)
            .radius(8.0)
            .fill(p.well)
            .child(text(name).font(ROW_TITLE).color(p.text))
            .child(text(how).font(SMALL).color(p.dim).wrap())
            .child(text(why).font(SMALL).color(p.faint).wrap())
    };
    col()
        .w_full()
        .px(12.0)
        .pt(4.0)
        .gap(8.0)
        .child(item("Email · .mbox", "takeout.google.com → Mail → export. Import the .mbox.", "Strongest signal: who you actually write to, and who shares threads with you."))
        .child(item("Calendar · .ics", "Google Calendar → Settings → Import & export → Export (or Takeout).", "Who you've met, and how often."))
        .child(item("Contacts · .vcf", "Phone, Google or iCloud contacts → Export vCard.", "Everyone you saved."))
        .child(item("LinkedIn · Connections.csv", "Settings → Data privacy → Get a copy of your data → Connections.", "Ask friends for theirs too: that's how you learn who they know."))
        .child(item("GitHub", "Switch to GitHub: owner/repo, an org, or a user.", "People who build together know each other."))
        .child(item("Public pages", "Switch to Harvest: a fund's team or portfolio page, a speaker list.", "Maps who works where and who invested in what."))
}

// ------------------------------------------------------------------ right: the selected route

fn right(s: &App, p: &Pal, k: &Kit) -> El {
    let i = &s.intro;
    let Some(path) = i.result.as_ref().and_then(|r| r.paths.get(i.selected)) else {
        let (head, sub) = if i.stats.edges == 0 {
            ("Map your network first", "Import a file or harvest a page (switch modes above), then search for who you want to reach.")
        } else {
            ("Who do you want to reach?", "Type a person, a firm or an email and press Find path. Routes are ranked by how likely the intro is.")
        };
        return col().key("intro-right").grow(1.0).shrink().min_w(0.0).h_full().child(ui::empty(p, head, sub));
    };
    let target = path.hops.last().map(|h| h.node.name.clone()).unwrap_or_default();
    let header = row()
        .w_full()
        .px(22.0)
        .pt(14.0)
        .pb(12.0)
        .gap(12.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(3.0)
                .child(text(path.ask.as_ref().map(|a| format!("Ask {a} for an intro to {target}")).unwrap_or_else(|| format!("Reach out to {target} directly"))).font(HEAD).color(p.text).truncate())
                .child(text(format!("score {:.2} · the product of each hop's strength", path.score)).font(SMALL).color(p.faint)),
        )
        .child(Button::new("Copy intro request").variant(ButtonVariant::Outline).on_press_tx(|s: &mut App, tx: &mut Tx| {
            if let Some(d) = s.intro.draft() {
                tx.copy(d);
            }
        }).build(k));

    let mut chain = col().w_full().max_w(760.0).gap(0.0);
    for (n, h) in path.hops.iter().enumerate() {
        if n > 0 {
            chain = chain.child(
                row()
                    .w_full()
                    .pl(19.0)
                    .gap(14.0)
                    .child(el().w(2.0).h(44.0).fill(p.accent.alpha(0.4)))
                    .child(
                        col()
                            .py(4.0)
                            .gap(2.0)
                            .grow(1.0)
                            .shrink()
                            .min_w(0.0)
                            .child(text(h.via.clone().unwrap_or_default()).font(LABEL).color(p.accent))
                            .child(text(h.evidence.clone().unwrap_or_else(|| "no evidence recorded".into())).font(SMALL).color(p.faint).truncate().selectable(true)),
                    ),
            );
        }
        chain = chain.child(hop_card(&h.node, n == 0, n + 1 == path.hops.len(), path.ask.as_deref() == Some(h.node.name.as_str()), p));
    }
    let draft = i.draft().unwrap_or_default();
    let draft_box = col()
        .w_full()
        .max_w(760.0)
        .mt(22.0)
        .padding(16.0)
        .gap(8.0)
        .radius(10.0)
        .fill(p.well)
        .border(1.0, p.line)
        .child(text("DRAFT MESSAGE").font(LABEL).color(p.faint))
        .child(text(draft).font(SMALL.lh(18.0)).color(p.dim).wrap().selectable(true));

    col()
        .key("intro-right")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .child(header)
        .child(
            col()
                .key(format!("route-scroll-{}", i.selected))
                .w_full()
                .grow(1.0)
                .shrink()
                .min_h(0.0)
                .px(28.0)
                .py(20.0)
                .child(chain)
                .child(draft_box)
                .scroll(ScrollSpec { stick_bottom: false })
                .scroll_edge_fade(14.0),
        )
}

fn hop_card(n: &Node, is_you: bool, is_target: bool, is_ask: bool, p: &Pal) -> El {
    let ring = if is_ask { p.accent } else if is_target { p.good } else { p.line };
    let glyph = if n.kind == NodeKind::Org { "▣" } else { "●" };
    let sub = [n.title.clone(), n.org.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let mut tags = row().gap(6.0);
    if is_you {
        tags = tags.child(badge("YOU", p.info));
    }
    if is_ask {
        tags = tags.child(badge("ASK", p.accent));
    }
    if is_target {
        tags = tags.child(badge("TARGET", p.good));
    }
    let mut links = row().gap(10.0);
    if let Some(e) = &n.email {
        links = links.child(text(e.clone()).font(MONO).color(p.info).selectable(true));
    }
    if let Some(l) = &n.linkedin {
        links = links.child(text(format!("linkedin.com/{l}")).font(MONO).color(p.info).selectable(true).truncate_middle());
    }
    if let Some(u) = &n.url {
        links = links.child(text(u.clone()).font(MONO).color(p.info).selectable(true).truncate_middle());
    }
    row()
        .w_full()
        .padding(12.0)
        .gap(12.0)
        .radius(10.0)
        .fill(p.panel)
        .border(if is_ask || is_target { 1.5 } else { 1.0 }, ring)
        .center_y()
        .child(el().size(28.0, 28.0).radius(14.0).fill(ring.alpha(0.18)).center().child(if is_you { icon(&BOLT, 13.0).color(p.info) } else { text(glyph).font(MONO).color(if n.kind == NodeKind::Org { p.accent } else { p.dim }) }))
        .child(
            col()
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(3.0)
                .child(row().gap(8.0).center_y().child(text(n.name.clone()).font(ROW_TITLE).color(p.text).truncate()).child(tags))
                .child(text(if sub.is_empty() { format!("{} links", n.degree) } else { format!("{sub} · {} links", n.degree) }).font(SMALL).color(p.faint).truncate())
                .child(links),
        )
}
