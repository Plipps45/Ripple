import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const perQuery = Number(process.argv[2] || 4);
const exe = process.env.RIPPLE_BIN || path.join(here, '..', '..', 'src-tauri', 'target', 'release', process.platform === 'win32' ? 'ripple.exe' : 'ripple');
const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), 'ripple-stress-'));

const queries = [
  'bad bunny titi me pregunto', 'karol g provenza', 'tame impala the less i know the better',
  'daft punk one more time', 'peso pluma ella baila sola', 'feid normal',
  'billie eilish birds of a feather', 'the weeknd blinding lights',
];
const platforms = process.env.STRESS_PLATFORMS ? process.env.STRESS_PLATFORMS.split(',') : ['youtube', 'ytmusic', 'soundcloud'];

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

async function readAudio(info) {
  if (info.isHls) {
    const list = await fetch(info.streamUrl).then((r) => r.text());
    const seg = list.split('\n').find((l) => l.startsWith('http'));
    if (!seg) return 'empty HLS playlist';
    const r = await fetch(seg);
    const n = (await r.arrayBuffer()).byteLength;
    return r.ok && n > 1000 ? null : `HLS segment ${r.status} (${n} B)`;
  }
  const r = await fetch(info.streamUrl, { headers: { Range: 'bytes=0-65535' } });
  const n = (await r.arrayBuffer()).byteLength;
  return (r.status === 200 || r.status === 206) && n > 1000 ? null : `audio ${r.status} (${n} B)`;
}

const totals = {};
for (const platform of platforms) {
  totals[platform] = { ok: 0, fail: 0, failures: [] };
  for (const q of queries) {
    const s = await api(`/api/search?q=${encodeURIComponent(q)}&platform=${platform}&limit=${perQuery}`).catch(() => null);
    const tracks = (s && s.tracks) || [];
    if (!tracks.length) {
      totals[platform].fail++;
      totals[platform].failures.push(`search "${q}": no results`);
      continue;
    }
    for (const t of tracks.slice(0, perQuery)) {
      const qs = new URLSearchParams({ url: t.url || '', id: t.id || '', platform: t.platform || platform, title: t.title || '', artist: t.artist || '', duration: String(t.duration || 0) });
      const started = Date.now();
      let err;
      try {
        const info = await api(`/api/stream-info?${qs}`);
        err = info.success ? await readAudio(info) : `stream-info: ${info.error}`;
      } catch (e) {
        err = String(e.message || e);
      }
      const ms = Date.now() - started;
      if (err) {
        totals[platform].fail++;
        totals[platform].failures.push(`${t.artist} - ${t.title} [${t.id}]: ${err}`);
      } else {
        totals[platform].ok++;
      }
      console.log(`${err ? 'FAIL' : ' ok '}  ${platform.padEnd(10)} ${String(ms).padStart(6)}ms  ${(t.artist + ' - ' + t.title).slice(0, 60)}${err ? '  -> ' + err : ''}`);
    }
  }
}

child.stdin.end();
child.kill();
try { fs.rmSync(sandbox, { recursive: true, force: true, maxRetries: 10, retryDelay: 500 }); } catch (e) {}
console.log('');
let anyFail = false;
for (const [p, t] of Object.entries(totals)) {
  console.log(`${p.padEnd(10)} ${t.ok}/${t.ok + t.fail} playable`);
  for (const f of t.failures) console.log('    ' + f);
  if (t.fail) anyFail = true;
}
process.exit(anyFail ? 1 : 0);
