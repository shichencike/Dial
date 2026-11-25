# Dial 编译器总路线图与当前进度

> 本文件是**单一事实源**：一屏看清「总路线图是什么、现在走到哪、接下来做什么」。
> 状态快照：**2026-11-24**。更细的证据链与历史裁决见 [`rationale/`](rationale/) 下各专题文档，
> 变更流水见 [`CHANGELOG.md`](CHANGELOG.md)。本文件只记结论与进度，不复述论证过程。

---

## 1. 战略总纲（写死，不随批次漂移）

> **目标：用自研 asm 后端代替 LLVM，不是超越 LLVM。性能够用即可。**

- **OPT-ENOUGH**：性能已达「够用线」并**正式冻结**。同配置 `--fast -O3` 背靠背实测几何平均
  **0.57x**、加权总耗时 **0.91x（反超 LLVM 约 9%）**，远超阈值（几何平均 ≤1.05x、总耗时 ≤1.10x），
  且运行在 ±5% 噪声中。性能优化降级为**回归护栏**，除非命中重启条件（见 §5）不再投入。
- **代替能力完备**：剩余判据全部落在**正确性、平台对等、功能对等、工程化**——即「用户能直接感知、
  且 LLVM 缺了就不叫代替」的维度。
- **「假设 LLVM 不存在、只有自己」总纲**：一切裁决以此为准；LLVM 已变为可选 oracle
  （`--features llvm`），发布包零 `libLLVM`，默认后端为 asm。
- **三条宪法铁律**（并发/回调能力必须遵守）：无闭包（入口一律具名函数 + 显式状态）、
  无用户可见可变全局状态、无用户可见原子类型（内部同步用 C 私有 mutex/cond）。
- **内存底线**：无 GC，ARC + COW + 显式裸指针； Fail-Fast 宁可当场崩溃，绝不静默损坏数据。

---

## 2. 总路线图（两条主线，共享护栏、独立推进）

| 主线 | 目标 | 阶段 | 状态 |
| --- | --- | --- | --- |
| **A. 代替 LLVM 后端** | 让 LLVM 变得不必要 | P0 正确性 → P1 平台 → P2 功能 → P3 工程化 | ✅ 全部收口 |
| **B. dialpkg 包管理** | 通用 C 包分发标准 | P4.0 → P4.1 → P4.2 → P4.3 → P4.4 | ✅ 全部落地 |
| **C. 语言能力补齐** | 补 §17.2 / §41 长期缺口 | own 所有权 → LSP 跨工作区 → 并发 → 反向回调 → GPU 合成 | ✅ 全部完成（5/5） |
| **D. 发布工程** | 一件交付、可信可验 | v1.20 发布 + 零 libLLVM + Windows 签名 | ✅ 已发布 |

专题路线图：[`replace-llvm-roadmap.md`](rationale/replace-llvm-roadmap.md) ·
[`dialpkg-roadmap.md`](rationale/dialpkg-roadmap.md) ·
[`roadmap-gap-analysis.md`](rationale/roadmap-gap-analysis.md)（历史） ·
[`replace-llvm-multidim-audit.md`](rationale/replace-llvm-multidim-audit.md)（历史）

---

## 3. 当前进度（截至 2026-11-22）

### 3.1 主线 A · 代替 LLVM —— ✅ 完成，五项验收全绿

