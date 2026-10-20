#!/bin/bash
# =============================================================================
# Nordkraft dev-node smoke test
#
# Drives a running dev node (nordkraft-node --dev) through the CLI and the
# REST API and fails on the first wrong answer. No source changes needed;
# runs the same on a laptop, inside OrbStack, or on a CI runner.
#
#   bash scripts/smoke-dev.sh                       # defaults below
#   NK=./client-cli/target/release/nordkraft \
#   API=http://127.0.0.1:8001/api \
#   HOST=nordkraft.orb.local bash scripts/smoke-dev.sh
#
#   IMAGE   image to deploy (default nginx:alpine; must serve HTTP on :80)
#   EXPECT  string the served page must contain (default "nginx")
#   RESET   command that resets the node (default: sudo install-node.sh --dev --reset)
#   SNAP    directory with JSON snapshots to compare against (default tests/snapshots)
#   UPDATE_SNAPSHOTS=1   rewrite snapshots instead of comparing
# =============================================================================
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
NK="${NK:-nordkraft}"
API="${API:-http://127.0.0.1:8001/api}"
HOST="${HOST:-127.0.0.1}"
IMAGE="${IMAGE:-nginx:alpine}"
EXPECT="${EXPECT:-nginx}"
RESET="${RESET:-sudo bash $HERE/install-node.sh --dev --reset}"
SNAP="${SNAP:-$HERE/../tests/snapshots}"

export HOME="${HOME:-/tmp}"
export API_BASE_URL="$API"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  ✓ $1"; }
fail() { FAIL=$((FAIL+1)); echo "  ✗ $1"; echo "    $2"; }
step() { echo; echo "── $1"; }
need() { command -v "$1" >/dev/null || { echo "missing: $1"; exit 2; }; }
need curl; need jq; need python3

# wait until the API answers (up to 60 s)
wait_api() {
  for _ in $(seq 1 60); do
    curl -fsS "$API/status" >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "API never came up at $API"; exit 2
}

# curl a URL until it serves EXPECT (up to 20 s)
wait_http() {
  for _ in $(seq 1 20); do
    if curl -fsS -m 3 "$1" 2>/dev/null | grep -qi "$EXPECT"; then return 0; fi
    sleep 1
  done
  return 1
}

# normalise a JSON doc for snapshotting: drop values that change every run,
# and keep only Nordkraft's own container labels (images and the runtime add
# their own, e.g. nginx sets maintainer and stop-signal).
norm() {
  jq -S '
    walk(
      if type == "object" then
        (if has("labels") and (.labels | type) == "object" then
           .labels |= with_entries(select(.key | test("^(app_|managed_by$|owner_pubkey$|tenant_id$|user_|created_at$)")))
         else . end)
        | with_entries(
          select(.key | test("^(nerdctl|containerd\\.io|io\\.containerd)[/.]") | not)
          | if (.key | IN("timestamp","created_at","updated_at","started_at","finished_at",
                          "container_id","name","container_name","hostname","container_ip","image","app_image",
                          "version","node_id","log-uri","state-dir")) then .value = "<var>"
            elif (.key == "access_url" or .key == "access_urls" or .key == "ipv6_url") then .value = "<var>"
            elif (.key == "status" and (.value | type) == "string" and (.value | test("^(Up|Exited)"))) then .value = "<state>"
            else . end)
      else . end)'
}

snapshot() { # name, json-on-stdin
  local name="$1" file="$SNAP/$1.json" got
  got=$(norm)
  mkdir -p "$SNAP"
  if [[ "${UPDATE_SNAPSHOTS:-}" == "1" || ! -f "$file" ]]; then
    printf '%s\n' "$got" > "$file"; ok "snapshot $name written"
  elif diff -u "$file" <(printf '%s\n' "$got") > /tmp/snap.diff; then
    ok "snapshot $name unchanged"
  else
    fail "snapshot $name changed" "$(head -20 /tmp/snap.diff)"
  fi
}

names() { "$NK" --json list 2>/dev/null | jq -r '.[].name'; }
count() { "$NK" --json list 2>/dev/null | jq 'length'; }

# =============================================================================
step "0 · preflight"
wait_api
ST=$(curl -fsS "$API/status")
[[ "$(jq -r .dev <<<"$ST")" == "true" ]] && ok "node is in dev mode" || fail "node is not in dev mode" "$ST"
[[ "$(jq -r .network_mode <<<"$ST")" == "bridge" ]] && ok "bridge networking" || fail "expected bridge networking" "$ST"
snapshot status <<<"$ST"

