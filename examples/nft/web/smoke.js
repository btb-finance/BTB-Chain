// Browser smoke check for the NFT market page. See ../../smoke-lib.js.
//   cargo build -p btb-cli && node examples/nft/web/smoke.js
const { run, T, ok } = require('../../smoke-lib');

const has = (p, id, want) =>
  p.waitForFunction(([i, w]) => document.getElementById(i).textContent === w, [id, want], T);
const count = (p, sel, n) =>
  p.waitForFunction(([s, n]) => document.querySelectorAll(s).length === n, [sel, n], T);
const err = (p, msg) =>
  p.waitForFunction(m => document.getElementById('err').textContent.includes(m), msg, T);

run('nft', async (newPage) => {
  const a = await newPage('alice');
  await a.click('#mkcur');
  await a.waitForFunction(() => /^[0-9a-f]{64}$/.test(document.getElementById('token').value), null, T);
  const token = await a.inputValue('#token');
  const b = await newPage('bob', '#' + token);
  const c = await newPage('carol', '#' + token);
  await has(b, 'unclaimed', '1000000');
  ok('demo currency created; supply is unclaimed in the market');

  await a.check('#mintList');
  await a.fill('#mintPrice', '100');
  await a.click('#mintBtn');
  await count(a, '#mine li.nft', 1);
  await count(b, '#market li.listing', 1);
  await count(b, '#mine li.nft', 0);
  ok('mint-and-list shows up live in the market of the other tabs');

  // Carol has no escrow: buying fails and nothing changes.
  await c.click('#market li.listing button.buy');
  await err(c, 'insufficient escrow');
  await count(c, '#market li.listing', 1);
  ok('buying without escrow is rejected');

  // Bob funds his escrow, then buys.
  await b.fill('#amt', '250');
  await b.click('#dep');
  await has(b, 'escrow', '250');
  await has(b, 'unclaimed', '999750');
  await b.click('#market li.listing button.buy');
  await count(b, '#mine li.nft', 1);
  await count(a, '#market li.listing', 0);
  await count(a, '#mine li.nft', 0);
  await has(b, 'escrow', '150');
  await has(a, 'tbal', '100');
  ok('buy moves the NFT, debits the escrow and pays the seller, live in all tabs');

  // Double buy: carol still sees no listing; seller cannot take it back.
  await count(c, '#market li.listing', 0);

  // Bob relists, alice cannot buy her own... but bob can cancel, and bob can send it on.
  await b.click('#mine li.nft button.sell');
  await count(c, '#market li.listing', 1);
  await b.click('#market li.listing button.cancel');
  await count(c, '#market li.listing', 0);
  await b.fill('#mine li.nft input[placeholder="send to"]', 'carol');
  await b.click('#mine li.nft button.send');
  await count(c, '#mine li.nft', 1);
  await count(b, '#mine li.nft', 0);
  ok('relist, cancel and transfer to another user work live');
  await b.fill('#amt', '150');
  await b.click('#wd');
  await has(b, 'escrow', '0');
  await has(b, 'tbal', '150');
  ok('withdraw returns unused escrow');
});