| 阶段 | 交付 | 证据 |
| --- | --- | --- |
| P0 正确性 | 修复 string 拼接/插值临时操作数 ARC 泄漏；§19 零泄漏底线恢复 | `tools/verify_string_arc_no_leak.sh`，RSS 7MB→1.4MB |
| P1 平台 | AArch64 实机语义（qemu）+ DWARF gdb + PGO + inline asm + 常量移位 | golden 22/22 逐字节一致，`verify_aarch64_*.sh` AI/AJ/AP3/AP4/AP5 段 |
| P1 发布前 | Windows x64 Microsoft ABI、CVE 浮点取模修复 | 首次 tag CI 全绿，Release `v1.2` 12 资产 |
| P2 功能 | select 超时、map string 值已落地；原子类型宪法永久否决；print(str) 两边都缺暂不管 | 四项终态，无未决 |
| P3 工程化 | 过时结论清理 + struct 进容器 fuzz 盲区固化 | diff_fuzz 450 次 0 失败 |
| P0 补漏（v1.21 审查） | 修复两类 ARC 泄漏（asm 出口 moved 污染 + LLVM 字面量堆分配）；补齐 asm 相对 LLVM 的 3 项 SIMD 缺口（变量 lane 索引 / bitcast simd / bitcast array） | R22（RSS 断言）、R23（双后端逐值一致 + 越界 Fail-Fast + O0-O3） |
| P2 功能（v1.21 字段级 ARC） | 含句柄字段 struct 进数组/map（desc 表 ARC box，逐字段级联 release）；双后端 `get_or` 命中字段结算对齐；asm KvMap 走专用 release；LLVM string `+` 操作数结算补齐 | R24（双后端逐字一致 + 20 万次混合压测 RSS < 8MB + double-free 回归）；ASAN UAF+leak 双检全绿，RSS 10~40 万次恒定 ~1.5MB |
| P2 功能（v1.22 调试裁剪） | `debug_print(...)` + `debug { ... }`：debug 构建正常输出/执行，release 构建在 analyze 前整块/整句裁剪（跳过类型检查 + 产物零残留）；LSP/test 保留完整 AST | R25（双后端 × debug/release 功能逐字一致 + 副作用开关 + `strings` 零残留 + release 跳过类型检查，10 项全绿） |
| P3 工程化（v1.23 预处理阶段） | 独立 `src/preprocess.rs` 收拢源码收集/import 解析/AST 合并/extern-link 结算/debug 裁剪；`PreprocessOptions` 作为唯一扩展点；`frontend` 瘦身为两步编排 | 行为不变重构：多文件合并+循环检测+DWARF+R25 裁剪 smoke 全过；单测全绿 + 全量回归 |
| P2 功能（v1.23 条件编译 + AST 宏） | 在预处理扩展点上加两道 pass：`@cfg(cond)`（内置 debug/target_os/target_arch + CLI `--cfg`，顶层/语句级，假分支跳过类型检查）；`macro name($a){}` AST 宏（参数替换/嵌套/递归保护/遮蔽函数） | R26 门禁 7 项全绿（cfg 各维度+自定义+release 零残留+宏参数/嵌套/遮蔽+递归 Fail-Fast+孤立 cfg Fail-Fast+跳过检查+宏体 cfg，双后端逐字一致）；单测 24 项全绿 |
| P2 功能（v1.24 `#` 预处理指令） | 词法前文本级层 `src/directives.rs`：`#define/#undef`（对象式 token 替换 + 函数式形参）、`#if/#ifdef/#else/#endif`（C 表达式 + defined + 嵌套 + 行号保持）、`#error/#pragma`、`-D` 注入；与 `@cfg/macro` 并存互补 | R27 门禁 4 组全绿（双后端逐字一致 + `-D` 注入 + 五类 Fail-Fast + 假分支忽略）；单测 40 项全绿 |
| P0 补强（v1.21 后 · 借用检查器） | 零生命周期标注的静态借用检查（NLL 区域推断）：`as_c_str`/`as_ptr` 借用指针在用期间来源被 move/drop/重赋值 → 编译期 `dialc.borrow.dangling`；`&x` 栈借用跳过（零误拒）；frontend 保留结构化错误码，CLI 三处显示一致 | R28 门禁 2 项全绿（三类悬垂拦截 + 合法借用/NLL 双后端一致）；全量回归 1112 项、CVE 8/8 |
| P3 工程（v1.21 后 · 门禁盲区） | cve_check 按 CI 同口径纳入本地 `run_tests.sh` AP3 段，消除「本地全绿、tag CI 才暴露」的发布事故盲区 | 全量回归末尾 CVE 8/8 全绿 |
| P1 优化（v1.21 后 · M10 字符串折叠） | 对齐 gcc -O2：`fold_string_concat`（O1+ 字面量合并）+ `fold_string_accum`（O2+ x86-64 累加循环整函数折叠 return const）；严格骨架识别 + 符号执行验证 + 长度上限，前缀追加/副作用/非静态串一律放弃 | R29 门禁 3 项（O2/O0 逐字一致 + b20 折叠为 `mov $124272` 零字符串调用 + 双后端一致）；全量回归 1112 项零误拒 |
| P1 优化（v1.21 后 · M11~M15 对齐 C 工具链） | 补齐 gcc/clang O2/O3 主力优化：**M11 通用函数内联**（Private 小函数体复制 + ARC 原样 + φ 回传）、**M12 通用 LICM**（循环不变纯计算外提 preheader，别名/归零护栏）、**M13 保守 SCCP**（常量沿 CFG/φ 传播 + 常量条件分支折叠）、**M14 死函数消除**（Private 零引用不动点删除）、**M15 保守循环展开**（标准 for + 上界 2..8 + 纯整数 body）；全部 x86-64 且排除 -g/PGO/插桩，AArch64/LLVM 零变化 | R30~R34 五项门禁（O0/O1/O2/O3 逐字一致 + 双后端逐字一致 + 各 pass 生效证据 + 反例零误拒）；`select_timeout`/GRA/移位形态门禁适配 export fn 后全量回归零回归；单测 41 项、CVE 8/8 |
| P1 产物形态（v1.21 后 · M16 动态库） | asm 后端 `--shared` 产出 ELF .so / macOS dylib / MinGW dll：C 运行时 `-fPIC` 重编（独立缓存分区，绕开非 PIC 预编译归档）+ `.init_array` constructor 自动 `dial_env_init`；仅 `export fn` 导出；与 `--single-file`/`--tiny` Fail-Fast 互斥 | R35 门禁（ELF 类型 + dlsym 符号 + 无 TEXTREL + dlopen 调用含 1000 次 ARC + random 非退化 + 冲突矩阵）；默认可执行产物零回归 |
| **验收** | ①OPT-ENOUGH ②P0 清零 ③AArch64 语义+DWARF ④P2 全裁决 ⑤P3 收口 | **五项全绿，能力上已代替 LLVM** |

