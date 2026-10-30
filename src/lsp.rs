//! `dialc lsp` —— 极简 Language Server（v0.93 骨架）。
//!
//! 设计取舍（与 §20「代码编写纪律」一致）：
//!
//! - **不引入 LSP 框架**（tower-lsp 之类）。本骨架只需要「读一行 JSON、回一行 JSON」，
//!   框架带来的异步运行时与依赖树远超收益。JSON 用已有的 `serde_json`。
//! - **复用编译器本身做诊断**：词法 / 语法 / 语义全部走的是编译同一套代码，
//!   因此绝不会出现「编辑器说能过、编译器说不能过」的分裂。
//! - **只做两件事**：保存（及打开）时发布诊断、以及供语法高亮用的语义 token。
//!
//! 协议细节：标准输入输出，消息用 `Content-Length: N\r\n\r\n<JSON>` 分帧。

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::error::DialcError;
use crate::lexer::{self, TokenKind};
use crate::parser;
use crate::semantic;

/// LSP 语义 token 类型表。索引即 `tokenTypes` 里的下标，
/// 客户端（VS Code 等）用这个下标去查自己的配色。
///
/// 顺序**必须与**下面 `SEMANTIC_TOKEN_TYPES` 一致——改动其一就必须改另一个。
///
/// 刻意**没有** `comment`：dialc 的词法器直接丢弃 `//` 注释、不产 token，
/// 这里也就拿不到注释位置。注释的高亮交给客户端的 TextMate 语法（那才是它的主场），
/// 语义 token 只负责「编译器真正认识的那些东西」。
///
/// 同样**没有** `function`：区分变量与函数需要符号表，本骨架不做跨符号解析，
/// 标识符一律报 `variable`——宁可不标，也不标错。
const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "keyword",  // 0
    "string",   // 1
    "number",   // 2
    "operator", // 3
    "variable", // 4
    "type",     // 5
];

const T_KEYWORD: u32 = 0;
const T_STRING: u32 = 1;
const T_NUMBER: u32 = 2;
const T_OPERATOR: u32 = 3;
const T_VARIABLE: u32 = 4;
const T_TYPE: u32 = 5;

/// 已打开的文档：`uri -> 文本`。
type Docs = HashMap<String, String>;

/// LSP 主循环：读 stdin 的 JSON-RPC 消息，处理后往 stdout 写响应。
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    // 诊断与响应都写 stdout；日志一律走 stderr，避免污染协议流。
    let mut out = stdout.lock();
    let mut docs: Docs = HashMap::new();

    loop {
        match read_message(&mut reader)? {
            None => return Ok(()), // EOF：客户端断开
            Some(msg) => {
                let is_exit = handle(&msg, &mut docs, &mut out)?;
                if is_exit {
                    return Ok(());
                }
            }
        }
    }
}

/// 按 LSP 分帧读取一条消息：`Content-Length: N` 头部 + 空行 + N 字节的 JSON。
fn read_message<R: BufRead>(reader: &mut R) -> io::Result<Option<Value>> {
    // 头部
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // 空行 = 头部结束
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok();
        }
        // 其余头部（Content-Type 等）一律忽略
    }
    let len = match content_length {
        Some(l) => l,
        None => return Ok(None),
    };
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    match serde_json::from_slice::<Value>(&buf) {
        Ok(v) => Ok(Some(v)),
        Err(_) => Ok(None), // 非法 JSON：跳过，不让整个 server 崩掉
    }
}

