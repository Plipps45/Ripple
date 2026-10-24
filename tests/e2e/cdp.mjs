export async function connect(port, pick = (t) => t.type === 'page') {
  const targets = await fetch(`http://127.0.0.1:${port}/json`).then((r) => r.json());
  const target = targets.find(pick);
  if (!target) throw new Error('no page target');
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = reject;
  });
  let id = 0;
  const waiting = new Map();
  const listeners = [];
  ws.onmessage = (msg) => {
    const data = JSON.parse(msg.data);
    if (data.id && waiting.has(data.id)) {
      const { resolve, reject } = waiting.get(data.id);
      waiting.delete(data.id);
      if (data.error) reject(new Error(data.error.message));
      else resolve(data.result);
    } else if (data.method) {
      for (const l of listeners) l(data);
    }
  };
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const n = ++id;
      waiting.set(n, { resolve, reject });
      ws.send(JSON.stringify({ id: n, method, params }));
    });
  const evaluate = async (expression) => {
    const r = await send('Runtime.evaluate', { expression: `(async () => { ${expression} })()`, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
    return r.result.value;
  };
  const screenshot = async (file) => {
    const { data } = await send('Page.captureScreenshot', { format: 'png' });
    (await import('node:fs')).writeFileSync(file, Buffer.from(data, 'base64'));
  };
  return { send, evaluate, screenshot, on: (fn) => listeners.push(fn), close: () => ws.close() };
}

export async function quitApp(page) {
  await page.evaluate(`await window.electronAPI.setTrayOnClose(false); window.electronAPI.closeWindow();`).catch(() => {});
  page.close();
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
