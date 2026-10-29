//! 抽象语法树（AST）与类型定义。
//!
//! 相比 v0.1 的变化：
//! - 新增 `struct` 定义、定长/动态容器 `array<T>`、字符串方法、字段/索引访问。
//! - 引入 `void` 返回类型（支持无返回值函数）。
//! - `String` / `Array` 在 IR 层仅以“不透明指针”(i8*) 出现，底层由 C Runtime 管理。

/// dialc 类型。v0.2 已实现：Void/Int/Float/Bool/String/Array<T>/Struct(name)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Void,
    Int,    // 64 位有符号整数
    Float,  // 64 位浮点数
    Bool,   // 布尔
    String, // UTF-8 字符串（IR 为 i8*，C 侧引用计数）
    Array(Box<Type>), // 动态数组（v0.2 元素固定为 Int）
    Map,    // 哈希表（int -> int，不透明指针，C 侧引用计数 ARC）
    File,   // 文件句柄（不透明指针，封装 C 的 FILE*；显式 open/close，不受 ARC 管理）
    Error,  // 可恢复异常（不透明指针，ARC 管理；由 throw 抛出、catch 捕获，见 §26）
    Thread, // 线程句柄（不透明指针；显式 join / detach，不受 ARC 管理，见 §27）
    /// 通道（不透明指针；元素类型仅编译期可见，运行时已擦除）。
    /// 跨线程共享 -> 走**原子**引用计数 ARC；send/recv 是所有权移交，不做 retain/release。
    Channel(Box<Type>),
    Struct(String),   // 用户结构体（按名引用），值语义
}

impl Type {
    /// 类型的显示名。借用接收者，避免在调用点频繁 clone。
    pub fn name(&self) -> &'static str {
        match *self {
            Type::Void => "void",
            Type::Int => "int",
            Type::Float => "float",
            Type::Bool => "bool",
            Type::String => "string",
            Type::Array(_) => "array",
            Type::Map => "map",
            Type::File => "file",
            Type::Error => "error",
            Type::Thread => "thread",
            Type::Channel(_) => "channel",
            Type::Struct(_) => "struct",
        }
    }
}

/// 二元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div,
    Eq, Neq, Lt, Gt, Le, Ge,
    And, Or,
}

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg, // 负号 -
    Not, // 逻辑非 not
}

/// 表达式。
#[derive(Debug, Clone)]
pub enum Expr {
    IntLit(i64),
    FloatLit(f64),
    BoolLit(bool),
    StringLit(String),
    Var(String),
    /// 字段访问：<expr>.<field>
    Field(Box<Expr>, String),
    /// 方法调用：<expr>.<method>(args)
    Method(Box<Expr>, String, Vec<Expr>),
    /// 索引访问：<expr>[index]
    Index(Box<Expr>, Box<Expr>),
    /// 数组字面量：[e1, e2, ...]
    ArrayLit(Vec<Expr>),
    /// 结构体字面量：TypeName { f1: e1, f2: e2, ... }
    StructLit(String, Vec<(String, Expr)>),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    /// 普通函数调用（print 或用户函数）
    Call(String, Vec<Expr>),
    /// 函数引用：`spawn(worker, ...)` 的首参。Dial 没有一等函数与闭包，
    /// 因此线程入口只能是一个顶层具名函数；FuncRef 仅允许出现在 spawn 首参位置。
    FuncRef(String),
}

/// 语句。
#[derive(Debug, Clone)]
pub enum Stmt {
    /// 变量声明：var 可变 / let 不可变。
    /// move_hint 由 Sema 的数据流分析写入：当源变量此后不再被使用（最后一次引用）时置 true，
    /// 表示可“移动语义”直接转移所有权，无需引用计数 retain。
    VarDecl {
        name: String,
        ty: Type,
        mutable: bool,
        value: Expr,
        move_hint: bool,
        line: usize,
        col: usize,
    },
    /// 赋值语句（仅可对 var 变量赋值）。
    Assign {
        name: String,
        value: Expr,
        move_hint: bool,
        line: usize,
        col: usize,
    },
    /// 字段赋值：<lvalue>.field = value
    AssignField {
        obj: Box<Expr>,
        field: String,
        value: Expr,
        line: usize,
        col: usize,
    },
    /// 数组元素赋值：arr[i] = value（底层走 dial_array_set，结果写回变量）
    AssignIndex {
        arr: Box<Expr>,
        index: Box<Expr>,
        value: Expr,
        line: usize,
        col: usize,
    },
    If {
        cond: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Option<Vec<Stmt>>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    /// `break;`：跳出最内层循环（while / for）。仅允许出现在循环体内。
    Break(usize, usize),
    /// `continue;`：跳到下一次迭代。
    /// while 中回到条件判定；for 中先执行 `i = i + 1` 再回到条件判定（否则会死循环）。
    Continue(usize, usize),
    /// 范围 for 循环：`for (i in start..end) { ... }`
    /// 循环变量 `i` 为 int，从 `start` 迭代到 `end - 1`（end 独占），可在循环体内被重新赋值。
    For {
        var: String,
        start: Expr,
        end: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>, usize, usize),
    /// `throw <expr>;`：抛出可恢复异常。expr 必须是 Error 类型（编译期强制规范化）。
    Throw(Expr, usize, usize),
    /// `try { body } catch var { ... }` 或 `try { body } catch { ... }`（catch 无圆括号）。
    /// catch 捕获的错误变量名可选；省略时表示忽略具体错误，只要捕获到不崩溃即可。
    Try {
        body: Vec<Stmt>,
        catch: Catch,
        line: usize,
        col: usize,
    },
    /// 表达式语句（例如 print(...)、obj.method() 等）。
    Expr(Expr),
}

/// catch 子句：直接含一个可选的捕获变量名（Identifier）与一个代码块（Block）。
/// 变量名省略时，catch 块内无法引用具体错误，仅用于「捕获到即不崩溃」。
#[derive(Debug, Clone)]
pub struct Catch {
    pub var: Option<String>,
    pub body: Vec<Stmt>,
}

/// 函数定义。
#[derive(Debug, Clone)]
pub struct Func {
    pub name: String,
    pub params: Vec<(String, Type)>,
    pub ret: Type,
    pub body: Vec<Stmt>,
}

/// 结构体定义。
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<(String, Type)>,
    pub line: usize,
    pub col: usize,
}

/// 整个程序 = 顶层结构体集合 + 顶层函数集合。
#[derive(Debug, Clone)]
pub struct Program {
    pub structs: Vec<StructDef>,
    pub functions: Vec<Func>,
}