/// 写一条消息（补上 Content-Length 头）。
fn write_message<W: Write>(out: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

/// 处理一条消息。返回 `true` 表示收到 `exit`，应结束进程。
fn handle<W: Write>(msg: &Value, docs: &mut Docs, out: &mut W) -> io::Result<bool> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();

    match method {
        // ---- 生命周期 ----
        "initialize" => {
            let result = json!({
                "capabilities": {
                    // 1 = Full：每次把整篇文档发过来。极简实现，不做增量同步。
                    "textDocumentSync": 1,
                    // 诊断由我们主动 push（didOpen / didSave 时）
                    "semanticTokensProvider": {
                        "legend": {
                            "tokenTypes": SEMANTIC_TOKEN_TYPES,
                            "tokenModifiers": []
                        },
                        "full": { "delta": false },
                        "range": false
                    }
                },
                "serverInfo": { "name": "dialc", "version": env!("CARGO_PKG_VERSION") }
            });
            respond(out, id, result)?;
        }
        "initialized" | "$/cancelRequest" => {}
        "shutdown" => respond(out, id, Value::Null)?,
        "exit" => return Ok(true),

        // ---- 文档同步 ----
        "textDocument/didOpen" => {
            if let Some(p) = msg.get("params") {
                if let (Some(uri), Some(text)) = (
                    p.pointer("/textDocument/uri").and_then(|u| u.as_str()),
                    p.pointer("/textDocument/text").and_then(|t| t.as_str()),
                ) {
                    docs.insert(uri.to_string(), text.to_string());
                    publish_diagnostics(out, uri, text)?;
                }
            }
        }
        "textDocument/didChange" => {
            // 记录最新文本，但**不**在这里发诊断——避免打字过程中刷屏。
            // 诊断在 didSave（以及 didOpen）时发布。
            if let Some(p) = msg.get("params") {
                if let (Some(uri), Some(text)) = (
                    p.pointer("/textDocument/uri").and_then(|u| u.as_str()),
                    p.pointer("/contentChanges/0/text").and_then(|t| t.as_str()),
                ) {
                    docs.insert(uri.to_string(), text.to_string());
                }
            }
        }
        "textDocument/didSave" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let text = match docs.get(&uri) {
                Some(t) => t.clone(),
                None => String::new(),
            };
            publish_diagnostics(out, &uri, &text)?;
        }
        "textDocument/didClose" => {
            if let Some(uri) = msg.pointer("/params/textDocument/uri").and_then(|u| u.as_str()) {
                docs.remove(uri);
                // 清空该文件的诊断，避免关掉后还留着红色波浪线
                let params = json!({ "uri": uri, "diagnostics": [] });
                notify(out, "textDocument/publishDiagnostics", params)?;
            }
        }

        // ---- 语法高亮 ----
        "textDocument/semanticTokens/full" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("");
            let text = docs.get(uri).cloned().unwrap_or_default();
            let data = semantic_tokens(&text);
            respond(out, id, json!({ "data": data }))?;
        }

        // 未知方法：静默忽略（LSP 允许客户端发送我们不认识的通知）
        _ => {
            // 若是**请求**（带 id）而不只是通知，必须回应，否则客户端会一直等待。
            if id.is_some() {
                respond(out, id, Value::Null)?;
            }
        }
    }
    Ok(false)
}

/// 发送成功响应。
fn respond<W: Write>(out: &mut W, id: Option<Value>, result: Value) -> io::Result<()> {
    // 无 id 的响应在协议里没有意义（那是通知），直接丢弃。
    let id = match id {
        Some(i) => i,
        None => return Ok(()),
    };
    write_message(
        out,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    )
}

/// 发送服务端主动通知。
fn notify<W: Write>(out: &mut W, method: &str, params: Value) -> io::Result<()> {
    write_message(
        out,
        &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
    )
}

// ---------------------------------------------------------------------------
// 诊断
// ---------------------------------------------------------------------------

/// 对文档跑一遍编译前端，把错误发布为诊断。
///
/// 三个阶段任一失败即停（后面的阶段依赖前面的结果），并**只报第一个错误**——
/// 这与 `dialc build` 的行为一致：第一个错误之后的位置信息往往已经失真，
/// 报一串连锁错误只会淹没真正的问题。
fn publish_diagnostics<W: Write>(out: &mut W, uri: &str, text: &str) -> io::Result<()> {
    let diags = collect_diagnostics(text);
    let params = json!({ "uri": uri, "diagnostics": diags });
    notify(out, "textDocument/publishDiagnostics", params)
}

