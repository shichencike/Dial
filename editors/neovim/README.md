# Dial 的 Neovim 集成

零依赖，只用 Neovim 内置的 LSP 客户端（**不需要** nvim-lspconfig、不需要 Node）。
直接调用 `dialc lsp`，提供：诊断（打开/保存即报）、语义高亮、Hover、跳转定义、
查找引用、重命名、签名帮助、补全。

> 前置条件：`dialc` 必须在 `PATH` 中，且支持 `lsp` 子命令（v0.98+）。
> 验证：`dialc lsp < /dev/null` 不报错即可。

## 安装（三选一）

### 方式 A：直接放进 runtimepath（最简单）

把本目录下的 `ftdetect/` 与 `plugin/` 拷进你的 Neovim 配置：

```bash
mkdir -p ~/.config/nvim/lua/dial-lsp
cp -r ftdetect plugin ~/.config/nvim/lua/dial-lsp/
```

然后在 `init.lua` 里 require 一次：

```lua
require("dial-lsp.plugin.dial")
```

### 方式 B：lazy.nvim

```lua
{
  dir = "/path/to/Dial/editors/neovim",  -- 指向本目录
  config = function()
    require("plugin.dial")              -- 目录内的 plugin/dial.lua
  end,
}
```

### 方式 C：Packer

```lua
use {
  "/path/to/Dial/editors/neovim",
  config = function() require("plugin.dial") end,
}
```

## 使用

打开任意 `.dial` 文件即可，LSP 会自动启动。

| 键位 | 功能 |
| --- | --- |
| `K` | Hover（类型 + 文档） |
| `gd` | 跳转定义 |
| `gr` | 查找引用 |
| `\rn` | 重命名 |
| `<C-k>` | 签名帮助 |
| `<C-Space>`（插入模式） | 触发补全 |
| `<leader>f` | 格式化（保守版：只去行尾空白，不重排） |
| `<leader>a` | 代码动作（缺包时可一键「安装这个包」） |
| `<leader>ds` | 文档符号大纲 |

> 内置 LSP 客户端会按服务端下发的 capability **自动启用**对应功能，
> 所以引用查找、重命名、签名帮助、折叠等无需额外配置。
> 折叠需要 Neovim 0.10+（0.9 会自动跳过，不会报错）。

## 语义高亮

Neovim 0.9+ 会按服务端下发的语义 token 自动上色（关键字 / 字符串 / 数字 / 运算符 /
变量 / 类型）。若发现没上色，确认你的配色方案没有禁用 `LspSemanticTokens` 高亮组，
并可在 `init.lua` 里手动确保：

```lua
vim.cmd("highlight default link LspSemanticTokensKeyword Keyword")
```

## 诊断

打开 `.dial` 文件后，语义错误会实时显示在对应行上（诊断带稳定错误码
`dialc.lex` / `dialc.parse` / `dialc.sema` / `dialc.package.missing`）。
`import` 了**未安装**的包时，也会明确报出来——不会像从前那样静悄悄什么也不说。

## 排障

- `:LspInfo` 查看 dialc 客户端是否已 attach 到当前 buffer。
- `:LspLog` 查看与 `dialc lsp` 的通信日志。
- 若提示「未找到 dialc」，把 `dialc` 所在目录加入 `PATH` 后重启 Neovim。
