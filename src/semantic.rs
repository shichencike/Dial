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
    /// 默认实参（v1.00）：与 `params` 一一对应；外部包函数一律为全 `None`。
    /// 必填参数个数 = 第一个 `Some` 的下标（默认值必须从末尾连续）。
    pub defaults: Vec<Option<Expr>>,
    /// C 风格异构变参（v1.03）：**只有 `.diali` 里的 `extern fn` 允许为 true**。
    /// 这是 FFI 特例：固定参数照常类型检查，尾部参数不检查类型、按 C varargs ABI 直传。
    /// Dial 自己定义的函数永远为 false —— 它的 `...xs: T` 已在 analyze 开头脱糖成 `T[]`。
    pub variadic: bool,
    /// FFI 导出（v1.04）：`exported` 为 true 表示该函数用 C 链接符号名（无 mangling）
    /// 与 C 调用约定对外暴露，可被 C 侧直接调用。语义层强制其签名仅含 C 兼容类型
    /// （标量 / rawptr / fnptr / extern struct 值），否则编译期拒绝。
    pub exported: bool,
}

/// 结构体表：名字 -> 字段(名字, 类型)。
pub type StructTable = HashMap<String, (Vec<(String, Type)>, HashMap<String, Expr>)>;

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
    // int32 / float32 同为数值类型：同类之间可算术 / 比较，
    // 但与 int / float 之间仍是零隐式转换（类型不同即拒绝）。
    matches!(*t, Type::Int | Type::Float | Type::Int32 | Type::Float32)
}

/// 原生向量逐元素二元运算的结果类型（v1.04 底层原语）。
///
/// 与标量规则刻意分开：向量没有短路逻辑、没有字符串拼接，位运算也不再只限
/// int 标量。两个操作数必须**完全相同的** `simd<T, N>`——零隐式转换铁律同样
/// 适用，`simd<int,4> + simd<float,4>` 直接拒绝，绝不自动提升。
fn simd_binary_type(lt: &Type, rt: &Type, op: BinOp) -> Result<Type> {
    if lt != rt || !matches!(lt, Type::Simd(..)) {
        return Err(DialcError::new(
            format!(
                "语义错误：向量运算要求两个操作数同为完全相同的 simd<T, N>（元素类型与长度都一致），实际为 {} 与 {}",
                lt.display(), rt.display()
            ),
            0, 0,
        ));
    }
    let (elem, n) = match lt {
        Type::Simd(e, n) => ((**e).clone(), *n),
        _ => unreachable!(),
    };
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            if matches!(elem, Type::Bool | Type::Char) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：simd<{}, {}> 不支持算术运算（{} 向量只可比较 / 位运算）；\
                         需要算术请改用 int / float / byte 向量",
                        elem.display(), n, elem.display()
                    ),
                    0, 0,
                ));
            }
            Ok(Type::Simd(Box::new(elem), n))
        }
        // LLVM 的向量比较直接返回 `<N x i1>`，正是 simd<bool, N> 的内存表示。
        BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
            Ok(Type::Simd(Box::new(Type::Bool), n))
        }
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
            if !matches!(elem, Type::Int | Type::Byte) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：向量位运算(& | ^ << >>)只支持整数元素（int / byte），实际为 simd<{}, {}>",
                        elem.display(), n
                    ),
                    0, 0,
                ));
            }
            Ok(lt.clone())
        }
        BinOp::And | BinOp::Or => Err(DialcError::new(
            "语义错误：逻辑 and/or 依赖短路求值，向量没有短路语义；逐元素请用位运算 & | ^",
            0, 0,
        )),
    }
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

