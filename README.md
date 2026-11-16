# ytmini

A minimal personal YouTube client: egui UI, yt-dlp for listing and sign-in
(browser cookies), mpv/libmpv for playback.

## Run

```bash
./run.sh                 # installs yt-dlp, mpv, deno via Homebrew if missing, then runs
./bundle.sh --install    # builds dist/ytmini.app and copies it to ~/Applications
ytmini --play <url|id>   # open a video straight into the player
```

Runtime dependencies (not bundled): `yt-dlp`, `mpv` (which provides libmpv), and
`deno` (yt-dlp needs a JS runtime for YouTube). Keep yt-dlp current —
"signature"/"JS challenge" errors usually mean it's stale: `brew upgrade yt-dlp`.

## Embedded player and libmpv

The player renders inside the window through libmpv's OpenGL render API
(`src/mpv.rs`, `src/player.rs`). **libmpv is loaded at runtime with `dlopen`,
not linked at build time**, so:

- no `build.rs` or linker paths — `cargo build` works without mpv installed;
- the app still starts if libmpv is missing or an mpv upgrade breaks it, and
  falls back to playing in a separate mpv window (also selectable in Settings).

Search order: `<app>/Contents/Frameworks/libmpv.2.dylib`, next to the
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
| Cookies (chmod 600), settings, mpv log | `~/Library/Application Support/ytmini/` |
| Disk cache (thumbnails, last tab results) | `~/Library/Caches/ytmini/` |

Cache defaults: 256 MB, 30 days, LRU — both adjustable in Settings.

## Tests

```bash
cargo test                                         # unit tests
cargo test -- --ignored --test-threads 1           # live: hits YouTube with your cookies, opens mpv
./target/release/ytmini --smoke-test aqz-KE-bpKQ   # embedded player: play, seek, back, replay,
                                                   # live quality switch, quit while playing
```
