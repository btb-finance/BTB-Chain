# BTB Chain for VS Code

Runs `btb-lsp` next to rust-analyzer. See [docs/LSP.md](../../docs/LSP.md).

```
cargo build -p btb-lsp --release       # then put target/release/btb-lsp on PATH or set btb.lsp.path
cd editors/vscode && npm install vscode-languageclient@9
code --extensionDevelopmentPath=$PWD <your-project>
```

Not published; package with `vsce package` if you want a `.vsix`.
