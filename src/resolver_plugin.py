# ytmini's resolver: a yt-dlp plugin that keeps one yt-dlp running and answers
# stream lookups over stdin/stdout, one JSON line each way. ytmini writes this
# file into its private config folder and starts:
#   yt-dlp --plugin-dirs <dir> ... ytmini-serve:
# Request:  {"url": "https://www.youtube.com/watch?v=…", "format": "…"}
# Reply:    {"ok": true, "info": {…}}  or  {"ok": false, "error": "…"}
# Staying alive saves Python's start-up, the player JS download, the HTTPS
# connections and (below) the parsing of YouTube's player script per video.
import json
import os
import sys

from yt_dlp.cache import Cache
from yt_dlp.extractor.common import InfoExtractor

# Keep each player's preprocessed challenge-solver input in memory, so the JS
# challenge for the next video skips re-parsing the ~2.5 MB player script.
# yt-dlp has this cache but leaves it off, since on disk the files pile up.
try:
    from yt_dlp.extractor.youtube.jsc._builtin.ejs import EJSBaseJCP

    _SECTION = EJSBaseJCP._CACHE_SECTION
    _players = {}
    _load, _store = Cache.load, Cache.store

    def _mem_load(self, section, key, *a, **k):
        if section == _SECTION and str(key).startswith('player:'):
            return _players.get(key)
        return _load(self, section, key, *a, **k)

    def _mem_store(self, section, key, data, *a, **k):
        if section == _SECTION and str(key).startswith('player:'):
            if len(_players) >= 4:
                _players.clear()
            _players[key] = data
            return None
        return _store(self, section, key, data, *a, **k)

    Cache.load, Cache.store = _mem_load, _mem_store
    EJSBaseJCP._ENABLE_PREPROCESSED_PLAYER_CACHE = True
except Exception:  # a newer yt-dlp moved things: solve the slow way
    pass


def _pick(info):
    fmt = lambda f: {'url': f.get('url'), 'vcodec': f.get('vcodec')}
    return {
        'url': info.get('url'),
        'title': info.get('title'),
        'vcodec': info.get('vcodec'),
        'requested_formats': [fmt(f) for f in info.get('requested_formats') or []],
        'http_headers': {'User-Agent': (info.get('http_headers') or {}).get('User-Agent')},
    }


class YtminiServeIE(InfoExtractor):
    IE_NAME = 'ytmini:serve'
    _VALID_URL = r'ytmini-serve:$'

    def _real_extract(self, url):
        ydl = self._downloader
        out = os.fdopen(os.dup(1), 'w', buffering=1)
        os.dup2(2, 1)  # anything yt-dlp prints must not mix with our replies
        out.write('{"ready":true}\n')
        for line in sys.stdin:
            try:
                req = json.loads(line)
                ydl.format_selector = ydl.build_format_selector(req['format'])
                info = ydl.extract_info(req['url'], download=False)
                rep = {'ok': True, 'info': _pick(info)}
            except KeyboardInterrupt:
                raise
            except BaseException as e:
                rep = {'ok': False, 'error': str(e)[:500]}
            out.write(json.dumps(rep) + '\n')
        # ytmini closed the pipe: leave without writing the cookie jar back
        try:
            os.remove(ydl.params.get('cookiefile') or '')
        except OSError:
            pass
        os._exit(0)
