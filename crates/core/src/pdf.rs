//! PDFs → Markdown, as fast as the machine allows.
//!
//! - The page count comes from the raw bytes (`/Count`), so no extra process runs for it.
//! - Big PDFs are split into page ranges converted on every core at once, then joined in order.
//! - The engine is MuPDF's `mutool` when installed (several times faster at text), else
//!   poppler's `pdftotext`.
//! - The file is written once to RAM (`/dev/shm`) for the workers to share.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use regex::bytes::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Mutool,
    Pdftotext,
}

pub static ENGINE: LazyLock<Option<Engine>> = LazyLock::new(|| {
    let has = |bin: &str| Command::new(bin).arg("-v").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok();
    if has("mutool") {
        Some(Engine::Mutool)
    } else if has("pdftotext") {
        Some(Engine::Pdftotext)
    } else {
        None
    }
});

/// Pages per worker below which splitting costs more than it saves (each worker re-reads the
/// document's structure and fonts).
const MIN_PAGES_PER_WORKER: usize = 3;

static COUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/Count\s+(\d+)").unwrap());

/// The page tree's `/Count` (the largest one is the root). None when it sits in a compressed
/// object stream; the PDF is then converted in one go.
pub fn page_count(bytes: &[u8]) -> Option<usize> {
    COUNT.captures_iter(bytes).filter_map(|c| std::str::from_utf8(&c[1]).ok()?.parse().ok()).max()
}

pub fn to_markdown(bytes: &[u8]) -> Result<String> {
    let engine = ENGINE.context("no PDF engine: install mupdf-tools or poppler")?;
    let file = TempPdf::new(bytes)?;
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let ranges = match page_count(bytes) {
        Some(pages) if pages >= 2 * MIN_PAGES_PER_WORKER => split(pages, workers.min(pages / MIN_PAGES_PER_WORKER)),
        _ => vec![None],
    };
    let texts: Vec<Result<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = ranges.iter().map(|r| s.spawn(|| extract(engine, file.path(), *r))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("PDF worker panicked")))).collect()
    });
    let mut out = String::new();
    for t in texts {
        let t = t?;
        if !t.trim().is_empty() {
            if !out.is_empty() {
                out.push('\u{c}');
            }
            out.push_str(&t);
        }
    }
    if out.trim().is_empty() {
        bail!("no text in this PDF (scanned images need OCR)");
    }
    Ok(paragraphs(&out))
}

fn split(pages: usize, parts: usize) -> Vec<Option<(usize, usize)>> {
    let parts = parts.max(1);
    let per = pages.div_ceil(parts);
    (0..parts).map(|i| (i * per + 1, ((i + 1) * per).min(pages))).filter(|(a, b)| a <= b).map(Some).collect()
}

fn extract(engine: Engine, path: &Path, range: Option<(usize, usize)>) -> Result<String> {
    let mut cmd = match engine {
        Engine::Mutool => {
            let mut c = Command::new("mutool");
            c.args(["draw", "-q", "-F", "txt", "-o", "-"]).arg(path);
            if let Some((a, b)) = range {
                c.arg(format!("{a}-{b}"));
            }
            c
        }
        Engine::Pdftotext => {
            let mut c = Command::new("pdftotext");
            c.args(["-q", "-enc", "UTF-8"]);
            if let Some((a, b)) = range {
                c.args(["-f", &a.to_string(), "-l", &b.to_string()]);
            }
            c.arg(path).arg("-");
            c
        }
    };
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().with_context(|| format!("running {engine:?}"))?;
    if !out.status.success() && out.stdout.is_empty() {
        bail!("{engine:?} could not read this PDF");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A PDF written to RAM for the workers, removed when dropped.
struct TempPdf(PathBuf);

impl TempPdf {
    fn new(bytes: &[u8]) -> Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = if Path::new("/dev/shm").is_dir() { PathBuf::from("/dev/shm") } else { std::env::temp_dir() };
        let path = dir.join(format!("snarf-{}-{}.pdf", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, bytes)?;
        Ok(TempPdf(path))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPdf {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Joins hard-wrapped lines into paragraphs and marks page breaks.
fn paragraphs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, page) in text.split('\u{c}').filter(|p| !p.trim().is_empty()).enumerate() {
        if i > 0 {
            out.push_str("\n\n---\n\n");
        }
        let mut para = String::new();
        for line in page.lines().map(str::trim) {
            if line.is_empty() {
                flush(&mut out, &mut para);
                continue;
            }
            if para.ends_with('-') && !para.ends_with("--") {
                para.pop(); // re-join a hyphenated word
            } else if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(line);
        }
        flush(&mut out, &mut para);
    }
    out.trim().to_string()
}

fn flush(out: &mut String, para: &mut String) {
    if !para.is_empty() {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push_str("\n\n");
        }
        out.push_str(para);
        para.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_wrapped_lines_and_pages() {
        let t = "Attention Is All\nYou Need\n\nThe domi-\nnant models\n\u{c}Page two.\n";
        assert_eq!(paragraphs(t), "Attention Is All You Need\n\nThe dominant models\n\n---\n\nPage two.");
    }

    #[test]
    fn splits_pages_evenly() {
        assert_eq!(split(15, 5), vec![Some((1, 3)), Some((4, 6)), Some((7, 9)), Some((10, 12)), Some((13, 15))]);
        assert_eq!(page_count(b"<< /Type /Pages /Count 7 >> << /Count 15 >>"), Some(15));
    }
}
