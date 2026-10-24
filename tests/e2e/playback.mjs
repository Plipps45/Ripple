import { connect, sleep } from './cdp.mjs';

const port = Number(process.argv[2] || 9333);
const shots = process.argv[3];
const query = process.argv[4] || 'daft punk get lucky';
const platform = process.argv[5] || 'youtube';
const expectProxy = process.argv.includes('--expect-proxy');
const page = await connect(port);
await page.send('Runtime.enable');
const errors = [];
page.on((m) => {
  if (m.method === 'Runtime.exceptionThrown') errors.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
  if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') errors.push(m.params.args.map((a) => a.value ?? a.description).join(' '));
});

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
};

const bridge = await page.evaluate(`
  const api = window.electronAPI;
  return {
    members: Object.keys(api).length,
    tokenLen: api.serverToken.length,
    isWindows: api.isWindows,
    port: await api.getServerPort(),
    version: await api.getAppVersion(),
    dataDir: (await api.getDataDir()).dir,
    downloadDir: (await api.getDownloadDir()).dir,
    gpu: await api.getGpuCompositing(),
    multi: await api.getMultiInstance(),
    discordEnabled: await api.discordRpcGetEnabled(),
    themes: (await api.listThemes()).length,
    plugins: (await api.listPlugins()).map((p) => p.file),
    css: (await api.getAppCss()).length,
    cache: await api.getCacheSize(),
    audioOut: await api.getAudioOutputInfo(),
    diag: (await api.getDiagnostics()).split('\\n').slice(0, 6),
    dragRegion: getComputedStyle(document.querySelector('.titlebar-drag')).getPropertyValue('-webkit-app-region') || getComputedStyle(document.querySelector('.titlebar-drag')).webkitAppRegion || '(unsupported)',
  };
`);
check('electronAPI exposes its 68 members', bridge.members === 68, `${bridge.members} members`);
check('bridge commands answer', bridge.port > 0 && /^\d+\.\d+\.\d+$/.test(bridge.version) && bridge.gpu === 'enabled', `port ${bridge.port}, v${bridge.version}`);
check('themes / plugins / css', bridge.plugins.includes('example-hello.js') && bridge.css > 100000, `plugins ${bridge.plugins.join(',')}, css ${bridge.css} chars`);
check('cache size + audio output + diagnostics', typeof bridge.cache.audio === 'number' && bridge.audioOut.name === 'Default output' && bridge.diag[0].startsWith('Ripple'), bridge.diag.slice(0, 3).join(' | '));
check('drag region style readable', bridge.dragRegion === 'drag', bridge.dragRegion);

await page.evaluate(`
  document.querySelector('.platform-btn[data-platform="${platform}"]').click();
  await new Promise((r) => setTimeout(r, 300));
  document.getElementById('search-tracks-list').innerHTML = '';
  const input = document.getElementById('search-input');
  input.value = ${JSON.stringify(query)};
  input.dispatchEvent(new Event('input', { bubbles: true }));
  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
`);
let rows = 0;
for (let i = 0; i < 40 && rows === 0; i++) {
  await sleep(500);
  rows = await page.evaluate(`return document.querySelectorAll('#search-tracks-list .track-row-title').length;`);
}
check('search renders results', rows > 0, `${rows} rows`);
if (shots) await page.screenshot(`${shots}/e2e-search.png`);

