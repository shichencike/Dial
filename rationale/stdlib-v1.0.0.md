# [stdlib v1.0.0] — 2026-10-31（独立 Release，tag `stdlib-v1.0.0`） · 理由与证据链

> 本文件是 [CHANGELOG.md](../CHANGELOG.md) 中 `## [stdlib v1.0.0] — 2026-10-31（独立 Release，tag `stdlib-v1.0.0`）` 版本段拆出的「理由 / 证据链」存档：为什么改、实测数据、边界在哪。
> 变更条目（新增 / 更改 / 破坏性变更 / 修复）本身仍留在 CHANGELOG.md。


官方标准库（由 `stdlib/` + `runtime/` 编译归档而成的 `libdial_std.a`，被**每一个** Dial 程序静态链入）
改造为 `.dialpkg` 形态，**独立发版、与 `dialc` 编译器解耦**，走 GitHub Releases 分发。

### Docs（文档）
- ARCHITECTURE.md §8 阶段 7 标 ✅；README §25 新增 §25.10「官方标准库 `stdlib.dialpkg`（独立发布）」；
  §25 顶部状态注释同步（URL 安装 / 一键脚本 / stdlib 化均已落地）。
