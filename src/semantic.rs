//! 语义分析 / 静态类型检查 / 数据流与逃逸分析。
//!
//! 设计要点：
//! - 由于采用「静态强类型 + 显式标注」，所有类型在编译期确定。
//! - `type_of` 设计为泛型函数，语义检查与代码生成共用，避免两份类型推导逻辑分歧。
//! - 额外实现两 passes 的**数据流分析**：
//!   1) collect_refs：统计每个变量在函数中被“读取引用”的所有语句位置与上下文（是否位于循环/分支内）。
//!   2) set_moves：若某次「把变量 a 复制给 b」恰好是 a 的“最后一次引用”、且不在循环/分支里，
//!      则判定可走**移动语义**（move_hint=true），codegen 直接转移指针、跳过 refcount retain，
//!      这即是需求中的「只读推断 + 移动语义 → O(1) 共享 / 零开销转移」。
//! - 逃逸分析（栈分配）：struct 一律值语义、在 IR 中以 alloca 落在栈上；其是否“逃逸”（传入函数/
//!   被返回）决定传参方式（见 codegen）。容器（String/Array）则是不透明指针，交由 C Runtime 管理。

use std::collections::HashMap;

use crate::ast::*;
use crate::error::{DialcError, Result};

/// 函数签名表：参数类型列表 + 返回类型。语义检查与代码生成共用。
#[derive(Debug, Clone)]
pub struct FuncInfo {
    pub params: Vec<Type>,
    pub ret: Type,
}

/// 结构体表：名字 -> 字段(名字, 类型)。
pub type StructTable = HashMap<String, Vec<(String, Type)>>;

/// 作用域栈：外层 Vec 是作用域层级，内层 HashMap 存「名字 -> (类型, 附加信息)」。
/// 附加信息 V 在语义阶段是 `VarState`，在代码生成阶段是「(是否可变, 指针)」。
pub type Scopes<V> = Vec<HashMap<String, (Type, V)>>;

/// 语义阶段的变量附加信息。
///
/// `moved` 是 v0.8 所有权转移的核心状态：变量一旦被 `move_to` 移交，
/// 就永久进入「已转移」状态，此后任何读取 / 写入 / 再次转移都在编译期被拒绝。
/// 这是把「move 后不可再用」从**约定**升级为**编译器死命令**的那一位。
#[derive(Debug, Clone, Copy)]
pub struct VarState {
    pub mutable: bool,
    pub moved: bool,
}

impl VarState {
    pub fn new(mutable: bool) -> Self {
        Self { mutable, moved: false }
    }
}

/// 在作用域栈中按由内到外顺序查找变量类型。
pub fn lookup_type<V>(scopes: &Scopes<V>, name: &str) -> Option<Type> {
    for scope in scopes.iter().rev() {
        if let Some((t, _)) = scope.get(name) {
            return Some(t.clone());
        }
    }
    None
}

fn is_numeric(t: &Type) -> bool {
    matches!(*t, Type::Int | Type::Float)
}

/// 类型兼容性：**零隐式转换铁律**。
///
/// 两条类型只有在“完全相同”时才兼容——不存在 int→float 之类的隐式提升，
/// 也不存在 float→int 的隐式窄化。凡类型不同，一律视为不兼容，需在源码中
/// 用内置函数 `int_to_float` / `float_to_int` 显式转换。
///
/// 这与 §1「比 C 更严格」一致：C 允许 `int` 与 `float` 在算术/比较中隐式混用，
/// Dial 认为那正是大量隐蔽精度 bug 的来源，因此在编译期即切断。
pub fn types_compatible(arg: &Type, target: &Type) -> bool {
    arg == target
}

/// 该类型**可能**为空指针吗？（v0.9 的 `is_null` 的准入名单）
///
/// Dial 的句柄类型在 IR 里一律是 `i8*`，其中只有「可能取到空值」的那些才值得判空：
/// - `recv` / `try_recv` / `recv_timeout` 在 EOF 或超时后返回空指针
/// - `open` 之类可能返回空句柄的调用
///
/// `int` / `float` / `bool` 是**值类型**，永远非空；对它们判空是纯粹的噪音，
/// 因此在编译期直接拒绝，而不是让它变成一个恒为 false 的死代码。
pub fn is_nullable_type(t: &Type) -> bool {
    matches!(
        t,
        Type::String
            | Type::Array(_)
            | Type::Map
            | Type::Error
            | Type::Channel(_)
            | Type::Thread
            | Type::File
    )
}

/// `channel(cap)` 的类型是 `Channel(Void)` ——「元素类型未定」的占位。
/// 声明处写出具体元素类型 `channel<T>` 时，在这里统一为 T。
///
/// 之所以可行：元素类型对运行时毫无意义（队列只存 void*），它只服务于编译期
/// 的 `move_to` 类型匹配与 `recv` 的返回类型推导——后者从变量自身的静态类型取，
/// 因此无需把 T 回填进 AST，只需在声明处判定「占位可以兼容任意具体元素类型」。
fn channel_placeholder_compat(arg: &Type, target: &Type) -> bool {
    match (arg, target) {
        (Type::Channel(a), Type::Channel(t)) => {
            matches!(a.as_ref(), Type::Void) && !matches!(t.as_ref(), Type::Void)
        }
        _ => false,
    }
}

/// 求 lvalue 表达式根部的变量名（用于判断字段/索引赋值是否作用于可变变量）。
fn lvalue_root(e: &Expr) -> Option<&str> {
    match e {
        Expr::Var(n) => Some(n),
        Expr::Field(o, _) => lvalue_root(o),
        Expr::Index(o, _) => lvalue_root(o),
        _ => None,
    }
}

