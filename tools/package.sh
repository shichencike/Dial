#!/usr/bin/env bash
#
# 打包 dialc 发布压缩包。
#
# 三条硬性要求（缺一不可，任一不满足立即失败，绝不产出残缺包）：
#   1) 【用户明确要求】必须包含三份开源协议原文：
#        LICENSE（三重许可说明文档）
#        LICENSE-GPLv3、LICENSE-MIT、LICENSE-APACHE（三份协议全文）
#   2) 必须包含 .git 目录 —— 否则提交历史无效，压缩包不能算一次完整交付。
#   3) 排除一切构建产物：target/、.dialc_build/、*.o、本地测试日志、旧压缩包。
#
# 用法：bash tools/package.sh          （版本号自动从 Cargo.toml 读取）
#
set -eu

cd "$(dirname "$0")/.." || exit 1          # 进入仓库根（dialc/）
REPO=$(basename "$PWD")

# ---- 1) 版本号：从 Cargo.toml 读取，三段式 -> 短式（0.7.0 -> 0.7；0.61.0 -> 0.61）----
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
MAJOR=$(echo "$VERSION" | cut -d. -f1)
MINOR=$(echo "$VERSION" | cut -d. -f2)
PATCH=$(echo "$VERSION" | cut -d. -f3)
if [ "$PATCH" = "0" ]; then SHORT="${MAJOR}.${MINOR}"; else SHORT="${VERSION}"; fi

OUT="$(cd .. && pwd)/${REPO}-v${SHORT}.zip"   # 绝对路径：下面打包时会 cd ..，相对路径会失效
echo "== 打包 ${REPO} v${VERSION} -> ${OUT} =="

# ---- 2) 硬性校验：四份许可文件必须存在 ----
echo
echo "-- 校验开源许可文件 --"
MISSING=0
for f in LICENSE LICENSE-GPLv3 LICENSE-MIT LICENSE-APACHE; do
    if [ -f "$f" ]; then
        printf '   ✅ %-18s %s 字节\n' "$f" "$(wc -c <"$f" | tr -d ' ')"
    else
        printf '   ❌ %-18s 缺失！\n' "$f"
        MISSING=1
    fi
done
if [ "$MISSING" -ne 0 ]; then
    echo "   -> 许可文件不全，拒绝打包（三份协议 + 一份说明文档必须齐全）"
    exit 1
fi

# ---- 3) 硬性校验：.git 必须存在 ----
if [ ! -d .git ]; then
    echo "   ❌ 缺少 .git 目录，拒绝打包"
    exit 1
fi
echo "   ✅ .git 目录存在"

# ---- 4) 打包（含 .git，排除构建产物）----
rm -f "$OUT"
cd ..
zip -r -q "$OUT" "$REPO" \
    -x "${REPO}/target/*" \
    -x "${REPO}/.dialc_build/*" \
    -x "*.o" \
    -x "*.zip" \
    -x "${REPO}/TEST_RUN_*.log"
cd "$REPO"

# ---- 5) 自检：确认包内确实含有许可文件与 .git ----
echo
echo "-- 自检包内容 --"
FAIL=0
for f in LICENSE LICENSE-GPLv3 LICENSE-MIT LICENSE-APACHE; do
    if unzip -l "$OUT" | grep -q "${REPO}/${f}$"; then
        echo "   ✅ 包内含 ${f}"
    else
        echo "   ❌ 包内缺少 ${f}"
        FAIL=1
    fi
done
if unzip -l "$OUT" | grep -q "${REPO}/.git/"; then
    echo "   ✅ 包内含 .git（提交历史完整）"
else
    echo "   ❌ 包内缺少 .git"
    FAIL=1
fi
if unzip -l "$OUT" | grep -q "${REPO}/target/"; then
    echo "   ❌ 包内混入了 target/ 构建产物"
    FAIL=1
else
    echo "   ✅ 包内无 target/ 构建产物"
fi

echo
if [ "$FAIL" -eq 0 ]; then
    echo "== 打包完成 ✅ : ${OUT} ($(du -h "$OUT" | cut -f1)) =="
else
    echo "== 打包失败 ❌ =="
    exit 1
fi
