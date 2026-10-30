# Dial for VS Code（v0.94 骨架）

给 `.dial` 文件提供两件事：

1. **保存即报诊断**——走的是编译器自己的词法/语法/语义，因此绝不会出现「编辑器说能过、编译器说不能过」。
2. **语义高亮**——token 由编译器自己的词法器产出，高亮与编译用的是同一份词法，不会慢慢分道扬镳。

> 这是**骨架**，不是完整 IDE 支持。当前没有：自动补全、跳转定义、悬停文档、重命名、格式化。
> 这些都要建立在跨文件符号表之上，属于后续版本。

## 安装

这个扩展**不需要 npm install**（没有第三方依赖，LSP 客户端是手写的 200 行）。

```bash
# 方式一：直接拷进扩展目录（改完源码可立刻生效）
mkdir -p ~/.vscode/extensions/dial-lang.dial-0.94.0
cp -r /path/to/dialc/editors/vscode/* ~/.vscode/extensions/dial-lang.dial-0.94.0/
# 重启 VS Code

# 方式二：开发模式——VS Code 里打开本目录，按 F5 启动「扩展开发宿主」
```

前置条件：`dialc` 在 `PATH` 里，且版本 **≥ v0.94**（要支持 `lsp` 子命令）。
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

任何支持 LSP 的编辑器都能接，只需要它去启动 `dialc lsp`。Neovim（原生 LSP，无需插件）：

```lua
vim.filetype.add({ extension = { dial = "dial" } })
vim.api.nvim_create_autocmd("FileType", {
  pattern = "dial",
  callback = function(args)
    vim.lsp.start({
      name = "dialc",
      cmd = { "dialc", "lsp" },
      root_dir = vim.fs.dirname(vim.fs.find({ ".git" }, { upward = true })[1]) or vim.fn.getcwd(),
    })
  end,
})
```

Vim 自带的语法高亮已经能覆盖注释与字符串；语义 token 需要编辑器支持
（Neovim 0.9+ 会在 `LspTokenUpdate` 里给出）。
