//! `dialc fmt` 的间距规范化器（v1.10 强化）。
//!
//! 旧版只敢重排行首缩进，用户嫌「太差」。本模块在**不牺牲安全契约**的前提下
//! 把间距也规范化：运算符两侧、逗号/分号/冒号后、关键字与 `(` 之间、
//! 大括号两侧、成员访问与调用括号不打空格。
//!
//! 三条硬契约（由 `cmd_fmt` 当场校验，任一不满足就拒绝写入）：
//!   1. **token 序列完全一致**：`lexer::lex` 的 `kind` 序列前后逐项相等；
//!   2. **字面量与注释原文完全一致**：`protected_spans()` 提取的原文序列前后相等；
//!   3. **幂等**：`format(format(x)) == format(x)`。
//!
//! 实现手法：把每行扫成 `Word / Op / Raw` 三种片段，空白只作为「片段间是否补
//! 一格」的决策依据，绝不改写片段内部。字符串 / 字符 / 字节字面量与行注释、
//! 跨行块注释一律作为 `Raw` 原样保留，因此格式化永远不会改到注释内容或
//! 字符串里的空格。缩进沿用旧的 `brace_delta` 深度算法。

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Word,
    Op,
    /// 字符串 / 字符 / 字节字面量、行注释、块注释：原文不动。
    Raw,
}

struct Piece {
    text: String,
    kind: Kind,
    /// 一元前缀运算符（`-` / `!` / `~` / `&` / `*`）：后面紧跟操作数，不补空格。
    prefix: bool,
}

impl Piece {
    fn raw(chars: &[char]) -> Self {
        Piece {
            text: chars.iter().collect(),
            kind: Kind::Raw,
            prefix: false,
        }
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || (c as u32) > 127
}

/// 多字符运算符优先，保证 `>>` / `==` / `...` 不被拆成两个片段。
const OPS: &[&str] = &[
    "<<=", ">>=", "...", "==", "!=", "<=", ">=", "&&", "||", "<<", ">>", "+=", "-=", "*=",
    "/=", "%=", "&=", "|=", "^=", "->", "::", "+", "-", "*", "/", "%", "=", "<", ">", "!",
    "~", "&", "|", "^", ".", ",", ";", ":", "(", ")", "[", "]", "{", "}", "?", "@",
];

fn op_at(chars: &[char], i: usize) -> Option<&'static str> {
    for op in OPS {
        let n = op.chars().count();
        if i + n <= chars.len() && chars[i..i + n].iter().collect::<String>() == *op {
            return Some(*op);
        }
    }
    None
}

fn is_kw(s: &str) -> bool {
    matches!(
        s,
        "if" | "else" | "while" | "for" | "in" | "return" | "catch" | "throw" | "try"
            | "as" | "case" | "default" | "break" | "continue" | "fn" | "struct"
            | "var" | "let" | "import" | "select" | "test"
    )
}

/// 把一行扫成片段序列。`in_block` 跨行传递块注释状态。
fn scan_line(chars: &[char], in_block: &mut bool) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        // 块注释续行：整段原样收进 Raw，直到遇见 `*/`。
        if *in_block {
            let start = i;
            while i < chars.len() {
                if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '/' {
                    i += 2;
                    *in_block = false;
                    break;
                }
                i += 1;
            }
            out.push(Piece::raw(&chars[start..i]));
            continue;
        }

        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // 行注释：从 `//` 到行尾原样保留。
        if i + 1 < chars.len() && c == '/' && chars[i + 1] == '/' {
            out.push(Piece::raw(&chars[i..]));
            break;
        }
        // 块注释：从 `/*` 到 `*/`（可能跨行，状态留给下一行）。
        if i + 1 < chars.len() && c == '/' && chars[i + 1] == '*' {
            let start = i;
            i += 2;
            *in_block = true;
            while i < chars.len() {
                if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == '/' {
                    i += 2;
                    *in_block = false;
                    break;
                }
                i += 1;
            }
            out.push(Piece::raw(&chars[start..i]));
            continue;
        }
        // 字符串 / 字符字面量（含 `b'..'` / `b".."` 字节前缀）。
        if c == '"' || c == '\'' {
            let mut start = i;
            if i > 0 && chars[i - 1] == 'b' && (i == 1 || !is_ident(chars[i - 2])) {
                if out.last().map(|p| p.text.as_str()) == Some("b") {
                    out.pop();
                    start = i - 1;
                }
            }
            let quote = chars[i];
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2; // 跳过转义序列里的下一个字符
                    continue;
                }
                if chars[j] == quote {
                    j += 1;
                    break;
                }
                j += 1;
            }
            out.push(Piece::raw(&chars[start..j]));
            i = j;
            continue;
        }
        // 运算符（最长匹配）。
        if let Some(op) = op_at(chars, i) {
            let prefix = matches!(op, "-" | "!" | "~" | "&" | "*")
                && match out.last() {
                    None => true,
                    Some(p) => {
                        p.kind == Kind::Op && !matches!(p.text.as_str(), ")" | "]" | "}")
                    }
                };
            out.push(Piece {
                text: op.to_string(),
                kind: Kind::Op,
                prefix,
            });
            i += op.chars().count();
            continue;
        }
        // 普通词（连续的非空白、非运算符、非字面量起始字符）。
        let start = i;
        while i < chars.len()
            && !chars[i].is_whitespace()
            && !(chars[i] == '"' || chars[i] == '\'')
            && !(i + 1 < chars.len()
                && chars[i] == '/'
                && (chars[i + 1] == '/' || chars[i + 1] == '*'))
            && op_at(chars, i).is_none()
        {
            i += 1;
        }
        if i == start {
            i += 1; // 兜底：防死循环
        }
        out.push(Piece {
            text: chars[start..i].iter().collect(),
            kind: Kind::Word,
            prefix: false,
        });
    }
    out
}