/// B1：该 Method 调用的接收者是否为 impl 具名类型（用户 `struct` / 包不透明句柄）。
///
/// 用途：`push` / `set` / `put` / `add` 这几个名字同时是**内建容器的原地修改协议**
/// （COW 可能更换底层指针，codegen 会把返回的新指针 `build_store` 写回接收变量，
/// semantic 会强制接收者是可变 var）。一个 impl 方法即便恰好叫 `push`，也不该
/// 参与这套容器协议——否则会把 impl 的返回值（任意类型）错误写回变量槽、
/// 或对 `let` 声明的具名类型变量误报「不可原地修改」。
///
/// codegen `gen_stmt` 与 semantic `check_stmt` 都据此把容器特例**收窄到容器类型**：
/// 具名接收者一律跳过特例，走普通表达式求值链（进而命中 impl 分派）。
/// 借用 `type_of` 复用同一份类型推导，绝不新写一份判定导致漂移。
pub fn is_impl_receiver<V>(
    obj: &Expr,
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> bool {
    match type_of(obj, scopes, funcs, structs) {
        Ok(t) => impl_receiver_name(&t).is_some(),
        Err(_) => false,
    }
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
            | Type::Map | Type::Set(_) | Type::KvMap(..)
            | Type::Error
            | Type::Channel(_)
            | Type::Thread
            | Type::File
            | Type::Opaque(_)
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
        Expr::CharLit(_) => Ok(Type::Char),
        Expr::ByteLit(_) => Ok(Type::Byte),
        Expr::StringLit(_) => Ok(Type::String),
        Expr::Slice(arr, from, to) => {
            let at = type_of(arr, scopes, funcs, structs)?;
            let ft = type_of(from, scopes, funcs, structs)?;
            let tt = type_of(to, scopes, funcs, structs)?;
            if !matches!(at, Type::Array(_)) {
                return Err(DialcError::new(
                    "语义错误：切片 [a..b] 只能用于数组类型", 0, 0,
                ));
            }
            if ft != Type::Int || tt != Type::Int {
                return Err(DialcError::new(
                    "语义错误：切片 [a..b] 的上下界必须是 int 类型", 0, 0,
                ));
            }
            Ok(at)
        }
        Expr::Interp(parts) => {
            // 插值片段的表达式类型必须可转为字符串（与 str() 的口径一致）。
            for p in parts {
                if let InterpPart::Expr(e) = p {
                    let t = type_of(e, scopes, funcs, structs)?;
                    match t {
                        Type::Int | Type::Float | Type::Int32 | Type::Float32 | Type::Bool | Type::Char | Type::String => {}
                        _ => {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：字符串插值 ${{...}} 内的表达式类型 {} 无法转为字符串",
                                    t.name()
                                ),
                                0, 0,
                            ))
                        }
                    }
                }
            }
            Ok(Type::String)
        }
        Expr::Var(name) => {
            if let Some(t) = lookup(name) {
                return Ok(t);
            }
            // v1.03：具名函数在右值位置求值为**纯函数地址**（Type::FnPtr）。
            // 不分配任何环境对象、不做捕获、不走 ARC —— 只有一个代码地址。
            if let Some(info) = funcs.get(name) {
                if info.variadic {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：C 变参外部函数 '{}' 的签名在编译期不闭合，不能取函数指针",
                            name
                        ),
                        0, 0,
                    ));
                }
                return Ok(Type::FnPtr(
                    info.params.clone(),
                    Box::new(info.ret.clone()),
                ));
            }
            Err(DialcError::new(
                format!("语义错误：未声明的变量 '{}'", name),
                0, 0,
            ))
        }
        // 函数引用只在 spawn 首参位置有意义；出现在别处一律是误用。
        // v1.03 起「把函数当值传递」走的是普通标识符表达式（Expr::Var 求值为函数指针），
        // 不需要 FuncRef 这个内部节点。
        Expr::FuncRef(name) => Err(DialcError::new(
            format!(
                "语义错误：函数引用 '{}' 只能作为 spawn 的第一个参数（要把函数当值传递，直接写函数名即可，它会求值为 fn(...) 函数指针）",
                name
            ),
            0,
            0,
        )),
        Expr::Field(obj, field) => {
            let ot = type_of(obj, scopes, funcs, structs)?;
            // v1.04 FFI：`rawptr<CStruct>` 也支持 `.field`——经裸指针读写 C 结构体字段。
            // 值语义 struct 与 C ABI struct 共用同一张字段表，差别只在 codegen 的寻址方式。
            let sname = match &ot {
                Type::Struct(n) => n.clone(),
                Type::RawPtr(inner) | Type::VolatilePtr(inner) => match &**inner {
                    Type::Struct(n) => n.clone(),
                    _ => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：类型 '{}' 没有字段 '{}'（裸指针字段访问需要 rawptr<结构体名>）",
                                ot.name(),
                                field
                            ),
                            0, 0,
                        ))
                    }
                },
                _ => {
                    return Err(DialcError::new(
                        format!("语义错误：类型 '{}' 没有字段 '{}'", ot.name(), field),
                        0, 0,
                    ))
                }
            };
            let fields = &structs
                .get(&sname)
                .ok_or_else(|| DialcError::new(format!("语义错误：未知结构体 '{}'", sname), 0, 0))?
                .0;
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
        Expr::Method(obj, method, args) => {
            let ot = type_of(obj, scopes, funcs, structs)?;
            // B1 impl 方法块：具名类型（用户 struct / 包不透明句柄）优先查
            // `__impl_<T>_<m>` 方法。命中则递归复用整条 `Expr::Call` 签名校验链
            // （参数个数 / 默认实参 / 逐参零隐式转换 / 返回类型），self 即首参。
            // 未命中不改动原有分派：内建容器方法照旧，未知方法给出明确诊断。
            if let Some(recv) = impl_receiver_name(&ot) {
                let symbol = impl_method_symbol(recv, method);
                if let Some(info) = funcs.get(&symbol) {
                    // 面向用户独立校验：`self` 是 info.params[0]，用户可见形参是
                    // params[1..]、默认值是 defaults[1..]。这样诊断里的参数计数与
                    // 序号都是用户写下的方法签名视角，既不含内部 mangle 名，也不把
                    // self 混进「需要几个参数」。零隐式转换铁律照旧（types_compatible）。
                    // 代码生成层另用 make_impl_call 把 Method 递归降级为 Call，复用
                    // 实参求值 / ABI 适配 / build_call 全链路，ABI 天然不变。
                    let user_params = &info.params[1..];
                    let user_defaults = &info.defaults[1..];
                    let required = user_defaults
                        .iter()
                        .position(|d| d.is_some())
                        .unwrap_or(user_params.len());
                    if args.len() < required || args.len() > user_params.len() {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：方法 '{}.{}' 需要 {}~{} 个参数，但传入了 {} 个",
                                recv, method, required, user_params.len(), args.len()
                            ),
                            0, 0,
                        ));
                    }
                    for (i, (a, pt)) in args.iter().zip(user_params.iter()).enumerate() {
                        let at = type_of(a, scopes, funcs, structs)?;
                        if !types_compatible(&at, pt) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：方法 '{}.{}' 第 {} 个参数类型不匹配（实参 {}，形参 {}）",
                                    recv, method, i + 1, at.display(), pt.display()
                                ),
                                0, 0,
                            ));
                        }
                    }
                    return Ok(info.ret.clone());
                }
                // 该具名类型存在，但没有这个方法：Fail-Fast，别让它掉进
                // 底下笼统的「类型不支持方法调用」里。
                return Err(DialcError::new(
                    format!(
                        "语义错误：类型 '{}' 没有方法 '{}'（impl {} 中未定义，也不是内建方法）",
                        ot.display(), method, recv
                    ),
                    0, 0,
                ));
            }
            // v1.04 FFI：string.as_c_str() / bytes.as_ptr() → rawptr<byte>。
            // 由编译器跳过 8 字节长度头，**绝不**让用户在 C/Dial 侧手写 +8 offset hack。
            // 返回的裸指针只是借用：不受 ARC 管理，生命周期绑定原对象（见 README §33）。
            if (ot == Type::String && method == "as_c_str")
                || (ot == Type::Bytes && method == "as_ptr")
            {
                if !args.is_empty() {
                    return Err(DialcError::new(
                        format!("语义错误：{}() 不接受参数", method),
                        0, 0,
                    ));
                }
                return Ok(Type::RawPtr(Box::new(Type::Byte)));
            }
            match ot {
                Type::String => string_method_type(method, args, scopes, funcs, structs),
                Type::Array(elem) => array_method_type(elem.as_ref(), method, args, scopes, funcs, structs),
                Type::Map => map_method_type(method, args, scopes, funcs, structs),
                Type::Set(elem) => set_method_type(elem.as_ref(), method, args, scopes, funcs, structs),
                Type::KvMap(k, v) => kvmap_method_type(k.as_ref(), v.as_ref(), method, args, scopes, funcs, structs),
                Type::Error => error_method_type(method, args, scopes, funcs, structs),
                Type::RawPtr(elem) | Type::VolatilePtr(elem) => rawptr_method_type(elem.as_ref(), method, args, scopes, funcs, structs),
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
                // 向量索引 v[i] 取出一个 lane，结果是元素标量类型。
                Type::Simd(elem, _) => Ok(*elem),
                _ => Err(DialcError::new(
                    format!("语义错误：类型 '{}' 不能被索引", at.name()),
                    0, 0,
                )),
            }
        }
        Expr::ArrayLit(elems, expect) => {
            // 期望元素类型优先（声明标注 / 变参脱糖）；否则从首元素推断。
            // **无论有没有期望类型，都逐元素校验一致性**：变参脱糖生成的数组一定带
            // expect=Some(T)，若沿用「有期望类型就跳过校验」的旧逻辑，
            // `sum(1, "a")` 会把字符串指针的位模式静默塞进 int[]（数据损坏）。
            // 这条校验正是「Dial 严禁异构变参」的落点。
            let elem_t: Type = if let Some(et) = expect {
                (**et).clone()
            } else if elems.is_empty() {
                return Err(DialcError::new(
                    "语义错误：空数组字面量 [] 无法推断元素类型；请为变量标注数组类型，例如 var a: int[] = [];",
                    0, 0,
                ));
            } else {
                type_of(&elems[0], scopes, funcs, structs)?
            };
            if !is_array_elem_type(&elem_t, structs) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：数组元素类型 {} 暂不支持（支持 int/float/bool/byte/char 与句柄类型 string / int[] / map / error / channel / 不透明指针，以及字段全为标量的结构体）",
                        elem_t.display()
                    ),
                    0, 0,
                ));
            }
            for e in elems {
                let t = type_of(e, scopes, funcs, structs)?;
                if !is_array_elem_type(&t, structs) {
                    return Err(DialcError::new(
                        format!("语义错误：数组字面量元素类型 {} 暂不支持", t.display()),
                        0, 0,
                    ));
                }
                if !types_compatible(&t, &elem_t) {
                    // expect=Some 是变参脱糖生成的数组（源码手写字面量为 None）：
                    // 给出「显式结构体」的引导，而不是让用户去找隐式转换。
                    let hint = if expect.is_some() {
                        "；变参只接受同类型实参，若需异构参数请显式定义结构体或自定义类型包"
                    } else {
                        ""
                    };
                    return Err(DialcError::new(
                        format!(
                            "语义错误：数组字面量元素类型不一致（{} 与 {}）{}",
                            elem_t.display(),
                            t.display(),
                            hint
                        ),
                        0, 0,
                    ));
                }
            }
            Ok(Type::Array(Box::new(elem_t)))
        }
        Expr::StructLit(name, fields) => {
            let sdef = structs
                .get(name)
                .ok_or_else(|| DialcError::new(format!("语义错误：未知结构体 '{}'", name), 0, 0))?;
            let field_defs = &sdef.0;
            let defaults = &sdef.1;
            // 字面量内重复字段名检测
            let mut seen = std::collections::HashSet::new();
            for (fname, _) in fields {
                if !seen.insert(fname.clone()) {
                    return Err(DialcError::new(
                        format!("语义错误：结构体 '{}' 字面量字段 '{}' 重复", name, fname),
                        0, 0,
                    ));
                }
            }
            // B8：每个结构体字段要么在字面量中提供，要么有显式编译期常量默认值；
            // 两者皆无则保留强类型底线，编译期报错（杜绝隐式未初始化内存）。
            for (fname, fty) in field_defs {
                let provided = fields.iter().any(|(f, _)| f == fname);
                if provided {
                    continue;
                }
                if let Some(de) = defaults.get(fname) {
                    let dt = type_of(de, scopes, funcs, structs)?;
                    if !types_compatible(&dt, fty) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：结构体 '{}' 字段 '{}' 的默认值类型不匹配（默认值 {}，期望 {}）",
                                name, fname, dt.name(), fty.name()
                            ),
                            0, 0,
                        ));
                    }
                } else {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：结构体 '{}' 的字段 '{}' 没有默认值，必须显式提供",
                            name, fname
                        ),
                        0, 0,
                    ));
                }
            }
            // 逐个检查已提供字段的类型
            for (fname, fe) in fields {
                let expected = field_defs
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
            } else if let Type::Simd(elem, n) = &t {
                // 逐元素取负：整数用 0 - v，浮点用 fneg，bool / char 无意义。
                if matches!(**elem, Type::Int | Type::Float | Type::Byte) {
                    Ok(t)
                } else {
                    Err(DialcError::new(
                        format!(
                            "语义错误：simd<{}, {}> 不支持一元负号（只支持 int / float / byte 向量）",
                            elem.display(), n
                        ),
                        0, 0,
                    ))
                }
            } else {
                Err(DialcError::new(
                    "语义错误：一元负号 '-' 只能用于数值类型（int/float）或数值向量",
                    0, 0,
                ))
            }
        }
        Expr::Unary(UnOp::BitNot, e) => {
            let t = type_of(e, scopes, funcs, structs)?;
            if t == Type::Int {
                Ok(Type::Int)
            } else if let Type::Simd(elem, _) = &t {
                if matches!(**elem, Type::Int | Type::Byte) {
                    Ok(t)
                } else {
                    Err(DialcError::new(
                        format!(
                            "语义错误：按位取反 '~' 只支持 int / byte 向量，实际为 {}",
                            t.display()
                        ),
                        0, 0,
                    ))
                }
            } else {
                Err(DialcError::new(
                    "语义错误：按位取反 '~' 只能用于 int 类型或 int / byte 向量",
                    0, 0,
                ))
            }
        }
        // 解引用 `*p`（v1.02）：裸指针 → 元素类型。
        Expr::Unary(UnOp::Deref, e) => {
            let t = type_of(e, scopes, funcs, structs)?;
            match t {
                Type::RawPtr(elem) | Type::VolatilePtr(elem) => {
                    if *elem == Type::Void {
                        return Err(DialcError::new(
                            "语义错误：rawptr<void> 是无类型裸指针，不能解引用（请先 ptr_cast 成具体类型）",
                            0, 0,
                        ));
                    }
                    Ok(*elem)
                }
                other => Err(DialcError::new(
                    format!(
                        "语义错误：解引用 '*' 只能用于裸指针，实际为 {}",
                        other.name()
                    ),
                    0, 0,
                )),
            }
        }
        // 取地址 `&x`（v1.02）：可变变量（栈上值类型）→ 指向它的裸指针。
        Expr::Unary(UnOp::AddrOf, e) => {
            if !matches!(&**e, Expr::Var(_)) {
                return Err(DialcError::new(
                    "语义错误：取地址 '&' 的对象必须是变量（如 &x），不能对表达式取址",
                    0, 0,
                ));
            }
            let t = type_of(e, scopes, funcs, structs)?;
            // 只允许**栈上的值类型**：堆对象（string / array / map ...）的「地址」
            // 只是槽位地址而非数据地址，取了也没有意义，一律拒绝。
            if !matches!(
                t,
                Type::Int | Type::Float | Type::Bool | Type::Char | Type::Byte | Type::Struct(_)
            ) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：不能对 {} 类型取地址（只有栈上的值类型 int/float/bool/byte/char/struct 可以）",
                        t.name()
                    ),
                    0, 0,
                ));
            }
            Ok(Type::RawPtr(Box::new(t)))
        }
        // 类型字面量（v1.02）：只允许出现在 ptr_cast<T>(...) 的类型实参位置。
        Expr::TypeLit(_) => Err(DialcError::new(
            "语义错误：类型标注只能出现在 ptr_cast<T>(...) 的类型实参位置",
            0, 0,
        )),
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
        Expr::Ternary(cond, then_e, else_e) => {
            let ct = type_of(cond, scopes, funcs, structs)?;
            if ct != Type::Bool {
                return Err(DialcError::new(
                    "语义错误：三元表达式的条件必须是 bool 类型",
                    0, 0,
                ));
            }
            let tt = type_of(then_e, scopes, funcs, structs)?;
            let et = type_of(else_e, scopes, funcs, structs)?;
            if !types_compatible(&tt, &et) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：三元表达式的两个分支类型必须相同（得到 {} 与 {}）",
                        tt.name(), et.name()
                    ),
                    0, 0,
                ));
            }
            Ok(tt)
        }
        Expr::Binary(left, op, right) => {
            let lt = type_of(left, scopes, funcs, structs)?;
            let rt = type_of(right, scopes, funcs, structs)?;
            // 原生向量：只要有一侧是 simd 就走独立规则，不落进下面的标量分支
            // （标量分支的诊断与结果类型都假设了标量，混进去会给出误导性错误）。
            if matches!(lt, Type::Simd(..)) || matches!(rt, Type::Simd(..)) {
                return simd_binary_type(&lt, &rt, *op);
            }
            match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                    // 字符串拼接：仅 + 支持 String + String -> String（CVE-2024-DIAL-006 新增）。
                    if matches!(op, BinOp::Add) && lt == Type::String && rt == Type::String {
                        Ok(Type::String)
                    } else if lt == rt && is_numeric(&lt) {
                        // 零隐式转换：两个操作数必须类型完全相同，且同为 int 或同为 float。
                        // `1 + 2.0` 这类 int/float 混合不在编译期默默提升为 float，而是直接报错，
                        // 强制用户写 `int_to_float(1) + 2.0`。
                        Ok(lt)
                    } else if lt == Type::Char || rt == Type::Char {
                        // char 不参与算术（§5.4）。这里单独给一条诊断：
                        // 通用那条消息会让人以为「补个转换就能做算术」，
                        // 而对 char 来说，`'a' + 1` 的意图根本无法判定——
                        // 想取下一个字母请 char_to_int 后自己算，再把结果 int_to_char 回来。
                        Err(DialcError::new(
                            "语义错误：char 不参与算术运算（+ - * / %）。'a' + 1 的意图无法判定（取下一个字母？还是拿码点做数值计算？）；\
                             请先 char_to_int(c) 得到码点，算完再用 int_to_char(x) 转回字符",
                            0, 0,
                        ))
                    } else {
                        Err(DialcError::new(
                            "语义错误：算术运算(+ - * / %)要求两个操作数类型完全相同且均为数值类型（int 或 float）；int 与 float 之间无隐式转换，请用 int_to_float / float_to_int 显式转换。字符串请用 + 拼接（如 a + b）",
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
                    } else if lt == Type::Char && rt == Type::Char {
                        // char 按码点比较：'a' < 'b' 成立，因为 97 < 98。
                        Ok(Type::Bool)
                    } else if lt == Type::String && rt == Type::String {
                        Ok(Type::Bool)
                    } else {
                        Err(DialcError::new(
                            "语义错误：相等/不相等(==, !=)要求两个操作数类型完全相同：数值（int 或 float，不可混合）、bool、char，或 string",
                            0, 0,
                        ))
                    }
                }
                BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    // char 可以比大小（码点序），但**不能与 int 混用**：
                    // 想拿码点数值请用 char_to_int 显式转换，绝不隐式把字符当数字。
                    let both_char = lt == Type::Char && rt == Type::Char;
                    if (lt == rt && is_numeric(&lt)) || both_char {
                        Ok(Type::Bool)
                    } else {
                        Err(DialcError::new(
                            "语义错误：比较运算(< > <= >=)要求两个操作数类型完全相同且均为数值类型（int 或 float，不可混合）或同为 char",
                            0, 0,
                        ))
                    }
                }
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                    // 位运算仅对 int 有意义（i64 上做按位操作）。
                    if lt == Type::Int && rt == Type::Int {
                        Ok(Type::Int)
                    } else {
                        Err(DialcError::new(
                            "语义错误：位运算(& | ^ ~ << >>)要求两个操作数均为 int 类型",
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
                // @asm<T>("模板", "约束", 输入...)（v1.04 底层原语）：LLVM inline asm。
                // 模板与约束必须是**字符串字面量**——LLVM 在编译期就要把约束解析成
                // 操作数映射，运行时字符串没有意义，也不能让用户输入任意模板。
                "asm" | "asm_intel" => {
                    if args.len() < 3 {
                        return Err(DialcError::new(
                            "语义错误：@asm 形如 @asm<T>(\"模板\", \"约束\", 输入...)，\
                             至少需要类型实参、汇编模板与约束字符串",
                            0, 0,
                        ));
                    }
                    let ret = match &args[0] {
                        Expr::TypeLit(ty) => ty.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：@asm 必须带返回类型实参，形如 @asm<int>(...)",
                                0, 0,
                            ))
                        }
                    };
                    // inline asm 的结果是**一个寄存器里的值**：ARC 容器 / 数组 /
                    // 结构体 / 向量都不可能由单条 asm 返回结果构造出来。
                    if !matches!(
                        ret,
                        Type::Int | Type::Float | Type::Int32 | Type::Float32 | Type::Bool | Type::Byte | Type::Char
                            | Type::RawPtr(_) | Type::VolatilePtr(_)
                    ) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：@asm 的返回类型必须是标量或裸指针，实际为 {}；\
                                 如需其它类型请先取得标量再显式转换",
                                ret.display()
                            ),
                            0, 0,
                        ));
                    }
                    for (i, a) in args.iter().enumerate().skip(1).take(2) {
                        if !matches!(a, Expr::StringLit(_)) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：@asm 的第 {} 个参数必须是字符串字面量（{}），不能是运行时表达式",
                                    i + 1,
                                    if i == 1 { "汇编模板" } else { "约束字符串" }
                                ),
                                0, 0,
                            ));
                        }
                    }
                    // 约束首项必须是输出约束（`=r` / `=&r` / `+r`），否则 LLVM
                    // 会把结果当未定义；verifier 的原生报错远不如这里直观。
                    let cons = match &args[2] {
                        Expr::StringLit(s) => s.clone(),
                        _ => unreachable!(),
                    };
                    let first = cons.split(',').next().unwrap_or("").trim();
                    if !(first.starts_with('=') || first.starts_with('+')) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：@asm 约束字符串必须以输出约束开头（如 \"=r,r\"），实际首项为 '{}'",
                                first
                            ),
                            0, 0,
                        ));
                    }
                    // 输入操作数同样只能是标量 / 裸指针：它们要直接进寄存器。
                    for a in args.iter().skip(3) {
                        let t = type_of(a, scopes, funcs, structs)?;
                        if !matches!(
                            t,
                            Type::Int | Type::Float | Type::Int32 | Type::Float32 | Type::Bool | Type::Byte | Type::Char
                                | Type::RawPtr(_) | Type::VolatilePtr(_)
                        ) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：@asm 的输入操作数必须是标量或裸指针，实际为 {}（ARC 容器 / 数组 / 结构体不能直接进寄存器）",
                                    t.display()
                                ),
                                0, 0,
                            ));
                        }
                    }
                    return Ok(ret);
                }
                // simd<T, N>(...)：向量构造。首参是 TypeLit(Simd(T,N))，
                // 1 个实参 = 广播到全部 lane，N 个实参 = 逐 lane 指定。
                "simd" => {
                    let ty = match args.first() {
                        Some(Expr::TypeLit(t)) => t.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：simd 构造必须带类型实参，形如 simd<int, 4>(...)",
                                0, 0,
                            ))
                        }
                    };
                    let (elem, n) = match &ty {
                        Type::Simd(e, n) => ((**e).clone(), *n),
                        other => {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：simd 构造的类型实参必须是 simd<T, N>，实际为 {}",
                                    other.display()
                                ),
                                0, 0,
                            ))
                        }
                    };
                    let vals = &args[1..];
                    if vals.is_empty() {
                        return Err(DialcError::new(
                            "语义错误：simd 构造需要 1 个标量（广播到全部 lane）或恰好 N 个标量（逐 lane）",
                            0, 0,
                        ));
                    }
                    if vals.len() != 1 && vals.len() != n as usize {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：simd<{}, {}> 构造需要 1 个或 {} 个实参，实际 {} 个",
                                elem.display(), n, n, vals.len()
                            ),
                            0, 0,
                        ));
                    }
                    for v in vals {
                        let t = type_of(v, scopes, funcs, structs)?;
                        if t != elem {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：simd<{}, {}> 的元素必须是 {} 类型，实际为 {}（无隐式转换）",
                                    elem.display(), n, elem.display(), t.display()
                                ),
                                0, 0,
                            ));
                        }
                    }
                    return Ok(ty);
                }
                // drop(x)：提前释放一个变量的所有权（move 语义）。参数的「消费后禁用」
                // 在 check_stmt 的 Expr 分支做（需要副作用 mark_moved，不在这里）。
                // 这里只做类型层面：drop 返回 Void，且参数必须是变量名。
                "drop" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：drop() 需要恰好 1 个参数（要释放的变量）",
                            0, 0,
                        ));
                    }
                    // 包提供的不透明句柄（type Db;）**不受 ARC 管理**：编译器既不会
                    // 为它 retain/release，作用域结束也不会释放它。drop 对它只是
                    // 静默 no-op，还会把变量误标为「已转移」——让人误以为资源已还。
                    // 因此编译期直接拦下，指引调用方走包自己的显式释放函数。
                    let dt = type_of(&args[0], scopes, funcs, structs)?;
                    if let Type::Opaque(n) = &dt {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：不透明句柄 '{}' 不受 ARC 管理，drop() 不会释放它；\
                                 请调用该包提供的显式释放函数（如 db_close(db)）手动释放",
                                n
                            ),
                            0, 0,
                        ));
                    }
                    // 函数指针是纯代码地址：没有所有权、没有堆对象，drop 对它无意义。
                    if matches!(dt, Type::FnPtr(..)) {
                        return Err(DialcError::new(
                            "语义错误：函数指针只是代码地址，不受 ARC 管理，drop() 对它无意义",
                            0, 0,
                        ));
                    }
                    return match &args[0] {
                        Expr::Var(_) => Ok(Type::Void),
                        _ => Err(DialcError::new(
                            "语义错误：drop() 的参数必须是变量名，例如 drop(buf)",
                            0, 0,
                        )),
                    };
                }
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
                // 32 位精确宽度 ↔ 64 位内部宽度的显式转换（FFI / 位级标量）。
                // int_to_int32 带范围校验（越界 Fail-Fast，绝不静默截断成垃圾）；
                // float_to_float32 是 IEEE 定义的精度收窄（binary64 → binary32），
                // 不是数据损坏，但源码必须显式写出，绝不隐式发生。
                "int_to_int32" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：int_to_int32 需要恰好 1 个参数", 0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：int_to_int32 的参数必须是 int", 0, 0,
                        ));
                    }
                    return Ok(Type::Int32);
                }
                "int32_to_int" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：int32_to_int 需要恰好 1 个参数", 0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Int32 {
                        return Err(DialcError::new(
                            "语义错误：int32_to_int 的参数必须是 int32", 0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                "float_to_float32" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：float_to_float32 需要恰好 1 个参数", 0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Float {
                        return Err(DialcError::new(
                            "语义错误：float_to_float32 的参数必须是 float", 0, 0,
                        ));
                    }
                    return Ok(Type::Float32);
                }
                "float32_to_float" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：float32_to_float 需要恰好 1 个参数", 0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Float32 {
                        return Err(DialcError::new(
                            "语义错误：float32_to_float 的参数必须是 float32", 0, 0,
                        ));
                    }
                    return Ok(Type::Float);
                }
                // char ↔ int 显式转换（v0.92）：与上面同族的「安全阀」。
                // char 在 IR 层就是 i64 码点，与 int 同表示，因此这两个转换
                // **不产生任何指令**——纯粹是编译期的类型视图切换（零成本）。
                // 想拿 'a' 的码点写 char_to_int('a')，想从码点构造字符写 int_to_char(97)。
                "char_to_int" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：char_to_int 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Char {
                        return Err(DialcError::new(
                            "语义错误：char_to_int 的参数必须是 char",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                "int_to_char" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：int_to_char 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if at != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：int_to_char 的参数必须是 int",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Char);
                }
                // 类型 -> 字符串：让 `print("x=" + str(n))` 这类拼接成为可能（CVE-2024-DIAL-006 新增）。
                // `str()` 是统一入口，按实参类型分派到 int_to_string / float_to_string / 字符串自身。
                "int_to_string" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：int_to_string 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：int_to_string 的参数必须是 int",
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                "float_to_string" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：float_to_string 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Float {
                        return Err(DialcError::new(
                            "语义错误：float_to_string 的参数必须是 float",
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                // int <-> byte：byte 是 8 位无符号值（0~255），
                // 与 int 的 64 位有符号范围不同，因此**需要范围校验**。
                // 这不是「零指令的类型视图切换」（char <-> int 才是）——
                // 越界的 int 转成 byte 会静默丢数据，那属于数据损坏。
                "byte_to_int" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：byte_to_int 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Byte {
                        return Err(DialcError::new(
                            "语义错误：byte_to_int 的参数必须是 byte",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                "int_to_byte" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：int_to_byte 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：int_to_byte 的参数必须是 int",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Byte);
                }
                "str" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：str 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    // char 自 v0.98.2 起纳入：编码成 1~4 字节的 UTF-8 字符串。
                    // 这样 `str(c) + "!"` 能直接拼出字符串，不必绕道 int_to_string。
                    if matches!(at, Type::Int | Type::Float | Type::Int32 | Type::Float32
                                | Type::String | Type::Char) {
                        return Ok(Type::String);
                    } else {
                        return Err(DialcError::new(
                            "语义错误：str 仅支持 int / int32 / float / float32 / char / string 参数（bool 请先用 int_to_string 转换）",
                            0, 0,
                        ));
                    }
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
                "new_set" => {
                    if !args.is_empty() {
                        return Err(DialcError::new(
                            "语义错误：new_set 不接受参数",
                            0, 0,
                        ));
                    }
                    // v1.00 的 set 元素类型固定为 int（见 check_type_wellformed）。
                    return Ok(Type::Set(Box::new(Type::Int)));
                }
                "new_kv_map" => {
                    // 两个 kind 实参由 parse_var_decl 从声明标注补齐（源码里永远写 new_kv_map()）。
                    // 值为结构体时（v1.04 B6）补第三个实参（结构体名）——kind 编码带不出名字。
                    // 出现在别处（0 参数）时明确拒绝：泛型 map 必须有静态类型标注。
                    if args.len() != 2 && args.len() != 3 {
                        return Err(DialcError::new(
                            "语义错误：new_kv_map() 只能在带 map<K,V> 类型标注的 var 声明中使用",
                            0, 0,
                        ));
                    }
                    let kk = match &args[0] {
                        Expr::IntLit(n) => *n,
                        _ => return Err(DialcError::new(
                            "内部错误：new_kv_map 的键 kind 不是编译期常量", 0, 0,
                        )),
                    };
                    let vk = match &args[1] {
                        Expr::IntLit(n) => *n,
                        _ => return Err(DialcError::new(
                            "内部错误：new_kv_map 的值 kind 不是编译期常量", 0, 0,
                        )),
                    };
                    let kt = kv_type_from_kind(kk)?;
                    let vt = if args.len() == 3 {
                        match &args[2] {
                            Expr::StringLit(sn) => Type::Struct(sn.clone()),
                            _ => return Err(DialcError::new(
                                "内部错误：new_kv_map 的结构体名实参不是字符串字面量", 0, 0,
                            )),
                        }
                    } else {
                        kv_type_from_kind(vk)?
                    };
                    // 结构体值：kind 必须是 KV_STRUCT(6)，且结构体须已定义、字段全为标量。
                    if let Type::Struct(ref sn) = vt {
                        if vk != 6 {
                            return Err(DialcError::new(
                                "内部错误：new_kv_map 的结构体值 kind 与 KV_STRUCT 不符", 0, 0,
                            ));
                        }
                        if !structs.contains_key(sn) {
                            return Err(DialcError::new(
                                format!("语义错误：map 的值类型结构体 '{}' 未定义", sn),
                                0, 0,
                            ));
                        }
                        if !struct_has_only_scalar_fields(sn, structs) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：结构体 '{}' 含句柄字段，暂不能作 map 值（装箱需字段级 ARC）",
                                    sn
                                ),
                                0, 0,
                            ));
                        }
                    }
                    return Ok(Type::KvMap(Box::new(kt), Box::new(vt)));
                }
                // bytes（二进制字节缓冲区，v0.98.2）：与 string 布局相同但类型独立，
                // 因此两者之间的转换必须**显式**走这些内置，编译器才拦得住混用。
                "bytes_from_string" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：bytes_from_string 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            "语义错误：bytes_from_string 的参数必须是 string",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Bytes);
                }
                "bytes_to_string" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：bytes_to_string 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Bytes {
                        return Err(DialcError::new(
                            "语义错误：bytes_to_string 的参数必须是 bytes",
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                // ---- v1.04 FFI：C 侧缓冲区 → Dial 对象（反向数据总线）----
                // string_from_cstr(p)：从 NUL 结尾的 C 字符串**复制**出一份新的 Dial string。
                // 与 as_c_str() 的「借用」相反：这里产生的是受 ARC 管理的新对象。
                "string_from_cstr" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：string_from_cstr 需要恰好 1 个参数（rawptr<byte>）",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if !matches!(at, Type::RawPtr(_) | Type::VolatilePtr(_)) {
                        return Err(DialcError::new(
                            format!("语义错误：string_from_cstr 的参数必须是裸指针（C 字符串地址），实际为 {}", at.name()),
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                // bytes_from_ptr(p, n)：从 C 侧缓冲区复制 n 字节构造新的 Dial bytes。
                // 长度必须显式给出（C 缓冲区可以含 \0，不能靠 strlen 猜）。
                "bytes_from_ptr" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：bytes_from_ptr 需要 2 个参数（rawptr<byte> 缓冲区, int 字节数）",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if !matches!(at, Type::RawPtr(_) | Type::VolatilePtr(_)) {
                        return Err(DialcError::new(
                            format!("语义错误：bytes_from_ptr 的第一个参数必须是裸指针，实际为 {}", at.name()),
                            0, 0,
                        ));
                    }
                    if type_of(&args[1], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：bytes_from_ptr 的第二个参数必须是 int（字节数）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Bytes);
                }
                "bytes_new" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：bytes_new 需要恰好 1 个参数（字节长度）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：bytes_new 的参数必须是 int（字节长度）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Bytes);
                }
                "bytes_length" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：bytes_length 需要恰好 1 个参数",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Bytes {
                        return Err(DialcError::new(
                            "语义错误：bytes_length 的参数必须是 bytes",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                "bytes_get" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：bytes_get 需要恰好 2 个参数（bytes, 索引）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Bytes {
                        return Err(DialcError::new(
                            "语义错误：bytes_get 的第一个参数必须是 bytes",
                            0, 0,
                        ));
                    }
                    if type_of(&args[1], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：bytes_get 的第二个参数（索引）必须是 int",
                            0, 0,
                        ));
                    }
                    // 返回 byte 而非 int：取值范围 0~255 由类型本身表达。
                    return Ok(Type::Byte);
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
                                "语义错误：spawn 的第一个参数必须是顶层具名函数（Dial 有函数指针但没有闭包，线程入口 thunk 需按目标函数静态生成；回调请用函数指针参数）",
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
                            "语义错误：len 的参数必须是 channel（数组/字符串长度请用 .length() 方法）",
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
                // ---- v1.00 内建：断言 / 退出 / 数学函数 ----
                "assert" => {
                    if args.is_empty() || args.len() > 2 {
                        return Err(DialcError::new(
                            "语义错误：assert 需要 1 或 2 个参数（bool 条件 [，string 消息]）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Bool {
                        return Err(DialcError::new(
                            "语义错误：assert 的条件必须是 bool", 0, 0,
                        ));
                    }
                    if args.len() == 2
                        && type_of(&args[1], scopes, funcs, structs)? != Type::String
                    {
                        return Err(DialcError::new(
                            "语义错误：assert 的消息必须是 string", 0, 0,
                        ));
                    }
                    return Ok(Type::Void);
                }
                "panic" => {
                    if args.len() != 1
                        || type_of(&args[0], scopes, funcs, structs)? != Type::String
                    {
                        return Err(DialcError::new(
                            "语义错误：panic 需要 1 个 string 参数", 0, 0,
                        ));
                    }
                    return Ok(Type::Void);
                }
                "exit" => {
                    if args.len() != 1
                        || type_of(&args[0], scopes, funcs, structs)? != Type::Int
                    {
                        return Err(DialcError::new(
                            "语义错误：exit 需要 1 个 int 参数（退出码）", 0, 0,
                        ));
                    }
                    return Ok(Type::Void);
                }
                // ---- v1.01 内建：环境 / 时间 / 随机 ----
                // ---- v1.02 裸指针内建 ----
                // raw_alloc(n)：申请 n 字节的裸内存，返回 rawptr<byte>。
                // **不受 ARC 管理**：编译器不会为它做任何 retain/release，
                // 必须由调用方显式 raw_free(p) 或 drop(p) 归还（谁申请、谁释放）。
                "raw_alloc" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：raw_alloc 需要 1 个 int 参数（字节数）",
                            0, 0,
                        ));
                    }
                    if type_of(&args[0], scopes, funcs, structs)? != Type::Int {
                        return Err(DialcError::new(
                            "语义错误：raw_alloc 的参数必须是 int（字节数）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::RawPtr(Box::new(Type::Byte)));
                }
                // raw_free(p)：归还裸指针。参数必须是裸指针。
                "raw_free" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：raw_free 需要 1 个参数（裸指针）",
                            0, 0,
                        ));
                    }
                    let at = type_of(&args[0], scopes, funcs, structs)?;
                    if !matches!(at, Type::RawPtr(_) | Type::VolatilePtr(_)) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：raw_free 的参数必须是裸指针，实际为 {}",
                                at.name()
                            ),
                            0, 0,
                        ));
                    }
                    return Ok(Type::Void);
                }
                // ptr_cast<T>(x)：显式的、带类型的指针转换。
                //   - 源是裸指针 → 位模式转换（不改变地址）；
                //   - 源是 string / bytes → 借用其**数据缓冲区**（handle + 8 字节头部之后）；
                //   - 源是标量 → 拒绝（不能对临时值取址）。
                // 裸指针与 ARC 容器之间**没有隐式转换**，必须显式走这里。
                "ptr_cast" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：ptr_cast 需要类型实参与源表达式，形如 ptr_cast<rawptr<byte>>(x)",
                            0, 0,
                        ));
                    }
                    let target = match &args[0] {
                        Expr::TypeLit(ty) => ty.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：ptr_cast 的第一个参数必须是类型实参（如 ptr_cast<rawptr<byte>>(x)）",
                                0, 0,
                            ))
                        }
                    };
                    let target_elem = match &target {
                        Type::RawPtr(e) | Type::VolatilePtr(e) => e.clone(),
                        other => {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：ptr_cast 的目标类型必须是裸指针，实际为 {}",
                                    other.name()
                                ),
                                0, 0,
                            ))
                        }
                    };
                    // v1.04：目标若是易失裸指针，结果**保持易失性**——
                    // 「转成 volatile 指针」正是把一段普通内存切进硬件语义的那一步，
                    // 若在此处抹掉 volatile，用户写的 volatile 意图就被编译器悄悄丢了。
                    let result_ptr = match &target {
                        Type::VolatilePtr(e) => Type::VolatilePtr(e.clone()),
                        _ => Type::RawPtr(target_elem.clone()),
                    };
                    let src = type_of(&args[1], scopes, funcs, structs)?;
                    match &src {
                        // 易失裸指针也可作为源（转成普通裸指针是用户显式选择——
                        // 抹掉易失性是允许的，只是他自担后果）。
                        Type::RawPtr(_) | Type::VolatilePtr(_) | Type::String | Type::Bytes => {}
                        other => {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：ptr_cast 的源必须是裸指针或 string / bytes，实际为 {}",
                                    other.name()
                                ),
                                0, 0,
                            ))
                        }
                    }
                    return Ok(result_ptr);
                }
                // bitcast<T>(x)：底层内存强转（v1.04 FFI）。把值 `x` 的**位模式**按目标类型
                // `T` 重新解释——不做任何转换、不读不改内存，纯粹是 LLVM `bitcast`
                // （C 的 `*(T*)&x` reinterpret_cast）。典型用途：整数地址强转成裸指针、
                // 裸指针强转成整数、把一块内存按另一种同尺寸布局解读。
                // 大小约束（源与目标必须同位宽）在 codegen 处用 target DataLayout 核对，
                // 这里只做类型层面的可行性放行。
                "bitcast" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：bitcast 需要类型实参与源表达式，形如 bitcast<int>(x)",
                            0, 0,
                        ));
                    }
                    let target = match &args[0] {
                        Expr::TypeLit(ty) => ty.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：bitcast 的第一个参数必须是类型实参（如 bitcast<rawptr<byte>>(x)）",
                                0, 0,
                            ))
                        }
                    };
                    if matches!(target, Type::Void | Type::Error) {
                        return Err(DialcError::new(
                            format!("语义错误：bitcast 的目标类型不能是 {}（无意义）", target.name()),
                            0, 0,
                        ));
                    }
                    let src = type_of(&args[1], scopes, funcs, structs)?;
                    if matches!(src, Type::Void | Type::Error) {
                        return Err(DialcError::new(
                            format!("语义错误：bitcast 的源不能是 {} 类型", src.name()),
                            0, 0,
                        ));
                    }
                    return Ok(target);
                }
                // ---- v1.04 FFI：编译期类型内建（sizeof / alignof / offsetof）----
                // 全部返回 int，具体数值由 codegen 按 target DataLayout 在编译期算出，
                // 不发射任何运行时指令（零开销）。主要用于手写 C 库绑定时核对内存布局。
                "sizeof" | "alignof" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：{} 需要 1 个类型实参，形如 {}(T)",
                                name, name
                            ),
                            0, 0,
                        ));
                    }
                    let ty = match &args[0] {
                        Expr::TypeLit(t) => t.clone(),
                        _ => {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：{} 的参数必须是类型实参，形如 {}(int)",
                                    name, name
                                ),
                                0, 0,
                            ))
                        }
                    };
                    // void 在 C 里 sizeof 未定义；Dial 的 Void 内部是 i64，但明确地禁止，
                    // 避免用户写出依赖实现细节的脆弱代码。
                    if matches!(ty, Type::Void) {
                        return Err(DialcError::new(
                            format!("语义错误：{} 的参数不能是 void", name),
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                "offsetof" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            "语义错误：offsetof 需要 2 个参数（类型实参, 字段名），形如 offsetof<Point>(x)",
                            0, 0,
                        ));
                    }
                    let (ty, field) = match (&args[0], &args[1]) {
                        (Expr::TypeLit(t), Expr::StringLit(f)) => (t.clone(), f.clone()),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：offsetof 的参数必须是（类型实参, 字段名），形如 offsetof<Point>(x)",
                                0, 0,
                            ))
                        }
                    };
                    let sname = match &ty {
                        Type::Struct(n) => n.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：offsetof 的第一个参数必须是结构体类型（含 extern struct）",
                                0, 0,
                            ))
                        }
                    };
                    match structs.get(&sname) {
                        Some((fields, _)) => {
                            if !fields.iter().any(|(fnm, _)| fnm == &field) {
                                return Err(DialcError::new(
                                    format!(
                                        "语义错误：结构体 '{}' 没有字段 '{}'",
                                        sname, field
                                    ),
                                    0, 0,
                                ));
                            }
                        }
                        None => {
                            return Err(DialcError::new(
                                format!("语义错误：offsetof 的结构体 '{}' 未定义", sname),
                                0, 0,
                            ))
                        }
                    }
                    return Ok(Type::Int);
                }
                "args" => {
                    if !args.is_empty() {
                        return Err(DialcError::new("语义错误：args() 不接受参数", 0, 0));
                    }
                    // 返回 string[]：命令行参数列表（argv[1..]）。
                    return Ok(Type::Array(Box::new(Type::String)));
                }
                "now" => {
                    if !args.is_empty() {
                        return Err(DialcError::new("语义错误：now() 不接受参数", 0, 0));
                    }
                    // 返回 float：自 Unix 纪元起的秒数（含小数部分）。
                    return Ok(Type::Float);
                }
                "random" => {
                    if !args.is_empty() {
                        return Err(DialcError::new("语义错误：random() 不接受参数", 0, 0));
                    }
                    // 返回 int：[0, 2^63) 的伪随机整数。
                    return Ok(Type::Int);
                }
                "sleep" => {
                    if args.len() != 1
                        || type_of(&args[0], scopes, funcs, structs)? != Type::Int
                    {
                        return Err(DialcError::new(
                            "语义错误：sleep 需要 1 个 int 参数（毫秒）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Void);
                }
                // ---- v1.04 第 2 批 C2：严格数值解析 / 显式探测 ----
                // 无 Option/Result，非法输入由 C 侧 Fail-Fast；is_* 是显式、无副作用的探测。
                "parse_int" | "is_int" | "parse_float" | "is_float" => {
                    if args.len() != 1 || type_of(&args[0], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个 string 参数", name),
                            0, 0,
                        ));
                    }
                    return Ok(match name.as_str() {
                        "parse_int" => Type::Int,
                        "parse_float" => Type::Float,
                        _ => Type::Bool,   // is_int / is_float
                    });
                }
                // ---- v1.04 第 2 批 C3：类型安全的定点格式化 ----
                // 刻意不提供 printf 式变参/运行期格式串（静态强类型 + 零隐式转换）。
                "format_float" => {
                    if args.len() != 2
                        || type_of(&args[0], scopes, funcs, structs)? != Type::Float
                        || type_of(&args[1], scopes, funcs, structs)? != Type::Int
                    {
                        return Err(DialcError::new(
                            "语义错误：format_float(v, digits) 需要 1 个 float 与 1 个 int（小数位数 0~9）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                // ---- v1.04 第 2 批 C7：文件 API（命名避开同名重载）----
                "read_all" | "eof" | "file_read_line" => {
                    if args.len() != 1 || type_of(&args[0], scopes, funcs, structs)? != Type::File {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个 file 参数", name),
                            0, 0,
                        ));
                    }
                    return Ok(if name == "eof" { Type::Bool } else { Type::String });
                }
                "file_exists" | "file_size" | "file_remove" => {
                    if args.len() != 1 || type_of(&args[0], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个 string 路径参数", name),
                            0, 0,
                        ));
                    }
                    return Ok(match name.as_str() {
                        "file_exists" | "file_remove" => Type::Bool,
                        _ => Type::Int,   // file_size（打不开返回 -1）
                    });
                }
                // ---- v1.04 第 2 批 C9：环境只读查询（无 setenv）----
                "getenv" | "has_env" => {
                    if args.len() != 1 || type_of(&args[0], scopes, funcs, structs)? != Type::String {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个 string 变量名参数", name),
                            0, 0,
                        ));
                    }
                    return Ok(if name == "has_env" { Type::Bool } else { Type::String });
                }
                "program_name" | "cwd" | "date_string" => {
                    if !args.is_empty() {
                        return Err(DialcError::new(
                            format!("语义错误：{}() 不接受参数", name),
                            0, 0,
                        ));
                    }
                    return Ok(Type::String);
                }
                // ---- v1.04 第 2 批 C10：本地时间 / 单调计时 ----
                "local_parts" => {
                    if !args.is_empty() {
                        return Err(DialcError::new(
                            "语义错误：local_parts() 不接受参数（返回 8 个分量的 int[]）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Array(Box::new(Type::Int)));
                }
                "monotonic_ms" => {
                    if !args.is_empty() {
                        return Err(DialcError::new(
                            "语义错误：monotonic_ms() 不接受参数（返回单调毫秒 int）",
                            0, 0,
                        ));
                    }
                    return Ok(Type::Int);
                }
                "min" | "max" => {
                    if args.len() != 2 {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 2 个参数", name), 0, 0,
                        ));
                    }
                    let a = type_of(&args[0], scopes, funcs, structs)?;
                    let b = type_of(&args[1], scopes, funcs, structs)?;
                    if a != b || (a != Type::Int && a != Type::Float) {
                        return Err(DialcError::new(
                            "语义错误：min / max 要求两个同类型参数，且只能是 int 或 float",
                            0, 0,
                        ));
                    }
                    return Ok(a);
                }
                "abs" => {
                    if args.len() != 1 {
                        return Err(DialcError::new(
                            "语义错误：abs 需要 1 个参数", 0, 0,
                        ));
                    }
                    let a = type_of(&args[0], scopes, funcs, structs)?;
                    if a != Type::Int && a != Type::Float {
                        return Err(DialcError::new(
                            "语义错误：abs 的参数必须是 int 或 float", 0, 0,
                        ));
                    }
                    return Ok(a);
                }
                "pow" => {
                    if args.len() != 2
                        || type_of(&args[0], scopes, funcs, structs)? != Type::Float
                        || type_of(&args[1], scopes, funcs, structs)? != Type::Float
                    {
                        return Err(DialcError::new(
                            "语义错误：pow 需要 2 个 float 参数", 0, 0,
                        ));
                    }
                    return Ok(Type::Float);
                }
                "sqrt" | "floor" | "ceil" | "round" => {
                    if args.len() != 1
                        || type_of(&args[0], scopes, funcs, structs)? != Type::Float
                    {
                        return Err(DialcError::new(
                            format!("语义错误：{} 需要 1 个 float 参数", name), 0, 0,
                        ));
                    }
                    return Ok(Type::Float);
                }
                _ => {}
            }
            // v1.04 C8：eprint 与 print 参数规则完全一致（连诊断文案都复用），
            // 唯一差别是输出目标 stderr。这里按名字并列识别，不新增一套校验逻辑。
            if name == "print" || name == "eprint" {
                if args.is_empty() {
                    return Err(DialcError::new(
                        format!("语义错误：内置函数 {} 至少需要 1 个参数", name),
                        0, 0,
                    ));
                }
                // variadic（v1.00）：逐个校验实参类型，全部可输出才放行。
                for a in args {
                let pt = type_of(a, scopes, funcs, structs)?;
                match pt {
                    // char 打印的是字符本身（不是码点数字），与 int 明确区分。
                    // byte 打印的是**数值**（0~255），也不是字符。
                    Type::Int | Type::Float | Type::Int32 | Type::Float32 | Type::Bool | Type::Char | Type::Byte | Type::String => {}
                    Type::RawPtr(_) | Type::VolatilePtr(_) => {
                        return Err(DialcError::new(
                            "语义错误：不能直接 print 裸指针（它只是一个地址，没有可打印的内容）",
                            0, 0,
                        ))
                    }
                    // 函数指针同样是纯地址：打印它只会输出一个无意义的代码地址。
                    Type::FnPtr(..) => {
                        return Err(DialcError::new(
                            "语义错误：不能直接 print 函数指针（它只是代码地址，没有可打印的内容）",
                            0, 0,
                        ))
                    }
                    // 向量是寄存器里的多值聚合，没有标量打印通道。
                    Type::Simd(elem, n) => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：simd<{}, {}> 不能直接 print（多个 lane 没有单一可读形态）；\
                                 请用 v[i] 取出单个元素再打印",
                                elem.display(), n
                            ),
                            0, 0,
                        ))
                    }
                    Type::Array(_) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print 数组，请逐元素打印 print(a.get(i))",
                            0, 0,
                        ))
                    }
                    // bytes 是**二进制**数据，直接打印会把原始字节当字符吐出乱码——
                    // 与 print(int[]) 同理，在编译期拒绝。
                    // 想看内容请显式转换：bytes_to_string(b)（可能是非法 UTF-8，需自行处理）。
                    Type::Bytes => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print bytes（二进制数据）；\
                             请先显式转换（bytes_to_string）再打印",
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
                    Type::Set(_) | Type::KvMap(..) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print 集合 / 泛型 map，请先用 values() 取快照再逐元素打印",
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
                    // 包提供的不透明句柄：与 thread / channel 同理，打印没有可读语义。
                    Type::Opaque(n) => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：当前版本不支持直接 print 包句柄 '{}'（不透明指针）；\
                                 请改用该包提供的取值函数",
                                n
                            ),
                            0,
                            0,
                        ))
                    }
                    Type::Void => {
                        return Err(DialcError::new(
                            "语义错误：print 的参数不能是 void",
                            0, 0,
                        ))
                    }
                }
                } // for a in args（variadic）
                // print 是**语句**而非表达式：无返回值，因此不能出现在
                // 赋值右侧、return 或任何需要值的位置。
                Ok(Type::Void)
            } else if name == "print_raw" || name == "eprint_raw" {
                // 原样输出、**不换行**（v0.98.2）：连续输出的基本积木。
                // 与 print 的区别只有「结尾那个 '\n'」，参数类型要求则完全一致——
                // 两者都接受标量与字符串，不接受数组/结构体/句柄（那些没有可读形态）。
                // v1.04 C8：eprint_raw 是 stderr 版本，规则完全复用。
                if args.is_empty() {
                    return Err(DialcError::new(
                        format!("语义错误：内置函数 {} 至少需要 1 个参数", name),
                        0, 0,
                    ));
                }
                for a in args {
                let pt = type_of(a, scopes, funcs, structs)?;
                match pt {
                    Type::Int | Type::Float | Type::Int32 | Type::Float32 | Type::Bool | Type::Char | Type::Byte | Type::String => {}
                    Type::RawPtr(_) | Type::VolatilePtr(_) => {
                        return Err(DialcError::new(
                            "语义错误：不能直接 print_raw 裸指针（它只是一个地址，没有可输出的内容）",
                            0, 0,
                        ))
                    }
                    Type::FnPtr(..) => {
                        return Err(DialcError::new(
                            "语义错误：不能直接 print_raw 函数指针（它只是代码地址，没有可输出的内容）",
                            0, 0,
                        ))
                    }
                    Type::Simd(elem, n) => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：simd<{}, {}> 不能直接 print_raw；请用 v[i] 取出单个元素再输出",
                                elem.display(), n
                            ),
                            0, 0,
                        ))
                    }
                    Type::Array(_) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw 数组，请逐元素输出 print_raw(str(a.get(i)))",
                            0, 0,
                        ))
                    }
                    // bytes 是二进制数据，原样输出会吐出乱码——与 print_raw(int[]) 同理拒绝。
                    Type::Bytes => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw bytes（二进制数据）；\
                             请先显式转换（bytes_to_string）再输出",
                            0, 0,
                        ))
                    }
                    Type::Struct(_) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw 结构体，请逐字段输出",
                            0, 0,
                        ))
                    }
                    Type::Map => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw 哈希表，请逐值输出",
                            0, 0,
                        ))
                    }
                    Type::Set(_) | Type::KvMap(..) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw 集合 / 泛型 map，请先用 values() 取快照再逐元素输出",
                            0, 0,
                        ))
                    }
                    Type::File => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw 文件句柄（file）；请改用 read 读成 string 后再输出",
                            0, 0,
                        ))
                    }
                    Type::Error => {}
                    Type::Thread => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw thread 句柄",
                            0, 0,
                        ))
                    }
                    Type::Channel(_) => {
                        return Err(DialcError::new(
                            "语义错误：当前版本不支持直接 print_raw channel；请 recv 出元素后再输出",
                            0, 0,
                        ))
                    }
                    Type::Opaque(n) => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：当前版本不支持直接 print_raw 包句柄 '{}'（不透明指针）；\
                                 请改用该包提供的取值函数",
                                n
                            ),
                            0,
                            0,
                        ))
                    }
                    Type::Void => {
                        return Err(DialcError::new(
                            "语义错误：print_raw 的参数不能是 void",
                            0, 0,
                        ))
                    }
                }
                } // for a in args（variadic）
                // 与 print 同为语句：无返回值。
                Ok(Type::Void)
            } else if name == "write_char" {
                // 输出单个字符、**不换行**（v0.98.2）：与 print(char) 的唯一差别就是换行。
                if args.len() != 1 {
                    return Err(DialcError::new(
                        "语义错误：内置函数 write_char 需要恰好 1 个参数",
                        0, 0,
                    ));
                }
                let pt = type_of(&args[0], scopes, funcs, structs)?;
                if pt != Type::Char {
                    return Err(DialcError::new(
                        "语义错误：write_char 的参数必须是 char（int 请先用 int_to_char 转换）",
                        0, 0,
                    ));
                }
                Ok(Type::Void)
            } else {
                let info = match funcs.get(name) {
                    Some(i) => i,
                    None => {
                        // v1.03：名字不是函数，但可能是一个持有函数指针的变量 —— 间接调用。
                        // 签名严格匹配（零隐式转换铁律），不检查任何「捕获环境」。
                        if let Some(Type::FnPtr(ps, ret)) = lookup(name) {
                            if args.len() != ps.len() {
                                return Err(DialcError::new(
                                    format!(
                                        "语义错误：函数指针 '{}' 需要 {} 个参数，但传入了 {} 个",
                                        name,
                                        ps.len(),
                                        args.len()
                                    ),
                                    0, 0,
                                ));
                            }
                            for (i, (a, pt)) in args.iter().zip(ps.iter()).enumerate() {
                                let at = type_of(a, scopes, funcs, structs)?;
                                if !types_compatible(&at, pt) {
                                    return Err(DialcError::new(
                                        format!(
                                            "语义错误：函数指针 '{}' 第 {} 个参数类型不匹配（实参 {}，形参 {}）",
                                            name,
                                            i + 1,
                                            at.display(),
                                            pt.display()
                                        ),
                                        0, 0,
                                    ));
                                }
                            }
                            return Ok(*ret);
                        }
                        return Err(DialcError::new(
                            format!("语义错误：调用了未定义的函数 '{}'", name),
                            0, 0,
                        ));
                    }
                };
                // C 风格异构变参（v1.03）：**仅 `.diali` 的 extern fn** 走这里。
                // 固定参数照常严格检查；尾部参数不做类型检查（FFI 契约由调用方负责），
                // 但仍必须是合法表达式——void / 语句不能被塞进 varargs。
                if info.variadic {
                    if args.len() < info.params.len() {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：C 变参外部函数 '{}' 至少需要 {} 个固定参数，但传入了 {} 个",
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
                                    "语义错误：C 变参外部函数 '{}' 第 1 个固定参数类型不匹配（实参 {}，形参 {}）",
                                    name,
                                    at.display(),
                                    pt.display()
                                ),
                                0, 0,
                            ));
                        }
                    }
                    for a in args.iter().skip(info.params.len()) {
                        let at = type_of(a, scopes, funcs, structs)?;
                        if at == Type::Void || matches!(at, Type::FnPtr(..)) {
                            return Err(DialcError::new(
                                "语义错误：C 变参的尾部实参不能是 void 表达式或函数指针（请显式转换成值）",
                                0, 0,
                            ));
                        }
                    }
                    return Ok(info.ret.clone());
                }
                // 默认实参（v1.00）：实参个数允许落在 [必填数, 形参总数] 区间。
                // 必填数 = 第一个带默认值形参的下标；默认值从末尾连续（parser 已强制）。
                let required = info
                    .defaults
                    .iter()
                    .position(|d| d.is_some())
                    .unwrap_or(info.params.len());
                if args.len() < required || args.len() > info.params.len() {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：函数 '{}' 需要 {}~{} 个参数，但传入了 {} 个",
                            name, required, info.params.len(), args.len()
                        ),
                        0, 0,
                    ));
                }
                for (i, (a, pt)) in args.iter().zip(info.params.iter()).enumerate() {
                    let at = type_of(a, scopes, funcs, structs)?;
                    if !types_compatible(&at, pt) {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：函数 '{}' 第 {} 个参数类型不匹配（实参 {}，形参 {}）",
                                name,
                                i + 1,
                                at.display(),
                                pt.display()
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
        "index_of" => {
            if args.len() != 1 || arg_t(0)? != Type::String {
                return Err(DialcError::new(
                    "语义错误：string.index_of(sub) 需要 1 个 string 参数，未找到返回 -1",
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        "replace" => {
            if args.len() != 2 || arg_t(0)? != Type::String || arg_t(1)? != Type::String {
                return Err(DialcError::new(
                    "语义错误：string.replace(from, to) 需要 2 个 string 参数，返回新字符串",
                    0, 0,
                ));
            }
            Ok(Type::String)
        }
        "trim" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：string.trim() 不接受参数", 0, 0));
            }
            Ok(Type::String)
        }
        "split" => {
            if args.len() != 1 || arg_t(0)? != Type::String {
                return Err(DialcError::new(
                    "语义错误：string.split(sep) 需要 1 个 string 参数，返回 string[]",
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(Type::String)))
        }
        // ---- v1.04 第 2 批 C1：字符串查询 / 规范化 ----
        // to_upper/to_lower 只变换 ASCII，UTF-8 多字节序列原样保留（见 dial_string.c）。
        "to_upper" | "to_lower" => {
            if !args.is_empty() {
                return Err(DialcError::new(
                    format!("语义错误：string.{}() 不接受参数", method),
                    0, 0,
                ));
            }
            Ok(Type::String)
        }
        "contains" | "starts_with" | "ends_with" => {
            if args.len() != 1 || arg_t(0)? != Type::String {
                return Err(DialcError::new(
                    format!("语义错误：string.{}(s) 需要 1 个 string 参数，返回 bool", method),
                    0, 0,
                ));
            }
            Ok(Type::Bool)
        }
        // ---- v1.04 第 2 批 C4：UTF-8 码点视图 ----
        // 索引单位是**码点**而非字节；length()/get()/substring() 的字节语义保持不变。
        "char_count" => {
            if !args.is_empty() {
                return Err(DialcError::new(
                    "语义错误：string.char_count() 不接受参数（返回 UTF-8 码点数）",
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        "chars" => {
            if !args.is_empty() {
                return Err(DialcError::new(
                    "语义错误：string.chars() 不接受参数（返回码点 int[]）",
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(Type::Int)))
        }
        "char_at" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：string.char_at(i) 需要 1 个 int 参数（码点索引，返回码点）",
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        "char_slice" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || arg_t(1)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：string.char_slice(from, to) 需要 2 个 int 参数（码点半开区间）",
                    0, 0,
                ));
            }
            Ok(Type::String)
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
/// B8：编译期常量判定——结构体字段默认值必须是字面量（含字面量数组），
/// 不得引用变量 / 函数调用等运行时值（宪法：禁止隐式未初始化内存，默认值亦须确定）。
fn is_ct_const(e: &Expr) -> bool {
    match e {
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::BoolLit(_)
        | Expr::CharLit(_)
        | Expr::ByteLit(_)
        | Expr::StringLit(_) => true,
        Expr::ArrayLit(es, _) => es.iter().all(is_ct_const),
        // 编译期常量表达式：字面量上的算术/逻辑/位运算仍可常量折叠。
        Expr::Unary(_, inner) => is_ct_const(inner),
        Expr::Binary(a, _, b) => is_ct_const(a) && is_ct_const(b),
        _ => false,
    }
}

/// 可作为动态数组元素的类型：标量（int/float/bool/byte/char）或任意 8 字节句柄/指针，
/// 外加 v1.04 B6 引入的**结构体**（值类型）。
/// 结构体按值装不进 8 字节槽，故先装箱为 RC 对象（指针）后入槽；
/// 本期只放开「字段全为标量」的结构体——含句柄字段的结构体装箱还需要字段级 ARC
/// （装箱 / 释放时逐字段 retain/release），尚未落地，故在语义层明确拒绝而非静默出错。
fn is_array_elem_type(t: &Type, structs: &StructTable) -> bool {
    match t {
        Type::Int | Type::Float | Type::Bool | Type::Byte | Type::Char
            | Type::String
            | Type::Bytes
            | Type::Array(_)
            | Type::Map | Type::Set(_) | Type::KvMap(..)
            | Type::Error
            | Type::Channel(_)
            | Type::Opaque(_) => true,
        Type::Struct(name) => struct_has_only_scalar_fields(name, structs),
        _ => false,
    }
}

/// 结构体字段是否全为标量（不含句柄）。
///
/// 这是 B6「结构体进容器」的现役边界：装箱只拷贝位模式，标量字段无需额外 retain/release；
/// 一旦含句柄字段，容器释放装箱对象时必须逐字段释放，否则会泄漏或悬垂——
/// 那需要字段级 ARC（本期未落地），故宁可拒绝，也不让它「看起来能用」。
fn struct_has_only_scalar_fields(name: &str, structs: &StructTable) -> bool {
    match structs.get(name) {
        Some((fields, _)) => fields
            .iter()
            .all(|(_, ft)| matches!(ft, Type::Int | Type::Float | Type::Bool | Type::Byte | Type::Char)),
        // 未知结构体：不算通过（保守拒绝，由后续查表给出更具体的诊断）
        None => false,
    }
}

/// 裸指针的方法（v1.02）：目前只有 `offset(n)`——**显式、带类型**的指针算术。
///
/// 绝不允许 C 式「void* + 1」的无类型偏移：元素大小必须由元素类型决定，
/// 否则同一个偏移在不同类型上含义不同，是悬垂与越界的常见来源。
fn rawptr_method_type<V>(
    elem: &Type,
    method: &str,
    args: &[Expr],
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    match method {
        "offset" => {
            if args.len() != 1 || type_of(&args[0], scopes, funcs, structs)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：裸指针的 offset() 需要 1 个 int 参数（元素个数偏移）",
                    0,
                    0,
                ));
            }
            // rawptr<void> 没有元素大小，算不出偏移——强制先 ptr_cast 成具体类型。
            if *elem == Type::Void {
                return Err(DialcError::new(
                    "语义错误：rawptr<void> 是无类型裸指针，不能做指针算术（请先 ptr_cast 成具体类型）",
                    0, 0,
                ));
            }
            Ok(Type::RawPtr(Box::new(elem.clone())))
        }
        _ => Err(DialcError::new(
            format!("语义错误：裸指针无方法 '{}'", method),
            0, 0,
        )),
    }
}

fn array_method_type<V>(
    elem: &Type,
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
            if args.len() != 1 || !types_compatible(&arg_t(0)?, elem) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：array.push() 需要 1 个 {} 类型参数（元素类型即数组的元素类型）",
                        elem.name()
                    ),
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(elem.clone())))
        }
        "get" => {
            if args.len() != 1 || arg_t(0)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：array.get() 需要 1 个 int 参数",
                    0, 0,
                ));
            }
            Ok(elem.clone())
        }
        "set" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || !types_compatible(&arg_t(1)?, elem) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：array.set(i, v) 需要 int 索引与 {} 值 v",
                        elem.name()
                    ),
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(elem.clone())))
        }
        "clone" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：array.clone() 不接受参数", 0, 0));
            }
            Ok(Type::Array(Box::new(elem.clone())))
        }
        // v1.04 B11：`a.slice(from, to)` 与切片语法 `a[from..to]` 完全等价，
        // 只是补上一个更符合直觉的方法名（此前能力在、方法名不成立，属「要绕路」）。
        // 半开区间 [from, to)，返回**新数组**，不改变接收者，因此无需写回变量槽。
        "slice" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || arg_t(1)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：array.slice(from, to) 需要 2 个 int 参数（半开区间）",
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(elem.clone())))
        }
        // ---- v1.04 第 2 批 C5：数组 API 扩充 ----
        // insert/reverse/clear 返回（可能经 COW 克隆的）新数组指针，由 codegen 写回接收变量。
        "insert" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || !types_compatible(&arg_t(1)?, elem) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：array.insert(i, v) 需要 int 索引与 {} 值 v（允许 i == length 追加）",
                        elem.name()
                    ),
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(elem.clone())))
        }
        "reverse" | "clear" => {
            if !args.is_empty() {
                return Err(DialcError::new(
                    format!("语义错误：array.{}() 不接受参数", method),
                    0, 0,
                ));
            }
            Ok(Type::Array(Box::new(elem.clone())))
        }
        // index_of 只对标量开放：string 等句柄元素直接比 8 字节指针位模式是静默坑，
        // 请改用循环 + eq()（零隐式转换，语义明确）。
        "index_of" => {
            if args.len() != 1 || !types_compatible(&arg_t(0)?, elem) {
                return Err(DialcError::new(
                    format!("语义错误：array.index_of(v) 需要 1 个 {} 参数（返回下标，未找到 -1）", elem.name()),
                    0, 0,
                ));
            }
            if !matches!(elem, Type::Int | Type::Float | Type::Bool | Type::Byte | Type::Char) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：array<{}>.index_of() 只支持标量元素（避免比较句柄指针位模式）；请写循环用 eq() 逐个比较",
                        elem.name()
                    ),
                    0, 0,
                ));
            }
            Ok(Type::Int)
        }
        // join 只对 string[] 开放：按 sep 拼接成新 string。
        "join" => {
            if *elem != Type::String {
                return Err(DialcError::new(
                    format!(
                        "语义错误：array<{}>.join() 只支持 string[]；标量数组请先逐元素 str() 再拼接",
                        elem.name()
                    ),
                    0, 0,
                ));
            }
            if args.len() != 1 || arg_t(0)? != Type::String {
                return Err(DialcError::new(
                    "语义错误：string[].join(sep) 需要 1 个 string 分隔符参数",
                    0, 0,
                ));
            }
            Ok(Type::String)
        }
        // reduce：回调 fn(acc: T, x: T, state: rawptr<void>): T，初值与返回值都是标量 T。
        // 复用 v1.03 高阶方法的函数指针机制——无闭包，状态显式经 rawptr<void> 透传。
        "reduce" => {
            if !matches!(elem, Type::Int | Type::Float | Type::Bool | Type::Byte | Type::Char) {
                return Err(DialcError::new(
                    "语义错误：array.reduce() 只支持标量元素数组（int/float/bool/byte/char）",
                    0, 0,
                ));
            }
            if args.len() < 2 || args.len() > 3 {
                return Err(DialcError::new(
                    "语义错误：array.reduce(fn, init) 需要 1 个回调与 1 个初值，外加可选的 1 个 rawptr<void> 状态参数",
                    0, 0,
                ));
            }
            let state_ty = Type::RawPtr(Box::new(Type::Void));
            let (ps, ret) = match arg_t(0)? {
                Type::FnPtr(ps, r) => (ps, r),
                _ => return Err(DialcError::new(
                    "语义错误：array.reduce() 的第一个参数必须是函数指针（Dial 没有闭包，具名函数可直接当值用）",
                    0, 0,
                )),
            };
            let expect_ps = vec![elem.clone(), elem.clone(), state_ty.clone()];
            if ps != expect_ps || *ret != *elem {
                return Err(DialcError::new(
                    format!(
                        "语义错误：array.reduce() 回调签名不匹配：期望 fn({}, {}, rawptr<void>): {}，实际 {}",
                        elem.name(), elem.name(), elem.name(),
                        Type::FnPtr(ps.clone(), ret.clone()).display()
                    ),
                    0, 0,
                ));
            }
            if !types_compatible(&arg_t(1)?, elem) {
                return Err(DialcError::new(
                    format!("语义错误：array.reduce() 的初值必须是 {} 类型", elem.name()),
                    0, 0,
                ));
            }
            if args.len() == 3 && arg_t(2)? != state_ty {
                return Err(DialcError::new(
                    "语义错误：array.reduce() 的状态参数必须是 rawptr<void>（裸指针生命周期由用户负责）",
                    0, 0,
                ));
            }
            Ok(elem.clone())
        }
        // remove(i) / pop()：返回被删（弹出）的元素，而非数组本身。
            // 数组自身的更新（COW 可能替换指针）由 codegen 在接收变量处写回，不影响返回类型。
            "remove" => {
                if args.len() != 1 || arg_t(0)? != Type::Int {
                    return Err(DialcError::new("语义错误：array.remove() 需要 1 个 int 参数（下标）", 0, 0));
                }
                Ok(elem.clone())
            }
            "pop" => {
                if !args.is_empty() {
                    return Err(DialcError::new("语义错误：array.pop() 不接受参数", 0, 0));
                }
                Ok(elem.clone())
            }
            "sort" => {
                if !args.is_empty() {
                    return Err(DialcError::new("语义错误：array.sort() 不接受参数", 0, 0));
                }
                // v1.04 第 2 批 C5：string[] 也放行——按 UTF-8 字节字典序排序
                // （走 dial_array_sort_string，比较的是内容而非指针位模式）。
                // 其余句柄元素仍无自然序，拒绝。
                if *elem != Type::String && matches!(
                    elem,
                    Type::Bytes
                        | Type::Array(_)
                        | Type::Map
                        | Type::Set(_)
                        | Type::KvMap(..)
                        | Type::Error
                        | Type::Channel(_)
                        | Type::Opaque(_)
                ) {
                    return Err(DialcError::new(
                        "语义错误：array.sort() 只支持标量元素类型（int/float/byte/char/bool）或 string（字典序）",
                        0, 0,
                    ));
                }
                // 返回（可能经 COW 克隆的）新数组指针，由 codegen 写回接收变量。
                Ok(Type::Array(Box::new(elem.clone())))
            }
            // v1.03：接受函数指针的高阶数组方法。
            // 回调签名固定带一个 `rawptr<void>` 状态参数——Dial 没有闭包，
            // 状态只能由调用方显式穿过 C 运行时再交回回调，绝不是隐式捕获。
            "map" | "filter" | "sort_by" => {
                if !matches!(
                    elem,
                    Type::Int | Type::Float | Type::Bool | Type::Byte | Type::Char
                ) {
                    return Err(DialcError::new(
                        "语义错误：map/filter/sort_by 只支持标量元素数组（int/float/bool/byte/char）；句柄元素请自行写循环",
                        0, 0,
                    ));
                }
                if args.is_empty() || args.len() > 2 {
                    return Err(DialcError::new(
                        "语义错误：map/filter/sort_by 需要 1 个回调参数，外加可选的 1 个 rawptr<void> 状态参数",
                        0, 0,
                    ));
                }
                let state_ty = Type::RawPtr(Box::new(Type::Void));
                let (ps, ret) = match arg_t(0)? {
                    Type::FnPtr(ps, r) => (ps, r),
                    _ => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：array.{}() 的第一个参数必须是函数指针（Dial 没有闭包，具名函数可直接当值用）",
                                method
                            ),
                            0, 0,
                        ))
                    }
                };
                let expect_ps: Vec<Type> = if method == "sort_by" {
                    vec![elem.clone(), elem.clone(), state_ty.clone()]
                } else {
                    vec![elem.clone(), state_ty.clone()]
                };
                if ps != expect_ps {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：array.{}() 的回调签名不匹配：期望 fn({}): ...，实际 {}（状态须用 rawptr<void> 显式传递）",
                            method,
                            expect_ps
                                .iter()
                                .map(|t| t.display())
                                .collect::<Vec<_>>()
                                .join(", "),
                            Type::FnPtr(ps.clone(), ret.clone()).display()
                        ),
                        0, 0,
                    ));
                }
                if args.len() == 2 && arg_t(1)? != state_ty {
                    return Err(DialcError::new(
                        "语义错误：高阶方法的状态参数必须是 rawptr<void>（裸指针生命周期由用户负责）",
                        0, 0,
                    ));
                }
                match method {
                    "filter" => {
                        if *ret != Type::Bool {
                            Err(DialcError::new(
                                "语义错误：array.filter() 的回调必须返回 bool",
                                0, 0,
                            ))
                        } else {
                            Ok(Type::Array(Box::new(elem.clone())))
                        }
                    }
                    "sort_by" => {
                        if *ret != Type::Int {
                            Err(DialcError::new(
                                "语义错误：array.sort_by() 的回调必须返回 int（比较器语义：正数表示左值更大）",
                                0, 0,
                            ))
                        } else {
                            Ok(Type::Array(Box::new(elem.clone())))
                        }
                    }
                    _ => {
                        // map：回调返回值决定新数组元素类型，必须是标量
                        // （C 侧新建数组时不做 ARC 所有权结算）。
                        if !matches!(
                            ret.as_ref(),
                            Type::Int | Type::Float | Type::Bool | Type::Byte | Type::Char
                        ) {
                            return Err(DialcError::new(
                                "语义错误：array.map() 的回调必须返回标量类型（int/float/bool/byte/char）",
                                0, 0,
                            ));
                        }
                        Ok(Type::Array(Box::new(*ret)))
                    }
                }
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
        // v1.04 C6：`get` 未命中返回 -1，与「真实值恰为 -1」无法区分。
        // get_or(k, def) 让调用方显式给出默认值，一次探测就把两种情况分开。
        "get_or" => {
            if args.len() != 2 || arg_t(0)? != Type::Int || arg_t(1)? != Type::Int {
                return Err(DialcError::new(
                    "语义错误：map.get_or(k, default) 需要 2 个 int 参数（键与默认值）",
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

/// 集合方法（v1.00，`set<T>`，底层复用 int→int map 的键集）的签名与返回类型。
///
/// `add` 与 `map.put` / `array.push` 同构：返回可能更新后的容器指针，由语句级
/// 代码生成写回接收变量（COW 语义）；`values` 返回成员快照数组，可安全迭代。
/// `remove` 返回被删成员的值（恒为 1），未命中返回 -1 —— 与 `map.remove` 一致，
/// 刻意不返回容器指针，避免赋值时产生未 retain 的别名（那会导致 double free）。
fn set_method_type<V>(
    elem: &Type,
    method: &str,
    args: &[Expr],
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    let expect_int_args = |n: usize| -> Result<()> {
        if args.len() != n {
            return Err(DialcError::new(
                format!("语义错误：set.{}() 需要 {} 个参数", method, n),
                0, 0,
            ));
        }
        for a in args {
            if type_of(a, scopes, funcs, structs)? != Type::Int {
                return Err(DialcError::new(
                    format!(
                        "语义错误：set.{}() 的实参必须是 int（当前版本 set 元素类型只支持 int）",
                        method
                    ),
                    0, 0,
                ));
            }
        }
        Ok(())
    };
    let no_args = || -> Result<()> {
        if args.is_empty() {
            Ok(())
        } else {
            Err(DialcError::new(
                format!("语义错误：set.{}() 不接受参数", method),
                0, 0,
            ))
        }
    };
    match method {
        "add" => { expect_int_args(1)?; Ok(Type::Set(Box::new(elem.clone()))) }
        "contains" => { expect_int_args(1)?; Ok(Type::Bool) }
        "remove" => { expect_int_args(1)?; Ok(Type::Int) }
        "size" => { no_args()?; Ok(Type::Int) }
        "values" => { no_args()?; Ok(Type::Array(Box::new(elem.clone()))) }
        _ => Err(DialcError::new(
            format!("语义错误：set 无方法 '{}'", method),
            0, 0,
        )),
    }
}

/// kind 编码 -> Dial 类型（与 parser::kv_kind_of / dial_kvmap.c 的 KV_* 一致）。
fn kv_type_from_kind(k: i64) -> Result<Type> {
    match k {
        0 => Ok(Type::Int),
        1 => Ok(Type::Float),
        2 => Ok(Type::String),
        3 => Ok(Type::Bool),
        4 => Ok(Type::Char),
        5 => Ok(Type::Byte),
        _ => Err(DialcError::new(
            "语义错误：map<K,V> 的键/值类型不受支持（kind 越界）", 0, 0,
        )),
    }
}

/// 泛型 map<K,V> 方法（v1.00）的签名与返回类型。
///
/// 零隐式转换铁律：`put` 的键/值必须**精确**匹配 K/V（例如 `map<string,int>`
/// 的 `put(1, 2)` 会被拒绝）。`get` / `remove` 返回 V，`contains` 返回 bool。
/// `keys()` / `values()` 是迭代 API：返回成员快照数组，交给既有 `for (v in arr)`。
fn kvmap_method_type<V>(
    k: &Type,
    v: &Type,
    method: &str,
    args: &[Expr],
    scopes: &Scopes<V>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<Type> {
    let arg_t = |i: usize| type_of(&args[i], scopes, funcs, structs);
    match method {
        "put" => {
            if args.len() != 2 || arg_t(0)? != *k || arg_t(1)? != *v {
                return Err(DialcError::new(
                    format!(
                        "语义错误：map<{}, {}>.put(k, v) 需要 2 个参数，类型分别为 {} 与 {}",
                        k.name(), v.name(), k.name(), v.name()
                    ),
                    0, 0,
                ));
            }
            // 与 map.put / array.push / set.add 同款：返回（可能更新后的）容器指针，
            // 由语句级代码生成写回接收变量（COW 契约）。
            Ok(Type::KvMap(Box::new(k.clone()), Box::new(v.clone())))
        }
        "get" | "contains" | "remove" => {
            if args.len() != 1 || arg_t(0)? != *k {
                return Err(DialcError::new(
                    format!(
                        "语义错误：map<{}, {}>.{}() 需要 1 个 {} 类型的键",
                        k.name(), v.name(), method, k.name()
                    ),
                    0, 0,
                ));
            }
            Ok(if method == "contains" { Type::Bool } else { v.clone() })
        }
        // v1.04 C6：泛型 map 的 get_or(k, default)。
        // 刻意只放行**标量 V**：标量按值存储，默认值位模式没有 ARC 所有权问题；
        // 若放行 string 这类句柄 V，「未命中时返回的默认字符串该由谁 retain」会
        // 产生歧义，与 §19「宁可泄漏，不可悬垂」相悖。那种场景请用 contains + get。
        // v1.04 B6：额外放行**标量字段结构体 V**——结构体值按值拷出（装箱对象只是
        // 容器内的存储），默认值与命中值都不产生所有权歧义。
        "get_or" => {
            let scalar_v = matches!(v, Type::Int | Type::Float | Type::Bool | Type::Byte | Type::Char);
            let struct_v = match v {
                Type::Struct(name) => struct_has_only_scalar_fields(name, structs),
                _ => false,
            };
            if !scalar_v && !struct_v {
                return Err(DialcError::new(
                    format!(
                        "语义错误：map<{}, {}>.get_or() 只支持标量值类型（int/float/bool/byte/char）\
                         或字段全为标量的结构体；\n\
                         句柄值请用 contains(k) + get(k) 显式处理（避免默认值的所有权歧义）",
                        k.name(), v.name()
                    ),
                    0, 0,
                ));
            }
            if args.len() != 2 || arg_t(0)? != *k || arg_t(1)? != *v {
                return Err(DialcError::new(
                    format!(
                        "语义错误：map<{}, {}>.get_or(k, default) 需要 2 个参数，类型分别为 {} 与 {}",
                        k.name(), v.name(), k.name(), v.name()
                    ),
                    0, 0,
                ));
            }
            Ok(v.clone())
        }
        "size" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：map.size() 不接受参数", 0, 0));
            }
            Ok(Type::Int)
        }
        "keys" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：map.keys() 不接受参数", 0, 0));
            }
            // string 键 -> string[]（RC 快照）；其它键 -> int[]（值或位模式）。
            let elem = if *k == Type::String { Type::String } else { Type::Int };
            Ok(Type::Array(Box::new(elem)))
        }
        "values" => {
            if !args.is_empty() {
                return Err(DialcError::new("语义错误：map.values() 不接受参数", 0, 0));
            }
            // V = int：返回 int[]（位模式快照）。
            // v1.04 B6：V = 结构体时返回对应结构体数组（装箱对象快照，读出即按值拷贝）。
            match v {
                Type::Int => Ok(Type::Array(Box::new(Type::Int))),
                Type::Struct(_) => Ok(Type::Array(Box::new(v.clone()))),
                _ => Err(DialcError::new(
                    "语义错误：map.values() 当前版本只支持 V = int 或结构体（其它标量值请用 get 逐键取值）",
                    0, 0,
                )),
            }
        }
        _ => Err(DialcError::new(
            format!("语义错误：map<K,V> 无方法 '{}'", method), 0, 0,
        )),
    }
}

// ===================== 入口 =====================

/// 同类型变参脱糖（v1.03）：`...xs: T` → 一个普通的 `xs: T[]` 形参；
/// 调用点 `f(a, 1, 2, 3)` → `f(a, [1, 2, 3])`。
///
/// **这是纯 AST 结构改写，不是运行期机制**：编译后没有类型标签、没有装箱、
/// 没有额外的分派，变参数组就是一个正常的 `T[]`——它随 ARC 走，
/// 进入函数时 retain、函数返回时由编译器插入 release（见 codegen 的临时数组结算）。
/// Dial 自己定义的函数**不允许异构变参**：数组字面量的元素类型一致性检查会拦下
/// `f(1, "a")` 这类调用，并在诊断里引导用户显式使用结构体。
fn desugar_varargs(program: &mut Program) -> Result<()> {
    // 结构体表（供变参元素类型校验：结构体作变参元素本期只放开标量字段结构体）
    let struct_table: StructTable = program
        .structs
        .iter()
        .map(|s| (s.name.clone(), (s.fields.clone(), s.field_defaults.clone())))
        .collect();
    let mut map: HashMap<String, (usize, Type)> = HashMap::new();
    for f in program.functions.iter_mut() {
        if let Some((name, elem, line, col)) = f.variadic.take() {
            if !is_array_elem_type(&elem, &struct_table) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：变参元素类型 {} 不受支持（变参只接受同类型 T[]，需要异构参数请显式定义结构体）",
                        elem.display()
                    ),
                    line,
                    col,
                ));
            }
            let fixed = f.params.len();
            f.params.push((name, Type::Array(Box::new(elem.clone()))));
            f.defaults.push(None);
            f.param_pos.push((line, col));
            map.insert(f.name.clone(), (fixed, elem));
        }
    }
    if map.is_empty() {
        return Ok(());
    }
    for f in program.functions.iter_mut() {
        for d in f.defaults.iter_mut() {
            if let Some(e) = d {
                desugar_expr(e, &map);
            }
        }
        desugar_stmts(&mut f.body, &map);
    }
    Ok(())
}

