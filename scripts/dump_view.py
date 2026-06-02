#!/usr/bin/env python3
"""Subscribe to ws://.../ws/view/<peer> for N seconds and dump raw bytes
to a file in the order they arrive.
"""
import os, sys, socket, struct, base64, time

HOST = os.environ.get("XYZEN_HOST", "127.0.0.1")
PORT = int(os.environ.get("XYZEN_STREAM_PORT", "21130"))
peer = sys.argv[1] if len(sys.argv) > 1 else "test123"
secs = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
out_path = sys.argv[3] if len(sys.argv) > 3 else "/tmp/sample.h264"

def ws_connect(path):
    s = socket.create_connection((HOST, PORT), timeout=3)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (f"GET {path} HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
           f"Upgrade: websocket\r\nConnection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
    s.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        c = s.recv(4096)
        if not c: raise RuntimeError("closed")
        buf += c
    head, _, rest = buf.partition(b"\r\n\r\n")
    return s, bytearray(rest)

def recv_frame(s, buf):
    s.settimeout(secs + 5)
    while True:
        while len(buf) < 2:
            c = s.recv(8192)
            if not c: return None
            buf.extend(c)
        b1, b2 = buf[0], buf[1]
        opcode = b1 & 0x0f
        n = b2 & 0x7f
        cur = 2
        if n == 126:
            while len(buf) < cur+2: buf.extend(s.recv(4096))
            n = struct.unpack(">H", bytes(buf[cur:cur+2]))[0]; cur += 2
        elif n == 127:
            while len(buf) < cur+8: buf.extend(s.recv(4096))
            n = struct.unpack(">Q", bytes(buf[cur:cur+8]))[0]; cur += 8
        while len(buf) < cur+n: buf.extend(s.recv(8192))
        body = bytes(buf[cur:cur+n])
        del buf[:cur+n]
        if opcode == 0x9: continue  # ping
        if opcode == 0x8: return None
        return body

s, buf = ws_connect(f"/ws/view/{peer}")
print(f"subscribed to {peer}, recording for {secs}s -> {out_path}", file=sys.stderr)
t0 = time.time()
total = 0
nals = 0
with open(out_path, "wb") as f:
    while time.time() - t0 < secs:
        body = recv_frame(s, buf)
        if body is None: break
        f.write(body)
        total += len(body)
        nals += 1

print(f"wrote {total} bytes, {nals} NAL frames", file=sys.stderr)
