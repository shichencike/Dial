# dialpkg 包管理路线图（2026-11-19 战略转向版）

> **本文是与 [`replace-llvm-roadmap.md`](replace-llvm-roadmap.md) 并行的第二条路线。**
> 前者回答「编译器后端如何代替 LLVM」；本文回答「包与包管理如何做到让人放弃其它 C 包管理器」。
> 两条路线共享同一套护栏（LLVM 路径零改动、零新依赖、docx 确定性、不打 tag），但各自独立推进。
> 那篇是「代替 LLVM」主线，这篇是「代替 vcpkg / conan / xmake」主线。

## 0. 战略转向：从「Dial 专用分发格式」到「通用 C 包管理标准」

用户 2026-11-19 决策原文精神：

> 再加入一个路线，打磨包还有包管理相关命令，打磨到极致好用。标准是什么？标准到足够让人
> 放弃掉其他，用这个作为 C 包管理分发。再给 dialpkg 强化一下，可以放 C 源代码。

这条决策**正式推翻**了旧规范（README §25.7 / ARCHITECTURE §6）写死的四条「刻意不做」铁律：

| 旧铁律（刻意不做） | 旧理由 | 本路线裁决 |
|---|---|---|
| C 源码分发 / 就地编译 | 包是纯数据、用户机器无需工具链 | **推翻**——C 源码双模式纳入（§2） |
| 版本区间求解 / 语义化版本匹配 | ABI 不变即兼容、冲突不存在 | **推翻**——完整语义版本纳入（P4.2） |
| 传递依赖自动解析 | 依赖显式 import、保持可见可控 | **推翻**——传递解析 + lockfile 纳入（P4.2） |
| 中央仓库 / 账号体系 / 官方索引 | 全走 GitHub Releases、零中心化 | **部分保留**——不建中心服务，但加静态索引/search（P4.1） |

保留不变的边界：包内**绝不执行任意构建脚本**（零脚本可审计是核心卖点，见 §1）；包只解压到
`~/.dial/packages/`、项目目录零侵入；许可隔离（§25.8）继续强制。

**为什么敢推翻**：旧理由是「防依赖地狱、防副作用面」——那是在包格式刚设计时的保守起点。如今
dialpkg 已有完整的 SHA256 逐文件校验、`.diali` 签名接入 Sema/LSP、按 triple 选库、内容哈希缓存，
地基足够牢。真正让用户放弃 vcpkg/conan 的，恰恰是它们最痛的两点：**要跑构建脚本、依赖地狱**。
dialpkg 的机会是用「零脚本 + 确定性 + 原生 FFI」重新定义 C 包体验，而不是继续躲在「极简」里。

## 1. 什么叫「极致好用」——可度量验收标准

用户选定的 5 个维度，每条给出可检验的判据（每期交付逐条对照）：

| # | 维度 | 可度量判据 |
|---|---|---|
| 1 | **零脚本可审计** | `install` 全程不执行任何构建脚本；包内脚本数恒为 **0**；`pack --verify` 逐文件 SHA256 覆盖率 **100%**；`native-src/` 源码仅供人审计、不进执行路径 |
| 2 | **一条命令装完即用** | `dialc install <url>` 一条命令完成（下载/校验/解压/选库）；**无 sudo、无对方工具链**（prebuilt 默认模式）；`import` 后直接编译运行，零手工改环境变量 |
| 3 | **完整依赖管理** | `dial.toml` 支持语义版本区间声明；传递依赖自动解析；`dial.lock` 锁定精确版本保证可复现；`dialc search <名>` 能在静态索引中找到包 |
| 4 | **Dial 原生 FFI** | `.diali` 接入 Sema + LSP（跳转/补全/签名提示）；三平台预编译机器码 `import` 即用；C 库用起来像 Dial 原生库一样顺手 |
| 5 | **pack 全自动** | 一条 `dialc pack` 自动填 manifest、自动算逐文件哈希、自动交叉编译产出三平台 `.a`；用户零手填字段；两类源码目录自动分工 |

> 这 5 条是「让人放弃其它」的产品定义，不是口号。每期交付必须能逐条勾验；勾不满的维度
> 明确标注「本期未达、留待 P4.x」，不许含糊。

