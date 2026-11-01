// Dial 的 VS Code 扩展（v0.94 骨架）。
//
// 【为什么不用 vscode-languageclient】
// 官方客户端库当然更完备，但它要 npm install 一整棵依赖树，
// 而这个扩展真正要做的只有 4 件事：起进程、转发文档变化、收诊断、转发语义 token。
// 自己写这 200 行，换来的好处是：把目录拷进 ~/.vscode/extensions 就能用，
// 不需要 Node、不需要 npm、不需要联网。对一门语言的第一版编辑器支持来说，
// 「装得上」比「功能全」重要得多。
//
// 【唯一需要小心的两处】
//   1. stdout 是协议通道：分帧必须严格按 Content-Length 读，绝不能按行读
//      （JSON 内部可能含裸的 \r\n，按行读会切碎消息）。
//   2. 语义 token 是差分编码的：服务端给的是相对偏移，
//      必须先还原成绝对行列，再交给 SemanticTokensBuilder 让它重新差分。

const vscode = require("vscode");
const { spawn } = require("child_process");

// 必须与 dialc lsp 的 SEMANTIC_TOKEN_TYPES 逐项一致（顺序即索引）。
// 服务端在 initialize 的 capability 里也会把这份表发回来，那份才是权威——
// 这里只用于构造 VS Code 要求的 legend 对象。
const TOKEN_TYPES = ["keyword", "string", "number", "operator", "variable", "type"];
const TOKEN_MODIFIERS = [];

// LSP 的 Range 与 VS Code 的 Range 字段同名同义，直接转。
function toRange(r) {
  return new vscode.Range(r.start.line, r.start.character, r.end.line, r.end.character);
}

// LSP 的 SymbolKind 从 1 起（File=1），VS Code 的 SymbolKind 枚举从 0 起 —— 差一位。
function toSymbolKind(k) {
  return typeof k === "number" && k >= 1 ? k - 1 : vscode.SymbolKind.Variable;
}

// LSP 的 FoldingRangeKind 是字符串，VS Code 的是枚举。
const FOLDING_KINDS = {
  comment: 1, // vscode.FoldingRangeKind.Comment
  imports: 2, // vscode.FoldingRangeKind.Imports
  region: 3, // vscode.FoldingRangeKind.Region
};

let client = null;