/// 表达式层：先递归改写子表达式，再把变参调用点的尾部实参打包成数组字面量。
/// `expect = Some(T)` 同时是 codegen 识别「脱糖生成的临时数组」的标记
/// （源码手写的 `[1, 2]` 是 `None`），调用结束后据此插入一次 release。
fn desugar_expr(e: &mut Expr, map: &HashMap<String, (usize, Type)>) {
    match e {
        Expr::Call(name, args) => {
            for a in args.iter_mut() {
                desugar_expr(a, map);
            }
            if let Some((fixed, elem)) = map.get(name) {
                if args.len() >= *fixed {
                    let rest: Vec<Expr> = args.drain(*fixed..).collect();
                    args.push(Expr::ArrayLit(rest, Some(Box::new(elem.clone()))));
                }
                // 实参不足 fixed 的情形保持原样，交给常规参数个数检查报错。
            }
        }
        Expr::ArrayLit(elems, _) => {
            for x in elems.iter_mut() {
                desugar_expr(x, map);
            }
        }
        Expr::StructLit(_, fields) => {
            for (_, x) in fields.iter_mut() {
                desugar_expr(x, map);
            }
        }
        Expr::Interp(parts) => {
            for p in parts.iter_mut() {
                if let InterpPart::Expr(x) = p {
                    desugar_expr(x.as_mut(), map);
                }
            }
        }
        Expr::Field(o, _) | Expr::Unary(_, o) => desugar_expr(o, map),
        Expr::Method(o, _, args) => {
            desugar_expr(o, map);
            for a in args.iter_mut() {
                desugar_expr(a, map);
            }
        }
        Expr::Index(o, i) => {
            desugar_expr(o, map);
            desugar_expr(i, map);
        }
        Expr::Slice(a, f, t) => {
            desugar_expr(a, map);
            desugar_expr(f, map);
            desugar_expr(t, map);
        }
        Expr::Binary(l, _, r) => {
            desugar_expr(l, map);
            desugar_expr(r, map);
        }
        Expr::Ternary(c, a, b) => {
            desugar_expr(c, map);
            desugar_expr(a, map);
            desugar_expr(b, map);
        }
        _ => {}
    }
}

