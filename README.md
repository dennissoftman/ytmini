<div align="center">

<img src="assets/icon.png" width="96" alt="ytmini icon">

# ytmini

A small, fast desktop YouTube client for macOS, Windows and Linux.
Your own account, no browser tab, no ads in the way.

[![CI](https://github.com/dennissoftman/ytmini/actions/workflows/ci.yml/badge.svg)](https://github.com/dennissoftman/ytmini/actions/workflows/ci.yml)
[![Solver check](https://github.com/dennissoftman/ytmini/actions/workflows/solver-check.yml/badge.svg)](https://github.com/dennissoftman/ytmini/actions/workflows/solver-check.yml)
[![Release](https://img.shields.io/github/v/release/dennissoftman/ytmini?sort=semver)](https://github.com/dennissoftman/ytmini/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/dennissoftman/ytmini/total)](https://github.com/dennissoftman/ytmini/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE.md)
![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)
![Rust](https://img.shields.io/badge/rust-stable-orange?logo=rust)

</div>

ytmini is a native app (Rust, [egui](https://github.com/emilk/egui) and
[libmpv](https://mpv.io)) that signs in with the YouTube session from your
browser and gives you your feed, subscriptions, Watch later, history and
playlists in a quick, quiet window, with mpv doing the playback.

> ytmini is an unofficial client. It is not affiliated with, endorsed by or
> connected to YouTube or Google. It uses YouTube's private web API, which can
> change at any time.

## Features

- **Your YouTube**: Home with category chips, Subscriptions, Watch later,
  History, playlists, channels and search, signed in as you.
- **Fast starts**: stream links are looked up directly (about 0.2 s when
  signed in; macOS and Linux for now) with [yt-dlp](https://github.com/yt-dlp/yt-dlp) as the fallback;
  the next video is prepared ahead and videos start preparing when you hover
  them.
- **Embedded mpv player**: hardware decoding, chapters, seek-bar previews,
  quality that follows battery and Low Power Mode, YouTube's keyboard
  shortcuts, fullscreen.
- **Up next**: a queue you can reorder, autoplay, and YouTube's related videos.
- **Music**: songs are recognised, get song-only suggestions, keep playing in a
  small card when you go back, and get an automatic EQ preset per genre.
- **Background play**: minimise and audio keeps going, autoplay included.
  Closing the window to the tray keeps songs playing and pauses videos.
- **Extras**: [SponsorBlock](https://sponsor.ajay.app) segment skipping,
  [DeArrow](https://dearrow.ajay.app) titles and thumbnails (experimental),
  downloads, comments (read-only), like / subscribe / save to playlist,
  continue where you left off.
- **13 interface languages**, with YouTube's language and region set
  separately.

## Install

Download the latest build from
[Releases](https://github.com/dennissoftman/ytmini/releases/latest).

| Platform | File | Notes |
|---|---|---|
| macOS 15+ (Apple Silicon) | `ytmini-<version>-macos-arm64.zip` | Unzip and move `ytmini.app` to Applications. Releases are signed with a Developer ID and notarized by Apple. |
| Windows 10/11 (x64) | `ytmini-<version>-setup.exe` | Per-user install, no admin rights. A portable zip is attached too. |
| Linux (x86_64, glibc 2.35+) | `ytmini_<version>_amd64.deb` or `ytmini-<version>-linux-x86_64.tar.gz` | Needs libmpv from your distribution: `sudo apt install --no-install-recommends ./ytmini_<version>_amd64.deb` pulls it in on Debian/Ubuntu (without the flag, apt also adds the distribution's yt-dlp, which ytmini doesn't use: it keeps its own up to date), `sudo dnf install mpv-libs` (Fedora), `sudo pacman -S mpv` (Arch). The tarball's `install.sh` installs for your user. No tray icon on Linux: closing the window quits. |

**First launch warnings.** If macOS says it can't check ytmini for malware
(a build that isn't notarized), open **System Settings → Privacy & Security**,
scroll down and click **Open Anyway**. Windows releases aren't signed yet (see
[Code signing policy](#code-signing-policy)), so SmartScreen may warn about an
unknown publisher: click **More info → Run anyway**.

On first launch ytmini downloads [yt-dlp](https://github.com/yt-dlp/yt-dlp)
and [deno](https://deno.com) from their official GitHub releases (checksums
verified; for yt-dlp also its signature, against yt-dlp's release key built
into ytmini) and keeps yt-dlp up to date.

### Signing in

ytmini reads the YouTube session from a browser you're already signed in to:

| Browser | macOS | Windows | Linux |
|---|---|---|---|
| Firefox, LibreWolf, Waterfox, Floorp, Zen | ✓ | ✓ | ✓ (also snap / Flatpak) |
| Chrome, Chromium, Brave, Edge, Opera, Vivaldi | ✓ (one Keychain prompt) | – (their cookies are locked to the browser) | ✓ (key from GNOME Keyring / KWallet) |
| Safari | ✓ (needs Full Disk Access) | | | Google blocks
sign-in inside embedded web views, and OAuth doesn't give access to the feed,
history or Watch later, so this is the only way to see your own YouTube.

You can also skip signing in: search, channels and playback work without an
account, and Home suggests videos related to what you played lately (from
ytmini's local history) plus popular videos for your region. Your own feed,
subscriptions, Watch later and history need an account.

Only cookies for `youtube.com` and Google sign-in are copied. They stay in
ytmini's own settings folder (readable only by you) and are sent nowhere but
YouTube. When Google retires the copy, ytmini refreshes it from the same
browser.

## How it works

| Job | How |
|---|---|
| Lists (feed, subscriptions, Watch later, history, playlists, search, channels) | YouTube's web API with your session ([`innertube.rs`](src/innertube.rs)); yt-dlp if a reply can't be read |
| Stream links | asked from YouTube's player API directly, with YouTube's JS challenges solved by yt-dlp's own solver in a deno process with no permissions ([`native.rs`](src/native.rs)); anything unexpected falls back to warm yt-dlp helpers ([`resolver.rs`](src/resolver.rs)) |
| Playback | libmpv's OpenGL render API inside the window ([`mpv.rs`](src/mpv.rs), [`player.rs`](src/player.rs)); a separate mpv window if libmpv can't load |
| Sign-in | your browser's cookie store, read directly ([`signin.rs`](src/signin.rs)) |
| Likes, subscriptions, Watch later edits, history | YouTube's web API |

libmpv is loaded at runtime, not linked, so `cargo build` works without mpv
installed and the app still starts if libmpv is missing.


### Where things live

| | macOS | Windows | Linux |
|---|---|---|---|
| Settings, session, logs, yt-dlp/deno | `~/Library/Application Support/ytmini/` | `%LOCALAPPDATA%\ytmini\` | `~/.config/ytmini/` |
| Cache (thumbnails, lists) | `~/Library/Caches/ytmini/` | `%LOCALAPPDATA%\ytmini\cache\` | `~/.cache/ytmini/` |

## Privacy

ytmini has no servers, accounts, analytics or telemetry. Everything it keeps
(settings, your session, resume points, logs) stays on your computer, and logs
are scrubbed of links and tokens.

It talks to:

| Service | When | What it receives |
|---|---|---|
| YouTube / Google | always | your session cookies and requests, as your browser would |
| GitHub | first launch, then a daily check | downloads of yt-dlp and deno; no user data |
| [SponsorBlock](https://sponsor.ajay.app) | on by default (Settings) | 4 characters of a hash of the video id, never the id |
| [Deezer](https://www.deezer.com) | Auto EQ for songs, on by default (Settings) | the artist and song title, to look up the genre ([privacy](https://www.deezer.com/legal/personal-datas)) |
| [DeArrow](https://dearrow.ajay.app) | experimental, off by default (Settings) | a hash prefix for titles, the video id for thumbnails |

Signed out, Home asks YouTube for suggestions for the videos you played
lately, which sends their ids without cookies, as opening them would.

None of the third parties gets cookies or anything that identifies you beyond
your IP address. Crash reports are only written to a local file; nothing is
sent anywhere.

## Code signing policy

Release packages are built by GitHub Actions from the tagged source
([`release.yml`](.github/workflows/release.yml)); nothing is built or uploaded
by hand.

- **macOS**: signed with the maintainer's Apple Developer ID and notarized by
  Apple.
- **Windows**: not signed yet. We're applying to the
  [SignPath Foundation](https://signpath.org) for free open-source code
  signing.

Team roles:

- Committers and reviewers: [@dennissoftman](https://github.com/dennissoftman)
- Approvers: [@dennissoftman](https://github.com/dennissoftman)

Outside contributions are merged only after review. Privacy: see
[Privacy](#privacy); ytmini sends no data except as listed there.

## Build from source

Needs [Rust](https://rustup.rs) (stable).

```bash
cargo build --release
```

**macOS app bundle** (needs [Homebrew](https://brew.sh) mpv on the build
machine; the result doesn't need Homebrew):

```bash
brew install mpv
./bundle.sh            # dist/ytmini.app
./bundle.sh --install  # and copy it to ~/Applications
```

`./run.sh` installs mpv if missing and runs a development build.

**Windows installer**, cross-compiled from macOS (needs `cargo-xwin`, LLVM,
`makensis`, `gh`):

```bash
./scripts/build_windows.sh   # dist/ytmini-<version>-setup.exe and a portable zip
```

**Linux packages**, built on Linux (needs a C compiler; `dpkg-deb` for the .deb):

```bash
./scripts/build_linux.sh     # dist/ytmini-<version>-linux-x86_64.tar.gz and ytmini_<version>_amd64.deb
```

## Contributing

Bug reports and pull requests are welcome. Please read
[CONTRIBUTING.md](CONTRIBUTING.md) first: it covers the privacy rules, how to
test without touching your own watch history, translations and the commit
style that drives releases.

## License

ytmini's own code is [MIT](LICENSE.md). The released app bundles mpv and
FFmpeg (GPL), so the macOS and Windows packages as a whole are distributed
under the GNU GPL v3; see [NOTICE.md](NOTICE.md).
