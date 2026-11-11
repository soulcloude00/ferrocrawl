//! `ferrocrawl mcp`: a Model Context Protocol server on stdio, so an AI agent (Claude Code,
//! Claude Desktop, any MCP client) can scrape, crawl and ask the intro graph directly.
//!
//! Register it with: `claude mcp add ferrocrawl -- ferrocrawl mcp`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ferrocrawl::{CrawlEvent, CrawlOptions, Engine, Format, MapOptions, ScrapeOptions};
use futures::StreamExt;
use intro::{EdgeKind, Graph, HarvestOptions, PersonIn, Relation};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    let s = |props: Value, req: &[&str]| json!({"type": "object", "properties": props, "required": req});
    json!([
        {"name": "scrape", "description": "Fetch one web page as clean Markdown (JavaScript pages are rendered only when needed). Fast: ~150 ms for a typical page.",
         "inputSchema": s(json!({"url": {"type": "string"}, "fullPage": {"type": "boolean", "description": "Keep navigation and footer; default false"}, "maxChars": {"type": "integer", "description": "Truncate the Markdown; default 40000"}}), &["url"])},
        {"name": "batch_scrape", "description": "Fetch many pages at once (in parallel) as Markdown. Much faster than calling scrape repeatedly.",
         "inputSchema": s(json!({"urls": {"type": "array", "items": {"type": "string"}}, "maxCharsEach": {"type": "integer", "description": "default 15000"}}), &["urls"])},
        {"name": "crawl", "description": "Crawl a site from a URL (pages under its path) and return each page's Markdown.",
         "inputSchema": s(json!({"url": {"type": "string"}, "limit": {"type": "integer", "description": "pages, default 20, max 200"}, "include": {"type": "array", "items": {"type": "string"}, "description": "path regexes"}, "maxChars": {"type": "integer", "description": "total, default 80000"}}), &["url"])},
        {"name": "map", "description": "List a site's URLs from its sitemaps and links, optionally filtered by a search word.",
         "inputSchema": s(json!({"url": {"type": "string"}, "search": {"type": "string"}, "limit": {"type": "integer"}}), &["url"])},
        {"name": "find_intro_path", "description": "Find the best warm-intro routes from the user to a person or organization (e.g. an investor or a fund), ranked by strength. Each route says whom to ask and the evidence for every hop.",
         "inputSchema": s(json!({"target": {"type": "string", "description": "Person name, organization, email, LinkedIn URL, or #id"}, "k": {"type": "integer", "description": "routes, default 5"}}), &["target"])},
        {"name": "search_people", "description": "Search the intro graph for people or organizations by name, title or company.",
         "inputSchema": s(json!({"query": {"type": "string"}, "limit": {"type": "integer"}}), &["query"])},
        {"name": "show_node", "description": "Show a person or organization and all their known connections, with evidence.",
         "inputSchema": s(json!({"query": {"type": "string"}}), &["query"])},
        {"name": "harvest_people", "description": "Read a public page (team page, portfolio page, speaker list) and add the people or portfolio companies it lists to the intro graph. Use to map an investor's partners or portfolio before calling find_intro_path. LinkedIn pages are refused.",
         "inputSchema": s(json!({"url": {"type": "string"}, "org": {"type": "string", "description": "Organization the page belongs to; default the site's name"}, "relation": {"type": "string", "enum": ["auto", "team", "portfolio", "people"]}, "limit": {"type": "integer", "description": "pages to crawl, default 1"}}), &["url"])},
        {"name": "add_connection", "description": "Record that two people know each other (use \"me\" for the user), e.g. from something the user said.",
         "inputSchema": s(json!({"a": {"type": "string"}, "b": {"type": "string"}, "strength": {"type": "number", "description": "0-1, default 0.85"}, "note": {"type": "string"}}), &["a", "b"])},
        {"name": "import_connections", "description": "Import a network file from disk: LinkedIn Connections.csv or contacts .csv, contacts .vcf, email .mbox (Gmail Takeout), calendar .ics. Email and meetings give the strongest intro paths. owner: whose network it is (omit for the user's own).",
         "inputSchema": s(json!({"path": {"type": "string"}, "owner": {"type": "string"}, "meEmails": {"type": "array", "items": {"type": "string"}, "description": "the user's own addresses, for .mbox/.ics"}}), &["path"])},
        {"name": "github_network", "description": "Add GitHub relationships: 'owner/repo' (its contributors), an org login (its public members), or a user login (contributors to their repos; mine=true if it's the user).",
         "inputSchema": s(json!({"target": {"type": "string"}, "mine": {"type": "boolean"}}), &["target"])},
        {"name": "graph_stats", "description": "Size of the intro graph and where its data came from.", "inputSchema": s(json!({}), &[])}
    ])
}

fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[truncated: {} of {} chars]", &s[..end], end, s.len())
}

struct Server {
    engine: Arc<Engine>,
    graph: Arc<Graph>,
}

impl Server {
    async fn call(&self, name: &str, a: &Value) -> Result<String> {
        let str_arg = |k: &str| a.get(k).and_then(Value::as_str).map(str::to_string);
        let need = |k: &str| str_arg(k).with_context(|| format!("missing {k}"));
        let num = |k: &str, d: u64| a.get(k).and_then(Value::as_u64).unwrap_or(d) as usize;
        match name {
            "scrape" => {
                let opts = ScrapeOptions { only_main_content: !a.get("fullPage").and_then(Value::as_bool).unwrap_or(false), ..Default::default() };
                let d = self.engine.scrape(&need("url")?, &opts).await?;
                let m = &d.metadata;
                Ok(format!("# {}\nURL: {}\n\n{}", m.title.as_deref().unwrap_or(""), m.url, cut(d.markdown.as_deref().unwrap_or(""), num("maxChars", 40_000))))
            }
            "batch_scrape" => {
                let urls: Vec<String> = a.get("urls").and_then(Value::as_array).context("missing urls")?.iter().filter_map(|u| u.as_str().map(str::to_string)).collect();
                let each = num("maxCharsEach", 15_000);
                let t0 = Instant::now();
                let engine = self.engine.clone();
                let docs: Vec<String> = futures::stream::iter(urls.into_iter().take(100))
                    .map(|u| {
                        let engine = engine.clone();
                        async move {
                            match engine.scrape(&u, &ScrapeOptions::default()).await {
                                Ok(d) => format!("## {}\nURL: {}\n\n{}", d.metadata.title.as_deref().unwrap_or(""), d.metadata.url, cut(d.markdown.as_deref().unwrap_or(""), each)),
                                Err(e) => format!("## {u}\nERROR: {e:#}"),
                            }
                        }
                    })
                    .buffered(16)
                    .collect()
                    .await;
                Ok(format!("{} pages in {} ms\n\n{}", docs.len(), t0.elapsed().as_millis(), docs.join("\n\n---\n\n")))
            }
            "crawl" => {
                let opts = CrawlOptions {
                    limit: num("limit", 20).min(200),
                    include_paths: a.get("include").and_then(Value::as_array).map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
                    scrape: ScrapeOptions { formats: vec![Format::Markdown], ..Default::default() },
                    ..Default::default()
                };
                let max = num("maxChars", 80_000);
                let (tx, mut rx) = mpsc::channel(64);
                let job = tokio::spawn(ferrocrawl::crawl(self.engine.clone(), need("url")?, opts, tx));
                let mut out = String::new();
                while let Some(ev) = rx.recv().await {
                    if let CrawlEvent::Page { document, .. } = ev {
                        out.push_str(&format!("\n\n---\n## {}\nURL: {}\n\n{}", document.metadata.title.as_deref().unwrap_or(""), document.metadata.url, document.markdown.as_deref().unwrap_or("")));
                        if out.len() > max {
                            break;
                        }
                    }
                }
                drop(rx);
                let s = job.await?.ok();
                Ok(format!("{} pages{}{}", s.as_ref().map(|s| s.pages).unwrap_or(0), s.map(|s| format!(" in {} ms", s.elapsed_ms)).unwrap_or_default(), cut(&out, max)))
            }
            "map" => {
                let opts = MapOptions { limit: num("limit", 2000), search: str_arg("search"), ..Default::default() };
                let links = ferrocrawl::map(&self.engine, &need("url")?, &opts).await?;
                Ok(format!("{} URLs\n{}", links.len(), links.join("\n")))
            }
            "find_intro_path" => {
                let g = self.graph.clone();
                let (target, k) = (need("target")?, num("k", 5));
                let r = tokio::task::spawn_blocking(move || g.paths(&target, k)).await??;
                // Slim hops for the model: what matters is who, how, and the evidence.
                let paths: Vec<Value> = r
                    .paths
                    .iter()
                    .map(|p| {
                        json!({"score": (p.score * 1000.0).round() / 1000.0, "ask": p.ask, "route": p.summary,
                               "hops": p.hops[1..].iter().map(|h| json!({"id": h.node.id, "name": h.node.name, "kind": h.node.kind, "title": h.node.title, "org": h.node.org, "linkedin": h.node.linkedin.as_ref().map(|l| format!("https://www.linkedin.com/{l}")), "via": h.via, "evidence": h.evidence})).collect::<Vec<_>>()})
                    })
                    .collect();
                let hint = if r.targets.is_empty() && r.candidates.is_empty() {
                    Some("target not in the graph: harvest_people on a page that lists them (e.g. the fund's team page), then retry")
                } else if r.targets.is_empty() {
                    Some("ambiguous: retry with one candidate's #id")
                } else if paths.is_empty() {
                    Some("no route yet: import more networks (a friend's Connections.csv via import_connections) or harvest pages linking them")
                } else {
                    None
                };
                Ok(serde_json::to_string_pretty(&json!({"targets": r.targets, "candidates": r.candidates, "paths": paths, "hint": hint}))?)
            }
            "search_people" => Ok(serde_json::to_string_pretty(&self.graph.search(&need("query")?, num("limit", 20))?)?),
            "show_node" => {
                let (t, c) = self.graph.resolve(&need("query")?)?;
                let mut out = Vec::new();
                for n in t.iter().chain(&c).take(5) {
                    let links: Vec<Value> = self.graph.neighbors(n.id)?.into_iter().take(60).map(|(m, k, s, e)| json!({"id": m.id, "name": m.name, "kind": k, "strength": s, "evidence": e})).collect();
                    out.push(json!({"node": n, "links": links}));
                }
                Ok(serde_json::to_string_pretty(&out)?)
            }
            "harvest_people" => {
                let relation = Relation::parse(&str_arg("relation").unwrap_or_else(|| "auto".into())).unwrap_or_default();
                let opts = HarvestOptions { org: str_arg("org"), relation, limit: num("limit", 1).min(200), include_paths: Vec::new() };
                let r = intro::harvest(self.engine.clone(), &self.graph, &need("url")?, &opts).await?;
                Ok(serde_json::to_string_pretty(&r)?)
            }
            "add_connection" => {
                let (a_name, b_name) = (need("a")?, need("b")?);
                let strength = a.get("strength").and_then(Value::as_f64);
                let note = str_arg("note");
                self.graph.batch(|t| {
                    let id = |n: &str| if n.eq_ignore_ascii_case("me") { t.me() } else { t.person(&PersonIn { name: n.into(), ..Default::default() }) };
                    let (x, y) = (id(&a_name)?, id(&b_name)?);
                    t.link(x, y, EdgeKind::Knows, strength, "agent", note.as_deref())?;
                    Ok(format!("recorded: {a_name} (#{x}) knows {b_name} (#{y})"))
                })
            }
            "import_connections" => {
                let me: Vec<String> = a.get("meEmails").and_then(Value::as_array).map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
                let r = intro::sources::import_any(&self.graph, &PathBuf::from(need("path")?), str_arg("owner").as_deref(), &me)?;
                Ok(serde_json::to_string_pretty(&r)?)
            }
            "github_network" => {
                let r = intro::sources::github(self.engine.http(), &self.graph, &need("target")?, a.get("mine").and_then(Value::as_bool).unwrap_or(false)).await?;
                Ok(serde_json::to_string_pretty(&r)?)
            }
            "graph_stats" => Ok(serde_json::to_string_pretty(&self.graph.stats()?)?),
            _ => Err(anyhow!("unknown tool {name}")),
        }
    }
}

