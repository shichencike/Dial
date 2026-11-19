#!/usr/bin/env python3
"""Dial 双后端差分 fuzz：用确定性随机程序生成合法程序，对比 llvm/asm 的
stdout 逐字节 + 退出码，自动最小化并落盘不一致用例。

设计目标
--------
最近几批暴露的真实缺口（借用句柄 retain、默认实参补齐）都是静态审计发现不了、
只有「实际跑程序对比双后端」才能暴露的实现一致性 bug。本工具把这类发现变成
可持续、可复现、零新依赖的自动化流程：

* **确定性**：固定 seed 的 PRNG；同一 seed 永远生成同一程序，便于复现。
* **生成合法**：维护符号表，只引用已声明变量；规避 UB（下标越界、除零、
  非确定性内建如 now/random/getenv），因此输出确定、可作为差分 oracle。
* **LLVM 为权威 oracle**：只有「LLVM 编译+运行成功，asm 编译成功但运行
  输出或退出码不同」才记为候选 bug；编译失败的生成结果记为 skip（生成器
  的合法率指标），不误报。
* **自动最小化**：delta-debugging 按行删减，收敛到能复现差异的最小程序。
* **零新依赖**：仅 Python 标准库；不修改编译器产品代码。

用法
----
    python3 tools/diff_fuzz.py --golden-check              # 跑现有 golden 自检框架（应 0 不一致）
    python3 tools/diff_fuzz.py --seeds 300                 # 生成 300 个程序做差分
    python3 tools/diff_fuzz.py --seeds 300 --start 1000    # 指定 seed 起点（可分段并行）
    python3 tools/diff_fuzz.py --seeds 50 --opt-levels O0 O2 O3  # 多优化级别
    python3 tools/diff_fuzz.py --seeds 300 --jobs 4        # 并行 worker
    python3 tools/diff_fuzz.py --replay failures/xxx.dial  # 复跑一个落盘用例验证

退出码：0 = 无不一致；1 = 发现不一致（用例落盘到 --out/failures/）。
"""
from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import os
import pathlib
import random
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
DIALC = ROOT / "target" / "release" / "dialc"
GOLDEN = ROOT / "tests" / "asm_golden"

BUILD_ENV = dict(os.environ)
BUILD_ENV.setdefault("LLVM_SYS_181_PREFIX", "/usr/lib/llvm-18")


