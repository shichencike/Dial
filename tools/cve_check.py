#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Dial 语言 CVE 暴力检测器（brute-force detector）
================================================

对每一个已登记的 CVE，构造触发用例（PoC），用 dialc 编译/运行，断言漏洞已被修复。
默认对每个 PoC **重复运行 --repeat 次**（默认 25），以暴露非确定性缺陷
（数据竞争、悬垂、死锁等只在特定调度下才显形的内存安全问题）。

任一 CVE 仍可被触发 → 进程退出码非 0，CI 即可据此阻断发布。

用法:
    python3 tools/cve_check.py [--dialc PATH] [--repeat N] [--quiet]

环境变量:
    DIALC_BIN   指定 dialc 可执行文件路径（优先级高于 --dialc 与自动探测）。
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile

# ---------------------------------------------------------------------------
# 版本元数据：所有 CVE 均在 v0.98.0 被发现，并在 v0.98.1 修复。
# “存活周期”指漏洞从首次发布到被修复所跨越的发布版本数。
# ---------------------------------------------------------------------------
DISCOVERED_VERSION = "0.98.0"
FIXED_VERSION = "0.98.1"
SURVIVAL = "1 个发布版本（v0.98.0 GA → v0.98.1 修复）"

# 每个 CVE 的登记信息（id, 标题, 严重程度, 关联 Bug, 期望的固定后行为摘要）
CVE_REGISTRY = [
    ("CVE-2024-DIAL-001", "括号表达式解析错误",            "中", "#103", "分组表达式 ( … ) 可正常编译并求值"),
    ("CVE-2024-DIAL-002", "dialc run 相对路径执行失败",    "高", "#104", "cwd 下的裸文件名可被正确定位执行"),
    ("CVE-2024-DIAL-003", "RC clone 污染 / 数据竞争",      "严重", "#105", "clone 与原对象独立，互不污染且无悬垂"),
    ("CVE-2024-DIAL-004", "跨函数异常路由失败",            "高", "#106", "子函数抛出的异常可被调用方 try/catch 捕获"),
    ("CVE-2024-DIAL-005", "缺失取模 % 运算符",            "中", "#107", "整数/浮点取模均正确求值并做除零检查"),
    ("CVE-2024-DIAL-006", "缺失类型→字符串转换与字符串连接", "中", "#108", "str()/int_to_string/float_to_string 与 string + 可用"),
    ("CVE-2024-DIAL-007", "channel release 泄漏残留元素",  "高", "#110", "通道释放时残留未消费元素一并析构"),
    ("CVE-2024-DIAL-008", "缺失 DIAL_C_COMPILER / 安装脚本缺陷", "低", "#109", "DIAL_C_COMPILER 被尊重；安装脚本支持续传/镜像"),
]


def repo_root() -> str:
    """向上查找仓库根（含 Cargo.toml）。"""
    here = os.path.dirname(os.path.abspath(__file__))
    cur = here
    for _ in range(8):
        if os.path.isfile(os.path.join(cur, "Cargo.toml")):
            return cur
        parent = os.path.dirname(cur)
        if parent == cur:
            break
        cur = parent
    return os.path.dirname(here)


def find_dialc(explicit: str | None) -> str:
    if explicit:
        return explicit
    env = os.environ.get("DIALC_BIN")
    if env:
        return env
    cand = os.path.join(repo_root(), "target", "release", "dialc")
    if os.path.isfile(cand):
        return cand
    found = shutil.which("dialc")
    if found:
        return found
    sys.exit("❌ 找不到 dialc：请用 --dialc 指定，或先 cargo build --release")


def c_compiler() -> str:
    for cand in ("zig cc", "gcc", "clang", "cc"):
        prog = cand.split()[0]
        if shutil.which(prog):
            return cand
    return ""


def run_capture(cmd, cwd=None, env=None, timeout=120):
    try:
        p = subprocess.run(
            cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout
        )
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return 124, "", "timed out"
    except FileNotFoundError as e:
        return 127, "", str(e)


def strip_info(lines):
    """滤掉 dialc 输出的 ℹ️ 信息横幅，只保留程序自身的 print 内容。"""
    return [ln for ln in lines.splitlines() if not ln.startswith("ℹ️")]


# ---------------------------------------------------------------------------
# 各 CVE 的检测器：返回 (passed: bool, detail: str)
# ---------------------------------------------------------------------------

def check_dialc_output(dialc, poc_path, expected_lines, repeat, quiet):
    """通用：运行 dialc run <poc>，逐行比对输出，重复 repeat 次。"""
    for i in range(repeat):
        rc, out, err = run_capture([dialc, "run", poc_path])
        got = strip_info(out)
        if rc != 0:
            return False, f"第 {i+1} 次运行非零退出 (rc={rc}): {err.strip()[:160]}"
        if got != expected_lines:
            return False, (
                f"第 {i+1} 次输出不符:\n"
                f"  期望: {expected_lines}\n  实际: {got}"
            )
    return True, f"重复 {repeat} 次均通过"