/// 跑前端并收集诊断（返回 LSP 的 Diagnostic 数组）。
fn collect_diagnostics(text: &str) -> Vec<Value> {
    let mut diags = Vec::new();

    // 1) 词法
    let tokens = match lexer::lex(text) {
        Ok(t) => t,
        Err(e) => {
            diags.push(diagnostic(&e));
            return diags;
        }
    };
    // 2) 语法（阶段一：只为拿 import 列表）
    let probe = match parser::parse(tokens.clone()) {
        Ok(p) => p,
        Err(e) => {
            diags.push(diagnostic(&e));
            return diags;
        }
    };

    // 2.5) 解析 import —— 拿不到包不算是「代码错误」，
    //      而是环境问题，因此失败时**静默降级**为空外部函数表，
    //      继续做本地的检查（否则装个包之前连语法高亮都看不到错误）。
    let (externs, _libs, opaque) = match crate::resolve_imports(&probe.imports) {
        Ok(r) => r,
        Err(_) => (Vec::new(), Vec::new(), Vec::new()),
    };

    // 3) 语义
    let mut program = match parser::parse_with(tokens, &opaque) {
        Ok(p) => p,
        Err(e) => {
            diags.push(diagnostic(&e));
            return diags;
        }
    };
    if let Err(e) = semantic::analyze(&mut program, &externs) {
        diags.push(diagnostic(&e));
    }
    diags
}

/// 把 `DialcError` 转成 LSP 的 Diagnostic。
///
/// 行列转换是这里唯一容易出错的地方：dialc 的 line / col 都是 **1 起**，
/// 而 LSP 的 line / character 都是 **0 起**，且**未饱和**（可能越界）。
fn diagnostic(e: &DialcError) -> Value {
    let (line, col) = if e.line == 0 || e.col == 0 {
        (0usize, 0usize) // 无位置信息（很多语义错误是 0,0）——落在文件开头
    } else {
        (e.line - 1, e.col - 1)
    };
    json!({
        "range": {
            "start": { "line": line, "character": col },
            "end":   { "line": line, "character": col + 1 }
        },
        "severity": 1,          // 1 = Error
        "source": "dialc",
        "message": e.message
    })
}

// ---------------------------------------------------------------------------
// 语义 token（语法高亮）
// ---------------------------------------------------------------------------

/// 用**编译器自己的词法器**产出语义 token。
///
/// 刻意不另写一套高亮用的扫描器：一旦有两份词法，高亮与编译必然慢慢分道扬镳
/// （最典型的症状是「编辑器里是字符串，编译器说不是」）。
///
/// LSP 语义 token 是**差分编码**的 5 元组流：
///   `deltaLine, deltaStartChar, length, tokenType, tokenModifiers`
/// 其中前两项是相对**上一个 token** 的偏移，因此必须按序输出。
fn semantic_tokens(src: &str) -> Vec<u32> {
    let tokens = match lexer::lex(src) {
        Ok(t) => t,
        Err(_) => return Vec::new(), // 词法都过不去就别谈高亮了
    };
    let lines: Vec<&str> = src.split('\n').collect();

    let mut data: Vec<u32> = Vec::new();
    let mut prev_line = 0usize;
    let mut prev_col = 0usize;

    for tk in tokens {
        if matches!(tk.kind, TokenKind::EOF) {
            break;
        }
        let (kind_idx, len) = classify(&tk.kind, &lines, tk.line, tk.col);
        if len == 0 || kind_idx == u32::MAX {
            continue; // 无需高亮（例如空白）
        }
        let line0 = tk.line.saturating_sub(1);
        // 词法器按 char 记列，LSP 按 UTF-16 码元记列——行内有中文/emoji 时必须换算。
        let col0 = char_col_to_utf16(lines.get(line0).copied().unwrap_or(""), tk.col);

        data.push((line0 - prev_line) as u32);
        // 同一行时列是相对上一个 token 的偏移；换行了则从头算起。
        let delta_col = if line0 == prev_line {
            col0.saturating_sub(prev_col)
        } else {
            col0
        };
        data.push(delta_col as u32);
        data.push(len as u32);
        data.push(kind_idx);
        data.push(0); // tokenModifiers：本骨架不用

        prev_line = line0;
        prev_col = col0;
    }
    data
}

