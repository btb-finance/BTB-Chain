#!/bin/sh
# Build the defi-yields contract for the BTB VM and copy it to defi-yields.elf.
set -eu
cd "$(dirname "$0")"
export RUSTC_BOOTSTRAP=1
cargo build --release
cp target/riscv64imac-unknown-none-elf/release/defi-yields defi-yields.elf
