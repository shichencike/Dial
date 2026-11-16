//! 自研后端的优化 Pass（核心子集，保守优先）。
//!
//! 设计原则：**绝不动 ARC 原语的正确性**，只做纯计算的等价变换。
//! - 常量折叠（constant folding）：纯算术 / 比较的立即数运算折叠为常量。
//! - 代数化简（algebraic simplification）：x-x→0、x^x→0 等安全恒等式。
//! - DCE：删除结果无用的纯指令（ARC/Store/Call 一律保留）。
//! - 保守 CSE：跨函数内对相同纯计算做值编号复用（仅限无内存副作用的 Bin/Un/Cmp/Conv）。
//! - 保守 ARC 简化：仅消除「retain 后无任何使用即 release」的净零配对（绝不破坏引用计数平衡）。
//!
//! 运行顺序：fold → cse → dce → arc_simplify。

use crate::ir::*;
use std::collections::{HashMap, HashSet};

/// 对整段程序跑全部 Pass。
pub fn run_passes(ip: &mut IrProgram) {
    // functions 在 lower 阶段已 push；这里就地变换。
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for name in names {
        // 取可变的 IrFunction 引用：从 Vec 中按名取得。
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            fold_constants(f);
            cse(f);
            dce(f);
            simplify_arc(f);
        }
    }
}

// ============================================================================
// 立即数读取辅助
// ============================================================================

fn val_i64(v: &Value) -> Option<i64> {
    if let Value::Imm(i) = v { Some(i.as_i64()) } else { None }
}

fn val_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Imm(Imm::F64(bits)) => Some(f64::from_bits(*bits)),
        Value::Imm(Imm::F32(bits)) => Some(f32::from_bits(*bits) as f64),
        _ => None,
    }
}

// ============================================================================
// 常量折叠 + 代数化简
// ============================================================================

fn fold_constants(f: &mut IrFunction) {
    for b in &mut f.blocks {
        for inst in &mut b.insts {
            fold_inst(inst);
        }
    }
}

fn fold_inst(inst: &mut IrInst) {
    match inst {
        IrInst::Bin { op, dst, a, b, ty } => {
            // 代数化简：x - x → 0、x ^ x → 0（两个相同寄存器）。
            if let (Value::Reg(ra), Value::Reg(rb)) = (&*a, &*b) {
                if ra == rb && (*op == ABin::Sub || *op == ABin::BitXor) {
                    *inst = IrInst::Conv { kind: AConv::Zext, dst: *dst, a: Value::Imm(Imm::I64(0)), ty: ty.clone() };
                    return;
                }
            }
            if let (Some(x), Some(y)) = (val_i64(a), val_i64(b)) {
                if *ty != IrType::I64 { return; }
                if let Some(r) = fold_bin_i64(*op, x, y) {
                    *inst = IrInst::Conv { kind: AConv::Zext, dst: *dst, a: Value::Imm(Imm::I64(r)), ty: IrType::I64 };
                }
            } else if let (Some(x), Some(y)) = (val_f64(a), val_f64(b)) {
                if *ty != IrType::F64 { return; }
                if let Some(r) = fold_bin_f64(*op, x, y) {
                    *inst = IrInst::Conv { kind: AConv::Zext, dst: *dst, a: Value::Imm(Imm::F64(f64::to_bits(r))), ty: IrType::F64 };
                }
            }
        }
        IrInst::ICmp { op, dst, a, b, ty: _ } => {
            if let (Some(x), Some(y)) = (val_i64(a), val_i64(b)) {
                let r = cmp_i64(*op, x, y) as i64;
                *inst = IrInst::Conv { kind: AConv::Zext, dst: *dst, a: Value::Imm(Imm::I64(r)), ty: IrType::I64 };
            }
        }
        IrInst::FCmp { op, dst, a, b, ty: _ } => {
            if let (Some(x), Some(y)) = (val_f64(a), val_f64(b)) {
                let r = cmp_f64(*op, x, y) as i64;
                *inst = IrInst::Conv { kind: AConv::Zext, dst: *dst, a: Value::Imm(Imm::I64(r)), ty: IrType::I64 };
            }
        }
        _ => {}
    }
}

