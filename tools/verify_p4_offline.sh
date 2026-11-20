#!/usr/bin/env bash
# P4.3 下载层门禁（2026-11-20 包管理路线图）：
#   1. 联机 URL 安装 → 下载缓存落盘；
#   2. --offline 命中缓存（停掉 HTTP 服务后仍能装）；
#   3. --offline 缺缓存 → Fail-Fast（非零退出 + 指引）；
#   4. SHA256 不符 → 拒绝安装且不缓存；
#   5. DIAL_MIRROR 前缀重写（含重写提示）；
#   6. 断点续传（半截 .part → 补全 → 缓存恢复完整）；
#   7. --json 输出纯净（无进度条/提示污染，可被 python json 解析）。
#
# 铁律：HTTP 用 localhost（不依赖外网），缓存与已装包隔离到临时 HOME；
# 缺 curl/python3/zig 时整段 skip。
#
# 用法：./tools/verify_p4_offline.sh [编译器路径]
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
    skip "缺 curl/python3/zig，跳过 P4.3 下载层门禁"; exit 0
fi

TD="$(mktemp -d /tmp/p43offline_gate_XXXX)"
OLD_HOME="${HOME:-}"
kill_pids() { for f in "$TD"/http*.pid; do [ -f "$f" ] && kill "$(cat "$f")" >/dev/null 2>&1 || true; done; }
cleanup() {
    kill_pids
    export HOME="$OLD_HOME"
    "$DIALC" uninstall dlgate >/dev/null 2>&1 || true
    rm -rf "$TD"
}
trap cleanup EXIT

PORT=$(( 24000 + RANDOM % 20000 ))
WWW="$TD/www"; mkdir -p "$WWW/lib/x86_64-linux-gnu"

# 造一个真包
echo 'int dlgate(void){return 7;}' > "$TD/dl.c"
zig cc -target x86_64-linux-gnu -c "$TD/dl.c" -o "$TD/dl.o" 2>/dev/null
zig ar rcs "$WWW/lib/x86_64-linux-gnu/libdlgate.a" "$TD/dl.o" 2>/dev/null
cat > "$WWW/dlgate.diali" <<'EOF'
extern fn dlgate(): int;
EOF
cat > "$WWW/dial.toml" <<'EOF'
name = "dlgate"
version = "1.0.0"
abi = 6
iface = "dlgate.diali"
license = "MIT"
[targets]
x86_64-linux-gnu = "lib/x86_64-linux-gnu/libdlgate.a"
[src]
EOF
"$DIALC" pack "$WWW" -o "$WWW/dlgate.dialpkg" >/dev/null 2>&1
SHA=$(sha256sum "$WWW/dlgate.dialpkg" | cut -d' ' -f1)
H="$TD/home"; mkdir -p "$H/.dial"

# 启动静态 HTTP 服务并等待端口真正就绪。
# 关键：必须用 exec 让 python3 替换子 shell，使 $! 记录的就是 python3 进程 PID；
# 否则 kill $! 只杀到子 shell，python3 变孤儿持续占端口（会累积并撞随机端口）。
# 再用 curl 轮询直到端口可连，消除 sleep 固定等待的就绪竞态。
start_http() {
    local p="$1" pf="$2"
    ( cd "$WWW" && exec python3 -m http.server "$p" >/dev/null 2>&1 ) &
    echo $! > "$pf"
    local i
    for i in $(seq 1 50); do
        if curl -sf -o /dev/null "http://localhost:$p/" 2>/dev/null; then return 0; fi
        if ! kill -0 "$(cat "$pf")" 2>/dev/null; then return 1; fi  # 服务进程已死，端口可能被占
        sleep 0.1
    done
    return 1
}

echo "== P4.3 下载层门禁 =="

# --- 1. 联机下载缓存落盘 ---
P1=$PORT; start_http "$P1" "$TD/http1.pid"
URL="http://localhost:$P1/dlgate.dialpkg"
if HOME="$H" "$DIALC" install "$URL" --sha256 "$SHA" 2>&1 | grep -q "已安装"; then
    ok "联机 URL 安装成功"
else
    bad "联机 URL 安装失败"
fi
CACHE=$(find "$H" -path '*downloads/*_dlgate.dialpkg' | head -1)
if [ -n "$CACHE" ] && [ -s "$CACHE" ]; then
    ok "下载缓存落盘（$CACHE）"
else
    bad "下载缓存未落盘"
