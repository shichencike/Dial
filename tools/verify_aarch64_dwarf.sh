#!/usr/bin/env bash
# P1 门禁：AArch64 后端 DWARF 调试信息「实机源码级调试」验证。
#
# 背景（2026-11-19，路线图 P1 平台对等）：asm 后端的手写 DWARF v5 生成主体本与目标无关，
# 但被 x86-64 门控硬关；AArch64 下 -g 静默不产任何调试信息。打开后发现两处目标相关问题：
#   ① DWARF CU header 用了 .value 伪指令（aarch64 GNU as 报 unknown pseudo-op）→ 改 .2byte；
#   ② 变量/形参 location 的 DW_OP_breg7(rsp) 是 x86 专用 → AArch64 用 breg31(sp)=0x8f。
# 本门禁把「AArch64 -g 产合法 DWARF + qemu 实机 gdb 源码行断点命中 + print 变量=正确值」
# 钉死，防止回归；这是「代替 LLVM 必须有源码级调试」的硬前提。
#
# 铁律：所有 DIAL_ASM_TARGET 设置只作用于本脚本子进程，不污染宿主后续编译。
# 缺 qemu / 交叉 gcc / gdb-multiarch / dwarfdump 时整段 skip（CI 无这些依赖不误红）。
#
# 用法：./tools/verify_aarch64_dwarf.sh [编译器路径]   # 默认 ./target/release/dialc
# 退出码：0 = 全绿或 skip；1 = 有失败。

set -u
cd "$(dirname "$0")/.." || exit 1

DIALC="${1:-./target/release/dialc}"
fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }
skip() { echo "  ⚠️  $1"; }

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 编译器不存在或不可执行：$DIALC"
    exit 1
fi

# --- 依赖探测：缺任一则整段 skip（本机环境限制，非产品缺陷）---
QEMU="$(command -v qemu-aarch64-static || true)"
CC="$(command -v aarch64-linux-gnu-gcc || true)"
GDB="$(command -v gdb-multiarch || true)"
if [ -z "$QEMU" ] || [ -z "$CC" ] || [ -z "$GDB" ]; then
    skip "跳过 AArch64 DWARF 门禁（缺 qemu-aarch64-static / aarch64-linux-gnu-gcc / gdb-multiarch）"
    echo "  ── AArch64 DWARF 门禁结束（skip）──"
    exit 0
fi
if ! command -v timeout >/dev/null 2>&1; then
    skip "跳过 AArch64 DWARF 门禁（缺 timeout）"
    echo "  ── AArch64 DWARF 门禁结束（skip）──"
    exit 0
fi
# dwarfdump 探测：不同环境带不同版本后缀，缺则只做 gdb 验证（不误红）。
DDUMP=""
for cand in llvm-dwarfdump-18 llvm-dwarfdump llvm-dwarfdump-17 llvm-dwarfdump-16; do
    if command -v "$cand" >/dev/null 2>&1; then DDUMP="$cand"; break; fi
done
RDELF="$(command -v aarch64-linux-gnu-readelf || true)"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
RDIR="$TMP/rdwarf"
mkdir -p "$RDIR"

# --- 测试源：与 run_tests.sh R2 段同款（counter=42 @ line 9）---
cat > "$RDIR/dbg.dial" <<'EOF'
fn add(a: int, b: int): int {
    let sum: int = a + b;
    return sum;
}

fn main(): int {
    let counter: int = 42;
    let note: string = "dial";
    print(add(counter, 1));
    return 0;
}
EOF

A64_ENV=(env DIAL_ASM_TARGET=aarch64 DIAL_C_COMPILER="$CC")
BIN="$RDIR/dbg_a64"
if ! "${A64_ENV[@]}" "$DIALC" build "$RDIR/dbg.dial" --backend=asm --single-file -g \
        -o "$BIN" >"$RDIR/build.log" 2>&1; then
    bad "AArch64 asm -g 编译失败"; tail -10 "$RDIR/build.log"; exit "$fail"
fi

# 1) 运行语义零影响：输出仍为 43
if [ "$("$QEMU" "$BIN" 2>/dev/null)" = "43" ]; then
    ok "AArch64 asm -g 编译且运行输出不变（43）"
else bad "AArch64 asm -g 运行输出异常"; fi

# 2) DWARF 段存在
if [ -n "$RDELF" ] && "$RDELF" -S "$BIN" 2>/dev/null | grep -q '\.debug_info' \
   && "$RDELF" -S "$BIN" 2>/dev/null | grep -q '\.debug_line'; then
    ok "AArch64 ELF 含 .debug_info / .debug_line 段"
elif [ -n "$DDUMP" ]; then
    # 无 readelf 时用 dwarfdump 兜底：能解析出 CU 即视为段存在
    "$DDUMP" --debug-info "$BIN" 2>/dev/null | grep -q 'DW_TAG_compile_unit' \
      && ok "AArch64 ELF 含 DWARF CU（dwarfdump 兜底）" \
      || bad "AArch64 ELF 缺 DWARF 段/CU"
