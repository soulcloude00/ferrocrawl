# Benchmark results

Runs of snarf against two public scrape benchmarks, scored with each benchmark's own
method. Every folder holds the run's `summary.json` (scores and leaderboard), `results.jsonl`
(one line per page: status, time, size, scores), `meta.json` (wall time, concurrency) and, for
LLM-judged suites, `verdicts.jsonl` (one judge verdict per page). Scraped page contents are not
kept here. Measured from a home connection, from one laptop, on 2026-11-09 to 2026-11-12. Charts in
`img/` are drawn from these files by `scripts/bench_charts.py`.

Rerun any of them with `snarf bench run <suite>` (or the app's Benchmark tab), and rescore a
saved run with `snarf bench report <dir>`, or re-extract its saved HTML with the current
code with `snarf bench rescore <dir>`.

## WebCrawlerAPI

1,000 rows of Firecrawl's public `scrape-content-dataset-v1` (133 marked broken by the dataset,
867 scraped). Scored by truth/lie token overlap; our scorer was checked against the benchmark's
own `benchmark.mjs`: identical on all 687 scored rows of `webcrawlerapi-iter5/`.

Current run: `webcrawlerapi-2026-11-12/` (16 concurrent, 167 s for the whole set).

| Provider | Coverage of returned | End-to-end (of 867) | Recall | F1 | p50 | p95 |
|---|---|---|---|---|---|---|
| context.dev | 94.9% | 86.2% | 82.5% | 0.175 | 3.07 s | 17.2 s |
| **snarf** | **93.9%** | **75.1%** | 75.9% | 0.561 | **1.15 s** | **8.6 s** |
| context.dev (main content) | 92.6% | 84.0% | 78.8% | 0.567 | 3.56 s | 29.8 s |
| WebCrawlerAPI | 92.6% | | 72.6% | 0.622 | 8.30 s | 49.3 s |
| Crawl4AI Cloud | 92.3% | | 75.7% | 0.560 | 1.60 s | 31.0 s |
| Firecrawl | 89.0% | 86.2% | 74.9% | 0.515 | 3.55 s | 16.0 s |
| Spider | 88.7% | | 73.0% | 0.520 | 5.76 s | 94.8 s |
| Jina Reader | 87.4% | | 73.5% | 0.178 | 5.39 s | 20.5 s |
| Crawlbase | 74.9% | | 46.5% | 0.465 | 9.84 s | 130.5 s |

Read end-to-end as the honest number: coverage counts only pages a provider returned, so failing
on hard pages raises it. snarf still returns fewer pages than context.dev and Firecrawl;
most of its 174 errors are sites that block non-browser clients, and snarf doesn't try to
get past captchas or bot walls. The extraction rules were tuned while iterating on this
dataset, so treat it as a regression check, not a held-out score.

**What's left of the gap.** Of the 42 returned pages snarf misses, 13 have no truth text
in the dataset (no provider can cover them), 12 have truth text that isn't in the HTML the site
serves today, and in most of the rest the truth is in another language than the page (English
truth for a French or Dutch site). From the HTML it receives, snarf's ceiling is about 94.2%.

**Speed work, 2026-11-11 → 2026-11-12** (`webcrawlerapi-2026-11-11-baseline/` → `webcrawlerapi-2026-11-12/`,
same URLs, same concurrency):

| | Before | After |
|---|---|---|
| Coverage | 93.2% | 93.9% |
| Pages returned | 678 | 693 |
| p50 / p90 / p95 / p99 | 1.77 / 9.75 / 12.2 / 21.0 s | 1.15 / 7.20 / 8.60 / 12.2 s |
| Mean | 3.85 s | 2.46 s |
| Whole run | 241 s | 167 s |

What changed: unreachable hosts and hard blocks skip the browser; renders block fonts, media and
ad networks and stop when the text settles (a loading shell gets up to 6 s); a page behind a bot
screen waits for a browser tab instead of giving up after 300 ms; picture galleries and text
held in a page's inline app state are kept; a missing meta description is added; plain-text
"Not Found" and firewall deny pages count as failures, not content. Network speed alone moves p50
by up to ~0.3 s between runs, so single-run deltas below that are noise.

`webcrawlerapi-iter5/` is the earlier run (92.7%, p50 1.60 s), kept for reference.

## TRACE

Runs: `trace-run1/` (baseline) and `trace-run2/` (current). Both predate the 2026-11-12 speed work.

context.dev's TRACE sample of 2026-11-06: 2,527 fresh pages in nine categories, scored as the
mean of per-category success rates (success = judged *substantive* and the requested page). The
other providers' numbers are recomputed from TRACE's published page-level data and match its
published table exactly.

The judge here is Claude Haiku agents given TRACE's rubric and evidence rule (full text up to
12,000 characters, else three 4,000-character excerpts), not TRACE's own judge.

| Provider | Score | p50 | p95 |
|---|---|---|---|
| context.dev | 87.0 | 1.75 s | 8.9 s |
| Keenable | 84.2 | 1.27 s | 16.8 s |
| Firecrawl | 82.3 | 1.60 s | 11.2 s |
| Exa | 79.1 | 1.19 s | 18.1 s |
| **snarf, run 2** | **78.4** (80.6 calibrated, see below) | **1.12 s** | 11.3 s |
| snarf, run 1 | 66.8 | 1.04 s | 11.3 s |

| Category | Run 1 | Run 2 | context.dev |
|---|---|---|---|
| commerce | 75.3 | 99.7 | 99.3 |
| community_social | 72.7 | 73.0 | 84.0 |
| finance_filings | 0.0 | 76.0 | 99.7 |
| jobs | 81.0 | 82.7 | 84.7 |
| legal_regulatory | 68.0 | 68.0 | 73.7 |
| news | 80.0 | 80.0 | 92.0 |
| other | 68.3 | 68.7 | 76.0 |
| research | 65.7 | 66.7 | 84.0 |
| software_docs | 90.6 | 90.6 | 89.8 |

What changed between the runs:

- **finance_filings**: every URL is on sec.gov, which blocks clients that don't identify
  themselves. snarf now sends the declared User-Agent SEC's fair-access policy asks for
  (operator name and contact, from `SNARF_CONTACT` or `~/.config/snarf/contact`) and
  keeps to SEC's rate limit, only on hosts with such a policy.
- **commerce, jobs, research**: product, job and article details that a page renders with
  JavaScript are read from the schema.org JSON-LD and citation meta tags the page publishes.
- Run 2 re-fetched the 300 SEC pages, re-extracted every other page from the saved HTML with the
  new code, and re-judged the pages whose output could have changed (408); other verdicts carry
  over from run 1.

**Calibration.** On 61 short SEC 8-K filings our judges called *thin*, snarf returned about
the same text as the other providers (median 4.3k vs 4.7k characters) and TRACE's own judge
passed all four of them. Counting those as TRACE would gives **80.6**. The raw 78.4 is the
conservative figure.

**Caveats.** Single-run numbers; LLM judges disagree with each other by a point or two (one
batch passed every short 8-K, another failed 40% of them); some fixes were found by reading this
sample's failures, so a future TRACE sample is the real held-out test.
