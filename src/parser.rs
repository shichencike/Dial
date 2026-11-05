//! 递归下降 + 运算符优先级（Pratt 风格）的语法分析器。
//!
//! 优先级从低到高：
//!   or < and < not < 相等 < 比较 < 加减 < 乘除 < 一元(-) < 后缀(. [] ) < 原子
//!
//! 新增语法：
//!   struct Name { f: T, ... }            结构体定义（顶层）
//!   Type 可写作 T[]                         动态数组
//!   [e1, e2, ...]                         数组字面量
//!   Name { f: e, ... }                    结构体字面量
//!   e.field / e.method(args) / e[i]        字段 / 方法 / 索引

use crate::ast::*;
use crate::error::{DialcError, Result};
use crate::lexer::{InterpPiece, Token, TokenKind};

/// 解析入口。`opaque_types` 是**已导入包**用 `type X;` 声明的不透明句柄名——
/// 它们在源码里直接当类型名用（如 `var h: DemoHandle = demo_new(1);`），
/// 但既不是内建类型、也不是本程序定义的结构体，因此必须由解析阶段就知道。
///
/// 之所以要外部传入：包要先 `import` 才能知道它有哪些句柄，而 `import` 本身
/// 又在源码里——所以调用方先解析一遍拿到 import 列表、解析好包，再带着这批名字重新解析。
/// 解析一遍的成本可以忽略，换来的是「类型名在语法阶段就正确」。
pub fn parse_with(tokens: Vec<Token>, opaque_types: &[String]) -> Result<Program> {
    let mut p = Parser {
        tokens,
        pos: 0,
        opaque: opaque_types.to_vec(),
        consts: std::collections::HashMap::new(),
        enum_names: std::collections::HashSet::new(),
    };
    let mut imports = Vec::new();
    let mut structs = Vec::new();
    let mut functions = Vec::new();
    while !p.at_end() {
        if p.check(&TokenKind::Import) {
            imports.push(p.parse_import()?);
        } else if p.check(&TokenKind::Struct) {
            structs.push(p.parse_struct_def()?);
        } else if p.check(&TokenKind::Const) {
            // const 不进入 AST：在 parser 内联为字面量，语义/代码生成零改动。
            p.parse_const()?;
        } else if p.check(&TokenKind::Enum) {
            // enum 同样内联：变体是 int-backed 的编译期常量。
            p.parse_enum()?;
        } else {
            functions.push(p.parse_function()?);
        }
    }
    Ok(Program {
        imports,
        structs,
        functions,
    })
}

