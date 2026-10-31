#!/usr/bin/env bash
#
# Dial 一键安装脚本（Linux / Termux）——ARCHITECTURE.md §3.2 阶段 5。
#
# 承担两件事（顺序不能反）：
#
#   1. **初始化 Dial 环境**：建 ~/.dial/ 目录树、把 dialc 挂进 PATH、写工具链配置；
#   2. **自动安装已下载的包**：初始化完成后**重新扫描**找到的 `.dialpkg`
#      （当前目录及其子目录，或脚本参数指定的目录），逐个执行安装命令。
#
# 为什么脚本要「自动重新扫描」而不是让用户逐个点名：一键安装的语义是
# 「我把这个目录交给你，你把该装的都装上」。用户不该需要记住包名、也不该需要
# 为每个包敲一条命令——包就在那里，脚本看得见。
#
# 用法：
#   bash install-dial.sh [扫描目录 ...]
#
#   不带参数   -> 扫描当前目录及其子目录
#   带参数     -> 扫描指定的目录（可多个）
#
# 环境变量：
#   DIALC_BIN   指定要挂进 PATH 的 dialc 可执行文件（默认：脚本同目录的 ./dialc）
#   DIALC_HOME  指定 ~/.dial 的位置（默认：$HOME/.dial）
#   DIALC_SRC   指定 Dial 发行包的根目录（含 stdlib/ 与 runtime/）
#                （默认：脚本所在目录）
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIALC_HOME="${DIALC_HOME:-$HOME/.dial}"
DIALC_BIN="${DIALC_BIN:-$SCRIPT_DIR/dialc}"
DIALC_SRC="${DIALC_SRC:-$SCRIPT_DIR}"

say()  { printf '  %s\n' "$*"; }
ok()   { printf '  ✅ %s\n' "$*"; }
warn() { printf '  ⚠️  %s\n' "$*"; }
die()  { printf '  ❌ %s\n' "$*" >&2; exit 1; }

echo "== Dial 一键安装 =="

# ---------------------------------------------------------------------------
# 第 1 步：定位 dialc
# ---------------------------------------------------------------------------
if [ ! -x "$DIALC_BIN" ]; then
    # 退而求其次：PATH 里已有 dialc？
    if command -v dialc >/dev/null 2>&1; then
        DIALC_BIN="$(command -v dialc)"
    else
        die "找不到 dialc 可执行文件。
  请把 dialc 放在本脚本同目录下，或用环境变量指定：
      DIALC_BIN=/path/to/dialc bash $(basename "$0")"
    fi
fi
ok "编译器: $DIALC_BIN"

# ---------------------------------------------------------------------------
# 第 2 步：初始化环境（建目录树 + 挂 PATH + 写配置）
# ---------------------------------------------------------------------------
echo
echo "-- 初始化环境 --"
mkdir -p "$DIALC_HOME/bin" "$DIALC_HOME/packages" "$DIALC_HOME/cache/downloads" || \
    die "创建 $DIALC_HOME 目录树失败"
ok "目录树: $DIALC_HOME/{bin,packages,cache/downloads}"

# 把 dialc 挂进 ~/.dial/bin（PATH 里加这一条即可全局可用）
cp -f "$DIALC_BIN" "$DIALC_HOME/bin/dialc" 2>/dev/null && \
    ok "编译器已安装到 $DIALC_HOME/bin/dialc" || \
    warn "复制编译器到 $DIALC_HOME/bin 失败（可稍后手动处理）"