/// 推导表达式静态类型。泛型 V 仅作用于作用域附加信息，类型推导只用类型部分。
pub fn type_of<V>(
    expr: &Expr,
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    let lookup = |name: &str| lookup_type(scopes, name);
    match expr {
        Expr::IntLit(_) => Ok(Type::Int),
        Expr::FloatLit(_) => Ok(Type::Float),
        Expr::BoolLit(_) => Ok(Type::Bool),
        Expr::StringLit(_) => Ok(Type::String),
        Expr::Var(name) => lookup(name).ok_or_else(|| {
            DialcError::new(format!("语义错误：未声明的变量 '{}'", name), 0, 0)
        }),
        // 函数引用只在 spawn 首参位置有意义；出现在别处一律是误用。
        Expr::FuncRef(name) => Err(DialcError::new(
            format!(
                "语义错误：函数引用 '{}' 只能作为 spawn 的第一个参数（Dial 没有一等函数，函数不能当值传递）",
                name
            ),
            0,
            0,
        )),
        Expr::Field(obj, field) => {
            let ot = type_of(obj, scopes, funcs, structs)?;
            match ot {
                Type::Struct(sname) => {
                    let fields = structs
                        .get(&sname)
                        .ok_or_else(|| DialcError::new(format!("语义错误：未知结构体 '{}'", sname), 0, 0))?;
                    fields
                        .iter()
                        .find(|(f, _)| f == field)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| {
                            DialcError::new(
                                format!("语义错误：结构体 '{}' 没有字段 '{}'", sname, field),
                                0, 0,
                            )
                        })
                }
                _ => Err(DialcError::new(
                    format!("语义错误：类型 '{}' 没有字段 '{}'", ot.name(), field),
                    0, 0,
                )),
            }
        }
        Expr::Method(obj, method, args) => {
            let ot = type_of(obj, scopes, funcs, structs)?;
            match ot {
                Type::String => string_method_type(method, args, scopes, funcs, structs),
                Type::Array(_) => array_method_type(method, args, scopes, funcs, structs),
                Type::Map => map_method_type(method, args, scopes, funcs, structs),
                Type::Error => error_method_type(method, args, scopes, funcs, structs),
                _ => Err(DialcError::new(
                    format!("语义错误：类型 '{}' 不支持方法调用 '{}'", ot.name(), method),
                    0, 0,
                )),
            }
        }
        Expr::Index(arr, idx) => {
            let at = type_of(arr, scopes, funcs, structs)?;
            let it = type_of(idx, scopes, funcs, structs)?;
            if it != Type::Int {
                return Err(DialcError::new(
                    "语义错误：数组索引必须是 int 类型",
                    0, 0,
                ));
            }
            match at {
                Type::Array(elem) => Ok(*elem),
                _ => Err(DialcError::new(
                    format!("语义错误：类型 '{}' 不能被索引", at.name()),
                    0, 0,
                )),
            }
        }
        Expr::ArrayLit(elems) => {
            if elems.is_empty() {
                return Err(DialcError::new(
                    "语义错误：空数组字面量无法推断元素类型（当前版本暂不支持）",
                    0, 0,
                ));
            }
            // 元素类型由第一个元素确定，且**只能是 int**（C 侧按 int64 存储，
            // 若允许 float 会在跨越 FFI 边界时被静默截断，故编译期即拒绝）
            let mut elem_t = None;
            for e in elems {
                let t = type_of(e, scopes, funcs, structs)?;
                if t != Type::Int {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：数组字面量元素只支持 int，遇到 {} 元素",
                            t.name()
                        ),
                        0, 0,
                    ));
                }
                elem_t = Some(t);
            }
            Ok(Type::Array(Box::new(elem_t.unwrap_or(Type::Int))))
        }
        Expr::StructLit(name, fields) => {
            let sdef = structs
                .get(name)
                .ok_or_else(|| DialcError::new(format!("语义错误：未知结构体 '{}'", name), 0, 0))?;
            if fields.len() != sdef.len() {
                return Err(DialcError::new(
                    format!(
                        "语义错误：结构体 '{}' 需要 {} 个字段，但提供了 {}",
                        name,
                        sdef.len(),
                        fields.len()
                    ),
                    0, 0,
                ));
            }
            for (fname, fe) in fields {
                let expected = sdef
                    .iter()
                    .find(|(f, _)| f == fname)
                    .map(|(_, t)| t.clone())
                    .ok_or_else(|| {
                        DialcError::new(
                            format!("语义错误：结构体 '{}' 没有字段 '{}'", name, fname),
                            0, 0,
                        )
                    })?;
                let ft = type_of(fe, scopes, funcs, structs)?;
                if !types_compatible(&ft, &expected) {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：结构体 '{}' 字段 '{}' 类型不匹配（实参 {}，期望 {}）",
                            name, fname, ft.name(), expected.name()
                        ),
                        0, 0,
                    ));
                }
            }
            Ok(Type::Struct(name.clone()))
        }
        Expr::Unary(UnOp::Neg, e) => {
            let t = type_of(e, scopes, funcs, structs)?;
            if t == Type::Int || t == Type::Float {
                Ok(t)
            } else {
                Err(DialcError::new(
                    "语义错误：一元负号 '-' 只能用于数值类型（int/float）",
                    0, 0,
                ))
            }
        }
        Expr::Unary(UnOp::Not, e) => {
            let t = type_of(e, scopes, funcs, structs)?;
            if t == Type::Bool {
                Ok(Type::Bool)
            } else {
                Err(DialcError::new(
                    "语义错误：逻辑非 'not' 只能用于 bool 类型",
                    0, 0,
                ))
            }
        }
        Expr::Binary(left, op, right) => {
            let lt = type_of(left, scopes, funcs, structs)?;
            let rt = type_of(right, scopes, funcs, structs)?;
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                    // 零隐式转换：两个操作数必须类型完全相同，且同为 int 或同为 float。
                    // `1 + 2.0` 这类 int/float 混合不在编译期默默提升为 float，而是直接报错，
                    // 强制用户写 `int_to_float(1) + 2.0`。
                    if lt == rt && is_numeric(&lt) {
                        Ok(lt)
                    } else {
                        Err(DialcError::new(
                            "语义错误：算术运算(+ - * /)要求两个操作数类型完全相同且均为数值类型（int 或 float）；int 与 float 之间无隐式转换，请用 int_to_float / float_to_int 显式转换",
                            0, 0,
                        ))
                    }
                }
                BinOp::Eq | BinOp::Neq => {
                    // 同样禁止 int/float 混合比较：必须同类型。
                    if lt == rt && is_numeric(&lt) {
                        Ok(Type::Bool)
                    } else if lt == Type::Bool && rt == Type::Bool {
                        Ok(Type::Bool)
                    } else if lt == Type::String && rt == Type::String {
                        Ok(Type::Bool)
                    } else {
                        Err(DialcError::new(
                            "语义错误：相等/不相等(==, !=)要求两个操作数类型完全相同：数值（int 或 float，不可混合）、bool，或 string",
                            0, 0,
                        ))
                    }
                }
                BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    if lt == rt && is_numeric(&lt) {
                        Ok(Type::Bool)
                    } else {
                        Err(DialcError::new(
                            "语义错误：比较运算(< > <= >=)要求两个操作数类型完全相同且均为数值类型（int 或 float，不可混合）",
                            0, 0,
                        ))
                    }
                }
                BinOp::And | BinOp::Or => {
                    if lt == Type::Bool && rt == Type::Bool {
                        Ok(Type::Bool)
                    } else {
                        Err(DialcError::new(
                            "语义错误：逻辑运算(and/or)要求两个操作数均为 bool 类型",
                            0, 0,
                        ))
                    }
                }
            }
        }
        Expr::Call(name, args) => {
            // 内置类型转换函数：零隐式转换铁律的“安全阀”。
            // 它们是显式的、命名清晰的转换入口，编译为 LLVM 的 sitofp / fptosi，
            // 不依赖任何 C 运行时函数。
            match name.as_str() {
                "int_to_float" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：int_to_float 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：int_to_float 的参数必须是 int",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Float);
                }
                "float_to_int" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：float_to_int 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Float {
                        return Err(DialcError::new(
                            "语义错误：float_to_int 的参数必须是 float",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                "new_map" => {
                    if !args.is_empty() {
                        return Err(DialcError::new(
                            "语义错误：new_map 不接受参数",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Map);
                }
                "read_line" => {
                    if !args.is_empty() {
                        return Err(DialcError::new(
                            "语义错误：read_line 不接受参数",
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                // 文件 IO（v0.61）：file 句柄是不透明的 FILE* 封装，显式 open/close，不受 ARC 管理。
                // 错误处理遵循“无异常”原则：打开失败返回空指针、读失败返回空串、写失败返回 -1，
                // 绝不引入 Result 封装或自动抛错。
                "open" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：open 需要 2 个参数（path: string, mode: string）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            "语义错误：open 的第一个参数（path）必须是 string",
                            0, 0,
                        ));
                    }
                    if type_of(&args[1], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            "语义错误：open 的第二个参数（mode）必须是 string",
                            0, 0,
                        ));
                    }
                    return Ok(Type::File);
                }
                "read" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：read 需要 2 个参数（f: file, n: int）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::File {
                        return Err(DialcError::new(
                            "语义错误：read 的第一个参数必须是 file 句柄",
                            0, 0,
                        ));
                    }
                    if type_of(&args[1], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：read 的第二个参数（读取字节数）必须是 int",
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                "write" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：write 需要 2 个参数（f: file, content: string）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::File {
                        return Err(DialcError::new(
                            "语义错误：write 的第一个参数必须是 file 句柄",
                            0, 0,
                        ));
                    }
                    if type_of(&args[1], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            "语义错误：write 的第二个参数（内容）必须是 string",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                // close 同时服务于 file 句柄与 channel：二者都是「显式终结」资源，
                // 语义相同，因此复用同一个名字，而非为通道另造一个 close_channel。
                //
                // v0.8 时曾刻意不把 close 扩展到 channel，担心「关掉之后变量还在、
                // 作用域结束再 release 一次」造成二次释放；v0.81 复核后确认该担心不成立：
                // close 只**置标志 + 广播唤醒**，从不释放通道对象，也不动引用计数；
                // 对象的销毁仍然**只由原子 ARC 归零触发一次**。二者职责不重叠。
                "close" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：close 需要 1 个参数（file 句柄或 channel）",
                            0, 0,
                        ));
                    }
                    let ct = type_of(&args[0], scopes, funcs, structs)?;
                    if ct != Type::File && !matches!(ct, Type::Channel(_)) {
                        return Err(DialcError::new(
                            "语义错误：close 的参数必须是 file 句柄或 channel",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Void);
                }
                // 可恢复异常（v0.7）：Error 是内置构造函数，只能构造 Error 类型，
                // 且 throw 只允许抛出 Error（杜绝裸 int/string 被抛出，强制规范化）。
                "Error" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：Error 需要恰好 1 个参数（错误消息 string）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            "语义错误：Error 的参数必须是 string（错误消息）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Error);
                }

                // ==================================================================
                //  所有权转移与多线程并发（v0.8）
                // ==================================================================
                // 创建并**立即**启动线程。首参必须是具名函数（FuncRef）——Dial 没有
                // 一等函数与闭包，线程入口只能是一个顶层具名函数。
                "spawn" => {
                    if args.is_empty() {
                        return Err(DialcError::new(
                            "语义错误：spawn 至少需要 1 个参数（线程入口函数名）",
                            0, 0,
                        ));
                    }
                    let fname = match &args[0] {
                        Expr::FuncRef(n) => n.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：spawn 的第一个参数必须是函数名（Dial 没有一等函数与闭包，线程入口只能是顶层具名函数）",
                                0, 0,
                            ))
                        }
                    };
                    let target = funcs.get(&fname).ok_or_else(|| {
                        DialcError::new(
                            format!("语义错误：spawn 引用了未定义的函数 '{}'", fname),
                            0, 0,
                        )
                    })?;
                    // 线程入口必须返回 void：返回值无处安放，而堆类型返回值还会在
                    // 无人释放的情况下泄漏——与其静默泄漏，不如编译期拒绝。
                    if target.ret != Type::Void {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：spawn 的线程入口函数 '{}' 必须返回 void，其返回类型为 {}（线程返回值无处安放）",
                                fname,
                                target.ret.name()
                            ),
                            0, 0,
                        ));
                    }
                    // 结构体是值语义的聚合体，塞不进 8 字节的实参槽。
                    for pty in &target.params {
                        if matches!(pty, Type::Struct(_)) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：spawn 的线程入口函数 '{}' 含 struct 形参，当前版本不支持（线程实参槽固定 8 字节，装不下值语义聚合体）",
                                    fname
                                ),
                                0, 0,
                            ));
                        }
                    }
                    if args.len() - 1 != target.params.len() {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：spawn 传给 '{}' 的实参个数为 {}，但该函数需要 {} 个参数",
                                fname,
                                args.len() - 1,
                                target.params.len()
                            ),
                            0, 0,
                        ));
                    }
                    for (i, pty) in target.params.iter().enumerate() {
                        let at = type_of(&args[i + 1], scopes, funcs, structs)?;
                        if !types_compatible(&at, pty) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：spawn 传给 '{}' 的第 {} 个实参类型为 {}，与形参类型 {} 不兼容",
                                    fname,
                                    i + 1,
                                    at.name(),
                                    pty.name()
                                ),
                                0, 0,
                            ));
                        }
                    }
                    return Ok(Type::Thread);
                }
                "join" | "detach" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个参数（thread 句柄）", name),
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Thread {
                        return Err(DialcError::new(
                            format!("语义错误：{} 的参数必须是 thread 句柄", name),
                            0, 0,
                        ));
                    }
                    return Ok(Type::Void);
                }
                // 创建有界通道。返回 Channel(Void) —— 元素类型未定，由声明处统一。
                "channel" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：channel 需要 1 个参数（容量 cap: int）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：channel 的参数（容量）必须是 int",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Channel(Box::new(Type::Void)));
                }
                // recv：阻塞取出；try_recv：非阻塞取出（队列空立即返回）。
                // 二者返回类型与所有权规则完全一致（都返回元素类型 T，接收方成为唯一所有者），
                // 唯一区别是「空的时候等不等」——因此合并为一条校验，只按名字区分报错文案。
                //
                // v0.81 EOF 语义：通道已关闭且已空时，recv 不再永久阻塞而是返回
                // 「空值」；是否真的结束了由 done(ch) 判定，详见 README §27.8。
                "recv" | "try_recv" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个参数（channel）", name),
                            0, 0,
                        ));
                    }
                    let ct = type_of(&args[0], scopes, funcs, structs)?;
                    match ct {
                        // 返回「元素类型」：接收方成为该指针的唯一所有者（不做 retain）。
                        Type::Channel(elem) => {
                            if matches!(elem.as_ref(), Type::Void) {
                                return Err(DialcError::new(
                                    format!("语义错误：{} 的通道元素类型未定，请在声明处写出具体类型（例如 let ch: channel<string> = channel(8);）", name),
                                    0, 0,
                                ));
                            }
                            return Ok(elem.as_ref().clone());
                        }
                        _ => {
                            return Err(DialcError::new(
                                format!("语义错误：{} 的参数必须是 channel，实际为 {}", name, ct.name()),
                                0, 0,
                            ))
                        }
                    }
                }
                // recv_timeout(ch, ms)：带超时的接收。
                // 返回类型与所有权规则与 recv 完全一致（取出的元素归接收者所有），
                // 唯一区别是「等不到就返回空值」而不是无限期阻塞。
                // 返回空值（null）时用 is_null(x) 判别——这正是 v0.9 引入 is_null 的直接原因。
                "recv_timeout" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：recv_timeout 需要 2 个参数（channel, 超时毫秒数 int）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[1], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：recv_timeout 的超时参数必须是 int（毫秒）",
                            0, 0,
                        ));
                    }
                    let ct = type_of(&args[0], scopes, funcs, structs)?;
                    match ct {
                        Type::Channel(elem) => {
                            if matches!(elem.as_ref(), Type::Void) {
                                return Err(DialcError::new(
                                    "语义错误：recv_timeout 的通道元素类型未定，请在声明处写出具体类型（例如 let ch: channel<string> = channel(8);）",
                                    0, 0,
                                ));
                            }
                            return Ok(elem.as_ref().clone());
                        }
                        _ => {
                            return Err(DialcError::new(
                                format!("语义错误：recv_timeout 的参数必须是 channel，实际为 {}", ct.name()),
                                0, 0,
                            ))
                        }
                    }
                }
                // is_null(x)：判断一个句柄是否为「空」。
                //
                // 这是 v0.9 的关键补充：recv / try_recv / recv_timeout 在 EOF 或超时后
                // 返回的是**空指针**，而 Dial 没有 Option 类型、没有 null 字面量，
                // 用户此前**无从分辨**「这是没有值」还是「这是一个真实值」——
                // 直接使用会撞上 Runtime 的 Fail-Fast 断言。
                // is_null 给出这条判别路径，让多消费者 / 超时场景可以被安全地写出。
                //
                // 只接受**可能为空**的句柄类型；int / float / bool 永远非空，
                // 对它们判空是纯粹的噪音，编译期直接拒绝。
                "is_null" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：is_null 需要 1 个参数（句柄）",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if !is_nullable_type(&at) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：is_null 的参数不能是 {}（标量永远非空，只有句柄才可能为空）",
                                at.name()
                            ),
                            0, 0,
                        ));
                    }
                    return Ok(Type::Bool);
                }
                // 通道状态查询（v0.81）：三个都只接受 channel，返回标量。
                //   closed(ch) -> bool：是否已调用过 close
                //   len(ch)    -> int ：队列中当前的剩余元素数
                //   done(ch)   -> bool：已关闭 且 已空 —— EOF 的精确定义，**单调**为真
                // done 的存在是为了让「while (!done(ch)) { r = recv(ch); }」这一行
                // 就能表达完整的收尾循环，用户不必自己拼 `len(ch) == 0 && closed(ch)`。
                "closed" | "done" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个参数（channel）", name),
                            0, 0,
                        ));
                    }
                    if !matches!(type_of(&args[0], scopes, funcs, structs)?, Type::Channel(_)) {
                        return Err(DialcError::new(
                            format!("语义错误：{} 的参数必须是 channel", name),
                            0, 0,
                        ));
                    }
                    return Ok(Type::Bool);
                }
                "len" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：len 需要 1 个参数（channel）",
                            0, 0,
                        ));
                    }
                    if !matches!(type_of(&args[0], scopes, funcs, structs)?, Type::Channel(_)) {
                        return Err(DialcError::new(
                            "语义错误：len 的参数必须是 channel（数组/字符串长度请用 .len() 方法）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                // move_to(变量, 通道)：编译期 intrinsic，把变量的所有权 O(1) 移交给通道。
                // 第一个参数必须是**变量**（而非任意表达式）——因为「清零原槽 + 标记
                // Moved」这两件事都只有在有确定存储位置时才成立。
                "move_to" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：move_to 需要 2 个参数（变量, 目标 channel）",
                            0, 0,
                        ));
                    }
                    if !matches!(&args[0], Expr::Var(_)) {
                        return Err(DialcError::new(
                            "语义错误：move_to 的第一个参数必须是变量（所有权转移需要有确定的存储位置）",
                            0, 0,
                        ));
                    }
                    let vt = type_of(&args[0], scopes, funcs, structs)?;
                    let ct = type_of(&args[1], scopes, funcs, structs)?;
                    match ct {
                        Type::Channel(elem) => {
                            if *elem.as_ref() != vt {
                                return Err(DialcError::new(
                                    format!(
                                        "语义错误：move_to 的目标通道元素类型为 {}，与被移交变量的类型 {} 不匹配",
                                        elem.as_ref().name(),
                                        vt.name()
                                    ),
                                    0, 0,
                                ));
                            }
                        }
                        _ => {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：move_to 的第二个参数必须是 channel，实际为 {}",
                                    ct.name()
                                ),
                                0, 0,
                            ))
                        }
                    }
                    return Ok(Type::Void);
                }
                _ => {}
            }
            if name == "print" {
                if args.len() != 1 {
                    return Err(DialcError::new(
                        "语义错误：内置函数 print 需要恰好 1 个参数",
                        0, 0,
                    ));
                }
                let pt = type_of(&args[0], scopes, funcs, structs)?;
                match pt {
                    Type::Int | Type::Float | Type::Bool | Type::String => {}
                    Type::Array(_) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print 数组，请逐元素打印 print(a.get(i))",
                            0, 0,
                        ))
                    }
                    Type::Struct(_) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print 结构体，请逐字段打印",
                            0, 0,
                        ))
                    }
                    Type::Map => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print 哈希表，请逐值打印 print(m.get(k))",
                            0, 0,
                        ))
                    }
                    Type::File => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print 文件句柄（file）；请改用 read 读成 string 后再打印",
                            0, 0,
                        ))
                    }
                    Type::Error => {}
                    // 句柄打印没有任何可读语义，且与「不透明指针」契约冲突（打印就等于泄露内部）。
                    Type::Thread => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print thread 句柄",
                            0, 0,
                        ))
                    }
                    Type::Channel(_) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print channel；请 recv 出元素后再打印",
                            0, 0,
                        ))
                    }
                    Type::Void => {
                        return Err(DialcError::new(
                            "语义错误：print 的参数不能是 void",
                            0, 0,
                        ))
                    }
                }
                // print 是**语句**而非表达式：无返回值，因此不能出现在
                // 赋值右侧、return 或任何需要值的位置。
                Ok(Type::Void)
            } else {
                let info = funcs.get(name).ok_or_else(|| {
                    DialcError::new(format!("语义错误：调用了未定义的函数 '{}'", name), 0, 0)
                })?;
                if info.params.len() != args.len() {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：函数 '{}' 需要 {} 个参数，但传入了 {} 个",
                            name,
                            info.params.len(),
                            args.len()
                        ),
                        0, 0,
                    ));
                }
                for (a, pt) in args.iter().zip(info.params.iter()) {
                    let at = type_of(a, scopes, funcs, structs)?;
                    if !types_compatible(&at, pt) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：函数 '{}' 第 {} 个参数类型不匹配（实参 {}，形参 {}）",
                                name,
                                pt.name(),
                                at.name(),
                                pt.name()
                            ),
                            0, 0,
                        ));
                    }
                }
                Ok(info.ret.clone())
            }
        }
    }
}

