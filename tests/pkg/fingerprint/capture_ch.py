"""TLS ClientHello 录制器（两种模式，无第三方依赖）。

模式一 · 录 Chrome 基准（--chrome）：
    起一个本地 TCP 监听 → 用 headless Chrome 访问它 → 读**首个 TLS record**
    （ClientHello）→ 保存二进制并可选输出解析 JSON。
    只取首包、**不完成握手**：Chrome 页面报证书错误无关，ClientHello 已在线上。
    用 `--host-resolver-rules` 把 fp.local 映射到 127.0.0.1，让 Chrome 走
    域名路径（发 SNI），得到 JA4 a 段的 `d` 而非 `i`。

模式二 · 录 dialc 候选（--listen）：
    监听指定端口、写端口文件供回归脚本轮询就绪，等 dialc 客户端连入后
    录首个 TLS record。用于「候选 ClientHello vs 基准」离线比对。

用法：
    python3 capture_ch.py --chrome --out record.bin [--json parsed.json]
    python3 capture_ch.py --listen 0 --port-file /tmp/fp_port --out cand.bin

技术诚实：Chrome 110+ 每次连接随机置换扩展顺序，故 JA3（顺序敏感）只与
录制那一次一致；JA4 排序后稳定。本工具不做任何「逐字节一致」承诺。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time

import clienthello


def _recv_full_record(conn) -> bytes:
    """读一个完整 TLS record（按 record 头的长度字段读齐，处理分片）。"""
    head = b""
    while len(head) < 5:
        chunk = conn.recv(5 - len(head))
        if not chunk:
            return head
        head += chunk
    if len(head) < 5:
        return head
    n = struct.unpack_from(">H", head, 3)[0]
    body = b""
    while len(body) < n:
        chunk = conn.recv(n - len(body))
        if not chunk:
            break
        body += chunk
    return head + body


def serve_once(port: int, port_file: str | None, timeout: float):
    """监听一次连接，返回首个 TLS record 字节。"""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(4)
    real_port = srv.getsockname()[1]
    if port_file:
        with open(port_file, "w", encoding="utf-8") as f:
            f.write(str(real_port))
        print("READY %d" % real_port, flush=True)
    srv.settimeout(timeout)
    captured = {}
    done = threading.Event()

    def loop():
        try:
            while not done.is_set():
                try:
                    conn, _ = srv.accept()
                except socket.timeout:
                    break
                conn.settimeout(2.0)
                try:
                    data = _recv_full_record(conn)
                except Exception:
                    data = b""
                if data and not captured:
                    captured["data"] = data
                    done.set()
                conn.close()
        finally:
            srv.close()

    return srv, real_port, captured, done, loop


def chrome_capture(out_bin: str, out_json: str | None, timeout: float) -> bytes:
    """起 Chrome headless，录其首个 ClientHello。"""
    work = "/tmp/dial_fp_chrome_profile"
    shutil.rmtree(work, ignore_errors=True)

    # 端口固定用一个高位值；端口文件写进 /tmp 便于诊断
    port = 18777
    srv, real_port, captured, done, loop = serve_once(port, None, timeout)
    t = threading.Thread(target=loop, daemon=True)
    t.start()
    time.sleep(0.3)

    url = "https://fp.local:%d/" % real_port
    cmd = [
        "google-chrome", "--headless=new", "--no-sandbox", "--disable-gpu",
        "--disable-dev-shm-usage", "--no-first-run", "--no-default-browser-check",
        "--no-proxy-server", "--ignore-certificate-errors", "--disable-quic",
        "--host-resolver-rules=MAP fp.local 127.0.0.1",
        "--user-data-dir=" + work,
        "--virtual-time-budget=2500",
        url,
    ]
    try:
        subprocess.run(cmd, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=min(timeout, 20))
    except subprocess.TimeoutExpired:
        pass
    except FileNotFoundError:
        print("未找到 google-chrome：基准录制需要本机装有 Chrome", file=sys.stderr)
    done.wait(2.0)
    t.join(1.5)

    data = captured.get("data")
    if not data:
        raise SystemExit("Chrome ClientHello 录制失败：未捕获到 TLS record")
    if data[0] != 0x16:
        raise SystemExit("捕获到的首包不是 TLS handshake record（0x16）")
    with open(out_bin, "wb") as f:
        f.write(data)
    print("录制成功：%d 字节 → %s" % (len(data), out_bin), flush=True)

    parsed = clienthello.parse_clienthello(data)
    if out_json:
        with open(out_json, "w", encoding="utf-8") as f:
            json.dump(parsed, f, ensure_ascii=False, indent=2)
        print("解析结果 → %s" % out_json, flush=True)
    print("JA4 = %s" % parsed["fingerprints"]["ja4"], flush=True)
    return data


def listen_capture(port: int, port_file: str, out_bin: str,
                   out_json: str | None, timeout: float) -> bytes:
    """监听 dialc 客户端连入，录其首个 ClientHello。"""
    srv, real_port, captured, done, loop = serve_once(port, port_file, timeout)
    t = threading.Thread(target=loop, daemon=True)
    t.start()
    t.join(timeout + 1)
    data = captured.get("data")
    if not data:
        raise SystemExit("候选 ClientHello 录制失败：超时未捕获 TLS record")
    with open(out_bin, "wb") as f:
        f.write(data)
    print("录制成功：%d 字节 → %s" % (len(data), out_bin), flush=True)
    if out_json:
        parsed = clienthello.parse_clienthello(data)
        with open(out_json, "w", encoding="utf-8") as f:
            json.dump(parsed, f, ensure_ascii=False, indent=2)
        print("JA4 = %s" % parsed["fingerprints"]["ja4"], flush=True)
    return data


def main():
    ap = argparse.ArgumentParser(description="录制 TLS ClientHello")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--chrome", action="store_true", help="起 headless Chrome 录基准")
    g.add_argument("--listen", action="store_true", help="监听 dialc 候选")
    ap.add_argument("--port", type=int, default=0, help="--listen 的监听端口（0=随机）")
    ap.add_argument("--port-file", default="", help="--listen 写就绪端口的文件")
    ap.add_argument("--out", required=True, help="输出 ClientHello 二进制路径")
    ap.add_argument("--json", default="", help="可选：同时输出解析 JSON")
    ap.add_argument("--timeout", type=float, default=12.0, help="监听超时秒数")
    args = ap.parse_args()

    if args.chrome:
        chrome_capture(args.out, args.json or None, args.timeout)
    else:
        listen_capture(args.port, args.port_file, args.out,
                       args.json or None, args.timeout)


if __name__ == "__main__":
    main()
