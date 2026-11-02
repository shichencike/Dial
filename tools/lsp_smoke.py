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
        # 协议污染检测：stdout 上出现任何非协议字节都记在这里（见 _read_one）。
        self.pollution = []
        # 收到的**通知**（如 publishDiagnostics）——断言诊断内容时要用。
        self.notifications = []
        # 超时自杀：请求迟迟没有响应时，由看门狗杀掉进程，避免 CI 无限挂住。
        self.timed_out = None
        self._stderr_thread = threading.Thread(
            target=self._drain_stderr, daemon=True
        )
        self._stderr_thread.start()

    def _drain_stderr(self):
        for line in self.proc.stderr:
            # server 的日志一律走 stderr（stdout 属于协议流）。
            # 这里原样转发，方便失败时定位；不影响任何断言。
            sys.stderr.write("[server] " + line.decode("utf-8", "replace"))
            sys.stderr.flush()

    def _write(self, msg: dict):
        body = json.dumps(msg).encode("utf-8")
        with self.lock:
            self.proc.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode())
            self.proc.stdin.write(body)
            self.proc.stdin.flush()

    def _read_one(self) -> dict:
        """读一条 LSP 消息。

        协议流必须是 `Content-Length: N\\r\\n\\r\\n` + N 字节的严格序列。
        这里**不做**任何"猜测式"修复：一旦发现裸字节（污染），就记下来并重新同步，
        让测试**明确失败**——而不是像从前那样悄悄错位、最后以超时收场。
        """
        while True:
            if not self.buf.startswith(b"Content-Length:"):
                # 缓冲区开头不是消息头：要么头还没到齐，要么混进了裸字节。
                idx = self.buf.find(b"Content-Length:")
                if idx < 0:
                    chunk = self.proc.stdout.read1(65536)
                    if not chunk:
                        raise EOFError("LSP 进程提前退出")
                    self.buf += chunk
                    continue
                # 确认是污染：裸字节夹在两条消息之间
                self.pollution.append(self.buf[:idx])
                self.buf = self.buf[idx:]
                continue
            header_end = self.buf.find(b"\r\n\r\n")
            if header_end < 0:
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
                # 头部没有长度：同样视为污染，跳过这一段
                self.pollution.append(self.buf[:header_end + 4])
                self.buf = self.buf[header_end + 4 :]
                continue
            start = header_end + 4
            if len(self.buf) < start + m:
                # 注意：对管道不能用 read(N)——它会等满 N 字节或 EOF 才返回，
                # 而 LSP 消息往往不足 N 字节，导致永久阻塞。read1 只读「当前可用的」字节。
                chunk = self.proc.stdout.read(start + m - len(self.buf))
                if not chunk:
                    raise EOFError("LSP 进程提前退出")
                self.buf += chunk
            body = self.buf[start : start + m]
            self.buf = self.buf[start + m :]
            msg = json.loads(body.decode("utf-8", "replace"))
            # 通知（无 id）顺手存一份：断言诊断内容时要用，而 request() 只等响应。
            if msg.get("id") is None and msg.get("method"):
                self.notifications.append(msg)
            return msg

    def request(self, method: str, params: dict, timeout: float = 30.0) -> object:
        rid = self.next_id
        self.next_id += 1
        self._write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        # 看门狗：超时就杀掉进程，把「永久挂住」变成「快速失败 + 明确原因」。
        timer = threading.Timer(timeout, self._on_timeout, args=(method,))
        timer.daemon = True
        timer.start()
        try:
            # 只收带该 id 的响应（忽略途中可能到来的通知）
            while True:
                msg = self._read_one()
                if msg.get("id") == rid:
                    if "error" in msg:
                        raise RuntimeError(f"{method} 返回错误: {msg['error']}")
                    return msg.get("result")
                # 通知（无 id）：跳过
        except EOFError:
            if self.timed_out:
                raise RuntimeError(
                    f"{method} 超时 {timeout:.0f}s 无响应（server 卡死？）"
                ) from None
            raise
        finally:
            timer.cancel()

    def _on_timeout(self, method: str):
        self.timed_out = method
        try:
            self.proc.kill()
        except Exception:
            pass

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
                "references": {},
                "rename": {"prepareSupport": True},
                "signatureHelp": {"signatureInformation": {"parameterInformation": {"labelOffsetSupport": True}}},
                "formatting": {},
                "inlayHint": {"dynamicRegistration": False},
                "codeAction": {"codeActionLiteralSupport": {"codeActionKind": {"valueSet": ["quickfix"]}}},
                "foldingRange": {},
                "selectionRange": {},
                "documentLink": {},
            },
            "workspace": {
                "symbol": {"dynamicRegistration": False},
                # 声明支持带版本的文档编辑 -> 服务端会用 documentChanges 形状
                "workspaceEdit": {"documentChanges": True},
            },
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
        for key in (
            "hoverProvider",
            "definitionProvider",
            "completionProvider",
            "documentSymbolProvider",
            "referencesProvider",
            "renameProvider",
            "workspaceSymbolProvider",
            "signatureHelpProvider",
            "documentFormattingProvider",
            "inlayHintProvider",
            "codeActionProvider",
            "foldingRangeProvider",
            "selectionRangeProvider",
            "documentLinkProvider",
        ):
            if not srv_caps.get(key):
                fails.append(f"能力协商缺失: {key}")
        # 规范：textDocumentSync 必须是**选项形式**才表达得了「请发 didSave」——
        # 纯数字形式里没有 save 字段，客户端就不会发 didSave，诊断便刷不出来。
        sync = srv_caps.get("textDocumentSync")
        if not isinstance(sync, dict):
            fails.append(f"textDocumentSync 应为选项形式（要带 save），实际 {sync!r}")
        else:
            if sync.get("change") != 1:
                fails.append(f"textDocumentSync.change 应为 1(Full)，实际 {sync.get('change')!r}")
            if not sync.get("save"):
                fails.append("textDocumentSync.save 应为真——否则客户端不发 didSave，诊断刷不出来")

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

        # 7) documentSymbol：大纲应含 add / main 两个顶层函数
        doc_sym = client.request(
            "textDocument/documentSymbol",
            {"textDocument": {"uri": args.uri}},
        ) or []
        names = [d.get("name") for d in doc_sym]
        if not doc_sym:
            fails.append("documentSymbol 返回空")
        elif "add" not in names or "main" not in names:
            fails.append(f"documentSymbol 大纲缺失函数: {names}")
        else:
            print(f"[smoke] documentSymbol 大纲: {names}")

        # 8) workspace/symbol：按 "add" 模糊搜索应命中
        ws_sym = client.request("workspace/symbol", {"query": "add"}) or []
        if not ws_sym:
            fails.append("workspace/symbol 搜索 'add' 返回空")
        else:
            print(f"[smoke] workspace/symbol 'add' 命中 {len(ws_sym)} 项")

        # 9) references：在 add 使用处（行5 列17）查引用，含声明应 >= 2
        refs = client.request(
            "textDocument/references",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 17},
                "context": {"includeDeclaration": True},
            },
        ) or []
        if len(refs) < 2:
            fails.append(f"references(含声明) 应 >=2，实际 {len(refs)}")
        else:
            print(f"[smoke] references(含声明) 命中 {len(refs)} 处")

        # 10) prepareRename：应返回可重命名区间
        prep = client.request(
            "textDocument/prepareRename",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 17},
            },
        )
        if not prep or "range" not in prep:
            fails.append("prepareRename 未返回区间")
        else:
            print(f"[smoke] prepareRename 区间: {json.dumps(prep['range'], ensure_ascii=False)}")

        # 11) rename：把 add 改名为 sum，应生成含 2 处编辑的 WorkspaceEdit。
        #     规范当前的形状是 `documentChanges`（元素为 TextDocumentEdit）；
        #     旧的 `changes` 自 3.16 起已废弃——这里断言用的是**新**形状。
        rn = client.request(
            "textDocument/rename",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 17},
                "newName": "sum",
            },
        ) or {}
        if "changes" in rn:
            fails.append("客户端已声明支持 documentChanges，rename 却用了 changes")
        doc_changes = rn.get("documentChanges") or []
        edits = []
        for dc in doc_changes:
            if dc.get("textDocument", {}).get("uri") == args.uri:
                edits = dc.get("edits") or []
        if len(edits) < 2:
            fails.append(f"rename 应生成 >=2 处编辑，实际 {len(edits)}")
        else:
            print(f"[smoke] rename 'add'->'sum' 生成 {len(edits)} 处编辑（documentChanges）")

        # 12) signatureHelp：在 add( 的实参处（行5 列21）应返回签名
        sig = client.request(
            "textDocument/signatureHelp",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 21},
            },
        ) or {}
        sigs = sig.get("signatures") or []
        if not sigs:
            fails.append("signatureHelp 未返回签名")
        else:
            print(f"[smoke] signatureHelp 签名: {sigs[0].get('label')}")

        # 13) formatting：保守格式化应返回覆盖全文的单个 TextEdit，且结果无行尾空白
        fmt = client.request(
            "textDocument/formatting",
            {"textDocument": {"uri": args.uri}, "options": {"tabSize": 4, "insertSpaces": True}},
        ) or []
        # 规范：`textDocument/formatting` 的结果就是 **TextEdit[]** 本身，
        # 不是 `{ "edits": [...] }` 那种自定义包装。
        if isinstance(fmt, dict):
            fails.append("formatting 应直接返回 TextEdit[]，不该是 {edits:[...]} 这类自定义包装")
        edits = fmt if isinstance(fmt, list) else []
        if len(edits) != 1:
            fails.append(f"formatting 应返回 1 个 TextEdit，实际 {len(edits)}")
        else:
            new_text = edits[0].get("newText", "")
            if any(ln != ln.rstrip() for ln in new_text.split("\n")):
                fails.append("formatting 结果仍存在行尾空白")
            else:
                print(f"[smoke] formatting 产出 {len(new_text)} 字节（无行尾空白）")

        # 14) inlayHint：在 add(1, 2) 调用处应给出 a: / b: 形参名提示
        inlay = client.request(
            "textDocument/inlayHint",
            {
                "textDocument": {"uri": args.uri},
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 20, "character": 0}},
            },
        ) or []
        labels = [h.get("label", "") for h in inlay]
        if len(inlay) < 2 or not any("a:" in l for l in labels) or not any("b:" in l for l in labels):
            fails.append(f"inlayHint 应给出 a:/b: 形参名提示，实际 {labels}")
        else:
            print(f"[smoke] inlayHint 形参提示: {labels}")

        # 15) foldingRange：SAMPLE 含 add / main 两个函数体 → 至少 1 个折叠区间
        folding = client.request(
            "textDocument/foldingRange",
            {"textDocument": {"uri": args.uri}},
        ) or []
        if not folding:
            fails.append("foldingRange 返回空")
        else:
            print(f"[smoke] foldingRange 折叠区间: {len(folding)} 个")

        # 16) selectionRange：在函数体内取一个位置，应返回带 range（可能含 parent）的结果
        sel = client.request(
            "textDocument/selectionRange",
            {
                "textDocument": {"uri": args.uri},
                "position": {"line": 6, "character": 4},
            },
        ) or {}
        if "range" not in sel:
            fails.append("selectionRange 未返回 range")
        else:
            print(f"[smoke] selectionRange 命中 range: {json.dumps(sel['range'], ensure_ascii=False)}")

        # 17) codeAction：缺失包诊断 → 给出 dialc.install；普通诊断 → 给出 dialc.build
        #     诊断按**真实形状**构造：带稳定错误码 + 结构化 data.package。
        #     （quickfix 现在认错误码，不认消息文本——所以合成诊断必须带 code。）
        ca_pkg = client.request(
            "textDocument/codeAction",
            {
                "textDocument": {"uri": args.uri},
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "context": {
                    "diagnostics": [
                        {
                            "code": "dialc.package.missing",
                            "data": {"package": "demo"},
                            "message": "找不到已安装的包 'demo'（缓存目录: ...）。请先用 dialc install 安装",
                        }
                    ]
                },
            },
        ) or []
        if not any(a.get("command", {}).get("command") == "dialc.install" for a in ca_pkg):
            fails.append("codeAction 缺失包未给出 dialc.install")
        else:
            print(f"[smoke] codeAction(缺失包) 动作: {[a['title'] for a in ca_pkg]}")

        # 18) documentLink：打开含 import 的文档，链接应返回数组（不崩溃）
        link_uri = "file:///with_import.dial"
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": link_uri,
                    "languageId": "dial",
                    "version": 1,
                    "text": 'import "demo";\n\nfn main(): int {\n    return 0;\n}\n',
                }
            },
        )
        links = client.request(
            "textDocument/documentLink", {"textDocument": {"uri": link_uri}}
        ) or []
        if not isinstance(links, list):
            fails.append("documentLink 未返回数组")
        else:
            print(f"[smoke] documentLink 链接数: {len(links)}（包不存在时为 0，不报错）")

        # 19) 回归防线：解析 import 会走「加载包」这条路径（包解析过程**曾经**
        #     往 stdout 吐裸文本，把协议流打乱，表现为「一开包就卡死」）。
        #     这里再要一次 documentLink：若协议流被污染，客户端会错位、
        #     要么拿到错乱的帧、要么直接超时——都必须明确失败，不能静悄悄。
        links2 = client.request(
            "textDocument/documentLink", {"textDocument": {"uri": link_uri}}
        ) or []
        if not isinstance(links2, list):
            fails.append("documentLink(二次) 未返回数组")
        else:
            print(f"[smoke] documentLink 二次请求仍正常（{len(links2)} 条）")

        # 20) selectionRange 的**规范形式**：LSP 规定用 `positions`（数组，可多选）。
        #     真实客户端（VS Code）发的就是这个形状，只认单数 position 会直接拿不到结果。
        sel_multi = client.request(
            "textDocument/selectionRange",
            {
                "textDocument": {"uri": args.uri},
                "positions": [
                    {"line": 6, "character": 4},
                    {"line": 0, "character": 4},
                ],
            },
        )
        if not isinstance(sel_multi, list) or len(sel_multi) != 2:
            fails.append(f"selectionRange(positions 数组) 应返回 2 条，实际 {sel_multi!r}")
        elif "range" not in sel_multi[0]:
            fails.append("selectionRange(positions 数组) 的元素缺少 range")
        else:
            print(f"[smoke] selectionRange 规范形式（positions 数组）返回 {len(sel_multi)} 条")

        # 22) 诊断要带**稳定错误码**：缺包必须真的报出来（此前被静默吞掉，
        #     于是编辑器里毫无提示，轮4 的「安装包」quickfix 也从不触发）。
        miss_uri = "file:///missing_pkg.dial"
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": miss_uri,
                    "languageId": "dial",
                    "version": 1,
                    "text": 'import "definitely_not_installed_pkg";\n\nfn main(): int {\n    return 0;\n}\n',
                }
            },
        )
        # 发一个请求把紧随其后的诊断通知消费掉（request 只等带 id 的响应）
        client.request("textDocument/documentSymbol", {"textDocument": {"uri": miss_uri}})
        miss_diags = []
        for n in client.notifications:
            if (
                n.get("method") == "textDocument/publishDiagnostics"
                and n.get("params", {}).get("uri") == miss_uri
            ):
                miss_diags = n["params"].get("diagnostics", [])
        if not miss_diags:
            fails.append("缺包时应当发布诊断（此前被静默吞掉，编辑器毫无提示）")
        elif miss_diags[0].get("code") != "dialc.package.missing":
            fails.append(
                f"缺包诊断的错误码应为 dialc.package.missing，实际 {miss_diags[0].get('code')!r}"
            )
        elif (miss_diags[0].get("data") or {}).get("package") != "definitely_not_installed_pkg":
            fails.append(
                f"缺包诊断应带结构化 data.package，实际 {miss_diags[0].get('data')!r}"
            )
        else:
            print("[smoke] 缺包诊断已发布（code=dialc.package.missing，data.package 齐备）")

        # 23) 语义错误的诊断：必须有码，且**位置落在出错的语句上**（不是文件开头）。
        sem_uri = "file:///semantic_err.dial"
        sem_src = (
            "fn helper(): int {\n"
            "    return 1;\n"
            "}\n"
            "\n"
            "fn main(): int {\n"
            "    let c: string = helper();\n"
            "    return 0;\n"
            "}\n"
        )
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": sem_uri,
                    "languageId": "dial",
                    "version": 1,
                    "text": sem_src,
                }
            },
        )
        client.request("textDocument/documentSymbol", {"textDocument": {"uri": sem_uri}})
        sem_diags = []
        for n in client.notifications:
            if (
                n.get("method") == "textDocument/publishDiagnostics"
                and n.get("params", {}).get("uri") == sem_uri
            ):
                sem_diags = n["params"].get("diagnostics", [])
        if not sem_diags:
            fails.append("语义错误应发布诊断")
        elif sem_diags[0].get("code") != "dialc.sema":
            fails.append(f"语义诊断的错误码应为 dialc.sema，实际 {sem_diags[0].get('code')!r}")
        elif sem_diags[0]["range"]["start"]["line"] != 5:
            fails.append(
                f"语义诊断应落在出错语句（第 6 行 / 0 起第 5 行），"
                f"实际第 {sem_diags[0]['range']['start']['line']} 行"
            )
        else:
            print("[smoke] 语义诊断带码且定位到出错语句（第 6 行）")

        # 24) codeAction 认**错误码**，不认消息文本：给一条带 code 的诊断，
        #     即便消息文本与真实缺包文案不同，也该给出「安装包」动作。
        ca_by_code = client.request(
            "textDocument/codeAction",
            {
                "textDocument": {"uri": miss_uri},
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "context": {
                    "diagnostics": [
                        {
                            "code": "dialc.package.missing",
                            "data": {"package": "some_pkg"},
                            "message": "（消息文本故意与真实文案不同）",
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 1},
                            },
                        }
                    ]
                },
            },
        ) or []
        if not any(
            a.get("command", {}).get("command") == "dialc.install"
            and "some_pkg" in str(a.get("title", ""))
            for a in ca_by_code
        ):
            fails.append("codeAction 应按错误码 + data.package 给出安装动作")
        else:
            print("[smoke] codeAction 按错误码与结构化包名给出安装动作")

        # 26) **规范符合性**：逐项核对响应的**形状**与 LSP 3.17 规范一致。
        #     这是「只用标准、不私自扩展」的自动化保证——形状一旦跑偏，当场失败。
        #     依据（规范原文）：
        #       · Diagnostic.code?: integer | string —— 值由语言服务器自定，**无中央码表**；
        #       · Diagnostic.data?: LSPAny —— 规范明说「在 publishDiagnostics 与
        #         codeAction 之间保留」，正是我们传包名的方式；
        #       · WorkspaceEdit —— 「客户端不支持 documentChanges 时，只有 changes 里的
        #         纯 TextEdit 被支持」，故形状由客户端能力决定；
        #       · textDocument/formatting 的结果是 **TextEdit[] 本身**（不是任何包装）。
        def _is_loc(v):
            return isinstance(v, dict) and "uri" in v and "range" in v

        CONFORM = [
            ("textDocument/hover", {"textDocument": {"uri": args.uri}, "position": {"line": 0, "character": 3}},
             lambda r: r is None or (isinstance(r, dict) and "contents" in r),
             "null | {contents, range?}（Hover）"),
            ("textDocument/definition", {"textDocument": {"uri": args.uri}, "position": {"line": 0, "character": 3}},
             lambda r: r is None or _is_loc(r) or (isinstance(r, list) and all(_is_loc(x) for x in r)),
             "null | Location | Location[]（Definition）"),
            ("textDocument/completion", {"textDocument": {"uri": args.uri}, "position": {"line": 5, "character": 17}},
             lambda r: isinstance(r, dict) and "items" in r and "isIncomplete" in r,
             "{isIncomplete, items}（CompletionList）"),
            ("textDocument/documentSymbol", {"textDocument": {"uri": args.uri}},
             lambda r: isinstance(r, list) and all(
                 {"name", "kind", "range", "selectionRange"} <= set(x.keys()) for x in r),
             "DocumentSymbol[]（含 name/kind/range/selectionRange）"),
            ("workspace/symbol", {"query": "add"},
             lambda r: isinstance(r, list) and all(
                 {"name", "kind", "location"} <= set(x.keys()) for x in r),
             "SymbolInformation[]（含 name/kind/location）"),
            ("textDocument/references", {"textDocument": {"uri": args.uri},
                                          "position": {"line": 0, "character": 3},
                                          "context": {"includeDeclaration": True}},
             lambda r: isinstance(r, list) and all(_is_loc(x) for x in r),
             "Location[]（含 uri/range）"),
            ("textDocument/prepareRename", {"textDocument": {"uri": args.uri},
                                            "position": {"line": 0, "character": 3}},
             lambda r: r is None or (isinstance(r, dict) and "range" in r),
             "null | Range | {range, placeholder}"),
            ("textDocument/rename", {"textDocument": {"uri": args.uri},
                                      "position": {"line": 0, "character": 3}, "newName": "zz"},
             lambda r: isinstance(r, dict) and (
                 ("documentChanges" in r and all(
                     {"textDocument", "edits"} <= set(dc.keys()) and
                     {"uri", "version"} <= set(dc["textDocument"].keys())
                     for dc in r["documentChanges"]))
                 or ("changes" in r and all(
                     _is_loc(e) or ("range" in e and "newText" in e)
                     for lst in r["changes"].values() for e in lst))),
             "WorkspaceEdit（documentChanges 或 changes，按客户端能力）"),
            ("textDocument/signatureHelp", {"textDocument": {"uri": args.uri},
                                            "position": {"line": 5, "character": 21}},
             lambda r: r is None or (isinstance(r, dict) and "signatures" in r),
             "null | {signatures, activeSignature?, activeParameter?}"),
            ("textDocument/formatting", {"textDocument": {"uri": args.uri},
                                          "options": {"tabSize": 4, "insertSpaces": True}},
             lambda r: isinstance(r, list) and all(
                 {"range", "newText"} <= set(x.keys()) for x in r),
             "TextEdit[]（**直接是数组**，不是 {edits:[...]}）"),
            ("textDocument/inlayHint", {"textDocument": {"uri": args.uri},
                                         "range": {"start": {"line": 0, "character": 0},
                                                   "end": {"line": 20, "character": 0}}},
             lambda r: isinstance(r, list) and all(
                 {"position", "label"} <= set(x.keys()) for x in r),
             "InlayHint[]（含 position/label）"),
            ("textDocument/codeAction", {"textDocument": {"uri": args.uri},
                                          "range": {"start": {"line": 0, "character": 0},
                                                    "end": {"line": 0, "character": 1}},
                                          "context": {"diagnostics": []}},
             lambda r: isinstance(r, list) and all("title" in x for x in r),
             "CodeAction[]（含 title）"),
            ("textDocument/foldingRange", {"textDocument": {"uri": args.uri}},
             lambda r: isinstance(r, list) and all(
                 {"startLine", "endLine"} <= set(x.keys()) for x in r),
             "FoldingRange[]（含 startLine/endLine）"),
            ("textDocument/selectionRange", {"textDocument": {"uri": args.uri},
                                              "positions": [{"line": 1, "character": 4}]},
             lambda r: isinstance(r, list) and all(
                 "range" in x and set(x.keys()) <= {"range", "parent"} for x in r),
             "SelectionRange[]（含 range，可嵌套 parent）"),
            ("textDocument/documentLink", {"textDocument": {"uri": args.uri}},
             lambda r: isinstance(r, list) and all("range" in x for x in r),
             "DocumentLink[]（含 range/target?）"),
        ]
        for label, params, ok, spec in CONFORM:
            resp = client.request(label, params)
            if not ok(resp):
                fails.append(f"{label} 的返回形状不符合 LSP 规范：应为 {spec}，实际 {str(resp)[:120]}")
        print(f"[smoke] 规范符合性：{len(CONFORM)} 个方法的返回形状逐项核对完毕")

        # 27) WorkspaceEdit 的形状**由客户端能力决定**：客户端没声明
        #     `workspace.workspaceEdit.documentChanges` 时，规范说「只有 changes 里的
        #     纯 TextEdit 被支持」——此时服务端必须回退到 changes。
        mini = LspClient(dialc)
        try:
            mini.request("initialize", {"processId": None, "rootUri": None, "capabilities": {}})
            mini.notify("textDocument/didOpen", {"textDocument": {
                "uri": args.uri, "languageId": "dial", "version": 7, "text": SAMPLE}})
            rn_mini = mini.request("textDocument/rename", {
                "textDocument": {"uri": args.uri},
                "position": {"line": 5, "character": 17}, "newName": "zz"}) or {}
            if "changes" not in rn_mini:
                fails.append("客户端未声明 documentChanges 能力时，rename 应回退到 changes 形状")
            elif "documentChanges" in rn_mini:
                fails.append("客户端未声明 documentChanges 能力时，不应发 documentChanges")
            else:
                print("[smoke] 未声明 documentChanges 的客户端 → 正确回退到 changes 形状")
        finally:
            mini.close()

        # 21) 协议流纯净性：stdout 上不得出现任何非协议字节。
        if client.pollution:
            sample = client.pollution[0][:120]
            fails.append(
                f"stdout 出现非协议内容 {len(client.pollution)} 处，"
                f"协议流已被打乱（首例: {sample!r}）"
            )
        else:
            print("[smoke] stdout 协议流纯净（无裸字节）")

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
