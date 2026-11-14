//! 自研后端专属 IR（中间表示）。
//!
//! 设计：3 地址码 + 基本块 + **块参数（= SSA 形参）**。每条指令最多 2 个操作数、1 个定义；
//! 块参数天然 SSA，避免 phi 插入 pass；DCE / 常量折叠 / 值编号都直接在之上做。
//!
//! 这是「自研后端」的第一层：AST（`src/ast.rs`）先经 `src/lower.rs` 降到这里，
//! 再过 `src/pass.rs` 的优化 Pass，最后由 `src/codegen_asm.rs` 发射成 x86-64 / AArch64 汇编。
//!
//! 与现有 LLVM 后端（`src/codegen.rs`）完全独立、互不干扰——LLVM 路径零改动。

use std::collections::HashMap;
use crate::ast::{self, Type};
use crate::semantic::FuncInfo;

// ============================================================================
// 类型
// ============================================================================

/// IR 标量/句柄类型。Dial 的三套宽度（寄存器内部 / 结构体字段 / 函数参数 C ABI）
/// 在 `reg_ir` / `field_ir` / `param_ir` 三个映射里单点实现，见文件末尾。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum IrType {
    I64, I32, F64, F32, I1, I8,
    /// 裸指针 / volatile 指针 / 任意 void*（i8*）。
    Ptr,
    /// 不透明堆句柄：string/bytes/array/map/kvmap/set/error/channel（i8*）。
    Handle,
    /// 值语义结构体，按名引用（查 StructTable 拿字段布局）。
    Struct(String),
    /// 函数地址（i8*）。
    FnPtr,
    Void,
}

impl IrType {
    /// 该类型在内存/栈槽里占几个字节（用于栈帧布局与 load/store 宽度）。
    pub fn size(&self) -> u32 {
        match self {
            IrType::I64 | IrType::F64 | IrType::Ptr | IrType::Handle | IrType::FnPtr => 8,
            IrType::I32 | IrType::F32 => 4,
            // I1 / I8 在普通 Dial struct 字段占 1 字节；作为独立栈槽我们仍按 8 字节对齐分配
            // （避免对齐陷阱），但 load/store 用 1 字节宽度。
            IrType::I1 | IrType::I8 => 1,
            IrType::Struct(_) => 8, // 实际大小由 StructTable 计算，这里只是占位
            IrType::Void => 0,
        }
    }

    pub fn is_float(&self) -> bool { matches!(self, IrType::F64 | IrType::F32) }
}

// ============================================================================
// 值 / 虚拟寄存器
// ============================================================================

pub type VReg = u32;

/// 编译期内联常量。浮点存位模式（与 CPU 一致），避免 Rust f64 打印/舍入干扰。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Imm {
    I64(i64),
    I32(i32),
    F64(u64), // f64::to_bits
    F32(u32), // f32::to_bits
    I1(bool),
    I8(i8),
}