/// 语句层：覆盖所有含表达式 / 含子语句块的变体（结构参照 collect_refs）。
fn desugar_stmts(stmts: &mut [Stmt], map: &HashMap<String, (usize, Type)>) {
    for s in stmts.iter_mut() {
        match s {
            Stmt::VarDecl { value, .. } | Stmt::Assign { value, .. } => {
                desugar_expr(value, map)
            }
            Stmt::AssignDeref { ptr, value, .. } => {
                desugar_expr(ptr, map);
                desugar_expr(value, map);
            }
            Stmt::AssignField { obj, value, .. } => {
                desugar_expr(obj, map);
                desugar_expr(value, map);
            }
            Stmt::AssignIndex { arr, index, value, .. } => {
                desugar_expr(arr, map);
                desugar_expr(index, map);
                desugar_expr(value, map);
            }
            Stmt::If { cond, then_branch, else_branch, .. } => {
                desugar_expr(cond, map);
                desugar_stmts(then_branch, map);
                if let Some(eb) = else_branch {
                    desugar_stmts(eb, map);
                }
            }
            Stmt::While { cond, body, .. } => {
                desugar_expr(cond, map);
                desugar_stmts(body, map);
            }
            Stmt::For { start, end, step, body, .. } => {
                desugar_expr(start, map);
                desugar_expr(end, map);
                if let Some(st) = step {
                    desugar_expr(st, map);
                }
                desugar_stmts(body, map);
            }
            Stmt::ForEach { coll, body, .. } | Stmt::ForEachMap { coll, body, .. } => {
                desugar_expr(coll, map);
                desugar_stmts(body, map);
            }
            Stmt::DoWhile { body, cond, .. } => {
                desugar_stmts(body, map);
                desugar_expr(cond, map);
            }
            Stmt::Match { subject, arms, .. } => {
                desugar_expr(subject, map);
                for arm in arms.iter_mut() {
                    desugar_stmts(&mut arm.body, map);
                }
            }
            Stmt::Defer(inner, ..) => desugar_stmts(std::slice::from_mut(inner.as_mut()), map),
            Stmt::With { resource, body, .. } => {
                desugar_expr(resource, map);
                desugar_stmts(body, map);
            }
            Stmt::Return(Some(e), ..) | Stmt::Throw(e, ..) | Stmt::Expr(e, ..) => {
                desugar_expr(e, map)
            }
            Stmt::Try { body, catch, .. } => {
                desugar_stmts(body, map);
                desugar_stmts(&mut catch.body, map);
            }
            Stmt::Select { arms, default_body, .. } => {
                for arm in arms.iter_mut() {
                    desugar_expr(&mut arm.chan, map);
                    desugar_stmts(&mut arm.body, map);
                }
                if let Some(db) = default_body {
                    desugar_stmts(db, map);
                }
            }
            Stmt::Block { body, .. } => desugar_stmts(body, map),
            _ => {}
        }
    }
}

