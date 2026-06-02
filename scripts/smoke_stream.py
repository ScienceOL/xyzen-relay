#!/usr/bin/env python3
"""End-to-end fan-out smoke test for xyzen-stream.

Opens one publisher (ws /ws/stream/<peer>) and two viewers (ws /ws/view/<peer>),
sends 5 binary frames from the publisher, expects each viewer to receive all 5
in order.
"""
import os
import socket
import struct
import sys
import base64
import threading
import time

HOST = os.environ.get("XYZEN_HOST", "127.0.0.1")
PORT = int(os.environ.get("XYZEN_STREAM_PORT", "21130"))


def ws_connect(path: str):
    s = socket.create_connection((HOST, PORT), timeout=3)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET {path} HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
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
    return s, rest


def ws_send_bin(sock, data: bytes):
    h = bytearray([0x82])  # FIN + binary
    n = len(data); m = os.urandom(4)
    if n < 126: h.append(0x80 | n)
    elif n < (1 << 16): h.append(0x80 | 126); h += struct.pack(">H", n)
    else: h.append(0x80 | 127); h += struct.pack(">Q", n)
    h += m
    sock.sendall(bytes(h) + bytes(b ^ m[i & 3] for i, b in enumerate(data)))


def ws_recv_frame(sock, prebuf: bytearray, timeout=2.0):
    sock.settimeout(timeout)
    while True:
        # need 2 bytes
        while len(prebuf) < 2:
            c = sock.recv(4096)
            if not c: raise RuntimeError("closed mid-frame")
            prebuf.extend(c)
        b1, b2 = prebuf[0], prebuf[1]
        opcode = b1 & 0x0f
        masked = b2 & 0x80
        n = b2 & 0x7f
        cur = 2
        while True:
            if n == 126:
                while len(prebuf) < cur + 2:
                    prebuf.extend(sock.recv(4096))
                n = struct.unpack(">H", bytes(prebuf[cur:cur+2]))[0]; cur += 2
            elif n == 127:
                while len(prebuf) < cur + 8:
                    prebuf.extend(sock.recv(4096))
                n = struct.unpack(">Q", bytes(prebuf[cur:cur+8]))[0]; cur += 8
            break
        if masked:
            while len(prebuf) < cur + 4:
                prebuf.extend(sock.recv(4096))
            mask = bytes(prebuf[cur:cur+4]); cur += 4
        else:
            mask = None
        while len(prebuf) < cur + n:
            prebuf.extend(sock.recv(4096))
        body = bytes(prebuf[cur:cur+n])
        if mask:
            body = bytes(b ^ mask[i & 3] for i, b in enumerate(body))
        del prebuf[:cur+n]
        if opcode == 0x9:  # ping
            continue
        if opcode == 0x8:  # close
            return None
        return body


def main():
    peer = "smoke-test-room"
    pub_sock, _ = ws_connect(f"/ws/stream/{peer}")
    print("publisher connected", file=sys.stderr)
    time.sleep(0.1)

    v1_sock, v1_buf = ws_connect(f"/ws/view/{peer}")
    v2_sock, v2_buf = ws_connect(f"/ws/view/{peer}")
    print("two viewers connected", file=sys.stderr)
    time.sleep(0.2)  # let server register subscribers

    frames = [b"frame-%d-payload" % i for i in range(5)]
    for f in frames:
        ws_send_bin(pub_sock, f)
    print("published 5 frames", file=sys.stderr)

    for name, sock, buf in [("v1", v1_sock, bytearray(v1_buf)), ("v2", v2_sock, bytearray(v2_buf))]:
        got = []
        for i in range(5):
            body = ws_recv_frame(sock, buf, timeout=2.0)
            if body is None:
                print(f"FAIL: {name} closed early", file=sys.stderr); return 1
            got.append(body)
        if got != frames:
            print(f"FAIL: {name} got {got!r}", file=sys.stderr); return 1
        print(f"OK: {name} received all 5 frames in order", file=sys.stderr)

    print("ALL GOOD: stream fan-out works", file=sys.stderr)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as e:
        print(f"ERROR: {e}", file=sys.stderr); sys.exit(2)
