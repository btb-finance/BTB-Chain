// Shared helpers for the examples' Playwright smoke scripts (optional; not part of `cargo test`).
//   cargo build -p btb-cli && node examples/<app>/web/smoke.js
// Needs the `playwright` package with a Chromium. Runs `btb dev` in the example directory on a
// free port (so the contract is built from source and the page is the one `btb dev` serves).
const { spawn } = require('child_process');
const path = require('path');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '..');

async function startDev(app) {
  const port = 20000 + Math.floor(Math.random() * 20000);
  const bin = process.env.BTB_BIN || path.join(root, 'target/debug/btb');
  const srv = spawn(bin, ['dev', '--port', String(port)], {
    cwd: path.join(root, 'examples', app), stdio: ['ignore', 'inherit', 'inherit'],
  });
  const exited = new Promise((_, rej) => srv.on('exit', c => rej(new Error('btb dev exited early: ' + c))));
  const url = `http://127.0.0.1:${port}/`;
  const up = (async () => {
    for (let i = 0; i < 600; i++) {   // the first build can take a while
      try { if ((await fetch(url)).ok) return; } catch { /* not up yet */ }
      await new Promise(r => setTimeout(r, 250));
    }
    throw new Error('btb dev did not start');
  })();
  await Promise.race([up, exited]);
  return { url, stop: () => srv.kill() };
}

/** Run `body(browser, url, newPage)` against a fresh `btb dev`; exit 1 on failure. */
async function run(app, body) {
  let dev, browser;
  try {
    dev = await startDev(app);
    browser = await chromium.launch();
    const errors = [];
    const newPage = async (who, hash = '') => {
      const p = await browser.newPage();
      p.on('pageerror', e => errors.push(e.message));
      await p.goto(dev.url + hash);
      await p.waitForFunction(() => document.body.dataset.who, null, { timeout: 8000 });
      await p.fill('#who', who);
      await p.dispatchEvent('#who', 'change');   // reconnects only when the name really changed
      await p.waitForFunction(w => document.body.dataset.who === w, who, { timeout: 8000 });
      return p;
    };
    await body(newPage, dev.url);
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
    console.log(`PASS ${app}`);
  } catch (e) {
    console.error('FAIL', app, e);
    process.exitCode = 1;
  } finally {
    if (browser) await browser.close();
    if (dev) dev.stop();
  }
}

const T = { timeout: 8000 };
const ok = msg => console.log('ok:', msg);
module.exports = { run, T, ok };