impl Imm {
    pub fn as_i64(self) -> i64 {
        match self {
            Imm::I64(v) => v, Imm::I32(v) => v as i64, Imm::I1(v) => v as i64,
            Imm::I8(v) => v as i64, Imm::F64(_) | Imm::F32(_) => unreachable!("非整数立即数"),
        }
    }
    pub fn ty(self) -> IrType {
        match self {
            Imm::I64(_) => IrType::I64, Imm::I32(_) => IrType::I32,
            Imm::F64(_) => IrType::F64, Imm::F32(_) => IrType::F32,
            Imm::I1(_) => IrType::I1, Imm::I8(_) => IrType::I8,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value {
    Reg(VReg),
    Imm(Imm),
}

impl Value {
    pub fn ty(self, reg_ty: &[IrType]) -> IrType {
        match self {
            Value::Reg(v) => reg_ty[v as usize].clone(),
            Value::Imm(i) => i.ty(),
        }
    }
    pub fn is_imm(self) -> bool { matches!(self, Value::Imm(_)) }
}

// ============================================================================
// 指令
// ============================================================================

/// 整数二元运算。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ABin {
    Add, Sub, Mul, Div, Mod,
    Eq, Neq, Lt, Gt, Le, Ge,
    And, Or,
    BitAnd, BitOr, BitXor, Shl, Shr,
}

/// 一元运算。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AUn { Neg, Not, BitNot }

/// 比较（产生 I1）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ACmp { Eq, Neq, Lt, Gt, Le, Ge }

/// 标量转换。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AConv {
    Sext,   // 窄→宽（i8/i32/i1 → i64）
    Zext,   // 无符号窄→宽
    Trunc,  // 宽→窄（i64 → i32/i8）
    FpSi,   // float → signed int
    SiFp,   // signed int → float
    FpExt,  // f32 → f64
    FpTrunc,// f64 → f32
}

/// ARC 原语的种类，决定发射时落到哪个 `dial_*_retain/release`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcKind { Generic, String, Array, Error, Bytes }

/// 调用链接种类：Dial 内部函数 / 语言层 extern（FFI）/ 运行时 C 符号（`dial_*`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallLink { Dial, Extern, Runtime }

#[derive(Clone, Debug)]
pub enum IrInst {
    /// 在栈上分配 `slots` 个 8 字节槽，返回其首地址（指针 vreg）。
    Alloca { dst: VReg, bytes: u32 },
    /// 从指针加载一个 `ty` 宽度的值。
    Load { dst: VReg, ptr: VReg, ty: IrType },
    /// 向指针写一个值。
    Store { ptr: VReg, val: Value, ty: IrType },
    /// 取结构体字段地址：base 是指向结构体的指针。
    /// `field` 是**字段序号**（不是字节偏移）；发射器按 `struct_layout(struct_name).offsets[field]`
    /// 换算成实际字节偏移，从而自动带上 C ABI 的 padding。
    FieldAddr { dst: VReg, base: VReg, struct_name: String, field: u32 },
    /// 取 rodata 字符串字面量的地址（emit 时落到 `.LstrN`）。
    LeaLabel { dst: VReg, lit: u32 },
    /// 指针加字节偏移：`dst = base + off`（off 为 64 位字节数）。
    /// 用于借出堆对象的内部数据区（如 string / bytes 的 payload 跳过 8 字节头）。
    PtrAdd { dst: VReg, base: VReg, off: Value },

    Bin { op: ABin, dst: VReg, a: Value, b: Value, ty: IrType },
    Un { op: AUn, dst: VReg, a: Value, ty: IrType },
    FBin { op: ABin, dst: VReg, a: Value, b: Value, ty: IrType },
    ICmp { op: ACmp, dst: VReg, a: Value, b: Value, ty: IrType },
    FCmp { op: ACmp, dst: VReg, a: Value, b: Value, ty: IrType },
    Conv { kind: AConv, dst: VReg, a: Value, ty: IrType },

    Call {
        dst: Option<VReg>,
        func: String,
        args: Vec<Value>,
        arg_tys: Vec<IrType>,
        ret_ty: IrType,
        linkage: CallLink,
    },

    /// 取具名函数的代码地址（函数指针的一等值）。
    /// 发射为 `leaq <符号>(%rip), %reg`——纯代码地址，无闭包、无环境。
    FuncAddr { dst: VReg, func: String },
    /// 间接调用：target 是**函数指针值**（IrType::FnPtr）。
    /// 实参与返回值的宽度按 arg_tys / ret_ty 走，与直接调用同一套 ABI 规则。
    CallIndirect {
        dst: Option<VReg>,
        target: VReg,
        args: Vec<Value>,
        arg_tys: Vec<IrType>,
        ret_ty: IrType,
    },

    /// ARC 原语（发射时落为具体 C call）。
    RcRetain { ptr: VReg, kind: ArcKind },
    RcRelease { ptr: VReg, kind: ArcKind },
    StructRetain { ptr: VReg, struct_name: String },
    StructRelease { ptr: VReg, struct_name: String },
    /// 调用 `dial_env_init(argc, argv)`（main 入口首句）。
    EnvInit { argc: Value, argv: Value },
}

impl IrInst {
    /// 是否「纯」（无副作用、DCE 可安全删除）。
    /// ARC 原语、Store、Call、EnvInit 一律视为有副作用——DCE 绝不删，避免破坏 ARC 配对 / 内存写 / 调用顺序。
    pub fn is_pure(&self) -> bool {
        match self {
            IrInst::Load { .. }
            | IrInst::LeaLabel { .. }
            | IrInst::Bin { .. }
            | IrInst::Un { .. }
            | IrInst::FBin { .. }
            | IrInst::ICmp { .. }
            | IrInst::FCmp { .. }
            | IrInst::Conv { .. }
            | IrInst::Alloca { .. }
            | IrInst::PtrAdd { .. }
            | IrInst::FuncAddr { .. }
            | IrInst::FieldAddr { .. } => true,
            IrInst::Store { .. }
            | IrInst::Call { .. }
            | IrInst::CallIndirect { .. }
            | IrInst::RcRetain { .. }
            | IrInst::RcRelease { .. }
            | IrInst::StructRetain { .. }
            | IrInst::StructRelease { .. }
            | IrInst::EnvInit { .. } => false,
        }
    }

