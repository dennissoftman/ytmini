#!/bin/sh
# Cross-compile ytmini for 64-bit Windows from macOS and package it with
# libmpv. Needs: rustup target x86_64-pc-windows-msvc, cargo-xwin, LLVM
# (llvm-rc, for the exe icon), bsdtar (7z), gh, makensis (brew install makensis).
# (makensis crashes on macOS without a UTF-8 locale, hence LC_ALL.)
# Output:
#   dist/ytmini-setup.exe   per-user installer (installer/ytmini.nsi)
#   dist/ytmini-win64.zip   the same files, portable
# Both hold ytmini.exe (static C runtime), libmpv-2.dll (shinchiro's
# mpv-winbuild, baseline x86-64) and the license files.
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
cp target/$target/release/ytmini.exe $cache/libmpv-2.dll LICENSE.md NOTICE.md \
    licenses/GPL-3.0.txt $stage/
rm -f dist/ytmini-win64.zip dist/ytmini-setup.exe
(cd target/win-pkg && zip -qr -9 ../../dist/ytmini-win64.zip ytmini)

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
build=$(git rev-parse --short HEAD)$(git diff --quiet HEAD -- || echo +)
LC_ALL=en_US.UTF-8 makensis -V2 -INPUTCHARSET UTF8 \
    -DVERSION="$version" -DBUILD="$build" -DROOT="$PWD" \
    -DSTAGE="$PWD/$stage" -DOUTFILE="$PWD/dist/ytmini-setup.exe" \
    installer/ytmini.nsi
ls -la dist/ytmini-win64.zip dist/ytmini-setup.exe
