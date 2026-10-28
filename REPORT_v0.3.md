# Dial 语言 v0.3 — 自测与真实汇编验证报告

> 本报告附带于 `dialc-v0.3.zip`。所有汇编均来自 `objdump -d` 对 **真实编译产物（`.o` / 可执行文件）** 的反汇编，
> 不是示意代码。编译器：`cargo build --release`（inkwell 0.11 / LLVM 18，`OptimizationLevel::Default` ≈ -O2）。
> 工具链：`zig cc` 链接，`objdump` 反汇编，`ldd` 校验依赖。

---

## 0. 本次修正的三处设计点

| # | 修正点 | v0.2 | v0.3 | 动机 |
| --- | --- | --- | --- | --- |
| 1 | `and` / `or` 求值 | 两侧都求值，不短路 | **短路求值**（同 C 的 `&&` / `\|\|`） | 现代语言常识；不短路会让"判空后访问"类代码在左操作数为假时崩溃 |
| 2 | `int` ↔ `float` 转换 | 曾允许 `int→float` 隐式提升 | **零隐式转换铁律**：混用即编译错误，必须写 `int_to_float` / `float_to_int` | 杜绝静默精度丢失，比 C 更严格 |
| 3 | `let` 变量调 `push` / `set` | 未定义 | **编译期拒绝**（`let` 不可重新赋值，无法接收 COW 换回的新指针）；接收者须为顶层 `var` | 把"容器被原地修改但接收者无法接收新指针"的悬垂 Bug 提前到编译期 |

架构契约（不透明指针、两步构建、产物零 LLVM 依赖、C 承担一切容器）**未改动**。

---

## 1. 端到端自测结果（`./tests/run_tests.sh`）

三组检查，全部通过：

- **A. 正向**：`examples/*.dial` 编译成功且运行输出与同名 `.expected` 黄金文件逐字节一致 —— **10/10 通过**。
- **B. 负向**：`tests/bad/*.dial` 必须编译失败，用于锁定诊断信息不被弱化 —— **18/18 通过**。
- **C. 契约**：产物可执行文件不得依赖 libLLVM，仅依赖 C Runtime（libc）—— **通过**。

关键负向用例（验证三处修正的报错落点）：

| 用例 | 期望诊断 | 实际结果 |
| --- | --- | --- |
| `let_push.dial` | `let` 变量调用 `push` 必须拒绝 | ✅ `语义错误：变量 'a' 由 let 声明，不可调用原地修改方法 'push'（容器可能被修改并更换指针，必须是 var 变量）` |
| `implicit_conv.dial` | `int`→`float` 隐式转换必须拒绝 | ✅ `语义错误：变量 'y' 的初始化表达式类型为 int，与声明类型 float 不兼容` |
| `mixed_arith.dial` | `1 + 2.0` 混合算术必须拒绝 | ✅ `语义错误：算术运算(+ - * /)要求两个操作数类型完全相同且均为数值类型（int 或 float）；int 与 float 之间无隐式转换，请用 int_to_float / float_to_int 显式转换` |

---

## 2. `and` / `or` 短路求值 —— 真实汇编证明

源文件：`examples/short_circuit.dial`。`main` 含三段：

- 例1 `left_false() and right_true()`：左操作数为假，**应跳过右操作数**。
- 例2 `right_true() or right_false()`：左操作数为真，**应跳过右操作数**。
- 例3 `right_true() and right_false()`：左操作数为真，**必须求值右操作数**。

编译器用"entry 块结果栈槽 + 三条基本块（eval / short / merge）+ 条件分支"实现短路，规避 inkwell 0.11 的 Phi 类型限制。

```asm
0000000000000060 <main>:
  60: push   %rax
;; ===== 例1: left_false() and right_true() =====
  61: call   66 <main+0x6>      ; left_false() -> %al
  66: test   $0x1,%al           ; 取左操作数
  68: je     77 <main+0x17>     ; ← 左为假(0) → 跳到 short 分支(存 0)，【跳过右操作数的 call】
  6a: call   6f <main+0xf>      ; right_true()（仅左为真时才执行）
  6f: and    $0x1,%al
  71: mov    %al,0x2(%rsp)      ; eval 分支：写右操作数结果
  75: jmp    7c <main+0x1c>     ; 跳 merge
  77: movb   $0x0,0x2(%rsp)     ; short 分支：直接存常量 FALSE(0)
  7c: movzbl 0x2(%rsp),%edi     ; merge：取出结果
  ...
;; ===== 例2: right_true() or right_false() =====
  8b: call   90 <main+0x30>     ; right_true() -> %al
  90: test   $0x1,%al
  92: je     9b <main+0x3b>     ; ← 左为假 → 跳 eval 分支(去求右操作数)
  94: movb   $0x1,0x3(%rsp)     ; 左为真 → 直接存常量 TRUE(1)，【跳过右操作数】
  99: jmp    a6 <main+0x46>
  9b: call   a0 <main+0x40>     ; right_false()（仅左为假时才执行）
  ...
;; ===== 例3: right_true() and right_false() =====
  b5: call   ba <main+0x5a>     ; right_true() -> %al
  ba: test   $0x1,%al
  bc: je     cb <main+0x6b>     ; 左为假 → 跳 short(存 0)
  be: call   c3 <main+0x63>     ; 左为真 → 必须求右操作数 right_false()
  ...
  df: xor    %eax,%eax          ; return 0
  e2: ret
```

