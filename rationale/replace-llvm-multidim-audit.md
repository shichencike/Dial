# asm 后端能否真正代替 LLVM：多维度审计（2026-11-19，实测版）

> ## ⚠️ 2026-11-19 战略转向：裁决口径已更新（审计正文保留作证据链）
>
> 用户 2026-11-19 决策：**目标是代替 LLVM，不是纯粹超越；性能够用即可，±5% 噪声下
> 继续死优化用户感知不到。** 本文下方「裁决：当前不能代替 LLVM」基于「性能落后 12%」
> 的旧数据；此后经 9 轮优化，性能已达标收口——几何平均 **0.574x / 0.572x**、总耗时
> **0.911x**（反超 LLVM），**维度 2「性能硬缺口」已消除**。
>
> **新裁决以 `rationale/replace-llvm-roadmap.md` 为准**：性能不再是代替的阻断项；剩余
> 待办转为「代替能力完备」——P0 正确性（本文维度 1 发现的 map string key 泄漏）、P1
> 平台（本文维度 4 的 AArch64 未实机验证）、P2 功能对等裁决、P3 文档收口。
> 下方五维数据与证据链**全部保留**，但「不能代替」的结论及其性能归因已被新战略取代。
>
> **2026-11-20 补记**：P0–P3 已全部收口（全量门禁 4 分钟全绿），维度 1 的 B6 struct 进容器、
> 维度 4 的 AArch64 实机语义/DWARF 均已修复并有门禁。同日源码级缺口盘点进一步确认：
> 在「性能够用即可」战略下，剩余项（AArch64 PGO/inline asm、DWARF 内联与复合类型、Os/Oz）
> 属**性能/体验的边际边界或两端共同缺口，不构成代替的硬阻塞**，已在
> `rationale/dialpkg-roadmap.md` §P4.4 与 `rationale/roadmap-gap-analysis.md` 如实标注。

> 本文回答一个此前一直被「单维度乐观结论」掩盖的问题：**自研 asm 后端现在到底能不能
> 真正代替 LLVM？** 结论不预设立场，全部由当机实测数据说话。审计范围覆盖五个独立维度，
> 任一维度不达标即不能宣称「已代替」。所有命令可复现。

> **2026-11-19 证伪记录（维度 2：pow2 除法运行时符号快路径，已回退）**：延续「逐点收益
> 诊断」纪律，按加权耗时差定位到 **b08「3n+1 总步数」占总差距 57%**（Δ+1.733ms、ratio
> 2.049），是当前最大单点短板。深挖汇编：`x%2==0` 已用 `testq $1`、`3*x+1` 已用 `leaq`
> 均为最优，但 `x/2` 用 6 条通用有符号 bias 修正序列；gcc -O3 靠值域分析证明 `x>1` 恒正、
> 用单条 `sarq`。受控 C 实验（内联汇编精确模拟）预测快路径省 33%、运行时符号检查版省
> ~18%。据此在 `inst_const_div_pow2` 实现「n>=0 走 sar、n<0 回退 bias」快路径（两种
> safety 语义均等价，负数除法双后端逐值验证通过）。**但真实编译管线背靠背 A/B 证伪**：
> 基线 b08 3.30-3.38ms，改动版 3.56-3.59ms，反而慢约 7-8%（两种分支布局均如此）。
> 推断运行时分支破坏了原无分支序列的乱序执行/微码融合收益，受控实验的孤立测量不成立。
> 遵循「不硬做负收益优化」纪律**已整体回退**，零代码留痕。这印证了审计报告原文
> 「idiv/除法微码主导、e3s 已证伪可从后端侧优化」的判断——b08 属 LLVM 深度值域分析
> 优势，非简单发射层 pass 可补，量化搁置。

