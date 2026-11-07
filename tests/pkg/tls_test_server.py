#!/usr/bin/env python3
"""本地 TLS 1.3 测试服务器（自签 CA 签名），供 http.dialpkg 的 TLS 路径端到端验证。"""
import sys, ssl, socket, threading, os

PORT = int(sys.argv[1])
# 端口文件路径（可选第二参数，默认 /tmp/tlsport），便于测试 harness 读取。
PORTFILE = sys.argv[2] if len(sys.argv) > 2 else "/tmp/tlsport"
# 证书路径可用环境变量覆盖（测试 harness 在 TMP 里自签后传入），默认 /tmp/tlstest。
import os
CERT = os.environ.get("DIAL_TLS_CERT", "/tmp/tlstest/srv.crt")
KEY = os.environ.get("DIAL_TLS_KEY", "/tmp/tlstest/srv.key")

ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3   # 强制 TLS 1.3
ctx.load_cert_chain(certfile=CERT, keyfile=KEY)

def handle(conn):
    """同一 TLS 连接上循环处理多个请求（keep-alive），直到客户端关闭或显式 Connection: close。"""
    try:
        while True:
            data = b""
            while b"\r\n\r\n" not in data:
                chunk = conn.recv(4096)
                if not chunk:
                    return  # 客户端关闭：连接结束
                data += chunk
            req = data.decode("latin1", "replace")
            lines = req.split("\r\n")
            first = lines[0] if lines else ""
            path = first.split(" ", 2)[1] if " " in first else "/"
            close = any(l.lower().startswith("connection:") and "close" in l.lower()
                        for l in lines[1:])

            if path == "/ok":
                body = b"TLS OK from dial-http"
            elif path == "/status404":
                body = b"not found"
            elif path == "/status500":
                body = b"server error"
            else:
                body = b"hello"
            code = 200
            if path == "/status404": code = 404
            if path == "/status500": code = 500
            headers = (
                f"HTTP/1.1 {code} OK\r\n"
                f"Content-Type: text/plain\r\n"
                f"Content-Length: {len(body)}\r\n"
                f"Connection: {'close' if close else 'keep-alive'}\r\n"
                f"X-Test: dialtls\r\n"
                f"\r\n"
            ).encode("latin1") + body
            conn.sendall(headers)
            if close:
                return
    except Exception as e:
        sys.stderr.write(f"handler err: {e}\n")
    finally:
        try: conn.close()
        except: pass

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", PORT))
srv.listen(16)
with open(PORTFILE, "w") as f:
    f.write(str(PORT))
while True:
    c, _ = srv.accept()
    try:
        tls = ctx.wrap_socket(c, server_side=True)
    except Exception as e:
        sys.stderr.write(f"wrap err: {e}\n")
        c.close()
        continue
    threading.Thread(target=handle, args=(tls,), daemon=True).start()
