//! LLVM IR 代码生成（基于 inkwell 0.11，目标 LLVM 18）。
//!
//! 架构核心：
//! - Dial 编译器**只翻译 AST → 对这些 C 函数的调用**；自身不做容器/字符串的底层实现。
//! - String / Array 在 IR 中一律是「不透明指针」i8*；用户永远无法直接访问底层内存布局。
//! - 引用计数 / 写时复制完全由 C Runtime（runtime/ + stdlib/）掌控。
//! - 数据流分析（Sema 阶段写入的 move_hint）在此决定：移动语义（跳过 retain）还是共享（retain）。
//! - `.o` 由 inkwell 在内存中构建 IR 后 `write_to_file` 直接 emit，不导出 .ll 文本。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use inkwell::context::Context;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, StructType};
use inkwell::basic_block::BasicBlock;
use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FloatValue, FunctionValue, IntValue, PointerValue,
};
use inkwell::AddressSpace;
use inkwell::FloatPredicate;
use inkwell::IntPredicate;
use inkwell::OptimizationLevel;

use crate::ast::*;
use crate::error::{DialcError, Result};
use crate::semantic::{type_of, FuncInfo, StructTable};

type Local<'ctx> = (Type, (bool, PointerValue<'ctx>));
type CgScopes<'ctx> = Vec<HashMap<String, Local<'ctx>>>;

/*
 * ============================================================================
 *  Fail-Fast 安全等级（与 C Runtime 的 DIAL_SAFETY 一一对应）
 * ============================================================================
 *  Debug（默认）  ：全检查。越界/除零 → llvm.debugtrap（x86 int3，调试器可当断点捕获）
 *  ReleaseSafe    ：保留防 UB 的检查，陷阱改用 llvm.trap（x86 ud2，指令更短）
 *  ReleaseFast    ：不插入任何检查，越界/除零即 UB（与 C 一致），只为极限性能
 *
 *  为什么除法也要检查：LLVM 的 sdiv 在除数为 0 时结果是 **poison**（未定义行为），
 *  优化器有权把后续代码整段删掉。这不是"崩溃"，而是更坏——程序会静默跑出错误结果。
 * ============================================================================
 */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SafetyMode {
    Debug,
    ReleaseSafe,
    ReleaseFast,
}

impl SafetyMode {
    /// 对应 C Runtime 的 -DDIAL_SAFETY=N
    pub fn as_c_macro(self) -> &'static str {
        match self {
            SafetyMode::Debug => "2",
            SafetyMode::ReleaseSafe => "1",
            SafetyMode::ReleaseFast => "0",
        }
    }
}

/// 代码生成选项。除 Fail-Fast 等级外，还携带「是否启用终极优化」的信息，
/// 用以打开 LLVM 的 O3 + SIMD 自动向量化 + 循环展开，以及是否额外导出
/// LLVM bitcode（供 LTO / PGO 流水线消费）。
pub struct CgOpts {
    pub safety: SafetyMode,
    /// true（仅 --fast 模式）：用 `OptimizationLevel::Aggressive`（等价于 O3）生成机器码，
    /// 并显式打开宿主 SIMD 特征（x86 的 AVX2 / ARM64 的 NEON），让 LLVM 的循环向量化器
    /// 与循环展开器全力工作。
    pub aggressive: bool,
    /// 若 Some(path)：在写出原生 .o 之外，把 LLVM bitcode 另存到该路径。
    /// LTO / PGO 需要它——只有 bitcode 里才携带可被链接器重新打散的中间表示，
    /// 原生 .o 无法被 LTO 跨越。
    pub bc_out: Option<std::path::PathBuf>,
}

pub struct CodeGen<'ctx> {
    context: &'ctx Context,
    module: inkwell::module::Module<'ctx>,
    builder: inkwell::builder::Builder<'ctx>,
    cfuns: HashMap<String, FunctionValue<'ctx>>,
    struct_types: HashMap<String, StructType<'ctx>>,
    str_id: std::cell::Cell<u32>,
    /// 已被“移动”的局部变量**栈槽指针**（转移所有权，跳过作用域结束时的 release）。
    ///
    /// 按**槽位指针**而非变量名记账：变量名会跨作用域/跨函数重名，按名字记账会让
    /// 同名变量互相误伤（该 release 的被跳过 -> 泄漏）。槽位指针由 alloca 唯一确定，
    /// 天然精确。`move_to` 是用户日常 API，这一位必须是准的。
    moved: RefCell<std::collections::HashSet<PointerValue<'ctx>>>,
    /// 受保护的局部槽位：**绝不能被 emit_throw 释放**。
    ///
    /// 语义：当异常在 `try` 体内抛出、被本帧的 `catch` 捕获时，`catch` 体可能引用
    /// `try` **之前**声明的外层变量（例如 `let path = ...; try { open(path) } catch { print(path) }`）。
    /// 这些外层变量不在 `try` 体的作用域内，但当前帧的 `emit_throw` 会遍历本帧所有活跃作用域——
    /// 若不排除它们，就会被误释放，待 `catch` 引用时变成悬垂指针（UAF）。
    ///
    /// 机制：`try` 入口（尚未 push 其体作用域）时，把**此刻所有外层作用域的槽位**登记进本集合，
    /// 并在 `try` 正常结束（merge）或被捕获（catch）后从集合中移除。这样：
    ///   - 同帧抛出：emit_throw 释放 `try` 体作用域的局部，跳过外层（被保护），`catch` 引用安全；
    ///   - 跨帧抛出（子函数内 throw）：子函数自身的 `protected` 为空，emit_throw 释放子帧全部局部——
    ///     父帧的 `try` 作用域深度与子帧作用域编号不互通，因此「按子帧自身深度释放全部」才是正确的
    ///     （子帧整体随 unwinding 销毁），绝不能用父帧记录的作用域深度去索引子帧的 scopes。
    protected: RefCell<std::collections::HashSet<PointerValue<'ctx>>>,
    /// 循环跳转目标栈：(continue 目标块, break 目标块)。
    /// for 的 continue 目标是「自增块」而非条件块——否则 continue 会跳过 i++ 造成死循环。
    loop_targets: RefCell<Vec<(BasicBlock<'ctx>, BasicBlock<'ctx>)>>,
    /// Fail-Fast 安全等级：决定是否在 IR 里插入边界/除零检查与 trap。
    safety: SafetyMode,
}

/// 编译入口：接收校验后的 AST、函数签名表、结构体表，生成并写出目标文件 `.o`。
pub fn compile(
    program: &Program,
    functions: &HashMap<String, FuncInfo>,
    structs: &StructTable,
    out_path: &Path,
    opts: &CgOpts,
    externs: &[(String, FuncInfo)],
) -> Result<()> {
    let context = Context::create();
    let module = context.create_module("dialc");

    let mut cg = CodeGen {
        context: &context,
        module,
        builder: context.create_builder(),
        cfuns: HashMap::new(),
        struct_types: HashMap::new(),
        str_id: std::cell::Cell::new(0),
        moved: RefCell::new(std::collections::HashSet::new()),
        // 注：HashSet<PointerValue> 无需按函数清空——每条 alloca 的 PointerValue 全局唯一，
        // 不同函数的同名变量不会互串。
        protected: RefCell::new(std::collections::HashSet::new()),
        loop_targets: RefCell::new(Vec::new()),
        safety: opts.safety,
    };

    cg.build_struct_types(program);
    cg.declare_stdlib();

    // 1) 声明所有 Dial 函数
    let mut func_values: HashMap<String, FunctionValue> = HashMap::new();
    for f in &program.functions {
        let fv = cg.declare_dial_function(f, functions);
        func_values.insert(f.name.clone(), fv);
    }

    // 1.5) 声明 import 引入的包外部函数（v0.92）。
    //
    // 它们**只有声明、没有函数体**——函数体在包提供的预编译 `.a` 里，
    // 由链接期（见 main.rs 的 link）把那份机器码缝进来。
    // 对 codegen 而言与本地函数毫无区别：调用点照常发射 `call` 指令。
    for (name, info) in externs {
        let fv = cg.declare_extern_function(name, info);
        func_values.insert(name.clone(), fv);
    }

    // 2) 生成函数体
    for f in &program.functions {
        cg.gen_function(f, &func_values, functions, structs)?;
    }

    // 3) 写出目标文件
    //
    // IR 结构自检：块缺少终结指令这类错误，LLVM 往往要到优化阶段的
    // BranchProbabilityInfo 才以**段错误**的形式炸出来（而不是给出可读的诊断）。
    // 这里先 verify 一遍，把问题挡在编译期、报成人能读懂的文字。
    // 调试时设 DIALC_DUMP_IR=1 可把完整 IR 打到 stderr。
    if std::env::var("DIALC_DUMP_IR").is_ok() {
        eprintln!("{}", cg.module.print_to_string().to_string());
    }
    if let Err(e) = cg.module.verify() {
        eprintln!("[dialc] IR 验证失败: {}", e);
    }
    cg.emit_object(out_path, opts)?;
    Ok(())
}

impl<'ctx> CodeGen<'ctx> {
    /*
     * ------------------------------------------------------------------
     *  Fail-Fast：陷阱发射与运行时检查插入（代码纪律 §20.4：直白，无框架）
     * ------------------------------------------------------------------
     */

    /// 发射一条陷阱指令并标记不可达。LLVM 会把 intrinsic 翻译成平台非法指令：
    /// `llvm.debugtrap` → x86 `int3`（可被调试器当作断点捕获）；
    /// `llvm.trap`      → x86 `ud2` / ARM `udf`（指令更短，ReleaseSafe 用）。
    fn emit_trap(&self) {
        let name = if self.safety == SafetyMode::Debug {
            "llvm.debugtrap"
        } else {
            "llvm.trap"
        };
        let f = self.module.get_function(name).unwrap_or_else(|| {
            let ty = self.context.void_type().fn_type(&[], false);
            self.module.add_function(name, ty, None)
        });
        self.builder.build_call(f, &[], "").unwrap();
        self.builder.build_unreachable().unwrap();
    }

    /// ReleaseFast 下不插入任何检查。
    fn checks_enabled(&self) -> bool {
        self.safety != SafetyMode::ReleaseFast
    }

    /// 下标边界检查：`0 <= idx < len` 不成立则 trap。
    ///
    /// 用**无符号**比较一次搞定两个条件：负数 idx 的位模式是巨大无符号数，
    /// 必然 `>= len`，无需再单独和 0 比较。
    fn emit_bounds_check(
        &self,
        function: FunctionValue<'ctx>,
        len: IntValue<'ctx>,
        idx: IntValue<'ctx>,
    ) {
        if !self.checks_enabled() {
            return;
        }
        let ok = self
            .builder
            .build_int_compare(IntPredicate::ULT, idx, len, "bchk")
            .unwrap();
        let ok_bb = self.context.append_basic_block(function, "bounds.ok");
        let bad_bb = self.context.append_basic_block(function, "bounds.fail");
        self.builder
            .build_conditional_branch(ok, ok_bb, bad_bb)
            .unwrap();
        self.builder.position_at_end(bad_bb);
        self.emit_trap();
        self.builder.position_at_end(ok_bb);
    }

    /// 整数除零检查：除数为 0 时 trap。
    ///
    /// 没有这条检查，`sdiv` / `srem` 的结果在 LLVM 里是 poison（UB），
    /// 优化器有权据此把后续任何代码当成不可达删掉——比崩溃危险得多。
    fn emit_div_zero_check(&self, function: FunctionValue<'ctx>, divisor: IntValue<'ctx>) {
        if !self.checks_enabled() {
            return;
        }
        let zero = self.context.i64_type().const_int(0, false);
        let ok = self
            .builder
            .build_int_compare(IntPredicate::NE, divisor, zero, "dchk")
            .unwrap();
        let ok_bb = self.context.append_basic_block(function, "div.ok");
        let bad_bb = self.context.append_basic_block(function, "div.fail");
        self.builder
            .build_conditional_branch(ok, ok_bb, bad_bb)
            .unwrap();
        self.builder.position_at_end(bad_bb);
        self.emit_trap();
        self.builder.position_at_end(ok_bb);
    }

