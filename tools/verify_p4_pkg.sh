#!/usr/bin/env bash
# P4.0 门禁：dialpkg 包管理核心能力验证（2026-11-19 包管理路线图）。
#
# 覆盖路线图 §6 可复现验证：
#   1. pack 编 C 源：含 [c] 段的样例包 → 自动交叉编译三平台 .a + 自动 manifest/SHA256；
#   2. 可复现：同一输入两次 pack 产物逐字节一致；
#   3. pack→install→import 联动：prebuilt 包装完双后端编译运行输出逐字节一致；
#   4. 源码随包审计：prebuilt 包含 c/ 源 + native-src/ 存档；
#   5. 哈希篡改拦截：改包内任一文件 → install 拒绝且不破坏已装旧包；
#   6. 原子安装：全新/覆盖安装后无 .install-*/.backup-* 残留；
#   7. source 模式：install 期就地声明式编译 C 源 + 内容哈希缓存落 .a；
#   8. 工具链缺失：source 包找不到 zig 时 Fail-Fast 给安装指引；
#   9. --json：install/list/uninstall/new 输出可被 python json 解析；
#  10. doctor：含「包平台/ABI 一致性」检查项。
#
# 铁律：所有包都装到临时 HOME（DIAL_HOME 若被支持，否则用系统缓存并清理本脚本装的包），
# 缺 zig 时整段 skip（gcc-only 环境不误红，但 pack 编 C 源需 zig 才能跑）。
#
# 用法：./tools/verify_p4_pkg.sh [编译器路径]   # 默认 ./target/release/dialc
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

# 交叉编译依赖 zig：缺则整段 skip（保持 gcc-only 环境不误红）。
if ! command -v zig >/dev/null 2>&1; then
    skip "未找到 zig，跳过 P4.0 C 源包门禁（pack 编 C 源需 zig cc -target）"
    exit 0
fi

TD="$(mktemp -d /tmp/p4pkg_gate_XXXX)"
cleanup() {
    "$DIALC" uninstall p4gate_prebuilt >/dev/null 2>&1 || true
    "$DIALC" uninstall p4gate_source >/dev/null 2>&1 || true
    rm -rf "$TD"
}
trap cleanup EXIT

# ---------- 造样例包（prebuilt + source）----------
mk_pkg() {
    local dir="$1" mode="$2" name="$3" fn="$4" expr="$5"
    mkdir -p "$dir/c/include" "$dir/native-src/upstream"
    cat > "$dir/dial.toml" <<EOF
name = "$name"
version = "1.0.0"
abi = 6
iface = "$name.diali"
license = "MIT"

[src]

[c]
sources = "c/$name.c"
includes = "c/include"
flags = "-O2", "-Wall"
defines = "PV=1"
mode = "$mode"
EOF
    cat > "$dir/$name.diali" <<EOF
// P4.0 门禁样例包
extern fn $fn(a: int, b: int): int;
EOF
    cat > "$dir/c/include/$name.h" <<EOF
#ifndef ${name}_H
#define ${name}_H
long $fn(long a, long b);
#endif
EOF
    cat > "$dir/c/$name.c" <<EOF
#include "$name.h"
long $fn(long a, long b) { return $expr; }
EOF
    echo "// 第三方源码快照，仅供审计不编译" > "$dir/native-src/upstream/README.txt"
}

PB="$TD/prebuilt"; SRC="$TD/source"
mk_pkg "$PB" prebuilt p4gate_prebuilt p4gate_add "a + b + PV - 1"
mk_pkg "$SRC" source p4gate_source p4gate_mul "a * b + PV - 1"

echo "== P4.0 1/10: pack 编 C 源（prebuilt 三平台）=="
if "$DIALC" pack "$PB" -o "$PB/pkg.dialpkg" >/dev/null 2>&1; then
    n=$(find "$PB/lib" -name '*.a' 2>/dev/null | wc -l)
    [ "$n" -eq 3 ] && ok "自动交叉编译三平台 .a（3 个）" || bad "三平台 .a 数量=$n（应 3）"
else
    bad "pack prebuilt 包失败"
fi

echo "== P4.0 2/10: 可复现（两次 pack 逐字节一致）=="
cp "$PB/pkg.dialpkg" "$TD/run1.dialpkg"
"$DIALC" pack "$PB" -o "$PB/pkg.dialpkg" >/dev/null 2>&1
if cmp -s "$TD/run1.dialpkg" "$PB/pkg.dialpkg"; then ok "同一输入两次 pack 逐字节一致"; else bad "两次 pack 产物不一致"; fi

echo "== P4.0 3/10: 源码随包审计 =="
if unzip -l "$PB/pkg.dialpkg" 2>/dev/null | grep -q 'c/p4gate_prebuilt.c' \
   && unzip -l "$PB/pkg.dialpkg" 2>/dev/null | grep -q 'native-src/upstream/README.txt'; then
    ok "prebuilt 包含 c/ 源 + native-src/ 存档"
else
    bad "prebuilt 包缺 C 源或 native-src 存档"
