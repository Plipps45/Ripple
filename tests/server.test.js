'use strict';

const test = require('node:test');
const assert = require('node:assert');
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');

const exe = process.env.RIPPLE_BIN || path.join(__dirname, '..', 'src-tauri', 'target', 'debug', process.platform === 'win32' ? 'ripple.exe' : 'ripple');
const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), 'ripple-test-'));
const dataDir = path.join(sandbox, 'data');
const configHome = path.join(sandbox, 'config');
const downloads = path.join(sandbox, 'downloads');
const configFolder = path.join(configHome, process.platform === 'win32' ? 'com.weebec.ripple' : 'ripple');
fs.mkdirSync(configFolder, { recursive: true });
fs.mkdirSync(downloads, { recursive: true });
fs.writeFileSync(path.join(configFolder, 'config.json'), JSON.stringify({ downloadDir: downloads }));

let child;
let port;
let token;

test.before(async () => {
  child = spawn(exe, ['--server-only'], {
    env: { ...process.env, RIPPLE_DATA_DIR: dataDir, LOCALAPPDATA: configHome, XDG_CONFIG_HOME: configHome },
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  await new Promise((resolve, reject) => {
    let buf = '';
    child.stdout.on('data', (d) => {
      buf += d;
      const m = buf.match(/RIPPLE_SERVER (\d+) ([a-f0-9]+)/);
      if (m) {
        port = Number(m[1]);
        token = m[2];
        resolve();
      }
    });
    child.on('exit', (code) => reject(new Error(`server exited early (${code})`)));
    setTimeout(() => reject(new Error('server did not start')), 20000);
  });
});

test.after(() => {
  try { child.stdin.end(); } catch (e) {}
  try { child.kill(); } catch (e) {}
  try { fs.rmSync(sandbox, { recursive: true, force: true }); } catch (e) {}
});

function request(pathname, { method = 'GET', headers = {}, body = null, auth = false } = {}) {
  return new Promise((resolve, reject) => {
    const h = { ...headers };
    if (auth) h['x-ripple-token'] = token;
    if (body != null && !h['content-type']) h['content-type'] = 'application/json';
    const req = http.request({ host: '127.0.0.1', port, path: pathname, method, headers: h }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        const raw = Buffer.concat(chunks);
        let json = null;
        try { json = JSON.parse(raw.toString('utf8')); } catch (e) {}
        resolve({ status: res.statusCode, headers: res.headers, raw, text: raw.toString('utf8'), json });
      });
    });
    req.on('error', reject);
    if (body != null) req.write(typeof body === 'string' || Buffer.isBuffer(body) ? body : JSON.stringify(body));
    req.end();
  });
}

test('server responds and rejects untrusted routes', async () => {
  assert.strictEqual((await request('/')).status, 404);
  assert.strictEqual((await request('/api/tools-status')).status, 403);
  const ok = await request('/api/tools-status', { auth: true });
  assert.strictEqual(ok.status, 200);
  assert.strictEqual(typeof ok.json.ytDlp, 'boolean');
  assert.strictEqual(typeof ok.json.ffmpeg, 'boolean');
  assert.strictEqual(ok.json.progress.status, 'idle');
  assert.strictEqual((await request('/api/tools-status', { headers: { 'x-ripple-token': 'x'.repeat(48) } })).status, 403);
});

test('origin and host guards match the Electron server', async () => {
  assert.strictEqual((await request('/', { headers: { origin: 'https://evil.example.com' } })).status, 403);
  assert.strictEqual((await request('/api/tools-status', { auth: true, headers: { origin: 'http://tauri.localhost' } })).status, 200);
  assert.strictEqual((await request('/api/tools-status', { auth: true, headers: { origin: 'null' } })).status, 200);
  assert.strictEqual((await request('/', { headers: { host: 'evil.example.com' } })).status, 403);
  const pre = await request('/api/search', { method: 'OPTIONS' });
  assert.strictEqual(pre.status, 204);
  assert.strictEqual(pre.headers['access-control-allow-origin'], '*');
  assert.strictEqual(pre.headers['access-control-allow-methods'], 'GET, POST, OPTIONS');
});

