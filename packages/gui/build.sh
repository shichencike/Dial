#!/usr/bin/env bash
#
# 构建 gui.dialpkg 的三平台静态库（**三份都是真实现**）。
#
# 产物：packages/gui/lib/<triple>/libgui.a
#   - dial_gui_core.o    FFI 包装层：句柄生命周期 / 错误槽 / 事件槽 / 平台分发
#   - dial_gui_raster.o  自研 2D 光栅化器 + 内置 8x8 点阵 + stb_truetype 实现
#   - dial_gui_widget.o  控件框架层：Widget 树 / 布局 / 命中 / packed 事件块（纯 C，三平台共用）
#   - dial_gui_theme.o   主题层：14 个可覆写颜色角色 / 默认值 / reset（纯 C，三平台共用）
#   - dial_gui_look.o    成品控件外观层：主题表驱动四态 / 八种控件绘制 / 纯值事件推进（纯 C，三平台共用）
#   - dial_gui_input.o   输入层：TextBox 单行编辑 / 焦点链 / 统一键码推进（纯 C，三平台共用）
#   - dial_gui_signal.o  第 6 层：信号槽 / 可订阅纯值信号块 + 每树环形队列（纯 C，三平台共用）
#   - dial_gui_menu.o    第 7 层：菜单 / 对话框窗口级覆盖层（纯 C，三平台共用）
#   - dial_gui_x11.o     Linux / Termux 后端：X11 + XShm + XRender（仅 POSIX 目标）
#   - dial_gui_win32.o   Windows 后端：GDI DIBSection + 消息循环（仅 windows 目标）
#
# 平台库一律**运行时动态装载**（POSIX dlopen / Win32 LoadLibraryA）：
#   - .a 自包含，Dial 侧不需要写 link，链接期也不需要 -lX11 / -lgdi32；
#   - 交叉编译三平台时不需要各平台的 GUI SDK 头文件（函数指针类型自己声明）；
#   - 缺库在 dial_gui_app_new() 里返回明确错误码，而不是让可执行文件加载失败。
#
# 全部用 zig cc 交叉编译，保证三个平台的机器码出自同一套工具链。
# stb_truetype.h v1.26 vendor 在 third_party/stb/，基线由
# patches/stb-1.26.SHA256SUMS 锁定（构建前后各校验一次，third_party/ 零改动）。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKG="$ROOT/packages/gui"
TP="$ROOT/third_party"
OUT="$PKG/lib"
SUMFILE="$ROOT/patches/stb-1.26.SHA256SUMS"

verify_upstream() {
    ( cd "$TP" && sha256sum -c "$SUMFILE" ) >/dev/null
}
verify_upstream
echo "==> 上游基线完整性校验通过（$(wc -l < "$SUMFILE") 个文件）"

# 目标：**zig cc 的目标名**与**manifest 里的 triple 键**不是同一套命名，
# 必须成对列出——前者给 zig cc，后者要逐字等于编译器 package::host_triple()
# 的取值，否则用户装上包后编译器按 host_triple() 选不到机器码。
# 写成 "zig目标:manifest键:平台族" 的三元组。
TARGETS=(
    "x86_64-linux-gnu:x86_64-linux-gnu:posix"
    "aarch64-linux-musl:aarch64-unknown-linux-musl:posix"
    "x86_64-windows-gnu:x86_64-pc-windows-gnu:win32"
)

# 可复现性三件套（与 http 包同口径）：
#   -ffile-prefix-map：规范化 __FILE__ 里的仓库绝对路径；
#   -g0：关掉 zig cc 在 COFF 目标下默认生成的 CodeView 调试段（含随机 obj 名）；
#   SOURCE_DATE_EPOCH：固定 COFF 文件头的 TimeDateStamp。
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}"
CFLAGS_COMMON=(-O2 -fPIC -Wall -Wextra -g0)
FP_PREFIX=(-ffile-prefix-map="$ROOT=dialc" -ffile-prefix-map="$TP=third_party")

for entry in "${TARGETS[@]}"; do
    IFS=: read -r t triple family <<< "$entry"
    echo "==> 构建 $t（manifest 键 $triple，后端族 $family）"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT

    INCS=(-I "$PKG/src" -I "$ROOT/stdlib" -I "$ROOT/runtime" -I "$TP/stb")
    SRCS=("$PKG/src/dial_gui_core.c" "$PKG/src/dial_gui_raster.c" \
          "$PKG/src/dial_gui_widget.c" "$PKG/src/dial_gui_theme.c" \
          "$PKG/src/dial_gui_look.c" "$PKG/src/dial_gui_input.c" \
          "$PKG/src/dial_gui_signal.c" "$PKG/src/dial_gui_menu.c")
    if [ "$family" = "win32" ]; then
        SRCS+=("$PKG/src/dial_gui_win32.c")
    else
        SRCS+=("$PKG/src/dial_gui_x11.c")
    fi

    for f in "${SRCS[@]}"; do
        b="$(basename "$f" .c)"
        zig cc -target "$t" -c "$f" \
            "${INCS[@]}" "${CFLAGS_COMMON[@]}" "${FP_PREFIX[@]}" \
            -o "$work/$b.o"
    done

    mkdir -p "$OUT/$triple"
    rm -f "$OUT/$triple/libgui.a"
    zig ar rcs "$OUT/$triple/libgui.a" "$work"/*.o
    rm -rf "$work"
    trap - EXIT
    echo "    -> $OUT/$triple/libgui.a ($(du -h "$OUT/$triple/libgui.a" | cut -f1))"
done

verify_upstream
echo "==> 构建后复查：third_party/ 仍与上游基线一致（零改动）"
echo "✅ gui 包三平台静态库构建完成（x11 / x11-musl / win32 三份真实现）"
echo "   下一步：dialc pack packages/gui -o gui.dialpkg"