pub async fn serve(engine: Arc<Engine>, db: Option<PathBuf>) -> Result<()> {
    let graph = Arc::new(Graph::open(db.unwrap_or_else(Graph::default_path))?);
    let server = Arc::new(Server { engine, graph });
    let out = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // a notification: nothing to answer
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let (server, out) = (server.clone(), out.clone());
        // Each request runs on its own, so a long crawl doesn't hold up a quick lookup.
        tokio::spawn(async move {
            let result = match method.as_str() {
                "initialize" => Ok(json!({
                    "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "ferrocrawl", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Fast web scraping (scrape, batch_scrape, crawl, map) and a warm-intro graph. To find who can introduce the user to someone: find_intro_path; if the target is missing, harvest_people on a page listing them (team/portfolio page) first."
                })),
                "ping" => Ok(json!({})),
                "tools/list" => Ok(json!({"tools": tools()})),
                "tools/call" => {
                    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    Ok(match server.call(name, &args).await {
                        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                        Err(e) => json!({"content": [{"type": "text", "text": format!("error: {e:#}")}], "isError": true}),
                    })
                }
                m => Err(json!({"code": -32601, "message": format!("method not found: {m}")})),
            };
            let reply = match result {
                Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
            };
            let mut o = out.lock().await;
            let _ = o.write_all(format!("{reply}\n").as_bytes()).await;
            let _ = o.flush().await;
        });
    }
    Ok(())
}
