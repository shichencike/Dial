#!/usr/bin/env bash
#
# Dial 一键安装脚本（Linux / Termux / macOS）——ARCHITECTURE.md §3.2 阶段 5。
#
# 承担四件事（顺序不能反）：
#
#   1. 初始化 Dial 环境：建 ~/.dial/ 目录树、把 dialc 挂进 PATH、写工具链配置；
#   2. 自动安装官方标准库（stdlib.dialpkg）与 VS Code 插件（.vsix）——都是
#      「装上编译器后自然该有」的东西，不该让用户再手动跑一遍；
#   3. 自动安装已下载的包：初始化完成后**重新扫描**找到的 .dialpkg（当前目录及其
#      子目录，或脚本参数指定的目录），逐个执行安装命令；
#   4. 收尾：给出清晰的「做了什么 / 还需要你做什么」总结。
#
# 与旧版相比的关键改进（v0.99.1 重写）：
#   - 版本**不再硬编码**：优先从已安装的 dialc 读出真实版本号，bootstrap 才回退到
#     内置默认版本；配套件（标准库 / 插件）一律按这个版本推导，**彻底消灭版本不同步**
#     （旧版写死 0.98.1，插件名却是 dial-0.98.0.vsix，每次都 404）；
#   - VS Code 插件按「Release 资产清单」解析真实文件名，不依赖插件自身 semver 与
#     编译器版本号对齐——命名再怎么漂移也不会找不到；
#   - 下载失败**可见原因**（旧版 2>/dev/null 把 curl 报错吞了，用户只看到「失败」二字）；
#   - 支持 bash / zsh / fish / profile 四种 shell 的 PATH 持久化，并提示当前会话怎么用；
#   - 支持 --help、--version、--uninstall；解压前校验压缩包完整性；重跑幂等。
#
# 用法：
#   bash install-dial.sh [选项] [扫描目录 ...]
#
#   不带参数        -> 扫描当前目录及其子目录里的 .dialpkg
#   带目录参数      -> 只扫描指定的目录（可多个）
#
# 选项：
#   -h, --help      显示帮助
#   -v, --version   显示本脚本将使用的 Dial 版本后退出
#   --uninstall     卸载（删除 ~/.dial 与 PATH 写入）
#
# 环境变量（一般无需设置，网络不稳 / 走镜像时才有用）：
#   DIAL_VERSION     覆盖配套件版本（默认：从 dialc 读出，否则内置默认）
#   DIALC_BIN       指定要挂进 PATH 的 dialc（默认：脚本同目录的 ./dialc）
#   DIALC_HOME      指定 ~/.dial 的位置（默认：$HOME/.dial）
#   DIALC_SRC       指定 Dial 发行包根目录（含 stdlib/ 与 runtime/；默认：脚本所在目录）
#   DIAL_MIRROR_PREFIX  国内镜像前缀（默认 https://ghproxy.net/；置空则只用官方源）
#   STDLIB_RELEASE  标准库独立发布标签（默认 stdlib-v1.0.0，与编译器解耦）
#   STDLIB_URL / DIAL_VSIX_URL  覆盖对应下载地址（镜像 / 自建源）
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-}")" && pwd)"
DIALC_HOME="${DIALC_HOME:-$HOME/.dial}"

# 内置默认版本：仅在「本地无任何 dialc 可探测」时用于 bootstrap。
# 正常流程（已装好 dialc）会立刻被真实版本覆盖，所以这里写错也无所谓。
DEFAULT_VERSION="0.99.1"
DIAL_VERSION="${DIAL_VERSION:-$DEFAULT_VERSION}"
DIAL_TAG="v${DIAL_VERSION}"

# 编译器本体文件名
if [ -x "$SCRIPT_DIR/dialc.exe" ]; then
  DIALC_BIN="${DIALC_BIN:-$SCRIPT_DIR/dialc.exe}"
elif [ -x "$SCRIPT_DIR/dialc" ]; then
  DIALC_BIN="${DIALC_BIN:-$SCRIPT_DIR/dialc}"
else
  DIALC_BIN="${DIALC_BIN:-$SCRIPT_DIR/dialc}"
fi
DIALC_SRC="${DIALC_SRC:-$SCRIPT_DIR}"

# 标准库独立发布标签（与编译器解耦）
STDLIB_RELEASE="${STDLIB_RELEASE:-stdlib-v1.0.0}"

REPO="shichencike/Dial"

