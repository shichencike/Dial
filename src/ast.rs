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
    /// 单个字符（v0.92）。
    ///
    /// 语义上是**一个 Unicode 码点**（不是字节、不是 UTF-8 片段），
    /// IR 层与 `int` 同为 i64 —— 这样它天然是**值类型**：
    /// 无 ARC、无堆分配、无空指针，与 `int` / `float` / `bool` 同一阵营。
    ///
    /// 之所以不复用 `int` 表示字符：`'a' + 1` 这种算术在语义上是可疑的，
    /// 而 `int` 允许任何算术。独立成类型后，编译期就能把「字符误当数字用」
    /// 挡在门外（char 不参与 + - * /，只可比较与显式转换）。
    Char,
    /// 单个**字节**（v0.98.2）：无符号 8 位值，取值范围 `0~255`。
    ///
    /// 与 `char` 的分工是**严格**的，二者绝不混淆：
    ///   - `char` 是**一个 Unicode 码点**（'a' = 97，'中' = 20013），
    ///     输出时按 UTF-8 编码为 1~4 字节；
    ///   - `byte` 是**一个字节**（0~255），与编码无关。
    ///
    /// IR 层同样是 `i64`（值类型：无 ARC、无堆分配、无空指针），
    /// 但**取值范围在运行时被强制检查**——超过 255 或为负数时当场 Fail-Fast，
    /// 而不是像 C 那样静默截断成另一个值（`300u8` 会变成 44，那是数据损坏）。
    ///
    /// 字面量写作 `0xFF` / `255`，用 `b'x'` 表示字节字面量。
    Byte,
    String, // UTF-8 字符串（IR 为 i8*，C 侧引用计数）
    /// 二进制字节缓冲区（v0.98.2）：不可变、ARC 管理的引用计数对象。
    ///
    /// 与 `string` 的分工：`string` 承载**文本**（UTF-8，语义上可打印），
    /// `bytes` 承载**任意字节**（可含 `\0`、可非 UTF-8）。
    /// 二者内存布局相同（`[refcount][len][data...]`），但**类型不同**——
    /// 零隐式转换铁律要求两者之间必须显式转换，
    /// 否则「把一段文本当二进制写进文件」这类错误会被编译器放过。
    ///
    /// 元素类型是 `byte`（0~255），与 `string` 的元素是「字节但语义上是 UTF-8 片段」相区分。
    Bytes,
    Array(Box<Type>), // 动态数组（v0.2 元素固定为 Int）
    Map,    // 哈希表（int -> int，不透明指针，C 侧引用计数 ARC）
    File,   // 文件句柄（不透明指针，封装 C 的 FILE*；显式 open/close，不受 ARC 管理）
    Error,  // 可恢复异常（不透明指针，ARC 管理；由 throw 抛出、catch 捕获，见 §26）
    Thread, // 线程句柄（不透明指针；显式 join / detach，不受 ARC 管理，见 §27）
    /// 通道（不透明指针；元素类型仅编译期可见，运行时已擦除）。
    /// 跨线程共享 -> 走**原子**引用计数 ARC；send/recv 是所有权移交，不做 retain/release。
    Channel(Box<Type>),
    Struct(String),   // 用户结构体（按名引用），值语义
    /// 包提供的**不透明句柄**（v0.92，见 ARCHITECTURE.md §1.3 的 `type Db;`）。
    ///
    /// 与 `file` / `thread` 同一阵营：IR 层是 `ptr`，**不受 ARC 管理**，
    /// 生命周期由包自己的 `xxx_open` / `xxx_close` 这类显式函数掌管。
    /// 用户拿不到内部布局——这正是「内部结构随便改也不破坏调用方」的根基（§4.4）。
    Opaque(String),
}

impl Type {
    /// 类型的显示名。借用接收者，避免在调用点频繁 clone。
    pub fn name(&self) -> &'static str {
        match *self {
            Type::Void => "void",
            Type::Int => "int",
            Type::Float => "float",
            Type::Bool => "bool",
            Type::Char => "char",
            Type::Byte => "byte",
            Type::String => "string",
            Type::Bytes => "bytes",
            Type::Array(_) => "array",
            Type::Map => "map",
            Type::File => "file",
            Type::Error => "error",
            Type::Thread => "thread",
            Type::Channel(_) => "channel",
            Type::Struct(_) => "struct",
            Type::Opaque(_) => "opaque",
        }
    }

    /// 类型显示名（含具体名，用于诊断信息）——包句柄要显示它自己的名字。
    pub fn display(&self) -> String {
        match self {
            Type::Struct(n) => format!("struct {}", n),
            Type::Opaque(n) => n.clone(),
            other => other.name().to_string(),
        }
    }
}

