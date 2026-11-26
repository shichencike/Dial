# 更新日志 / Changelog

本文件记录 Dial 语言编译器 `dialc` 的每个发布版本的相对变更。

> **版本号规则（强制）**
>
> 1. **最多两位小数**的单一十进制数：`v0.1`、`v0.2`、…、`v0.9`、`v0.91`、`v0.92`……
>    不再使用语义化版本的「主.次.修订」三段式。
>    **唯一例外是第三位的补丁号**（如 `v0.98.1`）：它是「两位小数版本 + 一位修复补丁位」，
>    而非「第三位连续小数」；仅用于**修复发布**（只修缺陷、不加新特性），
>    适用条件见 [README §23](README.md#23-版本号规则) 第 6 条。
> 2. **版本标题一律带 `v` 前缀、不写补丁位**：写作 `[v0.91]`，**不是** `[0.91.0]`。
>    （源码 `Cargo.toml` 的 `version` 字段受 Cargo 限制仍写作 `0.91.0`，那是存储格式，
>    文档与官网一律显示为 `v0.91`。）
>    **带补丁位的版本例外**：补丁发布写作 `[v0.98.1]`，补丁位必须显示——
>    否则读者无从判断自己装的是不是修复过的版本。
> 3. **版本号必须严格按时间递增，绝不许倒流**：后发版本的日期不得早于先前版本。
>
> 详见 [README §23](README.md#23-版本号规则)。

---

## [v1.22] — 2026-11-25

### Added（M16 asm 后端动态链接库 `--shared`）
- **`--shared` 产物形态**（`dialc build foo.dial --shared -o libfoo.so`）：自研 asm
  后端首次可产出 ELF 共享对象 / macOS dylib / MinGW dll，供 `dlopen`/`dlsym` 与
  链接器 `-l` 使用。只有 `export fn` 成为导出符号（私有 `fn` 不导出）。与
  `--single-file` / `--tiny` Fail-Fast 互斥；可与 `-O0..3` 叠加。
- **C 运行时 PIC 重编**：`--shared` 绕过非 PIC 的预编译 `libdial_std.a`，强制 C
  源码加 `-fPIC` 重编并存到独立缓存分区（key 含 `-fPIC`）。根因：默认归档有
  2714 处 `R_X86_64_32S` 绝对重定位，`dlopen` 到任意基址后 ARC 元数据错位 →
  实测含字符串拼接的导出函数 double free；PIC 重编后无 TEXTREL、500/1000 次
  ARC 调用稳定。
- **`dlopen` 自动初始化**：链接期注入带 `.init_array` constructor 的小对象，库加载
  时自动调 `dial_env_init(0, 0)`。否则 `g_rng_state=0` 使 xorshift 退化、`random()`
  恒返回 0（实测）。constructor 用 0 参数：种子由 `time(NULL)` 参与保证非零，argv
  快照为空（库本无 argv）。
- 输出提示文案区分「动态链接库」/「可执行文件」；usage 增补 `--shared`。
- R35 门禁：ELF 共享对象类型 + 导出符号 + 无 TEXTREL + `dlopen` 调用（含 1000
  次 ARC 循环 + random 非退化）+ 冲突矩阵；默认可执行路径零回归。

### Added（M11~M15 对齐 C 工具链 O2/O3 优化）
- **M11 通用函数内联**（`pass::inline_calls`，O2+ x86-64）：对标 gcc/clang `-O2`
  inlining。Private 小函数（≤96 指令 / ≤16 块 / 非递归）体复制到调用点，vreg 与
  block id 全量重映射、入口补回写参 `Store`（补 codegen prologue 的隐式写参）、
  ARC retain/release 原样保留、`Return` 改 φ 回传到 continuation。不动点 3 轮。
  排除 `-g`/PGO/插桩（DWARF 行号与 profile idx 对齐）；Line/DebugVar 不复制。
- **M12 通用 LICM**（`pass::licm`，O2+ x86-64）：循环不变纯计算外提到 preheader。
  标准自然循环识别（入口块被卷入则整块跳过）、只外提定义块 ≥ preheader 的指令
  （保证向前搬）、`Load` 仅在 ptr 槽循环内未被 Store/ARC 归零/取别名/调用逃逸时
  外提、`Div`/`Mod` 不外提（trap 位置）、不动点 8 轮 + DCE。
- **M13 保守 SCCP**（`pass::sccp`，O2+ x86-64）：格 `Option<Imm>` 沿 CFG/块参数 φ
  不动点传播，只信任立即数与纯整数/比较/转换折叠结果；把 `cond` 为整数常量的
  `Branch` 改为 `Jump`，不删除任何块/Call/Store/ARC（零副作用变更）。
- **M14 死函数消除**（`pass::eliminate_dead_functions`，O1+ x86-64）：Private 且
  全程序零引用（Call/FuncAddr 均计引用）的函数不动点删除；main / 导出 / 函数指针
  引用保留。
- **M15 保守循环展开**（`pass::unroll_loops`，O3 x86-64）：精确匹配标准 for 5 块
  骨架（上界常量 2..8、iv 0 步长 1、无 φ、body 仅纯整数栈指令），复制 N 份
  body+latch 串成链、末份跳 exit。运行时上界 / 上界过大 / 含副作用一律放弃。
- 调试设施：`DIALC_DUMP_LICM` / `DIALC_DUMP_SCCP` / `DIALC_DUMP_UNROLL` 打印各
  pass 生效计数。
- 门禁 R30~R34：O0/O1/O2/O3 逐字一致 + asm/LLVM 双后端逐字一致 + 各 pass 生效
  证据 + 反例零误拒；GRA 驻留 / x86 移位形态门禁改用 `export fn` 测试目标以适配
  内联+死函数消除（非真实回归）。全量回归零回归、单测 41 项、CVE 8/8。

### Added（M10 字符串拼接常量折叠：对齐 gcc -O2）
- **L1 字面量拼接折叠**（`pass::fold_string_concat`，O1+）：编译期把静态串之间的
  `dial_string_concat` 合并成单个静态对象，不动点迭代支持 `"a"+"b"+"c"` 链式；
  内容逐字节等价、静态串 immortal 故 ARC no-op、AArch64 亦安全。
- **L2 累加循环折叠**（`pass::fold_string_accum`，O2+ x86-64）：严格识别
  `x=S0; for i in 0..N { x=x+C }` 且退出块仅用 `length/get` + 纯算术的骨架，
  符号执行算出 `final=S0+C·N` 与 return 常量，把整函数折叠成单块 `return const`
  （对标 gcc -O2 对 bench.b20 的折叠：b20 从含 4000 次堆拼接变成一条 `mov $124272`）。
  零误拒护栏：前缀追加 / 循环体副作用 / 非静态追加串 / iv 退出块再用 / 超长串
  一律放弃；采样经 R29 门禁 + 全量 1112 项回归验证。
- 调试设施：`DIALC_DUMP_ASM_IR` / `DIALC_DUMP_ASM_IR_POST` 环境变量打印优化前/后 IR。
- 说明：LLVM 后端从 AST 直接生成、不经此 pass，但输出与折叠后的 asm **逐字一致**
  （折叠是等价变换）；`bench/bench.dial` 的 b20 校验和 124272 与 O0 完全相同。

### Added（零标注借用检查器 + 门禁盲区修复）

- **借用检查器（NLL、零生命周期标注）**：语义分析阶段新增纯分析 pass，基于内部数据流
  计算借用区域，检测「借用指针（`s.as_c_str()` / `b.as_ptr()`）仍在使用、来源却被
  `move_to` / `drop` / 重赋值 / `spawn` / `own` 接管」的悬垂 use-after-free，编译期
  Fail-Fast，错误码 `dialc.borrow.dangling`。NLL：借用到指针最后一次使用为止，此后
  释放来源合法；`&x` 栈值借用不检查（零误拒）。用户**无需写 `'a` 等任何生命周期标注**。
  纯分析不改 IR / codegen，双后端天然一致。见 README §31.7、R28 门禁。
- **本地门禁补齐 cve_check**：v1.21 发布事故（cve_check 只在 CI 跑、本地全绿却 tag CI
  失败）的流程盲区修复——`tests/run_tests.sh` 按 CI 同口径纳入 CVE 暴力检测 AP3 段。

### Fixed（结构化错误码 CLI 贯通）

- **缺陷 #5 修复**：此前 frontend 把 `DialcError` 降级成 `String`，CLI 只能从消息文本
  反解错误码，措辞一改工具就废。现 frontend 返回结构化 `FrontendError`（保留 `code`），
  `diagnostic::report_with_code` 优先用结构化码；CLI 人读 / JSON 与 LSP 三处对同一错误
  显示一致的 `dialc.borrow.dangling` 等码。

验证：全量回归 1112 项通过（原 1099 + 新增），R28 门禁 2 项全绿，CVE 8/8，零误拒。

### 性能基准（v1.22 发布前同机实测刷新）

本版发布前用现有四语言套件重跑全部 28 项微基准（`bench/data.json` 快照，
`docx/bench.html` 同步重生成，`check_bench_data` 全绿）；统一外部墙钟、校准重复数、
扣 noop 基线、112 项校验和逐项一致。工具链：gcc 13.3 `-O2` / Go 1.21.4 /
CPython 3.11 / **dialc 1.22.0 `--fast`**。

- **速度几何平均（C=1.00×，越小越快）**：C 1.0000× / **Dial 1.0244×** /
  Go 1.4967× / Python 34.44×。Dial 与 gcc -O2 基本持平，明显快于 Go。
  28 项纯计算耗时之和：C 42.49ms / Dial 48.51ms / Go 50.60ms。
- **Dial 最快 5 项**：b01 整数累加、b09 二叉递归 fib、b10 浅递归、b11 递归树、
  b20 字符串循环拼接（M10 折叠生效，对标 gcc -O2）。
- **六维工程指标**：Dial 启动 0.532ms（四语言最快）、b17 峰值 RSS 1444KB（最低）、
  产物 183.83KB（Go 1941.72KB）；Go 编译最快 69.5ms，Python 编译/体积占优。
- **已知短板**：b17 矩阵乘 7.01×、b12 互递归 3.71×、b23 popcount 3.32×（asm
  后端未做向量化/更激进的循环优化），列为后续方向，不影响本版功能交付。

### v1.22 发布后性能增量（不计入本版发布数据）

- **基准配置修正 + M16 popcount SWAR**：`bench_harness` 的 Dial 档位显式 `-O3`
  （原先 `--fast` 只切安全级别、实际仍 O2，导致 O3 pass 全程未生效）；新增 popcount
  循环 SWAR 位并行折叠（负数路径保留原循环）。b12 互递归 3.71×→0.53×，
  b23 popcount 3.33×→0.92×；28 项 O0~O3 校验和逐项一致。
- **M9-gra-linear 线性扫描寄存器分配（治根 b17）**：asm 后端含-call 函数的 GRA
  从静态 top-N 绑定改为基于 live interval 的线性扫描——块内 live range 不重叠的
  临时值复用同一 callee-saved 寄存器，配合动态驻留段 + 基于当前 resident 的实时写回，
  从架构层解决 b17 内层 15+ 个等权短命临时值竞争 6 个名额的问题。b17 0.821×
  （较基线快 18%），11 项代表任务几何平均 0.866×（快 13.4%）；b14（cache-bound
  随机访存）1.164× 小幅退化，为已知权衡。28 项校验和全绿、A 组 65/65 正向 golden
  零回归；`DIALC_GRA_LEGACY=1` 可回滚。护栏：非 x86-64 / -g / O0-O1 / 内联汇编 /
  longjmp(EnvInit) 函数均不启用。证据链见
  [rationale/unreleased-dev.md](rationale/unreleased-dev.md) M9-gra-linear 条。

## [v1.21] — 2026-11-24

> 本版是「预处理设施大补齐」的交付版本：字段级 ARC、调试期裁剪、专门的预处理阶段、
> 条件编译 `@cfg` 与 AST 宏、`#` 文本级预处理指令五批改动合并发布。按 README §23
> 的「小功能 / 新语法 `+0.01`」标准，从 `v1.20` 递增到 `v1.21`（**不按大版本 `+0.1`
> 跳档**）；发布标签 **`v1.21`**，资产名按版本号推导（`dialc-v1.21-*.zip` /
> `dial-1.21.0.vsix`）。端到端全量回归 1099 项全绿、单测 41 项全绿。

### Added（`#` 预处理指令：文本级预处理层）

- 新增独立模块 `src/directives.rs`：编译流水线**最前端**、词法之前的文本级预处理层，
  与 §39 的 `@cfg` / `macro`（词法后的 AST 级 pass）并存互补、职责不重叠。首批三类指令：
  - **宏 `#define` / `#undef`**：对象式（token 级替换，不误伤字符串字面量内部）与函数式
    （形参替换 + 嵌套调用）。函数式须 `NAME(` 紧邻；自引用按 C 语义不再展开，深度上限 64
    防间接递归。命令行 `-D NAME` / `-D NAME=value`（别名 `--define`，可重复）注入初始宏表。
  - **条件编译 `#if` / `#ifdef` / `#ifndef` / `#elif` / `#else` / `#endif`**：支持 `defined()`、
    C 运算符优先级、整数（`0x/0b/0o` + `_`）、字符串/字符字面量；未定义标识符为 0。
    假分支整块裁剪（含其中的 `#error` / 未知指令），**裁掉的行以空行占位保持行号**，诊断不漂移。
  - **诊断 `#error` / `#pragma`**：活跃分支 `#error` 立即 Fail-Fast（带路径行号）；
    `#pragma once` 由模块去重天然满足、接受为空操作，未知 pragma 记日志忽略。
- Fail-Fast：未闭合 `#if`、孤立 `#endif`/`#else`/`#elif`、重复 `#else`、`#define` 缺名/非法形参、
  宏重定义内容不同、宏实参个数不符、`#if` 表达式错误/除零、未知指令——一律当场报错。
- 挂接点：`preprocess::collect_sources` 中 lex 前跑文本 pass、lex 后做 token 宏展开（在
  `import_specs` 之前，使 `-D`/`#define` 能改写 import 字符串）；`PreprocessOptions` 加
  `defines` 字段；`main` 加 `-D/--define` 解析与 `mod directives`。
- 验证：单测 40 项全绿（新增 16 项覆盖对象式/函数式/嵌套条件/defined/undef/-D/Fail-Fast 等）；
  R27 门禁 4 组（双后端逐字一致 + `-D` 注入 + 五类 Fail-Fast + 假分支忽略），双后端一致。
  文档见 README §41。

### Added（条件编译 @cfg 与 AST 宏）

- **条件编译 `@cfg(cond)`**：复用 `@` 注解前缀（零新增词法关键字），可作用于顶层
  `fn` / `struct` 与语句/块。条件语法：裸名布尔（`debug` / `my_flag`）、键值
  （`target_os = "linux"` / `feature = "x"`）、`!cond`、`all(...)` / `any(...)`。
  - 内置维度：`debug`（随构建模式）、`target_os` / `target_arch`（宿主 triple，
    遵循 `DIAL_TARGET` 覆盖）；自定义维度来自 CLI `--cfg key[=value]`（可重复，
    无 `=` 视为布尔真）。
  - 在预处理阶段求值：假则整块/整项删除（**不参与类型检查**），真则清除标记；
    release 产物零被裁剪分支的字符串/符号残留；调用被裁剪的函数如期报未定义（Fail-Fast）。
- **AST 宏 `macro name($a, ...) { ... }` + `name(args)`**：普通调用语法，预处理阶段做
  AST 展开（非文本替换）。`$a` 词法为 `Ident("$a")`，展开时按 `$` 前缀把实参表达式
  替换进宏体的所有语句上下文（含嵌套表达式、子块）。支持宏体调用其它宏（递归展开，
  深度上限 64 防自递归），宏名遮蔽同名函数，参数个数不符/重名宏当场 Fail-Fast。
  - 两端共用同一份展开后的 AST，`dialc test` / LSP 亦看到展开结果。
- 实现落点：`lexer`（`$name` → Ident）、`ast`（`CfgCond` / `MacroDef` + cfg/macros 字段）、
  `parser`（注解/宏定义解析，均为上下文关键字不新增保留字）、`preprocess`（eval_cfg /
  cfg_program + 宏展开两道 pass，位于 debug 裁剪之前）、`main`（`--cfg` 解析与透传）。
- 验证：R26 门禁 7 项全绿（cfg 各维度 + `--cfg` + release 零残留 + 宏参数/嵌套/
  遮蔽 + 递归宏 Fail-Fast + 孤立 @cfg Fail-Fast + cfg 假分支跳过检查 + 宏体 cfg 求值，
  双后端逐字一致）；单测 24 项全绿（新增 cfg 求值、`$` 词法）。

### Added（专门的预处理阶段）

- 新增独立模块 `src/preprocess.rs`：编译流水线中**专门且唯一**的前置处理层，位于 token
  收集与语义分析 `analyze` 之间。收拢源码收集（三级 import 解析 / 循环检测 / 去重）、
  逐文件 parse + AST 合并（冲突检测）、语言层 extern/link 结算（魔数校验 / rpath）、
  以及 v1.22 的 debug 裁剪到一处。
- 阶段入口 `preprocess::run(src, log, &PreprocessOptions)` 返回 `PreprocessOutput`
  （`program` = 裁剪后喂给 analyze 的 AST；`raw_program` = 裁剪前完整 AST 供 LSP/test）。
- 扩展点：`PreprocessOptions` 是预处理行为的唯一配置入口，后续加条件编译（`cfg`）、
  源码宏、特征开关只需加字段 + 一道 pass，build/run/check/test 调用方零改动。
- `frontend()` 瘦身为「预处理 → 语义分析」两步编排；跨模块辅助函数以 `pub(crate) use`
  保留原 crate 根路径，`toolchain.rs` / `lsp.rs` 零改动。
- 行为完全不变的架构重构；新增 4 项预处理纯函数单元测试（共 22 项单元测试全绿），
  全量回归通过。

### Added（调试输出与调试代码块：编译期裁剪）

- **`debug_print(...)` 调试输出**：`print` 的调试期别名，输出目标/参数规则/variadic 行为完全一致
  （stdout、末尾换行）。debug 构建正常输出；release 构建在语义分析前整句删除。
- **`debug { ... }` 调试代码块**：复用裸块作用域（块内变量块外不可见），debug 构建正常执行
  （含副作用）；release 构建在语义分析前整块删除，块内语句（含副作用与资源分配）一概不执行。
- **编译期裁剪 pass**（`semantic::strip_debug_program`）：仅在 release 构建、且位于 `analyze`
  **之前**运行——因此 release ① 不检查调试代码类型错误（用户裁决）② 产物零调试字符串/调用残留
  ③ 零运行时开销。`raw_program` 始终保留完整 AST，LSP / `dialc test` 照常看到全部调试代码。
- **构建模式映射**：默认 `--safety debug` 保留调试代码；`--release` / `--fast` /
  `--safety safe|fast` 均裁剪。`dialc check` 作为纯检查工具始终保留并校验调试代码。
- 新增 R25 门禁：双后端 debug/release 功能逐字一致、副作用开关、release 产物零字符串残留、
  release 跳过类型检查（10 项）。

### Added（字段级 ARC：含句柄字段 struct 进容器）

- **字段级 ARC 落地**：含 `string` / `array` / `map` 等句柄字段的 struct 现在可作为数组元素
  与 map 值（此前语义层一律拒绝）。装箱为带 desc 表的 ARC box，布局 `[desc][rc][payload]`
  （rc 紧邻 payload 以兼容通用 `dial_rc_retain(p)` 读 `p[-1]`；desc 在 `p[-2]`），
  容器释放时按 desc 逐字段级联 release。desc 表 `[nfields][(kind,offset)×n]` 由两端
  静态发射（LLVM 全局常量 / asm `.rodata .Ldarc_<Struct>`）。
  - 新增 `dial_struct_box_arc` / `dial_struct_box_arc_release`（`runtime/dial_rt_mem.c`）。
  - 容器元素 kind 扩展：数组 `DIAL_ELEM_BOXARC=7`、map `KV_STRUCT_ARC=7`；`STDLIB_ABI` 6→7。
  - 语义层放开数组元素 / map 值 / `get_or` / 类型良构检查对可装箱 struct 的限制。
  - B6 负向断言翻转为正向；新增 R24 门禁（双后端功能逐字一致 + 20 万次混合压测 +
    峰值 RSS < 8MB）。

### Fixed（字段级 ARC 收口：双后端级联结算对齐）

- **LLVM `get_or` 命中 double-free（heap-use-after-free）**：命中时 C 侧 `dial_kvmap_get_or`
  对 ARC struct 不 retain，返回的是表内 box 的字段借用；LLVM 变量声明未对解箱副本字段 +1，
  作用域退出提前释放字段 → map 销毁二次释放（exit=134）。修复：解箱副本按 `desc` 递归
  retain 字段后再归还 default box（对齐 `get` 的 borrowed 口径）。
- **LLVM string `+` 操作数泄漏**：`dial_string_concat(a,b)` 只读不消费操作数，LLVM 端缺失
  对 owned 临时（如 `str(i)`、嵌套拼接）的结算，每轮泄漏一份；补齐与 asm
  `release_string_temp` 同口径的操作数释放。修复后 10 万~40 万次压测 RSS 恒定 ~1.5MB。
- **asm `KvMap` 走通用 `dial_rc_release` 不逐条释放值装箱**：新增 `ArcKind::KvMap`，
  retain/release 分派到 `dial_kvmap_retain` / `dial_kvmap_release`，与 LLVM `release_value`
  对齐。
- **asm `get_or` 命中字段 double-release**：与 LLVM 同源；补 `StructRetain` 对解箱副本字段 +1。
  （此前被 asm 漏逐条释放 box 掩盖为「仅泄漏」，修 KvMap release 后暴露，已一并修复。）
- 全部经 ASAN（clang `-fsanitize=address`，UAF + leak 双检）与双后端 RSS 规模无关性验证。

### Fixed（asm 后端 SIMD 功能对等：补齐相对 LLVM 的 3 项缺口）

- **以双后端实测核实并修复 asm 后端相对 LLVM 的 3 项功能缺口**（此前 ROADMAP §5 护栏
  称「LLVM 能编译而 asm 拒绝的构造为零」，以 3 个最小用例证伪：LLVM 全部编译运行正确，
  asm 全部 Fail-Fast 拒绝）：
  - **A · SIMD 变量 lane 索引 `v[i]`**（`lower.rs:1832` 旧版仅接受整数字面量）：字面量走
    原快速路径（既有 `.s` 逐字节零回归）；运行期索引新增动态路径——lower 索引 → 无符号
    `ICmp Lt(idx, N)` 边界检查（对齐 LLVM `emit_bounds_check` 的 ULT 口径，负数位模式
    巨大自然被拦截）→ 越界调 `dial_bounds_fail` 当场 trap → 正常则 `idx*8` 动态 `PtrAdd`
    + `Load`。双后端合法索引逐值一致、越界都非零终止（LLVM SIGTRAP / asm SIGILL + 诊断）。
  - **B · bitcast 到 simd 类型**（`lower.rs:3665` 旧版直接拒绝）：位宽口径改为
    `N × 元素原生宽度 × 8`（对齐 LLVM `<N x T>` 的 `get_abi_size×8`，与布局内建同源）；
    simd 值是栈段指针，simd↔simd 同为 `Ptr` 走恒等零拷贝返回，跨元素位重解释与 LLVM
    逐值一致（`simd<int,4>→simd<float,4>` 输出 `0.000000`、→`simd<byte,32>` 输出 `1`）。
    加类界限「simd 只能 simd↔simd」，与 LLVM 把向量归为聚合、拒绝向量↔整数/指针同口径。
  - **C · bitcast 到 array 类型**（`lower.rs:3666` 旧版直接拒绝）：array 是 8 字节不透明
    句柄（LLVM 侧擦除为 i8ptr），位宽按 64 计；array↔array（同 `Handle`）走恒等返回，
    array↔整数/指针走既有位重解释路径，与 LLVM `ptrtoint`/`inttoptr` 语义一致。
  - 新增 R23 门禁：综合正向（变量索引 + bitcast simd/array + 跨元素位重解释）双后端逐值
    一致、越界 Fail-Fast 双后端非零终止、O0-O3 四级别输出一致。AArch64 发射经检视正确
    （本地交叉汇编受工具链基线限制，与既有 golden 同口径）。

### Fixed（P0：ARC 泄漏，两个独立根因）

- **以资深程序员视角压测发现并修复两类 ARC 泄漏**（审查报告初判「异常路径泄漏、LLVM
  因 throw 漏更多、语言层通病」，经精细对照实验证明归因有误，真实是两个独立 bug）：
  - **A · asm 后端 `release_locals_upto` 全局 moved 污染**：出口（return/throw/break/
    continue/goto）释放局部时把编译期 `Local.moved=true`，该标记被条件分支的
    else→merge **正常落穿路径**继承，令其 `leave_scope`/`release_scope` 误以为已释放而
    跳过 → 每个条件出口泄漏一份 try/块作用域 ARC 局部。修复：出口释放不再写 moved；
    `leave_scope` 在已终止块（出口路径）跳过 `release_scope`，正常落穿路径照常释放，
    两条 CFG 路径互斥、无需全局标记。实测条件 throw 5000 次 6.3MB→2.0MB、条件 return
    6.5MB→2.0MB，无 double-free。
  - **B · LLVM 后端字符串字面量堆分配泄漏**：`Expr::StringLit` 每处求值都
    `dial_string_new`（malloc+strlen+memcpy，rc=1），而 `StringLit` 不在
    `yields_borrowed_ref`、concat 等又不释放右操作数 → 每个字面量每次求值泄漏一份
    （`s = s + "x"` 每轮漏一个 "x"）。修复：新增 `immortal_string_lit`，生成
    `[rc=INT64_MIN][len][data][\0]` 的 rodata 全局常量、句柄指向 +8 len 字段，与 asm
    后端 `.Ldstr` 完全同构（zero alloc / zero release）。实测百万次拼接 36MB→3.8MB。
  - 新增 R22 门禁：双后端压测用例 + 峰值 RSS 断言（修复版 ~2MB，泄漏版 >20MB），
    无 GNU time 平台退化为功能冒烟。双后端字符串语义（比较/map 键/数组 COW/插值/
    方法/传参返回）逐项一致。

### Added（真实 GPU 合成后端）

- **真实 GPU 合成**：补齐 §41 四项缺口中最后一项。用户裁决「继续」（最小破例 +
  Linux 真合成、其余平台降级）。只破例「运行时 dlopen EGL/GLES 做离屏合成」一条，
  其余宪法（零链接期符号、无反向回调、逐像素 CRC 确定、旧 ABI 冻结）原样保留。
  - C 层 `packages/gui/src/dial_gui_platform.c` 追加 6 个 `dial_gui_gpu_real_*` API
    （`available` / `backend` / `caps` / `composite` / `last_exact` / `degrade_reason`），
    **旧 9 个 `dial_gui_gpu_*` 行为一字不动**（仍被 AA 组 30 条钉值断言覆盖）。
  - Linux EGL 1.5 `surfaceless` + GLES2 恒等 shader：framebuffer 上传纹理 → 离屏 FBO →
    `glReadPixels` 读回逐字节校验，**绝不写源像素**（CRC 不漂移）；`CLAMP_TO_EDGE` +
    `NEAREST` 支持 NPOT 任意尺寸；headless 下靠 Mesa 软件渲染可跑通。
  - 软降级：`dlopen`/初始化/合成不精确一律返 `-2`（不写错误槽），缺库不导致加载失败；
    `DIAL_GUI_GPU_FORCE_DEGRADE=1` 可强制降级路径；Windows/macOS 条件编译恒 -2（未实机）。
  - 三平台 `.a` 重建，Windows `.a` 零 `dlopen`/`dlsym` 依赖；契约审计 344 → **350 个签名**
    （343 OO + `app_new` + 6 第19层），`abi=6`/`version=1.10.0` 不变。
  - 门禁 R21：能力自适应双后端用例（有软件 GPU 验证逐字节精确 + 源 CRC 不变，无则验证
    降级），强制降级双后端用例；README 新增 §38.24.7、§41 未实现清单同步移除本项。

### Added（C → Dial 反向回调）

- **C → Dial 反向回调**：补齐 §41 长期标注的「C → Dial 反向回调」缺口，
  **推翻 GUI 层「绝不反向回调」旧宪法**（用户明确裁决），但保留内存底线：
  仍无闭包（具名函数 + 显式 `rawptr<void>` state）、回调参数/结果限标量、
  **回调线程绝不触碰非原子 ARC 对象**（复杂数据走 `channel<T>`）。
  - C 运行时 `stdlib/dial_callback.{h,c}`：mutex 保护的注册表
    `id → {thunk, state, result, done}`；`dial_callback_register` /
    `dial_callback_fire_sync` / `dial_callback_thread_fire`（平台线程 sleep 后主动回调）/
    `dial_callback_join` / `dial_callback_result` / `dial_callback_done` /
    `dial_callback_wait` / `dial_callback_free` / `dial_callback_count`。
  - 类型 `callback`（不透明句柄）+ 内建 `callback` / `callback_fire` /
    `callback_thread` / `callback_result` / `callback_done` / `callback_wait` /
    `callback_free` / `callback_count`。
  - 回调入口签名强制 `fn(rawptr<void>, int): int`；静态 thunk `__dial_cb_thunk_<fn>`
    经 argcell 三槽（payload / state / result 回写）转发，双后端同源、零 ABI 分叉。
  - state 真实透传：入口可 `ptr_cast` 后读写其裸内存（R20 正向用例 100+7=107）。
  - Fail-Fast：入口签名不符 / state 非 `rawptr<void>` 编译期拒绝；未注册 id fire 当场终止。
  - 门禁 R20：同步 / 平台线程 / state 透传双后端一致 + 4 项负向拒绝，`DIALC_STDLIB_DIR`
    强制源码回退编入新 C 运行时。README 新增 §32.7、两处「未实现」清单同步更新。

### Added（协作式线程池 / 协程 / async-await）

- **线程池 / 有栈协程 / async-await**：补齐 §41 长期标注的「无线程池 / 无异步」缺口，
  实现「够用即可」的协作式并发。**严格延续三条宪法铁律**：无闭包（入口一律具名函数 +
  显式 argcell）、无用户可见可变全局状态（句柄显式 new→use→join/close/free）、无用户可见
  原子类型（内部同步用 pthread mutex+cond，C 私有）。
  - C 运行时 `stdlib/dial_async.{h,c}`：POSIX `<ucontext.h>` / Windows Fiber 有栈协程
    （yield 可在任意调用深度发生，局部变量与 ARC 引用随栈帧保留）+ 有界线程池（固定 worker +
    任务队列）+ 单线程协作式 async executor（round-robin resume，await 即让出）。
  - 类型：`threadpool` / `coroutine` / `task`（不透明句柄，显式管理，不受 ARC）。
  - 线程池内建：`threadpool(n)` / `pool_submit(pool, fn, args...) → task` /
    `task_wait` / `task_result` / `task_free` / `pool_join` / `pool_close`。
  - 协程内建：`coroutine(fn, args...)` / `co_resume → bool` / `co_yield` /
    `co_status` / `co_free`。
  - async 语法：`async fn` 声明 + `await task` 表达式（结果恒为 int，复杂结果走 channel<T>，
    符合 CSP 宪法）。`spawn_async` / `run_all` 驱动协作调度。
  - 双后端同源：asm 与 LLVM 均复用 `spawn` 的 argcell + 静态 `__dial_task_thunk_<fn>`
    机制（slot[0]=result，slot[1..]=形参），零 ABI 分叉。顺带修复 `spawn` 无参入口
    thunk 的 rdi 丢失隐患（params 恒为单个 `void* cell`）。
  - Fail-Fast：await 只能在 async fn 内、co_yield 只能在 async fn 内、任务入口返回类型
    静态校验（pool/spawn 入口须 int、coroutine 入口须 void）、无 struct 形参。
  回归见 `tests/run_tests.sh` R19 段（双后端一致 + 4 项负向）；C 运行时独立 smoke 见
  `tools/` 开发期验证。

### Added
- **显式独占所有权 `own var`**（开发中）：新增 `own` 关键字，`own var x: T = e;`
  把编译期自动推断的 Move 策略升级为程序员显式声明的独占转移——强制接管即转移
  （use-after-move 编译期报错）、离开作用域确定 drop、只能修饰拥有资源的类型。
  **刻意不引入借用检查器**（无 `&T` / 生命周期 / 别名分析）。语义层强制 `move_hint`
  与 `mark_moved`，asm / LLVM 两后端复用既有 Move 路径、代码生成零分叉。
  回归见 `tests/run_tests.sh` R12b 段。

### Fixed / Added（方案 C：完整 consume 体系）

- **修「假 move」——own 转移零开销承诺落空（P0）**：程序员视角对抗测试发现 asm 后端
  `need_retain = is_copy || (borrowed && !move_hint)` 中 `is_copy` 对 `Expr::Var/Field/Deref`
  恒为 true，短路了 `move_hint`，导致 own 转移与普通「最后引用 move」都仍 emit `retain`
  （实测 own 转移 retain=1，与 LLVM 普通绑定无差异）。对齐 LLVM `if !move_hint` 改为
  `need_retain = !move_hint && (is_copy || borrowed)`。实测 retain 计数：普通 move 1→0、
  own move 1→0、三变量链 2→0，release 恒为 1。**顺带修好普通变量 move 优化在 asm 侧长期失效**。
- **修「单向独占」——own 可被静默别名（P0）**：own 变量被普通 `var` 接管时源未标 moved，
  之后仍可用，own 退化为普通 var。新增完整 consume：sema `check_moved_stmt` 在 own 声明接管
  源、或 own 变量被赋值给他人时当场 `mark_moved` 源；`set_moves` 中源是 own 则强制
  `move_hint=true`（新增 `collect_own_names` 从 AST 收集 own 集合）。sema moved 与 lowering
  move_hint 两套记账严格配对，避免 double-free。实测 own→别名→返回 10 万次 RSS 稳定。
- **own 函数参数**：`fn f(own x: T)` 声明 consume 形参（AST `Func.own_params` / `FuncInfo.own_params`
  与 params 一一对应）。调用点 sema 对实参 `mark_moved`、两后端 lowering/gen_call 标 `Local.moved`
  并让被调方入口跳过 `+1 retain`（退出 `-1 release`），所有权从调用方转移到被调方。
  own 标量参数 / own 默认参数 / own struct 字段均 Fail-Fast 拒绝（后者因 Dial struct 值语义
  无法维持字段独占）。普通参数语义零回归。回归见 `tests/run_tests.sh` R12c 段（新增 13 项）。

### Added（跨工作区全量模块图，开发中）

- **LSP 跨工作区全量模块图**：补齐 §17.2 / §28.7.1 长期标注的「跨工作区全量引用图尚未实现」缺口。
  `initialize` 读 `workspaceFolders` / `rootUri` / `rootPath` 并递归扫描根目录全部 `.dial` 建索引，
  `workspace/symbol` 覆盖未打开文件；顶层函数 / struct 按**全局唯一名**跨文件解析（`Ref.global_name`
  + `SymTarget::Global`），references / rename 遍历全工作区，rename 的 `documentChanges` 每文件一条
  `TextDocumentEdit`；局部 Var / Param 仍限本文件按 `symbol_index` 精确匹配（遮蔽正确）；新增
  `workspace/didChangeWorkspaceFolders` 增删索引。**仍有边界**：递归包内 `src/` 模块索引（§38.14）
  与显式 import 依赖图可视化未实现。回归见 `tests/run_tests.sh` M 组新增 `tools/lsp_workspace_smoke.py`
  （全量索引 / 跨 2 文件 references / rename / 局部隔离 / 协议纯净），三个既有 LSP 冒烟脚本零回归。
  仅改 `src/lsp.rs` / `src/index.rs`，不动编译语义与 ABI。

## [v1.20] — 2026-11-21

> 本版是 **LLVM 可选化 + 默认后端切 asm** 的交付版本：LLVM 变为可选 oracle（`--features llvm`），
> 发布包零 `libLLVM`，默认后端改为自研 asm；发布标签 **`v1.2`**（延续 `v1.1` ↔ `v1.10` 的
> 「标签短、版本号补零」映射，见 [README §23](README.md#23-版本号规则)）。完整理由与证据链见
> [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

### Fixed

- **首次 tag CI（v1.2）暴露的 5 处「本地绿 / 干净 CI 红」发布阻塞修复**：默认后端切 asm 后，一批被本地沙箱状态（手工装过的 gui 包、`HOME=/root`）与 ELF-only 发射掩盖的既有缺陷在 CI 真实环境集中暴露。用户要求「有 bug 就得修，不慌发」，逐项修复并重打 tag：① **Linux semver 单测假失败**——`tools/verify_p4_deps.sh` 硬编码 `/workspace/dialc/Cargo.toml`，CI runner 工作目录不同致 cargo test 找不到清单，改相对路径。② **Windows 汇编失败**——asm 后端无条件发 ELF 专用 `.section .note.GNU-stack`，MinGW/COFF 报 junk，用 `cfg!(windows)` 门控跳过。③ **Windows 链接失败**——外部 C 调用发 `call sym@PLT`（ELF PLT 语法），COFF 链接器当成不同符号报 `undefined reference to dial_env_init@PLT`；新增 `plt_suffix()` helper，Windows 返回空串。**验证**：zig `x86_64-windows-gnu` 目标汇编/链接对照，带 GNU-stack/@PLT 失败、去掉后成功；Linux 构建 `.s` 逐字节不变。④ **R18 impl_gui_handle 双后端编译失败**——该用例 import "gui"，但 gui 包首次 pack/install 在其后的 AA 组，本地因手工装过 gui 而绿、干净 CI 必败；R18 前置幂等 pack+install gui，消除顺序依赖。⑤ **source 包未产出 .a**——`tools/verify_p4_pkg.sh` 四处硬编码 `/root/.dial/packages`（本地沙箱 HOME），CI 的 `$HOME=/home/runner` 找不到安装目录；改 `$HOME/.dial`。**验证**：临时 HOME（模拟干净环境）跑全量，R18 与 source 包两断言转绿；正常 HOME 全量 `run_tests.sh` EXIT=0、1023 项全绿；check_version/check_links 全绿。护栏：Linux 发射逐字节不变、LLVM 后端零改动、零新 Rust 依赖、版本 1.20.0、发布标签 v1.2。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **默认后端切 asm 后全量回归 9 项失败清零 + 自身引入的 3 项布局回归修正（FFI 布局 / struct 默认值 / GUI 悬垂指针 / IR 形态门禁）**：用户战略「LLVM 变可选 oracle、发布零 libLLVM、默认切 asm」落地后暴露一批被 llvm 掩盖的既有缺陷，按「有 bug 就得修，不慌发」逐项修复。① **FFI bool 往返 + extern struct 布局**：`field_ir(Byte)` 误映射 I64(8B) 而 C ABI 字段 byte/bool 是 1B，致 `Point` 布局 24/offsetof(flag)=16（应 16/9）、byte store 覆盖相邻 flag 字段、bool 往返反转；引入 `cabi_field_ir/size`。② **按 struct 名分两套字段口径（关键回归修正）**：初版让所有 struct 都用 C ABI（byte=1B）修好 FFI 却破坏 R8 普通 packed struct（PKT 24→10）；对齐 LLVM 双映射（`dialc_type_to_llvm` 普通 byte=8B / `cstruct_field_llvm` extern byte=1B），`IrProgram` 新增 `cstruct_names` 集合，`struct_layout`/`field_align_size` 与四处字段 load/store（新 `field_mem_ir`）按 struct 名选口径。③ **V 组 b8 结构体默认值**：asm StructLit 只写显式提供字段、省略字段留栈垃圾致崩溃；改为遍历全部定义字段，提供值优先→默认值兜底→无默认值写 0。④ **GUI list/virtual double free**：根因是 C 库 `dial_gui_list_clear`/`dial_gui_combo_clear` free label 后未置 NULL，`dispose_subtree` 全容量遍历释放悬垂指针（gdb + 三步最小复现定位，LLVM 仅因不同 malloc 模式碰巧未触发的 UB）；两个 clear 函数 free 后置 NULL，重建 gui 三平台 .a 并重打包。⑤ **三个 LLVM IR 形态门禁**：符号可见性断言口径改 `nm -g`（asm 私有链接本就正确，非掩盖 bug），volatile/向量 IR 两项显式 `--backend=llvm`。**验证**：全量 `run_tests.sh` r2 9 失败→r3 3 失败（②回归）→**r4 EXIT=0、1023 项全绿**；FFI 往返 diff 空、普通 packed struct 双后端逐字一致、extern 布局 C ABI 一致、b8 六断言全中、GUI 两用例 EXIT=0。护栏：LLVM 后端零改动、零新 Rust 依赖、版本 1.20.0、发布标签 v1.2。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **P2/P3 收口：路线图 P2 四项终态 + 原子类型宪法否决 + struct 进容器 fuzz 盲区固化（纯文档 + fuzz 生成器，零编译器产品代码）**：用户要求进入 P4 前必须先确认 P1-P3 全部完成；核查发现 P1 已达标，但 P2 有「两项已落地却路线图仍写建议纳入/需设计、原子类型未正式裁决」的文档自相矛盾，P3 有「差分 fuzz 未覆盖 struct 进容器」盲区。本批收口（不改产品代码）：① **P2 四项终态**——`select(ms)` 块级超时（提交 `1ce1061`）与 `map<string>` 值/`values()`/`get_or`（提交 `0d13bbd`、`31d4e40`）在路线图 P2 表更正为「已落地（v1.10）」并附证据；`print(str(i))` owned 临时直传只读消费方泄漏维持「两后端都缺暂不管」裁决；**原子类型/无锁结构正式宪法永久否决**——入 README §37.2 永久否决表（由「两条」扩为「三条」）+ 能力表 2149 行由「未实现」改为「永不提供」：与 §37.1 可变全局状态禁令 + CSP channel/`move_to` 零竞争并发宪法（§27）冲突，无锁正确性（ABA/内存序/安全回收）违反 §1 与 §19/§20 内存底线，底层同步走 §33 FFI 接 C11 atomics/pthread。② **P3 盲区固化**——扩展 `tools/diff_fuzz.py`：prelude 声明带默认值 `struct PT`，新增 `struct_container_ops` 生成 `PT[]` 字面量/下标读写/`set`/`push` 与 `map<string,PT>` 的 `put/get/get_or/contains/size/values/remove`（复用 golden `batch3_b6_smoke.dial` 已验证语法）；fuzz 统计新增「struct 进容器覆盖率 + 一边编不过」专项计数，**生成该构造却任一端编译失败即判门禁失败**（不再当 skip 放过）。③ 清理 `roadmap-gap-analysis.md` / `unreleased-dev.md` 中「原子待裁决/暂不展开」过时表述（加 2026-11-19 收口注记，历史原文保留）。④ 路线图验收判据 §3 更新为「五项全绿、能力上已具备代替 LLVM 条件」。**验证**：门固定 seed 区间 40000..40149 覆盖 59/150 程序，150 seed × O0/O2/O3 = 450 次全 OK、0 编译失败、0 不一致；`PT[]` 与 `map<string,PT>` 两条路径单独编译运行双端逐字节一致；差分门禁三段全绿。**复验结论**：P0/P1/P2/P3 全部收口，满足用户「P1-P3 全完成才进 P4」门槛；P4 路线图（已存在）待用户确认后按新状态校准，本批不动 P4、不打 tag、1.10.0/零依赖/零编译器产品代码变化。证据链见 [rationale/replace-llvm-roadmap.md](rationale/replace-llvm-roadmap.md)。

- **P1 平台对等：AArch64 后端补齐 DWARF 源码级调试（`-g` 从静默降级到实机可用）**：路线图 P1 剩余开放项，用户此前明确点名「调试信息都没有」。手写 DWARF v5 生成主体本与目标无关（`emit_dwarf_cu` / `.file` / `.loc` 均为目标无关 GAS 汇编），但被 x86-64 门控硬关，AArch64 `-g` 静默不产任何调试信息。仅改 `src/codegen_asm.rs`：① 新增 `dwarf_enabled(debug_info, target)` 共因函数统一 `.file` 表 / CU / `end_label` 三处门控（end_label 与 CU 必须同 gate，否则 high_pc 引用未定义标签链接失败）；② DWARF CU header 的 `.value`（x86-only 伪指令，aarch64 GNU as 报 unknown pseudo-op）改跨平台 `.2byte`（2 字节同义词，x86 二进制零变化）；③ 变量/形参 location 的 `DW_OP_breg7(rsp)=0x77` 按目标选择，AArch64 用 `DW_OP_breg31(sp)=0x8f`（DWARF for AArch64：sp=31），偏移仍为相对当前 sp 的 alloca_off（与 x86 rsp 同构、绕开 frame_base）。**实机验证**（qemu-aarch64-static + gdb-multiarch）：AArch64 `-g` 静态 ELF 含 .debug_info/.debug_line，llvm-dwarfdump 7 符号齐全 + 行表含源名 + `--verify` 无错误，生成 .s 中 breg=0x8f（5 处）无 0x77 残留，qemu gdbstub `break dbg.dial:9` 命中 + `print counter=42` + bt 显示源码行；无 `-g` 零调试符号（零开销）；qemu 运行输出 43 不变。新增门禁 `tools/verify_aarch64_dwarf.sh`（缺 qemu/交叉 gcc/gdb 自动 skip，挂入 run_tests.sh AJ 段），全量 run_tests（含 R2 x86 DWARF / AI 语义 / AJ DWARF）、x86 golden 24、差分 fuzz 450 全绿。**裁决**：同批 P1 开放项 inline_asm（按设计 Fail-Fast 拒绝）与 PGO（`pgo_active` 已仅 x86、性能冻结为回归护栏、AArch64 无法本机训练）记为「可接受降级」，仅路线图标注理由、零代码改动。**仅 AArch64 `-g` 路径变化**，x86 DWARF 与非 `-g` 路径零变化，1.10.0/零依赖变化/不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **P1 平台：AArch64 实机语义验证，抓出并修复 3 个根本性 codegen bug（语法校验盲区）**：路线图 P1 原记「AArch64 只做交叉汇编语法校验、未实机跑语义」。安装 qemu-user-static + aarch64 交叉 gcc 后实机跑 golden，**一次抓出 3 个助记符合法但语义错误的根本 bug**（全部仅改 `src/codegen_asm.rs`，x86 路径零变化）：① **LR(x30) 未保存**——x30 caller-saved，`bl` 覆盖后 `ret` 跳回最后一次调用返回地址→含调用的函数全死循环（连空 `main` 因 `dial_env_init` 也 hang）；统一给 AArch64 函数 prologue/epilogue 配对 `str/ldr x30`。② **ICmp fuse 跳物化**——`analyze_fusable` 无条件跳过 ICmp 结果物化，但 fuse 优化只实现 x86，AArch64 分支仍 `cbz cond` 读未写槽→while 条件随机死循环；AArch64 禁用 fuse。③ **PtrAdd 发射笔误**——`add o,b,b`（o=b+b）应为 `add b,b,o`，`p.offset(1)` 失效、连带 simd lane 错位；修正操作数顺序。**实机验证**：golden 22 用例（inline_asm 按设计 Fail-Fast skip）x86 LLVM 与 AArch64 asm qemu 实机输出 **22/22 逐字节一致**；x86 golden 24 零回归、差分 fuzz 450 零不一致、全量 run_tests 全绿。新增门禁 `tools/verify_aarch64_semantics.sh`（缺依赖自动 skip，挂入 run_tests.sh AI 段）。**诚实边界**：inline_asm 在 AArch64 明确 Fail-Fast（能力降级，非 bug）；PGO 在 AArch64 降级（可接受降级）。DWARF 已于同批次 P1 补齐（见上条「AArch64 后端补齐 DWARF 源码级调试」）。1.10.0/零依赖变化（qemu/交叉 gcc 仅为门禁依赖，缺则 skip）/不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **P0 正确性：修复 string 拼接 / 插值的临时操作数引用泄漏（违 §19 零泄漏底线）**：路线图原记「map string key 泄漏 ~30MB」，本轮隔离实验先证伪该归因——map 常量 string key 不泄漏；真正泄漏源是 **`src/lower.rs` 中 string concat / interp 未归还自带所有权的临时操作数**。C runtime `dial_string_concat` / `dial_string_new` / `*_to_string` 只读、不消费操作数引用，而 `"k"+str(i)`、`"v=${i}!"` 里的 `str(i)` / 插值段中间值没有 Local 持有、concat 后不 release → 每轮循环泄漏一份（~31B/轮）；实测 N=50万 17MB、N=200万 64MB 线性增长。修复：① `+` 拼接 lowering 在 concat 后对 owned、非借用、非静态串操作数插入 `RcRelease`；② 插值 `lower_interp` 把累加状态从 `Value` 改为 `(Value, owned)` 逐段追踪，中间段 / `to_string` 结果在 concat 后归还本地引用，最终 acc 交回变量作用域管理。借用引用（`yields_borrowed_ref`：变量/字段/下标/get/at）由来源继续持有、静态串（immortal，release no-op）跳过，避免悬垂与双重释放。护栏：新增 `tools/verify_string_arc_no_leak.sh`（RSS 斜率断言 + 双后端输出一致），实测 concat/interp/chain 各 20 万次 RSS 由 ~7MB 降至 **~1.4MB**；新增 golden `tests/asm_golden/string_concat_arc.dial`（借用/owned/嵌套拼接/map get/重复用/真插值/循环混合 9 类）；golden 24 用例、差分 fuzz 450（0 不一致）、全量 run_tests 全绿，无 double-free / heap assert。路线图 P0 段同步标注归因修正与收口。**仅 asm lowering 路径变化**，LLVM 后端 / AArch64 / 优化级别零改动，不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

### Added

- **P4.6 AArch64 常量移位立即数折叠（平台对等的最后一块指令级补齐）+ P4.3 下载门禁竞态修复**：延续「把 AArch64 相关补上」主线。源码盘点发现 x86-64 早在 M7-c1 就把 `[0,63]` 整数常量移位量折叠为 `shlq/sarq $n, reg`（跳过 `%cl` 装载），但同一 `shift_imm()` 判定被 target 门控只在 x86 生效，AArch64 始终发 `lslv/asrv reg,reg,reg`（必须先把移位量装进寄存器）。AArch64 立即数移位 `lsl/asr reg,reg,#n` 同样只接受 `[0,63]`、与 x86 完全同构，本批去掉 target 门控、两 ISA 共用同一折叠：① `src/codegen_asm.rs` 常量移位（`Shl`/`Shr`）在 AArch64 发射 `lsl/asr #n`，每条省 1 条装载指令；非常量移位仍走 `lslv/asrv`（未误折叠），`shift_imm` 的 `[0,63]` 闭区间保证两个 ISA 立即数范围一致、越界自动退回变址路径保留 `count&63` 语义。**性质**：这是性能维度冻结后「平台对等」的指令级补齐（LLVM 在 AArch64 同样用立即数移位），非性能竞赛；同批无 AArch64 之外的 codegen 变化。② 全量回归暴露**所有 P4.x HTTP 门禁的共性工程化 bug**（4 个脚本）：`verify_p4_offline.sh` / `verify_p4_index.sh` / `verify_p4_platform.sh` / `verify_p4_resolve.sh` 用 `( cd && python3 -m http.server ) &`，`$!` 记录的是子 shell PID，cleanup 只杀子 shell、python3 变孤儿持续占端口——系统累积到 67 个残留服务，随机端口撞上即 404/连接失败（表现为 P4.3 镜像重写/`--json` 偶发红）。统一改用 `exec` 让 python3 替换子 shell（PID 对齐，`kill` 真正回收），`verify_p4_offline.sh` 另把固定 `sleep 1` 改 curl 轮询等待端口就绪（消除就绪竞态）。修复后 4 脚本各自连跑全绿、残留进程恒 0。**门禁**：新增 `tools/verify_aarch64_shift.sh`（L1 常量折叠为 `lsl/asr #imm` / L2 边界 0/63 / L3 非常量仍变址 / L4 qemu 实机输出与 x86 逐字节一致 / L5 x86 零回归），挂 `run_tests.sh` AP5 段；P4.3 门禁修复后连续 5 次复跑全绿、残留进程恒 0。全量 `run_tests.sh` EXIT=0。护栏：x86-64 `.s` 逐字节不变、LLVM 后端零改动、零新 Rust 依赖、版本 1.10.0、不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md) §P4.6。
- **P4.5 落地：AArch64 PGO 与内联汇编（补齐「LLVM 有 asm 无」能力缺口）**：用户要求「把 AArch64 相关补上」，推翻此前按够用线把 PGO/inline asm 记为「可接受降级」的裁决——这两项是 A 类能力缺口（LLVM 在 AArch64 支持二者），补齐不违背「性能冻结」（冻结的是优化投入而非功能对等）。同为 1.10.0、零新依赖、LLVM 路径零改动。① **PGO 放开 AArch64**——`pgo_active` 去 target 门控；插桩 target-aware：counter_inc 用 `adrp+add :lo12:` 取 `.comm` 地址 + `ldr/add/str` 自增（add 不改 NZCV，scratch x9/x10/x11 在驻留开启前无活跃值，寄存器偏移无立即数上限），dump 用 `stp/ldp` 保护 x0-x17 + **LR(x30)**（AArch64 `bl` 覆盖 LR 而 x86 `call` 压栈不覆盖，且 main 可能无 lr_slot）。训练二进制 `-static` 链接 + `qemu-aarch64-static` 执行（原生 AArch64 直接跑）；**缺 qemu 当场 Fail-Fast**（不静默降级伪装成功）。② **内联汇编放开 AArch64**——去掉两处 Fail-Fast 预检；`inst_inline_asm`/`render_asm_template` 改 target-aware：操作数池 x86 `rdi/rsi/rdx/rcx/r8/r9` → AArch64 `x0-x5`、i8/i1 零扩展 `movzbq` → `uxtb`、AArch64 寄存器裸名（不加 ATT 的 `%`）、Intel 方言切换仅 x86 发射（AArch64 单一语法）。模板按目标汇编语法书写（与 LLVM 一致）。③ **门禁**：`tools/verify_aarch64_pgo.sh`（六层：magic/计数反映真实执行/冷热分段/qemu 输出一致/确定性/Fail-Fast）挂 AP3 段；`tools/verify_aarch64_inline_asm.sh`（qemu 实机逐值 identity/add7/byte/Intel + 形态断言裸名/无方言切换/uxtb）挂 AP4 段；两者缺 qemu/交叉 gcc 自动 skip。x86 inline asm 双后端零回归、x86 PGO 门禁全绿。护栏：LLVM 后端零改动、零新 Rust 依赖、版本 1.10.0、不打 tag。证据链见 [rationale/replace-llvm-roadmap.md](rationale/replace-llvm-roadmap.md) §P1。
- **P4.4 落地：依赖闭环 + 升级体验 + 工程加固（缺口盘点后补完）**：2026-11-20 源码级缺口盘点发现 P4.2 虽有「版本求解」但普通 `install <url>` 不解析依赖、deps 写进 manifest 却无人读取，「一条命令装完即用」与「完整依赖管理」未打通，本批补上断链。① **manifest 携带 deps**——`Manifest.dependencies` + `parse_manifest` 解析（逐条校验区间 Fail-Fast），与 pack 侧已有序列化对齐，旧包无此字段视为空。② **`install --with-deps`**——装完按 manifest 依赖自动 `resolve` + 递归安装，失败 Fail-Fast；默认模式只报告 declared/satisfied/missing 不阻断（保护旧包/离线/手动依赖用户）；`--index`/`--index-sha256`/`--offline` 沿用索引统一逻辑。③ **uninstall 反依赖检查**——被其它已装包依赖时拒绝破坏性卸载。④ **跨进程安装锁**——纯 std `create_new` 锁文件 + mtime 超时自愈 + nonce 防误删，装/卸共用防 TOCTOU，零新依赖且跨平台。⑤ **版本升级体验**——新增 `dialc outdated`（当前→最新）与 `dialc upgrade [包名]`（按索引最新版原子覆盖安装），semver 选版；usage/completion 同步。⑥ **search/info semver 修正**——版本比较从字符串序改语义版本，修复 `10.0.0` 被排到 `2.0.0` 后，补回归单测。**战略边界**：经源码评估，AArch64 PGO/inline asm、DWARF 内联与复合类型、Os/Oz 属性能/体验边际或两端共同缺口，在「性能够用即可」战略下如实暂缓、不阻塞代替，已在路线图标注。**门禁**：`tools/verify_p4_deps.sh` 13 项全绿，挂 `run_tests.sh` AP2 段；全量回归 EXIT=0。护栏：LLVM 后端零改动、零新 Rust 依赖、版本 1.10.0、不打 tag。证据链见 [rationale/dialpkg-roadmap.md](rationale/dialpkg-roadmap.md) §P4.4。
- **CI Windows 二进制安全加固：按版一次性自签证书 + Authenticode 签名 + 密码学擦除（crypto-shredding）**：用户指出 CI 产出的 Windows `dialc.exe` 没有证书、不安全、不符合哲学，要求给 CI 加签名流程，并否决「固定证书长期存 Secrets」方案，坚持一次一密模型。本批落地（零新 Rust 依赖、不手搓密码学、仅系统 openssl + Windows SDK signtool、不引入任何长期 Secrets）：① **一版一证书**——每个 `v*` 发布由 `tools/sign_windows_release.sh` 新生成 RSA-2048 密钥对 + 自签证书（`CN=dialc-sign-v<全版本>`、codeSigning EKU、365 天有效期），故意不加 RFC3161 时间戳使签名随证书过期失效（防证书长期可用/旧版签新版）。② **Authenticode 签名**——用该私钥对 `dialc.exe` 签名（signtool `/fd SHA256`，路径随 SDK 版本 glob 定位）。③ **数字信封 + 擦除**——另生成一次性 KEK（RSA-2048），用 `openssl cms -encrypt -aes-256-gcm`（GCM 不可用时回退 CBC）把签名私钥封成 CMS EnvelopedData（AES-256-GCM 内容 + RSA-OAEP/SHA256 包裹 CEK，即对称/非对称交替）；随后覆写删除全部明文（签名私钥/CSR/证书 PEM/PFX/KEK 私钥），KEK 被毁 ⇒ 密封密文不可解。这正防住用户点名的三件事：私钥泄露、证书无有效期/过期仍可用、低版本证书签高版本。④ **随发布物提交**——仅公钥证书 `dialc-code-sign.cer`（随 zip + 独立 Release 资产）与审计用密封密文 `dialc-sealed-v<ver>.cms`（仅 Release、不进 zip）；CI 断言禁止任何私钥/PFX/KEK 进入产物。⑤ **验签失败即拒装**——新增 `tools/verify_windows_signature.ps1`（独立验签器）并把逻辑接入 `tools/install-dial.ps1` 两处把关点（bootstrap 下载解压后、本地 release 模式）：用随包 `.cer` 逐字节比对签名证书（信任锚）+ 校验 CN 版本 == BUILD_INFO 版本（v1 签不了 v2）+ 校验当前时间 < NotAfter（过期即拒）+ OS 完整性校验（篡改即拒），任一失败 `Die` 拒绝安装。**诚实边界**：自签证书触发 SmartScreen「未知发布者」；信任锚随包分发，能防 exe 被单独篡改与版本混签，无法防整个 zip 连证书一起被替换（该情形仍由 Release HTTPS + 用户侧核对外层哈希兜底）；不宣称非对称信任链、不引入新依赖。ARCHITECTURE §5.2 的「不需要签名体系/信任根」已改写为「按版一次性自签 + 随包证书信任锚 + 密码学擦除，与分发/信任解耦不矛盾」。**门禁**：`tools/verify_windows_signing.sh` 16 项断言（证书结构/数字信封往返一致/机密性/篡改拒绝/擦除不可解/版本绑定/过期校验/资产卫生）全绿，挂 `run_tests.sh` AP 段；Authenticode 对 PE 的真实签名/验签交 CI（windows-2022 + signtool）实机验证。护栏：LLVM 后端零改动、零新 Rust 依赖、版本 1.10.0、不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **P4.3 落地：下载韧性与平台声明（下载缓存 / `--offline` / 镜像源 / 断点续传 / 安装期平台校验）**：承接 P4.2 完成包管理路线收官期（零新依赖，复用 curl/sha2/serde_json）。① **下载缓存 + `--offline`**——`install_from_url_opts` 把包缓存到 `~/.dial/cache/downloads/<sha256(url)[..16]>_<file>`（缓存键用原始 URL，换镜像不失效，内容由 SHA256 把关）；`dialc install <url> --offline` 只用缓存，缺失 Fail-Fast 给指引；联机优先复用缓存，给了 --sha256 会校验、不符重下。② **镜像源**——`DIAL_MIRROR=原前缀=镜像前缀`（多条逗号分隔）重写下载 URL，重写后仍强制 https（本机除外），不污染缓存键。③ **断点续传**——curl `-C -` 下载到 `<cache>.part`，失败保留供续传、成功原子 rename；.part 哈希已匹配则直接提交。④ **下载进度与 JSON 纯净**——TTY 显示 curl 进度条，非 TTY/`--json` 静默，提示走 stderr 不污染 stdout JSON。⑤ **平台声明（横向增强）**——`dial.toml` 顶层 `platforms = "..."`（逗号分隔 triple）→ manifest 可选字段 → **install 期校验**：host 不在白名单即拒绝（零字节落盘）+ 列出当前/支持平台 + DIAL_TARGET 指引；向后兼容（旧包无此字段不限制，仍由 `targets` 在链接期把关）；索引 schema v3 的 `IndexEntry.platforms` 供 `search`/`info` 展示（表格 + `--json`）。**增量更新评估结论：本批不做**（包体积 KB~MB 级，增量补丁元数据/重组复杂度与收益不成正比，且会引入「补丁链完整性」新安全面违背零手写安全栈，断点续传已覆盖大文件中断痛点；待有体积/带宽数据再重启）。**门禁**：`tools/verify_p4_offline.sh` 9 项（联机落盘/offline 命中/缺缓存 Fail-Fast/SHA256 不符拒绝且不缓存/镜像重写/断点续传/JSON 纯净）+ `tools/verify_p4_platform.sh` 11 项（pack 写 platforms/匹配可装/不匹配拒绝+退出码+提示/零落盘/DIAL_TARGET 覆盖/旧包兼容/索引展示/非法 Fail-Fast），均 localhost 静态 HTTP、缺 curl/python3/zig skip，挂 `run_tests.sh` AO/AN 段全绿；P4.0/P4.1/P4.2 门禁回归全绿。护栏：LLVM 后端零改动、零新依赖、版本 1.10.0、不打 tag。证据链见 [rationale/dialpkg-roadmap.md](rationale/dialpkg-roadmap.md) §7。
- **P4.2 落地：dialpkg 完整依赖管理（语义版本 / 传递解析+仲裁 / `dial.lock` / `install --locked`）**：承接 P4.1 完成包管理路线 P4.2（零新依赖，手写版本逻辑）。① **新模块 `src/version.rs`**——手写 semver 子集：`^` caret（锁最左非零位，^1.2.3→[1.2.3,2.0.0)、^0.2.3→[0.2.3,0.3.0)）、`~` tilde、`=`/裸版本精确、`*` 通配；预发布优先级（正式>预发布）；区间**交集**运算供仲裁；语法非法 Fail-Fast。② **新模块 `src/resolve.rs`**——工作队列传播传递依赖，同一包多约束求交，空集即**冲突 Fail-Fast**（打印每条约束来源路径，如 `http@1.0.0 -> json`）；选满足区间的最高版本（正式优先）；缺包 Fail-Fast；环检测终止；生成 `dial.lock`（精确 name/version/abi/url/sha256，原子写）+ `--check` 校验一致/篡改 + `install --locked` 按 lock 逐包钉 SHA256 安装。③ **索引 schema v2**——`IndexEntry` 增 `deps` 字段（包名→区间），向后兼容 v1，deps 区间解析期校验。④ **CLI/清单**——`dial.toml [dependencies]` 段解析（区间 Fail-Fast）；`dialc resolve [目录]`（`--json`/`--check`/`--index`/`--index-sha256`/`--offline`/`--lock`，与源码级 `deps` 命名区分）；`install --locked`；usage/completion 同步。**设计边界**：不回溯（semver 单调，贪心选最高+交集判冲突即收敛），语义版本选「装哪个」、`abi` 把关「能否链接」（README §25.3）职责分离。**门禁**：`tools/verify_p4_resolve.sh` 11 项（选版/传递/冲突路径/缺包/lock 生成+check+篡改/JSON/offline/按 lock 装真实包并 import 运行/单测），localhost 静态 HTTP，缺 curl/python3/zig skip，挂 `run_tests.sh` AM 段 11/11 全绿；16 个单元测试全过；P4.0/P4.1 门禁回归全绿。**§1 五维度全部达成**，dialpkg 产品定义闭环。P4.3（下载层离线/镜像/增量）未启动。零新依赖、LLVM 后端零改动、版本 1.10.0、不打 tag。证据链见 [rationale/dialpkg-roadmap.md](rationale/dialpkg-roadmap.md) §7。
- **P4.1 落地：dialpkg 静态索引与搜索（`search` / `info` / 索引 SHA256 校验，无中心服务）**：承接 P4.0 继续推进包管理路线 P4.1（零新依赖，复用 curl/sha2/serde_json）。① **新模块 `src/registry.rs`**——静态索引 JSON 解析（schema 门禁、包名去重校验、字段类型 Fail-Fast，手动解析 `serde_json::Value` 与 `parse_manifest` 同风格，零 serde derive 依赖）、下载复用 `package::download_to`（https 强制 + localhost 白名单）、本地缓存 `~/.dial/cache/index.json` 原子落盘。② **`dialc search <词>`**——子串匹配包名/描述、多版本去重取最新、按包名排序，表格 + `--json` 稳定字段。③ **`dialc info <包名>`**——精确匹配，展示元数据/版本列表/ABI/许可/平台 + 可直接复制的安装命令（条目带 sha256 时自动附 `--sha256`）。④ **信任模型（务实不含糊）**——索引靠 HTTPS + 可选 `--index-sha256` 钉死整文件哈希；**哈希不符 Fail-Fast 且绝不回退缓存**（用户钉死来源不可被静默绕过），回退缓存也校验钉的哈希；schema 错误 Fail-Fast；`--offline` 只读缓存、缺失即报错；网络失败回退缓存且来源标注准确。不宣称非对称签名（需新依赖/手写密码学，违反铁律）。⑤ **命令面**——`--index`/`DIAL_INDEX_URL`/`--index-sha256`/`--offline` 选项，usage 与 bash/zsh/fish 补全同步。**门禁**：`tools/verify_p4_index.sh` 12 项（search 匹配/去重/描述、info 元数据/版本、JSON 合法、SHA256 正确/篡改 Fail-Fast、offline 命中/缺失、schema Fail-Fast、网络回退来源、completion/help），localhost 静态 HTTP 不依赖外网，缺 curl/python3 自动 skip，挂入 `run_tests.sh` AL 段，12/12 全绿。**§1 维度③** search 已达成，语义版本/传递依赖/`dial.lock` 留待 P4.2。护栏：LLVM 后端零改动、零新依赖、版本 1.10.0、不打 tag。证据链见 [rationale/dialpkg-roadmap.md](rationale/dialpkg-roadmap.md) §7。
- **P4.0 落地：dialpkg 包管理核心能力（pack 编 C 源 / C 源码双模式 / 原子安装 / JSON / doctor 扩展）**：前置门槛「P1–P3 全部完成」已于 2026-11-19 复核满足（见 replace-llvm-roadmap §3）。本批落地包管理路线 P4.0 五项子能力（零新依赖，复用 zig/zip/sha2/serde_json）：① **pack 编 C 源（prebuilt）**——`pkgbuild.rs` 新增 `[c]` 段（sources/includes/flags/defines/mode，逗号分隔字符串列表、shell 元字符 Fail-Fast），`cross_compile_c()` 用 `zig cc -target` + `zig ar` 把 C 源交叉编译成三平台 `.a`（x86_64-linux-gnu / aarch64-unknown-linux-musl / x86_64-pc-windows-gnu），自动生成 `[targets]` 与逐文件 SHA256，用户零手填；可复现三件套 `-g0`/`-ffile-prefix-map`/`SOURCE_DATE_EPOCH=0` 保证两次 pack 产物逐字节一致。② **C 源码双模式**——prebuilt 包随包含 `c/` 源 + `native-src/` 存档供审计；source 包 `package.rs` 新增 `CInfo` manifest 字段 + `build_c_for_host()`，install 期按 `host_triple()` 就地声明式编译（无脚本/configure/CMake），缺预编译 `.a` 自动回退，工具链缺失 Fail-Fast 给安装指引。③ **原子安装 + 回滚**——`install_inner()` 改为临时目录解压 → SHA256 校验 → `rename` 原子发布，覆盖安装先备份旧目录、失败回滚，杜绝半包残留。④ **`--json` 全覆盖**——install / list / uninstall / new 补齐机器可读输出（字段稳定、jq/python 可解析），pack/doctor 原有。⑤ **doctor 扩展**——新增「包平台/ABI 一致性」检查（缺失 host target、ABI 与 STDLIB_ABI 不符警告）。**门禁**：`tools/verify_p4_pkg.sh` 10 项（三平台编译 / 可复现 / 源码随包 / pack→install→import 双后端运行 / 无残留 / 篡改拦截+旧包完好 / source 就地编译 / 缺 zig Fail-Fast / JSON / doctor），缺 zig 自动 skip，挂入 `run_tests.sh` AK 段，10/10 全绿；全量 run_tests 通过。护栏：LLVM 后端零改动、零新依赖、版本 1.10.0、不打 tag、docx 确定性。P4.1–P4.3（search/依赖管理/lockfile/镜像）未启动。证据链见 [rationale/dialpkg-roadmap.md](rationale/dialpkg-roadmap.md) §7。

- **新路线：dialpkg 打磨成通用 C 包管理标准（路线图确立，纯文档，零产品代码）**：用户决策——「再加入一个路线，打磨包还有包管理相关命令到极致好用，标准到足够让人放弃其它，用这个作为 C 包管理分发；再给 dialpkg 强化一下，可以放 C 源代码」。新建并行主线 [`rationale/dialpkg-roadmap.md`](rationale/dialpkg-roadmap.md)（与「代替 LLVM」路线并行、共享护栏、独立推进）：① **战略定位**从「Dial 专用机器码分发格式」升为「零脚本、可审计、装完即用的通用 C 包分发」，差异化卖点=脚本数 0 + 逐文件 SHA256 + `.diali` 原生 FFI + 三平台预编译；② **可度量验收标准**覆盖 5 维度（零脚本可审计 / 一条命令装完即用 / 完整依赖管理 / Dial 原生 FFI / pack 全自动）；③ **C 源码双模式**——prebuilt（pack 期交叉编译三平台 `.a`、用户无需工具链，默认）与 source（install 期按 `host_triple()` 声明式就地编译、opt-in），化解「放 C 源码」与旧铁律「用户无需工具链」冲突，就地编译**仍零脚本**；④ **目录结构**——现有 `src/`（Dial 源码只分发不编译）+ 新增 `c/`（C 源码自动编译成 `.a`）+ 新增 `native-src/`（源码存档仅供审计不编译），`pack` 自动填 manifest/哈希/多平台归档、用户零手填；⑤ **命令面分期** P4.0 核心（pack 编 C 源/ install 源模式/原子安装回滚/--json 全覆盖）→ P4.1 静态索引 search → P4.2 语义版本+传递依赖+`dial.lock` → P4.3 离线镜像/增量评估。同步改写旧铁律：README §25 开头/§25.7/§25.11、ARCHITECTURE §6 加战略转向注记（保留历史理由可追溯；「包内不执行构建脚本」「项目目录零侵入」核心约束继续保留），replace-llvm-roadmap 顶部加并行路线指引。**本轮仅路线图文档，代码下轮落地**（用户明确选择）；1.10.0/零依赖/零产品代码/不打 tag。
- **战略转向：路线图重制——性能达标收口，重心从「超越 LLVM」回归「代替 LLVM」**（纯文档，零产品代码）：用户决策——目标不是纯粹超越 LLVM，而是**代替**它；性能够用即可，运行时 ±5% 噪声下继续死优化用户感知不到。据此新建主线 [`rationale/replace-llvm-roadmap.md`](rationale/replace-llvm-roadmap.md)，宣布**性能维度正式冻结**：同配置 `--fast -O3` 背靠背 2 次几何平均 **0.5743x/0.5718x**、总净耗时 **0.9111x/0.9106x（反超 LLVM ~9%）**，远超「够用线」（几何平均 ≤1.05x、总耗时 ≤1.10x），不再作为投入方向，降级为回归护栏。新主线按「代替能力完备」重排：P0 正确性（map string key 泄漏 ~30MB，违 §19 零泄漏底线）→ P1 平台（AArch64 未实机跑语义、沙箱无 qemu）→ P2 功能对等裁决（B 类语言特性，按「LLVM 不存在」总纲独立裁决）→ P3 工程化（过时结论清理、fuzz 盲区）。写死性能优化**重启条件**（仅新 workload 使指标跌出够用线、或新增可并行语义、或 P0~P3 收口后用户主动要求），不满足一律不重启。同步更新 `roadmap-gap-analysis.md` 与 `replace-llvm-multidim-audit.md` 顶部转向声明（旧「性能落后故不能代替」裁决失效，作历史证据链保留）。1.10.0/零依赖/零产品代码变更。详见 [rationale/replace-llvm-roadmap.md](rationale/replace-llvm-roadmap.md)。
- **里程碑：线性递推循环编译期闭式求值（M9-linrec）——总净耗时首次反超 LLVM（1.07x→0.91x），几何平均 0.574x**：接班 M8-recspec  recursion 闭式，攻路线图记录的最后一类算术循环短板。诊断确认 b04（仿射 `s=(3s+7)%1e9+7`）、b07（迭代斐波那契 `(a,b)→(b,a+b)`）各 3.3ms、慢 1.3x，均为**无参 + 编译期常量上界 100 万**的线性递推，可用矩阵快速幂 O(log N) 编译期算终值；收益上界实测 11x。b08 是 Collatz（数据依赖分支）无闭式，不碰。实现（仅 `src/pass.rs` 新增 `specialize_linear_recurrences` + i128 矩阵幂 + 严格四块骨架识别；`src/asm.rs` O3+x86-64 挂接）：识别「无参 Private I64 纯函数 + 单循环 + 仿射/斐波那契递推 + 常量上界」，用 2×2 矩阵幂 O(log N) 算终值，把函数体折叠为 `return const`。**正确性核心（反证验证有效）**：不信任自造模拟，而是**克隆函数、把上界改成小值 K、用已有 e3p 解释器真实执行截断版 IR**，在 K=0/1/2/3/7/31/128/511 逐点与闭式比对，任一不符即放弃——故意注入错误系数时 b04 被正确拒绝折叠（回退原循环、校验和仍对），证明验证非摆设。护栏： Private/无参/I64、body 仅允许递推序列+iv 自增+循环内 Alloca（任何 Call/数组/额外 Store/分支即放弃）、系数初值非负立即数、i128 中间运算防溢出、结果 mod m 落 i64。**实测**（`--fast -O3` 全 28 项、背靠背 2 次稳定复现）：b04 3.557ms→0.005ms（1.300x→0.002x）、b07 3.282ms→0.005ms（1.331x→0.002x），其余在噪声内；几何平均 0.914x→**0.5743x/0.5718x**、总净耗时 asm 38.07ms vs llvm 41.78ms = **0.9111x / 0.9106x（首次反超）**、胜平负 11/7/10 与 10/9/9。**门禁**：golden 23 + 22 ReleaseFast + 新增 O3 专项段（`tests/asm_golden_o3/linear_recurrence.dial` 覆盖仿射多系数/a=1 边界/斐波那契多初值/Collatz 不误折叠/带参不误折叠）+ 差分 fuzz 450（0 不一致）+ 28 项 checksum 全一致。**诚实边界**：只对无参纯函数生效（带参递推按常量调用仍走 fold_pure_calls）；b08 Collatz 无闭式仍是最大单项短板（2.1x）；1.10.0/零依赖/AArch64/低优化级/LLVM 路径零变化、不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **里程碑：递归纯函数闭式特化（M8-recspec）——b12 从 3.30x 落后到 0.477x 反超 LLVM 2 倍，几何平均 0.98x→0.914x**：用户追问「私有后端又不是通用，为什么不更激进」。先诚实用三份只读诊断证伪了最直觉的路子：改 ABI/扩 caller-saved 寄存器池能消除的栈往返全 bench 仅 5 处（都在递归函数）、跨 call 长 live-range 槽仅 46 个 vs 583 个 ABI 强制流量，保护成本远超收益——e3at 否决仍成立，ABI 这条路不碰。真正的私有红利在**语义特化**：LLVM 作为通用后端绝不会把互递归识别成奇偶判定，我们能。诊断坐实 b12 最大短板根因是 evR/odR 互递归本质为 `n≥0 ? 1-(n&1) : 发散`，被 20000 次循环 × 平均 30 层递归吃掉。实现（仅 `src/pass.rs` + `src/asm.rs` 挂接，O3+x86-64）：复用 e3p 纯函数解释器，对递归纯整数单参函数用 129 点探针 `[0,128]` + 55 点抽样 `[129,512]` 解释执行、拟合闭式候选（常量/低位位模式/移位掩码/恒等）、逐点验证后，在函数入口插 guard：`n≥0` 走 O(1) 闭式、`n<0` 保留原递归体（负数语义原样）。只处理递归纯函数，其余零变化。**正确性护栏**：纯函数判定 + 递归判定 + 探针全域匹配 + guard 限定已验证域 + O3/x86-64 门控（AArch64/低优化级/LLVM 路径零变化）。**背靠背 A/B 各 2 次**（`--fast -O3`）：b12 **3.30x→0.477x（-86%）**、几何平均 0.9817x→**0.9141x**、递归类 0.525x→0.3247x、总耗时 1.0748x→1.0661x；逐项对比唯一变化 b12（-85.6%），b08 +7.6% 经汇编逐字节比对证实为噪声。全门禁绿：golden 23/22（含 mutual_recursion 钉死特化正确性）+ 差分 fuzz 450 + 工具链去 LLVM 22 + PGO L1-L4 + 全量 run_tests + 28 项 checksum 全一致。总裁裁更新：几何平均优势扩大到 ~8.6%，加权总耗时仍慢约 6.6%——b12 从「最大单项短板」转为「领先 LLVM 2 倍」，但 b04/b07/b08/b14 除法延迟链仍是总耗时落后主因，循环调度方向继续推进。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **加入 LLVM 式高级寄存器分配优先级（span×freq）——b05 2.098x→1.909x**：用户追问「LLVM 有高级分配逻辑，asm 有没有」——核查确认 asm 已有三种固定绑定 GRA + fGRA + ISA 微融合，但缺 LLVM Greedy 的 `priority = span × freq`：top-N 排序只用 `use 次数 × 循环深度权重`，跨块长命值会被内层单块短命值挤出 phys 寄存器。诊断坐实 b05「连乘幂内核」外层 phi 值 s/a 每轮迭代走 248/256(%rsp) 栈往返（25 处栈访问、2 个高频槽）。实现（仅 `src/codegen_asm.rs`）：新增 `compute_live_span()` 统计 vreg 作为 def/use/phi 出现的不同基本块数，`analyze_unified`（无-call 纯计算路径）的绑定排序改为 `score × span`。**背靠背逐项 A/B 纪律**：先全量加（含-call 路径一起），两次实测定位到含-call 函数退化（b21 +9.4%、b17 +6.4%、b15 +4.7%）——6 个 callee-saved 池太紧张、跨块跨度大的中转值未必是热点；据此**回退含-call 路径的 span 加权，仅在无-call 路径保留**。修正后同配置 `--fast -O3` 各 2 次（5 采样取最小）：几何平均 0.9828x→**0.9808x**、加权总耗时 1.0752x→1.0749x、b05 2.098x→**1.909x（-9%）**、b08 外溢改善（~2.04x→1.948x），含-call 退化消失；b05 栈访问 25→16、248/256 槽不再访问。全门禁绿：golden 23/22 + 差分 fuzz 450 + 工具链去 LLVM 22 + PGO L1-L4 + 全量 run_tests。**未触碰** caller-saved live-range 全重写（e3at ROI 否决）。总裁裁不变（总耗时仍慢约 7%）。护栏：1.10.0/零依赖/AArch64/LLVM 路径/safe-debug 零变化、不打 tag。证据链见 [rationale/replace-llvm-multidim-audit.md](rationale/replace-llvm-multidim-audit.md)。

- **里程碑：整体几何平均首次反超 LLVM（1.0794x→0.9793x）——dial_array_set 独占快路径内联**：数组内存类原是最大短板（b15 冒泡 2.676x、b17 矩阵乘 2.091x），受控实验坐实 `dial_array_set` 调用占冒泡内层约 80% 开销。实现（仅 `src/codegen_asm.rs::inst_call`，紧邻 get 内联）：仅 x86-64 + ReleaseFast 下，实参装 rdi/rsi/rdx（回退 call 复用），运行时检查 `refcount==1 && kind==0`（独占 int[]）→ `mov v,[a+24+i*8]` 直写返回 a，否则回退原 call 保留 COW/ARC；safe/debug 与 AArch64 原样。**正确性**：COW 共享写 2 万次压测双后端一致、原数组未污染、glibc 堆检查 0 错误；string[] 引用元素走回退正确；golden 23/22、fuzz 450。**实测**：数组类 1.556x→1.0945x（b15 2.676→1.195、b17 2.091→1.364、b16 反超 0.912），整体几何平均 1.0794x→**0.9793x（首次 <1）**，胜平负 7/5/16→9/7/12。**诚实补充**：几何平均已反超，但加权总净耗时仍 1.0728x（慢约 7%），两类指标都记录。总裁裁更新：按几何平均统计意义上追平/反超，按总耗时仍未全面代替；剩余 b12 闭式、b08/b05 算术、b22 substring、GRA 全 spill 架构级工程继续推进。护栏：1.10.0/零依赖/AArch64/LLVM/safe-debug 零变化、不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **字符串字面量静态化——b20「x=x+"y" 循环拼接」2.275x→1.084x（-52%，近乎追平 LLVM）**：延续代替 LLVM 的性能主线，攻审计「C runtime 字符串路径」架构级差距中最高 ROI 的一点。受控实验坐实循环内 `dial_string_new("y")`（malloc+strlen+memcpy）占该模式约 50% 开销。实现（仅 asm 路径 ir/lower/codegen_asm/pass）：新增 `LeaStaticString` IR，字面量在 `.rodata` 发射为静态 immortal DialString（`[rc=INT64_MIN][len][data\0]`，retain/release no-op），`Expr::StringLit` 直接取静态句柄、省去运行时构造；字符串不可变 + 内容比较，语义等价；LLVM 后端从 AST 直接生成、零改动。实测全 `.s` 中 `dial_string_new` 调用 23→0，字符串类几何平均 ~1.33x→1.083x，整体 1.0891x→1.0794x。**修复既有测试 UB**：R11 probe 原用 `extern strchr` 裸 C 指针当 string 句柄（旧版 exit=132 double-free，只看 stdout 漏检），静态化进 .rodata 后更早暴露为 SIGSEGV；改用 channel close+recv 的合法 null 来源重写（各优化级别双后端一致、exit=0），不改编译器语义、不掩盖 bug。验证全绿：golden 23/22 + 差分 fuzz 450 + 工具链 22 + PGO + 全量 run_tests + ARC 压测 + glibc 堆检查。护栏：1.10.0/零依赖/LLVM 路径零改动/不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **互递归尾调用帧复用——b12 互递归 4.263x→3.360x**：延续代替 LLVM 的性能主线，数据定位 b12 互递归奇偶判定 4.263x 为最突出单点缺口（递归类其他项 0.066x/0.342x 领先）。根因：`elide_tail_calls` 虽已把 evR/odR 的 `call`→`jmp`，但每层互跳仍 `subq $120` 建帧+`movq` 存参+`addq $120` 拆帧，60 层×2 万轮=上百万次冗余栈操作。实现（仅 `src/codegen_asm.rs::elide_tail_calls` 扩展）：caller/callee 同签名、同帧大小、无 callee-saved 时，尾跳保持 rsp 不变、跳到 callee prologue 内别名标签（跳过 subq、保留参数保存），N 层递归只建/拆一次帧；帧大小不同自动退回普通尾调用。**踩坑**：初版别名标签误插在参数保存之后，尾跳跳过保存致递归读到旧参数永不终止（b12 跑 6 分钟超时、最小程序 timeout）；修正为插在参数保存之前，新增 `tests/asm_golden/mutual_recursion.dial`（单/双参数互递归）钉死，ReleaseFast 双后端门禁覆盖。实测 b12 4.263x→3.360x（-21%）、递归类 0.5562x→0.5316x、整体 1.0905x→1.0891x（噪声内）；剩余 3.36x 是递归 vs LLVM 折叠成 n%2 闭式的本质差距，属前端优化不动。验证：golden 23+22、差分 fuzz 450、工具链 22、PGO L1-L4、全量 run_tests 全绿。护栏：1.10.0/零依赖/AArch64/LLVM 路径零变化、不打 tag。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **asm 后端自行内联 `dial_array_get`——攻性能硬阻断，数组内存类 1.60x→1.485x**：延续「代替 LLVM」主线，多维度审计裁决功能缺口 B6 已修复后，唯一剩余硬阻断是性能（公平 `--fast -O3` A/B 整体慢约 12%、数组内存类慢 60%）。根因坐实：数组下标 `a[i]` 降级为 C runtime `dial_array_get(a,i)` 调用，其函数体在 ReleaseFast（`DIAL_SAFETY=0`，边界检查编译期消除）下等价于一条 `load [a+24+i*8]`（payload 布局 `[kind][len][cap][elems@24]`）；手写汇编调预编译 C 函数无法被链接期 LTO 内联，每次读都付 call/ret+ABI 开销——正是 LLVM 靠 `--fast` LTO 吃掉、asm 长期吃不到的部分。实现（仅 `src/codegen_asm.rs::inst_call`，+38 行）：x86-64 + ReleaseFast + 签名 `[Handle,I64]->I64` 时用发射临时 rax/r10 就地发射 `shlq/addq/movq 24(...)` 内联读，跳过 call；safe/debug 保留原 call（Fail-Fast 边界检查不绕过）、AArch64 保持原 call、IR/lower/LLVM 路径零改动。**实测**（公平 A/B，校验和锚点保证等价计算）：数组内存类几何平均 1.60x→1.485x、整体 1.1155x→1.0905x（仍慢约 9%）、胜平负 6/6/16→6/7/15；`.s` 中 `dial_array_get` 调用 23→0。**门禁补盲区**：原 golden 用默认 ReleaseSafe 测不到内联路径，在 `tools/run_asm_golden.sh` 新增 ReleaseFast 双后端对比组（21 用例，LLVM LTO 内联 vs asm 发射内联输出须逐字节一致）。验证全绿：ReleaseFast 双后端 21 + 默认 golden 22 + 450 差分 fuzz + 工具链去 LLVM 化 + 自研 PGO L1-L4 + 全量 run_tests。总裁裁不变（仍慢约 9%，剩余是 GRA 全 spill + C runtime 字符串 + LTO 整体调度架构级工程，量化搁置）；`dial_array_set` 内联因 COW+所有权+C 布局耦合风险/收益不划算，本轮不动。护栏：1.10.0/零依赖/AArch64/LLVM 路径/safe-debug 行为零变化、不打 tag。证据链见 [rationale/replace-llvm-multidim-audit.md](rationale/replace-llvm-multidim-audit.md)。

- **asm 后端补齐 B6「结构体进容器」——消除审计发现的功能硬缺口**：多维度审计（`rationale/replace-llvm-multidim-audit.md`）实测发现此前 README 声称的「B6 双后端一致」失实——`run_tests.sh` 用默认 LLVM 编译 `batch3_b6_smoke.dial`，从未验证 asm；实际上 asm 在 `coerce_elem_bits` 对 struct 直接 Fail-Fast，`batch3_b6_smoke` 在 asm O0-O3 全部编译失败而 LLVM 正确运行。本批修复：`src/lower.rs` 的 `coerce_elem_bits` 支持 struct 装箱（`dial_struct_box`→payload 指针位模式）、`reinterpret_bits` 支持非 null 解箱 memcpy、新增 `unbox_struct_nullable`（**无分支 select 方案**——无条件 memset 零值模板 + 用经充分验证的 `select_values` 选源地址 + 单次 memcpy，规避手写跨块 phi 的落地 bug）处理 map get 未命中零值；补齐 put/get_or/remove 的 box 所有权结算（对齐 LLVM `sink_elem_after_retain`/remove 移交/get_or 默认 box 归还）。**验证**：`batch3_b6_smoke.dial` asm O0-O3 全部编译、四级别与 LLVM 输出逐字节一致、5 万次 put/get/remove/get_or 在 heap assert 下无 double-free/UAF、int-key 50 万次 put/remove 无内存泄漏（asm 1440KB≈LLVM 1664KB）；新增 `tests/asm_golden/batch3_b6_smoke.dial`（golden 22 个全绿）+ V 组 O0-O3 断言。**发现一个独立既有问题（不在本轮范围）**：`map<string,*>` 的 string key 本地引用未归还，50 万次循环泄漏约 30MB（`map<string,int>` 同样复现，与 struct 无关），记入独立待办。护栏：`codegen.rs`/`ast.rs`/C 运行时/1.10.0 零改动、零新依赖、AArch64 零变化、baseline .s 零回归、不打 tag。证据链见 [rationale/replace-llvm-multidim-audit.md](rationale/replace-llvm-multidim-audit.md)。

- **asm 能否代替 LLVM 的多维度实测审计（结论：当前不能，性能为剩余硬缺口）**：应要求回归核心命题，用五维证据回答「能否代替 LLVM」，不预设乐观立场。新增 `rationale/replace-llvm-multidim-audit.md`，五维结论：① 功能对等❌（审计当时 124 可编译源码中 1 个 B6 缺口——现已修复如上）；② 性能❌（同配置 `--fast -O3` 公平 A/B 跑两次，几何平均 1.1155x/1.1194x 即 asm 慢约 12%，胜平负 6/6/16，数组内存类 1.60x 最差；**推翻旧结论「领先 LLVM 46%」——那是不公平对比产物**）；③ 优化深度⚠️（IR 优化层已榨干，剩余是 LTO+数组/字符串 runtime 架构级差距）；④ 平台工具链⚠️（去 LLVM 工具链 20 用例全绿，但 AArch64 只验汇编写法不验运行语义、内联汇编/DWARF/PGO 降级）；⑤ 正确性✅（21 golden+450 差分 fuzz 0 不一致，但未覆盖 struct 进容器）。同步修正 `roadmap-gap-analysis.md` 三处过时断言（总纲 46%/基准现状/「语义层已完全追平」）。护栏：纯审计文档增量、产品代码零改动。

- **收回路线图第 4 项「PGO / LTO / bitcode 自建」——自研 PGO 闭环落地，唯一后端收回路线图四项全部收口（R4）**：让 `--pgo` 在 asm 后端可用且**零 bitcode / 零 LLVM 工具依赖**。诊断先行（`tools/diag_pgo.py`）：核实 LTO 源码级已天然单编译单元、包名级是 C `.a` 非 IR（无 IR 层工作量），bitcode 只是喂 clang 的手段非能力；实测 asm 默认块布局 55 回边循环头 100% 对齐、热路径 fallthrough、冷块甩末尾——通用块重排负 ROI（不硬做）。真正缺口是 `--pgo` 在 asm 下被 `exit(1)` 拒绝。实现：① `src/pgo.rs` 自研 `.dialprof` 纯文本格式（DIALPGO1 + 计数行，load 严格 Fail-Fast + merge，7 单测）；② `runtime/dial_rt_pgo.c` 的 `dial_pgo_dump` 读 `DIAL_PGO_FILE` 写回 profile（未设置则 no-op，自动编入 libdial_std.a）；③ `codegen_asm.rs` 的 `PgoPlan` 三态——instrument 发 `.comm` 计数器 + `pushfq/incq/popfq` 插桩 + main 返回前保护 9 个易失寄存器后 dump（避免破坏返回值，已实证 `lg(Value::Reg)` 驻留取源风险），optimize 做热函数判定（入口+循环计数、相对阈值 max×10%、main 强制、全零回退）与 `.text.hot/.text.unlikely` 分段 + 冷函数省循环对齐；④ `main.rs` 两段式编排（插桩编译→训练运行→读回→优化重编），asm PGO 独立于 `--fast`，删除原拒绝。**零回归铁证**：24 源码 `-O3` `.s` 与基线逐字节一致、baseline 路径 `profile=None` 全短路、AArch64/-g 不进 PGO；端到端输出与 baseline 一致、profile 反映真实热路径且确定、冷热分段归属正确。**性能如实记录**：A/B `pgo/baseline≈0.997`（中性偏微正、噪声内）——与诊断一致：默认布局已近最优，PGO 核心价值是功能完整性非性能飞跃，不硬声称加速。门禁 `tools/verify_asm_pgo.sh`（run_tests.sh AR 组）四层证据全绿。护栏：零新依赖、`codegen.rs`/`ast.rs`/1.10.0 零改动、AArch64 零变化、不打 tag。证据链见 [rationale/roadmap-gap-analysis.md](rationale/roadmap-gap-analysis.md)。

- **收回路线图第 3 项「B 类语言语义」之 impl 关联函数 `T::new()`（无 self 静态方法，R3）**：实现 `impl T { fn new(x: int): T }` 定义 + `T::new(args)` 调用，双后端语义一致。核心为**纯编译期脱糖**——`lexer.rs` 新增 `ColonColon` token（`:` 最长匹配，lsp 配色同步）；`parser.rs` 做三件事：① token 预扫描 `collect_type_names` 收集顶层 struct/opaque 与 impl 方法名（**支持前向引用**，与类型标注行为一致）并给出「类型未声明/没有关联函数」友好诊断；② `parse_impl_def` 以首参名是否为 `self` 区分实例方法与关联函数（空参 guard，方法注入接收者类型、关联函数不注入），两者统一 mangle 为 `__impl_T_m`；③ `parse_primary` 的 `::` 分支把 `T::m(args)` 改写为 `Expr::Call(__impl_T_m, args)`。`codegen.rs`/`lower.rs`/`semantic.rs`/`ast.rs` **零改动**（mangle 后是普通函数，复用整条 Call 链路），`::` 解析后消失不进符号表。覆盖：带参/空参关联函数、返回 struct/标量、链式 `T::new().m()`、前向引用；负向 Fail-Fast 文案正确。**零回归铁证**：26 个源码（18 golden + bench + 现有 impl + batch）.s 与基线逐字节一致（归一化调试路径后差异 0，git worktree 从 HEAD 构建基线二进制对比）、完整 golden 门禁全绿、现有三个 impl 用例输出匹配。新增 `tests/impl/impl_assoc.dial`+`.expected` 挂入 run_tests.sh R18 组。护栏：零新依赖、双后端一致、`codegen.rs`/`ast.rs`/1.10.0 零改动、AArch64 零变化、不打 tag。证据链见 [rationale/roadmap-gap-analysis.md](rationale/roadmap-gap-analysis.md)。

- **收回路线图第 2 项「LLVM 独占优化回收」诊断完成（量化搁置，零产品代码改动）**：按唯一后端总纲附则推进 R2。新增 `tools/diag_opt_gaps.py`（只读 .s，延续 diag_gra/diag_scalar_isa 的量化纪律）。反汇编对比 LLVM `--fast`（O3+LTO+SIMD）与 asm `-O3`，源码核实 pass 差集：asm 已具备常量折叠/CSE/DCE/ARC 简化/过程间折叠(e3p)/IVR 强度削减(e3t)/InstCombine(e3ad)/尾调用(e3q)/魔数除法(e3au)/ISA 微融合/浮点 GRA(e3av)；LLVM O3 有而 asm 无的仅 LICM/循环展开/函数内联/SCCP。形态量化（asm -O3 bench.s）：纯寄存器 mov 673、相邻拷贝链 82（x86 move elimination 零延迟、非瓶颈）、变量除数 idiv 0（除法全是常量魔数路径）、高频 call 全是 C runtime（dial_string_new 39/dial_string_eq 28/dial_array_get 23…，LTO 才可内联、asm 侧 IR 不可触及）。落后项归因：b02 1.071x（串行依赖链，LICM/展开免疫）、b20 1.057x 与 b22 1.144x（C runtime substring/拼接 + 数据依赖除法），均架构级不可达。`bench_asm_vs_llvm.py --quick` 实测几何平均 0.5321x、胜平负 14/11/3、asm 领先 LLVM ~47%。**结论：当前无正 ROI 的可回收 pass，诊断后量化搁置、不硬做负收益优化**（延续 e3at/e3au/e3av 纪律）；写死 4 条重启条件（高频小函数调用 / 大量循环不变量 / 可并行语义 / 落后项归因改变）。护栏：零产品代码、零新依赖、baseline/AArch64 零回归。证据链见 [rationale/roadmap-gap-analysis.md](rationale/roadmap-gap-analysis.md)。

- **唯一后端总纲确立 + 第 1 项「工具链去 LLVM 化」收口（规划批，零产品代码改动）**：按用户决策「asm 要代替 LLVM、假设 LLVM 不存在、自己就是一个且只有自己」，在 `rationale/roadmap-gap-analysis.md` 顶部确立**最高优先级总纲**（唯一后端哲学 + 对现有论证的修正指引），并附**从 LLVM 收回全部职责的路线图**——四类活（R1 工具链去 LLVM 化 / R2 LLVM 独占优化回收 / R3 B 类语言语义自建 / R4 PGO·LTO·bitcode 自建）全收回、串行推进、完成一项勾一项、不并行铺开。第 1 项已完成：新增 `tools/verify_no_llvm_toolchain.sh`（挂入 `tests/run_tests.sh` AF2 组），三层证据全绿——L1 GNU 工具链自检（gcc/cc/ar/as/ld）；L2 `DIAL_C_COMPILER=gcc` 下 18 个 golden 的 **asm 产物**与默认工具链逐字节一致（含 `.s` 产物，只测 asm 后端、不以 LLVM 后端为基准——发现 LLVM 后端在 gcc 下 fnptr/hof 有 PIE relocation 兼容问题属 LLVM 侧、与 asm 无关，已单独确认这两例 asm-via-gcc 正常）；L3 构造屏蔽 clang/zig/lld/llvm-ar 的隔离 PATH，不设 `DIAL_C_COMPILER` 时 `cc_command()` 自动兜底选 GNU cc、asm 编译运行正确。**结论：asm 后端零 LLVM 工具依赖得到回归脚本固化**。护栏：不改默认 cc 优先级、零产品代码改动、零新依赖、`codegen.rs`/`ast.rs`/1.10.0 零改动、AArch64 零变化、默认 golden 复跑全绿、不打 tag。证据链见 [rationale/roadmap-gap-analysis.md](rationale/roadmap-gap-analysis.md) 与 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3az 自研 asm 后端补齐显式 `simd<T, N>` 原生向量（高级指令集主线第二批，语义与 LLVM 逐值一致）**：承接高级指令集主线（e3ay 已建 `--target-cpu` 开关）。此前 `simd<T,N>` 是 LLVM 独有的语言一等类型，asm 后端在 `builtin.rs` 明确拒绝、布局内建也无法处理——这是语义层盘点后 asm 相对 LLVM 的**唯一实质能力缺口**（e3x 否决的「自动向量化」与此不同：那是编译器强行向量化标量循环，显式 simd 是程序员主动使用）。本批实现完整语义（对齐 LLVM `gen_simd_construct`/`gen_simd_binary`/lane 提取）：① `reg_ir(Simd)`→Ptr（向量值=指向 N×8 字节栈段的指针，仿 struct 内容区模型），每 lane 存 Dial 标量内部位模式（int/byte/char/bool=I64、float=F64）；② `lower_simd_construct`（1 参广播/N 参逐 lane）、向量二元运算（Add/Sub/Mul/Div/Mod/位运算、ICmp/FCmp 六种比较→simd<bool,N>）、`v[i]` lane 提取逐 lane 展开为既有标量 IR 指令（Load/Bin/Store/ICmp/FCmp）——**复用现有发射器，天然兼容 AArch64、与整数/浮点 GRA 完全隔离**；③ `layout_size_align` 支持 simd（int4=32、byte16=16/16，对齐 LLVM 向量 ABI）；④ byte 比较值 0..255 非负，直接有符号 ICmp 即得无符号谓词（对齐 LLVM 200>100 为真）；⑤ `expr_dt(Index)` 对 simd 返回元素类型（print 正确分派）。**验证**：ffi_lowlevel_demo 两后端 14 行输出逐行一致；广覆盖测试（加减乘/按位与/浮点六种比较/byte 边界 255）/含 call 函数向量除法与提取均两后端一致；simd 在 O0..O3 四级别输出 md5 全一致；新增 golden `tests/asm_golden/simd.dial`（19 个双后端黄金用例全绿）。**零回归铁证**：83 个源码（bench+17 golden+65 examples）默认 O2 `.s` 与改动前基线逐字节一致、AArch64 17 个源码 `.s` 逐字节一致；`bad/simd_*.dial` 语义拒绝（parser/sema 层）仍全部生效。门禁：run_tests.sh、cargo test、差分 fuzz（180 次零不一致）全绿。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0 零改动、零新依赖、LLVM 路径零改动、不打 tag。注：本批为语义补齐（逐 lane 标量展开），**目标 CPU 相关的高级 SIMD 指令发射（SSE/AVX targeted 融合，受 `--target-cpu` 门控）为后续批次**——延续 e3x「只做 targeted、不做完整自动向量化」的纪律。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3ay 自研 asm 后端引入 `--target-cpu` 高级指令集总开关（基础设施批，默认 baseline 零回归）**：用户确认多维度补齐后启动高级指令集主线（标量 ISA→SIMD→FMA→加密，由简到难），并要求编译期 `--target-cpu` 参数策略（对齐 LLVM `host_cpu()` 语义，避免 SIGILL）。本批只建立开关基础设施、不发射任何新指令——所有高级指令融合由后续批次实现并受此开关门控。实现：① `src/target.rs` 新增 `TargetCpu` 枚举（Baseline/V2/V3/V4，派生 PartialOrd 支持 `>=` 逐级判定），`parse`（baseline/x86-64/x86-64-v2/v3/v4/native 别名）+ `as_str` + `has_lzcnt`/`has_bmi1`（v2+）、`has_bmi2`/`has_avx2`/`has_fma`（v3+）特性查询；② `src/main.rs` 全链路接线（变量声明、`--target-cpu`/`--target-cpu=` 两种写法解析与非法值 Fail-Fast、asm 分支提示、`compile_asm` 传参、help 文本），仅作用于 asm 后端、LLVM 路径不动；③ `src/asm.rs::compile_asm` 与 `src/codegen_asm.rs::emit` 增加 `target_cpu` 参数（当前 `let _ = target_cpu` 占位，后续批次消费）。**零回归铁证**：默认（无 `--target-cpu`）= baseline，bench + 17 golden 共 18 个源码 `.s` 与改动前基线二进制**逐字节一致**（差异 0）；baseline/v2/v3/v4 四级别均编译运行正确、输出一致；非法值当场报错 rc≠0。AArch64 忽略本设置零变化。门禁：run_tests.sh（含差分 fuzz 180 次零不一致）、cargo test、构建零警告全绿。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0 零改动、零新依赖、LLVM 路径零改动、不打 tag、docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3ax 修复 DWARF 函数形参 location 偏移错误（gdb 可正确 print 参数，调试信息正确性补齐）**：承接「代替 LLVM」主线的调试信息维度——DWARF 自 e3w/e3z/e3aa/e3ab 落地后从未做过**端到端 gdb 实证**。本批先用 readelf/llvm-dwarfdump/gdb 对同一程序（core2.dial，覆盖参数/局部变量/struct/数组/string/float/bool）做两后端 `-g -O0` 对比，DIE 树统计本已对等（subprogram/param/variable/location 数完全一致，ASM 甚至类型 DIE 更丰富、DWARF v5 vs LLVM v4），但 **gdb 实测暴露真 bug**：ASM 后端断点停在 `add` 函数时 `print a`/`print b` 显示 `11/11`（实参本是 `3/4`），而 LLVM 后端正确。根因（临时插桩/反汇编/llvm-dwarfdump 三方定位）：`emit_dwarf_cu` 把 formal_parameter 的 `DW_OP_breg7(rsp)` 偏移写成占位的 `8*i`（a→RSP+0、b→RSP+8），而 prologue 实际把 C ABI 参数写到 `alloca_off[param_slots[idx]]`（本例 a→0x58、b→0x60）；局部变量 location 用真实 `alloca_off`（故局部变量正确），唯独形参从未接上真值。修复（仅 `src/codegen_asm.rs`）：`FnDwarfInfo.params` 从 `(名,类型)` 扩为 `(名,真实偏移,类型)`，偏移在 `emit_function` 里取 `alloca_off[param_slots[idx]]`（与 prologue 写参、函数体读参、局部变量 location 完全同源），`emit_dwarf_cu` 用该真值替代 `8*i`；栈传参（>6 个）与死参数 DCE 路径均已覆盖。验证：① 修复后 DWARF 偏移 a/b=RSP+88/+96（0x58/0x60，与反汇编一致），gdb `add(a=3,b=4)`、`info args` 全部正确；② 7 参数混合类型专项（6 寄存器 int/float/bool + 1 栈传参）gdb 一次断点显示全部 `a=11,b=2.5,c=1,d=44,e=55,f=66,g=77`，与实参与运行时完全一致；③ 运行时输出不变（add(3,4)=7）；④ **零回归**：非-g 全部 18 个源码（bench+17 golden）`.s` 与未修复基线逐字节一致、AArch64 全部 17 个源码 `.s` 逐字节一致（修复只在 `dwarf_on = debug_info && X86_64SysV` 路径生效）。回归门禁：run_tests.sh、cargo test、差分 fuzz、check 工具全绿。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0 零改动、零新依赖、LLVM 路径零改动、构建零警告、不打 tag、docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3aw 编译期常量折叠解释器记忆化（asm 后端含递归折叠程序编译提速 ~30%，零语义/产物变化）**：延续「代替 LLVM」主线的编译速度维度（e3an3 已做 stdlib 持久缓存 + `--fast-compile`，本批攻前端）。先做严谨归因：插桩分解 asm 后端 O2 编译（bench.dial）为 lower 0.36ms / IR passes 0.20ms / **fold_pure_calls 34.5ms（占前端近半）** / ivr 0.21ms / emit 9.6ms / `cc -c` 26ms。根因是 e3p 过程间常量折叠（e3p，把 `fibR(23)`/`treeR(18)` 等纯整数递归函数在编译期求值，是递归类基准大幅反超 LLVM 的核心优化）用**朴素递归解释器**：`fibR(23)` 重复求解同一子问题达 973,714 步、`treeR(18)` 91,966 步——纯函数返回值完全由 `(name, args)` 决定，这是纯粹的指数级重复计算。实现（仅 `src/pass.rs`，+18/-3 行）：给 `e3p_eval_call` 加 `(函数名, 全整数实参)→结果` 记忆化缓存，入口命中直接返回（不消耗步数预算，该结果上次已在预算内算出），只缓存成功求值（除零/超预算/放弃路径不写）、每个顶层折叠用独立 memo（保守：不改变任何折叠决策与跨调用行为，只加速单次求值），与 CSE 值编号思想同源。关键正确性：缓存键只含纯函数参数、命中结果与重新求值逐位一致；失败不写缓存避免污染；独立 memo 保证顶层调用间零交互。验证：① 折叠值不变（rsumR(25)=325 / fibR(23)=28657 / treeR(18)=8361，28 项 checksum 全一致）；② **产物零变化**——bench + 18 个 golden 程序 `.s` 与 HEAD 基线（独立 worktree 构建）逐字节一致，证明生成代码完全没有改变、运行时性能与语义天然一致；③ A/B 交替计时（min 口径）：bench.dial 112.9ms→76.1ms（**-32.6%**）、median 120.3→85.2ms（-29.1%），多源码复测 bench -34.4%，而无递归常量的 channel/array demo 无变化（+4.8%/-2.0% 噪声）——证明收益精确来自消除 e3p 指数重复求值、无额外开销。回归：AF 18 用例、AG 差分 180 次（golden 11 + 固化语料 4 + 在线 fuzz）零不一致、全量 run_tests.sh rc=0、cargo test 绿、AArch64 零变化（改动只在 x86-64 的 `fold_pure_calls` 路径内）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0 零改动、零新依赖、LLVM 路径零改动、构建零警告、不打 tag、docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3av 自研 asm 后端浮点寄存器驻留（fGRA，浮点密集 workload asm/LLVM 3.0×→1.64×）**：延续「代替 LLVM」主线，对性能支柱①里此前未诊断的浮点寄存器分配做诊断，发现是**正收益缺口**——浮点值被 `residency_candidate`（`!t.is_float()`）排除在 GRA 驻留池外，每个运算都走 `lf`(栈→xmm8)→op→`sf`(xmm8→栈)。新增可复现诊断工具 `tools/diag_fgra.py`：一个无 call 浮点密集热循环静态栈访问/运算比 5.8×，asm/LLVM 耗时比 3.0×；受控实验（常量外提）证明主因是浮点反复 load/store 栈、非常量物化。实现（仅 `src/codegen_asm.rs`，+134/-5 行）：新增独立的浮点驻留池 `FP_RES_POOL`（xmm1-7 + xmm10-15 共 13 个，排除返回值 xmm0 与 scratch xmm8/9），在 `analyze_unified`（无 call 纯计算函数）内并行累加 `fp_score` 并绑定；`lf`/`sf`/`bin_src` 加浮点驻留快路径（寄存器直读直写，与整数 `phys` 表完全独立，锁死整数回归面）；`inst_fbin` 加 dst 驻留快路径（结果直接落物理 xmm，不经 xmm8 scratch）；phi 用 `sf_force_stack` 强制写栈 + 块入口 `fp_loads` 重载（与整数 `sg_force_stack` 对称，避免快路径劫持导致静默错误）。关键根因实验：只扩 6 个 xmm 池收益仅 8%（中间值落栈稀释），扩到 13 个后 1.64×（容量是瓶颈）；极致单变量场景 6 池即 1.48×，证明机制本身有效。范围：只覆盖无 call 多块函数；含 call/单块/`-g`/O0-O1/含内联 asm 函数浮点仍全 spill（零变化），列为后续阶段。验证：diag_fgra 静态比 5.8×→2.87×、动态比 3.0×→1.64×，双后端 stdout 逐字一致；8 浮点参数（xmm0-7 ABI 全用）函数双后端一致；新 golden `fp_residency.dial` 覆盖多块浮点 phi/浮点参数/浮点返回。零回归归因：17 个既有 golden 中仅 `hof`（含 float sort_by 比较器）汇编变化且全是预期 `movsd 栈↔栈`→`寄存器↔寄存器` 驻留优化、输出一致，其余 16 个逐字节不变；bench 纯整数程序 `.s` 逐字节一致；AArch64 含浮点程序 `.o` 逐字节一致。回归：AF 18 用例、AG 180 次差分零不一致、全量 run_tests.sh rc=0、cargo test 绿。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0 零改动、零新依赖、LLVM 路径零改动、构建零警告、不打 tag、docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3au 自研 asm 后端常量乘立即数融合（`imulq $imm32, r` 省掉 `movabsq`）**：延续 targeted ISA 微融合主线，审计发现驻留快路径对整数乘 `Mul` 禁用了立即数折叠（`allow_imm` 排除 Mul，注释「imul 没有立即数形式」），导致 `x * 31` 每次发 `movabsq $31, %r10`（10 字节）+ `imulq %r10, %r15` 两条。实际上 x86-64 有 `imulq $imm32, r/m64, r64` 三操作数形式（GAS 对 `imulq $imm, r/m` 自动编码为隐含目的），imm32 按符号扩展参与 64 位乘法，与「先装 64 位再乘」在 int64 低 64 位语义上等价。实现：把 `ABin::Mul` 纳入 `allow_imm`，`bin_src` 对 imm32 范围内的整数常量直接返回 `$n`，驻留/通用两路径统一受益；超 imm32 常量（如 4294967296）仍自动退回 `movabsq + imulq`；AArch64 的 `bin_src` 不处理 Imm 故零影响。验证：cc 汇编 + objdump 确认正/负 imm32 编码合法；常量乘边界专项（imm32 上界 2147483647、负 -7、超范围 4294967296、0、1）双后端输出逐字节一致；bench 双后端输出逐字节一致。零回归归因：17 个 golden 中仅 `control_flow`/`fnptr` 汇编变化，且 diff **全部**是 `movabsq $2,%r10; imulq %r10,reg`→`imulq $2,reg`（预期融合），其余 15 个逐字节不变；含常量乘的 AArch64 程序交叉汇编 `.o` 逐字节一致（md5 596424068c…）。A/B：b03 +1.2%、b05 +1.4%、b02 +0.25%（b02 瓶颈在除法逆元依赖链，收益小），几何平均 0.5337x→0.5367x 属计时噪声带；b02 仍 1.067x 落后 LLVM（微融合收益已边际，符合既有判断）。回归：AF 17 用例、AG 180 次差分零不一致、全量 run_tests.sh rc=0、cargo test 绿。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0 零改动、零新依赖、LLVM 路径零改动、构建零警告、不打 tag、docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3at caller-saved 寄存器分配收益诊断（用数据否决高风险架构重写，零产品代码改动）**：承接 e3as 差距盘点——语义构造已清零，性能支柱① O3 管线里 caller-saved / 浮点 / 跨函数寄存器分配是最后有工程量的方向。用户「不留缺口逐个补」，但 caller-saved 不是增量 pass，而是要把 GRA 从「全函数固定绑定」重写为「有活跃区间的 live-range 分配」，内核级工程、多批次、高风险。按仓库 e3x 定量否决完整 SIMD 的纪律（先量化 ROI 再决定投入），本批先诊断、**不改任何产品代码**。新增 `tools/diag_gra.py`（只读 asm 后端生成的 .s，命令写在 docstring，完全可复现），三个独立指标：① 上界比 `S/(8*C)`——把 caller-saved 加进全函数绑定池须在每个 call 点 save/restore 4 个寄存器=8 条 mov（成本 8*C），收益乐观上界是全部栈访问 S；② 每函数独立的栈槽访问频次分布（区分 C ABI 强制落栈 vs 真正寄存器压力 spill）；③ call 密集函数栈帧规模。实测（bench.dial O2，21 个含 call 函数、208 个 call）：**上界比合计 0.629<1**（call 最密集的 run 86 call 仅 0.472）；**86.8% 栈槽只访问≤2 次**（1 次 15.1%+2 次 71.7%，为 call 结果落栈/参数中转，值活不过下一次 call、被 C ABI 强制，与寄存器数量无关），高频热点(≥4次)仅 9.6%、贡献 26% 访问；run 栈帧 1416B、b17 936B。**结论：暂不投入 live-range 重写**——三条证据同源，收益空间落在已领先 LLVM 的数组/字符串类，而持平 LLVM 最该提速的算术类是无 call 函数（caller-saved 够不着）；与 e3x 否决 SIMD 同决策模式（ROI 为负，工程投到确定方向）。rationale 写明**重启条件**（若出现 call 间大量长 live-range 整数值的新 workload，使上界比显著>1 且高频栈槽占比上升，则重评）。护栏：本批 `src/` 全部文件、`Cargo.toml`、所有 `.s` 输出逐字节不变，新增仅诊断工具+rationale；A/B/AF/AG/版本 1.10.0/零新依赖/零警告/不打 tag/docx 确定性不受影响。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3as 自研 asm 后端补齐内联汇编 `@asm<T>(...)` / `@asm_intel<T>(...)`（双后端一致性补全，语义层最后的纯能力差距）**：承接「代替 LLVM」主线，审计确认内联汇编是唯一 LLVM 完整支持却 asm 明确 `BKind::Unsupported` 拒绝的语义构造（SIMD 已由 M7-e3x 定量证伪完整后端 ROI 为负、战略否决，不算遗漏）。此前 `tests/asm_golden/negative/unsupported.dial` 是「最稳定的 unsupported 负例」，注释断言「需自研后端内嵌汇编器，短期不可能落地」——本批推翻该断言：LLVM inline asm 本质是「模板占位符替换 + 寄存器分配 + 原样交付汇编器」，真正的汇编由 `cc` 完成，asm 后端只需生成正确的 `.s`。实现：① `ir.rs` 新增 `IrInst::InlineAsm`（携带 template/constraints/dialect/args/arg_tys/ret_ty），补齐 `is_pure`（有副作用，DCE 不删）/`def`/`reg_uses`/`escapes` 四个穷尽 match，`pass.rs::subst_inst` 同步替换输入 vreg；② `lower.rs::lower_call` 拦截 `asm`/`asm_intel`，`lower_inline_asm` 按 C ABI 宽度（`param_ir`）适配输入实参、返回按 `abi_adapt` 口径用 Conv Zext 从 C ABI 宽度扩回 Dial 内部宽度（byte i8→i64、bool i32→i64）；③ `codegen_asm.rs` 发射期解析约束串（`=r`/`=&r`/`+r`/`r`/数字绑定如 `=r,0` 同寄存器），操作数用 x86-64 整数实参寄存器池 rdi/rsi/rdx/rcx/r8/r9（不属 GRA 驻留池、不属发射临时 rax/r10/r11），`render_asm_template` 逐字节替换 `$N`（按操作数 IrType 渲染宽度 `%rax`/`%eax`/`%al`）与 `$$`→`$`，Intel 方言经 `.intel_syntax noprefix` / `.att_syntax prefix` 包裹、寄存器名不带 `%`。**关键护栏**：含 InlineAsm 的函数强制全 spill（与 -g/O0 同路径禁用 GRA 驻留），避免手写汇编踩踏驻留值；AArch64 方言语义不同，`emit()` 预检 Fail-Fast 明确拒绝。实测四种形态（`$1→$0` 搬运、`=r,0` 原地累加 `$$7`、Intel 方言、byte 宽度适配）双后端 stdout 逐字节一致（42/42/77/200），单输出 `=r` 亦一致；语义层四个负例（返回类型/缺参/无输出约束/模板非常量）仍被拒。A/B：新增 `tests/asm_golden/inline_asm.dial`（第 17 个 golden），删除已失效的 `negative/unsupported.dial`（该文件仅测内联汇编）；其余 16 个 golden 程序与 28 项 bench 程序因不含 asm、汇编输出**逐字节不变**，几何平均 0.5402x→0.5366x 属计时噪声带；AArch64 普通程序 `.o` 逐字节一致（md5 596424068c…）。AF 门禁 17 用例全绿、AG 差分 180 次零不一致、全量 run_tests.sh rc=0、cargo test 绿。护栏：`src/codegen.rs`/`src/ast.rs` 零改动、`Cargo.toml` 1.10.0、零新依赖、LLVM 路径零改动、构建零警告、不打 tag、docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3ar 新增双后端差分 fuzz 框架（把「实现一致性 bug」变成可复现的自动化发现流程）**：承接 e3ao/e3ap/e3aq 的教训——这三批的真实缺口（嵌套下标赋值、借用句柄 retain、默认实参补齐）都是静态审计发现不了、只有实际跑程序对比双后端输出才能暴露的实现一致性 bug。用户「继续可以」后新增 `tools/diff_fuzz.py`：① **确定性生成器**（固定 seed 的 PRNG，维护符号表，只引用已声明变量、规避下标越界/除零/非确定性内建，合法率 100%），程序覆盖标量运算/控制流/int/string 数组/struct/默认实参/impl 方法/借用绑定/嵌套下标赋值/COW 别名/slice 后写等全部近期高风险路径；② **差分对比器**——同一程序编译 llvm/asm 两后端，逐字节对比 stdout + 退出码，只在「LLVM 编译+运行成功、asm 编译成功但输出或退出码不同」时记 DIFF（编译失败/UB 记 skip，不误报），并校验 `.s` 产物存在以防假路径；③ **delta-debugging 最小化器**——按行删减收敛到最小复现（元测试从 13 行收敛到 4 行）；④ 三模式：`--golden-check`（框架自检）、`--seeds N`（在线 fuzz，支持 `--opt-levels`/`--jobs`/seed 起点分段）、`--replay`（复跑落盘用例）。**元测试证明框架有效**：对修复前编译器（5262fbf，缺默认实参补齐）报出 DIFF（`asm=140733879947203` vs `llvm=15`），对当前编译器报 OK——不是永远返回 OK 的空壳。实测：golden 自检 15/15 OK；增强后 500 seeds × 5 优化级别（O0/O1/O2/O3/g）= 2500 次双后端对比全 OK、0 不一致；固化 4 份覆盖多路径的确定性语料到 `tests/diff/`。新增 `tools/run_diff_fuzz_gate.sh` 三段门禁（golden 自检 + 固化语料 + 固定 seed 在线 fuzz，quick 模式 60 seeds×3 O），接入 run_tests.sh 为 AG 组。纯工具增量，编译器产品代码零改动，故现有 golden `.s` 天然零变化。护栏：冻结文件/LLVM 路径/AArch64/版本 1.10.0/零新依赖/构建零警告/不打 tag/docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3aq 自研 asm 后端补齐 B1 impl 方法块调用 + 默认实参补齐（双后端一致性补全）**：承接"代替 LLVM"主线，审计确认两处 LLVM 支持却 asm 完全缺失的缺口。① **impl 方法调用**：LLVM 的 `gen_method_call`（codegen.rs:5479）对具名类型（用户 `struct` / 包 `type X;` 不透明句柄）用 `impl_receiver_name`→`impl_method_symbol`→`make_impl_call` 把 `obj.m(args)` 就地改写为 `Expr::Call(__impl_<T>_<m>, [obj]+args)` 并递归 `gen_expr`，复用整条普通函数调用链；asm 的 `lower_method` 对 Struct/Opaque 恒「方法暂不支持（后续相位）」拒绝——实测 `tests/impl/impl_struct.dial` 的 `p.sum()`、`impl_gui_handle.dial` 的 `win.add()` 全部编译失败，而 GUI 的 OO 封装层（`packages/gui/src/gui_oo.dial`）整层 B1 方法在 asm 下不可用。修复：`lower_method` 在 `od` 为 Struct/Opaque 且 `__impl_<T>_<m>` 在 `functions_sig` 时构造 `Expr::Call` 递归 `lower_expr`；`expr_dt` 的 Method 分支同步从脱糖符号签名取返回类型（否则返回 string/struct 的方法被误判为 Int）。必须放在内建方法分派之前——impl 方法可与内建撞名（`Point.push`），具名类型分派权归 impl。② **默认实参补齐**（实现 impl_default 时暴露的更根本缺口）：LLVM 的 `gen_call`（codegen.rs:5384）按 `FuncInfo.defaults` 在调用点补齐缺失尾部参数；asm 的 `lower_call` 完全缺失此步，少传参数读到未初始化栈槽——实测顶层 `add(5, b=10)` 得到 102 而非 15，所有带默认值的函数与 impl 方法均受影响。修复：把实参求值抽成 `lower_call_arg`（lower_expr→adapt_param→struct 按值拷贝，显式与补齐参数共用），在 args 循环后按 `sig.defaults`（`!variadic`）补齐并逐个 `lower_call_arg`。实测：三个官方用例 `impl_struct`/`impl_default`/`impl_gui_handle` asm 输出匹配 .expected 且与 LLVM 逐字节一致；impl 方法间调用、返回 struct、与内建撞名、Opaque 句柄、默认参数(表达式/string/struct)、各 O 级别全通过。A/B：新增 `tests/asm_golden/impl_method.dial`（第 15 个 golden，共 16 用例），其余 14 个 golden 程序 `.s` 与 HEAD（5262fbf）旧编译器（git worktree 独立构建）**逐字节一致**——`lower_call` 重构对无默认参数程序零影响；AArch64 普通程序（core1/structs）`.s` 零变化，impl 程序经 clang 交叉汇编产出 ELF aarch64 `.o`（含 4 个 `__impl_Point` 符号）。新增 R18 测试组（三官方 impl 用例 + 默认参数边界 + 各 O 级别）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 零改动，构建零警告，不打 tag，docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3ap 修复 asm 后端借用句柄绑局部变量的 retain 结算缺口（对齐 LLVM `retain_if_borrowed`）**：承接 e3ao 记录的独立既有缺口，本批修复。根因：asm 后端 `VarDecl`/`Assign`（`lower.rs`）用 `lower_expr` 返回的 `is_copy` 标志决定是否对存入变量的句柄 retain 一份，但该标志对 `Expr::Var`/`Expr::Field` 为 `true`、对 `Expr::Index`/`Expr::Method(get,at)` 却误返回 `false`——后两类同样是「从容器借来的元素句柄」（源容器仍持有），绑到局部变量时必须 retain，否则作用域退出 `release_scope` 无条件 release 会把 rc 多减 1。此前潜伏：`var row = m[i]` / `var row = m.get(i)` 在纯读取程序里 rc 恰好平衡不崩；e3ao 使官方示例 `examples/multi_dim_demo.dial` 原文（含 `var row: int[] = m.get(i)` + 之后 `m[0][0]=11`）可用 asm 编译后暴露——退出时 `dial_array_release(m)` 遍历释放已被提前 free 的内层数组，gdb 定位 `free(): unaligned chunk detected in tcache 2`（`dial_array_release → release_buffer → elem_release → dial_rc_release`），实测修复前 rc=134。修复：`VarDecl`/`Assign` 传给 `store_value` 的 retain 判据改为 `is_copy || (yields_borrowed_ref(value) && !*move_hint)`，`yields_borrowed_ref`（lower.rs:3871，与 LLVM 同口径：Var/Field/Index/get/at）兜住 `Index`/`get`/`at` 的遗漏、`!move_hint` 对齐 LLVM 移动语义不 retain 的条件；纯增量、只影响「句柄元素的 Index/get/at 绑变量且非 move」场景。对齐 LLVM 权威实现 `retain_if_borrowed`（codegen.rs:1970：`is_heap_type && yields_borrowed_ref && !move_hint`）。实测：multi_dim_demo 原文修复后 asm rc=0、无 stderr、双后端 stdout 逐字节一致（修复前 23 行即崩）；get/Index 绑局部变量、链式重绑、借用句柄作赋值右值、`string[][]` 句柄元素、-O0..-O3/-g、`MALLOC_CHECK_=3` 全通过。A/B：新增 `tests/asm_golden/borrow_bind.dial`（第 14 个 golden，共 15 个用例含默认/llvm/unsupported/aarch64/fast-compile 门禁），其余 13 个 golden 程序 `.s` 与 HEAD（067d006）旧编译器（git worktree 独立构建）**逐字节一致**——golden 中的 `arr[0]`/`.get()` 均为 print 参数不绑变量故不受影响；AArch64 普通程序（core1/core3/hof/containers）`.s` 零变化。新增 R17 测试组（multi_dim_demo 原文崩溃回归 + 借用绑定综合场景 + 各 O 级别）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 零改动，构建零警告，不打 tag，docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3ao 自研 asm 后端补齐嵌套下标赋值 `a[i][j] = v`（双后端一致性补全，语言层面最后一块被明确拒绝的构造）**：承接 e3ag~e3an3 双后端一致性主线（sizeof/bitcast/assert/is_null/drop/channel/spawn/高阶方法/select + 产物层三开关），盘点发现最后一处 LLVM 完整支持却 asm 明确 Fail-Fast 拒绝的语言构造——**嵌套下标赋值**：`Stmt::AssignIndex` 仅允许 `arr` 是变量（`Expr::Var`），`arr` 为 `Expr::Index`（即 `m[i][j] = v`）时在 `lower.rs` 直接报「索引赋值目标仅支持变量（嵌套下标后续相位）」。真实官方示例 `examples/multi_dim_demo.dial` 的 `m[0][0] = 11` 即触发，此前 asm 无法编译该程序。确凿缺口：LLVM 后端有完整递归左值解析 `gen_assign_lvalue`（codegen.rs:2450），对 `Expr::Index` 逐层 `dial_array_set_borrowed`（内层数组是从外层元素槽**借来**的引用，绝不消耗）+ 把 COW 更新后的新句柄写回外层容器直到最外层变量槽，并精确结算每层 value 所有权（`is_borrowed` 语义对齐 `retain_if_borrowed`/`settle_fresh_value`）。C runtime 的 `dial_array_set_borrowed` 符号早已就位（e3an 为高阶方法引入时同步落地）。实现：`lower.rs` 新增 `lower_assign_lvalue` 递归函数（`Var` 分支 load 旧→store 新→RcRelease 旧；`Index` 分支 lower 借来的容器句柄→`coerce_elem_bits`→`dial_array_set_borrowed`→递归写回→按 `is_borrowed` 结算 value），`Stmt::AssignIndex` 在 `arr` 为 `Expr::Index` 时构造 `Index(arr, index)` 完整左值转入该函数；单层 `a[i] = v`（arr 为变量）完全走既有路径、汇编零变化。实测：`examples/multi_dim_demo` 的去陷阱版（避免独立的 get-borrow 缺口，见下）双后端 stdout 逐字节一致，多维读写 / push 后写 / COW 共享后写 / 句柄元素 `string[][]` 嵌套写 / -O0..-O3 / -g 全部一致，无崩溃。A/B：新增 `tests/asm_golden/nested_index.dial`（第 13 个 golden），其余 12 个 golden 程序 `.s` 与 HEAD（ed342cf）旧编译器**逐字节一致**（git worktree 对比）、AArch64 普通程序 `.s` 零变化、nested_index 经 clang 交叉汇编产出 ELF aarch64 `.o`（含 `dial_array_set_borrowed` 符号）。新增 R16 测试组（双后端一致 / 各 O 级别 / 句柄元素）；AF 门禁 14 个用例全绿。**另发现一个独立既有缺口**（本次不修，留后续批次）：asm 后端 `Expr::Index` / `Expr::Method get` 返回借用句柄时 `is_copy=false`，局部变量绑定（`var row = m.get(i)` / `var row = m[i]`）时不 retain，但作用域退出 `release_scope` 无条件 release，导致 rc 多减——LLVM 有 `retain_if_borrowed` 补这一份。该 bug 此前因 multi_dim_demo 无法用 asm 编译而潜伏，本次嵌套赋值使其暴露：`examples/multi_dim_demo.dial` 原文（含 `var row: int[] = m.get(i)`）现可编译但退出时 `free(): unaligned chunk`。修复需统一 `Expr::Index`/`get` 的借用返回值语义，会改变所有含句柄元素读取程序的汇编，风险超出本批零回归护栏，故如实记录到路线图后续批次。**具体证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)**。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告，不打 tag，docx 确定性。
- **M7-e3an3 双后端补齐产物/工程层能力：stdlib 持久缓存 + `--fast-compile` 极速编译（编译速度维度）**：用户质疑上一批"语言层面对齐缺口全补齐"仅覆盖语义层，产物/工程层仍有缺口。实测确认：`--single-file`（全静态，1.1MB，`ldd` 显示 not a dynamic executable）与 `--tiny`（149KB→6.9KB，21×，strip+gc-sections）双后端均已支持且生效；唯**极致编译速度**是真缺口——无一键开关，且未装预编译 stdlib 包时 `resolve_stdlib_archive` 每次无条件重编全部 15 个 C 源（实测默认/-O0 均 ~0.35s，瓶颈在 C 编译而非 Dial 前端），`-O0` 仅跳过 Dial IR pass。实现：① **持久缓存**——回退分支改为按「源码内容 SHA256（含 .c 与 .h，头文件经 -I include 必须入 key）+ safety.as_c_macro + tiny + fast_compile + cc+cc_prefix（含 --target，机器码不兼容）+ 完整 C flags」拼 key 整体 SHA256（复用 `package::sha256_file`，新增 `sha256_str`），缓存落在 `.dialc_build/stdlib-cache/<key16>/{libdial_std.a,key.txt}`；命中跳过全部 C 编译，未命中编到 PID 临时子目录、`ar rcs` 输出到全新 tmp 路径（避免对已存在归档增量更新残留旧成员）、原子 rename **先 .a 后 key.txt**（崩溃最多退化重编，不会出现 key 新 .a 旧）、写入失败优雅退化；对 touch（mtime 变内容不变）不敏感，内容真变才失效；默认构建同样受益且零语义变化（同输入同 flags 的 .a 逐字节一致）。② **`--fast-compile`**（用户确认隐含强制 O0）：CLI 全链路（变量/解析/冲突矩阵/help/LinkOpts.fast_compile/mode 构造），与 `--fast`/`--pgo`/`--tiny`/显式 `-O1..3` 硬拒 Fail-Fast、与 `-O0`/`-g`/`--single-file` 允许，冲突检查后强制 `opt_level=O0`（asm 前端跳过全部 Dial IR pass）；C stdlib 用 `-O0`；`resolve_stdlib_archive` 强制 `installed=None` 绕开已装 -O2 预编译包（否则缓存与 -O0 失效）；签名改为收 `&LinkOpts`；LLVM 后端如实提示"仅作用于 C stdlib（-O0+缓存），Dial 优化仍由 --fast/-g 控制"；LTO 分支固定 -O3 不缓存（fast-compile 已与 fast/pgo 互斥）。非 quiet 打印缓存命中/未命中 k12 与跳过 C 源数。实测：首编未命中/二编命中且 .a sha256 不变、改 .h/.c 内容触发新 key 未命中、默认 -O2 与 fast -O0 两个独立缓存分区（2 个 .a）、冲突矩阵 4/4 Fail-Fast、双后端与 fast/非 fast 语义输出逐字节一致。新增 R15 测试组（临时副本 DIALC_STDLIB_DIR 隔离，12 项）+ AF 门禁 `--fast-compile` 双后端用例（13 个用例）；完整回归 758 PASS / 0 FAIL。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 零改动、构建零警告、不打 tag、docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3an2 自研 asm 后端补齐 `select` 多路通道选择（双后端一致性补全，补齐语句级控制流最后一块）**：承接 e3ag~e3an 双后端一致性主线（sizeof/bitcast/assert/is_null/drop/channel/spawn/高阶方法已补全），补齐 LLVM 支持却 asm 明确 Fail-Fast 拒绝的最后一个语句级构造——`select`（v0.91 多路通道选择）。确凿缺口：`select` 在 LLVM 完整支持（`codegen.rs:3466-3688`，setup→loop→chk→dispatch→take/case + nowait/eof/end，事件驱动等待），asm 却在 `lower.rs:1221` 直接 `self.err("asm 后端暂不支持 select")`。审计确认硬前置全齐：C runtime 的 `dial_sel_{new,add,pick,recv,free}` + `dial_chan_{epoch,wait}` 符号就位（stdlib/dial_channel.c，已链接）；IR 的 `new_block`/`Term::Branch`/`Term::Jump` 多基本块与静态栈槽跨块传值、`CallLink::Runtime` 的 C 调用、`ICmp`/`Load`/`Store` 齐备。核心难点是**多基本块控制流 + break 语义**：IR 无 Switch，用顺序 `Term::Branch` 条件链模拟按就绪下标分派；retry 回边只回 loop（绝不回 setup，否则每轮重复 `dial_sel_new` 泄漏 selector）；**select 的 retry loop 不 push `self.loops`**——case body 里的 `break` 必须经 `self.loops.last()` 跳到 select **外层**那个 while 的 exit（select 是语句不是可 break 的块，对齐 LLVM basic block 语义），`release_scopes_upto` 在 break 时释放绑定变量并标 moved、pop 时不二次释放。跨块值（selector 句柄/epoch/就绪下标/元素句柄）全部经 `emit_alloca` 栈槽传递，规避 ICmp 融合跨块重发隐患；epoch 先于 pick 读取（不丢唤醒）；`dial_sel_free` 在 case/default body 之前发（body 里 return 不泄漏），但 isnull 重试路径（元素被别的消费者抢走）不 free、直接回 loop 重挑；全体 EOF 直接结束不执行任何分支；无 default 时经 `dial_chan_wait(epoch)` 事件驱动阻塞（非轮询）。绑定变量类型=通道元素类型（语义层限定只放行句柄，白名单见 semantic.rs:3993-4010），`dial_sel_recv` 返回句柄所有权直接转移（is_copy=false 不 retain），push_scope/leave_scope 严格限定作用域。实测 6 个示例（default/eof/two_channels/fairness/exit_break/exit_condition）双后端 stdout 逐字节一致（含 break 跳出外层 while、公平性 true/true/400、总数统计 2/2/0），`-O0..-O3/-g` 一致；新 golden `tests/asm_golden/select.dial` 5 次运行稳定一致（输出 3/hello/1/4/2，验证跨块元素句柄内容正确）。A/B：新增 select golden（第 12 个），删除已失效负例 `unsupported_select.dial`（asm 现已支持该构造），其余 11 个 golden 程序 `.s` 与 HEAD（0688434）旧编译器**逐字节一致**（git worktree 对比，含 hof）、AArch64 普通程序 `.s` 零变化、select 程序经 clang 交叉汇编产出 ELF aarch64 `.o`（27 个 select/chan 符号）。完整回归 746 PASS / 0 FAIL。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告，不打 tag，docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3an 自研 asm 后端补齐高阶数组方法 `map`/`filter`/`reduce`/`sort_by`（双后端一致性补全，补齐动态回调最后一块）**：承接 e3ag~e3am 双后端一致性主线（sizeof/bitcast/assert/is_null/drop/channel/spawn 已补全），补齐 LLVM 支持却 asm 明确 Fail-Fast 拒绝的最后一组语义构造——高阶数组方法。确凿缺口：`map/filter/reduce/sort_by` 在 LLVM 完整支持（`codegen.rs::gen_hof_thunk` + `dial_array_hof`/`dial_array_reduce`），asm 却把它们批量归入「并发/高阶函数后续相位」`BKind::Unsupported` 明确拒绝（`builtin.rs` 顶层 + `lookup_method` 的 Array 分支均返回不支持），且 `expr_dt` 对方法调用一律推断成 `Type::Int`（链式调用如 `a.map(f).get(0)` 会错位）。审计确认硬前置全齐：C runtime 的 `dial_array_hof`/`dial_array_reduce` 符号就位、`FuncAddr` 可取具名函数代码地址、`CallIndirect`/`FuncAddr`/8 字节槽位模式重解释（`reinterpret_bits`/`coerce_elem_bits`）/三套标量宽度（`reg_ir`/`param_ir`）全部支持。核心难点是**合成精确签名回调 thunk**：C 运行时只认统一签名 `int64_t(*)(int64_t,int64_t,void*,void*)`，而 Dial 回调签名精确（float=f64 / bool=C ABI i32 / byte=i8 / int=i64，还带 rawptr<void> 状态参数），硬套统一签名是 UB——仿 LLVM `gen_hof_thunk` 生成独立 `IrFunction __dial_hof_<arity><elem><ret>`：从 i64 槽位位模式还原 elem（float 经 store/load 重解释、bool/byte 按 param_ir 收窄）→ 间接调用回调（`CallIndirect`，state 原样透传）→ 返回值 C ABI 宽度转回内部再压回 i64 位模式；按 (elem,ret,arity) 去重 + 名字排序确定性生成（与 spawn thunk 同款，在 `lower_program` 末尾统一追加）。`lower_method` 新增专用路径：`map`(op=0)/`filter`(1)/`sort_by`(2) 调 `dial_array_hof`（out_kind 按返回/元素类型），`reduce` 调 `dial_array_reduce`（init 压位模式、结果还原）；`expr_dt` 补高阶方法返回类型推断（与语义层 check_method_call 同口径）。**过程中发现并修复一个既有 codegen_asm bug**：回调的 `rawptr<void>` state 参数从未被读取时其参数槽 Alloca 被 DCE 删除，prologue 写入处 `unwrap_or(&0)` 会把 state 写到偏移 0，恰好覆盖 x86 callee-saved 保存位（save_off=0），破坏 C runtime sort_by 回调链里的 rbx/r13 → 插入排序错乱甚至 SIGSEGV（gdb 定位：崩在 `dial_array.c:315 e[j+1]=key`，rbx=state-1、r13=float 位模式）；修复为「槽已删除则跳过写入」（函数体无 Load 读它，跳过等价且安全），普通程序所有参数槽保活、off 必命中，此分支零影响。实测 int/float/byte/char 四种元素 × map/filter/reduce/sort_by × 真实 state/null state 全场景双后端 stdout 逐字节一致，float 比较器 sort_by 退出码正常（修复前 SIGSEGV），`-O0..-O3/-g` 一致。A/B：新增 `tests/asm_golden/hof.dial`（第 11 个 golden），其余 10 个 golden 程序 `.s` 与新编译器**逐字节一致**（git worktree 编译 HEAD 9f0ed9b 对比，md5 全同）、AArch64 普通程序 `.s` 零变化、hof 程序经 clang 交叉汇编产出 ELF aarch64 `.o`（42 个 thunk 符号）。新增 R15 相关断言（`hof.dial` golden）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告，不打 tag，docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3am 自研 asm 后端补齐 `spawn`/`join`/`detach` 真多线程（双后端一致性补全，补齐并发最后一块）**：承接 e3ag~e3al 双后端一致性主线，补齐并发族的真多线程部分，与上一批 channel（单线程收发）形成完整并发闭环。确凿缺口：`spawn/join/detach` 在 LLVM 完整支持（`codegen.rs:5109/5139`，`dial_thread_argcell`+合成 `__dial_thunk_<fn>` 入口+`dial_thread_new`），asm 却把它们归为「并发/高阶函数后续相位」`BKind::Unsupported` 明确拒绝。审计确认硬前置全齐：C runtime 的 `dial_thread_{new,argcell,join,detach}` 符号就位、`FuncAddr` 可取其名函数代码地址、`CallIndirect`/普通 `Call` 的支持（fnptr golden 已验证）、`drop`/`close` 已支持 Thread→join。核心难点是**合成线程入口 thunk**：C 的 pthread 只认统一签名 `void(*)(void*)`，而 Dial 入口函数参数类型各异——在 `lower_program` 遍历完所有函数后，为每个 spawn 目标生成独立 `IrFunction __dial_thunk_<fn>`（从 argcell 的 8 字节槽按形参类型 Load 实参→直接调用目标函数→对移交的 ARC 形参各做一次 RcRelease）。`spawn` 写端按类型移交所有权（string/array/map/set/kvmap/error：move 进槽+源槽清零+标 moved；channel：retain+1 共享；标量/float/裸指针：按值 store），先写满所有槽再 `dial_thread_new`（对齐 LLVM 的并发安全基石：pthread_create 之前实参已就位）；`join/detach` load 句柄→C 调用→槽清零（join 另标 moved 防二次回收，二次 join/detach 由 C 侧 HEAP_ASSERT Fail-Fast 而非 UAF）。实测 worker 经 channel 回传结果（string move + channel 共享）+ join、标量/句柄混合实参 + 多 worker + join/detach 全场景双后端 stdout 逐字一致（`a1` / `joined` 两行；`x=42` / `p!,q!` 两行），无 crash/double-free；`-O0..-O3/-g` 各级别一致。A/B：8 个 golden 程序（含 fnptr）`.s` 与 OLD 逐字节一致、AArch64 普通程序零变化；spawn 程序经 clang 交叉汇编产出 ELF aarch64 `.o`。新增 R14 测试组（channel 回传闭环 + 标量/多 worker/detach + 各 O 级别）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3al 自研 asm 后端补齐 channel 原语族（双后端一致性补全，单线程闭环）**：承接 e3ag~e3ak 主线，补齐并发原语中**单线程可测**的全部 channel 接口，与 is_null/drop 形成完整所有权闭环。确凿缺口：`channel/recv/try_recv/recv_timeout/closed/done/len/move_to` 在 LLVM 完整支持（`codegen.rs:4984-5160`），asm 却把它们和 spawn/channel 一起批量归入「并发/高阶函数后续相位」`BKind::Unsupported` 明确拒绝——实测完整闭环程序 LLVM 输出 `0 false false 1 hello 1 world 0 true true true done`、asm 直接 Fail-Fast 拒绝。审计确认 C runtime 12 个 `dial_channel_*` 符号齐备、`IrInst::FuncAddr` 可取 C 函数地址、`rc_kind(Channel)=Generic` 已支持 ARC release、`Local.moved` 记账就位。本批新增 `lower_channel_call`：`channel(cap)`→`dial_channel_new(cap, FuncAddr dial_rc_release)`（对齐 LLVM elem_release_fn，嵌套 channel 元素属已知边界暂不覆盖）；`recv/try_recv/recv_timeout`→C 调用（弹出即移交所有权、不 retain）；`closed/done`→C 调用 + `ICmp(Neq, r, 0)`→严格 bool；`len`→`dial_channel_len`；`move_to(var,ch)`→load 槽值 + `dial_channel_send` + 原槽清零 + 标记 moved（零拷贝所有权移交，零 retain/release）；扩展 `close` 支持 channel→`dial_channel_close` 且**不标 moved**（对齐 LLVM，channel 受 ARC 管理，标记会跳过 release 而泄漏）。实测完整闭环双后端 stdout 逐字一致（含 EOF 后 `recv` 返回 NULL 被 `is_null` 判 true）、`recv_timeout` 超时返回 NULL 一致、`-O0..-O3/-g -O3` 一致。A/B：7 个 golden 程序 `.s` 与 OLD 逐字节一致、AArch64 普通程序零变化；channel 程序经 clang 交叉汇编产出 ELF aarch64 `.o`。新增 R13 测试组（完整闭环 + recv_timeout + 各 O 级别）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3ak 自研 asm 后端补齐 `drop(x)` 所有权释放（双后端一致性补全）**：承接 e3ag~e3aj 双后端一致性主线，补齐所有权系统的核心接口 `drop`——提前释放一个变量的所有权（move 语义）。确凿缺口：`drop` 是 Dial 所有权/资源管理的关键环节（rawptr「谁申请谁释放」、ARC 句柄提前归还、with 块自动释放的同源机制），LLVM 后端完整支持（`codegen.rs:4939`：File→close、Thread→join、RawPtr→free、其余 ARC→release、标量 no-op，释放后标记 moved 防双释放），asm 后端却把它和 spawn/channel 一起批量归入「并发/高阶函数后续相位」`BKind::Unsupported`，明确 Fail-Fast 拒绝。本批在 `lower_call` 把 drop 接入 `lower_special_call` 专用路径，新增 drop 分支：参数（语义层已保证是 Expr::Var）按静态类型分派——File→`dial_file_close`、Thread→`dial_thread_join`、RawPtr/VolatilePtr→`dial_raw_free`（这三类先 load 出槽里的句柄值再传 C 函数）；ARC 句柄→`RcRelease(slot, rc_kind)`；struct/标量→no-op（对齐 LLVM `release_value` 的 `_ => return`，struct 字段由作用域退出单独管理，提前 StructRelease 会 UAF）；释放后标记 Local.moved 使作用域退出跳过二次释放，已 moved 再 drop 幂等 no-op。实测 rawptr/map/标量/数组/循环内/返回字符串后 drop 全场景双后端 stdout 逐字一致（`42 7 3 0 10 20 hello world done`）、无 double free/UAF；`-O0..-O3/-g -O3` 各级别一致；FnPtr/不透明句柄在语义层编译期拒绝（两端同源）。A/B：7 个 golden 程序 `.s` 与 OLD 逐字节一致（md5 全同）、AArch64 普通程序零变化；drop 程序经 clang 交叉汇编产出 ELF aarch64 `.o`。新增 R12 测试组（全场景 + 严格输出 + 各 O 级别 + FnPtr 拒绝）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3aj 自研 asm 后端补齐 `is_null(x)` 句柄判空（双后端一致性补全）**：承接 e3ag/e3ah/e3ai 双后端一致性主线，补齐又一个 LLVM 支持却 asm 明确 Fail-Fast 拒绝的内建。确凿缺口：`is_null` 是 FFI/句柄错误路径判空的核心接口（如 `HttpResponse`/`HttpConn` 空句柄判别），LLVM 后端完整支持（`codegen.rs:5031`，ptrtoint + icmp eq null → i1），asm 后端却因被批量归入「并发/高阶函数后续相位」而走 `BKind::Unsupported` 明确报错；实测 `strchr` 返回 NULL/非 NULL 的程序 LLVM 输出 `true/false`、asm 直接 Fail-Fast 拒绝。本批在 `lower_call` 拦截 is_null、新增 `lower_is_null`：asm 的所有句柄值本身就是指针语义的 8 字节槽值（IrType::Ptr/Handle），直接 emit 一条 `ICmp(Eq, x, 0)` 即得严格 0/1 的 bool（IrType::I64），零函数调用、零运行时依赖，等价于 LLVM 的 ptrtoint+icmp。准入名单由语义层 `is_nullable_type` 统一把关（两端同源：String/Array/Map/Set/KvMap/Error/Channel/Thread/File/Opaque，标量编译期拒绝），参数按自身静态类型 lower 避免误当标量。实测多句柄 + 条件分支场景双后端 stdout 逐字一致（`false/true/false` + `b is null`），结果严格是 `false/true`；`-O0..-O3/-g/-g -O3` 各级别一致。A/B：7 个 golden 程序 `.s` 与 OLD 逐字节一致（md5 全同）、AArch64 普通程序零变化；is_null 程序经 clang 交叉汇编产出 ELF aarch64 `.o`。新增 R11 测试组（多句柄判空 + 严格布尔 + 各 O 级别 + 标量编译期拒绝）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3ai 自研 asm 后端补齐 `assert` / `panic`（Fail-Fast 哲学 + 双后端一致性补全）**：承接 e3ae 除零检查、e3ag/e3ah 双后端一致性主线，补齐 `assert`/`panic` 这组用户级 Fail-Fast 内建。确凿缺口：`assert(cond[,msg])`/`panic(msg)` 是用户主动触发 Fail-Fast 的核心接口，LLVM 后端完整支持（`codegen.rs::gen_builtin` + `emit_fail_loc`，打印「消息 + file:line:col」后 abort），asm 后端却走 `BKind::Unsupported` 明确 Fail-Fast 报错（无位置追踪），且 `examples/builtin_demo.dial` 的 `assert` 只敢跑 LLVM。本批对齐 LLVM `emit_fail_loc`：在 `lower_call` 拦截 assert/panic、新增 `lower_assert_panic` 专用路径——`assert` 生成条件 CFG（真→ok 块返回 0 继续、假→fail 块），`panic` 无条件终止；fail/panic 分支把 Dial string 经 `dial_string_cstr` 转 `const char*`（跳过 8 字节 ARC 头，与未捕获异常路径同款），再调 C runtime `dial_assert_fail_loc(msg,file,line,col)` / `dial_panic_loc(...)` 打印诊断后 abort。位置取 `lower_stmt` 入口记录的 `stmt_pos`（纯内存赋值、不产 IR），文件名是编译期常量串（intern 进 .rodata），行列是编译期常量整数——运行期零额外开销，与 LLVM 逐字对齐；`lower_program` 新增 `src_file` 参数贯通源文件路径。断言默认消息 "assert 失败" 与 LLVM 一致。实测三种失败情形（自定义消息/默认消息/panic）双后端 stderr 诊断与 `file:line:col` 逐字一致、真实退出码一致（assert 132 / panic 134）、终止后哨兵未打印；assert 通过时双后端 stdout 逐字一致；`-O0/-O1/-O2/-O3/-g/-g -O0` 各级别位置诊断不漂移。A/B：6 个 golden 程序 `.s` 与 OLD 逐字节一致（md5 全同）、AArch64 普通程序零变化；assert/panic 程序经 clang 交叉汇编产出 ELF aarch64 .o。新增 R10 测试组（assert 通过/失败双参数/panic 双后端诊断+位置+退出码+哨兵、各 O 级别与 -g 不漂移）。护栏：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml` 1.10.0/零新依赖/LLVM 路径/AArch64 普通程序零改动，构建零警告。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3ah 自研 asm 后端补齐 FFI 内建 `bitcast<T>(x)`（双后端一致性补全）**：承接 e3ag，继续"代替 LLVM"的非墙钟硬指标补全。确凿缺口：`bitcast` 是 FFI 刚需的底层位模式重解释（整数地址↔裸指针、float 位模式），LLVM 完整支持（`codegen.rs::bitcast` + ptrtoint/inttoptr），asm 后端却走 `BKind::Unsupported` 明确 Fail-Fast 报错（有专门负例 `tests/bad/bitcast_*`，现有 FFI 用例 `ffi_export_demo::cast_probe` 只敢跑 LLVM）。用户明确要求"工具不合适就造新的支持，否则支持上了也不是好东西"。本批在 `lower_call` 拦截 bitcast、新增 `lower_bitcast`：标量复用 asm 已有的 8 字节槽 `reinterpret_bits`（store I64→load target），位宽校验用 `field_size×8`（标量）/ `layout_size_align`（struct），与 LLVM `llvm_bit_width` 逐项对齐；**struct↔struct 同尺寸造轻量零拷贝槽地址重解释**——asm 的 struct 值是「指向栈槽的 Ptr vreg」，地址不携带类型标签，`FieldAddr` 偏移完全由指令里的 `struct_name` 显式查布局得出，故直接返回源槽地址、下游按目标 struct 解读同一块内存即可（等价 LLVM 同尺寸聚合零开销 bitcast，且这正是 LLVM IR verifier 拒绝 `Invalid bitcast` 而 asm 超越 LLVM 的场景）。类界限对齐 LLVM（ptr/int↔struct 拒绝、array 因堆句柄明确拒绝、simd 口径不一致拒绝），位宽不匹配 Fail-Fast 带 `[asm 后端]`。实测标量位模式（int↔float/int32↔float32/int↔rawptr）双后端输出逐字一致，struct↔struct asm 零拷贝正确（7,9），非 bitcast 程序 `.s` 逐字节不变、AArch64 零变化。新增 R9 测试组（标量双后端比对 + struct asm 单侧覆盖 + 负例拒绝）。护栏：冻结文件/依赖/版本/AArch64/LLVM 路径零改动，构建零警告，docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3ag 自研 asm 后端补齐编译期布局内建 `sizeof`/`alignof`/`offsetof`（双后端一致性补全）**：上一批 e3af 把墙钟优化 pass 证伪后，转向"代替 LLVM"的非墙钟硬指标——双后端语义一致性。确凿缺口：`sizeof<T>()`/`alignof<T>()`/`offsetof<T>(field)` 是 FFI 刚需的编译期常量（零运行时指令），LLVM 用 `TargetData::get_abi_size` 支持，而 asm 后端走 `BKind::Unsupported` 明确 Fail-Fast 报错（有专门负例）。本批复用 asm 侧早已为结构体字段地址固化、与 LLVM DataLayout 逐字节对齐的 ABI 布局"单点真理"（`ir.rs::struct_layout`/`field_size`/`field_ir`），在 `lower_call` 拦截这三个内建、编译期算出整数常量并返回 `Value::Imm`（与整型字面量同形，零运行时指令），不碰 `codegen.rs`/`ast.rs`、零新依赖、LLVM 路径零改动。**关键正确性修正**：Dial 有 `packed struct` 语法，LLVM/C 口径 packed align=1，而 `struct_layout` 对 packed 仍返回自然对齐（如 8）——从 `prog.structs/cstructs` 收集 packed 名集合，packed 时 size 用 `struct_layout(true)`、align 固定 1；simd 在 LLVM 按向量宽度算、asm 无法对齐，明确 Fail-Fast 拒绝（绝不静默给错误值）。实测双后端输出逐字一致（标量/普通 struct/packed/内建容器/offsetof 全部对齐，含 `PktHdr align=1`），非布局程序 `.s` 逐字节不变、AArch64 零变化。负例 `unsupported.dial` 换为 `@asm`（LLVM 通过、asm 带 `[asm 后端]` 拒绝）；新增 R8 双后端一致性组 + core1 golden 正向断言。护栏：冻结文件/依赖/版本/AArch64/LLVM 路径零改动，构建零警告，docx 确定性。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3ae 自研 asm 后端 Fail-Fast 整数除零检查：`--safety` 分级贯通（代替 LLVM 的哲学对齐）**：用户指出"要代替 LLVM，不能不支持 fail-fast 哲学"。全面审计确认：数组/字符串越界、空指针、channel 关闭、int→byte/int32 范围校验等已通过共享 C runtime 覆盖并按 `DIAL_SAFETY` 分级，SIMD lane 除零与 INT_MIN/-1 溢出 LLVM 自身也不检查——**唯一真实缺口是整数 `/`、`%` 除零**：LLVM 后端有 `emit_div_zero_check`（safety≠fast 时 `divisor!=0 else trap`），而自研 asm 后端 `compile_asm` 根本没接 `SafetyMode`，`Bin{Div,Mod}` 直发 `cqo;idivq`/`sdiv`，x86 靠裸 SIGFPE、`--safety fast` 对它毫无影响，AArch64 `sdiv` 除零更是**静默返回 0**（真正产出错误数据，违反 §20 铁律）。本批把 `SafetyMode` 从 main 贯通到 `compile_asm`→`emit`→`emit_function`→`FnEmit`，在通用 Div/Mod 发射前插入 `cmpq $0, divisor`（x86，用 cmp 而非 test 以兼容内存操作数）/ `cbnz`（AArch64）+ 分级陷阱：Debug→`int3`/`brk #1`（SIGTRAP rc133），Safe→`ud2`/`brk #0`（SIGILL rc132），ReleaseFast 不插检查（x86 退回硬件 SIGFPE、AArch64 维持原样）；常量除数 0/1 被 const_div_pow2/magic（要求 d≥2）挡在通用路径之外，零误报。实测三档退出码 133/132/136、均无 9999 哨兵；`.s` 标记 debug 含 int3、safe 含 ud2、fast 全无；AArch64 debug/safe 含 cbnz+brk、fast 无；正常除法（含负被除数）三档与 LLVM 输出一致。新增 R7 专项（退出码/哨兵、.s 标记分级、AArch64 结构性、正常除法一致性、越界类全审计佐证）。**零改动护栏**：`src/codegen.rs`/`src/ast.rs`/`Cargo.toml`/LLVM 路径/依赖/版本零改动；非除法程序 `.s` 逐字节不变；**唯一破例**——AArch64 含除法程序的 `.s` 新增检查指令（用户已授权，正确性优先，rationale 如实声明）。证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

- **M7-e3ac/e3ad 自研 asm 后端优化级别参数 `-O0/-O1/-O2/-O3` + 移植 LLVM InstCombine 恒等式（支柱① O3 管线）**：此前自研后端所有优化（fold/cse/dce/ivr/GRA/ISA 指令融合）无条件全开、没有任何优化级别参数，而 LLVM 后端也仅在 `--fast` 下 O3——缺少编译器必备的 `-O` 参数界面。本批引入对齐 LLVM 语义的 `-O0`（无 Dial 优化，真·调试）/`-O1`（基础 IR Pass）/`-O2`（默认：既有全部优化，逐字节零回归）/`-O3`（+LLVM 风格代数恒等式），支持 `-O N`/`--opt-level=N` 写法，非法级别 Fail-Fast 拒绝；仅作用于 asm 后端，LLVM 路径给提示且零改动。O3 从 LLVM InstCombine 借鉴一组保守可证等价的整数恒等式（`a+0→a`、`a*1→a`、`a|0→a`、`a<<0→a`、`a&-1→a`、幂等 `a&a→a`/`a|a→a`），仅 x86-64/ty=I64，跑完补 DCE。新增 R6 专项 7 项；A/B：默认与 -O2 逐字节一致（md5 40548450…）、AArch64 各级别 .s 全同、恒等式密集循环 O3 比 O2 快约 1.23x（0.27s→0.22s），四级输出一致。冻结文件/依赖/版本/AArch64/LLVM 路径零改动。

- **M7-e3ab 自研 asm 后端 DWARF 精确宽度类型 DIE（支柱② DWARF 补全）**：e3z 的类型 DIE
  只有 int/float/bool/string/ptr 五类，FFI 精确宽度标量 `int32`(栈槽低 4 字节)、`byte`(低
  1 字节无符号)、`float32`(低 4 字节) 被错误映射成 int(8 字节有符号)/int/float(8 字节)，
  gdb 按 8 字节读会串入高位未定义字节，`print`/`ptype` 显示错误宽度与符号性。本批新增
  `int32`(4/signed)/`byte`(1/unsigned)/`float32`(4/float) 三个 base_type DIE，扩展 `DwarfType`
  分类并让 `dwarf_type`(AST) 精确映射 Int32/Byte/Float32、`dwarf_type_ir`(IR) 映射 I32/F32
  （I8 保留 Int 避免 bool 形参回归）。gdb 实测 `ptype`/`print`：int32=7、byte=255、float32=1.5
  全部正确；新增 DIE 只在 DWARF 元数据段，机器码段与非 -g `.s` 与新编译器前逐字节一致，
  dwarfdump 无 error。R 组新增 R5 专项（类型 DIE byte_size/encoding + gdb ptype/print）。
  宪法约束不动（codegen.rs/ast.rs/LLVM 路径/依赖/版本零改动）。证据链见
  [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **M7-e3aa 自研 asm 后端多文件 DWARF source 映射（支柱② DWARF 补全）**：`import "./x.dial";`
  合并编译多个源文件时，`-g` 生成的 DWARF 此前把所有函数的 `DW_AT_decl_file` / 行表都指向
  主源文件，子模块函数（如 `helper.dial` 里的函数）按各自文件的相对行号却错指主文件，
  gdb `break helper.dial:N` 打不上断点。本批让 `frontend()` 已有的 `fn_origin` 沿
  `compile_asm → codegen_asm::emit` 传到 DWARF 发射：为每个不同源文件分配 `.file N`（主文件
  恒为 1），每个函数按来源发 `.loc N` 与 `DW_AT_decl_file N`，其局部变量继承所属文件。
  POC 确认修复前 `helper_add` 的 decl_file 错误指向 `main.dial`，修复后指向 `helper.dial`、
  gdb `break helper.dial:2` 精确命中且 backtrace 分文件显示。R 组新增 R4 专项（静态 decl_file
  + 行表 + gdb 子模块断点）。单文件 / 非 -g / AArch64 零变化（单文件 `.s` 新旧编译器 md5 一致、多文件
  非 -g `.s` 逐字节一致）。宪法约束不动：`src/codegen.rs`/`src/ast.rs`/LLVM 路径/依赖/版本零改动。
  证据链见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。
- **LSP 再强化：诊断一体化 / 增量同步 / 文件监视 / 文档内高亮**（README §28.7.2）：
  - **码表同源**：`src/diagnostic.rs::stable_code(msg, fallback)` 公开，CLI `report()`
    与 LSP `diagnostic()` 共用；诊断新增 `codeDescription.href`。同一个错误在
    命令行 stderr 与编辑器红线里是同一个 `code`。
  - **增量同步**：`textDocumentSync.change` 由 `1` → `2`；`didChange` 逐条应用
    `contentChanges`，带 `range` 的经 `index::utf16_to_char_col()` 把 UTF-16 列换成
    字节偏移后打补丁，不带 `range` 的全量 change 仍兼容。
  - **文件监视**：`workspace/didChangeWatchedFiles` 收到后对全部已打开文档重跑索引与
    诊断；服务端不擅自注册全局 watcher，不侵入客户端配置。
  - **文档内高亮**：新增 `textDocument/documentHighlight`（无条件广播能力），
    光标下标识符经 `identifier_at()` 取名、编译器词法器扫同名 `Ident` token 出 range。
  - **防漂移**：README §28.4「增量文档同步没做」表格行、§28.7.1 的
    `{openClose, change: 1, save}`、§28.2 能力协商表三处同步改写；
    CHANGELOG v0.98.1 历史条目加注 v1.10 变更。
  - **回归**：三个 LSP 冒烟脚本新增 7 条断言；`lsp_full_smoke.py` 能力清单与
    CONFORM 表补 `documentHighlight`；全量 **612 ✅ / 0 ❌**；stdout 协议流纯净性
    静态 / 动态双守卫仍全过。

- **工具链再强化：`doctor` / `deps` / 统一诊断 / `watch` / `clean` / `completion`**（README §34.11）：
  - 新增 `src/toolchain.rs`：五个命令共用 `COMMANDS` 元数据（`help` / `completion` /
    `doctor` 三处命令清单的唯一真相）与本地源码递归；`doctor` 自检编译器版本 /
    C 编译器与 `ar` / 包缓存 / 已安装包 manifest + 逐文件 SHA256 / `dial.toml` /
    包内 `src` 解析；`deps` 展开三级 import 依赖图（本地源码 / 包内 src / 包名，
    含循环检测）；`watch` 轮询主文件 + 本地模块并限拍重跑 `check / build / test`；
    `clean` 默认 dry-run、只删 `.o` / `.bc` / `.ll` / `.dialpkg`；`completion` 生成
    bash / zsh / fish 补全脚本（缓冲后一次性写出，管道被 `head` 截断不 panic）。
  - 新增 `src/diagnostic.rs`：统一前端错误渲染。人读模式**原消息一字不动**，追加
    `文件:行:列` + 稳定错误码 + 源码行 + caret；`DIALC_DIAG_FORMAT=json` 输出一行
    JSON（`code / line / col / snippet / caret`）供机读，stdout 绝不被污染；只解析
    行列与错误码，**绝不重新判定错误**。
  - `main.rs`：三处前端错误出口改调 `diagnostic::report()`；命令分派与 `help` 补五个
    新命令。`Cargo.toml` `1.10.0`、`abi = 6`、既有命令默认行为、包契约与 sha256
    口径全部不变；`doctor` 只校验不修复，`clean` 绝不碰 `.dial` 源码与包缓存。
  - **回归**：Q 组新增 19 条断言，全量 **605 ✅ / 0 ❌**（`run_tests.sh` 实测；
    §34.10 记 587 ✅ 为按组累加口径，本节起统一实测：586 ✅ + 19 = 605 ✅）；
    `cargo build --release` 零警告。

- **工具链强化：`dialc fmt` 与 `dialc pack`**（README §34.10）：
  - **`dialc fmt` 间距规范化**：新增 `src/dialfmt.rs`，把每行扫成 `Word / Op / Raw`
    片段，规范运算符两侧、逗号 / 分号 / 冒号后、控制关键字与 `(` 之间、大括号两侧
    的空白；字符串 / 字符 / 字节字面量与行注释、跨行块注释作为 `Raw` 原样保留。
    三条硬契约当场自检：token 序列一致、字面量与注释原文一致（`protected_spans()`）、
    幂等；任一不满足即拒绝写入。新增 `--diff` 逐行对照（与 `--check` 一样不写入、
    有差异非零退出）。LSP 编辑器内格式化仍保持非破坏性，两者刻意分工。
  - **`dialc pack` 少手工**：抽出 `pkgbuild::plan()` 供 `pack` / `--dry-run` /
    `--json` 共用同一份计划（预览与产物逐字节一致）。新增 `--init` 自动扫描
    `*.diali` / `lib/<triple>/*.a` / `src/*.dial` 生成**显式声明**的 `dial.toml`
    草稿（已有清单拒绝覆盖）、`--dry-run` 预览入库文件与 SHA256 并提示「磁盘有但
    清单未登记」的文件（绝不自动塞进包）、`--json` 机器可读清单、
    `--verify <包.dialpkg>` 用安装期同一套 `parse_manifest` + `verify_archive`
    规则校验已有包（篡改即非零并给出期望 / 实际 SHA256）。
  - 缺字段 / 缺文件诊断补具体路径与补齐提示；默认 `pack` 行为、manifest 字段、
    `[src]` 显式声明规则、`abi = 6` / `version = 1.10.0` 全部不变。
  - **回归**：Q 组新增 11 条断言（4 条 fmt + 7 条 pack），全量
    **576 ✅ → 587 ✅ / 0 ❌**；`cargo check --release` 零警告。

- **`gui.dialpkg`：第 15 层快捷键 / 命令系统**（README §38.21）：
  - 新增应用自持的纯内存表 `GuiHotkey`（**不挂进 `GuiWindowImpl` 槽位**）：最多
    64 条绑定、命令 id `[0, 256)`，编译期定长、内存上界确定；命令默认全启用。
    配套 **18 个 `extern fn`**，`gui.diali` 达 **279 个签名**（旧 261 个一字不动）。
  - `bind` 冲突 Fail-Fast（同一 `(key, mods)` 不静默覆盖）/ `rebind` 覆盖式 /
    `unbind` 压实顺序 / `lookup` 读绑定不看启用态 / `dispatch` 只发启用且命中的
    命令 id（窗口事件主入口）/ `clear` 复位启用态但保留 tag /
    `key_at`·`mods_at`·`cmd_at` 逐条读回 / `draw` 先擦固定面板再画前 8 行、
    空表零像素影响、重绘幂等。
  - **命令语义归应用**：C 侧绝不保存函数指针、绝不 C → Dial 反向回调；不做系统级
    全局热键、不读键盘布局、不自动执行命令、不接管事件循环。纯 C、零平台符号、
    零时钟、零 DISPLAY 依赖，三平台行为逐字节一致；新增 `dial_gui_hotkey.c`。
  - **OO 层**：`gui_oo.dial` 追加 `impl GuiApp.hotkey_new` 工厂与 `impl GuiHotkey`
    17 个方法（共 18 个），封装层仍零 `extern fn` 声明；AD 组契约审计由
    **260 / 261** 同步为 **278 / 279**（`278 ↔ 278` 精确相等）。
  - **回归**：AA 组新增 5o 块——`tests/pkg/use_gui_hotkey.dial` 零 DISPLAY 跑
    166 条断言（修饰键精确匹配 / 冲突拒绝 / 覆盖 / 压实 / 容量 64 上界 / 启用态 /
    CRC 幂等），全绿；全量 **574 ✅ → 576 ✅ / 0 ❌**。
  - **宪法不变**：只追加 `extern fn`，`abi = 6` / `version = 1.10.0` 不变；
    三平台 `.a` 重建后零链接期 GUI 平台符号、18 个新符号三份齐全。

- **`gui.dialpkg`：第 14 层窗口级覆盖层自动集成**（README §38.20）：
  - `GuiWindowImpl` 新增 5 个**借用槽位**（`scroll` / `context` / `tooltip` /
    `statusbar` / `toast`），配套 5 × 3 = **15 个 `extern fn`**
    （`dial_gui_window_set_*` / 读回 / `clear_*`），`gui.diali` 达 **261 个签名**。
  - `gui_window_draw()` 在控件树 / 顶栏 / 弹窗 / 对话框之后按**固定 z-order**自动
    叠加：滚动条 → 上下文菜单 → 悬浮提示 → 状态栏 → 通知气泡；空槽位跳过绘制，
    既有各层离屏 CRC 基准逐字节不破。
  - **借用语义**：窗口只持指针、不拥有不释放；`dial_gui_window_close()` 不再释放
    `win->scroll`（原第 9 层「窗口拥有」的语义统一改为借用），`clear_*` 只清槽位，
    对象生死归应用。C 侧用 `DEFINE_WINDOW_OVERLAY(lname, Type, field)` 宏实例化
    5 层，纯 C、零平台符号、零时钟、**绝不 C → Dial 反向回调**；本层不新增 `.c` 文件。
  - **OO 层**：`gui_oo.dial` 追加第二个 `impl GuiWindow` 块（15 个方法），封装层仍
    零 `extern fn` 声明，AD 组契约审计由 **245 / 246** 同步为 **260 / 261**（精确相等）。
  - **宪法不变**：只追加 `extern fn`，旧 246 个签名一字不动；`abi = 6` /
    `version = 1.10.0` 不变；三平台 `.a` 重建后零链接期 GUI 平台符号。
  - **回归**：AA 组新增 5m / 5n 两块——`tests/pkg/use_gui_overlays.dial` 无 DISPLAY
    路径验证 15 个 API 空窗口 Fail-Fast + 5 类对象独立 `free`（应用未创建如实
    `skipped`），Xvfb 实窗路径验证空槽位 CRC 稳定 / 挂载后 CRC 改变且重绘幂等 /
    清空后 CRC 回基线 / `app_free` 后对象仍可 `free`；两条路径 `bad = 0`。
    全量 **571 ✅ → 574 ✅ / 0 ❌**。

- **`dialpkg` 新增 `src/` 目录：随包分发的 Dial 源码模块**（README §25.11）：
  `.dialpkg` 从「机器码 + 签名」扩展为「机器码 + 签名 + 随包 Dial 源码模块」。
  - **清单显式声明**：`dial.toml` 新增 `[src]` 段（`模块名 = "src/x.dial"`），
    键是 `import` 裸名、值必须位于 `src/` 下且以 `.dial` 结尾、文件必须真实存在；
    `dialc new` 骨架预建空 `src/`。未知段 / 重复段 / 非法路径一律打包期 Fail-Fast。
  - **装包即用**：`manifest.txt` 新增**可选** `src` 映射，前端 `collect_sources`
    把 import 解析扩为三级（① 本地源码 → ② 包内 `src/` 模块 → ③ 包名）；
    ② 级与本地模块走同一套递归 / 去重 / AST 合并，模块内部 `import "包名"`
    照常汇总到 ③ 级，故源码模块能直接调用包里的 `extern fn`。
  - **与机器码同级保护**：`src/*.dial` 进 `sha256` 字典、安装期逐文件校验，
    且每次 ② 级读取前再校验一次（`find_src_module` → `verify_sha256`），
    篡改包内源码被当场拦下；同名模块被多个包声明时报错而非静默任选。
  - **向后兼容**：`src` 字段缺失即空映射，旧包 / 旧脚本产物零改动照常可用；
    `abi` / `version` / 既有签名一律不变，包仍是纯数据、零构建脚本、零外部编译器。
  - **回归**：Q 组第 10) 块 10 条断言全绿（骨架预建 / 产物含 src + manifest.src +
    SHA256 / 安装落地 / 不复制文件直接 `import "qsrc_oo";` 联动包函数 = 42 /
    篡改拦截 / 本地同名文件优先 / 4 条非法声明 Fail-Fast），
    全量 **560 ✅ → 570 ✅ / 0 ❌**。

- **`gui.dialpkg`：第 8 层 OO 封装层纳入包分发**（README §25.11 / §38.14）：
  - `gui_oo.dial` 从 `packages/gui/` 移入 `packages/gui/src/`，`dial.toml` 新增
    `[src]` 段登记 `gui_oo = "src/gui_oo.dial"`；`pack` 产物随之多出
    `src/gui_oo.dial` + `manifest.src` 映射 + 逐文件 SHA256。
  - **装包即用**：装好 `gui.dialpkg` 后 `import "gui_oo";` 直接从包缓存 `src/`
    展开，不再需要把源码复制到用户项目；项目里同名 `.dial` 仍按三级优先覆盖包内副本。
  - **宪法不变**：`gui_oo.dial` 仍是纯 Dial 源码、零 `extern fn` 增量、不碰三平台
    `.a`、`abi = 6` / `version = 1.10.0` / 旧 154 个签名一字不动。
  - **回归**：AD 组保留「本地副本」路径并新增 2.5) 块——不复制任何文件直接编译
    `use_gui_oo.dial`，输出与本地副本版**逐字节一致**；全量 **570 ✅ → 571 ✅ / 0 ❌**。

- **B1「`impl` 方法块」语言特性落地**（README §37.12）：`[v1.10]` 发布后增量，
  这是 v1.04 第三批曾记为「明确不碰——暂用 `fn` 替代」的能力，本轮真正补上。
  让用户 `struct` 与包不透明句柄（`Type::Struct` / `Type::Opaque`，如 GUI 的
  `GuiWindow` / `GuiMenu`）能写 `impl T { fn m(self, ...) { ... } }`，调用
  `obj.m(args)` 编译期降级为静态函数调用（self 即首参）——`win.draw()` 就是
  `dial_gui_window_draw(win)` 的面向对象写法，**包 `.diali` 契约一个字都不用改**。
  - **宪法**：不新增关键字（`impl` 走 `Ident("impl")` + 类型名 + `{` 识别，与
    `test` 块同例，旧程序把 `impl` 当变量名/函数名仍合法）；`obj.m(args)` 与手写
    普通函数调用完全同构，**编译产物 / ABI 逐字节不变**，不碰 `STDLIB_ABI`、
    `.diali`、C Runtime；旧签名一字不动。
  - **实现（最小侵入、零重复）**：parser 把 impl 方法降级为普通顶层 `Func`
    （符号名 mangle 成 `__impl_T_m`，`self` 注入首参）；语义 `type_of` 按 obj
    静态类型做面向用户的参数校验（诊断显示 `T.m`、不含 mangle 名、不把 self
    计入参数个数，零隐式转换铁律照旧）；codegen `gen_method_call` 用
    `make_impl_call` 把 Method 就地改写为 `Expr::Call` 并递归复用整条调用生成链
    （签名校验 / 默认实参 / 实参求值 / C ABI 宽度适配 / `build_call` / 返回值
    全部零改动复用）。保持 Method 形态、不做全局 AST 脱糖，故 move / 引用分析
    / `desugar_expr` 等遍历 pass 天然适用、零改动。
  - **容器特例收窄**：`push` / `set` / `put` / `add` 也是内建容器 COW 原地写回
    协议名，用 `is_impl_receiver` 精确排除具名类型接收者，避免同名 impl 方法被
    误当成容器方法把返回值错误 `build_store` 回变量槽；内建容器方法行为一字未变。
  - **现役边界（如实标注，不夸大）**：只支持具名类型；`self` 按值传入副本，
    方法内 `self.f = ...` **不回写调用者**（Dial struct 值语义，与 Rust `&mut self`
    不同）；支持默认实参 / 普通形参 / 返回任意类型 / 方法间调用 / 返回结构体再调
    方法；**暂不支持**同类型变参 `...xs`（parser Fail-Fast 提示改用 `xs: T[]`）、
    C ABI 导出、关联函数（无 self 的 `T::new()`）、标量/内建容器/裸指针挂 impl。
  - **回归**：新增 AC 组（第 29 组）——`tests/impl/impl_struct.dial`（self 字段
    读写 / 方法间调用 / 返回结构体 / 表达式位置 / `impl` 作标识符 / 容器 `push`
    同名隔离 / 值语义边界）、`impl_default.dial`（默认实参）、
    `impl_gui_handle.dial`（包不透明句柄 `GuiMenu` 方法，无 DISPLAY 纯内存逻辑）、
    `tests/bad/impl_*.dial` 六个 Fail-Fast 负向 + 诊断面向用户断言。
    全量 **538 ✅ → 549 ✅ / 0 ❌**，A–Z + AA + AB + AC 二十九组全绿。

- **`int32` / `float32` 完整一等类型 + FFI 精确 32 位宽度**（README §5.8）：
  解决「只有 `int`(i64) / `float`(f64)，传给按 `int32_t` / `float` 读取的 C 侧
  时静默读到垃圾数据」这一隐蔽数据损坏。
  - 词法 / AST / 语义 / codegen 全链路新增 `int32`(LLVM `i32`) 与
    `float32`(LLVM `f32`)；值类型、无 ARC、可声明变量 / 参数 / 返回值，
    可参与**同类**算术与比较，可 `print` / `print_raw` / `str()` / 插值。
  - **零隐式转换铁律不变**：与 `int` / `float` 之间必须显式
    `int_to_int32` / `int32_to_int` / `float_to_float32` / `float32_to_float`；
    前一条走 C 侧范围校验，越出 int32 值域当场 Fail-Fast，绝不静默截断。
  - **FFI 精确宽度**：`ffi_type_to_llvm` / `cstruct_field_llvm` 按真实 i32 / f32
    生成签名与 `extern struct` 字段；`abi_adapt` 补 `f32 ↔ f64` 的
    `fpext` / `fptrunc`；`@asm` 输入 / 输出白名单纳入新类型；DWARF 给出 32 位
    精确宽度与编码；LSP 语义高亮归入类型关键字。
  - 边界如实标注：`int32[]` / `float32[]` 编译期拒绝（8 字节槽位模型），
    `simd<T, N>` 元素仍限原有标量，位运算仍只限 `int`。
  - 回归新增 **AB 组 4 条**：`tests/ffi/scalar32_probe.c` 用真实 `int32_t` /
    `float` 探针，逐项验证参数 / 返回值 / 多参数 / `extern struct`
    `sizeof=24` 与 `offsetof=8/12/16` / 双向字段读写与黄金输出逐字节一致；
    `tests/ffi/scalar32_overflow.dial` 锁定越界 Fail-Fast（哨兵 9999 不打印）；
    `tests/bad/s32_*.dial` 五个负向用例锁定零隐式转换。
  - 全量回归由 **A–Z + AA 二十七组 / 526 ✅** 扩为
    **A–Z + AA + AB 二十八组 / 535 ✅ / 0 ❌**。

- **GUI 第 8 层：Dial 侧 OO 封装层**（README §38.14）：`packages/gui/src/gui_oo.dial`
  用 **278 个 `impl` 方法**把前七层 + 第 9/10/11/12/13/14/15 层滚动视图·右键上下文
  菜单·悬浮提示·状态栏·通知气泡·窗口级覆盖层自动集成·快捷键 / 命令系统的 279 个
  C 风格 `extern fn` 包成惯用写法（`win.draw()` 即 `dial_gui_window_draw(win)`），按 13 个
  不透明句柄（`GuiApp` / `GuiWindow` / `GuiPainter` / `GuiFont` / `GuiWidget` /
  `GuiMenu` / `GuiDialog` / `GuiScroll` / `GuiContext` / `GuiTooltip` /
  `GuiStatusBar` / `GuiToast` / `GuiHotkey`）分组；**纯 Dial 源码、随包 `src/` 分发（装包即用）、
  零 `extern fn` 增量、`abi = 6` / `version = 1.10.0` 不变、旧 154 个签名一字不动**。
  B1 落地时借 v1.04 本地源码模块机制交付（`import "./gui_oo.dial";`），其后
  `dialpkg src/` 通用能力（§25.11）把本模块纳入 `gui.dialpkg`，装包后
  `import "gui_oo";` 一行即生效；`.diali` 只认声明不能放 impl，故这层始终不改
  `.diali` / C 层 / 三平台 `.a`。
  - **产物不漂移**：`use_gui_oo.dial` 让同一绘制序列分别以 C 风格与方法风格在两块
    离屏画布上执行，`painter_crc32()` **逐字节相等**；因为 impl 在 parser 阶段
    即降级为普通顶层函数、调用点就地改写为 `Expr::Call` 复用整条调用链。
  - **现役边界（如实标注）**：B1 不支持关联函数 → app 自身仍用
    `dial_gui_app_new()`，工厂以 `app.window_new(...)` / `app.menu_new()` 提供
    （self 仅作命名空间）；`self` 是值语义句柄副本（opaque 本质是指针，操作
    作用于同一对象）；两处保留字改名（`select` → `select_range`、
    `label` → `txt`）；LSP 不递归本地源码模块（v1.04 既有局限），
    CLI `build` / `run` / `check` / `test` 不受影响。
  - **回归**：新增 AD 组（第 30 组）6 条断言——契约审计（278 方法全覆盖 278 个
    `extern fn` / `gui.diali` 共 279 个签名 / 封装层零 `extern` 声明）、无 DISPLAY
    87 条、Xvfb 实窗 153 条（顶栏 / 弹窗 / 对话框 / 控件树全走方法）、3 个
    Fail-Fast 负向 + 诊断面向用户；全量 **549 ✅ → 556 ✅ / 0 ❌**（AA 组 +1 来自第 9 层滚动视图离屏回归）。
  - **第 10 层回归**：AA 组再 +1 条（第 10 层右键上下文菜单离屏回归，57 条断言全绿），全量 **556 ✅ → 557 ✅ / 0 ❌**。
  - **第 11 层回归**：AA 组再 +1 条（第 11 层悬浮提示离屏回归，51 条断言全绿），全量 **557 ✅ → 558 ✅ / 0 ❌**。
  - **第 12 层回归**：AA 组再 +1 条（第 12 层状态栏离屏回归，48 条断言全绿），全量 **558 ✅ → 559 ✅ / 0 ❌**。
  - **第 13 层回归**：AA 组再 +1 条（第 13 层通知气泡离屏回归，57 条断言全绿），全量 **559 ✅ → 560 ✅ / 0 ❌**。

- **`gui.dialpkg`：手搓 GUI 十五层**（README §38）：前七层不用 Qt / GTK / SDL、纯 C 自绘；
  第 8 层是 Dial 侧 OO 封装（纯源码、随包 `src/` 分发（装包即用）、零 `extern fn` 增量）。
  ① 地基层（窗 + 事件 + 绘图 + 字体，55 个 `extern fn`）；
  ② 控件框架层（Widget 树 / 绝对·VBox·HBox 布局 / 命中测试 / 主动事件分发，
  28 个）；③ **成品控件外观层**（Label / Button / CheckBox + 主题表驱动四态 +
  纯值事件推进，12 个）；④ **输入层**（TextBox 单行编辑 / 光标·选区 /
  焦点链 / 统一功能键码与修饰键，14 个）；⑤ **主题层 / 控件扩展**（14 个可覆写
  颜色角色 + Radio / Slider / ProgressBar + value / range / group，11 个）；
  ⑥ **信号槽 / 控件事件订阅**（6 类可订阅纯值信号 + 每树环形队列 +
  subscribe / pending / data / pop / clear / dropped，10 个，见下）；
  ⑦ **菜单 / 对话框覆盖层**（窗口级顶栏菜单 + 弹出菜单 + 模态对话框，复用第 6 层
  纯值信号队列：`SELECTED` / `SUBMITTED` 两类信号 + `dial_gui_menu_*` /
  `dial_gui_dialog_*` / `dial_gui_window_*` 统一分发，24 个，见下）；
  ⑧ **Dial 侧 OO 封装层**（`gui_oo.dial`：278 个 `impl` 方法覆盖上述 279 个函数，
  纯 Dial 源码、随包 `src/` 分发（装包即用）、零 `extern fn` 增量，见 README §38.14）。
  ⑨ **滚动视图层**（`dial_gui_scroll_*`：视口裁剪 + 滚动条几何 / 命中 / 绘制 +
  偏移推进 `wheel` / `drag` / `scroll_to`，15 个 `extern fn`，见 README §38.15）。
  ⑩ **右键上下文菜单层**（`dial_gui_context_*`：复用第 7 层 `GuiMenu` + 弹出锚点 +
  命中 / 悬停 / 选中 / 绘制，借用语义不转移菜单所有权，见 §38.16）；
  ⑪ **悬浮提示层**（`dial_gui_tooltip_*`：文本 + 位置 + 尺寸 + 悬停计时，延迟由
  调用方推进、不读时钟，见 §38.17）；
  ⑫ **状态栏层**（`dial_gui_statusbar_*`：左 / 右文本 + 高度 + 分隔线 + 进度条
  槽位，进度值越界夹紧，见 §38.18）；
  ⑬ **通知气泡层**（`dial_gui_toast_*`：FIFO 消息队列 + 生命周期纯值推进、
  到期自动消隐，见 §38.19）；
  ⑭ **窗口级覆盖层自动集成**（`dial_gui_window_set_*`：5 类覆盖层挂窗 +
  `win.draw()` 固定 z-order 自动叠加 + 借用语义 + 空槽位零像素影响，15 个
  `extern fn`，见 README §38.20）；
  ⑮ **快捷键 / 命令系统**（`dial_gui_hotkey_*`：按键组合 → 命令 id 的纯内存表 +
  命令启用态，`dispatch` 只发启用且命中的命令、命令语义归应用、C 侧零反向回调，
  18 个 `extern fn`，见 README §38.21）；
  十五层共 **279 个 `extern fn`**；第 8 层不新增任何 `extern fn`。**多行文本（IME） / 撤销重做 / 每控件主题覆写 /
  图片解码 / C→Dial 反向回调仍不进 `.diali` 契约**——把未实现函数写进契约会让
  用户 `import` 后链接失败，那是制造漂移而非预留接口（菜单 / 对话框覆盖层已由第 7 层
  以**纯值信号队列**落地，**不是** C → Dial 回调）。
- **自研迷你 2D 光栅化器**：ARGB32 像素缓冲、`source-over` alpha 混合、
  1/2/4/8 档覆盖面积采样抗锯齿、矩形 / 线 / 圆 / 圆环 / 椭圆 / 梯形 / 位图 /
  裁剪 / 像素读写；`dial_gui_painter_crc32()` 提供像素级回归断言。
- **字体层**：内置 8×8 ASCII 点阵（零运行依赖兜底）+ vendor `stb_truetype.h` v1.26
  （MIT）做 TrueType 字形栅格化；只加载本机可信字体文件（≤ 64 MB），
  **绝不解析不可信 / 网络字体**（上游明确 `NO SECURITY GUARANTEE`）。
- **Linux / Termux 后端（真实现）**：`libX11` / `libXext` / `libXrender` 一律
  `dlopen` + `dlsym` 动态装载；**XShm** 共享内存离屏画布 + **XRender** Picture/Filter
  合成，自定义 `XSetErrorHandler` 把异步 X11 错误转成 Dial 返回码 + 错误槽，
  避免 Xlib 默认处理器 `exit(1)` 带走宿主进程。
- **Windows 后端（真实现）**：`LoadLibraryA("user32.dll")` / `gdi32.dll` 动态解析，
  `CreateDIBSection` 32bpp top-down BGRA + `CreateCompatibleDC` / `StretchBlt`，
  `SetStretchBltMode(HALFTONE)` 对应 XRender 平滑 filter；Win32 消息循环映射统一事件码。
  **沙箱只能交叉编译、不能实机运行**，平台边界如实标注。
- **三平台 `.a` 零链接期依赖**：`x86_64-linux-gnu` / `aarch64-unknown-linux-musl` /
  `x86_64-pc-windows-gnu` 均由 `zig cc` 一次交叉编译；`nm -u` 断言 `.a` 不含
  `XOpenDisplay` / `XShmCreateImage` / `XRenderComposite` / `CreateDIBSection` /
  `CreateWindowExA` 等平台 GUI 未定义符号，`dialc` 最终链接**不需要**新增
  `-lX11` / `-lgdi32`。
- **控件框架层（28 个新 `extern fn` + `type GuiWidget`）**：并入同一个 `gui` 包
  （`.diali` 不透明句柄按包隔离、跨包引用不可行），旧 55 个签名**一字不动**、
  `abi = 6` / `version = 1.10.0` 不变，仅重新 `pack` + `install`（同 ABI 覆盖）。
  新增 `dial_gui_widget.c` 是纯 C、不碰任何平台 API，三份 `.a` 仍无 GUI 链接期依赖。
  - **独立纯内存树根**：`dial_gui_widget_tree` 不连显示服务器，无 DISPLAY 环境也能
    完整验收布局 / 命中；挂窗后由窗口拥有，随 `window_close` 释放。
  - **布局**：绝对 / VBox / HBox 三种，纯整数、固定尺寸优先扣减、gap/padding，
    **幂等**（每次从 `req_*` 重算，不把上轮弹性尺寸误当固定）。
  - **命中测试**：后加子优先、取最深可见 Box；放在后端 `pump` 的 Dial 调用线程，
    **绝不进 `wndproc` 系统回调**。
  - **主动事件分发 + 零越界**（内存宪法）：**绝不做 C → Dial 反向回调**。C 侧只写
    一块 `__attribute__((packed))` 纯值块 `GuiWidgetEvent`（104 字节），Dial 侧
    `poll/wait` 后经 `rawptr<byte>` + `ptr_cast` 读纯值；`win_title` / `widget_id`
    是 C 侧**借用**指针，必须 `string_from_cstr` 复制成自己的 ARC 对象。
    C 管 C 的，ARC 管 ARC 的，零隐式开销。
  - **布局双向锁死**：C 侧 `_Static_assert(sizeof==104, offsetof(...))` 与 Dial 侧
    `sizeof<T>()` / `alignof<T>()` / `offsetof<T>(field)` 编译期内建逐项比对，
    任何字段增删两边同时报错。
- **成品控件外观层（12 个新 `extern fn`）**：同样并入 `gui` 包、只追加、旧 83 个
  签名**一字不动**、`abi = 6` / `version = 1.10.0` 不变。新增
  `src/dial_gui_look.c` 是纯 C（只 include `dial_gui.h` + libc、零平台符号），
  三份 `.a` 仍无 GUI 链接期依赖。
  - **内置浅色默认主题**：颜色写死在 `dial_gui.h` 的 `GUI_THEME_*`，本版不暴露
    theme setter；字体用静态零分配内置点阵对象（`kind=0`、字号 16），绘制三平台
    逐像素一致，可被 `painter_crc32` 稳定断言。
  - **四态 + 纯值事件**：`widget_handle(root,kind,x,y,button)` 只读纯值，复用
    `widget_hit` 取最深可见控件，MOVE / PRESS / RELEASE 推进 `hovered` / `pressed`
    并激活（CHECKBOX 翻转 `checked`、BUTTON 返回 1）；`disabled` 不参与、走禁用
    配色。**绝不 C → Dial 反向回调**，被激活控件由 Dial 侧 `widget_hit` 自取。
  - **整树一次绘制**：`widget_draw(root,painter)` 前序递归累加相对几何（不依赖
    `abs_*`），父先画后加子在上，BOX 面板 / LABEL 文本 / BUTTON 四态背景+居中文本 /
    CHECKBOX 方框+勾+文本；只读状态、不推进状态。
  - **文本 ARC 边界**：`widget_set_text(string)` 由 C 侧 `strdup` 自有副本，
    `widget_text(): rawptr<byte>` 是借用指针，Dial 侧必须 `string_from_cstr`
    复制；`GuiWidgetImpl.text` 由 `dispose_subtree` 与 `id` 同口径释放。
- **输入层（14 个新 `extern fn`）**：同样并入 `gui` 包、只追加、旧 95 个签名
  **一字不动**、`abi = 6` / `version = 1.10.0` 不变；`GuiWidgetEvent` 的
  **104 字节布局一字不动**（修饰键走独立访问器，绝不塞进 packed 块改尺寸）。
  新增 `src/dial_gui_input.c` 是纯 C（只 include `dial_gui.h` + libc、零平台符号），
  `build.sh` 三平台 `SRCS` 统一追加，三份 `.a` 仍无 GUI 链接期依赖。
  - **TextBox 单行编辑**：新控件种类 `4 TEXTBOX`；可打印字符（Unicode 码点 →
    UTF-8）插入 / 替换选区、BACKSPACE / DELETE、LEFT / RIGHT / HOME / END 移光标、
    SHIFT 扩选区、ENTER 提交信号、ESCAPE 清选区。光标 / 选区一律是 **UTF-8
    字节偏移**并按字符边界对齐，绝不切断多字节序列（U+4E2D 三字节回归覆盖）。
    文本按新分配整块替换（不用原地挪动，避免越界 / 别名），`GUI_TEXT_MAX_LEN
    = 4096` 兜底。
  - **焦点链（纯 C 树操作）**：根上存一个 C 侧裸指针；Tab 按加入顺序环形前进、
    Shift+Tab 反向；禁用 / 切成非 TEXTBOX / 摘除子树时主动让焦点出栈并清裸指针，
    绝不悬垂；点击 TEXTBOX 的 RELEASE 会把焦点移过去（`widget_handle` 返回 1）。
    **绝不 C → Dial 反向回调**：焦点与编辑态全是 C 侧纯值。
  - **`max_len` 与 Fail-Fast**：`widget_set_max_len` 只接受 `0`（不限）..`4096`，
    越界**明确返回 -1 并写错误槽**，绝不静默截断；显式 `widget_set_text` 不受它
    约束但会把光标 / 选区归零。
  - **两平台统一功能键码**：`dial_gui.h` 定义 `0x110000+` 同表（大于 Unicode
    最大码点 `0x10FFFF`），`dial_gui_x11.c` 把 `X11 KeySym`（含
    `XK_ISO_Left_Tab` = Shift+Tab）与 `dial_gui_win32.c` 把 `VK` 都映射进同一套；
    表外键收敛为 `GUI_KEY_UNKNOWN`，绝不漏出 `0x10000|VK` 这类平台私有值。
    修饰键位掩码由新增的 `dial_gui_event_mods(app)` 提供（X11 取
    `XKeyEvent.state`、Win32 由 `wndproc` 跟踪 `VK_SHIFT/CONTROL/MENU`），
    QUIT 事件明确清零、绝不残留上一轮的陈旧值。
  - **XTest 实窗事件注入（真机验收）**：新增 `tests/pkg/xtest_inject.sh`，在 Xvfb
    里用 `xdotool`（底层 XTEST / `libXtst`）往真实窗口注入鼠标点击 / 键盘 `hi` /
    Tab / Shift+Tab，Dial 侧 `poll/wait` 主动读走并喂进 `widget_key` /
    `widget_handle`，断言落字 / 焦点环 / `mods` / 按钮激活 / `flush` 全通过。
    **`libXtst` 只在测试侧使用，包本体 `.a` 仍零平台链接期依赖**；无 `xdotool`
    环境如实打印 `input_xtest_injection = skipped`，不伪装通过。
- **主题层 / 控件扩展（11 个新 `extern fn`）**：并入 `gui` 包、只追加、旧 109 个
  签名**一字不动**，`abi = 6` / `version = 1.10.0` / `GuiWidgetEvent` 104 字节
  布局全部不变。新增 `src/dial_gui_theme.c`（纯 C，零平台符号）与 3 个新控件种类。
  - **14 个可覆写颜色角色**：进程级静态颜色表（GUI 单线程事件循环，无需加锁），
    `theme_set(role,color)` / `theme_get(role)` / `theme_reset()` /
    `theme_role_count()`；默认色值逐字取自 `dial_gui.h` 的 `GUI_THEME_*` 宏，
    `look.c` 全部配色改走 `gui_theme_color(role)`，因此 `theme_reset()` 后逐像素
    CRC 与引入 setter 之前完全一致，`look_crc_normal = 3884455222` **不漂移**。
    角色编号只准末尾追加，跨时点稳定（新增 `11 TRACK` / `12 FILL` / `13 KNOB`）。
  - **Radio / Slider / ProgressBar**：`5 RADIO` 圆圈 + 选中点 + 文本，RELEASE
    选中且 `group != 0` 时同组互斥（`0` = 各自独立），复用 `checked`；
    `6 SLIDER` 轨道 + 已填充 + 滑块，RELEASE 按命中点局部 `x` 的纯整数比例设
    `value` 并 clamp 进 `[vmin, vmax]`；`7 PROGRESS` 轨道 + 边框 + 按比例填充，
    **只读**、RELEASE 不激活。默认值域 `0..100`，全部纯整数几何、三平台逐像素一致。
  - **Fail-Fast（§20）**：`role` 越界 / 颜色高 32 位非 0 / `value` 越界 /
    值域倒置 / 值域过大一律返回 `-1` 并写错误槽，绝不静默截断；`set_range`
    成功后把已有 `value` clamp 进新值域（getter 立刻可读回，非不可见损坏）。
  - **回归**：新增 `tests/pkg/use_gui_theme.dial`（独立 offscreen 画布、关 AA），
    逐像素断言改色生效 / `theme_reset` 幂等 / 八种控件绘制确定性 / Radio 互斥 /
    Slider 单调 / Progress 只读 / 值域 Fail-Fast；AA 组 +3 条 `aapass`，
    Xvfb 实窗八种控件整树绘制 + `flush` 上屏全通过。

- **信号槽 / 控件事件订阅（第 6 层，10 个新 `extern fn`）**：并入 `gui` 包、
  只追加、旧 120 个签名**一字不动**，`abi = 6` / `version = 1.10.0` /
  `GuiWidgetEvent` 104 字节布局全部不变。新增 `src/dial_gui_signal.c`（纯 C，
  零平台符号）。**绝不 C → Dial 反向回调**：C 只在状态推进处把一条纯值信号压进
  每树环形队列，Dial 侧主动 `pending → data(ptr_cast 读) → pop` 消费并分派。
  - **6 类可订阅纯值信号**（位值可组合成每控件 `int64` 订阅掩码）：`CLICKED`(1) /
    `TOGGLED`(2) / `TEXT_CHANGED`(4) / `VALUE_CHANGED`(8) / `FOCUS_CHANGED`(16) /
    `SUBMITTED`(32)。埋点在 `widget_handle` / `widget_key` / `set_focus` /
    `focus_next` / `set_text` / `set_checked` / `set_value`，每个信号只由状态真的
    变化的那一次交互 / setter 触发一次（同值 setter、同组 RADIO 被动清除、
    `set_range` 的 clamp 都不另发信号）。
  - **`GuiWidgetSignal` 72 字节 packed 纯值块**：`seq / signal / tag / userdata /
    kind / a / b / x / y`（9 × `int64`），**100% 纯值、零指针、零句柄**——连控件
    句柄都不存，只存 `tag` / `userdata` 纯值键。控件被摘除 / 文本被覆盖 / 整树被
    释放后，已入队信号仍是自洽整数快照，**读数绝不悬垂**、无需 `string_from_cstr`。
    C 侧 `_Static_assert(sizeof==72)` 与 Dial 侧 `sizeof/offsetof` 双向锁死。
  - **零订阅 = 零开销**：订阅掩码默认 0，`emit` 首行即查掩码，未订阅直接返回
    （不找根、不分配、不入队）；旧程序行为 / 绘制 CRC / 返回值逐字节不变，
    `look_crc_normal` 等历史基准不漂移。队列只挂根节点、容量 64、首次 emit 才
    `calloc`，`dispose_subtree` 统一 `free`。
  - **Fail-Fast（§20）**：订阅掩码负数 / 未知位 / 越界返回 `-1` 写错误槽；队列满
    覆盖最旧、保留最新并把 `sig_dropped` 累计 `+1`，`calloc` 失败同样计数，Dial 侧
    `signal_dropped` 可读「漏了多少条」，**绝不静默损坏**；`seq` 单调、`clear` 不
    重置序号；`signal_data` 返回借用指针，空队列 NULL，Dial 须读完再 `pop`。
  - **回归**：新增 `tests/pkg/use_gui_signal.dial`（独立纯内存树，不碰任何 CRC
    基准），无 DISPLAY 断言 72 字节布局 / 六类信号逐字段（含 x/y）/ 零订阅零开销 /
    订阅 Fail-Fast / `seq` 单调 / 溢出 `dropped` 可观测 / 取消订阅 / 纯值块读数不依赖
    控件生命周期；Xvfb 实窗挂树 + 订阅 + 信号读取 + 整树绘制 + `flush` 上屏。
    AA 组 +3 条 `aapass`，全量 **523 ✅ → 526 ✅**。

- **菜单 / 对话框覆盖层（第 7 层，24 个新 `extern fn`）**：并入同一个 `gui` 包、
  只追加、旧 130 个签名**一字不动**，`abi = 6` / `version = 1.10.0` 不变。新增
  `src/dial_gui_menu.c`（纯 C，零平台符号）。**绝不 C → Dial 反向回调**：菜单项 /
  对话框按钮选中事件复用第 6 层纯值信号队列（`SELECTED` / `SUBMITTED`），所有权归
  窗口、关闭时一并释放。
  - **窗口级覆盖层，不进控件树**：顶栏菜单（`File` / `Edit` / `Help` 横排，`GUI_MENU_BAR_H = 22`、
    应用自行在控件树顶部留 22px 垫片）+ 弹出菜单（锚点定位、分隔符 / 禁用项不可命中）
    + 模态对话框（`OK`=1 / `CANCEL`=2 / `YES`=4 / `NO`=8 按钮位、半透明遮罩 + 居中盒）。
    三者浮在控件树之上、参与绘制但不进布局，故控件几何 / 绘制 CRC 逐字节不漂移。
  - **信号复用、不另起炉灶**：`GUI_SIG_SELECTED = (1<<6) = 64`（`a = item id`、
    `kind = GUI_WIDGET_MENU = 8`）+ 第 6 层 `GUI_SIG_SUBMITTED`（`a = 结果码`、
    `kind = GUI_WIDGET_DIALOG = 9`）；信号码总数 `GUI_SIG_COUNT` 6 → 7、掩码上限放开。
    `gui_signal_emit_ex` 是「非控件来源、显式给定 kind/tag/userdata」的发射变体。
  - **统一分发优先级**（`window_handle` 一次调用）：对话框（模态，命中按钮 → emit
    `SUBMITTED` 且 `open=0`、返回 3）→ 弹出菜单（命中项 → emit `SELECTED` 且选中即释放、
    返回 3）→ 顶栏菜单（命中项 → emit `SELECTED`、返回 3）→ 控件树（委派 `widget_handle`、
    旧行为不变，返回 1/0/-1）。`set_root` 未挂树时 `window_set_menubar` / `window_popup_menu`
    / `dialog_show` 一律返回 `-1` 写错误槽（Fail-Fast）。**注意**：覆盖层信号走 anchor
    `win->root`，故须订阅 `root` 到 `32|64` 位、零订阅零开销守卫才会放行。
  - **纯内存对象可单测**：未挂窗的 `dial_gui_menu_new` / `dial_gui_dialog_new` 直接用
    `gui_menu_free` / `gui_dialog_free` 释放，无 DISPLAY 也能验收；菜单项头插排序 / 越界
    返回 `-1` / 禁用 / `tag` 纯值键、对话框标题 / 消息 / 按钮位 / 结果码全部可断言。
  - **回归**：新增 `tests/pkg/use_gui_menu.dial`，段 A/B 无 DISPLAY 断言菜单对象与对话框
    对象逻辑，段 C 在 Xvfb 实窗验顶栏点击 → `SELECTED(a=item id, kind=MENU, tag 透传)`、
    弹窗点击命中 `Paste`、模态对话框点 `OK` → `SUBMITTED(a=1, kind=DIALOG)` 且自动关闭、
    控件树按钮仍收得到 `CLICKED`。AA 组 +3 条 `aapass`（无 DISPLAY / Xvfb 各一 + 编译成功），
    全量 **535 ✅ → 538 ✅**。

### Fixed

- **`http.dialpkg`：chunked 长度行不再静默损坏（§20 Fail-Fast）**。`dial_http.c` 的
  `dh_read_chunked` 用 `strtoll(line, &endp, 16)` 解析块长度却**不检查 `endp != line`**：
  服务器返回 `xyz` 时 `strtoll` 返回 0 且 `endp == line`，畸形响应被当成「结束块」，
  响应体凭空少一截却报 200。现补四道检查：`endp == line`（一个十六进制数字都没
  解析到）、`errno == ERANGE`（溢出）、负数、以及长度数字后除 `;chunk-ext` / 行尾空白
  以外的脏尾随字符；任一命中即 `DIAL_HTTP_ERR_PROTOCOL` 并把坏行写进错误槽，合法
  `1a;ext=1` 扩展照常放行。`errno.h` 提到文件顶部无条件包含，Windows 分支同样生效。
- **`http.dialpkg`：Termux 目标平台能加载系统根证书库**。`dial_tls.c` 的 `load_linux_ca`
  原本只读 `/etc/ssl/certs` 与 `/etc/ssl/cert.pem`，而 Termux 的证书在
  `$PREFIX/etc/tls/cert.pem`——文档把 Termux 列为目标平台，`https` 却直接报
  「无法加载系统根证书库」。现按 `getenv("PREFIX")` 的实际长度**动态拼**该路径
  （绝不静默截断），加载成功即计入受信链；失败时的中文提示同步列出 Termux 路径。
  自定义 CA 优先级与 fail-closed 证书校验一点未动。
- **`http.dialpkg`：额外请求头不再被 512 字节静默截断**。`dial_http2.c` 的 `h2_submit`
  原用 `char buf[512]` 且 `llen` 超限就截到 511 字节：很长的 `Authorization` /
  `Cookie` 会被悄悄砍掉，请求照样发出却鉴权失败。现按行长 `malloc` 整行
  （`dh_hlist_add` 本就拷贝 name/value，用完即释放），分配或入表失败按 Fail-Fast
  整条请求失败、不发半截头。HTTP/1.1 侧 `dh_request` 本就走动态 `dh_buf`，无此问题。
- **`http.dialpkg`：HTTP/2 额外头名统一小写化**（去掉截断后暴露的协议合规坑）。
  RFC 7540 §8.1.2 要求头名必须小写，用户按 HTTP/1.1 习惯写的
  `Authorization: <超长 token>` 会被对端以 `ProtocolError` 拒绝。现 `h2_submit` 只把
  `extra_headers` 的头名转小写，H1 侧大小写不敏感、保持原样；`.diali` 对外签名与
  `abi = 6` 均不变（仅注释补充，`pack` 时重算 SHA256、`install` 同 ABI 覆盖）。

### Changed

- **更新日志分层（文档结构）**：`CHANGELOG.md` 从 414 KB 瘦身为约 170 KB，只保留
  **新增 / 更改 / 破坏性变更 / 修复**四类变更条目；每版原有的「为什么改 / 实测数据 / 边界」
  证据链逐字拆到仓库根 `rationale/<版本>.md`（33 份，按版本 slug 命名），CHANGELOG 每版条目
  附直达链接。官网相应新增「理由与证据索引」页（`docx/rationale.html`）与 `docx/rationale/` 下每版
  一页的详情页（`tools/build_web.py` 确定性生成，`check_links.py` 纳入官网链接校验）。契约：CHANGELOG.md
  与各 rationale/*.md 的正文均与各自源文件逐字一致、信息零丢失；`## [vX]` 版本标题仍在 CHANGELOG
  供 `check_version.py` 校验；冻结文件 `src/codegen.rs` / `src/ast.rs`、`Cargo.toml`、LLVM 路径零改动。
- `tests/run_tests.sh` 的 **W 组**（`http.dialpkg` 端到端）新增 4 条断言，逐一覆盖
  本轮修复：`/chunked_bad` 畸形长度行必须 `is_null=true` 且错误码 `-5`（错误消息含
  坏行）、`/chunked_ext` 合法 `;ext=1` 必须解出完整 body；`$PREFIX/etc/tls/cert.pem`
  真实 TLS 1.3 握手成功（`verify=0`）且空 CA 目录仍 fail-closed；HTTP/2 1200 字节
  超长额外头按实际长度回显 `X-Long-Len=1200`。配套新增 `http_test_server.py` 两条
  端点、`http2_test_server.py` 的 `X-Long-Len` 回显头，以及 `use_http.dial` /
  `use_http2.dial` / `use_https.dial`（Termux 模式）里的断言。全量回归由
  **513 ✅ 增至 517 ✅**（上一轮时点），组数仍为 A–Z + AA 二十七组；
  上一轮 GUI 输入层由 **517 ✅ 增至 520 ✅**、主题层 / 控件扩展再由
  **520 ✅ 增至 523 ✅**；信号槽 / 控件事件订阅由 **523 ✅ 增至 526 ✅**，
  菜单 / 对话框覆盖层（第 7 层）再由 **526 ✅ 增至 538 ✅**，`int32` / `float32`
  一等类型新增 AB 组到 **535 ✅**；B1 `impl` 方法块新增 AC 组
  （第 29 组）5 条断言到 **549 ✅**；**本轮** GUI 第 8 层 OO 封装新增 AD 组
  （第 30 组）6 条断言，全量扩为 **555 ✅ / 0 ❌**（与 `[未发布]` 段口径一致）。
- `tests/run_tests.sh` 的 **AA 组**（`gui.dialpkg` 端到端）：三平台构建、零警告检查、
  `nm -u` 符号检查、`pack` / `install`、无 DISPLAY 优雅跳过、Xvfb 实窗严格断言
  （`window = ok` / `window_backend = x11/xshm/xrender` / `window_flush = ok` /
  `quit_event = ok`）、卸载。全量从 A–Z 二十六组扩为 **A–Z + AA 二十七组**。
  框架层那一轮给 AA 组增 3 条断言（`use_gui_widget.dial` 编译 / 无 DISPLAY 控件层 /
  Xvfb 挂树 + packed 事件块），外观层再增 3 条（`use_gui_look.dial` 编译 /
  无 DISPLAY 四态与 CRC / Xvfb 整树上屏），输入层又增 3 条
  （`use_gui_input.dial` 编译 / 无 DISPLAY 编辑·焦点·CRC / Xvfb + XTest 实机注入），
  主题层那轮再增 3 条（`use_gui_theme.dial` 编译 / 无 DISPLAY 主题·控件·值域 /
  Xvfb 八种控件整树上屏），信号层又增 3 条（`use_gui_signal.dial` 编译 /
  无 DISPLAY 纯值信号·订阅 Fail-Fast·溢出可观测 / Xvfb 挂树 + 信号读取 + 上屏），
  第 7 层再增 3 条（`use_gui_menu.dial` 编译 / 无 DISPLAY 菜单·对话框对象逻辑 /
  Xvfb 顶栏·弹窗 SELECTED + 模态对话框 SUBMITTED + 控件树 CLICKED），
  `aapass` 由 8 → 11 → 14 → 17 → 20 → 23 → **26 条**。
- 新增 `tests/pkg/use_gui_widget.dial`：与 `use_gui.dial` 完全独立、**不创建
  `GuiPainter`**，因此不触碰 `crc_offscreen = 4007435570` 像素回归基准；
  覆盖 `packed extern struct` 布局 / `string_from_cstr` 的 ARC 复制 /
  三种布局与幂等 / 命中（含后加子优先、不可见跳过）/ 防悬垂 / 挂窗生命周期。
- 新增 `tests/pkg/use_gui_look.dial`：用**自己的** offscreen 画布（关 AA），同样不碰
  `crc_offscreen = 4007435570`；覆盖三种控件种类 / 非法种类拒绝 / 文本
  `string_from_cstr` ARC 复制与空借用指针 / VBOX 三等分 / 四态背景逐像素色值
  （`get_pixel`）与 CRC 互异 / 移出画布复位幂等 / 禁用配色 / CheckBox 翻转 /
  `look_crc_normal = 3884455222` 硬钉 / Xvfb 下 `window_set_root` + 整树绘制 + flush。
- 新增 `tests/pkg/use_gui_input.dial` 与 `tests/pkg/xtest_inject.sh`：前者用**自己的**
  offscreen 画布（关 AA）覆盖输入层（TEXTBOX 种类 / 焦点环 / 字符插入 / Shift 选区
  与替换 / DELETE·BACKSPACE·HOME·END / UTF-8 三字节字符边界 / `max_len` 越界报错 /
  Tab·Enter 返回值 / 焦点态边框逐像素色值 / `input_crc_focused = 4147329629` 硬钉 /
  摘除焦点子树后裸指针归零）；后者在 Xvfb 里用 `xdotool`（XTEST）向真实窗口注入
  鼠标点击 / 键盘 `hi` / Tab / Shift+Tab，实机断言落字 / 焦点环 / `mods` 含 SHIFT /
  按钮激活 / `flush`。无 `xdotool` 时如实打印 `skipped`，不伪装通过。
- `packages/gui/` 新增 `src/dial_gui_widget.c`（框架层）、`src/dial_gui_look.c`
  （外观层）、`src/dial_gui_input.c`（输入层）、`src/dial_gui_theme.c`（主题层）、
  `src/dial_gui_signal.c`（信号层）与 `src/dial_gui_menu.c`（第 7 层菜单 / 对话框覆盖层），
  `build.sh` 三平台 `SRCS` 统一追加六者；`gui.diali` 六轮共追加 **99 个单行 `extern fn`**
  （28 + 12 + 14 + 11 + 10 + 24）与 `GuiWidget` / `GuiMenu` / `GuiDialog` 句柄并改写
  七层范围声明（合计 154 个）；`dial_gui.h` 增加 104 字节
  packed 事件块 / 72 字节 packed 信号块与各自 `_Static_assert`、全局根链表防悬垂、
  控件外观层 / 输入层 / 主题层 / 信号层字段（`value` / `vmin` / `vmax` / `group` /
  `sig_mask` / `sig_dropped` / `sigq`）、14 个主题角色与默认值宏、7 个信号码（第 7 层
  新增 `GUI_SIG_SELECTED = (1<<6)`、总数 6 → 7）、菜单 / 对话框覆盖层字段（顶栏 22px、
  `menubar` / `popup` / `dialog` 所有权归窗口）、统一的 `gui_signal_emit_ex` 发射变体；
  **第 8 层不在此列**——新增的 `gui_oo.dial` 是纯 Dial 源码层，`gui.diali` / C 层 /
  三平台 `.a` / `build.sh` 全部零改动，B1 落地当时不重新 `pack`（其后已随包
  `src/` 分发，见上文 §25.11 条目）；
  统一键码常量；`dial_gui_core.c` 共追加 35 个 FFI 包装（14 输入 + 11 主题 +
  10 信号，含 `dial_gui_event_mods`）与 `gui_core_emit` 的 QUIT 清零 `ev_mods`；
  两后端 `pump` 把 `X11 KeySym` / `Win32 VK` 映射到同一套键码并写入修饰键状态。
- `.github/workflows/ci.yml`：阶段 8 构建循环与 `dialc pack` 循环加入 `gui`，
  `dist-gui/gui.dialpkg` 归集进 `dist-c11/`，无头 X11 环境准备步骤本轮补装
  `xdotool` / `libxtst6`（XTest 实机注入所需），Release 资产列表加入 `gui.dialpkg`；
  顺手修正 `softprops/action-gh-release@v2` 的无效输入 `overwrite: true`
  （正确为 `overwrite_files`）。
- `third_party/README.md` 与 `patches/stb-1.26.SHA256SUMS` 登记 `stb/` vendor 基线，
  构建前后 `sha256sum -c` 校验 `third_party/` 零改动，保证离线可复现。

> 📄 本版的理由、实测数据与边界见 [rationale/unreleased-dev.md](rationale/unreleased-dev.md)。

---

## [v1.10] — 2026-11-08

### Added

- **`@asm<T>(...)` / `@asm_intel<T>(...)`：内嵌汇编**（见
  [README §33.13](README.md#3313-asmt内嵌汇编)）：ATT / Intel 双方言、`sideeffects=true`
  防优化、C ABI 标量宽度适配，模板与约束必须字符串字面量且首项为输出约束。
- **`simd<T, N>`：原生向量类型**（见
  [README §33.14](README.md#3314-simdt-n原生向量类型)）：编译期定长 LLVM `<N x T>`，
  广播 / 逐 lane 构造、逐元素算术比较位运算、无符号 `byte` 比较、越界 Fail-Fast。
- `@export` / `packed struct` / `volatile rawptr<T>` / `bitcast<T>(x)` /
  `sizeof<T>()` / `alignof<T>()` / `offsetof<T>(field)` 等 FFI 导出四件套与编译期类型内建
  （并入 `[v1.04]` 的增补，随本版一并发布）。

### Changed

- 编译器版本 `1.4.0 → 1.10.0`（显示 `v1.04 → v1.10`）；`Cargo.lock`、
  VS Code 扩展 `package.json`、`dial-<版本>.vsix` 命名、`make_stdlib_pkg.py` 默认版本
  （改为跟随 `Cargo.toml`，不再硬编码）、`ARCHITECTURE.md` 配套件说明全部同步。
- **修正官网下载链接漂移**：首页与 README 手动解压示例此前写
  `releases/download/v1.4.0/dialc-v1.4.0-*.zip`，既用错标签又用错资产名
  （`package_release.sh` 实际产出 `dialc-v1.4-*.zip`），指向不存在的资产；现统一改为
  `releases/latest/download/dialc-v1.10-*.zip`，并让 `check_version.py` 按同一
  「PATCH=0 取主.次」短式校验，杜绝再次漂移。

> 📄 本版的理由、实测数据与边界见 [rationale/v1.10.md](rationale/v1.10.md)。

---

## [v1.04] — 2026-11-06

### Added

- **语言层 `extern fn`**：直接写在 `.dial` 源码里，编译期生成 LLVM 外部声明、
  链接期由 C 编译器解析，不再强制绕 `.diali` → `.a` → `.dialpkg` → `install` → `import`。
  现有包机制完整保留、行为不变（见 [README §15](README.md#15-ffi-契约dial-语法--c-函数)）。
- **语言层 `link "...";`**：显式链入用户 C 扩展库。相对路径以源文件所在目录为基准解析。
- **语言层动态链接**：`link` 除静态归档外也接受 `.so` / `.dll` / `.dylib`
  （含 `libx.so.1` 带版本号变体），按**扩展名**校验 ELF `\x7fELF` / PE `MZ` /
  Mach-O 32、64 位或 fat magic；ELF / Mach-O 自动注入 `-Wl,-rpath,<库目录>`，
  产物无需手工设 `LD_LIBRARY_PATH`。PE 无 rpath，`.dll` 需放 exe 同目录或 `PATH`。
- **`extern struct`（C ABI / `repr(C)`）**：字段顺序、对齐、padding、sizeof 与 C 完全一致；
  只经 `rawptr<Struct>` 读写字段，不按值传递、不能用 `StructLit` 构造。
- **编译期类型内建 `sizeof<T>()` / `alignof<T>()` / `offsetof<T>(field)`**：手写 C 库绑定
  核对内存布局的刚需。语法与 `ptr_cast<T>(x)` 共用「类型实参在 `<>` 里」的约定
  （`offsetof` 的字段名是裸标识符，如 `offsetof<Point>(y)`）；数值按**宿主 target
  DataLayout** 在编译期算出、发射为 `i64` 常量，**零运行时开销**；全部返回 `int`。
  三种内建都直接吃 `extern struct`，与字段读写路径、与 C 的 `offsetof` 逐字节一致
  （已加 P 组 `2b` 子测试锁死）。`void` 被显式禁止（`sizeof` 在 C 里未定义）；
  `offsetof` 要求真实存在且类型实参确为结构体（含 `extern struct`），否则编译期报错。
  修复点：模块创建时把宿主目标机的 `DataLayout`/`triple` 显式写到 `module` 上——
  否则 `module` 默认 `DataLayout` 会把 `i64` 对齐写成 4（而非 x86-64 SysV 的 8），
  导致上述内建与 `extern struct` 字段偏移与 C 不一致。
- **`@export`：把 Dial 函数导出给 C 调用**（见 [README §33.9](README.md#339-export把-dial-函数导出给-c-调用)）。
  `extern fn` 是「Dial 调 C」，`@export` 是「C 调 Dial」——被标注的函数以**裸 C 符号名**
  （无 mangling）与 C 调用约定对外暴露，C 侧可直接链接 / `dlsym` 调用。两条硬规则：
  ① 签名必须全是 C 兼容类型（标量 / `rawptr<T>` / `fn(A):R` / `extern struct` 值），
  含 `string` / `bytes` / 数组 / map 一律**编译期拒绝**（C 侧无法按 C ABI 正确接收，
  放行等于埋 UB）；② **只有标了 `@export` 的函数才是全局符号**，其余 Dial 函数收成
  LLVM `Private` linkage——模块内照常递归、照常取函数指针，但不导出，既不污染 C 侧
  符号空间、也不会与别的目标文件里的同名符号撞车。`main` 例外，恒为外部可见。
  `export fn`（不带 `@`）是等价写法。
- **`packed struct`：网络协议的逐字节对齐**（见 [README §33.10](README.md#3310-packed-struct网络协议的逐字节对齐)）。
  `packed extern struct` 走 **C ABI 紧凑布局**（`bool` / `byte` → `i8`），与 C 的
  `__attribute__((packed))` / `#pragma pack(1)` 逐字节一致——网络协议头的字段偏移是
  协议硬约定，一个 padding 字节就让整包解析错位；`packed struct` 则是 Dial 自己
  值语义结构体的紧凑布局。布局用 `sizeof<T>()` / `alignof<T>()` / `offsetof<T>(field)`
  编译期核对（零运行时开销），P 组 2c 把 Dial 侧的三个数与 C 侧
  `sizeof` / `_Alignof` / `offsetof` **逐项比对**（刻意不写死期望值——布局随平台变，
  「两边相等」才是要锁住的东西）。
- **`volatile rawptr<T>`：内存映射 IO 的易失访问**（见 [README §33.11](README.md#3311-volatile-rawptrt内存映射-io-的易失访问)）。
  语义与 `rawptr<T>` 完全一致（同样不受 ARC 管、同样要显式 `raw_free`），差别只在
  代码生成：经它读写的内存发射 `load volatile` / `store volatile`，LLVM 不再对其做
  任何优化——每次访问都真实落总线。`volatile` **只能修饰裸指针**（`volatile int`
  编译期拒绝）；`ptr_cast<volatile rawptr<T>>(p)` 把普通内存切进易失视图时
  **易失性会保留**，不在转换处被悄悄抹掉。
- **`bitcast<T>(x)`：底层内存强转**（见 [README §33.12](README.md#3312-bitcasttx底层内存强转)）。
  把值的**位模式**按目标类型重新解释——不读、不改内存。类型实参约定与
  `ptr_cast<T>(x)` / `sizeof<T>()` 同一套。**位宽必须相同**，不一致一律编译期拒绝
  并报出两边实际位宽（位宽按 target DataLayout 现算——32 位平台上指针是 32 位，
  那时 `bitcast<int>(ptr)` 就该被拒）。指针 ↔ 整数走 `ptrtoint` / `inttoptr`
  （LLVM 的 `bitcast` 本身不允许跨类转），整数 ↔ 整数、浮点 ↔ 整数同位宽互转走
  真正的 `bitcast`。
- **`@asm<T>(...)`：内嵌汇编**（见 [README §33.13](README.md#3313-asmt内嵌汇编)）。
  把汇编模板与 LLVM 约束字符串在编译期写进 IR，产物里就是一条
  `call ... asm sideeffect ...`。占位符用 LLVM IR 的 `$0` / `$1`（不是 GCC C
  `__asm__` 的 `%0` / `%1`），`$$` 转义 AT&T 立即数；`@asm` 走 AT&T 方言，
  `@asm_intel` 走 Intel 方言。返回类型必须显式写成 `<T>` 且只能是标量 / 裸指针；
  模板与约束必须是**字符串字面量**，约束首项必须是输出约束（`=r` / `=&r` / `+r`），
  否则编译期拒绝。输入输出按 C ABI 宽度（`bool`→`i32`、`byte`→`i8`）走
  `abi_adapt` 对称适配，`sideeffects=true` 防止被优化掉。平台相关，不做可移植性保证。
- **`simd<T, N>`：原生向量类型**（见 [README §33.14](README.md#3314-simdt-n原生向量类型)）。
  编译期定长的 LLVM `<N x T>`，是**寄存器里的值**（无堆分配 / 无 ARC），逐元素算术
  落成单条向量指令。元素用**原生机器宽度**（`int`→`i64`、`float`→`f64`、
  `bool`→`i1`、`byte`→`i8`、`char`→`i32`）——`sizeof<simd<byte,16>>()` 是 **16**
  而不是 128。支持广播 / 逐 lane 构造、逐元素算术与比较（返回 `simd<bool,N>`，
  `byte` 用无符号谓词）、位运算、索引读（越界 Fail-Fast）；`N` 是 2~64 的编译期
  字面量，元素仅标量且不可嵌套。刻意不开放：原地改 lane、`and`/`or` 短路逻辑、
  直接 `print`、作为数组元素 / `extern struct` 字段 / `@export` 签名（C 侧向量 ABI
  需专门声明，编译器不替用户猜测）。向量除法不做逐 lane 除零检查，与 C SIMD
  intrinsic / LLVM 向量除法一致（含零 lane 为 poison，由调用方保证）。
- **`.diali` 类型白名单打穿**：`byte` / `bytes` / `rawptr<T>` / `fn(A, B): R` 全部放行；
  形参表改为按**顶层逗号**切分，嵌套泛型/函数类型签名不会被切坏。
- **完整数据总线**：`string.as_c_str()` / `bytes.as_ptr()` 返回**借用**的 `rawptr<byte>`
  （由 C Runtime 跳过 8 字节长度头，用户不写 `+8` offset hack）；
  `string_from_cstr(p)` / `bytes_from_ptr(p, n)` 从 C 缓冲区**复制**出受 ARC 管理的新对象。
- **C 回调**：`Type::FnPtr` 生成标准 C calling convention，C 侧可安全调用 Dial 具名函数；
  跨回调状态经 `rawptr<void>` 显式透传，无闭包、无隐藏环境对象。
- **工程工具链（并入 `[v1.04]` 的增补，见 [README §34](README.md#34-v104-增补工程工具链并入-v104版本不变)）**：
  - 新增 `dialc check / fmt / test / new / list / uninstall / pack / help` 八个子命令；
    `build / run / check / test` **共用同一条前端流水线**（`main.rs::frontend()`）；
  - **本地源码模块**：`import "./util.dial";` 或裸名 `import "util";`（同目录存在
    `util.dial` 即视为本地模块），递归合并编译，带循环引用 / 同名函数 / 缺失文件诊断；
  - **顶层 `test "名" { ... }` 块**：`dialc test` 逐块独立进程运行、`--filter` 筛选，
    测试代码不进 `build` 产物（刻意不新增 `test` 关键字，避免破坏已有标识符）；
  - **`dialc pack` + `dial.toml`**：通用产包命令，校验 `.diali` 与静态库 ar 魔数、
    生成 `manifest.txt` 与逐文件 SHA256、打包后就地自检；`dialc new` 生成项目骨架，
    `list` / `uninstall` 补齐包管理。`tools/make_stdlib_pkg.py` 保留给 CI 的三平台 stdlib 包。
- **DWARF 调试信息（`-g` / `--debug-info`，见 README §35）**：`dialc build / run` 加 `-g`
  即为 Dial 代码生成标准 DWARF——`DW_TAG_compile_unit` / `DW_TAG_subprogram`（每个 Dial 函数）/
  `DW_TAG_formal_parameter`（形参）/ `DW_TAG_variable` + `llvm.dbg.declare`（`Stmt::VarDecl`
  局部变量）/ `.debug_line` 行表（`gen_stmt` 的语句行列）。GDB / LLDB 因此能按 `.dial`
  源码行下断点、`print counter` 读标量值、`info args` 看形参、`bt` / `list` 直接指向
  Dial 源码，不需要调试器插件。`DIFile` 用绝对路径，`finalize()` 在 `module.verify()` 前调用。
  标量（`int` / `float` / `bool` / `char` / `byte`）给出精确 DWARF 宽度与编码；复合类型
  在 IR 里本就是不透明 `i8*`，如实映射为指向 `dial_object` 的指针，不伪装布局。
  不带 `-g` 时完全不创建 DIBuilder，零开销。
- **性能基准（4 语言 × 28 维，见 README §36）**：`bench/` 下新增 C / Go / Python / Dial
  四份等价实现，28 个纯整数微基准（算术/递归/数组/字符串/位与控制流五类），
  每语言每项产出**完全相同的校验和**（112/112）作为等价计算的正确性锚点。
  - `tools/bench_harness.py`：外部统一墙钟计时 + 单次进程内 `repeat` 自校准
    （摊薄启动开销）+ 扣除 `noop` 基线，给出**绝对值**（单次纯耗时 ms）与
    **绝对比值**（C = 1.00× 基准），并采集 6 大工程维度（速度 / 峰值内存 /
    编译耗时 / 启动耗时 / 产物体积 / 源码行数），产出 `bench/data.json`。
  - `tools/build_bench_web.py`：由数据生成独立静态页 `docx/bench.html`——
    绝对值表 + 比值表 + 六维表 + 底部**六维雷达图**（手写 SVG，4 语言叠加，
    按各维最优语言归一化，越靠外越优），无外部 CDN / JS 依赖、离线可看。
    触及计时下限（或被编译器常量折叠）的项以 `†` 诚实标注。
  - `tools/check_bench_data.py`：校验 28×4 数据完整、校验和与真值一致、
    比值基准、6 维 6 轴，及网页与数据同步（CI 与本地共用）。
  - 官网导航 / 首页 CTA / 安全公告页均加入口；`build_web.py` 不生成 `bench.html`，
    不影响官网产物同步校验。

- **真实程序缺口补齐（B/C 组，第一批）**：按 `REPORT_v1.04_gaps.md` 的裁决落地 5 项，
  规范见 [README §37](README.md#37-v104-增补真实程序缺口补齐与宪法裁决并入-v104分-3-批)。
  - **B9 块注释 `/* ... */`**：lexer 层跳过、跨行同步行号；**未闭合直接 Fail-Fast 报错**，
    绝不静默吞掉余下源码。
  - **B10 `!` 一元逻辑非**：词法层复用 `TokenKind::Not`（`parse_not` / 语义 / IR
    全链路零改动），`!=` 分支优先匹配不受影响；不是新运算符语义，只是同一语法的另一种拼写。
  - **B11 `a.slice(from, to)`**：与切片语法 `a[from..to]` 共用 `dial_array_slice`，
    返回新数组、不改接收者；补齐「能力在但方法名不直觉」的绕路。
  - **C6 `get_or(k, default)`**：普通 map 与泛型 map（标量 V）一次探测，把「未命中」
    与「真实值恰等于默认值」彻底分开。`dial_map_get_or` / `dial_kvmap_get_or`
    两个新 C 导出；句柄 V 由容器类型系统更早拦截，方法层的拒绝分支是纵深防御。
  - **C8 `eprint` / `eprint_raw`**：stderr 输出族（`dial_eprint_*`），与 `print`
    逐类型对称、variadic 规则一致；**不引入可变全局状态**（没有「当前输出流」开关、
    没有全局 `FILE*`），四个分派函数各自独立实现以守 §21.1。
  - **C8 配套 Fail-Fast 加固**：输出族内建（`print` / `print_raw` / `write_char` /
    `eprint` / `eprint_raw`）按名字硬编码分派，此前未列入 `RESERVED_NAMES`——
    用户写 `fn eprint(...)` 能编译却在调用点被内建分支**静默绕过**，违反 §20。
    本轮把这 5 个名字补进保留名清单，同名定义编译期直接拒绝；既有 `print` 调用
    行为不变。`.diali` 自描述补齐 `dial_array_slice` / `dial_map_get_or` /
    `dial_kvmap_get_or` 与 `type DialKvMap;`（新增导出符号不漏登记）。
  - **T 组回归**：stdout/stderr 严格分流、未闭合块注释 Fail-Fast、`get_or`
    类型严格校验、函数重载（B5 永久否决）与输出族保留名负向锁定；A–S 扩为 A–T。
  - **架构记录**：`ARCHITECTURE.md` 追加 §14（宪法裁决与 B/C 组补齐的分层约束、
    ABI 递增判据、3 批落地纪律），明确「规范先于实现，但绝不把未实现项标成已支持」。
- **真实程序缺口补齐（B/C 组，第二批）**：按 §37.5「C11 走独立包」的裁决，本批
  只做**标准库内建**（不进 `stdlib.dialpkg`），全部落在 `stdlib/*.c` +
  `src/semantic.rs` + `src/codegen.rs` + `stdlib/stdlib.diali` 四层。
  - **C1 字符串大小写 / 子串**：`to_upper` / `to_lower` / `contains` / `starts_with` /
    `ends_with`（`dial_string_*`）逐类型安全、UTF-8 字节级。
  - **C4 Unicode 码点**：`char_count` / `chars()` / `char_at(i)` / `char_slice(from,to)`
    按 **UTF-8 码点**而非字节；`length()` 保持字节语义不动，避免破坏既有 416 项回归。
  - **C2 数值严格解析**：`parse_int` / `parse_float` 非法输入 **Fail-Fast**（绝不静默
    返回 0），`dial_parse_int` / `dial_parse_float` 在 C 侧 `dial_assert_fail` 抛 Error；
    `is_int` / `is_float` 提供显式探测。
  - **C3 类型安全格式化**：`format_float(v: float, digits: int)` 拒绝 printf 式运行期
    格式串（§5.3 零隐式转换），`int` 参数编译期被拒；对应 `dial_array_new(1)` + `snprintf`。
  - **C5 数组 API 扩充**：`insert` / `index_of`（标量元素；`string[]` 因位模式歧义被拒）/
    `reverse` / `reduce`（复用 v1.03 函数指针 + `rawptr<void>` thunk，无闭包）/
    `join` / `sort` 放宽到 `string[]`（字典序）/ `clear`，新增 `dial_array_*` C 导出。
  - **C7 文件 IO**：`file_exists` / `file_size`（打不开 `-1`）/ `file_remove` /
    `file_read_all`（FNF→抛 Error）/ `file_read_line(f)`（逐行）/ `eof(f)`；
    刻意用 `file_read_line` 而非 `read_line(f)`，避免与 stdin 版 `read_line()` 撞名
    （B5 否决重载），所有文件操作为不透明句柄、显式 `close`、不进 ARC。
  - **C9 环境**：`has_env` / `getenv`（缺失返回空串）/ `program_name`（argv[0] 只读
    快照，非 `var` 全局）/ `cwd`，无 `setenv`（进程级可变全局状态，否决）。
  - **C10 时间**：`local_parts()`（int[8]，自取分量）/ `date_string()`（固定
    `YYYY-MM-DD HH:MM:SS`）/ `monotonic_ms()`，拒绝 printf 式格式串，组件由调用方
    显式拼（§5.3）；新增 `dial_local_parts` / `dial_date_string` / `dial_monotonic_ms`。
  - **Fail-Fast 加固**：第 2 批 18 个内建名补进 `RESERVED_NAMES`（延续第 1 批 `eprint`
    族加固），同名定义编译期拒绝，杜绝内建分支静默绕过（§20）。
  - **U 组回归**：8 组 API 端到端输出断言、C2 解析失败 Fail-Fast、C4 `char_at` 越界
    Fail-Fast、`string[].index_of` 编译期拒绝、C3 `format_float(int,..)` 零隐式转换拒绝、
    第 2 批内建同名定义被拒；A–T 扩为 A–U。
- **B/C 组缺口补齐 第 3 批：语言机制（B7 / B8 / B6）**，落在 `src/parser.rs` +
  `src/semantic.rs` + `src/codegen.rs` + `runtime/dial_rt_mem.c` + `stdlib/dial_kvmap.c`：
  - **B7 文件级只读 `const`**：支持标量 / 字符串 / 数组 / 嵌套数组 / 字符串数组 /
    结构体字面量，以及字面量上的常量表达式（可常量折叠）；const 可引用其他 const
    （解析期内联展开）；**顶层 `var` 全局变量编译期拒绝**（禁止可变全局状态红线）；
    const 重赋值 / 非编译期常量值 / 类型不匹配 / 局部变量遮蔽 const 均被拒。
  - **B8 结构体字段默认值**：`struct Point { x: int = 0, y: int = 0 }`；默认值必须是
    编译期常量；省略有默认值的字段取默认值、省略**无默认值**的字段编译期报错
    （不静默零初始化）；默认值类型不匹配按零隐式转换铁律拒绝。
  - **B6 结构体进容器**：`Point[]` 与 `map<string, Struct>`。值类型结构体装不进 8 字节
    数组 / map 值槽，故**装箱**为引用计数对象（`dial_struct_box`，布局
    `[int64 refcount][payload]`，与既有 Dial 堆对象同构）后入槽，读出时**解箱按值**拷贝；
    `put` / `get` / `contains` / `remove` / `get_or` / `values()` 全通路打通；
    map 侧新增 `KV_STRUCT(6)` 值种类（与 `KV_STRING` 同走 retain/release），
    未命中返回空指针并由编译器还原零值结构体（绝不解引用空指针）。
  - **边界（如实标注）**：B6 本批只放开「**字段全为标量**」的结构体；含句柄字段的
    结构体进容器需要字段级 ARC（装箱 / 释放时逐字段 retain/release），该能力尚未
    落地，故语义层**明确拒绝**而非让它「看起来能用」却在释放时泄漏或悬垂。
  - **顺带修好的通用缺陷**：字段访问此前只支持「变量 / 字段链」作基对象，
    `Pt{}.x` / `make().y` / `arr[i].f` 一律报「仅支持对变量/字段进行赋值」；现让
    `gen_expr` 字段分支把非 lvalue 基物化到栈上临时槽再取字段指针
    （`gen_lvalue_ptr` 保持只服务数组元素写，避免破坏 `a[i] = v` 的写回语义）。
    B7 的 const 结构体内联后「取字段」成为常态路径，此修复是其必要前提。
  - **V 组回归**（新增第 22 组）：B8 / B7 / B6 正向端到端、顶层 `var` 被拒、
    省略无默认值字段被拒、默认值类型不匹配被拒、含句柄字段结构体作数组元素 /
    map 值被拒；A–U 扩为 A–V。内存专项：3M 次 `Point[]` 建放、1M 次
    `map<string,Point>` 读写循环均在虚拟内存上限内通过（装箱对象随容器释放，无泄漏）。
  - **本批明确不碰（按裁决划清界限）**：B1（`impl` 方法，暂用 `fn` 替代）、
    B2（用户泛型，死守单态化底线）、B3（枚举 ADT，不引入 Tagged Union）；
    B5（函数名重载）与 B4（闭包）维持**永久否决**。C11（`http.dialpkg` 独立包）待落地。
- **C11 独立包第一包：`http.dialpkg`**（§37.5「常用库走独立包」的落地），
  落在 `packages/http/` + `third_party/`：
  - **按领域拆包，本轮只落地 `http`**：正则 / JSON / 摘要 / Base64 各自成包
    （`text` / `json` / `crypto`），后续批次逐个补。
  - **协议层自研**：`packages/http/src/dial_http.c` 实现 URL 解析、请求行与 Header
    拼装、响应报文解析（状态行 / 头部 / `Content-Length` / `chunked` / 读到关闭）、
    Socket 读写——线级细节留在自己手里，为后续的连接指纹定制留出空间。
  - **密码学绝不手搓**：TLS 由 **mbedTLS 3.6.2** 提供并**已接线上层**
    （`https://` 走 mbedTLS，证书校验 fail-closed：系统根 + 自定义 CA 双信任链，
    `mbedtls_ssl_get_verify_result()` 非 0 即拒绝），gzip / deflate 解压由
    **zlib 1.3.1** 提供；
    两者 **vendor 进仓库**（`third_party/`，保留上游 LICENSE 与版权声明），
    构建离线可复现；zlib 裁掉 `gz*.c`（gzip **文件** API，依赖 POSIX `lseek`，
    与 HTTP 内存解压无关且编不过 Windows / musl 目标）。
  - **两层 API**：底层「流式」（`connect` / `write` / `write_line` / `read_line` /
    `read_all`，调用方自己拼报文）+ 高层「一次请求」（`get` / `post` +
    `HttpResponse` 访问器：状态码 / 响应体 / 按名取头 / 按序号遍历）。
  - **错误与所有权口径**：包不抛 Dial 异常（不假定宿主装了异常栈），失败返回空句柄
    （`is_null` 可判），原因取自**线程局部**最近错误槽；不透明句柄**不进 ARC**，
    释放必须显式（`dial_http_close` / `dial_http_resp_free`）。
  - **链接顺序修正**（编译器侧）：静态归档由链接器从左到右扫描，符号只有在其归档
    被扫描时仍处于「未定义」状态才会被拉入。包要向 Dial 返回 `string` / `bytes`
    就必须引用标准库的 `dial_string_new` / `dial_bytes_new`，故顺序由
    `obj → runtime → stdlib → packages` 改为 `obj → runtime → packages → stdlib`。
  - **两条写包必备的 FFI 约定**（已写进包内注释）：Dial 的 `string` 跨到 C 传的是
    **载荷指针**（`[int64 len][bytes...]`），须用 `dial_string_cstr()` 取文本；
    带缓冲的读取器必须先取缓冲再读 socket，否则会漏掉已吞进缓冲的响应体并误报
    「连接提前关闭」。
  - **一处刻意不动语言**：Dial 字符串字面量只支持 `\n \t \\ \"`、**不支持 `\r`**，
    而 HTTP 行结束符是 CRLF——因此包提供 `dial_http_write_line` 负责补 CRLF，
    不为发一个 HTTP 包顺带改语言。
  - **W 组回归**（新增第 23 组）：三平台静态库构建、打包 + 安装（manifest /
    逐文件 SHA256 自检）、`import "http"` 编译运行、本地 HTTP 服务器端到端
    （200 / 404 / POST 回显 / chunked / gzip 解压 / 多响应头含同名多值 / 低层流式）、
    明文三条错误路径（https 连不可达主机 / URL 缺主机名 / 连接失败）必须明确失败；
    **TLS 1.3 子组**（同组第 7 步）：openssl 自签 CA + 本地 TLS 服务器，验证 TLS 1.3
    协商（`TLSv1.3`）、证书校验 fail-closed（清空自定义 CA 后系统根拒签自签）、
    keep-alive 连接池复用（`pool hits > 0`）、404 / 500 全绿。
- **HTTP/2.0 客户端（并入 `[v1.04]` 的后续增补，`http` 包 `1.1.0 → 1.2.0`，
  编译器版本 `1.4.0` / `STDLIB_ABI 6` 均不变）**：在既有 HTTP/1.1 + TLS 1.3 之上，
  落地 `packages/http/src/dial_http2.c` / `dial_hpack.c` / `dial_hpack_tables.h`：
  - **两条传输**：`https://` 走 TLS 1.3 + ALPN 协商 `h2`（未协商出 h2 则
    **fail-closed 拒绝降级**为 HTTP/1.1）；`http://` 走明文 **h2c prior-knowledge**
    （RFC 7540 §3.4，直发 client preface）。
  - **自研 HPACK（RFC 7541）**：请求侧统一用 literal-without-indexing / new name
    最简编码（不维护动态表）；响应侧完整实现静态表（61 项）、Huffman 解码（257 项）、
    动态表、整数前缀编码与动态表大小更新。静态表与 Huffman 码表由
    `tools/gen_hpack_tables.py` 从 Python `hpack` 库常量生成，避免手敲出错。
  - **真·并发多路复用**：新增会话式 API（`dial_http2_connect` /
    `dial_http2_request` / `dial_http2_session_free`）。一个 `Http2Session` = 一条连接，
    多个 `spawn` worker 可共享同一句柄并发提交 stream（客户端 stream id 取奇数），
    帧在单连接上按 `stream_id` 交错分发。因 mbedTLS 的 `ssl_context` 不能多线程
    并发读写，会话启动时创建**唯一 I/O 线程**独占 socket/TLS：worker 只入队帧、
    在自己的 condition variable 上等待，由 I/O 线程读帧分发后唤醒；
    `DialHttpResponse` 在 worker 自己线程里组装，reader 线程绝不构造 Dial ARC 对象。
  - **并发与跨平台**：POSIX 用 `pthread_mutex_t` / `pthread_cond_t` / `pthread_t`，
    Windows 用 `CRITICAL_SECTION` / `CONDITION_VARIABLE` / `CreateThread`；
    三平台静态库零警告零错误。为此修正了编译器 `spawn` thunk 对 `Type::Opaque`
    形参的槽宽：`Opaque` 已 lowering 成 `i8ptr`，thunk 必须按 ptr 槽读出，
    否则 load 出 `i64` 传给期望 ptr 的形参会被 LLVM verifier 拒绝。
  - **W 组回归新增 HTTP/2 子组**（同组第 9 步）：Python `h2/hpack/hyperframe`
    起本地服务器并**故意乱序响应**，6 个 worker 共享单连接、逐条断言
    `path == X-Stream`（不串台）、`conns = 1` / `streams = 6`、
    `h2` ALPN 协商成功、`no-ca rejected = true`、404 / 500 全绿。
- **浏览器一致性画像（协议层，期 1–4；并入 `[v1.04]`，编译器版本 `1.4.0` /
  `STDLIB_ABI 6` 均不变）**：让 `http` 包的 TLS 与 HTTP 行为可对照 Chrome 稳定版
  Windows/x64 基准逐项对齐，**只做协议层行为一致性，不伪造凭据、不绕验证码、
  不做批量自动化**。基准与离线比对工具在 `tests/pkg/fingerprint/`
  （`chrome_windows_x64.json` 为仓库内唯一基准，含 ClientHello 解析、内嵌原始
  record hex、HTTP 行为段与 9 项 `exemptions` 显式清单）。
  - **期 1（TLS 指纹）**：mbedTLS 以**外置 patch**（`patches/mbedtls-3.6.2-clienthello.patch`）
    重排 ClientHello 字节序 + 包内集中式 `dial_clienthello_fp.c` 配置；
    GREASE 用固定常量表（不调 RNG）、cipher / 扩展 / groups / sig_algs / ALPN
    按 Chrome 144 实测序发射，SNI 双路径（域名走 `d`，IP 走 `i`）。`ja3` / `ja4`
    （`ja4_a` 含 `h2` 与 ALPN 首项，故 ALPN 双 offer 归期 1 而非期 2）全字段对齐
    基准。**24 项 pass、0 MISMATCH**，9 项豁免声明（ECH HPKE 加密、证书压缩、
    `br/zstd`、ALPS、逐字节一致等，均不做）。
  - **期 2（HTTP 行为）**：画像数据抽成 H1/H2 共用的**单一数据源**
    `dial_profile.{c,h}`（头序 / 头值 / UA / Sec-CH-UA / H2 SETTINGS 六参数 /
    WINDOW_UPDATE 只写一次），`default` 保守、`chrome` 完整。`dial_hpack.c` 请求侧
    升级静态表索引 + Huffman（RFC 7541 合规）；`dial_http2.c` 连接前导补齐
    `preface → SETTINGS → WINDOW_UPDATE → HEADERS`。录制器 `record_http.py` 真实
    解析 HPACK（从 `dial_hpack_tables.h` 读码表 + 内置静态表），伪头序 / static_index
    / huffman 全由解码得出。**修复**：chrome 声明 `HEADER_TABLE_SIZE=65536` 时服务端
    发 dynamic table size update，原解码上限写死 4096 误判超限 → 按 RFC 7541 §4.2
    改为等于本端声明值。`compare_fp.py --behavior`：**TLS + HTTP 行为 34 项全 pass、
    0 MISMATCH、0 skip**，9 项豁免声明。
  - **期 3（profile 一等公民）**：当前画像由进程级全局改为 **`_Thread_local`**
    （每线程独立持有，多线程各自发不同画像请求互不干扰）；新增 `.diali` 诊断
    getter `dial_http_profile(): string` 回读当前线程生效的画像名。`check_profile_thread.c`
    断言主线程设 `chrome`、子线程不设各读到 `chrome` / `default`，证明线程局部。
  - **期 4（W 组步骤 11）**：`tests/pkg/use_http_profile.dial` 接入 W 组主回归
    （同组第 11 步，复用既有服务器与已安装包），校验 `set_profile` + `dial_http_profile`
    回读一致，且 chrome 画像真实发出的 `User-Agent`（含 `Chrome/144.0.0.0`）、
    `Sec-CH-UA` / `Sec-Fetch-*` 被服务端收到；切回 `default` 后 UA 回落
    `dial-http/1.1` 且不再发 Sec-CH-UA——把「画像确实在主回归里生效」锁进 CI。
    离线指纹比对（逐字段对齐基准）仍按 `tests/pkg/fingerprint/README.md` 手动跑，
    不进 CI（需录制真实 Chrome，且含随机字节，无法离线复现逐字节一致）。
- **宪法裁决入文档（§37）**：`STDLIB_ABI 5 → 6`（`make_stdlib_pkg.py` 默认、
  CI 两处、`dialc new` 骨架同步，后者改为引用 `package::STDLIB_ABI` 消除硬编码）；
  术语统一为「绝不引入可变全局状态」；「函数名重载」「闭包」写入**永久否决**；
  B7（文件级 `const` 只读常量、禁 `var` 全局）与 B8（结构体显式默认值、
  未声明默认值仍报错）的精确语义先于实现写清；C11 常用库确立为**独立 `.dialpkg`**。

- **C11 独立包补齐余下三包（`[v1.04]` 后续增补；编译器版本 `1.4.0` /
  `STDLIB_ABI 6` 均不变，三包各自 `version = 1.0.0`、`abi = 6`）**：
  §37.5 裁决「正则 / JSON / 摘要 / Base64 不进标准库、各自成包」后 `http` 包
  先落地；本批把 `text` / `json` / `crypto` 一次性补齐，并接入 X / Y / Z 三组
  回归。四包共用同一套 FFI 约定（`string` / `bytes` 跨到 C 都是 `DialString`
  句柄，须用 `dial_string_cstr()` / `dial_string_length()` 取数据，返回用
  `dial_string_new()` / `dial_bytes_new()` 构造句柄），三平台静态库均由
  `zig cc` 交叉编译、零警告零错误。包内机器码的契约边界由各自 `.diali` 的
  SHA256 校验保证；`abi = 6` 表示「编译此包所依据的标准库 ABI」，不是包版本。
  - **`text.dialpkg`（自研 Thompson NFA 正则，零第三方依赖）**：
    `packages/text/src/dial_regex.c` 把正则编译成 NFA 指令、由 Pike VM 执行，
    **线性时间**，从根上避开 `(a+)+b` 这类回溯爆炸。支持字面量 / `.` /
    字符类（含取反与区间）/ `\d \D \w \W \s \S` / 贪婪 `* + ?` / 分支 `|` /
    捕获组与非捕获组 / `^ $ \b`；**明确不做**后向引用、lookaround、懒惰量词、
    `{m,n}`、Unicode 属性类——能力边界写进 `text.diali`，不许当已支持。
    按字节匹配，与 Dial UTF-8 字节串一致；捕获组最多 9 个，指令 / 线程上限
    各 2048。两处已知语义边界同样写进契约：`(a*)*b` 上本引擎取最后一次
    非空迭代 `"aaa"`（PCRE / Python 会再跑一次空迭代把组 1 覆盖成空串，
    本引擎 `seen` 去重不允许空迭代重入同状态）；搜索从每个候选起点重跑 VM，
    最坏 `O(n²·m)`，定位为短文本 / 配置 / 日志行，不建议 MB 级全文扫描。
    独立 C harness 25 例全绿、线性时间用例 0.0033s；X 组 41 项断言零 `= bad`。
  - **`json.dialpkg`（自研递归下降解析 + 紧凑序列化，零第三方依赖）**：
    `.diali` 类型白名单只有裸 `map`（int→int），没有泛型容器，故整棵值树封在
    `type Json;` 不透明句柄里。**所有权只有一条规则**：所有返回 `Json` 的函数
    都返回独立拥有的句柄，调用方一律 `dial_json_free`；`array_push` /
    `object_set` 对 `child` 深拷贝，不存在借用句柄 / 双重释放的灰色地带。
    严格模式把尾随逗号、多余根值、未转义控制字符、非法 `\u`、孤立代理项
    一律报错并给字节偏移；嵌套深度上限 64；**明确拒绝 `\u0000`**（会编码成
    内嵌 NUL，被 C 字符串静默截断，Fail-Fast 不假装成功）。数字分两路：
    整数走 `int64_t` 单独保存，`9007199254740993`（2^53+1）不丢精度；
    浮点走 `double`。独立 C harness 35 例 + 2 专项（70 层嵌套拒绝、
    `js_clone()` 独立性）全绿；Y 组 45 项断言零 `= bad`。
  - **`crypto.dialpkg`（复用 vendor mbedTLS 3.6.2，密码学绝不手搓）**：
    `packages/crypto/src/dial_crypto.c` 只做 FFI 包装——五种摘要
    （MD5 / SHA-1 / SHA-256 / SHA-512 / SHA3-256）走 `mbedtls_md()`、
    HMAC-SHA256 走 `mbedtls_md_hmac()`、Base64 走
    `mbedtls_base64_encode/decode()`，**不自己拼 ipad/opad，不自己实现摘要**。
    每个算法给 `string` 与 `bytes` 两套入口（后者可含内嵌 NUL / 非法 UTF-8），
    另有按算法名返回原始摘要字节的 `dial_crypto_digest`。MD5 / SHA-1 在契约里
    **明确标注仅用于校验与兼容、不可用于新的安全设计**。失败一律返回空值
    并置线程局部错误码：`string` 类返回空串，`bytes` 类返回长度 0 的空缓冲
    ——因为 Dial 的 `is_null` 准入名单不含 `Type::Bytes`，返回 NULL 句柄会让
    语言层无法判别且 NULL 进入 ARC 有隐患。`build.sh` 把 mbedTLS **全量归档**
    （`md.c` 因 `MBEDTLS_PSA_CRYPTO_C` / `MBEDTLS_RIPEMD160_C` 已启用会引用
    psa / ripemd160 符号，只编摘要子集会在链接期缺符号）；静态库按需抽取，
    最终可执行文件不因此变大。独立 C harness 35 例（NIST / RFC 标准向量、
    含内嵌 NUL 的二进制、1 MiB 长输入）全绿，并顺带验证归档的链接符号闭包；
    Z 组 29 项断言零 `= bad`。
  - **实测**：全量回归 A→Z 二十六组 `EXIT=0`；三包三平台静态库
    （linux-gnu / musl / windows-gnu）均零警告零错误；CI 阶段 8 改为四包统一
    构建 / 打包，Release 资产新增三包。

- **官网更新日志改为「版本索引 + 每版本分页」（`[v1.04]` 后续增补，
  `CHANGELOG.md` 原文一字不改）**：`changelog.html` 单文件已达 213 KB，移动端
  首屏渲染与旧版本定位成本高，且重复锚点靠 `_1` 后缀区分、版本一多更难维护。
  改的是生成器 `tools/build_web.py`，**不是手拆产物**：`docx/changelog.html`
  只放 31 行版本索引（213 KB → 6.1 KB），`docx/changelog/` 下每版本一页存
  对应条目的原文渲染，另出 `overview.html`（版本规则）/ `index.json`（搜索与
  newer / older 导航）。生成器内置两道自检：分段拼接结果与 `CHANGELOG.md`
  逐字节相等、生成页数 == 段数 + 1；二次生成逐字节一致（满足 CI 的
  `git diff --exit-code -- docx/`）。正文纯文本旧 / 新逐字符相等（75,397 字符），
  重复锚点后缀 73 → 1。**记录粒度不变，只改变组织方式**。

### Changed

- **Dial 函数的链接可见性按 `@export` 收口**：v1.04 之前**所有** Dial 函数都是 LLVM
  默认的 `External` linkage，等于每个 Dial 程序都在往全局符号空间里倒函数名——
  既会与别的目标文件里的同名符号撞车，也让「哪些是给 C 用的」无从表达。现在
  **只有 `@export` 标注的函数与 `main` 是全局符号**，其余收成 `Private` linkage：
  模块内照常递归、照常取函数指针（传给 C 回调也照常工作），但**不导出**、不被
  死代码消除误删。依赖「Dial 函数能被 C 按名字链接」的代码请补 `@export`
  （见 [README §33.9](README.md#339-export把-dial-函数导出给-c-调用)）。
- **所有 Dial 函数的 LLVM 签名统一为 C ABI 宽度**：`bool` 内部 i1 → 边界 i32（C 默认整型提升），
  `byte` 内部 i64 → 边界 i8（C 的 `uint8_t`）；函数体内部仍是 i1 / i64，逻辑零改动。
  函数入口 / 出口 / 调用点 / HOF thunk / spawn thunk 五处做**对称** `abi_adapt` 转换，
  靠 A–Z 全量回归兜底。这是「C 能安全调用任意 Dial 函数」的前提。
- `STDLIB_ABI` **4 → 5**（新增 `stdlib/dial_ffi.c` 四个数据总线函数），
  编译器版本 `1.3.0 → 1.4.0`；`make_stdlib_pkg.py` 默认 `--abi 5`、CI `--abi 5`。
- 系统库（libc / libm）继续由 C 编译器**默认动态解析**，不进 `link`、不进 LTO 链路；
  所有 `.dialpkg` 仍**强制 `.a` / `.lib` 静态归档**并校验 ar 魔数 `!<arch>\n`，
  J 组三条硬规则一字未动。语言层 `link` 则**静态 / 动态并存**：动态库按扩展名验
  ELF / PE / Mach-O 魔数并注入 rpath，由源码作者显式承担运行期依赖；不写 `link`
  动态库的程序仍满足「只链 libc」的默认产物契约（测试 C 组照常断言）。
- **运行时诊断带位置（A7）**：`panic` / `assert` 失败 / 未捕获异常现在输出
  `文件:行:列`（文件名与行列是编译期常量，运行期零额外开销）；新增
  `dial_panic_loc` / `dial_assert_fail_loc` 两个 C 入口，旧符号保留给其它调用点。
  首行文案与旧版完全一致，G 组回归不受影响。
- **Fail-Fast 收紧（A3）**：旧版 `dialc build a.dial b.dial` 打印「未知参数」后
  **仍以 `rc=0` 产出 a 的可执行文件**（多余输入被静默吞掉）；现在任何未知参数一律
  非零退出，多出来的是 `.dial` 时附带 §34.2 本地模块写法的指引。同一规则推广到全部新子命令。
- **`-g` 隐含 O0**：`emit_object()` 在 `debug_info` 且非 `--fast` 时用
  `OptimizationLevel::None`，避免默认 O2 把栈槽变量优化成 `<optimized out>` 而让
  `print var_name` 失效；`-g --fast` 仍走用户明确要求的 O3 + SIMD/LTO，并打印兼容提示。
  安全等级、运行期检查、C Runtime 注入与 `.dialpkg` 契约均不变。

### Fixed

- 函数实参类型不匹配的诊断：参数序号此前被误填成形参类型名（显示「第 rawptr 个参数」），
  且泛型类型只显示 `rawptr` 而非完整签名；现改为正确序号 + `Type::display()` 完整类型。
- **D 组文档 / 诊断漂移**（按 `REPORT_v1.04_gaps.md` 实测校正）：
  `examples/string_demo.dial` 的「`+` 不对 string 重载」注释、README §16「print 恰好 1 参」、
  §5.5 / §15「只有 `str(char)`」、§17.2「截至 v1.02 / 函数指针不支持 / 源码模块未实现」、
  `Point[]` 报错里「元素类型只支持 int 与句柄」全部改为与实现一致；
  「找不到已安装的包」补上本地源码模块指引（LSP 稳定错误码与文案前缀不变）。
- `dialc test` 逐块链接时的 stdlib 编译日志改为静默（`LinkOpts.quiet`），
  避免把测试结果淹没在重复的 ℹ️ 行里。

> 📄 本版的理由、实测数据与边界见 [rationale/v1.04.md](rationale/v1.04.md)。

---

## [v1.0 里程碑] — 分版记录（v1.00 ~ v1.03）

> 📄 本版的理由、实测数据与边界见 [rationale/v1-0-milestone.md](rationale/v1-0-milestone.md)。

---

## [CI 修复与 v1.0 标签重打] — 2026-11-06

### Fixed

- **Windows 核心冒烟**：`stdlib/dial_env.c` 原先依赖 POSIX 的
  `srandom()` / `random()` / `clock_gettime()` / `nanosleep()`；MinGW-w64 只提供
  `rand`（`RAND_MAX` 仅 32767），`clock_gettime` / `nanosleep` 则靠 winpthreads
  才勉强可用，属于脆弱依赖。现改为：
  - 自实现 `xorshift64*` PRNG（纯 64 位整数运算），一次产出 64 位、掩掉符号位
    得到 `[0, 2^63)` 的非负随机数；三平台行为一致，不再依赖任何 libc 随机函数；
  - `dial_now()` / `dial_sleep()` 在 `_WIN32` 下改用 Win32 原生
    `GetSystemTimeAsFileTime`（FILETIME → Unix 纪元秒）与 `Sleep`，非 Windows
    仍走 `clock_gettime` / `nanosleep`。
- **Linux 全量测试**：`tools/cve_check.py` 的 `check_110_leak`（CVE-2026-DIAL-007）
  编译清单漏了 `stdlib/dial_array.c`，导致 `dial_string_split()` →
  `undefined reference to dial_array_new`，而检测器把「编译 / 链接失败」误判成
  「❌ 仍可利用」并阻断发布。补全清单后该用例输出 `FIXED`（3/3 残留元素已释放），
  全套 **8/8 个 CVE 已确认修复**。

> 📄 本版的理由、实测数据与边界见 [rationale/ci-fix-v1-0-retag.md](rationale/ci-fix-v1-0-retag.md)。

---

## [安装与发布流程调整] — 2026-11-06

> 📄 本版的理由、实测数据与边界见 [rationale/install-release-flow.md](rationale/install-release-flow.md)。

---

## [v1.03] — 2026-11-05

### Added

**一等函数指针 `fn(A, B): R`**
- `Type::FnPtr` 底层就是 LLVM `ptr`（纯代码地址）：函数名可直接作右值、可赋值 /
  传参 / 返回 / 存变量；`cb(x)` 走**精确签名**的间接调用（LLVM 18 的
  `build_indirect_call`），参数个数与类型编译期校验；
- **没有闭包**：无环境对象、无 `Fn` / `FnMut` / `FnOnce` 分类、无匿名函数语法；
  回调要带状态必须显式传 `rawptr<void>`，裸指针生命周期由用户自己负责；
- 不受 ARC 管理、无堆分配、**无需释放**：`drop(cb)`、`print(cb)`、
  `with cb as p { }` 都在编译期拦下并给出明确指引；
- `spawn` 首参仍要求顶层具名函数（线程 thunk 需按目标函数静态生成），但函数指针
  可以作为普通回调参数传递；相关诊断与注释里「Dial 没有一等函数」的旧表述已同步。

**同类型变参 `...xs: T`**
- 在 `semantic.rs` 的 `analyze` 开头**做 AST 脱糖**：形参变成 `xs: T[]`，调用点尾部
  实参打包成带元素类型标注的数组字面量；之后的类型检查 / LSP / codegen 只看得见
  一个普通数组；
- **严禁隐藏类型标签 / 装箱 / 运行期分派**；`...xs` 必须是参数列表最后一项，
  不能与默认实参共存，元素类型须能作为数组元素；
- **拒绝异构**：`sum(1, "a")` 编译期报「元素类型不一致」并引导「显式定义结构体或
  自定义类型包」；
- 变参数组是**普通 ARC 数组**：函数形参进入时 retain、退出时 release；调用点的临时
  数组由编译器在调用结束后插入 release（20 万次调用峰值 RSS **1348 KB**，无泄漏）。
  **绝不能**把裸指针的「手动释放」规则套到变参数组上。

**`extern fn` 的 C 风格异构变参（FFI 特例）**
- `.diali` 声明支持末尾 `...`：`extern fn demo_vprintf(fmt: string, ...): int;`，
  LLVM 侧声明为 varargs 函数；
- 固定参数仍做严格类型检查，尾部参数按 C ABI 直传（`bool` 的 i1 会 zext 成 i64，
  `int` / `char` / `byte` 本就是 i64、`float` 本就是 f64）；
- 只属于包 FFI，Dial 自身函数永远走同类型脱糖；无隐藏类型标签 / 装箱。

**stdlib 高阶数组方法（接受回调）**
- `arr.map(f[, state])` / `arr.filter(f[, state])` / `arr.sort_by(cmp[, state])`；
- 回调签名固定带 `rawptr<void>` 状态参数，状态由 C 运行时透传回回调（无闭包）；
- C 侧新增 `dial_array_hof`，只做遍历 / 建数组 / 插入排序；回调经编译器生成的
  **精确签名 thunk** 间接调用（8 字节位模式 ↔ 类型值适配），C 完全不知道回调类型；
- 仅支持标量元素（int / float / bool / byte / char）；`sort_by` 用插入排序而非
  `qsort`，因为后者的比较器拿不到 `fn_raw` / `state`，无法透传状态。

### Fixed

- 修正 `len` 内建的错误提示：数组 / 字符串长度应写 `.length()`，此前误写 `.len()`；
- `ArrayLit` 带期望元素类型时原本跳过元素一致性校验（旧行为对声明标注的空数组无害），
  会让异构变参实参静默塞进数组——改为**无条件逐元素校验**，这是「严禁异构变参」的落点。

### Changed / 发布协同

- `STDLIB_ABI` **3 → 4**（新增 `dial_array_hof`），官方标准库包版本 `1.2.0 → 1.3.0`；
  `make_stdlib_pkg.py`、CI、一键安装脚本（sh / ps1）同步；
- 4 个正向示例（`fnptr_demo` / `varargs_demo` / `callback_state_demo` / `hof_demo`）、
  14 个负向用例；H 组新增 C 变参 `extern fn` 的正向与负向回归；
- `tests/run_tests.sh` A–O 全套通过；版本 / 链接 / 官网检查通过；零编译警告。
- **版本层级**：`[v1.00]` ~ `[v1.03]` 统一归入上方「v1.0 里程碑」大标题下作为分版记录；
  收尾特性至此全部落地，`v1.0` 里程碑标签已打在本次提交上，并随 `main` 一起推送。

> 📄 本版的理由、实测数据与边界见 [rationale/v1.03.md](rationale/v1.03.md)。

---

## [v1.02] — 2026-11-05

### Added

**`with` 资源作用域块**
- 两种等价写法：`with 资源 as 名 { ... }` 与 `with 名 = 资源 { ... }`；
- 块末自动释放：`file` → close、`thread` → join、裸指针 → raw_free；
- 覆盖**全部**出口（正常落穿 / return / break / continue / throw），且互斥地各执行一次
  ——它直接复用既有的 `defer` 机制，而不是另发明一条出口路径；
- 块内已显式 close / join / free 的，块末**跳过**（二次 `fclose` 是 UB）；
- 只管「需要显式释放」的资源：ARC 句柄用 `with` 是多余的，编译期直接拒绝。

**`goto` 与标签**
- `label 名字:` + `goto 名字;`，标签函数内唯一，允许向前 / 向后跳；
- **三条死锁**（编译期硬拦）：
  ① 禁止跨过变量初始化（Zig 式严格，两个方向都禁；向前跳会读到未初始化的变量，
  向后跳会重复初始化并泄漏前一份）。判定用**全局深度优先语句序号**而非同块下标，
  因此「从内层块跳到外层标签、跳过外层声明」也被拦下（实测那是真实的 UB：`print(x)`
  会打印栈上垃圾值）；② 禁止跳出 / 跳进 `with` 块（否则自动释放被绕过
  或重复）；③ 禁止跨越 `try` 边界（try 依赖 setjmp/longjmp 保存栈帧，跨界跳转会毁掉
  jmp_buf，不同 try 块之间互跳同样禁止）；
- 另三条边界一并拦下：目标标签未定义、标签重复定义、**跳进嵌套块**（那里面的变量尚未初始化）；
- 跳到外层块的标签时，被跨越的若干层作用域**照常清理**（defer 按 LIFO 触发、
  ARC 句柄逐个 release）——这正是 goto 作为「底层清理利器」的价值。

**裸指针 `rawptr<T>`**
- 核心红线：**绝对不受 ARC 管理**——编译器不 retain、不在作用域结束时释放；
  `drop(p)` 对裸指针的唯一作用就是 `free`；
- 与 ARC 容器、裸指针之间**都没有隐式转换**：取 `string` 的字节缓冲区必须显式
  `ptr_cast<rawptr<byte>>(s)`，换视图必须显式 `ptr_cast<rawptr<int>>(p)`；
- `ptr_cast` 从 `string` / `bytes` 取到的是**数据缓冲区**的借用（原对象仍受 ARC 管理，
  原对象释放后该指针即悬垂——使用者的责任）；
- 指针算术**显式且带类型**：只有 `p.offset(n)`，步长由元素类型决定；
  `rawptr<void>` 无元素大小，**禁止**算术（须先 `ptr_cast`）；
- 解引用宽度按元素：`byte` / `bool` 1 字节、`char` 4 字节、`int` / `float` / 句柄 8 字节；
- `&x` 只允许对**可变的栈上值类型**变量取址：对 `let` 取址会绕过不可变约束 → 拒绝；
  对堆对象取址拿到的只是槽位地址 → 拒绝；
- 配套运行时：`dial_raw_alloc`（分配失败 Fail-Fast，绝不返回 NULL）/ `dial_raw_free`。

**包的不透明句柄（`Type::Opaque`）补齐文档与诊断**
- 沿用既有 `type Db;`（`.diali` 声明），**不新增语法、不新增运行时**；
- 明确契约：与裸指针同属「不受 ARC 管束」的一类——编译器不 retain / release、
  作用域结束不释放，必须由包自己的 `close` / `free` 手动释放；与 ARC 容器之间
  无隐式转换；不可打印；与 `rawptr<T>` 的区别写清（具名无布局 / 带元素类型可解引用）；
- **`drop(db)` 由「静默 no-op + 误标变量已转移」改为编译期报错并给出指引**
  （此前 `drop` 对不透明句柄什么都不做，却会禁用该变量，容易让人误以为资源已归还）；
- 作为数组 / 通道元素时按 RC 类别 retain / release，文档明确「包作者须保证句柄
  布局与 `dial_rc_*` 兼容」这一责任边界。

### ⚠ 破坏性变更（标准库 ABI 递增）

- **`stdlib` 包 ABI 由 2 递增到 3**（`STDLIB_ABI`）。原因：标准库新增
  `stdlib/dial_raw.c`（`dial_raw_alloc` / `dial_raw_free`），供裸指针的
  `raw_alloc` / `raw_free` / `drop(p)` / `with` 自动释放使用。与 v1.01 不同，
  这两个符号是**条件引用**（只有用户代码用到裸指针时才产生调用），但装着 abi=2
  旧包的用户只要写了裸指针程序，仍会撞上难懂的
  `undefined symbol: dial_raw_alloc`。判据是「**编译器是否可能要求旧包没有的符号**」，
  因此同样递增——ABI 校验这道安全网不能因为引用是条件性的就放过旧包。
  `build_stdlib.sh` 与 LTO 编译路径都用 `*.c` / `read_dir` 收集源文件，
  `dial_raw.c` 会**自动**进入静态库与官方包，无需手工登记清单。
- 配套同步：官方标准库包版本 `1.1.0 → 1.2.0`，打包默认 `--abi 3`，
  CI 发布作业 `--abi 3`，一键安装脚本的 `STDLIB_RELEASE` 默认标签
  `stdlib-v1.1.0 → stdlib-v1.2.0`（标准库仍**独立发版**，tag `stdlib-v*`）。
- 发布协同项：v1.01 ~ v1.03 的提交已随 `main` 一并推送；按版本号的发布标签
  （v1.03 起为 `v1.3.0` 编译器与 `stdlib-v1.3.0` 标准库包，见上方 v1.03 条目）
  由维护者另行决定何时打；`v1.0` 里程碑标签已在 `[v1.03]` 之后打上，见文件开头说明。

### Fixed

- **`break` / `continue` 未释放被跳出的作用域**（既有缺陷）：外层臂的 `release_scope_locals`
  位于落穿路径上，而 break/continue 恰恰走不到那条路径——循环体的局部因此永远不释放。
  实测 `continue` 循环 20 万轮（每轮一个 `string`）峰值 **13892 KB**，修正后 **1620 KB**。
  修法与 goto 同源：跳转前补发一份「被跳出作用域」的释放代码（两条路径互斥，安全）。

> 📄 本版的理由、实测数据与边界见 [rationale/v1.02.md](rationale/v1.02.md)。

---

## [v1.01] — 2026-11-05

### Added

**数组元素类型扩展**
- `float[]` / `byte[]` / `bool[]` / `char[]` 现在都是合法的动态数组元素；
- 底层沿用 `int64` 槽位：`float` 存 **IEEE754 位模式**（bitcast，绝不 `fptosi`
  截断小数——否则 `3.25` 会被写成 `3`）、`bool` 走 `i1 ↔ i64`、
  `byte` / `char` 与 `int` 同为 `i64`；句柄元素仍按元素 ARC 管理；
- 零隐式转换不松动：`float[]` 里 `push(3)`（int 字面量）仍然编译失败。

**空数组字面量 `[]`**
- `var a: int[] = [];` 合法：元素类型由变量标注提供；
- 裸写 `var a = [];` 仍编译失败——编译器不替你猜。

**数组删除元素**
- `a.remove(i)`：删除下标处元素并返回它；越界当场 Fail-Fast；
- `a.pop()`：弹出末元素并返回它；空数组当场 Fail-Fast；
- 数组自身的变化（COW 可能更换指针）由编译器写回接收变量。

**`drop()`：显式提前释放**
- 句柄类（`string` / `bytes` / `array` / `map` / `map<K,V>` / `set` / `error` /
  `channel`）立即调用对应 `*_release`；`file` 走 `dial_file_close`；
  `thread` 走 `dial_thread_join`；标量与结构体是 **no-op**（值类型无所有权）；
- 句柄被 `drop` 后进入「已转移」状态：此后任何使用都在**编译期**被 use-after-move
  诊断拦下，杜绝 use-after-free；标量 drop **不**标记 moved（值类型，drop 后仍可用）。

**字符串方法**
- `s.split(sep) -> string[]`：按分隔符切分（空分隔符整串为唯一元素）；
- `s.replace(from, to) -> string`：替换全部出现，返回新串（原串不变）；
- `s.trim() -> string`：去首尾空白；
- `s.index_of(sub) -> int`：首次出现的字节偏移，未找到返回 `-1`。

**环境 / 时间 / 随机内建**
- `args() -> string[]`：命令行参数（`argv[1..]`），每次调用返回独立副本；
- `now() -> float`：Unix 纪元起的秒数（含小数）；
- `sleep(ms)`：休眠毫秒；`random() -> int`：`[0, 2^63)` 伪随机整数；
- 为拿到 `argv`，编译器生成的 `main` 入口改为 `int main(int, char**)` 并在入口
  调用 `dial_env_init(argc, argv)`——对用户代码完全透明。

**泛型 `map<K,V>` 支持 `for (k, v in m)`**
- v1.00 时泛型 map 只能 `keys()` / `values()` 拿快照再绕路迭代；
- 现在直接可写，且**键 / 值按真实类型绑定**（`map<string,int>` 的 `k` 是 `string`）；
- 所有权口径：循环里的 `k` / `v` 均为**借来**的（键借自键快照、值借自表本身，
  都未 `retain`），故循环末尾与函数出口**不释放**它们，避免重复释放表内元素。

**数组排序 `sort()`**
- `a.sort()`：原地升序排序，遵守 COW 契约（被共享时先克隆，不影响其他持有者）；
- 只支持可比较的标量元素（`int` / `float` / `byte` / `char` / `bool`），
  `string[]` 等句柄元素编译期拒绝；
- `float` 按 **IEEE754 数值序**比较（负数正确排在最前），绝非位模式比较。

### Changed

- `main` 的 LLVM 签名由 `i32 main()` 改为 `i32 main(i64, i8*)`（接收 `argc` / `argv`）。
  这是 C ABI 层面透明的变化，既有程序行为不变。

### ⚠ 破坏性变更（标准库 ABI 递增）

- **`stdlib` 包 ABI 由 1 递增到 2**（`STDLIB_ABI`）。原因：标准库新增
  `dial_env.c`（`dial_env_init` / `dial_args` / `dial_now` / `dial_random` /
  `dial_sleep`）与 `dial_array_sort` 等导出符号，且编译器生成的 `main` 入口会
  **调用** `dial_env_init`——旧标准库包（abi=1）链接新编译器必然报出难懂的
  `undefined symbol: dial_env_init`。递增 ABI 后，旧包在**链接期**就被明确拦下，
  并给出「卸载旧包 → 重装匹配版本」的重装指引，而不是让用户去追一个符号错误。
- 配套同步：官方标准库包版本 `1.0.0 → 1.1.0`，一键安装脚本的
  `STDLIB_RELEASE` 默认标签 `stdlib-v1.0.0 → stdlib-v1.1.0`
  （标准库仍**独立发版**，tag `stdlib-v*`）。
- 发布协同项：本次发布需要**两个标签**——`v1.1.0`（编译器）与
  `stdlib-v1.1.0`（标准库包）。

> 📄 本版的理由、实测数据与边界见 [rationale/v1.01.md](rationale/v1.01.md)。

---

## [v1.00] — 2026-11-04

### Added（五组语言特性）

**① 运算符与字面量**
- 复合赋值 `+= -= *= /= %=`、自增自减 `++` / `--`（编译期脱糖为普通赋值）；
- 位运算 `& | ^ ~ << >>`（仅 `int`）；
- 进制字面量 `0x` / `0b` / `0o`、数字分隔符 `_`；
- 三元表达式 `cond ? a : b`（`cond` 必须是 `bool`，两分支类型必须相同）。
- 正向示例：`examples/op_demo.dial`。

**② 迭代与控制流**
- `for (i in a..b step s)` 步长循环（`step` 必须是 `int` 且为正）；
- `for (v in arr)` 数组遍历、`for (k, v in m)` 哈希表遍历、`for (v in s)` 集合遍历
  （集合遍历编译期自动插入键集快照并纳入函数出口释放）；
- `match` 模式匹配（int / bool / char / string 模式，必须有 `_` 通配分支）；
- `defer` 延迟语句（LIFO，覆盖正常落穿 / `return` / `break` / `continue` / `throw`）；
- `do { ... } while (cond);`。
- 正向示例：`examples/control_demo.dial`、`defer_demo.dial`、`control_edge_demo.dial`。

**③ 函数与内建**
- 默认参数（带默认值的形参必须连续位于末尾；在调用点补齐，不改变 LLVM 函数签名）；
- `print` / `print_raw` 改为 variadic；
- 内建 `assert` / `panic` / `exit` / `min` / `max` / `abs` / `sqrt` / `pow` /
  `floor` / `ceil` / `round`（浮点数学函数最终链接加入 `-lm`）。
- 正向示例：`examples/builtin_demo.dial`。

**④ 表达力增强**
- 字符串插值 `"n=${n}"`（逐段转字符串 + concat；插值表达式只允许可转字符串的类型）；
- 文件级常量 `const NAME: T = 字面量;`（编译期内联，禁止遮蔽与重新赋值）。
- 正向示例：`examples/interp_const_demo.dial`。

**⑤ 重型类型与容器**
- `enum` int-backed 名义类型（变体 `Name.Variant` 编译期内联为整数字面量，默认值 0 起递增，可显式指定）；
- 数组切片 `arr[from..to]`（半开区间；句柄元素逐个 retain，与原数组 ARC 独立）；
- `set<int>` 集合（`new_set()` / `add` / `contains` / `remove` / `size` / `values`）；
- **泛型 `map<K,V>`**（独立 C 运行时 `stdlib/dial_kvmap.c`）：
  - 键支持 `int` / `float` / `bool` / `char` / `byte` / `string`（string 键按条目 ARC 管理）；
  - 值支持标量 `int` / `float` / `bool` / `char` / `byte`（float 以 IEEE754 位模式存储，绝不丢小数）；
  - 方法 `put` / `get` / `contains` / `remove` / `size` / `keys` / `values`；
  - COW 克隆、扩容重哈希、墓碑法删除，全部逐条目 retain / release。
- 正向示例：`examples/container_demo.dial`、`set_demo.dial`、`kvmap_demo.dial`。

**string 值全链路（v1.10）**：泛型 map 的 `V = string` **全方法支持**——`get`/`remove` 未命中返回
共享的 immortal 静态空串（借用语义、永不释放），根除旧版默认对象的所有权移交歧义；`values()` 返回
`string[]` 快照（逐元素 retain，改快照不影响原表）；`get_or()` 对 string 返回已拥有值，C 侧补 retain、
两端结算默认实参所有权，命中/未命中 × 字面量/借用变量默认值四象限计数平衡。5 万次压测 + safety=2
heap assert 验证无 double-free / use-after-free，双后端输出逐字节一致。

### Fixed
- **开放寻址哈希表删除采用「左移紧凑」导致键不可达**：`dial_map_remove` 与新
  `dial_kvmap_remove` 改为**墓碑标记 + 负载重建**。2 万键压测里删除 1 万个后 `size`
  曾只剩 13082（应为 10000），修复后正确；`set<int>` 与 `for (k, v in m)` 同时受益。
- `panic` 前 stdout 缓冲内容丢失：`dial_panic()` 增加 `fflush(NULL)`。

### Changed
- `print` 中间参数改为原样输出、最后一个参数补换行；`print_raw` 全部参数原样输出。
- trap 接口补齐 `dial_assert_fail` / `dial_exit` / `dial_panic`；`dial_map_keys()` 供迭代使用。
- LSP 语义 token / 固定文本 / 符号索引补齐所有新增关键字、运算符与语句变体。

> 📄 本版的理由、实测数据与边界见 [rationale/v1.00.md](rationale/v1.00.md)。

---

## [v0.99.1] — 2026-11-03

### Fixed（修复）

- **`a[i] = v` 下标赋值在 COW 守卫上误判，导致每次赋值都深拷贝整个数组**：
  数组运行时在「独占」路径（`refcount == 1`）上**多做了 `retain`**，又在「共享」路径
  （`refcount > 1`）上**漏掉了对旧 buffer 的 `release`**——与 `dial_map_put` 已有的
  正确写法（独占时原样移交所有权、共享时克隆并释放旧引用）相悖。
  于是每写一次下标赋值引用计数就 +1，COW 守卫 `refcount > 1` 从此**恒真**：
  **每一次 `a[i] = v` 都深拷贝整个数组**。
  - 后果：`a[i] = v` 循环 N 次即需 **O(N²)** 内存。实测 100 万次写需约 **8 TB**，
    进程被 **OOM Killed**（`sys` 时间高达 3 秒）；同一程序修复后峰值约 **10 MB**、
    耗时 0.01 秒。
  - 根因定位手段：在 `dial_array_set` 入口用 gdb 读 `((int64_t*)a)[-1]`，
    可见引用计数在 `2 → 1 → 2 → 1 → 2` 之间震荡（数组明明由槽位独占持有）。
  - 修复：`dial_array_set` 与 `grow`（`push` 路径）改为与 `dial_map_put` 完全同款的
    所有权交接——独占时原样返回、**不 retain 不 release**；共享时克隆并 `release` 旧 buffer。
  - 附带修复：覆盖槽位时改为**先 `retain` 新元素、再 `release` 旧元素**。
    原顺序在「旧元素 == 新元素」（如 `a[0] = a[0]`）时会把计数打到 0（`free`），
    紧接着又对已释放内存 `retain` —— use-after-free。
- **`dial_array_clone` 在独占数组上泄漏元素的引用计数**：旧实现对「与原数组同一块
  buffer 的别名」也调用 `retain_elems`，等于把同一批元素的引用计数凭空 +1，
  而数组释放时只释放一次——元素引用计数净多 1，永久泄漏。
  现 `clone()` 一律产出**独立副本**（先 `retain` 迫使深拷贝，再抵消该次 `retain`）。
- **嵌套下标赋值 `m[i][j] = v` 的所有权结算错位**：内层数组是从外层数组元素槽
  **借来**的指针，其那一份引用属于外层数组；若内层 `set` 按「消耗引用」的语义
  处理，会与外层的元素替换重复释放同一份引用。现新增 `dial_array_set_borrowed`
  （不消耗被借的引用，改为交出「一份新拥有」的引用），并在 codegen 中按
  「消费者释放、变量槽转移」的规则结算。
- **`dial_rc_clone` 恢复纯粹语义**：唯一持有者时原样返回、**不再 +1**；
  多持有者时深拷贝。过去那个 `+1` 让 `clone()` 只能靠「别名 + 多一个计数」实现，
  是元素引用计数泄漏与 COW 守卫误判的共同源头。
  配套地，`dial_string_clone` / `dial_bytes_clone` 改为强制深拷贝产出独立副本
  （否则纯语义下 `b = s.clone()` 会让 `b` 悬垂）。

- **误拒合法程序 + 三处堆悬垂 + `bytes[]` 被拒 + 两处安全缺陷**（本轮一并修复）：
  - **① `select` 分支里 `move_to` 外层变量被误报「已转移」**：`check_moved_stmt` 的
    `Stmt::Select` 分支既不做「只查 `arm.chan`」的纪律，反而又对 `arm.body` /
    `default_body` 递归了一遍 `check_moved_stmt`，与 `check_stmt` 的递归重复——
    同一条 `move_to` 被检查两次，第二次必然报「变量已转移」。现删掉分支内的递归，
    只保留对 `arm.chan` 的 `check_moved_expr`（分支体仍由 `check_stmt` 递归各自检查，
    恰好一次）。
  - **③ 结构体字面量 / 字段读取 / 字段赋值的堆字段未 `retain`**：`Expr::StructLit`、
    字段读取（`t = p.name`）、字段赋值（`p.name = s`）在写堆类型字段时只 `store` 不
    `retain`，字段于是成了来源的**别名**；来源出作用域被 `release` 后字段即悬垂
    （use-after-free，读到的长度字段可能是垃圾，打印出成片乱码）。现统一引入
    `retain_if_borrowed`：借来的值（变量 / 字段 / 数组元素 / `get`·`at`）`retain` 一份，
    自带所有权的值（字面量 / `clone` 等）直接接管。
  - **③ 的同族泄漏**：`a[i] = <字面量>`、`a.push(<字面量>)`、`a.set(i, <字面量>)`、
    数组字面量元素在循环里逐次泄漏一份引用（容器 `retain` 了元素，字面量自带的
    那一份所有权却没移交）。现引入 `settle_fresh_value`：自带所有权的值在容器接管后
    归还其临时所有权，借来的值则不释放——嵌套下标赋值 `m[i][j] = v` 也按同一规则结算。
  - **④ `bytes[]` 被 `is_array_elem_type` 拒绝**：语义层漏收 `Type::Bytes`，但 codegen
    的 `array_elem_kind` 早已把 `Bytes` 归为 kind=1（RC），两层不一致。现补上
    `Type::Bytes`（`dial_bytes_retain` / `release` 内部即 `dial_rc_*`，完全兼容）。
  - **⑤ IR 验证失败不中止编译**：`module.verify()` 失败只 `eprintln` 后继续
    `emit_object`，可能产出无效 IR 或带未定义行为的机器码。现改为直接以易读文字
    `return Err(...)` 中止。
  - **⑥ `curl --proto '=https,http'` 与「只允许 https」声明矛盾**：https URL 可被
    302 重定向到 http，中间人攻击窗口重新打开。现 `--proto` 仅放行 https（本机地址
    经 `http_allowed` 判定才加 http），并新增 `--proto-redir` 限制重定向协议。

### Added（新增）

- **测试组 N：内存上界 + 所有权语义回归**（`tests/mem/`）。
  - `array_cow_bench.dial`：100 万次下标赋值的基准，在 **128 MB 虚拟内存上限**下运行——
    一旦 COW 误判复发（需 TB 级内存）立刻被上限打爆。
  - `array_cow_semantics.dial`：逐条钉死 `clone` 独立 / COW 隔离 / 嵌套赋值 /
    句柄元素数组四条所有权语义。
- **测试组 O：所有权 / 语义回归**（`tests/sem/`）：钉死「编译器应接受并正确运行」的合法
  程序——`select` 里 `move_to` 外层变量（`select_move_to`）、`StructLit` / 字段读取 /
  字段赋值的堆字段 retain（`structlit_heap_field` / `field_and_assignfield`）、
  `bytes[]` 不被拒（`bytes_array`）、借来值写入容器不重复释放（`borrowed_assign`）。
- **一键安装脚本彻底重写**（`tools/install-dial.sh` / `install-dial.ps1`）：
  - 版本**不再硬编码**：优先从已装好的 `dialc` 读出真实版本号，bootstrap 才回退到内置
    默认；配套件（标准库 / 插件）一律按该版本推导——**消灭版本不同步**。
  - VS Code 插件按 **Release 资产清单**解析真实文件名，不再依赖插件自身 semver 与
    编译器版本号对齐（旧版写死 `dial-0.98.1.vsix`，每次都 404）。
  - 下载失败**可见真实原因**（旧版 `2>/dev/null` 把 curl 报错吞了）。
  - `install-dial.sh` 支持 bash / zsh / fish / profile 四种 shell 的 PATH 持久化，并提示
    当前会话用法；新增 `--help` / `--version` / `--uninstall`，解压前校验压缩包完整性，
    重跑幂等。

### Changed（变更）

- README §11.2 / §11.3：补齐「独占路径必须原样移交所有权、一个引用计数都不能动」
  这条不变量（含做错的后果对照表），并把 `clone()` 的描述从「COW 感知克隆」
  更正为「独立副本（深拷贝）」。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.99.1.md](rationale/v0.99.1.md)。

---

## [v0.99] — 2026-11-03

### Added（新增）
- **多维数组 `int[][]`**：内层 `int[]` 作为 8 字节句柄直接存储，支持嵌套读 / 写 / 遍历 / `push`。
- **句柄元素数组** `string[]` / `map[]` / `error[]` / `channel[]` / 不透明指针 `[]`：
  数组头部新增 `kind` 字段（`INT` / `RC` / `ATOMIC`），按元素 ARC 复用
  `dial_rc_*` 与 `dial_channel_*`，正确 `retain` / `release` 元素所有权。
- **嵌套下标赋值 `a[i][j] = v`**：编译器新增递归左值解析 `gen_assign_lvalue`，
  逐层 `gen_expr` 取出内层数组句柄 → `dial_array_set` 写入 → 写回可能 COW 更新的外层句柄，
  直到最外层变量槽。单层 `a[i] = v` 路径不受影响。
- 新增示例 `examples/multi_dim_demo.dial`、`examples/string_array_demo.dial`、
  `examples/channel_array.dial` 及对应黄金文件（纳入 A 组回归）。
- 新增负向用例 `tests/bad/array_bool_elem.dial` / `array_byte_elem.dial` / `array_char_elem.dial`，
  锁定 `float[]` / `bool[]` / `byte[]` / `char[]` 等标量非 int 元素仍被编译期拒绝。

### Changed（变更）
- README §5.1/§5.2/§11/§11.6/§17.2 与官网 `spec.html` 同步：数组元素类型扩展到
  `int` 与 8 字节句柄，多维数组与嵌套下标赋值正式从「未实现」移除。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.99.md](rationale/v0.99.md)。

---

## [v0.98.1] — 2026-11-02

### Fixed（修复）
- **裸块作用域 `{ ... }` 从「语法能过、语义/索引崩溃」修到完整可用**：
  `Stmt::Block` 的 AST 变体在定义与模式匹配两侧不一致（元组 vs 具名结构体），
  导致**整个编译器无法编译**。现已统一为具名变体 `{ body, line, col }`，
  词法/语法/语义/索引/代码生成五处同步打通，裸块可正常引入词法作用域。
- **README §17.2 与 §28 对 LSP 能力的结论自相矛盾**：§17.2 仍写「重命名 / 引用查找 / 格式化
  **未实现**」，而 §28 记载这三项已在 v0.98 轮 2 / 轮 3 落地。现按实现统一口径，
  §17.2 改为「已实现」并指向 §28 对应小节。
- **README §3.2 的交叉引用指错节**：注释里的「见 §3.1.2 / §3.1.3」实际指向
  Windows x64 与 Termux aarch64，而 Linux 的 LLVM 依赖在 **§3.1.1**、
  动态库要求（`LD_LIBRARY_PATH`）在 **§3.1.5**。现改为正确节号。
- **README §2 与官网 spec.html §2 的目录结构里 `install-dial.ps1` 重复两行**
  （第二行还少了「支持 irm | iex 一键命令」的说明）。现删去重复行。
- **README §28 标题停在 v0.95**：正文已含 v0.98 的四轮工业化与三编辑器接入，标题与内容脱节。
  现改为覆盖到 v0.98。
- **spec.html §28.2 与 §28.7.1 对 `textDocumentSync` 的说法直接冲突**：
  一处写「声明 `textDocumentSync = 1`（全量同步）」，另一处写「必须用选项形式
  `{openClose, change: 1, save}`，不能用纯数字 1」。后者才是对的
  （纯数字形式没有 `save` 字段，客户端不会发 `didSave`，诊断刷新会静默失效）。
  现按后者统一。（v1.10 起 `change` 升为 `2` = Incremental，选项形式不变，见 §28.7.2）
- **CHANGELOG 缺 v0.98.1 条目**：v0.98.1 已实际发布（tag `v0.98.1`），
  但本文件顶部仍是 `[v0.98]`。现补上本条目。
- **README §23 的版本号规则与 v0.98.1 冲突**：原规则写死「最多两位小数、
  不存在三位小数的版本」，而 v0.98.1 是既成事实。现补充第 6 条，
  明确补丁位**仅**用于修复发布（只修缺陷、不加新特性），并规定其边界条件。
- **`cve.html` 的 CVE 编号年份穿越**：项目日期是 2026，编号却写成 `CVE-2024-DIAL-001`。
  现统一为 2026 年，并说明这是**项目自建的跟踪编号**（非 MITRE 分配的正式 CVE 编号）。
- **README §18 的 CI 关卡表缺 `cve_check` 一关**，而 `cve.html` 声称
  「已在 CI 工作流中接入，作为发布前的强制门禁」。现已核对 CI 工作流确实包含该步骤，
  §18 的表格补上这一行——文档与实际流水线对齐。

### Added（新增）
- **`print_raw(s)`**：原样输出字符串、**不追加换行**。此前 `print(s)` 永远在末尾补 `\n`，
  导致**任何需要连续输出的程序都写不出来**（拼行表格、进度条、逐字动画、协议帧）。
- **`write_char(c)`**：输出单个字符、**不追加换行**。与 `print(char)` 的唯一差别就是这个换行符。
- **`str(char)`**：`char` 纳入 `str()` 的支持类型，得到 1 字符的 UTF-8 字符串，
  可直接参与拼接（`str(c) + "!"`）。
- **`dialc version` / `dialc --version` / `dialc -V`**：显示版本号。
  版本取自 `Cargo.toml`（编译期注入），与全站唯一真相一致。

### Changed（变更）
- **`print(char)` 的换行语义被写进文档**：明确 `print(char)` **输出字符本身并追加一个 `\n`**
  （行式输出），而 `write_char(char)` 不追加。此前二者的差别只存在于实现里、没写进文档，
  读者无法确定该用哪个。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.98.1.md](rationale/v0.98.1.md)。

---

## [v0.98] — 2026-11-02

### Changed（变更）
- **`--fast`（ReleaseFast）不再是「只关检查」**：在关闭运行期检查之外，现在开启三层编译器终极优化，
  让 `--fast` 真正名副其实（此前它只关掉 Fail-Fast 检查，机器码仍是普通 O2）。
  - **① 自动向量化 (SIMD) + 循环展开**：LLVM 切到 `OptimizationLevel::Aggressive`（O3），
    并按宿主实测指令集挑对应的 x86-64 微架构级别（`x86-64-v2/v3/v4` → SSE4.2 / AVX2 / AVX-512，
    ARM64 显式 `+neon`），一次直接吃 4~8 个 int64；循环展开器同时压低分支预测开销。
  - **② LTO 链接时优化**：`--fast` 现在把 inkwell 产出的 LLVM bitcode 经 `clang -flto -O3` 编成
    含 LTO bitcode 的「胖对象」，C 运行时也逐文件以 `-flto` 编译，链接时由 clang 整体重排——
    `dial_array_get`/`push` 等热函数被直接内联进主程序，热循环零 `call` 开销（反汇编可证）。
  - **③ PGO 剖析引导优化（新增 `--pgo`）**：先插桩跑一遍训练样本、合并 profile，再据热点重排指令
    布局（热路径顶到 I-Cache 最前）。`--pgo` 必须配合 `--fast`（需本机 `clang` 支持）。
  - 三者都需要本机 `clang`；若 `DIAL_C_COMPILER` 指向非 clang，则优雅降级为「仅 O3 + SIMD」，
    保留 SIMD 收益、放弃跨模块 LTO/PGO。生成的二进制按本机 CPU 调优，仅供本机运行。
- 新增性能基准示例 [`examples/bench_array.dial`](examples/bench_array.dial)（1000 万次 push + 遍历求和），
  作为上述三层优化的「标准考题」。

### Added（新增）
- **编辑器插件安装包（三主流）**：编译型语言的编辑器支持是必做的苦活，本版落地三套。
  - `editors/vscode/`：**手写 LSP 客户端**（无 npm 依赖），`build.sh` 打包成 `dial-*.vsix`
    （`code --install-extension` 侧载；CI 在 `v*` 发版时现构建并附到 Release）。
  - `editors/neovim/`：零依赖，只用 Neovim 内置 LSP 客户端（`ftdetect/` + `plugin/`，拷进 `runtimepath`）。
  - `editors/emacs/`：**不开发插件**——用 Emacs 内置 `eglot` 原生连 LSP，仅提供 `init.el` 片段。
  - 其余编辑器（JetBrains / Zed / Sublime 等）只要能启动 `dialc lsp` 即可接入，不再单独维护插件。
  - `editors/README.md`：三编辑器总览与能力矩阵。

### Changed（变更）
- **LSP 能力协商**：`initialize` 读取客户端 `capabilities`，按声明动态广播能力（老客户端不报能力也不报错）；
  并新增 `window/logMessage` 自诊断通道（不污染协议流、不进 stdout）。
- **一键安装脚本自动补齐配套件**：除编译器、`stdlib/`+`runtime/`、`.dialpkg` 外，
  现在还会**自动下载并安装 VS Code 插件**（从 Release 取 `dial-<版本>.vsix`，
  再用 `code --install-extension` 装上），不必再自己去 Release 页翻资产、手动侧载。
  - 版本只从 `DIAL_VERSION` 一处推导（Release 标签 `v0.98.1` 与资产名 `dial-0.98.1.vsix`
    都由它算出），不会出现三处各说各话、下次发版又忘了改其中一个。
  - 没装 VS Code / 离线时**只告警、不阻断**安装（编译器与包安装不受影响）。
  - **不加 `--force`**：用户若已装了更新版插件，不该在「一键安装」里被悄悄降级。
  - 下载 URL 可用 `DIAL_VSIX_URL` 覆盖（自建镜像 / 网络受限时用）。
  - Neovim / Emacs 侧无需安装动作（零依赖配置），故不动。
- **一键安装脚本不再「一键卡住」**：下载标准库与插件的 curl 补上
  `--connect-timeout 15 --max-time`。此前没有超时——GitHub 不可达时 curl 会一直挂着，
  用户看到的是脚本**不动**，而不是一句清晰的失败提示。一键安装卡死比装不上更让人手足无措。
- **VS Code 插件接通轮 2~4 的全部能力**：文档大纲、引用查找、重命名（含 prepareRename）、签名帮助、
  保守格式化、参数 inlay 提示、代码动作、折叠范围、选择范围、文档链接——插件不再只转发三个 provider。
- **Neovim 插件补上新能力的键位**：`<leader>f` 格式化、`<leader>a` 代码动作、`<leader>ds` 文档大纲，
  并启用 Neovim 0.10+ 的 `foldingRange` 客户端（0.9 用 `pcall` 兜住）。
- **补全增强**：补全项带 `textEdit` 精确替换范围（修正旧实现「名字重复拼在后面」的 bug）+ `sortText`
  排序（函数/外部函数最前、结构体/不透明类型次之、变量/形参最后）+ `.` 触发字符。
- **VS Code 插件接通 hover / 跳转定义 / 补全**：原扩展仅接了诊断与语义高亮，现补齐三个 provider 转发到
  `dialc lsp`；`initialize` 发送真实 `capabilities`；`package.json` 升 `0.98.0`。
- 文档同步：`README §28.7`（编辑器插件安装包与 LSP 能力清单）、`ARCHITECTURE`、`CHANGELOG` 更新；
  各编辑器 README 与 `docx/` 重新生成。

### Fixed（修复）
- **能力协商把「没声明」误判成「不支持」**（轮 1 引入，本版才被自家测试抓到）：
  `negotiate_capabilities` 原按「客户端有没有**逐项点名**」判断能力，于是发 `capabilities: {}` 的
  客户端（不少最小客户端都这么发）会**一个能力都拿不到**——仓库自带的 `tests/lsp_features.py`（K 组）当场失败。
  现改为按「客户端有没有**提到这一段**」判断：压根没提 `textDocument` / `workspace` 段 = 没表达偏好 → 全开；
  提到了该段却漏掉某项 = 明确不支持 → 不广播。
  （多广播一个用不上的能力零代价；少广播则让功能**静悄悄消失**，用户查不出原因。）
- **selectionRange 不认规范形状**：LSP 规定 `textDocument/selectionRange` 用 `positions`（**数组**，可多选），
  真实客户端（VS Code）发的就是这个形状；旧实现只认单数 `position`，于是真实客户端**根本拿不到结果**。
  现按规范收数组、返数组，同时保留单数形式（老客户端不至于拿到数组而崩）。
- **LSP 协议流被裸文本污染 → 编辑器卡死**（本版最严重的一个）：
  `resolve_imports()` 是 CLI 构建与 LSP 诊断**共用**的函数，其中两处 `println!`
  （「包 'x' v…」「链接 x -> …」）会把纯文本写进 **stdout**——而 stdout 跑的正是 LSP 的
  `Content-Length` 分帧协议。客户端一旦读到裸字节，分帧立刻错位：症状**不是报错，而是卡死**
  （编辑器在等一条永远对不上的消息）。触发条件却极常见：打开一个 `import` 了**已安装**包的文档。
  - **修复**：`resolve_imports()` 改为**日志出口由调用方注入**（`log: &mut dyn FnMut(&str)`），
    函数本身不再有资格写 stdout；CLI 构建传 stdout，LSP 传 `window/logMessage` 通知（编辑器可见、协议合法）。
  - **加固一（动态）**：冒烟测试新增**协议流纯净性**断言——stdout 上出现任何非协议字节即判失败；
    并给每次请求加**超时看门狗**，服务器卡死会**快速失败并指名道姓**，而不是把 CI 挂死。
  - **加固二（静态）**：新增 `tools/check_lsp_stdout_purity.py`——LSP 用到的库模块与被 LSP 调用的函数里
    出现 `println!` 即失败（CI 独立步骤 + `tests/run_tests.sh` M 组双重把关）。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.98.md](rationale/v0.98.md)。

---

## [v0.97] — 2026-10-31

### Added（新增）
- **三平台 CI 构建与发布流水线**（`ci.yml`）：`build-linux` / `build-windows` / `build-termux`
  三个作业各自原生构建，打 `v*` 标签时由 `release` 作业把三个平台的 `dialc-v<版本>-<slug>.zip`
  一并上传到 GitHub Release。
- **Windows（MinGW / `x86_64-pc-windows-gnu`）**：使用官方 MSVC 构建的 LLVM 18 全量包，
  但只取 **C 接口**（`LLVM-C.lib` 导入库 + `LLVM-C.dll` 运行期）。纯 C ABI 跨 MSVC / MinGW
  无名字改写冲突，因此 `dialc.exe` 直接用 MinGW 目标链接它，无需自编译 LLVM。
  - 配套 `tools/llvm-config-win-shim.c`：由 MinGW `gcc` 编译成**真正的 `llvm-config.exe`**，
    把 LLVM 当 C 接口暴露给链接器（`--libnames` 只返回 `LLVM-C.lib`），屏蔽官方 MSVC
    `llvm-config` 的怪癖（它运行期依赖包内并未提供的 `LLVM-18.dll`）。
    **必须是 `.exe` 而不是 `.cmd`/`.bat`**：llvm-sys 先尝试 `llvm-config.exe`、再尝试裸名，
    而 Windows `CreateProcess` 对不带扩展名的程序名只补 `.exe`，批处理垫片永远不会被找到。
  - 用户程序由 **MinGW `gcc` + winpthreads** 编译，stdlib 的 `pthread` 代码**零改动**即用
    （winpthreads 提供兼容层），产物 `-Wl,-Bstatic -lpthread` 静态链入 winpthreads → 单文件 `.exe`。
- **Termux aarch64（musl 全静态）**：在 GitHub **原生 ARM64** runner（`ubuntu-24.04-arm`）上的
  Alpine aarch64 容器内构建（**不用 QEMU**——QEMU 用户态模拟在新 runner 上会报
  `Could not open '/lib/ld-linux-aarch64.so.1'`）。`rustup` 装 `aarch64-unknown-linux-musl`
  宿主工具链，`llvm18-static` 提供静态 LLVM，配合
  `RUSTFLAGS="-C target-feature=+crt-static -C linker=gcc -C link-arg=-static -C link-arg=-lgcc -C link-arg=/usr/lib/libffi.a"`
  → **完全静态**、零外部依赖的 `dialc`（实测：无 `INTERP` 段、无 `NEEDED`，约 118 MB）。
  - 配套 `tools/llvm-config-static-linux.sh`（**只静态**垫片）：inkwell 启用的是
    `llvm18-1-prefer-dynamic`，llvm-sys 会**主动优先** `--libnames --link-shared`、把 LLVM
    动态链成 `libLLVM.so`；垫片对 `--libnames --link-shared` 返回非零码，迫使 llvm-sys 回退到
    `--link-static`。（llvm-sys 并没有 `LLVM_SYS_<ver>_STATIC` 之类的环境变量可覆盖该偏好。）
  - `-lgcc` 与 `libffi.a` 都是补链接缺口：静态 LLVM 组件引用的 `__clear_cache` 在 `libgcc.a`；
    系统库种类取自 build script **自身**的 `cfg!(target_feature="crt-static")`（宿主而非目标），
    若不处理会残留 `NEEDED libffi.so.8`，而 Termux 上没有它 → 执行即 `not found`。

### Changed（变更）
- `src/codegen.rs`：`emit_object` 的 CPU 不再写死 `"x86-64"`，改为按宿主架构推导
  （x86_64→`x86-64`、aarch64→`generic`），aarch64 也能出正确机器码。
- `src/package.rs`：`host_triple()` 改用编译期 `cfg!` 宏推导（Windows→`x86_64-pc-windows-gnu`、
  aarch64+musl→`aarch64-unknown-linux-musl`、aarch64→`aarch64-linux-gnu`、x86_64-linux→`x86_64-linux-gnu`），
  保持与既有测试 / 包清单键兼容。
- `src/main.rs`：`link()` / `output_paths()` / `cc_command()` 平台化——Windows 产物补 `.exe`、
  C 编译器候选按平台排序、链接时静态链入 winpthreads；其余平台行为不变。
- `tools/package_release.sh`：新增精简的**单平台**发布打包（替代旧的全量源码包），产出
  `dialc-v<版本>-<slug>.zip`（含 `dialc[.exe]` + `stdlib/` + `runtime/` + 许可 + 安装脚本；
  Windows 包额外附带 `LLVM-C.dll`）。
- `tools/install-dial.sh` / `install-dial.ps1`：支持 `dialc.exe`、Windows 下随附复制 `LLVM-C.dll`；
  `install-dial.ps1` 可从 GitHub 最新 Release 拉取本平台资产一键安装。
- ARCHITECTURE.md §0.1 / §0.2 / §8、README §3.1 / §3.2.1 / §3.4：同步三平台支持说明。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.97.md](rationale/v0.97.md)。

---

## [stdlib v1.0.0] — 2026-10-31（独立 Release，tag `stdlib-v1.0.0`）

### Added（新增）
- **`stdlib.dialpkg` 包**：单包含三平台预编译静态库
  （`lib/x86_64-linux-gnu/libdial_std.a` · `lib/x86_64-pc-windows-gnu/libdial_std.a` ·
  `lib/aarch64-unknown-linux-musl/libdial_std.a`），`manifest.txt` 记录
  `name=stdlib` / `version=1.0.0` / `abi=1` / `license=MIT OR Apache-2.0` 与各平台机器码 SHA256。
- **`stdlib/stdlib.diali`**：与 `declare_stdlib()` 逐条对齐的接口签名文件，供 `.dialpkg` 自描述与校验。
- **`tools/build_stdlib.sh`**：统一选 `zig cc`/`clang`/`gcc` + `ar`，把 `runtime/*.c` + `stdlib/*.c`
  编译归档为单份 `libdial_std.a`（默认 `-DDIAL_SAFETY=1` ReleaseSafe 预编译）。
- **`tools/make_stdlib_pkg.py`**：收三平台 `.a` 与 `.diali`，算 SHA256、生成 `manifest.txt` 并打包 `stdlib.dialpkg`。
- **`dialc` 链接期自动链入**：`src/main.rs::link()` 优先 `package::find_installed("stdlib")` 取本平台 `.a`
  链入，日志打印「使用已安装的标准库包: stdlib v1.0.0（ABI 1，目标 …）」；
  未安装则回退实时编译 `stdlib/` + `runtime/` 源码（默认 Debug），**任何环境都能编出程序**。
- **ABI 兜底**：已装包 `manifest.abi ≠ package::STDLIB_ABI`（新增常量，锁定为 `1`）即**当场报错并给出重装方案**
  （`rm -rf ~/.dial/packages/stdlib` 后重装），绝不静默崩溃。
- **一键脚本自动安装**：`tools/install-dial.sh` / `install-dial.ps1` 装好 `dialc` 后静默从 Release
  `stdlib-v1.0.0` 拉取 `stdlib.dialpkg` 并执行 `dialc install`（`STDLIB_RELEASE` 可钉版本，离线/失败仅告警）。
- **CI 自动发布**：`.github/workflows/ci.yml` 在 `stdlib-v*` 标签触发 `publish-stdlib` 作业，
  由 `build-linux` / `build-windows` / `build-termux` 各自产出本平台 `.a` 后打包发布。

### Changed（变更）
- 预编译安全等级统一为 **ReleaseSafe（`-DDIAL_SAFETY=1`）**：C Runtime 层越界/空指针等 UB 由 `ud2`
  Fail-Fast 拦下，兼顾性能与安全；与用户程序自身 `--release`/`--fast` 等级是两回事（标准库等级发布时已钉死）。

> 📄 本版的理由、实测数据与边界见 [rationale/stdlib-v1.0.0.md](rationale/stdlib-v1.0.0.md)。

---

## [v0.96] — 2026-10-31

### Added（新增）
- **`dialc install <URL>`**（阶段 2）：包可以直接从 URL 装，**不用先手动下载**。
  - **只允许 `https://`**（本机 `localhost` / `127.0.0.1` / `::1` 例外，供本地镜像与自测）。
    明文 http 能被中间人改写，而 SHA256 挡不住「换了内容还带着正确哈希」的伪造，
    因此非本机地址一律拒绝——**宁可装不上，也不降级到明文**。
  - **`--sha256 <哈希>`**：钉死**整个下载物**的哈希，用来证明「这就是我要的那一份包」。
    比对发生在**解压之前**，不符即失败；下载物随即删除。
  - **没给 `--sha256` 也能装**，但会明确提示这次只验了「包内文件与 manifest 是否一致」
    （能发现残缺与篡改），**没验整包来源**——不静默降级。
  - **下载用外部 `curl` 而非引入 Rust 的 HTTP 库**：编译器**本来就依赖外部工具链**
    （`zig cc` / `clang` / `ar`），下载器属同一类依赖，不新增「运行期还要装个东西」的负担；
    引入 TLS 栈（rustls 等）会显著拉长编译时间与依赖树，与 §21「不引入无用依赖」相悖。
    **TLS 绝不手写**——证书校验、主机名校验、重定向全部交给 curl 背后的成熟实现。
  - 临时文件**成功失败都删**，`~/.dial/packages/` 里不留残骸。
- **解压前整包校验**（阶段 2 的安全底座，`src/package.rs::verify_archive`）：
  按包内 `manifest.txt` 登记的 SHA256，把包里**每个文件**验一遍，
  **一个字节都还没落到磁盘**——这是 ARCHITECTURE.md §5.1「校验先于一切 / 每个文件都要校验」
  的字面实现。此前 `dialc install`（本地文件）**完全不做校验**，
  哈希只在**编译时**对用到的那两个文件验；现在安装这一步就拦下了。
  **未在 `manifest.txt` 登记哈希的文件一律拒绝**——无法校验的文件不允许进入缓存。
- **一键安装脚本**（阶段 5）：`tools/install-dial.sh`（Linux / Termux）与
  `tools/install-dial.ps1`（Windows / PowerShell）。按 §3.2 承担两件事（顺序不能反）：
  1. **初始化 Dial 环境**：建 `~/.dial/` 目录树（`bin` / `packages` / `cache/downloads`）、
     把 `dialc` 装进 `~/.dial/bin/` 并挂进 `PATH`、**把 `stdlib/` 与 `runtime/` 一并装到
     `~/.dial/`**、写一份 `~/.dial/config.txt` 工具链配置；
  2. **自动安装已下载的包**：初始化完成后**重新扫描**找到的 `.dialpkg`
     （当前目录及其子目录，或参数指定的目录），逐个执行 `dialc install`。
  > **为什么脚本要「自动重新扫描」而不是让用户逐个点名**：一键安装的语义是
  > 「我把这个目录交给你，你把该装的都装上」。用户不该需要记住包名、也不该需要
  > 为每个包敲一条命令——包就在那里，脚本看得见。

### Fixed（修复）
- **一键安装后编译器找不到 `stdlib/` 与 `runtime/`**：编译器按**可执行文件所在位置**
  向上找这两个目录（`target/release/dialc` → 工程根）。装到 `~/.dial/bin/dialc` 后，
  那个位置正好是 `~/.dial/`——所以它们必须**跟着一起装**。
  少了这一步，用户在自己项目里编译会报「找不到 stdlib/ 与 runtime/ 目录」，
  一键安装等于没装。脚本现已一并安装这两个目录。
- **`install` 时的 `manifest.txt` 校验时机**：此前 `install()` 先解压再校验（且当时压根没校验），
  现在**先校验后解压**。

### Added（测试）
- 测试组 **L**（`tests/pkg_install_url.py`，26 项断言）：真起一个**本机 HTTP 服务器**
  真实提供 `.dialpkg`，用真正的 `dialc install <URL>` 去装，验证：
  - 正常安装成功；`--sha256` 与实际一致时也成功；未钉死哈希时给出明确提示；
  - **坏包一律装不进去**：内容被篡改（改 `.diali` 但保留原哈希）、
    `--sha256` 不符、404、非本机的明文 http——四种情形全部失败；
  - **失败后 `~/.dial/packages/` 无残骸**（半截的包绝不能留在缓存里）；
  - 装完能 `import` 并编译运行，输出正确；
  - 一键脚本：初始化出编译器 + `stdlib/` + `runtime/` + 配置，
    自动扫描并装上 `.dialpkg`，最后在**与 Dial 源码树无关的独立项目目录**里
    用装好的编译器编译 import 程序并运行——这是「一键安装」的意义所在。
  全部用例在**隔离的 HOME** 下运行，不碰用户真实缓存。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.96.md](rationale/v0.96.md)。

---

## [v0.95] — 2026-10-31

### Added（新增）
- **GitHub Actions 流水线** `.github/workflows/ci.yml`（**编译 + 发布流程**）：
  - `build` 作业（每次 push / PR）：装 LLVM 18 + Rust → `cargo build --release --locked`
    → 跑**全部十一组**端到端测试（A～K）→ 校验内部锚点 → 校验 `docx/` 与文档同步；
    四道关卡任何一道不过，PR 即不可合并。
  - `release` 作业（打 `v*` 标签时）：复用 build 的二进制，
    打成 `dialc-v<版本>.zip`（源码 + `.git` + 四份许可文件 + `docx/` + VS Code 扩展 + `bin/dialc`），
    上传为 **GitHub Release** 产物——即官网首页的下载链接。
  - 环境与 README §3.1 一致：Ubuntu 24.04 LTS（近两年内发布的发行版）、Linux x86_64 · 动态 glibc。
- **新增模块 `src/index.rs`**：作用域树（Scope，parent 链）+ 符号表（Symbol），独立于类型检查。
  - **作用域链「由内向外」查找**，与 `semantic::lookup_type` 语义一致，因此 `if` 块里 `let x: int` 遮蔽外层
    `let x: string` 在 Hover / 定义 / 补全三处都查的是**内层那个 `x`**。
  - **跨文件符号**：通过 `import` 自动扫描 `~/.dial/packages/<name>/`，复用现有
    `find_installed` / `verify_iface` / `parse_diali` 拿到包的导出（不透明类型 + `extern fn`），
    并记下来自 `.diali` 的**声明行号**与 `///` 文档——这两样是「跳进 `.diali`」与「Hover 显示包文档」的素材。
- **Hover**（`textDocument/hover`）：
  - 文件内符号（函数 / 结构体 / 局部变量 / 形参）显示名称、**推断类型**与声明处片段；
  - 包符号显示函数签名 + `.diali` 里的 `///` 文档，并标注 **「外部函数」**。
- **跳转定义**（`textDocument/definition`）：
  - 文件内符号 → 返回**本文件**的精确 `Location`（UTF-16 列换算，精确到标识符起止）；
  - 包符号 → 返回 `.diali` 文件的 `Location`，落点正是该 `extern fn` 的**声明行**。
- **基础补全**（`textDocument/completion`，触发字符 `.`）：
  - 收集光标**可见作用域**内的全部符号（本文件局部 + 全局函数/结构体 + 已 import 的包符号）；
  - 按光标处**已输入前缀**过滤（`demo_` 前缀只留 `demo_add`，不会把 `total` / `DemoHandle` 混进来）；
  - 即使光标落在空白（前缀为空）也返回当前可见的全部候选。
- **AST / 解析器 / 包接口补上位置字段**（稀疏、小改）：`VarDecl` / `For` / `Catch` / `SelectArm` 的变量名位置，
  `Func` 的名字与每个形参位置，`StructDef` 的名字位置；`.diali` 的 `type X;` 与 `extern fn f` 也记下声明行号。

### Changed（变更）
- `initialize` 能力协商新增 `hoverProvider: true` / `definitionProvider: true` /
  `completionProvider: { triggerCharacters: ["."] }`（v0.94 仅有语义高亮与诊断）。
- `didOpen` / `didChange` / `didSave` 在发诊断之外，额外把文件喂给符号表索引；
  `didClose` 时从工作区索引里移除该文件，避免残留。

### Added（测试）
- 测试组 **K**（`tests/lsp_features.py`）：真起 `dialc lsp` 子进程按 LSP 协议往返，覆盖：
  1. Hover 区分同名变量遮蔽（内层 `x: int` ≠ 外层 `x: string`）；
  2. Hover 包函数的 `.diali` `///` 文档与「外部函数」标注（跨文件符号）；
  3. 跳转定义对文件内符号回本文件、对包符号跳进 `.diali`；
  4. 补全收集可见符号 + 前缀过滤（含空白光标全量）。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.95.md](rationale/v0.95.md)。

---

## [v0.94] — 2026-10-31

### Added（新增）
- **`dialc lsp` 子命令**：stdio 上的 JSON-RPC 服务（`Content-Length` 分帧）。
  不引入 LSP 框架——骨架只需要「读一条 JSON、写一条 JSON」，
  异步运行时与依赖树的代价远超收益。
- **诊断**：`didOpen` / `didSave` 时发布；词法 / 语法 / 语义三阶段任一失败即停，
  只报第一个错误（与 `dialc build` 行为一致）。改正保存后自动清空，关文件后不留残留红线。
- **语义高亮**：`textDocument/semanticTokens/full`，token 由**编译器自己的词法器**产出
  （不另写一套高亮扫描器——一旦有两份词法，高亮与编译必然慢慢分道扬镳）。
- **VS Code 扩展** `editors/vscode/`：**不需要 npm install**（LSP 客户端是手写的 200 行），
  拷进扩展目录即可；另附 Neovim 原生 LSP 的三行配置。

### Changed（变更）
- **`If` / `While` / `For` / 表达式语句补上位置**（此前只有 `var` / `return` 这类带）：
  所有语句现在都带 `line` / `col`，语义诊断因此能落回源码。
- **语义错误的语句级位置兜底**：`check_stmt_at` 把内部产生的「无位置」错误
  盖到出错那一条语句上（只补缺失的，已精确定位的原样放行）。
  修掉的是一个真实缺陷：表达式错误原先一律报「第 0 行第 0 列」，
  在编辑器里就是**第一行一个红点**，等于没定位。
  B 组负向用例的输出也因此全部带上了「第 N 行第 M 列」。
- **`char` 参与算术的专门诊断**：此前落到通用的「要求两个操作数类型完全相同」那条，
  会让人以为补个转换就能做算术。现在单独说明：
  `'a' + 1` 的意图无法判定，请先 `char_to_int` 再 `int_to_char`。
- **`dialc install` 回显包导出函数**：装完直接列出 `extern fn` 签名与其 `///` 文档，
  不必再去翻 `.diali` 文件。

### Added（测试）
- 测试组 **I**（`tests/lsp_smoke.py`）：真起一个 `dialc lsp` 子进程按协议往返，
  覆盖初始化能力协商、诊断定位到出错那一行、语义 token 的**差分解码 + 源码子串反查**、
  `shutdown` 后干净退出、以及 stderr 不污染协议流。
- 其中 UTF-16 换算一条**特意用 emoji 而非中文**做用例：
  中文在 BMP 内（1 char = 1 码元）**测不出**换算错误，只有 emoji（1 char = 2 码元）能。
  已反向验证过：删掉换算代码重跑，确实只有 emoji 那条变红。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.94.md](rationale/v0.94.md)。

---

## [v0.93] — 2026-10-31

### Added（新增）
- **`import "包名";` 语句**（词法 / 语法 / 两阶段解析）。包名是字符串字面量而非标识符——
  刻意避开为命名空间再引入一套 `.` / `::` 解析规则。
- **新增模块 `src/package.rs`**：`.dialpkg` 解包、缓存管理、manifest 解析、`.diali` 解析。
- **`dialc install <包.dialpkg>`**：解压到 `~/.dial/packages/<name>/`，**项目目录零侵入**；
  缓存是可重建的派生数据。
- **`.dialpkg` 解包**：zip 容器 + **路径穿越防护**（拒绝绝对路径、`..` 逃逸、Windows 盘符）。
- **`manifest.txt` 解析**（JSON）：`name` / `version` / `abi` / `iface` / `license` /
  `targets`（Triple 映射字典）/ `sha256`（逐文件哈希）。字段缺失一律报错，绝不静默补默认值。
- **按 Target Triple 选库**：编译器按自己的 triple 索引 `targets`，
  查不到就报错并**列出本包支持哪些 triple**；交叉编译到 Windows 时自动链 `.lib`。
- **SHA256 强制校验**：`.diali` 与**本次实际要用的那份**机器码各自校验（§5.1）。
  残缺或被篡改的二进制**绝不进入链接器**。
- **`.diali` 解析**：`type X;`（不透明句柄）与 `extern fn f(a: T): R;`，
  签名接入 Sema 做类型检查；未声明又非内建的类型名一律报错。
- **新类型 `Type::Opaque(name)`**：包提供的不透明句柄，IR 为 `ptr`、**不受 ARC 管理**
  （生命周期由包自己的 open/close 掌管），与 `file` / `thread` 同一阵营。
- **链接期自动链库**：把选定平台的 `.a`（或 `.lib`）加入链接命令，用户不写路径。
- **许可可见**（§25.8）：`manifest.txt` 的 `license` 为必填字段，构建时回显到日志。

### Changed（变更）
- **两阶段解析**：先解析一遍取 `import` 列表 → 解析包（拿到不透明类型名）→ 再重新解析，
  使 `var h: DemoHandle = ...` 这类包类型名在**语法阶段**就被识别为 `Opaque` 而非结构体。
- **`analyze` 与 `compile` 接受外部函数表**：包函数对调用方与本地函数别无二致，
  区别仅是「函数体在别人的 `.a` 里」。名字冲突当场报错。
- **void 函数调用可作语句**：`close(f);` / `demo_free(h);` 这类「只做事、不给值」的调用
  此前在 codegen 被拒（「返回 void，暂不支持用作表达式」），现已放行。
  「把 void 当值用」仍由**语义层**挡住——两条规则各司其职。
- **首次引入第三方依赖** `zip` / `serde_json` / `sha2`：都只服务于「读外部数据」这一处；
  哈希与容器解析不手写（手写出 bug 的代价是「恶意包被判为可信」）。

### Added（测试）
- 测试组 **H**：用 `tools/make_sample_pkg.sh` 现场构建示例包 → 安装 → `import` 后编译运行并比对输出
  → **故意篡改一个字节**验证 SHA256 确实拦得住。
- `tests/pkg/use_demo.dial` 覆盖四种签名形态：纯数值、无参、返回不透明句柄、
  以不透明句柄为参数，以及 void 释放函数。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.93.md](rationale/v0.93.md)。

---

## [v0.92] — 2026-10-31

### Added（新增）
- **`char` 类型**：一个 Unicode 码点。值类型、无 ARC、无堆分配、无空指针，
  IR 层与 `int` 同为 `i64`，但**类型独立**。
- **字符字面量** `'a'` / `'中'` / `'\n'`，支持 `\n` `\t` `\r` `\0` `\\` `\'` `\"` 转义；
  空 `''` 与多字符 `'ab'` 在**词法阶段**即被拒绝（它们根本不是一个字符）。
- **比较运算**：`==` `!=` `<` `>` `<=` `>=` 按码点序比较（`'a' < 'b'` 为真）。
- **零成本显式转换** `char_to_int(c): int` 与 `int_to_char(n): char`：
  二者**不产生任何 LLVM 指令**（char 与 int 同表示），纯粹是编译期的类型视图切换。
- **C Runtime** `dial_print_char`：按 UTF-8 编码输出 1~4 字节。
  非法码点（负数、> `0x10FFFF`、UTF-16 代理区 `D800~DFFF`）当场打印诊断并终止。
- 正向示例 `examples/char_demo.dial`（含多字节 UTF-8 `中` / `é`）；
  负向用例 5 个（char 算术 / char 与 int 混用 / `char_to_int` 传 int / `int_to_char` 传 char / int 赋给 char 变量）。

### Changed（语义与规则变更）
- **统一容器写回规则**：废除 `m = m.put(1, 1)` 这类手写赋值，与数组 `push` 统一——
  **所有原地修改并触发 COW 的方法（`push` / `put`），编译器自动写回，严禁用户手写赋值**。
  现在 `a = a.push(...)` / `m = m.put(...)` 会在**编译期被拒绝**
  （把结果赋给**另一个**变量仍然允许，那是取新容器而非就地修改）。
- **版本号规则重写**：废弃此前的「十进制递增 / 三段式」表述，改为
  **最多两位小数**（`v0.1`…`v0.9`、`v0.91`、`v0.92`），版本标题统一为 **`[v0.92]`** 而非 `[0.92.0]`，
  且**版本号必须严格按时间递增、绝不倒流**。
- **许可隔离声明**（README §3.3.1 / §25.8 / LICENSE）：明确**按组件划分**——
  编译器前端为三重许可，但被静态链入用户程序的 `libdial_std.a`
  **强制锁定为 MIT / Apache-2.0，绝不允许以 GPL 条款链接进闭源程序**。

### Fixed（修复）
- **未捕获异常打印出乱码**（真实缺陷）：`dial_error_msg` 返回的是不透明 `DialString` 句柄，
  却被直接当作 `const char*` 传给 `dial_panic`，导致 panic 只打印出一个数字（实测 `9`）而非错误消息。
  已补上 `dial_string_cstr` 转换，现在正确输出
  `dial: 未捕获的异常（panic）: 无法打开文件（路径不存在或无访问权限）`。
  新增测试组 **G** 锁定该契约。
- **§27.9.5 的 select 空转教程自相矛盾**：原文称「`while (true) { select { ... } }」本身安全，
  实则全体 EOF 时 select 只是**直接结束**、不会跳出循环，于是每轮立刻结束又立刻重入，
  构成 **100% CPU 空转死循环**（实测 5 秒超时、`exit 124`）。
  已改为演示两种安全退出写法，并各配一个可运行正例（连跑 10 次输出稳定）：
  `examples/select_exit_condition.dial`（`while (not done(ch))`）与
  `examples/select_exit_break.dial`（case 内 `break`）。
- **§26.1 异常语法漏洞**：原文笼统断言「throw 必须出现在 try 块内」，混淆了两种情形。
  现明确区分（§26.1.1）：**显式 `throw Error(...)`** 不在 try 内 → **编译期报错**；
  **内建函数的隐式抛出**（`open` / `read` / `write` 失败）允许写在任何地方 → **编译通过**，
  运行到该处若无捕获则触发未捕获异常 panic 终止。
- **首页示例使用 `+` 拼接字符串**：Dial 的字符串没有 `+` 运算符，已改为 `print("hello, ".concat(name))`
  （同步修正生成器 `tools/build_web.py` 与产物 `docx/index.html`，避免重新生成时复发）。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.92.md](rationale/v0.92.md)。

---

## [v0.91] — 2026-10-30

### Added（新增）
- **新语法**（4 个关键字）：
  ```dial
  select {
      case recv(chA) as a { ... }
      case recv(chB) as b { ... }
      default { ... }
  }
  ```
  **刻意不照搬 Go**：接收永远是**函数调用** `recv(ch)`（没有 `<-` 运算符），
  绑定变量用 **`as`** 关键字（不是 `:=`）。`select` 是**语句**不是表达式——
  各 case 的绑定变量类型可以互不相同，不存在统一返回类型，编译期拒绝 `let x = select {...}`。
- **内建语义三条**：
  1. **case 只拿真实值**：分支体内绝不会拿到空指针（底层非阻塞取，元素被抢走会自动重挑）；
  2. **全体 EOF → select 直接结束**：所有通道都「已关闭且已空」时，select 语句结束，
     **一个分支都不执行**（包括 `default`）；
  3. **`default` 使 select 非阻塞**：没有任何通道就绪时立刻执行 default，绝不等待。
- **事件驱动的等待原语**（C Runtime）：全局事件纪元 `epoch` + 全局条件变量广播。
  - **不丢唤醒**：先读 epoch → 再查就绪 → 再睡到 epoch 变化，过期的等待假设永不成立；
  - **不死锁**：notify 一律在**释放通道本地锁之后**调用，
    「持通道锁 → 取全局锁」与「持全局锁 → 取通道锁」两种嵌套**都不存在**。
- **随机选取就绪通道（防饿死）**：多通道同时有数据时随机挑一个，
  起点用**原子递增**计数器取模（不用 `rand()`——它不是线程安全的）。
- **C Runtime**：`dial_chan_epoch` / `dial_chan_wait` / `dial_channel_pick_ready`，
  以及 selector 对象（`dial_sel_new` / `add` / `pick` / `recv` / `free`）。

### Added（测试与示例）
- `examples/select_two_channels.dial`：**双通道并发**——两个生产者往不同通道发数据，
  一个消费者同时监听；顺带校验「收到的确实是本通道的元素」（防串通道）。
- `examples/select_default.dial`：`default` 的非阻塞语义。
- `examples/select_eof.dial`：全体 EOF 时 select 直接结束。
- `examples/select_fairness.dial`：随机选取的防饿死断言（两个通道都必须被处理过）。
- 负向用例新增 9 个（case 非 channel / 元素类型未定 / 缺 `as` / 绑定变量作用域外使用 /
  case 不是 recv 调用 / select 为空 / 两个 default / select 当表达式 / case 用 try_recv）。

### Fixed（修复）
- **IR 生成缺一条跳转导致编译器段错误**：select 的 setup 块原先没有接上调用方当前块，
  使 `while` 的 body 块变成无终结指令的空块，LLVM 在分支概率分析时段错误。
  已补上「当前块 → setup」的跳转。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.91.md](rationale/v0.91.md)。

---

## [v0.9] — 2026-10-30

### Added（新增）
- **内建 `is_null(x)`**：句柄判空。准入名单 `string` / `int[]` / `map` / `error` /
  `channel<T>` / `thread` / `file`；`int` / `float` / `bool` 是值类型、永远非空，
  编译期直接拒绝（而不是让它变成恒为 false 的死代码）。
  IR 层只是一次指针比较（`ptrtoint` + `icmp eq 0`，优化器折叠回 `icmp eq ptr %p, null`），
  **零函数调用、零引用计数操作**。
- **内建 `recv_timeout(ch, ms)`**：带超时的接收。超时或已关闭且已空时返回空指针
  （用 `is_null` 判别），**绝不无限期阻塞**。
  所有权规则与 `recv` 完全一致。底层就是 `pthread_cond_timedwait` + 一个绝对时刻，
  **没有引入任何状态机**。
- **测试组 F**：绕开编译器直压 `stdlib/dial_channel.c` 的并发压测
  （`tests/stress/channel_mt.c`）：8 个消费者抢同一个通道、100 万元素、队列深 64。
  用「位图 + 原子 `fetch_or`」逐个记录领取情况，重复领取即判失败；总数必须守恒。

### Fixed（修复）
- **多消费者丢唤醒（死锁）**：通道的所有状态变化通知由 `pthread_cond_signal`
  改为 `pthread_cond_broadcast`。单消费者下无差别，多消费者下前者会让某个消费者
  永久阻塞。正确性优先于惊群开销。
- **超时不会吞掉元素**：`pthread_cond_timedwait` 返回 `ETIMEDOUT` 后回到 `while` 条件
  重新判定，若此刻队列非空则正常取走——「元素恰好在超时这一刻到达」也能被拿到。
- **超时基准钟从 `CLOCK_REALTIME` 切回 `CLOCK_MONOTONIC`**：底层超时取用的绝对时刻
  原先取自 `CLOCK_REALTIME`，会被 NTP 校时跳变干扰（校时回拨可能让等待被提前唤醒、
  前拨则拖长等待）。现已切回单调钟 `CLOCK_MONOTONIC`，借助 Linux vDSO 直接在用户态读取、
  无需系统调用，时间读取更快，且彻底不受 NTP 校时影响；因彻底放弃 macOS，
  无需为 `pthread_condattr_setclock` 的兼容性做任何妥协。

### Added（测试与示例）
- `examples/is_null_demo.dial`：EOF / 超时 / 真实值三种情形下 `is_null` 的判定。
- `examples/recv_timeout.dial`：超时返回空值；超时与到达同时发生时以「有元素」为准。
- `examples/channel_multiconsumer.dial`：**两个消费者抢同一个通道**。
  每个消费者把收到的元素转发到自己的报告通道，主线程分别取数后相加——
  各消费者分到多少不确定，但**总数恒为 100**（每个元素只被弹出一次）。
  注意多消费者必须用 `recv` + `is_null` 判结束，**不能用 `try_recv`**：
  后者在「队列暂时空」时也返回空指针，那不是 EOF，会漏收元素。
- 负向用例新增 8 个（`is_null` 作用于标量 / 参数个数 / `recv_timeout` 的四种错用 /
  两个新名字撞保留名）。

### Changed（文档）
- README §27.8 新增「判空、多消费者安全与超时」，§27.9 边界表同步；
  §18 测试章节补 F 组与新示例清单；§17.2 未实现清单同步。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.9.md](rationale/v0.9.md)。

---

## [v0.81] — 2026-10-30

### Added（新增）
- **内建 `close(ch)`**：关闭通道（幂等），广播唤醒所有阻塞在 `send` / `recv` 上的线程。
  同时服务于 `file` 与 `channel`（二者都是「显式终结」），按实参静态类型分派。
- **内建 `done(ch): bool`**：**已关闭 且 已空** —— EOF 的精确定义，且**单调**为真，
  因此 `while (not done(ch)) { r = recv(ch); }` 这个循环**必然终止**。
- **内建 `closed(ch): bool`** 与 **`len(ch): int`**：是否已关闭 / 队列中当前的剩余元素数。
- **内建 `try_recv(ch): T`**：非阻塞取出，队列空立即返回，绝不等待
  （配合 `len(ch) > 0` 使用；单选消费者下该条件保证必然取到）。
- **C Runtime**：`dial_channel_close` / `closed` / `len` / `done` / `try_recv`。
  `closed` 标志**不设 atomic**——因为 `done` 必须与元素计数在同一互斥临界区内判定，
  拆成两个原子读反而会读到不同时刻的状态。

### Changed（语义变更）
- **`recv` 具备 EOF 语义**：「已关闭且已空」时**立即返回空指针**，不再永久阻塞。
  未关闭时行为完全不变（既有代码不受影响）。
- **`send` 到已关闭通道 → Fail-Fast**：元素会永久滞留，使 `done` 永远为假，
  消费者的收尾循环永不结束——一次「关了还发」会把 EOF 语义整个废掉。
  ReleaseSafe 及以上当场终止；`--fast` 下关闭该检查。
- **`print` 一个 NULL `string` → Fail-Fast**：EOF 之后误用取回的值时，
  在使用点打印诊断并 `ud2`，而不是解引用 NULL 变成一次无解释的段错误。
- **保留名校验补齐**：`close` / `closed` / `len` / `done` / `try_recv`
  不能用作函数名或结构体名（此前 `close` 虽按名字分派内建却未列入保留表）。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.81.md](rationale/v0.81.md)。

---

## [v0.8] — 2026-10-30

### Added（新增）
- **类型**：`thread`（线程句柄，显式 `join` / `detach`，不受 ARC 管理）与
  `channel<T>`（通道句柄，跨线程共享 → **原子**引用计数 ARC）。
- **内建 `spawn(fn, args...)`**：创建并**立即**启动线程。所有实参在 `pthread_create`
  **之前**写进 argcell —— 零竞争窗口，这也是「move_to 到线程」的**唯一**合法形式。
- **内建 `move_to(x, ch)`**：编译期 intrinsic，把 `x` 的所有权移交给通道。
- **内建 `join` / `detach` / `channel(cap)` / `recv(ch)`**。
- **C Runtime**：新增 `stdlib/dial_thread.{h,c}`（argcell + `pthread_create` 封装）与
  `stdlib/dial_channel.{h,c}`（有界环形队列 + 互斥量 + 条件变量 + 原子引用计数）。
- **Moved 状态进 Sema**：被移交的变量此后**读取 / 写入 / 再次转移**全部编译期拒绝
  （「变量已转移，无法使用」）。标记落在声明该变量的作用域，分支内移交也会被保守拦下。
- **`spawn` thunk**：编译器为每个被 spawn 的函数生成私有 thunk（`void f(ptr argcell)`），
  按 8 字节槽宽取出实参后调用真函数，并负责释放被移交进来的那一份所有权。
- **保留名校验**：`spawn` / `join` / `detach` / `channel` / `recv` / `move_to` / `thread`
  不能用作函数名或结构体名。
- **示例**：`examples/{thread_spawn,thread_scalar,channel_move,channel_thread,channel_array}.dial`；
  **负向用例** 19 个（含 `move_use_after` / `move_twice` / `spawn_non_void_ret` /
  `spawn_struct_param` / `channel_int_elem` / `fn_named_move_to` 等）。

### Changed（变更）
- **`moved` 集合由「变量名」改为「栈槽指针」**：变量名会跨作用域 / 跨函数重名，按名字记账
  会让同名变量互相误伤（该 release 的被跳过 → 泄漏）。改为按 alloca 槽位记账后天然精确。
- **结构体字段 ARC 补全**：`retain_struct_fields` / `release_struct_fields` 此前漏了 `Type::Error`
  （与两侧不对称），本次一并补上 `Error` 与 `Channel`。
- **链接**：C 编译追加 `-pthread`、链接追加 `-lpthread`（glibc ≥ 2.34 已并入 libc，此处为兼容老 glibc）。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.8.md](rationale/v0.8.md)。

---

## [v0.7] — 2026-10-30

### Added（新增）
- **可恢复异常语法**：`throw Error("msg");` 抛出；`try { } catch e { }` 捕获（**catch 后无圆括号**，
  `e` 类型自动推导为 `error`）；`try { } catch { }` 省略变量名表示忽略具体错误。
- **内建 `Error` 类型**：`error`，底层 `ptr`（不透明），**受 ARC 管理**；构造 `Error("msg")`，
  方法 `.msg()` 取消息字符串；`print(e)` 等价于打印 `e.msg()`。
- **C Runtime**：新增 `stdlib/dial_error.{h,c}`（引用计数错误对象）与
  `stdlib/dial_exc.{h,c}`（`jmp_buf` 容器 `DialExBuf` + `dial_throw_to` / `dial_exbuf_take_err` / `dial_panic`）。
- **失败与业务分离**：`open` / `read` / `write` 失败由「返回错误标志」改为**抛出 Error**
  （判据在 C 侧、抛出在 Dial 侧）；越界 / 除零**仍走** `ud2` / Fail-Fast，绝不混入异常。
- **三重开源许可**：`GPL-3.0 OR MIT OR Apache-2.0`（任选其一）。三份协议原文
  `LICENSE-GPLv3` / `LICENSE-MIT` / `LICENSE-APACHE` 与说明文档 `LICENSE` 随源码及发布包分发。
- **打包脚本** `tools/package.sh`：发布压缩包**强制校验**四份许可文件与 `.git` 均在包内，缺一即拒绝打包。
- **生态规划草案**：README 新增 [§25 包分发与依赖管理](README.md#25-包分发与依赖管理生态规划--最终规范)
  （`.dialpkg` 专属格式、自动解压到 `~/.dial/packages/`、接口签名文件 `.diali`、SHA256 强制校验、零版本冲突）。
- **示例与负向用例**：`examples/try_catch_demo.dial`、`examples/error_zero_cost.dial`；
  `tests/bad/{throw_non_error,catch_with_paren,throw_outside_try}.dial`。

### Changed（变更）
- `dial_file_read` 的失败语义由「返回空串」改为**返回 `NULL`**，使「读取失败」与「读到空内容」不再混淆
  （成功读取 0 字节仍返回空串）。
- `readme` 同步：顶部三重许可说明、§4.3 关键字（补 `try`/`catch`/`throw`）、§5.1 类型表（补 `error`）、
  §17.2（补回 `move_to` / 并发为「规划中」）、§24.2（文件 IO 现抛 Error）、新增 §25 与 §26。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.7.md](rationale/v0.7.md)。

---

## [v0.61] — 2026-10-29

### Added（新增）
- **`file` 不透明指针类型**：新增 `file` 类型，IR 中为 `ptr`，封装 C 的 `FILE*`，**不受 ARC 管理**；用户必须显式 `open` / `close`（与内存宪法显式指针回收契约一致）。
- **`open` / `read` / `write` / `close`**：仅封装 C 标准库 `fopen` / `fread` / `fwrite` / `fclose`，Rust 前端不重写任何文件逻辑；只提供最基础的 C 风格 API，无链式流、无额外语法糖。
- **C 侧实现**：`stdlib/dial_io.c` 新增 `dial_file_open` / `dial_file_read` / `dial_file_write` / `dial_file_close`；`stdlib/dial_string.c` 新增 `dial_string_cstr` / `dial_string_from_bytes`（供 `read` 从字节缓冲区构造字符串）。
- **示例与负向用例**：`examples/file_demo.dial`（写后读回并打印，黄金输出 `dialc-file-io`）；`tests/bad/io_bad_arg.dial`（open 参数类型错误须在编译期拒绝）。

### Changed（变更）
- 遵循版本号规则：本版属「小功能 / 新语法」，版本号 `v0.6 → v0.61`（Cargo `version` 字段按三段式写作 `0.61.0`，语义等同 `v0.61`）。
- README 同步：§4.3 关键字、§5.1 基本类型、§15 FFI 表与 C 导出函数、§17.2（从「未实现」移除文件 IO）、新增 [§24 文件 IO](README.md#24-文件-iofile-句柄) 与 [§22 早期版本摘要](README.md#22-早期版本摘要v03-及以前)。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.61.md](rationale/v0.61.md)。

---

## [v0.6] — 2026-10-29

### Added（新增）

- **Fail-Fast 陷阱基础设施**：新增 `runtime/dial_rt_trap.h` / `.c`，提供 `dial_trap()`（`__builtin_trap`，
  x86 生成 `ud2`）、`dial_bounds_fail` / `dial_null_fail` / `dial_assert_fail`（先打印诊断再 trap）。
  诊断走 `write(2)` 直写 stderr，并在陷阱前冲刷 stdout 缓冲，保证崩溃上下文完整。
- **LLVM Codegen 硬插入检查**：
  - 数组下标读（`Expr::Index`）与写（`a[i] = v`）前插入边界检查，用一次**无符号比较**
    同时判定 `idx < 0` 与 `idx >= len`（负数的位模式必然 >= len）。
  - 整数除法前插入除零检查——LLVM 的 `sdiv` 除数为 0 时结果是 poison（UB），
    优化器可据此把后续代码整段删掉，比崩溃危险得多。
  - 失败分支发射 `llvm.debugtrap`（Debug → x86 `int3`，可被调试器当断点捕获）
    或 `llvm.trap`（ReleaseSafe → x86 `ud2`，指令更短），随后 `unreachable`。
- **C Runtime 边界检查**：`dial_array_get` / `dial_array_set` / `dial_string_get` /
  `dial_string_substring` 全部加入边界检查，越界调用 `dial_bounds_fail`。
  另新增：OOM 不再返回 NULL（`dial_assert_fail`）、引用计数下溢检测（重复释放 /
  use-after-free 当场终止）、`dial_array_length` / `dial_map_size` 的内部一致性断言。
- **三档安全等级**：`--safety debug|safe|fast`，简写 `--release`（safe）、`--fast`（fast）。
  等级**同时**作用于 LLVM IR 与 C Runtime（`-DDIAL_SAFETY=N`），是编译期常量，
  ReleaseFast 下检查代码被编译器完全消除。默认永远是 Debug——安全是默认值。
- **测试组 D / E**：新增 `tests/trap/`（5 个用例），D 组断言必须当场因信号终止且
  **不得打印哨兵 9999**；E 组用 `--fast` 作对照组，证明陷阱确由编译器插入而非碰巧崩溃。

### Changed（变更）

- `dialc build` / `run` 新增 `--safety` / `--release` / `--fast` 参数。
- C 静态库按安全等级分目录构建（`.dialc_build/safety-<N>/`），避免不同等级的 `.o` 互相污染。
- README §17.1「未定义行为」重写：越界 / 除零在默认等级下**不再是 UB**，而是确定性崩溃。
- README §11.5 越界行为：由「无边界检查」改为「默认有边界检查，仅 `--fast` 回落到 UB」。

### Added（v0.6 补充）

- **`--fast` 危险警告**：编译器在使用 `--fast`（ReleaseFast）时**当场打印醒目警告**，
  说明该模式会关闭所有边界检查、除零检查和空指针保护，越界将退化为 C 的 UB。
  文档侧同步写入 README §3.2 与 §20.4，并出现在官网首页卡片。
  安全性是默认值，选择放弃它的人必须当场被告知代价。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.6.md](rationale/v0.6.md)。

---

## [v0.5] — 2026-10-29

### Added（新增）
- **`break`**：跳出最内层循环；仅允许出现在 `while` / `for` 循环体内，写在别处是编译错误（见 README §8.4）。
- **`continue`**：进入下一次迭代。关键语义——`for` 的 `continue` **先执行 `i = i + 1` 再回到条件判定**：
  实现上把自增放在独立的 `for.inc` 块，`continue` 跳到该块而非条件块；若跳到条件块就会跳过自增造成死循环。
- 新增示例 `examples/loop_control.dial`（覆盖 for+continue、for+break、while+continue/break 三种组合）。
- 新增负向用例 `tests/bad/break_outside_loop.dial`、`tests/bad/continue_outside_loop.dial`。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.5.md](rationale/v0.5.md)。

---

## [v0.4] — 2026-10-29

### Added（新增）
- **`map` 类型（int → int）**：新增哈希表类型，内存布局 `[int64 cap][int64 count][entries...]`，
  与 `string` / `int[]` 共用同一套 **ARC 引用计数**（`dial_map_retain` / `dial_map_release`）。
  采用线性探测开放寻址，删除用左移紧凑；负载因子超过 3/4 自动扩容并重新哈希。
- **`map` 方法**：`put(k,v)`（返回可能更新的指针，编译器写回接收变量）、`get(k)`（未命中 -1）、
  `contains(k)`（bool）、`remove(k)`（返回其值，未命中 -1）、`size()`。
- **`new_map()`**：无参内置函数，构造空表。
- **`read_line()`**：无参内置函数，从标准输入读一行；EOF 返回空串（绝不崩溃）。
- **`else if` 语法糖**：`else if (c) {..}` 编译期展开为嵌套 `if`，不引入新 AST 节点（见 README §8.2）。
- **范围 `for` 循环**：`for (i in start..end)` 循环变量 `i` 为 int，半开区间 `[start, end)`，
  可在循环体内重新赋值（见 README §8.3）。
- 新增示例 `examples/map_demo.dial`、`examples/control_flow.dial`；
  新增负向用例 `tests/bad/map_let_put.dial`、`tests/bad/map_bad_arg.dial`、`tests/bad/for_range_type.dial`。

### Changed（变更）
- `dial_map.c` 由旧版「非 ARC 的 `Map*` 堆分配（进程退出时由 OS 回收）」重写为与 `dial_array.c`
  **完全对称**的 inline-entries ARC 实现：指针因扩容 / COW 可能更换，编译器负责把新指针写回接收变量。
  这消除了旧实现与[内存宪法](README.md#19-内存宪法防-gc-铁律)（防 GC 铁律）的矛盾。
- Map 与 `read_line` 从「未实现」清单移除（v0.4 已暴露）。

### Fixed（修复）
- 修正 `0..5` 这类范围字面量此前被数字词法器误识别为浮点 `0.5` 的问题：仅当小数点后紧跟数字才视为浮点，
  `..` 现被正确识别为范围运算符。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.4.md](rationale/v0.4.md)。

---

## [v0.3] — 2026-10-28

### Added（新增）
- 内置类型转换函数 `int_to_float(int): float`，编译为 LLVM `sitofp`（x86-64：`cvtsi2sd`）。
- 内置类型转换函数 `float_to_int(float): int`，编译为 LLVM `fptosi`（x86-64：`cvttsd2si`，向零截断，与 C 的 `(int)` 一致）。
- 两者均为**纯编译期内联**，不经过任何 C 运行时函数，因此不引入额外依赖。
- 新增示例 `examples/short_circuit.dial`（证明短路求值）、`examples/conversions.dial`（证明显式转换）。
- 新增负向用例 `tests/bad/let_push.dial`、`tests/bad/implicit_conv.dial`、`tests/bad/mixed_arith.dial`。

### Changed（变更）
- **`and` / `or` 改为短路求值**，与 C 的 `&&` / `||` 一致：先求左操作数，若已能决定结果则不再求值右操作数。
  这保证了"判空后访问"类代码在左操作数为假时不会因右操作数被盲目求值而崩溃。
- **严格执行"零隐式转换铁律"**：算术、相等、比较运算均要求两个操作数类型**完全相同**；`int` 与 `float`
  之间不再有隐式提升或窄化，混用一律编译错误，必须显式调用 `int_to_float` / `float_to_int`。
- `let` 变量调用 `push` / `set` 等"原地修改且可能更换指针"的方法现在会在语义阶段被**明确拒绝**；
  此类方法的接收者必须是顶层 `var` 变量（详见 README §6.2、§11.3–§11.4）。

### Fixed（修复）
- 修复了此前 `and` / `or` 不短路可能导致的判空崩溃隐患（如 `p != null and p.x > 0` 在 `p` 为空时仍会求值右操作数）。
- 修复了 `int → float` 隐式转换可能造成的静默精度丢失（如 `1 + 2.0` 曾默默将 `int` 提升为 `double`）。
- 修复了 `let` 变量调用 `push` 时"容器被原地修改但接收者无法接收新指针"的悬垂风险，将其提前到编译期拒绝。

> 📄 本版的理由、实测数据与边界见 [rationale/v0.3.md](rationale/v0.3.md)。

---

## [v0.2] — 基线版本（本仓库首个规范化基线）

> 📄 本版的理由、实测数据与边界见 [rationale/v0.2.md](rationale/v0.2.md)。

---

## 未实现（-Unreleased-，v0.2/v0.3 语言层均未暴露，v0.4 起部分已落地）

> 📄 本版的理由、实测数据与边界见 [rationale/unreleased.md](rationale/unreleased.md)。

---