fn fold_bin_i64(op: ABin, x: i64, y: i64) -> Option<i64> {
    use ABin::*;
    Some(match op {
        Add => x.wrapping_add(y),
        Sub => x.wrapping_sub(y),
        Mul => x.wrapping_mul(y),
        Div => if y == 0 { return None; } else { x / y },
        Mod => if y == 0 { return None; } else { x % y },
        Eq => (x == y) as i64,
        Neq => (x != y) as i64,
        Lt => (x < y) as i64,
        Gt => (x > y) as i64,
        Le => (x <= y) as i64,
        Ge => (x >= y) as i64,
        And => ((x != 0) && (y != 0)) as i64,
        Or => ((x != 0) || (y != 0)) as i64,
        BitAnd => x & y,
        BitOr => x | y,
        BitXor => x ^ y,
        Shl => x.wrapping_shl(y as u32),
        Shr => x.wrapping_shr(y as u32),
    })
}

fn fold_bin_f64(op: ABin, x: f64, y: f64) -> Option<f64> {
    use ABin::*;
    Some(match op {
        Add => x + y,
        Sub => x - y,
        Mul => x * y,
        Div => x / y,
        Mod => x % y,
        _ => return None, // 浮点无逻辑/位运算
    })
}

fn cmp_i64(op: ACmp, x: i64, y: i64) -> i64 {
    use ACmp::*;
    match op {
        Eq => (x == y) as i64, Neq => (x != y) as i64,
        Lt => (x < y) as i64, Gt => (x > y) as i64,
        Le => (x <= y) as i64, Ge => (x >= y) as i64,
    }
}

fn cmp_f64(op: ACmp, x: f64, y: f64) -> i64 {
    use ACmp::*;
    match op {
        Eq => (x == y) as i64, Neq => (x != y) as i64,
        Lt => (x < y) as i64, Gt => (x > y) as i64,
        Le => (x <= y) as i64, Ge => (x >= y) as i64,
    }
}

// ============================================================================
// 死代码消除（DCE）
// ============================================================================

fn dce(f: &mut IrFunction) {
    loop {
        let mut used: HashSet<VReg> = HashSet::new();
        for b in &f.blocks {
            let mut tmp: Vec<VReg> = Vec::new();
            for inst in &b.insts {
                inst.reg_uses(&mut tmp);
                used.extend(tmp.drain(..));
            }
            match &b.term {
                Term::Jump { args, .. } => push_vals(args, &mut used),
                Term::Branch { cond, then_args, else_args, .. } => {
                    used.insert(*cond);
                    push_vals(then_args, &mut used);
                    push_vals(else_args, &mut used);
                }
                Term::Return { val } => if let Some(v) = val { push_val(*v, &mut used); },
                Term::Unreachable => {}
            }
        }
        let mut changed = false;
        for b in &mut f.blocks {
            b.insts.retain(|inst| {
                if inst.is_pure() {
                    if let Some(d) = inst.def() {
                        if used.contains(&d) { true } else { changed = true; false }
                    } else { true }
                } else { true }
            });
        }
        if !changed { break; }
    }
}

fn push_val(v: Value, out: &mut HashSet<VReg>) {
    if let Value::Reg(r) = v { out.insert(r); }
}

fn push_vals(vs: &[Value], out: &mut HashSet<VReg>) {
    for v in vs { push_val(*v, out); }
}

// ============================================================================
// 保守 CSE（值编号，仅无内存副作用的纯指令）
// ============================================================================

