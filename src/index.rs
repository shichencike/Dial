//! 跨文件符号索引（v0.95）——为 LSP 的 Hover / 跳转定义 / 补全提供底座。
//!
//! 设计要点（与 §28「同一套代码」一致）：
//!
//! - **容错**：索引在**类型检查之外**独立构建。用户打字到一半的代码通常是错的，
//!   `semantic::analyze` 会中途报错；但索引必须照常把已经写出来的声明收集起来，
//!   否则 Hover / 补全在半成品代码上就全废了。
//! - **零隐式转换**：作用域链「由内向外」查找，正是 `semantic::lookup_type` 的语义，
//!   因此**同名变量的遮蔽是结构性的必然**，不是近似（见 `lookup_local`）。
//! - **跨文件**：import 的包符号来自 `.diali`，与编译器链接时用的是**同一份解析**
//!   （`package::parse_diali` / `resolve_type_name`），不存在第二份真相。
//! - **无配置**：范围 = 当前文件 + 其 `import` 的包。Dial 没有项目内多文件模块系统，
//!   自动扫描当前目录其它 `.dial` 既不合法也无意义（编译器根本不纳入它们）。

use std::collections::HashMap;

use crate::ast::*;
use crate::package;

/// 符号种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Var,
    Param,
    OpaqueType,
    ExternFn,
}

impl SymbolKind {
    /// LSP `CompletionItemKind` 数值（VS Code 语义）。
    pub fn completion_kind(&self) -> u32 {
        match self {
            SymbolKind::Function | SymbolKind::ExternFn => 3, // Function
            SymbolKind::Struct => 23,                         // Struct
            SymbolKind::Var | SymbolKind::Param => 6,         // Variable
            SymbolKind::OpaqueType => 22,                     // Class / 不透明句柄归为类型
        }
    }
}

/// 函数签名信息：`(完整签名标签, 各参数标签)`。
/// 用于 `signatureHelp`——`Symbol.ty` 只存返回类型，参数表需单独保留。
pub type SigInfo = (String, Vec<String>);

/// 一个标识符「使用」位置（引用），用于查找引用与重命名。
#[derive(Debug, Clone)]
pub struct Ref {
    /// 该引用解析到的符号在 `symbols` 中的下标。
    /// 与定义共用同一身份 → 天然正确处理「同名遮蔽」（内层的同名变量不会被误并进来）。
    pub symbol_index: usize,
    /// 标识符文本（冗余存储，便于断言 / 调试）。
    #[allow(dead_code)]
    pub name: String,
    /// 标识符本身的源码区间（1 起、char 列）：`(line1, char1, line1, char2)`。
    pub span: (usize, usize, usize, usize),
}

/// 一个声明的符号。
#[derive(Debug, Clone)]
pub struct Symbol {
    pub kind: SymbolKind,
    pub name: String,
    pub ty: Type,
    /// 标识符本身的源码区间（1 起、char 列）：`(line1, char1, line2, char2)`。
    pub span: (usize, usize, usize, usize),
    /// 文档（`///` 或未来源码注释）。包符号来自 `.diali` 的 `///`。
    pub doc: Vec<String>,
    /// 所属作用域 id（文件内符号）。预留：当前查找走作用域链，不直接读它。
    #[allow(dead_code)]
    pub scope_id: usize,
    /// `None` = 当前文件；`Some(path)` = 该声明位于外部 `.diali` 文件（跳转定义用）。
    pub decl_uri: Option<String>,
}

/// 一个作用域（块）。
#[derive(Debug, Clone)]
pub struct Scope {
    pub id: usize,
    pub parent: Option<usize>,
    /// 该块的源码包围盒（1 起、char 列）：由块内语句的位置极值推导。
    pub span: (usize, usize, usize, usize),
    pub symbols: Vec<usize>,
    /// 本作用域内所有语句的起始位置（仅用于推导 span）。
    positions: Vec<(usize, usize)>,
}

/// 一份文件索引后的结果。
#[derive(Debug, Clone)]
pub struct IndexedFile {
    /// 文档 URI。预留：当前索引以 uri 为 key 存于 `WorkspaceIndex.files`，不直接读它。
    #[allow(dead_code)]
    pub uri: String,
    pub scopes: Vec<Scope>,
    pub symbols: Vec<Symbol>,
    pub top_scope: usize,
    /// 本文件 import 的包名（供包符号查询）。
    pub imports: Vec<String>,
    /// 本文件中所有标识符「使用」位置（引用）。供 references / rename 使用。
    pub references: Vec<Ref>,
    /// 函数符号下标 → 其函数体作用域 id（供 documentSymbol 大纲派生子节点）。
    pub fn_bodies: HashMap<usize, usize>,
    /// 函数符号下标 → 签名信息（供 signatureHelp）。
    pub signatures: HashMap<usize, SigInfo>,
}

