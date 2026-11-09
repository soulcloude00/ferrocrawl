#!/usr/bin/env bash
# Start the desktop app in its own memory-capped scope (Chromium gets a separate one, see
# crates/core/src/render.rs). Extra env such as FERROCRAWL_BENCH=webcrawlerapi passes through.
set -euo pipefail
cd "$(dirname "$0")/.."
bin=target/release/ferrocrawl-app
if command -v systemd-run >/dev/null; then
  systemctl --user reset-failed ferrocrawl-app.scope 2>/dev/null || true
  exec systemd-run --user --scope --quiet --collect --unit=ferrocrawl-app \
    -p MemoryMax="${FERROCRAWL_APP_MEM:-2G}" -p MemorySwapMax=2G "$bin" "$@"
fi
exec "$bin" "$@"