/// 字符串方法的签名与返回类型。
fn string_method_type<V>(
    method: &str,
    args: &[Expr],
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    let arg_t = |i: usize| type_of(&args[i], scopes, funcs, structs);
    match method {
        "length" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：string.length() 不接受参数", 0, 0));
            }
            Ok(Type::Int)
        }
        "concat" => {
            if args.len() != 1 || arg_t(0)? != Type::String {
                return Err(DialcError::new(
                    "语义错误：string.concat() 需要 1 个 string 参数",
                    0, 0,
                ));
            }
            Ok(Type::String)
        }
        "get" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：string.get() 需要 1 个 int 参数（返回字符的 int 码）",
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        "substring" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || arg_t(1)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：string.substring(from, to) 需要 2 个 int 参数",
                    0, 0,
                ));
            }
            Ok(Type::String)
        }
        "clone" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：string.clone() 不接受参数", 0, 0));
            }
            Ok(Type::String)
        }
        "eq" => {
            if args.len() != 1 || arg_t(0)? != Type::String {
                return Err(DialcError::new(
                    "语义错误：string.eq() 需要 1 个 string 参数，返回 bool",
                    0, 0,
                ));
            }
            Ok(Type::Bool)
        }
        _ => Err(DialcError::new(
            format!("语义错误：string 无方法 '{}'", method),
            0, 0,
        )),
    }
}

