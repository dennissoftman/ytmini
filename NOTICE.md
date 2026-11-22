# ytmini

ytmini's own source code (this repository) is licensed under the MIT License;
see [LICENSE.md](LICENSE.md).

## macOS: ytmini.app

The macOS app bundle built by `bundle.sh` additionally contains third-party
software copied from Homebrew, under its own licenses:

- **mpv / libmpv**: GPL-2.0-or-later (parts LGPL-2.1-or-later)
- **FFmpeg**: GPL-3.0-or-later (built with `--enable-gpl --enable-version3`)
- **libass, libplacebo, codec and support libraries**: various (see below)

Because FFmpeg is included as GPL-3.0, the ytmini.app bundle *as a whole* is
distributed under the terms of the GNU GPL version 3 (`licenses/GPL-3.0.txt`).
The MIT License of ytmini's own code is compatible with this and still applies
to that code on its own.

Inside the bundle, `Contents/Resources/licenses/` holds:

- `LICENSE.md` (ytmini, MIT) and this `NOTICE.md`
- one folder per bundled Homebrew package with that package's license files
- `THIRD_PARTY.txt`: every bundled package and the exact version, so the
  corresponding source can be obtained

## Windows: ytmini-<version>-setup.exe / ytmini-<version>-win64.zip

The Windows build ships `libmpv-2.dll` from
[shinchiro/mpv-winbuild-cmake](https://github.com/shinchiro/mpv-winbuild-cmake)
(mpv with FFmpeg and its codec libraries linked in), under the GPL-3.0; the
Windows package as a whole is likewise distributed under the GNU GPL version 3
(`GPL-3.0.txt`, installed next to `ytmini.exe`). `libmpv-source.txt`, next to
it, names the mpv-winbuild-cmake release the DLL came from.

## Source

Corresponding source for the GPL/LGPL components is available from their
upstream projects at the versions listed in `THIRD_PARTY.txt` or the
mpv-winbuild-cmake release tag (for example
<https://github.com/mpv-player/mpv> and <https://ffmpeg.org/releases/>), and from
the maintainer of a given ytmini build on request for at least three years
after that build was distributed.

## Downloaded at first run

Not bundled, downloaded from their official releases:

- **yt-dlp**: The Unlicense (<https://github.com/yt-dlp/yt-dlp>)
- **deno**: MIT (<https://github.com/denoland/deno>), only if no system deno exists
