import { connect, sleep } from './cdp.mjs';

const port = Number(process.argv[2] || 9333);
const shots = process.argv[3];
const page = await connect(port);

let failed = 0;
const check = (name, ok, detail = '') => {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
};

const snapshot = `
  const row = document.getElementById('home-vibe-grid');
  const a = document.getElementById('native-audio');
  return {
    cards: row ? row.querySelectorAll('.my-wave-track-card').length : 0,
    focused: row ? row._focusedIndex : null,
    focusedTitle: document.getElementById('my-wave-focused-title')?.textContent || '',
    playing: document.getElementById('playbar-title')?.textContent || '',
    t: a.currentTime, paused: a.paused,
  };
`;
const waitFor = async (pred, ms = 30000) => {
  let s;
  for (let i = 0; i < ms / 500; i++) {
    s = await page.evaluate(snapshot);
    if (pred(s)) return s;
    await sleep(500);
  }
  return s;
};

await page.evaluate(`
  document.querySelector('.nav-item[data-view="discover"]')?.click();
  const input = document.getElementById('search-input');
  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
  input.blur();
`);
let s = await waitFor((x) => x.cards >= 3);
check('home carousel has covers', s.cards >= 3, `${s.cards} cards`);

await page.evaluate(`document.querySelectorAll('#home-vibe-grid .my-wave-track-card')[document.getElementById('home-vibe-grid')._focusedIndex || 0].click();`);
s = await waitFor((x) => !x.paused && x.t > 1 && x.playing === x.focusedTitle);
check('clicking the centred cover plays it', !s.paused && s.playing === s.focusedTitle, `playing "${s.playing}"`);

const before = s.playing;
await page.evaluate(`window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));`);
s = await waitFor((x) => x.playing !== before && x.playing === x.focusedTitle && !x.paused && x.t > 1);
if (shots) await page.screenshot(`${shots}/e2e-carousel-slide.png`);
check('sliding switches the playing track to the centred cover', s.playing !== before && s.playing === s.focusedTitle, `focused #${s.focused} "${s.focusedTitle}", playing "${s.playing}"`);

const prevFocused = s.focused;
await page.evaluate(`document.getElementById('btn-next').click();`);
s = await waitFor((x) => x.focused !== prevFocused && x.playing === x.focusedTitle);
check('Next moves the carousel to the new track', s.focused !== prevFocused && s.playing === s.focusedTitle, `focused #${s.focused} "${s.focusedTitle}", playing "${s.playing}"`);

page.close();
console.log(failed ? `\n${failed} check(s) failed` : '\nAll carousel checks passed');
process.exit(failed ? 1 : 0);
