// Browser smoke check for the launcher page. See ../../smoke-lib.js.
//   cargo build -p btb-cli && node examples/launcher/web/smoke.js
const { run, T, ok } = require('../../smoke-lib');

run('launcher', async (newPage) => {
  const a = await newPage('alice');
  await a.click('#launchBtn');
  await a.waitForFunction(() => /^[0-9a-f]{64}$/.test(document.getElementById('token').value), null, T);
  await a.waitForFunction(() => document.getElementById('info').textContent.includes('Gold (GLD), 6 decimals, supply 1000000'), null, T);
  ok('launch fills the token id and the live info');
  const token = await a.inputValue('#token');

  // Second tab, another user, same token via the URL hash.
  const b = await newPage('bob', '#' + token);
  await b.waitForFunction(() => document.getElementById('info').textContent.includes('Gold'), null, T);
  await a.waitForFunction(() => document.querySelectorAll('li.holder').length === 1, null, T); // the contract
  await a.click('#claim');
  await a.waitForFunction(() => document.getElementById('bal').textContent === '100', null, T);
  await b.waitForFunction(() => document.querySelectorAll('li.holder').length === 2, null, T);
  ok('claim shows up live in the other tab (holders 1 -> 2)');
  await b.click('#claim');
  await b.waitForFunction(() => document.getElementById('bal').textContent === '100', null, T);
  await a.waitForFunction(() => document.querySelectorAll('li.holder').length === 3, null, T);
  ok('second claimer appears live in the first tab');
  await b.click('#claim');
  await b.waitForFunction(() => document.getElementById('err').textContent.includes('already claimed'), null, T);
  ok('double claim is rejected');
});