/// 二元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
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
    /// 字符字面量 `'a'`。值为该字符的 **Unicode 码点**（i64），与 `int` 同表示。
    CharLit(i64),
    /// 字节字面量 `b'a'`（v0.98.2）。值为**单个字节**（0~255）。
    /// 与 `CharLit` 语义不同：字符是 Unicode 码点，字节是 8 位无符号值。
    ByteLit(i64),
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
        /// 标识符**本身**的位置（v0.95）：Hover / 跳转定义用，定位到名字而非语句起点。
        name_line: usize,
        name_col: usize,
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
        line: usize,
        col: usize,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
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
        line: usize,
        col: usize,
        /// 循环变量**名**的位置（v0.95）。
        var_line: usize,
        var_col: usize,
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
    /// 多路复用（v0.91）：
    /// ```dial
    /// select {
    ///     case recv(ch1) as a { ... }
    ///     case recv(ch2) as b { ... }
    ///     default { ... }
    /// }
    /// ```
    /// **是语句而非表达式**——各 case 绑定的变量类型可以互不相同，
    /// 因此不存在「统一返回类型」可言，绝不能当表达式用。
    ///
    /// `default_body` 为 `None` 时 select 阻塞等待；为 `Some` 时是**非阻塞**语义：
    /// 没有任何通道就绪就立刻走 default。
    ///
    /// 全体通道 EOF（已关闭且已空）时，select **直接结束**，不执行任何分支
    /// （包括 default）——EOF 是终止信号，不是「又一个可触发的事件」。
    Select {
        arms: Vec<SelectArm>,
        default_body: Option<Vec<Stmt>>,
        line: usize,
        col: usize,
    },
    /// 表达式语句（例如 print(...)、obj.method() 等）。
    ///
    /// 带位置是所有语句的**统一要求**（v0.93）：诊断要能落在源码里，
    /// 不能一律报到文件开头。见 `semantic::stmt_pos`。
    Expr(Expr, usize, usize),
    /// 裸块 `{ ... }`：引入一个新的词法作用域（v0.98.next）。
    ///
    /// 用途：局部变量遮蔽、生命周期隔离、把一段语句当匿名作用域使用。
    /// 内部语句列表与 if/while/for 的 body 完全相同；块内声明的变量在块结束后
    /// 脱离作用域（引用无效），堆类型局部变量（数组/字符串/映射/错误/通道）会在
    /// 离开块时被释放（见 `codegen::release_scope_locals`）。
    Block {
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
}

/// catch 子句：直接含一个可选的捕获变量名（Identifier）与一个代码块（Block）。
/// 变量名省略时，catch 块内无法引用具体错误，仅用于「捕获到即不崩溃」。
#[derive(Debug, Clone)]
pub struct Catch {
    pub var: Option<String>,
    pub body: Vec<Stmt>,
    /// 捕获变量**名**的位置（v0.95）。
    pub var_line: usize,
    pub var_col: usize,
}

/// `select` 的一个 case 分支（v0.91）。
///
/// `chan` 必须是接收调用 `recv(ch)`——Dial 没有 Go 的 `<-` 运算符，
/// 接收永远是函数调用（与 `move_to` / `spawn` 一样是内建）。
/// `var` 是该 case 内绑定变量的名字，其类型由通道的元素类型自动推导，
/// 生命周期**严格限制**在本 case 的 `{}` 内。
#[derive(Debug, Clone)]
pub struct SelectArm {
    pub var: String,
    pub chan: Expr,
    pub body: Vec<Stmt>,
    /// 绑定变量**名**的位置（v0.95）。
    pub var_line: usize,
    pub var_col: usize,
}

/// 函数定义。
#[derive(Debug, Clone)]
pub struct Func {
    pub name: String,
    pub params: Vec<(String, Type)>,
    /// 形参**名**的位置（v0.95）：与 `params` 一一对应，供形参 Hover / 跳转定义。
    pub param_pos: Vec<(usize, usize)>,
    pub ret: Type,
    pub body: Vec<Stmt>,
    /// 函数名位置（v0.95）：跳转定义指向这里。
    pub line: usize,
    pub col: usize,
}

/// 结构体定义。
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<(String, Type)>,
    pub line: usize,
    pub col: usize,
    /// 结构体名位置（v0.95）。
    pub name_line: usize,
    pub name_col: usize,
}

/// 整个程序 = `import` 列表 + 顶层结构体集合 + 顶层函数集合。
///
/// `imports` 是**包名**列表（v0.92）。包名只是名字——它引用的是
/// `~/.dial/packages/<name>/` 下已安装的包，编译器自行完成解压、选库与链接，
/// 用户不写路径、不写平台、不写版本（见 ARCHITECTURE.md §4.3「无缝」的确切含义）。
#[derive(Debug, Clone)]
pub struct Program {
    pub imports: Vec<String>,
    pub structs: Vec<StructDef>,
    pub functions: Vec<Func>,
}
