import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const exe = process.argv[2] || path.join(import.meta.dirname, '..', '..', 'src-tauri', 'target', 'debug', 'ripple.exe');
const bin = path.join(process.env.LOCALAPPDATA, 'com.weebec.ripple', 'bin');
const sb = fs.mkdtempSync(path.join(os.tmpdir(), 'ripple-dl-'));
const downloads = path.join(sb, 'downloads');
const cfgDir = path.join(sb, 'cfg', 'com.weebec.ripple');
fs.mkdirSync(cfgDir, { recursive: true });
fs.mkdirSync(downloads);
fs.writeFileSync(path.join(cfgDir, 'config.json'), JSON.stringify({ downloadDir: downloads }));

const child = spawn(exe, ['--server-only'], {
  env: { ...process.env, RIPPLE_DATA_DIR: path.join(sb, 'data'), LOCALAPPDATA: path.join(sb, 'cfg'), RIPPLE_YTDLP: path.join(bin, 'yt-dlp.exe'), RIPPLE_FFMPEG: path.join(bin, 'ffmpeg.exe') },
  stdio: ['pipe', 'pipe', 'inherit'],
});
const { port, token } = await new Promise((resolve) => {
  let buf = '';
  child.stdout.on('data', (d) => {
    buf += d;
    const m = buf.match(/RIPPLE_SERVER (\d+) ([a-f0-9]+)/);
    if (m) resolve({ port: Number(m[1]), token: m[2] });
  });
});
const api = (p, init = {}) => fetch(`http://127.0.0.1:${port}${p}`, { ...init, headers: { 'x-ripple-token': token, ...(init.headers || {}) } });
const probe = (file) => execFileSync(path.join(bin, 'ffprobe.exe'), ['-v', 'error', '-show_entries', 'format=duration,format_name:format_tags=title,artist:stream=codec_type', '-of', 'json', file]).toString();
let ok = true;
const check = (name, cond, detail) => {
  ok = ok && cond;
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}  — ${detail}`);
};

const q = 'url=' + encodeURIComponent('https://www.youtube.com/watch?v=5NV6Rdv1a3I') + '&id=5NV6Rdv1a3I&title=Get%20Lucky&artist=Daft%20Punk&format=mp3&quality=5';
let t = Date.now();
const mp3 = await api('/api/download?' + q, { headers: {} }).then((r) => r.json());
const mp3Path = path.join(downloads, 'Daft Punk - Get Lucky.mp3');
const info = fs.existsSync(mp3Path) ? JSON.parse(probe(mp3Path)) : null;
check('MP3 download', mp3.success && info && info.format.format_name === 'mp3', `${((Date.now() - t) / 1000).toFixed(1)}s ${JSON.stringify(mp3)}`);
check('MP3 has tags and cover art', info && info.format.tags && /lucky/i.test(info.format.tags.title || '') && info.streams.some((s) => s.codec_type === 'video'), JSON.stringify(info && info.format.tags));
check('no leftover temp files', fs.readdirSync(downloads).filter((f) => !f.startsWith('.')).length === 1, fs.readdirSync(downloads).join(', '));

t = Date.now();
const raw = await api('/api/download?' + q + '&raw=1&start=30&end=40').then((r) => r.json());
const wav = raw.tempName ? await api('/api/tmp-file?name=' + encodeURIComponent(raw.tempName)) : null;
const wavBytes = wav ? Buffer.from(await wav.arrayBuffer()) : Buffer.alloc(0);
check('trimmed raw WAV via /api/tmp-file', raw.success && wav.status === 200 && wavBytes.subarray(0, 4).toString() === 'RIFF', `${((Date.now() - t) / 1000).toFixed(1)}s ${wavBytes.length} bytes`);
const wavFile = path.join(sb, 'clip.wav');
fs.writeFileSync(wavFile, wavBytes);
const wavDur = Number(JSON.parse(probe(wavFile)).format.duration);
check('trim honours start/end (≈10 s)', wavDur > 9 && wavDur < 11.5, `${wavDur.toFixed(2)} s`);

const enc = await api(`/api/encode?name=${encodeURIComponent(raw.tempName)}&title=Clip&artist=Ripple&format=flac`, { method: 'POST', body: wavBytes, headers: { 'content-type': 'audio/wav' } }).then((r) => r.json());
const flac = path.join(downloads, 'Ripple - Clip.flac');
check('encode WAV → FLAC', enc.success && fs.existsSync(flac) && JSON.parse(probe(flac)).format.format_name === 'flac', JSON.stringify(enc));
check('encode cleans the source temp file', !fs.existsSync(path.join(downloads, '.tmp', raw.tempName)), raw.tempName);

const si = await api('/api/stream-info?title=Get%20Lucky&artist=Daft%20Punk&platform=youtube').then((r) => r.json());
check('stream-info prefers the downloaded file', si.isDownload === true, si.streamUrl);

child.stdin.end();
child.kill();
fs.rmSync(sb, { recursive: true, force: true });
process.exit(ok ? 0 : 1);
