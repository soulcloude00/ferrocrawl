//! Content that sites publish for machines but don't show in the HTML body: schema.org JSON-LD
//! (product details, job postings, article bodies) and the citation meta tags journals write for
//! Google Scholar. Shops and job boards often render their descriptions with JavaScript but put
//! the same text in JSON-LD, so reading it gets the content without a browser.
//!
//! Only what the page's Markdown doesn't already contain is added, so nothing is duplicated.

use std::sync::LazyLock;

use regex::Regex;
use scraper::{Html, Selector};
use serde_json::Value;

static LD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<script[^>]*type\s*=\s*["']?application/ld\+json["']?[^>]*>(.*?)</script>"#).unwrap());

/// Types whose fields are the page's content (not site furniture like WebSite or Organization).
const CONTENT_TYPES: &[&str] = &[
    "Product",
    "ProductGroup",
    "JobPosting",
    "Article",
    "NewsArticle",
    "BlogPosting",
    "ScholarlyArticle",
    "TechArticle",
    "Report",
    "LegalDocument",
    "Legislation",
    "Recipe",
    "Event",
    "Course",
    "Book",
    "SoftwareApplication",
    "Dataset",
    "VideoObject",
    "Question",
];

/// Extra Markdown for what the page publishes as structured data but `markdown` lacks.
pub fn extra(raw: &str, doc: &Html, markdown: &str) -> Option<String> {
    let have = norm(markdown);
    let is_new = |s: &str| {
        let n = norm(s);
        // Compare on the first 160 characters (not bytes: the text can be in any script).
        let head: String = n.chars().take(160).collect();
        head.chars().count() >= 40 && !have.contains(&head)
    };
    let mut out: Vec<String> = Vec::new();
    for item in ld_items(raw) {
        if let Some(block) = describe(&item, &is_new) {
            out.push(block);
        }
    }
    if let Some(block) = citation(doc, &is_new) {
        out.push(block);
    }
    (!out.is_empty()).then(|| out.join("\n\n"))
}

fn ld_items(raw: &str) -> Vec<Value> {
    let mut items = Vec::new();
    for m in LD.captures_iter(raw).take(20) {
        let Ok(v) = serde_json::from_str::<Value>(m[1].trim()) else { continue };
        flatten(v, &mut items, 0);
    }
    items
}

fn flatten(v: Value, out: &mut Vec<Value>, depth: usize) {
    if depth > 3 {
        return;
    }
    match v {
        Value::Array(a) => a.into_iter().for_each(|x| flatten(x, out, depth + 1)),
        Value::Object(mut o) => {
            if let Some(g) = o.remove("@graph") {
                flatten(g, out, depth + 1);
            }
            if is_content(&Value::Object(o.clone())) {
                out.push(Value::Object(o));
            }
        }
        _ => {}
    }
}

fn types(v: &Value) -> Vec<String> {
    match v.get("@type") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn is_content(v: &Value) -> bool {
    types(v).iter().any(|t| CONTENT_TYPES.contains(&t.as_str()))
}

fn s(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(x) => Some(x.trim().to_string()).filter(|x| !x.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(o) => o.get("name").and_then(|n| n.as_str()).map(str::to_string),
        Value::Array(a) => a.first().and_then(|x| match x {
            Value::String(x) => Some(x.clone()),
            Value::Object(o) => o.get("name").and_then(|n| n.as_str()).map(str::to_string),
            _ => None,
        }),
        _ => None,
    }
}

/// Text that may be HTML or entity-encoded HTML, as plain Markdown.
fn as_markdown(text: &str) -> String {
    let decoded = if text.contains("&lt;") || text.contains("&amp;") || text.contains("&#") {
        Html::parse_fragment(text).root_element().text().collect::<String>()
    } else {
        text.to_string()
    };
    if decoded.contains('<') && decoded.contains('>') { crate::extract::html_to_markdown(&decoded) } else { decoded.trim().to_string() }
}

fn describe(item: &Value, is_new: &impl Fn(&str) -> bool) -> Option<String> {
    let kind = types(item).into_iter().next().unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut body: Vec<String> = Vec::new();

    // The long text fields are what make a page substantive; add them when they're missing.
    for key in ["description", "articleBody", "text", "abstract", "responsibilities", "qualifications"] {
        if let Some(t) = s(item, key).map(|t| as_markdown(&t))
            && is_new(&t)
        {
            body.push(t);
        }
    }
    // A product group's description sometimes lives on its variants.
    if body.is_empty()
        && let Some(Value::Array(vars)) = item.get("hasVariant")
        && let Some(t) = vars.iter().find_map(|v| s(v, "description")).map(|t| as_markdown(&t))
        && is_new(&t)
    {
        body.push(t);
    }
    if body.is_empty() {
        return None;
    }

    let name = s(item, "name").or_else(|| s(item, "headline")).or_else(|| s(item, "title"));
    match kind.as_str() {
        "Product" | "ProductGroup" => {
            if let Some(b) = s(item, "brand") {
                lines.push(format!("**Brand:** {b}"));
            }
            let offers = item.get("offers").map(|o| if let Value::Array(a) = o { a.first().cloned().unwrap_or(Value::Null) } else { o.clone() });
            if let Some(o) = offers {
                let price = s(&o, "price").or_else(|| s(&o, "lowPrice"));
                if let Some(p) = price {
                    lines.push(format!("**Price:** {p} {}", s(&o, "priceCurrency").unwrap_or_default()).trim_end().to_string());
                }
                if let Some(a) = s(&o, "availability") {
                    lines.push(format!("**Availability:** {}", a.rsplit('/').next().unwrap_or(&a)));
                }
            }
            if let Some(sku) = s(item, "sku") {
                lines.push(format!("**SKU:** {sku}"));
            }
        }
        "JobPosting" => {
            if let Some(o) = s(item, "hiringOrganization") {
                lines.push(format!("**Company:** {o}"));
            }
            if let Some(loc) = item.get("jobLocation").and_then(location) {
                lines.push(format!("**Location:** {loc}"));
            }
            if let Some(t) = s(item, "employmentType") {
                lines.push(format!("**Employment type:** {t}"));
            }
            if let Some(d) = s(item, "datePosted") {
                lines.push(format!("**Posted:** {d}"));
            }
        }
        _ => {
            if let Some(a) = s(item, "author") {
                lines.push(format!("**Author:** {a}"));
            }
            if let Some(d) = s(item, "datePublished") {
                lines.push(format!("**Published:** {d}"));
            }
        }
    }
    let mut md = String::new();
    if let Some(n) = name {
        md.push_str(&format!("## {n}\n\n"));
    }
    if !lines.is_empty() {
        md.push_str(&lines.join("  \n"));
        md.push_str("\n\n");
    }
    md.push_str(&body.join("\n\n"));
    Some(md.trim().to_string())
}

fn location(v: &Value) -> Option<String> {
    let first = if let Value::Array(a) = v { a.first()? } else { v };
    let addr = first.get("address").unwrap_or(first);
    let parts: Vec<String> =
        ["addressLocality", "addressRegion", "addressCountry"].iter().filter_map(|k| s(addr, k)).collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

struct Cite {
    abstract_: Selector,
    title: Selector,
    authors: Selector,
    journal: Selector,
    date: Selector,
    doi: Selector,
}

static CITE: LazyLock<Cite> = LazyLock::new(|| {
    let s = |q: &str| Selector::parse(q).unwrap();
    Cite {
        abstract_: s(r#"meta[name="citation_abstract"], meta[name="dc.description"], meta[name="DC.description"], meta[name="dcterms.abstract"]"#),
        title: s(r#"meta[name="citation_title"]"#),
        authors: s(r#"meta[name="citation_author"]"#),
        journal: s(r#"meta[name="citation_journal_title"]"#),
        date: s(r#"meta[name="citation_publication_date"], meta[name="citation_date"]"#),
        doi: s(r#"meta[name="citation_doi"]"#),
    }
});

/// A paper's abstract and record from its citation meta tags, when the body lacks them.
fn citation(doc: &Html, is_new: &impl Fn(&str) -> bool) -> Option<String> {
    let get = |sel: &Selector| doc.select(sel).find_map(|e| e.value().attr("content")).map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
    let abstract_ = get(&CITE.abstract_).map(|a| as_markdown(&a)).filter(|a| is_new(a))?;
    let mut md = String::new();
    if let Some(t) = get(&CITE.title) {
        md.push_str(&format!("## {t}\n\n"));
    }
    let authors: Vec<String> = doc.select(&CITE.authors).filter_map(|e| e.value().attr("content")).map(str::to_string).take(12).collect();
    let mut lines = Vec::new();
    if !authors.is_empty() {
        lines.push(format!("**Authors:** {}", authors.join("; ")));
    }
    if let Some(j) = get(&CITE.journal) {
        lines.push(format!("**Journal:** {j}"));
    }
    if let Some(d) = get(&CITE.date) {
        lines.push(format!("**Published:** {d}"));
    }
    if let Some(d) = get(&CITE.doi) {
        lines.push(format!("**DOI:** {d}"));
    }
    if !lines.is_empty() {
        md.push_str(&lines.join("  \n"));
        md.push_str("\n\n");
    }
    md.push_str("### Abstract\n\n");
    md.push_str(&abstract_);
    Some(md)
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_a_product_description_the_body_lacks() {
        let raw = r#"<html><head><script type="application/ld+json">{"@context":"https://schema.org","@type":"ProductGroup","name":"Juniper Tee","brand":{"@type":"Brand","name":"tentree"},"description":"&lt;p&gt;A soft organic cotton tee made for everyday wear, with ten trees planted for every purchase.&lt;/p&gt;","offers":{"@type":"Offer","price":"48.00","priceCurrency":"USD","availability":"https://schema.org/InStock"}}</script></head><body><h1>Juniper Tee</h1><p>Reviews (12)</p></body></html>"#;
        let doc = Html::parse_document(raw);
        let md = extra(raw, &doc, "# Juniper Tee\n\nReviews (12)").unwrap();
        assert!(md.contains("A soft organic cotton tee") && md.contains("**Price:** 48.00 USD") && md.contains("tentree"), "{md}");
        assert!(md.contains("InStock") && !md.contains("<p>"), "{md}");
    }

    #[test]
    fn handles_multibyte_text() {
        let desc = "《投影仪行业大揭秘》网红和杂牌投影不会告诉你的二十个坑，二十四年再买投影必看避雷指南，附型号推荐与详细参数对比说明。".repeat(3);
        let raw = format!(r#"<script type="application/ld+json">{{"@type":"Article","headline":"投影","articleBody":"{desc}"}}</script>"#);
        let doc = Html::parse_document(&raw);
        assert!(extra(&raw, &doc, "").unwrap().contains("投影仪行业大揭秘"));
    }

    #[test]
    fn skips_what_the_page_already_shows() {
        let desc = "A soft organic cotton tee made for everyday wear, with ten trees planted for every purchase.";
        let raw = format!(r#"<script type="application/ld+json">{{"@type":"Product","name":"Tee","description":"{desc}"}}</script>"#);
        let doc = Html::parse_document(&raw);
        assert!(extra(&raw, &doc, &format!("# Tee\n\n{desc}")).is_none());
    }

    #[test]
    fn reads_job_postings_and_citation_abstracts() {
        let raw = r#"<script type="application/ld+json">{"@graph":[{"@type":"WebSite","name":"Jobs"},{"@type":"JobPosting","title":"Data Engineer","hiringOrganization":{"@type":"Organization","name":"Acme"},"jobLocation":{"@type":"Place","address":{"addressLocality":"Austin","addressRegion":"TX"}},"description":"<p>Build and run the pipelines that move our data between services every day.</p>"}]}</script>
            <meta name="citation_title" content="On Fast Scrapers"><meta name="citation_author" content="Lee, A."><meta name="citation_abstract" content="We measure how quickly web scrapers return clean text from fresh pages across nine categories.">"#;
        let doc = Html::parse_document(raw);
        let md = extra(raw, &doc, "").unwrap();
        assert!(md.contains("## Data Engineer") && md.contains("**Company:** Acme") && md.contains("Austin, TX"), "{md}");
        assert!(md.contains("Build and run the pipelines"), "{md}");
        assert!(md.contains("### Abstract") && md.contains("We measure how quickly") && md.contains("Lee, A."), "{md}");
    }
}
