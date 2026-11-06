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
    /// 泛型哈希表（v1.00）：`map<K, V>`，独立 C 运行时（stdlib/dial_kvmap.c）。
    /// 键支持 int/float/bool/char/byte/string（string 键按条目 ARC 管理）；
    /// 值支持标量 int/float/bool/char/byte —— v1.00 刻意不开放 string 值，
    /// 因为 `get` 未命中时的「空串默认对象」会引入所有权移交歧义（见 §29）。
    KvMap(Box<Type>, Box<Type>),
    /// 集合（v1.00）：`set<T>`，底层复用 int→int map（成员键 -> 1）。
    /// v1.00 的元素类型仅支持 int；不透明指针、ARC 与 map 完全同构。
    Set(Box<Type>),
    /// 裸指针（v1.02）：`rawptr<T>`，指向一个 T 值。
    ///
    /// **核心红线：裸指针绝对不受 ARC 管理**——编译器不会为它 retain/release，
    /// 作用域结束时也不会自动释放；它的生命周期完全由开发者显式管理
    /// （`raw_free(p)` 或 `drop(p)`）。这条红线让「谁申请、谁释放」的契约成立，
    /// 也让裸指针与 ARC 容器之间不存在隐式转换（见 README §19.3）。
    ///
    /// 元素类型为 `Void` 时是「无类型裸指针」（`rawptr<void>`）：只能传递与释放，
    /// **不能做指针算术**（没有元素大小可依），必须先 `ptr_cast` 成具体类型。
    RawPtr(Box<Type>),
    /// 函数指针（v1.03）：写作 `fn(A, B): R`，IR 层就是一个 `ptr`（纯函数地址）。
    ///
    /// **核心红线：绝不捕获环境**——Dial 没有闭包，函数指针里只有代码地址，
    /// 没有隐藏的上下文堆对象、没有 Fn/FnMut/FnOnce 分类、也没有隐式的 retain/release。
    /// 回调需要携带状态时，由调用方显式把 `rawptr<void>` 作为参数传进去，
    /// 裸指针的生命周期完全由用户自己负责（见 README §32）。
    ///
    /// 签名严格匹配（零隐式转换铁律）：`fn(int): int` 与 `fn(float): int` 是不同类型。
    FnPtr(Vec<Type>, Box<Type>),
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
            Type::KvMap(..) => "map",
            Type::Set(_) => "set",
            Type::File => "file",
            Type::Error => "error",
            Type::Thread => "thread",
            Type::Channel(_) => "channel",
            Type::Struct(_) => "struct",
            Type::Opaque(_) => "opaque",
            Type::RawPtr(_) => "rawptr",
            Type::FnPtr(..) => "fn",
        }
    }

    /// 类型显示名（含具体名，用于诊断信息）——包句柄要显示它自己的名字。
    pub fn display(&self) -> String {
        match self {
            Type::Struct(n) => format!("struct {}", n),
            Type::Opaque(n) => n.clone(),
            Type::KvMap(k, v) => format!("map<{}, {}>", k.display(), v.display()),
            Type::RawPtr(e) => format!("rawptr<{}>", e.display()),
            Type::FnPtr(ps, r) => format!(
                "fn({}): {}",
                ps.iter()
                    .map(|t| t.display())
                    .collect::<Vec<_>>()
                    .join(", "),
                r.display()
            ),
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
    // 位运算（v1.00，仅 int）：& | ^ 以及移位 << >>
    BitAnd, BitOr, BitXor, Shl, Shr,
}

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg, // 负号 -
    Not, // 逻辑非 not
    BitNot, // 按位取反 ~（v1.00，仅 int）
    /// 解引用裸指针 `*p`（v1.02）：读出指针所指的**值**。
    Deref,
    /// 取地址 `&x`（v1.02）：取栈上值类型变量的地址。
    ///
    /// 只允许对 **var（可变）** 的**值类型**变量使用——对 `let` 取址会绕过不可变约束，
    /// 对堆对象取址拿到的也只是槽位地址而非数据地址，两者都一律拒绝。
    AddrOf,
}

