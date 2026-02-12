#!/usr/bin/env bash
set -euo pipefail

if ! command -v npm >/dev/null 2>&1; then
  echo "[ui] npm is required but was not found in PATH" >&2
  exit 1
fi

echo "[ui] building gambi binary"
cargo build --bin gambi

echo "[ui] installing Node dependencies"
npm ci

echo "[ui] ensuring Playwright browser is installed"
npm run test:ui:install

GAMBI_BIN="${GAMBI_BIN:-$(pwd)/target/debug/gambi}"
echo "[ui] running Playwright suite with GAMBI_BIN=${GAMBI_BIN}"
GAMBI_BIN="${GAMBI_BIN}" npm run test:ui
