#!/bin/sh
# Cross-compile ytmini for 64-bit Windows from macOS and package it with
# libmpv. Needs: rustup target x86_64-pc-windows-msvc, cargo-xwin, LLVM
# (llvm-rc, for the exe icon), bsdtar (7z), gh, makensis (brew install makensis).
# (makensis crashes on macOS without a UTF-8 locale, hence LC_ALL.)
# Output:
#   dist/ytmini-<version>-setup.exe   per-user installer (installer/ytmini.nsi)
#   dist/ytmini-win64.zip   the same files, portable
# Both hold ytmini.exe (static C runtime), libmpv-2.dll (shinchiro's
# mpv-winbuild, baseline x86-64) and the license files.
# yt-dlp and deno download themselves on first launch, as on macOS.
set -eu
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-msvc
# -A linker-messages: lld-link lists every MSVC runtime object without its
# .pdb (LNK4099), harmless
RUSTFLAGS="-C target-feature=+crt-static -A linker-messages" cargo xwin build --release --target $target

cache=target/win-libmpv
# MPV_WINBUILD_TAG pins a release (e.g. 20251101); default: the latest.
# The release used is recorded in libmpv-source.txt (GPL source requests).
if [ ! -f $cache/libmpv-2.dll ] || [ ! -f $cache/libmpv-source.txt ]; then
    rel=${MPV_WINBUILD_TAG:+tags/$MPV_WINBUILD_TAG}
    json=$(gh api "repos/shinchiro/mpv-winbuild-cmake/releases/${rel:-latest}")
    tag=$(printf '%s' "$json" | jq -r .tag_name)
    url=$(printf '%s' "$json" | jq -r '.assets[] | select(.name|test("^mpv-dev-x86_64-[0-9]")) | .browser_download_url')
    rm -rf $cache && mkdir -p $cache
    curl -fsSL -o $cache/mpv.7z "$url"
    bsdtar -xf $cache/mpv.7z -C $cache libmpv-2.dll
    rm $cache/mpv.7z
    printf 'libmpv-2.dll: https://github.com/shinchiro/mpv-winbuild-cmake/releases/tag/%s\n(%s)\n' "$tag" "$(basename "$url")" > $cache/libmpv-source.txt
fi

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
setup=dist/ytmini-$version-setup.exe
stage=target/win-pkg/ytmini
rm -rf target/win-pkg && mkdir -p $stage dist
cp target/$target/release/ytmini.exe $cache/libmpv-2.dll $cache/libmpv-source.txt LICENSE.md NOTICE.md \
    licenses/GPL-3.0.txt $stage/
rm -f dist/ytmini-win64.zip dist/ytmini-*setup.exe
(cd target/win-pkg && zip -qr -9 ../../dist/ytmini-win64.zip ytmini)

build=$(git rev-parse --short HEAD)$(git diff --quiet HEAD -- || echo +)
LC_ALL=en_US.UTF-8 makensis -V2 -INPUTCHARSET UTF8 \
    -DVERSION="$version" -DBUILD="$build" -DROOT="$PWD" \
    -DSTAGE="$PWD/$stage" -DOUTFILE="$PWD/$setup" \
    installer/ytmini.nsi
ls -la dist/ytmini-win64.zip "$setup"
