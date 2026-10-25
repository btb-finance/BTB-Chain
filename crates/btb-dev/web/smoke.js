// Browser smoke check for the dashboard (optional; not part of `cargo test`).
//   cargo build -p btb-dev && examples/todo/build.sh && node crates/btb-dev/web/smoke.js
// Needs the `playwright` package with a Chromium. Starts btb-dev itself.
const { spawn } = require('child_process');
const path = require('path');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../../..');
const port = 8000 + Math.floor(Math.random() * 1000);
const base = `http://127.0.0.1:${port}`;

async function add(who, text) {
  const r = await fetch(`${base}/mutation/add?caller=${who}`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify([text]),
  });
  if (!r.ok) throw new Error('add failed ' + await r.text());
}

async function main() {
  const srv = spawn(path.join(root, 'target/debug/btb-dev'),
    [path.join(root, 'examples/todo/todo.elf'), '--port', String(port)], { stdio: 'inherit' });
  await new Promise(r => setTimeout(r, 800));
  const browser = await chromium.launch();
  try {
    await add('alice', 'buy milk');
    await add('alice', 'walk dog');
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', e => errors.push(e.message));
    await page.goto(`${base}/dashboard`);
    await page.waitForSelector('#tables li:has-text("Todo")');
    await page.waitForFunction(() => document.getElementById('status').textContent === 'live');
    await page.waitForSelector('#rows tbody tr');
    const n = await page.locator('#rows tbody tr').count();
    if (n !== 2) throw new Error(`expected 2 rows, got ${n}`);
    await page.waitForSelector('#rows td:has-text("buy milk")');
    console.log('ok: table Todo renders 2 decoded rows');
    // Live: a commit from elsewhere shows up without reload.
    await add('bob', 'live row');
    await page.waitForSelector('#rows td:has-text("live row")', { timeout: 5000 });
    console.log('ok: new row appears live via SSE');
    // Browse by index value.
    await page.selectOption('#idxsel', 'owner');
    await page.fill('#idxval', '0'.repeat(63) + '1');
    await page.click('#idxgo');
    await page.waitForFunction(() => document.querySelectorAll('#rows tbody tr').length === 0);
    console.log('ok: index browse with an unused value shows no rows');
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  } finally {
    await browser.close();
    srv.kill();
  }
}
main().catch(e => { console.error('FAIL', e); process.exit(1); });