await page.evaluate(`document.querySelector('#search-tracks-list .track-row-info').click();`);
let audio = null;
for (let i = 0; i < 60; i++) {
  await sleep(500);
  audio = await page.evaluate(`
    const a = document.getElementById('native-audio');
    const title = document.querySelector('#player-title, .player-title, .now-playing-title');
    return { src: a.currentSrc, t: a.currentTime, paused: a.paused, ready: a.readyState, err: a.error && a.error.code, dur: a.duration };
  `);
  if (audio.t > 3 && !audio.paused) break;
}
check('audio element plays a resolved stream', audio && audio.t > 3 && !audio.paused, JSON.stringify({ src: (audio.src || '').slice(0, 70), t: audio.t?.toFixed?.(1), dur: audio.dur, err: audio.err }));
const allowed = expectProxy ? 'audio-proxy' : 'audio-proxy|local-track|downloaded-track';
const viaLocalApi = new RegExp(`^http://127\\.0\\.0\\.1:\\d+/api/(${allowed})`).test(audio.src || '') || (audio.src || '').startsWith('blob:');
check(`stream served through the Rust API${expectProxy ? ' proxy' : ''}`, viaLocalApi, (audio.src || '').split('?')[0]);
if (shots) await page.screenshot(`${shots}/e2e-playing.png`);

const level = await page.evaluate(`
  const eng = window.audioEngine;
  const rmsDb = (node) => {
    const buf = new Float32Array(node.fftSize);
    node.getFloatTimeDomainData(buf);
    let sum = 0;
    for (const v of buf) sum += v * v;
    const rms = Math.sqrt(sum / buf.length);
    return rms > 0 ? 20 * Math.log10(rms) : -120;
  };
  let master = -120;
  for (let i = 0; i < 20; i++) {
    master = Math.max(master, rmsDb(eng.analyserNode));
    await new Promise((r) => setTimeout(r, 100));
  }
  eng.setEqBands([6, 4, 2, 0, 0, 0, 0, 0, 2, 4]);
  eng.applySettings(document.getElementById('native-audio'));
  let eq = -120;
  for (let i = 0; i < 20; i++) {
    eq = Math.max(eq, eng.getLevelDb());
    await new Promise((r) => setTimeout(r, 100));
  }
  eng.setEqBands([0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
  eng.applySettings(document.getElementById('native-audio'));
  return { master, eq, ctx: eng.ctx && eng.ctx.state };
`);
check('Web Audio master chain carries signal', level.master > -60 && level.ctx === 'running', `peak ${level.master.toFixed(1)} dBFS, context ${level.ctx}`);
check('10-band EQ path carries signal when enabled', level.eq > -60, `peak ${level.eq.toFixed(1)} dBFS`);

const t1 = await page.evaluate(`
  const a = document.getElementById('native-audio');
  document.getElementById('btn-play-pause')?.click();
  await new Promise((r) => setTimeout(r, 800));
  const pausedAfterClick = a.paused;
  document.getElementById('btn-play-pause')?.click();
  await new Promise((r) => setTimeout(r, 800));
  a.currentTime = 60;
  await new Promise((r) => setTimeout(r, 2500));
  return { pausedAfterClick, playingAgain: !a.paused, t: a.currentTime };
`);
check('pause / resume via player button', t1.pausedAfterClick === true && t1.playingAgain === true, JSON.stringify(t1));
check('seek within the stream (HTTP Range)', t1.t >= 60 && t1.t < 70, `t=${t1.t.toFixed(1)}`);

await sleep(2000);
const lyrics = await page.evaluate(`
  const port = await window.electronAPI.getServerPort();
  const title = document.querySelector('.player-track-title, #player-track-title, #current-title')?.textContent || '';
  const r = await fetch('http://127.0.0.1:' + port + '/api/lyrics?title=Get%20Lucky&artist=Daft%20Punk');
  const j = await r.json();
  return { ok: j.success, len: (j.lyrics || '').length };
`);
check('lyrics endpoint reachable from the webview', lyrics.ok && lyrics.len > 100, `${lyrics.len} chars`);

const relevantErrors = errors.filter((e) => !/favicon|ResizeObserver/i.test(e));
check('no page errors during the run', relevantErrors.length === 0, relevantErrors.slice(0, 3).join(' || '));

page.close();
const failed = results.filter((r) => !r.ok).length;
console.log(failed ? `\n${failed} check(s) failed` : '\nAll e2e checks passed');
process.exit(failed ? 1 : 0);
