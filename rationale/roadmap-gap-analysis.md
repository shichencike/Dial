# 缺口盘点与执行路线图（2026-11-18，源码核实版）

> 本文是「代替 LLVM」主线下对当前缺口的系统盘点，以及据此制定的分梯队执行计划。
> 与以往凭印象的盘点不同，**每一项都经过源码核实**：区分「LLVM 有而 asm 无」（可在
> LLVM 零改动护栏内补）、「两后端共同缺的语言特性」（加它须改 `codegen.rs`/`semantic.rs`，
> 违反护栏）、「战略/宪法否决」（永久不做）、「过时文档」（不是缺口）四类。

## 总纲：唯一后端哲学（用户 2026-11-18 确立，最高优先级）

> **asm 后端的目标是代替 LLVM，不是与 LLVM 并存。** 因此在本路线的一切推理中，
> **假设 LLVM 不存在——这个后端自己就是一个完整后端，而且只有它自己。**
> 本总纲统领下面所有分梯队决策；当任何「相对 LLVM」的论证与它冲突时，以本总纲为准。

三条核心内涵：

1. **没有退路，没有参照系。** 不得把 LLVM 当作 fallback、参考实现或「兜底后端」。
   凡是以「LLVM 也这样」作为不作为理由的论证一律不成立：在 LLVM 不存在的世界里，
   用户需要的能力就是**真的没有**。
2. **自己就是一个完整后端。** 代码生成、优化（O3）、调试信息（DWARF）、多目标架构、
   产物形态（单文件 / 极致体积 / 极致编译速度）、Fail-Fast、高级指令集——这些不是
   「追赶 LLVM 的加分项」，而是唯一后端与生俱来的**本来职责**。
3. **而且只有自己。** 全部正确性、完整性、性能责任由本后端独自承担，不存在「另一个
   后端补位」的可能。任何「两端共同缺、暂不用管」的处理，都必须附带一条
   「若 LLVM 不存在，此项是否仍可接受」的复核，不能仅凭 LLVM 也没有就搁置。

**对现有论证的修正指引（须牢记）**：

- B 类「LLVM 也没有，故不构成差距」：在总纲下应视为「语言层真实能力缺口，需独立裁决
  是否纳入」，而非「LLVM 也没有所以不算差距」。搁置理由只能是「触及语言 / 内存模型
  宪法需裁决」，不能是「LLVM 也没有」。
- 护栏「LLVM 路径零改动」「冻结 `codegen.rs` / `ast.rs`」：这是**并行过渡期的工程纪律**，
  当前批次继续严格遵守；但它不是永久宪法。在「LLVM 不存在」的终局里，本后端必须 own
  整条语言栈，届时这些冻结项须逐项复核解除。不得把过渡护栏误当作能力上限或永久不做。
- 性能基准「几何平均 ~0.54x / 领先 LLVM ~46%」：LLVM 仅作**外部标尺**用于量化，不是
  存在前提；即便 LLVM 消失，更快 / 更小 / 更准的目标仍独立成立。
  **（2026-11-19 修正：该 46% 数字已被公平 A/B 推翻，实测为 asm 慢约 12%，见
  `rationale/replace-llvm-multidim-audit.md`；LLVM 作为「外部标尺」的定位不变。）**

**当前状态（2026-11-18）**：本总纲即刻生效，为最高推理准绳与终局方向；现行护栏在并行期
继续有效、不因本总纲而改变当前批次的执行约束。阶段二第 11 项「SIMD 指令发射」原按用户
指示暂不开工；用户 2026-11-18 后续决策以本附则的「全部收回、串行推进」覆盖该暂缓，
从附则第 1 项开始逐项推进。

## 总纲执行附则：从 LLVM 收回全部职责的路线图（用户 2026-11-18 决策）

> 用户决策原文：**四类活全都要收回**，但不要一次性理想化，「写成路线图，一个一个来」。
> 本附则把总纲「假设 LLVM 不存在、只有自己」落成可串行执行的清单。
> 排序原则：先低风险独立性验证 → 再优化与语义深水 → 最后重型基础设施。
> 规则：**完成一项、勾选一项、再进下一项；不得并行铺开。**

### A. 四类活的真实起点（源码核实 2026-11-18，避免重复造已完成之物）

