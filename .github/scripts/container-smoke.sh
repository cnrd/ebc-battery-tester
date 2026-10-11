#!/usr/bin/env bash
# Usage: CONTAINER_ENGINE=podman bash .github/scripts/container-smoke.sh IMAGE
# Test only the mock device. Never pass through a physical serial device.
set -euo pipefail
IMAGE=${1:?Usage: container-smoke.sh IMAGE}
ENGINE=${CONTAINER_ENGINE:-docker}
NAME="ebc-container-smoke-$$"
PORT=${SMOKE_PORT:-18080}
BASE="http://127.0.0.1:$PORT"
# CI uses RUNNER_TEMP; local callers should set SMOKE_TMPDIR to workspace scratch.
DATA=$(mktemp -d "${SMOKE_TMPDIR:-${RUNNER_TEMP:-${TMPDIR:-.}}}/ebc-smoke.XXXXXX")
DATA=$(realpath "$DATA")
chmod 0777 "$DATA"
cleanup() {
  "$ENGINE" logs "$NAME" 2>/dev/null || true
  "$ENGINE" rm -f "$NAME" >/dev/null 2>&1 || true
  # Container-created files may belong to the mapped non-root UID. Remove only
  # this test's private directory using the same UID before host cleanup.
  "$ENGINE" run --rm --user 10001:10001 --entrypoint /bin/sh \
    -v "$DATA:/data" "$IMAGE" -c 'find /data -mindepth 1 -delete' || true
  rmdir "$DATA" || true
}
trap cleanup EXIT
run_server() {
  "$ENGINE" run -d --name "$NAME" \
    --user 10001:10001 \
    -e EBC_MOCK=true -e EBC_MDNS=false \
    -p "127.0.0.1:$PORT:8080" \
    -v "$DATA:/data" "$IMAGE"
  for _ in $(seq 1 180); do
    if [ "$("$ENGINE" inspect --format '{{.State.Health.Status}}' "$NAME")" = healthy ]; then return; fi
    sleep 1
  done
  "$ENGINE" logs "$NAME"
  return 1
}
run_server

HTML=$(curl --fail --silent --show-error "$BASE/")
curl --fail --silent --show-error "$BASE/manifest.json" >/dev/null
JS=$(printf '%s' "$HTML" | grep -oE '[A-Za-z0-9_./-]+[-.][a-f0-9]{8,}\.js' | sed -n '1p')
WASM=$(printf '%s' "$HTML" | grep -oE '[A-Za-z0-9_./-]+[-.][a-f0-9]{8,}(_bg)?\.wasm' | sed -n '1p')
test -n "$JS" && test -n "$WASM"
JS=${JS#./}; JS=${JS#/}
WASM=${WASM#./}; WASM=${WASM#/}
curl --fail --silent --show-error "$BASE/$JS" >/dev/null
curl --fail --silent --show-error "$BASE/$WASM" >/dev/null
curl --fail --silent --show-error "$BASE/api/status" >/dev/null

COMMAND=(-H 'X-EBC-Command: 1')
curl --fail --silent --show-error -X POST "${COMMAND[@]}" "$BASE/api/connect" >/dev/null
for _ in $(seq 1 100); do
  STATUS=$(curl --fail --silent --show-error "$BASE/api/status")
  if printf '%s' "$STATUS" | grep -q '"activity_known":true' && \
     printf '%s' "$STATUS" | grep -q '"active":false'; then break; fi
  sleep 0.1
done
printf '%s' "$STATUS" | grep -q '"activity_known":true'
printf '%s' "$STATUS" | grep -q '"active":false'
curl --fail --silent --show-error -X POST "${COMMAND[@]}" \
  -H 'Content-Type: application/json' \
  --data '{"config":{"mode":"discharge_constant_current","current_ma":100,"cutoff_voltage_mv":3000,"cutoff_time_min":0},"name":null}' \
  "$BASE/api/test/start" >/dev/null
sleep 3
curl --fail --silent --show-error -X POST "${COMMAND[@]}" "$BASE/api/test/stop" >/dev/null
for _ in $(seq 1 100); do
  STATUS=$(curl --fail --silent --show-error "$BASE/api/status")
  if printf '%s' "$STATUS" | grep -q 'stopped'; then break; fi
  sleep 0.1
done
printf '%s' "$STATUS" | grep -q 'stopped'
# Capture before grep: avoid SIGPIPE false failures once history grows larger.
HISTORY=$(curl --fail --silent --show-error "$BASE/api/history.csv")
printf '%s' "$HISTORY" | grep -q ',100,'

"$ENGINE" rm -f "$NAME" >/dev/null
run_server
curl --fail --silent --show-error "$BASE/" >/dev/null
curl --fail --silent --show-error "$BASE/manifest.json" >/dev/null
STATUS=$(curl --fail --silent --show-error "$BASE/api/status")
printf '%s' "$STATUS" | grep -q 'stopped'
HISTORY=$(curl --fail --silent --show-error "$BASE/api/history.csv")
printf '%s' "$HISTORY" | grep -q ',100,'
echo "Production runtime and /data replacement smoke passed: $IMAGE"
