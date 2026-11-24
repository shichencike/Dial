//! `#` 预处理指令（v1.24）：编译流水线最前端的**文本级**预处理层。
//!
//! 定位：位于「读取源文件文本」与「词法分析 `lexer::lex`」之间，比
//! [`crate::preprocess`] 的 AST 级 pass（`@cfg` / `macro`）更早——因为 `#` 指令是
//! **行级文本**语义，必须在词法之前完成条件裁剪与宏展开。
//!
//! 三种能力（与 `@cfg` / `macro` 并存互补，职责不重叠）：
//!   1. 条件编译：`#ifdef / #ifndef / #if / #elif / #else / #endif`，支持
//!      `defined()`、整数字面量、字符串比较与完整 C 运算符优先级。
//!   2. 宏：`#define / #undef`，对象式 + 函数式，**token 级**替换（贴近 C 语义，
//!      不误伤字符串字面量内部的同名文本）。
//!   3. 诊断与指令：`#error` 立即 Fail-Fast；`#pragma` 处理 `once` / 记录未知指令。
//!
//! 与 `@cfg` 的分工：`#ifdef` 查的是本模块的宏表（`#define` / CLI `-D`）；`@cfg`
//! 查的是 `cfg_facts`（内置 debug/target_os/target_arch + `--cfg`）。两者并行求值，
//! 互不干扰——宏是文本替换，cfg 是 AST 裁剪。
//!
//! 第一版边界（明确不做）：`#include`（已有 import 模块系统，见 README §34）、
//! 行尾续行 `\`、`#` / `##` 字符串化与 token 连接、宏可变参数。所有未闭合/孤立
//! 指令一律 Fail-Fast，绝不静默吞掉源码。

use std::collections::{HashMap, HashSet};

use crate::lexer::{self, Token as LToken, TokenKind as LKind};

// ---------------------------------------------------------------------------
//  宏定义
// ---------------------------------------------------------------------------

/// 一条宏定义。`params = None` 表示对象式；`Some(vec)` 表示函数式（形参名）。
/// 同时保存 `body_text`（供 `#if` 表达式递归求值）与 `body_tokens`（供 token 展开）。
#[derive(Clone, Debug)]
pub struct MacroDef {
    pub name: String,
    pub params: Option<Vec<String>>,
    pub body_text: String,
    pub body_tokens: Vec<LToken>,
    pub line: usize,
}

pub type MacroTable = HashMap<String, MacroDef>;

/// 文本级 pass 的产物。
pub struct DirectiveOutput {
    /// 条件裁剪后的源码文本。**行数与原文件严格一致**（裁掉的行替换为空行），
    /// 保证后续 lexer 的行号与原文件一致，诊断定位不漂移。
    pub source: String,
    /// 汇总后的宏表（含 CLI `-D` 注入），供 token 级展开与 `@cfg` 之外的条件求值。
    pub macros: MacroTable,
}

// ---------------------------------------------------------------------------
//  阶段 1：文本级指令 pass
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct CondFrame {
    parent_active: bool,
    taken: bool,
    active: bool,
    else_seen: bool,
}

fn stack_active(stack: &[CondFrame]) -> bool {
    stack.iter().all(|f| f.active)
}

/// 从 `-D NAME` / `-D NAME=value` 构造初始宏表（等价于源文件最顶部的 `#define`）。
fn cli_macros(defines: &[(String, Option<String>)]) -> MacroTable {
    let mut table = MacroTable::new();
    for (name, value) in defines {
        let body = value.clone().unwrap_or_else(|| "1".to_string());
        let toks = lex_body(&body, 0, 0).unwrap_or_default();
        table.insert(
            name.clone(),
            MacroDef {
                name: name.clone(),
                params: None,
                body_text: body,
                body_tokens: toks,
                line: 0,
            },
        );
    }
    table
}

/// 把宏体文本 lex 成 token 序列（去掉 lexer 自动追加的 EOF）。
fn lex_body(text: &str, line: usize, col: usize) -> Result<Vec<LToken>, String> {
    let mut toks = lexer::lex(text)
        .map_err(|e| format!("第 {} 行 第 {} 列：宏体词法错误：{}", line, col, e))?;
    if matches!(toks.last().map(|t| &t.kind), Some(LKind::EOF)) {
        toks.pop();
    }
    Ok(toks)
}

