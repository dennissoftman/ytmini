// ytmini's challenge solver host. ytmini prepends yt-dlp's bundled solver
// (yt-dlp/ejs: lib + core, defining `jsc`) and runs this with deno and no
// permissions at all (no files, no network): stdin in, stdout out.
// Request:  {"player_url": id, "player"?: code, "requests": [{type, challenges}]}
// Reply:    jsc's output, or {"type": "need_player"} when this process
//           hasn't seen that player yet.
// Each player's preprocessed form is kept, so later videos skip parsing
// YouTube's ~3 MB player script (~0.6 s → ~0.04 s).
const players = new Map();
const enc = new TextEncoder();
function reply(o) { Deno.stdout.writeSync(enc.encode(JSON.stringify(o) + "\n")); }
function handle(line) {
  const req = JSON.parse(line);
  let input;
  if (typeof req.player === "string") {
    input = { type: "player", player: req.player, requests: req.requests, output_preprocessed: true };
  } else if (players.has(req.player_url)) {
    input = { type: "preprocessed", preprocessed_player: players.get(req.player_url), requests: req.requests };
  } else {
    return reply({ type: "need_player" });
  }
  const out = jsc(input);
  if (out.preprocessed_player) {
    if (players.size >= 3) players.clear();
    players.set(req.player_url, out.preprocessed_player);
    delete out.preprocessed_player;
  }
  reply(out);
}
let buf = "";
const dec = new TextDecoder();
for await (const chunk of Deno.stdin.readable) {
  buf += dec.decode(chunk, { stream: true });
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    try { handle(line); } catch (e) { reply({ type: "error", error: String(e) }); }
  }
}
