//! 预处理阶段（v1.23）：编译流水线中**专门且唯一**的前置处理层。
//!
//! 定位：位于「词法 token 收集完成」与「语义分析 `analyze`」之间。它把源码收集、
//! import 解析、逐文件 parse、AST 合并（含冲突检测）、语言层 extern/link 结算、
//! 以及编译期裁剪（v1.22 debug）统一收拢到一个有明确边界的阶段。
//!
//! 为什么独立成阶段：
//!   1. **单一职责**：`frontend` 从此只做「预处理 → 语义分析」两步编排，不再混杂
//!      十几种前置细节；任何前置处理的新增/调试都只落在本文件。
//!   2. **可扩展点**：[`PreprocessOptions`] 是预处理行为的唯一配置入口。后续要加
//!      条件编译（`cfg`）、源码宏、特征开关等，只需在 options 加字段、在 [`run`]
//!      里加一道 pass，调用方（build/run/check/test）零改动。
//!   3. **双阶段产物**：[`PreprocessOutput::program`] 是裁剪后、可喂给 `analyze` 的 AST；
//!      [`PreprocessOutput::raw_program`] 是裁剪前的完整 AST，供 LSP / `dialc test`
//!      看到全部（含 debug）代码——这个区分从阶段层面固化，不再散落在调用点。
//!
//! 本模块零 I/O 副作用（除读源码文件与注入调用方提供的 `log`），不碰链接与产物路径。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{lexer, package, parser, semantic};

// ---------------------------------------------------------------------------
//  阶段配置与产物（扩展点）
// ---------------------------------------------------------------------------

/// 预处理阶段的可选行为。新增预处理器特性时在此加字段（带默认值，保持向后兼容）。
#[derive(Clone, Debug)]
pub struct PreprocessOptions {
    /// 是否保留 debug 代码（`debug_print(...)` 与 `debug { ... }`）。
    /// `true`（debug/check/test）保留；`false`（release 构建）在语义分析前删除。
    pub keep_debug: bool,
    /// v1.23：条件编译事实表。内置维度（debug/target_os/target_arch）已预置，
    /// 其余来自 CLI `--cfg key[=value]`。`@cfg(name)` 查布尔（有键即 true，除非值为 "0"/"false"）；
    /// `@cfg(key = "value")` 查精确字符串相等。
    pub cfg_facts: HashMap<String, String>,
    /// v1.24：`#` 预处理宏的命令行注入，等价源文件顶部 `#define NAME[=value]`。
    /// 每项为 `(宏名, 可选值)`；无值时按 `1` 处理。与 `#define` 同名时以源文件最后一次为准。
    pub defines: Vec<(String, Option<String>)>,
}

impl Default for PreprocessOptions {
    fn default() -> Self {
        Self { keep_debug: true, cfg_facts: HashMap::new(), defines: Vec::new() }
    }
}

impl PreprocessOptions {
    /// v1.24：构造选项并注入内置 cfg 事实（debug / target_os / target_arch）
    /// 与 CLI 宏定义（`#define` / `-D`）。
    pub fn with_all(
        keep_debug: bool,
        extra: HashMap<String, String>,
        defines: Vec<(String, Option<String>)>,
    ) -> Self {
        let mut cfg_facts = HashMap::new();
        // debug 维度与构建模式一致（与 keep_debug 同源）。
        cfg_facts.insert("debug".into(), if keep_debug { "1" } else { "0" }.into());
        // 目标平台维度：复用 package::host_triple() 的判定（含 DIAL_TARGET 覆盖）。
        let triple = package::host_triple();
        let (os, arch) = split_triple_os_arch(&triple);
        cfg_facts.insert("target_os".into(), os);
        cfg_facts.insert("target_arch".into(), arch);
        cfg_facts.extend(extra);
        Self { keep_debug, cfg_facts, defines }
    }
}

/// 从形如 `x86_64-unknown-linux-gnu` 的 triple 中抽出 (os, arch)。
/// 只取最有辨识度的两段，无法识别时回退到全文/空串（不会 panic）。
fn split_triple_os_arch(triple: &str) -> (String, String) {
    let parts: Vec<&str> = triple.split('-').collect();
    let arch = parts.first().copied().unwrap_or("").to_string();
    let os = parts
        .iter()
        .rev()
        .find(|p| {
            ["linux", "windows", "macos", "darwin", "android", "ios", "freebsd", "none"]
                .iter().any(|k| p.contains(k))
        })
        .copied()
        .unwrap_or("")
        .to_string();
    (os, arch)
}