"$NK" --json status | jq -e '.status == "online"' >/dev/null && ok "CLI reaches the API" || fail "CLI cannot reach the API" "$("$NK" status 2>&1 | head -3)"

# clean slate for the run
for n in $(names); do "$NK" -y rm --force "$n" >/dev/null 2>&1 || true; done
[[ "$(count)" == "0" ]] && ok "no containers at start" || fail "could not clean up" "$(names)"

# =============================================================================
step "1 · deploy, published port answers"
OUT=$("$NK" -y --json deploy "$IMAGE" -p 80)
N1=$(jq -r .container_name <<<"$OUT")
URL1=$(jq -r '.access_urls[0]' <<<"$OUT")
[[ -n "$N1" && "$N1" != "null" ]] && ok "deployed $N1" || fail "deploy returned no name" "$OUT"
[[ "$URL1" == http://* ]] && ok "deploy returned an access URL ($URL1)" || fail "no access_urls in deploy response" "$OUT"
# the URL carries PUBLIC_HOST; from outside the VM we may need HOST instead
URL1=${URL1//127.0.0.1/$HOST}
wait_http "$URL1" && ok "page served at $URL1" || fail "nothing served at $URL1" "$(curl -s -m 3 "$URL1" | head -3)"

# =============================================================================
step "2 · second container on the same port gets a new host port"
OUT=$("$NK" -y --json deploy "$IMAGE" -p 80)
N2=$(jq -r .container_name <<<"$OUT")
URL2=$(jq -r '.access_urls[0]' <<<"$OUT"); URL2=${URL2//127.0.0.1/$HOST}
[[ "$URL2" != "$URL1" ]] && ok "distinct URL ($URL2)" || fail "second container reused the same host port" "$OUT"
wait_http "$URL2" && ok "second page served" || fail "nothing served at $URL2" ""
wait_http "$URL1" && ok "first page still served" || fail "first container broke" ""
[[ "$(count)" == "2" ]] && ok "list shows 2" || fail "list count wrong" "$(count)"
LIST=$("$NK" --json list)
[[ "$(jq -r '[.[].ports[0].access_url] | unique | length' <<<"$LIST")" == "2" ]] && ok "list has 2 distinct access_urls" || fail "list access_urls not distinct" "$LIST"
snapshot list-two <<<"$LIST"

# =============================================================================
step "3 · logs, stop, start"
"$NK" --json logs "$N1" -n 5 2>/dev/null | jq -e '.logs | length > 0' >/dev/null && ok "logs returned" || fail "no logs" "$("$NK" logs "$N1" 2>&1 | head -3)"
"$NK" -y stop "$N1" >/dev/null && ok "stopped $N1" || fail "stop failed" ""
sleep 1
curl -fsS -m 2 "$URL1" >/dev/null 2>&1 && fail "still serving after stop" "" || ok "not served while stopped"
"$NK" -y start "$N1" >/dev/null && ok "started $N1" || fail "start failed" ""
wait_http "$URL1" && ok "served again after start, same URL" || fail "not served after start" ""

# =============================================================================
step "4 · inspect + REST direct"
INS=$(curl -fsS "$API/containers/$N1")
[[ "$(jq -r .status <<<"$INS")" == "running" ]] && ok "inspect: running" || fail "inspect status" "$INS"
snapshot inspect <<<"$INS"
curl -fsS -X POST "$API/containers/$N2/stop" | jq -e '.status == "stopped"' >/dev/null && ok "REST stop" || fail "REST stop" ""
curl -fsS -X POST "$API/containers/$N2/start" | jq -e '.status == "started"' >/dev/null && ok "REST start" || fail "REST start" ""

# =============================================================================
step "4b · exec, shell stream, wait, lifecycle"
# nginx:alpine has sh; the exec runs inside the running container N1.
EX=$("$NK" --json exec "$N1" -- sh -c 'echo out; echo err >&2; exit 3' 2>/dev/null || true)
[[ "$(jq -r .exit_code <<<"$EX")" == "3" ]] && ok "exec returns exit code" || fail "exec exit_code" "$EX"
[[ "$(jq -r .stdout <<<"$EX")" == "out" ]] && ok "exec stdout" || fail "exec stdout" "$EX"
[[ "$(jq -r .stderr <<<"$EX")" == "err" ]] && ok "exec stderr separate" || fail "exec stderr" "$EX"
RC=0; "$NK" exec "$N1" -- sh -c 'exit 7' >/dev/null 2>&1 || RC=$?; [[ $RC -eq 7 ]] && ok "CLI exit code mirrors the process" || fail "CLI exec exit code" "rc=$RC"
printf 'piped\n' | "$NK" exec "$N1" --stdin -- cat | grep -q '^piped$' && ok "exec --stdin" || fail "exec --stdin" ""
RC=0; "$NK" exec "$N1" --timeout 1 -- sleep 5 >/dev/null 2>&1 || RC=$?; [[ $RC -eq 124 ]] && ok "exec timeout → 124" || fail "exec timeout" "rc=$RC"
# The relay leg: the controller forwards exec to the agent's internal stream.
# A hybrid dev node mounts that route too, so drive it directly (dev mode
# trusts loopback) and make sure the agent — not just the controller —
# enforces the timeout. Regression: timeout_s was not forwarded, so a
# remote `sleep 10` with timeout 2 ran for the full 10 s.
python3 - "$API" "$N1" <<'PY' && ok "relay stream enforces timeout_s on the agent" || fail "relay stream timeout" ""
import asyncio, json, sys, time
try:
    import websockets
except ImportError:
    sys.exit(0)
api, name = sys.argv[1], sys.argv[2]
url = api.replace("http://", "ws://", 1) + f"/internal/containers/{name}/stream?tty=false&owner=dev_key_placeholder&timeout_s=2&cmd=sleep&cmd=10"
async def main():
    t0 = time.time()
    async with websockets.connect(url) as ws:
        await ws.send(bytes([0]))  # stdin EOF
        status = None
        async for msg in ws:
            if msg[0] == 3: status = json.loads(msg[1:])
    took = time.time() - t0
    assert status and status.get("exit_code") is None and "timeout" in status.get("error", ""), status
    assert took < 8, f"took {took:.1f}s"
asyncio.run(main())
PY
EX=$(curl -fsS -X POST "$API/containers/$N1/exec" -H 'Content-Type: application/json' -d '{"cmd":["sh","-c","pwd; echo $X"],"cwd":"/tmp","env":{"X":"y"}}')
[[ "$(jq -r .stdout <<<"$EX")" == "/tmp"$'\n'"y" ]] && ok "REST exec honours cwd + env" || fail "REST exec cwd/env" "$EX"
curl -s -o /dev/null -w '%{http_code}' -X POST "$API/containers/does-not-exist/exec" -H 'Content-Type: application/json' -d '{"cmd":["true"]}' | grep -q 404 && ok "exec on unknown container → 404" || fail "exec 404" ""
# A missing binary is the user's result, reported like a shell: exit 127 on
# stderr, no runtime internals, nothing above debug in the node log.
EX=$(curl -fsS -X POST "$API/containers/$N1/exec" -H 'Content-Type: application/json' -d '{"cmd":["no-such-binary-xyz","--flag"]}')
[[ "$(jq -r .exit_code <<<"$EX")" == "127" && "$(jq -r .stderr <<<"$EX")" == "sh: no-such-binary-xyz: not found" ]] && ok "missing binary → exit 127, shell-style stderr" || fail "missing binary reporting" "$EX"
jq -e '.stderr | test("backtrace|containerd|rpc error") | not' <<<"$EX" >/dev/null && ok "no runtime internals in exec output" || fail "runtime internals leaked" "$EX"
# the shell stream: open a tty session over WebSocket, resize, run a command, read the status frame
python3 - "$API" "$N1" <<'PY' && ok "PTY stream: resize seen, exit code delivered" || fail "PTY stream" ""
import asyncio, json, sys
try:
    import websockets
except ImportError:
    sys.exit(0)  # client lib missing: don't fail the suite on a tooling gap
api, name = sys.argv[1], sys.argv[2]
url = api.replace("http://", "ws://", 1) + f"/containers/{name}/stream?tty=1&cmd=sh&cols=80&rows=24"
async def main():
    async with websockets.connect(url) as ws:
        await ws.send(bytes([4]) + json.dumps({"width": 132, "height": 43}).encode())
        await ws.send(bytes([0]) + b"stty size; exit 9\n")
        out, status = b"", None
        async for msg in ws:
            if msg[0] == 1: out += msg[1:]
            elif msg[0] == 3: status = json.loads(msg[1:])
        assert b"43 132" in out, out
        assert status == {"exit_code": 9}, status
asyncio.run(main())
PY
# wait + lifecycle: a short-lived sandbox with labels; wait returns its exit code,
# the label filter finds it, and the reaper removes it after auto_delete.
OUT=$("$NK" -y --json deploy "$IMAGE" --command "sh -c 'sleep 2; exit 5'" --restart never --auto-delete 1s --label smoke=yes)
NJ=$(jq -r .container_name <<<"$OUT")
[[ -n "$NJ" && "$NJ" != "null" ]] && ok "deployed job $NJ" || fail "job deploy" "$OUT"
RC=0; "$NK" wait "$NJ" --timeout 60 >/dev/null 2>&1 || RC=$?; [[ $RC -eq 5 ]] && ok "wait returns the exit code" || fail "wait exit code" "rc=$RC"
"$NK" --json list --label smoke=yes | jq -e --arg n "$NJ" 'map(select(.name == $n)) | length == 1' >/dev/null && ok "list --label finds it" || fail "list --label" ""
"$NK" --json list --label smoke=no | jq -e 'length == 0' >/dev/null && ok "list --label excludes others" || fail "list --label exclude" ""
for _ in $(seq 1 60); do "$NK" --json list | jq -e --arg n "$NJ" 'map(select(.name == $n)) | length == 0' >/dev/null && break; sleep 2; done
"$NK" --json list | jq -e --arg n "$NJ" 'map(select(.name == $n)) | length == 0' >/dev/null && ok "auto_delete removed the job" || fail "auto_delete" "$("$NK" list 2>&1 | head -5)"
curl -s -X POST "$API/containers/deploy" -H 'Content-Type: application/json' -d '{"image":"x","lifecycle":{"auto_stop":"soon"}}' | jq -e '.error | test("invalid lifecycle")' >/dev/null && ok "bad duration rejected" || fail "lifecycle validation" ""
# deploy with wait=true must return only when exec works — no sleep in between.
DW=$(curl -fsS -X POST "$API/containers/deploy" -H 'Content-Type: application/json' -d "{\"image\":\"$IMAGE\",\"wait\":true,\"lifecycle\":{\"restart\":\"never\",\"auto_delete\":\"1h\"}}")
NW=$(jq -r .container_name <<<"$DW")
[[ "$(jq -r .status <<<"$DW")" == "deployed" && -n "$NW" && "$NW" != "null" ]] && ok "deploy wait=true → deployed" || fail "deploy wait" "$DW"
EX=$(curl -fsS -X POST "$API/containers/$NW/exec" -H 'Content-Type: application/json' -d '{"cmd":["sh","-c","echo ready"]}')
[[ "$(jq -r .stdout <<<"$EX")" == "ready" ]] && ok "exec works immediately after deploy wait=true" || fail "exec right after deploy" "$EX"
# the same contract for start: stop it, start with wait=true, exec at once.
curl -fsS -X POST "$API/containers/$NW/stop" >/dev/null
SW=$(curl -fsS -X POST "$API/containers/$NW/start?wait=true&wait_timeout_s=60")
[[ "$(jq -r .status <<<"$SW")" == "running" ]] && ok "start wait=true → running" || fail "start wait" "$SW"
EX=$(curl -fsS -X POST "$API/containers/$NW/exec" -H 'Content-Type: application/json' -d '{"cmd":["sh","-c","echo back"]}')
[[ "$(jq -r .stdout <<<"$EX")" == "back" ]] && ok "exec works immediately after start wait=true" || fail "exec right after start" "$EX"
"$NK" -y rm --force "$NW" >/dev/null 2>&1 || true

# =============================================================================
step "5 · rm --force is fast"
S=$(date +%s)
"$NK" -y rm --force "$N2" >/dev/null && ok "removed $N2" || fail "rm --force failed" ""
D=$(( $(date +%s) - S ))
[[ $D -lt 10 ]] && ok "rm --force took ${D}s" || fail "rm --force too slow" "${D}s"
[[ "$(count)" == "1" ]] && ok "list shows 1" || fail "list count after rm" "$(count)"

# =============================================================================
step "6 · reset wipes everything, node re-seeds"
if $RESET >/dev/null 2>&1; then
  wait_api
  [[ "$(count)" == "0" ]] && ok "no containers after reset" || fail "containers survived reset" "$(names)"
  OUT=$("$NK" -y --json deploy "$IMAGE" -p 80)
  N3=$(jq -r .container_name <<<"$OUT")
  URL3=$(jq -r '.access_urls[0]' <<<"$OUT"); URL3=${URL3//127.0.0.1/$HOST}
  wait_http "$URL3" && ok "deploy works after reset ($URL3)" || fail "deploy after reset" "$OUT"
  "$NK" -y rm --force "$N3" >/dev/null || true
else
  echo "  – reset skipped (RESET command failed or not permitted)"
  "$NK" -y rm --force "$N1" >/dev/null || true
fi

# =============================================================================
echo
echo "══ $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