/// 给可 CSE 的纯指令构造一个规范化 key；返回 (key, 定义的 vreg)。
fn cse_key(inst: &IrInst) -> Option<(Vec<u64>, VReg)> {
    let mut k: Vec<u64> = Vec::new();
    match inst {
        IrInst::Bin { op, a, b, ty, dst } => {
            k.push(1); k.push(*op as u64);
            push_key_val(&mut k, *a); push_key_val(&mut k, *b);
            k.push(ty_tag(ty));
            Some((k, *dst))
        }
        IrInst::FBin { op, a, b, ty, dst } => {
            k.push(2); k.push(*op as u64);
            push_key_val(&mut k, *a); push_key_val(&mut k, *b);
            k.push(ty_tag(ty));
            Some((k, *dst))
        }
        IrInst::ICmp { op, a, b, ty, dst } => {
            k.push(3); k.push(*op as u64);
            push_key_val(&mut k, *a); push_key_val(&mut k, *b);
            k.push(ty_tag(ty));
            Some((k, *dst))
        }
        IrInst::FCmp { op, a, b, ty, dst } => {
            k.push(4); k.push(*op as u64);
            push_key_val(&mut k, *a); push_key_val(&mut k, *b);
            k.push(ty_tag(ty));
            Some((k, *dst))
        }
        IrInst::Un { op, a, ty, dst } => {
            k.push(5); k.push(*op as u64);
            push_key_val(&mut k, *a);
            k.push(ty_tag(ty));
            Some((k, *dst))
        }
        IrInst::Conv { kind, a, ty, dst } => {
            k.push(6); k.push(*kind as u64);
            push_key_val(&mut k, *a);
            k.push(ty_tag(ty));
            Some((k, *dst))
        }
        _ => None,
    }
}

fn push_key_val(k: &mut Vec<u64>, v: Value) {
    match v {
        Value::Reg(r) => { k.push(0); k.push(r as u64); }
        Value::Imm(i) => { k.push(1); k.push(imm_bits(i)); }
    }
}

fn imm_bits(i: Imm) -> u64 {
    match i {
        Imm::I64(v) => v as u64,
        Imm::I32(v) => v as u64,
        Imm::F64(v) => v,
        Imm::F32(v) => v as u64,
        Imm::I1(v) => v as u64,
        Imm::I8(v) => v as u64,
    }
}

fn ty_tag(t: &IrType) -> u64 {
    match t {
        IrType::I64 => 0, IrType::I32 => 1, IrType::F64 => 2, IrType::F32 => 3,
        IrType::I1 => 4, IrType::I8 => 5, IrType::Ptr => 6, IrType::Handle => 7,
        IrType::Void => 8, IrType::FnPtr => 9,
        IrType::Struct(n) => { let mut h = 10u64; for c in n.bytes() { h = h.wrapping_mul(31).wrapping_add(c as u64); } h },
    }
}

fn cse(f: &mut IrFunction) {
    let mut map: HashMap<Vec<u64>, VReg> = HashMap::new();
    // 先扫描登记所有块的指令顺序；逐块处理。
    for bi in 0..f.blocks.len() {
        let mut i = 0;
        while i < f.blocks[bi].insts.len() {
            let inst = f.blocks[bi].insts[i].clone();
            if let Some((key, def)) = cse_key(&inst) {
                if let Some(&prev) = map.get(&key) {
                    if prev != def {
                        // 用 prev 替换 def 的所有使用，并删除本指令。
                        subst_reg(f, def, prev);
                        f.blocks[bi].insts.remove(i);
                        continue; // 同一位置继续
                    }
                } else {
                    map.insert(key, def);
                }
            }
            i += 1;
        }
    }
}

// ============================================================================
// 寄存器替换（用于 CSE 删除重复定义后改写所有使用点）
// ============================================================================

fn subst_reg(f: &mut IrFunction, from: VReg, to: VReg) {
    for b in &mut f.blocks {
        for inst in &mut b.insts {
            subst_inst(inst, from, to);
        }
        subst_term(&mut b.term, from, to);
        for (pr, _) in &mut b.params {
            if *pr == from { *pr = to; }
        }
    }
}

