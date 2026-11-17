# [CI 修复与 v1.0 标签重打] — 2026-11-06 · 理由与证据链

> 本文件是 [CHANGELOG.md](../CHANGELOG.md) 中 `## [CI 修复与 v1.0 标签重打] — 2026-11-06` 版本段拆出的「理由 / 证据链」存档：为什么改、实测数据、边界在哪。
> 变更条目（新增 / 更改 / 破坏性变更 / 修复）本身仍留在 CHANGELOG.md。


**目标**：修掉上一节留下的两个红色测试作业，让 `release` 作业能真正产出带
`stdlib.dialpkg` 的 `v*` Release；随后删除并重打 `v1.0` 标签触发新发布，
使 `releases/latest` 指向这个新页面。纯修复，**编译器版本 `1.3.0` 与
`STDLIB_ABI 4` 均不变**。

### 验证

- `zig cc -target x86_64-windows-gnu` 编译 `dial_env.c` 通过，`_WIN32` /
  `__MINGW32__` 宏确认定义，预处理确认选中 `GetSystemTimeAsFileTime` + `Sleep`
  分支；`-target aarch64-linux-musl` 与 `x86_64-linux-gnu` 全量编译链接通过
  （zig 裸 windows 目标缺 `pthread.h` 是验证环境限制，CI 已安装 winpthreads）；
- `bash tools/build_stdlib.sh` 全量归档通过；`examples/env_demo.dial` 端到端输出一致；
  C 级验证：PRNG 连续 8 次取值互异且非负、`now()` 在 `sleep(12)` 后严格增大；
- `python3 tools/cve_check.py --repeat 1` 8/8 通过；`bash tests/run_tests.sh` A–O 全绿。

### 发布

- 删除本地与远程 `refs/tags/v1.0`（该标签此前**没有**关联任何 Release，
  CI 的 `release` 作业因测试作业失败被 skip，删除是安全的）；
- 在本节修复后的最新提交上重新创建 `v1.0` 附注标签并推送，触发新的 `v*` CI：
  三平台构建 → `release` 作业上传 zip / `.vsix` / `stdlib.dialpkg` →
  `releases/latest` 指向该页面。安装脚本零改动即可生效。
