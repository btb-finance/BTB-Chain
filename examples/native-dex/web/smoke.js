// Browser smoke check for the native DEX page (optional; not part of `cargo test`).
//   cargo build -p btb-node -p btb-faucet && node examples/native-dex/web/smoke.js
// Needs the `playwright` package with a Chromium and the launcher ELF
// (`cd examples/launcher && ./build.sh`, or any `cargo test -p btb-chain` run builds it).
// Starts a local 4-validator testnet through scripts/testnet-local.sh on random loopback
// ports, creates two user tokens with the launcher contract, and drives three browser tabs
// (an issuer, Alice and Bob) through batch auctions on the page served at `GET /dex`.
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { chromium } = require('playwright');

const U64 = (1n << 64n) - 1n;
const root = path.resolve(__dirname, '../../..');
const rnd = () => 20000 + Math.floor(Math.random() * 20000);
const basePort = rnd(), rpcBase = rnd() + 100, faucetPort = rnd() + 200;
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'btb-dex-smoke-'));
const node0 = `http://127.0.0.1:${rpcBase}`;
const faucet = `http://127.0.0.1:${faucetPort}`;
const sleep = ms => new Promise(r => setTimeout(r, ms));
function check(cond, msg) { if (!cond) throw new Error('FAIL: ' + msg); console.log('ok: ' + msg); }
async function rpc(method, params = []) {
  const r = await fetch(node0 + '/', { method: 'POST', body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }) });
  const j = await r.json();
  if (j.error) throw new Error(j.error.message);
  return j.result;
}
const seeds = { issuer: '01'.repeat(32), alice: '02'.repeat(32), bob: '03'.repeat(32) };

// canonical argument encoding of the launcher's `launch` (name, symbol, decimals, supply, faucet, mintable)
const le = (v, n) => { v = BigInt(v); const o = Buffer.alloc(n); for (let i = 0; i < n; i++) { o[i] = Number(v & 0xffn); v >>= 8n; } return o; };
const str = s => Buffer.concat([le(Buffer.byteLength(s), 4), Buffer.from(s)]);
const launchArgs = (name, sym, supply, faucetAmount) =>
  Buffer.concat([str(name), str(sym), le(9, 1), le(supply, 16), le(faucetAmount, 16), le(0, 1)]);

