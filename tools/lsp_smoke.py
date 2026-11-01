#!/usr/bin/env python3
"""dialc lsp 的 JSON-RPC 冒烟测试。

启动 `dialc lsp`（stdio），发标准请求并断言关键响应字段，确保核心能力不回归。
逐轮扩展断言（轮 1 覆盖 initialize 能力协商 / hover / definition / completion）。

用法：
    python3 tools/lsp_smoke.py [--dialc 路径] [--uri 文件uri]

退出码 0 = 全部通过；非 0 = 有失败。
"""
import argparse
import json
import subprocess
import sys
import threading

SAMPLE = """fn add(a: int, b: int): int {
    return a + b;
}

fn main(): int {
    let x: int = add(1, 2);
    return x;
}
"""


class LspClient:
    def __init__(self, dialc: str):
        self.proc = subprocess.Popen(
            [dialc, "lsp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.buf = b""
        self.lock = threading.Lock()
        self.next_id = 1
        self._stderr_thread = threading.Thread(
            target=self._drain_stderr, daemon=True
        )
        self._stderr_thread.start()

    def _drain_stderr(self):
        for line in self.proc.stderr:
            # 仅用于本地调试，不影响断言
            pass

    def _write(self, msg: dict):
        body = json.dumps(msg).encode("utf-8")
        with self.lock:
            self.proc.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode())
            self.proc.stdin.write(body)
            self.proc.stdin.flush()

    def _read_one(self) -> dict:
        while True:
            header_end = self.buf.find(b"\r\n\r\n")
            if header_end < 0:
                # 注意：对管道不能用 read(N)——它会等满 N 字节或 EOF 才返回，
                # 而 LSP 消息往往不足 N 字节，导致永久阻塞。read1 只读「当前可用的」字节。
                chunk = self.proc.stdout.read1(65536)
                if not chunk:
                    raise EOFError("LSP 进程提前退出")
                self.buf += chunk
                continue
            header = self.buf[:header_end].decode("ascii", "replace")
            m = None
            for line in header.split("\r\n"):
                if line.lower().startswith("content-length:"):
                    m = int(line.split(":", 1)[1].strip())
            if m is None:
                self.buf = self.buf[header_end + 4 :]
                continue
            start = header_end + 4
            if len(self.buf) < start + m:
                chunk = self.proc.stdout.read(start + m - len(self.buf))
                if not chunk:
                    raise EOFError("LSP 进程提前退出")
                self.buf += chunk
            body = self.buf[start : start + m]
            self.buf = self.buf[start + m :]
            return json.loads(body.decode("utf-8", "replace"))

    def request(self, method: str, params: dict) -> object:
        rid = self.next_id
        self.next_id += 1
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        # 只收带该 id 的响应（忽略途中可能到来的通知）
        while True:
            msg = self._read_one()
            if msg.get("id") == rid:
                if "error" in msg:
                    raise RuntimeError(f"{method} 返回错误: {msg['error']}")
                return msg.get("result")
            # 通知（无 id）：跳过

    def notify(self, method: str, params: dict):
        self._write({"jsonrpc": "2.0", "method": method, "params": params})

    def close(self):
        try:
            self.notify("exit", {})
        except Exception:
            pass
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--dialc",
        default=None,
        help="dialc 可执行文件路径（默认 target/release/dialc，回退 target/debug/dialc）",
    )
    ap.add_argument("--uri", default="file:///sample.dial")
    args = ap.parse_args()

    import os

    dialc = args.dialc
    if dialc is None:
        for cand in ("target/release/dialc", "target/debug/dialc"):
            if os.path.exists(cand):
                dialc = cand
                break
        else:
            dialc = "dialc"
    print(f"[smoke] 使用 dialc: {dialc}")

    fails = []
    client = LspClient(dialc)
    try:
        # 1) initialize（带真实 capabilities，验证能力协商）
        caps = {
            "textDocument": {
                "hover": {"dynamicRegistration": False},
                "definition": {"dynamicRegistration": False},
                "completion": {"completionItem": {"snippetSupport": True}},
                "semanticTokens": {},
                "documentSymbol": {},
                "synchronization": {"dynamicRegistration": False},
            }
        }
        init = client.request(
            "initialize",
            {
                "processId": None,
                "rootUri": None,
                "capabilities": caps,
            },
        )
        srv_caps = init.get("capabilities", {})
        print(f"[smoke] 服务端能力: {json.dumps(srv_caps, ensure_ascii=False)}")
        for key in ("hoverProvider", "definitionProvider", "completionProvider"):
            if not srv_caps.get(key):
                fails.append(f"能力协商缺失: {key}")
        if srv_caps.get("textDocumentSync") != 1:
            fails.append("textDocumentSync 应为 1(Full)")

        client.notify("initialized", {})

        # 2) didOpen 示例文档
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": args.uri,
                    "languageId": "dial",
                    "version": 1,
                    "text": SAMPLE,
                }
            },
        )

        # 3) hover：在 `let x: int = add(1, 2)` 的 add 上（0 起行=5, 字符=17）
        hover = client.request(
            "textDocument/hover",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 17},
            },
        )
        if not hover or "contents" not in hover:
            fails.append("hover 返回空")
        else:
            print(f"[smoke] hover 命中: {json.dumps(hover['contents'], ensure_ascii=False)[:80]}")

        # 4) definition：同上位置，应跳到 fn add 定义（0 起行=0）
        definition = client.request(
            "textDocument/definition",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 17},
            },
        )
        if not definition:
            fails.append("definition 返回空")
        else:
            print(f"[smoke] definition 命中: {json.dumps(definition, ensure_ascii=False)[:80]}")

        # 5) completion：在 `let x: int = ` 之后（字符=17，即 add 之前），应返回带 textEdit 的候选
        completion = client.request(
            "textDocument/completion",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 17},
            },
        )
        if isinstance(completion, dict):
            items = completion.get("items", [])
        else:
            items = completion or []
        if not items:
            fails.append("completion 返回空")
        else:
            first = items[0]
            if "textEdit" not in first:
                fails.append("completion 项缺少 textEdit（精确替换范围）")
            if "sortText" not in first:
                fails.append("completion 项缺少 sortText")
            print(
                f"[smoke] completion 命中 {len(items)} 项, 首项: "
                f"label={first.get('label')} kind={first.get('kind')} "
                f"textEdit={'有' if 'textEdit' in first else '无'}"
            )

        # 6) 语义 token：至少应产出若干 token
        sem = client.request(
            "textDocument/semanticTokens/full",
            {"textDocument": {"uri": args.uri}},
        )
        data = (sem or {}).get("data", [])
        if not data:
            fails.append("semanticTokens 返回空")
        else:
            print(f"[smoke] semanticTokens 产出 {len(data) // 5} 个 token")

        client.request("shutdown", {})
    finally:
        client.close()

    if fails:
        print("\n[smoke] 失败项:")
        for f in fails:
            print(f"  ✗ {f}")
        sys.exit(1)
    print("\n[smoke] ✅ 全部通过")
    sys.exit(0)


if __name__ == "__main__":
    main()