/// 可恢复异常（v0.7）Error 类型的方法签名与返回类型。
fn error_method_type<V>(
    method: &str,
    args: &[Expr],
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    let _arg_t = |i: usize| type_of(&args[i], scopes, funcs, structs);
    match method {
        // e.msg()：返回错误消息字符串。
        "msg" => {
            if !args.is_empty() {
                return Err(DialcError::new(
                    "语义错误：error.msg() 不接受参数",
                    0, 0,
                ));
            }
            Ok(Type::String)
        }
        _ => Err(DialcError::new(
            format!("语义错误：error 无方法 '{}'（当前仅支持 .msg()）", method),
            0, 0,
        )),
    }
}

/// 动态数组方法（v0.2 元素为 int）的签名与返回类型。
fn array_method_type<V>(
    method: &str,
    args: &[Expr],
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    let arg_t = |i: usize| type_of(&args[i], scopes, funcs, structs);
    match method {
        "length" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：array.length() 不接受参数", 0, 0));
            }
            Ok(Type::Int)
        }
        "push" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：array.push() 需要 1 个 int 参数（元素类型固定为 int）",
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(Type::Int)))
        }
        "get" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：array.get() 需要 1 个 int 参数",
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        "set" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || arg_t(1)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：array.set(i, v) 需要 int 索引与 int 值 v",
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(Type::Int)))
        }
        "clone" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：array.clone() 不接受参数", 0, 0));
            }
            Ok(Type::Array(Box::new(Type::Int)))
        }
        _ => Err(DialcError::new(
            format!("语义错误：array 无方法 '{}'", method),
            0, 0,
        )),
    }
}

/// 哈希表方法（int -> int，C 侧 ARC 管理）的签名与返回类型。
fn map_method_type<V>(
    method: &str,
    args: &[Expr],
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    let arg_t = |i: usize| type_of(&args[i], scopes, funcs, structs);
    match method {
        "put" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || arg_t(1)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：map.put(k, v) 需要 2 个 int 参数（键与值）",
                    0, 0,
                ));
            }
            // 返回（可能更新后的）Map 指针，编译器把新指针写回接收变量（与 array.push 一致）。
            Ok(Type::Map)
        }
        "get" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：map.get(k) 需要 1 个 int 参数（键），未命中返回 -1",
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        "contains" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：map.contains(k) 需要 1 个 int 参数（键）",
                    0, 0,
                ));
            }
            Ok(Type::Bool)
        }
        "remove" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：map.remove(k) 需要 1 个 int 参数（键），未命中返回 -1",
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        "size" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：map.size() 不接受参数", 0, 0));
            }
            Ok(Type::Int)
        }
        _ => Err(DialcError::new(
            format!("语义错误：map 无方法 '{}'", method),
            0, 0,
        )),
    }
}

// ===================== 入口 =====================

/// 入口：收集函数签名 + 结构体表，校验 main，逐一检查函数体，最后跑数据流分析写入 move_hint。
/// 保留名：这些标识符在 `type_of` 里按**名字**匹配为内建函数/类型，
/// 与用户函数的查找是两条路。若允许用户定义同名函数，`Expr::Call` 的
/// 内建分支会被静默绕过（或反之）——因此必须在编译期直接拒绝。
const RESERVED_NAMES: &[&str] = &[
    "spawn", "join", "detach", "channel", "recv", "move_to", "thread",
    // v0.81 通道收尾：close 早已是 file 的内建（本就在按名字匹配，只是此前未列入此表），
    // 一并补齐；closed / len / done / try_recv 为本次新增。
    "close", "closed", "len", "done", "try_recv",
    // v0.9：判空与超时接收
    "is_null", "recv_timeout",
];

