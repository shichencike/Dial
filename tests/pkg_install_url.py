#!/usr/bin/env python3
"""包安装端到端测试（v0.96 · 测试组 L）：URL 安装 + 一键安装脚本。

起一个**本机 HTTP 服务器**（python http.server）真实提供 `.dialpkg`，
然后用真正的 `dialc install <URL>` 去装，验证三条链路的正确性：

1. **下载 → 校验 → 解压**这条链路走得通（正常安装、`--sha256` 钉死也能装）；
2. **坏包一律装不进去**（内容被篡改 / 哈希不符 / 404 / 非 https）；
   且失败时**缓存目录不留残骸**——半截的包绝不能留在 ~/.dial 里；
3. **装完能用**：`import "demo";` 后编译并运行，输出与黄金文件一致。

另外覆盖**一键安装脚本**（`tools/install-dial.sh`，阶段 5）：
初始化环境（编译器 + stdlib/ + runtime/ + 配置）→ 自动扫描并安装 `.dialpkg`
→ 在**与 Dial 源码树无关的独立项目目录**里用装好的编译器编译 import 程序。

所有用例都在**隔离的 HOME** 下运行，不碰用户真实的 ~/.dial 缓存。

退出码 0 = 全部通过；非 0 = 有失败。
"""

import http.server
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import zipfile

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
            for line in str(detail).splitlines()[:8]:
                print(f"       {line}")
        failures.append(label)


def run(args, home, cwd=None):
    """在隔离 HOME 下运行 dialc，返回 (rc, stdout+stderr)。"""
    env = dict(os.environ)
    env["HOME"] = home
    env["LD_LIBRARY_PATH"] = "/workspace/dial-llvm-fixed/lib" + os.pathsep + env.get("LD_LIBRARY_PATH", "")
    p = subprocess.run([DIALC] + args, capture_output=True, text=True, env=env, cwd=cwd)
    return p.returncode, p.stdout + p.stderr


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def make_tampered(src, dst):
    """复制一个 .dialpkg，但把 .diali 里的接口签名改掉（manifest 的哈希保持不变）。

    这模拟「包在传输途中被改了内容」：zip 容器仍然合法，但里面的文件与
    manifest 登记的哈希对不上——必须在校验阶段被拦下。
    """
    zin = zipfile.ZipFile(src)
    with zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED) as zout:
        for it in zin.infolist():
            data = zin.read(it.filename)
            if it.filename.endswith(".diali"):
                data = data.replace(b"demo_add", b"demo_ADX")
            zout.writestr(it, data)
    zin.close()


