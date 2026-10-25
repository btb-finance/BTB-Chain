#!/bin/sh
# Build the launcher contract for the BTB VM and copy it to launcher.elf.
set -eu
cd "$(dirname "$0")"
export RUSTC_BOOTSTRAP=1
cargo build --release
cp target/riscv64imac-unknown-none-elf/release/launcher launcher.elf
