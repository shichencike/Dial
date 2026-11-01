#!/usr/bin/env bash
# 构建 Dial 的 VS Code 插件安装包（.vsix）。
#
# 优先用官方 vsce（需 npm 环境）：`vsce package`。
# 若环境没有 vsce（无 npm / 无网络），回退到 make_vsix.py 直接打包合规的 .vsix。
set -euo pipefail

cd "$(dirname "$0")"

if command -v vsce >/dev/null 2>&1; then
  echo "== 使用 vsce 打包 =="
  vsce package
else
  echo "== 回退：python 打包（无需 npm）=="
  python3 make_vsix.py
fi
