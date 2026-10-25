// Browser smoke check for the todo demo (optional; not part of `cargo test`).
//   cargo build -p btb-dev && examples/todo/build.sh && node examples/todo/web/smoke.js
// Needs the `playwright` package with a Chromium. Starts btb-dev itself.
const { spawn } = require('child_process');
const path = require('path');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../../..');
const port = 8000 + Math.floor(Math.random() * 1000);

async function main() {
  const srv = spawn(path.join(root, 'target/debug/btb-dev'),
    [path.join(root, 'examples/todo/todo.elf'), '--port', String(port)], { stdio: 'inherit' });
  await new Promise(r => setTimeout(r, 800));
  const browser = await chromium.launch();
  try {
    const url = `http://127.0.0.1:${port}/`;
    const a = await browser.newPage();
    const b = await browser.newPage(); // second tab, same user: must update without reload
    await a.goto(url);
    await b.goto(url);
    await b.waitForFunction(() => document.getElementById('status').textContent.startsWith('live'));
    await a.fill('#text', 'buy milk');
    await a.click('form button');
    await b.waitForSelector('li:has-text("buy milk")', { timeout: 5000 });
    console.log('ok: add shows up live in the other tab');
    await a.click('li:has-text("buy milk") button');
    await b.waitForSelector('li.done:has-text("buy milk")', { timeout: 5000 });
    console.log('ok: toggle shows up live in the other tab');
    // A different user sees an empty list (index scoped to owner).
    await b.fill('#who', 'bob');
    await b.dispatchEvent('#who', 'change');
    await b.waitForFunction(() => document.querySelectorAll('li').length === 0, null, { timeout: 5000 });
    console.log('ok: other user sees only their own todos');
  } finally {
    await browser.close();
    srv.kill();
  }
}
main().catch(e => { console.error('FAIL', e); process.exit(1); });
