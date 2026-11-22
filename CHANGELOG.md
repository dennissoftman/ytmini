# Changelog

Entries after 0.2.0 are written by
[release-please](https://github.com/googleapis/release-please) from
Conventional Commits. Earlier versions were summarised by hand.

## [0.3.0](https://github.com/dennissoftman/ytmini/compare/v0.2.0...v0.3.0) (2026-11-22)


### Features

* Home without signing in: suggestions for what you played, then popular videos for your region ([3ca56fe](https://github.com/dennissoftman/ytmini/commit/3ca56feacf925e78d6f2ac5353fcb723c1ce5749))
* Linux packages (.deb and tarball) ([d5c0a95](https://github.com/dennissoftman/ytmini/commit/d5c0a95f041366ab9c12949a16b8dfd136855580))
* sign in from more browsers ([b0828ff](https://github.com/dennissoftman/ytmini/commit/b0828ff6f5072af40e96c430bfe293e752d7f147))
* use ytmini without signing in ([f9f2b1b](https://github.com/dennissoftman/ytmini/commit/f9f2b1b0b1bd8350686a152610b9a2b2c735e1e1))


### Bug Fixes

* **deps:** bump the cargo group with 2 updates ([#3](https://github.com/dennissoftman/ytmini/issues/3)) ([966a794](https://github.com/dennissoftman/ytmini/commit/966a7944720bb8e04c96f7c82a34cf924535fb4b))
* Linux: the window opens under WSL ([c3ab54c](https://github.com/dennissoftman/ytmini/commit/c3ab54cd5842eb96084ad17326052f86152892bf))
* playing videos without signing in works again ([d49479c](https://github.com/dennissoftman/ytmini/commit/d49479c56a7e7733f2fc7ddc49824b0f0066267a))
* videos play on the first start even when the system yt-dlp is outdated ([86ede1e](https://github.com/dennissoftman/ytmini/commit/86ede1eb47f3046f5ad8876aa58b574cb941329a))

## 0.2.0 (2026-11-22)

First public release.

### Features

* macOS: releases are signed with a Developer ID and notarized by Apple, so
  they open without a Gatekeeper warning.
* Windows: `ytmini.exe` carries its name and version in its file properties.

### Bug Fixes

* Windows: links from descriptions and comments are opened without going
  through `cmd`, which could run a command hidden in a link after `&`.
* The session is only sent to plain `youtube.com` hosts; unusual URLs
  (userinfo, fragments) are refused.
* Logs redact URLs wherever they appear, not only at the start of a word.
* macOS: the app states the macOS version its bundled libraries need, so an
  older Mac refuses it cleanly instead of crashing at launch.

## 0.1.13 (2026-11-22)

### Bug Fixes

* An expired sign-in is refreshed from your browser as soon as any request
  notices it, including stream lookups during playback.
* Windows: opening links no longer flashes a console window.

## 0.1.12 (2026-11-22)

### Bug Fixes

* Windows: no more endless console windows at startup. The daily yt-dlp
  update check could restart itself in a loop.

## 0.1.11 (2026-11-21)

### Features

* `--check-native` and `scripts/check_native.sh`: check the fast stream
  lookup without playing anything; an expired sign-in is reported as such.

## 0.1.10 (2026-11-21)

### Bug Fixes

* The fast stream lookup works again on videos with auto-dubbed audio and
  plays the original audio track, as yt-dlp does.

## 0.1.9 (2026-11-18)

### Bug Fixes

* The "Resumed" notice sits under the title; the EQ pill fits its label.

## 0.1.8 (2026-11-18)

### Features

* ytmini's own dropdown menus; seek time shown under the pointer.

### Performance

* No video decoding while a song is collapsed to the music card.

## 0.1.7 (2026-11-18)

### Features

* Auto EQ for songs: a preset per genre, and your picks kept per artist.

## 0.1.6 (2026-11-18)

### Features

* The music bar is a compact floating card at the bottom centre.

## 0.1.5 (2026-11-18)

### Features

* Title over the video, a play/pause flash, and Home offers to continue the
  last video.

## 0.1.4 (2026-11-17)

### Features

* Autoplay keeps going while minimised or in the tray; the tray plays audio
  only.

## 0.1.3 (2026-11-17)

### Features

* Music: ♫ on songs, a Music tab, song-only suggestions, and Back keeps a song
  playing in a bar.

## 0.1.2 (2026-11-17)

### Features

* Keep the display awake while a video plays.

## 0.1.1 (2026-11-17)

### Features

* Windows: installer (per user, offers an update over an existing install),
  app icon, Windows wording throughout.

## 0.1.0 (2026-11-16)

First version: embedded libmpv player; Home, Subscriptions, Watch later,
History, playlists, channels and search through YouTube's web API; sign-in
from your browser's cookies; a fast native stream lookup with yt-dlp as the
fallback; queue and autoplay; chapters, seek-bar previews, SponsorBlock,
DeArrow, downloads, comments, likes and subscriptions; 13 interface languages.
