#!/usr/bin/env python3
"""dialc 性能基准采集器 —— 4 语言（C / Go / Dial / Python）× 28 项微基准。

产出一个自描述的 JSON（默认 bench/data.json），供 tools/build_bench_web.py
生成独立性能网页 docx/bench.html。

测量口径
--------
* **绝对值**：每个 (语言, 任务) 的单次纯计算耗时（毫秒）。用「单次进程内
  重复 N 次」的**校准法**摊薄进程启动：先自校准出使总时长落在目标区间的
  重复数 N，跑若干次取最小总时长，再扣除一次 noop 启动基线并除以 N。
  这样 C（单次 ~微秒）与 Python（单次 ~百毫秒）都能稳定测量，且不含
  各语言内部计时 API 的差异（外部统一墙钟计时）。
* **绝对比值**：以 **C = 1.00×** 为基准，value = 该语言耗时 / C 耗时。
  大于 1 表示比 C 慢相应倍数。C 恒为 1.00×。
* **正确性锚点**：28 项在 4 语言必须算出**完全相同**的十进制校验和；
  采集器内嵌真值表，任一不符立即失败。这保证「比值」比较的是等价计算。
* **六大工程维度**（雷达图 6 轴，均为「越小越好」）：
  速度 / 峰值内存 / 编译耗时 / 启动耗时 / 产物体积 / 源码行数。

复现
----
    python3 tools/bench_harness.py --dialc target/release/dialc

依赖：gcc、go、python3、已编译的 dialc（含其 LLVM/C 运行时的编译链）。
仅支持 POSIX（os.fork/wait4 用于精确采集单子进程峰值 RSS）。
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import pathlib
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"

# ---------------------------------------------------------------------------
# 28 项元数据：唯一真相源，网页生成器直接消费。
# ---------------------------------------------------------------------------
TASKS = [
    ("b01", "整数累加依赖链", "算术循环", "100 万次 (s+i) mod M，串行依赖链"),
    ("b02", "乘加取模", "算术循环", "10 万次 (s*31+i) mod M"),
    ("b03", "除法与取模混合", "算术循环", "10 万次整除 + 取模累加"),
    ("b04", "FMA 乘加链", "算术循环", "100 万次 (s*3+7) mod M"),
    ("b05", "连乘幂内核", "算术循环", "2 万组 × 8 次模乘"),
    ("b06", "批量欧几里得 gcd", "算术循环", "3 万组 gcd 累加"),
    ("b07", "迭代斐波那契", "算术循环", "100 万次迭代 fib mod M"),
    ("b08", "3n+1 总步数", "算术循环", "1..20000 的 Collatz 步数和"),
    ("b09", "二叉递归斐波那契", "递归调用", "fib(23) 递归树 × 3"),
    ("b10", "浅递归重复调用", "递归调用", "深度 25 递归求和 × 5 万"),
    ("b11", "二叉递归树", "递归调用", "tree(18) 二叉递归 × 200"),
    ("b12", "互递归奇偶判定", "递归调用", "even/odd 互递归 × 2 万"),
    ("b13", "数组构建与顺序求和", "数组内存", "2048 元素 push + 顺序归约"),
    ("b14", "伪随机索引读写", "数组内存", "100 万次 LCG 索引读写"),
    ("b15", "冒泡排序", "数组内存", "512 元素 O(n²) 排序"),
    ("b16", "原地反转与归约", "数组内存", "2048 元素反转 × 100 轮"),
    ("b17", "矩阵乘法", "数组内存", "64×64 三重循环乘加"),
    ("b18", "前缀和与归约", "数组内存", "2048 元素前缀和 × 200 轮"),
    ("b19", "字符串逐字符扫描", "字符串", "36 字符串 × 2 万轮码点累加"),
    ("b20", "字符串循环拼接", "字符串", "4000 次 + 拼接"),
    ("b21", "子串查找 index_of", "字符串", "4 个子串 × 2 万轮查找"),
    ("b22", "拼接与定长子串", "字符串", "substring + get × 1 万轮"),
    ("b23", "位计数 popcount", "位与控制流", "10 万个数逐位计数"),
    ("b24", "xorshift32 累加", "位与控制流", "100 万次 xorshift 与异或"),
    ("b25", "三路分支", "位与控制流", "100 万次 if/elif/else"),
    ("b26", "嵌套循环异或", "位与控制流", "1000×1000 异或累加"),
    ("b27", "短路布尔求值", "位与控制流", "100 万次 and/or 短路"),
    ("b28", "综合混合负载", "位与控制流", "数组读写 + 算术 + 位运算"),
]

# 4 语言在 28 项上共同算出的校验和（已逐项验证一致）。任一语言产出不符即失败。
CHECKSUMS = {
    "b01": 999496507, "b02": 139598823, "b03": 235528676, "b04": 727273949,
    "b05": 53802088, "b06": 74999, "b07": 918091266, "b08": 1834604,
    "b09": 85971, "b10": 16250000, "b11": 1672200, "b12": 10000,
    "b13": 64994304, "b14": 643017769, "b15": 163968, "b16": 725990386,
    "b17": 405292314, "b18": 402252332, "b19": 67440000, "b20": 124272,
    "b21": 305000, "b22": 10875000, "b23": 815024, "b24": 292287901,
    "b25": 998659674, "b26": 511213536, "b27": 1452381, "b28": 39361297,
}

LANGS = ["C", "Go", "Dial", "Python"]
COLORS = {"C": "#7aa2f7", "Go": "#7dcfff", "Dial": "#f7768e", "Python": "#e0af68"}

# 校准参数：让单次进程运行时间落在 [TARGET_MS*0.6, TARGET_MS*1.5]。
TARGET_MS = 120.0
CALIB_MIN_REP = 1
CALIB_MAX_REP = 2_000_000
SAMPLES = 5          # 每个 (语言,任务) 取 5 次中最小的总时长
STARTUP_SAMPLES = 15 # noop 启动基线采样次数
# 计时分辨率下限（毫秒）。极快项（或被编译器常量折叠成空操作的项，如 C 下的
# b20 字符串拼接）扣启动基线后可能≈0 甚至为负。统一钳到 5µs，避免绝对值显示
# 0.000 与「绝对比值」除零级虚高；这是墙钟计时 + 进程噪声的合理下限。
FLOOR_MS = 0.005


def log(msg: str) -> None:
    print(f"[bench] {msg}", flush=True)


def run_capture(argv: list[str], env: dict | None = None, cwd: str | None = None):
    """返回 (墙钟秒, stdout 文本, returncode)。"""
    t0 = time.perf_counter()
    p = subprocess.run(argv, capture_output=True, text=True,
                       env=env, cwd=cwd, timeout=600)
    dt = time.perf_counter() - t0
    return dt, p.stdout, p.returncode


def run_with_rusage(argv: list[str], env: dict | None = None, out_file=None):
    """用 GNU time 的 `%M` 采集目标进程的**真实峰值 RSS(KB)** 与墙钟秒。

    不用 os.fork+wait4.ru_maxrss：子进程在 execv 之前会继承采集器自身约
    11MB 的驻留集，ru_maxrss 记录的是这个「历史最大」，导致 4 语言读数几乎
    相同（全是采集器进程的大小），完全失真。GNU time 自己是极小的独立进程，
    它 fork 出的目标进程峰值 RSS 才是被测程序的真实占用。
    若系统没有 /usr/bin/time，则回退到 fork+wait4（读数含继承基线，会标注）。
    """
    time_bin = "/usr/bin/time"
    if pathlib.Path(time_bin).exists() and out_file is not None:
        t0 = time.perf_counter()
        with open(out_file, "w") as tf:
            p = subprocess.run([time_bin, "-f", "%M", "-o", str(out_file)] + argv,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               env=env, timeout=600)
        dt = time.perf_counter() - t0
        try:
            rss = int(pathlib.Path(out_file).read_text().strip().split("\n")[-1])
        except Exception:
            rss = 0
        return dt, rss
    # 回退：wait4（读数含 fork 继承基线，仅作兜底）
    t0 = time.perf_counter()
    pid = os.fork()
    if pid == 0:
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, 1)
        os.dup2(devnull, 2)
        try:
            os.execvpe(argv[0], argv, env or os.environ)
        except Exception:
            os._exit(127)
    _, _status, ru = os.wait4(pid, 0)
    return time.perf_counter() - t0, ru.ru_maxrss


class Lang:
    def __init__(self, name: str, argv_fn, compile_argv, artifact, source,
                 compile_ms=0.0, flags=""):
        self.name = name
        self.argv_fn = argv_fn          # (task, rep) -> argv
        self.compile_argv = compile_argv
        self.artifact = artifact        # 交付产物路径（测体积）
        self.source = source            # 源码路径（测 LOC）
        self.compile_ms = compile_ms
        self.flags = flags
        self.env = None
        self.startup_ms = 0.0
        self.memory_kb = 0
        self.size_bytes = 0
        self.loc = 0


def count_loc(path: pathlib.Path) -> int:
    """有效代码行：去掉空行与整行注释（// 或 # 或 /* 起头）。"""
    n = 0
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s:
            continue
        if s.startswith("//") or s.startswith("#") or s.startswith("/*") or s.startswith("*"):
            continue
        n += 1
    return n


