#!/usr/bin/env python3
"""End-to-end WebSocket punch test.

Opens two WS connections to ws://127.0.0.1:21118:
  * peer A registers as id "A001"
  * peer B registers as id "B002"
  * A sends PunchHoleRequest{id="B002"}
Expected:
  * A receives RelayResponse  (top-level field 19, tag 0x9a 0x01)
  * B receives RequestRelay   (top-level field 18, tag 0x92 0x01)

Uses only stdlib + raw RFC 6455 framing. Reuses the helpers from
smoke_ws_register.py via import-by-path.
"""
import asyncio
import os
import socket
import struct
import sys
import base64
import hashlib
import threading
import queue
import time

HOST = "127.0.0.1"
PORT = 21118

# ---- protobuf helpers ----------------------------------------------------
def varint(n):
    out = bytearray()
    while n >= 0x80:
        out.append((n & 0x7f) | 0x80); n >>= 7
    out.append(n & 0x7f); return bytes(out)
def tag(field, wire): return varint((field << 3) | wire)
def lend(field, payload): return tag(field, 2) + varint(len(payload)) + payload
def sfield(field, s): return lend(field, s.encode())
def vfield(field, n): return tag(field, 0) + varint(n)

def build_register_peer(id_): return lend(6, sfield(1, id_) + vfield(2, 0))
def build_punch_hole_request(target): return lend(8, sfield(1, target))

# ---- WS client -----------------------------------------------------------
class WS:
    def __init__(self, sock, prebuf):
        self.sock = sock
        self.pre = prebuf

def ws_connect(host, port):
    s = socket.create_connection((host, port), timeout=3)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET / HTTP/1.1\r\nHost: {host}:{port}\r\n"
        f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    s.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        c = s.recv(4096)
        if not c: raise RuntimeError("ws closed during handshake")
        buf += c
    head, _, rest = buf.partition(b"\r\n\r\n")
    if b"101" not in head.split(b"\r\n", 1)[0]:
        raise RuntimeError(f"bad handshake: {head!r}")
    return WS(s, rest)

def ws_send(ws, data):
    h = bytearray([0x82])  # FIN + binary
    n = len(data)
    mask = os.urandom(4)
    if n < 126:                    h.append(0x80 | n)
    elif n < (1 << 16):            h.append(0x80 | 126); h += struct.pack(">H", n)
    else:                          h.append(0x80 | 127); h += struct.pack(">Q", n)
    h += mask
    payload = bytes(b ^ mask[i & 3] for i, b in enumerate(data))
    ws.sock.sendall(bytes(h) + payload)

def ws_recv(ws, timeout=2.0):
    ws.sock.settimeout(timeout)
    buf = bytearray(ws.pre); ws.pre = b""
    def need(n):
        while len(buf) < n:
            c = ws.sock.recv(4096)
            if not c: raise RuntimeError("ws closed mid-frame")
            buf.extend(c)
    need(2)
    b1, b2 = buf[0], buf[1]
    masked = b2 & 0x80
    n = b2 & 0x7f
    cur = 2
    if n == 126:   need(cur + 2); n = struct.unpack(">H", bytes(buf[cur:cur+2]))[0]; cur += 2
    elif n == 127: need(cur + 8); n = struct.unpack(">Q", bytes(buf[cur:cur+8]))[0]; cur += 8
    if masked:     need(cur + 4); m = bytes(buf[cur:cur+4]); cur += 4
    else:          m = None
    need(cur + n)
    body = bytes(buf[cur:cur+n])
    if m: body = bytes(b ^ m[i & 3] for i, b in enumerate(body))
    ws.pre = bytes(buf[cur+n:])
    return body

def first_field(body):
    """Return the wire field number of the first oneof in this RendezvousMessage."""
    if not body: return None
    return body[0] >> 3  # works for fields 1..15 anyway

def main():
    a = ws_connect(HOST, PORT)
    b = ws_connect(HOST, PORT)
    print("both ws connected", file=sys.stderr)

    ws_send(a, build_register_peer("A001"))
    ws_send(b, build_register_peer("B002"))
    ra = ws_recv(a); rb = ws_recv(b)
    print(f"A register reply: {ra.hex()}", file=sys.stderr)
    print(f"B register reply: {rb.hex()}", file=sys.stderr)
    assert first_field(ra) == 7, "A did not get RegisterPeerResponse"
    assert first_field(rb) == 7, "B did not get RegisterPeerResponse"

    # A asks for B
    ws_send(a, build_punch_hole_request("B002"))
    print("A sent PunchHoleRequest target=B002", file=sys.stderr)

    # B should receive RequestRelay (field 18, tag 0x92 = (18<<3|2))
    push_b = ws_recv(b, timeout=2.0)
    print(f"B got: {push_b.hex()}", file=sys.stderr)
    # field 18 → tag varint = 0x92 0x01
    assert push_b[:2] == b"\x92\x01", f"B did not get RequestRelay (got {push_b[:2].hex()})"
    print("OK: B received RequestRelay", file=sys.stderr)

    # B replies with RelayResponse to ack
    relay_response = lend(19, b"")  # empty payload is fine for the smoke
    ws_send(b, relay_response)
    print("B sent RelayResponse", file=sys.stderr)

    # A should receive RelayResponse (field 19 → tag 0x9a 0x01)
    push_a = ws_recv(a, timeout=2.0)
    print(f"A got: {push_a.hex()}", file=sys.stderr)
    assert push_a[:2] == b"\x9a\x01", f"A did not get RelayResponse (got {push_a[:2].hex()})"
    print("OK: A received RelayResponse", file=sys.stderr)

    print("ALL GOOD: end-to-end ws punch works", file=sys.stderr)
    return 0

if __name__ == "__main__":
    try:
        sys.exit(main())
    except AssertionError as e:
        print(f"FAIL: {e}", file=sys.stderr); sys.exit(2)
    except Exception as e:
        print(f"ERROR: {e}", file=sys.stderr); sys.exit(1)
