# 更新日志 / Changelog

本文件记录 Dial 语言编译器 `dialc` 的每个发布版本的相对变更。所有版本均遵循
[语义化版本](https://semver.org/lang/zh-CN/) 的精神：不兼容的语义变更属于同主版本内的修订（v0.x 阶段语言尚不稳定，修订号递增即可能含破坏性语义调整）。

**版本号规则**：Dial 采用**十进制递增**（非语义化版本的三段式）——重大更新 / 新引擎 / 大重构 `+0.1`，小功能 / 新语法 `+0.01`，补丁 / 次要改动 `+0.001`，且**禁止一次跳版**（除非用户明确要求）。详见 [README §23](#23-版本号规则)。

---

## [0.61.0] — 2026-10-29

本版落地**文件 IO（file 句柄）**（见 README [§24](#24-文件-iofile-句柄)），并正式确立**版本号规则**（十进制递增、禁止跳版，见 README [§23](#23-版本号规则)）。

### Added（新增）
- **`file` 不透明指针类型**：新增 `file` 类型，IR 中为 `ptr`，封装 C 的 `FILE*`，**不受 ARC 管理**；用户必须显式 `open` / `close`（与内存宪法显式指针回收契约一致）。
- **`open` / `read` / `write` / `close`**：仅封装 C 标准库 `fopen` / `fread` / `fwrite` / `fclose`，Rust 前端不重写任何文件逻辑；只提供最基础的 C 风格 API，无链式流、无额外语法糖。
- **C 侧实现**：`stdlib/dial_io.c` 新增 `dial_file_open` / `dial_file_read` / `dial_file_write` / `dial_file_close`；`stdlib/dial_string.c` 新增 `dial_string_cstr` / `dial_string_from_bytes`（供 `read` 从字节缓冲区构造字符串）。
- **示例与负向用例**：`examples/file_demo.dial`（写后读回并打印，黄金输出 `dialc-file-io`）；`tests/bad/io_bad_arg.dial`（open 参数类型错误须在编译期拒绝）。

### Changed（变更）
- 遵循版本号规则：本版属「小功能 / 新语法」，`+0.01`，版本号 `0.6.0 → 0.61.0`（Cargo `version` 写作 `0.61.0`，语义等同 `0.61`）。
- README 同步：§4.3 关键字、§5.1 基本类型、§15 FFI 表与 C 导出函数、§17.2（从「未实现」移除文件 IO）、新增 [§24 文件 IO](#24-文件-iofile-句柄) 与 [§22 版本变更摘要](#22-版本变更摘要)。

### 设计约束（用户拍板）
- **绝不引入全局变量**：文件句柄显式传递。
- **底层走 C 标准库**：直接封装 `FILE*`，不在 Rust 前端重写文件系统逻辑。
- **错误处理（无异常）**：打开失败返回空指针、读失败返回空串、写失败返回 `-1`；不引入 `Result` 封装。
- **不搞花哨语法**：只做 `open` / `read` / `write` / `close` 基础 API，无链式文件读取流。

---

## [0.6.0] — 2026-10-29

本版把 Zig 式 **Fail-Fast** 确立为与内存宪法并列的语言底线（见 README §20）：
**宁可当场崩溃，绝不产生未定义行为或静默错误数据。**

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

### Added（0.6.0 补充）

- **`--fast` 危险警告**：编译器在使用 `--fast`（ReleaseFast）时**当场打印醒目警告**，
  说明该模式会关闭所有边界检查、除零检查和空指针保护，越界将退化为 C 的 UB。
  文档侧同步写入 README §3.2 与 §20.4，并出现在官网首页卡片。
  安全性是默认值，选择放弃它的人必须当场被告知代价。

### 实测对照（同一份越界代码，三档行为）

| 等级 | `a[99]`（长度 3） | `100 / 0` |
| --- | --- | --- |
| Debug（默认） | SIGTRAP(5)，`int3` 当场终止 | SIGTRAP(5) 当场终止 |
| ReleaseSafe | SIGILL(4)，`ud2` 当场终止 | SIGILL(4) 当场终止 |
| ReleaseFast | 读出 `0` 并继续执行，打印哨兵 | 打印 `0` 并继续执行（**静默错误结果**） |

---

## [0.5.0] — 2026-10-29

本版补齐循环控制流，并把**代码编写纪律**正式写入规范（见 README §20）。

### Added（新增）
- **`break`**：跳出最内层循环；仅允许出现在 `while` / `for` 循环体内，写在别处是编译错误（见 README §8.4）。
- **`continue`**：进入下一次迭代。关键语义——`for` 的 `continue` **先执行 `i = i + 1` 再回到条件判定**：
  实现上把自增放在独立的 `for.inc` 块，`continue` 跳到该块而非条件块；若跳到条件块就会跳过自增造成死循环。
- 新增示例 `examples/loop_control.dial`（覆盖 for+continue、for+break、while+continue/break 三种组合）。
- 新增负向用例 `tests/bad/break_outside_loop.dial`、`tests/bad/continue_outside_loop.dial`。

### Docs（文档）
- README 新增 [§20 代码编写纪律（防臃肿与允许冗余原则）](#20-代码编写纪律防臃肿与允许冗余原则)：
  ① 绝对拒绝过度抽象（宁可 WET，绝不 DRY 过头）② 不引入无用依赖 ③ 允许安全与性能的冗余
  ④ 保持代码直白（能用一个循环和一个 if 解决的，绝不套设计模式）。
- README §8.1 语句一览、§8.4 `break` / `continue`、§17.2 未实现清单（移除已落地的 break/continue）同步更新。
- 新增 `REPORT_v0.5.md`（`objdump` 真实反汇编证据）与 `TEST_RUN_v0.5.log`。
- 官网 `web/` 重新生成，首页版本同步 v0.5。
- 版本号升级至 `0.5.0`。

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
- 多维数组、非 `int` 元素数组、`char` 类型。
- 裸块 `{ ... }`。
- 模块 / 包 / import、泛型、接口、trait、闭包。
- 指针、地址取值、指针算术（永不提供）；运算符重载（永不提供）；全局变量。