def effective_source_size(path: pathlib.Path) -> int:
    return path.stat().st_size


def build_all(dialc: pathlib.Path, tmp: pathlib.Path) -> dict[str, Lang]:
    """编译 4 语言，记录编译耗时（各测 2 次取最小，Dial LTO 较慢故取 1 次）。"""
    env = dict(os.environ)
    env["GO111MODULE"] = "off"
    env["GOCACHE"] = str(tmp / "gocache")
    env.setdefault("LLVM_SYS_181_PREFIX", "/usr/lib/llvm-18")
    os.makedirs(env["GOCACHE"], exist_ok=True)

    out_c = tmp / "bench_c"
    out_go = tmp / "bench_go"
    out_dial = tmp / "bench_dial"
    py = sys.executable

    specs = {
        "C": dict(
            compile_argv=["gcc", "-O2", "-o", str(out_c), str(BENCH / "bench.c")],
            argv_fn=lambda t, r: [str(out_c), t, str(r)],
            artifact=out_c, source=BENCH / "bench.c",
            flags="gcc -O2"),
        "Go": dict(
            compile_argv=["go", "build", "-o", str(out_go), "bench.go"],
            argv_fn=lambda t, r: [str(out_go), t, str(r)],
            artifact=out_go, source=BENCH / "bench.go",
            flags="go build（默认优化）"),
        "Dial": dict(
            compile_argv=[str(dialc), "build", str(BENCH / "bench.dial"),
                          "--fast", "-o", str(out_dial)],
            argv_fn=lambda t, r: [str(out_dial), t, str(r)],
            artifact=out_dial, source=BENCH / "bench.dial",
            flags="dialc build --fast（O3 + LTO + 向量化）"),
        "Python": dict(
            compile_argv=[py, "-m", "py_compile", str(BENCH / "bench.py")],
            argv_fn=lambda t, r: [py, str(BENCH / "bench.py"), t, str(r)],
            artifact=BENCH / "bench.py", source=BENCH / "bench.py",
            flags="CPython 解释执行（py_compile 记为编译耗时）"),
    }

    langs: dict[str, Lang] = {}
    for name, sp in specs.items():
        log(f"编译 {name}: {' '.join(sp['compile_argv'][:3])} ...")
        kw = dict(env=env)
        if name == "Go":
            kw["cwd"] = str(BENCH)
        reps = 1 if name == "Dial" else 2  # Dial LTO 编译较慢，测 1 次
        times = []
        for _ in range(reps):
            dt, _out, rc = run_capture(sp["compile_argv"], **kw)
            if rc != 0:
                raise RuntimeError(f"{name} 编译失败 (rc={rc}): {sp['compile_argv']}")
            times.append(dt * 1000.0)
        c_ms = min(times)

        langs[name] = Lang(
            name, sp["argv_fn"], sp["compile_argv"], sp["artifact"], sp["source"],
            compile_ms=round(c_ms, 3), flags=sp["flags"])
        langs[name].env = env
        log(f"  {name}: 编译 {c_ms:.1f} ms")

    # 体积：编译型取可执行文件，解释型取源码字节（口径在网页注明）
    for name, lang in langs.items():
        lang.size_bytes = effective_source_size(lang.artifact)
        lang.loc = count_loc(lang.source)
    return langs


