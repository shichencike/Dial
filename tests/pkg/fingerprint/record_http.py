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

期 2 起 H2 段是**真实解析**出来的，不再是占位常量：
  - 从 packages/http/src/dial_hpack_tables.h 读 k_huff_code/k_huff_len，
    用与 C 侧 dh_huffman_decode 相同的逐位匹配算法解 Huffman 串；
  - 内置 RFC 7541 Appendix A 静态表 61 项，解 indexed / indexed-name；
  - 伪头顺序、static_index、huffman 三者全部由解码结果得出。
  这样若 C 侧 Huffman 编码写错，录制器会直接解码失败而不是「假装通过」。

注意：本工具是**候选录制器**（录 dialc 客户端），不是 Chrome 录制器。
Chrome 基准的 HTTP 段是静态固化的常量（见基准文件 http_behavior.source）。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import struct
import sys
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

# RFC 7541 Appendix A，索引 1..61（下标 0 占位）
HPACK_STATIC = [
    None,
    (":authority", ""), (":method", "GET"), (":method", "POST"),
    (":path", "/"), (":path", "/index.html"),
    (":scheme", "http"), (":scheme", "https"),
    (":status", "200"), (":status", "204"), (":status", "206"),
    (":status", "304"), (":status", "400"), (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""), ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""), ("accept-ranges", ""), ("accept", ""),
    ("access-control-allow-origin", ""), ("age", ""), ("allow", ""),
    ("authorization", ""), ("cache-control", ""), ("content-disposition", ""),
    ("content-encoding", ""), ("content-language", ""), ("content-length", ""),
    ("content-location", ""), ("content-range", ""), ("content-type", ""),
    ("cookie", ""), ("date", ""), ("etag", ""), ("expect", ""),
    ("expires", ""), ("from", ""), ("host", ""), ("if-match", ""),
    ("if-modified-since", ""), ("if-none-match", ""), ("if-range", ""),
    ("if-unmodified-since", ""), ("last-modified", ""), ("link", ""),
    ("location", ""), ("max-forwards", ""), ("proxy-authenticate", ""),
    ("proxy-authorization", ""), ("range", ""), ("referer", ""),
    ("refresh", ""), ("retry-after", ""), ("server", ""),
    ("set-cookie", ""), ("strict-transport-security", ""),
    ("transfer-encoding", ""), ("user-agent", ""), ("vary", ""),
    ("via", ""), ("www-authenticate", ""),
]


# ---------------- Huffman 码表（唯一真源：C 头文件） ----------------

def _load_huffman():
    path = os.path.normpath(os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", "..",
        "packages", "http", "src", "dial_hpack_tables.h"))
    with open(path, encoding="utf-8") as f:
        text = f.read()

    def grab(name):
        m = re.search(r"k_" + name + r"\[257\]\s*=\s*\{(.*?)\};", text, re.S)
        if not m:
            raise SystemExit("无法从 %s 读取 k_%s[257]" % (path, name))
        return m.group(1)

    codes = [int(x.rstrip("Uu"), 16)
             for x in re.findall(r"0x[0-9a-fA-F]+U?", grab("huff_code"))]
    lens = [int(x) for x in re.findall(r"\b\d+\b", grab("huff_len"))]
    if len(codes) != 257 or len(lens) != 257:
        raise SystemExit("Huffman 码表长度异常：code=%d len=%d" % (len(codes), len(lens)))
    # (nbits, code) -> sym；sym 256 = EOS，不参与数据解码
    table = {}
    for sym in range(257):
        table[(lens[sym], codes[sym])] = sym
    return table


def huff_decode(data: bytes, table: dict) -> bytes:
    """与 C 侧 dh_huffman_decode 相同的逐位匹配算法（排除 EOS）。"""
    out = bytearray()
    acc = 0
    nbits = 0
    for byte in data:
        for b in range(7, -1, -1):
            acc = (acc << 1) | ((byte >> b) & 1)
            nbits += 1
            if nbits > 30:
                raise ValueError("非法 Huffman 码字（>30 位）")
            sym = table.get((nbits, acc))
            if sym is not None and sym < 256:
                out.append(sym)
                acc = 0
                nbits = 0
    if nbits > 7:
        raise ValueError("Huffman 补齐位超过 7 位")
    if nbits > 0 and acc != (1 << nbits) - 1:
        raise ValueError("Huffman 补齐位不是 EOS 前缀（全 1）")
    return bytes(out)


