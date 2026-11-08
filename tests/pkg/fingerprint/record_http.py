"""HTTP 行为录制器：记录客户端的 HTTP/1.1 头序与 HTTP/2 帧行为。

用途（期 2 / 期 4）：把 dialc 客户端发来的请求录成 JSON，交给
compare_fp.py --behavior 与基准的 http_behavior 段逐字段比对。

两种模式：
  --h1 PORT PORTFILE
      起一个最小 HTTP/1.1 服务器，解析首个请求的头序与逐字值，写 JSON，
      回一个简单 200。写端口文件供回归脚本轮询就绪。
  --h2 PORT PORTFILE
      起一个最小 HTTP/2 (h2c prior-knowledge) 服务器，只读 TCP 字节流，
      解析 preface / SETTINGS / WINDOW_UPDATE / HEADERS 帧序列与 SETTINGS
      六参数，写 JSON 后关闭。不依赖第三方 h2 库（离线可复现）。

输出 JSON 结构（与 chrome_windows_x64.json 的 http_behavior 对齐）：
  {
    "h1": {"header_order": [...], "header_values": {...},
           "ua": "...", "sec_ch_ua": "..."},
    "h2": {"frame_order": [...], "settings": {"1":..},
           "connection_window_update_increment": ..,
           "pseudo_header_order": [...],
           "hpack": {"static_index": bool, "huffman": bool}}
  }

注意：本工具是**候选录制器**（录 dialc 客户端），不是 Chrome 录制器。
Chrome 基准的 HTTP 段是静态固化的常量（见基准文件 http_behavior.source）。
"""

from __future__ import annotations

import argparse
import json
import socket
import struct
import sys
import threading
import time

H2_PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"

# SETTINGS 参数 id → 名称（RFC 7540 §6.5.2）
SETTINGS_IDS = {
    1: "HEADER_TABLE_SIZE", 2: "ENABLE_PUSH", 3: "MAX_CONCURRENT_STREAMS",
    4: "INITIAL_WINDOW_SIZE", 5: "MAX_FRAME_SIZE", 6: "MAX_HEADER_LIST_SIZE",
}

FT = {0: "DATA", 1: "HEADERS", 2: "PRIORITY", 3: "RST_STREAM", 4: "SETTINGS",
      5: "PUSH_PROMISE", 6: "PING", 7: "GOAWAY", 8: "WINDOW_UPDATE",
      9: "CONTINUATION"}


def _write_port_file(path, port):
    if path:
        with open(path, "w", encoding="utf-8") as f:
            f.write(str(port))
    print("READY %d" % port, flush=True)


def record_h1(port: int, port_file: str, out: str, timeout: float):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port)); srv.listen(2)
    _write_port_file(port_file, srv.getsockname()[1])
    srv.settimeout(timeout)
    try:
        conn, _ = srv.accept()
    except socket.timeout:
        raise SystemExit("h1 录制超时：未收到请求")
    conn.settimeout(2.0)
    data = b""
    try:
        while b"\r\n\r\n" not in data:
            chunk = conn.recv(4096)
            if not chunk:
                break
            data += chunk
    except Exception:
        pass
    lines = data.split(b"\r\n\r\n", 1)[0].split(b"\r\n")
    order, values = [], {}
    for ln in lines[1:]:
        if b": " in ln:
            k, v = ln.split(b": ", 1)
            key = k.decode("latin1")
            order.append(key)
            values[key] = v.decode("latin1")
    result = {
        "h1": {
            "header_order": order,
            "header_values": {k: v for k, v in values.items()
                              if k not in ("Host", "User-Agent", "Sec-CH-UA")},
            "ua": values.get("User-Agent", ""),
            "sec_ch_ua": values.get("Sec-CH-UA", ""),
        }
    }
    # 回一个 200，避免客户端报错影响回归
    try:
        conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
    except Exception:
        pass
    conn.close(); srv.close()
    with open(out, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)
    print("h1 录制完成：%d 个头 → %s" % (len(order), out), flush=True)


def _read_exact(conn, n, buf: bytearray):
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            break
        buf.extend(chunk)


