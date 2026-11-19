#!/usr/bin/env bash
# 自研后端黄金门禁（tests/run_tests.sh 的 AF 组）。
#
# 铁律：**同一份 .dial 源码，llvm 与 asm 两个后端编译出的程序，运行输出必须逐字节一致**。
# 这比维护一堆 .expected 更可靠——LLVM 后端是既定基准，任何汇编发射的语义偏差
# （调用约定 / 三套标量宽度 / ARC / 块参数 / 变参 print）都会立刻被 diff 抓出来。
#
# 用法：
#   ./tools/run_asm_golden.sh [编译器路径]     # 默认 ./target/release/dialc
#
# 退出码：0 = 全绿；1 = 有失败用例。

set -u

DIALC="${1:-./target/release/dialc}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CASES="$ROOT/tests/asm_golden"
cd "$ROOT" || exit 1
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0
count=0

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 编译器不存在或不可执行: $DIALC"
    exit 1
fi

ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }
skip() { echo "  ⚠️  $1"; }

# 把用例复制到临时目录再编译：obj_path 由源文件推导（同名 .o），asm 后端还会
# 额外产出同名 .s，放临时目录可保证仓库工作树零污染。
for src in "$CASES"/*.dial; do
    [ -f "$src" ] || continue
    name="$(basename "$src" .dial)"
    work="$TMP/$name"
    mkdir -p "$work"
    cp "$src" "$work/$name.dial"
    src2="$work/$name.dial"
    llvm_bin="$work/${name}_llvm"
    asm_bin="$work/${name}_asm"
    count=$((count + 1))

    if ! "$DIALC" build "$src2" --backend=llvm -o "$llvm_bin" >"$work/llvm.log" 2>&1; then
        bad "$name：llvm 后端编译失败"; sed 's/^/       /' "$work/llvm.log" | tail -12; continue
    fi
    if ! "$DIALC" build "$src2" --backend=asm -o "$asm_bin" >"$work/asm.log" 2>&1; then
        bad "$name：asm 后端编译失败"; sed 's/^/       /' "$work/asm.log" | tail -12; continue
    fi
    # 生成 .s 是「真的走了汇编发射路径」的硬证据，防止分派被误改回 LLVM。
    if ! [ -s "$work/$name.s" ]; then
        bad "$name：未生成 .s 汇编产物"; continue
    fi
    if ! "$llvm_bin" >"$work/llvm.out" 2>&1; then
        bad "$name：llvm 产物运行退出码非 0"; continue
    fi
    if ! "$asm_bin" >"$work/asm.out" 2>&1; then
        bad "$name：asm 产物运行退出码非 0"; sed 's/^/       /' "$work/asm.out" | tail -8; continue
    fi
    if diff -u "$work/llvm.out" "$work/asm.out" >"$work/diff" 2>&1; then
        ok "$name：双后端输出逐字节一致（含 .s 产物）"
    else
        bad "$name：双后端输出不一致"; sed 's/^/       /' "$work/diff" | head -30
    fi
done

if [ "$count" -eq 0 ]; then
    bad "未找到任何黄金用例（$CASES/*.dial）"
fi

# 默认后端必须仍是 llvm：不带 --backend 时输出与显式 --backend=llvm 一致。
def_src="$TMP/default.dial"
cp "$CASES/core1.dial" "$def_src"
if "$DIALC" build "$def_src" -o "$TMP/default_bin" >"$TMP/default.log" 2>&1 \
   && "$DIALC" build "$def_src" --backend=llvm -o "$TMP/llvm_bin" >"$TMP/default_llvm.log" 2>&1 \
   && "$TMP/default_bin" >"$TMP/default.out" 2>&1 \
   && "$TMP/llvm_bin" >"$TMP/default_llvm.out" 2>&1 \
   && diff -q "$TMP/default.out" "$TMP/default_llvm.out" >/dev/null 2>&1; then
    ok "默认后端仍为 llvm（不带 --backend 与显式 --backend=llvm 输出一致）"
else
    bad "默认后端行为与 --backend=llvm 不一致"; tail -8 "$TMP/default.log"
fi

# 未覆盖构造必须 Fail-Fast，绝不静默降级。
# 遍历 negative/ 下全部负例：每个负例都必须是「LLVM 能编译、asm 明确报错」。
for neg_src in "$CASES"/negative/*.dial; do
    [ -f "$neg_src" ] || continue
    neg_name="$(basename "$neg_src" .dial)"
    neg_work="$TMP/neg_$neg_name"
    mkdir -p "$neg_work"
    cp "$neg_src" "$neg_work/$neg_name.dial"
    if "$DIALC" build "$neg_work/$neg_name.dial" --backend=llvm -o "$neg_work/llvm_bin" >"$neg_work/llvm.log" 2>&1; then
        if "$DIALC" build "$neg_work/$neg_name.dial" --backend=asm -o "$neg_work/asm_bin" >"$neg_work/asm.log" 2>&1; then
            bad "$neg_name：asm 后端静默放行了未覆盖构造（应为 Fail-Fast）"
        elif grep -qF '[asm 后端]' "$neg_work/asm.log"; then
            ok "$neg_name：asm 后端明确报错（Fail-Fast），LLVM 后端仍可用"
        else
            bad "$neg_name：asm 后端失败但错误信息缺少 [asm 后端] 前缀"; tail -8 "$neg_work/asm.log"
        fi
    else
        bad "$neg_name：llvm 后端竟编译失败（用例本身不合法）"; tail -8 "$neg_work/llvm.log"
    fi
done

# AArch64 汇编语法（可选）：交叉汇编出 ELF aarch64 relocatable .o。
# 该检查只验证助记符合法性，不验证运行语义（x86 宿主无法执行 aarch64 程序）。
if command -v clang >/dev/null 2>&1 && command -v file >/dev/null 2>&1; then
    arm_work="$TMP/aarch64"
    mkdir -p "$arm_work"
    cp "$CASES/core3.dial" "$arm_work/aarch64.dial"
    if DIAL_ASM_TARGET=aarch64 DIAL_C_COMPILER="clang --target=aarch64-linux-gnu" \
       "$DIALC" build "$arm_work/aarch64.dial" --backend=asm --no-link \
       -o "$arm_work/aarch64.o" >"$arm_work/build.log" 2>&1 \
       && file "$arm_work/aarch64.o" 2>/dev/null | grep -qF 'ARM aarch64'; then
        ok "AArch64 汇编语法通过交叉汇编器（产出 ELF aarch64 .o）"
    else
        bad "AArch64 汇编语法校验失败"; tail -12 "$arm_work/build.log"
    fi
else
    skip "跳过 AArch64 语法校验（缺少 clang 或 file）"
fi

# --fast-compile（M7-e3an3）：极速编译模式下双后端输出仍须逐字节一致。
fc_work="$TMP/fast_compile"
mkdir -p "$fc_work"
cp "$CASES/core1.dial" "$fc_work/core1.dial"
if "$DIALC" build "$fc_work/core1.dial" --backend=llvm --fast-compile -o "$fc_work/llvm" >"$fc_work/llvm.log" 2>&1 \
   && "$DIALC" build "$fc_work/core1.dial" --backend=asm --fast-compile -o "$fc_work/asm" >"$fc_work/asm.log" 2>&1 \
   && "$fc_work/llvm" >"$fc_work/llvm.out" 2>&1 \
   && "$fc_work/asm" >"$fc_work/asm.out" 2>&1; then
    if diff -q "$fc_work/llvm.out" "$fc_work/asm.out" >/dev/null 2>&1; then
        ok "--fast-compile：双后端输出逐字节一致"
        count=$((count + 1))
    else
        bad "--fast-compile：双后端输出不一致"
    fi
else
    bad "--fast-compile：编译/运行失败"; tail -8 "$fc_work/asm.log"
fi

# ReleaseFast（--fast）双后端一致性：覆盖 asm 后端在 ReleaseFast 下内联 dial_array_get
# 的快路径。默认 safety=ReleaseSafe 的主循环不经过该路径，若不单列 --fast 组，
# 内联发射的任何语义偏差都会逃过门禁（审计「门禁盲区」教训）。此处 --fast 下
# LLVM 走 LTO 内联、asm 走发射内联，两者输出必须逐字节一致。
fast_work="$TMP/fast_all"; mkdir -p "$fast_work"
fast_ok=0; fast_fail=0
for src in "$CASES"/*.dial; do
    [ -f "$src" ] || continue
    name="$(basename "$src" .dial)"
    fw="$fast_work/$name"; mkdir -p "$fw"; cp "$src" "$fw/$name.dial"
    if ! "$DIALC" build "$fw/$name.dial" --backend=llvm --fast -o "$fw/llvm" >"$fw/ll.log" 2>&1; then
        bad "--fast/$name：llvm 编译失败"; fast_fail=$((fast_fail+1)); continue
    fi
    if ! "$DIALC" build "$fw/$name.dial" --backend=asm --fast -o "$fw/asm" >"$fw/as.log" 2>&1; then
        bad "--fast/$name：asm 编译失败"; sed 's/^/       /' "$fw/as.log" | tail -8; fast_fail=$((fast_fail+1)); continue
    fi
    "$fw/llvm" >"$fw/ll.out" 2>&1; "$fw/asm" >"$fw/as.out" 2>&1
    if diff -q "$fw/ll.out" "$fw/as.out" >/dev/null 2>&1; then
        fast_ok=$((fast_ok+1))
    else
        bad "--fast/$name：双后端输出不一致"; diff "$fw/ll.out" "$fw/as.out" | head -10; fast_fail=$((fast_fail+1))
    fi
done
if [ "$fast_fail" -eq 0 ]; then
    ok "ReleaseFast(--fast) 双后端一致：$fast_ok 用例（覆盖 dial_array_get 内联快路径）"
else
    bad "ReleaseFast(--fast) 有 $fast_fail 用例不一致"
fi

# O3 专项（M9-linrec）：asm 后端 O3-only 的线性递推闭式特化（仿射/斐波那契）
# 在主循环（O2）与 --fast（O2）组都不触发。此段用 --fast -O3 双后端编译
# tests/asm_golden_o3/*.dial，LLVM 忽略 opt-level、asm 走闭式折叠，输出必须逐字节一致。
# 钉死：① 仿射递推 s=(a*s+b)%m；② 斐波那契 (a,b)→(b,a+b)；③ 非递推/带参循环不误折叠。
O3CASES="$ROOT/tests/asm_golden_o3"
if [ -d "$O3CASES" ]; then
    o3_work="$TMP/o3_all"; mkdir -p "$o3_work"
    o3_ok=0; o3_fail=0
    for src in "$O3CASES"/*.dial; do
        [ -f "$src" ] || continue
        name="$(basename "$src" .dial)"
        ow="$o3_work/$name"; mkdir -p "$ow"; cp "$src" "$ow/$name.dial"
        if ! "$DIALC" build "$ow/$name.dial" --backend=llvm --fast -O3 -o "$ow/llvm" >"$ow/ll.log" 2>&1; then
            bad "O3/$name：llvm 编译失败"; o3_fail=$((o3_fail+1)); continue
        fi
        if ! "$DIALC" build "$ow/$name.dial" --backend=asm --fast -O3 -o "$ow/asm" >"$ow/as.log" 2>&1; then
            bad "O3/$name：asm 编译失败"; sed 's/^/       /' "$ow/as.log" | tail -8; o3_fail=$((o3_fail+1)); continue
        fi
        "$ow/llvm" >"$ow/ll.out" 2>&1; "$ow/asm" >"$ow/as.out" 2>&1
        if diff -q "$ow/ll.out" "$ow/as.out" >/dev/null 2>&1; then
            o3_ok=$((o3_ok+1))
        else
            bad "O3/$name：双后端输出不一致"; diff "$ow/ll.out" "$ow/as.out" | head -10; o3_fail=$((o3_fail+1))
        fi
    done
    if [ "$o3_fail" -eq 0 ]; then
        ok "O3(--fast -O3) 双后端一致：$o3_ok 用例（覆盖 M9-linrec 线性递推闭式）"
    else
        bad "O3 有 $o3_fail 用例不一致"
    fi
fi

echo "  共 $count 个双后端黄金用例（另 $fast_ok 个 --fast 快速路径、$o3_ok 个 O3 用例）"
exit "$fail"
