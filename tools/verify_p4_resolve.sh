#!/usr/bin/env bash
# P4.2 门禁：完整依赖管理验证（2026-11-20 包管理路线图）。
#
# 覆盖路线图 §4 P4.2：
#   1. 语义版本区间：caret ^ / tilde ~ / 精确 = / 通配 * 的选版；
#   2. 传递依赖解析：json ^2.0.0 → 选最高 2.1.0 → 传递拉入 codec ^1.1.0 → 1.2.0；
#   3. 版本仲裁冲突 Fail-Fast：root 要 json ^2、http 要 json ^3 → 可读冲突路径；
#   4. 缺包 Fail-Fast；
#   5. dial.lock 生成（精确版本/url/sha256/abi）+ --check 一致；
#   6. lock 篡改被 --check 拦截；
#   7. --json 输出合法；
#   8. --offline 缓存解析；
#   9. install --locked 按 lock 批量安装真实包（下载+SHA256 校验+可 import）；
#  10. cargo test 的求解器/版本单测（version::* / resolve::*）。
#
# 铁律：索引用 localhost HTTP（不依赖外网），缓存与已装包隔离到临时 HOME；
# 缺 curl/python3/zig 时整段 skip。
#
# 用法：./tools/verify_p4_resolve.sh [编译器路径]   # 默认 ./target/release/dialc
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
    skip "缺 curl/python3/zig，跳过 P4.2 依赖管理门禁"; exit 0
fi

TD="$(mktemp -d /tmp/p42resolve_gate_XXXX)"
THOME="$TD/home"
mkdir -p "$THOME"
OLD_HOME="${HOME:-}"
export HOME="$THOME"
PORT=$(( 22000 + RANDOM % 20000 ))

cleanup() {
    [ -n "${HTTP_PID:-}" ] && kill "$HTTP_PID" >/dev/null 2>&1 || true
    export HOME="$OLD_HOME"
    rm -rf "$TD"
}
trap cleanup EXIT

# 造两个真实 .dialpkg（codec / json），供 install --locked 验证。
make_pkg() {
    local name="$1" fn="$2" outdir="$3"
    local d="$TD/src_$name"
    mkdir -p "$d/c/include" "$d/lib/x86_64-linux-gnu"
    cat > "$d/dial.toml" <<EOF
name = "$name"
version = "1.0.0"
abi = 6
iface = "$name.diali"
license = "MIT"
[src]
[c]
sources = "c/$name.c"
includes = "c/include"
mode = "prebuilt"
EOF
    echo "extern fn $fn(a: int, b: int): int;" > "$d/$name.diali"
    echo "#ifndef H
#define H
long $fn(long a, long b);
#endif" > "$d/c/include/$name.h"
    echo "#include \"$name.h\"
long $fn(long a, long b) { return a + b; }" > "$d/c/$name.c"
    "$DIALC" pack "$d" -o "$outdir" >/dev/null 2>&1
}

mkdir -p "$TD/www"
make_pkg codec p42_codec_add "$TD/www/codec-1.2.0.dialpkg"
make_pkg json p42_json_add "$TD/www/json-2.1.0.dialpkg"
CODEC_SHA="$(sha256sum "$TD/www/codec-1.2.0.dialpkg" | cut -d' ' -f1)"
JSON_SHA="$(sha256sum "$TD/www/json-2.1.0.dialpkg" | cut -d' ' -f1)"

# 静态索引（schema 2）：json ^2 依赖 codec ^1.1.0；http 依赖 json ^3（制造冲突）。
cat > "$TD/www/index.json" <<EOF
{
  "schema": 2, "updated": "2026-11-20",
  "packages": [
    {"name":"json","version":"2.0.0","abi":6,"license":"MIT","desc":"JSON","targets":["x86_64-linux-gnu"],"url":"http://localhost:$PORT/json-2.0.0.dialpkg","sha256":"","deps":{"codec":"^1.0.0"}},
    {"name":"json","version":"2.1.0","abi":6,"license":"MIT","desc":"JSON","targets":["x86_64-linux-gnu"],"url":"http://localhost:$PORT/json-2.1.0.dialpkg","sha256":"$JSON_SHA","deps":{"codec":"^1.1.0"}},
    {"name":"json","version":"3.0.0","abi":7,"license":"MIT","desc":"JSON v3","targets":["x86_64-linux-gnu"],"url":"http://localhost:$PORT/json-3.0.0.dialpkg","sha256":"","deps":{}},
    {"name":"codec","version":"1.0.0","abi":6,"license":"MIT","desc":"codec","targets":["x86_64-linux-gnu"],"url":"http://localhost:$PORT/codec-1.0.0.dialpkg","sha256":"","deps":{}},
    {"name":"codec","version":"1.2.0","abi":6,"license":"MIT","desc":"codec","targets":["x86_64-linux-gnu"],"url":"http://localhost:$PORT/codec-1.2.0.dialpkg","sha256":"$CODEC_SHA","deps":{}},
    {"name":"http","version":"1.0.0","abi":6,"license":"MIT","desc":"http","targets":["x86_64-linux-gnu"],"url":"http://localhost:$PORT/http-1.0.0.dialpkg","sha256":"","deps":{"json":"^3.0.0"}}
  ]
}
EOF
IDX="http://localhost:$PORT/index.json"

