#!/usr/bin/env bash
#
# Dial 一键安装脚本（Linux / Termux）——ARCHITECTURE.md §3.2 阶段 5。
#
# 承担三件事（顺序不能反）：
#
#   1. **初始化 Dial 环境**：建 ~/.dial/ 目录树、把 dialc 挂进 PATH、写工具链配置；
#   2. **自动安装已下载的包**：初始化完成后**重新扫描**找到的 `.dialpkg`
#      （当前目录及其子目录，或脚本参数指定的目录），逐个执行安装命令。
#   3. **补齐配套件**：官方标准库（stdlib.dialpkg）与 VS Code 插件（.vsix）
#      静默下载并安装——都是「装上编译器后自然该有」的东西，不该让用户再手动跑一遍。
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
#   DIALC_VERSION 指定配套件的版本（默认：0.98.0）。Release 标签（v0.98）与
#                插件文件名（dial-0.98.0.vsix）都由它推导，只改这一处即可。
#   STDLIB_URL  覆盖官方标准库的下载地址（镜像 / 自建源），网络不稳时指向国内镜像。
#
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-}")" && pwd)"
DIALC_HOME="${DIALC_HOME:-$HOME/.dial}"

# 配套件版本号：单一来源。bootstrap 下载发布包、以及 VS Code 插件、标准库标签都由它推导。
# 每个发布包都会随附本脚本（tools/install-dial.sh -> 包内 install-dial.sh），发版时随仓库一起更新。
DIAL_VERSION="${DIAL_VERSION:-0.98.0}"
DIAL_TAG="v$(printf '%s' "$DIAL_VERSION" | cut -d. -f1,2)"

# 编译器本体可能是 dialc（Linux / Termux）或 dialc.exe（Windows / MinGW）。
if [ -x "$SCRIPT_DIR/dialc.exe" ]; then
  DIALC_BIN="${DIALC_BIN:-$SCRIPT_DIR/dialc.exe}"
elif [ -x "$SCRIPT_DIR/dialc" ]; then
  DIALC_BIN="${DIALC_BIN:-$SCRIPT_DIR/dialc}"
else
  DIALC_BIN="${DIALC_BIN:-$SCRIPT_DIR/dialc}"
fi
DIALC_SRC="${DIALC_SRC:-$SCRIPT_DIR}"

# 官方标准库独立发布（阶段 7）：tag 形如 stdlib-v<版本>，与编译器（dialc）解耦。
# 安装脚本在装好编译器后，会自动静默拉取该版本的标准库包并通过 dialc install 安装。
# 升级标准库时只需改这里的标签（与 Dial Releases 里的 stdlib-v* 对应）。
STDLIB_RELEASE="${STDLIB_RELEASE:-stdlib-v1.0.0}"

# ---------------------------------------------------------------------------
# 下载基础设施：国内镜像优先 + 官方回退 + 断点续传
# ---------------------------------------------------------------------------
# 探测可用的下载器（curl 优先，wget 兜底；Termux / 多数 Linux 都自带其一）。
HAVE_CURL=0; HAVE_WGET=0
command -v curl >/dev/null 2>&1 && HAVE_CURL=1
command -v wget >/dev/null 2>&1 && HAVE_WGET=1

# 国内加速镜像前缀（ghproxy.net）。作为候选源第一顺位，失败自动回退官方 GitHub。
# 可用 DIAL_MIRROR_PREFIX 覆盖（如自建反代）；置空则只用官方源。
MIRROR_PREFIX="${DIAL_MIRROR_PREFIX:-https://ghproxy.net/}"

# download <输出文件> <官方URL> [覆盖URL]
#   若给了覆盖URL（如 STDLIB_URL / DIAL_VSIX_URL），则只用它（镜像 / 自建源已体现在覆盖里）。
#   否则依次尝试：镜像前缀 + 官方URL -> 官方URL。
#   任一成功（且文件非空）即返回 0；全部失败返回 1。
#   curl 用 -C - 续传 + --retry，wget 用 -c 续传；两者都带超时，避免「一键卡住」。
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
    for url in "${candidates[@]}"; do
        say "  源: $url"
        if [ "$HAVE_CURL" = "1" ]; then
            if curl -fSL --retry 3 -C - --proto '=https' --connect-timeout 15 \
                    --max-time 600 -o "$out" "$url" 2>/dev/null && [ -s "$out" ]; then
                return 0
            fi
        elif [ "$HAVE_WGET" = "1" ]; then
            if wget -c -q -O "$out" "$url" 2>/dev/null && [ -s "$out" ]; then
                return 0
            fi
        fi
    done
    return 1
}