fn subst_inst(inst: &mut IrInst, from: VReg, to: VReg) {
    match inst {
        IrInst::Alloca { .. } | IrInst::LeaLabel { .. } | IrInst::FuncAddr { .. } => {}
        IrInst::Load { ptr, .. } => { if *ptr == from { *ptr = to; } }
        IrInst::Store { ptr, val, .. } => { if *ptr == from { *ptr = to; } if let Value::Reg(r) = val { if *r == from { *val = Value::Reg(to); } } }
        IrInst::FieldAddr { base, .. } => { if *base == from { *base = to; } }
        IrInst::Bin { a, b, .. } | IrInst::FBin { a, b, .. } | IrInst::ICmp { a, b, .. } | IrInst::FCmp { a, b, .. } => {
            if let Value::Reg(r) = a { if *r == from { *a = Value::Reg(to); } }
            if let Value::Reg(r) = b { if *r == from { *b = Value::Reg(to); } }
        }
        IrInst::Un { a, .. } | IrInst::Conv { a, .. } => {
            if let Value::Reg(r) = a { if *r == from { *a = Value::Reg(to); } }
        }
        IrInst::Call { args, .. } => {
            for v in args.iter_mut() {
                if let Value::Reg(r) = v { if *r == from { *v = Value::Reg(to); } }
            }
        }
        IrInst::CallIndirect { target, args, .. } => {
            if *target == from { *target = to; }
            for v in args.iter_mut() {
                if let Value::Reg(r) = v { if *r == from { *v = Value::Reg(to); } }
            }
        }
        IrInst::RcRetain { ptr, .. } | IrInst::RcRelease { ptr, .. }
        | IrInst::StructRetain { ptr, .. } | IrInst::StructRelease { ptr, .. } => {
            if *ptr == from { *ptr = to; }
        }
        IrInst::PtrAdd { base, off, .. } => {
            if *base == from { *base = to; }
            if let Value::Reg(r) = off { if *r == from { *off = Value::Reg(to); } }
        }
        IrInst::EnvInit { argc, argv } => {
            if let Value::Reg(r) = argc { if *r == from { *argc = Value::Reg(to); } }
            if let Value::Reg(r) = argv { if *r == from { *argv = Value::Reg(to); } }
        }
    }
}

fn subst_term(t: &mut Term, from: VReg, to: VReg) {
    match t {
        Term::Jump { args, .. } => for v in args.iter_mut() {
            if let Value::Reg(r) = v { if *r == from { *v = Value::Reg(to); } }
        },
        Term::Branch { cond, then_args, else_args, .. } => {
            if *cond == from { *cond = to; }
            for v in then_args.iter_mut() { if let Value::Reg(r) = v { if *r == from { *v = Value::Reg(to); } } }
            for v in else_args.iter_mut() { if let Value::Reg(r) = v { if *r == from { *v = Value::Reg(to); } } }
        }
        Term::Return { val } => if let Some(v) = val {
            if let Value::Reg(r) = v { if *r == from { *val = Some(Value::Reg(to)); } }
        },
        Term::Unreachable => {}
    }
}

// ============================================================================
// 保守 ARC 简化
// ============================================================================

