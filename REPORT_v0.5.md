# dialc v0.5 验证报告：break / continue 的真实汇编与端到端自测

本报告所有反汇编片段均来自 `objdump -d` 对**实际链接产物**的反汇编，非人工编写、非伪代码。
v0.4 的证据（`map` / `for` / `else if`）见 `REPORT_v0.4.md`。

| 项目 | 值 |
| --- | --- |
| 编译器版本 | `0.5.0`（`Cargo.toml`） |
| 构建 | `cargo build --release`（Rust + inkwell 0.11，LLVM 18） |
| 链接 | `zig cc` 归档 `libdial_std.a` 后与用户 `.o` 链接 |
| 测试日志 | `TEST_RUN_v0.5.log`（37 项 ✅，0 项 ❌） |
| 平台 | x86-64, Linux |

本轮新增：**`break` / `continue`**，并正式写入[代码编写纪律](README.md#20-代码编写纪律防臃肿与允许冗余原则)。

---

## 1. `for` + `continue`：关键正确性点（不跳过自增）

这是本次实现**最容易出错**的地方：若 `continue` 直接跳回条件块，就会跳过 `i = i + 1` 造成**死循环**。
实现上把自增放在独立的 `for.inc` 块，`continue` 跳到该块而非条件块。

源码（`examples/loop_control.dial`）：

```dial
var sum: int = 0;
for (i in 0..6) {
    if (i == 3) {
        continue;      // 跳过 3
    }
    sum = sum + i;
}
```

反汇编（`main` 内）：

```
 1001c11:	48 c7 44 24 10 00 00 	movq   $0x0,0x10(%rsp)      # i = 0
 1001c1a:	48 83 7c 24 10 05    	cmpq   $0x5,0x10(%rsp)      # 条件块：i <= 5（即 i < 6）
 1001c20:	7e 1b                	jle    1001c3d <main+0x4d>  #   -> 进入循环体
 1001c22:	eb 38                	jmp    1001c5c <main+0x6c>  #   -> 否则跳出（for.end）
 1001c30:	48 ff 44 24 10       	incq   0x10(%rsp)           # ★ for.inc 块：i = i + 1
 1001c35:	48 83 7c 24 10 05    	cmpq   $0x5,0x10(%rsp)
 1001c3b:	7f 1f                	jg     1001c5c <main+0x6c>  #   -> 超出则跳出
 1001c3d:	48 83 7c 24 10 03    	cmpq   $0x3,0x10(%rsp)      # 循环体：if (i == 3)
 1001c43:	74 eb                	je     1001c30 <main+0x40>  # ★★ continue -> 跳到 1001c30
 1001c45:	48 8b 44 24 10       	mov    0x10(%rsp),%rax      #    sum += i
 1001c4a:	48 01 44 24 20       	add    %rax,0x20(%rsp)
 1001c4f:	48 ff 44 24 10       	incq   0x10(%rsp)           #    i = i + 1（自然结束路径）
 1001c54:	48 83 7c 24 10 05    	cmpq   $0x5,0x10(%rsp)
 1001c5a:	7e e1                	jle    1001c3d <main+0x4d>  #    回到循环体
 1001c5c:	48 8b 7c 24 20       	mov    0x20(%rsp),%rdi       # for.end
 1001c61:	e8 4a 03 00 00       	call   1001fb0 <dial_print_int>
```

**判读要点（关键）**

- `1001c43` 处 `je 1001c30`：`continue` 的跳转目标是 **`1001c30`，即 `incq 0x10(%rsp)`（自增指令）**，
  而**不是**条件块 `1001c1a`。这从机器码层面证明 **`continue` 仍然执行 `i = i + 1`**——不会死循环。
- 自增块 `for.inc`（`1001c30`）与条件判定是分离的两块，正是 README §8.4 描述的设计。
- 运行时验证：程序在 **0.001 秒**内结束并输出 `12`（= 0+1+2+4+5，跳过了 3）。
  若 `continue` 跳错目标，这里会挂死，`timeout 10` 必然触发——实测未触发。

---

## 2. `for` + `break`：直接跳到循环出口

源码：

```dial
var total: int = 0;
for (j in 0..100) {
    if (j == 4) {
        break;
    }
    total = total + j;
}
```

反汇编：

```
 1001c83:	48 c7 44 24 18 00 00 	movq   $0x0,0x18(%rsp)      # j = 0
 1001c8c:	48 83 7c 24 18 63    	cmpq   $0x63,0x18(%rsp)     # 条件：j <= 99（即 j < 100）
 1001c92:	7f 2e                	jg     1001cc2 <main+0xd2>  #   -> 跳出（for.end）
 1001ca0:	48 83 7c 24 18 04    	cmpq   $0x4,0x18(%rsp)      # 循环体：if (j == 4)
 1001ca6:	74 1a                	je     1001cc2 <main+0xd2>  # ★ break -> 直接跳到 1001cc2
 1001ca8:	48 8b 44 24 18       	mov    0x18(%rsp),%rax      #    total += j
 1001cad:	48 01 44 24 28       	add    %rax,0x28(%rsp)
 1001cb2:	48 ff c0             	inc    %rax                  #    j = j + 1
 1001cb5:	48 89 44 24 18       	mov    %rax,0x18(%rsp)
 1001cba:	48 83 7c 24 18 63    	cmpq   $0x63,0x18(%rsp)
 1001cc0:	7e de                	jle    1001ca0 <main+0xb0>  #    回到循环体
 1001cc2:	48 8b 7c 24 28       	mov    0x28(%rsp),%rdi       # for.end
 1001cc7:	e8 e4 02 00 00       	call   1001fb0 <dial_print_int>
```

**判读要点**

- `1001ca6` 处 `je 1001cc2`：`break` 直接跳到循环出口 `1001cc2`（`for.end`），
  **绕过了自增块**——这正是 `break` 应有的语义（彻底离开循环，不再自增）。
- 实测输出 `6`（= 0+1+2+3），与"到 j==4 跳出"一致。

---

## 3. `while` + `continue` / `break`

源码（自增写在循环体**开头**，因此 `continue` 不会漏掉自增）：

```dial
var n: int = 0;
var acc: int = 0;
while (n < 100) {
    n = n + 1;
    if (n == 2) { continue; }
    if (n > 5) { break; }
    acc = acc + n;
}
```

- `while` 的 `continue` 跳回**条件块**（没有 for 那样的独立自增块，因为 while 没有隐式自增）。
- 这也是为什么 README §8.4 强调：while 里用 `continue` 时**自增必须写在 `continue` 之前**。
- 实测输出 `13`（= 1+3+4+5，跳过 2、在 n>5 时跳出），行为正确。

---

## 4. 编译期拒绝循环外的 `break` / `continue`

语义层用 `in_loop` 标志逐层下传（If 透传、While/For 置 true），写在循环外一律报错：

| 用例 | 诊断 |
| --- | --- |
| `tests/bad/break_outside_loop.dial` | `[dialc 错误] 第 4 行 第 9 列: 语义错误：break 只能出现在循环（while / for）体内` |
| `tests/bad/continue_outside_loop.dial` | `[dialc 错误] 第 7 行 第 5 列: 语义错误：continue 只能出现在循环（while / for）体内` |

---

## 5. 代码编写纪律（本轮写入 README §20）

本次实现严格遵循新写入的四条纪律，可对照检查：

| 纪律 | 本次落地情况 |
| --- | --- |
| ① 拒绝过度抽象（宁可 WET） | `break` / `continue` 各有独立的一条 match 分支，未抽成"跳转指令工厂"；`for` 与 `while` 的循环结构各自直白生成，不共享抽象层 |
| ② 不引入无用依赖 | 未新增任何 Crate 或 C 库。`Cargo.toml` 仍只有 `inkwell`；C Runtime 仍只用 `<stdlib.h>` / `<string.h>` / `<stdio.h>` |
| ③ 允许安全与性能冗余 | `continue` / `break` 后创建 `after.cont` / `after.brk` 兜底块，防止往已终结的块里追加指令（跨块边界的防御性冗余） |
| ④ 保持代码直白 | 循环目标就是一个 `(continue_bb, break_bb)` 二元组栈，压栈/弹栈——没有 visitor、没有设计模式套壳 |

---

## 6. 端到端自测结果

完整日志见 `TEST_RUN_v0.5.log`。摘要：

| 组 | 内容 | 结果 |
| --- | --- | --- |
| A | 正向：13 个示例编译 + 运行 + 与 `.expected` 黄金文件逐字节比对 | ✅ 13/13 一致 |
| B | 负向：22 个用例必须编译失败（锁定诊断不被弱化） | ✅ 22/22 正确报错 |
| C | 契约：产物可执行文件不得依赖 libLLVM | ✅ 通过 |
| **合计** | | **✅ 37 项通过，0 项失败** |

本轮新增用例：

| 类型 | 文件 | 覆盖点 |
| --- | --- | --- |
| 正向 | `examples/loop_control.dial` | for+continue、for+break、while+continue/break |
| 负向 | `tests/bad/break_outside_loop.dial` | 循环外 `break` 被拒 |
| 负向 | `tests/bad/continue_outside_loop.dial` | 循环外 `continue` 被拒 |

### 产物依赖契约（组 C 实测）

```
linux-vdso.so.1
libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6
/lib64/ld-linux-x86-64.so.2
```

**只依赖 C Runtime（libc）**，不含 `libLLVM` —— 两步构建契约仍然成立。

---

## 7. 结论

1. `for` 的 `continue` 跳转到**自增块**而非条件块，机器码 `je 1001c30`（指向 `incq`）可直接验证；
   实测 0.001s 内正常结束，排除死循环。
2. `break` 直接跳到循环出口 `for.end`，绕过自增，语义正确。
3. `while` 的 `continue` 回到条件块，因此要求自增写在 `continue` 之前（已写入文档）。
4. 循环外的 `break` / `continue` 在**编译期**被拒绝，不会漏到运行期。
5. 37 项端到端检查全绿，产物仍**零 LLVM 依赖**。
6. 实现过程符合新写入的代码编写纪律（拒绝过度抽象、零新增依赖、允许边界冗余、保持直白）。
