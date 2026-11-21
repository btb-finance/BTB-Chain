#!/bin/sh
# Build the market-data contract for the BTB VM and copy it to market-data.elf.
set -eu
cd "$(dirname "$0")"
export RUSTC_BOOTSTRAP=1
cargo build --release
cp target/riscv64imac-unknown-none-elf/release/market-data market-data.elf