/// 入口：收集函数签名 + 结构体表，校验 main，逐一检查函数体，最后跑数据流分析写入 move_hint。
/// 保留名：这些标识符在 `type_of` 里按**名字**匹配为内建函数/类型，
/// 与用户函数的查找是两条路。若允许用户定义同名函数，`Expr::Call` 的
/// 内建分支会被静默绕过（或反之）——因此必须在编译期直接拒绝。
/// v1.04 FFI：`extern struct` 字段是否具确定的 C ABI 布局。
///
/// 只放行「C 侧有明确对应宽度」的类型；嵌套结构体必须是另一个 `extern struct`
/// （按值嵌入，LLVM 非 packed struct 与 C 的对齐/padding 规则一致）。
fn is_c_abi_field(t: &Type, cstruct_names: &[String]) -> bool {
    match t {
        // int32(int32_t) / float32(float) 是**确定的 C 宽度**，正是 extern struct
        // 需要精确布局的场景；cstruct_field_llvm 会把它们落成 i32 / f32。
        Type::Int | Type::Float | Type::Int32 | Type::Float32
            | Type::Bool | Type::Byte | Type::RawPtr(_) | Type::VolatilePtr(_) => true,
        Type::Struct(n) => cstruct_names.iter().any(|x| x == n),
        _ => false,
    }
}

