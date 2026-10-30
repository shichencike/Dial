#!/usr/bin/env bash
#
# 构建一个**示例包** demo.dialpkg（用于自测 import / .dialpkg 这条链路）。
#
# 之所以要这个脚本：包分发是端到端能力，「包从哪来」必须能被机器重复产出，
# 否则测试就只能依赖某个手工制作、无法重建的二进制。
#
# 用法：bash tools/make_sample_pkg.sh [输出目录]      （默认 /tmp/dial-sample-pkg）
#
set -eu

OUT="${1:-/tmp/dial-sample-pkg}"
rm -rf "$OUT"
mkdir -p "$OUT/lib"
cd "$OUT"

# ---- 1) C 源码：一个最小的「数据库句柄」风格包 ----
# 刻意覆盖三种签名形态：
#   int f(int, int)     纯数值
#   int f(void)         无参
#   H   f(int)          返回不透明句柄
#   int f(H)            以不透明句柄为参数
#   void f(H)           释放（void —— 语句位置调用）
cat > demo.c <<'EOF'
#include <stdlib.h>
typedef struct { int v; } Demo;

int  demo_add(int a, int b)      { return a + b; }
int  demo_version(void)          { return 42; }
void *demo_new(int v)            { Demo *d = (Demo *)malloc(sizeof(Demo)); d->v = v; return d; }
int  demo_get(void *h)           { return ((Demo *)h)->v; }
void demo_free(void *h)          { free(h); }
EOF

# ---- 2) 编译为静态库（目标 triple 与 dialc 的 host_triple() 保持一致）----
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64)  TRIPLE="x86_64-linux-gnu" ;;
    aarch64) TRIPLE="aarch64-linux-gnu" ;;
    *)       echo "不支持的架构: $ARCH" >&2; exit 1 ;;
esac

if command -v zig >/dev/null 2>&1; then
    zig cc -O2 -c demo.c -o demo.o
elif command -v clang >/dev/null 2>&1; then
    clang -O2 -c demo.c -o demo.o
else
    cc -O2 -c demo.c -o demo.o
fi
ar rcs "lib/${TRIPLE}.a" demo.o

# ---- 3) 接口签名 .diali（纯文本，平台无关）----
cat > demo.diali <<'EOF'
// demo.diali —— 接口签名文件（纯文本，无实现，平台无关）
// dialc 用它做类型检查；LSP 用它做补全与悬停提示。

/// 不透明句柄：外部代码只拿到指针，永远不知道内部布局（见 §14.1）
type DemoHandle;

/// 两数相加
extern fn demo_add(a: int, b: int): int;

/// 返回包的版本号（无参数）
extern fn demo_version(): int;

/// 创建一个句柄；失败返回空句柄
extern fn demo_new(v: int): DemoHandle;

/// 从句柄取值
extern fn demo_get(h: DemoHandle): int;

/// 释放句柄；空句柄上安全无操作
extern fn demo_free(h: DemoHandle): void;
EOF

# ---- 4) manifest.txt（JSON）+ 逐文件 SHA256 ----
python3 - "$TRIPLE" <<'PY'
import hashlib, json, sys
triple = sys.argv[1]
lib_rel = f"lib/{triple}.a"
def sha(p):
    return hashlib.sha256(open(p, "rb").read()).hexdigest()
manifest = {
    "name": "demo",
    "version": "1.0.0",
    "abi": 1,
    "iface": "demo.diali",
    "license": "MIT OR Apache-2.0",
    "targets": {triple: {"lib": lib_rel}},
    "sha256": {"demo.diali": sha("demo.diali"), lib_rel: sha(lib_rel)},
}
with open("manifest.txt", "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2, ensure_ascii=False)
PY

# ---- 5) 打包为 .dialpkg（zip 容器，专属扩展名）----
rm -f demo.dialpkg
python3 - "$TRIPLE" <<'PY'
import sys, zipfile
triple = sys.argv[1]
with zipfile.ZipFile("demo.dialpkg", "w", zipfile.ZIP_DEFLATED) as z:
    z.write("manifest.txt")
    z.write("demo.diali")
    z.write(f"lib/{triple}.a")
PY

echo "✅ 已生成示例包: $OUT/demo.dialpkg（目标 $TRIPLE）"