# ---------------- HPACK 块解析 ----------------

def _read_int(block: bytes, p: int, prefix_bits: int):
    if p >= len(block):
        raise ValueError("HPACK 整数被截断")
    maxv = (1 << prefix_bits) - 1
    v = block[p] & maxv
    p += 1
    if v == maxv:
        m = 0
        while True:
            if p >= len(block):
                raise ValueError("HPACK 整数续字节被截断")
            b = block[p]
            p += 1
            v += (b & 0x7f) << m
            if not (b & 0x80):
                break
            m += 7
            if m > 35:
                raise ValueError("HPACK 整数过长")
    return v, p


def _read_string(block: bytes, p: int, table: dict):
    if p >= len(block):
        raise ValueError("HPACK 字符串首字节被截断")
    huffman = bool(block[p] & 0x80)
    slen, p = _read_int(block, p, 7)
    if p + slen > len(block):
        raise ValueError("HPACK 字符串长度越界")
    data = block[p:p + slen]
    p += slen
    if huffman:
        text = huff_decode(data, table).decode("latin1")
    else:
        text = data.decode("latin1")
    return text, p, huffman


def hpack_scan(block: bytes, table: dict):
    """返回 (所有头列表, static_index, huffman_seen)。"""
    p = 0
    headers = []
    static_index = False
    huffman_seen = False
    while p < len(block):
        first = block[p]
        if first & 0x80:                       # indexed header field
            idx, p = _read_int(block, p, 7)
            if not (1 <= idx <= 61):
                raise ValueError("HPACK indexed 索引越界: %d" % idx)
            name, value = HPACK_STATIC[idx]
            static_index = True
            headers.append((name, value))
        elif (first & 0xE0) == 0x20:          # dynamic table size update
            _, p = _read_int(block, p, 5)
        else:                                 # literal
            prefix_bits = 6 if (first & 0xC0) == 0x40 else 4
            name_idx, p = _read_int(block, p, prefix_bits)
            if name_idx:
                if not (1 <= name_idx <= 61):
                    raise ValueError("HPACK literal name 索引越界: %d" % name_idx)
                name, _ = HPACK_STATIC[name_idx]
                static_index = True
            else:
                name, p, h = _read_string(block, p, table)
                huffman_seen |= h
            value, p, h = _read_string(block, p, table)
            huffman_seen |= h
            headers.append((name, value))
    return headers, static_index, huffman_seen


# ---------------- 录制 ----------------

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
    table = _load_huffman()
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
    settings, wu_inc = {}, None
    settings_seen = False
    header_block = bytearray()
    headers_started = False
    headers_done = False

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
        if name == "WINDOW_UPDATE":
            if length >= 4:
                wu_inc = struct.unpack_from(">I", payload, 0)[0] & 0x7FFFFFFF
            frame_order.append(name)
            continue
        if name in ("HEADERS", "CONTINUATION"):
            body = payload
            if name == "HEADERS":
                if not headers_started:
                    frame_order.append("HEADERS")
                    headers_started = True
                if flags & 0x08:   # PADDED
                    body = body[1:] if body else b""
                if flags & 0x20:   # PRIORITY
                    body = body[5:] if len(body) >= 5 else b""
            header_block.extend(body)
            if flags & 0x04:       # END_HEADERS
                headers_done = True
                break
            continue
        # 其余帧（PING / GOAWAY / DATA …）不计入连接建立阶段帧序
    conn.close(); srv.close()

    pseudo, static_index, huffman = [], False, False
    if headers_done and header_block:
        try:
            decoded, static_index, huffman = hpack_scan(bytes(header_block), table)
            pseudo = [n for n, _ in decoded if n.startswith(":")]
        except (ValueError, UnicodeError) as e:
            print("warn = HEADERS HPACK 解析失败：%s" % e, flush=True)
    elif headers_started:
        print("warn = HEADERS 未收到 END_HEADERS（头块不完整）", flush=True)

    result = {
        "h2": {
            "frame_order": frame_order,
            "settings": settings,
            "connection_window_update_increment": wu_inc,
            "pseudo_header_order": pseudo,
            "hpack": {"static_index": static_index, "huffman": huffman},
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
