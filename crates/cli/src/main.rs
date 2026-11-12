mod bench;
mod intro_cmd;
mod mcp;
mod server;

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use snarf::{CrawlEvent, CrawlOptions, Engine, EngineConfig, Format, MapOptions, Render, ScrapeOptions};
use tokio::sync::mpsc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "snarf", version, about = "A fast web scraper: pages to clean Markdown")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Out {
    Markdown,
    Html,
    Text,
    Links,
    Json,
}

#[derive(clap::Args)]
struct PageArgs {
    /// auto: browser only when the page needs JavaScript.
    #[arg(long, value_enum, default_value = "auto")]
    render: RenderArg,
    /// Keep the whole page (navigation, footer…), not just the main content.
    #[arg(long)]
    full: bool,
    /// Extra milliseconds to wait after load when rendering.
    #[arg(long, default_value_t = 0)]
    wait: u64,
    #[arg(long, default_value_t = 30_000)]
    timeout: u64,
    /// CSS selectors to drop, comma-separated.
    #[arg(long, value_delimiter = ',')]
    exclude: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum RenderArg {
    Auto,
    Always,
    Never,
}

impl PageArgs {
    fn options(&self, formats: Vec<Format>) -> ScrapeOptions {
        ScrapeOptions {
            formats,
            only_main_content: !self.full,
            render: match self.render {
                RenderArg::Auto => Render::Auto,
                RenderArg::Always => Render::Always,
                RenderArg::Never => Render::Never,
            },
            timeout_ms: self.timeout,
            wait_for_ms: self.wait,
            max_age_ms: 0,
            exclude_tags: self.exclude.clone(),
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Scrape one page.
    Scrape {
        url: String,
        #[arg(short, long, value_enum, default_value = "markdown")]
        format: Out,
        #[command(flatten)]
        page: PageArgs,
    },
    /// Crawl a site. Prints NDJSON, or writes one Markdown file per page with --out.
    Crawl {
        url: String,
        #[arg(short, long, default_value_t = 100)]
        limit: usize,
        #[arg(short, long, default_value_t = 5)]
        depth: usize,
        #[arg(short, long, default_value_t = 24)]
        concurrency: usize,
        /// Directory to write Markdown files into.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Regexes the path must match.
        #[arg(long)]
        include: Vec<String>,
        /// Regexes that rule a path out.
        #[arg(long)]
        exclude_path: Vec<String>,
        /// Crawl the whole site, not only below the start path.
        #[arg(long)]
        entire_domain: bool,
        #[arg(long)]
        subdomains: bool,
        #[arg(long)]
        ignore_robots: bool,
        #[arg(long)]
        no_sitemap: bool,
        #[command(flatten)]
        page: PageArgs,
    },
    /// List a site's URLs from its sitemaps and links.
    Map {
        url: String,
        #[arg(short, long, default_value_t = 5000)]
        limit: usize,
        #[arg(short, long)]
        search: Option<String>,
        #[arg(long)]
        subdomains: bool,
    },
    /// Run the HTTP API (Firecrawl-style /v1/scrape, /v1/crawl, /v1/map).
    Serve {
        #[arg(short, long, default_value = "127.0.0.1:3002")]
        addr: String,
    },
    /// Warm intros: import networks, harvest public pages, find who can introduce you.
    Intro {
        /// Graph database (default $SNARF_INTRO_DB or ~/.local/share/snarf/intro.db).
        #[arg(long, global = true)]
        db: Option<PathBuf>,
        #[command(subcommand)]
        cmd: intro_cmd::IntroCmd,
    },
    /// Run an MCP server on stdio for AI agents: scrape, batch_scrape, crawl, map and intro tools.
    Mcp {
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Run public scrape benchmarks (tinyfish, webcrawlerapi, trace) and compare with published results.
    Bench {
        #[command(subcommand)]
        cmd: bench::BenchCmd,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn,chromiumoxide=error,html5ever=error".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let engine = Engine::new(EngineConfig::default());

    match cli.cmd {
        Cmd::Scrape { url, format, page } => {
            let t0 = Instant::now();
            let opts = page.options(vec![Format::Markdown, Format::Html, Format::Text, Format::Links]);
            let doc = engine.scrape(&url, &opts).await?;
            let mut out = std::io::stdout().lock();
            match format {
                Out::Markdown => writeln!(out, "{}", doc.markdown.unwrap_or_default())?,
                Out::Html => writeln!(out, "{}", doc.html.unwrap_or_default())?,
                Out::Text => writeln!(out, "{}", doc.text.unwrap_or_default())?,
                Out::Links => {
                    for l in doc.links.unwrap_or_default() {
                        writeln!(out, "{l}")?;
                    }
                }
                Out::Json => writeln!(out, "{}", serde_json::to_string_pretty(&doc)?)?,
            }
            let m = &doc.metadata;
            eprintln!(
                "{} {} · fetch {} ms · extract {} ms · {} KB{} · total {} ms",
                m.status_code,
                m.url,
                m.fetch_ms,
                m.extract_ms,
                m.bytes / 1024,
                if m.rendered { " · rendered" } else { "" },
                t0.elapsed().as_millis()
            );
        }
        Cmd::Crawl {
            url,
            limit,
            depth,
            concurrency,
            out,
            include,
            exclude_path,
            entire_domain,
            subdomains,
            ignore_robots,
            no_sitemap,
            page,
        } => {
            let opts = CrawlOptions {
                limit,
                max_depth: depth,
                concurrency,
                include_paths: include,
                exclude_paths: exclude_path,
                entire_domain,
                allow_subdomains: subdomains,
                ignore_robots,
                use_sitemap: !no_sitemap,
                scrape: page.options(vec![Format::Markdown]),
            };
            if let Some(dir) = &out {
                std::fs::create_dir_all(dir)?;
            }
            let (tx, mut rx) = mpsc::channel(256);
            let job = tokio::spawn(snarf::crawl(engine.clone(), url, opts, tx));
            let mut stdout = std::io::stdout().lock();
            while let Some(ev) = rx.recv().await {
                match (&out, &ev) {
                    (Some(dir), CrawlEvent::Page { document, .. }) => {
                        let name = file_name(&document.metadata.url);
                        let body = format!(
                            "<!-- {} -->\n\n{}\n",
                            document.metadata.url,
                            document.markdown.as_deref().unwrap_or_default()
                        );
                        std::fs::write(dir.join(name), body)?;
                        eprintln!("✓ {}", document.metadata.url);
                    }
                    (Some(_), CrawlEvent::Error { url, error }) => eprintln!("✗ {url}: {error}"),
                    (None, _) => writeln!(stdout, "{}", serde_json::to_string(&ev)?)?,
                }
            }
            let s = job.await??;
            eprintln!(
                "{} pages, {} errors, {} URLs seen in {:.2}s ({:.1} pages/s)",
                s.pages,
                s.errors,
                s.discovered,
                s.elapsed_ms as f64 / 1000.0,
                s.pages_per_sec
            );
        }
        Cmd::Map { url, limit, search, subdomains } => {
            let t0 = Instant::now();
            let opts = MapOptions { limit, search, include_subdomains: subdomains, ..Default::default() };
            let links = snarf::map(&engine, &url, &opts).await?;
            let mut out = std::io::stdout().lock();
            for l in &links {
                writeln!(out, "{l}")?;
            }
            eprintln!("{} URLs in {} ms", links.len(), t0.elapsed().as_millis());
        }
        Cmd::Serve { addr } => server::serve(engine, &addr).await?,
        Cmd::Bench { cmd } => bench::main(engine, cmd).await?,
        Cmd::Intro { db, cmd } => intro_cmd::main(engine, db, cmd).await?,
        Cmd::Mcp { db } => mcp::serve(engine, db).await?,
    }
    Ok(())
}

fn file_name(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let mut s: String = rest
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect();
    s = s.trim_matches('_').to_string();
    s.truncate(180);
    format!("{s}.md")
}