> **2026-11-19 证伪记录（维度 2：b04/b07 算术循环"指令级调度"五连证伪，零代码留痕）**：
> 承接 M8-recspec 后加权总耗时仍慢 ~6.6% 的短板，按绝对耗时定位到 b04(3.56ms/1.30x)、
> b07(3.28ms/1.33x)、b08(3.34ms/2.03x)、b14(3.77ms/1.05x) 四个 `% 常量` 串行循环占大头。
> 汇编对照发现 asm 版 b04/b07 每轮有 ~10 条冗余寄存器搬运、魔数 `movabsq` 每轮执行、
> 符号修正用 `sar63+sub`（LLVM 用 `shr63+add`），据此提出三个直觉落点。**五个汇编级受控
> POC（同进程背靠背、9 采样取最小、校验和一致）逐一证伪**：① 消除全部冗余 mov → 0.997x
> （x86 move elimination 令纯搬运近零成本）；② 魔数外提到循环外 → 0.998x（movabs 单 uop）；
> ③ 4 路循环展开 → 1.001x（斐波那契 a/b 是真数据依赖链，展开无法打破）；④ 符号修正
> 改 LLVM 模式（关键路径同为 6 周期，无差）；⑤ **逐指令精确复刻两后端 b07 循环 →
> 0.998x（耗时完全相同）**。决定性结论：asm 与 LLVM 后端生成的 b07 循环指令级性能
> **无差异**，bench 观测的 1.3x 来自 LLVM 后端对整程序的更深层优化（clang -O3 甚至对
> 线性递推做闭式求解到 0.000ms），非局部发射层可补。遵循「不硬做负收益优化」纪律，
> **本轮零产品代码改动、零回退**。这印证 e3s/e3u 原判断：算术循环短板属 LLVM 深度
> 值域/闭式分析优势；唯一可攻方向是「循环内线性递推闭式」（clang 同族思路，与
> M8-recspec 递归闭式同源），留给后续单独评估 ROI，不与指令级微调混淆。

> **2026-11-19 后续更新（维度 2 部分改善：asm 自行内联 dial_array_get）**：
> 审计报告后，asm 后端在 `--fast`（ReleaseFast）下于 `codegen_asm.rs::inst_call` 就地
> 内联 `dial_array_get(a,i)`——发射 `load [a+24+i*8]`（数组 payload 布局），跳过 C
> runtime 调用的 call/ret 开销；safe/debug 保留原 call，Fail-Fast 边界检查不绕过；
> AArch64 保持原 call。这正是 LLVM 靠 `--fast` 的 LTO 吃掉、asm 长期吃不到的那块，
> 如今 asm 自己拿回（符合「LLVM 不存在」总纲）。公平 A/B `--fast -O3` 实测：
> 数组内存类 **1.60x → 1.485x**，整体几何平均 **1.1155x → 1.0905x**（仍慢约 9%），
> 胜/平/负 6/6/16 → 6/7/15。校验和锚点 + ReleaseFast 双后端 21 用例 + 450 差分 fuzz
> + 工具链独立性 + PGO + 全量 run_tests 全绿。已将 ReleaseFast 双后端对比固化进
> `tools/run_asm_golden.sh`（堵住「默认 safety 门禁测不到内联路径」的盲区）。
> **总裁裁仍不变**：整体仍慢约 9%，尚未全面代替 LLVM；剩余差距是 GRA 全 spill +
> C runtime 字符串路径 + LTO 整体调度的架构级工程，非单点 pass 可回收，量化搁置。
> `dial_array_set` 内联因 COW 分支 + 元素所有权结算 + 与 C runtime 布局耦合，风险/收益
> 比不划算，本轮不动（延续「不硬做负收益优化」纪律）。

> **2026-11-19 后续更新（维度 2 再改善：字符串字面量静态化）**：审计指出
> 「C runtime 字符串路径」是架构级差距，本轮攻其中最高 ROI 的一点——字符串字面量
> 编译期已知且不可变，却每次求值都调用 `dial_string_new`（malloc+strlen+memcpy）。
> b20「`x=x+"y"` 循环 4000 次」实测该调用占约 50% 开销。asm 后端新增 IR 指令
> `LeaStaticString`，把每个字面量在 `.rodata` 发射为静态 immortal DialString
> （`[rc=INT64_MIN][len][data\0]`，retain/release no-op），`Expr::StringLit` 直接取
> 静态句柄，彻底省去运行时构造；字符串不可变、`==` 走内容比较，无对象身份依赖，
> 语义等价。仅改 asm 路径（ir/lower/codegen_asm/pass），LLVM 后端从 AST 直接生成、
> 零改动。**实测 b20 2.275x → 1.084x（-52%，近乎追平）**，全 `.s` 中
> `dial_string_new` 调用 23→0，字符串类几何平均 ~1.33x→1.083x，整体 1.0891x→1.0794x。
> 过程中发现既有测试 R11 的 probe 用 `extern strchr` 返回裸 C 指针当 string 句柄
> （既有 UB：旧版 exit=132 double-free，测试只看 stdout 漏检），静态化进 .rodata 后
> 更早暴露为 SIGSEGV；已改用 channel close+recv 的合法 null 来源重写 probe（各优化
> 级别双后端一致、exit=0），不改编译器语义、不掩盖 bug。全门禁绿（golden 23/22 +
> 差分 fuzz 450 + 工具链 22 + PGO + 全量 tests + ARC 压测 + glibc 堆检查）。
> **总裁裁不变**：整体仍慢约 9%，字符串类已接近追平；剩余差距是 GRA 全 spill、
> 数组 set 的 COW、b22 substring/C runtime 等架构级工程，继续量化推进。