/// 一个已安装包导出的符号（来自 `.diali`）。
#[derive(Debug, Clone, Default)]
pub struct PackageSyms {
    pub opaque: Vec<Symbol>,
    pub fns: Vec<Symbol>,
}

/// 整个工作区索引：当前文件 + 已加载的包。
#[derive(Debug, Default)]
pub struct WorkspaceIndex {
    pub files: HashMap<String, IndexedFile>,
    pub packages: HashMap<String, PackageSyms>,
    /// 外部函数名 → 签名信息（供 signatureHelp；来自已加载的 `.diali`）。
    pub package_sigs: HashMap<String, SigInfo>,
}

// ---------------------------------------------------------------------------
// 包符号加载
// ---------------------------------------------------------------------------

/// 确保某包的符号已载入 `ws.packages`。返回是否已加载（含本来就有）。
///
/// 复用 `package` 的查找 / 校验 / 解析链路——**不**调用 `main::resolve_imports`（那会往
/// stdout 打日志，污染 LSP 协议流）。`.diali` 的 SHA256 校验同样在此生效。
pub fn ensure_package(ws: &mut WorkspaceIndex, name: &str) -> bool {
    if ws.packages.contains_key(name) {
        return true;
    }
    let (dir, manifest) = match package::find_installed(name) {
        Ok(Some(x)) => x,
        _ => return false,
    };
    let iface_path = match package::verify_iface(&dir, &manifest) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let text = match std::fs::read_to_string(&iface_path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let iface = match package::parse_diali(&text) {
        Ok(i) => i,
        Err(_) => return false,
    };
    let uri = format!("file://{}", iface_path.display());
    let mut opaque = Vec::new();
    for (oname, oline) in &iface.opaque_lines {
        opaque.push(Symbol {
            kind: SymbolKind::OpaqueType,
            name: oname.clone(),
            ty: Type::Opaque(oname.clone()),
            span: span_of(*oline, 1, oname),
            doc: Vec::new(),
            scope_id: 0,
            decl_uri: Some(uri.clone()),
        });
    }
    let mut fns = Vec::new();
    for f in &iface.fns {
        let mut params = Vec::new();
        let mut param_labels = Vec::new();
        for (pn, pt) in &f.params {
            if let Ok(t) = package::resolve_type_name(pt, &iface.opaque_types) {
                params.push(t);
            }
            param_labels.push(format!("{}: {}", pn, pt));
        }
        let ret = package::resolve_type_name(&f.ret, &iface.opaque_types).unwrap_or(Type::Void);
        // 保留签名信息供 signatureHelp（包符号只读，.diali 里的声明行）。
        let mut sig = format!("{}(", f.name);
        sig.push_str(&param_labels.join(", "));
        sig.push_str(&format!("): {}", ret.display()));
        ws.package_sigs.insert(f.name.clone(), (sig, param_labels));
        fns.push(Symbol {
            kind: SymbolKind::ExternFn,
            name: f.name.clone(),
            ty: ret,
            span: span_of(f.line, 1, &f.name),
            doc: f.doc.clone(),
            scope_id: 0,
            decl_uri: Some(uri.clone()),
        });
    }
    ws.packages
        .insert(name.to_string(), PackageSyms { opaque, fns });
    true
}

// ---------------------------------------------------------------------------
// 文件索引
// ---------------------------------------------------------------------------

/// 索引一份已 parse 的程序。
///
/// `imports` 用于记录本文件引入了哪些包（供查询包符号）；包符号本身应在调用前
/// 由 `ensure_package` 载入 `ws.packages`。
pub fn index_program(uri: &str, program: &Program, imports: &[String]) -> IndexedFile {
    let mut ix = Indexer::new();
    let top = ix.push_scope(None);

    for s in &program.structs {
        ix.add(
            top,
            Symbol {
                kind: SymbolKind::Struct,
                name: s.name.clone(),
                ty: Type::Struct(s.name.clone()),
                span: span_of(s.name_line, s.name_col, &s.name),
                doc: Vec::new(),
                scope_id: top,
                decl_uri: None,
            },
        );
        ix.mark(top, (s.name_line, s.name_col));
    }

    for f in &program.functions {
        let fn_idx = ix.add(
            top,
            Symbol {
                kind: SymbolKind::Function,
                name: f.name.clone(),
                ty: f.ret.clone(),
                span: span_of(f.line, f.col, &f.name),
                doc: Vec::new(),
                scope_id: top,
                decl_uri: None,
            },
        );
        // 保留签名信息（函数名 + 形参 + 返回类型）供 signatureHelp 使用。
        let params: Vec<String> = f
            .params
            .iter()
            .map(|(pn, pt)| format!("{}: {}", pn, pt.display()))
            .collect();
        let mut sig = format!("{}(", f.name);
        sig.push_str(&params.join(", "));
        sig.push_str(&format!("): {}", f.ret.display()));
        ix.signatures.insert(fn_idx, (sig, params));
        ix.mark(top, (f.line, f.col));
        let body = ix.push_scope(Some(top));
        ix.fn_bodies.insert(fn_idx, body);
        for (i, (pname, pty)) in f.params.iter().enumerate() {
            let (pl, pc) = f.param_pos.get(i).copied().unwrap_or((f.line, f.col));
            ix.add(
                body,
                Symbol {
                    kind: SymbolKind::Param,
                    name: pname.clone(),
                    ty: pty.clone(),
                    span: span_of(pl, pc, pname),
                    doc: Vec::new(),
                    scope_id: body,
                    decl_uri: None,
                },
            );
            ix.mark(body, (pl, pc));
        }
        ix.index_stmts(&f.body, body);
    }

    let mut f = IndexedFile {
        uri: uri.to_string(),
        scopes: ix.scopes,
        symbols: ix.symbols,
        top_scope: top,
        imports: imports.to_vec(),
        references: Vec::new(),
        fn_bodies: ix.fn_bodies,
        signatures: ix.signatures,
    };
    f.finalize_spans();
    f
}

struct Indexer {
    scopes: Vec<Scope>,
    symbols: Vec<Symbol>,
    fn_bodies: HashMap<usize, usize>,
    signatures: HashMap<usize, SigInfo>,
}

impl Indexer {
    fn new() -> Self {
        Indexer {
            scopes: Vec::new(),
            symbols: Vec::new(),
            fn_bodies: HashMap::new(),
            signatures: HashMap::new(),
        }
    }

    fn push_scope(&mut self, parent: Option<usize>) -> usize {
        let id = self.scopes.len();
        self.scopes.push(Scope {
            id,
            parent,
            span: (0, 0, 0, 0),
            symbols: Vec::new(),
            positions: Vec::new(),
        });
        id
    }

    fn add(&mut self, scope_id: usize, sym: Symbol) -> usize {
        let idx = self.symbols.len();
        self.symbols.push(sym);
        self.scopes[scope_id].symbols.push(idx);
        idx
    }

    fn mark(&mut self, scope_id: usize, pos: (usize, usize)) {
        self.scopes[scope_id].positions.push(pos);
    }

    fn index_stmts(&mut self, stmts: &[Stmt], scope_id: usize) {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    name,
                    ty,
                    name_line,
                    name_col,
                    ..
                } => {
                    self.add(
                        scope_id,
                        Symbol {
                            kind: SymbolKind::Var,
                            name: name.clone(),
                            ty: ty.clone(),
                            span: span_of(*name_line, *name_col, name),
                            doc: Vec::new(),
                            scope_id,
                            decl_uri: None,
                        },
                    );
                    self.mark(scope_id, (*name_line, *name_col));
                }
                Stmt::If {
                    then_branch,
                    else_branch,
                    line,
                    col,
                    ..
                } => {
                    self.mark(scope_id, (*line, *col));
                    let t = self.push_scope(Some(scope_id));
                    self.index_stmts(then_branch, t);
                    if let Some(eb) = else_branch {
                        let e = self.push_scope(Some(scope_id));
                        self.index_stmts(eb, e);
                    }
                }
                Stmt::While { body, line, col, .. } => {
                    self.mark(scope_id, (*line, *col));
                    let w = self.push_scope(Some(scope_id));
                    self.index_stmts(body, w);
                }
                Stmt::For {
                    var,
                    var_line,
                    var_col,
                    body,
                    line,
                    col,
                    ..
                } => {
                    self.mark(scope_id, (*line, *col));
                    let fsc = self.push_scope(Some(scope_id));
                    self.add(
                        fsc,
                        Symbol {
                            kind: SymbolKind::Var,
                            name: var.clone(),
                            ty: Type::Int, // for 循环变量恒为 int（见 AST 注释）
                            span: span_of(*var_line, *var_col, var),
                            doc: Vec::new(),
                            scope_id: fsc,
                            decl_uri: None,
                        },
                    );
                    self.mark(fsc, (*var_line, *var_col));
                    self.index_stmts(body, fsc);
                }
                Stmt::Try {
                    body,
                    catch,
                    line,
                    col,
                    ..
                } => {
                    self.mark(scope_id, (*line, *col));
                    let tb = self.push_scope(Some(scope_id));
                    self.index_stmts(body, tb);
                    let cb = self.push_scope(Some(scope_id));
                    if let Some(cv) = &catch.var {
                        self.add(
                            cb,
                            Symbol {
                                kind: SymbolKind::Var,
                                name: cv.clone(),
                                ty: Type::Error, // 捕获变量恒为 Error（见 §26）
                                span: span_of(catch.var_line, catch.var_col, cv),
                                doc: Vec::new(),
                                scope_id: cb,
                                decl_uri: None,
                            },
                        );
                        self.mark(cb, (catch.var_line, catch.var_col));
                    }
                    self.index_stmts(&catch.body, cb);
                }
                Stmt::Select {
                    arms,
                    default_body,
                    line,
                    col,
                    ..
                } => {
                    self.mark(scope_id, (*line, *col));
                    for arm in arms {
                        let asc = self.push_scope(Some(scope_id));
                        self.add(
                            asc,
                            Symbol {
                                kind: SymbolKind::Var,
                                name: arm.var.clone(),
                                // 通道元素类型需语义分析才知，这里只声明其存在（见下）。
                                ty: Type::Void,
                                span: span_of(arm.var_line, arm.var_col, &arm.var),
                                doc: Vec::new(),
                                scope_id: asc,
                                decl_uri: None,
                            },
                        );
                        self.mark(asc, (arm.var_line, arm.var_col));
                        self.index_stmts(&arm.body, asc);
                    }
                    if let Some(db) = default_body {
                        let d = self.push_scope(Some(scope_id));
                        self.index_stmts(db, d);
                    }
                }
                // 裸块（v0.98.next）：引入新作用域并递归索引内部语句。
                Stmt::Block { body, line, col } => {
                    self.mark(scope_id, (*line, *col));
                    let b = self.push_scope(Some(scope_id));
                    self.index_stmts(body, b);
                }
                Stmt::ForEach { value, value_line, value_col, body, line, col, .. } => {
                    self.mark(scope_id, (*line, *col));
                    let fsc = self.push_scope(Some(scope_id));
                    self.add(
                        fsc,
                        Symbol {
                            kind: SymbolKind::Var,
                            name: value.clone(),
                            ty: Type::Int, // 占位：索引阶段无类型信息，元素类型以 Int 近似（不影响跳转/高亮）
                            span: span_of(*value_line, *value_col, value),
                            doc: Vec::new(),
                            scope_id: fsc,
                            decl_uri: None,
                        },
                    );
                    self.mark(fsc, (*value_line, *value_col));
                    self.index_stmts(body, fsc);
                }
                Stmt::ForEachMap {
                    key,
                    key_line,
                    key_col,
                    value,
                    value_line,
                    value_col,
                    body,
                    line,
                    col,
                    ..
                } => {
                    self.mark(scope_id, (*line, *col));
                    let fsc = self.push_scope(Some(scope_id));
                    for (nm, vl, vc) in [(&key, *key_line, *key_col), (&value, *value_line, *value_col)] {
                        self.add(
                            fsc,
                            Symbol {
                                kind: SymbolKind::Var,
                                name: nm.to_string(),
                                ty: Type::Int,
                                span: span_of(vl, vc, nm),
                                doc: Vec::new(),
                                scope_id: fsc,
                                decl_uri: None,
                            },
                        );
                        self.mark(fsc, (vl, vc));
                    }
                    self.index_stmts(body, fsc);
                }
                Stmt::DoWhile { body, line, col, .. } => {
                    self.mark(scope_id, (*line, *col));
                    let w = self.push_scope(Some(scope_id));
                    self.index_stmts(body, w);
                }
                Stmt::Match { arms, line, col, .. } => {
                    self.mark(scope_id, (*line, *col));
                    for arm in arms {
                        let asc = self.push_scope(Some(scope_id));
                        self.mark(asc, (arm.line, arm.col));
                        self.index_stmts(&arm.body, asc);
                    }
                }
                Stmt::Defer(inner, line, col) => {
                    self.mark(scope_id, (*line, *col));
                    self.index_stmts(std::slice::from_ref(inner.as_ref()), scope_id);
                }
                // 以下语句不引入绑定，仅登记位置以参与 span 推导。
                Stmt::Expr(_, line, col) => self.mark(scope_id, (*line, *col)),
                Stmt::Return(_, line, col) => self.mark(scope_id, (*line, *col)),
                Stmt::Throw(_, line, col) => self.mark(scope_id, (*line, *col)),
                Stmt::Assign { line, col, .. } => self.mark(scope_id, (*line, *col)),
                Stmt::AssignField { line, col, .. } => self.mark(scope_id, (*line, *col)),
                Stmt::AssignIndex { line, col, .. } => self.mark(scope_id, (*line, *col)),
                Stmt::Break(line, col) => self.mark(scope_id, (*line, *col)),
                Stmt::Continue(line, col) => self.mark(scope_id, (*line, *col)),
            }
        }
    }
}

