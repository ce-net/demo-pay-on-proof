#!/usr/bin/env bash
# Replay the committed run from this clone with the same seed and compare the report.
# Exits non-zero if the new report or cheat ledger differs from proof/, or the audit fails.
set -euo pipefail
cd "$(dirname "$0")/.."
SEED="${SEED:-68}"; OUT=runs/replay; PORT="${PORT:-7900}"; SIM_PORT="${SIM_PORT:-7878}"
rm -rf "$OUT"; mkdir -p "$OUT"
cargo build --release --locked
BIN=target/release/pay-on-proof
[ -x "$BIN" ] || BIN="${CARGO_TARGET_DIR:-target}/release/pay-on-proof"
"$BIN" board --seed "$SEED" --port "$PORT" --sim "http://127.0.0.1:$SIM_PORT/mcp" --report "$OUT/report.json" --exit-when-done > "$OUT/board.log" 2>&1 &
board=$!
PACE=0 SEED="$SEED" OUT="$OUT" SIM_PORT="$SIM_PORT" scripts/workers.sh > "$OUT/workers.log" 2>&1 || { cat "$OUT/board.log" "$OUT/workers.log"; exit 1; }
wait "$board" || true
diff -u proof/report.json "$OUT/report.json"
diff -u proof/cheats.jsonl "$OUT/cheats.jsonl"
"$BIN" audit --report "$OUT/report.json" --cheats "$OUT/cheats.jsonl"
echo "replay matches proof/report.json and proof/cheats.jsonl byte for byte"