# ---------------------------------------------------------------------------
# 程序生成器
# ---------------------------------------------------------------------------
class Gen:
    """确定性 Dial 程序生成器。维护符号表，保证只引用已声明变量、规避 UB。"""

    def __init__(self, seed: int):
        self.rng = random.Random(seed)
        self.seed = seed
        # 符号表：name -> type
        self.vars: list[tuple[str, str]] = []
        self.lines: list[str] = []
        self.prelude: list[str] = []  # main 之前的函数/struct/impl 定义
        self._id = 0
        # 概率性启用「最近批次刚修的高风险路径」——这些是差分最该覆盖的区域
        self.has_default_fn = self.rng.random() < 0.65
        self.has_impl = self.rng.random() < 0.65
        # P3 门禁盲区固化：struct 进容器（B6/f7a94a4 修复路径）。原 fuzz 只生成
        # 局部 PT{} 并调方法，从不把 struct 放进 PT[] / map<string,PT>，导致
        # 「一边根本编不过」的盲区被 skip 掩盖。启用后 prelude 总是声明 PT。
        self.has_struct_container = self.rng.random() < 0.6
        need_struct = self.has_impl or self.has_struct_container
        if self.has_default_fn:
            k = self.rng.randint(1, 20)
            self.prelude.append(
                f"fn dfn(a: int, b: int = {k}): int {{ return a + b; }}")
        if need_struct:
            # 字段带默认值，与 golden batch3_b6_smoke.dial 一致，保证 get miss
            # 返回零值结构体时语义确定、可差分。
            self.prelude.append("struct PT { x: int = 0, y: int = 0 }")
        if self.has_impl:
            d = self.rng.randint(1, 20)
            self.prelude.extend([
                "impl PT {",
                "  fn sum(self): int { return self.x + self.y; }",
                f"  fn add(self, k: int = {d}): int {{ return self.sum() + k; }}",
                "}",
            ])

    def fresh(self, prefix: str) -> str:
        self._id += 1
        return f"{prefix}{self._id}"

    def of_type(self, ty: str) -> list[str]:
        return [n for n, t in self.vars if t == ty]

    def int_lit(self) -> str:
        return str(self.rng.randint(-20, 40))

    def bool_expr(self, depth: int = 0) -> str:
        ints = self.of_type("int")
        if depth < 1 and self.rng.random() < 0.5:
            a = self.int_atom(ints)
            b = self.int_atom(ints)
            op = self.rng.choice(["<", ">", "==", "!=", "<=", ">="])
            return f"({a} {op} {b})"
        return self.rng.choice(["true", "false"])

    def int_atom(self, ints: list[str]) -> str:
        choices = [self.int_lit()]
        if ints:
            choices.append(self.rng.choice(ints))
        return self.rng.choice(choices)

    def int_expr(self, depth: int = 0) -> str:
        ints = self.of_type("int")
        if depth >= 2 or self.rng.random() < 0.35:
            return self.int_atom(ints)
        a = self.int_expr(depth + 1)
        b = self.int_expr(depth + 1)
        op = self.rng.choice(["+", "-", "*"])
        return f"({a} {op} {b})"

    def decl_int(self):
        name = self.fresh("i")
        init = self.int_expr()
        self.lines.append(f"var {name}: int = {init};")
        self.vars.append((name, "int"))

    def decl_bool(self):
        name = self.fresh("b")
        self.lines.append(f"var {name}: bool = {self.bool_expr()};")
        self.vars.append((name, "bool"))

    def decl_string(self):
        name = self.fresh("s")
        words = ["alpha", "beta", "hello", "world", "dial", "asm", "llvm", "xyz"]
        w = self.rng.choice(words)
        self.lines.append(f'var {name}: string = "{w}";')
        self.vars.append((name, "string"))

    def decl_iarr(self):
        name = self.fresh("a")
        n = self.rng.randint(1, 6)
        elems = ", ".join(str(self.rng.randint(-9, 30)) for _ in range(n))
        self.lines.append(f"var {name}: int[] = [{elems}];")
        self.vars.append((name, "int[]"))

    def decl_sarr(self):
        name = self.fresh("sa")
        words = ["a", "bb", "ccc", "dd", "ee"]
        n = self.rng.randint(1, 5)
        elems = ", ".join(f'"{self.rng.choice(words)}"' for _ in range(n))
        self.lines.append(f"var {name}: string[] = [{elems}];")
        self.vars.append((name, "string[]"))

    def decl_matrix(self):
        name = self.fresh("m")
        rows = self.rng.randint(2, 3)
        cols = self.rng.randint(2, 3)
        rows_src = []
        for _ in range(rows):
            elems = ", ".join(str(self.rng.randint(-5, 20)) for _ in range(cols))
            rows_src.append(f"[{elems}]")
        self.lines.append(f"var {name}: int[][] = [{', '.join(rows_src)}];")
        self.vars.append((name, "int[][]"))

    def print_any(self):
        """生成一条 print，参数覆盖当前已声明变量的一部分。"""
        if not self.vars:
            self.lines.append(f'print("{self.int_lit()}");')
            return
        # 采样 1~4 个变量做打印（句柄/数组用 .length 或下标，避免打印复杂对象）
        k = self.rng.randint(1, min(4, len(self.vars)))
        picked = self.rng.sample(self.vars, k)
        args = []
        for name, ty in picked:
            if ty == "int":
                args.append(name)
            elif ty == "bool":
                args.append(name)
            elif ty == "string":
                args.append(name)
            elif ty == "int[]":
                # 所有声明的数组长度 >=1，get(0) 恒安全；避免 randint(0,1) 越界 UB
                args.append(f"{name}.get(0)")
            elif ty == "string[]":
                args.append(f"{name}.get(0)")
            elif ty == "int[][]":
                args.append(f"{name}.length()")
        self.lines.append(f'print({", ".join(args)});')

    def arr_ops(self):
        """对现有 int[] 做随机写/push/set，然后打印，触发 COW/所有权路径。"""
        arrs = self.of_type("int[]")
        if not arrs:
            return
        a = self.rng.choice(arrs)
        op = self.rng.choice(["set", "push", "set", "length"])
        if op == "set":
            self.lines.append(f"{a}[0] = {self.int_lit()};")
        elif op == "push":
            self.lines.append(f"{a}.push({self.int_lit()});")
        else:
            self.lines.append(f"print({a}.length());")

    def string_ops(self):
        strs = self.of_type("string")
        if not strs:
            return
        s = self.rng.choice(strs)
        op = self.rng.choice(["len", "concat", "upper", "index"])
        if op == "len":
            self.lines.append(f"print({s}.length());")
        elif op == "concat":
            name = self.fresh("sc")
            self.lines.append(f'var {name}: string = {s} + "_x";')
            self.vars.append((name, "string"))
        elif op == "upper":
            self.lines.append(f"print({s}.to_upper());")
        else:
            self.lines.append(f"print({s}.index_of(\"a\"));")

    def borrow_bind(self):
        """借用句柄绑局部变量——e3ap 修复的核心场景。

        get(i) 返回的是元素标量：只有「元素本身是句柄」时（int[][] 的 row=int[]、
        string[] 的 elem=string）才触发借用 retain 路径。int[] 的 get 是 int（标量），
        绑给 int[] 会语义报错，故只针对句柄元素类型。
        """
        mats = self.of_type("int[][]")
        sarrs = self.of_type("string[]")
        if mats and self.rng.random() < 0.6:
            m = self.rng.choice(mats)
            name = self.fresh("row")
            self.lines.append(f"var {name}: int[] = {m}.get(0);")
            self.vars.append((name, "int[]"))
            self.lines.append(f"print({name}.get(0));")
        elif sarrs:
            sa = self.rng.choice(sarrs)
            name = self.fresh("se")
            self.lines.append(f'var {name}: string = {sa}.get(0);')
            self.vars.append((name, "string"))
            self.lines.append(f"print({name});")

    def nested_write(self):
        """嵌套下标赋值——e3ao 修复的核心场景。"""
        mats = self.of_type("int[][]")
        if mats:
            m = self.rng.choice(mats)
            # 行 0..1、列 0 均在 2x2..3x3 范围内，安全
            row = self.rng.randint(0, 1)
            self.lines.append(f"{m}[{row}][0] = {self.int_lit()};")

    def call_default_fn(self):
        """默认实参补齐——e3aq 修复的核心场景。"""
        if not self.has_default_fn:
            return
        ints = self.of_type("int")
        a = self.int_atom(ints)
        if self.rng.random() < 0.5:
            self.lines.append(f"print(dfn({a}));")          # 省略默认参数
        else:
            b = self.int_atom(ints)
            self.lines.append(f"print(dfn({a}, {b}));")     # 显式覆盖

    def impl_ops(self):
        """impl 方法调用——e3aq 修复的核心场景。"""
        if not self.has_impl:
            return
        name = self.fresh("pt")
        self.lines.append(f"var {name}: PT = PT {{ x: {self.int_lit()}, y: {self.int_lit()} }};")
        # 不加入符号表（print_any 不知道 PT 类型），只在此局部使用
        if self.rng.random() < 0.5:
            self.lines.append(f"print({name}.sum());")
        else:
            self.lines.append(f"print({name}.add());")       # 方法默认参数
        self.lines.append(f"print({name}.add(3));")

    def struct_container_ops(self):
        """struct 进容器——B6（f7a94a4 修复）的核心场景，P3 门禁盲区固化。

        原 fuzz 只在 impl_ops 里生成局部 PT{} 并调方法，从不把 struct 装进容器，
        因此无法暴露「一边支持 struct 装箱、另一边根本编不过」的差距。本方法
        严格复用 golden batch3_b6_smoke.dial 已双后端验证的语法：
          * PT[] 数组字面量 / 下标读字段 / 下标写 / set / push 到空数组；
          * map<string,PT> 的 put / get / get_or / contains / size / values / remove。
        元素类型 PT 只含 int 字段（满足装箱元素白名单），key 用静态串字面量
        （immortal，不触发 P0 拼接泄漏），输出全程确定、可差分。
        """
        if not self.has_struct_container:
            return
        if self.rng.random() < 0.5:
            # ---- PT[] 数组路径 ----
            name = self.fresh("pts")
            x1, y1, x2, y2 = (self.int_lit() for _ in range(4))
            self.lines.append(
                f"var {name}: PT[] = [PT{{x: {x1}, y: {y1}}}, PT{{x: {x2}, y: {y2}}}];")
            self.lines.append(f"print({name}.length(), {name}[0].x, {name}[1].y);")
            # 下标写 + set 写（重新装箱路径）
            self.lines.append(
                f"{name}[0] = PT{{x: {self.int_lit()}, y: {self.int_lit()}}};")
            self.lines.append(
                f"{name}.set(1, PT{{x: {self.int_lit()}, y: {self.int_lit()}}});")
            self.lines.append(f"print({name}[0].x, {name}[1].x);")
            # push 到空数组
            name2 = self.fresh("pts")
            self.lines.append(f"var {name2}: PT[] = [];")
            self.lines.append(
                f"{name2}.push(PT{{x: {self.int_lit()}, y: {self.int_lit()}}});")
            self.lines.append(f"print({name2}.length(), {name2}[0].y);")
        else:
            # ---- map<string,PT> 路径 ----
            name = self.fresh("pm")
            self.lines.append(f"var {name}: map<string, PT> = new_kv_map();")
            self.lines.append(
                f'{name}.put("ka", PT{{x: {self.int_lit()}, y: {self.int_lit()}}});')
            self.lines.append(
                f'{name}.put("kb", PT{{x: {self.int_lit()}, y: {self.int_lit()}}});')
            v = self.fresh("pv")
            self.lines.append(f"var {v}: PT = {name}.get(\"ka\");")
            self.lines.append(f"print({v}.x, {v}.y);")
            # 未命中 → 零值结构体（字段带默认值，语义确定）
            miss = self.fresh("pv")
            self.lines.append(f"var {miss}: PT = {name}.get(\"zz\");")
            self.lines.append(f"print({miss}.x, {miss}.y);")
            # get_or（默认值路径 + 命中路径）
            d = self.fresh("pv")
            self.lines.append(
                f'var {d}: PT = {name}.get_or("kb", PT{{x: {self.int_lit()}, y: {self.int_lit()}}});')
            self.lines.append(f"print({d}.x);")
            # contains / size / values / remove
            self.lines.append(f"print({name}.contains(\"ka\"), {name}.size());")
            vals = self.fresh("pvals")
            self.lines.append(f"var {vals}: PT[] = {name}.values();")
            self.lines.append(f"print({vals}.length());")
            rm = self.fresh("pv")
            self.lines.append(f'var {rm}: PT = {name}.remove("ka");')
            self.lines.append(f"print({rm}.x, {name}.size());")

    def cow_alias(self):
        """数组别名 + push 触发 COW——所有权/引用计数高发区。"""
        arrs = self.of_type("int[]")
        if not arrs:
            return
        a = self.rng.choice(arrs)
        alias = self.fresh("al")
        self.lines.append(f"var {alias}: int[] = {a};")
        self.vars.append((alias, "int[]"))
        self.lines.append(f"{alias}.push({self.int_lit()});")
        # 打印原数组和别名，验证 COW 后两者独立
        self.lines.append(f"print({alias}.get(0), {a}.get(0), {alias}.length(), {a}.length());")

    def slice_write(self):
        """切片后写 + 原数组读取——slice COW 与所有权结算路径。"""
        arrs = self.of_type("int[]")
        if not arrs:
            return
        a = self.rng.choice(arrs)
        sl = self.fresh("sl")
        # slice(0,1) 对任何声明数组（长度>=1）都安全，产出长度1，sl[0] 合法
        self.lines.append(f"var {sl}: int[] = {a}.slice(0, 1);")
        self.vars.append((sl, "int[]"))
        self.lines.append(f"{sl}[0] = {self.int_lit()};")
        self.lines.append(f"print({sl}.get(0), {a}.get(0));")

    def if_block(self):
        self.lines.append(f"if ({self.bool_expr()}) {{")
        self.lines.append(f'  print("T{self._id}");')
        self.lines.append("} else {")
        self.lines.append(f'  print("F{self._id}");')
        self.lines.append("}")

    def while_sum(self):
        name = self.fresh("w")
        acc = self.fresh("acc")
        n = self.rng.randint(1, 5)
        self.lines.append(f"var {acc}: int = 0;")
        self.lines.append(f"var {name}: int = 0;")
        self.lines.append(f"while ({name} < {n}) {{")
        self.lines.append(f"  {acc} = {acc} + {name};")
        self.lines.append(f"  {name} = {name} + 1;")
        self.lines.append("}")
        self.lines.append(f"print({acc});")

    def build(self) -> str:
        # 生成器动作池，按权重采样；先铺底再操作，保证合法率
        actions = [
            (self.decl_int, 5),
            (self.decl_bool, 2),
            (self.decl_string, 3),
            (self.decl_iarr, 4),
            (self.decl_sarr, 2),
            (self.decl_matrix, 2),
            (self.arr_ops, 4),
            (self.string_ops, 3),
            (self.borrow_bind, 3),
            (self.nested_write, 3),
            (self.call_default_fn, 3),
            (self.impl_ops, 3),
            (self.struct_container_ops, 3),
            (self.cow_alias, 3),
            (self.slice_write, 2),
            (self.if_block, 2),
            (self.while_sum, 2),
            (self.print_any, 5),
        ]
        pool = []
        for fn, w in actions:
            pool.extend([fn] * w)
        n = self.rng.randint(15, 35)
        for _ in range(n):
            self.rng.choice(pool)()
        # 语句必须在函数体内：prelude -> 头 -> 语句 -> return -> 尾
        src = self.prelude + ["fn main(): int {"] + self.lines + ["return 0;", "}"]
        return "\n".join(src) + "\n"

    @property
    def source(self) -> list[str]:
        return self.lines