test('Spotify login reports callback errors and protects account routes', async () => {
  for (const route of ['/api/spotify-auth-start?clientId=' + 'a'.repeat(32), '/api/spotify-status', '/api/spotify-library?likes=1', '/api/spotify-logout']) {
    assert.strictEqual((await request(route)).status, 403, route);
  }
  assert.strictEqual((await request('/api/spotify-auth-start?clientId=nope', { auth: true })).status, 400);
  const start = await request('/api/spotify-auth-start?clientId=' + 'a'.repeat(32), { auth: true });
  assert.strictEqual(start.status, 200);
  const authUrl = new URL(start.json.url);
  assert.strictEqual(authUrl.searchParams.get('redirect_uri'), start.json.redirectUri);
  assert.strictEqual(start.json.redirectUri, `http://127.0.0.1:${port}/api/spotify-callback`);
  assert.strictEqual(authUrl.searchParams.get('code_challenge_method'), 'S256');
  assert.match(authUrl.searchParams.get('code_challenge'), /^[A-Za-z0-9_-]{43}$/);
  const state = authUrl.searchParams.get('state');
  assert.strictEqual((await request('/api/spotify-callback?state=wrong&error=access_denied')).status, 400);
  const cb = await request(`/api/spotify-callback?state=${state}&error=access_denied`);
  assert.strictEqual(cb.status, 400);
  assert.match(cb.headers['content-type'], /text\/html/);
  const status = (await request('/api/spotify-status', { auth: true })).json;
  assert.strictEqual(status.connected, false);
  assert.match(status.error, /denied/i);
  assert.strictEqual((await request('/api/spotify-library?likes=1', { auth: true })).status, 401);
  assert.deepStrictEqual((await request('/api/spotify-logout', { auth: true })).json, { success: true });
});

test('local library: add, list, stream with Range, update, remove', async () => {
  const libDir = path.join(dataDir, 'library');
  fs.mkdirSync(libDir, { recursive: true });
  const bytes = Buffer.alloc(4096, 7);
  fs.writeFileSync(path.join(libDir, 'local_0123456789ab.mp3'), bytes);
  const add = await request('/api/library-add', {
    method: 'POST', auth: true,
    body: { items: [
      { id: 'local_0123456789ab', file: 'local_0123456789ab.mp3', ext: 'mp3', name: '01 - Artist Name - Song Title' },
      { id: 'evil', file: '../../etc/passwd', ext: 'mp3', name: 'x' },
    ] },
  });
  assert.deepStrictEqual(add.json, { success: true, count: 1 });
  const list = await request('/api/library', { auth: true });
  assert.strictEqual(list.json.length, 1);
  assert.deepStrictEqual(list.json[0], { id: 'local_0123456789ab', platform: 'local', title: 'Song Title', artist: 'Artist Name', duration: 0, thumbnail: '' });

  const full = await request('/api/library-file?id=local_0123456789ab');
  assert.strictEqual(full.status, 200);
  assert.strictEqual(full.headers['content-type'], 'audio/mpeg');
  assert.strictEqual(full.headers['accept-ranges'], 'bytes');
  assert.strictEqual(full.raw.length, 4096);
  const part = await request('/api/library-file?id=local_0123456789ab', { headers: { range: 'bytes=100-199' } });
  assert.strictEqual(part.status, 206);
  assert.strictEqual(part.headers['content-range'], 'bytes 100-199/4096');
  assert.strictEqual(part.raw.length, 100);
  assert.strictEqual((await request('/api/library-file?id=missing')).status, 404);

  await request('/api/library-update', { method: 'POST', auth: true, body: { id: 'local_0123456789ab', duration: 187.5 } });
  assert.strictEqual((await request('/api/library', { auth: true })).json[0].duration, 187.5);
  const saved = JSON.parse(fs.readFileSync(path.join(dataDir, 'library.json'), 'utf8'));
  assert.strictEqual(saved.local_0123456789ab.duration, 187.5);

  await request('/api/library-remove', { method: 'POST', auth: true, body: { id: 'local_0123456789ab' } });
  assert.deepStrictEqual((await request('/api/library', { auth: true })).json, []);
  assert.ok(!fs.existsSync(path.join(libDir, 'local_0123456789ab.mp3')));
});