/// 解析 `#define` 的参数串（形如 `NAME` 或 `NAME(a, b) body...`）。
fn parse_define(
    rest: &str,
    line: usize,
    col: usize,
) -> Result<MacroDef, String> {
    let s = rest.trim();
    if s.is_empty() {
        return Err(format!("第 {} 行 第 {} 列：#define 缺少宏名", line, col));
    }
    let name_end = s
        .char_indices()
        .find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '_'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let name = &s[..name_end];
    if name.is_empty()
        || !name.chars().next().unwrap().is_ascii_alphabetic() && name.chars().next().unwrap() != '_'
    {
        return Err(format!(
            "第 {} 行 第 {} 列：#define 宏名 '{}' 非法（须以字母或下划线开头）",
            line, col, name
        ));
    }
    let tail = &s[name_end..];
    // C 规则：函数名后必须紧接 '('（无空格）才是函数式宏。
    if tail.starts_with('(') {
        let mut depth = 0i32;
        let mut close = None;
        for (i, ch) in tail.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.ok_or_else(|| {
            format!("第 {} 行 第 {} 列：函数式宏 '{}' 的形参列表缺少 ')'", line, col, name)
        })?;
        let inner = &tail[1..close];
        let params: Vec<String> = if inner.trim().is_empty() {
            Vec::new()
        } else {
            inner.split(',').map(|p| {
                let p = p.trim();
                if p.is_empty()
                    || !p.chars().next().unwrap().is_ascii_alphabetic() && p.chars().next().unwrap() != '_'
                {
                    Err(format!("第 {} 行 第 {} 列：宏 '{}' 的形参名 '{}' 非法", line, col, name, p))
                } else {
                    Ok(p.to_string())
                }
            }).collect::<Result<Vec<_>, String>>()?
        };
        // 形参重名 Fail-Fast。
        let mut seen = HashSet::new();
        for p in &params {
            if !seen.insert(p.clone()) {
                return Err(format!("第 {} 行 第 {} 列：宏 '{}' 的形参 '{}' 重复", line, col, name, p));
            }
        }
        let body_text = tail[close + 1..].trim().to_string();
        let body_tokens = lex_body(&body_text, line, col)?;
        Ok(MacroDef {
            name: name.to_string(),
            params: Some(params),
            body_text,
            body_tokens,
            line,
        })
    } else {
        let body_text = tail.trim().to_string();
        let body_tokens = lex_body(&body_text, line, col)?;
        Ok(MacroDef {
            name: name.to_string(),
            params: None,
            body_text,
            body_tokens,
            line,
        })
    }
}

