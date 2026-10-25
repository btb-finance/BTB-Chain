#!/bin/sh
# Build the token contract for the BTB VM and copy it to token.elf.
set -eu
cd "$(dirname "$0")"
export RUSTC_BOOTSTRAP=1
cargo build --release
cp target/riscv64imac-unknown-none-elf/release/token token.elf