## 2. C 源码双模式设计（化解「放 C 源码」与「用户无需工具链」的冲突）

用户选择「prebuilt 与 source **两者都支持，按包声明选择**」。设计如下：

### 2.1 模式一：prebuilt（默认，打包期编译）

- 包作者在 `c/` 放 `.c`/`.h`，`dial.toml` 声明 C 源与编译参数。
- `dialc pack` 用 `cc_command()`（`src/main.rs`，支持 `DIAL_C_COMPILER --target=`）**交叉编译**
  成三个官方 triple 的 `.a`：`x86_64-linux-gnu` / `x86_64-pc-windows-gnu` /
  `aarch64-unknown-linux-musl`，归档进 `lib/<triple>/`。
- 源码（`c/` 或 `native-src/`）随包分发供审计，但**用户 `install` 只拿机器码**，无需装任何 C 工具链。
- **守住旧铁律精神**：默认路径下包仍是「装完即用」，用户机器零工具链依赖。

### 2.2 模式二：source（安装期就地编译，按声明 opt-in）

- 包在 `dial.toml` 显式声明 `mode = "source"`（或某 target 无预编译 `.a` 时回退）。
- `dialc install` 按 `host_triple()`（`src/package.rs`，可被 `DIAL_TARGET` 覆盖）用 `cc_command()`
  就地编译 `.c` → `.o` → `ar rcs` 成 `.a`，按「源内容 + flags + cc 版本 + triple」内容哈希缓存。
- 用户机器**需要**对应 C 工具链，`install` 前置检查并在缺失时明确报错（Fail-Fast，给出安装指引）。
- 就地编译**仍不执行任何脚本**——只做「声明式编译」：编译什么、什么参数全由清单静态决定，
  没有钩子、没有 `./configure`、没有 `CMakeLists.txt` 执行。

### 2.3 两种模式的统一

| 维度 | prebuilt | source |
|---|---|---|
| 编译时机 | pack 期（包作者机器） | install 期（用户机器） |
| 用户工具链 | 不需要 | 需要 |
| 包体积 | 含多平台 `.a`（较大） | 仅源码（小） |
| 审计性 | 源码随包 | 源码即主体 |
| 默认 | ✅ 是 | 否（显式声明/opt-in） |
| 零脚本 | ✅ | ✅（声明式编译，无脚本） |

## 3. 包项目目录结构（用户诉求的目录映射）

用户补充诉求明确要「两类源码目录 + pack 全自动」：

```
mylib/
├── dial.toml          # 清单：name/version/abi/iface/license + [targets] + [src] + [c] + 依赖声明
├── mylib.diali        # 接口签名（平台无关，供 Sema/LSP）
├── src/               # ① Dial 源码模块（现有 §25.11）——只分发、不机器码编译
│   └── mylib_oo.dial  #    由 dialc 前端合并编译
├── c/                 # ② C 源码目录（P4 新增）——pack 时自动交叉编译成三平台 .a
│   ├── mylib.c
│   └── include/
│       └── mylib.h
└── native-src/        # ③ 源码存档目录（P4 新增）——仅供审计，不编译
    └── upstream/      #    如第三方 C 源码快照，随包分发供人审阅
```

- `src/`：**不产生新机器码**，由 dialc 前端按本地模块编译（现状不变）。
- `c/`：**自动编译**——`pack` 扫描此目录，按清单参数交叉编译归档（P4.0 实现）。
- `native-src/`：**不编译**——纯源码存档，满足审计/合规诉求，落包但不进编译路径。

`dial.toml` 清单前瞻字段（P4.0 落地时细化，本路线图只定形）：

```toml
name = "mylib"
version = "1.2.0"
abi = 6
iface = "mylib.diali"
license = "MIT"

[targets]                 # 预编译 .a（prebuilt 模式）；可由 pack 自动生成
x86_64-linux-gnu = "lib/x86_64-linux-gnu/libmylib.a"

[src]                     # Dial 源码模块（现有）
mylib_oo = "src/mylib_oo.dial"

[c]                       # P4 新增：C 源码声明
sources = ["c/mylib.c"]
includes = ["c/include"]
flags = ["-O2"]
defines = ["MYLIB_VERSION=1"]
mode = "prebuilt"         # prebuilt | source
```

