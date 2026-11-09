#!/usr/bin/env python3
"""http.dialpkg 回归测试用的本地 HTTP/1.1 服务器。

只实现 W 组回归需要的那几条路径，每条都对应一个**协议要点**：

| 路径          | 考察点 |
| ------------- | ------ |
| /ok           | 200 + 响应体 + 自定义响应头 + Content-Type |
| /echo         | POST 请求体回显（验证 Content-Length 与请求体拼装） |
| /notfound     | 404 状态码 |
| /chunked      | `Transfer-Encoding: chunked` 分块响应（无 Content-Length） |
| /chunked_bad  | 畸形 chunked：长度行 `xyz` 非十六进制，客户端必须**明确失败**（§20），不得静默当成结束块 |
| /chunked_ext  | 合法 chunk-ext：长度行带 `;ext=1`，验证长度行校验没有误伤扩展参数 |
| /gzip         | `Content-Encoding: gzip` 压缩响应（验证 zlib 解压） |
| /multi        | 多个同名/异名响应头（验证按名取值与按序号遍历） |
| /echo-headers | 把**收到的请求头**原样回显为 JSON，验证客户端画像真实发出的头
|               | （如 chrome 画像的 User-Agent 含 Chrome、含 sec-ch-ua / sec-fetch-*） |

用法：python3 tests/pkg/http_test_server.py <端口文件>
  监听 127.0.0.1 的随机空闲端口，把端口号写进 <端口文件> 后打印 READY。
"""
from __future__ import annotations

import gzip
import json
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CHUNKED_BODY = b"chunk-one|chunk-two|chunk-three"
GZIP_BODY = b"gzip-compressed-payload-" * 8


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # 静默：测试输出要干净
        pass

    def _send(self, code: int, body: bytes, ctype: str = "text/plain", extra=None):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or []):
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path == "/ok":
            self._send(200, b"hello dial http\n", extra=[("X-Dial-Test", "hello")])
        elif path == "/notfound":
            self._send(404, b"no such thing\n")
        elif path == "/chunked":
            # 分块响应：故意不给 Content-Length
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for part in (b"chunk-one|", b"chunk-two|", b"chunk-three"):
                self.wfile.write(b"%x\r\n" % len(part) + part + b"\r\n")
            self.wfile.write(b"0\r\n\r\n")
        elif path == "/chunked_bad":
            # 畸形 chunked：长度行不是十六进制。修复前 strtoll 返回 0 且
            # endp==line，被静默当成「结束块」-> 客户端拿到空 body 却报成功。
            # 修复后必须按 §20 Fail-Fast 明确返回协议错，绝不消费后面的坏数据。
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            self.wfile.write(b"xyz\r\nthis-payload-must-never-be-consumed\r\n0\r\n\r\n")
        elif path == "/chunked_ext":
            # 合法 chunk-ext（RFC 7230 §4.1.1）：长度行带 ";ext=1"。
            # 用来锁住新增的 endp 检查没有误伤合法的扩展参数。
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            for part in (b"ext-one|", b"ext-two|", b"ext-three"):
                self.wfile.write(b"%x;ext=1\r\n" % len(part) + part + b"\r\n")
            self.wfile.write(b"0\r\n\r\n")
        elif path == "/gzip":
            payload = gzip.compress(GZIP_BODY)
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Encoding", "gzip")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        elif path == "/multi":
            self._send(200, b"multi\n", extra=[
                ("X-One", "1"),
                ("X-Two", "2"),
                ("X-One", "3"),      # 同名头多值：按序号遍历时都要看得到
            ])
        elif path == "/echo-headers":
            # 把收到的请求头原样回显为 JSON（键为服务端小写化后的头名），
            # 用于验证客户端画像真实发出的请求头（chrome 画像的 UA 含 Chrome、
            # 含 sec-ch-ua / sec-fetch-* 等）。键序不保证，调用方按子串/字段核对。
            hdrs = {k: v for k, v in self.headers.items()}
            self._send(200, json.dumps(hdrs).encode("utf-8"),
                       ctype="application/json")
        else:
            self._send(404, b"unknown path\n")

    def do_POST(self):
        path = self.path.split("?", 1)[0]
        length = int(self.headers.get("Content-Length", "0") or "0")
        body = self.rfile.read(length) if length else b""
        if path == "/echo":
            self._send(200, body, ctype=self.headers.get("Content-Type", "text/plain"),
                       extra=[("X-Echo-Len", str(len(body)))])
        else:
            self._send(404, b"unknown path\n")


def main() -> int:
    if len(sys.argv) != 2:
        print("用法: http_test_server.py <端口文件>", file=sys.stderr)
        return 2
    portfile = sys.argv[1]

    srv = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = srv.server_address[1]
    with open(portfile, "w", encoding="utf-8") as f:
        f.write(str(port))
    print("READY", flush=True)

    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    try:
        while True:
            t.join(0.5)
            # 服务线程死了就退出（否则测试会一直等）
            if not t.is_alive():
                break
    except KeyboardInterrupt:
        pass
    finally:
        srv.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