function activate(context) {
  const output = vscode.window.createOutputChannel("Dial Language Server");
  context.subscriptions.push(output);

  const cfg = vscode.workspace.getConfiguration("dial.lsp");
  const cmd = cfg.get("path", "dialc");
  const trace = cfg.get("trace", false);

  client = new LspClient(cmd, output, trace);

  // ---- 文档同步 ----
  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.languageId !== "dial") return;
      client.didOpen(doc);
    })
  );
  context.subscriptions.push(
    vscode.workspace.onDidChangeTextDocument((e) => {
      if (e.document.languageId !== "dial") return;
      client.didChange(e.document);
    })
  );
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (doc.languageId !== "dial") return;
      client.didSave(doc);
    })
  );
  context.subscriptions.push(
    vscode.workspace.onDidCloseTextDocument((doc) => {
      if (doc.languageId !== "dial") return;
      client.didClose(doc);
    })
  );

  // ---- 语义高亮 ----
  const legend = new vscode.SemanticTokensLegend(TOKEN_TYPES, TOKEN_MODIFIERS);
  context.subscriptions.push(
    vscode.languages.registerDocumentSemanticTokensProvider(
      { language: "dial" },
      {
        provideDocumentSemanticTokens: async (doc) => client.semanticTokens(doc),
      },
      legend
    )
  );

  // ---- Hover / 跳转定义 / 补全（dialc lsp v0.95+ 已支持，转发到服务端）----
  context.subscriptions.push(
    vscode.languages.registerHoverProvider(
      { language: "dial" },
      { provideHover: (doc, pos) => client.hover(doc, pos) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerDefinitionProvider(
      { language: "dial" },
      { provideDefinition: (doc, pos) => client.definition(doc, pos) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerCompletionItemProvider(
      { language: "dial" },
      { provideCompletionItems: (doc, pos) => client.completion(doc, pos) },
      "." // 触发字符：`.` 后主动触发成员补全
    )
  );

  // ---- 轮 2：导航（大纲 / 引用 / 重命名 / 签名帮助）----
  context.subscriptions.push(
    vscode.languages.registerDocumentSymbolProvider(
      { language: "dial" },
      { provideDocumentSymbols: (doc) => client.documentSymbols(doc) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerReferenceProvider(
      { language: "dial" },
      { provideReferences: (doc, pos, ctx) => client.references(doc, pos, ctx) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerRenameProvider(
      { language: "dial" },
      {
        prepareRename: (doc, pos) => client.prepareRename(doc, pos),
        provideRenameEdits: (doc, pos, newName) => client.rename(doc, pos, newName),
      }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerSignatureHelpProvider(
      { language: "dial" },
      {
        provideSignatureHelp: (doc, pos) => client.signatureHelp(doc, pos),
        signatureHelpTriggerCharacters: ["(", ","],
        signatureHelpRetriggerCharacters: [","],
      },
      "(",
      ","
    )
  );

  // ---- 轮 3：格式化 / inlay 提示 ----
  context.subscriptions.push(
    vscode.languages.registerDocumentFormattingEditProvider(
      { language: "dial" },
      { provideDocumentFormattingEdits: (doc) => client.formatting(doc) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerInlayHintsProvider(
      { language: "dial" },
      { provideInlayHints: (doc, range) => client.inlayHints(doc, range) }
    )
  );

  // ---- 轮 4：代码动作 / 折叠 / 选择范围 / 文档链接 ----
  context.subscriptions.push(
    vscode.languages.registerCodeActionsProvider(
      { language: "dial" },
      { provideCodeActions: (doc, range, ctx) => client.codeActions(doc, range, ctx) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerFoldingRangeProvider(
      { language: "dial" },
      { provideFoldingRanges: (doc) => client.foldingRanges(doc) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerSelectionRangeProvider(
      { language: "dial" },
      { provideSelectionRanges: (doc, pos, ranges) => client.selectionRanges(doc, pos, ranges) }
    )
  );
  context.subscriptions.push(
    vscode.languages.registerDocumentLinkProvider(
      { language: "dial" },
      { provideDocumentLinks: (doc) => client.documentLinks(doc) }
    )
  );

  // 启动时已经打开着的 .dial 文件，补一次 didOpen
  for (const doc of vscode.workspace.textDocuments) {
    if (doc.languageId === "dial") client.didOpen(doc);
  }

  context.subscriptions.push({
    dispose: () => client.dispose(),
  });
}

function deactivate() {
  if (client) client.dispose();
}

// ---------------------------------------------------------------------------
// 极简 LSP 客户端
// ---------------------------------------------------------------------------

class LspClient {
  constructor(command, output, trace) {
    this.output = output;
    this.trace = trace;
    this.nextId = 1;
    this.pending = new Map();
    this.diagnostics = vscode.languages.createDiagnosticCollection("dial");
    this.ready = false;

    this.log(`启动: ${command} lsp`);
    this.proc = spawn(command, ["lsp"], { stdio: ["pipe", "pipe", "pipe"] });

    this.proc.on("error", (e) => {
      this.log(`无法启动 dialc：${e.message}`);
      vscode.window.showErrorMessage(
        `无法启动 Dial 语言服务器（${command}）：${e.message}。请检查设置 dial.lsp.path。`
      );
    });
    this.proc.on("exit", (code, sig) => {
      this.log(`语言服务器退出 code=${code} signal=${sig}`);
    });
    this.proc.stderr.on("data", (b) => this.log(`[stderr] ${b.toString()}`));

    // 读缓冲区：跨 chunk 拼消息（一个 Content-Length 帧可能被 TCP/管道切成多段）
    this.buf = Buffer.alloc(0);
    this.proc.stdout.on("data", (chunk) => {
      this.buf = Buffer.concat([this.buf, chunk]);
      this.drain();
    });

    this.initialize();
  }

  log(msg) {
    const line = `[${new Date().toLocaleTimeString()}] ${msg}`;
    this.output.appendLine(line);
    if (this.trace) console.log(line);
  }

  // ---- 写 ----
  send(msg) {
    const body = Buffer.from(JSON.stringify(msg), "utf8");
    this.proc.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
    this.proc.stdin.write(body);
  }

  notify(method, params) {
    this.send({ jsonrpc: "2.0", method, params });
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve) => {
      this.pending.set(id, resolve);
      this.send({ jsonrpc: "2.0", id, method, params });
      // 兜底超时：服务进程卡住时不让编辑器界面永远转圈
      setTimeout(() => {
        if (this.pending.delete(id)) resolve(null);
      }, 10000);
    });
  }

  // ---- 读 ----
  drain() {
    for (;;) {
      const headerEnd = this.buf.indexOf("\r\n\r\n");
      if (headerEnd < 0) return;
      const header = this.buf.slice(0, headerEnd).toString("ascii");
      const m = /Content-Length:\s*(\d+)/i.exec(header);
      if (!m) {
        // 头部不完整或异常：丢弃这一帧，避免死循环
        this.buf = this.buf.slice(headerEnd + 4);
        continue;
      }
      const len = parseInt(m[1], 10);
      const start = headerEnd + 4;
      if (this.buf.length < start + len) return; // 还没收全
      const body = this.buf.slice(start, start + len).toString("utf8");
      this.buf = this.buf.slice(start + len);

      let msg;
      try {
        msg = JSON.parse(body);
      } catch (e) {
        this.log(`收到非法 JSON: ${e.message}`);
        continue;
      }
      this.handleMsg(msg);
    }
  }

  handleMsg(msg) {
    if (msg.id !== undefined && this.pending.has(msg.id)) {
      const resolve = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      resolve(msg.result !== undefined ? msg.result : null);
      return;
    }
    if (msg.method === "textDocument/publishDiagnostics") {
      this.applyDiagnostics(msg.params);
    }
  }

  applyDiagnostics(params) {
    const uri = vscode.Uri.parse(params.uri);
    const items = (params.diagnostics || []).map((d) => {
      const range = new vscode.Range(
        d.range.start.line,
        d.range.start.character,
        d.range.end.line,
        d.range.end.character
      );
      const diag = new vscode.Diagnostic(range, d.message, d.severity === 1 ? 0 : d.severity);
      diag.source = d.source || "dialc";
      return diag;
    });
    this.diagnostics.set(uri, items);
  }

  async initialize() {
    // 声明本客户端支持的能力，让服务端据此能力协商（dialc lsp 会据此决定是否广播
    // hoverProvider / definitionProvider / completionProvider）。即使用户侧的 VS Code
    // 版本较老、不报这些能力，服务端仍会响应请求，故此处的声明是「锦上添花」。
    await this.request("initialize", {
      processId: process.pid,
      rootUri: null,
      capabilities: {
        textDocument: {
          hover: { dynamicRegistration: false },
          definition: { dynamicRegistration: false },
          completion: { completionItem: { snippetSupport: true }, dynamicRegistration: false },
          synchronization: { dynamicRegistration: false },
          // 服务端无条件广播 semanticTokensProvider，这里如实声明（否则声明与实际不符）。
          semanticTokens: {
            dynamicRegistration: false,
            requests: { full: true },
            tokenTypes: TOKEN_TYPES,
            tokenModifiers: TOKEN_MODIFIERS,
            formats: ["relative"],
          },
          // 轮2（导航）
          documentSymbol: { dynamicRegistration: false, hierarchicalDocumentSymbolSupport: true },
          references: { dynamicRegistration: false },
          rename: { dynamicRegistration: false, prepareSupport: true },
          signatureHelp: {
            dynamicRegistration: false,
            signatureInformation: { documentationFormat: ["markdown", "plaintext"] },
            contextSupport: true,
          },
          // 轮3（编写辅助）
          formatting: { dynamicRegistration: false },
          inlayHint: { dynamicRegistration: false },
          // 轮4（高级）
          codeAction: { dynamicRegistration: false },
          foldingRange: { dynamicRegistration: false, lineFoldingOnly: true },
          selectionRange: { dynamicRegistration: false },
          documentLink: { dynamicRegistration: false },
        },
        workspace: {
          symbol: { dynamicRegistration: false },
        },
      },
    });
    this.ready = true;
    this.notify("initialized", {});
    this.log("已连接");
  }

  // ---- 文档通知 ----
  didOpen(doc) {
    this.notify("textDocument/didOpen", {
      textDocument: {
        uri: doc.uri.toString(),
        languageId: "dial",
        version: doc.version,
        text: doc.getText(),
      },
    });
  }

  didChange(doc) {
    // 全量同步（服务端声明 textDocumentSync = 1）：整篇文本发过去。
    // 打字过程中**不**请求诊断——那会让波浪线在半句话上乱闪；诊断留给保存时。
    this.notify("textDocument/didChange", {
      textDocument: { uri: doc.uri.toString(), version: doc.version },
      contentChanges: [{ text: doc.getText() }],
    });
  }

  didSave(doc) {
    this.notify("textDocument/didSave", { textDocument: { uri: doc.uri.toString() } });
  }

  didClose(doc) {
    this.notify("textDocument/didClose", { textDocument: { uri: doc.uri.toString() } });
  }

  // ---- 语义高亮 ----
  async semanticTokens(doc) {
    if (!this.ready) return null;
    const result = await this.request("textDocument/semanticTokens/full", {
      textDocument: { uri: doc.uri.toString() },
    });
    if (!result || !Array.isArray(result.data)) return null;

    // 差分 -> 绝对，再交给 builder（它内部会重新差分）
    const builder = new vscode.SemanticTokensBuilder();
    let line = 0;
    let char = 0;
    const data = result.data;
    for (let i = 0; i + 4 < data.length; i += 5) {
      const dl = data[i];
      const dc = data[i + 1];
      line += dl;
      char = dl === 0 ? char + dc : dc;
      builder.push(line, char, data[i + 2], data[i + 3], data[i + 4]);
    }
    return builder.build();
  }

  // ---- Hover / 跳转定义 / 补全：转发到 dialc lsp ----

  async hover(doc, position) {
    if (!this.ready) return null;
    const result = await this.request("textDocument/hover", {
      textDocument: { uri: doc.uri.toString() },
      position: { line: position.line, character: position.character },
    });
    if (!result || !result.contents) return null;
    const c = result.contents;
    const value =
      c.kind === "markdown" && typeof c.value === "string"
        ? c.value
        : typeof c === "string"
        ? c
        : JSON.stringify(c);
    return new vscode.Hover(new vscode.MarkdownString(value));
  }

  async definition(doc, position) {
    if (!this.ready) return null;
    const result = await this.request("textDocument/definition", {
      textDocument: { uri: doc.uri.toString() },
      position: { line: position.line, character: position.character },
    });
    if (!result) return null;
    const arr = Array.isArray(result) ? result : [result];
    return arr.map(
      (loc) =>
        new vscode.Location(
          vscode.Uri.parse(loc.uri),
          new vscode.Range(
            loc.range.start.line,
            loc.range.start.character,
            loc.range.end.line,
            loc.range.end.character
          )
        )
    );
  }

  async completion(doc, position) {
    if (!this.ready) return null;
    const result = await this.request("textDocument/completion", {
      textDocument: { uri: doc.uri.toString() },
      position: { line: position.line, character: position.character },
    });
    if (!result) return null;
    const items = Array.isArray(result) ? result : result.items || [];
    return items.map((it) => {
      const item = new vscode.CompletionItem(
        it.label,
        typeof it.kind === "number" ? it.kind : vscode.CompletionItemKind.Text
      );
      if (it.detail) item.detail = it.detail;
      if (it.documentation) {
        const docVal =
          it.documentation.kind === "markdown"
            ? it.documentation.value
            : it.documentation;
        item.documentation = new vscode.MarkdownString(docVal);
      }
      // 精确替换范围：服务端已算好从「已输入前缀起点」到光标的区间，
      // 直接套用即可，避免 VS Code 默认把触发字符也算进替换范围。
      if (it.textEdit) {
        const r = it.textEdit.range;
        item.textEdit = new vscode.TextEdit(
          new vscode.Range(r.start.line, r.start.character, r.end.line, r.end.character),
          it.textEdit.newText
        );
      } else if (it.insertText) {
        item.insertText = it.insertText;
      }
      if (it.sortText) item.sortText = it.sortText;
      return item;
    });
  }

  dispose() {
    try {
      this.notify("shutdown");
      this.notify("exit");
    } catch (e) {
      /* 进程已死就算了 */
    }
    try {
      this.proc.kill();
    } catch (e) {
      /* 同上 */
    }
    this.diagnostics.dispose();
  }

  // -------------------------------------------------------------------------
  // 轮 2：导航
  // -------------------------------------------------------------------------

  async documentSymbols(doc) {
    if (!this.ready) return [];
    const result = await this.request("textDocument/documentSymbol", {
      textDocument: { uri: doc.uri.toString() },
    });
    if (!Array.isArray(result)) return [];
    const toSymbol = (s) => {
      const range = toRange(s.range);
      const sel = s.selectionRange ? toRange(s.selectionRange) : range;
      const children = (s.children || []).map(toSymbol);
      return new vscode.DocumentSymbol(
        s.name,
        s.detail || "",
        toSymbolKind(s.kind),
        range,
        sel,
        children
      );
    };
    return result.map(toSymbol);
  }

  async references(doc, position, ctx) {
    if (!this.ready) return [];
    const result = await this.request("textDocument/references", {
      textDocument: { uri: doc.uri.toString() },
      position: { line: position.line, character: position.character },
      context: { includeDeclaration: !!(ctx && ctx.includeDeclaration) },
    });
    if (!Array.isArray(result)) return [];
    return result.map(
      (loc) => new vscode.Location(vscode.Uri.parse(loc.uri), toRange(loc.range))
    );
  }

  async prepareRename(doc, position) {
    if (!this.ready) return null;
    const result = await this.request("textDocument/prepareRename", {
      textDocument: { uri: doc.uri.toString() },
      position: { line: position.line, character: position.character },
    });
    if (!result || !result.range) return null;
    return toRange(result.range);
  }

  async rename(doc, position, newName) {
    if (!this.ready) return null;
    const result = await this.request("textDocument/rename", {
      textDocument: { uri: doc.uri.toString() },
      position: { line: position.line, character: position.character },
      newName: newName,
    });
    if (!result || !result.changes) return null;
    // 服务端按 uri 分组返回编辑，这里聚成一个 WorkspaceEdit。
    const edit = new vscode.WorkspaceEdit();
    for (const uri of Object.keys(result.changes)) {
      const target = vscode.Uri.parse(uri);
      for (const e of result.changes[uri]) {
        edit.replace(target, toRange(e.range), e.newText);
      }
    }
    return edit;
  }

  async signatureHelp(doc, position) {
    if (!this.ready) return null;
    const result = await this.request("textDocument/signatureHelp", {
      textDocument: { uri: doc.uri.toString() },
      position: { line: position.line, character: position.character },
    });
    if (!result || !result.signatures || !result.signatures.length) return null;
    const signatures = result.signatures.map((s) => {
      const params = (s.parameters || []).map(
        (p) =>
          new vscode.ParameterInformation(
            typeof p.label === "string" ? p.label : p.label[0],
            p.documentation ? new vscode.MarkdownString(p.documentation) : undefined
          )
      );
      return new vscode.SignatureInformation(
        s.label,
        s.documentation ? new vscode.MarkdownString(s.documentation) : undefined,
        params
      );
    });
    return new vscode.SignatureHelp(
      signatures,
      result.activeSignature || 0,
      result.activeParameter || 0
    );
  }

  // -------------------------------------------------------------------------
  // 轮 3：格式化 / inlay 提示
  // -------------------------------------------------------------------------

  async formatting(doc) {
    if (!this.ready) return [];
    const result = await this.request("textDocument/formatting", {
      textDocument: { uri: doc.uri.toString() },
      options: { tabSize: 4, insertSpaces: true },
    });
    if (!Array.isArray(result)) return [];
    return result.map((e) => vscode.TextEdit.replace(toRange(e.range), e.newText));
  }

  async inlayHints(doc, range) {
    if (!this.ready) return [];
    const result = await this.request("textDocument/inlayHint", {
      textDocument: { uri: doc.uri.toString() },
      range: {
        start: { line: range.start.line, character: range.start.character },
        end: { line: range.end.line, character: range.end.character },
      },
    });
    if (!Array.isArray(result)) return [];
    return result.map((h) => {
      const label = Array.isArray(h.label) ? h.label : [h.label];
      const text = label
        .map((p) => (typeof p === "string" ? p : p.value))
        .join("");
      const pos = h.position || (h.range && h.range.start);
      return new vscode.InlayHint(
        text,
        new vscode.Position(pos.line, pos.character),
        h.kind === 1 ? vscode.InlayHintKind.Type : vscode.InlayHintKind.Parameter
      );
    });
  }

  // -------------------------------------------------------------------------
  // 轮 4：代码动作 / 折叠 / 选择范围 / 文档链接
  // -------------------------------------------------------------------------

  async codeActions(doc, range, ctx) {
    if (!this.ready) return [];
    const diagnostics = ((ctx && ctx.diagnostics) || []).map((d) => ({
      range: {
        start: { line: d.range.start.line, character: d.range.start.character },
        end: { line: d.range.end.line, character: d.range.end.character },
      },
      message: d.message,
      severity: d.severity,
      source: d.source,
    }));
    const result = await this.request("textDocument/codeAction", {
      textDocument: { uri: doc.uri.toString() },
      range: {
        start: { line: range.start.line, character: range.start.character },
        end: { line: range.end.line, character: range.end.character },
      },
      context: { diagnostics: diagnostics },
    });
    if (!Array.isArray(result)) return [];
    return result.map((a) => {
      const kind =
        a.kind === "quickfix"
          ? vscode.CodeActionKind.QuickFix
          : vscode.CodeActionKind.Source;
      const action = new vscode.CodeAction(a.title, kind);
      if (a.command) {
        action.command = {
          command: a.command.command,
          title: a.command.title,
          arguments: a.command.arguments || [],
        };
      }
      return action;
    });
  }

  async foldingRanges(doc) {
    if (!this.ready) return [];
    const result = await this.request("textDocument/foldingRange", {
      textDocument: { uri: doc.uri.toString() },
    });
    if (!Array.isArray(result)) return [];
    return result.map((f) => {
      const kind = FOLDING_KINDS[f.kind];
      return new vscode.FoldingRange(
        f.startLine,
        f.endLine,
        kind ? kind : undefined
      );
    });
  }

  async selectionRanges(doc, position, ranges) {
    if (!this.ready) return [];
    const positions = ranges && ranges.length ? ranges : [position];
    const result = await this.request("textDocument/selectionRange", {
      textDocument: { uri: doc.uri.toString() },
      positions: positions.map((p) => ({ line: p.line, character: p.character })),
    });
    if (!Array.isArray(result)) return [];
    return result.map(toSelectionRange);
  }

  async documentLinks(doc) {
    if (!this.ready) return [];
    const result = await this.request("textDocument/documentLink", {
      textDocument: { uri: doc.uri.toString() },
    });
    if (!Array.isArray(result)) return [];
    return result.map((l) =>
      new vscode.DocumentLink(
        toRange(l.range),
        l.target ? vscode.Uri.parse(l.target) : undefined
      )
    );
  }
}

// LSP 的 SelectionRange 是「每层都带 parent」的嵌套结构，逐层还原。
function toSelectionRange(r) {
  if (!r || !r.range) return null;
  return new vscode.SelectionRange(
    toRange(r.range),
    r.parent ? toSelectionRange(r.parent) : undefined
  );
}

module.exports = { activate, deactivate };