/// 预处理阶段产物：`analyze` 所需的全部输入，已结算干净。
pub struct PreprocessOutput {
    /// 裁剪后的 AST，可直接喂给 `semantic::analyze`。
    pub program: crate::ast::Program,
    /// 裁剪前的完整 AST（含 debug 代码），供 LSP / `dialc test` 重新分析。
    pub raw_program: crate::ast::Program,
    /// 外部函数表（包 .diali + 源码 extern fn）。
    pub externs: Vec<(String, semantic::FuncInfo)>,
    /// 待链接的库列表（包机器码 + 源码 link 声明）。
    pub pkg_libs: Vec<PathBuf>,
    /// 动态库运行期查找路径。
    pub ffi_rpaths: Vec<PathBuf>,
    /// 函数名 → 定义它的源文件路径（asm 后端多文件 DWARF 使用）。
    pub fn_origin: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
//  源码单元与收集
// ---------------------------------------------------------------------------

struct SourceUnit {
    path: PathBuf,
    tokens: Vec<lexer::Token>,
}

struct SourceSet {
    /// 深度优先收集到的所有源码文件，units[0] 恒为主源文件。
    units: Vec<SourceUnit>,
    /// 汇总后的**包名** import（本地源码项不在这里），已去重。
    imports: Vec<String>,
}

/// 只从 token 流里抽 `import "..."` 的字符串，不做完整语法解析。
///
/// 之所以用「扫 token」而不是 `parser::parse`：两阶段解析的本意就是先拿 import
/// 列表、解析包（从而知道不透明类型名）后再正式解析；此时若强行完整 parse，
/// 源码里用到包句柄类型的地方会在第一遍就失败。扫 token 天然免疫这个问题。
pub(crate) fn import_specs(tokens: &[lexer::Token]) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..tokens.len() {
        if matches!(&tokens[i].kind, lexer::TokenKind::Import) {
            if let Some(n) = tokens.get(i + 1) {
                if let lexer::TokenKind::StringLit(s) = &n.kind {
                    out.push(s.clone());
                }
            }
        }
    }
    out
}

/// 判断一条 import 是否指向**本地源码模块**。
///
/// 两种写法都算本地：
///   - 显式相对路径：`import "./util.dial";` / `"../common/util"` / 任意 `.dial` 后缀；
///   - 裸名且同目录确有 `<name>.dial`：`import "util";` 直接可用。
///
/// 其它裸名再依次交给包内 `src/` 映射与包名解析。
pub(crate) fn is_local_import(spec: &str, base: &Path) -> bool {
    if spec.ends_with(".dial")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with(".\\")
        || spec.starts_with("..\\")
    {
        return true;
    }
    if !spec.contains('/') && !spec.contains('\\') {
        return base.join(format!("{}.dial", spec)).is_file();
    }
    false
}

pub(crate) fn local_module_path(spec: &str, base: &Path) -> PathBuf {
    let mut rel = PathBuf::from(spec);
    if rel.extension().and_then(|e| e.to_str()) != Some("dial") {
        rel.set_extension("dial");
    }
    if rel.is_absolute() {
        rel
    } else {
        base.join(rel)
    }
}

/// 递归收集源码模块，按**三级优先**解析每条 `import`：
///
/// 1. 本地源码模块（相对路径 / 同目录同名 `.dial`）——用户项目里的文件优先；
/// 2. 已安装包 `src/` 里的 Dial 源码模块（`[src]` 段声明的裸名）——随包分发；
/// 3. 包名（读 `.diali` + 选 `.a`）——汇总进 `set.imports` 交给 `resolve_imports`。
fn collect_sources(
    root: &Path,
    visited: &mut HashSet<PathBuf>,
    stack: &mut Vec<PathBuf>,
    set: &mut SourceSet,
    defines: &[(String, Option<String>)],
    log: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("无法读取源文件 {}: {}", root.display(), e))?;
    if stack.contains(&canonical) {
        let chain: Vec<String> = stack
            .iter()
            .chain(std::iter::once(&canonical))
            .map(|p| p.display().to_string())
            .collect();
        return Err(format!("本地源码 import 出现循环引用：{}", chain.join(" -> ")));
    }
    if visited.contains(&canonical) {
        return Ok(());
    }
    visited.insert(canonical.clone());
    stack.push(canonical.clone());

    let source = fs::read_to_string(&canonical)
        .map_err(|e| format!("无法读取 {}: {}", canonical.display(), e))?;

    // v1.24 Pass 0：`#` 预处理指令（词法前的文本级 pass）。
    //   先跑条件编译裁剪 + 收集 #define 宏（行号保持），再 lex，最后做 token 级宏展开。
    //   宏展开必须在 import_specs 之前，使 -D/#define 能改写 import 字符串。
    let directive_out = crate::directives::process_source(
        &source,
        &canonical.display().to_string(),
        defines,
        log,
    )
    .map_err(|e| format!("{}：{}", canonical.display(), e))?;
    let tokens = lexer::lex(&directive_out.source)
        .map_err(|e| format!("{}：{}", canonical.display(), e))?;
    let tokens = crate::directives::expand_tokens(tokens, &directive_out.macros)
        .map_err(|e| format!("{}：{}", canonical.display(), e))?;
    let specs = import_specs(&tokens);
    set.units.push(SourceUnit {
        path: canonical.clone(),
        tokens,
    });

    let base = canonical.parent().unwrap_or_else(|| Path::new("."));
    for spec in specs {
        if is_local_import(&spec, base) {
            let child = local_module_path(&spec, base);
            if !child.is_file() {
                return Err(format!(
                    "{}：本地源码 import '{}' 指向的文件不存在（解析为 {}）。\n\
                     本地模块请写相对路径（如 import \"./util.dial\";）；\n\
                     随包分发的 Dial 源码模块或已安装的包请写裸包名。",
                    canonical.display(),
                    spec,
                    child.display()
                ));
            }
            collect_sources(&child, visited, stack, set, defines, log)?;
        } else if let Some((pkg_dir, rel)) =
            package::find_src_module(&spec).map_err(|e| e.to_string())?
        {
            let child = pkg_dir.join(&rel);
            if !child.is_file() {
                return Err(format!(
                    "{}：包缓存 {} 声明了 Dial 源码模块 '{}'（src['{}'] = '{}'），\n\
                     但文件不存在：{}。包可能已损坏，请重新安装。",
                    canonical.display(),
                    pkg_dir.display(),
                    spec,
                    spec,
                    rel,
                    child.display()
                ));
            }
            collect_sources(&child, visited, stack, set, defines, log)?;
        } else if !set.imports.contains(&spec) {
            set.imports.push(spec);
        }
    }
    stack.pop();
    Ok(())
}

