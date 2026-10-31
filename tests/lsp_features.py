#!/usr/bin/env python3
"""`dialc lsp` 增强特性端到端测试（v0.95）：Hover / 跳转定义 / 补全。

同样是**真起子进程、按 LSP 协议往返**，但聚焦三件事：

1. Hover 能区分同名变量的遮蔽、并能从 import 的包 `.diali` 里拿到文档；
2. 跳转定义对文件内符号返回本文件 Location、对包符号跳进 `.diali`；
3. 补全收集光标处可见符号、并按已输入前缀过滤。

退出码 0 = 全部通过；非 0 = 有失败。
"""

import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
DIALC = os.path.join(ROOT, "target", "release", "dialc")
PKG_CACHE = os.path.expanduser("~/.dial/packages/demo")

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
        lib = "/workspace/dial-llvm-fixed/lib"
        env["LD_LIBRARY_PATH"] = lib + os.pathsep + env.get("LD_LIBRARY_PATH", "")
        self.p = subprocess.Popen(
            [DIALC, "lsp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        self.next_id = 1
        self.side = []

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
        while True:
            msg = self.recv()
            if msg is None:
                return None
            if msg.get("id") == m["id"]:
                return msg
            self.side.append(msg)

    def recv(self):
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


# 外层 x 是 string，内层 if 块里同名 x 是 int——专门用来验证「遮蔽」真的查对了作用域。
# （Dial 没有裸块语句，用 `if (true) { }` 制造嵌套作用域；不影响索引测试。）
# demo_add 来自 import 的 "demo" 包，其 `.diali` 里有 `/// 两数相加`。
SRC = '''import "demo";

fn main(): int {
    let x: string = "outer";
    var total: int = 0;
    if (true) {
        let x: int = 2;
        total = demo_add(x, 3);
    }
    return total;
}
'''

URI = "file:///tmp/feature.dial"


def main():
    if not os.path.exists(DIALC):
        print("  ❌ 找不到 dialc，请先 cargo build --release")
        return 1

    # 确保 demo 包已安装（K 组依赖它做跨文件符号）
    if not os.path.isdir(PKG_CACHE):
        print("  (安装 demo 包 …)")
        r = subprocess.run([DIALC, "install", os.path.join(ROOT, "tools", "make_sample_pkg.sh")],
                           capture_output=True, text=True)
        # make_sample_pkg 是脚本不是包；真正的包在 tests 之外由 run_tests 准备。
        # 这里直接调用脚本生成并安装，覆盖独立运行场景。
        import tempfile
        d = tempfile.mkdtemp()
        subprocess.run(["bash", os.path.join(ROOT, "tools", "make_sample_pkg.sh"), d],
                       capture_output=True, text=True)
        subprocess.run([DIALC, "install", os.path.join(d, "demo.dialpkg")],
                       capture_output=True, text=True)

    print("== LSP 增强特性测试（Hover / 定义 / 补全）==")
    srv = Server()
    srv.side = []

    r = srv.request("initialize", {"processId": os.getpid(), "rootUri": None, "capabilities": {}})
    check(r is not None and "result" in r, "initialize 有响应")
    caps = (r or {}).get("result", {}).get("capabilities", {})
    check(caps.get("hoverProvider") is True, "hoverProvider = true")
    check(caps.get("definitionProvider") is True, "definitionProvider = true")
    check(
        (caps.get("completionProvider") or {}).get("triggerCharacters") == ["."],
        "completionProvider.triggerCharacters = ['.']",
        caps.get("completionProvider"),
    )
    srv.notify("initialized", {})

    # 打开文件：触发（诊断 + 索引）
    srv.notify(
        "textDocument/didOpen",
        {"textDocument": {"uri": URI, "languageId": "dial", "version": 1, "text": SRC}},
    )
    # 吃掉 didOpen 触发的诊断通知
    for _ in range(10):
        m = srv.recv()
        if m is None:
            break
        if m.get("method") == "textDocument/publishDiagnostics":
            break

    # ---- 1) Hover：文件内同名变量遮蔽 ----
    # 内层 x 在第 6 行（0 起 line=5）`let x: int = 2;`，'x' 在 0 起 char=12。
    r = srv.request("textDocument/hover", {
        "textDocument": {"uri": URI},
        "position": {"line": 6, "character": 12},
    })
    hov = (r or {}).get("result")
    check(hov is not None, "Hover 返回了内容", r)
    markdown = ""
    if hov:
        c = hov.get("contents", {})
        markdown = c.get("value", "") if isinstance(c, dict) else str(c)
    # 内层 x 是 int；若遮蔽查错（取到外层 string），这里就会变成 string。
    check("int" in markdown, "Hover 内层 x 解析为 int（变量遮蔽正确）", markdown)
    check("string" not in markdown, "Hover 没有错误地取到外层 string（证明查的是内层作用域）", markdown)

    # ---- 2) Hover：import 的包函数，从 .diali 拿到文档 ----
    # `demo_add(x, 3)` 在第 7 行（0 起 line=6），'d' 在 0 起 char=24。
    r = srv.request("textDocument/hover", {
        "textDocument": {"uri": URI},
        "position": {"line": 7, "character": 24},
    })
    hov2 = (r or {}).get("result") or {}
    md2 = ""
    c = hov2.get("contents", {})
    if isinstance(c, dict):
        md2 = c.get("value", "")
    check("demo_add" in md2, "Hover 包函数显示了函数名", md2)
    check("两数相加" in md2, "Hover 包函数显示了 .diali 的 `///` 文档（跨文件符号）", md2)
    check("外部函数" in md2, "Hover 包函数标注为外部函数", md2)

    # ---- 3) 跳转定义：文件内符号 -> 本文件 Location ----
    r = srv.request("textDocument/definition", {
        "textDocument": {"uri": URI},
        "position": {"line": 6, "character": 12},  # 内层 x 的声明处（let x: int = 2）
    })
    defn = (r or {}).get("result")
    check(defn is not None, "跳转定义返回了位置", r)
    if defn:
        check(defn.get("uri") == URI, "定义指向本文件 uri", defn)
        rng = defn.get("range", {})
        # 内层 x 声明在第 6 行（0 起 line=5）；若跳到了外层（line=3）就是遮蔽查错。
        check(rng.get("start", {}).get("line") == 6, "定义定位到内层 x 声明行（line=6）", rng)
        check(rng.get("start", {}).get("character") == 12, "定义列偏移正确（光标定在 'x' 上）", rng)

    # ---- 4) 跳转定义：包符号 -> .diali 文件 ----
    r = srv.request("textDocument/definition", {
        "textDocument": {"uri": URI},
        "position": {"line": 7, "character": 24},  # demo_add 处
    })
    defn2 = (r or {}).get("result") or {}
    check(defn2.get("uri", "").endswith(".diali"), "包符号跳转定义指向 .diali 文件（跨文件）", defn2)

    # ---- 5) 补全：光标处可见符号收集（空白光标 -> 全量，含包函数与不透明类型） ----
    # 第 8 行（0 起 line=7）`total = demo_add(x, 3);`，character=27 落在逗号后的空白上
    # （前缀为空），此时应给出当前可见的全部符号。
    r = srv.request("textDocument/completion", {
        "textDocument": {"uri": URI},
        "position": {"line": 7, "character": 27},
    })
    items = (r or {}).get("result", {}).get("items", [])
    labels = [it.get("label") for it in items]
    check(len(items) > 0, "补全返回了候选", items[:6])
    check("demo_add" in labels, "补全面包含 import 的包函数 demo_add", labels)
    check("DemoHandle" in labels, "补全包含包导出的不透明类型 DemoHandle", labels)
    check("x" in labels, "补全包含内层局部变量 x", labels)
    check("total" in labels, "补全包含局部变量 total", labels)

    # ---- 6) 补全：前缀过滤（落在 demo_add 的 'd' 上 -> 只留 demo_add） ----
    # character=17 落在 `demo_add` 的 'd' 上（前缀 "demo_add"）。
    r = srv.request("textDocument/completion", {
        "textDocument": {"uri": URI},
        "position": {"line": 7, "character": 17},
    })
    items2 = (r or {}).get("result", {}).get("items", [])
    labels2 = [it.get("label") for it in items2]
    check("demo_add" in labels2, "前缀 'demo_add' 过滤后仍含 demo_add", labels2)
    check("total" not in labels2, "前缀过滤排除了非匹配的局部变量 total", labels2)
    check("DemoHandle" not in labels2, "前缀过滤排除了首字母不符的 DemoHandle", labels2)

    # ---- 7) 干净退出 ----
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
        print(f"== LSP 增强特性测试失败 {len(failures)} 项 ❌ ==")
        return 1
    print("== LSP 增强特性测试全部通过 ✅ ==")
    return 0


if __name__ == "__main__":
    sys.exit(main())