/// 文本级指令 pass：逐行处理 `#` 指令，输出行号保持的裁剪源码 + 宏表。
pub fn process_source(
    src: &str,
    path: &str,
    cli_defines: &[(String, Option<String>)],
    log: &mut dyn FnMut(&str),
) -> Result<DirectiveOutput, String> {
    let mut macros = cli_macros(cli_defines);
    let mut stack: Vec<CondFrame> = Vec::new();
    let mut out: Vec<String> = Vec::new();

    for (idx, raw_line) in src.lines().enumerate() {
        let line = idx + 1;
        let trimmed = raw_line.trim_start();
        let col = raw_line.len() - trimmed.len() + 1;

        if !trimmed.starts_with('#') {
            // 非指令行：仅当所有条件帧活跃时保留，否则填空行保持行号。
            out.push(if stack_active(&stack) { raw_line.to_string() } else { String::new() });
            continue;
        }

        // 指令行本身不进入源码（填空行保持行号）。
        out.push(String::new());
        let after = trimmed[1..].trim();
        if after.is_empty() {
            continue; // 单独一个 '#'：C 惯例为空指令，忽略。
        }
        let split = after.find(char::is_whitespace);
        let (directive, args) = match split {
            Some(i) => (&after[..i], after[i..].trim()),
            None => (after, ""),
        };
        let active = stack_active(&stack);

        match directive {
            "ifdef" | "ifndef" => {
                let name = args.split_whitespace().next().unwrap_or("");
                if name.is_empty() {
                    return Err(format!("{}:{}: #{} 缺少宏名", path, line, directive));
                }
                let is_def = macros.contains_key(name);
                let cond = if directive == "ifdef" { is_def } else { !is_def };
                stack.push(CondFrame {
                    parent_active: active,
                    taken: active && cond,
                    active: active && cond,
                    else_seen: false,
                });
            }
            "if" => {
                if args.is_empty() {
                    return Err(format!("{}:{}: #if 缺少表达式", path, line));
                }
                let cond = active && eval_condition(args, &macros, 0).map_err(|e| {
                    format!("{}:{}: #if 表达式错误：{}", path, line, e)
                })?;
                stack.push(CondFrame {
                    parent_active: active,
                    taken: cond,
                    active: cond,
                    else_seen: false,
                });
            }
            "elif" => {
                let frame = stack.last_mut().ok_or_else(|| {
                    format!("{}:{}: #elif 没有匹配的 #if", path, line)
                })?;
                if frame.else_seen {
                    return Err(format!("{}:{}: #elif 出现在 #else 之后", path, line));
                }
                if args.is_empty() {
                    return Err(format!("{}:{}: #elif 缺少表达式", path, line));
                }
                let cond = frame.parent_active && !frame.taken
                    && eval_condition(args, &macros, 0).map_err(|e| {
                        format!("{}:{}: #elif 表达式错误：{}", path, line, e)
                    })?;
                frame.active = cond;
                if cond { frame.taken = true; }
            }
            "else" => {
                let frame = stack.last_mut().ok_or_else(|| {
                    format!("{}:{}: #else 没有匹配的 #if", path, line)
                })?;
                if frame.else_seen {
                    return Err(format!("{}:{}: 重复的 #else", path, line));
                }
                frame.active = frame.parent_active && !frame.taken;
                frame.taken = true;
                frame.else_seen = true;
            }
            "endif" => {
                if stack.pop().is_none() {
                    return Err(format!("{}:{}: #endif 没有匹配的 #if", path, line));
                }
            }
            "define" => {
                if active {
                    let def = parse_define(args, line, col)
                        .map_err(|e| format!("{}:{}: {}", path, line, e.trim_start_matches("第 ")))?;
                    if let Some(prev) = macros.get(&def.name) {
                        if prev.body_text != def.body_text || prev.params != def.params {
                            return Err(format!(
                                "{}:{}: 宏 '{}' 重复定义且内容不同（前一次在第 {} 行）；\
                                 如需重定义请先 #undef",
                                path, line, def.name, prev.line
                            ));
                        }
                    } else {
                        macros.insert(def.name.clone(), def);
                    }
                }
            }
            "undef" => {
                if active {
                    let name = args.split_whitespace().next().unwrap_or("");
                    if name.is_empty() {
                        return Err(format!("{}:{}: #undef 缺少宏名", path, line));
                    }
                    macros.remove(name); // 未定义的宏 #undef：C 惯例静默忽略。
                }
            }
            "error" => {
                if active {
                    return Err(format!("{}:{}: 预处理 #error：{}", path, line, args));
                }
            }
            "pragma" => {
                if active {
                    match args.split_whitespace().next().unwrap_or("") {
                        "once" => {
                            // 本编译器已由 import 模块系统的 visited 集合保证每个源文件
                            // 只处理一次（见 preprocess::collect_sources），故 once 是天然满足的。
                        }
                        "" => return Err(format!("{}:{}: #pragma 缺少指令名", path, line)),
                        other => {
                            log(&format!("ℹ️  {}:{}: 忽略未知 #pragma {}", path, line, other));
                        }
                    }
                }
            }
            other => {
                // 假分支内的未知指令随整块裁剪一并忽略（与 #error 同理）；
                // 但 #endif/#else/#elif 等结构性指令在各自分支中始终处理，不受此限。
                if active {
                    return Err(format!("{}:{}: 未知的预处理指令 '#{}'", path, line, other));
                }
            }
        }
    }

    if let Some(frame) = stack.last() {
        let _ = frame;
        return Err(format!("{}: 预处理条件块未闭合（缺少 #endif，共 {} 层）", path, stack.len()));
    }

    let mut source = out.join("\n");
    if src.ends_with('\n') {
        source.push('\n');
    }
    Ok(DirectiveOutput { source, macros })
}

// ---------------------------------------------------------------------------
//  阶段 1b：`#if` 条件表达式求值（独立轻量 lexer + 递归下降）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum ETok {
    Int(i64),
    Str(String),
    Ident(String),
    LParen,
    RParen,
    Op(String),
    End,
}