> `pack` 全自动目标：上表中 `[targets]` 的哈希与多平台 `.a`、`manifest.txt` 的 sha256 映射，
> 全部由 `dialc pack` 自动生成/填充，用户只写业务字段（name/version/iface/license/[c]），
> 零手填哈希、零手动归档。

## 4. 命令面分期

### P4.0 · 核心能力（下一轮落地）

- `pack` 编 C 源：扫描 `c/`，`cc_command` 交叉编译三平台 `.a` 归档；自动生成 manifest + 逐文件 SHA256。
- `install` source 模式：缺 `.a` 时按 `host_triple()` 就地声明式编译 + 内容哈希缓存；前置工具链检查。
- 原子安装 + 回滚：临时目录解压 + rename 发布，失败回滚旧包，杜绝半包残留。
- `--json` 全覆盖：`install` / `list` / `uninstall` / `new` 补齐机器可读输出。
- `doctor` 包健康检查扩展（校验已装包哈希、缺失 target、ABI 一致性）。

### P4.1 · 静态索引与搜索

- `dialc search <名>`：基于静态索引文件（GitHub Releases 托管，无中心服务）搜索可用包。
- `dialc info <名>`：查看包元数据、版本列表、ABI、许可、平台支持。
- 索引签名校验，沿用 SHA256 信任链。

### P4.2 · 完整依赖管理（最重，单独一期）

- `dial.toml` 增 `dependencies`：语义版本区间（caret / tilde / 精确）。
- 传递依赖解析 + 版本仲裁（冲突明确报错，不静默覆盖）。
- `dial.lock`：锁定精确版本与哈希，保证可复现构建。
- 版本选择算法 Fail-Fast：无解时给出可读的冲突路径。

### P4.3 · 离线 / 镜像与增量

- 离线模式（`--offline` 只用缓存）、镜像源配置、下载进度与断点续传。
- 增量更新评估（当前包体积大、收益待证——先量化再决定，不盲目做）。

## 5. 与「代替 LLVM」路线的关系

- **并行独立**：本路线不改编译器后端，只动包工具链（`package.rs` / `pkgbuild.rs` / `main.rs` 包命令部分）
  与文档；与 P0 正确性 / P1 平台 / P2 功能 / P3 工程化无耦合。
- **护栏一致**：LLVM 路径零改动、零新依赖（复用现有 `cc_command`/`zip`/`sha2`）、docx 确定性、
  不打 tag、版本 1.10.0 不变。
- **验收独立**：本路线的完成判据是 §1 五维度逐条勾满，与「asm 代替 LLVM」的验收判据分开判定。

## 6. 可复现验证（每期）

- **pack→install→import 联动**：造含 `src/` + `c/` + `native-src/` 的样例包 → `pack` 自动产出三平台 `.a`
  与完整 manifest → `install` → 裸名 `import` → 编译运行输出与预期逐字节一致。
- **哈希篡改拦截**：改包内任一文件 → `install` 拒绝、不落字节（复用 `verify_archive`）。
- **双模式验证**：prebuilt 包在无交叉工具链环境可装；source 包缺工具链时 Fail-Fast 并给指引。
- **回滚验证**：模拟安装中断 → 旧包完好、无半包残留。
- **FFI 验证**：`.diali` 在 LSP 跳转/补全生效，Sema 签名校验通过。
- **JSON 验证**：所有 `--json` 命令输出可被 `jq` 解析、字段稳定。
- **确定性**：`pack` 同一输入两次产出字节一致（哈希/时间戳归一化）。

## 7. 当前状态（2026-11-20 更新）

**前置门槛已满足**：用户要求「P1–P3 全部完成才进 P4」，2026-11-19 已复核 P0 正确性 / P1 平台
（AArch64 语义+DWARF）/ P2 功能对等裁决 / P3 工程化（fuzz 盲区）全绿（见
[`replace-llvm-roadmap.md`](replace-llvm-roadmap.md) §3），gated 放行本路线。

### P4.0 · 核心能力 —— ✅ 已落地（2026-11-20）