say()  { printf '  %s\n' "$*"; }
ok()   { printf '  ✅ %s\n' "$*"; }
warn() { printf '  ⚠️  %s\n' "$*"; }
die()  { printf '  ❌ %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 平台探测：设置 PLAT_SLUG（发布包 slug）并校验支持范围。
# ---------------------------------------------------------------------------
detect_platform() {
    local os arch
    os="$(uname -s 2>/dev/null || echo unknown)"
    arch="$(uname -m 2>/dev/null || echo unknown)"
    case "$os" in
      Linux*) case "$arch" in
                x86_64|amd64)   PLAT_SLUG="linux-x86_64" ;;
                aarch64|arm64)  PLAT_SLUG="termux-aarch64" ;;
                *) die "不支持的架构： $arch（请用源码构建 dialc）" ;;
              esac ;;
      *) die "不支持的平台： $os（Windows 请用 install-dial.ps1）" ;;
    esac
}

# 是否处于「本地已有发布包」的常规安装路径（此时跳过引导下载）。
in_local_release() {
    [ -n "${DIALC_BIN:-}" ] && [ -x "$DIALC_BIN" ] && return 0
    command -v dialc >/dev/null 2>&1 && return 0
    [ -n "${SCRIPT_DIR:-}" ] && { [ -x "$SCRIPT_DIR/dialc" ] || [ -x "$SCRIPT_DIR/dialc.exe" ]; } && return 0
    { [ -x ./dialc ] || [ -x ./dialc.exe ]; } && return 0
    return 1
}

# ---------------------------------------------------------------------------
# 引导安装（curl ... | sh 一键命令走这里）
#   本地没有可安装的 dialc 时，按当前平台下载对应发布包（镜像优先 + 官方回退 +
#   断点续传），解压后调用包内自带的 install-dial.sh 完成安装。
#
#   关键设计：一键命令的 URL 指向 raw.githubusercontent.com/<分支>/tools/install-dial.sh，
#   该地址**直接返回文件、不经过 releases/latest 重定向**（正是此前容易在代理后失败的一环）；
#   下面下载发布包也用精确版本标签 v0.98.0，**同样不经过 latest 重定向**。
#   安装脚本本身也随每个发布包一起分发（package_release.sh 会把它拷进 Dial/）。
# ---------------------------------------------------------------------------
bootstrap_if_needed() {
    in_local_release && return 0
    echo "== 引导下载 Dial 发布包 =="
    detect_platform
    local zip="dialc-${DIAL_TAG}-${PLAT_SLUG}.zip"
    local work target official inner
    work="$(mktemp -d)" || die "无法创建临时目录"
    target="$work/$zip"
    say "  平台: $PLAT_SLUG   版本: $DIAL_VERSION"
    official="https://github.com/shichencike/Dial/releases/download/${DIAL_TAG}/$zip"
    if ! download "$target" "$official"; then
        die "无法下载 Dial 发布包（镜像与官方源均失败）。可手动下载后运行：
   $official
   然后执行包内 install-dial.sh"
    fi
    ( cd "$work" && { unzip -o "$zip" >/dev/null 2>&1 || tar -xf "$zip" 2>/dev/null || bsdtar -xf "$zip" 2>/dev/null; } ) \
        || die "解压发布包失败： $target"
    inner="$(find "$work" -name install-dial.sh -type f | head -1)"
    [ -n "$inner" ] || die "发布包内未找到 install-dial.sh"
    # 重新执行包内自带的安装脚本（此时已处于「本地发布包」常规路径）。
    exec bash "$inner" "$@"
}

echo "== Dial 一键安装 =="

# 一键命令（curl ... | sh）且本地无发布包时，先引导下载再安装。
bootstrap_if_needed "$@"

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
DST_BIN="$DIALC_HOME/bin/$(basename "$DIALC_BIN")"
cp -f "$DIALC_BIN" "$DST_BIN" 2>/dev/null && \
    ok "编译器已安装到 $DST_BIN" || \
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
dialc_path   = $DST_BIN
installed_at = $(date -u '+%Y-%m-%dT%H:%M:%SZ')
uname        = $(uname -srm)
EOF
ok "工具链配置: $DIALC_HOME/config.txt"

