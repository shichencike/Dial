# dialc v0.6 报告 —— Fail-Fast 铁律落地

> **宁可当场段错误（Segfault / Illegal Instruction），也绝对不允许产生未定义行为（UB）或静默产生错误数据。**

本报告的全部机器码均来自 `objdump -d` 对真实可执行文件的反汇编，非伪代码、非示意。

编译环境：`cargo build --release`（Rust + inkwell 0.11 / LLVM 18），链接 `zig cc`，目标 x86-64。

---

## 1. 要解决的问题：C 的静默损坏

改造前，`dial_array_get` / `dial_array_set` / `dial_string_get` 的实现分别是：

```c
int64_t dial_array_get(DialArray a, int64_t i) { return a_elems(a)[i]; }              // 无检查
DialString dial_string_get(DialString s, int64_t i) { return s_data(s)[i]; }          // 无检查
```

越界时它们会把相邻内存当成合法数据返回/覆盖，程序继续运行——Bug 在几万行之后
才以完全无关的形态爆发。这就是本版要消灭的东西。

---

## 2. 落地：三层防线

| 层 | 位置 | 触发方式 | 机器码 |
| --- | --- | --- | --- |
| **① Codegen 静态折叠** | LLVM IR 生成 | 下标/除数是编译期常量时，直接证明必然越界 → 无条件陷阱 | `int3` / `ud2` |
| **② Codegen 运行时检查** | LLVM IR 生成 | 长度运行时才知 → 插入比较 + 条件分支到陷阱块 | `cmp` + `jbe` → `int3` / `ud2` |
| **③ C Runtime 断言** | `runtime/dial_rt_trap.c` | 字符串 / Map / 引用计数层面的越界与内部损坏 | `test`/`cmp` → `call dial_bounds_fail` → `ud2` |

第 ③ 层是兜底：即便 codegen 的检查被 `--fast` 关闭，Runtime 仍不放过越界。

---

## 3. 真实反汇编证据

### 3.1 数组越界读 —— 运行时比较（Debug，默认等级）

源码 `tests/trap/array_oob_read.dial`：`var a: int[] = [10, 20, 30]; print(a[99]);`

```asm
1001f76: e8 a5 01 00 00    call   dial_array_length     # 取运行时长度 -> %rax
1001f7b: 48 83 f8 63       cmp    $0x63,%rax            # 0x63 = 99，与 length 比较
1001f7f: 76 31             jbe    1001fb2               # ★ 99 >= len 则跳转
1001f81: be 63 00 00 00    mov    $0x63,%esi
1001f89: e8 e2 00 00 00    call   dial_array_get        # 合法才真正访问
1001f8e: 48 89 c7          mov    %rax,%rdi
1001f91: e8 5a 05 00 00    call   dial_print_int
1001f96: bf 0f 27 00 00    mov    $0x270f,%edi          # 哨兵 9999（不可达）
...
1001fb2: cc                int3                         # ★★ llvm.debugtrap：当场终止
```

`jbe` 的目标 `1001fb2` 就是 `int3`——陷阱是跳转目标，不是对齐填充。

### 3.2 负下标 —— 编译期静态证明（Debug）

源码：`var i: int = 0 - 1; print(a[i]);`

```asm
1001f38: 48 c7 44 24 08 ff ff ff ff   movq $0xffffffffffffffff,0x8(%rsp)   # i = -1（补码）
1001f44: e8 a7 01 00 00               call dial_array_length
1001f49: b0 01                        mov  $0x1,%al      # ★ 常量折叠：必然越界 -> true
1001f4b: 84 c0                        test %al,%al
1001f4d: 75 32                        jne  1001f81       # ★★ 跳进陷阱块
```

用**一次无符号比较**同时判定 `idx < 0` 与 `idx >= len`：负数 idx 的位模式是巨大无符号数，
必然 `>= len`。这里 LLVM 进一步在编译期就证明了越界，直接折叠成无条件跳转。

### 3.3 整数除零 —— 编译期静态证明（Debug）

源码：`var y: int = 0; print(x / y);`

