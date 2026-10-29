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
use crate::lexer::{Token, TokenKind};

pub fn parse(tokens: Vec<Token>) -> Result<Program> {
    let mut p = Parser { tokens, pos: 0 };
    let mut structs = Vec::new();
    let mut functions = Vec::new();
    while !p.at_end() {
        if p.check(&TokenKind::Struct) {
            structs.push(p.parse_struct_def()?);
        } else {
            functions.push(p.parse_function()?);
        }
    }
    Ok(Program { structs, functions })
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
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
    fn expect_ident(&mut self) -> Result<String> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(DialcError::new(
                format!("语法错误：期望标识符，但遇到 {:?}", t.kind),
                t.line,
                t.col,
            )),
        }
    }

    // ---- 顶层 ----

    fn parse_struct_def(&mut self) -> Result<StructDef> {
        let start = self.advance(); // struct
        let name = self.expect_ident()?;
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
        })
    }

    fn parse_function(&mut self) -> Result<Func> {
        self.expect(TokenKind::Fn)?;
        let name = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                let pname = self.expect_ident()?;
                self.expect(TokenKind::Colon)?;
                let pty = self.parse_type()?;
                params.push((pname, pty));
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
            ret,
            body,
        })
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
            TokenKind::StringT => {
                self.advance();
                Type::String
            }
            TokenKind::VoidT => {
                self.advance();
                Type::Void
            }
            TokenKind::MapT => {
                self.advance();
                Type::Map
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
                            self.expect(TokenKind::Gt)?;
                            Type::Channel(Box::new(elem))
                        } else {
                            Type::Channel(Box::new(Type::Void))
                        }
                    }
                    // thread：线程句柄（不透明指针，不是泛型）
                    "thread" => Type::Thread,
                    _ => Type::Struct(n),
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
        match tok.kind {
            TokenKind::Var | TokenKind::Let => self.parse_var_decl(),
            TokenKind::If => self.parse_if(),
            TokenKind::While => self.parse_while(),
            TokenKind::For => self.parse_for(),
            TokenKind::Return => self.parse_return(),
            TokenKind::Try => self.parse_try(),
            TokenKind::Throw => self.parse_throw(),
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
            _ => {
                // 可能是赋值（含字段/索引 lvalue）或表达式语句
                let expr = self.parse_expr()?;
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
                    _ => {
                        self.expect(TokenKind::Semicolon)?;
                        Ok(Stmt::Expr(expr))
                    }
                }
            }
        }
    }

    fn parse_var_decl(&mut self) -> Result<Stmt> {
        let start = self.peek().clone();
        let mutable = matches!(start.kind, TokenKind::Var);
        self.advance(); // 消费 var / let
        let name = self.expect_ident()?;
        self.expect(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect(TokenKind::Assign)?;
        let value = self.parse_expr()?;
        self.expect(TokenKind::Semicolon)?;
        Ok(Stmt::VarDecl {
            name,
            ty,
            mutable,
            value,
            move_hint: false,
            line: start.line,
            col: start.col,
        })
    }

    fn parse_if(&mut self) -> Result<Stmt> {
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
        })
    }

    fn parse_for(&mut self) -> Result<Stmt> {
        self.expect(TokenKind::For)?;
        self.expect(TokenKind::LParen)?;
        let var = self.expect_ident()?;
        self.expect(TokenKind::In)?;
        let start = self.parse_expr()?;
        self.expect(TokenKind::DotDot)?;
        let end = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        let body = self.parse_block()?;
        Ok(Stmt::For {
            var,
            start,
            end,
            body,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt> {
        self.expect(TokenKind::While)?;
        self.expect(TokenKind::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        let body = self.parse_block()?;
        Ok(Stmt::While { cond, body })
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
            TokenKind::Ident(_) => Some(self.expect_ident()?),
            _ => None,
        };
        let catch_body = self.parse_block()?;
        Ok(Stmt::Try {
            body,
            catch: Catch {
                var: catch_var,
                body: catch_body,
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

    // ---- 表达式（优先级） ----

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
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
        let mut left = self.parse_term()?;
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
        self.parse_postfix()
    }

    /// 后缀：处理 `.field` / `.method(args)` / `[index]`，可链式。
    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.check(&TokenKind::Dot) {
                self.advance();
                let name = self.expect_ident()?;
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
                self.expect(TokenKind::RBracket)?;
                expr = Expr::Index(Box::new(expr), Box::new(idx));
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
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Expr::StringLit(s))
            }
            TokenKind::LBracket => self.parse_array_lit(),
            TokenKind::Ident(name) => {
                self.advance();
                if self.check(&TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    // spawn 的首参是「函数引用」，不是表达式：Dial 没有一等函数与闭包，
                    // 线程入口只能是一个顶层具名函数，因此这里直接消费标识符。
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
        Ok(Expr::ArrayLit(elems))
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
