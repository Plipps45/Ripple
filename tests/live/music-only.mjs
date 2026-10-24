import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const exe = process.env.RIPPLE_BIN || path.join(here, '..', '..', 'src-tauri', 'target', 'release', process.platform === 'win32' ? 'ripple.exe' : 'ripple');
const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), 'ripple-music-only-'));

const queries = ['daft punk get lucky', 'karol g provenza', 'the weeknd blinding lights', 'bad bunny titi me pregunto'];
const VIDEO_TITLE_RE = /[(\[][^)\]]*\b(v[ií]deos?|videoclips?|clips?|mv|m\/v|visuali[sz]ers?)\b[^)\]]*[)\]]|\b(official|music|lyrics?|letra|performance|4k|hd)\s+(v[ií]deo|mv|visuali[sz]er)\b|\bv[ií]deo\s*(oficial|clip)\b|\b(videoclip|m\/v)\b/i;

const binDir = path.join(process.env.LOCALAPPDATA || os.homedir(), 'com.weebec.ripple', 'bin');
const ext = process.platform === 'win32' ? '.exe' : '';
const env = {
  ...process.env,
  RIPPLE_DATA_DIR: sandbox,
  RIPPLE_YTDLP: process.env.RIPPLE_YTDLP || path.join(binDir, 'yt-dlp' + ext),
  RIPPLE_FFMPEG: process.env.RIPPLE_FFMPEG || path.join(binDir, 'ffmpeg' + ext),
};
const child = spawn(exe, ['--server-only'], { env, stdio: ['pipe', 'pipe', 'inherit'] });
const srv = await new Promise((resolve) => {
  let buf = '';
  child.stdout.on('data', (d) => {
    buf += d;
    const m = buf.match(/RIPPLE_SERVER (\d+) ([a-f0-9]+)/);
    if (m) resolve({ port: Number(m[1]), token: m[2] });
  });
});
const base = `http://127.0.0.1:${srv.port}`;
const api = (p) => fetch(base + p, { headers: { 'x-ripple-token': srv.token } }).then((r) => r.json());

const failures = [];
const videos = [];
for (const q of queries) {
  const plain = await api(`/api/search?q=${encodeURIComponent(q)}&platform=youtube&limit=10`);
  const music = await api(`/api/search?q=${encodeURIComponent(q)}&platform=youtube&limit=10&musicOnly=1`);
  const plainVideos = (plain.tracks || []).filter((t) => VIDEO_TITLE_RE.test(t.title));
  const leaked = (music.tracks || []).filter((t) => VIDEO_TITLE_RE.test(t.title));
  console.log(`${q.padEnd(28)} plain ${String((plain.tracks || []).length).padStart(2)} (${plainVideos.length} videos)   music only ${String((music.tracks || []).length).padStart(2)} (${leaked.length} videos)`);
  if (!(music.tracks || []).length) failures.push(`"${q}": music-only search returned nothing`);
  for (const t of leaked) failures.push(`"${q}": video kept by music-only search: ${t.title}`);
  if (plainVideos[0]) videos.push(plainVideos[0]);
}

console.log('');
let measured = 0;
for (const v of videos) {
  const info = await api(`/api/stream-info?${new URLSearchParams({ url: v.url || '', id: v.id, platform: 'youtube', title: v.title, artist: v.artist || '', duration: String(v.duration || 0) })}`);
  if (!info.success) console.log(`  (stream-info failed: ${info.error})`);
  const started = Date.now();
  const qs = new URLSearchParams({ id: v.id, title: v.title, artist: v.artist || '' });
  const r = await api(`/api/lyrics-offset?${qs}`);
  const ms = Date.now() - started;
  if (r.success) measured++;
  console.log(`${(v.artist + ' - ' + v.title).slice(0, 58).padEnd(58)} ${r.success ? `offset ${String(r.offset).padStart(6)} s  score ${r.score}  ${r.confident ? 'confident' : 'not trusted'}` : 'error: ' + r.error}  (${ms} ms)`);
}
if (videos.length && !measured) failures.push('no lyrics offset could be measured');

child.stdin.end();
child.kill();
try { fs.rmSync(sandbox, { recursive: true, force: true, maxRetries: 10, retryDelay: 500 }); } catch (e) {}
console.log('');
for (const f of failures) console.log('FAIL ' + f);
console.log(failures.length ? `${failures.length} failure(s)` : 'all checks passed');
process.exit(failures.length ? 1 : 0);