### 3.2 主线 B · dialpkg —— ✅ P4.0~P4.4 全部落地

| 阶段 | 交付 |
| --- | --- |
| P4.0 | pack 编 C 源（prebuilt 三平台 `.a`）、C 源码双模式、原子安装+回滚、`--json` 全覆盖、doctor 扩展 |
| P4.1 | 静态索引与搜索（`search`/`info`、SHA256 校验、无中心服务） |
| P4.2 | 完整依赖管理（`src/version.rs` semver + `src/resolve.rs` 传递解析/仲裁 + `dial.lock` + `install --locked`） |
| P4.3 | 下载韧性（缓存/`--offline`/镜像/断点续传）+ 平台声明校验 |
| P4.4 | 依赖闭环（`install --with-deps` + 反依赖检查 + 跨进程锁）+ 升级体验（`outdated`/`upgrade`） |

> 增量更新经评估**本批不做**（体积 KB~MB 级，补丁链引入新安全面，断点续传已覆盖痛点）。

### 3.3 主线 C · 语言能力补齐 —— ✅ 5/5 全部完成

> 用户指令「修复 §41 四项未实现：跨工作区模块图、线程池/协程/异步、C→Dial 反向回调、真实 GPU 合成」，
> 并要求**逐项完成并逐项提交**。五项均已完成并各自单独提交（own 为前置补齐）。

| 项 | 状态 | 提交 / 说明 |
| --- | --- | --- |
| **显式独占所有权 `own`** | ✅ | `da9bf6d` → `cb88c04` → `5adf42f`：方案 C 完整 consume 体系（own 参数、真独占、零开销、修正假 move/单向独占） |
| **跨工作区全量模块图** | ✅ | `9620640`：LSP initialize 递归扫描全部 `.dial`，`workspace/symbol`/references/rename 跨文件，`documentChanges` 每文件一条编辑 |
| **线程池/协程/async-await** | ✅ | `bf2fc0a`：`stdlib/dial_async.{h,c}` 有栈协程 + 有界线程池 + 协作式 executor，`threadpool`/`coroutine`/`task`、`async fn`/`await`，R19 门禁 |
| **C→Dial 反向回调** | ✅ | `98bedb1`：`stdlib/dial_callback.{h,c}` mutex 注册表，具名函数+`rawptr<void>` state，平台线程主动回调，双后端同源 thunk，R20 门禁；**推翻 GUI「绝不反向回调」宪法但保留内存底线** |
| **真实 GPU 合成后端** | ✅ | （本轮提交）第 19 层受控破例：Linux `dlopen` EGL 1.5 surfaceless + GLES2 恒等 shader 离屏合成，上传纹理→FBO→读回逐字节校验、不改源像素；6 个 `dial_gui_gpu_real_*` 追加 API，旧 9 个签名/CRC 钉值不动；软降级 -2，Windows 未实机；三平台 .a 重建、契约审计 344→350、R21 门禁双后端一致；README §38.24.7 |