> **2026-11-19 后续更新（维度 2 里程碑：几何平均首次反超 LLVM）**：承接上轮把
> `dial_array_get` 内联后，数组内存类仍是最大短板（b15 冒泡 2.676x、b17 矩阵乘 2.091x）。
> 受控实验坐实 `dial_array_set(a,i,v)` 调用在冒泡内层占约 80% 开销。但 set 比 get 复杂：
> 它含 COW（refcount>1 则克隆）+ 元素所有权结算（引用元素需 retain/release）+ 返回可能
> 变更的句柄。安全方案——在 `codegen_asm.rs::inst_call` 仅 x86-64 + ReleaseFast 下生成
> **条件快路径**：实参装进 rdi/rsi/rdx（回退 call 直接复用），运行时检查 `refcount==1`
> 且 `kind==0`（DIAL_ELEM_INT），满足则 `mov v,[a+24+i*8]` 直写并返回 a；否则回退原
> call 保留 COW/ARC。safe/debug 保留原 call（Fail-Fast 边界检查不绕过），AArch64 原样。
> **实测**：数组内存类 1.556x → 1.0945x（b15 2.676x→1.195x、b17 2.091x→1.364x、
> b14 1.435x→1.019x、b16 1.231x→0.912x 反超）；整体几何平均 **1.0794x → 0.9793x，
> 首次 <1（按几何平均 asm 反超约 2%）**，胜/平/负 7/5/16 → 9/7/12。
> **诚实补充**：几何平均是基准标准指标且已反超，但**加权总净耗时仍 1.0728x（asm 慢约
> 7%）**——b08 3n+1、b14/b18 大耗项仍落后，两类指标结论不同，不可只报其一。
> 正确性：COW 共享写 2 万次压测双后端一致、原数组未污染、glibc 堆检查 0 错误；
> string[] 引用元素 set 走回退路径正确；golden 23/22、差分 fuzz 450、工具链 22、
> PGO、全量 tests 全绿。**总裁裁更新**：按几何平均已在统计意义上追平/反超 LLVM，
> 但按总耗时仍未全面代替；剩余 b12 互递归闭式、b08/b05 算术深度优化、b22 substring
> C runtime、GRA 全 spill 等架构级工程，继续推进。

> **2026-11-19 后续更新（维度 3：加入 LLVM 式高级分配优先级 span×freq）**：
> 用户追问「LLVM 有高级分配逻辑，asm 有没有」——核查确认：asm 已有三种固定绑定
> GRA + fGRA + ISA 微融合，但**缺 LLVM Greedy 分配器的 `priority = span × freq`**
> ——当前 top-N 排序只用 `use 次数 × 循环深度权重`，跨块长命值会被内层单块短命
> 中间值挤出物理寄存器。诊断坐实 b05「连乘幂内核」外层 phi 值 `s`/`a` 每轮迭代
> 走 248/256(%rsp) 栈往返（汇编 25 处栈访问、2 个高频槽），是当前最明确的
> 分配层短板。实现：`compute_live_span()` 统计每个 vreg 作为 def/use/phi 出现的
> 不同基本块数，`analyze_unified` 的绑定排序改为 `score × span`。**关键纪律**：
> 先全量加（含-call 路径），背靠背 A/B 逐项定位到含-call 函数退化（b21 +9.4%、
> b17 +6.4%、b15 +4.7%）——6 个 callee-saved 极紧张、跨块跨度大的中转值未必是
> 热点；据此**回退含-call 路径的 span 加权，仅在无-call 纯计算路径保留**。
> 修正后同配置 `--fast -O3` 背靠背各 2 次（5 采样取最小）：几何平均
> **0.9828x → 0.9808x**、加权总耗时 1.0752x→1.0749x、b05 **2.098x→1.909x（-9%）**、
> b08 外溢改善（~2.04x→1.948x），含-call 函数退化消失。b05 栈访问 25→16、
> 248/256 槽不再访问（phi 已驻留）。全门禁绿（golden 23/22 + 差分 fuzz 450 +
> 工具链去 LLVM 22 + PGO L1-L4 + 全量 tests）。**总裁裁不变**：几何平均已反超，
> 总耗时仍慢约 7%；本轮是分配层 targeted 增强，非 caller-saved live-range 全重写
> （后者 e3at 已 ROI 否决，未触碰）。剩余 b12 闭式、b08 值域、b22 substring、
> GRA 全 spill 等架构级工程继续量化推进。

