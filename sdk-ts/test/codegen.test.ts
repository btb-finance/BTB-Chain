// The checked-in generated client of examples/todo (golden-tested against `btb codegen` in
// crates/btb-cli) must typecheck under `strict` against these SDK sources, and the type-level
// usage in test/fixtures/usage.ts must hold: its `@ts-expect-error` lines have to really error.
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import ts from "typescript";

const sdk = resolve(import.meta.dirname, "..");
const api = resolve(sdk, "../examples/todo/api");

function check(files: string[], extra: ts.CompilerOptions = {}): string[] {
  const program = ts.createProgram(files, {
    strict: true,
    noUncheckedIndexedAccess: true,
    noEmit: true,
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    lib: ["lib.es2022.d.ts", "lib.dom.d.ts"],
    jsx: ts.JsxEmit.ReactJSX,
    types: [],
    skipLibCheck: false,
    baseUrl: sdk,
    paths: { "@btb/client": ["src/index.ts"], "@btb/client/react": ["src/react.ts"] },
    ...extra,
  });
  return ts
    .getPreEmitDiagnostics(program)
    .filter((d) => !d.file || !d.file.fileName.includes("node_modules"))
    .map((d) => `${d.file?.fileName ?? ""}: ${ts.flattenDiagnosticMessageText(d.messageText, "\n")}`);
}

describe("generated TypeScript client", () => {
  it("api.ts typechecks with strict on", () => {
    expect(check([resolve(api, "api.ts")])).toEqual([]);
  });

  it("api.d.ts (the declarations of api.js) typechecks", () => {
    expect(check([resolve(api, "api.d.ts")])).toEqual([]);
  });

  it("calls are fully typed", () => {
    expect(check([resolve(sdk, "test/fixtures/usage.ts")])).toEqual([]);
  });

  it("the expect-error lines are load-bearing (a wrong call is rejected)", () => {
    const bad = resolve(sdk, "test/fixtures/bad-usage.ts");
    const host = ts.createCompilerHost({});
    const read = host.readFile.bind(host);
    host.readFile = (f) =>
      f === bad
        ? `import { BtbClient } from "@btb/client";\nimport { api } from "../../../examples/todo/api/api";\ndeclare const c: BtbClient;\nvoid c.mutation(api.add, [1n]);\n`
        : read(f);
    host.fileExists = ((orig) => (f: string) => f === bad || orig(f))(host.fileExists.bind(host));
    const program = ts.createProgram([bad], { strict: true, noEmit: true, moduleResolution: ts.ModuleResolutionKind.Bundler, module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022, baseUrl: sdk, paths: { "@btb/client": ["src/index.ts"] }, types: [], skipLibCheck: true }, host);
    expect(ts.getPreEmitDiagnostics(program).length).toBeGreaterThan(0);
  });
});
