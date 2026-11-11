//! Ranking warm-intro routes.
//!
//! An edge of strength `s` costs `-ln s`, so a path's cost is minus the log of the product of its
//! strengths, and the cheapest path is the most likely intro. One Dijkstra runs from the target(s)
//! outward; then every one of your direct contacts is scored by its own best route, so the result
//! is "ask A (0.31), or B (0.22), or C…", not one path and its near-duplicates.
//!
//! Organizations are hubs: sharing an employer of 50,000 says little about knowing each other.
//! Passing through an org costs extra the more people it connects.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::{EdgeKind, Graph, Node, NodeKind, names};

/// Hops beyond this aren't intros anyone will make.
const MAX_EDGES: usize = 5;
/// An org this connected costs nothing extra to pass through; bigger ones cost more.
const HUB_FREE: f64 = 12.0;

pub(crate) struct Snapshot {
    ids: Vec<i64>,
    index: HashMap<i64, usize>,
    is_org: Vec<bool>,
    adj: Vec<Vec<(usize, f64, EdgeKind)>>,
    me: Option<usize>,
}

impl Snapshot {
    pub(crate) fn new(nodes: Vec<(i64, bool)>, edges: Vec<(i64, i64, String, f64)>, me: Option<i64>) -> Self {
        let index: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
        let mut adj = vec![Vec::new(); nodes.len()];
        for (a, b, kind, s) in edges {
            let (Some(&a), Some(&b), Some(kind)) = (index.get(&a), index.get(&b), EdgeKind::parse(&kind)) else { continue };
            let cost = -s.clamp(0.01, 1.0).ln();
            adj[a].push((b, cost, kind));
            adj[b].push((a, cost, kind));
        }
        Self {
            ids: nodes.iter().map(|n| n.0).collect(),
            is_org: nodes.iter().map(|n| n.1).collect(),
            me: me.and_then(|m| index.get(&m).copied()),
            index,
            adj,
        }
    }

    fn hub_cost(&self, v: usize) -> f64 {
        if !self.is_org[v] {
            return 0.0;
        }
        0.5 * (self.adj[v].len() as f64 / HUB_FREE).max(1.0).ln()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Hop {
    #[serde(flatten)]
    pub node: Node,
    /// How this hop connects to the one before it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IntroPath {
    /// 0–1: the product of the hops' strengths, less for passing through big organizations.
    pub score: f64,
    /// The person to ask for the intro (None when you're directly connected).
    pub ask: Option<String>,
    pub summary: String,
    pub hops: Vec<Hop>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PathResult {
    pub targets: Vec<Node>,
    /// Set when the query matched several people and none exactly: pick one and ask again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Node>,
    pub paths: Vec<IntroPath>,
}

#[derive(PartialEq)]
struct Item(f64, usize, usize);
impl Eq for Item {}
impl Ord for Item {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.partial_cmp(&self.0).unwrap_or(Ordering::Equal)
    }
}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

fn label(kind: EdgeKind, from_org: bool, to_org: bool) -> &'static str {
    match (kind, from_org, to_org) {
        (EdgeKind::WorksAt, false, true) => "works at",
        (EdgeKind::WorksAt, true, false) => "team member",
        (EdgeKind::ListedOn, false, true) => "listed on",
        (EdgeKind::ListedOn, true, false) => "lists",
        (EdgeKind::InvestedIn, ..) => "portfolio link",
        (EdgeKind::Contributor, false, true) => "contributes to",
        (EdgeKind::Contributor, true, false) => "contributor",
        (EdgeKind::Contact, ..) => "in contacts",
        (k, ..) => k.label(),
    }
}

impl Graph {
    /// Resolves `query` (a name, organization, email, LinkedIn URL or `#id`) to target nodes.
    pub fn resolve(&self, query: &str) -> Result<(Vec<Node>, Vec<Node>)> {
        if let Some(id) = query.strip_prefix('#').and_then(|s| s.parse().ok()) {
            return Ok((self.node(id)?.into_iter().collect(), Vec::new()));
        }
        let found = self.search(query, 25)?;
        let (pk, ok) = (names::person_key(query), names::org_key(query));
        let exact: Vec<Node> = found
            .iter()
            .filter(|n| match n.kind {
                NodeKind::Person => names::person_key(&n.name) == pk || names::linkedin_id(query).is_some_and(|l| n.linkedin.as_deref() == Some(&l)),
                NodeKind::Org => names::org_key(&n.name) == ok,
            })
            .cloned()
            .collect();
        if !exact.is_empty() {
            return Ok((exact, Vec::new()));
        }
        // "sequoia" means the firm, not everyone who works there.
        let q = query.trim().to_lowercase();
        let orgs: Vec<&Node> = found.iter().filter(|n| n.kind == NodeKind::Org && (n.name.to_lowercase().contains(&q) || names::org_key(&n.name).starts_with(&ok))).collect();
        if orgs.len() == 1 {
            return Ok((vec![orgs[0].clone()], Vec::new()));
        }
        Ok(match found.len() {
            0 => (Vec::new(), Vec::new()),
            1 => (found, Vec::new()),
            _ => (Vec::new(), found),
        })
    }

