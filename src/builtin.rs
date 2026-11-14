// ============================================================================
// builtin.rs —— 自研 asm 后端：Dial 内建函数的表驱动分派
// ============================================================================
//
// LLVM 后端（codegen.rs）对 `Expr::Call(name, args)` 里按名字硬编码分派的内建函数
// 逐条手写实现；本文件把同一套语义整理成一张声明式分派表，供自研后端复用，
// 避免 lowering 里再写一遍 100+ 条 match 分支。
//
// 设计要点（与「三套标量宽度」宪法一致）：
//   * C 运行时内建的参数边界是**另一套 ABI**，不是 Dial 用户函数的 param_ir。
//     例如 C 侧 bool/byte/char 参数一律 int64_t，而 param_ir 会把它们收窄成
//     i32 / i8。因此表项必须**显式**给出每个参数的 Abi，绝不靠 param_ir 猜测。
//   * 返回值同理：C 侧 int64_t / double / void* / void 与 Dial 内部宽度分开记。
//   * 语义层已经做过完整的参数类型校验，这里只负责「把值摆进正确的寄存器组」。
//
// 覆盖范围：转换 / 数学 / 解析 / 断言退出 / 环境时间 / 字符串 / 字节 /
//           数组 / 容器构造与操作 / 文件 IO（失败抛错类见 M3 异常相位）。
// 未覆盖：@asm / simd / sizeof / alignof / offsetof / 高阶函数(map/filter/
//         reduce/sort_by) / 并发(spawn/channel/select) —— 一律明确 Unsupported，
//         绝不静默降级。
// ============================================================================

use crate::ast::Type;
use crate::ir::{AConv, IrType};

/// C 运行时参数通道。`Auto` 仅在确实无法静态确定时使用（按实参推断 + param_ir）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    I64,    // int / bool / byte / char → int64_t（整数寄存器组，8 字节）
    F64,    // float → double（浮点寄存器组）
    F64Ext, // float32 → 先 FpExt 到 double 再传（C 原型是 double）
    F32,    // float32 → float（C 原型是 float，少见）
    I32,    // int32 → int32_t
    Ptr,    // 不透明句柄 / 裸指针 / C 字符串 → void*（整数寄存器组，8 字节）
    Auto,   // 按 expr_dt 推断后走 param_ir（谨慎使用）
}

/// C 返回值的后处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Post {
    None,       // 原样返回
    NeqZero,    // C 返回 0/1（或任意非零）→ 规整成 Dial bool 0/1（LLVM 后端同款）
    EqZero,     // C 返回 0 表示真 → 规整成 bool
}

