#!/usr/bin/env python3
"""本地 HTTP/2 测试服务器，供 http.dialpkg 的 HTTP/2 路径端到端验证。

支持两种模式（由第一个位置参数决定）：
  h2c  <port> <portfile>            明文 HTTP/2 prior-knowledge
  h2   <port> <portfile>           TLS 1.3 + ALPN=h2（证书走 DIAL_TLS_CERT/KEY）

关键点：**故意乱序/延迟响应**，用来证明客户端真的按 stream id 交错分发，
而不是「串行请求-串行应答」。做法：对每个请求的响应加入与路径相关的延迟
（/slow* 慢、/fast* 快），且所有请求都在同一条连接上被并发处理。

路由：
  /ok        -> 200  X-Stream: <path>  body: "h2 OK <path>"
  /slowN     -> 200  人为延迟 (让后发的请求先返回)
  /status404 -> 404
  /status500 -> 500
每个响应都带 X-Test: dialh2 与 X-Stream: <path>（供 worker 断言拿到自己的响应）。

额外头回归：每个响应还带 X-Long-Len，值是本请求实际收到的
X-Long-Header 头值字节数（没发则为 0）。W 组用它验证 h2_submit 不再把
超长额外请求头静默截断到 511 字节（§20 Fail-Fast）。

依赖：pip install h2 hpack hyperframe（仅测试服务器用；构建/运行 http.dialpkg 不需要）。
"""
import sys
import os
import ssl
import socket
import threading
import time

from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import RequestReceived, DataReceived, StreamEnded


MODE = sys.argv[1] if len(sys.argv) > 1 else "h2c"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 0
PORTFILE = sys.argv[3] if len(sys.argv) > 3 else "/tmp/h2port"
CERT = os.environ.get("DIAL_TLS_CERT", "/tmp/tlstest/srv.crt")
KEY = os.environ.get("DIAL_TLS_KEY", "/tmp/tlstest/srv.key")


def make_response_headers(path, code, long_len=0):
    return [
        (":status", str(code)),
        ("content-type", "text/plain"),
        ("x-test", "dialh2"),
        ("x-stream", path),  # 让客户端校验「拿回的是自己那条 stream 的响应」
        ("x-long-len", str(long_len)),  # 收到的超长额外头的真实长度（0 = 没发）
    ]


def route(path):
    if path == "/status404":
        return 404, b"not found"
    if path == "/status500":
        return 500, b"server error"
    return 200, ("h2 OK " + path).encode("utf-8")


def delay_for(path):
    """人为制造乱序：/slow* 明显慢，/fast* 与 /ok 快。
    /slow0 最慢、/slow1 次之……这样即使客户端按顺序发，服务端也反序回。"""
    if path.startswith("/slow"):
        n = path[5:]
        idx = int(n) if n.isdigit() else 0
        return 0.25 - 0.03 * min(idx, 6)  # 越靠前的 slow 越慢 -> 反序
    return 0.0


def handle_conn(conn, lock, label):
    """一个 TCP/TLS 连接 = 一个 H2Connection，多个 stream 在其上并发交错。"""
    config = H2Configuration(client_side=False, header_encoding="utf-8")
    h2c = H2Connection(config=config)
    h2c.initiate_connection()
    with lock:
        conn.sendall(h2c.data_to_send())

    pending = {}  # stream_id -> (path, long_len)
    closed = set()

    def send_one(stream_id, path, long_len):
        code, body = route(path)
        # 延迟放在独立线程，模拟「服务端并发处理、乱序返回」
        d = delay_for(path)
        if d > 0:
            time.sleep(d)
        with lock:
            if stream_id in closed:
                return
            h2c.send_headers(stream_id, make_response_headers(path, code, long_len))
            h2c.send_data(stream_id, body, end_stream=True)
            conn.sendall(h2c.data_to_send())
        closed.add(stream_id)

    try:
        while True:
            data = conn.recv(65536)
            if not data:
                return
            events = h2c.receive_data(data)
            with lock:
                out = h2c.data_to_send()
                if out:
                    conn.sendall(out)
            for ev in events:
                if isinstance(ev, RequestReceived):
                    hdrs = dict(ev.headers)
                    path = hdrs.get(":path", "/")
                    # 头名大小写不敏感：HPACK/h2 实现可能保留或小写化。
                    long_len = 0
                    for hk, hv in hdrs.items():
                        if hk.lower() == "x-long-header":
                            long_len = len(hv)
                            break
                    pending[ev.stream_id] = (path, long_len)
                elif isinstance(ev, DataReceived):
                    # 请求体：回 ACK，保证流控窗口
                    with lock:
                        h2c.acknowledge_received_data(ev.flow_controlled_length, ev.stream_id)
                        out = h2c.data_to_send()
                        if out:
                            conn.sendall(out)
                elif isinstance(ev, StreamEnded):
                    path, long_len = pending.pop(ev.stream_id, ("/", 0))
                    threading.Thread(target=send_one, args=(ev.stream_id, path, long_len),
                                     daemon=True).start()
    except (OSError, ssl.SSLError):
        return
    finally:
        # 等待乱序响应发完（给客户端时间收齐）
        time.sleep(0.4)
        try:
            conn.close()
        except Exception:
            pass


def main():
    ctx = None
    if MODE == "h2":
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.minimum_version = ssl.TLSVersion.TLSv1_3
        ctx.load_cert_chain(certfile=CERT, keyfile=KEY)
        ctx.set_alpn_protocols(["h2"])

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", PORT))
    srv.listen(16)
    real_port = srv.getsockname()[1]
    with open(PORTFILE, "w") as f:
        f.write(str(real_port))

    while True:
        c, _ = srv.accept()
        try:
            if ctx is not None:
                c = ctx.wrap_socket(c, server_side=True)
        except Exception as e:
            sys.stderr.write("wrap err: %s\n" % e)
            c.close()
            continue
        lock = threading.Lock()
        threading.Thread(target=handle_conn, args=(c, lock, MODE), daemon=True).start()


if __name__ == "__main__":
    main()
