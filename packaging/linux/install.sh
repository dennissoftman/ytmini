#!/bin/sh
# Installs ytmini for the current user (no root): the binary to
# ~/.local/bin, the launcher and the icon to ~/.local/share.
# ytmini needs libmpv from your distribution:
#   Debian/Ubuntu: sudo apt install libmpv2   (libmpv1 on older releases)
#   Fedora:        sudo dnf install mpv-libs
#   Arch:          sudo pacman -S mpv
# Uninstall: ./install.sh --uninstall
set -eu
cd "$(dirname "$0")"
bin="$HOME/.local/bin"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/512x512/apps"
if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$bin/ytmini" "$apps/ytmini.desktop" "$icons/ytmini.png"
  echo "ytmini removed. Your settings stay in ~/.config/ytmini (delete it to remove them too)."
  exit 0
fi
mkdir -p "$bin" "$apps" "$icons"
install -m 755 ytmini "$bin/ytmini"
install -m 644 ytmini.desktop "$apps/ytmini.desktop"
install -m 644 ytmini.png "$icons/ytmini.png"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" 2>/dev/null || true
echo "Installed to $bin/ytmini; ytmini is in your applications menu."
case ":$PATH:" in *":$bin:"*) ;; *) echo "Note: $bin isn't on your PATH; start ytmini from the menu, or add it.";; esac
if ! ldconfig -p 2>/dev/null | grep -q 'libmpv\.so'; then
  echo "libmpv isn't installed yet; see the top of this script for the package name."
fi
