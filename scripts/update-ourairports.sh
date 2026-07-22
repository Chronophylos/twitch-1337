#!/usr/bin/env bash
# Downloads OurAirports airports + runways CSVs into the flight bake assets.
# Used by `just data-refresh` and `.github/workflows/data-refresh.yml`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_DIR="$ROOT/crates/data/assets/flight/ourairports"
AIRPORTS_URL="https://davidmegginson.github.io/ourairports-data/airports.csv"
RUNWAYS_URL="https://davidmegginson.github.io/ourairports-data/runways.csv"

mkdir -p "$DEST_DIR"

download_atomic() {
  local url="$1"
  local dest="$2"
  local tmp="${dest}.tmp.$$"
  # Fail on HTTP errors; write to temp then rename so a bad download
  # cannot truncate a previously good committed file.
  curl -fsSL --retry 3 --retry-delay 2 -o "$tmp" "$url"
  mv "$tmp" "$dest"
}

download_atomic "$AIRPORTS_URL" "$DEST_DIR/airports.csv"
download_atomic "$RUNWAYS_URL" "$DEST_DIR/runways.csv"

echo "Updated $DEST_DIR/airports.csv ($(wc -l < "$DEST_DIR/airports.csv") lines)"
echo "Updated $DEST_DIR/runways.csv ($(wc -l < "$DEST_DIR/runways.csv") lines)"