| 子项 | 实现 | 证据 |
|---|---|---|
| pack 编 C 源（prebuilt） | `pkgbuild.rs` 新增 `[c]` 段解析 + `cross_compile_c()`：`zig cc -target` 交叉编译三平台 `.a`，自动生成 `[targets]` 与逐文件 SHA256；可复现三件套 `-g0`/`-ffile-prefix-map`/`SOURCE_DATE_EPOCH` | 门禁 1/2/3：三平台 `.a`、两次 pack 逐字节一致、源码随包审计 |
| install source 模式 | `package.rs` 的 `CInfo` manifest 字段 + `build_c_for_host()`：缺预编译 `.a` 或 `mode=source` 时 install 期就地声明式编译（零脚本）+ 更新 manifest.targets；工具链缺失 Fail-Fast | 门禁 7/8：就地编译出 `.a` 且 import 运行正确、缺 zig Fail-Fast 给指引 |
| 原子安装 + 回滚 | `install_inner()` 改为临时目录解压 → 校验 → `rename` 原子发布；覆盖安装先备份旧目录，失败回滚 | 门禁 5/6：无 `.install`/`.backup` 残留、篡改包被拒且旧包完好 |
| `--json` 全覆盖 | install / list / uninstall / new 补 `--json`（pack/doctor 原有） | 门禁 9：全部可被 `python3 -m json.tool` 解析、字段稳定 |
| doctor 扩展 | `check_installed_platform()`：已装包逐文件 SHA256（原有）+ 缺失 host target + ABI 一致性 | 门禁 10：含「包平台/ABI 一致性」检查项 |

- **门禁固化**：`tools/verify_p4_pkg.sh`（10 项，缺 zig 自动 skip）挂入 `run_tests.sh` AK 段。
- **§1 五维度对照**：① 零脚本可审计 ✅（清单声明式编译、包内脚本数恒 0、源码随包）；
  ② 一条命令装完即用 ✅（prebuilt 无需用户工具链）；③ 完整依赖管理 ⬜ **P4.2**（本批未达，
  其中 `search` 部分已由 P4.1 达成）；④ Dial 原生 FFI ✅（`.diali` 装完即列导出函数、双后端
  import 运行通过）；⑤ pack 全自动 ✅（自动填 manifest/哈希/三平台 `.a`，用户零手填）。
  维度③的语义版本/传递依赖/lockfile 留待 P4.2。
- 零产品代码外的依赖变化：复用现有 `zig`/`zip`/`sha2`/`serde_json`，无新依赖；版本 1.10.0、不打 tag。

### P4.1 · 静态索引与搜索 —— ✅ 已落地（2026-11-20）

| 子项 | 实现 | 证据 |
|---|---|---|
| `dialc search <词>` | `registry.rs` 静态索引搜索：子串匹配包名/描述、多版本去重取最新、按包名排序；表格 + `--json` 稳定字段 | 门禁 1/2/3：子串匹配、去重、描述匹配、JSON 合法 |
| `dialc info <包名>` | 精确包名匹配，展示元数据/版本列表/ABI/许可/平台 + 可直接复制的安装命令（`--sha256` 有时自动带上） | 门禁 2/3：版本列表有序、ABI/许可/平台、JSON 有序 |
| 索引签名校验 | 沿用 SHA256 信任链：`--index-sha256` 钉死索引整文件哈希；**哈希不符 Fail-Fast 且绝不回退缓存**（钉死来源不可被绕过）；回退缓存也校验钉的哈希 | 门禁 4：正确值通过、错误值非零退出不回退 |
| 无中心服务 + 可复现 | 索引是静态 JSON 单文件（默认 GitHub Releases，`--index`/`DIAL_INDEX_URL` 可覆盖）；下载复用 `package::download_to`（https 强制）；本地缓存 `~/.dial/cache/index.json` 原子落盘；`--offline` 只读缓存 | 门禁 5/6/7：offline 命中/缺失 Fail-Fast、schema 错误 Fail-Fast、网络失败回退且来源标注准确 |

- **门禁固化**：`tools/verify_p4_index.sh`（12 项，缺 curl/python3 自动 skip）挂入 `run_tests.sh` AL 段。
- **§1 五维度对照**：③ 完整依赖管理——`search`/`info` ✅（静态索引可发现包）；语义版本区间 /
  传递依赖解析 / `dial.lock` ✅ 由 P4.2 达成。其余维度沿用 P4.0 达成状态。
