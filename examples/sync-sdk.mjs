#!/usr/bin/env node
// Inline the built @btb/client into every example page (no build step at runtime, no extra files:
// `btb dev` serves only web/index.html).
//
//   (cd sdk-ts && npm ci && npm run build) && node examples/sync-sdk.mjs [--check]
//
// Rewrites the block between `<!-- btb-client:begin -->` and `<!-- btb-client:end -->` in
// examples/*/web/index.html with a classic <script> that defines the global `btb`
// ({ BtbClient, BtbError, t, encode, decode, toHex, fromHex }). `--check` only reports pages whose
// block differs from the current sdk-ts/dist (exit 1), for CI.
import { readFileSync, writeFileSync, readdirSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "../sdk-ts/dist");
const check = process.argv.includes("--check");

function module(file) {
  return readFileSync(join(dist, file), "utf8")
    .replace(/^import .*$/gm, "")
    .replace(/^export (?=(const|function|class|let) )/gm, "")
    .replace(/^\/\/# sourceMappingURL=.*$/gm, "");
}

if (!existsSync(join(dist, "client.js"))) {
  console.error("sdk-ts/dist is missing: run `npm ci && npm run build` in sdk-ts first");
  process.exit(2);
}
const bundle =
  "<script>\n// @btb/client (MIT), inlined from sdk-ts/dist by examples/sync-sdk.mjs. Do not edit.\n" +
  "const btb = (() => {\n" +
  module("codec.js") +
  "\n" +
  module("client.js") +
  "\nreturn { BtbClient, BtbError, t, encode, decode, toHex, fromHex };\n})();\n</script>";
const re = /<!-- btb-client:begin -->[\s\S]*?<!-- btb-client:end -->/;
const block = `<!-- btb-client:begin -->\n${bundle}\n<!-- btb-client:end -->`;

let stale = 0;
for (const d of readdirSync(here, { withFileTypes: true })) {
  const page = join(here, d.name, "web/index.html");
  if (!d.isDirectory() || !existsSync(page)) continue;
  const html = readFileSync(page, "utf8");
  if (!re.test(html)) continue;
  const next = html.replace(re, () => block);
  if (next === html) continue;
  if (check) {
    console.error(`stale: examples/${d.name}/web/index.html`);
    stale++;
  } else {
    writeFileSync(page, next);
    console.log(`updated examples/${d.name}/web/index.html`);
  }
}
process.exit(stale ? 1 : 0);