function launcherElf() {
  const dirL = path.join(root, 'examples/launcher');
  const elf = path.join(dirL, 'target/riscv64imac-unknown-none-elf/release/launcher');
  if (!fs.existsSync(elf)) {
    const env = { ...process.env, RUSTC_BOOTSTRAP: '1' };
    for (const k of ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET', 'CARGO_TARGET_DIR']) delete env[k];
    require('child_process').execSync('cargo build --release --quiet', { cwd: dirL, env, stdio: 'inherit' });
  }
  return fs.readFileSync(elf);
}

async function main() {
  const net = spawn(path.join(root, 'scripts/testnet-local.sh'), [
    '--no-build', '--dir', dir, '--base-port', String(basePort), '--rpc-base-port', String(rpcBase),
    '--faucet-port', String(faucetPort),
  ], { stdio: ['ignore', 'pipe', 'inherit'] });
  let out = '';
  net.stdout.on('data', d => { out += d; });
  const browser = await chromium.launch();
  try {
    for (let i = 0; i < 240 && !out.includes('is running'); i++) await sleep(250);
    check(out.includes('is running'), 'local testnet of 4 validators started');

    const errors = [];
    const open = async (seed) => {
      const ctx = await browser.newContext();
      const p = await ctx.newPage();
      p.on('pageerror', e => errors.push(e.message));
      p.on('console', m => { if (m.type() === 'error') errors.push(m.text()); });
      await p.goto(`${node0}/dex?seed=${seed}`);
      await p.waitForSelector('body[data-ready="1"]');
      return p;
    };
    const issuer = await open(seeds.issuer), alice = await open(seeds.alice), bob = await open(seeds.bob);
    const addr = async p => p.evaluate(() => window.dexApp.key.hexAddr);
    const A = { issuer: await addr(issuer), alice: await addr(alice), bob: await addr(bob) };
    check(await issuer.textContent('#chain') !== '-' && Number(await issuer.textContent('#window')) >= 1, 'page loads the chain id and DEX parameters from the node');

    // 1. Fund the three accounts from the faucet.
    for (const a of Object.values(A)) {
      const r = await fetch(faucet + '/drip', { method: 'POST', body: JSON.stringify({ address: a }) });
      check(r.ok, 'faucet funded ' + a.slice(0, 8));
    }
    for (let i = 0; i < 120; i++) {
      const b = await rpc('btb_getBalance', [A.bob]);
      if (BigInt(b.balance) > 0n) break;
      await sleep(250);
    }

    // 2. Issuer deploys the launcher contract and creates two tokens; Alice and Bob claim them.
    const elf = launcherElf().toString('base64');
    const ok = r => { if (r.status !== 'success') throw new Error('tx failed: ' + JSON.stringify(r)); return r; };
    const r1 = ok(await issuer.evaluate(async b64 => {
      const code = Uint8Array.from(atob(b64), c => c.charCodeAt(0));
      return window.dexApp.send(window.dexApp.T.deploy(code, 'launcher'));
    }, elf));
    const launcher = r1.output;
    ok(await issuer.evaluate(async c => window.dexApp.send(window.dexApp.T.transfer(c, 1000000000n)), launcher));
    const tokens = [];
    for (const [name, sym] of [['Gold', 'GLD'], ['Silver', 'SLV']]) {
      const args = launchArgs(name, sym, 10n ** 12n, 100000n).toString('hex');
      const r = ok(await issuer.evaluate(async ([c, a]) =>
        window.dexApp.send(window.dexApp.T.call(c, 'launch', window.dexApp.unhex(a))), [launcher, args]));
      tokens.push(r.output);
    }
    for (const t of tokens) for (const p of [alice, bob]) {
      ok(await p.evaluate(async ([c, t]) =>
        window.dexApp.send(window.dexApp.T.call(c, 'claim', window.dexApp.unhex(t))), [launcher, t]));
    }
    const bal = async (who, asset) => BigInt(await rpc('btb_dexBalance', [A[who], asset]));
    check((await bal('alice', tokens[0])) === 100000n && (await bal('bob', tokens[0])) === 100000n, 'Alice and Bob each hold 100000 GLD (user token)');

    // 3. Pair BTB/GLD in the UI. Alice rests an ask; Bob sees it live and crosses it.
    const pick = async (p, a, b) => { await p.selectOption('#asset-a', a); await p.selectOption('#asset-b', b); };
    for (const p of [alice, bob]) await pick(p, 'btb', tokens[0]);
    const aliceBtb0 = await bal('alice', 'btb'), bobBtb0 = await bal('bob', 'btb');
    await alice.fill('#amount', '1000'); await alice.fill('#price', '10'); await alice.fill('#batches', '50');
    await alice.click('#sell');
    await alice.waitForFunction(() => /placed in block/.test(document.getElementById('msg').textContent), null, { timeout: 60000 });
    check(true, 'Alice: ask 1000 BTB units at 10 GLD placed through a signed transaction');
    await bob.waitForFunction(() => document.querySelectorAll('#asks tr.ask').length === 1, null, { timeout: 15000 });
    check((await bob.textContent('#asks')).includes('1000'), "Bob's page shows Alice's resting ask live (no reload)");
    check((await bal('alice', 'btb')) <= aliceBtb0 - 1000n, "Alice's 1000 units are escrowed");

    // Bob bids for 1000 at up to 12: both limits are candidates, the volume is the same (1000) and
    // the imbalance 0 at both, so the lowest price 10 is the one uniform price.
    await bob.fill('#amount', '1000'); await bob.fill('#price', '12'); await bob.fill('#batches', '1');
    await bob.click('#buy');
    await bob.waitForFunction(() => /placed in block/.test(document.getElementById('msg').textContent), null, { timeout: 60000 });
    await bob.waitForFunction(() => document.getElementById('last').textContent === '10', null, { timeout: 15000 });
    check(true, 'Bob: bid cleared at the single batch price 10 (shown as the last clearing price)');
    const info = await bob.textContent('#lastinfo');
    check(/1000 BTB for 10000 GLD in 2 orders/.test(info), 'clearing info: 1000 BTB for 10000 GLD in 2 orders (' + info + ')');
    await alice.waitForFunction(() => document.querySelectorAll('#history tr').length >= 1 && /10/.test(document.getElementById('history').textContent), null, { timeout: 15000 });
    check((await bob.textContent('#history')).includes('1000'), 'recent clearings table lists the batch');
    check((await bal('alice', tokens[0])) === 100000n + 9990n, 'Alice received 10000 GLD less the 0.1% clearing fee (9990)');
    check((await bal('bob', tokens[0])) === 100000n - 10000n, 'Bob paid 10000 GLD');
    check((await bal('bob', 'btb')) >= bobBtb0 + 999n - 100000000n, 'Bob received the 1000 BTB units less the fee (999)');
    await bob.waitForFunction(() => /filled/.test(document.getElementById('results').textContent), null, { timeout: 15000 });
    check(/filled/.test(await bob.textContent('#results')), 'Bob: closed-order list shows the fill');
    const cfg = await rpc('btb_dexConfig');
    const feeGld = BigInt(await rpc('btb_dexBalance', [cfg.fee_account, tokens[0]]));
    check(feeGld === 10n && (await bal('alice', tokens[0])) + (await bal('bob', tokens[0])) + feeGld === 200000n, 'the 10 GLD clearing fee sits in the fee account; Alice + Bob + fee = what they claimed');

    // 4. Cancel from the UI refunds the escrow.
    await alice.fill('#amount', '500'); await alice.fill('#price', '50'); await alice.fill('#batches', '100');
    await alice.click('#sell');
    await alice.waitForFunction(() => document.querySelectorAll('#orders [data-cancel]').length === 1, null, { timeout: 60000 });
    const before = await bal('alice', 'btb');
    await alice.click('#orders [data-cancel]');
    await alice.waitForFunction(() => /cancelled and refunded/.test(document.getElementById('msg').textContent), null, { timeout: 60000 });
    await alice.waitForFunction(() => document.querySelectorAll('#orders [data-cancel]').length === 0, null, { timeout: 15000 });
    check((await bal('alice', 'btb')) >= before + 500n - 100000000n, 'Cancel refunded the 500 escrowed units (less the gas fee)');
    check(/cancelled/.test(await alice.textContent('#results')), 'closed orders list shows the cancelled order');

    // 5. A token/token pair works through the same page (GLD vs SLV).
    for (const p of [alice, bob]) await pick(p, tokens[0], tokens[1]);
    await alice.fill('#amount', '100'); await alice.fill('#price', '2'); await alice.fill('#batches', '50');
    await alice.click('#sell');
    await alice.waitForFunction(() => /placed in block/.test(document.getElementById('msg').textContent), null, { timeout: 60000 });
    await bob.fill('#amount', '100'); await bob.fill('#price', '3'); await bob.fill('#batches', '1');
    await bob.click('#buy');
    await bob.waitForFunction(() => /placed in block/.test(document.getElementById('msg').textContent), null, { timeout: 60000 });
    await bob.waitForFunction(() => document.getElementById('last').textContent !== '-', null, { timeout: 15000 });
    const pairs = await rpc('btb_dexPairs');
    check(pairs.length === 2 && pairs.every(p => p.last), 'two pairs (BTB/GLD and a token/token pair) have clearing prices');
    // Conservation across every known holder: the launcher, Alice, Bob, the DEX vault and the fee account.
    const total = async t => {
      let sum = 0n;
      for (const a of [launcher, A.alice, A.bob, A.issuer, cfg.vault, cfg.fee_account]) sum += BigInt(await rpc('btb_dexBalance', [a, t]));
      return sum;
    };
    check((await total(tokens[0])) === 10n ** 12n && (await total(tokens[1])) === 10n ** 12n, 'GLD and SLV supplies (10^12 each) are exactly conserved across holders, vault and fee account');

    // 6. Amounts beyond 64 bits: two tokens with supply 10^30 and a faucet of 10^22 per claim.
    //    An order of 2 * 10^20 units (above 2^64 = 1.8 * 10^19) is placed through the page and cleared.
    const wide = [];
    for (const [name, sym] of [['Wide', 'WID'], ['Deep', 'DEP']]) {
      const args = launchArgs(name, sym, 10n ** 30n, 10n ** 22n).toString('hex');
      const r = ok(await issuer.evaluate(async ([c, a]) =>
        window.dexApp.send(window.dexApp.T.call(c, 'launch', window.dexApp.unhex(a))), [launcher, args]));
      wide.push(r.output);
    }
    for (const t of wide) for (const p of [alice, bob]) {
      ok(await p.evaluate(async ([c, t]) =>
        window.dexApp.send(window.dexApp.T.call(c, 'claim', window.dexApp.unhex(t))), [launcher, t]));
    }
    const wp = await rpc('btb_dexPair', wide);
    const [wBase, wQuote] = [wp.base.id, wp.quote.id];
    const big = 200000000000000000000n;
    for (const p of [alice, bob]) await pick(p, wide[0], wide[1]);
    const aq0 = await bal('alice', wQuote), bb0 = await bal('bob', wBase);
    await alice.fill('#amount', big.toString()); await alice.fill('#price', '1'); await alice.fill('#batches', '50');
    await alice.click('#sell');
    await alice.waitForFunction(() => /placed in block/.test(document.getElementById('msg').textContent), null, { timeout: 60000 });
    await bob.fill('#amount', big.toString()); await bob.fill('#price', '1'); await bob.fill('#batches', '1');
    await bob.click('#buy');
    await bob.waitForFunction(() => /placed in block/.test(document.getElementById('msg').textContent), null, { timeout: 60000 });
    await bob.waitForFunction(() => /200000000000000000000/.test(document.getElementById('lastinfo').textContent), null, { timeout: 30000 });
    check(big > U64, 'a 2 * 10^20 unit order (above 2^64) was accepted and cleared at price 1');
    check((await bal('alice', wQuote)) === aq0 + big - big / 1000n, 'Alice received 2 * 10^20 less the 0.1% fee in the quote token');
    check((await bal('bob', wBase)) === bb0 + big - big / 1000n, 'Bob received 2 * 10^20 less the 0.1% fee in the base token');
    const totalW = async t => {
      let sum = 0n;
      for (const a of [launcher, A.alice, A.bob, A.issuer, cfg.vault, cfg.fee_account]) sum += BigInt(await rpc('btb_dexBalance', [a, t]));
      return sum;
    };
    check((await totalW(wBase)) === 10n ** 30n && (await totalW(wQuote)) === 10n ** 30n, 'both wide token supplies (10^30) are exactly conserved');

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
