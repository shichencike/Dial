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
use crate::index::{self, IndexedFile, Scope, Symbol, SymbolKind, WorkspaceIndex};
use crate::lexer::{self, TokenKind};
use crate::package;
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

/// 客户端能力里我们**真正要区分**的那一项。
///
/// 为什么只记这一项：LSP 规范说得很明确——「若客户端既不支持 `documentChanges`、
/// 也不支持 `workspace.workspaceEdit.resourceOperations`，那么**只有 `changes` 里的
/// 纯 TextEdit 是被支持的**」。所以 `WorkspaceEdit` 该用哪个形状，**由客户端说了算**，
/// 不能我们单方面决定。
#[derive(Default, Clone, Copy)]
struct ClientCaps {
    /// `workspace.workspaceEdit.documentChanges` —— 客户端支持带版本的文档编辑。
    workspace_edit_document_changes: bool,
}

/// 已打开文档的版本号：`uri -> version`。
///
/// 为什么需要它：LSP 的 `WorkspaceEdit.documentChanges` 用
/// `OptionalVersionedTextDocumentIdentifier`，其中 `version` 要如实填客户端给我们的
/// 版本号（未打开的文件才填 null）。不跟踪版本就只能填 null——那是「降级」而非「照抄」。
type DocVersions = HashMap<String, i64>;