/// 仅删除「retain{p,k} 之后、到对应 release{p,k} 之间 p 再未被任何指令使用」的净零配对。
/// 这是唯一安全可消除的 ARC 形态：retain(+1) 紧接着 release(-1) 且无其他引用/逃逸，
/// 净变化为 0，删除两者不改变引用计数平衡，也不会破坏 ARC 配对。
fn simplify_arc(f: &mut IrFunction) {
    for b in &mut f.blocks {
        let mut i = 0;
        while i < b.insts.len() {
            // 找 retain
            let (rp, rk) = match &b.insts[i] {
                IrInst::RcRetain { ptr, kind } => (*ptr, ArcId::Rc(*kind)),
                IrInst::StructRetain { ptr, struct_name } => (*ptr, ArcId::Struct(struct_name.clone())),
                _ => { i += 1; continue; }
            };
            // 向后找最近的同 ptr/kind 的 release，且中间没有使用 rp 的指令。
            let mut j = i + 1;
            let mut safe = false;
            while j < b.insts.len() {
                if uses_reg(&b.insts[j], rp) {
                    break; // 中间有人用了 rp，不能删。
                }
                match &b.insts[j] {
                    IrInst::RcRelease { ptr, kind } if *ptr == rp && ArcId::Rc(*kind) == rk => { safe = true; break; }
                    IrInst::StructRelease { ptr, struct_name } if *ptr == rp && ArcId::Struct(struct_name.clone()) == rk => { safe = true; break; }
                    _ => {}
                }
                j += 1;
            }
            if safe {
                b.insts.remove(j); // 先删 release
                b.insts.remove(i); // 再删 retain
                continue; // 同一位置继续
            }
            i += 1;
        }
    }
}

/// ARC 原语的种类标识（与 `ArcKind` / 结构体名等价，用于配对匹配）。
#[derive(PartialEq, Clone)]
enum ArcId {
    Rc(ArcKind),
    Struct(String),
}

/// 指令是否引用了给定寄存器（含块参数语义外的普通使用）。
fn uses_reg(inst: &IrInst, r: VReg) -> bool {
    let mut tmp: Vec<VReg> = Vec::new();
    inst.reg_uses(&mut tmp);
    tmp.contains(&r)
}

// ============================================================================
// M7-e3p：过程间常量折叠（纯整数函数调用的编译期求值）
// ============================================================================
//
// 动机：bench 的 fibR/rsumR/treeR 是纯整数递归函数，被循环以**常量实参**
// 反复调用（rsumR(25) × 50000 等）。LLVM 因 Dial 的 Call 未标记纯性而无法
// 外提 / 折叠，这里在 IR Pass 层对满足严格白名单的函数调用做编译期求值，
// 把 `call` 直接替换为常量定义，彻底消除运行时的递归开销。
//
// 安全性（任一不满足即放弃、保持原样）：
//   1. 被调函数必须是 `Private` linkage、`func` 在同一程序内可查；
//   2. 被调函数的所有参数与返回值都是 I64（第一版仅 I64，最窄最安全）；
//   3. 函数体指令只在白名单内：Alloca / Load / Store(本地栈槽) /
//      Bin(整数) / Un / ICmp / Conv(整数) / Call(纯 Dial 函数) / 控制流；
//      任何 FBin/FCmp/浮点 Conv/FieldAddr/LeaLabel/PtrAdd/FuncAddr/
//      CallIndirect/ARC 原语/EnvInit/Extern/Runtime 调用 → 整函数判非纯；
//   4. 调用实参全部是整数立即数（Reg 实参一律不折叠）；
//   5. 求值有步数与递归深度上限，除零即放弃，超时即放弃；
//   6. 仅 x86-64 启用（由 asm.rs 的 target 守卫控制）；AArch64 零变化。
//
// 这是「语义保持」变换：纯整数函数在相同实参下结果确定，编译期求值与
// 运行时逐位一致（wrapping 语义与 codegen 的 add/sub/imul/cqo/idiv 对齐）。

/// 递归步数上限（防病态函数拖慢编译 / 指数递归）。treeR(18) 约 1 万次调用。
const E3P_MAX_STEPS: u64 = 2_000_000;
/// 递归深度上限（防栈溢出；Dial 递归函数典型深度 < 1000）。
const E3P_MAX_DEPTH: u32 = 4096;