fn lex_expr(s: &str) -> Result<Vec<ETok>, String> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() { i += 1; continue; }
        // 字符串字面量
        if ch == '"' {
            i += 1;
            let mut val = String::new();
            let mut closed = false;
            while i < c.len() {
                if c[i] == '"' { i += 1; closed = true; break; }
                if c[i] == '\\' && i + 1 < c.len() {
                    i += 1;
                    match c[i] {
                        'n' => val.push('\n'), 't' => val.push('\t'),
                        'r' => val.push('\r'), '0' => val.push('\0'),
                        other => val.push(other),
                    }
                    i += 1;
                } else {
                    val.push(c[i]); i += 1;
                }
            }
            if !closed { return Err("字符串字面量未闭合".into()); }
            out.push(ETok::Str(val));
            continue;
        }
        // 字符字面量 'x'（简单单字符）
        if ch == '\'' && i + 2 < c.len() && c[i + 2] == '\'' {
            let v = c[i + 1] as i64;
            out.push(ETok::Int(v));
            i += 3;
            continue;
        }
        // 数字
        if ch.is_ascii_digit() {
            let start = i;
            if ch == '0' && i + 1 < c.len() && matches!(c[i + 1], 'x' | 'X' | 'b' | 'B' | 'o' | 'O') {
                let radix = match c[i + 1] { 'x' | 'X' => 16, 'b' | 'B' => 2, _ => 8 };
                i += 2;
                while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') { i += 1; }
                let digits: String = c[start + 2..i].iter().filter(|x| **x != '_').collect();
                let v = i64::from_str_radix(&digits, radix)
                    .map_err(|_| format!("非法整数 '{}'", digits))?;
                out.push(ETok::Int(v));
            } else {
                while i < c.len() && (c[i].is_ascii_digit() || c[i] == '_') { i += 1; }
                let digits: String = c[start..i].iter().filter(|x| **x != '_').collect();
                let v = digits.parse::<i64>().map_err(|_| format!("非法整数 '{}'", digits))?;
                out.push(ETok::Int(v));
            }
            continue;
        }
        // 标识符
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') { i += 1; }
            out.push(ETok::Ident(c[start..i].iter().collect()));
            continue;
        }
        if ch == '(' { out.push(ETok::LParen); i += 1; continue; }
        if ch == ')' { out.push(ETok::RParen); i += 1; continue; }
        // 运算符：最长匹配优先
        let two: String = c[i..std::cmp::min(i + 2, c.len())].iter().collect();
        if matches!(two.as_str(), "<<" | ">>" | "<=" | ">=" | "==" | "!=" | "&&" | "||") {
            out.push(ETok::Op(two));
            i += 2;
            continue;
        }
        if "+-*/%<>&^|!~".contains(ch) {
            out.push(ETok::Op(ch.to_string())); i += 1; continue;
        }
        return Err(format!("表达式中出现无法识别的字符 '{}'", ch));
    }
    out.push(ETok::End);
    Ok(out)
}

#[derive(Clone, Debug)]
enum EVal { Int(i64), Str(String) }

impl EVal {
    fn truthy(&self) -> bool {
        match self {
            EVal::Int(v) => *v != 0,
            EVal::Str(s) => !s.is_empty(),
        }
    }
    fn as_int(&self) -> Result<i64, String> {
        match self {
            EVal::Int(v) => Ok(*v),
            EVal::Str(_) => Err("字符串不能用于算术/位运算".into()),
        }
    }
}

const EXPR_MAX_DEPTH: usize = 64;

struct ExprParser<'a> {
    toks: Vec<ETok>,
    pos: usize,
    macros: &'a MacroTable,
    depth: usize,
}

