# Dial 的 Emacs 集成（eglot）

Emacs 通过内置的 **eglot**（Emacs 29+ 已内置，更早版本用 `package-install eglot` 即可）
原生连 LSP，**不需要任何额外插件**。下面的片段把 `dialc lsp` 注册为 Dial 的 LSP 服务器，
打开 `.dial` 文件即自动获得：诊断（Flymake 波浪线）、Hover、跳转定义、补全、语义高亮。

> 前置条件：`dialc` 在 `exec-path` 中，且支持 `lsp` 子命令（v0.98+）。
> 验证：在 shell 跑 `dialc lsp < /dev/null` 不报错即可。

## 安装：把下面这段加进 `init.el`

```elisp
;; 1) 给 .dial 文件一个专属 major-mode（eglot 按 major-mode 选服务器，故此步关键）
(define-derived-mode dial-mode prog-mode "Dial"
  "Major mode for Dial source files."
  (setq-local comment-start "// ")
  (setq-local comment-start-skip "//+\\s-*"))

;; 2) 关联扩展名
(add-to-list 'auto-mode-alist '("\\.dial\\'" . dial-mode))

;; 3) 注册 dialc lsp 为 dial-mode 的 LSP 服务器
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '(dial-mode . ("dialc" "lsp"))))

;; 4) 打开 .dial 时自动启动 eglot
(add-hook 'dial-mode-hook #'eglot-ensure)
```

用 `use-package` 的话：

```elisp
(use-package eglot
  :hook (dial-mode . eglot-ensure)
  :config
  (add-to-list 'eglot-server-programs
               '(dial-mode . ("dialc" "lsp"))))
```

## 常用操作

| 键位 / 命令 | 功能 |
| --- | --- |
| `M-x eglot-help-at-point`（`C-h .`） | Hover |
| `M-x xref-find-definitions`（`M-.`） | 跳转定义 |
| `M-x xref-find-references`（`M-?`） | 查找引用 |
| `M-x eglot-rename` | 重命名 |
| `M-x completion-at-point`（`M-TAB`） | 补全 |
| `M-x flymake-show-buffer-diagnostics` | 查看诊断 |

## 语义高亮

Emacs 29+ 的 eglot 会自动消费服务端下发的语义 token 并上色（关键字 / 字符串 / 数字 /
变量 / 类型），无需额外配置。

## 大纲（函数/结构体列表）

`which-func-mode` 或 `imenu` 可显示当前光标所在的函数名：

```elisp
(add-hook 'dial-mode-hook #'which-func-mode)
```

> 注意：当前 `dialc lsp` 的文档符号（documentSymbol）能力计划在后续 LSP 轮次补齐，
> 届时 `imenu` 列表会直接由语言服务器填充；在此之前 `which-func-mode` 仅显示 eglot
> 已索引到的符号。

## 排障

- `M-x eglot-reconnect`：若 LSP 进程异常可重连。
- `*EGLOT (dial-mode) ...*` 事件缓冲：查看与 `dialc lsp` 的通信。
- 若 Emacs 报「无法找到 dialc」，把 `dialc` 所在目录加入 `exec-path`：
  ```elisp
  (add-to-list 'exec-path "/usr/local/bin")
  ```
