#!/usr/bin/env bash
# Builds dist/ytmini.app (macOS). Pass --install to copy it to ~/Applications.
# Runtime deps (yt-dlp, mpv, deno) are NOT bundled; the app finds them in
# Homebrew's paths even when launched from Finder.
set -euo pipefail
cd "$(dirname "$0")"

[[ "$(uname)" == "Darwin" ]] || { echo "bundle.sh builds a macOS .app; on other systems use: cargo build --release"; exit 1; }

VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
APP=dist/ytmini.app

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/ytmini "$APP/Contents/MacOS/ytmini"

# icon: assets/icon.png -> ytmini.icns
ICONSET=$(mktemp -d)/ytmini.iconset
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) assets/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/ytmini.icns"
rm -rf "$(dirname "$ICONSET")"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>               <string>ytmini</string>
  <key>CFBundleDisplayName</key>        <string>ytmini</string>
  <key>CFBundleIdentifier</key>         <string>local.ytmini</string>
  <key>CFBundleExecutable</key>         <string>ytmini</string>
  <key>CFBundleIconFile</key>           <string>ytmini</string>
  <key>CFBundlePackageType</key>        <string>APPL</string>
  <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
  <key>CFBundleVersion</key>            <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>     <string>11.0</string>
  <key>LSApplicationCategoryType</key>  <string>public.app-category.video</string>
  <key>NSHighResolutionCapable</key>    <true/>
  <key>NSPrincipalClass</key>           <string>NSApplication</string>
</dict>
</plist>
PLIST

# Ad-hoc signature so Gatekeeper/TCC treat it as one consistent app.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || echo "warning: codesign failed (app still runs locally)"

echo "built $APP"

if [[ "${1:-}" == "--install" ]]; then
  mkdir -p "$HOME/Applications"
  rm -rf "$HOME/Applications/ytmini.app"
  cp -R "$APP" "$HOME/Applications/"
  echo "installed to ~/Applications/ytmini.app"
fi
