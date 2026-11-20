#!/usr/bin/env bash
# 平台声明门禁（P4.3 平台声明，2026-11-20）：
#   1. dial.toml 顶层 platforms 字段 → pack 写入 manifest；
#   2. install 期平台匹配：当前 host 在白名单 → 可装；
#   3. install 期平台不匹配：拒绝 + 退出码 1 + 提示支持平台与当前平台；
#   4. DIAL_TARGET 覆盖 host 校验（换个受支持平台能装）；
#   5. 向后兼容：无 platforms 的旧包在任意 DIAL_TARGET 下都能装；
#   6. 索引 schema v3 的 platforms 字段 + info 表格/--json 展示；
#   7. platforms 非法 triple（含空白/路径穿越）Fail-Fast。
#
# 铁律：索引用 localhost HTTP（不依赖外网），缓存与已装包隔离到临时 HOME；
# 缺 curl/python3/zig 时整段 skip。
#
# 用法：./tools/verify_p4_platform.sh [编译器路径]
# 退出码：0 = 全绿或 skip；1 = 有失败。

set -u
cd "$(dirname "$0")/.." || exit 1

DIALC="${1:-./target/release/dialc}"
fail=0
ok()   { echo "  ✅ $1"; }
bad()  { echo "  ❌ $1"; fail=1; }
skip() { echo "  ⚠️  $1"; }

if [ ! -x "$DIALC" ]; then
    echo "  ❌ 编译器不存在或不可执行：$DIALC"; exit 1
fi
if ! command -v curl >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1 || ! command -v zig >/dev/null 2>&1; then
    skip "缺 curl/python3/zig，跳过平台声明门禁"; exit 0
fi

TD="$(mktemp -d /tmp/p4platform_gate_XXXX)"
THOME="$TD/home"
mkdir -p "$THOME"
OLD_HOME="${HOME:-}"
export HOME="$THOME"
PORT=$(( 23000 + RANDOM % 20000 ))

cleanup() {
    [ -n "${HTTP_PID:-}" ] && kill "$HTTP_PID" >/dev/null 2>&1 || true
    export HOME="$OLD_HOME"
    for p in pfplat pfold; do "$DIALC" uninstall "$p" >/dev/null 2>&1 || true; done
    rm -rf "$TD"
}
trap cleanup EXIT

# 造一个真实静态库包（x86_64-linux-gnu），platforms 声明 linux 两平台
mk_pkg() {
    local dir="$1" with_platforms="$2" name="$3"
    mkdir -p "$dir/lib/x86_64-linux-gnu"
    echo 'int pfplat(void){return 1;}' > "$TD/pf.c"
    zig cc -target x86_64-linux-gnu -c "$TD/pf.c" -o "$TD/pf.o" 2>/dev/null
    zig ar rcs "$dir/lib/x86_64-linux-gnu/lib${name}.a" "$TD/pf.o" 2>/dev/null
    cat > "$dir/${name}.diali" <<EOF
extern fn pfplat(): int;
EOF
    {
        echo "name = \"$name\""
        echo "version = \"1.0.0\""
        echo "abi = 6"
        echo "iface = \"${name}.diali\""
        echo "license = \"MIT\""
        if [ "$with_platforms" = "yes" ]; then
            echo 'platforms = "x86_64-linux-gnu", "aarch64-linux-gnu"'
        fi
        echo "[targets]"
        echo "x86_64-linux-gnu = \"lib/x86_64-linux-gnu/lib${name}.a\""
        echo "[src]"
    } > "$dir/dial.toml"
}

echo "== 平台声明门禁 =="

# --- 1. pack 写 platforms ---
PKG="$TD/pkg"; mk_pkg "$PKG" yes pfplat
"$DIALC" pack "$PKG" -o "$TD/pfplat.dialpkg" >/dev/null 2>&1
if unzip -p "$TD/pfplat.dialpkg" manifest.txt 2>/dev/null | grep -q '"platforms"'; then
    ok "pack 将 platforms 写入 manifest"
else
    bad "manifest 缺少 platforms 字段"
fi
PLATFORMS_JSON=$(unzip -p "$TD/pfplat.dialpkg" manifest.txt 2>/dev/null | python3 -c "import json,sys; print(','.join(json.load(sys.stdin).get('platforms',[])))" 2>/dev/null)
if echo "$PLATFORMS_JSON" | grep -q 'aarch64-linux-gnu' && echo "$PLATFORMS_JSON" | grep -q 'x86_64-linux-gnu'; then
    ok "manifest platforms 内容正确（$PLATFORMS_JSON）"
else
    bad "manifest platforms 内容异常: $PLATFORMS_JSON"
fi

# --- 2. 匹配平台可装 ---
"$DIALC" uninstall pfplat >/dev/null 2>&1 || true
if DIAL_TARGET=x86_64-linux-gnu "$DIALC" install "$TD/pfplat.dialpkg" 2>&1 | grep -q "已安装"; then
    ok "当前平台在白名单内 → 安装成功"
