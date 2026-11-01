# Dial for VS Code（v0.98）

给 `.dial` 文件提供：

1. **保存即报诊断**——走的是编译器自己的词法/语法/语义，因此绝不会出现「编辑器说能过、编译器说不能过」。
2. **语义高亮**——token 由编译器自己的词法器产出，高亮与编译用的是同一份词法，不会慢慢分道扬镳。
3. **Hover / 跳转定义 / 补全**——由 `dialc lsp`（v0.95+）提供，光标悬停看类型与文档，点击跳到定义，自动补全可见符号。

> 安装包：`dial-0.98.0.vsix`（见各版本 GitHub Release，或本地 `bash editors/vscode/build.sh` 现构建）。
> 余下能力（文档符号大纲、引用查找、重命名、格式化、签名帮助等）在后续 LSP 轮次补齐，见主仓库 README [§28.7](../../README.md#287-编辑器插件安装包v098)。

## 安装

这个扩展**不需要 npm install**（没有第三方依赖，LSP 客户端是手写的 200 行）。

```bash
# 方式一：侧载 .vsix（推荐，来自 Release 或本地构建）
code --install-extension dial-0.98.0.vsix

# 方式二：直接拷进扩展目录（改完源码可立刻生效）
mkdir -p ~/.vscode/extensions/dial-lang.dial-0.98.0
cp -r /path/to/dialc/editors/vscode/* ~/.vscode/extensions/dial-lang.dial-0.98.0/
# 重启 VS Code

# 方式三：开发模式——VS Code 里打开本目录，按 F5 启动「扩展开发宿主」
```

前置条件：`dialc` 在 `PATH` 里，且版本 **≥ v0.98**（要支持 `lsp` 子命令）。
不在 PATH 里也行，改设置里的 `dial.lsp.path` 指到绝对路径。

> 如果你的 `dialc` 是动态链接本机 LLVM 的（如 `libLLVM.so.18.1rc`），需要先让加载器找得到它，
> 例如 `export LD_LIBRARY_PATH=/path/to/llvm/lib`。启动失败时看「输出 → Dial Language Server」面板。

## 设置

| 设置项 | 默认 | 说明 |
| --- | --- | --- |
| `dial.lsp.path` | `dialc` | 编译器路径 |
| `dial.lsp.trace` | `false` | 把协议收发打到输出面板（排障用） |

## 为什么不用 `vscode-languageclient`

官方客户端库更完备，但要拉一整棵 npm 依赖树。本扩展真正要做的只有四件事：
起进程、转发文档变化、收诊断、转发语义 token。手写这 200 行换来的是
**拷进目录就能用，不需要 Node / npm / 联网**。对一门语言的第一版编辑器支持来说，
「装得上」比「功能全」重要。

## 其它编辑器

任何支持 LSP 的编辑器都能接，只需要它去启动 `dialc lsp`。本仓库已附带：

- **Neovim**：[`../neovim/README.md`](../neovim/README.md)（零依赖，内置 LSP 客户端）
- **Emacs**：[`../emacs/README.md`](../emacs/README.md)（`eglot` 原生连，只加几行 `init.el`）

其它编辑器（JetBrains / Zed / Sublime 等）照各自文档把 `dialc lsp` 配成 LSP server 即可，
不再单独维护插件。