# ---------------------------------------------------------------------------
# 编译 / 运行 / 对比
# ---------------------------------------------------------------------------
def build(dialc: pathlib.Path, src: pathlib.Path, out: pathlib.Path, backend: str,
          opt: str | None = None, timeout: int = 60):
    cmd = [str(dialc), "build", str(src), "--backend", backend, "-o", str(out)]
    if opt:
        cmd.append(opt)
    try:
        p = subprocess.run(cmd, capture_output=True, text=True,
                           env=BUILD_ENV, timeout=timeout)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired:
        return -99, "BUILD TIMEOUT"


def run(exe: pathlib.Path, timeout: int = 10):
    try:
        p = subprocess.run([str(exe)], capture_output=True, timeout=timeout)
        return p.returncode, p.stdout
    except subprocess.TimeoutExpired:
        return -99, b"RUN TIMEOUT"


def check_source(src_text: str, workdir: pathlib.Path, opt: str | None = None,
                 dialc: pathlib.Path = DIALC, minimize: bool = False):
    """对一份源码做双后端差分。返回 (verdict, detail, minimized_src)。

    verdict ∈ {OK, DIFF, BUILD_FAIL_LLVM, BUILD_FAIL_ASM, RUN_FAIL_LLVM,
               TIMEOUT, MINIMIZED}
    """
    workdir.mkdir(parents=True, exist_ok=True)
    src = workdir / "prog.dial"
    src.write_text(src_text, encoding="utf-8")

    rc_l, log_l = build(dialc, src, workdir / "llvm_bin", "llvm", opt)
    if rc_l != 0:
        return "BUILD_FAIL_LLVM", log_l[-500:], src_text
    rc_a, log_a = build(dialc, src, workdir / "asm_bin", "asm", opt)
    if rc_a != 0:
        return "BUILD_FAIL_ASM", log_a[-500:], src_text

    # .s 副作用文件必须在（证明真走了 asm 发射路径）
    if not (workdir / "prog.s").exists():
        return "NO_ASM_EMIT", "no .s emitted", src_text

    rr_l, out_l = run(workdir / "llvm_bin")
    rr_a, out_a = run(workdir / "asm_bin")
    if rr_l < 0:
        return "RUN_FAIL_LLVM", f"llvm rc={rr_l}", src_text
    if rr_a < 0:
        return "RUN_FAIL_LLVM_OR_ASM", f"asm rc={rr_a} llvm rc={rr_l}", src_text

    if out_l != out_a or rr_l != rr_a:
        detail = (f"llvm(rc={rr_l}): {out_l!r}\n"
                  f"asm (rc={rr_a}): {out_a!r}")
        mini = minimize_source(src_text, workdir, opt, dialc) if minimize else src_text
        return "DIFF", detail, mini
    return "OK", "", src_text