/// 判断一个 token 的语义类型与**长度**（UTF-16 码元数，LSP 的计量单位）。
///
/// `u32::MAX` 表示该 token 不参与高亮。
fn classify(kind: &TokenKind, lines: &[&str], line: usize, col: usize) -> (u32, usize) {
    let idx = match kind {
        // 关键字
        TokenKind::Var
        | TokenKind::Let
        | TokenKind::Fn
        | TokenKind::Struct
        | TokenKind::If
        | TokenKind::Else
        | TokenKind::While
        | TokenKind::For
        | TokenKind::In
        | TokenKind::Break
        | TokenKind::Continue
        | TokenKind::Return
        | TokenKind::Try
        | TokenKind::Catch
        | TokenKind::Throw
        | TokenKind::Select
        | TokenKind::Case
        | TokenKind::Default
        | TokenKind::As
        | TokenKind::Import
        | TokenKind::And
        | TokenKind::Or
        | TokenKind::Not => T_KEYWORD,
        // true / false 虽是字面量，但视觉上按关键字处理更符合直觉
        TokenKind::BoolLit(_) => T_KEYWORD,
        // 类型关键字
        TokenKind::IntT | TokenKind::FloatT | TokenKind::BoolT | TokenKind::CharT
        | TokenKind::StringT | TokenKind::VoidT | TokenKind::MapT | TokenKind::FileT => T_TYPE,
        // 字面量
        TokenKind::StringLit(_) => T_STRING,
        TokenKind::CharLit(_) => T_STRING, // 字符字面量按字符串配色（同样带引号）
        TokenKind::IntLit(_) | TokenKind::FloatLit(_) => T_NUMBER,
        // 标识符：无法在词法层区分变量 / 函数 / 类型名（那需要符号表），
        // 本骨架统一按 variable 处理——高亮够用，不做跨文件符号解析。
        TokenKind::Ident(_) => T_VARIABLE,
        // 运算符与标点
        TokenKind::Plus
        | TokenKind::Minus
        | TokenKind::Star
        | TokenKind::Slash
        | TokenKind::Eq
        | TokenKind::Neq
        | TokenKind::Lt
        | TokenKind::Gt
        | TokenKind::Le
        | TokenKind::Ge
        | TokenKind::Assign
        | TokenKind::Semicolon
        | TokenKind::Comma
        | TokenKind::LParen
        | TokenKind::RParen
        | TokenKind::LBrace
        | TokenKind::RBrace
        | TokenKind::Colon
        | TokenKind::Dot
        | TokenKind::DotDot
        | TokenKind::LBracket
        | TokenKind::RBracket => T_OPERATOR,
        TokenKind::EOF => return (u32::MAX, 0),
    };

    let len = token_len(kind, lines, line, col);
    (idx, len)
}

/// 求 token 在源码中的长度（UTF-16 码元数）。
///
/// 词法器只记录「起点行列」，不记录长度，因此这里按 token 种类反推：
/// - 带文本的种类（Ident、关键字、运算符）→ 用已知文本或固定长度；
/// - 字面量 → **回到源码里从起点扫到真正的结尾**（这样转义的 `\n`、多字节字符都算得准），
///   而不是用「格式化后的值」去猜长度（`IntLit(7)` 可能是源码里的 `007`，长度完全不同）。
fn token_len(kind: &TokenKind, lines: &[&str], line: usize, col: usize) -> usize {
    match kind {
        TokenKind::StringLit(_) => scan_quoted_len(lines, line, col, '"'),
        TokenKind::CharLit(_) => scan_quoted_len(lines, line, col, '\''),
        TokenKind::IntLit(_) | TokenKind::FloatLit(_) => scan_number_len(lines, line, col),
        TokenKind::Ident(s) => utf16_len(s),
        TokenKind::BoolLit(b) => if *b { 4 } else { 5 }, // true / false
        // 关键字与运算符：文本固定，长度由文本算
        other => match fixed_text(other) {
            Some(text) => utf16_len(text),
            None => 1,
        },
    }
}

