#!/bin/sh
# Cross-compile ytmini for 64-bit Windows from macOS and zip it with libmpv.
# Needs: rustup target x86_64-pc-windows-msvc, cargo-xwin, LLVM (llvm-rc, for
# the exe icon), bsdtar (7z), gh. Output: dist/ytmini-win64.zip
#   ytmini/ytmini.exe       static C runtime, no installer
#   ytmini/libmpv-2.dll     shinchiro's mpv-winbuild (baseline x86-64)
# yt-dlp and deno download themselves on first launch, as on macOS.
set -eu
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-msvc
RUSTFLAGS="-C target-feature=+crt-static" cargo xwin build --release --target $target

cache=target/win-libmpv
if [ ! -f $cache/libmpv-2.dll ]; then
    url=$(gh api repos/shinchiro/mpv-winbuild-cmake/releases/latest \
        --jq '.assets[] | select(.name|test("^mpv-dev-x86_64-[0-9]")) | .browser_download_url')
    rm -rf $cache && mkdir -p $cache
    curl -fsSL -o $cache/mpv.7z "$url"
    bsdtar -xf $cache/mpv.7z -C $cache libmpv-2.dll
    rm $cache/mpv.7z
fi

stage=target/win-pkg/ytmini
rm -rf target/win-pkg && mkdir -p $stage dist
cp target/$target/release/ytmini.exe $cache/libmpv-2.dll LICENSE NOTICE $stage/
rm -f dist/ytmini-win64.zip
(cd target/win-pkg && zip -qr -9 ../../dist/ytmini-win64.zip ytmini)
ls -la dist/ytmini-win64.zip
