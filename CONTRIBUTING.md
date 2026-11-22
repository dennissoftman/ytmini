# Contributing to ytmini

Thanks for helping. This file covers the rules every change follows, how
releases happen, and how to test without disturbing your own YouTube account.

## Ground rules

### Cookies and privacy

ytmini works with a real YouTube session, so these are non-negotiable:

- Never log, print or copy cookie contents anywhere, debug output included.
- Cookies stay in ytmini's config dir (`chmod 600` on Unix). Copies for a
  yt-dlp run go to `<config>/run/` and are deleted afterwards, never `/tmp`.
- The session goes to YouTube (and Google sign-in) only. Requests to third
  parties (SponsorBlock, DeArrow, Deezer for Auto EQ) carry no cookies.
- Logs hold no links or tokens: pass error text through
  `crashlog::redact` before writing it anywhere.
- Opening links: only `http(s)` URLs, never through a shell (`cmd /C start`
  would execute `&…` in a URL on Windows).

### Language and region

- Any language sent to YouTube (InnerTube `hl`, yt-dlp lang) comes from
  ytmini's own Language setting. Never hard-code it.
- Region (`gl`) is a separate setting with its own override. Don't conflate
  the two. `crate::i18n::youtube_locale()` is the single source for both.
- Parsers must not depend on English text from YouTube. The one exception is
  `remove_from_history`, which forces English (`Client.english = true`)
  because it finds a menu item by its English title.

### Interface strings

- Every UI string goes through `t()` / `tf()` and is translated into all
  languages in `src/i18n/*.rs`.
- Regenerate the key list after adding strings:
  `YTMINI_KEYS=$PWD/src/i18n/keys.json cargo test --release dump_keys -- --ignored`
- Platform wording must match the OS: "Finder" on macOS, "Explorer" on
  Windows.

### Windows

- Every child process gets `yt::hide_console`, or Windows opens a console
  window for it.
- `hide_console` doesn't help `cmd.exe` commands; avoid them.

## Commits and releases

