//! LLVM IR 代码生成（基于 inkwell 0.11，目标 LLVM 18）。
//!
//! 架构核心：
//! - Dial 编译器**只翻译 AST → 对这些 C 函数的调用**；自身不做容器/字符串的底层实现。
//! - String / Array 在 IR 中一律是「不透明指针」i8*；用户永远无法直接访问底层内存布局。
//! - 引用计数 / 写时复制完全由 C Runtime（runtime/ + stdlib/）掌控。
//! - 数据流分析（Sema 阶段写入的 move_hint）在此决定：移动语义（跳过 retain）还是共享（retain）。
//! - `.o` 由 inkwell 在内存中构建 IR 后 `write_to_file` 直接 emit，不导出 .ll 文本。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::debug_info::{
    AsDIScope, DebugInfoBuilder, DICompileUnit, DIFlags, DIFlagsConstants, DIFile,
    DILocation, DIScope, DIType, DWARFEmissionKind, DWARFSourceLanguage,
};
use inkwell::module::{FlagBehavior, Linkage};
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetData,
};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType, StructType};
use inkwell::basic_block::BasicBlock;
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FloatValue, FunctionValue, IntValue,
    PointerValue,
};
use inkwell::AddressSpace;
use inkwell::FloatPredicate;
use inkwell::IntPredicate;
use inkwell::OptimizationLevel;

use crate::ast::*;
use crate::error::{DialcError, Result};
use crate::semantic::{analyze_gotos, type_of, FuncInfo, GotoAnalysis, StructTable};

type Local<'ctx> = (Type, (bool, PointerValue<'ctx>));
type CgScopes<'ctx> = Vec<HashMap<String, Local<'ctx>>>;

/// -g 模式下持有的 DWARF 生成器状态。None 表示普通构建，完全不碰调试 API。
struct DebugInfo<'ctx> {
    builder: DebugInfoBuilder<'ctx>,
    cu: DICompileUnit<'ctx>,
    file: DIFile<'ctx>,
}

/// gen_stmt 的「调试位置」RAII 守卫：进入语句时把 builder 的当前 DILocation
/// 切到该语句的 行:列，语句（含所有早退分支）返回时自动恢复外层位置。
/// 不用它的话，嵌套语句返回后外层后续指令会错误地沿用内层行号。
struct DebugLocGuard<'a, 'ctx> {
    builder: &'a Builder<'ctx>,
    prev: Option<DILocation<'ctx>>,
}

impl Drop for DebugLocGuard<'_, '_> {
    fn drop(&mut self) {
        match self.prev {
            Some(loc) => self.builder.set_current_debug_location(loc),
            None => self.builder.unset_current_debug_location(),
        }
    }
}

