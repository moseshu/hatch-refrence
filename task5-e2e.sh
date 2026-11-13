#!/usr/bin/env bash
# Task 5 end-to-end (real binary + file home, script models only):
# CLI task list/show vs daemon API GET /v1/tasks, GET /v1/tasks/{id},
# SSE replay, and POST /v1/tasks/{id}/cancel. No --live, no real sends.
set -u
cd ~/workspace/hatch-refrence-dev || exit 1
EXE=./target/debug/hatch
HOME_DIR=$(mktemp -d ~/workspace/.tmp-task5e2e-XXXXXX)
OUT_DIR=$(mktemp -d ~/workspace/.tmp-task5e2e-out-XXXXXX)
echo "home=$HOME_DIR out=$OUT_DIR"
fail=0
check() { # check <name> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "PASS: $1"; else echo "FAIL: $1 (want [$2] got [$3])"; fail=1; fi
}

# 1. CLI: create + run a brief task to its approval (script model).
ID=$("$EXE" --home "$HOME_DIR" task create --title "E2E 简报" --kind meeting-brief | head -1 | awk '{print $2}')
echo "task=$ID"
"$EXE" --home "$HOME_DIR" task run --world eval/worlds/meeting-brief --out "$OUT_DIR" --task "$ID" >/dev/null 2>&1
CLI_LIST=$("$EXE" --home "$HOME_DIR" task list)
echo "$CLI_LIST" | grep -q "$ID" && echo "PASS: CLI list contains task" || { echo "FAIL: CLI list"; fail=1; }
"$EXE" --home "$HOME_DIR" task show "$ID" | grep -q "artifact" && echo "PASS: CLI show has plan card" || { echo "FAIL: CLI show"; fail=1; }

# 2. Start the real daemon API on loopback.
LOG="$HOME_DIR/daemon.log"
HATCH_DAEMON_API_TOKEN=e2e-task5-token "$EXE" --home "$HOME_DIR" daemon run --api --api-bind 127.0.0.1:0 --tick-secs 3600 2>"$LOG" &
DAEMON_PID=$!
PORT=""
for _ in $(seq 1 300); do
  if [ -f "$LOG" ]; then PORT=$(grep -o 'http://127.0.0.1:[0-9]*/v1' "$LOG" | head -1 | sed 's/.*://; s/\/v1//'); [ -n "$PORT" ] && break; fi
  sleep 0.1
done
[ -n "$PORT" ] && echo "PASS: daemon API on port $PORT" || { echo "FAIL: daemon did not start"; cat "$LOG"; fail=1; }
AUTH=(-H "Authorization: Bearer e2e-task5-token")
BASE="http://127.0.0.1:$PORT"

# 3. API list matches CLI list (same task, same status, same steps).
API_LIST=$(curl -s "${AUTH[@]}" "$BASE/v1/tasks")
echo "$API_LIST" | grep -q "$ID" && echo "PASS: API list contains task" || { echo "FAIL: API list"; fail=1; }
API_STATUS=$(echo "$API_LIST" | python3 -c "import json,sys; print([t for t in json.load(sys.stdin)['tasks'] if t['id']=='$ID'][0]['status'])")
check "API list status = blocked" "blocked" "$API_STATUS"
API_STEPS=$(echo "$API_LIST" | python3 -c "import json,sys; t=[t for t in json.load(sys.stdin)['tasks'] if t['id']=='$ID'][0]; print(f\"{t['steps_done']}/{t['steps_total']}\")")
check "API list steps = 2/3" "2/3" "$API_STEPS"

# 4. API detail matches CLI show (plan version + current step).
API_DETAIL=$(curl -s "${AUTH[@]}" "$BASE/v1/tasks/$ID")
API_VER=$(echo "$API_DETAIL" | python3 -c "import json,sys; print(json.load(sys.stdin)['plan_version'])")
check "API detail plan_version = 1" "1" "$API_VER"
API_CUR=$(echo "$API_DETAIL" | python3 -c "import json,sys; print(json.load(sys.stdin)['current_step'])")
check "API detail current_step = artifact" "artifact" "$API_CUR"

# 5. SSE: full replay sends each event once; reconnect at last id sends none.
SSE1=$(curl -s --max-time 3 "${AUTH[@]}" -H "Accept: text/event-stream" -H "Last-Event-ID: 0" "$BASE/v1/tasks/$ID/events")
N1=$(echo "$SSE1" | grep -c "event: task_event")
echo "SSE replay frames: $N1"
[ "$N1" -ge 3 ] && echo "PASS: SSE replayed >=3 events" || { echo "FAIL: SSE replay frames=$N1"; fail=1; }
DUP=$(echo "$SSE1" | grep "event: task_event" -A 0 | grep -o "id: [0-9]*" | sort | uniq -d | wc -l)
check "SSE replay has no duplicate ids" "0" "$DUP"
LAST_ID=$(echo "$SSE1" | grep -o "id: [0-9]*" | tail -1 | awk '{print $2}')
SSE2=$(curl -s --max-time 2 "${AUTH[@]}" -H "Accept: text/event-stream" -H "Last-Event-ID: $LAST_ID" "$BASE/v1/tasks/$ID/events")
N2=$(echo "$SSE2" | grep -c "event: task_event" || true)
check "SSE reconnect at last id sends 0 frames" "0" "$N2"

# 6. API cancel matches CLI cancel.
CANCEL=$(curl -s -X POST "${AUTH[@]}" -H "Content-Type: application/json" -d '{"note": "e2e cancel"}' "$BASE/v1/tasks/$ID/cancel")
echo "$CANCEL" | grep -q "已取消" && echo "PASS: API cancel confirmed" || { echo "FAIL: API cancel: $CANCEL"; fail=1; }
"$EXE" --home "$HOME_DIR" task show "$ID" | head -1 | grep -q "cancelled\|已取消" && echo "PASS: CLI show sees cancelled" || { echo "FAIL: CLI show after API cancel"; fail=1; }

# 7. Stop the daemon.
"$EXE" --home "$HOME_DIR" daemon stop --wait-secs 15 >/dev/null 2>&1
wait "$DAEMON_PID" 2>/dev/null
rm -rf "$HOME_DIR" "$OUT_DIR"
if [ "$fail" = 0 ]; then echo "E2E: ALL PASS"; else echo "E2E: FAILURES PRESENT"; fi
exit "$fail"