| # | 类别 | 真实起点 | 自主度 |
|---|---|---|---|
| R1 | 工具链去 LLVM 化 | `.s→.o`/链接走 `cc_command()`，优先 `zig cc → clang`（LLVM 系），gcc/cc 可选；C 运行时 gcc/cc 可编，无 clang-only 硬依赖 | 部分（clang 仅 LTO/PGO 分支用） |
| R2 | LLVM 独占优化回收 | asm 已有 fold/cse/dce/arc_simplify + O3 代数恒等式；尾调用消除 e3q 已落地（`elide_tail_calls`） | 大部分已自主，剩差集需诊断 |
| R3 | B 类语言语义 | select 超时 / impl 关联函数 `T::new()` / map<string> 值 / 并发原子，LLVM 也没有；此前按「两端都缺暂不管」搁置 | 未做；总纲下改判为真实缺口 |
| R4 | PGO / LTO / bitcode | `--pgo` 在 asm 下直接 `exit(1)`（`main.rs:504`，依赖 LLVM `.bc`）；LTO 仅 LLVM 分支；纯 LLVM 独占 | 零；工程量最大 |

### B. 串行执行顺序（一个一个来）

**第 1 项 · 工具链去 LLVM 化（独立性验证）** — ✅ **已完成（2026-11-18）**
- 目标：证明 asm 后端在**完全无 LLVM 工具**（仅 gcc/cc/binutils）环境下全链路可编译、可运行、产物正确。
- 落地：新增 `tools/verify_no_llvm_toolchain.sh`（已挂入 `tests/run_tests.sh` AF2 组），三层证据全绿：
  - **L1 环境自检**：gcc/cc/ar/as/ld 全部可用。
  - **L2 官方开关**：`DIAL_C_COMPILER=gcc` 下 18 个 golden 用例的 **asm 产物**与默认工具链逐字节一致（含 `.s` 产物），把工具链隔离为唯一变量；只测 asm 后端，不以 LLVM 后端为基准——因为发现 LLVM 后端在 gcc 下 fnptr/hof 有 PIE relocation 兼容问题，属 LLVM 侧、与 asm 独立性无关（已单独确认这两例 asm-via-gcc 编译运行正常）。
  - **L3 兜底选择**：构造屏蔽 clang/zig/lld/llvm-ar 的隔离 PATH（仅留 GNU cc/as/ld），不设 `DIAL_C_COMPILER` 时 `cc_command()` 自动落到 GNU cc，asm 编译运行正确。
- 护栏遵守：不改默认 cc 优先级、零产品代码改动、零新依赖、AArch64 零变化；默认 golden 门禁复跑全绿。
- 结论：**asm 后端零 LLVM 工具依赖得到回归脚本固化**，第 1 项收口。

**第 2 项 · LLVM 独占优化回收（诊断先行）** — ✅ **诊断完成，量化搁置（2026-11-18）**
- 目标：找出「LLVM `-O` 做而 asm 未做」且**有正 ROI**的优化，逐个实现（尾调用已做，不重复）。
- 落地：新增 `tools/diag_opt_gaps.py`（只读 .s，零产品代码改动）。反汇编对比 LLVM `--fast`（O3+LTO+SIMD）与 asm `-O3`，源码核实 pass 差集 + 形态量化，结论：**当前无正 ROI 的可回收 pass**。
  - **pass 差集核实**：asm 已具备常量折叠/CSE/DCE/ARC 简化/过程间折叠(e3p)/归纳变量强度削减(e3t)/InstCombine 代数恒等式(e3ad)/尾调用(e3q)/魔数除法(e3au)/ISA 微融合/浮点 GRA(e3av)；LLVM O3 有而 asm 无的经典 pass = LICM / 循环展开 / 函数内联 / SCCP。
  - **形态量化**（asm -O3 bench.s）：纯寄存器 mov 673、相邻拷贝链 82（x86 move elimination 零延迟，非瓶颈）；变量除数 idiv **0**（所有除法已是常量魔数路径）；高频 call 全是 C runtime（dial_string_new 39 / dial_string_eq 28 / dial_array_get 23 …），**无 Dial 用户小函数**——LTO 才可内联，asm 侧 IR 内联不可触及。
  - **落后项归因**：b02 1.071x（串行依赖链 `s=(s*31+i)%MOD`，LICM/展开免疫）、b20 1.057x / b22 1.144x（C runtime substring/拼接 + 数据依赖除法）。三者均架构级、非单点 pass 可解。
  - **基准现状**：`bench_asm_vs_llvm.py --quick` 实测几何平均 0.5321x、胜平负 14/11/3、字符串类 1.0361x——asm 整体领先 LLVM ~47%，高 ROI 优化已吃干净。
