#!/usr/bin/env bash
#
# XTest 真注入脚本（AA 组第 5d 项，必须在 xvfb-run 启动的 X server 内运行）。
#
# 用法：xvfb-run -a -s "..." bash tests/pkg/xtest_inject.sh <二进制> <日志>
#
# 职责：启动 Dial 程序 → 等它把窗口映射出来 → 用 xdotool（底层 XTEST）
# 往真实窗口注入鼠标点击 / 键盘 "hi" / Tab / Shift+Tab → 等程序收完事件退出。
# 注入全在 X server 侧完成，gui.dialpkg 自身**不需要**链接 libXtst，
# 也不新增任何平台符号（nm -u 断言照旧成立）。
#
# 环境不满足时如实写 `input_xtest_injection = skipped [原因]`，绝不把
# 未验证伪装成通过；run_tests.sh 读到该标记会打印跳过而不计入失败。
set -u

BIN="$1"
LOG="$2"

skip() {
    printf 'input_xtest_injection = skipped [%s]\n' "$1" >> "$LOG"
    if [ -n "${PID:-}" ]; then kill "$PID" >/dev/null 2>&1 || true; fi
    exit 0
}

if ! command -v xdotool >/dev/null 2>&1; then
    : > "$LOG"
    skip "无 xdotool，真实事件注入未实机验证"
fi

: > "$LOG"
"$BIN" >"$LOG" 2>&1 &
PID=$!

WID=""
for _ in $(seq 1 80); do
    WID="$(xdotool search --name 'Dial GUI Input' 2>/dev/null | head -n 1 || true)"
    [ -n "$WID" ] && break
    sleep 0.1
done
[ -n "$WID" ] || skip "Xvfb 里未找到 Dial GUI Input 窗口"

GEO="$(xdotool getwindowgeometry --shell "$WID" 2>/dev/null || true)"
[ -n "$GEO" ] || skip "无法读取窗口几何"
# shellcheck disable=SC2086  # xdotool 输出的是 X=/Y=/WIDTH=/HEIGHT= 纯数字
eval "$GEO"
X="${X:-0}"; Y="${Y:-0}"

xdotool windowfocus --sync "$WID" >/dev/null 2>&1 || true

# 窗口客户区坐标：窗口由 XCreateSimpleWindow 放在 (40,40)，无 WM 装饰，
# 客户区原点即窗口原点。tb1 中心 (140,40)，btn 中心 (295,40)。
CX1=$((X + 140)); CY1=$((Y + 40))
CX2=$((X + 295)); CY2=$((Y + 40))

# 1) 点击第一个 TextBox → 真实 MOUSE_MOVE/PRESS/RELEASE，RELEASE 把焦点移过去
xdotool mousemove --sync "$CX1" "$CY1"
xdotool click 1
# 2) 真实键盘输入 "hi" → 两个 KEY_PRESS，TextBox 落字
xdotool type --delay 60 --clearmodifiers "hi"
# 3) Tab 前进焦点 / Shift+Tab 反向（后者事件 mods 应含 SHIFT=1）
xdotool key Tab
xdotool key shift+Tab
# 4) 点击 BUTTON → 真实鼠标激活一次
xdotool mousemove --sync "$CX2" "$CY2"
xdotool click 1

wait "$PID"
RC=$?
printf 'input_xtest_injection = ok\n' >> "$LOG"
exit "$RC"