    /// 本指令定义的目标 vreg（若有）。
    pub fn def(&self) -> Option<VReg> {
        match self {
            IrInst::Alloca { dst, .. }
            | IrInst::Load { dst, .. }
            | IrInst::FieldAddr { dst, .. }
            | IrInst::LeaLabel { dst, .. }
            | IrInst::FuncAddr { dst, .. }
            | IrInst::PtrAdd { dst, .. }
            | IrInst::Bin { dst, .. }
            | IrInst::Un { dst, .. }
            | IrInst::FBin { dst, .. }
            | IrInst::ICmp { dst, .. }
            | IrInst::FCmp { dst, .. }
            | IrInst::Conv { dst, .. } => Some(*dst),
            IrInst::Call { dst, .. } => *dst,
            IrInst::CallIndirect { dst, .. } => *dst,
            IrInst::Store { .. }
            | IrInst::RcRetain { .. }
            | IrInst::RcRelease { .. }
            | IrInst::StructRetain { .. }
            | IrInst::StructRelease { .. }
            | IrInst::EnvInit { .. } => None,
        }
    }

    /// 本指令引用的所有寄存器操作数（用于 DCE use-count）。
    pub fn reg_uses(&self, out: &mut Vec<VReg>) {
        match self {
            IrInst::Load { ptr, .. } | IrInst::FieldAddr { base: ptr, .. }
            | IrInst::RcRetain { ptr, .. } | IrInst::RcRelease { ptr, .. }
            | IrInst::StructRetain { ptr, .. } | IrInst::StructRelease { ptr, .. } => out.push(*ptr),
            IrInst::Store { ptr, val, .. } => { out.push(*ptr); push_val(*val, out); }
            IrInst::Bin { a, b, .. } | IrInst::FBin { a, b, .. } | IrInst::ICmp { a, b, .. }
            | IrInst::FCmp { a, b, .. } => { push_val(*a, out); push_val(*b, out); }
            IrInst::Un { a, .. } | IrInst::Conv { a, .. } => push_val(*a, out),
            IrInst::PtrAdd { base, off, .. } => { out.push(*base); push_val(*off, out); }
            IrInst::LeaLabel { .. } | IrInst::Alloca { .. } | IrInst::FuncAddr { .. } => {}
            IrInst::Call { args, .. } => for a in args { push_val(*a, out); }
            IrInst::CallIndirect { target, args, .. } => {
                out.push(*target);
                for a in args { push_val(*a, out); }
            }
            IrInst::EnvInit { argc, argv } => { push_val(*argc, out); push_val(*argv, out); }
        }
    }