- 结论：**诊断后量化搁置，不硬做负收益 pass**（延续 e3at GRA 否决 / e3au ISA 边际递减 / e3av 跨函数 GRA 否决的纪律）。
- 重启条件（写死在诊断工具输出）：① 出现高频小函数调用的新 workload；② 出现大量循环不变量计算；③ 语言层新增可并行语义 / 显式展开提示；④ 落后项归因改变。满足任一则重跑诊断。
- 护栏：零产品代码、零新依赖、baseline/AArch64 零回归。

**第 3 项 · B 类语言语义（在 asm 侧自建）** — 部分完成：**impl 关联函数已落地**，其余三项按用户 2026-11-18 决策暂不展开
- 目标：把 LLVM 也没有的语言能力在 asm 后端补齐——select 超时 / impl 关联函数 `T::new()` / map<string> 值 / 原子类型，按总纲视为真实缺口。
- ✅ **已完成：impl 关联函数 `T::new()`（无 self 静态方法）**。纯编译期脱糖：`lexer.rs` 加 `ColonColon` token；`parser.rs` 加 token 预扫描（前向引用 + 友好诊断）+ `parse_impl_def` 按首参名区分方法/关联函数 + `parse_primary` 的 `::` 降级为 `Expr::Call(__impl_T_m, args)`。`codegen.rs`/`lower.rs`/`semantic.rs`/`ast.rs` 零改动（复用 Call 链路）。覆盖带参/空参、返回 struct/标量、链式调用、前向引用；双后端一致、26 源码 .s 零回归；新增 `tests/impl/impl_assoc.dial`（R18 组）。
- ⏸ **暂不展开**：select 超时（底层 `recv_timeout` 已具备，仅缺多通道语法糖）、map<string> 值（触及 semantic 泛型策略）、原子类型/无锁（内存模型宪法裁决）。用户 2026-11-18 明确本轮 R3 只做 impl 关联函数；其余待单独裁决再启。
- 护栏遵守：零新依赖、`codegen.rs`/`ast.rs`/1.10.0 零改动、AArch64 零变化、双后端一致。

**第 4 项 · PGO / LTO / bitcode（自建 profile 基础设施）** — ✅ 已完成（2026-11-18）
- 目标：让 asm 不再因缺 `.bc` 拒绝 `--pgo`，自建「插桩 → profile 收集 → 优化决策」闭环。
- **诊断修正（`tools/diag_pgo.py`）**：原描述的"跨模块 IR 合并 LTO"经源码核实——源码级 import 早已合成单个 `IrProgram`（天然 LTO），包名 import 是 C `.a` 机器码非 IR（不在 IR 合并范畴），故 LTO 无 IR 层工作量；bitcode 只是喂 clang 的手段非能力，asm 不需自建。块布局 ROI 实测：37 多块函数、55 回边循环头 100% 已对齐、热路径 fallthrough、冷块甩末尾——通用块重排是负 ROI（不硬做）。真正缺口是 `--pgo` 被 `exit(1)` 拒绝。
- 实际落地（自研、零 bitcode、零 LLVM 工具）：① `src/pgo.rs` 自研 `.dialprof` 纯文本格式（DIALPGO1）；② `runtime/dial_rt_pgo.c` 的 `dial_pgo_dump` 读 `DIAL_PGO_FILE` 写回；③ `codegen_asm.rs` 的 `PgoPlan` 三态——插桩（`.comm` 计数器 + `pushfq/incq/popfq` + main 返回前保护易失寄存器后 dump）、热函数判定（入口+循环计数、相对阈值 max×10%、main 强制、全零回退）、`.text.hot/.text.unlikely` 分段 + 冷函数省对齐；④ `main.rs` 两段式编排（插桩编译→训练运行→读回→优化重编），asm PGO 独立于 `--fast`。
- 验收：`--pgo` 在 asm 下可用、零 `.bc` 依赖、端到端输出与 baseline 一致、profile 确定性、24 源码 `-O3` `.s` 逐字节零回归；门禁 `tools/verify_asm_pgo.sh`（AR 组）四层证据全绿。
- **性能如实记录**：A/B 实测 `pgo/baseline≈0.997`（中性偏微正，落在噪声内）——与诊断一致：asm 默认布局已近最优，PGO 收益上界低。R4 的核心价值是**功能完整性**（`--pgo` 不再被拒、零 LLVM 依赖），非性能飞跃；不硬声称加速。
- 护栏：零新依赖、`codegen.rs`/`ast.rs`/1.10.0 零改动、AArch64/-g 不进 PGO、baseline 逐字节零回归、每子步独立提交可回滚。

