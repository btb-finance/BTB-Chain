// Minimal VS Code client for btb-lsp. No build step and no dependencies beyond `vscode-languageclient`
// (see README.md). rust-analyzer keeps doing completion and types; btb-lsp adds BTB diagnostics,
// attribute hover and host API completion.
const vscode = require("vscode");

let client;

function activate() {
  const cfg = vscode.workspace.getConfiguration("btb.lsp");
  if (!cfg.get("enable", true)) return;
  let lc;
  try {
    lc = require("vscode-languageclient/node");
  } catch (e) {
    vscode.window.showWarningMessage(
      "BTB: run `npm install` in editors/vscode to enable btb-lsp (" + e.message + ")"
    );
    return;
  }
  const command = cfg.get("path", "btb-lsp");
  client = new lc.LanguageClient(
    "btb-lsp",
    "BTB Language Server",
    { command, args: [] },
    { documentSelector: [{ scheme: "file", language: "rust" }] }
  );
  client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