// ---------------------------------------------------------------------------
//  FFI link 结算
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FfiLibKind {
    Static,
    Dynamic,
}

struct FfiLib {
    path: PathBuf,
    kind: FfiLibKind,
}

/// v1.04 FFI：校验并解析 `link "...";` 声明的库路径（五条硬约束见原 main.rs）。
fn resolve_ffi_link(spec: &str, src_path: &Path) -> Result<FfiLib, String> {
    let file_part = Path::new(spec)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(spec);
    let lower = file_part.to_ascii_lowercase();

    let (kind, expected) = if lower.ends_with(".a") || lower.ends_with(".lib") {
        (FfiLibKind::Static, "ar 归档")
    } else if lower.ends_with(".so") || lower.contains(".so.") {
        (FfiLibKind::Dynamic, "ELF 共享对象")
    } else if lower.ends_with(".dll") {
        (FfiLibKind::Dynamic, "PE 动态库")
    } else if lower.ends_with(".dylib") {
        (FfiLibKind::Dynamic, "Mach-O 动态库")
    } else {
        return Err(format!(
            "link 的路径必须是静态归档 .a / .lib 或动态库 .so / .dll / .dylib，遇到 '{}'；\
             系统库（libc / libm）由 C 编译器默认链接，不需要也不允许用 link 声明",
            spec
        ));
    };

    let raw = Path::new(spec);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        let base = src_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        base.join(raw)
    };
    let canonical = candidate.canonicalize().map_err(|e| {
        format!(
            "link 声明的库 '{}' 无法读取（解析为 {}）：{}",
            spec,
            candidate.display(),
            e
        )
    })?;

    let mut f = fs::File::open(&canonical)
        .map_err(|e| format!("无法打开库 '{}'：{}", canonical.display(), e))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).map_err(|_| {
        format!(
            "库 '{}' 内容不完整（不足 8 字节），不是合法的 {}",
            canonical.display(),
            expected
        )
    })?;

    let m4 = &magic[..4];
    let ok = match kind {
        FfiLibKind::Static => &magic == b"!<arch>\n",
        FfiLibKind::Dynamic => {
            if lower.ends_with(".so") || lower.contains(".so.") {
                m4 == &b"\x7fELF"[..]
            } else if lower.ends_with(".dll") {
                &magic[..2] == &b"MZ"[..]
            } else {
                const MACHO: [[u8; 4]; 8] = [
                    [0xfe, 0xed, 0xfa, 0xce], [0xce, 0xfa, 0xed, 0xfe],
                    [0xfe, 0xed, 0xfa, 0xcf], [0xcf, 0xfa, 0xed, 0xfe],
                    [0xca, 0xfe, 0xba, 0xbe], [0xbe, 0xba, 0xfe, 0xca],
                    [0xca, 0xfe, 0xba, 0xbf], [0xbf, 0xba, 0xfe, 0xca],
                ];
                MACHO.iter().any(|m| m4 == &m[..])
            }
        }
    };
    if !ok {
        return Err(format!(
            "{} '{}' 缺少合法的{}魔数，后缀对了但内容不是真正的库（拒绝伪装文件）",
            if kind == FfiLibKind::Static { "静态库" } else { "动态库" },
            canonical.display(),
            expected
        ));
    }

    Ok(FfiLib { path: canonical, kind })
}

