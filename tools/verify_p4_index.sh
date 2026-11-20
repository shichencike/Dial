#!/usr/bin/env bash
# P4.1 门禁：静态索引与搜索验证（2026-11-20 包管理路线图）。
#
# 覆盖路线图 §4 P4.1 / §1 维度③的 search 部分：
#   1. search <词>：子串匹配包名/描述，多版本去重取最新；
#   2. info <包名>：元数据 + 版本列表 + ABI + 许可 + 平台 + 安装命令；
#   3. --json：search/info 输出可被 python json 解析，字段稳定；
#   4. 索引钉 SHA256：正确值通过、错误值 Fail-Fast（非零退出，且不回退缓存）；
#   5. --offline：只读缓存命中，缓存缺失 Fail-Fast；
#   6. schema 错误 Fail-Fast；
#   7. 网络失败回退缓存（来源标注准确）；
#   8. completion/help 含 search/info。
#
# 铁律：索引用 localhost HTTP（复用 download_to 的 http 白名单），不依赖外网；
# 索引缓存落到临时 HOME，避免污染真实 ~/.dial；缺 curl/python3 时整段 skip。
#
# 用法：./tools/verify_p4_index.sh [编译器路径]   # 默认 ./target/release/dialc
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
if ! command -v curl >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1; then
    skip "缺 curl/python3，跳过 P4.1 索引门禁"
    exit 0
fi

TD="$(mktemp -d /tmp/p4index_gate_XXXX)"
THOME="$TD/home"
mkdir -p "$THOME"
# 用独立 HOME 隔离索引缓存（~/.dial/cache/index.json）
OLD_HOME="${HOME:-}"
export HOME="$THOME"

cleanup() {
    [ -n "${HTTP_PID:-}" ] && kill "$HTTP_PID" >/dev/null 2>&1 || true
    export HOME="$OLD_HOME"
    rm -rf "$TD"
}
trap cleanup EXIT