### C. 状态

- ~~第 1 项（工具链去 LLVM 化）~~ ✅ 已完成（2026-11-18，`tools/verify_no_llvm_toolchain.sh` AF2 组）。
- ~~第 2 项（LLVM 独占优化回收）~~ ✅ 诊断完成、量化搁置（2026-11-18，`tools/diag_opt_gaps.py`）。
- ~~第 3 项（B 类语言语义）~~ ✅ impl 关联函数 `T::new()` 已落地（2026-11-18，`tests/impl/impl_assoc.dial`）；其余三项按用户决策暂不展开。
- ~~第 4 项（PGO / LTO / bitcode 自建）~~ ✅ 已完成（2026-11-18，`tools/diag_pgo.py` 诊断 + `tools/verify_asm_pgo.sh` AR 组门禁；自研 `.dialprof` 闭环，零 bitcode）。
- **收回路线图四项全部收口**。唯一后端总纲执行完毕：asm 不再把任何能力交给 LLVM。

## 0. 护栏（路线的硬边界）

1. LLVM 路径零改动：`src/codegen.rs`、`src/ast.rs` 相对基线冻结。
2. 语义构造已清零（M7-e3as 后，LLVM 能编译而 asm 拒绝的构造为零）。
3. AArch64 普通程序零变化；版本 1.10.0；零新依赖；docx 确定性；不打 tag。
4. 每批带 A/B 交替 + 归因 + 全门禁 + 证据链。

**推论**：只有「LLVM 已支持、asm 缺失」的项能在护栏内补齐；语言层新特性若要动
`codegen.rs`，必须先申请放宽护栏或列为独立版本主题，不能塞进 asm 后端批次。

## 1. 盘点结果（四类）

### A 类 · LLVM 有而 asm 无 —— 护栏内可做

盘点结论：**此类已清零**。逐一核实：

| 候选 | LLVM 状态 | asm 状态 | 结论 |
|---|---|---|---|
| 内联汇编 `@asm`/`@asm_intel` | 支持 | M7-e3as 已支持 | 已清零 |
| impl 具名类型方法（`self`） | 支持 | M7-e3aq 已支持 | 已清零 |
| 嵌套下标赋值 `a[i][j]=v` | 支持 | e3ao 已支持 | 已清零 |
| sizeof/alignof/offsetof、bitcast、assert/panic、is_null、drop、channel、spawn/join、HOF、select | 支持 | e3ag~e3an3 全支持 | 已清零 |
| SIMD `simd<T,N>` | 支持 | M7-e3az 已支持 | 显式向量类型已补齐（逐值一致）；e3x 否决的是**自动向量化**，两者不同。指令级 SSE/AVX 融合见阶段二补充第 11 项 |

### B 类 · 两后端共同缺的语言特性 —— 须改 codegen.rs，违反护栏

这四项经源码核实 LLVM 后端**也没有实现**，加它们必须改冻结文件：

| 项 | 核实证据 | 性质 |
|---|---|---|
| `select` 语句级超时 | `src/codegen.rs` 无 `select timeout` 实现；README:2116 记「未实现」，变通为单通道 `recv_timeout` | 语言新特性，两后端共同缺 |
| impl 关联函数 `T::new()`（无 self） | `gen_method_call` 与 `make_impl_call` 均假设 `self=params[0]`；semantic.rs:354 明确 self 是首参；无无-self 分派 | 语言新特性，两后端共同缺 |
| `map<K,string>` 值 / `values()` 非 int | semantic.rs:3952 **语义层统一拒绝** | 语言新特性，两后端共同缺 |
| 线程池 / 协程 / 异步、原子类型 / 无锁结构 | README:2118-2119「未实现」 | 触及内存模型与并发宪法，需设计裁决 |

