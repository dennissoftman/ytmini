#!/bin/bash
# Daily check: does ytmini's own stream lookup (the fast path that replaces
# yt-dlp) still work? Runs the installed app with --check-native: it looks up
# a few test videos the way playback does, plays nothing, and touches no
# watch history.
#
#   ./scripts/check_native.sh            # the default test videos
#   ./scripts/check_native.sh <id> ...   # specific videos
#
# On failure, the report is copied to the clipboard: paste it into a bug report.
set -u

APP="${YTMINI_BIN:-$HOME/Applications/ytmini.app/Contents/MacOS/ytmini}"
LOGS="$HOME/Library/Application Support/ytmini/logs"

if [ ! -x "$APP" ]; then
    echo "ytmini isn't installed at $APP (set YTMINI_BIN to another binary)."
    exit 2
fi

out=$("$APP" --check-native "$@" 2>&1)
status=$?
echo "$out"

if [ $status -ne 0 ]; then
    {
        echo "ytmini native lookup check failed"
        echo
        echo "$out"
        if [ -f "$LOGS/native-lookup.log" ]; then
            echo
            echo "Recent fallbacks to yt-dlp (logs/native-lookup.log):"
            tail -n 10 "$LOGS/native-lookup.log"
        fi
    } | pbcopy
    echo
    echo "The report is on the clipboard: paste it into a bug report."
fi
exit $status