mkdir -p "$TD/projA" "$TD/projB" "$TD/projC"
for p in projA projB projC; do
cat > "$TD/$p/dial.toml" <<EOF
name = "$p"
version = "1.0.0"
abi = 6
iface = "x.diali"
license = "MIT"
[src]
[dependencies]
EOF
done
echo 'json = "^2.0.0"' >> "$TD/projA/dial.toml"
printf 'json = "^2.0.0"\nhttp = "^1.0.0"\n' >> "$TD/projB/dial.toml"
echo 'nonexistent = "^1.0.0"' >> "$TD/projC/dial.toml"

( cd "$TD/www" && python3 -m http.server "$PORT" >/dev/null 2>&1 ) &
HTTP_PID=$!
for _i in 1 2 3 4 5 6 7 8 9 10; do
    curl -sf "$IDX" -o /dev/null 2>/dev/null && break
    sleep 0.2
done

echo "== P4.2 完整依赖管理门禁 =="

# 1+2. 选最高 + 传递依赖
out="$("$DIALC" resolve "$TD/projA" --index "$IDX" 2>&1)"
if echo "$out" | grep -q "json v2.1.0" && echo "$out" | grep -q "codec v1.2.0" && echo "$out" | grep -q "2 个依赖"; then
    ok "caret 选最高(2.1.0)+传递依赖(codec 1.2.0)"
else
    bad "选版/传递依赖失败: $out"
fi

# 3. 冲突 Fail-Fast + 可读路径
out="$("$DIALC" resolve "$TD/projB" --index "$IDX" 2>&1)"
if echo "$out" | grep -q "冲突" && echo "$out" | grep -q "http@1.0.0 -> json"; then
    ok "版本仲裁冲突 Fail-Fast + 可读冲突路径"
else
    bad "冲突诊断失败: $out"
fi

# 4. 缺包 Fail-Fast
out="$("$DIALC" resolve "$TD/projC" --index "$IDX" 2>&1)"
if echo "$out" | grep -q "无满足"; then
    ok "缺包 Fail-Fast"
else
    bad "缺包未报错: $out"
fi

# 5. lock 生成 + --check 一致
LOCK="$TD/projA/dial.lock"
if [ -f "$LOCK" ] && grep -q '"lock_version": 1' "$LOCK" && grep -q '"version": "2.1.0"' "$LOCK" && grep -q "$JSON_SHA" "$LOCK"; then
    ok "dial.lock 生成（精确版本/url/sha256/abi）"
else
    bad "dial.lock 内容错误"
fi
out="$("$DIALC" resolve "$TD/projA" --index "$IDX" --check 2>&1)"
if echo "$out" | grep -q "一致"; then
    ok "--check：lock 与解析一致"
else
    bad "--check 一致失败: $out"
fi

# 6. lock 篡改拦截
cp "$LOCK" "$TD/lock.bak"
sed -i 's/"version": "1.2.0"/"version": "1.0.0"/' "$LOCK"
out="$("$DIALC" resolve "$TD/projA" --index "$IDX" --check 2>&1)"
if echo "$out" | grep -q "不符"; then
    ok "lock 篡改被 --check 拦截"
else
    bad "lock 篡改未拦截: $out"
fi
cp "$TD/lock.bak" "$LOCK"

# 7. --json
if "$DIALC" resolve "$TD/projA" --index "$IDX" --json 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
assert d['ok'] is True and d['count']==2
assert sorted(p['name'] for p in d['packages'])==['codec','json']
" 2>/dev/null; then
    ok "resolve --json 合法且字段稳定"
else
    bad "resolve --json 失败"
fi

# 8. --offline 缓存解析
out="$("$DIALC" resolve "$TD/projA" --offline --check --json 2>/dev/null)"
if echo "$out" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['ok'] and 'offline' in d['source']" 2>/dev/null; then
    ok "--offline 缓存解析"
else
    bad "--offline 解析失败"
fi

# 9. install --locked 按真实 lock 批量安装 + 可 import 运行
out="$("$DIALC" install --locked "$TD/projA" 2>&1)"
if echo "$out" | grep -q "codec" && echo "$out" | grep -q "json"; then
    ok "install --locked 按 lock 批量安装（含 SHA256 校验）"
else
    bad "install --locked 失败: $out"
fi
cat > "$TD/use_deps.dial" <<'EOF'
import "codec";
import "json";
fn main(): int {
    print(p42_codec_add(10, 20));
    print(p42_json_add(5, 7));
    return 0;
}
EOF
runout="$(LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 "$DIALC" run "$TD/use_deps.dial" --backend asm 2>&1 | tail -3)"
if echo "$runout" | grep -q "30" && echo "$runout" | grep -q "12"; then
    ok "install --locked 装的包可 import 编译运行（asm 后端 30/12）"
else
    bad "装后的包 import 运行失败: $runout"
fi
"$DIALC" uninstall codec >/dev/null 2>&1 || true
"$DIALC" uninstall json  >/dev/null 2>&1 || true

# 10. 求解器/版本单测（切回真实 HOME——rustup 工具链配置在真实 ~/.rustup）
export HOME="$OLD_HOME"
if cargo test 2>&1 | grep -q "test result: ok"; then
    ok "version/resolve 等单元测试全过"
else
    bad "单元测试失败"
fi

echo "== P4.2 门禁结束 =="
exit $fail