# **标准库与运行时也要装**：编译器按「可执行文件所在位置」向上找 stdlib/ 与 runtime/
# （find_project_base：<exe_dir>/../../ 即工程根）。装到 ~/.dial/bin/dialc 后，
# 那个位置正好是 ~/.dial/ —— 所以把 stdlib/ 与 runtime/ 放这里即可。
# 少了这一步，用户在自己项目里编译会报「找不到 stdlib/ 与 runtime/ 目录」。
if [ -d "$DIALC_SRC/stdlib" ] && [ -d "$DIALC_SRC/runtime" ]; then
    rm -rf "$DIALC_HOME/stdlib" "$DIALC_HOME/runtime"
    cp -r "$DIALC_SRC/stdlib" "$DIALC_HOME/stdlib" && \
    cp -r "$DIALC_SRC/runtime" "$DIALC_HOME/runtime" && \
    ok "标准库与运行时已安装到 $DIALC_HOME/{stdlib,runtime}" || \
    warn "复制 stdlib/ 与 runtime/ 失败"
else
    warn "在 $DIALC_SRC 下没找到 stdlib/ 与 runtime/。
  请用完整发行包运行本脚本（或用 DIALC_SRC 指定发行包根目录）。
  否则编译时会报「找不到 stdlib/ 与 runtime/ 目录」。"
fi

# PATH 持久化：写进 ~/.profile 与 ~/.bashrc（存在才写，不新建 shell 配置）
SHELL_RC_TOUCHED=0
for rc in "$HOME/.profile" "$HOME/.bashrc"; do
    [ -f "$rc" ] || continue
    if grep -qF "$DIALC_HOME/bin" "$rc" 2>/dev/null; then
        continue
    fi
    {
        echo ""
        echo "# Dial（由 install-dial.sh 写入）"
        echo "export PATH=\"\$PATH:$DIALC_HOME/bin\""
    } >> "$rc" && SHELL_RC_TOUCHED=1
done
if [ "$SHELL_RC_TOUCHED" = "1" ]; then
    ok "PATH 已写入 ~/.profile / ~/.bashrc（重开终端后生效）"
else
    say "本次会话请先执行： export PATH=\"\$PATH:$DIALC_HOME/bin\""
fi

# 工具链配置：记录本机信息，便于排查「这台机器上装的是啥」
cat > "$DIALC_HOME/config.txt" <<EOF
# Dial 环境配置（由 install-dial.sh 生成）
dialc_path   = $DIALC_HOME/bin/dialc
installed_at = $(date -u '+%Y-%m-%dT%H:%M:%SZ')
uname        = $(uname -srm)
EOF
ok "工具链配置: $DIALC_HOME/config.txt"

# ---------------------------------------------------------------------------
# 第 3 步：重新扫描 .dialpkg 并逐个安装
# ---------------------------------------------------------------------------
echo
echo "-- 扫描 .dialpkg --"

SCAN_DIRS=("$@")
if [ ${#SCAN_DIRS[@]} -eq 0 ]; then
    SCAN_DIRS=(".")
fi

PKGS=()
for d in "${SCAN_DIRS[@]}"; do
    [ -d "$d" ] || { warn "扫描目录不存在: $d"; continue; }
    while IFS= read -r f; do
        [ -n "$f" ] && PKGS+=("$f")
    done < <(find "$d" -type f -name '*.dialpkg' 2>/dev/null | sort)
done

if [ ${#PKGS[@]} -eq 0 ]; then
    say "未找到 .dialpkg（跳过包安装）"
else
    printf '  找到 %d 个包：\n' "${#PKGS[@]}"
    for f in "${PKGS[@]}"; do
        printf '    - %s\n' "$f"
    done
    echo
    INSTALLED=0
    FAILED=0
    for f in "${PKGS[@]}"; do
        echo "  → 安装 $f"
        # 用 ~/.dial/bin/dialc（刚装好的那个），保证「装的包」与「装的编译器」配套
        if "$DIALC_HOME/bin/dialc" install "$f"; then
            INSTALLED=$((INSTALLED + 1))
        else
            FAILED=$((FAILED + 1))
            warn "安装失败: $f"
        fi
        echo
    done
    ok "包安装完成：成功 $INSTALLED 个，失败 $FAILED 个"
fi

echo
echo "== 完成 =="
say "用法：dialc build 你的程序.dial -o 程序"
say "      import \"包名\"; 即可调用已安装的包"
