//! Firecrawl-style HTTP API.
//!
//! - `POST /v1/scrape` `{ "url": "...", ...ScrapeOptions }` → `{ success, data: Document }`
//! - `POST /v1/crawl`  `{ "url": "...", ...CrawlOptions }` → NDJSON stream, one event per page,
//!   then `{ "type": "done", ...summary }`
//! - `POST /v1/map`    `{ "url": "...", ...MapOptions }` → `{ success, links }`
//! - `POST /v1/batch/scrape` `{ "urls": [...], ...ScrapeOptions }` → `{ success, data: [Document | {error}] }`,
//!   16 pages in flight, results in request order
//!
//! Set `SNARF_API_KEY` to require `Authorization: Bearer <key>`.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use snarf::{CrawlOptions, Engine, MapOptions, ScrapeOptions};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Deserialize)]
struct WithUrl<T> {
    url: String,
    #[serde(flatten)]
    opts: T,
}

type App = Arc<Engine>;

pub async fn serve(engine: Arc<Engine>, addr: &str) -> Result<()> {
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/scrape", post(scrape))
        .route("/v1/crawl", post(crawl))
        .route("/v1/map", post(map))
        .route("/v1/batch/scrape", post(batch_scrape))
        .layer(middleware::from_fn(auth))
        .with_state(engine);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("snarf listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn auth(req: Request, next: Next) -> Response {
    if let Ok(key) = std::env::var("SNARF_API_KEY")
        && req.uri().path() != "/health"
    {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.strip_prefix("Bearer ") == Some(key.as_str()));
        if !ok {
            return fail(StatusCode::UNAUTHORIZED, "missing or wrong API key");
        }
    }
    next.run(req).await
}

fn fail(code: StatusCode, msg: impl ToString) -> Response {
    (code, Json(json!({ "success": false, "error": msg.to_string() }))).into_response()
}

async fn scrape(State(engine): State<App>, Json(req): Json<WithUrl<ScrapeOptions>>) -> Response {
    match engine.scrape(&req.url, &req.opts).await {
        Ok(doc) => Json(json!({ "success": true, "data": doc })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn map(State(engine): State<App>, Json(req): Json<WithUrl<MapOptions>>) -> Response {
    match snarf::map(&engine, &req.url, &req.opts).await {
        Ok(links) => Json(json!({ "success": true, "links": links })).into_response(),
        Err(e) => fail(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn crawl(State(engine): State<App>, Json(req): Json<WithUrl<CrawlOptions>>) -> Response {
    let (tx, rx) = mpsc::channel(64);
    let (lines_tx, lines_rx) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        let job = tokio::spawn(snarf::crawl(engine, req.url, req.opts, tx));
        let mut events = ReceiverStream::new(rx);
        while let Some(ev) = events.next().await {
            let line = serde_json::to_string(&ev).unwrap_or_default() + "\n";
            if lines_tx.send(line).await.is_err() {
                // The client went away; dropping the stream stops the crawl.
                return;
            }
        }
        let last = match job.await {
            Ok(Ok(s)) => json!({ "type": "done", "summary": s }),
            Ok(Err(e)) => json!({ "type": "failed", "error": format!("{e:#}") }),
            Err(e) => json!({ "type": "failed", "error": e.to_string() }),
        };
        let _ = lines_tx.send(last.to_string() + "\n").await;
    });
    let body = Body::from_stream(ReceiverStream::new(lines_rx).map(Ok::<_, std::convert::Infallible>));
    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(body)
        .unwrap()
}

#[derive(Deserialize)]
struct WithUrls {
    urls: Vec<String>,
    #[serde(flatten)]
    opts: ScrapeOptions,
}

async fn batch_scrape(State(engine): State<App>, Json(req): Json<WithUrls>) -> Response {
    if req.urls.len() > 1000 {
        return fail(StatusCode::BAD_REQUEST, "at most 1000 urls per batch");
    }
    let opts = Arc::new(req.opts);
    let data: Vec<serde_json::Value> = futures::stream::iter(req.urls)
        .map(|u| {
            let (engine, opts) = (engine.clone(), opts.clone());
            async move {
                match engine.scrape(&u, &opts).await {
                    Ok(doc) => serde_json::to_value(doc).unwrap_or_default(),
                    Err(e) => json!({ "url": u, "error": format!("{e:#}") }),
                }
            }
        })
        .buffered(16)
        .collect()
        .await;
    Json(json!({ "success": true, "data": data })).into_response()
}