impl<'a> ExprParser<'a> {
    fn peek(&self) -> &ETok { self.toks.get(self.pos).unwrap_or(&ETok::End) }
    fn next(&mut self) -> ETok {
        let t = self.peek().clone();
        self.pos += 1;
        t
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), ETok::Op(s) if s == op) { self.pos += 1; true } else { false }
    }

    fn parse(&mut self) -> Result<EVal, String> {
        let v = self.or()?;
        if !matches!(self.peek(), ETok::End) {
            return Err(format!("表达式末尾有多余内容：{:?}", self.peek()));
        }
        Ok(v)
    }

    fn or(&mut self) -> Result<EVal, String> {
        let mut v = self.and()?;
        while self.eat_op("||") {
            let r = self.and()?;
            v = EVal::Int((v.truthy() || r.truthy()) as i64);
        }
        Ok(v)
    }
    fn and(&mut self) -> Result<EVal, String> {
        let mut v = self.bit_or()?;
        while self.eat_op("&&") {
            let r = self.bit_or()?;
            v = EVal::Int((v.truthy() && r.truthy()) as i64);
        }
        Ok(v)
    }
    fn bit_or(&mut self) -> Result<EVal, String> {
        let mut v = self.bit_xor()?;
        while self.eat_op("|") { let r = self.bit_xor()?; v = EVal::Int(v.as_int()? | r.as_int()?); }
        Ok(v)
    }
    fn bit_xor(&mut self) -> Result<EVal, String> {
        let mut v = self.bit_and()?;
        while self.eat_op("^") { let r = self.bit_and()?; v = EVal::Int(v.as_int()? ^ r.as_int()?); }
        Ok(v)
    }
    fn bit_and(&mut self) -> Result<EVal, String> {
        let mut v = self.equality()?;
        while self.eat_op("&") { let r = self.equality()?; v = EVal::Int(v.as_int()? & r.as_int()?); }
        Ok(v)
    }
    fn equality(&mut self) -> Result<EVal, String> {
        let mut v = self.relational()?;
        loop {
            if self.eat_op("==") {
                let r = self.relational()?;
                v = EVal::Int((vals_eq(&v, &r)) as i64);
            } else if self.eat_op("!=") {
                let r = self.relational()?;
                v = EVal::Int((!vals_eq(&v, &r)) as i64);
            } else { break; }
        }
        Ok(v)
    }
    fn relational(&mut self) -> Result<EVal, String> {
        let mut v = self.shift()?;
        loop {
            let op = match self.peek() {
                ETok::Op(s) if matches!(s.as_str(), "<" | "<=" | ">" | ">=") => s.clone(),
                _ => break,
            };
            self.pos += 1;
            let r = self.shift()?;
            let (a, b) = (v.as_int()?, r.as_int()?);
            v = EVal::Int(match op.as_str() {
                "<" => (a < b) as i64, "<=" => (a <= b) as i64,
                ">" => (a > b) as i64, _ => (a >= b) as i64,
            });
        }
        Ok(v)
    }
    fn shift(&mut self) -> Result<EVal, String> {
        let mut v = self.add()?;
        loop {
            let op = match self.peek() {
                ETok::Op(s) if s == "<<" || s == ">>" => s.clone(), _ => break,
            };
            self.pos += 1;
            let r = self.add()?;
            let (a, b) = (v.as_int()?, r.as_int()?);
            v = EVal::Int(if op == "<<" { a << b } else { a >> b });
        }
        Ok(v)
    }
    fn add(&mut self) -> Result<EVal, String> {
        let mut v = self.mul()?;
        loop {
            if self.eat_op("+") { let r = self.mul()?; v = EVal::Int(v.as_int()? + r.as_int()?); }
            else if self.eat_op("-") { let r = self.mul()?; v = EVal::Int(v.as_int()? - r.as_int()?); }
            else { break; }
        }
        Ok(v)
    }
    fn mul(&mut self) -> Result<EVal, String> {
        let mut v = self.unary()?;
        loop {
            if self.eat_op("*") { let r = self.unary()?; v = EVal::Int(v.as_int()? * r.as_int()?); }
            else if self.eat_op("/") {
                let r = self.unary()?.as_int()?;
                if r == 0 { return Err("除零".into()); }
                v = EVal::Int(v.as_int()? / r);
            } else if self.eat_op("%") {
                let r = self.unary()?.as_int()?;
                if r == 0 { return Err("取模除零".into()); }
                v = EVal::Int(v.as_int()? % r);
            } else { break; }
        }
        Ok(v)
    }
    fn unary(&mut self) -> Result<EVal, String> {
        if self.eat_op("!") { return Ok(EVal::Int((!self.unary()?.truthy()) as i64)); }
        if self.eat_op("~") { return Ok(EVal::Int(!self.unary()?.as_int()?)); }
        if self.eat_op("+") { return Ok(EVal::Int(self.unary()?.as_int()?)); }
        if self.eat_op("-") { return Ok(EVal::Int(-self.unary()?.as_int()?)); }
        self.primary()
    }
    fn primary(&mut self) -> Result<EVal, String> {
        match self.next() {
            ETok::Int(v) => Ok(EVal::Int(v)),
            ETok::Str(s) => Ok(EVal::Str(s)),
            ETok::LParen => {
                let v = self.or()?;
                if !matches!(self.next(), ETok::RParen) { return Err("缺少 ')'".into()); }
                Ok(v)
            }
            ETok::Ident(name) if name == "defined" => {
                let paren = matches!(self.peek(), ETok::LParen);
                if paren { self.pos += 1; }
                let id = match self.next() {
                    ETok::Ident(n) => n,
                    _ => return Err("defined 后缺少宏名".into()),
                };
                if paren && !matches!(self.next(), ETok::RParen) {
                    return Err("defined(...) 缺少 ')'".into());
                }
                Ok(EVal::Int(self.macros.contains_key(&id) as i64))
            }
            ETok::Ident(name) => {
                // 未定义的标识符按 C 惯例求值为 0；已定义的对象式宏递归求值其体。
                match self.macros.get(&name) {
                    None => Ok(EVal::Int(0)),
                    Some(def) => {
                        if self.depth >= EXPR_MAX_DEPTH {
                            return Err(format!("宏值求值超过最大深度 {}（可能存在递归宏）", EXPR_MAX_DEPTH));
                        }
                        let inner = lex_expr(&def.body_text)?;
                        let mut p = ExprParser { toks: inner, pos: 0, macros: self.macros, depth: self.depth + 1 };
                        p.parse()
                    }
                }
            }
            other => Err(format!("表达式中出现意外的 token：{:?}", other)),
        }
    }
}