/// 无包上下文的解析（供测试或不需要 import 的场景使用）。
pub fn parse(tokens: Vec<Token>) -> Result<Program> {
    parse_with(tokens, &[])
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// 已导入包声明的不透明句柄名（见 `parse_with`）。
    opaque: Vec<String>,
    /// 文件级常量（v1.00）：名字 -> (标注类型, 字面量值)。在使用点内联替换。
    consts: std::collections::HashMap<String, (Type, Expr)>,
    /// 已声明的枚举名（v1.00）：用于把 `Name.Variant` 整段识别为枚举变体。
    enum_names: std::collections::HashSet<String>,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }
    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }
    fn at_end(&self) -> bool {
        matches!(self.peek_kind(), TokenKind::EOF)
    }
    fn check(&self, k: &TokenKind) -> bool {
        self.peek_kind() == k
    }
    /// 向前看第 n 个 token（n=0 即当前 token），越界返回 EOF。
    /// 用于 `with 名 = 资源` 与 `with 资源 as 名` 两种形式的二选一判断。
    fn peek_at(&self, n: usize) -> &Token {
        let i = (self.pos + n).min(self.tokens.len().saturating_sub(1));
        &self.tokens[i]
    }
    /// 若当前 token 是复合赋值运算符（+= -= *= /= %=），返回对应的二元运算符（v1.00）。
    fn compound_op(&self) -> Option<BinOp> {
        match self.peek_kind() {
            TokenKind::PlusEq => Some(BinOp::Add),
            TokenKind::MinusEq => Some(BinOp::Sub),
            TokenKind::MulEq => Some(BinOp::Mul),
            TokenKind::DivEq => Some(BinOp::Div),
            TokenKind::ModEq => Some(BinOp::Mod),
            _ => None,
        }
    }
    fn expect(&mut self, k: TokenKind) -> Result<()> {
        if self.check(&k) {
            self.advance();
            Ok(())
        } else {
            let t = self.peek();
            Err(DialcError::new(
                format!("语法错误：期望 {:?}，但遇到 {:?}", k, t.kind),
                t.line,
                t.col,
            ))
        }
    }
    /// 期望一个泛型闭合符 `>`。
    ///
    /// 词法层把 `>>` 合并成了一个 `Shr`（右移运算符）——这在 `map<map<int,int>,int>`
    /// 与 `ptr_cast<rawptr<byte>>(x)` 里都会撞上。做法与 C++ 模板解析一致：
    /// **在解析层把 `Shr` 拆成两个 `>`**（就地改写 token 流）。
    fn expect_gt(&mut self) -> Result<()> {
        if self.check(&TokenKind::Gt) {
            self.advance();
            return Ok(());
        }
        if self.check(&TokenKind::Shr) {
            let t = self.peek().clone();
            // 把 Shr 改成 Gt，并在其后插入第二个 Gt（两个 `>` 各自闭合一层泛型）。
            self.tokens[self.pos] = Token {
                kind: TokenKind::Gt,
                line: t.line,
                col: t.col,
            };
            self.tokens.insert(
                self.pos + 1,
                Token {
                    kind: TokenKind::Gt,
                    line: t.line,
                    col: t.col + 1,
                },
            );
            self.advance();
            return Ok(());
        }
        let t = self.peek();
        Err(DialcError::new(
            format!("语法错误：期望 Gt，但遇到 {:?}", t.kind),
            t.line,
            t.col,
        ))
    }
    fn expect_ident(&mut self) -> Result<String> {
        Ok(self.expect_ident_pos()?.0)
    }

    /// 与 `expect_ident` 相同，但额外回传标识符的 1 起行列（v0.95：位置信息）。
    fn expect_ident_pos(&mut self) -> Result<(String, usize, usize)> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Ident(name) => {
                self.advance();
                Ok((name, t.line, t.col))
            }
            _ => Err(DialcError::new(
                format!("语法错误：期望标识符，但遇到 {:?}", t.kind),
                t.line,
                t.col,
            )),
        }
    }

    // ---- 顶层 ----

    /// `import "<包名>";`
    ///
    /// 包名是**字符串字面量**而非标识符，这是刻意的：
    /// 包可能带命名空间（`"std/io"`），用标识符会逼出一套 `.` 或 `::` 的解析规则——
    /// 那是毫无收益的额外语法。字符串一眼就能看出「这是外部世界的一个名字」。
    fn parse_import(&mut self) -> Result<String> {
        let t = self.advance(); // 消费 import
        let name_tok = self.peek().clone();
        let name = match &name_tok.kind {
            TokenKind::StringLit(s) => {
                self.advance();
                s.clone()
            }
            other => {
                return Err(DialcError::new(
                    format!(
                        "语法错误：import 后必须是包名的字符串字面量，例如 import \"sqlite\"; 但遇到 {:?}",
                        other
                    ),
                    name_tok.line,
                    name_tok.col,
                ))
            }
        };
        if name.trim().is_empty() {
            return Err(DialcError::new("语法错误：import 的包名不能为空", t.line, t.col));
        }
        self.expect(TokenKind::Semicolon)?;
        Ok(name)
    }

    fn parse_struct_def(&mut self) -> Result<StructDef> {
        let start = self.advance(); // struct
        let (name, nline, ncol) = self.expect_ident_pos()?;
        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.at_end() {
            let fname = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let fty = self.parse_type()?;
            fields.push((fname, fty));
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(StructDef {
            name,
            fields,
            line: start.line,
            col: start.col,
            name_line: nline,
            name_col: ncol,
        })
    }

    /// 禁止局部变量 / 形参 / 循环变量与文件级 const 同名。
    /// const 在 `parse_primary` 里是**按名字内联**的，若允许同名，变量会被静默替换成常量；
    /// 与其留下这种无声的错误，不如在绑定点当场拒绝。
    fn reject_const_shadow(&self, name: &str, line: usize, col: usize) -> Result<()> {
        if self.consts.contains_key(name) {
            return Err(DialcError::new(
                format!(
                    "语法错误：'{}' 是文件级常量名，不能用作变量 / 形参 / 循环变量名",
                    name
                ),
                line, col,
            ));
        }
        Ok(())
    }

    /// `const NAME: T = 字面量;`（v1.00）：文件级常量，编译期内联替换，不可重新赋值。
    /// 只允许字面量值（可引用此前已定义的 const —— 那会在解析时先被内联成字面量），
    /// 且标注类型必须与值类型完全一致（零隐式转换铁律）。
    fn parse_const(&mut self) -> Result<()> {
        self.expect(TokenKind::Const)?;
        let (name, nline, ncol) = self.expect_ident_pos()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(TokenKind::Assign)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        let vt = match &value {
            Expr::IntLit(_) => Type::Int,
            Expr::FloatLit(_) => Type::Float,
            Expr::BoolLit(_) => Type::Bool,
            Expr::CharLit(_) => Type::Char,
            Expr::StringLit(_) => Type::String,
            _ => {
                return Err(DialcError::new(
                    "语法错误：const 的值必须是字面量常量（先定义后使用，可引用已定义的 const）",
                    nline, ncol,
                ))
            }
        };
        if vt != ty {
            return Err(DialcError::new(
                format!(
                    "语义错误：const '{}' 的标注类型 {} 与值类型 {} 不符（零隐式转换）",
                    name, ty.name(), vt.name()
                ),
                nline, ncol,
            ));
        }
        if self.consts.contains_key(&name) {
            return Err(DialcError::new(
                format!("语法错误：常量 '{}' 重复定义", name),
                nline, ncol,
            ));
        }
        self.consts.insert(name, (ty, value));
        Ok(())
    }

    /// `enum Name { A, B = 10, C }`（v1.00）：int-backed 枚举。
    /// 变体在 parser 内联为整数字面量（默认值从 0 递增，可显式指定），
    /// 因此语义/代码生成零改动；`Name.Variant` 整段当作一个常量引用。
    fn parse_enum(&mut self) -> Result<()> {
        self.expect(TokenKind::Enum)?;
        let (name, nline, ncol) = self.expect_ident_pos()?;
        if self.enum_names.contains(&name) {
            return Err(DialcError::new(
                format!("语法错误：枚举 '{}' 重复定义", name),
                nline, ncol,
            ));
        }
        self.expect(TokenKind::LBrace)?;
        let mut next: i64 = 0;
        while !self.check(&TokenKind::RBrace) && !self.at_end() {
            let (v, vl, vc) = self.expect_ident_pos()?;
            let val = if self.check(&TokenKind::Assign) {
                self.advance();
                match self.parse_expr()? {
                    Expr::IntLit(n) => n,
                    _ => {
                        return Err(DialcError::new(
                            "语法错误：枚举值必须是整数字面量", vl, vc,
                        ))
                    }
                }
            } else {
                next
            };
            next = val + 1;
            let key = format!("{}.{}", name, v);
            if self.consts.contains_key(&key) {
                return Err(DialcError::new(
                    format!("语法错误：枚举变体 '{}' 重复定义", key), vl, vc,
                ));
            }
            self.consts.insert(key, (Type::Int, Expr::IntLit(val)));
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }
        self.expect(TokenKind::RBrace)?;
        self.enum_names.insert(name);
        Ok(())
    }

    /// 把插值 `${...}` 内的源码文本二次 lex + parse 成表达式。
    fn parse_interp_expr(&self, src: &str, line: usize, col: usize) -> Result<Expr> {
        let toks = crate::lexer::lex(src).map_err(|_| {
            DialcError::new("语法错误：字符串插值 ${...} 内的表达式词法错误", line, col)
        })?;
        let mut sub = Parser {
            tokens: toks,
            pos: 0,
            opaque: self.opaque.clone(),
            consts: self.consts.clone(),
            enum_names: self.enum_names.clone(),
        };
        sub.parse_expr().map_err(|_| {
            DialcError::new("语法错误：字符串插值 ${...} 内的表达式语法错误", line, col)
        })
    }

    fn parse_function(&mut self) -> Result<Func> {
        self.expect(TokenKind::Fn)?;
        let (name, nline, ncol) = self.expect_ident_pos()?;
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        let mut param_pos = Vec::new();
        let mut defaults: Vec<Option<Expr>> = Vec::new();
        let mut seen_default = false;
        // 同类型变参 `...xs: T`（v1.03）：只记录标记，脱糖由 semantic 负责。
        let mut variadic: Option<(String, Type, usize, usize)> = None;
        if !self.check(&TokenKind::RParen) {
            loop {
                if self.check(&TokenKind::Ellipsis) {
                    let vt = self.peek().clone();
                    self.advance();
                    let (pname, pline, pcol) = self.expect_ident_pos()?;
                    self.reject_const_shadow(&pname, pline, pcol)?;
                    self.expect(TokenKind::Colon)?;
                    let elem = self.parse_type()?;
                    if seen_default {
                        return Err(DialcError::new(
                            "语法错误：变参函数不允许同时使用默认实参（两者都要求占据参数列表末尾）",
                            vt.line, vt.col,
                        ));
                    }
                    if self.check(&TokenKind::Assign) {
                        return Err(DialcError::new(
                            "语法错误：变参形参不能有默认值（缺省时它就是空数组）",
                            vt.line, vt.col,
                        ));
                    }
                    if self.check(&TokenKind::Comma) {
                        return Err(DialcError::new(
                            "语法错误：变参形参 ...xs 必须是参数列表的最后一项",
                            vt.line, vt.col,
                        ));
                    }
                    variadic = Some((pname, elem, pline, pcol));
                    break;
                }
                let (pname, pline, pcol) = self.expect_ident_pos()?;
                self.reject_const_shadow(&pname, pline, pcol)?;
                self.expect(TokenKind::Colon)?;
                let pty = self.parse_type()?;
                // 默认实参 `= expr`（v1.00）。
                let default = if self.check(&TokenKind::Assign) {
                    self.advance();
                    seen_default = true;
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                // 带默认值的形参必须连续位于末尾，其后不得再出现必填形参。
                if seen_default && default.is_none() {
                    return Err(DialcError::new(
                        "语法错误：带默认值的形参必须在参数列表末尾（其后不得再出现必填形参）",
                        pline, pcol,
                    ));
                }
                params.push((pname, pty));
                param_pos.push((pline, pcol));
                defaults.push(default);
                if self.check(&TokenKind::Comma) {
                    self.advance();
                    continue;
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        self.expect(TokenKind::Colon)?;
        let ret = self.parse_type()?;
        let body = self.parse_block()?;
        Ok(Func {
            name,
            params,
            defaults,
            param_pos,
            variadic,
            ret,
            body,
            line: nline,
            col: ncol,
        })
    }

    /// 泛型 map 的键/值 kind 编码（与 stdlib/dial_kvmap.c 的 KV_* 常量严格一致）。
    /// `new_kv_map()` 的实参由声明标注在 parse_var_decl 里补齐，源码不暴露这些数字。
    fn kv_kind_of(t: &Type) -> i64 {
        match t {
            Type::Int => 0,
            Type::Float => 1,
            Type::String => 2,
            Type::Bool => 3,
            Type::Char => 4,
            Type::Byte => 5,
            _ => -1, // 非法类型交由 check_type_wellformed 给出明确诊断
        }
    }

    fn parse_type(&mut self) -> Result<Type> {
        let t = self.peek().clone();
        let mut ty = match t.kind {
            TokenKind::IntT => {
                self.advance();
                Type::Int
            }
            TokenKind::FloatT => {
                self.advance();
                Type::Float
            }
            TokenKind::BoolT => {
                self.advance();
                Type::Bool
            }
            TokenKind::CharT => {
                self.advance();
                Type::Char
            }
            TokenKind::ByteT => {
                self.advance();
                Type::Byte
            }
            TokenKind::BytesT => {
                self.advance();
                Type::Bytes
            }
            TokenKind::StringT => {
                self.advance();
                Type::String
            }
            TokenKind::VoidT => {
                self.advance();
                Type::Void
            }
            // 函数指针类型 `fn(A, B): R`（v1.03）。
            // 与函数声明 `fn name(a: A): R` 同形，只是省掉名字；类型位置遇上
            // `fn` 关键字时不会与任何语句开头混淆（parse_type 只在类型上下文被调用）。
            TokenKind::Fn => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let mut ps = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    loop {
                        ps.push(self.parse_type()?);
                        if self.check(&TokenKind::Comma) {
                            self.advance();
                            continue;
                        }
                        break;
                    }
                }
                self.expect(TokenKind::RParen)?;
                self.expect(TokenKind::Colon)?;
                let ret = self.parse_type()?;
                Type::FnPtr(ps, Box::new(ret))
            }
            TokenKind::MapT => {
                self.advance();
                // map<K, V>（v1.00）：泛型哈希表。省略 <K,V> 时仍是老 int→int map。
                if self.check(&TokenKind::Lt) {
                    self.advance();
                    let k = self.parse_type()?;
                    self.expect(TokenKind::Comma)?;
                    let v = self.parse_type()?;
                    self.expect_gt()?;
                    Type::KvMap(Box::new(k), Box::new(v))
                } else {
                    Type::Map
                }
            }
            TokenKind::FileT => {
                self.advance();
                Type::File
            }
            TokenKind::Ident(n) => {
                self.advance();
                match n.as_str() {
                    // channel<T>：泛型通道。`<` / `>` 词法已存在（Lt / Gt），无需新增 token。
                    // 省略 <T> 时得到 Channel(Void) —— 「元素类型未定」占位，
                    // 由变量声明处的类型标注统一（见 semantic 的 VarDecl 分支）。
                    "channel" => {
                        if self.check(&TokenKind::Lt) {
                            self.advance();
                            let elem = self.parse_type()?;
                            self.expect_gt()?;
                            Type::Channel(Box::new(elem))
                        } else {
                            Type::Channel(Box::new(Type::Void))
                        }
                    }
                    // set<T>（v1.00）：集合，底层复用 int→int map 的键集。
                    // 当前版本元素类型仅支持 int —— 写别的元素类型在语法层就明确拒绝，
                    // 避免用户以为拿到了泛型集合、实际却只存得下 int。
                    "set" => {
                        self.expect(TokenKind::Lt)?;
                        let elem = self.parse_type()?;
                        self.expect(TokenKind::Gt)?;
                        if elem != Type::Int {
                            return Err(DialcError::new(
                                format!(
                                    "语法错误：当前版本 set 的元素类型只支持 int，遇到 {}",
                                    elem.name()
                                ),
                                t.line,
                                t.col,
                            ));
                        }
                        Type::Set(Box::new(elem))
                    }
                    // rawptr<T>（v1.02）：裸指针，**不受 ARC 管理**，需手动释放。
                    // 元素类型为 void 时是无类型裸指针（只能传递/释放，不能做指针算术）。
                    "rawptr" => {
                        self.expect(TokenKind::Lt)?;
                        let elem = self.parse_type()?;
                        self.expect_gt()?;
                        Type::RawPtr(Box::new(elem))
                    }
                    // thread：线程句柄（不透明指针，不是泛型）
                    "thread" => Type::Thread,
                    _ => {
                        // 已导入包声明的不透明句柄优先于「当成结构体」：
                        // 它是包私有的不透明指针，用户拿不到布局（见 ast::Type::Opaque）。
                        if self.opaque.iter().any(|o| o == &n) {
                            Type::Opaque(n)
                        } else {
                            Type::Struct(n)
                        }
                    }
                }
            }
            _ => {
                return Err(DialcError::new(
                    format!("语法错误：期望类型关键字或结构体名，但遇到 {:?}", t.kind),
                    t.line,
                    t.col,
                ))
            }
        };
        // 后缀 `[]` 表示动态数组（可嵌套：T[][]）
        while self.check(&TokenKind::LBracket) {
            self.advance();
            self.expect(TokenKind::RBracket)?;
            ty = Type::Array(Box::new(ty));
        }
        Ok(ty)
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>> {
        self.expect(TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.at_end() {
            stmts.push(self.parse_stmt()?);
        }
        self.expect(TokenKind::RBrace)?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt> {
        let tok = self.peek().clone();
        // const 是编译期内联的，不能被重新赋值。这里给出明确诊断，避免落到
        // 「期望 Semicolon，但遇到 Assign」这类费解的语法错误上。
        if let TokenKind::Ident(ref name) = tok.kind {
            if self.consts.contains_key(name) {
                return Err(DialcError::new(
                    format!("语法错误：'{}' 是文件级常量，不可重新赋值", name),
                    tok.line, tok.col,
                ));
            }
        }
        match tok.kind {
            TokenKind::Var | TokenKind::Let => self.parse_var_decl(),
            TokenKind::If => self.parse_if(),
            TokenKind::While => self.parse_while(),
            TokenKind::For => self.parse_for(),
            TokenKind::Do => self.parse_do_while(),
            TokenKind::Match => self.parse_match(),
            TokenKind::Defer => self.parse_defer(),
            TokenKind::With => self.parse_with(),
            TokenKind::Goto => self.parse_goto(),
            TokenKind::Label => self.parse_label(),
            TokenKind::Return => self.parse_return(),
            TokenKind::Try => self.parse_try(),
            TokenKind::Throw => self.parse_throw(),
            TokenKind::Select => self.parse_select(),
            // 裸块 `{ ... }`：引入一个新的词法作用域（v0.98.next）。
            // 直接复用 parse_block（它负责消费配对的 { 与 }，并递归 parse_stmt 内部语句，
            // 因此裸块可任意嵌套）。
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                Ok(Stmt::Block { body: block, line: tok.line, col: tok.col })
            }
            TokenKind::Break => {
                self.advance();
                self.expect(TokenKind::Semicolon)?;
                Ok(Stmt::Break(tok.line, tok.col))
            }
            TokenKind::Continue => {
                self.advance();
                self.expect(TokenKind::Semicolon)?;
                Ok(Stmt::Continue(tok.line, tok.col))
            }
            // 结构体定义只允许出现在顶层（收集进 Program.structs），函数体内一律拒绝。
            TokenKind::Struct => Err(DialcError::new(
                "语法错误：struct 定义只能出现在顶层，不能写在函数体内",
                tok.line,
                tok.col,
            )),
            // 前缀自增/自减（v1.00）：++i; / --a[i]; 脱糖为 i = i + 1 / a[i] = a[i] - 1
            TokenKind::PlusPlus | TokenKind::MinusMinus => {
                let dec = matches!(tok.kind, TokenKind::MinusMinus);
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(TokenKind::Semicolon)?;
                let op = if dec { BinOp::Sub } else { BinOp::Add };
                return match expr {
                    Expr::Var(name) => Ok(Stmt::Assign {
                        name: name.clone(),
                        value: Expr::Binary(Box::new(Expr::Var(name)), op, Box::new(Expr::IntLit(1))),
                        move_hint: false,
                        line: tok.line,
                        col: tok.col,
                    }),
                    Expr::Field(obj, field) => Ok(Stmt::AssignField {
                        obj: obj.clone(),
                        field: field.clone(),
                        value: Expr::Binary(Box::new(Expr::Field(obj, field)), op, Box::new(Expr::IntLit(1))),
                        line: tok.line,
                        col: tok.col,
                    }),
                    Expr::Index(arr, index) => Ok(Stmt::AssignIndex {
                        arr: arr.clone(),
                        index: index.clone(),
                        value: Expr::Binary(Box::new(Expr::Index(arr, index)), op, Box::new(Expr::IntLit(1))),
                        line: tok.line,
                        col: tok.col,
                    }),
                    _ => Err(DialcError::new(
                        "语法错误：自增/自减左侧必须是变量、字段或下标（如 ++i; / --a[i];）",
                        tok.line,
                        tok.col,
                    )),
                };
            }
            _ => {
                // 可能是赋值（含字段/索引 lvalue）或表达式语句
                let expr = self.parse_expr()?;
                // 复合赋值（v1.00）：x += e 脱糖为 x = x + e（复用已有赋值语义与类型检查）。
                // 对 string 而言 `x += y` 即 `x = x + y`，走字符串拼接 `concat`，无需特判。
                if let Some(op) = self.compound_op() {
                    self.advance();
                    let rhs = self.parse_expr()?;
                    self.expect(TokenKind::Semicolon)?;
                    return match &expr {
                        Expr::Var(name) => Ok(Stmt::Assign {
                            name: name.clone(),
                            value: Expr::Binary(Box::new(Expr::Var(name.clone())), op, Box::new(rhs)),
                            move_hint: false,
                            line: tok.line,
                            col: tok.col,
                        }),
                        Expr::Field(obj, field) => Ok(Stmt::AssignField {
                            obj: obj.clone(),
                            field: field.clone(),
                            value: Expr::Binary(Box::new(Expr::Field(obj.clone(), field.clone())), op, Box::new(rhs)),
                            line: tok.line,
                            col: tok.col,
                        }),
                        Expr::Index(arr, index) => Ok(Stmt::AssignIndex {
                            arr: arr.clone(),
                            index: index.clone(),
                            value: Expr::Binary(Box::new(Expr::Index(arr.clone(), index.clone())), op, Box::new(rhs)),
                            line: tok.line,
                            col: tok.col,
                        }),
                        _ => Err(DialcError::new(
                            "语法错误：复合赋值左侧必须是变量、字段或下标",
                            tok.line,
                            tok.col,
                        )),
                    };
                }
                // 后缀自增/自减（v1.00）：仅作语句，脱糖为 x = x + 1 / x = x - 1。
                let is_incdec = matches!(
                    &expr,
                    Expr::Var(_) | Expr::Field(_, _) | Expr::Index(_, _)
                ) && matches!(self.peek_kind(), TokenKind::PlusPlus | TokenKind::MinusMinus);
                if is_incdec {
                    let dec = matches!(self.peek_kind(), TokenKind::MinusMinus);
                    self.advance();
                    self.expect(TokenKind::Semicolon)?;
                    let op = if dec { BinOp::Sub } else { BinOp::Add };
                    return match &expr {
                        Expr::Var(name) => Ok(Stmt::Assign {
                            name: name.clone(),
                            value: Expr::Binary(Box::new(Expr::Var(name.clone())), op, Box::new(Expr::IntLit(1))),
                            move_hint: false,
                            line: tok.line,
                            col: tok.col,
                        }),
                        Expr::Field(obj, field) => Ok(Stmt::AssignField {
                            obj: obj.clone(),
                            field: field.clone(),
                            value: Expr::Binary(Box::new(Expr::Field(obj.clone(), field.clone())), op, Box::new(Expr::IntLit(1))),
                            line: tok.line,
                            col: tok.col,
                        }),
                        Expr::Index(arr, index) => Ok(Stmt::AssignIndex {
                            arr: arr.clone(),
                            index: index.clone(),
                            value: Expr::Binary(Box::new(Expr::Index(arr.clone(), index.clone())), op, Box::new(Expr::IntLit(1))),
                            line: tok.line,
                            col: tok.col,
                        }),
                        _ => unreachable!(),
                    };
                }
                // 原有：普通赋值 / 表达式语句
                match &expr {
                    Expr::Var(name) if self.check(&TokenKind::Assign) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(TokenKind::Semicolon)?;
                        Ok(Stmt::Assign {
                            name: name.clone(),
                            value,
                            move_hint: false,
                            line: tok.line,
                            col: tok.col,
                        })
                    }
                    Expr::Field(obj, field) if self.check(&TokenKind::Assign) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(TokenKind::Semicolon)?;
                        Ok(Stmt::AssignField {
                            obj: obj.clone(),
                            field: field.clone(),
                            value,
                            line: tok.line,
                            col: tok.col,
                        })
                    }
                    Expr::Index(arr, index) if self.check(&TokenKind::Assign) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(TokenKind::Semicolon)?;
                        Ok(Stmt::AssignIndex {
                            arr: arr.clone(),
                            index: index.clone(),
                            value,
                            line: tok.line,
                            col: tok.col,
                        })
                    }
                    // 裸指针解引用赋值 `*p = v`（v1.02）
                    Expr::Unary(UnOp::Deref, ptr) if self.check(&TokenKind::Assign) => {
                        self.advance();
                        let value = self.parse_expr()?;
                        self.expect(TokenKind::Semicolon)?;
                        Ok(Stmt::AssignDeref {
                            ptr: ptr.clone(),
                            value: Box::new(value),
                            line: tok.line,
                            col: tok.col,
                        })
                    }
                    _ => {
                        self.expect(TokenKind::Semicolon)?;
                        Ok(Stmt::Expr(expr, tok.line, tok.col))
                    }
                }
            }
        }
    }

    fn parse_var_decl(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        let mutable = matches!(start.kind, TokenKind::Var);
        self.advance(); // 消费 var / let
        let (name, nline, ncol) = self.expect_ident_pos()?;
        self.reject_const_shadow(&name, nline, ncol)?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(TokenKind::Assign)?;
        let mut value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        // new_kv_map() 的 K/V 由声明标注决定：编译期把两个 kind 常量补进实参。
        // 语义层据此还原 Type::KvMap(K,V)，代码生成据此调用 dial_kvmap_new(kk, vk)。
        // 无需新增 AST 节点，也不把 kind 暴露给源码（源码永远只写 new_kv_map()）。
        if let Type::KvMap(ref k, ref v) = ty {
            if let Expr::Call(ref nm, ref a) = value {
                if nm == "new_kv_map" && a.is_empty() {
                    value = Expr::Call(
                        "new_kv_map".to_string(),
                        vec![Expr::IntLit(Self::kv_kind_of(k)), Expr::IntLit(Self::kv_kind_of(v))],
                    );
                }
            }
        }
        // 空数组 `[]` 无法推断元素类型：若声明类型是 Array(et)，在此补上期望元素类型，
        // 使 `var a: float[] = [];` 合法；若没有类型标注，语义层会给出明确诊断。
        if let Type::Array(ref et) = ty {
            if let Expr::ArrayLit(es, None) = &value {
                if es.is_empty() {
                    value = Expr::ArrayLit(vec![], Some(Box::new((**et).clone())));
                }
            }
        }
        Ok(Stmt::VarDecl {
            name,
            ty,
            mutable,
            value,
            move_hint: false,
            line: start.line,
            col: start.col,
            name_line: nline,
            name_col: ncol,
        })
    }

    fn parse_if(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::If)?;
        self.expect(TokenKind::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        let then_branch = self.parse_block()?;
        let mut else_branch = None;
        if self.check(&TokenKind::Else) {
            self.advance();
            // 语法糖 `else if (c) {..}`：递归解析为一个嵌套的 if，包成单元素 else 块。
            // 这样无需新增 AST 节点——语义/代码生成看到的依然是普通的嵌套 If。
            if self.check(&TokenKind::If) {
                let nested = self.parse_if()?;
                else_branch = Some(vec![nested]);
            } else {
                else_branch = Some(self.parse_block()?);
            }
        }
        Ok(Stmt::If {
            cond,
            then_branch,
            else_branch,
            line: start.line,
            col: start.col,
        })
    }

    fn parse_for(&mut self) -> Result<Stmt> {
        // `start` 已被循环起始表达式占用，关键字位置另取一个名字
        let kw = self.peek().clone();
        self.expect(TokenKind::For)?;
        self.expect(TokenKind::LParen)?;
        // 循环变量：单个 `v`，或 `(k, v)` 双变量（仅 map 用）
        let (a, a_line, a_col) = self.expect_ident_pos()?;
        self.reject_const_shadow(&a, a_line, a_col)?;
        let mut b: Option<(String, usize, usize)> = None;
        if self.check(&TokenKind::Comma) {
            self.advance();
            let (bn, bl, bc) = self.expect_ident_pos()?;
            self.reject_const_shadow(&bn, bl, bc)?;
            b = Some((bn, bl, bc));
        }
        self.expect(TokenKind::In)?;
        // 判断 range（后跟 `..`）还是集合（数组 / 映射）
        let first = self.parse_expr()?;
        if self.check(&TokenKind::DotDot) {
            // range：`for (i in start..end)` 或 `for (i in start..end step s)`
            self.advance();
            let end = self.parse_expr()?;
            let step = if self.check(&TokenKind::Step) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            if b.is_some() {
                return Err(DialcError::new(
                    "语法错误：range 循环只支持单个循环变量，不能用 (k, v) 形式",
                    a_line,
                    a_col,
                ));
            }
            self.expect(TokenKind::RParen)?;
            let body = self.parse_block()?;
            Ok(Stmt::For {
                var: a,
                start: first,
                end,
                step,
                body,
                line: kw.line,
                col: kw.col,
                var_line: a_line,
                var_col: a_col,
            })
        } else {
            // 集合：`for (v in arr)` 或 `for (k, v in m)`
            self.expect(TokenKind::RParen)?;
            let body = self.parse_block()?;
            match b {
                Some((bv, bl, bc)) => Ok(Stmt::ForEachMap {
                    key: a,
                    key_line: a_line,
                    key_col: a_col,
                    value: bv,
                    value_line: bl,
                    value_col: bc,
                    coll: first,
                    body,
                    line: kw.line,
                    col: kw.col,
                }),
                None => Ok(Stmt::ForEach {
                    value: a,
                    value_line: a_line,
                    value_col: a_col,
                    coll: first,
                    body,
                    line: kw.line,
                    col: kw.col,
                }),
            }
        }
    }

    /// `do { body } while (cond);`：循环体至少执行一次。
    fn parse_do_while(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::Do)?;
        let body = self.parse_block()?;
        self.expect(TokenKind::While)?;
        self.expect(TokenKind::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::DoWhile {
            body,
            cond,
            line: start.line,
            col: start.col,
        })
    }

    /// `match (subject) { pat => { ... } ... }`：多分支选择。要求必须有 `_` 通配分支。
    fn parse_match(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::Match)?;
        self.expect(TokenKind::LParen)?;
        let subject = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        self.expect(TokenKind::LBrace)?;
        let mut arms: Vec<MatchArm> = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.at_end() {
            let pline = self.peek().line;
            let pcol = self.peek().col;
            let pattern = match self.peek().kind {
                TokenKind::IntLit(n) => {
                    self.advance();
                    MatchPattern::Int(n)
                }
                TokenKind::BoolLit(b) => {
                    self.advance();
                    MatchPattern::Bool(b)
                }
                TokenKind::CharLit(c) => {
                    self.advance();
                    MatchPattern::Char(c)
                }
                TokenKind::StringLit(ref s) => {
                    let s = s.clone();
                    self.advance();
                    MatchPattern::Str(s)
                }
                TokenKind::Ident(ref id) if id == "_" => {
                    self.advance();
                    MatchPattern::Wildcard
                }
                _ => {
                    return Err(DialcError::new(
                        "语法错误：match 分支必须是字面量（int / bool / char / string）或通配 `_`",
                        pline,
                        pcol,
                    ))
                }
            };
            self.expect(TokenKind::Arrow)?; // =>
            let body = self.parse_block()?;
            arms.push(MatchArm {
                pattern,
                body,
                line: pline,
                col: pcol,
            });
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Stmt::Match {
            subject,
            arms,
            line: start.line,
            col: start.col,
        })
    }

    /// `defer <stmt>;`：延迟到离开当前作用域时按 LIFO 执行。
    fn parse_defer(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::Defer)?;
        // defer 后跟一条语句（普通语句或裸块）；以该语句自身的终止符（; 或 }）结束
        let inner = self.parse_stmt()?;
        Ok(Stmt::Defer(Box::new(inner), start.line, start.col))
    }

    /// `with` 资源作用域块（v1.02）。两种等价写法：
    ///
    /// ```dial
    /// with open(path, "r") as f { ... }   // 资源在前
    /// with f = open(path, "r") { ... }     // 名字在前
    /// ```
    ///
    /// 靠「当前 token 是标识符且**下一个** token 是 `=`」区分第二种写法。
    fn parse_with(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::With)?;
        let (resource, name, name_line, name_col) =
            if matches!(self.peek_kind(), TokenKind::Ident(_))
                && matches!(self.peek_at(1).kind, TokenKind::Assign)
            {
                // with 名 = 资源 { ... }
                let (n, l, c) = self.expect_ident_pos()?;
                self.expect(TokenKind::Assign)?;
                let r = self.parse_expr()?;
                (r, n, l, c)
            } else {
                // with 资源 as 名 { ... }
                let r = self.parse_expr()?;
                self.expect(TokenKind::As)?;
                let (n, l, c) = self.expect_ident_pos()?;
                (r, n, l, c)
            };
        let body = self.parse_block()?;
        Ok(Stmt::With {
            resource,
            name,
            name_line,
            name_col,
            body,
            line: start.line,
            col: start.col,
        })
    }

    /// `goto 标签;`（v1.02）
    fn parse_goto(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::Goto)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Goto(name, start.line, start.col))
    }

    /// `label 名字:`（v1.02）
    fn parse_label(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::Label)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        Ok(Stmt::Label(name, start.line, start.col))
    }

    fn parse_while(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::While)?;
        self.expect(TokenKind::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        let body = self.parse_block()?;
        Ok(Stmt::While {
            cond,
            body,
            line: start.line,
            col: start.col,
        })
    }

    fn parse_return(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        self.expect(TokenKind::Return)?;
        if self.check(&TokenKind::Semicolon) {
            self.advance();
            Ok(Stmt::Return(None, start.line, start.col))
        } else {
            let e = self.parse_expr()?;
            self.expect(TokenKind::Semicolon)?;
            Ok(Stmt::Return(Some(e), start.line, start.col))
        }
    }

    fn parse_try(&mut self) -> Result<Stmt> {
        let start = self.advance(); // try
        let body = self.parse_block()?;
        if !self.check(&TokenKind::Catch) {
            return Err(DialcError::new(
                "语法错误：try 块后必须紧跟 catch 子句",
                start.line,
                start.col,
            ));
        }
        self.advance(); // catch
        // catch 后：可选变量名（无圆括号），或直接 { 块。
        // 显式拒绝圆括号，避免视觉污染（用户要求：catch 后直接跟变量名）。
        let catch_var = match self.peek_kind().clone() {
            TokenKind::LParen => {
                return Err(DialcError::new(
                    "语法错误：catch 后不得使用圆括号，直接写变量名，例如 catch e { ... }",
                    start.line,
                    start.col,
                ))
            }
            TokenKind::Ident(_) => {
                let (cv, cvl, cvc) = self.expect_ident_pos()?;
                self.reject_const_shadow(&cv, cvl, cvc)?;
                Some((cv, cvl, cvc))
            }
            _ => None,
        };
        let catch_body = self.parse_block()?;
        let (catch_name, catch_line, catch_col) = catch_var.unwrap_or((
            String::new(),
            start.line,
            start.col,
        ));
        Ok(Stmt::Try {
            body,
            catch: Catch {
                var: if catch_name.is_empty() { None } else { Some(catch_name) },
                body: catch_body,
                var_line: catch_line,
                var_col: catch_col,
            },
            line: start.line,
            col: start.col,
        })
    }

    fn parse_throw(&mut self) -> Result<Stmt> {
        let start = self.advance(); // throw
        let e = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::Throw(e, start.line, start.col))
    }

    /// 解析 select 语句（v0.91）：
    /// ```dial
    /// select {
    ///     case recv(ch1) as a { ... }
    ///     case recv(ch2) as b { ... }
    ///     default { ... }
    /// }
    /// ```
    ///
    /// Dial 风格的两点适配（与 Go 的 `<-` 语法刻意不同）：
    ///   1. 接收永远是**函数调用** `recv(ch)`——没有 `<-` 运算符；
    ///   2. 绑定变量用 `as` 关键字：`case recv(ch) as v { ... }`。
    ///
    /// `default` 必须在所有 case 之后，且至多一个。
    fn parse_select(&mut self) -> Result<Stmt> {
        let start = self.advance(); // select
        self.expect(TokenKind::LBrace)?;

        let mut arms: Vec<SelectArm> = Vec::new();
        let mut default_body: Option<Vec<Stmt>> = None;

        loop {
            if self.check(&TokenKind::Case) {
                let case_tok = self.advance(); // case
                // case 后是一个接收调用：recv(ch)
                let chan = self.parse_expr()?;
                // as <变量名>
                if !self.check(&TokenKind::As) {
                    return Err(DialcError::new(
                        "语法错误：case 后必须用 as 绑定变量名，例如 case recv(ch) as v { ... }",
                        case_tok.line,
                        case_tok.col,
                    ));
                }
                self.advance(); // as
                let (var, vline, vcol) = self.expect_ident_pos()?;
                self.reject_const_shadow(&var, vline, vcol)?;
                let body = self.parse_block()?;
                arms.push(SelectArm {
                    var,
                    chan,
                    body,
                    var_line: vline,
                    var_col: vcol,
                });
                continue;
            }
            if self.check(&TokenKind::Default) {
                if !arms.is_empty() && default_body.is_some() {
                    return Err(DialcError::new(
                        "语法错误：default 至多只能出现一次，且必须放在所有 case 之后",
                        start.line,
                        start.col,
                    ));
                }
                if default_body.is_some() {
                    return Err(DialcError::new(
                        "语法错误：default 至多只能出现一次",
                        start.line,
                        start.col,
                    ));
                }
                self.advance(); // default
                default_body = Some(self.parse_block()?);
                // default 之后不再允许 case —— 直接结束循环
                if self.check(&TokenKind::Case) {
                    return Err(DialcError::new(
                        "语法错误：default 必须放在所有 case 之后",
                        start.line,
                        start.col,
                    ));
                }
                break;
            }
            break;
        }

        if arms.is_empty() && default_body.is_none() {
            return Err(DialcError::new(
                "语法错误：select 至少需要一个 case 分支",
                start.line,
                start.col,
            ));
        }

        self.expect(TokenKind::RBrace)?;
        Ok(Stmt::Select {
            arms,
            default_body,
            line: start.line,
            col: start.col,
        })
    }

    // ---- 表达式（优先级） ----

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_ternary()
    }

    /// 三元表达式 `cond ? a : b`（v1.00）。优先级最低（仅高于赋值语句），右结合
    /// （else 分支可继续是三元表达式，故用 `parse_expr` 解析）。
    fn parse_ternary(&mut self) -> Result<Expr> {
        let cond = self.parse_or()?;
        if self.check(&TokenKind::Question) {
            self.advance();
            let then_e = self.parse_expr()?;
            self.expect(TokenKind::Colon)?;
            let else_e = self.parse_expr()?;
            Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_e),
                Box::new(else_e),
            ))
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while self.check(&TokenKind::Or) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Binary(Box::new(left), BinOp::Or, Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_not()?;
        while self.check(&TokenKind::And) {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::Binary(Box::new(left), BinOp::And, Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.check(&TokenKind::Not) {
            self.advance();
            let e = self.parse_not()?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(e)));
        }
        self.parse_equality()
    }

    fn parse_equality(&mut self) -> Result<Expr> {
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Eq => Some(BinOp::Eq),
                TokenKind::Neq => Some(BinOp::Neq),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_comparison()?;
                left = Expr::Binary(Box::new(left), op, Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut left = self.parse_bit_or()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Lt => Some(BinOp::Lt),
                TokenKind::Gt => Some(BinOp::Gt),
                TokenKind::Le => Some(BinOp::Le),
                TokenKind::Ge => Some(BinOp::Ge),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_term()?;
                left = Expr::Binary(Box::new(left), op, Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    // 位运算层（v1.00，仅 int）。优先级：`|` < `^` < `&` < `<< >>`，
    // 整体低于比较/相等、高于加减（与 C 一致）。
    fn parse_bit_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_bit_xor()?;
        while self.check(&TokenKind::Pipe) {
            self.advance();
            let right = self.parse_bit_xor()?;
            left = Expr::Binary(Box::new(left), BinOp::BitOr, Box::new(right));
        }
        Ok(left)
    }

    fn parse_bit_xor(&mut self) -> Result<Expr> {
        let mut left = self.parse_bit_and()?;
        while self.check(&TokenKind::Caret) {
            self.advance();
            let right = self.parse_bit_and()?;
            left = Expr::Binary(Box::new(left), BinOp::BitXor, Box::new(right));
        }
        Ok(left)
    }

    fn parse_bit_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_shift()?;
        while self.check(&TokenKind::Amp) {
            self.advance();
            let right = self.parse_shift()?;
            left = Expr::Binary(Box::new(left), BinOp::BitAnd, Box::new(right));
        }
        Ok(left)
    }

    fn parse_shift(&mut self) -> Result<Expr> {
        let mut left = self.parse_term()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Shl => Some(BinOp::Shl),
                TokenKind::Shr => Some(BinOp::Shr),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_term()?;
                left = Expr::Binary(Box::new(left), op, Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> Result<Expr> {
        let mut left = self.parse_factor()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => Some(BinOp::Add),
                TokenKind::Minus => Some(BinOp::Sub),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_factor()?;
                left = Expr::Binary(Box::new(left), op, Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_factor(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => Some(BinOp::Mul),
                TokenKind::Slash => Some(BinOp::Div),
                TokenKind::Mod => Some(BinOp::Mod),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_unary()?;
                left = Expr::Binary(Box::new(left), op, Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.check(&TokenKind::Minus) {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::Neg, Box::new(e)));
        }
        // 按位取反 `~`（v1.00，仅 int）
        if self.check(&TokenKind::Tilde) {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::BitNot, Box::new(e)));
        }
        // 解引用 `*p`（v1.02）：裸指针求值
        if self.check(&TokenKind::Star) {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::Deref, Box::new(e)));
        }
        // 取地址 `&x`（v1.02）：取栈上值类型变量的地址
        if self.check(&TokenKind::Amp) {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::AddrOf, Box::new(e)));
        }
        self.parse_postfix()
    }

    /// `.` 后面的成员名（v1.03）：`map` 在词法层是类型关键字 `MapT`，
    /// 但 `arr.map(f)` 的方法名同样是 `map`，这里单独放行，避免为了一个
    /// 高阶方法去改动全局关键字表（那会连带影响类型解析）。
    fn expect_member_name(&mut self) -> Result<String> {
        if self.check(&TokenKind::MapT) {
            self.advance();
            return Ok("map".to_string());
        }
        self.expect_ident()
    }

    /// 后缀：处理 `.field` / `.method(args)` / `[index]`，可链式。
    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.check(&TokenKind::Dot) {
                self.advance();
                let name = self.expect_member_name()?;
                if self.check(&TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.check(&TokenKind::Comma) {
                                self.advance();
                                continue;
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    expr = Expr::Method(Box::new(expr), name, args);
                } else {
                    expr = Expr::Field(Box::new(expr), name);
                }
            } else if self.check(&TokenKind::LBracket) {
                self.advance();
                let idx = self.parse_expr()?;
                if self.check(&TokenKind::DotDot) {
                    self.advance();
                    let to = self.parse_expr()?;
                    self.expect(TokenKind::RBracket)?;
                    expr = Expr::Slice(Box::new(expr), Box::new(idx), Box::new(to));
                } else {
                    self.expect(TokenKind::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(idx));
                }
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::IntLit(n) => {
                self.advance();
                Ok(Expr::IntLit(n))
            }
            TokenKind::FloatLit(f) => {
                self.advance();
                Ok(Expr::FloatLit(f))
            }
            TokenKind::BoolLit(b) => {
                self.advance();
                Ok(Expr::BoolLit(b))
            }
            TokenKind::CharLit(c) => {
                self.advance();
                Ok(Expr::CharLit(c))
            }
            TokenKind::ByteLit(b) => {
                self.advance();
                Ok(Expr::ByteLit(b))
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Expr::StringLit(s))
            }
            TokenKind::InterpLit(pieces) => {
                self.advance();
                let mut parts = Vec::new();
                for piece in pieces {
                    match piece {
                        InterpPiece::Text(t) => parts.push(InterpPart::Text(t)),
                        InterpPiece::Expr(src) => {
                            let e = self.parse_interp_expr(&src, tok.line, tok.col)?;
                            parts.push(InterpPart::Expr(Box::new(e)));
                        }
                    }
                }
                Ok(Expr::Interp(parts))
            }
            TokenKind::LBracket => self.parse_array_lit(),
            // 括号表达式 `(expr)`：分组优先级，Pratt Parser 的「隐式优先级提升」。
            // 缺失该分支会导致 `let idx = i - (i / 3) * 3;` 这类合法代码在 LParen 处报
            // 「期望表达式，但遇到 LParen」（CVE-2024-DIAL-001 根因）。
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(inner)
            }
            TokenKind::Ident(name) => {
                // const 内联（v1.00）：文件级常量在使用点直接替换为字面量。
                if let Some((_, e)) = self.consts.get(&name).cloned() {
                    self.advance();
                    return Ok(e);
                }
                // 枚举变体 Name.Variant（v1.00）：整段内联为 int 字面量。
                if self.pos + 1 < self.tokens.len()
                    && matches!(self.tokens[self.pos + 1].kind, TokenKind::Dot)
                    && self.enum_names.contains(&name)
                {
                    self.advance(); // 消费枚举名
                    self.expect(TokenKind::Dot)?;
                    let (v, vl, vc) = self.expect_ident_pos()?;
                    let key = format!("{}.{}", name, v);
                    if let Some((_, e)) = self.consts.get(&key).cloned() {
                        return Ok(e);
                    }
                    return Err(DialcError::new(
                        format!("语法错误：枚举 '{}' 没有变体 '{}'", name, v),
                        vl, vc,
                    ));
                }
                self.advance();
                // 类型实参（v1.02）：`ptr_cast<rawptr<byte>>(x)`。
                // 只对 `ptr_cast` 开放——它必须带目标类型，否则「裸指针 ↔ 其它类型」
                // 就成了隐式转换，与零隐式转换铁律冲突。
                let mut type_arg: Option<Type> = None;
                if name == "ptr_cast" && self.check(&TokenKind::Lt) {
                    let save = self.pos;
                    self.advance();
                    match self.parse_type() {
                        Ok(ty) => {
                            if self.expect_gt().is_ok() {
                                type_arg = Some(ty);
                            } else {
                                self.pos = save;
                            }
                        }
                        _ => {
                            self.pos = save;
                        }
                    }
                }
                if self.check(&TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    // 类型实参作为**第一个实参**传入（Expr::TypeLit），
                    // 语义层据此还原目标裸指针类型。
                    if let Some(ty) = type_arg {
                        args.push(Expr::TypeLit(ty));
                    }
                    // spawn 的首参是「函数引用」，不是表达式：线程入口 thunk 要按目标
                    // 函数静态生成，因此这里直接消费一个顶层具名函数字面量。
                    // （v1.03 起 Dial 已有函数指针，但那走普通标识符表达式 Expr::Var。）
                    if name == "spawn" && !self.check(&TokenKind::RParen) {
                        args.push(Expr::FuncRef(self.expect_ident()?));
                        if self.check(&TokenKind::Comma) {
                            self.advance();
                        }
                    }
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.check(&TokenKind::Comma) {
                                self.advance();
                                continue;
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    Ok(Expr::Call(name, args))
                } else if self.check(&TokenKind::LBrace) {
                    // 结构体字面量：Name { f: e, ... }
                    self.parse_struct_lit(name)
                } else {
                    Ok(Expr::Var(name))
                }
            }
            _ => Err(DialcError::new(
                format!("语法错误：期望表达式，但遇到 {:?}", tok.kind),
                tok.line,
                tok.col,
            )),
        }
    }

    fn parse_array_lit(&mut self) -> Result<Expr> {
        self.expect(TokenKind::LBracket)?;
        let mut elems = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            loop {
                elems.push(self.parse_expr()?);
                if self.check(&TokenKind::Comma) {
                    self.advance();
                    continue;
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBracket)?;
        // 期望元素类型：非空数组由语义层从首个元素推断（None）；空数组 `[]` 在
        // parse_var_decl 已知声明类型时再补（见下方 KvMap 注入之后那段）。
        Ok(Expr::ArrayLit(elems, None))
    }

    fn parse_struct_lit(&mut self, name: String) -> Result<Expr> {
        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.at_end() {
            let fname = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let fe = self.parse_expr()?;
            fields.push((fname, fe));
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::StructLit(name, fields))
    }
}
