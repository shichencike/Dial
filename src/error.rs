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
}

impl DialcError {
    pub fn new(message: impl Into<String>, line: usize, col: usize) -> Self {
        DialcError {
            message: message.into(),
            line,
            col,
        }
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
