use anyhow::{Result, bail};
use url::Url;

/// Parses a URL a person typed: adds `https://` when the scheme is missing.
pub fn parse_http(s: &str) -> Result<Url> {
    let s = s.trim();
    let u = match Url::parse(s) {
        Ok(u) => u,
        Err(url::ParseError::RelativeUrlWithoutBase) => Url::parse(&format!("https://{s}"))?,
        Err(e) => return Err(e.into()),
    };
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() {
        bail!("not an http(s) URL: {s}");
    }
    Ok(u)
}

/// The key two URLs share when they're the same page: no fragment, no trailing slash, no
/// tracking parameters.
pub fn normalize(u: &Url) -> String {
    let mut u = u.clone();
    u.set_fragment(None);
    if u.query().is_some() {
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !k.starts_with("utm_") && k != "fbclid" && k != "gclid" && k != "ref")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            u.set_query(None);
        } else {
            u.query_pairs_mut().clear().extend_pairs(kept);
        }
    }
    let mut s = u.to_string();
    if s.ends_with('/') && u.path() != "/" {
        s.pop();
    }
    s
}

const SKIP_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "ico", "bmp", "tif", "tiff", "css", "js", "mjs", "map",
    "json", "woff", "woff2", "ttf", "otf", "eot", "zip", "gz", "tgz", "rar", "7z", "tar", "exe", "dmg", "msi",
    "apk", "deb", "rpm", "iso", "mp3", "wav", "ogg", "flac", "mp4", "webm", "mov", "avi", "mkv", "pdf", "doc",
    "docx", "xls", "xlsx", "ppt", "pptx", "rss", "atom",
];

/// Links that are files, not pages.
pub fn is_asset(u: &Url) -> bool {
    let last = u.path().rsplit('/').next().unwrap_or("");
    match last.rsplit_once('.') {
        Some((_, ext)) => SKIP_EXT.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// `www.example.com` and `example.com` count as the same site.
pub fn site(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

pub fn same_site(a: &str, b: &str, subdomains: bool) -> bool {
    let (a, b) = (site(a), site(b));
    a == b || (subdomains && (b.ends_with(&format!(".{a}")) || a.ends_with(&format!(".{b}"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        let u = Url::parse("https://Example.com/a/?utm_source=x&id=2#top").unwrap();
        assert_eq!(normalize(&u), "https://example.com/a/?id=2".trim_end_matches('/'));
        assert_eq!(normalize(&Url::parse("https://x.dev/").unwrap()), "https://x.dev/");
        assert_eq!(parse_http("x.dev/docs").unwrap().as_str(), "https://x.dev/docs");
        assert!(is_asset(&Url::parse("https://x.dev/a/logo.PNG").unwrap()));
        assert!(same_site("www.x.dev", "x.dev", false));
        assert!(same_site("x.dev", "docs.x.dev", true) && !same_site("x.dev", "docs.x.dev", false));
    }
}
