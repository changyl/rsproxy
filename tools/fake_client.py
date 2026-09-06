#!/usr/bin/env python3
# 假 mysql 客户端:连代理,完成 native 认证,发 SELECT 1,dump 收到的每个包
import socket, struct, hashlib, sys

HOST, PORT = "127.0.0.1", 4051
USER, PASSWORD = "root", "test_password"
ARGS = [a for a in sys.argv[1:]]
DEPRECATE_EOF = "--old" not in ARGS
QUERY = next((a for a in ARGS if not a.startswith("--")), "SELECT 1")
DB = b"test"

def parse_greeting(g):
    i = 1  # 跳过 protocol version
    i = g.index(0, i) + 1                      # 版本串 NUL
    i += 4                                     # connection id
    salt1 = g[i:i+8]; i += 8
    i += 1                                     # filler
    cap_lower = struct.unpack("<H", g[i:i+2])[0]; i += 2
    charset = g[i]; i += 1
    i += 2                                     # status
    cap_upper = struct.unpack("<H", g[i:i+2])[0]; i += 2
    auth_len = g[i]; i += 1
    i += 10                                    # reserved
    salt2 = g[i:i+max(0, auth_len-8-1)]
    return salt1 + salt2

def recv_exact(s, n):
    d = b""
    while len(d) < n:
        c = s.recv(n - len(d))
        if not c: break
        d += c
    return d

def read_pkt(s):
    h = recv_exact(s, 4)
    if len(h) < 4: return None, None
    ln = h[0] | (h[1] << 8) | (h[2] << 16)
    return h[3], recv_exact(s, ln)

def send_pkt(s, seq, pl):
    s.sendall(struct.pack("<I", len(pl))[:3] + bytes([seq]) + pl)

def scramble(pw, salt):
    h1 = hashlib.sha1(pw.encode()).digest()
    h2 = hashlib.sha1(h1).digest()
    h3 = hashlib.sha1(salt + h2).digest()
    return bytes(a ^ b for a, b in zip(h1, h3))

s = socket.create_connection((HOST, PORT))
# 1. greeting
seq, g = read_pkt(s)
print(f"[greeting] {len(g)}B: proto={g[0]}, version={g[1:g.index(0)+1]}")
salt = parse_greeting(g)

# 2. auth response
caps = 0x00000200 | 0x00008000 | 0x00080000 | 0x00200000 | 0x00000008
if DEPRECATE_EOF:
    caps |= 0x01000000
auth = struct.pack("<I", caps) + struct.pack("<I", 0x01000000) + bytes([45]) + b"\x00"*23
auth += USER.encode() + b"\x00"
resp = scramble(PASSWORD, salt)
auth += bytes([len(resp)]) + resp
auth += DB + b"\x00"
auth += b"mysql_native_password\x00"
send_pkt(s, 1, auth)
seq, ok = read_pkt(s)
print(f"[auth result] seq={seq} {len(ok)}B first={ok[:8].hex()}")

# 3. 发查询,dump 所有响应包
send_pkt(s, 0, b"\x03" + QUERY.encode())
print(f"[query] {QUERY}")
for i in range(20):
    seq, p = read_pkt(s)
    if p is None:
        print("[EOF] 连接关闭")
        break
    kind = "?"
    if p and p[0] == 0x00: kind = "OK"
    elif p and p[0] == 0xff: kind = "ERR"
    elif p and p[0] == 0xfe: kind = "EOF/OK-FE"
    print(f"[pkt] seq={seq} len={len(p)} type={kind} hex={p[:24].hex()}")
    if kind in ("OK", "ERR") or (kind == "EOF/OK-FE" and (len(p) == 5 or len(p) >= 7)):
        break
s.close()
