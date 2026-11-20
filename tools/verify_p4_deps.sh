#!/usr/bin/env bash
# P4.4 门禁：包管理依赖闭环 + 工程加固（2026-11-20 缺口盘点后的补丁批）。
#
# 覆盖：
#   1. pack 把 [dependencies] 写进 manifest.txt（deps 不再只活于索引）；
#   2. install <本地包> --json 报告 declared/satisfied/missing（默认不 Fail）；
#   3. install --with-deps 按 manifest 依赖自动 resolve + 递归安装（本地索引 HTTP）；
#   4. --with-deps 缺依赖/索引失败 Fail-Fast（ok=false + 退出码 1）；
#   5. uninstall 反依赖检查：被其它已装包依赖时拒绝卸载；
#   6. 并发安装锁：锁文件存在时 Second 安装被拒（fail-fast 提示）；
#   7. semver 版本比较单测（search/info 不再把 10.0.0 排到 2.0.0 后）；
#   8. 原子锁崩溃自愈（陈旧锁 mtime 超阈值被清理）——用注入 mtime 模拟。
#
# 铁律：索引用 localhost HTTP（不依赖外网），缓存/已装包/锁隔离到临时 HOME；
# 缺 curl/python3/zip/unzip 时整段 skip。零新依赖、LLVM 路径零改动。
#
# 用法：./tools/verify_p4_deps.sh [编译器路径]
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
if ! command -v curl >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1 \
   || ! command -v unzip >/dev/null 2>&1; then
    skip "缺 curl/python3/unzip，跳过 P4.4 依赖闭环门禁"; exit 0
fi

echo "== AP2) P4.4 依赖闭环与工程加固门禁 =="

TD="$(mktemp -d /tmp/p44deps_gate_XXXX)"
THOME="$TD/home"
mkdir -p "$THOME"
OLD_HOME="${HOME:-}"
export HOME="$THOME"
PORT=$(( 24000 + RANDOM % 15000 ))

cleanup() {
    [ -n "${HTTP_PID:-}" ] && kill "$HTTP_PID" >/dev/null 2>&1 || true
    export HOME="$OLD_HOME"
    rm -rf "$TD"
}
trap cleanup EXIT