# ---------------------------------------------------------------------------
# 输出助手
# ---------------------------------------------------------------------------
say()  { printf '  %s\n' "$*"; }
ok()   { printf '  \033[32m✅ %s\033[0m\n' "$*"; }
warn() { printf '  \033[33m⚠️  %s\033[0m\n' "$*" >&2; }
die()  { printf '  \033[31m❌ %s\033[0m\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 下载基础设施：国内镜像优先 + 官方回退 + 断点续传；失败给出真实原因
# ---------------------------------------------------------------------------
HAVE_CURL=0; HAVE_WGET=0
command -v curl >/dev/null 2>&1 && HAVE_CURL=1
command -v wget >/dev/null 2>&1 && HAVE_WGET=1
MIRROR_PREFIX="${DIAL_MIRROR_PREFIX:-https://ghproxy.net/}"

# download <输出文件> <官方URL> [覆盖URL]
#   覆盖URL优先级最高（镜像 / 自建源已体现在覆盖里）；否则镜像前缀 + 官方 -> 官方。
#   任一成功（且文件非空）即返回 0；全部失败返回 1，并把最近一次 curl 的报错打印出来。
download() {
    local out="$1" official="$2" override="${3:-}"
    local candidates=()
    if [ -n "$override" ]; then
        candidates=("$override")
    elif [ -n "$MIRROR_PREFIX" ]; then
        candidates=("${MIRROR_PREFIX}${official}" "$official")
    else
        candidates=("$official")
    fi
    local last_err=""
    for url in "${candidates[@]}"; do
        say "  源: $url"
        if [ "$HAVE_CURL" = "1" ]; then
            local errfile; errfile="$(mktemp -t dialdl.XXXXXX)"
            if curl -fSL --retry 3 -C - --proto '=https' --connect-timeout 15 \
                    --max-time 600 -o "$out" "$url" 2>"$errfile" \
               && [ -s "$out" ]; then
                rm -f "$errfile"
                return 0
            fi
            last_err="$(cat "$errfile" | tail -3 | tr '\n' ' ')"
            rm -f "$errfile"
        elif [ "$HAVE_WGET" = "1" ]; then
            if wget -c -q -O "$out" "$url" 2>/dev/null && [ -s "$out" ]; then
                return 0
            fi
            last_err="wget 退出码非 0"
        fi
    done
    [ -n "$last_err" ] && say "  最近一次报错: $last_err"
    return 1
}

# 用 GitHub API 列出某 tag 的 Release 资产名（用于按真实文件名解析插件等）。
# 成功时输出资产名（一个），失败输出空。优先直连 GitHub，失败再走镜像。
list_release_assets() {
    local tag="$1"
    local api="https://api.github.com/repos/${REPO}/releases/tags/${tag}"
    local json; json="$(curl -fSL --connect-timeout 15 --max-time 60 \
        -H "Accept: application/vnd.github+json" "$api" 2>/dev/null)"
    if [ -n "$json" ]; then
        printf '%s' "$json" | grep -oE '"name":[[:space:]]*"[^"]+\.(vsix|zip)"' \
            | sed -E 's/.*"name":[[:space:]]*"([^"]+)".*/\1/'
    fi
}

# ---------------------------------------------------------------------------
# 平台探测
# ---------------------------------------------------------------------------
detect_platform() {
    local os arch
    os="$(uname -s 2>/dev/null || echo unknown)"
    arch="$(uname -m 2>/dev/null || echo unknown)"
    case "$os" in
      Linux*) case "$arch" in
                x86_64|amd64)   PLAT_SLUG="linux-x86_64" ;;
                aarch64|arm64)  PLAT_SLUG="termux-aarch64" ;;
                *) die "不支持的架构：$arch（请用源码构建 dialc）" ;;
              esac ;;
      Darwin*) case "$arch" in
                x86_64|amd64)   PLAT_SLUG="macos-x86_64" ;;
                aarch64|arm64)  PLAT_SLUG="macos-aarch64" ;;
                *) die "不支持的 macOS 架构：$arch（请用源码构建 dialc）" ;;
              esac ;;
      *) die "不支持的平台：$os（Windows 请用 install-dial.ps1）" ;;
    esac
}

# 从已安装的 dialc 读出真实版本号，覆盖内置默认（消灭版本不同步）。
detect_compiler_version() {
    local bin="$1" out
    out="$($bin --version 2>/dev/null || $bin version 2>/dev/null)"
    # 形如 "dialc 0.99.1" / "dialc 0.99.1 (debug)" —— 取第一个 X.Y.Z
    local ver; ver="$(printf '%s' "$out" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
    if [ -n "$ver" ]; then
        DIAL_VERSION="$ver"
        DIAL_TAG="v${ver}"
    fi
}

