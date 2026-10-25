#!/bin/sh
# Build the counter contract for the BTB VM and copy it to counter.elf.
set -eu
cd "$(dirname "$0")"
export RUSTC_BOOTSTRAP=1
cargo build --release
cp target/riscv64imac-unknown-none-elf/release/counter counter.elf