Releases are automated with
[release-please](https://github.com/googleapis/release-please). Write commit
messages (or PR titles, when squash-merging) as
[Conventional Commits](https://www.conventionalcommits.org):

| Prefix | Use for | Version bump |
|---|---|---|
| `feat:` | something new a user can see | minor |
| `fix:` | a bug fix | patch |
| `perf:`, `revert:` | speed-ups, reverts | patch |
| `refactor:`, `docs:`, `ci:`, `chore:`, `test:` | everything else | none (not in the changelog) |
| `feat!:` / `BREAKING CHANGE:` | breaking change | major (minor while < 1.0) |

Describe the change from the user's point of view:
`fix: no more cmd windows at startup on Windows`.

On every push to `master`, release-please updates a release PR with the next
version and the `CHANGELOG.md` entry. Merging that PR tags the release, and the
release workflow builds and attaches:

- `ytmini-<version>-macos-arm64.zip` (`ytmini.app`, Apple Silicon)
- `ytmini-<version>-setup.exe` and `ytmini-<version>-win64.zip`
- `ytmini-<version>-linux-x86_64.tar.gz` and `ytmini_<version>_amd64.deb`

Don't bump `version` in `Cargo.toml` by hand.

### Signing

The release workflow signs and notarizes the macOS app when these repository
secrets exist (otherwise it ships an ad-hoc signed app):

| Secret | What |
|---|---|
| `MACOS_CERTIFICATE` | the *Developer ID Application* certificate with its private key, exported as `.p12`, base64 |
| `MACOS_CERTIFICATE_PASSWORD` | the `.p12` export password |
| `APPLE_API_KEY` | an App Store Connect API key (`AuthKey_….p8` contents, Developer access) for `notarytool` |
| `APPLE_API_KEY_ID`, `APPLE_API_ISSUER` | that key's ID and issuer ID |

`bundle.sh` signs with `CODESIGN_IDENTITY` when it's set: every bundled
library, the mpv helper and the app, with the hardened runtime and
[`assets/ytmini.entitlements`](assets/ytmini.entitlements). mpv's LuaJIT needs
executable memory; without those entitlements macOS kills the app as soon as
mpv loads a script.

## Building

```bash
cargo build --release
cargo test --release
cargo fmt --check && cargo clippy --release --all-targets -- -D warnings
```

- macOS bundle: `./bundle.sh` (needs `brew install mpv`).
- Windows installer from macOS: `./scripts/build_windows.sh`. It needs
  `cargo-xwin` (target `x86_64-pc-windows-msvc`, static CRT), LLVM (for
  `llvm-rc`), `makensis` and `gh`. libmpv comes from
  [shinchiro/mpv-winbuild-cmake](https://github.com/shinchiro/mpv-winbuild-cmake)
  and is cached in `target/win-libmpv`.
- Linux packages: `./scripts/build_linux.sh` on Linux (release builds run on
  Ubuntu 22.04, so the binary needs glibc 2.35+). libmpv isn't bundled; the
  .deb depends on `libmpv2 | libmpv1`. No tray on Linux (it would need GTK);
  closing the window quits. Chromium browsers there encrypt cookies with
  "peanuts" (v10) or the "<Browser> Safe Storage" password from the desktop
  keyring (v11), read through the Secret Service (`secret-service` crate) or
  `kwallet-query`.
- `build.rs` embeds `assets/icon.ico` with `llvm-rc`, using relative paths
  because llvm-rc reads `/Users/…` as an option.
- The NSIS installer (`installer/ytmini.nsi`, strings in
  `installer/strings.nsh`) installs per user to
  `%LOCALAPPDATA%\Programs\ytmini`, offers "Update" over an older version,
  and asks before deleting user data on uninstall. `makensis` needs
  `LC_ALL=en_US.UTF-8` on macOS.

## Testing without touching your own account

Live tests and dev runs use your real session. Your watch history drives your
recommendations, so keep test playback out of it:

- Launch with `YTMINI_NO_HISTORY=1` whenever a test plays anything. It also
  keeps resume points and the continue card in memory only (as
  `--smoke-test` does), so your own aren't overwritten.
- If a test video lands in your history anyway, remove it:
  `YTMINI_REMOVE=id1,id2 cargo test --release remove_history -- --ignored --nocapture`
- Use `YTMINI_CONFIG_DIR=<scratch dir>` for a separate, signed-out profile
  instead of editing your real settings while ytmini is running.
- Space out live tests. About 20 rapid yt-dlp resolves once tripped YouTube's
  "confirm you're not a bot" check and logged the session out. Prefer
  measuring inside the app (`YTMINI_DEBUG=1`), and never ship
  `player_skip=webpage`.

Live tests are `#[ignore]`d. Run them one at a time, for example
`cargo test --release -- --ignored deezer_genres --nocapture`.

**Test videos**: Big Buck Bunny `aqz-KE-bpKQ`, Sintel `eRsGyueVLvQ`,
Tears of Steel `R6MlUcmOul8`; a song for music features and Auto EQ: Rick
Astley `dQw4w9WgXcQ`.

### Checking the native stream lookup

`native.rs` asks YouTube's player API for stream links itself, and YouTube
changes that API often. Check it with:

```bash
./scripts/check_native.sh               # the installed app
./target/release/ytmini --check-native  # a dev build
./target/release/ytmini --check-native <id> ...
```

It looks up each test video with both clients (web_creator, tv_samsung),
solves the challenges and fetches one byte of each stream. Nothing plays, so
nothing reaches your watch history. Exit codes: 0 fine; 1 a lookup failed
(please open an issue with the output); 2 can't check here; 3 the saved
session expired (open ytmini, which refreshes it from your browser).

Audio tracks: `xtags` is a base64 protobuf of key/value tags. `acont`
(original, dubbed-auto, …) and `lang` mark language tracks; any other tag
(DRC, voice boost, unknown) marks an altered variant we skip. Like yt-dlp, the
original track wins over YouTube's default, which is often an auto-dub.

### Driving the app: `YTMINI_PLAY_SEQ`

A comma-separated script that starts a few seconds after launch:

| Step | Does |
|---|---|
| `<video id>` | play it |
| `back` | Back (collapses a song to the music card) |
| `expand` | reopen the player from the music card |
| `pause` | toggle pause |
| `seek:<s>` | seek to s seconds |
| `wait:<s>` | wait |
| `minimize` / `tray` | minimize / close to tray |
| `end` | jump to the end (tests autoplay) |
| `shot:<file.png>` | in-app screenshot (macOS `screencapture` gives black frames) |

```bash
YTMINI_NO_HISTORY=1 YTMINI_DEBUG=1 \
  YTMINI_PLAY_SEQ=dQw4w9WgXcQ,wait:5,back,wait:2,shot:/tmp/card.png \
  ./target/release/ytmini
```

`./target/release/ytmini --smoke-test aqz-KE-bpKQ` drives the embedded player
through play, seek, back, replay, a live quality switch and quitting while
playing.

### Environment variables

| Variable | Effect |
|---|---|
| `YTMINI_DEBUG=1` | timing and state lines on stderr |
| `YTMINI_NO_HISTORY=1` | don't report playback to your watch history; don't save resume points or the continue card |
| `YTMINI_MPV_LOG=<file>` | full mpv log (check the `af` chain for EQ) |
| `YTMINI_CONFIG_DIR=<dir>` | separate profile with its own cache (fresh install, signed out) |
| `YTMINI_LANG=ru` | UI language, for screenshots |
| `YTMINI_HL` / `YTMINI_GL` | YouTube language / region override |
| `YTMINI_THEME=light\|dark` | force the theme |
| `YTMINI_NO_NATIVE=1` | skip the native stream lookup |
| `YTMINI_X11=1` | Linux: use X11 instead of Wayland (automatic in WSL) |
| `YTMINI_IMPORT=1` | allow Keychain cookie import in sign-in tests |

## Design decisions

- Auto EQ gets the genre from Deezer. A preset picked by hand sticks to the
  artist. Presets only, no band sliders.
- Music is detected from YouTube's own music badge and the YouTube Music
  player's `musicVideoType`. Songs get song-only suggestions. Back collapses
  a song to a compact card; the video stops decoding and audio keeps playing.
- Closing to the tray keeps audio only. Autoplay works while minimized or in
  the tray.
- Home starts with the last stopped video, so it can be continued.
- The title is an overlay at the top of the player, as on YouTube.
- An expired session is refreshed from the browser it came from as soon as
  any request notices, without asking.
