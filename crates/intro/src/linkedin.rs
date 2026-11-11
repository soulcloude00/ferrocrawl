//! Imports LinkedIn's data export (Settings → Data privacy → Get a copy of your data →
//! Connections). Its `Connections.csv` starts with a few lines of notes, then the header
//! `First Name,Last Name,URL,Email Address,Company,Position,Connected On`.
//!
//! Any contacts CSV with recognizable headers works too (Google Contacts, a CRM export), so a
//! friend can share whatever they have.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::{EdgeKind, Graph, PersonIn};

#[derive(Clone, Debug, Default, Serialize)]
pub struct ImportReport {
    pub owner: String,
    pub rows: usize,
    pub new_connections: usize,
    pub skipped: usize,
}

#[derive(Default)]
struct Columns {
    first: Option<usize>,
    last: Option<usize>,
    full: Option<usize>,
    url: Option<usize>,
    email: Option<usize>,
    company: Option<usize>,
    title: Option<usize>,
    connected: Option<usize>,
}

impl Columns {
    fn from(headers: &csv::StringRecord) -> Self {
        let mut c = Columns::default();
        for (i, h) in headers.iter().enumerate() {
            let h = h.trim().trim_start_matches('\u{feff}').to_lowercase();
            let slot = match h.as_str() {
                "first name" | "given name" | "firstname" => &mut c.first,
                "last name" | "family name" | "surname" | "lastname" => &mut c.last,
                "name" | "full name" | "fullname" | "display name" => &mut c.full,
                "url" | "profile url" | "linkedin" | "linkedin url" | "linkedin profile" => &mut c.url,
                "email address" | "email" | "e-mail 1 - value" | "email 1" | "e-mail address" => &mut c.email,
                "company" | "organization" | "organization name" | "organization 1 - name" | "employer" => &mut c.company,
                "position" | "title" | "job title" | "organization title" | "organization 1 - title" => &mut c.title,
                "connected on" => &mut c.connected,
                _ => continue,
            };
            slot.get_or_insert(i);
        }
        c
    }

    fn usable(&self) -> bool {
        self.full.is_some() || self.first.is_some()
    }
}

/// Skips the notes LinkedIn puts above the header.
fn body(text: &str) -> &str {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let l = line.trim_start_matches('\u{feff}').to_lowercase();
        if l.starts_with("first name,") || l.starts_with("\"first name\"") || l.starts_with("name,") || l.contains(",first name,") || l.starts_with("given name,") {
            return &text[offset..];
        }
        offset += line.len();
    }
    text
}

pub fn import_file(graph: &Graph, path: &Path, owner: Option<&str>) -> Result<ImportReport> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    import_str(graph, &text, owner, &path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "csv".into()))
}

/// Imports `owner`'s connections (you when `owner` is None).
pub fn import_str(graph: &Graph, text: &str, owner: Option<&str>, source_name: &str) -> Result<ImportReport> {
    let mut rd = csv::ReaderBuilder::new().flexible(true).trim(csv::Trim::All).from_reader(body(text).as_bytes());
    let cols = Columns::from(rd.headers()?);
    if !cols.usable() {
        bail!("no name columns found: expected LinkedIn's Connections.csv or a CSV with a Name / First Name column");
    }
    let records: Vec<csv::StringRecord> = rd.records().filter_map(|r| r.ok()).collect();
    graph.batch(|t| {
        let (owner_id, owner_name) = match owner {
            Some(name) => (t.person(&PersonIn { name: name.into(), ..Default::default() })?, name.to_string()),
            None => (t.me()?, "you".to_string()),
        };
        let source = format!("{source_name} of {owner_name}");
        let mut rep = ImportReport { owner: owner_name, rows: records.len(), ..Default::default() };
        for r in &records {
            let get = |i: Option<usize>| i.and_then(|i| r.get(i)).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
            let name = match get(cols.full) {
                Some(n) => n,
                None => [get(cols.first), get(cols.last)].into_iter().flatten().collect::<Vec<_>>().join(" "),
            };
            if name.trim().is_empty() {
                rep.skipped += 1;
                continue;
            }
            let p = PersonIn {
                name,
                title: get(cols.title),
                org: get(cols.company),
                email: get(cols.email),
                linkedin: get(cols.url),
                url: None,
            };
            let Ok(id) = t.person(&p) else {
                rep.skipped += 1;
                continue;
            };
            let evidence = get(cols.connected).map(|d| format!("{source}, connected {d}")).unwrap_or_else(|| source.clone());
            if t.link(owner_id, id, EdgeKind::Connection, None, &source, Some(&evidence))? {
                rep.new_connections += 1;
            }
            if let Some(org) = &p.org {
                t.works_at(id, org, EdgeKind::WorksAt, &source, Some(&format!("{} at {org} ({source})", p.title.as_deref().unwrap_or("works"))))?;
            }
        }
        Ok(rep)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = "Notes:\n\"When exporting your connection data, you may notice that some of the email addresses are missing.\"\n\nFirst Name,Last Name,URL,Email Address,Company,Position,Connected On\nGuy,A,https://www.linkedin.com/in/guy-a,,Acme Ventures,Partner,12 Mar 2021\nSam,Lee,https://www.linkedin.com/in/samlee,sam@x.io,Beta Labs,CTO,01 Jan 2024\n";

    #[test]
    fn imports_linkedin_export() -> Result<()> {
        let g = Graph::in_memory()?;
        let r = import_str(&g, EXPORT, None, "Connections.csv")?;
        assert_eq!((r.rows, r.new_connections), (2, 2));
        let s = g.stats()?;
        assert_eq!((s.people, s.orgs), (3, 2));
        // Re-importing changes nothing.
        assert_eq!(import_str(&g, EXPORT, None, "Connections.csv")?.new_connections, 0);
        Ok(())
    }
}