def verify_checksums(langs: dict[str, Lang]) -> None:
    """4 语言 × 28 项，rep=1，逐一比对内嵌真值表。"""
    log("校验 28 项 × 4 语言校验和一致性 ...")
    bad = []
    for tid, *_ in TASKS:
        for name in LANGS:
            lang = langs[name]
            _dt, out, rc = run_capture(lang.argv_fn(tid, 1), env=lang.env)
            got = out.strip().split("\n")[-1].strip()
            if rc != 0 or got != str(CHECKSUMS[tid]):
                bad.append(f"{tid}/{name}: 期望 {CHECKSUMS[tid]} 实得 {got!r} (rc={rc})")
    if bad:
        raise RuntimeError("校验和不一致:\n  " + "\n  ".join(bad))
    log("  ✅ 112/112 校验和全部一致")


def measure_startup(langs: dict[str, Lang]) -> None:
    """noop 任务的进程墙钟（含解释器/运行时初始化），采中位数。"""
    log("采集启动耗时（noop × %d）..." % STARTUP_SAMPLES)
    for name in LANGS:
        lang = langs[name]
        vals = []
        for _ in range(STARTUP_SAMPLES):
            dt, _out, _rc = run_capture(lang.argv_fn("noop", 1), env=lang.env)
            vals.append(dt * 1000.0)
        lang.startup_ms = round(statistics.median(vals), 4)
        log(f"  {name}: 启动 {lang.startup_ms:.3f} ms")


