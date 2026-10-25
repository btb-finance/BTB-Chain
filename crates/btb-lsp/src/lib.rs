//! BTB language server library: diagnostics, hover and completion for BTB contract sources.
//!
//! Everything is best effort and never panics on invalid code: a file that `syn` cannot parse
//! yields no BTB diagnostics (rust-analyzer already reports syntax errors). Positions are 0-based
//! lines and columns counted in `char`s (LSP asks for UTF-16 units; they agree for ASCII).
//! See docs/LSP.md.

mod analysis;
mod docs;
pub mod server;

pub use analysis::{analyze, Diagnostic, Severity};
pub use docs::{completions, hover, CompletionItem};