/// 求值环境：vreg → i64。Alloca 分配的栈槽也用 vreg 号索引（值语义）。
struct E3pEnv {
    regs: HashMap<VReg, i64>,
    steps: u64,
    depth: u32,
}

impl E3pEnv {
    fn new() -> Self {
        E3pEnv { regs: HashMap::new(), steps: 0, depth: 0 }
    }
    fn tick(&mut self) -> bool {
        self.steps += 1;
        self.steps <= E3P_MAX_STEPS && self.depth <= E3P_MAX_DEPTH
    }
}

/// 读取一个 Value 的整数值（仅立即数 / 已求值寄存器）。
fn e3p_val(v: &Value, env: &E3pEnv) -> Option<i64> {
    match v {
        Value::Imm(im) => Some(im.as_i64()),
        Value::Reg(r) => env.regs.get(r).copied(),
    }
}

/// 求值一个整数二元运算，语义与 pass.rs 的 fold_bin_i64 完全一致。
fn e3p_bin(op: ABin, x: i64, y: i64) -> Option<i64> {
    fold_bin_i64(op, x, y)
}

/// 判定函数是否为「可编译期求值的纯整数函数」（白名单，保守）。
/// `visiting` 用于打断自递归 / 互递归的判定环。
fn e3p_is_pure(ip: &IrProgram, name: &str, visiting: &mut HashSet<String>) -> bool {
    if visiting.contains(name) { return true; } // 递归环：暂视为纯，最终由全体指令白名单兜底
    let f = match ip.functions.iter().find(|f| f.name == name) {
        Some(f) => f,
        None => return false,
    };
    if f.linkage != FuncLink::Private { return false; }
    if f.ret != IrType::I64 { return false; }
    if f.params.iter().any(|(_, t)| *t != IrType::I64) { return false; }
    // 第一版要求零 phi：块参数会让「跨块携带值」的求值复杂化，直接放弃更安全。
    if f.blocks.iter().any(|b| !b.params.is_empty()) { return false; }
    // 调用实参里也不能有块参数传递（Jump/Branch 的 args）。
    for b in &f.blocks {
        match &b.term {
            Term::Jump { args, .. } if !args.is_empty() => return false,
            Term::Branch { then_args, else_args, .. }
                if !then_args.is_empty() || !else_args.is_empty() => return false,
            _ => {}
        }
        for inst in &b.insts {
            match inst {
                IrInst::Alloca { .. } | IrInst::Load { .. } => {}
                IrInst::Store { ty, .. } => if *ty != IrType::I64 { return false; },
                IrInst::Bin { op, ty, .. } => {
                    // 只接受确定性的整数运算（与 fold_bin_i64 对齐）；Div/Mod 除零在求值期放弃。
                    let ok = matches!(op,
                        ABin::Add | ABin::Sub | ABin::Mul | ABin::Div | ABin::Mod
                        | ABin::Eq | ABin::Neq | ABin::Lt | ABin::Gt | ABin::Le | ABin::Ge
                        | ABin::And | ABin::Or | ABin::BitAnd | ABin::BitOr | ABin::BitXor
                        | ABin::Shl | ABin::Shr);
                    if !ok || *ty != IrType::I64 { return false; }
                }
                IrInst::Un { ty, .. } => if *ty != IrType::I64 { return false; },
                IrInst::ICmp { ty, .. } => if *ty != IrType::I64 { return false; },
                IrInst::Conv { kind, ty, .. } => {
                    // 仅整数扩展/截断；浮点转换一律拒绝。
                    if !matches!(kind, AConv::Sext | AConv::Zext | AConv::Trunc) { return false; }
                    if ty.is_float() { return false; }
                }
                IrInst::Call { func, linkage, arg_tys, ret_ty, .. } => {
                    if *linkage != CallLink::Dial || *ret_ty != IrType::I64 { return false; }
                    if arg_tys.iter().any(|t| *t != IrType::I64) { return false; }
                    // 被调函数也必须纯（递归判定）。
                    visiting.insert(name.to_string());
                    let pure = e3p_is_pure(ip, func, visiting);
                    visiting.remove(name);
                    if !pure { return false; }
                }
                // 任何副作用 / 地址 / 浮点 / ARC / 间接调用一律拒绝。
                _ => return false,
            }
        }
    }
    true
}