    /// 本指令是否令 `v` 逃逸（被写入内存 / 作为调用实参 / 被 Struct* 嵌套引用）。
    /// ARC 简化 Pass 用：逃逸则保留 retain/release 配对。
    pub fn escapes(&self, v: VReg) -> bool {
        match self {
            IrInst::Store { ptr, val, .. } => *ptr == v || *val == Value::Reg(v),
            IrInst::RcRetain { ptr, .. } | IrInst::RcRelease { ptr, .. }
            | IrInst::StructRetain { ptr, .. } | IrInst::StructRelease { ptr, .. } => *ptr == v,
            IrInst::Call { args, .. } => args.contains(&Value::Reg(v)),
            IrInst::CallIndirect { target, args, .. } => *target == v || args.contains(&Value::Reg(v)),
            IrInst::Bin { a, b, .. } | IrInst::FBin { a, b, .. } | IrInst::ICmp { a, b, .. }
            | IrInst::FCmp { a, b, .. } => *a == Value::Reg(v) || *b == Value::Reg(v),
            IrInst::Un { a, .. } | IrInst::Conv { a, .. } => *a == Value::Reg(v),
            IrInst::Load { ptr, .. } | IrInst::FieldAddr { base: ptr, .. } => *ptr == v,
            IrInst::PtrAdd { base, off, .. } => *base == v || *off == Value::Reg(v),
            IrInst::LeaLabel { .. } | IrInst::Alloca { .. } | IrInst::FuncAddr { .. } => false,
            IrInst::EnvInit { argc, argv } => *argc == Value::Reg(v) || *argv == Value::Reg(v),
        }
    }
}

fn push_val(v: Value, out: &mut Vec<VReg>) {
    if let Value::Reg(r) = v { out.push(r); }
}

// ============================================================================
// 基本块 / 函数 / 程序
// ============================================================================

#[derive(Clone, Debug)]
pub enum Term {
    Jump { target: u32, args: Vec<Value> },
    Branch { cond: VReg, then_b: u32, else_b: u32, then_args: Vec<Value>, else_args: Vec<Value> },
    Return { val: Option<Value> },
    Unreachable,
}

#[derive(Clone, Debug)]
pub struct IrBlock {
    pub id: u32,
    pub params: Vec<(VReg, IrType)>,
    pub insts: Vec<IrInst>,
    pub term: Term,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FuncLink { External, Private }

#[derive(Clone, Debug)]
pub struct IrFunction {
    pub name: String,
    pub params: Vec<(String, IrType)>,
    pub ret: IrType,
    pub linkage: FuncLink,
    pub blocks: Vec<IrBlock>,
    pub entry: u32,
    /// 与 `params` 平行：每个形参对应的栈槽指针 vreg（entry 块里的 Alloca 定义）。
    /// prologue 把 C ABI 传参寄存器写入这些槽，函数体再从槽里 Load。
    pub param_slots: Vec<VReg>,
    /// vreg → 类型（单一赋值，便于分配器 / 类型推导 / 宽度选择）。
    pub reg_ty: Vec<IrType>,
}

impl IrFunction {
    pub fn new(name: String, params: Vec<(String, IrType)>, ret: IrType, linkage: FuncLink) -> Self {
        IrFunction {
            name, params, ret, linkage,
            blocks: Vec::new(), entry: 0, param_slots: Vec::new(), reg_ty: Vec::new(),
        }
    }
    /// 申请一个新 vreg，记录其类型。
    pub fn new_reg(&mut self, ty: IrType) -> VReg {
        let v = self.reg_ty.len() as VReg;
        self.reg_ty.push(ty);
        v
    }
    pub fn new_block(&mut self, id: u32) -> &mut IrBlock {
        self.blocks.push(IrBlock { id, params: Vec::new(), insts: Vec::new(), term: Term::Unreachable });
        self.blocks.last_mut().unwrap()
    }
}

#[derive(Clone, Debug)]
pub struct IrProgram {
    pub functions: Vec<IrFunction>,
    /// 字符串字面量索引 → UTF-8 原文（emit 时加 `\0` 进 .rodata）。
    pub string_literals: Vec<String>,
    /// 函数名 → 签名（来自 semantic 解析结果）。
    pub functions_sig: HashMap<String, FuncInfo>,
    /// 结构体名 → (字段(名,类型), 默认值)，复用 semantic 的 StructTable。
    pub structs: HashMap<String, (Vec<(String, Type)>, HashMap<String, ast::Expr>)>,
    /// label 名 → 作用域深度（语义层 analyze_gotos 统一计算，自研后端生成
    /// load/释放层时按深度跨作用域释放）。
    pub goto_depths: HashMap<String, usize>,
}

impl IrProgram {
    pub fn new(
        functions_sig: HashMap<String, FuncInfo>,
        structs: HashMap<String, (Vec<(String, Type)>, HashMap<String, ast::Expr>)>,
        goto_depths: HashMap<String, usize>,
    ) -> Self {
        IrProgram { functions: Vec::new(), string_literals: Vec::new(), functions_sig, structs, goto_depths }
    }
    /// 登记一个字符串字面量，返回其索引。
    pub fn intern_string(&mut self, s: &str) -> u32 {
        if let Some(i) = self.string_literals.iter().position(|x| x == s) {
            return i as u32;
        }
        self.string_literals.push(s.to_string());
        (self.string_literals.len() - 1) as u32
    }

    /// 计算一个 Dial 结构体（按名）按 C ABI 的字段布局：返回 (offsets, total_size, align)。
    /// 非 packed：字段按自然对齐（i64/f64/ptr/handle/struct = 8，i32/f32 = 4，i1/i8 = 1）；
    /// packed：逐字节排列无 padding。与现有 LLVM 后端 `build_struct_types` 对齐。
    ///
    /// **嵌套结构体必须递归求尺寸/对齐**：C ABI 下 `struct Rect { Point origin; int w; }`
    /// 的 origin 占 Point 的完整尺寸（16 字节）而非一个指针宽（8 字节），
    /// 否则字段偏移与 LLVM 后端（按 DataLayout 算真实布局）不一致，双后端会错位。
    pub fn struct_layout(&self, name: &str, packed: bool) -> (Vec<i64>, i64, i64) {
        self.struct_layout_depth(name, packed, 0)
    }