/// 相邻两片段之间是否补恰好一个空格。
fn need_space(l: &Piece, r: &Piece) -> bool {
    let a = l.text.as_str();
    let b = r.text.as_str();
    // 开符号 / 成员访问 / 作用域后紧跟。
    if matches!(a, "(" | "[" | "." | "::") {
        return false;
    }
    // 一元前缀运算符紧跟操作数。
    if l.prefix && l.kind == Kind::Op {
        return false;
    }
    // 注释前补一格（`x = 1; // note`）。
    if r.kind == Kind::Raw && (b.starts_with("//") || b.starts_with("/*")) {
        return true;
    }
    // 闭符号、分隔符、成员访问、类型冒号前不补。
    if matches!(b, ")" | "]" | "," | ";" | "." | "::" | ":") {
        return false;
    }
    // 逗号 / 分号 / 类型冒号后补一格。
    if matches!(a, "," | ";" | ":") {
        return true;
    }
    // 大括号两侧补一格。
    if matches!(a, "{") || matches!(b, "{") || matches!(a, "}") || matches!(b, "}") {
        return true;
    }
    // 右操作数是前缀运算符（`x = -1` / `return -1`）。
    if r.prefix && r.kind == Kind::Op {
        return true;
    }
    // `(` 只在控制关键字后补一格；函数调用 `foo(x)` 不补。
    if b == "(" {
        return l.kind == Kind::Word && is_kw(a);
    }
    // 其余：二元运算符两侧、两个词之间、词与字面量之间。
    true
}

fn rebuild(pieces: &[Piece]) -> String {
    let mut out = String::new();
    for (idx, p) in pieces.iter().enumerate() {
        if idx > 0 && need_space(&pieces[idx - 1], p) {
            out.push(' ');
        }
        out.push_str(&p.text);
    }
    out
}

/// 统计一行里**字符串/字符字面量与行注释之外**的花括号净变化。
fn brace_delta(line: &str) -> i32 {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    let mut delta = 0i32;
    let mut in_str = false;
    let mut in_char = false;
    let mut escaped = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            break;
        }
        if c == '"' {
            in_str = true;
        } else if c == '\'' {
            in_char = true;
        } else if c == '{' {
            delta += 1;
        } else if c == '}' {
            delta -= 1;
        }
        i += 1;
    }
    delta
}

/// 保守但完整的格式化：重排缩进 + 规范化 token 间空白。
pub fn format_source(src: &str) -> String {
    if src.trim().is_empty() {
        return "\n".to_string();
    }
    let mut depth: i32 = 0;
    let mut out = String::new();
    let mut in_block = false;
    for raw in src.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        let leading_close = trimmed.starts_with('}') || trimmed.starts_with(')');
        let eff = (depth - if leading_close { 1 } else { 0 }).max(0) as usize;
        for _ in 0..eff {
            out.push_str("    ");
        }
        let chars: Vec<char> = trimmed.chars().collect();
        out.push_str(&rebuild(&scan_line(&chars, &mut in_block)));
        out.push('\n');
        depth = (depth + brace_delta(raw)).max(0);
    }
    out
}

/// 提取所有「原文不许动」的片段（字面量 / 行注释 / 块注释），按出现顺序。
/// `cmd_fmt` 用它做格式化前后一致性自检。
pub fn protected_spans(src: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut in_block = false;
    for raw in src.lines() {
        let chars: Vec<char> = raw.trim().chars().collect();
        for p in scan_line(&chars, &mut in_block) {
            if p.kind == Kind::Raw {
                spans.push(p.text);
            }
        }
    }
    spans
}