> **2026-11-19 后续更新（维度 3 里程碑：递归纯函数闭式特化，私有后端语义级激进）**：
> 用户追问「私有后端又不是通用，为什么不更激进」。先用三份只读诊断证伪了最直觉的
> 「改 ABI / 扩 caller-saved 寄存器池」路子（可消除栈往返全 bench 仅 5 处、跨 call
> 长 live-range 槽 46 个 vs 583 ABI 强制流量，保护成本远超收益，e3at 否决仍成立；
> 且 ABI 必须 SysV 兼容以维持 C 互操作）——**这条路不碰**。真正的私有红利是**语义
> 特化**：LLVM 通用后端绝不会把互递归识别成奇偶判定。诊断坐实此前最大单项短板 b12
> 根因是 evR/odR 互递归本质 `n≥0 ? 1-(n&1) : 发散`，被 20000 次 × 平均 30 层递归吃掉。
> 实现（仅 pass.rs + asm.rs 挂接，O3+x86-64）：复用 e3p 解释器对递归纯整数单参函数做
> 129 点探针 + 55 点抽样、拟合闭式、逐点验证，入口插 guard（n≥0 走 O(1) 闭式、n<0
> 保留原递归体）。背靠背 A/B 各 2 次：**b12 3.30x→0.477x（-86%，反超 LLVM 2 倍）**、
> 几何平均 0.9817x→**0.9141x**、递归类 0.525x→0.3247x、总耗时 1.0748x→1.0661x；
> 逐项唯一变化 b12，b08 +7.6% 经汇编逐字节比对证实为噪声。全门禁绿（golden 23/22 含
> mutual_recursion + fuzz 450 + 工具链 22 + PGO L1-L4 + 全量 tests + 28 checksum）。
> **总裁裁更新**：几何平均优势扩大到 ~8.6%，b12 从最大短板转为领先项；加权总耗时仍慢
> 约 6.6%，b04/b07/b08/b14 除法延迟链是主因（常量除法已魔数优化到最优，下一步攻调度）。

> **2026-11-19 后续更新（互递归尾调用帧复用）**：asm 的文本级尾调用消除
> `elide_tail_calls` 此前虽把互递归 `evR`/`odR` 的 `call` 改成 `jmp`，但每层仍要
> `subq $120` 建帧 + `movq` 存参 + `addq $120` 拆帧——b12 互递归奇偶判定 4.263x
> 的根因。本轮扩展该 pass：caller/callee 同签名、同栈帧大小、无 callee-saved 时，
> 尾跳保持 rsp 不变、跳到 callee 的 prologue 内别名标签（跳过 subq、保留参数保存），
> N 层递归只建/拆一次帧。**实测 b12 4.263x → 3.360x（-21%）**，递归类 0.5562x→0.5316x，
> 整体 1.0905x→1.0891x（噪声内）。实现中曾踩严重 bug：别名标签误插在参数保存之后，
> 尾跳跳过保存、递归读到旧参数永不终止（b12 跑 6 分钟超时）；已修正为插在参数保存
> 之前，并新增 `tests/asm_golden/mutual_recursion.dial`（单/双参数互递归）钉死该路径，
> ReleaseFast 双后端门禁覆盖。剩余 b12 3.36x 是「递归 vs LLVM 折叠成 n%2 闭式」的
> 本质差距，属前端优化，本轮不动。全门禁绿（差分 fuzz 450 + golden 23/22 + 工具链 22
> + PGO L1-L4 + 全量 tests）。