# ---------------------------------------------------------------------------
# 引导下载（curl ... | sh 且无本地 dialc 时走这里）
# ---------------------------------------------------------------------------
bootstrap_if_needed() {
    local bin
    bin="$(command -v dialc 2>/dev/null || true)"
    if { [ -x "${DIALC_BIN:-}" ] || [ -n "$bin" ] \
         || [ -x "$SCRIPT_DIR/dialc" ] || [ -x "$SCRIPT_DIR/dialc.exe" ] \
         || [ -x ./dialc ] || [ -x ./dialc.exe ]; }; then
        return 0
    fi
    echo "== 引导下载 Dial 发布包 =="
    detect_platform
    local zip="dialc-${DIAL_TAG}-${PLAT_SLUG}.zip"
    local work target official inner
    work="$(mktemp -d)" || die "无法创建临时目录"
    target="$work/$zip"
    say "  平台: $PLAT_SLUG   版本: $DIAL_VERSION"
    official="https://github.com/${REPO}/releases/download/${DIAL_TAG}/$zip"
    if ! download "$target" "$official"; then
        die "无法下载 Dial 发布包（镜像与官方源均失败）。请手动下载后运行：
   $official
   然后执行包内 install-dial.sh"
    fi
    # 完整性：解压前先校验是合法的 zip
    if ! ( unzip -t "$target" >/dev/null 2>&1 || tar -tf "$target" >/dev/null 2>&1 ); then
        rm -rf "$work"
        die "下载的发布包损坏（不是合法的 zip），请删除后重试。"
    fi
    ( cd "$work" && { unzip -o "$zip" >/dev/null 2>&1 || tar -xf "$zip" 2>/dev/null || bsdtar -xf "$zip" 2>/dev/null; } ) \
        || die "解压发布包失败：$target"
    inner="$(find "$work" -name install-dial.sh -type f | head -1)"
    [ -n "$inner" ] || die "发布包内未找到 install-dial.sh"
    # 用包内自带脚本继续（此时已是「本地发布包」常规路径），并把参数原样传下去
    exec bash "$inner" "$@"
}

# ---------------------------------------------------------------------------
# 卸载
# ---------------------------------------------------------------------------
do_uninstall() {
    echo "== 卸载 Dial =="
    if [ ! -d "$DIALC_HOME" ]; then
        say "未找到 $DIALC_HOME，无需卸载。"
        return 0
    fi
    rm -rf "$DIALC_HOME" && ok "已删除 $DIALC_HOME"
    # 清理 PATH 写入（四种 shell 配置）
    local cleaned=0
    for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.config/fish/config.fish"; do
        [ -f "$rc" ] || continue
        if grep -qF "$DIALC_HOME/bin" "$rc" 2>/dev/null; then
            # 只删掉我们写入的那一段（含前后空行）
            local tmp; tmp="$(mktemp)"
            awk -v home="$DIALC_HOME" '
                BEGIN{b=0}
                /# Dial（由 install-dial.sh 写入）/{b=1; next}
                b && /^export PATH=.*DIALC_HOME\/bin/{b=0; next}
                b && /^$/{next}
                b{next}
                {print}
            ' "$rc" > "$tmp" && mv "$tmp" "$rc" && cleaned=1
        fi
    done
    [ "$cleaned" = "1" ] && ok "已清理 shell 配置里的 PATH 写入（重开终端后生效）" \
        || say "shell 配置中未发现写入（或无需清理）"
    say "卸载完成。已安装的 .dialpkg 包位于各项目目录内，未一并删除。"
    exit 0
}

# ---------------------------------------------------------------------------
# 选项解析（在任何网络操作之前）
# ---------------------------------------------------------------------------
for arg in "$@"; do
    case "$arg" in
      -h|--help)
        sed -n '3,60p' "$0" | sed 's/^# \{0,1\}//'
        exit 0 ;;
      -v|--version)
        detect_compiler_version "${DIALC_BIN:-}" 2>/dev/null || true
        echo "Dial 版本：$DIAL_VERSION  标签：$DIAL_TAG"
        exit 0 ;;
      --uninstall) DO_UNINSTALL=1 ;;
    esac
done
[ "${DO_UNINSTALL:-0}" = "1" ] && do_uninstall

echo "== Dial 一键安装 =="
bootstrap_if_needed "$@"

# 第 1 步：定位 dialc（并把真实版本号读出来）
if [ ! -x "$DIALC_BIN" ]; then
    bin="$(command -v dialc 2>/dev/null || true)"
    if [ -n "$bin" ]; then DIALC_BIN="$bin"; else
        die "找不到 dialc 可执行文件。
  请把 dialc 放在本脚本同目录下，或用环境变量指定：
      DIALC_BIN=/path/to/dialc bash $(basename "$0")"
    fi