### 3.4 主线 D · 发布工程 —— ✅ v1.20 已发布

- 版本映射：标签 **`v1.2`** ↔ 对外版本 **`v1.20`** ↔ Cargo **`1.20.0`**。
- LLVM 可选化：`inkwell` optional，`llvm=["dep:inkwell"]`，默认 `default=[]`；发布包零 `libLLVM`（`ldd` 门禁）。
- Windows：MS ABI + 按版一次性自签证书 + Authenticode 签名 + 密码学擦除；验签失败拒装。
- 上述 v1.21 能力（own / 跨工作区 / 并发 / 反向回调）均已提交，**尚未发布**，列于 CHANGELOG `[未发布]` 段。

---

## 4. 版本与提交基线

- 当前工作版本：**1.20.0**（对外 `v1.20`）；v1.21 能力在 `[未发布]` 段累积，**未打 tag、未发 Release**。
- 本轮未推送提交（本地领先 `origin/main` 6 个，现已全部推送）：

| 提交 | 主题 |
| --- | --- |
| `da9bf6d` | 加入显式独占所有权 own var（不引入借用检查器） |
| `cb88c04` | 方案 C-1 修假 move + 真独占 consume（零开销所有权） |
| `5adf42f` | 方案 C-2 完整 consume 体系——own 函数参数 + 真独占 + 零开销 |
| `9620640` | LSP 跨工作区全量模块图（补齐 §17.2 长期缺口） |
| `bf2fc0a` | 协作式线程池/有栈协程/async-await（补齐 §41 缺口） |
| `98bedb1` | C→Dial 反向回调（推翻宪法，受控开放） |

---

## 5. 护栏与重启条件（延续，不变）

1. **回归门禁**：`bash tests/run_tests.sh` 全量全绿（当前 1064 项、0 失败），新增能力必须挂门禁。
2. **双后端一致**：asm 与 LLVM oracle 对同一 `.dial` 输出逐字节一致；默认构建零 `libLLVM`。
3. **文档确定**：`tools/check_version.py` / `check_links.py` 全绿；README 不描述未实现特性。
4. **零新 Rust 依赖**（除非明确授权）；每批带证据链（CHANGELOG + rationale）。
5. **性能重启条件**（仅满足其一才允许重启优化诊断）：新 workload 使 OPT-ENOUGH 跌出够用线 /
   新增可并行语义使负 ROI 方向出现正收益 / P0~P4 收口后用户主动要求压榨性能。

---

## 6. 下一步（按优先级）

1. **打包发布 v1.21**（§41 五项已全部补齐、能力集合稳定）：当用户确认后，收口 v1.21 变更集、
   打 tag、发 Release、推送资产。当前五项的提交均已就位，仅差发布动作。
2. **§38 其余未实现**（如实标注、非当前主线）：原生弹窗/托盘/全局快捷键实机、IMM32/IBus/Fcitx
   输入法平台采集、多行文本/富文本排版、图片解码、可切换深色主题包。
3. **LSP 剩余**（§17.2）：递归包内 `src/` 模块索引、import 依赖图可视化。
4. **发布 v1.21**：当 v1.21 能力集合稳定且用户确认后，打 tag、发 Release、推送资产。

---

## 7. 一句话结论

> **代替 LLVM 的两条主线（后端能力 + 包管理）已全部收口并发布 v1.20；性能冻结、五项验收全绿。
> §41 五项长期缺口（own / LSP 跨工作区 / 协作式并发 / C→Dial 反向回调 / 真实 GPU 合成）
> 已全部逐项补齐并单独提交，仅待打包进 v1.21 发布。**