/// Dial 类型 → DWARF 里显示的类型名。复合类型在 IR 里本就是不透明 i8*，
/// 这里只负责给出一个可读名字，不假装知道 C Runtime 的内部布局。
fn dial_type_name(t: &Type) -> String {
    match t {
        Type::Void => "void".into(),
        Type::Int => "int".into(),
        Type::Float => "float".into(),
        Type::Bool => "bool".into(),
        Type::Char => "char".into(),
        Type::Byte => "byte".into(),
        Type::String => "string".into(),
        Type::Bytes => "bytes".into(),
        Type::Array(e) => format!("{}[]", dial_type_name(e)),
        Type::Map => "map".into(),
        Type::Struct(n) => n.clone(),
        Type::RawPtr(e) => format!("rawptr<{}>", dial_type_name(e)),
        Type::Channel(e) => format!("channel<{}>", dial_type_name(e)),
        Type::FnPtr(..) => "fnptr".into(),
        _ => "dial_object".into(),
    }
}

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
    /// 当前编译单元的源文件名（v1.04 工具链 / A7）：panic、断言失败、未捕获异常
    /// 的运行时诊断会把「文件:行:列」一起打出来，便于稍有规模的程序定位。
    /// 多文件 `import` 合并编译时取**主源文件**路径（各块的行列仍按各自文件准确）。
    pub src_file: String,
    /// -g：生成标准 DWARF 调试信息（CompileUnit / 函数 / 行号 / 局部变量名）。
    /// 关闭时完全不创建 DIBuilder，零额外开销。
    pub debug_info: bool,
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
    /// `defer` 延迟语句栈：每一层作用域（函数体 / 裸块 / 循环体 / if 分支 / try-catch）维护一个
    /// 待执行语句列表；离开该作用域时按 LIFO 逆序发射。`defer_stack` 与 `scopes` **严格同步进出**
    /// （进入作用域时同时 push、离开时同时 pop），因此其下标与 `scopes` 一致，break/continue 可
    /// 凭循环体所在下标精准排空「被跳出的若干层作用域」的延迟语句。
    defer_stack: RefCell<Vec<Vec<Stmt>>>,
    /// 循环嵌套栈：记录每条进入的循环**体帧在 `defer_stack` 中的下标**。break/continue 发生时，
    /// 从该下标到栈顶的所有（含循环体本身）延迟语句都要按 LIFO 发射并弹出——即离开该循环所跨越的
    /// 全部作用域（内层裸块等）的 defer 都一并执行。
    loop_stack: RefCell<Vec<usize>>,
    /// `try` 嵌套栈：记录每个 try **体帧在 `defer_stack` 中的下标**。`throw` 会 longjmp 离开
    /// try 体作用域，必须在跳转前按 LIFO 触发该帧及其内层的 defer，否则资源清理被跳过。
    try_frames: RefCell<Vec<usize>>,
    /// `goto` / `label`（v1.02）：标签名 → 预分配的目标基本块。
    ///
    /// 前向 goto 要求目标块在**生成跳转时**就已存在，故在函数开头一次性预分配；
    /// 生成到 `label` 语句时再把执行流接到该块上。
    label_blocks: RefCell<HashMap<String, BasicBlock<'ctx>>>,
    /// 标签名 → 该标签所在处的作用域深度（codegen 口径 = `scopes.len()`）。
    /// goto 跳出作用域时据此计算「要释放几层局部」。
    label_depth: RefCell<HashMap<String, usize>>,
    /// 当前函数的 goto/label 分析结果（标签位置、深度、路径），来自 `semantic::analyze_gotos`。
    goto_analysis: RefCell<GotoAnalysis>,
    /// Fail-Fast 安全等级：决定是否在 IR 里插入边界/除零检查与 trap。
    safety: SafetyMode,
    /// v1.04 FFI：`extern struct` 的名字集合。
    /// 用于区分「C ABI 布局结构体」（字段 bool/byte 为 1 字节）与
    /// 「Dial 值语义结构体」（字段按内部宽度），以便裸指针字段读写时做正确的宽度转换。
    cstruct_names: std::collections::HashSet<String>,
    /// A7：当前编译单元源文件名，随运行时诊断一起输出。
    src_file: String,
    /// A7：当前正在生成的语句位置（1 起行列）。由 `gen_stmt` 入口写入；
    /// `Expr` 不带位置，panic/assert/throw 这些「发生在某条语句里」的失败据此定位。
    cur_pos: std::cell::Cell<(usize, usize)>,
    /// -g：DWARF 生成器（None = 普通构建）。
    debug: Option<DebugInfo<'ctx>>,
    /// -g：当前函数的 DIScope，作为局部变量与语句行号的父作用域。
    cur_scope: Cell<Option<DIScope<'ctx>>>,
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

    // v1.04: 把宿主目标机的 DataLayout 写到模块上。否则 module 默认 DataLayout 会把
    // i64 的对齐写成 4（而非 x86-64 SysV 的 8），导致 sizeof/alignof/offsetof 以及
    // extern struct 的字段偏移与 C 编译器（zig cc 同 ABI）不一致，破坏 FFI 布局。
    {
        Target::initialize_all(&InitializationConfig::default());
        let triple = TargetMachine::get_default_triple();
        if let Some(tm) = Target::from_triple(&triple).ok().and_then(|t| {
            t.create_target_machine(
                &triple,
                "",
                "",
                OptimizationLevel::None,
                RelocMode::Default,
                CodeModel::Default,
            )
        }) {
            let dl = tm.get_target_data().get_data_layout();
            module.set_data_layout(&dl);
            module.set_triple(&triple);
        }
    }

    // -g：先建 CompileUnit。"Debug Info Version" 模块标志必须早于任何 debug metadata，
    // 否则 LLVM 会以「Debug info version mismatch」告警并可能丢弃调试信息。
    // DIFile 用绝对路径：GDB 才能按行表里的文件名直接打开源码，`list` / `break f:N` 才生效。
    let debug = if opts.debug_info {
        let ver = context.i32_type().const_int(3, false);
        module.add_basic_value_flag("Debug Info Version", FlagBehavior::Warning, ver);
        let abs = std::fs::canonicalize(&opts.src_file)
            .unwrap_or_else(|_| PathBuf::from(&opts.src_file));
        let filename = abs.to_string_lossy().into_owned();
        let directory = abs
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let producer = format!("dialc {}", env!("CARGO_PKG_VERSION"));
        let (builder, cu) = module.create_debug_info_builder(
            true,
            DWARFSourceLanguage::C,
            &filename,
            &directory,
            &producer,
            false,
            "",
            0,
            "",
            DWARFEmissionKind::Full,
            0,
            false,
            false,
            "",
            "",
        );
        Some(DebugInfo {
            builder,
            cu,
            file: cu.get_file(),
        })
    } else {
        None
    };

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
        defer_stack: RefCell::new(Vec::new()),
        loop_stack: RefCell::new(Vec::new()),
        try_frames: RefCell::new(Vec::new()),
        label_blocks: RefCell::new(HashMap::new()),
        label_depth: RefCell::new(HashMap::new()),
        goto_analysis: RefCell::new(GotoAnalysis::default()),
        safety: opts.safety,
        cstruct_names: program.cstructs.iter().map(|c| c.name.clone()).collect(),
        src_file: opts.src_file.clone(),
        cur_pos: std::cell::Cell::new((0, 0)),
        debug,
        cur_scope: Cell::new(None),
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

    // -g：所有 DISubprogram / DILocalVariable 已建完，收束 deferred metadata。
    // inkwell 要求 finalize 在 module.verify() 与任何 codegen 之前调用。
    if let Some(d) = &cg.debug {
        d.builder.finalize();
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
        // IR 验证失败意味着 codegen 产出了**无效**的 LLVM IR：继续 emit 要么让 LLVM
        // 拒绝（报一条更晦涩、更难定位的错），要么干脆 emit 出带未定义行为的机器码。
        // 与其把问题推到下游，这里直接以易读文字中止编译。
        return Err(DialcError::new(
            format!("[dialc] IR 验证失败: {}", e),
            0,
            0,
        ));
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
        // v1.04 FFI：extern struct 也注册进同一张表（先建 opaque 以支持互相嵌套）。
        for cs in &program.cstructs {
            self.struct_types
                .insert(cs.name.clone(), self.context.opaque_struct_type(&cs.name));
        }
        for sd in &program.structs {
            let fields: Vec<BasicTypeEnum> = sd
                .fields
                .iter()
                .map(|(_, t)| self.dialc_type_to_llvm(t))
                .collect();
            // v1.04 紧凑布局：`packed struct` 第二参传 true —— 不插对齐 padding，
            // 字段紧密排列，与 C 的 `__attribute__((packed))` / `#pragma pack` 逐字节一致
            // （网络协议头、硬件寄存器映射等「按偏移强约定」的场景刚需）。
            self.struct_types[&sd.name].set_body(&fields, sd.packed);
        }
        // v1.04 FFI：C ABI 结构体用 C 字段宽度；`packed extern struct` 同样不插 padding，
        // 与 C 的 `__attribute__((packed))` 一致——网络协议结构体（如 TCP 头）逐字节对齐。
        for cs in &program.cstructs {
            let fields: Vec<BasicTypeEnum> = cs
                .fields
                .iter()
                .map(|(_, t)| self.cstruct_field_llvm(t))
                .collect();
            self.struct_types[&cs.name].set_body(&fields, cs.packed);
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
        self.decl_func("dial_string_index_of", i64b, &[i8b.into(), i8b.into()]);
        self.decl_func(
            "dial_string_replace",
            i8b,
            &[i8b.into(), i8b.into(), i8b.into()],
        );
        self.decl_func("dial_string_trim", i8b, &[i8b.into()]);
        self.decl_func("dial_string_split", i8b, &[i8b.into(), i8b.into()]);
        self.decl_void("dial_string_retain", &[i8b.into()]);
        self.decl_void("dial_string_release", &[i8b.into()]);
        self.decl_void("dial_print_string", &[i8b.into()]);
        // Array（元素类型由 kind 决定：0=INT 1=RC 2=ATOMIC，见 dial_array.c）
        self.decl_func("dial_array_new", i8b, &[i64b.into()]);
        self.decl_func("dial_array_push", i8b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_array_get", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func(
            "dial_array_set",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        // 嵌套下标赋值专用：与 dial_array_set 同签名，但**不消耗** a 的引用
        // （a 是从外层数组元素槽借来的），改为交出「一份新拥有」的引用。
        self.decl_func(
            "dial_array_set_borrowed",
            i8b,
            &[i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_func("dial_array_length", i64b, &[i8b.into()]);
        self.decl_func("dial_array_slice", i8b, &[i8b.into(), i64b.into(), i64b.into()]);
        self.decl_func("dial_array_clone", i8b, &[i8b.into()]);
        self.decl_func("dial_array_remove", i8b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_array_pop", i8b, &[i8b.into()]);
        self.decl_func("dial_array_sort", i8b, &[i8b.into()]);
        // v1.03：高阶数组操作（map / filter / sort_by）统一入口。
        // thunk 是函数指针，在 LLVM 18 的不透明指针模式下同样是 ptr，故用 i8b 声明。
        self.decl_func(
            "dial_array_hof",
            i8b,
            &[
                i8b.into(),   // DialArray a
                i64b.into(),  // op: 0=map 1=filter 2=sort_by
                i64b.into(),  // out_kind: map 结果的元素类别码
                i8b.into(),   // fn_raw: Dial 函数指针
                i8b.into(),   // state: rawptr<void>
                i8b.into(),   // thunk
            ],
        );
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
        // v1.04 C6：get_or(m, k, default)，一次探测区分「没找到」与「真实值 == 默认值」。
        self.decl_func("dial_map_get_or", i64b, &[i8b.into(), i64b.into(), i64b.into()]);
        self.decl_func("dial_map_contains", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_remove", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_map_size", i64b, &[i8b.into()]);
        self.decl_func("dial_map_keys", i8b, &[i8b.into()]);
        // 泛型 map<K,V>（v1.00）：独立 C 运行时 stdlib/dial_kvmap.c。
        self.decl_func("dial_kvmap_new", i8b, &[i64b.into(), i64b.into()]);
        self.decl_func("dial_kvmap_put", i8b, &[i8b.into(), i64b.into(), i64b.into()]);
        self.decl_func("dial_kvmap_get", i64b, &[i8b.into(), i64b.into()]);
        // v1.04 C6：泛型 map 的 get_or（位模式默认值，仅标量 V 可达）。
        self.decl_func("dial_kvmap_get_or", i64b, &[i8b.into(), i64b.into(), i64b.into()]);
        self.decl_func("dial_kvmap_contains", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_kvmap_remove", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_kvmap_size", i64b, &[i8b.into()]);
        self.decl_func("dial_kvmap_keys", i8b, &[i8b.into()]);
        self.decl_func("dial_kvmap_values", i8b, &[i8b.into()]);
        self.decl_void("dial_kvmap_retain", &[i8b.into()]);
        self.decl_void("dial_kvmap_release", &[i8b.into()]);
        // v1.00：断言 / 退出 / 浮点数学内建
        self.decl_void("dial_assert_fail", &[i8b.into()]);
        // v1.04 工具链（A7）：带源码位置的失败诊断，签名统一 (msg, file, line, col)。
        self.decl_void(
            "dial_assert_fail_loc",
            &[i8b.into(), i8b.into(), i64b.into(), i64b.into()],
        );
        self.decl_void("dial_exit", &[i64b.into()]);
        self.decl_func("sqrt", f64b, &[f64b.into()]);
        self.decl_func("pow", f64b, &[f64b.into(), f64b.into()]);
        self.decl_func("floor", f64b, &[f64b.into()]);
        self.decl_func("ceil", f64b, &[f64b.into()]);
        self.decl_func("round", f64b, &[f64b.into()]);
        self.decl_func("fabs", f64b, &[f64b.into()]);
        // v1.01：环境 / 时间 / 随机内建
        self.decl_func("dial_args", i8b, &[]);
        self.decl_func("dial_now", f64b, &[]);
        self.decl_func("dial_random", i64b, &[]);
        self.decl_void("dial_sleep", &[i64b.into()]);
        self.decl_void("dial_env_init", &[i64b.into(), i8b.into()]);
        // v1.02：裸指针运行时（不受 ARC 管理，谁申请谁释放）
        self.decl_func("dial_raw_alloc", i8b, &[i64b.into()]);
        self.decl_void("dial_raw_free", &[i8b.into()]);
        // v1.04 FFI：string / bytes <-> C 缓冲区的数据总线（stdlib/dial_ffi.c）。
        // 前两个是借用指针（跳过 8 字节长度头），后两个是从 C 缓冲区复制出新对象。
        self.decl_func("dial_ffi_string_cstr", i8b, &[i8b.into()]);
        self.decl_func("dial_ffi_bytes_data", i8b, &[i8b.into()]);
        self.decl_func("dial_ffi_string_from_cstr", i8b, &[i8b.into()]);
        self.decl_func("dial_ffi_bytes_from_ptr", i8b, &[i8b.into(), i64b.into()]);
        // v1.00：字符串插值所需的标量 -> 字符串转换
        self.decl_func("dial_bool_to_string", i8b, &[i64b.into()]);
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
        // v1.04 C8：stderr 输出族，与上面 stdout 族逐一对应（仅换输出流）。
        self.decl_void("dial_eprint_int", &[i64b.into()]);
        self.decl_void("dial_eprint_float", &[f64b.into()]);
        self.decl_void("dial_eprint_bool", &[i64b.into()]);
        self.decl_void("dial_eprint_char", &[i64b.into()]);
        self.decl_void("dial_eprint_string", &[i8b.into()]);
        self.decl_void("dial_eprint_raw", &[i8b.into()]);
        self.decl_void("dial_eprint_write_char", &[i64b.into()]);
        self.decl_void("dial_eprint_raw_int", &[i64b.into()]);
        self.decl_void("dial_eprint_raw_float", &[f64b.into()]);
        self.decl_void("dial_eprint_raw_bool", &[i64b.into()]);
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
        self.decl_void(
            "dial_panic_loc",
            &[i8b.into(), i8b.into(), i64b.into(), i64b.into()],
        );

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
        // v1.04 B6：把值类型结构体装箱为引用计数对象（[int64 refcount][payload]），
        // 用于把结构体放进容器（数组 / map 值）。返回 payload 指针（同 Dial 堆对象布局）。
        self.decl_func("dial_struct_box", i8b, &[i8b.into(), i64b.into()]);
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

        // ---- v1.04 第 2 批：C 组标准库 API（C1–C5 / C7 / C9 / C10）----
        // 所有符号由 stdlib/*.c 导出；不透明句柄一律 i8*，标量按 C ABI 宽度。
        // 详见 README §37 的第 2 批小节与 stdlib/stdlib.diali 自描述。
        // C1 字符串查询 / 规范化
        self.decl_func("dial_string_to_upper", i8b, &[i8b.into()]);
        self.decl_func("dial_string_to_lower", i8b, &[i8b.into()]);
        self.decl_func("dial_string_contains", i64b, &[i8b.into(), i8b.into()]);
        self.decl_func("dial_string_starts_with", i64b, &[i8b.into(), i8b.into()]);
        self.decl_func("dial_string_ends_with", i64b, &[i8b.into(), i8b.into()]);
        // C4 UTF-8 码点视图（length/get/substring 字节语义一字未动）
        self.decl_func("dial_string_char_count", i64b, &[i8b.into()]);
        self.decl_func("dial_string_chars", i8b, &[i8b.into()]);
        self.decl_func("dial_string_char_at", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_string_char_slice", i8b, &[i8b.into(), i64b.into(), i64b.into()]);
        // C2 严格数值解析 / C3 定点格式化（拒绝 printf 式变参格式串）
        self.decl_func("dial_parse_int", i64b, &[i8b.into()]);
        self.decl_func("dial_parse_float", f64b, &[i8b.into()]);
        self.decl_func("dial_is_int", i64b, &[i8b.into()]);
        self.decl_func("dial_is_float", i64b, &[i8b.into()]);
        self.decl_func("dial_format_float", i8b, &[f64b.into(), i64b.into()]);
        // C5 数组 API 扩充（arr 一律 i8*；返回新数组/标量位模式）
        self.decl_func("dial_array_insert", i8b, &[i8b.into(), i64b.into(), i64b.into()]);
        self.decl_func("dial_array_reverse", i8b, &[i8b.into()]);
        self.decl_func("dial_array_clear", i8b, &[i8b.into()]);
        self.decl_func("dial_array_index_of", i64b, &[i8b.into(), i64b.into()]);
        self.decl_func("dial_array_join", i8b, &[i8b.into(), i8b.into()]);
        self.decl_func("dial_array_sort_string", i8b, &[i8b.into()]);
        // reduce：fn_raw / state / thunk 三个指针 + init 位模式（复用 v1.03 thunk 机制）
        self.decl_func(
            "dial_array_reduce",
            i64b,
            &[i8b.into(), i64b.into(), i8b.into(), i8b.into(), i8b.into()],
        );
        // C7 文件 API（命名刻意避开与零参 read_line() 撞名 = 重载否决）
        self.decl_func("dial_file_eof", i64b, &[i8b.into()]);
        self.decl_func("dial_file_read_line", i8b, &[i8b.into()]);
        self.decl_func("dial_file_read_all", i8b, &[i8b.into()]);
        self.decl_func("dial_file_exists", i64b, &[i8b.into()]);
        self.decl_func("dial_file_size", i64b, &[i8b.into()]);
        self.decl_func("dial_file_remove", i64b, &[i8b.into()]);
        // C9 环境只读查询（无 setenv：绝不引入可变全局状态）
        self.decl_func("dial_getenv", i8b, &[i8b.into()]);
        self.decl_func("dial_has_env", i64b, &[i8b.into()]);
        self.decl_func("dial_program_name", i8b, &[]);
        self.decl_func("dial_cwd", i8b, &[]);
        // C10 本地时间 / 日期 / 单调计时
        self.decl_func("dial_local_parts", i8b, &[]);
        self.decl_func("dial_date_string", i8b, &[]);
        self.decl_func("dial_monotonic_ms", i64b, &[]);
    }

    /// 声明一个由 `import` 引入的包外部函数（只有签名，无函数体）。
    ///
    /// 与 C 侧 FFI 函数（declare_stdlib）用的是同一套机制：LLVM 的
    /// **外部链接声明**——模块里只出现 `declare`，定义由链接器从 `.a` 中解析。
    fn declare_extern_function(&self, name: &str, info: &FuncInfo) -> FunctionValue<'ctx> {
        // v1.04：FFI 边界一律用 C ABI 宽度（bool→i32 / byte→i8）。
        let param_tys: Vec<BasicMetadataTypeEnum> = info
            .params
            .iter()
            .map(|t| self.ffi_type_to_llvm(t).into())
            .collect();
        // C 风格异构变参（v1.03）：`.diali` 的 `extern fn f(a: T, ...): R` 声明为
        // LLVM varargs 函数；固定参数照常检查，尾部参数按 C ABI 直传。
        let ft = if info.ret == Type::Void {
            self.context.void_type().fn_type(&param_tys, info.variadic)
        } else {
            self.ffi_type_to_llvm(&info.ret).fn_type(&param_tys, info.variadic)
        };
        let fv = self.module.add_function(name, ft, None);
        // 显式固定 C calling convention（LLVM 编号 0）：由 target triple 决定
        // 具体的 SysV AMD64 / Windows x64 / AArch64 AAPCS 参数传递细节。
        fv.set_call_conventions(0);
        fv
    }

    /// 从 Dial 签名构造**精确**的 LLVM 函数类型。
    ///
    /// 函数指针的间接调用必须给出精确签名（LLVM 18 的 `build_indirect_call` 要求），
    /// 绝不能像 C 侧那样猜一个通用 `void* (*)(void*)`——那正是 UB 的温床。
    fn llvm_fn_type(&self, params: &[Type], ret: &Type) -> FunctionType<'ctx> {
        // v1.04：函数指针签名也统一走 C ABI 宽度，确保 C 侧能安全地调用 Dial 函数。
        let param_tys: Vec<BasicMetadataTypeEnum> = params
            .iter()
            .map(|t| self.ffi_type_to_llvm(t).into())
            .collect();
        if *ret == Type::Void {
            self.context.void_type().fn_type(&param_tys, false)
        } else {
            self.ffi_type_to_llvm(ret).fn_type(&param_tys, false)
        }
    }

    /// 为高阶数组操作生成一个**精确回调签名**的适配 thunk（v1.03）。
    ///
    /// C 侧只认统一签名 `i64 (i64 a_bits, i64 b_bits, ptr fn_raw, ptr state)`；
    /// thunk 把位模式还原成 Dial 的 T、把 `fn_raw` 当作精确函数指针间接调用，
    /// 再把返回值转回位模式。`fn_raw` / `state` 由 C 透传，**thunk 不捕获任何东西**，
    /// 因此同一个 (元素类型, 返回类型, 参数个数) 组合只生成一次、可跨调用点复用。
    ///
    /// 这是「无闭包」设计的直接结果：状态不藏在函数值里，而是显式穿过 C 再交回回调。
    fn gen_hof_thunk(
        &self,
        elem: &Type,
        ret: &Type,
        arity: usize,
    ) -> Result<FunctionValue<'ctx>> {
        let code = |t: &Type| match t {
            Type::Int => "i",
            Type::Float => "f",
            Type::Bool => "b",
            Type::Byte => "y",
            Type::Char => "c",
            _ => "x",
        };
        let name = format!("__dial_hof_{}_{}_{}", arity, code(elem), code(ret));
        if let Some(existing) = self.module.get_function(&name) {
            return Ok(existing);
        }
        // 生成 thunk 会移动 builder 的插入点，先存下来再还原（与 spawn thunk 同款）。
        let saved = self.builder.get_insert_block();
        let i64t = self.context.i64_type();
        let ft = i64t.fn_type(
            &[i64t.into(), i64t.into(), self.i8ptr().into(), self.i8ptr().into()],
            false,
        );
        let thunk = self.module.add_function(&name, ft, None);
        let entry = self.context.append_basic_block(thunk, "entry");
        self.builder.position_at_end(entry);

        let a_bits = thunk.get_nth_param(0).unwrap().into_int_value();
        let b_bits = thunk.get_nth_param(1).unwrap().into_int_value();
        let fn_raw = thunk.get_nth_param(2).unwrap().into_pointer_value();
        let state = thunk.get_nth_param(3).unwrap();

        // Dial 回调签名：fn(T, rawptr<void>): R 或 fn(T, T, rawptr<void>): R。
        let mut params: Vec<Type> = vec![elem.clone()];
        if arity == 2 {
            params.push(elem.clone());
        }
        params.push(Type::RawPtr(Box::new(Type::Void)));
        let call_ft = self.llvm_fn_type(&params, ret);

        // v1.04 FFI：回调签名已统一为 C ABI 宽度。从 C 侧 i64 槽位还原出的
        // 只是 Dial 内部宽度（bool=i1 / byte=i64），传给回调前必须再做一次 abi_adapt。
        let mut call_args: Vec<BasicMetadataValueEnum> = Vec::new();
        let a = self.from_elem_bits(elem, a_bits);
        call_args.push(
            self.abi_adapt(a, a.get_type(), self.ffi_type_to_llvm(elem))
                .into(),
        );
        if arity == 2 {
            let b = self.from_elem_bits(elem, b_bits);
            call_args.push(
                self.abi_adapt(b, b.get_type(), self.ffi_type_to_llvm(elem))
                    .into(),
            );
        }
        call_args.push(state.into());
        let call = self
            .builder
            .build_indirect_call(call_ft, fn_raw, &call_args, "hof.call")
            .unwrap();
        let rv = call
            .try_as_basic_value()
            .basic()
            .expect("高阶回调返回值不应为 void（语义层已拦截）");
        // 反向：C ABI 返回宽度 → Dial 内部宽度，再压回 C 运行时的 i64 槽位。
        let rv_internal = self.abi_adapt(rv, rv.get_type(), self.dialc_type_to_llvm(ret));
        let bits = self.to_elem_bits(thunk, rv_internal, ret);
        self.builder.build_return(Some(&bits)).unwrap();

        if let Some(bb) = saved {
            self.builder.position_at_end(bb);
        }
        Ok(thunk)
    }

    fn declare_dial_function(
        &self,
        f: &Func,
        functions: &HashMap<String, FuncInfo>,
    ) -> FunctionValue<'ctx> {
        let info = &functions[&f.name];
        // v1.04：**所有 Dial 函数**都用 C ABI 签名——这是「C 能安全调用 Dial 函数」
        // 的前提。函数体内仍是内部宽度（bool=i1 / byte=i64），边界由 abi_adapt 对称转换。
        let param_tys: Vec<BasicMetadataTypeEnum> = f
            .params
            .iter()
            .map(|(_, t)| self.ffi_type_to_llvm(t).into())
            .collect();
        let ft = if f.ret == Type::Void {
            self.context.void_type().fn_type(&param_tys, false)
        } else if f.name == "main" && f.ret == Type::Int {
            // main 走 C ABI：返回 int (i32)，且额外接收 argc/argv 供 args() 使用。
            // argc 用 i64（与 dial_env_init 的 int64_t 形参一致），argv 为 i8*。
            self.context
                .i32_type()
                .fn_type(&[self.context.i64_type().into(), self.i8ptr().into()], false)
        } else {
            self.ffi_type_to_llvm(&f.ret).fn_type(&param_tys, false)
        };
        let _ = info;
        let fv = self.module.add_function(&f.name, ft, None);
        // v1.04 FFI 导出：`@export fn` 与 `main` 对外暴露为 C 链接符号（无 mangling、
        // C calling convention），可被 C 侧 dlsym / 直接链接调用；其余内部函数收为
        // Private linkage——模块内仍可递归/取函数指针，但链接器不会导出、也不会与
        // 其它目标文件里的同名符号冲突，且未被引用的会被死代码消除掉。
        if f.exported || f.name == "main" {
            fv.set_linkage(Linkage::External);
        } else {
            fv.set_linkage(Linkage::Private);
        }
        // 显式固定 C calling convention（LLVM 0）→ 平台标准的 SysV / Win64 / AAPCS。
        fv.set_call_conventions(0);
        fv
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
            | Type::Map | Type::Set(_) | Type::KvMap(..)
            | Type::File
            | Type::Error
            | Type::Thread
            | Type::Channel(_)
            | Type::Opaque(_)
            // 裸指针（v1.02）：不透明指针（元素类型由调用处的 gep 决定）
            | Type::RawPtr(_) | Type::VolatilePtr(_)
            // 函数指针（v1.03）：纯代码地址，IR 层同样是不透明 ptr。
            | Type::FnPtr(..) => self.i8ptr().into(),
            Type::Struct(ref n) => (*self
                .struct_types
                .get(n)
                .expect("结构体类型应在 build_struct_types 中已注册"))
            .into(),
        }
    }

    /// v1.04 FFI：Dial 类型 → **C ABI 函数签名**宽度。
    ///
    /// 差异只有两个：
    ///   - `bool`：内部 i1 → C 的 `int`(i32)（C 默认整型提升，绝不把 i1 直接丢给 C）；
    ///   - `byte`：内部 i64 → C 的 `uint8_t`(i8)（绝不是 i64）。
    /// 其余类型（int=i64 / float=f64 / 各类句柄与裸指针 / 函数指针 = ptr）本就与 C ABI 一致。
    /// **结构体字段**另用 `cstruct_field_llvm`——那里 bool 是 1 字节 `_Bool`，不做整型提升。
    fn ffi_type_to_llvm(&self, t: &Type) -> BasicTypeEnum<'ctx> {
        match t {
            Type::Bool => self.context.i32_type().into(),
            Type::Byte => self.context.i8_type().into(),
            other => self.dialc_type_to_llvm(other),
        }
    }

    /// v1.04 FFI：`extern struct` 字段的 LLVM 类型（C 内存布局宽度）。
    /// LLVM 的**非 packed** struct 会按 target DataLayout 自动插入 padding，
    /// 与 C 编译器（SysV / Windows x64 ABI）的字段顺序、对齐、sizeof 完全一致。
    fn cstruct_field_llvm(&self, t: &Type) -> BasicTypeEnum<'ctx> {
        match t {
            Type::Bool => self.context.i8_type().into(), // C _Bool（1 字节）
            Type::Byte => self.context.i8_type().into(), // C uint8_t
            other => self.dialc_type_to_llvm(other),
        }
    }

    /// v1.04 FFI：某个 LLVM 类型的**位宽**（bitcast 要求源与目标同位宽）。
    ///
    /// 用 target DataLayout 而不是硬编码 64：32 位平台上指针是 32 位，
    /// 那时 `bitcast<int>(ptr)` 就是错的（位宽不等）。
    /// 聚合类型（结构体 / 数组）用 ABI 字节数 × 8 近似——它的真实位宽含 padding，
    /// 精确值交给 LLVM 校验器兜底。
    fn llvm_bit_width(&self, t: &BasicTypeEnum) -> u64 {
        match t {
            BasicTypeEnum::IntType(i) => i.get_bit_width() as u64,
            BasicTypeEnum::FloatType(f) => f.get_bit_width() as u64,
            BasicTypeEnum::PointerType(_) => {
                let dl = self.module.get_data_layout();
                let td = TargetData::create(dl.as_str().to_str().unwrap_or(""));
                td.get_pointer_byte_size(None) as u64 * 8
            }
            other => {
                let dl = self.module.get_data_layout();
                let td = TargetData::create(dl.as_str().to_str().unwrap_or(""));
                td.get_abi_size(other) * 8
            }
        }
    }

    /// FFI ↔ Dial 边界的整数宽度适配（zext / trunc），其余类型原样返回。
    ///
    /// 按**实际 IR 宽度**判断，不依赖调用方传 Dial 类型——函数入口、return、
    /// 调用实参、调用返回值、thunk 转发都复用这一份逻辑，不会漏掉某条路径。
    fn abi_adapt(
        &self,
        v: BasicValueEnum<'ctx>,
        from: BasicTypeEnum<'ctx>,
        to: BasicTypeEnum<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        if let (BasicTypeEnum::IntType(ft), BasicTypeEnum::IntType(tt)) = (from, to) {
            let fw = ft.get_bit_width();
            let tw = tt.get_bit_width();
            if fw < tw {
                return self
                    .builder
                    .build_int_z_extend(v.into_int_value(), tt, "abi.zext")
                    .unwrap()
                    .into();
            } else if fw > tw {
                return self
                    .builder
                    .build_int_truncate(v.into_int_value(), tt, "abi.trunc")
                    .unwrap()
                    .into();
            }
        }
        v
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
        } else if matches!(
            *t,
            Type::String
                | Type::Bytes
                | Type::Array(_)
                | Type::Map | Type::Set(_) | Type::KvMap(..)
                | Type::Error
                | Type::Channel(_)
                | Type::Opaque(_)
        ) {
            // 句柄/指针元素（string / int[] / map / error / channel / 不透明指针）按位转成 i64 存入数组槽
            self.builder
                .build_ptr_to_int(v.into_pointer_value(), self.context.i64_type(), "p2i")
                .unwrap()
        } else {
            v.into_int_value()
        }
    }

    /// 数组元素类别 → dial_array_new 的 kind 参数（0=INT 1=RC 2=ATOMIC）。
    /// 与 runtime/dial_array.c 的 DIAL_ELEM_* 宏保持一致。
    /// 数组元素类别 → dial_array_new 的 kind 参数。
    /// 与 runtime/dial_array.c 的 DIAL_ELEM_* 宏保持一致：
    /// 0=INT 1=RC 2=ATOMIC 3=FLOAT 4=BOOL 5=BYTE 6=CHAR。
    /// 3~6 都是无所有权的标量：C 侧 elem_retain/elem_release 对它们本就是 no-op，
    /// 故无需改 C 逻辑，只要编译器侧 kind 数字一致即可。
    fn array_elem_kind(&self, t: &Type) -> u64 {
        match t {
            Type::Int => 0,
            Type::Float => 3,
            Type::Bool => 4,
            Type::Byte => 5,
            Type::Char => 6,
            Type::Channel(_) => 2,
            Type::String | Type::Bytes | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error | Type::Opaque(_) => {
                1
            }
            // 结构体装箱为 RC 句柄（指针），与 string/array 等同走 kind=1 的 retain/release。
            Type::Struct(_) => 1,
            _ => 0, // 理论上不可达：check_type_wellformed 已拒绝其它元素类型
        }
    }

    /// 把 Dial 值按元素类型转成 C 侧数组槽统一的 i64 位模式（与 kv_arg 同思路）：
    /// float 走 bitcast（保留 IEEE754 位模式，绝不 fptosi 丢小数）、bool 走 zext、
    /// 句柄走 ptr_to_int、int/byte/char 本就是 i64。
    /// 结构体（v1.04 B6）：值类型不能直接塞进 8 字节槽，先装箱为 RC 对象
    /// （[int64 refcount][payload]），槽内存其 payload 指针的位模式；调用方在容器
    /// retain 之后须用 `int_to_ptr(bits)` 取回指针并 `dial_rc_release` 归还本地那份。
    fn to_elem_bits(
        &self,
        function: FunctionValue<'ctx>,
        v: BasicValueEnum<'ctx>,
        t: &Type,
    ) -> IntValue<'ctx> {
        if *t == Type::Float {
            self.builder
                .build_bit_cast(v, self.context.i64_type(), "f2bits")
                .unwrap()
                .into_int_value()
        } else if let Type::Struct(name) = t {
            // 装箱：把结构体值拷进堆上 RC 对象，返回 payload 指针（i8*）。
            let sty = self.struct_types[name];
            let size = sty.size_of().expect("结构体应有确定尺寸");
            let tmp = self.alloca_in_entry(function, sty.into(), "sbox_src");
            self.builder.build_store(tmp, v).unwrap();
            let boxp = self
                .call_c("dial_struct_box", &[tmp.into(), size.into()])
                .into_pointer_value();
            self.builder
                .build_ptr_to_int(boxp, self.context.i64_type(), "box2bits")
                .unwrap()
        } else {
            self.to_i64(v, t)
        }
    }

    /// 把 C 侧返回的 i64 槽位模式还原成 Dial 的 LLVM 值（与 kv_from_bits 同思路）。
    fn from_elem_bits(&self, t: &Type, v: IntValue<'ctx>) -> BasicValueEnum<'ctx> {
        match t {
            Type::Float => self.builder
                .build_bit_cast(v, self.context.f64_type(), "bits2f")
                .unwrap().into(),
            Type::Bool => self.builder
                .build_int_compare(
                    IntPredicate::NE, v,
                    self.context.i64_type().const_int(0, false), "bits2b",
                )
                .unwrap().into(),
            Type::String | Type::Bytes | Type::Array(_) | Type::Map | Type::Set(_)
                | Type::KvMap(..) | Type::Error | Type::Channel(_) | Type::Opaque(_) => {
                self.builder
                    .build_int_to_ptr(v, self.i8ptr(), "bits2p")
                    .unwrap().into()
            }
            Type::Struct(name) => {
                // 解箱：槽位存的是装箱 payload 指针，按结构体布局 load 出**值**（拷贝），
                // 容器仍独占那一份装箱对象；读出的副本随作用域结束被 release（标量字段无副作用）。
                let boxp = self.builder.build_int_to_ptr(v, self.i8ptr(), "bits2box").unwrap();
                let sty = self.struct_types[name];
                let sp = self
                    .builder
                    .build_pointer_cast(boxp, self.context.ptr_type(AddressSpace::default()), "box2s")
                    .unwrap();
                self.builder.build_load(sty, sp, "unbox").unwrap().into()
            }
            _ => v.into(), // Int / Byte / Char 都是 i64
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
            | Type::Map | Type::Set(_) | Type::KvMap(..)
            | Type::File
            | Type::Error
            | Type::Thread
            | Type::Channel(_)
            | Type::Opaque(_)
            | Type::RawPtr(_) | Type::VolatilePtr(_)
            | Type::FnPtr(..) => self.i8ptr().const_null().into(),
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

    // ---- `defer` 延迟语句机制（v1.00）-----------------------------------------
    //
    // `defer_stack` 与 `scopes` 严格同步进出：每进入一层作用域（函数体 / 裸块 / 循环体 /
    // if 分支 / try-catch）时同时 push，离开时同时 pop。因此两者下标一一对应，
    // 可用「循环体帧下标」精准排空被跳出作用域的延迟语句。

    /// 进入一层作用域：压入空的延迟语句列表（与 `scopes.push` 配对）。
    fn push_defer_frame(&self) {
        self.defer_stack.borrow_mut().push(Vec::new());
    }

    /// 登记一条 `defer` 语句到当前最内层作用域。
    fn add_defer(&self, s: Stmt) {
        self.defer_stack.borrow_mut().last_mut().unwrap().push(s);
    }

    /// 正常落穿离开当前作用域时调用：按 LIFO 逆序发射本层延迟语句，并弹出本层帧。
    /// 必须在 `release_scope_locals` + `scopes.pop` **之前**调用（延迟语句可能引用本层局部）。
    fn pop_defer_frame_emit(
        &self,
        function: FunctionValue<'ctx>,
        scopes: &mut CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        let frame = self.defer_stack.borrow_mut().pop().unwrap();
        // 当前块已终止（前一条语句是 return / throw 等）：正常落穿路径不可达，该出口的
        // defer 已由 drain_defers_from 在跳转前发射过，此处只需丢弃帧、保持栈同步。
        // 切勿再往已终止的块追加指令——IRBuilder 会把它们插到 terminator 之后，导致
        // "Terminator found in the middle of a basic block" 的非法 IR。
        if self.is_terminated() {
            return Ok(());
        }
        for s in frame.into_iter().rev() {
            self.gen_stmt(&s, function, scopes, func_values, functions, structs)?;
        }
        Ok(())
    }

    /// 从 `from` 下标到栈顶：按 LIFO（内层先、同层逆序）**额外发射**一份延迟语句。
    /// 用于 `break` / `continue`（离开循环跨越多层作用域）与 `return`（离开整个函数）。
    ///
    /// **绝不清空 `defer_stack` 的帧**：它是编译期静态结构，正常落穿路径的代码随后才由
    /// `pop_defer_frame_emit` 生成，若此时清帧，那条路径会拿到空帧而漏发 defer。
    /// 运行期「提前出口」与「正常落穿」两条分支互斥，各自执行自己那一份，合计恰好一次。
    ///
    /// 注意：LLVM IRBuilder 会把新指令插到当前块 terminator **之前**，因此即使当前块已因
    /// return/break 终止，被发射的延迟语句仍排在终止指令之前、即「出口处按序执行」，语义正确。
    fn drain_defers_from(
        &self,
        from: usize,
        function: FunctionValue<'ctx>,
        scopes: &mut CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        // 先收集（释放 borrow，避免 gen_stmt 时再借冲突），再原地发射；帧内容保持不变。
        let to_run: Vec<Stmt> = {
            let ds = self.defer_stack.borrow();
            let mut v = Vec::new();
            for i in (from..ds.len()).rev() {
                for s in ds[i].iter().rev() {
                    v.push(s.clone());
                }
            }
            v
        };
        for s in to_run {
            self.gen_stmt(&s, function, scopes, func_values, functions, structs)?;
        }
        Ok(())
    }

    /// `return` / 函数末尾：发射全部延迟语句（帧内容保持不变，见 `drain_defers_from`）。
    fn drain_all_defers(
        &self,
        function: FunctionValue<'ctx>,
        scopes: &mut CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        self.drain_defers_from(0, function, scopes, func_values, functions, structs)
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

    /// A7：发射一条**带源码位置**的失败诊断。
    ///
    /// `dial_panic_loc` / `dial_assert_fail_loc` 的 C ABI 签名统一为
    /// `(const char *msg, const char *file, int64_t line, int64_t col)`。
    /// 位置取 `gen_stmt` 入口记下的当前语句行列——`Expr` 本身不带位置，
    /// 但「panic 发生在哪一条语句」这个信息在语句层是齐全的。
    /// 文件名是编译期常量串，行列是编译期常量整数，运行期零额外开销。
    fn emit_fail_loc(&self, callee: &str, msg: BasicMetadataValueEnum<'ctx>) {
        let (line, col) = self.cur_pos.get();
        let file = self.global_string(
            &format!("loc.file.{}", self.next_str_id()),
            &self.src_file,
        );
        let i64t = self.context.i64_type();
        self.call_c_void(
            callee,
            &[
                msg,
                file.into(),
                i64t.const_int(line as u64, false).into(),
                i64t.const_int(col as u64, false).into(),
            ],
        );
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

    // ------------------------------------------------------------------
    //  -g：标准 DWARF 调试信息（CompileUnit / 函数 / 行号 / 变量名）
    // ------------------------------------------------------------------

    /// Dial 类型 → DIType。标量给出精确宽度与编码，因此 GDB 里 `print n` 显示 42
    /// 而不是十六进制地址；string / array / map / struct / 句柄在 IR 里本就是
    /// 不透明 i8*，如实呈现为指向 `dial_object` 的指针（能按名字 print，不伪装布局）。
    fn di_type(&self, t: &Type) -> DIType<'ctx> {
        let d = self.debug.as_ref().expect("di_type 仅在 -g 下调用");
        let (name, bits, enc): (&str, u64, u32) = match t {
            Type::Int => ("int", 64, 0x05),     // DW_ATE_signed
            Type::Float => ("float", 64, 0x04), // DW_ATE_float
            Type::Bool => ("bool", 1, 0x02),    // DW_ATE_boolean（IR 为 i1）
            Type::Char => ("char", 64, 0x05),   // Unicode 码点，IR 为 i64
            Type::Byte => ("byte", 64, 0x07),   // DW_ATE_unsigned（IR 为 i64）
            Type::Void => ("void", 64, 0x05),
            _ => {
                let opaque = d
                    .builder
                    .create_basic_type("dial_object", 8, 0x07, DIFlags::PUBLIC)
                    .unwrap()
                    .as_type();
                return d
                    .builder
                    .create_pointer_type(
                        &dial_type_name(t),
                        opaque,
                        64,
                        64,
                        AddressSpace::default(),
                    )
                    .as_type();
            }
        };
        d.builder
            .create_basic_type(name, bits, enc, DIFlags::PUBLIC)
            .unwrap()
            .as_type()
    }

    /// 进入函数：建 DISubprogram 并挂到 LLVM 函数上，作为本函数行号与局部变量的父作用域。
    fn begin_debug_function(&self, f: &Func, function: FunctionValue<'ctx>) {
        let d = match self.debug.as_ref() {
            Some(d) => d,
            None => return,
        };
        let params: Vec<DIType<'ctx>> = f.params.iter().map(|(_, t)| self.di_type(t)).collect();
        let ret = self.di_type(&f.ret);
        let sub_ty = d
            .builder
            .create_subroutine_type(d.file, Some(ret), &params, DIFlags::PUBLIC);
        let sub = d.builder.create_function(
            d.cu.as_debug_info_scope(),
            &f.name,
            None,
            d.file,
            f.line as u32,
            sub_ty,
            false,
            true,
            f.line as u32,
            DIFlags::PUBLIC,
            false,
        );
        function.set_subprogram(sub);
        let scope = sub.as_debug_info_scope();
        self.cur_scope.set(Some(scope));
        let loc = d.builder.create_debug_location(
            self.context,
            f.line as u32,
            f.col as u32,
            scope,
            None,
        );
        self.builder.set_current_debug_location(loc);
    }

    /// 把一条语句的 行:列 设为当前 IR 插入位置，返回退出时自动恢复的守卫。
    fn stmt_debug_loc(&self, stmt: &Stmt) -> Option<DebugLocGuard<'_, 'ctx>> {
        let d = self.debug.as_ref()?;
        let scope = self.cur_scope.get()?;
        let (line, col) = crate::semantic::stmt_pos(stmt);
        let prev = self.builder.get_current_debug_location();
        let loc = d.builder.create_debug_location(
            self.context,
            line as u32,
            col as u32,
            scope,
            None,
        );
        self.builder.set_current_debug_location(loc);
        Some(DebugLocGuard {
            builder: &self.builder,
            prev,
        })
    }

    /// 为一个栈槽发射 `llvm.dbg.declare`，把 Dial 变量名写进 DWARF。
    /// `arg_no = Some(n)` 表示形参（DWARF 参数序号从 1 起），None 表示局部变量。
    fn debug_declare(
        &self,
        name: &str,
        ty: &Type,
        line: usize,
        col: usize,
        storage: PointerValue<'ctx>,
        arg_no: Option<u32>,
    ) {
        let d = match self.debug.as_ref() {
            Some(d) => d,
            None => return,
        };
        let scope = match self.cur_scope.get() {
            Some(s) => s,
            None => return,
        };
        let di_ty = self.di_type(ty);
        let var = match arg_no {
            Some(n) => d.builder.create_parameter_variable(
                scope, name, n, d.file, line as u32, di_ty, true, DIFlags::PUBLIC,
            ),
            None => d.builder.create_auto_variable(
                scope, name, d.file, line as u32, di_ty, true, DIFlags::PUBLIC, 64,
            ),
        };
        let loc = d.builder.create_debug_location(
            self.context,
            line as u32,
            col as u32,
            scope,
            None,
        );
        let bb = match self.builder.get_insert_block() {
            Some(b) => b,
            None => return,
        };
        d.builder
            .insert_declare_at_end(storage, Some(var), None, loc, bb);
    }

    fn emit_object(&self, out: &Path, opts: &CgOpts) -> Result<()> {
        Target::initialize_all(&InitializationConfig::default());
        let triple = TargetMachine::get_default_triple();
        let target = Target::from_triple(&triple)
            .map_err(|e| DialcError::new(format!("LLVM 目标平台错误: {:?}", e), 0, 0))?;
        // --fast 模式：拉满 LLVM 优化。Aggressive 即 O3，会打开循环向量化器与循环展开器；
        // 再叠加宿主 SIMD 特征（x86 的 AVX2 / ARM64 的 NEON），让纯内存计算（如数组求和）
        // 一次直接处理 4~8 个整数。其余模式保持 Default（O2），优先编译速度与调试体验。
        // -g 隐含 O0：默认 O2 会把栈槽变量优化进寄存器甚至整个消掉，GDB 里只剩
        // `<optimized out>`，那就失去了「按变量名 print」的意义。--fast 是用户
        // 明确的性能契约，仍保留 O3，但文档里说明变量可能被优化掉。
        let opt_level = if opts.aggressive {
            OptimizationLevel::Aggressive
        } else if opts.debug_info {
            OptimizationLevel::None
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
            Type::Map | Type::Set(_) => "dial_map_retain",
            // 泛型 map 有独立 C 运行时与自定义析构（遍历条目释放 string 键）。
            Type::KvMap(..) => "dial_kvmap_retain",
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
            Type::Map | Type::Set(_) => "dial_map_release",
            Type::KvMap(..) => "dial_kvmap_release",
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

    /// 取（必要时创建）某个标签对应的目标基本块（v1.02）。
    ///
    /// 前向 goto 要求目标块在**生成跳转时**就已存在，故第一次取用时就创建，
    /// 之后取用返回同一个块。生成到 `label` 语句时再把执行流接到该块上。
    fn label_block(&self, name: &str, function: FunctionValue<'ctx>) -> BasicBlock<'ctx> {
        if let Some(bb) = self.label_blocks.borrow().get(name) {
            return *bb;
        }
        let bb = self.context.append_basic_block(function, &format!("label.{}", name));
        self.label_blocks.borrow_mut().insert(name.to_string(), bb);
        bb
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
            Type::String | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error | Type::Channel(_)
        )
    }

    /// 裸指针元素的**内存表示类型**（v1.02）：决定 load / store 的宽度与 offset 的步长。
    ///
    /// - `byte` / `bool`：各占 1 字节（`rawptr<byte>` 指向原始字节缓冲时的自然宽度）；
    /// - `char`：码点占 4 字节（UTF-8 码点的上限）；
    /// - `int` / `float` / 各类句柄：各占 8 字节；
    /// - `struct`：按其 LLVM 实际大小。
    fn raw_elem_llvm(&self, t: &Type) -> BasicTypeEnum<'ctx> {
        match t {
            Type::Byte | Type::Bool => self.context.i8_type().into(),
            Type::Char => self.context.i32_type().into(),
            Type::Int => self.context.i64_type().into(),
            Type::Float => self.context.f64_type().into(),
            Type::Struct(_) => self.dialc_type_to_llvm(t),
            _ => self.i8ptr().into(),
        }
    }

    /// 把「从内存读出来的值」还原成 Dial 的值表示（v1.02）。
    fn raw_from_mem(&self, t: &Type, v: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        match t {
            // byte：1 字节 → 零扩展成 i64（Dial 的 byte 值是 i64，取值 0~255）
            Type::Byte => self
                .builder
                .build_int_z_extend(v.into_int_value(), self.context.i64_type(), "mem2byte")
                .unwrap()
                .into(),
            // bool：内存里 1 字节 0/1 → i1
            Type::Bool => self
                .builder
                .build_int_compare(
                    IntPredicate::NE,
                    v.into_int_value(),
                    self.context.i8_type().const_int(0, false),
                    "mem2bool",
                )
                .unwrap()
                .into(),
            // char：4 字节码点 → 零扩展成 i64
            Type::Char => self
                .builder
                .build_int_z_extend(v.into_int_value(), self.context.i64_type(), "mem2char")
                .unwrap()
                .into(),
            _ => v,
        }
    }

    /// 把 Dial 的值转换成「内存表示」，用于写入裸指针（v1.02）。
    fn raw_to_mem(&self, t: &Type, v: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        match t {
            Type::Byte => self
                .builder
                .build_int_truncate(v.into_int_value(), self.context.i8_type(), "byte2mem")
                .unwrap()
                .into(),
            // bool：i1 → 1 字节 0/1
            Type::Bool => self
                .builder
                .build_int_z_extend(v.into_int_value(), self.context.i8_type(), "bool2mem")
                .unwrap()
                .into(),
            Type::Char => self
                .builder
                .build_int_truncate(v.into_int_value(), self.context.i32_type(), "char2mem")
                .unwrap()
                .into(),
            _ => v,
        }
    }

    /// 把**借来的**堆值存进更长寿的位置前，先 retain 一份，
    /// 让目标位置成为该对象的一个持有者。
    ///
    /// 少了这一步，存进去的只是一个**别名**：来源（变量 / 结构体 / 数组）一旦被释放，
    /// 目标位置就悬垂——读出来的是已归还给分配器的内存（use-after-free，
    /// 且读到的长度字段可能是垃圾，从而打印出成片乱码）。
    fn retain_if_borrowed(&self, e: &Expr, ty: &Type, val: BasicValueEnum<'ctx>) {
        if Self::is_heap_type(ty) && yields_borrowed_ref(e) {
            self.retain(ty, val);
        }
    }

    /// runtime 在把元素写进容器时会 retain 一份（容器成为该元素的持有者）。
    ///
    /// 若传入的值**自带所有权**（字面量、构造调用等），runtime 的 retain 就多算了一份——
    /// 那一份本应直接移交容器，故此处归还；否则每写一次就泄漏一个引用
    /// （`a.push("字面量")` / `a[0] = "字面量"` 在循环里会线性吃内存）。
    fn settle_fresh_value(&self, e: &Expr, ty: &Type, val: BasicValueEnum<'ctx>) {
        if Self::is_heap_type(ty) && !yields_borrowed_ref(e) {
            self.release_value(ty, val);
        }
    }

    /// 把元素写进容器（push/set/insert）后，归还「本地所有权」：
    /// - 结构体（v1.04 B6）：to_elem_bits 已把它装箱为 RC 对象（refcount 1），
    ///   容器 retain 后 refcount=2，此处用槽位位模式取回指针并 release，使其回到 1（容器独占）；
    /// - 其它类型沿用既有的 settle_fresh_value（句柄 release 本地那份，标量无操作）。
    fn sink_elem_after_retain(
        &self,
        e: &Expr,
        ty: &Type,
        val: BasicValueEnum<'ctx>,
        bits: IntValue<'ctx>,
    ) {
        if let Type::Struct(_) = ty {
            let boxp = self.builder.build_int_to_ptr(bits, self.i8ptr(), "boxp").unwrap();
            self.call_c_void("dial_rc_release", &[boxp.into()]);
        } else {
            self.settle_fresh_value(e, ty, val);
        }
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
            (Some(f), Some(t)) => (f.0.clone(), *t),
            _ => return,
        };
        for (i, (_fname, fty)) in fields.iter().enumerate() {
            let gep = self
                .builder
                .build_struct_gep(llvm_sty, ptr, i as u32, "frtn")
                .unwrap();
            match fty {
                // v0.8 补上 Error 与 Channel（此前漏掉，与 release_struct_fields 不对称）。
                Type::String | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error | Type::Channel(_) => {
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
            (Some(f), Some(t)) => (f.0.clone(), *t),
            _ => return,
        };
        for (i, (_fname, fty)) in fields.iter().enumerate() {
            let gep = self
                .builder
                .build_struct_gep(llvm_sty, ptr, i as u32, "frel")
                .unwrap();
            match fty {
                Type::String | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error | Type::Channel(_) => {
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
        // -g：必须在生成任何指令之前挂上 DISubprogram 并设好函数行号位置。
        self.begin_debug_function(f, function);

        // 每个函数独立：进入时清空受保护集合（否则上一个函数的登记会串进来）。
        self.protected.borrow_mut().clear();
        // 每个函数独立：清空 defer / 循环栈（与上个函数的状态隔离）。
        self.defer_stack.borrow_mut().clear();
        self.loop_stack.borrow_mut().clear();
        self.try_frames.borrow_mut().clear();

        // goto / label（v1.02）：做一次函数级分析，并**预分配**所有标签的目标基本块。
        // 前向 goto 要求目标块在生成跳转时就已存在，故必须提前建好。
        let analysis = analyze_gotos(&f.body)?;
        *self.goto_analysis.borrow_mut() = analysis;
        self.label_blocks.borrow_mut().clear();
        self.label_depth.borrow_mut().clear();
        {
            let names: Vec<String> = self
                .goto_analysis
                .borrow()
                .labels
                .keys()
                .cloned()
                .collect();
            for name in names {
                let bb = self
                    .context
                    .append_basic_block(function, &format!("label.{}", name));
                self.label_blocks.borrow_mut().insert(name, bb);
            }
        }

        let mut scopes: CgScopes = vec![HashMap::new()];
        // 函数体作为最外层作用域：压入对应的 defer 帧（下标 0，与 scopes[0] 对应）。
        self.push_defer_frame();

        // main 入口：把 OS 传来的 argc/argv 交给运行时，供 args() 内建取用。
        // （其余函数的参数仍按 Dial 签名正常处理，不受影响。）
        if f.name == "main" {
            let argc = function.get_nth_param(0).unwrap();
            let argv = function.get_nth_param(1).unwrap();
            self.call_c_void(
                "dial_env_init",
                &[argc.into(), argv.into()],
            );
        }
        for (i, (pname, pty)) in f.params.iter().enumerate() {
            let llvm_ty = self.dialc_type_to_llvm(pty);
            let ptr = self.alloca_in_entry(function, llvm_ty, pname);
            // v1.04：入口把 C ABI 宽度（bool=i32 / byte=i8）转回 Dial 内部宽度
            // （bool=i1 / byte=i64）再存进局部变量。函数体逻辑完全不用改。
            let raw_param = function.get_nth_param(i as u32).unwrap();
            let param_val = self.abi_adapt(raw_param, self.ffi_type_to_llvm(pty), llvm_ty);
            self.builder.build_store(ptr, param_val).unwrap();
            scopes[0].insert(pname.clone(), (pty.clone(), (true, ptr)));
            // -g：形参也写进 DWARF，GDB 的 `info args` / `print a` 才有名字。
            self.debug_declare(pname, pty, f.line, f.col, ptr, Some(i as u32 + 1));
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
        // 函数末尾未显式 return 时补默认返回（先排空 defer，再补默认返回）
        if !self.is_terminated() {
            self.drain_all_defers(function, &mut scopes, func_values, functions, structs)?;
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
                // v1.04：默认返回值也要从内部宽度适配到 C ABI 函数返回宽度。
                let to = function
                    .get_type()
                    .get_return_type()
                    .unwrap_or_else(|| self.ffi_type_to_llvm(&f.ret));
                let dv = self.abi_adapt(dv, dv.get_type(), to);
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
                // 句柄类一律按 ptr 槽读：dialc_type_to_llvm 把这些类型都 lowering 成
                // i8ptr，spawn 写端也用 i8ptr 槽 store（两者槽宽同为 8 字节，位模式一致）。
                // Opaque（包句柄）必须显式列在这里：若落到下面 `_` 的 i64 槽，会 load 出
                // i64 再传给期望 ptr 的形参，abi_adapt 只做整数间 zext/trunc、不做
                // inttoptr，最终被 LLVM verifier 判「参数类型与函数签名不符」。
                // Opaque 不进 ARC（retain/release_value 均为 no-op），因此多个 worker
                // 可按值安全共享同一个会话句柄，由包侧显式释放。
                Type::String | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error | Type::Channel(_) | Type::Opaque(_) => {
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
            // v1.04 FFI：被 spawn 的用户函数签名同样是 C ABI 宽度，传入前对称适配。
            // arg_vals 保留内部宽度值，供函数结束时的 release_value 使用。
            let call_v = self.abi_adapt(v, v.get_type(), self.ffi_type_to_llvm(pty));
            arg_vals.push(v);
            meta.push(call_v.into());
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
        // A7：未捕获异常同样带上抛出点的「文件:行:列」。
        self.emit_fail_loc("dial_panic_loc", cstr.into());
        self.builder.build_unreachable().unwrap();
        Ok(())
    }

    /// 递归解析「多维左值」并完成赋值：a[i][j] = v 时，逐层 dial_array_set ，
    /// 把生成的新容器写回外层容器，直到最外层的变量槽。value 是已求值好的右值；
    /// value_ty 是其类型（句柄元素按指针处理）。
    ///
    /// 实现要点（以 m[i][j] = v 为例）：
    ///   1. 读出 container = m[i] 当前指向的数组指针；
    ///   2. new_child = dial_array_set(container, j, to_i64(v))；
    ///   3. 递归地把 new_child 写回 container 左值本身（即 m[i]），直到根变量。
    /// 句柄元素在此路径上自动获得正确的 retain / release（由 dial_array_set 按元素类别完成）。
    #[allow(clippy::too_many_arguments)]
    fn gen_assign_lvalue(
        &self,
        lhs: &Expr,
        value: BasicValueEnum<'ctx>,
        value_ty: &Type,
        is_borrowed: bool,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<()> {
        match lhs {
            Expr::Var(name) => {
                let info = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()))
                    .ok_or_else(|| DialcError::new(format!("未声明的变量 '{}'", name), 0, 0))?;
                let (vty, (_m, ptr)) = info;
                // value 是「一份新拥有」的引用（dial_array_set_borrowed 的返回值），
                // 写回槽位后由槽位持有；槽位原先持有的那一份引用必须释放——
                // 否则每写一次嵌套下标就漏 1 个引用计数，COW 守卫随之恒真，
                // 每次嵌套赋值都深拷贝整层数组。
                let old = self.builder.build_load(self.i8ptr(), ptr, "oldval").unwrap();
                self.builder.build_store(ptr, value).unwrap();
                self.release_value(&vty, old);
                Ok(())
            }
            Expr::Index(container, idx) => {
                let carr = self.gen_expr(container, function, scopes, func_values, functions, structs)?;
                let cty = type_of(container, scopes, functions, structs)?;
                let ii64 = self.to_i64(
                    self.gen_expr(idx, function, scopes, func_values, functions, structs)?,
                    &type_of(idx, scopes, functions, structs)?,
                );
                // Fail-Fast：越界写当场 trap（与单层下标赋值一致）
                if self.checks_enabled() {
                    let len = self.call_c("dial_array_length", &[carr.into()]);
                    self.emit_bounds_check(function, len.into_int_value(), ii64);
                }
                let v = self.to_i64(value, value_ty);
                // 用 set_borrowed 而非 set：carr 是从外层数组**元素槽借来**的指针，
                // 它的那一份引用属于外层数组，不能在这里被消耗掉（否则外层元素替换
                // 时会重复释放同一份引用）。set_borrowed 不消耗它，而是交出
                // 「一份新拥有」的引用，由本层负责在装回外层后释放。
                let new_child = self.call_c(
                    "dial_array_set_borrowed",
                    &[carr.into(), ii64.into(), v.into()],
                );
                // 把新容器写回 container 左值（递归处理更外层）。new_child 是运行时
                // 返回的新对象，恒为自带所有权，故 is_borrowed=false。
                self.gen_assign_lvalue(container, new_child, &cty, false, function, scopes, func_values, functions, structs)?;
                // 所有权结算：本层是**消费者**——value 的那一份所有权是上一层交给我们的。
                // 元素替换（dial_array_set_borrowed 内部）对 value 额外 retain 了一份，
                // 归 container 的元素槽所有；所以我们手里这一份必须结算。
                //
                // 用与单层赋值一致的规则：借来的值（变量 / 字段 / 数组元素）来源仍持有，
                // 不得再释放；自带所有权的值（字面量 / clone 等）应把临时所有权移交数组。
                // （此前这里无条件 release，会把「变量元素」的引用计数多减一份。）
                // `is_borrowed` 由顶层调用按原始 Expr 判定；本层递归传入的 new_child
                // 是运行时返回的新对象，恒为自带所有权（borrowed=false）。
                if !is_borrowed {
                    self.release_value(value_ty, value);
                }
                Ok(())
            }
            _ => Err(DialcError::new(
                "语义错误：索引赋值目标必须是变量或数组下标",
                0,
                0,
            )),
        }
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
        // A7：先记下当前语句位置，供 panic / assert / 未捕获异常写进运行时诊断。
        // 嵌套语句递归调用 gen_stmt 时会覆盖为更精确的内层位置，正是我们要的。
        self.cur_pos.set(crate::semantic::stmt_pos(stmt));
        // -g：把后续所有 IR 指令标到本语句的 行:列；_loc 在函数返回（含早退）
        // 时自动恢复外层位置，嵌套语句不会污染外层的行号表。
        let _loc = self.stmt_debug_loc(stmt);
        match stmt {
            Stmt::VarDecl {
                name,
                ty,
                mutable,
                value,
                move_hint,
                line,
                col,
                ..
            } => {
                let llvm_ty = self.dialc_type_to_llvm(ty);
                let ptr = self.alloca_in_entry(function, llvm_ty, name);
                // -g：把 Dial 源码里的变量名绑到这个栈槽，GDB 才能 `print name`。
                self.debug_declare(name, ty, *line, *col, ptr, None);
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, ty);
                // 绑定策略：借用来的值 -> retain（成为新持有者）；自带所有权的值 -> 直接取得所有权。
                // 移动语义（move_hint）下所有权本就转移，不 retain。
                if !move_hint {
                    self.retain_if_borrowed(value, ty, stored);
                }
                if let Expr::Var(src) = value {
                    if *move_hint {
                        self.mark_moved(scopes, src);
                    }
                }
                self.builder.build_store(ptr, stored).unwrap();
                // 结构体：值拷贝已把堆字段的指针复制过来，需要对称地给这些字段 +1，
                // 否则本变量在作用域结束时的 release 会与源变量互相踩踏。
                // 结构体字面量例外：它的字段在构造时就已各自 retain 过
                // （见 Expr::StructLit），这里再 retain 一遍就多算了一份。
                if !move_hint && yields_borrowed_ref(value) && matches!(ty, Type::Struct(_)) {
                    self.retain_struct_fields(ty, ptr, structs);
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
                    if yields_borrowed_ref(value) && !move_hint {
                        self.retain_struct_fields(&vty, tmp, structs);
                    }
                    self.release_struct_fields(&vty, ptr, structs);
                    let v = self.builder.build_load(llvm_ty, tmp, "asgval").unwrap();
                    self.builder.build_store(ptr, v).unwrap();
                } else {
                    if !move_hint {
                        self.retain_if_borrowed(value, &vty, stored);
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
                let ot = type_of(obj, scopes, functions, structs)?;
                // v1.04 FFI：易失指针字段写入走 volatile 存储。
                let vol = matches!(ot, Type::VolatilePtr(_));
                let base = self.field_base_ptr(obj, &ot, function, scopes, func_values, functions, structs)?;
                // 找到字段索引与类型
                let sty = self.get_struct_type(&ot)?;
                let (fidx, fty) = self.field_info(&ot, field, structs, *line, *col)?;
                let fgep = self
                    .builder
                    .build_struct_gep(sty, base, fidx, "fgep")
                    .unwrap();
                let val = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                let stored = self.coerce(val, &type_of(value, scopes, functions, structs)?, &fty);
                if self.field_is_c_abi(&ot) {
                    // v1.04 FFI：C 侧内存字段**不受 ARC 管理**（is_c_abi_field 已排除
                    // 所有堆类型），既不 release 旧值也不 retain 新值。只需把 Dial 内部
                    // 宽度写成 C 字段宽度（bool/byte → i8），否则会破坏 struct 布局。
                    let cval = self.abi_adapt(
                        stored,
                        stored.get_type(),
                        self.cstruct_field_llvm(&fty),
                    );
                    // v1.04 FFI：易失指针字段写入走 volatile 存储——每次写入必须真实落到总线。
                    let inst = self.builder.build_store(fgep, cval).unwrap();
                    if vol { inst.set_volatile(true).unwrap(); }
                    return Ok(());
                }
                // 覆盖前 release 旧字段值（堆类型）
                if Self::is_heap_type(&fty) {
                    self.release(&fty, fgep);
                }
                // 字段成为新值的持有者：借来的值必须 retain 一份。
                // 少了这一步，字段只是来源的别名——来源所在的块一结束，
                // 字段就悬垂（读出来是已释放内存）。
                self.retain_if_borrowed(value, &fty, stored);
                // v1.04 FFI：易失指针字段写入走 volatile 存储。
                let inst = self.builder.build_store(fgep, stored).unwrap();
                if vol { inst.set_volatile(true).unwrap(); }
                Ok(())
            }
            Stmt::AssignIndex {
                arr,
                index,
                value,
                line,
                col,
            } => {
                // 嵌套下标赋值（a[i][j] = v）：交给递归左值解析，逐层 dial_array_set
                // 并把新容器写回外层容器，直到最外层变量槽。单层下标赋值（a[i] = v，
                // a 为变量）仍走下方既有逻辑——保持 int[] 既有测试路径完全不变。
                if matches!(arr.as_ref(), &Expr::Index(..)) {
                    // 嵌套下标赋值（a[i][j] = v）：把 arr[index] 拼成完整的左值表达式
                    // Index(Index(m, 0), 0)，交给递归左值解析。gen_assign_lvalue 会从最外层
                    // Index 一层层解包：gen_expr(Index(m,0)) 取出内层数组句柄 → dial_array_set
                    // 写入值 → 再把「可能 COW 更新」的句柄写回外层容器，直到最外层变量槽。
                    // 单层下标赋值（a[i] = v，a 为变量）走下方既有逻辑——int[] 既有测试路径不变。
                    let full = Expr::Index(arr.clone(), index.clone());
                    let vt = type_of(value, scopes, functions, structs)?;
                    let v = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                    return self.gen_assign_lvalue(
                        &full,
                        v,
                        &vt,
                        yields_borrowed_ref(value),
                        function,
                        scopes,
                        func_values,
                        functions,
                        structs,
                    );
                }
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
                let vty = type_of(value, scopes, functions, structs)?;
                let vali64 = self.to_elem_bits(function, val, &vty);
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
                // 元素归属结算：set 已为元素 retain 一份（数组成为该元素的持有者）。
                //   - 借来的值（变量 / 字段 / 数组元素）：来源仍持有，不得再释放；
                //   - 自带所有权的值（字面量 / clone 等）：本语句的临时所有权应当
                //     移交给数组，故此处归还——否则每个字面量元素泄漏一份引用
                //     （`a[i] = "字面量"`、`a.push("字面量")` 在循环里线性吃内存）。
                self.sink_elem_after_retain(value, &vty, val, vali64);
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
                self.push_defer_frame();
                for s in then_branch {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                }
                self.builder.position_at_end(else_bb);
                if let Some(eb) = else_branch {
                    scopes.push(HashMap::new());
                    self.push_defer_frame();
                    for s in eb {
                        self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                    }
                    self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
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
                // 记录循环体帧下标（break/continue 据此排空被跳出作用域的 defer）
                self.loop_stack.borrow_mut().push(self.defer_stack.borrow().len());
                scopes.push(HashMap::new());
                self.push_defer_frame();
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                self.loop_targets.borrow_mut().pop();
                self.loop_stack.borrow_mut().pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(header_bb).unwrap();
                }
                self.builder.position_at_end(exit_bb);
                Ok(())
            }
            Stmt::Break(line, col) => {
                let from = *self
                    .loop_stack
                    .borrow()
                    .last()
                    .ok_or_else(|| DialcError::new("内部错误：break 不在循环内", *line, *col))?;
                // 发射被跳出作用域（含循环体本身）的延迟语句（LIFO）；帧保留、仅清空其内容，
                // 以保持与 `scopes` 下标同步（外层臂的 release/pop 随后照常执行，不会重复发射）。
                // 注：break 的局部释放沿用既有约定（由外层臂统一处理），此处只保证 defer 正确触发。
                self.drain_defers_from(from, function, scopes, func_values, functions, structs)?;
                // 释放被跳出的作用域（含循环体本身）的局部：break 的落穿路径在循环体
                // 末尾才做这件事，而 break **提前离开了循环体**——那条路径永远走不到，
                // 于是句柄永远不被 release（实测 continue 20 万轮泄漏 ~12 MB）。
                // 两条路径互斥（break 分支跳走、落穿路径不跳），故各自发一份释放代码是安全的。
                {
                    let cur = scopes.len();
                    for k in (from..cur).rev() {
                        self.release_scope_locals(&scopes[k], structs);
                    }
                }
                let target = self
                    .loop_targets
                    .borrow()
                    .last()
                    .map(|t| t.1)
                    .ok_or_else(|| DialcError::new("内部错误：break 不在循环内", *line, *col))?;
                // 注意：`loop_targets` / `loop_stack` 是**编译期**静态栈，`break` 只是运行期的
                // 一条分支，绝不能在这里 pop——否则同一循环体中后续的 break/continue 会找不到
                // 栈帧（错误「break 不在循环内」）。这两个栈由循环臂在生成结束后统一 pop。
                self.builder.build_unconditional_branch(target).unwrap();
                // 后续语句（若有）落进新块，避免往已终结的块里继续追加指令
                let dead = self.context.append_basic_block(function, "after.brk");
                self.builder.position_at_end(dead);
                Ok(())
            }
            Stmt::Continue(line, col) => {
                let from = *self
                    .loop_stack
                    .borrow()
                    .last()
                    .ok_or_else(|| DialcError::new("内部错误：continue 不在循环内", *line, *col))?;
                // 与 break 同：离开循环体所跨越的作用域前先按 LIFO 触发 defer（帧保留、内容清空）
                self.drain_defers_from(from, function, scopes, func_values, functions, structs)?;
                // 同 break：continue 同样提前离开了循环体，落穿路径的 release 永远走不到，
                // 必须在这里补一份，否则每轮迭代泄漏一次。
                {
                    let cur = scopes.len();
                    for k in (from..cur).rev() {
                        self.release_scope_locals(&scopes[k], structs);
                    }
                }
                let target = self
                    .loop_targets
                    .borrow()
                    .last()
                    .map(|t| t.0)
                    .ok_or_else(|| DialcError::new("内部错误：continue 不在循环内", *line, *col))?;
                // 同 break：编译期静态栈不在此处 pop，由循环臂统一收尾。
                self.builder.build_unconditional_branch(target).unwrap();
                let dead = self.context.append_basic_block(function, "after.cont");
                self.builder.position_at_end(dead);
                Ok(())
            }
            Stmt::For { var, start, end, step, body, .. } => {
                let i64t = self.context.i64_type();
                // 循环变量：在入口块分配，跨迭代存活。
                let var_ptr = self.alloca_in_entry(function, i64t.into(), var);
                let start_val = self.gen_expr(start, function, scopes, func_values, functions, structs)?;
                let start_i64 = self.to_i64(start_val, &type_of(start, scopes, functions, structs)?);
                self.builder.build_store(var_ptr, start_i64).unwrap();
                // 步长：缺省 1；否则在循环前一次性求值（与上下界一致，语义等价于快照）。
                let step_i64: IntValue = match step {
                    Some(e) => {
                        let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                        self.to_i64(v, &type_of(e, scopes, functions, structs)?)
                    }
                    None => i64t.const_int(1, false),
                };
                // 把循环变量登记进作用域（int 非堆类型，无需 retain/release）。
                scopes.push(HashMap::new());
                // 记录循环体帧下标（break/continue 据此排空被跳出作用域的 defer）。
                let for_frame = self.defer_stack.borrow().len();
                self.push_defer_frame();
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(var.clone(), (Type::Int, (true, var_ptr)));

                // 注意：自增必须落在独立的 for.inc 块，continue 跳到这里而非 for.cond，
                // 否则 continue 会跳过 i = i + step 造成死循环。
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
                // for 的 continue 目标是自增块 for.inc（先 i += step 再回到条件判定）
                self.loop_targets.borrow_mut().push((inc_bb, exit_bb));
                self.loop_stack.borrow_mut().push(for_frame);
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                self.loop_targets.borrow_mut().pop();
                self.loop_stack.borrow_mut().pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(inc_bb).unwrap();
                }
                self.builder.position_at_end(inc_bb);
                // i = i + step
                let cur2 = self.builder.build_load(i64t, var_ptr, "i").unwrap();
                let next = self
                    .builder
                    .build_int_add(cur2.into_int_value(), step_i64, "iinc")
                    .unwrap();
                self.builder.build_store(var_ptr, next).unwrap();
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(exit_bb);
                scopes.pop();
                Ok(())
            }
            Stmt::ForEach { value, value_line: _, value_col: _, coll, body, line, col } => {
                let i64t = self.context.i64_type();
                let coll_t = type_of(coll, scopes, functions, structs)?;
                let elem_t = match &coll_t {
                    Type::Array(e) => (**e).clone(),
                    Type::Set(e) => (**e).clone(),
                    _ => Type::Int, // 不应到达（语义已校验为 Array / Set）
                };
                let coll_val = self.gen_expr(coll, function, scopes, func_values, functions, structs)?;
                // 集合迭代：成员即 map 的键，先快照为 int[]，再复用数组迭代路径。
                // 快照登记到函数级作用域帧，由出口统一释放（循环体内 return 也不会泄漏）。
                let arr_val = if matches!(coll_t, Type::Set(_)) {
                    let keys = self.call_c("dial_map_keys", &[coll_val.into()]);
                    let slot = self.alloca_in_entry(function, self.i8ptr().into(), "set.keys");
                    self.builder.build_store(slot, keys).unwrap();
                    scopes.first_mut().unwrap().insert(
                        format!("__setkeys_{}_{}", line, col),
                        (Type::Array(Box::new(Type::Int)), (true, slot)),
                    );
                    keys
                } else {
                    coll_val
                };
                let len = self.call_c("dial_array_length", &[arr_val.into()]);
                let v_ptr = self.alloca_in_entry(function, self.dialc_type_to_llvm(&elem_t), value);
                // 循环变量 i（下标）
                let i_ptr = self.alloca_in_entry(function, i64t.into(), "fei");
                self.builder
                    .build_store(i_ptr, i64t.const_int(0, false))
                    .unwrap();
                let header_bb = self.context.append_basic_block(function, "fe.cond");
                let body_bb = self.context.append_basic_block(function, "fe.body");
                let inc_bb = self.context.append_basic_block(function, "fe.inc");
                let exit_bb = self.context.append_basic_block(function, "fe.end");
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(header_bb);
                let i_val = self.builder.build_load(i64t, i_ptr, "fei").unwrap();
                let cond = self
                    .builder
                    .build_int_compare(IntPredicate::SLT, i_val.into_int_value(), len.into_int_value(), "fecond")
                    .unwrap();
                self.builder.build_conditional_branch(cond, body_bb, exit_bb).unwrap();
                self.builder.position_at_end(body_bb);
                self.loop_targets.borrow_mut().push((inc_bb, exit_bb));
                let fe_frame = self.defer_stack.borrow().len();
                self.push_defer_frame();
                self.loop_stack.borrow_mut().push(fe_frame);
                scopes.push(HashMap::new());
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(value.clone(), (elem_t.clone(), (true, v_ptr)));
                // 取第 i 个元素并绑定到 v（按元素类型还原位模式；句柄类元素需 retain 一份）
                let res = self.call_c("dial_array_get", &[arr_val.into(), i_val.into()]);
                let ev = self.from_elem_bits(&elem_t, res.into_int_value());
                if Self::is_heap_type(&elem_t) {
                    self.retain(&elem_t, ev);
                }
                self.builder.build_store(v_ptr, ev).unwrap();
                for s in body {
                    self.gen_stmt(&s, function, scopes, func_values, functions, structs)?;
                }
                // 每轮迭代结束释放本元素（v 槽被标记 moved，避免循环退出时重复释放）
                if Self::is_heap_type(&elem_t) {
                    self.release(&elem_t, v_ptr);
                }
                self.moved.borrow_mut().insert(v_ptr);
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                self.loop_targets.borrow_mut().pop();
                self.loop_stack.borrow_mut().pop();
                scopes.pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(inc_bb).unwrap();
                }
                self.builder.position_at_end(inc_bb);
                let i2 = self.builder.build_load(i64t, i_ptr, "fei").unwrap();
                let i3 = self
                    .builder
                    .build_int_add(i2.into_int_value(), i64t.const_int(1, false), "feinc")
                    .unwrap();
                self.builder.build_store(i_ptr, i3).unwrap();
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(exit_bb);
                Ok(())
            }
            Stmt::ForEachMap {
                key,
                key_line: _,
                key_col: _,
                value,
                value_line: _,
                value_col: _,
                coll,
                body,
                line,
                col,
            } => {
                let i64t = self.context.i64_type();
                // 判定容器类型：map（int->int）走 dial_map_*；map<K,V> 走 dial_kvmap_*。
                // v1.01 起两者都支持 for (k, v in m)。
                let ct = type_of(coll, scopes, functions, structs)?;
                let is_kv = matches!(ct, Type::KvMap(..));
                let (k_ty, v_ty): (Type, Type) = match &ct {
                    Type::KvMap(k, v) => ((**k).clone(), (**v).clone()),
                    _ => (Type::Int, Type::Int),
                };
                let coll_val = self.gen_expr(coll, function, scopes, func_values, functions, structs)?;
                // 快照键集登记进函数级作用域帧：由 return / 函数末尾的 release_all 统一释放。
                // 这样即便循环体内提前 return 也不会泄漏；循环体帧每轮只释放 k/v，
                // 不会误释放快照，正常落穿与提前返回走同一条释放路径。
                // KvMap 的键数组元素类别与 K 一致（string 键为 RC 数组，元素已 retain），
                // 释放数组时一并释放其元素。
                let keys = if is_kv {
                    self.call_c("dial_kvmap_keys", &[coll_val.into()])
                } else {
                    self.call_c("dial_map_keys", &[coll_val.into()])
                };
                let keys_slot = self.alloca_in_entry(function, self.i8ptr().into(), "map.keys");
                self.builder.build_store(keys_slot, keys).unwrap();
                scopes.first_mut().unwrap().insert(
                    format!("__mapkeys_{}_{}", line, col),
                    (Type::Array(Box::new(k_ty.clone())), (true, keys_slot)),
                );
                let key_len = self.call_c("dial_array_length", &[keys.into()]);
                // map 的键/值恒为 int（i64 槽）；map<K,V> 的键/值按真实类型分配槽位。
                let k_llvm: BasicTypeEnum = if is_kv { self.dialc_type_to_llvm(&k_ty) } else { i64t.into() };
                let v_llvm: BasicTypeEnum = if is_kv { self.dialc_type_to_llvm(&v_ty) } else { i64t.into() };
                let k_ptr = self.alloca_in_entry(function, k_llvm, key.as_str());
                let v_ptr = self.alloca_in_entry(function, v_llvm, value.as_str());
                let i_ptr = self.alloca_in_entry(function, i64t.into(), "fmi");
                self.builder.build_store(i_ptr, i64t.const_int(0, false)).unwrap();
                let header_bb = self.context.append_basic_block(function, "fem.cond");
                let body_bb = self.context.append_basic_block(function, "fem.body");
                let inc_bb = self.context.append_basic_block(function, "fem.inc");
                let exit_bb = self.context.append_basic_block(function, "fem.end");
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(header_bb);
                let i_val = self.builder.build_load(i64t, i_ptr, "fmi").unwrap();
                let cond = self
                    .builder
                    .build_int_compare(IntPredicate::SLT, i_val.into_int_value(), key_len.into_int_value(), "femcond")
                    .unwrap();
                self.builder.build_conditional_branch(cond, body_bb, exit_bb).unwrap();
                self.builder.position_at_end(body_bb);
                self.loop_targets.borrow_mut().push((inc_bb, exit_bb));
                let fem_frame = self.defer_stack.borrow().len();
                self.push_defer_frame();
                self.loop_stack.borrow_mut().push(fem_frame);
                scopes.push(HashMap::new());
                scopes.last_mut().unwrap().insert(key.clone(), (k_ty.clone(), (true, k_ptr)));
                scopes.last_mut().unwrap().insert(value.clone(), (v_ty.clone(), (true, v_ptr)));
                // k = keys[i]（位模式），按 K 类型还原；v = m.get(k)。
                let ki = self.call_c("dial_array_get", &[keys.into(), i_val.into()]);
                let kval = self.from_elem_bits(&k_ty, ki.into_int_value());
                let vi = if is_kv {
                    // map<K,V>：键以位模式交给 dial_kvmap_get，返回值再以 V 类型还原。
                    // get 返回的是「借用」自表内的值（未 retain），因此循环末尾不得释放 v。
                    let kb = self.to_elem_bits(function, kval, &k_ty);
                    self.call_c("dial_kvmap_get", &[coll_val.into(), kb.into()])
                } else {
                    self.call_c("dial_map_get", &[coll_val.into(), ki.into()])
                };
                let vval = self.from_elem_bits(&v_ty, vi.into_int_value());
                self.builder.build_store(k_ptr, kval).unwrap();
                self.builder.build_store(v_ptr, vval).unwrap();
                // map<K,V> 的 k/v 均借自键快照数组 / 表本身（未转移所有权），
                // 标记 moved 使循环末尾与函数出口的释放跳过它们，避免重复释放 / 误释放。
                if is_kv {
                    self.moved.borrow_mut().insert(k_ptr);
                    self.moved.borrow_mut().insert(v_ptr);
                }
                for s in body {
                    self.gen_stmt(&s, function, scopes, func_values, functions, structs)?;
                }
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                self.loop_targets.borrow_mut().pop();
                self.loop_stack.borrow_mut().pop();
                scopes.pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(inc_bb).unwrap();
                }
                self.builder.position_at_end(inc_bb);
                let i2 = self.builder.build_load(i64t, i_ptr, "fmi").unwrap();
                let i3 = self
                    .builder
                    .build_int_add(i2.into_int_value(), i64t.const_int(1, false), "feminc")
                    .unwrap();
                self.builder.build_store(i_ptr, i3).unwrap();
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(exit_bb);
                // 快照键集的释放统一交给函数出口（见上方函数级帧登记），此处无需再显式 release。
                Ok(())
            }
            Stmt::DoWhile { body, cond, .. } => {
                let body_bb = self.context.append_basic_block(function, "dw.body");
                let cond_bb = self.context.append_basic_block(function, "dw.cond");
                let exit_bb = self.context.append_basic_block(function, "dw.end");
                self.builder.build_unconditional_branch(body_bb).unwrap();
                self.builder.position_at_end(body_bb);
                // do-while 的 continue 回到条件判定
                self.loop_targets.borrow_mut().push((cond_bb, exit_bb));
                let dw_frame = self.defer_stack.borrow().len();
                self.push_defer_frame();
                self.loop_stack.borrow_mut().push(dw_frame);
                scopes.push(HashMap::new());
                for s in body {
                    self.gen_stmt(&s, function, scopes, func_values, functions, structs)?;
                }
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                self.loop_targets.borrow_mut().pop();
                self.loop_stack.borrow_mut().pop();
                scopes.pop();
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(cond_bb).unwrap();
                }
                self.builder.position_at_end(cond_bb);
                let cond_val = self.gen_expr(cond, function, scopes, func_values, functions, structs)?;
                let cond_i1 = cond_val.into_int_value();
                self.builder
                    .build_conditional_branch(cond_i1, body_bb, exit_bb)
                    .unwrap();
                self.builder.position_at_end(exit_bb);
                Ok(())
            }
            Stmt::Match { subject, arms, .. } => {
                let i64t = self.context.i64_type();
                let subj_ty = type_of(subject, scopes, functions, structs)?;
                let subj_val = self.gen_expr(subject, function, scopes, func_values, functions, structs)?;
                let end_bb = self.context.append_basic_block(function, "match.end");
                for (idx, arm) in arms.into_iter().enumerate() {
                    let arm_bb = self.context.append_basic_block(function, "match.arm");
                    let next_bb = self.context.append_basic_block(function, "match.next");
                    match &arm.pattern {
                        MatchPattern::Wildcard => {
                            // 通配：直接落入本分支（后续分支成为不可达死代码，合法）
                            self.builder.build_unconditional_branch(arm_bb).unwrap();
                        }
                        _ => {
                            let mcond = match (&arm.pattern, &subj_ty) {
                                (MatchPattern::Int(p), Type::Int) => self.builder.build_int_compare(
                                    IntPredicate::EQ,
                                    subj_val.clone().into_int_value(),
                                    i64t.const_int(*p as u64, false),
                                    "match.cmp",
                                ),
                                (MatchPattern::Bool(p), Type::Bool) => self.builder.build_int_compare(
                                    IntPredicate::EQ,
                                    subj_val.clone().into_int_value(),
                                    self.context.bool_type().const_int(if *p { 1 } else { 0 }, false),
                                    "match.cmp",
                                ),
                                (MatchPattern::Char(c), Type::Char) => self.builder.build_int_compare(
                                    IntPredicate::EQ,
                                    subj_val.clone().into_int_value(),
                                    i64t.const_int(*c as u64, false),
                                    "match.cmp",
                                ),
                                (MatchPattern::Str(s), Type::String) => {
                                    // 模式字面量必须先经 dial_string_new 包成 DialString（带 ARC 头），
                                    // 不能把裸 C 字符串直接交给 dial_string_eq——那会读错内存布局。
                                    let sg = self.global_string(&format!("matchstr_{}", idx), s);
                                    let sobj = self.call_c("dial_string_new", &[sg.into()]);
                                    let slot = self
                                        .alloca_in_entry(function, self.i8ptr().into(), "match.str");
                                    self.builder.build_store(slot, sobj).unwrap();
                                    let r = self.call_c("dial_string_eq", &[subj_val.clone().into(), sobj.into()]);
                                    // 临时对象比较完即释放，避免每个分支的模式串泄漏。
                                    self.release(&Type::String, slot);
                                    self.builder.build_int_compare(
                                        IntPredicate::EQ,
                                        r.into_int_value(),
                                        i64t.const_int(1, false),
                                        "match.cmp",
                                    )
                                }
                                _ => {
                                    return Err(DialcError::new(
                                        "内部错误：match 模式与主体类型不匹配",
                                        0, 0,
                                    ))
                                }
                            }
                            .unwrap();
                            self.builder.build_conditional_branch(mcond, arm_bb, next_bb).unwrap();
                        }
                    }
                    // 分支体：独立作用域（含 defer 帧）
                    self.builder.position_at_end(arm_bb);
                    scopes.push(HashMap::new());
                    self.push_defer_frame();
                    for s in &arm.body {
                        self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                    }
                    self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                    self.release_scope_locals(scopes.last().unwrap(), structs);
                    scopes.pop();
                    if !self.is_terminated() {
                        self.builder.build_unconditional_branch(end_bb).unwrap();
                    }
                    // 下一个分支的比较落在此处（当前分支未命中时）
                    self.builder.position_at_end(next_bb);
                }
                // 收尾：未命中任何分支（理论上被 _ 覆盖）时安全落到 end
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(end_bb).unwrap();
                }
                self.builder.position_at_end(end_bb);
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
                        // 无返回值：先排空 defer，再释放本函数持有的全部堆对象后返回
                        self.drain_all_defers(function, scopes, func_values, functions, structs)?;
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
                        // v1.04：统一按「实际值宽度 → 函数 LLVM 返回宽度」做 C ABI 适配。
                        // 一条同时覆盖 main 的 int(i64)→i32、bool(i1)→i32、byte(i64)→i8，
                        // 也保证函数指针指向的真实签名与 FnPtr 声明精确一致（C 侧可安全调用）。
                        let ret_val: BasicValueEnum = match function.get_type().get_return_type() {
                            Some(to) => self.abi_adapt(val, val.get_type(), to),
                            None => {
                                return Err(DialcError::new(
                                    "内部错误：非 void 函数不应返回 void",
                                    *line, *col,
                                ))
                            }
                        };
                        self.drain_all_defers(function, scopes, func_values, functions, structs)?;
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
                // longjmp 离开 try 体作用域前，按 LIFO 触发该帧及其内层的 defer（资源清理不遗漏）。
                // 无外层 try 时（未捕获异常，程序即将终止）退化为排空全部 defer。
                let tf = self.try_frames.borrow().last().copied().unwrap_or(0);
                self.drain_defers_from(tf, function, scopes, func_values, functions, structs)?;
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
                self.try_frames.borrow_mut().push(self.defer_stack.borrow().len());
                scopes.push(HashMap::new());
                self.push_defer_frame();
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                    if self.is_terminated() {
                        break;
                    }
                }
                // 先触发本层 defer（可能引用本层局部），再释放局部；帧与 scopes 同步弹出。
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
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
                self.push_defer_frame();
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
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
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
                self.try_frames.borrow_mut().pop();
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
                    self.push_defer_frame();
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
                    self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
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
                    self.push_defer_frame();
                    for s in default_body.as_ref().unwrap() {
                        self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                        if self.is_terminated() {
                            break;
                        }
                    }
                    self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
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
                // 裸块：引入新词法作用域，递归发射内部语句；离开作用域时先按 LIFO 触发本块 defer，
                // 再释放块内堆类型局部变量，防止数组/字符串/映射/错误/通道等 ARC 对象泄漏。
                scopes.push(HashMap::new());
                self.push_defer_frame();
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                Ok(())
            }
            Stmt::Defer(inner, ..) => {
                // 不立即执行，登记到当前最内层作用域的延迟队列，待离开该作用域时按 LIFO 触发。
                self.add_defer((**inner).clone());
                Ok(())
            }
            // 裸指针解引用赋值 `*p = v`（v1.02）
            Stmt::AssignDeref { ptr, value, .. } => {
                let pt = type_of(ptr, scopes, functions, structs)?;
                let (elem, vol) = match pt {
                    Type::RawPtr(el) => (*el, false),
                    Type::VolatilePtr(el) => (*el, true),
                    other => {
                        return Err(DialcError::new(
                            format!(
                                "内部错误：'*p = v' 的左侧收到 {} 类型（应由 Sema 先行拦截）",
                                other.name()
                            ),
                            0,
                            0,
                        ))
                    }
                };
                let p = self.gen_expr(ptr, function, scopes, func_values, functions, structs)?;
                let v = self.gen_expr(value, function, scopes, func_values, functions, structs)?;
                // 元素类型与写入值的类型由 Sema 核对过；这里把 Dial 值转成内存表示再写。
                let v = self.raw_to_mem(&elem, v);
                // v1.04 FFI：易失指针经 volatile 存储——每次写入必须真实落到总线。
                let inst = self.builder.build_store(p.into_pointer_value(), v).unwrap();
                if vol { inst.set_volatile(true).unwrap(); }
                Ok(())
            }
            // ===================== with 资源作用域块（v1.02） =====================
            Stmt::With {
                resource,
                name,
                body,
                line,
                col,
                ..
            } => {
                // 1) 先求值资源表达式（在**外层**作用域里求值，语义与普通表达式一致）。
                let res_val =
                    self.gen_expr(resource, function, scopes, func_values, functions, structs)?;
                let res_ty = type_of(resource, scopes, functions, structs)?;
                // 2) 进入 with 作用域（新局部作用域 + 新 defer 帧）。
                scopes.push(HashMap::new());
                self.push_defer_frame();
                // 3) 把资源绑定到块内的名字上。
                let llvm_ty = self.dialc_type_to_llvm(&res_ty);
                let ptr = self.alloca_in_entry(function, llvm_ty, name.as_str());
                self.builder.build_store(ptr, res_val).unwrap();
                scopes
                    .last_mut()
                    .unwrap()
                    .insert(name.clone(), (res_ty.clone(), (true, ptr)));
                // 4) 登记「块末释放资源」。
                //
                // 走**既有的 defer 机制**而不是自己发明一条出口路径：defer 覆盖了
                // 正常落穿 / return / break / continue / throw 全部出口，且各出口互斥地
                // 各执行一次——这正是 with 的「自动释放」所需要的语义。
                //
                // 用内部调用 __with_release 而非直接写 close/join：块内若显式
                // close(f) 过，__with_release 会看到该变量已被标记 moved 而**跳过**，
                // 从而不会二次关闭（fclose 二次调用是 UB）。
                self.add_defer(Stmt::Expr(
                    Expr::Call(
                        "__with_release".to_string(),
                        vec![Expr::Var(name.clone())],
                    ),
                    *line,
                    *col,
                ));
                // 5) 生成块体。
                for s in body {
                    self.gen_stmt(s, function, scopes, func_values, functions, structs)?;
                }
                // 6) 离开 with 作用域：先排空本层 defer（含上面的自动释放），再释放块内局部。
                self.pop_defer_frame_emit(function, scopes, func_values, functions, structs)?;
                self.release_scope_locals(scopes.last().unwrap(), structs);
                scopes.pop();
                Ok(())
            }
            // ===================== goto / label（v1.02） =====================
            Stmt::Label(name, line, col) => {
                let bb = self.label_block(name, function);
                // 把执行流接到标签块：前一条语句若未终结，补一条无条件跳转。
                if !self.is_terminated() {
                    self.builder.build_unconditional_branch(bb).unwrap();
                }
                self.builder.position_at_end(bb);
                // 记录该标签所在处的作用域深度（codegen 口径），供 goto 计算释放层数。
                let d = scopes.len();
                self.label_depth.borrow_mut().insert(name.clone(), d);
                // 与语义层的深度核对：两层对「块」的划分必须一致，
                // 否则 goto 跳出会释放错层的作用域（悬垂 / 二次释放）。不一致即中止编译。
                let sem_depth = self
                    .goto_analysis
                    .borrow()
                    .labels
                    .get(name)
                    .map(|s| s.depth);
                if let Some(sd) = sem_depth {
                    if sd != d {
                        return Err(DialcError::new(
                            format!(
                                "内部错误：标签 '{}' 的作用域深度在语义层（{}）与代码生成层（{}）不一致",
                                name, sd, d
                            ),
                            *line,
                            *col,
                        ));
                    }
                }
                Ok(())
            }
            Stmt::Goto(target, line, col) => {
                let bb = self.label_block(target, function);
                // 要释放几层作用域：当前深度 - 标签所在深度。
                // 标签若还没生成（向前跳），用语义层给出的深度；已生成则用 codegen 记录值
                // （生成标签时已核对过两者一致）。
                let target_depth = self
                    .label_depth
                    .borrow()
                    .get(target)
                    .copied()
                    .or_else(|| {
                        self.goto_analysis
                            .borrow()
                            .labels
                            .get(target)
                            .map(|s| s.depth)
                    })
                    .unwrap_or(scopes.len());
                let cur = scopes.len();
                if target_depth > cur {
                    return Err(DialcError::new(
                        format!("内部错误：goto 目标标签 '{}' 的作用域深度异常", target),
                        *line,
                        *col,
                    ));
                }
                // 离开被跨越的作用域：先按 LIFO 触发它们的 defer，再释放它们的局部。
                // （约束 ② 保证不会跨越 with 块，故 with 的自动释放不会在此被绕过。）
                if target_depth < cur {
                    self.drain_defers_from(
                        target_depth,
                        function,
                        scopes,
                        func_values,
                        functions,
                        structs,
                    )?;
                    for k in (target_depth..cur).rev() {
                        self.release_scope_locals(&scopes[k], structs);
                    }
                }
                self.builder.build_unconditional_branch(bb).unwrap();
                // 后续语句落进新块，避免往已终结的块里继续追加指令。
                let dead = self.context.append_basic_block(function, "after.goto");
                self.builder.position_at_end(dead);
                Ok(())
            }
            Stmt::Expr(e, ..) => {
                // 语句级方法调用且返回新容器指针（push/set）-> 把结果写回接收变量
                if let Expr::Method(obj, method, args) = e {
                    if matches!(method.as_str(), "push" | "set" | "put" | "add") {
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

    /// v1.04 FFI：取出「可做字段访问」的结构体名。
    /// 值语义 `struct` 直接返回名字；`rawptr<CStruct>` 则解一层裸指针。
    fn struct_name_of(t: &Type) -> Option<&str> {
        match t {
            Type::Struct(n) => Some(n.as_str()),
            Type::RawPtr(inner) | Type::VolatilePtr(inner) => match &**inner {
                Type::Struct(n) => Some(n.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// v1.04 FFI：该字段访问是否落在 `extern struct`（C ABI 布局）上。
    fn field_is_c_abi(&self, ot: &Type) -> bool {
        Self::struct_name_of(ot)
            .map(|n| self.cstruct_names.contains(n))
            .unwrap_or(false)
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
        if let Some(sname) = Self::struct_name_of(ot) {
            let fields = &structs
                .get(sname)
                .ok_or_else(|| DialcError::new(format!("未知结构体 '{}'", sname), line, col))?
                .0;
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

    /// 由类型取得其 LLVM 结构体类型（struct 与 rawptr<CStruct> 均有效）。
    fn get_struct_type(&self, ot: &Type) -> Result<StructType<'ctx>> {
        match Self::struct_name_of(ot) {
            Some(name) => Ok(self.struct_types[name]),
            None => Err(DialcError::new(
                format!("语义错误：类型 '{}' 不是结构体，无法做字段访问", ot.name()),
                0,
                0,
            )),
        }
    }

    /// v1.04 FFI：求字段访问的基址指针。
    ///
    /// - `rawptr<CStruct>`：对象本身就求值为 C 侧内存地址，直接当基址；
    /// - 值语义 `struct`：对象是栈上（或父结构体内）的值，需取 lvalue 指针。
    fn field_base_ptr(
        &self,
        obj: &Expr,
        ot: &Type,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<PointerValue<'ctx>> {
        if matches!(ot, Type::RawPtr(_) | Type::VolatilePtr(_)) {
            Ok(self
                .gen_expr(obj, function, scopes, func_values, functions, structs)?
                .into_pointer_value())
        } else {
            self.gen_lvalue_ptr(obj, function, scopes, func_values, functions, structs)
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
                let ot = type_of(obj, scopes, functions, structs)?;
                let base = self.field_base_ptr(obj, &ot, function, scopes, func_values, functions, structs)?;
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
            Expr::Interp(parts) => {
                self.gen_interp(parts, function, scopes, func_values, functions, structs)
            }
            Expr::Slice(arr, from, to) => {
                let av = self.gen_expr(arr, function, scopes, func_values, functions, structs)?;
                let fv = self.gen_expr(from, function, scopes, func_values, functions, structs)?;
                let tv = self.gen_expr(to, function, scopes, func_values, functions, structs)?;
                let f = self.to_i64(fv, &type_of(from, scopes, functions, structs)?);
                let t = self.to_i64(tv, &type_of(to, scopes, functions, structs)?);
                Ok(self.call_c("dial_array_slice", &[av.into(), f.into(), t.into()]))
            }
            Expr::StringLit(s) => {
                let name = format!("dialc.str.{}", self.next_str_id());
                let g = self.global_string(&name, s);
                Ok(self.call_c("dial_string_new", &[g.into()]))
            }
            Expr::Var(name) => {
                let found = scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(name).map(|v| v.clone()));
                match found {
                    Some((ty, (_m, ptr))) => {
                        let llvm_ty = self.dialc_type_to_llvm(&ty);
                        Ok(self.builder.build_load(llvm_ty, ptr, name).unwrap())
                    }
                    None => {
                        // v1.03：具名函数在右值位置求值为**纯函数地址**。
                        // 没有环境对象、没有分配、没有 ARC，只是把函数符号的地址取出来。
                        let fv = func_values.get(name).ok_or_else(|| {
                            DialcError::new(format!("未声明的变量 '{}'", name), 0, 0)
                        })?;
                        Ok(fv.as_global_value().as_pointer_value().into())
                    }
                }
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
                let ot = type_of(obj, scopes, functions, structs)?;
                // v1.04 FFI：易失指针字段访问走 volatile 加载。
                let vol = matches!(ot, Type::VolatilePtr(_));
                // 基对象若是 lvalue（变量 / 字段链），直接取字段指针；
                // 否则（结构体字面量 / 函数返回的结构体 / 数组元素）先物化到栈上临时槽再取字段，
                // 支持 `Pt{}.x`、`make().y`、`arr[i].f` 而无需先绑到变量
                // （B7 内联 const 后，const 结构体取字段成为常态路径）。
                let base = match obj.as_ref() {
                    Expr::Var(_) | Expr::Field(..) => {
                        self.field_base_ptr(obj, &ot, function, scopes, func_values, functions, structs)?
                    }
                    _ => {
                        let v = self.gen_expr(obj, function, scopes, func_values, functions, structs)?;
                        let ptr = self.alloca_in_entry(function, v.get_type(), "fldtmp");
                        self.builder.build_store(ptr, v).unwrap();
                        ptr
                    }
                };
                let sty = self.get_struct_type(&ot)?;
                let (fidx, fty) = self.field_info(&ot, field, structs, 0, 0)?;
                let fgep = self.builder.build_struct_gep(sty, base, fidx, "fgep").unwrap();
                // v1.04 FFI：extern struct 字段按 C 内存宽度读取（bool/byte 均为 i8），
                // 再转回 Dial 内部宽度；值语义 struct 仍按内部宽度读取。
                let load_ty = if self.field_is_c_abi(&ot) {
                    self.cstruct_field_llvm(&fty)
                } else {
                    self.dialc_type_to_llvm(&fty)
                };
                // v1.04 FFI：易失指针字段访问走 volatile 加载——编译器不得优化掉这次访问。
                let raw = self.builder.build_load(load_ty, fgep, "fld").unwrap();
                if vol {
                    raw.as_instruction_value().unwrap().set_volatile(true).unwrap();
                }
                Ok(self.abi_adapt(raw, load_ty, self.dialc_type_to_llvm(&fty)))
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
                // 按元素类型把 i64 槽位模式还原：int/byte/char 原样，float bitcast，
                // bool i1↔i64，句柄 int_to_ptr（与 push/set 的 ptr_to_int 对称）。
                Ok(self.from_elem_bits(&elem_t, res.into_int_value()))
            }
            Expr::ArrayLit(elems, expect) => {
                let elem_ty = match expect {
                    Some(et) => (**et).clone(),
                    None => if elems.is_empty() {
                        Type::Int
                    } else {
                        type_of(&elems[0], scopes, functions, structs)?
                    },
                };
                let kind = self.array_elem_kind(&elem_ty);
                let mut cur = self.call_c(
                    "dial_array_new",
                    &[self.context.i64_type().const_int(kind, false).into()],
                );
                for el in elems {
                    let ev = self.gen_expr(el, function, scopes, func_values, functions, structs)?;
                    let ety = type_of(el, scopes, functions, structs)?;
                    let ei64 = self.to_elem_bits(function, ev, &ety);
                    cur = self.call_c("dial_array_push", &[cur.into(), ei64.into()]);
                    // push 会为元素 retain 一份（数组成为持有者）。字面量自带的那一份
                    // 所有权本应直接移交数组，故此处归还——否则每个字面量元素泄漏一份引用。
                    self.sink_elem_after_retain(el, &ety, ev, ei64);
                }
                Ok(cur)
            }
            Expr::StructLit(name, fields) => {
                let sty = self.struct_types[name];
                let sdef = structs[name].0.clone();
                let defaults = &structs[name].1;
                let ptr = self.alloca_in_entry(function, sty.into(), "slit");
                // 零初始化：所有字段先取零值（确定性语义，杜绝未初始化内存）
                let zero = sty.const_zero();
                self.builder.build_store(ptr, zero).unwrap();
                // 按结构体定义顺序处理每个字段：字面量提供的用提供值，省略且有默认值的用默认值。
                // 语义层已保证「省略且无默认值的字段」被拒绝，故这里不会出现未初始化字段。
                let provided: std::collections::HashMap<&str, &Expr> =
                    fields.iter().map(|(f, e)| (f.as_str(), e)).collect();
                for (fidx, (fname, fty)) in sdef.iter().enumerate() {
                    let fe: &Expr = match provided.get(fname.as_str()) {
                        Some(e) => *e,
                        None => match defaults.get(fname) {
                            Some(d) => d,
                            // 防御：语义层应已拒绝；仍零初始化避免未初始化
                            None => continue,
                        },
                    };
                    let fidx_gep = fidx as u32;
                    let gep = self
                        .builder
                        .build_struct_gep(sty, ptr, fidx_gep, "fgep")
                        .unwrap();
                    let fv = self.gen_expr(fe, function, scopes, func_values, functions, structs)?;
                    let stored = self.coerce(fv, &type_of(fe, scopes, functions, structs)?, fty);
                    // 堆类型字段：结构体成为该对象的一个持有者。
                    // 借来的值（来自变量 / 字段 / 数组元素）必须 retain 一份——
                    // 否则字段只是来源的别名，来源出作用域被 release 后字段即悬垂
                    // （use-after-free：读到的长度字段是垃圾，会打印出成片乱码）。
                    // 自带所有权的值（字面量等）直接接管，不得再 retain。
                    self.retain_if_borrowed(fe, fty, stored);
                    self.builder.build_store(gep, stored).unwrap();
                }
                Ok(self.builder.build_load(sty, ptr, "sval").unwrap())
            }
            // 类型字面量只出现在 ptr_cast<T>(x) 的实参里，由该内建单独处理。
            Expr::TypeLit(_) => Err(DialcError::new(
                "内部错误：类型字面量不应出现在此位置（应由 Sema 先行拦截）",
                0,
                0,
            )),
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
                UnOp::BitNot => {
                    let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                    // ~x 即 x 与全 1 异或（等价于按位取反）
                    let all_ones = self.context.i64_type().const_all_ones();
                    Ok(self
                        .builder
                        .build_xor(v.into_int_value(), all_ones, "bnot")
                        .unwrap()
                        .into())
                }
                // 解引用 `*p`（v1.02）：按元素类型从指针指向的地址**加载**一个值。
                UnOp::Deref => {
                    let pt = type_of(e, scopes, functions, structs)?;
                    let (elem, vol) = match pt {
                        Type::RawPtr(el) => (*el, false),
                        Type::VolatilePtr(el) => (*el, true),
                        other => {
                            return Err(DialcError::new(
                                format!("内部错误：解引用收到 {} 类型（应由 Sema 先行拦截）", other.name()),
                                0,
                                0,
                            ))
                        }
                    };
                    let p = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                    // 按元素的**内存宽度**加载（byte 是 1 字节、char 是 4 字节……），
                    // 再还原成 Dial 的值表示。
                    let llvm_elem = self.raw_elem_llvm(&elem);
                    // v1.04 FFI：易失指针经 volatile 加载——编译器不得优化掉这次访问。
                    // v1.04 FFI：易失指针经 volatile 加载——编译器不得优化掉这次访问。
                    let v = self
                        .builder
                        .build_load(llvm_elem, p.into_pointer_value(), "deref")
                        .unwrap();
                    if vol {
                        v.as_instruction_value().unwrap().set_volatile(true).unwrap();
                    }
                    Ok(self.raw_from_mem(&elem, v))
                }
                // 取地址 `&x`（v1.02）：x 是栈上值类型变量，它**本来就在 alloca 槽里**，
                // 因此「地址」就是那个槽位的指针（无需另做分配）。
                UnOp::AddrOf => {
                    let name = match &**e {
                        Expr::Var(n) => n.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "内部错误：取地址的对象必须是变量（应由 Sema 先行拦截）",
                                0,
                                0,
                            ))
                        }
                    };
                    let slot = Self::lookup_slot(scopes, &name).ok_or_else(|| {
                        DialcError::new(
                            format!("内部错误：取地址找不到变量 '{}' 的槽位", name),
                            0,
                            0,
                        )
                    })?;
                    // 槽位是「元素类型」的指针，转成通用指针（i8*）对外暴露。
                    let p = self
                        .builder
                        .build_pointer_cast(slot, self.i8ptr(), "addr.of")
                        .unwrap();
                    Ok(p.into())
                }
            },
            // 三元表达式 `cond ? a : b`（v1.00）：两个分支类型相同（语义层已保证），
            // 结果写入 entry 块的栈槽，两条分支各自 store，merge 处 load。
            Expr::Ternary(cond, then_e, else_e) => {
                let ct = type_of(cond, scopes, functions, structs)?;
                let vt = type_of(then_e, scopes, functions, structs)?;
                let cond_val = self.gen_expr(cond, function, scopes, func_values, functions, structs)?;
                let cond_bit = self.to_i64(cond_val, &ct);
                let res_ty = self.dialc_type_to_llvm(&vt);
                let slot = self.alloca_in_entry(function, res_ty, "tern.res");
                let then_bb = self.context.append_basic_block(function, "tern.then");
                let else_bb = self.context.append_basic_block(function, "tern.else");
                let merge_bb = self.context.append_basic_block(function, "tern.merge");
                let zero = self.context.i64_type().const_int(0, false);
                let is_true = self
                    .builder
                    .build_int_compare(IntPredicate::NE, cond_bit, zero, "tern.cond")
                    .unwrap();
                self.builder
                    .build_conditional_branch(is_true, then_bb, else_bb)
                    .unwrap();
                self.builder.position_at_end(then_bb);
                let tv = self.gen_expr(then_e, function, scopes, func_values, functions, structs)?;
                self.builder.build_store(slot, tv).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(else_bb);
                let ev = self.gen_expr(else_e, function, scopes, func_values, functions, structs)?;
                self.builder.build_store(slot, ev).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(self.builder.build_load(res_ty, slot, "tern.val").unwrap())
            }
            Expr::Method(obj, method, args) => self.gen_method_call(obj, method, args, function, scopes, func_values, functions, structs),
            Expr::Binary(left, op, right) => self.gen_binary(*op, left, right, function, scopes, func_values, functions, structs),
            Expr::Call(name, args) => {
                // v1.04 FFI：编译期类型内建 sizeof / alignof / offsetof。
                // 数值按 target DataLayout 在编译期算出，发射为 i64 常量（零运行时开销）。
                if matches!(name.as_str(), "sizeof" | "alignof" | "offsetof") {
                    let ty = match &args[0] {
                        Expr::TypeLit(t) => t.clone(),
                        _ => {
                            return Err(DialcError::new(
                                "语义错误：sizeof/alignof/offsetof 的第一个参数必须是类型实参",
                                0, 0,
                            ))
                        }
                    };
                    let dl = self.module.get_data_layout();
                    let td = TargetData::create(
                        dl.as_str().to_str().unwrap_or(""),
                    );
                    let lt = self.dialc_type_to_llvm(&ty);
                    let val: u64 = match name.as_str() {
                        "sizeof" => td.get_abi_size(&lt),
                        "alignof" => td.get_abi_alignment(&lt) as u64,
                        "offsetof" => {
                            let field = match &args[1] {
                                Expr::StringLit(f) => f.clone(),
                                _ => {
                                    return Err(DialcError::new(
                                        "语义错误：offsetof 的第二个参数必须是字段名",
                                        0, 0,
                                    ))
                                }
                            };
                            let sname = match &ty {
                                Type::Struct(n) => n.clone(),
                                _ => {
                                    return Err(DialcError::new(
                                        "语义错误：offsetof 的第一个参数必须是结构体类型",
                                        0, 0,
                                    ))
                                }
                            };
                            let idx = match structs.get(&sname) {
                                Some((fields, _)) => {
                                    fields.iter().position(|(fnm, _)| fnm == &field)
                                }
                                None => None,
                            };
                            let idx = match idx {
                                Some(i) => i as u32,
                                None => {
                                    return Err(DialcError::new(
                                        format!(
                                            "语义错误：offsetof 的结构体 '{}' 没有字段 '{}'",
                                            sname, field
                                        ),
                                        0, 0,
                                    ))
                                }
                            };
                            let sty = self
                                .struct_types
                                .get(&sname)
                                .expect("offsetof 的结构体应在 build_struct_types 中注册");
                            td.offset_of_element(sty, idx)
                                .expect("offsetof 的字段索引应在结构体字段范围内")
                        }
                        _ => unreachable!(),
                    };
                    return Ok(self.context.i64_type().const_int(val, false).into());
                }
                // v1.00 内建：断言 / 退出 / 数学函数（独立于下面的类型转换与 print 分派）。
                if matches!(
                    name.as_str(),
                    "assert" | "panic" | "exit" | "min" | "max" | "abs"
                        | "sqrt" | "pow" | "floor" | "ceil" | "round"
                        | "args" | "now" | "random" | "sleep"
                ) {
                    return self.gen_builtin_v1(
                        name, args, function, scopes, func_values, functions, structs,
                    );
                }
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
                    // v1.04 FFI：C 侧缓冲区 → Dial 对象（复制出新对象，受 ARC 管理）。
                    // 与 as_c_str() / as_ptr() 的「借用」正好反向，构成完整数据总线。
                    "string_from_cstr" => {
                        let p = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_ffi_string_from_cstr", &[p.into()]))
                    }
                    "bytes_from_ptr" => {
                        let p = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let n = self.to_i64(
                            self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?,
                            &type_of(&args[1], scopes, functions, structs)?,
                        );
                        Ok(self.call_c("dial_ffi_bytes_from_ptr", &[p.into(), n.into()]))
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
                    // set 与 map 共用同一份 C 运行时：成员即键（value 恒为 1），零 C 改动。
                    "new_set" => Ok(self.call_c("dial_map_new", &[])),
                    // args 是 parse_var_decl 补进的两个编译期 kind 常量。
                    "new_kv_map" => {
                        let kk = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let vk = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_kvmap_new", &[kk.into(), vk.into()]))
                    }
                    "read_line" => Ok(self.call_c("dial_read_line", &[])),
                    // ---- v1.04 第 2 批 C2：严格数值解析 / 显式探测 ----
                    "parse_int" | "is_int" | "parse_float" | "is_float" => {
                        let s = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let cname = match name.as_str() {
                            "parse_int" => "dial_parse_int",
                            "is_int" => "dial_is_int",
                            "parse_float" => "dial_parse_float",
                            _ => "dial_is_float",
                        };
                        let r = self.call_c(cname, &[s.into()]);
                        // is_* / has_* 等返回 C 的 0/1，需压成 Dial 内部 i1（bool）。
                        if matches!(name.as_str(), "is_int" | "is_float") {
                            let zero = self.context.i64_type().const_int(0, false);
                            Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), zero, "isbool").unwrap().into())
                        } else {
                            Ok(r)
                        }
                    }
                    // ---- v1.04 第 2 批 C3：类型安全定点格式化 ----
                    "format_float" => {
                        let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let d = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &Type::Int);
                        Ok(self.call_c("dial_format_float", &[v.into(), d.into()]))
                    }
                    // ---- v1.04 第 2 批 C7：文件 API ----
                    // read_all / file_read_line 返回 NULL = 业务失败，抛 Error（与既有 read 同款）。
                    "read_all" | "file_read_line" => {
                        let f = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let cname = if name == "read_all" { "dial_file_read_all" } else { "dial_file_read_line" };
                        let s = self.call_c(cname, &[f.into()]);
                        let s_int = self.builder.build_ptr_to_int(s.into_pointer_value(), self.context.i64_type(), "sint").unwrap();
                        let is_null = self.builder.build_int_compare(IntPredicate::EQ, s_int, self.context.i64_type().const_int(0, false), "filenull").unwrap();
                        let ok_bb = self.context.append_basic_block(function, "fileread.ok");
                        let fail_bb = self.context.append_basic_block(function, "fileread.fail");
                        self.builder.build_conditional_branch(is_null, fail_bb, ok_bb).unwrap();
                        self.builder.position_at_end(fail_bb);
                        let g = self.global_string("dialc.err.fileread", "读取文件失败（句柄无效或内存不足）");
                        let msg = self.call_c("dial_string_new", &[g.into()]);
                        let err = self.call_c("dial_error_new", &[msg.into()]);
                        self.emit_throw(err, scopes, structs)?;
                        self.builder.position_at_end(ok_bb);
                        Ok(s)
                    }
                    "eof" => {
                        let f = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let r = self.call_c("dial_file_eof", &[f.into()]);
                        let zero = self.context.i64_type().const_int(0, false);
                        Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), zero, "eofbool").unwrap().into())
                    }
                    "file_exists" | "file_remove" | "file_size" | "has_env" => {
                        let a = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        let cname = match name.as_str() {
                            "file_exists" => "dial_file_exists",
                            "file_remove" => "dial_file_remove",
                            "file_size" => "dial_file_size",
                            _ => "dial_has_env",
                        };
                        let r = self.call_c(cname, &[a.into()]);
                        if matches!(name.as_str(), "file_exists" | "file_remove" | "has_env") {
                            let zero = self.context.i64_type().const_int(0, false);
                            Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), zero, "predbool").unwrap().into())
                        } else {
                            Ok(r)
                        }
                    }
                    // ---- v1.04 第 2 批 C9/C10：环境 / 时间（无可变全局状态）----
                    "getenv" => {
                        let a = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_getenv", &[a.into()]))
                    }
                    "program_name" => Ok(self.call_c("dial_program_name", &[])),
                    "cwd" => Ok(self.call_c("dial_cwd", &[])),
                    "date_string" => Ok(self.call_c("dial_date_string", &[])),
                    "local_parts" => Ok(self.call_c("dial_local_parts", &[])),
                    "monotonic_ms" => Ok(self.call_c("dial_monotonic_ms", &[])),
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
                            Type::File => {
                                self.call_c_void("dial_file_close", &[v.into()]);
                                // 显式关闭即「消费」该句柄：标记 moved，使 with 块末的
                                // 自动释放跳过它——fclose 二次调用是 UB，绝不能发生。
                                // （channel 不标：它受 ARC 管理，标记 moved 会跳过其 release 而泄漏。）
                                if let Expr::Var(n) = &args[0] {
                                    self.mark_moved(scopes, n);
                                }
                            }
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
                    // __with_release(v)：with 块末的自动释放（**内部调用**，用户代码无法触发）。
                    //   - 若该变量已在块内被显式 close / join 过（moved），则跳过，避免二次释放；
                    //   - 否则按类型释放：file → close、thread → join（裸指针见 v1.02 裸指针一节）。
                    "__with_release" => {
                        let vname = match &args[0] {
                            Expr::Var(n) => n.clone(),
                            _ => {
                                return Err(DialcError::new(
                                    "内部错误：__with_release 的参数必须是变量名",
                                    0,
                                    0,
                                ))
                            }
                        };
                        let info = scopes
                            .iter()
                            .rev()
                            .find_map(|s| s.get(&vname).map(|v| v.clone()))
                            .ok_or_else(|| {
                                DialcError::new(
                                    format!("内部错误：__with_release 找不到变量 '{}'", vname),
                                    0,
                                    0,
                                )
                            })?;
                        let (ty, (_m, ptr)) = info;
                        if self.moved.borrow().contains(&ptr) {
                            return Ok(self.context.i64_type().const_int(0, false).into());
                        }
                        let v = self.builder.build_load(self.i8ptr(), ptr, "withrel").unwrap();
                        match ty {
                            Type::File => self.call_c_void("dial_file_close", &[v.into()]),
                            Type::Thread => self.call_c_void("dial_thread_join", &[v.into()]),
                            // 裸指针：with 块末自动 free（谁申请、谁释放）。
                            Type::RawPtr(_) | Type::VolatilePtr(_) => self.call_c_void("dial_raw_free", &[v.into()]),
                            _ => {}
                        }
                        self.moved.borrow_mut().insert(ptr);
                        Ok(self.context.i64_type().const_int(0, false).into())
                    }
                    // 可恢复异常（v0.7）：Error("msg") 构造一个带引用计数的错误对象（ARC 类型，见 §26）。
                    // 严禁裸的 int/string —— 该约束由 Sema 在编译期拦截；此处 msg 必为 string。
                    "Error" => {
                        let msg =
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        Ok(self.call_c("dial_error_new", &[msg.into()]))
                    }
                    // ---- v1.02 裸指针内建 ----
                    // raw_alloc(n)：申请 n 字节裸内存。**不受 ARC 管**：编译器不 retain、
                    // 不在作用域结束时释放——必须由调用方显式 raw_free / drop 归还。
                    "raw_alloc" => {
                        let n = self.to_i64(
                            self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?,
                            &Type::Int,
                        );
                        Ok(self.call_c("dial_raw_alloc", &[n.into()]))
                    }
                    // raw_free(p)：归还裸指针（NULL 安全）。
                    "raw_free" => {
                        let p = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                        self.call_c_void("dial_raw_free", &[p.into()]);
                        Ok(self.context.i64_type().const_int(0, false).into())
                    }
                    // ptr_cast<T>(x)：显式的带类型指针转换。
                    //   - 源是裸指针 → 位模式转换（地址不变）；
                    //   - 源是 string / bytes → 借用其**数据缓冲区**（跳过 8 字节头部）。
                    // 借用得到的指针**不受 ARC 保护**：原对象被释放后它即悬垂，
                    // 这是使用者的责任（README §19.3）。
                    "ptr_cast" => {
                        // 目标类型由语义层校验；codegen 只需确认类型实参确实存在——
                        // 对外一律是 i8*，目标裸指针类型不改变 IR。
                        if !matches!(&args[0], Expr::TypeLit(_)) {
                            return Err(DialcError::new(
                                "内部错误：ptr_cast 缺少类型实参（应由 Sema 先行拦截）",
                                0,
                                0,
                            ));
                        }
                        let src_ty = type_of(&args[1], scopes, functions, structs)?;
                        let src =
                            self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                        match src_ty {
                            Type::RawPtr(_) | Type::VolatilePtr(_) => {
                                let p = self
                                    .builder
                                    .build_pointer_cast(src.into_pointer_value(), self.i8ptr(), "cast")
                                    .unwrap();
                                Ok(p.into())
                            }
                            Type::String | Type::Bytes => {
                                // payload 布局：[ int64 len ][ data... ]，数据从 +8 开始。
                                let off = self.context.i8_type().const_int(8, false);
                                let data = unsafe {
                                    self.builder
                                        .build_gep(self.context.i8_type(), src.into_pointer_value(), &[off], "payload")
                                        .unwrap()
                                };
                                let p = self
                                    .builder
                                    .build_pointer_cast(data, self.i8ptr(), "cast")
                                    .unwrap();
                                Ok(p.into())
                            }
                            other => Err(DialcError::new(
                                format!(
                                    "内部错误：ptr_cast 收到 {} 类型的源（应由 Sema 先行拦截）",
                                    other.name()
                                ),
                                0,
                                0,
                            )),
                        }
                    }
                    // bitcast<T>(x)：底层内存强转（v1.04 FFI）。把 `x` 的位模式按目标类型
                    // `T` 重解释——LLVM `bitcast`，不读不改内存。源与目标必须同位宽，
                    // 否则是 zext/trunc/inttoptr 的活，这里用 target DataLayout 钉死。
                    "bitcast" => {
                        let target = match &args[0] {
                            Expr::TypeLit(ty) => ty.clone(),
                            _ => {
                                return Err(DialcError::new(
                                    "内部错误：bitcast 缺少类型实参（应由 Sema 先行拦截）",
                                    0, 0,
                                ))
                            }
                        };
                        let src = self.gen_expr(
                            &args[1], function, scopes, func_values, functions, structs,
                        )?;
                        let src_dial_ty = type_of(&args[1], scopes, functions, structs)?;
                        let src_ty_name = src_dial_ty.name();
                        let src_ty = src.get_type();
                        let dst_ty = self.dialc_type_to_llvm(&target);
                        // 同位宽校验：bitcast 不改变位宽（C 的 reinterpret_cast 同理）。
                        let src_bits = self.llvm_bit_width(&src_ty);
                        let dst_bits = self.llvm_bit_width(&dst_ty);
                        if src_bits != dst_bits {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：bitcast 源（{} 位）与目标 '{}'（{} 位）位宽不一致，\
                                     无法做位模式重解释——请改用显式转换或调整类型宽度",
                                    src_bits, target.name(), dst_bits
                                ),
                                0, 0,
                            ));
                        }
                        // 同类型重解释 = 恒等（LLVM 的 bitcast 不允许同类型自转）。
                        if src_ty == dst_ty {
                            return Ok(src);
                        }
                        // 指针 ↔ 整数在 LLVM 里**不是** bitcast（bitcast 只能同「指针类」或
                        // 同「数值类」之间转），必须走 ptrtoint / inttoptr。
                        // 这正是「把地址当整数」「把整数当地址」这层内存强转的正解。
                        let src_is_ptr = matches!(src_ty, BasicTypeEnum::PointerType(_));
                        let src_is_int = matches!(src_ty, BasicTypeEnum::IntType(_));
                        let dst_is_ptr = matches!(dst_ty, BasicTypeEnum::PointerType(_));
                        let dst_is_int = matches!(dst_ty, BasicTypeEnum::IntType(_));
                        // 指针与非指针之间不能 bitcast（LLVM 的硬规则）。
                        // 聚合类型（结构体 / 数组）只能与同尺寸的聚合类型互转。
                        let src_is_agg = !src_is_ptr && !src_is_int && !matches!(src_ty, BasicTypeEnum::FloatType(_));
                        let dst_is_agg = !dst_is_ptr && !dst_is_int && !matches!(dst_ty, BasicTypeEnum::FloatType(_));
                        if (src_is_ptr && dst_is_agg) || (src_is_agg && dst_is_ptr) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：bitcast 不能在指针与聚合类型之间转换（源 {} → 目标 '{}'）；\
                                     请先用 rawptr 指向那块内存，再按字段读取",
                                    src_ty_name, target.name()
                                ),
                                0, 0,
                            ));
                        }
                        if (src_is_int && dst_is_agg) || (src_is_agg && dst_is_int) {
                            return Err(DialcError::new(
                                format!(
                                    "语义错误：bitcast 不能在整数与聚合类型之间转换（源 {} → 目标 '{}'）；\
                                     请先用 rawptr 指向那块内存，再按字段读取",
                                    src_ty_name, target.name()
                                ),
                                0, 0,
                            ));
                        }
                        let v = if src_is_ptr && dst_is_int {
                            let it = match dst_ty {
                                BasicTypeEnum::IntType(t) => t,
                                _ => unreachable!(),
                            };
                            self.builder
                                .build_ptr_to_int(src.into_pointer_value(), it, "bitcast")
                                .unwrap()
                                .into()
                        } else if src_is_int && dst_is_ptr {
                            let pt = match dst_ty {
                                BasicTypeEnum::PointerType(t) => t,
                                _ => unreachable!(),
                            };
                            self.builder
                                .build_int_to_ptr(src.into_int_value(), pt, "bitcast")
                                .unwrap()
                                .into()
                        } else {
                            self.builder.build_bit_cast(src, dst_ty, "bitcast").unwrap()
                        };
                        Ok(v)
                    }
                    // drop(x)：提前释放一个变量的所有权（move 语义）。
                    //   - file 走 dial_file_close（fclose）、thread 走 dial_thread_join；
                    //   - 其余 ARC 句柄（string / array / map / set / kvmap / error / channel）
                    //     统一走 self.release（按类型分派 *_release）；
                    //   - 标量（int / float / bool / byte / char）无所有权，no-op。
                    // 释放后该槽位标记 moved（codegen 层），防止后续重复释放或 use-after-free。
                    "drop" => {
                        let vname = match &args[0] {
                            Expr::Var(n) => n.clone(),
                            _ => {
                                return Err(DialcError::new(
                                    "语义错误：drop() 的参数必须是变量名",
                                    0, 0,
                                ))
                            }
                        };
                        let info = scopes
                            .iter()
                            .rev()
                            .find_map(|s| s.get(&vname).map(|v| v.clone()))
                            .ok_or_else(|| {
                                DialcError::new(format!("未声明的变量 '{}'", vname), 0, 0)
                            })?;
                        let (ty, (_m, ptr)) = info;
                        match ty {
                            Type::File => {
                                let v =
                                    self.builder.build_load(self.i8ptr(), ptr, "dropval").unwrap();
                                self.call_c_void("dial_file_close", &[v.into()]);
                            }
                            Type::Thread => {
                                let v =
                                    self.builder.build_load(self.i8ptr(), ptr, "dropval").unwrap();
                                self.call_c_void("dial_thread_join", &[v.into()]);
                            }
                            // 裸指针：drop 的唯一作用就是 free（它不受 ARC 管理，
                            // 编译器不会替它做 release——「谁申请、谁释放」）。
                            Type::RawPtr(_) | Type::VolatilePtr(_) => {
                                let v =
                                    self.builder.build_load(self.i8ptr(), ptr, "dropval").unwrap();
                                self.call_c_void("dial_raw_free", &[v.into()]);
                            }
                            _ => self.release(&ty, ptr),
                        }
                        self.moved.borrow_mut().insert(ptr);
                        Ok(self.context.i64_type().const_int(0, false).into()) // Void 占位
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
                            // join 即「消费」该线程句柄：标记 moved，使 with 块末的
                            // 自动释放跳过它（二次 join 会二次回收线程栈，绝不能发生）。
                            if name == "join" {
                                self.mark_moved(scopes, n);
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
                                Type::String | Type::Array(_) | Type::Map | Type::Set(_) | Type::KvMap(..) | Type::Error => {
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
                        if name == "print" || name == "eprint" {
                            // variadic（v1.00）：中间实参按原样紧接输出（不换行），
                            // 只有最后一个补 '\n'。单实参时行为与旧版完全一致。
                            // v1.04 C8：eprint 走独立的 emit_eprint / emit_eprint_raw，
                            // 与 stdout 版各自直白实现（§21.1 宁可 WET，绝不合并成带标志的通用函数）。
                            let to_stderr = name == "eprint";
                            let last = args.len() - 1;
                            for (i, arg) in args.iter().enumerate() {
                                let ty = type_of(arg, scopes, functions, structs)?;
                                let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                                let newline = i == last;
                                match (to_stderr, newline) {
                                    (false, true) => self.emit_print(&ty, val)?,
                                    (false, false) => self.emit_print_raw(&ty, val)?,
                                    (true, true) => self.emit_eprint(&ty, val)?,
                                    (true, false) => self.emit_eprint_raw(&ty, val)?,
                                }
                            }
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
                        } else if name == "print_raw" || name == "eprint_raw" {
                            // 原样输出、**不换行**（v0.98.2）；v1.00 起同样支持 variadic。
                            // v1.04 C8：eprint_raw 是 stderr 版本，走独立分派函数。
                            let to_stderr = name == "eprint_raw";
                            for arg in args {
                                let ty = type_of(arg, scopes, functions, structs)?;
                                let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                                if to_stderr {
                                    self.emit_eprint_raw(&ty, val)?;
                                } else {
                                    self.emit_print_raw(&ty, val)?;
                                }
                            }
                            Ok(self.context.i64_type().const_int(0, false).into())
                        } else if name == "write_char" {
                            // 输出单个字符、**不换行**（v0.98.2）。
                            // 语义层已保证实参是 char，这里不再重复校验类型。
                            let arg = &args[0];
                            let val = self.gen_expr(arg, function, scopes, func_values, functions, structs)?;
                            self.call_c_void("dial_write_char", &[val.into()]);
                            Ok(self.context.i64_type().const_int(0, false).into())
                        } else {
                            // v1.03：两条调用路径
                            //   A. 具名函数（含 C 变参 extern）→ 直接调用；
                            //   B. 函数指针变量 → 精确签名的间接调用（无捕获、无 thunk）。
                            let info = functions.get(name);
                            let (sig_params, sig_ret, is_c_varargs): (Vec<Type>, Type, bool);
                            let direct_fv: Option<FunctionValue<'ctx>>;
                            let indirect_ptr: Option<PointerValue<'ctx>>;
                            if let Some(inf) = info {
                                sig_params = inf.params.clone();
                                sig_ret = inf.ret.clone();
                                is_c_varargs = inf.variadic;
                                direct_fv = Some(*func_values.get(name).ok_or_else(|| {
                                    DialcError::new(
                                        format!("缺少函数 '{}' 的 LLVM 定义", name),
                                        0, 0,
                                    )
                                })?);
                                indirect_ptr = None;
                            } else {
                                let (ty, (_m, ptr)) = scopes
                                    .iter()
                                    .rev()
                                    .find_map(|s| s.get(name).map(|v| v.clone()))
                                    .ok_or_else(|| {
                                        DialcError::new(
                                            format!("调用了未定义的函数 '{}'", name),
                                            0, 0,
                                        )
                                    })?;
                                match ty {
                                    Type::FnPtr(ps, ret) => {
                                        sig_params = ps;
                                        sig_ret = *ret;
                                    }
                                    _ => {
                                        return Err(DialcError::new(
                                            format!("'{}' 不是函数或函数指针，不能调用", name),
                                            0, 0,
                                        ))
                                    }
                                }
                                is_c_varargs = false;
                                direct_fv = None;
                                indirect_ptr = Some(
                                    self.builder
                                        .build_load(self.i8ptr(), ptr, "fnptr")
                                        .unwrap()
                                        .into_pointer_value(),
                                );
                            }
                            let mut arg_vals: Vec<BasicValueEnum<'ctx>> = Vec::new();
                            // 变参脱糖生成的临时数组：调用结束后要归还调用方那一份所有权。
                            let mut temp_arrays: Vec<(BasicValueEnum<'ctx>, Type)> = Vec::new();
                            for (i, a) in args.iter().enumerate() {
                                let mut v = self.gen_expr(
                                    a, function, scopes, func_values, functions, structs,
                                )?;
                                if is_c_varargs && i >= sig_params.len() {
                                    // C 默认实参提升：bool 的 IR 是 i1，必须扩成 i64 再进
                                    // varargs（否则 C 侧读到的是未定义的高位）；char/byte/int
                                    // 本来就是 i64，float 本来就是 f64(== double)，指针原样传。
                                    if type_of(a, scopes, functions, structs)? == Type::Bool {
                                        v = self
                                            .builder
                                            .build_int_z_extend(
                                                v.into_int_value(),
                                                self.context.i64_type(),
                                                "va.zext",
                                            )
                                            .unwrap()
                                            .into();
                                    }
                                }
                                if let Expr::ArrayLit(_, Some(et)) = a {
                                    // `expect=Some` 是脱糖标记的临时数组（源码手写为 None）。
                                    temp_arrays.push((v, Type::Array(Box::new((**et).clone()))));
                                }
                                arg_vals.push(v);
                            }
                            // 默认实参（v1.00）：在调用点按声明顺序补齐缺失的尾部参数，
                            // 每次调用求值一次（与「默认值是常量表达式」的约束一致）。
                            if let Some(inf) = info {
                                if !inf.variadic {
                                    for (i, d) in inf.defaults.iter().enumerate() {
                                        if i >= arg_vals.len() {
                                            if let Some(e) = d {
                                                arg_vals.push(self.gen_expr(
                                                    e, function, scopes, func_values, functions,
                                                    structs,
                                                )?);
                                            }
                                        }
                                    }
                                }
                            }
                            // v1.04 FFI：所有**固定形参**都从 Dial 内部宽度适配到
                            // C ABI 函数签名宽度（bool i1→i32、byte i64→i8）。
                            // 函数声明、函数指针签名、Dial 函数定义三处都已统一为 ffi 宽度，
                            // 调用点必须做对称转换，否则 IR 类型不符且 bool/byte 会静默错位。
                            // C 变参尾部已在上面按「C 默认实参提升」处理，此处不再二次转换。
                            let adapted: Vec<BasicValueEnum<'ctx>> = arg_vals
                                .iter()
                                .enumerate()
                                .map(|(i, v)| {
                                    if is_c_varargs && i >= sig_params.len() {
                                        return *v;
                                    }
                                    match sig_params.get(i) {
                                        Some(pt) => self.abi_adapt(
                                            *v,
                                            v.get_type(),
                                            self.ffi_type_to_llvm(pt),
                                        ),
                                        None => *v,
                                    }
                                })
                                .collect();
                            let metadata: Vec<BasicMetadataValueEnum> =
                                adapted.iter().map(|v| (*v).into()).collect();
                            let call = if let Some(fv) = direct_fv {
                                self.builder.build_call(fv, &metadata, name).unwrap()
                            } else {
                                let ft = self.llvm_fn_type(&sig_params, &sig_ret);
                                self.builder
                                    .build_indirect_call(
                                        ft,
                                        indirect_ptr.unwrap(),
                                        &metadata,
                                        name,
                                    )
                                    .unwrap()
                            };
                            // 变参数组是普通 ARC 数组：函数形参进入时已 retain 自己那一份，
                            // 这里释放的是**调用方**那一份临时数组的所有权——
                            // 两侧对称，既不双重释放也不泄漏（用户明确要求自动插入 release）。
                            for (v, ty) in temp_arrays {
                                self.release_value(&ty, v);
                            }
                            // void 调用（v0.92）：`close(f);` / `demo_free(h);` 这类
                            // 「只做事、不给值」的调用是包生态里最常见的形态。
                            // 这里发射调用后返回一个占位值——**它只可能出现在语句位置**，
                            // 因为「把 void 当值用」已由语义层用 types_compatible 挡住。
                            let rv = match call.try_as_basic_value().basic() {
                                // v1.04 FFI：C ABI 返回宽度 → Dial 内部宽度（i32→i1 / i8→i64）。
                                Some(v) if sig_ret != Type::Void => self.abi_adapt(
                                    v,
                                    v.get_type(),
                                    self.dialc_type_to_llvm(&sig_ret),
                                ),
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
        // 泛型 map<K,V> 走独立分派：键/值需按静态类型转成 8 字节位模式，
        // 与老 int→int map 的纯 i64 接口不同，故不混进下面的 match。
        if let Type::KvMap(k, v) = &ot {
            return self.gen_kvmap_method(
                optr, k.as_ref(), v.as_ref(), method, args,
                function, scopes, func_values, functions, structs,
            );
        }
        match (&ot, method) {
            // v1.04 FFI：借用 C 侧数据区指针。跳过 8 字节长度头由 C Runtime 代劳，
            // 编译器不在 IR 里硬编码布局，用户更不需要手写 +8 offset hack。
            // 返回的是 rawptr<byte> 借用：不受 ARC 管理，生命周期绑定原对象。
            (Type::String, "as_c_str") => Ok(self.call_c("dial_ffi_string_cstr", &[optr.into()])),
            (Type::Bytes, "as_ptr") => Ok(self.call_c("dial_ffi_bytes_data", &[optr.into()])),
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
            (Type::String, "index_of") => {
                let sub = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                Ok(self.call_c("dial_string_index_of", &[optr.into(), sub.into()]))
            }
            (Type::String, "replace") => {
                let from = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                let to = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                Ok(self.call_c("dial_string_replace", &[optr.into(), from.into(), to.into()]))
            }
            (Type::String, "trim") => Ok(self.call_c("dial_string_trim", &[optr.into()])),
            (Type::String, "split") => {
                let sep = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                Ok(self.call_c("dial_string_split", &[optr.into(), sep.into()]))
            }
            // ---- v1.04 第 2 批 C1：字符串查询 / 规范化 ----
            (Type::String, "to_upper") => Ok(self.call_c("dial_string_to_upper", &[optr.into()])),
            (Type::String, "to_lower") => Ok(self.call_c("dial_string_to_lower", &[optr.into()])),
            (Type::String, "contains" | "starts_with" | "ends_with") => {
                let a = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                let cname = match method {
                    "contains" => "dial_string_contains",
                    "starts_with" => "dial_string_starts_with",
                    _ => "dial_string_ends_with",
                };
                let r = self.call_c(cname, &[optr.into(), a.into()]);
                let zero = i64t.const_int(0, false);
                Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), zero, "strpred").unwrap().into())
            }
            // ---- v1.04 第 2 批 C4：UTF-8 码点视图（字节语义方法一字未动）----
            (Type::String, "char_count") => Ok(self.call_c("dial_string_char_count", &[optr.into()])),
            (Type::String, "chars") => Ok(self.call_c("dial_string_chars", &[optr.into()])),
            (Type::String, "char_at") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                Ok(self.call_c("dial_string_char_at", &[optr.into(), i.into()]))
            }
            (Type::String, "char_slice") => {
                let from = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                let to = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &Type::Int);
                Ok(self.call_c("dial_string_char_slice", &[optr.into(), from.into(), to.into()]))
            }
            // 裸指针的指针算术（v1.02）：p.offset(n) —— **显式、带类型**。
            // 绝不允许 C 式「void* + 1」的无类型偏移：元素大小必须由类型决定。
            // rawptr<void> 无元素大小，语义层已直接拒绝（须先 ptr_cast 成具体类型）。
            (Type::RawPtr(elem), "offset") => {
                if args.len() != 1 {
                    return Err(DialcError::new(
                        "内部错误：offset 需要 1 个 int 参数（应由 Sema 先行拦截）",
                        0,
                        0,
                    ));
                }
                let n = self.to_i64(
                    self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?,
                    &Type::Int,
                );
                let llvm_elem = self.raw_elem_llvm(elem);
                // 索引直接用 i64 的 n（LLVM 的 gep 索引可以是 i64）：
                // 负偏移靠补码自然生效，且支持**运行期**偏移量（不必是常量）。
                let idx = n;
                let p = unsafe {
                    self.builder
                        .build_gep(llvm_elem, optr.into_pointer_value(), &[idx], "offset")
                        .unwrap()
                };
                let p = self
                    .builder
                    .build_pointer_cast(p, self.i8ptr(), "offset")
                    .unwrap();
                Ok(p.into())
            }
            (Type::Array(_), "length") => Ok(self.call_c("dial_array_length", &[optr.into()])),
            (Type::Array(_), "push") => {
                let elem_ty = type_of(&args[0], scopes, functions, structs)?;
                let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                // 用 to_elem_bits：float 走 bitcast 保留 IEEE754 位模式，绝不 fptosi 截断小数。
                let v_i64 = self.to_elem_bits(function, v, &elem_ty);
                let res = self.call_c("dial_array_push", &[optr.into(), v_i64.into()]);
                // 元素归属结算：push 已为元素 retain 一份，借来的值不得再释放、
                // 自带所有权的值（字面量等）应把临时所有权移交给数组。
                self.sink_elem_after_retain(&args[0], &elem_ty, v, v_i64);
                Ok(res)
            }
            (Type::Array(e), "get") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                let res = self.call_c("dial_array_get", &[optr.into(), i.into()]);
                // 按元素类型还原 i64 槽位模式（float bitcast、bool i1↔i64、句柄 int_to_ptr）。
                Ok(self.from_elem_bits(e.as_ref(), res.into_int_value()))
            }
            (Type::Array(_), "set") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                let elem_ty = type_of(&args[1], scopes, functions, structs)?;
                let v = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                let v_i64 = self.to_elem_bits(function, v, &elem_ty);
                let res = self.call_c("dial_array_set", &[optr.into(), i.into(), v_i64.into()]);
                // 元素归属结算：set 已为元素 retain 一份，借来的值不得再释放、
                // 自带所有权的值（字面量等）应把临时所有权移交给数组。
                self.sink_elem_after_retain(&args[1], &elem_ty, v, v_i64);
                Ok(res)
            }
            (Type::Array(_), "clone") => Ok(self.call_c("dial_array_clone", &[optr.into()])),
            // v1.04 B11：`a.slice(from, to)` 与切片语法 `a[from..to]` 共用同一个
            // C 运行时函数 dial_array_slice；返回新数组，不改变接收者，无需写回变量槽。
            (Type::Array(_), "slice") => {
                let from = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                let to = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &Type::Int);
                Ok(self.call_c("dial_array_slice", &[optr.into(), from.into(), to.into()]))
            }
            // remove / pop：返回被删元素（已还原为元素类型），并就地写回数组
            // （COW 可能更新指针）。先 get 取出被删元素，句柄类额外 retain 一份，
            // 因为 remove/pop 会从数组释放它；随后把数组新指针写回接收变量（仅接收者为变量时）。
            (Type::Array(e), "remove") => {
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                let got = self.call_c("dial_array_get", &[optr.into(), i.into()]);
                let removed = self.from_elem_bits(e.as_ref(), got.into_int_value());
                if Self::is_heap_type(e.as_ref()) { self.retain(e.as_ref(), removed); }
                let newp = self.call_c("dial_array_remove", &[optr.into(), i.into()]);
                if let Expr::Var(name) = obj {
                    if let Some((_, (_m, ptr))) =
                        scopes.iter().rev().find_map(|s| s.get(name).map(|v| v.clone()))
                    {
                        self.builder.build_store(ptr, newp).unwrap();
                    }
                }
                Ok(removed)
            }
            (Type::Array(e), "pop") => {
                let len = self.call_c("dial_array_length", &[optr.into()]);
                let last = self.builder.build_int_sub(len.into_int_value(), i64t.const_int(1, false), "last").unwrap();
                let got = self.call_c("dial_array_get", &[optr.into(), last.into()]);
                let removed = self.from_elem_bits(e.as_ref(), got.into_int_value());
                if Self::is_heap_type(e.as_ref()) { self.retain(e.as_ref(), removed); }
                let newp = self.call_c("dial_array_pop", &[optr.into()]);
                if let Expr::Var(name) = obj {
                    if let Some((_, (_m, ptr))) =
                        scopes.iter().rev().find_map(|s| s.get(name).map(|v| v.clone()))
                    {
                        self.builder.build_store(ptr, newp).unwrap();
                    }
                }
                Ok(removed)
            }
            // sort()：将数组排序（COW 若被共享则先克隆，再原地排序），返回
            // （可能更新后的）数组指针，并写回接收变量（仅接收者为变量时）。
            (Type::Array(elem), "sort") => {
                // v1.04 第 2 批 C5：string[] 按字节字典序（比较内容而非指针），
                // 标量数组仍走原 dial_array_sort。两者都 COW 后返回可能更新的指针。
                let cname = if *elem.as_ref() == Type::String {
                    "dial_array_sort_string"
                } else {
                    "dial_array_sort"
                };
                let newp = self.call_c(cname, &[optr.into()]);
                if let Expr::Var(name) = obj {
                    if let Some((_, (_m, ptr))) =
                        scopes.iter().rev().find_map(|s| s.get(name).map(|v| v.clone()))
                    {
                        self.builder.build_store(ptr, newp).unwrap();
                    }
                }
                Ok(newp)
            }
            // ---- v1.04 第 2 批 C5：数组 API 扩充 ----
            // insert/reverse/clear 与 sort/remove 同款：返回（可能 COW 更新的）指针，
            // 写回接收变量。用一个小闭包统一「写回」逻辑，避免三处复制粘贴。
            (Type::Array(_), "insert") => {
                let elem_ty = type_of(&args[1], scopes, functions, structs)?;
                let i = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                let v = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                let v_i64 = self.to_elem_bits(function, v, &elem_ty);
                let newp = self.call_c("dial_array_insert", &[optr.into(), i.into(), v_i64.into()]);
                // C 侧 insert 已 elem_retain 新元素，与 push 同款做一次归属结算。
                self.sink_elem_after_retain(&args[1], &elem_ty, v, v_i64);
                if let Expr::Var(name) = obj {
                    if let Some((_, (_m, ptr))) = scopes.iter().rev().find_map(|s| s.get(name).map(|v| v.clone())) {
                        self.builder.build_store(ptr, newp).unwrap();
                    }
                }
                Ok(newp)
            }
            (Type::Array(_), "reverse" | "clear") => {
                let cname = if method == "reverse" { "dial_array_reverse" } else { "dial_array_clear" };
                let newp = self.call_c(cname, &[optr.into()]);
                if let Expr::Var(name) = obj {
                    if let Some((_, (_m, ptr))) = scopes.iter().rev().find_map(|s| s.get(name).map(|v| v.clone())) {
                        self.builder.build_store(ptr, newp).unwrap();
                    }
                }
                Ok(newp)
            }
            (Type::Array(_), "index_of") => {
                let elem_ty = type_of(&args[0], scopes, functions, structs)?;
                let v = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                let v_i64 = self.to_elem_bits(function, v, &elem_ty);
                Ok(self.call_c("dial_array_index_of", &[optr.into(), v_i64.into()]))
            }
            (Type::Array(_), "join") => {
                let sep = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                Ok(self.call_c("dial_array_join", &[optr.into(), sep.into()]))
            }
            // reduce：复用 v1.03 的 arity=2 thunk（fn(T,T,rawptr<void>):T），无闭包。
            // init / 返回值都是标量位模式，用 to_elem_bits / from_elem_bits 对称转换。
            (Type::Array(elem), "reduce") => {
                let thunk = self.gen_hof_thunk(elem.as_ref(), elem.as_ref(), 2)?;
                let fn_raw = self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?;
                let init_ty = type_of(&args[1], scopes, functions, structs)?;
                let init_v = self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?;
                let init_bits = self.to_elem_bits(function, init_v, &init_ty);
                let state: BasicValueEnum<'ctx> = if args.len() > 2 {
                    self.gen_expr(&args[2], function, scopes, func_values, functions, structs)?
                } else {
                    self.i8ptr().const_null().into()
                };
                let r = self.call_c(
                    "dial_array_reduce",
                    &[
                        optr.into(),
                        init_bits.into(),
                        fn_raw.into(),
                        state.into(),
                        thunk.as_global_value().as_pointer_value().into(),
                    ],
                );
                Ok(self.from_elem_bits(elem.as_ref(), r.into_int_value()))
            }
            // v1.03：接受函数指针的高阶数组方法。回调签名精确、状态显式透传，
            // 没有闭包、没有环境对象、没有隐藏类型标签。
            (Type::Array(elem), "map" | "filter" | "sort_by") => {
                let op: u64 = match method {
                    "map" => 0,
                    "filter" => 1,
                    _ => 2,
                };
                let arity = if method == "sort_by" { 2 } else { 1 };
                let ret_ty = if method == "map" {
                    match type_of(&args[0], scopes, functions, structs)? {
                        Type::FnPtr(_, r) => *r,
                        _ => {
                            return Err(DialcError::new(
                                "内部错误：map 的首参应是函数指针（语义层已拦截）",
                                0, 0,
                            ))
                        }
                    }
                } else if method == "filter" {
                    Type::Bool
                } else {
                    Type::Int
                };
                let thunk = self.gen_hof_thunk(elem.as_ref(), &ret_ty, arity)?;
                // fn_raw 是运行期值：既可以是具名函数（求值为地址），
                // 也可以是函数指针变量（load 出来的地址）。
                let fn_raw = self.gen_expr(
                    &args[0], function, scopes, func_values, functions, structs,
                )?;
                let state: BasicValueEnum<'ctx> = if args.len() > 1 {
                    self.gen_expr(
                        &args[1], function, scopes, func_values, functions, structs,
                    )?
                } else {
                    // 不传状态时给一个空 rawptr<void>；回调若解引用它后果由用户负责。
                    self.i8ptr().const_null().into()
                };
                let out_kind = if method == "map" {
                    self.array_elem_kind(&ret_ty)
                } else {
                    self.array_elem_kind(elem.as_ref())
                };
                let newp = self.call_c(
                    "dial_array_hof",
                    &[
                        optr.into(),
                        i64t.const_int(op, false).into(),
                        i64t.const_int(out_kind, false).into(),
                        fn_raw.into(),
                        state.into(),
                        thunk.as_global_value().as_pointer_value().into(),
                    ],
                );
                // sort_by 原地排序（COW 后可能换 buffer），把新指针写回变量槽，
                // 与 dial_array_sort 的处理保持一致。
                if method == "sort_by" {
                    if let Expr::Var(name) = obj {
                        if let Some((_, (_m, ptr))) = scopes
                            .iter()
                            .rev()
                            .find_map(|s| s.get(name).map(|v| v.clone()))
                        {
                            self.builder.build_store(ptr, newp).unwrap();
                        }
                    }
                }
                Ok(newp)
            }
            (Type::Map, "put") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let v = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &type_of(&args[1], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_put", &[optr.into(), k.into(), v.into()]))
            }
            (Type::Map, "get") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_get", &[optr.into(), k.into()]))
            }
            // v1.04 C6：命中返回真实值，未命中返回调用方给的默认值（一次探测）。
            (Type::Map, "get_or") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &Type::Int);
                let d = self.to_i64(self.gen_expr(&args[1], function, scopes, func_values, functions, structs)?, &Type::Int);
                Ok(self.call_c("dial_map_get_or", &[optr.into(), k.into(), d.into()]))
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
            // set<int>（v1.00）：成员即 map 的键，value 恒为 1。全部复用 dial_map_* 运行时。
            (Type::Set(_), "add") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_put", &[optr.into(), k.into(), i64t.const_int(1, false).into()]))
            }
            (Type::Set(_), "contains") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                let r = self.call_c("dial_map_contains", &[optr.into(), k.into()]);
                Ok(self.builder.build_int_compare(IntPredicate::NE, r.into_int_value(), i64t.const_int(0, false), "scontains").unwrap().into())
            }
            (Type::Set(_), "remove") => {
                let k = self.to_i64(self.gen_expr(&args[0], function, scopes, func_values, functions, structs)?, &type_of(&args[0], scopes, functions, structs)?);
                Ok(self.call_c("dial_map_remove", &[optr.into(), k.into()]))
            }
            (Type::Set(_), "size") => Ok(self.call_c("dial_map_size", &[optr.into()])),
            (Type::Set(_), "values") => Ok(self.call_c("dial_map_keys", &[optr.into()])),
            (Type::Error, "msg") => Ok(self.call_c("dial_error_msg", &[optr.into()])),
            _ => Err(DialcError::new(
                format!("语义错误：类型 '{}' 无方法 '{}'（应由 Sema 先行拦截）", ot.name(), method),
                0, 0,
            )),
        }
    }

    /// 把表达式按 K/V 的静态类型转成 C 侧统一使用的 8 字节位模式。
    /// 同时返回原始值，供调用方做所有权结算（settle_fresh_value）。
    #[allow(clippy::too_many_arguments)]
    fn kv_arg(
        &self,
        e: &Expr,
        ty: &Type,
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<(BasicValueEnum<'ctx>, IntValue<'ctx>)> {
        let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
        let bits = match ty {
            // float 必须是**位模式**（bitcast），绝不能 fptosi —— 后者会丢掉小数，
            // 那正是零隐式转换铁律要防的静默数据损坏。
            Type::Float => self.builder
                .build_bit_cast(v, self.context.i64_type(), "f2bits").unwrap().into_int_value(),
            Type::String => self.builder
                .build_ptr_to_int(v.into_pointer_value(), self.context.i64_type(), "p2bits").unwrap(),
            Type::Bool => self.builder
                .build_int_z_extend(v.into_int_value(), self.context.i64_type(), "b2bits").unwrap(),
            // v1.04 B6：结构体值装箱为 RC 对象后存指针位模式（与数组元素同一套机制）。
            Type::Struct(name) => {
                let sty = self.struct_types[name];
                let size = sty.size_of().expect("结构体应有确定尺寸");
                let tmp = self.alloca_in_entry(function, sty.into(), "sbox_src");
                self.builder.build_store(tmp, v).unwrap();
                let boxp = self
                    .call_c("dial_struct_box", &[tmp.into(), size.into()])
                    .into_pointer_value();
                self.builder
                    .build_ptr_to_int(boxp, self.context.i64_type(), "box2bits")
                    .unwrap()
            }
            // int / char / byte 的 IR 本来就是 i64。
            _ => v.into_int_value(),
        };
        Ok((v, bits))
    }

    /// C 侧返回的 i64 位模式还原成 Dial 的 V 类型值。
    fn kv_from_bits(
        &self,
        function: FunctionValue<'ctx>,
        bits: IntValue<'ctx>,
        ty: &Type,
    ) -> BasicValueEnum<'ctx> {
        match ty {
            Type::Float => self.builder
                .build_bit_cast(bits, self.context.f64_type(), "bits2f").unwrap().into(),
            // bool 用「!= 0」而非 truncate：未命中返回的 -1 也该是 false 之外的值，
            // 但 C 侧对 KV_BOOL 的默认值已是 0，这里保持与 print(bool) 一致的口径。
            Type::Bool => self.builder
                .build_int_compare(
                    IntPredicate::NE, bits,
                    self.context.i64_type().const_int(0, false), "bits2b",
                ).unwrap().into(),
            Type::String => self.builder
                .build_int_to_ptr(bits, self.i8ptr(), "bits2p").unwrap().into(),
            // v1.04 B6：结构体值——map 未命中时 C 侧返回空指针（0），此时还原为零值结构体。
            // 用**分支**而非 select：select 两侧都会求值，空指针解引用就是 use-after-free。
            Type::Struct(name) => {
                let sty = self.struct_types[name];
                let i64t = self.context.i64_type();
                let is_null = self.builder
                    .build_int_compare(
                        IntPredicate::EQ, bits, i64t.const_int(0, false), "isnull",
                    )
                    .unwrap();
                let null_bb = self.context.append_basic_block(function, "unbox.null");
                let box_bb = self.context.append_basic_block(function, "unbox.box");
                let cont_bb = self.context.append_basic_block(function, "unbox.cont");
                self.builder.build_conditional_branch(is_null, null_bb, box_bb).unwrap();
                // 未命中：零值结构体
                self.builder.position_at_end(null_bb);
                let zero = sty.const_zero();
                self.builder.build_unconditional_branch(cont_bb).unwrap();
                // 命中：解箱，按值拷出
                self.builder.position_at_end(box_bb);
                let boxp = self.builder.build_int_to_ptr(bits, self.i8ptr(), "bits2box").unwrap();
                let sp = self.builder
                    .build_pointer_cast(boxp, self.context.ptr_type(AddressSpace::default()), "box2s")
                    .unwrap();
                let loaded = self.builder.build_load(sty, sp, "unbox").unwrap();
                self.builder.build_unconditional_branch(cont_bb).unwrap();
                // 合流
                self.builder.position_at_end(cont_bb);
                let phi = self.builder.build_phi(sty, "unboxphi").unwrap();
                let zb: &dyn BasicValue = &zero;
                // build_load 返回 BasicValueEnum；phi 需要具体类型，取其中的 StructValue 分支。
                let lb: &dyn BasicValue = match &loaded {
                    BasicValueEnum::StructValue(sv) => sv,
                    _ => unreachable!("结构体 load 的结果必为 StructValue"),
                };
                phi.add_incoming(&[(zb, null_bb)]);
                phi.add_incoming(&[(lb, box_bb)]);
                phi.as_basic_value().into()
            }
            _ => bits.into(),
        }
    }

    /// 泛型 map<K,V> 的方法分派（v1.00）。所有元素都以 8 字节位模式进出 C 运行时。
    #[allow(clippy::too_many_arguments)]
    fn gen_kvmap_method(
        &self,
        optr: BasicValueEnum<'ctx>,
        k: &Type,
        v: &Type,
        method: &str,
        args: &[Expr],
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        let i64t = self.context.i64_type();
        match method {
            "put" => {
                let (ka, kb) = self.kv_arg(&args[0], k, function, scopes, func_values, functions, structs)?;
                let (va, vb) = self.kv_arg(&args[1], v, function, scopes, func_values, functions, structs)?;
                let res = self.call_c("dial_kvmap_put", &[optr.into(), kb.into(), vb.into()]);
                // C 侧已对 string 键/值 retain 一份；自带所有权的字面量要抵消这一份，
                // 借来的变量则保留（与 array.push 同一结算口径）。
                // 结构体值：put 已把装箱对象 retain 进表里，本地那份须归还（表独占）。
                self.settle_fresh_value(&args[0], k, ka);
                self.sink_elem_after_retain(&args[1], v, va, vb);
                Ok(res)
            }
            "get" | "contains" | "remove" => {
                let (ka, kb) = self.kv_arg(&args[0], k, function, scopes, func_values, functions, structs)?;
                let cname = match method {
                    "get" => "dial_kvmap_get",
                    "contains" => "dial_kvmap_contains",
                    _ => "dial_kvmap_remove",
                };
                let r = self.call_c(cname, &[optr.into(), kb.into()]);
                // 查找用的键不被表持有：字面量自带的那一份所有权就地归还。
                self.settle_fresh_value(&args[0], k, ka);
                if method == "contains" {
                    Ok(self.builder.build_int_compare(
                        IntPredicate::NE, r.into_int_value(),
                        i64t.const_int(0, false), "kvcontains",
                    ).unwrap().into())
                } else {
                    let out = self.kv_from_bits(function, r.into_int_value(), v);
                    // remove：被删条目的值所有权随返回值移交调用方
                    // （结构体值即那个装箱对象）——拷出副本后释放它。
                    if method == "remove" && matches!(v, Type::Struct(_)) {
                        let boxp = self.builder
                            .build_int_to_ptr(r.into_int_value(), self.i8ptr(), "boxp")
                            .unwrap();
                        self.call_c_void("dial_rc_release", &[boxp.into()]);
                    }
                    Ok(out)
                }
            }
            // v1.04 C6：泛型 map 的 get_or(k, default)。键与默认值都按静态类型
            // 转成 8 字节位模式；C 侧命中返回真实值、未命中返回默认值位模式。
            // 语义层已把 V 限定为标量，故这里不存在句柄所有权结算问题。
            "get_or" => {
                let (ka, kb) = self.kv_arg(&args[0], k, function, scopes, func_values, functions, structs)?;
                let (_da, db) = self.kv_arg(&args[1], v, function, scopes, func_values, functions, structs)?;
                let r = self.call_c("dial_kvmap_get_or", &[optr.into(), kb.into(), db.into()]);
                self.settle_fresh_value(&args[0], k, ka);
                let out = self.kv_from_bits(function, r.into_int_value(), v);
                // 结构体默认值：先按值拷出（未命中时返回值就是本地这份装箱），
                // 再归还本地那份装箱——副本已是纯标量位，释放不影响已拷出的值。
                if matches!(v, Type::Struct(_)) {
                    let boxp = self.builder.build_int_to_ptr(db, self.i8ptr(), "boxp").unwrap();
                    self.call_c_void("dial_rc_release", &[boxp.into()]);
                }
                Ok(out)
            }
            "size" => Ok(self.call_c("dial_kvmap_size", &[optr.into()])),
            "keys" => Ok(self.call_c("dial_kvmap_keys", &[optr.into()])),
            "values" => Ok(self.call_c("dial_kvmap_values", &[optr.into()])),
            _ => Err(DialcError::new(
                format!("内部错误：未知的 map<K,V> 方法 '{}'", method), 0, 0,
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
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                let lval = self.gen_expr(l, function, scopes, func_values, functions, structs)?;
                let rval = self.gen_expr(r, function, scopes, func_values, functions, structs)?;
                let a = lval.into_int_value();
                let b = rval.into_int_value();
                let res: BasicValueEnum = match op {
                    BinOp::BitAnd => self.builder.build_and(a, b, "band").unwrap().into(),
                    BinOp::BitOr => self.builder.build_or(a, b, "bor").unwrap().into(),
                    BinOp::BitXor => self.builder.build_xor(a, b, "bxor").unwrap().into(),
                    BinOp::Shl => self.builder.build_left_shift(a, b, "shl").unwrap().into(),
                    BinOp::Shr => {
                        // int 为有符号 i64，>> 用算术右移（保留符号位）
                        self.builder.build_right_shift(a, b, true, "shr").unwrap().into()
                    }
                    _ => unreachable!(),
                };
                Ok(res)
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

    /// 字符串插值（v1.00）：逐段转成 DialString，再用 concat 串起来。
    ///
    /// 前置插入一个空串片段，保证至少发生一次 concat，最终结果恒为**新建对象**；
    /// 于是所有自建的中间片段都能安全释放，而借来的 string 变量绝不会被误释放、
    /// 也不会被当作「新对象的所有权」直接返回。
    #[allow(clippy::too_many_arguments)]
    fn gen_interp(
        &self,
        parts: &[InterpPart],
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        let empty_g = self.global_string(
            &format!("dialc.interp.{}", self.next_str_id()),
            "",
        );
        let empty = self.call_c("dial_string_new", &[empty_g.into()]);
        // (值, 是否为本次新建、可在拼接后释放)
        let mut segs: Vec<(BasicValueEnum<'ctx>, bool)> = vec![(empty, true)];
        for p in parts {
            match p {
                InterpPart::Text(t) => {
                    let g = self.global_string(
                        &format!("dialc.interp.{}", self.next_str_id()),
                        t,
                    );
                    let v = self.call_c("dial_string_new", &[g.into()]);
                    segs.push((v, true));
                }
                InterpPart::Expr(e) => {
                    let ty = type_of(e, scopes, functions, structs)?;
                    let v = self.gen_expr(e, function, scopes, func_values, functions, structs)?;
                    let borrowed = yields_borrowed_ref(e);
                    let (sv, owned) = match ty {
                        Type::Int => (self.call_c("dial_int_to_string", &[v.into()]), true),
                        Type::Float => (self.call_c("dial_float_to_string", &[v.into()]), true),
                        Type::Char => (self.call_c("dial_char_to_string", &[v.into()]), true),
                        Type::Bool => {
                            let i = self.to_i64(v, &Type::Bool);
                            (self.call_c("dial_bool_to_string", &[i.into()]), true)
                        }
                        Type::String => (v, !borrowed),
                        _ => {
                            return Err(DialcError::new(
                                "内部错误：字符串插值片段类型不可转字符串（语义层应已拦截）",
                                0, 0,
                            ))
                        }
                    };
                    segs.push((sv, owned));
                }
            }
        }
        let mut acc: Option<(BasicValueEnum<'ctx>, bool)> = None;
        for (v, owned) in segs {
            match acc {
                None => acc = Some((v, owned)),
                Some((a, a_owned)) => {
                    let next = self.call_c("dial_string_concat", &[a.into(), v.into()]);
                    // concat 只复制、不消耗输入；输入与中间结果若是自建的，就地释放。
                    if a_owned {
                        self.release_value(&Type::String, a);
                    }
                    if owned {
                        self.release_value(&Type::String, v);
                    }
                    acc = Some((next, true));
                }
            }
        }
        Ok(acc.unwrap().0)
    }

    /// v1.00 内建：assert / panic / exit / min / max / abs / 浮点数学函数。
    #[allow(clippy::too_many_arguments)]
    fn gen_builtin_v1(
        &self,
        name: &str,
        args: &[Expr],
        function: FunctionValue<'ctx>,
        scopes: &CgScopes<'ctx>,
        func_values: &HashMap<String, FunctionValue<'ctx>>,
        functions: &HashMap<String, FuncInfo>,
        structs: &StructTable,
    ) -> Result<BasicValueEnum<'ctx>> {
        let i64t = self.context.i64_type();
        let zero: BasicValueEnum = i64t.const_int(0, false).into();
        let gen = |a: &Expr| self.gen_expr(a, function, scopes, func_values, functions, structs);
        match name {
            "assert" => {
                let cond = gen(&args[0])?;
                let ok_bb = self.context.append_basic_block(function, "assert.ok");
                let fail_bb = self.context.append_basic_block(function, "assert.fail");
                self.builder
                    .build_conditional_branch(cond.into_int_value(), ok_bb, fail_bb)
                    .unwrap();
                self.builder.position_at_end(fail_bb);
                let msg: BasicMetadataValueEnum = if args.len() == 2 {
                    let m = gen(&args[1])?;
                    self.call_c("dial_string_cstr", &[m.into()]).into()
                } else {
                    self.global_string("assert.default", "assert 失败").into()
                };
                // A7：断言失败带「文件:行:列」，不再是只有一句「assert 失败」。
                self.emit_fail_loc("dial_assert_fail_loc", msg);
                self.builder.build_unreachable().unwrap();
                self.builder.position_at_end(ok_bb);
                Ok(zero)
            }
            "panic" => {
                let m = gen(&args[0])?;
                let cstr = self.call_c("dial_string_cstr", &[m.into()]);
                self.emit_fail_loc("dial_panic_loc", cstr.into());
                Ok(zero)
            }
            "exit" => {
                let c = gen(&args[0])?;
                self.call_c_void("dial_exit", &[c.into()]);
                Ok(zero)
            }
            "args" => Ok(self.call_c("dial_args", &[])),
            "now" => Ok(self.call_c("dial_now", &[])),
            "random" => Ok(self.call_c("dial_random", &[])),
            "sleep" => {
                let ms = gen(&args[0])?;
                self.call_c_void("dial_sleep", &[ms.into()]);
                Ok(zero)
            }
            "min" | "max" => {
                let a = gen(&args[0])?;
                let b = gen(&args[1])?;
                let ty = type_of(&args[0], scopes, functions, structs)?;
                let cmp = if ty == Type::Int {
                    self.builder
                        .build_int_compare(
                            if name == "min" { IntPredicate::SLT } else { IntPredicate::SGT },
                            a.into_int_value(),
                            b.into_int_value(),
                            "minmax.cmp",
                        )
                        .unwrap()
                } else {
                    self.builder
                        .build_float_compare(
                            if name == "min" { FloatPredicate::OLT } else { FloatPredicate::OGT },
                            a.into_float_value(),
                            b.into_float_value(),
                            "minmax.cmp",
                        )
                        .unwrap()
                };
                Ok(self.builder.build_select(cmp, a, b, "minmax.sel").unwrap())
            }
            "abs" => {
                let a = gen(&args[0])?;
                let ty = type_of(&args[0], scopes, functions, structs)?;
                if ty == Type::Int {
                    let neg: BasicValueEnum = self
                        .builder
                        .build_int_neg(a.into_int_value(), "abs.neg")
                        .unwrap()
                        .into();
                    let is_neg = self
                        .builder
                        .build_int_compare(
                            IntPredicate::SLT,
                            a.into_int_value(),
                            i64t.const_int(0, false),
                            "abs.cmp",
                        )
                        .unwrap();
                    Ok(self.builder.build_select(is_neg, neg, a, "abs.sel").unwrap())
                } else {
                    Ok(self.call_c("fabs", &[a.into()]))
                }
            }
            "sqrt" | "pow" | "floor" | "ceil" | "round" => {
                let vals: Vec<BasicMetadataValueEnum> = args
                    .iter()
                    .map(|a| gen(a).map(|v| v.into()))
                    .collect::<Result<Vec<_>>>()?;
                Ok(self.call_c(name, &vals))
            }
            _ => Err(DialcError::new(
                format!("内部错误：未知的 v1.00 内建 '{}'", name),
                0, 0,
            )),
        }
    }

    fn emit_print(&self, ty: &Type, val: BasicValueEnum<'ctx>) -> Result<()> {
        match *ty {
            // 裸指针不可打印（语义层已拦截；这里仅保证 match 穷尽）。
            Type::RawPtr(_) | Type::VolatilePtr(_) => {
                return Err(DialcError::new(
                    "内部错误：裸指针不可打印（应由 Sema 先行拦截）",
                    0,
                    0,
                ))
            }
            // 函数指针同样是纯地址，语义层已拦下（这里只保证 match 穷尽）。
            Type::FnPtr(..) => {
                return Err(DialcError::new(
                    "内部错误：函数指针不可打印（应由 Sema 先行拦截）",
                    0,
                    0,
                ))
            }
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
            // 集合 / 泛型 map 与 map 同构：直接 print 没有可读形态，请用 values() 取快照后逐元素打印。
            Type::Set(_) | Type::KvMap(..) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print 集合 / 泛型 map，请先用 values() 取快照再逐元素打印",
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
            // 裸指针不可输出（语义层已拦截；这里仅保证 match 穷尽）。
            Type::RawPtr(_) | Type::VolatilePtr(_) => {
                return Err(DialcError::new(
                    "内部错误：裸指针不可输出（应由 Sema 先行拦截）",
                    0,
                    0,
                ))
            }
            Type::FnPtr(..) => {
                return Err(DialcError::new(
                    "内部错误：函数指针不可输出（应由 Sema 先行拦截）",
                    0,
                    0,
                ))
            }
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
            // 与 emit_print 同口径：集合 / 泛型 map 没有可读形态，不能退化成把二进制 payload 当字符串吐出。
            Type::Set(_) | Type::KvMap(..) => {
                return Err(DialcError::new(
                    "语义错误：当前版本不支持直接 print_raw 集合 / 泛型 map，请先用 values() 取快照再逐元素输出",
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

    /// eprint 的 stderr 类型分派（v1.04 C8，末尾补 '\n'）。
    /// 刻意与 emit_print **各自独立**复制一份：§21.1 拒绝「为省几行合并成带标志的
    /// 通用函数」，四个输出函数读起来各自直白，调试时可以一眼定位到具体那条 C 调用。
    /// 语义层已保证实参只能是下列可输出类型，其余分支仅作穷尽性兜底。
    fn emit_eprint(&self, ty: &Type, val: BasicValueEnum<'ctx>) -> Result<()> {
        match *ty {
            Type::Int | Type::Byte => {
                self.call_c_void("dial_eprint_int", &[val.into()]);
            }
            Type::Float => {
                self.call_c_void("dial_eprint_float", &[val.into()]);
            }
            Type::Bool => {
                let i = self.to_i64(val, &Type::Bool);
                self.call_c_void("dial_eprint_bool", &[i.into()]);
            }
            // char 输出的是码点对应的 UTF-8 字符，不是码点数字（与 print 一致）。
            Type::Char => {
                self.call_c_void("dial_eprint_char", &[val.into()]);
            }
            Type::String => {
                self.call_c_void("dial_eprint_string", &[val.into()]);
            }
            // 可恢复异常：stderr 打印其错误消息字符串（带换行）。
            Type::Error => {
                let msg = self.call_c("dial_error_msg", &[val.into()]);
                self.call_c_void("dial_eprint_string", &[msg.into()]);
            }
            _ => {
                return Err(DialcError::new(
                    "内部错误：eprint 收到了不可输出的类型（应由 Sema 先行拦截）",
                    0, 0,
                ))
            }
        }
        Ok(())
    }

    /// eprint_raw 的 stderr 类型分派（v1.04 C8，**不补 '\n'**）。
    /// 与 emit_print_raw 逐类型对应；char 走 dial_eprint_write_char，标量走栈缓冲版本。
    fn emit_eprint_raw(&self, ty: &Type, val: BasicValueEnum<'ctx>) -> Result<()> {
        match *ty {
            Type::Int | Type::Byte => {
                self.call_c_void("dial_eprint_raw_int", &[val.into()]);
            }
            Type::Float => {
                self.call_c_void("dial_eprint_raw_float", &[val.into()]);
            }
            Type::Bool => {
                let i = self.to_i64(val, &Type::Bool);
                self.call_c_void("dial_eprint_raw_bool", &[i.into()]);
            }
            Type::Char => {
                self.call_c_void("dial_eprint_write_char", &[val.into()]);
            }
            Type::String => {
                self.call_c_void("dial_eprint_raw", &[val.into()]);
            }
            Type::Error => {
                let msg = self.call_c("dial_error_msg", &[val.into()]);
                self.call_c_void("dial_eprint_raw", &[msg.into()]);
            }
            _ => {
                return Err(DialcError::new(
                    "内部错误：eprint_raw 收到了不可输出的类型（应由 Sema 先行拦截）",
                    0, 0,
                ))
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
/// 表达式产出的堆引用是「借来的」还是「自带所有权」的？
///
/// - **借来**：所有权在别处——变量槽、结构体字段、数组元素、容器的取值方法
///   （`get` / `at`）。借用方把它存进别处时**必须 retain 一份**，
///   否则来源一被释放，目标位置就悬垂（use-after-free）。
/// - **自带所有权**：字面量、返回新对象的调用（`clone` / `substring` / `concat` /
///   `new_map` / `channel`）、结构体字面量。借用方**直接接管**即可，
///   不得再 retain（那会多算一份，导致泄漏）。
fn yields_borrowed_ref(e: &Expr) -> bool {
    match e {
        Expr::Var(_) | Expr::Field(..) | Expr::Index(..) => true,
        Expr::Method(_, m, _) => matches!(m.as_str(), "get" | "at"),
        _ => false,
    }
}

fn lvalue_root(e: &Expr) -> Option<&str> {
    match e {
        Expr::Var(n) => Some(n),
        Expr::Field(o, _) => lvalue_root(o),
        Expr::Index(o, _) => lvalue_root(o),
        _ => None,
    }
}