/// 字符串插值的一个片段（v1.00）。
#[derive(Debug, Clone)]
pub enum InterpPart {
    /// 普通文本。
    Text(String),
    /// `${...}` 内的表达式（由 parser 二次解析得到）。
    Expr(Box<Expr>),
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
    /// 字符串插值（v1.00）：`"x=${v}"` 编译为「逐段转字符串 + concat」。
    /// 每段要么是文本，要么是任意表达式（其类型必须可转字符串）。
    Interp(Vec<InterpPart>),
    Var(String),
    /// 字段访问：<expr>.<field>
    Field(Box<Expr>, String),
    /// 方法调用：<expr>.<method>(args)
    Method(Box<Expr>, String, Vec<Expr>),
    /// 索引访问：<expr>[index]
    Index(Box<Expr>, Box<Expr>),
    /// 数组切片 `arr[from..to]`（v1.00）：半开区间 [from, to)，返回同元素类型的新数组。
    /// 区间越界在运行时 Fail-Fast；句柄元素逐个 retain，绝不共享底层 buffer。
    Slice(Box<Expr>, Box<Expr>, Box<Expr>),
    /// 数组字面量：[e1, e2, ...]
    /// 第二个参数是「期望元素类型」：非空数组由第一个元素推断（None）；
    /// 空数组 `[]` 无法推断，由 parse_var_decl 在已知声明类型时填入（如 `var a: int[] = [];`）。
    ArrayLit(Vec<Expr>, Option<Box<Type>>),
    /// 结构体字面量：TypeName { f1: e1, f2: e2, ... }
    StructLit(String, Vec<(String, Expr)>),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    /// 三元表达式 `cond ? a : b`（v1.00）。cond 必须是 bool，`a` 与 `b` 类型必须相同。
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// 普通函数调用（print 或用户函数）
    Call(String, Vec<Expr>),
    /// 类型字面量（v1.02）：只出现在 `ptr_cast<T>(x)` 的**类型实参**位置。
    ///
    /// 它不是普通表达式：语义层在别处遇到它一律报错
    /// （「类型标注只能出现在 ptr_cast<T>(...) 的类型实参位置」）。
    TypeLit(Type),
    /// 线程入口的具名函数引用：`spawn(worker, ...)` 的首参。
    ///
    /// v1.03 起 Dial 已有一等**函数指针**（`Expr::Var` 指向函数名时求值为
    /// `Type::FnPtr`），但 `spawn` 需要按目标函数静态生成 pthread thunk，
    /// 因此它的首参仍必须写成顶层具名函数字面量；函数指针变量请用回调传参。
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
    /// 裸指针解引用赋值：`*p = value`（v1.02）。
    ///
    /// 裸指针是**唯一**能写到「别人家的内存」里的手段，因此这条语句是
    /// 内存安全的第一道闸门：只有 `rawptr<T>` 类型（T 为**具体**类型，
    /// `rawptr<void>` 不行）的指针才能被解引用写入。
    AssignDeref {
        ptr: Box<Expr>,
        value: Box<Expr>,
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
    /// 范围 for 循环：`for (i in start..end) { ... }` 或 `for (i in 0..10 step 2) { ... }`
    /// 循环变量 `i` 为 int，从 `start` 迭代到 `end - 1`（end 独占），可在循环体内被重新赋值。
    /// `step` 为可选步长（缺省 1），必须为正整数字面量或常量表达式（编译期求值）。
    For {
        var: String,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
        /// 循环变量**名**的位置（v0.95）。
        var_line: usize,
        var_col: usize,
    },
    /// 数组遍历：`for (v in arr) { ... }`。循环变量 `v` 的类型为数组元素类型 `T`，
    /// 顺序取 `arr` 的每个元素（0..len-1）。`arr` 必须为 `Array(T)`。
    ForEach {
        value: String,
        value_line: usize,
        value_col: usize,
        coll: Expr,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
    /// 映射遍历：`for (k, v in m) { ... }`。`k` 为键、`v` 为值（均为 int，因当前 `map` 为 int→int）。
    /// `m` 必须为 `map` 类型。底层先快照键集为 `Array(int)`，再逐键取值，迭代期间不修改 `m`。
    ForEachMap {
        key: String,
        key_line: usize,
        key_col: usize,
        value: String,
        value_line: usize,
        value_col: usize,
        coll: Expr,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
    /// `do { body } while (cond);`：循环体至少执行一次，之后条件为真则继续。
    DoWhile {
        body: Vec<Stmt>,
        cond: Expr,
        line: usize,
        col: usize,
    },
    /// `match (subject) { pattern => { body } ... }`：多分支选择。
    /// 要求**必须**有 `_` 通配分支（否则编译期报错，保证不漏分支、不引入未定义行为）。
    /// 支持字面量模式：`int` / `bool` / `char` / `string` 与 `_` 通配。
    Match {
        subject: Expr,
        arms: Vec<MatchArm>,
        line: usize,
        col: usize,
    },
    /// `defer <stmt>;`：将一条语句延迟到**离开当前作用域**时按 LIFO 执行（资源清理）。
    /// 作用域包括：函数体、裸块 `{ }`、循环体。在以下所有出口都会触发：正常落穿、
    /// `return`、导致离开该块的 `break` / `continue`。
    Defer(Box<Stmt>, usize, usize),
    /// `with <资源> as <名> { ... }`（v1.02）：**资源作用域块**。
    ///
    /// 资源表达式求值后绑定给块内的名字，离开块时**自动释放**
    /// （file → close、thread → join、裸指针 → free）。
    /// 块内 `goto` 不得跳出（否则自动释放会被绕过）——见 §31.2 约束 2。
    With {
        resource: Expr,
        name: String,
        name_line: usize,
        name_col: usize,
        body: Vec<Stmt>,
        line: usize,
        col: usize,
    },
    /// `label <名字>:`（v1.02）：跳转目标标签。函数内唯一。
    Label(String, usize, usize),
    /// `goto <名字>;`（v1.02）：无条件跳到同函数内的某个标签。
    /// 受三条死锁约束（禁止跨过变量初始化 / 禁止跳出 with 块 / 禁止跨越 try 边界）。
    Goto(String, usize, usize),
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

/// `match` 的一个分支：`pattern => { body }`（v1.00）。
#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub body: Vec<Stmt>,
    pub line: usize,
    pub col: usize,
}

/// `match` 的模式（v1.00）。
///
/// 仅支持字面量模式与通配符——不做类型守卫、不做绑定变量，保持编译期可判定、
/// 运行期 O(分支数) 线性比较的极简语义。键类型由 subject 的类型决定：
/// `int` 对应 `Int`、`bool` 对应 `Bool`、`char` 对应 `Char`、`string` 对应 `Str`。
#[derive(Debug, Clone)]
pub enum MatchPattern {
    Int(i64),
    Bool(bool),
    Char(i64),
    Str(String),
    /// `_` 通配：匹配任意值，必须存在（否则编译期报错，杜绝漏分支）。
    Wildcard,
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
    /// 默认实参（v1.00）：与 `params` 一一对应，`None` 表示必填形参。
    /// 带默认值的形参必须连续位于参数列表末尾（parser 强制）；调用点按声明顺序补齐。
    pub defaults: Vec<Option<Expr>>,
    /// 同类型变参声明（v1.03）：`...name: T`，只允许出现在参数列表末尾。
    /// parser 只负责把这个标记留在这里；**语义层在 analyze 开头把它脱糖成
    /// 一个普通的 `name: T[]` 形参**——编译后没有任何运行期类型标签或装箱，
    /// 变参数组就是一个正常的 ARC 数组。脱糖完成后该字段被清空。
    pub variadic: Option<(String, Type, usize, usize)>, // (名字, 元素类型, 行, 列)
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

/// 语言层外部函数声明（v1.04 FFI）：`extern fn puts(s: rawptr<byte>): int;`
///
/// 与 `.diali` 的区别：它**直接写在 `.dial` 源码里**，不必造包、不必 install/import。
/// 解析成 AST 后由 `main.rs` 转成 `FuncInfo` 并入外部函数表，
/// 语义检查与代码生成对它与包函数一视同仁。
#[derive(Debug, Clone)]
pub struct ExternFnDecl {
    pub name: String,
    pub params: Vec<(String, Type)>,
    pub ret: Type,
    /// C 风格异构变参（`...`），必须是最后一个形参。
    pub variadic: bool,
    pub line: usize,
    pub col: usize,
}

/// 静态库链接声明（v1.04 FFI）：`link "lib/libfoo.a";`
///
/// **铁律**：只接受 `.a` / `.lib` 静态归档；`.so` / `.dll` / `.dylib` 一律在编译期拒绝。
/// 系统库（libc / libm 等）由 C 编译器默认动态解析，不需要、也不允许用 `link` 指定，
/// 这样既保持系统兼容性，又不让庞大系统库进入 LTO 编译链路。
#[derive(Debug, Clone)]
pub struct LinkDecl {
    pub path: String,
    pub line: usize,
    pub col: usize,
}

/// C ABI 布局结构体（v1.04 FFI）：`extern struct Point { x: int, y: int }`
///
/// 字段顺序、对齐、padding、大小与 C 完全一致（LLVM 非 packed struct + C ABI 字段宽度）。
/// 它**只能经 `rawptr<T>` 访问**（代表 C 侧内存），不能按值传给 Dial 函数，
/// 也不能用 `StructLit` 构造——避免把 C 布局误当 Dial 值语义结构体。
#[derive(Debug, Clone)]
pub struct CStructDef {
    pub name: String,
    pub fields: Vec<(String, Type)>,
    pub line: usize,
    pub col: usize,
}

/// 整个程序 = `import` 列表 + 顶层结构体集合 + 顶层函数集合 + FFI 声明（v1.04）。
///
/// `imports` 是**包名**列表（v0.92）。包名只是名字——它引用的是
/// `~/.dial/packages/<name>/` 下已安装的包，编译器自行完成解压、选库与链接，
/// 用户不写路径、不写平台、不写版本（见 ARCHITECTURE.md §4.3「无缝」的确切含义）。
#[derive(Debug, Clone)]
pub struct Program {
    pub imports: Vec<String>,
    pub structs: Vec<StructDef>,
    pub functions: Vec<Func>,
    /// 语言层 `extern fn` 声明（v1.04）。
    pub externs: Vec<ExternFnDecl>,
    /// 语言层 `link` 静态库声明（v1.04）。
    pub links: Vec<LinkDecl>,
    /// `extern struct` C ABI 结构体定义（v1.04）。
    pub cstructs: Vec<CStructDef>,
}
