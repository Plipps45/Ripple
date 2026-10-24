import { spawn, execSync } from 'node:child_process';
import { connect, sleep } from './cdp.mjs';

const port = Number(process.argv[2] || 9333);
const exe = process.argv[3];
const native = () => {
  const out = execSync('powershell -NoProfile -Command "$p = @(Get-Process ripple -ErrorAction SilentlyContinue); \'{0}|{1}\' -f $p.Count, $p[0].MainWindowTitle"').toString().trim();
  const [count, title] = out.split('|');
  return { count: Number(count), shown: title === 'Ripple' };
};
const relaunch = () => new Promise((resolve) => {
  const child = spawn(exe, [], { stdio: 'ignore', env: { ...process.env, RIPPLE_REMOTE_DEBUG_PORT: '' } });
  child.on('exit', resolve);
});

let ok = true;
const check = (name, cond, detail) => {
  ok = ok && cond;
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}  — ${detail}`);
};

await relaunch();
await sleep(1200);
const start = native();
check('window is shown before the test', start.shown && start.count === 1, JSON.stringify(start));

const page = await connect(port);
page.evaluate(`window.electronAPI.closeWindow();`).catch(() => {});
await sleep(1500);
page.close();
const closed = native();
check('close hides the window to the tray (process stays alive)', !closed.shown && closed.count === 1, JSON.stringify(closed));

const code = await relaunch();
await sleep(1200);
const back = native();
check('second launch exits and re-shows the running window', code === 0 && back.shown && back.count === 1, JSON.stringify({ exit: code, ...back }));
process.exit(ok ? 0 : 1);
