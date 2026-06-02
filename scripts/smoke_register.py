#!/usr/bin/env python3
"""Send a hand-crafted RegisterPeer over UDP and read the response.

This is a minimal protobuf serializer for the two messages we need —
no protobuf library required. Used to verify the rendezvous wire path
end-to-end without spinning up a real RustDesk client.
"""
import socket
import struct
import sys

def varint(n: int) -> bytes:
    out = bytearray()
    while True:
        b = n & 0x7f
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)

def field_len_delim(field: int, payload: bytes) -> bytes:
    tag = (field << 3) | 2  # wire type 2: length-delimited
    return varint(tag) + varint(len(payload)) + payload

def field_string(field: int, s: str) -> bytes:
    return field_len_delim(field, s.encode())

def field_varint(field: int, n: int) -> bytes:
    tag = (field << 3) | 0
    return varint(tag) + varint(n)

# RegisterPeer { string id = 1; int32 serial = 2; }
register_peer = field_string(1, "TESTID123") + field_varint(2, 0)
# RendezvousMessage { oneof union { ... RegisterPeer register_peer = 6; ... } }
# UDP: raw protobuf, no length prefix.
payload = field_len_delim(6, register_peer)

print(f"sending {len(payload)} bytes (raw protobuf): {payload.hex()}", file=sys.stderr)

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
sock.settimeout(2.0)
sock.sendto(payload, ("127.0.0.1", 21116))
try:
    data, addr = sock.recvfrom(4096)
    print(f"got {len(data)} bytes from {addr}: {data.hex()}", file=sys.stderr)
    # Field 7 = RegisterPeerResponse → tag = (7<<3)|2 = 0x3a
    if data[0] == 0x3a:
        print("OK: server replied with RegisterPeerResponse", file=sys.stderr)
        sys.exit(0)
    print(f"unexpected first byte 0x{data[0]:02x}", file=sys.stderr)
    sys.exit(2)
except socket.timeout:
    print("TIMEOUT: no response from rendezvous", file=sys.stderr)
    sys.exit(1)
