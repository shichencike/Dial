//! 词法分析器：把源码切分为带行列位置的 Token 序列。
//!
//! v0.1 支持的 token：
//! - 字面量：整数、浮点数、布尔、字符串
//! - 标识符与关键字：var let fn if else while return and or not true false int float bool string
//! - 运算符与标点：+ - * / = == != < > <= >= ! ; , ( ) { } :
//! - 行注释 `//` 到行尾

use crate::error::{DialcError, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // 字面量
    IntLit(i64),
    FloatLit(f64),
    BoolLit(bool),
    StringLit(String),
    Ident(String),

    // 关键字
    Var,
    Let,
    Fn,
    Struct,
    If,
    Else,
    While,
    Return,
    And,
    Or,
    Not,

    // 类型关键字
    IntT,
    FloatT,
    BoolT,
    StringT,
    VoidT,

    // 运算符与标点
    Plus,
    Minus,
    Star,
    Slash,
    Eq,       // ==
    Neq,      // !=
    Lt,       // <
    Gt,       // >
    Le,       // <=
    Ge,       // >=
    Assign,   // =
    Semicolon,
    Comma,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Colon,
    Dot,
    LBracket,
    RBracket,

    EOF,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub col: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = src.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;

    let push = |kind: TokenKind, l: usize, c: usize, out: &mut Vec<Token>| {
        out.push(Token { kind, line: l, col: c });
    };

    while i < chars.len() {
        let c = chars[i];

        // 空白
        if c.is_whitespace() {
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
            i += 1;
            continue;
        }

        // 行注释
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
                col += 1;
            }
            continue;
        }

        // 字符串
        if c == '"' {
            let start_line = line;
            let start_col = col;
            i += 1;
            col += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != '"' {
                let ch = chars[i];
                if ch == '\\' && i + 1 < chars.len() {
                    let nxt = chars[i + 1];
                    match nxt {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        '\\' => s.push('\\'),
                        '"' => s.push('"'),
                        _ => {
                            s.push(ch);
                            s.push(nxt);
                        }
                    }
                    i += 2;
                    col += 2;
                } else {
                    s.push(ch);
                    i += 1;
                    col += 1;
                }
            }
            if i >= chars.len() {
                return Err(DialcError::new("字符串字面量未闭合", start_line, start_col));
            }
            i += 1; // 跳过结束引号
            col += 1;
            push(TokenKind::StringLit(s), start_line, start_col, &mut tokens);
            continue;
        }

        // 数字（整数 / 浮点）
        if c.is_ascii_digit() {
            let start_line = line;
            let start_col = col;
            let mut num = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                num.push(chars[i]);
                i += 1;
                col += 1;
            }
            if i < chars.len() && chars[i] == '.' {
                num.push('.');
                i += 1;
                col += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    num.push(chars[i]);
                    i += 1;
                    col += 1;
                }
                let f: f64 = num
                    .parse()
                    .map_err(|_| DialcError::new("非法的浮点数字面量", start_line, start_col))?;
                push(TokenKind::FloatLit(f), start_line, start_col, &mut tokens);
            } else {
                let n: i64 = num
                    .parse()
                    .map_err(|_| DialcError::new("非法的整数字面量", start_line, start_col))?;
                push(TokenKind::IntLit(n), start_line, start_col, &mut tokens);
            }
            continue;
        }

        // 标识符 / 关键字
        if c.is_alphabetic() || c == '_' {
            let start_line = line;
            let start_col = col;
            let mut id = String::new();
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                id.push(chars[i]);
                i += 1;
                col += 1;
            }
            let kind = match id.as_str() {
                "var" => TokenKind::Var,
                "let" => TokenKind::Let,
                "fn" => TokenKind::Fn,
                "if" => TokenKind::If,
                "else" => TokenKind::Else,
                "while" => TokenKind::While,
                "return" => TokenKind::Return,
                "and" => TokenKind::And,
                "or" => TokenKind::Or,
                "not" => TokenKind::Not,
                "true" => TokenKind::BoolLit(true),
                "false" => TokenKind::BoolLit(false),
                "struct" => TokenKind::Struct,
                "int" => TokenKind::IntT,
                "float" => TokenKind::FloatT,
                "bool" => TokenKind::BoolT,
                "string" => TokenKind::StringT,
                "void" => TokenKind::VoidT,
                _ => TokenKind::Ident(id),
            };
            push(kind, start_line, start_col, &mut tokens);
            continue;
        }

        // 运算符与标点
        let peek = chars.get(i + 1).copied();
        let start_line = line;
        let start_col = col;
        match c {
            '+' => {
                push(TokenKind::Plus, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '-' => {
                push(TokenKind::Minus, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '*' => {
                push(TokenKind::Star, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '/' => {
                push(TokenKind::Slash, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '=' => match peek {
                Some('=') => {
                    push(TokenKind::Eq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Assign, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            '!' => match peek {
                Some('=') => {
                    push(TokenKind::Neq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    return Err(DialcError::new(
                        "意外的 '!'，请使用关键字 not 表示逻辑非",
                        start_line,
                        start_col,
                    ))
                }
            },
            '<' => match peek {
                Some('=') => {
                    push(TokenKind::Le, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Lt, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            '>' => match peek {
                Some('=') => {
                    push(TokenKind::Ge, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Gt, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            ';' => {
                push(TokenKind::Semicolon, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            ',' => {
                push(TokenKind::Comma, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '(' => {
                push(TokenKind::LParen, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            ')' => {
                push(TokenKind::RParen, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '{' => {
                push(TokenKind::LBrace, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '}' => {
                push(TokenKind::RBrace, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            ':' => {
                push(TokenKind::Colon, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '.' => {
                push(TokenKind::Dot, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '[' => {
                push(TokenKind::LBracket, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            ']' => {
                push(TokenKind::RBracket, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            _ => {
                return Err(DialcError::new(
                    format!("无法识别的字符 '{}'", c),
                    start_line,
                    start_col,
                ))
            }
        }
    }

    push(TokenKind::EOF, line, col, &mut tokens);
    Ok(tokens)
}
