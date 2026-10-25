//! `btb-lsp`: BTB-specific diagnostics, hover and completion over LSP on stdio.

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let code = btb_lsp::server::run(&mut stdin.lock(), &mut stdout.lock());
    std::process::exit(code);
}
