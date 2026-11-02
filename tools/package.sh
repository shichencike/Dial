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
# 包名是**固定**的：dialc-v<短版本>.zip（不含补丁位），与官网首页的下载链接一致。
# 包内根目录固定为 Dial/ —— 与仓库名、README 的「克隆 → 进入（cd Dial）」一致，
# 不随本地 checkout 目录名而变（本地叫 dialc、CI 里叫 Dial 的差异就此消除）。
#
# 可选：INCLUDE_BIN=1 时，把当前构建好的编译器二进制一并放进包内 bin/，
#       并附 BUILD_INFO.txt 说明其运行环境要求（仍需系统装 libLLVM-18）。
#       CI 的发布流程用它把「编译产物」也一并放进压缩包。
#
# 用法：bash tools/package.sh
#       INCLUDE_BIN=1 bash tools/package.sh
#
set -eu

cd "$(dirname "$0")/.." || exit 1          # 进入仓库根
REPO=$(basename "$PWD")

# ---- 1) 版本号：从 Cargo.toml 读取，三段式 -> 短式（0.7.0 -> 0.7；0.61.0 -> 0.61）----
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
MAJOR=$(echo "$VERSION" | cut -d. -f1)
MINOR=$(echo "$VERSION" | cut -d. -f2)
PATCH=$(echo "$VERSION" | cut -d. -f3)
if [ "$PATCH" = "0" ]; then SHORT="${MAJOR}.${MINOR}"; else SHORT="${VERSION}"; fi

PKG_ROOT="Dial"                          # 包内根目录（固定）
OUT="$(cd .. && pwd)/dialc-v${SHORT}.zip" # 绝对路径（下面会 cd，���对路径会失效）
echo "== 打包 dialc v${VERSION} -> ${OUT} =="

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

# ---- 4) 收集到暂存目录（根目录名固定为 Dial/）----
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DEST="${STAGE}/${PKG_ROOT}"
mkdir -p "$DEST"

# 源码与文档：整个仓库（排除构建产物与一次性文件）
tar -cf - \
    --exclude=./target \
    --exclude=./.dialc_build \
    --exclude='*.o' \
    --exclude='*.zip' \
    --exclude='./TEST_RUN_*.log' \
    --exclude='./REPORT_*.md' \
    --exclude=./__pycache__ \
    --exclude='./tools/__pycache__' \
    . | (cd "$DEST" && tar -xf - --exclude='./.git/objects/pack/tmp_*')

# ---- 5) 可选：把 CI 构建好的二进制放进包内 ----
if [ "${INCLUDE_BIN:-0}" = "1" ]; then
    if [ ! -x target/release/dialc ]; then
        echo "   ❌ INCLUDE_BIN=1 但 target/release/dialc 不存在，拒绝打包"
        exit 1
    fi
    mkdir -p "$DEST/bin"
    cp target/release/dialc "$DEST/bin/dialc"
    {
        if [ -n "${GITHUB_SHA:-}" ]; then
            BUILD_BY="由 GitHub Actions 构建"
        else
            BUILD_BY="本地构建"
        fi
        echo "dialc v${VERSION}（${BUILD_BY}）"
        echo
        echo "该二进制编译于 Linux x86_64，链接 libLLVM-18（运行时需要它）。"
        echo "使用前请设置："
        echo "    export LD_LIBRARY_PATH=/usr/lib/llvm-18/lib:\$LD_LIBRARY_PATH"
        echo "或自行 clone 源码后用 cargo build --release 重新构建。"
        echo
        echo "版本:   ${VERSION}"
        echo "构建提交: ${GITHUB_SHA:-（本地构建，无提交哈希）}"
        echo "构建时间: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    } > "$DEST/bin/BUILD_INFO.txt"
    echo "   ✅ 已把编译好的二进制放入包内 bin/dialc"
fi

# ---- 6) 打包 ----
rm -f "$OUT"
(cd "$STAGE" && zip -r -q "$OUT" "$PKG_ROOT")

# ---- 7) 自检：确认包内确实含有许可文件与 .git，且没有构建产物 ----
echo
echo "-- 自检包内容 --"
FAIL=0
for f in LICENSE LICENSE-GPLv3 LICENSE-MIT LICENSE-APACHE; do
    if unzip -l "$OUT" | grep -q "${PKG_ROOT}/${f}$"; then
        echo "   ✅ 包内含 ${f}"
    else
        echo "   ❌ 包内缺少 ${f}"
        FAIL=1
    fi
done
if unzip -l "$OUT" | grep -q "${PKG_ROOT}/.git/"; then
    echo "   ✅ 包内含 .git（提交历史完整）"
else
    echo "   ❌ 包内缺少 .git"
    FAIL=1
fi
if unzip -l "$OUT" | grep -q "${PKG_ROOT}/target/"; then
    echo "   ❌ 包内混入了 target/ 构建产物"
    FAIL=1
else
    echo "   ✅ 包内无 target/ 构建产物"
fi
if unzip -l "$OUT" | grep -qE "${PKG_ROOT}/.*\.o$"; then
    echo "   ❌ 包内混入了 .o 目标文件"
    FAIL=1
else
    echo "   ✅ 包内无 .o 目标文件"
fi
if unzip -l "$OUT" | grep -q "${PKG_ROOT}/REPORT_"; then
    echo "   ❌ 包内混入了已废弃的一次性报告"
    FAIL=1
else
    echo "   ✅ 包内无已废弃的一次性报告"
fi

echo
if [ "$FAIL" -eq 0 ]; then
    echo "== 打包完成 ✅ : ${OUT} ($(du -h "$OUT" | cut -f1)) =="
else
    echo "== 打包失败 ❌ =="
    exit 1
fi
