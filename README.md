# ytmini

A minimal personal YouTube client: egui UI, yt-dlp for listing and sign-in
(browser cookies), mpv/libmpv for playback.

## Run

```bash
./run.sh                 # installs yt-dlp, mpv, deno via Homebrew if missing, then runs
./bundle.sh --install    # builds dist/ytmini.app and copies it to ~/Applications
ytmini --play <url|id>   # open a video straight into the player
```

`ytmini.app` is self-contained except for two downloads it manages itself:

| Component | Where it comes from |
|---|---|
| libmpv + ~50 dylibs (FFmpeg, libass, codecs…), `mpv` fallback binary | copied from Homebrew into the bundle by `bundle.sh` (`scripts/bundle_dylibs.py` relinks them to `@rpath`, so nothing points at `/opt/homebrew`) |
| yt-dlp | downloaded on first run from yt-dlp's GitHub release into `~/Library/Application Support/ytmini/bin/`, SHA-256 verified, update check daily (and right away when YouTube errors look like a stale yt-dlp) |
| deno (JS runtime yt-dlp needs for YouTube) | a system `deno` if present, otherwise downloaded and verified the same way |

Why the *unpacked* yt-dlp build (`yt-dlp_macos.zip`, 124 MB on disk) and not the
single-file one: the single-file build unpacks itself on every launch and macOS
re-scans those files each time — 13 s per call, measured, vs 0.26 s unpacked.

Building the bundle needs `brew install mpv` on the build machine; running it
doesn't need Homebrew at all.

**Licensing:** Homebrew's FFmpeg is built with `--enable-gpl --enable-version3`
and mpv is GPL-2.0+/LGPL-2.1+. Fine for personal use; if you distribute the
`.app`, the GPL applies (ship or offer the mpv/FFmpeg sources, and license
ytmini compatibly).

## Embedded player and libmpv

The player renders inside the window through libmpv's OpenGL render API
(`src/mpv.rs`, `src/player.rs`). **libmpv is loaded at runtime with `dlopen`,
not linked at build time**, so:

- no `build.rs` or linker paths — `cargo build` works without mpv installed;
- the app still starts if libmpv is missing or an mpv upgrade breaks it, and
  falls back to playing in a separate mpv window (also selectable in Settings).

Search order: `<app>/Contents/Frameworks/libmpv.2.dylib` (bundled), next to the
executable, `/opt/homebrew/lib` (Apple Silicon Homebrew), `/usr/local/lib`
(Intel Homebrew), `/opt/local/lib` (MacPorts), then the system loader path.
On Linux `libmpv.so.2`, on Windows `libmpv-2.dll` next to the exe or on PATH.
Requires libmpv client API 2.x (mpv ≥ 0.35).

Because a Finder-launched `.app` gets a bare PATH, the app adds the Homebrew
locations to its own PATH at startup so libmpv's ytdl hook finds yt-dlp and
yt-dlp finds deno.

## Where things live (macOS)

| What | Path |
|---|---|
| Cookies (chmod 600), settings, mpv log, managed yt-dlp/deno (`bin/`) | `~/Library/Application Support/ytmini/` |
| Disk cache (thumbnails, last tab results) | `~/Library/Caches/ytmini/` |

Cache defaults: 256 MB, 30 days, LRU — both adjustable in Settings.

## Tests

```bash
cargo test                                         # unit tests
cargo test -- --ignored --test-threads 1           # live: hits YouTube with your cookies, opens mpv
./target/release/ytmini --smoke-test aqz-KE-bpKQ   # embedded player: play, seek, back, replay,
                                                   # live quality switch, quit while playing
```
