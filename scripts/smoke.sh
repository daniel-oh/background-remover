#!/usr/bin/env sh
# Runs the built binary as a real process and checks it survives what unit
# tests cannot: a corrupt JPEG must be a 500, not a dead process (tests are
# always built with unwinding, so only the shipped binary shows a panic
# strategy mistake). Needs MODEL_PATH; curl and python3 on PATH.
#
#   scripts/smoke.sh target/release/background-remover
set -eu
BIN=${1:-target/release/background-remover}
PORT=${SMOKE_PORT:-7077}
: "${MODEL_PATH:?set MODEL_PATH to the isnet-general-use model}"
TMP=$(mktemp -d)
trap 'kill $PID 2>/dev/null; rm -rf "$TMP"' EXIT

python3 - "$TMP/junk.jpg" <<'PY'
import sys
x = 0x12345678
out = bytearray([0xFF, 0xD8, 0xFF, 0xE0])
for _ in range(5000):
    x = (x * 1664525 + 1013904223) & 0xFFFFFFFF
    out.append(x >> 24)
open(sys.argv[1], "wb").write(out)
PY

PORT=$PORT BIND=127.0.0.1 "$BIN" >"$TMP/log" 2>&1 &
PID=$!
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
  sleep 0.2
done

check() { # name expected-status request...
  name=$1; want=$2; shift 2
  got=$(curl -s -o /dev/null -w '%{http_code}' --max-time 90 "$@") || got=000
  if [ "$got" != "$want" ]; then
    echo "smoke: $name: expected $want, got $got"; cat "$TMP/log"; exit 1
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "smoke: $name: the process died"; cat "$TMP/log"; exit 1
  fi
  echo "smoke: $name: $got"
}

check "good jpeg"     200 --data-binary @testdata/sample.jpg -H 'content-type: image/jpeg' "http://127.0.0.1:$PORT/remove"
check "corrupt jpeg"  500 --data-binary "@$TMP/junk.jpg"     -H 'content-type: image/jpeg' "http://127.0.0.1:$PORT/remove"
check "corrupt png"   500 --data-binary "@$TMP/junk.jpg"     -H 'content-type: image/png'  "http://127.0.0.1:$PORT/remove"
check "empty body"    400 -X POST                            -H 'content-type: image/jpeg' "http://127.0.0.1:$PORT/remove"
check "health after"  200 "http://127.0.0.1:$PORT/health"
check "webp"          200 --data-binary @testdata/sample.jpg -H 'content-type: image/jpeg' "http://127.0.0.1:$PORT/remove?format=webp"
check "mask"          200 --data-binary @testdata/sample.jpg -H 'content-type: image/jpeg' "http://127.0.0.1:$PORT/remove?mask=1"
echo "smoke: ok"