// ---------------------------------------------------------------------------
//  v1.23 Pass 1：条件编译求值（@cfg）
// ---------------------------------------------------------------------------

/// 求一个 cfg 条件的真假。
///
/// - `Name(k)`：查事实表，存在且值非 `0`/`false`/`` 即为真；不存在为 false。
/// - `Eq(k, v)`：事实表中 `k == v` 为真。
/// - `Not/All/Any`：标准布尔组合。
fn eval_cfg(cond: &crate::ast::CfgCond, facts: &HashMap<String, String>) -> bool {
    use crate::ast::CfgCond;
    match cond {
        CfgCond::Name(k) => facts
            .get(k)
            .map(|v| !matches!(v.as_str(), "0" | "false" | "False" | "FALSE" | ""))
            .unwrap_or(false),
        CfgCond::Eq(k, v) => facts.get(k).map(|x| x == v).unwrap_or(false),
        CfgCond::Not(c) => !eval_cfg(c, facts),
        CfgCond::All(cs) => cs.iter().all(|c| eval_cfg(c, facts)),
        CfgCond::Any(cs) => cs.iter().any(|c| eval_cfg(c, facts)),
    }
}

/// 处理一条语句的 cfg：递归子语句 → 若本层带 cfg 且为假则删除、为真则清标记。
fn cfg_stmts(stmts: &mut Vec<crate::ast::Stmt>, facts: &HashMap<String, String>) {
    for s in stmts.iter_mut() {
        cfg_stmt_children(s, facts);
    }
    stmts.retain(|s| {
        let c = match s {
            crate::ast::Stmt::Block { cfg, .. } => cfg.as_ref(),
            _ => None,
        };
        c.map(|x| eval_cfg(x, facts)).unwrap_or(true)
    });
    // 保留下来的块清掉已求值的 cfg 标记（后续语义/后端不再关心）。
    for s in stmts.iter_mut() {
        if let crate::ast::Stmt::Block { cfg, .. } = s {
            *cfg = None;
        }
    }
}

