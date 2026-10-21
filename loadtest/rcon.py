#!/usr/bin/env python3
"""Cliente RCON minimo: rcon.py <command> [password] [host] [port]"""
import socket
import struct
import sys
import time


def packet(rid, typ, body):
    return struct.pack("<iii", 10 + len(body), rid, typ) + body + b"\x00\x00"


def read_packet(s):
    raw = s.recv(4)
    if len(raw) < 4:
        return None
    (ln,) = struct.unpack("<i", raw)
    data = b""
    while len(data) < ln:
        chunk = s.recv(ln - len(data))
        if not chunk:
            break
        data += chunk
    return data


def main():
    cmd = sys.argv[1]
    password = sys.argv[2] if len(sys.argv) > 2 else "lazarus-test"
    host = sys.argv[3] if len(sys.argv) > 3 else "127.0.0.1"
    port = int(sys.argv[4]) if len(sys.argv) > 4 else 25575

    s = socket.create_connection((host, port), timeout=5)
    s.settimeout(1.0)
    s.sendall(packet(1, 3, password.encode()))
    resp = read_packet(s)
    if not resp or resp[4:8] != struct.pack("<i", 2):
        print("auth failed", resp)
        return 1
    s.sendall(packet(2, 2, cmd.encode()))
    out = []
    while True:
        try:
            resp = read_packet(s)
        except socket.timeout:
            break
        if not resp:
            break
        body = resp[8:-2]
        out.append(body.decode("utf-8", "replace"))
        if len(out) > 1 and not body:
            break
    print("\n".join(out))
    s.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
