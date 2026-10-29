# Dial v0.61 交付报告 —— 文件 IO（file 句柄）

- **版本号**：`0.61.0`（Cargo 三段式）/ `v0.61`（文档与官网统一显示）
- **相对上次发布**：v0.6 → v0.61，按 §23 版本号规则 **+0.01**（小功能 / 新语法）
- **提交**：`45ab414 feat: v0.61 —— 文件 IO（file 句柄，不透明指针封装 C FILE*）`
- **发布包**：`dialc-v0.61.zip`（含 `.git` 完整历史，11 个提交；排除 `target/`、`.dialc_build/`、`*.o`）
- **测试结论**：`== 全部测试通过 ✅ ==`（exit 0）

---

## 1. 本次新增能力

新增一等类型 `file`，底层**直接封装 C 标准库 `FILE*`**，提供最基础的 C 风格 API：

| 函数 | 签名（Dial） | 返回 | 说明 |
|------|------|------|------|
| `open` | `open(path: string, mode: string) -> file` | `file` 句柄；失败返回 `null` | 封装 `fopen` |
| `read` | `read(f: file, n: int) -> string` | 读取到的字符串；失败返回空串 | 封装 `fread` |
| `write` | `write(f: file, content: string) -> int` | 实际写入字节数；失败返回 `-1` | 封装 `fwrite` |
| `close` | `close(f: file)` | `void` | 封装 `fclose`，**必须显式调用** |

前端链路：`ast` 类型 → `lexer` 关键字 `file` → `parser` 类型解析 → `semantic` 内置函数签名注册 + `print` 拒绝直接打印句柄 → `codegen` 类型映射（`i8*` 不透明指针）/C 声明/调用发射。
C 侧：`dial_io.c` 四个 `fopen/fread/fwrite/fclose` 封装 + `dial_string.c` 新增 `dial_string_cstr` / `dial_string_from_bytes` 两个内部辅助。

## 2. 四条不可违反的底线（已落实并自检）

> 用户原话要求：不透明指针、仅封装 C stdio、无异常/无 Result、无花哨语法。

1. **绝不引入全局变量**：文件句柄是显式传递的 **Opaque Pointer（`void*` / `i8*`）**，用户代码无法触碰布局，Rust 前端不持有任何文件状态。
2. **底层只封装 C stdio**：`dial_file_open/read/write/close` 仅转发到 `fopen/fread/fwrite/fclose`；Rust 前端零文件系统逻辑。
3. **无异常处理**：打开失败返 `NULL`、读失败返空串、写失败返 `-1`，**不引入 `Result`、不搞异常**；编译期类型不匹配直接拒绝（见 §3 负向用例）。
4. **无花哨语法**：仅有 `open/read/write/close` 四个最基础 API，无链式读取流、无迭代器封装。

## 3. 内存宪法 §19 的边界：file 不受 ARC 管理

`file` 句柄是 §19 ARC+COW 的**例外**——符合 §19.3「显式指针回收」：`open` 获得的句柄由用户显式 `close`，**不会被引用计数自动释放**，也不会被 COW 复制。这一点已在 README §24.3 写明，提醒用户忘记 `close` 会泄漏（但绝不会破坏堆安全）。

## 4. 端到端测试（TEST_RUN_v0.61.log）

| 分组 | 内容 | 结果 |
|------|------|------|
| A 正向 | 15 个黄金用例输出一致，含新建 `file_demo` | ✅ 全部通过 |
| B 负向 | `io_bad_arg.dial`：`open(123,"w")` 编译期拒绝 | ✅ `语义错误：open 的第一个参数（path）必须是 string` |
| C 契约 | 用户程序 `ldd` 仅依赖 libc（无 libLLVM） | ✅ |
| D Fail-Fast | 5 个越界/除零用例信号终止（int3/ud2），不打印哨兵 | ✅ |
| E `--fast` 对照 | 同一份代码关闭检查后恢复 C 静默 UB | ✅ 符合预期 |

## 5. 文档与官网同步

- `README.md`：§4.3 关键字补 `file`、§5.1 基本类型表补 `file` 行、§15 FFI 表与 C 导出函数补 4 行 + `DialFile`、§17.2 未实现清单**移除「文件 IO」**、§22 新增 `v0.61` 变更摘要、文末新增 **§24 文件 IO（file 句柄）** 专章（API / 无异常 / 必须 close / 完整示例）。
- `CHANGELOG.md`：顶部新增 `[0.61.0] — 2026-10-29`，含 Added/Changed/设计约束四底线；顺手修正 `break/continue` 仍被误标为「v0.5 计划」的事实错误。
- `tools/build_web.py` + 重新生成 `web/*.html`：页脚/指标/下载链接 `v0.6` → `v0.61`；卡片新增「文件 IO（v0.61）」；校验锚点 `id="24-文件-iofile-句柄"`、`id="23-版本号规则"` 存在，`v0.61` 在 `index.html` 出现 4 次。

## 6. 交付物清单

- 源码 + 文档 + 官网（已 `git commit` 于 `45ab414`）
- `examples/file_demo.dial` + `examples/file_demo.expected`（黄金字节 `dialc-file-io\n`）
- `tests/bad/io_bad_arg.dial`（负向用例）
- `TEST_RUN_v0.61.log`（本次测试完整日志）
- `dialc-v0.61.zip`（含 `.git`，已自检：历史完整、工作树干净、无构建残留）

## 7. 后续路线图（需用户拍板方可推进）

受「禁止跳版、逐档推进」约束，以下特性尚未排入：**全局变量**、**多维非 int 数组**、**字符或裸块 `{ }` 语法**（当前唯一保留在「未实现」清单项）。下一档版本号将按 §23 规则确定（+0.01 或 +0.1）。