fi

echo "== P4.0 4/10: install → import → 双后端运行 =="
"$DIALC" uninstall p4gate_prebuilt >/dev/null 2>&1 || true
if "$DIALC" install "$PB/pkg.dialpkg" >/dev/null 2>&1; then
    cat > "$TD/use.dial" <<'EOF'
import "p4gate_prebuilt";
fn main(): int {
    print(p4gate_add(2, 3));
    return 0;
}
EOF
    lo=$(LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 "$DIALC" run "$TD/use.dial" --backend llvm 2>/dev/null | tail -1)
    la=$(LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 "$DIALC" run "$TD/use.dial" --backend asm 2>/dev/null | tail -1)
    [ "$lo" = "5" ] && [ "$la" = "5" ] && ok "双后端 import 运行输出一致（5）" \
        || bad "双后端输出不一致 llvm=$lo asm=$la（应 5）"
else
    bad "install prebuilt 包失败"
fi

echo "== P4.0 5/10: 原子安装无残留 =="
"$DIALC" install "$PB/pkg.dialpkg" >/dev/null 2>&1  # 覆盖安装
leftover=$(find /root/.dial/packages -maxdepth 1 \( -name '.install-*' -o -name '.backup-*' \) 2>/dev/null | wc -l)
[ "$leftover" -eq 0 ] && ok "全新/覆盖安装后无 .install/.backup 残留" || bad "残留临时目录 $leftover 个"

echo "== P4.0 6/10: 哈希篡改拦截 + 旧包完好 =="
oldh=$(sha256sum /root/.dial/packages/p4gate_prebuilt/p4gate_prebuilt.diali 2>/dev/null | cut -d' ' -f1)
TM="$TD/tamper"; mkdir -p "$TM"; ( cd "$TM" && unzip -q "$PB/pkg.dialpkg" && echo "// TAMPER" >> p4gate_prebuilt.diali && zip -q -r t.dialpkg manifest.txt p4gate_prebuilt.diali lib )
"$DIALC" install "$TM/t.dialpkg" >/dev/null 2>&1 && bad "篡改包竟安装成功" || ok "篡改包被 SHA256 拒绝"
newh=$(sha256sum /root/.dial/packages/p4gate_prebuilt/p4gate_prebuilt.diali 2>/dev/null | cut -d' ' -f1)
[ "$oldh" = "$newh" ] && ok "篡改被拒后旧包完好" || bad "旧包被篡改包破坏"

echo "== P4.0 7/10: source 模式 install 期就地编译 =="
if "$DIALC" pack "$SRC" -o "$SRC/pkg.dialpkg" >/dev/null 2>&1 \
   && "$DIALC" install "$SRC/pkg.dialpkg" >/dev/null 2>&1; then
    [ -n "$(find /root/.dial/packages/p4gate_source/lib -name '*.a' 2>/dev/null)" ] \
        && ok "source 包 install 期编译出本机 .a" || bad "source 包未产出 .a"
    cat > "$TD/use_src.dial" <<'EOF'
import "p4gate_source";
fn main(): int { print(p4gate_mul(6, 7)); return 0; }
EOF
    out=$(LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 "$DIALC" run "$TD/use_src.dial" --backend asm 2>/dev/null | tail -1)
    [ "$out" = "42" ] && ok "source 包 import 运行正确（42）" || bad "source 包输出=$out（应 42）"
else
    bad "source 包 pack/install 失败"
fi

echo "== P4.0 8/10: 工具链缺失 Fail-Fast =="
"$DIALC" uninstall p4gate_source >/dev/null 2>&1 || true
msg=$(PATH=/usr/bin:/bin "$DIALC" install "$SRC/pkg.dialpkg" 2>&1 | tail -3)
echo "$msg" | grep -q "source 模式" && ok "缺 zig 时 Fail-Fast 给安装指引" || bad "缺 zig 未 Fail-Fast：$msg"

echo "== P4.0 9/10: --json 全覆盖 =="
for spec in "list --json" "install $PB/pkg.dialpkg --json"; do
    "$DIALC" $spec 2>/dev/null | python3 -m json.tool >/dev/null 2>&1 \
        && ok "$spec 输出合法 JSON" || bad "$spec 非合法 JSON"
done
"$DIALC" new "$TD/newproj" --json 2>/dev/null | python3 -m json.tool >/dev/null 2>&1 \
    && ok "new --json 合法" || bad "new --json 非法"
"$DIALC" uninstall p4gate_prebuilt --json 2>/dev/null | python3 -m json.tool >/dev/null 2>&1 \
    && ok "uninstall --json 合法" || bad "uninstall --json 非法"

echo "== P4.0 10/10: doctor 包平台/ABI 检查 =="
"$DIALC" doctor --json 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); assert any("平台/ABI" in c["name"] for c in d["checks"])' \
    && ok "doctor 含包平台/ABI 一致性检查" || bad "doctor 缺包平台/ABI 检查"

echo "  ── P4.0 包管理门禁结束（fail=$fail）──"
exit "$fail"