/// LSP 主循环：读 stdin 的 JSON-RPC 消息，处理后往 stdout 写响应。
pub fn run() -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    // 诊断与响应都写 stdout；日志一律走 stderr，避免污染协议流。
    let mut out = stdout.lock();
    let mut docs: Docs = HashMap::new();
    // 文档版本号（供 WorkspaceEdit.documentChanges 使用）
    let mut versions: DocVersions = HashMap::new();
    // 客户端能力（决定 WorkspaceEdit 用 changes 还是 documentChanges）
    let mut caps = ClientCaps::default();
    // 跨文件符号索引（v0.95）：当前打开文件 + import 的包。
    let mut ws: WorkspaceIndex = WorkspaceIndex::default();

    loop {
        match read_message(&mut reader)? {
            None => return Ok(()), // EOF：客户端断开
            Some(msg) => {
                let is_exit = handle(&msg, &mut docs, &mut versions, &mut caps, &mut ws, &mut out)?;
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
    versions: &mut DocVersions,
    caps: &mut ClientCaps,
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
            // 记下 `workspace.workspaceEdit.documentChanges`——规范要求 WorkspaceEdit 的
            // 形状**由客户端能力决定**，不能我们单方面只发一种。
            caps.workspace_edit_document_changes = client_caps
                .and_then(|c| c.pointer("/workspace/workspaceEdit/documentChanges"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
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
                    if let Some(v) = p.pointer("/textDocument/version").and_then(|v| v.as_i64()) {
                        versions.insert(uri.to_string(), v);
                    }
                    index_document(ws, uri, text); // 维护符号索引（v0.95）
                    publish_diagnostics(out, uri, text)?;
                }
            }
        }
        "textDocument/didChange" => {
            // v1.10：增量同步（change=2）。逐条应用 contentChanges：带 range 的按
            // LSP 行列（UTF-16 码元）打补丁，不带 range 的整篇替换——老客户端与
            // 冒烟脚本继续发全量文本也照常工作。
            // 仍然**不**在这里发诊断（避免打字刷屏）：诊断由 didOpen / didSave /
            // didChangeWatchedFiles 刷新；符号索引每次都更新，补全不滞后。
            if let Some(p) = msg.get("params") {
                let uri = p
                    .pointer("/textDocument/uri")
                    .and_then(|u| u.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(v) = p.pointer("/textDocument/version").and_then(|v| v.as_i64()) {
                    versions.insert(uri.clone(), v);
                }
                if let Some(changes) = p.pointer("/contentChanges").and_then(|c| c.as_array()) {
                    let mut cur = docs.get(&uri).cloned().unwrap_or_default();
                    for ch in changes {
                        cur = apply_change(&cur, ch);
                    }
                    docs.insert(uri.clone(), cur.clone());
                    index_document(ws, &uri, &cur);
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
                versions.remove(uri);
                ws.files.remove(uri); // 一并清掉索引，避免残留
                // 清空该文件的诊断，避免关掉后还留着红色波浪线
                let params = json!({ "uri": uri, "diagnostics": [] });
                notify(out, "textDocument/publishDiagnostics", params)?;
            }
        }

        // ---- v1.10：工作区文件感知 / 文档内高亮 ----
        // 客户端（含 VS Code 插件可配的 watcher）在磁盘文件变化时发此通知。
        // 包 / 本地模块的改动会影响所有已打开文档的诊断，因此对所有 docs 重跑
        // 索引与诊断；通知不带 id，绝不回响应。
        "workspace/didChangeWatchedFiles" => {
            let touched = msg
                .pointer("/params/changes")
                .and_then(|c| c.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let _ = log_msg(
                out,
                3,
                &format!("[didChangeWatchedFiles] {} 个文件变更，重跑已打开文档诊断", touched),
            );
            let snapshot: Vec<(String, String)> =
                docs.iter().map(|(u, t)| (u.clone(), t.clone())).collect();
            for (u, t) in snapshot {
                index_document(ws, &u, &t);
                publish_diagnostics(out, &u, &t)?;
            }
        }
        "textDocument/documentHighlight" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let l = msg
                .pointer("/params/position/line")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let c = msg
                .pointer("/params/position/character")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let reply = document_highlight(docs, &uri, l, c);
            respond(out, id, json!(reply))?;
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

        // ---- 轮2：导航 ----
        "textDocument/documentSymbol" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let syms = build_document_symbols(ws, docs, &uri);
            respond(out, id, json!(syms))?;
        }
        "workspace/symbol" => {
            let q = msg
                .pointer("/params/query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let items = workspace_symbols(ws, docs, &q);
            respond(out, id, json!(items))?;
        }
        "textDocument/references" => {
            let include_decl = msg
                .pointer("/params/context/includeDeclaration")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let items = pos
                .map(|(uri, line, character)| {
                    references_at(ws, docs, &uri, line, character, include_decl)
                })
                .unwrap_or_default();
            respond(out, id, json!(items))?;
        }
        "textDocument/prepareRename" => {
            let reply = pos
                .map(|(uri, line, character)| prepare_rename_at(ws, docs, &uri, line, character))
                .unwrap_or(Value::Null);
            respond(out, id, reply)?;
        }
        "textDocument/rename" => {
            let new_name = msg
                .pointer("/params/newName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reply = pos
                .map(|(uri, line, character)| {
                    rename_at(ws, docs, versions, caps, &uri, line, character, &new_name)
                })
                .unwrap_or(Value::Null);
            respond(out, id, reply)?;
        }
        "textDocument/signatureHelp" => {
            let reply = pos
                .map(|(uri, line, character)| signature_help_at(ws, docs, &uri, line, character))
                .unwrap_or(Value::Null);
            respond(out, id, reply)?;
        }

        // ---- 轮3：编写辅助 ----
        "textDocument/formatting" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let text = docs.get(&uri).cloned().unwrap_or_default();
            let edits = format_document(&text);
            respond(out, id, edits)?;
        }
        "textDocument/inlayHint" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let range = msg
                .get("params")
                .and_then(|p| p.get("range"))
                .cloned()
                .unwrap_or(Value::Null);
            let hints = inlay_hints(ws, docs, &uri, &range);
            respond(out, id, json!(hints))?;
        }

        // ---- 轮4：高级 ----
        "textDocument/codeAction" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let range = msg
                .pointer("/params/range")
                .cloned()
                .unwrap_or(Value::Null);
            let diags = msg
                .pointer("/params/context/diagnostics")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            let actions = code_action(&uri, &range, &diags);
            respond(out, id, json!(actions))?;
        }
        "textDocument/foldingRange" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let ranges = folding_ranges(ws, &uri);
            respond(out, id, json!(ranges))?;
        }
        "textDocument/selectionRange" => {
            // LSP 规范用 `positions`（**数组**，支持多选）；历史上我们也收单数 `position`。
            // 返回形状跟着请求走：数组进数组出，单数进单数出（老客户端不至于拿到数组而崩）。
            let multi = msg.pointer("/params/positions").and_then(|p| p.as_array()).is_some();
            let reply = match selection_positions(msg) {
                Some((uri, ps)) if ps.is_empty() => Value::Null,
                Some((uri, ps)) if multi => {
                    let arr: Vec<Value> = ps
                        .iter()
                        .map(|(l, c)| selection_range(ws, docs, &uri, *l, *c))
                        .collect();
                    json!(arr)
                }
                Some((uri, ps)) => selection_range(ws, docs, &uri, ps[0].0, ps[0].1),
                None => Value::Null,
            };
            respond(out, id, reply)?;
        }
        "textDocument/documentLink" => {
            let uri = msg
                .pointer("/params/textDocument/uri")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let links = document_links(ws, docs, &uri);
            respond(out, id, json!(links))?;
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
/// 判据是「**客户端有没有提到这一段**」，而不是「客户端有没有逐项点名」：
/// - 客户端**完全没发** capabilities（极老客户端）→ 默认全开。
/// - 客户端发了 `textDocument` 段 → 该段内逐项判断：没点名的项视为不支持，不广播。
/// - 客户端**压根没提** `textDocument` / `workspace` 段 → 它没表达任何偏好，此时全开。
///
/// 为什么「多广播」比「少广播」安全：客户端没声明某能力，多半只是**忘了填**
/// （不少客户端干脆发一个空的 `capabilities: {}`）。我们因此不广播，客户端就会
/// 安安静静地少一个功能——用户查不出原因；反过来多广播一个用不上的能力，
/// 客户端自己不会去调，零代价。
/// 文档同步与语义高亮是本项目必开项，无条件广播。
fn negotiate_capabilities(client: Option<&Value>) -> Value {
    let supports = |path: &str| -> bool {
        let c = match client {
            None => return true,
            Some(c) => c,
        };
        // path 形如 "/textDocument/hover"：父段 "/textDocument" 才是「客户端表态」的粒度。
        let (parent, leaf) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        if parent.is_empty() {
            return c.pointer(path).is_some();
        }
        match c.pointer(parent) {
            Some(section) if section.is_object() => section.get(leaf).is_some(),
            // 整段都没提到（或不是对象）→ 客户端没表达偏好 → 保守全开。
            _ => true,
        }
    };
    let mut caps = json!({
        // **必须是选项形式**，不能只写数字 1。
        // 规范对 `TextDocumentSyncOptions.save` 的原文是：
        //   "If present save notifications are sent to the server.
        //    If omitted the notification should not be sent."
        // 而纯数字形式（TextDocumentSyncKind）里**没有** `save` 这个字段——
        // 客户端据此就不会发 `textDocument/didSave`，而我们的诊断正是靠它刷新的。
        // 用数字形式的话，「保存即报诊断」只在自家插件里成立（插件无条件发 didSave），
        // 在别的客户端（VS Code 官方客户端等）上会**静默失效**。
        "textDocumentSync": {
            "openClose": true,      // 要收到 didOpen / didClose
            "change": 2,            // 2 = Incremental：按 range 打补丁；无 range 的全量 change 也兼容
            "save": { "includeText": false }  // 要收到 didSave（诊断靠它刷新）
        },
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
    if supports("/textDocument/documentSymbol") {
        caps["documentSymbolProvider"] = json!(true);
    }
    if supports("/workspace/symbol") {
        caps["workspaceSymbolProvider"] = json!(true);
    }
    if supports("/textDocument/references") {
        caps["referencesProvider"] = json!(true);
    }
    if supports("/textDocument/rename") {
        // prepareProvider=true：先发 prepareRename 让客户端确认可重命名区间。
        caps["renameProvider"] = json!({ "prepareProvider": true });
    }
    if supports("/textDocument/signatureHelp") {
        caps["signatureHelpProvider"] = json!({ "triggerCharacters": ["(", ","] });
    }
    if supports("/textDocument/formatting") {
        caps["documentFormattingProvider"] = json!(true);
    }
    if supports("/textDocument/inlayHint") {
        caps["inlayHintProvider"] = json!(true);
    }
    if supports("/textDocument/codeAction") {
        caps["codeActionProvider"] = json!(true);
    }
    if supports("/textDocument/foldingRange") {
        caps["foldingRangeProvider"] = json!(true);
    }
    if supports("/textDocument/selectionRange") {
        caps["selectionRangeProvider"] = json!(true);
    }
    if supports("/textDocument/documentLink") {
        caps["documentLinkProvider"] = json!(true);
    }
    // v1.10：文档内同名标识符高亮。无条件广播（与 semanticTokensProvider 同口径）：
    // 纯本地词法扫描，不依赖客户端额外能力；老客户端不请求也没有副作用。
    caps["documentHighlightProvider"] = json!(true);
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
    // 编译前端的过程日志（如「包 'demo' v1.0.0」）走 `window/logMessage` 通知，
    // 而不是 stdout 的裸文本——stdout 是协议流，混入裸文本会让客户端分帧错位。
    // 闭包借用 out，因此必须限制在块内，块结束后才能再次借用 out 发诊断。
    let diags = {
        let mut log = |m: &str| {
            let _ = log_msg(out, 3, m);
        };
        collect_diagnostics(text, &mut log)
    };
    let params = json!({ "uri": uri, "diagnostics": diags });
    notify(out, "textDocument/publishDiagnostics", params)
}

/// 跑前端并收集诊断（返回 LSP 的 Diagnostic 数组）。
///
/// `log` 是过程日志的出口——由调用方决定送到哪儿（LSP 里是 `window/logMessage`）。
/// 本函数**不直接**写 stdout：stdout 属于协议流，只能由分帧器统一写。
fn collect_diagnostics(text: &str, log: &mut dyn FnMut(&str)) -> Vec<Value> {
    let mut diags = Vec::new();

    // 1) 词法
    let tokens = match lexer::lex(text) {
        Ok(t) => t,
        Err(e) => {
            diags.push(diagnostic(&e, "dialc.lex"));
            return diags;
        }
    };
    // 2) 语法（阶段一：只为拿 import 列表）
    let probe = match parser::parse(tokens.clone()) {
        Ok(p) => p,
        Err(e) => {
            diags.push(diagnostic(&e, "dialc.parse"));
            return diags;
        }
    };

    // 2.5) 解析 import —— 先问「包装了没」。
    //
    // 缺包**必须报出来**：它是环境问题，用户看得见才能去装。
    // 而且此时继续做语义检查只会产出一堆「未定义的函数」——那是**后果**不是**原因**，
    // 报出来只会淹没真正要修的那一条。所以缺包时直接返回，只报这一条。
    //
    // （此前这里把 resolve_imports 的失败**静默吞掉**，于是「import 了没装的包」
    //   在编辑器里毫无提示——用户只能去命令行才知道少了什么。）
    if let Some(pkg) = crate::first_missing_package(&probe.imports) {
        diags.push(package_missing_diagnostic(&pkg, &tokens));
        return diags;
    }
    let (externs, _libs, opaque) = match crate::resolve_imports(&probe.imports, log) {
        Ok(r) => r,
        Err(_) => (Vec::new(), Vec::new(), Vec::new()),
    };

    // 3) 语义
    let mut program = match parser::parse_with(tokens, &opaque) {
        Ok(p) => p,
        Err(e) => {
            diags.push(diagnostic(&e, "dialc.parse"));
            return diags;
        }
    };
    if let Err(e) = semantic::analyze(&mut program, &externs) {
        diags.push(diagnostic(&e, "dialc.sema"));
    }
    diags
}

/// 「找不到已安装的包 'x'」的诊断。
///
/// - **错误码** `dialc.package.missing`：工具据此给「安装这个包」的 quickfix。
/// - **`data.package`**：把包名以**结构化**方式带上。LSP 的 diagnostic 允许携带任意
///   `data`，工具应优先读它——而不是去解析消息文本（措辞会变）。
/// - 位置定位到对应的 `import` 语句（靠重扫 token 流：`Import` + `StringLit` 一对）。
fn package_missing_diagnostic(pkg: &str, tokens: &[lexer::Token]) -> Value {
    let (line, col) = import_stmt_pos(pkg, tokens).unwrap_or((1, 1));
    // 走**统一**的诊断构造路径：错误自带更具体的码时，以自带的为准。
    let e = DialcError::with_code(
        "dialc.package.missing",
        crate::missing_package_message(pkg),
        line,
        col,
    );
    let mut d = diagnostic(&e, "dialc.package.missing");
    // 再补上结构化的包名，供 quickfix 直接取用（不必解析消息文本）。
    d["data"] = json!({ "package": pkg });
    d
}

/// 找到 `import "pkg";` 语句的位置（1 起的行列）。
///
/// `probe.imports` 只有包名、没有位置；而 import 语句在 token 流里是
/// `Import` + `StringLit` 一对，认起来很直接。
fn import_stmt_pos(pkg: &str, tokens: &[lexer::Token]) -> Option<(usize, usize)> {
    for (i, t) in tokens.iter().enumerate() {
        if !matches!(t.kind, TokenKind::Import) {
            continue;
        }
        if let Some(TokenKind::StringLit(s)) = tokens.get(i + 1).map(|t| &t.kind) {
            if s == pkg {
                return Some((t.line, t.col));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// v1.10 协议面补齐：增量同步 / 文件监视 / 文档内高亮
// ---------------------------------------------------------------------------

/// 把一条 `TextDocumentContentChangeEvent` 应用到当前文档。
/// 带 `range` 的按 LSP 行列打补丁（character 是 **UTF-16 码元**，靠
/// `index::utf16_to_char_col` 换回 Rust 的 char 列）；不带 `range` 的视为整篇
/// 替换——老客户端与冒烟脚本继续发完整文本也照常工作。
fn apply_change(doc: &str, change: &Value) -> String {
    let new = match change.pointer("/text").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return doc.to_string(),
    };
    let range = match change.get("range") {
        Some(r) => r,
        None => return new.to_string(),
    };
    let sl = range.pointer("/start/line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let sc = range.pointer("/start/character").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let el = range.pointer("/end/line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let ec = range.pointer("/end/character").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let start = byte_offset(doc, sl, sc);
    let end = byte_offset(doc, el, ec);
    let (start, end) = if start <= end { (start, end) } else { (end, start) };
    let mut out = String::with_capacity(doc.len() + new.len());
    out.push_str(&doc[..start]);
    out.push_str(new);
    out.push_str(&doc[end..]);
    out
}

/// LSP 的 `(line, utf16_character)`（均 0 起）→ 文档字节偏移。
fn byte_offset(doc: &str, line0: usize, utf16_col: usize) -> usize {
    let mut base = 0usize;
    for (i, line) in doc.split('\n').enumerate() {
        if i == line0 {
            let char1 = index::utf16_to_char_col(line, utf16_col); // 1 起
            let char_idx = char1.saturating_sub(1);
            return base
                + line
                    .char_indices()
                    .nth(char_idx)
                    .map(|(b, _)| b)
                    .unwrap_or(line.len());
        }
        base += line.len() + 1; // 含 '\n'
    }
    doc.len()
}

/// 文档内同名标识符高亮：复用编译器词法器，光标下标识符的所有 `Ident` token
/// 都返回 range（kind=1 / Text）。绝不自己写词法——边界必须与编译一致。
fn document_highlight(docs: &Docs, uri: &str, line0: usize, char0: usize) -> Vec<Value> {
    let text = match docs.get(uri) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let lt = lines.get(line0).copied().unwrap_or("");
    let char1 = index::utf16_to_char_col(lt, char0);
    let name = match index::identifier_at(text, line0 + 1, char1) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let tokens = match lexer::lex(text) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for t in tokens {
        if let TokenKind::Ident(s) = &t.kind {
            if *s == name {
                let l0 = t.line.saturating_sub(1);
                let c0 = char_col_to_utf16(lines.get(l0).copied().unwrap_or(""), t.col) as u32;
                out.push(json!({
                    "range": {
                        "start": { "line": l0, "character": c0 },
                        "end":   { "line": l0, "character": c0 + utf16_len(s) as u32 }
                    },
                    "kind": 1
                }));
            }
        }
    }
    out
}

/// 把 `DialcError` 转成 LSP 的 Diagnostic。
///
/// 行列转换是这里唯一容易出错的地方：dialc 的 line / col 都是 **1 起**，
/// 而 LSP 的 line / character 都是 **0 起**，且**未饱和**（可能越界）。
///
/// `phase_code` 是该阶段的**默认**错误码：错误自带更具体的码时以自带的为准
/// （如 `dialc.package.missing`），否则用阶段级码（`dialc.lex` / `dialc.parse` / `dialc.sema`）。
///
/// **错误码是给工具看的**：编辑器可按码过滤或忽略某类诊断，LSP 侧据此给出
/// 针对性 quickfix。消息文本会随版本调整，工具**不该**去匹配它——
/// 改一次措辞就废掉一个工具，这种坑我们不踩第二次。
///
/// 行列换算：dialc 的 line / col 都是 **1 起**，LSP 的 line / character 都是 **0 起**。
fn diagnostic(e: &DialcError, phase_code: &str) -> Value {
    let (line, col) = if e.line == 0 || e.col == 0 {
        (0usize, 0usize) // 无位置信息（很多语义错误是 0,0）——落在文件开头
    } else {
        (e.line - 1, e.col - 1)
    };
    // 稳定错误码与 CLI 诊断（src/diagnostic.rs）**共用同一张码表**：错误自带
    // 具体码时以自带为准，否则按消息推断，再不济才回退到阶段级码。
    let code = if e.code.is_empty() {
        crate::diagnostic::stable_code(&e.message, phase_code)
    } else {
        e.code.to_string()
    };
    json!({
        "range": {
            "start": { "line": line, "character": col },
            "end":   { "line": line, "character": col + 1 }
        },
        "severity": 1,          // 1 = Error
        "source": "dialc",
        "code": code,
        "codeDescription": { "href": "https://github.com/shichencike/Dial/blob/main/README.md" },
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
        | TokenKind::Not
        | TokenKind::Match
        | TokenKind::Defer
        | TokenKind::Do
        | TokenKind::Step | TokenKind::Const | TokenKind::Enum
        | TokenKind::With | TokenKind::Goto | TokenKind::Label
        | TokenKind::Extern | TokenKind::Link | TokenKind::Export | TokenKind::Packed
        | TokenKind::Volatile | TokenKind::Asm | TokenKind::Simd => T_KEYWORD,
        // true / false 虽是字面量，但视觉上按关键字处理更符合直觉
        TokenKind::BoolLit(_) => T_KEYWORD,
        // 类型关键字
        TokenKind::IntT | TokenKind::FloatT | TokenKind::Int32T | TokenKind::Float32T
        | TokenKind::BoolT | TokenKind::CharT
        | TokenKind::ByteT | TokenKind::BytesT
        | TokenKind::StringT | TokenKind::VoidT | TokenKind::MapT | TokenKind::FileT => T_TYPE,
        // 字面量
        TokenKind::StringLit(_) | TokenKind::InterpLit(_) => T_STRING,
        TokenKind::CharLit(_) => T_STRING, // 字符字面量按字符串配色（同样带引号）
        TokenKind::ByteLit(_) => T_NUMBER,  // 字节字面量按数字配色（它是数值，不是字符）
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
        | TokenKind::Ellipsis
        | TokenKind::LBracket
        | TokenKind::RBracket
        | TokenKind::Mod
        | TokenKind::PlusEq | TokenKind::MinusEq | TokenKind::MulEq | TokenKind::DivEq | TokenKind::ModEq
        | TokenKind::PlusPlus | TokenKind::MinusMinus
        | TokenKind::Amp | TokenKind::Pipe | TokenKind::Caret | TokenKind::Tilde
        | TokenKind::Shl | TokenKind::Shr | TokenKind::Question | TokenKind::Arrow
        | TokenKind::At => T_OPERATOR,
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
        TokenKind::StringLit(_) | TokenKind::InterpLit(_) => scan_quoted_len(lines, line, col, '"'),
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
        TokenKind::Match => "match",
        TokenKind::Defer => "defer",
        TokenKind::Do => "do",
        TokenKind::Step => "step",
        TokenKind::Const => "const",
        TokenKind::Enum => "enum",
        TokenKind::With => "with",
        TokenKind::Goto => "goto",
        TokenKind::Label => "label",
        TokenKind::Extern => "extern",
        TokenKind::Link => "link",
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
        TokenKind::Ellipsis => "...",
        TokenKind::LBracket => "[",
        TokenKind::RBracket => "]",
        TokenKind::Mod => "%",
        TokenKind::PlusEq => "+=",
        TokenKind::MinusEq => "-=",
        TokenKind::MulEq => "*=",
        TokenKind::DivEq => "/=",
        TokenKind::ModEq => "%=",
        TokenKind::PlusPlus => "++",
        TokenKind::MinusMinus => "--",
        TokenKind::Amp => "&",
        TokenKind::Pipe => "|",
        TokenKind::Caret => "^",
        TokenKind::Tilde => "~",
        TokenKind::Shl => "<<",
        TokenKind::Shr => ">>",
        TokenKind::Question => "?",
        TokenKind::Arrow => "=>",
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

/// 取 `textDocument/selectionRange` 的位置列表：`positions`（规范，数组）或 `position`（单数）。
fn selection_positions(msg: &Value) -> Option<(String, Vec<(u32, u32)>)> {
    let uri = msg
        .pointer("/params/textDocument/uri")
        .and_then(|u| u.as_str())?
        .to_string();
    if let Some(arr) = msg.pointer("/params/positions").and_then(|p| p.as_array()) {
        let mut out = Vec::with_capacity(arr.len());
        for p in arr {
            let line = p.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
            let character = p.get("character").and_then(|c| c.as_u64()).unwrap_or(0) as u32;
            out.push((line, character));
        }
        return Some((uri, out));
    }
    let line = msg.pointer("/params/position/line").and_then(|l| l.as_u64())? as u32;
    let character = msg
        .pointer("/params/position/character")
        .and_then(|c| c.as_u64())? as u32;
    Some((uri, vec![(line, character)]))
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
    let mut indexed = index::index_program(uri, &program, &program.imports);
    indexed.collect_references(text);
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

// ---------------------------------------------------------------------------
// 轮2：导航能力（大纲 / 工作区符号 / 引用 / 重命名 / 签名帮助）
// ---------------------------------------------------------------------------

/// 符号种类 → LSP SymbolKind 数值。
fn lsp_symbol_kind(k: SymbolKind) -> u32 {
    match k {
        SymbolKind::Function | SymbolKind::ExternFn => 12, // Function
        SymbolKind::Struct => 23,                         // Struct
        SymbolKind::OpaqueType => 22,                     // Class
        SymbolKind::Var | SymbolKind::Param => 13,        // Variable
    }
}

/// 文档大纲：顶层 fn/struct 为根节点，函数的形参与局部变量为子节点。
fn build_document_symbols(ws: &WorkspaceIndex, docs: &Docs, uri: &str) -> Vec<Value> {
    let file = match ws.files.get(uri) {
        Some(f) => f,
        None => return Vec::new(),
    };
    let text = docs.get(uri).cloned().unwrap_or_default();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    for &si in &file.scopes[file.top_scope].symbols {
        out.push(symbol_node(file, &lines, si));
    }
    out
}

/// 取某作用域**严格后代**作用域内的所有符号下标（供大纲子节点）。
fn descendant_symbols(file: &IndexedFile, scope_id: usize) -> Vec<usize> {
    let mut result = Vec::new();
    let mut stack = vec![scope_id];
    while let Some(s) = stack.pop() {
        for child in 0..file.scopes.len() {
            if file.scopes[child].parent == Some(s) {
                stack.push(child);
                result.extend(file.scopes[child].symbols.iter().copied());
            }
        }
    }
    result
}

/// 把一个符号渲染成 LSP 的 DocumentSymbol（递归子节点）。
fn symbol_node(file: &IndexedFile, lines: &[&str], si: usize) -> Value {
    let sym = &file.symbols[si];
    let l0 = (sym.span.0 as u32).saturating_sub(1);
    let line_text = lines.get(sym.span.0.saturating_sub(1)).copied().unwrap_or("");
    let (sc, ec) = char_span_to_lsp(line_text, sym.span.1, sym.span.3);
    let range = json!({
        "start": { "line": l0, "character": sc },
        "end": { "line": l0, "character": ec }
    });
    let mut children = Vec::new();
    // 只有函数有「自己的作用域」（函数体），其后代符号作为子节点。
    if let Some(&body) = file.fn_bodies.get(&si) {
        for cs in descendant_symbols(file, body) {
            children.push(symbol_node(file, lines, cs));
        }
    }
    json!({
        "name": sym.name,
        "kind": lsp_symbol_kind(sym.kind),
        "range": range,
        "selectionRange": range,
        "children": children
    })
}

/// 工作区符号搜索：跨所有已打开文件 + 已加载包，按名字子串模糊匹配。
fn workspace_symbols(ws: &WorkspaceIndex, docs: &Docs, q: &str) -> Vec<Value> {
    let ql = q.to_lowercase();
    let mut out = Vec::new();
    for (uri, file) in &ws.files {
        let text = docs.get(uri).cloned().unwrap_or_default();
        let lines: Vec<&str> = text.split('\n').collect();
        for &si in &file.scopes[file.top_scope].symbols {
            let sym = &file.symbols[si];
            if !matches!(
                sym.kind,
                SymbolKind::Function | SymbolKind::Struct | SymbolKind::ExternFn | SymbolKind::OpaqueType
            ) {
                continue;
            }
            if !ql.is_empty() && !sym.name.to_lowercase().contains(&ql) {
                continue;
            }
            let l0 = (sym.span.0 as u32).saturating_sub(1);
            let line_text = lines.get(sym.span.0.saturating_sub(1)).copied().unwrap_or("");
            let (sc, ec) = char_span_to_lsp(line_text, sym.span.1, sym.span.3);
            out.push(json!({
                "name": sym.name,
                "kind": lsp_symbol_kind(sym.kind),
                "location": {
                    "uri": uri,
                    "range": {
                        "start": { "line": l0, "character": sc },
                        "end": { "line": l0, "character": ec }
                    }
                }
            }));
        }
    }
    for (_pname, syms) in &ws.packages {
        for s in syms.fns.iter().chain(syms.opaque.iter()) {
            if !ql.is_empty() && !s.name.to_lowercase().contains(&ql) {
                continue;
            }
            let l0 = (s.span.0 as u32).saturating_sub(1);
            // 包符号的 .diali 文本我们不一定持有，但包名/符号名都是 ASCII，可直接用 char 列。
            let (sc, ec) = char_span_to_lsp("", s.span.1, s.span.3);
            out.push(json!({
                "name": s.name,
                "kind": lsp_symbol_kind(s.kind),
                "location": {
                    "uri": s.decl_uri.clone().unwrap_or_default(),
                    "range": {
                        "start": { "line": l0, "character": sc },
                        "end": { "line": l0, "character": ec }
                    }
                }
            }));
        }
    }
    out
}

/// 取光标下符号在 `symbols` 中的下标（仅文件内符号；包符号返回 None，因为包只读不可重命名）。
fn symbol_index_at_cursor(
    ws: &WorkspaceIndex,
    docs: &Docs,
    uri: &str,
    line: u32,
    character: u32,
) -> Option<usize> {
    let text = docs.get(uri)?;
    let lines: Vec<&str> = text.split('\n').collect();
    let line1 = line as usize + 1;
    let line_text = lines.get(line as usize).copied().unwrap_or("");
    let char1 = index::utf16_to_char_col(line_text, character as usize);
    let name = index::identifier_at(text, line1, char1)?;
    let indexed = ws.files.get(uri)?;
    let point = (line1, char1);
    if let Some(scope_id) = indexed.scope_at(point) {
        return indexed.lookup_local_index(scope_id, &name);
    }
    None
}

/// 查找引用：返回光标符号在文件内的所有使用位置（含声明，除非 includeDeclaration=false）。
fn references_at(
    ws: &WorkspaceIndex,
    docs: &Docs,
    uri: &str,
    line: u32,
    character: u32,
    include_decl: bool,
) -> Vec<Value> {
    let target = match symbol_index_at_cursor(ws, docs, uri, line, character) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let file = match ws.files.get(uri) {
        Some(f) => f,
        None => return Vec::new(),
    };
    let sym_span = file.symbols[target].span; // 声明位置，用于判断是否跳过声明
    let text = docs.get(uri).cloned().unwrap_or_default();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    for r in &file.references {
        if r.symbol_index != target {
            continue;
        }
        if !include_decl && r.span == sym_span {
            continue;
        }
        let l0 = (r.span.0 as u32).saturating_sub(1);
        let line_text = lines.get(r.span.0.saturating_sub(1)).copied().unwrap_or("");
        let (sc, ec) = char_span_to_lsp(line_text, r.span.1, r.span.3);
        out.push(json!({
            "uri": uri,
            "range": {
                "start": { "line": l0, "character": sc },
                "end": { "line": l0, "character": ec }
            }
        }));
    }
    out
}

/// 合法标识符判定：首字符字母或 `_`，后续字母/数字/`_`。
fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// 重命名：对光标符号的所有引用（含声明）生成跨位置编辑。仅允许合法标识符；包符号返回 null。
fn rename_at(
    ws: &WorkspaceIndex,
    docs: &Docs,
    versions: &DocVersions,
    caps: &ClientCaps,
    uri: &str,
    line: u32,
    character: u32,
    new_name: &str,
) -> Value {
    if new_name.is_empty() || !is_ident(new_name) {
        return Value::Null;
    }
    let target = match symbol_index_at_cursor(ws, docs, uri, line, character) {
        Some(i) => i,
        None => return Value::Null,
    };
    let file = match ws.files.get(uri) {
        Some(f) => f,
        None => return Value::Null,
    };
    let text = docs.get(uri).cloned().unwrap_or_default();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut edits = Vec::new();
    for r in &file.references {
        if r.symbol_index != target {
            continue;
        }
        let l0 = (r.span.0 as u32).saturating_sub(1);
        let line_text = lines.get(r.span.0.saturating_sub(1)).copied().unwrap_or("");
        let (sc, ec) = char_span_to_lsp(line_text, r.span.1, r.span.3);
        edits.push(json!({
            "range": {
                "start": { "line": l0, "character": sc },
                "end": { "line": l0, "character": ec }
            },
            "newText": new_name
        }));
    }
    if edits.is_empty() {
        return Value::Null;
    }
    // LSP 规范对 `WorkspaceEdit` 有两条规定，本实现**两条都照办**：
    //  1. `documentChanges` 里的 `TextDocumentEdit` 带**文档版本号**——版本号要如实填
    //     客户端给我们的（未打开的文件按规范填 null）。
    //  2. 「若客户端既不支持 `documentChanges` 也不支持
    //     `workspace.workspaceEdit.resourceOperations`，那么**只有 `changes` 里的纯
    //     TextEdit 是被支持的**」——所以用哪个形状**由客户端能力决定**，
    //     不能我们单方面只发一种。
    if caps.workspace_edit_document_changes {
        let version = match versions.get(uri) {
            Some(v) => json!(v),
            None => Value::Null,
        };
        json!({
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": version },
                "edits": edits
            }]
        })
    } else {
        json!({ "changes": { uri: edits } })
    }
}

/// prepareRename：返回可重命名的区间与占位名；不可重命名处返回 null。
fn prepare_rename_at(ws: &WorkspaceIndex, docs: &Docs, uri: &str, line: u32, character: u32) -> Value {
    let target = match symbol_index_at_cursor(ws, docs, uri, line, character) {
        Some(i) => i,
        None => return Value::Null,
    };
    let file = match ws.files.get(uri) {
        Some(f) => f,
        None => return Value::Null,
    };
    let sym = &file.symbols[target];
    let l0 = (sym.span.0 as u32).saturating_sub(1);
    let text = docs.get(uri).cloned().unwrap_or_default();
    let lines: Vec<&str> = text.split('\n').collect();
    let line_text = lines.get(sym.span.0.saturating_sub(1)).copied().unwrap_or("");
    let (sc, ec) = char_span_to_lsp(line_text, sym.span.1, sym.span.3);
    json!({
        "range": {
            "start": { "line": l0, "character": sc },
            "end": { "line": l0, "character": ec }
        },
        "placeholder": sym.name
    })
}

/// 从光标所在行推导出「当前正在调用的函数名」与「活动参数序号」。
///
/// 思路：从光标向前扫描，跳过配平的 `)`，找到与当前实参列表对应的那个 `(`，
/// 它前面的标识符就是被调用的函数名；`(` 与光标之间未配平的 `,` 数即活动参数序号。
fn call_context(line_text: &str, char1: usize) -> Option<(String, usize)> {
    let chars: Vec<char> = line_text.chars().collect();
    let end = char1; // 光标前（含最后一个紧邻字符）的字符数 = LSP character 列
    let mut depth = 0i32;
    let mut i = end;
    let mut call_open: Option<usize> = None;
    let mut active_param = 0usize;
    while i > 0 {
        i -= 1;
        let c = chars[i];
        if c == ')' {
            depth += 1;
        } else if c == '(' {
            if depth == 0 {
                call_open = Some(i);
                break;
            } else {
                depth -= 1;
            }
        } else if c == ',' && depth == 0 {
            active_param += 1;
        }
    }
    let op = call_open?;
    // 函数名：op 之前紧邻的标识符
    let mut j = op;
    let mut name = String::new();
    while j > 0 {
        let c = chars[j - 1];
        if c.is_alphanumeric() || c == '_' {
            name.insert(0, c);
            j -= 1;
        } else {
            break;
        }
    }
    if name.is_empty() {
        return None;
    }
    Some((name, active_param))
}

/// 签名帮助：在 `(` 后按光标前函数名查签名，返回 SignatureInformation。
fn signature_help_at(ws: &WorkspaceIndex, docs: &Docs, uri: &str, line: u32, character: u32) -> Value {
    let text = match docs.get(uri) {
        Some(t) => t.clone(),
        None => return Value::Null,
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let line1 = line as usize + 1;
    let line_text = lines.get(line as usize).copied().unwrap_or("");
    let char1 = index::utf16_to_char_col(line_text, character as usize);
    let (name, active_param) = match call_context(line_text, char1) {
        Some(x) => x,
        None => return Value::Null,
    };
    let indexed = match ws.files.get(uri) {
        Some(f) => f,
        None => return Value::Null,
    };
    // 先查文件内符号，再查 import 的包。
    let mut sig_params: Option<(String, Vec<String>)> = None;
    let point = (line1, char1);
    if let Some(scope_id) = indexed.scope_at(point) {
        if let Some(sidx) = indexed.lookup_local_index(scope_id, &name) {
            sig_params = indexed.signatures.get(&sidx).cloned();
        }
    }
    if sig_params.is_none() && indexed.lookup_package(ws, &name).is_some() {
        sig_params = ws.package_sigs.get(&name).cloned();
    }
    let (sig, params) = match sig_params {
        Some(x) => x,
        None => return Value::Null,
    };
    let param_infos: Vec<Value> = params.iter().map(|p| json!({ "label": p })).collect();
    json!({
        "signatures": [{
            "label": sig,
            "parameters": param_infos
        }],
        "activeSignature": 0,
        "activeParameter": active_param
    })
}

// ---------------------------------------------------------------------------
// 轮3：编写辅助（保守格式化 / 参数 inlay 提示）
// ---------------------------------------------------------------------------

/// 保守、非破坏性格式化：只做两件事 —— 去除每行行尾空白、确保文件末尾恰有一行换行。
///
/// **刻意不做**任何重排 / 换行的激进格式化（那会误伤对齐敏感的代码，且 Dial 没有官定代码风格）。
/// 缩进层级一律保留原样；只动行尾的空白与文件末尾的换行。返回覆盖全文的单个 TextEdit。
fn format_document(text: &str) -> Value {
    let segs: Vec<&str> = text.split('\n').collect();
    let n = segs.len();
    let mut result = String::new();
    for (i, line) in segs.iter().enumerate() {
        if i + 1 < n {
            // 非最后一段：原样（去行尾空白）后补换行
            result.push_str(line.trim_end());
            result.push('\n');
        } else {
            // 最后一段：若非空则保留并补一个换行（确保文件末尾换行）；为空（原文件以换行结尾）则不补
            if !line.is_empty() {
                result.push_str(line.trim_end());
                result.push('\n');
            }
        }
    }
    // 全文范围：起点 (0,0)，终点在最后一个 '\n' 之后那一行行首（或末行行尾）。
    let end_line = text.matches('\n').count() as u32;
    let end_char = if text.ends_with('\n') {
        0u32
    } else {
        segs.last().map(|l| utf16_len(l.trim_end()) as u32).unwrap_or(0)
    };
    // LSP 规范：`textDocument/formatting` 的结果是 **`TextEdit[]` 本身**，
    // 不是 `{ "edits": [...] }` 那种自定义包装——后者是私自定义形状，
    // 客户端按规范解析会拿到一个对象而不是数组。
    json!([{
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": end_line, "character": end_char }
        },
        "newText": result
    }])
}

/// 调用帧：扫描源码时维护 `(` 的配对与参数起始位置。
struct CallFrame {
    depth: i32,
    name: Option<String>,
    arg_starts: Vec<(u32, u32)>, // (0 起行, 0 起 UTF-16 列)
}

/// 参数 inlay 提示：在函数调用实参前标注形参名（如 `a: ` / `b: `）。
///
/// 仅做**安全**的一种：形参名来自已索引的签名，不做类型推断（索引独立于语义、半成品代码拿不到类型）。
/// 扫描 token 流，按括号深度配对调用，记录每个顶层实参的起始位置，再按签名里的形参名生成提示。
fn inlay_hints(ws: &WorkspaceIndex, docs: &Docs, uri: &str, range: &Value) -> Vec<Value> {
    let text = match docs.get(uri) {
        Some(t) => t.clone(),
        None => return Vec::new(),
    };
    let indexed = match ws.files.get(uri) {
        Some(f) => f,
        None => return Vec::new(),
    };
    let start_line = range
        .pointer("/start/line")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let end_line = range
        .pointer("/end/line")
        .and_then(|v| v.as_u64())
        .unwrap_or(text.split('\n').count() as u64) as usize;
    let tokens = match crate::lexer::lex(&text) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let line_utf16 = |line1: usize, char1: usize| -> u32 {
        let lt = lines.get(line1.saturating_sub(1)).copied().unwrap_or("");
        char_col_to_utf16(lt, char1) as u32
    };
    let mut out = Vec::new();
    let mut stack: Vec<CallFrame> = Vec::new();
    let mut depth: i32 = 0;
    let mut prev_ident: Option<String> = None;
    // 区分「函数声明 fn name(」与「函数调用 name(」：仅当「前一个 token 是标识符、
    // 且该标识符的前一个 token 是 fn 关键字」时，才是声明参数列表，不算调用（不给 inlay）。
    let mut prev_was_ident = false;
    let mut prev_was_fn = false;
    let mut last_ident_pre_was_fn = false;
    for tk in &tokens {
        let is_ident = matches!(tk.kind, TokenKind::Ident(_));
        let is_fn = matches!(tk.kind, TokenKind::Fn);
        match &tk.kind {
            TokenKind::Ident(name) => {
                last_ident_pre_was_fn = prev_was_fn;
                prev_ident = Some(name.clone());
            }
            TokenKind::LParen => {
                depth += 1;
                let is_call = !(prev_was_ident && last_ident_pre_was_fn);
                let l0 = (tk.line as u32).saturating_sub(1);
                let c0 = line_utf16(tk.line, tk.col + 1);
                let name = if is_call { prev_ident.take() } else { None };
                stack.push(CallFrame {
                    depth,
                    name,
                    arg_starts: vec![(l0, c0)], // 第一个实参起始 = '(' 之后
                });
                prev_ident = None;
            }
            TokenKind::RParen => {
                if let Some(f) = stack.pop() {
                    if let Some(ref name) = f.name {
                        let params = lookup_param_names(indexed, ws, name);
                        for (i, &(l0, c0)) in f.arg_starts.iter().enumerate() {
                            if i >= params.len() {
                                break;
                            }
                            let lr = l0 as usize;
                            if lr + 1 < start_line || lr > end_line {
                                continue;
                            }
                            out.push(json!({
                                "position": { "line": l0, "character": c0 },
                                "label": format!("{}: ", params[i]),
                                "kind": 2, // InlayHintKind.Parameter
                                "paddingLeft": true
                            }));
                        }
                    }
                }
                depth -= 1;
            }
            TokenKind::Comma => {
                if let Some(f) = stack.last_mut() {
                    if f.depth == depth {
                        let l0 = (tk.line as u32).saturating_sub(1);
                        let c0 = line_utf16(tk.line, tk.col + 1);
                        f.arg_starts.push((l0, c0)); // 后续实参起始 = ',' 之后
                    }
                }
            }
            _ => {}
        }
        prev_was_ident = is_ident;
        prev_was_fn = is_fn;
    }
    out
}

/// 按函数名查其形参名列表（仅名字，去掉类型部分）；文件内函数优先，其次包函数。
fn lookup_param_names(indexed: &IndexedFile, ws: &WorkspaceIndex, name: &str) -> Vec<String> {
    for (idx, sym) in indexed.symbols.iter().enumerate() {
        if sym.name == name && matches!(sym.kind, SymbolKind::Function) {
            if let Some((_, params)) = indexed.signatures.get(&idx) {
                return params.iter().map(|p| param_name(p)).collect();
            }
        }
    }
    if let Some((_, params)) = ws.package_sigs.get(name) {
        return params.iter().map(|p| param_name(p)).collect();
    }
    Vec::new()
}

/// "a: int" -> "a"（取形参名部分）。
fn param_name(label: &str) -> String {
    label.split(':').next().unwrap_or(label).trim().to_string()
}

// ---------------------------------------------------------------------------
// 轮4：高级（codeAction / foldingRange / selectionRange / documentLink）
// ---------------------------------------------------------------------------

/// 把 Scope 的包围盒换算成 LSP 范围。
fn scope_range(sc: &Scope, lines: &[&str]) -> Value {
    let sl = (sc.span.0 as u32).saturating_sub(1);
    let el = (sc.span.2 as u32).saturating_sub(1);
    let start_text = lines.get(sc.span.0.saturating_sub(1)).copied().unwrap_or("");
    let end_text = lines.get(sc.span.2.saturating_sub(1)).copied().unwrap_or("");
    let (s0, _) = char_span_to_lsp(start_text, sc.span.1, sc.span.1);
    let (_, e0) = char_span_to_lsp(end_text, sc.span.3, sc.span.3);
    json!({
        "start": { "line": sl, "character": s0 },
        "end": { "line": el, "character": e0 }
    })
}

/// 折叠区间：基于各作用域的包围盒，多行作用域各产生一个 region 折叠。
fn folding_ranges(ws: &WorkspaceIndex, uri: &str) -> Vec<Value> {
    let file = match ws.files.get(uri) {
        Some(f) => f,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for sc in &file.scopes {
        // 不折叠整个文件（top_scope 覆盖全文），单行作用域也不值得折叠。
        if sc.id == file.top_scope {
            continue;
        }
        let (sl, _, el, _) = sc.span;
        if sl == 0 || el == 0 || sl == el {
            continue;
        }
        out.push(json!({
            "startLine": (sl as u32).saturating_sub(1),
            "endLine": (el as u32).saturating_sub(1),
            "kind": "region"
        }));
    }
    out
}

/// 选择范围：从光标所在最内层作用域逐级向外扩展，构成嵌套的 SelectionRange 链。
fn selection_range(
    ws: &WorkspaceIndex,
    docs: &Docs,
    uri: &str,
    line: u32,
    character: u32,
) -> Value {
    let file = match ws.files.get(uri) {
        Some(f) => f,
        None => return Value::Null,
    };
    let text = docs.get(uri).cloned().unwrap_or_default();
    let lines: Vec<&str> = text.split('\n').collect();
    let line1 = line as usize + 1;
    let line_text = lines.get(line as usize).copied().unwrap_or("");
    let char1 = index::utf16_to_char_col(line_text, character as usize);
    let point = (line1, char1);

    // 收集从最内层到最外层的 scope id 链。
    let mut chain: Vec<usize> = Vec::new();
    if let Some(scope_id) = file.scope_at(point) {
        let mut cur = Some(scope_id);
        while let Some(id) = cur {
            chain.push(id);
            cur = file.scopes.get(id).and_then(|s| s.parent);
        }
    }
    // 没有任何作用域命中（光标在顶层空白）：退回整个文件（top_scope）。
    if chain.is_empty() {
        if let Some(sc) = file.scopes.get(file.top_scope) {
            return json!({ "range": scope_range(sc, &lines) });
        }
        return Value::Null;
    }
    // 由外到内构建，使内层的 parent 指向上一层。
    let mut node: Value = Value::Null;
    for &sid in chain.iter().rev() {
        let sc = &file.scopes[sid];
        node = json!({ "range": scope_range(sc, &lines), "parent": node });
    }
    node
}

/// 从诊断文案里抽取「找不到的包名」：`找不到已安装的包 '<name>'`。
fn extract_missing_pkg(msg: &str) -> Option<String> {
    let marker = "找不到已安装的包 '";
    if let Some(pos) = msg.find(marker) {
        let rest = &msg[pos + marker.len()..];
        if let Some(end) = rest.find('\'') {
            let name = &rest[..end];
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// 代码动作（安全 quickfix）：对诊断提供「安装缺失包」或「重新构建」命令。
/// 只发 Command（不擅自改代码），避免误伤；契合计划里「安全项」的约束。
fn code_action(_uri: &str, _range: &Value, diagnostics: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for d in diagnostics {
        let code = d.get("code").and_then(|c| c.as_str()).unwrap_or("");
        let msg = d.get("message").and_then(|m| m.as_str()).unwrap_or("");
        // 包名优先取**结构化**的 `data.package`；客户端没保留 data 时才回退到解析消息文本。
        let pkg = d
            .pointer("/data/package")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string())
            .or_else(|| extract_missing_pkg(msg));
        if code == "dialc.package.missing" {
            if let Some(pkg) = pkg {
                out.push(json!({
                    "title": format!("安装包 '{}'（dialc install）", pkg),
                    "kind": "quickfix",
                    "command": {
                        "title": format!("dialc install {}", pkg),
                        "command": "dialc.install",
                        "arguments": [pkg]
                    }
                }));
                continue;
            }
        }
        out.push(json!({
            "title": "在终端运行 dialc build 查看完整错误",
            "kind": "quickfix",
            "command": {
                "title": "dialc build",
                "command": "dialc.build",
                "arguments": []
            }
        }));
    }
    out
}

/// 把包名解析为其 `.diali` 接口文件的 file:// 路径（供 documentLink 跳转）。
fn resolve_pkg_iface(name: &str) -> Option<String> {
    if let Some((dir, manifest)) = package::find_installed(name).ok().flatten() {
        if let Ok(p) = package::verify_iface(&dir, &manifest) {
            return Some(format!("file://{}", p.display()));
        }
    }
    None
}

/// 文档链接：`import "pkg";` 语句的字符串字面量指向该包的 `.diali` 接口文件。
fn document_links(ws: &mut WorkspaceIndex, docs: &Docs, uri: &str) -> Vec<Value> {
    let text = match docs.get(uri) {
        Some(t) => t.clone(),
        None => return Vec::new(),
    };
    let tokens = match crate::lexer::lex(&text) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let n = tokens.len();
    let mut i = 0;
    while i < n {
        if matches!(tokens[i].kind, TokenKind::Import) && i + 1 < n {
            if let TokenKind::StringLit(inner) = &tokens[i + 1].kind {
                let pkg = inner.clone();
                let lt = &tokens[i + 1];
                let start_char = char_col_to_utf16(
                    lines.get(lt.line.saturating_sub(1)).copied().unwrap_or(""),
                    lt.col,
                ) as u32;
                let end_char = start_char + utf16_len(inner) as u32 + 2; // 含两侧引号
                // 顺手确保包已载入索引（不影响链接生成）
                let _ = index::ensure_package(ws, &pkg);
                if let Some(target) = resolve_pkg_iface(&pkg) {
                    out.push(json!({
                        "range": {
                            "start": { "line": (lt.line as u32).saturating_sub(1), "character": start_char },
                            "end": { "line": (lt.line as u32).saturating_sub(1), "character": end_char }
                        },
                        "target": target,
                        "tooltip": format!("跳到包 {} 的接口签名 (.diali)", pkg)
                    }));
                }
            }
            i += 2; // 跳过 import 与字符串字面量
            continue;
        }
        i += 1;
    }
    out
}