/// 解释执行一个纯整数函数 `name(args)`，返回常量结果；失败 / 超时返回 None。
fn e3p_eval_call(ip: &IrProgram, name: &str, args: &[i64], env: &mut E3pEnv) -> Option<i64> {
    if !env.tick() { return None; }
    env.depth += 1;
    if env.depth > E3P_MAX_DEPTH { env.depth -= 1; return None; }
    let f = ip.functions.iter().find(|f| f.name == name)?;
    if args.len() != f.params.len() { env.depth -= 1; return None; }

    // 参数槽：prologue 语义 —— 形参 i 写入 param_slots[i]。
    for (slot, val) in f.param_slots.iter().zip(args.iter()) {
        env.regs.insert(*slot, *val);
    }
    // Alloca 是栈槽分配：这里用 vreg 号直接当槽，初始化为 0（若尚未定义）。
    for b in &f.blocks {
        for inst in &b.insts {
            if let IrInst::Alloca { dst, .. } = inst {
                env.regs.entry(*dst).or_insert(0);
            }
        }
    }

    let mut cur = f.entry;
    let result = loop {
        if !env.tick() { env.depth -= 1; return None; }
        let b = f.blocks.iter().find(|b| b.id == cur)?;
        for inst in &b.insts {
            if !env.tick() { env.depth -= 1; return None; }
            match inst {
                IrInst::Alloca { dst, .. } => { env.regs.entry(*dst).or_insert(0); }
                IrInst::Load { dst, ptr, .. } => {
                    let v = *env.regs.get(ptr)?;
                    env.regs.insert(*dst, v);
                }
                IrInst::Store { ptr, val, .. } => {
                    let v = e3p_val(val, env)?;
                    env.regs.insert(*ptr, v);
                }
                IrInst::Bin { op, dst, a, b: rb, .. } => {
                    let x = e3p_val(a, env)?;
                    let y = e3p_val(rb, env)?;
                    let r = e3p_bin(*op, x, y)?; // 除零 → None → 放弃折叠
                    env.regs.insert(*dst, r);
                }
                IrInst::Un { op, dst, a, .. } => {
                    let x = e3p_val(a, env)?;
                    let r = match op {
                        AUn::Neg => x.wrapping_neg(),
                        AUn::Not => (x == 0) as i64, // 逻辑非
                        AUn::BitNot => !x,
                    };
                    env.regs.insert(*dst, r);
                }
                IrInst::ICmp { op, dst, a, b: rb, .. } => {
                    let x = e3p_val(a, env)?;
                    let y = e3p_val(rb, env)?;
                    env.regs.insert(*dst, cmp_i64(*op, x, y));
                }
                IrInst::Conv { kind, dst, a, .. } => {
                    let x = e3p_val(a, env)?;
                    let r = match kind {
                        AConv::Sext | AConv::Zext => x,
                        AConv::Trunc => x, // 函数内全 I64，截断恒等
                        _ => { env.depth -= 1; return None; }
                    };
                    env.regs.insert(*dst, r);
                }
                IrInst::Call { dst, func, args: cargs, linkage, .. } => {
                    if *linkage != CallLink::Dial { env.depth -= 1; return None; }
                    // 实参先在当前（父）环境求值。
                    let mut cv = Vec::with_capacity(cargs.len());
                    for a in cargs { cv.push(e3p_val(a, env)?); }
                    // Dial 的 vreg 是「函数内编号」，递归重入会复用同样的 vreg 号；
                    // 因此子调用必须用独立的 regs 环境，否则内层会覆盖外层活跃值。
                    // 仅把步数计数带入 / 带回，保证总预算仍受控。
                    let mut child = E3pEnv {
                        regs: HashMap::new(),
                        steps: env.steps,
                        depth: env.depth,
                    };
                    let r = e3p_eval_call(ip, func, &cv, &mut child)?;
                    env.steps = child.steps;
                    if let Some(d) = dst { env.regs.insert(*d, r); }
                }
                _ => { env.depth -= 1; return None; }
            }
        }
        match &b.term {
            Term::Jump { target, .. } => cur = *target,
            Term::Branch { cond, then_b, else_b, .. } => {
                let c = *env.regs.get(cond)?;
                cur = if c != 0 { *then_b } else { *else_b };
            }
            Term::Return { val } => {
                let v = match val { Some(x) => e3p_val(x, env)?, None => { env.depth -= 1; return None; } };
                break v;
            }
            Term::Unreachable => { env.depth -= 1; return None; }
        }
    };
    env.depth -= 1;
    Some(result)
}