- **信任模型（务实、不含糊）**：索引靠 HTTPS 传输保护 + 可选 `--index-sha256` 钉死整文件；
  每个包条目登记 `sha256`，`info` 直接给出带 `--sha256` 的安装命令，把信任链从索引延伸到下载物。
  **不宣称非对称签名**——那需要新依赖或手写密码学，违反零依赖/不手写安全栈铁律。
- 零新依赖：复用 `curl`/`sha2`/`serde_json`；注册模块 `src/registry.rs`；版本 1.10.0、不打 tag。

### P4.2 · 完整依赖管理 —— ✅ 已落地（2026-11-20）

| 子项 | 实现 | 证据 |
|---|---|---|
| 语义版本区间 | 新模块 `version.rs` 手写 semver 子集：`^` caret（锁最左非零位）、`~` tilde（锁 minor）、`=`/裸版本 精确、`*` 通配；预发布比较；区间**交集**运算供仲裁；语法非法 Fail-Fast | version 单测 4 项 + 门禁 1：caret 选最高 2.1.0 不跨大版本 |
| 传递依赖 + 版本仲裁 | 新模块 `resolve.rs`：工作队列传播传递依赖；同一包多约束求交，空集即冲突（Fail-Fast 打印每条约束的**来源路径**）；正式版优先、选最高满足版本；缺包 Fail-Fast；环检测（去重终止） | 门禁 2/3/4：冲突给出 `http@1.0.0 -> json` 路径、缺包报错、环终止单测 |
| `dial.lock` | `resolve` 生成精确 `name/version/abi/url/sha256` 锁定文件（原子写）；`--check` 校验 lock 与解析一致且未被篡改；`install --locked [目录]` 按 lock 逐包下载安装（钉 SHA256） | 门禁 5/6/9：lock 生成、`--check` 一致/篡改拦截、按 lock 装真实包并可 import 运行 |
| 索引 schema v2 | `registry.rs` 的 `IndexEntry` 增 `deps` 字段（包名→区间），schema 升 2 且**向后兼容 v1**（v1 条目视为无依赖）；deps 区间在解析期即校验 | P4.1 门禁 12/12 回归（v1 索引仍可读、schema 99 仍拒） |
| CLI 与工程化 | `dialc resolve [目录]`（`--json`/`--check`/`--index`/`--index-sha256`/`--offline`/`--lock`）；`dial.toml [dependencies]` 段解析；usage/completion 同步；与源码级 `deps`（import 图）命名区分 | 门禁 7/8/10：JSON 合法、offline 缓存解析、version/resolve 单测全过 |

- **门禁固化**：`tools/verify_p4_resolve.sh`（11 项，缺 curl/python3/zig 自动 skip）挂入 `run_tests.sh` AM 段。
- **§1 五维度对照**：③ 完整依赖管理——语义版本区间 ✅ / 传递依赖+仲裁 ✅ / `dial.lock` ✅ /
  search ✅。**五维度全部达成**，dialpkg 在「让人放弃其它 C 包管理器」的产品定义上闭环。
- **设计边界（不含糊）**：① 不回溯——semver 区间单调，贪心选最高 + 交集判冲突即可收敛，
  冲突明确报错而非静默换版（Fail-Fast 铁律）；② 语义版本选「装哪个版本」，`abi` 把关「能否链接」
  （README §25.3），两者职责分离不冲突；③ 零新依赖（手写版本逻辑）、零脚本、无中心服务。
- 零新依赖：复用 `curl`/`sha2`/`serde_json`；新模块 `src/version.rs`/`src/resolve.rs`；版本 1.10.0、不打 tag。

### P4.3 —— ⬜ 未启动

- 离线 / 镜像与增量（`--offline` 下载层、镜像源配置、断点续传、增量更新评估）：按批次独立推进，
  每期对照 §1 勾验。注意 P4.1/P4.2 的 `--offline` 仅指「索引/依赖解析只读缓存」，与 P4.3 的
  「包下载离线/镜像」是不同层。

> 路线图文档初版（2026-11-19）确立战略定位与分期；P4.0/P4.1/P4.2 代码均于 2026-11-20 落地。