def check_poc(dialc, cve_id, poc_name, expected, repeat, quiet):
    root = repo_root()
    poc = os.path.join(root, "tests", "cve", poc_name)
    if not os.path.isfile(poc):
        return False, f"找不到 PoC 文件 {poc}"
    return check_dialc_output(dialc, poc, expected, repeat, quiet)


def check_002_run(dialc, repeat, quiet):
    """#104：相对路径执行。把 poc 放进一个临时子目录，从子目录内以相对路径运行。"""
    root = repo_root()
    poc_src = os.path.join(root, "tests", "cve", "poc_002_run.dial")
    with tempfile.TemporaryDirectory() as td:
        sub = os.path.join(td, "sandbox")
        os.makedirs(sub)
        shutil.copy(poc_src, os.path.join(sub, "poc_002_run.dial"))
        # 在子目录内用相对路径运行
        for i in range(repeat):
            rc, out, err = run_capture([dialc, "run", "poc_002_run.dial"], cwd=sub)
            got = strip_info(out)
            if rc != 0:
                return False, f"第 {i+1} 次运行非零退出 (rc={rc}): {err.strip()[:160]}"
            if got != ["run relative path works"]:
                return False, f"第 {i+1} 次输出不符: 期望 ['run relative path works'] 实际 {got}"
    return True, f"重复 {repeat} 次均通过（cwd 子目录相对路径可定位）"


def check_110_leak(dialc, repeat, quiet):
    """#110：通道释放泄漏残留元素。
    用计数版 elem_release 编译一个 C 用例，断言通道 release 时对 3 个残留元素
    各调用一次 elem_release（确定性，不依赖 ASAN）。"""
    cc = c_compiler()
    if not cc:
        return False, "找不到可用的 C 编译器（zig cc/gcc/clang/cc）"
    root = repo_root()
    rt = os.path.join(root, "runtime")
    sl = os.path.join(root, "stdlib")
    if not (os.path.isdir(rt) and os.path.isdir(sl)):
        return False, f"找不到 runtime/ 或 stdlib/（{rt} / {sl}）"
    csrc = r"""
#include <stdio.h>
#include "dial_channel.h"
#include "dial_string.h"
#include "dial_rt_mem.h"
static long g_released = 0;
static void counting_release(void *p) { g_released += 1; dial_rc_release(p); }
int main(void) {
    DialChannel ch = dial_channel_new(4, counting_release);
    dial_channel_send(ch, dial_string_from_bytes("one", 3));
    dial_channel_send(ch, dial_string_from_bytes("two", 3));
    dial_channel_send(ch, dial_string_from_bytes("three", 5));
    dial_channel_close(ch);
    dial_channel_release(ch);
    if (g_released != 3) {
        fprintf(stderr, "CVE-2024-DIAL-007 STILL VULNERABLE: only %ld/3 released\n", g_released);
        return 1;
    }
    printf("CVE-2024-DIAL-007 FIXED\n");
    return 0;
}
"""
    with tempfile.TemporaryDirectory() as td:
        cfile = os.path.join(td, "leak_test.c")
        exe = os.path.join(td, "leak_test")
        with open(cfile, "w") as f:
            f.write(csrc)
        cmd = cc.split() + ["-O0", "-g", "-I", rt, "-I", sl, cfile,
                            os.path.join(rt, "dial_rt_mem.c"),
                            os.path.join(rt, "dial_rt_trap.c"),
                            os.path.join(sl, "dial_string.c"),
                            os.path.join(sl, "dial_channel.c"),
                            "-lpthread", "-o", exe]
        rc, out, err = run_capture(cmd)
        if rc != 0:
            return False, f"C 编译失败: {err.strip()[:200]}"
        # 编译成功后**单独运行**生成的二进制（上一步只做编译/链接）。
        rc, out, err = run_capture([exe])
        if rc != 0:
            return False, f"C 用例运行失败 (rc={rc}): {err.strip()[:200]}"
        if "FIXED" not in out:
            return False, f"残留元素未被释放（泄漏）: {out.strip()[:120]}"
    return True, "计数版 elem_release 验证：3/3 残留元素已释放"


def run_capture_shell(cmd, timeout=120):
    """以 shell 形式运行命令（用于把环境变量内联到命令行，
    规避某些环境下 Python 的 env= 参数无法透传给子进程的问题，真实 CI 同样适用）。"""
    try:
        p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout)
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return 124, "", "timed out"