test('offline cache: check-cache, local-track, offline-library, analysis, delete', async () => {
  const tracks = path.join(dataDir, 'tracks');
  fs.mkdirSync(tracks, { recursive: true });
  fs.writeFileSync(path.join(tracks, 'dQw4w9WgXcQ.opus'), Buffer.alloc(70000, 1));
  fs.writeFileSync(path.join(tracks, 'tiny.opus'), Buffer.alloc(10, 1));

  const cc = await request('/api/check-cache?id=dQw4w9WgXcQ', { auth: true });
  assert.deepStrictEqual(cc.json, { cached: true, streamUrl: `http://127.0.0.1:${port}/api/local-track?id=dQw4w9WgXcQ` });
  assert.deepStrictEqual((await request('/api/check-cache?id=tiny', { auth: true })).json, { cached: false, streamUrl: null });

  const lt = await request('/api/local-track?id=dQw4w9WgXcQ', { headers: { range: 'bytes=0-' } });
  assert.strictEqual(lt.status, 206);
  assert.strictEqual(lt.headers['content-type'], 'audio/ogg; codecs=opus');
  assert.strictEqual(lt.headers['content-range'], 'bytes 0-69999/70000');

  const si = await request('/api/stream-info?id=dQw4w9WgXcQ&platform=youtube', { auth: true });
  assert.deepStrictEqual(si.json, { success: true, isLocal: true, streamUrl: `http://127.0.0.1:${port}/api/local-track?id=dQw4w9WgXcQ` });

  await request('/api/track-analysis', { method: 'POST', auth: true, body: { id: 'dQw4w9WgXcQ', analysis: { bpm: 113 } } });
  assert.deepStrictEqual((await request('/api/track-analysis?id=dQw4w9WgXcQ', { auth: true })).json, { success: true, analysis: { bpm: 113 } });

  const off = (await request('/api/offline-library', { auth: true })).json;
  assert.strictEqual(off.success, true);
  const entry = off.tracks.find((t) => t.cleanId === 'dQw4w9WgXcQ');
  assert.ok(entry, 'cached track listed');
  assert.strictEqual(entry.fileSize, 70000);
  assert.deepStrictEqual(entry.analysis, { bpm: 113 });

  await request('/api/offline-delete?id=dQw4w9WgXcQ', { method: 'POST', auth: true });
  assert.ok(!fs.existsSync(path.join(tracks, 'dQw4w9WgXcQ.opus')));
  assert.deepStrictEqual((await request('/api/clear-offline-library', { method: 'POST', auth: true })).json, { success: true });
});

test('downloads folder tracks are found by stream-info and served safely', async () => {
  fs.writeFileSync(path.join(downloads, 'Some Artist - Some Song.mp3'), Buffer.alloc(2048, 3));
  const si = await request('/api/stream-info?title=Some%20Song&artist=Some%20Artist&platform=youtube', { auth: true });
  assert.strictEqual(si.json.isDownload, true);
  const served = await request(new URL(si.json.streamUrl).pathname + new URL(si.json.streamUrl).search);
  assert.strictEqual(served.status, 200);
  assert.strictEqual(served.raw.length, 2048);
  assert.strictEqual((await request('/api/downloaded-track?name=..%2F..%2Fsecret.mp3')).status, 404);
  assert.strictEqual((await request('/api/downloaded-track?name=notes.txt')).status, 404);
  assert.strictEqual((await request('/api/tmp-file?name=..%5Cevil.wav')).status, 400);
  assert.strictEqual((await request('/api/tmp-file?name=missing.wav')).status, 404);
  const dl = await request('/api/download', { auth: true });
  assert.deepStrictEqual(dl.json, { success: false, error: 'missing track information' });
});