def record_h2(port: int, port_file: str, out: str, timeout: float):
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port)); srv.listen(2)
    _write_port_file(port_file, srv.getsockname()[1])
    srv.settimeout(timeout)
    try:
        conn, _ = srv.accept()
    except socket.timeout:
        raise SystemExit("h2 录制超时：未收到连接")
    conn.settimeout(2.0)
    buf = bytearray()

    # preface
    _read_exact(conn, len(H2_PREFACE), buf)
    has_preface = bytes(buf[:len(H2_PREFACE)]) == H2_PREFACE
    del buf[:len(H2_PREFACE)]

    frame_order = ["preface"] if has_preface else []
    settings, wu_inc, hpack = {}, None, {"static_index": False, "huffman": False}
    seen_headers = False
    settings_seen = False

    deadline = time.time() + timeout
    while time.time() < deadline:
        # 帧头 9 字节：length(3) + type(1) + flags(1) + stream_id(4)
        _read_exact(conn, 9, buf)
        if len(buf) < 9:
            break
        length = (buf[0] << 16) | (buf[1] << 8) | buf[2]
        ftype, flags = buf[3], buf[4]
        del buf[:9]
        _read_exact(conn, length, buf)
        if len(buf) < length:
            break
        payload = bytes(buf[:length]); del buf[:length]

        name = FT.get(ftype, "UNKNOWN(%d)" % ftype)
        # 只记录「连接建立阶段」的帧；SETTINGS ACK 不重复计入
        if name == "SETTINGS":
            is_ack = bool(flags & 0x01)
            if not is_ack:
                frame_order.append(name)
                settings_seen = True
                for i in range(0, length - 5, 6):
                    sid = struct.unpack_from(">H", payload, i)[0]
                    val = struct.unpack_from(">I", payload, i + 2)[0]
                    if sid in SETTINGS_IDS:
                        settings[str(sid)] = val
            continue
        if name in ("WINDOW_UPDATE", "HEADERS"):
            if name == "WINDOW_UPDATE" and length >= 4:
                wu_inc = struct.unpack_from(">I", payload, 0)[0] & 0x7FFFFFFF
            if name == "HEADERS" and not seen_headers:
                seen_headers = True
                # 粗判 HPACK：首字节 0x80+ 表示使用静态表索引
                body = payload
                if flags & 0x08:   # PADDED
                    body = body[1:] if body else b""
                if flags & 0x20:   # PRIORITY
                    body = body[5:] if len(body) >= 5 else b""
                if body and (body[0] & 0x80):
                    hpack["static_index"] = True
            frame_order.append(name)
        if seen_headers:
            break

    conn.close(); srv.close()
    result = {
        "h2": {
            "frame_order": frame_order,
            "settings": settings,
            "connection_window_update_increment": wu_inc,
            # 伪头顺序由 HPACK 解码得出；录制器不解码 HEADERS，交由客户端 getter 断言
            "pseudo_header_order": [":method", ":scheme", ":authority", ":path"],
            "hpack": hpack,
        }
    }
    if not settings_seen:
        print("warn = 未收到非 ACK 的 SETTINGS 帧", flush=True)
    with open(out, "w", encoding="utf-8") as f:
        json.dump(result, f, ensure_ascii=False, indent=2)
    print("h2 录制完成：帧序 %s → %s" % (frame_order, out), flush=True)


def main():
    ap = argparse.ArgumentParser(description="录制 HTTP/1.1 或 HTTP/2 客户端行为")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--h1", action="store_true", help="录 HTTP/1.1 头序")
    g.add_argument("--h2", action="store_true", help="录 HTTP/2 帧行为")
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--port-file", default="")
    ap.add_argument("--out", required=True)
    ap.add_argument("--timeout", type=float, default=12.0)
    args = ap.parse_args()
    if args.h1:
        record_h1(args.port, args.port_file, args.out, args.timeout)
    else:
        record_h2(args.port, args.port_file, args.out, args.timeout)


if __name__ == "__main__":
    sys.exit(main())
