#!/bin/sh
# Build ytmini for 64-bit Windows and package it with libmpv: cross-compiled
# with cargo-xwin on macOS (local builds), natively on Windows in Git Bash
# (the release job). Needs: rustup target x86_64-pc-windows-msvc, LLVM
# (llvm-rc, for the exe icon), gh, jq, makensis, and on macOS cargo-xwin and
# bsdtar (brew install makensis); on Windows 7-Zip unpacks and zips instead.
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
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) windows=1 build=build ;;
    *) windows='' build="xwin build" ;;
esac
# makensis is a Windows program there: it needs C:\ paths, not /d/a/…
winpath() { if [ -n "$windows" ]; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
# shellcheck disable=SC2086 # "xwin build" is two words
RUSTFLAGS="-C target-feature=+crt-static -A linker-messages" cargo $build --release --target $target

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
    if command -v bsdtar >/dev/null; then
        bsdtar -xf $cache/mpv.7z -C $cache libmpv-2.dll
    else
        7z e -y -bso0 -o"$cache" $cache/mpv.7z libmpv-2.dll
    fi
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
if command -v zip >/dev/null; then
    (cd target/win-pkg && zip -qr -9 ../../dist/ytmini-win64.zip ytmini)
else
    (cd target/win-pkg && 7z a -tzip -mx=9 -bso0 ../../dist/ytmini-win64.zip ytmini)
fi

commit=$(git rev-parse --short HEAD)$(git diff --quiet HEAD -- || echo +)
LC_ALL=en_US.UTF-8 makensis -V2 -INPUTCHARSET UTF8 \
    -DVERSION="$version" -DBUILD="$commit" -DROOT="$(winpath "$PWD")" \
    -DSTAGE="$(winpath "$PWD/$stage")" -DOUTFILE="$(winpath "$PWD/$setup")" \
    "$(winpath installer/ytmini.nsi)"
ls -la dist/ytmini-win64.zip "$setup"
