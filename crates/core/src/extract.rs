//! HTML → clean Markdown.
//!
//! 1. One streaming `lol_html` pass drops scripts, styles and (optionally) page chrome, makes every
//!    link and image absolute (honouring `<base href>`), un-lazies images and collects links.
//! 2. `scraper` reads metadata and picks the main content element.
//! 3. `htmd` converts that element to Markdown.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::LazyLock;

use lol_html::{RewriteStrSettings, element, rewrite_str};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use url::Url;

/// Never content. Form controls go, but not `<form>` itself: ASP.NET sites wrap the whole page
/// in one.
const ALWAYS_DROP: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed",
    // Inline XBRL's hidden fact block at the top of SEC filings: machine tags, not text.
    "ix\\:header",
    "button", "input", "select", "textarea", "dialog",
];

/// Never the page's content, whatever level the main-content pass falls back to: cookie and
/// consent banners, accessibility-widget prompts.
const JUNK: &[&str] = &[
    "[id*=\"cookie\"]",
    "[class*=\"cookie\"]",
    "[id*=\"consent\"]",
    "[class*=\"consent\"]",
    "[class*=\"cmplz\"]",
    "[id*=\"onetrust\"]",
    "[class*=\"skip-link\"]",
    "[class*=\"acsb-\"]",
];

/// Page chrome, dropped when only the main content is wanted.
const CHROME: &[&str] = &[
    "nav",
    "footer",
    "aside",
    "body > header",
    "[role=\"navigation\"]",
    "[role=\"banner\"]",
    "[role=\"contentinfo\"]",
    "[role=\"complementary\"]",
    "[role=\"dialog\"]",
    "[aria-hidden=\"true\"]",
    "[hidden]",
    "[id*=\"cookie\"]",
    "[class*=\"cookie\"]",
    "[id*=\"consent\"]",
    "[class*=\"consent\"]",
    "[class*=\"newsletter\"]",
    "[class*=\"sidebar\"]",
    "[id*=\"sidebar\"]",
    "[class*=\"breadcrumb\"]",
    "[class*=\"skip-link\"]",
    "[class*=\"advert\"]",
    ".ad",
    ".ads",
    ".social-share",
    ".share-buttons",
];

pub struct Extracted {
    pub markdown: String,
    pub html: String,
    pub text: String,
    pub links: Vec<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub canonical: Option<String>,
    pub og_image: Option<String>,
}

static NOSCRIPT_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?noscript[^>]*>").unwrap());

pub fn extract(raw: &str, page_url: &str, only_main: bool, exclude: &[String]) -> Extracted {
    let mut ex = extract_once(raw, page_url, only_main, exclude);
    // Some sites (Discourse forums, older SPAs) ship the readable page inside `<noscript>` for
    // clients that don't run scripts. When the normal pass finds next to nothing, use it.
    if words(&ex.text) < 60 && raw.to_ascii_lowercase().contains("<noscript") {
        let unwrapped = NOSCRIPT_TAGS.replace_all(raw, "");
        let alt = extract_once(&unwrapped, page_url, only_main, exclude);
        if alt.text.len() > ex.text.len() + 400 {
            ex = alt;
        }
    }
    // The page's own summary (`<meta name="description">`, `og:description`) when the content
    // doesn't already say it: a social post's caption, a product's blurb. Only a real sentence or
    // more, never a few keywords.
    if only_main
        && let Some(d) = ex.description.as_deref()
        && words(d) >= 12
        && !collapse(&ex.text).contains(&collapse(d).chars().take(80).collect::<String>())
    {
        let d = collapse(d);
        ex.text = format!("{d} {}", ex.text);
        ex.markdown = if ex.markdown.is_empty() { d } else { format!("{d}\n\n{}", ex.markdown) };
    }
    // Still next to nothing: an app that draws its text from inline state (`ytInitialData`,
    // hydration blobs). Read the prose out of that.
    if only_main
        && words(&ex.text) < 80
        && let Some(extra) = crate::structured::state_text(raw, &ex.markdown)
    {
        ex.text.push(' ');
        ex.text.push_str(&collapse(&extra));
        if !ex.markdown.is_empty() {
            ex.markdown.push_str("\n\n");
        }
        ex.markdown.push_str(&extra);
    }
    ex
}