const RESERVED_NAMES: &[&str] = &[
    "spawn", "join", "detach", "channel", "recv", "move_to", "thread",
    // v0.81 通道收尾：close 早已是 file 的内建（本就在按名字匹配，只是此前未列入此表），
    // 一并补齐；closed / len / done / try_recv 为本次新增。
    "close", "closed", "len", "done", "try_recv",
    // v0.9：判空与超时接收
    "is_null", "recv_timeout",
    // v1.00：断言 / 退出 / 数学内建
    "assert", "panic", "exit", "min", "max", "abs",
    "sqrt", "pow", "floor", "ceil", "round",
    // v1.00：集合与泛型 map 构造
    "new_set", "new_kv_map",
    // v1.01：提前释放（move 语义）——drop(x) 之后禁止再使用 x，杜绝 use-after-free
    "drop",
    // v1.04：FFI 反向数据总线（C 缓冲区 → Dial 对象）
    "string_from_cstr", "bytes_from_ptr",
    // v1.04 C8：输出族内建按**名字**硬编码分派（type_of 的 Expr::Call 分支），
    // 与用户函数表是两条路。若不设防，用户写 `fn eprint(...)` 会编译通过却在调用点
    // 被内建分支静默绕过——直接违反 §20 Fail-Fast「绝不静默损坏」。
    // print / print_raw / write_char 是同族的既有内建，此前也漏列，一并补齐以消除不对称；
    // eprint / eprint_raw 为本轮新增，必须随新内建同步设防。
    // 注：其余按名字分派的历史内建（read_line / args / now / sleep 等）的保留名覆盖
    // 属独立的历史加固议题，本轮不扩大范围，见 ARCHITECTURE §14.5 的批次纪律。
    "print", "print_raw", "write_char", "eprint", "eprint_raw",
    // v1.04 第 2 批：C 组新增内建函数（同样按名字硬编码分派，必须设防）。
    // 漏列会让用户定义同名函数却在调用点被内建分支静默绕过（§20 Fail-Fast）。
    "parse_int", "parse_float", "is_int", "is_float", "format_float",
    "read_all", "eof", "file_read_line", "file_exists", "file_size", "file_remove",
    "getenv", "has_env", "program_name", "cwd",
    "local_parts", "date_string", "monotonic_ms",
    // v1.04 底层原语：@asm / simd 构造都是按名字硬编码分派的内建，
    // 不设防则用户定义同名函数会被静默绕过（与 §20 Fail-Fast 冲突）。
    "asm", "asm_intel", "simd",
];

/// `externs`：由 `import` 引入的**包外部函数**（v0.92，见 package.rs）。
/// 它们没有函数体，只有 `.diali` 里的签名——但对调用方而言与本地函数别无二致：
/// 类型检查照做、调用照写，区别仅在于「函数体在别人的 `.a` 里」。
pub fn analyze(
    program: &mut Program,
    externs: &[(String, FuncInfo)],
) -> Result<(HashMap<String, FuncInfo>, StructTable, HashMap<String, usize>)> {
    // v1.03：同类型变参先做 AST 脱糖。之后的类型检查 / LSP / codegen 都只看得见
    // 一个普通的 T[] 形参 —— 编译产物里没有任何变参专用的运行期机制。
    desugar_varargs(program)?;

    // 0) 保留名校验（函数名与结构体名都不能撞内建）
    for f in &program.functions {
        if RESERVED_NAMES.contains(&f.name.as_str()) {
            return Err(DialcError::new(
                format!(
                    "语义错误：'{}' 是内建函数的保留名，不能用作函数名",
                    f.name
                ),
                f.line,
                f.col,
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
        structs.insert(s.name.clone(), (s.fields.clone(), s.field_defaults.clone()));
    }
    // B8：字段默认值必须是编译期常量（字面量 / 字面量数组），不得引用运行时值。
    for s in &program.structs {
        for (fname, de) in &s.field_defaults {
            if !is_ct_const(de) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：结构体 '{}' 字段 '{}' 的默认值必须是编译期常量（字面量）",
                        s.name, fname
                    ),
                    s.line, s.col,
                ));
            }
        }
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

    // 1.5) v1.04 FFI：C ABI 结构体（`extern struct`）。
    //      字段表并入 `structs` 供字段访问统一查表；布局合法性由 is_c_abi_field 兜住——
    //      ARC 容器、`string` / `bytes` 句柄、`char`（码点 i64 与 C char 宽度歧义）
    //      一律拒绝，避免出现「编译器算出的 padding 与 C 不一致」的静默内存损坏。
    let cstruct_names: Vec<String> = program.cstructs.iter().map(|c| c.name.clone()).collect();
    for cs in &program.cstructs {
        if structs.contains_key(&cs.name) {
            return Err(DialcError::new(
                format!("语义错误：结构体 '{}' 重复定义（普通 struct 与 extern struct 撞名）", cs.name),
                cs.line, cs.col,
            ));
        }
        for (fname, ft) in &cs.fields {
            if !is_c_abi_field(ft, &cstruct_names) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：extern struct '{}' 的字段 '{}' 类型 {} 不具确定的 C ABI 布局；\
                         只允许 int(int64_t) / int32(int32_t) / float(double) / float32(float) / bool(_Bool) / byte(uint8_t) / rawptr<T>(void*) / 嵌套 extern struct。\
                         char 请改用 byte 或 int，string / bytes / 数组 / map 请用 rawptr<T> 传递",
                        cs.name, fname, ft.display()
                    ),
                    cs.line, cs.col,
                ));
            }
        }
        structs.insert(cs.name.clone(), (cs.fields.clone(), HashMap::new()));
    }

    // 1.6) v1.04 FFI：语言层 extern fn 的保留名 / 与本地函数撞名校验。
    for e in &program.externs {
        if RESERVED_NAMES.contains(&e.name.as_str()) {
            return Err(DialcError::new(
                format!("语义错误：'{}' 是内建函数的保留名，不能用作 extern fn 名", e.name),
                e.line, e.col,
            ));
        }
        if program.functions.iter().any(|f| f.name == e.name) {
            return Err(DialcError::new(
                format!("语义错误：extern fn '{}' 与本地函数定义撞名", e.name),
                e.line, e.col,
            ));
        }
    }

    // 2) 函数签名
    let mut funcs: HashMap<String, FuncInfo> = HashMap::new();
    // v1.04：label/goto 的作用域深度（自研后端生成 phi / 释放层所需，LLVM 后端
    // 已有等价计算；这里统一收集，避免两个后端各算一套导致深度口径不一致）。
    let mut goto_depths: HashMap<String, usize> = HashMap::new();
    // v1.04 FFI 导出校验：extern struct 名集合（按值嵌入 C 结构的字段才走 C ABI）。
    let cstruct_names: Vec<String> = program.cstructs.iter().map(|c| c.name.clone()).collect();
    for f in &program.functions {
        if funcs.contains_key(&f.name) {
            return Err(DialcError::new(
                format!("语义错误：函数 '{}' 重复定义", f.name),
                f.line,
                f.col,
            ));
        }
        // v1.04 FFI 导出：导出给 C 的符号，签名必须仅含 C 兼容类型，
        // 否则 C 侧无法按 C ABI 正确传参/收值（mangling / 内存布局会错位）。
        if f.exported {
            for (pi, (pname, pt)) in f.params.iter().enumerate() {
                if !is_c_abi_field(pt, &cstruct_names) {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：导出的函数 '{}' 第 {} 个形参 '{}' 的类型 '{}' 不是 C 兼容类型；\
                             @export 函数的签名只能含标量 / rawptr / fnptr / extern struct（值）",
                            f.name, pi + 1, pname, pt.name()
                        ),
                        f.line, f.col,
                    ));
                }
            }
            if !matches!(f.ret, Type::Void) && !is_c_abi_field(&f.ret, &cstruct_names) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：导出的函数 '{}' 的返回类型 '{}' 不是 C 兼容类型；\
                         @export 函数的返回类型只能是 void / 标量 / rawptr / fnptr / extern struct（值）",
                        f.name, f.ret.name()
                    ),
                    f.line, f.col,
                ));
            }
        }
        funcs.insert(
            f.name.clone(),
            FuncInfo {
                params: f.params.iter().map(|(_, t)| t.clone()).collect(),
                ret: f.ret.clone(),
                defaults: f.defaults.clone(),
                // Dial 自身函数的 `...xs: T` 已脱糖成 T[]；只有包 extern 才可能是 C 变参。
                variadic: false,
                exported: f.exported,
            },
        );
    }

    // 2.4) 默认实参类型校验（v1.00）：默认值必须是与形参类型兼容的常量表达式。
    // 用空作用域求值——Dial 没有全局变量，默认值里引用任何变量都应当场拒绝。
    for f in &program.functions {
        let empty: Scopes<VarState> = vec![HashMap::new()];
        for (i, d) in f.defaults.iter().enumerate() {
            if let Some(e) = d {
                let dt = type_of(e, &empty, &funcs, &structs)?;
                let pt = &f.params[i].1;
                if !types_compatible(&dt, pt) {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：函数 '{}' 第 {} 个形参的默认值类型 {} 与形参类型 {} 不兼容",
                            f.name,
                            i + 1,
                            dt.name(),
                            pt.name()
                        ),
                        f.line, f.col,
                    ));
                }
            }
        }
    }

    // 2.5) 导入的包外部函数。
    // 名字冲突当场拒绝：本地函数与包函数同名时，链接期会变成「到底链了哪一个」的悬案，
    // 而这里能用一句明确的错误把它钉死。
    for (name, info) in externs {
        if funcs.contains_key(name) {
            return Err(DialcError::new(
                format!(
                    "语义错误：导入的包函数 '{}' 与本程序中的函数重名；请重命名本地函数",
                    name
                ),
                0,
                0,
            ));
        }
        funcs.insert(name.clone(), info.clone());
    }

    if !funcs.contains_key("main") {
        return Err(DialcError::new("语义错误：缺少程序入口函数 main()", 0, 0));
    }
    if funcs["main"].ret.clone() != Type::Int {
        // 定位到 main 的定义处（而不是文件开头）：用户一眼就能看到是哪个 main 的问题。
        let main_pos = program
            .functions
            .iter()
            .find(|f| f.name == "main")
            .map(|f| (f.line, f.col))
            .unwrap_or((0, 0));
        return Err(DialcError::new(
            "语义错误：main 函数必须返回 int（作为进程退出码）",
            main_pos.0,
            main_pos.1,
        ));
    }

    for f in &program.functions {
        check_function(f, &funcs, &structs, &mut goto_depths)?;
    }

    // 3) 数据流分析：写入 move_hint
    for f in &mut program.functions {
        run_usage_analysis(f, &structs);
    }

    Ok((funcs, structs, goto_depths))
}