impl Scope {
    fn contains(&self, point: (usize, usize)) -> bool {
        le((self.span.0, self.span.1), point) && le(point, (self.span.2, self.span.3))
    }
}

impl IndexedFile {
    /// 计算各作用域的包围盒（index 完成后调用一次）。
    pub fn finalize_spans(&mut self) {
        for sc in &mut self.scopes {
            if sc.positions.is_empty() {
                continue;
            }
            let mut min = sc.positions[0];
            let mut max = sc.positions[0];
            for p in &sc.positions {
                if le(*p, min) {
                    min = *p;
                }
                if le(max, *p) {
                    max = *p;
                }
            }
            sc.span = (min.0, min.1, max.0, max.1);
        }
    }

    /// 找包含某位置的最内层作用域（1 起、char 列）。
    pub fn scope_at(&self, point: (usize, usize)) -> Option<usize> {
        let mut best: Option<usize> = None;
        for sc in &self.scopes {
            if sc.contains(point) {
                match best {
                    None => best = Some(sc.id),
                    Some(b) => {
                        if self.depth(sc.id) > self.depth(b) {
                            best = Some(sc.id);
                        }
                    }
                }
            }
        }
        best
    }

    fn depth(&self, id: usize) -> usize {
        let mut d = 0;
        let mut cur = self.scopes.get(id).and_then(|s| s.parent);
        while let Some(p) = cur {
            d += 1;
            cur = self.scopes.get(p).and_then(|s| s.parent);
        }
        d
    }