/// How much of the page to keep.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    /// `<main>`/`<article>` when there is one, minus chrome (class rules) and link farms.
    Main,
    /// The body minus semantic chrome only (`nav`, `footer`, `aside`, page header).
    Semantic,
    /// The whole body.
    Full,
}

fn extract_once(raw: &str, page_url: &str, only_main: bool, exclude: &[String]) -> Extracted {
    let base = RefCell::new(Url::parse(page_url).ok());
    let links: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let seen: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    // Row count of each open table, innermost last.
    let rows: std::rc::Rc<RefCell<Vec<usize>>> = std::rc::Rc::default();

    let resolve = |href: &str| -> Option<Url> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") || href.starts_with("data:") {
            return None;
        }
        let b = base.borrow();
        match b.as_ref() {
            Some(b) => b.join(href).ok(),
            None => Url::parse(href).ok(),
        }
    };

    let drop = ALWAYS_DROP;

    let mut settings = RewriteStrSettings::new()
        .append_element_content_handler(element!("base[href]", |el| {
            if let Some(h) = el.get_attribute("href")
                && let Some(u) = resolve(&h)
            {
                *base.borrow_mut() = Some(u);
            }
            Ok(())
        }))
        .append_element_content_handler(element!("a[href]", |el| {
            let Some(h) = el.get_attribute("href") else { return Ok(()) };
            match resolve(&h) {
                Some(mut u) => {
                    let abs = u.to_string();
                    el.set_attribute("href", &abs)?;
                    if matches!(u.scheme(), "http" | "https") {
                        u.set_fragment(None);
                        let key = u.to_string();
                        if seen.borrow_mut().insert(key.clone()) {
                            links.borrow_mut().push(key);
                        }
                    }
                }
                None => el.remove_attribute("href"),
            }
            Ok(())
        }))
        // A heading inside a table cell makes the Markdown converter drop the whole cell (pricing
        // grids put row labels in `<th><h4>`). Bold keeps the label and the table shape.
        .append_element_content_handler(element!(
            "td h1, td h2, td h3, td h4, td h5, td h6, th h1, th h2, th h3, th h4, th h5, th h6",
            |el| {
                el.set_tag_name("strong")?;
                Ok(())
            }
        ))
        // htmd only keeps a table when its header cells are `<th>` and every other cell is `<td>`:
        // a `<th scope="row">` label in a body row is dropped, and a table without `<th>` comes out as
        // loose paragraphs. Make the first row the header and every later cell a `<td>`. A stack of
        // row counters keeps nested tables apart.
        .append_element_content_handler(element!("table", |el| {
            rows.borrow_mut().push(0);
            let rows = rows.clone();
            if let Some(handlers) = el.end_tag_handlers() {
                let pop: lol_html::EndTagHandler<'static> = Box::new(move |_end| {
                    rows.borrow_mut().pop();
                    Ok(())
                });
                handlers.push(pop);
            }
            Ok(())
        }))
        .append_element_content_handler(element!("tr", |_el| {
            if let Some(top) = rows.borrow_mut().last_mut() {
                *top += 1;
            }
            Ok(())
        }))
        .append_element_content_handler(element!("td, th", |el| {
            let first_row = rows.borrow().last().is_some_and(|&n| n <= 1);
            match (el.tag_name().as_str(), first_row) {
                ("td", true) => el.set_tag_name("th")?,
                ("th", false) => el.set_tag_name("td")?,
                _ => {}
            }
            Ok(())
        }))
        .append_element_content_handler(element!("img", |el| {
            let src = ["data-src", "data-lazy-src", "data-original", "src"]
                .iter()
                .find_map(|a| el.get_attribute(a).filter(|v| !v.trim().is_empty() && !v.starts_with("data:")));
            match src.and_then(|s| resolve(&s)) {
                Some(u) => el.set_attribute("src", u.as_str())?,
                None => el.remove(),
            }
            el.remove_attribute("srcset");
            Ok(())
        }));
    for sel in drop {
        settings = settings.append_element_content_handler(element!(sel, |el| {
            el.remove();
            Ok(())
        }));
    }

    let cleaned = rewrite_str(raw, settings)
        // A selector lol_html can't parse or a broken document: fall back to the raw page.
        .unwrap_or_else(|_| raw.to_string());

    let mut doc = Html::parse_document(&cleaned);
    let meta = |sel: &Selector, attr: &str| {
        doc.select(sel).find_map(|e| e.value().attr(attr)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let title = meta(&S.og_title, "content").or_else(|| {
        doc.select(&S.title).next().map(|t| collapse(&t.text().collect::<String>())).filter(|s| !s.is_empty())
    });
    let description = meta(&S.description, "content").or_else(|| meta(&S.og_description, "content"));
    let language = meta(&S.html, "lang");
    let canonical = meta(&S.canonical, "href").and_then(|h| resolve(&h)).map(|u| u.to_string());
    let og_image = meta(&S.og_image, "content").and_then(|h| resolve(&h)).map(|u| u.to_string());

    // User selectors that don't parse are skipped rather than failing the page.
    let user: Vec<Selector> = exclude.iter().filter_map(|q| Selector::parse(q).ok()).collect();
    let (total, total_linked) = doc.select(&S.body).next().map(|b| (text_len(b), linked_len(b))).unwrap_or((0, 0));
    let mut level = if only_main { Level::Main } else { Level::Full };
    let (html, text) = loop {
        let root_id = main_element(&doc, level == Level::Main).map(|e| e.id());
        if let Some(id) = root_id {
            strip(&mut doc, id, &user, false);
            match level {
                Level::Main => {
                    strip(&mut doc, id, std::slice::from_ref(&S.chrome), true);
                    prune_link_farms(&mut doc, id);
                }
                Level::Semantic => strip(&mut doc, id, std::slice::from_ref(&S.semantic_junk), true),
                Level::Full => {}
            }
            drop_empty(&mut doc, id);
        }
        let root = root_id.and_then(|id| doc.tree.get(id)).and_then(ElementRef::wrap);
        // Safety net: when little is left and what was cut reads like prose (not link lists),
        // the rules cut content. Try again keeping more.
        let (kept, kept_linked) = root.map(|e| (text_len(e), linked_len(e))).unwrap_or((0, 0));
        let (cut, cut_linked) = (total.saturating_sub(kept), total_linked.saturating_sub(kept_linked));
        let cut_content = kept < 1500 && kept * 4 < total && cut_linked * 10 < cut * 6;
        // Next to nothing left of a sizeable page (a listing whose content is links, say):
        // take the body minus semantic chrome instead.
        let gutted = level == Level::Main && kept < 800 && kept * 5 < total;
        if level != Level::Full && (cut_content || gutted) {
            level = if level == Level::Main { Level::Semantic } else { Level::Full };
            doc = Html::parse_document(&cleaned);
            continue;
        }
        let html = root.map(|e| e.inner_html()).unwrap_or_default();
        let text = root.map(|e| collapse(&e.text().collect::<Vec<_>>().join(" "))).unwrap_or_default();
        break (html, text);
    };
    let mut markdown = to_markdown(&html);
    let mut text = text;
    // What the site publishes as structured data but the body doesn't show (product and job
    // descriptions rendered by JavaScript, article bodies, paper abstracts).
    if let Some(extra) = crate::structured::extra(raw, &doc, &markdown) {
        text.push(' ');
        text.push_str(&collapse(&extra));
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        markdown.push_str(&extra);
    }
    Extracted {
        markdown,
        html,
        text,
        links: links.into_inner(),
        title,
        description,
        language,
        canonical,
        og_image,
    }
}

struct Sels {
    title: Selector,
    og_title: Selector,
    description: Selector,
    og_description: Selector,
    og_image: Selector,
    canonical: Selector,
    html: Selector,
    body: Selector,
    main: Selector,
    article: Selector,
    blocks: Selector,
    a: Selector,
    emptyable: Selector,

    img: Selector,
    img_alt: Selector,
    chrome: Selector,
    semantic_junk: Selector,
}

static S: LazyLock<Sels> = LazyLock::new(|| {
    let s = |q: &str| Selector::parse(q).unwrap();
    Sels {
        title: s("title"),
        og_title: s("meta[property=\"og:title\"]"),
        description: s("meta[name=\"description\"]"),
        og_description: s("meta[property=\"og:description\"]"),
        og_image: s("meta[property=\"og:image\"]"),
        canonical: s("link[rel=\"canonical\"]"),
        html: s("html"),
        body: s("body"),
        main: s("main, [role=\"main\"]"),
        article: s("article"),
        // Numbered lists are content (steps, references, a list of sources); never pruned.
        blocks: s("ul, table, div, section, details"),
        a: s("a"),
        // Layout tables (SEC filings are full of them) leave rows and tables with no text.
        emptyable: s("li, ul, ol, h1, h2, h3, h4, h5, h6, tr, table"),

        img: s("img"),
        img_alt: s("img[alt]:not([alt=\"\"])"),
        chrome: s(&CHROME.join(", ")),
        semantic_junk: s(&format!("nav, footer, aside, body > header, [role=\"navigation\"], [role=\"banner\"], [role=\"contentinfo\"], [hidden], {}", JUNK.join(", "))),
    }
});

/// Text inside links under `e`.
fn linked_len(e: ElementRef) -> usize {
    e.select(&S.a).map(text_len).sum()
}

fn text_len(e: ElementRef) -> usize {
    e.text().map(|t| t.trim().len()).sum()
}

/// `<main>` if there's one with real text, else a lone `<article>`, else `<body>`.
fn main_element(doc: &Html, only_main: bool) -> Option<ElementRef<'_>> {
    let body = doc.select(&S.body).next()?;
    if !only_main {
        return Some(body);
    }
    // A `<main>` that holds only part of the page (a hero, a widget area) would throw the
    // rest away: take it only when it has at least half the page's text.
    // Link text (menus) doesn't count on either side.
    let prose = |e: ElementRef| text_len(e).saturating_sub(linked_len(e));
    let total = prose(body).max(1);
    let substantial = |e: &ElementRef| prose(*e) * 2 >= total;
    let mains: Vec<_> = doc.select(&S.main).collect();
    if mains.len() == 1 && substantial(&mains[0]) {
        return Some(mains[0]);
    }
    let articles: Vec<_> = doc.select(&S.article).collect();
    if articles.len() == 1 && substantial(&articles[0]) {
        return Some(articles[0]);
    }
    Some(body)
}