```asm
1001d74: movq $0x64,0x8(%rsp)      # x = 100
1001d7d: movq $0x0,0x10(%rsp)      # y = 0
1001d86: b0 01                     mov  $0x1,%al        # ★ 常量折叠：必然除零 -> true
1001d88: 84 c0                     test %al,%al
1001d8a: 75 20                     jne  1001dac         # ★★ 跳进陷阱块
1001d96: call dial_print_int       # 正常路径（不可达）
1001dac: cc                        int3                 # ★★★ llvm.debugtrap
```

**为什么除零必须检查**：LLVM 的 `sdiv` 在除数为 0 时结果是 **poison**（未定义行为），
优化器有权据此认为该路径不可达、把后续代码整段删掉。实测关闭检查后 `100 / 0`
**打印 `0` 并继续执行**——程序没崩，却给出了完全错误的结果。这比崩溃危险得多。

### 3.4 C Runtime 侧边界检查（`dial_string_get`）

字符串下标只有 C 侧的检查，用来单独验证第 ③ 层：

```asm
1001e30: push  %r14
1001e3a: mov   (%rdi),%rsi              # len = s[0]
1001e3d: test  %rbx,%rbx                # idx < 0 ?
1001e40: js    1001e47                  #   是 -> fail
1001e42: cmp   %rbx,%rsi                # len > idx ?
1001e45: jg    1001e4f                  #   是 -> 合法路径
1001e47: mov   %rbx,%rdi
1001e4a: e8 a1 02 00 00  call dial_bounds_fail   # ★ 打印诊断后 ud2
1001e4f: movzbl 0x8(%r14,%rbx,1),%eax   # 正常读取
1001e5c: ret
```

实际运行时 stderr 输出：

```
dial: 下标越界（Fail-Fast 终止）: index = 99, length = 3
```

诊断走 `write(2)` 直写 fd 2（不用 stdio——进程马上就要死，stdio 缓冲来不及 flush），
并且在陷阱前先 `fflush(NULL)`，否则崩溃前已 print 的内容会随之消失，让人误判崩溃位置。

### 3.5 安全等级切换 —— 同一份代码的三份产物

| 等级 | 越界处的机器码 | 进程结果 |
| --- | --- | --- |
| **Debug**（默认） | `cmp $0x63` + `jbe` → `int3` | SIGTRAP(5)，退出码 133 |
| **ReleaseSafe**（`--release`） | `cmp $0x63` + `jbe` → `ud2` | SIGILL(4)，退出码 132 |
| **ReleaseFast**（`--fast`） | 无 `cmp`、无跳转、无陷阱 | 继续执行，打印哨兵 9999 |

ReleaseSafe 的跳转目标：

```asm
1001e8b: 48 83 f8 63    cmp  $0x63,%rax
1001e8f: 76 31          jbe  1001ec2 <main+0xb2>
...
1001ec2: 0f 0b          ud2                    # ★ llvm.trap
```

ReleaseFast 的 `main`（对比：干净得像手写 C）：

```asm
1001e14: call dial_array_length    # 仅来自源码里显式的 a.length()
1001e2a: call dial_array_get       # a[1]
1001e40: call dial_array_get       # a[99] —— 直接访问，无任何检查
```

等级是**编译期常量**（`-DDIAL_SAFETY=N`），ReleaseFast 下检查代码被编译器完全消除，零运行时开销。
等级同时作用于 LLVM IR 与 C Runtime，两侧口径一致。

---

## 4. 运行时行为对照（实测）

| 用例 | Debug | ReleaseSafe | ReleaseFast |
| --- | --- | --- | --- |
| `a[99]`（长度 3）越界读 | 信号 5，`int3` | 信号 4，`ud2` | 读出 `0`，继续跑，打印哨兵 |
| `a[5] = 777` 越界写 | 信号 5 | 信号 4 | 写入相邻内存，继续跑 |
| `a[-1]` 负下标 | 信号 5 | 信号 4 | 读取对象头前内存，继续跑 |
| `100 / 0` 除零 | 信号 5 | 信号 4 | **打印 `0`，继续跑** |
| `s.get(99)` 字符串越界 | 信号 4（C 侧 `ud2`）+ 诊断 | 信号 4 + 诊断 | 返回垃圾字节，继续跑 |

ReleaseFast 那一列正是 C 的行为——**这就是我们要消灭的东西，而现在它是需要显式选择才有的**。

---

## 5. 测试

```bash
./tests/run_tests.sh
```

**47 项全绿，0 失败**（详见 `TEST_RUN_v0.6.log`）。