fi
detect_compiler_version "$DIALC_BIN"   # 用真实版本覆盖默认/环境变量
ok "编译器: $DIALC_BIN  (Dial $DIAL_VERSION)"

# 第 2 步：初始化环境
echo
echo "-- 初始化环境 --"
mkdir -p "$DIALC_HOME/bin" "$DIALC_HOME/packages" "$DIALC_HOME/cache/downloads" || \
    die "创建 $DIALC_HOME 目录树失败"
ok "目录树: $DIALC_HOME/{bin,packages,cache/downloads}"

DST_BIN="$DIALC_HOME/bin/$(basename "$DIALC_BIN")"
if cp -f "$DIALC_BIN" "$DST_BIN" 2>/dev/null; then
    chmod +x "$DST_BIN"
    ok "编译器已安装到 $DST_BIN"
else
    warn "复制编译器到 $DIALC_HOME/bin 失败（可稍后手动处理）"
fi

# 标准库与运行时：编译器按「可执行文件所在位置」向上找 stdlib/ 与 runtime/，
# 装到 ~/.dial/bin/dialc 后，那个位置正好是 ~/.dial/ —— 把两者放这里即可。
if [ -d "$DIALC_SRC/stdlib" ] && [ -d "$DIALC_SRC/runtime" ]; then
    rm -rf "$DIALC_HOME/stdlib" "$DIALC_HOME/runtime"
    if cp -r "$DIALC_SRC/stdlib" "$DIALC_HOME/stdlib" 2>/dev/null \
       && cp -r "$DIALC_SRC/runtime" "$DIALC_HOME/runtime" 2>/dev/null; then
        ok "标准库与运行时已安装到 $DIALC_HOME/{stdlib,runtime}"
    else
        warn "复制 stdlib/ 与 runtime/ 失败"
    fi
else
    warn "在 $DIALC_SRC 下没找到 stdlib/ 与 runtime/。
  请用完整发行包运行本脚本（或用 DIALC_SRC 指定发行包根目录）。
  否则编译时会报「找不到 stdlib/ 与 runtime/ 目录」。"
fi

# PATH 持久化：bash / zsh / fish / profile 都覆盖；并提示当前会话。
SHELL_RC_TOUCHED=0
PATH_LINE="export PATH=\"\$PATH:$DIALC_HOME/bin\""
BLOCK_START="# Dial（由 install-dial.sh 写入）"
for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
    [ -f "$rc" ] || continue
    if grep -qF "$DIALC_HOME/bin" "$rc" 2>/dev/null; then continue; fi
    {
        echo ""
        echo "$BLOCK_START"
        echo "$PATH_LINE"
    } >> "$rc" && SHELL_RC_TOUCHED=1
done
# fish：语法不同
FISH_RC="$HOME/.config/fish/config.fish"
if command -v fish >/dev/null 2>&1; then
    mkdir -p "$(dirname "$FISH_RC")"
    if [ -f "$FISH_RC" ] && grep -qF "$DIALC_HOME/bin" "$FISH_RC" 2>/dev/null; then :;
    else
        echo "set -gx PATH \$PATH $DIALC_HOME/bin" >> "$FISH_RC" && SHELL_RC_TOUCHED=1
    fi
fi
if [ "$SHELL_RC_TOUCHED" = "1" ]; then
    ok "PATH 已写入 shell 配置（bash / zsh / fish / profile，重开终端后生效）"
else
    say "当前会话请先执行： export PATH=\"\$PATH:$DIALC_HOME/bin\""
fi

# 工具链配置
cat > "$DIALC_HOME/config.txt" <<EOF
# Dial 环境配置（由 install-dial.sh 生成）
dialc_path   = $DST_BIN
version      = $DIAL_VERSION
installed_at = $(date -u '+%Y-%m-%dT%H:%M:%SZ')
uname        = $(uname -srm)
EOF
ok "工具链配置: $DIALC_HOME/config.txt"

# 第 2.5 步：官方标准库包（失败只告警、不阻断）
echo
echo "-- 安装官方标准库 stdlib.dialpkg --"
STDLIB_OFFICIAL="https://github.com/${REPO}/releases/download/${STDLIB_RELEASE}/stdlib.dialpkg"
STDLIB_TMP="$(mktemp -t stdlib.XXXXXX.dialpkg)"
if [ "$HAVE_CURL" = "1" ] || [ "$HAVE_WGET" = "1" ]; then
    if download "$STDLIB_TMP" "$STDLIB_OFFICIAL" "${STDLIB_URL:-}"; then
        if "$DST_BIN" install "$STDLIB_TMP" 2>/dev/null; then
            ok "官方标准库已安装（${STDLIB_RELEASE}，镜像优先 + 官方回退）"
        else
            warn "dialc install stdlib.dialpkg 失败（不影响编译器本体，可稍后手动安装）"
        fi
    else
        warn "下载官方标准库失败（可能离线 / 镜像与官方均不可达）；链接时将回退到源码编译 stdlib。"
    fi
