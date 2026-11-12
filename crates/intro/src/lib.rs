//! Warm-intro graph: who you know, who they know, and the best way to reach someone.
//!
//! People and organizations are nodes; relationships are weighted edges with the evidence they
//! came from. The graph is filled from sources you're allowed to use:
//!
//! - LinkedIn's own data export (`Connections.csv`), yours and any your friends share with you
//! - email (`.mbox`), calendar (`.ics`) and contacts (`.vcf`) exports: who you actually talk to
//! - GitHub: contributors to the same repositories, members of an organization
//! - public web pages snarf reads: team pages, portfolio pages, speaker lists
//! - connections you add by hand
//!
//! [`Graph::paths`] then ranks routes from you to a person or an organization, best first.

pub mod harvest;
pub mod linkedin;
pub mod names;
pub mod path;
pub mod sources;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub use harvest::{HarvestOptions, HarvestReport, Relation, harvest};
pub use path::{Hop, IntroPath, PathResult};

/// What an edge means, and how much an intro along it is worth (0–1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A first-degree LinkedIn connection, from someone's data export.
    Connection,
    /// Added by hand: you know these two actually know each other.
    Knows,
    /// Works at (or on the team of) an organization.
    WorksAt,
    /// An organization invested in another, from a portfolio page.
    InvestedIn,
    /// Listed on an organization's page (speaker, advisor, member, mentor).
    ListedOn,
    /// Emailed each other (strength grows with the number of messages).
    Emailed,
    /// Met: shared calendar events (strength grows with the number of meetings).
    Met,
    /// Saved in someone's phone or address book.
    Contact,
    /// Contributes to a code repository (repositories are organization nodes).
    Contributor,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Knows => "knows",
            Self::WorksAt => "works_at",
            Self::InvestedIn => "invested_in",
            Self::ListedOn => "listed_on",
            Self::Emailed => "emailed",
            Self::Met => "met",
            Self::Contact => "contact",
            Self::Contributor => "contributor",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "connection" => Self::Connection,
            "knows" => Self::Knows,
            "works_at" => Self::WorksAt,
            "invested_in" => Self::InvestedIn,
            "listed_on" => Self::ListedOn,
            "emailed" => Self::Emailed,
            "met" => Self::Met,
            "contact" => Self::Contact,
            "contributor" => Self::Contributor,
            _ => return None,
        })
    }

    /// Default strength. A LinkedIn connection is often loose; a hand-added "knows" is real.
    pub fn strength(self) -> f64 {
        match self {
            Self::Knows => 0.85,
            Self::Connection => 0.55,
            Self::WorksAt => 0.6,
            Self::InvestedIn => 0.4,
            Self::ListedOn => 0.3,
            Self::Emailed => 0.6,
            Self::Met => 0.65,
            Self::Contact => 0.7,
            Self::Contributor => 0.5,
        }
    }

    /// How the hop reads in a path ("Guy A —connection→ Guy B").
    pub fn label(self) -> &'static str {
        match self {
            Self::Connection => "LinkedIn connection",
            Self::Knows => "knows",
            Self::WorksAt => "works at",
            Self::InvestedIn => "invested in",
            Self::ListedOn => "listed on",
            Self::Emailed => "emails with",
            Self::Met => "met with",
            Self::Contact => "in contacts of",
            Self::Contributor => "contributes to",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Person,
    Org,
}

/// A person as a source describes them. Any field but `name` may be missing.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersonIn {
    pub name: String,
    pub title: Option<String>,
    pub org: Option<String>,
    pub email: Option<String>,
    /// Profile URL, used only to match records.
    pub linkedin: Option<String>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: i64,
    pub kind: NodeKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linkedin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub is_me: bool,
    pub degree: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stats {
    pub people: usize,
    pub orgs: usize,
    pub edges: usize,
    pub by_kind: Vec<(String, usize)>,
    pub sources: Vec<(String, usize)>,
    pub me: Option<String>,
}

pub struct Graph {
    db: Mutex<Connection>,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS nodes (
    id       INTEGER PRIMARY KEY,
    kind     TEXT NOT NULL,
    name     TEXT NOT NULL,
    key      TEXT NOT NULL,
    title    TEXT,
    org      TEXT,
    email    TEXT,
    linkedin TEXT,
    url      TEXT,
    is_me    INTEGER NOT NULL DEFAULT 0,
    created  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS nodes_key ON nodes(kind, key);
CREATE UNIQUE INDEX IF NOT EXISTS nodes_linkedin ON nodes(linkedin) WHERE linkedin IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS nodes_email ON nodes(email) WHERE email IS NOT NULL;
CREATE TABLE IF NOT EXISTS edges (
    a        INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    b        INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,
    strength REAL NOT NULL,
    source   TEXT NOT NULL,
    evidence TEXT,
    created  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (a, b, kind)
);
CREATE INDEX IF NOT EXISTS edges_b ON edges(b);
"#;

fn clean(s: Option<&str>) -> Option<String> {
    s.map(|v| v.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|v| !v.is_empty())
}

impl Graph {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }

    pub fn in_memory() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }

    /// `$SNARF_INTRO_DB`, else `~/.local/share/snarf/intro.db`.
    pub fn default_path() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("SNARF_INTRO_DB") {
            return p.into();
        }
        let base = std::env::var("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|_| ".".into());
        base.join("snarf/intro.db")
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` inside one transaction: imports of thousands of rows stay fast.
    pub fn batch<T>(&self, f: impl FnOnce(&Tx<'_>) -> Result<T>) -> Result<T> {
        let mut db = self.conn();
        let tx = db.transaction()?;
        let out = f(&Tx(&tx))?;
        tx.commit()?;
        Ok(out)
    }

    /// The node that is you, made on first use.
    pub fn me(&self) -> Result<i64> {
        self.batch(|t| t.me())
    }

    pub fn set_me(&self, name: &str) -> Result<i64> {
        self.batch(|t| {
            let id = t.me()?;
            t.0.execute("UPDATE nodes SET name = ?1, key = ?2 WHERE id = ?3", params![name.trim(), names::person_key(name), id])?;
            Ok(id)
        })
    }

    pub fn node(&self, id: i64) -> Result<Option<Node>> {
        let db = self.conn();
        node_by_id(&db, id)
    }

    /// People and organizations matching `q`: exact key matches first, then substrings of the
    /// name, title or organization.
    pub fn search(&self, q: &str, limit: usize) -> Result<Vec<Node>> {
        let db = self.conn();
        let (pk, ok) = (names::person_key(q), names::org_key(q));
        let mut ids: Vec<i64> = Vec::new();
        let mut push = |v: Vec<i64>| {
            for id in v {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        };
        let col = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> Result<Vec<i64>> {
            let mut st = db.prepare(sql)?;
            let r = st.query_map(p, |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(r)
        };
        push(col("SELECT id FROM nodes WHERE (kind='person' AND key=?1) OR (kind='org' AND key=?2)", params![pk, ok])?);
        if let Some(li) = names::linkedin_id(q) {
            push(col("SELECT id FROM nodes WHERE linkedin = ?1", params![li])?);
        }
        if q.contains('@') {
            push(col("SELECT id FROM nodes WHERE email = ?1", params![q.trim().to_lowercase()])?);
        }
        let like = format!("%{}%", q.trim());
        push(col(
            "SELECT n.id FROM nodes n WHERE n.name LIKE ?1 OR n.title LIKE ?1 OR n.org LIKE ?1
             ORDER BY (SELECT count(*) FROM edges e WHERE e.a=n.id OR e.b=n.id) DESC LIMIT ?2",
            params![like, limit as i64],
        )?);
        ids.truncate(limit);
        ids.into_iter().filter_map(|id| node_by_id(&db, id).transpose()).collect()
    }

    /// Everything one hop from `id`, strongest first.
    pub fn neighbors(&self, id: i64) -> Result<Vec<(Node, String, f64, Option<String>)>> {
        let db = self.conn();
        let mut st = db.prepare(
            "SELECT CASE WHEN a=?1 THEN b ELSE a END, kind, strength, evidence FROM edges WHERE a=?1 OR b=?1 ORDER BY strength DESC",
        )?;
        let rows = st
            .query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, Option<String>>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().filter_map(|(n, k, s, e)| node_by_id(&db, n).transpose().map(|n| n.map(|n| (n, k, s, e)))).collect()
    }

    pub fn stats(&self) -> Result<Stats> {
        let db = self.conn();
        let count = |sql: &str| -> Result<usize> { Ok(db.query_row(sql, [], |r| r.get::<_, i64>(0))? as usize) };
        let pairs = |sql: &str| -> Result<Vec<(String, usize)>> {
            let mut st = db.prepare(sql)?;
            let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))?.collect::<rusqlite::Result<_>>()?;
            Ok(v)
        };
        Ok(Stats {
            people: count("SELECT count(*) FROM nodes WHERE kind='person'")?,
            orgs: count("SELECT count(*) FROM nodes WHERE kind='org'")?,
            edges: count("SELECT count(*) FROM edges")?,
            by_kind: pairs("SELECT kind, count(*) FROM edges GROUP BY kind ORDER BY 2 DESC")?,
            sources: pairs("SELECT source, count(*) FROM edges GROUP BY source ORDER BY 2 DESC LIMIT 20")?,
            me: db.query_row("SELECT name FROM nodes WHERE is_me=1", [], |r| r.get(0)).optional()?,
        })
    }

    /// Every node and edge, for path search. Small graphs (100k edges) load in milliseconds.
    pub(crate) fn snapshot(&self) -> Result<path::Snapshot> {
        let db = self.conn();
        let mut st = db.prepare("SELECT id, kind FROM nodes")?;
        let nodes = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)? == "org")))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut st = db.prepare("SELECT a, b, kind, strength FROM edges")?;
        let edges = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let me = db.query_row("SELECT id FROM nodes WHERE is_me=1", [], |r| r.get(0)).optional()?;
        Ok(path::Snapshot::new(nodes, edges, me))
    }

    pub(crate) fn edge_evidence(&self, a: i64, b: i64, kind: &str) -> Option<String> {
        let (a, b) = (a.min(b), a.max(b));
        self.conn().query_row("SELECT evidence FROM edges WHERE a=?1 AND b=?2 AND kind=?3", params![a, b, kind], |r| r.get(0)).ok().flatten()
    }
}

fn node_by_id(db: &Connection, id: i64) -> Result<Option<Node>> {
    let n = db
        .query_row(
            "SELECT id, kind, name, title, org, email, linkedin, url, is_me,
                    (SELECT count(*) FROM edges e WHERE e.a=n.id OR e.b=n.id)
             FROM nodes n WHERE id = ?1",
            [id],
            |r| {
                Ok(Node {
                    id: r.get(0)?,
                    kind: if r.get::<_, String>(1)? == "org" { NodeKind::Org } else { NodeKind::Person },
                    name: r.get(2)?,
                    title: r.get(3)?,
                    org: r.get(4)?,
                    email: r.get(5)?,
                    linkedin: r.get(6)?,
                    url: r.get(7)?,
                    is_me: r.get::<_, i64>(8)? != 0,
                    degree: r.get::<_, i64>(9)? as usize,
                })
            },
        )
        .optional()?;
    Ok(n)
}

/// Writes inside one transaction.
pub struct Tx<'a>(&'a rusqlite::Transaction<'a>);

impl Tx<'_> {
    pub fn me(&self) -> Result<i64> {
        if let Some(id) = self.0.query_row("SELECT id FROM nodes WHERE is_me=1", [], |r| r.get(0)).optional()? {
            return Ok(id);
        }
        self.0.execute("INSERT INTO nodes(kind, name, key, is_me) VALUES ('person', 'Me', 'me', 1)", [])?;
        Ok(self.0.last_insert_rowid())
    }

    pub fn org(&self, name: &str) -> Result<i64> {
        let key = names::org_key(name);
        if key.is_empty() {
            bail!("empty organization name");
        }
        if let Some(id) = self.0.query_row("SELECT id FROM nodes WHERE kind='org' AND key=?1", [&key], |r| r.get(0)).optional()? {
            return Ok(id);
        }
        self.0.execute("INSERT INTO nodes(kind, name, key) VALUES ('org', ?1, ?2)", params![name.trim(), key])?;
        Ok(self.0.last_insert_rowid())
    }

    /// An organization by name or, failing that, by its website's domain ("Sequoiacap" and
    /// "Sequoia Capital" are one firm if both say sequoiacap.com).
    pub fn org_at(&self, name: &str, home: Option<&str>) -> Result<i64> {
        let domain = home.and_then(|h| url::Url::parse(h).ok()).and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()));
        let key = names::org_key(name);
        let by_key: Option<i64> = self.0.query_row("SELECT id FROM nodes WHERE kind='org' AND key=?1", [&key], |r| r.get(0)).optional()?;
        let id = match (by_key, &domain) {
            (Some(id), _) => id,
            (None, Some(d)) => {
                let hit: Option<i64> = self
                    .0
                    .query_row("SELECT id FROM nodes WHERE kind='org' AND (url LIKE ?1 OR url LIKE ?2)", params![format!("%://{d}/%"), format!("%://www.{d}/%")], |r| r.get(0))
                    .optional()?;
                match hit {
                    Some(id) => id,
                    None => self.org(name)?,
                }
            }
            (None, None) => self.org(name)?,
        };
        if let Some(d) = domain {
            self.0.execute("UPDATE nodes SET url = COALESCE(url, ?2) WHERE id = ?1", params![id, format!("https://{d}/")])?;
        }
        Ok(id)
    }

    /// Finds the node this person already is, or makes one. Matching, most certain first: LinkedIn
    /// profile, email, then name — and a name only matches when it's unambiguous (one person by
    /// that name, or one at the same organization) and no profile URL contradicts it.
    pub fn person(&self, p: &PersonIn) -> Result<i64> {
        let name = clean(Some(&p.name)).context("person without a name")?;
        let key = names::person_key(&name);
        if key.is_empty() {
            bail!("person without a name");
        }
        let li = p.linkedin.as_deref().and_then(names::linkedin_id);
        let email = clean(p.email.as_deref()).map(|e| e.to_lowercase()).filter(|e| e.contains('@'));
        let (title, org, url) = (clean(p.title.as_deref()), clean(p.org.as_deref()), clean(p.url.as_deref()));

        let find = |sql: &str, v: &str| -> Result<Option<i64>> { Ok(self.0.query_row(sql, [v], |r| r.get(0)).optional()?) };
        let mut id = None;
        if let Some(li) = &li {
            id = find("SELECT id FROM nodes WHERE linkedin=?1", li)?;
        }
        if id.is_none()
            && let Some(e) = &email
        {
            id = find("SELECT id FROM nodes WHERE email=?1", e)?;
        }
        if id.is_none() {
            let mut st = self.0.prepare("SELECT id, linkedin, org FROM nodes WHERE kind='person' AND key=?1 AND is_me=0")?;
            let cands = st
                .query_map([&key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // A different LinkedIn profile means a different person with the same name.
            let cands: Vec<_> = cands.into_iter().filter(|(_, l, _)| l.is_none() || li.is_none() || *l == li).collect();
            let same_org: Vec<_> = match &org {
                Some(o) => cands.iter().filter(|(_, _, co)| co.as_deref().is_some_and(|c| names::org_key(c) == names::org_key(o))).collect(),
                None => Vec::new(),
            };
            id = match (same_org.len(), cands.len()) {
                (1, _) => Some(same_org[0].0),
                (_, 1) => Some(cands[0].0),
                _ => None,
            };
        }
        match id {
            Some(id) => {
                // Fill gaps; a newer title or employer replaces an older one.
                self.0.execute(
                    "UPDATE nodes SET
                        title = COALESCE(?2, title), org = COALESCE(?3, org), url = COALESCE(url, ?4),
                        linkedin = COALESCE(linkedin, ?5), email = COALESCE(email, ?6)
                     WHERE id = ?1",
                    params![id, title, org, url, li, email],
                )?;
                Ok(id)
            }
            None => {
                self.0.execute(
                    "INSERT INTO nodes(kind, name, key, title, org, email, linkedin, url) VALUES ('person', ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![name, key, title, org, email, li, url],
                )?;
                Ok(self.0.last_insert_rowid())
            }
        }
    }

    /// Adds an edge (undirected), keeping the stronger one if it exists. Returns true when new.
    pub fn link(&self, a: i64, b: i64, kind: EdgeKind, strength: Option<f64>, source: &str, evidence: Option<&str>) -> Result<bool> {
        if a == b {
            return Ok(false);
        }
        let (a, b) = (a.min(b), a.max(b));
        let s = strength.unwrap_or(kind.strength()).clamp(0.01, 1.0);
        let existed = self
            .0
            .query_row("SELECT 1 FROM edges WHERE a=?1 AND b=?2 AND kind=?3", params![a, b, kind.as_str()], |_| Ok(()))
            .optional()?
            .is_some();
        self.0.execute(
            "INSERT INTO edges(a, b, kind, strength, source, evidence) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(a, b, kind) DO UPDATE SET strength = max(strength, excluded.strength),
                 evidence = COALESCE(evidence, excluded.evidence)",
            params![a, b, kind.as_str(), s, source, evidence],
        )?;
        Ok(!existed)
    }

    /// Links a person to their organization (and records it on the person).
    pub fn works_at(&self, person: i64, org: &str, kind: EdgeKind, source: &str, evidence: Option<&str>) -> Result<i64> {
        let o = self.org(org)?;
        self.link(person, o, kind, None, source, evidence)?;
        Ok(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_resolution() -> Result<()> {
        let g = Graph::in_memory()?;
        g.batch(|t| {
            let a = t.person(&PersonIn { name: "Jane Doe".into(), org: Some("Acme Inc".into()), ..Default::default() })?;
            let b = t.person(&PersonIn { name: "Dr. Jane  Doe".into(), linkedin: Some("https://linkedin.com/in/janedoe".into()), ..Default::default() })?;
            assert_eq!(a, b, "one Jane Doe so far: the name matches");
            let c = t.person(&PersonIn { name: "Jane Doe".into(), linkedin: Some("https://linkedin.com/in/other-jane".into()), ..Default::default() })?;
            assert_ne!(a, c, "a different profile is a different person");
            let d = t.person(&PersonIn { name: "Jane Doe".into(), org: Some("ACME".into()), ..Default::default() })?;
            assert_eq!(a, d, "two Jane Does now, but only one at Acme");
            Ok(())
        })
    }
}
