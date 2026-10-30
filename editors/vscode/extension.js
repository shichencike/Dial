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
    await this.request("initialize", {
      processId: process.pid,
      rootUri: null,
      capabilities: {},
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
}

module.exports = { activate, deactivate };