/// 递归进所有含子语句的 Stmt 变体（字段参照 semantic::desugar_stmts）。
fn cfg_stmt_children(s: &mut crate::ast::Stmt, facts: &HashMap<String, String>) {
    use crate::ast::Stmt;
    match s {
        Stmt::If { then_branch, else_branch, .. } => {
            cfg_stmts(then_branch, facts);
            if let Some(e) = else_branch { cfg_stmts(e, facts); }
        }
        Stmt::While { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::ForEachMap { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::With { body, .. }
        | Stmt::Block { body, .. } => cfg_stmts(body, facts),
        Stmt::Match { arms, .. } => {
            for arm in arms { cfg_stmts(&mut arm.body, facts); }
        }
        Stmt::Try { body, catch, .. } => {
            cfg_stmts(body, facts);
            cfg_stmts(&mut catch.body, facts);
        }
        Stmt::Select { arms, default_body, timeout_body, .. } => {
            for arm in arms { cfg_stmts(&mut arm.body, facts); }
            if let Some(d) = default_body { cfg_stmts(d, facts); }
            if let Some(t) = timeout_body { cfg_stmts(t, facts); }
        }
        Stmt::Defer(inner, ..) => cfg_stmt_children(inner, facts),
        _ => {}
    }
}

/// 对整个 Program 跑条件编译：顶层函数/结构体/测试块 + 各自函数体。
fn cfg_program(prog: &mut crate::ast::Program, facts: &HashMap<String, String>) {
    // 顶层函数：条件为假者标记删除，为真者清标记并递归处理函数体。
    for f in prog.functions.iter_mut() {
        let keep = f.cfg.as_ref().map(|c| eval_cfg(c, facts)).unwrap_or(true);
        if keep {
            f.cfg = None;
            cfg_stmts(&mut f.body, facts);
        }
    }
    prog.functions.retain(|f| f.cfg.is_none());

    for st in prog.structs.iter_mut() {
        if st.cfg.as_ref().map(|c| eval_cfg(c, facts)).unwrap_or(true) {
            st.cfg = None;
        }
    }
    prog.structs.retain(|st| st.cfg.is_none());

    for t in prog.tests.iter_mut() {
        cfg_stmts(&mut t.body, facts);
    }
}

// ---------------------------------------------------------------------------
//  v1.23 Pass 2：AST 宏展开
// ---------------------------------------------------------------------------

/// 宏调用最大递归深度（防御自递归宏导致无限展开）。
const MACRO_MAX_DEPTH: usize = 64;

type MacroTable = HashMap<String, crate::ast::MacroDef>;

/// 用实参替换表达式里所有以 `$` 开头的形参占位符（Expr::Var("$x")）。
fn subst_expr(e: &mut crate::ast::Expr, params: &[String], args: &[crate::ast::Expr]) {
    use crate::ast::Expr;
    match e {
        Expr::Var(name) => {
            if let Some(idx) = params.iter().position(|p| p == name) {
                if let Some(arg) = args.get(idx) {
                    *e = arg.clone();
                }
            }
        }
        Expr::Field(obj, ..) => subst_expr(obj, params, args),
        Expr::Method(obj, _, a) => {
            subst_expr(obj, params, args);
            for x in a.iter_mut() { subst_expr(x, params, args); }
        }
        Expr::Index(obj, idx) => {
            subst_expr(obj, params, args);
            subst_expr(idx, params, args);
        }
        Expr::Slice(arr, from, to) => {
            subst_expr(arr, params, args);
            subst_expr(from, params, args);
            subst_expr(to, params, args);
        }
        Expr::ArrayLit(xs, _) => {
            for x in xs.iter_mut() { subst_expr(x, params, args); }
        }
        Expr::Interp(parts) => {
            for part in parts.iter_mut() {
                if let crate::ast::InterpPart::Expr(x) = part {
                    subst_expr(x, params, args);
                }
            }
        }
        Expr::StructLit(_, fields) => {
            for (_, x) in fields.iter_mut() { subst_expr(x, params, args); }
        }
        Expr::Binary(l, _, r) => {
            subst_expr(l, params, args);
            subst_expr(r, params, args);
        }
        Expr::Ternary(c, a, b) => {
            subst_expr(c, params, args);
            subst_expr(a, params, args);
            subst_expr(b, params, args);
        }
        Expr::Unary(_, x) | Expr::Await(x) => subst_expr(x, params, args),
        Expr::Call(_, a) => {
            for x in a.iter_mut() { subst_expr(x, params, args); }
        }
        // 字面量 / TypeLit / FuncRef 等无子表达式，无需处理。
        _ => {}
    }
}

/// 递归展开语句序列里的宏调用。
///
/// 宏调用表现为 `Stmt::Expr(Expr::Call(name, args), ..)`：若 `name` 命中宏表，
/// 则把宏体（参数替换后）内联到调用点，替换这一条语句为多条语句。
fn expand_macros_in_stmts(
    stmts: &mut Vec<crate::ast::Stmt>,
    table: &MacroTable,
    depth: usize,
) -> Result<(), String> {
    if depth > MACRO_MAX_DEPTH {
        return Err(format!("AST 宏展开超过最大深度 {}（可能存在递归宏）", MACRO_MAX_DEPTH));
    }
    // 先递归处理每条语句的子块，再在本层做宏内联。
    let mut out: Vec<crate::ast::Stmt> = Vec::with_capacity(stmts.len());
    for stmt in stmts.drain(..) {
        let mut s = stmt;
        macro_children(&mut s, table, depth)?;
        // 本层是否是宏调用？
        let call = match &s {
            crate::ast::Stmt::Expr(crate::ast::Expr::Call(name, args), line, col) => {
                table.get(name).map(|d| (d.clone(), args.clone(), *line, *col))
            }
            _ => None,
        };
        if let Some((def, args, line, col)) = call {
            if args.len() != def.params.len() {
                return Err(format!(
                    "第 {} 行 第 {} 列：宏 '{}' 需要 {} 个参数，实际 {} 个",
                    line, col, def.name, def.params.len(), args.len()
                ));
            }
            let mut body = def.body.clone();
            for st in body.iter_mut() {
                subst_stmt(st, &def.params, &args);
            }
            // 宏体本身可能调用其它宏：递归展开后再内联。
            expand_macros_in_stmts(&mut body, table, depth + 1)?;
            out.extend(body);
        } else {
            out.push(s);
        }
    }
    *stmts = out;
    Ok(())
}

/// 对单条语句递归进子块做宏展开（与 cfg_stmt_children 覆盖集一致）。
fn macro_children(s: &mut crate::ast::Stmt, table: &MacroTable, depth: usize) -> Result<(), String> {
    use crate::ast::Stmt;
    match s {
        Stmt::If { then_branch, else_branch, .. } => {
            expand_macros_in_stmts(then_branch, table, depth)?;
            if let Some(e) = else_branch { expand_macros_in_stmts(e, table, depth)?; }
        }
        Stmt::While { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::ForEachMap { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::With { body, .. }
        | Stmt::Block { body, .. } => expand_macros_in_stmts(body, table, depth)?,
        Stmt::Match { arms, .. } => {
            for arm in arms { expand_macros_in_stmts(&mut arm.body, table, depth)?; }
        }
        Stmt::Try { body, catch, .. } => {
            expand_macros_in_stmts(body, table, depth)?;
            expand_macros_in_stmts(&mut catch.body, table, depth)?;
        }
        Stmt::Select { arms, default_body, timeout_body, .. } => {
            for arm in arms { expand_macros_in_stmts(&mut arm.body, table, depth)?; }
            if let Some(d) = default_body { expand_macros_in_stmts(d, table, depth)?; }
            if let Some(t) = timeout_body { expand_macros_in_stmts(t, table, depth)?; }
        }
        Stmt::Defer(inner, ..) => macro_children(inner, table, depth)?,
        _ => {}
    }
    Ok(())
}

/// 对单条语句里的所有表达式做形参替换（宏体内联前调用）。
fn subst_stmt(s: &mut crate::ast::Stmt, params: &[String], args: &[crate::ast::Expr]) {
    use crate::ast::Stmt;
    match s {
        Stmt::VarDecl { value, .. } => subst_expr(value, params, args),
        Stmt::Assign { value, .. }
        | Stmt::Throw(value, ..)
        | Stmt::Expr(value, ..) => subst_expr(value, params, args),
        Stmt::AssignDeref { ptr, value, .. } => {
            subst_expr(ptr, params, args);
            subst_expr(value, params, args);
        }
        Stmt::AssignField { obj, value, .. } => {
            subst_expr(obj, params, args);
            subst_expr(value, params, args);
        }
        Stmt::AssignIndex { arr, index, value, .. } => {
            subst_expr(arr, params, args);
            subst_expr(index, params, args);
            subst_expr(value, params, args);
        }
        Stmt::If { cond, then_branch, else_branch, .. } => {
            subst_expr(cond, params, args);
            for x in then_branch.iter_mut() { subst_stmt(x, params, args); }
            if let Some(e) = else_branch { for x in e.iter_mut() { subst_stmt(x, params, args); } }
        }
        Stmt::While { cond, body, .. } | Stmt::DoWhile { cond, body, .. } => {
            subst_expr(cond, params, args);
            for x in body.iter_mut() { subst_stmt(x, params, args); }
        }
        Stmt::For { start, end, step, body, .. } => {
            subst_expr(start, params, args);
            subst_expr(end, params, args);
            if let Some(e) = step { subst_expr(e, params, args); }
            for x in body.iter_mut() { subst_stmt(x, params, args); }
        }
        Stmt::ForEach { coll, body, .. } | Stmt::ForEachMap { coll, body, .. } => {
            subst_expr(coll, params, args);
            for x in body.iter_mut() { subst_stmt(x, params, args); }
        }
        Stmt::With { resource, body, .. } => {
            subst_expr(resource, params, args);
            for x in body.iter_mut() { subst_stmt(x, params, args); }
        }
        Stmt::Match { subject, arms, .. } => {
            subst_expr(subject, params, args);
            for arm in arms.iter_mut() { for x in arm.body.iter_mut() { subst_stmt(x, params, args); } }
        }
        Stmt::Try { body, catch, .. } => {
            for x in body.iter_mut() { subst_stmt(x, params, args); }
            for x in catch.body.iter_mut() { subst_stmt(x, params, args); }
        }
        Stmt::Select { arms, default_body, timeout, timeout_body, .. } => {
            for arm in arms.iter_mut() {
                subst_expr(&mut arm.chan, params, args);
                for x in arm.body.iter_mut() { subst_stmt(x, params, args); }
            }
            if let Some(db) = default_body { for x in db.iter_mut() { subst_stmt(x, params, args); } }
            if let Some(te) = timeout { subst_expr(te, params, args); }
            if let Some(tb) = timeout_body { for x in tb.iter_mut() { subst_stmt(x, params, args); } }
        }
        Stmt::Block { body, .. } => { for x in body.iter_mut() { subst_stmt(x, params, args); } }
        Stmt::Defer(inner, ..) => subst_stmt(inner, params, args),
        Stmt::Return(None, ..) | Stmt::Break(..) | Stmt::Continue(..) | Stmt::Label(..) | Stmt::Goto(..) => {}
        _ => {}
    }
}

/// 对整个 Program 展开宏：构建宏表 → 展开各函数体/测试体 → 清空宏定义。
fn expand_macros_program(prog: &mut crate::ast::Program) -> Result<(), String> {
    if prog.macros.is_empty() { return Ok(()); }
    // 重名宏检测（Fail-Fast）。
    let mut table: MacroTable = HashMap::new();
    for m in &prog.macros {
        if table.insert(m.name.clone(), m.clone()).is_some() {
            return Err(format!("第 {} 行 第 {} 列：宏 '{}' 重复定义", m.line, m.col, m.name));
        }
    }
    for f in prog.functions.iter_mut() {
        expand_macros_in_stmts(&mut f.body, &table, 0)?;
    }
    for t in prog.tests.iter_mut() {
        expand_macros_in_stmts(&mut t.body, &table, 0)?;
    }
    // 宏展开完毕后从 program 移除定义（raw_program 保留，供 LSP 查看）。
    prog.macros.clear();
    Ok(())
}

// ---------------------------------------------------------------------------
//  阶段入口
// ---------------------------------------------------------------------------

/// 执行预处理阶段：源码 → 干净 AST + 已结算的外部依赖。
///
/// 顺序：递归收集源码 → 解析包（拿不透明类型）→ 逐文件 parse → AST 合并（冲突检测）
/// → 语言层 extern/link 并入 → 编译期裁剪（debug）。
/// 合并时把每个文件 `link` 的相对路径按**该文件所在目录**绝对化。
pub fn run(
    src_path: &str,
    log: &mut dyn FnMut(&str),
    options: &PreprocessOptions,
) -> Result<PreprocessOutput, String> {
    let mut set = SourceSet {
        units: Vec::new(),
        imports: Vec::new(),
    };
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    collect_sources(
        Path::new(src_path),
        &mut visited,
        &mut stack,
        &mut set,
        &options.defines,
        log,
    )?;

    let (mut externs, mut pkg_libs, opaque_types) = crate::resolve_imports(&set.imports, log)
        .map_err(|e| {
            if e.contains("找不到已安装的包") {
                format!(
                    "{}\n（提示：import 按三级解析——① 同目录 .dial 源码（写 import \"./xxx.dial\"; 或裸名 import \"xxx\";）；② 已安装包 src/ 里 [src] 段声明的 Dial 源码模块（裸名，装包即用、无需复制）；③ 包名（读 .diali + 链 .a）。① ② 都会自动合并编译。见 README §34）",
                    e
                )
            } else {
                e
            }
        })?;

    let mut program = crate::ast::Program {
        imports: set.imports.clone(),
        structs: Vec::new(),
        functions: Vec::new(),
        externs: Vec::new(),
        links: Vec::new(),
        cstructs: Vec::new(),
        tests: Vec::new(),
        macros: Vec::new(),
    };
    let mut fn_origin: HashMap<String, String> = HashMap::new();
    let mut struct_origin: HashMap<String, String> = HashMap::new();

    for unit in &set.units {
        let mut p = parser::parse_with(unit.tokens.clone(), &opaque_types)
            .map_err(|e| format!("{}：{}", unit.path.display(), e))?;
        let origin = unit.path.display().to_string();
        for f in &p.functions {
            if let Some(prev) = fn_origin.get(&f.name) {
                return Err(format!(
                    "函数 '{}' 重复定义：{} 与 {}（本地源码模块合并后不允许同名函数）",
                    f.name, prev, origin
                ));
            }
            fn_origin.insert(f.name.clone(), origin.clone());
        }
        for st in &p.structs {
            if let Some(prev) = struct_origin.get(&st.name) {
                return Err(format!(
                    "结构体 '{}' 重复定义：{} 与 {}",
                    st.name, prev, origin
                ));
            }
            struct_origin.insert(st.name.clone(), origin.clone());
        }
        let base = unit.path.parent().unwrap_or_else(|| Path::new("."));
        for l in &mut p.links {
            let raw = Path::new(&l.path);
            if !raw.is_absolute() {
                l.path = base.join(raw).to_string_lossy().into_owned();
            }
        }
        program.structs.append(&mut p.structs);
        program.functions.append(&mut p.functions);
        program.externs.append(&mut p.externs);
        program.links.append(&mut p.links);
        program.cstructs.append(&mut p.cstructs);
        program.tests.append(&mut p.tests);
        program.macros.append(&mut p.macros);
    }

    // 语言层 extern fn 并入外部函数表（与包 .diali 一视同仁）。
    for e in &program.externs {
        if externs.iter().any(|(n, _)| n == &e.name) {
            return Err(format!(
                "外部函数 '{}' 重复声明（包 .diali 与源码 extern fn 撞名）",
                e.name
            ));
        }
        externs.push((
            e.name.clone(),
            semantic::FuncInfo {
                params: e.params.iter().map(|(_, t)| t.clone()).collect(),
                own_params: vec![false; e.params.len()],
                ret: e.ret.clone(),
                defaults: vec![None; e.params.len()],
                variadic: e.variadic,
                exported: true,
                is_async: false,
            },
        ));
    }

    // link 声明的库按后缀验魔数，动态库额外收集 rpath。
    let mut ffi_rpaths: Vec<PathBuf> = Vec::new();
    for l in &program.links {
        match resolve_ffi_link(&l.path, Path::new(src_path)) {
            Ok(lib) => {
                if lib.kind == FfiLibKind::Dynamic {
                    if let Some(dir) = lib.path.parent() {
                        if !ffi_rpaths.iter().any(|d| d.as_path() == dir) {
                            ffi_rpaths.push(dir.to_path_buf());
                        }
                    }
                }
                pkg_libs.push(lib.path);
            }
            Err(msg) => {
                return Err(format!("第 {} 行 第 {} 列：{}", l.line, l.col, msg));
            }
        }
    }

    // v1.23 Pass 1+2：宏展开与条件编译求值。二者必须在所有产物（含 dialc test 用的
    // raw_program）上生效——否则 test 代码生成会遇到未展开的宏调用或未求值的 cfg 项。
    // 顺序：先宏展开（可能引入新的 @cfg 块）→ 再 cfg 求值。
    expand_macros_program(&mut program).map_err(|e| format!("{}：{}", src_path, e))?;
    cfg_program(&mut program, &options.cfg_facts);

    // raw_program = 宏已展开、cfg 已求值、但**保留 debug 代码**的 AST（供 dialc test）。
    let raw_program = program.clone();
    if !options.keep_debug {
        semantic::strip_debug_program(&mut program);
    }

    Ok(PreprocessOutput {
        program,
        raw_program,
        externs,
        pkg_libs,
        ffi_rpaths,
        fn_origin,
    })
}

// ---------------------------------------------------------------------------
//  单元测试：预处理阶段纯函数的行为契约（扩展基座）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_default_keeps_debug() {
        let o = PreprocessOptions::default();
        assert!(o.keep_debug, "默认（debug/check/test）必须保留调试代码");
    }

    #[test]
    fn local_import_recognition() {
        assert!(is_local_import("./util.dial", Path::new(".")));
        assert!(is_local_import("../common/util", Path::new(".")));
        assert!(is_local_import(".\\util.dial", Path::new(".")), "Windows 相对路径");
        assert!(!is_local_import("sub\\win", Path::new(".")), "含 \\ 但非 .\\ 开头不算本地");
        // 裸名：同目录确有 .dial 才算本地
        let dir = std::env::temp_dir().join("dialc_preprocess_local_probe");
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("helper.dial");
        let _ = fs::write(&f, "");
        assert!(is_local_import("helper", &dir), "同目录存在 helper.dial 应判为本地");
        assert!(!is_local_import("nonexistent_pkg", &dir), "裸名无源码应判为包名");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_module_path_extension() {
        let base = Path::new("/proj");
        assert_eq!(local_module_path("./util", base), base.join("util.dial"));
        assert_eq!(local_module_path("util.dial", base), base.join("util.dial"));
    }

    #[test]
    fn import_specs_scans_tokens() {
        let tokens = lexer::lex("import \"a\"; import \"b\"; fn main() {}").unwrap();
        assert_eq!(import_specs(&tokens), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn cfg_eval_boolean_and_eq() {
        use crate::ast::CfgCond;
        let mut facts = HashMap::new();
        facts.insert("debug".into(), "1".into());
        facts.insert("off".into(), "0".into());
        facts.insert("target_os".into(), "linux".into());
        assert!(eval_cfg(&CfgCond::Name("debug".into()), &facts));
        assert!(!eval_cfg(&CfgCond::Name("off".into()), &facts));
        assert!(!eval_cfg(&CfgCond::Name("missing".into()), &facts));
        assert!(eval_cfg(&CfgCond::Eq("target_os".into(), "linux".into()), &facts));
        assert!(!eval_cfg(&CfgCond::Eq("target_os".into(), "windows".into()), &facts));
        assert!(eval_cfg(&CfgCond::Not(Box::new(CfgCond::Name("off".into()))), &facts));
        assert!(eval_cfg(
            &CfgCond::All(vec![CfgCond::Name("debug".into()), CfgCond::Eq("target_os".into(), "linux".into())]),
            &facts
        ));
        assert!(eval_cfg(
            &CfgCond::Any(vec![CfgCond::Name("off".into()), CfgCond::Name("debug".into())]),
            &facts
        ));
    }

    #[test]
    fn dollar_ident_lexes_as_macro_param() {
        let tokens = lexer::lex("macro m($a, $item) { print($a); }").unwrap();
        let idents: Vec<String> = tokens.iter().filter_map(|t| {
            if let lexer::TokenKind::Ident(s) = &t.kind { Some(s.clone()) } else { None }
        }).collect();
        assert!(idents.contains(&"$a".to_string()));
        assert!(idents.contains(&"$item".to_string()));
    }
}