else
    warn "未找到 curl / wget，跳过自动安装标准库（链接时将回退到源码编译 stdlib）。"
fi
rm -f "$STDLIB_TMP" 2>/dev/null

# 第 2.7 步：VS Code 插件（按真实文件名解析，消灭版本不同步导致的 404）
echo
echo "-- 安装 VS Code 插件 --"
CODE_CLI=""
for c in code code-insiders codium; do
    if command -v "$c" >/dev/null 2>&1; then CODE_CLI="$c"; break; fi
done
if [ -z "$CODE_CLI" ]; then
    warn "未找到 VS Code 命令行（code / code-insiders / codium），跳过插件安装。
  装好 VS Code 后手动执行即可：code --install-extension <下载到的 .vsix>"
elif [ "$HAVE_CURL" != "1" ] && [ "$HAVE_WGET" != "1" ]; then
    warn "未找到 curl / wget，跳过 VS Code 插件安装（编译器本体不受影响）。"
else
    # 先按真实 Release 资产名解析，再回退到版本推导名
    VSIX_NAME="$(list_release_assets "$DIAL_TAG" | grep -m1 '\.vsix$' || true)"
    if [ -z "$VSIX_NAME" ]; then
        VSIX_NAME="dial-${DIAL_VERSION}.vsix"
        say "  （无法从 Release 资产清单解析插件名，回退到版本推导名：$VSIX_NAME）"
    fi
    VSIX_OFFICIAL="https://github.com/${REPO}/releases/download/${DIAL_TAG}/${VSIX_NAME}"
    VSIX_TMP="$(mktemp -t dialplugin.XXXXXX.vsix)"
    if download "$VSIX_TMP" "$VSIX_OFFICIAL" "${DIAL_VSIX_URL:-}" && [ -s "$VSIX_TMP" ]; then
        if "$CODE_CLI" --install-extension "$VSIX_TMP"; then
            ok "VS Code 插件已安装（$VSIX_NAME）"
        else
            warn "VS Code 插件安装失败，可稍后手动执行：
    $CODE_CLI --install-extension $VSIX_NAME"
        fi
    else
        warn "下载 VS Code 插件失败（可能离线 / 镜像与官方均不可达）；可稍后手动安装。"
    fi
    rm -f "$VSIX_TMP" 2>/dev/null
fi

# 第 3 步：重新扫描 .dialpkg 并逐个安装
echo
echo "-- 扫描 .dialpkg --"
SCAN_DIRS=("$@")
if [ ${#SCAN_DIRS[@]} -eq 0 ]; then SCAN_DIRS=("."); fi
PKGS=()
for d in "${SCAN_DIRS[@]}"; do
    [ -d "$d" ] || { warn "扫描目录不存在: $d"; continue; }
    while IFS= read -r f; do [ -n "$f" ] && PKGS+=("$f"); done \
        < <(find "$d" -type f -name '*.dialpkg' 2>/dev/null | sort)
done
if [ ${#PKGS[@]} -eq 0 ]; then
    say "未找到 .dialpkg（跳过包安装）"
else
    printf '  找到 %d 个包：\n' "${#PKGS[@]}"
    for f in "${PKGS[@]}"; do printf '    - %s\n' "$f"; done
    echo
    INSTALLED=0; FAILED=0
    for f in "${PKGS[@]}"; do
        echo "  → 安装 $f"
        if "$DST_BIN" install "$f"; then INSTALLED=$((INSTALLED + 1));
        else FAILED=$((FAILED + 1)); warn "安装失败: $f"; fi
        echo
    done
    ok "包安装完成：成功 $INSTALLED 个，失败 $FAILED 个"
fi

# 收尾总结
echo
echo "== 完成 =="
ok "Dial $DIAL_VERSION 已安装到 $DIALC_HOME"
if [ "$SHELL_RC_TOUCHED" = "1" ]; then
    say "请**重开终端**（或执行 export PATH=\"\$PATH:$DIALC_HOME/bin\"）让 dialc 全局可用。"
else
    say "当前会话即可用：export PATH=\"\$PATH:$DIALC_HOME/bin\""
fi
say "用法：dialc build 你的程序.dial -o 程序"
say "      import \"包名\"; 即可调用已安装的包"