def measure_memory(langs: dict[str, Lang], tmp: pathlib.Path) -> None:
    """代表性负载 b17（矩阵乘，分配 3 个 4096 槽数组）的峰值 RSS。
    含语言运行时基线——这正是「内存」工程维度要体现的差异。"""
    log("采集峰值内存（b17 代表性负载，含运行时）...")
    for name in LANGS:
        lang = langs[name]
        rss_file = tmp / f"rss_{name}.txt"
        _dt, rss = run_with_rusage(lang.argv_fn("b17", 1), env=lang.env,
                                   out_file=rss_file)
        lang.memory_kb = rss
        log(f"  {name}: 峰值 RSS {rss} KB")


def calibrate(lang: Lang, tid: str) -> int:
    """找到使「单次进程运行」接近 TARGET_MS 的重复数。"""
    rep = 1
    dt, _out, _rc = run_capture(lang.argv_fn(tid, rep), env=lang.env)
    ms = dt * 1000.0
    # 以实测单次耗时估算目标重复数；迭代收敛，避免冷启动/缓存偏差。
    for _ in range(12):
        if ms <= 0:
            ms = 0.001
        want = max(CALIB_MIN_REP, int(TARGET_MS / ms * rep))
        want = min(want, CALIB_MAX_REP)
        if want == rep:
            break
        rep = want
        dt, _out, _rc = run_capture(lang.argv_fn(tid, rep), env=lang.env)
        ms = dt * 1000.0
    return max(CALIB_MIN_REP, min(rep, CALIB_MAX_REP))


