# ferrocrawl

A fast web scraper in Rust. It turns pages into clean Markdown for LLMs, and can crawl and map whole sites. It comes as a CLI, a Firecrawl-style HTTP API and a desktop app built with [Rune](https://github.com/anomalyco/rune).

## Why it's fast

- **No browser unless a page needs one.** Pages come over one pooled HTTP/2 client with compression. Headless Chromium starts on first need, and only for pages that look client-rendered (an empty `#root`/`#__next` shell, or "enable JavaScript" with no text). `--render always` forces it.
- **A streaming cleaner.** One `lol_html` pass (Cloudflare's rewriter) drops scripts and styles, makes links and images absolute, un-lazies images and collects links. Then `html5ever` picks the main content, and `htmd` writes the Markdown on Tokio's blocking pool.
- **The crawl streams.** Up to N pages are in flight at once, and each result goes out as soon as it lands. Sitemaps seed the queue. Duplicate URLs are dropped after normalizing them.
- **A shared cache.** Identical requests made at the same time share one fetch (moka), bounded by bytes.

Measured from a home connection (`--release`):

| Job | Result |
| --- | --- |
| Scrape one MDN page | 163 ms total (149 ms network, 13 ms extract) |
| Crawl the whole Rust Book (113 pages) | 0.76 s, 149 pages/s |
| Crawl MDN HTTP docs, 300 pages | 2.4 s, 125 pages/s |
| Crawl Python stdlib docs, 328 pages | 3.3 s, 100 pages/s |

## Use

```sh
cargo build --release                        # CLI + server (no GUI deps)
cargo build --release -p ferrocrawl-app      # the Rune desktop app

ferrocrawl scrape https://example.com                    # Markdown to stdout
ferrocrawl scrape https://example.com -f json            # markdown, html, text, links, metadata
ferrocrawl crawl https://doc.rust-lang.org/book/ -l 500 -o book/   # one .md per page
ferrocrawl crawl https://site.dev -l 100 > pages.ndjson  # NDJSON stream
ferrocrawl map https://site.dev --search blog
ferrocrawl serve -a 127.0.0.1:3002
ferrocrawl-app                                           # desktop app
```

`crawl` keeps to pages under the start URL's path. `--entire-domain` lifts that, and `--include`/`--exclude-path` take regexes. It respects `robots.txt` unless you pass `--ignore-robots`.

## HTTP API

```sh
curl -X POST localhost:3002/v1/scrape -H 'content-type: application/json' \
  -d '{"url":"https://example.com","formats":["markdown","links"]}'
curl -N -X POST localhost:3002/v1/crawl -d '{"url":"https://site.dev","limit":100}' -H 'content-type: application/json'
curl -X POST localhost:3002/v1/map -d '{"url":"https://site.dev"}' -H 'content-type: application/json'
```

`/v1/crawl` streams NDJSON: one `{"type":"page",...}` line per page, then `{"type":"done","summary":{...}}`. Closing the connection stops the crawl. Set `FERROCRAWL_API_KEY` to require `Authorization: Bearer <key>`.

Scrape options (camelCase, all optional): `formats` (`markdown`, `html`, `rawHtml`, `links`, `text`), `onlyMainContent`, `render` (`auto`, `always`, `never`), `timeoutMs`, `waitForMs`, `maxAgeMs`, `excludeTags`.

## Desktop app

Type a URL and pick Scrape, Crawl or Map, then rendering (Auto, HTTP or Browser) and a page limit. Pages stream into the list with timing, size, word count and a JS badge when the browser rendered them. The right side shows the rendered Markdown or the raw source, with Copy. Use ↑/↓ to move through pages and Esc to stop. `ferrocrawl-app --snap out.png --scene demo` renders it headlessly.

## On a machine with little RAM

Build and run through the scripts. Each runs in its own memory-capped systemd scope, so if one
runs out, only it is killed, not your terminal or desktop session:

```sh
scripts/build.sh -p ferrocrawl-app          # cargo build --release, capped (FERROCRAWL_BUILD_MEM, default 2.5G)
scripts/run-app.sh                          # the desktop app, capped (FERROCRAWL_APP_MEM, default 2G)
FERROCRAWL_BENCH=trace scripts/run-app.sh   # open straight into a benchmark (add FERROCRAWL_BENCH_RUN=1 to start it)
```

Headless Chromium moves itself into its own systemd scope; ferrocrawl caps that scope at 1.2 GB
(`FERROCRAWL_CHROME_MEM`) and starts a fresh browser if it gets killed. Plain `--release` builds
skip LTO to keep the linker small; `--profile dist` is the fully optimized build.

## Benchmarks

Saved runs of the WebCrawlerAPI and TRACE benchmarks, with scores, per-page results and judge
verdicts, are in [`benchmarks/`](benchmarks/RESULTS.md). The app's Benchmark tab and
`ferrocrawl bench run <suite>` rerun them.

## Layout

- `crates/core`: the engine (`fetch`, `extract`, `render`, `pdf`, `crawl`, `map`, `urls`, `bench`)
- `crates/cli`: CLI and HTTP server
- `crates/app`: the Rune app

## Tests

```sh
cargo test                                         # offline unit tests
cargo test --release -p ferrocrawl-app -- --ignored   # live crawl/map through the app state
```

## Not yet

PDF and DOCX parsing, proxy rotation and anti-bot handling, screenshots, LLM extraction (`/extract` with a JSON schema), and persistent async crawl jobs.

## License

Proprietary. All rights reserved; see [LICENSE](LICENSE).