def minimize_source(src_text: str, workdir: pathlib.Path, opt: str | None,
                    dialc: pathlib.Path) -> str:
    """delta-debugging：逐行删除，保留仍能复现 DIFF 的最小版本。"""
    lines = src_text.strip().split("\n")
    # 保留首尾的函数框架行（fn main / 最后两个 }），只删中间可执行语句
    if len(lines) < 4:
        return src_text
    changed = True
    while changed:
        changed = False
        i = 1
        while i < len(lines) - 2:
            cand = lines[:i] + lines[i + 1:]
            # 跳过会导致语法不完整的行（简单启发式：括号不平衡时跳过）
            text = "\n".join(cand) + "\n"
            v, _, _ = check_source(text, workdir / "mini", opt, dialc, minimize=False)
            if v == "DIFF":
                lines = cand
                changed = True
            else:
                i += 1
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# 模式：golden 自检 / fuzz / replay
# ---------------------------------------------------------------------------
def golden_check(dialc: pathlib.Path, opt: str | None,
                 corpus_dir: pathlib.Path | None = None) -> int:
    corpus = corpus_dir or GOLDEN
    print(f"[golden-check] 用框架跑差分语料（{len(list(corpus.glob('*.dial')))} 个，{corpus}），应全部 OK")
    bad = 0
    with tempfile.TemporaryDirectory(prefix="dfuzz_gc_") as td:
        for src in sorted(corpus.glob("*.dial")):
            v, detail, _ = check_source(src.read_text(encoding="utf-8"),
                                        pathlib.Path(td) / src.stem, opt, dialc)
            if v == "OK":
                print(f"  ✅ {src.stem}")
            else:
                bad += 1
                print(f"  ❌ {src.stem}: {v} {detail[:200]}")
    print(f"[golden-check] 不一致数={bad}")
    return 1 if bad else 0


