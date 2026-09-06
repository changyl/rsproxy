#!/usr/bin/env python3
# 伪 MySQL 5.5 服务器:复刻真实后端 MySQL 的 greeting(caps=0x0008a20c, charset=33)
# 用途:抓取"真实 mysql 客户端"与"代理"的 HandshakeResponse41 原始字节,逐字节对比
import socket, struct, sys, threading

LISTEN_PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 3307
SCRAMBLE1 = b"12345678"
SCRAMBLE2 = b"abcdefghijkl"

def build_greeting():
    pkt = b""
    pkt += bytes([0x0a])                       # protocol version 10
    pkt += b"5.5.39b" + b"\x00"                # server version
    pkt += struct.pack("<I", 1)                # connection id
    pkt += SCRAMBLE1                           # auth-plugin-data-part-1 (8B)
    pkt += b"\x00"                             # filler
    pkt += struct.pack("<H", 0xa20c)           # capabilities lower (同真实后端)
    pkt += bytes([33])                         # charset (同真实后端)
    pkt += struct.pack("<H", 0x0002)           # status flags
    pkt += struct.pack("<H", 0x0008)           # capabilities upper
    pkt += bytes([21])                         # auth-plugin-data len
    pkt += b"\x00" * 10                        # reserved
    pkt += SCRAMBLE2 + b"\x00"                 # part-2 (12B) + NUL
    pkt += b"mysql_native_password" + b"\x00"  # plugin name
    return pkt  # 只返回 payload,send_packet 会加包头

def recv_exact(sock, n):
    data = b""
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            break
        data += chunk
    return data

def read_packet(sock):
    hdr = recv_exact(sock, 4)
    if len(hdr) < 4:
        return None
    ln = hdr[0] | (hdr[1] << 8) | (hdr[2] << 16)
    payload = recv_exact(sock, ln)
    return hdr[3], payload

def dump(label, payload):
    print(f"===== {label}: {len(payload)} bytes =====", flush=True)
    print("HEX :", " ".join(f"{b:02x}" for b in payload), flush=True)
    print("ASCII:", "".join(chr(b) if 32 <= b < 127 else "." for b in payload), flush=True)
    print(flush=True)

def send_packet(sock, seq, payload):
    sock.sendall(struct.pack("<I", len(payload))[:3] + bytes([seq]) + payload)

def handle(conn, addr):
    print(f"\n########## 新连接来自 {addr} ##########")
    try:
        # 1. 发 greeting(与真实后端一致)
        send_packet(conn, 0, build_greeting())
        # 2. 读客户端 HandshakeResponse41
        r = read_packet(conn)
        if r is None:
            print("!! 客户端未发 auth 就断开")
            return
        seq, auth = r
        dump("HandshakeResponse41 (客户端 auth 包)", auth)
        # 3. 回 OK(与真实后端一致: 00 00 00 02 00 00 00)
        send_packet(conn, seq + 1, bytes([0x00, 0x00, 0x00, 0x02, 0x00, 0x00]))
        print(f"[回 OK,进入命令阶段]")
        # 4. 读后续命令(指纹查询等),逐包 dump
        for i in range(6):
            r = read_packet(conn)
            if r is None:
                print("!! 连接断开")
                break
            seq, payload = r
            dump(f"命令包 #{i+1} (seq={seq})", payload)
            # 是查询就回一个最小结果集,让客户端继续发下一条
            if payload and payload[0] == 0x03:
                send_packet(conn, 1, b"\x01")  # column_count=1
                # coldef 标准布局:catalog(03def,无 NUL)+schema/table/org_table(各1B空)
                # + name(lenenc)+org_name(空)+0x0c+12B 固定字段
                coldef = (b"\x03def" + b"\x00" * 3 +
                          b"\x11" + b"@@version_comment" + b"\x00" +
                          b"\x0c\x21\x00\x2a\x00\x00\x00\xfd\x00\x00\x00\x00\x00")
                send_packet(conn, 2, coldef)
                send_packet(conn, 3, b"\xfe\x00\x00\x02\x00")  # EOF
                send_packet(conn, 4, b"\x13" + b"Source distribution")  # row
                send_packet(conn, 5, b"\xfe\x00\x00\x02\x00")  # EOF
            else:
                send_packet(conn, 1, bytes([0x00, 0x00, 0x00, 0x02, 0x00, 0x00]))
    except Exception as e:
        print(f"!! 异常: {e}")
    finally:
        conn.close()

def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", LISTEN_PORT))
    srv.listen(5)
    print(f"伪 MySQL 5.5 服务器监听 127.0.0.1:{LISTEN_PORT} (Ctrl+C 退出)")
    while True:
        conn, addr = srv.accept()
        threading.Thread(target=handle, args=(conn, addr), daemon=True).start()

if __name__ == "__main__":
    main()