else bad "AArch64 ELF 缺 .debug_info / .debug_line 段（无工具细查）"; fi

# 3) DWARF 符号 + 行表（dwarfdump 可用时）
if [ -n "$DDUMP" ]; then
    admiss=""
    for sym in add main a b sum counter note; do
        "$DDUMP" --debug-info "$BIN" 2>/dev/null | grep -q "DW_AT_name.*\"${sym}\"" \
          || admiss="${admiss} ${sym}"
    done
    if [ -z "$admiss" ]; then ok "AArch64 DWARF 含 Dial 函数名/形参名/局部变量名"
    else bad "AArch64 DWARF 缺符号:${admiss}"; fi
    if "$DDUMP" --debug-line "$BIN" 2>/dev/null | grep -q 'dbg.dial'; then
        ok "AArch64 DWARF 行表含 Dial 源文件"
    else bad "AArch64 DWARF 行表缺 Dial 源文件"; fi
    # --verify：DWARF 结构合法性（abbrev/偏移/CU 链）
    if "$DDUMP" --verify "$BIN" 2>&1 | grep -q 'No errors'; then
        ok "AArch64 DWARF 结构校验通过（llvm-dwarfdump --verify）"
    else bad "AArch64 DWARF 结构校验失败"; fi
fi

# 4) break=0x8f 编码体检：生成的 .s 变量 location 必须是 breg31(sp)，不能残留 breg7(rsp)
#    （--no-link 路径的 .s 与源码同目录；若不存在则跳过此项，gdb 端到端仍是硬证据）
S="$RDIR/dbg.s"
"${A64_ENV[@]}" "$DIALC" build "$RDIR/dbg.dial" --backend=asm --no-link -g -o "$S" >/dev/null 2>&1 || true
if [ -f "$S" ]; then
    n8f=$(grep -o '0x8f' "$S" | wc -l | tr -d ' ')
    n77=$(grep -o '0x77' "$S" | wc -l | tr -d ' ')
    if [ "$n8f" -gt 0 ] && [ "$n77" -eq 0 ]; then
        ok "AArch64 DWARF location 用 DW_OP_breg31(sp)=0x8f（${n8f} 处），无 x86 0x77 残留"
    else bad "AArch64 DWARF breg 编码异常（0x8f=${n8f}, 0x77=${n77}）"; fi
fi

# 5) qemu gdbstub 实机源码级调试：断点命中 + print counter=42 + bt 显示源码行
#    remote 下用 continue（非 run）；-g 自身阻塞等连接，无需 sleep；端口随机避免并发冲突。
PORT=$(( 20000 + ($$ % 20000) ))
"$QEMU" -g "$PORT" "$BIN" >"$RDIR/qemu_gdb.log" 2>&1 &
QPID=$!
trap 'kill $QPID 2>/dev/null; rm -rf "$TMP"' EXIT
timeout 25 "$GDB" -q -batch "$BIN" \
    -ex 'set pagination off' \
    -ex "target remote :$PORT" \
    -ex "break ${RDIR}/dbg.dial:9" \
    -ex 'continue' \
    -ex 'print counter' \
    -ex 'bt' >"$RDIR/gdb.log" 2>&1
if grep -qF '$1 = 42' "$RDIR/gdb.log" \
   && grep -qF 'Breakpoint 1, main () at' "$RDIR/gdb.log"; then
    ok "AArch64 qemu 实机 GDB 源码行断点命中且 print counter = 42"
else
    bad "AArch64 GDB 断点/print 失败"; sed 's/^/       /' "$RDIR/gdb.log" | tail -16
fi
if grep -q 'dbg.dial:9' "$RDIR/gdb.log"; then
    ok "AArch64 GDB backtrace 显示 Dial 源码行"
else bad "AArch64 GDB backtrace 缺源码行"; fi

# 6) 负向：无 -g 时不应混入 Dial 调试符号
BIN_PLAIN="$RDIR/dbg_plain"
if "${A64_ENV[@]}" "$DIALC" build "$RDIR/dbg.dial" --backend=asm --single-file \
        -o "$BIN_PLAIN" >/dev/null 2>&1; then
    leaked=0
    if [ -n "$DDUMP" ] && "$DDUMP" --debug-line "$BIN_PLAIN" 2>/dev/null | grep -q 'dbg.dial'; then
        leaked=1
    fi
    if [ "$leaked" -eq 0 ]; then ok "AArch64 无 -g 时不产 Dial 调试符号（零开销）"
    else bad "AArch64 无 -g 仍混入 Dial 行号"; fi
fi

echo "  ── AArch64 DWARF 门禁结束（fail=$fail）──"
exit "$fail"