    /// struct_layout 的递归实现。depth 用于环检测（自引用结构体在 Dial 里不合法，
    /// 但坏输入下必须终止而不是无限递归）。
    fn struct_layout_depth(&self, name: &str, packed: bool, depth: u32) -> (Vec<i64>, i64, i64) {
        // 字段默认值表在此无关；直接用第一个元素（字段列表）。
        let fields = match self.structs.get(name) {
            Some((f, _)) => f.clone(),
            None => return (Vec::new(), 0, 1),
        };
        let mut offsets = Vec::with_capacity(fields.len());
        let mut off = 0i64;
        let mut align = 1i64;
        for (_, t) in &fields {
            let (a, sz) = self.field_align_size(t, packed, depth);
            align = align.max(a);
            if !packed { off = (off + a - 1) / a * a; }
            offsets.push(off);
            off += sz as i64;
        }
        if !packed { off = (off + align - 1) / align * align; }
        (offsets, off, align)
    }

    /// 字段的 (对齐, 字节数)。标量按宽度；嵌套结构体按其自身的 C ABI 布局递归求。
    fn field_align_size(&self, t: &Type, packed: bool, depth: u32) -> (i64, u32) {
        if let Type::Struct(n) = t {
            if depth < 8 {
                let (_off, size, align) = self.struct_layout_depth(n, packed, depth + 1);
                // C ABI：嵌套结构体的对齐取其自身对齐，尺寸取其尾部 padding 后的大小。
                return (align.max(1), size.max(1) as u32);
            }
            // 深度超限（异常输入）：按单指针宽兜底，绝不无限递归。
            return (8, 8);
        }
        let it = field_ir(t);
        let a = if it == IrType::I32 || it == IrType::F32 { 4 }
            else if it == IrType::I1 || it == IrType::I8 { 1 }
            else { 8 };
        (a, field_size(t))
    }
}

// ============================================================================
// 三套宽度映射（单点真理，全后端共用）
// ============================================================================

/// 寄存器内部宽度（reg_width）：Dial 类型在寄存器 / 栈槽里怎么存。
pub fn reg_ir(t: &Type) -> IrType {
    match t {
        Type::Int |         Type::Byte | Type::Char => IrType::I64,
        Type::Float => IrType::F64,
        Type::Int32 => IrType::I32,
        Type::Float32 => IrType::F32,
        // 内部寄存器宽度一律用 I64（0/1）搬运布尔，规避 I1 全程搬运的脆弱性；
        // 只有在 param_ir（C ABI 参数）与 field_ir（结构体字段）两处才按边界收窄。
        Type::Bool => IrType::I64,
        Type::String | Type::Bytes | Type::Array(_) | Type::Map | Type::KvMap(..)
        | Type::Set(_) | Type::Error | Type::Channel(_) | Type::File | Type::Thread
        | Type::Opaque(_) => IrType::Handle,
        Type::Struct(n) => IrType::Struct(n.clone()),
        Type::RawPtr(_) | Type::VolatilePtr(_) => IrType::Ptr,
        Type::FnPtr(..) => IrType::FnPtr,
        Type::Simd(..) => IrType::I64, // 首版不实现 simd
        Type::Void => IrType::Void,
    }
}

/// 函数参数 C ABI 宽度（param_abi）：bool 抬成 i32、byte 压成 i8，其余同 reg。
pub fn param_ir(t: &Type) -> IrType {
    match t {
        Type::Bool => IrType::I32,
        Type::Byte => IrType::I8,
        // 结构体按**值**传递在 C ABI 下要么进寄存器组、要么走内存，两套后端的
        // 分级规则不同；自研后端统一用「内容区地址指针」传递（callee 收到后
        // memcpy 出自己的副本），语义等价于按值且与 LLVM 后端的值拷贝行为一致。
        Type::Struct(_) => IrType::Ptr,
        other => reg_ir(other),
    }
}

/// Dial 结构体字段宽度（field_layout）：bool 占 1 字节、byte 占 8 字节，其余同 reg。
pub fn field_ir(t: &Type) -> IrType {
    match t {
        Type::Bool => IrType::I1,
        other => reg_ir(other),
    }
}

/// Dial 结构体字段占用的字节数（用于布局偏移推进）。
pub fn field_size(t: &Type) -> u32 {
    match field_ir(t) {
        IrType::I32 | IrType::F32 => 4,
        IrType::I1 | IrType::I8 => 1,
        _ => 8,
    }
}

/// 给定 IrType（标量/句柄/指针）在栈槽里分配的对齐后字节数（full-spill 用）。
pub fn slot_size(t: IrType) -> u32 {
    match t {
        IrType::I32 | IrType::F32 => 4,
        IrType::I1 | IrType::I8 => 1,
        _ => 8,
    }
}