/// 关键字 / 类型关键字 / 运算符的**源码文本**。
///
/// 长度一律由文本算，绝不手写 `Return => 6` 这样的常量：
/// 那种写法在关键字改名或新增时必然悄悄腐烂，而且编译器不会提醒你。
fn fixed_text(kind: &TokenKind) -> Option<&'static str> {
    let text = match kind {
        // ---- 关键字 ----
        TokenKind::Var => "var",
        TokenKind::Let => "let",
        TokenKind::Fn => "fn",
        TokenKind::Struct => "struct",
        TokenKind::If => "if",
        TokenKind::Else => "else",
        TokenKind::While => "while",
        TokenKind::For => "for",
        TokenKind::In => "in",
        TokenKind::Break => "break",
        TokenKind::Continue => "continue",
        TokenKind::Return => "return",
        TokenKind::Try => "try",
        TokenKind::Catch => "catch",
        TokenKind::Throw => "throw",
        TokenKind::Import => "import",
        TokenKind::Select => "select",
        TokenKind::Case => "case",
        TokenKind::Default => "default",
        TokenKind::As => "as",
        TokenKind::And => "and",
        TokenKind::Or => "or",
        TokenKind::Not => "not",
        // ---- 类型关键字 ----
        TokenKind::IntT => "int",
        TokenKind::FloatT => "float",
        TokenKind::BoolT => "bool",
        TokenKind::CharT => "char",
        TokenKind::StringT => "string",
        TokenKind::VoidT => "void",
        TokenKind::MapT => "map",
        TokenKind::FileT => "file",
        // ---- 运算符与标点 ----
        TokenKind::Plus => "+",
        TokenKind::Minus => "-",
        TokenKind::Star => "*",
        TokenKind::Slash => "/",
        TokenKind::Eq => "==",
        TokenKind::Neq => "!=",
        TokenKind::Lt => "<",
        TokenKind::Gt => ">",
        TokenKind::Le => "<=",
        TokenKind::Ge => ">=",
        TokenKind::Assign => "=",
        TokenKind::Semicolon => ";",
        TokenKind::Comma => ",",
        TokenKind::LParen => "(",
        TokenKind::RParen => ")",
        TokenKind::LBrace => "{",
        TokenKind::RBrace => "}",
        TokenKind::Colon => ":",
        TokenKind::Dot => ".",
        TokenKind::DotDot => "..",
        TokenKind::LBracket => "[",
        TokenKind::RBracket => "]",
        // 其余（字面量、标识符、EOF）没有固定文本，交给调用方另行处理
        _ => return None,
    };
    Some(text)
}

/// 从 (line, col) 起扫到一个**未转义**的 `quote` 为止（含两侧引号）。
fn scan_quoted_len(lines: &[&str], line: usize, col: usize, quote: char) -> usize {
    let line_idx = line.saturating_sub(1);
    let start = col.saturating_sub(1);
    let text = match lines.get(line_idx) {
        Some(l) => l,
        None => return 1,
    };
    let chars: Vec<char> = text.chars().collect();
    if start >= chars.len() || chars[start] != quote {
        return 1;
    }
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2; // 跳过被转义的字符
            continue;
        }
        if chars[i] == quote {
            return utf16_len(&chars[start..=i].iter().collect::<String>());
        }
        i += 1;
    }
    // 未闭合（词法器会在别处报错）：至少把开头那个引号标出来
    1
}

/// 数字字面量长度：从起点连续扫 `[0-9]` 与 `.`。
fn scan_number_len(lines: &[&str], line: usize, col: usize) -> usize {
    let line_idx = line.saturating_sub(1);
    let start = col.saturating_sub(1);
    let text = match lines.get(line_idx) {
        Some(l) => l,
        None => return 1,
    };
    let chars: Vec<char> = text.chars().collect();
    let mut n = 0usize;
    let mut i = start;
    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
        n += 1;
        i += 1;
    }
    if n == 0 {
        1
    } else {
        n
    }
}

/// LSP 以 UTF-16 码元计量列偏移，因此长度也要按 UTF-16 算。
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// dialc 词法器按 **char** 记列（`chars()` 逐个走），LSP 按 **UTF-16 码元**记列。
///
/// 纯 ASCII 时两者相同；一旦行里出现中文或 emoji，「第 N 个字符」与「第 N 个码元」
/// 就分道扬镳了——直接把 char 列当 UTF-16 列发出去，高亮整体会右偏。
fn char_col_to_utf16(line_text: &str, char_col: usize) -> usize {
    let take = char_col.saturating_sub(1); // 词法器 1 起
    let prefix: String = line_text.chars().take(take).collect();
    prefix.encode_utf16().count()
}