> **2026-11-19 后续更新（维度 1 已修复）**：审计报告后，维度 1 的功能硬缺口
> 「struct 进容器（B6）」已在 asm 后端补齐——`lower.rs` 的 `coerce_elem_bits`/
> `reinterpret_bits` 支持 struct 装箱/解箱，新增 `unbox_struct_nullable`（无分支
> select 方案）处理 map get 未命中零值，并补齐 put/get_or/remove 的 box 所有权结算。
> `batch3_b6_smoke.dial` 现 asm O0-O3 全部编译、双后端逐字节一致，已纳入 golden（22 个）
> 与 V 组门禁；int-key 50 万次 put/remove 无内存泄漏（asm 1440KB ≈ LLVM 1664KB）。
> **总裁决不变**：维度 2（性能整体慢 12%）仍是硬缺口，故仍不能代替 LLVM。
> 另发现一个与本轮无关的既有问题：`map<string,*>` 的 string key 本地引用未归还，
> 50 万次循环泄漏约 30MB（string key + int value 同样复现），记入独立待办。
> 下方保留审计当时的原始结论与全部证据，不因后续修复而改写历史记录。

## 裁决（先给结论）

**当前不能代替 LLVM。** 五个维度中，四个达标或部分达标，**两个存在硬缺口**：

| 维度 | 结论 | 硬数据 |
| --- | --- | --- |
| 1. 功能对等性 | ❌ **有硬缺口** | 124 个可编译源码中 1 个 asm 全优化级别编译失败（struct 进数组/容器，B6）；LLVM 正确运行 |
| 2. 性能 | ❌ **整体落后** | 同配置 `--fast -O3` 公平 A/B：几何平均 **1.12x（asm 慢 12%）**，胜/平/负 = 6/6/16 |
| 3. 优化深度 | ⚠️ 部分达标 | 常量折叠/CSE/DCE/IVR/fGRA 等已吃干净；但数组内存类仍慢 1.60x，归因 LTO+寄存器分配架构级差距 |
| 4. 平台/工具链 | ⚠️ 部分达标 | 工具链去 LLVM 化 20 用例全绿；但 AArch64 只验证汇编写法、**不验证运行语义**，且内联汇编/DWARF/PGO 在 AArch64 降级 |
| 5. 正确性/一致性 | ✅ 达标 | 21 双后端 golden + 450 差分 fuzz 对比 0 不一致 |

**一句话**：asm 在「能编译的程序上输出正确、工具链独立、x86-64 语义完备」，但**还不能在功能覆盖和性能上全面接管 LLVM**——存在 1 个确凿的编译期功能缺口（struct 进容器），且公平对比下整体慢约 12%（数组/字符串/算术循环明显落后）。

> **2026-11-19 战略转向更新（覆盖上表裁决）**：上表两个硬缺口中，**维度 2 性能已消除**
> （几何平均 0.574x、总耗时 0.911x 反超 LLVM，见顶部转向声明）；**维度 1 的 B6 struct
> 进容器也已修复并纳入 asm 门禁**（见本文维度 1 的「已修复」更新段）。但战略转向的意义
> 在于：**性能已不再是代替的判据**——即便性能数字倒退一点，只要落在 ±5% 噪声与够用线内
> 仍算达标。当前真正的阻断项转为 map string key 泄漏（正确性）与 AArch64 实机语义（平台），
> 详见 `rationale/replace-llvm-roadmap.md`。

---

## 维度 1：功能对等性 —— ❌ 有硬缺口

### 测法
全仓 378 个 `.dial` 源码，逐个用 `--backend=llvm --no-link` 与 `--backend=asm -O0 --no-link` 编译，
比较成功/失败集合。排除含 `import` 的源码（单文件编译缺模块路径，非后端差异）。
`run_tests.sh` 已通过（工作区从本轮起点 0d13bbd 起干净）。

### 结果
- LLVM 成功 125 / asm 成功 124；**两者都成功 124**。
- 215 个「都失败」经核实全是负例或环境依赖：`bad/` 213、`pkg/` 7（需包路径）、`ffi/` 3（需 C 库路径）、examples 全过。
- **唯一差异集：`tests/batch3_b6_smoke.dial` —— LLVM 成功、asm 失败。**

