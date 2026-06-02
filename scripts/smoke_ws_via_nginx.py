#!/usr/bin/env python3
"""Verify that ws://localhost/ws/id reaches xyzen-rendezvous via nginx."""
import os, socket, struct, sys, base64

HOST = os.environ.get("XYZEN_HOST", "127.0.0.1")
PORT = int(os.environ.get("XYZEN_PORT", "80"))
PATH = os.environ.get("XYZEN_PATH", "/ws/id")

def varint(n):
    o = bytearray()
    while n >= 0x80:
        o.append((n & 0x7f) | 0x80); n >>= 7
    o.append(n & 0x7f); return bytes(o)
def tag(f, w): return varint((f << 3) | w)
def lend(f, p): return tag(f, 2) + varint(len(p)) + p
def sfield(f, s): return lend(f, s.encode())
def vfield(f, n): return tag(f, 0) + varint(n)

# RegisterPeer { id="NGINX01"; serial=0; } in RendezvousMessage.union[6]
inner = sfield(1, "NGINX01") + vfield(2, 0)
payload = lend(6, inner)

s = socket.create_connection((HOST, PORT), timeout=3)
key = base64.b64encode(os.urandom(16)).decode()
req = (
    f"GET {PATH} HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
    f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
    f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
)
s.sendall(req.encode())
buf = b""
while b"\r\n\r\n" not in buf:
    c = s.recv(4096)
    if not c:
        sys.exit("proxy closed during handshake — check /ws/id mapping")
    buf += c
head, _, rest = buf.partition(b"\r\n\r\n")
status_line = head.split(b"\r\n", 1)[0]
print(f"HTTP: {status_line.decode()}", file=sys.stderr)
if b"101" not in status_line:
    print(f"non-101 response head:\n{head.decode(errors='replace')}", file=sys.stderr)
    sys.exit(2)

# send Binary frame
def send_bin(sock, data):
    h = bytearray([0x82])
    n = len(data); m = os.urandom(4)
    if n < 126: h.append(0x80 | n)
    elif n < (1 << 16): h.append(0x80 | 126); h += struct.pack(">H", n)
    else: h.append(0x80 | 127); h += struct.pack(">Q", n)
    h += m
    sock.sendall(bytes(h) + bytes(b ^ m[i & 3] for i, b in enumerate(data)))

send_bin(s, payload)
print(f"sent RegisterPeer ({len(payload)} bytes)", file=sys.stderr)

s.settimeout(3)
buf = bytearray(rest)
def need(n):
    while len(buf) < n:
        c = s.recv(4096)
        if not c: raise RuntimeError("ws closed mid-frame")
        buf.extend(c)
need(2)
b1, b2 = buf[0], buf[1]
n = b2 & 0x7f; cur = 2
if n == 126: need(cur+2); n = struct.unpack(">H", bytes(buf[cur:cur+2]))[0]; cur += 2
elif n == 127: need(cur+8); n = struct.unpack(">Q", bytes(buf[cur:cur+8]))[0]; cur += 8
need(cur + n)
body = bytes(buf[cur:cur+n])
print(f"got {len(body)} bytes: {body.hex()}", file=sys.stderr)
if body[:2] == b"\x3a\x00":
    print("OK: nginx → xyzen-rendezvous WebSocket path works", file=sys.stderr)
    sys.exit(0)
else:
    print(f"unexpected body: {body.hex()}", file=sys.stderr)
    sys.exit(3)
