# Dial 架构规划 —— 《Dial 标准库与包分发最终规范》

> **状态：规范已定稿；v0.92 起核心链路已实现。**
> 本文件是 Dial 生态的**前瞻性架构规范**，用于提前锁定标准库与第三方库的分发形态。
>
> **v0.92 已落地**：`import` 语句、`.dialpkg` 解包（含路径穿越防护与整包校验）、
> `manifest.txt` 解析、按 Target Triple 选库、逐文件 SHA256 强制校验、
> `.diali` 解析接入 Sema 与链接期自动链库——即下方 [§8 实现路线](#8-实现路线尚未开始供后续版本参考)
> 的阶段 1 / 3 / 4。用法见 [README §25.9](README.md#259-用法import-与-dialc-installv092-已实现)。
>
> **v0.96 已补齐**：阶段 2 的 `dialc install <URL>`（网络下载）与阶段 5 的一键安装脚本。
> **仍待实现**：阶段 6 的 Windows 线程后端、阶段 7 的官方标准库 `.dialpkg` 化。
>
> 之所以在实现之前就把规范写死：让「不透明指针 + C ABI」这条底层设计
> （[§14.1](README.md#141-不透明指针契约)、[§15](README.md#15-ffi-契约dial-语法--c-函数)）
> 从第一天起就为包分发服务，而不是等生态长歪了再回头打补丁。
> **规范先于实现，是防止依赖地狱的唯一低成本时机。**

配套阅读：[README §25 包分发与依赖管理](README.md#25-包分发与依赖管理生态规划--草案)（决策摘要）、
[README §14 内存与所有权模型](README.md#14-内存与所有权模型)、[§20 Fail-Fast 铁律](README.md#20-fail-fast-铁律宁可崩溃绝不静默损坏)。

---

## 0. 目标平台（第一约束）

| 目标 | 状态 |
| --- | --- |
| **Windows**（x64 / ARM64） | ✅ 目标平台 |
| **Linux**（x86_64 / ARM64，glibc 与 musl） | ✅ 目标平台 |
| **Termux**（Android ARM64） | ✅ 目标平台 |
| macOS / Darwin / Apple | ❌ **不在目标范围内**，不为其做设计妥协 |

平台矩阵一旦确定，整套规范的分发形态立刻被它决定——**尤其是「一个包包含全部平台机器码」**
这个决策（下节 §2）：既然只有 3 个平台、每个平台 1~2 个变体，那么把它们的机器码全量打进
同一个包里，体积代价（几十 MB）完全可以接受，而用户侧的收益是**一个包走天下**。

### 0.1 平台矩阵与 Target Triple

`.dialpkg` 内的机器码按 **Target Triple** 索引。标准目标集如下：

| Target Triple | 平台 | libc | 机器码文件名 | 备注 |
| --- | --- | --- | --- | --- |
| `x86_64-linux-gnu` | Linux x86_64 | glibc | `lib/linux-x86_64.a` | 主流桌面 / 服务器 ✅ 已支持 |
| `aarch64-unknown-linux-musl` | **Termux**（ARM64） | musl | `lib/termux-aarch64.a` | musl 全静态单文件 ✅ 已支持 |
| `x86_64-pc-windows-gnu` | Windows x64 | msvcrt | `lib/windows-x86_64-mingw.a` | MinGW 工具链 ✅ 已支持 |
| `x86_64-linux-musl` | Linux x86_64 | musl | `lib/linux-x86_64-musl.a` | 静态链接友好（未来变体） |
| `aarch64-linux-gnu` | Linux ARM64 | glibc | `lib/linux-aarch64.a` | ARM 服务器 / 树莓派（未来变体） |
| `aarch64-linux-android` | Android ARM64 | bionic | `lib/termux-aarch64.a` | bionic 变体（未来变体） |
| `x86_64-pc-windows-msvc` | Windows x64 | ucrt | `lib/windows-x86_64.lib` | MSVC 工具链（未来变体） |
| `aarch64-pc-windows-msvc` | Windows ARM64 | ucrt | `lib/windows-arm64.lib` | Windows on ARM（未来变体） |

> **v0.97 已落地三平台原生构建**：Linux x86_64（glibc）、Windows x64（**MinGW / gnu**）、
> Termux aarch64（**musl 全静态**）。下方带 ✅ 的三行即当前实际支持的 triple。
> `host_triple()` 完全由编译期 `cfg!` 宏推导（见 `src/package.rs`）——各平台均**原生构建**，
> 编译时的 target 即运行时的真实平台，无需运行时猜测，也无需交叉编译的 ABI 兜底。

> **Target Triple 是唯一的寻址键。** 不另造「arch」字段——「架构」不足以定位一份机器码：
> 同为 x86_64，Linux/glibc、Linux/musl、Windows/GNU、Windows/MSVC 四者的 ABI 互不兼容。
> 编译器手里本来就有完整的 triple（`rustc --print cfg` / LLVM 的 target triple），
> 用它做键，天然与编译产物同源，不需要任何额外的配置或猜测。

### 0.2 平台差异对 C Runtime 的约束（实现路线备忘）

当前 Dial 的 C Runtime（`stdlib/dial_thread.c`、`stdlib/dial_channel.c`）**直接使用 pthread**。
在上述目标平台中：

| 平台 | pthread 可用性 | 备注 |
| --- | --- | --- |
| Linux（glibc / musl） | ✅ 原生 | glibc ≥ 2.34 已并入 libc |
| Termux（musl / bionic） | ✅ 原生 | musl / bionic 均提供完整 POSIX 线程 |
| **Windows（MinGW + winpthreads）** | ✅ **winpthreads 兼容层** | MinGW 工具链自带 winpthreads，提供与 pthread 兼容的 API，因此 stdlib 的线程代码**无需改写**即可在 Windows 编译运行 |

因此**支持 Windows 的前置工作**（阶段 6）在 MinGW 路线下被大幅简化：由于 winpthreads 已提供
pthread 兼容层，`dial_thread.c` / `dial_channel.c` 中的 `pthread_*` 调用在 Windows 上**原样可用**，
阶段 6 的「Win32 原生线程后端」因此可**延后**——仅在需要脱离 MinGW、改用纯 MSVC 工具链时才必须落地。
对上层（`spawn` / `join` / `channel` / `recv_timeout`）的语义**完全不变**——这正是 §4
「接口契约与机器码解耦」的精神在 Runtime 内部的延伸。

另有一处**待办**：v0.9 的 `recv_timeout` 出于 macOS 兼容考虑，把超时基准钟定为
`CLOCK_REALTIME`（因为 `pthread_condattr_setclock` 在 macOS 上不存在）。
既然目标平台已收敛为 Windows / Linux / Termux，这项顾虑**不再成立**——
`pthread_condattr_setclock(CLOCK_MONOTONIC)` 在 Linux（glibc / musl）与 bionic 上均可用，
可在 Windows 后端落地时一并把基准钟切回单调钟（消除系统时间跳变对超时精度的影响）。
该改动属实现工作，不在本规范范围内。

---

## 1. 统一的二进制包格式（`.dialpkg`）

### 1.1 铁律

| 决策 | 规定 |
| --- | --- |
| **不分发源码** | 标准库与第三方库**统一分发预编译好的机器码**。用户机器无需安装对方的工具链、SDK、编译器 |
| **容器格式** | **拒绝**直接用 `.zip` / `.tar.gz` 裸分发 |
| **专属扩展名** | **`.dialpkg`**（Dial Package） |
| **用户是否手动解压** | **永远不需要** |
| **包内容** | 极简三件套：一个映射字典 `manifest.txt`、一份唯一的接口声明 `xxx.diali`、一个 `lib/` 目录 |

> **为什么不用 `.dpkg`**：它会与 Debian 的 `dpkg` 撞名，造成工具链与人的双重误判。
> `.dialpkg` 全局唯一，双击 / 命令行都不会被当成别的东西。

### 1.2 包内结构

```
sqlite.dialpkg
├── manifest.txt          # 映射字典：Target Triple -> 机器码文件；含元信息与哈希
├── sqlite.diali          # 唯一的接口声明（纯文本，平台无关）
└── lib/                  # 全平台预编译静态库，原样平铺
    ├── linux-x86_64.a
    ├── linux-x86_64-musl.a
    ├── linux-aarch64.a
    ├── termux-aarch64.a
    ├── windows-x86_64.lib
    └── windows-arm64.lib
```

**三件套的职责切分**：

| 部件 | 回答什么问题 | 形态 |
| --- | --- | --- |
| `manifest.txt` | 「有哪些平台？各自的机器码在哪？可信吗？」 | JSON 映射字典 |
| `xxx.diali` | 「这些函数长什么样？参数与返回类型是什么？」 | 纯文本声明 |
| `lib/` | 「给我当前平台的那份机器码。」 | 二进制静态库 |

### 1.3 `xxx.diali` —— 签名即契约（平台无关）

`.diali` 是**纯文本**的接口签名文件，只含声明，**不含任何实现代码**。
`dialc` 与 LSP **只读取这个文件**来完成：类型检查（Sema）、代码补全、悬停提示（hover）。

```dial
// sqlite.diali —— 接口签名文件（纯文本，无实现，平台无关）
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
- **LSP 与编译器共用同一份真相。** 补全 / 悬停与编译期检查读的是同一个文件，
  杜绝「编辑器说能过、编译器说不能过」的经典分裂。文档注释（`///`）就是悬停提示的来源。

> **`.diali` 只有一份，是刻意的。** 它描述的是**接口**，不是某个平台的 ABI 布局。
> 同一个 `.diali` 配 Linux 的 `.a` 或 Windows 的 `.lib`，接口契约完全一致——
> 这就是 §4「接口契约与底层是哪个平台的机器码解耦」的落地形态。

---

## 2. 多平台机器码「揉捏」与字典映射

### 2.1 决策

| 决策 | 规定 |
| --- | --- |
| **打包范围** | `lib/` 目录内**直接存放多个平台**的预编译静态库 |
| **体积** | **不限制包体积**（一般也就几十 MB），所有平台机器码**全量打包** |
| **寻址机制** | 通过 `manifest.txt` 作为**映射字典**，明确声明 **Target Triple → 库文件名**的对应关系 |

### 2.2 为什么全量打包是对的

「一个平台一个包」的方案看起来更省体积，实则把成本转嫁给了用户：

| 方案 | 用户要做什么 | 后果 |
| --- | --- | --- |
| 每个平台一个包 | 交叉编译时**逐个平台**下载 | N 个平台 = N 次下载、N 份缓存、心智负担 |
| **全量揉捏（采用）** | **下载一次** | 包大几十 MB，但换来「一个包走天下」 |

关键洞察：**机器码是编译产物，不是源码**。它不会被人阅读、不会被手工编辑、
也不会像源码那样产生「版本分叉」的困扰；而几十 MB 的下载成本在有 CDN 的前提下可以忽略。
既然如此，把「选择哪个平台的机器码」这件事从**用户侧**移到**编译器侧**——
用户在意的只是「我写的代码能编译能跑」，而不是「我装的是哪个平台的包」。

### 2.3 `manifest.txt` —— 映射字典

采用 **JSON**（而非 ini）：映射字典的本质是「键 → 值」的嵌套结构，JSON 表达力足够，
且几乎所有语言的 JSON 解析都是三行代码，不引入新依赖。

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
    "sqlite.diali":              "3a7e04...（省略）...d8",
    "lib/linux-x86_64.a":        "9f2b7c...（省略）...c1",
    "lib/linux-x86_64-musl.a":   "1b83ee...（省略）...02",
    "lib/linux-aarch64.a":       "5c1d02...（省略）...77",
    "lib/termux-aarch64.a":      "a20f91...（省略）...19",
    "lib/windows-x86_64.lib":    "7d3ab8...（省略）...45",
    "lib/windows-arm64.lib":     "e04c62...（省略）...30"
  }
}
```

| 字段 | 含义 |
| --- | --- |
| `name` | 包名（同时决定缓存子目录名） |
| `version` | 包作者的版本号（**仅用于展示与人工核对，编译器不做版本求解**，见 §6） |
| `abi` | **ABI 版本号**；这才是编译器唯一关心的兼容性判据 |
| `iface` | 唯一的接口声明文件名 |
| `targets` | **映射字典**：Target Triple → 该平台的机器码文件 |
| `sha256` | **每个文件各自**的哈希：`iface` 一份，`lib/` 下每个平台机器码各一份 |

> **哈希必须逐平台各存一份。** 旧草案里是「一个 lib 一个哈希」，那是「单 arch 包」的遗留物；
> 全量揉捏之后每个平台都是独立文件，**选中了哪一个平台，就校验哪一个平台的哈希**。
> 校验只覆盖当前要用的那一份，其余平台的哈希用于**分发完整性**（整个 `.dialpkg` 的指纹）
> 而非链接期检查——这样既保证「要用的那份一定可信」，又不为用不到的机器码付出校验代价。

---

## 3. 一键安装与本地缓存

### 3.1 安装路径

| 入口 | 行为 |
| --- | --- |
| **`dialc install <URL>`** | 下载 `.dialpkg` → **解压前强制 SHA256 校验** → 解压到本地缓存目录 |
| **一键脚本**（Linux / Termux：`install-dial.sh`；Windows：`install-dial.ps1`） | 初始化 Dial 环境，**并自动扫描找到的 `.dialpkg` 逐个执行安装命令**；同时补齐配套件（官方标准库、VS Code 插件） |

- **缓存目录**：`~/.dial/packages/<name>/`（Windows：`%USERPROFILE%\.dial\packages\<name>\`）。
  **绝不污染用户的当前项目目录。**
- 缓存是**可重建的派生数据**，不是权威来源：用户可以整个删掉，编译器会重新解压。

### 3.2 一键脚本的行为

一键脚本承担三件事：

1. **初始化 Dial 环境**：创建 `~/.dial/` 目录树、把 `dialc` 挂进 `PATH`、写入工具链配置。
2. **自动安装已下载的包**：脚本在初始化完成后，**重新扫描**找到的 `.dialpkg`
   （当前目录及其子目录，或脚本参数指定的目录），对每一个执行安装命令。
3. **补齐配套件**：官方标准库（`stdlib.dialpkg`）与 VS Code 插件（`dial-<版本>.vsix`）
   静默下载并安装——它们都是「装上编译器后自然该有」的东西，不该再让用户手动跑一遍。

```
用户把 sqlite.dialpkg 丢进项目目录
            │
            ▼
   运行 install-dial.sh（Linux / Termux）或 install-dial.ps1（Windows）
            │
            ├── 初始化环境：建 ~/.dial/、挂 PATH、写配置
            │
            ├── 补齐配套件：stdlib.dialpkg / VS Code 插件（.vsix）
            │      └── 失败只告警、不阻断（离线或没装 VS Code 时照常装完编译器）
            │
            └── 重新扫描 → 找到 sqlite.dialpkg → 执行安装
                              │
                              ▼
              dialc install sqlite.dialpkg
                              │
                              ▼
              SHA256 校验 → 解压到 ~/.dial/packages/sqlite/
```

> **为什么脚本要「自动重新扫描」而不是让用户逐个点名**：一键安装的语义是
> 「我把这个目录交给你，你把该装的都装上」。用户不该需要记住包名、也不该需要为每个包
> 敲一条命令——包就在那里，脚本看得见。

> **配套件的版本只有一个来源**：`DIAL_VERSION`（默认**空**：跟随最新 Release；钉版本写法如 `DIAL_VERSION=1.4.0`）。Release 标签（`v1.04`）与插件文件名（`dial-1.4.0.vsix`）都由它推导——三处各写一份的做法，下次发版必然忘掉改其中一个。下载地址可用 `DIAL_VSIX_URL` 覆盖（自建镜像 / 网络受限时用）。
> 下载**必须带超时**（`--connect-timeout` / `--max-time`）：GitHub 不可达时 curl 会一直挂着，
> 「一键安装」变成「一键卡住」比装不上更让人手足无措。

### 3.3 解压格式与安全性

`.dialpkg` 是**专属格式**，因此解压器也是专属的：

- 解压前**必须**完成整包 SHA256 校验（§5）。
- 解压时**拒绝路径穿越**（`../`、绝对路径、符号链接指向包外）——包是外部输入，
  不能给它写入任意路径的能力。
- 解压目标固定为 `~/.dial/packages/<name>/`，**不接受用户指定的输出目录**
  （消除「解压到项目目录污染源码树」的可能）。

---

## 4. 无缝链接机制

### 4.1 用户视角

```bash
dialc build main.dial -o main
```

**用户不需要指定任何包、任何路径、任何平台。** 编译器自己完成全部决策。

### 4.2 编译器视角：`dialc build` 的包处理流水线

```
dialc build main.dial
        │
        ├─ ① 扫描包来源：~/.dial/packages/*/manifest.txt  +  项目内 *.dialpkg（未解压的）
        │
        ├─ ② 读取源码里的 import，确定被引用的包
        │
        ├─ ③ 对每个被引用的包：
        │       读 manifest.txt
        │       ├─ 按【当前编译 Target Triple】查 targets 映射字典
        │       │      └─ 查不到 → 当场报错（明确列出本包支持哪些 triple）
        │       ├─ 校验 iface 与【该平台机器码】的 SHA256
        │       │      └─ 不符 → 当场报错终止（绝不进链接器）
        │       └─ 读 xxx.diali → 喂给 Sema 做类型检查（供 LSP 补全 / 悬停）
        │
        ├─ ④ 链接阶段：把 targets[triple].lib 对应的文件加入链接命令
        │      ├─ 编译到 Windows  → 自动链接包内的 .lib
        │      └─ 编译到 Linux/Termux → 自动链接包内的 .a
        │
        └─ ⑤ 产出可执行文件（仍只依赖 libc / 平台运行库，见 §3.3 产物依赖契约）
```

### 4.3 「无缝」的确切含义

| 性质 | 说明 |
| --- | --- |
| **无显式配置** | 用户不写依赖列表、不写路径、不写平台。`import` 什么包，就链接什么包 |
| **无平台指定** | 编译器按自己的 Target Triple 索引；**交叉编译到 Windows 时自动链接 `.lib`**，无需用户干预 |
| **无手工干预** | 包未解压 → 自动解压；哈希不符 → 自动拦截；目标缺失 → 明确报错而不是静默降级 |
| **接口与实现解耦** | 类型检查读 `.diali`（一份，平台无关）；链接读 `lib/`（按 triple 选一份）。**二者互不依赖** |

### 4.4 绝对零版本冲突（底层兼容）

这是整套规范的核心赌注，也是它敢不做版本求解的底气：

> 因为 Dial 的底层**严格采用不透明指针（Opaque Handle）**，对外只暴露**标准 C ABI 函数**
> （[§14.1](README.md#141-不透明指针契约)、[§15](README.md#15-ffi-契约dial-语法--c-函数)），
> 外部代码只是**把句柄传给底层函数**来操作，完全不知道底层内存布局。

由此推出：

| 结论 | 说明 |
| --- | --- |
| **内部结构随便改** | 库作者可以任意重构内部数据结构、增删字段、换算法——只要函数签名与语义不变 |
| **旧代码零改动** | 调用方只需**链接新的机器码**即可完美运行，无需重新编译、无需改一行源码 |
| **依赖地狱被消灭** | 不存在「A 要 v1、B 要 v2 所以装不上」的局面：同一进程里只有一个 ABI，一个包只有一个版本 |
| **判据是 `abi` 字段** | 只要 `manifest.txt` 里的 **ABI 版本号不变**，就承诺二进制兼容；ABI 变更必须递增该号，编译器据此拒绝 |

- **一个包 = 一个 ABI = 无版本矩阵。** 规范里 `version` 只用于人类阅读与人工核对；
  编译器的兼容性判据**只有 `abi`**。这从根上取消了「版本区间求解」这个 NP 难题的存在理由。
- **ABI 变更是显式事件。** 库作者改坏了兼容性，就必须递增 `abi`；`dialc` 加载时若发现
  调用方期望的 ABI 与包声明不符，**当场报错**，而不是让不匹配的二进制悄悄链进产物。

> 这不是「尽力兼容」，而是**由类型系统强制的不可能冲突**：
> 你拿不到布局，就无法依赖布局；无法依赖布局，内部结构怎么改都不会破坏你。

> **跨平台版本也是同一个道理**：`.diali` 只有一份、ABI 号只有一个，
> 所以「Windows 版 1.4.0」与「Linux 版 1.4.0」在兼容性判据上是**同一个 1.4.0**，
> 不需要任何 per-platform 的版本矩阵。

---

## 5. 安全与分发机制

### 5.1 SHA256 强制校验（防断网与防篡改）

所有 `.dialpkg` **必须自带 SHA256 哈希校验**，由 `dialc` 在**解压前**强制验证。

| 场景 | 后果 |
| --- | --- |
| 下载时网络中断 → 文件残缺 | **当场拦截并报错**，绝不让残缺二进制进入链接器 |
| 文件被恶意篡改（植入后门） | **当场拦截并报错** |
| 哈希比对通过 | 正常解压、链接 |

铁律：

- **校验先于一切。** 整包校验发生在**解压之前**；解压之后、进入链接器**之前**，
  还要对**本次实际要用的那个平台的机器码**再校验一次（§2.3）。
  残缺或被篡改的二进制**绝不允许**流到链接阶段——否则就会产生「链接通过、运行随机崩」
  的幽灵 Bug：它不报编译错、不报链接错，只在客户机器上偶发，极难复现，
  最终全部变成无效的客服纠纷。
- **每个文件都要校验。** `.diali`（签名）与各平台机器码**各自**都有独立哈希。
  只校验其中一个是不够的：`.diali` 被篡改同样致命——它会让「类型检查通过」建立在谎言之上
  （签名说返回 `int`，真实库返回指针）。
- **失败即终止，不做降级。** 与 [Fail-Fast 铁律 §20](README.md#20-fail-fast-铁律宁可崩溃绝不静默损坏)同调：
  宁可让编译当场失败并给出明确原因，也绝不带病产出可执行文件。

### 5.2 拒绝中心化服务器

| 决策 | 规定 |
| --- | --- |
| **中心化服务器** | **拒绝**。不设官方包仓库、不做账号体系、不做中央索引、不做自建 CDN |
| **分发渠道** | **所有包（包括官方标准库）均通过 GitHub Releases 分发** |
| **成本** | 零运维成本，**利用 GitHub 的免费 CDN** |
| **版本对齐** | Release tag 与包版本天然对应，版本历史即 git 历史 |

这条决策与 Dial 的整体哲学一致：

- **不设账号 = 没有「谁可以发布这个包」的问题**，也就不需要审核、签名体系、信任根。
- **不用中央服务器 = 没有单点故障、没有带宽成本、没有「服务关停后生态归零」的风险。**
- **包的信任由哈希而非服务器担保**：GitHub Releases 只负责「把字节送到你面前」，
  「这些字节是不是我要的那个」由 SHA256 独立回答。**分发渠道与信任判定解耦**——
  换任何一个 CDN 或镜像，都不影响安全性。

### 5.3 分发与校验的分工

```
     GitHub Releases（免费 CDN）
              │  下载
              ▼
      .dialpkg（整包 SHA256 校验）──── 不符 ──→ 当场拦截，终止
              │  相符
              ▼
        解压到 ~/.dial/packages/<name>/
              │
              ▼
      按 Triple 选中某平台机器码 ──── 该文件 SHA256 再校验 ──→ 不符 → 终止
              │  相符
              ▼
         进入链接器
```

**两层校验的分工**：整包哈希回答「这个包完整吗、没被改过吗」；
单文件哈希回答「我要用的这一份机器码可信吗」。两者不可互相替代。

---

## 6. 这套规范刻意不做的事

把边界写清楚，和规范本身同等重要：

| 不做 | 原因 |
| --- | --- |
| 版本区间求解 / 语义化版本匹配 | ABI 不变即兼容，冲突不存在，求解无对象（§4.4） |
| 传递依赖自动解析 | 包内不含依赖声明；依赖由使用者显式 `import`，保持可见与可控 |
| 中央仓库 / 账号体系 / 官方索引服务 | §5.2：全部走 GitHub Releases，零中心化 |
| 源码分发 / 就地编译 | 只分发**机器码 + 签名**，用户机器无需装对方的工具链 |
| 包内执行构建脚本 | 无脚本、无钩子，包是**纯数据**，解压即可用，无可执行的副作用面 |
| 覆盖 / 污染用户项目目录 | 一律解压到 `~/.dial/packages/`，项目目录零侵入（§3.1） |
| 二进制差分 / 增量更新 | 一个包几十 MB，一次下载即可；增量更新带来的复杂度远超收益 |
| 自建 CDN / 镜像站 | §5.2：直接用 GitHub 的免费 CDN |

---

## 7. 与 v0.9 之前草案的差异（修订记录）

本章是《最终规范》，取代此前 README §25 中的「草案」版本。实质修订如下：

| 维度 | 草案（旧） | 最终规范（本文件） |
| --- | --- | --- |
| **包内架构** | 一个包一个 arch（`arch = x86_64-linux`，不匹配即拒绝） | **一个包包含全部平台**；`lib/` 目录平铺，`manifest.txt` 作映射字典索引（§2） |
| **机器码命名** | `libxxx.a` | `lib/<平台>.a` / `.lib`，由 `targets` 映射到具体文件 |
| **manifest 格式** | ini | **JSON**（映射字典的本质是嵌套键值） |
| **校验范围** | `sha256.lib` + `sha256.iface` 两个 | `iface` 一份 + **每个平台机器码各一份**；整包另有一层（§5.1） |
| **安装入口** | 仅 `import` 时静默解压 | 增加 **`dialc install <URL>`** 与**一键脚本**（Linux/Termux shell + Windows PowerShell），脚本会自动重新扫描并安装找到的 `.dialpkg`（§3） |
| **分发渠道** | 「不做中央仓库 / 网络下载」 | 仍**拒绝中心化服务器**，但明确**全部走 GitHub Releases + 免费 CDN**；`dialc install` 可直接吃 Release URL（§5.2） |
| **平台范围** | 顺带提及 macOS | 收敛为 **Windows / Linux / Termux**（§0）；macOS 明确不在目标内 |
| **Windows 可行性** | 未讨论 | 明确记录：**Windows 无 pthread**，需为 C Runtime 引入线程后端抽象（§0.2） |

---

## 8. 实现路线

| 阶段 | 内容 | 依赖 | 状态（v0.98） |
| --- | --- | --- | --- |
| 1 | `.dialpkg` 容器格式定义 + 解包器（含路径穿越防护）+ 整包 SHA256 校验 | 无 | ✅ **已完成** |
| 2 | `dialc install <URL>` / `dialc install <本地文件>` + 缓存目录管理 | 阶段 1 | ✅ **已完成**——`<URL>` 网络下载于 v0.96 落地（https 强制；`--sha256` 可钉死整包哈希；解压前逐文件校验） |
| 3 | `import` 语句的词法 / 语法 / Sema；`.diali` 解析器接入 Sema 与 LSP | 阶段 2 | ✅ **已完成**（LSP 侧已由 v0.95 的跨文件符号表落地：补全 / 悬停 / 跳转定义） |
| 4 | `manifest.txt` 解析 + Target Triple 索引 + 链接期选库 | 阶段 3 | ✅ **已完成** |
| 5 | 一键安装脚本（`install-dial.sh` / `install-dial.ps1`） | 阶段 2 | ✅ **已完成**（v0.96）——初始化环境（编译器 + `stdlib/` + `runtime/` + PATH + 配置）→ 自动重新扫描并安装 `.dialpkg` |
| 6 | Windows 线程后端（C Runtime 抽象层），使 `spawn` / `channel` 在 Windows 可用 | 独立 | 🟡 **MinGW 路线下已非阻塞**（winpthreads 提供 pthread 兼容层，stdlib 线程代码无需改写即可在 Windows 编译运行）；纯 MSVC 工具链所需的 Win32 后端仍待做 |
| 7 | 官方标准库改造为 `.dialpkg` 形态并走 GitHub Releases 发布 | 阶段 4 | ✅ **已完成**（首次为独立 Release `stdlib-v1.0.0`；**现改为随每个 `v*` 编译器 Release 同页发布**，与 zip / vsix 一起可被 `releases/latest` 重定向找到；三平台预编译 `libdial_std.a` 打包为 `stdlib.dialpkg`，统一 **ReleaseSafe=1**（`-DDIAL_SAFETY=1`）预编译；`dialc` 链接期**优先自动链入**已安装包、缺失则回退源码、ABI 不兼容明确报错要求重装） |
| 8 | LSP 工业化（四轮打磨）+ 三套编辑器接入 | 独立 | ✅ **已完成**（v0.98：能力协商 / 导航 / 编写辅助 / 高级能力四轮落地；诊断带稳定错误码；协议流纯净性有静态 + 动态两路守卫；VS Code / Neovim / Emacs 接入见 [§9](#9-编辑器支持与语言服务器)） |

> 以上为路线记录，**不含实现承诺与时间表**。
> 截至 v0.98 + 官方标准库 v1.0.0，已完成阶段 1 / 2 / 3 / 4 / 5 / 7，并落地了**三平台原生构建**
> （Linux x86_64 · Windows x64-MinGW · Termux aarch64-musl 全静态，见 [§0.1](#01-平台矩阵与-target-triple)）；
> 因此 `import` **已可使用**（见 [README §25.9](README.md#259-用法import-与-dialc-installv093-已实现v096-支持-url)），
> 不再是「不支持」；包的来源既可以是本地文件，也可以是 **URL**。
> 官方标准库 `stdlib.dialpkg` 现与三平台 zip、`.vsix` 同处每个 `v*` 编译器 Release 页面
> （首次发布为独立 Release `stdlib-v1.0.0`，见 [README §25.10](#2510-官方标准库stdlibdialpkg-独立发布)），
> 由 CI 自动构建并打包；安装脚本通过 `releases/latest/download/stdlib.dialpkg` 重定向取用，
> 版本与标签均不写死。
> 剩余：阶段 6 的**纯 MSVC 后端**（MinGW 路线下非阻塞）。

---

## 9. 编辑器支持与语言服务器

`dialc lsp` 是**唯一**的编辑器后端（stdio JSON-RPC）。v0.98 起，编辑器接入不再是
「装个插件就行」——能力清单、错误码、协议纪律都定下来了，写在这里以免各处各说各话。

### 9.1 编辑器支持矩阵

| 编辑器 | 接入方式 | 安装包 | 说明 |
| --- | --- | --- | --- |
| **VS Code** | 手写 LSP 客户端（无 npm 依赖） | `dial-<版本>.vsix`（`code --install-extension` 侧载） | 免 Node / npm / 联网；能力最全 |
| **Neovim** | 内置 LSP 客户端 | 拷进 `runtimepath` 即可 | 零依赖，不需要 nvim-lspconfig |
| **Emacs** | 内置 `eglot` | 仅 `init.el` 片段 | **不开发插件**——eglot 原生连 LSP |
| 其它（JetBrains / Zed / Sublime …） | 各自客户端启动 `dialc lsp` | — | 不单独维护插件 |

> 只做三套主流编辑器是**有意的取舍**：编译型语言的编辑器支持是必做的苦活，
> 但精力优先留给语言本身。三套之外的环境只要能启动 `dialc lsp` 就能用。

### 9.2 LSP 能力清单（v0.98）

| 能力 | 方法 | 说明 |
| --- | --- | --- |
| 诊断 | `textDocument/publishDiagnostics` | 只报第一个错误（与 `dialc build` 同因同位置） |
| 语义高亮 | `textDocument/semanticTokens/full` | 复用编译器自己的词法器 |
| Hover / 跳转定义 / 补全 | `hover` / `definition` / `completion` | 补全带精确替换范围与排序 |
| 文档符号 / 工作区符号 | `documentSymbol` / `workspace/symbol` | 大纲递归到局部变量 |
| 引用查找 / 重命名 | `references` / `rename`(+`prepareRename`) | 基于索引器的**引用边**，同名遮蔽天然正确；重命名按客户端能力选 `WorkspaceEdit` 形状（见 §9.4） |
| 签名帮助 | `signatureHelp` | `(` / `,` 触发 |
| 格式化 / inlay 提示 | `formatting` / `inlayHint` | 格式化**保守**（只去行尾空白，不重排）；结果按规范是 `TextEdit[]` 本身 |
| 代码动作 / 折叠 / 选择范围 / 文档链接 | `codeAction` / `foldingRange` / `selectionRange` / `documentLink` | 代码动作只发 `Command`，不擅自改代码 |

能力**按客户端声明动态广播**：客户端没提的能力我们不广播。
（多广播一个用不上的能力零代价；少广播则让功能**静悄悄消失**，用户查不出原因。）

### 9.3 诊断错误码

诊断带**稳定错误码**（LSP 的 `code` 字段）。消息是给人看的、码是给工具看的：

| 码 | 含义 |
| --- | --- |
| `dialc.lex` | 词法错误 |
| `dialc.parse` | 语法错误 |
| `dialc.sema` | 语义错误 |
| `dialc.package.missing` | `import` 的包未安装（附带 `data.package` 结构化包名） |

工具**应按码处理**，不要去匹配消息文本——消息措辞会随版本调整，改一次就废掉一个工具。

**这些码不是「私自扩展」**：LSP 3.17 规范原文是
`code?: integer | string`（注释：*"The diagnostic's code, which usually appear in the user
interface."*）——**码值由语言服务器自定，规范不设中央码表**；我们用 `dialc.` 前缀命名空间化，
避免与其他服务器的码撞车。规范对 `data` 的定义更直接：*"A data entry field that is preserved
between a `textDocument/publishDiagnostics` notification and `textDocument/codeAction` request."*
——我们用 `data.package` 把包名从诊断带到 quickfix，正是这个字段的设计用途。

### 9.4 规范符合性（只用标准，不自定义形状）

实现只使用**标准 LSP 方法与标准字段**，没有任何私有扩展：广播的 14 项能力、
处理的 24 个方法全部是 LSP 3.17 的标准方法；响应形状逐个对照规范。

两条容易被忽略、但必须照做的规定：

| 规定 | 规范原文 | 本实现 |
| --- | --- | --- |
| `textDocument/formatting` 的结果 | `TextEdit[]`（**数组本身**，不是任何包装） | 返回数组本身 |
| `WorkspaceEdit` 用哪个形状 | *"If a client neither supports `documentChanges` nor `workspace.workspaceEdit.resourceOperations` then only plain `TextEdit`s using the `changes` property are supported."* | **由客户端能力决定**：声明了 `workspace.workspaceEdit.documentChanges` 就发 `documentChanges`（带文档版本号），没声明就回退 `changes` |

还有两条容易漏、但同样影响**实际可用性**的：

| 规定 | 规范原文 | 本实现 |
| --- | --- | --- |
| 文档同步要**选项形式**才能要到 `didSave` | *"If present save notifications are sent to the server. If omitted the notification should not be sent."*（`TextDocumentSyncOptions.save`） | 广告 `{openClose, change: 1, save}`——纯数字形式里没有 `save`，客户端就不发 `didSave`，诊断便刷不出来 |
| `$/cancelRequest` 的语义 | 取消后应回 `RequestCancelled(-32800)` | 我们**保证每个请求恰好回一次响应**（客户端不会挂等），但不打断正在处理的请求——我们的请求都是微秒级的，没有可取消的长任务；要做「取消即中止」需要并发执行模型 |

这两条都写成了**自动化断言**（`tools/lsp_full_smoke.py` 的返回形状对照表与能力断言），
形状一旦跑偏当场失败——而不是等某个客户端连上来才发现不对。

### 9.5 协议流纯净性（铁律）

LSP 的 stdout 跑的是 `Content-Length` 分帧协议。**任何非协议字节都会让客户端分帧错位**——
症状不是报错，而是**编辑器卡死**（客户端在等一条永远对不上的消息）。

因此 stdout 只允许出现协议字节；日志一律走 stderr（CLI）或 `window/logMessage` 通知（LSP）。
这条纪律由两路自动化守住：冒烟测试的**动态**断言（发现裸字节即失败）
与 `tools/check_lsp_stdout_purity.py` 的**静态**守卫（LSP 路径上出现 `println!` 即失败）。

### 9.6 安装包获取方式

| 方式 | 步骤 |
| --- | --- |
| **一键安装**（推荐） | 下载发行 zip → 解压 → `bash install-dial.sh`（Windows：`install-dial.ps1`）。脚本装好编译器、标准库，**并自动装上 VS Code 插件** |
| **Release 资产** | 从 [Releases](https://github.com/shichencike/Dial/releases) 直接下载 `dialc-v<版本>-<平台>.zip` 与 `dial-<版本>.vsix` |
| **源码目录** | 克隆仓库后 `cargo build --release`，产物在 `target/release/dialc`；插件用 `bash editors/vscode/build.sh` 现构建 |

---

## 10. 语言层 FFI（v1.04）与包机制的分工

本文件前面 9 章描述的是**包分发**：`.diali` 声明外部函数、编成 `.a`、打 `.dialpkg`、
`dialc install`、`import`。v1.04 起还有一条**语言层 FFI** 路径，两者互补而非替代：

| 维度 | 包机制（§1~§9） | 语言层 FFI（v1.04） |
| --- | --- | --- |
| 声明位置 | 独立的 `.diali` 接口文件 | 直接写在 `.dial` 源码里（`extern fn` / `extern struct`） |
| 分发 | `.dialpkg` + SHA256 + 三平台机器码 | 不分发；函数体由系统库或用户本地 `.a` 提供 |
| 系统库（libc / libm） | 不适用 | 由 C 编译器**默认动态解析**，不写 `link`、不进 LTO 链路 |
| 用户 C 扩展库（静态） | 打成包，强制 `.a` / `.lib` | `link "xxx.a";`，验 ar 魔数，产物零第三方动态依赖 |
| 用户 C 扩展库（动态） | **不支持**（包分发铁律） | `link "xxx.so";` / `.dll` / `.dylib`，按扩展名验 ELF/PE/Mach-O 魔数并注入 rpath |

**包分发铁律不变**：`.dialpkg` 的 `targets` 只认静态归档，动态库一律安装期拒绝
（§5、J 组）。**语言层 `link` 与之分离**：源码作者可以显式声明动态库，代价是产物
运行期必须能找到那份库——ELF / Mach-O 由编译器注入 `RUNPATH`，PE 需把 `.dll` 放
exe 同目录或 `PATH`；不写 `link` 动态库的程序仍满足 §3.4 的「只链 libc」契约，
C 组 `ldd` 检查照常生效。系统库是天然例外，且刻意不进静态归档列表，以免 LTO
阶段因塞入庞大系统库而耗时爆炸。

C ABI 宽度、string / bytes 数据总线、`repr(C)` 结构体、C 回调等语言细节，
见 [README §33](README.md)。

---

## 11. 工程工具链与产包（v1.04 增补）

> 本节是**已落地实现**的记录，不是规划。完整用法见 [README §34](README.md)。

### 11.1 前端共用

`build / run / check / test` 共用 `main.rs::frontend()`：递归收集本地源码模块 →
解析包（拿不透明类型名）→ 逐个文件正式 parse → AST 合并 → 语言层 FFI 声明校验 →
语义分析。`check` 到此为止；`build / run` 继续 codegen + link；`test` 为每个测试块
构造一个只含该块的合成入口再走同一后端。共用一条前端是硬约束——「检查通过」与
「实际编译」不允许因为两套解析逻辑给出不同结论。

### 11.2 本地源码模块与包模块的分工

| 维度 | 本地源码模块（v1.04） | `.dialpkg` 包（v0.93 起） |
| --- | --- | --- |
| 声明 | `import "./util.dial";` / 裸名 `import "util";` | `import "包名";` |
| 来源 | 项目内 `.dial` 源码，编译期合并 AST | `~/.dial/packages/<name>/` 已安装 |
| 函数体 | 与调用方一起编译 | 预编译 `.a`，链接期缝入 |
| 校验 | 循环引用 / 同名声明 / 文件存在 | manifest / SHA256 / ABI / Triple |
| 产包 | 不适用 | `dialc pack` + `dial.toml`（v1.04） |

裸名优先本地：同目录存在 `<name>.dial` 即按源码模块处理，否则回退包解析。
这个优先级让本地模块可覆盖同名包，同时不动 v0.93 以来的包语义。子模块的
`link` 相对路径按**该子模块所在目录**解析，不是主文件目录。

### 11.3 产包闭环

`dialc pack` 读 `dial.toml`（一个显式的 TOML 子集：未知 / 重复 / 缺字段一律报错），
校验 `.diali` 与各平台静态库的 ar 魔数，生成 `manifest.txt` + 逐文件 SHA256，
压成 `.dialpkg` 后**就地**用 `parse_manifest` + `verify_archive` 自检。产包与安装
共用同一套校验实现（`src/package.rs`），不存在「自己打的包自己装不上」。

**包分发铁律（§5、J 组）一字未动**：`targets` 只认 `.a` / `.lib`，动态库一律拒绝。
`tools/make_stdlib_pkg.py` 保留给 CI 生成官方三平台 `stdlib.dialpkg`（一次收三份 `.a`），
与 `dialc pack` 的「单项目通用打包」分工明确。

### 11.4 诊断与 Fail-Fast

`CgOpts.src_file` + `gen_stmt` 记下的语句位置，在 `panic` / `assert` / 未捕获异常处
注入「文件:行:列」（编译期常量，运行期零额外开销）。所有 CLI 子命令对未知参数
一律非零退出，绝不静默吞掉多余输入（A3）；`fmt` 还会校验格式化前后 token 序列
完全一致且结果幂等，保证格式化不改变语义。

## 12. DWARF 调试信息与 `-g`（v1.04 增补）

### 12.1 定位

§11 的 A7 让**运行时诊断**带「文件:行:列」——它是编译期写进机器码的字符串常量，
给崩溃日志看的。本章补的是另一条线：让**调试器**理解 Dial 源码，属于元数据而非运行时逻辑。
两者共用 `semantic::stmt_pos()` 取语句位置，但产物完全不同，互不依赖。

`int3` / `ud2`（§20）解决「停在哪里」，DWARF 解决「停下之后看到什么」：
没有它，GDB 只能显示寄存器与地址；有了它，`break app.dial:9` / `print var_name` /
`bt` 的源码行 / `info args` 全部按 Dial 的 AST 名字工作。

### 12.2 生成链路

```
-g → CgOpts.debug_info
   → compile(): "Debug Info Version" 模块标志 + DebugInfoBuilder / DICompileUnit / DIFile
   → gen_function(): DISubprogram（挂到 FunctionValue.set_subprogram）
   → gen_stmt(): DILocation（语句行列）→ builder.set_current_debug_location
   → VarDecl / 形参: DILocalVariable + llvm.dbg.declare（绑定栈槽）
   → compile() 收尾: finalize() → module.verify() → emit_object()
```

- `DIFile` 用**绝对路径**：GDB 依赖行表里的文件名去打开源码；相对路径在多目录工程里
  会找不到文件，`list` / `break f:N` 直接失效。
- `DWARFSourceLanguage::C`：DWARF 没有 Dial 的语言码，用 C 是调试器兼容度最高的选择；
  调试器不会因此把 Dial 代码当 C 解析，它只按 DWARF 的结构读符号与行表。
- `finalize()` 必须在 `module.verify()` 之前（inkwell / LLVM 的约束），否则 deferred
  metadata 未收束，verifier 可能拒绝模块。

### 12.3 行号精确性与 RAII 守卫

`gen_stmt` 在入口把 builder 的当前 `DILocation` 切到本语句行列。嵌套语句（if / 循环 /
try / with）会递归调用 `gen_stmt`，若不在返回时恢复，外层后续指令就会沿用内层行号，
行表出现「跳行 / 串行」。`DebugLocGuard` 在 `gen_stmt` 入口保存旧位置、返回时由 `Drop`
恢复，覆盖所有 `return Err(...)` 早退分支——这是零人工维护成本的正确性保证。

### 12.4 类型映射边界

标量按 IR 真实宽度给出 `DIBasicType`（`int` / `char` 64 位 signed、`float` 64 位 float、
`bool` 1 位 boolean、`byte` 64 位 unsigned），所以 `print` 直接读到值。
其余类型（string / bytes / 数组 / map / set / struct / file / error / thread / channel /
opaque / rawptr / fnptr）在 IR 里一律是不透明 `ptr`（见 §1、§10），C Runtime 的内部布局
不对外承诺；因此 DWARF 里统一为指向 `dial_object` 的 pointer type，只保证「变量名 +
类型名可见」，不伪装成可展开的结构体。这与 §10「包分发 / 句柄不透明」是同一条边界。

### 12.5 与优化级别、各安全等级的关系

默认 O2 会把栈槽变量优化进寄存器或整个消除，调试器只能显示 `<optimized out>`。
故 `-g` 且非 `--fast` 时把 `OptimizationLevel` 降为 `None`（O0）；`-g --fast` 保留
用户明确要求的 O3 + SIMD + LTO/PGO，并在编译期提示变量可能被优化。
这条只影响 LLVM 优化级别：Fail-Fast 陷阱（§20）、`--safety` 三档、C Runtime 的
`-DDIAL_SAFETY=N`、`.dialpkg` 静态归档铁律（§5）全部不变。

`dialc test` 恒以 `debug_info = false` 编译测试块：测试只跑断言，不需要符号，
也避免逐块链接时重复生成调试元数据。

### 12.6 验证

R 组双路验证：静态用 `llvm-dwarfdump` 核对段、七个 Dial 名字与 `.dial` 行表；
动态用真实 `gdb -batch` 在 `.dial:9` 下断点断言 `print counter = 42` 与 `bt` 源码行。
另有无 `-g` 零调试符号、`-g --fast` 兼容两组断言。CI 若缺 `gdb` / `llvm-dwarfdump`，
对应子项明确跳过而非判失败。

---

## 13. 性能基准与工程维度度量（v1.04 增补）

一套独立于编译器主链路的性能度量体系，回答「Dial 相对 C / Go / Python 处在什么位置」。
它不改变编译产物，也不进入 `dialc` 的 CLI——由 `tools/` 下的 Python 脚本驱动。

### 13.1 等价性优先于绝对速度

跨语言对比最大的陷阱是「各写一套、结果不可比」。因此 28 个微基准被设计成
**纯整数、确定性、4 语言位级一致**：全程值域 `[0, 2^63)`，模数恒定且被除数
恒非负，整数除法只对正数（C/Go/Dial 向零截断 == Python `//`），位运算不触碰
符号位。每项产出一个十进制校验和，采集器内嵌真值表，要求 4 语言 28 项
（112 个）全部相等才计入对比。**校验和一致性是比值有意义的前提**。

四份实现各用该语言惯用的最优档：C `gcc -O2`、Go `go build`、Python CPython
解释执行、Dial `dialc build --fast`（O3 + LTO + SIMD）。口径差异在网页显式标注，
不伪装成「同档对比」。

### 13.2 计时的公平性

不采用各语言内部时钟 API（单位、单调性、精度都不同），而是**外部统一墙钟**：
每个程序只打印校验和，计时完全在采集器侧。为消除「C 单次几微秒、Python 几百
毫秒」的量级差与进程启动噪声，程序支持 `<任务名> [重复次数]`，采集器
**自校准**出使单次进程运行落在目标时长的重复数 `N`，跑多次取最小总时长，
扣除 `noop` 启动基线后除以 `N`。Dial 无 `parse_int`，用 `s.get(c) - 48` 手写
十进制解析——这是唯一一处为语言限制做的适配。

极快项或被编译器常量折叠成空操作的项（如 C `-O2` 折叠 b20 的字符串拼接）
会触及计时分辨率下限，统一钳到 5µs 并在网页标 `†`，避免 `0.000ms` 与
比值除零级虚高。**诚实标注优于制造精确假象**。

### 13.3 两类「维度」的分离

- **28 个 benchmark 维度**：微基准本身，产出绝对值（ms）与绝对比值
  （C = 1.00× 基准）。
- **6 个工程维度**（雷达图 6 轴）：速度（28 项耗时之和）、峰值内存、编译耗时、
  启动耗时、产物体积、源码行数。这是从微基准与工具链特性**聚合**出的工程画像，
  不是 benchmark 的分类。

6 轴量纲不同，按「各维最优语言 = 100、其余 = 最优/自身 ×100」归一化后叠加
4 语言多边形（越靠外越优）。原始绝对值与归一化得分都在网页给出，避免只看
雷达面积误读——各轴等权不代表任何具体场景的真实取舍。

### 13.4 数据、网页与 CI 的解耦

| 产物 | 生成者 | 是否入库 |
| --- | --- | --- |
| `bench/data.json` | `tools/bench_harness.py`（本地校准实测） | 是，权威快照 |
| `web/bench.html` | `tools/build_bench_web.py`（读 data.json） | 是，独立静态页 |
| CI 实测数据/网页 | 同一对脚本，`--out /tmp/...` | 否，仅 artifact |

`web/bench.html` 与 `web/cve.html` 一样是**独立页面**，不在 `build_web.py` 的
产物清单内，只复用 `assets/style.css`，因此官网同步校验
（`git diff --exit-code -- web/`）不受影响。CI 实跑采集器验证等价性与交付快照
配套，但产物只写 `/tmp` 后上传 artifact，**绝不回写** `web/bench.html`——
否则 CI 机器测得的数据与提交的快照不同，会让同步校验误报失败。
性能数值随机器与负载浮动，网页是某次实测快照，复现以采集器为准。

---

## 14. 真实程序缺口补齐与宪法裁决（v1.04 增补）

来源：`REPORT_v1.04_gaps.md`——按「普通程序员写日常程序」的思路写 54 个小程序 +
27 个探针后整理的 B/C 组缺口清单。本章只记**架构约束与分层决策**；规范正文在
README §37，版本记录见 [CHANGELOG.md](CHANGELOG.md)。

### 14.1 裁决先于实现，宪法不是需求池

B/C 共 22 项缺口不能按「想要什么就加什么」处理：**先过宪法，再排批次**。
§1 设计原则、§19 内存宪法、§20 Fail-Fast、§21 代码纪律、§5.3 零隐式转换、
v1.03「不引入闭包」共同构成否决权。本轮据用户裁决新增三条永久约束：

| 约束 | 影响的架构层 |
| --- | --- |
| **绝不引入可变全局状态**（术语替换） | 语义层禁止任何 `var` 全局变量；文件级 `const` 只读常量在编译期降维，不进运行期状态管理（§14.3） |
| **明确否决：函数名重载** | 符号表 / `.diali` / 包 ABI 全链路只认唯一名字，不做隐式名字改编（§14.2） |
| **明确否决：闭包** | 回调状态必须显式经 `rawptr<void>` 透传，编译器不生成任何隐式环境捕获结构 |

术语替换是**澄清而非放松**：被禁止的是可变共享状态（数据竞争根源），不是只读常量数据；
它在 v0.61 等历史语境下同样成立，因此历史条目一并替换、不改写史实。

### 14.2 为什么重载会破坏 ABI，而泛型单态化不会

Dial 的包契约是 `STDLIB_ABI` **显式整数版本号 + 链接期拦截**：`.dialpkg` 的
`manifest.txt` 写死 ABI，安装时与编译器常量比对，不匹配就明确拦下并给重装指引。
C++ 式重载把签名集合编进符号名，签名集合一变，符号名**隐式漂移**——编译器与包
没有机会用整数版本号发现不一致，破坏的是「链接期可验证」这一层。

被否决的是**用户可见的同名函数重载**。B2 泛型若采用单态化，编译器**内部**会为
每个实例生成带后缀的 IR 私有符号，但那不导出到 `.diali`、不改变用户可见函数名、
不影响包 ABI，两者不是一回事。同名函数继续按「重复定义」在编译期拒绝，
T 组用负向用例把这条锁死。

### 14.3 只读常量与字段默认值：编译期降维，不进运行期

B7 只允许文件级 `const`（标量字面量、数组字面量、已定义 const 引用）：

- 值必须是**编译期常量**，编译期直接降维到 LLVM `.rodata` 段或按需内联；
- 不需要 ARC、不引入全局构造顺序、不产生后台线程，与 §19 内存宪法完全兼容；
- 绝对禁止任何 `var` 声明的全局变量——这条不因 B7 破例。

B8 只允许在 `struct` 定义里显式写编译期常量默认值。没填且没有默认值的字段
**仍然编译期报错**：与 C「剩余字段静默零初始化」有本质区别，保留 §1
「比 C 更严格」的确定性底线。§1 措辞随之从「禁止省略结构体字段」修订为
「禁止省略**无默认值**的字段」。

二者共同点：**新增的是编译期信息，不是运行期可变状态**，因此都不触碰
ARC / COW / 异常栈 / 线程模型这些既有子系统。

### 14.4 标准库增量与 ABI 递增判据

C 组增量全部落在 `stdlib/*.c` + `stdlib/*.h`（C Runtime）、
`src/semantic.rs`（方法签名 / 内建分派）、`src/codegen.rs`（`declare_stdlib`
声明与 `call_c` 分派）、`stdlib/stdlib.diali`（自描述）四层，不改变
`lexer → parser → ast → semantic → codegen` 的链路结构。

`STDLIB_ABI 5 → 6` 沿用既有判据：**编译器可能要求旧包没有的符号**即递增。
本轮新增 `dial_eprint_*` stderr 输出族、`dial_map_get_or` /
`dial_kvmap_get_or`，以及第 2 批 C 组的 `dial_string_*`（大小写 / 子串 /
UTF-8 码点）、`dial_array_insert` / `index_of` / `reverse` / `reduce` /
`join` / `sort_string` / `clear`、`dial_file_*` / `dial_getenv` /
`dial_local_parts` / `dial_date_string` / `dial_monotonic_ms` 等导出。
装 abi=5 旧包的用户一旦写 `eprint(...)` 或第 2 批 API 就会撞上
`undefined symbol`，递增到 6 让旧包在链接期被明确拦下。`make_stdlib_pkg.py`
默认 `--abi 6`、CI 两处 `--abi 6` 同步；
`dialc new` 骨架改为引用 `package::STDLIB_ABI`，消除硬编码漂移；Q 组断言
改为从 `src/package.rs` 动态读取，避免下次递增时测试自己先过期。

`eprint` 族刻意不做「当前输出流」开关或全局 `FILE*`：四个分派函数
（`emit_print` / `emit_print_raw` / `emit_eprint` / `emit_eprint_raw`）各自独立，
守 §21.1「绝对拒绝过度抽象 / 宁可 WET」，也顺带不引入任何可变全局状态。

### 14.5 分批落地与回归纪律

按风险从低到高分 3 批，每批独立跑全量回归，不破既有 403 ✅：

| 批次 | 范围 | 状态 |
| --- | --- | --- |
| 第 1 批 | B9 块注释 / B10 `!` / B11 `.slice()` / C6 `get_or` / C8 `eprint` | **已落地**（T 组锁定） |
| 第 2 批 | C1–C5 / C7 / C9 / C10 标准库 API | **已落地**（U 组锁定：8 组端到端、C2 解析 Fail-Fast、C4 越界 Fail-Fast、`string[].index_of` 拒、C3 零隐式转换、第 2 批内建进保留名） |
| 第 3 批 | B7 文件级只读 `const` / B8 结构体字段默认值 / B6 结构体进容器 | **已落地**（V 组锁定，见 §14.6） |
| 收尾 | C11 独立包（`http` / `text` / `json` / `crypto`）、全量回归、文档官网同步、本地提交 | **已落地**（W / X / Y / Z 组锁定，见 §14.7） |

第 3 批的**范围裁决**（用户明确划定）：B7 / B8 / B6 落地；
B1（`impl` 方法）暂用 `fn` 替代、B2（用户泛型）死守单态化底线、B3（枚举 ADT）
不引入 Tagged Union——三者**本轮明确不碰**，保持类型系统极简。

### 14.6 结构体进容器：装箱 / 解箱与字段级 ARC 的现役边界

B6 让 `Point[]` 与 `map<string, Struct>` 成为可能。技术难点在内存模型：
Dial 的数组槽与 map 值槽**统一是 8 字节位模式**（float 走 IEEE754 bitcast、
bool 走 i1↔i64、句柄按位存指针），而结构体是**值类型**，按值装不进 8 字节槽。

本批采用的方案是**装箱**：值类型结构体拷进堆上引用计数对象
（`runtime/dial_rt_mem.c` 的 `dial_struct_box`，布局 `[int64 refcount][payload]`，
与既有 Dial 堆对象同构），槽内存其 payload 指针的位模式；读出时**解箱按值**拷贝。
这样复用了既有 ARC 通路（数组 / map 的 `kind=1` 走 `dial_rc_retain/release`），
容器释放即释放装箱对象，**不需要任何新的所有权子系统**。

边界（**如实标注，不夸大**）：装箱只拷贝位模式，**含句柄字段的结构体**需要装箱与
释放时逐字段 retain/release（字段级 ARC），该能力尚未落地，因此语义层
**明确拒绝**这类结构体进容器（`struct_has_only_scalar_fields`），
而不是让它「看起来能用」却在释放时泄漏或悬垂。这条边界写进了 README §37.9
与 V 组回归用例。

本批顺带修好的通用缺陷：字段访问此前只支持「变量 / 字段链」作为基对象，
`Pt{}.x` / `make().y` / `arr[i].f` 一律报「仅支持对变量/字段进行赋值」。
修法是让 `gen_expr` 的字段分支把非 lvalue 基**物化到栈上临时槽**再取字段指针；
`gen_lvalue_ptr` 保持只服务**数组元素写**（`a[i] = v`），避免把写回落到临时副本上。
B7 的 const 结构体内联后「取字段」成为常态路径，这条修复因此是 B7/B6 的必要前提。

C11（正则 / JSON / 摘要 / Base64 / HTTP）**不进 `stdlib.dialpkg`**：与
§21.2「不引入无用依赖」一致，这些能力各自带平台差异与第三方实现，塞进标准库
会让每个 Dial 程序都背上它们；改为独立 `.dialpkg`、独立发版、独立校验
ABI / SHA256，语言层 FFI（§10）已提供接 C 库的能力。

> **交付约束**：并入 `[v1.04]`；`Cargo.toml` 版本 `1.4.0` 不变；只做本地提交，
> 不推送、不触发远程 GitHub Actions、不打 tag、不发 Release。
> 规范先于实现写清，但**绝不把未实现项标成已支持**——每批落地后同步更新
> README §37 的「已落地」表。

### 14.7 C11 独立包架构：四个包的分层与 FFI 约定

§37.5 定下「常用库走独立 `.dialpkg`、不进标准库包」，并按领域拆包。
本轮四个包全部落地：`http`（HTTP/1.1 + TLS 1.3 + keep-alive + HTTP/2）、
`text`（正则）、`json`（解析 / 序列化）、`crypto`（摘要 / Base64 / HMAC）。
下表以 `http` 为例给出完整分层；其余三包的分层同构，差异见本节末尾。

**分层**（每层职责单一，不互相越界）：

| 层 | 位置 | 职责 |
| --- | --- | --- |
| 接口契约 | `packages/http/http.diali` | **唯一**对外签名：`type HttpConn;` / `type HttpResponse;` + `extern fn`。带 SHA256 校验，被篡改在链接期被拦下 |
| 清单 | `packages/http/dial.toml` | 包名 / 版本 / ABI / 接口文件名 / 许可 / 三平台静态库路径 |
| 机器码 | `packages/http/src/dial_http.c` | 协议实现：URL 解析、请求行与 Header 拼装、响应解析（`Content-Length` / `chunked` / 读到关闭）、Socket 读写、gzip 解压；`dial_http.c` 还管 keep-alive 连接池与客户端画像 |
| TLS 封装 | `packages/http/src/dial_tls.c` | mbedTLS 客户端封装：自定义 BIO（复用调用方 socket）、SNI=host、ALPN 由调用方定、系统根 / 自定义 CA 双信任链、证书校验 fail-closed |
| HTTP/2 帧层 | `packages/http/src/dial_http2.c` | 帧头解析 / 8 类帧、会话与 stream 状态机、单 I/O 线程独占 socket/TLS、按 `stream_id` 分发并唤醒 worker；`https` 走 ALPN `h2`、`http` 走 h2c prior-knowledge |
| HPACK | `packages/http/src/dial_hpack.c` + `dial_hpack_tables.h` | 自研 RFC 7541：请求侧最简编码，响应侧静态表 61 项 / Huffman 解码 257 项 / 动态表 / 整数前缀编码；码表由 `tools/gen_hpack_tables.py` 从 Python `hpack` 常量生成 |
| 第三方 | `third_party/` | mbedTLS（TLS 1.3，已接线上层）+ zlib（gzip 解压）。**vendor 进仓库**，构建离线可复现 |

**协议层自研、密码学不手搓**：HTTP 的报文与线级细节自己写（可控，才能为后续的
连接指纹定制留空间）；TLS 与压缩算法一律用成熟实现——自己写密码学是安全事故的
经典入口（侧信道、padding oracle、随机数质量、边界条件）。

**所有权口径**：包对外暴露的不透明句柄**不进 ARC**，释放必须显式
（`dial_http_close` / `dial_http_resp_free`），与 stdlib 的文件句柄同一口径——
「谁分配谁释放」在 FFI 边界上必须看得见。返回给 Dial 的 `string` / `bytes`
则走 Dial 自己的引用计数堆对象，由 Dial 侧 ARC 负责。

**错误口径**：包**不抛 Dial 异常**——包不应假定宿主程序装了异常处理栈。
失败返回空句柄（`is_null` 可判），原因取自**线程局部**的最近错误槽
（`dial_http_last_error` / `dial_http_last_error_msg`）。

**两条必须写进包注释的 FFI 约定**（写包时踩到，且都不是 Dial 特有的）：

1. Dial 的 `string` 跨到 C 时传的是**载荷指针**（`[int64 len][bytes...]`），
   不是字节首地址——标准库因此才用 `dial_string_cstr()` 取文本（载荷 + 8）。
   把收到的字符串直接当 `const char *` 用，内容会带上前导的长度字段。
2. 带缓冲的读取器必须**先取缓冲再读 socket**：行读取可能已把整份响应吞进缓冲，
   直接读 socket 会漏掉缓冲里的部分，并对已关闭的连接误报「连接提前关闭」。

**链接顺序**：静态归档由链接器从左到右扫描，符号只有在其归档被扫描时仍处于
「未定义」状态才会被拉入。包要向 Dial 返回 `string` / `bytes`，就必须引用标准库的
`dial_string_new` / `dial_bytes_new`，因此包的归档**必须排在标准库归档之前**
（`obj → runtime → packages → stdlib`）。标准库从不反向引用包符号，故该次序安全。

**现役能力（如实标注）**：`http` 包已落地 **明文 HTTP/1.1 + TLS 1.3（https）+
keep-alive 连接池 + HTTP/2（h2c 明文 / h2 ALPN，自研 HPACK，真·并发多路复用）**：
`https://` 走 mbedTLS，证书校验 **fail-closed**（系统根与自定义 CA 两条信任链，
自签名不可冒用）；同一主机连续请求复用已建连接（`dial_http_pool_hits` 可见命中）；
客户端画像只做兼容性配置（头序 / UA / ALPN），**不做以规避识别为目标的绕检测逻辑**。
进程级 keep-alive 池为全局数组（host:port:scheme 为键）+ 可移植互斥锁，
服务器发 `Connection: close` 或连接 EOF 时移出池。

**HTTP/2 并发模型**：一个 `Http2Session` = 一条连接 + 一个**专用 I/O 线程**。
mbedTLS 的 `ssl_context` 不能被多线程并发读写（未开 `MBEDTLS_THREADING_C`），
因此该线程独占 socket / TLS，负责把各 worker 提交的帧原子写出、读帧、按
`stream_id` 分发到对应 stream 并 `cond_broadcast` 唤醒等待者；Dial 侧多个
`spawn` worker 只入队帧并在自己的 condition variable 上等待，醒来后**在 worker
自己线程里**组装 `DialHttpResponse`（reader 线程绝不构造 Dial ARC 对象）。
`Http2Session` 是不透明裸指针、不进 ARC，多 worker 按值共享、由
`dial_http2_session_free` 显式释放。POSIX 用 pthread 互斥/条件变量，Windows 用
`CRITICAL_SECTION` / `CONDITION_VARIABLE` / `CreateThread`。ALPN 未协商出 `h2`
时 fail-closed 拒绝降级为 HTTP/1.1。

**其余三个包（同批补齐，分层同构）**：

| 包 | 契约 | 实现 | 关键架构决策 |
| --- | --- | --- | --- |
| `text` | `text.diali`（`type Regex;`） | `dial_regex.c`（Thompson NFA 编译 + Pike VM）+ `dial_text.c`（FFI / 替换 / 错误槽） | 线性时间正则，零第三方依赖；不透明句柄不进 ARC；能力子集与已知语义边界写进契约 |
| `json` | `json.diali`（`type Json;`） | `dial_json_codec.c`（递归下降解析 + 紧凑序列化 + 深拷贝）+ `dial_json.c`（FFI / 访问器 / 构造器） | 值树封在单一不透明句柄里（`.diali` 无泛型容器）；所有权只有「返回即独立拥有、一律 free」一条规则；整数走 `int64_t` 避免 >2^53 丢精度 |
| `crypto` | `crypto.diali`（无状态，无句柄） | `dial_crypto.c`（FFI 包装）+ vendor mbedTLS 全量 | 摘要 / HMAC / Base64 全部走 mbedTLS，**不实现任何密码学原语**；`string` / `bytes` 双入口；失败返回空值 + 线程局部错误码，`bytes` 类返回长度 0 的空缓冲（Dial 的 `is_null` 不含 `Type::Bytes`） |

三包与 `http` 共用同一套 FFI 约定（`DialString` 句柄、长度优先于 NUL、
返回值必须构造句柄、`.diali` 单行声明、错误走返回码），故链接顺序同样是
`obj → runtime → packages → stdlib`。回归分别由 X / Y / Z 组锁定。

**尚未落地（后续独立包 / 批次，不许当已支持）**：HTTP/3.0（QUIC，独立
`http3.dialpkg` + vendor ngtcp2/nghttp3）。

---

> 本文件为**架构规划**，不含任何实现承诺与时间表。规范一旦在真实包上落地，
> 将同步更新本文件、README §25 与 [CHANGELOG.md](CHANGELOG.md)。