    fn i8ptr(&self) -> inkwell::types::PointerType<'ctx> {
        self.context.ptr_type(AddressSpace::default())
    }

    /// 构建用户结构体的 LLVM 命名结构体类型（先建 opaque，再补 body 以支持互相引用）。
    fn build_struct_types(&mut self, program: &Program) {
        for sd in &program.structs {
            self.struct_types
                .insert(sd.name.clone(), self.context.opaque_struct_type(&sd.name));
        }
        for sd in &program.structs {
            let fields: Vec<BasicTypeEnum> = sd
                .fields
                .iter()
                .map(|(_, t)| self.dialc_type_to_llvm(t))
                .collect();
            self.struct_types[&sd.name].set_body(&fields, false);
        }
    }

    /// 声明一个返回值类型为 ret 的 C 函数。
    fn decl_func(
        &mut self,
        name: &str,
        ret: BasicTypeEnum<'ctx>,
        params: &[BasicMetadataTypeEnum<'ctx>],
    ) {
        let ft = ret.fn_type(params, false);
        self.cfuns
            .insert(name.to_string(), self.module.add_function(name, ft, None));
    }

    /// 声明一个返回 void 的 C 函数。
    fn decl_void(&mut self, name: &str, params: &[BasicMetadataTypeEnum<'ctx>]) {
        let ft = self.context.void_type().fn_type(params, false);
        self.cfuns
            .insert(name.to_string(), self.module.add_function(name, ft, None));
    }

    /// 声明全部 C 标准库 / Runtime 导出函数（不透明指针契约）。
    fn declare_stdlib(&mut self) {
        let i8b: BasicTypeEnum = self.i8ptr().into();
        let i64b: BasicTypeEnum = self.context.i64_type().into();
        let f64b: BasicTypeEnum = self.context.f64_type().into();

        // String
        self.decl_func("dial_string_new", i8b, &[i8b.into()]);
        self.decl_func("dial_string_length", i64b, &[i8b.into()]);
        self.decl_func("dial_string_concat", i8b, &[i8b.into(), i8b.into()]);
        self.decl_func("dial_string_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_string_eq", i64b, &[i8b.into(), i8b.into()]);
        self.decl_func(
            "dial_string_substring",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_func("dial_string_clone", i8b, &[i8b.into()]);
        self.decl_void("dial_string_retain", &[i8b.into()]);
        self.decl_void("dial_string_release", &[i8b.into()]);
        self.decl_void("dial_print_string", &[i8b.into()]);
        // Array<int>
        self.decl_func("dial_array_new", i8b, &[]);
        self.decl_func("dial_array_push", i8b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_array_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func(
            "dial_array_set",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_func("dial_array_length", i64b, &[i8b.into()]);
        self.decl_func("dial_array_clone", i8b, &[i8b.into()]);
        self.decl_void("dial_array_retain", &[i8b.into()]);
        self.decl_void("dial_array_release", &[i8b.into()]);
        // Map (int -> int, ARC)
        self.decl_func("dial_map_new", i8b, &[]);
        self.decl_func(
            "dial_map_put",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_func("dial_map_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_contains", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_remove", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_size", i64b, &[i8b.into()]);
        self.decl_void("dial_map_retain", &[i8b.into()]);
        self.decl_void("dial_map_release", &[i8b.into()]);
        // IO / 打印
        self.decl_void("dial_print_int", &[i64b.into()]);
        self.decl_void("dial_print_float", &[f64b.into()]);
        self.decl_void("dial_print_bool", &[i64b.into()]);
        // char 打印（v0.92）：打印字符本身而非码点数字，**末尾带换行**。
        self.decl_void("dial_print_char", &[i64b.into()]);
        // 无换行输出（v0.98.2）：连续输出的基本积木。
        //   dial_print_raw     原样输出字符串（按字节长度，不补 '\n'）
        //   dial_write_char    输出单个字符（不补 '\n'）
        //   dial_print_raw_*   标量版本（栈缓冲，零堆分配）
        self.decl_void("dial_print_raw", &[i8b.into()]);
        self.decl_void("dial_write_char", &[i64b.into()]);
        self.decl_void("dial_print_raw_int", &[i64b.into()]);
        self.decl_void("dial_print_raw_float", &[f64b.into()]);
        self.decl_void("dial_print_raw_bool", &[i64b.into()]);
        // 类型 -> 字符串（CVE-2024-DIAL-006 新增）：支撑 `print("x=" + str(n))` 这类拼接。
        self.decl_func("dial_int_to_string", i8b, &[i64b.into()]);
        self.decl_func("dial_float_to_string", i8b, &[f64b.into()]);
        // char -> 字符串（v0.98.2）：支撑 `str(c)` 与 `str(c) + "suffix"`。
        self.decl_func("dial_char_to_string", i8b, &[i64b.into()]);
        // bytes（二进制字节缓冲区，v0.98.2）：布局与 string 相同，但走**自己的**函数族，
        // 以保证「bytes 与 string 是不同类型」的边界在运行时也被守住。
        self.decl_func("dial_bytes_new", i8b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_bytes_zero", i8b, &[i64b.into()]);
        self.decl_func("dial_bytes_length", i64b, &[i8b.into()]);
        self.decl_func("dial_bytes_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_bytes_clone", i8b, &[i8b.into()]);
        self.decl_func("dial_bytes_from_string", i8b, &[i8b.into()]);
        self.decl_func("dial_bytes_to_string", i8b, &[i8b.into()]);
        // int <-> byte（v0.98.2）：byte_to_int 零指令（编译器侧直传），int_to_byte 带范围校验。
        self.decl_func("dial_int_to_byte", i64b, &[i64b.into()]);
        self.decl_func("dial_byte_to_string", i8b, &[i64b.into()]);
        self.decl_void("dial_bytes_retain", &[i8b.into()]);
        self.decl_void("dial_bytes_release", &[i8b.into()]);
        self.decl_func("dial_read_line", i8b, &[]);
        // 文件 IO：file 句柄是不透明的 FILE* 封装，显式 open/close，不受 ARC 管理。
        self.decl_func("dial_file_open", i8b, &[i8b.into(), i8b.into()]);
        self.decl_func("dial_file_read", i8b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_file_write", i64b, &[i8b.into(), i8b.into()]);
        self.decl_void("dial_file_close", &[i8b.into()]);
        // 可恢复异常（v0.7）：Error 是带引用计数的 ARC 类型（与 string/map 同等对待）。
        self.decl_func("dial_error_new", i8b, &[i8b.into()]);
        self.decl_func("dial_error_msg", i8b, &[i8b.into()]);
        self.decl_void("dial_error_retain", &[i8b.into()]);
        self.decl_void("dial_error_release", &[i8b.into()]);
        // 异常缓冲区（jmp_buf 容器）：对 IR 仅暴露 i8*。
        self.decl_func("dial_exbuf_new", i8b, &[]);
        self.decl_func("dial_exbuf_env_ptr", i8b, &[i8b.into()]);
        self.decl_void("dial_throw_to", &[i8b.into(), i8b.into()]);
        self.decl_func("dial_exbuf_err", i8b, &[i8b.into()]);
        self.decl_func("dial_exbuf_take_err", i8b, &[i8b.into()]);
        self.decl_void("dial_exbuf_free", &[i8b.into()]);
        // 运行时异常栈（跨函数 unwinding）：try 压栈 / catch·merge 弹栈 / throw 取栈顶。
        self.decl_void("dial_exc_push", &[i8b.into()]);
        self.decl_void("dial_exc_pop", &[]);
        self.decl_func("dial_exc_top", i8b, &[]);
        // DialString -> const char*：把不透明字符串句柄转成真正的 C 字符串（跳过 8 字节头部）。
        // 凡是想把 Dial 的 string 交给**期望 const char* 的 C 函数**（例如 dial_panic），
        // 必须先经过这一步。直接把 DialString 当 char* 传，会把结构体头部字节当成文本打印
        // （未捕获异常曾因此只打印出一个数字，而不是错误消息）。
        self.decl_func("dial_string_cstr", i8b, &[i8b.into()]);
        self.decl_void("dial_panic", &[i8b.into()]);

        // setjmp **必须**由「包含 try 的那个函数」亲自调用（IR 侧直接发起）：
        // jmp_buf 里存的是调用点的栈指针，若包在 C 助手函数里、助手一返回，
        // 再 longjmp 就是跳进已死的栈帧（UB，实测当场崩溃）。
        // 因此这里直连 libc setjmp，并打上 returns_twice 属性——否则优化器会误判控制流，
        // 认为它只返回一次，进而破坏异常跳转。（longjmp 由 dial_throw_to 在 C 侧发起。）
        let setjmp_ty = self.context.i32_type().fn_type(&[self.i8ptr().into()], false);
        let setjmp_fn = self.module.add_function("setjmp", setjmp_ty, None);
        let rt_kind = inkwell::attributes::Attribute::get_named_enum_kind_id("returns_twice");
        setjmp_fn.add_attribute(
            inkwell::attributes::AttributeLoc::Function,
            self.context.create_enum_attribute(rt_kind, 0),
        );
        self.cfuns.insert("setjmp".to_string(), setjmp_fn);

        // ---- 多线程并发（v0.8）：线程与通道 ----
        // 线程：argcell（每槽 8 字节）+ 统一签名 void(*)(void*) 的入口。
        self.decl_func("dial_thread_argcell", i8b, &[self.context.i64_type().into()]);
        self.decl_func(
            "dial_thread_new",
            i8b,
            &[i8b.into(), i8b.into()], // (DialThreadFn thunk, void* argcell)
        );
        self.decl_void("dial_thread_join", &[i8b.into()]);
        self.decl_void("dial_thread_detach", &[i8b.into()]);
        // 通道：send/recv 只搬运指针（所有权移交），retain/release 走原子引用计数。
        self.decl_func("dial_channel_new", i8b, &[self.context.i64_type().into(), i8b.into()]);
        self.decl_void("dial_channel_send", &[i8b.into(), i8b.into()]);
        self.decl_func("dial_channel_recv", i8b, &[i8b.into()]);
        self.decl_void("dial_channel_retain", &[i8b.into()]);
        self.decl_void("dial_channel_release", &[i8b.into()]);
        // 通用 ARC 释放（供通道元素析构器使用，CVE-2024-DIAL-007）。
        self.decl_void("dial_rc_release", &[i8b.into()]);
        // v0.81 通道收尾：关闭 / 状态查询 / 非阻塞取出
        self.decl_func("dial_channel_try_recv", i8b, &[i8b.into()]);
        self.decl_void("dial_channel_close", &[i8b.into()]);
        // v0.9：带超时的接收（超时由 C 侧的 pthread_cond_timedwait 实现）
        self.decl_func(
            "dial_channel_recv_timeout",
            i8b,
            &[i8b.into(), self.context.i64_type().into()],
        );
        // v0.91 多路复用：全局事件广播 + 选择器对象
        self.decl_func("dial_chan_epoch", self.context.i64_type().into(), &[]);
        self.decl_void("dial_chan_wait", &[self.context.i64_type().into()]);
        self.decl_func("dial_sel_new", i8b, &[]);
        self.decl_void("dial_sel_add", &[i8b.into(), i8b.into()]);
        self.decl_func("dial_sel_pick", self.context.i64_type().into(), &[i8b.into()]);
        self.decl_func(
            "dial_sel_recv",
            i8b,
            &[i8b.into(), self.context.i64_type().into()],
        );
        self.decl_void("dial_sel_free", &[i8b.into()]);
        self.decl_func(
            "dial_channel_closed",
            self.context.i64_type().into(),
            &[i8b.into()],
        );
        self.decl_func("dial_channel_len", self.context.i64_type().into(), &[i8b.into()]);
        self.decl_func("dial_channel_done", self.context.i64_type().into(), &[i8b.into()]);
    }

    /// 声明一个由 `import` 引入的包外部函数（只有签名，无函数体）。
    ///
    /// 与 C 侧 FFI 函数（declare_stdlib）用的是同一套机制：LLVM 的
    /// **外部链接声明**——模块里只出现 `declare`，定义由链接器从 `.a` 中解析。
    fn declare_extern_function(&self, name: &str, info: &FuncInfo) -> FunctionValue<'ctx> {
        let param_tys: Vec<BasicMetadataTypeEnum> = info
            .params
            .iter()
            .map(|t| self.dialc_type_to_llvm(t).into())
            .collect();
        let ft = if info.ret == Type::Void {
            self.context.void_type().fn_type(&param_tys, false)
        } else {
            self.dialc_type_to_llvm(&info.ret).fn_type(&param_tys, false)
        };
        self.module.add_function(name, ft, None)
    }

    fn declare_dial_function(
        &self,
        f: &Func,
        functions: &HashMap<String, FuncInfo>,
    ) -> FunctionValue<'ctx> {
        let info = &functions[&f.name];
        let param_tys: Vec<BasicMetadataTypeEnum> = f
            .params
            .iter()
            .map(|(_, t)| self.dialc_type_to_llvm(t).into())
            .collect();
        let ft = if f.ret == Type::Void {
            self.context.void_type().fn_type(&param_tys, false)
        } else if f.name == "main" && f.ret == Type::Int {
            // main 走 C ABI：返回 int (i32)
            self.context.i32_type().fn_type(&param_tys, false)
        } else {
            self.dialc_type_to_llvm(&f.ret).fn_type(&param_tys, false)
        };
        let _ = info;
        self.module.add_function(&f.name, ft, None)
    }

    fn dialc_type_to_llvm(&self, t: &Type) -> BasicTypeEnum<'ctx> {
        match *t {
            Type::Void => self.context.i64_type().into(),
            Type::Int => self.context.i64_type().into(),
            Type::Float => self.context.f64_type().into(),
            Type::Bool => self.context.bool_type().into(),
            // char 与 int 同为 i64：它就是一个码点数值，只是**类型**被独立出来
            // 以拦住「把字符当数字做算术」。表示层不付出任何额外代价。
            Type::Char => self.context.i64_type().into(),
            // byte 同为 i64：一个 8 位无符号值的存储形式（0~255，运行时校验范围）。
            Type::Byte => self.context.i64_type().into(),
            Type::String
            | Type::Bytes
            | Type::Array(_)
            | Type::Map
            | Type::File
            | Type::Error
            | Type::Thread
            | Type::Channel(_)
            | Type::Opaque(_) => self.i8ptr().into(),
            Type::Struct(ref n) => (*self
                .struct_types
                .get(n)
                .expect("结构体类型应在 build_struct_types 中已注册"))
            .into(),
        }
    }

    fn to_float(&self, v: BasicValueEnum<'ctx>) -> FloatValue<'ctx> {
        if v.is_float_value() {
            v.into_float_value()
        } else {
            self.builder
                .build_signed_int_to_float(v.into_int_value(), self.context.f64_type(), "itof")
                .unwrap()
        }
    }

    /// 把值强制转换为 i64：float -> i64 用 fptosi；bool(i1) -> i64 用 zext；int 原样。
    fn to_i64(&self, v: BasicValueEnum<'ctx>, t: &Type) -> IntValue<'ctx> {
        if *t == Type::Float {
            self.builder
                .build_float_to_signed_int(v.into_float_value(), self.context.i64_type(), "ftoi")
                .unwrap()
        } else if *t == Type::Bool {
            self.builder
                .build_int_z_extend(v.into_int_value(), self.context.i64_type(), "b2i")
                .unwrap()
        } else {
            v.into_int_value()
        }
    }

    /// 把值规整到目标类型。
    ///
    /// 在“零隐式转换铁律”下，类型检查阶段已保证 `from == to`，因此这里恒为恒等返回。
    /// 任何 int↔float 的转换都必须经源码中的内置函数 `int_to_float` / `float_to_int`，
    /// 编译器绝不在背后偷偷做 sitofp / fptosi。保留此函数为显式关卡，杜绝隐性转换死灰复燃。
    fn coerce(
        &self,
        v: BasicValueEnum<'ctx>,
        _from: &Type,
        _to: &Type,
    ) -> BasicValueEnum<'ctx> {
        v
    }

    fn default_value(&self, t: &Type) -> BasicValueEnum<'ctx> {
        match *t {
            Type::Int => self.context.i64_type().const_int(0, false).into(),
            Type::Float => self.context.f64_type().const_float(0.0).into(),
            Type::Bool => self.context.bool_type().const_int(0, false).into(),
            Type::Char => self.context.i64_type().const_int(0, false).into(),
            Type::Byte => self.context.i64_type().const_int(0, false).into(),
            Type::String
            | Type::Bytes
            | Type::Array(_)
            | Type::Map
            | Type::File
            | Type::Error
            | Type::Thread
            | Type::Channel(_)
            | Type::Opaque(_) => self.i8ptr().const_null().into(),
            Type::Struct(_) => {
                // 结构体默认值：对 v0.2 仅用于 main 收尾，构造一个零初始化聚合
                let bte = self.dialc_type_to_llvm(t);
                // 用 alloca + store zero 过于复杂；这里仅支持 int/float/bool 默认，结构体路径不触发
                let _ = bte;
                self.context.i64_type().const_int(0, false).into()
            }
            Type::Void => self.context.i64_type().const_int(0, false).into(),
        }
    }

    fn is_terminated(&self) -> bool {
        if let Some(bb) = self.builder.get_insert_block() {
            bb.get_terminator().is_some()
        } else {
            false
        }
    }

    fn global_string(&self, name: &str, value: &str) -> PointerValue<'ctx> {
        let value = format!("{}\0", value);
        let arr_ty = self.context.i8_type().array_type(value.len() as u32);
        let g = self.module.add_global(arr_ty, None, name);
        let const_arr = self.context.const_string(value.as_bytes(), false);
        g.set_initializer(&const_arr);
        g.set_constant(true);
        self.builder
            .build_pointer_cast(g.as_pointer_value(), self.i8ptr(), "strcast")
            .unwrap()
    }

    fn next_str_id(&self) -> u32 {
        let n = self.str_id.get();
        self.str_id.set(n + 1);
        n
    }

    /// 解析 `/proc/cpuinfo` 的 flags 行（x86 的 feature 列表），返回以空格分隔的标志集合。
    /// 非 Linux 或读取失败时返回空集，调用方据此退回默认特征。
    fn read_cpu_flags() -> std::collections::HashSet<String> {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in text.lines() {
                if line.starts_with("flags") || line.starts_with("Features") {
                    if let Some(idx) = line.find(':') {
                        for tok in line[idx + 1..].split_whitespace() {
                            set.insert(tok.to_string());
                        }
                    }
                }
            }
        }
        set
    }

    /// 当前宿主架构对应的 LLVM CPU 名（原生构建时即目标 CPU）。
    ///
    /// 为让 --fast 拉满 SIMD，不再写死 `"x86-64"`（那只会给 SSE2），而是按宿主实测的
    /// 指令集挑对应的 **x86-64 微架构级别**（glibc 2.26+ 起 LLVM/ GCC 都认这组名字）：
    ///   - avx512f → `x86-64-v4`（AVX-512，一次吃 8 个 int64）
    ///   - avx2    → `x86-64-v3`（AVX2 + FMA，一次吃 4 个 int64）
    ///   - sse4.2  → `x86-64-v2`
    ///   - 其余    → `x86-64`（仅 SSE2）
    /// 这些级别都已在 cpuinfo 里实测存在，因此生成的二进制在本机必然可跑（不会 SIGILL）。
    /// aarch64 上 NEON 为标配，用 `generic` 即可；其余架构留空让 LLVM 自选默认 CPU。
    fn host_cpu() -> &'static str {
        match std::env::consts::ARCH {
            "x86_64" => {
                let f = Self::read_cpu_flags();
                if f.contains("avx512f") {
                    "x86-64-v4"
                } else if f.contains("avx2") {
                    "x86-64-v3"
                } else if f.contains("sse4_2") {
                    "x86-64-v2"
                } else {
                    "x86-64"
                }
            }
            "aarch64" => "generic",
            _ => "",
        }
    }

    fn emit_object(&self, out: &Path, opts: &CgOpts) -> Result<()> {
        Target::initialize_all(&InitializationConfig::default());
        let triple = TargetMachine::get_default_triple();
        let target = Target::from_triple(&triple)
            .map_err(|e| DialcError::new(format!("LLVM 目标平台错误: {:?}", e), 0, 0))?;
        // --fast 模式：拉满 LLVM 优化。Aggressive 即 O3，会打开循环向量化器与循环展开器；
        // 再叠加宿主 SIMD 特征（x86 的 AVX2 / ARM64 的 NEON），让纯内存计算（如数组求和）
        // 一次直接处理 4~8 个整数。其余模式保持 Default（O2），优先编译速度与调试体验。
        let opt_level = if opts.aggressive {
            OptimizationLevel::Aggressive
        } else {
            OptimizationLevel::Default
        };
        let features = if opts.aggressive {
            Self::host_simd_features()
        } else {
            String::new()
        };
        let tm = target
            .create_target_machine(
                &triple,
                Self::host_cpu(),
                &features,
                opt_level,
                RelocMode::Default,
                CodeModel::Default,
            )
            .ok_or_else(|| DialcError::new("创建目标机失败（LLVM 未支持当前平台？）", 0, 0))?;
        tm.write_to_file(&self.module, FileType::Object, out)
            .map_err(|e| DialcError::new(format!("生成目标文件失败: {:?}", e), 0, 0))?;
        // LTO / PGO 需要 bitcode：原生 .o 不含可被链接器重排的中间表示，必须另存 .bc。
        if let Some(bc) = &opts.bc_out {
            if !self.module.write_bitcode_to_path(bc) {
                return Err(DialcError::new(
                    format!("导出 LLVM bitcode 失败: {}", bc.display()),
                    0,
                    0,
                ));
            }
        }
        Ok(())
    }

    /// 探测宿主 SIMD 特征串（`+feature` 逗号分隔），供 LLVM 目标机附加启用。
    ///
    /// 在 x86_64 上，CPU 微架构级别（`host_cpu` 里的 `x86-64-vN`）已经把对应指令集设为基线，
    /// 这里通常留空即可；仅在 aarch64 上显式补 `+neon` 让向量化器放心使用 Advanced SIMD。
    /// 非 Linux / 探测失败：返回空（退回默认特征，仍可被 O3 向量化，只是不强制宽向量）。
    fn host_simd_features() -> String {
        match std::env::consts::ARCH {
            "aarch64" => "+neon".to_string(),
            _ => String::new(),
        }
    }

    // ---- 调用 C 函数 ----

    fn call_c(&self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) -> BasicValueEnum<'ctx> {
        let fv = self.cfuns[name];
        self.builder
            .build_call(fv, args, name)
            .unwrap()
            .try_as_basic_value()
            .basic()
            .unwrap()
    }

    fn call_c_void(&self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) {
        let fv = self.cfuns[name];
        self.builder.build_call(fv, args, name).unwrap();
    }

    fn retain(&self, ty: &Type, val: BasicValueEnum<'ctx>) {
        let name = match ty {
            Type::String => "dial_string_retain",
            // bytes 与 string 布局相同，但走**自己的** retain 函数——
            // 类型边界必须由运行时函数族来守，不能只靠编译器单方面。
            Type::Bytes => "dial_bytes_retain",
            Type::Array(_) => "dial_array_retain",
            Type::Map => "dial_map_retain",
            Type::Error => "dial_error_retain",
            // 通道跨线程共享，因此走**原子**引用计数（与 string/map 的非原子 rc 不同）。
            Type::Channel(_) => "dial_channel_retain",
            _ => return,
        };
        self.call_c_void(name, &[val.into()]);
    }

    /// 按**槽位**释放：从 ptr 里 load 出指针再 release（用于局部变量）。
    fn release(&self, ty: &Type, ptr: PointerValue<'ctx>) {
        let v = self.builder.build_load(self.i8ptr(), ptr, "ldrel").unwrap();
        self.release_value(ty, v);
    }

    /// 按**值**释放：直接对已取出的指针调 release（供线程 thunk 使用——
    /// 那里持有的是实参值，没有对应的 alloca 槽位）。
    fn release_value(&self, ty: &Type, val: BasicValueEnum<'ctx>) {
        let name = match ty {
            Type::String => "dial_string_release",
            Type::Bytes => "dial_bytes_release",
            Type::Array(_) => "dial_array_release",
            Type::Map => "dial_map_release",
            Type::Error => "dial_error_release",
            Type::Channel(_) => "dial_channel_release",
            _ => return,
        };
        self.call_c_void(name, &[val.into()]);
    }

    /// 返回「元素析构器」的函数指针（i8*），供 dial_channel_new 在通道销毁时释放残留元素。
    /// 嵌套通道（channel<channel<...>>）必须用 dial_channel_release（原子 rc），
    /// 其余元素类型（string/array/map/error）统一走 dial_rc_release。
    fn elem_release_fn(&self, elem_ty: &Type) -> PointerValue<'ctx> {
        let name = match elem_ty {
            Type::Channel(_) => "dial_channel_release",
            _ => "dial_rc_release",
        };
        self.module
            .get_function(name)
            .unwrap_or_else(|| panic!("内部错误：缺少 {} 的 LLVM 定义", name))
            .as_global_value()
            .as_pointer_value()
    }

    /// 按名查找局部变量的栈槽（由内到外）。
    fn lookup_slot(scopes: &CgScopes<'ctx>, name: &str) -> Option<PointerValue<'ctx>> {
        scopes
            .iter()
            .rev()
            .find_map(|s| s.get(name))
            .map(|(_ty, (_m, ptr))| *ptr)
    }

    /// 标记「该变量的栈槽已转移所有权」：作用域结束时跳过它的 release。
    fn mark_moved(&self, scopes: &CgScopes<'ctx>, name: &str) {
        if let Some(p) = Self::lookup_slot(scopes, name) {
            self.moved.borrow_mut().insert(p);
        }
    }

    /// 受 ARC 管理的类型。**不含** File（显式 open/close）与 Thread（显式 join/detach）——
    /// 这两者都是「显式终结」资源，交给 ARC 反而会让生命周期语义变得含混。
    fn is_heap_type(t: &Type) -> bool {
        matches!(
            *t,
            Type::String | Type::Array(_) | Type::Map | Type::Error | Type::Channel(_)
        )
    }

    /// 在函数的 **entry 块** 里分配栈槽。
    ///
    /// 这是必须的，而非优化偏好：局部变量声明可能出现在循环体内，
    /// 若在当时所在块直接 `alloca`，每进入一次循环就会在栈上多压一个槽，
    /// 迭代次数一多就会把栈耗尽（段错误）。把 alloca 统一提到 entry 块，
    /// 每个变量只占一个固定栈槽、跨迭代复用，栈用量与循环次数无关。
    fn alloca_in_entry(
        &self,
        function: FunctionValue<'ctx>,
        ty: BasicTypeEnum<'ctx>,
        name: &str,
    ) -> PointerValue<'ctx> {
        let entry = function
            .get_first_basic_block()
            .expect("函数应至少有一个 entry 块");
        let builder = self.context.create_builder();
        match entry.get_first_instruction() {
            Some(first) => builder.position_before(&first),
            None => builder.position_at_end(entry),
        }
        builder.build_alloca(ty, name).unwrap()
    }

    /// 递归地对结构体值中的**堆字段**（string / int[]）执行 retain。
    ///
    /// 结构体本身是值类型（栈上 alloca），但它的堆字段是不透明指针：
    /// 结构体值被复制一次，其堆字段就多一个持有者，因此必须对称地 +1，
    /// 否则作用域结束时两侧的 release 会互相踩踏（double free）。
    fn retain_struct_fields(
        &self,
        sty: &Type,
        ptr: PointerValue<'ctx>,
        structs: &StructTable,
    ) {
        let name = match sty {
            Type::Struct(n) => n,
            _ => return,
        };
        let (fields, llvm_sty) = match (structs.get(name), self.struct_types.get(name)) {
            (Some(f), Some(t)) => (f.clone(), *t),
            _ => return,
        };
        for (i, (_fname, fty)) in fields.iter().enumerate() {
            let gep = self
                .builder
                .build_struct_gep(llvm_sty, ptr, i as u32, "frtn")
                .unwrap();
            match fty {
                // v0.8 补上 Error 与 Channel（此前漏掉，与 release_struct_fields 不对称）。
                Type::String | Type::Array(_) | Type::Map | Type::Error | Type::Channel(_) => {
                    let v = self.builder.build_load(self.i8ptr(), gep, "lfr").unwrap();
                    self.retain(fty, v);
                }
                Type::Struct(_) => self.retain_struct_fields(fty, gep, structs),
                _ => {}
            }
        }
    }

    /// `retain_struct_fields` 的对称操作：递归 release 结构体值中的堆字段。
    fn release_struct_fields(
        &self,
        sty: &Type,
        ptr: PointerValue<'ctx>,
        structs: &StructTable,
    ) {
        let name = match sty {
            Type::Struct(n) => n,
            _ => return,
        };
        let (fields, llvm_sty) = match (structs.get(name), self.struct_types.get(name)) {
            (Some(f), Some(t)) => (f.clone(), *t),
            _ => return,
        };
        for (i, (_fname, fty)) in fields.iter().enumerate() {
            let gep = self
                .builder
                .build_struct_gep(llvm_sty, ptr, i as u32, "frel")
                .unwrap();
            match fty {
                Type::String | Type::Array(_) | Type::Map | Type::Error | Type::Channel(_) => {
                    self.release(fty, gep)
                }
                Type::Struct(_) => self.release_struct_fields(fty, gep, structs),
                _ => {}
            }
        }
    }

    // ---- 函数体 ----

    fn gen_function(
        &self,
        f: &Func,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        let function = func_values[&f.name];
        let entry = self.context.append_basic_block(function, "entry");
        self.builder.position_at_end(entry);

        // 每个函数独立：进入时清空受保护集合（否则上一个函数的登记会串进来）。
        self.protected.borrow_mut().clear();

        let mut scopes: CgScopes = vec![HashMap::new()];
        for (i, (pname, pty)) in f.params.iter().enumerate() {
            let llvm_ty = self.dialc_type_to_llvm(pty);
            let ptr = self.alloca_in_entry(function, llvm_ty, pname);
            let param_val = function.get_nth_param(i as u32).unwrap();
            self.builder.build_store(ptr, param_val).unwrap();
            scopes[0].insert(pname.clone(), (pty.clone(), (true, ptr)));
            // 形参取得共享所有权（进入时对称 +1，退出时 -1）：
            //   - 字符串/数组形参：直接 retain 指针；
            //   - 结构体形参：值拷贝本身不带所有权，需递归 retain 其中的堆字段。
            if Self::is_heap_type(pty) {
                let v = self.builder.build_load(self.i8ptr(), ptr, "parm").unwrap();
                self.retain(pty, v);
            } else if matches!(pty, Type::Struct(_)) {
                self.retain_struct_fields(pty, ptr, structs);
            }
        }
        for stmt in &f.body {
            self.gen_stmt(stmt, function, &mut scopes, func_values, functions, structs)?;
        }
        // 函数末尾未显式 return 时补默认返回
        if !self.is_terminated() {
            if f.ret == Type::Void {
                self.builder.build_return(None).unwrap();
            } else if f.name == "main" && f.ret == Type::Int {
                let zero = self
                    .context
                    .i32_type()
                    .const_int(0, false);
                self.builder.build_return(Some(&zero)).unwrap();
            } else {
                let dv = self.default_value(&f.ret);
                self.builder.build_return(Some(&dv)).unwrap();
            }
        }
        Ok(())
    }

    /// 为 `spawn(fn, ...)` 生成线程入口 thunk：`void __dial_thunk_fn(ptr argcell)`。
    ///
    /// 为什么必须有一个 thunk：C 的 `pthread_create` 只认统一签名 `void (*)(void*)`，
    /// 而 Dial 函数的实参类型各不相同（ptr / i64 / f64 / i1）。thunk 负责从 argcell
    /// 里按 8 字节槽宽把实参取出来，再按真实签名调用目标函数。
    ///
    /// **所有权会计（易错点）**：被调函数在入口 retain、出口 release（净零，它只是借用）；
    /// 移交进来的那一份由 thunk 在调用返回后无条件释放。若被调函数内部把某个形参
    /// `move_to` 进了通道，它的出口 release 会被跳过，计数恰好落在 thunk 这一份上，
    /// 最终由通道的接收方释放——既不泄漏也不双释放。
    fn gen_spawn_thunk(
        &self,
        fname: &str,
        params: &[Type],
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        structs: &StructTable,
    ) -> Result<FunctionValue<'ctx>> {
        let thunk_name = format!("__dial_thunk_{}", fname);
        // 同一函数可能被多处 spawn：thunk 只生成一次（重复 add_function 会被 LLVM 改名）。
        if let Some(existing) = self.module.get_function(&thunk_name) {
            return Ok(existing);
        }
        // 生成 thunk 会移动 builder 的插入点，必须先把调用方的位置存下来再还原，
        // 否则后续 IR 会被写进 thunk 的函数体里。
        let saved = self.builder.get_insert_block();

        let ft = self
            .context
            .void_type()
            .fn_type(&[self.i8ptr().into()], false);
        let thunk = self.module.add_function(&thunk_name, ft, None);
        let entry = self.context.append_basic_block(thunk, "entry");
        self.builder.position_at_end(entry);
        let cell = thunk.get_nth_param(0).unwrap().into_pointer_value();

        let fv = *func_values.get(fname).ok_or_else(|| {
            DialcError::new(format!("缺少函数 '{}' 的 LLVM 定义", fname), 0, 0)
        })?;

        let mut arg_vals: Vec<BasicValueEnum<'ctx>> = Vec::new();
        let mut meta: Vec<BasicMetadataValueEnum<'ctx>> = Vec::new();
        for (i, pty) in params.iter().enumerate() {
            let idx = self.context.i64_type().const_int(i as u64, false);
            let v: BasicValueEnum<'ctx> = match pty {
                Type::Float => {
                    let slot = unsafe {
                        self.builder
                            .build_gep(self.context.f64_type(), cell, &[idx], "th.slot")
                            .unwrap()
                    };
                    self.builder
                        .build_load(self.context.f64_type(), slot, "th.arg")
                        .unwrap()
                }
                Type::Bool => {
                    let slot = unsafe {
                        self.builder
                            .build_gep(self.context.i64_type(), cell, &[idx], "th.slot")
                            .unwrap()
                    };
                    let w = self
                        .builder
                        .build_load(self.context.i64_type(), slot, "th.argw")
                        .unwrap()
                        .into_int_value();
                    self.builder
                        .build_int_truncate(w, self.context.bool_type(), "th.arg")
                        .unwrap()
                        .into()
                }
                Type::String | Type::Array(_) | Type::Map | Type::Error | Type::Channel(_) => {
                    let slot = unsafe {
                        self.builder
                            .build_gep(self.i8ptr(), cell, &[idx], "th.slot")
                            .unwrap()
                    };
                    self.builder.build_load(self.i8ptr(), slot, "th.arg").unwrap()
                }
                _ => {
                    let slot = unsafe {
                        self.builder
                            .build_gep(self.context.i64_type(), cell, &[idx], "th.slot")
                            .unwrap()
                    };
                    self.builder
                        .build_load(self.context.i64_type(), slot, "th.arg")
                        .unwrap()
                }
            };
            arg_vals.push(v);
            meta.push(v.into());
        }

        self.builder.build_call(fv, &meta, "th.call").unwrap();

        // 被移交进来的那一份所有权，由本线程负责最终释放（无条件，不查 moved）。
        for (pty, v) in params.iter().zip(arg_vals.iter()) {
            self.release_value(pty, *v);
        }
        self.builder.build_return(None).unwrap();

        if let Some(bb) = saved {
            self.builder.position_at_end(bb);
        }
        let _ = structs;
        Ok(thunk)
    }

    fn release_scope_locals(
        &self,
        scope: &HashMap<String, Local<'ctx>>,
        structs: &StructTable,
    ) {
        let moved = self.moved.borrow();
        for (_name, (ty, (_m, ptr))) in scope.iter() {
            if moved.contains(ptr) {
                continue;
            }
            if Self::is_heap_type(ty) {
                self.release(ty, *ptr);
            } else if matches!(ty, Type::Struct(_)) {
                // 结构体值本身在栈上，无需释放；但其堆字段必须有对称的 release
                self.release_struct_fields(ty, *ptr, structs);
            }
        }
    }

    fn release_all(&self, scopes: &CgScopes<'ctx>, except: Option<&str>, structs: &StructTable) {
        for scope in scopes.iter() {
            let moved = self.moved.borrow();
            for (name, (ty, (_m, ptr))) in scope.iter() {
                if let Some(ex) = except {
                    if name == ex {
                        continue;
                    }
                }
                if moved.contains(ptr) {
                    continue;
                }
                if Self::is_heap_type(ty) {
                    self.release(ty, *ptr);
                } else if matches!(ty, Type::Struct(_)) {
                    self.release_struct_fields(ty, *ptr, structs);
                }
            }
        }
    }

    /// 抛出可恢复异常（v0.7）。
    ///
    /// 在真正跳转前，**必须**释放当前帧内「本帧 try 体作用域」的所有活跃 ARC 局部
    /// （string/int[]/map/error 及含堆字段的结构体），否则异常一抛、这些局部就随栈展开而“丢失”，
    /// 其引用计数永不清零 -> 内存泄漏。
    ///
    /// **释放范围由 `protected` 集合精确界定**：`try` 入口会把其体作用域之外的所有外层槽位登记进
    /// `protected`，emit_throw 释放时一律跳过它们——只释放 try 体（及其内部嵌套块）的局部，
    /// 绝不触碰外层作用域的变量（README §26.4）。这比记录变量名集合更稳健（天然兼容遮蔽）。
    ///
    /// 跨帧抛出（子函数内 throw，CVE-2024-DIAL-004 修复）时，子帧自身的 `protected` 为空，
    /// 于是释放子帧全部局部——子帧整体随 unwinding 销毁，这才正确；绝不能用父帧 try 记录的作用域
    /// 深度去索引子帧的 scopes（两套作用域编号不互通）。
    ///
    /// 被抛出的错误对象本身（payload）不会被释放：若它来自某个局部变量，该变量已在 Throw 处被标记 moved 而跳过。
    ///
    /// 跳转到栈顶 exbuf：dial_throw_to(buf, err) 把 err 存入缓冲区后 longjmp 回 dial_try 调用点；
    /// 若缓冲区里原先已有值（re-throw 场景），由 C 侧先释放旧值，避免泄漏。
    /// 若无任何 try 包裹（栈空）：未捕获异常 -> dial_panic（打印消息并终止进程，绝不静默继续）。
    fn emit_throw(
        &self,
        err: BasicValueEnum<'ctx>,
        scopes: &CgScopes<'ctx>,
        structs: &StructTable,
    ) -> Result<()> {
        // 运行时异常栈栈顶：跨函数 unwinding 的关键。
        // 包含 try 的函数（调用栈上仍存活，其 setjmp 帧有效）已把自己的 exbuf 压栈，
        // 因此子函数里抛出的异常也能取到它并 longjmp 回去（CVE-2024-DIAL-004 修复）。
        // 栈空则说明异常出现在任何 try 之外 -> 未捕获 -> dial_panic。
        let func = self.builder.get_insert_block().unwrap().get_parent().unwrap();
        let top = self.call_c("dial_exc_top", &[]).into_pointer_value();
        let null = self.i8ptr().const_null();
        let has_buf = self
            .builder
            .build_int_compare(IntPredicate::NE, top, null, "exc.hasbuf")
            .unwrap();
        let throw_bb = self.context.append_basic_block(func, "exc.throw");
        let panic_bb = self.context.append_basic_block(func, "exc.panic");
        self.builder
            .build_conditional_branch(has_buf, throw_bb, panic_bb)
            .unwrap();

        // ---- 捕获路径：释放当前函数所有活跃 ARC 局部后 longjmp 回 try ----
        self.builder.position_at_end(throw_bb);
        // 跨函数 unwinding 只释放「当前函数（抛异常这一帧）」的活跃局部——
        // 外层 try 所在帧仍然存活，其局部会在它自己的 catch / 正常收尾中释放，无需我们代劳。
        // moved 集合保证被 move 出去（含正要抛出的 Error 本身）的槽位不会被重复释放。
        let moved = self.moved.borrow();
        let protected = self.protected.borrow();
        for scope in scopes.iter() {
            for (_name, (ty, (_m, ptr))) in scope.iter() {
                if moved.contains(ptr) || protected.contains(ptr) {
                    continue;
                }
                if Self::is_heap_type(ty) {
                    self.release(ty, *ptr);
                } else if matches!(ty, Type::Struct(_)) {
                    self.release_struct_fields(ty, *ptr, structs);
                }
            }
        }
        // dial_throw_to(buf, err)：把 err 写入缓冲区（覆盖前由 C 侧释放旧值，re-throw 安全），
        // 再 longjmp 回 try 调用点。C 侧封装了 setjmp/longjmp 的 returns_twice / noreturn 语义。
        self.call_c_void("dial_throw_to", &[top.into(), err.into()]);
        self.builder.build_unreachable().unwrap();

        // ---- 未捕获路径：打印错误消息并终止进程（绝不静默继续） ----
        self.builder.position_at_end(panic_bb);
        // 注意 **必须**先 dial_string_cstr 再交给 dial_panic：
        // dial_error_msg 返回的是不透明的 DialString 句柄（[refcount][len][data...]），
        // 而 dial_panic 的形参是 const char*。少这一步转换，打印出来的就是
        // 结构体头部的字节而不是错误消息（曾实测只打印出一个 "9"）。
        let msg = self.call_c("dial_error_msg", &[err.into()]);
        let cstr = self.call_c("dial_string_cstr", &[msg.into()]);
        self.call_c_void("dial_panic", &[cstr.into()]);
        self.builder.build_unreachable().unwrap();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_stmt(
        &self,
        stmt: &Stmt,
        function: FunctionValue<'ctx>,
        scopes: &mut CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        match stmt {
            Stmt::VarDecl {
                name,
                ty,
                mutable,
                value,
                move_hint,
                ..
            } => {
                let llvm_ty = self.dialc_type_to_llvm(ty);
                let ptr = self.alloca_in_entry(function, llvm_ty, name);
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, ty);
                // 绑定策略：Var 复制且非移动 -> retain（共享）；其余（构造/方法/call）-> 直接取得所有权
                if let Expr::Var(src) = value {
                    if Self::is_heap_type(ty) && !move_hint {
                        self.retain(ty, stored);
                    }
                    if *move_hint {
                        self.mark_moved(scopes, src);
                    }
                }
                self.builder.build_store(ptr, stored).unwrap();
                // 结构体：值拷贝已把堆字段的指针复制过来，需要对称地给这些字段 +1，
                // 否则本变量在作用域结束时的 release 会与源变量互相踩踏。
                if let Expr::Var(_) = value {
                    if matches!(ty, Type::Struct(_)) && !move_hint {
                        self.retain_struct_fields(ty, ptr, structs);
                    }
                }
                scopes.last_mut().unwrap().insert(name.clone(), (ty.clone(), (*mutable, ptr)));
                Ok(())
            }
            Stmt::Assign {
                name,
                value,
                move_hint,
                line,
                col,
            } => {
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", name), *line, *col))?;
                let (vty, (_m, ptr)) = info;
                // 严格顺序：先取得新值的所有权（retain），再释放旧值（release）。
                // 这保证自赋值 `x = x` 安全——若先 release，引用计数可能归零并被 free，
                // 随后的 retain 就变成 use-after-free。
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, &vty);

                if matches!(vty, Type::Struct(_)) {
                    // 结构体：新值先落到临时槽，再“先加后减”，最后写回。
                    // 这样即便 `x = x`，也是先 +1 再 -1，不会把对象提前释放。
                    let llvm_ty = self.dialc_type_to_llvm(&vty);
                    let tmp = self.alloca_in_entry(function, llvm_ty, "asgtmp");
                    self.builder.build_store(tmp, stored).unwrap();
                    if let Expr::Var(_) = value {
                        if !move_hint {
                            self.retain_struct_fields(&vty, tmp, structs);
                        }
                    }
                    self.release_struct_fields(&vty, ptr, structs);
                    let v = self.builder.build_load(llvm_ty, tmp, "asgval").unwrap();
                    self.builder.build_store(ptr, v).unwrap();
                } else {
                    if let Expr::Var(_) = value {
                        if Self::is_heap_type(&vty) && !move_hint {
                            self.retain(&vty, stored);
                        }
                    }
                    if Self::is_heap_type(&vty) {
                        self.release(&vty, ptr);
                    }
                    self.builder.build_store(ptr, stored).unwrap();
                }

                if let Expr::Var(src) = value {
                    if *move_hint {
                        self.mark_moved(scopes, src);
                    }
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
                let lptr = self.gen_lvalue_ptr(obj, function, scopes, func_values, functions, structs)?;
                // 找到字段索引与类型
                let ot = type_of(obj, scopes, functions, structs)?;
                let sty = self.get_struct_type(&ot)?;
                let (fidx, fty) = self.field_info(&ot, field, structs, *line, *col)?;
                let fgep = self
                    .builder
                    .build_struct_gep(sty, lptr, fidx, "fgep")
                    .unwrap();
                // 覆盖前 release 旧字段值（堆类型）
                if Self::is_heap_type(&fty) {
                    self.release(&fty, fgep);
                }
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, &fty);
                self.builder.build_store(fgep, stored).unwrap();
                Ok(())
            }
            Stmt::AssignIndex {
                arr,
                index,
                value,
                line,
                col,
            } => {
                // arr[i] = v  ->  arr = dial_array_set(arr, i, v)
                let root = lvalue_root(arr).ok_or_else(|| {
                    DialcError::new("语义错误：数组索引赋值目标必须是变量", *line, *col)
                })?;
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(root).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", root), *line, *col))?;
                let (_aty, (_m, arr_ptr)) = info;
                let cur = self.builder.build_load(self.i8ptr(), arr_ptr, "arrptr").unwrap();
                let idx = self.gen_expr(index, function, scopes, func_values, functions, structs)?;
                let idxi64 = self.to_i64(idx, &type_of(index, scopes, functions, structs)?);
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let vali64 = self.to_i64(val, &type_of(value, scopes, functions, structs)?);
                // Fail-Fast：越界写是 C 里最阴险的 Bug（悄悄改坏相邻数据），当场 trap。
                // ReleaseFast 下这段（含这次 length 调用）被整段消除，零开销。
                if self.checks_enabled() {
                    let len = self.call_c("dial_array_length", &[cur.into()]);
                    self.emit_bounds_check(function, len.into_int_value(), idxi64);
                }
                let newp = self.call_c(
                    "dial_array_set",
                    &[cur.into(), idxi64.into(), vali64.into()],
                );
                // 所有权契约：dial_array_set 返回的指针**替换**旧指针。
                //   - 独占时（refcount==1）原地写入并返回同一指针，旧引用计数不变；
                //   - 共享时（refcount>1）在 C 侧完成 COW 克隆并释放本调用者的旧引用。
                // 两种情形下调用方都**不得**再对旧指针 release，否则分别造成
                // 悬垂指针与双重释放。故此处只做写回。
                self.builder.build_store(arr_ptr, newp).unwrap();
                Ok(())
            }
            Stmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                let cond_val = self.gen_expr(cond, function, scopes, func_values, functions, structs)?;
                let cond_i1 = cond_val.into_int_value();
                let then_bb = self.context.append_basic_block(function, "then");
                let else_bb = self.context.append_basic_block(function, "else");
                let merge_bb = self.context.append_basic_block(function, "ifcont");
                self.builder
                    .build_conditional_branch(cond_i1, then_bb, else_bb)
                    .unwrap();
                self.builder.position_at_end(then_bb);
                scopes.push(HashMap::new());
                for s in then_branch {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                }
                self.builder.position_at_end(else_bb);
                if let Some(eb) = else_branch {
                    scopes.push(HashMap::new());
                    for s in eb {
                        self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                    }
                    self.release_scope_locals(scopes.last().unwrap(), structs);
                    scopes.pop();
                }
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                }
                self.builder.position_at_end(merge_bb);
                Ok(())
            }
            Stmt::While { cond, body, .. } => {
                let header_bb = self.context.append_basic_block(function, "while.cond");
                let body_bb = self.context.append_basic_block(function, "while.body");
                let exit_bb = self.context.append_basic_block(function, "while.end");
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(header_bb);
                let cond_val = self.gen_expr(cond, function, scopes, func_values, functions, structs)?;
                let cond_i1 = cond_val.into_int_value();
                self.builder
                    .build_conditional_branch(cond_i1, body_bb, exit_bb)
                    .unwrap();
                self.builder.position_at_end(body_bb);
                // while 的 continue 直接回到条件判定
                self.loop_targets.borrow_mut().push((header_bb, exit_bb));
                scopes.push(HashMap::new());
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                self.loop_targets.borrow_mut().pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(header_bb).unwrap();
                }
                self.builder.position_at_end(exit_bb);
                Ok(())
            }
            Stmt::Break(line, col) => {
                let target = self
                    .loop_targets
                    .borrow()
                    .last()
                    .map(|t| t.1)
                    .ok_or_else(|| DialcError::new("内部错误：break 不在循环内", *line, *col))?;
                self.builder.build_unconditional_branch(target).unwrap();
                // 后续语句（若有）落进新块，避免往已终结的块里继续追加指令
                let dead = self.context.append_basic_block(function, "after.brk");
                self.builder.position_at_end(dead);
                Ok(())
            }
            Stmt::Continue(line, col) => {
                let target = self
                    .loop_targets
                    .borrow()
                    .last()
                    .map(|t| t.0)
                    .ok_or_else(|| DialcError::new("内部错误：continue 不在循环内", *line, *col))?;
                self.builder.build_unconditional_branch(target).unwrap();
                let dead = self.context.append_basic_block(function, "after.cont");
                self.builder.position_at_end(dead);
                Ok(())
            }
            Stmt::For { var, start, end, body, .. } => {
                let i64t = self.context.i64_type();
                // 循环变量：在入口块分配，跨迭代存活。
                let var_ptr = self.alloca_in_entry(function, i64t.into(), var);
                let start_val = self.gen_expr(start, function, scopes, func_values, functions, structs)?;
                let start_i64 = self.to_i64(start_val, &type_of(start, scopes, functions, structs)?);
                self.builder.build_store(var_ptr, start_i64).unwrap();
                // 把循环变量登记进作用域（int 非堆类型，无需 retain/release）。
                scopes.push(HashMap::new());
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(var.clone(), (Type::Int, (true, var_ptr)));

                // 注意：自增必须落在独立的 for.inc 块，continue 跳到这里而非 for.cond，
                // 否则 continue 会跳过 i = i + 1 造成死循环。
                let header_bb = self.context.append_basic_block(function, "for.cond");
                let body_bb = self.context.append_basic_block(function, "for.body");
                let inc_bb = self.context.append_basic_block(function, "for.inc");
                let exit_bb = self.context.append_basic_block(function, "for.end");
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(header_bb);
                let cur = self.builder.build_load(i64t, var_ptr, "i").unwrap();
                let end_val = self.gen_expr(end, function, scopes, func_values, functions, structs)?;
                let end_i64 = self.to_i64(end_val, &type_of(end, scopes, functions, structs)?);
                let cond = self
                    .builder
                    .build_int_compare(IntPredicate::SLT, cur.into_int_value(), end_i64, "forcond")
                    .unwrap();
                self.builder
                    .build_conditional_branch(cond, body_bb, exit_bb)
                    .unwrap();
                self.builder.position_at_end(body_bb);
                // for 的 continue 目标是自增块 for.inc（先 i++ 再回到条件判定）
                self.loop_targets.borrow_mut().push((inc_bb, exit_bb));
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.release_scope_locals(scopes.last().unwrap(), structs);
                self.loop_targets.borrow_mut().pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(inc_bb).unwrap();
                }
                self.builder.position_at_end(inc_bb);
                // i = i + 1
                let cur2 = self.builder.build_load(i64t, var_ptr, "i").unwrap();
                let next = self
                    .builder
                    .build_int_add(cur2.into_int_value(), i64t.const_int(1, false), "iinc")
                    .unwrap();
                self.builder.build_store(var_ptr, next).unwrap();
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(exit_bb);
                scopes.pop();
                Ok(())
            }
            Stmt::Return(e, line, col) => {
                match e {
                    None => {
                        if !f_ret_is_void(function) {
                            return Err(DialcError::new(
                                "内部错误：非 void 函数不应有无值 return",
                                *line, *col,
                            ));
                        }
                        // 无返回值：释放本函数持有的全部堆对象后返回
                        self.release_all(scopes, None, structs);
                        self.builder.build_return(None).unwrap();
                    }
                    Some(expr) => {
                        // 严格顺序（必须如此，否则会把返回值提前释放）：
                        //   1) 先求值返回表达式，取得待返回的值；
                        //   2) 若返回的是局部变量，标记 moved，使其所有权随返回值转移；
                        //   3) 最后释放其余**未被转移**的局部变量；
                        //   4) 生成 ret。
                        let val = self.gen_expr(expr, function, scopes, func_values, functions, structs)?;
                        if let Expr::Var(n) = expr {
                            self.mark_moved(scopes, n);
                        }
                        // 处理 main 返回 i32（C ABI）
                        let ret_bte = function.get_type().get_return_type();
                        let is_main_i32 = matches!(
                            ret_bte,
                            Some(BasicTypeEnum::IntType(it)) if it.get_bit_width() == 32
                        );
                        let ret_val: BasicValueEnum = if is_main_i32 {
                            let i = self.to_i64(val, &type_of(expr, scopes, functions, structs)?);
                            self.builder
                                .build_int_truncate(i, self.context.i32_type(), "i32")
                                .unwrap()
                                .into()
                        } else {
                            val
                        };
                        self.release_all(scopes, None, structs);
                        self.builder.build_return(Some(&ret_val)).unwrap();
                    }
                }
                Ok(())
            }
            Stmt::Throw(e, _line, _col) => {
                // 求值被抛出的错误对象，并把它源变量标记为 moved，
                // 使 emit_throw 释放 try 作用域 ARC 局部时跳过该 payload（否则会把正要抛出的 Error 一起释放 -> UAF）。
                let val =
                    self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                if let Expr::Var(n) = e {
                    self.mark_moved(scopes, n);
                }
                self.emit_throw(val, scopes, structs)?;
                Ok(())
            }
            Stmt::Try { body, catch, .. } => {
                let setup_bb = self.context.append_basic_block(function, "try.setup");
                let body_bb = self.context.append_basic_block(function, "try.body");
                let catch_bb = self.context.append_basic_block(function, "try.catch");
                let cont_bb = self.context.append_basic_block(function, "try.cont");
                let merge_bb = self.context.append_basic_block(function, "try.merge");

                self.builder.build_unconditional_branch(setup_bb).unwrap();

                // ---- try.setup：建立异常缓冲区、setjmp（必须在本帧内发起） ----
                self.builder.position_at_end(setup_bb);
                // 登记「外层作用域（try 体之前）的所有局部槽位」进 protected：
                // 这些变量在 try 体抛异常、被本帧 catch 捕获后仍可能被 catch 体引用，
                // 绝不能随 emit_throw 被释放（否则 catch 引用即 UAF，见 §26.4）。
                // 在 push try 体作用域之前登记，恰好覆盖「try 之前」的全部外层作用域。
                let try_protected: Vec<PointerValue<'ctx>> = scopes
                    .iter()
                    .flat_map(|s| s.values().map(|(_, (_, ptr))| *ptr))
                    .collect();
                {
                    let mut prot = self.protected.borrow_mut();
                    for p in &try_protected {
                        prot.insert(*p);
                    }
                }
                let buf = self.call_c("dial_exbuf_new", &[]);
                let buf_alloca = self.alloca_in_entry(function, self.i8ptr().into(), "exbuf");
                self.builder.build_store(buf_alloca, buf).unwrap();
                // 压入运行时异常栈：包含 try 的函数（其 setjmp 帧此刻已活在调用栈上）把自己的
                // exbuf 登记上去，使**任何深层子函数**里的 throw 都能取栈顶、longjmp 回此处
                // （跨函数 unwinding，CVE-2024-DIAL-004 修复）。必须在 setjmp 之前完成。
                self.call_c_void("dial_exc_push", &[buf.into()]);
                // setjmp：首次返回 0（进入 try 体），longjmp 回来时返回非 0（进入 catch）。
                // 必须由本函数亲自调用（jmp_buf 保存的是本帧栈指针，见 declare_stdlib 注释）。
                let env_ptr = self.call_c("dial_exbuf_env_ptr", &[buf.into()]);
                let r = self.call_c("setjmp", &[env_ptr.into()]);
                let is_throw = self
                    .builder
                    .build_int_compare(
                        IntPredicate::NE,
                        r.into_int_value(),
                        self.context.i32_type().const_int(0, false),
                        "try.isthrow",
                    )
                    .unwrap();
                self.builder
                    .build_conditional_branch(is_throw, catch_bb, body_bb)
                    .unwrap();

                // ---- try.body ----
                self.builder.position_at_end(body_bb);
                scopes.push(HashMap::new());
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                    if self.is_terminated() {
                        break;
                    }
                }
                let body_done = !self.is_terminated();
                if body_done {
                    // 正常走完：释放 try 体局部后汇入 merge（merge 负责 free exbuf）。
                    self.release_scope_locals(scopes.last().unwrap(), structs);
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                }
                scopes.pop();

                // ---- try.catch：捕获点 ----
                self.builder.position_at_end(catch_bb);
                // 弹出本层 exbuf（使 catch 体内的 re-throw 路由到外层 try）。
                self.call_c_void("dial_exc_pop", &[]);
                let buf = self
                    .builder
                    .build_load(self.i8ptr(), buf_alloca, "exbuf.ld")
                    .unwrap();
                // 取出错误对象（所有权转移给 catch 作用域），随后 free 缓冲区（b->err 已置 NULL，不会重复释放）。
                let err = self.call_c("dial_exbuf_take_err", &[buf.into()]);
                self.call_c_void("dial_exbuf_free", &[buf.into()]);
                scopes.push(HashMap::new());
                match &catch.var {
                    // catch e { }：把捕获到的 Error 绑定到 e（e 是独立 alloca，作用域结束时自动 release，零额外开销）。
                    Some(name) => {
                        let e_slot =
                            self.alloca_in_entry(function, self.i8ptr().into(), name);
                        self.builder.build_store(e_slot, err).unwrap();
                        scopes
                            .last_mut()
                            .unwrap()
                            .insert(name.clone(), (Type::Error, (false, e_slot)));
                    }
                    // catch { }：无人接收错误对象 -> 立即 release，避免泄漏。
                    None => {
                        self.call_c_void("dial_error_release", &[err.into()]);
                    }
                }
                for s in &catch.body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                    if self.is_terminated() {
                        break;
                    }
                }
                let catch_done = !self.is_terminated();
                if catch_done {
                    self.release_scope_locals(scopes.last().unwrap(), structs);
                    // try 已彻底结束：解除此前登记的外层槽位保护，避免污染后续作用域。
                    {
                        let mut prot = self.protected.borrow_mut();
                        for p in &try_protected {
                            prot.remove(p);
                        }
                    }
                    self.builder.build_unconditional_branch(cont_bb).unwrap();
                }
                scopes.pop();

                // ---- try.merge：仅「try 体正常完成」路径到达，弹栈并 free exbuf 后汇入 cont ----
                // 与 catch 路径严格二选一：只有一条会执行，故这里无条件弹栈 + 释放（配对 setup 的 push）。
                self.builder.position_at_end(merge_bb);
                // try 已彻底结束：解除此前登记的外层槽位保护。
                {
                    let mut prot = self.protected.borrow_mut();
                    for p in &try_protected {
                        prot.remove(p);
                    }
                }
                self.call_c_void("dial_exc_pop", &[]);
                let buf = self
                    .builder
                    .build_load(self.i8ptr(), buf_alloca, "exbuf.ld2")
                    .unwrap();
                self.call_c_void("dial_exbuf_free", &[buf.into()]);
                self.builder.build_unconditional_branch(cont_bb).unwrap();

                // ---- try.cont：try/catch 之后的统一续跑点 ----
                self.builder.position_at_end(cont_bb);
                Ok(())
            }
            // ==================================================================
            //  多路复用 select（v0.91）
            // ==================================================================
            //   select { case recv(ch1) as a { ... } default { ... } }
            //
            //  控制流骨架：
            //      setup   -> 建选择器、登记各 case 的通道
            //      loop    -> 取 epoch、挑一个就绪通道（随机起点，防饿死）
            //      dispatch-> switch 到对应的 take 块
            //      take_i  -> 从第 i 个通道非阻塞取一个元素
            //                 取到 NULL 说明「被别人抢走了」-> 回 loop 重挑
            //      case_i  -> 绑定变量、执行分支体
            //      nowait  -> 无就绪时：有 default 就走 default，否则等事件
            //      eof     -> 全体 EOF：释放选择器、直接结束（不执行任何分支）
            //
            //  等待是**事件驱动**的（挂在全局条件变量上），不是轮询——
            //  被 send 唤醒的延迟是微秒级。
            Stmt::Select {
                arms,
                default_body,
                line,
                col,
            } => {
                let n_arms = arms.len();
                let setup_bb = self.context.append_basic_block(function, "select.setup");
                let loop_bb = self.context.append_basic_block(function, "select.loop");
                let chk_bb = self.context.append_basic_block(function, "select.chk");
                let dispatch_bb = self.context.append_basic_block(function, "select.dispatch");
                let nowait_bb = self.context.append_basic_block(function, "select.nowait");
                let eof_bb = self.context.append_basic_block(function, "select.eof");
                let end_bb = self.context.append_basic_block(function, "select.end");

                // 选择器句柄的槽必须在 entry block（否则栈帧会随循环增长）。
                let sel_slot = self.alloca_in_entry(function, self.i8ptr().into(), "select.sel");

                // ---- setup：建选择器并登记各 case 的通道 ----
                // 先把**当前块**接到 setup 上（当前块可能是 while 的 body 块）。
                // 少了这一步，调用方的那个块会变成没有终结指令的空块。
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(setup_bb).unwrap();
                }
                self.builder.position_at_end(setup_bb);
                let sel = self.call_c("dial_sel_new", &[]);
                self.builder.build_store(sel_slot, sel).unwrap();
                for arm in arms {
                    // case 的形状在 Sema 已校验（必须是 recv(ch)），这里只取通道表达式。
                    let ch_expr = match &arm.chan {
                        Expr::Call(fname, cargs) if fname == "recv" && cargs.len() == 1 => &cargs[0],
                        _ => {
                            return Err(DialcError::new(
                                "内部错误：select 的 case 必须是 recv(ch) 形式",
                                *line,
                                *col,
                            ))
                        }
                    };
                    let ch = self.gen_expr(
                        ch_expr,
                        function,
                        scopes,
                        func_values,
                        functions,
                        structs,
                    )?;
                    self.call_c_void("dial_sel_add", &[sel.into(), ch.into()]);
                }
                self.builder.build_unconditional_branch(loop_bb).unwrap();

                // ---- loop：挑一个就绪通道 ----
                self.builder.position_at_end(loop_bb);
                // 先读 epoch，再挑通道：若两者之间发生了变化，epoch 已经变了，
                // 后面的 wait 会立即返回而不是带着过期假设睡死（不丢唤醒）。
                let epoch = self.call_c("dial_chan_epoch", &[]);
                let idx = self.call_c("dial_sel_pick", &[sel.into()]);
                let is_eof = self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        idx.into_int_value(),
                        self.context.i64_type().const_int((-2i64) as u64, true),
                        "sel.eof",
                    )
                    .unwrap();
                let is_ready = self
                    .builder
                    .build_int_compare(
                        IntPredicate::SGE,
                        idx.into_int_value(),
                        self.context.i64_type().const_int(0, false),
                        "sel.ready",
                    )
                    .unwrap();
                self.builder
                    .build_conditional_branch(is_eof, eof_bb, chk_bb)
                    .unwrap();

                // ---- chk：三路分支的中间块（LLVM 的条件分支只有两路）----
                self.builder.position_at_end(chk_bb);
                self.builder
                    .build_conditional_branch(is_ready, dispatch_bb, nowait_bb)
                    .unwrap();

                // ---- dispatch：按就绪下标分派 ----
                self.builder.position_at_end(dispatch_bb);
                let mut switch_cases: Vec<(IntValue<'ctx>, BasicBlock<'ctx>)> = Vec::new();
                let mut take_blocks: Vec<BasicBlock<'ctx>> = Vec::new();
                let mut case_blocks: Vec<BasicBlock<'ctx>> = Vec::new();
                for i in 0..n_arms {
                    let tb = self
                        .context
                        .append_basic_block(function, &format!("select.take{}", i));
                    let cb = self
                        .context
                        .append_basic_block(function, &format!("select.case{}", i));
                    switch_cases.push((self.context.i64_type().const_int(i as u64, false), tb));
                    take_blocks.push(tb);
                    case_blocks.push(cb);
                }
                // 理论上不会落到 default（下标必在范围内），落回 loop 重来。
                self.builder
                    .build_switch(idx.into_int_value(), loop_bb, &switch_cases)
                    .unwrap();

                // ---- 每个 case 的 take / case 块 ----
                for (i, arm) in arms.iter().enumerate() {
                    // take：从第 i 个通道非阻塞取一个元素
                    self.builder.position_at_end(take_blocks[i]);
                    let v = self.call_c(
                        "dial_sel_recv",
                        &[sel.into(), self.context.i64_type().const_int(i as u64, false).into()],
                    );
                    let vnull = self
                        .builder
                        .build_ptr_to_int(v.into_pointer_value(), self.context.i64_type(), "vp")
                        .unwrap();
                    let isnull = self
                        .builder
                        .build_int_compare(
                            IntPredicate::EQ,
                            vnull,
                            self.context.i64_type().const_int(0, false),
                            "sel.null",
                        )
                        .unwrap();
                    // NULL = 「在我们挑出它之后、被别的消费者抢走了」-> 重挑，**不是 EOF**。
                    self.builder
                        .build_conditional_branch(isnull, loop_bb, case_blocks[i])
                        .unwrap();

                    // case：绑定变量 + 执行分支体
                    self.builder.position_at_end(case_blocks[i]);
                    // 选择器到此为止已完成使命（元素已取走），立即释放。
                    // 放在分支体之前：分支体里即使 return，也不会泄漏。
                    self.call_c_void("dial_sel_free", &[sel.into()]);
                    let elem_ty = type_of(&arm.chan, scopes, functions, structs)?;
                    let llvm_ty = self.dialc_type_to_llvm(&elem_ty);
                    let var_slot = self.alloca_in_entry(function, llvm_ty, "select.var");
                    self.builder.build_store(var_slot, v).unwrap();
                    // 绑定变量的作用域严格限制在本 case 的块内。
                    scopes.push(HashMap::new());
                    scopes
                        .last_mut()
                        .unwrap()
                        .insert(arm.var.clone(), (elem_ty, (true, var_slot)));
                    for s in &arm.body {
                        self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                        if self.is_terminated() {
                            break;
                        }
                    }
                    let done = !self.is_terminated();
                    if done {
                        // 分支体结束 -> 释放绑定变量（它独占着一个从通道取来的元素）。
                        self.release_scope_locals(scopes.last().unwrap(), structs);
                        self.builder.build_unconditional_branch(end_bb).unwrap();
                    }
                    scopes.pop();
                }

                // ---- nowait：没有就绪的通道 ----
                self.builder.position_at_end(nowait_bb);
                if default_body.is_some() {
                    let def_bb = self.context.append_basic_block(function, "select.default");
                    self.builder.build_unconditional_branch(def_bb).unwrap();
                    // ---- default：没有任何通道就绪时执行（非阻塞语义）----
                    self.builder.position_at_end(def_bb);
                    self.call_c_void("dial_sel_free", &[sel.into()]);
                    scopes.push(HashMap::new());
                    for s in default_body.as_ref().unwrap() {
                        self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                        if self.is_terminated() {
                            break;
                        }
                    }
                    let done = !self.is_terminated();
                    if done {
                        self.release_scope_locals(scopes.last().unwrap(), structs);
                        self.builder.build_unconditional_branch(end_bb).unwrap();
                    }
                    scopes.pop();
                } else {
                    // 无 default：阻塞等待事件（事件驱动，非轮询）。
                    self.call_c_void("dial_chan_wait", &[epoch.into()]);
                    self.builder.build_unconditional_branch(loop_bb).unwrap();
                }

                // ---- eof：全体通道已关闭且已空 -> select 直接结束，不执行任何分支 ----
                self.builder.position_at_end(eof_bb);
                self.call_c_void("dial_sel_free", &[sel.into()]);
                self.builder.build_unconditional_branch(end_bb).unwrap();

                // ---- end：select 之后的统一续跑点 ----
                self.builder.position_at_end(end_bb);
                Ok(())
            }
            Stmt::Block { body, .. } => {
                // 裸块：引入新词法作用域，递归发射内部语句；离开作用域时释放块内堆类型局部变量，
                // 防止数组/字符串/映射/错误/通道等 ARC 对象泄漏（与 if/for 的处理一致）。
                scopes.push(HashMap::new());
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                Ok(())
            }
            Stmt::Expr(e, ..) => {
                // 语句级方法调用且返回新容器指针（push/set）-> 把结果写回接收变量
                if let Expr::Method(obj, method, args) = e {
                    if matches!(method.as_str(), "push" | "set" | "put") {
                        if let Expr::Var(name) = obj.as_ref() {
                            let info = scopes
                                .iter()
                                .rev()
                                .find_map(|s| s.get(name).map(|v| v.clone()));
                            if let Some((_ty, (_m, ptr))) = info {
                                let newp = self.gen_method_call(obj, method, args, function, scopes, func_values, functions, structs)?;
                                // 同上：push/set 返回的是替换指针，禁止对旧指针 release。
                                self.builder.build_store(ptr, newp).unwrap();
                                return Ok(());
                            }
                        }
                    }
                }
                self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                Ok(())
            }
        }
    }

    /// 求结构体字段的 (索引, 类型)。
    fn field_info(
        &self,
        ot: &Type,
        field: &str,
        structs: &StructTable,
        line: usize,
        col: usize,
    ) -> Result<(u32, Type)> {
        if let Type::Struct(sname) = ot {
            let fields = structs
                .get(sname)
                .ok_or_else(|| DialcError::new(format!("未知结构体 '{}'", sname), line, col))?;
            fields
                .iter()
                .enumerate()
                .find(|(_, (f, _))| f == field)
                .map(|(i, (_, t))| (i as u32, t.clone()))
                .ok_or_else(|| DialcError::new(format!("结构体 '{}' 无字段 '{}'", sname, field), line, col))
        } else {
            Err(DialcError::new(
                format!("类型 '{}' 没有字段 '{}'", ot.name(), field),
                line, col,
            ))
        }
    }

    /// 由类型取得其 LLVM 结构体类型（仅对 struct 有效）。
    fn get_struct_type(&self, ot: &Type) -> Result<StructType<'ctx>> {
        match ot {
            Type::Struct(name) => Ok(self.struct_types[name]),
            other => Err(DialcError::new(
                format!("语义错误：类型 '{}' 不是结构体，无法做字段访问", other.name()),
                0,
                0,
            )),
        }
    }

    /// 生成“字段/数组元素”的 lvalue 指针（仅支持 Var / Field 链）。
    fn gen_lvalue_ptr(
        &self,
        e: &Expr,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<PointerValue<'ctx>> {
        match e {
            Expr::Var(name) => {
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", name), 0, 0))?;
                Ok(info.1 .1)
            }
            Expr::Field(obj, field) => {
                let base = self.gen_lvalue_ptr(obj, function, scopes, func_values, functions, structs)?;
                let ot = type_of(obj, scopes, functions, structs)?;
                let sty = self.get_struct_type(&ot)?;
                let (fidx, _fty) = self.field_info(&ot, field, structs, 0, 0)?;
                Ok(self.builder.build_struct_gep(sty, base, fidx, "fgep").unwrap())
            }
            _ => Err(DialcError::new(
                "语义错误：仅支持对变量/字段进行赋值",
                0, 0,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_expr(
        &self,
        expr: &Expr,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        match expr {
            Expr::IntLit(n) => Ok(self.context.i64_type().const_int(*n as u64, false).into()),
            // char 字面量就是一个 i64 码点常量，与 IntLit 完全同构——
            // 差异只存在于**类型层面**（Sema 已拦住把它当数字用的写法）。
            Expr::CharLit(c) => Ok(self.context.i64_type().const_int(*c as u64, false).into()),
            // 字节字面量同样是 i64 常量（0~255，词法阶段已校验范围）——
            // 与 char 的差异只在**类型层面**。
            Expr::ByteLit(b) => Ok(self.context.i64_type().const_int(*b as u64, false).into()),
            Expr::FloatLit(f) => Ok(self.context.f64_type().const_float(*f).into()),
            Expr::BoolLit(b) => Ok(self.context.bool_type().const_int(if *b { 1 } else { 0 }, false).into()),
            Expr::StringLit(s) => {
                let name = format!("dialc.str.{}", self.next_str_id());
                let g = self.global_string(&name, s);
                Ok(self.call_c("dial_string_new", &[g.into()]))
            }
            Expr::Var(name) => {
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", name), 0, 0))?;
                let (ty, (_m, ptr)) = info;
                let llvm_ty = self.dialc_type_to_llvm(&ty);
                Ok(self.builder.build_load(llvm_ty, ptr, name).unwrap())
            }
            // 函数引用只在 spawn 首参位置出现；那里由 Call("spawn") 分支直接消费名字，
            // 不会走到这里。落到此处说明它被用在了别的位置——语义阶段已拒绝，此处为防御。
            Expr::FuncRef(name) => Err(DialcError::new(
                format!(
                    "内部错误：函数引用 '{}' 只能作为 spawn 的第一个参数，不应参与求值",
                    name
                ),
                0,
                0,
            )),
            Expr::Field(obj, field) => {
                let lptr = self.gen_lvalue_ptr(obj, function, scopes, func_values, functions, structs)?;
                let ot = type_of(obj, scopes, functions, structs)?;
                let sty = self.get_struct_type(&ot)?;
                let (fidx, fty) = self.field_info(&ot, field, structs, 0, 0)?;
                let fgep = self.builder.build_struct_gep(sty, lptr, fidx, "fgep").unwrap();
                Ok(self.builder.build_load(self.dialc_type_to_llvm(&fty), fgep, "fld").unwrap())
            }
            Expr::Index(arr, idx) => {
                let at = type_of(arr, scopes, functions, structs)?;
                let aptr = self.gen_expr(arr, function, scopes, func_values, functions, structs)?;
                let ival = self.gen_expr(idx, function, scopes, func_values, functions, structs)?;
                let ii64 = self.to_i64(ival, &type_of(idx, scopes, functions, structs)?);
                let elem_t = match at {
                    Type::Array(e) => *e,
                    _ => Type::Int,
                };
                // Fail-Fast：只读也要检查——越界读会把相邻内存当成合法数据返回，
                // 让 Bug 在几万行之后才爆发。ReleaseFast 下整段消除。
                if self.checks_enabled() {
                    let len = self.call_c("dial_array_length", &[aptr.into()]);
                    self.emit_bounds_check(function, len.into_int_value(), ii64);
                }
                let res = self.call_c("dial_array_get", &[aptr.into(), ii64.into()]);
                // 元素是 int（v0.2）；若将来支持其他元素类型在此分支
                let _ = elem_t;
                Ok(res)
            }
            Expr::ArrayLit(elems) => {
                let mut cur = self.call_c("dial_array_new", &[]);
                for el in elems {
                    let ev = self.gen_expr(el, function, scopes, func_values, functions, structs)?;
                    let ei64 = self.to_i64(ev, &type_of(el, scopes, functions, structs)?);
                    cur = self.call_c("dial_array_push", &[cur.into(), ei64.into()]);
                }
                Ok(cur)
            }
            Expr::StructLit(name, fields) => {
                let sty = self.struct_types[name];
                let sdef = structs[name].clone();
                let ptr = self.alloca_in_entry(function, sty.into(), "slit");
                // 零初始化：结构体字面量允许省略字段，省略者取其类型的零值（确定性语义，杜绝未初始化内存）
                let zero = sty.const_zero();
                self.builder.build_store(ptr, zero).unwrap();
                // 按字段名定位下标，而非字面量书写顺序（顺序无关，杜绝错位）
                for (fname, fe) in fields.iter() {
                    let fidx = sdef
                        .iter()
                        .position(|(n, _)| n == fname)
                        .ok_or_else(|| {
                            DialcError::new(
                                format!("语义错误：结构体 '{}' 不存在字段 '{}'", name, fname),
                                0, 0,
                            )
                        })?;
                    let fty = sdef[fidx].1.clone();
                    let gep = self
                        .builder
                        .build_struct_gep(sty, ptr, fidx as u32, "fgep")
                        .unwrap();
                    let fv = self.gen_expr(fe, function, scopes, func_values, functions, structs)?;
                    let stored = self.coerce(fv, &type_of(fe, scopes, functions, structs)?, &fty);
                    self.builder.build_store(gep, stored).unwrap();
                }
                Ok(self.builder.build_load(sty, ptr, "sval").unwrap())
            }
            Expr::Unary(op, e) => match op {
                UnOp::Neg => {
                    let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                    if v.is_float_value() {
                        Ok(self.builder.build_float_neg(v.into_float_value(), "neg").unwrap().into())
                    } else {
                        Ok(self.builder.build_int_neg(v.into_int_value(), "neg").unwrap().into())
                    }
                }
                UnOp::Not => {
                    let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                    Ok(self.builder.build_not(v.into_int_value(), "not").unwrap().into())
                }
            },
            Expr::Method(obj, method, args) => self.gen_method_call(obj, method, args, function, scopes, func_values, functions, structs),
            Expr::Binary(left, op, right) => self.gen_binary(*op, left, right, function, scopes, func_values, functions, structs),
            Expr::Call(name, args) => {
                // 内置类型转换函数：与零隐式转换铁律配合，是 int<->float 的唯一合法通道。
                // 直接编译为 LLVM 的 sitofp / fptosi，不依赖任何 C 运行时。
                match name.as_str() {
                    "int_to_float" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self
                            .builder
                            .build_signed_int_to_float(
                                v.into_int_value(),
                                self.context.f64_type(),
                                "itof",
                            )
                            .unwrap()
                            .into())
                    }
                    // char<->int 是**零指令**的类型视图切换：char 与 int 同为 i64，
                    // 因此这里直接原样返回实参值，不发射任何 LLVM 指令。
                    // 它不是「隐式转换」，因为源码里必须显式写出 char_to_int / int_to_char。
                    "char_to_int" | "int_to_char" => {
                        self.gen_expr(&args[0], function, scopes, func_values, functions, structs)
                    }
                    // byte -> int 是**零指令**的类型视图切换（byte 与 int 同为 i64）。
                    "byte_to_int" => {
                        self.gen_expr(&args[0], function, scopes, func_values, functions, structs)
                    }
                    // int -> byte 必须**带范围校验**（0~255）：越界时 Fail-Fast，
                    // 绝不静默截断（那是被伪装成合法值的数据损坏）。
                    "int_to_byte" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_int_to_byte", &[v.into()]))
                    }
                    // bytes（二进制缓冲区，v0.98.2）：与 string 显式互转。
                    "bytes_from_string" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_bytes_from_string", &[v.into()]))
                    }
                    "bytes_to_string" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_bytes_to_string", &[v.into()]))
                    }
                    "bytes_new" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_bytes_zero", &[v.into()]))
                    }
                    "bytes_length" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_bytes_length", &[v.into()]))
                    }
                    "bytes_get" => {
                        let b = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let i = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_bytes_get", &[b.into(), i.into()]))
                    }
                    // 类型 -> 字符串（CVE-2024-DIAL-006）：int/float 经 C 侧格式化，string 原样返回。
                    "int_to_string" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_int_to_string", &[v.into()]))
                    }
                    "float_to_string" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_float_to_string", &[v.into()]))
                    }
                    "float_to_int" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self
                            .builder
                            .build_float_to_signed_int(
                                v.into_float_value(),
                                self.context.i64_type(),
                                "ftoi",
                            )
                            .unwrap()
                            .into())
                    }
                    "new_map" => Ok(self.call_c("dial_map_new", &[])),
                    "read_line" => Ok(self.call_c("dial_read_line", &[])),
                    // 文件 IO（v0.61）：file 句柄 = 不透明 ptr，不做 retain/release，显式 close。
                    // v0.7 起：失败即抛出 Error（业务失败与系统崩溃分离——越界/除零仍走 ud2/Fail-Fast）。
                    "open" => {
                        let p = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let m = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                        let f = self.call_c("dial_file_open", &[p.into(), m.into()]);
                        // 返回 NULL = 打开失败 -> 抛出 Error（无 try 包裹则升级为 panic 终止）。
                        let f_int = self.builder.build_ptr_to_int(f.into_pointer_value(), self.context.i64_type(), "fint").unwrap();
                        let is_null = self.builder.build_int_compare(IntPredicate::EQ, f_int, self.context.i64_type().const_int(0, false), "opennull").unwrap();
                        let ok_bb = self.context.append_basic_block(function, "open.ok");
                        let fail_bb = self.context.append_basic_block(function, "open.fail");
                        self.builder.build_conditional_branch(is_null, fail_bb, ok_bb).unwrap();
                        self.builder.position_at_end(fail_bb);
                        let g = self.global_string("dialc.err.open", "无法打开文件（路径不存在或无访问权限）");
                        let msg = self.call_c("dial_string_new", &[g.into()]);
                        let err = self.call_c("dial_error_new", &[msg.into()]);
                        self.emit_throw(err, scopes, structs)?;
                        self.builder.position_at_end(ok_bb);
                        Ok(f)
                    }
                    "read" => {
                        let f = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let n = self.to_i64(
                            self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?,
                            &type_of(&args[1], scopes, functions, structs)?,
                        );
                        let s = self.call_c("dial_file_read", &[f.into(), n.into()]);
                        // 返回 NULL = 读取失败（句柄无效/分配失败）-> 抛出 Error。
                        let s_int = self.builder.build_ptr_to_int(s.into_pointer_value(), self.context.i64_type(), "sint").unwrap();
                        let is_null = self.builder.build_int_compare(IntPredicate::EQ, s_int, self.context.i64_type().const_int(0, false), "readnull").unwrap();
                        let ok_bb = self.context.append_basic_block(function, "read.ok");
                        let fail_bb = self.context.append_basic_block(function, "read.fail");
                        self.builder.build_conditional_branch(is_null, fail_bb, ok_bb).unwrap();
                        self.builder.position_at_end(fail_bb);
                        let g = self.global_string("dialc.err.read", "读取文件失败（句柄无效或内存不足）");
                        let msg = self.call_c("dial_string_new", &[g.into()]);
                        let err = self.call_c("dial_error_new", &[msg.into()]);
                        self.emit_throw(err, scopes, structs)?;
                        self.builder.position_at_end(ok_bb);
                        Ok(s)
                    }
                    "write" => {
                        let f = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let c = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                        let w = self.call_c("dial_file_write", &[f.into(), c.into()]);
                        // 返回 -1 = 写入失败 -> 抛出 Error。
                        let is_fail = self.builder.build_int_compare(IntPredicate::EQ, w.into_int_value(), self.context.i64_type().const_int(-1i64 as u64, false), "writefail").unwrap();
                        let ok_bb = self.context.append_basic_block(function, "write.ok");
                        let fail_bb = self.context.append_basic_block(function, "write.fail");
                        self.builder.build_conditional_branch(is_fail, fail_bb, ok_bb).unwrap();
                        self.builder.position_at_end(fail_bb);
                        let g = self.global_string("dialc.err.write", "写入文件失败（句柄无效或写入中断）");
                        let msg = self.call_c("dial_string_new", &[g.into()]);
                        let err = self.call_c("dial_error_new", &[msg.into()]);
                        self.emit_throw(err, scopes, structs)?;
                        self.builder.position_at_end(ok_bb);
                        Ok(w)
                    }
                    // close 按实参的静态类型分派：file 走 fclose，channel 走关闭广播。
                    // 二者语义不同（前者真的释放资源，后者只置标志 + 唤醒等待者，
                    // 通道对象仍由原子 ARC 销毁），但都是「显式终结」，故共用一个名字。
                    "close" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        match type_of(&args[0], scopes, functions, structs)? {
                            Type::File => self.call_c_void("dial_file_close", &[v.into()]),
                            Type::Channel(_) => self.call_c_void("dial_channel_close", &[v.into()]),
                            other => {
                                return Err(DialcError::new(
                                    format!("内部错误：close 收到了 {} 类型的实参", other.name()),
                                    0,
                                    0,
                                ))
                            }
                        }
                        Ok(self.context.i64_type().const_int(0, false).into())
                    }
                    // 可恢复异常（v0.7）：Error("msg") 构造一个带引用计数的错误对象（ARC 类型，见 §26）。
                    // 严禁裸的 int/string —— 该约束由 Sema 在编译期拦截；此处 msg 必为 string。
                    "Error" => {
                        let msg =
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_error_new", &[msg.into()]))
                    }

                    // ==================================================================
                    //  所有权转移与多线程并发（v0.8）
                    // ==================================================================
                    "channel" => {
                        let cap = self.to_i64(
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?,
                            &Type::Int,
                        );
                        // 元素析构器：按通道元素类型分派（CVE-2024-DIAL-007）。
                        // 嵌套通道走 dial_channel_release（原子 rc），其余走 dial_rc_release。
                        let inner = match type_of(expr, scopes, functions, structs)? {
                            Type::Channel(inner) => *inner,
                            _ => Type::String, // 防御性兜底，正常不会到这
                        };
                        let elem_release = self.elem_release_fn(&inner);
                        Ok(self.call_c("dial_channel_new", &[cap.into(), elem_release.into()]))
                    }
                    // 取出一个元素的所有权：队列里存的就是裸指针，弹出即归接收者所有（不做 retain）。
                    //
                    // v0.81 EOF：通道已关闭且已空时，recv / try_recv 返回 NULL（不再永久阻塞）。
                    // NULL 意味着「没有值」——用户应当用 done(ch) 守在循环条件上，
                    // 这样正常路径永远取不到 NULL；一旦取到并继续使用，Runtime 会在
                    // 使用点（例如 print）当场 Fail-Fast，而不是让它伪装成空值流下去。
                    "recv" | "try_recv" => {
                        let ch =
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c(
                            if name == "recv" {
                                "dial_channel_recv"
                            } else {
                                "dial_channel_try_recv"
                            },
                            &[ch.into()],
                        ))
                    }
                    // recv_timeout(ch, ms)：带超时的接收。超时或已关闭且已空时返回 NULL，
                    // 用 is_null(x) 判别。超时由 C 侧 pthread_cond_timedwait 实现，
                    // IR 层只是多传一个毫秒数。
                    "recv_timeout" => {
                        let ch =
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let ms = self.to_i64(
                            self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?,
                            &Type::Int,
                        );
                        Ok(self.call_c("dial_channel_recv_timeout", &[ch.into(), ms.into()]))
                    }
                    // is_null(x)：句柄判空。直接落到 LLVM 的指针比较上——
                    // ptrtoint 后与 0 比较，instcombine 会把它折叠回 icmp eq ptr %p, null。
                    // 全程不产生任何函数调用，也不碰引用计数。
                    "is_null" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let p = v.into_pointer_value();
                        let i = self
                            .builder
                            .build_ptr_to_int(p, self.context.i64_type(), "pint")
                            .unwrap();
                        let b = self
                            .builder
                            .build_int_compare(
                                IntPredicate::EQ,
                                i,
                                self.context.i64_type().const_int(0, false),
                                "isnull",
                            )
                            .unwrap();
                        Ok(b.into())
                    }
                    // 通道状态查询（v0.81）。C 侧统一返回 int64，此处按 Dial 的静态类型收口：
                    // closed / done 是 bool（LLVM i1），len 是 int（i64）。
                    "closed" | "done" => {
                        let ch =
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let r = self.call_c(
                            if name == "closed" {
                                "dial_channel_closed"
                            } else {
                                "dial_channel_done"
                            },
                            &[ch.into()],
                        );
                        let b = self
                            .builder
                            .build_int_compare(
                                IntPredicate::NE,
                                r.into_int_value(),
                                self.context.i64_type().const_int(0, false),
                                "cheof",
                            )
                            .unwrap();
                        Ok(b.into())
                    }
                    "len" => {
                        let ch =
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_channel_len", &[ch.into()]))
                    }
                    // move_to(变量, 通道)：**编译期 intrinsic**，不生成任何函数调用栈。
                    // 三条 IR 指令完成所有权移交：取指针 -> 入队 -> 原槽清零。
                    // 全程零 retain / 零 release（所有权完整交给通道，计数不变）。
                    "move_to" => {
                        let src = match &args[0] {
                            Expr::Var(n) => n.clone(),
                            _ => {
                                return Err(DialcError::new(
                                    "内部错误：move_to 的第一个参数必须是变量",
                                    0,
                                    0,
                                ))
                            }
                        };
                        let slot = Self::lookup_slot(scopes, &src).ok_or_else(|| {
                            DialcError::new(format!("未声明的变量 '{}'", src), 0, 0)
                        })?;
                        let ety = type_of(&args[0], scopes, functions, structs)?;
                        let llvm_ty = self.dialc_type_to_llvm(&ety);
                        let v = self.builder.build_load(llvm_ty, slot, "mv").unwrap();
                        let ch =
                            self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                        self.call_c_void("dial_channel_send", &[ch.into(), v.into()]);
                        // 原变量所在的栈槽清零 —— 从这一刻起它不再持有任何东西。
                        self.builder
                            .build_store(slot, self.i8ptr().const_null())
                            .unwrap();
                        // 标记 moved：作用域结束时跳过该槽的 release（其所有权已随指针出走）。
                        self.moved.borrow_mut().insert(slot);
                        Ok(self.context.i64_type().const_int(0, false).into())
                    }
                    "join" | "detach" => {
                        let t =
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        self.call_c_void(
                            if name == "join" {
                                "dial_thread_join"
                            } else {
                                "dial_thread_detach"
                            },
                            &[t.into()],
                        );
                        // 句柄已被 C 侧回收：把槽置 null，使二次 join/detach 当场 Fail-Fast
                        // 而不是退化成 use-after-free。
                        if let Expr::Var(n) = &args[0] {
                            if let Some(p) = Self::lookup_slot(scopes, n) {
                                self.builder
                                    .build_store(p, self.i8ptr().const_null())
                                    .unwrap();
                            }
                        }
                        Ok(self.context.i64_type().const_int(0, false).into())
                    }
                    // spawn(fn, args...)：**先把实参全部写进 argcell，再创建线程**。
                    // 顺序是这个语言的并发安全基石——pthread_create 之前实参已就位，
                    // 因此不存在「先启动再移交」的竞争窗口。
                    "spawn" => {
                        let fname = match &args[0] {
                            Expr::FuncRef(n) => n.clone(),
                            _ => {
                                return Err(DialcError::new(
                                    "内部错误：spawn 的第一个参数必须是函数名",
                                    0,
                                    0,
                                ))
                            }
                        };
                        let target = functions.get(&fname).ok_or_else(|| {
                            DialcError::new(format!("未定义的函数 '{}'", fname), 0, 0)
                        })?;
                        let n = target.params.len();
                        let cell = self.call_c(
                            "dial_thread_argcell",
                            &[self.context.i64_type().const_int(n as u64, false).into()],
                        );
                        for (i, pty) in target.params.iter().enumerate() {
                            let v = self.gen_expr(
                                &args[i + 1],
                                function,
                                scopes,
                                func_values,
                                functions,
                                structs,
                            )?;
                            let idx = self.context.i64_type().const_int(i as u64, false);
                            // 槽宽固定 8 字节：指针 / i64 / f64 都能直接 store；
                            // bool(i1) 先 zext 成 i64 再存（thunk 侧 trunc 回来）。
                            let slot = unsafe {
                                match pty {
                                    Type::Float => self.builder.build_gep(
                                        self.context.f64_type(),
                                        cell.into_pointer_value(),
                                        &[idx],
                                        "sp.slot",
                                    ),
                                    Type::Int | Type::Bool => self.builder.build_gep(
                                        self.context.i64_type(),
                                        cell.into_pointer_value(),
                                        &[idx],
                                        "sp.slot",
                                    ),
                                    _ => self.builder.build_gep(
                                        self.i8ptr(),
                                        cell.into_pointer_value(),
                                        &[idx],
                                        "sp.slot",
                                    ),
                                }
                                .unwrap()
                            };
                            match pty {
                                // 移动：指针入槽 + 源槽清零 + 源槽标 moved（无 retain / release）
                                Type::String | Type::Array(_) | Type::Map | Type::Error => {
                                    self.builder.build_store(slot, v).unwrap();
                                    if let Expr::Var(s) = &args[i + 1] {
                                        if let Some(sp) = Self::lookup_slot(scopes, s) {
                                            self.builder
                                                .build_store(sp, self.i8ptr().const_null())
                                                .unwrap();
                                            self.moved.borrow_mut().insert(sp);
                                        }
                                    }
                                }
                                // 共享：通道天生要被两端持有，因此原子 +1，源变量仍可用。
                                Type::Channel(_) => {
                                    self.retain(pty, v);
                                    self.builder.build_store(slot, v).unwrap();
                                }
                                Type::Bool => {
                                    let w = self.to_i64(v, &Type::Bool);
                                    self.builder.build_store(slot, w).unwrap();
                                }
                                _ => {
                                    self.builder.build_store(slot, v).unwrap();
                                }
                            }
                        }
                        let thunk = self.gen_spawn_thunk(
                            &fname,
                            &target.params,
                            func_values,
                            structs,
                        )?;
                        // 函数值必须先转成指针值才能作为参数传递（IR 里线程入口就是个 ptr）。
                        let thunk_ptr = thunk.as_global_value().as_pointer_value();
                        Ok(self.call_c("dial_thread_new", &[thunk_ptr.into(), cell.into()]))
                    }
                    _ => {
                        if name == "print" {
                            let arg = &args[0];
                            let ty = type_of(arg, scopes, functions, structs)?;
                            let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                            self.emit_print(&ty, val)?;
                            Ok(self.context.i64_type().const_int(0, false).into())
                        } else if name == "str" {
                            // 统一入口：按实参类型分派到 int_to_string / float_to_string /
                            // char_to_string / 字符串自身。
                            let arg = &args[0];
                            let ty = type_of(arg, scopes, functions, structs)?;
                            let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                            match ty {
                                Type::Int => Ok(self.call_c("dial_int_to_string", &[val.into()])),
                                Type::Float => Ok(self.call_c("dial_float_to_string", &[val.into()])),
                                // char -> 1~4 字节的 UTF-8 字符串（v0.98.2）。
                                Type::Char => Ok(self.call_c("dial_char_to_string", &[val.into()])),
                                Type::String => Ok(val), // 字符串自身，无需转换
                                _ => Err(DialcError::new(
                                    "内部错误：str 收到了非 int/float/char/string 的实参（语义层应已拦截）",
                                    0, 0,
                                )),
                            }
                        } else if name == "print_raw" {
                            // 原样输出、**不换行**（v0.98.2）。
                            // 与 print 走同一套类型分派（emit_print），差别只在末尾那个 '\n'：
                            // print_raw 走 emit_print_raw，末尾不补换行。
                            let arg = &args[0];
                            let ty = type_of(arg, scopes, functions, structs)?;
                            let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                            self.emit_print_raw(&ty, val)?;
                            Ok(self.context.i64_type().const_int(0, false).into())
                        } else if name == "write_char" {
                            // 输出单个字符、**不换行**（v0.98.2）。
                            // 语义层已保证实参是 char，这里不再重复校验类型。
                            let arg = &args[0];
                            let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                            self.call_c_void("dial_write_char", &[val.into()]);
                            Ok(self.context.i64_type().const_int(0, false).into())
                        } else {
                            let _info = functions.get(name).ok_or_else(|| {
                                DialcError::new(format!("调用了未定义的函数 '{}'", name), 0, 0)
                            })?;
                            let fv = func_values.get(name).ok_or_else(|| {
                                DialcError::new(format!("缺少函数 '{}' 的 LLVM 定义", name), 0, 0)
                            })?;
                            let mut arg_vals = Vec::new();
                            for a in args {
                                arg_vals.push(self.gen_expr(a, function, scopes, func_values, functions, structs)?);
                            }
                            let metadata: Vec<BasicMetadataValueEnum> =
                                arg_vals.iter().map(|v| (*v).into()).collect();
                            let call = self.builder.build_call(*fv, &metadata, name).unwrap();
                            // void 调用（v0.92）：`close(f);` / `demo_free(h);` 这类
                            // 「只做事、不给值」的调用是包生态里最常见的形态。
                            // 这里发射调用后返回一个占位值——**它只可能出现在语句位置**，
                            // 因为「把 void 当值用」（赋值给变量、参与运算、作为实参）
                            // 已由 **语义层** 用 types_compatible 挡住（void 与任何类型都不兼容）。
                            // 于是「返回 void 却仍能写成一个语句」与「void 不能被当值」
                            // 两条规则各司其职，不必在 codegen 里重复判定。
                            let rv = match call.try_as_basic_value().basic() {
                                Some(v) => v,
                                None => self.context.i64_type().const_int(0, false).into(),
                            };
                            Ok(rv)
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_method_call(
        &self,
        obj: &Expr,
        method: &str,
        args: &[Expr],
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        let ot = type_of(obj, scopes, functions, structs)?;
        let optr = self.gen_expr(obj, function, scopes, func_values, functions, structs)?;
        let i64t = self.context.i64_type();
        match (&ot, method) {
            (Type::String, "length") => Ok(self.call_c("dial_string_length", &[optr.into()])),
            (Type::String, "concat") => {
                let b = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                Ok(self.call_c("dial_string_concat", &[optr.into(), b.into()]))
            }
            (Type::String, "get") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_string_get", &[optr.into(), i.into()]))
            }
            (Type::String, "substring") => {
                let from = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let to = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &type_of(&args[1], scopes, functions, structs)?);
                Ok(self.call_c("dial_string_substring", &[optr.into(), from.into(), to.into()]))
            }
            (Type::String, "clone") => Ok(self.call_c("dial_string_clone", &[optr.into()])),
            (Type::String, "eq") => {
                let b = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                let r = self.call_c("dial_string_eq", &[optr.into(), b.into()]);
                let zero = i64t.const_int(0, false);
                Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), zero, "eq").unwrap().into())
            }
            (Type::Array(_), "length") => Ok(self.call_c("dial_array_length", &[optr.into()])),
            (Type::Array(_), "push") => {
                let v = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_array_push", &[optr.into(), v.into()]))
            }
            (Type::Array(_), "get") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_array_get", &[optr.into(), i.into()]))
            }
            (Type::Array(_), "set") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let v = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &type_of(&args[1], scopes, functions, structs)?);
                Ok(self.call_c("dial_array_set", &[optr.into(), i.into(), v.into()]))
            }
            (Type::Array(_), "clone") => Ok(self.call_c("dial_array_clone", &[optr.into()])),
            (Type::Map, "put") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let v = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &type_of(&args[1], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_put", &[optr.into(), k.into(), v.into()]))
            }
            (Type::Map, "get") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_get", &[optr.into(), k.into()]))
            }
            (Type::Map, "contains") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let r = self.call_c("dial_map_contains", &[optr.into(), k.into()]);
                Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), i64t.const_int(0, false), "mcontains").unwrap().into())
            }
            (Type::Map, "remove") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_remove", &[optr.into(), k.into()]))
            }
            (Type::Map, "size") => Ok(self.call_c("dial_map_size", &[optr.into()])),
            (Type::Error, "msg") => Ok(self.call_c("dial_error_msg", &[optr.into()])),
            _ => Err(DialcError::new(
                format!("语义错误：类型 '{}' 无方法 '{}'（应由 Sema 先行拦截）", ot.name(), method),
                0, 0,
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_binary(
        &self,
        op: BinOp,
        l: &Expr,
        r: &Expr,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        match op {
            BinOp::And | BinOp::Or => {
                // 短路求值（与 C 的 && / || 一致，这是现代语言的常识）：
                //   AND：先求左操作数；若为假，结果即为假，不再求值右操作数；否则结果等于右操作数。
                //   OR ：先求左操作数；若为真，结果即为真，不再求值右操作数；否则结果等于右操作数。
                // 若照搬 build_and / build_or，两侧会被同时求值，既违背短路语义，也会让
                // “判空 + 副作用”这类代码在左操作数为假时崩溃。
                //
                // 实现：结果写入 entry 块的栈槽，两条分支各自 store，merge 处 load 出来
                // （不依赖 PhiValue，规避其不属于 BasicValueEnum 的 API 限制）。
                let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                let lbit = lval.into_int_value();
                let bool_t = self.context.bool_type();
                let slot = self.alloca_in_entry(function, bool_t.into(), "sc.res");
                let eval_bb = self.context.append_basic_block(function, "sc.eval");
                let short_bb = self.context.append_basic_block(function, "sc.short");
                let merge_bb = self.context.append_basic_block(function, "sc.merge");
                // 短路分支直接给出的常量结果：AND 短路为假，OR 短路为真。
                let short_const = bool_t.const_int(if op == BinOp::And { 0 } else { 1 }, false);
                if op == BinOp::And {
                    self.builder.build_conditional_branch(lbit, eval_bb, short_bb).unwrap();
                } else {
                    self.builder.build_conditional_branch(lbit, short_bb, eval_bb).unwrap();
                }
                // 求值分支：求右操作数并写入结果槽
                self.builder.position_at_end(eval_bb);
                let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                let rbit = rval.into_int_value();
                self.builder.build_store(slot, rbit).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                // 短路分支：直接写入常量结果
                self.builder.position_at_end(short_bb);
                self.builder.build_store(slot, short_const).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                // merge：取出结果
                self.builder.position_at_end(merge_bb);
                let res = self.builder.build_load(bool_t, slot, "sc.res").unwrap();
                Ok(res)
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let tl = type_of(l, scopes, functions, structs)?;
                let tr = type_of(r, scopes, functions, structs)?;
                // 字符串拼接：String + String -> String（仅 + 支持，CVE-2024-DIAL-006）。
                if op == BinOp::Add && tl == Type::String && tr == Type::String {
                    let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                    let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                    return Ok(self.call_c("dial_string_concat", &[lval.into(), rval.into()]));
                }
                let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                let res_ty = if tl == Type::Float || tr == Type::Float {
                    Type::Float
                } else {
                    Type::Int
                };
                let result: BasicValueEnum = if res_ty == Type::Float {
                    let a = self.to_float(lval);
                    let b = self.to_float(rval);
                    match op {
                        BinOp::Add => self.builder.build_float_add(a, b, "add").unwrap().into(),
                        BinOp::Sub => self.builder.build_float_sub(a, b, "sub").unwrap().into(),
                        BinOp::Mul => self.builder.build_float_mul(a, b, "mul").unwrap().into(),
                        BinOp::Div => self.builder.build_float_div(a, b, "div").unwrap().into(),
                        BinOp::Mod => self.builder.build_float_rem(a, b, "rem").unwrap().into(),
                        _ => unreachable!(),
                    }
                } else {
                    let a = lval.into_int_value();
                    let b = rval.into_int_value();
                    // Fail-Fast：sdiv / srem 除数为 0 在 LLVM 中是 UB（poison），
                    // 优化器可以据此把后续代码整段删掉。必须先检查再除/取模。
                    if op == BinOp::Div || op == BinOp::Mod {
                        self.emit_div_zero_check(function, b);
                    }
                    match op {
                        BinOp::Add => self.builder.build_int_add(a, b, "add").unwrap().into(),
                        BinOp::Sub => self.builder.build_int_sub(a, b, "sub").unwrap().into(),
                        BinOp::Mul => self.builder.build_int_mul(a, b, "mul").unwrap().into(),
                        BinOp::Div => self.builder.build_int_signed_div(a, b, "div").unwrap().into(),
                        BinOp::Mod => self.builder.build_int_signed_rem(a, b, "rem").unwrap().into(),
                        _ => unreachable!(),
                    }
                };
                Ok(result)
            }
            BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                let tl = type_of(l, scopes, functions, structs)?;
                let tr = type_of(r, scopes, functions, structs)?;
                let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                if tl == Type::String && tr == Type::String {
                    // dial_string_eq 的约定是“相等返回 1、不等返回 0”，
                    // 因此 `a == b` 对应“结果 != 0”，`a != b` 对应“结果 == 0”。
                    // 注意与数值比较的方向相反，切勿照抄 IntPredicate::EQ。
                    let cmp = self.call_c("dial_string_eq", &[lval.into(), rval.into()]);
                    let zero = self.context.i64_type().const_int(0, false);
                    let pred = if op == BinOp::Eq { IntPredicate::NE } else { IntPredicate::EQ };
                    return Ok(self.builder.build_int_compare(pred, cmp.into_int_value(), zero, "streq").unwrap().into());
                }
                let result: BasicValueEnum = if tl == Type::Bool {
                    let a = lval.into_int_value();
                    let b = rval.into_int_value();
                    let pred = if op == BinOp::Eq { IntPredicate::EQ } else { IntPredicate::NE };
                    self.builder.build_int_compare(pred, a, b, "cmp").unwrap().into()
                } else if tl == Type::Float || tr == Type::Float {
                    let a = self.to_float(lval);
                    let b = self.to_float(rval);
                    let pred = match op {
                        BinOp::Eq => FloatPredicate::OEQ,
                        BinOp::Neq => FloatPredicate::ONE,
                        BinOp::Lt => FloatPredicate::OLT,
                        BinOp::Gt => FloatPredicate::OGT,
                        BinOp::Le => FloatPredicate::OLE,
                        BinOp::Ge => FloatPredicate::OGE,
                        _ => unreachable!(),
                    };
                    self.builder.build_float_compare(pred, a, b, "cmp").unwrap().into()
                } else {
                    let a = lval.into_int_value();
                    let b = rval.into_int_value();
                    let pred = match op {
                        BinOp::Eq => IntPredicate::EQ,
                        BinOp::Neq => IntPredicate::NE,
                        BinOp::Lt => IntPredicate::SLT,
                        BinOp::Gt => IntPredicate::SGT,
                        BinOp::Le => IntPredicate::SLE,
                        BinOp::Ge => IntPredicate::SGE,
                        _ => unreachable!(),
                    };
                    self.builder.build_int_compare(pred, a, b, "cmp").unwrap().into()
                };
                Ok(result)
            }
        }
    }

    fn emit_print(&self, ty: &Type, val: BasicValueEnum<'ctx>) -> Result<()> {
        match *ty {
            Type::Int => {
                self.call_c_void("dial_print_int", &[val.into()]);
            }
            Type::Float => {
                self.call_c_void("dial_print_float", &[val.into()]);
            }
            Type::Bool => {
                let i = self.to_i64(val, &Type::Bool);
                self.call_c_void("dial_print_bool", &[i.into()]);
            }
            // char 打印字符本身（'a' 输出 a），而不是它的码点 97——
            // 后者是 char_to_int(c) 的语义，两者在源码里必须能一眼区分。
            Type::Char => {
                self.call_c_void("dial_print_char", &[val.into()]);
            }
            // byte 打印**数值**（0~255），而不是那个字节代表的字符。
            // 这与 char 相反：byte 是数字，char 是字符。
            // （想按字符输出请显式 int_to_char(b) 再 print —— 零隐式转换铁律）
            Type::Byte => {
                self.call_c_void("dial_print_int", &[val.into()]);
            }
            Type::String => {
                self.call_c_void("dial_print_string", &[val.into()]);
            }
            // 与语义层保持同一口径：数组/结构体/v 均无对应打印实现，
            // 绝不能退化成 dial_print_string（那样会把二进制 payload 当字符串吐出乱码）。
            Type::Array(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 数组，请逐元素打印 print(a.get(i))",
                    0, 0,
                ));
            }
            // bytes 是二进制数据：绝不能退化成 dial_print_string（那会把原始字节当 UTF-8 吐出乱码）。
            Type::Bytes => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print bytes（二进制数据）；\
                     请先显式转换（bytes_to_string）再打印",
                    0, 0,
                ));
            }
            Type::Struct(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 结构体，请逐字段打印",
                    0, 0,
                ));
            }
            Type::Map => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 哈希表，请逐值打印 print(m.get(k))",
                    0, 0,
                ));
            }
            Type::File => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 文件句柄（file）；请改用 read 读成 string 后再打印",
                    0, 0,
                ));
            }
            // 可恢复异常（v0.7）：print(error) 打印其错误消息字符串。
            Type::Error => {
                let msg = self.call_c("dial_error_msg", &[val.into()]);
                self.call_c_void("dial_print_string", &[msg.into()]);
            }
            // 句柄打印没有任何可读语义，且违反「不透明指针」契约（打印就等于泄露内部）。
            Type::Thread => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print thread 句柄",
                    0, 0,
                ));
            }
            Type::Channel(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print channel；请 recv 出元素后再打印",
                    0, 0,
                ));
            }
            // 包句柄同样是不透明指针：打印它既没有可读语义，也等于泄露包内部结构。
            Type::Opaque(ref n) => {
                return Err(DialcError::new(
                    format!(
                        "语义错误：当前版本不支持直接 print 包句柄 '{}'（不透明指针）；\
                         请改用该包提供的取值函数",
                        n
                    ),
                    0,
                    0,
                ));
            }
            Type::Void => {
                return Err(DialcError::new("语义错误：print 的参数不能是 void", 0, 0));
            }
        }
        Ok(())
    }

    /// print_raw 的类型分派（v0.98.2）：与 `emit_print` **逐类型对应**，
    /// 唯一区别是末尾不补换行。
    ///
    /// 之所以不复用 emit_print 再想办法去掉换行：那需要在 Dial 侧
    /// 先把标量 str() 成堆字符串（一次性的中转对象却仍占引用计数 = 泄漏），
    /// 或者让 C 层返回「已输出的字节数」再回退——后者要动 stdout 的文件位置，
    /// 在管道/重定向下并不可靠。因此这里为标量单独走 C 层的无换行函数。
    fn emit_print_raw(&self, ty: &Type, val: BasicValueEnum<'ctx>) -> Result<()> {
        match *ty {
            Type::Int => {
                self.call_c_void("dial_print_raw_int", &[val.into()]);
            }
            Type::Float => {
                self.call_c_void("dial_print_raw_float", &[val.into()]);
            }
            Type::Bool => {
                let i = self.to_i64(val, &Type::Bool);
                self.call_c_void("dial_print_raw_bool", &[i.into()]);
            }
            // char 输出字符本身（不换行）；与 print(char) 的差别就是那个 '\n'。
            Type::Char => {
                self.call_c_void("dial_write_char", &[val.into()]);
            }
            // byte 输出数值（0~255），与 print(byte) 保持同一形态。
            Type::Byte => {
                self.call_c_void("dial_print_raw_int", &[val.into()]);
            }
            Type::String => {
                self.call_c_void("dial_print_raw", &[val.into()]);
            }
            // 与 emit_print 保持同一口径：数组/结构体/句柄都没有可读形态，
            // 绝不能退化成 dial_print_raw_string（那会把二进制 payload 当字符串吐出乱码）。
            Type::Array(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw 数组，请逐元素输出 print_raw(str(a.get(i)))",
                    0, 0,
                ));
            }
            // bytes 是二进制数据：绝不能退化成 dial_print_raw（那会把原始字节当字符串吐出乱码）。
            Type::Bytes => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw bytes（二进制数据）；\
                     请先显式转换（bytes_to_string）再输出",
                    0, 0,
                ));
            }
            Type::Struct(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw 结构体，请逐字段输出",
                    0, 0,
                ));
            }
            Type::Map => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw 哈希表，请逐值输出",
                    0, 0,
                ));
            }
            Type::File => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw 文件句柄（file）；请改用 read 读成 string 后再输出",
                    0, 0,
                ));
            }
            // 可恢复异常：输出其错误消息字符串（不换行）。
            Type::Error => {
                let msg = self.call_c("dial_error_msg", &[val.into()]);
                self.call_c_void("dial_print_raw", &[msg.into()]);
            }
            Type::Thread => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw thread 句柄",
                    0, 0,
                ));
            }
            Type::Channel(_) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw channel；请 recv 出元素后再输出",
                    0, 0,
                ));
            }
            Type::Opaque(ref n) => {
                return Err(DialcError::new(
                    format!(
                        "语义错误：当前版本不支持直接 print_raw 包句柄 '{}'（不透明指针）；\
                         请改用该包提供的取值函数",
                        n
                    ),
                    0,
                    0,
                ));
            }
            Type::Void => {
                return Err(DialcError::new(
                    "语义错误：print_raw 的参数不能是 void",
                    0, 0,
                ));
            }
        }
        Ok(())
    }
}

/// 判断函数是否返回 void。
fn f_ret_is_void(function: FunctionValue) -> bool {
    function.get_type().get_return_type().is_none()
}

/// 求 lvalue 表达式根部的变量名（用于数组索引赋值等）。
fn lvalue_root(e: &Expr) -> Option<&str> {
    match e {
        Expr::Var(n) => Some(n),
        Expr::Field(o, _) => lvalue_root(o),
        Expr::Index(o, _) => lvalue_root(o),
        _ => None,
    }
}