新增 D / E 两组：

- **D. Fail-Fast**：`tests/trap/*.dial` 必须**当场因信号终止**（退出码 ≥ 128），
  且**不得打印哨兵 9999**——证明它在越界那一行就停了，而不是带病跑完。
- **E. 对照组**：同一批用例在 `--fast` 下应当关闭检查并跑完。
  这组存在的意义是**证明陷阱确由编译器插入**，而不是"碰巧段错误"。

```
== D) Fail-Fast：越界/除零必须当场终止（默认 Debug 等级）==
  ✅ array_negative_index 信号 5 终止
  ✅ array_oob_read 信号 5 终止
  ✅ array_oob_write 信号 5 终止
  ✅ div_by_zero 信号 5 终止
  ✅ string_oob_get 信号 4 终止（诊断: dial: 下标越界（Fail-Fast 终止）: index = 99, length = 3）

== E) 对照组：--fast 关闭检查后，同一份代码应恢复 C 的“静默”行为 ==
  ✅ array_negative_index (--fast) 检查已关闭，产生未定义行为（符合预期）
  ✅ array_oob_read (--fast) 检查已关闭，产生未定义行为（符合预期）
  ✅ array_oob_write (--fast) 检查已关闭，产生未定义行为（符合预期）
  ✅ div_by_zero (--fast) 检查已关闭，产生未定义行为（符合预期）
  ✅ string_oob_get (--fast) 检查已关闭，产生未定义行为（符合预期）
```

契约组仍然成立：产物**仅依赖 libc**，无 libLLVM。

---

## 6. 设计取舍

- **默认安全，性能显式选择。** 默认等级永远是 Debug。想要性能必须主动写 `--fast`，
  并且要接受"回到 C 的静默损坏模式"这一后果。
- **Debug 用 `debugtrap` 而非 `trap`。** `int3` 在有调试器附加时会被当作断点捕获，
  停在越界的那一条指令上；`ud2` 则无条件终止。ReleaseSafe 用 `ud2` 是因为指令更短。
- **三档不是"全开/全关"，而是分层。** 边界与除零检查（`DIAL_SAFETY >= 1`）防的是 UB，
  ReleaseSafe 保留；堆管理完整性检查（`DIAL_HEAP_ASSERT`，同样 `>= 1`）防的是重复释放 /
  use-after-free——一旦发生后续任何行为都不可预测，开销仅一次比较，同样保留；
  只有排查 Runtime 自身 Bug 的冗余断言（`DIAL_ASSERT`，`>= 2`）才在 ReleaseSafe 下关闭；
  OOM 检查则**无条件开启**——分配失败绝不能返回 NULL 让上层带病运行。
- **整数溢出仍未检查**（按 i64 补码回绕）。这是有意保留：Zig 在 Debug 下会检查溢出，
  但那会在几乎所有算术路径上插入 `with.overflow`，与"保持代码直白、不为零概率付费"
  的纪律冲突。若将来需要，应作为一个独立开关引入，而不是混进现有等级。

---

## 7. 变更文件

| 文件 | 变更 |
| --- | --- |
| `runtime/dial_rt_trap.h` / `.c` | **新增**：陷阱设施与三档安全等级宏 |
| `runtime/dial_rt_mem.c` | OOM / 引用计数下溢改为当场 trap |
| `stdlib/dial_array.c` | `get` / `set` 加边界检查；`length` 加一致性断言 |
| `stdlib/dial_string.c` | `get` 加边界检查；`substring` 检查 `[from, to)` 范围 |
| `stdlib/dial_map.c` | `size` 加 `count ∈ [0, cap]` 一致性断言 |
| `src/codegen.rs` | `SafetyMode` 三档；`emit_trap` / `emit_bounds_check` / `emit_div_zero_check` |
| `src/main.rs` | `--safety` / `--release` / `--fast`；`-DDIAL_SAFETY=N` 注入 C 编译 |
| `tests/trap/*.dial` | **新增** 5 个 Fail-Fast 用例 |
| `tests/run_tests.sh` | 新增 D / E 两组 |
| `README.md` | 新增 §20 Fail-Fast 铁律；§17.1 UB 清单重写；§11.5 / §3.2 / §1 同步 |
| `CHANGELOG.md` | 新增 [0.6.0] |
