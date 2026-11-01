//! dialc 编译器统一错误类型。
//! 所有阶段（词法 / 语法 / 语义 / 代码生成）都返回 `Result<T, DialcError>`，
//! 错误信息包含行列号，便于定位。

use std::fmt;

/// 编译错误：携带人类可读信息以及出错位置（行、列，均从 1 开始）。
#[derive(Debug, Clone)]
pub struct DialcError {
    pub message: String,
    pub line: usize,
    pub col: usize,
    /// **稳定错误码**（如 `dialc.package.missing`）。
    ///
    /// 为什么消息与码要分开：消息是给**人**看的，措辞会随版本调整；
    /// 码是给**工具**看的（编辑器按码过滤 / 忽略某类诊断、LSP 侧据此给
    /// 针对性 quickfix）。工具绝不该去匹配消息文本——改一次措辞就废掉一个工具。
    ///
    /// 空串 = 未指定具体码，由 LSP 侧按阶段补 `dialc.lex` / `dialc.parse` /
    /// `dialc.sema` 这类阶段级码。
    pub code: &'static str,
}

impl DialcError {
    pub fn new(message: impl Into<String>, line: usize, col: usize) -> Self {
        DialcError {
            message: message.into(),
            line,
            col,
            code: "",
        }
    }

    /// 带**稳定错误码**的错误。码给工具看，消息给人看——两者职责不同，不可互相替代。
    pub fn with_code(
        code: &'static str,
        message: impl Into<String>,
        line: usize,
        col: usize,
    ) -> Self {
        DialcError {
            message: message.into(),
            line,
            col,
            code,
        }
    }

    /// 返回一个**位置已补上**的副本：原错误没有位置（0,0）时才补。
    ///
    /// 为什么要有这个方法：语义错误多半来自**表达式**（`Expr` 不带位置），
    /// 但「错在哪一条语句」这个信息我们是有的——把它填上，编辑器里的波浪线
    /// 才会落在出错的那一行，而不是文件开头。
    ///
    /// 注意它**保留消息与错误码**：早期实现用 `DialcError::new(e.message, …)`
    /// 重建错误，那会把错误码悄悄丢掉——工具于是又得去匹配消息文本。
    pub fn at(mut self, line: usize, col: usize) -> Self {
        if self.line == 0 && self.col == 0 {
            self.line = line;
            self.col = col;
        }
        self
    }
}

impl fmt::Display for DialcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 && self.col == 0 {
            write!(f, "[dialc 错误] {}", self.message)
        } else {
            write!(f, "[dialc 错误] 第 {} 行 第 {} 列: {}", self.line, self.col, self.message)
        }
    }
}

impl std::error::Error for DialcError {}

/// dialc 内部统一的 Result 别名。
pub type Result<T> = std::result::Result<T, DialcError>;