def measure_tasks(langs: dict[str, Lang], quick: bool):
    """逐项测量绝对耗时。返回 {tid: {lang: unit_ms}}。"""
    abs_ms: dict[str, dict[str, float]] = {tid: {} for tid, *_ in TASKS}
    reps_used: dict[str, dict[str, int]] = {tid: {} for tid, *_ in TASKS}
    samples = 2 if quick else SAMPLES
    for tid, tname, *_ in TASKS:
        for name in LANGS:
            lang = langs[name]
            rep = 1 if quick else calibrate(lang, tid)
            best = None
            for _ in range(samples):
                dt, out, rc = run_capture(lang.argv_fn(tid, rep), env=lang.env)
                # 顺带再校验一次：重复运行不改校验和（纯函数式任务）
                got = out.strip().split("\n")[-1].strip()
                if got != str(CHECKSUMS[tid]):
                    raise RuntimeError(f"{tid}/{name} rep={rep} 校验和漂移: {got!r}")
                total = dt * 1000.0
                if best is None or total < best:
                    best = total
            unit = (best - lang.startup_ms) / rep
            # 钳到计时分辨率下限：极快项 / 被编译器折叠成空操作的项（如 C 的
            # b20 字符串拼接）扣基线后≈0，否则绝对值显示 0.000 且比值虚高。
            unit = max(unit, FLOOR_MS)
            abs_ms[tid][name] = round(unit, 6)
            reps_used[tid][name] = rep
        log(f"  {tid} {tname}: " +
            "  ".join(f"{n}={abs_ms[tid][n]:.3f}ms" for n in LANGS))
    return abs_ms, reps_used


def tool_versions(langs: dict[str, Lang], dialc: pathlib.Path) -> dict:
    def v(argv, first_line=True):
        try:
            out = subprocess.check_output(argv, stderr=subprocess.STDOUT,
                                          text=True, timeout=30)
            return out.strip().split("\n")[0] if first_line else out.strip()
        except Exception as e:
            return f"(未知: {e})"
    return {
        "os": f"{platform.system()} {platform.release()} ({platform.machine()})",
        "cpu": _cpu_model(),
        "python": v([sys.executable, "--version"]),
        "gcc": v(["gcc", "--version"]),
        "go": v(["go", "version"]),
        "dialc": v([str(dialc), "--version"]),
    }


def _cpu_model() -> str:
    try:
        for line in pathlib.Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except Exception:
        pass
    return platform.processor() or "(未知)"