### 缺口坐实
```
$ dialc build tests/batch3_b6_smoke.dial --backend=asm -O{0,1,2,3} --no-link
[asm 后端] 数组元素类型 struct Point 暂不支持赋值     # O0/O1/O2/O3 全部失败
$ dialc build tests/batch3_b6_smoke.dial --backend=llvm && ./a.out
B6 len = 2 ... B6 map contains = true                  # LLVM 完整正确运行
```
根因：`src/lower.rs::coerce_elem_bits` 对 `IrType::Aggregate`（struct）走 `_ => Err`，
即 **struct 值无法存入数组元素**。进一步隔离测试显示，连 `var pts: Point[] = [Point{...}]`
（无下标赋值）与 `m.put("a", Point{...})` 也失败——整个「结构体进容器（B6）」在 asm 后端不可用。
该限制由 `3ab9f80`（M2-a）引入，**本轮零改动**，是历史遗留缺口。

### 为何门禁没发现
`tests/run_tests.sh:3078` 的 B6 断言用 `dialc build tests/batch3_b6_smoke.dial`
（**不指定 `--backend`，默认 LLVM**）编译。因此 README 声称的「B6 双后端一致」从未被门禁真正验证。
这正是「只看单维度（功能声明 / 默认后端门禁全绿）会漏掉」的直接例证。

---

## 维度 2：性能 —— ❌ 整体落后（推翻旧结论）

### 测法
`tools/bench_asm_vs_llvm.py`，同一份 `bench/bench.dial`（28 项跨语言等价微基准），
只有 `--backend` 不同，带校验和锚点保证「比的是等价计算」。为对比各自最强发布配置，
两后端都传 `--fast -O3`（LLVM 走 `--fast` O3+LTO 并忽略 -O3；asm 走 O3+fast C 运行时）。
独立跑两次验证稳定性。

### 结果（两次运行高度一致）
| 指标 | 第 1 次 | 第 2 次 |
| --- | --- | --- |
| 几何平均比值（asm/llvm，<1 为 asm 快） | **1.1155x** | **1.1194x** |
| 总净耗时 | 50.43 vs 41.51 ms = 1.215x | 50.63 vs 41.51 ms = 1.220x |
| 逐项胜/平/负 | 6 / 6 / 16 | 6 / 6 / 16 |

### 按类别几何平均
| 类别 | 比值 | 判定 |
| --- | --- | --- |
| 递归调用 | 0.55x | 🟢 asm 大幅领先（尾调用消除，b09 达 0.065x） |
| 位与控制流 | 0.94x | 🟢 略领先 |
| 算术循环 | 1.26x | 🔴 落后（b05 2.11x、b08 1.94x、b12 互递归 4.06x） |
| 字符串 | 1.33x | 🔴 落后（b20 拼接 2.28x） |
| 数组内存 | 1.60x | 🔴 最差（b15 冒泡 2.23x、b17 矩阵乘 2.27x、b14 随机索引 1.59x） |

### 与旧结论的冲突（必须纠正）
`rationale/roadmap-gap-analysis.md` 总纲与 `tools/diag_opt_gaps.py` 结论仍写着
「几何平均 ~0.54x / 领先 LLVM ~46%」。该数字是**不公平对比**的产物（asm O2/O3 vs LLVM 默认 O0，
或被常量折叠项干扰）。公平同配置 A/B 证明：**asm 实际整体慢约 12%**。旧结论必须修正，
否则会持续误导「性能已超过 LLVM」的判断。

---

## 维度 3：优化深度 —— ⚠️ 部分达标

`tools/diag_opt_gaps.py /tmp/r2/bench.s`（asm -O3，4069 行）量化：
- 已有并吃干净：常量折叠 / CSE / DCE / IVR（归纳变量）/ InstCombine / 尾调用 / 常量魔数除法 / fGRA / ISA 微融合 / 过程间折叠。
- 缺失的经典 pass（LICM / 循环展开 / 内联 / SCCP）：在当前基准语料上判**负 ROI**——
  循环多为串行依赖链（无循环不变量、迭代不可并行），小函数各只调一次。
- 落后项（b02/b20/b22）归因：C runtime 开销 + 串行依赖链 + LLVM `--fast` 的 **LTO 整体调度/内联**，
  属架构级差距，非单点 pass 可回收。

