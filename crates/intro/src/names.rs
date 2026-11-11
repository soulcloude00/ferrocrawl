//! Keys that let the same person or company, written different ways, land on one node.

use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// Lowercase ASCII-folded words: "José  García-López" → "jose garcia lopez".
fn fold(s: &str) -> String {
    let ascii: String = s.nfkd().filter(|c| c.is_ascii()).collect::<String>().to_lowercase();
    ascii.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

const HONORIFICS: &[&str] = &["dr", "mr", "mrs", "ms", "prof", "sir", "phd", "md", "mba", "jr", "sr", "ii", "iii", "cfa", "cpa"];

/// A person's key: folded, honorifics and middle initials dropped.
pub fn person_key(name: &str) -> String {
    let words: Vec<String> = fold(name).split(' ').filter(|w| !HONORIFICS.contains(w)).map(str::to_string).collect();
    let n = words.len();
    words
        .into_iter()
        .enumerate()
        // Keep first and last whatever their length; drop single-letter middles ("John F Kennedy").
        .filter(|(i, w)| *i == 0 || *i + 1 == n || w.len() > 1)
        .map(|(_, w)| w)
        .collect::<Vec<_>>()
        .join(" ")
}

const ORG_SUFFIXES: &[&str] = &[
    "inc", "incorporated", "llc", "ltd", "limited", "corp", "corporation", "co", "company", "gmbh", "ag", "sa", "plc", "pvt",
    "private", "lp", "llp", "bv", "srl", "oy", "ab", "the",
];

/// A company's key: "The Acme Corp, Inc." → "acme".
pub fn org_key(name: &str) -> String {
    let folded = fold(name);
    let mut words: Vec<&str> = folded.split(' ').filter(|w| !w.is_empty()).collect();
    while words.len() > 1 && words.last().is_some_and(|w| ORG_SUFFIXES.contains(w)) {
        words.pop();
    }
    while words.len() > 1 && words.first().is_some_and(|w| *w == "the") {
        words.remove(0);
    }
    words.join(" ")
}

static LINKEDIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)linkedin\.com/(in|company|school)/([^/?#\s]+)").unwrap());

/// `https://www.linkedin.com/in/Jane-Doe-123/?x=1` → `in/jane-doe-123`. Used only to match records,
/// never fetched.
pub fn linkedin_id(url: &str) -> Option<String> {
    let c = LINKEDIN.captures(url)?;
    let slug = c[2].trim_end_matches('/').to_lowercase();
    let slug = percent_decode(&slug);
    (!slug.is_empty()).then(|| format!("{}/{}", c[1].to_lowercase(), slug))
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_else(|| s.to_string())
}

/// Words that are capitalized on web pages but aren't parts of names.
const NOT_NAME_WORDS: &[&str] = &[
    "about", "all", "and", "angeles", "apply", "back", "blog", "board", "book", "careers", "case", "company", "companies", "contact",
    "cookie", "cookies", "demo", "docs", "events", "faq", "featured", "for", "francisco", "free", "get", "help", "home", "in",
    "inc", "investors", "join", "kingdom", "latest", "learn", "llc", "log", "ltd", "meet", "menu", "more", "new", "news", "next",
    "of", "our", "out", "partners", "policy", "portfolio", "press", "previous", "pricing", "privacy", "product", "products", "read",
    "resources", "san", "search", "see", "settings", "sign", "started", "states", "studies", "subscribe", "support", "team", "terms",
    "the", "to", "trial", "united", "up", "us", "view", "watch", "we", "with", "york", "your", "world", "fund", "capital",
    "ventures", "partner", "founders", "leadership", "people", "speakers", "agenda", "schedule", "tickets", "register", "download",
    "now", "here", "close", "open", "share", "follow", "email", "phone", "address", "office", "offices", "global", "group",
    "services", "solutions", "platform", "technology", "technologies", "labs", "ai", "api", "app", "apps", "series", "seed",
];

const PARTICLES: &[&str] = &["de", "da", "di", "del", "della", "van", "von", "der", "den", "la", "le", "bin", "al", "dos", "du", "ter"];

/// True for text shaped like a personal name: 2–4 capitalized words, maybe with particles or an
/// initial ("Jane Doe", "Ludwig van Beethoven", "Mary J. Blige").
pub fn looks_like_name(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 4 || s.len() > 48 || s.chars().any(|c| c.is_ascii_digit() || "@|:/&+()[]{}!?#$%*=<>\"".contains(c)) {
        return false;
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    if !(2..=5).contains(&words.len()) {
        return false;
    }
    let mut capitalized = 0;
    for (i, w) in words.iter().enumerate() {
        let lower = w.to_lowercase();
        let bare = lower.trim_matches(|c: char| c == '.' || c == ',');
        if NOT_NAME_WORDS.contains(&bare) {
            return false;
        }
        if i > 0 && i + 1 < words.len() && PARTICLES.contains(&bare) {
            continue;
        }
        let mut chars = w.chars();
        let Some(first) = chars.next() else { return false };
        if !first.is_uppercase() {
            return false;
        }
        let rest: String = chars.collect();
        // An initial ("J.") or a word whose remainder is mostly lowercase ("McDonald", "O'Neil").
        let initial = rest == "." || rest.is_empty();
        let ok_rest = rest.chars().all(|c| c.is_alphabetic() || "'’-.".contains(c)) && rest.chars().filter(|c| c.is_lowercase()).count() * 2 >= rest.chars().filter(|c| c.is_alphabetic()).count();
        if !(initial || ok_rest) {
            return false;
        }
        if !initial {
            capitalized += 1;
        }
    }
    capitalized >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(person_key("Dr. José  García-López"), "jose garcia lopez");
        assert_eq!(person_key("John F. Kennedy"), "john kennedy");
        assert_eq!(org_key("The Acme Corp, Inc."), "acme");
        assert_eq!(org_key("Sequoia Capital"), "sequoia capital");
        assert_eq!(linkedin_id("https://www.linkedin.com/in/Jane-Doe-123/?trk=x").as_deref(), Some("in/jane-doe-123"));
    }

    #[test]
    fn names() {
        for n in ["Jane Doe", "Ludwig van Beethoven", "Mary J. Blige", "Seán O'Neil", "Ana María López"] {
            assert!(looks_like_name(n), "{n}");
        }
        for n in ["Read More", "Contact Us", "Our Team", "San Francisco", "Series A", "jane doe", "IBM Watson", "Privacy Policy", "Acme"] {
            assert!(!looks_like_name(n), "{n}");
        }
    }
}
