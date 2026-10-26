import { resolve } from "node:path";
import { defineConfig } from "vitest/config";

// Generated clients (`btb codegen`) import "@btb/client"; point it at the sources under test.
export default defineConfig({
  resolve: {
    alias: [
      { find: /^@btb\/client\/react$/, replacement: resolve(import.meta.dirname, "src/react.ts") },
      { find: /^@btb\/client$/, replacement: resolve(import.meta.dirname, "src/index.ts") },
    ],
  },
});