诚实解读：asm 的**纯 IR 优化层已接近榨干**，剩余性能差距不在「缺哪个 pass」，而在
LTO 整体内联 + 数组/字符串 C runtime 路径。**这不是「做几个 pass 就能补上」的问题**。

---

## 维度 4：平台与工具链 —— ⚠️ 部分达标

### 达标项
- **工具链去 LLVM 化**（`tools/verify_no_llvm_toolchain.sh`）：20 用例在 gcc-only + 隔离 PATH
  （屏蔽 clang/zig/lld）下 asm 产物与默认工具链逐字节一致——**编译器本身不依赖 LLVM 工具链**，这是「代替」的关键前提，已满足。
- 产物形态三件套（单文件 / 极致体积 / 极致编译速度）、Fail-Fast、FFI/C ABI、x86-64 DWARF 均已落地。

### 缺口项
- **AArch64 只验证「汇编写法合法」，不验证运行语义**：`run_asm_golden.sh:114` 用
  `clang --target=aarch64-linux-gnu` 交叉汇编出 ELF .o 即算通过；x86 宿主无法执行 aarch64 程序，
  **从未实机跑过**。且 AArch64 下内联汇编 Fail-Fast、无 DWARF、PGO 优雅降级（`pgo_effective=false`）。
  即 asm 在 AArch64 上是「能生成代码但能力缩水且未实机验证」的状态。
- 对比 LLVM：LLVM 路径是多平台成熟后端，AArch64 走完整 codegen。asm 在平台覆盖广度与深度上仍有明显差距。

---

## 维度 5：正确性与一致性 —— ✅ 达标

- `tools/run_asm_golden.sh`：**21 个双后端黄金用例**输出 + .s 产物逐字节一致。
- `tools/run_diff_fuzz_gate.sh`：golden 自检 + 4 个固化差分语料 + **150 seeds × O0/O2/O3 = 450 次
  双后端对比，0 不一致**（OK 450 / DIFF 0 / BUILD_FAIL 0）。
- 历史一致性 bug（默认实参补齐、借用绑定 retain、嵌套下标 COW）均已被 diff fuzz 捕获并修复。

**但维度盲区**：fuzz 生成器未覆盖「struct 进容器」这类构造，因此维度 5 的「0 不一致」
不能为维度 1 的缺口背书——fuzz 只保证「两边都能编译的程序输出一致」，不保证「覆盖所有语言构造」。

---

## 阻断项清单（要真正代替 LLVM，必须先解决）

按优先级排序，均为实测确凿项，非猜测：

1. **【功能·硬阻断】struct 进容器（B6）在 asm 后端全优化级别编译失败**。
   位置 `src/lower.rs::coerce_elem_bits` 的 `Aggregate` 分支。需实现 struct 值的位模式装箱/还原
   （对齐 LLVM 的 `kv_from_bits` struct 路径），并把 `batch3_b6_smoke.dial` 纳入 `--backend=asm` 门禁。
2. **【性能·硬阻断】公平对比整体慢约 12%，数组内存类慢 60%**。
   根因是 LTO 整体内联缺失 + 数组/字符串 C runtime 路径开销。这是架构级工程，非单点 pass。
3. **【平台·重要】AArch64 未实机验证运行语义，且内联汇编/DWARF/PGO 降级**。
4. **【文档·必须纠正】roadmap 与诊断工具里「领先 LLVM 46%」的过时结论**，以及
   「语义层 asm 已完全追平 LLVM」的断言（与维度 1 实测矛盾）。

## 复现命令

```bash
cd /workspace/dialc
# 维度1：功能覆盖率（脚本逻辑见正文）
bash /tmp/coverage_probe.sh
# 维度2：性能公平 A/B（本次实测配置）
python3 tools/bench_asm_vs_llvm.py --backend-flags "--fast -O3"
# 维度3：优化缺口
mkdir -p /tmp/r2 && cp bench/bench.dial /tmp/r2/
dialc build /tmp/r2/bench.dial --backend=asm -O3 --no-link -o /tmp/r2/bin
python3 tools/diag_opt_gaps.py /tmp/r2/bench.s
# 维度4：工具链独立性 + AArch64 语法
bash tools/verify_no_llvm_toolchain.sh
bash tools/run_asm_golden.sh ./target/release/dialc
# 维度5：差分一致性
bash tools/run_diff_fuzz_gate.sh
```