/// 对程序中所有「实参全为整数常量的纯函数调用」做编译期求值，
/// 把 Call 指令替换为 `Conv Zext dst (Imm I64 result)`（既有 pass 已用的常量定义形式）。
/// 返回被折叠的调用数。
pub fn fold_pure_calls(ip: &mut IrProgram) -> usize {
    // 先算出全程序纯函数集合。
    let mut pure: HashSet<String> = HashSet::new();
    let all_names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for name in &all_names {
        let mut visiting = HashSet::new();
        if e3p_is_pure(ip, name, &mut visiting) {
            pure.insert(name.clone());
        }
    }
    if pure.is_empty() { return 0; }

    // 收集待折叠：(func_name, block_idx, inst_idx, dst, result)。
    let mut edits: Vec<(String, usize, usize, VReg, i64)> = Vec::new();
    for f in &ip.functions {
        for (bi, b) in f.blocks.iter().enumerate() {
            for (ii, inst) in b.insts.iter().enumerate() {
                if let IrInst::Call { dst: Some(d), func, args, linkage: CallLink::Dial, ret_ty, .. } = inst {
                    if *ret_ty != IrType::I64 || !pure.contains(func) { continue; }
                    // 护栏：只折叠「带非空常量实参」的调用。
                    // 无参纯函数（如 benchmark 的 b02/b03 任务体）虽可整体求值且语义正确，
                    // 但会让 driver 的调用被常量替换、使运行时基准失真，且编译期求值面更大；
                    // 第一版保守排除。递归内部的 rsumR(n-1) 等实参非空，照常处理。
                    if args.is_empty() { continue; }
                    // 实参必须全部是整数立即数。
                    let mut cv = Vec::with_capacity(args.len());
                    let mut ok = true;
                    for a in args {
                        match a {
                            Value::Imm(im) => cv.push(im.as_i64()),
                            _ => { ok = false; break; }
                        }
                    }
                    if !ok { continue; }
                    let mut env = E3pEnv::new();
                    if let Some(r) = e3p_eval_call(ip, func, &cv, &mut env) {
                        edits.push((f.name.clone(), bi, ii, *d, r));
                    }
                }
            }
        }
    }

    // 应用替换（按块/指令索引从后往前，避免索引失效）。用 dst 定位以防同块多改。
    let mut count = 0;
    for (fname, bi, ii, dst, result) in edits {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == fname) {
            if let Some(inst) = f.blocks.get_mut(bi).and_then(|b| b.insts.get_mut(ii)) {
                if inst.def() == Some(dst) && matches!(inst, IrInst::Call { .. }) {
                    *inst = IrInst::Conv {
                        kind: AConv::Zext,
                        dst,
                        a: Value::Imm(Imm::I64(result)),
                        ty: IrType::I64,
                    };
                    count += 1;
                }
            }
        }
    }
    count
}
