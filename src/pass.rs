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
        IrInst::Alloca { .. } | IrInst::LeaLabel { .. } => {}
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
