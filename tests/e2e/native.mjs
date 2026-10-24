import { connect, sleep } from './cdp.mjs';

const port = Number(process.argv[2] || 9333);
let page = await connect(port);
await page.send('Runtime.enable');
await page.send('Log.enable');
const logs = [];
page.on((m) => {
  if (m.method === 'Log.entryAdded') logs.push(m.params.entry.text);
  if (m.method === 'Runtime.consoleAPICalled') logs.push(m.params.args.map((a) => a.value ?? a.description).join(' '));
});

const results = [];
const check = (name, ok, detail = '') => {
  results.push(ok);
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
};
const size = () => page.evaluate(`return { w: window.innerWidth, h: window.innerHeight };`);
const restoreWindow = async () => {
  await page.evaluate(`window.electronAPI.maximizeWindow();`);
  await sleep(700);
  await page.evaluate(`window.electronAPI.maximizeWindow();`);
  await sleep(700);
};
await restoreWindow();

const z0 = await page.evaluate(`return window.electronAPI.getZoomFactor();`);

const before = await size();
const mini = await page.evaluate(`return await window.electronAPI.toggleMiniWindow();`);
await sleep(600);
const small = await size();
const back = await page.evaluate(`return await window.electronAPI.toggleMiniWindow();`);
await sleep(600);
const restored = await size();
const miniLogical = { w: Math.round(small.w * z0), h: Math.round(small.h * z0) };
check('mini player toggles to 380x160 and back', mini === true && back === false && Math.abs(miniLogical.w - 380) <= 2 && Math.abs(miniLogical.h - 160) <= 2 && Math.abs(restored.w - before.w) < 4, `zoom ${z0}: ${before.w}x${before.h} → ${miniLogical.w}x${miniLogical.h} logical → ${restored.w}x${restored.h}`);

page.evaluate(`window.electronAPI.maximizeWindow();`);
await sleep(800);
const maxed = await size();
page.evaluate(`window.electronAPI.maximizeWindow();`);
await sleep(800);
const unmaxed = await size();
check('maximize / restore', maxed.w > unmaxed.w || maxed.h > unmaxed.h, `${unmaxed.w}x${unmaxed.h} ↔ ${maxed.w}x${maxed.h}`);

const zoom = await page.evaluate(`
  const w0 = window.innerWidth;
  window.electronAPI.setZoomFactor(1.25);
  await new Promise((r) => setTimeout(r, 500));
  const w1 = window.innerWidth;
  const f = window.electronAPI.getZoomFactor();
  window.electronAPI.setZoomFactor(${z0});
  await new Promise((r) => setTimeout(r, 500));
  return { w0, w1, f, w2: window.innerWidth };
`);
check('zoom factor applies to the webview', zoom.f === 1.25 && Math.abs(zoom.w1 - (zoom.w0 * z0) / 1.25) <= 2 && zoom.w2 === zoom.w0, JSON.stringify(zoom));

const visibility = await page.evaluate(`
  window.__visEvents = [];
  window.electronAPI.onWindowVisibility((v) => window.__visEvents.push(v));
  await new Promise((r) => setTimeout(r, 300));
  window.electronAPI.minimizeWindow();
  await new Promise((r) => setTimeout(r, 1200));
  return window.__visEvents.slice();
`);
check('minimize emits window-visibility=false', visibility.includes(false), JSON.stringify(visibility));
await restoreWindow();
const shown = await page.evaluate(`await new Promise((r) => setTimeout(r, 300)); return window.__visEvents.slice();`);
check('restore emits window-visibility=true', shown[shown.length - 1] === true, JSON.stringify(shown));

const corsWarnings = logs.filter((l) => /CORS|cross-origin|outputs zeroes/i.test(l || ''));
check('no CORS warnings from the audio graph', corsWarnings.length === 0, corsWarnings.slice(0, 2).join(' | '));

await page.send('Page.enable');
await page.send('Page.navigate', { url: 'http://tauri.localhost/welcome.html' });
await sleep(2500);
const onWelcome = await page.evaluate(`return { path: location.pathname, api: typeof window.electronAPI.welcomeFinish };`);
check('welcome page loads with the bridge', onWelcome.path === '/welcome.html' && onWelcome.api === 'function', JSON.stringify(onWelcome));
page.evaluate(`window.electronAPI.welcomeFinish({ skipped: false, theme: 'dark', platform: 'youtube' });`).catch(() => {});
await sleep(3500);
page.close();
page = await connect(port);
const after = await page.evaluate(`
  const got = await new Promise((resolve) => {
    window.electronAPI.onWelcomeResult((d) => resolve(d));
    setTimeout(() => resolve('none'), 1500);
  });
  return { path: location.pathname, got };
`);
check('welcome finish navigates to the player', after.path === '/index.html' || after.path === '/', after.path);
check('welcome result is delivered exactly once', after.got === 'none' || (after.got && after.got.theme === 'dark'), JSON.stringify(after.got));

page.close();
const failed = results.filter((r) => !r).length;
console.log(failed ? `\n${failed} check(s) failed` : '\nAll native checks passed');
process.exit(failed ? 1 : 0);
