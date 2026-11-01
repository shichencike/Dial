# Dial 编辑器集成

`dialc lsp` 是唯一的编辑器后端（stdio JSON-RPC）。本目录下的三个子目录对应三套主流编辑器的接入方式。

| 目录 | 编辑器 | 接入方式 | 安装包 |
| --- | --- | --- | --- |
| [`vscode/`](vscode/) | VS Code | 手写 LSP 客户端（无 npm 依赖） | `dial-*.vsix`，`code --install-extension` 侧载 |
| [`neovim/`](neovim/) | Neovim | 内置 LSP 客户端 | 拷进 `runtimepath` 即可，零依赖 |
| [`emacs/`](emacs/) | Emacs | 内置 `eglot` | 仅 `init.el` 片段，不开发插件 |

## 前置条件（三者通用）

- `dialc` 必须在 `PATH`（或编辑器配置的可执行路径）中，且支持 `lsp` 子命令（v0.98+）。
- 验证：`dialc lsp < /dev/null` 不报错即说明后端可用。

## 已支持的 LSP 能力

诊断（保存即报）、语义高亮、Hover、跳转定义、补全（精确替换 + 排序）、文档符号大纲、
工作区符号、引用查找、重命名（含 prepareRename）、签名帮助、保守格式化、参数 inlay 提示、
codeAction、折叠范围、选择范围、文档链接。

能力清单与实现要点详见主仓库 README [§28.7](https://github.com/shichencike/Dial/blob/main/README.md#287-编辑器插件安装包v098)。

## 为什么只做三套

编译型语言的编辑器支持是「必做的苦活」，没有它语言没人用。但精力优先留给语言本身：
三种主流编辑器之外的环境（JetBrains / Zed / Sublime 等）只要能启动 `dialc lsp`，用各自的 LSP
客户端连上即可，不再单独维护插件。