/// Detaches elements under `root` that match `sels`. With `guard`, an element holding half the
/// text or more is kept: a loose rule like `[class*="sidebar"]` can hit a page-wide wrapper such
/// as `<div class="layout--with-sidebar">`, and no real sidebar holds most of a page.
fn strip(doc: &mut Html, root: ego_tree::NodeId, sels: &[Selector], guard: bool) {
    let Some(r) = doc.tree.get(root).and_then(ElementRef::wrap) else { return };
    let total = text_len(r);
    let mut cut: Vec<ego_tree::NodeId> = Vec::new();
    for sel in sels {
        for e in r.select(sel) {
            if cut.contains(&e.id()) || e.ancestors().any(|a| cut.contains(&a.id())) {
                continue;
            }
            // A big block that is mostly links is a menu however big it is; drop it anyway.
            let n = text_len(e);
            let mostly_links = linked_len(e) * 10 >= n * 6;
            if guard && ((n * 2 >= total.max(1) && !mostly_links) || e.select(&S.main).next().is_some()) {
                continue;
            }
            cut.push(e.id());
        }
    }
    for id in cut {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// Removes list items, lists and headings with no text and no image: lazy-loaded "Most viewed"
/// rails come out as `1. 2. 3.`, and icon-only headings as a bare `####`.
fn drop_empty(doc: &mut Html, root: ego_tree::NodeId) {
    let Some(r) = doc.tree.get(root).and_then(ElementRef::wrap) else { return };
    let cut: Vec<ego_tree::NodeId> =
        r.select(&S.emptyable).filter(|e| text_len(*e) == 0 && e.select(&S.img).next().is_none()).map(|e| e.id()).collect();
    for id in cut {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// A real picture (a photo, a product shot), not a menu icon or a logo.
fn is_picture(img: ElementRef) -> bool {
    let v = img.value();
    let src = v.attr("src").unwrap_or("").to_ascii_lowercase();
    if ["icon", "logo", "sprite", "chevron", "arrow", "flag", "avatar"].iter().any(|k| src.contains(k)) {
        return false;
    }
    v.attr("width").and_then(|w| w.trim_end_matches("px").parse::<u32>().ok()).is_none_or(|w| w >= 100)
}

/// Drops blocks that are almost all links (language pickers, navboxes, tag clouds, related-post
/// grids), unless that would leave next to nothing: then the links are the page.
fn prune_link_farms(doc: &mut Html, root: ego_tree::NodeId) {
    let Some(r) = doc.tree.get(root).and_then(ElementRef::wrap) else { return };
    let total = text_len(r);
    let mut cut: Vec<ego_tree::NodeId> = Vec::new();
    let mut removed = 0;
    for e in r.select(&S.blocks) {
        if e.ancestors().any(|a| cut.contains(&a.id())) {
            continue;
        }
        let n = text_len(e);
        let anchors: Vec<_> = e.select(&S.a).collect();
        // A short list of links is usually content ("further reading"); menus run long.
        if n == 0 || anchors.len() < 15 {
            continue;
        }
        // A grid of linked pictures (a gallery, a product listing) is content: its words are in
        // the images' alt text, which text length doesn't count.
        // Only links that are just a picture: a card with a headline ("Related articles") is
        // a link list with thumbnails, and stays prunable.
        let pictured = anchors.iter().filter(|a| text_len(**a) < 15 && a.select(&S.img_alt).any(is_picture)).count();
        if pictured >= 5 && pictured * 3 >= anchors.len() {
            continue;
        }
        let linked: usize = anchors.iter().map(|a| text_len(*a)).sum();
        if linked * 100 >= n * 80 {
            cut.push(e.id());
            removed += n;
        }
    }
    // When link blocks are most of the page, the page is a listing and the links are its content.
    if removed * 2 > total || total.saturating_sub(removed) < 200 {
        return;
    }
    for id in cut {
        if let Some(mut node) = doc.tree.get_mut(id) {
            node.detach();
        }
    }
}

/// HTML to tidy Markdown, for other modules.
pub(crate) fn html_to_markdown(html: &str) -> String {
    to_markdown(html)
}

fn to_markdown(html: &str) -> String {
    thread_local! {
        static CONV: htmd::HtmlToMarkdown = htmd::HtmlToMarkdown::builder()
            .skip_tags(vec!["head", "script", "style", "noscript"])
            .build();
    }
    let md = CONV.with(|c| c.convert(html)).unwrap_or_default();
    tidy(&md)
}

static BLANKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n[ \t]*\n(?:[ \t]*\n)+").unwrap());

fn tidy(md: &str) -> String {
    let trimmed: String = md.lines().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n");
    BLANKS.replace_all(&trimmed, "\n\n").trim().to_string()
}

/// Word count that works for scripts written without spaces: Chinese, Japanese and Korean
/// characters count as half a word each, on top of the whitespace-separated words.
pub fn words(text: &str) -> usize {
    let spaced = text.split_whitespace().count();
    let cjk = text
        .chars()
        .filter(|c| matches!(*c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff))
        .count();
    spaced + cjk / 2
}

pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does this page probably need JavaScript to show its content?
/// Does this page probably need a browser to show its content?
///
/// Only when the plain fetch clearly came back empty: an app shell (`#root`, `#__next`…) with
/// almost no text, a challenge screen, or a near-empty page that runs scripts. Short pages with
/// real text stay on the fast path; the browser costs ~30× the time and on a 1,000-page benchmark
/// rescued only half the pages it was sent.
pub fn looks_client_rendered(raw: &str, ex: &Extracted) -> bool {
    // Hardly any text in a heavy page: the content is in script data and needs a browser.
    if ex.text.len() < 600 && raw.len() > 150_000 {
        return true;
    }
    let words = words(&ex.text);
    if words >= 80 {
        return false;
    }
    let lower = raw.to_ascii_lowercase();
    let shell = ["id=\"root\"", "id=\"app\"", "id=\"__next\"", "id=\"___gatsby\"", "ng-version", "data-reactroot", "id=\"__nuxt\""]
        .iter()
        .any(|m| lower.contains(m));
    is_challenge(&lower) || (shell && words < 50) || (words < 20 && lower.contains("<script"))
}

/// Is this short page a failure dressed as a page: a block screen, an error page, a parked site?
/// Returns what it is. Only looked for on short pages, so an article that mentions "404" or
/// "access denied" is never caught.
pub fn failure_page(text: &str) -> Option<&'static str> {
    let words = words(text);
    if words < 10 {
        return Some("empty page");
    }
    if words > 150 {
        return None;
    }
    let t = text.to_lowercase();
    const BLOCKED: &[&str] = &[
        "performing security verification",
        "you have been blocked",
        "confirm you are human",
        "confirm you are a human",
        "verify you are human",
        "verify you are a human",
        "checking your browser",
        "request blocked",
        "access denied",
        "access restricted",
        "unusual activity",
        "unusual traffic",
        "are you a robot",
        "automated process",
        "press & hold",
        "security check",
        "don’t have access to this page",
        "don't have access to this page",
        "403 forbidden",
        // F5 / Akamai / WAF denial screens.
        "requested url was rejected",
        "your support id is",
        "incident number",
        "please return to the previous page to try again",
    ];
    const ERROR: &[&str] = &[
        "404 not found",
        "page not found",
        "page could not be found",
        "page you are looking for doesn",
        "page you requested could not",
        "nosuchkey",
        "temporarily unavailable",
        "website expired",
        "account has expired",
        "is unavailable",
        "something went wrong",
        "something has gone wrong",
        "server encountered an error",
        "taking longer than usual",
        "account temporary on hold",
    ];
    if BLOCKED.iter().any(|m| t.contains(m)) {
        return Some("blocked by the site");
    }
    if ERROR.iter().any(|m| t.contains(m)) || (words < 30 && (t.contains("404") || t.contains("not found") || t.contains("forbidden"))) {
        return Some("error page");
    }
    None
}

/// Block pages a real browser on the same machine gets too: captcha walls (DataDome,
/// PerimeterX), Incapsula, Cloudflare's "you have been blocked" error page, Akamai's deny page.
/// Over five 1,000-page benchmark runs a render never got past one of these; it did get past
/// Cloudflare's JavaScript challenge ("Just a moment…"), which is why that isn't here.
pub fn is_hard_block(lower: &str) -> bool {
    const MARKS: &[&str] = &[
        "captcha-delivery.com",
        "px-captcha",
        "_incapsula_resource",
        "request unsuccessful. incapsula",
        "cf-error-details",
        "you don't have permission to access",
        "you don&#39;t have permission to access",
    ];
    MARKS.iter().any(|m| lower.contains(m)) && !lower.contains("cf-chl-")
}

/// Bot-wall and challenge pages from the common CDNs and anti-bot vendors.
pub fn is_challenge(lower: &str) -> bool {
    const MARKS: &[&str] = &[
        "just a moment...",
        "cf-chl-",
        "challenge-platform",
        "enable js and disable any ad blocker",
        "captcha-delivery.com",
        "verify you are human",
        "are you a robot",
        "px-captcha",
        "access denied</h1>",
        "request unsuccessful. incapsula",
        "_incapsula_resource",
    ];
    MARKS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_and_converts() {
        let html = r#"<html lang="en"><head><title>Hi there</title><base href="https://x.dev/docs/">
            <meta name="description" content="A page"></head><body>
            <nav><a href="/home">Home</a></nav>
            <main><h1>Title</h1><p>Read <a href="guide#top">the guide</a>.</p>
            <img data-src="pic.png" src="data:image/gif;base64,AAA" alt="pic"><script>alert(1)</script></main>
            <footer>© corp</footer></body></html>"#;
        let ex = extract(html, "https://x.dev/start", true, &[]);
        assert_eq!(ex.title.as_deref(), Some("Hi there"));
        assert_eq!(ex.description.as_deref(), Some("A page"));
        assert_eq!(ex.language.as_deref(), Some("en"));
        assert!(ex.markdown.starts_with("# Title"), "{}", ex.markdown);
        assert!(ex.markdown.contains("[the guide](https://x.dev/docs/guide#top)"), "{}", ex.markdown);
        assert!(ex.markdown.contains("https://x.dev/docs/pic.png"), "{}", ex.markdown);
        assert!(!ex.markdown.contains("Home") && !ex.markdown.contains("corp") && !ex.markdown.contains("alert"));
        assert!(ex.links.contains(&"https://x.dev/docs/guide".to_string()));
        assert!(ex.links.contains(&"https://x.dev/home".to_string()), "links come from the whole page");
    }

    #[test]
    fn prunes_link_farms_but_keeps_link_pages() {
        let langs: String = (0..30).map(|i| format!("<li><a href=\"/l{i}\">Lang {i}</a></li>")).collect();
        let prose = "Real words about the topic at hand. ".repeat(20);
        let html = format!("<body><main><h1>T</h1><ul>{langs}</ul><p>{prose}</p></main></body>");
        let ex = extract(&html, "https://x.dev/", true, &[]);
        assert!(!ex.markdown.contains("Lang 3") && ex.markdown.contains("Real words"), "{}", ex.markdown);
        let index = format!("<body><main><h1>Docs</h1><ul>{langs}</ul></main></body>");
        assert!(extract(&index, "https://x.dev/", true, &[]).markdown.contains("Lang 3"));
    }

    #[test]
    fn never_drops_the_body() {
        let html = "<html><body class=\"sidebar-visible\"><div class=\"sidebar\">menu</div><main><p>Kept text here.</p></main></body></html>";
        let ex = extract(html, "https://x.dev/", true, &[]);
        assert_eq!(ex.markdown, "Kept text here.");
    }

    #[test]
    fn keeps_wrappers_with_loose_class_names() {
        let prose = "Plenty of article text sits in here. ".repeat(10);
        let html = format!("<body><div class=\"layout--with-sidebar\"><div class=\"sidebar\">Menu</div><p>{prose}</p></div></body>");
        let ex = extract(&html, "https://x.dev/", true, &[]);
        assert!(ex.markdown.contains("Plenty of") && !ex.markdown.contains("Menu"), "{}", ex.markdown);
    }

    #[test]
    fn keeps_short_link_lists_and_numbered_lists() {
        let prose = "Body text that matters to the reader. ".repeat(20);
        let ol: String = (0..20).map(|i| format!("<li><a href=\"/d{i}\">Doc {i}</a></li>")).collect();
        let ul: String = (0..10).map(|i| format!("<li><a href=\"/r{i}\">Read {i}</a></li>")).collect();
        let html = format!("<body><main><p>{prose}</p><ol>{ol}</ol><ul>{ul}</ul></main></body>");
        let md = extract(&html, "https://x.dev/", true, &[]).markdown;
        assert!(md.contains("Doc 19") && md.contains("Read 9"), "{md}");
    }

    #[test]
    fn table_row_headers_keep_their_labels() {
        let prose = "Plans compared in full detail below for every team. ".repeat(8);
        let html = format!(r#"<body><main><p>{prose}</p><table><tr><th></th><th>Pro</th><th>Team</th></tr>
            <tr role="row"><th scope="row" role="rowheader"><h4 class="rowTitle"><span>Invoicing / net terms</span>
            <span class="tooltipWrap"><button type="button"><svg><path d="M1"/></svg></button></span></h4></th>
            <td><span class="sr-only">No</span></td><td><span class="sr-only">Yes</span></td></tr></table></main></body>"#);
        let md = extract(&html, "https://x.dev/", true, &[]).markdown;
        assert!(md.contains("Invoicing / net terms"), "{md}");
    }

    #[test]
    fn tables_without_th_still_become_tables() {
        let html = "<body><table><tr><td>Plan</td><td>Price</td></tr><tr><td>Pro</td><td>$20</td></tr></table></body>";
        let md = extract(html, "https://x.dev/", false, &[]).markdown;
        assert!(md.contains("| Plan | Price |") && md.contains("| Pro  | $20   |"), "{md}");
    }

    #[test]
    fn drops_empty_lists_and_headings() {
        let html = "<body><main><p>Story text.</p><h2>Most viewed</h2><ol><li></li><li> </li></ol><h4><svg></svg></h4></main></body>";
        assert_eq!(extract(html, "https://x.dev/", true, &[]).markdown, "Story text.\n\n## Most viewed");
    }

    #[test]
    fn reads_noscript_when_the_page_is_otherwise_empty() {
        let prose = "The forum post text lives in here for crawlers. ".repeat(12);
        let html = format!("<body><div id=\"app\"></div><noscript><div><p>{prose}</p></div></noscript><script src=\"a.js\"></script></body>");
        let ex = extract(&html, "https://x.dev/t/1", true, &[]);
        assert!(ex.markdown.contains("forum post text"), "{}", ex.markdown);
    }

    #[test]
    fn falls_back_when_main_holds_little_and_the_rest_is_prose() {
        let prose = "Documentation body that a class rule would wrongly cut away. ".repeat(30);
        let html = format!("<body><main><p>Short stub.</p></main><div class=\"content-sidebar-wrap\"><p>{prose}</p></div></body>");
        let ex = extract(&html, "https://x.dev/", true, &[]);
        assert!(ex.markdown.contains("Documentation body"), "{}", ex.markdown);
        // A short page with a big link menu is not a reason to fall back.
        let menu: String = (0..200).map(|i| format!("<a href=\"/c{i}\">Category number {i}</a> ")).collect();
        let html = format!("<body><nav>{menu}</nav><main><p>Short but real page.</p></main></body>");
        assert_eq!(extract(&html, "https://x.dev/", true, &[]).markdown, "Short but real page.");
    }

    #[test]
    fn spots_failure_pages_but_not_articles() {
        assert_eq!(failure_page("Not Found"), Some("empty page"));
        assert_eq!(failure_page("当社は、ITソリューションを中心にクラウド課題に対応したIT支援やSaaSサービスを提供しています。"), None);
        assert_eq!(failure_page("www.dnb.com Performing security verification This website uses a security service to protect against malicious bots"), Some("blocked by the site"));
        assert_eq!(failure_page("404 Page not found The page you are looking for doesn’t exist. Here are some helpful links"), Some("error page"));
        let article = "HTTP status codes explained. ".repeat(60) + "A 404 not found error means access denied is unrelated.";
        assert_eq!(failure_page(&article), None);
        assert_eq!(failure_page("This domain is for use in documentation examples without needing permission. Avoid use in operations."), None);
    }

    #[test]
    fn spots_spa_shells() {
        let html = r#"<html><body><div id="root"></div><script src="a.js"></script></body></html>"#;
        let ex = extract(html, "https://x.dev/", true, &[]);
        assert!(looks_client_rendered(html, &ex));
    }

    #[test]
    fn keeps_picture_galleries_but_prunes_icon_menus() {
        let card = |i: usize, img: &str| format!(r#"<a href="/p/{i}"><img src="https://x.dev/{img}{i}.jpg" width="400" alt="Mountain lake at dawn number {i}"></a>"#);
        let gallery: String = (0..20).map(|i| card(i, "photo")).collect();
        // A mega-menu: an icon and a label per link.
        let menu: String = (0..20).map(|i| format!(r#"<a href="/m/{i}"><img src="https://x.dev/nav-icon-{i}.png" width="40" alt="Product line {i}">Product line {i} overview</a>"#)).collect();
        let prose = "<p>".to_string() + &"Words about the collection and the artist behind it. ".repeat(12) + "</p>";
        let html = format!("<html><body><main>{prose}<div class=grid>{gallery}</div><div class=mega>{menu}</div></main></body></html>");
        let ex = extract(&html, "https://x.dev/", true, &[]);
        assert!(ex.markdown.contains("Mountain lake at dawn number 7"), "gallery kept");
        assert!(!ex.markdown.contains("Product line 7 overview"), "icon menu pruned");
    }

    #[test]
    fn consent_banners_go_even_when_falling_back() {
        let html = r#"<html><body><div class="cmplz-cookiebanner" role="region"><p>Manage Consent. To provide the best experiences, we use technologies like cookies to store and access device information on your device.</p></div><div><p>Host your next event at the Delta Grand Theater in Opelousas. The theater seats four hundred guests, with a stage, dressing rooms and a lobby for receptions. Check availability, then see the rental agreement below for rates, deposits and the rules for decorations and catering.</p></div></body></html>"#;
        let ex = extract(html, "https://x.dev/", true, &[]);
        assert!(ex.markdown.contains("Delta Grand"));
        assert!(!ex.markdown.contains("Manage Consent"));
    }

    #[test]
    fn firewall_denials_are_failures() {
        assert_eq!(failure_page("L.L.Bean Please use the link above to continue. Please return to the previous page to try again or contact Email Text"), Some("blocked by the site"));
        assert_eq!(failure_page("The requested URL was rejected. Please consult with your administrator. Your support ID is: 1234567890"), Some("blocked by the site"));
    }

    #[test]
    fn hard_blocks_skip_the_browser_but_js_challenges_dont() {
        assert!(is_hard_block("<h1>access denied</h1> you don't have permission to access \"http://x/\" on this server. reference #18.1"));
        assert!(is_hard_block("<script src=\"https://ct.captcha-delivery.com/c.js\"></script>"));
        assert!(is_hard_block("<div id=\"cf-error-details\">sorry, you have been blocked</div>"));
        assert!(!is_hard_block("<title>just a moment...</title><script src=\"/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1?ray=1\"></script> cf-chl-opt"));
        assert!(!is_hard_block("<p>403 forbidden</p>"));
    }
}
