#!/usr/bin/env bash
# Start two workers against a running board: one honest, one rigged (--cheat).
#
#   cargo run -- board          # terminal 1
#   ./scripts/workers.sh        # terminal 2
#
# Both run the public ce-net task-worker-loop unchanged (scripts/pay_worker.py only gives it a
# skill). They find the board on a local ce-net sim (ce-net-local-dev-sim) with apps_search.
# Env: PACE (seconds per job, default 0.4), SEED (default 68, must match the board), OUT (default runs).
set -euo pipefail
cd "$(dirname "$0")/.."
PACE="${PACE:-0.4}"; SEED="${SEED:-68}"; OUT="${OUT:-runs}"; SIM_PORT="${SIM_PORT:-7878}"
SIM="http://127.0.0.1:$SIM_PORT/mcp"
scripts/deps.sh
mkdir -p "$OUT"; : > "$OUT/cheats.jsonl"
pids=()
cleanup() { for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

rpc() { # tool json-args -> structuredContent
  python3 - "$SIM" "$1" "$2" <<'PY'
import json, sys, urllib.request
url, tool, args = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
msg = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}
req = urllib.request.Request(url, json.dumps(msg).encode(), {"Content-Type": "application/json"})
print(json.dumps(json.load(urllib.request.urlopen(req, timeout=5))["result"].get("structuredContent")))
PY
}

if ! rpc apps_search '{"q":""}' >/dev/null 2>&1; then
  python3 .deps/ce-net-local-dev-sim/cenet_mock.py "$SIM_PORT" > "$OUT/sim.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 50); do rpc apps_search '{"q":""}' >/dev/null 2>&1 && break; sleep 0.1; done
  rpc apps_search '{"q":""}' >/dev/null 2>&1 || { echo "the local ce-net sim did not start on port $SIM_PORT (in use?); set SIM_PORT" >&2; cat "$OUT/sim.log" >&2; exit 1; }
  echo "started the local ce-net sim on $SIM"
fi

BOARD=""
for _ in $(seq 120); do
  BOARD=$(rpc apps_search '{"q":"job.board"}' 2>/dev/null | python3 -c 'import json,sys
apps=(json.load(sys.stdin) or {}).get("apps") or []
print(next((a["url"] for a in apps if a.get("name")=="job.board"), ""))' 2>/dev/null || true)
  [ -n "$BOARD" ] && break
  sleep 0.5
done
[ -n "$BOARD" ] || { echo "no job.board listed on $SIM: start it first with: cargo run -- board" >&2; exit 1; }
PAGE="${BOARD%/mcp}"
echo "found job.board at $BOARD (live page: $PAGE/)"

state() { python3 -c 'import json,sys,urllib.request
s=json.load(urllib.request.urlopen(sys.argv[1]+"/state.json",timeout=5)); print(sys.argv[2] in ("done",) and str(s["done"]).lower() or len(s["workers"]))' "$PAGE" "$1"; }

run_worker() { # label flags...
  local label=$1; shift
  CE_MCP_URL="$BOARD" CE_SKILL=pay-on-proof CE_KEY_DIR="$(mktemp -d)" WORKER_LOOP_DIR=.deps/ce-net-task-worker-loop \
    CHEAT_SEED="$SEED" CHEAT_LEDGER="$OUT/cheats.jsonl" \
    python3 -u scripts/pay_worker.py --pace "$PACE" "$@" \
    > >(while IFS= read -r l; do printf '[%s] %.110s\n' "$label" "$l"; done) 2>&1 &
  pids+=($!)
}

before=$(state workers)
run_worker honest
for _ in $(seq 100); do [ "$(state workers)" -gt "$before" ] && break; sleep 0.1; done
run_worker rigged --cheat

for _ in $(seq 1200); do [ "$(state done)" = "true" ] && break; sleep 0.5; done
[ "$(state done)" = "true" ] || { echo "board did not finish" >&2; exit 1; }
echo "all jobs checked. injected wrong answers: $(wc -l < "$OUT/cheats.jsonl") (ledger $OUT/cheats.jsonl)"
echo "audit it: cargo run -- audit --report $OUT/report.json --cheats $OUT/cheats.jsonl"