**结论**：这些不是 asm 后端落后 LLVM，而是**语言本身尚未提供的能力**。在「代替 LLVM」
语境下它们不构成差距（LLVM 也没有）；要做必须作为独立语言版本主题并改 `codegen.rs`，
不属于 asm 后端补齐范畴。

### C 类 · 性能层 O3 —— 真正的剩余工程，但高成本/边际

| 项 | 现状 | 决策 |
|---|---|---|
| caller-saved GRA | 未做 | M7-e3at 量化否决（上界比 0.629<1、86.8% 栈槽仅访问≤2 次为 ABI 强制落栈），附重启条件 |
| LLVM 式高级分配优先级（span×freq） | ✅ 部分落地（M8-gra-span） | 无-call 纯计算路径加入 live-range 跨块跨度权重，b05 2.098x→1.909x、几何平均 0.9828x→0.9808x；含-call 路径实测退化已回退（callee-saved 池太紧张，跨度≠热点） |
| 递归纯函数闭式特化（M8-recspec） | ✅ 已落地（语义级激进） | 私有后端专属：e3p 解释器探针拟合闭式 + guard，b12 3.30x→0.477x、几何平均 0.98x→0.914x；「改 ABI 扩寄存器池」经三份诊断证伪负收益且破坏 C 互操作，不碰 |
| 浮点 / 跨函数寄存器分配 | 未做 | 高成本内核级方向，**建议先做同类收益诊断再决定**（延续 e3at 纪律） |
| 多级管线（LTO 级） | 未做 | 同上，须先量化 |
| ISA 微融合 | POPCNT/LEA/掩码/常量乘已做 | e3au 后边际收益递减，b02/b13/b22 落后项是 C runtime + idiv 微码主导，非微融合可解 |
| 算术循环指令级调度（b04/b07/b08/b14） | 证伪搁置 | 五个汇编级 POC 逐一证伪冗余 mov(0.997x)/魔数外提(0.998x)/循环展开(1.001x)/符号修正/逐指令复刻(0.998x)——两后端循环代码性能无差异，1.3x 来自 LLVM 整程序深层优化 |
| 循环内线性递推闭式（M9-linrec） | ✅ 已落地（clang -O3/SCEV 同族，语义级激进） | 无参纯整数单循环仿射(s=(a·s+b)%m)/斐波那契递推，i128 矩阵幂 O(log N) 算终值折叠为 return const；**e3p 解释器截断上界真实执行验证**（反证注入错误系数被正确拒绝）。b04 3.557→0.005ms、b07 3.282→0.005ms，几何平均 0.914x→0.574x、**总耗时 1.07x→0.911x 首次反超 LLVM**；b08 Collatz 无闭式未解决、带参递推仍走 fold_pure_calls |

基准现状：
~~（几何平均 ~0.54x，整体领先 LLVM ~46%；胜/平/负约 14/10/4）：
算术类基本持平（0.985x）、数组类领先（0.76x）、递归类大幅领先（0.025x）、
位与控制流领先（0.81x）、字符串类略落后（1.09x）。~~
**2026-11-19 修正**：上述为不公平对比（asm O2/O3 vs LLVM 默认 O0 + 常量折叠干扰）产物。
同配置 `--fast -O3` 公平 A/B（跑两次，见 `rationale/replace-llvm-multidim-audit.md`）：
几何平均 **1.12x（asm 慢 12%）**，胜/平/负 = 6/6/16；递归类领先（0.55x）、位控制流略领先（0.94x），
算术循环（1.26x）、字符串（1.33x）、数组内存（1.60x）落后。性能上尚未代替 LLVM。
**2026-11-19 再更新（asm 自内联 dial_array_get）**：asm 在 ReleaseFast 下于 `inst_call`
就地发射 `load [a+24+i*8]` 内联数组读（`dial_array_get`），自己拿回 LLVM 靠 LTO 做的
优化。同配置公平 A/B：数组内存 **1.60x → 1.485x**、几何平均 **1.12x → 1.0905x**
（仍慢约 9%）、胜/平/负 6/7/15。ReleaseFast 双后端对比已固化进 golden 门禁。
剩余差距（GRA 全 spill + C runtime 字符串 + LTO 整体调度）仍属架构级，量化搁置。