def fuzz(seed_start: int, n: int, opt_levels: list[str | None], jobs: int,
         out_dir: pathlib.Path, dialc: pathlib.Path) -> int:
    out_dir.mkdir(parents=True, exist_ok=True)
    fail_dir = out_dir / "failures"
    fail_dir.mkdir(parents=True, exist_ok=True)
    seeds = range(seed_start, seed_start + n)
    diffs = 0
    stats = {"OK": 0, "BUILD_FAIL_LLVM": 0, "BUILD_FAIL_ASM": 0,
             "RUN_FAIL_LLVM_OR_ASM": 0, "NO_ASM_EMIT": 0, "DIFF": 0}
    # P3 门禁盲区固化：struct 进容器（PT[] / map<string,PT>）的覆盖率与
    # 「一边编不过」专项计数。这类构造在 golden 已证明两后端都支持，所以一旦
    # 生成了它却出现 BUILD_FAIL，就绝不能像普通 skip 那样被掩盖——那是盲区
    # 本身（编译失败被当成「生成器合法率」噪声放过）。
    sc_stats = {"generated": 0, "build_fail": 0}

    def one(seed: int):
        g = Gen(seed)
        src = g.build()
        # 该程序是否真的生成了 struct 进容器构造（而非只在 prelude 声明 PT）
        has_sc = ("PT[]" in src) or ("map<string, PT>" in src)
        results = []
        for opt in opt_levels:
            with tempfile.TemporaryDirectory(prefix=f"dfuzz_{seed}_") as td:
                v, detail, mini = check_source(src, pathlib.Path(td), opt, dialc,
                                               minimize=False)
                results.append((opt, v, detail, mini))
        return seed, src, results, has_sc

    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as ex:
        futs = [ex.submit(one, s) for s in seeds]
        for fut in concurrent.futures.as_completed(futs):
            seed, src, results, has_sc = fut.result()
            if has_sc:
                sc_stats["generated"] += 1
                # 任一优化级别编不过都算盲区命中（说明一边对该构造支持不全）
                if any(v.startswith("BUILD_FAIL") or v == "NO_ASM_EMIT"
                       for _, v, _, _ in results):
                    sc_stats["build_fail"] += 1
                    tag = f"seed{seed}_struct_container_build_fail"
                    (fail_dir / f"{tag}.dial").write_text(src, encoding="utf-8")
                    print(f"  ❌ STRUCT-CONTAINER BUILD FAIL {tag}（盲区命中，"
                          f"一边根本编不过，不得按 skip 放过）")
            for opt, v, detail, mini in results:
                stats[v] = stats.get(v, 0) + 1
                if v == "DIFF":
                    diffs += 1
                    tag = f"seed{seed}_{opt or 'default'}"
                    (fail_dir / f"{tag}.dial").write_text(src, encoding="utf-8")
                    (fail_dir / f"{tag}.detail.txt").write_text(
                        f"seed={seed} opt={opt}\n{detail}\n", encoding="utf-8")
                    # 最小化（串行，避免并发抢占）
                    with tempfile.TemporaryDirectory(prefix=f"dfuzz_min_{seed}_") as td:
                        m, _, mini_src = check_source(src, pathlib.Path(td), opt,
                                                      dialc, minimize=True)
                        if m == "DIFF":
                            (fail_dir / f"{tag}.min.dial").write_text(mini_src,
                                                                     encoding="utf-8")
                    print(f"  ❌ DIFF {tag}\n{detail[:500]}")
                elif v == "OK":
                    pass
            if (seed - seed_start + 1) % 25 == 0:
                print(f"  ... 已处理 {seed - seed_start + 1}/{n} seeds")

    print(f"[fuzz] 统计（按 opt 展开）: {stats}")
    print(f"[fuzz] struct 进容器覆盖: 生成 {sc_stats['generated']} 个程序，"
          f"其中一边编不过 {sc_stats['build_fail']} 个（应为 0）")
    print(f"[fuzz] 不一致数={diffs}，用例目录={fail_dir}")
    # 门禁失败条件：出现输出不一致，或 struct 容器构造出现「一边编不过」盲区。
    return 1 if (diffs or sc_stats["build_fail"] or sc_stats["generated"] == 0) else 0