# 造本地静态索引：http 包有两个版本（测去重 + 版本列表），json 包一个版本。
cat > "$TD/index.json" <<'EOF'
{
  "schema": 1,
  "updated": "2026-11-20",
  "packages": [
    {
      "name": "http", "version": "1.2.0", "abi": 6,
      "license": "MIT", "desc": "HTTP client with TLS",
      "targets": ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"],
      "url": "http://localhost:PORT/http-1.2.0.dialpkg",
      "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    },
    {
      "name": "http", "version": "1.1.0", "abi": 6,
      "license": "MIT", "desc": "HTTP client with TLS",
      "targets": ["x86_64-unknown-linux-gnu"],
      "url": "http://localhost:PORT/http-1.1.0.dialpkg",
      "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    },
    {
      "name": "json", "version": "2.0.0", "abi": 6,
      "license": "Apache-2.0", "desc": "JSON parser and serializer",
      "targets": ["x86_64-unknown-linux-gnu"],
      "url": "http://localhost:PORT/json-2.0.0.dialpkg", "sha256": ""
    }
  ]
}
EOF
cat > "$TD/bad-schema.json" <<'EOF'
{"schema": 99, "packages": []}
EOF

# 选一个高位随机端口，替换索引里的 PORT 占位符。
PORT=$(( 20000 + RANDOM % 20000 ))
sed -i "s/PORT/$PORT/g" "$TD/index.json"
IDXSHA="$(sha256sum "$TD/index.json" | cut -d' ' -f1)"
IDXURL="http://localhost:$PORT/index.json"

# 起本地静态 HTTP 服务。
# exec 让 python3 替换子 shell，使 $! 是 python3 真 PID（cleanup 能真正回收，避免孤儿占端口）
( cd "$TD" && exec python3 -m http.server "$PORT" >/dev/null 2>&1 ) &
HTTP_PID=$!
# 等服务起来（最多 ~2s，不用 sleep 循环：直接探测端口）。
for _i in 1 2 3 4 5 6 7 8 9 10; do
    curl -sf "http://localhost:$PORT/index.json" -o /dev/null 2>/dev/null && break
    sleep 0.2
done

echo "== P4.1 静态索引与搜索门禁 =="

# 1. search 子串匹配 + 多版本去重
out="$("$DIALC" search http --index "$IDXURL" 2>&1)"
if echo "$out" | grep -q "1 个结果" && echo "$out" | grep -q "v1.2.0" && ! echo "$out" | grep -q "v1.1.0"; then
    ok "search 子串匹配 + 多版本去重取最新（v1.2.0）"
else
    bad "search 去重失败: $out"
fi
out="$("$DIALC" search TLS --index "$IDXURL" 2>&1)"
if echo "$out" | grep -q "http" && echo "$out" | grep -q "1 个结果"; then
    ok "search 按描述子串匹配（TLS → http）"
else
    bad "search 描述匹配失败: $out"
fi

# 2. info 元数据 + 版本列表
out="$("$DIALC" info http --index "$IDXURL" 2>&1)"
if echo "$out" | grep -q "2 个版本" && echo "$out" | grep -q "v1.2.0, v1.1.0" && echo "$out" | grep -q "MIT" && echo "$out" | grep -q "ABI: 6"; then
    ok "info 元数据/版本列表/ABI/许可/平台"
else
    bad "info 输出失败: $out"
fi

# 3. --json 合法性 + 稳定字段
if "$DIALC" search http --index "$IDXURL" --json 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
assert d['ok'] is True and d['count']==1 and d['results'][0]['name']=='http'
assert 'sha256' in d['results'][0] and 'targets' in d['results'][0]
" 2>/dev/null; then
    ok "search --json 合法且字段稳定"
else
    bad "search --json 失败"
fi
if "$DIALC" info http --index "$IDXURL" --json 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
assert d['ok'] is True and d['count']==2
assert [v['version'] for v in d['versions']]==['1.2.0','1.1.0']
" 2>/dev/null; then
    ok "info --json 合法且版本列表有序"
else
    bad "info --json 失败"
fi

# 4. 索引钉 SHA256：正确通过
if "$DIALC" search json --index "$IDXURL" --index-sha256 "$IDXSHA" 2>&1 | grep -q "json"; then
    ok "索引钉 SHA256 正确值通过"
else
    bad "索引钉 SHA256 正确值未通过"
fi
# 错误值必须 Fail-Fast（非零退出），且不回退缓存
"$DIALC" search json --index "$IDXURL" --index-sha256 "0000000000000000000000000000000000000000000000000000000000000000" >/dev/null 2>&1
RC=$?
if [ "$RC" -ne 0 ]; then
    ok "索引 SHA256 错误 Fail-Fast（退出码 $RC，不回退缓存）"
else
    bad "索引 SHA256 错误竟然成功（回退了缓存？）"
fi

# 5. --offline 缓存命中（前面 online 已落缓存到临时 HOME）
if "$DIALC" search json --offline 2>&1 | grep -q "本地缓存"; then
    ok "--offline 只读缓存命中"
else
    bad "--offline 缓存命中失败"
fi
# offline 且缓存不存在必须 Fail-Fast
rm -f "$THOME/.dial/cache/index.json"
"$DIALC" search json --offline >/dev/null 2>&1
RC=$?
if [ "$RC" -ne 0 ]; then
    ok "--offline 缓存缺失 Fail-Fast（退出码 $RC）"
else
    bad "--offline 缓存缺失未报错"
fi
# 重建缓存供后续用
"$DIALC" search json --index "$IDXURL" >/dev/null 2>&1

# 6. schema 错误 Fail-Fast
"$DIALC" search x --index "http://localhost:$PORT/bad-schema.json" >/dev/null 2>&1
RC=$?
if [ "$RC" -ne 0 ]; then
    ok "schema 错误 Fail-Fast（退出码 $RC）"
else
    bad "schema 错误未报错"
fi

# 7. 网络失败回退缓存 + 来源标注准确
out="$("$DIALC" search json --index "http://localhost:$((PORT+1))/index.json" 2>&1)"
if echo "$out" | grep -q "回退本地缓存" && echo "$out" | grep -q "下载失败回退"; then
    ok "网络失败回退缓存且来源标注准确"
else
    bad "网络失败回退/来源标注失败: $out"
fi

# 8. completion/help 含 search/info
if "$DIALC" completion bash 2>/dev/null | grep -q "search" && "$DIALC" help 2>&1 | grep -q "info"; then
    ok "completion/help 含 search/info"
else
    bad "completion/help 缺 search/info"
fi

echo "== P4.1 门禁结束 =="
exit $fail