### D 类 · 战略/宪法否决（永久不做，不进计划）

SIMD 完整后端（−6.7%）、caller-saved GRA（ROI 为负）、函数名重载、闭包、
运算符重载、用户自定义泛型/接口/trait、可变全局状态、追踪式 GC。

### E 类 · 过时文档 / 不是缺口（核实后排除）

- README:590「DWARF 尚未开工」是 e3v 段落的**历史叙述**，紧接 e3w 已落地（37 处 DWARF 代码）。
- README:338「尚未覆盖嵌套下标赋值」在 e3as 批次已修正。
- `lower.rs:1602` TypeLit 独立表达式：LLVM:4211 同样拒绝（「应由 Sema 先行拦截」），两后端同源。
- `builtin.rs` 的 thread 构造 / 布局内建「后续相位」：thread 构造 LLVM 自身未单独实现；
  布局内建已由 e3e 落地，属过时注释。

## 2. 执行路线图（按护栏兼容性与 ROI 排序）

### 阶段一：护栏内的确定性工作（低风险）

1. **过时文档清理**：修正 README:590 等历史叙述的时态标注，避免误导（纯文档）。
2. **收益诊断工具化**：把 e3at 的 `tools/diag_gra.py` 思路扩展到浮点/跨函数 GRA，
   先在 bench 与真实 workload 上量化 ROI（纯工具，零产品代码改动）。

### 阶段二：C 类性能方向（先诊断后投入）

3. **浮点寄存器分配收益诊断** → 若 ROI 为正再实现。
   ✅ **已完成（M7-e3av）**：诊断为正收益缺口（浮点全 spill，静态栈访问比 5.8×、
   动态 asm/LLVM 3.0×），已实现 fGRA（浮点寄存器驻留），动态比降至 1.64×。
   剩余边界（含 call 函数 / 单块函数）受 SysV xmm 全 caller-saved 结构性限制、
   且 bench 无含 call 浮点函数（浮点栈访问合计 0），ROI 为负，不再投入。
4. **跨函数寄存器分配收益诊断** → 同上。
   ⏸ **实质已被覆盖**：整数 caller-saved 由 M7-e3at 量化否决（上界比 0.629<1）；
   浮点跨 call 因 SysV xmm 全 caller-saved（无 callee-saved 兜底）结构上不可行，
   比整数更差。无新 workload 触发则维持否决（见 e3at 重启条件）。
5. **字符串类落后项归因**：b20/b22 的 C runtime 开销是否有 asm 侧可优化空间
   （如 substring 的边界检查、拼接的拷贝次数），先归因后动手。
   ✅ **已完成（M7-e3aw 附带）**：字符串类已追平 LLVM（几何平均 1.0016x，优于
   原记录的 1.02x；b20 0.949x / b21 0.984x / b22 1.107x）。唯一落后项 b22 的
   反汇编确认瓶颈是 `dial_string_substring` 库调用（每次分配新字符串）+ `s % 1e9+7`
   魔数除法（数据依赖、无法强度削减）；LLVM 靠 O3 把 28 个基准内联进 main 并用
   循环管线优化——架构级差距，peephole 不可达（与 e3r/e3s/e3t 结论一致），
   无 asm 侧可落地空间，不改动产品代码。

### 阶段二补充：编译速度维度（M7-e3aw）

6. ✅ **前端常量折叠解释器记忆化**：插桩归因发现 e3p 的 `fold_pure_calls` 占 asm
   前端近半（34.5ms），根因是朴素递归解释器对 `fibR(23)` 等纯递归函数重复求解
   97 万步。加 `(name, args)→result` 记忆化后编译提速 ~30%（bench 112.9→76.1ms），
   折叠值与生成 `.s` 逐字节不变。这是「代替 LLVM」在编译速度维度上首个前端
   算法级改进（e3an3 是产物/链接层）。

### 阶段二补充：DWARF 调试信息维度（M7-e3ax）

