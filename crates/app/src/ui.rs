//! ferrocrawl's look: a toolbar with the URL field and run controls, a strip of live stats, the
//! results list on the left and the selected page's Markdown on the right. Dark, rust accent.

use rune::{Color, Cx, El, GrowthSpec, Icon, ScrollSpec, TextStyle, Tx, col, el, icon, memo, oklch, oklcha, row, spacer, text, text_input};
use rune_kit::{Button, ButtonVariant, Kit, Markdown, PaletteSeed, SegmentedControl, Spinner};

use crate::{App, LIMITS, MODES, Mode, RENDERS, Row, Run};

pub struct Pal {
    pub bg: Color,
    pub panel: Color,
    pub well: Color,
    pub line: Color,
    pub hover: Color,
    pub selected: Color,
    pub text: Color,
    pub dim: Color,
    pub faint: Color,
    pub accent: Color,
    pub accent_soft: Color,
    pub good: Color,
    pub bad: Color,
    pub info: Color,
}

pub fn pal() -> Pal {
    Pal {
        bg: oklch(0.18, 0.006, 260.0),
        panel: oklch(0.205, 0.007, 260.0),
        well: oklch(0.245, 0.008, 260.0),
        line: oklcha(1.0, 0.0, 0.0, 0.08),
        hover: oklcha(1.0, 0.0, 0.0, 0.05),
        selected: oklcha(0.70, 0.16, 45.0, 0.16),
        text: oklch(0.96, 0.004, 260.0),
        dim: oklch(0.76, 0.006, 260.0),
        faint: oklch(0.56, 0.008, 260.0),
        accent: oklch(0.71, 0.165, 45.0),
        accent_soft: oklcha(0.71, 0.165, 45.0, 0.16),
        good: oklch(0.76, 0.14, 150.0),
        bad: oklch(0.68, 0.19, 25.0),
        info: oklch(0.74, 0.11, 240.0),
    }
}

pub fn kit() -> Kit {
    Kit::from_palette(PaletteSeed { bg: oklch(0.18, 0.006, 260.0), fg: oklch(0.96, 0.004, 260.0), accent: oklch(0.71, 0.165, 45.0) })
}

pub const BRAND: TextStyle = TextStyle::new(15.0, 720.0).tracking(-0.3);
pub const BODY: TextStyle = TextStyle::new(13.0, 400.0);
pub const ROW_TITLE: TextStyle = TextStyle::new(13.0, 560.0);
pub const SMALL: TextStyle = TextStyle::new(11.5, 450.0);
pub const LABEL: TextStyle = TextStyle::new(10.5, 650.0).tracking(0.8);
pub const STAT: TextStyle = TextStyle::new(20.0, 680.0).tracking(-0.4);
pub const MONO: TextStyle = TextStyle::new(11.5, 450.0).mono();
pub const HEAD: TextStyle = TextStyle::new(17.0, 700.0).tracking(-0.3);