pub fn analyze(program: &mut Program) -> Result<(HashMap<String, FuncInfo>, StructTable)> {
    // 0) 保留名校验（函数名与结构体名都不能撞内建）
    for f in &program.functions {
        if RESERVED_NAMES.contains(&f.name.as_str()) {
            return Err(DialcError::new(
                format!(
                    "语义错误：'{}' 是内建函数的保留名，不能用作函数名",
                    f.name
                ),
                0, 0,
            ));
        }
    }
    for s in &program.structs {
        if RESERVED_NAMES.contains(&s.name.as_str()) {
            return Err(DialcError::new(
                format!(
                    "语义错误：'{}' 是内建函数的保留名，不能用作结构体名",
                    s.name
                ),
                s.line, s.col,
            ));
        }
    }

    // 1) 结构体表
    let mut structs: StructTable = HashMap::new();
    for s in &program.structs {
        if structs.contains_key(&s.name) {
            return Err(DialcError::new(
                format!("语义错误：结构体 '{}' 重复定义", s.name),
                s.line, s.col,
            ));
        }
        structs.insert(s.name.clone(), s.fields.clone());
    }
    // 校验字段类型引用的结构体确实存在
    for s in &program.structs {
        for (_, t) in &s.fields {
            if let Type::Struct(n) = t {
                if !structs.contains_key(n) {
                    return Err(DialcError::new(
                        format!("语义错误：结构体 '{}' 的字段引用了未定义的结构体 '{}'", s.name, n),
                        s.line, s.col,
                    ));
                }
            }
        }
    }

    // 2) 函数签名
    let mut funcs: HashMap<String, FuncInfo> = HashMap::new();
    for f in &program.functions {
        if funcs.contains_key(&f.name) {
            return Err(DialcError::new(
                format!("语义错误：函数 '{}' 重复定义", f.name),
                0, 0,
            ));
        }
        funcs.insert(
            f.name.clone(),
            FuncInfo {
                params: f.params.iter().map(|(_, t)| t.clone()).collect(),
                ret: f.ret.clone(),
            },
        );
    }

    if !funcs.contains_key("main") {
        return Err(DialcError::new("语义错误：缺少程序入口函数 main()", 0, 0));
    }
    if funcs["main"].ret.clone() != Type::Int {
        return Err(DialcError::new(
            "语义错误：main 函数必须返回 int（作为进程退出码）",
            0, 0,
        ));
    }

    for f in &program.functions {
        check_function(f, &funcs, &structs)?;
    }

    // 3) 数据流分析：写入 move_hint
    for f in &mut program.functions {
        run_usage_analysis(f, &structs);
    }

    Ok((funcs, structs))
}

/// 类型良构性检查：拒绝当前版本尚无法正确实现的类型。
///
/// 动态数组的 C 实现（dial_array_*）以 int64 存储元素，因此元素类型**只允许 int**。
/// 若放任 `float[]`，元素会在传入 C 时被 fptosi 静默截断——这正是必须在
/// 编译期就拒绝而非运行期静默降级的地方。
fn check_type_wellformed(t: &Type, ctx: &str, line: usize, col: usize) -> Result<()> {
    match t {
        Type::Array(elem) => {
            if matches!(elem.as_ref(), Type::Int) {
                Ok(())
            } else {
                Err(DialcError::new(
                    format!(
                        "语义错误：{} 是 {}[] 类型，但当前版本动态数组元素类型只支持 int",
                        ctx,
                        elem.as_ref().name()
                    ),
                    line,
                    col,
                ))
            }
        }
        // 通道元素类型白名单：只放行**句柄**（不透明指针）。
        //
        // 通道传的是所有权，只有句柄能做到 O(1) 的指针搬运。标量（int/float/bool）
        // 每发一次都要装箱（malloc 8 字节），既砸掉「零分配移交」的立身之本，
        // 又让 C 侧无法判断槽里的东西该 free 还是直接丢——所以编译期就拒绝。
        Type::Channel(elem) => {
            if matches!(elem.as_ref(), Type::Void) {
                return Ok(()); // 占位：元素类型未定
            }
            match elem.as_ref() {
                Type::String | Type::Array(_) | Type::Map | Type::Error | Type::Channel(_) => Ok(()),
                _ => Err(DialcError::new(
                    format!(
                        "语义错误：{} 是 channel<{}> 类型，但通道元素类型只支持不透明句柄（string / int[] / map / error / channel）",
                        ctx,
                        elem.as_ref().name()
                    ),
                    line,
                    col,
                )),
            }
        }
        _ => Ok(()),
    }
}