**读法**：每组 `and`/`or` 都有一条 `je <short>` 条件跳转——当左操作数已能决定结果时，直接跳到"写常量结果"的分支，**右操作数的 `call` 被整段跳过**。这恰恰是 C `&&`/`||` 的短路语义：被短路的分支其副作用（此处的 `print`）不会发生。运行期输出 `L=0 / false / R=1 / true / R=1 / R=0 / false` 也印证：例1 只打印 `L=0`、例2 只打印一个 `R=1`，右操作数的副作用均被抑制。

---

## 3. `int_to_float` / `float_to_int` —— 真实汇编证明

### 3.1 常量场景（源 `examples/conversions.dial`）：编译期内联 + 常量折叠

因为两个操作数都是字面量，编译器把 `int_to_float(10)+2.5` 等在**编译期直接算出常量**，`int_to_float`/`float_to_int` 被内联且折叠，连对应的机器转换指令都不需要——这是"纯编译期内联、不经任何 C 运行时函数"的直接证据：

```asm
0000000000000000 <main>:
   0: sub    $0x28,%rsp
   4: movq   $0xa,(%rsp)          ; let i = 10
  c: movabs $0x4029000000000000,%rax   ; 0x4029... = 双精度 12.5 = int_to_float(10)+2.5 的【编译期结果】
  16: mov    %rax,0x8(%rsp)       ; let f = 12.5
  ...
  28: movq   $0x3,0x10(%rsp)      ; let j = float_to_int(3.7) = 3（编译期截断）
  ...
  3b: movabs $0x4039800000000000,%rax   ; 0x4039... = 双精度 25.5 = int_to_float(5)*5.1 的【编译期结果】
  45: mov    %rax,0x18(%rsp)      ; let k = 25.5
  ...
  57: movq   $0xa,0x20(%rsp)      ; let m = float_to_int(10.9)/1 = 10
```

注意：反汇编中**没有任何 `dial_*` 调用**——转换完全发生在编译期，零运行时开销。

### 3.2 运行时场景：真实 `sitofp` / `fptosi` 指令

但当操作数来自内存（如 `dial_array_get` 返回 `i64`，优化器无法折叠），转换必须落地为真实机器指令。下面用"数组元素"作为不可折叠操作数，迫使 `int_to_float`/`float_to_int` 真正生成指令。`objdump` 显示：

```asm
;; int_to_float(x: int): float  ---- 编译为 LLVM sitofp ----
0000000000000000 <to_f>:
   0: mov    %rdi,-0x8(%rsp)
   5: cvtsi2sd %rdi,%xmm0         ; ← Signed Int TO Scalar Double = sitofp（int_to_float）
   a: ret

;; float_to_int(y: float): int  ---- 编译为 LLVM fptosi ----
0000000000000010 <to_i>:
  10: movsd  %xmm0,-0x8(%rsp)
  16: cvttsd2si %xmm0,%rax        ; ← Float TO Truncating Signed Int = fptosi（float_to_int，向零截断）
  1b: ret
```

- `cvtsi2sd` 正是 LLVM `sitofp` 的 x86-64 落地指令 —— `int_to_float` 的全部实现。
- `cvttsd2si` 正是 LLVM `fptosi` 的 x86-64 落地指令（带 `tt` = truncate toward zero，与文档声明的"向零截断，与 C 的 `(int)` 一致"吻合）。
- 这两条指令**不调用任何 C 运行时函数**，证明转换"纯编译期内联"。

---

## 4. 产物契约：零 LLVM 依赖（组 C 硬断言）

```text
$ ldd examples/hello
    linux-vdso.so.1
    libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6
    /lib64/ld-linux-x86-64.so.2
```

用户编译出的可执行文件**仅依赖 C Runtime（libc）**，不链接 libLLVM。`run_tests.sh` 的组 C 以 `grep -qi LLVM` 作为硬断言，本次通过。

---

## 5. 结论

v0.3 三处修正均已落地并验证：

1. `and`/`or` 短路求值 —— 真实汇编中可见 `je <short>` 跳过被短路操作数的 `call`（§2）。
2. 零隐式转换铁律 —— 混合算术/比较一律编译错误；`int_to_float`/`float_to_int` 编译为 `cvtsi2sd`/`cvttsd2si`（§3），纯内联、零 C 运行时。
3. `let` 变量调 `push`/`set` 编译期拒绝；接收者须为顶层 `var`（§1 负向用例）。

`cargo build --release` 零错误零警告；端到端自测 10 正向 + 18 负向 + 1 契约检查全部通过。