fn vals_eq(a: &EVal, b: &EVal) -> bool {
    match (a, b) {
        (EVal::Int(x), EVal::Int(y)) => x == y,
        (EVal::Str(x), EVal::Str(y)) => x == y,
        (EVal::Int(x), EVal::Str(y)) | (EVal::Str(y), EVal::Int(x)) => {
            y.parse::<i64>().map(|v| v == *x).unwrap_or(false)
        }
    }
}

fn eval_condition(s: &str, macros: &MacroTable, depth: usize) -> Result<bool, String> {
    let toks = lex_expr(s)?;
    let mut p = ExprParser { toks, pos: 0, macros, depth };
    Ok(p.parse()?.truthy())
}

// ---------------------------------------------------------------------------
//  阶段 2：token 级宏展开（对象式 + 函数式）
// ---------------------------------------------------------------------------

const MACRO_EXPAND_MAX_DEPTH: usize = 64;

/// 对 token 流做宏展开。`expand_into` 遇到宏时把展开结果（改写行号为调用点）递归
/// 展开，`expanding` 集合防止直接/间接递归宏无限展开。
pub fn expand_tokens(tokens: Vec<LToken>, macros: &MacroTable) -> Result<Vec<LToken>, String> {
    let mut expanding = HashSet::new();
    expand_into(&tokens, macros, &mut expanding, 0)
}