fn check_function(
    f: &Func,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<()> {
    // 形参类型良构性
    for (pname, pty) in &f.params {
        check_type_wellformed(pty, &format!("函数 '{}' 的形参 '{}'", f.name, pname), 0, 0)?;
    }
    // 返回类型良构性
    check_type_wellformed(
        &f.ret,
        &format!("函数 '{}' 的返回类型", f.name),
        0,
        0,
    )?;

    let mut scopes: Scopes<VarState> = vec![HashMap::new()];
    for (pname, pty) in &f.params {
        // 形参一律按可变登记（与既有行为一致：形参可在函数体内被重新赋值）。
        scopes[0].insert(pname.clone(), (pty.clone(), VarState::new(true)));
    }
    for stmt in &f.body {
        check_stmt(stmt, &mut scopes, funcs, f, structs, false, 0)?;
    }
    Ok(())
}

/// 取语句的行列号（用于诊断定位）；该语句不带位置时退化为 (0,0)。
fn stmt_line_col(stmt: &Stmt) -> (usize, usize) {
    match stmt {
        Stmt::VarDecl { line, col, .. } => (*line, *col),
        Stmt::Assign { line, col, .. } => (*line, *col),
        Stmt::AssignField { line, col, .. } => (*line, *col),
        Stmt::AssignIndex { line, col, .. } => (*line, *col),
        Stmt::Break(l, c) => (*l, *c),
        Stmt::Continue(l, c) => (*l, *c),
        Stmt::Return(_, l, c) => (*l, *c),
        Stmt::Throw(_, l, c) => (*l, *c),
        Stmt::Try { line, col, .. } => (*line, *col),
        _ => (0, 0),
    }
}

/// 把变量标记为「已转移」。向外找到**声明它的那个作用域**再置位，
/// 而不是只写 `last()`：这样「if 分支里 move 之后，主流程再用」也会被保守拦下。
fn mark_moved(scopes: &mut Scopes<VarState>, name: &str) {
    for scope in scopes.iter_mut().rev() {
        if let Some((_, st)) = scope.get_mut(name) {
            st.moved = true;
            return;
        }
    }
}

/// 语句级所有权检查：在类型检查**之前**执行。
///
/// 被 `move_to` 移交、或被 `spawn` 当作堆类型实参移交的变量，此后任何引用
/// ——读取、写入、再次转移——都在编译期当场拒绝。三者都必然经过一次
/// `Expr::Var` 引用，因此在这一处拦截即可同源覆盖，无需在各调用点分别判断语境。
///
/// 只遍历本语句自身的表达式，**不**下探嵌套语句块：嵌套块由 `check_stmt`
/// 递归时各自检查，重复下探只会造成重复标记。
fn check_moved_stmt(
    stmt: &Stmt,
    scopes: &mut Scopes<VarState>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<()> {
    let (line, col) = stmt_line_col(stmt);
    match stmt {
        Stmt::VarDecl { value, .. } => check_moved_expr(value, scopes, funcs, structs, line, col),
        Stmt::Assign { name, value, .. } => {
            // 写入目标同样受约束：给已转移的变量赋值，与读取它一样是错误。
            if scopes
                .iter()
                .rev()
                .find_map(|s| s.get(name))
                .map(|v| v.1.moved)
                .unwrap_or(false)
            {
                return Err(DialcError::new(
                    format!("语义错误：变量 '{}' 已转移，无法使用", name),
                    line,
                    col,
                ));
            }
            check_moved_expr(value, scopes, funcs, structs, line, col)
        }
        Stmt::AssignField { obj, value, .. } => {
            check_moved_expr(obj, scopes, funcs, structs, line, col)?;
            check_moved_expr(value, scopes, funcs, structs, line, col)
        }
        Stmt::AssignIndex { arr, index, value, .. } => {
            check_moved_expr(arr, scopes, funcs, structs, line, col)?;
            check_moved_expr(index, scopes, funcs, structs, line, col)?;
            check_moved_expr(value, scopes, funcs, structs, line, col)
        }
        Stmt::If { cond, .. } => check_moved_expr(cond, scopes, funcs, structs, line, col),
        Stmt::While { cond, .. } => check_moved_expr(cond, scopes, funcs, structs, line, col),
        Stmt::For { start, end, .. } => {
            check_moved_expr(start, scopes, funcs, structs, line, col)?;
            check_moved_expr(end, scopes, funcs, structs, line, col)
        }
        Stmt::Return(Some(e), ..) | Stmt::Throw(e, ..) | Stmt::Expr(e) => {
            check_moved_expr(e, scopes, funcs, structs, line, col)
        }
        // select（v0.91）：case 里的通道表达式与各分支体都要受所有权约束。
        // 分支体不在 Stmt 枚举里（是 Vec<Stmt>），因此必须手动递归。
        Stmt::Select {
            arms, default_body, ..
        } => {
            for arm in arms {
                check_moved_expr(&arm.chan, scopes, funcs, structs, line, col)?;
                for s in &arm.body {
                    check_moved_stmt(s, scopes, funcs, structs)?;
                }
            }
            if let Some(db) = default_body {
                for s in db {
                    check_moved_stmt(s, scopes, funcs, structs)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// 表达式级所有权检查（由 `check_moved_stmt` 驱动）。
fn check_moved_expr(
    e: &Expr,
    scopes: &mut Scopes<VarState>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
    line: usize,
    col: usize,
) -> Result<()> {
    match e {
        Expr::Var(name) => {
            let moved = scopes
                .iter()
                .rev()
                .find_map(|s| s.get(name))
                .map(|v| v.1.moved)
                .unwrap_or(false);
            if moved {
                Err(DialcError::new(
                    format!("语义错误：变量 '{}' 已转移，无法使用", name),
                    line,
                    col,
                ))
            } else {
                Ok(())
            }
        }
        Expr::Field(o, _) => check_moved_expr(o, scopes, funcs, structs, line, col),
        Expr::Index(o, i) => {
            check_moved_expr(o, scopes, funcs, structs, line, col)?;
            check_moved_expr(i, scopes, funcs, structs, line, col)
        }
        Expr::Method(o, _, args) => {
            check_moved_expr(o, scopes, funcs, structs, line, col)?;
            for a in args {
                check_moved_expr(a, scopes, funcs, structs, line, col)?;
            }
            Ok(())
        }
        Expr::Binary(l, _, r) => {
            check_moved_expr(l, scopes, funcs, structs, line, col)?;
            check_moved_expr(r, scopes, funcs, structs, line, col)
        }
        Expr::Unary(_, o) => check_moved_expr(o, scopes, funcs, structs, line, col),
        Expr::ArrayLit(elems) => {
            for a in elems {
                check_moved_expr(a, scopes, funcs, structs, line, col)?;
            }
            Ok(())
        }
        Expr::StructLit(_, fields) => {
            for (_, a) in fields {
                check_moved_expr(a, scopes, funcs, structs, line, col)?;
            }
            Ok(())
        }
        Expr::FuncRef(_) => Ok(()),
        Expr::Call(name, args) => {
            // move_to(x, ch)：x 的所有权当场移交给通道。
            if name == "move_to" && args.len() == 2 {
                if let Expr::Var(x) = &args[0] {
                    // 先校验（若 x 此前已被转移，这里就会报「已转移，无法使用」），通过后再标记。
                    check_moved_expr(&args[0], scopes, funcs, structs, line, col)?;
                    check_moved_expr(&args[1], scopes, funcs, structs, line, col)?;
                    mark_moved(scopes, x);
                    return Ok(());
                }
            }
            // spawn(fn, args...)：堆类型实参按所有权移交（channel 例外——它是共享的）。
            if name == "spawn" && args.len() >= 2 {
                for a in &args[1..] {
                    check_moved_expr(a, scopes, funcs, structs, line, col)?;
                    if let Expr::Var(v) = a {
                        if let Ok(t) = type_of(a, scopes, funcs, structs) {
                            if matches!(t, Type::String | Type::Array(_) | Type::Map | Type::Error) {
                                mark_moved(scopes, v);
                            }
                        }
                    }
                }
                return Ok(());
            }
            for a in args {
                check_moved_expr(a, scopes, funcs, structs, line, col)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// in_loop：当前是否处于循环体内（用于约束 break / continue）。
/// try_depth：当前嵌套的 try 层数（用于约束 throw 必须出现在 try 内）。
fn check_stmt(
    stmt: &Stmt,
    scopes: &mut Scopes<VarState>,
    funcs: &HashMap<String, FuncInfo>,
    f: &Func,
    structs: &StructTable,
    in_loop: bool,
    try_depth: usize,
) -> Result<()> {
    // 所有权检查必须排在类型检查之前：已移交的变量连「被引用」的资格都没有，
    // 更不该走到类型推导里去产生一条误导性的类型错误。
    check_moved_stmt(stmt, scopes, funcs, structs)?;
    match stmt {
        Stmt::VarDecl {
            name,
            ty,
            mutable,
            value,
            line,
            col,
            ..
        } => {
            // 声明类型必须先良构，再谈兼容
            check_type_wellformed(ty, &format!("变量 '{}'", name), *line, *col)?;
            let vt = type_of(value, scopes, funcs, structs)?;
            if !types_compatible(&vt, ty) && !channel_placeholder_compat(&vt, ty) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：变量 '{}' 的初始化表达式类型为 {}，与声明类型 {} 不兼容",
                        name,
                        vt.name(),
                        ty.name()
                    ),
                    *line, *col,
                ));
            }
            if scopes.last().unwrap().contains_key(name) {
                return Err(DialcError::new(
                    format!("语义错误：变量 '{}' 在当前作用域重复声明", name),
                    *line, *col,
                ));
            }
            // 可变性如实登记：var -> true，let -> false。
            // 绝不可一律记为 true，否则 let 的“不可变”承诺形同虚设。
            scopes
                .last_mut()
                .unwrap()
                .insert(name.clone(), (ty.clone(), VarState::new(*mutable)));
            Ok(())
        }
        Stmt::Assign {
            name,
            value,
            line,
            col,
            ..
        } => {
            let info = lookup_type(scopes, name).ok_or_else(|| {
                DialcError::new(format!("语义错误：未声明的变量 '{}'", name), *line, *col)
            })?;
            let (_ty, st) = scopes
                .iter()
                .rev()
                .find_map(|s| s.get(name).map(|v| v.clone()))
                .unwrap();
            if !st.mutable {
                return Err(DialcError::new(
                    format!("语义错误：变量 '{}' 由 let 声明，不可重新赋值", name),
                    *line, *col,
                ));
            }
            let vt = type_of(value, scopes, funcs, structs)?;
            if !types_compatible(&vt, &info) && !channel_placeholder_compat(&vt, &info) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：对 '{}' 的赋值类型不兼容（实参 {}，变量 {}）",
                        name,
                        vt.name(),
                        info.name()
                    ),
                    *line, *col,
                ));
            }
            Ok(())
        }
        Stmt::AssignField {
            obj,
            field,
            value,
            line,
            col,
        } => {
            let ot = type_of(obj, scopes, funcs, structs)?;
            match ot {
                Type::Struct(sname) => {
                    let fields = structs
                        .get(&sname)
                        .ok_or_else(|| DialcError::new(format!("语义错误：未知结构体 '{}'", sname), *line, *col))?;
                    let expected = fields
                        .iter()
                        .find(|(fld, _)| fld == field)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| {
                            DialcError::new(
                                format!("语义错误：结构体 '{}' 没有字段 '{}'", sname, field),
                                *line, *col,
                            )
                        })?;
                    let vt = type_of(value, scopes, funcs, structs)?;
                    if !types_compatible(&vt, &expected) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：对字段 '{}' 的赋值类型不兼容（实参 {}，字段 {}）",
                                field, vt.name(), expected.name()
                            ),
                            *line, *col,
                        ));
                    }
                    // 写入目标必须是可变变量
                    if let Some(root) = lvalue_root(obj) {
                        let mutable = scopes
                            .iter()
                            .rev()
                            .find_map(|s| s.get(root).map(|v| v.1.mutable))
                            .unwrap_or(false);
                        if !mutable {
                            return Err(DialcError::new(
                                format!("语义错误：字段赋值目标 '{}' 不是可变变量", root),
                                *line, *col,
                            ));
                        }
                    }
                }
                _ => {
                    return Err(DialcError::new(
                        format!("语义错误：类型 '{}' 没有字段 '{}'", ot.name(), field),
                        *line, *col,
                    ))
                }
            }
            Ok(())
        }
        Stmt::AssignIndex {
            arr,
            index,
            value,
            line,
            col,
        } => {
            let at = type_of(arr, scopes, funcs, structs)?;
            let it = type_of(index, scopes, funcs, structs)?;
            if it != Type::Int {
                return Err(DialcError::new("语义错误：数组索引必须是 int 类型", *line, *col));
            }
            match at {
                Type::Array(elem) => {
                    let vt = type_of(value, scopes, funcs, structs)?;
                    if !types_compatible(&vt, elem.as_ref()) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：数组元素赋值类型不兼容（实参 {}，元素 {}）",
                                vt.name(),
                                elem.name()
                            ),
                            *line, *col,
                        ));
                    }
                    if let Some(root) = lvalue_root(arr) {
                        let mutable = scopes
                            .iter()
                            .rev()
                            .find_map(|s| s.get(root).map(|v| v.1.mutable))
                            .unwrap_or(false);
                        if !mutable {
                            return Err(DialcError::new(
                                format!("语义错误：数组赋值目标 '{}' 必须是可变变量", root),
                                *line, *col,
                            ));
                        }
                    }
                }
                _ => {
                    return Err(DialcError::new(
                        format!("语义错误：类型 '{}' 不能被索引赋值", at.name()),
                        *line, *col,
                    ))
                }
            }
            Ok(())
        }
        Stmt::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let ct = type_of(cond, scopes, funcs, structs)?;
            if ct != Type::Bool {
                return Err(DialcError::new(
                    "语义错误：if 条件表达式必须是 bool 类型",
                    0, 0,
                ));
            }
            scopes.push(HashMap::new());
            for s in then_branch {
                check_stmt(s, scopes, funcs, f, structs, in_loop, try_depth)?;
            }
            scopes.pop();
            if let Some(eb) = else_branch {
                scopes.push(HashMap::new());
                for s in eb {
                    check_stmt(s, scopes, funcs, f, structs, in_loop, try_depth)?;
                }
                scopes.pop();
            }
            Ok(())
        }
        Stmt::While { cond, body } => {
            let ct = type_of(cond, scopes, funcs, structs)?;
            if ct != Type::Bool {
                return Err(DialcError::new(
                    "语义错误：while 条件表达式必须是 bool 类型",
                    0, 0,
                ));
            }
            scopes.push(HashMap::new());
            for s in body {
                check_stmt(s, scopes, funcs, f, structs, true, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::For { var, start, end, body } => {
            let st = type_of(start, scopes, funcs, structs)?;
            let et = type_of(end, scopes, funcs, structs)?;
            if st != Type::Int {
                return Err(DialcError::new(
                    "语义错误：for 循环下界（start）必须是 int 类型",
                    0, 0,
                ));
            }
            if et != Type::Int {
                return Err(DialcError::new(
                    "语义错误：for 循环上界（end）必须是 int 类型",
                    0, 0,
                ));
            }
            if scopes.last().unwrap().contains_key(var) {
                return Err(DialcError::new(
                    format!("语义错误：循环变量 '{}' 与外层变量重名", var),
                    0, 0,
                ));
            }
            // 循环变量作用域限定在循环体内；声明为可变（用户可在体内重新赋值）。
            scopes.push(HashMap::new());
            scopes
                .last_mut()
                .unwrap()
                .insert(var.clone(), (Type::Int, VarState::new(true)));
            for s in body {
                check_stmt(s, scopes, funcs, f, structs, true, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::Break(line, col) => {
            if !in_loop {
                return Err(DialcError::new(
                    "语义错误：break 只能出现在循环（while / for）体内",
                    *line, *col,
                ));
            }
            Ok(())
        }
        Stmt::Continue(line, col) => {
            if !in_loop {
                return Err(DialcError::new(
                    "语义错误：continue 只能出现在循环（while / for）体内",
                    *line, *col,
                ));
            }
            Ok(())
        }
        Stmt::Return(Some(e), line, col) => {
            let t = type_of(e, scopes, funcs, structs)?;
            if !types_compatible(&t, &f.ret) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：return 表达式类型 {} 与函数返回类型 {} 不兼容",
                        t.name(),
                        f.ret.name()
                    ),
                    *line, *col,
                ));
            }
            Ok(())
        }
        Stmt::Return(None, line, col) => {
            if f.ret != Type::Void {
                return Err(DialcError::new(
                    format!(
                        "语义错误：函数 '{}' 返回类型为 {}，return 必须带表达式",
                        f.name, f.ret.name()
                    ),
                    *line, *col,
                ));
            }
            Ok(())
        }
        Stmt::Throw(e, line, col) => {
            // 纪律：throw 只能出现在 try 块内，否则异常没有捕获点，等于“裸奔”崩溃。
            if try_depth == 0 {
                return Err(DialcError::new(
                    "语义错误：throw 必须出现在 try 块内（没有捕获点，异常无法被处理）",
                    *line, *col,
                ));
            }
            let t = type_of(e, scopes, funcs, structs)?;
            if t != Type::Error {
                return Err(DialcError::new(
                    format!(
                        "语义错误：只能抛出 Error 类型，禁止抛出裸的 {}（请使用 Error(\"...\") 构造可恢复异常）",
                        t.name()
                    ),
                    *line, *col,
                ));
            }
            Ok(())
        }
        Stmt::Try { body, catch, .. } => {
            // try 块体处于 try 上下文（try_depth+1），使块内嵌套 throw 合法。
            scopes.push(HashMap::new());
            for s in body {
                check_stmt(s, scopes, funcs, f, structs, in_loop, try_depth + 1)?;
            }
            scopes.pop();
            // catch 子句：自成一个作用域。若声明了捕获变量，登记为 Error 类型；
            // 变量名与外层（含 try 块）重名时由正常的重复声明检查在 body 内拦截。
            scopes.push(HashMap::new());
            if let Some(name) = &catch.var {
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), (Type::Error, VarState::new(false)));
            }
            for s in &catch.body {
                check_stmt(s, scopes, funcs, f, structs, in_loop, try_depth + 1)?;
            }
            scopes.pop();
            Ok(())
        }
        // 多路复用（v0.91）：
        //   select { case recv(ch) as v { ... } default { ... } }
        //
        // 三条硬规则：
        //   1) case 必须是**接收调用** `recv(ch)`——Dial 没有 Go 的 `<-` 运算符，
        //      接收永远是函数调用。
        //   2) 绑定变量的类型 = 通道的元素类型（自动推导），元素类型必须已确定
        //      （不允许 channel<void>）。
        //   3) 绑定变量的作用域**严格限制**在本 case 的 `{}` 内——
        //      它是「这一轮收到的那个值」，出了这个块就不该再被使用。
        Stmt::Select {
            arms,
            default_body,
            line,
            col,
        } => {
            for arm in arms {
                // 规则 1：形状校验
                let chan_expr = match &arm.chan {
                    Expr::Call(fname, cargs) if fname == "recv" && cargs.len() == 1 => &cargs[0],
                    _ => {
                        return Err(DialcError::new(
                            "语义错误：select 的 case 必须是接收调用 recv(ch)（Dial 没有 <- 运算符，接收永远是函数调用）",
                            *line,
                            *col,
                        ))
                    }
                };
                // 规则 2：元素类型必须已确定
                let ct = type_of(chan_expr, scopes, funcs, structs)?;
                let elem = match ct {
                    Type::Channel(e) => {
                        if matches!(e.as_ref(), Type::Void) {
                            return Err(DialcError::new(
                                "语义错误：select 的通道元素类型未定，请在声明处写出具体类型（例如 let ch: channel<string> = channel(8);）",
                                *line,
                                *col,
                            ));
                        }
                        e.as_ref().clone()
                    }
                    other => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：select 的 case 只能监听 channel，实际为 {}",
                                other.name()
                            ),
                            *line,
                            *col,
                        ))
                    }
                };
                // 规则 3：绑定变量只在本 case 的块内可见
                scopes.push(HashMap::new());
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(arm.var.clone(), (elem, VarState::new(true)));
                for s in &arm.body {
                    check_stmt(s, scopes, funcs, f, structs, in_loop, try_depth)?;
                }
                scopes.pop();
            }
            // default 分支自成一个作用域，没有绑定变量。
            if let Some(db) = default_body {
                scopes.push(HashMap::new());
                for s in db {
                    check_stmt(s, scopes, funcs, f, structs, in_loop, try_depth)?;
                }
                scopes.pop();
            }
            Ok(())
        }
        Stmt::Expr(e) => {
            // 「原地修改型」方法约束（push / set）：
            // 这两种操作会修改容器，且因 COW 可能更换底层指针，编译器把返回指针写回接收变量。
            // 因此接收者必须是**可重新赋值**的顶层变量（var）。
            //   - let 变量不可被原地修改（与不可变契约一致），调用即编译错误；
            //   - 字段 / 下标等表达式作为接收者也无法被可靠地写回，同样报错。
            // 若只需获得新容器而不就地修改，请以表达式形式使用：let b: int[] = a.push(v);
            if let Expr::Method(obj, method, _) = e {
                if matches!(method.as_str(), "push" | "set" | "put") {
                    match obj.as_ref() {
                        Expr::Var(name) => {
                            let mutable = scopes
                                .iter()
                                .rev()
                                .find_map(|s| s.get(name).map(|v| v.1.mutable))
                                .unwrap_or(false);
                            if !mutable {
                                return Err(DialcError::new(
                                    format!(
                                        "语义错误：变量 '{}' 由 let 声明，不可调用原地修改方法 '{}'（容器可能被修改并更换指针，必须是 var 变量）",
                                        name, method
                                    ),
                                    0, 0,
                                ));
                            }
                        }
                        _ => {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：方法 '{}' 会原地修改容器并可能更换指针，必须作用于顶层可变变量（var），不能直接作用于字段/下标等表达式",
                                    method
                                ),
                                0, 0,
                            ));
                        }
                    }
                }
            }
            type_of(e, scopes, funcs, structs)?;
            Ok(())
        }
    }
}

