-- Dial 的 Neovim 集成（零依赖，只用 Neovim 内置 LSP 客户端）。
--
-- 要做的事：
--   1. 打开 .dial 文件时启动 `dialc lsp`（stdio）。
--   2. 把 Hover / 跳转定义 / 引用 / 重命名 / 补全 接到常用键位。
--   3. 启用 LSP 语义高亮（Neovim 0.9+ 会据此给关键字/字符串/数字等上色）。
--   4. 轮2~4 的新能力（大纲 / 格式化 / 代码动作 / 折叠）也接上——
--      内置 LSP 客户端会按服务端广播的 capability 自动启用，无需手动协商。
--
-- 唯一前置条件：dialc 在 PATH 中（`dialc lsp` 可用，即 v0.98+）。

local M = {}

--- 启动 dialc 语言服务器（幂等：已启动则跳过）。
local function ensure_dial_lsp()
  if vim.fn.executable("dialc") == 0 then
    vim.notify(
      "未找到 dialc：请确认 dialc 已安装并在 PATH 中（需要支持 lsp 子命令，v0.98+）",
      vim.log.levels.WARN
    )
    return
  end

  -- 避免同一 buffer 重复启动
  for _, client in ipairs(vim.lsp.get_clients({ name = "dialc" })) do
    if client.attached_buffers[vim.api.nvim_get_current_buf()] then
      return
    end
  end

  vim.lsp.start({
    name = "dialc",
    cmd = { "dialc", "lsp" },
    filetypes = { "dial" },
    root_dir = vim.fs.dirname(vim.api.nvim_buf_get_name(0)),
    settings = {},
  })
end

vim.api.nvim_create_autocmd("FileType", {
  pattern = "dial",
  callback = function()
    local opts = { buffer = true, silent = true }

    -- 键位：对齐 Neovim 社区惯例
    vim.keymap.set("n", "K", vim.lsp.buf.hover, opts)              -- 悬停
    vim.keymap.set("n", "gd", vim.lsp.buf.definition, opts)       -- 跳转定义
    vim.keymap.set("n", "gr", vim.lsp.buf.references, opts)       -- 查找引用
    vim.keymap.set("n", "<leader>rn", vim.lsp.buf.rename, opts)   -- 重命名
    vim.keymap.set("n", "<C-k>", vim.lsp.buf.signature_help, opts)-- 签名帮助
    vim.keymap.set("i", "<C-Space>", vim.lsp.buf.completion, opts) -- 补全
    vim.keymap.set("n", "<leader>f", vim.lsp.buf.format, opts)     -- 格式化（保守版）
    vim.keymap.set("n", "<leader>a", vim.lsp.buf.code_action, opts) -- 代码动作
    vim.keymap.set("n", "<leader>ds", vim.lsp.buf.document_symbol, opts) -- 文档大纲

    ensure_dial_lsp()

    -- 语义高亮：Neovim 0.9+ 在会话刷新时会自动请求；这里显式启动一次，
    -- 确保打开文件即上色。
    pcall(vim.lsp.semantic_tokens.start, 0)

    -- 折叠：Neovim 0.10+ 才有 foldingRange 客户端（0.9 会报字段不存在，用 pcall 兜住）。
    pcall(function() vim.lsp.folding_range.enable(true) end)
  end,
})

return M
