#!/usr/bin/env bash
# Run a local Tenebra devnet: N validators (default 1), each a CometBFT node
# plus its own tenebra-node application, all on 127.0.0.1.
#
#   scripts/devnet.sh        # 1 validator
#   scripts/devnet.sh 4      # 4 validators (tolerates 1 crashing)
#   DEVNET_DIR=/tmp/tnb scripts/devnet.sh 4
#   scripts/devnet.sh --reset [N]   # wipe and start from a fresh genesis
#
# Ports for node i: p2p 26656+10i, RPC 26657+10i, ABCI 26658+10i.
# Press Ctrl-C to stop everything.
set -euo pipefail

RESET=0
if [ "${1:-}" = "--reset" ]; then RESET=1; shift; fi
N=${1:-1}
ROOT=${DEVNET_DIR:-$HOME/.tenebra-devnet}
REPO=$(cd "$(dirname "$0")/.." && pwd)

command -v cometbft >/dev/null || {
  echo "cometbft not found. Install it (needs Go 1.22+):"
  echo "  go install github.com/cometbft/cometbft/cmd/cometbft@v0.38.26"
  echo "  export PATH=\$PATH:\$(go env GOPATH)/bin"
  exit 1
}
echo "building tenebra-node…"
cargo build --release -q -p tenebra-node --manifest-path "$REPO/Cargo.toml"
TNB="$REPO/target/release/tenebra-node"

if [ "$RESET" = 1 ]; then rm -rf "$ROOT"; fi
if [ ! -d "$ROOT/node0" ]; then
  echo "creating $N-validator devnet in $ROOT"
  cometbft testnet --v "$N" --o "$ROOT" --starting-ip-address 127.0.0.1 >/dev/null
  HOMES=()
  for i in $(seq 0 $((N - 1))); do HOMES+=(--home "$ROOT/node$i"); done
  IDS=()
  for i in $(seq 0 $((N - 1))); do IDS+=("$(cometbft show-node-id --home "$ROOT/node$i")@127.0.0.1:$((26656 + 10 * i))"); done
  PEERS=$(IFS=,; echo "${IDS[*]}")
  for i in $(seq 0 $((N - 1))); do
    python3 - "$ROOT/node$i/config/config.toml" "$i" "$PEERS" <<'PY'
import re, sys
path, i, peers = sys.argv[1], int(sys.argv[2]), sys.argv[3]
own = [p for p in peers.split(",") if p.endswith(f":{26656 + 10 * i}")]
peers = ",".join(p for p in peers.split(",") if p not in own)
s = open(path).read()
section = None
out = []
for line in s.splitlines():
    m = re.match(r"^\[(.+)\]$", line.strip())
    if m:
        section = m.group(1)
    if section is None and line.startswith("proxy_app"):
        line = f'proxy_app = "tcp://127.0.0.1:{26658 + 10 * i}"'
    elif section == "rpc" and line.startswith("laddr"):
        line = f'laddr = "tcp://127.0.0.1:{26657 + 10 * i}"'
    elif section == "p2p" and line.startswith("laddr"):
        line = f'laddr = "tcp://127.0.0.1:{26656 + 10 * i}"'
    elif section == "p2p" and line.startswith("persistent_peers ="):
        line = f'persistent_peers = "{peers}"'
    elif section == "p2p" and line.startswith("addr_book_strict"):
        line = "addr_book_strict = false"
    elif section == "p2p" and line.startswith("allow_duplicate_ip"):
        line = "allow_duplicate_ip = true"
    elif line.startswith("pprof_laddr"):
        line = 'pprof_laddr = ""'
    out.append(line)
open(path, "w").write("\n".join(out) + "\n")
PY
  done
  "$TNB" init-devnet "${HOMES[@]}" --keys "$ROOT/keys" --symbol "${SYMBOL:-TENEBERA}"
else
  N=$(ls -d "$ROOT"/node* | wc -l)
  echo "reusing $N-validator devnet in $ROOT (state is rebuilt by replaying blocks)"
fi

mkdir -p "$ROOT/logs"
PIDS=()
cleanup() { echo; echo "stopping devnet"; kill "${PIDS[@]}" 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT INT TERM
for i in $(seq 0 $((N - 1))); do
  "$TNB" start --abci "127.0.0.1:$((26658 + 10 * i))" >"$ROOT/logs/app$i.log" 2>&1 &
  PIDS+=($!)
done
sleep 1
for i in $(seq 0 $((N - 1))); do
  cometbft start --home "$ROOT/node$i" >"$ROOT/logs/cometbft$i.log" 2>&1 &
  PIDS+=($!)
done

echo -n "waiting for blocks"
for _ in $(seq 1 120); do
  if "$TNB" query summary >/dev/null 2>&1; then break; fi
  echo -n "."; sleep 0.5
done
echo
"$TNB" query summary
cat <<INFO

Devnet running ($N validator(s)). Logs: $ROOT/logs/
Try (in another terminal):
  $TNB query summary
  $TNB address --key $ROOT/keys/alice.json
  $TNB keygen --out $ROOT/keys/bob.json
  $TNB tx transfer --key $ROOT/keys/alice.json --to <bob address> --amount 25
  $TNB query balance/<address>
Node i's RPC is http://127.0.0.1:\$((26657 + 10*i)) (pass --rpc to use another node).
Ctrl-C stops everything.
INFO
wait