// ===================== 数据流 / 逃逸分析 =====================

/// 单次引用的位置与上下文安全性（是否处于循环/分支中，处于则不能作为“安全移动”的最后一次使用）。
#[derive(Debug, Clone)]
struct RefEntry {
    stmt: usize,
    unsafe_ctx: bool,
}

/// 每个变量的引用事实：最后一次引用语句序号、是否存在不安全上下文。
#[derive(Debug, Clone, Default)]
struct VarFacts {
    last: usize,
    unsafe_ctx: bool,
}

fn record_expr_refs(e: &Expr, si: usize, unsafe_ctx: bool, refs: &mut HashMap<String, Vec<RefEntry>>) {
    match e {
        Expr::Var(n) => refs.entry(n.clone()).or_default().push(RefEntry { stmt: si, unsafe_ctx }),
        Expr::Field(o, _) => record_expr_refs(o, si, unsafe_ctx, refs),
        Expr::Index(o, i) => {
            record_expr_refs(o, si, unsafe_ctx, refs);
            record_expr_refs(i, si, unsafe_ctx, refs);
        }
        Expr::Method(o, _, args) => {
            record_expr_refs(o, si, unsafe_ctx, refs);
            for a in args {
                record_expr_refs(a, si, unsafe_ctx, refs);
            }
        }
        Expr::ArrayLit(elems) => {
            for el in elems {
                record_expr_refs(el, si, unsafe_ctx, refs);
            }
        }
        Expr::StructLit(_, fields) => {
            for (_, fe) in fields {
                record_expr_refs(fe, si, unsafe_ctx, refs);
            }
        }
        Expr::Binary(l, _, r) => {
            record_expr_refs(l, si, unsafe_ctx, refs);
            record_expr_refs(r, si, unsafe_ctx, refs);
        }
        Expr::Unary(_, x) => record_expr_refs(x, si, unsafe_ctx, refs),
        Expr::Call(_, args) => {
            for a in args {
                record_expr_refs(a, si, unsafe_ctx, refs);
            }
        }
        _ => {}
    }
}