fn expand_into(
    tokens: &[LToken],
    macros: &MacroTable,
    expanding: &mut HashSet<String>,
    depth: usize,
) -> Result<Vec<LToken>, String> {
    if depth > MACRO_EXPAND_MAX_DEPTH {
        return Err(format!("宏展开超过最大深度 {}（可能存在递归宏）", MACRO_EXPAND_MAX_DEPTH));
    }
    let mut out: Vec<LToken> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        let name = match &tok.kind {
            LKind::Ident(n) => n.clone(),
            _ => { out.push(tok.clone()); i += 1; continue; }
        };
        let def = match macros.get(&name) {
            Some(d) if !expanding.contains(&name) => d.clone(),
            _ => { out.push(tok.clone()); i += 1; continue; }
        };

        match &def.params {
            None => {
                // 对象式：直接内联宏体。
                expanding.insert(name.clone());
                let expanded = expand_into(&def.body_tokens, macros, expanding, depth + 1)?;
                expanding.remove(&name);
                for mut t in expanded {
                    t.line = tok.line; t.col = tok.col;
                    out.push(t);
                }
                i += 1;
            }
            Some(params) => {
                // 函数式：必须紧跟 '('，否则该标识符原样保留（C 惯例）。
                if !matches!(tokens.get(i + 1).map(|t| &t.kind), Some(LKind::LParen)) {
                    out.push(tok.clone()); i += 1; continue;
                }
                let mut j = i + 2;
                let mut nest = 1i32;
                let mut args: Vec<Vec<LToken>> = Vec::new();
                let mut cur: Vec<LToken> = Vec::new();
                let mut closed = false;
                while j < tokens.len() {
                    let t = &tokens[j];
                    match t.kind {
                        LKind::LParen => { nest += 1; cur.push(t.clone()); }
                        LKind::RParen => {
                            nest -= 1;
                            if nest == 0 { closed = true; break; }
                            cur.push(t.clone());
                        }
                        LKind::Comma if nest == 1 => { args.push(std::mem::take(&mut cur)); }
                        _ => cur.push(t.clone()),
                    }
                    j += 1;
                }
                if !closed {
                    return Err(format!(
                        "第 {} 行 第 {} 列：宏 '{}' 的调用缺少 ')'",
                        tok.line, tok.col, name
                    ));
                }
                if !(cur.is_empty() && args.is_empty()) { args.push(cur); }
                let got = args.len();
                if got != params.len() {
                    return Err(format!(
                        "第 {} 行 第 {} 列：宏 '{}' 需要 {} 个参数，实际 {} 个",
                        tok.line, tok.col, name, params.len(), got
                    ));
                }
                // 形参替换：宏体里的 Ident(param) → 对应实参 token 序列。
                let mut substituted: Vec<LToken> = Vec::new();
                for bt in &def.body_tokens {
                    if let LKind::Ident(ref id) = bt.kind {
                        if let Some(k) = params.iter().position(|p| p == id) {
                            for mut at in args[k].clone() {
                                at.line = tok.line; at.col = tok.col;
                                substituted.push(at);
                            }
                            continue;
                        }
                    }
                    let mut nt = bt.clone();
                    nt.line = tok.line; nt.col = tok.col;
                    substituted.push(nt);
                }
                expanding.insert(name.clone());
                let expanded = expand_into(&substituted, macros, expanding, depth + 1)?;
                expanding.remove(&name);
                out.extend(expanded);
                i = j + 1; // 跳过整个 NAME(...) 调用
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
//  单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn defs_from_src(src: &str) -> MacroTable {
        process_source(src, "<test>", &[], &mut |_| {}).unwrap().macros
    }
    fn idents(tokens: &[LToken]) -> Vec<String> {
        tokens.iter().filter_map(|t| if let LKind::Ident(s) = &t.kind { Some(s.clone()) } else { None }).collect()
    }
    fn kinds(tokens: &[LToken]) -> Vec<String> {
        tokens.iter().map(|t| match &t.kind {
            LKind::Ident(s) => format!("I({})", s),
            LKind::IntLit(v) => format!("N({})", v),
            LKind::StringLit(s) => format!("S({})", s),
            LKind::Plus => "+".into(), LKind::Semicolon => ";".into(),
            LKind::LParen => "(".into(), LKind::RParen => ")".into(),
            LKind::Comma => ",".into(),
            other => format!("{:?}", other),
        }).collect()
    }

    #[test]
    fn object_macro_token_substitution() {
        let macros = defs_from_src("#define N 42\nvar x: int = N;\n");
        let toks = lexer::lex("var x: int = N;").unwrap();
        let out = expand_tokens(toks, &macros).unwrap();
        assert!(kinds(&out).contains(&"N(42)".to_string()), "对象式宏应替换为 token");
        assert!(!idents(&out).iter().any(|s| s == "N"));
    }

    #[test]
    fn object_macro_does_not_touch_string_literal() {
        let macros = defs_from_src("#define N 42\n");
        let toks = lexer::lex("print(\"N\");").unwrap();
        let out = expand_tokens(toks, &macros).unwrap();
        assert!(kinds(&out).contains(&"S(N)".to_string()), "字符串字面量内部不应被替换");
    }

    #[test]
    fn function_macro_substitution() {
        let macros = defs_from_src("#define ADD(a, b) a + b\nvar y = ADD(1, 2);\n");
        let toks = lexer::lex("var y = ADD(1, 2);").unwrap();
        let out = expand_tokens(toks, &macros).unwrap();
        let k = kinds(&out);
        assert!(k.contains(&"N(1)".to_string()) && k.contains(&"N(2)".to_string()), "实参应代入");
        assert!(!idents(&out).iter().any(|s| s == "a" || s == "b" || s == "ADD"));
    }

    #[test]
    fn function_macro_unrelated_identifier_kept() {
        let macros = defs_from_src("#define F(x) x\n");
        assert!(macros.contains_key("F"));
        let toks = lexer::lex("F").unwrap(); // 函数式宏名单独出现，后无 '('
        let out = expand_tokens(toks, &macros).unwrap();
        assert_eq!(idents(&out), vec!["F".to_string()], "函数式宏后无括号应原样保留");
    }

    #[test]
    fn recursive_macro_guard() {
        // C 语义：宏体里再次出现自身时**不再展开**（不是报错），从而天然终止。
        // 这里验证展开不会挂死，且自引用标识符被原样保留交给后续语义分析。
        let mut macros = MacroTable::new();
        macros.insert("LOOP".into(), MacroDef {
            name: "LOOP".into(), params: None,
            body_text: "LOOP".into(),
            body_tokens: lexer::lex("LOOP").unwrap()[..1].to_vec(),
            line: 0,
        });
        let toks = lexer::lex("LOOP;").unwrap();
        let out = expand_tokens(toks, &macros).unwrap();
        assert_eq!(idents(&out), vec!["LOOP".to_string()], "自引用宏应原样保留，不无限展开");
    }

    #[test]
    fn ifdef_else_keeps_line_count() {
        let src = "#define FEATURE\n#ifdef FEATURE\nprint(\"on\");\n#else\nprint(\"off\");\n#endif\n";
        let out = process_source(src, "<test>", &[], &mut |_| {}).unwrap();
        assert_eq!(out.source.lines().count(), src.lines().count(), "行号必须保持");
        let kept: Vec<&str> = out.source.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!(kept.iter().any(|l| l.contains("on")));
        assert!(!kept.iter().any(|l| l.contains("off")));
    }

    #[test]
    fn ifndef_and_arithmetic_condition() {
        let src = "#ifndef X\n#if (2 + 3) * 4 == 20 && !0\nKEEP\n#endif\n#endif\n";
        let out = process_source(src, "<test>", &[], &mut |_| {}).unwrap();
        assert!(out.source.contains("KEEP"));
    }

    #[test]
    fn if_defined_and_string_compare() {
        let src = "#define OS \"linux\"\n#if defined(OS) && OS == \"linux\"\nYES\n#endif\n";
        let out = process_source(src, "<test>", &[], &mut |_| {}).unwrap();
        assert!(out.source.contains("YES"));
    }

    #[test]
    fn cli_define_feeds_ifdef() {
        let src = "#ifdef VER\nHAS_VER\n#endif\n";
        let out = process_source(src, "<test>", &[("VER".into(), None)], &mut |_| {}).unwrap();
        assert!(out.source.contains("HAS_VER"));
    }

    #[test]
    fn nested_conditions() {
        let src = "#if 1\nA\n#if 0\nB\n#else\nC\n#endif\nD\n#endif\n";
        let out = process_source(src, "<test>", &[], &mut |_| {}).unwrap();
        let kept: Vec<&str> = out.source.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(kept, vec!["A", "C", "D"]);
    }

    #[test]
    fn undef_disables_branch() {
        let src = "#define X\n#undef X\n#ifdef X\nBAD\n#else\nGOOD\n#endif\n";
        let out = process_source(src, "<test>", &[], &mut |_| {}).unwrap();
        assert!(out.source.contains("GOOD") && !out.source.contains("BAD"));
    }

    #[test]
    fn error_in_active_branch_fails_fast() {
        let src = "#if 1\n#error boom\n#endif\n";
        assert!(process_source(src, "<test>", &[], &mut |_| {}).is_err());
    }

    #[test]
    fn error_in_inactive_branch_is_ignored() {
        let src = "#if 0\n#error boom\n#endif\nOK\n";
        let out = process_source(src, "<test>", &[], &mut |_| {}).unwrap();
        assert!(out.source.contains("OK"));
    }

    #[test]
    fn unknown_directive_in_inactive_branch_is_ignored() {
        // 假分支内的未知指令随整块裁剪一并忽略；活跃分支内仍须 Fail-Fast。
        let src = "#if 0\n#also_unknown x\n#endif\nOK\n";
        let out = process_source(src, "<test>", &[], &mut |_| {}).unwrap();
        assert!(out.source.contains("OK"));
        assert!(process_source("#also_unknown x\n", "<test>", &[], &mut |_| {}).is_err());
    }

    #[test]
    fn unmatched_endif_fails_fast() {
        assert!(process_source("#endif\n", "<test>", &[], &mut |_| {}).is_err());
    }

    #[test]
    fn unclosed_if_fails_fast() {
        assert!(process_source("#if 1\nBODY\n", "<test>", &[], &mut |_| {}).is_err());
    }

    #[test]
    fn unknown_directive_fails_fast() {
        assert!(process_source("#frobnicate x\n", "<test>", &[], &mut |_| {}).is_err());
    }
}