fi

# --- 2. --offline 命中（停 HTTP）---
kill "$(cat "$TD/http1.pid")" 2>/dev/null || true; sleep 1
"$DIALC" uninstall dlgate >/dev/null 2>&1 || true
if HOME="$H" "$DIALC" install "$URL" --sha256 "$SHA" --offline 2>&1 | grep -q "已安装"; then
    ok "--offline 命中缓存（HTTP 已停仍可装）"
else
    bad "--offline 命中缓存失败"
fi

# --- 3. --offline 缺缓存 Fail-Fast ---
H2="$TD/home2"; mkdir -p "$H2/.dial"
ERR=$(HOME="$H2" "$DIALC" install "$URL" --sha256 "$SHA" --offline 2>&1); RC=$?
if [ "$RC" -ne 0 ] && echo "$ERR" | grep -q "缓存中找不到"; then
    ok "--offline 缺缓存 → 非零退出 + 指引"
else
    bad "--offline 缺缓存未正确失败（rc=$RC）: $ERR"
fi

# --- 4. SHA256 不符拒绝 + 不缓存 ---
H3="$TD/home3"; mkdir -p "$H3/.dial"
P4=$((PORT+1)); start_http "$P4" "$TD/http4.pid"
BADURL="http://localhost:$P4/dlgate.dialpkg"
WRONG=$(printf '0%.0s' {1..64})
ERR=$(HOME="$H3" "$DIALC" install "$BADURL" --sha256 "$WRONG" 2>&1); RC=$?
if [ "$RC" -ne 0 ] && echo "$ERR" | grep -q "SHA256 不符"; then
    ok "SHA256 不符 → 拒绝安装"
else
    bad "SHA256 不符未被拒绝（rc=$RC）"
fi
if [ -z "$(find "$H3" -path '*downloads/*_dlgate.dialpkg' 2>/dev/null | head -1)" ]; then
    ok "SHA256 不符时不缓存坏文件"
else
    bad "SHA256 不符却缓存了文件"
fi

# --- 5. 镜像重写 ---
H4="$TD/home4"; mkdir -p "$H4/.dial"
P5=$((PORT+2)); start_http "$P5" "$TD/http5.pid"
ORIG="http://localhost:$P4/dlgate.dialpkg"
MIRROR="http://localhost:$P4=http://localhost:$P5"
OUT=$(HOME="$H4" DIAL_MIRROR="$MIRROR" "$DIALC" install "$ORIG" --sha256 "$SHA" 2>&1)
if echo "$OUT" | grep -q "镜像重写" && echo "$OUT" | grep -q "已安装"; then
    ok "DIAL_MIRROR 前缀重写生效"
else
    bad "镜像重写失败: $OUT"
fi

# --- 6. 断点续传 ---
H5="$TD/home5"; mkdir -p "$H5/.dial"
P6=$((PORT+3)); start_http "$P6" "$TD/http6.pid"
RURL="http://localhost:$P6/dlgate.dialpkg"
HOME="$H5" "$DIALC" install "$RURL" --sha256 "$SHA" >/dev/null 2>&1
RCACHE=$(find "$H5" -path '*downloads/*_dlgate.dialpkg' | head -1)
FULL=$(stat -c%s "$RCACHE")
head -c 100 "$RCACHE" > "${RCACHE}.part"
rm -f "$RCACHE"
"$DIALC" uninstall dlgate >/dev/null 2>&1 || true
HOME="$H5" "$DIALC" install "$RURL" --sha256 "$SHA" >/dev/null 2>&1
if [ "$(stat -c%s "$RCACHE" 2>/dev/null)" = "$FULL" ]; then
    ok "断点续传补全缓存（$FULL 字节）"
else
    bad "断点续传未恢复完整缓存"
fi

# --- 7. --json 输出纯净 ---
H6="$TD/home6"; mkdir -p "$H6/.dial"
P7=$((PORT+4)); start_http "$P7" "$TD/http7.pid"
JURL="http://localhost:$P7/dlgate.dialpkg"
if HOME="$H6" "$DIALC" install "$JURL" --sha256 "$SHA" --json 2>/dev/null | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['ok'] and d['name']=='dlgate'" 2>/dev/null; then
    ok "--json 输出纯 JSON（无进度/提示污染）"
else
    bad "--json 输出不纯净"
fi

echo "== P4.3 下载层门禁结束（fail=$fail）=="
exit $fail
