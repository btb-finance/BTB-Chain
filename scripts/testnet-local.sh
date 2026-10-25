#!/usr/bin/env bash
# Run a local BTB testnet: 4 validators (more with --validators) plus the faucet, all on 127.0.0.1.
# Nothing here touches the network beyond loopback. Ctrl-C stops everything.
#
#   scripts/testnet-local.sh [--dir DIR] [--validators N] [--base-port P] [--rpc-base-port P]
#                            [--faucet-port P] [--epoch-len N] [--pow-bits N] [--release] [--no-build]
#
# Node i serves its JSON-RPC and the explorer on 127.0.0.1:(rpc-base-port + i).
# The faucet page is on 127.0.0.1:faucet-port. See docs/TESTNET.md.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dir=""
validators=4
base_port=9000
rpc_base_port=8545
faucet_port=8080
epoch_len=""
pow_bits=0
profile=debug
build=1

while [ $# -gt 0 ]; do
  case "$1" in
    --dir) dir="$2"; shift 2 ;;
    --validators) validators="$2"; shift 2 ;;
    --base-port) base_port="$2"; shift 2 ;;
    --rpc-base-port) rpc_base_port="$2"; shift 2 ;;
    --faucet-port) faucet_port="$2"; shift 2 ;;
    --epoch-len) epoch_len="$2"; shift 2 ;;
    --pow-bits) pow_bits="$2"; shift 2 ;;
    --release) profile=release; shift ;;
    --no-build) build=0; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$dir" ]; then dir="$(mktemp -d "${TMPDIR:-/tmp}/btb-testnet.XXXXXX")"; fi
if [ -e "$dir/genesis.json" ]; then
  echo "error: $dir already holds a testnet; pick a new --dir (a chain cannot be restarted from memory)" >&2
  exit 1
fi

if [ "$build" = 1 ]; then
  flag=""; [ "$profile" = release ] && flag="--release"
  (cd "$root" && cargo build $flag -p btb-node -p btb-faucet)
fi
bin="$root/target/$profile"
[ -x "$bin/btb-node" ] && [ -x "$bin/btb-faucet" ] || { echo "error: build btb-node and btb-faucet first" >&2; exit 1; }

"$bin/btb-node" init-testnet --validators "$validators" --dir "$dir" \
  --base-port "$base_port" --rpc-base-port "$rpc_base_port"
if [ -n "$epoch_len" ]; then
  # The validator set and balances are committed in the genesis hash: edit before the first start.
  sed -i "s/\"epoch_len\": [0-9]*/\"epoch_len\": $epoch_len/" "$dir/genesis.json"
fi

pids=()
cleanup() {
  trap - EXIT INT TERM
  for p in "${pids[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

mkdir -p "$dir/logs"
for ((i = 0; i < validators; i++)); do
  "$bin/btb-node" run --config "$dir/node$i/node.json" >"$dir/logs/node$i.log" 2>&1 &
  pids+=($!)
done

# Wait for the first blocks.
rpc0="127.0.0.1:$rpc_base_port"
for _ in $(seq 1 120); do
  h="$(curl -fsS "http://$rpc0/status" 2>/dev/null | sed -n 's/.*"height":\([0-9]*\).*/\1/p' || true)"
  if [ -n "${h:-}" ] && [ "$h" -ge 2 ]; then break; fi
  sleep 0.5
done
if [ -z "${h:-}" ] || [ "$h" -lt 2 ]; then echo "error: the chain did not start; see $dir/logs" >&2; exit 1; fi

rpcs=()
for ((i = 0; i < validators; i++)); do rpcs+=(--rpc "127.0.0.1:$((rpc_base_port + i))"); done
"$bin/btb-faucet" --key "$dir/faucet.key" "${rpcs[@]}" --bind "127.0.0.1:$faucet_port" \
  --pow-bits "$pow_bits" >"$dir/logs/faucet.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$faucet_port/health" >/dev/null 2>&1 && break
  sleep 0.25
done

cat <<MSG

BTB local testnet is running ($validators validators, chain height $h).
  data and keys   $dir        (faucet.key is the funded faucet account)
  logs            $dir/logs
  node 0 RPC      http://127.0.0.1:$rpc_base_port/status
  explorer        http://127.0.0.1:$rpc_base_port/explorer
  faucet          http://127.0.0.1:$faucet_port/
Ctrl-C to stop. The chain lives in memory and is gone when the nodes stop.
MSG

wait
