#!/usr/bin/env bash
# End-to-end smoke test for xyzen-relay-control.
#
# Verifies:
#   1. POST /v1/peers binds a user_id to a fresh 9-digit peer_id
#   2. Posting the same user_id again is idempotent
#   3. GET /v1/peers/:user_id returns the binding
#   4. Trigger a UDP register on rendezvous → audit picks it up
#   5. GET /v1/audit shows the register event
#   6. Bearer-token auth rejects unauthenticated calls
#
# Requires: xyzen-control on :21120, xyzen-rendezvous on :21116, both wired
# with the same XYZEN_CONTROL_TOKEN.

set -euo pipefail

TOKEN="${XYZEN_CONTROL_TOKEN:-dev-secret-test-token}"
CONTROL="${XYZEN_CONTROL_URL:-http://127.0.0.1:21120}"

red()   { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
say()   { printf "\033[2m%s\033[0m\n" "$*"; }

# ---- 1. unauthenticated call must 401 -----------------------------------
say "1) unauth POST /v1/peers (expect 401)"
code=$(curl -s -o /dev/null -w "%{http_code}" -XPOST -H 'content-type: application/json' \
  -d '{"user_id":"alice"}' "$CONTROL/v1/peers")
[[ "$code" == "401" ]] && green "   ok: 401" || { red "   FAIL got $code"; exit 1; }

# ---- 2. bind alice → returns peer_id ------------------------------------
say "2) bind user_id=alice"
resp=$(curl -fsS -XPOST -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"user_id":"alice"}' "$CONTROL/v1/peers")
echo "   $resp"
alice_pid=$(echo "$resp" | python3 -c 'import json,sys;print(json.load(sys.stdin)["peer_id"])')
[[ "${#alice_pid}" == "9" ]] && green "   ok: alice → $alice_pid" || { red "   FAIL bad peer_id"; exit 1; }

# ---- 3. idempotency check -----------------------------------------------
say "3) bind alice again, must return same peer_id"
resp2=$(curl -fsS -XPOST -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"user_id":"alice"}' "$CONTROL/v1/peers")
alice_pid2=$(echo "$resp2" | python3 -c 'import json,sys;print(json.load(sys.stdin)["peer_id"])')
[[ "$alice_pid" == "$alice_pid2" ]] && green "   ok: idempotent" || { red "   FAIL got different ids"; exit 1; }

# ---- 4. bind bob → distinct id ------------------------------------------
say "4) bind bob → distinct id"
resp3=$(curl -fsS -XPOST -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"user_id":"bob"}' "$CONTROL/v1/peers")
bob_pid=$(echo "$resp3" | python3 -c 'import json,sys;print(json.load(sys.stdin)["peer_id"])')
[[ "$bob_pid" != "$alice_pid" ]] && green "   ok: bob → $bob_pid" || { red "   FAIL same id as alice"; exit 1; }

# ---- 5. GET /v1/peers/:user_id ------------------------------------------
say "5) GET /v1/peers/alice"
resp4=$(curl -fsS -H "authorization: Bearer $TOKEN" "$CONTROL/v1/peers/alice")
got_pid=$(echo "$resp4" | python3 -c 'import json,sys;print(json.load(sys.stdin)["peer_id"])')
[[ "$got_pid" == "$alice_pid" ]] && green "   ok: lookup matches" || { red "   FAIL"; exit 1; }

# ---- 6. trigger UDP register, audit must pick it up ---------------------
say "6) UDP-register $alice_pid on rendezvous, then check audit"
python3 - "$alice_pid" <<'PY'
import sys, socket, struct

peer_id = sys.argv[1]

def varint(n):
    out=bytearray()
    while n >= 0x80: out.append((n & 0x7f) | 0x80); n >>= 7
    out.append(n & 0x7f); return bytes(out)
def lend(f, p): return varint((f<<3)|2) + varint(len(p)) + p
def sfield(f,s): return lend(f, s.encode())
def vfield(f,n): return varint((f<<3)|0) + varint(n)

inner = sfield(1, peer_id) + vfield(2, 0)
payload = lend(6, inner)
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(2)
s.sendto(payload, ("127.0.0.1", 21116))
data, _ = s.recvfrom(4096)
assert data[:2] == b"\x3a\x00", f"unexpected reply: {data.hex()}"
print(f"   ok: register reply for {peer_id}")
PY

sleep 0.5  # audit is fire-and-forget over HTTP

say "7) GET /v1/audit?peer_id=$alice_pid"
audit=$(curl -fsS -H "authorization: Bearer $TOKEN" "$CONTROL/v1/audit?peer_id=$alice_pid&limit=10")
echo "   $audit" | python3 -c '
import json, sys
rows = json.load(sys.stdin)
print("   {} rows".format(len(rows)))
for r in rows:
    print("   - kind={} ts={} addr={}".format(r["kind"], r["ts"], r.get("addr")))
'
n=$(echo "$audit" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))')
[[ "$n" -ge "1" ]] && green "   ok: audit sees $n event(s)" || { red "   FAIL: no audit"; exit 1; }

green ""
green "ALL GOOD"
