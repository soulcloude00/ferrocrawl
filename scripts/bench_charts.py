#!/usr/bin/env python3
"""Draws the benchmark charts in benchmarks/img/ from the saved runs' summary.json files.

    python3 scripts/bench_charts.py        # needs rsvg-convert for the PNGs

Each chart is written for a light and a dark page (README picks one with <picture>).
Form: emphasis - snarf in the accent blue, every other provider in gray. Scores are dot
plots (close values, so the axis doesn't start at zero); latencies are bars from zero. Two
measures are two panels, never two y-axes. Only snarf's values are labeled; the tables in
benchmarks/RESULTS.md carry every number.
"""

import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "benchmarks"
OUT = BENCH / "img"

# Validated pair (dataviz validator: CVD ΔE 15.9 light / 19.2 dark, contrast >= 3:1 both).
THEMES = {
    "light": dict(surface="#fcfcfb", ink="#0b0b0b", ink2="#52514e", muted="#898781", grid="#e1e0d9",
                  axis="#c3c2b7", accent="#2a78d6", other="#898781"),
    "dark": dict(surface="#1a1a19", ink="#f0efec", ink2="#c3c2b7", muted="#898781", grid="#2c2c2a",
                 axis="#383835", accent="#3987e5", other="#6b6a65"),
}
FONT = "Noto Sans, Inter, Helvetica, Arial, sans-serif"
W = 1200


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def text(x, y, s, size, fill, weight=400, anchor="start", tnum=False):
    feat = ' font-variant-numeric="tabular-nums"' if tnum else ""
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-family="{FONT}" font-size="{size}" font-weight="{weight}" '
            f'fill="{fill}" text-anchor="{anchor}"{feat}>{esc(s)}</text>')


def bar(x, y, w, h, fill, r=4):
    """A horizontal bar anchored at x, rounded only at its data end."""
    w = max(w, 0.5)
    r = min(r, w, h / 2)
    return (f'<path d="M{x:.1f},{y:.1f} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} v{h - 2 * r:.1f} '
            f'a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} z" fill="{fill}"/>')