def main() -> int:
    ap = argparse.ArgumentParser(description="dialc 4 语言 × 28 项性能基准采集器")
    ap.add_argument("--dialc", default=str(ROOT / "target" / "release" / "dialc"),
                    help="已编译的 dialc 二进制路径")
    ap.add_argument("--out", default=str(BENCH / "data.json"), help="输出 JSON 路径")
    ap.add_argument("--quick", action="store_true",
                    help="快速模式（rep=1、少采样），仅用于本地冒烟，不产出正式数据")
    args = ap.parse_args()

    dialc = pathlib.Path(args.dialc)
    if not dialc.exists():
        log(f"❌ 找不到 dialc: {dialc}")
        return 1
    for tool in ("gcc", "go"):
        if shutil.which(tool) is None:
            log(f"❌ 缺少工具链: {tool}")
            return 1

    with tempfile.TemporaryDirectory(prefix="dialc-bench-") as td:
        tmp = pathlib.Path(td)
        langs = build_all(dialc, tmp)
        verify_checksums(langs)
        measure_startup(langs)
        measure_memory(langs, tmp)
        log("开始 28 项 × 4 语言绝对耗时采集（校准法）...")
        abs_ms, reps_used = measure_tasks(langs, args.quick)

    # 组装 benchmarks：绝对值 + 绝对比值（C=1.00）
    benchmarks = []
    for tid, tname, cat, desc in TASKS:
        c = abs_ms[tid]["C"]
        ratio = {n: round(abs_ms[tid][n] / c, 4) if c > 0 else 0.0 for n in LANGS}
        benchmarks.append({
            "id": tid, "name": tname, "category": cat, "description": desc,
            "checksum": CHECKSUMS[tid],
            "abs_ms": abs_ms[tid],
            "ratio": ratio,
            "reps": reps_used[tid],
        })

    # 六大工程维度（绝对值，"越小越好"）
    speed_sum = {n: round(sum(abs_ms[t][n] for t, *_ in TASKS), 4) for n in LANGS}
    dims = {
        "speed":   {"label": "速度", "unit": "ms（28 项单次耗时之和）",
                    "abs": speed_sum,
                    "note": "越小越快；28 项纯计算耗时之和"},
        "memory":  {"label": "峰值内存", "unit": "KB",
                    "abs": {n: langs[n].memory_kb for n in LANGS},
                    "note": "b17 负载的峰值 RSS，含语言运行时基线"},
        "compile": {"label": "编译耗时", "unit": "ms",
                    "abs": {n: langs[n].compile_ms for n in LANGS},
                    "note": "Python 为 py_compile 生成字节码耗时"},
        "startup": {"label": "启动耗时", "unit": "ms",
                    "abs": {n: langs[n].startup_ms for n in LANGS},
                    "note": "noop 进程墙钟（含解释器/运行时初始化）"},
        "binary":  {"label": "产物体积", "unit": "KB",
                    "abs": {n: round(langs[n].size_bytes / 1024.0, 2) for n in LANGS},
                    "note": "编译型为可执行文件字节，Python 为源码字节（解释型无二进制）"},
        "loc":     {"label": "源码行数", "unit": "行",
                    "abs": {n: langs[n].loc for n in LANGS},
                    "note": "实现 28 项的有效代码行（去空行与整行注释）"},
    }

    # 雷达图得分：每个维度取最优（最小）语言为 100，其余 = 最优/自身 ×100。
    # 于是「越靠外越好」，4 语言多边形可直接叠加对比。
    axis_order = ["speed", "memory", "compile", "startup", "binary", "loc"]
    radar_score = {n: [] for n in LANGS}
    for k in axis_order:
        vals = {n: max(dims[k]["abs"][n], 1e-6) for n in LANGS}
        best = min(vals.values())
        for n in LANGS:
            radar_score[n].append(round(best / vals[n] * 100.0, 2))

    data = {
        "meta": {
            "generated": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
            "title": "Dial 性能基准：C / Python / Go / Dial 对比",
            "task_count": len(TASKS),
            "langs": LANGS,
            "colors": COLORS,
            "host": tool_versions(langs, dialc),
            "flags": {n: langs[n].flags for n in LANGS},
            "target_ms": TARGET_MS,
            "quick": args.quick,
            "baseline": "C = 1.00×（绝对比值 = 该语言耗时 / C 耗时）",
            "method": (
                "外部统一墙钟计时；单次进程内重复 N 次自校准以摊薄启动开销；"
                "扣除 noop 启动基线后除以 N 得单次纯计算耗时；"
                "极快项或被编译器常量折叠成空操作的项统一钳到 5µs 计时下限"
                "（如 C 的 b20 字符串拼接可能被 -O2 折叠）；"
                "28 项 × 4 语言校验和逐项一致作为正确性锚点；"
                "雷达图 6 轴按各维最优语言归一化到 100（越小越好 → 越靠外越优）。"
            ),
            "floor_ms": FLOOR_MS,
        },
        "benchmarks": benchmarks,
        "dimensions": dims,
        "radar": {
            "axes": [dims[k]["label"] for k in axis_order],
            "keys": axis_order,
            "score": radar_score,
        },
    }

    out_path = pathlib.Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
    log(f"✅ 数据已写入 {out_path}（{len(benchmarks)} 项 × {len(LANGS)} 语言）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