    /// The best `k` routes from you to `query`, one per person you'd ask.
    pub fn paths(&self, query: &str, k: usize) -> Result<PathResult> {
        let (targets, candidates) = self.resolve(query)?;
        if targets.is_empty() {
            return Ok(PathResult { targets, candidates, paths: Vec::new() });
        }
        let snap = self.snapshot()?;
        let Some(me) = snap.me else { bail!("the graph has no you yet: import your LinkedIn Connections.csv first") };
        if snap.adj[me].is_empty() {
            bail!("you have no connections yet: import your LinkedIn Connections.csv first");
        }
        let tset: Vec<usize> = targets.iter().filter_map(|t| snap.index.get(&t.id).copied()).collect();
        let n = snap.ids.len();
        let mut dist = vec![f64::INFINITY; n];
        let mut hops = vec![usize::MAX; n];
        let mut prev: Vec<Option<(usize, EdgeKind)>> = vec![None; n];
        let mut heap = BinaryHeap::new();
        for &t in &tset {
            dist[t] = 0.0;
            hops[t] = 0;
            heap.push(Item(0.0, t, 0));
        }
        while let Some(Item(d, u, h)) = heap.pop() {
            if d > dist[u] || u == me || h >= MAX_EDGES - 1 {
                continue;
            }
            let through = if tset.contains(&u) { 0.0 } else { snap.hub_cost(u) };
            for &(v, c, kind) in &snap.adj[u] {
                let nd = d + c + through;
                if nd < dist[v] {
                    dist[v] = nd;
                    hops[v] = h + 1;
                    prev[v] = Some((u, kind));
                    heap.push(Item(nd, v, h + 1));
                }
            }
        }

        // Score each first hop by its own best route.
        let mut best: HashMap<usize, (f64, EdgeKind)> = HashMap::new();
        for &(v, c, kind) in &snap.adj[me] {
            if !dist[v].is_finite() {
                continue;
            }
            let through = if tset.contains(&v) { 0.0 } else { snap.hub_cost(v) };
            let total = c + dist[v] + through;
            if best.get(&v).is_none_or(|(t, _)| total < *t) {
                best.insert(v, (total, kind));
            }
        }
        let mut ranked: Vec<(usize, f64, EdgeKind)> = best.into_iter().map(|(v, (t, k))| (v, t, k)).collect();
        ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));

        let mut paths = Vec::new();
        let mut asked: Vec<i64> = Vec::new();
        for (first, total, kind) in ranked {
            if paths.len() >= k {
                break;
            }
            // Walk target-ward from the first hop.
            let mut chain: Vec<(usize, Option<EdgeKind>)> = vec![(me, None), (first, Some(kind))];
            let mut cur = first;
            while let Some((nx, k)) = prev[cur] {
                chain.push((nx, Some(k)));
                cur = nx;
            }
            let mut out_hops = Vec::new();
            for (i, &(v, k)) in chain.iter().enumerate() {
                let Some(node) = self.node(snap.ids[v])? else { continue };
                let (via, evidence) = match (k, i.checked_sub(1).map(|p| chain[p].0)) {
                    (Some(k), Some(p)) => (Some(label(k, snap.is_org[p], snap.is_org[v]).to_string()), self.edge_evidence(snap.ids[p], snap.ids[v], k.as_str())),
                    _ => (None, None),
                };
                out_hops.push(Hop { node, via, evidence });
            }
            if out_hops.len() < 2 {
                continue;
            }
            // The person to ask: the first person after you who isn't the target. Two first hops
            // that lead to the same introducer (your coworker via two orgs) count once.
            let ask = out_hops[1..out_hops.len() - 1].iter().find(|h| h.node.kind == NodeKind::Person).map(|h| (h.node.id, h.node.name.clone()));
            if let Some((id, _)) = &ask {
                if asked.contains(id) {
                    continue;
                }
                asked.push(*id);
            }
            let summary = std::iter::once("You".to_string())
                .chain(out_hops[1..].iter().map(|h| format!("—{}→ {}", h.via.as_deref().unwrap_or("?"), h.node.name)))
                .collect::<Vec<_>>()
                .join(" ");
            paths.push(IntroPath { score: (-total).exp(), ask: ask.map(|a| a.1), summary, hops: out_hops });
        }
        Ok(PathResult { targets, candidates, paths })
    }
}

#[cfg(test)]
mod tests {
    use crate::{EdgeKind, Graph, PersonIn};
    use anyhow::Result;

    fn p(name: &str) -> PersonIn {
        PersonIn { name: name.into(), ..Default::default() }
    }

    #[test]
    fn finds_friend_of_friend_and_ranks_strongest_first() -> Result<()> {
        let g = Graph::in_memory()?;
        g.batch(|t| {
            let me = t.me()?;
            let a = t.person(&p("Guy A"))?;
            let c = t.person(&p("Cara C"))?;
            let b = t.person(&p("Guy B"))?;
            t.link(me, a, EdgeKind::Knows, None, "test", None)?;
            t.link(me, c, EdgeKind::Connection, None, "test", None)?;
            t.link(a, b, EdgeKind::Connection, None, "test", None)?;
            // Cara reaches B only through an org.
            t.works_at(c, "Fund X", EdgeKind::WorksAt, "test", None)?;
            t.works_at(b, "Fund X", EdgeKind::WorksAt, "test", None)?;
            Ok(())
        })?;
        let r = g.paths("guy b", 5)?;
        assert_eq!(r.paths.len(), 2);
        assert_eq!(r.paths[0].ask.as_deref(), Some("Guy A"));
        assert_eq!(r.paths[1].ask.as_deref(), Some("Cara C"));
        assert!(r.paths[0].score > r.paths[1].score);
        // An organization works as a target: who can get me into Fund X?
        let r = g.paths("Fund X", 5)?;
        assert_eq!(r.paths[0].ask.as_deref(), Some("Cara C"));
        Ok(())
    }
}