# ---------------------------------------------------------------------------
# 第 2.5 步：自动拉取官方标准库包 stdlib.dialpkg 并安装
#   官方标准库独立发布（tag stdlib-v*，与编译器解耦）。装完编译器后静默拉取
#   对应版本并通过 dialc install 安装，使链接期能优先使用预编译的标准库包。
#   失败（如离线）只告警、不阻断安装——链接时会回退到源码编译 stdlib。
# ---------------------------------------------------------------------------
echo
echo "-- 安装官方标准库 stdlib.dialpkg --"
# 下载地址可被 STDLIB_URL 覆盖（镜像 / 自建源）：网络不稳或想走国内镜像时，
# 用 STDLIB_URL 指向自己的地址即可，其余逻辑不变（与 DIAL_VSIX_URL 同思路）。
STDLIB_OFFICIAL="https://github.com/shichencike/Dial/releases/download/${STDLIB_RELEASE}/stdlib.dialpkg"
STDLIB_TMP="$(mktemp -t stdlib.XXXXXX.dialpkg)"
if [ "$HAVE_CURL" = "1" ] || [ "$HAVE_WGET" = "1" ]; then
    # download() 默认「镜像优先 + 官方回退 + 断点续传」，任一成功即用。
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

# ---------------------------------------------------------------------------
# 第 2.7 步：自动下载并安装 VS Code 插件（.vsix）
#   编译器装好了，编辑器插件也得跟上——否则用户还得自己去 Release 页翻 .vsix、
#   再手动 `code --install-extension`，「一键」就名不副实了。
#   与标准库同理：失败（离线 / 没装 VS Code / 没装 curl）只告警、不阻断安装。
#
#   版本只有一个来源：DIAL_VERSION（形如 0.98.0）。Release 标签由它推导
#   （v0.98），插件文件名也由它推导（dial-0.98.0.vsix）——三者不会各说各话。
# ---------------------------------------------------------------------------
echo
echo "-- 安装 VS Code 插件 --"
DIAL_VERSION="${DIAL_VERSION:-0.98.0}"
DIAL_TAG="v$(printf '%s' "$DIAL_VERSION" | cut -d. -f1,2)"
VSIX_FILE="dial-${DIAL_VERSION}.vsix"
# URL 可覆盖：某些网络环境访问 GitHub 不稳（或想走自建镜像）时，
# 用 DIAL_VSIX_URL 指向自己的地址即可，其余逻辑完全不变。
VSIX_OFFICIAL="https://github.com/shichencike/Dial/releases/download/${DIAL_TAG}/${VSIX_FILE}"

# 找 VS Code 的命令行入口（不同发行版名字不同）
CODE_CLI=""
for c in code code-insiders codium; do
    if command -v "$c" >/dev/null 2>&1; then
        CODE_CLI="$c"
        break
    fi
done

if [ -z "$CODE_CLI" ]; then
    warn "未找到 VS Code 命令行（code / code-insiders / codium），跳过插件安装。
  装好 VS Code 后手动执行即可：
    ${DIAL_VSIX_URL:-$VSIX_OFFICIAL}
    code --install-extension <下载到的 .vsix>"
elif [ "$HAVE_CURL" != "1" ] && [ "$HAVE_WGET" != "1" ]; then
    warn "未找到 curl / wget，跳过 VS Code 插件安装（编译器本体不受影响）。"
else
    VSIX_TMP="$(mktemp -t dialplugin.XXXXXX.vsix)"
    # 同样走「镜像优先 + 官方回退 + 断点续传」。
    if download "$VSIX_TMP" "$VSIX_OFFICIAL" "${DIAL_VSIX_URL:-}" && [ -s "$VSIX_TMP" ]; then
        # 不加 --force：用户若已装了更新版本，VS Code 会保留新版本，
        # 我们不该在「一键安装」里悄悄把人家降级。
        if "$CODE_CLI" --install-extension "$VSIX_TMP"; then
            ok "VS Code 插件已安装（$VSIX_FILE）"
        else
            warn "VS Code 插件安装失败，可稍后手动执行：
    $CODE_CLI --install-extension $VSIX_FILE"
        fi
    else
        warn "下载 VS Code 插件失败（可能离线 / 镜像与官方均不可达）；可稍后手动安装。"
    fi
    rm -f "$VSIX_TMP" 2>/dev/null
fi

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
        # 用刚装好的编译器（保证「装的包」与「装的编译器」配套）
        if "$DST_BIN" install "$f"; then
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