    /// 沿作用域链（由内向外）按名字查找符号。
    pub fn lookup_local(&self, scope_id: usize, name: &str) -> Option<&Symbol> {
        let mut cur = Some(scope_id);
        while let Some(id) = cur {
            let sc = match self.scopes.get(id) {
                Some(s) => s,
                None => break,
            };
            for &si in &sc.symbols {
                let sym = &self.symbols[si];
                if sym.name == name {
                    return Some(sym);
                }
            }
            cur = sc.parent;
        }
        None
    }

    /// 在 import 的包里按名字查找符号（extern fn / 不透明类型）。
    pub fn lookup_package<'a>(&self, ws: &'a WorkspaceIndex, name: &str) -> Option<&'a Symbol> {
        for pkg in &self.imports {
            if let Some(syms) = ws.packages.get(pkg) {
                for s in syms.fns.iter().chain(syms.opaque.iter()) {
                    if s.name == name {
                        return Some(s);
                    }
                }
            }
        }
        None
    }

    /// 收集某位置可见的全部符号（作用域链 + 包），按名字去重（就近优先）。
    ///
    /// 返回**克隆**后的 `Symbol`：符号既可能来自本文件（`self`）也可能来自包（`ws`），
    /// 两者的借用生命周期不同，无法统一成一个 `&Symbol` 返回，故克隆。
    pub fn visible_symbols(&self, ws: &WorkspaceIndex, scope_id: usize) -> Vec<Symbol> {
        let mut out: Vec<Symbol> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        // 1) 文件内作用域链（由内向外）
        let mut cur = Some(scope_id);
        while let Some(id) = cur {
            let sc = match self.scopes.get(id) {
                Some(s) => s,
                None => break,
            };
            for &si in &sc.symbols {
                let sym = &self.symbols[si];
                let e = seen.entry(sym.name.clone()).or_insert(out.len());
                if *e == out.len() {
                    out.push(sym.clone());
                }
            }
            cur = sc.parent;
        }
        // 2) import 的包符号
        for pkg in &self.imports {
            if let Some(syms) = ws.packages.get(pkg) {
                for s in syms.fns.iter().chain(syms.opaque.iter()) {
                    let e = seen.entry(s.name.clone()).or_insert(out.len());
                    if *e == out.len() {
                        out.push(s.clone());
                    }
                }
            }
        }
        out
    }

    /// 沿作用域链按名字查找符号，返回其在 `symbols` 中的下标。
    /// 与 `lookup_local` 类似，但返回下标（重命名 / 引用去重需要身份而非克隆）。
    pub fn lookup_local_index(&self, scope_id: usize, name: &str) -> Option<usize> {
        let mut cur = Some(scope_id);
        while let Some(id) = cur {
            let sc = self.scopes.get(id)?;
            for &si in &sc.symbols {
                if self.symbols[si].name == name {
                    return Some(si);
                }
            }
            cur = sc.parent;
        }
        None
    }

    /// 重新扫描全文，记录每个标识符解析到的符号（引用）。
    /// 必须在 `finalize_spans` 之后调用（依赖 scope 包围盒）。
    pub fn collect_references(&mut self, text: &str) {
        let tokens = match crate::lexer::lex(text) {
            Ok(t) => t,
            Err(_) => return,
        };
        for tk in tokens {
            if let crate::lexer::TokenKind::Ident(name) = &tk.kind {
                let line1 = tk.line;
                let char1 = tk.col;
                let char2 = tk.col + name.chars().count();
                if let Some(scope_id) = self.scope_at((line1, char1)) {
                    if let Some(sidx) = self.lookup_local_index(scope_id, name) {
                        self.references.push(Ref {
                            symbol_index: sidx,
                            name: name.clone(),
                            span: (line1, char1, line1, char2),
                        });
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 位置辅助
// ---------------------------------------------------------------------------

/// 标识符区间：`(line, char)` 起点 + 名字长度（UTF-16 码元）。
fn span_of(line: usize, col: usize, name: &str) -> (usize, usize, usize, usize) {
    (line, col, line, col + utf16_len(name))
}

/// UTF-16 码元数（LSP 计量单位）。
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 位置比较：`a <= b`（按 (line, char) 字典序）。
fn le(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 <= b.1)
}

// ---------------------------------------------------------------------------
// 光标 <-> 标识符
// ---------------------------------------------------------------------------

/// 把 LSP 的（0 起行, 0 起 UTF-16 列）换算成 dialc 内部（1 起行, 1 起 char 列）。
///
/// 与 `lsp::char_col_to_utf16` 互逆：`char_col_to_utf16` 把 char 列折成 UTF-16 列，
/// 这里把 UTF-16 列展开回 char 列（行内有 emoji 时才不同）。
pub fn utf16_to_char_col(line_text: &str, utf16_col: usize) -> usize {
    let mut acc = 0usize;
    for (i, ch) in line_text.chars().enumerate() {
        if acc >= utf16_col {
            return i + 1; // 1 起
        }
        acc += ch.len_utf16();
    }
    line_text.chars().count() + 1
}

/// 取光标下的标识符名字。找不到返回 None。
///
/// 复用**编译器自己的词法器**：token 边界与编译一致，绝不出现「编辑器认得、编译器不认得」。
pub fn identifier_at(text: &str, line1: usize, char1: usize) -> Option<String> {
    let tokens = match crate::lexer::lex(text) {
        Ok(t) => t,
        Err(_) => return None,
    };
    for tk in tokens {
        if let crate::lexer::TokenKind::Ident(name) = &tk.kind {
            if tk.line != line1 {
                continue;
            }
            let start = tk.col; // 1 起 char 列
            let end = tk.col + name.chars().count();
            if char1 >= start && char1 <= end {
                return Some(name.clone());
            }
        }
    }
    None
}
