//! AST → 自研后端 IR 的 lowering（核心子集）。
//!
//! 关键简化：布尔在 IR 内部一律用 I64（0/1）表示；只有在「函数参数 C ABI」与
//! 「结构体字段」这两处才按 ABI 收窄到 i32 / 1 字节（`param_ir` / `field_ir`），
//! 从而绕开 I1 全程搬运的复杂性（详见 src/ir.rs 的三套宽度映射）。
//!
//! 覆盖：标量 / 字符串 / 整数数组 / 结构体(局部 + 字段读写) /
//! 控制流(if/while/for/for-in/do-while/break/continue/三元/短路 and or) /
//! 函数(def/call/递归) / FFI(extern) / 基础 ARC / print / 字符串插值 / 返回值 / match(字面量)。
//!
//! 未覆盖（首版排除，明确报编译错误）：defer/with/label/goto/throw/try/select/spawn/
//! ForEachMap/map/kvmap/set/channel/file/thread/opaque/fnptr/simd/结构体作为函数
//! 参数或返回值 / 结构体方法调用 / ptr_cast / 裸指针解引用赋值 / 数组元素赋值。

use crate::ast::*;
use crate::builtin;
use crate::ir::*;
use crate::semantic::{FuncInfo, StructTable};
use std::collections::{HashMap, HashSet};

/// 一个局部变量在 lowering 期的记账。
#[derive(Clone)]
struct Local {
    /// 指向其栈槽（8 字节，结构体为更大槽）的指针 vreg。
    ptr: VReg,
    /// 该变量的值类型（IR 内部宽度）。
    ty: IrType,
    /// 值语义 Dial 类型（用于 ARC 种类判定与字段布局）。
    dt: Type,
    /// 是否不透明堆句柄（需要 ARC）。
    is_handle: bool,
    /// 所有权是否已转移（move）：转移后不再 release，避免 UAF / 双释放。
    moved: bool,
}

/// 给一个 IR 标量宽度返回对应的「全零」立即数，用于结构体省略字段的防御性零初始化。
/// 与既有 lower_expr(IntLit/FloatLit) 的 Imm 表示一致，发射器直接支持无 vreg 的 Imm。
fn zero_imm_for(ty: &IrType) -> Value {
    match ty {
        IrType::F64 => Value::Imm(Imm::F64(0)),
        IrType::F32 => Value::Imm(Imm::F32(0)),
        IrType::I1 => Value::Imm(Imm::I1(false)),
        IrType::I8 => Value::Imm(Imm::I8(0)),
        IrType::I32 => Value::Imm(Imm::I32(0)),
        // Ptr/Handle/Struct/FnPtr 等一律用 I64(0) 位模式的空值。
        _ => Value::Imm(Imm::I64(0)),
    }
}

/// 结构体字段的**内存宽度**口径：extern struct（C ABI）字段 byte=I8(1B)，
/// 普通 struct（寄存器口径）字段 byte=I64(8B)，与 LLVM 后端的
/// cstruct_field_llvm / dialc_type_to_llvm 两套映射逐字节对齐。bool 两者均为 1B。
/// base_dt 必须是字段所属的结构体类型（含 extern struct）。
fn field_mem_ir(cstructs: &HashSet<String>, base_dt: &Type, fty: &Type) -> IrType {
    match struct_name_of(base_dt) {
        n if cstructs.contains(&n) => cabi_field_ir(fty),
        _ => field_ir(fty),
    }
}

/// 某个堆句柄类型的 ARC 种类；非句柄返回 None。
fn rc_kind(dt: &Type) -> Option<ArcKind> {
    match dt {
        Type::String => Some(ArcKind::String),
        Type::Bytes => Some(ArcKind::Bytes),
        Type::Array(_) => Some(ArcKind::Array),
        Type::Error => Some(ArcKind::Error),
        Type::Map | Type::KvMap(..) | Type::Set(_) | Type::Channel(_) => Some(ArcKind::Generic),
        _ => None,
    }
}

pub fn lower_program(
    prog: &Program,
    functions_sig: HashMap<String, FuncInfo>,
    structs: StructTable,
    goto_depths: HashMap<String, usize>,
    debug_info: bool,
    src_file: &str,
) -> Result<IrProgram, String> {
    // v1.04 FFI：extern struct 名集合。struct_layout 与字段 load/store 需据此区分
    // 两套字段口径（extern=C ABI byte=1B / 普通=寄存器 byte=8B），对齐 LLVM 后端。
    let cstruct_names: HashSet<String> = prog.cstructs.iter().map(|c| c.name.clone()).collect();
    let mut ip = IrProgram::new(functions_sig, structs, goto_depths, cstruct_names);
    // M7-e3ag：收集 packed 结构体名。布局内建（sizeof/alignof/offsetof）须按 C ABI
    // 口径处理 packed：size 为紧密排列值、align 固定 1（struct_layout 对 packed 的
    // align 仍返回自然对齐，直接复用会与 LLVM/C 不一致）。从 AST 的 structs/cstructs
    // 取 packed 标志，不修改 IrProgram / Lower::new 之外的签名。
    let packed: HashSet<String> = prog.structs.iter()
        .filter(|s| s.packed).map(|s| s.name.clone())
        .chain(prog.cstructs.iter().filter(|s| s.packed).map(|s| s.name.clone()))
        .collect();
    let mut spawn_targets: HashSet<String> = HashSet::new();
    let mut hof_thunks: Vec<(Type, Type, usize)> = Vec::new();
    for func in &prog.functions {
        // M7-e3w：调试标记只在 -g 时插入。非 -g 的 IR 必须与 e3v 逐字节一致，
        // 否则 Line/DebugVar 会打断 e3p 折叠 / popcount 融合等 pass 的模式窗口。
        let mut l = Lower::new(&mut ip, debug_info, &packed, src_file);
        l.lower_function(func)?;
        // M7-e3am：收集本函数内 spawn 引用的线程入口目标（去重在生成阶段）。
        spawn_targets.extend(l.spawn_targets.drain());
        // M7-e3an：收集本函数内高阶方法需要的回调 thunk 描述。
        hof_thunks.append(&mut l.hof_thunks);
    }
    // M7-e3am：为所有 spawn 目标生成线程入口 thunk（对齐 LLVM gen_spawn_thunk）。
    // 必须在所有函数 lower 完之后统一生成——thunk 是独立函数，不能嵌在调用方体内。
    // 排序保证生成顺序确定（哈希集合遍历无序），否则 .s 输出会随哈希种子漂移。
    let mut targets: Vec<String> = spawn_targets.into_iter().collect();
    targets.sort();
    for fname in &targets {
        gen_spawn_thunk(&mut ip, fname)?;
    }
    // M7-e3an：为所有高阶方法生成回调 thunk（对齐 LLVM gen_hof_thunk）。
    // Type 未实现 Hash，故先按 thunk 名字（elem码 + ret码 + arity，确定性）排序去重，
    // 再逐个生成；gen_hof_thunk 内部亦按名字幂等去重，双保险。
    hof_thunks.sort_by_key(|(e, r, a)| {
        format!("{}_{}_{}", hof_code(e), hof_code(r), a)
    });
    hof_thunks.dedup_by(|x, y| x.0 == y.0 && x.1 == y.1 && x.2 == y.2);
    for (elem, ret, arity) in &hof_thunks {
        gen_hof_thunk(&mut ip, elem, ret, *arity)?;
    }
    Ok(ip)
}

/// M7-e3am：为 `spawn(fn, ...)` 生成线程入口 thunk：`void __dial_thunk_fn(ptr cell)`。
///
/// C 的 pthread_create 只认统一签名 `void (*)(void*)`，而 Dial 函数实参类型各异。
/// thunk 从 argcell 的 8 字节槽按形参类型 Load 出实参，直接调用目标函数（具名调用，
/// 对齐 LLVM gen_spawn_thunk），调用后对移交进来的 ARC 形参各做一次 RcRelease
/// （释放 spawn 写端移交的那一份所有权；目标函数入口 retain / 出口 release 净零）。
///
/// 直接构造 IrFunction（不借 Lower 的可变状态），复用与 lower_call 普通调用一致的
/// ABI 规则：arg_tys 用 param_ir(pty)，调用返回值 void。
fn gen_spawn_thunk(ip: &mut IrProgram, fname: &str) -> Result<(), String> {
    let thunk_name = format!("__dial_thunk_{}", fname);
    // 同一函数被多处 spawn：thunk 只生成一次。
    if ip.functions.iter().any(|f| f.name == thunk_name) {
        return Ok(());
    }
    let sig = ip.functions_sig.get(fname)
        .ok_or_else(|| format!("[asm 后端] spawn 目标函数 `{}` 无签名信息", fname))?
        .clone();
    // 与 lower_function 一致：形参用 param_ir 声明（C ABI 宽度），prologue 按此填槽。
    let params: Vec<(String, IrType)> = sig.params.iter().enumerate()
        .map(|(i, t)| (format!("a{}", i), param_ir(t)))
        .collect();
    let mut f = IrFunction::new(thunk_name.clone(), params, IrType::Void, FuncLink::Private);

    // 手动构造 entry 块（IrFunction::new_block 返回 &mut IrBlock，这里直接 push）。
    f.blocks.push(IrBlock { id: 0, params: Vec::new(), insts: Vec::new(), term: Term::Unreachable });
    f.entry = 0;
    // 形参 cell（void*）的栈槽：prologue 把第 0 个实参寄存器（rdi/x0）写进来。
    // 必须与 params 平行地登记到 param_slots。
    let cell_slot = f.new_reg(IrType::Ptr);
    f.blocks[0].insts.push(IrInst::Alloca { dst: cell_slot, bytes: 8 });
    f.param_slots.push(cell_slot);

    // 从 cell_slot 读回 argcell 指针值。
    let cell_v = f.new_reg(IrType::Ptr);
    f.blocks[0].insts.push(IrInst::Load { dst: cell_v, ptr: cell_slot, ty: IrType::Ptr });

    // 逐形参：从 argcell 的第 i 个 8 字节槽 Load 出实参。
    // 槽宽统一 8 字节；load 类型对齐写端的 store 类型：
    //   Float→F64、句柄→Handle、裸指针→Ptr、其余（int/bool/byte/int32）→I64。
    let mut arg_vals: Vec<Value> = Vec::new();
    let mut arg_tys: Vec<IrType> = Vec::new();
    for (i, pty) in sig.params.iter().enumerate() {
        let slot = f.new_reg(IrType::Ptr);
        f.blocks[0].insts.push(IrInst::PtrAdd {
            dst: slot, base: cell_v, off: Value::Imm(Imm::I64((i * 8) as i64)),
        });
        let load_ty = match pty {
            Type::Float => IrType::F64,
            Type::Float32 => IrType::F32,
            t if reg_ir(t) == IrType::Handle => IrType::Handle,
            t if reg_ir(t) == IrType::Ptr => IrType::Ptr,
            _ => IrType::I64,
        };
        let v = f.new_reg(load_ty.clone());
        f.blocks[0].insts.push(IrInst::Load { dst: v, ptr: slot, ty: load_ty });
        arg_vals.push(Value::Reg(v));
        arg_tys.push(param_ir(pty));
    }

    // 直接调用目标函数（无返回值——语义层已保证线程入口返回 void）。
    f.blocks[0].insts.push(IrInst::Call {
        dst: None, func: fname.to_string(), args: arg_vals, arg_tys,
        ret_ty: IrType::Void, linkage: CallLink::Dial,
    });

    // 释放 spawn 写端移交的那一份所有权（对齐 LLVM thunk 末的 release_value）。
    // RcRelease 作用于「槽地址」，故复用前面 load 出的参数槽地址即可——但那几个
    // slot vreg 在循环里是局部的，这里重新计算一遍（cell_v 在整个函数体持续有效）。
    for (i, pty) in sig.params.iter().enumerate() {
        if let Some(k) = rc_kind(pty) {
            let slot = f.new_reg(IrType::Ptr);
            f.blocks[0].insts.push(IrInst::PtrAdd {
                dst: slot, base: cell_v, off: Value::Imm(Imm::I64((i * 8) as i64)),
            });
            f.blocks[0].insts.push(IrInst::RcRelease { ptr: slot, kind: k });
        }
    }

    f.blocks[0].term = Term::Return { val: None };
    ip.functions.push(f);
    Ok(())
}

// ============================================================================
// M7-e3an：高阶数组方法（map/filter/reduce/sort_by）的回调 thunk。
//
// C 运行时 dial_array_hof / dial_array_reduce 只认统一签名
//   int64_t (*thunk)(int64_t a, int64_t b, void *fn_raw, void *state)
// 而 Dial 回调签名精确（fn(elem[, elem], rawptr<void>): ret，标量宽度各异：
// float=f64 / bool=C ABI i32 / byte=i8 / int=i64）。硬套统一签名是 UB——编译器
// 知道静态签名，于是生成一份「i64 槽位位模式 ↔ 精确 C ABI 类型」的适配代码。
// 与 LLVM 后端 gen_hof_thunk 逐口径对齐（名字、位转换、C ABI 宽度、空 state）。
// ============================================================================

/// thunk 名字里的单字符类型码（与 LLVM gen_hof_thunk 的 code 闭包一致）。
fn hof_code(t: &Type) -> &'static str {
    match t {
        Type::Int => "i",
        Type::Float => "f",
        Type::Bool => "b",
        Type::Byte => "y",
        Type::Char => "c",
        _ => "x",
    }
}

/// 把 C 槽位的 i64 位模式 `v` 按 Dial 类型 `t` 还原，并适配到回调的 C ABI 参数宽度。
/// 返回 (参数值, 该参数的 IrType)：float 经 store/load 重解释位模式，
/// bool/byte 内部是 I64、再按 param_ir 收窄到 i32/i8（对齐 LLVM from_elem_bits + abi_adapt）。
fn hof_bits_to_cb_arg(f: &mut IrFunction, v: VReg, t: &Type) -> (Value, IrType) {
    let rit = reg_ir(t);
    let pit = param_ir(t);
    // 1) i64 位模式 → 内部宽度值（float 需 store/load 重解释；其余标量内部即 I64）。
    let internal: Value = if rit == IrType::I64 {
        Value::Reg(v)
    } else {
        let s = f.new_reg(IrType::Ptr);
        f.blocks[0].insts.push(IrInst::Alloca { dst: s, bytes: 8 });
        f.blocks[0].insts.push(IrInst::Store { ptr: s, val: Value::Reg(v), ty: IrType::I64 });
        let d = f.new_reg(rit.clone());
        f.blocks[0].insts.push(IrInst::Load { dst: d, ptr: s, ty: rit.clone() });
        Value::Reg(d)
    };
    // 2) 内部宽度 → C ABI 宽度（bool→i32 / byte→i8）。
    if pit == rit {
        (internal, pit)
    } else {
        let d = f.new_reg(pit.clone());
        let kind = match (&rit, &pit) {
            (IrType::I64, IrType::I32) | (IrType::I64, IrType::I8) => AConv::Trunc,
            _ => AConv::Zext,
        };
        f.blocks[0].insts.push(IrInst::Conv { kind, dst: d, a: internal, ty: pit.clone() });
        (Value::Reg(d), pit)
    }
}

/// 把回调返回的 C ABI 值 `cb_v`（宽度 param_ir(t)）转回 Dial 内部宽度，
/// 再压成 i64 槽位位模式（对齐 LLVM abi_adapt + to_elem_bits）。
fn hof_cb_ret_to_bits(f: &mut IrFunction, cb_v: VReg, t: &Type) -> VReg {
    let pit = param_ir(t);
    let rit = reg_ir(t);
    // 1) C ABI 返回宽度 → 内部宽度（bool/byte 窄整型零扩展回 I64）。
    let internal: VReg = if pit == rit {
        cb_v
    } else {
        let d = f.new_reg(rit.clone());
        let kind = match (&pit, &rit) {
            (IrType::I32, IrType::I64) | (IrType::I8, IrType::I64) => AConv::Zext,
            _ => AConv::Zext,
        };
        f.blocks[0].insts.push(IrInst::Conv { kind, dst: d, a: Value::Reg(cb_v), ty: rit.clone() });
        d
    };
    // 2) 内部宽度 → i64 位模式（float 经 store/load 取 IEEE754 位）。
    if rit == IrType::I64 {
        internal
    } else {
        let s = f.new_reg(IrType::Ptr);
        f.blocks[0].insts.push(IrInst::Alloca { dst: s, bytes: 8 });
        f.blocks[0].insts.push(IrInst::Store { ptr: s, val: Value::Reg(internal), ty: rit.clone() });
        let d = f.new_reg(IrType::I64);
        f.blocks[0].insts.push(IrInst::Load { dst: d, ptr: s, ty: IrType::I64 });
        d
    }
}

/// 生成 `__dial_hof_<arity><elem><ret>` thunk（独立函数，追加到 ip.functions）。
/// arity=1: fn(elem, state): ret；arity=2: fn(elem, elem, state): ret。
fn gen_hof_thunk(ip: &mut IrProgram, elem: &Type, ret: &Type, arity: usize) -> Result<(), String> {
    let name = format!("__dial_hof_{}_{}_{}", arity, hof_code(elem), hof_code(ret));
    if ip.functions.iter().any(|f| f.name == name) {
        return Ok(());
    }
    // thunk 自身签名（C ABI）：(i64 a, i64 b, ptr fn_raw, ptr state) -> i64。
    let params: Vec<(String, IrType)> = vec![
        ("a".into(), IrType::I64),
        ("b".into(), IrType::I64),
        ("fn".into(), IrType::Ptr),
        ("state".into(), IrType::Ptr),
    ];
    let mut f = IrFunction::new(name.clone(), params, IrType::I64, FuncLink::Private);
    f.blocks.push(IrBlock { id: 0, params: Vec::new(), insts: Vec::new(), term: Term::Unreachable });
    f.entry = 0;

    // 4 个 C ABI 参数各一个栈槽，按序登记到 param_slots 供 prologue 填值。
    let mut arg_regs: Vec<VReg> = Vec::new();
    for ty in [IrType::I64, IrType::I64, IrType::Ptr, IrType::Ptr] {
        let slot = f.new_reg(IrType::Ptr);
        f.blocks[0].insts.push(IrInst::Alloca { dst: slot, bytes: 8 });
        f.param_slots.push(slot);
        let v = f.new_reg(ty.clone());
        f.blocks[0].insts.push(IrInst::Load { dst: v, ptr: slot, ty });
        arg_regs.push(v);
    }
    let a_bits = arg_regs[0];
    let b_bits = arg_regs[1];
    let fn_raw = arg_regs[2];
    let state = arg_regs[3];

    // i64 位模式 → 回调实参（C ABI 宽度），arity=2 时两个 elem 参数。
    let (a_val, a_ty) = hof_bits_to_cb_arg(&mut f, a_bits, elem);
    let mut args = vec![a_val];
    let mut arg_tys = vec![a_ty];
    if arity == 2 {
        let (b_val, b_ty) = hof_bits_to_cb_arg(&mut f, b_bits, elem);
        args.push(b_val);
        arg_tys.push(b_ty);
    }
    // 第 3 个回调参数是 rawptr<void> 状态（Ptr），原样透传。
    args.push(Value::Reg(state));
    arg_tys.push(IrType::Ptr);

    // 间接调用回调：目标来自 fn_raw 寄存器，返回宽度按 C ABI。
    let ret_abi = param_ir(ret);
    let cb_dst = f.new_reg(ret_abi.clone());
    f.blocks[0].insts.push(IrInst::CallIndirect {
        dst: Some(cb_dst), target: fn_raw, args, arg_tys, ret_ty: ret_abi,
    });

    let bits = hof_cb_ret_to_bits(&mut f, cb_dst, ret);
    f.blocks[0].term = Term::Return { val: Some(Value::Reg(bits)) };
    ip.functions.push(f);
    Ok(())
}

struct Lower<'a> {
    ip: &'a mut IrProgram,
    func: IrFunction,
    cur: u32,
    scopes: Vec<HashMap<String, Local>>,
    /// 循环目标栈：(continue 块, break 块, 循环体所在作用域深度)。
    loops: Vec<(u32, u32, usize)>,
    /// 已登记的 defer 语句：(登记时作用域深度, 语句)。LIFO 触发。
    defers: Vec<(usize, Stmt)>,
    /// 活跃 try 帧的「进入前作用域深度」，throw 时只排空本帧及内层的 defer。
    try_frames: Vec<usize>,
    /// 与 try_frames 平行：每个活跃 try 的 exbuf 槽地址，供提前 return 时弹栈并释放。
    try_bufs: Vec<VReg>,
    /// 函数内标签 → 基本块（goto/label 用）。
    labels: HashMap<String, u32>,
    /// 函数内标签 → 作用域深度（来自 ip.goto_depths，goto 跨作用域释放时按深度消费）。
    label_depths: HashMap<String, usize>,
    /// 函数参数槽（与 func.params 平行），供 prologue 填值。
    param_slots: Vec<VReg>,
    /// M7-e3w：-g 时才插入 Line/DebugVar 调试标记。
    debug_info: bool,
    /// M7-e3ag：packed 结构体名集合，供布局内建按 C ABI 口径计算（packed align=1）。
    packed: HashSet<String>,
    /// M7-e3ai：当前语句的源码行列（文件级），供 assert/panic 的
    /// 「file:line:col」Fail-Fast 诊断使用。对齐 LLVM 后端 cur_pos（在 gen_stmt
    /// 入口 set crate::semantic::stmt_pos(stmt)）。仅在 lower_stmt 入口更新，
    /// 不产生任何 IR，也不影响 -g / 非 -g 的既有指令流。
    cur_pos: (usize, usize),
    /// M7-e3ai：当前编译单元的源文件路径（原始字符串，与 LLVM 后端 self.src_file
    /// 同源），供 assert/panic 的位置诊断作为编译期常量串写入 .rodata。
    src_file: String,
    /// M7-e3am：本编译单元内被 spawn 引用的线程入口函数名集合。thunk 无法在
    /// 当前函数体内生成（它是独立函数），故只记录名字，由 lower_program 在
    /// 遍历完所有函数后统一为每个目标生成 `__dial_thunk_<fn>` 追加到 ip.functions。
    spawn_targets: HashSet<String>,
    /// M7-e3an：本编译单元内需要的高阶回调 thunk 描述 (elem, ret, arity)。
    /// 与 spawn_targets 同理（独立函数），由 lower_program 统一生成；Type 未实现
    /// Hash，故用 Vec 保存，生成时按 thunk 名字去重 + 排序保证 .s 确定性。
    hof_thunks: Vec<(Type, Type, usize)>,
}