def nice_ticks(lo, hi, n=5):
    span = hi - lo
    step = [s for s in (0.25, 0.5, 1, 2, 2.5, 5, 10, 20, 25, 50, 100) if span / s <= n][0] if span > 0 else 1
    t = (lo // step) * step
    out = []
    while t <= hi + 1e-9:
        if t >= lo - 1e-9:
            out.append(t)
        t += step
    return out


def leaderboard(rows, title, subtitle, score_label, score_lo, score_hi, lat_key, lat_label, note, theme):
    """rows: [(name, ours, score, latency_seconds)], best score first."""
    c = THEMES[theme]
    row_h, top, left = 38, 138, 250
    panel_w, gap = 420, 70
    h = top + row_h * len(rows) + 92
    p1, p2 = left, left + panel_w + gap
    lat_hi = max(r[3] for r in rows) * 1.08
    sx = lambda v: p1 + (v - score_lo) / (score_hi - score_lo) * panel_w
    lx = lambda v: p2 + v / lat_hi * panel_w
    body = [f'<rect width="{W}" height="{h}" fill="{c["surface"]}"/>',
            text(32, 46, title, 24, c["ink"], 650),
            text(32, 76, subtitle, 15, c["ink2"])]
    y0, y1 = top - 12, top + row_h * len(rows) - 8
    # Panel headers, gridlines, ticks.
    body.append(text(p1, top - 24, score_label, 13, c["ink2"], 600))
    body.append(text(p2, top - 24, lat_label, 13, c["ink2"], 600))
    for t in nice_ticks(score_lo, score_hi):
        x = sx(t)
        body.append(f'<line x1="{x:.1f}" y1="{y0}" x2="{x:.1f}" y2="{y1}" stroke="{c["grid"]}" stroke-width="1"/>')
        body.append(text(x, y1 + 20, f"{t:g}", 12, c["muted"], anchor="middle", tnum=True))
    for t in nice_ticks(0, lat_hi):
        x = lx(t)
        body.append(f'<line x1="{x:.1f}" y1="{y0}" x2="{x:.1f}" y2="{y1}" stroke="{c["grid"]}" stroke-width="1"/>')
        body.append(text(x, y1 + 20, f"{t:g} s", 12, c["muted"], anchor="middle", tnum=True))
    body.append(f'<line x1="{p2}" y1="{y0}" x2="{p2}" y2="{y1}" stroke="{c["axis"]}" stroke-width="1"/>')
    for i, (name, ours, score, lat) in enumerate(rows):
        cy = top + i * row_h + row_h / 2 - 8
        col = c["accent"] if ours else c["other"]
        body.append(text(left - 16, cy + 5, name, 14, c["ink"] if ours else c["ink2"], 650 if ours else 400, "end"))
        # Score: a dot on a faint rail from the axis start.
        x = sx(score)
        body.append(f'<line x1="{p1}" y1="{cy}" x2="{x:.1f}" y2="{cy}" stroke="{col}" stroke-opacity="0.35" stroke-width="2"/>')
        body.append(f'<circle cx="{x:.1f}" cy="{cy}" r="{7 if ours else 5.5}" fill="{col}" stroke="{c["surface"]}" stroke-width="2"/>')
        # Latency: a bar from zero.
        body.append(bar(p2, cy - 8, lx(lat) - p2, 16, col))
        if ours:
            body.append(text(x + 14, cy + 5, f"{score:.1f}", 14, c["ink"], 650, tnum=True))
            body.append(text(lx(lat) + 10, cy + 5, f"{lat:.2f} s", 14, c["ink"], 650, tnum=True))
    body.append(text(32, h - 24, note, 12, c["muted"]))
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{h}" viewBox="0 0 {W} {h}">{"".join(body)}</svg>'


def before_after(metrics, title, subtitle, note, theme):
    """metrics: [(label, before_s, after_s)]"""
    c = THEMES[theme]
    row_h, top, left, pw = 64, 140, 150, 820
    h = top + row_h * len(metrics) + 80
    hi = max(max(b, a) for _, b, a in metrics) * 1.12
    x = lambda v: left + v / hi * pw
    body = [f'<rect width="{W}" height="{h}" fill="{c["surface"]}"/>',
            text(32, 46, title, 24, c["ink"], 650),
            text(32, 76, subtitle, 15, c["ink2"])]
    # Legend (two series): swatch + label, identity never by color alone.
    lx = left
    for lab, col in (("Before (2026-11-11)", c["other"]), ("Now (2026-11-12)", c["accent"])):
        body.append(bar(lx, top - 44, 14, 14, col, 3))
        body.append(text(lx + 22, top - 32, lab, 13, c["ink2"]))
        lx += 200
    y1 = top + row_h * len(metrics) - 6
    for t in nice_ticks(0, hi):
        gx = x(t)
        body.append(f'<line x1="{gx:.1f}" y1="{top - 10}" x2="{gx:.1f}" y2="{y1}" stroke="{c["grid"]}" stroke-width="1"/>')
        body.append(text(gx, y1 + 20, f"{t:g} s", 12, c["muted"], anchor="middle", tnum=True))
    body.append(f'<line x1="{left}" y1="{top - 10}" x2="{left}" y2="{y1}" stroke="{c["axis"]}" stroke-width="1"/>')
    for i, (lab, b, a) in enumerate(metrics):
        y = top + i * row_h
        body.append(text(left - 16, y + 24, lab, 14, c["ink"], 600, "end"))
        body.append(bar(left, y, x(b) - left, 18, c["other"]))  # 2px surface gap between the pair
        body.append(bar(left, y + 20, x(a) - left, 18, c["accent"]))
        pct = (a - b) / b * 100
        body.append(text(x(a) + 10, y + 34, f"{a:.2f} s  ({pct:+.0f}%)".replace("-", "−"), 14, c["ink"], 650, tnum=True))
    body.append(text(32, h - 24, note, 12, c["muted"]))
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{h}" viewBox="0 0 {W} {h}">{"".join(body)}</svg>'


def load(run):
    return json.loads((BENCH / run / "summary.json").read_text())


def board_rows(summary, ours_name="snarf"):
    rows = [(b["name"], False, b["score"], b["p50_ms"] / 1000) for b in summary["board"]]
    rows.append((ours_name, True, summary["score"], summary["latency"]["p50"] / 1000))
    return sorted(rows, key=lambda r: -r[2])


def write(name, svg_by_theme):
    OUT.mkdir(parents=True, exist_ok=True)
    for theme, svg in svg_by_theme.items():
        svg_path = OUT / f"{name}-{theme}.svg"
        svg_path.write_text(svg)
        png = OUT / f"{name}-{theme}.png"
        subprocess.run(["rsvg-convert", "-z", "2", "-o", str(png), str(svg_path)], check=True)
        svg_path.unlink()
        print("wrote", png.relative_to(ROOT))


def main():
    wca = load("webcrawlerapi-2026-11-12")
    trace = load("trace-run2")
    base = load("webcrawlerapi-2026-11-11-baseline")

    rows = [r for r in board_rows(wca) if r[2] >= 80]  # Crawlbase (74.9) would stretch the axis
    write("webcrawlerapi", {t: leaderboard(
        rows, "WebCrawlerAPI benchmark",
        f"Firecrawl's scrape-content dataset, {wca['ok']} pages returned · higher score and lower latency are better",
        "Coverage of returned pages (%)", 86, 96, "p50", "Median latency (p50)",
        "Ours: 2026-11-12, 16 concurrent, home connection. Others: published results. Crawlbase (74.9%, 9.8 s) not shown.",
        t) for t in THEMES})

    write("trace", {t: leaderboard(
        board_rows(trace), "TRACE benchmark",
        "context.dev's 2,527 fresh pages in nine categories, LLM-judged · higher score and lower latency are better",
        "Useful-content score", 75, 90, "p50", "Median latency (p50)",
        "Ours: run 2, 2026-11-10 (before the speed work). Others: TRACE's published page-level data.",
        t) for t in THEMES})

    b, n = base["latency"], wca["latency"]
    bm, nm = json.loads((BENCH / "webcrawlerapi-2026-11-11-baseline/meta.json").read_text()), json.loads((BENCH / "webcrawlerapi-2026-11-12/meta.json").read_text())
    metrics = [(k.upper() if k != "mean" else "Mean", b[k] / 1000, n[k] / 1000) for k in ("p50", "p90", "p95", "p99", "mean")]
    write("speed", {t: before_after(
        metrics, "Latency, before and after the speed work",
        f"WebCrawlerAPI, same 1,000 URLs, 16 concurrent · coverage {base['score']:.1f}% → {wca['score']:.1f}% · "
        f"whole run {bm['elapsed_ms'] / 1000:.0f} s → {nm['elapsed_ms'] / 1000:.0f} s",
        "Each bar is one run from one home connection; network speed varies between runs by up to ~0.3 s at p50.",
        t) for t in THEMES})


if __name__ == "__main__":
    main()