test('custom lyrics round-trip and offline lookup', async () => {
  const q = 'title=My%20Song&artist=My%20Artist';
  assert.deepStrictEqual((await request(`/api/lyrics?${q}&online=0`, { auth: true })).json, { success: true, lyrics: '' });
  const save = await request(`/api/save-custom-lyrics?${q}`, { method: 'POST', auth: true, body: { lyrics: '[00:01.00] hola' } });
  assert.deepStrictEqual(save.json, { success: true });
  assert.strictEqual((await request(`/api/save-custom-lyrics?${q}`, { method: 'POST', auth: true, body: { nope: 1 } })).status, 400);
  const got = await request(`/api/lyrics?${q}&online=0`, { auth: true });
  assert.deepStrictEqual(got.json, { success: true, lyrics: '[00:01.00] hola', isCustom: true });
  const files = fs.readdirSync(path.join(dataDir, 'custom-lyrics'));
  assert.ok(files.some((f) => /^[a-f0-9]{16}\.lrc$/.test(f)), 'stored under sha1 key');
});

test('state broadcasts to the lyrics SSE stream', async () => {
  const events = [];
  const sse = await new Promise((resolve) => {
    const req = http.get({ host: '127.0.0.1', port, path: '/api/lyrics-stream' }, (res) => {
      res.setEncoding('utf8');
      res.on('data', (d) => events.push(d));
      resolve({ req, res });
    });
  });
  assert.strictEqual(sse.res.headers['content-type'], 'text/event-stream');
  await request('/api/state', { method: 'POST', auth: true, body: { title: 'T', artist: 'A', isPlaying: true, word: 'hey', wordIndex: 1, lineIndex: 2 } });
  await new Promise((r) => setTimeout(r, 300));
  sse.req.destroy();
  const joined = events.join('');
  assert.match(joined, /^data: \{\}\n\n/);
  assert.match(joined, /data: \{"title":"T","artist":"A","isPlaying":true,"word":"hey","wordIndex":1,"lineIndex":2\}\n\n/);
  assert.strictEqual((await request('/api/state', { method: 'POST', auth: true, body: 'not json' })).status, 400);
});

test('audio proxy refuses local and private targets', async () => {
  assert.strictEqual((await request('/api/audio-proxy')).status, 400);
  assert.strictEqual((await request('/api/audio-proxy?url=ftp%3A%2F%2Fx')).status, 400);
  for (const target of ['http://127.0.0.1:1/x', 'http://localhost/x', 'http://2130706433/x', 'http://[::1]/x', 'http://10.0.0.5/x', 'http://192.168.1.1/a']) {
    assert.strictEqual((await request('/api/audio-proxy?url=' + encodeURIComponent(target))).status, 403, target);
  }
});

test('body limits and empty payloads', async () => {
  const big = 'x'.repeat(2 * 1024 * 1024 + 10);
  assert.strictEqual((await request('/api/state', { method: 'POST', auth: true, body: big })).status, 413);
  assert.deepStrictEqual((await request('/api/translate', { method: 'POST', auth: true, body: { lines: [] } })).json, { success: true, translations: [] });
  assert.deepStrictEqual((await request('/api/albums/search?q=', {})).json, { success: true, albums: [] });
  assert.strictEqual((await request('/api/albums/tracks?id=abc')).status, 400);
  assert.strictEqual((await request('/api/artist', { auth: true })).status, 400);
  assert.strictEqual((await request('/api/playlist-import?url=https%3A%2F%2Fexample.com%2Fx')).status, 400);
  assert.strictEqual((await request('/api/apple-import?url=nope')).status, 400);
  assert.strictEqual((await request('/api/spotify-import?url=nope', { auth: true })).status, 400);
});
