#!/bin/bash
# Daily check: does ytmini's own stream lookup (the fast path that replaces
# yt-dlp) still work? Runs the installed app with --check-native: it looks up
# a few test videos the way playback does, plays nothing, and touches no
# watch history.
#
#   ./scripts/check_native.sh            # the default test videos
#   ./scripts/check_native.sh <id> ...   # specific videos
#
# Exit codes: 0 all fine; 1 a lookup failed: the report is copied to the
# clipboard, paste it into a bug report; 2 can't check (tools or sign-in missing);
# 3 the saved YouTube session expired: open ytmini (it refreshes the session
# from your browser) and run the check again.
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

# 3: the saved login expired (normal every few days); not a lookup bug
if [ $status -eq 3 ]; then
    exit 3
fi

if [ $status -eq 1 ]; then
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
