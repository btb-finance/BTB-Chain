#!/bin/sh
# Build the C counter contract for the BTB VM (rv64im, no compressed/atomic/float, no relaxation).
set -eu
cd "$(dirname "$0")"
CLANG="${CLANG:-/usr/bin/clang}"
LLD="${LLD:-ld.lld}"
mkdir -p build
"$CLANG" --target=riscv64-unknown-elf -march=rv64im -mabi=lp64 -nostdlib -ffreestanding \
  -O2 -mno-relax -fno-pic -fno-pie -I ../../include -c counter.c -o build/counter.o
"$LLD" -T link.ld --gc-sections --no-relax -o counter.elf build/counter.o