impl<'a> Lower<'a> {
    fn new(ip: &'a mut IrProgram, debug_info: bool, packed: &HashSet<String>,
           src_file: &str) -> Self {
        let label_depths = ip.goto_depths.clone();
        Lower {
            ip,
            debug_info,
            packed: packed.clone(),
            cur_pos: (0, 0),
            src_file: src_file.to_string(),
            spawn_targets: HashSet::new(),
            hof_thunks: Vec::new(),
            func: IrFunction::new(String::new(), Vec::new(), IrType::Void, FuncLink::Private),
            cur: 0,
            scopes: Vec::new(),
            loops: Vec::new(),
            defers: Vec::new(),
            try_frames: Vec::new(),
            try_bufs: Vec::new(),
            labels: HashMap::new(),
            label_depths,
            param_slots: Vec::new(),
        }
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("[asm 后端] {}", msg))
    }

    // ---- 块 / 指令构造 ----

    fn new_block(&mut self, params: Vec<(VReg, IrType)>) -> u32 {
        let id = self.func.blocks.len() as u32;
        self.func.blocks.push(IrBlock { id, params, insts: Vec::new(), term: Term::Unreachable });
        id
    }

    fn set_cur(&mut self, id: u32) { self.cur = id; }

    fn emit(&mut self, inst: IrInst) {
        self.func.blocks[self.cur as usize].insts.push(inst);
    }

    fn terminate(&mut self, term: Term) {
        self.func.blocks[self.cur as usize].term = term;
    }

    fn new_reg(&mut self, ty: IrType) -> VReg { self.func.new_reg(ty) }

    fn cur_terminated(&self) -> bool {
        !matches!(self.func.blocks[self.cur as usize].term, Term::Unreachable)
    }

    // ---- 作用域 / 变量 ----

    fn push_scope(&mut self) { self.scopes.push(HashMap::new()); }

    fn pop_scope(&mut self) -> HashMap<String, Local> { self.scopes.pop().unwrap() }

    fn declare(&mut self, name: &str, loc: Local) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), loc);
    }

    fn lookup(&self, name: &str) -> Option<&Local> {
        for s in self.scopes.iter().rev() {
            if let Some(l) = s.get(name) { return Some(l); }
        }
        None
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut Local> {
        for s in self.scopes.iter_mut().rev() {
            if let Some(l) = s.get_mut(name) { return Some(l); }
        }
        None
    }

    /// 释放一个作用域里所有存活且未 moved 的堆局部（逆序）。
    fn release_scope(&mut self, scope: &HashMap<String, Local>) {
        let mut items: Vec<&Local> = scope.values().collect();
        items.sort_by_key(|l| l.ptr);
        for l in items.iter().rev() {
            if l.is_handle && !l.moved {
                if let Some(k) = rc_kind(&l.dt) {
                    self.emit(IrInst::RcRelease { ptr: l.ptr, kind: k });
                } else if let Type::Struct(n) = &l.dt {
                    self.emit(IrInst::StructRelease { ptr: l.ptr, struct_name: n.clone() });
                }
            }
        }
    }

    /// 释放 `down_to` 及以上所有作用域里的堆局部（Return / break / continue 用）。
    ///
    /// **只发射 release 指令，绝不 pop 作用域**：Return 之后同一函数里可能还有别的语句
    /// （例如 `if (c) { return x; }` 之后的收尾），它们仍需能 lookup 到参数；而 `If` /
    /// `While` 的 `pop_scope().unwrap()` 也要求作用域结构完整。被释放的局部标记 moved，
    /// 保证随后 pop_scope 时的 release_scope 不会二次释放。
    fn release_locals_upto(&mut self, down_to: usize) {
        if self.scopes.len() <= down_to { return; }
        let mut items: Vec<(usize, String, VReg, Option<ArcKind>, Option<String>)> = Vec::new();
        for si in down_to..self.scopes.len() {
            for (name, l) in &self.scopes[si] {
                if l.is_handle && !l.moved {
                    let sn = if let Type::Struct(n) = &l.dt { Some(n.clone()) } else { None };
                    items.push((si, name.clone(), l.ptr, rc_kind(&l.dt), sn));
                }
            }
        }
        items.sort_by_key(|x| std::cmp::Reverse(x.2)); // 与 release_scope 一致：按槽地址逆序
        for (si, name, ptr, k, sn) in items {
            if let Some(k) = k {
                self.emit(IrInst::RcRelease { ptr, kind: k });
            } else if let Some(sn) = sn {
                self.emit(IrInst::StructRelease { ptr, struct_name: sn });
            }
            if let Some(l) = self.scopes.get_mut(si).and_then(|s| s.get_mut(&name)) {
                l.moved = true;
            }
        }
    }

    /// 离开若干作用域：先按 LIFO 排空这些作用域的 defer，再释放堆局部。
    fn release_scopes_upto(&mut self, down_to: usize) -> Result<(), String> {
        self.drain_defers_from(down_to + 1)?;
        self.release_locals_upto(down_to);
        Ok(())
    }

    /// LIFO 执行所有「登记深度 >= depth」的 defer（作用域深度 = 登记时的 scopes.len()）。
    ///
    /// **只 clone 发射，绝不消耗 self.defers**（对齐 LLVM codegen.rs:1438）：defer 是
    /// 编译期静态结构，break/continue/return 的提前出口与正常落穿两条路径互斥，各自
    /// 发射自己那一份 clone，运行期只走其中一条。此前用 pop 消耗，导致循环体里
    /// break/continue 展开后，同一循环体的正常落穿路径拿到空 defer → 漏发（defer_demo
    /// 的 iter.end / inner.end 在部分迭代消失）。原 defer 由 leave_scope 在真正离开
    /// 作用域时统一清理。
    fn drain_defers_from(&mut self, depth: usize) -> Result<(), String> {
        let to_run: Vec<Stmt> = self.defers.iter().rev()
            .filter(|(d, _)| *d >= depth)
            .map(|(_, st)| st.clone())
            .collect();
        for st in to_run {
            self.lower_stmt(&st)?;
        }
        Ok(())
    }

    /// 离开当前最内层作用域：LIFO defer + 释放该层堆局部。
    fn leave_scope(&mut self) -> Result<(), String> {
        let d = self.scopes.len();
        // 当前块已终止（前一条是 return/break/continue/goto/throw）：正常落穿路径不可达，
        // 该出口的 defer 已由 drain_defers_from 在跳转前发射过。此处只清理 defer 帧，
        // 切勿再往已终止的块追加指令（否则 defer 被发射两次——goto_demo 的 cleanup 重复）。
        // 对齐 LLVM codegen.rs:1426 的 is_terminated 保护。
        if !self.cur_terminated() {
            self.drain_defers_from(d)?;
        }
        // drain 只 clone 不消耗；此处（真正退出作用域）清理本层及更深的 defer。
        self.defers.retain(|(dep, _)| *dep < d);
        let sc = self.scopes.pop().unwrap();
        self.release_scope(&sc);
        Ok(())
    }

    // ---- 函数 ----

    fn lower_function(&mut self, f: &Func) -> Result<(), String> {
        let is_main = f.name == "main";
        let linkage = if is_main || f.exported { FuncLink::External } else { FuncLink::Private };
        let ret_ir = if is_main { IrType::I64 } else { param_ir(&f.ret) };
        let mut params: Vec<(String, IrType)> = Vec::new();
        if is_main {
            params.push(("argc".to_string(), IrType::I64));
            params.push(("argv".to_string(), IrType::Ptr));
        } else {
            for (n, t) in &f.params { params.push((n.clone(), param_ir(t))); }
        }
        self.func = IrFunction::new(f.name.clone(), params, ret_ir.clone(), linkage);
        self.func.decl_line = f.line as u32; // M7-e3w：-g DW_AT_decl_line
        self.scopes.clear();
        self.loops.clear();
        self.defers.clear();
        self.try_frames.clear();
        self.try_bufs.clear();
        self.labels.clear();
        self.param_slots.clear();

        let entry = self.new_block(Vec::new());
        self.set_cur(entry);
        self.func.entry = entry;

        // 参数槽：每个参数一个栈槽，prologue 把实参寄存器填进去。
        self.push_scope();
        if is_main {
            let a_ptr = self.emit_alloca(IrType::I64);
            let v_ptr = self.emit_alloca(IrType::Ptr);
            self.declare("argc", Local { ptr: a_ptr, ty: IrType::I64, dt: Type::Int, is_handle: false, moved: false });
            self.declare("argv", Local { ptr: v_ptr, ty: IrType::Ptr, dt: Type::RawPtr(Box::new(Type::Byte)), is_handle: false, moved: false });
            self.param_slots.push(a_ptr);
            self.param_slots.push(v_ptr);
            // EnvInit 的实参是「值」而非槽地址：prologue 把 argc/argv 写进槽后，先 Load 再调用。
            let argc_v = self.new_reg(IrType::I64);
            self.emit(IrInst::Load { dst: argc_v, ptr: a_ptr, ty: IrType::I64 });
            let argv_v = self.new_reg(IrType::Ptr);
            self.emit(IrInst::Load { dst: argv_v, ptr: v_ptr, ty: IrType::Ptr });
            self.emit(IrInst::EnvInit { argc: Value::Reg(argc_v), argv: Value::Reg(argv_v) });
        } else {
            for (i, (n, t)) in f.params.iter().enumerate() {
                // v1.21 consume 形参：`own x: T` 由调用方移交所有权，入口**不** retain
                // （普通参数是共享所有权→入口 +1、退出 -1；own 参数直接接管调用方那份）。
                // 退出时 release_scope 仍会 -1，与调用方 mark_moved 后不再 release 正好配对。
                let is_own_param = f.own_params.get(i).copied().unwrap_or(false);
                // 内部宽度统一用 reg_ir（I64），栈槽 8 字节；emit 的 prologue 把
                // 边界宽度的实参寄存器填进来（写 32/8 位会自动零扩展到 64 位）。
                if let Type::Struct(sn) = t {
                    // 结构体形参按**值**传递（对齐 LLVM 后端 codegen.rs:2152 的
                    // alloca + store 值拷贝语义）：调用方传「内容区地址」，
                    // 本函数把它 memcpy 进自己的内容区，得到独立副本——
                    // 于是被调方改形参不会影响调用方（值语义），且函数返回后
                    // 调用方的栈帧销毁也不影响被调方正在使用的副本。
                    //
                    // 实现上分两段：① 参数槽（8 字节）先由 prologue 写入传入的
                    // 内容区地址；② 函数体开头据此 memcpy 出本函数的内容区。
                    // 变量名绑定到**内容区**（Local.ptr 语义 = 内容区地址），
                    // 与局部结构体变量的表示完全一致。
                    let ptr_slot = self.emit_alloca(IrType::Ptr);
                    let content = self.emit_alloca(IrType::Struct(sn.clone()));
                    let addr = self.new_reg(IrType::Ptr);
                    self.emit(IrInst::Load { dst: addr, ptr: ptr_slot, ty: IrType::Ptr });
                    self.emit_memcpy(content, Value::Reg(addr), self.struct_byte_size(t));
                    self.declare(n, Local {
                        ptr: content, ty: reg_ir(t), dt: t.clone(), is_handle: false, moved: false,
                    });
                    self.param_slots.push(ptr_slot);
                    // 值拷贝本身不带所有权：内容区里的堆字段需对称 +1（入口），
                    // 作用域退出时由 release_scope 递归 -1（与 LLVM 的
                    // retain_struct_fields / release_struct_fields 对称）。
                    // own 形参跳过入口 +1（接管调用方那份）。
                    if !is_own_param {
                        self.emit(IrInst::StructRetain { ptr: content, struct_name: sn.clone() });
                    }
                    continue;
                }
                let slot = self.emit_alloca(reg_ir(t));
                let is_h = rc_kind(t).is_some();
                self.declare(n, Local { ptr: slot, ty: reg_ir(t), dt: t.clone(), is_handle: is_h, moved: false });
                self.param_slots.push(slot);
                // 堆形参入口 retain（与调用方共享所有权，对称借用）；
                // own 形参跳过——所有权已由调用方移交，本函数退出时负责释放。
                if !is_own_param {
                    if let Some(k) = rc_kind(t) {
                        self.emit(IrInst::RcRetain { ptr: slot, kind: k });
                    }
                }
            }
        }

        // 函数体是扁平语句序列：一旦当前块被 return / throw 终结，后续非标签语句是死代码，
        // 必须停止 lower（否则 `terminate` 会用后面的 return 覆盖前面的，导致错误返回——
        // 这正是 M3-a2 早返回修复）。但 `label` 例外：goto 会把当前块终结为 Jump，
        // 而紧随其后的 `label`/`goto` 目标块仍可达（goto 跳出后由标签重新定位 cur），
        // 绝不能当作死代码跳过，否则 `done` 块会停在 `ud2`（见 control_flow.dial）。
        for s in &f.body {
            if self.cur_terminated() && !matches!(s, Stmt::Label(..)) {
                break;
            }
            self.lower_stmt(s)?;
        }

        if !self.cur_terminated() {
            self.release_scopes_upto(0)?;
            if is_main {
                self.terminate(Term::Return { val: Some(Value::Imm(Imm::I64(0))) });
            } else if ret_ir == IrType::Void {
                self.terminate(Term::Return { val: None });
            } else {
                // 非 void 函数缺少显式 return：保守补零值（Dial 语义本应拒绝）。
                let zero = match ret_ir {
                    IrType::I64 => Some(Value::Imm(Imm::I64(0))),
                    IrType::I32 => Some(Value::Imm(Imm::I32(0))),
                    IrType::F64 => Some(Value::Imm(Imm::F64(0))),
                    IrType::F32 => Some(Value::Imm(Imm::F32(0))),
                    _ => Some(Value::Imm(Imm::I64(0))),
                };
                self.terminate(Term::Return { val: zero });
            }
        }

        self.func.param_slots = self.param_slots.clone();
        self.ip.functions.push(self.func.clone());
        Ok(())
    }

    // ---- 内存辅助 ----

    fn emit_alloca(&mut self, ty: IrType) -> VReg {
        let bytes = match &ty {
            // 值语义结构体：槽要能装下整个结构体（按 C ABI 布局），否则字段写入会越界。
            IrType::Struct(n) => self.struct_byte_size(&Type::Struct(n.clone())) as u32,
            _ => slot_bytes(ty),
        };
        let p = self.new_reg(IrType::Ptr);
        self.emit(IrInst::Alloca { dst: p, bytes });
        p
    }

    /// M7-e3az：分配一个 simd 向量栈段（N 个 lane × 8 字节），返回段首指针 vreg。
    fn emit_simd_alloca(&mut self, n: u32) -> VReg {
        let p = self.new_reg(IrType::Ptr);
        self.emit(IrInst::Alloca { dst: p, bytes: n * 8 });
        p
    }

    /// M7-e3az：取向量段 `base` 的第 `lane` 个元素地址 = base + lane*8（字节）。
    fn simd_lane_addr(&mut self, base: VReg, lane: u32) -> VReg {
        let p = self.new_reg(IrType::Ptr);
        self.emit(IrInst::PtrAdd {
            dst: p, base, off: Value::Imm(Imm::I64(lane as i64 * 8)),
        });
        p
    }

    /// M7-e3az：把 lane 值按元素标量宽度写入向量段的第 `lane` 槽。
    fn simd_store_lane(&mut self, base: VReg, lane: u32, val: Value, ty: IrType) {
        let addr = self.simd_lane_addr(base, lane);
        self.emit(IrInst::Store { ptr: addr, val, ty });
    }

    /// M7-e3az：从向量段第 `lane` 槽读出 lane 值（按元素标量宽度）。
    fn simd_load_lane(&mut self, base: VReg, lane: u32, ty: IrType) -> VReg {
        let addr = self.simd_lane_addr(base, lane);
        let r = self.new_reg(ty.clone());
        self.emit(IrInst::Load { dst: r, ptr: addr, ty });
        r
    }

    /// 把一个值写入槽（含 ARC）。
    ///
    /// 时序铁律：**先 Store，再 retain 新值**——retain 作用于槽里刚写进去的句柄。
    /// `release_old` 仅对「已有旧值的槽」（赋值）为 true；VarDecl 的槽是刚分配的，
    /// 对其 release 会对未初始化的垃圾指针调用 release，属严重 UB。
    fn store_value(&mut self, slot: VReg, val: Value, val_is_copy: bool, val_dt: &Type, release_old: bool) {
        if release_old {
            if let Some(k) = rc_kind(val_dt) {
                self.emit(IrInst::RcRelease { ptr: slot, kind: k });
            }
        }
        self.emit(IrInst::Store { ptr: slot, val, ty: self.val_slot_ty(val_dt) });
        if val_is_copy {
            if let Some(k) = rc_kind(val_dt) {
                self.emit(IrInst::RcRetain { ptr: slot, kind: k });
            }
        }
    }

    fn val_slot_ty(&self, dt: &Type) -> IrType {
        match dt { Type::Struct(_) => IrType::Ptr, other => reg_ir(other) }
    }

    // ---- 语句 ----

    /// M7-e3w：提取语句源码行号（AST 各 Stmt 变体的行号位置不统一）。
    /// 仅用于 -g 调试的行标记；非 -g 编译时这些 Line 指令会被发射器跳过。
    fn stmt_line(s: &Stmt) -> u32 {
        let line = match s {
            Stmt::VarDecl { line, .. } | Stmt::Assign { line, .. }
            | Stmt::AssignDeref { line, .. } | Stmt::AssignField { line, .. }
            | Stmt::AssignIndex { line, .. } | Stmt::If { line, .. }
            | Stmt::While { line, .. } | Stmt::For { line, .. }
            | Stmt::ForEach { line, .. } | Stmt::ForEachMap { line, .. }
            | Stmt::DoWhile { line, .. } | Stmt::Match { line, .. }
            | Stmt::With { line, .. } | Stmt::Try { line, .. }
            | Stmt::Select { line, .. } | Stmt::Block { line, .. } => *line,
            Stmt::Break(l, _) | Stmt::Continue(l, _) | Stmt::Label(_, l, _)
            | Stmt::Goto(_, l, _) | Stmt::Return(_, l, _) | Stmt::Throw(_, l, _)
            | Stmt::Expr(_, l, _) | Stmt::Defer(_, l, _) => *l,
        };
        line as u32
    }

    fn lower_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        // M7-e3ai：无条件记录当前语句行列，供 assert/panic 的位置诊断读取。
        // 纯内存赋值、不 emit 任何 IR，对齐 LLVM 后端 gen_stmt 入口的 cur_pos.set。
        // 只有真正调用 assert/panic 时该值才会被消费，对其它程序零影响。
        self.cur_pos = crate::semantic::stmt_pos(s);
        // M7-e3w：每条语句起点插一条行标记，行号随指令流经 pass（重排不丢失）。
        // 仅 -g：非 -g 下这些标记会打断折叠/融合 pass 的模式窗口，必须完全不插入。
        if self.debug_info {
            self.emit(IrInst::Line(Self::stmt_line(s)));
        }
        match s {
            Stmt::VarDecl { name, ty, value, move_hint, line, .. } => {
                let dt = ty.clone();
                let slot = self.emit_alloca(reg_ir(&dt));
                // M7-e3w：-g 调试变量名标记（非 -g 完全不插入，保证 IR 与 e3v 一致）。
                if self.debug_info {
                    self.emit(IrInst::DebugVar { name: name.clone(), slot, line: *line as u32, ty: dt.clone() });
                }
                let (v, is_copy) = self.lower_expr(value, &dt)?;
                if let Type::Struct(_) = &dt {
                    // 值语义结构体：StructLit 返回一块临时栈区地址，整体拷进变量槽。
                    self.emit_memcpy(slot, v, self.struct_byte_size(&dt));
                } else {
                    // M7-e3ap：对齐 LLVM 后端 retain_if_borrowed（codegen.rs:1970）。
                    // lower_expr 返回的 is_copy 对 Expr::Var/Field 为 true，但对
                    // Expr::Index / Method(get,at) 误返回 false——它们同样是「借用别名」：
                    // 源容器仍持有该元素，绑到本变量须 retain 一份，否则作用域退出时
                    // release 会把 rc 多减 1（`var row = m[i]` / `var row = m.get(i)`）。
                    // 用 yields_borrowed_ref(value) 兜住这一缺口；move 语义下所有权已转移，
                    // 不应 retain（对齐 LLVM 的 `!move_hint` 条件）。
                    // v1.21 修「假 move」：对齐 LLVM 后端 `if !move_hint { retain_if_borrowed }`。
                    // 原 `is_copy || (borrowed && !move_hint)` 中 is_copy 对 Expr::Var/Field/Deref
                    // 为 true，会短路 move_hint，导致 move（含 own 独占转移）仍 emit retain——
                    // 于是「零开销独占」名不副实，普通 var 的最后引用 move 优化也失效。
                    // 正确口径：move_hint 是总开关，move 时一律不 retain（所有权已随位模式移交）。
                    let need_retain = !*move_hint && (is_copy || yields_borrowed_ref(value));
                    self.store_value(slot, v, need_retain, &dt, false);
                }
                let is_h = rc_kind(&dt).is_some();
                let mut loc = Local { ptr: slot, ty: reg_ir(&dt), dt: dt.clone(), is_handle: is_h, moved: *move_hint };
                if *move_hint { loc.moved = true; }
                self.declare(name, loc);
                Ok(())
            }
            Stmt::Assign { name, value, move_hint, .. } => {
                let l = match self.lookup(name).cloned() {
                    Some(l) => l,
                    None => return self.err(&format!("未声明的变量 '{}'", name)),
                };
                if let Type::Struct(_) = l.dt {
                    let (src, _) = self.lower_expr(value, &l.dt)?;
                    self.emit_memcpy(l.ptr, src, self.struct_byte_size(&l.dt));
                    if *move_hint {
                        if let Expr::Var(src_name) = value {
                            if let Some(sl) = self.scopes.last_mut().unwrap().get_mut(src_name) { sl.moved = true; }
                        }
                    }
                    return Ok(());
                }
                let dt = l.dt.clone();
                let (v, is_copy) = self.lower_expr(value, &dt)?;
                // M7-e3ap：同 VarDecl，补齐借用别名（Index/get/at）绑变量的 retain。
                // v1.21 修「假 move」：同 VarDecl，move_hint 为总开关，move 时不再 retain。
                let need_retain = !*move_hint && (is_copy || yields_borrowed_ref(value));
                self.store_value(l.ptr, v, need_retain, &dt, true);
                if let Some(sl) = self.scopes.last_mut().unwrap().get_mut(name) { sl.moved = *move_hint; }
                if *move_hint {
                    if let Expr::Var(src_name) = value {
                        if let Some(sl) = self.scopes.last_mut().unwrap().get_mut(src_name) { sl.moved = true; }
                    }
                }
                Ok(())
            }
            Stmt::AssignField { obj, field, value, .. } => {
                // 基址分两种（对齐 LLVM 后端 codegen.rs:3990 的 field_base_ptr）：
                //   · obj 是裸指针：基址是**指针值**本身（写进 C 侧内存）；
                //   · obj 是变量：基址是该结构体变量的内容区。
                let obj_ty = self.expr_dt(obj);
                let is_raw = matches!(obj_ty, Type::RawPtr(_) | Type::VolatilePtr(_));
                let (base_ptr, base_dt) = if is_raw {
                    let (bv, _) = self.lower_expr(obj, &obj_ty)?;
                    let st = match &obj_ty {
                        Type::RawPtr(e) | Type::VolatilePtr(e) => (**e).clone(),
                        _ => obj_ty.clone(),
                    };
                    (bv.as_reg(), st)
                } else {
                    let obj_name = match obj.as_ref() {
                        Expr::Var(n) => n.clone(),
                        _ => return self.err("asm 后端：AssignField 的 obj 仅支持对变量或裸指针取字段"),
                    };
                    let base_loc = match self.lookup(&obj_name).cloned() {
                        Some(l) => l,
                        None => return self.err(&format!("未声明的变量 '{}'", obj_name)),
                    };
                    (base_loc.ptr, base_loc.dt)
                };
                let (field_off, field_ty) = self.struct_field(&base_dt, field)
                    .ok_or_else(|| format!("[asm 后端] 结构体 {} 无字段 {}", base_dt.display(), field))?;
                let fa = self.new_reg(IrType::Ptr);
                self.emit(IrInst::FieldAddr { dst: fa, base: base_ptr, struct_name: struct_name_of(&base_dt), field: field_off as u32 });
                let (v, is_copy) = self.lower_expr(value, &field_ty)?;
                if let Type::Struct(_) = &field_ty {
                    // 嵌套结构体字段：按值拷贝内容（v 是内容区地址）。
                    self.emit_memcpy(fa, v, self.struct_byte_size(&field_ty));
                    self.emit(IrInst::StructRetain { ptr: fa, struct_name: struct_name_of(&field_ty) });
                    return Ok(());
                }
                if let Some(k) = rc_kind(&field_ty) {
                    self.emit(IrInst::RcRelease { ptr: fa, kind: k });
                }
                // 字段内存宽度按 struct 类型分口径：extern struct 的 byte=I8(1B)，
                // 普通 struct 的 byte=I64(8B)；与 struct_layout 偏移同口径。
                let mty = field_mem_ir(&self.ip.cstruct_names, &base_dt, &field_ty);
                let v_store = if mty != reg_ir(&field_ty) {
                    let t = self.new_reg(mty.clone());
                    self.emit(IrInst::Conv { kind: AConv::Trunc, dst: t, a: v, ty: mty.clone() });
                    Value::Reg(t)
                } else { v };
                self.emit(IrInst::Store { ptr: fa, val: v_store, ty: mty });
                // retain 必须在 Store 之后：作用于刚写进字段的新句柄。
                if is_copy {
                    if let Some(k) = rc_kind(&field_ty) {
                        self.emit(IrInst::RcRetain { ptr: fa, kind: k });
                    }
                }
                Ok(())
            }
            Stmt::AssignDeref { ptr, value, .. } => {
                // `*p = v`：p 是裸指针（rawptr<T> / volatile rawptr<T>），
                // 写入值的类型由语义层核对过；这里把 Dial 值转成内存表示再写。
                let pt = self.expr_dt(ptr);
                let elem = match &pt {
                    Type::RawPtr(e) | Type::VolatilePtr(e) => (**e).clone(),
                    other => return self.err(&format!(
                        "[asm 后端] '*p = v' 的左侧收到 {} 类型（仅支持裸指针）", other.display())),
                };
                let (pv, _) = self.lower_expr(ptr, &pt)?;
                let vdt = self.expr_dt(value);
                let (vv, _) = self.lower_expr(value, &vdt)?;
                // 内存宽度：byte=1B、bool=1B、char=4B、其余 8B（与 LLVM 的 raw_to_mem 同口径）。
                let mty = match &elem {
                    Type::Byte | Type::Bool => IrType::I8,
                    Type::Char => IrType::I32,
                    other => reg_ir(other),
                };
                let v_store = if mty != reg_ir(&elem) {
                    let t = self.new_reg(mty.clone());
                    // byte / bool / char 都是「取低字节或低 4 字节」：内部宽度是 I64，
                    // 内存宽度更窄，截断即可（bool 的内部值本就是 0/1）。
                    self.emit(IrInst::Conv { kind: AConv::Trunc, dst: t, a: vv, ty: mty.clone() });
                    Value::Reg(t)
                } else { vv };
                self.emit(IrInst::Store { ptr: pv.as_reg(), val: v_store, ty: mty });
                Ok(())
            }
            Stmt::AssignIndex { arr, index, value, .. } => {
                // M7-e3ao：嵌套下标赋值（a[i][j] = v）——arr 本身是 Expr::Index，
                // 交给递归左值解析（对齐 LLVM gen_assign_lvalue，codegen.rs:2450）：
                // 逐层 dial_array_set_borrowed（不消耗借来的内层数组）+ 写回外层容器，
                // 精确结算每层 value 所有权。单层 a[i] = v（arr 为变量）仍走下方既有
                // 逻辑——int[] 既有测试路径与汇编输出完全不变。
                if matches!(arr.as_ref(), Expr::Index(..)) {
                    let full = Expr::Index(arr.clone(), index.clone());
                    let vdt = self.expr_dt(value);
                    let (vv, _) = self.lower_expr(value, &vdt)?;
                    let borrowed = yields_borrowed_ref(value);
                    return self.lower_assign_lvalue(&full, vv, &vdt, borrowed);
                }
                // 语义等价于 `arr = dial_array_set(arr, i, v)`：C 侧在共享时会 COW
                // 克隆并释放本调用者的旧引用，因此**只能写回新句柄，绝不能再 release
                // 旧值**（否则悬垂 / 双重释放）。见 codegen.rs:2727 的所有权契约注释。
                let name = match arr.as_ref() {
                    Expr::Var(n) => n.clone(),
                    _ => return self.err("asm 后端：索引赋值目标仅支持变量（嵌套下标后续相位）"),
                };
                let l = match self.lookup(&name).cloned() {
                    Some(x) => x,
                    None => return self.err(&format!("未声明的变量 '{}'", name)),
                };
                if !matches!(l.dt, Type::Array(_)) {
                    return self.err("asm 后端：索引赋值仅支持数组");
                }
                let cur = self.new_reg(IrType::Handle);
                self.emit(IrInst::Load { dst: cur, ptr: l.ptr, ty: IrType::Handle });
                let (iv, _) = self.lower_expr(index, &Type::Int)?;
                let vdt = self.expr_dt(value);
                let (vv, _) = self.lower_expr(value, &vdt)?;
                let bits = self.coerce_elem_bits(vv, &vdt)?;
                let newp = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call {
                    dst: Some(newp), func: "dial_array_set".to_string(),
                    args: vec![Value::Reg(cur), iv, bits],
                    arg_tys: vec![IrType::Handle, IrType::I64, IrType::I64],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime,
                });
                // 元素归属结算（对齐 LLVM 后端 codegen.rs:2743 的 sink_elem_after_retain）：
                // C 侧 dial_array_set 已为元素 retain 一份（数组成为该元素的持有者）。
                //   · 借来的值（变量 / 字段 / 数组元素 / get / at）：来源仍持有，
                //     不得再释放，否则悬垂 / 双重释放；
                //   · 自带所有权的值（字面量 / clone / 方法返回值等）：本语句的临时
                //     所有权应当移交给数组，故此处归还一份，否则每轮循环泄漏一次引用。
                if matches!(&vdt, Type::Struct(_)) {
                    // v1.10 B6：struct 值 coerce 时无条件新建 box，array_set 又 +1，归还本地这份。
                    self.release_box(bits.clone());
                } else if let Some(k) = rc_kind(&vdt) {
                    if !yields_borrowed_ref(value) {
                        // RcRelease 作用于「槽地址」（内部 load 出句柄值再 release），
                        // 故先把句柄落进临时槽。
                        let tmp = self.emit_alloca(reg_ir(&vdt));
                        self.emit(IrInst::Store { ptr: tmp, val: vv.clone(), ty: reg_ir(&vdt) });
                        self.emit(IrInst::RcRelease { ptr: tmp, kind: k });
                    }
                }
                // 只写回，不做 ARC（旧引用已由 C 侧结算）。
                self.emit(IrInst::Store { ptr: l.ptr, val: Value::Reg(newp), ty: IrType::Handle });
                Ok(())
            }
            Stmt::If { cond, then_branch, else_branch, .. } => {
                let (cond_v, _) = self.lower_expr(cond, &Type::Bool)?;
                let then_b = self.new_block(Vec::new());
                // 无 else 时也必须新建一个空块；复用 0 号块会覆盖 entry 的 terminator。
                let else_b = self.new_block(Vec::new());
                let merge_b = self.new_block(Vec::new());
                let cond = self.cond_reg(cond_v)?;
                self.terminate(Term::Branch { cond, then_b, else_b, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(then_b);
                self.push_scope();
                for st in then_branch { self.lower_stmt(st)?; }
                self.leave_scope()?;
                if !self.cur_terminated() { self.terminate(Term::Jump { target: merge_b, args: Vec::new() }); }
                if let Some(eb) = else_branch {
                    self.set_cur(else_b);
                    self.push_scope();
                    for st in eb { self.lower_stmt(st)?; }
                    self.leave_scope()?;
                    if !self.cur_terminated() { self.terminate(Term::Jump { target: merge_b, args: Vec::new() }); }
                } else {
                    self.set_cur(else_b);
                    self.terminate(Term::Jump { target: merge_b, args: Vec::new() });
                }
                self.set_cur(merge_b);
                Ok(())
            }
            Stmt::While { cond, body, .. } => {
                let header = self.new_block(Vec::new());
                let body_b = self.new_block(Vec::new());
                let exit = self.new_block(Vec::new());
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(header);
                let (cond_v, _) = self.lower_expr(cond, &Type::Bool)?;
                let cond = self.cond_reg(cond_v)?;
                self.terminate(Term::Branch { cond, then_b: body_b, else_b: exit, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(body_b);
                let body_depth = self.scopes.len();
                self.push_scope();
                self.loops.push((header, exit, body_depth));
                for st in body { self.lower_stmt(st)?; }
                self.loops.pop();
                self.leave_scope()?;
                if !self.cur_terminated() { self.terminate(Term::Jump { target: header, args: Vec::new() }); }
                self.set_cur(exit);
                Ok(())
            }
            Stmt::DoWhile { body, cond, .. } => {
                let body_b = self.new_block(Vec::new());
                let cond_b = self.new_block(Vec::new());
                let exit = self.new_block(Vec::new());
                self.terminate(Term::Jump { target: body_b, args: Vec::new() });
                self.set_cur(body_b);
                let body_depth = self.scopes.len();
                self.push_scope();
                self.loops.push((cond_b, exit, body_depth));
                for st in body { self.lower_stmt(st)?; }
                self.loops.pop();
                self.leave_scope()?;
                if !self.cur_terminated() { self.terminate(Term::Jump { target: cond_b, args: Vec::new() }); }
                self.set_cur(cond_b);
                let (cond_v, _) = self.lower_expr(cond, &Type::Bool)?;
                let cond = self.cond_reg(cond_v)?;
                self.terminate(Term::Branch { cond, then_b: body_b, else_b: exit, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(exit);
                Ok(())
            }
            Stmt::For { var, start, end, step, body, .. } => {
                let dt = Type::Int;
                let slot = self.emit_alloca(IrType::I64);
                let (sv, _) = self.lower_expr(start, &dt)?;
                self.emit(IrInst::Store { ptr: slot, val: sv, ty: IrType::I64 });
                self.declare(var, Local { ptr: slot, ty: IrType::I64, dt: dt.clone(), is_handle: false, moved: false });
                let step_v = match step {
                    Some(e) => self.lower_expr(e, &dt)?.0,
                    None => Value::Imm(Imm::I64(1)),
                };
                let header = self.new_block(Vec::new());
                let body_b = self.new_block(Vec::new());
                // continue 目标必须是独立的自增块（inc_b）而非条件块（header）：
                // 若 continue 跳 header，自增指令（内联在 body 尾部）会被跳过，循环变量
                // 永不变化 → 死循环。这是 loop_control.dial 注释明确点出的正确性点。
                let inc_b = self.new_block(Vec::new());
                let exit = self.new_block(Vec::new());
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(header);
                let cv = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: cv, ptr: slot, ty: IrType::I64 });
                let (ev, _) = self.lower_expr(end, &dt)?;
                // 循环条件无条件 i < end，与 LLVM 后端（codegen.rs:2913 恒 SLT）对齐。
                // 此前按 step 常量符号选 Lt/Gt：step 是变量表达式（如 `for i in 0..9 step s`）
                // 时 step_v 非 Imm → 误判为负 → 用 Gt → 初始 i=0 立即退出、循环体从不执行。
                let op = ACmp::Lt;
                let cmp = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op, dst: cmp, a: Value::Reg(cv), b: ev, ty: IrType::I64 });
                self.terminate(Term::Branch { cond: cmp, then_b: body_b, else_b: exit, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(body_b);
                let body_depth = self.scopes.len();
                self.push_scope();
                self.loops.push((inc_b, exit, body_depth));
                for st in body { self.lower_stmt(st)?; }
                self.loops.pop();
                self.leave_scope()?;
                if !self.cur_terminated() {
                    self.terminate(Term::Jump { target: inc_b, args: Vec::new() });
                }
                // 自增块：正常落穿与 continue 都汇入这里，执行 i = i + step 后回到条件判定。
                self.set_cur(inc_b);
                let cv2 = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: cv2, ptr: slot, ty: IrType::I64 });
                let nv = self.new_reg(IrType::I64);
                self.emit(IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(cv2), b: step_v, ty: IrType::I64 });
                self.emit(IrInst::Store { ptr: slot, val: Value::Reg(nv), ty: IrType::I64 });
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(exit);
                Ok(())
            }
            Stmt::ForEach { value, coll, body, line, col, .. } => {
                // 迭代变量的元素类型必须取自数组真实类型：硬编码 Int 会让 string[] 的
                // 元素（句柄位模式）被 print 当整数打印成地址。行参 slot 仍用 I64
                // （句柄/标量位模式都是 64 位），is_handle 保持 false（dial_array_get
                // 返回 borrowed 引用，retain 会 double free）；仅 dt 决定 print 分派。
                let coll_dt = self.expr_dt(coll);
                let elem_dt = match &coll_dt {
                    Type::Array(e) => (**e).clone(),
                    Type::Set(e) => (**e).clone(),
                    _ => Type::Int,
                };
                let (coll_v, _) = self.lower_expr(coll, &coll_dt)?;
                // 集合迭代（for v in set）：成员即 map 的键，先快照成 int[] 再走数组迭代
                // 路径——与 LLVM 后端（codegen.rs:2956 dial_map_keys）对齐。此前直接把
                // set handle 当数组传给 dial_array_get，取出的元素全是 0（set_demo 红）。
                // 快照登记到当前作用域（is_handle=true），函数出口统一 release。
                let arr_v = if matches!(coll_dt, Type::Set(_)) {
                    let keys = self.new_reg(IrType::Handle);
                    self.emit(IrInst::Call { dst: Some(keys), func: "dial_map_keys".to_string(),
                        args: vec![coll_v], arg_tys: vec![IrType::Handle], ret_ty: IrType::Handle,
                        linkage: CallLink::Runtime });
                    let keys_slot = self.emit_alloca(IrType::Handle);
                    self.emit(IrInst::Store { ptr: keys_slot, val: Value::Reg(keys), ty: IrType::Handle });
                    self.declare(&format!("__asm_setkeys_{}_{}", line, col), Local {
                        ptr: keys_slot, ty: IrType::Handle,
                        dt: Type::Array(Box::new(Type::Int)), is_handle: true, moved: false,
                    });
                    Value::Reg(keys)
                } else {
                    coll_v
                };
                let idx_slot = self.emit_alloca(IrType::I64);
                self.emit(IrInst::Store { ptr: idx_slot, val: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                let len = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(len), func: "dial_array_length".to_string(), args: vec![arr_v], arg_tys: vec![IrType::Handle], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let val_slot = self.emit_alloca(IrType::I64);
                self.declare(value, Local { ptr: val_slot, ty: IrType::I64, dt: elem_dt, is_handle: false, moved: false });
                let header = self.new_block(Vec::new());
                let body_b = self.new_block(Vec::new());
                // continue 目标为独立自增块（同 Stmt::For）：continue 跳过 body 尾部的
                // idx 自增会死循环。
                let inc_b = self.new_block(Vec::new());
                let exit = self.new_block(Vec::new());
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(header);
                let iv = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: iv, ptr: idx_slot, ty: IrType::I64 });
                let cmp = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op: ACmp::Lt, dst: cmp, a: Value::Reg(iv), b: Value::Reg(len), ty: IrType::I64 });
                self.terminate(Term::Branch { cond: cmp, then_b: body_b, else_b: exit, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(body_b);
                let body_depth = self.scopes.len();
                self.push_scope();
                self.loops.push((inc_b, exit, body_depth));
                let vv = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(vv), func: "dial_array_get".to_string(), args: vec![arr_v, Value::Reg(iv)], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                self.emit(IrInst::Store { ptr: val_slot, val: Value::Reg(vv), ty: IrType::I64 });
                for st in body { self.lower_stmt(st)?; }
                self.loops.pop();
                self.leave_scope()?;
                if !self.cur_terminated() {
                    self.terminate(Term::Jump { target: inc_b, args: Vec::new() });
                }
                self.set_cur(inc_b);
                let iv2 = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: iv2, ptr: idx_slot, ty: IrType::I64 });
                let nv = self.new_reg(IrType::I64);
                self.emit(IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(iv2), b: Value::Imm(Imm::I64(1)), ty: IrType::I64 });
                self.emit(IrInst::Store { ptr: idx_slot, val: Value::Reg(nv), ty: IrType::I64 });
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(exit);
                Ok(())
            }
            Stmt::ForEachMap { key, value, coll, body, line, col, .. } => {
                let ct = self.expr_dt(coll);
                let is_kv = matches!(ct, Type::KvMap(..));
                let (k_ty, v_ty) = match &ct {
                    Type::KvMap(k, v) => ((**k).clone(), (**v).clone()),
                    _ => (Type::Int, Type::Int),
                };
                let (cv, _) = self.lower_expr(coll, &ct)?;
                // 键集快照（堆数组）登记到当前作用域，由作用域退出统一释放；
                // 循环体里提前 return 也会经 release_scopes_upto 释放它。
                let keys = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call {
                    dst: Some(keys),
                    func: (if is_kv { "dial_kvmap_keys" } else { "dial_map_keys" }).to_string(),
                    args: vec![cv.clone()], arg_tys: vec![IrType::Handle],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime,
                });
                let keys_slot = self.emit_alloca(IrType::Handle);
                self.emit(IrInst::Store { ptr: keys_slot, val: Value::Reg(keys), ty: IrType::Handle });
                self.declare(&format!("__asm_mapkeys_{}_{}", line, col), Local {
                    ptr: keys_slot, ty: IrType::Handle,
                    dt: Type::Array(Box::new(k_ty.clone())), is_handle: true, moved: false,
                });
                let len = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(len), func: "dial_array_length".to_string(),
                    args: vec![Value::Reg(keys)], arg_tys: vec![IrType::Handle],
                    ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let idx_slot = self.emit_alloca(IrType::I64);
                self.emit(IrInst::Store { ptr: idx_slot, val: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                let k_ir = reg_ir(&k_ty);
                let v_ir = reg_ir(&v_ty);
                let k_slot = self.emit_alloca(k_ir.clone());
                let v_slot = self.emit_alloca(v_ir.clone());
                // 键/值是「借用」自键集与表内（C 侧未 retain），故 is_handle=false，
                // 循环体作用域退出时绝不 release，避免 double free。
                self.declare(key, Local { ptr: k_slot, ty: k_ir.clone(), dt: k_ty, is_handle: false, moved: false });
                self.declare(value, Local { ptr: v_slot, ty: v_ir.clone(), dt: v_ty, is_handle: false, moved: false });
                let header = self.new_block(Vec::new());
                let body_b = self.new_block(Vec::new());
                // continue 目标为独立自增块（同 Stmt::For / ForEach）。
                let inc_b = self.new_block(Vec::new());
                let exit = self.new_block(Vec::new());
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(header);
                let iv = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: iv, ptr: idx_slot, ty: IrType::I64 });
                let cmp = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op: ACmp::Lt, dst: cmp, a: Value::Reg(iv), b: Value::Reg(len), ty: IrType::I64 });
                self.terminate(Term::Branch { cond: cmp, then_b: body_b, else_b: exit, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(body_b);
                let body_depth = self.scopes.len();
                self.push_scope();
                self.loops.push((inc_b, exit, body_depth));
                // k = keys[i]（位模式 → 按 K 重新解释）
                let kb = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(kb), func: "dial_array_get".to_string(),
                    args: vec![Value::Reg(keys), Value::Reg(iv)], arg_tys: vec![IrType::Handle, IrType::I64],
                    ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let kv = self.reinterpret_bits(Value::Reg(kb), k_ir.clone())?;
                self.emit(IrInst::Store { ptr: k_slot, val: kv, ty: k_ir });
                // v = m.get(k)
                let vb = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(vb),
                    func: (if is_kv { "dial_kvmap_get" } else { "dial_map_get" }).to_string(),
                    args: vec![cv, Value::Reg(kb)], arg_tys: vec![IrType::Handle, IrType::I64],
                    ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let vv = self.reinterpret_bits(Value::Reg(vb), v_ir.clone())?;
                self.emit(IrInst::Store { ptr: v_slot, val: vv, ty: v_ir });
                for st in body { self.lower_stmt(st)?; }
                self.loops.pop();
                self.leave_scope()?;
                if !self.cur_terminated() {
                    self.terminate(Term::Jump { target: inc_b, args: Vec::new() });
                }
                self.set_cur(inc_b);
                let iv2 = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: iv2, ptr: idx_slot, ty: IrType::I64 });
                let nv = self.new_reg(IrType::I64);
                self.emit(IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(iv2), b: Value::Imm(Imm::I64(1)), ty: IrType::I64 });
                self.emit(IrInst::Store { ptr: idx_slot, val: Value::Reg(nv), ty: IrType::I64 });
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(exit);
                Ok(())
            }
            Stmt::Break(_, _) => {
                if let Some((_, exit, depth)) = self.loops.last().copied() {
                    self.release_scopes_upto(depth)?;
                    self.terminate(Term::Jump { target: exit, args: Vec::new() });
                    Ok(())
                } else { self.err("break 不在循环内") }
            }
            Stmt::Continue(_, _) => {
                if let Some((cont, _, depth)) = self.loops.last().copied() {
                    self.release_scopes_upto(depth)?;
                    self.terminate(Term::Jump { target: cont, args: Vec::new() });
                    Ok(())
                } else { self.err("continue 不在循环内") }
            }
            Stmt::Match { subject, arms, .. } => self.lower_match(subject, arms),
            Stmt::Defer(inner, ..) => {
                // 登记到当前作用域，离开该作用域时按 LIFO 触发（正常落穿 /
                // return / break / continue / throw 全部出口统一经 leave_scope /
                // release_scopes_upto 排空）。
                let d = self.scopes.len();
                self.defers.push((d, (**inner).clone()));
                Ok(())
            }
            Stmt::With { resource, name, body, .. } => self.lower_with(resource, name, body),
            // label / goto：与 codegen 的作用域压栈点一一对应（analyze_gotos 的
            // depth 与 scopes.len() 口径一致，见 semantic.rs:5290）。goto 跨作用域
            // 释放时按 label_depths 把 scopes 收敛到标签所在深度。
            Stmt::Label(name, ..) => {
                let bid = match self.labels.get(name) {
                    Some(b) => *b,
                    None => { let b = self.new_block(Vec::new()); self.labels.insert(name.clone(), b); b }
                };
                // 前一条语句若未终结（顺序落到标签），补一条无条件跳转接上。
                if !self.cur_terminated() {
                    self.terminate(Term::Jump { target: bid, args: Vec::new() });
                }
                self.set_cur(bid);
                Ok(())
            }
            Stmt::Goto(target, ..) => {
                let bid = match self.labels.get(target) {
                    Some(b) => *b,
                    None => { let b = self.new_block(Vec::new()); self.labels.insert(target.clone(), b); b }
                };
                let d = match self.label_depths.get(target) {
                    Some(d) => *d,
                    None => return self.err(&format!("asm 后端：goto 目标标签 '{}' 深度未知（语义层未登记）", target)),
                };
                // 跨作用域释放：与 LLVM 后端 codegen.rs:3813 的作用域口径对齐——
                // 只释放「标签深度 d 及以上」所有层的 defer 与堆局部（标记 moved 防双释放），
                // **绝不 pop 作用域帧**：LLVM 后端同样保留帧，使标签块在原深度仍能 lookup
                // 到该层局部（如 count_down 的 sum/i）。若此处 truncate，外层 if/块收尾的
                // leave_scope 会误弹被跨过的帧、把标签层局部一起删掉（→「未声明的变量」）。
                self.drain_defers_from(d)?;
                self.release_locals_upto(d);
                self.terminate(Term::Jump { target: bid, args: Vec::new() });
                Ok(())
            }
            Stmt::Return(val, _, _) => {
                // 铁律：**先求值，再释放作用域**。若在求值前 release_scopes_upto(0)，
                // 返回表达式里的局部变量会被提前释放（甚至已出作用域查不到名字），造成悬垂返回。
                let ret_val = match val {
                    Some(e) => {
                        let dt = self.expr_dt(e);
                        let (v, _) = self.lower_expr(e, &dt)?;
                        let v = self.adapt_param(v, &dt, &dt);
                        if let Type::Struct(_) = &dt {
                            // 结构体返回值：内容区在本函数栈帧上，函数返回后即失效，
                            // 不能直接把内容区地址交出去（调用方会读到已销毁的栈）。
                            // 故先把内容 **box 到堆**（dial_struct_box，RC 对象），
                            // 返回其 payload 指针；调用方 memcpy 出自己的副本后 release。
                            // 与 LLVM 后端对结构体装箱的处理同源（codegen.rs:5895）。
                            let size = self.struct_byte_size(&dt);
                            let sz = self.new_reg(IrType::I64);
                            self.emit(IrInst::Conv { kind: AConv::Zext, dst: sz,
                                a: Value::Imm(Imm::I64(size)), ty: IrType::I64 });
                            let boxed = self.new_reg(IrType::Handle);
                            self.emit(IrInst::Call { dst: Some(boxed), func: "dial_struct_box".to_string(),
                                args: vec![v, Value::Reg(sz)], arg_tys: vec![IrType::Ptr, IrType::I64],
                                ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                            Some(Value::Reg(boxed))
                        } else {
                            // 返回值直接来自堆句柄局部变量：标记 moved，所有权随返回值转移，
                            // 避免紧随其后的作用域释放把返回句柄降为 0（悬垂指针）。
                            if let Expr::Var(n) = e {
                                if rc_kind(&dt).is_some() {
                                    if let Some(sl) = self.lookup_mut(n) { sl.moved = true; }
                                }
                            }
                            Some(v)
                        }
                    }
                    None => None,
                };
                self.release_scopes_upto(0)?;
                // 提前 return 会绕过 try 的 merge/catch 收尾，必须把本函数仍活跃的
                // exbuf 逐个弹栈并释放，否则运行时异常栈残留（后续 try 路由错帧）。
                self.cleanup_try_stack();
                self.terminate(Term::Return { val: ret_val });
                Ok(())
            }
            Stmt::Throw(e, _, _) => {
                let dt = self.expr_dt(e);
                let (err, _) = self.lower_expr(e, &dt)?;
                // 正要抛出的 Error 本身不能被随后的局部释放降为 0（UAF）。
                if let Expr::Var(n) = e {
                    if let Some(l) = self.lookup_mut(n) { l.moved = true; }
                }
                // 只排空「本 try 帧及内层」的 defer；外层的留给 catch 之后的
                // 作用域退出，避免 longjmp 回同帧 catch 时被重复执行。
                let td = self.try_frames.last().copied().unwrap_or(0);
                self.drain_defers_from(td + 1)?;
                // 只释放本帧被跨越的局部；try 之前的局部要留给 catch 体引用。
                self.release_locals_upto(td);
                self.lower_throw_value(err)?;
                Ok(())
            }
            Stmt::Try { body, catch, .. } => self.lower_try(body, catch),
            Stmt::Select { arms, default_body, timeout, timeout_body, .. } => {
                self.lower_select(arms, default_body, timeout, timeout_body)
            }
            Stmt::Expr(e, _, _) => { self.lower_expr(e, &Type::Void)?; Ok(()) }
            Stmt::Block { body, .. } => {
                self.push_scope();
                for st in body { self.lower_stmt(st)?; }
                self.leave_scope()?;
                Ok(())
            }
        }
    }

    // ---- 异常（C 侧 setjmp/longjmp + 运行时异常栈） ----

    /// 把本函数仍活跃的 exbuf 逐个弹栈并释放（提前 return 时的兜底清理）。
    fn cleanup_try_stack(&mut self) {
        let slots: Vec<VReg> = self.try_bufs.clone();
        for slot in slots.into_iter().rev() {
            self.emit(IrInst::Call { dst: None, func: "dial_exc_pop".to_string(),
                args: vec![], arg_tys: vec![], ret_ty: IrType::Void, linkage: CallLink::Runtime });
            let b = self.new_reg(IrType::Handle);
            self.emit(IrInst::Load { dst: b, ptr: slot, ty: IrType::Handle });
            self.emit(IrInst::Call { dst: None, func: "dial_exbuf_free".to_string(),
                args: vec![Value::Reg(b)], arg_tys: vec![IrType::Handle],
                ret_ty: IrType::Void, linkage: CallLink::Runtime });
        }
    }

    /// 抛出 Error：取运行时异常栈顶，能捕获则 dial_throw_to（longjmp 回 setjmp
    /// 调用点），无 try 则 dial_panic 打印消息并终止，绝不静默继续。
    /// 全 spill 固定栈帧天然满足 longjmp 的要求：所有 vreg 都在固定栈槽里，
    /// longjmp 恢复 rsp/rbp 后仍能读回 setjmp 之前算好的 exbuf 句柄。
    fn lower_throw_value(&mut self, err: Value) -> Result<(), String> {
        let top = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(top), func: "dial_exc_top".to_string(),
            args: vec![], arg_tys: vec![], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
        let has = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Neq, dst: has, a: Value::Reg(top),
            b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
        let throw_b = self.new_block(Vec::new());
        let panic_b = self.new_block(Vec::new());
        self.terminate(Term::Branch { cond: has, then_b: throw_b, else_b: panic_b,
            then_args: Vec::new(), else_args: Vec::new() });
        // 捕获路径
        self.set_cur(throw_b);
        self.emit(IrInst::Call { dst: None, func: "dial_throw_to".to_string(),
            args: vec![Value::Reg(top), err], arg_tys: vec![IrType::Handle, IrType::Handle],
            ret_ty: IrType::Void, linkage: CallLink::Runtime });
        self.terminate(Term::Unreachable);
        // 未捕获路径：dial_error_msg 返回 DialString，必须先 dial_string_cstr
        // 转成裸 C 字符串再交给 dial_panic（否则打印的是 ARC 头的字节）。
        self.set_cur(panic_b);
        let msg = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(msg), func: "dial_error_msg".to_string(),
            args: vec![err], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Handle, linkage: CallLink::Runtime });
        let cstr = self.new_reg(IrType::Ptr);
        self.emit(IrInst::Call { dst: Some(cstr), func: "dial_string_cstr".to_string(),
            args: vec![Value::Reg(msg)], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Ptr, linkage: CallLink::Runtime });
        self.emit(IrInst::Call { dst: None, func: "dial_panic".to_string(),
            args: vec![Value::Reg(cstr)], arg_tys: vec![IrType::Ptr],
            ret_ty: IrType::Void, linkage: CallLink::Runtime });
        self.terminate(Term::Unreachable);
        // 后续语句落进不可达块，绝不污染两条已终结路径。
        let dead = self.new_block(Vec::new());
        self.set_cur(dead);
        Ok(())
    }

    /// try { body } catch e { ... }：与 codegen.rs:3323 的 CFG 同构。
    /// setjmp 必须由「包含 try 的本函数」亲自调用（jmp_buf 存的是本帧栈指针），
    /// 因此这里直接 call setjmp，绝不包到 C 助手函数里再返回。
    fn lower_try(&mut self, body: &[Stmt], catch: &Catch) -> Result<(), String> {
        // ---- setup：建 exbuf、压异常栈、setjmp ----
        let buf = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(buf), func: "dial_exbuf_new".to_string(),
            args: vec![], arg_tys: vec![], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
        let buf_slot = self.emit_alloca(IrType::Handle);
        self.emit(IrInst::Store { ptr: buf_slot, val: Value::Reg(buf), ty: IrType::Handle });
        self.emit(IrInst::Call { dst: None, func: "dial_exc_push".to_string(),
            args: vec![Value::Reg(buf)], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Void, linkage: CallLink::Runtime });
        let env = self.new_reg(IrType::Ptr);
        self.emit(IrInst::Call { dst: Some(env), func: "dial_exbuf_env_ptr".to_string(),
            args: vec![Value::Reg(buf)], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Ptr, linkage: CallLink::Runtime });
        let r32 = self.new_reg(IrType::I32);
        self.emit(IrInst::Call { dst: Some(r32), func: "setjmp".to_string(),
            args: vec![Value::Reg(env)], arg_tys: vec![IrType::Ptr],
            ret_ty: IrType::I32, linkage: CallLink::Runtime });
        let r = self.new_reg(IrType::I64);
        self.emit(IrInst::Conv { kind: AConv::Sext, dst: r, a: Value::Reg(r32), ty: IrType::I64 });
        let is_throw = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Neq, dst: is_throw, a: Value::Reg(r),
            b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
        let body_b = self.new_block(Vec::new());
        let catch_b = self.new_block(Vec::new());
        let merge_b = self.new_block(Vec::new());
        let cont_b = self.new_block(Vec::new());
        self.terminate(Term::Branch { cond: is_throw, then_b: catch_b, else_b: body_b,
            then_args: Vec::new(), else_args: Vec::new() });

        // ---- try.body（首次 setjmp 返回 0 走这里）----
        self.set_cur(body_b);
        let try_depth = self.scopes.len();
        self.try_frames.push(try_depth);
        self.try_bufs.push(buf_slot);
        self.push_scope();
        for st in body { self.lower_stmt(st)?; if self.cur_terminated() { break; } }
        let body_done = !self.cur_terminated();
        self.leave_scope()?;
        if body_done { self.terminate(Term::Jump { target: merge_b, args: Vec::new() }); }
        self.try_frames.pop();
        self.try_bufs.pop();

        // ---- catch（longjmp 回来，setjmp 返回非 0）----
        self.set_cur(catch_b);
        self.emit(IrInst::Call { dst: None, func: "dial_exc_pop".to_string(),
            args: vec![], arg_tys: vec![], ret_ty: IrType::Void, linkage: CallLink::Runtime });
        let buf2 = self.new_reg(IrType::Handle);
        self.emit(IrInst::Load { dst: buf2, ptr: buf_slot, ty: IrType::Handle });
        let err = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(err), func: "dial_exbuf_take_err".to_string(),
            args: vec![Value::Reg(buf2)], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Handle, linkage: CallLink::Runtime });
        self.emit(IrInst::Call { dst: None, func: "dial_exbuf_free".to_string(),
            args: vec![Value::Reg(buf2)], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Void, linkage: CallLink::Runtime });
        self.push_scope();
        match &catch.var {
            Some(name) => {
                let slot = self.emit_alloca(IrType::Handle);
                self.emit(IrInst::Store { ptr: slot, val: Value::Reg(err), ty: IrType::Handle });
                self.declare(name, Local { ptr: slot, ty: IrType::Handle,
                    dt: Type::Error, is_handle: true, moved: false });
            }
            None => {
                // 无人接收错误对象：立即释放，避免泄漏。
                let slot = self.emit_alloca(IrType::Handle);
                self.emit(IrInst::Store { ptr: slot, val: Value::Reg(err), ty: IrType::Handle });
                self.emit(IrInst::RcRelease { ptr: slot, kind: ArcKind::Error });
            }
        }
        for st in &catch.body { self.lower_stmt(st)?; if self.cur_terminated() { break; } }
        let catch_done = !self.cur_terminated();
        self.leave_scope()?;
        if catch_done { self.terminate(Term::Jump { target: cont_b, args: Vec::new() }); }

        // ---- merge：仅「try 体正常完成」到达，与 catch 严格二选一 ----
        self.set_cur(merge_b);
        self.emit(IrInst::Call { dst: None, func: "dial_exc_pop".to_string(),
            args: vec![], arg_tys: vec![], ret_ty: IrType::Void, linkage: CallLink::Runtime });
        let buf3 = self.new_reg(IrType::Handle);
        self.emit(IrInst::Load { dst: buf3, ptr: buf_slot, ty: IrType::Handle });
        self.emit(IrInst::Call { dst: None, func: "dial_exbuf_free".to_string(),
            args: vec![Value::Reg(buf3)], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Void, linkage: CallLink::Runtime });
        self.terminate(Term::Jump { target: cont_b, args: Vec::new() });
        self.set_cur(cont_b);
        Ok(())
    }

    // ---- 表达式：返回 (Value, is_copy) ----

    fn lower_expr(&mut self, e: &Expr, _hint: &Type) -> Result<(Value, bool), String> {
        match e {
            Expr::IntLit(v) => Ok((Value::Imm(Imm::I64(*v)), false)),
            Expr::FloatLit(v) => Ok((Value::Imm(Imm::F64(f64::to_bits(*v))), false)),
            Expr::BoolLit(v) => Ok((Value::Imm(Imm::I64(if *v { 1 } else { 0 })), false)),
            Expr::CharLit(v) => Ok((Value::Imm(Imm::I64(*v)), false)),
            Expr::ByteLit(v) => Ok((Value::Imm(Imm::I64(*v)), false)),
            Expr::StringLit(s) => {
                let lit = self.ip.intern_string(s);
                // 静态 immortal DialString：字面量编译期已知且字符串不可变，
                // 直接用 .rodata 静态对象句柄（rc=INT64_MIN，retain/release no-op），
                // 省去每次求值的 dial_string_new(malloc+strlen+memcpy) 与成对 release。
                // 语义等价：内容相同、比较仍走 dial_string_eq（内容比较，无身份依赖）。
                let h = self.new_reg(IrType::Handle);
                self.emit(IrInst::LeaStaticString { dst: h, lit });
                Ok((Value::Reg(h), false)) // 静态 immortal 对象，无需 retain/release
            }
            Expr::Interp(parts) => self.lower_interp(parts),
            Expr::Var(name) => {
                let l = match self.lookup(name) {
                    Some(l) => l.clone(),
                    None => {
                        // 不是局部变量，但可能是**具名函数**——函数名在右值位置
                        // 求值为纯代码地址（v1.03 的一等函数指针，语义层同口径：
                        // 名字既不是变量也不是函数时才报错）。
                        if self.ip.functions_sig.contains_key(name) {
                            let p = self.new_reg(IrType::FnPtr);
                            self.emit(IrInst::FuncAddr { dst: p, func: name.clone() });
                            return Ok((Value::Reg(p), false));
                        }
                        return self.err(&format!("未声明的变量 '{}'", name));
                    }
                };
                if let Type::Struct(_) = l.dt {
                    Ok((Value::Reg(l.ptr), false)) // 结构体变量：返回槽地址（指针语义）
                } else {
                    let r = self.new_reg(l.ty.clone());
                    self.emit(IrInst::Load { dst: r, ptr: l.ptr, ty: l.ty });
                    Ok((Value::Reg(r), true)) // 从变量读是「拷贝」（句柄需 retain 当存到别处）
                }
            }
            Expr::Field(base, field) => {
                let base_ty = self.expr_dt(base);
                // 基址分两种（对齐 LLVM 后端 codegen.rs:3990 的 field_base_ptr）：
                //   · base 是裸指针：基址就是**指针值**本身（指向 C 侧内存的视图）；
                //   · 否则：base 是结构体内容区地址。
                let is_raw = matches!(base_ty, Type::RawPtr(_) | Type::VolatilePtr(_));
                let (bv, _) = self.lower_expr(base, &base_ty)?;
                // 裸指针的元素类型即结构体类型；否则按字段链逐级解析。
                let base_dt = if is_raw {
                    match &base_ty {
                        Type::RawPtr(e) | Type::VolatilePtr(e) => (**e).clone(),
                        _ => base_ty.clone(),
                    }
                } else {
                    self.expr_struct_dt(base)
                };
                let (off, fty) = self.struct_field(&base_dt, field)
                    .ok_or_else(|| format!("[asm 后端] 结构体 {} 无字段 {}", base_dt.display(), field))?;
                let fa = self.new_reg(IrType::Ptr);
                self.emit(IrInst::FieldAddr { dst: fa, base: bv.as_reg(), struct_name: struct_name_of(&base_dt), field: off as u32 });
                match &fty {
                    Type::Struct(_) => Ok((Value::Reg(fa), false)),
                    other => {
                        // 字段内存宽度按 struct 类型分口径（extern byte=1B / 普通 byte=8B）；
                        // 与 struct_layout 偏移同口径，内部宽度 reg_ir。
                        let mty = field_mem_ir(&self.ip.cstruct_names, &base_dt, other);
                        let r = self.new_reg(mty.clone());
                        self.emit(IrInst::Load { dst: r, ptr: fa, ty: mty.clone() });
                        if mty != reg_ir(other) {
                            let rr = self.new_reg(reg_ir(other));
                            self.emit(IrInst::Conv { kind: AConv::Zext, dst: rr, a: Value::Reg(r), ty: reg_ir(other) });
                            Ok((Value::Reg(rr), true))
                        } else {
                            Ok((Value::Reg(r), true))
                        }
                    }
                }
            }
            Expr::Method(obj, method, args) => self.lower_method(obj, method, args),
            Expr::Index(arr, idx) => {
                // 元素类型必须按数组的真实类型取（此前硬编码 int[]，导致 string[]
                // 的元素位模式没被按句柄还原——读出来是一串指针数值）。
                let at = self.expr_dt(arr);
                // M7-e3az：向量 lane 提取 `v[i]`。对齐 LLVM 4108 的 extractelement。
                // lane 索引须为编译期整数常量（语义层已约束；这里尽力解析）。
                if let Type::Simd(elem, n) = &at {
                    let lane = match idx.as_ref() {
                        Expr::IntLit(k) if *k >= 0 && (*k as u32) < *n => *k as u32,
                        _ => return self.err(&format!(
                            "[asm 后端] 向量 lane 索引必须是 0..{} 的整数字面量", n - 1)),
                    };
                    let (av, _) = self.lower_expr(arr, &at)?;
                    // lane 存的是 Dial 内部位模式，直接按标量宽度读出即可返回。
                    let r = self.simd_load_lane(av.as_reg(), lane, simd_lane_ir(elem));
                    return Ok((Value::Reg(r), false));
                }
                let (av, _) = self.lower_expr(arr, &at)?;
                let (iv, _) = self.lower_expr(idx, &Type::Int)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_get".to_string(), args: vec![av, iv], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                // 元素按其真实类型还原位模式（句柄 / 浮点 / 窄整型）。
                let et = match &at {
                    Type::Array(e) => (**e).clone(),
                    other => other.clone(),
                };
                let out = self.reinterpret_bits(Value::Reg(r), reg_ir(&et))?;
                // 借来的元素：数组仍持有它，读侧不得释放（对齐 LLVM 后端
                // codegen.rs 的 from_elem_bits + 借用语义）。
                Ok((out, false))
            }
            Expr::Slice(arr, from, to) => {
                // a[from..to] / a.slice(from, to)：半开区间，返回新数组句柄。
                // 数组类型按实际取（此前硬编码 int[]，string[] 的切片会被误判元素类型）。
                let at = self.expr_dt(arr);
                let (av, _) = self.lower_expr(arr, &at)?;
                let (fv, _) = self.lower_expr(from, &Type::Int)?;
                let (tv, _) = self.lower_expr(to, &Type::Int)?;
                let r = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call {
                    dst: Some(r), func: "dial_array_slice".to_string(),
                    args: vec![av, fv, tv],
                    arg_tys: vec![IrType::Handle, IrType::I64, IrType::I64],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime,
                });
                Ok((Value::Reg(r), false))
            }
            Expr::ArrayLit(elems, expect) => {
                // 元素类型决定 kind（0=INT 1=RC 2=ATOMIC 3=FLOAT 4=BOOL 5=BYTE 6=CHAR）。
                // kind 必须与元素类型严格对应：kind=INT 时 C 侧的 elem_retain /
                // elem_release 是 no-op，句柄元素就既不被 retain 也不被 release，
                // 数组释放后留下悬垂指针（string_array_demo 即栽在这里）。
                let elem_ty = match expect {
                    Some(et) => (**et).clone(),
                    None => match elems.first() {
                        Some(e) => self.expr_dt(e),
                        None => Type::Int,
                    },
                };
                let kind = builtin::array_elem_kind(&elem_ty);
                let mut h = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(h), func: "dial_array_new".to_string(),
                    args: vec![Value::Imm(Imm::I64(kind as i64))], arg_tys: vec![IrType::I64],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                for el in elems {
                    let ety = self.expr_dt(el);
                    let (ev, _) = self.lower_expr(el, &ety)?;
                    let bits = self.coerce_elem_bits(ev.clone(), &ety)?;
                    let h2 = self.new_reg(IrType::Handle);
                    self.emit(IrInst::Call { dst: Some(h2), func: "dial_array_push".to_string(),
                        args: vec![Value::Reg(h), bits], arg_tys: vec![IrType::Handle, IrType::I64],
                        ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                    h = h2;
                    // push 会为元素 retain 一份（数组成为持有者）。字面量自带的那一份
                    // 所有权本应直接移交数组，故此处归还——否则每个字面量元素泄漏一份引用。
                    if matches!(&ety, Type::Struct(_)) {
                        // v1.10 B6：struct 值 coerce 时无条件新建 box（rc=1），push 又 +1，
                        // 归还本地这份（对齐 LLVM sink_elem_after_retain）。
                        self.release_box(bits.clone());
                    } else if let Some(k) = rc_kind(&ety) {
                        if !yields_borrowed_ref(el) {
                            let tmp = self.emit_alloca(reg_ir(&ety));
                            self.emit(IrInst::Store { ptr: tmp, val: ev, ty: reg_ir(&ety) });
                            self.emit(IrInst::RcRelease { ptr: tmp, kind: k });
                        }
                    }
                }
                Ok((Value::Reg(h), false))
            }
            Expr::StructLit(name, fields) => {
                let dt = Type::Struct(name.clone());
                let size = self.struct_byte_size(&dt) as u32;
                let s2 = self.new_reg(IrType::Ptr);
                self.emit(IrInst::Alloca { dst: s2, bytes: size.max(8) });

                // B8 结构体字段默认值：对齐 LLVM codegen.rs 的 StructLit 处理——
                // 遍历**定义的所有字段**，字面量显式提供的用提供值，省略且有默认值的
                // 用编译期常量默认值。此前 asm 后端只遍历显式 fields，省略字段留在
                // Alloca 的未初始化栈区（p0.x=64 / r0.tag=NULL 崩溃即由此而来）。
                // 先 clone 出字段表与默认值表，避免后续 lower_expr(&mut self) 借用冲突。
                let (def_fields, def_defaults) = match self.ip.structs.get(name) {
                    Some((f, d)) => (f.clone(), d.clone()),
                    None => return self.err(&format!("[asm 后端] 未知结构体 {}", name)),
                };
                let provided: HashMap<&str, &Expr> = fields.iter()
                    .map(|(f, e)| (f.as_str(), e)).collect();

                for (fidx, (fname, fty)) in def_fields.iter().enumerate() {
                    let fa = self.new_reg(IrType::Ptr);
                    self.emit(IrInst::FieldAddr { dst: fa, base: s2, struct_name: name.clone(), field: fidx as u32 });

                    // 提供值优先；省略则用语义层已校验为编译期常量的默认值。
                    let fe: &Expr = match provided.get(fname.as_str()) {
                        Some(e) => *e,
                        None => match def_defaults.get(fname) {
                            Some(d) => d,
                            None => {
                                // 防御：语义层通常会拒绝「省略且无默认值」；万一漏过，
                                // 按字段内存宽度写 0，杜绝未初始化垃圾（对齐 LLVM const_zero）。
                                let mty0 = field_mem_ir(&self.ip.cstruct_names, &dt, fty);
                                let z = zero_imm_for(&mty0);
                                self.emit(IrInst::Store { ptr: fa, val: z, ty: mty0 });
                                continue;
                            }
                        },
                    };
                    let (fv, is_copy) = self.lower_expr(fe, fty)?;
                    if let Type::Struct(_) = &fty {
                        // 嵌套结构体字段：按值拷贝**内容**（fv 是内容区地址），
                        // 不能只存 8 字节指针——那会让读侧把指针当成字段内容，
                        // 嵌套访问 r.origin.x 于是读到指针位模式（垃圾值）。
                        self.emit_memcpy(fa, fv, self.struct_byte_size(fty));
                        self.emit(IrInst::StructRetain { ptr: fa, struct_name: struct_name_of(fty) });
                        continue;
                    }
                    // 字段内存宽度按 struct 类型分口径（extern byte=1B / 普通 byte=8B）；
                    // 与 struct_layout 偏移同口径，内部 I64 需截断。
                    let mty = field_mem_ir(&self.ip.cstruct_names, &dt, fty);
                    let fv_store = if mty != reg_ir(fty) {
                        let t = self.new_reg(mty.clone());
                        self.emit(IrInst::Conv { kind: AConv::Trunc, dst: t, a: fv, ty: mty.clone() });
                        Value::Reg(t)
                    } else { fv };
                    self.emit(IrInst::Store { ptr: fa, val: fv_store, ty: mty });
                    if is_copy {
                        if let Some(k) = rc_kind(fty) {
                            self.emit(IrInst::RcRetain { ptr: fa, kind: k });
                        }
                    }
                }
                Ok((Value::Reg(s2), false))
            }
            Expr::Binary(a, op, b) => self.lower_binary(a, *op, b),
            Expr::Unary(op, a) => self.lower_unary(*op, a),
            Expr::Ternary(c, t, f) => {
                let (cv, _) = self.lower_expr(c, &Type::Bool)?;
                let hint = self.expr_dt(t);
                let cc = self.cond_reg(cv)?;
                let r = self.lower_value_if(cc, t, f, &hint)?;
                Ok((Value::Reg(r), false))
            }
            Expr::Call(name, args) => self.lower_call(name, args),
            Expr::TypeLit(_) => self.err("asm 后端暂不支持类型字面量（后续相位）"),
            // 具名函数当值：取其**代码地址**（纯代码地址，无闭包、无环境）。
            // 与 LLVM 后端一致：fn(A,B):R 在 IR 层就是一个 ptr。
            Expr::FuncRef(fname) => {
                if !self.ip.functions_sig.contains_key(fname) {
                    return self.err(&format!("未定义的函数 '{}'", fname));
                }
                let p = self.new_reg(IrType::FnPtr);
                self.emit(IrInst::FuncAddr { dst: p, func: fname.clone() });
                Ok((Value::Reg(p), false))
            }
        }
    }

    /// 构造「条件 ? then : else」式的 SSA 值：返回 merge 块参数寄存器（I64 0/1 或给定 hint）。
    fn lower_value_if(&mut self, cond: VReg, t: &Expr, f: &Expr, hint: &Type) -> Result<VReg, String> {
        let t_reg = reg_ir(hint);
        let tb = self.new_block(Vec::new());
        let eb = self.new_block(Vec::new());
        let mb = self.new_block(Vec::new());
        let p = self.new_reg(t_reg.clone());
        self.func.blocks[mb as usize].params.push((p, t_reg));
        self.terminate(Term::Branch { cond, then_b: tb, else_b: eb, then_args: Vec::new(), else_args: Vec::new() });
        self.set_cur(tb);
        let (tv, _) = self.lower_expr(t, hint)?;
        self.terminate(Term::Jump { target: mb, args: vec![tv] });
        self.set_cur(eb);
        let (fv, _) = self.lower_expr(f, hint)?;
        self.terminate(Term::Jump { target: mb, args: vec![fv] });
        self.set_cur(mb);
        Ok(p)
    }

    // ---- 二元 / 一元 ----

    fn lower_binary(&mut self, a: &Expr, op: BinOp, b: &Expr) -> Result<(Value, bool), String> {
        // M7-e3az：向量二元 / 比较运算。任一侧是 simd 即走向量路径（对齐 LLVM
        // gen_simd_binary）。比较两个向量产生 simd<bool,N>（不是标量 bool），
        // 故必须在标量比较分支之前处理。
        let da = self.expr_dt(a);
        let db = self.expr_dt(b);
        if let Type::Simd(elem, n) = &da {
            let (av, _) = self.lower_expr(a, &da)?;
            let (bv, _) = self.lower_expr(b, &db)?;
            let a_ptr = av.as_reg();
            let b_ptr = bv.as_reg();
            let lane_ty = simd_lane_ir(elem);
            let is_float = lane_ty.is_float();
            let is_cmp = matches!(op, BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge);
            let out = self.emit_simd_alloca(*n);
            // 逐 lane 展开：load a[i]/b[i] → 标量 Bin/FBin 或 ICmp/FCmp → store out[i]。
            // 复用既有标量发射器，天然兼容 AArch64；byte 比较值 0..255 非负，
            // 有符号 ICmp 即得无符号语义（对齐 LLVM 的无符号向量谓词）。
            for i in 0..*n {
                let a_lane = self.simd_load_lane(a_ptr, i, lane_ty.clone());
                let b_lane = self.simd_load_lane(b_ptr, i, lane_ty.clone());
                let r = self.new_reg(if is_cmp { IrType::I64 } else { lane_ty.clone() });
                if is_cmp {
                    if is_float {
                        self.emit(IrInst::FCmp { op: acmp(op), dst: r, a: Value::Reg(a_lane),
                            b: Value::Reg(b_lane), ty: lane_ty.clone() });
                    } else {
                        self.emit(IrInst::ICmp { op: acmp(op), dst: r, a: Value::Reg(a_lane),
                            b: Value::Reg(b_lane), ty: lane_ty.clone() });
                    }
                    // 比较结果 0/1 作为 bool lane 存入（I64 宽度）。
                    self.simd_store_lane(out, i, Value::Reg(r), IrType::I64);
                } else {
                    let bin = match op {
                        BinOp::Add => ABin::Add, BinOp::Sub => ABin::Sub,
                        BinOp::Mul => ABin::Mul, BinOp::Div => ABin::Div,
                        BinOp::Mod => ABin::Mod, BinOp::BitAnd => ABin::BitAnd,
                        BinOp::BitOr => ABin::BitOr, BinOp::BitXor => ABin::BitXor,
                        BinOp::Shl => ABin::Shl, BinOp::Shr => ABin::Shr,
                        _ => return self.err(&format!("[asm 后端] 向量不支持运算符 {:?}", op)),
                    };
                    if is_float {
                        if op == BinOp::Mod {
                            // 浮点取模同标量：降为 libm fmod/fmodf 调用（LLVM frem 同构）。
                            let (fname, fty) = if lane_ty == IrType::F32 {
                                ("fmodf", IrType::F32)
                            } else {
                                ("fmod", IrType::F64)
                            };
                            self.emit(IrInst::Call {
                                dst: Some(r), func: fname.to_string(),
                                args: vec![Value::Reg(a_lane), Value::Reg(b_lane)],
                                arg_tys: vec![fty.clone(), fty.clone()], ret_ty: fty,
                                linkage: CallLink::Runtime,
                            });
                        } else {
                            self.emit(IrInst::FBin { op: bin, dst: r, a: Value::Reg(a_lane),
                                b: Value::Reg(b_lane), ty: IrType::F64 });
                        }
                    } else {
                        self.emit(IrInst::Bin { op: bin, dst: r, a: Value::Reg(a_lane),
                            b: Value::Reg(b_lane), ty: IrType::I64 });
                    }
                    self.simd_store_lane(out, i, Value::Reg(r), lane_ty.clone());
                }
            }
            return Ok((Value::Reg(out), false));
        }
        let is_string_concat = op == BinOp::Add && self.expr_dt(a) == Type::String && self.expr_dt(b) == Type::String;
        if is_string_concat {
            let (av, _) = self.lower_expr(a, &Type::String)?;
            let (bv, _) = self.lower_expr(b, &Type::String)?;
            let r = self.new_reg(IrType::Handle);
            self.emit(IrInst::Call { dst: Some(r), func: "dial_string_concat".to_string(), args: vec![av.clone(), bv.clone()], arg_tys: vec![IrType::Handle, IrType::Handle], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
            // C 侧 concat 只读不消费操作数：归还自带所有权的临时操作数引用，
            // 否则 `str(i)` 这类无 Local 持有的中间值每轮循环泄漏一份。
            self.release_string_temp(&av, a);
            self.release_string_temp(&bv, b);
            return Ok((Value::Reg(r), false));
        }
        let da = self.expr_dt(a);
        let db = self.expr_dt(b);
        let is_float = matches!(da, Type::Float | Type::Float32) || matches!(db, Type::Float | Type::Float32);
        let (av, _) = self.lower_expr(a, &da)?;
        // and / or 必须短路：右操作数 b 只能由 lower_value_if 在条件分支内求值。
        // 此前在此处无条件 `lower_expr(b)`，既让被短路的 b 副作用照常触发，又让
        // lower_value_if 内再 lower 一次 b（副作用重复）——short_circuit.dial 因此
        // 右侧 print 多出或重复。故 And/Or 直接在这里返回，不预先求值 b。
        match op {
            BinOp::And => {
                // 短路：a ? b : false（b 仅在 a 为真时求值）
                let cc = self.cond_reg(av)?;
                let r = self.lower_value_if(cc, b, &Expr::BoolLit(false), &Type::Bool)?;
                return Ok((Value::Reg(r), false));
            }
            BinOp::Or => {
                // 短路：a ? true : b（b 仅在 a 为假时求值）
                let cc = self.cond_reg(av)?;
                let r = self.lower_value_if(cc, &Expr::BoolLit(true), b, &Type::Bool)?;
                return Ok((Value::Reg(r), false));
            }
            _ => {}
        }
        let (bv, _) = self.lower_expr(b, &db)?;
        match op {
            BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                // 字符串比较必须走 **dial_string_eq**（内容比较），绝不能用
                // 指针相等——两个内容相同的字符串是**不同的堆对象**（字面量
                // 每次求值都会 dial_string_new 出一份新副本），指针比较必然
                // 为假，于是 `s == "a"` 恒假、分派链全部走错分支
                // （bench.dial 的 run(name) 分派就是这样失效的）。
                // dial_string_eq 的约定是「相等返回 1、不等返回 0」，因此
                // `a == b` 对应「结果 != 0」、`a != b` 对应「结果 == 0」——
                // 与数值比较的方向**相反**，切勿照抄。
                if da == Type::String && db == Type::String && matches!(op, BinOp::Eq | BinOp::Neq) {
                    let cmp = self.new_reg(IrType::I64);
                    self.emit(IrInst::Call { dst: Some(cmp), func: "dial_string_eq".to_string(),
                        args: vec![av, bv], arg_tys: vec![IrType::Handle, IrType::Handle],
                        ret_ty: IrType::I64, linkage: CallLink::Runtime });
                    let r = self.new_reg(IrType::I64);
                    let icmp_op = if op == BinOp::Eq { ACmp::Neq } else { ACmp::Eq };
                    self.emit(IrInst::ICmp { op: icmp_op, dst: r, a: Value::Reg(cmp),
                        b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                    return Ok((Value::Reg(r), false));
                }
                let ty = if is_float { IrType::F64 } else { IrType::I64 };
                let r = self.new_reg(IrType::I64);
                if is_float {
                    self.emit(IrInst::FCmp { op: acmp(op), dst: r, a: av, b: bv, ty });
                } else {
                    self.emit(IrInst::ICmp { op: acmp(op), dst: r, a: av, b: bv, ty });
                }
                Ok((Value::Reg(r), false))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
            | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                // F32 算术必须保留 F32 宽度：此前恒用 F64（addsd），把 f32 位模式当
                // double 读——`float32(1.5)+float32(0.25)` 得 0.0（FFI32 门禁红点）。
                // 操作数由 lower_expr 按各自 dt 产出（Float32 → F32 寄存器/movss）。
                let f_ty = if matches!(da, Type::Float32) && matches!(db, Type::Float32) {
                    IrType::F32
                } else {
                    IrType::F64
                };
                let ty = if is_float { f_ty.clone() } else { IrType::I64 };
                let r = self.new_reg(ty.clone());
                if is_float && op == BinOp::Mod {
                    // 浮点取模：x86 无标量 frem 指令，LLVM 把 float/double 的 `frem`
                    // 降级为 libm 的 fmodf/fmod 调用（向零截断余数）。asm 后端此前
                    // 误把 Mod 当 div（codegen_asm.rs 的 fop 映射 Mod→div），导致
                    // `10.5 % 3.0` 输出 3.5（=10.5/3.0）。这里在 IR 层直接降为
                    // fmod/fmodf 调用，复用 inst_call 完整的 ABI 发射（SysV/MS/AArch64
                    // 参数寄存器、shadow space、PLT、返回值）——与 LLVM 语义对齐，
                    // 且 -lm 已在链接层无条件加入（main.rs）。
                    let (fname, fty) = if f_ty == IrType::F32 {
                        ("fmodf", IrType::F32)
                    } else {
                        ("fmod", IrType::F64)
                    };
                    self.emit(IrInst::Call {
                        dst: Some(r), func: fname.to_string(),
                        args: vec![av, bv],
                        arg_tys: vec![fty.clone(), fty.clone()], ret_ty: fty,
                        linkage: CallLink::Runtime,
                    });
                } else if is_float {
                    self.emit(IrInst::FBin { op: abin(op), dst: r, a: av, b: bv, ty: f_ty });
                } else {
                    self.emit(IrInst::Bin { op: abin(op), dst: r, a: av, b: bv, ty: IrType::I64 });
                }
                Ok((Value::Reg(r), false))
            }
            // And/Or 已在上方短路分支 return；此处仅满足 match 穷尽性。
            BinOp::And | BinOp::Or => unreachable!("and/or handled above"),
        }
    }

    fn lower_unary(&mut self, op: UnOp, a: &Expr) -> Result<(Value, bool), String> {
        let da = self.expr_dt(a);
        let (av, _) = self.lower_expr(a, &da)?;
        match op {
            UnOp::Neg => {
                let is_float = matches!(da, Type::Float | Type::Float32);
                let ty = if is_float { IrType::F64 } else { IrType::I64 };
                let r = self.new_reg(ty.clone());
                if is_float {
                    self.emit(IrInst::FBin { op: ABin::Sub, dst: r, a: Value::Imm(Imm::F64(f64::to_bits(0.0))), b: av, ty: IrType::F64 });
                } else {
                    self.emit(IrInst::Bin { op: ABin::Sub, dst: r, a: Value::Imm(Imm::I64(0)), b: av, ty: IrType::I64 });
                }
                Ok((Value::Reg(r), false))
            }
            UnOp::Not => {
                // I64 0/1 取反：1 - x
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Bin { op: ABin::Sub, dst: r, a: Value::Imm(Imm::I64(1)), b: av, ty: IrType::I64 });
                Ok((Value::Reg(r), false))
            }
            UnOp::BitNot => {
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Bin { op: ABin::BitXor, dst: r, a: av, b: Value::Imm(Imm::I64(-1)), ty: IrType::I64 });
                Ok((Value::Reg(r), false))
            }
            UnOp::Deref => {
                // `*p`：p 是裸指针。`lower_expr(a)` 已把 p 的**指针值**取到 av
                // （Expr::Var 对裸指针变量做一次 Load，得到的是地址本身）。
                // 故此处**直接**用 av 作地址读元素——不能再多解引用一层：
                // 早先把 av 当成「指向指针的槽」又 Load 一次，读到的其实是
                // 裸内存里的垃圾（rawptr_demo 即栽在这里，表现为段错误）。
                //
                // 读取宽度按**内存宽度**（byte/bool=1B、char=4B、其余 8B，
                // 与 LLVM 后端 raw_elem_llvm 同口径），窄类型再零扩展回内部宽度——
                // 否则读 byte 会连着后面的字节一起读进来（"Hi" 被读成 0x6948）。
                let inner = da.strip_ptr();
                let mty = match &inner {
                    Type::Byte | Type::Bool => IrType::I8,
                    Type::Char => IrType::I32,
                    other => reg_ir(other),
                };
                let raw = self.new_reg(mty.clone());
                self.emit(IrInst::Load { dst: raw, ptr: av.as_reg(), ty: mty.clone() });
                if mty != reg_ir(&inner) {
                    let r = self.new_reg(reg_ir(&inner));
                    self.emit(IrInst::Conv { kind: AConv::Zext, dst: r, a: Value::Reg(raw), ty: reg_ir(&inner) });
                    Ok((Value::Reg(r), true))
                } else {
                    Ok((Value::Reg(raw), true))
                }
            }
            UnOp::AddrOf => {
                if let Expr::Var(name) = a {
                    if let Some(l) = self.lookup(name) { return Ok((Value::Reg(l.ptr), false)); }
                }
                self.err("asm 后端：`&x` 仅支持对值类型可变变量取址")
            }
        }
    }

    // ---- 方法调用（内置容器）----

    fn lower_method(&mut self, obj: &Expr, method: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        let od = self.expr_dt(obj);
        // M7-e3aq：B1「impl 方法块」——具名类型（用户 struct / 包不透明句柄）的
        // `obj.m(args)` 优先改写为对 `__impl_<T>_<m>` 的普通函数调用（对齐 LLVM
        // gen_method_call，codegen.rs:5479）。必须放在下面所有内建方法分派之前：
        // impl 方法可与内建方法撞名（如 Point.push），具名类型的分派权归 impl。
        // 复用既有 Expr::Call 链路 = 实参求值 / 默认实参补齐 / C ABI 宽度适配 /
        // struct 按值传递 / 返回值处理零改动，ABI 天然不变，self 即首参。
        if let Some(recv) = impl_receiver_name(&od) {
            let symbol = impl_method_symbol(recv, method);
            if self.ip.functions_sig.contains_key(&symbol) {
                let call = make_impl_call(obj, recv, method, args);
                return self.lower_expr(&call, &Type::Int);
            }
        }
        // remove / pop 返回「被删元素」而非新数组指针，且要写回 COW 新指针。
        if matches!(od, Type::Array(_)) && matches!(method, "remove" | "pop") {
            return self.lower_array_remove(obj, &od, method, args);
        }
        // M7-e3an：高阶数组方法（map/filter/reduce/sort_by）走专用路径——生成回调
        // thunk + 调用 C 运行时 dial_array_hof / dial_array_reduce（对齐 LLVM）。
        if matches!(od, Type::Array(_)) && matches!(method, "map" | "filter" | "reduce" | "sort_by") {
            return self.lower_hof_method(obj, &od, method, args);
        }
        // 裸指针 offset(n)：按元素类型步长做指针算术（负偏移靠补码自然生效，
        // 且支持运行期偏移量——不必是常量）。步长口径与 LLVM 后端的
        // raw_elem_llvm 一致：byte/bool=1、char=4、int/float=8、struct 按布局、
        // 其余按 8。
        if matches!(od, Type::RawPtr(_) | Type::VolatilePtr(_)) && method == "offset" {
            let elem = match &od {
                Type::RawPtr(e) | Type::VolatilePtr(e) => (**e).clone(),
                _ => Type::Byte,
            };
            let step = match &elem {
                Type::Byte | Type::Bool => 1i64,
                Type::Char => 4,
                Type::Int | Type::Float | Type::Float32 | Type::Int32 => 8,
                Type::Struct(sn) => self.struct_byte_size(&Type::Struct(sn.clone())),
                _ => 8,
            };
            let (ov, _) = self.lower_expr(obj, &od)?;
            let (nv, _) = self.lower_expr(&args[0], &Type::Int)?;
            // 字节偏移 = n × 步长（乘法在 64 位里做，溢出与 C 侧一致）。
            let prod = self.new_reg(IrType::I64);
            self.emit(IrInst::Bin { op: ABin::Mul, dst: prod, a: nv,
                b: Value::Imm(Imm::I64(step)), ty: IrType::I64 });
            let p = self.new_reg(IrType::Ptr);
            self.emit(IrInst::PtrAdd { dst: p, base: ov.as_reg(), off: Value::Reg(prod) });
            return Ok((Value::Reg(p), false));
        }
        let spec = match builtin::lookup_method(&od, method) {
            Some(s) => s,
            None => return self.err(&format!("asm 后端：方法 `{}` 暂不支持（后续相位）", method)),
        };
        let (ov, _) = self.lower_expr(obj, &od)?;
        // COW 写回目标：接收者必须是变量（与 LLVM 后端 gen_stmt 的 container_inplace 一致）。
        let obj_slot = if spec.writeback {
            if let Expr::Var(n) = obj { self.lookup(n).map(|l| l.ptr) } else { None }
        } else { None };

        let mut vals: Vec<Value> = vec![ov];
        let mut tys: Vec<IrType> = vec![IrType::Handle];
        // v1.10：保留各实参的原始值（get_or 的 string 默认值结算需要）
        // 与装箱后的位模式（get_or 的 struct 默认值 box 归还需要）。
        let mut raw_args: Vec<Value> = Vec::with_capacity(args.len());
        let mut coerced: Vec<Value> = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            let abi = spec.args.get(i).copied().unwrap_or(builtin::Abi::Auto);
            let bit_mode = spec.elem_arg == Some(i) || spec.is_kv;
            let hint = if bit_mode || abi == builtin::Abi::Auto {
                self.expr_dt(a)
            } else {
                builtin::abi_hint(abi)
            };
            let (v, _) = self.lower_expr(a, &hint)?;
            raw_args.push(v.clone());
            let v2 = if bit_mode {
                // 数组元素 / KvMap 键与值：按实参静态类型转成 C 的 int64 位模式。
                self.coerce_elem_bits(v, &self.expr_dt(a))?
            } else {
                self.coerce_to_abi(v, a, abi).0
            };
            coerced.push(v2.clone());
            vals.push(v2);
            tys.push(builtin::abi_ir(abi));
        }
        if spec.extra_one {
            // set.add(k) → dial_map_put(m, k, 1)
            vals.push(Value::Imm(Imm::I64(1)));
            tys.push(IrType::I64);
        }

        let ret_ir = spec.ret_ir.clone();
        let dst = if ret_ir == IrType::Void { None } else { Some(self.new_reg(ret_ir.clone())) };
        self.emit(IrInst::Call {
            dst, func: spec.sym.to_string(), args: vals, arg_tys: tys,
            ret_ty: ret_ir, linkage: CallLink::Runtime,
        });
        let mut out = match dst {
            None => Value::Imm(Imm::I64(0)),
            Some(d) => Value::Reg(d),
        };
        // v1.10 B6：Call 返回的原始位模式（struct 的 box 指针），在 elem_ret 解箱后
        // 用于 remove/get_or 的 box 归还（解箱是按值 memcpy，释放不影响副本）。
        let raw_out = out.clone();
        // C 侧谓词 0/1 → Dial bool 严格 0/1。
        if spec.post != builtin::Post::None {
            let r = self.new_reg(IrType::I64);
            let op = if spec.post == builtin::Post::NeqZero { ACmp::Neq } else { ACmp::Eq };
            self.emit(IrInst::ICmp { op, dst: r, a: out, b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
            out = Value::Reg(r);
        }
        // 数组元素 / KvMap 值：按元素类型把 int64 位模式还原。
        if spec.elem_ret {
            let et = match &od {
                Type::Array(e) => (**e).clone(),
                Type::KvMap(_, v) => (**v).clone(),
                _ => Type::Int,
            };
            if matches!(&et, Type::Struct(_)) {
                // v1.10 B6：map get/get_or/remove 的 struct 值可能未命中（bits==0），
                // 走跨块 phi 还原零值；数组元素恒非 null，但走此路也安全（0 分支不会命中）。
                // 对齐 codegen.rs::kv_from_bits。
                out = self.unbox_struct_nullable(out, &et)?;
            } else {
                out = self.reinterpret_bits(out, reg_ir(&et))?;
            }
        }
        // COW：只写回新句柄，绝不 release 旧值（C 侧已结算）。
        if let (Some(slot), Some(d)) = (obj_slot, dst) {
            self.emit(IrInst::Store { ptr: slot, val: Value::Reg(d), ty: IrType::Handle });
        }
        // v1.10：get_or 的 string 默认值结算（对齐 LLVM settle_fresh_value）。
        // C 侧 dial_kvmap_get_or 已对返回值 +1（调用方退出作用域 release）；
        // 这里归还默认实参的本地那份：字面量等自带所有权的值 rc=1 需 release，
        // 借用变量 no-op（否则悬垂/双重释放）。
        if method == "get_or" {
            if let Type::KvMap(_, v) = &od {
                if matches!(v.as_ref(), Type::String) {
                    if let Some(def_v) = raw_args.get(1) {
                        if let Some(k) = rc_kind(&Type::String) {
                            if !yields_borrowed_ref(&args[1]) {
                                let tmp = self.emit_alloca(reg_ir(&Type::String));
                                self.emit(IrInst::Store { ptr: tmp, val: def_v.clone(), ty: reg_ir(&Type::String) });
                                self.emit(IrInst::RcRelease { ptr: tmp, kind: k });
                            }
                        }
                    }
                } else if matches!(v.as_ref(), Type::Struct(_)) {
                    // v1.10 B6：struct 默认值 coerce 时新建 box（rc=1）；C 侧 get_or 对
                    // struct 不 retain，未命中返回的就是这份默认 box。解箱按值拷出后归还它
                    // （命中时归还的是那份未被使用的默认 box，同样正确）。对齐 codegen.rs:6111。
                    if let Some(def_bits) = coerced.get(1) {
                        self.release_box(def_bits.clone());
                    }
                }
            }
        }
        // v1.10 B6：remove 的 struct 值——C 侧把被删条目的值所有权移交调用方
        // （只 release 键、不 release 值），解箱按值拷出后须归还这个值 box，否则泄漏。
        // 未命中时 raw_out=0，dial_rc_release(NULL) 是 no-op，安全。对齐 codegen.rs:6091。
        if method == "remove" {
            if let Type::KvMap(_, v) = &od {
                if matches!(v.as_ref(), Type::Struct(_)) {
                    self.release_box(raw_out);
                }
            }
        }
        // v1.10 B6：put 的 struct 值——coerce 时新建 box（rc=1），C 侧 kvmap_put 又
        // retain 进表里（rc=2），归还本地这份让表独占（rc=1）。对齐 codegen.rs 的
        // sink_elem_after_retain。
        if method == "put" {
            if let Type::KvMap(_, v) = &od {
                if matches!(v.as_ref(), Type::Struct(_)) {
                    if let Some(val_bits) = coerced.get(1) {
                        self.release_box(val_bits.clone());
                    }
                }
            }
        }
        Ok((out, false))
    }

    /// M7-e3an：高阶数组方法 map / filter / sort_by / reduce 的专用 lowering。
    /// 对齐 LLVM codegen.rs gen_method 的 (Type::Array, 高阶方法) 分支：
    ///   - 生成精确签名回调 thunk（__dial_hof_<arity><elem><ret>），登记到 hof_thunks；
    ///   - 求值回调为 fn_raw（具名函数→FuncAddr，函数指针变量→load 出的地址）；
    ///   - state 显式透传（缺省空 rawptr<void>）；reduce 另有 init 位模式实参；
    ///   - 调用 dial_array_hof(op/out_kind) 或 dial_array_reduce；
    ///   - sort_by 把 COW 后新指针写回接收变量槽；reduce 把 i64 位模式还原成 elem。
    fn lower_hof_method(
        &mut self,
        obj: &Expr,
        od: &Type,
        method: &str,
        args: &[Expr],
    ) -> Result<(Value, bool), String> {
        let elem = match od {
            Type::Array(e) => (**e).clone(),
            _ => return self.err("asm 后端：高阶方法仅支持数组"),
        };

        // 回调签名 fn(elem[, elem], rawptr<void>): ret —— 语义层已严格核对。
        let cb_dt = self.expr_dt(&args[0]);
        let (ps, cb_ret) = match &cb_dt {
            Type::FnPtr(ps, ret) => (ps.clone(), (**ret).clone()),
            _ => return self.err("asm 后端：高阶方法首参必须是函数指针"),
        };
        // ps 末位是 state(rawptr<void>)，前面 1/2 个是 elem → arity。
        let arity = ps.len().saturating_sub(1);
        if arity == 0 || arity > 2 {
            return self.err("asm 后端：高阶回调 arity 仅支持 1 或 2");
        }

        // thunk 的 ret：map 用回调返回类型；filter=Bool；sort_by=Int；reduce=elem。
        let thunk_ret = match method {
            "map" => cb_ret.clone(),
            "filter" => Type::Bool,
            "sort_by" => Type::Int,
            "reduce" => elem.clone(),
            _ => return self.err("asm 后端：未知高阶方法"),
        };
        // 登记 thunk 描述（去重 + 统一生成在 lower_program）。
        let key = (elem.clone(), thunk_ret.clone(), arity);
        if !self.hof_thunks.contains(&key) {
            self.hof_thunks.push(key);
        }
        let thunk_name = format!(
            "__dial_hof_{}_{}_{}", arity, hof_code(&elem), hof_code(&thunk_ret)
        );

        // 接收者数组指针。
        let (ov, _) = self.lower_expr(obj, od)?;

        // fn_raw：具名函数求值为 FuncAddr，函数指针变量求值为 load 出的地址。
        let (fn_raw, _) = self.lower_expr(&args[0], &cb_dt)?;

        // state：map/filter/sort_by 是第 2 参，reduce 是第 3 参；缺省空指针。
        let state_idx = if method == "reduce" { 2 } else { 1 };
        let state = if args.len() > state_idx {
            let st = self.lower_expr(&args[state_idx], &Type::RawPtr(Box::new(Type::Void)))?;
            st.0
        } else {
            Value::Imm(Imm::I64(0))
        };

        // thunk 函数地址。
        let thunk_addr = self.new_reg(IrType::FnPtr);
        self.emit(IrInst::FuncAddr { dst: thunk_addr, func: thunk_name });

        if method == "reduce" {
            // init：第 2 参，按 elem 静态类型压成 i64 槽位位模式。
            let (init_v, _) = self.lower_expr(&args[1], &elem)?;
            let init_bits = self.coerce_elem_bits(init_v, &elem)?;
            let dst = self.new_reg(IrType::I64);
            self.emit(IrInst::Call {
                dst: Some(dst),
                func: "dial_array_reduce".to_string(),
                args: vec![ov, init_bits, fn_raw, state, Value::Reg(thunk_addr)],
                arg_tys: vec![IrType::Handle, IrType::I64, IrType::FnPtr, IrType::Ptr, IrType::FnPtr],
                ret_ty: IrType::I64,
                linkage: CallLink::Runtime,
            });
            // i64 位模式 → elem（标量经 8 字节槽重解释）。
            let out = self.reinterpret_bits(Value::Reg(dst), reg_ir(&elem))?;
            Ok((out, false))
        } else {
            let op: i64 = match method {
                "map" => 0,
                "filter" => 1,
                "sort_by" => 2,
                _ => unreachable!(),
            };
            let out_elem = if method == "map" { &thunk_ret } else { &elem };
            let out_kind = builtin::array_elem_kind(out_elem) as i64;
            let dst = self.new_reg(IrType::Handle);
            self.emit(IrInst::Call {
                dst: Some(dst),
                func: "dial_array_hof".to_string(),
                args: vec![
                    ov,
                    Value::Imm(Imm::I64(op)),
                    Value::Imm(Imm::I64(out_kind)),
                    fn_raw,
                    state,
                    Value::Reg(thunk_addr),
                ],
                arg_tys: vec![
                    IrType::Handle, IrType::I64, IrType::I64,
                    IrType::FnPtr, IrType::Ptr, IrType::FnPtr,
                ],
                ret_ty: IrType::Handle,
                linkage: CallLink::Runtime,
            });
            // sort_by 原地排序（COW 后可能换 buffer）：写回接收变量槽，与 sort 一致。
            if method == "sort_by" {
                if let Expr::Var(n) = obj {
                    if let Some(slot) = self.lookup(n).map(|l| l.ptr) {
                        self.emit(IrInst::Store { ptr: slot, val: Value::Reg(dst), ty: IrType::Handle });
                    }
                }
            }
            Ok((Value::Reg(dst), false))
        }
    }

    /// 数组 remove(i) / pop()：返回被删元素（按元素类型还原），并把 COW 新指针
    /// 写回接收变量。
    ///
    /// 堆元素的所有权结算（对齐 LLVM 后端 codegen.rs:5640）：
    /// C 侧 `dial_array_remove` / `dial_array_pop` 会把该元素从数组里**摘走并释放**
    /// （数组不再是它的持有者），因此返回值必须额外 retain 一份，
    /// 否则调用方拿到的就是一个已释放的悬垂句柄。
    fn lower_array_remove(&mut self, obj: &Expr, od: &Type, method: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        let elem = match od {
            Type::Array(e) => (**e).clone(),
            _ => return self.err("asm 后端：remove/pop 仅支持数组"),
        };
        let (ov, _) = self.lower_expr(obj, od)?;
        let idx: Value = if method == "pop" {
            let len = self.new_reg(IrType::I64);
            self.emit(IrInst::Call { dst: Some(len), func: "dial_array_length".to_string(),
                args: vec![ov], arg_tys: vec![IrType::Handle], ret_ty: IrType::I64, linkage: CallLink::Runtime });
            let last = self.new_reg(IrType::I64);
            self.emit(IrInst::Bin { op: ABin::Sub, dst: last, a: Value::Reg(len), b: Value::Imm(Imm::I64(1)), ty: IrType::I64 });
            Value::Reg(last)
        } else {
            self.lower_expr(&args[0], &Type::Int)?.0
        };
        let got = self.new_reg(IrType::I64);
        self.emit(IrInst::Call { dst: Some(got), func: "dial_array_get".to_string(),
            args: vec![ov, idx], arg_tys: vec![IrType::Handle, IrType::I64],
            ret_ty: IrType::I64, linkage: CallLink::Runtime });
        let removed = self.reinterpret_bits(Value::Reg(got), reg_ir(&elem))?;
        // 堆元素：C 侧 remove/pop 会把它从数组摘走并释放（数组不再是持有者），
        // 故返回值需额外 retain 一份，否则调用方拿到的是已释放的悬垂句柄。
        // RcRetain 作用于「槽地址」（内部会 load 出句柄值），故先落临时槽。
        if let Some(k) = rc_kind(&elem) {
            let tmp = self.emit_alloca(reg_ir(&elem));
            self.emit(IrInst::Store { ptr: tmp, val: removed.clone(), ty: reg_ir(&elem) });
            self.emit(IrInst::RcRetain { ptr: tmp, kind: k });
        }
        let newp = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call {
            dst: Some(newp),
            func: if method == "pop" { "dial_array_pop" } else { "dial_array_remove" }.to_string(),
            args: if method == "pop" { vec![ov] } else { vec![ov, idx] },
            arg_tys: if method == "pop" { vec![IrType::Handle] } else { vec![IrType::Handle, IrType::I64] },
            ret_ty: IrType::Handle, linkage: CallLink::Runtime,
        });
        if let Expr::Var(n) = obj {
            if let Some(l) = self.lookup(n).cloned() {
                self.emit(IrInst::Store { ptr: l.ptr, val: Value::Reg(newp), ty: IrType::Handle });
            }
        }
        Ok((removed, false))
    }

    // ---- 函数调用 ----

    /// M7-e3aq：把单个调用实参按**形参静态类型**求值并适配到 C ABI 宽度。
    /// 显式实参和补齐的默认实参走同一条路径，保证 ABI 处理完全一致。
    /// struct 形参按值传递：先拷到调用方栈上临时区，再把临时区地址传给被调方
    /// （被调方 memcpy 出自己的副本），避免共享存储破坏值语义。
    fn lower_call_arg(&mut self, a: &Expr, at: &Type) -> Result<Value, String> {
        let (v, _) = self.lower_expr(a, at)?;
        let v = self.adapt_param(v, at, at);
        if let Type::Struct(_) = at {
            let tmp = self.emit_alloca(IrType::Struct(struct_name_of(at)));
            self.emit_memcpy(tmp, v, self.struct_byte_size(at));
            Ok(Value::Reg(tmp))
        } else {
            Ok(v)
        }
    }

    fn lower_call(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        if name == "print" || name == "eprint" || name == "print_raw" || name == "eprint_raw" || name == "write_char" {
            if args.is_empty() { return self.err(&format!("{} 至少需要一个参数", name)); }
            return self.lower_print(name, args);
        }
        // str(x)：统一字符串化入口，按实参类型分派到对应的 to_string 运行时函数
        // （对齐 LLVM 后端 codegen.rs:5250）。char 走 UTF-8 编码，字符串直接返回自身。
        if name == "str" {
            if args.is_empty() { return self.err("str 至少需要一个参数"); }
            let at = self.expr_dt(&args[0]);
            let (av, _) = self.lower_expr(&args[0], &at)?;
            let sym = match &at {
                Type::Int | Type::Int32 => "dial_int_to_string",
                Type::Float | Type::Float32 => "dial_float_to_string",
                Type::Char => "dial_char_to_string",
                Type::String => return Ok((av, false)),
                other => return self.err(&format!(
                    "[asm 后端] str 仅支持 int / int32 / float / float32 / char / string 参数（收到 {}）",
                    other.display())),
            };
            // int32 / float32 的内部宽度窄于 C 侧的 int64_t / double，需先扩展。
            let arg = match &at {
                Type::Int32 => {
                    let r = self.new_reg(IrType::I64);
                    self.emit(IrInst::Conv { kind: AConv::Sext, dst: r, a: av, ty: IrType::I64 });
                    Value::Reg(r)
                }
                Type::Float32 => {
                    let r = self.new_reg(IrType::F64);
                    self.emit(IrInst::Conv { kind: AConv::FpExt, dst: r, a: av, ty: IrType::F64 });
                    Value::Reg(r)
                }
                _ => av,
            };
            let h = self.new_reg(IrType::Handle);
            let aty = match &at {
                Type::Float | Type::Float32 => IrType::F64,
                _ => IrType::I64,
            };
            self.emit(IrInst::Call { dst: Some(h), func: sym.to_string(),
                args: vec![arg], arg_tys: vec![aty],
                ret_ty: IrType::Handle, linkage: CallLink::Runtime });
            return Ok((Value::Reg(h), false));
        }
        // 文件 IO / with 自动释放 / 裸指针视图是「带失败抛错的 CFG」或「需要 moved 记账」
        // 的专用路径，不走普通内建表（builtin::special_ret 仅用于类型推断）。
        match name {
            "open" | "read" | "read_all" | "file_read_line" | "write" | "eof"
            | "file_exists" | "file_size" | "file_remove" | "close" | "__with_release"
            // M7-e3ak：drop(x) 提前释放所有权（move 语义），与 __with_release 同属
            // 需要 moved 记账的专用路径。对齐 LLVM codegen.rs:4939。
            | "ptr_cast" | "drop" =>
                return self.lower_special_call(name, args),
            // M7-e3ag：编译期布局内建。复用 ir.rs 的 ABI 布局单点真理（与 LLVM
            // DataLayout 对齐），在编译期算出常量并返回 Value::Imm，零运行时指令。
            // 必须在此拦截——否则会落到下方 lower_builtin 的 BKind::Unsupported 报错。
            "sizeof" | "alignof" | "offsetof" =>
                return self.lower_layout_call(name, args),
            // M7-e3ah：底层位模式重解释 bitcast<T>(x)。标量走 8 字节槽 store→load；
            // struct↔struct 同尺寸走零拷贝槽地址重解释（FieldAddr 按目标 struct_name
            // 解读同一块内存，等价 LLVM 同尺寸聚合零开销 bitcast）。必须在此拦截——
            // 否则会落到下方 lower_builtin 的 BKind::Unsupported 报错。
            "bitcast" => return self.lower_bitcast(args),
            // M7-e3as：内联汇编 @asm<T>(...) / @asm_intel<T>(...)。对齐 LLVM
            // codegen.rs:6113 gen_inline_asm：模板 / 约束原样交付，操作数按 C ABI
            // 宽度进出。必须在此拦截——否则落到下方 lower_builtin 的
            // BKind::Unsupported 报错。仅 x86-64：AArch64 方言语义不同，Fail-Fast。
            "asm" | "asm_intel" => return self.lower_inline_asm(name, args),
            // M7-e3az：simd<T, N>(...) 向量构造。对齐 LLVM gen_simd_construct：
            // 1 个实参广播、N 个实参逐 lane。必须在此拦截——否则落到下方
            // lower_builtin 的 BKind::Unsupported 报错。
            "simd" => return self.lower_simd_construct(args),
            // M7-e3ai：assert / panic 的 Fail-Fast 专用路径。对齐 LLVM 后端
            // emit_fail_loc：assert 条件失败 / panic 都打印「消息 + file:line:col」
            // 后终止进程。必须在此拦截——否则落到下方 lower_builtin 的
            // BKind::Unsupported 报错。
            "assert" | "panic" => return self.lower_assert_panic(name, args),
            // M7-e3aj：句柄判空 is_null(x)。对齐 LLVM codegen.rs:5031 的
            // ptrtoint + icmp eq null：句柄在 IR 里就是指针，与 0 做一次 Eq
            // 比较即得严格 0/1 的 bool，零函数调用。语义层 is_nullable_type
            // 已保证实参是可空句柄（标量在编译期被拒），两端准入名单同源。
            // 必须在此拦截——否则落到下方 lower_builtin 的 BKind::Unsupported。
            "is_null" => return self.lower_is_null(args),
            // M7-e3al：channel 原语族（单线程可测部分）。对齐 LLVM
            // codegen.rs:4984-5160：channel/recv/try_recv/recv_timeout/closed/
            // done/len 是 C 调用薄封装，move_to 是零拷贝所有权移交 intrinsic。
            // 必须在此拦截——否则落到 lower_builtin 的 BKind::Unsupported。
            "channel" | "recv" | "try_recv" | "recv_timeout" | "closed" | "done"
            | "len" | "move_to" => return self.lower_channel_call(name, args),
            // M7-e3am：真多线程原语 spawn/join/detach。对齐 LLVM codegen.rs:5109/5139。
            // spawn：分配 argcell + 移交实参 + 生成线程入口 thunk + dial_thread_new；
            // join/detach：C 调用 + 槽清零（join 另标 moved 防二次回收）。
            // 必须在此拦截——否则落到 lower_builtin 的 BKind::Unsupported。
            "spawn" | "join" | "detach" => return self.lower_thread_call(name, args),
            _ => {}
        }
        // functions_sig 只含用户函数与 extern 声明；按名字硬编码分派的内建
        // （语义层 RESERVED_NAMES 设防，不会与之撞名）走表驱动分派。
        let sig = match self.ip.functions_sig.get(name) {
            Some(s) => s.clone(),
            None => {
                // 名字不是函数，但可能是**持有函数指针的变量** —— 间接调用
                // （语义层 codegen 对应处：type_of 的 FnPtr 分支，签名严格匹配）。
                if let Some(Type::FnPtr(ps, ret)) = self.lookup(name).map(|l| l.dt.clone()) {
                    return self.lower_call_indirect(name, &ps, &ret, args);
                }
                return self.lower_builtin(name, args);
            }
        };
        let ret_dt = sig.ret.clone();
        let mut arg_vals = Vec::new();
        let mut arg_tys = Vec::new();
        for (i, a) in args.iter().enumerate() {
            // C 变参（FFI varargs）的尾部实参超出声明形参表：此前一律 fallback 到
            // Type::Int，导致 `demo_vprintf(fmt, 42, 3.5, true)` 里的 3.5 被当整数
            // 位模式塞进 %rdx（而非 %xmm0），C 侧 va_arg 读 double 得 0.0、bool 读垃圾。
            // 修复（对齐 LLVM codegen.rs:5413）：尾部实参用其**实际静态类型**；bool /
            // byte / char 按 C 默认参数提升为 Int（i64，匹配用例的 %ld 读取）。固定参数
            // 仍严格用声明类型（语义层已校验）。
            let at = if sig.variadic && i >= sig.params.len() {
                match self.expr_dt(a) {
                    Type::Bool | Type::Byte | Type::Char => Type::Int,
                    other => other,
                }
            } else {
                sig.params.get(i).cloned().unwrap_or(Type::Int)
            };
            // v1.21 consume 形参：own 实参变量在调用点标 moved（所有权随调用移交），
            // 这样作用域退出时 release_scope 不再释放它——被调方入口接管（且不 +1
            // retain），退出时 -1，正好配对。与 sema check_moved_expr 的 mark_moved
            // 严格对应（那里负责拒绝调用后使用，这里负责 ARC 记账）。
            if sig.own_params.get(i).copied().unwrap_or(false) {
                if let Expr::Var(vn) = a {
                    if let Some(l) = self.lookup_mut(vn) { l.moved = true; }
                }
            }
            let v = self.lower_call_arg(a, &at)?;
            arg_vals.push(v);
            arg_tys.push(param_ir(&at));
        }
        // M7-e3aq：默认实参补齐（对齐 LLVM gen_call，codegen.rs:5384）。
        // 缺失的尾部参数按声明顺序用 FuncInfo.defaults 里的**常量表达式**补齐，
        // 每次调用求值一次。此前 asm 完全缺失此步：少传的参数在栈槽里是未初始化
        // 垃圾（实测 add(5) 得到 102 而非 15），影响所有带默认值的函数与 impl 方法。
        // 变参（FFI varargs）尾部无默认值定义，跳过。
        if !sig.variadic {
            for (i, d) in sig.defaults.iter().enumerate() {
                if i >= arg_vals.len() {
                    if let Some(e) = d {
                        let at = sig.params.get(i).cloned().unwrap_or(Type::Int);
                        let v = self.lower_call_arg(e, &at)?;
                        arg_vals.push(v);
                        arg_tys.push(param_ir(&at));
                    }
                }
            }
        }
        let linkage = if sig.exported { CallLink::Extern } else { CallLink::Dial };
        let ret_ir = param_ir(&ret_dt);
        let dst = if ret_dt == Type::Void { None } else { Some(self.new_reg(ret_ir.clone())) };
        self.emit(IrInst::Call { dst, func: name.to_string(), args: arg_vals, arg_tys, ret_ty: ret_ir.clone(), linkage });
        // 调用返回的是 ABI 宽度；转回内部宽度（bool：i32→I64 零扩展）。
        match dst {
            None => Ok((Value::Imm(Imm::I64(0)), false)),
            Some(d) => {
                if let Type::Struct(sn) = &ret_dt {
                    // 结构体返回值：被调方交回的是**堆上 box 的 payload 指针**
                    // （见 Stmt::Return 的装箱说明）。调用方 memcpy 出自己的
                    // 内容区副本，再 release 那个 box（它归调用方所有）。
                    let content = self.emit_alloca(IrType::Struct(sn.clone()));
                    self.emit_memcpy(content, Value::Reg(d), self.struct_byte_size(&ret_dt));
                    self.emit(IrInst::Call { dst: None, func: "dial_rc_release".to_string(),
                        args: vec![Value::Reg(d)], arg_tys: vec![IrType::Handle],
                        ret_ty: IrType::Void, linkage: CallLink::Runtime });
                    // 副本里的堆字段需对称 +1（值拷贝不带所有权），
                    // 与 LLVM 后端的 retain_struct_fields 对齐。
                    self.emit(IrInst::StructRetain { ptr: content, struct_name: sn.clone() });
                    return Ok((Value::Reg(content), false));
                }
                let internal = reg_ir(&ret_dt);
                if internal != ret_ir {
                    let r2 = self.new_reg(internal.clone());
                    self.emit(IrInst::Conv { kind: AConv::Zext, dst: r2, a: Value::Reg(d), ty: internal });
                    Ok((Value::Reg(r2), false))
                } else {
                    Ok((Value::Reg(d), false))
                }
            }
        }
    }

    /// M7-e3ai：assert / panic 的 Fail-Fast 专用路径（对齐 LLVM 后端
    /// codegen.rs 的 gen_builtin 中 assert/panic 分支 + emit_fail_loc）。
    ///
    /// C 运行时符号（stdlib/dial_exc.c）：
    ///   - `dial_assert_fail_loc(what, file, line, col)`：打印断言失败 + 位置后 abort。
    ///   - `dial_panic_loc(what, file, line, col)`：打印未捕获异常 + 位置后 abort。
    /// 二者都要求 `what` 是 **const char\***：Dial string 句柄须先经
    /// `dial_string_cstr` 跳过 8 字节 ARC 头（与 lower_throw_value 的未捕获路径、
    /// LLVM 后端同款，少这一步会打印出头的字节）。
    ///
    /// 位置取当前语句的 stmt_pos（lower_stmt 入口已写入 self.cur_pos），
    /// 文件名是编译期常量串（intern 进 .rodata），行列是编译期常量整数——
    /// 运行期只有一次 printf + abort，与 LLVM 后端口径逐字一致。
    ///
    /// assert 是**带条件的控制流**：条件为真走 ok 块（返回 0 继续），为假才进
    /// fail 块调 dial_assert_fail_loc 后 Unreachable（绝不静默继续）。
    fn lower_assert_panic(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        let is_assert = name == "assert";
        if is_assert {
            if args.is_empty() || args.len() > 2 {
                return self.err("assert 需要 1 或 2 个参数（bool 条件 [，string 消息]）");
            }
        } else if args.len() != 1 {
            return self.err("panic 需要 1 个 string 参数");
        }

        // fail / panic 分支：把消息 DialString → const char*。
        // 断言可为单参（无消息），此时用编译期常量串 "assert 失败"。
        let build_fail = |l: &mut Lower<'a>| -> Result<(), String> {
            let msg_cstr = if is_assert && args.len() == 1 {
                let lit = l.ip.intern_string("assert 失败");
                let p = l.new_reg(IrType::Ptr);
                l.emit(IrInst::LeaLabel { dst: p, lit });
                Value::Reg(p)
            } else {
                let msg_idx = if is_assert { 1 } else { 0 };
                let (mv, _) = l.lower_expr(&args[msg_idx], &Type::String)?;
                let cstr = l.new_reg(IrType::Ptr);
                l.emit(IrInst::Call { dst: Some(cstr), func: "dial_string_cstr".to_string(),
                    args: vec![mv], arg_tys: vec![IrType::Handle],
                    ret_ty: IrType::Ptr, linkage: CallLink::Runtime });
                Value::Reg(cstr)
            };
            // 文件名编译期常量串 → const char*。
            let file_lit = l.ip.intern_string(&l.src_file.clone());
            let file_p = l.new_reg(IrType::Ptr);
            l.emit(IrInst::LeaLabel { dst: file_p, lit: file_lit });
            let (line, col) = l.cur_pos;
            let callee = if is_assert { "dial_assert_fail_loc" } else { "dial_panic_loc" };
            l.emit(IrInst::Call { dst: None, func: callee.to_string(),
                args: vec![msg_cstr, Value::Reg(file_p),
                           Value::Imm(Imm::I64(line as i64)), Value::Imm(Imm::I64(col as i64))],
                arg_tys: vec![IrType::Ptr, IrType::Ptr, IrType::I64, IrType::I64],
                ret_ty: IrType::Void, linkage: CallLink::Runtime });
            // dial_*_loc 不返回（abort）；显式 Unreachable，绝不让后续语句落进死块。
            l.terminate(Term::Unreachable);
            Ok(())
        };

        if is_assert {
            // 条件：语义层保证 args[0] 是 bool。
            let (cv, _) = self.lower_expr(&args[0], &Type::Bool)?;
            let cond = self.cond_reg(cv)?;
            let ok_b = self.new_block(Vec::new());
            let fail_b = self.new_block(Vec::new());
            self.terminate(Term::Branch { cond, then_b: ok_b, else_b: fail_b,
                then_args: Vec::new(), else_args: Vec::new() });
            self.set_cur(fail_b);
            build_fail(self)?;
            self.set_cur(ok_b);
            // assert 返回 0（对齐 LLVM 的 zero）——语句语境下该值被丢弃。
            Ok((Value::Imm(Imm::I64(0)), false))
        } else {
            // panic：无条件终止。后续语句落进新建的不可达块。
            build_fail(self)?;
            let dead = self.new_block(Vec::new());
            self.set_cur(dead);
            Ok((Value::Imm(Imm::I64(0)), false))
        }
    }

    // ---- 表驱动的内建分派 ----

    /// 间接调用：被调目标是**函数指针变量**（fn(A,B):R）。
    /// 签名严格匹配（语义层已核对实参类型），调用目标从寄存器取出后 `call *%reg`。
    fn lower_call_indirect(
        &mut self,
        name: &str,
        ps: &[Type],
        ret: &Type,
        args: &[Expr],
    ) -> Result<(Value, bool), String> {
        if args.len() != ps.len() {
            return self.err(&format!(
                "[asm 后端] 函数指针 '{}' 需要 {} 个参数，但传入了 {} 个",
                name, ps.len(), args.len()
            ));
        }
        let (fv, _) = self.lower_expr(&Expr::Var(name.to_string()), &Type::Void)?;
        let mut arg_vals = Vec::new();
        let mut arg_tys = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let pt = ps[i].clone();
            let (v, _) = self.lower_expr(a, &pt)?;
            let v = self.adapt_param(v, &pt, &pt);
            arg_vals.push(v);
            arg_tys.push(param_ir(&pt));
        }
        let ret_ir = param_ir(ret);
        let dst = if *ret == Type::Void { None } else { Some(self.new_reg(ret_ir.clone())) };
        self.emit(IrInst::CallIndirect {
            dst, target: fv.as_reg(), args: arg_vals, arg_tys, ret_ty: ret_ir.clone(),
        });
        match dst {
            None => Ok((Value::Imm(Imm::I64(0)), false)),
            Some(d) => {
                let internal = reg_ir(ret);
                if internal != ret_ir {
                    let r2 = self.new_reg(internal.clone());
                    self.emit(IrInst::Conv { kind: AConv::Zext, dst: r2, a: Value::Reg(d), ty: internal });
                    Ok((Value::Reg(r2), false))
                } else {
                    Ok((Value::Reg(d), false))
                }
            }
        }
    }

    /// functions_sig 未命中时调用：查 src/builtin.rs 的分派表。
    fn lower_builtin(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        let spec = match builtin::lookup(name) {
            Some(s) => s,
            None => return self.err(&format!("asm 后端：未找到函数 `{}`（FFI/内建未覆盖）", name)),
        };
        let ret_dt = spec.ret.clone();
        match spec.kind {
            builtin::BKind::Unsupported(why) =>
                return self.err(&format!("asm 后端：内建 `{}` 暂不支持——{}", name, why)),
            // char/byte 与 int 在 IR 里同为 I64：零指令类型视图切换。
            builtin::BKind::Identity => {
                let dt = self.expr_dt(&args[0]);
                let (v, _) = self.lower_expr(&args[0], &dt)?;
                Ok((v, false))
            }
            builtin::BKind::Conv { kind, to_ir } => {
                let dt = self.expr_dt(&args[0]);
                let (v, _) = self.lower_expr(&args[0], &dt)?;
                let d = self.new_reg(to_ir.clone());
                self.emit(IrInst::Conv { kind, dst: d, a: v, ty: to_ir });
                Ok((Value::Reg(d), false))
            }
            builtin::BKind::MinMax { is_min } => self.lower_minmax(is_min, args),
            builtin::BKind::Abs => self.lower_abs(args),
            builtin::BKind::Call { sym, args: abis, ret_ir, post } => {
                // new_kv_map 在「结构体值」场景由 parser 补第 3 个实参（结构体名
                // 字符串），但 C 的 dial_kvmap_new 只有两个 kind 参数。与 LLVM
                // 后端一致只取前两个，避免多生成一个字符串临时对象并造成泄漏。
                let eff: &[Expr] = if sym == "dial_kvmap_new" && args.len() > 2 { &args[..2] } else { args };
                let mut vals = Vec::new();
                let mut tys = Vec::new();
                for (i, a) in eff.iter().enumerate() {
                    let abi = abis.get(i).copied().unwrap_or(builtin::Abi::Auto);
                    let hint = if abi == builtin::Abi::Auto { self.expr_dt(a) } else { builtin::abi_hint(abi) };
                    let (v, _) = self.lower_expr(a, &hint)?;
                    let (v2, ty) = self.coerce_to_abi(v, a, abi);
                    vals.push(v2);
                    tys.push(ty);
                }
                let dst = if ret_ir == IrType::Void { None } else { Some(self.new_reg(ret_ir.clone())) };
                self.emit(IrInst::Call {
                    dst, func: sym.to_string(), args: vals, arg_tys: tys,
                    ret_ty: ret_ir.clone(), linkage: CallLink::Runtime,
                });
                let mut v = match dst {
                    None => return Ok((Value::Imm(Imm::I64(0)), false)),
                    Some(d) => Value::Reg(d),
                };
                // C 侧谓词返回 0/1（或任意非零）→ 规整成 Dial bool 的严格 0/1。
                if post != builtin::Post::None {
                    let r = self.new_reg(IrType::I64);
                    let op = if post == builtin::Post::NeqZero { ACmp::Neq } else { ACmp::Eq };
                    self.emit(IrInst::ICmp { op, dst: r, a: v, b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                    v = Value::Reg(r);
                }
                // C ABI 返回宽度 → IR 内部宽度（仅精确宽度标量需要转换）。
                let internal = reg_ir(&ret_dt);
                if internal != ret_ir
                    && !matches!(internal, IrType::Handle | IrType::Ptr | IrType::Void)
                    && !matches!(ret_ir, IrType::Handle | IrType::Ptr | IrType::Void)
                {
                    let r = self.new_reg(internal.clone());
                    let kind = match (&ret_ir, &internal) {
                        (IrType::F32, IrType::F64) => AConv::FpExt,
                        (IrType::F64, IrType::F32) => AConv::FpTrunc,
                        (_, IrType::I64) => AConv::Sext,
                        _ => AConv::Trunc,
                    };
                    self.emit(IrInst::Conv { kind, dst: r, a: v, ty: internal });
                    v = Value::Reg(r);
                }
                Ok((v, false))
            }
        }
    }

    // ---- 文件 IO / with 自动释放：带失败抛错的专用路径 ----

    /// 构造一个 Error 句柄（DialString → dial_error_new），返回其 vreg。
    fn make_error_handle(&mut self, msg: &str) -> Result<VReg, String> {
        let lit = self.ip.intern_string(msg);
        let p = self.new_reg(IrType::Ptr);
        self.emit(IrInst::LeaLabel { dst: p, lit });
        let s = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(s), func: "dial_string_new".to_string(),
            args: vec![Value::Reg(p)], arg_tys: vec![IrType::Ptr],
            ret_ty: IrType::Handle, linkage: CallLink::Runtime });
        let e = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(e), func: "dial_error_new".to_string(),
            args: vec![Value::Reg(s)], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Handle, linkage: CallLink::Runtime });
        Ok(e)
    }

    /// 若 `cond_v` 为真（I64 0/1）则跳到 fail 块抛 Error，否则落到 ok 块继续。
    fn fail_throw_if(&mut self, cond_v: VReg, msg: &str) -> Result<(), String> {
        let ok = self.new_block(Vec::new());
        let fail = self.new_block(Vec::new());
        self.terminate(Term::Branch { cond: cond_v, then_b: fail, else_b: ok,
            then_args: Vec::new(), else_args: Vec::new() });
        self.set_cur(fail);
        let err = self.make_error_handle(msg)?;
        self.lower_throw_value(Value::Reg(err))?; // 终结 fail 块（throw 后不可达）
        self.set_cur(ok);
        Ok(())
    }

    /// 句柄为 NULL（ptr == 0）时抛 Error。`h` 必须是 Handle 类型 vreg。
    fn fail_throw_if_null(&mut self, h: VReg, msg: &str) -> Result<(), String> {
        let is_null = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Eq, dst: is_null, a: Value::Reg(h),
            b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
        self.fail_throw_if(is_null, msg)
    }

    /// M7-e3az：`simd<T, N>(...)` 向量构造。对齐 LLVM `gen_simd_construct`：
    /// 第一个实参是 `Expr::TypeLit(Type::Simd(elem, n))`（parser 塞入），其余是
    /// lane 值。1 个 lane 值→广播到全部 N 个；N 个→逐 lane。返回一个 Ptr vreg
    /// 指向 N×8 字节的向量栈段（每 lane 存 Dial 标量内部宽度的位模式）。
    fn lower_simd_construct(&mut self, args: &[Expr]) -> Result<(Value, bool), String> {
        let (elem, n) = match args.first() {
            Some(Expr::TypeLit(Type::Simd(e, n))) => ((**e).clone(), *n),
            _ => return self.err("simd 构造缺少类型实参 `simd<T, N>`"),
        };
        let lanes_expr = &args[1..];
        if lanes_expr.len() != 1 && lanes_expr.len() != n as usize {
            return self.err(&format!(
                "simd<{}, {}> 构造需要 1 个（广播）或 {} 个实参，实际 {} 个",
                elem.display(), n, n, lanes_expr.len()));
        }
        // 逐 lane 求值（用元素 Dial 类型作 hint，确保 float 走 F64、窄整型正确）。
        let mut lane_vals: Vec<Value> = Vec::with_capacity(n as usize);
        for e in lanes_expr {
            let (v, _) = self.lower_expr(e, &elem)?;
            lane_vals.push(v);
        }
        // 广播：复制第一份到 N 份。
        if lane_vals.len() == 1 {
            lane_vals = vec![lane_vals[0]; n as usize];
        }
        // 分配 N×8 字节栈段（每 lane 一个 8 字节槽，存 Dial 内部位模式）。
        let dst = self.emit_simd_alloca(n);
        let lane_ty = simd_lane_ir(&elem);
        for (i, v) in lane_vals.into_iter().enumerate() {
            self.simd_store_lane(dst, i as u32, v, lane_ty.clone());
        }
        Ok((Value::Reg(dst), false))
    }

    // ---- M7-e3ag：编译期布局内建 sizeof / alignof / offsetof ----
    // 复用 ir.rs 的 ABI 布局「单点真理」（struct_layout / field_size / field_ir），
    // 其口径与 LLVM 后端 TargetData（DataLayout）逐字节对齐。返回 Imm → 零运行时指令。

    /// 某类型的 (字节大小, 对齐)。口径对齐 LLVM `dialc_type_to_llvm` + TargetData。
    fn layout_size_align(&self, t: &Type) -> Result<(i64, i64), String> {
        match t {
            // 语义层已拒绝 void，这里只做防御。
            Type::Void => self.err("布局内建不接受 void 类型"),
            // M7-e3az：向量 ABI 布局与 LLVM `<N x T>` 对齐：size=N×原生元素宽度，
            // align=size 向上取 2 的幂（上限 32，覆盖 byte16=16 / int4=32）。
            Type::Simd(elem, n) => {
                let es = simd_elem_size(elem) as i64;
                let size = es * *n as i64;
                let align = if size <= 1 { 1 } else { (1i64 << (64 - (size - 1).leading_zeros())).min(32) };
                Ok((size, align))
            }
            Type::Struct(n) => {
                let pk = self.packed.contains(n);
                let (_offs, size, align) = self.ip.struct_layout(n, pk);
                // 关键修正：LLVM/C 的 packed 对齐固定为 1；struct_layout 对 packed
                // 仍返回字段自然对齐（如 8），直接复用会与 LLVM/C 不一致。
                Ok((size, if pk { 1 } else { align }))
            }
            _ => {
                // 标量 + 内建容器：LLVM 把 String/Array/Map/RawPtr/Handle 等一律
                // 擦除为 i8ptr（8 字节/8 对齐）；field_ir 将这些映射为 Ptr/Handle，
                // field_size=8、对齐=8，天然一致。bool=I1(1)，i32/f32=4。
                let size = field_size(t) as i64;
                let align = match field_ir(t) {
                    IrType::I1 | IrType::I8 => 1,
                    IrType::I32 | IrType::F32 => 4,
                    _ => 8,
                };
                Ok((size, align))
            }
        }
    }

    /// offsetof：字段在结构体内的字节偏移（按 packed 口径）。
    fn layout_offset(&self, t: &Type, field: &str) -> Result<i64, String> {
        let n = match t {
            Type::Struct(n) => n.clone(),
            _ => return self.err("offsetof 的第一个参数必须是结构体类型"),
        };
        let idx = match self.struct_field(t, field) {
            Some((i, _)) => i,
            None => return self.err(&format!("offsetof：结构体 '{}' 没有字段 '{}'", n, field)),
        };
        let pk = self.packed.contains(&n);
        let (offs, _, _) = self.ip.struct_layout(&n, pk);
        Ok(offs[idx])
    }

    /// sizeof/alignof/offsetof 统一入口。args[0] 是 Expr::TypeLit（语义层已校验），
    /// offsetof 的 args[1] 是 Expr::StringLit（字段名）。不 lower 实参本身——
    /// TypeLit 无运行时值、StringLit lower 会生成运行时字符串对象（错误）。
    fn lower_layout_call(&self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        let ty = match args.get(0) {
            Some(Expr::TypeLit(t)) => t.clone(),
            _ => return self.err(&format!("{} 的第一个参数必须是类型实参", name)),
        };
        let v = match name {
            "sizeof" => self.layout_size_align(&ty)?.0,
            "alignof" => self.layout_size_align(&ty)?.1,
            "offsetof" => {
                let field = match args.get(1) {
                    Some(Expr::StringLit(f)) => f.clone(),
                    _ => return self.err("offsetof 的第二个参数必须是字段名"),
                };
                self.layout_offset(&ty, &field)?
            }
            _ => unreachable!("lower_layout_call 仅处理 sizeof/alignof/offsetof"),
        };
        // 与整型字面量 lower_expr(Expr::IntLit) 完全同形，emit 全链已支持无 vreg 的 Imm。
        Ok((Value::Imm(Imm::I64(v)), false))
    }

    /// M7-e3aj：is_null(x) —— 句柄判空，对齐 LLVM codegen.rs:5031。
    ///
    /// LLVM 侧实现：v.into_pointer_value() → ptrtoint i64 → icmp eq 0 → i1。
    /// asm 侧所有句柄值本身就是「指针语义的 8 字节槽值」（IrType::Ptr/Handle），
    /// 直接 emit 一条 ICmp(Eq, x, 0) 即得严格 0/1 的 bool（IrType::I64），
    /// 完全等价，零函数调用、零运行时依赖。
    ///
    /// 准入名单由语义层 is_nullable_type 统一把关（String/Array/Map/Set/KvMap/
    /// Error/Channel/Thread/File/Opaque），两端同源；标量在编译期已被拒，
    /// 因此这里无需再做类型判断。参数按自身静态类型 lower，避免被误当标量。
    fn lower_is_null(&mut self, args: &[Expr]) -> Result<(Value, bool), String> {
        if args.len() != 1 {
            return self.err("is_null 需要 1 个参数（句柄）");
        }
        let dt = self.expr_dt(&args[0]);
        let (v, _) = self.lower_expr(&args[0], &dt)?;
        let r = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp {
            op: ACmp::Eq,
            dst: r,
            a: v,
            b: Value::Imm(Imm::I64(0)),
            ty: IrType::I64,
        });
        Ok((Value::Reg(r), false))
    }

    /// M7-e3al：channel 原语族 —— 对齐 LLVM codegen.rs:4984-5160。
    /// 单线程可测的部分全部覆盖；spawn/thread（真多线程）仍属后续相位。
    ///
    /// - `channel(cap)` → `dial_channel_new(cap, elem_release)`：元素析构器按
    ///   LLVM elem_release_fn 取 C 函数地址（FuncAddr）。默认 `dial_rc_release`
    ///   （覆盖 string/array/map/error 等 RC 句柄与标量——标量元素 C 侧根本不会
    ///   调析构）；嵌套 channel 元素 LLVM 用 `dial_channel_release`，但那需要真
    ///   并发才有意义，本批暂不覆盖（声明为已知边界，不静默给错值）。
    /// - `recv`/`try_recv`/`recv_timeout` → C 调用，队列弹出即移交所有权（不 retain）。
    /// - `closed`/`done` → C 调用返回 int64，ICmp(Neq, r, 0) → 严格 bool。
    /// - `len` → `dial_channel_len` → i64（int）。
    /// - `move_to(var, ch)` → load 槽值 → `dial_channel_send(ch, v)` → 槽清零 →
    ///   moved（零拷贝所有权移交，全程零 retain/release）。对齐 LLVM：语义层
    ///   已保证首参是变量、元素类型匹配。
    fn lower_channel_call(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        match name {
            "channel" => {
                if args.len() != 1 {
                    return self.err("channel 需要 1 个参数（容量 cap: int）");
                }
                let cap = self.lower_expr(&args[0], &Type::Int)?.0;
                let rel = self.new_reg(IrType::FnPtr);
                self.emit(IrInst::FuncAddr { dst: rel, func: "dial_rc_release".to_string() });
                let dst = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call {
                    dst: Some(dst), func: "dial_channel_new".to_string(),
                    args: vec![cap, Value::Reg(rel)],
                    arg_tys: vec![IrType::I64, IrType::FnPtr],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime,
                });
                Ok((Value::Reg(dst), false))
            }
            "recv" | "try_recv" => {
                if args.len() != 1 { return self.err(&format!("{} 需要 1 个参数（channel）", name)); }
                let ct = self.expr_dt(&args[0]);
                let ch = self.lower_expr(&args[0], &ct)?.0;
                let func = if name == "recv" { "dial_channel_recv" } else { "dial_channel_try_recv" };
                let dst = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call {
                    dst: Some(dst), func: func.to_string(), args: vec![ch],
                    arg_tys: vec![IrType::Handle], ret_ty: IrType::Handle,
                    linkage: CallLink::Runtime,
                });
                Ok((Value::Reg(dst), false))
            }
            "recv_timeout" => {
                if args.len() != 2 { return self.err("recv_timeout 需要 2 个参数（channel, 毫秒）"); }
                let ct = self.expr_dt(&args[0]);
                let ch = self.lower_expr(&args[0], &ct)?.0;
                let ms = self.lower_expr(&args[1], &Type::Int)?.0;
                let dst = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call {
                    dst: Some(dst), func: "dial_channel_recv_timeout".to_string(),
                    args: vec![ch, ms], arg_tys: vec![IrType::Handle, IrType::I64],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime,
                });
                Ok((Value::Reg(dst), false))
            }
            "closed" | "done" => {
                if args.len() != 1 { return self.err(&format!("{} 需要 1 个参数（channel）", name)); }
                let ct = self.expr_dt(&args[0]);
                let ch = self.lower_expr(&args[0], &ct)?.0;
                let func = if name == "closed" { "dial_channel_closed" } else { "dial_channel_done" };
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call {
                    dst: Some(r), func: func.to_string(), args: vec![ch],
                    arg_tys: vec![IrType::Handle], ret_ty: IrType::I64,
                    linkage: CallLink::Runtime,
                });
                // C 侧 int64 0/非0 → 严格 bool（对齐 LLVM 的 icmp ne 0）。
                let b = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op: ACmp::Neq, dst: b, a: Value::Reg(r),
                    b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                Ok((Value::Reg(b), false))
            }
            "len" => {
                if args.len() != 1 { return self.err("len 需要 1 个参数（channel）"); }
                let ct = self.expr_dt(&args[0]);
                let ch = self.lower_expr(&args[0], &ct)?.0;
                let dst = self.new_reg(IrType::I64);
                self.emit(IrInst::Call {
                    dst: Some(dst), func: "dial_channel_len".to_string(), args: vec![ch],
                    arg_tys: vec![IrType::Handle], ret_ty: IrType::I64,
                    linkage: CallLink::Runtime,
                });
                Ok((Value::Reg(dst), false))
            }
            "move_to" => {
                if args.len() != 2 { return self.err("move_to 需要 2 个参数（变量, 目标 channel）"); }
                let vname = match &args[0] {
                    Expr::Var(n) => n.clone(),
                    _ => return self.err("内部错误：move_to 的第一个参数必须是变量（应由语义层拦截）"),
                };
                let l = match self.lookup(&vname) {
                    Some(l) => l.clone(),
                    None => return self.err(&format!("内部错误：move_to 找不到变量 '{}'", vname)),
                };
                // 取变量当前值（load 槽），交给 channel_send，再清零原槽 + 标记 moved。
                let v = self.new_reg(IrType::Handle);
                self.emit(IrInst::Load { dst: v, ptr: l.ptr, ty: IrType::Handle });
                let ct = self.expr_dt(&args[1]);
                let ch = self.lower_expr(&args[1], &ct)?.0;
                self.emit(IrInst::Call {
                    dst: None, func: "dial_channel_send".to_string(),
                    args: vec![ch, Value::Reg(v)],
                    arg_tys: vec![IrType::Handle, IrType::Handle],
                    ret_ty: IrType::Void, linkage: CallLink::Runtime,
                });
                // 原槽清零（从这一刻起不再持有所有权）+ 标记 moved（作用域退出跳过 release）。
                self.emit(IrInst::Store { ptr: l.ptr, val: Value::Imm(Imm::I64(0)), ty: IrType::Handle });
                if let Some(sl) = self.lookup_mut(&vname) { sl.moved = true; }
                Ok((Value::Imm(Imm::I64(0)), false)) // Void 占位，对齐 LLVM
            }
            _ => unreachable!("lower_channel_call 仅处理 channel 原语族"),
        }
    }

    /// M7-e3am：真多线程原语 spawn / join / detach，对齐 LLVM codegen.rs:5109/5139。
    ///
    /// C runtime（stdlib/dial_thread.c）：
    ///   - `dial_thread_argcell(n)`：分配 n 个 8 字节槽（calloc，NULL 安全 Fail-Fast）。
    ///   - `dial_thread_new(fn, cell)`：pthread_create，返回 Thread 句柄。
    ///   - `dial_thread_join/join(t)`、`dial_thread_detach(t)`：回收/分离线程。
    ///
    /// spawn 的所有权会计（与 LLVM 同源，易错点）：
    ///   - 堆句柄（string/array/map/set/kvmap/error）：move 进槽（store 值 + 源槽清零
    ///     + 源标 moved），目标函数入口 retain、thunk 末 release，净零；
    ///   - channel：共享语义，retain +1 后 store（源变量仍可用）；
    ///   - 标量（int/bool/byte/…）与 float：按值 store（槽宽 8 字节）。
    /// 顺序铁律：**先写满所有槽，再 dial_thread_new**——pthread_create 之后实参必须已就位。
    fn lower_thread_call(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        if name == "join" || name == "detach" {
            if args.len() != 1 {
                return self.err(&format!("{} 需要 1 个参数（thread 句柄）", name));
            }
            let (tv, _) = self.lower_expr(&args[0], &Type::Thread)?;
            let h = tv.as_reg();
            let sym = if name == "join" { "dial_thread_join" } else { "dial_thread_detach" };
            self.emit(IrInst::Call { dst: None, func: sym.to_string(), args: vec![Value::Reg(h)],
                arg_tys: vec![IrType::Handle], ret_ty: IrType::Void, linkage: CallLink::Runtime });
            // 槽清零：二次 join/detach 会当场 Fail-Fast（dial_thread_join 的 HEAP_ASSERT）
            // 而非 use-after-free；join 即消费句柄，另标 moved 使 with 块末跳过 release。
            if let Expr::Var(vn) = &args[0] {
                let slot_ptr = self.lookup(vn).map(|l| l.ptr);
                if let Some(p) = slot_ptr {
                    self.emit(IrInst::Store { ptr: p, val: Value::Imm(Imm::I64(0)), ty: IrType::Handle });
                    if name == "join" {
                        if let Some(l) = self.lookup_mut(vn) { l.moved = true; }
                    }
                }
            }
            return Ok((Value::Imm(Imm::I64(0)), false)); // Void 占位
        }

        // ---- spawn ----
        let fname = match &args[0] {
            Expr::FuncRef(n) => n.clone(),
            _ => return self.err("spawn 的第一个参数必须是具名函数（见语义层约束）"),
        };
        let sig = self.ip.functions_sig.get(&fname)
            .ok_or_else(|| format!("spawn 引用了未定义的函数 `{}`", fname))?
            .clone();
        if args.len() - 1 != sig.params.len() {
            return self.err(&format!("spawn 传给 `{}` 的实参数目与形参不符", fname));
        }

        // 1) 分配 argcell：dial_thread_argcell(n)。
        let n = sig.params.len() as i64;
        let cell = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(cell), func: "dial_thread_argcell".to_string(),
            args: vec![Value::Imm(Imm::I64(n))], arg_tys: vec![IrType::I64],
            ret_ty: IrType::Handle, linkage: CallLink::Runtime });

        // 2) 逐实参写入 argcell 的第 i 个 8 字节槽。
        for (i, pty) in sig.params.iter().enumerate() {
            let (v, _) = self.lower_expr(&args[i + 1], pty)?;
            let slot = self.new_reg(IrType::Ptr);
            self.emit(IrInst::PtrAdd { dst: slot, base: cell, off: Value::Imm(Imm::I64((i * 8) as i64)) });
            match pty {
                // 共享：channel 原子 +1，源变量仍可用。
                Type::Channel(_) => {
                    if let Some(k) = rc_kind(pty) {
                        self.emit(IrInst::RcRetain { ptr: slot, kind: k });
                    }
                    self.emit(IrInst::Store { ptr: slot, val: v, ty: IrType::Handle });
                }
                // 移动：堆句柄 store 值 + 源槽清零 + 源标 moved（无 retain/release）。
                t if rc_kind(t).is_some() => {
                    self.emit(IrInst::Store { ptr: slot, val: v, ty: IrType::Handle });
                    if let Expr::Var(vn) = &args[i + 1] {
                        let slot_ptr = self.lookup(vn).map(|l| l.ptr);
                        if let Some(p) = slot_ptr {
                            self.emit(IrInst::Store { ptr: p, val: Value::Imm(Imm::I64(0)), ty: IrType::Handle });
                            if let Some(l) = self.lookup_mut(vn) { l.moved = true; }
                        }
                    }
                }
                // 标量 / float / 裸指针 / fnptr：按值 store（槽宽 8 字节）。
                _ => {
                    let sty = match pty {
                        Type::Float => IrType::F64,
                        Type::Float32 => IrType::F32,
                        t2 => reg_ir(t2),
                    };
                    self.emit(IrInst::Store { ptr: slot, val: v, ty: sty });
                }
            }
        }

        // 3) 记录 thunk 目标，取线程入口函数地址。
        self.spawn_targets.insert(fname.clone());
        let thunk_name = format!("__dial_thunk_{}", fname);
        let thunk_addr = self.new_reg(IrType::Ptr);
        self.emit(IrInst::FuncAddr { dst: thunk_addr, func: thunk_name });

        // 4) dial_thread_new(thunk, cell) → Thread 句柄。
        let t = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call { dst: Some(t), func: "dial_thread_new".to_string(),
            args: vec![Value::Reg(thunk_addr), Value::Reg(cell)],
            arg_tys: vec![IrType::Ptr, IrType::Handle], ret_ty: IrType::Handle,
            linkage: CallLink::Runtime });
        Ok((Value::Reg(t), false))
    }

    /// M7-e3ah：bitcast<T>(x) —— 底层位模式重解释，对齐 LLVM codegen.rs:4842-4924。
    /// 不改内存、不读源数据，只把 x 的位模式按目标类型 T 重新解读。
    ///
    /// asm 栈槽模型下的正确性依据：
    ///   - 标量：所有值都是 8 字节槽，reinterpret_bits 以 store(I64)→load(target)
    ///     重解释位模式，与整型字面量同形；
    ///   - struct：值是「指向栈槽的 Ptr vreg」，地址本身不携带 struct 类型标签；
    ///     FieldAddr 的偏移完全由指令里的 struct_name 显式查 struct_layout 得出，
    ///     从不读 base vreg 类型。故 struct↔struct 同尺寸只需返回源槽地址（零拷贝），
    ///     下游 FieldAddr 用目标 struct_name 即按新布局解读同一块内存。
    ///
    /// Fail-Fast 拒绝（绝不静默给错误值）：位宽不一致、array（堆 Handle 非连续栈槽，
    /// 无法做槽重解释）、simd（宽度口径与 LLVM 无法对齐）。
    fn lower_bitcast(&mut self, args: &[Expr]) -> Result<(Value, bool), String> {
        if args.len() < 2 {
            return self.err("bitcast 需要类型实参与源表达式，形如 bitcast<int>(x)");
        }
        let target = match &args[0] {
            Expr::TypeLit(t) => t.clone(),
            _ => return self.err("bitcast 的第一个参数必须是类型实参"),
        };
        let src_ty = self.expr_dt(&args[1]);
        let (v, _) = self.lower_expr(&args[1], &src_ty)?;

        // 位宽/尺寸校验（对齐 LLVM llvm_bit_width）：标量用 field_size×8；
        // struct 用 layout_size_align（区分 packed）；simd/array 明确拒绝。
        let bit_size = |t: &Type| -> Result<u64, String> {
            match t {
                Type::Simd(..) => self.err("bitcast 暂不支持 simd 类型（LLVM 按向量宽度计算，自研后端无法对齐）"),
                Type::Array(..) => self.err("bitcast 暂不支持 array 类型（数组是堆句柄，无法做同址位重解释）"),
                Type::Struct(_) => Ok(self.layout_size_align(t)?.0 as u64 * 8),
                _ => Ok(field_size(t) as u64 * 8),
            }
        };
        let src_bits = bit_size(&src_ty)?;
        let dst_bits = bit_size(&target)?;
        if src_bits != dst_bits {
            return self.err(&format!(
                "bitcast 源（{}，{} 位）与目标 '{}'（{} 位）位宽不一致，无法做位模式重解释",
                src_ty.display(), src_bits, target.display(), dst_bits));
        }

        // 类界限（对齐 LLVM codegen.rs:4888-4912）：聚合类型（struct）只能与同尺寸
        // 聚合互转，不能与指针 / 整数 / 标量互转。位宽相同也不行——这是 LLVM 的
        // 「指针 / 整数 / 聚合」三类硬界限。bit_size 已拒绝 array，这里只处理 struct。
        let src_is_struct = matches!(src_ty, Type::Struct(_));
        let dst_is_struct = matches!(target, Type::Struct(_));
        if src_is_struct != dst_is_struct {
            return self.err(&format!(
                "bitcast 不能在结构体与非结构体之间转换（源 {} → 目标 '{}'）；请先用 rawptr 指向那块内存再按字段读取",
                src_ty.display(), target.display()));
        }

        // 同类型恒等（reg_ir 相同）：int↔byte↔char、bool↔bool、各 Ptr/Handle/FnPtr、
        // 同名 struct 等——直接返回源值，零指令。与 LLVM src_ty==dst_ty 恒等分支等价。
        if reg_ir(&src_ty) == reg_ir(&target) {
            return Ok((v, false));
        }

        // struct↔struct 同尺寸：源 v 已是槽地址 Ptr vreg，尺寸校验通过→零拷贝返回，
        // 下游 FieldAddr 按目标 struct_name 解读同一块内存。
        if matches!(src_ty, Type::Struct(_)) && matches!(target, Type::Struct(_)) {
            return Ok((v, false));
        }

        // 标量跨类重解释：int↔float(F64)、int32↔float32(F32) 等。
        // ptr↔int 因 reg_ir 同为 Ptr/I64 已在上面恒等分支覆盖。
        let result = self.reinterpret_bits(v, reg_ir(&target))?;
        Ok((result, false))
    }

    /// M7-e3as：内联汇编 @asm<T>(template, constraints, args...) —— 对齐 LLVM
    /// codegen.rs:6113 gen_inline_asm。语义层（semantic.rs:797-875）已强制：
    /// 类型实参是标量/裸指针、模板与约束是字符串字面量、约束首项是 =/+ 输出、
    /// 输入标量/裸指针。这里把实参按 C ABI 宽度适配（param_ir）后交给 IR，
    /// 模板里的 `$N`/`$$` 占位符与物理寄存器分配留到 codegen_asm.rs 发射期。
    /// 返回值的 C ABI 宽度→Dial 内部宽度的适配也在这里用 Conv 补齐（对齐 LLVM
    /// abi_adapt：ret_ty=param_ir 窄→ reg_ir 宽用 Zext）。
    fn lower_inline_asm(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        if args.len() < 3 {
            return self.err("内联汇编形如 @asm<T>(\"模板\", \"约束\", 输入...)，至少需要类型实参、模板与约束");
        }
        let ret_ty_dial = match &args[0] {
            Expr::TypeLit(t) => t.clone(),
            _ => return self.err("内联汇编第一参数必须是类型实参 @asm<T>(...)"),
        };
        let template = match &args[1] {
            Expr::StringLit(s) => s.clone(),
            _ => return self.err("内联汇编模板必须是字符串字面量"),
        };
        let constraints = match &args[2] {
            Expr::StringLit(s) => s.clone(),
            _ => return self.err("内联汇编约束必须是字符串字面量"),
        };
        let dialect = if name == "asm_intel" { AsmDialect::Intel } else { AsmDialect::Att };

        // AArch64 方言语义与 x86 的 AT&T/Intel 不同，模板原样交付在交叉目标上
        // 无法保证正确——Fail-Fast 明确拒绝（普通程序不含 asm，零影响）。
        // 真正的 target 判定在 emit() 预检里（lower 阶段拿不到 target）。

        // 返回值 C ABI 宽度（byte→i8、bool→i32、其余同 reg）。
        let ret_abi = param_ir(&ret_ty_dial);
        let ret_internal = reg_ir(&ret_ty_dial);

        // 输入实参：lower + 按 C ABI 宽度适配（对齐 LLVM abi_adapt 的窄化方向）。
        let mut in_vals: Vec<Value> = Vec::new();
        let mut in_tys: Vec<IrType> = Vec::new();
        for a in args.iter().skip(3) {
            let at = self.expr_dt(a);
            let (v, _) = self.lower_expr(a, &at)?;
            let adapted = self.adapt_param(v, &at, &at);
            in_tys.push(param_ir(&at));
            in_vals.push(adapted);
        }

        // C ABI 宽度 → Dial 内部宽度：byte(i8)→i64、bool(i32)→i64 用零扩展，
        // 与 LLVM abi_adapt(raw, ret_llvm, dialc_type) 完全一致。先算判据再移动 ret_abi。
        let need_widen = ret_abi != ret_internal;
        let dst = self.new_reg(ret_abi.clone());
        self.emit(IrInst::InlineAsm {
            dst, template, constraints, dialect,
            args: in_vals, arg_tys: in_tys, ret_ty: ret_abi,
        });

        if need_widen {
            let wide = self.new_reg(ret_internal.clone());
            self.emit(IrInst::Conv { kind: AConv::Zext, dst: wide,
                a: Value::Reg(dst), ty: ret_internal });
            return Ok((Value::Reg(wide), false));
        }
        Ok((Value::Reg(dst), false))
    }

    /// 文件 IO 与 __with_release：与 codegen.rs:4596–4768 的 CFG / moved 记账同构。
    /// 文件句柄 = 不透明 ptr，**不受 ARC 管理**：必须显式 close，作用域不自动释放。
    fn lower_special_call(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        match name {
            // ptr_cast<T>(x)：把 x 转成裸指针视图。
            //   - 源是裸指针：直接取其地址（对外一律 i8*，目标类型不改变 IR）；
            //   - 源是 string / bytes：借出其**数据缓冲区**（跳过 8 字节 payload 头）。
            // 借用得到的指针**不受 ARC 保护**：原对象释放后即悬垂，由使用者负责。
            "ptr_cast" => {
                if args.len() < 2 {
                    return self.err("内部错误：ptr_cast 需要类型实参与源值");
                }
                let src_ty = self.expr_dt(&args[1]);
                let (src, _) = self.lower_expr(&args[1], &src_ty)?;
                match src_ty {
                    Type::RawPtr(_) | Type::VolatilePtr(_) => Ok((src, false)),
                    Type::String | Type::Bytes => {
                        // payload 布局：[ int64 len ][ data... ]，数据从 +8 开始。
                        let off = self.new_reg(IrType::I64);
                        self.emit(IrInst::Conv { kind: AConv::Zext, dst: off,
                            a: Value::Imm(Imm::I64(8)), ty: IrType::I64 });
                        let p = self.new_reg(IrType::Ptr);
                        self.emit(IrInst::PtrAdd { dst: p, base: src.as_reg(), off: Value::Reg(off) });
                        Ok((Value::Reg(p), false))
                    }
                    other => self.err(&format!(
                        "[asm 后端] ptr_cast 收到 {} 类型的源（仅支持裸指针 / string / bytes）", other.display())),
                }
            }
            // open(path, mode)：DialString → DialString，返回 NULL 抛错。
            "open" => {
                let (p, _) = self.lower_expr(&args[0], &Type::String)?;
                let (m, _) = self.lower_expr(&args[1], &Type::String)?;
                let h = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(h), func: "dial_file_open".to_string(),
                    args: vec![p, m], arg_tys: vec![IrType::Handle, IrType::Handle],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                self.fail_throw_if_null(h, "无法打开文件（路径不存在或无访问权限）")?;
                Ok((Value::Reg(h), false))
            }
            // read(f, n) / read_all(f) / file_read_line(f)：NULL 抛错。
            "read" => {
                let (f, _) = self.lower_expr(&args[0], &Type::File)?;
                let (n, _) = self.lower_expr(&args[1], &Type::Int)?;
                let h = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(h), func: "dial_file_read".to_string(),
                    args: vec![f, n], arg_tys: vec![IrType::Handle, IrType::I64],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                self.fail_throw_if_null(h, "读取文件失败（句柄无效或内存不足）")?;
                Ok((Value::Reg(h), false))
            }
            "read_all" | "file_read_line" => {
                let (f, _) = self.lower_expr(&args[0], &Type::File)?;
                let sym = if name == "read_all" { "dial_file_read_all" } else { "dial_file_read_line" };
                let h = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(h), func: sym.to_string(),
                    args: vec![f], arg_tys: vec![IrType::Handle],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                self.fail_throw_if_null(h, "读取文件失败（句柄无效或内存不足）")?;
                Ok((Value::Reg(h), false))
            }
            // write(f, content)：返回 -1 抛错。
            "write" => {
                let (f, _) = self.lower_expr(&args[0], &Type::File)?;
                let (c, _) = self.lower_expr(&args[1], &Type::String)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_file_write".to_string(),
                    args: vec![f, c], arg_tys: vec![IrType::Handle, IrType::Handle],
                    ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let fail = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op: ACmp::Eq, dst: fail, a: Value::Reg(r),
                    b: Value::Imm(Imm::I64(-1)), ty: IrType::I64 });
                self.fail_throw_if(fail, "写入文件失败（句柄无效或写入中断）")?;
                Ok((Value::Reg(r), false))
            }
            // eof(f)：feof，!= 0 → bool。
            "eof" => {
                let (f, _) = self.lower_expr(&args[0], &Type::File)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_file_eof".to_string(),
                    args: vec![f], arg_tys: vec![IrType::Handle],
                    ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let b = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op: ACmp::Neq, dst: b, a: Value::Reg(r),
                    b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                Ok((Value::Reg(b), false))
            }
            // file_exists / file_remove：!= 0 → bool；file_size：int。
            "file_exists" | "file_remove" | "file_size" => {
                let sym = if name == "file_exists" { "dial_file_exists" }
                    else if name == "file_remove" { "dial_file_remove" } else { "dial_file_size" };
                let (p, _) = self.lower_expr(&args[0], &Type::String)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: sym.to_string(),
                    args: vec![p], arg_tys: vec![IrType::Handle],
                    ret_ty: IrType::I64, linkage: CallLink::Runtime });
                if name == "file_size" {
                    Ok((Value::Reg(r), false))
                } else {
                    let b = self.new_reg(IrType::I64);
                    self.emit(IrInst::ICmp { op: ACmp::Neq, dst: b, a: Value::Reg(r),
                        b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                    Ok((Value::Reg(b), false))
                }
            }
            // close(f)：file → dial_file_close（空指针安全）；显式关闭即消费句柄，标记 moved
            // 使 with 块末的自动释放跳过它（fclose 二次调用是 UB）。thread/channel 后续相位。
            "close" => {
                let dt = self.expr_dt(&args[0]);
                let (v, _) = self.lower_expr(&args[0], &dt)?;
                match dt {
                    Type::File => {
                        self.emit(IrInst::Call { dst: None, func: "dial_file_close".to_string(),
                            args: vec![v], arg_tys: vec![IrType::Handle],
                            ret_ty: IrType::Void, linkage: CallLink::Runtime });
                        if let Expr::Var(n) = &args[0] {
                            if let Some(l) = self.lookup_mut(n) { l.moved = true; }
                        }
                    }
                    // M7-e3al：channel 关闭。对齐 LLVM codegen.rs:4720——**不标 moved**：
                    // channel 受 ARC 管理，标记 moved 会让作用域退出跳过 release 而泄漏。
                    Type::Channel(_) => {
                        self.emit(IrInst::Call { dst: None, func: "dial_channel_close".to_string(),
                            args: vec![v], arg_tys: vec![IrType::Handle],
                            ret_ty: IrType::Void, linkage: CallLink::Runtime });
                    }
                    other => return self.err(&format!(
                        "asm 后端：close 暂不支持 {} 类型（仅 file/channel；thread 后续相位）", other.display())),
                }
                Ok((Value::Imm(Imm::I64(0)), false))
            }
            // __with_release(v)：with 块末自动释放（内部调用，用户代码不可达）。
            //   块内已显式 close 过 → moved → 跳过；否则按类型释放。
            "__with_release" => {
                let vname = match &args[0] {
                    Expr::Var(n) => n.clone(),
                    _ => return self.err("内部错误：__with_release 的参数必须是变量名"),
                };
                let l = match self.lookup(&vname) {
                    Some(l) => l.clone(),
                    None => return self.err(&format!("内部错误：__with_release 找不到变量 '{}'", vname)),
                };
                if l.moved { return Ok((Value::Imm(Imm::I64(0)), false)); }
                let v = self.new_reg(IrType::Handle);
                self.emit(IrInst::Load { dst: v, ptr: l.ptr, ty: IrType::Handle });
                match l.dt {
                    Type::File => self.emit(IrInst::Call { dst: None, func: "dial_file_close".to_string(),
                        args: vec![Value::Reg(v)], arg_tys: vec![IrType::Handle],
                        ret_ty: IrType::Void, linkage: CallLink::Runtime }),
                    Type::Thread => self.emit(IrInst::Call { dst: None, func: "dial_thread_join".to_string(),
                        args: vec![Value::Reg(v)], arg_tys: vec![IrType::Handle],
                        ret_ty: IrType::Void, linkage: CallLink::Runtime }),
                    Type::RawPtr(_) | Type::VolatilePtr(_) => self.emit(IrInst::Call { dst: None,
                        func: "dial_raw_free".to_string(), args: vec![Value::Reg(v)],
                        arg_tys: vec![IrType::Handle], ret_ty: IrType::Void, linkage: CallLink::Runtime }),
                    _ => {}
                }
                if let Some(sl) = self.lookup_mut(&vname) { sl.moved = true; }
                Ok((Value::Imm(Imm::I64(0)), false))
            }
            // M7-e3ak：drop(x) —— 提前释放一个变量的所有权（move 语义），
            // 对齐 LLVM codegen.rs:4939。语义层已保证参数是 Expr::Var 且非
            // Opaque/FnPtr；这里按变量静态类型分派：
            //   - File → dial_file_close、Thread → dial_thread_join（显式资源）；
            //   - RawPtr/VolatilePtr → dial_raw_free（裸指针「谁申请谁释放」）；
            //   - ARC 句柄 → RcRelease(slot, rc_kind)（槽地址，发射器内部 load 后 release）；
            //   - struct（字段各自 ARC）→ StructRelease；
            //   - 标量 → no-op（LLVM release_value 的 `_ => return`）。
            // 释放后标记 moved，使作用域退出时的 release_scope 跳过它（防双释放/UAF）。
            "drop" => {
                let vname = match &args[0] {
                    Expr::Var(n) => n.clone(),
                    _ => return self.err("内部错误：drop 的参数必须是变量名（应由语义层拦截）"),
                };
                let l = match self.lookup(&vname) {
                    Some(l) => l.clone(),
                    None => return self.err(&format!("内部错误：drop 找不到变量 '{}'", vname)),
                };
                // 已 moved 的变量再 drop 是 no-op：对齐 LLVM 的幂等行为（moved 槽
                // 二次 release 是 UAF），这里直接跳过整段发射。
                if !l.moved {
                    match &l.dt {
                        // 显式资源 / 裸指针：先 load 出槽里的句柄**值**，再传给 C 函数
                        // （与 __with_release / close 同款；RcRelease/StructRelease 则
                        // 相反——它们作用于**槽地址**，发射器内部负责 load）。
                        dt @ (Type::File | Type::Thread | Type::RawPtr(_) | Type::VolatilePtr(_)) => {
                            let v = self.new_reg(IrType::Handle);
                            self.emit(IrInst::Load { dst: v, ptr: l.ptr, ty: IrType::Handle });
                            let func = match dt {
                                Type::File => "dial_file_close",
                                Type::Thread => "dial_thread_join",
                                _ => "dial_raw_free",
                            };
                            self.emit(IrInst::Call { dst: None, func: func.to_string(),
                                args: vec![Value::Reg(v)], arg_tys: vec![IrType::Handle],
                                ret_ty: IrType::Void, linkage: CallLink::Runtime });
                        }
                        // struct / 标量：对齐 LLVM release_value 的 `_ => return`（no-op）。
                        // struct 的字段 ARC 由作用域退出的 release_scope 单独管理；
                        // 这里若提前 StructRelease 会与 LLVM 不一致且 drop 后用字段 UAF。
                        // rc_kind 对 struct/标量返回 None，自然落到 no-op。
                        dt => if let Some(k) = rc_kind(dt) {
                            self.emit(IrInst::RcRelease { ptr: l.ptr, kind: k });
                        },
                    }
                }
                if let Some(sl) = self.lookup_mut(&vname) { sl.moved = true; }
                Ok((Value::Imm(Imm::I64(0)), false)) // Void 占位，对齐 LLVM
            }
            _ => self.err(&format!("asm 后端：未找到函数 `{}`（FFI/内建未覆盖）", name)),
        }
    }

    /// with <resource> as <name> { ... }：复用既有的 defer 机制实现自动释放，
    /// 与 codegen.rs:3730 作用域/defer 帧结构同构。
    fn lower_with(&mut self, resource: &Expr, name: &str, body: &[Stmt]) -> Result<(), String> {
        // 1) 在**外层**作用域里求值资源表达式（语义与普通表达式一致）。
        let rt = self.expr_dt(resource);
        let (rv, _) = self.lower_expr(resource, &rt)?;
        // 2) 进入 with 作用域。
        self.push_scope();
        // 3) 把资源绑定到块内名字。file 句柄不是 ARC 类型，is_handle=false。
        let slot = self.emit_alloca(reg_ir(&rt));
        self.emit(IrInst::Store { ptr: slot, val: rv, ty: reg_ir(&rt) });
        let is_h = rc_kind(&rt).is_some();
        self.declare(name, Local { ptr: slot, ty: reg_ir(&rt), dt: rt.clone(), is_handle: is_h, moved: false });
        // 4) 登记块末自动释放（复用 defer：覆盖正常落穿 / return / break / continue / throw
        //    全部出口，且各出口互斥各执行一次）。块内若已显式 close，会自动跳过。
        self.defers.push((self.scopes.len(), Stmt::Expr(
            Expr::Call("__with_release".to_string(), vec![Expr::Var(name.to_string())]), 0, 0)));
        // 5) 生成块体。
        for s in body { self.lower_stmt(s)?; }
        // 6) 离开 with 作用域：排空本层 defer（含自动释放）再释放块内局部。
        //    若块体提前 return/break 已排空，leave_scope 因 moved 标记而变为空操作（无指令
        //    写入已终结块）。
        self.leave_scope()?;
        Ok(())
    }

    /// 把已 lower 出的值摆进内建所需的 C ABI 通道。
    /// 指针 / 句柄与 I64 同为 8 字节整数通道，只需改 arg_ty，不改值本身。
    fn coerce_to_abi(&mut self, v: Value, a: &Expr, abi: builtin::Abi) -> (Value, IrType) {
        let target = builtin::abi_ir(abi);
        let src = reg_ir(&self.expr_dt(a));
        if src == target { return (v, target); }
        let (kind, dst_ty) = match (&src, &target) {
            (IrType::F32, IrType::F64) => (AConv::FpExt, IrType::F64),
            (IrType::F64, IrType::F32) => (AConv::FpTrunc, IrType::F32),
            (IrType::I32, IrType::I64) => (AConv::Sext, IrType::I64),
            (_, IrType::I32) => (AConv::Trunc, IrType::I32),
            (_, IrType::I8) => (AConv::Trunc, IrType::I8),
            _ => return (v, target),
        };
        let d = self.new_reg(dst_ty.clone());
        self.emit(IrInst::Conv { kind, dst: d, a: v, ty: dst_ty });
        (Value::Reg(d), target)
    }

    /// 用块参数做一个「已有值」的 SSA 三目（不重复求值，避免副作用被放大）。
    fn select_values(&mut self, cond: VReg, a: Value, b: Value, ty: IrType) -> VReg {
        let tb = self.new_block(Vec::new());
        let eb = self.new_block(Vec::new());
        let mb = self.new_block(Vec::new());
        let p = self.new_reg(ty.clone());
        self.func.blocks[mb as usize].params.push((p, ty));
        self.terminate(Term::Branch { cond, then_b: tb, else_b: eb, then_args: Vec::new(), else_args: Vec::new() });
        self.set_cur(tb);
        self.terminate(Term::Jump { target: mb, args: vec![a] });
        self.set_cur(eb);
        self.terminate(Term::Jump { target: mb, args: vec![b] });
        self.set_cur(mb);
        p
    }

    /// 把元素值按 C 侧的 int64 位模式重解释。数组元素在 C 运行时里统一按
    /// int64_t 槽存放（float 是 double 的位模式，f32 是低 32 位）。
    /// M7-e3ao：嵌套下标赋值的递归左值解析（对齐 LLVM `gen_assign_lvalue`）。
    ///
    /// `lhs` 形如 `m[i][j]`（`Expr::Index(Index(Var, i), j)`）：从最外层变量槽递归
    /// 下钻，每层用 `dial_array_set_borrowed` 写入（内层数组是从外层元素槽**借来**的
    /// 引用，绝不能被消耗），把「可能 COW 更新」的新句柄写回外层容器，直到最外层
    /// 变量槽，最后结算每层 value 的所有权。
    ///
    /// - `value` / `value_dt`：本层要写入的元素值（已 lower 成内部位模式）及其类型；
    /// - `is_borrowed`：value 是否借来的——借来的来源仍持有，set 内部已 retain 一份
    ///   归数组所有，调用方不得再释放；自带所有权的值（字面量 / clone）须归还一份。
    fn lower_assign_lvalue(
        &mut self,
        lhs: &Expr,
        value: Value,
        value_dt: &Type,
        is_borrowed: bool,
    ) -> Result<(), String> {
        match lhs {
            Expr::Var(name) => {
                // 最外层：把新容器句柄写回变量槽，释放槽里被替换的旧引用。
                // 对齐 LLVM gen_assign_lvalue 的 Var 分支（load old → store → release old）。
                let l = match self.lookup(name).cloned() {
                    Some(x) => x,
                    None => return self.err(&format!("未声明的变量 '{}'", name)),
                };
                let old = self.new_reg(IrType::Handle);
                self.emit(IrInst::Load { dst: old, ptr: l.ptr, ty: IrType::Handle });
                self.emit(IrInst::Store { ptr: l.ptr, val: value, ty: IrType::Handle });
                // 释放被替换的旧容器引用（RcRelease 作用于槽地址，先落进临时槽）。
                if let Some(k) = rc_kind(&l.dt) {
                    let tmp = self.emit_alloca(IrType::Handle);
                    self.emit(IrInst::Store { ptr: tmp, val: Value::Reg(old), ty: IrType::Handle });
                    self.emit(IrInst::RcRelease { ptr: tmp, kind: k });
                }
                Ok(())
            }
            Expr::Index(container, idx) => {
                // container（如 m[i]）求值得到的是从外层元素槽**借来**的内层数组句柄；
                // 其自身的写回由下一层递归负责。这里只负责把 value 写进 container。
                let cty = self.expr_dt(container);
                let (carr, _) = self.lower_expr(container, &cty)?;
                let (iv, _) = self.lower_expr(idx, &Type::Int)?;
                // Fail-Fast：越界写由 C 侧 dial_array_set_borrowed 内的 DIAL_CHECK_BOUNDS
                // 当场 trap（与单层 dial_array_set 同口径，ReleaseFast 下消除），无需 IR 层重复。
                let bits = self.coerce_elem_bits(value, value_dt)?;
                let new_child = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call {
                    dst: Some(new_child), func: "dial_array_set_borrowed".to_string(),
                    args: vec![carr, iv, bits.clone()],
                    arg_tys: vec![IrType::Handle, IrType::I64, IrType::I64],
                    ret_ty: IrType::Handle, linkage: CallLink::Runtime,
                });
                // 递归把新容器写回 container 左值。new_child 是运行时新对象、自带
                // 所有权（borrowed=false）；递归到最外层 Var 分支时由槽负责持有它。
                self.lower_assign_lvalue(container, Value::Reg(new_child), &cty, false)?;
                // 结算本层 value：set_borrowed 为元素 retain 了一份归数组所有，
                // 借来的值来源仍持有不得再释放；自带所有权的值须归还这一份。
                if matches!(value_dt, Type::Struct(_)) {
                    // v1.10 B6：struct 值 coerce 时无条件新建 box，set_borrowed 又 +1，归还本地这份。
                    self.release_box(bits.clone());
                } else if !is_borrowed {
                    if let Some(k) = rc_kind(value_dt) {
                        let tmp = self.emit_alloca(reg_ir(value_dt));
                        self.emit(IrInst::Store { ptr: tmp, val: value, ty: reg_ir(value_dt) });
                        self.emit(IrInst::RcRelease { ptr: tmp, kind: k });
                    }
                }
                Ok(())
            }
            _ => self.err("[asm 后端] 嵌套下标赋值目标必须是变量或数组下标"),
        }
    }

    fn coerce_elem_bits(&mut self, v: Value, dt: &Type) -> Result<Value, String> {
        // v1.10 B6：值语义结构体进容器——box 成 RC 对象，返回 payload 指针的 i64 位模式。
        // 对齐 codegen.rs::to_elem_bits（codegen.rs:1300）的 dial_struct_box + ptr_to_int。
        // v 对 struct 是「内容区地址」Ptr vreg；dial_struct_box 把它拷进堆对象。
        // ret_ty=I64 直接拿整型返回寄存器的位模式（dial_struct_box 返回 void*，与 i64 同通道）。
        if let Type::Struct(_) = dt {
            let size = self.struct_byte_size(dt);
            let sz = self.new_reg(IrType::I64);
            self.emit(IrInst::Conv { kind: AConv::Zext, dst: sz,
                a: Value::Imm(Imm::I64(size)), ty: IrType::I64 });
            let boxed = self.new_reg(IrType::I64);
            self.emit(IrInst::Call {
                dst: Some(boxed), func: "dial_struct_box".to_string(),
                args: vec![v, Value::Reg(sz)], arg_tys: vec![IrType::Ptr, IrType::I64],
                ret_ty: IrType::I64, linkage: CallLink::Runtime });
            return Ok(Value::Reg(boxed));
        }
        match reg_ir(dt) {
            IrType::I64 => Ok(v),
            IrType::I32 => {
                let d = self.new_reg(IrType::I64);
                self.emit(IrInst::Conv { kind: AConv::Sext, dst: d, a: v, ty: IrType::I64 });
                Ok(Value::Reg(d))
            }
            IrType::F32 => {
                // 先把 8 字节槽清零，再写 f32 的低 32 位，最后按 I64 读出位模式。
                let s = self.emit_alloca(IrType::I64);
                self.emit(IrInst::Store { ptr: s, val: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                self.emit(IrInst::Store { ptr: s, val: v, ty: IrType::F32 });
                let d = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: d, ptr: s, ty: IrType::I64 });
                Ok(Value::Reg(d))
            }
            src @ (IrType::F64 | IrType::Handle | IrType::Ptr) => {
                let s = self.emit_alloca(src.clone());
                self.emit(IrInst::Store { ptr: s, val: v, ty: src });
                let d = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: d, ptr: s, ty: IrType::I64 });
                Ok(Value::Reg(d))
            }
            _ => Err(format!("[asm 后端] 数组元素类型 {} 暂不支持赋值", dt.display())),
        }
    }

    /// 把 C 返回的 int64 位模式按 Dial 类型重新解释（KvMap 的键/值、
    /// float[] 元素等）。整数通道的 8 字节类型直接复用；窄/浮点类型经
    /// 同一个 8 字节槽 store→load 完成位模式重解释。
    fn reinterpret_bits(&mut self, v: Value, target: IrType) -> Result<Value, String> {
        if matches!(target, IrType::I64 | IrType::Handle | IrType::Ptr | IrType::FnPtr) {
            return Ok(v);
        }
        // v1.10 B6：struct 元素解箱——bits 是 box payload 指针（数组元素恒非 null），
        // memcpy 到一块新的内容区并返回其地址。容器仍持有那份 box，读侧不 release
        // （对齐 codegen.rs::from_elem_bits 的借用语义）。
        if let IrType::Struct(ref sn) = target {
            let size = self.struct_byte_size(&Type::Struct(sn.clone()));
            let content = self.emit_alloca(IrType::Struct(sn.clone()));
            self.emit_memcpy(content, v, size);
            return Ok(Value::Reg(content));
        }
        let s = self.emit_alloca(IrType::I64);
        self.emit(IrInst::Store { ptr: s, val: v, ty: IrType::I64 });
        let d = self.new_reg(target.clone());
        self.emit(IrInst::Load { dst: d, ptr: s, ty: target });
        Ok(Value::Reg(d))
    }

    fn lower_minmax(&mut self, is_min: bool, args: &[Expr]) -> Result<(Value, bool), String> {
        let dt = self.expr_dt(&args[0]);
        let is_float = matches!(dt, Type::Float | Type::Float32);
        let (mut av, _) = self.lower_expr(&args[0], &dt)?;
        let (mut bv, _) = self.lower_expr(&args[1], &dt)?;
        let ty = if is_float { IrType::F64 } else { IrType::I64 };
        if dt == Type::Float32 {
            let a2 = self.new_reg(IrType::F64);
            self.emit(IrInst::Conv { kind: AConv::FpExt, dst: a2, a: av, ty: IrType::F64 });
            let b2 = self.new_reg(IrType::F64);
            self.emit(IrInst::Conv { kind: AConv::FpExt, dst: b2, a: bv, ty: IrType::F64 });
            av = Value::Reg(a2); bv = Value::Reg(b2);
        }
        let cmp = self.new_reg(IrType::I64);
        let op = if is_min { ACmp::Lt } else { ACmp::Gt };
        if is_float {
            self.emit(IrInst::FCmp { op, dst: cmp, a: av.clone(), b: bv.clone(), ty });
        } else {
            self.emit(IrInst::ICmp { op, dst: cmp, a: av.clone(), b: bv.clone(), ty });
        }
        // is_min: a < b ? a : b；is_max: a > b ? a : b。
        let r = self.select_values(cmp, av, bv, if is_float { IrType::F64 } else { IrType::I64 });
        Ok((Value::Reg(r), false))
    }

    fn lower_abs(&mut self, args: &[Expr]) -> Result<(Value, bool), String> {
        let dt = self.expr_dt(&args[0]);
        let (av, _) = self.lower_expr(&args[0], &dt)?;
        if matches!(dt, Type::Float | Type::Float32) {
            let mut a2 = av;
            if dt == Type::Float32 {
                let r = self.new_reg(IrType::F64);
                self.emit(IrInst::Conv { kind: AConv::FpExt, dst: r, a: av, ty: IrType::F64 });
                a2 = Value::Reg(r);
            }
            let d = self.new_reg(IrType::F64);
            self.emit(IrInst::Call { dst: Some(d), func: "fabs".to_string(), args: vec![a2], arg_tys: vec![IrType::F64], ret_ty: IrType::F64, linkage: CallLink::Runtime });
            return Ok((Value::Reg(d), false));
        }
        // 整数：a < 0 ? 0 - a : a。
        let cmp = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Lt, dst: cmp, a: av.clone(), b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
        let neg = self.new_reg(IrType::I64);
        self.emit(IrInst::Bin { op: ABin::Sub, dst: neg, a: Value::Imm(Imm::I64(0)), b: av.clone(), ty: IrType::I64 });
        let r = self.select_values(cmp, Value::Reg(neg), av, IrType::I64);
        Ok((Value::Reg(r), false))
    }

    /// Dial 的 print / eprint 是 **variadic**：中间实参紧接输出不换行，只有最后一个换行；
    /// print_raw / eprint_raw 则全部不换行。这与 LLVM 后端 `emit_print` / `emit_print_raw`
    /// 的分派一一对应（见 codegen.rs:5231 / 5279）。
    fn lower_print(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        let to_stderr = name == "eprint" || name == "eprint_raw";
        let all_raw = name == "print_raw" || name == "eprint_raw" || name == "write_char";
        let last = args.len() - 1;
        for (i, arg) in args.iter().enumerate() {
            let dt = self.expr_dt(arg);
            let (v, _) = self.lower_expr(arg, &dt)?;
            let newline = !all_raw && i == last;
            let (func, aty) = print_symbol(&dt, to_stderr, newline)
                .ok_or_else(|| format!("[asm 后端] 不能直接 print {}", dt.display()))?;
            // C 侧标量通道一律收 int64_t / double：窄类型必须显式扩展，不能靠 ABI 猜。
            let v2 = match dt {
                Type::Int32 => {
                    let r = self.new_reg(IrType::I64);
                    self.emit(IrInst::Conv { kind: AConv::Sext, dst: r, a: v, ty: IrType::I64 });
                    Value::Reg(r)
                }
                Type::Float32 => {
                    let r = self.new_reg(IrType::F64);
                    self.emit(IrInst::Conv { kind: AConv::FpExt, dst: r, a: v, ty: IrType::F64 });
                    Value::Reg(r)
                }
                _ => v,
            };
            self.emit(IrInst::Call {
                dst: None, func: func.to_string(), args: vec![v2], arg_tys: vec![aty],
                ret_ty: IrType::Void, linkage: CallLink::Runtime,
            });
        }
        Ok((Value::Imm(Imm::I64(0)), false))
    }

    fn lower_interp(&mut self, parts: &[InterpPart]) -> Result<(Value, bool), String> {
        // (值, 是否自带所有权)：owned 的中间段/累积值在 concat 之后必须归还本地引用，
        // 否则它们没有任何 Local 持有、作用域退出不释放——真插值 `"v=${i}!"` 每轮泄漏。
        let mut acc: Option<(Value, bool)> = None;
        for part in parts {
            let (seg, seg_owned): (Value, bool) = match part {
                InterpPart::Text(t) => {
                    let lit = self.ip.intern_string(t);
                    let p = self.new_reg(IrType::Ptr);
                    self.emit(IrInst::LeaLabel { dst: p, lit });
                    let h = self.new_reg(IrType::Handle);
                    self.emit(IrInst::Call { dst: Some(h), func: "dial_string_new".to_string(), args: vec![Value::Reg(p)], arg_tys: vec![IrType::Ptr], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                    (Value::Reg(h), true) // dial_string_new 新分配，owned
                }
                InterpPart::Expr(e) => {
                    let et = self.expr_dt(e);
                    let (ev, _) = self.lower_expr(e, &et)?;
                    match et {
                        // 已是字符串：借用引用由来源持有（不 release），其余自带所有权。
                        Type::String | Type::Bytes => (ev, !yields_borrowed_ref(e)),
                        _ => {
                            let h = self.new_reg(IrType::Handle);
                            let f = match et {
                                Type::Int | Type::Int32 => "dial_int_to_string",
                                Type::Float | Type::Float32 => "dial_float_to_string",
                                Type::Bool => "dial_bool_to_string",
                                Type::Char => "dial_char_to_string",
                                Type::Byte => "dial_byte_to_string",
                                _ => return self.err(&format!("asm 后端：插值不支持 {}", et.display())),
                            };
                            // *_to_string 的 C 原型统一是 int64_t / double，窄类型显式扩展。
                            let (farg, faty): (Value, IrType) = match et {
                                Type::Int32 => {
                                    let r = self.new_reg(IrType::I64);
                                    self.emit(IrInst::Conv { kind: AConv::Sext, dst: r, a: ev, ty: IrType::I64 });
                                    (Value::Reg(r), IrType::I64)
                                }
                                Type::Float32 => {
                                    let r = self.new_reg(IrType::F64);
                                    self.emit(IrInst::Conv { kind: AConv::FpExt, dst: r, a: ev, ty: IrType::F64 });
                                    (Value::Reg(r), IrType::F64)
                                }
                                Type::Float => (ev, IrType::F64),
                                _ => (ev, IrType::I64),
                            };
                            self.emit(IrInst::Call { dst: Some(h), func: f.to_string(), args: vec![farg], arg_tys: vec![faty], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                            (Value::Reg(h), true) // *_to_string 新分配，owned
                        }
                    }
                }
            };
            acc = Some(match acc {
                None => (seg, seg_owned),
                Some((prev, prev_owned)) => {
                    let r = self.new_reg(IrType::Handle);
                    self.emit(IrInst::Call { dst: Some(r), func: "dial_string_concat".to_string(), args: vec![prev.clone(), seg.clone()], arg_tys: vec![IrType::Handle, IrType::Handle], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                    // concat 只读不消费：归还 prev 与 seg 的本地引用（若 owned）。
                    if prev_owned { self.release_owned_string(&prev); }
                    if seg_owned { self.release_owned_string(&seg); }
                    (Value::Reg(r), true) // 新 concat 结果 owned
                }
            });
        }
        match acc {
            Some((v, _)) => Ok((v, false)),
            None => {
                let lit = self.ip.intern_string("");
                let p = self.new_reg(IrType::Ptr);
                self.emit(IrInst::LeaLabel { dst: p, lit });
                let h = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(h), func: "dial_string_new".to_string(), args: vec![Value::Reg(p)], arg_tys: vec![IrType::Ptr], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                Ok((Value::Reg(h), false))
            }
        }
    }

    // ---- match（字面量模式 → if 链）----

    fn emit_icmp_eq(&mut self, a: Value, b: Value) -> VReg {
        let r = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Eq, dst: r, a, b, ty: IrType::I64 });
        r
    }

    /// match 的字符串模式：模式串必须经 dial_string_new 包成 DialString（带 ARC
    /// 头）再交给 dial_string_eq，比较完立即释放临时对象，避免每个分支泄漏。
    fn lower_string_pattern_eq(&mut self, subj: Value, s: &str) -> Result<VReg, String> {
        let lit = self.ip.intern_string(s);
        let p = self.new_reg(IrType::Ptr);
        self.emit(IrInst::LeaLabel { dst: p, lit });
        let h = self.new_reg(IrType::Handle);
        self.emit(IrInst::Call {
            dst: Some(h), func: "dial_string_new".to_string(),
            args: vec![Value::Reg(p)], arg_tys: vec![IrType::Ptr],
            ret_ty: IrType::Handle, linkage: CallLink::Runtime,
        });
        let eq = self.new_reg(IrType::I64);
        self.emit(IrInst::Call {
            dst: Some(eq), func: "dial_string_eq".to_string(),
            args: vec![subj, Value::Reg(h)], arg_tys: vec![IrType::Handle, IrType::Handle],
            ret_ty: IrType::I64, linkage: CallLink::Runtime,
        });
        // RcRelease 的 ptr 是「存放句柄的槽地址」，发射器会先取句柄再调用。
        let slot = self.emit_alloca(IrType::Handle);
        self.emit(IrInst::Store { ptr: slot, val: Value::Reg(h), ty: IrType::Handle });
        self.emit(IrInst::RcRelease { ptr: slot, kind: ArcKind::String });
        Ok(self.emit_icmp_eq(Value::Reg(eq), Value::Imm(Imm::I64(1))))
    }

    fn lower_match(&mut self, subject: &Expr, arms: &[MatchArm]) -> Result<(), String> {
        let sd = self.expr_dt(subject);
        let (sv, _) = self.lower_expr(subject, &sd)?;
        let merge = self.new_block(Vec::new());
        // 顺序 if 链（self.cur 已是当前块，逐分支串联）。
        let mut has_wild = false;
        for arm in arms {
            let cmp: VReg = match &arm.pattern {
                MatchPattern::Int(v) => self.emit_icmp_eq(sv, Value::Imm(Imm::I64(*v))),
                MatchPattern::Bool(v) => self.emit_icmp_eq(sv, Value::Imm(Imm::I64(if *v { 1 } else { 0 }))),
                MatchPattern::Char(v) => self.emit_icmp_eq(sv, Value::Imm(Imm::I64(*v))),
                MatchPattern::Str(s) => self.lower_string_pattern_eq(sv, s)?,
                MatchPattern::Wildcard => { has_wild = true; continue; }
            };
            let body_b = self.new_block(Vec::new());
            let next_b = self.new_block(Vec::new());
            self.terminate(Term::Branch { cond: cmp, then_b: body_b, else_b: next_b, then_args: Vec::new(), else_args: Vec::new() });
            self.set_cur(body_b);
            self.push_scope();
            for st in &arm.body { self.lower_stmt(st)?; }
            self.leave_scope()?;
            if !self.cur_terminated() { self.terminate(Term::Jump { target: merge, args: Vec::new() }); }
            self.set_cur(next_b);
        }
        if has_wild {
            // 兜底：直接在 cursor 执行 _ 分支体。
            for arm in arms {
                if let MatchPattern::Wildcard = arm.pattern {
                    self.push_scope();
                    for st in &arm.body { self.lower_stmt(st)?; }
                    self.leave_scope()?;
                    if !self.cur_terminated() { self.terminate(Term::Jump { target: merge, args: Vec::new() }); }
                    break;
                }
            }
        }
        if !self.cur_terminated() { self.terminate(Term::Jump { target: merge, args: Vec::new() }); }
        self.set_cur(merge);
        Ok(())
    }

    /// M7-e3an2：多路通道选择 `select`（v0.91），对齐 LLVM 后端 codegen.rs 的
    /// Stmt::Select 实现。控制流骨架：
    ///   setup -> loop -> chk -> dispatch(条件链) -> take_i/case_i
    ///   nowait(default / 阻塞等待)、eof(全体关闭)、end(统一续跑点)。
    ///
    /// 关键约束：
    ///   · retry 回边只能回 loop，绝不回 setup（否则每轮重复 dial_sel_new 泄漏 selector）；
    ///   · 不 push self.loops —— case body 的 break 必须通过 self.loops.last() 跳到
    ///     select **外层**那个循环（select 是语句，不是可 break 的块，对齐 LLVM 语义）；
    ///   · dial_sel_free 在 case/default body 之前发（body 里 return 不泄漏），但 isnull
    ///     重试路径不能 free（被别的消费者抢走时 selector 还要留着重试）；
    ///   · epoch 先于 pick 读取，避免「读 epoch 与 pick 之间发生变化」导致睡死（不丢唤醒）。
    ///
    /// 跨块值全部经 alloca 栈槽传递（sel/epoch/idx/v），规避 ICmp 融合跨块重发的隐患；
    /// 绑定变量生命周期严格限制在本 case 块内，由 push_scope/leave_scope 管理。
    fn lower_select(
        &mut self,
        arms: &[SelectArm],
        default_body: &Option<Vec<Stmt>>,
        timeout: &Option<Expr>,
        timeout_body: &Option<Vec<Stmt>>,
    ) -> Result<(), String> {
        let n = arms.len();
        if n == 0 { return self.err("select 至少需要一个 case"); }

        // ---- 跨块状态槽（静态栈布局，循环内 alloca 不增长栈帧）----
        let sel_slot = self.emit_alloca(IrType::Handle);   // DialSelector*
        let epoch_slot = self.emit_alloca(IrType::I64);    // epoch（nowait 等待用）
        let idx_slot = self.emit_alloca(IrType::I64);      // dial_sel_pick 结果
        let v_slot = self.emit_alloca(IrType::Handle);     // dial_sel_recv 取到的元素句柄
        // v1.10：绝对 deadline（now_ms + 超时毫秒），跨块传到 nowait 等待。
        let deadline_slot = if timeout.is_some() {
            Some(self.emit_alloca(IrType::I64))
        } else {
            None
        };

        // ---- 块骨架 ----
        let setup_b = self.new_block(Vec::new());
        let loop_b = self.new_block(Vec::new());
        let chk_b = self.new_block(Vec::new());
        let nowait_b = self.new_block(Vec::new());
        let eof_b = self.new_block(Vec::new());
        let end_b = self.new_block(Vec::new());
        let timeout_end_b = if timeout.is_some() {
            Some(self.new_block(Vec::new()))
        } else {
            None
        };
        // dispatch 条件链 + take/case 块，每个 arm 一对。
        let mut dispatch_b: Vec<u32> = Vec::new();
        let mut take_b: Vec<u32> = Vec::new();
        let mut case_b: Vec<u32> = Vec::new();
        for _ in 0..n {
            dispatch_b.push(self.new_block(Vec::new()));
            take_b.push(self.new_block(Vec::new()));
            case_b.push(self.new_block(Vec::new()));
        }

        // 通用 C 调用发射助手（避免重复构造 IrInst::Call）。
        fn call_runtime(
            l: &mut Lower<'_>, func: &str, args: Vec<Value>, arg_tys: Vec<IrType>, ret_ty: IrType,
        ) -> Option<VReg> {
            let dst = if ret_ty == IrType::Void { None } else { Some(l.new_reg(ret_ty.clone())) };
            l.emit(IrInst::Call {
                dst, func: func.to_string(), args, arg_tys, ret_ty, linkage: CallLink::Runtime,
            });
            dst
        }

        // ---- v1.10：绝对 deadline = now_ms + 超时毫秒（在 select 入口当前块求值一次）----
        if let (Some(te), Some(dslot)) = (timeout, deadline_slot) {
            let (ms_val, _) = self.lower_expr(te, &Type::Int)?;
            let ms_reg = self.cond_reg(ms_val)?;
            let now = call_runtime(self, "dial_chan_now_ms", vec![], vec![], IrType::I64).unwrap();
            let dl = self.new_reg(IrType::I64);
            self.emit(IrInst::Bin {
                op: ABin::Add, dst: dl, a: Value::Reg(now), b: Value::Reg(ms_reg), ty: IrType::I64,
            });
            self.emit(IrInst::Store { ptr: dslot, val: Value::Reg(dl), ty: IrType::I64 });
        }

        // ---- setup：建 selector，登记各 case 的通道 ----
        if !self.cur_terminated() { self.terminate(Term::Jump { target: setup_b, args: Vec::new() }); }
        self.set_cur(setup_b);
        let sel = call_runtime(self, "dial_sel_new", vec![], vec![], IrType::Handle).unwrap();
        self.emit(IrInst::Store { ptr: sel_slot, val: Value::Reg(sel), ty: IrType::Handle });
        for arm in arms {
            // case 形状已由语义层校验为 recv(ch)；取通道表达式 ch（不求值 recv 本身）。
            let ch_expr = match &arm.chan {
                Expr::Call(fname, cargs) if fname == "recv" && cargs.len() == 1 => &cargs[0],
                _ => return self.err("内部错误：select 的 case 必须是 recv(ch) 形式"),
            };
            let ct = self.expr_dt(ch_expr);
            let ch = self.lower_expr(ch_expr, &ct)?.0;
            call_runtime(self, "dial_sel_add",
                vec![Value::Reg(sel), ch], vec![IrType::Handle, reg_ir(&ct)], IrType::Void);
        }
        self.terminate(Term::Jump { target: loop_b, args: Vec::new() });

        // ---- loop：先读 epoch，再挑就绪通道 ----
        self.set_cur(loop_b);
        let epoch = call_runtime(self, "dial_chan_epoch", vec![], vec![], IrType::I64).unwrap();
        self.emit(IrInst::Store { ptr: epoch_slot, val: Value::Reg(epoch), ty: IrType::I64 });
        let idx = call_runtime(self, "dial_sel_pick",
            vec![Value::Reg(sel)], vec![IrType::Handle], IrType::I64).unwrap();
        self.emit(IrInst::Store { ptr: idx_slot, val: Value::Reg(idx), ty: IrType::I64 });
        // is_eof = idx == -2（本块内计算+使用，不跨块）。
        let is_eof = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Eq, dst: is_eof,
            a: Value::Reg(idx), b: Value::Imm(Imm::I64(-2)), ty: IrType::I64 });
        self.terminate(Term::Branch { cond: is_eof, then_b: eof_b, else_b: chk_b,
            then_args: Vec::new(), else_args: Vec::new() });

        // ---- chk：is_ready = idx >= 0（从槽重新 load，避免跨块 ICmp 融合）----
        self.set_cur(chk_b);
        let idx_v = self.new_reg(IrType::I64);
        self.emit(IrInst::Load { dst: idx_v, ptr: idx_slot, ty: IrType::I64 });
        let is_ready = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Ge, dst: is_ready,
            a: Value::Reg(idx_v), b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
        self.terminate(Term::Branch { cond: is_ready, then_b: dispatch_b[0], else_b: nowait_b,
            then_args: Vec::new(), else_args: Vec::new() });

        // ---- dispatch 条件链：idx == i ? take_i : next ----
        for i in 0..n {
            self.set_cur(dispatch_b[i]);
            let iv = self.new_reg(IrType::I64);
            self.emit(IrInst::Load { dst: iv, ptr: idx_slot, ty: IrType::I64 });
            let eq = self.new_reg(IrType::I64);
            self.emit(IrInst::ICmp { op: ACmp::Eq, dst: eq,
                a: Value::Reg(iv), b: Value::Imm(Imm::I64(i as i64)), ty: IrType::I64 });
            let next = if i + 1 < n { dispatch_b[i + 1] } else { loop_b };
            self.terminate(Term::Branch { cond: eq, then_b: take_b[i], else_b: next,
                then_args: Vec::new(), else_args: Vec::new() });
        }

        // ---- take_i / case_i ----
        for (i, arm) in arms.iter().enumerate() {
            // take：非阻塞取元素；NULL = 被别的消费者抢走 -> 回 loop 重挑（不是 EOF）。
            self.set_cur(take_b[i]);
            let sv = self.new_reg(IrType::Handle);
            self.emit(IrInst::Load { dst: sv, ptr: sel_slot, ty: IrType::Handle });
            let v = call_runtime(self, "dial_sel_recv",
                vec![Value::Reg(sv), Value::Imm(Imm::I64(i as i64))],
                vec![IrType::Handle, IrType::I64], IrType::Handle).unwrap();
            self.emit(IrInst::Store { ptr: v_slot, val: Value::Reg(v), ty: IrType::Handle });
            let vnull = self.new_reg(IrType::I64);
            self.emit(IrInst::ICmp { op: ACmp::Eq, dst: vnull,
                a: Value::Reg(v), b: Value::Imm(Imm::I64(0)), ty: IrType::Handle });
            self.terminate(Term::Branch { cond: vnull, then_b: loop_b, else_b: case_b[i],
                then_args: Vec::new(), else_args: Vec::new() });

            // case：先释放 selector（body 里 return 也不泄漏），再绑定变量 + 执行分支体。
            self.set_cur(case_b[i]);
            let sv = self.new_reg(IrType::Handle);
            self.emit(IrInst::Load { dst: sv, ptr: sel_slot, ty: IrType::Handle });
            call_runtime(self, "dial_sel_free", vec![Value::Reg(sv)], vec![IrType::Handle], IrType::Void);

            // 绑定变量类型 = 通道元素类型（语义层已保证为句柄，非 void）。
            let ch_expr = match &arm.chan {
                Expr::Call(_, cargs) => &cargs[0],
                _ => unreachable!(),
            };
            let ct = self.expr_dt(ch_expr);
            let elem = match ct {
                Type::Channel(e) => *e,
                _ => return self.err("内部错误：select case 的通道类型异常"),
            };
            self.push_scope();
            let var_slot = self.emit_alloca(reg_ir(&elem));
            let ev = self.new_reg(IrType::Handle);
            self.emit(IrInst::Load { dst: ev, ptr: v_slot, ty: IrType::Handle });
            // 元素句柄所有权已由 dial_sel_recv 转移给接收方：is_copy=false，不再 retain。
            self.store_value(var_slot, Value::Reg(ev), false, &elem, false);
            let is_h = rc_kind(&elem).is_some() || matches!(elem, Type::Struct(_));
            self.declare(&arm.var, Local {
                ptr: var_slot, ty: reg_ir(&elem), dt: elem.clone(), is_handle: is_h, moved: false,
            });
            for st in &arm.body {
                self.lower_stmt(st)?;
                if self.cur_terminated() { break; }  // break/return 后不再 emit（对齐 LLVM）
            }
            self.leave_scope()?;
            if !self.cur_terminated() { self.terminate(Term::Jump { target: end_b, args: Vec::new() }); }
        }

        // ---- nowait：无就绪通道 ----
        self.set_cur(nowait_b);
        if let Some(db) = default_body {
            let sv = self.new_reg(IrType::Handle);
            self.emit(IrInst::Load { dst: sv, ptr: sel_slot, ty: IrType::Handle });
            call_runtime(self, "dial_sel_free", vec![Value::Reg(sv)], vec![IrType::Handle], IrType::Void);
            self.push_scope();
            for st in db {
                self.lower_stmt(st)?;
                if self.cur_terminated() { break; }
            }
            self.leave_scope()?;
            if !self.cur_terminated() { self.terminate(Term::Jump { target: end_b, args: Vec::new() }); }
        } else if let (Some(dslot), Some(tend)) = (deadline_slot, timeout_end_b) {
            // v1.10：无 default、有超时——带绝对 deadline 的阻塞等待。
            // dial_chan_wait_timeout 返回 1=超时、0=被唤醒。
            let ep = self.new_reg(IrType::I64);
            self.emit(IrInst::Load { dst: ep, ptr: epoch_slot, ty: IrType::I64 });
            let dl = self.new_reg(IrType::I64);
            self.emit(IrInst::Load { dst: dl, ptr: dslot, ty: IrType::I64 });
            let to = call_runtime(
                self, "dial_chan_wait_timeout",
                vec![Value::Reg(ep), Value::Reg(dl)],
                vec![IrType::I64, IrType::I64], IrType::I64,
            ).unwrap();
            let is_to = self.new_reg(IrType::I64);
            self.emit(IrInst::ICmp {
                op: ACmp::Eq, dst: is_to, a: Value::Reg(to), b: Value::Imm(Imm::I64(1)),
                ty: IrType::I64,
            });
            self.terminate(Term::Branch {
                cond: is_to, then_b: tend, else_b: loop_b,
                then_args: Vec::new(), else_args: Vec::new(),
            });
        } else {
            // 无 default、无超时：事件驱动阻塞等待（非轮询），epoch 来自进入 select 时记录的值。
            let ep = self.new_reg(IrType::I64);
            self.emit(IrInst::Load { dst: ep, ptr: epoch_slot, ty: IrType::I64 });
            call_runtime(self, "dial_chan_wait", vec![Value::Reg(ep)], vec![IrType::I64], IrType::Void);
            self.terminate(Term::Jump { target: loop_b, args: Vec::new() });
        }

        // ---- timeout.end：超时分支（v1.10）----
        if let Some(tend) = timeout_end_b {
            self.set_cur(tend);
            let sv = self.new_reg(IrType::Handle);
            self.emit(IrInst::Load { dst: sv, ptr: sel_slot, ty: IrType::Handle });
            call_runtime(self, "dial_sel_free", vec![Value::Reg(sv)], vec![IrType::Handle], IrType::Void);
            if let Some(tb) = timeout_body {
                self.push_scope();
                for st in tb {
                    self.lower_stmt(st)?;
                    if self.cur_terminated() { break; }
                }
                self.leave_scope()?;
            }
            if !self.cur_terminated() { self.terminate(Term::Jump { target: end_b, args: Vec::new() }); }
        }

        // ---- eof：全体通道已关闭且已空 -> 直接结束，不执行任何分支 ----
        self.set_cur(eof_b);
        let sv = self.new_reg(IrType::Handle);
        self.emit(IrInst::Load { dst: sv, ptr: sel_slot, ty: IrType::Handle });
        call_runtime(self, "dial_sel_free", vec![Value::Reg(sv)], vec![IrType::Handle], IrType::Void);
        self.terminate(Term::Jump { target: end_b, args: Vec::new() });

        // ---- end：统一续跑点 ----
        self.set_cur(end_b);
        Ok(())
    }

    // ---- 类型辅助 ----

    /// 把参数值按边界宽度适配（bool→i32, byte→i8 等）。
    fn adapt_param(&mut self, v: Value, from: &Type, _to: &Type) -> Value {
        let pit = param_ir(from);
        let rit = reg_ir(from);
        if pit == rit { return v; }
        let dst = self.new_reg(pit.clone());
        let kind = match (rit, pit.clone()) {
            (IrType::I64, IrType::I32) | (IrType::I64, IrType::I8) => AConv::Trunc,
            (IrType::I32, IrType::I64) | (_, IrType::I64) => AConv::Zext,
            (_, IrType::I8) => AConv::Trunc,
            _ => AConv::Zext,
        };
        self.emit(IrInst::Conv { kind, dst, a: v, ty: pit });
        Value::Reg(dst)
    }

    /// 把条件值规整为分支所需的 I64 0/1 寄存器。
    fn cond_reg(&mut self, v: Value) -> Result<VReg, String> {
        match v {
            Value::Reg(r) => Ok(r),
            Value::Imm(Imm::I64(_)) => {
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Conv { kind: AConv::Zext, dst: r, a: v, ty: IrType::I64 });
                Ok(r)
            }
            _ => Err("[asm 后端] 条件表达式非 bool".to_string()),
        }
    }

    // ---- 结构体 / 类型推断辅助 ----

    fn struct_field(&self, dt: &Type, field: &str) -> Option<(usize, Type)> {
        if let Type::Struct(n) = dt {
            if let Some((fields, _)) = self.ip.structs.get(n) {
                return fields.iter().position(|(f, _)| f == field).map(|i| (i, fields[i].1.clone()));
            }
        }
        None
    }

    fn struct_byte_size(&self, dt: &Type) -> i64 {
        if let Type::Struct(n) = dt {
            let (_off, size, _align) = self.ip.struct_layout(n, false);
            return size;
        }
        8
    }

    fn expr_dt(&self, e: &Expr) -> Type {
        match e {
            // char / byte 在**寄存器宽度**上与 int 同为 I64，但**类型层面**是独立的
            // （语义层 char_to_int / int_to_char 的显式转换依赖这个区分），
            // 故这里必须给出各自的 Dial 类型，不能一律当 int。
            Expr::IntLit(_) => Type::Int,
            Expr::CharLit(_) => Type::Char,
            Expr::ByteLit(_) => Type::Byte,
            Expr::FloatLit(_) => Type::Float,
            Expr::BoolLit(_) => Type::Bool,
            Expr::StringLit(_) | Expr::Interp(_) => Type::String,
            Expr::Var(name) => match self.lookup(name) {
                Some(l) => l.dt.clone(),
                // 具名函数在右值位置求值为**纯函数地址**（fn(A,B):R）。
                None => match self.ip.functions_sig.get(name) {
                    Some(info) => Type::FnPtr(info.params.clone(), Box::new(info.ret.clone())),
                    None => Type::Int,
                },
            },
            Expr::Binary(a, op, b) => {
                let da = self.expr_dt(a);
                let db = self.expr_dt(b);
                match op {
                    BinOp::And | BinOp::Or | BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => Type::Bool,
                    // 字符串拼接的结果仍是字符串（不能退化成 Int，否则 print 会把句柄当数字吐出）。
                    BinOp::Add if da == Type::String && db == Type::String => Type::String,
                    _ => {
                        // F32+F32 必须仍是 F32（此前一律退化成 F64，导致 lower 产出 f32
                        // 位模式而 print 按 f64 读 → float32 算术打印成 0.0）；F32 与 F64
                        // 混合仍按 F64（对齐 lower_binary 的 f_ty 判定）。
                        if matches!(da, Type::Float32) && matches!(db, Type::Float32) {
                            Type::Float32
                        } else if matches!(da, Type::Float | Type::Float32) || matches!(db, Type::Float | Type::Float32) {
                            Type::Float
                        } else { da }
                    }
                }
            }
            Expr::Unary(op, a) => match op {
                UnOp::Not => Type::Bool,
                UnOp::Deref => e.strip_ptr_of(),
                _ => self.expr_dt(a),
            },
            Expr::Ternary(_, t, _) => self.expr_dt(t),
            Expr::Call(name, args) => {
                // min / max / abs 的返回类型跟随实参（表里只能写一个占位）。
                if matches!(name.as_str(), "min" | "max" | "abs") && !args.is_empty() {
                    return self.expr_dt(&args[0]);
                }
                // M7-e3ah：bitcast<T>(x) 的返回类型就是类型实参 T，而非表里占位
                // 的 Int。否则 bitcast<B>(a).f 链式访问字段时 base 被当成 Int，
                // 字段解析错位（Expr::Field 按 expr_dt(base) 查 struct_field）。
                if name == "bitcast" {
                    if let Some(Expr::TypeLit(t)) = args.get(0) {
                        return t.clone();
                    }
                }
                self.ip.functions_sig.get(name)
                    .map(|s| s.ret.clone())
                    .or_else(|| builtin::builtin_ret(name))
                    .or_else(|| builtin::special_ret(name))
                    .unwrap_or(Type::Int)
            }
            // 下标 / 切片 / 字面量的元素类型必须按数组的**实际**类型推导：
            // 此前一律硬编码 int / int[]，于是 string[] 的元素被当成整数——
            // print 走 dial_print_int 通道，把句柄当数字吐出（指针数值）。
            Expr::Index(arr, _) => match self.expr_dt(arr) {
                Type::Array(e) => *e,
                // M7-e3az：向量下标返回元素类型（lane 提取），供 print 等按标量分派。
                Type::Simd(e, _) => *e,
                other => other,
            },
            Expr::Slice(arr, _, _) => self.expr_dt(arr),
            Expr::Field(base, field) => {
                // 裸指针指向结构体时，字段类型取指针的元素结构体的字段类型
                // （对齐 LLVM 后端 field_base_ptr 的裸指针分支）。
                let bd = match self.expr_dt(base) {
                    Type::RawPtr(e) | Type::VolatilePtr(e) => (*e).clone(),
                    other => other,
                };
                if let Some((_, ft)) = self.struct_field(&bd, field) { ft } else { Type::Int }
            }
            Expr::Method(obj, method, margs) => {
                let od = self.expr_dt(obj);
                // M7-e3aq：B1 impl 方法返回类型从脱糖符号的签名取（对齐 LLVM
                // type_of：语义层已把 Method 递归降级为 Call 校验并返回 info.ret）。
                // 必须放在内建方法推断之前，否则 impl 方法会 fallback 到 Type::Int，
                // 使返回 string / struct / 另一具名类型的方法在表达式位置类型错位。
                if let Some(recv) = impl_receiver_name(&od) {
                    let symbol = impl_method_symbol(recv, method);
                    if let Some(info) = self.ip.functions_sig.get(&symbol) {
                        return info.ret.clone();
                    }
                }
                // remove/pop 返回被删元素（不是新数组指针）。
                if matches!(method.as_str(), "remove" | "pop") {
                    if let Type::Array(e) = &od { return *e.clone(); }
                }
                // M7-e3an：高阶方法返回类型（与语义层 check_method_call 同口径）。
                // lookup_method 对这些方法返回 None（走专用路径），此处须显式推断，
                // 否则链式调用（如 a.map(f).get(0)）会把结果误当成 Int。
                if matches!(method.as_str(), "map" | "filter" | "reduce" | "sort_by") {
                    if let Type::Array(e) = &od {
                        return match method.as_str() {
                            "reduce" => (**e).clone(),
                            "map" => match margs.first().map(|a| self.expr_dt(a)) {
                                Some(Type::FnPtr(_, r)) => Type::Array(r),
                                _ => Type::Array(e.clone()),
                            },
                            _ => Type::Array(e.clone()), // filter / sort_by
                        };
                    }
                }
                builtin::method_ret(&od, method).unwrap_or(Type::Int)
            }
            Expr::StructLit(name, _) => Type::Struct(name.clone()),
            // 具名函数在右值位置求值为**纯函数地址**（fn(A,B):R）——
            // 不分配环境对象、不做捕获、不走 ARC，只有一个代码地址。
            Expr::FuncRef(fname) => match self.ip.functions_sig.get(fname) {
                Some(info) => Type::FnPtr(info.params.clone(), Box::new(info.ret.clone())),
                None => Type::Int,
            },
            Expr::ArrayLit(elems, expect) => match expect {
                Some(et) => Type::Array(Box::new((**et).clone())),
                None => {
                    let et = match elems.first() {
                        Some(e) => self.expr_dt(e),
                        None => Type::Int,
                    };
                    Type::Array(Box::new(et))
                }
            },
            _ => Type::Int,
        }
    }

    fn expr_struct_dt(&self, e: &Expr) -> Type {
        match e {
            Expr::Var(name) => self.lookup(name).map(|l| l.dt.clone()).unwrap_or(Type::Struct("?".to_string())),
            // 嵌套字段链（r.origin.x）：逐级解析——基类型的**当前字段**类型才是
            // 本级基类型。此前直接递归到 base 的类型（丢掉字段名），导致
            // `r.origin.x` 拿 Rect 去查字段 x 而误报「无字段 x」。
            Expr::Field(base, field) => {
                let base_dt = self.expr_struct_dt(base);
                match self.struct_field(&base_dt, field) {
                    Some((_off, fty)) => fty,
                    None => base_dt,
                }
            }
            _ => self.expr_dt(e),
        }
    }

    fn emit_memcpy(&mut self, dst: VReg, src: Value, size: i64) {
        let sz = self.new_reg(IrType::I64);
        self.emit(IrInst::Conv { kind: AConv::Zext, dst: sz, a: Value::Imm(Imm::I64(size)), ty: IrType::I64 });
        self.emit(IrInst::Call { dst: None, func: "memcpy".to_string(), args: vec![Value::Reg(dst), src, Value::Reg(sz)], arg_tys: vec![IrType::Ptr, IrType::Ptr, IrType::I64], ret_ty: IrType::Ptr, linkage: CallLink::Extern });
    }

    /// v1.10 B6：把一块内容区清零，构造零值结构体（对齐 LLVM sty.const_zero()）。
    /// 用于 map get 未命中（bits==0）时的零值还原。memset 是 libc 符号，
    /// Call 有副作用不被 DCE 删除，O0-O3 都发射。
    fn emit_memset(&mut self, dst: VReg, size: i64) {
        let sz = self.new_reg(IrType::I64);
        self.emit(IrInst::Conv { kind: AConv::Zext, dst: sz, a: Value::Imm(Imm::I64(size)), ty: IrType::I64 });
        let z = self.new_reg(IrType::I64);
        self.emit(IrInst::Conv { kind: AConv::Zext, dst: z, a: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
        self.emit(IrInst::Call { dst: None, func: "memset".to_string(), args: vec![Value::Reg(dst), Value::Reg(z), Value::Reg(sz)], arg_tys: vec![IrType::Ptr, IrType::I64, IrType::I64], ret_ty: IrType::Ptr, linkage: CallLink::Extern });
    }

    /// v1.10 B6：归还 struct box 的本地那份引用（对齐 LLVM sink_elem_after_retain）。
    /// dial_struct_box 造出 rc=1 的 box，容器 push/put 内部又 retain 一份（rc=2），
    /// 容器释放时只还它自己那份（rc=1）——本地这份若不还就泄漏。bits 是 box payload
    /// 指针的 i64 位模式，转成指针后 dial_rc_release。
    fn release_box(&mut self, bits: Value) {
        let p = self.bits_as_ptr(bits);
        self.emit(IrInst::Call { dst: None, func: "dial_rc_release".to_string(),
            args: vec![p], arg_tys: vec![IrType::Handle],
            ret_ty: IrType::Void, linkage: CallLink::Runtime });
    }

    /// 释放一个「自带所有权、不绑定到局部变量」的临时 string 句柄。
    ///
    /// `dial_string_concat(a,b)` 只读操作数、不消费其引用（C 侧不 retain/release），
    /// 因此当操作数是表达式临时（如 `str(i)`、嵌套拼接结果、`dial_string_new`）时，
    /// 调用方必须在 concat 之后归还这份本地引用——否则该临时没有任何 Local 持有，
    /// 作用域退出不会释放它，每轮循环泄漏一份（`"k"+str(i)` 每轮 ~31B）。
    ///
    /// 判定：借用引用（变量 / 字段 / 下标 / get / at）由来源继续持有，**不得**释放，
    /// 否则悬垂或双重释放；静态串字面量是 immortal（release no-op），跳过以少发指令。
    fn release_string_temp(&mut self, val: &Value, expr: &Expr) {
        if yields_borrowed_ref(expr) || matches!(expr, Expr::StringLit(_)) { return; }
        self.release_owned_string(val);
    }

    /// 释放一个已知「自带所有权」的临时 string 句柄（插入插值中间段使用）。
    /// RcRelease 作用于槽地址，故先落临时槽。静态 immortal 对象由 C 侧短路 no-op。
    fn release_owned_string(&mut self, val: &Value) {
        if let Some(k) = rc_kind(&Type::String) {
            let tmp = self.emit_alloca(IrType::Handle);
            self.emit(IrInst::Store { ptr: tmp, val: val.clone(), ty: IrType::Handle });
            self.emit(IrInst::RcRelease { ptr: tmp, kind: k });
        }
    }

    /// i64 位模式的指针 → Ptr vreg（对齐 LLVM int_to_ptr）。
    fn bits_as_ptr(&mut self, bits: Value) -> Value {
        let s = self.emit_alloca(IrType::I64);
        self.emit(IrInst::Store { ptr: s, val: bits, ty: IrType::I64 });
        let p = self.new_reg(IrType::Ptr);
        self.emit(IrInst::Load { dst: p, ptr: s, ty: IrType::Ptr });
        Value::Reg(p)
    }

    /// v1.10 B6：map get 未命中的 struct 还原——bits==0 → 零值结构体，否则解箱。
    /// 无分支方案：先无条件造一块零值内容区，再用 select 在「零值区地址」与
    /// 「bits（box 指针）」之间选源地址，最后单次 memcpy。select 选的是**地址**
    /// 而非解引用，故 bits==0 时不会像 LLVM select 两侧求值那样解引用空指针。
    /// 用项目现成的 select_values（经过大量验证的块参数 phi），避免手写 phi 落地出错。
    /// 返回的 Value 是内容区地址（Ptr）；self.cur 被切到合流块，后续指令自然落进去。
    fn unbox_struct_nullable(&mut self, bits: Value, dt: &Type) -> Result<Value, String> {
        let sn = match dt { Type::Struct(n) => n.clone(), _ => return self.err("unbox_struct_nullable 仅接受 struct") };
        let size = self.struct_byte_size(dt);
        // 零值模板：无条件清零一块内容区（当前块）。
        let zero = self.emit_alloca(IrType::Struct(sn.clone()));
        self.emit_memset(zero, size);
        // bits == 0 ? 选源地址：null → zero 区地址；非 null → bits（box 指针）。
        let is_null = self.new_reg(IrType::I64);
        self.emit(IrInst::ICmp { op: ACmp::Eq, dst: is_null, a: bits.clone(), b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
        let src = self.select_values(is_null, Value::Reg(zero), bits, IrType::Ptr);
        // 单次解箱 memcpy 到结果内容区（容器仍持有 box，读侧不 release）。
        let content = self.emit_alloca(IrType::Struct(sn.clone()));
        self.emit_memcpy(content, Value::Reg(src), size);
        Ok(Value::Reg(content))
    }
}

// ============================================================================
// 自由函数辅助
// ============================================================================

fn acmp(op: BinOp) -> ACmp {
    match op {
        BinOp::Eq => ACmp::Eq, BinOp::Neq => ACmp::Neq, BinOp::Lt => ACmp::Lt,
        BinOp::Gt => ACmp::Gt, BinOp::Le => ACmp::Le, BinOp::Ge => ACmp::Ge,
        _ => ACmp::Eq,
    }
}

/// BinOp → ABin（算术 / 位运算部分；逻辑 And/Or 在 lower_binary 里短路处理）。
fn abin(op: BinOp) -> ABin {
    match op {
        BinOp::Add => ABin::Add, BinOp::Sub => ABin::Sub, BinOp::Mul => ABin::Mul,
        BinOp::Div => ABin::Div, BinOp::Mod => ABin::Mod,
        BinOp::BitAnd => ABin::BitAnd, BinOp::BitOr => ABin::BitOr, BinOp::BitXor => ABin::BitXor,
        BinOp::Shl => ABin::Shl, BinOp::Shr => ABin::Shr,
        _ => ABin::Add,
    }
}

fn struct_name_of(dt: &Type) -> String {
    if let Type::Struct(n) = dt { n.clone() } else { "?".to_string() }
}

/// 表达式是否产出「借来的」引用（来源仍持有所有权）。
///
/// 与 LLVM 后端 codegen.rs 的 yields_borrowed_ref 同口径：变量 / 字段 / 数组元素
/// 以及容器 get / at 的返回值都是借来的——写进容器时容器会 retain 一份，
/// 但来源那份不能因此被释放（否则悬垂 / 双重释放）。
fn yields_borrowed_ref(e: &Expr) -> bool {
    match e {
        Expr::Var(_) | Expr::Field(..) | Expr::Index(..) => true,
        Expr::Method(_, m, _) => matches!(m.as_str(), "get" | "at"),
        _ => false,
    }
}

fn slot_bytes(t: IrType) -> u32 {
    match t {
        IrType::I32 | IrType::F32 => 4,
        IrType::I1 | IrType::I8 => 1,
        _ => 8,
    }
}

/// M7-e3az：simd 元素的原生机器宽度（字节），对齐 LLVM `simd_elem_llvm`：
/// int=i64(8)、float=f64(8)、bool=i1(1)、byte=i8(1)、char=i32(4)。
fn simd_elem_size(t: &Type) -> u32 {
    match t {
        Type::Int => 8,
        Type::Float => 8,
        Type::Bool => 1,
        Type::Byte => 1,
        Type::Char => 4,
        _ => 8, // parser 已保证 simd 元素只能是上述标量
    }
}

/// M7-e3az：simd lane 的**内部存储宽度**——直接用 Dial 标量的寄存器宽度
/// （int/byte/char/bool=I64、float=F64），与 `reg_ir` 一致。每个 lane 占 8 字节槽，
/// 存的是 Dial 内部位模式（byte 为 0..255 零扩展值）——因此无符号 byte 比较
/// 直接用有符号 ICmp 即得正确结果（值非负），lane 提取也可直接当标量返回。
fn simd_lane_ir(t: &Type) -> IrType {
    reg_ir(t)
}

/// print 系列分派表：与 LLVM 后端 emit_print / emit_print_raw / emit_eprint* 一一对应。
/// `newline=true` → 带换行的通道；`false` → *_raw 通道（char 例外，走 write_char）。
fn print_symbol(dt: &Type, to_stderr: bool, newline: bool) -> Option<(&'static str, IrType)> {
    let s = match (dt, to_stderr, newline) {
        (Type::Int | Type::Byte, false, true) => "dial_print_int",
        (Type::Int | Type::Byte, false, false) => "dial_print_raw_int",
        (Type::Int | Type::Byte, true, true) => "dial_eprint_int",
        (Type::Int | Type::Byte, true, false) => "dial_eprint_raw_int",
        (Type::Int32, false, true) => "dial_print_int",
        (Type::Int32, false, false) => "dial_print_raw_int",
        (Type::Int32, true, true) => "dial_eprint_int",
        (Type::Int32, true, false) => "dial_eprint_raw_int",
        (Type::Float | Type::Float32, false, true) => "dial_print_float",
        (Type::Float | Type::Float32, false, false) => "dial_print_raw_float",
        (Type::Float | Type::Float32, true, true) => "dial_eprint_float",
        (Type::Float | Type::Float32, true, false) => "dial_eprint_raw_float",
        (Type::Bool, false, true) => "dial_print_bool",
        (Type::Bool, false, false) => "dial_print_raw_bool",
        (Type::Bool, true, true) => "dial_eprint_bool",
        (Type::Bool, true, false) => "dial_eprint_raw_bool",
        (Type::Char, false, true) => "dial_print_char",
        (Type::Char, false, false) => "dial_write_char",
        (Type::Char, true, true) => "dial_eprint_char",
        (Type::Char, true, false) => "dial_eprint_write_char",
        (Type::String, false, true) => "dial_print_string",
        (Type::String, false, false) => "dial_print_raw",
        (Type::String, true, true) => "dial_eprint_string",
        (Type::String, true, false) => "dial_eprint_raw",
        _ => return None,
    };
    let aty = match dt {
        Type::Float | Type::Float32 => IrType::F64,
        Type::String => IrType::Handle,
        _ => IrType::I64,
    };
    Some((s, aty))
}

/// Value 转寄存器（仅在 Value::Reg 时；用于 FieldAddr 的 base 与 Deref 的 ptr）。
trait AsReg { fn as_reg(self) -> VReg; }
impl AsReg for Value {
    fn as_reg(self) -> VReg { if let Value::Reg(r) = self { r } else { unreachable!() } }
}

/// 裸指针类型剥去外层，得到指向的元素类型。
trait StripPtrOf { fn strip_ptr_of(&self) -> Type; }
impl StripPtrOf for Expr {
    fn strip_ptr_of(&self) -> Type {
        // 仅用于 Unary::Deref 的 hint，不需要精确；返回 Int 占位。
        Type::Int
    }
}

// 让 Type 也有 strip_ptr（用于 Unary::Deref 内层类型）。
trait StripPtr { fn strip_ptr(&self) -> Type; }
impl StripPtr for Type {
    fn strip_ptr(&self) -> Type {
        match self {
            Type::RawPtr(e) | Type::VolatilePtr(e) => (**e).clone(),
            other => other.clone(),
        }
    }
}