/// 一条内建的实现形态。
#[derive(Clone, Debug)]
pub enum BKind {
    /// 直接调用一个 C 符号。
    Call {
        sym: &'static str,
        args: &'static [Abi],
        ret_ir: IrType,
        post: Post,
    },
    /// 纯 IR 一元转换（无任何 C 调用）。
    Conv { kind: AConv, to_ir: IrType },
    /// 零指令类型视图切换（char/byte 与 int 同为 I64）。
    Identity,
    /// min / max：按实参类型选择整数或浮点比较 + 三目。
    MinMax { is_min: bool },
    /// abs：整数走条件取负，浮点走 C 的 fabs。
    Abs,
    /// 明确不支持（附一句原因），由调用方转成 Fail-Fast 错误。
    Unsupported(&'static str),
}

#[derive(Clone, Debug)]
pub struct BSpec {
    /// Dial 层返回类型（供 expr_dt / 变量槽 / print 分派使用）。
    pub ret: Type,
    pub kind: BKind,
}

fn call(sym: &'static str, args: &'static [Abi], ret_ir: IrType, ret: Type, post: Post) -> BSpec {
    BSpec { ret, kind: BKind::Call { sym, args, ret_ir, post } }
}
fn c(sym: &'static str, args: &'static [Abi], ret_ir: IrType, ret: Type) -> BSpec {
    call(sym, args, ret_ir, ret, Post::None)
}
fn conv(kind: AConv, to_ir: IrType, ret: Type) -> BSpec {
    BSpec { ret, kind: BKind::Conv { kind, to_ir } }
}
fn ident(ret: Type) -> BSpec {
    BSpec { ret, kind: BKind::Identity }
}
fn unsup(ret: Type, why: &'static str) -> BSpec {
    BSpec { ret, kind: BKind::Unsupported(why) }
}

// 常用参数形状（Rust 会把这些切片常量提升为 'static）。
const A0: &[Abi] = &[];
const A_I: &[Abi] = &[Abi::I64];
const A_FX: &[Abi] = &[Abi::F64Ext];
const A_P: &[Abi] = &[Abi::Ptr];
const A_II: &[Abi] = &[Abi::I64, Abi::I64];
const A_PI: &[Abi] = &[Abi::Ptr, Abi::I64];

/// 内建名 → 规格。命中即表示「这是按名字分派的内建」，用户函数表里不会有同名项
/// （语义层 RESERVED_NAMES 已强制设防）。
pub fn lookup(name: &str) -> Option<BSpec> {
    match name {
        // ---- 编译期/底层原语：明确不支持，绝不静默降级 ----
        "sizeof" | "alignof" | "offsetof" =>
            return Some(unsup(Type::Int, "编译期布局内建暂未接入（后续相位）")),
        "asm" | "asm_intel" | "simd" | "bitcast" | "ptr_cast" =>
            return Some(unsup(Type::Int, "内联汇编 / SIMD / 底层位强转暂未接入（后续相位）")),

        // ---- 零指令类型视图切换（char / byte 与 int 同为 I64） ----
        "char_to_int" | "byte_to_int" => return Some(ident(Type::Int)),
        "int_to_char" => return Some(ident(Type::Char)),

        // ---- 纯 IR 数值转换 ----
        "int_to_float" => return Some(conv(AConv::SiFp, IrType::F64, Type::Float)),
        "float_to_int" => return Some(conv(AConv::FpSi, IrType::I64, Type::Int)),
        "int32_to_int" => return Some(conv(AConv::Sext, IrType::I64, Type::Int)),
        "float_to_float32" => return Some(conv(AConv::FpTrunc, IrType::F32, Type::Float32)),
        "float32_to_float" => return Some(conv(AConv::FpExt, IrType::F64, Type::Float)),

        // ---- 带范围校验的转换（C 侧 Fail-Fast，不静默截断） ----
        "int_to_byte" => return Some(c("dial_int_to_byte", A_I, IrType::I64, Type::Byte)),
        "int_to_int32" => return Some(c("dial_int_to_int32", A_I, IrType::I32, Type::Int32)),

        // ---- 标量 → 字符串（C 侧格式化） ----
        "int_to_string" => return Some(c("dial_int_to_string", A_I, IrType::Handle, Type::String)),
        "float_to_string" => return Some(c("dial_float_to_string", A_FX, IrType::Handle, Type::String)),
        "format_float" => return Some(c("dial_format_float", &[Abi::F64Ext, Abi::I64], IrType::Handle, Type::String)),

        // ---- 严格数值解析 / 显式探测 ----
        "parse_int" => return Some(c("dial_parse_int", A_P, IrType::I64, Type::Int)),
        "parse_float" => return Some(c("dial_parse_float", A_P, IrType::F64, Type::Float)),
        "is_int" => return Some(call("dial_is_int", A_P, IrType::I64, Type::Bool, Post::NeqZero)),
        "is_float" => return Some(call("dial_is_float", A_P, IrType::I64, Type::Bool, Post::NeqZero)),

        // ---- 数学（C 标准库 / libm，符号同名，链接层已带 -lm） ----
        "sqrt" => return Some(c("sqrt", A_FX, IrType::F64, Type::Float)),
        "pow" => return Some(c("pow", &[Abi::F64Ext, Abi::F64Ext], IrType::F64, Type::Float)),
        "floor" => return Some(c("floor", A_FX, IrType::F64, Type::Float)),
        "ceil" => return Some(c("ceil", A_FX, IrType::F64, Type::Float)),
        "round" => return Some(c("round", A_FX, IrType::F64, Type::Float)),
        "abs" => return Some(BSpec { ret: Type::Int, kind: BKind::Abs }),
        "min" => return Some(BSpec { ret: Type::Int, kind: BKind::MinMax { is_min: true } }),
        "max" => return Some(BSpec { ret: Type::Int, kind: BKind::MinMax { is_min: false } }),

        // ---- 退出 ----
        // assert / panic 需要「当前语句行列 + 源文件名」编译期常量，并且 assert 本身
        // 是一条带条件的控制流；首版没有语句位置追踪，明确延后（绝不静默降级成
        // 无诊断的 abort）。exit 是无位置依赖的薄封装，先行接入。
        "exit" => return Some(c("dial_exit", A_I, IrType::Void, Type::Void)),
        "assert" | "panic" =>
            return Some(unsup(Type::Void, "assert / panic 需语句位置追踪（后续相位）")),

        // ---- 环境 / 时间 / 系统（无可变全局状态） ----
        "args" => return Some(c("dial_args", A0, IrType::Handle, Type::Array(Box::new(Type::String)))),
        "now" => return Some(c("dial_now", A0, IrType::F64, Type::Float)),
        "random" => return Some(c("dial_random", A0, IrType::I64, Type::Int)),
        "sleep" => return Some(c("dial_sleep", A_I, IrType::Void, Type::Void)),
        "read_line" => return Some(c("dial_read_line", A0, IrType::Handle, Type::String)),
        "getenv" => return Some(c("dial_getenv", A_P, IrType::Handle, Type::String)),
        "has_env" => return Some(call("dial_has_env", A_P, IrType::I64, Type::Bool, Post::NeqZero)),
        "program_name" => return Some(c("dial_program_name", A0, IrType::Handle, Type::String)),
        "cwd" => return Some(c("dial_cwd", A0, IrType::Handle, Type::String)),
        "date_string" => return Some(c("dial_date_string", A0, IrType::Handle, Type::String)),
        "local_parts" => return Some(c("dial_local_parts", A0, IrType::Handle, Type::Array(Box::new(Type::Int)))),
        "monotonic_ms" => return Some(c("dial_monotonic_ms", A0, IrType::I64, Type::Int)),

        // ---- bytes（二进制缓冲区） ----
        "bytes_new" => return Some(c("dial_bytes_zero", A_I, IrType::Handle, Type::Bytes)),
        "bytes_length" => return Some(c("dial_bytes_length", A_P, IrType::I64, Type::Int)),
        "bytes_get" => return Some(c("dial_bytes_get", A_PI, IrType::I64, Type::Int)),
        "bytes_from_string" => return Some(c("dial_bytes_from_string", A_P, IrType::Handle, Type::Bytes)),
        "bytes_to_string" => return Some(c("dial_bytes_to_string", A_P, IrType::Handle, Type::String)),
        "bytes_from_ptr" => return Some(c("dial_ffi_bytes_from_ptr", A_PI, IrType::Handle, Type::Bytes)),
        "string_from_cstr" => return Some(c("dial_ffi_string_from_cstr", A_P, IrType::Handle, Type::String)),

        // ---- 可恢复异常对象（ARC） ----
        "Error" => return Some(c("dial_error_new", A_P, IrType::Handle, Type::Error)),

        // ---- 容器构造 ----
        "new_map" => return Some(c("dial_map_new", A0, IrType::Handle, Type::Map)),
        "new_set" => return Some(c("dial_map_new", A0, IrType::Handle, Type::Set(Box::new(Type::Int)))),
        "new_kv_map" => return Some(c("dial_kvmap_new", A_II, IrType::Handle, Type::KvMap(Box::new(Type::Int), Box::new(Type::Int)))),

        // 注意：put / get / get_or / contains / remove / keys / values / sort /
        // reverse / join / split / slice / index_of / clone 等在 Dial 里都是**方法**
        // （Expr::Method），由 M2 的方法表处理，不出现在 Expr::Call 里。

        // 文件 IO / with 自动释放是「带失败抛错的 CFG」或「需要 moved 记账」的
        // 专用路径，不走普通内建表：见 lower.rs 的 lower_call 特判与 special_ret。

        // ---- 并发 / 高阶函数：后续相位 ----
        "spawn" | "join" | "detach" | "channel" | "recv" | "try_recv" |
        "recv_timeout" | "move_to" | "thread" | "done" | "closed" |
        "map" | "filter" | "reduce" | "sort_by" | "is_null" | "drop" =>
            return Some(unsup(Type::Int, "并发 / 高阶函数内建暂未接入（后续相位）")),

        _ => return None,
    }
}

/// 仅取内建返回类型（expr_dt 用），不构造完整规格。
pub fn builtin_ret(name: &str) -> Option<Type> {
    lookup(name).map(|b| b.ret)
}

/// 专用路径内建的返回 Dial 类型（不走 lookup：它们需要 CFG / moved 记账）。
pub fn special_ret(name: &str) -> Option<Type> {
    Some(match name {
        "open" => Type::File,
        "close" | "__with_release" => Type::Void,
        "read" | "read_all" | "file_read_line" => Type::String,
        "write" | "file_size" => Type::Int,
        "eof" | "file_exists" | "file_remove" => Type::Bool,
        _ => return None,
    })
}

/// Abi → 传给 lower_expr 的期望 Dial 类型（主要影响字符串字面量 rodata 与
/// 浮点字面量的内部宽度；语义层已保证实参类型合法）。
pub fn abi_hint(a: Abi) -> Type {
    match a {
        Abi::F64 | Abi::F64Ext | Abi::F32 => Type::Float,
        Abi::I32 => Type::Int32,
        Abi::Ptr => Type::String,
        Abi::I64 | Abi::Auto => Type::Int,
    }
}

/// Abi → Call 的 arg_ty / ret_ty 所需的 IrType。
pub fn abi_ir(a: Abi) -> IrType {
    match a {
        Abi::I64 => IrType::I64,
        Abi::F64 | Abi::F64Ext => IrType::F64,
        Abi::F32 => IrType::F32,
        Abi::I32 => IrType::I32,
        Abi::Ptr => IrType::Handle,
        Abi::Auto => IrType::I64,
    }
}

// ============================================================================
// 方法分派表（obj.m(args)）
// ============================================================================
//
// 与 codegen.rs:5503 的 `match (&ot, method)` 一一对应。接收者恒为不透明句柄
// （Abi::Ptr），args 只描述 obj 之后的参数。
//   * writeback：C 侧 COW 返回「可能更新的新指针」，必须写回接收变量槽，
//     且绝不 release 旧值（与 Stmt::AssignIndex 的所有权契约相同）。
//   * elem_arg / elem_ret：数组元素按 C 的 int64 位模式存取，需按元素类型
//     做位模式重解释（float 保留 IEEE754、句柄按指针位模式）。
//   * is_kv：泛型 map<K,V> 走 dial_kvmap_*，键/值按 K/V 静态类型转位模式。

#[derive(Clone, Debug)]
pub struct MSpec {
    pub sym: &'static str,
    pub args: &'static [Abi],
    pub ret: Type,
    pub ret_ir: IrType,
    pub post: Post,
    pub writeback: bool,
    pub elem_arg: Option<usize>,
    pub elem_ret: bool,
    pub is_kv: bool,
    /// set.add：C 侧是 dial_map_put(m, k, 1)，需追加常量 1 作为第三个实参。
    pub extra_one: bool,
}

impl MSpec {
    fn new(sym: &'static str, args: &'static [Abi], ret: Type, ret_ir: IrType) -> Self {
        MSpec { sym, args, ret, ret_ir, post: Post::None, writeback: false,
                elem_arg: None, elem_ret: false, is_kv: false, extra_one: false }
    }
    fn post(mut self, p: Post) -> Self { self.post = p; self }
    fn wb(mut self) -> Self { self.writeback = true; self }
    fn earg(mut self, i: usize) -> Self { self.elem_arg = Some(i); self }
    fn eret(mut self) -> Self { self.elem_ret = true; self }
    fn kv(mut self) -> Self { self.is_kv = true; self }
    fn one(mut self) -> Self { self.extra_one = true; self }
}

const MA0: &[Abi] = &[];
const MA_I: &[Abi] = &[Abi::I64];
const MA_II: &[Abi] = &[Abi::I64, Abi::I64];
const MA_P: &[Abi] = &[Abi::Ptr];
const MA_PP: &[Abi] = &[Abi::Ptr, Abi::Ptr];

fn arr(ret: Type) -> Type { Type::Array(Box::new(ret)) }

/// 方法 → 规格。返回 None 表示「非内建方法」（impl 方法 / 未覆盖构造）。
pub fn lookup_method(recv: &Type, name: &str) -> Option<MSpec> {
    let int = Type::Int;
    let string = Type::String;
    let spec = match recv {
        Type::String => match name {
            "length" | "len" => MSpec::new("dial_string_length", MA0, int.clone(), IrType::I64),
            "concat" => MSpec::new("dial_string_concat", MA_P, string.clone(), IrType::Handle),
            "get" => MSpec::new("dial_string_get", MA_I, int.clone(), IrType::I64),
            "substring" => MSpec::new("dial_string_substring", MA_II, string.clone(), IrType::Handle),
            "clone" => MSpec::new("dial_string_clone", MA0, string.clone(), IrType::Handle),
            "eq" => MSpec::new("dial_string_eq", MA_P, Type::Bool, IrType::I64).post(Post::NeqZero),
            "index_of" => MSpec::new("dial_string_index_of", MA_P, int.clone(), IrType::I64),
            "replace" => MSpec::new("dial_string_replace", MA_PP, string.clone(), IrType::Handle),
            "trim" => MSpec::new("dial_string_trim", MA0, string.clone(), IrType::Handle),
            "split" => MSpec::new("dial_string_split", MA_P, arr(string.clone()), IrType::Handle),
            "to_upper" => MSpec::new("dial_string_to_upper", MA0, string.clone(), IrType::Handle),
            "to_lower" => MSpec::new("dial_string_to_lower", MA0, string.clone(), IrType::Handle),
            "contains" => MSpec::new("dial_string_contains", MA_P, Type::Bool, IrType::I64).post(Post::NeqZero),
            "starts_with" => MSpec::new("dial_string_starts_with", MA_P, Type::Bool, IrType::I64).post(Post::NeqZero),
            "ends_with" => MSpec::new("dial_string_ends_with", MA_P, Type::Bool, IrType::I64).post(Post::NeqZero),
            "char_count" => MSpec::new("dial_string_char_count", MA0, int.clone(), IrType::I64),
            "chars" => MSpec::new("dial_string_chars", MA0, arr(int.clone()), IrType::Handle),
            "char_at" => MSpec::new("dial_string_char_at", MA_I, int.clone(), IrType::I64),
            "char_slice" => MSpec::new("dial_string_char_slice", MA_II, string.clone(), IrType::Handle),
            _ => return None,
        },
        Type::Array(elem) => match name {
            "length" | "len" | "size" => MSpec::new("dial_array_length", MA0, int.clone(), IrType::I64),
            "get" => MSpec::new("dial_array_get", MA_I, (**elem).clone(), IrType::I64).eret(),
            "push" => MSpec::new("dial_array_push", MA_I, recv.clone(), IrType::Handle).earg(0).wb(),
            "set" => MSpec::new("dial_array_set", MA_II, recv.clone(), IrType::Handle).earg(1).wb(),
            "insert" => MSpec::new("dial_array_insert", MA_II, recv.clone(), IrType::Handle).earg(1).wb(),
            "index_of" => MSpec::new("dial_array_index_of", MA_I, int.clone(), IrType::I64).earg(0),
            "clone" => MSpec::new("dial_array_clone", MA0, recv.clone(), IrType::Handle),
            "slice" => MSpec::new("dial_array_slice", MA_II, recv.clone(), IrType::Handle),
            // remove / pop 返回**被删元素**（不是新数组指针），另见 lower_method 特判。
            "sort" => MSpec::new(if **elem == Type::String { "dial_array_sort_string" } else { "dial_array_sort" },
                                 MA0, recv.clone(), IrType::Handle).wb(),
            "reverse" => MSpec::new("dial_array_reverse", MA0, recv.clone(), IrType::Handle).wb(),
            "clear" => MSpec::new("dial_array_clear", MA0, recv.clone(), IrType::Handle).wb(),
            "join" => MSpec::new("dial_array_join", MA_P, string.clone(), IrType::Handle),
            // 高阶方法需要 fnptr thunk，明确不支持（由 lower_method 报 Fail-Fast）。
            "map" | "filter" | "reduce" | "sort_by" => return None,
            _ => return None,
        },
        Type::Map => match name {
            "put" => MSpec::new("dial_map_put", MA_II, recv.clone(), IrType::Handle).wb(),
            "get" => MSpec::new("dial_map_get", MA_I, int.clone(), IrType::I64),
            "get_or" => MSpec::new("dial_map_get_or", MA_II, int.clone(), IrType::I64),
            "contains" => MSpec::new("dial_map_contains", MA_I, Type::Bool, IrType::I64).post(Post::NeqZero),
            "remove" => MSpec::new("dial_map_remove", MA_I, int.clone(), IrType::I64),
            "size" => MSpec::new("dial_map_size", MA0, int.clone(), IrType::I64),
            _ => return None,
        },
        Type::Set(_) => match name {
            "add" => MSpec::new("dial_map_put", MA_I, recv.clone(), IrType::Handle).wb().one(),
            "contains" => MSpec::new("dial_map_contains", MA_I, Type::Bool, IrType::I64).post(Post::NeqZero),
            "remove" => MSpec::new("dial_map_remove", MA_I, int.clone(), IrType::I64),
            "size" => MSpec::new("dial_map_size", MA0, int.clone(), IrType::I64),
            "values" => MSpec::new("dial_map_keys", MA0, arr(int.clone()), IrType::Handle),
            _ => return None,
        },
        Type::KvMap(k, v) => match name {
            "put" => MSpec::new("dial_kvmap_put", MA_II, recv.clone(), IrType::Handle).kv().wb(),
            "get" => MSpec::new("dial_kvmap_get", MA_I, (**v).clone(), IrType::I64).kv().eret(),
            "get_or" => MSpec::new("dial_kvmap_get_or", MA_II, (**v).clone(), IrType::I64).kv().eret(),
            "contains" => MSpec::new("dial_kvmap_contains", MA_I, Type::Bool, IrType::I64).kv().post(Post::NeqZero),
            "remove" => MSpec::new("dial_kvmap_remove", MA_I, (**v).clone(), IrType::I64).kv().eret(),
            "size" => MSpec::new("dial_kvmap_size", MA0, int.clone(), IrType::I64).kv(),
            "keys" => MSpec::new("dial_kvmap_keys", MA0, arr((**k).clone()), IrType::Handle).kv(),
            "values" => MSpec::new("dial_kvmap_values", MA0, arr((**v).clone()), IrType::Handle).kv(),
            _ => return None,
        },
        Type::Bytes => match name {
            "as_ptr" => MSpec::new("dial_ffi_bytes_data", MA0, Type::RawPtr(Box::new(Type::Byte)), IrType::Ptr),
            "length" | "len" => MSpec::new("dial_bytes_length", MA0, int.clone(), IrType::I64),
            "get" => MSpec::new("dial_bytes_get", MA_I, int.clone(), IrType::I64),
            _ => return None,
        },
        Type::Error => match name {
            "msg" => MSpec::new("dial_error_msg", MA0, string.clone(), IrType::Handle),
            _ => return None,
        },
        _ => return None,
    };
    Some(spec)
}

/// expr_dt 用：方法的返回 Dial 类型（不含接收者上下文）。
pub fn method_ret(recv: &Type, name: &str) -> Option<Type> {
    lookup_method(recv, name).map(|m| m.ret)
}