/// 第一遍：收集所有变量引用位置。
fn collect_refs(stmts: &[Stmt], si: &mut usize, loop_d: usize, cond_d: usize, refs: &mut HashMap<String, Vec<RefEntry>>) {
    for stmt in stmts {
        let unsafe_ctx = loop_d > 0 || cond_d > 0;
        match stmt {
            Stmt::VarDecl { value, .. } => {
                record_expr_refs(value, *si, unsafe_ctx, refs);
            }
            Stmt::Assign { value, .. } => {
                record_expr_refs(value, *si, unsafe_ctx, refs);
            }
            Stmt::AssignField { obj, value, .. } => {
                record_expr_refs(obj, *si, unsafe_ctx, refs);
                record_expr_refs(value, *si, unsafe_ctx, refs);
            }
            Stmt::AssignIndex { arr, index, value, .. } => {
                record_expr_refs(arr, *si, unsafe_ctx, refs);
                record_expr_refs(index, *si, unsafe_ctx, refs);
                record_expr_refs(value, *si, unsafe_ctx, refs);
            }
            Stmt::If { cond, then_branch, else_branch } => {
                record_expr_refs(cond, *si, unsafe_ctx, refs);
                collect_refs(then_branch, si, loop_d, cond_d + 1, refs);
                if let Some(eb) = else_branch {
                    collect_refs(eb, si, loop_d, cond_d + 1, refs);
                }
            }
            Stmt::While { cond, body } => {
                record_expr_refs(cond, *si, unsafe_ctx, refs);
                collect_refs(body, si, loop_d + 1, cond_d, refs);
            }
            Stmt::For { var: _, start, end, body } => {
                record_expr_refs(start, *si, unsafe_ctx, refs);
                record_expr_refs(end, *si, unsafe_ctx, refs);
                collect_refs(body, si, loop_d + 1, cond_d, refs);
            }
            Stmt::Return(Some(e), ..) => {
                record_expr_refs(e, *si, unsafe_ctx, refs);
            }
            Stmt::Throw(e, ..) => {
                record_expr_refs(e, *si, unsafe_ctx, refs);
            }
            Stmt::Try { body, catch, .. } => {
                collect_refs(body, si, loop_d, cond_d, refs);
                collect_refs(&catch.body, si, loop_d, cond_d, refs);
            }
            // select（v0.91）：各 case 的通道表达式与分支体都要纳入引用分析。
            Stmt::Select {
                arms, default_body, ..
            } => {
                for arm in arms {
                    record_expr_refs(&arm.chan, *si, unsafe_ctx, refs);
                    collect_refs(&arm.body, si, loop_d, cond_d, refs);
                }
                if let Some(db) = default_body {
                    collect_refs(db, si, loop_d, cond_d, refs);
                }
            }
            Stmt::Expr(e) => {
                record_expr_refs(e, *si, unsafe_ctx, refs);
            }
            _ => {}
        }
        *si += 1;
    }
}

/// 第二遍：依据引用事实，对「Var 复制」形式的赋值写入 move_hint。
fn set_moves(stmts: &mut [Stmt], si: &mut usize, loop_d: usize, cond_d: usize, facts: &HashMap<String, VarFacts>) {
    for stmt in stmts.iter_mut() {
        let _unsafe_ctx = loop_d > 0 || cond_d > 0;
        match stmt {
            Stmt::VarDecl { value, move_hint, .. } => {
                if let Expr::Var(src) = value {
                    if let Some(f) = facts.get(src) {
                        *move_hint = (f.last == *si) && !f.unsafe_ctx;
                    }
                }
            }
            Stmt::Assign { value, move_hint, .. } => {
                if let Expr::Var(src) = value {
                    if let Some(f) = facts.get(src) {
                        *move_hint = (f.last == *si) && !f.unsafe_ctx;
                    }
                }
            }
            Stmt::If { cond: _, then_branch, else_branch } => {
                set_moves(then_branch, si, loop_d, cond_d + 1, facts);
                if let Some(eb) = else_branch {
                    set_moves(eb, si, loop_d, cond_d + 1, facts);
                }
            }
            Stmt::While { body, .. } => {
                set_moves(body, si, loop_d + 1, cond_d, facts);
            }
            Stmt::For { body, .. } => {
                set_moves(body, si, loop_d + 1, cond_d, facts);
            }
            Stmt::Try { body, catch, .. } => {
                set_moves(body, si, loop_d, cond_d, facts);
                set_moves(&mut catch.body, si, loop_d, cond_d, facts);
            }
            // select（v0.91）
            Stmt::Select {
                arms, default_body, ..
            } => {
                for arm in arms {
                    set_moves(&mut arm.body, si, loop_d, cond_d, facts);
                }
                if let Some(db) = default_body {
                    set_moves(db, si, loop_d, cond_d, facts);
                }
            }
            _ => {}
        }
        *si += 1;
    }
}

/// 对单个函数运行数据流分析：先收集引用，再写回 move_hint。
fn run_usage_analysis(f: &mut Func, _structs: &StructTable) {
    let mut refs: HashMap<String, Vec<RefEntry>> = HashMap::new();
    let mut si = 0usize;
    collect_refs(&f.body, &mut si, 0, 0, &mut refs);

    let mut facts: HashMap<String, VarFacts> = HashMap::new();
    for (name, entries) in &refs {
        let mut last = 0usize;
        let mut unsafe_ctx = false;
        for e in entries {
            if e.stmt > last {
                last = e.stmt;
            }
            if e.unsafe_ctx {
                unsafe_ctx = true;
            }
        }
        facts.insert(name.clone(), VarFacts { last, unsafe_ctx });
    }

    let mut si2 = 0usize;
    set_moves(&mut f.body, &mut si2, 0, 0, &facts);
}
