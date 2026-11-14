# dialc — Dial 语言编译器 v1.10

[![CI](https://github.com/shichencike/Dial/actions/workflows/ci.yml/badge.svg)](https://github.com/shichencike/Dial/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/shichencike/Dial?label=%E5%8F%91%E5%B8%83)](https://github.com/shichencike/Dial/releases)

> **GitHub Repository：** https://github.com/shichencike/Dial

> ## 开源许可：三重许可（任选其一）
>
> Dial 以 **GPL-3.0、MIT、Apache-2.0** 三份协议**同时**发布，用 `OR` 连接：
>
> ```
> GPL-3.0 OR MIT OR Apache-2.0
> ```
>
> **你可以任选其中一份遵守**，无需同时满足三者（详见 [`LICENSE`](LICENSE)）。
> 三份协议原文随源码与发布压缩包**完整分发**：[`LICENSE-GPLv3`](LICENSE-GPLv3)、
> [`LICENSE-MIT`](LICENSE-MIT)、[`LICENSE-APACHE`](LICENSE-APACHE)。
>
> 注意：本许可只覆盖 **Dial 编译器与标准库的源码**。你用 Dial **写出来的程序**
> 只链接 libc，不链接 LLVM 运行库，因此**不受本许可约束**（见 [§3.4 产物依赖契约](#34-产物依赖契约)）。

Dial 是一门**静态强类型、显式标注、无 GC** 的编译型语言。编译器前端（Rust + inkwell）
只负责把 Dial 源码翻译成 LLVM IR；**所有容器与运行时能力都由 C 语言实现**，
Dial 代码对这些能力的一切访问都降级为对 C 导出函数的调用。

本文件是 v1.10 的**规范性文档**：描述的语言行为与 `src/` 中的实现逐条对应，
不描述任何尚未实现的特性。凡文档中未出现的写法，即为不支持。

---

## 1. 设计原则

| 原则 | 含义 |
| --- | --- |
| **静态强类型，无推断** | 每个变量、参数、返回值都必须显式标注类型；不存在 `auto` / 类型推导 |
| **不透明指针（Opaque Pointer）** | `string` 与 `int[]` 在 IR 中只是一个 `ptr`；用户代码永远无法触碰其内存布局 |
| **C 承担一切容器** | 字符串、动态数组、哈希表、IO、内存管理全部由 `stdlib/` 与 `runtime/` 的 C 代码实现 |
| **编译期所有权优化** | Sema 阶段做数据流分析，尽可能把引用计数开销静态消除；只有静态分析失效时才由 C Runtime 的 COW 兜底 |
| **产物零 LLVM 依赖** | 用户编译出的可执行文件只依赖 C Runtime（libc），**绝不**依赖 libLLVM |
| **比 C 更严格** | 强制初始化、强制不可变性、禁止隐式窄化转换、禁止省略结构体字段——把 C 中大量"未定义行为"变成"编译期错误" |
| **无 GC 底线（内存宪法）** | 永不引入追踪式 GC；确定性内存管理靠 ARC + 编译期分析 + 显式裸指针（v1.02），见 §19 |
| **Fail-Fast 底线** | 宁可当场崩溃，绝不产生未定义行为或静默错误数据；越界/除零必须当场终止，见 §20 |
| **绝不引入可变全局状态** | **禁止任何 `var` 声明的全局变量**（严防数据竞争）；只允许**文件级 `const` 只读常量**（含标量字面量与数组字面量），编译期降维到 LLVM `.rodata` 段或按需内联，不参与运行期可变状态管理，见 §37。术语澄清：v1.04 之前的文档写作「绝不引入全局变量」，本轮统一改为本条更精确的表述——被禁止的是**可变共享状态**，而非只读常量数据 |
| **明确否决：函数名重载** | **永不提供**用户可见的同名函数重载。理由：C++ 式名字改编会让 ABI 随签名集合隐式漂移，与 Dial 的「显式整数版本号 + 链接期 ABI 拦截」契约相悖，见 §37.2 |
| **明确否决：闭包** | **永不提供**闭包 / 隐式环境捕获。回调状态必须显式经 `rawptr<void>` 透传（v1.03 起），见 §32 |

---

> **术语统一说明（v1.04 增补）**：README 与官网规范中凡出现「绝不引入全局变量」的旧措辞，
> 一律统一为「**绝不引入可变全局状态**」。该表述在 v0.61 等历史语境下同样成立
> （当时本就没有任何全局状态），因此历史条目也一并替换、不失真；沿革见 §37.1。

## 2. 目录结构

```
dialc/
├── src/                    # 编译器前端（Rust + inkwell）
│   ├── main.rs             #   驱动：两步构建（emit .o / 链接）
│   ├── lexer.rs            #   词法分析
│   ├── parser.rs           #   语法分析（递归下降 + 优先级）
│   ├── ast.rs              #   抽象语法树与类型定义
│   ├── semantic.rs         #   语义分析 + 数据流分析（移动/共享判定）
│   ├── codegen.rs          #   LLVM IR 生成（inkwell，内存中构建，直接 emit .o）
│   ├── index.rs            #   跨文件符号表 + 作用域树（LSP 用，v0.95）
│   ├── lsp.rs              #   语言服务器：诊断 / 语义高亮 / 悬停 / 跳转定义 / 补全
│   ├── package.rs          #   .dialpkg 解包 / .diali 接口解析 / SHA256 校验
│   └── error.rs            #   带行列号的错误类型
├── runtime/                # C 运行时（引用计数原语 + Fail-Fast 陷阱）
│   ├── dial_rt_mem.h / .c  #   refcount 分配 / retain / release / COW clone
│   └── dial_rt_trap.h / .c #   Fail-Fast：ud2 陷阱 + 越界/空指针/断言诊断（§20）
├── stdlib/                 # C 标准库（Dial 语言的实际能力来源）
│   ├── dial_string.h / .c  #   String
│   ├── dial_array.h / .c   #   Array<int>
│   ├── dial_map.h / .c     #   Map（int → int，ARC）
│   └── dial_io.h / .c      #   打印 / 读行
├── packages/               # 官方 .dialpkg 包（http / text / json / crypto / gui）
│   └── gui/                #   手搓 GUI 十五层：七层 C 契约 + 第 9/10/11/12/13/14/15 层（125 个 extern fn，合计 279 个）+ 第 8 层 Dial 侧 OO 封装（gui_oo.dial，278 个 impl 方法，随包 src/ 分发）（见 §38）
├── examples/               # 示例（*.dial + 同名 *.expected 黄金输出）
├── tests/
│   ├── run_tests.sh        # 端到端测试（A 正向 / B 负向 / C 契约 / D Fail-Fast / E 对照 /
│   │                       #   F 并发压测 / G 未捕获异常 / H 包分发 / I LSP / J 安装规则 /
│   │                       #   K LSP 增强 / L 包安装 / M LSP 全能力 + 协议流纯净性）
│   ├── bad/                # 必须编译失败的负向用例
│   ├── trap/               # 必须当场因信号终止的 Fail-Fast 用例
│   ├── pkg/                # 包端到端 demo（use_gui.dial / use_gui_widget.dial）
│   ├── lsp_smoke.py        # I 组：LSP 协议往返（诊断 + 语义高亮）；与 tools/lsp_full_smoke.py 是两个不同文件
│   ├── lsp_features.py     # K 组：LSP 增强（悬停 / 跳转定义 / 补全）
│   └── pkg_install_url.py  # L 组：包 URL 安装 + 一键安装脚本
├── tools/
│   ├── build_web.py        # 生成官网 docx/（规范 / 更新日志索引 + 每版本分页 / 首页）
│   ├── check_links.py      # 校验 README / CHANGELOG 的内部锚点（--fix 可自动修正）
│   ├── check_version.py    # 校验版本号在 README / CHANGELOG / 官网三处一致
│   ├── check_lsp_stdout_purity.py  # 校验 LSP 路径上不出现 stdout 裸输出（协议流纯净性）
│   ├── lsp_full_smoke.py  # M 组：LSP 全能力往返 + 协议流纯净性断言
│   ├── package.sh          # 打发布包 dialc-v<版本>-<平台>.zip（含许可与 .git）
│   ├── install-dial.sh     # 一键安装（Linux / Termux，支持 curl | bash 一键命令）
│   ├── install-dial.ps1    # 一键安装（Windows / PowerShell，支持 irm | iex 一键命令）
│   └── make_sample_pkg.sh  # 现场构建示例 .dialpkg（H / J / L 组用）
├── .github/workflows/ci.yml  # GitHub Actions：编译 + 二十八组测试（A–Z+AA+AB）+ 打包发布
├── editors/vscode/         # 免 npm 安装的 VS Code 扩展
├── docx/                    # 官网产物（由 tools/build_web.py 生成；更新日志分页在 docx/changelog/）
└── README.md               # 本文件
```

---

## 3. 构建与使用

### 3.1 运行环境要求（三平台）

Dial v0.97 起提供**三个平台**的预编译包，均为**原生构建**（CI 在各平台宿主上直接编译，
编译期目标即运行期平台，无交叉编译的 ABI 陷阱）：

| 平台 | 包 slug | 编译器 `dialc` 的运行依赖 | 你编出的程序 |
| --- | --- | --- | --- |
| **Linux x86_64** | `linux-x86_64` | 系统装 `libLLVM-18.so`（动态 glibc） | 仅依赖 libc（glibc） |
| **Windows x64（MinGW）** | `windows-x86_64` | 同目录的 `LLVM-C.dll`（随包分发） | 单文件 `.exe`，无外部依赖 |
| **Termux aarch64（musl）** | `termux-aarch64` | **无**（musl 全静态） | 单文件，无外部依赖 |

> 三个平台的 `dialc` 都通过 **LLVM 18 的 C 接口**与 LLVM 交互，因此产物（你写的 Dial 程序）
> 一律**不依赖 libLLVM**——这一契约在每条平台的 CI 里都做了断言（见 [§3.4](#34-产物依赖契约)）。

#### 3.1.1 Linux x86_64（动态 glibc）

要求系统使用 **glibc 动态链接**。`dialc` 编译器本体链接 `libLLVM-18.so`，因此运行期需要系统装有
LLVM 18 的开发库（提供 `libLLVM.so.18` 与 `llvm-config`）。近两年内发布、带 glibc 的 x86_64 发行版
（Ubuntu 24.04 LTS / Fedora 42 / Debian 13 / Arch 滚动等）均可。musl 系统（Alpine）**不适用于此包**——
请用下面的 Termux/musl 包（它已在 Alpine aarch64 上构建并静态链接）。

#### 3.1.2 Windows x64（MinGW）

- `dialc.exe` 走 **MinGW 目标**（`x86_64-pc-windows-gnu`），运行期需要同目录的 **`LLVM-C.dll`**
  （由官方 MSVC 构建的 LLVM 18 提供，纯 C ABI，跨工具链无冲突；该 DLL 已随包分发）。
- 用 `dialc` 编译你的 `.dial` 程序时，需要 **MinGW-w64 工具链**（`gcc` + `winpthreads`）。
  标准库的 `pthread` 代码由 winpthreads 提供，因此 **Windows 上无需改写 Runtime**，
  产物为**单文件 `.exe`**（winpthreads 已静态链入）。
- 推荐安装 [MSYS2](https://www.msys2.org/) 并执行 `pacman -S mingw-w64-x86_64-toolchain`，
  或在 PATH 中放一份 MinGW-w64 `gcc`。

#### 3.1.3 Termux aarch64（musl 全静态）

- 在 **Alpine aarch64** 容器内原生构建，**完全静态链接 musl libc + 静态 LLVM**，
  得到的是**单文件、零外部依赖**的 `dialc`，可在 Termux、任意 ARM64 Linux 上直接运行。
- 用 `dialc` 编译你的程序只需本机装有 `gcc` / `clang`，产物同样无外部依赖。

#### 3.1.4 通用工具链

| 组件 | 版本要求 | 用途 | 备注 |
| --- | --- | --- | --- |
| **Rust 工具链**（`cargo` / `rustc`） | stable（≥ 1.80） | 编译 `dialc` 本体 | 用 `rustup` 安装最省事 |
| **LLVM 18** | 18.x | `dialc` 通过 C 接口（`LLVM-C`）使用 | Linux 需 `libLLVM-18.so` + `llvm-config`；Windows 随包含 `LLVM-C.dll`；Termux 静态链入 |
| **C 编译器**：`zig cc` 优先，缺失时回退 `clang` / `gcc` | zig ≥ 0.11 **或** clang ≥ 18 **或** gcc（MinGW） | 链接阶段把 `stdlib/` + `runtime/` 编成 `libdial_std.a` 并链接 | 见 [§3.3](#33-编译-dial-程序) |

安装示例（Ubuntu 24.04，构建 Linux 包）：

```bash
# LLVM 18（含 libLLVM.so 与 clang）
sudo apt-get update
sudo apt-get install -y llvm-18-dev clang-18 libclang-18-dev

# Rust（若尚未安装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 链接阶段用的 C 编译器：优先 zig；装了就无需 clang
#   sudo apt-get install -y zig        # 可选，未装则自动回退 clang-18
```

#### 3.1.5 运行 `dialc` 本体时的动态库要求（Linux）

`dialc` 自身依赖 `libLLVM.so.18`。如果该库不在系统默认库路径里，运行 `dialc` 前需要把它加进
`LD_LIBRARY_PATH`（**仅影响编译器，不影响你编出来的程序**）：

```bash
export LD_LIBRARY_PATH=/usr/lib/llvm-18/lib:$LD_LIBRARY_PATH   # 按本机路径调整
dialc --version
```

> 把 `libLLVM.so.18` 装进 `/usr/lib` 或 `/usr/lib/x86_64-linux-gnu` 等默认路径后，
> 这一行就可以省掉——它只是给「编译器启动」用的，与最终产物的依赖无关。

### 3.2 从源码构建：克隆 → 进入 → 编译

```bash
# 1) 克隆源码
git clone https://github.com/shichencike/Dial.git
cd Dial                                  # 进入工程根目录（编译器本体就在这里：Cargo.toml 与 src/ 都在根）

# 2) 编译（编译器本体就位于仓库根，无需再进子目录）
export LLVM_SYS_181_PREFIX=/usr/lib/llvm-18   # 指向本机 LLVM 18（见 §3.1.1）
cargo build --release                          # 产物：target/release/dialc

# 3) 让编译器本体能找到 libLLVM（见 §3.1.5）
export LD_LIBRARY_PATH=/usr/lib/llvm-18/lib:$LD_LIBRARY_PATH

# 4) 验证
./target/release/dialc --version
```

`dialc` 自身链接 libLLVM——这是**编译器**的依赖，与用户程序无关。

### 3.2.1 一键安装（v0.97 起，三平台）

不想自己编译？每个平台有独立的预编译包 `dialc-v<版本>-<slug>.zip`，内含编译器、标准库、运行时
与一键安装脚本（ARCHITECTURE.md §3.2 阶段 5）：

#### 一行命令安装（推荐）

直接复制下面一行即可，脚本会自动探测平台、下载对应发布包（镜像优先 + 官方回退 + 断点续传）并完成安装：

```bash
# Linux x86_64 / Termux aarch64（默认始终跟随最新 Release）
curl -fsSL https://raw.githubusercontent.com/shichencike/Dial/main/tools/install-dial.sh | bash
# 需要钉到某个版本时才设置 DIAL_VERSION：
# DIAL_VERSION=1.1 curl -fsSL https://raw.githubusercontent.com/shichencike/Dial/main/tools/install-dial.sh | bash
```

```powershell
# Windows x64（MinGW）：一行式从 main 分支拉取并安装（输出为英文，避免终端代码页乱码）
irm https://raw.githubusercontent.com/shichencike/Dial/main/tools/install-dial.ps1 | iex
```

> **关于 URL 稳定性与「零硬编码」**：这两条命令取的是 **`main` 分支上 `tools/` 里的安装脚本**（直出文件，脚本本身不走任何重定向）。脚本运行时通过 GitHub API 读取**最新 Release 的真实资产清单**，再用
> `https://github.com/shichencike/Dial/releases/latest/download/<资产名>` 下载——版本、标签、资产名**都不写死在脚本里**，发新版后无需改脚本即可自动跟上。
>
> 之所以仍需解析资产名：GitHub 的 `latest/download/` 要求**精确文件名**，而本项目的 zip / vsix 名字带版本号（`dialc-v1.10-linux-x86_64.zip`、`dial-1.10.0.vsix`），靠重定向本身推不出来；脚本按平台 slug 与扩展名从清单里挑，`stdlib.dialpkg` 名字固定、且现在与 zip / vsix 挂在**同一个 `v*` Release 页面**，因此同样能被 latest 找到。
>
> 需要复现旧环境或离线时，用 `DIAL_VERSION`（钉编译器版本）、`STDLIB_RELEASE`（钉旧 `stdlib-v*` 通道）、`STDLIB_URL` / `DIAL_VSIX_URL`（完全覆盖地址）即可；镜像优先 + 官方回退 + 失败原因可见的行为不变。安装脚本本身也随每个发布包一起分发。
>
> 注意用 `bash` 而非 `sh`：`set -o pipefail` 与脚本里的数组语法在 Ubuntu 的 `sh`(dash)、Termux 的 `mksh` 下不被支持。Windows 侧用 `iex` 即可（PowerShell 原生）。

#### 手动解压（三平台通用，可选）

若不想用管道，可先下载再运行：

```bash
# Linux x86_64
curl -LO https://github.com/shichencike/Dial/releases/latest/download/dialc-v1.10-linux-x86_64.zip
unzip dialc-v1.10-linux-x86_64.zip && cd Dial
bash install-dial.sh                 # 扫描当前目录及其子目录
bash install-dial.sh /path/to/pkgs   # 或扫描指定目录
```

```bash
# Termux aarch64（musl 全静态，包名 slug = termux-aarch64）
curl -LO https://github.com/shichencike/Dial/releases/latest/download/dialc-v1.10-termux-aarch64.zip
unzip dialc-v1.10-termux-aarch64.zip && cd Dial
bash install-dial.sh
```

```powershell
# Windows x64（MinGW）：解压后运行 PowerShell 安装脚本
curl.exe -LO https://github.com/shichencike/Dial/releases/latest/download/dialc-v1.10-windows-x86_64.zip
Expand-Archive dialc-v1.10-windows-x86_64.zip -DestinationPath .
powershell -ExecutionPolicy Bypass -File Dial\install-dial.ps1
```

#### 下载源与回退

安装脚本下载标准库 / VS Code 插件时，**默认先走国内镜像 `ghproxy.net` 前缀代理，失败自动回退官方 GitHub**，
并对大文件支持断点续传（`curl -C -` / `wget -c`），网络不稳也不会「一键卡住」。可用环境变量微调：

| 环境变量 | 作用 |
| --- | --- |
| `DIAL_MIRROR_PREFIX` | 镜像前缀（默认 `https://ghproxy.net/`）；置空则只用官方源 |
| `STDLIB_URL` | 覆盖官方标准库下载地址（镜像 / 自建源） |
| `DIAL_VSIX_URL` | 覆盖 VS Code 插件下载地址 |
| `DIAL_VERSION` | 指定配套件版本（默认**空**：跟随最新 Release；钉版本写法如 `DIAL_VERSION=1.1`，对应发布标签 `v1.1`） |

脚本承担两件事（**顺序不能反**）：

1. **初始化 Dial 环境**：建 `~/.dial/` 目录树（`bin` / `packages` / `cache/downloads`）、
   把 `dialc` 装进 `~/.dial/bin/` 并挂进 `PATH`、把 `stdlib/` 与 `runtime/` 一并装到
   `~/.dial/`、写一份 `~/.dial/config.txt` 工具链配置；
2. **自动安装已下载的包**：初始化完成后**重新扫描**找到的 `.dialpkg`（当前目录及其子目录，
   或参数指定的目录），逐个执行 `dialc install`。

> **为什么脚本要「自动重新扫描」而不是让用户逐个点名**：一键安装的语义是
> 「我把这个目录交给你，你把该装的都装上」。用户不该需要记住包名、也不该需要
> 为每个包敲一条命令——包就在那里，脚本看得见。

> **为什么 `stdlib/` 与 `runtime/` 也要装**：编译器按**可执行文件所在位置**向上找
> `stdlib/` 与 `runtime/`（`target/release/dialc` → 工程根）。装到 `~/.dial/bin/dialc` 后，
> 那个位置正好是 `~/.dial/`——所以把它们放这里即可。少了这一步，
> 用户在自己项目里编译会报「找不到 stdlib/ 与 runtime/ 目录」。

装完之后，在**你自己的项目目录**里直接用：

```bash
export PATH="$PATH:$HOME/.dial/bin"   # 脚本已写入 ~/.profile / ~/.bashrc，重开终端后自动生效
dialc build main.dial -o main
```

### 3.2.2 命令行用法总览

```bash
dialc build <源文件.dial> [-o 可执行文件] [--no-link] [--safety debug|safe|fast] [--tiny] [--single-file] [--backend llvm|asm]
dialc run   <源文件.dial> [-o 可执行文件]              [--safety debug|safe|fast] [--tiny] [--single-file] [--backend llvm|asm]
dialc install <包.dialpkg | URL> [--sha256 <哈希>]
dialc version | --version | -V
dialc lsp
```

| 命令 | 作用 |
| --- | --- |
| `build` | 编译并链接出可执行文件。`--no-link` 只产出 `.o` 不链接 |
| `run` | 编译后**直接运行**（等价于 build + 执行） |
| `install` | 安装 `.dialpkg` 包；给 URL 时先下载，再做整包哈希与逐文件 SHA256 校验（见 [§25](#25-包分发与依赖管理生态规划--最终规范)） |
| `version` / `--version` / `-V` | **打印版本号**并退出，例如 `dialc 1.10.0` |
| `lsp` | 启动语言服务器（走 stdio 的 JSON-RPC；**不由人手敲**，见 [§28](#28-语言服务器-dialc-lspv094-骨架--v095-增强--v098-工业化)） |

> **自研后端 `--backend=asm`（实验性，默认 llvm）**：不指定时走既有 LLVM 后端，
> 行为与旧版逐字节一致。加上 `--backend=asm`（也可写 `--backend asm`）改用自研后端：
> 专属 IR → 优化 Pass（常量折叠 / DCE / CSE / 保守 ARC 简化）→ x86-64 SysV 或
> AArch64 AAPCS 汇编 `.s` → 系统 `cc -c` 汇编成 `.o` → 复用既有链接流程。
> **不发射 C 源码、不引第三方代码生成库、零新包依赖**；FFI 包、`--tiny`、
> `--single-file` 因复用链接层而零改动生效。未覆盖的语言构造会明确报
> `[asm 后端] ...` 编译错误，绝不静默降级。`asm` 后端拒绝 `--pgo`、忽略 `-g`。
> 双后端黄金门禁见 `tools/run_asm_golden.sh`（`run_tests.sh` 的 AF 组）。
>
> **当前覆盖范围**（M1–M5 已落地）：
>
> | 批次 | 覆盖内容 |
> | --- | --- |
> | M1 | 内建函数**表驱动**分派（`src/builtin.rs`）：math / 类型转换 / 解析 / 环境 / 时间 / bytes / 容器构造等约 55 个 |
> | M2-a | 索引赋值 / 切片 / `match` 字符串分支 / `ForEachMap` 遍历 |
> | M2-b | 方法**表驱动**分派：全部内建方法（含容器 COW 写回、`kv()`、`eret()`、`one()`、`earg()`） |
> | M3-a | 控制流与异常：`defer` / `try`–`catch` / `throw`（运行时 setjmp/longjmp + 异常栈）、文件 IO 内建（失败抛 `Error`）、`with` 自动释放 |
> | M3-b | `label` / `goto`（含跨作用域释放，深度表与 LLVM 后端同源核对） |
> | M5-a | 结构体**按值语义**（形参 / 返回值 / 嵌套字段 / 值拷贝隔离） |
> | M5-b | 容器元素类型与所有权结算（`string[]` / `bool[]` / `float[]` 的 kind 与位模式） |
> | M5-c | 裸指针 FFI：`raw_alloc` / `raw_free` / `ptr_cast` / `offset` 指针算术 / `*p` 读写 / C 库互通 |
> | M5-d | 一等**函数地址**（函数指针）：取地址 / 间接调用 / 作参数与返回值 |
>
> 尚未覆盖（会明确报错）：`select` / `spawn` / 通道并发、编译期布局内建
> （`sizeof` / `alignof` / `offsetof`）、内联汇编与 SIMD、高阶容器方法
> （`map` / `filter` / `reduce` / `sort_by`）、嵌套下标赋值等。
>
> **双后端黄金门禁**（`tools/run_asm_golden.sh`，`run_tests.sh` 的 AF 组）：
> 同一份 `.dial` 分别用 `llvm` / `asm` 编译运行并**逐字节 diff**——LLVM 后端即基准，
> 无需维护 `.expected`。正向用例 10 份（`core1`–`core3` 核心子集 + `builtins`
> 内建与方法表 + `exceptions` 异常与文件 IO + `control_flow` `label`/`goto` +
> `structs` 结构体按值语义 + `containers` 容器元素类型 + `rawptr` 裸指针 FFI +
> `fnptr` 一等函数地址）；
> 负例在 `tests/asm_golden/negative/` 下逐个遍历，每个都要求「LLVM 能编译、
> asm 报错且带 `[asm 后端]` 前缀」，杜绝静默降级。用例在临时目录编译，仓库零污染。
>
> **性能基准**（`tools/bench_asm_vs_llvm.py`）：用 `bench/bench.dial` 的 28 项
> 跨语言等价微基准，让两个后端在**同一份源码、同一套计时口径**（校准重复数 →
> 扣启动基线 → 多次取最小）下直接对跑，并以**内嵌校验和锚点**保证比的是等价
> 计算而非「一边算错所以更快」。当前首个可信基线（几何平均 asm/llvm）：
> **1.3575x**，3 胜 2 平 23 负；最短板是位运算密集循环（b23 popcount 4.758x）。
> 自研后端目前是「全 spill 固定栈帧」（每个中间值都落栈、无寄存器分配），
> 热点循环里约八成指令是栈槽搬运——这是与 LLVM 的主要差距，也是后续优化主战场。

> **第 18 层两种产物形态**（工具链层，非 `.dialpkg` 契约）：`--tiny` 体积模式
> （C 侧 `-Os -ffunction-sections -fdata-sections` + 链接期 `-Wl,--gc-sections -s`，
> 去掉 DWARF 与符号表）、`--single-file` 便携模式（链接期 `-static` 全静态，
> 产物不依赖宿主动态 libc）。详见 [§38.24.1](#38241-编译器---tiny体积模式---single-file便携模式) 与
> [§3.4.2](#342---single-file显式全静态链接第-18-层)。

> **版本号只有一个出处**：`dialc version` 打印的版本直接取自 `Cargo.toml` 的 `version` 字段
> （编译期注入），与官网、CHANGELOG、发布包名**同源**——
> 任何一处手写版本号都必然随发布漂移（这仓库已经因为硬编码漂过三轮）。
> 校验脚本见 `tools/check_version.py`（CI 强制执行）。

### 3.3 编译 Dial 程序

```bash
dialc build <源文件.dial> [-o 可执行文件] [--no-link] [--safety debug|safe|fast] [--tiny] [--single-file] [--backend llvm|asm]
dialc run   <源文件.dial> [-o 可执行文件]              [--safety debug|safe|fast] [--tiny] [--single-file] [--backend llvm|asm]
```

安全等级（Fail-Fast，详见 §20）：

| 参数 | 等级 | 行为 |
| --- | --- | --- |
| （默认） | Debug | 崩溃保护全开，陷阱用 `llvm.debugtrap`（x86 `int3`） |
| `--release` / `--safety safe` | ReleaseSafe | 保留防 UB 检查，陷阱用 `llvm.trap`（x86 `ud2`） |
| `--fast` / `--safety fast` | ReleaseFast | 关闭全部检查，越界/除零即 UB（与 C 一致）——⚠️ **危险，详见下方警告** |

等级同时作用于 LLVM IR 与 C Runtime（`-DDIAL_SAFETY=N`），两侧口径一致。

产物形态开关（第 18 层，与上面的安全等级**正交**）：

| 参数 | 形态 | 行为 |
| --- | --- | --- |
| `--tiny` | 体积模式 | C 侧 `-Os -ffunction-sections -fdata-sections`、链接期 `-Wl,--gc-sections -s`（strip 掉 DWARF 与符号表），并强制走源码编译 stdlib 回退路径（绕开预编译 `-O2` 归档）；与 `--fast` / `--pgo` / `-g` 目标相反，一律 **Fail-Fast 拒绝**（见 §38.24.1） |
| `--single-file` | 便携模式 | 链接期 `-static` 全静态链接，产物 `file` 显示 `statically linked`、`ldd` 显示 `not a dynamic executable`；可与 LTO / PGO / `-g` 叠加（见 §3.4.2） |

两者可叠加，但 `-static` 会把 libc 静态段拉入产物，通常比 `--tiny` 单独使用更大——
前者求「最小体积」、后者求「不依赖宿主」，编译器如实提示但不静默覆盖任何一侧。
LLVM 侧（inkwell 无原生 `Oz` 档）保持 `O2`，体积压缩全部压在 C 侧与链接期；
**不承诺具体 KB 数**，实测数据见 §38.24.1。

> ⚠️ **关于 `--fast` 模式（ReleaseFast）：此模式会关闭所有边界检查、除零检查和空指针保护，
> 以换取极致的性能。除非你是正在编写底层核心算法、且完全确信你的数组下标绝对不会越界，
> 否则请永远不要使用此模式。在 `--fast` 下发生的越界，将退化为 C 语言的未定义行为（UB），
> 可能导致静默的错误数据甚至灾难性后果。**
> （详见 [§20.4](#204-性能模式的切换)）

`--fast` 不只是「关掉检查」——它同时开启三层编译器终极优化（均需本机存在 `clang`，
否则优雅降级为「仅 O3 + SIMD」）。详见示例 [`examples/bench_array.dial`](examples/bench_array.dial)：

| 优化 | 作用 | 验证方式 |
| --- | --- | --- |
| **① 自动向量化 (SIMD) + 循环展开** | LLVM 用 `O3` + 宿主 SIMD（x86 的 AVX2/AVX-512、ARM64 的 NEON）一次吃 4~8 个 int64，并展开循环减少分支 | 反汇编可见 `ymm`/`zmm` 向量指令 |
| **② LTO 链接时优化** | 把 Dial 代码与 C 运行时的 bitcode 整体重排，`dial_array_get`/`push` 等热函数被直接内联进主程序，消灭函数调用开销 | 反汇编中热循环 0 次 `call dial_array_*` |
| **③ PGO 剖析引导优化**（`--pgo`） | 先插桩跑一遍训练样本记录最热分支，再据热点把热路径顶到 I-Cache 最前、冷错误后置 | 构建日志出现「PGO 优化完成」 |

> 注意：`--fast` 生成的二进制按**本机 CPU 微架构级别**（`x86-64-v3`/`-v4` 或 NEON）调优，
> 仅供本机运行，请勿跨机型分发（否则可能 `SIGILL`）。

`dialc build` 严格分成两步，这是架构约束而非实现细节：

| 步骤 | 负责组件 | 是否需要 LLVM | 产物 |
| --- | --- | --- | --- |
| **第一步：代码生成** | inkwell 在**内存中**构建 IR，再由 `TargetMachine::write_to_file` 直接写出机器码 | **需要** | `<源文件>.o` |
| **第二步：链接** | `zig cc`（缺失时回退 `clang`）把 `stdlib/` 与 `runtime/` 的 C 代码编译并归档为 `libdial_std.a`，再与用户 `.o` 链接 | **不需要** | 可执行文件 |

要点：

- 第一步不导出 `.ll` 文本、不调用外部命令，IR 全程在内存中。
- 第二步完全交给 `zig cc`，此时 LLVM 已退出舞台。
- `--no-link` 只执行第一步，便于交叉编译或自行接管链接。
- 中间产物落在工程根的 `.dialc_build/`（C 的 `.o` 与 `libdial_std.a`）。

### 3.4 产物依赖契约

用户编译出的可执行文件**只允许**依赖 C Runtime：

```bash
$ ldd examples/hello
    linux-vdso.so.1
    libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6
    /lib64/ld-linux-x86-64.so.2
```

`tests/run_tests.sh` 的 C 组会把"产物不得依赖 libLLVM"作为**硬断言**执行。该契约在三条平台
（Linux / Windows / Termux）的 CI 里都做了断言：Windows 用 `llvm-readobj --coff-imports` 检查
用户程序不导入 `LLVM-C.dll`，Termux 用 `readelf -d` 检查产物无动态依赖，Linux 用 `ldd` 检查无 libLLVM。

> **显式动态链接的例外（v1.04）**：源码里写 `link "xxx.so";` 会主动引入第三方
> 动态依赖（§33.8）。这是源码作者的显式选择，不属于**默认产物**契约；C 组检查的
> 是 `examples/hello.dial` 这类不带 `link` 的程序，仍严格只链 libc。

#### 3.4.1 许可隔离：标准库绝不以 GPL 链接进用户程序（防法务传染）

上表的 `libc` 之外还有一个**被静态链入**用户产物的组件：**`libdial_std.a`**
（`stdlib/` 与 `runtime/` 编译归档而成的 C Runtime，见 [§15](#15-ffi-契约dial-语法--c-函数)）。
它随每一个 Dial 程序一起进入最终可执行文件，因此它的许可条款**直接作用于用户的程序**：

> **Dial 编译器前端（`dialc` 本体）采用三重许可：`GPL-3.0 OR MIT OR Apache-2.0`，任选其一。**
> **但 Dial 的标准库（`libdial_std.a`）必须额外豁免，并强制锁定为 `MIT` 或 `Apache-2.0` 二选一；
> 绝对不允许以 GPL 协议的条款把它链接进用户的程序（尤其是闭源程序），以避免法务传染。**

为什么必须这样切分：

| 事实 | 后果 |
| --- | --- |
| `libdial_std.a` 是**静态链接**进用户可执行文件的 | 它构成用户程序的**一部分**，用户的产物是它的衍生作品 |
| **GPL-3.0 具有传染性** | 若标准库按 GPL-3.0 授权，**任何**链接它的程序（含闭源商业软件）都必须整体以 GPL-3.0 开源 |
| 而用户只是想用一门语言写程序 | 一旦被传染，等于**强制公开自己的全部源码**——这事实上否决了 Dial 的一切闭源与商业用途 |

因此本项目的执行口径是**按组件划分**，而非全仓库一刀切：

| 组件 | 许可 | 理由 |
| --- | --- | --- |
| **编译器前端**（`src/`，`dialc` 本体） | `GPL-3.0 OR MIT OR Apache-2.0` 三重许可 | 它只是**工具**，不进入用户产物，无传染风险 |
| **标准库 / C Runtime**（`stdlib/`、`runtime/` → `libdial_std.a`） | **强制 `MIT` 或 `Apache-2.0`** | 它被**静态链入**用户程序，必须免传染 |
| 用户用 Dial 写出的程序 | **不受 Dial 许可约束** | 前提是上述隔离成立；用户的代码是用户自己的 |

> **一句话：工具可以是 GPL，被链进你程序里的运行时绝不能是。**
> 为标准库选择许可时，请在 MIT 与 Apache-2.0 之间选，不要选 GPL-3.0。

#### 3.4.2 `--single-file`：显式全静态链接（第 18 层）

默认产物按本节开头那张表动态链 libc。加 `--single-file` 后，链接期追加 `-static`，
产物不再依赖宿主动态 libc：`file` 显示 `statically linked`，`ldd` 显示
`not a dynamic executable`。这是**用户显式选择**的产物形态，与 3.4.1 的许可隔离
不冲突——`libdial_std.a` 仍是 MIT / Apache-2.0 豁免，只是 libc 由动态改为静态链入。

> **实现要点（避坑记录）**：实测 `zig cc -static` 对 glibc 目标无效（纯 C 最小实验
> 仍产出动态可执行文件），故 `--single-file` **仅在链接阶段**把驱动切到系统
> `gcc` / `clang` / `cc`（`static_link_cc()`，优先尊重 `DIAL_C_COMPILER`），
> 代码生成与 C Runtime 编译仍走原路径；`-static` 下跳过 `-Wl,-rpath`。
> 实验确认 `gcc -static` 可链接 `zig cc -c` 编出的 `.o`。与 GUI 包的 `dlopen`
> 运行期动态装载后端**可共存**——静态链接不影响运行期装载系统库。

本机实测（`examples/hello.dial`，x86_64-linux-gnu）：

| 形态 | 体积 | 链接方式 | 关键证据 |
| --- | --- | --- | --- |
| 默认 | `129776 B` | 动态 | `ldd` 列 libc |
| `--tiny` | `7024 B` | 动态 | `readelf -S` 无 `.symtab` / `.debug_info` |
| `--single-file` | `1077568 B` | 静态 | `file` = `statically linked`、`ldd` = `not a dynamic executable` |
| `--tiny --single-file` | `719768 B` | 静态 | 两者叠加，仍可运行 |

详见 [§38.24.1](#38241-编译器---tiny体积模式---single-file便携模式)；AE 组把这四条形态、
冲突矩阵与正交叠加全部做成回归断言。

---

## 4. 词法结构

### 4.1 注释

`//` 到行尾。无块注释。

> **`///` 文档注释不是语言层语法。** 三斜杠 `///` **仅出现在 `.diali` 接口签名文件里**（标注包导出函数的文档，供 LSP 悬停提示读取，见 [§25.2](#252-包内结构与接口签名供-lsp-与-sema-使用)），**不能**写在 `.dial` 源码中——Dial 源码只有 `//` 一种注释。

### 4.2 标识符

以字母或 `_` 开头，后接字母、数字或 `_`。

### 4.3 关键字

```
var  let  fn  struct  if  else  while  for  in  return
try  catch  throw
select  case  default  as
and  or   not
true false
int  float  bool  char  string  void  file  error  thread  channel
```

> **以上仅限 `.dial` 源码的关键字。** 接口签名文件 `.diali` 另有 `type` / `extern fn` 等**专用语法**（见 [§25.2](#252-包内结构与接口签名供-lsp-与-sema-使用)），它们**不是** Dial 语言的关键字，由包解析器 `src/package.rs` 单独处理，不会出现在 `.dial` 源码里。

> `char` 是 v0.92 引入的单字符类型：**一个 Unicode 码点**，底层与 `int` 同为 `i64`，
> 但类型独立——不参与算术，只可比较或经 `char_to_int` / `int_to_char` 显式转换
> （见 [§5.1](#51-基本类型) 与 [§17.2](#172-尚未实现的能力截至-v110)）。

> `try` / `catch` / `throw` 是 v0.7 引入的可恢复异常关键字（见 [§26](#26-可恢复异常trycatchthrow)）。
> 注意 **`error` 不是关键字**——它是内建构造函数名（与 `open` / `print` 同类），写作 `Error("...")`。
> 同理 `catch` 后**不跟圆括号**：`catch e { }`，而非 `catch (e) { }`。
>
> `thread` / `channel` 在**词法上也不是关键字**，只是 `parse_type` 对标识符的特判——
> 否则 `channel(8)` 这个内建调用会被当成类型名而无法解析。并发内建（`spawn` / `join` /
> `detach` / `channel` / `recv` / `move_to`）全部按名字匹配，**不能用作函数名或结构体名**
>（见 [§27](#27-所有权转移与多线程并发move_to--thread--channel)）。

### 4.4 字面量

| 类别 | 写法 | 说明 |
| --- | --- | --- |
| 整数 | `123` | 十进制，对应 `int`（i64） |
| 浮点 | `3.14` | 必须含小数点，对应 `float`（f64） |
| 布尔 | `true` / `false` | 对应 `bool` |
| 字符串 | `"hello"` | 双引号；转义仅支持 `\n` `\t` `\\` `\"` |

### 4.5 运算符与标点

```
+  -  *  /  ==  !=  <  >  <=  >=  =  .  [  ]  ;  ,  (  )  {  }  :
```

> **注意**：单独的 `!` 是词法错误。逻辑非必须写关键字 `not`。

---

## 5. 类型系统

### 5.1 基本类型

| 类型 | 底层表示 | 说明 |
| --- | --- | --- |
| `int` | LLVM `i64` | 整数 |
| `float` | LLVM `double` | 双精度浮点 |
| `bool` | LLVM `i1` | 布尔 |
| `char` | LLVM `i64` | **单个 Unicode 码点**（v0.92）。**值类型**：无 ARC、无堆分配、无空指针。与 `int` 同表示但**类型独立**——不参与 `+ - * /`，只可比较，或经 `char_to_int` / `int_to_char` 显式转换。字面量写作 `'a'`、`'\n'`、`'中'` |
| `byte` | LLVM `i64` | **单字节无符号值**（v0.99）。**值类型**：与 `char` 同表示但语义不同——`byte` 是 0~255 的**数值**，参与算术、比较，经 `int_to_byte` / `byte_to_int` 与 `int` 显式互转；与 `char`（码点）严格区分，避免「一个字节到底是字符还是数字」的歧义 |
| `int32` | LLVM `i32` | **32 位有符号整数**（v1.10 开发中增补）。FFI 精确宽度标量：与 `int`(i64) 严格区分、零隐式转换，经 `int_to_int32` / `int32_to_int` 显式互转；`int_to_int32` 越出 `[-2^31, 2^31-1]` 当场 Fail-Fast。用于 C 侧 `int32_t` 参数 / 返回值 / 结构体字段（见 §5.8） |
| `float32` | LLVM `float`（f32） | **32 位浮点**（IEEE 754 binary32，v1.10 开发中增补）。与 `float`(f64) 严格区分，经 `float_to_float32` / `float32_to_float` 显式互转；FFI 边界按 C 的 `float` 传递，绝不把 `double` 的位模式递给 C（见 §5.8） |
| `string` | LLVM `ptr`（不透明） | 由 C Runtime 管理的引用计数字符串 |
| `bytes` | LLVM `ptr`（不透明） | 不可变二进制缓冲区（v0.99），由 C Runtime 管理的引用计数对象，**独立于 `string`**（零隐式转换）。承载任意字节序列（可能非法 UTF-8）。构造/转换见 [§5.7](#57-bytes二进制缓冲区v099) |
| `int[]` | LLVM `ptr`（不透明） | 由 C Runtime 管理的动态数组。**元素类型可为 `int` 或任意 8 字节句柄**（见 [§11](#11-动态数组-int)）：`string[]` / `int[][]` / `map[]` / `error[]` / `channel[]` / 不透明指针数组；`float[]` / `bool[]` / `byte[]` / `char[]` 等标量非 int 元素在编译期拒绝（见 [§5.2](#52-类型良构性约束)） |
| `file` | LLVM `ptr`（不透明） | 文件句柄，封装 C 的 `FILE*`；显式 `open` / `close`，**不受 ARC 管理**（见 [§24](#24-文件-iofile-句柄)） |
| `error` | LLVM `ptr`（不透明） | **可恢复异常对象**（v0.7），由 C Runtime 管理的 ARC 类型；由 `Error("msg")` 构造、`throw` 抛出、`catch` 捕获（见 [§26](#26-可恢复异常trycatchthrow)） |
| `thread` | LLVM `ptr`（不透明） | 线程句柄（v0.8）；显式 `join` / `detach`，**不受 ARC 管理**（见 [§27](#27-所有权转移与多线程并发move_to--thread--channel)） |
| `channel<T>` | LLVM `ptr`（不透明） | 通道句柄（v0.8），元素类型仅编译期可见；跨线程共享 → **原子**引用计数 ARC。元素只能是 `string` / `int[]` / `map` / `error` / `channel`（见 [§27](#27-所有权转移与多线程并发move_to--thread--channel)） |
| `void` | LLVM `void` | **仅可用作函数返回类型** |
| 结构体 | LLVM 具名 struct | 值类型，栈分配 |

### 5.2 类型良构性约束

以下写法在编译期即被拒绝（而非运行期静默降级）：

| 写法 | 结果 |
| --- | --- |
| `float[]` | ❌ 错误。数组元素只支持 `int` 与 8 字节句柄（string / int[] / map / error / channel / 不透明指针）；`float` 是 double，位模式塞进 int64 槽会失真，必须拒绝（见 [§5.1](#51-基本类型)） |
| `bool[]` / `byte[]` / `char[]` | ❌ 错误。标量（1/4 字节）无法塞进数组的「8 字节槽位」，编译期拒绝（见 [§5.1](#51-基本类型)） |
| `int32[]` / `float32[]` | ❌ 错误。32 位标量无法塞进数组的「8 字节槽位」，编译期拒绝（与 `float[]` / `bool[]` 同理，见 §5.1） |
| `int[][]` | ✅ **支持**（v0.99）：多维数组 =「元素是数组句柄的数组」，内层 `int[]` 作为 8 字节句柄统一管理（见 [§11.6](#116-多维数组与句柄元素数组)） |
| `var v: void = ...` | ❌ 错误。`void` 不能作为变量类型 |

> 之所以在编译期拒绝 `float[]` / `bool[]` / `byte[]` / `char[]`：C 侧 `dial_array_*` 以 `int64` 存储元素，
> 数组槽位语义是「int 或 8 字节不透明句柄」。标量浮点 / 布尔 / 字节 / 字符无法塞进这一模型：
> 要么位模式失真（`float`），要么宽度不足（`bool` / `byte` / `char`）。宁可拒绝，不可静默损坏数据。
> 而 `string[]` / `int[][]` / `map[]` / `error[]` / `channel[]` 等句柄天然是 8 字节指针，直接存储即可（见 [§11.6](#116-多维数组与句柄元素数组)）。

### 5.3 类型转换

**零隐式转换铁律。**

Dial **不存在任何**隐式类型转换——既没有 `int → float` 的隐式提升，也没有 `float → int` 的隐式窄化，
更没有 `int`/`float` 在算术或比较中的隐式混用。两个操作数只要类型不完全相同，就一律编译错误。
这比 C 更严格：C 允许 `1 + 2.0` 把 `int` 默默提升为 `double`，正是大量隐蔽精度 bug 的温床，
Dial 在编译期即切断。

唯一的合法转换通道是**命名清晰的内置函数**，它们把"发生过转换"这件事明明白白写出来：

| 内置函数 | 签名 | 编译为 | 说明 |
| --- | --- | --- | --- |
| `int_to_float` | `int_to_float(x: int): float` | LLVM `sitofp` | 整数 → 双精度浮点 |
| `float_to_int` | `float_to_int(x: float): int` | LLVM `fptosi` | 浮点 → 整数（向零截断，与 C 的 `(int)` 一致） |
| `char_to_int` | `char_to_int(c: char): int` | **无指令**（恒等） | 字符 → 码点（v0.92） |
| `int_to_char` | `int_to_char(n: int): char` | **无指令**（恒等） | 码点 → 字符（v0.92）；非法码点在**打印时**才会被 C Runtime 拦截 |
| `int_to_int32` | `int_to_int32(x: int): int32` | C 侧 `dial_int_to_int32` | 64→32 位；越出 `[-2^31, 2^31-1]` **当场 Fail-Fast**，绝不静默截断成另一个「合法」值 |
| `int32_to_int` | `int32_to_int(x: int32): int` | LLVM `sext` | 32→64 位符号扩展，精确、零信息损失 |
| `float_to_float32` | `float_to_float32(x: float): float32` | LLVM `fptrunc` | binary64→binary32（IEEE 定义的精度收窄，必须在源码里显式写出） |
| `float32_to_float` | `float32_to_float(x: float32): float` | LLVM `fpext` | binary32→binary64，精确、零信息损失 |

示例：

```dial
let i: int = 10;
let f: float = int_to_float(i) + 2.5;   // 必须先显式转换，再参与浮点运算
let j: int = float_to_int(3.7);          // j == 3（截断）
```

> `int_to_float` / `float_to_int` 是纯编译器内联的 LLVM 指令，**不经过任何 C 运行时函数**，
> 因此不会引入额外依赖（见 §15）。
>
> `char_to_int` / `int_to_char` 更彻底：**连一条指令都不产生**——`char` 与 `int` 在 IR 层就是
> 同一个 `i64`，转换纯粹是编译期的类型视图切换（零成本）。它仍然不是「隐式转换」，
> 因为源码里必须写出函数名。

### 5.4 `char`：单字符类型（v0.92）

`char` 是**一个 Unicode 码点**——不是字节、不是 UTF-8 片段。它补齐了 Dial 在字符层面的空白，
是字符串操作的基础类型。

```dial
var c: char = 'a';
print(c);                  // a        （打印字符本身）
print(char_to_int(c));     // 97       （打印码点）
print(int_to_char(98));    // b
print('中');                // 中       （多字节 UTF-8，输出时按码点编码）
print('\n');               // 转义：\n \t \r \0 \\ \' \" 均支持
```

**字面量规则**（词法阶段即校验）：

| 写法 | 结果 |
| --- | --- |
| `'a'`、`'中'`、`'é'` | ✅ 单个字符（可为任意 Unicode 码点） |
| `'\n'`、`'\t'`、`'\0'`、`'\\'`、`'\''`、`'\"'` | ✅ 转义序列 |
| `''` | ❌ 词法错误：字符字面量不能为空 |
| `'ab'` | ❌ 词法错误：字符字面量只能包含一个字符 |
| `'a`（未闭合） | ❌ 词法错误：缺少结束的单引号 |

**能做什么 / 不能做什么**：

| 操作 | 是否允许 | 说明 |
| --- | --- | --- |
| 赋值、作参数、作返回值 | ✅ | 值语义，与 `int` 一致 |
| `==` `!=` `<` `>` `<=` `>=` | ✅ | 按**码点序**比较：`'a' < 'b'` 为真 |
| `char_to_int` / `int_to_char` | ✅ | 与 `int` 互转，零成本 |
| `+ - * /` 算术 | ❌ **编译期拒绝** | `'a' + 1` 语义可疑（下一个字母？还是数值计算？），一律拒绝 |
| 与 `int` 混用（`'a' == 97`） | ❌ **编译期拒绝** | 零隐式转换铁律；请写 `char_to_int('a') == 97` |
| 作为 `channel<T>` 的元素类型 | ❌ **编译期拒绝** | 通道元素只能是不透明句柄（§27.2） |
| 判空 `is_null(c)` | ❌ **编译期拒绝** | `char` 是值类型，永远非空，与 `int` / `bool` 同理 |

**为什么不让 char 参与算术**：`'a' + 1` 的意图无法判定——是「下一个字母」还是「拿码点做加法」？
Dial 的选择是让源码把意图写清楚：要码点就 `char_to_int('a') + 1`，要下一个字母就 `int_to_char(char_to_int('a') + 1)`。
**宁可多写一次转换，也不接受一个含混的运算符。**

**打印语义与 Fail-Fast**：`print(c)` 输出的是**字符**（按 UTF-8 编码为 1~4 字节），不是码点数字。
若码点非法（负数、> `0x10FFFF`、或落在 UTF-16 代理区 `D800~DFFF`），
C Runtime 会在打印时**当场打印诊断并终止**，绝不输出半个坏字符——
静默产出乱码比当场崩溃更危险，它会在下游变成无法复现的数据损坏（Fail-Fast 铁律 §20）。

### 5.5 输出三件套：连续输出（v0.99）

`print(x)` **永远在末尾追加一个 `\n`**——它表达的是「打印一行」。
但很多程序需要的不是「一行」，而是**一段连续的字节流**：拼行的表格、
进度条、逐字动画、协议帧、日志尾巴。这类程序在 v0.99 之前**根本写不出来**：
唯一的输出手段 `print` 每写一次就断一行，想拼出一行就得把内容先拼成一个大字符串
（`"a" + "b" + ...`），而这既啰嗦又不适用于「边算边输出」的场景。

v0.99 补齐三个内置，构成输出的最小完备集：

| 内置 | 签名 | 输出 | 追加换行？ |
| --- | --- | --- | --- |
| `print` | `print(x: int \| float \| bool \| char \| string \| error): void` | 标量或字符串 | **是**（`\n`） |
| `print_raw` | `print_raw(x: int \| float \| bool \| char \| string \| error): void` | 同上，**原样**输出 | **否** |
| `write_char` | `write_char(c: char): void` | 单个字符 | **否** |

**`print` 与 `print_raw` 的唯一差别就是结尾那个 `\n`**，其余完全一致：
接受的类型集合相同、标量的文本形态相同（`print(42)` 与 `print_raw(42)` 都输出 `42`）、
字符串都按**字节长度**原样写出（不解释、不转义、不因内嵌 `\0` 而截断）。

**`write_char` 与 `print(char)` 的唯一差别也是那个 `\n`**：
`print('x')` 输出 `x` 并换行；`write_char('x')` 只输出 `x`，光标停在原处。

```dial
// 拼一行（表格的一格一行）
print_raw("[");
print_raw("alpha");
print_raw(",");
print_raw("beta");
print_raw("]");
// 输出： [alpha,beta]

// 逐字符输出（进度条 / 动画）
write_char('H');
write_char('i');
write_char('!');
// 输出： Hi!   （无换行）

// 一行结束时自己补换行
print("");
```

**`print_raw` 的返回值**：无（与 `print` 一致，都是**语句**而非表达式），
不能出现在赋值右侧或 `return` 位置。

**`str(x)` 的类型分派**：`str()` 在 v0.99 起接受 `char` 实参，返回**只含这一个字符**的 UTF-8 字符串
（ASCII 字符为 1 字节，多字节码点如 `'中'` 为 3 字节）。它让字符能直接参与字符串拼接：

```dial
let s: string = str('A') + "BC";   // "ABC"
```

> **注意 `length()` 是字节长度，不是字符数**：`str('中').length()` 是 `3` 而不是 `1`。
> Dial 的字符串按 **UTF-8 字节**计量长度（见 [§13.1](#131-方法)），
> 因此「字符串有几个字符」这个问题在 Dial 里**没有**直接答案——
> 按码点遍历需要自行解码 UTF-8。

> **v1.04 实测校正**：`str()` 除 `char` 外还接受 `int`（十进制）与 `float`（十进制小数），
> `str(42)` / `str(3.14)` / `str('A')` 都可用；`bool` 编译期拒绝，
> 需先 `int_to_string(...)` 之类的显式转换。早期文档只写了 `str(char)` 这一行，
> 那是**实现细节的一例**，不是全部重载。

**与文件 IO 的区别**：`print_raw` / `write_char` 写的是 **stdout**，
不受 `open` 的文件句柄管辖；要写进文件请用 `write(f, content)`（见 [§24](#24-文件-iofile-句柄)）。

**实现细节**：标量版的 `print_raw` 在 C Runtime 侧用**栈上缓冲**格式化，**零堆分配**——
它不先 `str(v)` 再输出，因为那样会分配一个带引用计数的堆字符串，
而它只是中转对象（用完即弃却仍占着引用计数），属于实打实的内存泄漏。

---

### 5.6 `byte`：单字节无符号值（v0.99）

`byte` 是 **0~255 的无符号单字节值类型**，与 `char` 故意区分：

| 类型 | 语义 | 参与算术 | 与 `int` 互转 |
| --- | --- | --- | --- |
| `char` | 一个 **Unicode 码点**（如 `'A'` = 65） | 否（只比较 / 经 `char_to_int` 转 `int`） | `char_to_int` / `int_to_char` |
| `byte` | 一个 **0~255 的数值**（如 `int_to_byte(65)` = 65） | 是 | `byte_to_int` / `int_to_byte`（带范围校验） |

> 为什么要分两个类型？「一个字节到底是字符还是数字」是 C 里长期的根源歧义。
> 把 `byte` 与 `char` 设为**类型独立**，就强迫程序员在每个使用点显式声明意图：
> 想当字符用 `char` + `int_to_char`，想当数值用 `byte` + `int_to_byte`，二者永不静默混用。

**字面量与转换**：

```dial
let b: byte = int_to_byte(65);   // 数值 65（范围校验：越界 [0,255] 编译期/运行期拒绝）
let n: int = byte_to_int(b);     // 65
let c: char = int_to_char(65);  // 'A'（码点语义，与上面的 b 值相同但类型不同）

print(b);            // 65   —— byte 按**数值**打印
print(c);            // A    —— char 按**字符**打印
```

- `int_to_byte(v: int): byte`：范围校验 `v ∈ [0,255]`，否则 Fail-Fast（绝不静默截断到 8 位）。
- `byte_to_int(b: byte): int`：零成本（同 `i64` 表示），仅类型重解释。
- `byte` 是**值类型**：无 ARC、无堆分配、无空指针，可参与算术与比较。

### 5.7 `bytes`：二进制缓冲区（v0.99）

`bytes` 是不可变的**二进制缓冲区**，由 C Runtime 以引用计数（ARC）管理，**与 `string` 类型独立**——
二者零隐式转换。它用来承载「任意字节序列」，包括**非法 UTF-8** 的原始数据（网络报文、加密 blob、图像字节等），
而 `string` 语义上始终是合法 UTF-8 文本。

**构造与转换**（全是内置函数）：

| 函数 | 签名 | 说明 |
| --- | --- | --- |
| `bytes_from_string` | `(s: string): bytes` | 把 UTF-8 文本原样当作字节序列 |
| `bytes_to_string` | `(b: bytes): string` | 把字节序列原样当作 UTF-8 文本（可能含非法 UTF-8，下游需自行处理） |
| `bytes_new` | `(len: int): bytes` | 构造长度 `len`、全零填充的缓冲区 |
| `bytes_length` | `(b: bytes): int` | 字节长度 |
| `bytes_get` | `(b: bytes, i: int): byte` | 取第 `i` 个字节（返回 `byte`，越界 Fail-Fast） |

```dial
let raw: bytes = bytes_from_string("hello");
print(bytes_length(raw));      // 5
print(bytes_get(raw, 0));      // 104  —— 'h' 的 ASCII 值（byte 按数值打印）
let s: string = bytes_to_string(raw);
print(s);                     // hello

let buf: bytes = bytes_new(4);  // 4 字节全零
print(bytes_length(buf));       // 4
```

> **与 `string` 的关系**：`bytes` 与 `string` 在 C 侧布局相同（`[refcount][len][data...]`），
> 但**类型严格分离**——不能把 `bytes` 直接当 `string` 打印（见 [§16](#16-内置-print-与输出三件套)），
> 必须先 `bytes_to_string` 显式转换，让「这段二进制该不该被当文本解释」成为一处显式决策。
> `bytes` 与 `string` 互转是**字节级原样拷贝**，不做任何编码转换。

---

### 5.8 `int32` / `float32`：FFI 精确 32 位宽度标量（v1.10 开发中增补）

**为什么必须新增这两个类型。** 在此之前 Dial 只有 `int`(LLVM `i64`) 与
`float`(LLVM `f64`)。用它们声明 `extern fn` 参数时，生成的 C ABI 签名也是
`i64` / `f64`，而 C 侧常常是 `int32_t` / `float`：

| 场景 | 用 `int` / `float` 声明的后果 |
| --- | --- |
| C 侧参数是 `float` | `f64` 的位模式低 32 位**绝不是**那个 float 值，C 读到的必然是垃圾 |
| C 侧返回 `int32_t` | C 只写 `eax`，Dial 按 `i64` 读 `rax`，高 32 位是未定义残留 |
| `extern struct` 含 32 位字段 | 编译器算出的 padding / `offsetof` 与 C 不一致，字段整体读错位 |
| 多参数连续传递 | 宽度错位在后续参数上累积，症状是「偶尔对、偶尔错」的隐蔽数据损坏 |

这类错误**不会崩溃**：程序带着一个看起来合法的错误值继续跑，往往到很后面才暴露。
按 §20 Fail-Fast 的口径，宁可编译期把宽度写进类型本身，也不让静默损坏溜过去。

**落地方式（完整一等类型，不是只加 FFI 标注）。**

| Dial 类型 | LLVM IR | C 类型 | 与 64 位类型的关系 |
| --- | --- | --- | --- |
| `int` | `i64` | `int64_t` | 基准 |
| `int32` | `i32` | `int32_t` | 独立类型，零隐式转换 |
| `float` | `double` | `double` | 基准 |
| `float32` | `float` | `float` | 独立类型，零隐式转换 |

两者是**值类型**（无 ARC、无堆分配、无空指针），可声明变量 / 参数 / 返回值，
可参与**同类**算术与比较（`int32 + int32 → int32`、`float32 * float32 → float32`），
可 `print` / `print_raw` / `str()` / 字符串插值，可进 `extern struct` 字段、
`@asm` 输入 / 输出操作数。与 `int` / `float` 之间**一律零隐式转换**：
`var x: int32 = 5` 是编译错误，必须写 `int_to_int32(5)`；`int32 + 2` 也是编译错误。

`int32` / `float32` **不进数组与 ARC 容器**（`int32[]` / `float32[]` 编译期拒绝），
也不是 `simd<T, N>` 的元素类型——这两处的槽位 / 向量语义另有一套宽度模型，
混进来只会制造新的静默损坏。位运算仍只限 `int`。

**Fail-Fast 边界。** `int_to_int32(x)` 是四条转换里唯一走 C 运行时的一条：
`stdlib/dial_bytes.c` 的 `dial_int_to_int32` 先校验 `x` 落在 int32 值域内，
越界当场打印真实数值并 `dial_assert_fail` 终止，绝不返回被截断的值
（C 里 `(int32_t)2147483648` 会变成 `-2147483648`，那是被伪装成合法值的数据损坏）。
`float_to_float32` 的精度收窄是 IEEE 754 定义行为，不是数据损坏，但仍必须显式写出。

**回归（AB 组）。** `tests/ffi/scalar32_probe.c` 的每个函数都用真实 `int32_t` /
`float`，由 `tests/ffi/scalar32_probe.dial` 声明为 `int32` / `float32` 后往返：
参数、返回值、四个连续 32 位参数、两个连续 `float` 参数、
`extern struct`（`sizeof = 24`、`offsetof = 8/12/16`）与双向字段读写逐项比对黄金输出；
`tests/ffi/scalar32_overflow.dial` 锁定越界 Fail-Fast；
`tests/bad/s32_*.dial` 五个负向用例锁定零隐式转换铁律。

## 6. 变量与可变性

### 6.1 声明

```dial
var x: int = 10;      // 可变
let y: int = 20;      // 不可变
```

规则：

1. **必须显式标注类型**，无类型推断。
2. **必须初始化**，不存在未初始化变量（这一点比 C 严格）。
3. `let` 变量**不可重新赋值**，也不可经字段/下标写入。违反即编译错误：

   ```
   语义错误：变量 'n' 由 let 声明，不可重新赋值
   ```

4. 同一作用域内不可重复声明。
5. `if` / `while` 体内构成内层作用域，允许声明与外层同名的变量（遮蔽外层）。

### 6.2 可变性判定

写入操作的合法性取决于**根变量**的可变性：

| 操作 | 要求 |
| --- | --- |
| `x = e` | `x` 必须是 `var` |
| `x.f = e` | `x` 必须是 `var` |
| `x[i] = e` | `x` 必须是 `var` |
| `x.push(v)` / `x[i] = v`（原地修改） | `x` 必须是顶层 `var`；`let` 变量、字段、下标均不可作接收者（见 §11.3） |

> **为什么 `let` 变量不能调用 `push` / `set`**：这两种操作会原地修改容器，且因 COW 可能**更换底层指针**；
> 编译器必须把返回的新指针写回接收者。接收者若不可重新赋值（`let`）或根本不是可写回的顶层变量
> （字段、下标），就无法可靠地完成这次"写回"，于是编译器在**语义阶段即拒绝**，而非把旧指针留在原地酿成悬垂。
> 若只是想获得一个新容器而不就地修改，请以表达式形式使用：`let b: int[] = a.push(v);`

---

## 7. 表达式与运算符

### 7.1 优先级（由低到高）

| 层级 | 运算符 | 结合性 |
| --- | --- | --- |
| 1 | `or` | 左 |
| 2 | `and` | 左 |
| 3 | `not`（前缀） | 右 |
| 4 | `==` `!=` | 左 |
| 5 | `<` `>` `<=` `>=` | 左 |
| 6 | `+` `-` | 左 |
| 7 | `*` `/` | 左 |
| 8 | `-`（一元负号） | 右 |
| 9 | `.field` `.method(args)` `[index]` | 左（可链式） |
| 10 | 字面量、变量、调用、数组/结构体字面量 | — |

因此 `not a == b` 解析为 `not (a == b)`。

### 7.2 类型规则

| 运算符 | 操作数要求 | 结果类型 |
| --- | --- | --- |
| `+` `-` `*` `/` | 两侧**必须同为 `int`**（→`int`）或同为 `float`（→`float`），**不可混合** | `int` 或 `float` |
| `==` `!=` | 两侧类型完全相同：同为数值（`int` 或 `float`，不可混合）、或同为 `bool`、或同为 `string` | `bool` |
| `<` `>` `<=` `>=` | 两侧类型完全相同且均为数值（`int` 或 `float`，不可混合） | `bool` |
| `and` `or` | 两侧均为 `bool`，**短路求值** | `bool` |
| `-`（一元） | `int` 或 `float`（与原操作数同类型） | 同操作数 |
| `not` | `bool` | `bool` |

> 所有二元运算都要求操作数**类型完全相同**。特别地，`1 + 2.0` 在 C 里合法（int 提升为 double），
> 在 Dial 里是**编译错误**——必须先写 `int_to_float(1) + 2.0`。这把"隐式混用"从运行期隐患提前到编译期拒绝。

违反以上任一条即编译错误。

### 7.3 语义细则（与 C 的重要差异）

| 行为 | Dial 的规定 | 对比 C |
| --- | --- | --- |
| 整数除法 `7 / 2` | 整数除法，结果 `3` | 同 C |
| 混合算术 `7 / 2.0` | ❌ **编译错误**：`int` 与 `float` 无隐式转换，必须先 `int_to_float(7) / 2.0` | C 隐式把 `int` 提升为 `double`，结果 `3.5` |
| `and` / `or` 求值 | **短路求值**（与 C 的 `&&` / `\|\|` 一致） | C 同样短路 |
| 字符串 `==` | 按**内容**比较（`dial_string_eq`） | C 的 `==` 比指针 |
| 数组 `==` | ❌ 不支持 | — |
| 结构体 `==` | ❌ 不支持 | — |
| 运算符重载 | ❌ 不存在。字符串拼接必须写 `concat()` | — |

> `and` / `or` 的短路求值是一项**强制语义**：先求左操作数，若已能决定结果（`and` 遇假、`or` 遇真）则不再求值右操作数。
> 这保证了诸如"判空后访问"这类依赖短路的代码不会因右操作数被盲目求值而崩溃——这是现代语言的常识。
> 若需要两侧副作用都必然发生，请显式写成两条独立语句。

---

## 8. 语句与控制流

### 8.1 语句一览

```dial
var x: int = 1;            // 变量声明
x = 2;                     // 赋值
p.x = 3;                   // 字段赋值
a[0] = 4;                  // 下标赋值
print(e);                  // 打印语句（追加换行）
print_raw(e);              // 原样打印语句（v0.99，不追加换行）
write_char(c);             // 输出单字符（v0.99，不追加换行）
f(1, 2);                   // 表达式语句
if (cond) { ... } else { ... }
if (cond) { ... } else if (c2) { ... } else { ... }   // else if 语法糖
for (i in 0..n) { ... }    // 范围 for 循环（end 独占）
while (cond) { ... }
break;                     // 跳出最内层循环（仅循环体内）
continue;                  // 进入下一次迭代（仅循环体内）
return e;                  // 或 return;（仅 void 函数）
```

### 8.2 `if` / `else if` / `while`

条件**必须带括号**，且类型必须是 `bool`：

```dial
if (x > 0) { ... } else { ... }

// else if 是语法糖：编译期展开为嵌套 if，无需新语法节点
if (x < 0) { ... }
else if (x == 0) { ... }
else if (x < 10) { ... }
else { ... }

while (i < n) { ... }
```

循环体内可用 `break` / `continue`（见 §8.4）与 `return` 提前退出。

### 8.3 范围 `for` 循环

`for (i in start..end)`：循环变量 `i` 为 `int`，从 `start` 迭代到 `end - 1`（`end` **独占**，半开区间）。
`start` / `end` 必须为 `int` 表达式，`i` 在循环体内为可变变量（可被重新赋值）。

```dial
var sum: int = 0;
for (i in 0..5) {        // i = 0,1,2,3,4
    sum = sum + i;       // 0+1+2+3+4 = 10
}
```

循环变量作用域限定在循环体内；`start`/`end` 在每次迭代开始前各求值一次（不支持 C 式「每次迭代重新求值边界」的语义，等价于把上下界快照到循环前）。

### 8.4 `break` / `continue`

`break` 跳出**最内层**循环；`continue` 进入下一次迭代。两者**只允许出现在 `while` / `for` 循环体内**，
写在别处（包括 `if` 但外层无循环）都是编译错误。

```dial
var sum: int = 0;
for (i in 0..6) {
    if (i == 3) {
        continue;      // 跳过 3
    }
    sum = sum + i;     // 0+1+2+4+5 = 12
}
```

**关键语义**：`for` 的 `continue` 会**先执行 `i = i + 1` 再回到条件判定**。
实现上自增落在独立的 `for.inc` 块，`continue` 跳到该块而非条件块——若跳到条件块就会跳过自增造成死循环。
`while` 的 `continue` 则直接回到条件判定，因此**自增语句必须写在 `continue` 之前**（否则同样死循环）：

```dial
var n: int = 0;
while (n < 10) {
    n = n + 1;         // 自增在前：continue 不会漏掉它
    if (n == 2) { continue; }
    ...
}
```

### 8.5 块与作用域

`{ ... }` 既可以作为函数体或 `if`/`while`/`for` 的分支体，**也可以**作为**独立的裸块语句**
（v0.99 起，见 [§8.6](#86-裸块独立的词法作用域v099)）。

---

### 8.6 裸块：独立的词法作用域（v0.99）

裸块 `{ ... }` 是一个**语句**，用来显式划定一段代码的作用域。它的用途有三：

1. **变量遮蔽**：块内可以声明与外层同名的变量，互不干扰；
2. **生命周期隔离**：块内声明的堆类型局部变量（`string` / `int[]` / `map` / `error`）在**离开块时即被释放**，
   不必等到函数结束；
3. **把一段语句当匿名作用域使用**：无需为此造一个只调用一次的函数。

```dial
let x: int = 1;
{
    let x: int = 2;    // 遮蔽外层的 x
    print(x);          // 2
}
print(x);              // 1 —— 外层 x 从未被改动
```

**语义规则**：

| 规则 | 说明 |
| --- | --- |
| 遮蔽方向 | 块内**可以**遮蔽外层同名变量；块内引用不到外层的同名变量（要到外层请先在外面改名或换名引用） |
| 生命周期 | 块内声明的局部变量在**离开块时**脱离作用域；此后引用即编译错误 |
| 堆类型释放 | 块内的 `string` / `int[]` / `map` / `error` 局部变量在离开块时被 ARC 释放（与函数返回时的释放规则一致） |
| 嵌套 | 裸块可任意嵌套；每层各自成一个作用域 |
| 跨块引用 | **禁止**把块内变量带到块外使用（编译期拒绝） |

**与 `if` / `while` / `for` 的分支体的区别**：分支体**也**引入作用域，
但它是**语法的一部分**——`if (c) { ... }` 里的 `{ ... }` 不是独立语句，不能单独出现、也不能被赋值。
裸块则是**可以单独写出来**的语句。

```dial
if (x > 0) { let y: int = 1; }   // 分支体：作用域限于该分支
{ let y: int = 2; }               // 裸块：独立语句，效果同上但可单独书写
```

---

## 9. 函数

### 9.1 定义

```dial
fn add(a: int, b: int): int {
    return a + b;
}

fn log(msg: string): void {
    print(msg);
    return;
}
```

- 参数与返回类型均须显式标注。
- 返回类型可以是 `void`。

> **`.dial` 源码里没有 `extern` 关键字。** 外部函数（来自已安装包）用 `extern fn` 在 `.diali` 里**声明签名**（见 [§25.2](#252-包内结构与接口签名供-lsp-与-sema-使用)），调用方在 `.dial` 里照常写 `函数名(...)` 即可，与普通本地函数无差别——`extern` 只存在于包接口层，不存在于语言层。

### 9.2 入口函数

程序**必须**定义 `main`，且返回类型必须是 `int`：

```dial
fn main(): int {
    return 0;
}
```

`main` 的返回值作为进程退出码（C ABI 下为 `i32`）。

### 9.3 参数传递与所有权

| 参数类型 | 传递方式 | 被调方所有权 |
| --- | --- | --- |
| `int` `float` `bool` | 按值 | 无 |
| `string` `int[]` | 不透明指针按值传递 | 入口 `retain`，出口 `release`（借用语义） |
| 结构体 | 按值拷贝（含其堆字段指针的复制） | 入口递归 `retain` 堆字段，出口递归 `release` |

### 9.4 返回值的确定性

非 `void` 函数若执行到函数体末尾仍无 `return`，返回该类型的**零值**：

| 类型 | 零值 |
| --- | --- |
| `int` | `0` |
| `float` | `0.0` |
| `bool` | `false` |
| `string` / `int[]` | 空指针 |
| 结构体 | 各字段取其零值 |

> 这比 C 严格：C 中"非 void 函数落到末尾"是**未定义行为**，Dial 定义为确定的零值。

---

## 10. 结构体

### 10.1 定义

只能在**顶层**定义，写在函数体内是语法错误：

```dial
struct Point {
    x: int,
    y: int,
}
```

字段类型可以是 `int` / `float` / `bool` / `string` / `int[]` / 另一个结构体。

### 10.2 字面量

```dial
let p: Point = Point { x: 3, y: 4 };
let q: Point = Point { y: 10, x: 20 };   // 顺序无关
```

规则：

1. **必须显式列出全部字段。** 不支持"省略即零初始化"——C 允许 `Point p = {.x=1}`，
   Dial 不允许，以此杜绝漏初始化字段。
2. **字段顺序与声明顺序无关**，字面量按**字段名**定位下标（不会因顺序不同而错位）。
3. 字段类型必须与该字段声明类型兼容。

### 10.3 访问与赋值

```dial
print(p.x);            // 读
p.x = 100;             // 写（p 必须是 var）
print(r.origin.x);     // 支持链式
```

### 10.4 语义

结构体是**值类型**：

- 在 LLVM 中以 `alloca` 落在栈上，字段访问编译为 `getelementptr`，零运行时开销。
- 赋值、传参、返回均为**值拷贝**。
- 若结构体含有 `string` / `int[]` 字段，值拷贝时这些字段按**共享引用**处理：
  编译期插入对称的 `retain`，作用域结束时插入对称的 `release`（详见 §14.4）。

---

## 11. 动态数组 `int[]`

`int[]` 是不透明指针，内存布局 `[refcount][kind][len][cap][elems...]`（头部 `kind` 记录元素类别）由 C 侧私有维护。
**元素类型可为任意标量，或任意 8 字节句柄**：

| 类别 | 元素类型 | 起始版本 |
| --- | --- | --- |
| 标量 | `int` | 最初 |
| 标量 | `float` / `byte` / `bool` / `char` | **v1.01**（见 [§30.1](#301-数组元素类型扩展)） |
| 句柄 | `string` / `int[]`（多维）/ `map` / `error` / `channel` / 不透明指针 | v0.99（见 [§11.6](#116-多维数组与句柄元素数组)） |

### 11.1 构造

```dial
var a: int[] = [10, 20, 30];
var names: string[] = ["alice", "bob", "carol"];   // v0.99：句柄元素
var m: int[][] = [[1, 2], [3, 4, 5]];              // v0.99：多维（元素是 int[] 句柄）
var f: float[] = [1.5, 2.5];                       // v1.01：浮点元素
```

空数组字面量 `[]` **需要类型标注**（`var e: int[] = [];`）——元素类型由标注提供；
裸写 `var e = [];` 仍编译失败（编译器不替你猜）。见 [§30.2](#302-空数组字面量-)。

### 11.2 方法

| 方法 | 等价于 C 调用 | 返回 | 说明 |
| --- | --- | --- | --- |
| `a.length()` | `dial_array_length` | `int` | 元素个数 |
| `a.get(i)` | `dial_array_get` | 元素类型（`int` 或句柄指针） | 按下标读 |
| `a.push(v)` | `dial_array_push` | 数组类型（如 `int[]` / `string[]`） | 追加；**返回（可能更新后的）数组指针** |
| `a.set(i, v)` | `dial_array_set` | 数组类型 | 按下标写；被共享时内部触发 COW，**返回新指针** |
| `a.clone()` | `dial_array_clone` | 数组类型 | **独立副本**（深拷贝，元素引用另行计数） |
| `a.remove(i)` | `dial_array_remove` | 元素类型 | 删除下标 `i` 处的元素并**返回它**；越界当场 Fail-Fast（v1.01） |
| `a.pop()` | `dial_array_pop` | 元素类型 | 弹出末元素并**返回它**；空数组当场 Fail-Fast（v1.01） |
| `a.sort()` | `dial_array_sort` | 数组类型 | 原地**升序排序**；仅标量元素可用（v1.01） |

下标语法 `a[i]` 等价于 `a.get(i)`；`a[i] = v` 等价于 `a = a.set(i, v)`。嵌套下标 `a[i][j] = v` 同样支持（v0.99，见 [§11.6](#116-多维数组与句柄元素数组)）。

v1.01 新增的 `remove` / `pop` / `sort` 见 [§30.3](#303-数组删除元素-removei--pop) 与 [§30.8](#308-数组排序-sort)。

### 11.3 为什么 `push` / `set` 要返回指针

这两种操作在需要扩容或存在多个持有者时，会产生**新的**不透明指针。
编译器强制把返回值写回接收变量，杜绝"旧指针失效"的悬垂问题：

```dial
var a: int[] = [10, 20, 30];
a.push(40);     // 编译器自动写回 a，无需（也不允许）手写 a = a.push(40)
a[0] = 99;      // 同理
```

> 此处**不**对旧指针做 `release`：`dial_array_push` / `dial_array_set` 已经在
> C 侧完成了所有权交接（独占时返回同一指针、共享时克隆并释放本调用者的引用）。
> 若编译器再补一次 `release`，会造成悬垂指针或双重释放。

**「独占」路径必须原样移交所有权，一个引用计数都不能动。** 这是最容易写错的一处：

| 情形 | `refcount` | C 侧该做什么 | 做错的后果 |
| --- | --- | --- | --- |
| 独占（`== 1`） | 保持 **1** | 原地写入，**不 retain、不 release** | 多一次 `retain` → 每写一次计数 +1 → COW 守卫 `refcount > 1` 恒真 → **每次赋值都深拷贝整个数组**（O(N²) 内存） |
| 共享（`> 1`） | 减 **1** | 克隆出新 buffer，并 `release` 旧 buffer | 漏掉 `release` → 旧 buffer 与其元素永久泄漏 |

> v0.99 实测：违反「独占路径不动计数」这条不变量时，`a[i] = v` 循环 100 万次
> 需要约 **8 TB** 内存（进程被 OOM Killed）；修正后峰值约 **10 MB**。
> 该不变量现由测试组 N 以「虚拟内存上限」钉死（见 `tests/mem/`）。

### 11.4 接收者必须是可变变量（`var`）

`push` / `set` 会修改容器并可能更换指针，因此**接收表达式必须是顶层可变变量**：

| 调用形式 | 是否允许 |
| --- | --- |
| `var a: int[] = ...; a.push(v);` | ✅ 允许（编译器把新指针写回 `a`） |
| `let a: int[] = ...; a.push(v);` | ❌ 编译错误（`let` 不可重新赋值，见 §6.2） |
| `rec.field.push(v);` | ❌ 编译错误（字段不可被可靠写回） |
| `arr[i].push(v);` | ❌ 编译错误（下标不可被可靠写回） |
| `let b: int[] = a.push(v);` | ✅ 允许（以表达式形式获取新容器，不就地修改原变量） |

> 这一约束把"容器被原地修改但接收者无法接收新指针"这一类悬垂 Bug **提前到编译期拒绝**，
> 而不是留到运行期靠运气。

### 11.5 越界行为

默认等级（Debug / ReleaseSafe）下**有边界检查**：越界读写当场触发陷阱终止，
由 codegen 插入的检查（§20.2）与 C Runtime 的检查（§20.3）双重保证。
仅在 `--fast`（ReleaseFast）下才回落到"无边界检查、越界即 UB"的 C 行为。

### 11.6 多维数组与句柄元素数组

v0.99 起，数组不再「只能装 int」。只要元素类型是 `int` 或**任意 8 字节句柄**，就能直接存储——
因为所有不透明句柄（`string` / `int[]` / `map` / `error` / 不透明指针）共享同一份 `dial_rc_*` 引用计数，
`channel` 则走原子引用计数 `dial_channel_*`。数组头部用一个 `kind` 字段记下自己的元素类别，
`push` / `set` / 克隆 / 释放时据此对元素做对应的 retain / release，从而实现**按元素 ARC**。

v1.01 起，**标量元素也不止 `int`**：`float` / `byte` / `bool` / `char` 都能直接存——
它们与 `int` 同为 8 字节值类型（无 ARC、无堆分配），只是 `float` 在槽位里存
IEEE754 **位模式**（绝不 `fptosi` 截断小数），`bool` 存 `i1` 扩展后的 0/1。
详见 [§30.1](#301-数组元素类型扩展)。

```dial
var names: string[] = ["alice", "bob"];   // 句柄元素：每个 string 都按元素 ARC 管理
names.push("carol");                      // 自动 retain "carol"
print(names[0]);                          // "alice"

var m: int[][] = [[1, 2, 3], [4, 5]];     // 多维：内层 int[] 是外层数组的句柄元素
print(m[2][3]);                           // 嵌套读
m[0][0] = 11;                            // 嵌套写（编译器递归解包两层下标）
```

**内存布局**：`[int64 refcount][int64 kind][int64 len][int64 cap][int64 elems...]`（24 字节头，
前导 refcount 仍由 `dial_rc_*` 管理）。`kind` 取值：`0 = INT`、`1 = RC`（string / int[] / map / error / 不透明指针）、`2 = ATOMIC`（channel）。

**按元素 ARC 的边界**：

- `string[]` / `int[][]` / `map[]` / `error[]` / 不透明指针 `[]` → 走 `dial_rc_retain` / `dial_rc_release`；
- `channel[]` → 走 `dial_channel_retain` / `dial_channel_release`（原子）；
- `int[]` → 元素无所有权，读写零开销。

**嵌套下标赋值**（`m[i][j] = v`）：编译器把 `arr[index]` 拼成完整左值表达式，递归解包——
先 `gen_expr` 取出内层数组句柄，调用 `dial_array_set` 写入值，再把「可能 COW 更新」的句柄写回外层容器，
直到最外层变量槽。单层下标赋值（`a[i] = v`）仍走原有高效路径，完全不受影响。

**标量非 int 元素仍被拒绝**：`float[]` / `bool[]` / `byte[]` / `char[]` 在编译期即报错（见 [§5.2](#52-类型良构性约束)）。

---

## 12. 哈希表 `map`（int → int）

`map` 是 Dial 的哈希表类型，键与值均固定为 `int`（v0.4）。在 IR 中它同样只是一个 `ptr`（`i8*`），
其内存布局 `[int64 cap][int64 count][entries...]` 由 C 侧私有维护，并走与 `string` / `int[]`
**同一套 ARC 引用计数**（`dial_map_retain` / `dial_map_release`）。这完全契合 [§19 内存宪法](#19-内存宪法防-gc-铁律)：
确定性、零 GC 停顿、所有权可控。

每条 entry 24 字节：`{ int64 key; int64 val; int64 used; }`，采用线性探测（linear probing）开放寻址，
删除采用「左移紧凑」维护簇连续性。负载因子超过 3/4 时自动扩容并重新哈希。

### 12.1 构造

```dial
var m: map = new_map();   // 创建空表，返回 map 类型（ARC 引用计数 1）
```

`new_map()` 无参数，在 C 侧调用 `dial_map_new`，分配一个 `cap = 8` 的空表。

### 12.2 方法

| 方法 | 签名 | 返回 | 说明 |
| --- | --- | --- | --- |
| `put` | `m.put(k: int, v: int)` | `map`（可能更新的指针，**由编译器自动写回**） | 写入键值对；命中覆盖值，未命中追加 |
| `get` | `m.get(k: int)` | `int` | 返回值；**未命中返回 -1** |
| `contains` | `m.contains(k: int)` | `bool` | 命中返回 `true` |
| `remove` | `m.remove(k: int)` | `int` | 移除并返回其值；未命中返回 -1 |
| `size` | `m.size()` | `int` | 当前条目数 |

```dial
var m: map = new_map();
m.put(1, 100);
m.put(2, 200);
print(m.get(2));        // 200
print(m.contains(2));   // true
print(m.size());        // 2
m.remove(2);
print(m.get(2));        // -1（已删除）
```

### 12.3 为什么 `put` 要返回指针

与 `int[].push` / `int[].set` **完全一致**（见 [§11.3](#113-为什么-push--set-要返回指针)）：`put` 在底层可能因
**扩容**或**写时复制（COW）**更换底层 buffer 指针。

**统一规则（强制）**：

> **所有原地修改并触发 COW 的方法（`push` / `put`），编译器自动写回接收变量，
> 严禁用户手写赋值。**

```dial
var m: map = new_map();
m.put(1, 1);       // ✅ 正确：编译器自动把新指针写回 m

m = m.put(1, 1);   // ❌ 禁止：严禁手写赋值（与数组规则统一，见下）
```

为什么废弃 `m = m.put(...)` 这种写法：

| 理由 | 说明 |
| --- | --- |
| **与数组规则不统一** | 数组写 `a.push(40)`，map 却要写 `m = m.put(...)`——这是纯粹的不一致，没有任何技术理由支撑 |
| **诱导错误的所有权直觉** | 手写赋值看起来像「由用户把返回值赋回去」，实则**无论写不写，编译器都会写回**；两种写法并存只会让人误以为二者有区别 |
| **ARC 归属容易被误判** | `dial_map_put` 已在 C 侧完成所有权交接（独占时返回同一指针、共享时克隆并释放本调用者的引用）。让用户侧再参与赋值，只会模糊 retain/release 的归属 |

> 与数组一致：此处**不**对旧指针做 `release`，C 侧已完成所有权交接；
> 若编译器再补一次 `release`，会造成悬垂指针或双重释放（同 §11.3 的说明）。

若 `m` 当前被多者共享（例如已作为实参传入某函数），`dial_map_put` 会先 COW 克隆再修改，
**调用方持有的旧指针不受影响**（ARC 隔离，见 [§14.5](#145-cow-的触发时机)）。

### 12.4 接收者必须是可变变量（`var`）

与数组一致：`put` 可能更换指针，因此接收者必须是顶层 `var` 变量。`let` 变量、字段、下标均不可作接收者：

```dial
let m: map = new_map();
m.put(1, 1);   // ❌ 编译错误：let 不可调用原地修改方法
```

### 12.5 ARC 与确定性内存

`map` 与 `string` / `int[]` 共享同一份引用计数实现（`runtime/dial_rt_mem.h` 的 `dial_rc_*`）。
作用域结束时编译器插入对称 `release`，归零即释放，**绝不引入追踪式 GC**。

---

## 13. 字符串 `string`

`string` 是不透明指针，内存布局 `[refcount][len][data...]` 由 C 侧私有维护。

### 13.1 方法

| 方法 | 等价于 C 调用 | 返回 | 说明 |
| --- | --- | --- | --- |
| `s.length()` | `dial_string_length` | `int` | **字节长度**（UTF-8 字节数，**不是字符数**）：`"中文abc".length()` == 9 |
| `s.concat(t)` | `dial_string_concat` | `string` | 拼接，产生新对象 |
| `s.get(i)` | `dial_string_get` | `int` | 取第 `i` 个**字节**的值（`0~255`）。**注意**：这是字节、不是 Unicode 码点——`"中".get(0)` 返回 `228`（`'中'` 的 UTF-8 首字节），而非码点 `20013`。想要 `char` 请显式转换 `int_to_char(s.get(i))`（v0.92 起）；但对多字节字符，只取首字节并不能还原该字符 |
| `s.substring(from, to)` | `dial_string_substring` | `string` | 半开区间 `[from, to)` |
| `s.clone()` | `dial_string_clone` | `string` | **独立副本**（深拷贝） |
| `s.eq(t)` | `dial_string_eq` | `bool` | 按内容比较 |
| `s.index_of(sub)` | `dial_string_index_of` | `int` | 首次出现的**字节偏移**；未找到返回 `-1`（v1.01） |
| `s.replace(from, to)` | `dial_string_replace` | `string` | 替换全部出现，返回**新**字符串（原串不变）（v1.01） |
| `s.trim()` | `dial_string_trim` | `string` | 去首尾空白（空格 / 制表 / 换行 / 回车），返回新串（v1.01） |
| `s.split(sep)` | `dial_string_split` | `string[]` | 按分隔符切分；可直接 `for` 遍历（v1.01） |

`==` / `!=` 对 `string` 按**内容**比较（编译为 `dial_string_eq`），而非比较指针地址。

v1.01 新增的四个方法见 [§30.5](#305-字符串方法补齐)。

### 13.2 不可变语义

v0.2 未提供任何原地修改字符串的接口：所有操作（`concat`、`substring`）都产生新对象。
字符串因此天生满足"只读推断"，跨函数共享永远是 O(1)。

---

## 14. 内存与所有权模型

这是 Dial 相对 C 的核心增值点：**尽可能在编译期把内存管理开销消掉**。

### 14.1 不透明指针契约

Dial 代码中的 `string` / `int[]` 只是 `i8*`：

```llvm
; s.length() 的唯一形态
%len = call i64 @dial_string_length(ptr %s)
```

用户代码**永远无法**：

- 计算其偏移量、做指针算术；
- 读取 refcount 或容量字段；
- 直接 `free` 它。

一切都必须经由 C 导出的 `dial_*` 函数。

### 14.2 引用计数对象的布局

```
[ int64 refcount ][ payload ... ]
                    ^
                    └─ Dial 看到的"不透明指针"指向这里
```

`dial_rc_alloc` 返回 payload 指针；`dial_rc_retain/release` 操作其前 8 字节。

### 14.3 编译期三种绑定策略

Sema 阶段对每条 `let/var x = y;` 做数据流分析，判定下列三种策略之一：

| 策略 | 触发条件 | 生成的代码 | 开销 |
| --- | --- | --- | --- |
| **移动语义（Move）** | `y` 在此之后再未被引用，且不在循环/分支内 | 只复制指针，**不生成任何 retain/release** | 零 |
| **只读共享（Share）** | `y` 之后仍被读取但从未被写入 | 一次 `retain`，双方共享同一指针 | O(1) |
| **COW 兜底（Copy）** | 静态分析无法证明独占（如穿过函数边界） | 交给 C Runtime，在**写入瞬间**按 refcount 决定是否深拷贝 | 仅在此刻可能 O(n) |

前两种在编译期即消除了运行时开销；只有第三种可能真的复制内存。

判定"最后一次引用"时，位于循环或分支内的引用被视为**不安全上下文**，
不参与移动判定（因为该语句可能被执行零次或多次）。

### 14.4 栈分配与逃逸分析

- **结构体**：值类型，一律在栈上 `alloca`，字段访问是 `getelementptr`。
  这是确定的，不依赖逃逸分析结果。
- **所有局部变量的栈槽**：统一分配在函数的 **entry 块**上。
  这不只是优化，而是**正确性要求**：局部变量声明可能出现在循环体内，
  若在当时所在块直接 `alloca`，每轮迭代都会多压一个栈槽，
  迭代次数一多就会耗尽栈空间（段错误）。提到 entry 块后每个变量只占一个
  固定栈槽、跨迭代复用，**栈用量与循环次数无关**。
- **结构体中的堆字段**：结构体值被复制时，其 `string` / `int[]` 字段按共享引用处理，
  编译期插入**对称**的 `retain`；作用域结束时插入对称的 `release`。
  `retain` 与 `release` 的数量严格相等——不多（泄漏）、不少（双重释放）。

  验证方式：把循环次数从 1 万提到 100 万，峰值 RSS 基本不变即为平衡
  （见 `examples/ownership_stress.dial`）。

### 14.5 COW 的触发时机

```
                     ┌─ 编译期能证明独占 ──→ 移动语义：零开销
let/var x = y;  ─────┤
                     ├─ 编译期能证明只读 ──→ 共享：一次 retain，O(1)
                     └─ 静态分析失效 ─────→ 交给 C Runtime
                                              │
                                    写入瞬间检查 refcount
                                              │
                            ┌─────────────────┴─────────────────┐
                      refcount == 1                        refcount > 1
                      原地写入，零拷贝                    先克隆再写（真实复制）
```

C 侧 `dial_rc_clone` 是这一兜底的落点：唯一持有时直接返回自身，否则深拷贝。

### 14.6 自赋值安全

`x = x` 必须安全。编译器采用"**先取得新值所有权，再释放旧值**"的顺序：

```dial
x = x;   // 先 retain（计数 1→2），再 release（2→1），绝不会先 free 再 use
```

若顺序颠倒，计数会先归零被 `free`，随后的 `retain` 即 use-after-free。

### 14.7 返回值不被提前释放

`return` 的处理顺序是**强制**的：

1. 求值返回表达式；
2. 若返回的是局部变量，标记"已移动"（所有权随返回值转移）；
3. 释放其余**未被转移**的局部变量；
4. 生成 `ret`。

若在求值之前就释放全部局部变量，返回的对象会被提前销毁。

---

## 15. FFI 契约：Dial 语法 → C 函数

编译器前端把 Dial 语法**逐条**翻译为对 C 导出函数的调用，不重新实现任何容器：

| Dial 写法 | 生成的 LLVM IR |
| --- | --- |
| `"abc"` | `call ptr @dial_string_new(ptr @str)` |
| `s.length()` | `call i64 @dial_string_length(ptr %s)` |
| `s.concat(t)` | `call ptr @dial_string_concat(ptr %s, ptr %t)` |
| `s.get(i)` | `call i64 @dial_string_get(ptr %s, i64 %i)` |
| `s.substring(a, b)` | `call ptr @dial_string_substring(ptr %s, i64 %a, i64 %b)` |
| `s.clone()` | `call ptr @dial_string_clone(ptr %s)` |
| `s == t` | `call i64 @dial_string_eq(ptr %s, ptr %t)` 后与 0 比较 |
| `[1, 2, 3]` | `call ptr @dial_array_new()` + 三次 `dial_array_push` |
| `a.length()` | `call i64 @dial_array_length(ptr %a)` |
| `a.get(i)` / `a[i]` | `call i64 @dial_array_get(ptr %a, i64 %i)` |
| `a.push(v)` | `call ptr @dial_array_push(ptr %a, i64 %v)` 并写回 `%a` |
| `a[i] = v` | `%a = call ptr @dial_array_set(ptr %a, i64 %i, i64 %v)` |
| `a.clone()` | `call ptr @dial_array_clone(ptr %a)` |
| `new_map()` | `call ptr @dial_map_new()` |
| `m.put(k, v)` | `%m = call ptr @dial_map_put(ptr %m, i64 %k, i64 %v)` 并写回 `%m` |
| `m.get(k)` | `call i64 @dial_map_get(ptr %m, i64 %k)` |
| `m.contains(k)` | `call i64 @dial_map_contains(ptr %m, i64 %k)` 后与 0 比较 |
| `m.remove(k)` | `call i64 @dial_map_remove(ptr %m, i64 %k)` |
| `m.size()` | `call i64 @dial_map_size(ptr %m)` |
| `read_line()` | `call ptr @dial_read_line()` |
| `open(p, m)` | `call ptr @dial_file_open(ptr %p, ptr %m)`（失败返回 null） |
| `read(f, n)` | `call ptr @dial_file_read(ptr %f, i64 %n)`（读最多 n 字节，失败返回空串） |
| `write(f, c)` | `call i64 @dial_file_write(ptr %f, ptr %c)`（返回写入字节数，失败 -1） |
| `close(f)` | `call void @dial_file_close(ptr %f)`（空指针安全） |
| `print(x)` | `dial_print_int` / `dial_print_float` / `dial_print_bool` / `dial_print_char` / `dial_print_string` |
| `print_raw(x)`（v0.99） | `dial_print_raw` / `dial_print_raw_int` / `dial_print_raw_float` / `dial_print_raw_bool` / `dial_write_char`（**均不追加换行**） |
| `write_char(c)`（v0.99） | `call void @dial_write_char(i64 %c)`（不追加换行） |
| `str(x)`（v0.99；`x` 可为 `char` / `int` / `float`） | `char` → `dial_char_to_string`（UTF-8，1~4 字节）；`int` / `float` → 对应格式化调用；`bool` 编译期拒绝（见 §5.5） |
| 变量绑定（共享） | `dial_string_retain` / `dial_array_retain` / `dial_map_retain` |
| 作用域结束 | `dial_string_release` / `dial_array_release` / `dial_map_release` |

**`extern fn` / `extern struct` 的标量宽度（v1.10 增补）**：函数签名与
`extern struct` 字段都按 C ABI **真实宽度**生成，不再把一切标量统一抬成 64 位：

| Dial 类型 | C 类型 | LLVM 函数签名 | `extern struct` 字段 |
| --- | --- | --- | --- |
| `int` | `int64_t` | `i64` | `i64` |
| `int32` | `int32_t` | `i32` | `i32` |
| `float` | `double` | `double` | `double` |
| `float32` | `float` | `float` | `float` |
| `bool` | `int`（默认整型提升） | `i32` | `i8`（C `_Bool`，不做提升） |
| `byte` | `uint8_t` | `i8` | `i8` |

`int32` / `float32` 的内部 IR 宽度与 FFI 宽度一致，因此 `abi_adapt` 无需额外
收窄；它同时补上了 `f32 ↔ f64` 的 `fpext` / `fptrunc` 分支，保证任何宽度
不一致的路径都显式转换，绝不把位模式原样递给 C。详见 §5.8。

### C Runtime 导出函数

**`runtime/dial_rt_mem.h`**（引用计数原语）

```c
void *dial_rc_alloc(int64_t payload_size);          // refcount 初始为 1
void  dial_rc_retain(void *p);                      // +1
void  dial_rc_release(void *p);                     // -1，归零则释放
void *dial_rc_clone(void *p, int64_t payload_size); // 唯一持有时返回自身（COW 命中）
```

**`stdlib/dial_string.h`**

```c
DialString dial_string_new(const char *lit);
int64_t    dial_string_length(DialString s);
DialString dial_string_concat(DialString a, DialString b);
int64_t    dial_string_get(DialString s, int64_t i);          // 返回第 i 个「字节」的值（0~255）
int64_t    dial_string_eq(DialString a, DialString b);        // 相等返回 1
DialString dial_string_substring(DialString s, int64_t from, int64_t to);
DialString dial_string_clone(DialString s);
void       dial_string_retain(DialString s);
void       dial_string_release(DialString s);
void       dial_print_string(DialString s);
DialString dial_char_to_string(int64_t code);       // v0.99：码点 → UTF-8 字符串（1~4 字节）
int        dial_encode_utf8(int64_t code, char *buf); // v0.99：运行时内唯一的码点编码实现
```

**`stdlib/dial_array.h`**

```c
DialArray dial_array_new(void);
DialArray dial_array_push(DialArray a, int64_t v);            // 返回（可能更新的）指针
int64_t   dial_array_get(DialArray a, int64_t i);
DialArray dial_array_set(DialArray a, int64_t i, int64_t v);  // 共享时内部 COW
int64_t   dial_array_length(DialArray a);
DialArray dial_array_clone(DialArray a);
void      dial_array_retain(DialArray a);
void      dial_array_release(DialArray a);
```

**`stdlib/dial_io.h`**

```c
void       dial_print_int(int64_t v);
void       dial_print_float(double v);
void       dial_print_bool(int64_t b);      // 0/1 → false/true
void       dial_print_char(int64_t code);   // 输出码点对应的 UTF-8 字符 + 换行
void       dial_print_string(DialString s);
DialString dial_read_line(void);                  // 已暴露为 read_line()

// 连续输出（v0.99）：均**不追加换行**，是「拼行 / 进度条 / 逐字输出」的基本积木。
void       dial_print_raw(DialString s);      // 原样输出字符串（按字节长度，内嵌 '\0' 不截断）
void       dial_write_char(int64_t code);     // 输出单个字符
void       dial_print_raw_int(int64_t v);     // 标量版（栈缓冲，零堆分配）
void       dial_print_raw_float(double v);
void       dial_print_raw_bool(int64_t b);

// 文件 IO（v0.61）：文件句柄是不透明的 FILE*（经 void* 传递），显式 open/close，不受 ARC 管理。
typedef void *DialFile;
DialFile  dial_file_open(DialString path, DialString mode);   // 已暴露为 open(p, m)，失败返回 NULL
DialString dial_file_read(DialFile f, int64_t n);            // 已暴露为 read(f, n)，读最多 n 字节
int64_t   dial_file_write(DialFile f, DialString content);   // 已暴露为 write(f, c)，返回写入字节数
void      dial_file_close(DialFile f);                       // 已暴露为 close(f)，空指针安全
```

**`stdlib/dial_bytes.h`**（v0.99：与 `string` **共享同一 C 布局** `[refcount][len][data...]`，但 Dial 侧类型独立、零隐式转换）

> 注意：C 侧 `bytes` 与 `string` 是**同一底层表示**，所以这里的 C 签名一律用 `DialString`
> （而不是另起一个 `DialBytes` typedef）。`dial_bytes_*` 与 `dial_string_*` 共享 ARC 原语
> （`dial_bytes_retain` / `dial_bytes_release` 内部走同一套引用计数）。

```c
DialString dial_bytes_from_string(DialString s);   // 已暴露为 bytes_from_string(s)
DialString dial_bytes_to_string(DialString b);      // 已暴露为 bytes_to_string(b)
DialString dial_bytes_zero(int64_t len);           // 已暴露为 bytes_new(len)，全零填充
int64_t    dial_bytes_length(DialString b);        // 已暴露为 bytes_length(b)
int64_t    dial_bytes_get(DialString b, int64_t i);// 已暴露为 bytes_get(b, i)，越界 Fail-Fast
DialString dial_bytes_clone(DialString b);         // 独立副本（深拷贝）
void       dial_bytes_retain(DialString b);
void       dial_bytes_release(DialString b);
```

**`stdlib/dial_map.h`**（与 `string` / `int[]` 共用 ARC；已暴露为 `map` 类型）

```c
DialMap  dial_map_new(void);                           // 已暴露为 new_map()
DialMap  dial_map_put(DialMap m, int64_t key, int64_t val); // 返回（可能更新的）指针；已暴露为 m.put(k,v)
int64_t  dial_map_get(DialMap m, int64_t key);         // 未命中返回 -1；已暴露为 m.get(k)
int64_t  dial_map_contains(DialMap m, int64_t key);    // 命中返回 1；已暴露为 m.contains(k)
int64_t  dial_map_remove(DialMap m, int64_t key);      // 移除并返回其值，未命中返回 -1；已暴露为 m.remove(k)
int64_t  dial_map_size(DialMap m);                    // 已暴露为 m.size()
void     dial_map_retain(DialMap m);                  // 与 string/array 对称
void     dial_map_release(DialMap m);
```

---

## 16. 内置 `print` 与输出三件套

```dial
print(e);
```

- `print` 是**语句**，返回 `void`，因此**不能**出现在赋值右侧、返回值或任何需要值的位置：

  ```dial
  let n: int = print("hi");   // ❌ 编译错误：void 与 int 不兼容
  ```

- **可变参数**（v1.00 起）：接受 1 个或任意多个参数，按声明顺序连续输出、末尾只追加一个换行；
  `print("a=", 1, " b=", 2.5)` 输出 `a=1 b=2.500000`。单参数仍是最常见形式。
- 支持的参数类型：`int`、`float`、`bool`、`char`、`string`、`error`。
- **不支持** `int[]`、`map`、结构体与各类句柄（`file` / `thread` / `channel` / 包句柄）——
  必须逐元素 / 逐字段打印，或先转成字符串。
  若放任会把二进制 payload 当字符串吐出乱码，故在编译期直接拒绝。
- 输出格式：

  | 类型 | 格式 | 换行 |
  | --- | --- | --- |
  | `int` | 十进制整数 | ✅ |
  | `float` | `%f`（6 位小数） | ✅ |
  | `bool` | `true` / `false` | ✅ |
  | `char` | 该码点对应的 **UTF-8 字符**（不是码点数字） | ✅ |
  | `string` | 内容（按字节长度原样写出） | ✅ |
  | `error` | 错误消息字符串 | ✅ |

### 16.1 连续输出：`print_raw` / `write_char`（v0.99）

`print` 永远在末尾追加 `\n`，因此**需要连续输出的程序在 v0.99 之前写不出来**
（拼行表格、进度条、逐字动画、协议帧）。v0.99 补齐两个内置：

| 内置 | 签名 | 与 `print` 的差别 |
| --- | --- | --- |
| `print_raw` | `print_raw(x: int \| float \| bool \| char \| string \| error): void` | **唯一差别：不追加 `\n`**。接受的类型集合与标量文本形态都完全一致 |
| `write_char` | `write_char(c: char): void` | 只接受 `char`；**不追加 `\n`**。与 `print(char)` 的差别同样只是那个换行符 |

```dial
print_raw("[");
print_raw("alpha");
print_raw("]");
print("");        // 自己补换行 -> [alpha]

write_char('H');
write_char('i');
print("");        // -> Hi
```

完整语义、示例与实现细节见 [§5.5](#55-输出三件套连续输出v099)。

---

## 17. 未定义行为与已实现边界

诚实列出边界，避免"文档写了、实现没有"的冲突。

### 17.1 未定义行为

默认（Debug / ReleaseSafe）等级下，下列曾经的 UB **已被 Fail-Fast 消除**——它们不再是 UB，
而是"确定性崩溃"：

| 场景 | Debug / ReleaseSafe 行为 | ReleaseFast 行为 |
| --- | --- | --- |
| 数组下标越界（读/写） | **当场终止**（`int3` / `ud2`），见 §20 | 未定义（回落到 C 的行为） |
| 字符串下标越界 | **当场终止**（C Runtime 侧检查），见 §20.3 | 未定义 |
| 负下标 | **当场终止**（无符号比较一次判定） | 未定义 |
| 整数除零 | **当场终止**（LLVM `sdiv` 的 poison 是 UB） | 未定义（实测打印错误结果后继续跑） |
| 内存分配失败（OOM） | **当场终止**，绝不返回 NULL | 同左（这条检查不关闭） |
| 重复释放 / use-after-free | **当场终止**（引用计数下溢检测） | 未定义 |

仍是 UB、与 C 一致的：

| 场景 | 行为 |
| --- | --- |
| 整数溢出 | 按 i64 二进制补码回绕（Debug 等级亦不检查） |

### 17.2 尚未实现的能力（截至 v1.10）

> 已落地：`Map`（int → int，ARC 管理）、`read_line()`、短路求值、零隐式转换铁律、
> `let` + 原地修改方法（push/set/put）的接收者约束、`else if` 语法糖、`for` 范围循环、`break` / `continue`、
> Fail-Fast 运行时检查（§20）、文件 IO（§24）、可恢复异常（§26）、
> 所有权转移与多线程并发（§27，含 v0.81 的通道关闭与 EOF、v0.9 的判空与多消费者安全）、
> **`char` 类型（v0.92，见 §5.1）**、**`import` 与 `.dialpkg` 包分发（v0.93，见 §25）**、
> **三平台原生构建（v0.97：Linux x86_64 / Windows x64-MinGW / Termux aarch64-musl 全静态）**、
> **`dialc lsp` 语言服务器（v0.94 骨架 → v0.95 增强 → v0.98 工业化，见 §28）**、
> **连续输出三件套 `print_raw` / `write_char` / `str(char)`（v0.99，见 §5.5 与 §16.1）**、
> **`byte` 单字节值类型与 `bytes` 二进制缓冲区（v0.99，见 §5.6 与 §5.7）**、
> **多维数组 `int[][]` 与句柄元素数组 `string[]` / `map[]` / `error[]` / `channel[]`（v0.99，见 [§11.6](#116-多维数组与句柄元素数组) 与 [§5.2](#52-类型良构性约束)）：数组元素类型扩展到 int 与 8 字节句柄，按元素 ARC 管理所有权**。
> **v1.00 五组语言特性补齐**（复合赋值 / 位运算 / 步长与容器迭代 / `match` / `defer` / 默认参数 / variadic print / 字符串插值 / `const` / `enum` / 数组切片 / `set<T>` / 泛型 `map<K,V>`，见 [§29](#29-v100语言特性补齐)）。
> 其余仍按计划未实现：

| 能力 | 状态 |
| --- | --- |
| 裸块 `{ ... }` | **已支持**（v0.99）：裸块是引入新词法作用域的独立语句，支持变量遮蔽与堆类型局部变量的块内释放，见 [§8.6](#86-裸块独立的词法作用域v099) |
| 模块 / 包 / import | **已支持**（v1.04）：`import "包名";` + `dialc install <本地文件\|URL>`（v0.93 / v0.96，见 §25）；**源码形式的模块拆分**也已落地（`import "./util.dial";` 合并编译，无需 `mod` 关键字，见 §34）。**尚未实现**：跨工作区的全量模块图 |
| 泛型、接口、trait、闭包 | **用户自定义泛型 / 接口 / trait / 闭包仍不支持**；`channel<T>`、`set<T>`、`map<K,V>` 是 **v1.00 已落地的内建特化容器语法**（见 [§29](#29-v100语言特性补齐)），不是用户泛型 |
| 裸指针、地址取值、指针算术 | **v1.02 已支持裸指针**（`rawptr<T>` + `&x` / `*p` / `p.offset(n)` / `raw_alloc` / `raw_free`），但它**绝对不受 ARC 管理**，必须手动释放，且与 ARC 容器之间没有隐式转换——见 [§31.4](#314-裸指针不受-arc-管束的显式契约)。**用户自定义泛型的指针运算**仍不提供 |
| 运算符重载 | **永不提供** |
| 可变全局状态（`var` 全局变量） | **永不提供**（严防数据竞争）。宪法允许的是文件级 `const` **只读**常量（含数组字面量），编译期降维到 `.rodata` 或内联，不参与运行期可变状态；精确语义与落地批次见 §37.1 |
| 数组/结构体字面量作为常量表达式 | 文件级 `const` 的数组字面量按 §37.1 的宪法裁决纳入只读常量；具体落地状态见 §37.4 批次表 |
| `select` 语句级超时（`select` 块本身没有超时参数） | **未实现**——`select`（v0.91 已落地）一旦进入就阻塞到「有通道就绪」或「全体 EOF」，**没有超时参数**。要「最多等 N 毫秒」，用**已实现**的 `recv_timeout(ch, ms)`（v0.9 起，作用于**单个通道**）——它不能替代多路复用，只是「等一会儿再看」的手段 |
| 通道关闭的「反向」操作（重新打开） | **永不提供**——关闭不可逆，这是 `done` 单调性的前提 |
| 线程池、协程、异步 | 未实现 |
| 原子类型、无锁数据结构 | 未实现 |
| `move_to` 到**已启动的**线程 | 编译期不支持——移交只能在 `spawn` 时完成（见 [§27.3](#273-零竞争移交与-o1-论证)） |
| 通道元素为标量（`int` / `float` / `bool`） | 编译期拒绝（需装箱，见 [§27.2](#272-channelt-与所有权移交)） |
| 一等函数 / 函数指针 | **已支持**（v1.03）：`fn(A, B): R` 一等函数指针、函数名作右值、高阶数组方法与 C 回调（见 §32）。**闭包仍不支持**：跨回调状态须显式经 `rawptr<void>` 透传 |
| `map<K,V>` 的 `string` 值、`map.values()` 对非 `int` 值 | **未实现**（v1.00）：`V` 只支持标量，`values()` 只支持 `V = int`——`get` 未命中时的默认 string 对象会带来所有权移交歧义，见 [§29.5](#295-重型类型与容器) |
| LSP 的重命名 / 跨文件引用查找 / 格式化 | **已实现**（v0.98 轮 2 / 轮 3）——重命名与引用查找基于索引器的**引用边**（`src/index.rs`），格式化只做「去行尾空白 + 末尾换行」这类**绝不重排**的非破坏性动作。能力清单见 [§28.7.1](#2871-当前-lsp-能力清单)。另见 v1.04 新增的 `dialc fmt` 命令行格式化（§34）。**尚未实现**：跨工作区的**全量**引用图（当前只覆盖已打开文件与已加载包） |
| GUI 控件树 / 布局 / 信号槽 | **十五层已落地**（`gui.dialpkg`，见 [§38](#38-手搓-gui-地基包guidialpkg)）：地基层 + 控件框架层（Widget 树 / 绝对·VBox·HBox 布局 / 命中 / 主动分发）+ 成品控件外观层（Label / Button / CheckBox + 主题表驱动四态绘制 + 纯值事件推进）+ 输入层（TextBox 单行编辑 / 光标·选区 / 焦点链 / 统一功能键码 / XTest 实窗注入）+ 主题层/控件扩展（14 个可覆写颜色角色 + Radio / Slider / ProgressBar + value / range / group）+ 信号槽/控件事件订阅（6 类可订阅纯值信号 + 每树环形队列，见 §38.12）+ 菜单/对话框覆盖层（顶栏菜单 + 弹出菜单 + 模态对话框，复用纯值信号队列，见 §38.13）+ 滚动视图层（视口裁剪 + 滚动条几何 / 命中 / 绘制 + 偏移推进，零 DISPLAY 依赖，见 §38.15）+ 右键上下文菜单层（复用第 7 层菜单 + 锚点 / 命中 / 悬停 / 选中 / 绘制，零 DISPLAY 依赖，见 §38.16）+ 悬浮提示层（文本 + 位置 + 尺寸 + 悬停计时，延迟由调用方推进、零时钟依赖，见 §38.17）+ 状态栏层（左 / 右文本 + 进度条槽位，进度值越界夹紧、零时钟依赖，见 §38.18）+ 通知气泡层（FIFO 消息队列，生命周期纯值推进、到期自动消隐、零时钟依赖，见 §38.19）+ 窗口级覆盖层自动集成（5 类覆盖层挂到窗口槽位，`win.draw()` 按固定 z-order 自动叠加、借用语义、空槽位零像素影响，见 §38.20）+ 快捷键 / 命令系统（KeyMap + 命令启用态，纯内存表、零 DISPLAY 依赖，
  见 §38.21），共 **279 个 `extern fn`**（截至第 15 层；第 16 / 17 / 18 层追加见 §38.22 / §38.23 / §38.24，现共 **344 个**）；**第 8 层 Dial 侧 OO 封装**（`gui_oo.dial`，278 个 `impl` 方法，现 343 个，纯源码、随包 `src/` 分发（装包即用）、零 `extern fn` 增量，见 §38.14）。撤销重做 / 每控件主题覆写 / Slider 拖动 / Grid 自动换行已于第 17 层落地（见 §38.23）；高 DPI 整数放大 / ListBox 虚拟化 lazy load / IME 预编辑纯值事件 / GPU+原生平台纯值契约层已于第 18 层落地（见 §38.24）。**仍未实现**：真实 GPU 合成后端与原生弹窗/托盘/全局快捷键实机、IMM32/IBus/Fcitx 输入法平台采集、多行文本 / 富文本排版、C → Dial 反向回调、图片解码、可切换深色主题包。Win32 后端**仅交叉编译、未实机验证**（XTest 实机注入仅在 Linux/Xvfb 验收） |
| `impl` 方法块（B1） | **已支持**（v1.10 开发中增量）：`impl T { fn m(self, ...) }` 让用户 `struct` 与包不透明句柄（如 `GuiWindow` / `GuiMenu`）挂方法，`obj.m(args)` 编译期降级为静态调用（self 即首参），**ABI 不变、旧签名一字不动、不新增关键字**（`impl` 仍可作普通标识符），见 [§37.12](#3712-b1impl-方法块v110-开发中增补落地)。**现役边界**：只支持具名类型（`Struct` / `Opaque`）；方法内 `self.f = ...` 因 Dial struct **值语义**只改副本、不回写调用者（与 Rust `&mut self` 不同）；暂不支持同类型变参 `...xs`、C ABI 导出、对标量/内建容器挂 impl、关联函数（无 self 的 `T::new()`）。B2 泛型 / B3 枚举 ADT / B4 闭包 / B5 重载维持原裁决 |

---

## 18. 测试

```bash
./tests/run_tests.sh
```

`bash tests/run_tests.sh` 的**当前全量回归是 A–Z + AA + AB 共二十八组**（535 ✅ / 0 ❌），
全部通过才算绿。下表逐项详列 **A–M 十三组**——这组逐组说明自 v0.9x 沿用至今、
内容未变，且每一组至今仍在执行；**N–Z 与 AA、AB 组**分别见 §33–§38 各章与
`tests/run_tests.sh`（其中 AA 组是手搓 GUI 地基包 `gui.dialpkg` 的专项回归，见 §38）：

| 组 | 内容 |
| --- | --- |
| **A. 正向** | `examples/*.dial` 必须编译成功，且运行输出与同名 `.expected` 黄金文件**逐字节一致** |
| **B. 负向** | `tests/bad/*.dial` 必须编译失败，用于锁定诊断信息不被弱化 |
| **C. 契约** | 产物可执行文件不得依赖 libLLVM（只链 libc） |
| **D. Fail-Fast** | `tests/trap/*.dial` 必须**当场因信号终止**（SIGILL/SIGTRAP/SIGSEGV），且**不得打印哨兵 9999**——证明它在越界那一行就停了，而不是带病跑完 |
| **E. 对照组** | 同一批用例在 `--fast` 下应当关闭检查并跑完（证明陷阱确由编译器插入，而非碰巧崩溃） |
| **F. 并发压测**（v0.9 新增） | 绕开编译器直压 `stdlib/dial_channel.c`：8 个消费者抢同一个通道、100 万元素、队列深 64。校验「每个元素恰好被取走一次」（位图 + 原子 `fetch_or`，重复领取即失败）与「总数守恒」。若发生丢唤醒，消费者 `join` 会永远挂住，由 timeout 判失败 |
| **G. 未捕获异常**（v0.92 新增） | 内建函数的隐式抛出允许写在 `try` 外：必须编译通过，运行期 panic 终止并打印**真实消息**（锁定的是「消息没被当成整数打印」这个曾经的缺陷）。不参与 E 组对照——异常不是安全等级检查，`--fast` 不该关掉它 |
| **H. 包分发**（v0.93 新增） | 构建示例 `.dialpkg` → 安装 → `import` 后编译运行比对黄金输出 → **篡改一个字节后必须被 SHA256 拦下**，绝不允许流到链接器 |
| **I. LSP**（v0.94 新增） | 真起一个 `dialc lsp` 子进程、按 JSON-RPC 往返：初始化能力、诊断定位到**出错那一行**、语义 token 的差分解码与**源码子串反查**（含中文的 UTF-16 换算）、`shutdown` 后干净退出。见 `tests/lsp_smoke.py` |
| **J. 包安装三条硬规则**（v0.94 收尾） | `dialc install`：① 同名包 **ABI 相同则覆盖、不同则报错要求先卸载**；② `targets` 的库文件**只允许 `.a` / `.lib` 静态库，拒绝 `.so` / `.dll` 动态库**；③ 卸载即 `rm -rf ~/.dial/packages/<名>/`（缓存是纯派生数据）。三条均被 J 组锁死 |
| **K. LSP 增强**（v0.95 新增） | Hover / 跳转定义 / 补全：真起 `dialc lsp` 子进程按 LSP 协议往返，验证变量遮蔽的 Hover、跨文件 `.diali` 文档、跳转定义（文件内→本文件、包符号→`.diali`）、可见符号收集与前缀过滤。见 `tests/lsp_features.py` |
| **L. 包安装**（v0.96 新增） | **URL 安装**：真起一个本机 HTTP 服务器提供 `.dialpkg`，用 `dialc install <URL>` 装——正常装得上，`--sha256` 钉死也能装；坏包（内容篡改 / 哈希不符 / 404 / 非 https）**一律装不进去**，且失败后缓存目录无残骸。**一键脚本**：`install-dial.sh` 初始化环境（编译器 + `stdlib/` + `runtime/`）→ 自动扫描并安装 `.dialpkg` → 在**与源码树无关的独立项目目录**里编译 import 程序并运行。见 `tests/pkg_install_url.py` |
| **M. LSP 全能力 + 协议流纯净性**（v0.98 新增） | 真起 `dialc lsp` 子进程把轮 2~4 的能力全跑一遍（文档大纲 / 工作区符号 / 引用查找 / 重命名 / 签名帮助 / 格式化 / inlay 提示 / 代码动作 / 折叠 / 选择范围 / 文档链接），并断言 **stdout 上只有协议字节**——混进一行裸文本就会让客户端分帧错位，症状是**编辑器卡死**而非报错；另配一道**静态**守卫：`tools/check_lsp_stdout_purity.py` 扫 LSP 运行时用到的模块与函数，出现 `println!` 即失败。见 `tools/lsp_full_smoke.py` |

#### 持续集成（GitHub Actions）

二十八组检查由 `.github/workflows/ci.yml` 在**每次 push / PR** 上自动执行
（AA 组 GUI 实窗验收在无头 Xvfb 下运行；无显示环境时明确跳过并标注，绝不伪装通过）：

| 阶段 | 做什么 | 不通过的后果 |
| --- | --- | --- |
| **编译** | 装 LLVM 18 + Rust → `cargo build --release --locked` | 立即失败，PR 不可合并 |
| **测试** | `bash tests/run_tests.sh`（A–Z+AA+AB 全部二十八组，含 AA 组 Xvfb GUI 实窗验收与 AB 组 FFI 32 位往返） | 立即失败 |
| **CVE 暴力检测** | `python3 tools/cve_check.py --repeat 10`——8 个已登记漏洞各有专门 PoC，每例重复 10 次跑，确认**修复后确实不再复现** | 立即失败（漏洞回归 = 修复失效，必须当场发现） |
| **锚点校验** | `python3 tools/check_links.py`——README / CHANGELOG 的内部链接必须全部可解析 | 立即失败（防止再次出现「点不动的链接」） |
| **协议流纯净性** | `python3 tools/check_lsp_stdout_purity.py`——LSP 运行时用到的模块与被 LSP 调用的函数里不得出现 `println!` | 立即失败（stdout 是协议流，裸输出会让编辑器卡死） |
| **版本号一致性** | `python3 tools/check_version.py`——README / CHANGELOG / 官网三处产物的版本号必须一致 | 立即失败（防止「代码已发版、文档还停在旧版」） |
| **官网同步** | 重新生成 `docx/` 并与仓库内容比对 | 立即失败（文档改了但官网没重新生成） |

打 **`v*` 标签**时，额外的 `release` 作业会把编译器二进制一并打进
`dialc-v<版本>-<平台>.zip`（**三个平台各一个**：linux-x86_64 / windows-x86_64 / termux-aarch64；
含源码、`.git`、四份许可文件、`docx/`、VS Code 扩展），
上传为 GitHub Release 产物——即官网首页的下载链接。
打包脚本 `tools/package.sh` 有**硬性校验**：许可文件不全、或缺 `.git`、或混入构建产物，一律拒绝出包。

### 示例清单

| 示例 | 覆盖点 |
| --- | --- |
| `hello.dial` | 最小程序 |
| `flow.dial` | 变量、`while`、`if/else`、函数调用 |
| `struct_demo.dial` | 结构体：定义、字面量、字段读写、嵌套、作为参数与返回值 |
| `string_demo.dial` | 字符串全部方法、`==`/`!=` 内容比较 |
| `array_demo.dial` | 数组字面量、`push`/`get`/`set`、下标读写、作为函数参数 |
| `cow_demo.dial` | 移动语义、只读共享、COW 触发与隔离验证 |
| `record_demo.dial` | 结构体持有 `string` 与 `int[]` 的组合场景 |
| `ownership_stress.dial` | 所有权压力测试：栈槽复用 + 引用计数平衡 |
| `short_circuit.dial` | `and` / `or` 短路求值（以"副作用是否被触发"证明） |
| `conversions.dial` | `int_to_float` / `float_to_int` 显式转换（零隐式转换铁律） |
| `thread_spawn.dial` | 线程创建、`spawn` 移交 string、`join` |
| `channel_move.dial` | `move_to` 把所有权移交进通道，`recv` 取回 |
| `channel_thread.dial` | 子线程 `move_to` → 主线程 `recv` → `join` |
| `channel_array.dial` | `channel<int[]>` 传数组 |
| `thread_scalar.dial` | 标量实参按值拷贝（源变量不 Moved，之后仍可用） |
| `channel_close.dial` | 生产者 `close` → 消费者 `done` 收尾（EOF 全流程） |
| `channel_try_recv.dial` | `len` / `try_recv` / `closed` / `done` 状态机 |
| `is_null_demo.dial` | `is_null` 判别 EOF / 超时 / 真实值三种情形 |
| `recv_timeout.dial` | `recv_timeout` 超时返回空值；超时不会吞掉元素 |
| `channel_multiconsumer.dial` | **两个消费者抢同一个通道**，元素总数守恒（多消费者安全） |
| `select_two_channels.dial` | **双通道 select**：两个生产者往不同通道发数据，一个消费者同时监听；顺带校验没有串通道 |
| `select_default.dial` | `default` 分支（非阻塞语义）：队列空时每一轮都走 default |
| `select_eof.dial` | **全体 EOF 时 select 直接结束**，不执行任何分支 |
| `select_fairness.dial` | 多通道同时就绪时的**随机选取**（防饿死）：两个通道都必须被处理过 |

### 负向用例清单（节选，全部位于 `tests/bad/`）

| 用例 | 锁定行为 |
| --- | --- |
| `let_push.dial` | `let` 变量调用 `push` 必须在编译期拒绝 |
| `implicit_conv.dial` | `let y: float = x`（x 为 int）隐式转换必须在编译期拒绝 |
| `mixed_arith.dial` | `1 + 2.0` 这类 int/float 混合算术必须在编译期拒绝 |

---

## 19. 内存宪法（防 GC 铁律）

> 这是 Dial 区别于 Java / Go / C# 的**核心底线**，也是本语言不可动摇的内存契约：
> **绝不背追踪式垃圾回收的历史包袱。** 以下四条在任何版本中都不得违反。

### 19.1 永远拒绝 GC

Dial 的设计**绝不引入任何形式的追踪式垃圾回收（Tracing GC）**——没有 mark-sweep、没有 STW（Stop-The-World）停顿、没有分代回收。
内存回收的时机与开销在 Dial 程序里必须是**确定性的、可推理的**，而不是由运行时在某次不可预期的 GC 周期里"替你决定"。

这是 Dial 与 Java / Go / C# 最根本的分道扬镳：后者把内存管理的不可预测性藏进运行时，Dial 把它摊在编译期与显式契约上。

### 19.2 编译期接管内存

Dial 依托**不透明指针 + C Runtime 的 ARC（自动引用计数）**，结合编译器前端的**逃逸分析与移动语义**，实现"零 GC 停顿的确定性内存管理"：

- 值类型（如结构体）在栈上分配，作用域结束即回收，**零运行时参与**；
- 引用类型（`string` / `int[]` / `Map`）由 ARC 管理；编译器在编译期通过数据流分析，尽量把 `retain` / `release` 静态消除
  （移动语义零开销、只读共享 O(1)），只有跨边界等静态分析失效处，才由 C Runtime 在**写入瞬间**按引用计数兜底（COW，见 §14）；
- **没有任何后台线程、没有并发回收、没有不可预期的暂停。**

#### ARC 不解决循环引用——这是它的已知边界，不是遗漏

`string` / `int[]` / `map` 的 ARC 是**引用计数**，不是追踪 GC。因此**互相持有引用会形成引用环**：
环上每个对象的计数都 ≥ 1，「归零即释放」的条件**永远不成立**，结果是**内存泄漏**。

必须把两种后果分清楚：

| 后果 | 性质 | Dial 的选择 |
| --- | --- | --- |
| **内存泄漏**（环上的对象永不释放） | 安全——程序逻辑照常正确，进程退出时由 OS 回收 | **接受**。绝不用追踪 GC 去「自动」打破环：那会引入停顿与不可预测的回收时机，与 §19.1 的底线直接冲突 |
| **悬垂指针 / 重复释放**（提前释放） | **不安全**——会读写已释放内存 | **绝不容忍**。ARC + 编译期分析保证「释放后不再有人持有该引用」 |

> 一句话：**宁可泄漏，不可悬垂。** 这是 Dial 内存模型的取舍，不是待补的缺口。

环的成因是可以靠纪律规避的，因此有明确的处置办法：

| 场景 | 处置 |
| --- | --- |
| 值类型数据（结构体、标量） | **不产生环**——结构体按值传递，其字段里的指针仍指向独立对象 |
| 确实需要「互相引用」的图结构 | 用**不透明句柄**（`file` / `thread` / 包的 `Type::Opaque`）：它们**不受 ARC 管理**，由 `close` / `join` 显式回收，**天然不参与引用环** |
| 确需共享同一份数据 | 用**只读共享**（多个持有者只读同一 `string` / `int[]`）——只读不构成环 |
| 未来需要可变共享的图 | 走 [§19.3](#193-显式指针回收裸指针的显式契约) 的 `rawptr<T>` + 显式 `raw_free`，**而不是**依赖 GC |

### 19.3 显式指针回收（裸指针的显式契约）

> **v1.02 起已落地**：裸指针（`rawptr<T>`）正式成为语言的一部分，实现与本节契约逐条对齐，
> 详见 [§31.4](#314-裸指针不受-arc-管束的显式契约)。

Dial 对裸指针坚持**"谁申请、谁释放"**的显式契约：

- 裸指针**绝对不受 ARC 保护**，不会被任何引用计数自动回收；
- 它的生命周期完全交由开发者显式管理，例如调用底层 `dial_raw_free` 归还；
- 这一设计与 §19.2 的 ARC 容器**互补**：日常用 ARC 容器获得确定性安全，需要极致性能或底层控制时再用裸指针并自行负责回收；
- 裸指针与 ARC 容器之间**不允许**隐式转换——这正是"不透明指针契约"的延伸，避免把受管指针与不受管指针混为一谈而引入悬挂。

### 19.4 为什么这是底线

| 语言 | 内存回收方式 | Dial 的立场 |
| --- | --- | --- |
| Java / Go / C# | 追踪式 GC（存在 STW 停顿） | ❌ **永不引入** |
| C / C++ | 手动 `free` / RAII | 借鉴"确定性"，但用 ARC + 编译期分析消除大部分手写负担 |
| **Dial** | ARC + 编译期移动/逃逸分析 + 显式裸指针（v1.02 起） | ✅ 确定性、零 GC 停顿、可控 |

> 这条底线写进语言本身，而非仅仅写在文档里：任何试图为 Dial 添加追踪式 GC 的提案，都在原则上被否决。

---

## 20. Fail-Fast 铁律（宁可崩溃，绝不静默损坏）

> **宁可当场段错误（Segfault / Illegal Instruction），也绝对不允许产生未定义行为（UB）或静默产生错误数据。**

这是借鉴 Zig 的错误处理哲学，与 [§19 内存宪法](#19-内存宪法防-gc-铁律) 并列为 Dial 的两条工程底线：
内存宪法管"内存怎么回收"，Fail-Fast 管"出错时怎么办"。

### 20.1 拒绝 C 语言的静默损坏

C 的越界访问不会崩溃，而是**悄悄修改相邻内存然后继续跑**，Bug 往往在几万行之后
才以完全无关的形态爆发，排查成本极高。Dial 拒绝这种模式：

| 非法状态 | C 的行为 | Dial 的行为 |
| --- | --- | --- |
| 数组越界读 | 返回相邻内存的垃圾值，程序继续 | **当场终止**，不返回任何值 |
| 数组越界写 | 悄悄改坏别人的数据，程序继续 | **当场终止** |
| 负下标 | 读写对象头 / 前一块内存 | **当场终止** |
| 字符串越界 | 读到 `'\0'` 之后的字节或越界 | **当场终止** |
| 整数除零 | x86 上 SIGFPE；但 LLVM 层是 **poison（UB）** | **当场终止**（见 20.3） |
| 内存分配失败 | 返回 NULL，上层带病运行 | **当场终止**，绝不返回 NULL |
| 重复释放 / use-after-free | 静默损坏堆管理结构 | **当场终止**（引用计数下溢检测） |

### 20.2 LLVM Codegen 的硬性插入

在生成 LLVM IR 时，编译器**主动插入**运行时检查；失败分支用 LLVM 内建函数发射陷阱指令：

| 安全等级 | 陷阱 intrinsic | x86 机器码 | 进程信号 |
| --- | --- | --- | --- |
| Debug（默认） | `llvm.debugtrap` | `int3` (0xCC) | SIGTRAP（5），可被调试器当断点捕获 |
| ReleaseSafe | `llvm.trap` | `ud2` (0F 0B) | SIGILL（4） |
| ReleaseFast | —— | —— | 无（检查被完全消除） |

非法指令由操作系统立刻接管并 Core Dump，调试器能**精确停在越界的那一条指令上**。

真实反汇编（`a[99]`，长度 3，Debug 等级）：

```asm
1001f76: call dial_array_length      # 取长度
1001f7b: cmp  $0x63,%rax             # 与下标 99 比较（0x63 = 99）
1001f7f: jbe  1001fb2                # ★ 越界则跳转
1001f81: mov  $0x63,%esi
1001f89: call dial_array_get         # 合法才真正访问
...
1001fb2: int3                        # ★★ llvm.debugtrap：当场终止
```

### 20.3 边界检查与断言（C Runtime 侧）

底层 C Runtime 的所有下标访问**必须**先做边界检查，越界直接触发底层 trap。
这是第二道防线——即便 codegen 检查被关闭，Runtime 仍不放过越界：

```asm
# dial_string_get（C Runtime，DIAL_SAFETY >= 1）
1001e3a: mov  (%rdi),%rsi            # len = s[0]
1001e3d: test %rbx,%rbx              # idx < 0 ?
1001e40: js   1001e47                #   是 -> fail
1001e42: cmp  %rbx,%rsi              # len > idx ?
1001e45: jg   1001e4f                #   是 -> 合法路径
1001e47: call dial_bounds_fail       # ★ 打印诊断后 ud2 终止
1001e4f: movzbl 0x8(%r14,%rbx,1),%eax
```

为什么**除零也必须检查**：LLVM 的 `sdiv` 在除数为 0 时结果是 **poison**（未定义行为），
优化器有权据此认为该路径不可达，进而把后续代码整段删掉。**实测**：关闭检查后
`100 / 0` 打印出 `0` 并继续执行——程序没崩，却给出了完全错误的结果。这比崩溃危险得多。

### 20.4 性能模式的切换

```bash
dialc build app.dial                 # Debug（默认）：崩溃保护全开
dialc build app.dial --release       # ReleaseSafe：保留防 UB 检查，陷阱用 ud2
dialc build app.dial --fast          # ReleaseFast：关闭全部检查，越界即 UB
dialc build app.dial --safety debug  # 显式指定（debug / safe / fast）
```

> <span id="warn-fast">⚠️ **关于 `--fast` 模式（ReleaseFast）**</span>
>
> **此模式会关闭所有边界检查、除零检查和空指针保护，以换取极致的性能。**
> **除非你是正在编写底层核心算法、且完全确信你的数组下标绝对不会越界，否则请永远不要使用此模式。**
> **在 `--fast` 下发生的越界，将退化为 C 语言的未定义行为（UB），可能导致静默的错误数据甚至灾难性后果。**

同一个等级**同时**作用于 LLVM IR 与 C Runtime（通过 `-DDIAL_SAFETY=N` 注入），
保证两侧检查口径一致。等级是编译期常量，ReleaseFast 下检查代码被编译器完全消除：

```asm
# ReleaseFast 下的 a[99]：无长度调用、无比较、无跳转、无陷阱
1001e14: call dial_array_length      # 这一条来自源码里显式的 a.length()
1001e2a: call dial_array_get         # 直接访问
1001e40: call dial_array_get
```

> **ReleaseFast 的后果由使用者自行承担**：它换来的性能，代价是回到 C 的静默损坏模式。
> 默认等级永远是 Debug——安全性是默认值，性能才是需要显式选择的东西。

### 20.5 崩溃时保留上下文

`dial_bounds_fail` / `dial_null_fail` / `dial_assert_fail` 在触发陷阱前会：
① 冲刷 stdout 缓冲（否则崩溃前已经 print 的内容会随之消失，让人误判崩溃位置）；
② 用 `write(2)` 直接写 stderr 打印原因（不用 stdio，避免缓冲来不及 flush）。

```
dial: 下标越界（Fail-Fast 终止）: index = 99, length = 3
[1]    12345 illegal hardware instruction (core dumped)  ./app
```

---

## 21. 代码编写纪律（防臃肿与允许冗余原则）

本节约束**编译器前端（Rust）与 C Runtime 自身的实现代码**，而非 Dial 用户代码。
它与 [§19 内存宪法](#19-内存宪法防-gc-铁律) 同为工程底线：一个是内存模型的底线，一个是代码形状的底线。

### 21.1 绝对拒绝过度抽象

严禁为了复用而把代码抽象成极度复杂的高阶函数、宏或泛型。
**如果复制粘贴几行代码能让逻辑更直观、调试更方便，就果断复制粘贴——宁可 WET，绝不 DRY 过头。**

- 典型体现：`array_method_type` 与 `map_method_type` 是**两份各自独立的直白函数**，
  不合并成一个带泛型的"通用方法签名解析器"；`dial_array.c` 与 `dial_map.c` 也各自独立实现，
  不为省几行而引入共享的"通用容器抽象层"。
- 判据：新增一个抽象前先问——它是让**读代码的人**更快理解，还是只让**写代码的人**少敲几行？后者一律不做。

### 21.2 不引入无用依赖

Rust 前端与 C Runtime 的代码**尽量只依赖标准库**。任何新引入的 Crate 或 C 库，
必须能证明它**不可替代且极其轻量**。

- 当前事实：`Cargo.toml` 的唯一依赖是 `inkwell`（LLVM 绑定，不可替代——需要它 emit 目标文件）；
  C Runtime 只用 `<stdlib.h>` / `<string.h>` / `<stdio.h>` / `<stdint.h>`。
- 判据：能用 `for` + `if` 写完的，绝不引一个 Crate；能自己算的哈希，绝不引哈希库。

### 21.3 允许安全与性能上的冗余

明确**允许**两类冗余，它们不是"多余代码"：

| 冗余类型 | 允许的形式 | 现实例子 |
| --- | --- | --- |
| 边界防御 | 跨语言（Dial ↔ C）边界处的少量防御性判断，防止崩溃 | `dial_map_put` 在共享时先 COW 再改；`dial_read_line` 在 `fgets` 返回 NULL 时返回空串而非空指针；C Runtime 每个下标访问前都做边界检查（§20.3） |
| 性能冗余 | C 数据结构中为内存对齐、预分配留出的冗余空间 | `dial_map` 的 entry 用 `int64_t used`（而非 `int8_t`）保持 8 字节对齐；`new_map()` 直接预分配 `cap = 8` |

> Fail-Fast 的检查本身也是**被明确允许的冗余**：它让"安全"成为默认值，
> 把性能留给需要的人显式选择（`--fast`）。这与 §20 是一致的，而非冲突。

### 21.4 保持代码直白

**能用一个循环和一个 `if` 解决的事情，绝对不要用设计模式套壳。**

- 线性探测就是一层 `while` 循环查找空槽；删除就是一层"左移紧凑"循环——不套迭代器、不套策略模式。
- 编译器的 pass 就是顺序执行的几个函数（收集签名 → 检查函数体 → 数据流分析 → 代码生成），
  不引入 visitor 框架或 IR 中间层的插件机制。

---

## 22. 早期版本摘要（v0.3 及以前）

> 本节仅保留 **v0.3 及以前** 的相对变更摘要（早期历史）。
> **v0.4 及之后** 的每个版本相对变更、跨文件符号表（v0.95）等**完整记录见 [CHANGELOG.md](CHANGELOG.md)**，
> 官网渲染为[版本索引 changelog.html](docx/changelog.html)与 `docx/changelog/` 下**每版本一页的详情**
> （正文与 CHANGELOG.md 对应条目逐字一致；索引 + 分页是为避免单文件 200 KB 的长滚动与移动端加载成本，
> 并消除跨版本重复小节标题的锚点撞名）；最新版本号与能力矩阵见 [首页 index.html](docx/index.html)。

### 22.0 v0.61（相对 v0.6）

本版落地**文件 IO（file 句柄）**，并正式确立[版本号规则](#23-版本号规则)（最多两位小数、禁止跳版、严格按时间递增）：

| 新增 / 修正点 | 说明 |
| --- | --- |
| **`file` 不透明指针类型** | 新增 `file` 类型，IR 中为 `ptr`；封装 C 的 `FILE*`，**不受 ARC 管理**，由用户显式 `open` / `close`（与内存宪法 [§19](#19-内存宪法防-gc-铁律) 显式指针回收一致） |
| **`open` / `read` / `write` / `close`** | 仅封装 C 标准库 `fopen` / `fread` / `fwrite` / `fclose`，Rust 前端不重写任何文件逻辑；最基础的 C 风格 API，无链式流、无额外语法糖 |
| **错误处理（无异常）** | 严格遵循「无异常」原则：打开失败返回**空指针**、读失败返回**空串**、写失败返回 **-1**；不为其引入 Result 封装或自动抛错 |
| **绝不引入可变全局状态** | 文件句柄必须显式传递，绝不引入全局文件状态（v1.04 §37.1 统一措辞，原写作「无全局变量」） |
| 示例与负向用例 | 新增 `examples/file_demo.dial`（写后读回并打印）；新增 `tests/bad/io_bad_arg.dial`（open 参数类型错误须在编译期拒绝） |

> 版本号按[版本号规则](#23-版本号规则)：本版属「小功能 / 新语法」，递增 `+0.01`（`v0.6 → v0.61`）。

### 22.1 v0.6（相对 v0.5）

本版把 Zig 式 **Fail-Fast** 确立为与内存宪法并列的语言底线（[§20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)）：
宁可当场崩溃，绝不产生未定义行为或静默错误数据。

| 新增 / 修正点 | 说明 |
| --- | --- |
| **Codegen 硬性插入陷阱** | 数组下标读（`Expr::Index`）、写（`a[i] = v`）、整数除法前插入检查，失败分支发射 `llvm.debugtrap`（Debug，`int3`）或 `llvm.trap`（ReleaseSafe，`ud2`），由 OS 接管 Core Dump |
| **C Runtime 边界检查** | `dial_array_get/set`、`dial_string_get`、`dial_string_substring` 全部加入边界检查，越界调用 `dial_bounds_fail` → `ud2`；空指针、OOM、重复释放同样当场终止 |
| **除零检查** | LLVM `sdiv` 除数为 0 时结果是 **poison（UB）**，优化器可据此删掉后续代码；现改为先检查再除。实测关闭检查时 `100 / 0` 打印 `0` 后继续跑——静默错误结果 |
| **三档安全等级** | 默认 Debug（`int3`）/ `--release`（ReleaseSafe，`ud2`）/ `--fast`（ReleaseFast，关闭全部检查）；等级同时注入 LLVM IR 与 C Runtime（`-DDIAL_SAFETY=N`） |
| **崩溃上下文** | 陷阱前先冲刷 stdout 缓冲，再用 `write(2)` 直接写 stderr 打印原因（如 `index = 99, length = 3`） |
| **新增测试组 D / E** | D：`tests/trap/*.dial` 必须当场因信号终止且不得打印哨兵；E：同一批用例在 `--fast` 下应关闭检查（证明陷阱确由编译器插入） |

相关文档同步更新：§1（设计原则表）、§2（目录结构）、§3.3（安全等级参数）、§11.5（越界行为）、
§17.1（UB 清单重写）、§18（测试组）、§21.3（冗余原则与 Fail-Fast 的关系）。

### 22.2 v0.5（相对 v0.4）

本版补齐循环控制流，并把[代码编写纪律](#21-代码编写纪律防臃肿与允许冗余原则)正式写入规范。

| 新增 / 修正点 | 说明 |
| --- | --- |
| **`break`** | 跳出最内层循环（仅允许出现在 `while` / `for` 体内，见 §8.4） |
| **`continue`** | 进入下一次迭代；`for` 中先执行 `i = i + 1` 再回到条件判定（自增落在独立的 `for.inc` 块，避免死循环） |
| **代码编写纪律** | 新增 [§21 代码编写纪律](#21-代码编写纪律防臃肿与允许冗余原则)：拒绝过度抽象（宁可 WET）、不引入无用依赖、允许安全与性能冗余、保持代码直白 |

相关文档同步更新：§8.1（语句一览）、§8.4（`break` / `continue`）、§17.2（从「未实现」移除 break/continue）。

### 22.3 v0.4（相对 v0.3）

本版落地**部分标准库**与**语言特性扩充**，并正式写入[内存宪法](#19-内存宪法防-gc-铁律)：

| 新增 / 修正点 | 说明 |
| --- | --- |
| **`map` 类型（int → int）** | 新增哈希表类型，内存布局 `[cap][count][entries...]`，与 `string` / `int[]` 共用同一套 **ARC 引用计数**（`dial_map_retain` / `dial_map_release`）；线性探测 + 左移紧凑删除；负载超 3/4 自动扩容重哈希 |
| **`map` 方法** | `put(k,v)`（返回可能更新的指针，编译器写回接收变量）、`get(k)`（未命中 -1）、`contains(k)`（bool）、`remove(k)`（返回其值，未命中 -1）、`size()` |
| **`new_map()` / `read_line()`** | 两个无参内置函数：`new_map()` 构造空表；`read_line()` 从标准输入读一行（EOF 返回空串，绝不崩溃） |
| **`else if` 语法糖** | `else if (c) {..}` 编译期展开为嵌套 `if`（不引入新 AST 节点），见 [§8.2](#82-if--else-if--while) |
| **`for` 范围循环** | `for (i in start..end)`：循环变量 `i` 为 int，半开区间 `[start, end)`，可在体内重新赋值，见 [§8.3](#83-范围-for-循环) |
| **内存宪法写入 README** | 新增 [§19 内存宪法（防 GC 铁律）](#19-内存宪法防-gc-铁律)：永不追踪式 GC；确定性内存靠 ARC + 编译期分析 +（未来）显式裸指针 |

> 设计约束：Map 的 ARC 实现与 `int[]` 完全对称（inline-entries、put 触发扩容 / COW 并写回指针），**不引入任何非 ARC 的堆管理**，与内存宪法一致。

相关文档同步更新：§5.1（基本类型新增 `map`）、§12（哈希表专章）、§15（FFI 表与 C 导出函数新增 `dial_map_*` / `dial_read_line`）、§17.2（从「未实现」移除 Map 与 read_line）。

### 22.4 v0.3（相对 v0.2）

本版只修改**语义与诊断**，未改变任何架构契约（不透明指针、两步构建、产物零 LLVM 依赖、C 承担一切容器）。

| 修正点 | v0.2 行为 | v0.3 行为 | 动机 |
| --- | --- | --- | --- |
| **`and` / `or` 求值** | 两侧都求值，不短路 | **短路求值**，与 C 的 `&&` / `\|\|` 一致 | 现代语言常识；不短路会让"判空后访问"类代码在左操作数为假时崩溃 |
| **`int` ↔ `float` 转换** | 曾允许 `int→float` 隐式提升（算术/比较中混用） | **零隐式转换铁律**：混用即编译错误，必须写 `int_to_float` / `float_to_int` | 杜绝静默精度丢失；比 C 更严格 |
| **`let` 变量调 `push` / `set`** | 未明确定义 | **编译期拒绝**（`let` 不可重新赋值，无法接收 COW 换回的新指针）；接收者须为顶层 `var` | 把"容器被原地修改但接收者无法接收新指针"的悬垂 Bug 提前到编译期 |
| **新增内置函数** | — | `int_to_float(int): float`（LLVM `sitofp`）、`float_to_int(float): int`（LLVM `fptosi`），纯编译期内联、不经 C 运行时 | 零隐式转换铁律的"显式安全阀" |

相关文档同步更新：§5.3（零隐式转换铁律）、§6.2（可变性判定含 `push`/`set`）、§7.2（类型规则）、§7.3（语义细则表，修正 `and`/`or` 与混合算术两行）、§11.3–§11.4（接收者须为 `var`）。

---

## 23. 版本号规则

Dial 目前处于 v0.x 快速演进阶段。版本号采用**最多两位小数**的单一十进制数，
**不再使用**语义化版本的「主.次.修订」三段式。

| 变更类型 | 版本增量 | 演进示例 |
| --- | --- | --- |
| **重大更新 / 新引擎 / 大重构** | `+0.1` | `v0.6 → v0.7` |
| **小功能 / 新语法** | `+0.01` | `v0.9 → v0.91` |
| **补丁 / 次要改动** | `+0.01`（与上一种同档） | `v0.91 → v0.92` |

约定与约束（强制）：

1. **最多两位小数**：`v0.1`、`v0.2`、…、`v0.9`、`v0.91`、`v0.92`……
   不存在 `v0.601` 这类**三位连续小数**的版本——补丁不再单独占一个数量级，
   与小功能同按 `+0.01` 推进，避免版本号无意义地膨胀、也避免「补丁」与「小功能」纠缠不清。
   **唯一例外是第三位的补丁号**（如 `v0.98.1`），它不是「第三位连续小数」，
   而是「两位小数版本 + 一位修复补丁位」，适用条件见第 6 条。
2. **标题带 `v`、不写补丁位**：文档与 CHANGELOG 的版本标题一律写作 **`[v0.91]`**，
   **不是** `[0.91.0]`。源码 `Cargo.toml` 的 `version` 字段受 Cargo 三段式限制仍写作 `0.91.0`，
   那只是**存储格式**，对外一律显示为 `v0.91`。
   **带补丁位的版本例外**：补丁发布写作 `[v0.98.1]`，显示为 `v0.98.1`（补丁位必须显示，
   否则修复发布与前一版在文档上无法区分——读者无从判断自己装的是不是修复过的版本）。
3. **不可跳版**：例如不能从 `v0.6` 直接到 `v0.7`；必须按 `v0.61`、`v0.62`……逐档推进，
   除非用户明确要求跳版。
   > **v1.00 里程碑**：用户明确要求从 `v0.99.1` 直接跳到 1.00 里程碑。Cargo 的 semver
   > 不允许前导零（`1.00.0` 非法），因此 `Cargo.toml` 存 `1.0.0`、对外统一显示 **`v1.00`**
   > （`tools/check_version.py` 与 `tools/build_web.py` 在主版本 ≥ 1 时把次版本补零到两位）。
   >
   > **v1.10 跳版与发布标签**：用户明确要求以 `v1.1` 作为发布标签，其对应版本号是
   > **`v1.10`**（`Cargo.toml` 存 `1.10.0`）。`v1.1` 只是**标签名**、不是版本号；
   > 资产名一律按版本号推导（`dialc-v1.10-*.zip` / `dial-1.10.0.vsix`），下载地址走
   > `releases/latest/download/` 重定向，不硬编码标签。从 `v1.04` 跳到 `v1.10`
   > 属本条「用户明确要求跳版」的例外，数值 `1.10 > 1.04` 不倒流；完整映射与交付清单
   > 见 CHANGELOG 的 `[v1.10]` 段。
4. **版本号必须严格按时间递增，绝不许倒流**：后发版本的日期不得早于先前版本。
   CHANGELOG 中一旦出现日期倒流（例如 `v0.4` 的日期晚于 `v0.5`），即视为文档缺陷，必须当场修正。
5. **每档一次完整交付**：每一个版本号都对应一次完整交付——代码实现 + 端到端测试（A/B/D/E/C 全绿）
   + README/CHANGELOG 文档 + 官网重新生成 + git 提交 + 含 `.git` 目录的发布压缩包。
6. **补丁位（第三位）的唯一适用场景：修复发布**。
   当一个已发布的版本存在**必须修复的缺陷**（安全漏洞、构建失败、文档与实现不符），
   而这些修复**不引入任何新特性、不改变任何既有行为**时，允许发布
   `<两位小数版本>.<补丁号>`（补丁号从 `1` 起递增，如 `v0.98.1`、`v0.98.2`）。
   **补丁发布必须满足**：
   - 只含修复，不含新特性——有新特性就应当走 `+0.01` 开新档；
   - 补丁号只增不减，且同一两位小数版本内单调递增；
   - 必须在 CHANGELOG 中**单独列出条目**，写明修了什么、影响哪些版本；
   - 补丁号**不能**用来「抢」一个尚未发布的新档（例如 `v0.99` 未发布就不许写 `v0.99.1`）。

   > **为什么要有这条例外**：v0.98 之后发现了 CVE #103–#111 等必须修复的安全缺陷。
   > 若严格禁止第三位，就只能把修复混进 `v0.99`——但那会让 `v0.99` 同时包含
   > 「已发布的 v0.98 的全部内容」与「修复」，用户无法只取修复、不取新特性；
   > 更糟的是，等待新版本意味着**缺陷版本仍在用户机器上留存更久**。
   > 补丁位让「有缺陷的版本」能被精确地、更快地替换掉。
   > 这正是语义化版本的「修订号」要解决的问题，此处按本项目的 `v0.x` 风格落地。

---

## 24. 文件 IO（file 句柄）

文件 IO 在 v0.61 落地。设计严守两条底线：**绝不引入可变全局状态**（v1.04 §37.1 统一措辞，原写作「绝不引入全局变量」）、**不重写文件逻辑（只封装 C 标准库）**。
文件句柄是显式传递的不透明指针（`file` 类型，IR 中为 `ptr`），封装 C 的 `FILE*`，**不受 ARC 管理**——
用户必须自己 `open` 后 `close`，这与[内存宪法 §19](#19-内存宪法防-gc-铁律)的「显式指针回收」契约一致。

### 24.1 API

| 函数 | 签名 | 返回 | 说明 |
| --- | --- | --- | --- |
| `open` | `open(path: string, mode: string): file` | `file` | 打开文件；底层 `fopen(path, mode)`。**失败（打不开）→ 抛出 `Error`** |
| `read` | `read(f: file, n: int): string` | `string` | 从 `f` 读取**最多 `n` 字节**，返回字符串。**失败 → 抛出 `Error`** |
| `write` | `write(f: file, content: string): int` | `int` | 把 `content` 写入 `f`，返回**实际写入字节数**。**失败 → 抛出 `Error`** |
| `close` | `close(f: file): void` | `void` | 关闭文件（`fclose`）；`f` 为空指针时安全无操作。**v0.81 起也可用于 `channel`**（关闭广播，见 [§27.7](#277-通道关闭与-eofv081)），二者按实参静态类型分派 |

`open` 的 `mode` 直接透传给 C 的 `fopen`，即 `"r"` / `"w"` / `"a"` / `"rb"` 等标准模式。

### 24.2 错误处理（v0.7 起）：失败即抛出 `Error`

> **v0.7 变更**：文件 IO 由「返回错误标志」改为「**失败即抛出 `Error`**」。
> 这是[§26.3 失败与业务分离](#263-失败与业务分离两码事)的直接落地——
> 文件不存在属于**业务失败**，不该让程序崩溃，也不该逼用户去判空指针。

- `open` 打不开文件 → 抛出 `Error`（消息形如「无法打开文件（路径不存在或无访问权限）」）。
- `read` 失败（句柄无效 / 内存不足）→ 抛出 `Error`。
- `write` 失败（句柄无效 / 写入中断）→ 抛出 `Error`。
- `close` 从不失败（空句柄上安全无操作）。

未被 `try` 包裹时，抛出的 `Error` 属于**未捕获异常**——程序打印诊断并终止（§26.5），
**绝不静默继续**。要恢复，用 `try { } catch e { }` 包住：

```dial
try {
    let f: file = open("no_such_file.txt", "r");
    close(f);
} catch e {
    print("caught: ".concat(e.msg()));
}
```

> 底层仍是「C Runtime 返回错误标志（NULL / -1）+ Dial 层转成 `throw`」的两段式：
> 判据在 C 侧，抛出在 Dial 侧，不含任何 LLVM 运行时依赖，也不破坏
> [代码编写纪律 §21](#21-代码编写纪律防臃肿与允许冗余原则)对依赖与抽象的限制。

### 24.3 用后即焚：必须 `close`

`file` 句柄**不受 ARC 管理**，编译器不会在作用域结束时自动关闭或释放它。忘记 `close` 会造成文件描述符泄漏
（与 C 的 `FILE*` 完全一致——这是显式指针回收的代价，也是它确定性的来源）。示例见 `examples/file_demo.dial`。

### 24.4 完整示例

> `open` / `read` / `write` **失败即抛 `Error`**（[§24.2](#242-错误处理v07-起失败即抛出-error)），
> 因此**整段 IO 必须被 `try` 包住**——不包就是未捕获异常，程序会打印诊断并终止（§26.5）。

```dial
fn main(): int {
    let path: string = "/tmp/dialc_io_demo.txt";

    try {
        var wf: file = open(path, "w");
        write(wf, "dialc-file-io");
        close(wf);                 // 写完务必关闭

        var rf: file = open(path, "r");
        var content: string = read(rf, 100);
        print(content);           // dialc-file-io
        close(rf);
    } catch e {
        print("IO 失败: ".concat(e.msg()));
        return 1;
    }
    return 0;
}
```

`file` 句柄不能 `print` 直接打印（会编译期报错），请先 `read` 成 `string` 再打印。

---

## 25. 包分发与依赖管理（生态规划 · 最终规范）

> **状态：规范已定稿；v0.93 起核心链路已实现。**
> 本章是 Dial 生态**最终规范**的决策摘要。**完整规范见
> [ARCHITECTURE.md《Dial 标准库与包分发最终规范》](ARCHITECTURE.md)**——
> 那里有完整的平台矩阵、包内目录树、JSON 映射字典格式、安装流程、
> 与旧草案的逐条修订记录。
>
> **v0.93 已落地**：`import "包名";` 语句、`.dialpkg` 解包（含路径穿越防护）、
> `manifest.txt` 解析、按 Target Triple 选库、**SHA256 强制校验**、
> `.diali` 签名解析接入 Sema、链接期自动链入对应平台的机器码。
> 见 [§25.9](#259-用法import-与-dialc-installv093-已实现v096-支持-url)、[§25.10](#2510-官方标准库-stdlibdialpkg独立发布)。
> 截至 v0.96 + 官方标准库 v1.0.0，`dialc install <URL>`、一键安装脚本、`stdlib.dialpkg` 化均已落地；
> 仅**纯 MSVC 线程后端**仍在规划（MinGW 路线下非阻塞）。
>
> 之所以现在就把规范写死：让「不透明指针 + C ABI」这条底层设计（[§14.1](#141-不透明指针契约)、[§15](#15-ffi-契约dial-语法--c-函数)）
> 从第一天起就为包分发服务，而不是等生态长歪了再回头打补丁。规范先于实现，是防止依赖地狱的唯一低成本时机。

Dial 的包分发只解决一件事：**把别人写好的机器码 + 函数签名，安全、确定地接进你的程序**。

它**不是**一个包管理器：不做版本求解、不做依赖树仲裁、不做语义化版本区间匹配。
它是一套**极简、防呆、零歧义**的分发格式——把复杂度的根部直接砍断（见 [§25.3](#253-绝对零版本冲突底层兼容)）。

### 25.0 目标平台

| 目标 | 状态 |
| --- | --- |
| **Windows**（x64 / ARM64） | ✅ 目标平台 |
| **Linux**（x86_64 / ARM64，glibc 与 musl） | ✅ 目标平台 |
| **Termux**（Android ARM64） | ✅ 目标平台 |
| macOS / Darwin / Apple | ❌ **不在目标范围内**，不为其做设计妥协 |

平台只有三个变体集合，这个事实直接决定了下一节的决策：**既然平台数这么少，
把它们的机器码全量打进同一个包，体积代价（几十 MB）完全可以接受。**

### 25.1 专属格式与自动解压（防呆设计）

| 决策 | 规定 |
| --- | --- |
| 容器格式 | **拒绝**直接用 `.zip` / `.tar.gz` 裸分发 |
| 专属扩展名 | **`.dialpkg`**（Dial Package） |
| 用户是否手动解压 | **永远不需要** |
| 解压位置 | 本地缓存目录 `~/.dial/packages/<包名>/`，**绝不污染当前项目目录** |
| 分发内容 | **不分发源码**——标准库与第三方库统一分发**预编译好的机器码** |
| 触发时机 | 源码中出现 `import` 时，`dialc` 自动识别并**静默**解压；或由 `dialc install <URL>` 显式安装 |

要点：

- **必须有专属扩展名。** 不用 `.dpkg`——它会与 Debian 的 `dpkg` 撞名，造成工具链与人的双重误判；
  `.dialpkg` 全局唯一，双击/命令行都不会被当成别的东西。
- **用户永远不手动解压。** 用户把包下载到本地，在代码里 `import`，`dialc` 自动识别 `.dialpkg` 格式、
  解压到全局缓存 `~/.dial/packages/`。**项目目录里不会出现任何解压产物**——
  既不污染版本控制，也不会让「第三方代码」混进用户的源码树。
- **静默但有痕。** 解压是静默的（不打满屏日志），但缓存目录是真实可读的目录，用户可以 `ls` 查看、
  也可以整目录删除后让编译器重新解压——缓存永远是**可重建的派生数据**，不是权威来源。

> 与[代码编写纪律 §21](#21-代码编写纪律防臃肿与允许冗余原则)一致：不引入「包管理器框架」这种抽象，
> `dialc` 自己认识自己的格式，一步到位。

### 25.2 包内结构与接口签名（供 LSP 与 Sema 使用）

解压后的包**必须**包含以下三个核心部分，另可**选带**一个 `src/` 目录（v1.10 增补，
见 §25.11）：

| 部件 | 作用 | 是否含实现代码 |
| --- | --- | --- |
| `lib/` **目录** | **全平台**预编译静态库（`linux-x86_64.a` / `termux-aarch64.a` / `windows-x86_64.lib` …），平铺存放，**不限制包体积** | 是（二进制） |
| `xxx.diali` | **唯一的**接口签名文件：`extern fn` 签名、类型定义、注释。**平台无关**，供 Sema 与 LSP 使用 | **否**（纯文本，只有声明） |
| `manifest.txt` | **映射字典**：Target Triple → 机器码文件名；另含包名、ABI 版本号、各文件的 sha256；有 `src/` 时还含模块名 → 源码路径的 `src` 映射 | 否（纯文本 / JSON） |
| `src/` **目录**（可选） | 随包分发的 **Dial 源码模块**（`.dial`）。由 `dial.toml` 的 `[src]` 段显式声明，装包后可直接裸名 `import`，无需复制到用户项目 | 是（Dial 源码，由 dialc 前端合并编译） |

```
sqlite.dialpkg
├── manifest.txt          # 映射字典：Target Triple -> 机器码文件
├── sqlite.diali          # 唯一的接口声明（平台无关）
├── src/                  # 可选：随包分发的 Dial 源码模块（见 §25.11）
│   └── sqlite_oo.dial    #   [src] 段显式声明；装包后 import "sqlite_oo"; 即用
└── lib/                  # 全平台预编译静态库
    ├── linux-x86_64.a
    ├── linux-x86_64-musl.a
    ├── linux-aarch64.a
    ├── termux-aarch64.a
    ├── windows-x86_64.lib
    └── windows-arm64.lib
```

**三件套的职责切分**：`manifest.txt` 回答「有哪些平台、机器码在哪、可信吗」；
`xxx.diali` 回答「这些函数长什么样」；`lib/` 负责「给我当前平台的那份机器码」。
可选的 `src/` 是第四件套——它只放 **Dial 源码模块**，回答「包作者还想随包附上哪些
用 Dial 写好的惯用 API / 辅助函数」；这些文件**不产生新的机器码**，由 dialc 前端
按本地源码模块的方式合并编译（详见 §25.11）。

> **为什么全量打包而不是一个平台一个包**：机器码是**编译产物**，不是源码——
> 它不会被人阅读、不会被手工编辑、不会产生「版本分叉」的困扰；而几十 MB 的下载成本
> 在有 CDN 的前提下可以忽略。既然如此，把「选哪个平台的机器码」这件事从**用户侧**
> 移到**编译器侧**：用户只管写代码，一个包走天下。详见
> [ARCHITECTURE.md §2](ARCHITECTURE.md#2-多平台机器码揉捏与字典映射)。

#### `xxx.diali` —— 签名即契约

`.diali` 是**纯文本**的接口签名文件，只含声明，**不含任何实现代码**。
`dialc` 与 LSP **只读取这个文件**来完成：类型检查（Sema）、代码补全、悬停提示（hover）。

> **⚠️ 以下语法只属于 `.diali`，不是 Dial 编程语言本身。** 本文件的 `type` / `extern fn` / `///`
> 由包解析器 `src/package.rs` 单独处理，**不会**出现在 `.dial` 源码中，也**不在** §4.3 关键字表与 §9 函数定义里。
> 语言层的关键字与函数定义语法分别见 [§4.3](#43-关键字) 与 [§9.1](#91-定义)；`.diali` 能写的声明只有两种（见下方示例与 [§25.9](#259-用法import-与-dialc-installv093-已实现v096-支持-url)）：
> `type X;`（不透明句柄）与 `extern fn f(a: T, ...): R;`（外部函数签名）。

```dial
// sqlite.diali —— 接口签名文件（纯文本，无实现）
// dialc 用它做类型检查；LSP 用它做补全与悬停提示。

/// 不透明句柄：外部代码只拿到指针，永远不知道内部布局（见 §14.1）
type Db;

/// 打开数据库；失败返回空句柄。
/// 句柄不受 ARC 管理，必须显式 db_close（§19.3 显式指针回收契约）。
extern fn db_open(path: string): Db;

/// 执行 SQL，返回受影响行数
extern fn db_exec(db: Db, sql: string): int;

/// 关闭数据库；空句柄上安全无操作
extern fn db_close(db: Db): void;
```

由此得到两个直接收益：

- **Sema 不需要读二进制。** 类型检查只依赖 `.diali`，不必解析 `.a` 里的符号表——
  交叉编译、跨架构检查因此成为可能（`.diali` 与架构无关）。
- **LSP 与编译器共用同一份真相。** 补全/悬停与编译期检查读的是同一个文件，
  杜绝「编辑器说能过、编译器说不能过」的经典分裂。文档注释（`///`）就是悬停提示的来源。

#### `manifest.txt` —— 映射字典与元信息

采用 **JSON**（映射字典的本质是「键 → 值」的嵌套结构，JSON 表达力足够，
且几乎所有语言的 JSON 解析都是三行代码，不引入新依赖）：

```json
{
  "name": "sqlite",
  "version": "1.4.0",
  "abi": 1,
  "iface": "sqlite.diali",

  "targets": {
    "x86_64-linux-gnu":       { "lib": "lib/linux-x86_64.a" },
    "x86_64-linux-musl":       { "lib": "lib/linux-x86_64-musl.a" },
    "aarch64-linux-gnu":       { "lib": "lib/linux-aarch64.a" },
    "aarch64-linux-android":   { "lib": "lib/termux-aarch64.a" },
    "x86_64-pc-windows-msvc":  { "lib": "lib/windows-x86_64.lib" },
    "aarch64-pc-windows-msvc": { "lib": "lib/windows-arm64.lib" }
  },

  "sha256": {
    "sqlite.diali":            "3a7e04...（省略）...d8",
    "lib/linux-x86_64.a":      "9f2b7c...（省略）...c1",
    "lib/termux-aarch64.a":    "a20f91...（省略）...19",
    "lib/windows-x86_64.lib":  "7d3ab8...（省略）...45"
  }
}
```

| 字段 | 含义 |
| --- | --- |
| `name` | 包名（同时决定缓存子目录名） |
| `version` | 包作者的版本号（**仅用于展示与人工核对，编译器不做版本求解**） |
| `abi` | **ABI 版本号**；这才是编译器唯一关心的兼容性判据（见 §25.3） |
| `iface` | 唯一的接口声明文件名 |
| `targets` | **映射字典**：Target Triple → 该平台的机器码文件。**Target Triple 是唯一寻址键**——「架构」不足以定位一份机器码（同为 x86_64，glibc / musl / MSVC / MinGW 四者 ABI 互不兼容） |
| `sha256` | **每个文件各自**的哈希：`.diali` 一份，`lib/` 下每个平台机器码各一份。**选中哪个平台就校验哪个平台的**；其余用于整包分发完整性 |

### 25.3 绝对零版本冲突（底层兼容）

这是整套规范的核心赌注，也是它敢不做版本求解的底气：

> 因为 Dial 的底层**严格采用不透明指针（Opaque Handle）**，对外只暴露**标准 C ABI 函数**
> （[§14.1](#141-不透明指针契约)、[§15](#15-ffi-契约dial-语法--c-函数)），
> 外部代码只是**把句柄传给底层函数**来操作，完全不知道底层内存布局。

由此推出：

| 结论 | 说明 |
| --- | --- |
| **内部结构随便改** | 库作者可以任意重构内部数据结构、增删字段、换算法——只要函数签名与语义不变 |
| **旧代码零改动** | 调用方只需**链接新的 `.a` 文件**即可完美运行，无需重新编译、无需改一行源码 |
| **依赖地狱被消灭** | 不存在「A 要 v1、B 要 v2 所以装不上」的局面：同一进程里只有一个 ABI，一个包只有一个版本 |
| **判据是 `abi` 字段** | 只要 `manifest.txt` 里的 **ABI 版本号不变**，就承诺二进制兼容；ABI 变更必须递增该号，编译器据此拒绝 |

- **一个包 = 一个 ABI = 无版本矩阵。** 规范里 `version` 只用于人类阅读与人工核对；
  编译器的兼容性判据**只有 `abi`**。这从根上取消了「版本区间求解」这个 NP 难题的存在理由。
- **ABI 变更是显式事件。** 库作者改坏了兼容性，就必须递增 `abi`；`dialc` 加载时若发现
  调用方期望的 ABI 与包声明不符，**当场报错**，而不是让不匹配的二进制悄悄链进产物。
- **跨平台也是同一个版本号。** `.diali` 只有一份、ABI 号只有一个，
  所以「Windows 版 1.4.0」与「Linux 版 1.4.0」在兼容性判据上是**同一个 1.4.0**，
  不需要任何 per-platform 的版本矩阵。

> 这不是「尽力兼容」，而是**由类型系统强制的不可能冲突**：
> 你拿不到布局，就无法依赖布局；无法依赖布局，内部结构怎么改都不会破坏你。

### 25.4 SHA256 强制校验（防断网与防篡改）

所有 `.dialpkg` **必须自带 SHA256 哈希校验**，由 `dialc` 在**解压前**强制验证整包；
解压之后、进入链接器**之前**，还要对**本次实际要用的那个平台的机器码**再校验一次。

| 场景 | 后果 |
| --- | --- |
| 下载时网络中断 → 文件残缺 | **当场拦截并报错**，绝不让残缺二进制进入链接器 |
| 文件被恶意篡改（植入后门） | **当场拦截并报错** |
| 哈希比对通过 | 正常解压、链接 |

铁律：

- **校验先于一切。** 残缺或被篡改的二进制**绝不允许**流到链接阶段——否则就会产生
  「链接通过、运行随机崩」的幽灵 Bug：它不报编译错、不报链接错，只在客户机器上偶发，
  极难复现，最终全部变成无效的客服纠纷。
- **每个文件都要校验。** `.diali`（签名）与各平台机器码**各自**都有独立哈希。
  只校验其中一个是不够的：`.diali` 被篡改同样致命——它会让「类型检查通过」建立在谎言之上
  （签名说返回 `int`，真实库返回指针）。
- **两层校验分工明确**：整包哈希回答「这个包完整吗、没被改过吗」；
  单文件哈希回答「我要用的这一份机器码可信吗」。两者不可互相替代。
- **失败即终止，不做降级。** 与 [Fail-Fast 铁律 §20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)同调：
  宁可让编译当场失败并给出明确原因，也绝不带病产出可执行文件。

### 25.5 分发渠道：拒绝中心化服务器

| 决策 | 规定 |
| --- | --- |
| **中心化服务器** | **拒绝**。不设官方包仓库、不做账号体系、不做中央索引、不做自建 CDN |
| **分发渠道** | **所有包（包括官方标准库）均通过 GitHub Releases 分发** |
| **成本** | 零运维成本，**利用 GitHub 的免费 CDN** |

这条决策与 Dial 的整体哲学一致：

- **不设账号 = 没有「谁可以发布这个包」的问题**，也就不需要审核、签名体系、信任根。
- **不用中央服务器 = 没有单点故障、没有带宽成本、没有「服务关停后生态归零」的风险。**
- **包的信任由哈希而非服务器担保**：GitHub Releases 只负责「把字节送到你面前」，
  「这些字节是不是我要的那个」由 SHA256 独立回答。**分发渠道与信任判定解耦**——
  换任何一个 CDN 或镜像，都不影响安全性。

### 25.6 一键安装与无缝链接

**一键安装**（Linux / Termux：`install-dial.sh`；Windows：`install-dial.ps1`）：

```
用户把 sqlite.dialpkg 丢进项目目录
            │
            ▼
   运行一键脚本
            │
            ├── 初始化 Dial 环境：建 ~/.dial/、挂 PATH、写配置
            │
            └── 重新扫描 → 找到 sqlite.dialpkg → 执行安装命令
                              │
                              ▼
              dialc install sqlite.dialpkg
                              │
                              ▼
              SHA256 校验 → 解压到 ~/.dial/packages/sqlite/
```

**无缝链接**（`dialc build` 自动完成，用户零配置）：

```
dialc build main.dial
        │
        ├─ ① 扫描包来源：~/.dial/packages/*/ 与项目内 *.dialpkg
        ├─ ② 读源码里的 import，确定被引用的包
        ├─ ③ 读 manifest.txt
        │      ├─ 按【当前编译 Target Triple】查 targets 映射字典
        │      │     └─ 查不到 → 当场报错（并列出本包支持哪些 triple）
        │      ├─ 校验 .diali 与该平台机器码的 SHA256
        │      └─ 读 .diali → 喂给 Sema 做类型检查（供 LSP 补全 / 悬停）
        ├─ ④ 链接阶段：按 triple 自动选中机器码加入链接命令
        │      ├─ 编译到 Windows     → 自动链接包内的 .lib
        │      └─ 编译到 Linux/Termux → 自动链接包内的 .a
        └─ ⑤ 产出可执行文件（仍只依赖 libc / 平台运行库）
```

**「无缝」的含义**：用户不写依赖列表、不写路径、不写平台；包未解压就自动解压，
哈希不符就自动拦截，目标缺失就明确报错而不是静默降级。
**接口契约由 `.diali` 单独统一提供，与底层是哪个平台的机器码完全解耦。**

#### 25.6.1 三条硬规则（已落地）

**① 强制静态库——拒绝一切动态库。**
`targets` 映射的库文件，后缀**必须为 `.a`（Linux/Termux）或 `.lib`（Windows）**；
形如 `.so` / `.dll` 的动态库一律在安装解析 `manifest.txt` 时**当场拒绝**。
理由：Dial 产物的依赖契约是「**只链 libc，绝不引入运行时动态依赖**」（见 [§3.4 产物依赖契约](#34-产物依赖契约)）。
一旦允许 `.so` / `.dll` 混进来，用户程序就会多出一份运行期必须随附的动态依赖，
既违背「零运行时依赖」的承诺，也会让 `ldd` 契约检查（测试 C 组）失效。

> **与语言层 `link` 的边界**：本节只约束**包分发**（别人打好的 `.dialpkg`）。
> 源码作者在 `.dial` 里自己写 `link "xxx.so";` 属于显式选择，不受此限，
> 代价是产物运行期必须随附那份动态库——见 §33.8。J 组的「拒绝动态库」断言
> 打在 `dialc install` 路径上，与语言层是两条互不相干的链路。

**② 重复安装——ABI 相同覆盖，ABI 不同报错。**
缓存目录 `~/.dial/packages/<name>/` 下已存在同名包时，`dialc install` 会先读出旧 `manifest.txt` 比对 `abi`：

| 情形 | 行为 |
| --- | --- |
| **ABI 相同** | 直接**覆盖**——整目录删除后重新解压，避免残留旧平台的机器码或陈旧的 `.diali` |
| **ABI 不同** | **报错并拒绝**，提示先卸载旧包再装（见下），防止不兼容的二进制悄悄链进用户程序 |

> 「为什么不让不同 ABI 共存？」因为同一包名在 `import` 侧只凭名字引用，
> 若允许同名不同 ABI 的包同时躺在缓存里，编译器无法判断该链哪一份——版本冲突的口子就开了。
> Dial 选择「同名即唯一」，把冲突暴露成「先卸载再装」的显式动作，而非静默选错。

**③ 卸载——直接删目录即可。**
Dial 目前**没有** `dialc uninstall` 命令，因为缓存是**纯派生数据**：包的所有内容都来自
`.dialpkg`，删了随时能重装。要卸载一个包，直接在终端删除它的缓存目录即可：

```bash
rm -rf ~/.dial/packages/<包名>/
```

> 注意这条与 ② 的报错提示是自洽的：ABI 冲突时，运行上面这行删掉旧包，再 `dialc install`
> 新包即可，无需任何额外工具。

### 25.7 这套规范刻意不做的事

把边界写清楚，和规范本身同等重要：

| 不做 | 原因 |
| --- | --- |
| 版本区间求解 / 语义化版本匹配 | ABI 不变即兼容，冲突不存在，求解无对象（§25.3） |
| 传递依赖自动解析 | 包内不含依赖声明；依赖由使用者显式 `import`，保持可见与可控 |
| 中央仓库 / 账号体系 / 官方索引服务 | §25.5：全部走 GitHub Releases，零中心化 |
| **C 源码分发 / 用外部工具链就地编译 C 代码** | 只分发**机器码 + 签名**（外加可选的 Dial 源码模块，见 §25.11），用户机器无需装对方的 C 工具链。`src/` 里的 `.dial` 不是 C 源码：它由 dialc **自己的前端**合并编译，不调用任何外部编译器、不执行构建脚本，包仍是纯数据 |
| 包内执行构建脚本 | 无脚本、无钩子，包是**纯数据**，解压即可用，无可执行的副作用面 |
| 覆盖/污染用户项目目录 | 一律解压到 `~/.dial/packages/`，项目目录零侵入（§25.1） |
| 二进制差分 / 增量更新 | 一个包几十 MB，一次下载即可；增量更新的复杂度远超收益 |
| 自建 CDN / 镜像站 | §25.5：直接用 GitHub 的免费 CDN |

### 25.8 许可隔离：第三方包同样绝不以 GPL 链接进用户程序

包分发的机器码（`lib/` 下的 `.a` / `.lib`）会被**静态链入**用户的可执行文件，
因此**每一个包**都与 [§3.4.1](#341-许可隔离标准库绝不以-gpl-链接进用户程序防法务传染) 的标准库
面临完全相同的法务传染问题，并且风险更大——包的作者不是 Dial 自己，用户无从替他做担保。

> **包内的机器码与 `.diali` 接口签名，其许可由包作者自行决定；
> 但 Dial 官方标准库（`libdial_std.a`）必须额外豁免，并强制锁定为 `MIT` 或 `Apache-2.0`，
> 绝对不允许以 GPL 协议的条款链接进用户的闭源程序，避免造成法务传染。**

| 组件 | 许可要求 | 说明 |
| --- | --- | --- |
| **Dial 官方标准库**（`libdial_std.a`） | **强制 `MIT` 或 `Apache-2.0`** | 官方运行时被链进**每一个** Dial 程序，必须免传染 |
| **第三方包**（`~/.dial/packages/*/lib/*.a`） | 由包作者声明，**`dialc` 不做审查** | 但 `manifest.txt` **必须**含 `license` 字段，`dialc` 在链接时把它打印到构建日志 |

因此 `manifest.txt` 增加一项**必填**字段：

```json
{
  "name": "sqlite",
  "abi": 1,
  "license": "MIT OR Apache-2.0",
  "...": "..."
}
```

| 字段 | 说明 |
| --- | --- |
| `license` | **必填**。SPDX 许可表达式。`dialc` **不校验**其相容性（那是法务判断，不是编译器职责），但**必须存在且非空**——缺失即拒绝加载该包 |

> **编译器的职责边界**：`dialc` 负责让许可**可见**（强制声明 + 构建日志回显），
> 但**不替用户做法务判断**。是否接受某个包的条款，由使用者自己决定。
> 唯一被**强制**的，是 Dial 自己的标准库：它必须是 MIT 或 Apache-2.0，绝不可以是 GPL。

### 25.9 用法：`import` 与 `dialc install`（v0.93 已实现，v0.96 支持 URL）

**第一步：安装一个包**——本地 `.dialpkg` 文件，或 **URL**（v0.96 起）

```bash
$ dialc install sqlite.dialpkg
✅ 已安装包 'sqlite' v1.4.0（ABI 1）
   许可: MIT OR Apache-2.0
   位置: /home/you/.dial/packages/sqlite
   支持平台: x86_64-linux-gnu, aarch64-linux-gnu, x86_64-pc-windows-msvc, ...
```

包**永远**解压到 `~/.dial/packages/<name>/`，项目目录零侵入。
缓存是可重建的派生数据，整个删掉后编译器会重新解压。

#### 从 URL 安装（v0.96）

包也可以直接给 URL——**不用先手动下载**：

```bash
$ dialc install https://github.com/某人/sqlite/releases/download/v1.4.0/sqlite.dialpkg
```

**只允许 `https://`**（本机 `localhost` / `127.0.0.1` 例外，可用于本地镜像与自测）：
明文 http 能被中间人改写，而 SHA256 挡不住「换了内容还带着正确哈希」的伪造，
因此非本机地址一律拒绝——**宁可装不上，也不降级到明文**。

**下载 → 校验 → 解压**三步，一步都不省（[ARCHITECTURE.md §3.1 / §5.1](ARCHITECTURE.md#3-一键安装与本地缓存)）：

| 步骤 | 做什么 | 失败时 |
| --- | --- | --- |
| 1. 下载 | 用 `curl` 取到临时文件（临时文件**成功失败都删**，不留残骸） | 立即报错，缓存目录无残留 |
| 2. 整包哈希（可选） | 给了 `--sha256` 就比对**整个下载物**的哈希——这能证明「这就是我要的那一份包」 | **解压之前**就失败 |
| 3. 逐文件校验 | 按包内 `manifest.txt` 登记的 SHA256，把包里**每个文件**验一遍——**一个字节都还没落到磁盘** | **解压之前**就失败 |

第 3 步是 [§5.1「校验先于一切」](#254-sha256-强制校验防断网与防篡改) 的字面实现：
残缺或被篡改的机器码**绝不允许**进入缓存、更绝不允许进入链接器。

```bash
$ dialc install https://.../sqlite.dialpkg --sha256 3f786850e387550fdab836ed7e6dc881de23001b
```

**没给 `--sha256` 也能装**，但会明确提示你这次只验了「包内文件与 manifest 是否一致」
（能发现残缺与篡改），**没验整包来源**——不静默降级。想要强保证就把发行页给出的哈希传进来。

> 下载用外部 `curl` 而非引入 Rust 的 HTTP 库：编译器**本来就依赖外部工具链**
> （`zig cc` / `clang` / `ar`），下载器属同一类依赖，不新增「运行期还要装个东西」的负担；
> 引入 TLS 栈会显著拉长编译时间与依赖树（与 [§21](#21-代码编写纪律防臃肿与允许冗余原则) 相悖）。
> **TLS 绝不手写**——证书校验、主机名校验、重定向全部交给 curl 背后的成熟实现。

**第二步：源码里 `import`，然后照常编译**

```dial
import "sqlite";                 // 包名是字符串字面量

fn main(): int {
    var db: Db = db_open("app.db");   // Db 是包声明的**不透明句柄**
    let n: int = db_exec(db, "delete from t");
    db_close(db);                     // void 调用：只作语句，不能当值用
    print(n);
    return 0;
}
```

```bash
$ dialc build main.dial -o main
ℹ️  包 'sqlite' v1.4.0（ABI 1，许可 MIT OR Apache-2.0）
ℹ️  链接 sqlite -> /home/you/.dial/packages/sqlite/lib/linux-x86_64.a（目标 x86_64-linux-gnu）
✅ 已生成可执行文件: main
```

**用户不写路径、不写平台、不写版本**——编译器按自己的 Target Triple 索引，
交叉编译到 Windows 时自动改为链包内的 `.lib`。

**`.diali` 里能写什么**（v0.93 支持两种声明，其余一律报错）：

| 声明 | 含义 | Dial 侧的类型 |
| --- | --- | --- |
| `type Db;` | 不透明句柄 | `Type::Opaque("Db")`——IR 为 `ptr`，**不受 ARC 管理**，生命周期由包自己的 open/close 掌管 |
| `extern fn f(a: T, ...): R;` | 外部函数签名 | 按签名类型检查；调用与本地函数无差别 |

参数与返回类型可用：`int` / `float` / `bool` / `char` / `string` / `void` / `map` / `file` /
`error` / `thread` / `channel<T>` / `T[]`，以及本包声明的不透明类型名。
**未声明又非内建的类型名一律报错**——让它悄悄退化成某个默认类型，
等于把「签名不匹配」这个能在编译期抓住的错误推迟成运行期的内存损坏。

**安全保证（已实现并纳入自测）**：

| 场景 | 行为 |
| --- | --- |
| 机器码被篡改 / 下载残缺 | **SHA256 校验失败，当场拦截**，绝不进入链接器 |
| `.diali` 被篡改 | 同上——被篡改的签名会让「类型检查通过」建立在谎言之上 |
| 包内路径含 `../` 或绝对路径 | **拒绝解压**（路径穿越防护） |
| 当前平台不在 `targets` 里 | 报错并**列出本包支持哪些 triple** |
| 导入的函数与本程序函数重名 | 编译期报错（避免「到底链了哪一个」的悬案） |

> 自测覆盖：`tests/run_tests.sh` 的 **H 组**会用 `tools/make_sample_pkg.sh`
> 现场构建一个示例包、安装、`import` 后编译运行并比对输出，
> 再**故意篡改一个字节**验证 SHA256 确实拦得住。

### 25.10 官方标准库 `stdlib.dialpkg`（独立发布）

> **发布方式已调整（本节标题沿用早期架构名）**：`stdlib.dialpkg` 不再单独开 `stdlib-v*`
> 页面，而是与三平台 zip、`.vsix` 一样挂在**同一个 `v*` 编译器 Release** 上；每次发版都
> 重新组装一份（哪怕 stdlib 源码一行未改），因此 `releases/latest/download/stdlib.dialpkg`
> 与其他资产走同一个重定向入口，用户在 Release 页面里也能直接看到、直接拿到。

官方标准库（`stdlib/` + `runtime/` 编译归档而成的 `libdial_std.a`，被**每一个** Dial 程序静态链入）
以 `.dialpkg` 形态分发：

| 项 | 值 |
| --- | --- |
| **所在 Release** | 每个 `v*` 编译器 Release（资产名固定 `stdlib.dialpkg`） |
| **`abi`** | `4`（编译器 `package::STDLIB_ABI` 同步锁定；每次破坏性变更递增，见 [§31.5.1](#3151-标准库-abi-递增破坏性变更) 与 [§32.5.1](#3251-标准库-abi-递增破坏性变更)） |
| **`version`** | `1.3.0` |
| **`license`** | `MIT OR Apache-2.0`（强制免传染，见 §25.8） |
| **包名** | `stdlib`（`dialc` 按此名自动加载，用户无感知） |
| **三平台机器码** | `lib/x86_64-linux-gnu/libdial_std.a` · `lib/x86_64-pc-windows-gnu/libdial_std.a` · `lib/aarch64-unknown-linux-musl/libdial_std.a` |

**预编译安全等级：官方标准库默认以 `ReleaseSafe`（`-DDIAL_SAFETY=1`）级别预编译，兼顾极致性能与绝对安全。**
即越界/空指针等 UB 在 C Runtime 层被 `ud2` Fail-Fast 拦下，既不丢性能也不静默损坏内存——
这与用户程序自身的 `--release` / `--fast` 安全等级是**两件事**（标准库是预编译产物，等级在发布时已钉死）。

**全自动、零配置**：

1. **一键脚本自动拉取**：`install-dial.sh` / `install-dial.ps1` 把 `dialc` 装好后，会**静默**从
   `https://github.com/shichencike/Dial/releases/latest/download/stdlib.dialpkg`
   拉取并安装（默认跟随最新，版本不写死；`STDLIB_RELEASE` 可钉旧 `stdlib-v*` 通道，
   `STDLIB_URL` 可完全覆盖地址，离线/失败仅告警不阻断）。
2. **`dialc build` 优先链入预编译包**：链接期先找已安装的 `stdlib` 包，按本机 Target Triple 取对应 `.a`；
   日志会打印「使用已安装的标准库包: stdlib v1.3.0（ABI 4，目标 …）」。
3. **缺失回退源码编译**：若未安装该包（离线/脚本跳过），`dialc` 退化为实时编译 `stdlib/` + `runtime/`
   的 C 源码（默认 Debug 安全等级），保证**任何环境下都能编译出程序**，不会因拿不到包而卡死。
4. **ABI 不兼容明确报错**：若已安装包的 `manifest.abi ≠ STDLIB_ABI`，`dialc` **当场报错并给出重装方案**
   （`rm -rf ~/.dial/packages/stdlib` 后重新 `dialc install`），**绝不静默崩溃或链错二进制**。

**CI 自动构建与发布**：`.github/workflows/ci.yml` 的三个 build 作业各自产出本平台 `libdial_std.a`；
`v*` 标签触发的 `release` 作业用 `tools/make_stdlib_pkg.py --abi 4` 组装 `stdlib.dialpkg`，
与 zip / .vsix 一起上传到**同一个 Release**。
构建脚本 `tools/build_stdlib.sh <输出 .a> [安全等级]` 统一选 `zig cc`/`clang`/`gcc` 与 `ar`，
把 `runtime/*.c` + `stdlib/*.c` 编译归档为单份静态库。

> 旧的 `stdlib-v*` 独立通道（`publish-stdlib` 作业）**仅作向后兼容保留**，并标记为预发布，
> 以免它抢走 `releases/latest` 的指向；历史 `stdlib-v1.0.0` Release 及其链接仍然可用。
> 破坏性 ABI 变更时同步递增 `STDLIB_ABI` 与包 `abi`，旧用户会收到明确的「ABI 不兼容」报错与重装指引。

### 25.11 包内 Dial 源码模块（`src/`，v1.10 增补）

> `.dialpkg` 从「机器码 + 签名」扩展为「机器码 + 签名 + **随包 Dial 源码模块**」：
> 包作者可以把用 Dial 写好的惯用 API / 辅助函数放进包内 `src/` 目录，由
> `dial.toml` 的 `[src]` 段**显式声明**；用户装包后直接写裸名 `import "xxx";`
> 就能用，**不必再手动复制源码文件到自己的项目**。包仍是**纯数据**——不执行
> 构建脚本、不调用外部编译器，`src/*.dial` 由 dialc 自己的前端合并编译。

**动机**：此前包只分发 `.diali`（声明）与 `lib/*.a`（机器码），任何用 Dial 写的
上层封装都只能留在仓库里，用户装了包却拿不到，只能自己 `cp` 一份到项目目录。
这既割裂了「装包即用」，也让封装版本与包版本容易对不上。`src/` 把这类源码变成
**包的一部分**，与机器码同生共死、同受哈希保护。

**清单声明**（`dial.toml`，显式声明而非自动扫描——包作者完全掌控哪些源码随包分发）：

```toml
[src]
mylib_oo = "src/mylib_oo.dial"     # 键 = import 裸名；值 = src/ 下的 .dial
helpers  = "src/helpers.dial"
```

键是裸名（不含路径分隔符、不带 `.dial` 后缀），值必须位于 `src/` 目录下、以
`.dial` 结尾、且文件真实存在。`dialc pack` 把它们与 `.diali` / `.a` 一起收进包，
并在 `manifest.txt` 里写一份 `src` 映射（模块名 → 包内路径）：

```text
mylib.dialpkg
├── manifest.txt          # 含 "src": { "mylib_oo": "src/mylib_oo.dial" }
├── mylib.diali
├── src/
│   └── mylib_oo.dial     # 已在 sha256 字典里登记
└── lib/<triple>/libmylib.a
```

**三级 import 优先**（`main.rs::collect_sources`，见 §34.2）：

| 级别 | 命中条件 | 行为 |
| --- | --- | --- |
| ① 本地源码 | 相对路径 / `.dial` 后缀 / 同目录存在 `<name>.dial` | 展开用户项目里的文件（**最高优先**，覆盖包内副本） |
| ② 包内 `src/` 模块 | 裸名命中某已安装包 `manifest.src` 的键 | 从包缓存 `src/` 展开该 `.dial`，与 ① 同一套递归 / 去重 / AST 合并 |
| ③ 包名 | 其余裸名 | 读 `.diali` + 按 Target Triple 选 `.a`（v0.93 以来语义不变） |

② 级模块内部若写 `import "mylib";`，会照常汇总到 ③ 级，因此模块可以直接调用包里的
`extern fn`——「源码模块 + 机器码」是同一个包的两面，不需要用户写第二个 `import`。
同名模块被多个包声明时**报错**（不静默任选一份），遵守 Fail-Fast。

**与机器码同级的完整性保护**：`src/*.dial` 与其它文件一样进 `manifest.sha256` 字典，
安装期 `verify_archive` 逐个校验；此外**每次按 ② 级读取时**还会再校验一次
（`package::find_src_module` → `verify_sha256`），防止安装后源码被悄悄替换——
源码改动会直接改变程序行为，不能因为「只是文本」就放宽（§25.4「校验先于一切」）。

**向后兼容**：`manifest.txt` 的 `src` 是**可选字段**，缺失即空映射；`[src]` 段也是
可选的。旧包、旧脚本（`tools/make_stdlib_pkg.py` / `make_sample_pkg.sh`）生成的包
没有任何改动，照常 `pack` / `install` / `import`。`abi` / `version` / 既有签名一律不变。

**Fail-Fast**（全部在 `dialc pack` 打包期就地报错，不拖到安装 / 编译期）：

| 非法写法 | 拒绝原因 |
| --- | --- |
| `foo = "src/foo.txt"` | 不是 `.dial` |
| `foo = "foo.dial"` | 不在 `src/` 目录下（src 目录单独存放 Dial 源码） |
| `bad/name = "src/x.dial"` | 模块名不是裸名 |
| `foo = "src/missing.dial"` | 声明的文件不存在 |
| `[src]` 段重复 / 未知段 | TOML 子集只允许 `[targets]` / `[src]` 各一次 |

**与 §25.7 边界的关系**：那条边界禁的是「把 **C 源码**发给用户、让用户在自己机器上
用外部工具链就地编译」——那会让用户被迫装对方的编译器、引入构建脚本副作用面。
`src/` 里的 `.dial` 是**由 dialc 自己的前端**合并编译的源码模块，两者不是一回事；
包依旧是纯数据、零钩子、解压即可用。

**回归（Q 组）**：`tests/run_tests.sh` 的 Q 组第 10) 块用隔离 `HOME` 走完整链路——
`dialc new` 骨架预建空 `src/`、`[src]` 段显式声明、`pack` 产物含 `src/*.dial` +
`manifest.src` 映射 + 逐文件 SHA256、`install` 落地包缓存、**不复制任何文件**直接
`import "qsrc_oo";` 联动包函数输出 `42`、篡改包内源码被 SHA256 拦截、本地同名
`.dial` 优先于包内副本、4 条非法声明 Fail-Fast，10 条断言全绿。

---

> 本章为**生态规划**，不含任何实现承诺与时间表。**完整规范**（平台矩阵、包内目录树、
> JSON 映射字典格式、安装流程、与旧草案的逐条修订记录、实现路线）见
> **[ARCHITECTURE.md《Dial 标准库与包分发最终规范》](ARCHITECTURE.md)**。
> 规范一旦在真实包上落地，将同步更新本章、ARCHITECTURE.md 与 [CHANGELOG.md](CHANGELOG.md)；
> 在此之前，`import` 仍按 [§17.2](#172-尚未实现的能力截至-v110) 视为**不支持**。

---

## 26. 可恢复异常（try/catch/throw）

v0.7 落地。Dial 至此拥有**两套互补的错误机制**，各管一半，绝不混用：

| 类别 | 例子 | 机制 | 能否恢复 |
| --- | --- | --- | --- |
| **系统崩溃**（程序员的错） | 数组越界、除零、重复释放 | Fail-Fast：`int3` / `ud2` **当场终止**（[§20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)） | **否**（也不该恢复） |
| **业务失败**（运行环境的错） | 文件不存在、网络超时、磁盘满 | **可恢复异常**：`throw Error(...)` / `try ... catch` | **是** |

> v0.6 只有 `ud2`（不可恢复）这一半；v0.7 补上另一半，但**绝不**用它去接越界/除零——
> 那些是 bug，不是业务分支，恢复它们只会把 crash 变成更贵的幽灵 bug。

### 26.1 语法（catch 后无圆括号）

```dial
// 1) 捕获并拿到错误对象：catch e { }   —— 注意没有圆括号
try {
    let f: file = open("no_such_file.txt", "r");
    close(f);
} catch e {
    print(e.msg());          // e 的类型自动推导为内建 Error
}

// 2) 忽略具体错误：catch { }            —— 只要捕获到不崩溃即可
try {
    throw Error("boom");
} catch {
    print("ignored");
}
```

| 写法 | 含义 |
| --- | --- |
| `throw Error("msg");` | 抛出一个错误对象；**只能**抛 `Error` 类型 |
| `try { ... } catch e { ... }` | 捕获，并把错误对象绑定到 `e`（`e` 自动推导为 `error`） |
| `try { ... } catch { ... }` | 捕获但**忽略**具体错误（错误对象被立即释放，不泄漏） |

语法上的两条硬性约束（违反即编译期拒绝）：

- **`catch` 后不得写圆括号。** `catch (e) { }` 会报
  「语法错误：catch 后不得使用圆括号，直接写变量名，例如 `catch e { ... }`」。
  理由与[代码编写纪律 §21](#21-代码编写纪律防臃肿与允许冗余原则)一致：括号在 C 骨架里属于函数或条件专属，
  用在 `catch` 后面只会造成视觉污染，且对解析器毫无帮助。
- **显式 `throw Error(...)` 必须出现在 `try` 块内。** 否则报
  「throw 必须出现在 try 块内（没有捕获点，异常无法被处理）」。
  注意这条**只约束你手写的 `throw`**，不约束内建函数的隐式抛出——见 [§26.1.1](#2611-显式-throw-与内建函数的隐式抛出两条不同的规则)。

#### 26.1.1 显式 `throw` 与内建函数的隐式抛出：两条不同的规则

「必须写在 `try` 里」这条约束**只适用于你自己手写的 `throw`**。
Dial 里有**两种**产生异常的方式，它们的编译期待遇**完全不同**，必须严格区分：

| | ① **显式 throw**（你自己抛） | ② **内建函数的隐式抛出**（运行环境的错） |
| --- | --- | --- |
| 写法 | `throw Error("msg");` | `open(...)` / `read(...)` / `write(...)` 失败 |
| 不在 `try` 块内时 | **编译期报错，拒绝编译** | **编译通过** |
| 运行到该处且无捕获时 | （已编译失败，不存在此情形） | 触发**未捕获异常（panic）**，打印消息并终止进程 |
| 为什么 | 你明确写了一个异常却没有准备捕获点——这是**代码缺陷**，应在编译期挡住 | 失败取决于运行时的文件系统/网络，编译器**无从预知**，无权替你决定「这里一定要捕获」 |

**① 显式 throw —— 编译期强制必须有捕获点**

```dial
fn bad(): void {
    throw Error("boom");      // ❌ 编译错误：throw 必须出现在 try 块内
}

fn good(): void {
    try {
        throw Error("boom");  // ✅ 有捕获点
    } catch e {
        print(e.msg());
    }
}
```

**② 内建函数的隐式抛出 —— 允许写在任何地方**

```dial
fn h(): void {
    let f: file = open("maybe_missing.txt", "r");   // ✅ 编译通过（无论是否在 try 内）
    close(f);
}
```

上面 `h()` **能通过编译**。`open` 会不会失败，取决于运行那一刻文件是否存在，编译期无法判断。
一旦它真的失败、且调用链上没有任何 `try` 捕获，进程会这样终止（**实测输出**）：

```
dial: 未捕获的异常（panic）: 无法打开文件（路径不存在或无访问权限）   ← stderr
（进程以 SIGABRT 终止，退出码 134）
```

同样的失败若被 `try` 接住，则只是**一次普通的业务分支**，程序继续运行（**实测输出**）：

```
caught:
无法打开文件（路径不存在或无访问权限）
still alive
```

> 这不是「编译器漏检」，而是**刻意的语义分工**：
>
> - 显式 `throw` 是你**写死的控制流**，没有捕获点就是写错了 → **编译期拒绝**；
> - 内建函数的失败是**运行环境的分支**，编译器放行到运行期；
>   未被捕获就以 panic **当场明确终止**，而不是让你静默拿到一个坏句柄继续跑
>   （与 [Fail-Fast 铁律 §20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)同调）。
>
> 想要「不终止」，就在调用处包一层 `try / catch`。**是否捕获是你的选择，
> 编译器只负责让失败可见、且绝不静默。**

### 26.2 `Error` 类型：内建、ARC 管理

| 项 | 说明 |
| --- | --- |
| 类型名 | `error`（底层 LLVM `ptr`，**不透明**——与 string/int[] 同一契约，见 [§14.1](#141-不透明指针契约)） |
| 构造 | `Error("消息")`，**唯一**的构造方式（不是关键字，与 `open`/`print` 同类的内建函数） |
| 内存 | **受 ARC 管理**（带引用计数），与 `string` / `int[]` / `map` 同等对待 |
| 方法 | `.msg()` → 返回消息字符串（`string`） |
| 打印 | `print(e)` 等价于打印 `e.msg()` |
| 存储 | 可赋给变量、可作参数/返回值，引用计数自动维护 |

**严禁抛出非 `Error` 类型**——这条由 Sema 强制，编译期即拒绝：

```dial
throw 42;                 // ❌ 语义错误：只能抛出 Error 类型，禁止抛出裸的 int
throw "oops";             // ❌ 同上（string 也不行）
throw Error("oops");      // ✅
```

### 26.3 失败与业务分离（两码事）

判据只有一条：**这是 bug，还是运行环境的正常分支？**

| 判为「系统崩溃」走 `ud2` | 判为「业务失败」走 `throw` |
| --- | --- |
| 下标越界（数组 / 字符串） | 文件不存在 / 无访问权限 |
| 整数除零 | 网络超时、连接被拒 |
| 重复释放 / use-after-free | 磁盘写满、配额不足 |
| 内存分配失败（OOM） | 外部输入格式非法 |

实现上严格两段式，**判据在 C 侧、抛出在 Dial 侧**：

1. C Runtime 只返回**错误标志**（`open` 返回 `NULL`、`write` 返回 `-1`），绝不自己 longjmp；
2. Dial 层（`open`/`read`/`write` 内置的 codegen）识别该标志，构造 `Error` 并 `throw`。

> 这样 C Runtime 保持纯粹（可单独复用、可测试），Dial 层独占控制流的跳转权。

### 26.4 抛出前自动释放 ARC 引用（防漏核心）

异常最容易埋的雷是**内存泄漏**：控制流一跳走，try 体里的 `string` / `int[]` / `error` 局部
就随栈展开而"丢失"，引用计数永不清零。

Dial 的做法：**在 `longjmp` 之前，编译器自动为「try 作用域内」所有活跃 ARC 局部插入 `release`**。

```dial
try {
    let s: string = "很长的字符串……";   // rc = 1
    let a: int[] = [1, 2, 3];           // rc = 1
    throw Error("炸了");                 // ← 跳转前：先 release(s)、release(a)，再 longjmp
} catch e {
    print(e.msg());                      // s 与 a 已正确释放，无泄漏
}
```

释放范围的精确界定（实现细节）：

- 编译器在 `try` 入口记录**当时的作用域深度** `depth`；抛出时只释放 `scopes[depth..]`——
  即「try 体 + 其中嵌套块」的局部，**绝不触碰外层作用域**的变量。
  比"记录变量名集合"更稳健：天然兼容同名遮蔽，不会误放外层的同名变量。
- **被抛出的错误对象本身不被释放**：若它来自某个局部变量（如 `throw myErr;`），
  该变量在 `throw` 处被标记 `moved`，所有权随异常转移给 `catch`，因此不会被误放（避免 UAF）。
- 结构体虽是值类型，但其**堆字段**（string/int[]）同样被递归释放。

### 26.5 未捕获异常：终止，绝不静默继续

`throw` 若不在任何 `try` 内（或 `catch` 块里再次抛出且外层无 `try`），属于**未捕获异常**。
此时程序打印诊断并**终止进程**：

```
dial: 未捕获的异常（panic）: boom
```

> 与 [Fail-Fast §20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)同调：宁可当场死，
> 也绝不让一个没人处理的错误被悄悄吞掉——那才是真正昂贵的失败模式。
> 注意编译器已在 Sema 阶段直接拒绝「`throw` 不在 `try` 内」，所以这条通常只在
> 运行期由 `open`/`read`/`write` 等业务失败触发。

### 26.6 零隐式负担（零成本抽象）

**没有异常的代码，运行时开销严格为零。**

| 代码里有 try/catch 吗 | 生成的东西 |
| --- | --- |
| **没有** | **完全没有**异常相关指令：无 `setjmp`、无异常缓冲区分配、无 ARC 释放插入 |
| 有 | 进入时一次 `dial_exbuf_new` + `setjmp`；正常结束时一次 `dial_exbuf_free`；**仅当真的抛出时**才做 ARC 释放 |

实测（`nm` 检查产物符号，见 `examples/error_zero_cost.dial`）：

```
# 无 try/catch 的程序（error_zero_cost）：
$ nm ./error_zero_cost | grep -iE "setjmp|exbuf|dial_throw|dial_error"
（无任何输出 —— 零开销）

# 有 try/catch 的程序（try_catch_demo）：
$ nm ./try_catch_demo | grep -iE "setjmp|exbuf|dial_throw|dial_error"
0000000001002bb0 T dial_exbuf_new
0000000001002c10 T dial_throw_to
...
```

> 底层用 `setjmp` / `longjmp`（经 C Runtime 封装 `jmp_buf`，对 IR 只暴露为 `i8*`）。
> **关键约束**：`setjmp` 必须由「包含 try 的那个函数」亲自调用——
> `jmp_buf` 里存的是调用点的栈指针，若把它包在某个 C 助手函数里、助手一返回，
> 之后再 `longjmp` 就是跳进一个已死的栈帧（未定义行为）。
> 因此 IR 直接发起 `setjmp`（并标 `returns_twice` 属性），而 `longjmp` 由 C 侧的 `dial_throw_to` 发起。

### 26.7 完整示例

见 `examples/try_catch_demo.dial`（黄金输出 `try_catch_demo.expected`）：

```dial
fn main(): int {
    // 1) 捕获「业务失败」：open 打不开文件 -> 抛 Error -> 被 catch e 捕获
    try {
        let f: file = open("no_such_file.txt", "r");
        close(f);
    } catch e {
        let prefix: string = "caught: ";
        print(prefix.concat(e.msg()));
    }

    // 2) catch 省略变量名：忽略具体错误，只要捕获到不崩溃即可
    try {
        throw Error("boom");
    } catch {
        print("ignored");
    }

    // 3) 捕获之后程序继续正常执行
    print("after");
    return 0;
}
```

输出：

```
caught: 无法打开文件（路径不存在或无访问权限）
ignored
after
```

### 26.8 当前边界

| 边界 | 说明 |
| --- | --- |
| `catch` 不做类型/条件过滤 | 只有一个 `catch`，捕获一切 `Error`；不做多 catch 分支匹配 |
| `Error` 只有 `.msg()` | 不提供错误码、错误链、自定义子类 |
| `finally` | 不支持（用 try 之后的代码即可，Dial 无异常穿透的清理需求） |
| 跨函数抛出 | `try` 只捕获**本函数内**（含被调函数）抛出的异常 |
| `select` 各 `arm` / `try` 体的 moved 分析为独立快照 | `move_to` 只在「流敏感」的单条路径上标记「已转移」。若在某 `select` arm 里 `move_to(x, ch)`，编译器**不会**因它而把另一 `arm` 里的 `x` 误判为「已转移」（各 arm 互斥，互不影响）；同理 `try` 体里 move 的变量，也不会让 `catch` 体误以为它已转移。**已知限制**：相反方向的误报目前不在编译期消除——若你在 `select` 的某 arm 或 `try` 体里先 `move_to(x, ...)`，再在「另一条互斥路径」里引用 `x`，编译器可能因全局 moved 标记而报「已转移」。该用法（在互斥分支间转移同一变量）极其罕见，当前记为已知限制，不阻断编译；后续若需要再做流敏感的 moved 状态回滚 |

---

## 27. 所有权转移与多线程并发（`move_to` · `thread` · `channel`）

> v0.8 起可用。**Dial 用「转移所有权」取代「共享内存 + 锁」**：
> 数据不共享，只移交。队列里流动的指针永远只有一个持有者，因此根本不存在数据竞争，
> 也就不需要任何互斥锁——这是本节全部设计的出发点。

### 27.1 三个新东西

| 名字 | 类别 | 说明 |
| --- | --- | --- |
| `thread` | 类型 | 线程句柄（不透明指针）。**不受 ARC 管理**，必须显式 `join` 或 `detach` 之一 |
| `channel<T>` | 类型 | 通道句柄（不透明指针）。跨线程共享 → **原子**引用计数 ARC |
| `spawn(fn, args...)` | 内建 | 创建并**立即**启动线程，返回 `thread` |
| `join(t)` / `detach(t)` | 内建 | 等待线程结束 / 分离线程；二者选一且仅一次 |
| `channel(cap)` | 内建 | 创建容量为 `cap` 的有界通道 |
| `recv(ch)` | 内建 | 阻塞取出一个元素，返回类型即 `T`，接收方成为其唯一所有者 |
| `move_to(x, ch)` | 内建（intrinsic） | 把 `x` 的所有权**移交**给通道 `ch` |

### 27.2 `channel<T>` 与所有权移交

```dial
fn producer(ch: channel<string>): void {
    var s: string = "from-thread";
    move_to(s, ch);          // 移交：s 此后不可再用
}

fn main(): int {
    let ch: channel<string> = channel(2);
    let t: thread = spawn(producer, ch);
    let r: string = recv(ch);
    join(t);
    print(r);                // from-thread
    return 0;
}
```

- **元素类型白名单**：只能是 `string` / `int[]` / `map` / `error` / `channel<...>`。
  通道传的是所有权，只有句柄能做到 O(1) 的指针搬运；`int` / `float` / `bool` 每发一次都要
  装箱（malloc 8 字节），既砸掉「零分配移交」的立身之本，又让 C 侧无法判断槽里的东西
  该 `free` 还是直接丢——因此在**编译期**就拒绝。
- **元素类型是编译期的**：`channel(cap)` 返回 `channel<void>`（未定占位），声明处写出
  `let ch: channel<string> = ...` 才确定 `T`。运行时已擦除（队列只存 `void*`），
  `recv` 的返回类型从变量自身的静态类型取出。
- **`send` / `recv` 不做任何 retain / release**：指针进出队列，引用计数一个数都不动。

### 27.3 零竞争移交与 O(1) 论证

`move_to(x, ch)` **不是函数调用**，是编译期 intrinsic。Codegen 只发三条 IR 指令：

```llvm
%v = load ptr, ptr %x_slot             ; 取指针（不 retain）
call void @dial_channel_send(ptr %ch, ptr %v)
store ptr null, ptr %x_slot            ; 原槽清零
```

- **O(1)**：没有调用栈、没有参数编组、没有引用计数操作，就是一次指针入队加一次清零。
- **零竞争**：`spawn` 的所有实参都在 `pthread_create` **之前**写进线程的 argcell，
  因此不存在「先启动、再移交」的窗口。这也是为什么**不支持**「move_to 到一个已启动的线程」——
  那条路必然有竞争，Dial 选择在编译期就把它关掉。`spawn(fn, x)` 本身就是「move_to 到线程」。

### 27.4 移交之后：编译器的死命令

一旦 `move_to`（或 `spawn` 的堆类型实参移交）发生，该变量即进入 **Moved** 状态，
此后**读取、写入、再次转移**全部当场编译失败：

```
[dialc 错误] 第 N 行: 语义错误：变量 's' 已转移，无法使用
```

Moved 标记落在**声明该变量的那个作用域**上（而非最内层），所以「if 分支里移交、
主流程再用」也会被保守拦下。生命周期交接的会计规则：

| 实参类型 | `spawn` 时的行为 |
| --- | --- |
| `int` / `float` / `bool` | **按值拷贝**（8 字节直存），源变量不受影响、之后照用 |
| `string` / `int[]` / `map` / `error` | **移交**：源槽清零 + 标 Moved，**不 retain 不 release** |
| `channel<T>` | **共享**：原子 `retain` +1，源变量仍可用（通道天生要被两端持有） |

被移交的那一份所有权，由**接收方**负责最终释放：

| 场景 | 引用计数推演 |
| --- | --- |
| `spawn(worker, s)`，worker 不再移交 | main 移交 rc=1 → worker 入口 retain(2) → worker 出口 release(1) → thunk release(0) ✅ |
| worker 内 `move_to(a0, ch)` | 入口 retain(2) → `moved` 使出口**跳过** release → 2 → thunk release(1，归通道) → 接收方释放(0) ✅ |
| `channel` 作 spawn 实参 | 原子 retain(2) → worker 入口(3) → 出口(2) → main 作用域结束(1) → thunk release(0) ✅ |
| `let r : string = recv(ch)` | `recv` 是调用而非变量引用，VarDecl **不会** retain → 直接取得所有权(1) → 作用域结束(0) ✅ |

### 27.5 `spawn` 的底层：thunk + argcell

C 的 `pthread_create` 只认统一签名 `void (*)(void*)`，而 Dial 函数实参类型各异。
编译器为每个被 spawn 的函数生成一个**私有 thunk**：

```llvm
define private void @__dial_thunk_producer(ptr %cell) {
  %p0 = getelementptr ptr, ptr %cell, i64 0
  %a0 = load ptr, ptr %p0                       ; 取出 channel 实参
  call void @producer(ptr %a0)
  call void @dial_channel_release(ptr %a0)      ; 移交来的那一份由本线程最终释放
  ret void
}
```

- argcell **每槽固定 8 字节**：指针 / `i64` / `f64` 直接 `store`；`bool(i1)` 存前 `zext`、
  取后 `trunc`。
- 线程句柄内部**只存 `pthread_t`**，启动参数包由子线程第一时间取走并释放，
  因此 `join` / `detach` 可以立即回收句柄，无需与子线程做原子握手。
- `join` / `detach` 之后 IR 会把句柄槽置 `null`，二次 `join` 触发 `DIAL_HEAP_ASSERT` 当场 Fail-Fast，
  而不是退化成 use-after-free。

### 27.6 确定性纪律（写并发代码时必须遵守）

> **子线程一律不要 `print`**，要观察结果就走 channel 汇聚回 main 后按序打印。
> 多线程输出的交错顺序不确定，会让「逐字节比对」的黄金测试随机变红且极难复现。
> 多生产者通道的到达顺序同样不确定——需要确定顺序时用**单生产者**。

### 27.7 通道关闭与 EOF（v0.81）

v0.8 的通道有一个致命缺口：**没有 EOF**。生产者一旦退出，消费者就永远卡在 `recv` 上——
程序不是跑错，而是**死锁**。v0.81 把这块补上，新增四个内建：

| 内建 | 签名 | 语义 |
| --- | --- | --- |
| `close(ch)` | `(channel<T>) -> void` | 关闭通道（幂等），广播唤醒所有等待者。**不释放通道对象** |
| `done(ch)` | `(channel<T>) -> bool` | **已关闭 且 已空** —— EOF 的精确定义 |
| `closed(ch)` | `(channel<T>) -> bool` | 是否已调用过 `close` |
| `len(ch)` | `(channel<T>) -> int` | 队列中当前的剩余元素数 |
| `try_recv(ch)` | `(channel<T>) -> T` | 非阻塞取出；队列空立即返回，绝不等待 |

#### `done` 为什么是 EOF 的正确判据

`done = closed && len == 0`。它具有一个关键性质：**单调**——一旦为真就永远为真。
因为关闭不可逆，且关闭后 `send` 被禁止，元素只会越取越少，绝不会「刚判断完又冒出一个」。
单调性保证了这个循环**必然终止**：

```dial
while (not done(ch)) {          // 注意：Dial 的逻辑非写作 not，不是 !
    var r: string = recv(ch);
    print(r);
}
```

若 `done` 为假，则要么队列里有元素（`recv` 立刻返回），要么还有生产者会送来或最终关闭
（`recv` 阻塞等一个一定会来的事件）。因此这个循环**既不漏收，也不死锁**。

> 不要用 `len(ch) == 0` 当结束条件——生产者还没来得及发第一个元素时它也成立，会漏收。
> 也不要用 `closed(ch)`——关闭后可能还有残留元素，会提前退出、丢数据。

#### `close` 与 ARC 为什么不会打架

v0.8 曾刻意不给通道加 `close`，担心「关掉之后变量还在、作用域结束再 `release` 一次」造成二次释放。
复核后确认该担心不成立，关键在于**职责划分**：

- `close` 只**置标志 + 广播**，既不释放通道对象，也不动引用计数；
- 通道对象的销毁仍然**只由原子 ARC 归零触发一次**。

二者不重叠，因此 `close` 之后继续持有变量、继续 `recv` 残留元素、最后正常退出作用域，都是安全的。

#### EOF 之后拿到的是什么

`recv` / `try_recv` 在「已关闭且已空」时返回**空指针**（不是空串、不是默认值）。
这是刻意的：NULL 意味着「没有值」，一旦被继续使用，Runtime 会在**使用点**当场 Fail-Fast
（例如 `print` 一个 NULL string 会打印诊断并 `ud2`），而不是让它伪装成空串悄悄流进业务逻辑——
这正是 [§20 Fail-Fast 铁律](#20-fail-fast-铁律宁可崩溃绝不静默损坏) 的要求。

正常写法压根碰不到它：`while (not done(ch))` 守住了循环，`done` 为真时根本不会进去。

#### 关闭后 `send` = Fail-Fast

关了还发是最危险的一种误用：元素会**永久滞留**在队列里，而只要队列非空 `done` 就永远为假，
消费者的收尾循环将永不结束——一次「关了还发」会把 EOF 语义整个废掉，让程序重新退化成死锁。
因此 ReleaseSafe 及以上当场终止；只有 `--fast` 才关闭该检查（后果自负）。

> `close` 同时服务于 `file` 与 `channel`（二者都是「显式终结」），按实参静态类型分派。

### 27.8 判空、多消费者安全与超时（v0.9）

v0.81 补上了 EOF，但并发底座还剩三处硬伤。v0.9 一次把它们清干净。

#### 27.8.1 `is_null(x)`：Dial 终于有了判空

`recv` / `try_recv` / `recv_timeout` 在 EOF 或超时后返回的是**空指针**。而 Dial 没有 `Option`
类型、没有 `null` 字面量——用户此前**无从分辨**「这是没有值」还是「这是一个真实值」，
直接使用会撞上 Runtime 的 Fail-Fast 断言（[§20](#20-fail-fast-铁律宁可崩溃绝不静默损坏)）。

```dial
var r: string = recv(ch);
if (is_null(r)) {
    // 没有值（EOF 或超时）
} else {
    // r 是一个真实的值
}
```

准入名单：`string` / `int[]` / `map` / `error` / `channel<T>` / `thread` / `file`。
`int` / `float` / `bool` 是**值类型，永远非空**，对它们判空是纯噪音——编译期直接拒绝，
而不是让它变成一段恒为 `false` 的死代码。

落到 IR 上只是一次指针比较（`ptrtoint` + `icmp eq 0`，优化器会折叠回 `icmp eq ptr %p, null`），
**不产生任何函数调用，也不碰引用计数**。

#### 27.8.2 多消费者：一个被实证过的死锁

v0.8/v0.81 的通道通知用的是 `pthread_cond_signal`——只唤醒**一个**等待者。
单消费者时看不出问题；多消费者时，「队列非空」是**众多消费者共享的状态**：

> 唤醒其中一个消费者后，若队列里**还剩元素**，其余消费者会带着**已经失效的等待条件**
> （「队列为空」）继续睡死——直到下一次 `send` 才被叫醒。而那次 `send` 又只唤醒一个。
> 于是只要队列深度大于 1、消费者多于 1，就会有消费者永久阻塞。

这就是经典的**丢唤醒（lost wakeup）**。本仓库用对照实验坐实了它（`tests/stress/channel_mt.c`）：

| 版本 | 8 个消费者抢 100 万个元素（队列深 64） |
| --- | --- |
| `pthread_cond_signal`（v0.8 / v0.81） | **25 秒后超时挂死**（exit 124） |
| `pthread_cond_broadcast`（v0.9） | **4.25 秒跑完**，总数恰好 100 万，零重复、零丢失 |

修复只有一处：所有「队列状态变化」的通知一律改用 `broadcast`，让每个等待者都重新检查条件。
正确性优先于惊群开销——Dial 的并发规模远不值得为省一点唤醒去换一次死锁。

**多消费者的标准写法**：

```dial
while (true) {
    var r: string = recv(ch);     // 只在「已关闭且已空」时返回空指针
    if (is_null(r)) { break; }     // 这才是 EOF
    ... 处理 r ...
}
```

注意**不能**用 `try_recv` 判结束：`try_recv` 在「队列暂时空」时也返回空指针，
那不是 EOF，只是「现在没有」。多消费者下用 `try_recv` 会漏收元素。

> 单消费者场景仍推荐 `while (not done(ch))`（[§27.7](#277-通道关闭与-eofv081)）——
> 它连一次多余的 `recv` 都不需要。

#### 27.8.3 `recv_timeout(ch, ms)`：带超时的接收

```dial
var s: string = recv_timeout(ch, 500);   // 最多等 500 毫秒
if (is_null(s)) {
    // 超时（或者通道已关闭且已空）
}
```

- **超时是上限，不是下限**：元素提前到了就立刻返回。
- **超时不会吞掉元素**：`pthread_cond_timedwait` 返回 `ETIMEDOUT` 后会回到 `while` 条件
  重新判定，若此刻队列非空则正常取走——「元素恰好在超时这一刻到达」也能被拿到。
- 所有权规则与 `recv` 完全一致：取出的元素归接收者所有，不做 retain / release。
- 底层实现就是 `pthread_cond_timedwait` + 一个绝对时刻，**没有引入任何状态机**。

> 超时是**边界情况下的兜底**，不是常态。正常收尾仍应走 `close` + `recv` 的 EOF 路径——
> 超时会让「生产者在慢、消费者在快」这种正常节奏也变成一次失败的等待。

#### 27.8.4 ARC 会计：不变，但被验证了

多消费者不会破坏所有权会计：环形队列的每一次出队都在**同一个互斥量**下完成，
因此每个元素只会被弹出一次，也就只会有一个持有者——**结构上不可能 double-free**。

`tests/stress/channel_mt.c` 用「位图 + 原子 `fetch_or`」逐个记录每个序号的领取情况，
重复领取即判失败；`examples/channel_multiconsumer.dial` 从 Dial 层验证同一件事
（两个消费者抢同一个通道，元素总数守恒）。

### 27.9 多路复用 `select`（v0.91）

同时监听多个通道，**任一就绪即执行对应分支**。

```dial
select {
    case recv(chA) as a {
        print(a);
    }
    case recv(chB) as b {
        print(b);
    }
    default {
        print("暂时没有就绪的通道");
    }
}
```

#### 27.9.1 语法：Dial 风格，不是 Go 风格

Go 写的是 `case v := <-ch:`。Dial **刻意不照搬**，有两条硬性适配：

| | Go | Dial |
| --- | --- | --- |
| 接收动作 | `<-` 运算符 | **函数调用** `recv(ch)`——与 `move_to` / `spawn` 同为内建 |
| 绑定变量 | `:=` | **`as` 关键字** |

```dial
case recv(ch) as v { ... }
```

- **`select` 是语句，不是表达式。** 各 case 绑定的变量类型可以互不相同，
  因此不存在「统一返回类型」可言——绝不能写 `let x = select { ... }`（编译期拒绝）。
- 绑定变量的类型**自动推导**为通道的元素类型（`channel<string>` → `string`），
  生命周期**严格限制**在 case 的 `{}` 内，出了块再用即编译错误。
- `default` 至多一个，且必须放在所有 case 之后。

#### 27.9.2 三条语义

| # | 语义 | 说明 |
| --- | --- | --- |
| 1 | **case 只拿真实值** | 分支体内拿到的**一定是一个真实元素**，不会是空指针 |
| 2 | **全体 EOF → select 直接结束** | 所有通道都「已关闭且已空」时，select 语句结束，**一个分支都不执行**（包括 `default`） |
| 3 | **`default` 使 select 非阻塞** | 没有任何通道就绪时**立刻**执行 `default`，绝不等待 |

> **第 1 条与 `recv` 不同**：`recv` 在 EOF 时返回**空指针**，需要用 `is_null` 判别；
> 而 `select` 的 case 永远拿不到空指针——EOF 的判据是「**select 语句结束了**」。
> 若在 case 里写 `is_null(v)`，它恒为 `false`。
>
> 之所以能保证：底层用**非阻塞**取元素，若在我们挑中某个通道之后、真正取之前，
> 那个元素被别的消费者抢走了，select 会**自动重挑一个**（而不是把空指针交给用户）。

#### 27.9.3 事件驱动：底层怎么等

select **不用**「一把大锁锁住所有队列」（那会引入复杂的锁顺序问题），
而是走**事件驱动**路线：

- 任何通道的状态变化（`send` / `recv` / `close`）都递增一个**全局事件纪元 `epoch`**，
  并在**全局条件变量**上广播；
- select 等待时**先读 epoch → 再查各通道是否就绪 → 若都不就绪就睡到 epoch 变化为止**。

两个关键性质：

| 性质 | 保证方式 |
| --- | --- |
| **不丢唤醒** | epoch 是「先读后查」的：等待前若已有事件发生，epoch 已经变了，等待条件立即不成立，绝不会带着过期的假设睡死 |
| **不死锁** | 通知（notify）一律在**释放通道本地锁之后**才调用 —— 「持通道锁 → 取全局锁」与「持全局锁 → 取通道锁」两种嵌套**都不存在**，全局锁与通道锁永不死锁 |

实测：50 万次「send + select」往返，**wall 0.19 s**，CPU 0.36 s。

> **Windows 说明**：Windows 没有 pthread，对应实现是 `WakeConditionVariable` / `SetEvent`
> （见 [ARCHITECTURE.md §0.2](ARCHITECTURE.md#02-平台差异对-c-runtime-的约束实现路线备忘) 的线程后端规划）。

#### 27.9.4 随机选取：防饿死

多个通道同时有数据时，select **随机**挑一个就绪的 case 执行。
若固定总挑第一个，第二个通道的数据会被一直压着——这就是经典的**饥饿**。

底层用一个**原子递增**的计数器取随机起点（不能用 `rand()`，它不是线程安全的）。
`examples/select_fairness.dial` 断言「两个通道都被处理过」。

#### 27.9.5 ⚠️ 警告：`default` 是空转与死锁的来源

> **`default` 使 select 永不阻塞。** 因此把带 `default` 的 select 放进 `while` 循环里，
> 就是一个 **100% CPU 的空转循环**——每一轮都立刻返回，什么也没等到。
>
> 更糟的是：如果用 `default` 分支去「顶替」真实接收（比如在 default 里也把计数器加一），
> 就会产生**虚假计数**，进而让接收方的期望量与生产方的实际量脱节——
> 生产者把队列塞满后**永久阻塞**，接收方却以为已经收够了，整个程序死锁。

**反例（错误）**：

```dial
// ❌ 错：default 立即返回 -> 100% CPU 空转；且计数是虚假的
var n: int = 0;
while (n < 1000) {
    select {
        case recv(ch) as x {
            n = n + 1;          // 只有真的收到才加
        }
        default {
            n = n + 1;          // ❌ 什么都没收到也算数
        }
    }
}
```

这个反例在本仓库实测：**空转 0.38 s 后卡死在 `join`**（队列被塞满、生产者永久阻塞），
120 秒都没跑完。

**正例（正确）**：

```dial
// ✅ 对：default 只做「记录一次无事发生」，绝不影响计数
var n: int = 0;
var idle: int = 0;
while (n < 1000) {
    select {
        case recv(ch) as x {
            n = n + 1;
        }
        default {
            idle = idle + 1;    // ✅ 只记空闲，不冒充收到
            // 想避免空转，就在这里让出 CPU / 退避
        }
    }
}
```

#### 27.9.5.1 ⚠️ `while (true) { select { ... } }` 是空转陷阱

不带 `default` 的 select 是**阻塞**的——没有就绪通道时它会睡到有事件为止，
单看一次 select 确实不空转。**但把它放进 `while (true)` 就不是了**：

> 全体通道 EOF 时，select 只是**直接结束**（一个分支都不执行），
> 它**不会**替你跳出外层循环。外层若写成 `while (true)`，
> 那么 EOF 之后每一轮 select 都立刻结束、循环立刻重入 ——
> **这就是一个 100% CPU 的空转死循环**（本仓库实测：5 秒超时挂死，`exit 124`）。

因此 **select 循环必须自带退出条件**，只有下面两种正确写法：

**写法一（推荐）：把退出条件写进 `while` 条件**

```dial
// ✅ 对：以「未收到 EOF」为循环条件，通道关空后自然收敛
while (not done(ch)) {
    select {
        case recv(ch) as x {
            ... 处理 x ...
        }
    }
}
```

**写法二：在 case 内主动 `break`**

```dial
// ✅ 对：满足结束条件时在分支里跳出（break 跳出的是 select 外层的那个循环）
while (true) {
    select {
        case recv(ch) as x {
            if (x.eq("quit")) { break; }
            ... 处理 x ...
        }
    }
}
```

两种写法各自对应一个可运行的正例，均已连跑 10 次输出稳定（`got=2` 后正常退出，退出码 0）：

| 写法 | 示例 |
| --- | --- |
| 写法一（`while (not done(ch))`） | `examples/select_exit_condition.dial` |
| 写法二（case 内 `break`） | `examples/select_exit_break.dial` |

**凡是写不出「某条路径能让循环终止」的 select 循环，一律视为缺陷。**

> 若确实需要在「等待 + 超时」之间做选择，用
> [`recv_timeout`](#278-判空多消费者安全与超时v09) 单独接收，而不是给 select 加 default。

#### 27.9.6 未来展望：消除惊群

当前的全局广播有一个已知代价：**惊群**——一次广播会唤醒所有 select 等待者，
其中大多数醒来后发现「不是我」，于是重新睡下。

基线实现选择**逻辑正确 + 低延迟**优先于惊群开销：惊群只造成少量无谓唤醒，
而轮询方案会造成持续 CPU 空转与毫秒级延迟。

在 Linux 上可以用 **epoll + eventfd** 替代条件变量，实现「只唤醒真正相关的那一个」，
把惊群降到零。这是 v0.92 之后的优化方向，不影响本版的语义。

### 27.10 当前边界

| 边界 | 说明 |
| --- | --- |
| 线程入口必须返回 `void` | 返回值无处安放；堆类型返回值还会无人释放地泄漏，故编译期拒绝 |
| 线程入口禁止 `struct` 形参 | 值语义聚合体塞不进 8 字节实参槽 |
| `spawn` 首参仍是具名函数 | v1.03 起已有函数指针 `fn(A): R`，但线程入口 thunk 需按目标函数静态生成，故 `spawn` 仍要求顶层具名函数字面量；回调请用函数指针参数（见 §32）。**闭包仍不提供** |
| 关闭不可逆 | 没有「重新打开」；关闭后 `send` 一律 Fail-Fast |
| `recv` 仍会阻塞 | 未关闭时队列空就一直等；要非阻塞请用 `try_recv`，要限时请用 `recv_timeout` |
| `select` 多路复用 | **已支持**（v0.91）。但它**不带超时**——要「等一会儿再看」，用 `recv_timeout` 单独接收 |
| `select` 有惊群开销 | 全局广播会唤醒所有等待者，多数醒来后发现「不是我」。Linux 上可用 epoll/eventfd 消除（见 [§27.9.6](#2796-未来展望消除惊群)） |
| `select` **不受安全等级影响** | `--fast`（ReleaseFast）**不改变 select 的任何语义**：阻塞等待、全体 EOF 判定、`default` 非阻塞、随机选取防饿死，在三档等级下**完全一致**——这些是**协议语义**，不是「安全检查」。唯一随等级变化的是**向已关闭通道 `send`**：Debug / ReleaseSafe 下当场 Fail-Fast；`--fast` 下该断言被**编译期消除**（`DIAL_HEAP_ASSERT` 仅在 `DIAL_SAFETY >= SAFE` 时生效），于是元素被静默丢弃、`done` 可能永远为假 → 接收方**永久阻塞**。与 G 组的约定同理：**协议语义不随安全等级漂移** |
| `select` 不支持表达式 | 它是**语句**：各 case 的绑定变量类型可以互不相同，没有统一返回类型 |
| 无 `Option` 类型 | 「有没有取到」用 `is_null(x)` 判断；没有 `Some` / `None` 这类包装 |
| 多消费者 | **已支持**（v0.9）。前提是消费者用 `recv` + `is_null` 判结束；用 `try_recv` 判结束会漏收。`select` 的 case 则永远拿不到空值 |
| 无线程池 / 无异步 | 一个 `spawn` 就是一个真线程 |
| `join` 后句柄失效 | 二次 `join` / `detach` 触发 Fail-Fast 断言 |

---

## 28. 语言服务器 `dialc lsp`（v0.94 骨架 → v0.95 增强 → v0.98 工业化）

```bash
dialc lsp
```

它不从终端读命令、也不往终端写报告：**stdin 收 JSON-RPC，stdout 回 JSON-RPC**，
按 `Content-Length` 分帧。你不会手敲这条命令——它由编辑器启动
（[`editors/vscode/`](editors/vscode/) 下有一个免安装依赖的 VS Code 扩展）。

```
编辑器  ──stdin──▶  dialc lsp  ──stdout──▶  编辑器
                        │
                        └─ 词法 → 语法 → 语义（与 dialc build 同一套代码）
```

### 28.1 为什么是「同一套代码」而不是独立的解析器

编辑器里的诊断最容易出的毛病不是报错太少，而是**与编译器不一致**：
编辑器说没问题、编译器说有；或者反过来。根因几乎总是——编辑器侧另写了一套解析器，
两套实现慢慢长歪。

`dialc lsp` 不写第二套：它直接调用 `lexer::lex` / `parser::parse_with` / `semantic::analyze`。
因此**编辑器里的红线与 `dialc build` 的报错是同一条错误、同一个位置、同一句文案**。
代价是每次诊断都要跑一遍完整前端；收益是「所见即所编」，这个代价值得。

### 28.2 已支持

| 能力 | 时机 | 说明 |
| --- | --- | --- |
| **诊断** | `didOpen`、`didSave` | 词法 / 语法 / 语义三阶段任一失败即停，只报**第一个**错误（与 `dialc build` 一致：第一个错误之后的位置信息已经失真，报一串连锁错误只会淹没真正的问题） |
| **诊断清空** | `didSave`（改正后）、`didClose` | 关掉文件后不留残留红线 |
| **语义高亮** | `textDocument/semanticTokens/full` | 关键字 / 字符串 / 数字 / 运算符 / 类型 / 标识符 |
| **悬停（Hover）** | `textDocument/hover` | 显示符号名称、推断类型、声明片段；包符号额外显示 `.diali` 的 `///` 文档并标注「外部函数」 |
| **跳转定义** | `textDocument/definition` | 文件内符号回本文件精确 `Location`（UTF-16 列换算）；包符号跳进 `.diali` 的声明行 |
| **基础补全** | `textDocument/completion`（`.` 触发） | 收集光标可见作用域内的全部符号，按已输入前缀过滤；空白光标给全量候选 |
| 能力协商 | `initialize` | 声明 `textDocumentSync = {openClose: true, change: 2, save: {includeText: false}}`（选项形式：v1.10 起增量同步 + 接收 `didSave`，见 §28.7.2）、语义 token 的 legend，以及 `hoverProvider` / `definitionProvider` / `completionProvider` |

刻意**不**在 `didChange` 时发诊断：打字到一半的代码必然是错的，
边打边报会让红线在半句话上反复闪。诊断留给保存时——这也是本节标题写「保存时诊断」的原因。

### 28.3 两个容易出错、已经处理好的细节

**行列基准不同。** dialc 内部的 `line` / `col` 都从 **1** 起，LSP 的 `line` / `character`
都从 **0** 起。少减一个 1，整列高亮就会偏移一格。

**列的计量单位不同。** 词法器按 **char** 记列，LSP 按 **UTF-16 码元**记列。

这里有个容易搞错的直觉：很多人以为「有中文就要换算」，**并不对**。
中文「你」在 BMP 内，1 个 char 就是 1 个码元，`"你好"` 连引号是 4 个 char 也是 **4 个码元**——
它根本测不出换算错误。真正分叉的是 BMP **之外**的字符（emoji 等）：
`"🙂"` 是 **3 个 char 但 4 个码元**，少算那一个码元，这一行后面所有 token 会整体右偏一格。

I 组测试因此**同时**放了两行：中文那行证明长度本身没算错，emoji 那行才真正锁定换算。
我们验证过这个测试不是摆设：把换算那一行代码删掉重跑，只有 emoji 那条变红（中文那条依然绿）——
正是上面这段分析所预期的样子。

### 28.4 刻意没做的部分

> **本节记录的是 v0.94 骨架当时的决策。** 当时「不做」的理由是：**缺少跨文件符号表**，
> 硬做只能做出「猜」的结果——那比不做更糟。
>
> 其中**前三项（悬停 / 跳转定义 / 补全）已在 v0.95 补上**（见 [§28.6](#286-lsp-增强hover--跳转定义--补全v095)）。
> 表格里仍保留它们，是为了记录**这个决策后来如何被推翻**——而不是说它们至今未实现。
> **重命名 / 引用查找 / 签名帮助已在 v0.98 轮 2 补上**（见 [§28.7.1](#2871-当前-lsp-能力清单)）；
> **格式化 / inlay 提示在轮 3 补上**，**codeAction / folding / selection / documentLink 在轮 4 补上**——
> 至此本表列举的能力**全部已实现**（下表保留历史理由，是为记录决策如何被推翻）。

| 没做 | 原因 |
| --- | --- |
| 悬停、跳转定义、补全 | **v0.94 当时刻意不做**（无跨文件符号表）；**已在 v0.95 落地**（见 [§28.6](#286-lsp-增强hover--跳转定义--补全v095)）。它们建立在 v0.95 新增的**跨文件符号表**（`src/index.rs`）之上——该模块独立于类型检查、可容错，半句没写完的代码也能索引 |
| 重命名、跨文件引用查找、格式化 | 重命名需要全工作区引用图，当前索引器只建符号、不建引用边；格式化交给编辑器/用户。**v0.98 轮 2（重命名 / 引用）与轮 3（保守格式化）已补上**——重命名靠索引器新增的**引用边**，格式化只做「去行尾空白 + 末尾换行」这类**绝不重排**的非破坏性动作 |
| 区分变量名与函数名（高亮上） | 需要符号表。当前标识符一律标 `variable`：**宁可不标，也不标错**（v0.95 的 Hover/补全已能区分种类，但语义高亮的 legend 暂未细分） |
| 行注释高亮 | 词法器直接丢弃注释、不产 token，语义 token 也就拿不到它。注释高亮交给编辑器的 TextMate 语法（已在扩展里提供），那才是它的主场 |
| 增量文档同步 | **v1.10 已补上**：`textDocumentSync.change` 由 `1`（Full）升为 `2`（Incremental），按 LSP 的 UTF-16 行列把 `contentChanges[].range` 换算成字节补丁；不带 `range` 的整篇 change 仍兼容（老客户端 / 冒烟脚本）。见 §28.7.2 |
| 引入 LSP 框架（tower-lsp 等） | 本骨架只需要「读一条 JSON、写一条 JSON」。框架带来的异步运行时与依赖树，远超它省下的那点代码 |
| 补全时自动补 `import` 语句（`additionalTextEdits`） | 轮 3 规划时留的占位，最终**不做**：插入位置有歧义（文件头？现有 import 之后？），而 Dial 的 `import` 还要求包**已安装**——自动插一行 `import "x";` 却没装包，只会把「缺包」这个真问题推迟到编译期才暴露。更要紧的是 LSP 的 `additionalTextEdits` 会**在用户接受补全时静默改文件**，而用户只请求了「补一个名字」。宁可让用户自己写 import，也不擅自改他没要求改的地方 |
| 诊断的表达式级精确定位 | 语义错误目前定位到**语句**级（语句带位置，表达式 `Expr` 不带）。要精确到表达式，得给 15 个 `Expr` 变体全部加上位置并贯穿解析器与语义分析器——改动面太大，而收益只是波浪线短几个字符。**先保证「落在正确的语句上」**（旧行为是落在文件开头） |

### 28.5 诊断位置（v0.94 顺带修掉的一个缺陷）

Dial 的 `Expr` 不携带行列，因此表达式里的语义错误（比如 `'a' + 1`）天然「不知道在哪」，
原先一律报 `第 0 行 第 0 列`——在编辑器里就是**第一行一个红点**，等于没定位。

v0.94 的处理：给**所有**语句补上位置（连 `If` / `While` / `For` / 表达式语句都补了，
此前只有 `var` / `return` 这类带），然后在语义检查的语句边界上做**兜底包装**：

```rust
// 只补「缺失」的位置，已经精确定位的错误原样放行
check_stmt(...).map_err(|e| if e.line == 0 && e.col == 0 {
    DialcError::new(e.message, line, col)   // line/col = 本语句的位置
} else { e })
```

于是 `let c: char = 'a' + 1;` 的诊断落在**这一行**，而不是文件开头。
代价是精度只到「语句级」而非「表达式级」——这是不做 Expr span 的必然结果，
也是当前规模下正确的取舍：语句级定位已经够用，而给每个 Expr 加 span 会牵动
解析器与代码生成的每一处构造点。

顺带一提，也是这次改动让 B 组负向用例的输出全部带上了「第 N 行 第 M 列」。

---

### 28.6 LSP 增强：Hover / 跳转定义 / 补全（v0.95）

v0.94 把「编辑体验」的第一块地基（诊断 + 高亮）铺好了，但当时**刻意没做**悬停、跳转定义、补全——
理由是「都建立在跨文件符号表之上，Dial 还没有符号表可查」。

v0.95 补上的，正是这座符号表。它不是把 `semantic::analyze` 的副产品拿过来用，
而是**另起一座独立、可容错的索引**（`src/index.rs`）：

> 索引器**不调用**语义分析，只复用前端的「解析成形」（`parse_diali` / `resolve_type_name`）。
> 因此**半句没写完的代码也能索引**——你打字打到一半、缺括号、少分号，Hover 与补全照样有反应，
> 不会因为「语义没过」就整个崩掉。代价是索引不保证类型一定正确，但它**宁可给一个保守的答案，也不标错**。

#### 28.6.1 作用域链「由内向外」查找（遮蔽查的是内层）

符号按**作用域树**组织：每个 `if` / `while` / `for` / `try` / `select` / `else` 块各开一个子作用域，
挂到外层作用域的 `parent` 上。查找时**从最内层作用域沿 parent 链一路向外**：

```text
fn main() {
    let x: string = "outer";      // 外层 x
    if (true) {
        let x: int = 2;           // 内层 x，开了一个子作用域
        total = demo_add(x, 3);   // 这里的 x 查到的是内层那个 int
    }
}
```

这与 `semantic::lookup_type` 的语义**完全对齐**——所以 Hover / 跳转定义 / 补全三处对「遮蔽」的判定
和编译器对「遮蔽」的判定是同一套规则，不会编辑器里查的是内层、编译器实际用的是外层。

#### 28.6.2 跨文件符号：import 驱动的自动扫描

索引器不需要任何用户配置。它顺着 `import "demo";` 去 `~/.dial/packages/demo/` 找已安装的包，
复用 v0.93 的 `find_installed` / `verify_iface` / `parse_diali` 三件套，把包装进内存符号表：

| 包装导出 | 进来后是什么 |
| --- | --- |
| `type DemoHandle;` | 不透明类型符号（`OpaqueType`），补全是 `DemoHandle` |
| `extern fn demo_add(a: int, b: int): int;` | 外部函数符号（`ExternFn`），补全是 `demo_add` |
| `/// 两数相加` | 挂到对应 `extern fn` 的 `doc` 上，Hover 时显示 |

两样东西是「跳进 `.diali`」和「Hover 显示包文档」的关键素材，v0.95 把它们一并记下来：

- **声明行号**：每个 `extern fn` / 不透明类型在 `.diali` 里的行号——跳转定义据此把光标送进 `.diali` 的精确一行；
- **`///` 文档**：Hover 包函数时直接渲染出来，并标注 **「外部函数」**，与本地函数区分开。

#### 28.6.3 三个能力的契约

**Hover**（`textDocument/hover`）
- 文件内符号 → 名称 + **推断类型** + 声明处片段；
- 包符号 → 函数签名 + `.diali` 的 `///` 文档 + 「外部函数」标注。

**跳转定义**（`textDocument/definition`）
- 文件内符号 → 返回**本文件**的精确 `Location`，列偏移按 UTF-16 换算到标识符起止；
- 包符号 → 返回 `.diali` 文件的 `Location`，落点正是该 `extern fn` 的**声明行**。

**基础补全**（`textDocument/completion`，触发字符 `.`）
- 收集光标**可见作用域**内的全部符号（本文件局部 + 全局函数/结构体 + 已 import 的包符号）；
- 按光标处**已输入前缀**过滤：`demo_` 前缀只留 `demo_add`，不会把 `total` / `DemoHandle` 混进来；
- 即使光标落在空白（前缀为空）也返回当前可见的全部候选。

#### 28.6.4 刻意仍不做（v0.95 当时）/ 后续已补

> 下表是 v0.95 当时的「暂不做」清单。**重命名 / 引用查找 / 签名帮助已在 v0.98 轮 2 补上**——
> 索引器新增引用边（`Ref`），不再「只建符号、不建引用边」。其余项仍保持原决策。

| 没做 / 已补 | 原因 |
| --- | --- |
| 重命名、引用查找、签名帮助 | **轮 2 已实现**（v0.98）：基于引用边，所见即所改、同名遮蔽天然正确 |
| 格式化 | 非破坏性格式化留给轮 3；Dial 没有官定代码风格，激进格式化易误伤 |
| 诊断下放到 `didChange` | 打字到一半的代码必然有错，边打边报只会让红线在半句话上闪；诊断仍留给保存时 |
| LSP 框架（tower-lsp 等） | 本服务仍是手写的「读 JSON / 写 JSON」，框架的异步运行时与依赖树不划算 |

### 28.7 编辑器插件安装包（v0.98）

`dialc lsp` 是**唯一的**编辑器后端（stdio JSON-RPC）。v0.98 补齐了三套主流编辑器的接入，
目录都在 [`editors/`](editors/)：

| 编辑器 | 接入方式 | 安装包 | 说明 |
| --- | --- | --- | --- |
| **VS Code** | 手写 LSP 客户端（无 npm 依赖） | [`editors/vscode/dial-1.10.0.vsix`](editors/vscode/)（`code --install-extension` 侧载） | 免 Node / npm / 联网；能力最全 |
| **Neovim** | 内置 LSP 客户端 | [`editors/neovim/`](editors/neovim/)（拷进 `runtimepath` 即可） | 零依赖，不需要 nvim-lspconfig |
| **Emacs** | 内置 `eglot` | [`editors/emacs/README.md`](editors/emacs/)（init.el 片段） | 不开发插件——eglot 原生连 LSP，加几行配置即可 |

> 编译型语言的编辑器支持是「必做的苦活」：没有它，再好的语言也没人用。三种主流编辑器之外的
> 编辑器（JetBrains / Zed / Sublime 等）只要能启动 `dialc lsp`，用各自的 LSP 客户端连上即可，
> 不再单独维护插件——把精力留给语言本身。

**一键安装顺带装插件**：跑 `install-dial.sh` / `install-dial.ps1` 时，脚本除编译器与标准库外，
还会**自动下载并安装 VS Code 插件**（从 Release 取 `dial-<版本>.vsix`，再用 `code --install-extension` 装上），
不必再自己去 Release 页翻资产、手动侧载。

- 版本只有一个来源：`DIAL_VERSION`（默认**空**：跟随最新 Release；钉版本写法如 `DIAL_VERSION=1.1`，对应发布标签 `v1.1`）——Release 标签（`v1.1`）与资产文件名
  （`dial-1.10.0.vsix`）都由它推导，不会三处各说各话。
- **没装 VS Code / 离线时只告警、不阻断安装**：编译器本体与包安装不受影响。
- **不加 `--force`**：用户若已装了更新版本的插件，不该在「一键安装」里被悄悄降级。
- 下载地址可用 `DIAL_VSIX_URL` 覆盖（自建镜像 / 网络受限时用）。
- 下载带超时（`--connect-timeout 15 --max-time`）：GitHub 不可达时脚本会**快速失败并说明原因**，
  而不是一直挂着让用户干等。
- Neovim 与 Emacs 侧无需安装动作（零依赖配置，拷进 `runtimepath` 或配几行 eglot 即可）。

#### 28.7.1 当前 LSP 能力清单

| 能力 | 状态 | 说明 |
| --- | --- | --- |
| 诊断（保存即报） | ✅ 已实现 | 复用编译器前端，红线与 `dialc build` 同因同位置 |
| 语义高亮 | ✅ 已实现 | 关键字 / 字符串 / 数字 / 运算符 / 变量 / 类型 |
| Hover | ✅ 已实现 | 类型 + 声明片段；包符号显示 `.diali` 文档 |
| 跳转定义 | ✅ 已实现 | 文件内精确跳转；包符号跳到 `.diali` 声明行 |
| 补全 + 精确替换 | ✅ 已实现 | 作用域可见符号；`textEdit` 精确范围、`sortText` 排序、`.` 触发 |
| 能力协商 | ✅ 已实现 | 按客户端 `capabilities` 动态广播，老客户端不报能力也不报错 |
| 文档符号（大纲） | ✅ 已实现（轮 2） | 顶层 fn/struct 为根，函数体形参与局部变量为子节点（递归 DocumentSymbol） |
| 工作区符号 | ✅ 已实现（轮 2） | 跨所有已打开文件 + 已加载包，按名字子串模糊搜索 |
| 引用查找 | ✅ 已实现（轮 2） | 基于索引器的引用边（`Ref`）；`includeDeclaration` 可含声明 |
| 重命名（含 prepareRename） | ✅ 已实现（轮 2） | 同符号的全部引用（含声明）一次改全；包符号只读不重命名 |
| 签名帮助 | ✅ 已实现（轮 2） | `(` / `,` 触发；解析光标前函数名，返回完整签名与参数标签、活动参数序号 |
| 格式化（保守） | ✅ 已实现（轮 3） | 仅去除行尾空白 + 确保文件末尾恰有一行换行；**绝不**重排/换行的激进格式化（非破坏性） |
| 参数 inlay 提示 | ✅ 已实现（轮 3） | 调用实参前标注形参名（如 `a: ` / `b: `）；索引独立于语义、半成品代码拿不到类型，故不做类型 inlay |
| codeAction | ✅ 已实现（轮 4） | 按诊断给安全 quickfix：缺失包 → 「安装包 'x'（dialc install）」；只发 `Command`、不擅自改代码 |
| 折叠范围 | ✅ 已实现（轮 4） | 基于作用域包围盒；顶层作用域与单行区间不报 |
| 选择范围 | ✅ 已实现（轮 4） | 从光标处逐级向外构造嵌套 `SelectionRange`（表达式 → 语句 → 块 → 函数） |
| 文档链接 | ✅ 已实现（轮 4） | `import "pkg";` 的字符串字面量指向该包的 `.diali` 接口文件，可点击跳转 |
| 文档内高亮 | ✅ 已实现（v1.10） | `textDocument/documentHighlight`：光标下标识符的全部同名 `Ident` token 返回 range（`kind=1`），复用编译器词法器 |
| 增量同步 | ✅ 已实现（v1.10） | `textDocumentSync.change = 2`；带 `range` 的 change 按 UTF-16 行列打补丁，全量 change 仍兼容 |
| 工作区文件感知 | ✅ 已实现（v1.10） | `workspace/didChangeWatchedFiles`：磁盘文件变更后对全部已打开文档重跑索引与诊断 |

> 路线：v0.98 完成**轮 1（基础稳健 + 补全增强 + 三编辑器接通）**、**轮 2（导航全家桶）**、**轮 3（编写辅助）**
> 与 **轮 4（高级能力）**——循序渐进，每轮可独立验证（见 `tools/lsp_full_smoke.py`）。
>
> 实现要点：引用 / 重命名依赖 `src/index.rs` 新增的**引用边**——索引不再只建符号、也记录每个标识符解析到哪个符号
> （`Ref.symbol_index`），因此**同名遮蔽天然正确**（内层同名变量不会被误并入外层引用）。Dial 单文件编译，
> 重命名只作用于当前 `.dial`，包符号（`.diali`）只读。
>
> **协议流纯净性（铁律）**：LSP 的 stdout 跑的是 `Content-Length` 分帧协议，
> **任何非协议字节都会让客户端分帧错位**——症状不是报错，而是**编辑器卡死**。
> 因此 stdout 只允许出现协议字节；日志一律走 stderr（CLI）或 `window/logMessage` 通知（LSP）。
> 这条纪律由两路自动化守住：`tools/lsp_full_smoke.py` 的**动态**断言（发现裸字节即失败）
> 与 `tools/check_lsp_stdout_purity.py` 的**静态**守卫（LSP 路径上出现 `println!` 即失败）。
>
> **能力协商的判据**：按「客户端有没有**提到这一段**」判断，而不是「有没有逐项点名」——
> 客户端压根没提 `textDocument` / `workspace` 段 = 它没表达偏好 → **全开**；
> 提到了该段却漏掉某项 = 明确不支持 → 不广播。
> 理由很直接：多广播一个用不上的能力零代价，少广播则让功能**静悄悄消失**、用户查不出原因。
> （`tests/lsp_features.py` 用的就是 `capabilities: {}`，正因如此才暴露过轮 1 的误判。）
>
> **诊断带稳定错误码**：消息是给人看的、码是给工具看的，两者职责不同。
> | 码 | 含义 |
> | --- | --- |
> | `dialc.lex` | 词法错误 |
> | `dialc.parse` | 语法错误 |
> | `dialc.sema` | 语义错误 |
> | `dialc.package.missing` | `import` 的包未安装（附 `data.package` 结构化包名） |
>
> v1.10 起这些码与 CLI 诊断（§34.11）**共用同一张码表**（`src/diagnostic.rs::stable_code`），
> 且每条诊断带 `codeDescription.href` 指向仓库文档——同一个错误在命令行与编辑器里拿到同一个 `code`。
>
> 工具**应按码处理**，不要去匹配消息文本——消息措辞会随版本调整，改一次就废掉一个工具。
> 缺包诊断会**真的报出来**（旧行为是被静默吞掉，编辑器里毫无提示），
> 并且带 `data.package`，于是「安装这个包」的 quickfix 靠结构化数据就能给出，不必猜。
>
> **这些码为什么不算「私自扩展」**——LSP 3.17 规范对 `Diagnostic` 的原文是：
> ```
> code?: integer | string;      // "The diagnostic's code, which usually appear in the user interface."
> data?: LSPAny;                // "A data entry field that is preserved between a
>                               //  textDocument/publishDiagnostics notification and
>                               //  textDocument/codeAction request."   (@since 3.16.0)
> ```
> 即：**码值由语言服务器自定，规范不设中央码表**（类型是 `integer | string`，我们用
> `dialc.` 前缀命名空间化，避免与其他服务器的码撞车）；而 `data` 的用途——**在诊断通知与
> codeAction 请求之间保留结构化数据**——正是我们传包名的做法，一字不差地对上规范。
>
> **文档同步用选项形式**：`textDocumentSync` 广告的是 `{openClose, change: 2, save}` 而不是
> 纯数字 `1`。规范对 `save` 的原文是「*If present save notifications are sent to the server.
> If omitted the notification should not be sent.*」——纯数字形式里**没有** `save` 这个字段，
> 客户端据此就不发 `textDocument/didSave`，而诊断正是靠它刷新的。用数字形式的话，
> 「保存即报」只在自家插件里成立（它无条件发 didSave），在别的客户端上会**静默失效**。
>
> **已知边界**：`$/cancelRequest` 被接受但**不打断**正在处理的请求。我们的请求都是微秒级的
> （没有跨文件全量扫描这类长任务），没有可观测的取消语义；而规范要求的「取消后回
> `RequestCancelled(-32800)`」需要并发执行模型才做得到。我们保证的是更重要的一条：
> **每个请求恰好回一次响应**，客户端绝不会挂等。
>
> **规范符合性是被自动检查的**：`tools/lsp_full_smoke.py` 里有一张**返回形状对照表**，
> 逐个方法核对响应结构与规范一致（`Hover` 要 `contents`、`CompletionList` 要
> `isIncomplete`+`items`、`formatting` 必须是 **`TextEdit[]` 本身**而不是任何包装、
> `SelectionRange[]` 只能有 `range`/`parent` 两个键……），
> 形状一旦跑偏当场失败。`WorkspaceEdit` 用哪个形状也**由客户端能力决定**：
> 客户端声明了 `workspace.workspaceEdit.documentChanges` 就发 `documentChanges`（带版本号），
> 没声明就回退到 `changes`——规范原文：「若客户端既不支持 `documentChanges` 也不支持
> `workspace.workspaceEdit.resourceOperations`，那么**只有 `changes` 里的纯 TextEdit
> 是被支持的**」。

#### 28.7.2 v1.10：诊断一体化 / 增量同步 / 文件监视 / 文档内高亮

> 这一轮不动语义、不动索引器、不动 ABI，只把「LSP 与 CLI 各说各话」和
> 「协议面还缺的几块」补齐。四项都只改 `src/lsp.rs` / `src/diagnostic.rs`，
> 三个冒烟脚本（`tests/lsp_smoke.py`、`tests/lsp_features.py`、
> `tools/lsp_full_smoke.py`）同步加断言。

| 项 | 做法 | 为什么 |
| --- | --- | --- |
| **诊断码表同源** | `src/diagnostic.rs::stable_code(msg, fallback)` 公开；CLI 的 `report()` 与 LSP 的 `diagnostic()` 共用。错误自带具体码优先，否则按消息阶段推断，最后回退到调用方给的阶段码 | 同一个错误在 `dialc check` 的 stderr 与编辑器红线里必须是同一个 `code`；两份手写码表必然漂移 |
| `codeDescription.href` | 每条 `publishDiagnostics` 带指向仓库 README 的 `href` | LSP 3.16+ 允许给码附文档链接，编辑器「查看错误说明」有落点 |
| **增量同步** | `textDocumentSync.change` 由 `1` → `2`；`didChange` 逐条应用 `contentChanges`，带 `range` 的用 `index::utf16_to_char_col()` 把 UTF-16 列换回 Rust char 列后按字节打补丁；不带 `range` 的整篇替换 | 大文件只传差异；同时保留对老客户端 / 冒烟脚本发全量文本的兼容 |
| **文件监视** | `workspace/didChangeWatchedFiles` 收到后，对**全部**已打开文档重跑 `index_document()` + `publish_diagnostics()` | 包 / 本地模块在磁盘上被改动后，编辑器里的红线要跟着刷新（客户端自带 watcher 即可，服务端不擅自注册全局 watcher） |
| **文档内高亮** | 新增 `textDocument/documentHighlight`（无条件广播能力）：光标下标识符经 `index::identifier_at()` 取名，再扫编译器词法器的全部 `Ident` token 出同名 range（`kind=1`） | 纯本地词法扫描、零跨文件依赖，边界与编译一致；「宁可不标，也不标错」的纪律不变 |

**边界不变**：诊断仍只在 `didOpen` / `didSave` / `didChangeWatchedFiles` 发布（打字过程不刷屏）；
stdout 协议流纯净性铁律不变（`check_lsp_stdout_purity.py` 静态守卫 +
`lsp_full_smoke.py` 动态断言）；`$/cancelRequest` 仍是「接受但不打断」（请求都是微秒级）；
跨工作区**全量**引用图、递归包内 `src/` 模块索引仍属已知边界（§28.7.1 / §38.14）。

**回归**：三个 LSP 冒烟脚本共新增 7 条断言（增量补丁生效与定位、码表同源、
`codeDescription`、documentHighlight 命中 2 处、文件监视重诊、能力声明），
全量 `bash tests/run_tests.sh` 实测 **612 ✅ / 0 ❌ / 三十组**。

---

---

## 29. v1.00：语言特性补齐

v1.00 是**语言特性补齐**里程碑。用户最初的诉求很直接：语言特性太少，很多正常程序
需要的能力此前只能「模拟」或「绕路实现」。本版一次性补齐五组特性，覆盖运算符、
控制流、函数、表达力与容器。

**所有改动共同守住项目铁律（v1.00 当时）**：不引入 GC；不提供裸指针、地址取值、指针算术；
不支持运算符重载；不支持一等函数 / 闭包 / 函数指针；ARC 所有权语义不退化；
**零隐式转换铁律保持**——`1 + 2.0` 仍然编译失败，`float` 与 `int` 之间必须显式
`int_to_float` / `float_to_int`。

> 后续边界变更：「不提供裸指针」已在 **v1.02** 解除（§31），「不支持函数指针」已在
> **v1.03** 解除（§32）；**闭包至今仍不提供**，回调状态一律显式传 `rawptr<void>`。

### 29.1 运算符与字面量

| 语法 | 语义与约束 |
| --- | --- |
| `x += e`、`-=`、`*=`、`/=`、`%=` | 编译期脱糖为 `x = x op e`；`string += string` 生成新对象（自建临时对象按 ARC 结算） |
| `x++`、`x--` | 仅 `int` 变量，脱糖为赋值 |
| `&`、`\|`、`^`、`~`、`<<`、`>>` | 位运算，仅 `int`；`~` 为一元按位取反 |
| `0xFF` / `0b1010` / `0o17` | 十六进制 / 二进制 / 八进制整数字面量 |
| `1_000_000` | 数字分隔符 `_`，只影响词法可读性，不进入值 |
| `cond ? a : b` | `cond` 必须是 `bool`；`a` 与 `b` 类型必须相同 |

位运算是 `int` 专属：`1.5 & 1` 在编译期被拒绝。编译期脱糖不改变 LLVM 函数签名，
也不引入任何新的运行时状态。

### 29.2 迭代与控制流

| 语法 | 语义与约束 |
| --- | --- |
| `for (i in a..b)` / `for (i in a..b step s)` | 范围 / 步长循环；`step` 必须是 `int`，字面量 `<= 0` 编译期拒绝；`continue` 跳到 `for.inc` |
| `for (v in arr)` | 数组遍历；句柄元素每轮 retain / release |
| `for (k, v in m)` | `map` 遍历；键集快照登记进函数级作用域，由出口统一释放（循环体内 `return` 也不泄漏） |
| `for (v in s)` | 集合遍历；编译期自动插入键集快照 |
| `match (x) { 1 => ...; _ => ... }` | 模式匹配；主体限 `int` / `bool` / `char` / `string`，模式类型必须一致，**必须有 `_` 通配分支** |
| `defer <stmt>;` | 延迟语句，按 LIFO 触发 |
| `do { ... } while (cond);` | 条件必须是 `bool` |

`defer` 的出口覆盖是最容易出错的一块：正常落穿、`return`、`break`、`continue`、
`throw` 都会触发，且各出口**互斥地各执行一次**。实现上 `defer_stack` / `loop_stack` /
`try_frames` 与 `scopes` 严格同步；`break` / `continue` 不 pop 编译期静态栈（否则同一循环
的后续跳转会失效），`drain_defers_from()` 只「额外发射」一份 defer 而不清空编译期帧。
`defer` 体内禁止 `break` / `continue` / `return` / `throw`（延迟执行时跳转目标可能失效）。

### 29.3 函数与内建

| 能力 | 语义与约束 |
| --- | --- |
| `fn f(a: int, b: int = 10): int` | 默认参数；带默认值的形参必须连续位于**末尾**，调用点补齐实参，LLVM 函数签名不变 |
| `print(a, b, c)` / `print_raw(...)` | variadic，至少 1 个参数；`print` 中间参数原样输出、最后一个补换行，`print_raw` 全部原样 |
| `assert(cond, "msg")` | 失败调用 `dial_assert_fail` 后 Fail-Fast 终止 |
| `panic(msg)` / `exit(code)` | 未捕获 panic / 进程退出；`panic` 前 `fflush(NULL)` 保证 stdout 不丢 |
| `min` / `max` / `abs` | `int` 与 `float` 各有分派；混合类型编译期拒绝 |
| `sqrt` / `pow` / `floor` / `ceil` / `round` | 仅 `float` → `float`；最终链接加入 `-lm` |

### 29.4 表达力增强

**字符串插值** `"n=${n}"`：词法层按花括号深度扫描 `${...}`，对内部源码二次 lex +
parse，因此插值里可以写任意表达式（含函数调用）。只允许 `int` / `float` / `bool` /
`char` / `string`，数组等编译期拒绝。代码生成走 `dial_string_new` + `dial_string_concat`
逐段拼接：自建的临时字符串及时释放，**借来的 `string` 变量绝不释放**。

**文件级常量** `const NAME: T = 字面量;`：值只允许五种字面量，标注类型必须与值类型
完全一致；编译期内联为字面量，因此不占运行时存储。禁止用常量名作变量 / 形参 /
循环变量名，也禁止对其重新赋值——否则会出现「声明了局部变量却静默拿到常量值」的歧义。

### 29.5 重型类型与容器

#### `enum`

`enum Color { Red, Green, Blue }` / `enum Http { Ok = 200, NotFound = 404 }`。
int-backed 名义类型，默认值从 0 递增，显式值必须是整数字面量。变体 `Color.Blue`
在 parser 内联为 `Expr::IntLit`，零运行时开销。

#### 数组切片

`var b: int[] = a[1..4];` 取半开区间 `[from, to)`，返回同元素类型的新数组。
越界在运行时 Fail-Fast。句柄元素（`string[]` 等）通过 `dial_array_push()` 逐元素
retain，切片与原数组**不共享底层 buffer**。

#### `set<int>`

`new_set()` 创建，`add` / `contains` / `remove` / `size` / `values` 操作。
底层复用 int→int map 的键集（value 恒为 1），因此零 C 改动；`add` 返回可能更新后的
容器指针，由语句级代码生成写回接收变量（COW 契约）。`for (v in s)` 直接迭代。

#### 泛型 `map<K,V>`

```dial
var m: map<string, int> = new_kv_map();
m.put("alice", 90);
print(m.get("alice"));          // 90
var ks: string[] = m.keys();    // 键快照
```

| 维度 | 支持范围 |
| --- | --- |
| 键 `K` | `int` / `float` / `bool` / `char` / `byte` / `string` |
| 值 `V` | `int` / `float` / `bool` / `char` / `byte`（标量） |
| 方法 | `put(k,v)` / `get(k)` / `contains(k)` / `remove(k)` / `size()` / `keys()` / `values()` |
| 未命中 | 返回 `V` 的默认值（`int` / `char` / `byte` → `-1`，`float` → `-1.0`，`bool` → `false`） |

设计要点：

- **独立运行时**：`stdlib/dial_kvmap.c` 与老 `map`（int→int）完全隔离，老路径一个字节不改。
  `new_kv_map()` 的 K/V 由变量声明的类型标注决定——parser 在 `parse_var_decl()` 里把两个
  kind 常量补进实参，语义层据此还原 `Type::KvMap(K,V)`；源码永远只写 `new_kv_map()`。
- **零隐式转换**：`put` 的键 / 值必须**精确**匹配 `K` / `V`；`map<string,int>.put(1, 2)` 编译失败。
  `float` 以 IEEE754 **位模式**进出 C 运行时（bitcast，绝不 `fptosi` 丢小数）。
- **ARC**：`string` 键按条目持有引用；COW 克隆 / 扩容重建时新 buffer 逐条目 retain，
  最后一个持有者离开时逐条目 release 后回收 buffer。`remove` 释放被删条目的键引用。
- **删除用墓碑**：开放寻址表删除若「左移紧凑」，会把哈希位置靠后的条目挪出探测链，
  令其永远查不到。2 万键压测中删除 1 万个后 `size` 曾只剩 13082（应为 10000）；
  现统一改为墓碑标记 + 负载过高时重建（老 `dial_map.c` 同步修复，`set<T>` 与
  `for (k, v in m)` 一并受益）。
- **已知边界**：`V` 暂不开放 `string`（`get` 未命中时的默认 string 对象会带来所有权
  移交歧义）；`map.values()` 当前只支持 `V = int`。
  （`for (k, v in m)` 对泛型 map 的限制已在 **v1.01 解除**，见 [§30.7](#307-泛型-mapkv-支持-for-k-v-in-m)。）

### 29.6 实现落点

| 层 | 改动 |
| --- | --- |
| `src/lexer.rs` | 新运算符 / 关键字 / `Arrow` / `InterpLit`；进制字面量与数字分隔符 |
| `src/parser.rs` | 复合赋值与 `++` / `--` 脱糖、`ForEach` / `Match` / `Defer` / `DoWhile`、`const` / `enum` 内联、默认参数、插值二次解析、`map<K,V>` / `set<T>` 类型、`[from..to]` 切片 |
| `src/ast.rs` | `BinOp` 位运算、`UnOp::BitNot`、`Expr::Ternary` / `Interp` / `Slice`、`Type::Set` / `Type::KvMap`、新 `Stmt` 变体、`Func::defaults` |
| `src/semantic.rs` | 类型检查、方法签名、内建 variadic / 数学分派、保留名、`defer` 跳转约束、`const` 遮蔽、类型良构性 |
| `src/codegen.rs` | LLVM IR、ARC retain / release、defer 栈、`gen_builtin_v1()`、`gen_interp()`、默认参数补齐、切片 / 集合 / 泛型 map 分派 |
| `src/lsp.rs` / `src/index.rs` | 新 token 的语义分类、固定文本与符号索引 |
| `stdlib/` / `runtime/` | `dial_array_slice`、`dial_map_keys`、`dial_bool_to_string`、新 `dial_kvmap_*`、trap 接口 |
| `src/main.rs` | 外部函数签名补齐、最终链接 `-lm` |

### 29.7 测试

- 8 个正向示例 + 同名 `.expected` 黄金文件（A 组逐字节比对）；
- 30+ 个 `tests/bad/*.dial` 负向用例锁定语义边界（零隐式转换、`match` 必须有通配、
  `defer` 禁止跳转、`const` 遮蔽、`set` / `map<K,V>` 的类型约束等）；
- `bash tests/run_tests.sh` A–O 全套通过，含 LSP 协议流纯净性、内存上界与所有权回归；
- 2 万键 ARC 压测：泛型 map 峰值约 4.4 MB、`set<int>` 约 2.4 MB，无泄漏、无 Fail-Fast。

---

## 30. v1.01：按「真实程序」盘点后的缺口补齐

v1.00 补齐了五组语言特性之后，下一步不是继续「凭想象加特性」，而是**照着程序员写
真实程序的方式，把编译器拿出来跑**：先写 30 多个真实小程序，逐个看哪些能直接写、
哪些要绕道、哪些只能模拟、哪些根本写不出来。盘点的结论集中在一张表里：

| 缺口 | 此前状态 | v1.01 落点 |
| --- | --- | --- |
| 数组只能装 `int` | `float[]` / `byte[]` / `bool[]` / `char[]` 全部写不出来 | §30.1 |
| 空数组写不出来 | `var a = []` 无法推断元素类型 | §30.2 |
| 数组删元素只能绕道 | 只能重建整个数组 | §30.3 |
| 提前释放资源要绕道 | 只能靠嵌套块缩小作用域 | §30.4 |
| 字符串只能取子串 | `split` / `replace` / `trim` / `index_of` 全缺 | §30.5 |
| 拿不到命令行参数 / 时间 / 随机数 | 完全没有对应内建 | §30.6 |
| 泛型 map 只能拿 `keys()` 再循环 | `for (k, v in m)` 对 `map<K,V>` 不可用 | §30.7 |
| 数组排序要手写 | 只能自己写 O(n²) 冒泡 | §30.8 |

盘点中最刺眼的一条是**资源释放**：此前想提前释放一个大数组，只能把它塞进一层
嵌套块里靠作用域结束触发释放——实测峰值内存 **57160 KB**；改成 `drop()` 显式
释放后，同样的三阶段程序峰值降到 **33344 KB（-41.7%）**，与嵌套块绕道
（33460 KB）持平，却不必再套一层块、也不影响阶段间传数据。

本节所有改动继续守住项目铁律（v1.01 当时）：不引入 GC；不提供裸指针 / 地址取值 / 指针算术；
不支持运算符重载；不支持一等函数 / 闭包；ARC 所有权语义不退化；
**零隐式转换铁律保持**（`1 + 2.0` 仍然编译失败）。

> 其中「不提供裸指针」已在 v1.02 解除（§31），「不支持一等函数」中的**函数指针**
> 已在 v1.03 解除（§32）；闭包仍不提供。

### 30.1 数组元素类型扩展

数组元素类型白名单从「只有 `int`」放宽到**全部标量 + 句柄**：

| 元素类型 | 声明 | C 侧存储 |
| --- | --- | --- |
| `int` | `int[]` | `int64` 槽（原有） |
| `float` | `float[]` | `int64` 槽，存 **IEEE754 位模式** |
| `byte` | `byte[]` | `int64` 槽（0~255，语义同 `byte`） |
| `char` | `char[]` | `int64` 槽（码点，语义同 `char`） |
| `bool` | `bool[]` | `int64` 槽，存 `i1` 扩展后的 0/1 |
| `string` 等句柄 | `string[]` | `int64` 槽存指针，按元素 ARC |

**float 必须存位模式，不能 fptosi**：这是本节最容易踩的坑。C 侧数组槽统一是
`int64`，若把 `3.25` 用 `fptosi` 转过去，小数部分会被截断成 `3`——写进去的是
另一个数。编译器侧因此统一走 `to_elem_bits` / `from_elem_bits` 这一对转换：
`float` 走 LLVM `bitcast` 保留 IEEE754 位模式，`bool` 走 `i1 ↔ i64`，句柄走
`ptr_to_int` / `int_to_ptr`，`int` / `byte` / `char` 本就是 `i64`。

零隐式转换铁律**不因元素类型放宽而松动**：`float[]` 里 `push(3)`（`int` 字面量）
依然编译失败，必须写 `push(3.0)`。

### 30.2 空数组字面量 `[]`

`var a: int[] = [];` 现在合法。此前 `[]` 无法推断元素类型，只能报错；现在元素类型
由**变量的类型标注**提供。裸写 `var a = [];` 仍然编译失败——编译器不会替你猜。

### 30.3 数组删除元素 `remove(i)` / `pop()`

| 方法 | 返回 | 说明 |
| --- | --- | --- |
| `a.remove(i)` | 被删元素（元素类型） | 删除下标 `i` 处的元素；越界当场 Fail-Fast |
| `a.pop()` | 被弹出的末元素 | 空数组 `pop` 当场 Fail-Fast |

数组自身的变化（COW 可能更换指针）由编译器在接收变量处**写回**；被删元素的
所有权随返回值移交给调用者，句柄类元素会额外 `retain` 一份。

### 30.4 `drop()`：显式提前释放

```dial
var buf: int[] = [1, 2, 3];
drop(buf);          // 立刻释放，不必等到作用域结束
print(buf.length()); // 编译期报错：变量 'buf' 已转移，无法使用
```

| 被 drop 的类型 | 行为 |
| --- | --- |
| `string` / `bytes` / `array` / `map` / `map<K,V>` / `set` / `error` / `channel` | 调用对应 `*_release`；引用计数归零则当场回收 |
| `file` | `dial_file_close`（`fclose`） |
| `thread` | `dial_thread_join` |
| `int` / `float` / `bool` / `byte` / `char` / 结构体 | **no-op**（值类型没有所有权），drop 后变量仍可继续使用 |

句柄类变量被 `drop` 后进入「已转移」状态：此后任何使用都在**编译期**被
use-after-move 诊断拦下（「变量 'x' 已转移，无法使用」），杜绝 use-after-free。
标量是值类型，drop 不改变所有权，因此**不**标记 moved——否则会误伤后续的合法使用。

### 30.5 字符串方法补齐

| 方法 | 返回 | 说明 |
| --- | --- | --- |
| `s.split(sep)` | `string[]` | 按分隔符切分；`sep` 为空串时整串作为唯一元素；找不到分隔符时返回单元素数组 |
| `s.replace(from, to)` | `string` | 替换全部出现，返回**新**字符串（原串不变） |
| `s.trim()` | `string` | 去掉首尾空白（空格 / 制表 / 换行 / 回车） |
| `s.index_of(sub)` | `int` | 首次出现的字节偏移；未找到返回 `-1` |

### 30.6 环境 / 时间 / 随机内建

| 内建 | 返回 | 说明 |
| --- | --- | --- |
| `args()` | `string[]` | 命令行参数（`argv[1..]`）；每次调用返回一份独立副本 |
| `now()` | `float` | 自 Unix 纪元起的秒数（含小数） |
| `sleep(ms)` | void | 休眠毫秒数 |
| `random()` | `int` | `[0, 2^63)` 的伪随机整数 |

`args()` 需要拿到 OS 传进来的 `argv`，因此编译器生成的 `main` 入口改为接收
`argc` / `argv`（C ABI 的 `int main(int, char**)`），并在入口处调用
`dial_env_init(argc, argv)` 把参数捕获进运行时——对用户代码完全透明，
既有程序不受影响。

### 30.7 泛型 `map<K,V>` 支持 `for (k, v in m)`

v1.00 的泛型 map 只能 `keys()` / `values()` 拿快照再配 `for (v in arr)` 迭代，
绕了一圈。现在 `for (k, v in m)` 对 `map<K,V>` 直接可用，且**键 / 值按真实类型
绑定**：

```dial
var m: map<string, int> = new_kv_map();
m.put("a", 10);
for (k, v in m) { print(k); print(v); }   // k 是 string，v 是 int
```

所有权口径：循环里的 `k` / `v` 都是**借来**的（键借自键快照数组、值借自表本身，
均未 `retain`），因此循环末尾与函数出口**不释放**它们——否则会重复释放表内元素。

### 30.8 数组排序 `sort()`

```dial
var a: int[] = [5, 3, 9, 1, 7];
a.sort();          // 升序，就地排序
```

- 只支持**可比较的标量元素**：`int` / `float` / `byte` / `char` / `bool`；
  `string[]` 等句柄元素无自然序，编译期拒绝；
- `float` 按 **IEEE754 数值序**比较（负数正确排在最前），绝非位模式比较；
- 遵守 COW 契约：数组被多者共享时先克隆再排序，**不影响其他持有者**；
  排序后的指针写回接收变量。

### 30.9 实现落点

| 层 | 改动 |
| --- | --- |
| `src/ast.rs` | `ArrayLit` 增加「期望元素类型」参数（供空数组推断） |
| `src/parser.rs` | `parse_var_decl` 在已知数组类型时把期望元素类型注入 `[]` |
| `src/semantic.rs` | 元素类型白名单扩容；`string_method_type` / `array_method_type` 新方法；`args` / `now` / `sleep` / `random` 内建类型；`drop` 保留名与消费后禁用；`is_copy_type` 判定值类型 |
| `src/codegen.rs` | `to_elem_bits` / `from_elem_bits` 位模式转换；`dial_array_sort` / `dial_string_split` 等分派；`drop` 按类型分派；`main` 入口追加 `dial_env_init`；泛型 map 的 `for (k,v in m)` 双形态分派 |
| `stdlib/dial_array.c` | `dial_array_remove` / `dial_array_pop` / `dial_array_sort`（COW 感知） |
| `stdlib/dial_string.c` | `dial_string_split` / `dial_string_replace` / `dial_string_trim` / `dial_string_index_of` |
| `stdlib/dial_env.c` | 新增：`dial_env_init` / `dial_args` / `dial_now` / `dial_random` / `dial_sleep` |
| `src/package.rs` | `STDLIB_ABI` 由 1 递增到 **2**（见下方「标准库 ABI 递增」） |
| `tools/` / `.github/workflows/ci.yml` | 标准库包版本 `1.0.0 → 1.1.0`、打包 `--abi 2`、一键安装的 `STDLIB_RELEASE` → `stdlib-v1.1.0` |

### 30.9.1 标准库 ABI 递增（破坏性变更）

新增 `dial_env.c` 意味着标准库**多导出**了一批符号，而编译器生成的 `main` 入口
会**主动调用** `dial_env_init`。于是旧标准库包（`abi=1`）链接新编译器时，必然报出
难懂的链接错误：

```
ld.lld: error: undefined symbol: dial_env_init
```

这属于**破坏性 ABI 变更**，因此把 `STDLIB_ABI` 由 1 递增到 **2**。递增后，旧包在
**链接期**就被明确拦下，并给出重装指引（而不是让用户去追一个符号错误）：

```
已安装的标准库 'stdlib' ABI 不兼容：包 abi=1，编译器要求 abi=2。
为避免不兼容的二进制悄悄链入程序，已中断链接。
请先卸载旧包再安装匹配版本：
  rm -rf ~/.dial/packages/stdlib
```

配套同步：官方标准库包版本 `1.0.0 → 1.1.0`，一键安装脚本的 `STDLIB_RELEASE`
默认标签 `stdlib-v1.0.0 → stdlib-v1.1.0`。标准库当时**独立发版**（tag `stdlib-v*`），
因此本次发布需要**两个标签**：`v1.1.0`（编译器）与 `stdlib-v1.1.0`（标准库包）。

> 该协同方式后续已调整：标准库包现随每个 `v*` Release 同页发布，安装脚本跟随 `latest`，
> 见 [§25.10](#2510-官方标准库-stdlibdialpkg独立发布)。

### 30.10 测试

- 6 个新增正向示例 + 同名 `.expected` 黄金文件：`array_demo`（元素类型 / 空数组 /
  `remove` / `pop`）、`drop_demo`、`string_methods_demo`、`env_demo`、
  `kvmap_foreach_demo`、`sort_demo`；
- 4 个新增负向用例：`array_empty_no_annot`、`array_push_wrong_elem`、
  `drop_use_after_move`、`string_split_bad_arg`、`array_sort_handle_elem`；
- `bash tests/run_tests.sh` A–O 全套通过。

---

## 31. v1.02：goto / with / 裸指针

本版补三样东西，它们互为因果：

- **`goto` + 标签**：底层清理的利器，但无限制地乱跳会把程序打崩——因此配三条**死锁**（§31.2）；
- **`with` 资源作用域块**：`goto` 不得跳出 with 块（否则自动释放被绕过），所以先有 with（§31.1）；
- **裸指针 `rawptr<T>`**：**不受 ARC 管束**，谁申请谁释放（§31.4）；
  包的不透明句柄（`Type::Opaque`）同属这一类，本版补齐其契约与诊断（§31.4.1）。

三样都守住既有铁律：不引入 GC；ARC 所有权语义不退化；零隐式转换（裸指针与 ARC 容器之间**没有**隐式转换）。

### 31.1 `with` 资源作用域块

```dial
with open(path, "r") as f {      // 也可写成 with f = open(path, "r") { ... }
    var content: string = read(f, 100);
    print(content);
}                                 // ← 离开块时自动 close(f)
```

| 资源类型 | 块末自动释放 |
| --- | --- |
| `file` | `close` |
| `thread` | `join` |
| 裸指针 `rawptr<T>` | `raw_free` |

| 性质 | 说明 |
| --- | --- |
| 覆盖全部出口 | 正常落穿 / `return` / `break` / `continue` / `throw` 都会触发，且**互斥地各执行一次**——因为它直接复用既有的 `defer` 机制，而不是另发明一条出口路径 |
| 不二次释放 | 块内若已显式 `close(f)` / `join(t)` / `raw_free(p)`，块末**跳过**（二次 `fclose` 是 UB） |
| 适用面 | 只管「需要显式释放」的资源；`string` / `array` / `map` 等 ARC 句柄本就随作用域自动释放，用 `with` 是多余的（编译期直接拒绝） |

### 31.2 `goto` 与标签：三条死锁

```dial
label done:
    ...
goto done;
```

`goto` 允许**向前**与**向后**跳，但被三条死锁圈住——它们不是「建议」，而是**编译期硬拦**：

| # | 死锁 | 为什么必须有 | 诊断 |
| --- | --- | --- | --- |
| ① | **禁止跨过变量初始化**（Zig 式严格：两个方向都禁） | C 允许 `goto` 跳过 `let x = 5;`，之后用到 `x` 就是 UB；向后跳则重复初始化，前一份（ARC 句柄）泄漏 | `goto 不得跨过变量初始化` |
| ② | **禁止跳出 / 跳进 `with` 块** | with 靠作用域结束自动释放资源，跨界跳转会**绕过**该释放（泄漏）或**重复**它（二次 fclose = UB） | `goto 不得跳出或跳进 with 块` |
| ③ | **禁止跨越 `try` 边界** | try 靠 `setjmp` / `longjmp` 保存栈帧，跨界跳转会直接毁掉 `jmp_buf`；**不同 try 块之间互跳**同样禁止 | `goto 不得跨越 try 边界` |

另外三条边界一并由编译期拦下：

- 目标标签未定义 → `goto 目标标签 'x' 未定义`；
- 同一函数内标签重复定义 → `标签 'x' 重复定义`；
- **跳进嵌套块**（目标标签在一个更深的块里）→ `不在当前作用域内`——那里面的变量尚未初始化。

约束 ① 的判定用**全局深度优先语句序号**，而不是「同一块内的语句下标」。这点很关键：
「从内层块跳到外层标签」同样会跳过外层块中位于跳转点与标签之间的变量声明——
实测那种写法会让 `print(x)` 读到未初始化的栈槽（打印出一个随机的栈上垃圾值），
属于真正的 UB。只比同块下标会漏掉它，因此跨块 / 同块、向前 / 向后统一用全局序号判定。

**跳转的语义**：跳到**外层块**的标签时，被跨越的那几层作用域会**照常清理**——`defer` 按 LIFO 触发、ARC 句柄逐个 release。这正是 `goto` 作为「底层清理利器」的价值所在。

```dial
while (i < 10) {
    var j: int = 0;
    while (j < 10) {
        if (i * 10 + j == 47) { goto done; }   // 一次跳出两层
        j += 1;
    }
    i += 1;
}
label done:
```

### 31.3 `break` / `continue` 的释放修复（顺带修好的既有缺陷）

实现 `goto` 时发现：**`break` / `continue` 提前离开了循环体，却没有释放循环体的局部**——
外层臂的 `release_scope_locals` 位于落穿路径上，而 break/continue 恰恰**走不到**那条路径。
实测 `continue` 循环 20 万轮（每轮一个 `string`）峰值 **13892 KB**，修正后 **1620 KB**。

修法与 `goto` 同源：在跳转前补发一份「被跳出作用域」的释放代码。两条路径互斥，
各自发一份是安全的（不会重复释放）。这条修复让 `break` / `continue` / `goto` 三者
的清理语义终于一致。

### 31.4 裸指针：不受 ARC 管束的显式契约

```dial
var buf: rawptr<byte> = raw_alloc(64);          // 申请（不受 ARC 管）
var ip: rawptr<int> = ptr_cast<rawptr<int>>(buf); // 显式转换（无隐式转换）
*ip = 1234;                                       // 解引用写入
print(*ip);
var next: rawptr<int> = ip.offset(1);             // 显式带类型的指针算术
*next = 5678;
raw_free(buf);                                    // 释放（谁申请、谁释放）
```

| 规则 | 说明 |
| --- | --- |
| **绝对不受 ARC 管理** | 编译器**不**为裸指针 retain / release，作用域结束时**也不**自动释放；`drop(p)` 对裸指针的唯一作用就是 `free` |
| **无隐式转换** | 裸指针 ↔ ARC 容器、裸指针 ↔ 裸指针都**没有**隐式转换：取 `string` 的字节缓冲区必须显式 `ptr_cast<rawptr<byte>>(s)`，裸指针换视图必须显式 `ptr_cast<rawptr<int>>(p)` |
| **借用不接管** | `ptr_cast` 从 `string` / `bytes` 取到的是**数据缓冲区**的借用；原对象仍受 ARC 管理，原对象释放后该指针即悬垂——**这是使用者的责任** |
| **指针算术显式且带类型** | 只有 `p.offset(n)` 一种算术，步长由元素类型决定（`rawptr<int>` → 8 字节 / 元素）。`rawptr<void>` **没有元素大小**，因此**禁止**做算术（须先 `ptr_cast` 成具体类型） |
| **解引用宽度按元素** | `byte` / `bool` 各 1 字节、`char` 4 字节（UTF-8 码点上限）、`int` / `float` / 句柄 8 字节 |
| **`&x` 的边界** | 只允许对**可变的栈上值类型**变量取址（`int` / `float` / `bool` / `byte` / `char` / `struct`）。对 `let` 取址会绕过不可变约束 → 拒绝；对堆对象取址拿到的只是槽位地址 → 拒绝 |

`rawptr<void>` 是「无类型裸指针」：只能传递与释放，不能解引用、不能算术。

### 31.4.1 不透明指针（包句柄）：同样不受 ARC 管束

「不受自动管束」的不止裸指针。包在 `.diali` 里声明的**不透明句柄**
（`type Db;` → 类型 `Type::Opaque`，见 [§25.9](#259-用法import-与-dialc-installv093-已实现v096-支持-url)）
同样**不受 ARC 管理**。v1.02 沿用这一既有类型（不新增语法、不新增运行时），
把它的契约与诊断补齐——用户要求的「这两个不受自动管束、需要手动释放、明确说明」，
另一半就在这里。

| 类别 | 例子 | 谁负责释放 | 编译器是否 retain / release |
| --- | --- | --- | --- |
| ARC 句柄 | `string` / `int[]` / `map` / `error` | 编译器（作用域 / COW / move） | ✅ |
| **包的不透明句柄** | `type Db;` 的 `Db` | **包自己**（如 `db_close(db)`） | ❌ |
| **裸指针** | `rawptr<T>` | **使用者**（`raw_free(p)` / `drop(p)`） | ❌ |

具体契约：

- **手动释放**：编译器不 retain、不 release，作用域结束也不释放；必须由包提供、
  由调用方显式调用 `close` / `free` 一类函数（如 §25.10 示例里的 `db_close(db)`）。
- **`drop(db)` 编译期报错**：此前它是个**静默 no-op**，还会把变量误标为「已转移」，
  让人误以为资源已经归还。v1.02 起直接拦下并给出指引：
  ```
  语义错误：不透明句柄 'Db' 不受 ARC 管理，drop() 不会释放它；
  请调用该包提供的显式释放函数（如 db_close(db)）手动释放
  ```
- **不与 ARC 容器隐式互转**：`Db` 是独立类型，赋给 `string` / `rawptr<T>` 等一律
  编译失败；取它的内存视图必须走包自己的接口（零隐式转换铁律）。
- **不可打印**：`print(db)` / `print_raw(db)` 已有专门诊断，提示改用包提供的取值函数。
- **与裸指针的区别**：不透明句柄是**具名、无布局、无操作**的句柄——不能解引用、
  不能做指针算术；`rawptr<T>` 是带元素类型的地址，可 `*p` 读写、可 `p.offset(n)`。
- **放进容器时的责任**：数组 / 通道按 8 字节句柄传递它，并按 RC 元素类别做
  retain / release（见 [§11.6](#116-多维数组与句柄元素数组)），因此**包作者须保证
  句柄布局与 `dial_rc_*` 兼容**；不满足时不要把句柄塞进容器，改由包自己管理生命周期。

### 31.5 实现落点

| 层 | 改动 |
| --- | --- |
| `src/lexer.rs` | 新关键字 `with` / `goto` / `label` |
| `src/ast.rs` | `Stmt::With` / `Stmt::Label` / `Stmt::Goto` / `Stmt::AssignDeref`；`Type::RawPtr(Box<Type>)`；`UnOp::Deref` / `UnOp::AddrOf`；`Expr::TypeLit`（类型实参） |
| `src/parser.rs` | `with` / `goto` / `label` 语法；`rawptr<T>` 类型；`*p` / `&x` 一元运算；`*p = v` 赋值；`ptr_cast<T>(x)` 类型实参；**`>>` 在解析层拆成两个 `>`**（泛型闭合） |
| `src/semantic.rs` | `analyze_gotos()`：标签 / 跳转点收集 + 三条死锁校验（约束 ① 用全局深度优先序号）；`rawptr` 的类型推导与内建；`&x` 的可变性约束；`with` 的资源类型约束；`drop(不透明句柄)` 的明确诊断 |
| `src/codegen.rs` | `with` 的资源登记（走 defer 机制）；`goto` 的目标块预分配与跨作用域清理；`drop` / `__with_release` 按类型分派；裸指针的 load / store / gep；`break` / `continue` 的释放补齐 |
| `stdlib/dial_raw.c` | `dial_raw_alloc`（分配失败 Fail-Fast）/ `dial_raw_free`（NULL 安全） |
| `stdlib/dial_env.c` | v1.01 已有（`args` / `now` / `sleep` / `random`） |
| `src/package.rs` | `STDLIB_ABI` 由 2 递增到 **3**（见下方 §31.5.1） |
| `tools/` / `.github/workflows/ci.yml` | 标准库包版本 `1.1.0 → 1.2.0`、打包 `--abi 3`、一键安装的 `STDLIB_RELEASE` → `stdlib-v1.2.0` |

### 31.5.1 标准库 ABI 递增（破坏性变更）

新增 `stdlib/dial_raw.c` 意味着标准库又多导出了一批符号，编译器在**用户写了裸指针
程序时**会调用它们。装着 abi=2 旧包的编译器链接这类程序时，会报出难懂的链接错误：

```
ld.lld: error: undefined symbol: dial_raw_alloc
```

与 v1.01 的 `dial_env_init` 不同，这两个符号是**条件引用**（只有用到裸指针才产生调用），
但判据始终是「**编译器是否可能要求旧包没有的符号**」，而不是「是否每个程序都引用」。
因此把 `STDLIB_ABI` 由 2 递增到 **3**，让旧包在**链接期**就被明确拦下并给出重装指引：

```
已安装的标准库 'stdlib' ABI 不兼容：包 abi=2，编译器要求 abi=3。
为避免不兼容的二进制悄悄链入程序，已中断链接。
请先卸载旧包再安装匹配版本：
  rm -rf ~/.dial/packages/stdlib
```

`build_stdlib.sh` 与编译器的 LTO 路径都用 `*.c` / `read_dir` 收集源文件，
`dial_raw.c` 会**自动**进入静态库与官方包，无需手工登记清单。

配套同步：官方标准库包版本 `1.1.0 → 1.2.0`，打包默认 `--abi 3`，CI 发布作业
`--abi 3`，一键安装脚本当时把 `STDLIB_RELEASE` 默认标签改为 `stdlib-v1.2.0`。
v1.01 ~ v1.03 的提交已随 `main` 推送；按版本号的发布标签曾以两个为准
（`v1.3.0` 编译器与 `stdlib-v1.3.0` 标准库包，见 [§32.5.1](#3251-标准库-abi-递增破坏性变更)）。

> **发布方式后续已调整**：标准库包现与 zip / vsix 同处一个 `v*` Release 页面，
> 安装脚本默认跟随 `releases/latest`，不再需要上述两个标签；
> 详见 [§25.10](#2510-官方标准库-stdlibdialpkg独立发布)。

### 31.6 测试

- 3 个正向示例 + 同名 `.expected` 黄金文件：`goto_demo`、`with_demo`、`rawptr_demo`；
- 12 个负向用例锁定边界：跨初始化（同块 + **跨块**）/ 跳出 with / 跨 try /
  跳进嵌套块 / 未定义标签 / 对 `let` 取址 / 解引用非指针 / `rawptr<void>` 算术 /
  `free` 非指针 / `with` 非资源 / 裸指针隐式转换；
- `bash tests/run_tests.sh` A–O 全套通过；H 组另加一条回归：`drop(不透明句柄)`
  必须报错并给出「不受 ARC 管理、请手动释放」的指引。

---

## 32. v1.03：回调（函数指针）与可变参数

v1.03 补齐两个「正常程序绕不过去」的能力：**把函数当值传递**、**写同类型变参函数**。
两者都按 Dial 的既有铁律落地，没有引入任何运行期魔法：

- **不引入 GC**；ARC 所有权语义不退化；
- **不引入闭包**：函数指针里只有一个代码地址，绝不隐式捕获环境；
- **零隐式转换**：函数签名与变参元素类型都严格匹配（`1 + 2.0` 仍编译失败）；
- **零隐藏开销**：变参脱糖成普通 `T[]`，没有类型标签、没有装箱、没有运行期分派。

### 32.1 函数指针 `fn(A, B): R`

| 项 | 契约 |
| --- | --- |
| 类型写法 | `fn(int, int): int`、`: void`、`fn(int, rawptr<void>): int` |
| 底层表示 | LLVM `ptr`（纯函数地址），与 `rawptr<T>` 一样是不透明指针 |
| 求值 | 具名函数出现在右值位置即求值为函数地址：`var cb: fn(int): int = double;` |
| 调用 | `cb(21)` 走**精确签名的间接调用**，参数个数 / 类型编译期校验 |
| 捕获 | **绝对没有**。没有环境对象、没有 `Fn` / `FnMut` / `FnOnce` 分类、没有匿名函数语法 |
| 所有权 | 不受 ARC 管理、无堆分配、**无需释放**；`drop(cb)`、`with cb as p` 都是编译期错误 |
| 状态传递 | 显式把 `rawptr<void>` 作为回调形参，由调用方传进去，生命周期用户自负 |

`fnptr_demo.dial` 覆盖声明 / 赋值 / 传参 / 返回 / 间接调用；`callback_state_demo.dial`
演示「回调写回调用方内存」：函数指针 + 变参 + 显式 `rawptr<void>` 状态 + 最后手动 `raw_free`。

> **为什么不做闭包**：闭包需要分配捕获环境并决定它的生命周期，那会与 ARC、`with`、
> `goto` 的作用域清理全面纠缠。Dial 选择把状态的所有权明明白白交给调用方——
> 与 C 的 `void*` 上下文参数同款，也和 v1.02 的裸指针契约一脉相承。

### 32.2 同类型变参 `...xs: T`

```dial
fn sum(...xs: int): int {
    var t: int = 0;
    var i: int = 0;
    while (i < xs.length()) { t += xs.get(i); i += 1; }
    return t;
}
print(sum(1, 2, 3, 4));  // 10
print(sum());             // 0（缺省就是空数组）
```

`...xs: T` **在 `semantic.rs` 的 `analyze` 开头被脱糖**：

1. 形参变成普通的 `xs: T[]`；
2. 调用点的尾部实参打包成带元素类型标注的 `T[]` 数组字面量；
3. 之后的类型检查、LSP、codegen 只看得见一个普通数组。

因此编译产物里**没有**变参专用的运行期机制：没有类型标签、没有装箱、没有分派表。
`...xs` 必须是参数列表最后一项，不能与默认实参共存，元素类型必须能作为数组元素。

**异构变参在语言层被拒**：`sum(1, "a")` 会在编译期报「数组元素类型不一致」并明确
提示「若需异构参数请显式定义结构体或自定义类型包」。这与零隐式转换铁律一致，
也避免为了方便引入 `any` 之类的动态类型后门。

### 32.3 C 风格异构变参（仅 `extern fn`，FFI 特例）

`.diali` 的接口声明允许写 `...`，表示按 **C varargs ABI** 直传尾部参数：

```text
extern fn demo_vprintf(fmt: string, ...): int;
```

- 固定参数照常做严格类型检查（传 `int` 给 `fmt: string` 仍编译失败）；
- 尾部参数**不检查类型**，按 C ABI 原样传递：`i64` 对应 `%ld`、`f64` 对应 `%f`、
  `bool` 会被 zext 成 `i64`（不能让 `i1` 直接进 varargs）；
- 这是**包 FFI 特例**，Dial 自己定义的函数永远不能这样写（它们走 §32.2 的同类型脱糖）；
- 不引入任何隐藏类型标签或装箱。

H 组回归同时锁定正向（int64 / double / bool 混传输出 `i=42 f=3.5 b=1`）与负向
（固定参数类型错误仍被拦下）。

### 32.4 高阶数组方法 `map` / `filter` / `sort_by`

| 方法 | 回调签名 | 返回 |
| --- | --- | --- |
| `arr.map(f[, state])` | `fn(T, rawptr<void>): U` | `U[]`（新数组） |
| `arr.filter(f[, state])` | `fn(T, rawptr<void>): bool` | `T[]`（新数组） |
| `arr.sort_by(cmp[, state])` | `fn(T, T, rawptr<void>): int` | `T[]`（原地排序，COW 安全） |

- 回调签名**固定带一个 `rawptr<void>` 状态参数**——没有闭包，状态必须显式穿过 C 运行时
  再交回回调；省略 `state` 时回调收到空指针。
- C 侧 `dial_array_hof` 只做遍历 / 建数组 / 插入排序，回调经编译器生成的**精确签名
  thunk** 间接调用：thunk 把 8 字节位模式还原成 T、把函数地址按真实签名调用、再把结果
  转回位模式。C 完全不知道回调类型，因此没有运行期类型标签。
- 仅支持标量元素（`int` / `float` / `bool` / `byte` / `char`）：句柄元素数组请自己写循环
  （C 侧新建数组时不做 ARC 所有权结算）。
- `sort_by` 用插入排序而非 `qsort`：`qsort` 的比较器拿不到 `fn_raw` / `state`，无法透传状态。

### 32.5 释放契约：谁自动、谁手动（**不要混淆**）

最容易搞错的一点：**「不受自动管束」不等于「都需要手动释放」**。

| 对象 | 是否受 ARC 管理 | 是否需要手动释放 | 依据 |
| --- | --- | --- | --- |
| 变参数组 `T[]` 及其元素 | **是** | **否** | 脱糖生成的是普通 ARC 数组：函数形参进入时 retain、退出时 release；调用点的临时数组由编译器在调用结束后插入 release |
| 函数指针 `fn(...)` | 否（纯地址值） | **否** | 没有堆对象、没有所有权，`drop()` 对它无意义，编译期直接拒绝 |
| 裸指针 `rawptr<T>` | **否** | **是** | `raw_free(p)` / `drop(p)` / `with` 自动释放，见 §31 |
| `file` / `thread` / 不透明句柄 | 否 | **是** | `close` / `join` / 包自己的 `xxx_free` |

实测：20 万次变参调用（每次生成 5 元素临时数组）峰值 RSS **1348 KB**，
证明调用方那一份临时数组确实被编译器释放了；若漏掉 release，这一项会线性吃内存。

### 32.5.1 标准库 ABI 递增（破坏性变更）

`stdlib/dial_array.c` 新增 `dial_array_hof`，因此 `STDLIB_ABI` **3 → 4**，
官方包版本 `1.2.0 → 1.3.0`，`make_stdlib_pkg.py` 默认 `--abi 4`、CI `--abi 4`；
`stdlib.dialpkg` 随每个 `v*` Release 一同发布，安装脚本默认跟随 `releases/latest`
（版本不写死）。旧包（abi=3）会被编译器当场拒绝并给出重装指引。

### 32.6 测试与发布协同

- 4 个正向示例：`fnptr_demo`、`varargs_demo`、`callback_state_demo`、`hof_demo`；
- 14 个负向用例：签名不匹配 / 间接调用参数个数 / print / drop / with / 闭包语法 /
  异构变参 / 变参不在末尾 / 变参与默认值共存 / 变参元素类型 / 高阶回调签名 /
  句柄元素 / 非函数指针 / 状态参数类型；
- H 组新增 C 变参 `extern fn` 的正向与负向回归；
- `bash tests/run_tests.sh` A–O 全套通过。

> **关于 `v1.0` 标签**：`v1.00` ~ `v1.03` 都是「v1.0 里程碑」达成过程中的分版记录
> （CHANGELOG 里已收拢到该标题下）。回调、变参等收尾特性在 `[v1.03]` 全部完成并
> 回归通过，**里程碑标签 `v1.0` 已打并推送；CI 修复完成后又删除同名标签、
> 重新指向里程碑收尾的最新提交并重推**（用于触发带 `stdlib.dialpkg` 的新 Release）。
>
> **发布与安装已与版本号解耦**：一键安装脚本不再按 `v1.3.0` / `stdlib-v1.3.0`
> 这类精确标签下载，而是通过 GitHub API 解析最新 Release 的真实资产名，统一走
> `releases/latest/download/<资产名>`；`stdlib.dialpkg` 也随每个 `v*` Release
> 一同发布（见 [§25.10](#2510-官方标准库-stdlibdialpkg独立发布)）。因此发新版时
> **不需要改脚本、也不需要补打配套标签**，`DIAL_VERSION` / `STDLIB_RELEASE`
> 只作为可选的「钉版本」覆盖保留。

---

## 33. v1.04：FFI（外部函数接口）

> 本章讲的是**用户侧 FFI**：在 `.dial` 源码里直接调用 C 函数、读写 C 结构体、
> 把 Dial 函数交给 C 回调。§15 讲的是「编译器如何把 Dial 语法降级成 C runtime 调用」，
> 两者不是一回事。§25.6 与 J 组是**包分发**的静态库铁律，一字未动；语言层 `link`
> 除此之外还允许源码作者显式声明动态库（见 §33.8）。

### 33.1 定位与三条硬规则

在 v1.04 之前，想调一个 C 函数（哪怕只是 `puts`）必须先写 `.diali` 接口、编 `.a`、
打 `.dialpkg`、`dialc install`、`import`——FFI 被包机制绑架。v1.04 把 C FFI 变成语言一等公民：

| 路径 | 谁提供函数体 | 怎么链接 |
| --- | --- | --- |
| 系统库（libc / libm / CRT） | 平台自带 | C 编译器**默认动态解析**，不写 `link`、不进 LTO 链路 |
| 用户 C 静态库 | 自己编的 `.a` / `.lib` | `link "xxx.a";`，静态链入产物，零第三方动态依赖 |
| 用户 C 动态库 | 自己编的 `.so` / `.dll` / `.dylib` | `link "xxx.so";`，链接期记录依赖、运行期由系统加载器解析（§33.8） |
| `.dialpkg` 包 | 包内机器码 | `import`，仍强制 `.a` / `.lib`（J 组铁律一字未动） |

三条硬规则：

1. **`link` 只认五类后缀**：`.a` / `.lib` / `.so`（含 `libx.so.1` 带版本号变体）/
   `.dll` / `.dylib`，其它一律编译期拒绝；系统库本就由 C 编译器默认动态解析，
   它们本就是动态的，塞进 LTO 链路只会让编译耗时爆炸，直接写 `extern fn` 即可。
2. **内容魔数必验**：静态库须有 ar 归档魔数 `!<arch>\n`；动态库按**扩展名**验
   ELF `\x7fELF` / PE `MZ` / Mach-O 32、64 位或 fat magic。后缀对但内容是文本
   也当场拒绝（Fail-Fast，绝不把伪装文件丢给链接器换一句莫名其妙的 ld 报错）。
3. **包分发与语言层彻底分开**：`.dialpkg` 的 `targets` 仍只认静态归档（J 组锁死）。
   语言层 `link` 一个动态库是**源码作者显式选择**，由其自行承担运行期依赖；
   不写 `link` 动态库的程序仍满足「只链 libc」的产物契约（§3.4、测试 C 组）。

### 33.2 语法：`extern fn` / `link` / `extern struct`

```dial
// 直接写在 .dial 源码里，没有函数体，以分号结尾；`...` 表示 C 变参，必须是最后一个形参。
extern fn puts(s: rawptr<byte>): int;
extern fn strlen(s: rawptr<byte>): int;
extern fn printf(fmt: rawptr<byte>, ...): int;

// 显式链入用户 C 扩展库；静态归档与动态库二选一，
// 相对路径以**本文件所在目录**为基准解析。
link "lib/libffi_demo.a";        // 静态：产物零第三方动态依赖
// link "lib/libffi_demo.so";    // 动态：见 §33.8，运行期需能找到该库

// repr(C) 结构体：只能经 rawptr<T> 访问，代表 C 侧内存。
extern struct Point { x: int, y: byte, flag: bool }
```

`.diali` 的类型白名单同步打穿：`byte` / `bytes` / `rawptr<T>` / `fn(A, B): R` 全部放行；
形参表改为按**顶层逗号**切分，`fn(int, int): int` 这类自带逗号的类型不会被切坏。
语言层 `extern fn` 与包 `.diali` 撞名、占用内建保留名、与本地函数重名，一律编译期拒绝。

### 33.3 FFI 类型映射

| Dial | LLVM IR（函数边界） | C 侧对应 |
| --- | --- | --- |
| `int` | `i64` | `int64_t` / `ssize_t` / `size_t` |
| `float` | `f64` | `double` |
| `bool` | `i32` | `int`（0/1，C 默认整型提升） |
| `byte` | `i8` | `uint8_t` / `unsigned char` |
| `rawptr<T>` | `ptr` | `void*` / `T*` |
| `fn(A, B): R` | `ptr` | 函数指针 |
| `extern struct`（字段） | 非 packed struct | `struct`（`bool` 字段是 1 字节 `_Bool`，**不**做整型提升） |
| `string` / `bytes` | `ptr`（句柄） | **不要直接传**：用 `as_c_str()` / `as_ptr()` 取数据区 |

> **C 的 32 位 `int` 返回值**：Dial 的 `int` 是 64 位。声明 `extern fn puts(...): int;`
> 时 LLVM 按 `i64` 读取返回值；在主流 64 位 ABI 上，C 的 `i32` 返回值写入 `eax`
> 会零扩展，因此读成 `i64` 是安全的（本仓库示例即如此）。但**不要依赖 32 位有符号
> 语义**——例如 `strcmp` 返回 `-1` 会被读成 `4294967295`。需要精确 32 位语义时，
> 请用自定义静态库把 C 函数封装成 `int64_t` 签名。

### 33.4 C ABI 宽度与 calling convention

用户要求「C 侧可以安全地调用 Dial 函数」，而 Dial 函数体内部 `bool` 是 `i1`、
`byte` 是 `i64`——直接把函数地址交给 C 就是 UB。因此 v1.04 把**所有 Dial 函数的
LLVM 签名统一为 C ABI 宽度**（`bool → i32`、`byte → i8`），并显式固定 C calling
convention（LLVM 0，由 target triple 决定 SysV AMD64 / Windows x64 / AArch64 AAPCS）。

函数体内部仍是 `i1` / `i64`，逻辑零改动；边界由 `abi_adapt` 在**五处**做对称转换：

| 位置 | 方向 |
| --- | --- |
| 函数入口（形参） | C ABI 宽度 → 内部宽度 |
| `return` / 默认返回值 | 内部宽度 → C ABI 宽度 |
| 调用点实参 | 内部宽度 → C ABI 宽度 |
| 调用点返回值 | C ABI 宽度 → 内部宽度 |
| HOF thunk / spawn thunk | 双向（内部 ↔ C ABI） |

`bool` 无条件 `zext` 到 `i32`（**绝不**把 `i1` 直接丢给 C），`byte` 无条件 `trunc`
到 `i8`（**绝不**传 `i64`）。C 变参尾部仍按 C 默认实参提升单独处理，不二次转换。

### 33.5 string / bytes ↔ C 数据总线

Dial 的 `string` / `bytes` 对象布局是 `[ int64 len ][ char data[] ]`，对外句柄指向 `len`。
过去 C 侧要读内容得手写 `+8` 跳过长度头（`demo_vprintf` 就是这么干的），属于 hack。
v1.04 把这一步收进 C Runtime，用户只写方法名：

| 内建 | 方向 | 语义 |
| --- | --- | --- |
| `s.as_c_str(): rawptr<byte>` | Dial → C | **借用**，跳过 8 字节长度头；NUL 结尾，可直接给 `puts` / `printf` |
| `b.as_ptr(): rawptr<byte>` | Dial → C | **借用** bytes 数据区，同上 |
| `string_from_cstr(p): string` | C → Dial | **复制**出新的受 ARC 管理对象；按 NUL 求长度，NULL 视为空串 |
| `bytes_from_ptr(p, n): bytes` | C → Dial | **复制** n 字节；长度必须显式给出（C 缓冲区可含 `'\0'`，不能靠 `strlen` 猜） |

**所有权必须分清**：`as_c_str()` / `as_ptr()` 返回的是不受 ARC 管理的裸指针，
生命周期完全绑定原对象——原对象一释放，指针立即悬垂；`*_from_*` 返回的是新对象，
按普通 Dial 值使用即可。裸指针可直接喂给 `memcpy` / `read` / `write` / `memcmp`。

### 33.6 `extern struct`：C 布局与字段读写

`extern struct` 用 LLVM **非 packed** struct 生成，字段类型按 C 内存宽度
（`bool` / `byte` 均为 `i8`），LLVM 依 target DataLayout 自动插入与 C 一致的对齐 padding。
字段类型白名单只允许 `int` / `float` / `bool` / `byte` / `rawptr<T>` / 嵌套 `extern struct`；
`string` / `bytes` / 数组 / `map` 等 ARC 容器与 `char`（码点 i64 与 C char 宽度歧义）
一律编译期拒绝——避免出现「编译器算的 padding 与 C 不一致」的静默内存损坏。

它**只能经 `rawptr<T>` 访问**（代表 C 侧内存），不能按值传给 Dial 函数、也不能用
`StructLit` 构造。字段读写由编译器按 C 宽度 `load` / `store`，再转回 Dial 内部宽度：

```dial
var raw: rawptr<byte> = raw_alloc(16);   // sizeof(Point) = 8 + 1 + 1 + 6 padding
var p: rawptr<Point> = ptr_cast<rawptr<Point>>(raw);
p.x = 123; p.y = int_to_byte(7); p.flag = true;
print(p.x); print(byte_to_int(p.y)); print(p.flag);
raw_free(raw);
```

`tests/ffi/` 的 P 组用 C 侧 `sizeof` / `offsetof` 逐项比对（`16 / 0 / 8 / 9`），
并做 Dial↔C 双向字段读写，确保布局真的与 C 一致而不是两套 padding。

### 33.7 Dial 函数作 C 回调

`fn(A, B): R` 在 `.diali` 与语言层 `extern fn` 里都能写。具名函数在右值位置求值为
纯代码地址，直接传给 C 的回调参数即可；状态经 `rawptr<void>` 显式透传，
**没有闭包、没有环境对象、没有隐藏类型标签**（延续 v1.03 的设计）：

```dial
extern fn atexit(cb: fn(): void): int;
fn on_exit(): void { print("C called a Dial function"); }
fn main(): int { atexit(on_exit); return 0; }
```

P 组用 `int64_t (*)(int64_t, void*)` 的 C 函数回调 Dial 具名函数并透传 `rawptr<void>`
状态，验证标准 C calling convention 下 C 侧能安全调用 Dial 函数。

### 33.8 `link` 的校验与动态链接

`link "...";` 在编译流程里做四件事：① 后缀白名单（`.a` / `.lib` / `.so` / `.dll` /
`.dylib`，含 `libx.so.1` 这类带版本号的变体）；② 相对路径以**源文件所在目录**
为基准解析并要求文件确实存在；③ 读前 8 字节按后缀验内容魔数；④ 静态库直接追加进
链接器输入，动态库额外把所在目录写进产物的运行期查找路径。

| 后缀 | 种类 | 校验魔数 | 运行期查找 |
| --- | --- | --- | --- |
| `.a` / `.lib` | 静态归档 | `!<arch>\n` | 无（已链入产物） |
| `.so` / `.so.N` | ELF 共享对象 | `\x7fELF` | `-Wl,-rpath,<库目录>` → `RUNPATH` |
| `.dll` | PE 动态库 | `MZ` | 需把 `.dll` 放到 exe 同目录或 `PATH`（PE 无 rpath） |
| `.dylib` | Mach-O 动态库 | 32/64 位或 fat magic | `-Wl,-rpath,<库目录>` |

动态库按**扩展名**判定格式而非宿主平台，因此交叉编译链接他平台库时校验同样成立。
ELF / Mach-O 下编译器自动注入 `-Wl,-rpath,<库所在绝对目录>`，产物不设
`LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH` 也能直接跑；P 组用 `readelf -d` 断言
`RUNPATH` 已写入——而不是碰巧因为 `DT_NEEDED` 里带了绝对路径才成功。

**动态链接是显式选择，代价要认**：链了 `.so` / `.dll` / `.dylib` 之后，产物不再是
「零第三方动态依赖」的单文件，运行期必须能找到那份库（库被移动或删除即加载失败）。
不写 `link` 动态库的程序仍是 §3.4 那套契约，C 组 `ldd` 检查照常生效。Windows 上
PE 没有 rpath，请把 `.dll` 复制到 exe 同目录或加入 `PATH`；需要随产物分发时也可
改用导入库 `.lib`（ar 格式）配 `.dll`。

P 组用真实共享库做端到端（显式 `link` + `extern fn`，比对 `42 / 45`），再用三种
后缀的纯文本伪装库与伪装的 `.a` 验证魔数拒绝不是靠「文件不存在」蒙混过关。

### 33.9 `@export`：把 Dial 函数导出给 C 调用

`extern fn` 是**Dial 调 C**；`@export` 是**C 调 Dial**——把 Dial 函数以**裸 C 符号名**
（无 mangling）与 C 调用约定对外暴露，C 侧可直接链接 / `dlsym` 调用。

```dial
@export fn dial_add(a: int, b: int): int {
    return a + b;
}
```

```c
extern int64_t dial_add(int64_t a, int64_t b);   /* 裸符号名，无 mangling */
int main(void) { printf("%lld\n", dial_add(20, 22)); }   /* → 42 */
```

两条硬规则：

1. **签名必须全是 C 兼容类型**（标量 / `rawptr<T>` / `fn(A):R` / `extern struct` 值）。
   `string` / `bytes` / 数组 / map 这类 ARC 句柄或聚合类型**编译期拒绝**——
   C 侧无法按 C ABI 正确接收它们，放行等于埋 UB。
2. **只有标了 `@export` 的函数才是全局符号**。其余 Dial 函数收成 LLVM `Private`
   linkage：模块内照常递归、照常取函数指针，但**不导出**——既不污染 C 侧的符号
   空间，也不会与别的目标文件里的同名符号撞车；未被引用的还会被死代码消除。
   `main` 例外，恒为外部可见（进程入口）。

`export fn`（不带 `@`）是等价写法。参数与返回值按 §33.4 的 C ABI 宽度传递
（Dial 的 `int` ↔ C 的 `int64_t`）。

**怎么用**：`dialc build x.dial --no-link` 产出目标文件，再由 C 侧链接；或把目标文件
打成共享库（`cc -shared x.o -o libx.so`）后 C 侧 `-l` 链接 / `dlsym` 取符号。

### 33.10 `packed struct`：网络协议的逐字节对齐

普通 `extern struct` 按 C 的自然对齐插 padding（`seq: int` 之后 `flags: byte`
会被推到下一个 8 字节边界）。但**网络协议头**的字段偏移是协议**硬约定**，
一个 padding 字节就让整个包解析错位。因此需要紧凑布局：

```dial
packed extern struct PktHdr {     // 与 C 的 __attribute__((packed)) 逐字节一致
    seq: int,
    flags: byte,
    len: byte
}
packed struct ValHdr { ... }       // Dial 自己的值语义结构体，同样不插 padding
```

两种写法都合法：

- `packed extern struct` —— **C ABI 紧凑布局**。字段宽度走 C 宽度
  （`bool` / `byte` → `i8`），与 C 的 `__attribute__((packed))` / `#pragma pack(1)`
  逐字节一致。这是与 C / 网络协议对口的写法。
- `packed struct` —— **Dial 值语义结构体**的紧凑布局（字段走 Dial 内部宽度）。

布局用 §33.3 那套编译期内建核对：`sizeof<T>()` / `alignof<T>()` / `offsetof<T>(field)`
在编译期按 target DataLayout 算出，零运行时开销。P 组把 Dial 侧的三个数与 C 侧
`sizeof` / `_Alignof` / `offsetof` **逐项比对**（且刻意不写死期望值——布局随平台变，
「两边相等」才是要锁住的东西）。

### 33.11 `volatile rawptr<T>`：内存映射 IO 的易失访问

内存映射寄存器、硬件 FIFO、MMIO 区域有个硬要求：**每次读写都必须真实落到总线**，
编译器不得把它优化掉、重排或合并。`volatile rawptr<T>` 就是这个语义：

```dial
var buf: rawptr<byte> = raw_alloc(8);
var vp: volatile rawptr<byte> = ptr_cast<volatile rawptr<byte>>(buf);
*vp = int_to_byte(42);        // 写入必须落总线
let v: int = byte_to_int(*vp); // 读取必须落总线
```

- 语义与 `rawptr<T>` **完全一致**（同样不受 ARC 管、同样要显式 `raw_free`），
  差别只在**代码生成**：经它读写的内存发射 `load volatile` / `store volatile`，
  LLVM 不再对其做任何优化。
- `volatile` **只能修饰裸指针**。普通值类型没有「每次访问必须落总线」的语义，
  写 `volatile int` 一律编译期拒绝。
- `ptr_cast<volatile rawptr<T>>(p)` 把一段普通内存切进易失视图——**易失性会保留**，
  不会在转换处被悄悄抹掉。

### 33.12 `bitcast<T>(x)`：底层内存强转

`bitcast` 把值 `x` 的**位模式**按目标类型 `T` 重新解释——不读、不改内存，
纯粹是 LLVM `bitcast`（C 的 `reinterpret_cast`）。

```dial
var buf: rawptr<byte> = raw_alloc(8);
let addr: int = bitcast<int>(buf);                    // 指针 -> 整数
var back: rawptr<byte> = bitcast<rawptr<byte>>(addr);  // 整数 -> 指针
```

- **位宽必须相同**。源与目标位宽不一致一律**编译期拒绝**并报出两边的实际位宽
  （`bitcast` 源（64 位）与目标 'bool'（1 位）位宽不一致……）。
  位宽按 target DataLayout 现算——32 位平台上指针是 32 位，
  那时 `bitcast<int>(ptr)` 就该被拒。
- 指针 ↔ 整数走 `ptrtoint` / `inttoptr`（LLVM 的 `bitcast` 本身不允许跨类转），
  整数 ↔ 整数、浮点 ↔ 整数同位宽互转走真正的 `bitcast`。
- 目标类型是**类型实参**（`bitcast<T>(x)`），与 `ptr_cast<T>(x)` /
  `sizeof<T>()` 同一套约定。

### 33.13 `@asm<T>(...)`：内嵌汇编

把一段汇编**原样**交给 LLVM，编译产物里就是一条 `call ... asm sideeffect ...`。
这是写系统调用、读时间戳计数器、操作特殊寄存器的最后一道口子。

```dial
fn asm_add7(x: int): int {
    // "=r,0"：输出约束 + 输入绑定到输出 0；$$7 是 AT&T 立即数 $7 的转义
    return @asm<int>("addq $$7, $0", "=r,0", x);
}
fn asm_intel_id(x: int): int {
    return @asm_intel<int>("mov $0, $1", "=r,r", x);
}
```

- **占位符是 `$0` / `$1`（LLVM IR inline asm 语法），不是 GCC C `__asm__` 的 `%0` / `%1`**。
  AT&T 方言里 `%` 是寄存器前缀，写成 `%0` 会得到 `invalid register name`；
  `$$` 表示一个字面的 `$`（AT&T 立即数）。
- `@asm` 用 AT&T 方言（LLVM 默认），`@asm_intel` 用 Intel 方言，其余规则完全相同。
- **返回类型必须显式写成 `<T>`**，且只能是标量（`int` / `float` / `bool` / `byte` /
  `char`）或裸指针——asm 的结果是一个寄存器里的值，ARC 容器 / 数组 / 结构体 /
  向量都不可能由它构造。约束字符串首项必须是**输出约束**（`=r` / `=&r` / `+r`），
  否则编译期拒绝。
- 模板与约束都必须是**字符串字面量**（编译期常量），运行时字符串没有意义，
  也杜绝了把任意模板拼进 IR 的可能。
- 输入 / 输出按 **C ABI 宽度**进出（`bool`→`i32`、`byte`→`i8`），复用 §33.4
  那套 `abi_adapt` 对称转换；因此 `@asm<byte>` 不会把 Dial 内部的 `i64` 直接塞给
  8 位寄存器。
- `sideeffects = true`：编译器不得把这段汇编当纯函数删掉、合并或重排。
- **平台相关**：模板、约束、寄存器名都是目标机的事，编译器不做可移植性保证。
  这是「信任开发者」的底层原语——它本身就是显式的，与零隐式转换铁律不冲突。

### 33.14 `simd<T, N>`：原生向量类型

```dial
var a: simd<int, 4> = simd<int, 4>(1, 2, 3, 4);   // 逐 lane
var b: simd<int, 4> = simd<int, 4>(10);             // 广播（splat）
var c: simd<int, 4> = a + b;                        // 一条 add <4 x i64>
print(c[0]);                                        // 一条 extractelement
```

**它是寄存器里的值，不是堆上的容器**：`T[]` 是 ARC 管理的动态数组（越界检查、
retain/release、元素按 i64 槽位存储）；`simd<T, N>` 是编译期定长的 LLVM 向量，
无堆分配、无 ARC，逐元素算术直接落到一条向量指令，由后端决定生成
SSE / AVX / NEON。

元素在 IR 里用**原生机器宽度**，而不是 Dial 标量统一的 i64 内部表示——
这正是「原生」二字的含义：

| `T` | LLVM 元素 | 说明 |
| --- | --- | --- |
| `int` | `i64` | Dial 的 int |
| `float` | `f64` | Dial 的 float 是 double |
| `bool` | `i1` | 正是 LLVM 向量比较的原生结果 |
| `byte` | `i8` | 0~255；比较用**无符号**谓词 |
| `char` | `i32` | Unicode 码点 |

所以 `sizeof<simd<byte, 16>>()` 是 **16**（`<16 x i8>`，一条 SSE 寄存器就装得下），
而不是 128（`<16 x i64>`）。`sizeof<T>()` / `alignof<T>()` 按 target DataLayout
编译期现算，与 C 的 `vector_size` 扩展一致。

- `T` 只允许上表五种标量，**不允许嵌套 simd**、不能放 string / 数组 / 结构体 /
  裸指针；`N` 必须是编译期整数字面量，范围 `2~64`（1 个 lane 没有向量意义，
  过大的向量后端无法合法化）。2 的幂更容易映射到硬件指令，但不是硬要求。
- 构造：`simd<T,N>(v)` 把单个标量广播到全部 lane；`simd<T,N>(v0, v1, ..., vN-1)`
  逐 lane 指定。**零隐式转换**同样适用——`simd<byte,4>(200)` 会被拒绝（200 是 int），
  需写 `int_to_byte(200)`。
- 运算：`+ - * / %`（逐元素）、`== != < > <= >=`（返回 `simd<bool, N>`）、
  `& | ^ << >>`（仅整数元素）、一元 `-` 与 `~`。**没有** `and` / `or`——
  短路逻辑对向量没有意义，逐元素请用位运算。
- 索引 `v[i]` 取回一个标量（越界 Fail-Fast）；向量是 SSA 值类型，
  **不能原地改单个 lane**，要换 lane 请重新构造后整体赋值给 `var` 变量
  （优化器会把一串 `insertelement` 合并掉）。向量本身不能直接 `print`，
  请用 `v[i]` 取 lane。
- 边界：`simd` 不能作为数组元素、`extern struct` 字段或 `@export` / `extern fn`
  签名的一部分——C 侧向量 ABI 需要专门的 vector extension 声明，
  编译器不替用户猜测，直接编译期拒绝。
- 向量除法**不做逐 lane 除零检查**：与 C SIMD intrinsic / LLVM 向量除法一致，
  含零除数的 lane 结果为 poison，由调用方保证（标量路径的 Fail-Fast 检查
  无法逐 lane 套用，强行插入会毁掉向量化）。

### 33.15 实现落点

| 层 | 文件 | 改动 |
| --- | --- | --- |
| 词法 / LSP | `lexer.rs` / `lsp.rs` | 新增 `extern` / `link` 关键字与语义 token |
| AST / 解析 | `ast.rs` / `parser.rs` | `ExternFnDecl` / `LinkDecl` / `CStructDef`，顶层声明解析 |
| 包接口 | `package.rs` | `.diali` 白名单打穿 + 顶层逗号切分；`STDLIB_ABI 4 → 5` |
| 编译流程 | `main.rs` | 语言层 extern 并入外部函数表；`link` 五类后缀 + 魔数校验、动态库目录注入 `-Wl,-rpath` |
| 语义 | `semantic.rs` | `rawptr<CStruct>` 字段访问、数据总线内建、保留名与 C 布局白名单 |
| 代码生成 | `codegen.rs` | C ABI 宽度映射、`abi_adapt` 五处对称转换、C 布局字段读写、FFI 内建分派 |
| C Runtime | `stdlib/dial_ffi.c` | 四个数据总线函数（跳过长度头 / 复制构造） |

**导出四件套的落点**（§33.9 – §33.12）：

| 层 | 文件 | 改动 |
| --- | --- | --- |
| 词法 / LSP | `lexer.rs` / `lsp.rs` | 新增 `export` / `packed` / `volatile` 关键字与 `@` 注解前缀 |
| AST | `ast.rs` | `Func.exported`、`StructDef.packed` / `CStructDef.packed`、`Type::VolatilePtr` |
| 解析 | `parser.rs` | `@export fn` 前缀、`packed struct` / `packed extern struct`、`volatile rawptr<T>`、`bitcast<T>` 类型实参 |
| 语义 | `semantic.rs` | `FuncInfo.exported` + C 兼容签名校验、`bitcast` 类型推导与目标校验、`VolatilePtr` 全量并入裸指针分支 |
| 代码生成 | `codegen.rs` | `External` / `Private` linkage 切换、`set_body(&fields, packed)`、`load/store volatile`（`set_volatile`）、`bitcast` + `ptrtoint`/`inttoptr` |

**底层原语的落点**（§33.13 – §33.14）：

| 层 | 文件 | 改动 |
| --- | --- | --- |
| 词法 / LSP | `lexer.rs` / `lsp.rs` | 新增 `asm` / `simd` 关键字与语义 token |
| AST | `ast.rs` | `Type::Simd(Box<Type>, u32)` + `name()` / `display()` |
| 解析 | `parser.rs` | `simd<T,N>` 类型与构造共用 `parse_simd_tail` 校验；`At` 分支识别 `@asm` / `@asm_intel` |
| 语义 | `semantic.rs` | `simd_binary_type` 逐元素推导、构造 / 索引 / 一元校验、print 与 C ABI 白名单拒绝向量、`RESERVED_NAMES` 设防 |
| 代码生成 | `codegen.rs` | `simd_elem_llvm` 原生宽度映射、构造 / `extractelement` / 向量算术与比较、`create_inline_asm` + `build_indirect_call`、`abi_adapt` 边界适配 |

### 33.16 标准库 ABI 递增（破坏性变更）

`stdlib/dial_ffi.c` 新增四个导出函数，因此 `STDLIB_ABI` **4 → 5**，
编译器版本 `1.3.0 → 1.4.0`，`make_stdlib_pkg.py` 默认 `--abi 5`、CI `--abi 5`。
旧包（abi=4）会被编译器当场拒绝并给出重装指引。安装脚本仍走 `releases/latest`
动态解析，不写死版本号。

### 33.17 测试与交付

- 4 个正向示例：`ffi_hello`、`ffi_buffer`、`ffi_struct`、`ffi_callback`；
- 19 个负向用例：伪装动态库 / 动态库缺失 / 非归档后缀、C 布局字段白名单、
  保留名与撞名、C 变参位置、`rawptr<byte>` 字段访问、数据总线参数、
  `extern` 后跟错关键字等；
- 新增 **P 组**：C 侧静态库 + `sizeof`/`offsetof` 比对 + 双向字段读写 + C 回调；
  另编真实共享库做动态链接端到端并断言 ELF `RUNPATH`，再用伪装的
  `.a` / `.so` / `.dll` / `.dylib` 验证魔数拒绝；
- **导出四件套专测**（P 组 2c，`tests/ffi/ffi_export_demo.dial` +
  `tests/ffi/ffi_export_bridge.c`）：
  - `@export` 双向：C 侧回调 Dial 导出的符号（值 `42`）；
  - 符号可见性：`nm` 断言 `dial_add` 是**全局**符号、未标注的内部函数**不导出**；
  - `packed struct`：Dial 侧的 `sizeof` / `alignof` / `offsetof` 与 C 侧
    `sizeof` / `_Alignof` / `offsetof` **逐项相等**（刻意不写死期望值）；
  - `volatile`：运行期往返值 + **`DIALC_DUMP_IR=1` 断言 IR 里确实是
    `load volatile` / `store volatile`**（这才是「每次访问都落总线」的真正保证）；
  - `bitcast`：地址 ↔ 整数往返值 + 位宽不一致必须编译期拒绝；
- **底层原语专测**（P 组 2d，`tests/ffi/ffi_lowlevel_demo.dial`）：
  `@asm` 四种形态（AT&T 恒等 / 匹配约束 `=r,0` 加法 / Intel 方言 / `byte` 的
  C ABI 宽度适配）、`simd` 构造与广播、逐元素算术与比较、无符号 byte 比较、
  索引与 `sizeof` / `alignof` 原生宽度；另用 `DIALC_DUMP_IR=1` 断言 IR 里确实是
  `asm sideeffect` / `inteldialect` / `add <4 x i64>` / `<4 x i8>` / `<2 x double>`
  ——**运行期值只证明结果对，IR 形态才证明没有被降级成标量模拟**；
- **22 个新增负向用例**：导出四件套 5 个（`@export` 签名含 `string`、
  `volatile` 修饰非指针、`bitcast` 位宽不一致 / 指针 ↔ 聚合、`packed` 后跟错关键字）
  + 底层原语 17 个（asm 模板非字面量 / 返回类型非法 / 缺输出约束 / 参数不足；
  simd 元素非标量 / 嵌套 / N 非法 / N 非常量 / 构造参数个数 / 混合类型运算 /
  短路逻辑 / 直接 print / 改 lane / `@export` 签名 / `extern struct` 字段；
  以及 `asm` / `simd` 保留名设防）；
- `bash tests/run_tests.sh` **A–Z 二十六组全套通过（499 ✅ / 0 ❌）**，
  `tools/cve_check.py` 8/8、三平台 `zig cc` 编译通过。

> **本轮交付范围**：代码已推送 `origin/main`，并打**发布标签 `v1.1`**（对应版本号
> `v1.10` / `Cargo.toml 1.10.0`，映射见 §23 与 CHANGELOG `[v1.10]` 段），由 `v*` CI 规则
> 触发三平台 Release；**`v1.0` 里程碑标签不受影响**。`[v1.04]` 及其后续增补
> （含 `@asm` / `simd<T, N>`）均随本次发布一并交付。
>
> **本节上方的「A–Z 二十六组 / 499 ✅」是 `v1.1` 标签发布时的验收快照，保持原文不改。**
> 此后工作树又新增 `gui.dialpkg` 地基层与 AA 组（507 ✅），再补控件框架层
> （AA 组 +3 条断言到 **510 ✅**），又补成品控件外观层（AA 组 +3 条到
> **513 ✅**），再修 `http.dialpkg` 三处 Fail-Fast / 可用性缺陷并给 W 组 +4 条
> 断言到 **517 ✅**，再补 GUI 输入层（TextBox / 焦点链 / 统一键码 / XTest
> 实机注入）给 AA 组 +3 条到 **520 ✅**，其后补主题层 + 控件扩展
> （可覆写主题色 / Radio / Slider / ProgressBar）又给 AA 组 +3 条到 **523 ✅**，
> 本轮补信号槽 / 控件事件订阅（6 类可订阅纯值信号 + 每树环形队列）再给 AA 组
> +3 条断言到 **526 ✅**，全量回归仍为 **A–Z + AA 二十七组**；
> 其后补 `int32` / `float32` 一等类型 + FFI 精确 32 位宽度，新增 AB 组 4 条
> 断言与 B 组 5 条零隐式转换负向，扩为 **A–Z + AA + AB 二十八组 / 535 ✅**；
> 再补菜单 / 对话框覆盖层（第 7 层）给 AA 组 +3 条到 **538 ✅**，
> 补 B1 `impl` 方法块（语言特性）新增 AC 组 5 条断言，扩为
> **A–Z + AA + AB + AC 二十九组 / 549 ✅**；本轮再补 GUI 第 8 层 Dial 侧 OO
> 封装（`gui_oo.dial`，纯源码、随包 `src/` 分发、零 `extern fn` 增量）新增 AD 组 6 条断言，
> 第 9 层滚动视图 / 第 10 层右键上下文菜单 / 第 11 层悬浮提示 / 第 12 层状态栏 /
> 第 13 层通知气泡（均为纯内存对象 + 离屏画布，零 DISPLAY 依赖）各新增 AA 组
> +1 条断言，扩为 **A–Z + AA + AB + AC + AD 三十组 / 560 ✅**；随后 dialpkg
> `src/` 随包 Dial 源码模块（§25.11）给 Q 组新增 10 条断言到 **570 ✅**；
> 随后把 `gui_oo.dial` 纳入 gui 包分发、AD 组新增「装包即用」1 条断言，扩为
> **三十组 / 571 ✅**；随后 GUI 第 14 层窗口级覆盖层自动集成给 AA 组新增 3 条
> 断言（编译成功 / 无 DISPLAY 空窗口 Fail-Fast / Xvfb 实窗自动绘制 CRC），扩为
> **三十组 / 574 ✅**；随后 GUI 第 15 层快捷键 / 命令系统给 AA 组新增 2 条
> 断言（编译成功 / 166 条零 DISPLAY 断言），扩为 **三十组 / 576 ✅**；随后
> `dialc fmt` 强化与 `dialc pack` 强化给 Q 组新增 11 条断言（4 条格式化 +
> 7 条打包），扩为 **三十组 / 587 ✅**（按组累加口径，脚本当时实测 586 ✅）；随后
> 工具链再强化（`doctor` / `deps` / 统一诊断 / `watch` / `clean` / `completion`，
> §34.11）给 Q 组新增 19 条断言，扩为 **三十组 / 605 ✅**（本节起统一以
> `tests/run_tests.sh` 实测为准：586 ✅ + 19 = 605 ✅）；随后 LSP 再强化
> （§28.7.2）给三个冒烟脚本新增 7 条断言，扩为 **三十组 / 612 ✅**
> （605 ✅ + 7 = 612 ✅），属
> **未发布的开发中增量**，见 §5.8 / §25.11 / §34.2 / §34.3 / §34.5 / §34.10 /
> §37.12 / §38.14 / §38.15 / §38.16 / §38.17 / §38.18 / §38.19 / §38.20 /
> §38.21 / §37.10 与 CHANGELOG `[未发布]` 段。


---

## 34. v1.04 增补：工程工具链（并入 `[v1.04]`，版本不变）

> 上一轮 `REPORT_v1.04_gaps.md` 按「普通程序员写日常程序」实测了 54 个小程序 + 27 个探针，
> 结论是 **15 个能直接写、39 个编译期就写不出来**，其中最挡真实项目的是 **A 组工程化/工具链**：
> 本地几个 `.dial` 之间无法互相引用、CLI 只有 4 个命令、多余输入被静默吞掉、出错没有位置信息。
> 本章把这一组补齐，并顺带修掉 D 组「文档 / 诊断与实现不一致」。
>
> **交付约定**：按用户要求**并入 `[v1.04]`**——`Cargo.toml` 保持 `1.4.0`、
> `STDLIB_ABI` 保持 `5`、CHANGELOG 不新增 `[v1.05]`；只做本地提交，不推送、不打 tag、不发 Release。
> 清单里的 **B / C 组**（用户泛型、ADT/Option、字符串 / 数组标准库 API）本轮**不动**，仍是待办；
> 这一轮专注「让已有能力在工程里用得顺」。

### 34.1 CLI 全景

`dialc help`（或 `--help` / `-h`）打印完整用法。全部命令：

| 命令 | 作用 | 产物 / 退出码 |
| --- | --- | --- |
| `dialc build <文件.dial> [-o 输出] [--no-link] [--safety debug\|safe\|fast] [--pgo]` | 编译链接 | 可执行文件；`--no-link` 只出 `.o` |
| `dialc run <文件.dial> [同上]` | 编译并运行 | 透传被运行程序的退出码 |
| `dialc check <文件.dial>` | **只做词法 / 语法 / 语义检查**，不生成 `.o`、不链接 | 通过打印摘要，失败非零退出 |
| `dialc test <文件.dial> [--filter 名字]` | 运行顶层 `test "名" { ... }` 块 | 逐块 ✅/❌，有失败即非零退出 |
| `dialc fmt <文件.dial>... [--check\|--diff]` | **间距规范化格式化**（token 流 / 字面量·注释原文不变、幂等） | `--check` 只报告、`--diff` 逐行对照，均不写入 |
| `dialc new <目录> [包名]` | 生成可打包的项目骨架 | `dial.toml` / `main.dial` / `<包名>.diali` / `lib/` |
| `dialc pack <目录> [-o 包.dialpkg] [--dry-run\|--json\|--init]` | 打包 / 预览 / 机器可读清单 / 自动生成清单草稿 | `--dry-run` / `--json` 不产出包；`--init` 只写清单、绝不覆盖 |
| `dialc pack --verify <包.dialpkg>` | 用安装期同一套规则校验已有包 | manifest + 逐文件 SHA256，篡改即非零退出 |
| `dialc install <包.dialpkg\|URL> [--sha256 哈希]` | 安装包（v0.93 / v0.96） | 行为不变 |
| `dialc list` | 列出已安装包（名字 / 版本 / ABI / 路径） | 无包时给提示 |
| `dialc uninstall <包名>` | 删除缓存目录里的包 | 未安装则报错 |
| `dialc version \| --version \| -V` | 版本号 | 取自 `Cargo.toml`，全站唯一真相 |
| `dialc doctor [--json]` | 环境自检（编译器 / C 工具链 / 缓存 / 包完整性 / 清单 / src 解析） | 有 `fail` 即非零退出；`--json` 机器可读 |
| `dialc deps <文件.dial> [--json]` | 三级 import 依赖图 + 循环检测 | `--json` 给节点 / 深度 / `cycles` |
| `dialc watch <文件.dial> [--action check\|build\|test] [--interval ms] [--max-ticks N]` | 变更自动重跑 | 限拍退出，动作白名单外拒绝 |
| `dialc clean [目录] [--apply] [--json]` | 清理编译产物 | 默认 dry-run，`--apply` 才删 |
| `dialc completion <bash\|zsh\|fish>` | 生成 shell 补全脚本 | 与 `help` 同源的 `COMMANDS` 元数据 |
| `dialc lsp` | 编辑器语言服务器 | stdio 协议，stdout 绝不混入人读文本 |

`build / run / check / test` **共用同一条前端流水线**（`main.rs` 的 `frontend()`）：
本地源码收集 → 包解析 → 逐个文件正式 parse → AST 合并 → 语言层 FFI 声明 → 语义分析。
共用一条前端是硬要求——否则「`check` 说通过」和「`build` 真编译」可能给出不同结论。

### 34.2 本地源码模块（`import "./util.dial";`，A1）

此前 `import` 只认 `~/.dial/packages/<name>/` 下的已安装包，想把工具函数拆到另一个
`.dial` 只能先打 `.dialpkg` + `install`。v1.04 起源码模块是一等公民：

```dial
// main.dial
import "./util.dial";        // 显式相对路径
import "helper";             // 裸名：同目录存在 helper.dial 即视为本地模块
fn main(): int { print(helper() + inner()); return 0; }
```

判定规则（按顺序，**三级优先**，v1.10 增补第 3 级）：

1. 以 `.dial` 结尾，或以 `./` / `../` 开头 → **本地源码**（第一级）；未写后缀时自动补 `.dial`。
2. 裸名且**同目录存在 `<name>.dial`** → **本地源码**（第一级，清单 A1 里 `import "util";` 的原样写法）。
3. 裸名且**某已安装包的 `manifest.src` 声明了该模块名** → **包内 `src/` 的 Dial 源码模块**
   （第二级，见 §25.11）——直接从 `~/.dial/packages/<name>/src/` 展开，**无需复制文件**。
4. 其余裸名 → **已安装包**（第三级，v0.93 以来的语义完全不变，读 `.diali` + 选 `.a`）。

本地文件永远优先于包内副本（「改一份源码试试」不必动已安装的包）；包内源码模块
内部若写了 `import "包名";`，会照常汇总到第三级，故模块能直接调用包里的 `extern fn`。

编译器递归展开第 1 / 2 级模块、合并 AST 后统一编译：

- **循环引用**当场报错并列出完整链路（`a.dial -> b.dial -> a.dial`）；
- **同名函数 / 结构体**报错并给出两个文件的路径；
- 同一文件被多个模块 import 时**只展开一次**（不会重复定义）；
- 子模块里的 `link "...";` 相对路径按**该子模块所在目录**解析，不是按主文件目录；
- `const` / `enum` 是 parser 内联的编译期常量，**各文件各自持有**，不跨文件共享；
- 诊断的「文件:行:列」在多文件项目里以**主源文件**为编译单元名，各语句的行列仍按各自文件准确。

源码作者显式 `link` 动态库是自己的选择，与包分发铁律（§25.6、J 组）是两条互不相干的链路。

### 34.3 `dialc check` 与 `dialc fmt`

`check` 跑完整前端但不产出任何文件，适合接进编辑器保存钩子 / CI 的静态检查步骤：
`dialc check src/main.dial` → `✅ 类型检查通过：...（N 个函数、M 个 test 块）`。

`fmt` 是**间距规范化格式化**（v1.10 强化，见 §34.10）：在重排缩进之外，还规范
运算符两侧、逗号 / 分号 / 冒号后、控制关键字与 `(` 之间、大括号两侧的空白。
它仍然**不合并 / 拆分行、不增删 token**，安全性由三条当场自检的硬契约保证：

- 格式化前后 `lexer::lex` 的 **token 序列必须完全一致**，不一致就拒绝写入；
- **字面量与注释原文必须完全一致**（`protected_spans()` 提取后逐项比对）；
- 结果**幂等**（`fmt(fmt(x)) == fmt(x)`）。

因此格式化永远不会改变程序语义，`fmt` 后可直接编译运行。`dialc fmt --check <文件...>`
只报告不写入（适合 CI），`dialc fmt --diff <文件...>` 逐行打印差异也不写入。
LSP 的编辑器内格式化（§28.7.1）仍是「去行尾空白 + 末尾换行」的非破坏性动作，
两者刻意不同：命令行敢重排间距，编辑器内绝不改动你正在敲的那一行。

### 34.4 `dialc test`（顶层测试块，A4）

```dial
fn add(a: int, b: int): int { return a + b; }

test "addition" {
    assert(add(1, 2) == 3, "add should work");
}

test "string length" {
    var s: string = "abc";
    assert(s.length() == 3, "len");
}

fn main(): int { return 0; }
```

`test "名字" { ... }` 是**顶层块**（不是函数、不是表达式）。刻意不新增 `test` 关键字：
parser 用「`Ident("test")` 后跟字符串字面量」识别，因此 `test` 依然可作变量名 / 函数名，
不破坏任何已有程序。

- `dialc build` / `run` **忽略**测试块（与 Rust `#[cfg(test)]` 同理，测试代码不进正式产物），
  但会打印一行提示，避免「写了 test 却以为 build 跑过了」；
- `dialc test` 把**每一块单独**编成一个可执行文件并运行。之所以一进程一块：`assert` 失败
  按 Fail-Fast 铁律立刻发非法指令终止进程，塞进同一个二进制会让第一块失败就跑不到后面；
- `--filter <名字>` 按子串筛选；逐块输出 `✅ 名字` / `❌ 名字` 并附上 stderr 前几行
  （含 §34.7 的文件:行:列），最后汇总，有失败即非零退出；
- 测试块内不要写显式 `return`（合成入口要求 `int` 返回值）；块里可直接调用同文件的其它函数。

### 34.5 `dialc pack` 与 `dial.toml`（打包闭环）

此前「包」只有消费端：`dialc install` 能装、`import` 能用，但仓库里没有**通用产包路径**——
`tools/make_stdlib_pkg.py` 写死 `stdlib`、`tools/make_sample_pkg.sh` 写死 `demo`，
作者想发自己的 C 库只能复制脚本或手工拼 zip + manifest + SHA256（错包要到安装期才暴露）。
v1.04 把产包做成编译器一等公民：

```toml
# dial.toml —— 放在项目根目录，dialc pack <目录> 读取
name = "mylib"
version = "1.0.0"
abi = 6
iface = "mylib.diali"
license = "MIT OR Apache-2.0"

[targets]
# 值既是磁盘上相对项目根的路径，也是 .dialpkg 内的路径
x86_64-linux-gnu = "lib/x86_64-linux-gnu/libmylib.a"

[src]
# 可选：随包分发的 Dial 源码模块（见 §25.11）。键 = import 裸名，值 = src/ 下的 .dial。
mylib_oo = "src/mylib_oo.dial"
```

> `abi` 必须与编译器 `package::STDLIB_ABI` 一致，否则 `dialc install` 会在**链接期前**
> 明确拦截并给重装指引（§37.6 起为 `6`）。`dialc new` 生成的骨架已改为直接引用
> `package::STDLIB_ABI`，不会写死数字；这里手写示例时按当前 ABI 填，别照抄旧值。

`dial.toml` 只支持 `key = "字符串"` / `key = 整数` 与 `[targets]` / `[src]` 两个段的
**TOML 子集**：未知字段、重复字段、点分键、缺字段一律**显式报错**，绝不悄悄忽略配置项。
`[src]` 段额外收紧：键必须是裸名（不含路径分隔符、不带 `.dial` 后缀），值必须位于
`src/` 目录下且以 `.dial` 结尾，声明的文件必须真实存在——四条任一不满足都在**打包期**
Fail-Fast，不拖到安装 / 编译期（见 §25.11）。
清单是本地作者文件、字段固定，故不引入新的 TOML 依赖（三个 target 的静态构建也少一份供应链负担）；
读外部不可信输入的 `manifest.txt` 仍用 `serde_json`（§25.2）。

`pack` 的校验与打包：

1. 校验 `iface` 存在且以 `.diali` 结尾；每个 target 的库存在、后缀为 `.a` / `.lib`、
   且带 ar 归档魔数 `!<arch>\n`——**包分发只认静态归档，J 组铁律一字未动**；
2. 生成 `manifest.txt`（字段与 `package::parse_manifest` 完全对齐：`name / version / abi /
   iface / license / targets / src / sha256`），逐文件算 SHA256（含 `src/*.dial`）；
   `src` 是**可选字段**，旧包没有它照常能装（向后兼容）；
3. 压成 `.dialpkg`（zip 容器，路径用正斜杠、顺序确定）；
4. **打包后就地自检**：用 `parse_manifest` 解析刚生成的 manifest、再用 `verify_archive`
   逐文件复核哈希——产物一定过得了 `dialc install` 的两道校验，不把问题留到安装期。

完整闭环（`dialc new` 生成的骨架可直接走完）：

```bash
dialc new ./mylib mylib            # 生成 dial.toml / main.dial / mylib.diali / lib/ / src/
# 把编译好的 .a 放到 dial.toml [targets] 登记的路径；把 .dial 源码模块放进 src/
# 并在 [src] 段登记（可选）
dialc pack ./mylib -o mylib.dialpkg
dialc install mylib.dialpkg
dialc list
# 源码里：import "mylib"; 调用 .diali 里的 extern fn；
#        import "mylib_oo"; 直接用包内 src/ 的 Dial 源码模块，无需复制文件
dialc uninstall mylib
```

`tools/make_stdlib_pkg.py` **保留**：CI 生成官方三平台 `stdlib.dialpkg` 仍走它
（它一次收三份 `.a`、包名固定 `stdlib`），与 `dialc pack` 的「单项目通用打包」分工明确。

v1.10 起 `dialc pack` 另有 `--init` / `--dry-run` / `--json` / `--verify` 四个选项，
把「手写清单 + 盲打」变成「自动扫描 + 先预览后打包 + 可校验」，见 §34.10。

### 34.6 诊断带位置（A7）与 Fail-Fast 收紧（A3）

**A7**：`panic("...")`、`assert(...)` 失败、未捕获异常现在都带**源码位置**：

```
dial: 运行时断言失败（Fail-Fast 终止）: x should be 2 (at /path/main.dial:3:5)
dial: 未捕获的异常（panic）: something broke
       位置: /path/main.dial:2:5
```

文件名、行列是**编译期常量**（`CgOpts.src_file` + `gen_stmt` 记下的语句位置），
运行期零额外开销；首行保持与旧版完全一致，既有脚本与 G 组回归不受影响。
未捕获异常的新入口叫 `dial_panic_loc` / `dial_assert_fail_loc`，旧符号保留给其它 C 调用点。

**A3**：旧版 `dialc build a.dial b.dial` 只打印一行「未知参数 'b.dial'」、
**却仍以 `rc=0` 产出 a 的可执行文件**，让人误以为两个文件都编进去了。现在任何不认识的
参数一律**当场非零退出**；若多出来的是 `.dial`，错误信息直接指向 §34.2 的本地模块写法：
「dialc 一次只编译一个源文件；多个 .dial 请用 `import "./xxx.dial";`」。
同一条规则推广到所有新子命令（`ensure_no_extra`）。

### 34.7 D 组文档 / 诊断漂移修复

| 编号 | 修正 |
| --- | --- |
| D1 | `examples/string_demo.dial` 注释曾写「`+` 不对 string 重载」，实测 `+` / `+=` 早已可用，改为正确说明（`concat()` 仍保留） |
| D2 | §16 曾写「`print` 恰好接受 1 个参数」，改为 variadic（v1.00 起）并给示例 |
| D3 | §5.5 / §15 只写了 `str(char)`，补上实测分派：`int` / `float` / `char` 可用，`bool` 编译期拒绝 |
| D4 | §17.2 标题「截至 v1.02」→「截至 v1.04」，表格里「函数指针不支持」改为「v1.03 已支持（闭包仍无）」、「源码模块拆分尚未实现」改为「v1.04 已支持」，并同步三处锚点引用 |
| D5 | `Point[]` 的报错曾声称「元素类型只支持 int 与句柄」，改为列出真实的标量 + 句柄白名单 |
| D6 | 「找不到已安装的包」现在附带「本地源码模块写 `import "./xxx.dial";`」的指引；LSP 的稳定错误码与文案前缀不变 |

### 34.8 实现落点

| 层 | 文件 | 改动 |
| --- | --- | --- |
| CLI / 前端 | `main.rs` | 命令分派表；`frontend()` 共用流水线；`collect_sources()` 递归合并本地模块；`check / fmt / test / new / list / uninstall / pack / doctor / deps / watch / clean / completion`；三处前端错误出口统一走 `diagnostic::report()` |
| 格式化 | `src/dialfmt.rs`（v1.10 新增） | `Word / Op / Raw` 片段扫描 + 间距规则；`protected_spans()` 原文自检；`format_source()` 幂等重排 |
| 工具链命令 | `src/toolchain.rs`（v1.10 新增） | `COMMANDS` 元数据；`doctor` 自检 / `deps` 三级依赖图 / `watch` 限拍重跑 / `clean` dry-run / `completion` 三 shell；共用本地源码递归 |
| 诊断渲染 | `src/diagnostic.rs`（v1.10 新增） | 人读定位 + 稳定错误码 + caret；`DIALC_DIAG_FORMAT=json` 一行 JSON；stdout 绝不污染 |
| 打包 | `src/pkgbuild.rs` | `dial.toml` TOML 子集解析、`pack` 生成 manifest + SHA256 + zip 并自检、`new` 骨架、`list` / `uninstall`；v1.10 抽出 `plan()` 供 `--dry-run` / `--json` 共用，新增 `--init` / `--verify` |
| AST / 解析 | `ast.rs` / `parser.rs` | `TestDef` / `Program.tests`；顶层 `test "名" { ... }` 用「Ident + 字符串」识别，零新增关键字 |
| 语义 | `semantic.rs` | `stmt_pos` 提升为 crate 内可见；D5 诊断文本校正 |
| 代码生成 | `codegen.rs` | `CgOpts.src_file` / `cur_pos`；`emit_fail_loc()` 给 panic / assert / 未捕获异常注入位置 |
| C Runtime | `runtime/dial_rt_trap.c` / `stdlib/dial_exc.c` | `dial_assert_fail_loc` / `dial_panic_loc` 两个带位置入口 |
| 回归 | `tests/run_tests.sh` | 新增 **Q 组**（工具链专项） |

### 34.9 测试与交付

- 新增 **Q 组**：`help / version / list / check`、A3 非零退出且不产物、`fmt` 幂等与
  token 流不变、`test` 通过/失败/过滤、多文件 import 合并与循环/冲突/缺失诊断、
  `new → pack → install → import → uninstall` 全链路闭环、`pack` 拒绝动态库 / 缺字段，
  以及 A7 位置信息断言；v1.10 再补 11 条——`fmt --diff` / 间距规范化 / 原文保护 /
  编译运行一致（4 条），`pack --init` 草稿与拒绝覆盖 / `--dry-run` 预览与未登记提示 /
  `--json` 机器可读 / `--verify` 校验与篡改拦截（7 条）；再补 19 条——`doctor` 人读 /
  `--json`、`deps` 三级分类 / `--json` / 循环引用、统一诊断人读定位 / stdout 干净 /
  一行 JSON、`watch` 限拍与动作白名单、`clean` dry-run / `--json` / `--apply`、
  `completion` 三 shell / 拒绝未知 shell / 管道截断不 panic（见 §34.11）；
- `bash tests/run_tests.sh` **A–Q 全量回归 0 ❌**；`check_version` / `check_links` /
  `cve_check` 全通过；`cargo check` 零警告；新增 C 入口经 `zig cc` 三目标编译通过。

> **本轮交付范围**：并入 `[v1.04]`、版本号与 `STDLIB_ABI` 均不变；只做**本地提交**，
> 不推送 GitHub、不打 tag、不发 Release。`v1.0` 标签不受影响。

---

### 34.10 工具链强化：真格式化与少手工打包（v1.10 增补）

> 一个语言好不好用，工具链往往决定第一印象。本轮强化两条最常被手敲的路径：
> `fmt` 从「只敢理缩进」升级为「敢理间距」，`pack` 从「手写清单 + 盲打」升级为
> 「自动扫描 + 先预览后打包 + 可校验」。两者都**不改 ABI / 版本号 / 既有命令的默认行为**。

#### `dialc fmt`：间距规范化

旧版只重排行首缩进、去行尾空白、统一末尾换行——用户嫌「太差」。v1.10 把实现
换到 `src/dialfmt.rs`：每行扫成 `Word / Op / Raw` 三种片段，空白只作为「片段间
是否补一格」的决策依据，**绝不改写片段内部**。

| 规范化项 | 例子（前 → 后） |
| --- | --- |
| 运算符两侧 | `a+b` → `a + b`、`x>0` → `x > 0` |
| 逗号 / 分号 / 类型冒号后 | `fn add(a:int,b:int):int{...}` → `fn add(a: int, b: int): int { ... }` |
| 控制关键字与 `(` | `if(x>0)` → `if (x > 0)`；函数调用 `print("x")` 不补空格 |
| 大括号两侧 | `){` → `) {`、`return a+b;}` → `return a + b; }` |
| 一元前缀不拆 | `x=-1` → `x = -1`（绝不变成 `x = - 1`） |
| 多字符运算符不拆 | `a>>b` → `a >> b`（`>>` 始终是一个片段） |

字符串 / 字符 / 字节字面量（`b'..'`）、行注释、跨行块注释一律作为 `Raw` 原样
保留：`"a  b // not comment"` 里的双空格与 `//` 不会被当成代码，注释里的
双空格也不会被压缩。三条硬契约由 `cmd_fmt` **当场校验，任一不满足就拒绝写入**：

1. `lexer::lex` 的 **token 序列完全一致**；
2. `protected_spans()` 提取的**字面量与注释原文序列完全一致**；
3. **幂等**（`format(format(x)) == format(x)`）。

`--check` 只报告不写入（CI 用），`--diff` 逐行打印 `---/+++/@@/-/+` 差异也不写入；
两者对「有文件未格式化」都返回非零。LSP 的编辑器内格式化仍保持非破坏性
（§28.7.1），命令行与编辑器刻意分工。Q 组新增 4 条断言锁定间距、原文保护、
幂等与格式化后编译运行一致。

#### `dialc pack`：五个视角，一条计划

`pack` 的「收集入库文件 + 生成 manifest + 逐文件 SHA256」被抽成 `pkgbuild::plan()`，
`pack` / `--dry-run` / `--json` **共用同一个计划**——预览看到的与真正打出来的
逐字节一致，不会出现「预览说有、包里没有」。

| 选项 | 作用 | 写文件吗 |
| --- | --- | --- |
| `dialc pack <目录>` | 原行为：打包并自检 | 写 `.dialpkg` |
| `--init` | 扫描目录自动生成 `dial.toml` 草稿：自动发现 `*.diali` / `lib/<triple>/*.a` / `src/*.dial`，逐项写成**显式声明** | 只写清单，**已有清单拒绝覆盖** |
| `--dry-run` | 打印入库文件、SHA256 前 16 位、字节数、`manifest.src` 映射与输出路径 | 不写 |
| `--json` | 同一份计划的机器可读版本（`files` / `sha256` / `unlisted` / `written=false`） | 不写 |
| `--verify <包.dialpkg>` | 用**安装期同一套规则**（`parse_manifest` + `verify_archive`）校验已有包 | 不写 |

「清单显式声明」仍是唯一入库依据：`--init` 只生成可核对、可删改的草稿，
`--dry-run` 会额外列出**磁盘上有但清单未登记**的文件并明确标注「不会进包」，
绝不自动塞进去。缺字段 / 缺文件的诊断带具体路径与补齐提示。`--verify` 对
被篡改的包返回非零并给出期望 / 实际 SHA256。Q 组新增 7 条断言覆盖草稿生成、
拒绝覆盖、预览、JSON、未登记提示、正常校验与篡改拦截。


### 34.11 工具链再强化：自检 / 依赖图 / 统一诊断 / 开发循环（v1.10 增补）

> §34.10 解决了「格式化太差 / 打包太吃操作」，本轮把另外五件过去只能靠人眼或
> 外部脚本的高频动作收进 `dialc` 自身：环境自检、依赖图、诊断机读、变更自动重跑、
> 产物清理与 shell 补全。全部落在 `src/toolchain.rs` 的**一份命令元数据 + 一套本地
> 源码递归**上，`src/diagnostic.rs` 统一前端错误渲染。**不改 ABI / 版本号 / 任何既有
> 命令的默认行为**，也不碰用户已安装包的内容（`doctor` 只校验、不修复）。

| 命令 | 作用 | 边界 / 退出码 |
| --- | --- | --- |
| `dialc doctor [--json]` | 自检编译器版本 / C 编译器与 `ar` / 包缓存目录 / 已安装包 manifest + 逐文件 SHA256 / 当前目录 `dial.toml` / 包内 `src` 模块解析 | 有 `fail` 项即非零退出；`--json` 给 `healthy` / `failures` / `warnings` / 逐项 `checks` |
| `dialc deps <文件.dial> [--json]` | 递归展开**三级 import**：① 本地源码 ② 包内 `src` 模块 ③ 包名 | 复用 `import_specs()` / `is_local_import()` / `find_src_module()`；命中循环标注 `cyclic` 并停止递归 |
| `dialc watch <文件.dial> [--action check\|build\|test] [--interval ms] [--max-ticks N]` | 监控主文件 + 递归本地模块，mtime / 长度变化即重跑指定动作 | `--max-ticks` 限拍便于 CI 与冒烟；动作白名单外一律拒绝 |
| `dialc clean [目录] [--apply] [--json]` | 清理 `.o` / `.bc` / `.ll` / `.dialpkg` 编译产物 | **默认 dry-run 只列不删**，`--apply` 才真删；`.dial` 源码与包缓存绝不触碰 |
| `dialc completion <bash\|zsh\|fish>` | 生成 shell 补全脚本 | 命令与选项取自 `COMMANDS` 元数据，与 `help` / `doctor` 同源 |

#### 统一诊断渲染（人读 + 机读）

`build / run / check / test` 的前端错误出口统一交给 `diagnostic::report()`：

- **人读模式**：先把**原消息一字不动**打到 stderr（既有回归按原文匹配，一个字都不能
  改），再追加 `--> 文件:行:列 [稳定错误码]`、源码行与 `^` caret。错误码优先取消息里
  已有的 `dialc.xxx`，否则按词法 / 缺包 / 重复定义 / 语法 / 语义阶段推断。
- **机读模式**：`DIALC_DIAG_FORMAT=json` 时 stderr 输出**一行 JSON**——
  `tool / severity / code / message / file / line / col / snippet / caret`，供编辑器与
  CI 直接解析；stdout 始终不被诊断污染（与 `lsp` 的协议通道同一条纪律）。
- **绝不重新判定错误**：行列号与错误码只从 `frontend()` 汇总后的消息里解析，
  诊断层不碰语义、不改退出条件。

#### 共用基础设施与边界

`COMMANDS`（`src/toolchain.rs`）是 `help`、`completion`、`doctor` 三处命令清单的
唯一真相，避免多份手写清单漂移；`local_file_set()` / `deps_walk()` 复用
`import_specs()`、`lexer::lex()` 与 `frontend()` 的同一套三级解析路径。`completion`
把脚本缓冲后**一次性写出**，因此 `dialc completion bash | head` 提前关闭管道时
只忽略一次写错误，不会触发逐行 `println!` 的 Broken pipe panic。`doctor` 的
包完整性校验复用安装期同一套 `manifest.txt` + 逐文件 SHA256 口径（§25.4）。

Q 组新增 **19 条断言**：help 收录新命令、doctor 人读 / `--json`、deps 三级分类 /
`--json` 结构 / 循环引用、诊断人读定位 / stdout 干净 / 一行 JSON、watch 限拍与
动作白名单、clean dry-run / `--json` / `--apply`、completion 三 shell / 拒绝未知
shell / 管道截断不 panic。全量 `bash tests/run_tests.sh` 实测
**605 ✅ / 0 ❌ / 三十组**；本节起计数统一以回归脚本实测为准（§34.10 此前记的
587 ✅ 为按组累加口径，586 ✅ + 19 = 605 ✅；其后 LSP 再强化 +7 到 612 ✅，见 §28.7.2）。实现落点见 §34.8。

---

## 35. v1.04 增补：DWARF 调试信息与 `-g`（并入 `[v1.04]`，版本不变）

> §20 的 `int3` / `ud2` 让崩溃能被调试器**精确停住**，但停下之后看到的仍是
> `rdi` / `rsi` 和一堆十六进制地址。本章补上最后一公里：把 LLVM 裸 IR 与
> Dial 源码的 AST 名字对上，让 GDB / LLDB 能按**源码行**下断点、按**源码变量名**
> `print`，`bt` 里直接显示 `main () at app.dial:9`。
>
> **交付约定**：并入 `[v1.04]`，`Cargo.toml` 保持 `1.4.0`、`STDLIB_ABI` 保持 `5`，
> 不新增 `[v1.05]`；只做本地提交，不推送、不打 tag、不发 Release。

### 35.1 用法

```bash
dialc build app.dial -g -o app     # -g / --debug-info 等价；build / run 都支持
gdb -q ./app
```

`-g` 是**编译期开关**，不改变语言语义；`dialc run app.dial -g` 同样可用。
不带 `-g` 时完全不创建 DIBuilder，产物里没有 Dial 的调试符号，体积与编译速度不变。

### 35.2 GDB 会话实录

以下面这个最小程序为例：

```dial
fn add(a: int, b: int): int {
    let sum: int = a + b;
    return sum;
}

fn main(): int {
    let counter: int = 42;
    let note: string = "dial";
    print(add(counter, 1));
    return 0;
}
```

```text
$ gdb -q -batch -ex 'break app.dial:9' -ex run -ex 'print counter' -ex bt ./app
Breakpoint 1 at 0x...: file app.dial, line 9.
Breakpoint 1, main () at app.dial:9
9       print(add(counter, 1));
$1 = 42                                  # ← 不是 rdi，是 Dial 源码里的变量名
#0  main () at app.dial:9                # ← backtrace 直接指向 .dial 行
```

`info args` 在 `add` 里显示 `a = 42` / `b = 1`，`print sum` 显示 `43`，
`list` 直接列出 `.dial` 源码——行表、函数名、形参名、局部变量名全部来自标准 DWARF，
不需要调试器插件，也不需要 Dial 专属的 pretty-printer。

### 35.3 生成了什么

| DWARF 结构 | 来源 | 效果 |
| --- | --- | --- |
| `DW_TAG_compile_unit` | `module.create_debug_info_builder`（`DWARFSourceLanguage::C`、`DWARFEmissionKind::Full`、producer `dialc <版本>`，取自 `CARGO_PKG_VERSION` 随发版自动更新） | GDB 识别为一份标准编译单元 |
| `DW_TAG_subprogram` | 每个 `Func`，行号取 `Func.line` | `break main`、`bt` 显示函数名 |
| `DW_TAG_formal_parameter` | 函数形参，序号从 1 起 | `info args` / `print a` |
| `DW_TAG_variable` + `llvm.dbg.declare` | `Stmt::VarDecl` 的栈槽 | `print counter` |
| `.debug_line` 行表 | `gen_stmt` 入口的 `stmt_pos(stmt)` → `DILocation` | `break app.dial:9`、`list`、`bt` 带行列 |

行号必须精确：嵌套语句（if / 循环 / try）递归调用 `gen_stmt` 会把当前位置切到内层，
若不在返回时恢复，外层后续指令就会被打上内层行号。实现用 `DebugLocGuard`
（RAII，在 `gen_stmt` 返回时自动把 builder 的 `DILocation` 恢复到外层），
包括所有 `return Err(...)` 早退分支。

### 35.4 类型映射（诚实边界）

| Dial 类型 | DWARF 类型 | `print` 效果 |
| --- | --- | --- |
| `int` / `byte` / `char` | 64 位 `DW_ATE_signed` / `unsigned` | 十进制数值 |
| `float` | 64 位 `DW_ATE_float` | 浮点数值 |
| `bool` | 1 位 `DW_ATE_boolean` | `true` / `false` |
| `string` / `bytes` / `int[]` / `map` / `struct` / 句柄 / 函数指针 / 裸指针 | 指向 `dial_object` 的不透明指针 | 变量名与类型可见，值为对象地址 |

标量能直接读到值——这是「`print var_name` 而不是十六进制地址」的核心。
复合类型在 IR 里本就是不透明 `i8*`（见 §1「不透明指针」、§19 内存宪法），
C Runtime 的内部布局不对外暴露，所以 DWARF 也**如实**呈现为不透明指针，
不伪装成结构体、不承诺字段展开。想看内容仍走 Dial 自己的 `print` / `str`。

### 35.5 `-g` 隐含 O0，以及与 `--fast` 的关系

默认构建是 O2；若只加调试信息而不降优化，LLVM 会把栈槽变量优化进寄存器甚至整个消掉，
GDB 里只剩 `<optimized out>`，`print counter` 依然打不出值。因此：

- `-g` 且非 `--fast`：`OptimizationLevel::None`（O0），变量稳定留在栈槽；
- `-g --fast`：仍按用户明确的性能契约走 O3 + SIMD（+ LTO/PGO），
  编译期打印一行提示「部分变量可能显示 `<optimized out>`」，构建不阻断；
- 纯调试场景建议只用 `-g`，不要叠 `--fast`。

`-g` 只影响 LLVM 侧优化级别，**不改变** Fail-Fast 运行期检查（§20）、
`--safety` 等级、C Runtime 的 `-DDIAL_SAFETY=N` 注入，也不改变 `.dialpkg` 契约。

### 35.6 实现落点与回归

| 层 | 文件 | 改动 |
| --- | --- | --- |
| CLI | `src/main.rs` | 解析 `-g` / `--debug-info` → `CgOpts.debug_info`；`help` 与构建提示同步；`dialc test` 恒为 `false` |
| 选项 | `src/codegen.rs::CgOpts` | 新增 `debug_info` |
| 编译单元 | `codegen::compile()` | `Debug Info Version` 模块标志、`DebugInfoBuilder` / `DICompileUnit` / `DIFile`（绝对路径）、`finalize()` 在 `verify()` 前 |
| 函数 / 语句 | `begin_debug_function()` / `stmt_debug_loc()` / `DebugLocGuard` | DISubprogram、语句级 DILocation 及嵌套恢复 |
| 变量 | `debug_declare()` / `di_type()` | `dbg.declare`、Dial 类型 → DIType |
| 优化级别 | `emit_object()` | `-g` 非 `--fast` 时 O0 |
| 回归 | `tests/run_tests.sh` | 新增 **R 组** |

**R 组**用 `llvm-dwarfdump` 静态核对 `.debug_info` / `.debug_line` 段、
`add` / `main` / 形参 `a` `b` / 局部 `sum` `counter` `note` 七个名字与 Dial 源文件行表，
再用真实 `gdb -batch` 在 `.dial:9` 下断点断言 `print counter = 42` 与 `bt` 源码行；
另有无 `-g` 零调试符号的负向断言、`-g --fast` 兼容断言。环境缺 `gdb` / `llvm-dwarfdump`
时该子项明确跳过（⏭️）而非误报失败，保证 CI 不被工具缺失阻断。

> **本轮交付范围**：并入 `[v1.04]`、版本号与 `STDLIB_ABI` 均不变；只做**本地提交**，
> 不推送 GitHub、不打 tag、不发 Release。`v1.0` 标签不受影响。

---

## 36. v1.04 增补：性能基准（4 语言 × 28 维，并入 `[v1.04]`，版本不变）

一套**可复现、跨语言等价**的性能基准：同一台机器、同一套 **28 个微基准**，对比
**C / Python / Go / Dial** 四种语言；独立网页 [`docx/bench.html`](docx/bench.html)
存放全部数据，底部叠加**六维工程能力雷达图**。

### 36.1 28 个维度

28 个纯整数微基准按五类组织，全部产出**十进制校验和**：

| 分类 | 维度 | 覆盖 |
| --- | --- | --- |
| 算术循环 | b01–b08 | 依赖链累加、乘加取模、除法/取模、FMA、连乘幂、gcd、迭代 fib、Collatz |
| 递归调用 | b09–b12 | 二叉递归 fib、浅递归重复调用、二叉递归树、互递归奇偶 |
| 数组内存 | b13–b18 | 构建求和、伪随机读写、冒泡排序、原地反转、矩阵乘、前缀和 |
| 字符串 | b19–b22 | 逐字符扫描、循环拼接、`index_of`、拼接与定长子串 |
| 位与控制流 | b23–b28 | popcount、xorshift32、三路分支、嵌套异或、短路布尔、综合负载 |

四份源码在 `bench/` 下：`bench.c`（`gcc -O2`）、`bench.go`（`go build`）、
`bench.py`（CPython 解释执行）、`bench.dial`（`dialc build --fast`，O3 + LTO + SIMD）。
每个程序接受 `<任务名> [重复次数]`，只在 stdout 打印一行校验和。

> **正确性锚点**：28 项在 4 语言必须算出**完全相同**的校验和（112/112），
> 采集器内嵌真值表逐项断言。这保证「比值」比较的是等价计算，而非各写一套。
> 一致性靠三条铁律：全程 `[0, 2^63)` 且被取模/相除的值恒非负；整数除法只对正数
> （C/Go/Dial 向零截断 == Python `//`）；位运算不触碰符号位。

### 36.2 测量口径：绝对值与绝对比值

| 概念 | 定义 |
| --- | --- |
| **绝对值** | 每个 (语言, 任务) 的单次纯计算耗时（毫秒） |
| **绝对比值** | 以 **C = 1.00×** 为基准，`该语言耗时 / C 耗时`；>1 表示比 C 慢相应倍数 |

计时用**外部统一墙钟**（不引入各语言内部时钟 API 差异）：单次进程内把任务重复
`N` 次，`N` 由采集器自校准到目标时长以摊薄进程启动，扣除 `noop` 启动基线后除以
`N` 得单次纯耗时。极快项或被编译器常量折叠成空操作的项（如 C 的 `-O2` 对 b20
字符串拼接）统一钳到 5µs 计时下限，网页以 `†` 标注，避免 `0.000ms` 与比值虚高。

### 36.3 六大工程维度与雷达图

雷达图 6 轴是**工程维度**（不是 28 个 benchmark 的分类），均为「越小越好」：

| 轴 | 绝对值口径 |
| --- | --- |
| 速度 | 28 项单次耗时之和（ms） |
| 峰值内存 | b17 负载的峰值 RSS（KB，含语言运行时基线；GNU `time %M`） |
| 编译耗时 | 各语言编译档位耗时（ms；Python 记 `py_compile`） |
| 启动耗时 | `noop` 进程墙钟（ms，含解释器/运行时初始化） |
| 产物体积 | 可执行文件字节（KB；解释型 Python 为源码字节） |
| 源码行数 | 实现 28 项的有效代码行（去空行与整行注释） |

6 轴量纲不同，先按「各维最优（最小）语言 = 100、其余 = 最优/自身 ×100」归一化，
再叠加 4 语言多边形（**越靠外越优**）。归一化得分与原始绝对值都在网页列出，
避免只看雷达面积导致的误读。

### 36.4 网页与复现

独立网页 [`docx/bench.html`](docx/bench.html) 为**纯静态 HTML**（自带表格与手写 SVG
雷达图，无外部 CDN、无 JS 依赖，离线双击可看）。它不由 `tools/build_web.py` 生成，
因此不影响「官网产物同步校验」；官网导航、首页 CTA 与安全公告页均已加入口。

```bash
# 采集成 JSON（自动编译 4 语言、校验 112 校验和、采 6 维）
python3 tools/bench_harness.py --dialc target/release/dialc
# 由 bench/data.json 生成网页
python3 tools/build_bench_web.py
# 校验数据完整性与网页/数据同步（CI 与本地共用）
python3 tools/check_bench_data.py
```

数据文件 `bench/data.json` 是本地校准后的**权威快照**，随仓库提交。
CI 的 `build-linux` 作业每次都会实跑采集器、校验 112 校验和与交付快照配套，
把 CI 机器上的实测数据与网页作为 artifact（`bench-4lang-28dim`）上传；
CI 产物只写 `/tmp` 再上传，**绝不回写** `docx/bench.html` —— 否则测得的数据与
提交的快照不同会让「官网同步校验」的 `git diff --exit-code -- docx/` 误报。

### 36.5 实现落点

| 层 | 文件 | 职责 |
| --- | --- | --- |
| 基准源码 | `bench/bench.{c,go,py,dial}` | 28 项等价实现 + `repeat` 自校准入口 |
| 采集器 | `tools/bench_harness.py` | 编译计时、校准法计时、校验和真值、6 维采集、JSON |
| 网页生成 | `tools/build_bench_web.py` | 绝对值/比值表、六维表、SVG 雷达图 |
| 校验器 | `tools/check_bench_data.py` | 数据完整性 + 网页/数据同步（CI 复用） |
| 数据 | `bench/data.json` / `docx/bench.html` | 提交的权威快照 / 独立性能页 |
| CI | `.github/workflows/ci.yml` | `build-linux` 内实跑 + artifact |

> **本轮交付范围**：并入 `[v1.04]`，编译器版本 `1.4.0` 与 `STDLIB_ABI 5` 均不变；
> 只做**本地提交**，不推送 GitHub、不打 tag、不发 Release。性能数据随机器与负载浮动，
> 网页是**某次实测快照**而非固定承诺值；复现以 `tools/bench_harness.py` 为准。

---

## 37. v1.04 增补：真实程序缺口补齐与宪法裁决（并入 `[v1.04]`，分 3 批）

> 来源：`REPORT_v1.04_gaps.md`——按「普通程序员写日常程序」的思路写 54 个小程序 +
> 27 个探针后整理的 B/C 组缺口清单。本章是**规范与裁决**，实现按批次落地，
> 每批独立跑 A–T 全量回归，不破既有 403 ✅。

### 37.1 术语统一：绝不引入可变全局状态

v1.04 之前的文档写「绝不引入全局变量」。本轮按裁决把 README 与官网规范中的
该措辞**统一替换为**「**绝不引入可变全局状态**」——被禁止的是**可变共享状态**
（数据竞争的根源），而非只读常量数据。新表述在 v0.61 等历史语境下同样成立
（当时本就没有任何全局状态），因此历史条目一并替换、不改写史实；§24 保留
「原写作…」的沿革注记，便于读者对照。

### 37.2 两条永久否决（写进宪法，不是"尚未实现"）

| 否决项 | 理由 |
| --- | --- |
| **函数名重载（B5）** | C++ 式名字改编让 ABI 随签名集合**隐式漂移**；Dial 的契约是 `STDLIB_ABI` **显式整数版本号 + 链接期拦截**，二者不可调和。同名函数继续按「重复定义」编译期拒绝 |
| **闭包 / 隐式环境捕获（B4）** | 与「零隐藏开销」「确定性内存」相悖；回调状态必须显式经 `rawptr<void>` 透传（v1.03 既有铁律，本轮正式入宪） |

**与泛型单态化的边界**：B2 泛型若采用单态化，编译器**内部**会为每个实例生成带后缀的符号，
但那是 IR 私有符号，**不导出到 `.diali` 契约、不改变用户可见的函数名、不影响包 ABI**。
它与被否决的「用户可见同名函数重载」是两件事，不构成对 B5 的破例。

### 37.3 B7：文件级 `const` 的精确语义（第 3 批落地）

按裁决，**绝对禁止任何 `var` 声明的全局变量**（严防数据竞争）；只允许文件级 `const`：

- 值必须是**编译期常量**（标量字面量、数组字面量、已定义的 const 引用）；
- 它是**不可变的只读数据**，编译期降维到 LLVM `.rodata` 段或按需内联；
- **不参与运行期可变状态管理**，不需要 ARC，不引入后台线程或全局构造顺序问题；
- 与 §19 内存宪法完全兼容：只读常量不是「共享可变状态」，无环、无悬垂、无停顿。

### 37.4 B8：结构体字段默认值的精确语义（第 3 批落地）

按裁决，允许在 `struct` 定义中为字段指定**编译期常量默认值**（如 `x: int = 0`）：

- 语义上**依然禁止隐式未初始化内存**：用户没填某字段且该字段**没有显式默认值**，
  仍然编译期报错（保留 §1「比 C 更严格」的强类型与确定性底线）；
- 默认值必须显式写在定义里，调用方一眼可见——这与 C 的「剩余字段静默零初始化」有本质区别；
- §1 中「禁止省略结构体字段」的措辞随之修订为「禁止省略**无默认值**的字段」。

### 37.5 C11：常用库走独立包，不进标准库包

正则 / JSON / 摘要 / Base64 / HTTP 等**不做标准库内建**，改为独立 `.dialpkg`
（如 `http.dialpkg`），通过 `import "http";` 使用。理由：

- 与 §21.2「不引入无用依赖」一致：这些能力各自带平台差异与第三方实现，
  塞进 `stdlib.dialpkg` 会让每个 Dial 程序都背上它们；
- 与 §25 包分发铁律一致：包独立发版、独立校验 ABI / SHA256；
- 语言层 FFI（§33）已提供接 C 库的能力，官方封装只是「打包好的便利」，不是语言特性。

### 37.6 标准库 ABI 递增：5 → 6

`STDLIB_ABI 5 → 6`（`src/package.rs`）。判据沿用既有约定：编译器**可能要求旧包没有的符号**
即递增。本轮新增 `dial_eprint_*` stderr 输出族、`dial_map_get_or` /
`dial_kvmap_get_or`，以及后续批次的 C 组 `dial_*` 导出。装 abi=5 旧包的用户一旦写
`eprint(...)` 就会撞上 `undefined symbol: dial_eprint_int`，因此递增到 6，
让旧包在链接期被**明确拦下并给出重装指引**。`make_stdlib_pkg.py` 默认 `--abi 6`、
CI 两处 `--abi 6` 同步；`dialc new` 骨架的 `abi` 改为引用 `package::STDLIB_ABI`
（消除硬编码，避免以后递增时再次漂移）。

### 37.7 第一批已落地（本轮交付）

| 编号 | 能力 | 实现要点 |
| --- | --- | --- |
| B9 | 块注释 `/* ... */` | lexer 跳过，跨行同步行号；**未闭合直接 Fail-Fast 报错**，绝不静默吞源码 |
| B10 | `!` 一元逻辑非 | 词法层复用 `TokenKind::Not`，`parse_not` / 语义 / IR 全链路零改动；`!=` 分支优先匹配不受影响 |
| B11 | `a.slice(from, to)` | 与切片语法 `a[from..to]` 共用 `dial_array_slice`，返回新数组，不改接收者 |
| C6 | `get_or(k, default)` | 普通 map 与泛型 map（标量 V）一次探测，把「未命中」与「真实值 == 默认值」彻底分开；句柄 V 由容器类型系统更早拦截，`get_or` 的拒绝分支是纵深防御 |
| C8 | `eprint` / `eprint_raw` | stderr 输出族，与 `print` 逐类型对称；**不引入可变全局状态**（无「当前输出流」开关、无全局 `FILE*`），四个分派函数各自独立（§21.1 宁可 WET） |

C8 配套的 Fail-Fast 加固：输出族内建（`print` / `print_raw` / `write_char` /
`eprint` / `eprint_raw`）在语义层按**名字**硬编码分派，此前未进入 `RESERVED_NAMES`，
用户若定义 `fn eprint(...)` 会编译通过却在调用点被内建分支**静默绕过**（§20 严禁的
静默损坏）。本轮把这 5 个名字补进保留名清单，同名定义编译期直接拒绝；既有调用不变。

T 组回归锁定：stdout/stderr 严格分流、块注释未闭合 Fail-Fast、
`get_or` 类型严格校验、函数重载（B5）仍被拒绝、输出族内建同名定义被拒。

### 37.8 第二批已落地（C 组标准库 API 全量）

按 §37.5「C11 走独立包」的裁决，本批只做**标准库内建**——语言层 FFI（§33）已能接 C 库，
正则 / JSON / 摘要 / Base64 / HTTP 留给 C11 独立包，不进 `stdlib.dialpkg`。
全部落在 `stdlib/*.c`（C Runtime）+ `src/semantic.rs`（方法 / 内建签名）
+ `src/codegen.rs`（`declare_stdlib` / `call_c` 分派）+ `stdlib/stdlib.diali`（自描述）四层。

| 编号 | 能力 | 实现要点 |
| --- | --- | --- |
| C1 | 字符串大小写 / 子串 | `to_upper` / `to_lower` / `contains` / `starts_with` / `ends_with`（逐类型安全，UTF-8 字节级） |
| C4 | Unicode 码点 | `char_count` / `chars()` / `char_at(i)` / `char_slice(from,to)` —— **按 UTF-8 码点**，不是字节；`length()` 保持字节语义不动，避免破坏既有 416 项回归 |
| C2 | 数值严格解析 | `parse_int` / `parse_float` 非法输入 **Fail-Fast**（绝不静默返回 0）；`is_int` / `is_float` 显式探测 |
| C3 | 类型安全格式化 | `format_float(v: float, digits: int)` —— 拒绝 printf 式运行期格式串（§5.3 零隐式转换），`int` 参数编译期被拒 |
| C5 | 数组 API 扩充 | `insert` / `index_of`（标量元素；`string[]` 因位模式歧义被拒）/ `reverse` / `reduce`（复用 v1.03 函数指针 + `rawptr<void>`，无闭包）/ `join` / `sort` 放宽到 `string[]` 字典序 / `clear` |
| C7 | 文件 IO | `file_exists` / `file_size`（打不开 `-1`）/ `file_remove` / `file_read_all`（FNF→抛 Error）/ `file_read_line(f)`（逐行）/ `eof(f)`；刻意用 `file_read_line` 而非 `read_line(f)`，避免与 stdin 版 `read_line()` 撞名（B5 否决重载） |
| C9 | 环境 | `has_env` / `getenv`（缺失返回空串）/ `program_name`（argv[0] 只读快照，非 `var` 全局）/ `cwd` |
| C10 | 时间 | `local_parts()`（int[8]，自取分量）/ `date_string()`（固定 `YYYY-MM-DD HH:MM:SS`）/ `monotonic_ms()` —— 拒绝 printf 式格式串，组件由调用方显式拼（§5.3） |

C2/C7 配套 Fail-Fast 加固：第 2 批 18 个内建名（`parse_int` / `parse_float` / `is_int` /
`is_float` / `format_float` / `read_all` / `eof` / `file_read_line` / `file_exists` /
`file_size` / `file_remove` / `getenv` / `has_env` / `program_name` / `cwd` /
`local_parts` / `date_string` / `monotonic_ms`）全部补进 `RESERVED_NAMES`，
同名定义编译期拒绝（延续第 1 批 `eprint` 族加固，杜绝内建分支静默绕过 §20）。

U 组回归锁定：8 组 API 端到端输出断言、C2 解析失败 Fail-Fast、C4 `char_at` 越界 Fail-Fast、
`string[].index_of` 编译期拒绝、C3 `format_float(int,..)` 零隐式转换拒绝、第 2 批内建同名定义被拒。

> **本轮交付范围**：并入 `[v1.04]`；编译器版本 `1.4.0` 不变，`STDLIB_ABI 5 → 6`；
> 只做**本地提交**，不推送 GitHub、不打 tag、不发 Release。
> B1/B2/B3/B6/B7/B8 与 C11 按 §37.3–37.5 的裁决在后续批次落地，
> 届时同步更新本章「已落地」表——**规范先于实现写清，但绝不把未实现项标成已支持**。

### 37.9 第三批已落地（语言机制：B7 / B8 / B6）

本批落地 §37.3–37.4 的 B7 / B8 与 B6（结构体进容器）。实现分布在
`src/parser.rs`（语法与 const 降维）、`src/semantic.rs`（类型与白名单）、
`src/codegen.rs`（装箱 / 解箱与保留名）、`runtime/dial_rt_mem.c`（装箱分配）、
`stdlib/dial_kvmap.c`（map 值种类）五层。

| 编号 | 能力 | 实现要点 |
| --- | --- | --- |
| B7 | 文件级只读 `const` | 标量 / 字符串 / 数组 / 嵌套数组 / 字符串数组 / 结构体字面量，以及字面量上的常量表达式（可常量折叠）；const 可引用其他 const（解析期内联展开）；**顶层 `var` 全局变量编译期拒绝**（§37.1 红线：禁止可变全局状态）；const 重赋值 / 非编译期常量值 / 类型不匹配 / 局部变量遮蔽 const 均被拒 |
| B8 | 结构体字段默认值 | `struct Point { x: int = 0, y: int = 0 }`；默认值必须是**编译期常量**；省略有默认值的字段取默认值、省略无默认值的字段**编译期报错**（不静默零初始化）；默认值类型不匹配按零隐式转换铁律拒绝 |
| B6 | 结构体进容器 | `Point[]` 与 `map<string, Struct>`：值类型结构体装不进 8 字节数组槽，故**装箱**为引用计数对象（`[int64 refcount][payload]`）后入槽，读出时**解箱按值**拷贝；`put` / `get` / `contains` / `remove` / `get_or` / `values()` 全通路打通 |

**B6 的现役边界（如实标注，不夸大）**：本批只放开「**字段全为标量**」的结构体。
含句柄字段（`string` / 数组 / map 等）的结构体进容器，需要装箱与释放时**逐字段**
retain/release（字段级 ARC），该能力尚未落地，故语义层**明确拒绝**并给出可读诊断——
而不是让它「看起来能用」却在释放时泄漏或悬垂。这条边界同时写进了 `README`、
`ARCHITECTURE §14.6` 与 V 组回归用例（`tests/bad/struct_array_handle_field.dial` /
`tests/bad/struct_map_handle_field.dial`）。

**B6 顺带修好的通用缺陷**：字段访问此前只支持「变量 / 字段链」作为基对象，
`Pt{}.x` / `make().y` / `arr[i].f` 一律报「仅支持对变量/字段进行赋值」。
本批让 `gen_expr` 的字段分支把非 lvalue 基**物化到栈上临时槽**再取字段指针
（`gen_lvalue_ptr` 保持只服务数组元素写，避免破坏 `a[i] = v` 的写回语义）。
B7 的 const 结构体内联后「取字段」成为常态路径，这条修复因此是 B7/B6 的必要前提。

**B7 顺带修好的通用缺陷**：`const` 的值在解析期内联为字面量，因此
`const ORIGIN: Point = Pt{};` 之后 `ORIGIN.x` 的基对象是结构体字面量——
正是上面那条字段访问修复的用武之地。

**V 组回归**（新增第 22 组）：B8 正向、B7 正向、B6 正向（`Point[]` 与
`map<string, Point>` 端到端）、顶层 `var` 被拒、省略无默认值字段被拒、
默认值类型不匹配被拒、含句柄字段结构体作数组元素 / map 值被拒。
B6 内存专项：3M 次 `Point[]` 建放、1M 次 `map<string,Point>` 读写循环，
均在虚拟内存上限内通过（装箱对象随容器释放，无泄漏、无重复释放）。

**本批明确不碰（按裁决划清界限）**：B1（`impl` 方法——暂用 `fn` 替代）、
B2（用户泛型——死守单态化底线，不做）、B3（枚举 ADT——不引入 Tagged Union，
保持类型系统极简）、C11（`http.dialpkg` 独立包，下一批）。
B5（函数名重载）与 B4（闭包）维持**永久否决**。

> **沿革注记（v1.10 开发中增补）**：上面「本批明确不碰」是 v1.04 第三批的
> **历史裁决**，如实保留。其中 **B1（`impl` 方法）已在 `[v1.10]` 发布之后的
> 开发中增量里落地**，见 [§37.12](#3712-b1impl-方法块v110-开发中增补落地)；
> B2 / B3 / C11 的裁决不变（C11 independent 包随后按 §37.10–37.11 落地）。

### 37.10 C11 第一包已落地：`http.dialpkg`（HTTP/1.1 + TLS 1.3 + HTTP/2）

§37.5 定了「常用库走独立包、不进标准库包」，并按领域拆包。本节记录**第一个包**
`http` 的完整决策；同批补齐的 `text` / `json` / `crypto` 三包见 §37.11。
四包共用同一套 FFI 约定、三平台构建口径与 `.dialpkg` 分发方式。

| 决策 | 落地方式 |
| --- | --- |
| **协议层自研** | `packages/http/src/dial_http.c`：URL 解析、请求行与 Header 拼装、响应报文解析（状态行 / 头部 / `Content-Length` / `chunked` / 读到关闭）、Socket 读写，全部自己实现。线级细节留在自己手里，才能为后续的连接指纹定制留出空间 |
| **密码学绝不手搓** | TLS 由 **mbedTLS** 提供并已接线上层：`https://` 走 mbedTLS，证书校验 **fail-closed**（系统根 + 自定义 CA 两条信任链，自签名不可冒用；Termux 上系统根另读 `$PREFIX/etc/tls/cert.pem`）；gzip / deflate 解压由 **zlib** 提供。协议层自己写，密码学与压缩算法一律用成熟实现 |
| **两层 API** | 底层「流式」：`connect` / `write` / `write_line` / `read_line` / `read_all` —— 调用方自己拼报文；高层「一次请求」：`get` / `post` + `HttpResponse` 访问器（状态码 / 响应体 / 响应头 / 按序号遍历） |
| **HTTP/2 自研分帧 + HPACK** | `packages/http/src/dial_http2.c` / `dial_hpack.c`：9 字节帧头与 8 类帧、HPACK（请求侧最简编码，响应侧静态表 61 项 + Huffman 解码 257 项 + 动态表）、会话式真·并发多路复用。`https://` 走 ALPN 协商 `h2`（未协商出 h2 **拒绝降级**），`http://` 走明文 **h2c** prior-knowledge |
| **错误走返回码** | 不抛 Dial 异常（包不假定宿主装了异常栈）：失败返回空句柄（`is_null` 可判），原因取 `dial_http_last_error()` / `dial_http_last_error_msg()`（**线程局部**错误槽，多线程各看各的） |
| **句柄不进 ARC** | `HttpConn` / `HttpResponse` / `Http2Session` 是不透明句柄，释放必须显式（`dial_http_close` / `dial_http_resp_free` / `dial_http2_session_free`）——与 stdlib 的文件句柄同一口径 |
| **离线可复现** | mbedTLS 3.6.2 与 zlib 1.3.1 **vendor 进仓库**（`third_party/`），保留上游 LICENSE 与版权声明；构建不依赖外网，上游变动也不会悄悄改掉产物 |

**为让包能引用标准库 API 而修的链接顺序**：静态归档由链接器**从左到右**扫描，
符号只有在其归档被扫描时仍处于「未定义」状态，对应成员才会被拉进来。原顺序是
`obj → runtime → stdlib → packages`，于是包**无法**引用标准库符号——而包要向 Dial
返回 `string` / `bytes`，就必须构造带引用计数头的堆对象（`dial_string_new` /
`dial_bytes_new`），必然要引用标准库。改为 `obj → runtime → packages → stdlib`：
包先于标准库被扫描，标准库从不反向引用包符号，次序安全。

**写包时踩到的两个 FFI 约定坑（已写进包内注释，供后续包作者参考）**：

1. Dial 的 `string` 跨到 C 时传的是**载荷指针**（布局 `[int64 len][bytes...]`），
   **不是字节首地址**——标准库因此才用 `dial_string_cstr()` 取文本（载荷 + 8）。
   把收到的字符串直接当 `const char *` 用，host 就会带上前导的长度字段。
2. 读响应体必须**先取连接的读缓冲**再读 socket：行读取可能已经把整份响应吞进缓冲
   （服务器发完就关闭时，一次 `recv` 就把响应全读了进来），若直接去 socket 读，
   缓冲里的 body 会被漏掉，而 socket 那边已关闭 → 误报「连接提前关闭」。

**一处刻意不动语言的地方**：Dial 的字符串字面量只支持 `\n \t \\ \"` 四种转义，
**不支持 `\r`**（README §字面量表），而 HTTP 的行结束符恰恰是 CRLF。为发一个 HTTP 包
就顺带改语言不合适——因此包里提供 `dial_http_write_line`，由它补 CRLF：
既贴合 HTTP 语义，又不动语言。

**W 组回归**（新增第 23 组）：三平台静态库构建、打包 + 安装（manifest / 逐文件
SHA256 自检）、`import "http"` 编译运行、本地 HTTP 服务器端到端
（200 / 404 / POST 回显 / chunked / gzip 解压 / 多响应头含同名多值 / 低层流式）、
明文三条错误路径（https 连不上 / URL 缺主机名 / 连接失败）必须**明确失败**；
**TLS 1.3 子组**（同组第 7 步）：自签 CA + 本地 TLS 服务器，验证 TLS 1.3 协商、
证书校验 fail-closed（清空自定义 CA 后系统根拒签自签）、keep-alive 连接池复用
（`pool hits > 0`）、404 / 500 错误路径；
**HTTP/2 子组**（同组第 9 步）：Python `h2` 本地服务器（h2c 明文 + h2 TLS/ALPN）
**故意乱序响应**，6 个 `spawn` worker 共享**单条连接**并发提交 6 个 stream，
逐条断言 `path == X-Stream`（不串台）、`conns = 1` / `streams = 6`、
`h2` ALPN 协商成功、fail-closed 拒签自签 CA、404 / 500 全绿。

**Fail-Fast / 可用性回归**（v1.10 开发中增补，同组 +4 条断言）：
① `/chunked_bad` 长度行 `xyz` 必须明确报 `DIAL_HTTP_ERR_PROTOCOL(-5)` 并把坏行写进
错误槽，`/chunked_ext` 的 `1a;ext=1` 合法扩展必须解出完整 body；
② Termux 目标平台：把自签 CA 装成 `$PREFIX/etc/tls/cert.pem` 后，不设自定义 CA
也能真实握手成功（`verify=0`），空 CA 目录则仍 fail-closed 明确拒绝；
③ HTTP/2 发 1200 字节超长额外头，服务端按实际长度回显 `X-Long-Len=1200`
（旧 `char buf[512]` 会静默截到 511 字节）。H2 侧额外头名按 RFC 7540 统一小写化，
H1 侧大小写不敏感、保持原样。四处修复均**不改 `.diali` 对外签名与 `abi = 6`**。

**客户端画像（兼容性，非规避引擎）**：`dial_http_set_profile("default" | "chrome")`
只控制请求头序与 User-Agent 这类兼容参数（与只认浏览器客户端的 API 互通），
**不做以「绕过服务器限制 / 欺骗识别」为目标的绕检测逻辑**——这是有意的领域边界。

**后续（尚未落地，不许当已支持）**：HTTP/3.0（QUIC，独立 `http3.dialpkg` +
vendor ngtcp2/nghttp3）。`text` / `json` / `crypto` 三包已落地，见 §37.11。

### 37.11 C11 其余三包已落地：`text` / `json` / `crypto`

§37.5 把正则 / JSON / 摘要 / Base64 拆成独立包，`http` 之外这三包同批补齐。
三包都是 `version = 1.0.0` / `abi = 6`，三平台静态库由 `zig cc` 交叉编译、
零警告零错误；CI 阶段 8 与 `stdlib.dialpkg` 同一口径，把包版本跟随编译器版本。
回归分别是 **X / Y / Z 组**（当时全量 A→Z 二十六组 `EXIT=0`；当前全量已扩为
A–Z + AA + AB **二十八组**，见 §38）。

| 包 | 实现路线 | 关键决策与边界 |
| --- | --- | --- |
| `text.dialpkg` | **自研 Thompson NFA + Pike VM**（`packages/text/src/dial_regex.c`），零第三方依赖 | 线性时间，从根上避开回溯爆炸。支持字面量 / `.` / 字符类 / `\d \w \s` 等 / `* + ?` / 分支 `\|` / 捕获组 / `^ $ \b`；**不做**后向引用、lookaround、懒惰量词、`{m,n}`、Unicode 属性类。按字节匹配，捕获组最多 9 个。`Regex` 句柄缓存最近一次匹配结果，**不要跨线程共享**同一句柄的 `find/is_match` 与 `group`。已知语义差异 `(a*)*b` 组 1 取 `"aaa"`（见 `text.diali`） |
| `json.dialpkg` | **自研递归下降解析 + 紧凑序列化**（`packages/json/src/dial_json_codec.c`），零第三方依赖 | `.diali` 无泛型容器，值树封在 `type Json;`。**所有权唯一规则**：返回 `Json` 的函数都给独立拥有的句柄，一律 `dial_json_free`；`array_push` / `object_set` 深拷贝 `child`。严格模式拒尾随逗号 / 多余根值 / 非法 `\u` / 孤立代理项并给字节偏移，深度上限 64，拒绝 `\u0000`；整数走 `int64_t`（>2^53 不丢精度），浮点走 `double` |
| `crypto.dialpkg` | **复用 vendor mbedTLS 3.6.2**，只做 FFI 包装 | 五种摘要走 `mbedtls_md()`、HMAC-SHA256 走 `mbedtls_md_hmac()`、Base64 走 `mbedtls_base64_*()`——**密码学绝不手搓**。MD5 / SHA-1 明确标注仅用于校验与兼容。`string` / `bytes` 双入口，`dial_crypto_digest` 返回原始摘要字节。失败返回空值 + 线程局部错误码；`bytes` 类返回长度 0 的空缓冲而非 NULL（`is_null` 不适用于 `Type::Bytes`） |

**三包共同的 FFI 约定**（与 `http` 包一致，写进各包 `src/*.h` 注释）：

1. `string` / `bytes` 跨到 C 都是 **`DialString` 句柄**（内存布局相同：
   `[int64 len][data...]`），取数据用 `dial_string_cstr()` + `dial_string_length()`，
   **长度优先于 NUL**——`bytes` 允许内嵌 `\0`；把句柄当 `const char *` 会读到
   前导长度字段。
2. 返回 `string` 必须构造 `dial_string_new()` / `dial_string_from_bytes()`，
   返回 `bytes` 必须构造 `dial_bytes_new()`，**绝不返回裸 `const char *`**。
3. `.diali` 的 `extern fn` 必须**单行写完整**：解析器 `parse_diali` 逐行解析，
   跨行声明会让末尾逗号被 `rsplit_once(':')` 误认成返回类型（HTTP/2 阶段踩过）。
4. 不透明句柄（`Regex` / `Json`）**不进 ARC**，必须显式 free；包不抛 Dial 异常，
   错误走返回码 + 线程局部错误槽。

**为什么 `crypto` 把 mbedTLS 全量归档**：本仓库 `mbedtls_config.h` 打开了
`MBEDTLS_PSA_CRYPTO_C` 与 `MBEDTLS_RIPEMD160_C`，`md.c` 会引用 `psa_*` /
`ripemd160` 符号，只编摘要子集会在链接期缺符号。静态库由链接器**按需抽取**，
全量归档只让 `.dialpkg` 体积略增（约 6 MB），不会让用户可执行文件变大。

**能力边界（不许当已支持）**：`text` 的正则子集、`json` 的 `\u0000` 拒绝、
`crypto` 只提供摘要 / Base64 / HMAC（**没有对称加密、非对称加密、签名、
密钥派生、随机数**）——都在各自 `.diali` 里逐条写明。HTTP/3.0 / QUIC 仍未落地。

### 37.12 B1：`impl` 方法块（v1.10 开发中增补落地）

> v1.04 第三批把 B1 记为「明确不碰——暂用 `fn` 替代」（见 §37.9 沿革注记）。
> 本节记录 `[v1.10]`（标签 `v1.1`）发布**之后**的开发中增量把它真正落地：
> 编译器版本仍为 `1.10.0`，**未升版本号、未打标签、未发 Release**
> （见 CHANGELOG `[未发布]`）。它是**语言层语法糖**，不碰任何 `.diali` /
> `STDLIB_ABI` / C Runtime，**ABI 不变、旧签名一字不动**。

**语法**（不新增关键字——`impl` 与 `test` 块同例，用 `Ident("impl")` + 类型名 +
`{` 识别，旧程序把 `impl` 当变量名 / 函数名依旧合法）：

```dial
struct Point { x: int, y: int }

impl Point {
    fn sum(self): int { return self.x + self.y; }
    fn add(self, k: int = 10): int { return self.sum() + k; }
    fn scaled(self, k: int): Point {
        return Point { x: self.x * k, y: self.y * k };
    }
}

fn main(): int {
    var p: Point = Point { x: 3, y: 4 };
    print(p.sum());            // 7
    print(p.add());            // 17（默认实参 k=10）
    print(p.scaled(2).sum());  // 14
    return 0;
}
```

**给包不透明句柄挂方法（「继续完善 GUI」的核心诉求）**：`GuiWindow` / `GuiMenu`
等 `type X;` 句柄也能挂 impl 方法，于是 `win.draw()` 就是 `dial_gui_window_draw(win)`
的面向对象写法——**包契约一个字都不用改**：

```dial
import "gui";
impl GuiMenu {
    fn add(self, id: int, txt: string): int {
        return dial_gui_menu_add_item(self, id, txt);
    }
    fn count(self): int { return dial_gui_menu_count(self); }
}
fn main(): int {
    var m: GuiMenu = dial_gui_menu_new();
    m.add(100, "File");       // 等价 dial_gui_menu_add_item(m, 100, "File")
    print(m.count());
    dial_gui_menu_free(m);
    return 0;
}
```

**实现（最小侵入、零重复）**：函数签名表是唯一的 `HashMap<String, FuncInfo>`，
普通 `Expr::Call` 链路（签名校验 / 默认实参 / 实参求值 / C ABI 宽度适配 /
`build_call` / 返回值）已完整可复用，因此不重写任何一条：

| 环节 | 做法 |
| --- | --- |
| parser | `impl T { fn m(self, ...) }` 就地降级为普通顶层 `Func`，符号名 mangle 成 `__impl_T_m`，`self` 注入为**首参**（类型 = `Struct(T)` 或 `Opaque(T)`，由 `impl_receiver_name` 判定）；`self.field` 天然是 `Expr::Field` |
| 语义 `type_of` | `obj.m(args)` 按 obj 静态类型拼出 `__impl_T_m`；命中则做**面向用户**的参数个数 / 零隐式转换校验并给出 `T.m` 视角诊断（不含 mangle 名、不把 self 计入参数）；未命中 Fail-Fast「类型没有该方法」 |
| 代码生成 `gen_method_call` | 用 `make_impl_call` 把 Method 就地改写为 `Expr::Call(__impl_T_m, [obj]+args)` 并**递归** `gen_expr`，复用整条调用生成链；`self` 由 `gen_function` 的形参绑定自动落到栈槽 |
| 其他 pass | **保持 Method 形态、不做全局 AST 脱糖**：`check_moved_expr` / 引用分析 / `desugar_expr` 对 obj 与 args 的递归遍历天然适用，零改动 |

因为 `obj.m(args)` 与手写的 `__impl_T_m(obj, args)` 完全同构，**编译产物与 ABI
逐字节不变**；`funcs` / `func_values` 两张表按名字自动登记方法，`analyze` 的
重复定义校验会在极端撞名时 Fail-Fast。

**容器特例收窄**：`push` / `set` / `put` / `add` 同时是内建容器的 COW 原地写回
协议名。codegen `gen_stmt` 与 semantic `check_stmt` 用 `is_impl_receiver` 把这套
特例**精确排除具名类型接收者**——否则一个恰好叫 `push` 的 impl 方法会被误当成
容器方法、把返回值错误 `build_store` 回变量槽。内建容器方法行为一字未变。

**现役边界（如实标注，不夸大）**：

- 只支持**具名类型**（用户 `struct` = `Type::Struct`、包 `type X;` = `Type::Opaque`）；
  标量 / 内建容器 / 裸指针 / `extern struct` 不挂 impl（它们的方法仍走各自白名单）。
- `self` 是**按值传入的副本**（Dial struct 值语义）：方法内 `self.f = ...` 只改副本、
  **不回写调用者**，与 Rust `&mut self` 不同；要「改原对象」请返回新结构体再赋值。
  包不透明句柄（`Opaque`，IR 是不透明指针）天然作用于原对象。
- 支持默认实参、普通形参、返回任意类型、方法间调用 `self.m()`、返回结构体再调方法；
  **暂不支持**同类型变参 `...xs`（脱糖阶段无法按 impl 符号名打包尾部实参，
  parser 直接 Fail-Fast 并提示改用 `xs: T[]`）、C ABI 导出、关联函数（无 self 的
  `T::new()`）、对同一类型重复/空 impl 块（编译期拒绝）。
- `push` / `set` / `put` 等名字可与内建容器方法同名，语义由**接收者类型**区分，
  不会互相污染。

**AC 组回归**（新增第 29 组）：`tests/impl/impl_struct.dial`（self 字段读写 /
方法间调用 / 返回结构体 / 表达式位置 / `impl` 作普通标识符 / 容器 `push` 隔离 /
值语义边界）、`impl_default.dial`（默认实参）、`impl_gui_handle.dial`（包不透明
句柄 `GuiMenu` 方法，无 DISPLAY 纯内存逻辑）、`tests/bad/impl_*.dial` 六个 Fail-Fast
负向（未知方法 / 参数类型 / 参数个数 / 缺 self / 重复方法 / 空块）+ 诊断面向用户。
全量回归 **538 ✅ → 549 ✅ / 0 ❌**，A–Z + AA + AB + AC 二十九组全绿。
（本节记录 B1 落地时的验收快照；其后 GUI 第 8 层 OO 封装新增 AD 组、第 14 层
窗口级覆盖层自动集成新增 AA 组 3 条断言、第 15 层快捷键 / 命令系统新增 AA 组
2 条断言、`fmt` / `pack` 强化再新增 Q 组 11 条断言，当前全量
**612 ✅ / 三十组**（`run_tests.sh` 实测；§34.10 记 587 ✅ 为按组累加口径，
586 ✅ + 19 = 605 ✅，LSP 再强化 +7 = 612 ✅），见 §25.11 / §28.7.2 / §34.2 /
§34.3 / §34.5 / §34.10 / §34.11 / §38.14 /
§38.15 / §38.16 / §38.17 / §38.18 / §38.19 / §38.20 / §38.21。）

---

## 38. 手搓 GUI 地基包：gui.dialpkg

> **定位与交付口径**：本章描述 `packages/gui/` 产出的 `gui.dialpkg`——一个
> **不用 Qt / GTK / SDL、纯 C 手搓**的 GUI 包，覆盖**七层 C 契约 + 第 9 层滚动视图 +
> 第 10 层右键上下文菜单 + 第 11 层悬浮提示 + 第 12 层状态栏 +
> 第 13 层通知气泡 + 第 14 层窗口级覆盖层自动集成 +
> 第 15 层快捷键 / 命令系统、共 279 个 `extern fn`**，
> 其上是**第 8 层 Dial 侧 OO 封装层**
> （不新增 `extern fn`，见 §38.14），第 9 ~ 15 层同属 C 契约
> （见 §38.15 ~ §38.21）：
> ① **地基层**（窗 + 事件 + 绘图 + 字体，55 个 `extern fn`）；
> ② **控件框架层**（`GuiWidget` 树 + 布局 + 命中测试 + 主动事件分发，
> 28 个 `extern fn`，见 §38.2 / §38.9）；
> ③ **成品控件外观层**（Label / Button / CheckBox + 内置浅色默认主题 +
> 四态绘制 + 纯值事件推进，12 个 `extern fn`，见 §38.2 / §38.9）；
> ④ **输入层**（TextBox 单行编辑 / 光标·选区 / 焦点链 / 统一功能键码与修饰键 /
> XTest 实窗事件注入，14 个 `extern fn`，见 §38.2 / §38.10）；
> ⑤ **主题层 / 控件扩展**（14 个可覆写颜色角色 + Radio / Slider / ProgressBar
> 三种成品控件 + value / range / group 纯值访问器，11 个 `extern fn`，见 §38.2）；
> ⑥ **信号槽 / 控件事件订阅**（6 类可订阅纯值信号 + 每树环形队列 +
> subscribe / pending / data / pop / clear / dropped，10 个 `extern fn`，见 §38.12）；
> ⑦ **菜单 / 对话框覆盖层**（窗口级顶栏菜单 + 弹出菜单 + 模态对话框，复用第 6 层
> 纯值信号队列：`SELECTED` / `SUBMITTED` 两类信号 + `dial_gui_menu_*` /
> `dial_gui_dialog_*` / `dial_gui_window_*` 统一分发，24 个 `extern fn`，见 §38.13）；
> ⑧ **Dial 侧 OO 封装层**（`gui_oo.dial` 用 **278 个 `impl` 方法**包住上述 279 个
> C 风格函数，`win.draw()` 等价 `dial_gui_window_draw(win)`；**纯 Dial 源码、随包
> `src/` 分发（装包即用）、零 `extern fn` 增量**，见 §38.14）。
> ⑨ **滚动视图层**（`dial_gui_scroll_*`，视口裁剪 + 滚动条几何 / 命中 / 绘制 +
> 偏移推进 wheel / drag / scroll_to；纯内存对象 + 离屏画布，零 DISPLAY 依赖；
> 15 个 `extern fn`，见 §38.15）；
> ⑩ **右键上下文菜单层**（`dial_gui_context_*`，复用第 7 层 `GuiMenu` + 弹出锚点 +
> 命中 / 悬停 / 选中 / 绘制；借用语义不转移菜单所有权；纯内存对象 + 离屏画布，
> 零 DISPLAY 依赖；15 个 `extern fn`，见 §38.16）；
> ⑪ **悬浮提示层**（`dial_gui_tooltip_*`，文本 + 位置 + 尺寸 + 悬停计时；**延迟由
> 调用方推进、本层不读时钟**，故三平台行为逐字节一致；纯内存对象 + 离屏画布，
> 零 DISPLAY 依赖；19 个 `extern fn`，见 §38.17）；
> ⑫ **状态栏层**（`dial_gui_statusbar_*`，左 / 右文本 + 进度条槽位；进度值纯值推进、
> 越界夹紧、值域收窄同步夹紧；纯内存对象 + 离屏画布，零 DISPLAY 依赖；
> 20 个 `extern fn`，见 §38.18）；
> ⑬ **通知气泡层**（`dial_gui_toast_*`，FIFO 消息队列 + 生命周期纯值推进、
> 到期自动消隐、`life_ms<=0` 永不过期；纯内存对象 + 离屏画布，零 DISPLAY 依赖；
> 23 个 `extern fn`，见 §38.19）；
> ⑭ **窗口级覆盖层自动集成**（`dial_gui_window_set/scroll/clear_*`，把第 9 ~ 13 层
> 5 类对象挂到窗口槽位，`win.draw()` 按固定 z-order 自动叠加；窗口**借用**对象、
> 只持指针不释放，空槽位零像素影响；15 个 `extern fn`，见 §38.20）；
> ⑮ **快捷键 / 命令系统**（`dial_gui_hotkey_*`，按键组合 → 命令 id 的纯内存表 +
> 命令启用态；`bind` 冲突 Fail-Fast / `rebind` 覆盖 / `unbind` 压实 / `dispatch`
> 只发启用且命中的命令；命令语义归应用、C 侧零反向回调；纯内存对象 + 离屏画布，
> 零 DISPLAY 依赖；18 个 `extern fn`，见 §38.21）。
> 七层同为纯 C、零平台符号、绝不 C → Dial 反向回调，
> 旧 154 个签名一字不动。
> 它是 `[v1.10]`（标签 `v1.1`）发布**之后**的**开发中增量**：编译器版本仍为
> `1.10.0`，**未升版本号、未打标签、未发 Release**（见 CHANGELOG `[未发布]` 段）。
> 前七层都是**真实现**，不是占位接口；十五层一律沿用「只追加 `extern fn`（第 8 层
> 连 `extern fn` 都不加）、ABI 不变、绝不 C → Dial 反向回调」的宪法。

**架构分叉（用户拍板）**：

| 分叉 | 选择 |
| --- | --- |
| 底层边界 | 纯 X11 自绘，**强制 XShm 共享内存 + XRender 合成** + 自研迷你 2D 光栅化器 + `stb_truetype` 字体；不用任何现成 GUI 工具包 |
| Windows | **复刻同一套逻辑**，底层换成 Win32 GDI（`CreateDIBSection` + `StretchBlt`）；消息循环映射成同一套事件码，Dial 侧代码平台无关 |
| 平台策略 | **三平台真实现**：`x86_64-linux-gnu` / `aarch64-unknown-linux-musl` / `x86_64-pc-windows-gnu` 都由 `zig cc` 交叉编译出真实 `.a`，不是占位 stub |
| 第一版范围（地基轮） | 只做地基层（`GuiApp` / `GuiWindow` / `GuiPainter` / `GuiFont` / Event）；**当时**控件树 / 布局 / 信号槽不进契约——后续六轮已补框架层、外观层、输入层、主题层、信号槽层与菜单/对话框层，见下七行 |
| 控件框架层范围（当时） | Widget 树 + 绝对 / VBox / HBox 布局 + 命中测试 + 事件分发；**当时**统一通用 Box（28 个 `extern fn`），后续四轮已在其上补成品控件、TextBox、主题层/控件扩展与信号槽层 |
| 外观层范围（当时） | **最小可用集**：Label / Button / CheckBox 三种控件 + C 侧内置浅色默认主题 + 四态（普通 / 悬停 / 按下 / 禁用）绘制 + 纯值事件推进 + 整树一次绘制（12 个 `extern fn`）；当时不引入 C → Dial 回调、不暴露 theme setter（本轮已补） |
| 输入层范围（当时） | TextBox 单行编辑（可打印字符 / 退格 / 删除 / 光标 / 选区 / UTF-8 边界 / `max_len`）+ 焦点链（Tab / Shift+Tab 环形切换 / 点击聚焦）+ **两平台统一功能键码**（`X11 KeySym` / `Win32 VK` → 同表）+ 修饰键位掩码 + Xvfb **XTest 真实鼠标/键盘注入**闭环（14 个 `extern fn`）；当时不暴露 theme setter（本轮已补） |
| 主题层 / 控件扩展范围（第 5 层） | **14 个可覆写颜色角色**（`theme_set/get/reset/role_count`；默认值逐字不变，故 `look_crc_normal` 不漂移）+ Radio / Slider / ProgressBar 三种成品控件 + `value` / `range` / `group` 纯值访问器（11 个 `extern fn`）；仍只追加、ABI 不变、零反向回调，旧 109 个签名一字不动 |
| 信号槽 / 控件事件订阅范围（第 6 层） | **6 类可订阅纯值信号**（点击 / 勾选翻转 / 文本变更 / 值变更 / 焦点变化 / 提交）+ 每树一个**环形队列**（容量 64 / 72 字节纯值信号块 / 零订阅零开销）+ subscribe / pending / data / pop / clear / dropped（10 个 `extern fn`，见 §38.12）；纯 C、零平台符号、**绝不 C → Dial 反向回调**，旧 120 个签名一字不动 |
| 菜单 / 对话框覆盖层范围（第 7 层） | **窗口级覆盖层**：顶栏菜单（`File` / `Edit` / `Help` 横排，22px 高）+ 弹出菜单（锚点定位、分隔符 / 禁用项不可命中）+ 模态对话框（`OK` / `Cancel` / `Yes` / `No` 按钮位，半透明遮罩 + 居中盒）。三者**不进控件树**，几何 / 绘制 CRC 逐字节不漂移；菜单项 / 对话框按钮选中事件**复用第 6 层纯值信号队列**（`SELECTED` a=item id、kind=`MENU`；`SUBMITTED` a=结果码、kind=`DIALOG`），所有权归窗口、关闭时一并释放（24 个 `extern fn`，见 §38.13）；纯 C、零平台符号、**绝不 C → Dial 反向回调**，旧 130 个签名一字不动 |
| **Dial 侧 OO 封装层范围（第 8 层）** | `packages/gui/src/gui_oo.dial`：**278 个 `impl` 方法**覆盖 279 个 C 风格 `extern fn`（入口 `dial_gui_app_new` 无实例可挂，仍用原函数）；按各不透明句柄分组，方法名去掉 `dial_gui_` 与类型前缀；进程级全局状态 / 工厂归到 `GuiApp` 命名空间（self 忽略）。**纯 Dial 源码、随包 `src/` 分发（装包即用）、零 `extern fn` 增量、ABI 不变**；用户装包后 `import "gui_oo";` 一行即生效（模块内 `import "gui";` 由前端汇总去重），无需复制源码；项目里同名 `.dial` 仍可覆盖包内副本，见 §25.11 / §38.14 |
| **滚动视图层范围（第 9 层）** | **窗口级辅助覆盖层**：`GuiScroll` 纯内存对象，视口裁剪（调用方按 `off_x/off_y` 平移内容 + `dial_gui_set_clip`）+ 滚动条几何 / 命中（轨道 / 滑块 / 无）/ 绘制（`gui_raster_*` 主题色）+ 偏移推进（`wheel` / `drag` / `scroll_to`，越界自动夹紧 `[0, 内容-视口]`）。15 个 `extern fn`，与旧 154 个同契约；纯 C、零平台符号、**绝不 C → Dial 反向回调**，旧 154 个签名一字不动，见 §38.15 |
| **窗口级覆盖层自动集成范围（第 14 层）** | `GuiWindowImpl` 新增 5 个**借用槽位**（`scroll` / `context` / `tooltip` / `statusbar` / `toast`），配套 5 × 3 = **15 个 `extern fn`**（`set` / 读回 / `clear`）。`win.draw()` 在控件树 / 顶栏 / 弹窗 / 对话框之后按固定 z-order 自动叠加：滚动条 → 上下文菜单 → 悬浮提示 → 状态栏 → 通知气泡；空槽位跳过绘制、**既有 CRC 逐字节不破**。窗口**不拥有**这 5 类对象，`window_close` / `app_free` 只清槽位、不释放对象，对象生死归应用；空窗口 / 空对象一律 Fail-Fast 返回 -1。纯 C、零平台符号、零时钟、**绝不 C → Dial 反向回调**，旧 246 个签名一字不动，见 §38.20 |
| **快捷键 / 命令系统范围（第 15 层）** | 应用自持的纯内存表 `GuiHotkey`（**不挂进 `GuiWindowImpl` 槽位**）：最多 64 条绑定、命令 id `[0, 256)`，命令默认全启用；`bind` 冲突 Fail-Fast / `rebind` 覆盖 / `unbind` 压实顺序 / `dispatch` 只发启用且命中的命令 id（语义归应用、C 侧零反向回调）/ `lookup` 不看启用态 / `clear` 复位启用态但保留 tag。18 个 `extern fn`；`draw` 先擦固定面板再画前 8 行、空表零像素影响、重绘幂等。纯 C、零平台符号、零时钟、零 DISPLAY 依赖，旧 261 个签名一字不动，见 §38.21 |
| 事件分发 | **主动分发，绝不 C → Dial 反向回调**：C 侧写 `packed struct` 纯值块 / 纯值四态 / 纯值编辑态 / 纯值颜色表 / 纯值信号队列，Dial 侧 `poll/wait` 后读；字符串由 `string_from_cstr` 复制成 ARC 对象。C 管 C 的、ARC 管 ARC 的，零隐式开销 |
| 包契约 | **追加函数、ABI 不变**：七层都只加 `extern fn` + 句柄，旧签名一字不动（框架层追加前 55 个、外观层追加前 83 个、输入层追加前 95 个、主题层追加前 109 个、信号层追加前 120 个，菜单/对话框层追加前 130 个，第 9 ~ 13 层追加前 154 个，第 14 层追加前 261-15=246 个，第 15 层追加前 279-18=261 个），`abi = 6` / `version = 1.10.0` 不变，重新 `pack` + `install`（同 ABI 覆盖）；**第 8 层不增签名**——它是 `.dial` 源码层，随包 `src/` 分发（见 §25.11），装包即用、不碰 `abi` / `version` / 既有签名；改动它只需随包重新 `pack` + `install` |

### 38.1 包结构

```
packages/gui/
├── dial.toml             # name=gui / version 跟随编译器 / abi=6 / 三平台 .a 路径
├── gui.diali             # 唯一对外契约（不透明句柄 + extern fn，单行签名）
├── build.sh              # zig cc 三平台交叉编译 + stb SHA256 自检 + 可复现参数
├── src/dial_gui.h        # 内部公共头：错误码 / 事件码 / 结构体 / 后端 vtable
├── src/dial_gui_core.c   # FFI 包装：句柄生命周期 / 错误槽 / 事件槽 / 调 vtable
├── src/dial_gui_raster.c # 自研光栅化器 + 内置 8×8 点阵 + stb_truetype 字形
├── src/dial_gui_widget.c # 控件框架层：纯 C 树 / 布局 / 命中 / packed 事件块（三平台共用）
├── src/dial_gui_theme.c  # 主题层：14 个可覆写颜色角色 / 默认值 / reset（纯 C，三平台共用）
├── src/dial_gui_look.c   # 成品控件外观层：主题表驱动四态 / 八种控件绘制 / 事件推进（纯 C）
├── src/dial_gui_input.c  # 输入层：TextBox 编辑 / 光标·选区 / 焦点链 / 键码推进（纯 C）
├── src/dial_gui_signal.c # 信号层：可订阅纯值信号块 + 每树环形队列（纯 C，三平台共用）
├── src/dial_gui_menu.c   # 第 7 层：菜单 / 对话框覆盖层（纯 C，三平台共用，零平台符号）
├── src/dial_gui_scroll.c # 第 9 层：滚动视图（视口裁剪 + 滚动条几何/命中/绘制 + 偏移推进，纯 C，三平台共用，零平台符号）
├── src/dial_gui_context.c# 第 10 层：右键上下文菜单（复用第 7 层 GuiMenu，纯 C，三平台共用，零平台符号）
├── src/dial_gui_tooltip.c# 第 11 层：悬浮提示（延迟由调用方推进、不读时钟，纯 C，三平台共用，零平台符号）
├── src/dial_gui_statusbar.c # 第 12 层：状态栏（左右文本 + 进度条槽位，纯 C，三平台共用，零平台符号）
├── src/dial_gui_toast.c  # 第 13 层：通知气泡（FIFO 消息队列 + 生命周期消隐，纯 C，三平台共用，零平台符号）
├── src/gui_oo.dial      # 第 8 层：Dial 侧 OO 封装（278 个 impl 方法，纯 Dial 源码，随包 src/ 分发、装包即用）
├── src/gui_font8.h       # 内置 ASCII 点阵静态常量表
├── src/dial_gui_x11.c    # POSIX 后端（X11 + XShm + XRender，dlopen）
├── src/dial_gui_win32.c  # Windows 后端（GDI DIBSection，LoadLibraryA）
└── lib/<triple>/libgui.a # 三平台产物（被 .gitignore 忽略，构建时生成）
third_party/stb/          # stb_truetype.h v1.26 + LICENSE（MIT）
patches/stb-1.26.SHA256SUMS
tests/pkg/use_gui.dial        # 地基层 demo（像素级 CRC 回归，crc_offscreen 基准）
tests/pkg/use_gui_widget.dial # 控件框架层 demo（不创建 GuiPainter，不碰 CRC 基准）
tests/pkg/use_gui_look.dial   # 外观层 demo（独立 offscreen 画布，crc_look 硬钉）
tests/pkg/use_gui_input.dial  # 输入层 demo（编辑 / 选区 / 焦点 / 键码 / 实窗事件闭环）
tests/pkg/use_gui_theme.dial  # 主题层 demo（14 角色 set/get/reset / 八种控件 / 值域 Fail-Fast）
tests/pkg/use_gui_signal.dial # 信号层 demo（纯值块布局 / 六类信号 / 零订阅零开销 / 溢出可观测）
tests/pkg/use_gui_menu.dial  # 第 7 层 demo（菜单对象 / 顶栏·弹窗 SELECTED / 模态对话框 SUBMITTED / 控件树 CLICKED）
tests/oo/use_gui_oo.dial      # 第 8 层 demo（AD 组：C 风格 / OO 风格绘制 CRC32 逐字节相等 + Xvfb 实窗）
tests/pkg/xtest_inject.sh     # Xvfb 下用 xdotool（XTEST）真注入鼠标/键盘事件
```

`dial_gui_widget.c` / `dial_gui_theme.c` / `dial_gui_look.c` / `dial_gui_input.c` /
`dial_gui_signal.c` / `dial_gui_menu.c` **都只 include `dial_gui.h` + libc**，不碰 X11 /
Win32 任何 API，因此三份 `.a` 的 `nm -u` 不会因框架层 / 外观层 / 输入层 / 主题层 /
信号层 / 菜单对话框层新增任何平台符号。
`dial_gui_theme.c` 只是进程级静态颜色表（GUI 单线程事件循环，无需加锁），
默认色值逐字取自 `dial_gui.h` 的 `GUI_THEME_*` 宏，因此 `theme_reset()` 后逐像素
CRC 与引入 setter 之前完全一致。`dial_gui_look.c` 也**不碰像素细节**：只调
`dial_gui_raster.c` 导出的 `gui_raster_*` / `gui_font_*`，像素仍由唯一的光栅化器
TU 管理；内置点阵字体用一个静态零分配对象（`kind=0`），无需 `gui_font_default`
分配。XTest（libXtst）**只在测试侧**由 `xtest_inject.sh` / `xdotool` 使用，包本体
`.a` 依然零平台链接期依赖。

### 38.2 对外契约：`gui.diali`

五个**不透明句柄**（不进 ARC，必须显式 `*_free`）：

| 句柄 | 含义 | 生命周期 |
| --- | --- | --- |
| `GuiApp` | 显示连接 / 窗口表 / 事件循环状态 / 最近事件槽 | `dial_gui_app_new` → `dial_gui_app_free` |
| `GuiWindow` | 一个窗口 | `dial_gui_window_new` → `dial_gui_window_close` |
| `GuiPainter` | 离屏像素画布（窗口绑定或纯内存） | 窗口画布随窗口释放；`dial_gui_painter_offscreen` 的需 `dial_gui_painter_free` |
| `GuiFont` | 字体 | `dial_gui_font_default` / `dial_gui_font_load` → `dial_gui_font_free` |
| `GuiWidget` | 控件节点（框架层为通用 Box；外观层可设为 LABEL / BUTTON / CHECKBOX，输入层可设为 TEXTBOX，主题层可设为 RADIO / SLIDER / PROGRESS） | 独立树 `dial_gui_widget_tree` → `dial_gui_widget_tree_free`；挂窗后由窗口拥有，随 `window_close` 释放，**此后不得再 free** |

**颜色**：所有 `color` 参数一律 `0xAARRGGBB`（装进 `int64`，高 32 位为 0）。
C 侧在 X11 走 ARGB32、在 GDI 走 BGRA，通道转换由包内部负责，Dial 侧无感。

**事件码**（`dial_gui_app_poll` / `dial_gui_app_wait` 返回值）：

| 码 | 事件 | 码 | 事件 |
| --- | --- | --- | --- |
| 0 | NONE | 5 | KEY_RELEASE |
| 1 | MOUSE_PRESS | 6 | RESIZE |
| 2 | MOUSE_RELEASE | 7 | EXPOSE |
| 3 | MOUSE_MOVE | 8 | CLOSE |
| 4 | KEY_PRESS | 9 | QUIT |

事件字段用 `dial_gui_event_kind/x/y/button/key/mods/width/height/window` 访问
（每 App 一份最近事件槽，单线程事件循环，无需加锁）；`dial_gui_event_mods`
返回修饰键位掩码（`1` SHIFT / `2` CTRL / `4` ALT，按位或），输入层的
`widget_key(root, kind, key, mods)` 直接消费它。键码见下方统一键码表。

#### 控件框架层契约（28 个追加 `extern fn`）

控件层**必须并入同一个 `gui` 包**：`.diali` 的不透明句柄按包隔离解析
（`src/main.rs` 的 `resolve_imports` 每包只用自己的 `opaque_types`，且全局禁止重名），
新建 `gui_widgets` 包根本无法引用 `GuiWindow` / `GuiApp`。因此**框架层那一轮**
只**追加** 28 个单行 `extern fn` + `type GuiWidget;`，**旧 55 个签名一字不动**；
外观层那一轮同理只追加 12 个（见下），输入层那一轮只追加 14 个（见下），
主题层 / 控件扩展那一轮只追加 11 个、信号槽 / 控件事件订阅本轮再追加 10 个（见
§38.12），合计 **130 个 `extern fn`**。
`dial.toml` 的 `abi = 6` / `version = 1.10.0` 始终不变，重新 `pack` +
`install` 即可（同 ABI 覆盖；`.diali` 的 SHA256 由 manifest 现算）。

| 分组 | 数量 | 代表 API |
| --- | --- | --- |
| 树 | 7 | `widget_tree(w,h)` / `widget_tree_free` / `widget_add` / `widget_remove` / `widget_parent` / `widget_first_child` / `widget_next_sibling` |
| 几何 | 5 | `widget_set_rect` + `widget_x/y/width/height` |
| 可见 / id | 4 | `widget_set_visible` / `widget_visible` / `widget_set_id(string)` / `widget_id(): rawptr<byte>` |
| 纯值标记 | 4 | `widget_set_tag` / `widget_tag` / `widget_set_userdata` / `widget_userdata`（一律 `int64`，**绝不存 Dial 指针**） |
| 布局 | 4 | `widget_set_layout(0 ABS / 1 VBOX / 2 HBOX)` / `set_padding` / `set_gap` / `widget_layout_apply` |
| 命中 / 挂窗 | 2 | `widget_hit(root,x,y)` / `window_set_root(win,root)` |
| 事件 | 2 | `event_widget(app)` / `event_data(app): rawptr<byte>` |

**独立树根是刻意设计**：`dial_gui_app_new()` 在无 DISPLAY 时因 `XOpenDisplay`
失败返回空句柄，若控件树只能绑窗口，框架层就无法在无头环境验收。
`widget_tree` 建的是纯内存树、不连显示服务器，因此 AA 组无 DISPLAY 路径也能
把布局 / 命中 / packed 布局全跑完；挂窗后树归窗口所有。

布局是**纯整数、幂等**的：`set_rect` 写 `req_*`（期望几何），
`layout_apply` 每次只从 `req_*` 重算，固定尺寸优先扣减、弹性余量整数除法向下取整，
重复调用不会叠加 padding / gap，也不会把上一轮弹性尺寸误当固定尺寸。
命中测试取**最深可见** Box、后加子优先（与自绘层序一致）。

#### 主动事件分发：packed 纯值块 + ARC 边界

**绝不 C → Dial 反向回调**。Dial 无 GC，全靠 ARC + 手动释放；C 侧若在平台线程
直接调 Dial 函数，跨线程引用计数管理会失控。因此 C 侧只写一块
`__attribute__((packed))` 的 `GuiWidgetEvent`（**104 字节**，三目标均 64 位），
Dial 侧 `poll/wait` 后经 `rawptr<byte>` + `ptr_cast` 主动读走：

| 偏移 | 字段 | 类型 | 语义 |
| --- | --- | --- | --- |
| 0..55 | `kind,x,y,button,key,w,h` | `int` ×7 | 纯值 |
| 56..87 | `local_x,local_y,tag,userdata` | `int` ×4 | 纯值；命中控件时 `tag`/`userdata` 取自控件，`local_*` 相对控件左上角 |
| 88 | `win_title` | `rawptr<byte>` | **C 侧借用**（窗口标题 `title[256]`） |
| 96 | `widget_id` | `rawptr<byte>` | **C 侧借用**（控件 `strdup` 副本，未命中为 NULL） |

- 块内嵌于 `GuiAppImpl`、**非堆分配**；两个借用指针由 C 拥有、C 绝不 `free`，
  有效期至**下次 `poll/wait` 或窗口 / 树释放**。Dial 侧必须立即
  `string_from_cstr` 复制成自己的 ARC `string`，**绝不长期持有**；
  NULL 借用指针会得到空串（`string_from_cstr(NULL)` 已定义为空串）。
- `dial_gui_event_widget(app)` 给命中的 `GuiWidget` 裸句柄（窗口级 / QUIT 为 NULL）；
  命中测试放在两后端 `pump` 的 **Dial 调用线程**，绝不进 `wndproc` 系统回调。
- **布局双向锁死**：C 侧 `_Static_assert(sizeof==104, offsetof(win_title)==88,
  offsetof(widget_id)==96)` 与 Dial 侧 `sizeof<T>()` / `alignof<T>()` /
  `offsetof<T>(field)` 编译期内建逐项比对（复用 §33.17 P 组范式），
  任何字段增删两边同时报错，杜绝契约漂移。
- QUIT / 窗口级事件没有命中控件，`tag` / `userdata` / `local_*` 为 0、
  `win_title` / `widget_id` 为 NULL；`GuiWidgetEvent` 仍是 **104 字节布局不变**
  （输入层的修饰键另由 `dial_gui_event_mods` 单独提供，绝不塞进 packed 块改尺寸）。
  **真实鼠标 / 键盘注入已由 XTest（`xtest_inject.sh` + `xdotool`）在 Xvfb 实窗实机
  验收**（AA 组第 10 项）；无 XTest 环境如实打印跳过，不伪装通过。

**统一键码表（`dial_gui_event_key`）**：两平台**同值**（本轮收口，见 §38.11）。
起点 `0x110000`（十进制 `1114112`）大于 Unicode 最大码点 `0x10FFFF`，绝不与可打印
字符冲突；X11 `KeySym`（含 `XK_ISO_Left_Tab`）与 Win32 `VK` 都先映射进同一张表。

| 键类别 | 键码 | 两平台映射来源 |
| --- | --- | --- |
| 可打印字符 | Unicode 码点 | X11 `XLookupString` 的 UTF-8 首码点 / Win32 `WM_CHAR` 的 `wParam` |
| BACKSPACE / TAB / ENTER / ESCAPE | `0x110001` / `0x110002` / `0x110003` / `0x110004` | X11 `0xFF08/0xFF09(或 0xFE20 ISO_Left_Tab)/0xFF0D/0xFF1B`；Win32 `VK 0x08/0x09/0x0D/0x1B` |
| LEFT / UP / RIGHT / DOWN | `0x110005`..`0x110008` | X11 `0xFF51..0xFF54`；Win32 `VK 0x25..0x28` |
| HOME / END / PAGEUP / PAGEDOWN | `0x110009`..`0x11000C` | X11 `0xFF50/0xFF57/0xFF55/0xFF56`；Win32 `VK 0x24/0x23/0x21/0x22` |
| DELETE / SHIFT / CONTROL / ALT | `0x11000D`..`0x110010` | X11 `0xFFFF/0xFFE1-2/0xFFE3-4/0xFFE9-A`；Win32 `VK 0x2E/0x10/0x11/0x12` |
| F1..F12 | `0x110011`..`0x11001C` | X11 `0xFFBE..0xFFC9`；Win32 `VK 0x70..0x7B` |
| 表外键 | `0x11003F`（`GUI_KEY_UNKNOWN`） | 一律收敛为**单一**未知码，绝不漏出平台私有值 |

> 可打印字符在两平台都是 Unicode 码点、**一致**；功能键编码**两平台已统一**到
> 上表同一套常量，Dial 侧代码平台无关。`mods` 位掩码由 `dial_gui_event_mods` 取
> （X11 取 `XKeyEvent.state` 的 `ShiftMask/ControlMask/Mod1Mask`，Win32 由
> `wndproc` 跟踪 `VK_SHIFT/CONTROL/MENU` 的按下 / 抬起），`Shift+Tab` 的焦点回退
> 就是靠它判向。Win32 后端仍**仅交叉编译、未实机运行**。

**错误码**（`dial_gui_last_error`）：`0` 无错误、`1` 参数非法、`2` 分配失败、
`3` 平台库缺失 / 初始化失败、`4` 窗口创建 / 上屏失败、`5` 字体失败、
`6` 后端不支持该操作。错误走**返回码 + 线程局部错误槽**，
`dial_gui_last_error_msg()` 取人类可读说明，`dial_gui_clear_error()` 清空；
**不抛 Dial 异常**。后端 vtable 内部用 `1=成功 / 0=失败`，core 层由 `be_result`
统一转成 Dial 契约的 `0=成功 / -1=失败`，避免两套返回码漂移。

#### 成品控件外观层契约（12 个追加 `extern fn`）

第三层同样**只追加**：旧 83 个签名一字不动、`abi = 6` / `version = 1.10.0`
不变。控件种类 / 文本 / 四态全是 `GuiWidgetImpl` 上的 C 侧纯值或自有副本；
配色一律走 `dial_gui_theme.c` 的颜色表，默认值逐字取自 `dial_gui.h` 的
`GUI_THEME_*`（本轮起可由 `theme_set` 按角色覆写、`theme_reset` 逐值复位），
因此三平台逐像素一致、可被 `painter_crc32` 稳定断言。

| 分组 | 数量 | 代表 API |
| --- | --- | --- |
| 种类 | 2 | `widget_set_kind(0 BOX / 1 LABEL / 2 BUTTON / 3 CHECKBOX / 4 TEXTBOX)` / `widget_kind`（TEXTBOX 为输入层新增种类） |
| 文本 | 2 | `widget_set_text(string)` / `widget_text(): rawptr<byte>`（C 借用，须 `string_from_cstr` 复制） |
| 状态 | 6 | `widget_set_checked` / `checked` / `set_disabled` / `disabled` / `hovered` / `pressed` |
| 事件推进 | 1 | `widget_handle(root,kind,x,y,button)`：纯值 MOVE / PRESS / RELEASE 推进四态，返回 `1=激活 / 0=无 / -1=参数错` |
| 绘制 | 1 | `widget_draw(root,painter)`：按默认主题 + 当前四态把整树画到任意 `GuiPainter` |

**内置浅色默认主题**（`0xAARRGGBB`，14 个角色；`theme_set` 可覆写、
`theme_reset` 逐值复位，默认值逐像素可断言且**不漂移**）：

| 用途 | 色值 | 用途 | 色值 |
| --- | --- | --- | --- |
| 面板 / BOX 背景 | `FFF4F6F9` | 细边框 | `FFC7CEDA` |
| 文本 / 禁用文本 | `FF202733` / `FFA9B0BD` | 勾选框 / Radio 底 | `FFFFFFFF` |
| 按钮 普通 | `FFE7ECF4` | 按钮 悬停 | `FFD3E0FA` |
| 按钮 按下 | `FFB2C8F2` | 按钮 禁用 | `FFF0F2F5` |
| 勾 / 焦点边框 / 光标 / 高亮 / 选中点 | `FF2F6FE0` | 文本字号 | 16（内置点阵 `sc=2`，每字符 18×16） |
| TextBox 输入底 | `FFFFFFFF` | TextBox 选区底 | `FFC9DCFB`（仅焦点态绘制） |
| Slider / Progress 轨道底 | `FFE2E8F0` | 进度填充 | `FF2F6FE0` |
| Slider 滑块 | `FFFFFFFF` | 滑块描边 | `FF2F6FE0`（MARK 角色） |

**四态与纯值事件**：`widget_handle` 只读纯值 `kind / x / y / button`，复用框架层
`widget_hit` 取最深可见控件，MOVE 清/置 `hovered`、PRESS 清/置 `pressed`、
RELEASE 清 `pressed` 并激活（CHECKBOX 翻转 `checked`、BUTTON 记一次点击、
RADIO 选中并清同组、SLIDER 按局部 `x` 设 `value`，返回 1；PROGRESS 只读不激活、
返回 0）；`disabled` 控件不 hover / 不 press / 不激活、走禁用配色。**绝不
C → Dial 反向回调**：被激活控件由 Dial 侧用 `widget_hit(root,x,y)` 自行取回，
C 侧绝不返回或缓存 Dial 对象。

**绘制确定性**：`widget_draw` 前序递归自己累加相对几何（不依赖 `abs_*`），
父先画、后加子在上（与「后加子优先」的命中层序一致），只读纯值状态、
不推进状态；BOX 铺面板背景 + 边框，LABEL 只画文本，BUTTON 四态背景 +
居中文本，CHECKBOX 方框 + 勾 + 右侧文本，TEXTBOX 白底 + 边框（焦点态加粗换成
高亮色）+ 选区底 + 文本 + 光标竖线（文本区临时裁剪到 `x+6 .. w-12`，光标恒定
不闪烁以保证逐像素可断言），RADIO 圆圈 + 选中点 + 文本，SLIDER 轨道 +
已填充 + 滑块，PROGRESS 轨道 + 边框 + 按比例填充（全部纯整数几何）。
`use_gui_look.dial` 关 AA 后硬钉 `look_crc_normal = 3884455222`，并断言四态
CRC 互异、移出画布后复位逐像素回到普通态（幂等）；`use_gui_input.dial` 另钉
`input_crc_focused = 4147329629` 并断言移走焦点后边框色与 CRC 同时变化；
`use_gui_theme.dial` 断言八种控件连画两次 CRC 一致、`theme_set` 改色后逐像素
生效、`theme_reset` 后 CRC 回到基准（默认值不漂移）。

#### 输入层契约（14 个追加 `extern fn`）

第四层同样**只追加**：旧 95 个签名一字不动、`abi = 6` / `version = 1.10.0`
不变，`GuiWidgetEvent` 的 104 字节布局**一字不动**（修饰键走独立访问器）。
控件种类新可接受 `4 TEXTBOX`；焦点是树根上的一个 C 侧裸指针，文本仍是控件
自己的 `strdup` 副本，光标 / 选区是 **UTF-8 字节偏移**（一律按字符边界对齐，
绝不切断多字节序列）。`max_len` 按字节计，`0` = 不限（仍有 `4096` 兜底），
越界值**明确报错而非静默截断**；`widget_set_text` 是显式 API 不受它约束，
但会把光标 / 选区归零。

| 分组 | 数量 | 代表 API |
| --- | --- | --- |
| 事件修饰键 | 1 | `dial_gui_event_mods(app)`（在事件访问器区） |
| 焦点链 | 5 | `widget_set_focus` / `widget_focus` / `widget_focused` / `widget_focus_next` / `widget_focus_prev` |
| 光标 / 选区 | 5 | `widget_cursor` / `widget_set_cursor` / `widget_sel_start` / `widget_sel_end` / `widget_select` |
| 输入上限 | 2 | `widget_set_max_len` / `widget_max_len` |
| 键盘事件推进 | 1 | `widget_key(root,kind,key,mods)`：返回 `0` 无变化 / `1` 文本或光标变化 / `2` 焦点变化 / `3` Enter 提交 / `-1` 参数错 |

**可聚焦控件** = 可见 / 未禁用 / `kind == 4 TEXTBOX`；Tab 按加入顺序环形前进，
Shift+Tab 反向，禁用的控件自动让焦点出栈，摘除子树时先清焦点裸指针（绝不
悬垂）。键盘编辑只认 `KEY_PRESS`（`KEY_RELEASE` 忽略）：可打印字符插入（有选区
先替换）、BACKSPACE / DELETE 删前 / 后一个字符、LEFT / RIGHT / HOME / END 移光标
（带 SHIFT 扩选区）、TAB 切焦点、ENTER 给提交信号、ESCAPE 清选区，其余功能键 /
修饰键本身不改文本。**绝不 C → Dial 反向回调**：Dial 侧 `poll/wait` 后把
`event_key` / `event_mods` 主动喂进 `widget_key`，C 侧从不回调、绝不返回或缓存
Dial 对象。

#### 主题层与控件扩展契约（11 个追加 `extern fn`）

第五层同样**只追加**：旧 109 个签名一字不动、`abi = 6` / `version = 1.10.0` /
`GuiWidgetEvent` 104 字节布局全部不变。主题是 `dial_gui_theme.c` 里的
**进程级静态颜色表**（GUI 单线程事件循环，无需加锁）：14 个角色编号稳定、
只准末尾追加，`theme_set` 一次改色对之后所有 `widget_draw` 立即生效，
`theme_reset` 逐值复位到 `GUI_THEME_*` 宏。默认值逐字不变，因此
`look_crc_normal = 3884455222` **不漂移**；每个 demo 是独立进程，静态表天然
从默认值起步，绝不污染其它用例的 CRC 基准。

| 分组 | 数量 | 代表 API |
| --- | --- | --- |
| 主题角色表 | 4 | `theme_role_count()` / `theme_get(role)` / `theme_set(role,color)` / `theme_reset()` |
| 值 / 值域 | 5 | `widget_value` / `widget_set_value` / `widget_set_range(lo,hi)` / `widget_min` / `widget_max` |
| 单选组 | 2 | `widget_set_group` / `widget_group` |

| 角色 | 用途 | 默认色 |
| --- | --- | --- |
| `0 BG` / `1 BORDER` / `2 TEXT` / `3 TEXT_DIS` | 面板 / 边框 / 文本 / 禁用文本 | `FFF4F6F9` / `FFC7CEDA` / `FF202733` / `FFA9B0BD` |
| `4..7 BTN*` | 按钮四态 | `FFE7ECF4` / `FFD3E0FA` / `FFB2C8F2` / `FFF0F2F5` |
| `8 CHECK_BG` / `9 MARK` / `10 SEL` | 输入底 / 高亮 / 选区 | `FFFFFFFF` / `FF2F6FE0` / `FFC9DCFB` |
| `11 TRACK` / `12 FILL` / `13 KNOB` | 轨道 / 进度填充 / 滑块 | `FFE2E8F0` / `FF2F6FE0` / `FFFFFFFF` |

新增三种控件（`kind` 5 / 6 / 7）：**RADIO** 圆圈 + 选中点 + 右侧文本，
RELEASE 选中且 `group != 0` 时同组互斥（`0` = 各自独立），选中态复用 `checked`；
**SLIDER** 轨道 + 已填充 + 滑块，RELEASE 按命中点局部 `x` 的纯整数比例设 `value`
并 clamp 进 `[vmin, vmax]`；**PROGRESS** 轨道 + 边框 + 按 `value` 比例填充，
**只读**、RELEASE 不激活。默认值域 `0..100`。

**Fail-Fast（§20）**：`role` 越界 / 颜色高 32 位非 0 / `value` 越界 /
值域倒置（`lo > hi`）/ 值域过大一律返回 `-1` 并写错误槽，绝不静默截断；
`set_range` 成功后把已有 `value` clamp 进新值域，getter 立刻可读回。
`use_gui_theme.dial` 逐像素断言改色生效、`theme_reset` 后 CRC 回到基准（幂等）、
八种控件绘制确定性、Radio 互斥、Slider 单调、Progress 只读与值域 Fail-Fast。

#### 信号槽与控件事件订阅契约（10 个追加 `extern fn`，第 6 层）

第六层同样**只追加**：旧 120 个签名一字不动、`abi = 6` / `version = 1.10.0` /
`GuiWidgetEvent` 104 字节布局全部不变。这里把「信号槽」落地为**可订阅的纯值
信号队列**，而不是 C 侧注册函数指针——**绝不 C → Dial 反向回调**：C 只在「语义
状态真的变化」处把一条 `GuiWidgetSignal` 压进每树环形队列，Dial 侧主动
`pending → data(ptr_cast 读) → pop` 消费并自行分派。C 管 C 的、ARC 管 ARC 的。

| 分组 | 数量 | 代表 API |
| --- | --- | --- |
| 常量 | 2 | `signal_count()` (=6) / `signal_capacity()` (=64) |
| 订阅掩码 | 3 | `widget_subscribe(w,mask)` / `widget_subscribed(w)` / `widget_unsubscribe(w)` |
| 队列消费 | 5 | `widget_signal_pending(root)` / `signal_data(root): rawptr<byte>` / `signal_pop(root)` / `signal_clear(root)` / `signal_dropped(root)` |

**信号码**（位值，可组合成订阅掩码；编号跨版本稳定，只准末尾追加新位）：

| 位 | 信号 | 触发（`a` / `b` 为新值 / 旧值） |
| --- | --- | --- |
| `1` | `CLICKED` | BUTTON 在 RELEASE 激活（`a=1` / `b=0`） |
| `2` | `TOGGLED` | CHECKBOX 翻转 / RADIO 选中 / `set_checked`（`a=新checked` / `b=旧checked`） |
| `4` | `TEXT_CHANGED` | TEXTBOX 键盘编辑成功 / `set_text`（`a=新字节数` / `b=旧字节数`） |
| `8` | `VALUE_CHANGED` | SLIDER 按 x 设值 / `set_value`（`a=新值` / `b=旧值`） |
| `16` | `FOCUS_CHANGED` | 焦点迁移（点击 / Tab / `set_focus`）（`a=1 获得 / 0 失去` / `b=0`） |
| `32` | `SUBMITTED` | TEXTBOX Enter 提交（`a=1` / `b=0`） |

每个信号只由状态**真的变化**的那一次交互 / setter 触发一次；同值 setter、同组
RADIO 被动清除、`set_range` 的 clamp 都不另发信号（如实标注边界）。

**`GuiWidgetSignal` 是 72 字节 packed 纯值块**：`int seq, signal, tag, userdata,
kind, a, b, x, y`（9 × 8 字节）。与 104 字节 `GuiWidgetEvent` 的**关键区别**：
本块 **100% 纯值、零指针、零句柄**——连控件句柄都不存，只存 `tag` / `userdata`
两个 `int64` 纯值键（Dial 侧自建 map 定位控件）。因此控件被摘除、文本被覆盖、
整树被释放后，已入队信号仍是自洽整数快照，**读数绝不悬垂**、无需
`string_from_cstr`。布局由 C 侧 `_Static_assert(sizeof==72, offsetof(y)==64)` 与
Dial 侧 `sizeof<T>()` / `offsetof<T>(field)` 编译期内建双向锁死。

**零订阅 = 零开销**：每控件一个 `int64` 订阅掩码，默认 0；`gui_signal_emit` 第一行
就查掩码，未订阅直接返回（不找根、不分配、不入队）。所以不调用 `subscribe` 的旧
程序行为 / 绘制 CRC / 函数返回值逐字节不变，历史断言不漂移。**Fail-Fast（§20）**：
订阅掩码越界 / 未知位明确返回 `-1` 并写错误槽；队列满（容量 64）覆盖最旧、保留
最新并把根上 `sig_dropped` 累计 +1，Dial 侧用 `signal_dropped` 读「漏了多少条」，
`calloc` 失败同样计数——**绝不静默损坏**；`seq` 单调递增、`clear` 不重置序号。
详见 §38.12 与 `use_gui_signal.dial`。

### 38.3 分层架构

```
Dial 源码 --import "gui"--> gui.diali 签名（SHA256 锁定）
                                 |
                         dial_gui_core.c   <- 句柄 / 错误槽 / 事件槽 / 130 个 FFI 包装
                                 |  GuiBackend vtable
                                 |  ├─ dial_gui_widget.c（纯 C 树 / 布局 / 命中 / packed 块）
                                 |  ├─ dial_gui_theme.c（14 角色可覆写颜色表 / reset）
                                 |  ├─ dial_gui_look.c（主题表驱动四态 / 八种控件 / 事件推进）
                                 |  ├─ dial_gui_input.c（TextBox 编辑 / 焦点链 / 键码推进）
                                 |  └─ dial_gui_signal.c（纯值信号块 / 每树环形队列 / 订阅掩码）
                                 |      五 TU 均三平台共用、零平台符号；
                                 |      theme.c 只存静态颜色表，look.c 只调 gui_raster_* / gui_font_*；
                                 |      input.c / signal.c 只碰 C 树纯值，不碰平台 API / 不碰像素
              +------------------+------------------+
        dial_gui_x11.c                      dial_gui_win32.c
     (dlopen X11/Xext/Xrender)        (LoadLibraryA user32/gdi32)
              +------------------+------------------+
                         dial_gui_raster.c  <- 唯一碰像素的 TU
                         (ARGB32 / AA / 字体)
```

`core.c` **不 `#include` 任何 X11 / `windows.h`**，只认 `GuiBackend` vtable；
`dial_gui_widget.c` / `dial_gui_theme.c` / `dial_gui_look.c` / `dial_gui_input.c` /
`dial_gui_signal.c` 更是只 include `dial_gui.h` + libc。因此交叉编译三平台不需要
各平台 GUI SDK 头，框架层 / 外观层 / 输入层 / 主题层 / 信号层也不会给 `.a` 引入任何平台符号。平台库
**一律运行时动态装载**，
三份 `.a` 都不产生链接期未定义符号；XTest（`libXtst`）只在测试侧使用，
不进包、不进 `.a`。

### 38.4 自研 2D 光栅化器

`dial_gui_raster.c` 是**唯一碰像素的 TU**，也是唯一展开
`STB_TRUETYPE_IMPLEMENTATION` 的地方。CPU 侧一份 `ARGB32` 行主序像素缓冲，
窗口后端的 XShm 共享内存 / DIBSection **与它共享同一份像素**
（`GuiPainterImpl.ext_pixels` 标记外部缓冲，`dispose` 不 `free`）。

- `source-over` alpha 混合；覆盖面积子采样 AA，倍率 `1 / 2 / 4 / 8`
  （`dial_gui_set_aa`，默认 4）；
- 图元：`fill_rect` / `stroke_rect` / `draw_line`（圆帽）/ `fill_circle` /
  `stroke_circle` / `fill_ellipse` / `fill_trapezoid_aa` / `blit_pixels`；
- `set_clip` / `clear_clip` 裁剪、`get_pixel` / `set_pixel` 像素读写、
  `painter_crc32` 整块 CRC32（像素级回归断言）；
- `dial_gui_fill_trapezoid_aa` **一律画进 CPU 离屏缓冲**：XRender 的矢量能力
  用在 `window_flush` 的 Picture 合成链，而不是绕过 CPU 缓冲直接画到窗口——
  否则 `get_pixel` / `painter_crc32` 与用户自绘会看到两套状态。

### 38.5 字体层与安全边界

双路径，Dial 侧用同一个 `GuiFont` 句柄：

| 路径 | 来源 | 说明 |
| --- | --- | --- |
| 内置 8×8 ASCII 点阵 | `src/gui_font8.h` 静态常量表（`0x20..0x7E`） | 零运行依赖兜底；开发期由本机 TTF 生成静态表，运行时不读文件、不依赖字体库 |
| TrueType | vendor `stb_truetype.h` v1.26（MIT） | `stbtt_GetCodepointBitmap` 覆盖率混合；UTF-8 解码，非法字节替换成 U+FFFD |

> **安全边界（必须遵守）**：`stb_truetype.h` 上游明确
> `NO SECURITY GUARANTEE -- DO NOT USE THIS ON UNTRUSTED FONT FILES`。
> `dial_gui_font_load(path)` **只加载本机可信 TTF/OTF 文件**（≤ 64 MB），
> **绝不解析来自网络的字体数据**；内置点阵路径完全不经过 stb。
> `third_party/stb/` 基线由 `patches/stb-1.26.SHA256SUMS` 锁定，
> `build.sh` 构建前后 `sha256sum -c` 校验零改动。

### 38.6 Linux / Termux 后端：X11 + XShm + XRender

`dial_gui_x11.c` **不 include X11 头**，按 ABI 复刻 `XEvent` / `XErrorEvent` /
`XShmSegmentInfo`，`dlopen` 动态装载 `libX11.so` / `libXext.so` / `libXrender.so`。

- **XShm 离屏共享内存**：`shmget` / `shmat` / `XShmCreateImage` / `XShmAttach`，
  共享内存段同时映射给 CPU 画布与 XImage，`XShmPutImage` 上传**零拷贝**
  （不经过 socket 传像素）；
- **XRender 合成**：`XRenderFindVisualFormat` / `XRenderCreatePicture` /
  `XRenderSetPictureFilter` / `XRenderComposite(PictOpSrc)`，
  `dial_gui_window_set_filter("fast" / "good" / "best" / "nearest")` 控制平滑；
- **上屏顺序**：`XShmPutImage`（共享内存 → 中转 Pixmap）→
  `XRenderComposite`（Pixmap Picture → 窗口 Picture）→ `XSync` → `XFlush`；
- **异步错误不再带走宿主进程**：自定义 `XSetErrorHandler` 记录错误码与可读说明，
  `XSync` 后转成 `GUI_ERR_WINDOW` + Dial 错误槽。Xlib 默认处理器会直接
  `exit(1)`，曾让 `BadAccess`（`XShmAttach` 的 `seg.shmid` 未显式填写）表现为
  进程静默退出；现在显式 `seg.shmid = shmid` 并逐处 `XSync` 检查。

### 38.7 Windows 后端：Win32 GDI

`dial_gui_win32.c` 不 include `windows.h`，按 Win32 ABI 复刻 `MSG` /
`WNDCLASSA` / `BITMAPINFOHEADER` / `RECT`，`LoadLibraryA("user32.dll")` /
`gdi32.dll` + `GetProcAddress` 动态解析。

- `CreateDIBSection` 32bpp **top-down BGRA**（`biHeight = -h`），
  `CreateCompatibleDC` / `SelectObject` 绑定；
- `SetStretchBltMode(HALFTONE)` 对应 XRender 的平滑 filter，
  `StretchBlt` 上屏（`filter=best` 时后端能力串含 `stretch=halftone`）；
- WndProc 把 Win32 消息入队再映射成统一事件码，Dial 侧代码平台无关。

> **平台边界（如实标注）**：沙箱只做 `zig cc` 交叉编译 + `nm -u` 符号检查，
> **无法实机运行 Win32 后端**。X11 后端在 Xvfb 下真开窗口验收；
> Win32 后端的实机行为**尚未验证**，绝不写成「已实测」。Direct2D 亦未落地，
> 当前 Windows 走的是 GDI 路径。

### 38.8 零链接期依赖

三平台 `.a` 只保留 libc / libm / kernel32 / `dial_string_*` 等符号，
`nm -u` 断言**不含** `XOpenDisplay` / `XShmCreateImage` / `XRenderComposite` /
`CreateDIBSection` / `CreateWindowExA` 等平台 GUI 未定义符号。
`dialc` 最终链接**不需要**新增 `-lX11` / `-lXext` / `-lXrender` / `-lgdi32`；
缺库在**运行时**由 `dlopen` / `LoadLibraryA` 返回明确错误码 + 错误槽，
绝不静默崩溃。包版本跟随编译器（`dial.toml` 的 `version` 由 CI 用
`Cargo.toml` 覆盖），`abi = 6`，与 `stdlib.dialpkg` 同口径。控件框架层（28 个）、
成品控件外观层（12 个）、输入层（14 个）、主题层 / 控件扩展（11 个）与
信号槽 / 控件事件订阅（10 个）新增的
`extern fn` 一律是**纯追加**：追加前旧签名零改动（55 → 83 → 95 → 109 → 120 → 130）、
`abi` / `version` 不变，仅 `.diali` 的 SHA256 随 manifest 重算，用户
`uninstall gui` 后 `install` 新包即可（同 ABI 覆盖）。

### 38.9 Dial 侧用法

```dial
import "gui";

// 颜色 0xAARRGGBB（Dial 支持 0x 前缀字面量）
// 纯内存离屏画布：不开窗、不连显示服务器，像素级可断言
var p: GuiPainter = dial_gui_painter_offscreen(32, 32);
dial_gui_set_aa(p, 4);
dial_gui_clear(p, 0xFFFFFFFF);                 // 不透明白
dial_gui_set_color(p, 0xFFFF0000);             // 不透明红
dial_gui_fill_rect(p, 4, 4, 8, 8);
dial_gui_set_color(p, 0xFF0000FF);
dial_gui_fill_trapezoid_aa(p, 24, 28, 22.5, 27.5, 22.5, 27.5);
var edge: int = dial_gui_get_pixel(p, 22, 25); // AA 边缘 != 纯色
print("crc = " + str(dial_gui_painter_crc32(p)));
dial_gui_painter_free(p);

// 真实窗口 + 事件循环（X11 下需 DISPLAY；生产代码须检查 is_null）
var app: GuiApp = dial_gui_app_new();
var win: GuiWindow = dial_gui_window_new(app, "hello", 320, 200);
var wp: GuiPainter = dial_gui_window_painter(win);
dial_gui_clear(wp, 0xFFFFFFFF);
dial_gui_window_set_filter(win, "best");
dial_gui_window_flush(win);
print(dial_gui_window_backend(win));
while (dial_gui_app_should_quit(app) == 0) {
    var ev: int = dial_gui_app_wait(app, 16);
    if (ev == 8) { dial_gui_app_quit(app); }   // 8 = CLOSE
}
dial_gui_window_close(win);
dial_gui_app_free(app);
```

控件框架层用法（`GuiWidgetEvent` 必须在**文件顶层**声明，与 C 侧 packed 布局逐字节一致）：

```dial
packed extern struct GuiWidgetEvent {
    kind: int, x: int, y: int, button: int, key: int, w: int, h: int,
    local_x: int, local_y: int, tag: int, userdata: int,
    win_title: rawptr<byte>, widget_id: rawptr<byte>
}
// ... main 里，tree 是独立纯内存树（不开窗也能布局 / 命中）：
var tree: GuiWidget = dial_gui_widget_tree(200, 100);
var b1: GuiWidget = dial_gui_widget_add(tree);
dial_gui_widget_set_id(b1, "btn_ok");
dial_gui_widget_set_tag(b1, 1);
dial_gui_widget_set_layout(tree, 1);        // 1 = VBOX
dial_gui_widget_set_padding(tree, 10);
dial_gui_widget_set_gap(tree, 5);
dial_gui_widget_layout_apply(tree, 200, 100);

// id 是 C 侧借用指针，必须复制成自己的 ARC string
print(string_from_cstr(dial_gui_widget_id(b1)));        // btn_ok
print(str(dial_gui_widget_tag(dial_gui_widget_hit(tree, 15, 15))));
print(str(sizeof<GuiWidgetEvent>()));                   // 104（编译期常量）
dial_gui_window_set_root(win, tree);                    // 此后树归窗口所有

// 主动分发：poll/wait 后读 packed 纯值块，绝不注册回调
var ev: rawptr<GuiWidgetEvent> =
    ptr_cast<rawptr<GuiWidgetEvent>>(dial_gui_event_data(app));
print(str(ev.kind));                                    // 纯值直接读
var title: string = string_from_cstr(ev.win_title);     // 借用指针 → ARC 复制
```

成品控件外观层用法（不注册回调：纯值事件喂进树、整树一次绘制）：

```dial
var tree: GuiWidget = dial_gui_widget_tree(240, 120);
var lab: GuiWidget = dial_gui_widget_add(tree);
var btn: GuiWidget = dial_gui_widget_add(tree);
var cb: GuiWidget = dial_gui_widget_add(tree);
dial_gui_widget_set_kind(lab, 1);          // LABEL（label 是关键字，变量名避开）
dial_gui_widget_set_kind(btn, 2);          // BUTTON
dial_gui_widget_set_kind(cb, 3);           // CHECKBOX
dial_gui_widget_set_text(btn, "OK");
dial_gui_widget_set_text(cb, "Enable AA");
dial_gui_widget_set_layout(tree, 1);       // VBOX 三等分
dial_gui_widget_layout_apply(tree, 240, 120);

var p: GuiPainter = dial_gui_painter_offscreen(240, 120);
dial_gui_set_aa(p, 1);                     // 关 AA，色值 / CRC 完全确定
// 事件循环里把纯值事件喂进树：四态在 C 侧推进，绝不反向回调
dial_gui_widget_handle(tree, 3, 20, 60, 0);            // 3 = MOVE → hovered
dial_gui_widget_handle(tree, 1, 20, 60, 1);            // 1 = PRESS → pressed
var act: int = dial_gui_widget_handle(tree, 2, 20, 60, 1);  // 2 = RELEASE → 激活
dial_gui_widget_draw(tree, p);                        // 整树一次画到任意画布
print("crc = " + str(dial_gui_painter_crc32(p)));
```

输入层用法（同样是纯值事件主动喂进树，绝不注册回调）：

```dial
var tree: GuiWidget = dial_gui_widget_tree(320, 160);
var box1: GuiWidget = dial_gui_widget_add(tree);
var box2: GuiWidget = dial_gui_widget_add(tree);
dial_gui_widget_set_kind(box1, 4);          // 4 = TEXTBOX
dial_gui_widget_set_kind(box2, 4);
dial_gui_widget_layout_apply(tree, 320, 160);
dial_gui_widget_set_focus(tree, box1);      // 也可 widget_focus_next / 点击聚焦
dial_gui_widget_set_max_len(box1, 64);      // 0 = 不限；越界明确报错

// 事件循环里：一条键盘纯值事件 = kind + key + mods，C 侧推进编辑态 / 焦点环
var ev: int = dial_gui_app_wait(app, 16);
if (ev == 4) {                              // 4 = KEY_PRESS
    var r: int = dial_gui_widget_key(tree, ev, dial_gui_event_key(app),
                                     dial_gui_event_mods(app));
    // r: 1 文本/光标变化 / 2 焦点变化(Tab) / 3 Enter 提交 / 0 无变化
}
if (ev == 1 or ev == 2 or ev == 3) {        // 鼠标：推进四态 / 点击聚焦
    dial_gui_widget_handle(tree, ev, dial_gui_event_x(app),
                           dial_gui_event_y(app), dial_gui_event_button(app));
}
dial_gui_widget_draw(tree, wp);             // 焦点边框 / 选区 / 光标一起画
print(string_from_cstr(dial_gui_widget_text(box1)));   // 借用指针 → ARC 复制
```

> 完整可运行 demo 见 `tests/pkg/use_gui.dial`（地基层，用 `rgba(a,r,g,b)`
> 拼颜色）、`tests/pkg/use_gui_widget.dial`（控件框架层，**不创建 `GuiPainter`**，
> 因此不触碰 `crc_offscreen = 4007435570` 像素回归基准）、
> `tests/pkg/use_gui_look.dial`（外观层，用自己的 offscreen 画布、关 AA，
> 硬钉 `look_crc_normal = 3884455222`）、`tests/pkg/use_gui_input.dial`
> （输入层，编辑 / 选区 / UTF-8 边界 / 焦点环 / `max_len` / 统一键码 / 实窗
> 事件闭环，同样用自己的 offscreen 画布，硬钉 `input_crc_focused = 4147329629`）
> 与 `tests/pkg/use_gui_theme.dial`（主题层 / 控件扩展，14 角色
> `theme_set` / `theme_get` / `theme_reset` 往返与幂等、八种控件、
> Radio 同组互斥、Slider 按 x 设值、Progress 只读、值域 Fail-Fast；同样独立
> offscreen 画布，结束前显式 `theme_reset()` 不污染）。主题是**进程级静态表**，
> 一次 `dial_gui_theme_set(4, rgba(255,200,10,20))` 改掉按钮普通态颜色后，
> 下一次 `widget_draw` 立即生效；`dial_gui_theme_reset()` 逐值复位，
> 逐像素 CRC 回到改色前的默认主题（默认值不漂移，各自用例的 CRC 基准互不污染）。
> 无 DISPLAY 时窗口路径打印 `window = skipped [原因]` /
> `widget_window = skipped [原因]` / `look_window = skipped [原因]` /
> `input_window = skipped [原因]` / `theme_window = skipped [原因]`，
> **不伪装通过**；**真实鼠标 / 键盘注入已由
> `tests/pkg/xtest_inject.sh`（Xvfb + xdotool/XTEST）实机验收**：点击聚焦 /
> 键盘落字 / Tab·Shift+Tab 焦点环 / 按钮激活 / 统一键码 + mods 逐条断言，
> 无 XTest 环境才如实打印 `input_xtest_injection = skipped`。

### 38.10 AA 组回归与平台边界

`tests/run_tests.sh` 新增 **AA 组**（全量从 A–Z 二十六组扩为
A–Z + AA + AB **二十八组**），依次验收：

1. `packages/gui/build.sh` 三平台 `.a` 构建（零警告）+ `stb` SHA256 自检；
2. `nm -u` 检查三份 `.a` 无平台 GUI 未定义符号；
3. `dialc pack` / `install` / `import` / 编译 / 运行闭环；
4. **无 DISPLAY 路径**：离屏画布 24 项断言（尺寸 / AA / 各图元 / 裁剪 /
   点阵与 TrueType 字体 / 坏字体路径 / `get_pixel` / CRC32），窗口明确跳过；
5. **控件框架层无 DISPLAY 路径**（`use_gui_widget.dial`，不创建 `GuiPainter`）：
   `sizeof<GuiWidgetEvent>() == 104` 与各 `offsetof` 编译期内建断言、
   `string_from_cstr` 的 ARC 复制、树增删遍历与防悬垂、VBox/HBox 等分与幂等、
   固定尺寸优先、命中（后加子优先 / 不可见跳过）、窗口明确跳过；
6. **Xvfb 实窗路径**（`+extension MIT-SHM +extension RENDER`）：
   严格断言 `window = ok` / `window_backend = x11/xshm/xrender ...` /
   `window_flush = ok` / `quit_event = ok`；有 `xvfb-run` 就必须实机验证，
   没有则明确打印「真实窗口路径未实机验证」，绝不把未验证写成通过；
7. **控件框架层 Xvfb 实窗**：`window_set_root` / `widget_tree_free` 挂窗后拒绝、
   QUIT 主动分发、packed 纯值块字段读取、`string_from_cstr` 对 NULL 借用指针
   返回空串；命中逻辑由无头纯 C 树覆盖，真实鼠标注入由第 10 项统一实机验收；
8. **成品控件外观层**（`use_gui_look.dial`，独立 offscreen 画布、关 AA）：
   无 DISPLAY 路径断言三种控件种类 / 非法种类拒绝 / 文本 `string_from_cstr`
   ARC 复制 / VBOX 三等分 / 四态背景逐像素色值（`get_pixel`）与 CRC 互异 /
   移出画布复位幂等 / 禁用配色 / CheckBox 勾选翻转 / `look_crc_normal` 硬钉；
   Xvfb 实窗断言 `window_set_root` 挂树 + 整树绘制 + `flush` 上屏全通过；
9. **输入层无 DISPLAY**（`use_gui_input.dial`，独立 offscreen 画布、关 AA）：
   TEXTBOX 种类 / 非法种类拒绝 / 焦点环（set / next / prev / 拒绝非 TEXTBOX /
   摘除子树清焦点）/ 可打印字符插入 / Shift 选区与替换 / DELETE / BACKSPACE /
   HOME / END / UTF-8 三字节字符按字符边界移动 / `max_len` 越界设置明确报错 /
   Tab·Enter 返回值语义 / 焦点态边框逐像素色值与 `input_crc_focused` 硬钉；
10. **XTest 实机注入**（Xvfb + `xtest_inject.sh` + `xdotool`，真实 X11 客户端
   事件）：点击第一个 TextBox 聚焦、键盘真打 `hi` 落字、Tab 前进焦点、
   Shift+Tab 反向（断言 `mods` 含 SHIFT）、点击 BUTTON 真实激活；逐条断言
   `input_xtest_keys` / `tabs` / `shift_mods` / `mouse_events` /
   `mouse_activation` / `typed_text` / `focus_back` / `flush`。`libXtst` 只在
   测试侧，包本体 `.a` 仍零平台链接期依赖；无 `xdotool` 时如实打印
   `input_xtest_injection = skipped`，不伪装通过；
11. **主题层 / 控件扩展**（`use_gui_theme.dial`，独立 offscreen 画布、关 AA）：
   14 角色默认值 / 非法 role·颜色 Fail-Fast / `theme_set`-`get` 往返 /
   `theme_reset` 后 CRC 幂等 / 八种控件种类与非法值拒绝 / 改色后逐像素生效 /
   Radio 同组互斥与未分组独立 / Slider 按局部 x 设值单调且落在值域 /
   Progress 只读 / `set_value` 越界与 `set_range` 倒置拒绝、改值域后 clamp 可读回；
   Xvfb 实窗断言八种控件整树绘制 + `flush` 上屏全通过；
12. 卸载 `gui` 包。

全量实测 `EXIT=0`，**535 ✅ / 0 ❌**（AA 组 23 条 `aapass`：地基层 8 条 +
控件框架层 3 条 + 外观层 3 条 + 输入层 3 条 + 主题层 3 条 + 信号层 3 条；W 组上一轮 +4 条
http 缺陷回归；本轮新增 AB 组 4 条断言 + B 组 5 条零隐式转换负向）；
Xvfb 实窗能力串 `x11/xshm/xrender argb32 filter=best visual_depth=24`，
地基层像素 CRC 稳定 `4007435570`、外观层 `look_crc_normal` 稳定
`3884455222`、输入层 `input_crc_focused` 稳定 `4147329629`；
XTest 实窗注入实测 `keys 2 / focus 2 / mouse 6 / act 2 / shift 1` 全绿。

### 38.11 能力边界（不许当已支持）

- **十五层已实现**：地基层（窗 / 事件 / 绘图 / 字体）、**控件框架层**
  （Widget 树 / 绝对·VBox·HBox 布局 / 命中测试 / 主动事件分发）、**成品控件
  外观层**（Label / Button / CheckBox + 内置浅色默认主题 + 四态绘制 + 纯值
  事件推进）、**输入层**（TextBox 单行编辑 / 光标·选区 / 焦点链 / 统一功能
  键码 / 修饰键）、**主题层 / 控件扩展**（14 角色可覆写颜色表 + Radio / Slider /
  ProgressBar + value / range / group）、**信号槽 / 控件事件订阅层**（6 类可订阅
  纯值信号 + 每树环形队列，见 §38.12）、**菜单 / 对话框覆盖层**（顶栏菜单 + 弹出
  菜单 + 模态对话框，复用第 6 层纯值信号队列，见 §38.13）、**滚动视图层**
  （视口裁剪 + 滚动条几何 / 命中 / 绘制 + 偏移推进，零 DISPLAY 依赖，见 §38.15）
  + **右键上下文菜单层**（复用第 7 层菜单 + 锚点 / 命中 / 悬停 / 选中 / 绘制，
  零 DISPLAY 依赖，见 §38.16）+ **悬浮提示层**（文本 + 位置 + 尺寸 + 悬停计时，
  延迟由调用方推进、零时钟依赖，见 §38.17）+ **状态栏层**（左 / 右文本 + 进度条
  槽位，进度值越界夹紧、零时钟依赖，见 §38.18）  + **通知气泡层**（FIFO 消息队列，
  生命周期纯值推进、到期自动消隐、零时钟依赖，见 §38.19）
  + **窗口级覆盖层自动集成**（5 类覆盖层挂窗、`win.draw()` 固定 z-order 自动叠加、
  空槽位零像素影响、借用语义，见 §38.20）+ **快捷键 / 命令系统**（按键组合 →
  命令 id + 启用态，纯内存表、零 DISPLAY 依赖，见 §38.21）都已有真实现并写进
  `gui.diali`（共 **279 个 `extern fn`**，截至第 15 层；第 16 / 17 / 18 层追加见
  §38.22 / §38.23 / §38.24，现共 **344 个**）；**第 8 层 Dial 侧 OO 封装层**
  （`gui_oo.dial`，278 个 `impl` 方法，纯 Dial 源码、随包 `src/` 分发、零
  `extern fn` 增量，见 §38.14）也已落地（第 16 / 17 / 18 层同步扩到 **343 个方法**）。
  主题色仍以**进程级全局表**为默认真相源，但第 17 层已支持**每控件角色覆写**
  （未覆写角色逐像素回落全局表，见 §38.23.3）；Slider 已接通**左键拖动设值**
  （PRESS 置态 / MOVE 按绝对 x 设值 / RELEASE 清态，见 §38.23.2）；TextBox 已有
  **快照式撤销 / 重做栈**（见 §38.23.1）；Grid 已有**自动换行**（见 §38.23.4）；
  第 18 层已支持**高 DPI 整数放大**（见 §38.24.2）、**ListBox 虚拟化 lazy load**
  （见 §38.24.3）、**IME 预编辑纯值事件**（见 §38.24.4）、**GPU+原生平台纯值契约层**
  （见 §38.24.5）。
  **仍未实现、也不写进契约**：C → Dial 反向回调（信号槽已由第 6 层以纯值信号
  队列落地，**不是**回调）、富文本编辑（TextBox 只做单行）、图片解码、
  多行文本 / 复杂文本排版（HarfBuzz 级 shaping）、**可切换深色主题包**（单控件覆写 ≠ 主题包切换）、
  编辑合并 / 跨控件全局历史 / 历史持久化、ComboBox 可编辑输入
  ——把未实现函数写进契约会让用户 `import` 后链接失败，那是制造漂移而非预留接口。
  Dial 侧仍可用 `GuiPainter` 在图元层自由自绘；
- Windows 后端**未实机验证**（沙箱只能交叉编译），Direct2D **未落地**；
  XTest 真实事件注入**仅在 Linux / Xvfb 实机验收**，Win32 的键码映射与
  `mods` 跟踪是同表真实现但**未实机运行**；
- 功能键编码**两平台已统一**到 `0x110000+` 同表（§38.2 键码表），表外键一律
  收敛为 `GUI_KEY_UNKNOWN`，不承诺逐键同值但绝不漏出平台私有值；
- 字体只加载**本机可信**文件，绝不接受网络字体；内置点阵仅覆盖 ASCII
  `0x20..0x7E`，非 ASCII 走 TrueType 路径；
- 事件循环是**单线程**模型，`GuiApp` 的事件槽每 App 一份，不要多线程共享；
- `.diali` 的 `extern fn` 必须**单行写完整**（与 http / json / crypto 包同口径），
  跨行声明会被 `parse_diali` 误判返回类型。

### 38.12 信号槽 / 控件事件订阅层（第 6 层）

> 本节是第 6 层的完整设计说明；契约签名见 `gui.diali` 末段，C 实现见
> `packages/gui/src/dial_gui_signal.c`，端到端用例见 `tests/pkg/use_gui_signal.dial`。

**为什么是「纯值信号队列」而不是回调**。Qt 式 `connect(signal, slot)` 会把 Dial
函数指针注册进 C，但 Dial 无 GC、全靠 ARC + 手动释放；C 侧若在平台线程直接调
Dial 函数，跨线程引用计数管理会失控。因此本层把「信号槽」落地为：**C 侧只在
语义状态真的变化处写一条纯值信号到每树环形队列，Dial 侧主动 `pending → data
→ pop` 读走并自行分派**。C 从不保存函数指针、从不认识 Dial 对象、从不碰 ARC，
**绝不 C → Dial 反向回调**——与前五层的内存宪法完全一致。

**六类信号与订阅掩码**。每控件一个 `int64` 订阅掩码（`widget_subscribe(w, mask)`，
`mask` 是下表位值的按位或；`0` 表示取消全部订阅）。信号码编号跨版本稳定，
只准末尾追加新位：

| 位值 | 信号 | 触发点 | `a` / `b` |
| --- | --- | --- | --- |
| `1` | `CLICKED` | BUTTON 在 RELEASE 激活（`look.c`） | `1` / `0` |
| `2` | `TOGGLED` | CHECKBOX 翻转 / RADIO 选中 / `widget_set_checked` | 新 `checked` / 旧 `checked` |
| `4` | `TEXT_CHANGED` | TEXTBOX 键盘编辑成功 / `widget_set_text` | 新字节数 / 旧字节数 |
| `8` | `VALUE_CHANGED` | SLIDER 按局部 x 设值 / `widget_set_value` | 新值 / 旧值 |
| `16` | `FOCUS_CHANGED` | 焦点迁移（点击聚焦 / Tab / `set_focus`） | `1` 获得 / `0` 失去，`b=0` |
| `32` | `SUBMITTED` | TEXTBOX Enter 提交 | `1` / `0` |

埋点在既有状态推进处（`gui_widget_handle` / `gui_widget_key` / `set_focus` /
`focus_next` / `set_text` / `set_checked` / `set_value`），**只增副作用**：每个信号
只由状态**真的变化**的那一次交互 / setter 触发一次。同值 setter、同组 RADIO 被动
清除、`set_range` 的 clamp 都不另发信号（边界如实标注）。

**72 字节纯值信号块 `GuiWidgetSignal`**（`packed`，9 × `int64`）：

| 偏移 | 字段 | 语义 |
| --- | --- | --- |
| 0 | `seq` | 单调递增序号（`clear` 不重置，便于 Dial 检测跳号） |
| 8 | `signal` | 单个 `GUI_SIG_*` 位值 |
| 16 / 24 | `tag` / `userdata` | 发出控件的**纯值键**（Dial 侧自建 map 定位控件） |
| 32 | `kind` | 控件种类（`GUI_WIDGET_*`） |
| 40 / 48 | `a` / `b` | 主值 / 副值（见上表） |
| 56 / 64 | `x` / `y` | 事件坐标（键盘类信号为 0） |

与 104 字节 `GuiWidgetEvent` 的**关键区别**：本块 **100% 纯值、零指针、零句柄**——
连控件句柄都不存，只存 `tag` / `userdata` 两个 `int64` 纯值键。因此控件被摘除、
文本被覆盖、整树被释放后，已入队信号仍是自洽整数快照，**读数绝不悬垂**，也
不需要 `string_from_cstr` 复制成 ARC 对象。布局由 C 侧
`_Static_assert(sizeof==72, offsetof(y)==64)` 与 Dial 侧 `sizeof<T>()` /
`offsetof<T>(field)` 编译期内建**双向锁死**（`use_gui_signal.dial` 逐项断言）。

**零订阅 = 零开销（防漂移的关键）**。每控件订阅掩码默认 `0`（`calloc` 归零），
`gui_signal_emit` 第一行就查掩码，未订阅直接返回——不找根、不分配、不入队。
所有不调用 `subscribe` 的旧程序，行为 / 绘制 CRC / 函数返回值逐字节不变，
历史 `523 ✅` 断言不漂移。队列只挂在**根节点**、容量 `GUI_SIGNAL_CAP = 64`、
首次 emit 时才 `calloc`（零订阅连队列都不分配）；`dispose_subtree` 统一 `free`
（`free(NULL)` 安全），挂窗树随 `window_close` 释放，绝不泄漏。

**Fail-Fast（§20）**：
- `widget_subscribe` 的 `mask` 为负、含未知位或超出 `0..63` 一律返回 `-1` 并写
  错误槽，绝不静默截断；`widget_subscribed` / `widget_unsubscribe` 读改同一掩码。
- 队列满（64）时**覆盖最旧、保留最新**，并把根上 `sig_dropped` 累计 `+1`；
  `calloc` 失败同样计数。Dial 侧用 `widget_signal_dropped(root)` 读「漏了多少条」，
  可断言它恒为 0 证明消费及时——**任何信号丢失都可被观测，绝不静默损坏**
  （与 `http.dialpkg` 修掉的「512 字节静默截断」同一口径）。
- `signal_data(root)` 返回队首 **C 借用**指针，空队列返回 NULL。Dial 侧须在下次
  emit / pop / clear / 树释放之前 `ptr_cast` 读完 9 个字段，绝不长期持有（纯值无
  悬垂，但复用后会脏读）。

**Dial 侧主动分派（典型循环，绝不注册回调）**：

```dial
import "gui";

// 与 C 侧 GuiWidgetSignal（packed，72 字节）逐字节一致
packed extern struct GuiWidgetSignal {
    seq: int, signal: int, tag: int, userdata: int, kind: int,
    a: int, b: int, x: int, y: int
}

fn dispatch(root: GuiWidget): void {
    while (dial_gui_widget_signal_pending(root) > 0) {
        var raw: rawptr<byte> = dial_gui_widget_signal_data(root);
        var s: rawptr<GuiWidgetSignal> = ptr_cast<rawptr<GuiWidgetSignal>>(raw);
        // 用 s.tag / s.userdata 纯值键在自建 map 里定位控件，按 s.signal 分派：
        //   1 CLICKED / 2 TOGGLED / 4 TEXT_CHANGED / 8 VALUE_CHANGED /
        //   16 FOCUS_CHANGED / 32 SUBMITTED；s.a / s.b 是新值 / 旧值。
        dial_gui_widget_signal_pop(root);        // 读完立即弹出
    }
    if (dial_gui_widget_signal_dropped(root) > 0) {
        // 明确知道漏了信号，可据此报错——绝不静默吞掉
    }
}
```

事件循环里仍由 Dial 侧 `poll/wait` 后主动 `widget_handle` / `widget_key` 推进
控件状态，状态变化顺带 emit 信号，再调 `dispatch(root)` 消费——C 从不主动调
Dial。**信号订阅是每控件粒度的**（订阅掩码存在 `GuiWidgetImpl` 上），因此可以给
一个按钮只订 `CLICKED`、给文本框只订 `TEXT_CHANGED`，互不串扰。

**平台边界 / 回归**：本层是纯 C、零平台符号，三份 `.a` 的 `nm -u` 不新增任何
X11 / GDI 未定义符号；X11 / Win32 两端无需改动（信号由平台无关的控件逻辑产生）。
`use_gui_signal.dial` 用独立纯内存树在**无 DISPLAY** 下完整断言：72 字节布局
双向锁死、6 类信号 / 容量常量、零订阅零开销、订阅掩码 Fail-Fast、六类信号逐字段
（含 x/y）读取、`seq` 单调、队列溢出 `dropped` 可观测、取消订阅、纯值块读数不依赖
控件生命周期；Xvfb 实窗另验挂树 + 订阅 + 纯值信号读取 + 整树绘制 + `flush` 上屏。
AA 组本层 +3 条 `aapass`，全量回归 **523 ✅ → 526 ✅**。

### 38.13 菜单 / 对话框覆盖层（第 7 层）

> 本节是第 7 层的完整设计说明；契约签名见 `gui.diali` 末段，C 实现见
> `packages/gui/src/dial_gui_menu.c`，端到端用例见 `tests/pkg/use_gui_menu.dial`。

**为什么是「窗口级覆盖层」而不是控件**。菜单（顶栏 + 弹窗）与对话框是**整窗
范围的临时 UI**：它们浮在控件树之上、按需出现、不参与控件树布局，因此刻意
**不进 `GuiWidget` 树**——这样控件几何 / 绘制 CRC 逐字节不漂移（与覆盖层无关的
旧 demo 像素断言全部保留）。三者所有权归 `GuiWindow`：窗口关闭时 `dial_gui_window_close`
一并 `gui_menu_free` / `gui_dialog_free`，调用方**不要**对已挂窗的菜单 / 对话框再 free。

**信号复用，不另起炉灶**。菜单项选中、对话框按钮点击**复用第 6 层纯值信号队列**，
不新增队列、不新增信号种类语义，只是新增两个信号码与一个 kind 标记：

- `GUI_SIG_SELECTED = (1 << 6) = 64`：菜单项被点击选中，`a = item id`，
  `kind = GUI_WIDGET_MENU (= 8)`；
- `GUI_SIG_SUBMITTED`（第 6 层按钮 Enter 提交）：对话框按钮点击时 `a = 结果码`
  （`1 OK` / `2 CANCEL` / `4 YES` / `8 NO`，取点击的那一枚），`kind = GUI_WIDGET_DIALOG (= 9)`；
- 信号码总数 `GUI_SIG_COUNT` 从 6 升到 **7**，掩码上限同步放开（含 `64` 位合法）。

**关键约定：`set_root` 先挂树**。覆盖层事件要进 `win->root` 的队列，故
`window_set_root` 必须先调；`window_set_menubar` / `window_popup_menu` / `dialog_show`
在 `root` 未挂时一律返回 `-1` 并写错误槽（Fail-Fast）。同时，菜单 / 对话框的
`SELECTED` / `SUBMITTED` 信号走的 anchor 是 `win->root` 本身，因此**必须订阅 root**
（而非某个按钮）到对应位——零订阅零开销守卫要求 root 自身持 `64` / `32` 位，否则
信号被丢弃。`gui_signal_emit_ex` 是这套「非控件来源、显式给定 kind/tag/userdata」
发射的变体。

**统一事件分发的优先级**（`window_handle` 一次调用，返回语义与旧控件层一致）：

1. 对话框（最高优先级，模态）：按钮命中 → 写 `result` + 关闭 `open` + emit `SUBMITTED`
   + 返回 `3`；对话框外点击忽略（模态）；
2. 弹出菜单：命中项 → emit `SELECTED` + **选中即释放弹窗** + 返回 `3`；外部点击关闭弹窗、返回 `0`；
3. 顶栏菜单：命中项 → emit `SELECTED` + 返回 `3`；
4. 控件树：委派 `widget_handle`，旧行为不变（返回 `1` = 控件激活 / `0` = 无 / `-1` = 参数错）。

菜单项几何：`GUI_MENU_BAR_H = 22` 为顶栏高度，应用自行在控件树顶部留 `22px` 垫片；
弹出菜单行高 `20px`、分隔符与禁用项不可命中；对话框按钮按下则 `open = 0` 自动消失。

**纯内存对象也能单测（无 DISPLAY 可用）**：`dial_gui_menu_new` / `dial_gui_dialog_new`
产生的纯内存对象（未挂窗）直接用 `gui_menu_free` / `gui_dialog_free` 释放，无需任何
显示服务器。`use_gui_menu.dial` 段 A / B 在无 DISPLAY 下完整断言菜单项的头插排序 /
越界返回 `-1` / 禁用 / `tag` 纯值键、对话框标题 / 消息 / 按钮位 / 结果码；段 C 在
Xvfb 实窗另验：顶栏点击 → `SELECTED(a=item id, kind=MENU, tag 透传)`、弹窗点击命中
`Paste`、模态对话框点 `OK` → `SUBMITTED(a=1, kind=DIALOG)` 且对话框自动关闭、控件树
按钮仍收得到 `CLICKED`。AA 组本层 +2 条 `aapass`（无 DISPLAY / Xvfb 各一段）。

**平台边界 / 回归**：本层是纯 C、零平台符号，三份 `.a` 的 `nm -u` 不新增任何
X11 / GDI 未定义符号；X11 / Win32 两端无需改动（菜单 / 对话框绘制走平台无关的
`gui_raster_*` / `gui_font_*`）。七层合计 **154 个 `extern fn`**，`abi = 6` /
`version = 1.10.0` 不变、旧 130 个签名一字不动。

---

### 38.14 Dial 侧 OO 封装层（第 8 层）

> 前七层都是 **C 契约**：`gui.diali` 里 279 个 `extern fn`（前七层 154 + 第 9 层
> 滚动视图 15 + 第 10 层右键上下文菜单 15 + 第 11 层悬浮提示 19 +
> 第 12 层状态栏 20 + 第 13 层通知气泡 23 + 第 14 层窗口级覆盖层自动集成 15 +
> 第 15 层快捷键 / 命令系统 18，见 §38.15 ~ §38.21）。第 8 层换一种
> 形态——**不新增任何 `extern fn`**，而是用 §37.12 落地的 B1 `impl` 方法块，在
> `packages/gui/src/gui_oo.dial` 里写 **278 个纯
> Dial 方法**，把 C 风格调用编译期
> 脱糖成习惯写法。这一层是**纯源码**，随包 `src/` 分发（见 §25.11）、
> 不碰三平台 `.a`、`abi = 6` / `version = 1.10.0` 不变、旧 154 个签名一字不动。

**用法**（装包即用，见 §25.11 / §34）：装好 `gui.dialpkg` 后写一行裸名 import，
即可从包内 `src/gui_oo.dial` 自动展开本模块，**无需复制任何文件到项目目录**：

```dial
import "gui_oo";
```

若项目目录自己放了一份同名的 `gui_oo.dial`，则**本地那份优先**（三级优先见
§34.2），方便临时改源码试验而不必动已安装的包；也可显式写
`import "./gui_oo.dial";` 走本地路径。文件内的 `import "gui";` 由前端收集所有
模块后**汇总、去重、全局生效**，故用户不必再写第二个 import；随后
`dial_gui_app_new()` 建 app，其余全部走方法：

```dial
var app: GuiApp = dial_gui_app_new();
var win: GuiWindow = app.window_new("Demo", 400, 300);
var root: GuiWidget = app.widget_tree(400, 300);
win.set_root(root);
win.draw();
win.flush();                     // == dial_gui_window_flush(win)
```

**为什么必须是独立源码文件**：`.diali` 是带 SHA256 校验的**签名契约**，
`parse_diali` 只认 `type X;` 与 `extern fn`，不允许函数体（见 §25.2）；
`dialc pack` 的清单只含 `manifest.txt` + `.diali` + 各平台 `.a`，`dial.toml`
未知字段一律报错。要让 `import "gui";` 自动带上方法，就得改打包器、改前端、
改 LSP 并重发已发布的包——与本层「零改动」的立场正面冲突。所以第 8 层按语言
既有的**本地源码模块**机制交付，是最小且可逆的路径。

**映射规则**（便于逐条审计，AD 组脚本会机械校验）：

| 规则 | 说明 |
| --- | --- |
| 接收者 | 第一个形参是 7 个句柄之一 → 挂到该类型并降为 `self`；否则归到 `GuiApp` 作命名空间（`self` 被忽略） |
| 方法名 | 去掉 `dial_gui_` 与接收者类型前缀（`app_` / `window_` / `painter_` / `font_` / `widget_` / `menu_` / `dialog_`） |
| 保留字改名 | `widget_select` → `select_range`（`select` 是关键字）；`menu_add_item` 形参 `label` → `txt`（`label` 是关键字） |
| 覆盖 | 279 个 `extern fn` → **278 个方法** + 1 个入口 `dial_gui_app_new`（B1 不支持关联函数，无实例可挂） |

13 组接收者分布：`GuiApp` 39（含进程级全局状态 / 工厂，第 15 层多 `hotkey_new`）、
`GuiWindow` 33（含第 14 层 15 个覆盖层槽位方法）、`GuiPainter` 20、`GuiFont` 3、
`GuiWidget` 64、`GuiMenu` 9、`GuiDialog` 6、`GuiScroll` 14、`GuiContext` 14、
`GuiTooltip` 18、`GuiStatusBar` 19、`GuiToast` 22、`GuiHotkey` 17，
合计 **278 个方法**。

**产物不漂移（硬证据）**：`use_gui_oo.dial` 让**同一段绘制序列**分别在两个离屏
画布上执行——一份全用 C 风格 `dial_gui_*`，一份全用方法——最后比较
`painter_crc32()` **逐字节相等**（`oo_painter_crc_matches_c`）。因为 `impl` 在
parser 阶段就降级为普通顶层函数、调用点在语义与代码生成两处就地改写成
`Expr::Call` 复用整条调用链，产物里根本不存在「方法」这个概念。

**现役边界（如实标注，不夸大）**：

- B1 不支持关联函数 → app 自身仍用 `dial_gui_app_new()`；工厂以
  `app.window_new(...)` / `app.menu_new()` 提供，`self` 仅作命名空间。
- `self` 是值语义句柄副本；opaque 句柄本质是指针，方法内操作作用于同一对象。
- 封装层不定义顶层辅助函数 / `const`（避免与用户源码跨文件撞名），也不重复
  声明 `extern fn dial_gui_*`（会与 `.diali` 撞名被 Fail-Fast 拒绝）。
- LSP 当前**不递归源码模块**（v1.04 既有局限，包内 `src/` 模块同理），故
  `import "gui_oo";` 在编辑器里可能被误报「找不到已安装的包」；CLI `build` /
  `run` / `check` / `test` 全走 `frontend`，不受影响。

**回归（AD 组，第 30 组）**：契约审计（278 方法恰好覆盖 278 个 `extern fn`、
`gui.diali` 共 279 个签名、封装层零 `extern fn` 声明）、无 DISPLAY 纯内存路径
（含 CRC 对照与 Fail-Fast 透传）、Xvfb 实窗路径（顶栏 / 弹窗 / 对话框 / 控件树
全走方法，信号队列与 AA 组逐项对齐）、3 个负向用例（未知方法 / 零隐式转换 /
参数个数）+ 诊断面向用户（显示 `GuiMenu.add_item`、参数个数不含 `self`、
不暴露 `__impl_` mangle 名）。第 14 层 15 个窗口覆盖层方法与第 15 层 18 个快捷键
表方法同样纳入本组契约审计（278 ↔ 278 精确相等），实窗自动绘制回归在 AA 组
5m / 5n 块，快捷键 / 命令系统回归在 5o 块。

### 38.15 滚动视图层（第 9 层）

> 第 9 层给「内容大于视口」的控件树提供**滚动浏览**：视口裁剪 + 滚动条几何 /
> 命中 / 绘制 + 偏移推进。它走与第 7 层菜单 / 对话框覆盖层**完全相同的宪法边界**——
> **纯 C、零平台符号、绝不 C → Dial 反向回调**——只是把「菜单 / 对话框」换成
> 「滚动条」。15 个 `extern fn`（前缀 `dial_gui_scroll_*`），挂在 `GuiScroll`
> 不透明句柄上，与旧 154 个签名同契约、`abi = 6` / `version = 1.10.0` 不变、
> 旧签名一字不动。

**为什么是「窗口级辅助覆盖层」而不是新控件**：滚动视图**不进控件树**，而是作为
`GuiWindow` 的一个可选覆盖层（`win->scroll`）。调用方自己持有 `GuiScroll`，在
绘制时按 `off_x/off_y` 平移内容并设裁剪区（`dial_gui_set_clip`），再调
`draw_bar` 画滚动条；偏移是**纯值**，由 Dial 侧主动 `scroll_to` / `wheel` /
`drag` 推进，再用 `offset_x` / `offset_y` 读回。这样几何 / 绘制 CRC 逐字节不漂移，
且零反向回调。

**`which` 约定**：`0` = 水平滚动条、`1` = 垂直滚动条（与 `enable` 的掩码位一致，
`which = 2` 表示两者）。

**15 个 `extern fn`**（全部纯内存、可在无 DISPLAY 环境单元测试）：

| 函数 | 作用 |
| --- | --- |
| `dial_gui_scroll_new` / `dial_gui_scroll_free` | 新建 / 释放纯内存滚动视图（`free` 返回 0，空句柄返回 -1） |
| `dial_gui_scroll_set_viewport(s, x, y, w, h)` | 设视口矩形（窗口内坐标 + 尺寸，尺寸须正；Fail-Fast 返回 -1） |
| `dial_gui_scroll_set_content(s, w, h)` | 设内容总尺寸（须为正；Fail-Fast 返回 -1） |
| `dial_gui_scroll_enable(s, which, on)` | 启用 / 禁用某方向（which 0/1/2，on 非 0 即启用；越界 Fail-Fast） |
| `dial_gui_scroll_set_tag(s, tag)` / `dial_gui_scroll_tag(s)` | 用户 tag（**64 位全宽**，可 round-trip，与 `GuiMenu.tag` 同宽） |
| `dial_gui_scroll_offset_x(s)` / `dial_gui_scroll_offset_y(s)` | 读当前偏移（已夹紧；空句柄返回 0） |
| `dial_gui_scroll_scroll_to(s, x, y)` | 直接设偏移，**越界自动夹紧**到 `[0, 内容-视口]` |
| `dial_gui_scroll_wheel(s, dy)` | 滚轮推进（dy>0 向下滚），返回新 off_y（已夹紧） |
| `dial_gui_scroll_hit(s, x, y, which)` | 命中测试：0 未命中 / 1 命中滑块 / 2 命中轨道（须该方向已启用且内容>视口） |
| `dial_gui_scroll_drag(s, x, y, which)` | 拖拽推进：按鼠标位置纯几何换算滑块中心跟随，返回新 off（已夹紧） |
| `dial_gui_scroll_set_bar_size(s, sz)` | 设滚动条厚度（须 >= 10；Fail-Fast 过小返回 -1） |
| `dial_gui_scroll_draw_bar(p, s, which)` | 绘制某方向滚动条（轨道 + 滑块，主题色）；须该方向已启用且内容>视口，否则返回 0（不绘制），空句柄返回 -1 |

> **返回类型约定（防坑）**：本包所有声明为 `extern fn ...: int` 的函数，C 侧实现
> **必须返回 `int64_t`**——Dial 的 `int` 是 64 位，若 C 用 32 位 `int` 返回 -1，
> 会被零扩展成 `4294967295` 而非 `-1`（见 §33.2 / `dial_gui_dialog_set_buttons`
> 同口径）。第 9 层 13 个 `: int` 函数（含 `free`）因此一律 `int64_t` 返回。

**OO 封装**：第 8 层 `gui_oo.dial` 的 `impl GuiScroll` 用 15 个方法包住上述 15 个
`extern fn`（`GuiApp` 上挂 `scroll_new` 工厂），方法名与 `.diali` 一一对应（见
§38.14 映射规则）。AD 组契约审计因此由 153 / 154 同步为 **168 / 169**。

**颜色走主题表**：滚动条轨道 / 边 / 滑块一律从主题表取
（`C(TRACK)` / `C(BORDER)` / `C(BTN)`），`theme_set` 一次改色整树立即生效。

**回归（AA 组，第 30 组）**：`tests/pkg/use_gui_scroll.dial` 在无 DISPLAY 环境跑
纯内存对象 + 离屏画布全流程——夹紧（溢出 / 负值 / 中间值）、滚轮推进、命中
（轨道 / 滑块 / 无）、拖拽夹紧、tag 64 位全宽 round-trip、bar_size Fail-Fast、
离屏绘制 CRC32 像素级稳定（垂直 / 水平），39 条断言全绿、窗口路径明确标注 `ok`。

### 38.16 右键上下文菜单层（第 10 层）

> 第 10 层给「鼠标右键」提供**上下文菜单**：一份第 7 层菜单 + 一个弹出锚点，
> 打包成可复用的上下文菜单对象，负责打开 / 关闭 / 命中 / 悬停 / 选中 / 绘制。
> 它走与第 7 / 第 9 层**完全相同的宪法边界**——**纯 C、零平台符号、绝不
> C → Dial 反向回调**——15 个 `extern fn`（前缀 `dial_gui_context_*`），挂在
> `GuiContext` 不透明句柄上，与旧 154 个签名同契约、`abi = 6` /
> `version = 1.10.0` 不变、旧签名一字不动。

**复用第 7 层菜单，不重复实现**：本层**不新增任何菜单类型**。菜单项的增删 /
禁用 / tag 全部走既有的 `dial_gui_menu_*` API，`GuiContext` 只**借用**一份
`GuiMenu`，只管几何与状态。外观也直接复用第 7 层弹出菜单的绘制路径，故上下文
菜单与弹出菜单**逐像素同款**；行高常量 `GUI_MENU_ROW_H` 是两层共享的**单一
真相源**。

**所有权（借用语义）**：`set_menu` **不转移所有权**，`context_free` **也不释放
菜单**——菜单的生死始终归应用侧掌控。上下文菜单自身是纯内存对象，未挂任何
窗口时用 `context_free` 释放，可在无 DISPLAY 环境单元测试。

**状态推进（纯值，绝不自动捕获鼠标）**：打开 / 命中 / 悬停 / 选中全是纯值，由
应用主动推进，与第 9 层滚动视图同构——本层**绝不自动捕获鼠标事件**，也绝不
代替应用判断「该不该弹」。典型用法：应用收到右键（`button = 3`，X11 与 Win32
两后端取值一致）事件后调 `context_open(x, y)`，绘制时调 `context_draw`，鼠标
移动调 `context_hover`，左键点击调 `context_select`（命中则返回菜单项 id 并
**自动收起**）。

**命中行序 = 绘制行序**：本层的 `hit` / `hover` / `select` 与绘制走**同一条菜单
链表序**（`GuiMenuItem` 头插，故自上而下依次是「最后 `add` 的项」在前），
杜绝「点到的」与「看到的」不是同一行。分隔符与禁用项统一不可命中（返回 -1）。

**15 个 `extern fn`**（全部纯内存、可在无 DISPLAY 环境单元测试）：

| 函数 | 作用 |
| --- | --- |
| `dial_gui_context_new` / `dial_gui_context_free` | 新建 / 释放纯内存上下文菜单（`free` **不释放借用的菜单**，空句柄返回 -1） |
| `dial_gui_context_set_menu(c, m)` / `dial_gui_context_menu(c)` | 绑定 / 读回菜单（**借用**，不转移所有权；空句柄 Fail-Fast 返回 -1） |
| `dial_gui_context_set_tag(c, tag)` / `dial_gui_context_tag(c)` | 用户 tag（**64 位全宽**，可 round-trip，与 `GuiScroll.tag` 同宽） |
| `dial_gui_context_open(c, x, y)` | 在锚点展开（窗口内坐标）；未绑定菜单 Fail-Fast 返回 -1，重复展开重置悬停态 |
| `dial_gui_context_close(c)` / `dial_gui_context_is_open(c)` | 收起（保留绑定与锚点）/ 读展开态 |
| `dial_gui_context_anchor_x(c)` / `dial_gui_context_anchor_y(c)` | 读弹出锚点 |
| `dial_gui_context_hit(c, x, y)` | 命中测试：返回命中**行号**（0 起，与绘制行序一致）；未命中 / 分隔符 / 禁用项统一 -1 |
| `dial_gui_context_hover(c, x, y)` | 推进悬停态并返回命中行号（绘制时该行高亮） |
| `dial_gui_context_select(c, x, y)` | 点击选中：命中可选项时返回该菜单项 id 并**自动收起**；未命中返回 -1 且**保持展开** |
| `dial_gui_context_draw(p, c)` | 绘制（复用第 7 层弹出菜单外观）；未展开不绘制（返回 0），空句柄返回 -1 |

> **返回类型约定（防坑）**：本包所有声明为 `extern fn ...: int` 的函数，C 侧实现
> **必须返回 `int64_t`**——Dial 的 `int` 是 64 位，若 C 用 32 位 `int` 返回 -1，
> 会被零扩展成 `4294967295` 而非 `-1`（见 §33.2）。第 9 / 第 10 层全部 `: int`
> 函数因此一律 `int64_t` 返回。

**OO 封装**：第 8 层 `gui_oo.dial` 的 `impl GuiContext` 用 15 个方法包住上述 15 个
`extern fn`（`GuiApp` 上挂 `context_new` 工厂）；`select` 是 Dial 关键字，方法名
改为 `select_item`（同第 8 层 `widget_select → select_range` 的先例）。AD 组
契约审计因此由 168 / 169 同步为 **183 / 184**。

**回归（AA 组，第 30 组）**：`tests/pkg/use_gui_context.dial` 在无 DISPLAY 环境跑
纯内存对象 + 离屏画布全流程——未绑定菜单时 `open` Fail-Fast、菜单同一性（以
对象内 tag 间接验证）、锚点读回、命中（行序 / 分隔符 / 区域外）、悬停推进、
选中自动收起 / 未命中保持展开、tag 64 位全宽 round-trip、**借用的菜单不随
`context_free` 释放**、离屏绘制 CRC32 像素级稳定（重绘幂等 + 悬停态改变像素），
57 条断言全绿、窗口路径明确标注 `ok`。

### 38.17 悬浮提示层（第 11 层）

> 第 11 层给「鼠标停在某个控件上」提供**悬浮提示**：一段文本 + 位置 + 尺寸 +
> 悬停计时，打包成可复用的提示框对象。它走与第 7 / 9 / 10 层**完全相同的
> 宪法边界**——**纯 C、零平台符号、绝不 C → Dial 反向回调**——19 个
> `extern fn`（前缀 `dial_gui_tooltip_*`），挂在 `GuiTooltip` 不透明句柄上，
> 与旧 154 个签名同契约、`abi = 6` / `version = 1.10.0` 不变、旧签名一字不动。

**延迟由调用方推进，本层不读时钟**：这是本层最重要的设计取舍。悬浮提示的核心
语义是「悬停一段时间才弹出」，但**时钟是唯一会把三平台行为拉开的来源**。所以
本层**完全不读时钟、不调用任何平台 API**，而是由应用侧以毫秒数推进
`tick(dt_ms)`：

- 应用在鼠标移动事件里命中某控件 → `set_text(该控件的提示)` + `tick(本帧毫秒)`；
- 鼠标移开 → `reset()` 归零悬停计时。

于是「延迟弹出」变成一个**纯值状态机**：`hover_ms` 累计到 `delay_ms` 才
`shown = 1`。三平台行为逐字节一致，且可在无 DISPLAY 环境**确定性**单测（不依赖
真实时间流逝）。同理，本层**不捕获鼠标**——应用自行判断「鼠标在哪个控件上」。

**文本是 C 侧自有副本**：`set_text` 会拷贝一份（`strdup`，与 `GuiWidget.text` /
`GuiMenuItem.label` 同口径），调用方不保留指针；空串等价于清空文本，此时
`tick` 恒返回 0（**永不凭空弹出**）。换文本等价于换了目标——悬停计时归零、
立即取消显示。

**19 个 `extern fn`**（全部纯内存、可在无 DISPLAY 环境单元测试）：

| 函数 | 作用 |
| --- | --- |
| `dial_gui_tooltip_new` / `dial_gui_tooltip_free` | 新建 / 释放纯内存提示框（`free` 连带释放文本副本，空句柄返回 -1） |
| `dial_gui_tooltip_set_text(t, text)` / `dial_gui_tooltip_text(t)` | 设 / 读提示文本（**C 侧副本**；`text` 返回借用指针，空串表示无文本） |
| `dial_gui_tooltip_set_delay(t, ms)` / `dial_gui_tooltip_delay(t)` | 设 / 读悬停延迟（毫秒，须非负；Fail-Fast 返回 -1），默认 500 |
| `dial_gui_tooltip_set_pos(t, x, y)` / `dial_gui_tooltip_pos_x(t)` / `dial_gui_tooltip_pos_y(t)` | 设 / 读提示框左上角（窗口内坐标） |
| `dial_gui_tooltip_set_size(t, w, h)` / `dial_gui_tooltip_size_w(t)` / `dial_gui_tooltip_size_h(t)` | 显式设 / 读尺寸（须为正；Fail-Fast 返回 -1）；不调用时按文本自动算 |
| `dial_gui_tooltip_tick(t, dt_ms)` | 推进悬停计时（负值按 0 处理），返回 1 = 当前应显示 / 0 = 不显示 |
| `dial_gui_tooltip_reset(t)` | 归零悬停计时并立即取消显示（鼠标移出时调用） |
| `dial_gui_tooltip_shown(t)` / `dial_gui_tooltip_hover_ms(t)` | 读显示态 / 已悬停累计毫秒（纯值可观测） |
| `dial_gui_tooltip_set_tag(t, tag)` / `dial_gui_tooltip_tag(t)` | 用户 tag（**64 位全宽**，可 round-trip，与 `GuiScroll.tag` 同宽） |
| `dial_gui_tooltip_draw(p, t)` | 绘制提示框（主题底色 + 细边框 + 文本）；未达延迟 / 无文本则不绘制（返回 0） |

> **返回类型约定（防坑）**：本包所有声明为 `extern fn ...: int` 的函数，C 侧实现
> **必须返回 `int64_t`**——Dial 的 `int` 是 64 位，若 C 用 32 位 `int` 返回 -1，
> 会被零扩展成 `4294967295` 而非 `-1`（见 §33.2）。第 9 / 10 / 11 层全部
> `: int` 函数因此一律 `int64_t` 返回。

**OO 封装**：第 8 层 `gui_oo.dial` 的 `impl GuiTooltip` 用 19 个方法包住上述 19 个
`extern fn`（`GuiApp` 上挂 `tooltip_new` 工厂）。AD 组契约审计因此由 183 / 184
同步为 **202 / 203**。

**回归（AA 组，第 30 组）**：`tests/pkg/use_gui_tooltip.dial` 在无 DISPLAY 环境跑
纯内存对象 + 离屏画布全流程——默认延迟 500ms、**无文本时 tick 恒不显示**、文本
读回、延迟未到不显示但计时推进、累计达延迟才显示、`reset` 归零并取消显示、重新
累计、自定义延迟、**负延迟 Fail-Fast**、**换文本归零计时**、**空文本永不显示**、
位置 / 尺寸读取、**非法尺寸 Fail-Fast**、tag 64 位全宽 round-trip、离屏绘制
CRC32 像素级稳定（重绘幂等 + `reset` 后像素不变），51 条断言全绿、窗口路径明确
标注 `ok`。

### 38.18 状态栏层（第 12 层）

> 第 12 层给窗口底栏提供**状态栏**：左文本 + 右文本 + 可选进度条槽位，打包成
> 可复用的状态栏对象。它走与第 7 / 9 / 10 / 11 层**完全相同的宪法边界**——
> **纯 C、零平台符号、绝不 C → Dial 反向回调**——20 个 `extern fn`（前缀
> `dial_gui_statusbar_*`），挂在 `GuiStatusBar` 不透明句柄上，与旧 154 个签名
> 同契约、`abi = 6` / `version = 1.10.0` 不变、旧签名一字不动。

**纯值模型**：左右两段文本都是 C 侧 `strdup` 自有副本（与 `GuiWidget.text` /
`GuiMenuItem.label` 同口径），调用方不保留指针；空串等价于清空该段文本。进度条
的**当前值是纯值**，由调用方 `set_value` 推进，**越界自动夹紧**到
`[vmin, vmax]`——所以本层**不读时钟、不捕获鼠标**，三平台行为逐字节一致。

**值域收窄会同步夹紧当前值**：把值域从 `[0, 100]` 收窄到 `[0, 20]` 时，若当前
值是 30，会被**当场夹紧到 20**——这样「值域」与「值」永远自洽，不会出现「值超出
当前值域」的非法中间态。非法值域（`vmax < vmin`）与过小高度（`< 8`）一律
Fail-Fast 返回 -1。

**贴底边绘制**：`statusbar_draw(p, sb, win_w)` 把状态栏画在画布**底边**
（`y = 画布高 - 高度`），左右文本分别贴左 / 右内边距，进度条槽位贴右。调用方
通常先把裁剪区设到窗口底部那一行再调它，这样状态栏天然不与控件树重叠。

**20 个 `extern fn`**（全部纯内存、可在无 DISPLAY 环境单元测试）：

| 函数 | 作用 |
| --- | --- |
| `dial_gui_statusbar_new` / `dial_gui_statusbar_free` | 新建 / 释放纯内存状态栏（默认高度 22、进度值域 0..100、无进度条槽位；`free` 连带释放两段文本副本） |
| `dial_gui_statusbar_set_left(sb, text)` / `dial_gui_statusbar_set_right(sb, text)` | 设左 / 右文本（**C 侧副本**；空串等价于清空该段） |
| `dial_gui_statusbar_left(sb)` / `dial_gui_statusbar_right(sb)` | 读回左 / 右文本（**借用指针**，空串表示无文本） |
| `dial_gui_statusbar_set_height(sb, h)` / `dial_gui_statusbar_height(sb)` | 设 / 读高度（像素，须 >= 8；Fail-Fast 返回 -1） |
| `dial_gui_statusbar_set_sep(sb, on)` / `dial_gui_statusbar_sep(sb)` | 开关左右文本间的竖分隔线 |
| `dial_gui_statusbar_enable_progress(sb, on)` / `dial_gui_statusbar_has_progress(sb)` | 开关进度条槽位 |
| `dial_gui_statusbar_set_range(sb, vmin, vmax)` / `dial_gui_statusbar_value_min(sb)` / `dial_gui_statusbar_value_max(sb)` | 设 / 读进度值域（须 `vmax >= vmin`；Fail-Fast 返回 -1）；**收窄值域同步夹紧当前值** |
| `dial_gui_statusbar_set_value(sb, v)` / `dial_gui_statusbar_value(sb)` | 设 / 读进度值（**越界自动夹紧**到 `[vmin, vmax]`） |
| `dial_gui_statusbar_set_tag(sb, tag)` / `dial_gui_statusbar_tag(sb)` | 用户 tag（**64 位全宽**，可 round-trip，与 `GuiScroll.tag` 同宽） |
| `dial_gui_statusbar_draw(p, sb, win_w)` | 绘制状态栏（底板 + 顶边细线 + 左文本 + 右文本 + 可选分隔线 + 可选进度条，全部主题色），贴画布底边 |

> **返回类型约定（防坑）**：本包所有声明为 `extern fn ...: int` 的函数，C 侧实现
> **必须返回 `int64_t`**——Dial 的 `int` 是 64 位，若 C 用 32 位 `int` 返回 -1，
> 会被零扩展成 `4294967295` 而非 `-1`（见 §33.2）。第 9 / 10 / 11 / 12 层全部
> `: int` 函数因此一律 `int64_t` 返回。

**OO 封装**：第 8 层 `gui_oo.dial` 的 `impl GuiStatusBar` 用 20 个方法包住上述
20 个 `extern fn`（`GuiApp` 上挂 `statusbar_new` 工厂）。AD 组契约审计因此由
202 / 203 同步为 **222 / 223**。

**回归（AA 组，第 30 组）**：`tests/pkg/use_gui_statusbar.dial` 在无 DISPLAY 环境跑
纯内存对象 + 离屏画布全流程——默认高度 / 值域、左右文本读回、**空串清空该段而
另一段保留**、过小高度 Fail-Fast、分隔线与进度槽位开关、进度值**上下越界夹紧**、
值域收窄**同步夹紧当前值**、非法值域 Fail-Fast 且值域不变、tag 64 位全宽
round-trip、离屏绘制 CRC32 像素级稳定（重绘幂等 + 进度槽位 / 进度值 / 右文本
各自改变像素），48 条断言全绿、窗口路径明确标注 `ok`。

### 38.19 通知气泡层（第 13 层）

> 第 13 层给「操作结果反馈」提供**通知气泡**：一组消息按 FIFO 排队，每条自带
> 生命周期，到期自动消隐。它走与第 7 / 9 / 10 / 11 / 12 层**完全相同的宪法
> 边界**——**纯 C、零平台符号、绝不 C → Dial 反向回调**——23 个 `extern fn`
> （前缀 `dial_gui_toast_*`），挂在 `GuiToast` 不透明句柄上，与旧 154 个签名同
> 契约、`abi = 6` / `version = 1.10.0` 不变、旧签名一字不动。

**生命周期由调用方推进，本层不读时钟**：与第 11 层悬浮提示同理——时钟是唯一
会把三平台行为拉开的来源。所以本层**完全不读时钟、不调用任何平台 API**，由
应用侧以毫秒数推进 `tick(dt_ms)`：每条消息的 `elapsed_ms` 累加，累计达到
`life_ms` 就被自动消隐。`life_ms <= 0` 表示该条**永不过期**（适合「等待用户确认」
这类常驻提示）。

**队列语义**：FIFO，`0` 号是最旧的一条。队列容量 16，**满时 `push` 挤掉最旧的
一条**——这样新消息总能进来，且不会因为旧消息堆积而丢新反馈。到期消隐也走 FIFO
（最旧的先到期）。

**三种气泡种类**：`push` 的 `kind` 参数取 `0` = 信息 / `1` = 警告 / `2` = 错误，
分别取不同主题底色（信息=面板底 / 警告=按钮底 / 错误=选区底），越界 Fail-Fast
返回 -1。消息文本是 C 侧 `strdup` 自有副本，调用方不保留指针。

**23 个 `extern fn`**（全部纯内存、可在无 DISPLAY 环境单元测试）：

| 函数 | 作用 |
| --- | --- |
| `dial_gui_toast_new` / `dial_gui_toast_free` | 新建 / 释放通知队列（容量 16、间距 6；`free` 连带释放全部消息文本副本） |
| `dial_gui_toast_push(t, text, life_ms, kind)` | 入队一条消息（**C 侧副本**；`life_ms <= 0` 永不过期；`kind` 越界 Fail-Fast；队列满时挤掉最旧一条） |
| `dial_gui_toast_count(t)` / `dial_gui_toast_clear(t)` | 读当前条数 / 清空队列（释放全部消息文本） |
| `dial_gui_toast_tick(t, dt_ms)` | 推进所有消息存活时间并消隐到期条目，返回消隐后的剩余条数 |
| `dial_gui_toast_item_text(t, idx)` | 读第 idx 条（0 = 最旧）的文本（**借用指针**；越界返回空串） |
| `dial_gui_toast_item_kind(t, idx)` / `dial_gui_toast_item_life(t, idx)` / `dial_gui_toast_item_elapsed(t, idx)` | 读单条的种类 / 总生命周期 / 已存活毫秒（越界返回 -1） |
| `dial_gui_toast_item_set_tag(t, idx, tag)` / `dial_gui_toast_item_tag(t, idx)` | 单条 tag（**64 位全宽**，可 round-trip；越界返回 -1） |
| `dial_gui_toast_set_pos(t, x, y)` / `dial_gui_toast_pos_x(t)` / `dial_gui_toast_pos_y(t)` | 设 / 读气泡堆叠锚点（窗口内坐标，左上角） |
| `dial_gui_toast_set_size(t, w, h)` / `dial_gui_toast_size_w(t)` / `dial_gui_toast_size_h(t)` | 显式设 / 读单条气泡尺寸（须为正；Fail-Fast 返回 -1）；设置后不再按文本自适应 |
| `dial_gui_toast_set_spacing(t, sp)` / `dial_gui_toast_spacing(t)` | 设 / 读相邻气泡间距（须非负；Fail-Fast 返回 -1），默认 6 |
| `dial_gui_toast_set_tag(t, tag)` / `dial_gui_toast_tag(t)` | 队列整体 tag（**64 位全宽**，可 round-trip） |
| `dial_gui_toast_draw(p, t)` | 绘制整个队列（自上而下堆叠，底色按种类取主题色）；队列为空不绘制（返回 0） |

> **返回类型约定（防坑）**：本包所有声明为 `extern fn ...: int` 的函数，C 侧实现
> **必须返回 `int64_t`**——Dial 的 `int` 是 64 位，若 C 用 32 位 `int` 返回 -1，
> 会被零扩展成 `4294967295` 而非 `-1`（见 §33.2）。第 9 ~ 13 层全部 `: int`
> 函数因此一律 `int64_t` 返回。

**OO 封装**：第 8 层 `gui_oo.dial` 的 `impl GuiToast` 用 23 个方法包住上述 23 个
`extern fn`（`GuiApp` 上挂 `toast_new` 工厂）。AD 组契约审计因此由 222 / 223
同步为 **245 / 246**（第 14 层再同步为 260 / 261、第 15 层再同步为
278 / 279，见 §38.20 / §38.21）。

**回归（AA 组，第 30 组）**：`tests/pkg/use_gui_toast.dial` 在无 DISPLAY 环境跑
纯内存队列 + 离屏画布全流程——FIFO 顺序、三种 kind、**非法 kind Fail-Fast 且
队列不变**、越界读返回 -1 / 空串、单条 tag 64 位全宽 round-trip、**计时未到期
不消隐 / 恰好到期即消隐 / FIFO 顺序左移**、**`life_ms <= 0` 永不过期**、
清空队列、**队列满（16 条）时挤掉最旧一条且新消息在队尾**、位置 / 尺寸 / 间距
设置、非法尺寸与负间距 Fail-Fast、队列整体 tag round-trip、离屏绘制 CRC32
像素级稳定（重绘幂等 + 第二条消息 / 换 kind 各自改变像素 + 到期消隐后像素不变），
57 条断言全绿、窗口路径明确标注 `ok`。

---

### 38.20 窗口级覆盖层自动集成（第 14 层）

> 第 14 层把第 9 ~ 13 层的 5 类覆盖层对象**挂到窗口**：`GuiWindowImpl` 新增 5 个
> **借用槽位**（`scroll` / `context` / `tooltip` / `statusbar` / `toast`），配套
> 5 × 3 = **15 个 `extern fn`**（`set` / 读回 / `clear`）。挂好之后 `win.draw()`
> 按固定 z-order 自动叠加，应用不再需要手工逐个调用各层 `*_draw`。它与第 9 ~ 13 层
> 走**完全相同的宪法边界**——**纯 C、零平台符号、零时钟、绝不 C → Dial 反向回调**——
> 与旧 246 个签名同契约、`abi = 6` / `version = 1.10.0` 不变、旧签名一字不动。

**固定 z-order（先画的在下、后画的在上）**：`gui_window_draw()` 原有顺序
`clear → 控件树 → 顶栏菜单 → 弹出菜单 → 模态对话框` 保持不变，末尾依次追加：

| 顺序 | 覆盖层 | 绘制入口 |
| --- | --- | --- |
| 1 | 第 9 层滚动视图 | `dial_gui_scroll_draw_bar(p, s, 0/1)`（横 / 纵两条滚动条） |
| 2 | 第 10 层右键上下文菜单 | `dial_gui_context_draw(p, c)` |
| 3 | 第 11 层悬浮提示 | `dial_gui_tooltip_draw(p, t)` |
| 4 | 第 12 层状态栏 | `dial_gui_statusbar_draw(p, sb, win->w)` |
| 5 | 第 13 层通知气泡 | `dial_gui_toast_draw(p, t)` |

槽位为 `NULL` 时整段跳过，**空槽位对像素零影响**：AA 组用离屏 `painter_crc32()`
证明「空槽位两次 `draw` CRC 稳定 → 挂载后 CRC 改变且重绘幂等 → 清空后 CRC 回到基线」，
既有各层 CRC 基准逐字节不破。

**借用语义（所有权边界）**：窗口**只持指针，不拥有、不释放**这 5 类对象。
`dial_gui_window_close()` / `dial_gui_app_free()` 不再释放 `win->scroll`（原第 9 层
曾由窗口拥有，本层统一改为借用），`clear_*` 只把槽位置 `NULL`，对象生死完全归应用。
用例在 `app_free` 之后仍能对 5 个对象逐个 `*_free` 成功，这是借用语义的硬证据。

**15 个 `extern fn`**（按 5 类覆盖层同构，前缀 `dial_gui_window_*`）：

| 函数 | 作用 |
| --- | --- |
| `dial_gui_window_set_scroll(win, s)` / `dial_gui_window_scroll(win)` / `dial_gui_window_clear_scroll(win)` | 挂载 / 读回 / 卸载滚动视图覆盖层 |
| `dial_gui_window_set_context(win, c)` / `dial_gui_window_context(win)` / `dial_gui_window_clear_context(win)` | 挂载 / 读回 / 卸载右键上下文菜单覆盖层 |
| `dial_gui_window_set_tooltip(win, t)` / `dial_gui_window_tooltip(win)` / `dial_gui_window_clear_tooltip(win)` | 挂载 / 读回 / 卸载悬浮提示覆盖层 |
| `dial_gui_window_set_statusbar(win, sb)` / `dial_gui_window_statusbar(win)` / `dial_gui_window_clear_statusbar(win)` | 挂载 / 读回 / 卸载状态栏覆盖层 |
| `dial_gui_window_set_toast(win, t)` / `dial_gui_window_toast(win)` / `dial_gui_window_clear_toast(win)` | 挂载 / 读回 / 卸载通知气泡覆盖层 |

`set` / `clear` 返回 `int`（C 侧一律 `int64_t`，见 §33.2）：空窗口或空对象一律
**Fail-Fast 返回 -1** 并写错误槽；读回在空窗口 / 空槽位时返回空句柄（NULL）。
C 侧用同一个 `DEFINE_WINDOW_OVERLAY(lname, Type, field)` 宏实例化 5 层，避免 5 份
重复实现漂移；纯值推进、不读时钟、不调用任何平台 API，X11 / Win32 三平台行为一致。
本层**不新增 `.c` 文件**（只改 `dial_gui.h` / `dial_gui_core.c` / `dial_gui_menu.c`），
三平台 `.a` 重建后仍是零链接期 GUI 平台符号。

**OO 封装**：第 8 层 `gui_oo.dial` 追加第二个 `impl GuiWindow` 块，用 15 个方法
（`win.set_scroll(s)` / `win.scroll()` / `win.clear_scroll()` …）包住上述 15 个
`extern fn`，封装层仍**零 `extern fn` 声明**。AD 组契约审计因此由 **245 / 246**
同步为 **260 / 261**（第 15 层再同步为 **278 / 279**，见 §38.21）。

**回归（AA 组 5m / 5n，第 30 组）**：`tests/pkg/use_gui_overlays.dial`——
无 DISPLAY 路径验证 15 个新 API 对空窗口全部 Fail-Fast、5 类对象纯内存可建并
独立 `free`（应用未创建如实标注 `skipped`）；Xvfb 实窗路径验证空槽位 CRC 稳定、
5 层逐个 `set` / 读回、挂载后 CRC 改变且重绘幂等、逐个 `clear` 读回空句柄、
清空后 CRC 回到基线，以及 `app_free` 后 5 个对象仍可 `free`。两条路径 `bad = 0`。

---

### 38.21 快捷键 / 命令系统（第 15 层）

> 第 15 层把「按键组合 `(key, mods)` → 命令 id」的映射与命令启用态收进一张
> **纯内存表** `GuiHotkey`。应用从 `dial_gui_event_key` / `dial_gui_event_mods`
> 主动送值，`dispatch` 命中且命令启用时返回命令 id，执行什么完全归应用；
> C 侧绝不保存函数指针、**绝不 C → Dial 反向回调**。纯值、零时钟、不查系统
> 键盘布局、零平台符号、零 DISPLAY 依赖，三平台行为逐字节一致。

**模型**：编译期定长——最多 64 条绑定，命令 id 取 `[0, 256)`，无动态扩容，
内存上界确定。命令默认全部启用；`clear` 清空绑定并把启用态复位到全启用，
用户 `tag` 保留。绑定按插入顺序存放，`unbind` 后前移压实（顺序稳定、可逐条读回）。

| API | 语义 |
| --- | --- |
| `dial_gui_hotkey_new()` / `free(h)` | 建 / 释放表（命令默认全启用） |
| `bind(h, key, mods, cmd)` | 新增绑定；**同一 `(key, mods)` 已存在即 Fail-Fast 返回 -1**（冲突不静默覆盖），表满亦 -1 |
| `rebind(h, key, mods, cmd)` | 覆盖式：已存在改命令 id、不存在则新增（表满仍 -1） |
| `unbind(h, key, mods)` / `clear(h)` | 删除一条（未找到 -1）/ 清空全部并复位启用态 |
| `lookup(h, key, mods)` | 返回绑定的命令 id，**不看启用态**；未命中 -1 |
| `dispatch(h, key, mods)` | **窗口事件主入口**：命中且命令启用才返回 id，其余一律 -1 |
| `set_enabled(h, cmd, on)` / `is_enabled(h, cmd)` | 命令启用态开关 / 读回（越界 id 返回 -1） |
| `count(h)` / `capacity(h)` | 当前绑定数 / 表容量 64 |
| `set_tag(h, tag)` / `tag(h)` | 用户 tag（64 位 round-trip） |
| `key_at` / `mods_at` / `cmd_at(h, idx)` | 按插入顺序逐条读回（越界 -1） |
| `draw(p, h, x, y)` | 把前 8 条渲染成 ASCII 列（`Ctrl+Shift+S -> 7`）；先擦固定 200×8 行面板再绘制，重绘幂等；**空表不绘制、零像素影响** |

`key` 须非负；`mods` 只能是 `1 SHIFT / 2 CTRL / 4 ALT` 的按位或；`cmd` 越界、
负键码、非法掩码、冲突绑定、容量溢出一律 **Fail-Fast 返回 -1 并写错误槽**。
所有 `: int` 的 C 实现一律返回 `int64_t`（见 §33.2，`-1` 不能被零扩展成
`4294967295`）。

**边界（不许当已支持）**：不做系统级全局热键、不读键盘布局、不自动执行命令、
不接管窗口事件循环，也不挂进 `GuiWindowImpl` 槽位（与第 14 层的 5 类覆盖层
不同，本表由应用自己持有并主动查询）。命令语义、菜单项的启用态联动、
多套键位方案切换都归应用层。

**OO 封装**：第 8 层 `gui_oo.dial` 追加 `impl GuiApp` 的 `hotkey_new` 工厂与
`impl GuiHotkey` 的 17 个方法，共 18 个方法包住上述 18 个 `extern fn`，封装层仍
**零 `extern fn` 声明**。AD 组契约审计因此由 **260 / 261** 同步为 **278 / 279**
（`278 ↔ 278` 精确相等）。

**回归（AA 组 5o，第 30 组）**：`tests/pkg/use_gui_hotkey.dial` 在**零 DISPLAY**
环境跑 166 条断言——修饰键精确匹配（`Ctrl+S` 不等于裸 `S`）、冲突拒绝且原绑定
不变、`rebind` 覆盖、`unbind` 压实顺序、容量 64 上界（第 65 条 / 表满新增被拒、
表满 `rebind` 已存在仍成功）、命令启用态 `lookup` 不看 / `dispatch` 看、
`clear` 复位启用态但保留 tag、空表零像素、绑定后 CRC 改变且重绘幂等、
命令 id 变化改像素、启用态不影响渲染、`unbind` 后像素变化。全部 `bad = 0`。

**宪法**：只追加 18 个 `extern fn`，旧 261 个签名一字不动；`abi = 6` /
`version = 1.10.0` 不变；新增 `dial_gui_hotkey.c` 为纯 C，三平台 `.a` 重建后
仍是零链接期 GUI 平台符号、零反向回调；未打 tag、未发 Release。

### 38.22 网格布局 + 组合框 / 列表框 + 右键·滚轮交互（第 16 层）

> 第 16 层把「**容器级网格布局**」「**两个成品选择控件 ComboBox / ListBox**」
> 「**右键按下态 + 滚轮步进**」三件基础设施收进 gui 包，与既有七层 + 第 9~15 层
> 同一套契约纪律：只追加 `extern fn`、旧签名一字不动、纯 C、零链接期 GUI 平台符号、
> 绝不 C → Dial 反向回调、`abi = 6` / `version = 1.10.0` 不变。本轮共追加 **23 个
> `extern fn`**（2 GRID + 10 ComboBox + 10 ListBox + 1 右键态），对外契约由
> **279 → 302**，第 8 层 OO 封装 `gui_oo.dial` 同步挂 **23 个方法**（278 → 301 个
> 方法 + 入口 `dial_gui_app_new`，精确相等，仍零 `extern fn` 声明）。AD 组契约审计
> 因此由 **278 / 278 / 279** 同步为 **301 / 301 / 302**。

#### 38.22.1 网格布局 GRID（`set_layout` 接受 mode 3）

容器用 `dial_gui_widget_set_grid(w, cols, rows)` 声明列 / 行数，子节点用
`dial_gui_widget_set_grid_cell(w, col, row, cspan, rspan)` 声明自己占哪格。
`layout_apply` 按弹性均分余量算每个单元格矩形，**纯几何、零 DISPLAY 依赖、headless
可测**。

| API | 语义 |
| --- | --- |
| `set_grid(w, cols, rows)` | 声明 `cols × rows`；**越界 Fail-Fast 拒绝**：`< 1` 或 `> 256` 返回 -1（明确拒绝，不静默截断为 1×1） |
| `set_grid_cell(w, col, row, cspan, rspan)` | 子节点占格；`col/row` 须 `0..255`、`cspan/rspan` 须 `1..256`，越界一律 -1 |
| `set_layout(w, 3)` | 接受 `3 = GRID`；非法 mode（如 99）返回 -1 |

几何规则（`dial_gui_widget.c::layout_node`）：单元格宽 `cell_w = (cw - gap*(cols-1)) / cols`
（向下取整，`< 0` 夹 0）；子节点 `x = gc*(cell_w+gap)`、`w = cspan*cell_w + (cspan-1)*gap`；
超界列 / 行夹到 `[0, cols-1]` / `[0, rows-1]`、跨度夹到不越界。例如 200×200、2×2、`gap=0`
每格 100×100；`gap=10` 每格 95×95、`x` 偏移 105；`cspan=2` 占满整行宽 200。

#### 38.22.2 组合框 ComboBox（kind = 10）

只读框 + 点击展开下拉，单选取值。`set_kind` 上界由 7 扩到 **11**：`10`（COMBO）/
`11`（LIST）合法，`12` 越界被拒；**`8` / `9` 仍被显式拒绝**——它们是第 7 层信号
kind 标记 `MENU` / `DIALOG`，不是可绘制的真实控件。扩上界时若顺手放行 8 / 9，控件会被
切成「未知种类」而落进 `draw` 的 BOX 兜底，**静默破坏配色 / RELEASE 激活**（第 16 层
曾踩此坑，现由 `use_gui_look` / `use_gui_input`（9）/ `use_gui_theme`（8）/
`use_gui_combo`（8+9 且选中态不变）四处断言锁死）。
项存 `items`（**容量 64 固定，无动态扩容**）；`label` 是 Dial `string`，C 侧
`dial_string_length` / `dial_string_cstr` 取长度与指针、`malloc`+`memcpy` 造自有副本
（与 `dial_gui_menu_add_item` 同口径）；`item_label` 返回**借用 `rawptr<byte>`**，Dial 侧
必须 `string_from_cstr` 复制成 ARC string，不得长持。

| API | 语义 |
| --- | --- |
| `combo_add_item(w, id, label)` / `combo_clear(w)` | 追加项（id 由应用自定）/ 清空：计数归零、**选中与展开态一并复位** |
| `combo_count(w)` / `combo_item_id(w, idx)` / `combo_item_label(w, idx)` | 项数 / 第 idx 项 id（越界 -1）/ 借用标签指针（越界 NULL） |
| `combo_select(w, idx)` | 单选第 idx 项并**收起下拉**；越界 -1 且选中不变 |
| `combo_selected(w)` | 当前选中下标，默认 -1 |
| `combo_open(w)` / `combo_close(w)` / `combo_is_open(w)` | 手动展开 / 收起 / 读展开态 |

**交互**：点击框体（RELEASE）切换展开；展开后下拉项 `row = (ry - h) / GUI_MENU_ROW_H`
命中即选中 + 收起 + 发 `GUI_SIG_SELECTED`（信号 `a = item id`）。选定语义归应用，C 侧
零反向回调。

#### 38.22.3 列表框 ListBox（kind = 11）

常显可选项，单击单选或 `select_toggle` 多选。`set_kind` 同取上界 11。项存储与 ComboBox
**共用 `GuiWidgetImpl.items`**（容量 64 固定）。

| API | 语义 |
| --- | --- |
| `list_add_item(w, id, label)` / `list_clear(w)` | 追加项 / 清空（选中 64 位位图一并归零） |
| `list_count(w)` / `list_item_id(w, idx)` / `list_item_label(w, idx)` | 项数 / id（越界 -1）/ 借用标签指针（越界 NULL） |
| `list_select(w, idx)` | 单选：覆盖位图为仅 idx |
| `list_select_toggle(w, idx)` | 多选 toggle：翻转第 idx 位（64 位位图） |
| `list_is_selected(w, idx)` | 第 idx 是否选中（1 / 0） |
| `list_selected_count(w)` / `list_selected_first(w)` | 选中个数 / 最小选中下标（无选中 -1） |

**交互**：点击行 `row = (y - abs_y) / GUI_MENU_ROW_H` 选中（单击单选、覆盖位图）并发
`GUI_SIG_SELECTED`（`a = item id`）。

#### 38.22.4 右键按下态 + 滚轮步进（事件推进增强）

在 `dial_gui_look.c::gui_widget_handle` 的 PRESS 段新增两条分支，与既有四态（hover /
pressed / checked / disabled）平行、互不串扰：

- **右键 `button == 3`**：置 `right_pressed = 1`（与 left `pressed` 平行）；
  RELEASE 段遇 `button != 0/1` 直接返回、不激活，并一道清除 `right_pressed`——
  右键纯值态，**不改 left pressed / hovered 语义**。
- **滚轮 `button == 4 / 5`**：命中 `SLIDER` 时按 `step = span / 100`（最小 1）增减
  `value` 并 clamp 到 `[min, max]`；未在 `SLIDER` 上则零影响（不改值、返回 0）。

`dial_gui_widget_right_pressed(w)` 读回该纯值态，`dial_gui_widget_pressed(w)` 仍只读
left pressed，二者独立。

#### 38.22.5 边界（不许当已支持）

不做自动网格换行（仅显式 `set_grid_cell`）、不做 ComboBox 可编辑输入、不做 ListBox
虚拟滚动（容量固定 64）、不接管系统级右键菜单（第 10 层上下文菜单是另一条独立路径）、
不读系统滚动加速（step 由 `span/100` 纯几何决定、零时钟依赖）。列 / 行 / 跨度越界一律
Fail-Fast 返回 -1 并写错误槽，绝不静默截断。

#### 38.22.6 OO 封装与回归

第 8 层 `gui_oo.dial` 在 `impl GuiWidget` 末尾追加 **23 个方法**（去 `dial_gui_`/
`dial_gui_widget_` 前缀、接收者变 `self`）：`set_grid` / `set_grid_cell`、
`combo_add_item` / `…clear` / `…count` / `…item_id` / `…item_label` / `…select` /
`…selected` / `…open` / `…close` / `…is_open`、`list_add_item` / `…clear` / `…count` /
`…item_id` / `…item_label` / `…select` / `…select_toggle` / `…is_selected` /
`…selected_count` / `…selected_first`、`right_pressed`。封装层仍**零 `extern fn`
声明**。

**回归（AA 组 5p ~ 5s，纯内存对象 + 离屏 CRC 锁定，零 DISPLAY 依赖）**：
`tests/pkg/use_gui_grid.dial` / `use_gui_combo.dial` / `use_gui_list.dial` /
`use_gui_wheel.dial` 各含独立 offscreen 画布，断言经 `grep ' = bad'` 计失败、
`grep -qF` 钉关键输出、CRC 用 `expect_str(str(crc), "N", "x_crc_pinned")` 钉死像素级
基准——

- **GRID**：`set_grid` / `set_grid_cell` 越界 Fail-Fast、`set_layout(3)` 接受 /
  `set_layout(99)` 拒绝、2×2 每格 100×100 / gap10 每格 95×95 / `cspan=2` 占满整行、
  离屏 CRC 钉值 `4049314674`，全绿。
- **ComboBox**：`kind=10` 合法 / 12 拒、项增删 / 容量上界、单选 `select` / `selected`、
  `open` / `close` / `is_open`、clear 复位、点击框体切展开、下拉项命中选中发
  `SELECTED`（订阅后验 `signal.a == item id`）、离屏 CRC 钉值 `3235232961`，全绿。
- **ListBox**：`kind=11` 合法、单选 / 多选 `toggle` / `is_selected` / `selected_count` /
  `selected_first`、点击行选中发 `SELECTED`、离屏 CRC 钉值 `3941106424`，全绿。
- **右键·滚轮**：SLIDER 滚轮上(4) / 下(5)滚动 `step=1`、clamp 不越界、右键(3)置
  `right_pressed` 且不改 left pressed、RELEASE(3) 清除、滚轮落非 SLIDER 零影响、
  离屏 CRC 钉值 `2509410687`，全绿。

**宪法**：只追加 23 个 `extern fn`，旧 279 个签名一字不动；`abi = 6` /
`version = 1.10.0` 不变；**不新增 `.c` 文件**，`build.sh` 源清单不变，三平台 `.a` 重建后
仍是零链接期 GUI 平台符号、零反向回调；未打 tag、未发 Release。

### 38.23 TextBox 撤销 / 重做 + Slider 拖动 + 每控件主题覆写 + Grid 自动换行（第 17 层）

第 17 层解锁 §38.11 / §38.22.5 里明确留着的四条边界，全部**只追加 16 个 `extern fn`**
（`gui.diali` **302 → 318**，`gui_oo.dial` **301 → 317 个方法**，精确相等），
旧 302 个签名一字不动；`abi = 6` / `version = 1.10.0` 不变；**不新增 `.c` 文件**
（改动只落在既有 `dial_gui.h` / `core.c` / `input.c` / `look.c` / `widget.c`），
`build.sh` 源清单不变；纯 C、零链接期 GUI 平台符号、绝不 C → Dial 反向回调。

四组能力有一条共同的**防漂移铁律**：未启用时逐像素等价于旧路径。因此第 1~16 层
全部既有 CRC 钉值（`look_crc_normal = 3884455222` 起）一字未改，七个既有 demo
在新内核下重跑仍是 `0 bad`。

#### 38.23.1 TextBox 撤销 / 重做栈（8 个 `extern fn`）

快照式历史：每次键盘编辑（可打印字符 / `Backspace` / `Delete`）**成功改掉文本之前**，
把「文本 C 侧副本 + 光标 + 选区锚点」推进 `undo` 栈，并清空 `redo` 分支（新编辑使
重做历史失效）。两栈容量 `GUI_UNDO_CAP = 32` 固定、无动态扩容，栈满丢最老一条；
文本副本逐条由 `dispose_subtree` / `undo_clear` 释放。

| API | 语义 |
| --- | --- |
| `dial_gui_widget_undo` / `redo` | `1` 成功 / `0` 栈空（**不是错误，不写错误槽**）/ `-1` 无效句柄 |
| `can_undo` / `can_redo` | `1` / `0`，纯值读回 |
| `undo_depth` / `redo_depth` | 当前栈深（0..32） |
| `undo_clear` | 清两栈并释放副本 |
| `undo_capacity` | 常量 `32` |

**显式 `widget_set_text` 是程序化赋值**：建立新基线并清两栈，绝不混进用户编辑历史
（这是 §38.11「撤销重做栈」从「未实现」移除后仍要写清的边界）。撤销 / 重做恢复
文本后照常发一次 `TEXT_CHANGED`（未订阅零开销）；光标随快照一并恢复。

#### 38.23.2 Slider 拖动（1 个 `extern fn`）

`widget_handle` 在 `MOUSE_PRESS` 命中 `SLIDER` 时置纯值 `drag_active`（**PRESS 不改
`value`，第 5 层「RELEASE 按局部 x 设值」语义一字不动**）；`MOUSE_MOVE` 沿树累加
相对几何拿到控件绝对 x（与 `hit_node` 同口径，不依赖上一轮布局残留），按拖动点
设值并 clamp 进 `[vmin, vmax]`，鼠标拖出控件矩形仍生效、绝不跳变；`MOUSE_RELEASE`
清态。`dial_gui_widget_dragging` 读回该纯值，供 Dial 侧区分拖动与单击。
非 `SLIDER` 的 PRESS 不进入拖动态，后续 MOVE 对 value 零影响。

#### 38.23.3 每控件主题覆写（5 个 `extern fn`）

进程级全局表仍是**默认真相源**；`ov_mask` 置位的角色由本控件私有色覆盖，未覆写
角色逐像素回落全局表。绘制侧只把 `C(role)` 简写从「直接查全局表」改成
「先查控件覆写、再回落全局表」——**默认态与旧路径逐像素一致**，这是既有 CRC
不漂移的关键。

| API | 语义 |
| --- | --- |
| `set_color(w, role, color)` | 覆写角色；`role` 越界 / `color` 高 32 位非 0 一律 Fail-Fast 返回 `-1` |
| `color(w, role)` | **生效色**（覆写优先，否则全局表） |
| `has_color(w, role)` | 该角色是否被本控件覆写 |
| `clear_color` / `clear_colors` | 清单角色 / 清全部，CRC 逐像素回到历史基准 |

`color = 0x00000000` 也是**合法覆写值**：是否覆写由独立位图 `ov_mask` 判定，
绝不与「未覆写」哨兵混淆。覆写不传给子控件（不继承），也不等于「可切换深色主题包」。

#### 38.23.4 Grid 自动换行（2 个 `extern fn`）

容器级开关 `set_grid_autoflow(w, on)` / `grid_autoflow(w)`：打开后子节点按
**加入顺序**自动依次占格（第 `i` 个可见子节点 → `col = i % cols`、`row = i / cols`），
忽略显式 `set_grid_cell` 的 `col/row`；`cspan` / `rspan` 仍由子节点显式声明。
溢出 `cols×rows` 的部分夹到最后一行（与既有 clamp 风格一致），重叠由应用自己负责
（确定性）。关闭时与第 16 层 `layout_node` 逐像素等价。

#### 38.23.5 边界（不许当已支持）

不做编辑合并（一次按键一条）、不做跨控件全局历史、不做历史持久化；不做拖动阈值 /
吸附 / 系统时钟读取；不做主题包切换与覆写继承；不做自动换行重排（不因 `cspan`
变化回溯）、不做双向 / 稀疏网格。列 / 行 / 角色 / 颜色越界一律 Fail-Fast 返回 `-1`
并写错误槽，绝不静默截断。

#### 38.23.6 OO 封装与回归

第 8 层 `gui_oo.dial` 在 `impl GuiWidget` 末尾追加 **16 个方法**（去
`dial_gui_widget_` 前缀、接收者变 `self`）：`undo` / `redo` / `can_undo` /
`can_redo` / `undo_depth` / `redo_depth` / `undo_clear` / `undo_capacity`、
`dragging`、`set_color` / `color` / `has_color` / `clear_color` / `clear_colors`、
`set_grid_autoflow` / `grid_autoflow`。封装层仍**零 `extern fn` 声明**，
AD 组契约审计由 **301 / 301 / 302** 同步为 **317 / 317 / 318**。

**回归（AA 组 5t ~ 5w，纯内存对象 + 离屏 CRC 锁定，零 DISPLAY 依赖）**：
`tests/pkg/use_gui_undo.dial`（53 条）/ `use_gui_drag.dial`（38 条）/
`use_gui_theme_override.dial`（46 条）/ `use_gui_autoflow.dial`（44 条），
各含独立 offscreen 画布，CRC 用 `expect_str(str(crc), "N", "x_crc_pinned")` 钉死
像素级基准——

- **撤销 / 重做**：显式 `set_text` 不进历史、两级 undo / redo 文本与深度同步、
  新编辑使 redo 失效、光标随快照恢复、栈满夹到 32、clear 复位、
  离屏 CRC 钉值 `1521191646`，全绿。
- **Slider 拖动**：PRESS 置态不改 value、MOVE 按绝对 x 设值（中点 50）并 clamp、
  拖出控件仍生效、RELEASE 清态、非 SLIDER 零影响、离屏 CRC 钉值 `734328573`，全绿。
- **每控件主题覆写**：未覆写回落全局表、覆写只改本控件像素、Fail-Fast 六边界、
  `color=0` 合法、`clear` 后 CRC 回到历史基准、离屏 CRC 钉值 `2387101612`，全绿。
- **Grid 自动换行**：按加入顺序排 2×2、显式 col/row 被忽略、cspan 仍生效、
  关闭回到第 16 层语义、溢出夹末行、离屏 CRC 钉值 `3418075373`，全绿。

**宪法**：只追加 16 个 `extern fn`，旧 302 个签名一字不动；`abi = 6` /
`version = 1.10.0` 不变；**不新增 `.c` 文件**，三平台 `.a` 重建后仍是零链接期
GUI 平台符号、零反向回调；未打 tag、未发 Release。

### 38.24 高 DPI 放大 + 虚拟化 lazy load + IME 预编辑 + GPU/原生纯值契约层 + 编译器两 flag（第 18 层）

第 18 层回应社区对标的五个能力缺口：虚拟化视图（Model/View）、真实 GPU 渲染与动画、
输入法与复杂文本排版、平台深度集成（原生文件框 / 系统托盘 / 全局快捷键）、高 DPI 多屏自适应；
同时落地 Qt 对比里点名的两个编译器分发模式 `--tiny` / `--single-file`。本轮**只做纯值契约层**
（能力探测 + Fail-Fast 参数校验 + 确定性降级码 `-2`），真实 GPU 合成、原生弹窗 / 托盘 / 快捷键实机、
IMM32 / IBus / Fcitx 平台采集**有意留下一批**（见 §38.24.5 / §38.24.6）。

契约从 **318 个 `extern fn`** 扩到 **344 个**（追加 26 个），OO 封装从 **317 个方法**
扩到 **343 个**（追加 26 个方法 + 1 个入口 `dial_gui_app_new`，AD 审计口径 **343 / 343 / 344 / 0**）。

#### 38.24.1 编译器 `--tiny`（体积模式）/ `--single-file`（便携模式）

纯工具链层，**不触碰 GUI 宪法**：不必新增 `.c`、不必改 `abi` / `version`、不破任何旧签名。

| flag | 行为 | 实测（hello.dial） |
| --- | --- | --- |
| `--tiny` | C 侧 `-Os -ffunction-sections -fdata-sections`，链接期 `-Wl,--gc-sections -s`；强制走源码编译 stdlib 回退路径（绕开预编译 `-O2` `.a`），构建目录隔离为 `safety-*-tiny` | 产物 **7024 B**，`readelf -S` 无 `.symtab` / `.debug_info` |
| `--single-file` | `-static` 全静态链接；因 `zig cc -static` 对 glibc 目标无效，链接驱动改用系统 `gcc/clang/cc`（`static_link_cc()`），`-static` 下跳过 rpath | `file` 显示 **statically linked**，`ldd` 显示 **not a dynamic executable** |
| `--tiny --single-file` | 上述二者叠加（`-static` 会拉入 libc 静态段，通常更大） | 约 719 KB，仍静态链接、可运行 |

冲突矩阵（目标相反即 Fail-Fast，退出码 1）：`--tiny` 与 `--fast` / `--pgo`（配合 `--fast`）/ `-g`
互斥；`--single-file` 允许与 `--fast` / LTO / `--pgo` / `-g` 正交叠加。AE 组端到端断言上述
九条形态全部成立。LLVM 侧 inkwell 无原生 Oz 档，体积压缩全部压在 C 侧与链接期，不承诺 60 KB、
实测多少写多少。

#### 38.24.2 高 DPI 整数放大（scale_factor）

原框架纯像素级 1.0，无 `scale_factor`。本轮**不挂 App/Window 级**（headless 下
`dial_gui_app_new()` 返回 NULL 测不到），改为挂 `GuiPainter` 的整数放大因子
`scale ∈ [1, 4]`（常量 `GUI_SCALE_MIN/MAX`）：

- `dial_gui_painter_set_scale(p, n)` / `painter_scale(p)`：设 / 取放大因子，`scale < 1`
  或 `> 4` 一律 Fail-Fast 返回 `-1` 并写错误槽；
- `dial_gui_painter_scaled(p, factor)`：用**最近邻**把源画布放大 `factor` 倍，返回新的
  独立 offscreen painter；`factor == 1` 时逐像素拷贝（与源 CRC 相同），不会改源。

`GuiPainterImpl.scale` 由 `gui_painter_new` **显式置 1**（`calloc` 归零不是 1，这是防漂移头号点）；
`scale == 1` 时绘制与历史路径逐像素等价，第 1~17 层 demo CRC 一字不漂移。
AA 组 `use_gui_scale.dial`（38 条）覆盖默认态 / 设值 / 越界 Fail-Fast / `scaled(1)` 等价 /
`scaled(2)`→8×8 / `scaled(3)`→12×12，离屏 CRC 钉值 **2473558418**。

#### 38.24.3 ListBox / ComboBox 虚拟化 lazy load

不做反向回调、不改 `GuiWidgetSignal`、不改 `signal_count()`（仍为 7）。在 widget 上挂纯值
数据槽：`virtual_on`（开关）、`virtual_total`（逻辑总行数，上限 `GUI_VIRTUAL_MAX_TOTAL = 1000000`）、
`virtual_first` / `virtual_count`（可视窗口），C 侧只持有 `<= 64` 个槽（复用 `items[]`）：

- `dial_gui_widget_set_virtual(w, on, total)`：开虚拟模式时清空全量 `item_count`，只收
  `virtual_first/count` 窗口；
- `dial_gui_widget_virtual_window(w, first, count)`：`first < 0` / `count > 64` /
  `first + count > total` 一律 Fail-Fast；
- `dial_gui_widget_virtual_slot(w, slot, text: string, id)`：Dial → C 单向推进可视窗口内
  第 `slot` 个数据（`DialString` 铁律：C 侧 `malloc+memcpy` 造自有副本），零 C → Dial 反向回调。

绘制时 `vis = virtual_on ? virtual_count : item_count`，命中时 `row` 以可视窗口为基准；
`virtual_on == 0` 时 `vis` 恰等于旧 `item_count`，保证旧路径 CRC 零漂移。
AA 组 `use_gui_virtual.dial`（58 条）覆盖 100 万逻辑行 / 窗口推进 / 槽数据 /
越界 Fail-Fast / LIST+COMBO 命中信号 id，离屏 CRC 钉值 **1677088382**。

#### 38.24.4 IME 预编辑纯值事件块

不扩 `GUI_SIG_*` 位值体系（避免改写 `signal_count()==7` 与既有 demo 断言），改为挂在 widget
树根的**独立纯值事件块**（不写 `GuiWidgetSignal`、不改信号类名数）。C 侧字段：`ime_preedit[256]`、
`ime_caret`、`ime_cx/cy/cw/ch`（候选框矩形）、`ime_cand_count`、`ime_seq`、`ime_active`：

- `dial_gui_ime_feed(w, preedit, caret, cx, cy, cw, ch, cand_count)`：校验预编辑串长度 /
  caret / 候选框矩形 / 候选数，推进事件块并自增 `ime_seq`；
- `dial_gui_ime_preedit(w)`（借用 `rawptr<byte>`，Dial 侧 `string_from_cstr` 复制成 ARC string）/
  `dial_gui_ime_caret` / `dial_gui_ime_cand_count` / `dial_gui_ime_rect` /
  `dial_gui_ime_seq`：取事件快照；
- `dial_gui_ime_clear(w)`：清内容但 **`ime_seq` 不重置**（序列号只增，供应用去重）。

`gui.diali` 开头注释由「输入法（IME）未实现」改写为「真实 IMM32/IBus/Fcitx 采集未接；第 18 层已落地
IME 预编辑纯值事件块契约」。AA 组 `use_gui_ime.dial`（46 条）覆盖 feed / 取串 / caret / rect /
seq 自增 / clear / 越界，离屏 CRC 钉值 **1103561703**。

#### 38.24.5 GPU 合成 + 原生平台集成纯值契约层

本轮经授权新增唯一一个 `.c`（`dial_gui_platform.c`，见 §38.24.6），但**只做纯值契约层**：
不 `dlopen` GL/EGL/Vulkan、不 `#include` 平台头、不碰 framebuffer。headless 下能力恒未装载、
动作恒返 `GUI_PLATFORM_DEGRADE (-2)`：

- `dial_gui_gpu_available()` → `0`（未装载）；`dial_gui_gpu_backend()` → `"none"`（借用串）；
  `dial_gui_gpu_caps()` → `GPU_CAP_NONE`；`dial_gui_gpu_try_composite(p)` → 参数与尺寸校验后
  返回 `-2`（真实合成后端留下一批）；
- `dial_gui_tray_set` / `dial_gui_native_file_dialog` / `dial_gui_global_hotkey_register`：
  先 Fail-Fast 校验参数，再返回 `-2`；
- `dial_gui_gpu_degrade_reason()` / `dial_gui_platform_degrade_code()` → 返回 `-2` 与
  降级原因串（借用 `rawptr<byte>`）。

AA 组 `use_gui_platform.dial`（30 条）覆盖 `gpu_available()` / `gpu_backend()` / 降级码 `-2` /
参数 Fail-Fast / 不碰 framebuffer（离屏 CRC 前后不变），离屏 CRC 钉值 **2482968047**。
**明确未实机验证**：真实 GPU 合成、原生弹窗 / 系统托盘 / 全局快捷键 / 原生文件框、IMM32/IBus/Fcitx
输入法采集——本轮只把契约与降级码钉死，真实后端作为后续批次。

#### 38.24.6 宪法破例声明

本轮**仅放宽一条**宪法约束：**允许新增 1 个 `.c` 文件**（`packages/gui/src/dial_gui_platform.c`），
并在 `build.sh` 的 `SRCS` 公共清单追加 `dial_gui_platform.c`（头注释写明「第 18 层为纯值契约层
新增唯一一个 `.c`」）后三平台 `.a` 重建成功（x86_64-linux-gnu 420K / aarch64-linux-musl 376K /
x86_64-pc-windows-gnu 296K），零链接期 GUI/GL 平台符号审计通过。

其余宪法**全守**：只追加 26 个 `extern fn`（318→344），旧 318 个签名一字不动；`abi = 6` /
`version = 1.10.0` 不变；OO 封装层零 `extern fn` 声明（317→343 个方法）；零 C → Dial 反向回调；
三平台 `.a` 仍是零链接期 GUI/GL 平台符号（运行期动态装载符号仅 `dlopen`/`dlsym`/`LoadLibraryA`/`GetProcAddress`）；
末打 tag、未发 Release。AA 组 5x~5z 四个 demo（scale / virtual / ime / platform）+ AE 组编译器两 flag
随包回归全绿，第 1~17 层 21 个既有 demo `bad=0` 防漂移验证通过。