# 造一个 .dialpkg；$1=name $2=fn $3=版本 $4=输出 $5..=额外 dial.toml 行（依赖段）
make_pkg() {
    local name="$1" fn="$2" ver="$3" out="$4"; shift 4
    local d="$TD/src_${name}_${ver}"
    mkdir -p "$d/c/include" "$d/lib/x86_64-linux-gnu"
    {
        echo "name = \"$name\""
        echo "version = \"$ver\""
        echo "abi = 6"
        echo "iface = \"$name.diali\""
        echo "license = \"MIT\""
        if [ $# -gt 0 ]; then
            echo "[dependencies]"
            for dep in "$@"; do echo "$dep"; done
        fi
        echo "[src]"
        echo "[c]"
        echo "sources = \"c/$name.c\""
        echo "includes = \"c/include\""
        echo "mode = \"prebuilt\""
    } > "$d/dial.toml"
    echo "extern fn $fn(a: int, b: int): int;" > "$d/$name.diali"
    printf '#ifndef H\n#define H\nlong %s(long a, long b);\n#endif\n' "$fn" > "$d/c/include/$name.h"
    printf '#include "%s.h"\nlong %s(long a, long b) { return a + b; }\n' "$name" "$fn" > "$d/c/$name.c"
    "$DIALC" pack "$d" -o "$out" >/dev/null 2>&1
}

# ---- 1) pack 写 manifest.dependencies ----
echo "-- 1) pack 把 deps 写进 manifest.txt --"
mkdir -p "$TD/www"
make_pkg top p44_top_add 1.0.0 "$TD/www/top-1.0.0.dialpkg" 'json = "^2.0.0"'
if unzip -p "$TD/www/top-1.0.0.dialpkg" manifest.txt 2>/dev/null \
   | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('dependencies',{}).get('json')=='^2.0.0'" 2>/dev/null; then
    ok "manifest.txt 含 dependencies{json:^2.0.0}"
else
    bad "manifest.txt 未写入 dependencies"
fi
TOP_SHA="$(sha256sum "$TD/www/top-1.0.0.dialpkg" | cut -d' ' -f1)"

# ---- 2) install 默认报告（不 Fail）----
echo "-- 2) install 默认只提示缺失依赖，不 Fail-Fast --"
DEF_OUT="$($DIALC install "$TD/www/top-1.0.0.dialpkg" --json 2>/dev/null)"
DEF_RC=$?
echo "$DEF_OUT" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert d['ok'] is True, '默认模式顶层 ok 应为 true'
assert d['dependencies']['declared']==1
assert d['dependencies']['missing'][0]['name']=='json'
" && [ $DEF_RC -eq 0 ] && ok "默认模式报告缺失 json 且安装成功(ok=true)" \
   || bad "默认模式依赖报告异常 (rc=$DEF_RC)"

# 清掉刚装的 top，保证 --with-deps 从干净状态开始。
rm -rf "$THOME/.dial/packages/top"

# ---- 造依赖包 json + 本地索引，供 --with-deps ----
make_pkg json p44_json_add 2.1.0 "$TD/www/json-2.1.0.dialpkg"
JSON_SHA="$(sha256sum "$TD/www/json-2.1.0.dialpkg" | cut -d' ' -f1)"
cat > "$TD/www/index.json" <<EOF
{
  "schema": 3, "updated": "2026-11-20",
  "packages": [
    {"name":"json","version":"2.1.0","abi":6,"license":"MIT","desc":"JSON lib","targets":["x86_64-linux-gnu"],"url":"http://127.0.0.1:$PORT/json-2.1.0.dialpkg","sha256":"$JSON_SHA","deps":{}}
  ]
}
EOF
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$TD/www" >"$TD/http.log" 2>&1 &
HTTP_PID=$!
# 等待 HTTP 就绪（最多 ~4s），避免首次 curl 抢跑导致连接失败。
HTTP_UP=0
for _wait in $(seq 1 20); do
    if curl -sf "http://127.0.0.1:$PORT/index.json" >/dev/null 2>&1; then HTTP_UP=1; break; fi
    sleep 0.2
done
if [ "$HTTP_UP" -ne 1 ]; then
    bad "测试用 HTTP 服务未能启动"; cat "$TD/http.log"; exit 1
fi

# ---- 3) --with-deps 自动装齐 ----
echo "-- 3) install --with-deps 自动 resolve + 递归安装 --"
rm -rf "$THOME/.dial/packages/top" "$THOME/.dial/packages/json"
WD_OUT="$($DIALC install "$TD/www/top-1.0.0.dialpkg" --with-deps --index "http://127.0.0.1:$PORT/index.json" --json 2>/dev/null)"
WD_RC=$?
echo "$WD_OUT" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert d['ok'] is True, d
deps=d['dependencies']
assert deps['with_deps'] is True
assert any(x['name']=='json' for x in deps['installed']), deps
assert deps['missing']==[], deps
" && [ $WD_RC -eq 0 ] && ok "--with-deps 自动安装 json v2.1.0 成功" \
   || { bad "--with-deps 未装齐依赖 (rc=$WD_RC)"; echo "$WD_OUT" | head -20; }

# 确认 json 真落到缓存
[ -f "$THOME/.dial/packages/json/manifest.txt" ] && ok "json 已落盘到包缓存" \
   || bad "json 未落盘"

# ---- 4) uninstall 反依赖检查 ----
echo "-- 4) uninstall 反依赖检查（top 依赖 json → 禁止卸载 json）--"
UOUT="$($DIALC uninstall json --json 2>&1)"; URC=$?
echo "$UOUT" | python3 -c "
import sys
s=sys.stdin.read()
assert ('top' in s) and ('无法卸载' in s or 'depend' in s.lower() or 'ok' not in s), s
" 2>/dev/null
# uninstall 当前是文本错误到 stderr；用退出码 + 关键字判定更稳。
if [ $URC -ne 0 ] && echo "$UOUT" | grep -q "top"; then
    ok "卸载 json 被拒（top 依赖它）"
else
    bad "反依赖检查失效：json 被允许卸载 (rc=$URC, out=$UOUT)"
fi
# 卸载 top（无依赖者）应成功
$DIALC uninstall top >/dev/null 2>&1 && ok "卸载 top（无反依赖）成功" || bad "卸载 top 失败"
$DIALC uninstall json >/dev/null 2>&1 && ok "top 卸载后 json 可卸载" || bad "json 仍无法卸载"

# ---- 5) 并发安装锁 ----
echo "-- 5) 并发安装锁（伪造持锁 → 第二次安装被拒）--"
LOCK="$THOME/.dial/.install.lock"
mkdir -p "$THOME/.dial"
echo "999999:probe" > "$LOCK"
LOCK_OUT="$($DIALC install "$TD/www/top-1.0.0.dialpkg" --json 2>/dev/null)"; LOCK_RC=$?
if [ $LOCK_RC -ne 0 ] && echo "$LOCK_OUT" | grep -qi "锁\|lock\|另一个"; then
    ok "持锁时第二次安装被拒（Fail-Fast）"
else
    bad "并发锁未生效 (rc=$LOCK_RC)"
fi
# 陈旧锁（mtime 超过 30min）应被清理后可正常安装
touch -d '31 minutes ago' "$LOCK" 2>/dev/null || true
STALE_OUT="$($DIALC install "$TD/www/top-1.0.0.dialpkg" --json 2>/dev/null)"; STALE_RC=$?
[ $STALE_RC -eq 0 ] && ok "陈旧锁被自愈清理，安装恢复" || bad "陈旧锁未自愈 (rc=$STALE_RC)"

# ---- 6) --with-deps 索引不可用 Fail-Fast ----
echo "-- 6) --with-deps 索引不可用 → Fail-Fast --"
# 必须制造「依赖确实缺失 + 无索引缓存可回退」的状态：
#  - 若 json 已装则依赖已满足，根本不需要索引；
#  - registry 下载失败会回退固定路径 ~/.dial/cache/index.json（与 URL 无关），
#    故必须清掉缓存，否则回退的旧索引仍能让 resolve 成功。
rm -rf "$THOME/.dial/packages/top" "$THOME/.dial/packages/json" "$THOME/.dial/cache/index.json"
FOUT="$($DIALC install "$TD/www/top-1.0.0.dialpkg" --with-deps --index "http://localhost:1/nope.json" --json 2>/dev/null)"; FRC=$?
echo "$FOUT" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert d['ok'] is False, d
" 2>/dev/null && [ $FRC -ne 0 ] && ok "索引不可用时 --with-deps 拒装（ok=false）" \
   || bad "--with-deps 索引失败未 Fail-Fast (rc=$FRC)"

# ---- 6.5) outdated / upgrade（版本升级体验）----
echo "-- 6.5) outdated 报告过期 + upgrade 原子升级 --"
make_pkg demo p44_demo_add 1.0.0 "$TD/www/demo-1.0.0.dialpkg"
make_pkg demo p44_demo_add 2.0.0 "$TD/www/demo-2.0.0.dialpkg"
D1="$(sha256sum "$TD/www/demo-1.0.0.dialpkg" | cut -d' ' -f1)"
D2="$(sha256sum "$TD/www/demo-2.0.0.dialpkg" | cut -d' ' -f1)"
cat > "$TD/www/index2.json" <<EOF
{
  "schema": 3, "updated": "2026-11-20",
  "packages": [
    {"name":"demo","version":"1.0.0","abi":6,"license":"MIT","desc":"demo","targets":["x86_64-linux-gnu"],"url":"http://127.0.0.1:$PORT/demo-1.0.0.dialpkg","sha256":"$D1","deps":{}},
    {"name":"demo","version":"2.0.0","abi":6,"license":"MIT","desc":"demo","targets":["x86_64-linux-gnu"],"url":"http://127.0.0.1:$PORT/demo-2.0.0.dialpkg","sha256":"$D2","deps":{}}
  ]
}
EOF
rm -rf "$THOME/.dial/packages/demo"
$DIALC install "$TD/www/demo-1.0.0.dialpkg" >/dev/null 2>&1
OUTD="$($DIALC outdated --index "http://127.0.0.1:$PORT/index2.json" --json 2>/dev/null)"
echo "$OUTD" | python3 -c "
import sys,json
d=json.load(sys.stdin)
row=[x for x in d['outdated'] if x['name']=='demo']
assert len(row)==1 and row[0]['current']=='1.0.0' and row[0]['latest']=='2.0.0', d
" && ok "outdated 报告 demo v1.0.0→v2.0.0" || bad "outdated 报告异常: $OUTD"
UPG="$($DIALC upgrade demo --index "http://127.0.0.1:$PORT/index2.json" --json 2>/dev/null)"
echo "$UPG" | python3 -c "
import sys,json
d=json.load(sys.stdin)
assert d['ok'] is True and d['count']==1 and d['upgraded'][0]['to']=='2.0.0', d
" && ok "upgrade 把 demo 升到 v2.0.0" || bad "upgrade 异常: $UPG"
DEMO_VER="$(grep -o '"version": *"[^"]*"' "$THOME/.dial/packages/demo/manifest.txt" | head -1 | grep -o '2.0.0')"
[ "$DEMO_VER" = "2.0.0" ] && ok "升级后 manifest 版本为 v2.0.0" || bad "升级后版本未更新"

# ---- 7) semver 单测 ----
# 注意：cargo 依赖真实 HOME 下的 rustup，临时 HOME 会找不到工具链，故此处切回原 HOME。
echo "-- 7) semver 版本比较单测 --"
TUNIT="$(HOME="$OLD_HOME" cargo test --manifest-path /workspace/dialc/Cargo.toml registry::tests 2>&1)"
echo "$TUNIT" | grep -q "2 passed" && ok "search/info semver 单测通过" || bad "semver 单测未通过"

echo
if [ $fail -eq 0 ]; then
    echo "== AP2) P4.4 依赖闭环门禁全绿 ✅ =="
else
    echo "== AP2) P4.4 依赖闭环门禁存在失败 ❌ =="
fi
exit $fail