def main():
    if not os.path.exists(DIALC):
        print("  ❌ 找不到 dialc，请先 cargo build --release")
        return 1

    print("== L) 包 URL 安装：下载 → 校验 → 解压（v0.96）==")

    tmp = tempfile.mkdtemp(prefix="dialc-l-")
    serve_dir = os.path.join(tmp, "serve")
    os.makedirs(serve_dir)
    home = os.path.join(tmp, "home")
    os.makedirs(home)

    # ---- 准备示例包 ----
    pkgdir = os.path.join(tmp, "pkg")
    r = subprocess.run(["bash", os.path.join(ROOT, "tools", "make_sample_pkg.sh"), pkgdir],
                       capture_output=True, text=True)
    if r.returncode != 0:
        print("  ❌ 示例包构建失败")
        print(r.stdout[-800:], r.stderr[-800:])
        return 1
    good_pkg = os.path.join(pkgdir, "demo.dialpkg")
    shutil.copy(good_pkg, os.path.join(serve_dir, "demo.dialpkg"))
    make_tampered(good_pkg, os.path.join(serve_dir, "tampered.dialpkg"))

    # ---- 起本机 HTTP 服务器 ----
    port = free_port()

    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **kw):
            super().__init__(*a, directory=serve_dir, **kw)

        def log_message(self, *a):  # 静音
            pass

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    t = threading.Thread(target=httpd.serve_forever, daemon=True)
    t.start()
    base = f"http://127.0.0.1:{port}"
    time_wait = 0.4
    import time
    time.sleep(time_wait)

    try:
        # ---- 1) 正常 URL 安装 ----
        rc, out = run(["install", f"{base}/demo.dialpkg"], home)
        check(rc == 0, "URL 安装成功", out)
        cached = os.path.join(home, ".dial", "packages", "demo")
        check(os.path.isfile(os.path.join(cached, "manifest.txt")), "包已落到 ~/.dial/packages/demo/")
        check(os.path.isfile(os.path.join(cached, "demo.diali")), "接口文件已解压")
        check("未指定 --sha256" in out, "未钉死哈希时给出明确提示（不静默降级）", out)

        # ---- 2) --sha256 正确 → 成功 ----
        h2 = os.path.join(tmp, "home2")
        os.makedirs(h2)
        import hashlib
        good_hash = hashlib.sha256(open(good_pkg, "rb").read()).hexdigest()
        rc, out = run(["install", f"{base}/demo.dialpkg", "--sha256", good_hash], h2)
        check(rc == 0, "--sha256 与实际一致时安装成功", out)

        # ---- 3) --sha256 不符 → 必须失败，且不留残骸 ----
        h3 = os.path.join(tmp, "home3")
        os.makedirs(h3)
        rc, out = run(["install", f"{base}/demo.dialpkg", "--sha256", "0" * 64], h3)
        check(rc != 0, "--sha256 不符时安装失败", out)
        check("SHA256" in out, "报错明确指出是 SHA256 问题", out)
        left = os.path.join(h3, ".dial", "packages")
        n_left = len(os.listdir(left)) if os.path.isdir(left) else 0
        check(n_left == 0, "失败后缓存目录无残留（不留半截的包）", f"残留 {n_left} 项")

        # ---- 4) 包内容被篡改 → 解压前逐文件校验必须拦下 ----
        h4 = os.path.join(tmp, "home4")
        os.makedirs(h4)
        rc, out = run(["install", f"{base}/tampered.dialpkg"], h4)
        check(rc != 0, "内容被篡改的包装不进去", out)
        check("SHA256" in out, "报错明确指出是 SHA256 问题", out)
        left = os.path.join(h4, ".dial", "packages")
        n_left = len(os.listdir(left)) if os.path.isdir(left) else 0
        check(n_left == 0, "被篡改的包未落盘（校验先于解压）", f"残留 {n_left} 项")

        # ---- 5) 404 → 失败 ----
        h5 = os.path.join(tmp, "home5")
        os.makedirs(h5)
        rc, out = run(["install", f"{base}/nope.dialpkg"], h5)
        check(rc != 0, "404 时安装失败", out)

        # ---- 6) 非 https 且非本机 → 直接拒绝（连请求都不发） ----
        h6 = os.path.join(tmp, "home6")
        os.makedirs(h6)
        rc, out = run(["install", "http://example.com/x.dialpkg"], h6)
        check(rc != 0, "非本机的明文 http 被拒绝", out)
        check("https" in out, "报错说明了必须用 https", out)

        # ---- 7) 装完能用：import → 编译 → 运行 ----
        src = os.path.join(tmp, "use_demo.dial")
        with open(src, "w", encoding="utf-8") as f:
            f.write('import "demo";\n\n'
                    'fn main(): int {\n'
                    '    let n: int = demo_add(20, 22);\n'
                    '    print(n);\n'
                    '    return 0;\n'
                    '}\n')
        exe = os.path.join(tmp, "use_demo")
        rc, out = run(["build", src, "-o", exe], home)
        check(rc == 0, "装完的包能被 import 并编译", out)
        if rc == 0:
            env = dict(os.environ)
            env["LD_LIBRARY_PATH"] = "/workspace/dial-llvm-fixed/lib" + os.pathsep + env.get("LD_LIBRARY_PATH", "")
            p = subprocess.run([exe], capture_output=True, text=True, env=env)
            check(p.returncode == 0 and p.stdout.strip() == "42",
                  "运行结果正确（demo_add(20,22) = 42）", f"rc={p.returncode} out={p.stdout!r}")

        # ---- 8~10) 一键安装脚本（阶段 5）----
        # 脚本要在**隔离 HOME** 下跑，并且必须把 stdlib/ 与 runtime/ 一起装上——
        # 否则用户在自己项目里编译会报「找不到 stdlib/ 与 runtime/ 目录」。
        sh_home = os.path.join(tmp, "shome")
        sh_root = os.path.join(tmp, "shroot")
        os.makedirs(sh_home)
        os.makedirs(sh_root)
        shutil.copy(DIALC, os.path.join(sh_root, "dialc"))
        shutil.copytree(os.path.join(ROOT, "stdlib"), os.path.join(sh_root, "stdlib"))
        shutil.copytree(os.path.join(ROOT, "runtime"), os.path.join(sh_root, "runtime"))
        work = os.path.join(tmp, "work")
        os.makedirs(work)
        shutil.copy(good_pkg, os.path.join(work, "demo.dialpkg"))

        env = dict(os.environ)
        env["HOME"] = sh_home
        env["DIALC_BIN"] = os.path.join(sh_root, "dialc")
        env["DIALC_SRC"] = sh_root
        env["LD_LIBRARY_PATH"] = "/workspace/dial-llvm-fixed/lib" + os.pathsep + env.get("LD_LIBRARY_PATH", "")
        p = subprocess.run(["bash", os.path.join(ROOT, "tools", "install-dial.sh")],
                           capture_output=True, text=True, env=env, cwd=work)
        check(p.returncode == 0, "一键脚本执行成功", p.stdout[-600:] + p.stderr[-400:])
        dial_home = os.path.join(sh_home, ".dial")
        check(os.path.isfile(os.path.join(dial_home, "bin", "dialc")), "一键脚本把编译器挂进了 ~/.dial/bin")
        check(os.path.isdir(os.path.join(dial_home, "stdlib")), "一键脚本装上了 stdlib/")
        check(os.path.isdir(os.path.join(dial_home, "runtime")), "一键脚本装上了 runtime/")
        check(os.path.isfile(os.path.join(dial_home, "config.txt")), "一键脚本写了工具链配置")
        check(os.path.isdir(os.path.join(dial_home, "packages", "demo")),
              "一键脚本自动扫描并安装了 .dialpkg", p.stdout[-600:])
        check("找到 1 个包" in p.stdout, "一键脚本报告了扫描到的包", p.stdout[-400:])
        # 一键安装还应包含 VS Code 插件那一步（下载 .vsix + code --install-extension）。
        # 没装 VS Code 时它只告警不阻断——所以这里只查「步骤在、且没让脚本失败」。
        check("安装 VS Code 插件" in p.stdout,
              "一键脚本包含 VS Code 插件安装步骤", p.stdout[-600:])

        # ---- 11) 装好后，在**用户自己的项目目录**里能编译 import 程序 ----
        # 这一条是「一键安装」的意义所在：编译器、stdlib、runtime、包，四者齐备，
        # 且项目目录与 Dial 源码树毫无关系。
        myproj = os.path.join(tmp, "myproj")
        os.makedirs(myproj)
        with open(os.path.join(myproj, "p.dial"), "w", encoding="utf-8") as f:
            f.write('import "demo";\n\n'
                    'fn main(): int {\n'
                    '    let n: int = demo_add(1, 2);\n'
                    '    print(n);\n'
                    '    return 0;\n'
                    '}\n')
        env2 = dict(env)
        env2["HOME"] = sh_home
        p = subprocess.run([os.path.join(dial_home, "bin", "dialc"), "build", "p.dial", "-o", "p"],
                           capture_output=True, text=True, env=env2, cwd=myproj)
        check(p.returncode == 0, "在独立项目目录里用装好的编译器编译 import 程序成功",
              p.stdout[-500:] + p.stderr[-300:])
        if p.returncode == 0:
            p2 = subprocess.run([os.path.join(myproj, "p")], capture_output=True, text=True, cwd=myproj)
            check(p2.returncode == 0 and p2.stdout.strip() == "3",
                  "运行结果正确（demo_add(1,2) = 3）", f"rc={p2.returncode} out={p2.stdout!r}")
    finally:
        httpd.shutdown()
        httpd.server_close()
        shutil.rmtree(tmp, ignore_errors=True)

    print()
    if failures:
        print(f"== 包 URL 安装测试失败 {len(failures)} 项 ❌ ==")
        return 1
    print("== 包 URL 安装测试全部通过 ✅ ==")
    return 0


if __name__ == "__main__":
    sys.exit(main())
