# 更新日志 / Changelog

本文件记录 Dial 语言编译器 `dialc` 的每个发布版本的相对变更。所有版本均遵循
[语义化版本](https://semver.org/lang/zh-CN/) 的精神：不兼容的语义变更属于同主版本内的修订（v0.x 阶段语言尚不稳定，修订号递增即可能含破坏性语义调整）。

---

## [0.4.0] — 2026-11-02

本版落地**部分标准库**与**控制流语法糖**，并正式写入[内存宪法](#内存宪法防-gc-铁律)（见 README §19）。

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
  这消除了旧实现与[内存宪法](#内存宪法防-gc-铁律)（防 GC 铁律）的矛盾。
- Map 与 `read_line` 从「未实现」清单移除（v0.4 已暴露）。

### Fixed（修复）
- 修正 `0..5` 这类范围字面量此前被数字词法器误识别为浮点 `0.5` 的问题：仅当小数点后紧跟数字才视为浮点，
  `..` 现被正确识别为范围运算符。

### Docs（文档）
- README 新增 [§12 哈希表 `map`](#12-哈希表-mapint--int)、[§8.2 `else if`](#82-if--else-if--while)、[§8.3 范围 `for`](#83-范围-for-循环)、
  [§19 内存宪法](#19-内存宪法防-gc-铁律)；并将 FFI 表与 C 导出函数清单补齐 `dial_map_*` / `dial_read_line`。
- 官网文档（`web/`）随 README / CHANGELOG 重新生成。
- 版本号升级至 `0.4.0`。

---

## [0.3.0] — 2026-10-28

本版只修改**语义与诊断**，未改变任何架构契约（不透明指针、两步构建、产物零 LLVM 依赖、C 承担一切容器）。

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

### Docs（文档）
- README 同步更新：§5.3 零隐式转换铁律、§6.2 可变性判定（含 `push`/`set`）、§7.2 类型规则、
  §7.3 语义细则表（修正 `and`/`or` 与混合算术两行）、§11.3–§11.4 接收者须为 `var`、§18 v0.3 变更摘要。
- 新增 `REPORT_v0.3.md`：基于 `objdump -d` 真实反汇编的短路求值与 `int_to_float`/`float_to_int` 落地证据，
  以及端到端自测结果与产物零 LLVM 依赖契约校验。

---

## [0.2.0] — 基线版本（本仓库首个规范化基线）

> 注：本仓库的历史从 v0.3.0 的源码状态起记。`0.2.0` 及以下版本的特性以下列条目作为基线描述，
> 具体提交不在本仓库历史中。

- 编译器前端（Rust + inkwell）把 Dial AST 翻译为 LLVM IR；IR 全程在内存中构建并直接 `write_to_file` 写出 `.o`。
- 两步构建：`zig cc` 把 `stdlib/` 与 `runtime/` 编译归档为 `libdial_std.a`，再与用户 `.o` 链接。
- 不透明指针契约：`string` / `int[]` 在 IR 中只是 `ptr`，用户代码无法触碰其内存布局。
- 引用计数 + COW 所有权模型，Sema 阶段做数据流/逃逸分析以静态消除运行时开销。
- 基本类型 `int` / `float` / `bool` / `string` / `int[]` / 结构体（值类型）。
- `and` / `or` 在 v0.2 中为**非短路**（旧行为，v0.3 已改为短路）；`int→float` 在 v0.2 中存在隐式提升（v0.3 已废除）。

---

## 未实现（-Unreleased-，v0.2/v0.3 语言层均未暴露，v0.4 起部分已落地）
- 文件 IO、多维数组、非 `int` 元素数组、`char` 类型。
- `break` / `continue`（v0.5 计划）、裸块 `{ ... }`。
- 模块 / 包 / import、泛型、接口、trait、闭包。
- 指针、地址取值、指针算术（永不提供）；运算符重载（永不提供）；全局变量。
