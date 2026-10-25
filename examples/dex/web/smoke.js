// Browser smoke check for the DEX page. See ../../smoke-lib.js.
//   cargo build -p btb-cli && node examples/dex/web/smoke.js
const { run, T, ok } = require('../../smoke-lib');

const text = (p, id) => p.waitForFunction(([i]) => document.getElementById(i).textContent, [id], T);
const has = (p, id, want) =>
  p.waitForFunction(([i, w]) => document.getElementById(i).textContent === w, [id, want], T);

run('dex', async (newPage) => {
  const a = await newPage('alice');
  const b = await newPage('bob');
  await a.click('#addBtn');
  await has(a, 'shares', '2000000');
  await has(b, 'ra', '1000000');
  await has(b, 'rb', '4000000');
  ok('liquidity added by alice shows up live for bob');
  await has(b, 'quote', '3984');
  ok('quote for 1000 A is 3984 B');
  await b.click('#swapBtn');
  await has(a, 'ra', '1001000');
  await has(a, 'rb', '3996016');
  ok('bob\'s swap moves the reserves live in alice\'s tab');
  await b.fill('#amtIn', '10000');
  await has(b, 'quote', '39407');
  ok('changing the amount re-subscribes the quote');
  await a.fill('#addA', '0');
  await a.click('#addBtn');
  await a.waitForFunction(() => document.getElementById('err').textContent.includes('zero amount'), null, T);
  ok('zero amount is rejected with the contract message');
});