7. ✅ **DWARF 端到端 gdb 实证 + 形参 location 修复**：DWARF 自 e3w/e3z/e3aa/e3ab
   落地后从未做过 gdb 端到端验证。本批用 readelf/llvm-dwarfdump/gdb 对同一程序
   两后端 `-g -O0` 对比：DIE 树统计本已对等（subprogram/param/variable/location
   数完全一致），但 gdb 实测暴露真 bug——ASM 断点停在 `add` 时 `print a/b` 显示
   11/11（实参 3/4）。根因：`emit_dwarf_cu` 把形参 `DW_OP_breg7` 偏移写成占位
   `8*i`，而 prologue 实际写参位置是 `alloca_off[param_slots[idx]]`（局部变量用
   真实 alloca_off，故只有形参错）。修复后形参偏移接上真值，gdb 正确 print
   （7 参数混合类型含栈传参专项全对），运行时与非-g/AArch64 产物逐字节不变。
   **结论：调试信息维度从「段存在」升级为「gdb 实证可用」，护栏内缺陷清零。**
   后续对 string/struct/array 复合类型的 gdb print 实证：两端都只按句柄/裸指针呈现
   （Dial 运行时 COW/RC 句柄非 C 布局，DWARF 无对应结构描述），且 LLVM 的 bool
   呈现还有错——属**两端共同的语言层 DWARF 映射缺口**，按用户「两端都缺暂时不用管」
   指示搁置（修复须改冻结的 codegen.rs，违反护栏）；ASM 标量 int/float/bool print
   均正确、比 LLVM 更准。默认不进 asm 批次。

### 阶段二补充：高级指令集主线（用户 2026-11-18 决策启动）

用户确认多维度补齐后启动"超越 LLVM"的高级指令集主线，决策：
- **兼容策略**：编译期 `--target-cpu`（baseline/v2/v3/v4），默认 baseline 零回归，避免 SIGILL。
- **推进顺序**：由简到难——标量 ISA 扩展 → SIMD → 浮点 FMA → 加密/专用指令（最后，锦上添花）。

8. ✅ **e3ay `--target-cpu` 开关基础设施**：只建开关、不发射新指令。默认 baseline
   与旧版 `.s` 逐字节一致（18 源码），四级别编译运行正确，非法值 Fail-Fast。
9. ⏸️ **标量 ISA 扩展（诊断后搁置）**：ROI 诊断（`tools/diag_scalar_isa.py`）确认当前
   全部语料中 LZCNT/TZCNT/BMI1/CMOV 的可替代惯用法频次为 **0**（位扫描/变量移位/andn
   全 0，63 个 cmp+jcc 全是控制流非条件赋值）；popcount(e3y)/xorshift(e3n)/常量乘(e3au)
   三个 targeted 机会已被吃干净。**当前无可安全优化目标，硬做=零收益死代码**。重启条件：
   语言层新增 clz/ctz/ctpop 内建（双后端同步）或出现位密集新 workload。
10. ✅ **e3az 显式 `simd<T,N>` 原生向量（语义补齐批）**：asm 后端从「builtin 明确拒绝」
    到完整支持——构造（广播/逐 lane）、向量加减乘除取模/位运算、六种比较→simd<bool,N>、
    lane 提取、sizeof/alignof。实现为「Ptr 指向 N×8 字节栈段 + 逐 lane 展开为既有标量
    IR」，复用发射器、AArch64 零变化、GRA 隔离。两后端逐值一致（ffi demo + 广覆盖 +
    含 call）、83 源码 `.s` 零回归、19 golden 全绿。**注意**：这是**显式向量类型**（程序员
    主动使用、语言一等类型），与 e3x 否决的**自动向量化**（编译器强行向量化标量循环）是
    两件事，不引入后者的灾难性负收益。本批为语义补齐；下一步才做目标 CPU 指令发射。
