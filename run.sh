#!/usr/bin/env bash
# Installs what's missing (via Homebrew) and runs ytmini.
# Pass --reimport to pick a browser and grab fresh cookies again.
set -euo pipefail
cd "$(dirname "$0")"

command -v brew  >/dev/null || { echo "Install Homebrew first: https://brew.sh"; exit 1; }
command -v cargo >/dev/null || { echo "Install Rust first: https://rustup.rs"; exit 1; }

for p in yt-dlp mpv deno; do
  command -v "$p" >/dev/null || brew install "$p"
done

cargo run --release -- "$@"