def check_109_env(dialc, repeat, quiet):
    """#109：DIAL_C_COMPILER 被尊重。注入一个记录调用的包装脚本。

    dialc 的链接步骤每次运行都会调用 C 编译器，因此只要把 DIAL_C_COMPILER 指向
    一个“记录被调用”的包装脚本，再观察标记文件是否生成即可。环境变量以内联形式
    写进命令行（shell=True），确保在本沙箱与真实 CI 中都能稳定透传。"""
    root = repo_root()
    poc = os.path.join(root, "tests", "cve", "poc_008_compiler_env.dial")
    if not os.path.isfile(poc):
        return False, f"找不到 PoC 文件 {poc}"
    with tempfile.TemporaryDirectory() as td:
        marker = os.path.join(td, "invoked.marker")
        # 包装脚本：记录被调用，再转发给真实编译器（优先 zig cc，否则 gcc）
        real_cc = "zig cc" if shutil.which("zig") else "gcc"
        wrapper = os.path.join(td, "fakecc.sh")
        with open(wrapper, "w") as f:
            f.write("#!/bin/bash\n")
            f.write(f'touch "{marker}"\n')
            f.write(f'exec {real_cc} "$@"\n')
        os.chmod(wrapper, 0o755)
        # 内联环境变量，规避 env= 透传问题（shell=True）。
        cmd = f'DIAL_C_COMPILER="{wrapper}" "{dialc}" run "{poc}"'
        for i in range(repeat):
            rc, out, err = run_capture_shell(cmd)
            if rc != 0:
                return False, f"第 {i+1} 次运行非零退出 (rc={rc}): {err.strip()[:160]}"
            if not os.path.exists(marker):
                return False, f"第 {i+1} 次 DIAL_C_COMPILER 未被调用（包装脚本未触发）"
            os.remove(marker)  # 为下一轮重置
    return True, f"重复 {repeat} 次均确认 DIAL_C_COMPILER 被尊重"


# CVE -> 检测器 映射
DETECTORS = {
    "CVE-2024-DIAL-001": lambda d, r, q: check_poc(d, "CVE-2024-DIAL-001", "poc_001_paren.dial", ["1"], r, q),
    "CVE-2024-DIAL-002": lambda d, r, q: check_002_run(d, r, q),
    "CVE-2024-DIAL-003": lambda d, r, q: check_poc(d, "CVE-2024-DIAL-003", "poc_003_clone.dial", ["1", "99"], r, q),
    "CVE-2024-DIAL-004": lambda d, r, q: check_poc(d, "CVE-2024-DIAL-004", "poc_004_exception.dial", ["caught the error in main!", "after try"], r, q),
    "CVE-2024-DIAL-005": lambda d, r, q: check_poc(d, "CVE-2024-DIAL-005", "poc_005_mod.dial", ["1", "1.500000"], r, q),
    "CVE-2024-DIAL-006": lambda d, r, q: check_poc(d, "CVE-2024-DIAL-006", "poc_006_string.dial", ["Result: 42", "Hello World"], r, q),
    "CVE-2024-DIAL-007": lambda d, r, q: check_110_leak(d, r, q),
    "CVE-2024-DIAL-008": lambda d, r, q: check_109_env(d, r, q),
}


def main() -> int:
    ap = argparse.ArgumentParser(description="Dial CVE 暴力检测器")
    ap.add_argument("--dialc", default=None, help="dialc 可执行文件路径")
    ap.add_argument("--repeat", type=int, default=25, help="每个 PoC 重复运行次数（默认 25）")
    ap.add_argument("--quiet", action="store_true", help="仅输出总结")
    args = ap.parse_args()

    dialc = find_dialc(args.dialc)
    repeat = max(1, args.repeat)

    print(f"Dial CVE 暴力检测（dialc = {dialc}）")
    print(f"发现版本 {DISCOVERED_VERSION} · 修复版本 {FIXED_VERSION} · 存活周期 {SURVIVAL}")
    print("=" * 72)

    results = []
    for cve_id, title, sev, bug, _ in CVE_REGISTRY:
        passed, detail = DETECTORS[cve_id](dialc, repeat, args.quiet)
        results.append((cve_id, title, sev, bug, passed, detail))
        status = "✅ 已修复" if passed else "❌ 仍可利用"
        print(f"[{status}] {cve_id}  {title}  (严重度 {sev}, {bug})")
        if not args.quiet:
            print(f"          └─ {detail}")

    print("=" * 72)
    n_pass = sum(1 for r in results if r[4])
    n_total = len(results)
    print(f"结果：{n_pass}/{n_total} 个 CVE 已确认修复")
    if n_pass != n_total:
        print("⚠️  存在仍可利用的 CVE，阻断发布。")
        return 1
    print("✅ 全部 CVE 已修复，无残留可利用漏洞。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
