#!/usr/bin/env python3
"""`dialc lsp` 端到端冒烟测试。

这不是单元测试，而是**真的把 Language Server 当黑盒驱动**：
起子进程、按 LSP 分帧写 JSON-RPC、读回响应，逐条断言。

为什么值得单独写一个脚本（而不是塞进 run_tests.sh 用 shell 拼 JSON）：
LSP 的消息是差分编码的二进制式整数流，shell 里没法可靠地解码 5 元组并反查源码子串。
这里要做的最有价值的一步恰恰是那个反查——
「高亮说第 3 行第 5 列起 6 个字符是关键字」，那就把源码那一段切出来看看是不是 `return`。
只有做了这一步，才能证明列偏移与 UTF-16 换算没算错。

退出码 0 = 全部通过；非 0 = 有失败。
"""

import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
DIALC = os.path.join(ROOT, "target", "release", "dialc")

failures = []


def check(cond, label, detail=""):
    if cond:
        print(f"  ✅ {label}")
    else:
        print(f"  ❌ {label}")
        if detail:
            for line in str(detail).splitlines():
                print(f"       {line}")
        failures.append(label)


class Server:
    def __init__(self):
        env = dict(os.environ)
        # dialc 依赖本机 LLVM 运行库（libLLVM.so.18.1rc），运行期必须找得到
        lib = "/workspace/dial-llvm-fixed/lib"
        env["LD_LIBRARY_PATH"] = lib + os.pathsep + env.get("LD_LIBRARY_PATH", "")
        self.p = subprocess.Popen(
            [DIALC, "lsp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

    def send(self, msg):
        body = json.dumps(msg).encode()
        self.p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.p.stdin.flush()

    def notify(self, method, params=None):
        m = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            m["params"] = params
        self.send(m)

    def request(self, method, params=None):
        m = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        self.next_id += 1
        if params is not None:
            m["params"] = params
        self.send(m)
        # 读响应时跳过中间夹带的通知（诊断就是这种）
        while True:
            msg = self.recv()
            if msg is None:
                return None
            if msg.get("id") == m["id"]:
                return msg
            self.side.append(msg)

    next_id = 1
    side = None

    def recv(self):
        # 头部
        length = None
        while True:
            line = self.p.stdout.readline()
            if line == b"":
                return None
            line = line.decode().strip()
            if not line:
                break
            if line.lower().startswith("content-length:"):
                length = int(line.split(":")[1].strip())
        if length is None:
            return None
        body = self.p.stdout.read(length)
        return json.loads(body)

    def close(self):
        try:
            self.p.stdin.close()
        except Exception:
            pass
        try:
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


def decode_tokens(data):
    """把差分的 5 元组流还原成 (line, col, length, tokenType, tokenModifiers)。"""
    out = []
    line = 0
    col = 0
    for i in range(0, len(data), 5):
        dl, dc, ln, tt, tm = data[i : i + 5]
        line += dl
        col = dc if dl else col + dc
        out.append((line, col, ln, tt, tm))
    return out


# 含中文注释、中文字符串与 emoji 字符串，专门用来验证 UTF-16 列换算。
#
# 注意区分两种「多字节」：
#   - 中文「你」在 BMP 内，1 个 char == 1 个 UTF-16 码元 —— 它**测不出**换算错误；
#   - emoji「🙂」在 BMP 外，1 个 char == **2** 个码元 —— 只有它才能真正暴露
#     「把 char 列当 UTF-16 列发出去」这个 bug（后面所有 token 会整体右偏）。
SRC_BAD = '''fn main(): int {
    // 中文注释
    let s: string = "你好";
    let c: char = 'a' + 1;
    let e: string = "\U0001F642";
    return 0;
}
'''

SRC_GOOD = '''fn main(): int {
    let s: string = "你好";
    let c: char = 'a';
    print(c);
    return 0;
}
'''

TOKEN_TYPES = ["keyword", "string", "number", "operator", "variable", "type"]


def main():
    if not os.path.exists(DIALC):
        print("  ❌ 找不到 dialc，请先 cargo build --release")
        return 1

    print("== LSP 冒烟测试 ==")
    srv = Server()
    srv.side = []

    # 1) initialize：能力协商
    r = srv.request("initialize", {"processId": os.getpid(), "rootUri": None, "capabilities": {}})
    check(r is not None and "result" in r, "initialize 有响应")
    caps = (r or {}).get("result", {}).get("capabilities", {})
    # textDocumentSync 必须是**选项形式**才表达得了「请发 didSave」——
    # 纯数字形式里没有 save 字段，客户端就不会发 didSave，诊断便刷不出来。
    sync = caps.get("textDocumentSync")
    check(isinstance(sync, dict), "textDocumentSync 是选项形式（含 save）", f"实际: {sync!r}")
    if isinstance(sync, dict):
        check(sync.get("change") == 1, "textDocumentSync.change = 1（全量同步）", f"实际: {sync.get('change')!r}")
        check(bool(sync.get("save")), "textDocumentSync.save 为真（要收到 didSave）", f"实际: {sync.get('save')!r}")
    legend = caps.get("semanticTokensProvider", {}).get("legend", {}).get("tokenTypes", [])
    check(legend == TOKEN_TYPES, "semanticTokens legend 与实现一致", f"实际: {legend}")
    server_info = (r or {}).get("result", {}).get("serverInfo", {})
    check(server_info.get("name") == "dialc", "serverInfo.name = dialc", server_info)

    srv.notify("initialized", {})

    # 2) didOpen 一份有语义错误的代码 -> 必须收到诊断
    uri = "file:///tmp/t_bad.dial"
    srv.notify(
        "textDocument/didOpen",
        {"textDocument": {"uri": uri, "languageId": "dial", "version": 1, "text": SRC_BAD}},
    )
    diag_msg = None
    for _ in range(10):
        m = srv.recv()
        if m is None:
            break
        if m.get("method") == "textDocument/publishDiagnostics":
            diag_msg = m["params"]
            break
    check(diag_msg is not None, "didOpen 后推送了 publishDiagnostics")
    diags = (diag_msg or {}).get("diagnostics", [])
    check(len(diags) >= 1, "有诊断（char 参与算术应报错）", diags)
    d0 = diags[0] if diags else {}
    check(d0.get("severity") == 1, "诊断 severity = 1（Error）", d0)
    check(d0.get("source") == "dialc", "诊断 source = dialc", d0)
    # `let c: char = 'a' + 1;` 在源码第 4 行，0 起即 line=3。
    # 这一条是本次改动的核心回归点：语义错误原本全都报 (0,0)，
    # 在编辑器里表现为「第一行一个红点」，等于没定位。
    start = d0.get("range", {}).get("start", {})
    check(start.get("line") == 3, "诊断定位到第 4 行（0 起 line=3）", start)
    check(
        "char" in d0.get("message", ""),
        "诊断文案针对 char 给出了专门说明",
        d0.get("message"),
    )

    # 3) 语义 token：解码后反查源码子串，证明列偏移没算错
    r = srv.request("textDocument/semanticTokens/full", {"textDocument": {"uri": uri}})
    data = (r or {}).get("result", {}).get("data", [])
    check(len(data) > 0 and len(data) % 5 == 0, "semanticTokens 返回合法的 5 元组流", data[:20])
    toks = decode_tokens(data)

    lines = SRC_BAD.split("\n")

    def slice_of(tok):
        ln, col, length, _tt, _tm = tok
        text = lines[ln]
        # LSP 列是 UTF-16 码元，源码里切也要按码元切
        u = text.encode("utf-16-le")
        start = col * 2
        return u[start : start + length * 2].decode("utf-16-le")

    def find_token(text, ttype=None):
        for t in toks:
            if slice_of(t) == text and (ttype is None or t[3] == ttype):
                return t
        return None

    kw = TOKEN_TYPES.index("keyword")
    var = TOKEN_TYPES.index("variable")
    typ = TOKEN_TYPES.index("type")
    st = TOKEN_TYPES.index("string")
    op = TOKEN_TYPES.index("operator")

    check(find_token("fn", kw) is not None, "`fn` 被标为关键字")
    check(find_token("return", kw) is not None, "`return` 被标为关键字")
    check(find_token("let", kw) is not None, "`let` 被标为关键字")
    check(find_token("int", typ) is not None, "`int` 被标为类型")
    check(find_token("char", typ) is not None, "`char` 被标为类型")
    check(find_token("main", var) is not None, "`main` 被标为变量（骨架不区分函数）")
    check(find_token("'a'", st) is not None, "字符字面量 `'a'` 被标为字符串")
    # 中文字符串：这是 UTF-16 换算的关键用例——`"你好"` 与后面的 `;` 必须各就各位
    # 中文：BMP 内，`"你好"` 是 4 个 char 也是 4 个码元（这条证明长度本身没算错）
    cn = find_token('"你好"', st)
    check(cn is not None and cn[2] == 4, "中文字符串 `\"你好\"` 长度正确（4 个码元）", cn)

    # emoji：BMP 外，`"🙂"` 是 3 个 char 但 **4** 个码元。
    # 只有这一条能真正证明 UTF-16 换算生效——没换算时长度会是 3、且后续 token 全部右偏 1。
    em = find_token('"\U0001F642"', st)
    check(
        em is not None and em[2] == 4,
        "emoji 字符串 `\"\U0001F642\"` 长度按 UTF-16 算为 4（char 数只有 3）",
        em,
    )
    if em:
        # 同一个 token 之后紧跟分号：列偏移必须正好接上长度，差一个就说明换算没做
        after = [t for t in toks if t[0] == em[0] and t[1] == em[1] + em[2]]
        check(
            any(slice_of(t) == ";" and t[3] == op for t in after),
            "emoji 之后紧跟的 `;` 列偏移正确（UTF-16 换算确实生效）",
            after,
        )

    # 4) didChange 改成正确代码 + didSave -> 诊断必须被清空
    srv.notify(
        "textDocument/didChange",
        {
            "textDocument": {"uri": uri, "version": 2},
            "contentChanges": [{"text": SRC_GOOD}],
        },
    )
    srv.notify("textDocument/didSave", {"textDocument": {"uri": uri}})
    cleared = None
    for _ in range(10):
        m = srv.recv()
        if m is None:
            break
        if m.get("method") == "textDocument/publishDiagnostics":
            cleared = m["params"]
            break
    check(cleared is not None, "didSave 后重新推送了诊断")
    check(
        cleared is not None and cleared.get("diagnostics") == [],
        "改正后诊断被清空（不再有红色波浪线）",
        cleared,
    )

    # 5) didClose：清掉残留诊断
    srv.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
    got_close = False
    for _ in range(5):
        m = srv.recv()
        if m is None:
            break
        if m.get("method") == "textDocument/publishDiagnostics":
            got_close = True
            break
    check(got_close, "didClose 后清空了该文件的诊断")

    # 6) 未知请求也必须回应（否则客户端会一直等）
    r = srv.request("textDocument/hover", {"textDocument": {"uri": uri}, "position": {"line": 0, "character": 0}})
    check(r is not None and "result" in r or (r or {}).get("error"), "未知请求也有回应（不会挂死客户端）", r)

    # 7) shutdown / exit：正常退出
    srv.request("shutdown")
    srv.notify("exit")
    try:
        rc = srv.p.wait(timeout=5)
        check(rc == 0, "shutdown/exit 后进程干净退出", f"rc={rc}")
    except Exception as e:
        check(False, "shutdown/exit 后进程干净退出", f"超时: {e}")
        srv.p.kill()

    err = srv.p.stderr.read().decode() if srv.p.stderr else ""
    check(err.strip() == "", "stderr 无异常输出（日志不得污染协议流）", err[:400])

    print()
    if failures:
        print(f"== LSP 冒烟测试失败 {len(failures)} 项 ❌ ==")
        return 1
    print("== LSP 冒烟测试全部通过 ✅ ==")
    return 0


if __name__ == "__main__":
    sys.exit(main())