else
    bad "匹配平台却安装失败"
fi

# --- 3. 不匹配平台拒绝 + 退出码 + 提示 ---
"$DIALC" uninstall pfplat >/dev/null 2>&1 || true
ERR=$(DIAL_TARGET=x86_64-pc-windows-gnu "$DIALC" install "$TD/pfplat.dialpkg" 2>&1)
RC=$?
if [ "$RC" -ne 0 ] && echo "$ERR" | grep -q "不支持当前平台" \
    && echo "$ERR" | grep -q "aarch64-linux-gnu" && echo "$ERR" | grep -q "x86_64-linux-gnu" \
    && echo "$ERR" | grep -q "x86_64-pc-windows-gnu"; then
    ok "平台不匹配 → 非零退出 + 提示当前平台与支持平台列表"
else
    bad "平台不匹配未被正确拒绝（rc=$RC）: $ERR"
fi
# 拒绝时零字节落盘：包目录不应存在
if [ ! -d "$THOME/.dial/packages/pfplat" ]; then
    ok "平台拒绝时零字节落盘（未创建包目录）"
else
    bad "平台拒绝却创建了包目录"
fi

# --- 4. DIAL_TARGET 覆盖：换到另一个受支持平台能装 ---
if DIAL_TARGET=aarch64-linux-gnu "$DIALC" install "$TD/pfplat.dialpkg" 2>&1 | grep -q "已安装"; then
    ok "DIAL_TARGET 切到另一受支持平台 → 可装"
else
    bad "受支持平台经 DIAL_TARGET 覆盖却装不上"
fi

# --- 5. 向后兼容：旧包（无 platforms）任意平台可装 ---
PKG2="$TD/pkg_old"; mk_pkg "$PKG2" no pfold
"$DIALC" pack "$PKG2" -o "$TD/pfold.dialpkg" >/dev/null 2>&1
if unzip -p "$TD/pfold.dialpkg" manifest.txt | grep -q '"platforms"'; then
    bad "旧包不应被写入 platforms 字段"
else
    ok "无 platforms 的旧包 manifest 保持无该字段"
fi
if DIAL_TARGET=x86_64-pc-windows-gnu "$DIALC" install "$TD/pfold.dialpkg" 2>&1 | grep -q "已安装"; then
    ok "无 platforms 旧包在任意平台可装（向后兼容）"
else
    bad "旧包向后兼容被破坏"
fi

# --- 6. 索引 schema v3 platforms + info 展示 ---
IDXDIR="$TD/www"; mkdir -p "$IDXDIR"
cat > "$IDXDIR/index.json" <<EOF
{"schema":3,"updated":"2026-11-20","packages":[
 {"name":"pfi","version":"1.0.0","abi":6,"license":"MIT","desc":"platform demo",
  "targets":["x86_64-linux-gnu"],"url":"http://localhost:$PORT/pfi.dialpkg","sha256":"",
  "platforms":["x86_64-linux-gnu","aarch64-linux-gnu"],"deps":{}}
]}
EOF
# exec 让 python3 替换子 shell，使 $! 是 python3 真 PID（cleanup 能真正回收，避免孤儿占端口）
( cd "$IDXDIR" && exec python3 -m http.server "$PORT" >/dev/null 2>&1 ) & HTTP_PID=$!
sleep 1
IDXURL="http://localhost:$PORT/index.json"
if "$DIALC" info pfi --index "$IDXURL" 2>&1 | grep -q "可安装平台"; then
    ok "info 表格展示可安装平台"
else
    bad "info 未展示可安装平台"
fi
if "$DIALC" info pfi --index "$IDXURL" --json 2>/dev/null | python3 -c "import json,sys; d=json.load(sys.stdin); assert 'aarch64-linux-gnu' in d['versions'][0]['platforms']" 2>/dev/null; then
    ok "info --json 的 platforms 字段正确"
else
    bad "info --json 的 platforms 字段异常"
fi

# --- 7. platforms 非法 triple Fail-Fast ---
BAD="$TD/bad"; mk_pkg "$BAD" no pfplat
# 注入含空白的非法平台（用引号分组绕过逗号切分不影响——这里直接放一个带空格的值）
sed -i 's/^license = "MIT"/license = "MIT"\nplatforms = "bad platform"/' "$BAD/dial.toml"
if "$DIALC" pack "$BAD" -o "$TD/bad.dialpkg" 2>&1 | grep -qi "非法\|空白\|错误"; then
    ok "platforms 含空白的非法 triple 被 Fail-Fast 拒绝"
else
    # 若解析层放行，则检查 pack 返回码非零
    "$DIALC" pack "$BAD" -o "$TD/bad.dialpkg" >/dev/null 2>&1; RC=$?
    if [ "$RC" -ne 0 ]; then ok "platforms 非法 triple 被拒绝（rc=$RC）"; else bad "非法 triple 未被拒绝"; fi
fi

echo "== 平台声明门禁结束（fail=$fail）=="
exit $fail
