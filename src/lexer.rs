//! 词法分析器：把源码切分为带行列位置的 Token 序列。
//!
//! v0.1 支持的 token：
//! - 字面量：整数、浮点数、布尔、字符串
//! - 标识符与关键字：var let fn if else while return and or not true false int float bool string
//! - 运算符与标点：+ - * / = == != < > <= >= ! ; , ( ) { } :
//! - v1.00 新增：复合赋值 += -= *= /= %=、自增自减 ++ --、位运算 & | ^ ~ << >>、三元 ? 、
//!   进制字面量 0x/0b/0o 与数字分隔符 1_000
//! - 行注释 `//` 到行尾

use crate::error::{DialcError, Result};

/// 字符串插值的一个片段（v1.00）：`"x=${v}"` 拆成 `Text("x=")` + `Expr("v")`。
#[derive(Debug, Clone, PartialEq)]
pub enum InterpPiece {
    /// 普通文本（已处理转义）。
    Text(String),
    /// `${...}` 内的源码文本，由 parser 做二次 lex/parse 得到真正的 `Expr`。
    Expr(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // 字面量
    IntLit(i64),
    FloatLit(f64),
    BoolLit(bool),
    /// 字符字面量 `'a'`（v0.92）。值为该字符的 Unicode 码点。
    CharLit(i64),
    /// 字节字面量 `b'a'`（v0.98.2）。值为单个字节（0~255），与字符**语义不同**。
    ByteLit(i64),
    StringLit(String),
    /// 字符串插值（v1.00）：含至少一个 `${...}` 片段，例如 `"x=${v}"`。
    InterpLit(Vec<InterpPiece>),
    Ident(String),

    // 关键字
    Var,
    Let,
    Fn,
    Struct,
    If,
    Else,
    While,
    For,
    In,
    Break,
    Continue,
    Return,
    Try,
    Catch,
    Throw,
    /// `import "<包名>";`（v0.92，见 ARCHITECTURE.md）
    Import,
    // 多路复用（v0.91）：select { case recv(ch) as val { ... } default { ... } }
    Select,
    Case,
    Default,
    As,
    And,
    Or,
    Not,
    // 控制流增强（v1.00）
    Match, // match 多分支：match (x) { 1 => {...} 2 => {...} _ => {...} }
    Defer, // defer 资源清理：defer close(f); 离开作用域时按 LIFO 执行
    Do,    // do { ... } while (c); 循环体至少执行一次
    Step,  // for 步长关键字：for (i in 0..10 step 2)
    Const, // const 文件级常量：const N: int = 10;（编译期内联，不可重新赋值）
    Enum,  // enum 枚举：enum Color { Red, Green, Blue }（int-backed）
    /// `with` 资源作用域块（v1.02）：with 资源 as 名 { ... }
    With,
    /// `goto` 无条件跳转（v1.02）：goto 标签;
    Goto,
    /// `label` 标签定义（v1.02）：label 名字:
    Label,

    // 类型关键字
    IntT,
    FloatT,
    BoolT,
    CharT, // char（v0.92）
    ByteT, // byte（v0.98.2）：单字节无符号值 0~255
    BytesT, // bytes（v0.98.2）：不可变二进制缓冲区
    StringT,
    VoidT,
    MapT,
    FileT,

    // 运算符与标点
    Plus,
    Minus,
    Star,
    Slash,
    /// 取模运算符 `%`（与 * / 同级，整数/浮点均支持，CVE-2024-DIAL-005 新增）。
    Mod,
    // 复合赋值运算符（v1.00）：+= -= *= /= %=
    PlusEq, MinusEq, MulEq, DivEq, ModEq,
    // 自增 / 自减（v1.00）：x++ ++x x-- --x（仅作语句，作为表达式位置使用时编译期拒绝）
    PlusPlus, MinusMinus,
    // 位运算（v1.00，仅 int）：& | ^ ~ 以及移位 << >>
    Amp, Pipe, Caret, Tilde, Shl, Shr,
    // 三元表达式（v1.00）：cond ? a : b
    Question,
    Eq,       // ==
    Neq,      // !=
    Lt,       // <
    Gt,       // >
    Le,       // <=
    Ge,       // >=
    Assign,   // =
    /// `=>`：match 分支的箭头（v1.00）。必须与 `>=`(Ge) / `==`(Eq) 区分开。
    Arrow,    // =>
    Semicolon,
    Comma,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Colon,
    Dot,
    DotDot, // ..
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

        // 字节字面量 b'x'（v0.98.2）：单字节无符号值，取值 0~255。
        //
        // 刻意与字符字面量 `'x'` 分开：二者的**语义完全不同**——
        //   'x'   是 Unicode 码点（'中' 合法，值为 20013）
        //   b'x'  是**一个字节**（'中' 非法，因为 UTF-8 编码后是 3 个字节）
        // 用不同的语法形态（b 前缀）让「我写的是字节还是字符」在源码里一眼可辨，
        // 避免把 20013 塞进一个只装得下 255 的类型里。
        //
        // 放在标识符分支**之前**：否则 'b' 会被当成普通标识符的开头，
        // b'x' 会被拆成标识符 b + 字符字面量 'x'，报出一句莫名其妙的语法错误。
        if c == 'b' && i + 1 < chars.len() && chars[i + 1] == '\'' {
            let start_line = line;
            let start_col = col;
            i += 2; // 跳过 b 与 '
            col += 2;
            if i >= chars.len() {
                return Err(DialcError::new(
                    "字节字面量未闭合（缺少结束的单引号 '）",
                    start_line,
                    start_col,
                ));
            }
            let val = if chars[i] == '\\' {
                if i + 1 >= chars.len() {
                    return Err(DialcError::new(
                        "字节字面量中的转义序列不完整（反斜杠后缺少字符）",
                        start_line,
                        start_col,
                    ));
                }
                let nxt = chars[i + 1];
                let esc = match nxt {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '0' => '\0',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    _ => {
                        return Err(DialcError::new(
                            format!("字节字面量中无法识别的转义序列 '\\{}'", nxt),
                            start_line,
                            start_col,
                        ))
                    }
                };
                i += 2;
                col += 2;
                esc as i64
            } else {
                let ch = chars[i];
                i += 1;
                col += 1;
                ch as i64
            };
            if i >= chars.len() || chars[i] != '\'' {
                return Err(DialcError::new(
                    "字节字面量只能包含一个字节，且必须以单引号 ' 结束",
                    start_line,
                    start_col,
                ));
            }
            i += 1; // 跳过结束单引号
            col += 1;
            // 范围校验在**词法阶段**就做：一个字节装不下大于 255 的值。
            // 与 char 不同，char 可以是任意 Unicode 码点（到 0x10FFFF）。
            if val < 0 || val > 255 {
                return Err(DialcError::new(
                    format!(
                        "字节字面量的值超出范围 0~255（实际为 {}）；\
                         字节是单个 8 位无符号值，多字节字符请用字符字面量 '...'",
                        val
                    ),
                    start_line,
                    start_col,
                ));
            }
            push(TokenKind::ByteLit(val), start_line, start_col, &mut tokens);
            continue;
        }

        // 字符字面量 'a'（v0.92）
        //
        // 刻意与字符串字面量分开处理，而不是「切出一个子串再复用」：
        // 字符的合法内容只有一个码点（或一对转义），空 `''` 与多字符 `'ab'`
        // 都必须在**词法阶段**当场拒绝——它们不是「类型错了」，而是「根本不是一个字符」。
        if c == '\'' {
            let start_line = line;
            let start_col = col;
            i += 1;
            col += 1;
            if i >= chars.len() {
                return Err(DialcError::new(
                    "字符字面量未闭合（缺少结束的单引号 '）",
                    start_line,
                    start_col,
                ));
            }
            let code = if chars[i] == '\\' {
                // 转义序列：\n \t \r \0 \\ \' \"
                if i + 1 >= chars.len() {
                    return Err(DialcError::new(
                        "字符字面量中的转义序列不完整（反斜杠后缺少字符）",
                        start_line,
                        start_col,
                    ));
                }
                let nxt = chars[i + 1];
                let esc = match nxt {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '0' => '\0',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    _ => {
                        return Err(DialcError::new(
                            format!("字符字面量中无法识别的转义序列 '\\{}'", nxt),
                            start_line,
                            start_col,
                        ))
                    }
                };
                i += 2;
                col += 2;
                esc as i64
            } else {
                let ch = chars[i];
                i += 1;
                col += 1;
                ch as i64
            };
            if i >= chars.len() || chars[i] != '\'' {
                return Err(DialcError::new(
                    "字符字面量只能包含一个字符，且必须以单引号 ' 结束",
                    start_line,
                    start_col,
                ));
            }
            i += 1; // 跳过结束单引号
            col += 1;
            push(TokenKind::CharLit(code), start_line, start_col, &mut tokens);
            continue;
        }

        // 字符串（v1.00 起支持 `${...}` 插值）
        if c == '"' {
            let start_line = line;
            let start_col = col;
            i += 1;
            col += 1;
            let mut s = String::new();
            let mut pieces: Vec<InterpPiece> = Vec::new();
            while i < chars.len() && chars[i] != '"' {
                // 插值片段：${ 表达式 }。花括号按嵌套深度配对，允许 `${ f({...}) }` 之类写法。
                if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                    if !s.is_empty() {
                        pieces.push(InterpPiece::Text(std::mem::take(&mut s)));
                    }
                    i += 2;
                    col += 2;
                    let mut depth: i64 = 1;
                    let mut code = String::new();
                    while i < chars.len() && depth > 0 {
                        let ch = chars[i];
                        if ch == '{' {
                            depth += 1;
                            code.push(ch);
                        } else if ch == '}' {
                            depth -= 1;
                            if depth > 0 {
                                code.push(ch);
                            }
                        } else {
                            code.push(ch);
                        }
                        i += 1;
                        col += 1;
                    }
                    if depth != 0 {
                        return Err(DialcError::new(
                            "字符串插值 ${...} 未闭合",
                            start_line, start_col,
                        ));
                    }
                    if code.trim().is_empty() {
                        return Err(DialcError::new(
                            "字符串插值 ${...} 内不能为空",
                            start_line, start_col,
                        ));
                    }
                    pieces.push(InterpPiece::Expr(code));
                    continue;
                }
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
            if pieces.is_empty() {
                push(TokenKind::StringLit(s), start_line, start_col, &mut tokens);
            } else {
                if !s.is_empty() {
                    pieces.push(InterpPiece::Text(s));
                }
                push(TokenKind::InterpLit(pieces), start_line, start_col, &mut tokens);
            }
            continue;
        }

        // 数字（整数 / 浮点）。v1.00 起支持：
        //   - 进制前缀：0x/0X（十六）、0b/0B（二）、0o/0O（八）
        //   - 数字分隔符 `_`：1_000_000、0xFF_FF 都可（纯可读性，编译期剥离）
        //   - 浮点仅十进制支持小数点（十六/二/八进制不支持浮点字面量）
        if c.is_ascii_digit() || (c == '0' && i + 1 < chars.len() && matches!(chars[i + 1], 'x' | 'X' | 'b' | 'B' | 'o' | 'O')) {
            let start_line = line;
            let start_col = col;
            // 进制判定（默认十进制）
            let radix: u32 = if c == '0' {
                match chars[i + 1] {
                    'x' | 'X' => 16,
                    'b' | 'B' => 2,
                    'o' | 'O' => 8,
                    _ => 10,
                }
            } else {
                10
            };
            let mut num = String::new();
            if radix != 10 {
                // 跳过 '0' 与进制字母
                i += 2;
                col += 2;
            }
            // 收集数字（跳过下划线分隔符）
            while i < chars.len() {
                let d = chars[i];
                if d == '_' {
                    i += 1;
                    col += 1;
                    continue;
                }
                let valid = if radix == 16 {
                    d.is_ascii_hexdigit()
                } else {
                    d.is_ascii_digit()
                };
                if valid {
                    num.push(d);
                    i += 1;
                    col += 1;
                } else {
                    break;
                }
            }
            // 浮点：仅十进制允许小数点
            if radix == 10 && i < chars.len() && chars[i] == '.' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
                num.push('.');
                i += 1;
                col += 1;
                while i < chars.len() {
                    let d = chars[i];
                    if d == '_' {
                        i += 1;
                        col += 1;
                        continue;
                    }
                    if d.is_ascii_digit() {
                        num.push(d);
                        i += 1;
                        col += 1;
                    } else {
                        break;
                    }
                }
                let f: f64 = num
                    .parse()
                    .map_err(|_| DialcError::new("非法的浮点数字面量", start_line, start_col))?;
                push(TokenKind::FloatLit(f), start_line, start_col, &mut tokens);
                continue;
            }
            // 整数（十进制 / 十六 / 二 / 八）
            let n: i64 = if radix == 10 {
                num.parse()
                    .map_err(|_| DialcError::new("非法的整数字面量", start_line, start_col))?
            } else {
                i64::from_str_radix(&num, radix)
                    .map_err(|_| DialcError::new("非法的整数字面量（检查进制前缀与字符）", start_line, start_col))?
            };
            push(TokenKind::IntLit(n), start_line, start_col, &mut tokens);
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
                "for" => TokenKind::For,
                "in" => TokenKind::In,
                "break" => TokenKind::Break,
                "continue" => TokenKind::Continue,
                "return" => TokenKind::Return,
                "try" => TokenKind::Try,
                "catch" => TokenKind::Catch,
                "throw" => TokenKind::Throw,
                "import" => TokenKind::Import,
                "select" => TokenKind::Select,
                "case" => TokenKind::Case,
                "default" => TokenKind::Default,
                "as" => TokenKind::As,
                "and" => TokenKind::And,
                "or" => TokenKind::Or,
                "not" => TokenKind::Not,
                "match" => TokenKind::Match,
                "defer" => TokenKind::Defer,
                "do" => TokenKind::Do,
                "step" => TokenKind::Step,
                "const" => TokenKind::Const,
                "enum" => TokenKind::Enum,
                "with" => TokenKind::With,
                "goto" => TokenKind::Goto,
                "label" => TokenKind::Label,
                "true" => TokenKind::BoolLit(true),
                "false" => TokenKind::BoolLit(false),
                "struct" => TokenKind::Struct,
                "int" => TokenKind::IntT,
                "float" => TokenKind::FloatT,
                "bool" => TokenKind::BoolT,
                "char" => TokenKind::CharT,
                "byte" => TokenKind::ByteT,
                "bytes" => TokenKind::BytesT,
                "string" => TokenKind::StringT,
                "void" => TokenKind::VoidT,
                "map" => TokenKind::MapT,
                "file" => TokenKind::FileT,
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
            '+' => match peek {
                Some('=') => {
                    push(TokenKind::PlusEq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                Some('+') => {
                    push(TokenKind::PlusPlus, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Plus, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            '-' => match peek {
                Some('=') => {
                    push(TokenKind::MinusEq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                Some('-') => {
                    push(TokenKind::MinusMinus, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Minus, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            '*' => match peek {
                Some('=') => {
                    push(TokenKind::MulEq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Star, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            '/' => match peek {
                Some('=') => {
                    push(TokenKind::DivEq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Slash, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            '%' => match peek {
                Some('=') => {
                    push(TokenKind::ModEq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Mod, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
            '=' => match peek {
                Some('=') => {
                    push(TokenKind::Eq, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                Some('>') => {
                    push(TokenKind::Arrow, start_line, start_col, &mut tokens);
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
                Some('<') => {
                    push(TokenKind::Shl, start_line, start_col, &mut tokens);
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
                Some('>') => {
                    push(TokenKind::Shr, start_line, start_col, &mut tokens);
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
            '&' => {
                push(TokenKind::Amp, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '|' => {
                push(TokenKind::Pipe, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '^' => {
                push(TokenKind::Caret, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '~' => {
                push(TokenKind::Tilde, start_line, start_col, &mut tokens);
                i += 1;
                col += 1;
            }
            '?' => {
                push(TokenKind::Question, start_line, start_col, &mut tokens);
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
            '.' => match peek {
                Some('.') => {
                    push(TokenKind::DotDot, start_line, start_col, &mut tokens);
                    i += 2;
                    col += 2;
                }
                _ => {
                    push(TokenKind::Dot, start_line, start_col, &mut tokens);
                    i += 1;
                    col += 1;
                }
            },
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
