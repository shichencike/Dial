# asm 后端能否真正代替 LLVM：多维度审计（2026-11-19，实测版）

> 本文回答一个此前一直被「单维度乐观结论」掩盖的问题：**自研 asm 后端现在到底能不能
> 真正代替 LLVM？** 结论不预设立场，全部由当机实测数据说话。审计范围覆盖五个独立维度，
> 任一维度不达标即不能宣称「已代替」。所有命令可复现。

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
