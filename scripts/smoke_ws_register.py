#!/usr/bin/env python3
"""WebSocket smoke test: connect to ws://127.0.0.1:21118, send a
RegisterPeer message, expect a RegisterPeerResponse back.

Uses only stdlib + the `websockets` library. If `websockets` isn't
installed, falls back to a hand-rolled WS client over a raw socket.
"""
import asyncio
import os
import socket
import struct
import sys
import base64
import hashlib

HOST = os.environ.get("XYZEN_HOST", "127.0.0.1")
PORT = int(os.environ.get("XYZEN_WS_PORT", "21118"))

def varint(n: int) -> bytes:
    out = bytearray()
    while n >= 0x80:
        out.append((n & 0x7f) | 0x80)
        n >>= 7
    out.append(n & 0x7f)
    return bytes(out)

def tag(field: int, wire: int) -> bytes: return varint((field << 3) | wire)
def len_delim(field: int, payload: bytes) -> bytes:
    return tag(field, 2) + varint(len(payload)) + payload
def str_field(field: int, s: str) -> bytes:
    return len_delim(field, s.encode())
def varint_field(field: int, n: int) -> bytes:
    return tag(field, 0) + varint(n)

# RegisterPeer { string id = 1; int32 serial = 2; }
register_peer = str_field(1, "WSTEST001") + varint_field(2, 0)
# RendezvousMessage.register_peer = field 6, len-delimited
payload = len_delim(6, register_peer)
print(f"protobuf payload ({len(payload)} bytes): {payload.hex()}", file=sys.stderr)

# ---- minimal WebSocket client (RFC 6455) ---------------------------------
def ws_handshake(sock, host, port):
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET / HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        f"Upgrade: websocket\r\n"
        f"Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        f"Sec-WebSocket-Version: 13\r\n"
        f"\r\n"
    )
    sock.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise RuntimeError("ws handshake closed")
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    if b"101" not in head.split(b"\r\n", 1)[0]:
        raise RuntimeError(f"bad handshake: {head!r}")
    expected = base64.b64encode(
        hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
    ).decode()
    if expected.encode() not in head:
        raise RuntimeError(f"accept mismatch: {head!r}")
    return rest  # any bytes already in buffer past the response

def ws_send_binary(sock, data: bytes):
    # FIN=1, opcode=0x2 (binary)
    header = bytearray([0x82])
    n = len(data)
    mask = os.urandom(4)
    if n < 126:
        header.append(0x80 | n)
    elif n < (1 << 16):
        header.append(0x80 | 126)
        header += struct.pack(">H", n)
    else:
        header.append(0x80 | 127)
        header += struct.pack(">Q", n)
    header += mask
    masked = bytearray(n)
    for i, b in enumerate(data):
        masked[i] = b ^ mask[i & 3]
    sock.sendall(bytes(header) + bytes(masked))

def ws_recv_frame(sock, prebuf: bytes = b"") -> bytes:
    buf = bytearray(prebuf)
    def need(n):
        nonlocal buf
        while len(buf) < n:
            chunk = sock.recv(4096)
            if not chunk:
                raise RuntimeError("ws closed mid-frame")
            buf += chunk
    need(2)
    b1, b2 = buf[0], buf[1]
    masked = b2 & 0x80
    n = b2 & 0x7f
    cur = 2
    if n == 126:
        need(cur + 2); n = struct.unpack(">H", bytes(buf[cur:cur+2]))[0]; cur += 2
    elif n == 127:
        need(cur + 8); n = struct.unpack(">Q", bytes(buf[cur:cur+8]))[0]; cur += 8
    if masked:
        need(cur + 4); mask = bytes(buf[cur:cur+4]); cur += 4
    else:
        mask = None
    need(cur + n)
    body = bytes(buf[cur:cur+n])
    if mask:
        body = bytes(b ^ mask[i & 3] for i, b in enumerate(body))
    return body

def main():
    sock = socket.create_connection((HOST, PORT), timeout=3)
    rest = ws_handshake(sock, HOST, PORT)
    print("handshake OK", file=sys.stderr)
    ws_send_binary(sock, payload)
    print("sent RegisterPeer", file=sys.stderr)
    sock.settimeout(3)
    body = ws_recv_frame(sock, rest)
    print(f"got {len(body)} bytes: {body.hex()}", file=sys.stderr)
    # Expect tag for field 7 (RegisterPeerResponse), len-delimited = 0x3a
    if body and body[0] == 0x3a:
        print("OK: server replied with RegisterPeerResponse over WS", file=sys.stderr)
        return 0
    print(f"unexpected first byte 0x{body[0]:02x}", file=sys.stderr)
    return 2

if __name__ == "__main__":
    sys.exit(main())
