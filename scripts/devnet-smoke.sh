#!/usr/bin/env bash
# End-to-end smoke test: start a fresh devnet, send a transfer through
# CometBFT, check it was executed and its fee burned, then stop.
#   scripts/devnet-smoke.sh [validators]
set -euo pipefail
N=${1:-1}
REPO=$(cd "$(dirname "$0")/.." && pwd)
export DEVNET_DIR=${DEVNET_DIR:-$(mktemp -d)}
TNB="$REPO/target/release/tenebra-node"

"$REPO/scripts/devnet.sh" --reset "$N" >"$DEVNET_DIR.out" 2>&1 &
DEVNET=$!
trap 'kill $DEVNET 2>/dev/null; wait $DEVNET 2>/dev/null || true' EXIT

for _ in $(seq 1 300); do
  grep -q "Devnet running" "$DEVNET_DIR.out" 2>/dev/null && break
  kill -0 $DEVNET 2>/dev/null || break
  sleep 1
done
grep -q "Devnet running" "$DEVNET_DIR.out" || { cat "$DEVNET_DIR.out"; echo "devnet did not start"; exit 1; }

BOB=$("$TNB" keygen --out "$DEVNET_DIR/keys/bob.json")
"$TNB" tx transfer --key "$DEVNET_DIR/keys/alice.json" --to "$BOB" --amount 7.25 >/dev/null
BAL=$("$TNB" query "balance/$BOB" | python3 -c "import sys,json; print(json.load(sys.stdin)['balance'])")
BURNED=$("$TNB" query summary | python3 -c "import sys,json; print(json.load(sys.stdin)['total_fees_burned'])")
[ "$BAL" = "7250000000" ] || { echo "unexpected balance $BAL"; exit 1; }
[ "$BURNED" != "0" ] || { echo "no fee burned"; exit 1; }
echo "devnet smoke test passed ($N validator(s)): bob=$BAL, fees burned=$BURNED"
