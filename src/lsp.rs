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
use crate::index::{self, Symbol, SymbolKind, WorkspaceIndex};
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
    // 跨文件符号索引（v0.95）：当前打开文件 + import 的包。
    let mut ws: WorkspaceIndex = WorkspaceIndex::default();

    loop {
        match read_message(&mut reader)? {
            None => return Ok(()), // EOF：客户端断开
            Some(msg) => {
                let is_exit = handle(&msg, &mut docs, &mut ws, &mut out)?;
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
fn handle<W: Write>(
    msg: &Value,
    docs: &mut Docs,
    ws: &mut WorkspaceIndex,
    out: &mut W,
) -> io::Result<bool> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();

    // 把 (uri, line, character) 从 params 里取出（Hover / 定义 / 补全共用）。
    let pos = doc_position(msg);

    match method {
        // ---- 生命周期 ----
        "initialize" => {
            // 能力协商：老客户端不报能力时尽量全开；现代客户端会显式声明它支持什么，
            // 我们据此决定是否广播——客户端没说支持 hover，我们就别广播 hoverProvider，
            // 否则客户端收了声明却用不了，体验更糟。
            let client_caps = msg.pointer("/params/capabilities");
            let result = json!({
                "capabilities": negotiate_capabilities(client_caps),
                "serverInfo": { "name": "dialc", "version": env!("CARGO_PKG_VERSION") }
            });
            respond(out, id, result)?;
            let _ = log_msg(out, 3, &format!("dialc lsp {} 已就绪", env!("CARGO_PKG_VERSION")));
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
                    index_document(ws, uri, text); // 维护符号索引（v0.95）
                    publish_diagnostics(out, uri, text)?;
                }
            }
        }
        "textDocument/didChange" => {
            // 记录最新文本，但**不**在这里发诊断——避免打字过程中刷屏。
            // 诊断在 didSave（以及 didOpen）时发布。符号索引则**每次都更新**，
            // 这样补全在打字过程中也能给出当前可见的符号。
            if let Some(p) = msg.get("params") {
                if let (Some(uri), Some(text)) = (
                    p.pointer("/textDocument/uri").and_then(|u| u.as_str()),
                    p.pointer("/contentChanges/0/text").and_then(|t| t.as_str()),
                ) {
                    docs.insert(uri.to_string(), text.to_string());
                    index_document(ws, uri, text);
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
            index_document(ws, &uri, &text);
            publish_diagnostics(out, &uri, &text)?;
        }
        "textDocument/didClose" => {
            if let Some(uri) = msg.pointer("/params/textDocument/uri").and_then(|u| u.as_str()) {
                docs.remove(uri);
                ws.files.remove(uri); // 一并清掉索引，避免残留
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

        // ---- v0.95：悬停 / 跳转定义 / 补全 ----
        "textDocument/hover" => {
            let reply = pos
                .as_ref()
                .and_then(|(uri, line, character)| hover(ws, docs, uri, *line, *character));
            respond(out, id, reply.unwrap_or(Value::Null))?;
        }
        "textDocument/definition" => {
            let reply = pos
                .as_ref()
                .and_then(|(uri, line, character)| definition(ws, docs, uri, *line, *character));
            respond(out, id, reply.unwrap_or(Value::Null))?;
        }
        "textDocument/completion" => {
            let items = pos
                .map(|(uri, line, character)| completion(ws, docs, &uri, line, character))
                .unwrap_or_default();
            respond(
                out,
                id,
                json!({ "isIncomplete": false, "items": items }),
            )?;
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

/// 服务端的自诊断日志：经 `window/logMessage` 发给客户端（不污染协议流、不进 stdout）。
///
/// `type`：1=Error 2=Warning 3=Info 4=Log（LSP 标准）。
fn log_msg<W: Write>(out: &mut W, level: u8, message: &str) -> io::Result<()> {
    notify(
        out,
        "window/logMessage",
        json!({ "type": level, "message": message }),
    )
}

/// 能力协商：根据客户端声明的能力，决定本端广播哪些 capability。
///
/// - 客户端**完全没发** capabilities（极老客户端）→ 默认全开。
/// - 客户端**发了** capabilities 但没声明某项 → 那项不广播（避免客户端收了声明却用不了）。
/// 文档同步与语义高亮是本项目必开项，无条件广播。
fn negotiate_capabilities(client: Option<&Value>) -> Value {
    // 客户端显式声明了某能力路径才返回 true；没发 capabilities 时一律 true（全开）。
    let supports = |path: &str| -> bool {
        match client {
            None => true,
            Some(c) => c.pointer(path).is_some(),
        }
    };
    let mut caps = json!({
        // 1 = Full：每次把整篇文档发过来。极简实现，不做增量同步。
        "textDocumentSync": 1,
        "semanticTokensProvider": {
            "legend": {
                "tokenTypes": SEMANTIC_TOKEN_TYPES,
                "tokenModifiers": []
            },
            "full": { "delta": false },
            "range": false
        }
    });
    if supports("/textDocument/hover") {
        caps["hoverProvider"] = json!(true);
    }
    if supports("/textDocument/definition") {
        caps["definitionProvider"] = json!(true);
    }
    if supports("/textDocument/completion") {
        caps["completionProvider"] = json!({ "triggerCharacters": ["."] });
    }
    caps
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

// ---------------------------------------------------------------------------
// v0.95：符号索引辅助（Hover / 跳转定义 / 补全）
// ---------------------------------------------------------------------------

/// 从请求里取出 `(uri, 0 起行, 0 起 UTF-16 列)`。缺 position 时返回 None。
fn doc_position(msg: &Value) -> Option<(String, u32, u32)> {
    let uri = msg
        .pointer("/params/textDocument/uri")
        .and_then(|u| u.as_str())?
        .to_string();
    let line = msg.pointer("/params/position/line").and_then(|l| l.as_u64())? as u32;
    let character = msg
        .pointer("/params/position/character")
        .and_then(|c| c.as_u64())? as u32;
    Some((uri, line, character))
}

/// 把一份文档重新解析并写入 `ws.files`。解析失败（半成品代码）就移除旧索引。
fn index_document(ws: &mut WorkspaceIndex, uri: &str, text: &str) {
    // 1) 词法
    let tokens = match lexer::lex(text) {
        Ok(t) => t,
        Err(_) => {
            ws.files.remove(uri);
            return;
        }
    };
    // 2) 语法（只取 import 列表）
    let probe = match parser::parse(tokens.clone()) {
        Ok(p) => p,
        Err(_) => {
            ws.files.remove(uri);
            return;
        }
    };
    // 3) 载入 import 的包符号（与编译器同源；不往 stdout 打日志以免污染协议）
    let mut opaque = Vec::new();
    for imp in &probe.imports {
        if index::ensure_package(ws, imp) {
            if let Some(syms) = ws.packages.get(imp) {
                for s in &syms.opaque {
                    opaque.push(s.name.clone());
                }
            }
        }
    }
    // 4) 带包类型名重新解析 + 构建索引
    let program = match parser::parse_with(tokens, &opaque) {
        Ok(p) => p,
        Err(_) => {
            ws.files.remove(uri);
            return;
        }
    };
    let indexed = index::index_program(uri, &program, &program.imports);
    ws.files.insert(uri.to_string(), indexed);
}

/// 把 dialc 内部（1 起行, 1 起 char 列）的区间换算成 LSP 的 [start, end) 范围。
///
/// 行内包含 emoji 时 char 列与 UTF-16 列不同，因此要用行文本重算。
fn char_span_to_lsp(line_text: &str, c1: usize, c2: usize) -> (u32, u32) {
    let chars: Vec<char> = line_text.chars().collect();
    let to_utf16 = |c: usize| -> u32 {
        let take = c.saturating_sub(1).min(chars.len());
        chars.iter().take(take).map(|ch| ch.len_utf16()).sum::<usize>() as u32
    };
    (to_utf16(c1), to_utf16(c2))
}

/// 光标前的部分标识符（用于补全前缀过滤），按 UTF-16 列从行尾回扫。
fn partial_identifier(line_text: &str, utf16_col: u32) -> String {
    let chars: Vec<char> = line_text.chars().collect();
    let mut acc = 0usize;
    let mut idx = 0;
    for (i, ch) in chars.iter().enumerate() {
        let w = ch.len_utf16();
        if acc + w > utf16_col as usize {
            idx = i;
            break;
        }
        acc += w;
        idx = i + 1;
    }
    let mut s = String::new();
    let mut j = idx;
    while j > 0 {
        let ch = chars[j - 1];
        if ch.is_alphanumeric() || ch == '_' {
            s.insert(0, ch);
            j -= 1;
        } else {
            break;
        }
    }
    s
}

/// 符号种类的可读名（中文）。
fn kind_label(k: SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "函数",
        SymbolKind::Struct => "结构体",
        SymbolKind::Var => "变量",
        SymbolKind::Param => "形参",
        SymbolKind::OpaqueType => "不透明类型",
        SymbolKind::ExternFn => "外部函数",
    }
}

/// 补全/悬停用的类型详情字符串。
fn symbol_detail(s: &Symbol) -> String {
    match s.kind {
        SymbolKind::Function | SymbolKind::ExternFn => {
            format!("{} → {}", kind_label(s.kind), s.ty.display())
        }
        SymbolKind::Struct => "结构体".to_string(),
        SymbolKind::OpaqueType => "不透明类型（包内部布局对用户不可见）".to_string(),
        SymbolKind::Var | SymbolKind::Param => {
            format!("{}: {}", kind_label(s.kind), s.ty.display())
        }
    }
}

/// 取光标下的符号（文件内优先，否则查 import 的包）。
///
/// 返回克隆出的 `Symbol`：包含它的 span（指向定义处）、文档与 decl_uri（包符号指向 .diali）。
fn symbol_at_cursor<'a>(
    ws: &'a WorkspaceIndex,
    docs: &Docs,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<Symbol> {
    let text = docs.get(uri)?;
    let lines: Vec<&str> = text.split('\n').collect();
    let line1 = line as usize + 1;
    let line_text = lines.get(line as usize).copied().unwrap_or("");
    let char1 = index::utf16_to_char_col(line_text, character as usize);
    let name = index::identifier_at(&text, line1, char1)?;
    let indexed = ws.files.get(uri)?;
    let point = (line1, char1);
    if let Some(scope_id) = indexed.scope_at(point) {
        if let Some(s) = indexed.lookup_local(scope_id, &name) {
            return Some(s.clone());
        }
    }
    indexed.lookup_package(ws, &name).cloned()
}

/// Hover：返回 Markdown 内容（含类型与文档）。
fn hover(ws: &WorkspaceIndex, docs: &Docs, uri: &str, line: u32, character: u32) -> Option<Value> {
    let s = symbol_at_cursor(ws, docs, uri, line, character)?;
    let mut md = format!("**{}** — {}\n\n", s.name, kind_label(s.kind));
    md.push_str(&symbol_detail(&s));
    md.push('\n');
    if !s.doc.is_empty() {
        md.push('\n');
        md.push_str(&s.doc.join("\n"));
    }
    Some(json!({ "contents": { "kind": "markdown", "value": md } }))
}

/// 跳转定义：返回 Location（文件内符号指向本文件；包符号指向 .diali）。
fn definition(
    ws: &WorkspaceIndex,
    docs: &Docs,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<Value> {
    let s = symbol_at_cursor(ws, docs, uri, line, character)?;
    if let Some(decl_uri) = &s.decl_uri {
        // 包符号：跳到 .diali 的声明行（列定为行首）。
        // 这里不持有 .diali 文本，行内精确定位交由编辑器自行滚到该行。
        let l0 = (s.span.0 as u32).saturating_sub(1);
        return Some(json!({
            "uri": decl_uri,
            "range": { "start": { "line": l0, "character": 0 }, "end": { "line": l0, "character": 0 } }
        }));
    }
    // 文件内符号：精确区间（带 UTF-16 列换算）。
    let text = docs.get(uri).cloned().unwrap_or_default();
    let lines: Vec<&str> = text.split('\n').collect();
    let line_text = lines
        .get((s.span.0 as u32).saturating_sub(1) as usize)
        .copied()
        .unwrap_or("");
    let (sc, ec) = char_span_to_lsp(line_text, s.span.1, s.span.3);
    let l0 = (s.span.0 as u32).saturating_sub(1);
    Some(json!({
        "uri": uri,
        "range": { "start": { "line": l0, "character": sc }, "end": { "line": l0, "character": ec } }
    }))
}

/// 补全：收集光标处可见的全部符号，按已输入前缀过滤。
///
/// 与 Hover / 定义不同，补全**不要求光标下方必须是个标识符**——
/// 光标在空白上（例如刚敲完 `(`）也应该能给出当前可见的符号，
/// 因此这里直接从行文本推算前缀，再用 `scope_at` 定位作用域，不经由 `symbol_at_cursor`。
fn completion(ws: &WorkspaceIndex, docs: &Docs, uri: &str, line: u32, character: u32) -> Vec<Value> {
    let text = match docs.get(uri) {
        Some(t) => t.clone(),
        None => return Vec::new(),
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let line1 = line as usize + 1;
    let line_text = lines.get(line as usize).copied().unwrap_or("");
    let char1 = index::utf16_to_char_col(line_text, character as usize);
    let prefix = partial_identifier(line_text, character);
    // 精确替换范围：从「已输入前缀的起点」一直到光标，整段替换为补全项。
    // 这样无论前缀打到一半的哪几个字母，都不会出现「旧实现里把名字重复拼在后面」的 bug。
    // 没有前缀时范围收缩到光标处，等价于正常插入。
    let pre_utf16 = utf16_len(&prefix) as u32;
    let start_char = character.saturating_sub(pre_utf16);
    let replace_range = json!({
        "start": { "line": line, "character": start_char },
        "end": { "line": line, "character": character }
    });

    let indexed = match ws.files.get(uri) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let scope_id = indexed.scope_at((line1, char1)).unwrap_or(indexed.top_scope);
    let mut items = Vec::new();
    for s in indexed.visible_symbols(ws, scope_id) {
        if !prefix.is_empty() && !s.name.starts_with(&prefix) {
            continue;
        }
        let doc_val = if s.doc.is_empty() {
            Value::Null
        } else {
            json!({ "kind": "markdown", "value": s.doc.join("\n") })
        };
        // 排序：函数 / 外部函数最前，结构体 / 不透明类型次之，变量 / 形参最后，
        // 让「最可能被调用」的入口排在最上面。
        let sort_prefix: &str = match s.kind {
            SymbolKind::Function | SymbolKind::ExternFn => "0",
            SymbolKind::Struct | SymbolKind::OpaqueType => "1",
            SymbolKind::Var | SymbolKind::Param => "2",
        };
        items.push(json!({
            "label": s.name,
            "kind": s.kind.completion_kind(),
            "detail": symbol_detail(&s),
            "documentation": doc_val,
            "sortText": format!("{}_{}", sort_prefix, s.name),
            "textEdit": { "range": replace_range, "newText": s.name },
            "insertText": s.name
        }));
    }
    items
}
