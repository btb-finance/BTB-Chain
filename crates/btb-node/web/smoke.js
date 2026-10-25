// Browser smoke check for the explorer and the faucet page (optional; not part of `cargo test`).
//   cargo build -p btb-node -p btb-faucet && node crates/btb-node/web/smoke.js
// Needs the `playwright` package with a Chromium. Starts a local 4-validator testnet through
// scripts/testnet-local.sh on random loopback ports and tears it down afterwards.
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { chromium } = require('playwright');

const root = path.resolve(__dirname, '../../..');
const rnd = () => 20000 + Math.floor(Math.random() * 20000);
const basePort = rnd(), rpcBase = rnd() + 100, faucetPort = rnd() + 200;
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'btb-smoke-'));
const node0 = `http://127.0.0.1:${rpcBase}`;
const faucet = `http://127.0.0.1:${faucetPort}`;

const sleep = ms => new Promise(r => setTimeout(r, ms));
async function rpc(method, params = []) {
  const r = await fetch(node0 + '/', { method: 'POST', body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) });
  const j = await r.json();
  if (j.error) throw new Error(j.error.message);
  return j.result;
}
function check(cond, msg) { if (!cond) throw new Error('FAIL: ' + msg); console.log('ok: ' + msg); }

async function main() {
  const net = spawn(path.join(root, 'scripts/testnet-local.sh'), [
    '--no-build', '--dir', dir, '--base-port', String(basePort), '--rpc-base-port', String(rpcBase),
    '--faucet-port', String(faucetPort), '--pow-bits', '10', '--epoch-len', '40',
  ], { stdio: ['ignore', 'pipe', 'inherit'] });
  let out = '';
  net.stdout.on('data', d => { out += d; });
  const browser = await chromium.launch();
  try {
    for (let i = 0; i < 240 && !out.includes('is running'); i++) await sleep(250);
    check(out.includes('is running'), 'local testnet of 4 validators started');

    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', e => errors.push(e.message));
    // the refused second drip is a deliberate 429
    page.on('console', m => { if (m.type() === 'error' && !/status of 429/.test(m.text())) errors.push(m.text()); });

    // 1. Faucet page: request coins for a fresh address (with proof of work in the browser).
    const recipient = Array.from({ length: 32 }, () => Math.floor(Math.random() * 256).toString(16).padStart(2, '0')).join('');
    await page.goto(faucet + '/');
    await page.waitForFunction(() => document.getElementById('info').textContent.includes('faucet account'));
    await page.fill('#addr', recipient);
    await page.click('#go');
    await page.waitForFunction(() => document.getElementById('msg').textContent.startsWith('Confirmed in block'), null, { timeout: 60000 });
    const msg = await page.textContent('#msg');
    const txId = msg.match(/Transaction ([0-9a-f]{64})/)[1];
    check(/success/.test(msg), 'faucet page: drip confirmed with proof of work (' + txId.slice(0, 8) + '...)');
    await page.fill('#addr', recipient);
    await page.click('#go');
    await page.waitForFunction(() => /already received/.test(document.getElementById('msg').textContent), null, { timeout: 30000 });
    check(true, 'faucet page: second request for the same address is refused');

    // 2. Explorer home: latest blocks, live updates.
    await page.goto(node0 + '/explorer');
    await page.waitForSelector('#view table tbody tr');
    await page.waitForFunction(() => document.getElementById('status').textContent.startsWith('live'));
    const h1 = await page.evaluate(() => Number(document.querySelector('#view table tbody tr td a').textContent));
    check(h1 >= 2, 'explorer: latest blocks listed (head ' + h1 + ')');
    await page.waitForFunction(h => Number(document.querySelector('#view table tbody tr td a').textContent) > h, h1, { timeout: 15000 });
    check(true, 'explorer: list advances without reload');

    // 3. Search by tx id -> tx detail with receipt, gas and fee.
    await page.fill('#q', txId);
    await page.press('#q', 'Enter');
    await page.waitForSelector('#view h2:text("Transaction")');
    const txText = await page.textContent('#view');
    check(/success/.test(txText) && /transfer/.test(txText) && /100 BTB/.test(txText) && /fee/.test(txText) && /gas used/.test(txText),
      'explorer: tx detail shows kind, amount, status, gas and fee');

    // 4. Block detail via the tx's block link, containing the tx.
    await page.click('#view dd a[href^="#/block/"]');
    await page.waitForSelector('#view h2:text-matches("^Block \\\\d+")');
    check((await page.textContent('#view')).includes(txId.slice(0, 10)), 'explorer: block detail lists the transaction');

    // 5. Account page via search: balance and recent transactions.
    await page.fill('#q', recipient);
    await page.press('#q', 'Enter');
    await page.waitForSelector('#view h2:text("Account")');
    const acc = await page.textContent('#view');
    check(/balance\s*100 BTB/.test(acc) && acc.includes(txId.slice(0, 10)), 'explorer: account shows 100 BTB and its transaction');

    // 6. Faucet account has stake=0, many txs; validator view with stake and status.
    await page.fill('#q', 'validators-not-a-thing');
    await page.press('#q', 'Enter');
    await page.waitForSelector('#view .bad');
    check(true, 'explorer: bad search input shows an error');
    await page.click('nav a[href="#/validators"]');
    await page.waitForSelector('#view h2:text-matches("^Epoch")');
    const rows = await page.locator('#view table tbody tr').count();
    const vt = await page.textContent('#view');
    check(rows === 4 && (vt.match(/100,000 BTB/g) || []).length === 4 && (vt.match(/active/g) || []).length === 4,
      'explorer: validator set of the epoch (4 validators, 100,000 BTB each, active)');

    // 7. Epoch advances (epoch length 40 blocks) and the next epoch's set is shown.
    for (let i = 0; i < 160 && (await rpc('btb_status')).epoch < 1; i++) await sleep(250);
    await page.goto(node0 + '/explorer#/validators/1');
    await page.waitForSelector('#view h2:text("Epoch 1")');
    check((await page.locator('#view table tbody tr').count()) === 4, 'explorer: epoch 1 validator set after rotation');

    check(errors.length === 0, 'no page or console errors' + (errors.length ? ': ' + errors.join('; ') : ''));
    console.log('smoke passed');
  } finally {
    await browser.close();
    net.kill('SIGINT');
    await sleep(500);
    fs.rmSync(dir, { recursive: true, force: true });
  }
}
main().catch(e => { console.error(e.message); process.exit(1); });