macro_rules! ico {
    ($name:ident, $body:expr) => {
        pub static $name: Icon = Icon { name: stringify!($name), view: 24.0, body: $body };
    };
}
ico!(GLOBE, r#"<circle cx="12" cy="12" r="8.5" stroke="currentColor" stroke-width="1.8" fill="none"/><path d="M3.5 12h17M12 3.5c2.4 2.4 3.6 5.2 3.6 8.5s-1.2 6.1-3.6 8.5c-2.4-2.4-3.6-5.2-3.6-8.5s1.2-6.1 3.6-8.5z" stroke="currentColor" stroke-width="1.8" fill="none"/>"#);
ico!(BOLT, r#"<path d="M13.2 2.8 5.6 13.1a.6.6 0 0 0 .5 1h4.9l-1.3 7.1 7.7-10.4a.6.6 0 0 0-.5-1h-4.9z" fill="currentColor"/>"#);
ico!(WARN, r#"<circle cx="12" cy="12" r="8.5" stroke="currentColor" stroke-width="1.9" fill="none"/><path d="M12 7.5v5.5" stroke="currentColor" stroke-width="2" stroke-linecap="round"/><circle cx="12" cy="16.3" r="1.2" fill="currentColor"/>"#);
ico!(PAGE, r#"<path d="M6.5 3.5h7l4 4v13h-11z" stroke="currentColor" stroke-width="1.8" fill="none" stroke-linejoin="round"/><path d="M9 12h6M9 15.5h6" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/>"#);

pub fn row_key(i: usize) -> String {
    format!("row-{i}")
}

pub fn view(s: &App, _cx: &mut Cx<App>) -> El {
    let p = pal();
    let k = kit();
    if s.mode == Mode::Bench {
        let base = col()
            .key("root")
            .w_full()
            .h_full()
            .fill(p.bg)
            .child(toolbar(s, &p, &k))
            .child(crate::bench_ui::body(s, &p, &k));
        return rune_kit::root(base, Vec::new());
    }
    if s.mode == Mode::Intro {
        let base = col().key("root").w_full().h_full().fill(p.bg).child(toolbar(s, &p, &k)).child(crate::intro_ui::body(s, &p, &k));
        return rune_kit::root(base, Vec::new());
    }
    let body = row()
        .key("body")
        .w_full()
        .grow(1.0)
        .shrink()
        .min_h(0.0)
        .child(results(s, &p))
        .child(preview(s, &p, &k));
    let base = col().key("root").w_full().h_full().fill(p.bg).child(toolbar(s, &p, &k)).child(stats(s, &p)).child(body);
    rune_kit::root(base, Vec::new())
}

// ------------------------------------------------------------------ toolbar

fn toolbar(s: &App, p: &Pal, k: &Kit) -> El {
    let logo = el()
        .size(26.0, 26.0)
        .radius(7.0)
        .gradient(oklch(0.76, 0.16, 55.0), oklch(0.6, 0.18, 35.0))
        .center()
        .child(icon(&BOLT, 15.0).color(Color::WHITE));
    let brand = row().gap(9.0).center_y().child(logo).child(text("ferrocrawl").font(BRAND).color(p.text));

    let intro_mode = s.mode == Mode::Intro;
    let focused = if intro_mode { s.intro.input.focused } else { s.url.focused };
    let input = if intro_mode {
        text_input(&s.intro.input, BODY)
            .key("intro-input")
            .single_line()
            .grow(1.0)
            .color(p.text)
            .placeholder(crate::intro::PLACEHOLDER[s.intro.action], p.faint)
            .on_edit(|s: &mut App| &mut s.intro.input)
            .on_submit(|s: &mut App, tx: &mut Tx| {
                s.start();
                tx.blur();
            })
    } else {
        text_input(&s.url, BODY)
            .key("url")
            .single_line()
            .grow(1.0)
            .color(p.text)
            .placeholder("https://example.com", p.faint)
            .on_edit(|s: &mut App| &mut s.url)
            .on_submit(|s: &mut App, tx: &mut Tx| {
                s.start();
                tx.blur();
            })
    };
    let field = row()
        .key("url-box")
        .grow(1.0)
        .shrink()
        .min_w(200.0)
        .h(34.0)
        .px(11.0)
        .gap(8.0)
        .radius(9.0)
        .fill(p.well)
        .border(if focused { 2.0 } else { 1.0 }, if focused { p.accent.alpha(0.7) } else { p.line })
        .center_y()
        .child(icon(&GLOBE, 15.0).color(p.faint))
        .child(input);

    let modes = SegmentedControl::new(&MODES, Some(s.mode.index()))
        .label("Mode")
        .fit()
        .on_pick(|s: &mut App, i, _: &mut Tx| s.set_mode(i))
        .build(k);
    let renders = SegmentedControl::new(&RENDERS, Some(s.render))
        .label("Rendering")
        .on_pick(|s: &mut App, i, _: &mut Tx| s.render = i)
        .build(k);
    let limit_labels: Vec<String> = LIMITS.iter().map(|n| n.to_string()).collect();
    let limits = SegmentedControl::new(&limit_labels, Some(s.limit))
        .label("Page limit")
        .on_pick(|s: &mut App, i, _: &mut Tx| s.limit = i)
        .build(k);

    let go = if s.mode == Mode::Bench {
        crate::bench_ui::run_button(s, k)
    } else if intro_mode {
        let label = if s.intro.running() { "Working…" } else { crate::intro::GO[s.intro.action] };
        Button::new(label).variant(ButtonVariant::Submit).on_press(|s: &mut App| s.start()).build(k)
    } else if s.running() {
        Button::new("Stop").variant(ButtonVariant::Danger).on_press(|s: &mut App| s.stop()).build(k)
    } else {
        Button::new(MODES[s.mode.index()]).variant(ButtonVariant::Submit).on_press(|s: &mut App| s.start()).build(k)
    };

    let mut controls = row().gap(10.0).center_y().child(modes);
    if s.mode == Mode::Bench {
        controls = controls.child(crate::bench_ui::controls(s, p, k));
    } else if intro_mode {
        let actions = SegmentedControl::new(&crate::intro::ACTIONS, Some(s.intro.action))
            .label("Action")
            .fit()
            .on_pick(|s: &mut App, i, _: &mut Tx| s.intro.action = i)
            .build(k);
        controls = controls.child(actions);
    } else {
        controls = controls.child(renders);
    }
    if s.mode == Mode::Crawl {
        controls = controls.child(text("Limit").font(SMALL).color(p.faint)).child(limits);
    }

    col()
        .key("toolbar")
        .w_full()
        .px(18.0)
        .pt(14.0)
        .pb(12.0)
        .gap(12.0)
        .fill(p.panel)
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(row().w_full().gap(16.0).center_y().child(brand).child(if s.mode == Mode::Bench {
            row()
                .grow(1.0)
                .shrink()
                .min_w(200.0)
                .h(34.0)
                .px(12.0)
                .radius(9.0)
                .fill(p.well)
                .border(1.0, p.line)
                .center_y()
                .child(text(crate::bench_ui::suite_blurb(s.bench.suite)).font(SMALL).color(p.dim).truncate())
        } else {
            field
        }).child(go))
        .child(controls)
}

// ------------------------------------------------------------------ stats

fn stats(s: &App, p: &Pal) -> El {
    let secs = s.elapsed().as_secs_f64();
    let pages = s.pages();
    let rate = if secs > 0.05 { pages as f64 / secs } else { 0.0 };
    let ok: Vec<&Row> = s.rows.iter().filter(|r| r.ok).collect();
    let avg = if ok.is_empty() { 0 } else { ok.iter().map(|r| r.ms).sum::<u64>() / ok.len() as u64 };
    let rendered = ok.iter().filter(|r| r.rendered).count();
    let words: usize = ok.iter().map(|r| r.words).sum();

    let tile = |key: &'static str, label: &'static str, value: String, color: Color| {
        col()
            .key(key)
            .gap(2.0)
            .min_w(96.0)
            .child(text(label).font(LABEL).color(p.faint))
            .child(text(value).font(STAT.features("'tnum' 1")).color(color))
    };
    let (status, status_color) = match &s.run {
        Run::Idle => ("Ready".to_string(), p.dim),
        Run::Running { .. } => ("Running".to_string(), p.accent),
        Run::Finished { note, .. } => (note.clone().unwrap_or_else(|| "Done".into()), p.good),
    };
    let mut status_el = row().gap(8.0).center_y();
    if s.running() {
        status_el = status_el.child(Spinner::new().size(14.0).color(p.accent).build(&kit()));
    }
    status_el = status_el.child(text(status).font(SMALL).color(status_color).truncate());

    let found = if s.mode == Mode::Map { s.links.len() } else { pages };
    row()
        .key("stats")
        .w_full()
        .px(22.0)
        .py(12.0)
        .gap(26.0)
        .center_y()
        .border_sides([0.0, 0.0, 1.0, 0.0], p.line)
        .child(tile("st-pages", if s.mode == Mode::Map { "URLS" } else { "PAGES" }, fmt_n(found), p.text))
        .child(tile("st-rate", "PAGES / SEC", format!("{rate:.1}"), p.accent))
        .child(tile("st-time", "ELAPSED", format!("{secs:.2}s"), p.text))
        .child(tile("st-avg", "AVG PAGE", format!("{avg} ms"), p.text))
        .child(tile("st-words", "WORDS", fmt_n(words), p.text))
        .child(tile("st-js", "RENDERED", rendered.to_string(), p.info))
        .child(tile("st-err", "ERRORS", s.errors().to_string(), if s.errors() > 0 { p.bad } else { p.text }))
        .child(spacer())
        .child(status_el.max_w(260.0))
}

pub fn fmt_n(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ------------------------------------------------------------------ results list

fn results(s: &App, p: &Pal) -> El {
    let (title, count) = if s.mode == Mode::Map { ("URLs", s.links.len()) } else { ("Pages", s.rows.len()) };
    let header = row()
        .w_full()
        .px(16.0)
        .py(11.0)
        .center_y()
        .child(text(title).font(ROW_TITLE).color(p.text))
        .child(spacer())
        .child(text(fmt_n(count)).font(MONO).color(p.faint));

    let list = if s.mode == Mode::Map {
        if s.links.is_empty() {
            empty(p, "No URLs yet", "Map lists a site's URLs from its sitemaps and links.")
        } else {
            col().key("links").w_full().growth(GrowthSpec::default()).virtual_rows(
                s.links.len(),
                26.0,
                |s: &App, i| s.links[i].as_str().into(),
                |s: &App, i| {
                    row()
                        .w_full()
                        .px(16.0)
                        .py(4.0)
                        .child(text(s.links[i].clone()).font(MONO).color(pal().dim).truncate_middle())
                },
            )
        }
    } else if s.rows.is_empty() {
        let hint = match s.mode {
            Mode::Scrape => "Scrape turns one page into clean Markdown.",
            Mode::Crawl => "Crawl follows links and streams pages in as they land.",
            Mode::Map | Mode::Intro | Mode::Bench => "",
        };
        empty(p, "Nothing yet", hint)
    } else {
        col().key("rows").w_full().growth(GrowthSpec::default()).virtual_rows_version(s.rows.len()).virtual_rows(
            s.rows.len(),
            52.0,
            |_: &App, i| row_key(i).into(),
            |s: &App, i| result_row(s, i),
        )
    };

    col()
        .key("results")
        .w(430.0)
        .h_full()
        .min_h(0.0)
        .fill(p.panel)
        .border_sides([0.0, 1.0, 0.0, 0.0], p.line)
        .child(header)
        .child(col().key("results-scroll").w_full().grow(1.0).shrink().min_h(0.0).child(list).scroll(ScrollSpec { stick_bottom: false }).scroll_edge_fade(12.0))
}

fn result_row(s: &App, i: usize) -> El {
    let p = pal();
    let r = &s.rows[i];
    let on = s.selected == Some(i);
    let (glyph, color) = if r.ok { (&PAGE, p.dim) } else { (&WARN, p.bad) };
    let mut meta = row().gap(10.0).center_y();
    if r.ok {
        meta = meta
            .child(text(format!("{} ms", r.ms)).font(MONO).color(p.faint))
            .child(text(format!("{} KB", r.kb)).font(MONO).color(p.faint))
            .child(text(format!("{} words", fmt_n(r.words))).font(MONO).color(p.faint));
        if r.depth > 0 {
            meta = meta.child(text(format!("depth {}", r.depth)).font(MONO).color(p.faint));
        }
        if r.rendered {
            meta = meta.child(badge("JS", p.info));
        }
        if r.cached {
            meta = meta.child(badge("cache", p.good));
        }
    } else {
        meta = meta.child(text(r.body.to_string()).font(MONO).color(p.bad).truncate());
    }
    let url_tail = r.url.split("://").nth(1).unwrap_or(&r.url).to_string();
    row()
        .key(row_key(i))
        .w_full()
        .px(10.0)
        .py(3.0)
        .child(
            row()
                .w_full()
                .px(8.0)
                .py(7.0)
                .gap(10.0)
                .radius(8.0)
                .fill(if on { p.selected } else { Color::TRANSPARENT })
                .hover(if on { p.selected } else { p.hover })
                .on_press(move |s: &mut App| s.selected = Some(i))
                .child(icon(glyph, 15.0).color(if on { p.accent } else { color }))
                .child(
                    col()
                        .grow(1.0)
                        .shrink()
                        .min_w(0.0)
                        .gap(3.0)
                        .child(text(r.title.clone()).font(ROW_TITLE).color(p.text).truncate())
                        .child(text(url_tail).font(SMALL).color(p.faint).truncate_middle())
                        .child(meta),
                ),
        )
}

pub fn badge(label: &'static str, c: Color) -> El {
    el().px(5.0).py(1.0).radius(4.0).fill(c.alpha(0.16)).child(text(label).font(LABEL).color(c))
}

pub fn empty(p: &Pal, head: &'static str, sub: &'static str) -> El {
    col()
        .w_full()
        .pt(80.0)
        .px(30.0)
        .gap(8.0)
        .items_center()
        .child(el().size(56.0, 56.0).radius(28.0).fill(p.accent_soft).center().child(icon(&BOLT, 24.0).color(p.accent)))
        .child(el().h(4.0))
        .child(text(head).font(ROW_TITLE).color(p.text))
        .child(text(sub).font(SMALL).color(p.faint))
}

// ------------------------------------------------------------------ preview

/// Markdown past this is shown as raw text: a 300 KB page laid out as rich text helps nobody.
const RICH_LIMIT: usize = 60_000;

fn preview(s: &App, p: &Pal, k: &Kit) -> El {
    let Some(r) = s.selected.and_then(|i| s.rows.get(i)) else {
        let msg = if s.mode == Mode::Map && !s.links.is_empty() {
            empty(p, "Site mapped", "Copy copies every URL, one per line.")
        } else {
            empty(p, "Select a page", "Its Markdown shows here. ↑ ↓ move between pages.")
        };
        let mut c = col().key("preview").grow(1.0).shrink().min_w(0.0).h_full();
        if s.mode == Mode::Map && !s.links.is_empty() {
            c = c.child(
                row().w_full().px(22.0).py(12.0).child(spacer()).child(
                    Button::new("Copy URLs").variant(ButtonVariant::Outline).on_press_tx(|s: &mut App, tx: &mut Tx| s.copy_selected(tx)).build(k),
                ),
            );
        }
        return c.child(msg);
    };

    let index = s.selected.unwrap_or(0);
    let raw = s.raw || !r.ok || r.body.len() > RICH_LIMIT;
    let view_toggle = SegmentedControl::new(&["Rendered", "Markdown"], Some(usize::from(s.raw)))
        .label("Preview")
        .on_pick(|s: &mut App, i, _: &mut Tx| s.raw = i == 1)
        .build(k);
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
                .child(text(r.title.clone()).font(HEAD).color(p.text).truncate())
                .child(text(r.url.clone()).font(SMALL).color(p.accent).truncate_middle().selectable(true)),
        )
        .child(view_toggle)
        .child(Button::new("Copy").variant(ButtonVariant::Outline).on_press_tx(|s: &mut App, tx: &mut Tx| s.copy_selected(tx)).build(k));

    let body_src = r.body.clone();
    let content = if raw {
        let color = if r.ok { p.dim } else { p.bad };
        memo(format!("raw-{index}"), (index, body_src.len(), r.ok), move || {
            text(body_src.to_string()).font(MONO.lh(18.0)).color(color).selectable(true).w_full()
        })
    } else {
        let k2 = kit();
        memo(format!("md-{index}"), (index, body_src.len()), move || Markdown::new(body_src.to_string()).selectable(true).build(&k2))
    };

    col()
        .key("preview")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .child(header)
        .child(
            col()
                .key(format!("preview-scroll-{index}"))
                .w_full()
                .grow(1.0)
                .shrink()
                .min_h(0.0)
                .px(28.0)
                .py(20.0)
                .child(col().w_full().max_w(860.0).child(content))
                .scroll(ScrollSpec { stick_bottom: false })
                .scroll_edge_fade(14.0),
        )
}