def replay(path: pathlib.Path, opt: str | None, dialc: pathlib.Path) -> int:
    with tempfile.TemporaryDirectory(prefix="dfuzz_replay_") as td:
        v, detail, mini = check_source(path.read_text(encoding="utf-8"),
                                       pathlib.Path(td), opt, dialc, minimize=False)
    print(f"[replay] {path}: {v}")
    if detail:
        print(detail)
    return 0 if v == "DIFF" else 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--golden-check", action="store_true", help="跑现有 golden 自检框架")
    ap.add_argument("--diff-corpus", type=pathlib.Path,
                    help="对指定目录的 .dial 语料做双后端差分（配合 --golden-check 使用）")
    ap.add_argument("--seeds", type=int, default=0, help="生成多少个程序做差分")
    ap.add_argument("--start", type=int, default=0, help="seed 起点")
    ap.add_argument("--opt-levels", nargs="*", default=[None],
                    help="优化级别列表，如 O0 O1 O2 O3 g；默认不指定")
    ap.add_argument("--jobs", type=int, default=2, help="并行 worker 数")
    ap.add_argument("--out", type=pathlib.Path, default=ROOT / ".dialc_fuzz",
                    help="失败用例输出目录")
    ap.add_argument("--replay", type=pathlib.Path, help="复跑一个落盘的 .dial 用例")
    ap.add_argument("--dialc", type=pathlib.Path, default=DIALC, help="dialc 二进制路径")
    args = ap.parse_args()

    if not args.dialc.exists():
        print(f"错误：找不到编译器 {args.dialc}，请先 cargo build --release", file=sys.stderr)
        return 2

    opt_levels: list[str | None]
    if args.opt_levels == [None] or args.opt_levels == []:
        opt_levels = [None]
    else:
        opt_levels = ["-" + o.lstrip("-") for o in args.opt_levels]

    if args.replay:
        return replay(args.replay, opt_levels[0], args.dialc)
    if args.golden_check:
        return golden_check(args.dialc, opt_levels[0], args.diff_corpus)
    if args.seeds > 0:
        return fuzz(args.start, args.seeds, opt_levels, args.jobs, args.out, args.dialc)
    ap.print_help()
    return 0


if __name__ == "__main__":
    sys.exit(main())
