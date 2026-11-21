# Dial 编译器总路线图与当前进度

> 本文件是**单一事实源**：一屏看清「总路线图是什么、现在走到哪、接下来做什么」。
> 状态快照：**2026-11-22**。更细的证据链与历史裁决见 [`rationale/`](rationale/) 下各专题文档，
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
| **C. 语言能力补齐** | 补 §17.2 / §41 长期缺口 | own 所有权 → LSP 跨工作区 → 并发 → 反向回调 → GPU 合成 | 🔄 进行中（4/5） |
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

### 3.3 主线 C · 语言能力补齐 —— 🔄 4/5

> 用户指令「修复 §41 四项未实现：跨工作区模块图、线程池/协程/异步、C→Dial 反向回调、真实 GPU 合成」，
> 并要求**逐项完成并逐项提交**。前三项已完成并各自单独提交，第四项进行中。

| 项 | 状态 | 提交 / 说明 |
| --- | --- | --- |
| **显式独占所有权 `own`** | ✅ | `da9bf6d` → `cb88c04` → `5adf42f`：方案 C 完整 consume 体系（own 参数、真独占、零开销、修正假 move/单向独占） |
| **跨工作区全量模块图** | ✅ | `9620640`：LSP initialize 递归扫描全部 `.dial`，`workspace/symbol`/references/rename 跨文件，`documentChanges` 每文件一条编辑 |
| **线程池/协程/async-await** | ✅ | `bf2fc0a`：`stdlib/dial_async.{h,c}` 有栈协程 + 有界线程池 + 协作式 executor，`threadpool`/`coroutine`/`task`、`async fn`/`await`，R19 门禁 |
| **C→Dial 反向回调** | ✅ | `98bedb1`：`stdlib/dial_callback.{h,c}` mutex 注册表，具名函数+`rawptr<void>` state，平台线程主动回调，双后端同源 thunk，R20 门禁；**推翻 GUI「绝不反向回调」宪法但保留内存底线** |
| **真实 GPU 合成后端** | 🟡 **进行中** | 第 18 层纯值契约已落地（`dial_gui_platform.c`，恒返降级码 -2）。**技术可行性已 spike 验证**：无 DISPLAY 下 EGL 1.5 surfaceless + Mesa 软件渲染可跑通纹理上传→GLES2 shader→FBO 读回，逐字节精确（fwd_err=0）。实现以「追加 extern fn、旧签名/CRC 钉值不动、能力自适应测试」方式落地，尚未提交 |

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

1. **真实 GPU 合成后端**（当前中断点）：以最小破例 dlopen `libEGL`/`libGLESv2` 做离屏合成——
   旧 9 个契约 API 与 CRC 钉值不动，追加 `gpu_real_*` extern fn；Linux 走 EGL surfaceless 软件渲染
   实机验证，其余平台保留降级 -2；挂 R21 门禁 + 文档 + 单独提交。已备可行性 spike 证据。
2. **§38 其余未实现**（如实标注、非当前主线）：原生弹窗/托盘/全局快捷键实机、IMM32/IBus/Fcitx
   输入法平台采集、多行文本/富文本排版、图片解码、可切换深色主题包。
3. **LSP 剩余**（§17.2）：递归包内 `src/` 模块索引、import 依赖图可视化。
4. **发布 v1.21**：当 v1.21 能力集合稳定且用户确认后，打 tag、发 Release、推送资产。

---

## 7. 一句话结论

> **代替 LLVM 的两条主线（后端能力 + 包管理）已全部收口并发布 v1.20；性能冻结、五项验收全绿。
> 当前在补 §41 最后一项「真实 GPU 合成」——技术可行性已验证，实现待续。own 所有权、跨工作区
> LSP、协作式并发、C→Dial 反向回调四项均已逐项提交，仅待打包进 v1.21 发布。**