/// 类型良构性检查：拒绝当前版本尚无法正确实现的类型。
///
/// 动态数组的 C 实现（dial_array_*）以 int64 存储元素，因此元素类型**只允许 int**。
/// 若放任 `float[]`，元素会在传入 C 时被 fptosi 静默截断——这正是必须在
/// 编译期就拒绝而非运行期静默降级的地方。
fn check_type_wellformed(
    t: &Type,
    ctx: &str,
    line: usize,
    col: usize,
    structs: &StructTable,
) -> Result<()> {
    match t {
        // v1.00 的 set 复用 int→int map 的键集，故元素类型只支持 int。
        // 放宽到别的元素类型需要 C 侧先支持任意宽度的键，这里在编译期明确拒绝，
        // 避免「写了 set<float> 却静默按 int 截断」这类数据损坏。
        Type::KvMap(k, v) => {
            let ok_k = matches!(k.as_ref(), Type::Int | Type::Float | Type::Bool | Type::Char | Type::Byte | Type::String);
            // v1.00 值只支持标量：string 值的 get 未命中默认对象会带来所有权移交歧义。
            // v1.04 B6：值额外放开「字段全为标量」的结构体——装箱为 RC 对象后存指针位模式，
            // 未命中返回空指针，由编译器还原为零值结构体（不存在所有权移交歧义）。
            let ok_v = matches!(v.as_ref(), Type::Int | Type::Float | Type::Bool | Type::Char | Type::Byte)
                || match v.as_ref() {
                    Type::Struct(name) => struct_has_only_scalar_fields(name, structs),
                    _ => false,
                };
            if ok_k && ok_v {
                Ok(())
            } else {
                Err(DialcError::new(
                    format!(
                        "语义错误：{} 是 map<{}, {}> 类型；当前版本键支持 int/float/bool/char/byte/string，值支持标量（暂不支持 string 值）与字段全为标量的结构体",
                        ctx, k.name(), v.name()
                    ),
                    line, col,
                ))
            }
        }
        Type::Set(elem) => {
            if matches!(elem.as_ref(), Type::Int) {
                Ok(())
            } else {
                Err(DialcError::new(
                    format!(
                        "语义错误：{} 是 set<{}> 类型，但当前版本 set 的元素类型只支持 int",
                        ctx,
                        elem.as_ref().name()
                    ),
                    line,
                    col,
                ))
            }
        }
        Type::Array(elem) => {
            if is_array_elem_type(elem, structs) {
                Ok(())
            } else {
                Err(DialcError::new(
                    format!(
                        "语义错误：{} 是 {}[] 类型，但动态数组元素类型只支持标量\n\
                         （int / float / bool / char / byte）与句柄类型\n\
                         （string / bytes / 数组 / map / set / kvmap / error / channel / 不透明句柄），\n\
                         以及字段全为标量的结构体；\n\
                         含句柄字段的结构体与 C ABI 结构体暂不能作数组元素",
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
                Type::String | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error | Type::Channel(_) => Ok(()),
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
    goto_depths: &mut HashMap<String, usize>,
) -> Result<()> {
    // 形参类型良构性
    for (pname, pty) in &f.params {
        check_type_wellformed(
            pty,
            &format!("函数 '{}' 的形参 '{}'", f.name, pname),
            f.line,
            f.col,
            structs,
        )?;
    }
    // 返回类型良构性
    check_type_wellformed(
        &f.ret,
        &format!("函数 '{}' 的返回类型", f.name),
        f.line,
        f.col,
        structs,
    )?;

    let mut scopes: Scopes<VarState> = vec![HashMap::new()];
    for (pname, pty) in &f.params {
        // 形参一律按可变登记（与既有行为一致：形参可在函数体内被重新赋值）。
        scopes[0].insert(pname.clone(), (pty.clone(), VarState::new(true)));
    }
    // goto / label 的三条死锁在**函数级**统一校验（需要看到全部标签才能判断
    // 目标是否存在、是否同块、是否跨越 try / with 边界）。
    let ga = analyze_gotos(&f.body)?;
    for (name, site) in ga.labels {
        goto_depths.insert(name, site.depth);
    }
    for stmt in &f.body {
        check_stmt_at(stmt, &mut scopes, funcs, f, structs, false, 0)?;
    }
    Ok(())
}

/// 把变量标记为「已转移」。向外找到**声明它的那个作用域**再置位，
/// 而不是只写 `last()`：这样「if 分支里 move 之后，主流程再用」也会被保守拦下。
/// 值类型（Copy）判定：drop 对它们无意义（无所有权），故 drop 后仍能继续使用。
/// 标量 int/float/bool/char/byte 与用户结构体（值语义、无 ARC）均属于此类。
/// 其余（string/bytes/array/map/kvmap/set/error/channel/file/thread/opaque）
/// 都是带所有权的句柄，drop 会真正释放并随后禁用该变量。
fn is_copy_type(t: &Type) -> bool {
    matches!(
        t,
        Type::Int
            | Type::Float
            | Type::Bool
            | Type::Char
            | Type::Byte
            | Type::Struct(_)
            // 函数指针是纯地址值类型：没有所有权，drop 对它无意义。
            | Type::FnPtr(..)
    )
}

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
/// `defer` 体内是否含控制流跳转语句。
///
/// 延迟语句在「离开作用域」时才执行，此时 `break` / `continue` 的跳转目标可能已随循环结束
/// 失效，`return` / `throw` 则会与触发该出口的那次跳转互相冲突（甚至生成非法 IR）。
/// 因此编译期一律拒绝，并给出明确诊断，而不是把未定义行为留给运行期。
fn defer_has_jump(s: &Stmt) -> bool {
    match s {
        Stmt::Break(..) | Stmt::Continue(..) | Stmt::Return(..) | Stmt::Throw(..) => true,
        // goto 同样不行：延迟执行时目标标签所在的作用域可能已消失。
        Stmt::Goto(..) => true,
        Stmt::Defer(inner, ..) => defer_has_jump(inner),
        Stmt::Block { body, .. }
        | Stmt::While { body, .. }
        | Stmt::For { body, .. }
        | Stmt::ForEach { body, .. }
        | Stmt::ForEachMap { body, .. }
        | Stmt::DoWhile { body, .. } => body.iter().any(defer_has_jump),
        Stmt::If { then_branch, else_branch, .. } => {
            then_branch.iter().any(defer_has_jump)
                || else_branch
                    .as_ref()
                    .map(|b| b.iter().any(defer_has_jump))
                    .unwrap_or(false)
        }
        Stmt::Match { arms, .. } => arms.iter().any(|a| a.body.iter().any(defer_has_jump)),
        Stmt::Try { body, catch, .. } => {
            body.iter().any(defer_has_jump) || catch.body.iter().any(defer_has_jump)
        }
        Stmt::Select { arms, default_body, .. } => {
            arms.iter().any(|a| a.body.iter().any(defer_has_jump))
                || default_body
                    .as_ref()
                    .map(|b| b.iter().any(defer_has_jump))
                    .unwrap_or(false)
        }
        _ => false,
    }
}

fn check_moved_stmt(
    stmt: &Stmt,
    scopes: &mut Scopes<VarState>,
    funcs: &HashMap<String, FuncInfo>,
    structs: &StructTable,
) -> Result<()> {
    let (line, col) = stmt_pos(stmt);
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
        Stmt::AssignDeref { ptr, value, .. } => {
            check_moved_expr(ptr, scopes, funcs, structs, line, col)?;
            check_moved_expr(value, scopes, funcs, structs, line, col)
        }
        Stmt::With { resource, .. } => {
            check_moved_expr(resource, scopes, funcs, structs, line, col)
        }
        Stmt::If { cond, .. } => check_moved_expr(cond, scopes, funcs, structs, line, col),
        Stmt::While { cond, .. } => check_moved_expr(cond, scopes, funcs, structs, line, col),
        Stmt::For { start, end, .. } => {
            check_moved_expr(start, scopes, funcs, structs, line, col)?;
            check_moved_expr(end, scopes, funcs, structs, line, col)
        }
        Stmt::ForEach { coll, .. } | Stmt::ForEachMap { coll, .. } => {
            check_moved_expr(coll, scopes, funcs, structs, line, col)
        }
        Stmt::DoWhile { cond, .. } => {
            check_moved_expr(cond, scopes, funcs, structs, line, col)
        }
        Stmt::Match { subject, .. } => {
            check_moved_expr(subject, scopes, funcs, structs, line, col)
        }
        Stmt::Defer(inner, ..) => {
            check_moved_stmt(inner, scopes, funcs, structs)
        }
        Stmt::Return(Some(e), ..) | Stmt::Throw(e, ..) | Stmt::Expr(e, ..) => {
            check_moved_expr(e, scopes, funcs, structs, line, col)
        }
        // select（v0.91）：case 里的通道表达式要受所有权约束（arm.chan 是表达式，
        // 需检查它引用的通道是否已被转移）。
        //
        // 分支体**不在这里下探**——它们是嵌套语句块，由 check_stmt 的 Select 分支
        // 递归时各自检查（check_stmt_at -> check_moved_stmt），恰好一次。
        // 若这里再递归一遍，同一条 move_to 会被检查两次：第一次通过并标记 moved，
        // 第二次立刻报「变量已转移，无法使用」——一个完全合法的程序被误拒
        // （例如 select { case recv(ch) as v { move_to(s, ch); } } 只写了一次 move_to）。
        Stmt::Select {
            arms, ..
        } => {
            for arm in arms {
                check_moved_expr(&arm.chan, scopes, funcs, structs, line, col)?;
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
        // 取地址 `&x`（v1.02）：只允许对 **var（可变）** 变量取址。
        // 对 `let` 取址等于给了一条绕过不可变约束的旁路（拿到指针就能写），
        // 因此在编译期直接拒绝。
        Expr::Unary(UnOp::AddrOf, o) => {
            if let Expr::Var(name) = &**o {
                let mutable = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name))
                    .map(|v| v.1.mutable)
                    .unwrap_or(false);
                if !mutable {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：不能对不可变变量 '{}' 取地址（let 声明的变量不可变，取址会绕过该约束）",
                            name
                        ),
                        line,
                        col,
                    ));
                }
            }
            check_moved_expr(o, scopes, funcs, structs, line, col)
        }
        Expr::Unary(_, o) => check_moved_expr(o, scopes, funcs, structs, line, col),
        Expr::ArrayLit(elems, _) => {
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
                            if matches!(t, Type::String | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error) {
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
/// 取一条语句在源码中的位置。
///
/// v0.93 起**所有**语句都带位置（连 `If` / `While` / `For` / `Expr` 都补上了）。
/// 这不是洁癖：语义错误发生在**表达式**里，而 Dial 的 `Expr` 不携带行列，
/// 因此一条 `let c: char = 'a' + 1;` 报出来的错误天然是「不知道在哪」。
/// 有了语句位置，`check_stmt_at` 就能把它盖到出错的那条语句上——
/// 否则编辑器里看到的是「第一行一个红点，真正的错误你自己找」。
/// 求一条语句的 (行, 列)。codegen 也用它把运行时失败（panic / assert）定位到源码行，
/// 故暴露为 crate 内可见——语义层与代码生成层必须共用同一套位置口径。
pub(crate) fn stmt_pos(stmt: &Stmt) -> (usize, usize) {
    match stmt {
        Stmt::VarDecl { line, col, .. }
        | Stmt::Assign { line, col, .. }
        | Stmt::AssignField { line, col, .. }
        | Stmt::AssignIndex { line, col, .. }
        | Stmt::AssignDeref { line, col, .. }
        | Stmt::If { line, col, .. }
        | Stmt::While { line, col, .. }
        | Stmt::For { line, col, .. }
        | Stmt::ForEach { line, col, .. }
        | Stmt::ForEachMap { line, col, .. }
        | Stmt::DoWhile { line, col, .. }
        | Stmt::Match { line, col, .. }
        | Stmt::Try { line, col, .. }
        | Stmt::Select { line, col, .. }
        | Stmt::Return(_, line, col)
        | Stmt::Throw(_, line, col)
        | Stmt::Break(line, col)
        | Stmt::Continue(line, col)
        | Stmt::Defer(_, line, col)
        | Stmt::Expr(_, line, col)
        | Stmt::With { line, col, .. }
        | Stmt::Label(_, line, col)
        | Stmt::Goto(_, line, col)
        | Stmt::Block { line, col, .. } => (*line, *col),
    }
}

/// `check_stmt` 的位置兜底包装。
///
/// 只补**缺失**的位置（`line == 0 && col == 0`）：已经带位置的错误
/// （语法阶段、以及少数本来就精确定位的语义检查）原样放行，绝不被覆盖。
///
/// 所有调用点都必须走这里，包括 `check_stmt` 自身的递归——
/// 漏掉一处递归，那棵子树里的错误就又会掉回「文件开头」。
fn check_stmt_at(
    stmt: &Stmt,
    scopes: &mut Scopes<VarState>,
    funcs: &HashMap<String, FuncInfo>,
    f: &Func,
    structs: &StructTable,
    in_loop: bool,
    try_depth: usize,
) -> Result<()> {
    let (line, col) = stmt_pos(stmt);
    // 语义错误多半来自表达式（Expr 不带位置），但「错在哪条语句」我们**有**——
    // 填上它，波浪线才不会落在文件开头。
    // `e.at(..)` 保留消息**与错误码**：工具据此按类处理，不该再去匹配文本。
    check_stmt(stmt, scopes, funcs, f, structs, in_loop, try_depth).map_err(|e| e.at(line, col))
}

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
            check_type_wellformed(ty, &format!("变量 '{}'", name), *line, *col, structs)?;
            let vt = type_of(value, scopes, funcs, structs)?;
            if !types_compatible(&vt, ty) && !channel_placeholder_compat(&vt, ty) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：变量 '{}' 的初始化表达式类型为 {}，与声明类型 {} 不兼容",
                        name,
                        // 用 display()（带具体参数，如 rawptr<byte>）而不是 name()
                        // ——裸指针 / 泛型 map 的元素类型必须显式写出来，否则
                        // 「rawptr 与 rawptr 不兼容」这种话毫无信息量。
                        vt.display(),
                        ty.display()
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
            // 【统一容器写回规则】`push` / `put` 这类「原地修改且可能因 COW 更换指针」的方法，
            // 其返回值由**编译器自动写回**接收变量（README §11.3 / §12.3）。
            // 因此严禁用户再手写 `a = a.push(...)` / `m = m.put(...)`：
            //   ① 与数组规则不统一——数组从来不这么写，map 没理由例外；
            //   ② 手写赋值暗示「由用户负责赋回」，而实际上编译器**无条件**写回，
            //      两种写法并存只会让人误以为它们有区别；
            //   ③ ARC 的交接由 C 侧完成（独占返回同指针、共享时克隆并释放本调用者的引用），
            //      让用户侧再参与赋值只会模糊 retain/release 的归属。
            // 把结果赋给**另一个**变量是允许的（那是取得新容器，不是就地修改），
            // 所以只在「接收者恰好就是被赋值的这个变量」时才拒绝。
            if let Expr::Method(recv, method, _args) = value {
                let is_cow_method = matches!(method.as_str(), "push" | "set" | "put");
                let same_receiver = lvalue_root(recv) == Some(name.as_str());
                if is_cow_method && same_receiver {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：'{}' 的返回值由编译器自动写回接收变量，严禁手写赋值；请直接写 {}.{}(...)，而不是 {} = {}.{}(...)",
                            method, name, method, name, name, method
                        ),
                        *line,
                        *col,
                    ));
                }
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
            // v1.04 FFI：`rawptr<CStruct>` 的字段写入。经裸指针写 C 侧内存不改变
            // 指针变量本身的值，因此不套用「目标必须是可变变量」的约束。
            let (sname, via_rawptr) = match &ot {
                Type::Struct(n) => (n.clone(), false),
                Type::RawPtr(inner) | Type::VolatilePtr(inner) => match &**inner {
                    Type::Struct(n) => (n.clone(), true),
                    _ => {
                        return Err(DialcError::new(
                            format!(
                                "语义错误：类型 '{}' 没有字段 '{}'（裸指针字段访问需要 rawptr<结构体名>）",
                                ot.name(),
                                field
                            ),
                            *line, *col,
                        ))
                    }
                },
                _ => {
                    return Err(DialcError::new(
                        format!("语义错误：类型 '{}' 没有字段 '{}'", ot.name(), field),
                        *line, *col,
                    ))
                }
            };
            let fields = &structs
                .get(&sname)
                .ok_or_else(|| DialcError::new(format!("语义错误：未知结构体 '{}'", sname), *line, *col))?
                .0;
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
            // 写入目标必须是可变变量（裸指针写 C 内存除外）
            if !via_rawptr {
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
                // 向量是 SSA 值类型，没有「原地改某个 lane」的地址语义。
                // 要换 lane 请重新构造一个向量整体赋值给 var 变量——优化器会把
                // 一串 insertelement 合并成单条指令，并不会真的逐 lane 跑。
                Type::Simd(elem, n) => {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：simd<{}, {}> 是值类型，不能原地改写单个 lane；\
                             请用 simd<{}, {}>(...) 重新构造后整体赋值给 var 变量",
                            elem.display(), n, elem.display(), n
                        ),
                        *line, *col,
                    ))
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
            ..
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
                check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth)?;
            }
            scopes.pop();
            if let Some(eb) = else_branch {
                scopes.push(HashMap::new());
                for s in eb {
                    check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth)?;
                }
                scopes.pop();
            }
            Ok(())
        }
        Stmt::While { cond, body, .. } => {
            let ct = type_of(cond, scopes, funcs, structs)?;
            if ct != Type::Bool {
                return Err(DialcError::new(
                    "语义错误：while 条件表达式必须是 bool 类型",
                    0, 0,
                ));
            }
            scopes.push(HashMap::new());
            for s in body {
                check_stmt_at(s, scopes, funcs, f, structs, true, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::For { var, start, end, step, body, .. } => {
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
            // 步长：可选，必须是编译期可求值的 int 常量且为正数（负步长会改变区间语义，暂不开放）。
            if let Some(se) = step {
                let sv = type_of(se, scopes, funcs, structs)?;
                if sv != Type::Int {
                    return Err(DialcError::new(
                        "语义错误：for 循环步长（step）必须是 int 类型",
                        0, 0,
                    ));
                }
                if let Expr::IntLit(n) = se {
                    if *n <= 0 {
                        return Err(DialcError::new(
                            "语义错误：for 循环步长（step）必须为正整数（当前支持递增区间 [start, end)）",
                            0, 0,
                        ));
                    }
                }
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
                check_stmt_at(s, scopes, funcs, f, structs, true, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::ForEach { value, coll, body, .. } => {
            let ct = type_of(coll, scopes, funcs, structs)?;
            let elem = match &ct {
                Type::Array(e) => (**e).clone(),
                // 集合迭代：成员即键，语义层按元素类型绑定迭代变量；
                // codegen 会先快照成 int[]，再走与数组相同的迭代路径。
                Type::Set(e) => (**e).clone(),
                _ => {
                    return Err(DialcError::new(
                        "语义错误：for (v in coll) 的 coll 必须是数组 Array(T) 或集合 set<T>",
                        0, 0,
                    ))
                }
            };
            if scopes.last().unwrap().contains_key(value) {
                return Err(DialcError::new(
                    format!("语义错误：迭代变量 '{}' 与外层变量重名", value),
                    0, 0,
                ));
            }
            scopes.push(HashMap::new());
            scopes
                .last_mut()
                .unwrap()
                .insert(value.clone(), (elem.clone(), VarState::new(true)));
            for s in body {
                check_stmt_at(s, scopes, funcs, f, structs, true, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::ForEachMap { key, value, coll, body, .. } => {
            let ct = type_of(coll, scopes, funcs, structs)?;
            // 支持 int->int 的 `map` 与泛型 `map<K,V>`（v1.01 起）。
            // 键/值变量类型随容器具体类型走：map -> (int, int)；map<K,V> -> (K, V)。
            let (k_ty, v_ty) = match &ct {
                Type::Map => (Type::Int, Type::Int),
                Type::KvMap(k, v) => ((**k).clone(), (**v).clone()),
                _ => {
                    return Err(DialcError::new(
                        "语义错误：for (k, v in m) 的 m 必须是 map 或 map<K,V> 类型",
                        0, 0,
                    ))
                }
            };
            if scopes.last().unwrap().contains_key(key) {
                return Err(DialcError::new(
                    format!("语义错误：键变量 '{}' 与外层变量重名", key),
                    0, 0,
                ));
            }
            if scopes.last().unwrap().contains_key(value) {
                return Err(DialcError::new(
                    format!("语义错误：值变量 '{}' 与外层变量重名", value),
                    0, 0,
                ));
            }
            scopes.push(HashMap::new());
            scopes
                .last_mut()
                .unwrap()
                .insert(key.clone(), (k_ty, VarState::new(true)));
            scopes
                .last_mut()
                .unwrap()
                .insert(value.clone(), (v_ty, VarState::new(true)));
            for s in body {
                check_stmt_at(s, scopes, funcs, f, structs, true, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::DoWhile { body, cond, .. } => {
            let ct = type_of(cond, scopes, funcs, structs)?;
            if ct != Type::Bool {
                return Err(DialcError::new(
                    "语义错误：do-while 条件必须是 bool 类型",
                    0, 0,
                ));
            }
            scopes.push(HashMap::new());
            for s in body {
                check_stmt_at(s, scopes, funcs, f, structs, true, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::Match { subject, arms, .. } => {
            let st = type_of(subject, scopes, funcs, structs)?;
            // 主体类型决定允许的模式类型；仅支持字面量可比较的类型
            match st {
                Type::Int | Type::Bool | Type::Char | Type::String => {}
                _ => {
                    return Err(DialcError::new(
                        "语义错误：match 主体必须是 int / bool / char / string 类型",
                        0, 0,
                    ))
                }
            }
            // 校验每个模式的类型与主体一致，并要求存在 _ 通配分支
            let mut has_wildcard = false;
            for arm in arms {
                match &arm.pattern {
                    MatchPattern::Wildcard => has_wildcard = true,
                    MatchPattern::Int(_) => {
                        if st != Type::Int {
                            return Err(DialcError::new(
                                "语义错误：match 模式 int 与主体类型不符",
                                0, 0,
                            ));
                        }
                    }
                    MatchPattern::Bool(_) => {
                        if st != Type::Bool {
                            return Err(DialcError::new(
                                "语义错误：match 模式 bool 与主体类型不符",
                                0, 0,
                            ));
                        }
                    }
                    MatchPattern::Char(_) => {
                        if st != Type::Char {
                            return Err(DialcError::new(
                                "语义错误：match 模式 char 与主体类型不符",
                                0, 0,
                            ));
                        }
                    }
                    MatchPattern::Str(_) => {
                        if st != Type::String {
                            return Err(DialcError::new(
                                "语义错误：match 模式 string 与主体类型不符",
                                0, 0,
                            ));
                        }
                    }
                }
            }
            if !has_wildcard {
                return Err(DialcError::new(
                    "语义错误：match 必须包含 `_` 通配分支（保证不遗漏任何值，杜绝未定义行为）",
                    0, 0,
                ));
            }
            // 各分支体独立作用域
            for arm in arms {
                scopes.push(HashMap::new());
                for s in &arm.body {
                    check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth)?;
                }
                scopes.pop();
            }
            Ok(())
        }
        Stmt::Defer(inner, ..) => {
            // defer 在离开作用域时执行，体内的控制流跳转目标届时可能已失效，编译期拒绝。
            if defer_has_jump(inner) {
                return Err(DialcError::new(
                    "语义错误：defer 体内不得使用 break / continue / return / throw（延迟语句在离开作用域时执行，跳转目标可能已失效）",
                    0, 0,
                ));
            }
            // defer 内部的语句语义上仍受当前上下文约束（作用域、循环、try 深度一致）
            check_stmt_at(inner, scopes, funcs, f, structs, in_loop, try_depth)
        }
        Stmt::With {
            resource,
            name,
            body,
            line,
            col,
            ..
        } => {
            let rt = type_of(resource, scopes, funcs, structs)?;
            // with 只管理「需要显式释放」的资源类型：file / thread。
            // （string / array / map 等 ARC 句柄本就随作用域自动释放，无需 with；
            //   裸指针由 v1.02 的裸指针一节单独纳入。）
            if !matches!(rt, Type::File | Type::Thread | Type::RawPtr(_) | Type::VolatilePtr(_)) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：with 只能管理 file / thread / 裸指针资源，实际为 {}（ARC 句柄随作用域自动释放，无需 with）",
                        rt.name()
                    ),
                    *line,
                    *col,
                ));
            }
            if scopes.last().unwrap().contains_key(name) {
                return Err(DialcError::new(
                    format!("语义错误：with 绑定的名字 '{}' 与外层变量重名", name),
                    *line,
                    *col,
                ));
            }
            scopes.push(HashMap::new());
            scopes
                .last_mut()
                .unwrap()
                .insert(name.clone(), (rt.clone(), VarState::new(true)));
            for s in body {
                check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::AssignDeref {
            ptr,
            value,
            line,
            col,
        } => {
            // 裸指针写入的第一道闸门：元素类型必须**具体**（rawptr<void> 不可写），
            // 且写入值的类型必须与元素类型严格一致（零隐式转换）。
            let pt = type_of(ptr, scopes, funcs, structs)?;
            let elem = match &pt {
                Type::RawPtr(e) | Type::VolatilePtr(e) => {
                    if **e == Type::Void {
                        return Err(DialcError::new(
                            "语义错误：rawptr<void> 是无类型裸指针，不能写入（请先 ptr_cast 成具体类型）",
                            *line,
                            *col,
                        ));
                    }
                    (**e).clone()
                }
                other => {
                    return Err(DialcError::new(
                        format!(
                            "语义错误：'*p = v' 的左侧必须是裸指针，实际为 {}",
                            other.name()
                        ),
                        *line,
                        *col,
                    ))
                }
            };
            let vt = type_of(value, scopes, funcs, structs)?;
            if !types_compatible(&vt, &elem) {
                return Err(DialcError::new(
                    format!(
                        "语义错误：写入裸指针的值类型为 {}，与指针元素类型 {} 不匹配",
                        vt.name(),
                        elem.name()
                    ),
                    *line,
                    *col,
                ));
            }
            Ok(())
        }
        Stmt::Label(_, _, _) => Ok(()),
        Stmt::Goto(_, _, _) => Ok(()),
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
                check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth + 1)?;
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
                check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth + 1)?;
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
                    check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth)?;
                }
                scopes.pop();
            }
            // default 分支自成一个作用域，没有绑定变量。
            if let Some(db) = default_body {
                scopes.push(HashMap::new());
                for s in db {
                    check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth)?;
                }
                scopes.pop();
            }
            Ok(())
        }
        Stmt::Block { body, .. } => {
            // 裸块：引入新词法作用域，递归检查内部语句后弹出作用域。
            scopes.push(HashMap::new());
            for s in body {
                check_stmt_at(s, scopes, funcs, f, structs, in_loop, try_depth)?;
            }
            scopes.pop();
            Ok(())
        }
        Stmt::Expr(e, ..) => {
            // 「原地修改型」方法约束（push / set）：
            // 这两种操作会修改容器，且因 COW 可能更换底层指针，编译器把返回指针写回接收变量。
            // 因此接收者必须是**可重新赋值**的顶层变量（var）。
            //   - let 变量不可被原地修改（与不可变契约一致），调用即编译错误；
            //   - 字段 / 下标等表达式作为接收者也无法被可靠地写回，同样报错。
            // 若只需获得新容器而不就地修改，请以表达式形式使用：let b: int[] = a.push(v);
            if let Expr::Method(obj, method, _) = e {
                // B1：具名类型（Struct/Opaque）的 impl 方法即便叫 push/set/put，
                // 也不是容器原地修改，跳过下面的 mutable 约束。
                let container_inplace = matches!(method.as_str(), "push" | "set" | "put")
                    && !is_impl_receiver(obj, scopes, funcs, structs);
                if container_inplace {
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
            // drop(x)：类型校验通过后，把 x 标记为「已转移」——后续对 x 的任何使用
            // 都会触发 use-after-move 诊断（"变量 'x' 已转移，无法使用"），杜绝 use-after-free。
            if let Expr::Call(name, args) = e {
                if name.as_str() == "drop" {
                    if let Expr::Var(vname) = &args[0] {
                        // 仅句柄类型（非 Copy）drop 后才禁用：标量/结构体是值类型，
                        // drop 是 no-op，标 moved 反而会误伤后续的合法使用。
                        if let Some(t) = lookup_type(scopes, vname) {
                            if !is_copy_type(&t) {
                                mark_moved(scopes, vname);
                            }
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

// ===================== goto / label 分析（v1.02） =====================
//
// `goto` 是「底层清理」利器，但无限制地乱跳会把程序打崩（未初始化变量、绕过
// with 的自动释放、破坏 try 的 jmp_buf 栈帧）。因此**三条死锁**在语义层硬拦：
//
//   ① 禁止跨过变量初始化（Zig 式严格）：goto 与标签之间若隔着一条变量声明，
//      无论方向一律拒绝——向前跳会读到未初始化的变量（C 的经典 UB），
//      向后跳会重复初始化并泄漏前一份（ARC 句柄）。
//   ② 禁止跳出 / 跳进 with 块：with 的自动释放会被绕过，资源将泄漏或被二次释放。
//   ③ 禁止跨越 try 边界：try 靠 setjmp/longjmp 保存栈帧，跨边界跳转会直接毁掉
//      jmp_buf；不同 try 块之间互跳同样禁止。
//
// 分析结果同时被 codegen 复用（标签所在的作用域深度 → 跳出去时要释放几层局部）。

/// 标签在函数内的位置信息。
#[derive(Debug, Clone)]
pub struct LabelSite {
    /// 所在块的路径（块 id 序列）。与跳转点的路径比较即可判定「同块 / 祖先块 / 旁支块」。
    pub path: Vec<usize>,
    /// 所在处的 try 路径（每个 try 块一个 id）。
    pub try_path: Vec<usize>,
    /// 所在处的 with 路径（每个 with 块一个 id）。
    pub with_path: Vec<usize>,
    /// 所在处的作用域深度（函数体 = 1，与 codegen 的 `scopes.len()` 同口径）。
    pub depth: usize,
    /// 全局深度优先语句序号（跨块比较「是否跨过声明」用）。
    pub seq: usize,
}

/// 跳转点信息。
#[derive(Debug, Clone)]
pub struct GotoSite {
    pub target: String,
    pub path: Vec<usize>,
    pub try_path: Vec<usize>,
    pub with_path: Vec<usize>,
    /// 全局深度优先语句序号（跨块比较「是否跨过声明」用）。
    pub seq: usize,
    pub line: usize,
    pub col: usize,
}

/// 一次函数级 goto/label 分析的结果。
#[derive(Debug, Default)]
pub struct GotoAnalysis {
    /// 标签名 → 标签位置（函数内唯一）。
    pub labels: HashMap<String, LabelSite>,
    /// 全部跳转点。
    pub gotos: Vec<GotoSite>,
}

/// 遍历期使用的 id 分配器（块 / try / with 各一路），外加全局语句序号。
#[derive(Debug, Default)]
struct GotoIds {
    next_block: usize,
    next_try: usize,
    next_with: usize,
    /// 全局深度优先语句序号（每条语句前分配一个，单调不重复）。
    seq: usize,
    /// 所有变量声明所在语句的全局序号。
    decl_seqs: Vec<usize>,
}

/// 判断 `prefix` 是否是 `path` 的前缀（祖先或相同）。
fn path_is_prefix(prefix: &[usize], path: &[usize]) -> bool {
    prefix.len() <= path.len() && path[..prefix.len()] == *prefix
}

/// 收集函数体内的标签与跳转点，并施加三条死锁。
///
/// 块的划分与 codegen 的作用域压栈点**一一对应**（If 的两个分支各自成块、
/// 循环体 / match 分支 / try 体与 catch 体 / select 各分支 / 裸块 / with 块各成一块），
/// 因此这里的 `depth` 与 codegen 的 `scopes.len()` 口径一致。
pub fn analyze_gotos(body: &[Stmt]) -> Result<GotoAnalysis> {
    let mut out = GotoAnalysis::default();
    let mut ids = GotoIds::default();
    let mut path: Vec<usize> = Vec::new();
    let mut try_path: Vec<usize> = Vec::new();
    let mut with_path: Vec<usize> = Vec::new();
    walk_gotos(
        body,
        1,
        &mut path,
        &mut try_path,
        &mut with_path,
        &mut out,
        &mut ids,
    )?;

    // ---- 三条死锁的校验 ----
    for g in &out.gotos {
        let l = out.labels.get(&g.target).ok_or_else(|| {
            DialcError::new(
                format!("语义错误：goto 目标标签 '{}' 未定义", g.target),
                g.line,
                g.col,
            )
        })?;

        // ③ 禁止跨越 try 边界（含「跳进 try」「跳出 try」「两个 try 之间互跳」）
        if l.try_path != g.try_path {
            return Err(DialcError::new(
                format!(
                    "语义错误：goto 不得跨越 try 边界（目标标签 '{}' 与跳转点不在同一个 try 作用域内；\
                     try 依赖 setjmp/longjmp 保存栈帧，跨界跳转会毁掉 jmp_buf）",
                    g.target
                ),
                g.line,
                g.col,
            ));
        }

        // ② 禁止跳出 / 跳进 with 块
        if l.with_path != g.with_path {
            return Err(DialcError::new(
                format!(
                    "语义错误：goto 不得跳出或跳进 with 块（目标标签 '{}'；\
                     with 块靠作用域结束自动释放资源，跨界跳转会绕过或重复该释放）",
                    g.target
                ),
                g.line,
                g.col,
            ));
        }

        // 只能跳到**同一块或外层块**的标签：跳进嵌套块 = 跳进一个尚未初始化的作用域。
        if !path_is_prefix(&l.path, &g.path) {
            return Err(DialcError::new(
                format!(
                    "语义错误：goto 目标标签 '{}' 不在当前作用域内（禁止跳进嵌套块：\
                     那里面的变量尚未初始化）",
                    g.target
                ),
                g.line,
                g.col,
            ));
        }

        // ① 禁止跨过变量初始化（Zig 式严格：两个方向都禁）
        //
        // 用**全局深度优先语句序号**而不是「同块内下标」：后者只能拦同块跳转，
        // 而「从内层块跳到外层标签」同样会跳过外层块中位于跳转点与标签之间的
        // 变量声明——实测那种写法会读到未初始化的栈槽（UB）。全局序号把跨块、
        // 同块、向前、向后一并覆盖。
        let (lo, hi) = if l.seq < g.seq {
            (l.seq, g.seq)
        } else {
            (g.seq, l.seq)
        };
        for d in &ids.decl_seqs {
            if *d > lo && *d < hi {
                return Err(DialcError::new(
                    format!(
                        "语义错误：goto 不得跨过变量初始化（目标标签 '{}' 与跳转点之间存在变量声明；\
                         向前跳会读到未初始化的变量，向后跳会重复初始化并泄漏前一份）",
                        g.target
                    ),
                    g.line,
                    g.col,
                ));
            }
        }
    }
    Ok(out)
}

/// 递归遍历语句，收集标签 / 跳转点，并记录每个块内的变量声明位置。
#[allow(clippy::too_many_arguments)]
fn walk_gotos(
    stmts: &[Stmt],
    depth: usize,
    path: &mut Vec<usize>,
    try_path: &mut Vec<usize>,
    with_path: &mut Vec<usize>,
    out: &mut GotoAnalysis,
    ids: &mut GotoIds,
) -> Result<()> {
    for s in stmts.iter() {
        // 每条语句（含递归进入的子块语句）按深度优先顺序领一个全局序号。
        let cur_seq = ids.seq;
        ids.seq += 1;
        match s {
            Stmt::VarDecl { .. } => {
                ids.decl_seqs.push(cur_seq);
            }
            Stmt::Label(name, line, col) => {
                if out.labels.contains_key(name) {
                    return Err(DialcError::new(
                        format!("语义错误：标签 '{}' 重复定义（同一函数内标签必须唯一）", name),
                        *line,
                        *col,
                    ));
                }
                out.labels.insert(
                    name.clone(),
                    LabelSite {
                        path: path.clone(),
                        try_path: try_path.clone(),
                        with_path: with_path.clone(),
                        depth,
                        seq: cur_seq,
                    },
                );
            }
            Stmt::Goto(target, line, col) => {
                out.gotos.push(GotoSite {
                    target: target.clone(),
                    path: path.clone(),
                    try_path: try_path.clone(),
                    with_path: with_path.clone(),
                    seq: cur_seq,
                    line: *line,
                    col: *col,
                });
            }
            Stmt::With { body, .. } => {
                // with 块自成一个块：块内 goto 不得跳出去（由 with_path 判定）
                let id = ids.next_block;
                ids.next_block += 1;
                let wid = ids.next_with;
                ids.next_with += 1;
                path.push(id);
                with_path.push(wid);
                walk_gotos(
                    body,
                    depth + 1,
                    path,
                    try_path,
                    with_path,
                    out,
                    ids,
                )?;
                with_path.pop();
                path.pop();
            }
            Stmt::Block { body, .. } => {
                let id = ids.next_block;
                ids.next_block += 1;
                path.push(id);
                walk_gotos(
                    body,
                    depth + 1,
                    path,
                    try_path,
                    with_path,
                    out,
                    ids,
                )?;
                path.pop();
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                let id = ids.next_block;
                ids.next_block += 1;
                path.push(id);
                walk_gotos(
                    then_branch,
                    depth + 1,
                    path,
                    try_path,
                    with_path,
                    out,
                    ids,
                )?;
                path.pop();
                if let Some(eb) = else_branch {
                    let id = ids.next_block;
                    ids.next_block += 1;
                    path.push(id);
                    walk_gotos(
                        eb,
                        depth + 1,
                        path,
                        try_path,
                        with_path,
                        out,
                        ids,
                    )?;
                    path.pop();
                }
            }
            Stmt::While { body, .. }
            | Stmt::For { body, .. }
            | Stmt::ForEach { body, .. }
            | Stmt::ForEachMap { body, .. }
            | Stmt::DoWhile { body, .. } => {
                let id = ids.next_block;
                ids.next_block += 1;
                path.push(id);
                walk_gotos(
                    body,
                    depth + 1,
                    path,
                    try_path,
                    with_path,
                    out,
                    ids,
                )?;
                path.pop();
            }
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    let id = ids.next_block;
                    ids.next_block += 1;
                    path.push(id);
                    walk_gotos(
                        &arm.body,
                        depth + 1,
                        path,
                        try_path,
                        with_path,
                        out,
                        ids,
                    )?;
                    path.pop();
                }
            }
            Stmt::Try { body, catch, .. } => {
                // try 体与 catch 体是**两个**块，但同属一个 try 上下文
                let tid = ids.next_try;
                ids.next_try += 1;
                try_path.push(tid);
                let id = ids.next_block;
                ids.next_block += 1;
                path.push(id);
                walk_gotos(
                    body,
                    depth + 1,
                    path,
                    try_path,
                    with_path,
                    out,
                    ids,
                )?;
                path.pop();
                let id = ids.next_block;
                ids.next_block += 1;
                path.push(id);
                walk_gotos(
                    &catch.body,
                    depth + 1,
                    path,
                    try_path,
                    with_path,
                    out,
                    ids,
                )?;
                path.pop();
                try_path.pop();
            }
            Stmt::Select {
                arms,
                default_body,
                ..
            } => {
                for arm in arms {
                    let id = ids.next_block;
                    ids.next_block += 1;
                    path.push(id);
                    walk_gotos(
                        &arm.body,
                        depth + 1,
                        path,
                        try_path,
                        with_path,
                        out,
                        ids,
                    )?;
                    path.pop();
                }
                if let Some(db) = default_body {
                    let id = ids.next_block;
                    ids.next_block += 1;
                    path.push(id);
                    walk_gotos(
                        db,
                        depth + 1,
                        path,
                        try_path,
                        with_path,
                        out,
                        ids,
                    )?;
                    path.pop();
                }
            }
            // defer 体内不允许 goto（跳转目标在 defer 执行时可能已失效），
            // 该限制由 defer_has_jump 统一施加。
            Stmt::Defer(inner, ..) => {
                let id = ids.next_block;
                ids.next_block += 1;
                path.push(id);
                walk_gotos(
                    std::slice::from_ref(&**inner),
                    depth + 1,
                    path,
                    try_path,
                    with_path,
                    out,
                    ids,
                )?;
                path.pop();
            }
            _ => {}
        }
    }
    Ok(())
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
        Expr::ArrayLit(elems, _) => {
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
            Stmt::If { cond, then_branch, else_branch, .. } => {
                record_expr_refs(cond, *si, unsafe_ctx, refs);
                collect_refs(then_branch, si, loop_d, cond_d + 1, refs);
                if let Some(eb) = else_branch {
                    collect_refs(eb, si, loop_d, cond_d + 1, refs);
                }
            }
            Stmt::While { cond, body, .. } => {
                record_expr_refs(cond, *si, unsafe_ctx, refs);
                collect_refs(body, si, loop_d + 1, cond_d, refs);
            }
            Stmt::For { var: _, start, end, body, .. } => {
                record_expr_refs(start, *si, unsafe_ctx, refs);
                record_expr_refs(end, *si, unsafe_ctx, refs);
                collect_refs(body, si, loop_d + 1, cond_d, refs);
            }
            Stmt::ForEach { coll, body, .. } | Stmt::ForEachMap { coll, body, .. } => {
                record_expr_refs(coll, *si, unsafe_ctx, refs);
                collect_refs(body, si, loop_d + 1, cond_d, refs);
            }
            Stmt::DoWhile { cond, body, .. } => {
                record_expr_refs(cond, *si, unsafe_ctx, refs);
                collect_refs(body, si, loop_d + 1, cond_d, refs);
            }
            Stmt::Match { subject, arms, .. } => {
                record_expr_refs(subject, *si, unsafe_ctx, refs);
                for arm in arms {
                    collect_refs(&arm.body, si, loop_d, cond_d + 1, refs);
                }
            }
            Stmt::Defer(inner, ..) => {
                collect_refs(std::slice::from_ref(inner), si, loop_d, cond_d, refs);
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
            Stmt::Expr(e, ..) => {
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
            Stmt::If { cond: _, then_branch, else_branch, .. } => {
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
            Stmt::ForEach { body, .. } | Stmt::ForEachMap { body, .. } => {
                set_moves(body, si, loop_d + 1, cond_d, facts);
            }
            Stmt::DoWhile { body, .. } => {
                set_moves(body, si, loop_d + 1, cond_d, facts);
            }
            Stmt::Match { arms, .. } => {
                for arm in arms.iter_mut() {
                    set_moves(&mut arm.body, si, loop_d, cond_d + 1, facts);
                }
            }
            Stmt::Defer(inner, ..) => {
                set_moves(std::slice::from_mut(inner), si, loop_d, cond_d, facts);
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
