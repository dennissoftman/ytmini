#!/usr/bin/env bash
# Installs what's missing (via Homebrew) and runs ytmini.
# Pass --reimport to pick a browser and grab fresh cookies again.
set -euo pipefail
cd "$(dirname "$0")"

command -v brew  >/dev/null || { echo "Install Homebrew first: https://brew.sh"; exit 1; }
command -v cargo >/dev/null || { echo "Install Rust first: https://rustup.rs"; exit 1; }

# mpv provides libmpv for development builds; yt-dlp/deno are fetched by the app itself.
command -v mpv >/dev/null || brew install mpv

cargo run --release -- "$@"
