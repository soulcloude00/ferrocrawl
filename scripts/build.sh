#!/usr/bin/env bash
# Build inside a memory-capped systemd scope: if the compiler runs out of memory, only the build
# dies, not the terminal it was started from. Usage: scripts/build.sh [-p crate] [cargo args…]
set -euo pipefail
cd "$(dirname "$0")/.."
cap="${FERROCRAWL_BUILD_MEM:-2500M}"
if command -v systemd-run >/dev/null; then
  exec systemd-run --user --scope --quiet --collect -p MemoryMax="$cap" -p MemorySwapMax=2G \
    nice -n 10 cargo build --release -j "${FERROCRAWL_BUILD_JOBS:-2}" "$@"
fi
exec nice -n 10 cargo build --release -j "${FERROCRAWL_BUILD_JOBS:-2}" "$@"