11. ⏸️ **SIMD 指令发射 —— 诊断完成、量化搁置（2026-11-19，`tools/diag_advanced_isa.py`）**：
    原计划 v3+ 把逐 lane 展开 targeted 融合为 SSE/AVX 打包指令（paddq/addpd）。诊断三项
    证据一致否决此刻开工：① **无 workload**——bench 含 simd 的文件=0，simd 仅出现在
    golden + tests/bad（14 文件/50 行），无可测 ROI；② **发射层拿不到向量操作**——lower.rs
    已把向量逐 lane 展开为标量 IR（`IrInst::Bin/FBin/ICmp/FCmp` 40 处），codegen 层只见标量；
    ③ **布局绑定 LLVM 语义**——golden 断言 `sizeof<simd<int,4>>=32`（每 lane 8B），紧密打包
    布局会破坏双后端逐值一致。汇编佐证：simd golden 展开 1144 行、751 条标量算术/mov，且
    baseline 与 `--target-cpu=v3` 的 `.s` **逐字节一致**（当前零打包发射）。baseline 保持逐 lane
    标量（零兼容风险）；⚠️ 绝不重造 e3x 已证伪的完整自动向量化（−6.7%、b15 慢 18×）。
    **重启条件**：bench/真实 workload 出现热点 simd 运算，或语言层引入独立紧密向量 ABI
    （双后端同步、明确放宽 sizeof 护栏），或 e3x 结论被新证据推翻。
12. ⏸️ **浮点 FMA 融合 —— 诊断完成、量化搁置（2026-11-19）**：bench 无浮点类型命中、
    浮点 `a*b+c` 形态=0（乘加全是整数，走 imulq+addq），无负载；且 FMA 单次舍入与「先乘后加」
    两次舍入语义不同，对齐须改冻结的 codegen.rs，属双后端语言层裁决。**重启条件**：出现热点
    浮点乘加 workload 且实测有正 ROI，或语言层明确 FMA 舍入规范并放宽护栏。
13. ⏸️ **加密/专用指令（AES-NI/SHA/CRC32）—— 诊断完成、量化搁置（2026-11-19）**：
    tests/bench/examples 全部命中（25 文件/124 行）均为 C 库调用（dial_crypto_* 包 /
    dial_gui_painter_crc32），非本编译器发射的标量代码，ISA 发射层无从 targeted 融合；重写进
    语言层属独立主题、收益小难度最大，排序最后。**重启条件**：语言层引入原生加密内建由本后端
    发射其标量主体，或出现可实测热点且指令级加密有正 ROI。

**高级指令集主线状态（2026-11-19 收口）**：第 8 项开关基础设施 ✅、第 10 项显式向量
语义 ✅ 已落地；第 9 项标量 ISA、第 11 项 SIMD 打包、第 12 项 FMA、第 13 项加密指令
四项均经 `tools/diag_scalar_isa.py` / `tools/diag_advanced_isa.py` 诊断后**量化搁置**
（无正 ROI workload 或触及双后端护栏），重启条件已逐项写死。至此高级指令集主线
**无护栏内可立即开工且有正 ROI 的项**；「开关 + 语义」已就位，实际指令发射属有硬件、
无负载的储备能力，条件满足再重开诊断，不硬做零收益死代码。

### 阶段三：B 类语言特性（需单独裁决，默认不进 asm 批次）

6. `select` 超时 / impl 关联函数 / map string 值 —— 若要做，作为**独立语言版本主题**，
   明确放宽 `codegen.rs` 零改动护栏，双后端同步实现 + 全门禁。
7. 线程池 / 协程 / 原子 —— 先做内存模型宪法裁决（数据竞争、§20 零悬垂底线），
   裁决通过才进入设计。

## 3. 一句话结论

> **⚠️ 2026-11-19 多维审计修正**：下面这段是旧的「单维度乐观结论」，已被实测推翻，
> 保留仅为记录。以 `rationale/replace-llvm-multidim-audit.md` 为准：**当前不能代替 LLVM**。
> 两个硬缺口——(1) struct 进容器（B6）在 asm 全优化级别编译失败，LLVM 正确运行；
> (2) 同配置 `--fast -O3` 公平 A/B 几何平均 1.12x（asm 慢 12%，数组内存类慢 60%）。
> 旧结论「领先 LLVM 46%」是不公平对比产物；「语义层已完全追平」被 B6 缺口直接证伪。

~~**语义层 asm 已完全追平 LLVM；剩余差距全是性能深度与语言层新特性。**
前者须用收益诊断决定投入（高成本、易负收益），后者须改冻结文件、超出 asm 后端补齐范畴。
护栏内真正低风险可做的，是收益诊断工具与过时文档清理；其余要么先量化、要么先裁决。~~
