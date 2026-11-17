# [安装与发布流程调整] — 2026-11-06 · 理由与证据链

> 本文件是 [CHANGELOG.md](../CHANGELOG.md) 中 `## [安装与发布流程调整] — 2026-11-06` 版本段拆出的「理由 / 证据链」存档：为什么改、实测数据、边界在哪。
> 变更条目（新增 / 更改 / 破坏性变更 / 修复）本身仍留在 CHANGELOG.md。


**目标**：安装脚本不再硬编码任何版本 / 标签，下载地址改为能重定向到最新 Release；
标准库包与其他资产同处一个 Release 页面，方便重定向、也方便被找到。

### 安装脚本（零硬编码）

- `tools/install-dial.sh` 删除 `DEFAULT_VERSION="1.3.0"`、`STDLIB_RELEASE` 默认
  `stdlib-v1.3.0`；`tools/install-dial.ps1` 删除 `$DialVersion = "0.99.1"`（旧残留）
  与 `$StdlibRelease` 默认值。版本 / 标签 / 资产名全部运行时解析；
- 新增资产解析：调 `api.github.com/repos/<repo>/releases/latest`（钉版本时改调
  `releases/tags/<tag>`）拿到 `tag_name` 与 `assets[].name`，按平台 slug 选 zip、
  按扩展名选 `.vsix`，`stdlib.dialpkg` 名字固定；
- 下载地址统一为 `releases/latest/download/<资产名>`，钉版本时退化为
  `releases/download/<tag>/<资产名>`。GitHub 的 latest 下载**要求精确资产名**，
  而本项目资产名带版本号，故必须由清单解析而非推导——顺带修掉
  `dialc-v1.3-linux-x86_64.zip`（CI 实际命名）与旧脚本推导的
  `dialc-v1.3.0-…` 不一致造成的 404；
- `DIAL_VERSION` / `STDLIB_RELEASE` 降级为**可选钉版本**覆盖；`STDLIB_URL` /
  `DIAL_VSIX_URL` 覆盖、镜像优先 + 官方回退、断点续传、失败可见原因、卸载与
  `.dialpkg` 扫描均不变。API 不可达时给出明确指引，不静默猜资产名。

### 发布流程（同一页面）

- `.github/workflows/ci.yml` 的 `release` 作业新增「组装 stdlib.dialpkg」步骤，
  并把 `dist-stdlib/stdlib.dialpkg` 加入上传资产列表：每个 `v*` Release 现在同时含
  三平台 zip、`.vsix` 与 `stdlib.dialpkg`，**即使 stdlib 源码一行未改也照常重新组装**
  （ABI 与编译器版本绑定）；Release 说明里的安装命令也改为零版本号的一键脚本；
- 旧 `publish-stdlib` 独立通道仅作向后兼容保留，并标记 `prerelease: true`，
  避免它抢走 `releases/latest` 的指向；历史 `stdlib-v1.0.0` Release 及旧链接仍可用；
- README 的安装说明、§25.10、§32.5.1、§32.6 同步。历史分版条目里「独立发版 /
  两个标签」的描述系当时做法，不再代表当前约定。

### 验证

- `bash -n` 语法通过；`--help` / `--version` 输出正常；
- 实测 GitHub API 解析出当前 latest 的 `tag_name` 与 4 个资产名，
  `releases/latest/download/<zip>` 与 `<vsix>` 均返回 **200** 并正确重定向到真实资产；
- `stdlib.dialpkg` 在当前 latest（`v0.98`，旧 Release 未附带该资产）返回 404，
  脚本会给出「该版本发布时未附带 stdlib.dialpkg」的明确告警，符合预期。

> **后续修复（同日）**：上述两个测试作业已由上方「CI 修复与 v1.0 标签重打」
> 章节修好；`v1.0` 标签删除后重打并推送，新的 `v*` Release 已附带
> `stdlib.dialpkg`，`releases/latest` 随之指向该页面，安装脚本无需再改动。
> 本节记录的是修复前的发布流程设计与当时的已知前置。
