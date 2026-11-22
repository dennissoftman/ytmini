#!/usr/bin/env bash
# Builds the Linux packages (on Linux):
#   dist/ytmini-<version>-linux-x86_64.tar.gz  binary, launcher, icon, install.sh
#   dist/ytmini_<version>_amd64.deb            for Debian/Ubuntu (needs dpkg-deb)
# libmpv isn't bundled: it comes from the distribution (Depends: libmpv2 | libmpv1).
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname)" == "Linux" ]] || { echo "build_linux.sh runs on Linux"; exit 1; }

VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
cargo build --release
strip target/release/ytmini 2>/dev/null || true

rm -rf dist/linux && mkdir -p dist/linux
# 512 px icon for the hicolor theme (assets/icon.png is 1024 px)
icon=dist/linux/ytmini.png
if command -v convert >/dev/null; then
  convert assets/icon.png -resize 512x512 "$icon"
elif python3 -c "import PIL" 2>/dev/null; then
  python3 -c "from PIL import Image; Image.open('assets/icon.png').resize((512, 512), Image.LANCZOS).save('$icon')"
else
  cp assets/icon.png "$icon"
fi

# tarball
name="ytmini-${VERSION}-linux-${ARCH}"
stage="dist/linux/$name"
mkdir -p "$stage"
cp target/release/ytmini packaging/linux/ytmini.desktop packaging/linux/install.sh "$icon" \
  LICENSE.md NOTICE.md "$stage/"
tar -C dist/linux -czf "dist/$name.tar.gz" "$name"
echo "built dist/$name.tar.gz"

# .deb
if command -v dpkg-deb >/dev/null && [[ "$ARCH" == "x86_64" ]]; then
  root="dist/linux/deb"
  install -Dm755 target/release/ytmini "$root/usr/bin/ytmini"
  install -Dm644 packaging/linux/ytmini.desktop "$root/usr/share/applications/ytmini.desktop"
  install -Dm644 "$icon" "$root/usr/share/icons/hicolor/512x512/apps/ytmini.png"
  install -Dm644 LICENSE.md "$root/usr/share/doc/ytmini/copyright"
  mkdir -p "$root/DEBIAN"
  size=$(du -sk "$root/usr" | cut -f1)
  glibc=$(objdump -T target/release/ytmini | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)
  cat > "$root/DEBIAN/control" <<CONTROL
Package: ytmini
Version: ${VERSION}
Architecture: amd64
Maintainer: Denys Mitin <denisukmol2002@gmail.com>
Depends: libmpv2 | libmpv1, libc6 (>= ${glibc})
Section: video
Priority: optional
Installed-Size: ${size}
Homepage: https://github.com/dennissoftman/ytmini
Description: small, fast desktop YouTube client
 ytmini shows your YouTube feed, subscriptions, Watch later, history and
 playlists with your own account, and plays videos in an embedded mpv player.
 It signs in with the YouTube session from your browser (Firefox and its
 forks, Chrome, Chromium, Brave, Edge, Opera, Vivaldi), or works without
 signing in.
CONTROL
  dpkg-deb --root-owner-group --build "$root" "dist/ytmini_${VERSION}_amd64.deb" >/dev/null
  echo "built dist/ytmini_${VERSION}_amd64.deb"
fi
