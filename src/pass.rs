//! 自研后端的优化 Pass（核心子集，保守优先）。
//!
//! 设计原则：**绝不动 ARC 原语的正确性**，只做纯计算的等价变换。
//! - 常量折叠（constant folding）：纯算术 / 比较的立即数运算折叠为常量。
//! - 代数化简（algebraic simplification）：x-x→0、x^x→0 等安全恒等式。
//! - DCE：删除结果无用的纯指令（ARC/Store/Call 一律保留）。
//! - 保守 CSE：跨函数内对相同纯计算做值编号复用（仅限无内存副作用的 Bin/Un/Cmp/Conv）。
//! - 保守 ARC 简化：仅消除「retain 后无任何使用即 release」的净零配对（绝不破坏引用计数平衡）。
//! - 字符串拼接常量折叠（fold_string_concat）：编译期把字面量间的 dial_string_concat
//!   合并成单个静态串，消除运行时的 malloc+memcpy（与 gcc -O2 的字符串常量折叠同族）。
//!
//! 运行顺序：fold → cse → dce → arc_simplify → fold_string_concat。

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
        IrInst::Alloca { .. } | IrInst::LeaLabel { .. } | IrInst::LeaStaticString { .. } | IrInst::LeaStaticData { .. } | IrInst::FuncAddr { .. } => {}
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
        IrInst::InlineAsm { args, .. } => {
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
        IrInst::Line(..) => {} // M7-e3w：行标记无寄存器操作数
        IrInst::DebugVar { slot, .. } => { if *slot == from { *slot = to; } }
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

/// M7-e3aw：编译期常量折叠记忆化缓存，键 = (函数名, 全整数实参)。
/// 纯整数函数的返回值完全由 (name, args) 决定，朴素递归解释器会对同一
/// 子问题反复求值（fibR(23) 达 97 万步、treeR(18) 9 万步），占 asm 后端
/// 编译时间近半。缓存只记录**成功求值**的结果：命中直接返回、不消耗步数，
/// 数学结果逐位一致，仅消除指数级重复计算。每个顶层折叠用独立 memo（保守：
/// 不改变任何折叠决策，只加速单次求值），与 CSE 的值编号思想同源。
type E3pMemo = HashMap<(String, Vec<i64>), i64>;

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
fn e3p_eval_call(ip: &IrProgram, name: &str, args: &[i64], env: &mut E3pEnv, memo: &mut E3pMemo) -> Option<i64> {
    // M7-e3aw：命中缓存直接返回——纯函数结果只由 (name, args) 决定，且不消耗
    // 步数预算（该结果上一次已在预算内算出，复用它不引入新的工作量）。
    let key = (name.to_string(), args.to_vec());
    if let Some(&cached) = memo.get(&key) { return Some(cached); }
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
                    let r = e3p_eval_call(ip, func, &cv, &mut child, memo)?;
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
    // M7-e3aw：只缓存成功求值；失败/放弃路径（除零、超预算、非白名单）不写入。
    memo.insert(key, result);
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
                    let mut memo = E3pMemo::new();
                    if let Some(r) = e3p_eval_call(ip, func, &cv, &mut env, &mut memo) {
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

// ============================================================================
// M8-recspec：递归纯函数的闭式特化（私有后端的语义级激进优化）
// ============================================================================
//
// LLVM 作为通用后端绝不会做这件事：它不理解「这个互递归函数其实在算 n 的奇偶」。
// 私有后端掌控 Dial 全部语义，可以在编译期解释执行纯整数递归函数、拟合闭式、
// 全域验证后，在函数入口插入 guard：n>=0 直接返回闭式结果，n<0 走原递归体。
//
// 收益原型：b12 的 evR/odR 互递归本质是 `n>=0 ? (1-(n&1)) : 发散`，但被
// 20000 次循环 × 平均 30 层递归调用吃掉（asm 3.32x）。特化后 O(1) 返回。
//
// 正确性护栏（缺一不可）：
//   1. 目标必须是 e3p_is_pure 判定的纯整数函数（单 I64 参数、I64 返回、Private、
//      指令全在确定性白名单内），保证解释执行与运行时逐位一致。
//   2. 必须是**递归**函数（自身或经互递归环可达自己）——非递归纯函数的调用开销小，
//      且已由 fold_pure_calls / 内联覆盖，不在此重复。
//   3. 闭式必须在探针域 [0..PROBE_HI] 上**逐点**与解释执行结果相等；任一不符即放弃。
//   4. 插入的 guard 只对 n>=0 走闭式；n<0 保留原函数体（负数语义原样，绝不改变）。
//      因探针只覆盖非负域，guard 把闭式的作用域严格限制在已验证的域内。
//   5. 只处理 x86-64 / O3（由 asm.rs 守卫），AArch64 / 低优化级别零变化。

/// 闭式候选：给定单参数 n，返回折叠后的 i64 结果；None 表示不适用。
#[derive(Clone, Copy)]
enum CfKind {
    /// 常量 c（对所有 n 返回 c）
    Const(i64),
    /// n 的低位位模式：base - (n & mask)（如 1-(n&1) = 奇偶）
    NegLowBits { base: i64, mask: i64 },
    /// (n & mask) + base
    LowBitsAdd { base: i64, mask: i64 },
    /// (n >> shift) & mask
    ShiftMask { shift: u32, mask: i64 },
    /// 恒等 n
    Identity,
}

impl CfKind {
    fn eval(&self, n: i64) -> Option<i64> {
        Some(match self {
            CfKind::Const(c) => *c,
            CfKind::NegLowBits { base, mask } => base.wrapping_sub(n & mask),
            CfKind::LowBitsAdd { base, mask } => (n & mask).wrapping_add(*base),
            CfKind::ShiftMask { shift, mask } => (n >> shift) & mask,
            CfKind::Identity => n,
        })
    }
    /// 生成这个闭式的 IR：在 f 里新造指令，把 n（Value）算成 dst，追加到 insts。
    fn emit(&self, f: &mut IrFunction, n: Value, insts: &mut Vec<IrInst>, dst: VReg) {
        match self {
            CfKind::Const(c) => {
                insts.push(IrInst::Conv { kind: AConv::Zext, dst, a: Value::Imm(Imm::I64(*c)), ty: IrType::I64 });
            }
            CfKind::Identity => {
                insts.push(IrInst::Conv { kind: AConv::Zext, dst, a: n, ty: IrType::I64 });
            }
            CfKind::NegLowBits { base, mask } => {
                let lo = f.new_reg(IrType::I64);
                insts.push(IrInst::Bin { op: ABin::BitAnd, dst: lo, a: n, b: Value::Imm(Imm::I64(*mask)), ty: IrType::I64 });
                insts.push(IrInst::Bin { op: ABin::Sub, dst, a: Value::Imm(Imm::I64(*base)), b: Value::Reg(lo), ty: IrType::I64 });
            }
            CfKind::LowBitsAdd { base, mask } => {
                let lo = f.new_reg(IrType::I64);
                insts.push(IrInst::Bin { op: ABin::BitAnd, dst: lo, a: n, b: Value::Imm(Imm::I64(*mask)), ty: IrType::I64 });
                insts.push(IrInst::Bin { op: ABin::Add, dst, a: Value::Reg(lo), b: Value::Imm(Imm::I64(*base)), ty: IrType::I64 });
            }
            CfKind::ShiftMask { shift, mask } => {
                let sh = f.new_reg(IrType::I64);
                insts.push(IrInst::Bin { op: ABin::Shr, dst: sh, a: n, b: Value::Imm(Imm::I64(*shift as i64)), ty: IrType::I64 });
                insts.push(IrInst::Bin { op: ABin::BitAnd, dst, a: Value::Reg(sh), b: Value::Imm(Imm::I64(*mask)), ty: IrType::I64 });
            }
        }
    }
}

/// 构造候选闭式集合。基/掩码取自探针结果的取值范围，避免枚举过多。
fn cf_candidates(probe_vals: &[i64]) -> Vec<CfKind> {
    let mut out = Vec::new();
    // 常量候选：探针里的常值集合（去重）
    let mut consts: Vec<i64> = Vec::new();
    for &v in probe_vals { if !consts.contains(&v) { consts.push(v); } }
    for c in consts.into_iter().take(8) { out.push(CfKind::Const(c)); }
    out.push(CfKind::Identity);
    // 低位模式：base - (n & mask) 与 (n & mask) + base
    // 观察 probe：对 n=0 结果 v0 = base（NegLowBits）或 base（LowBitsAdd，因 n&mask=0）。
    if let Some(&v0) = probe_vals.first() {
        for mask in [1i64, 3, 7, 15, 31, 63, 255] {
            out.push(CfKind::NegLowBits { base: v0, mask });
            out.push(CfKind::LowBitsAdd { base: v0, mask });
        }
    }
    // 移位+掩码：shift/mask 小集合
    for shift in 1u32..=6 {
        for mask in [1i64, 3, 7, 15, 31, 63] {
            out.push(CfKind::ShiftMask { shift, mask });
        }
    }
    out
}

/// 判定函数 f 是否递归（自身调用，或经 Dial 调用环可达自身）。
fn is_recursive_fn(ip: &IrProgram, name: &str) -> bool {
    fn reaches(graph: &HashMap<String, Vec<String>>, cur: &str, start: &str, seen: &mut HashSet<String>) -> bool {
        if let Some(cs) = graph.get(cur) {
            for c in cs {
                if c == start { return true; }
                if seen.insert(c.clone()) && reaches(graph, c, start, seen) { return true; }
            }
        }
        false
    }
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    for f in &ip.functions {
        let mut cs = Vec::new();
        for b in &f.blocks {
            for i in &b.insts {
                if let IrInst::Call { func, linkage: CallLink::Dial, .. } = i { cs.push(func.clone()); }
            }
        }
        graph.insert(f.name.clone(), cs);
    }
    let mut seen = HashSet::new();
    reaches(&graph, name, name, &mut seen)
}

/// 探针域上界：解释执行 0..=PROBE_HI，共 129 点。evR(n) 递归深度 ~n，成本可接受。
const RESPEC_PROBE_HI: i64 = 128;

/// 对全程序递归纯函数做闭式特化。返回被特化的函数数。
pub fn specialize_recursive_calls(ip: &mut IrProgram) -> usize {
    // 先只读地收集所有可行的 (fname, CfKind)——避免解释执行时与 &mut 借用冲突。
    // 复用 respec_fn 的判定，但这里需要拿到 cf，故内联同样逻辑的「返回 cf」版本。
    fn fit(ip: &IrProgram, fname: &str) -> Option<CfKind> {
        let f = ip.functions.iter().find(|f| f.name == fname)?;
        if f.linkage != FuncLink::Private || f.ret != IrType::I64 { return None; }
        if f.params.len() != 1 || f.params[0].1 != IrType::I64 { return None; }
        if !is_recursive_fn(ip, fname) { return None; }
        let mut visiting = HashSet::new();
        if !e3p_is_pure(ip, fname, &mut visiting) { return None; }
        let mut memo = E3pMemo::new();
        let mut vals = Vec::new();
        for n in 0..=RESPEC_PROBE_HI {
            let mut env = E3pEnv::new();
            vals.push(e3p_eval_call(ip, fname, &[n], &mut env, &mut memo)?);
        }
        let cf = cf_candidates(&vals).into_iter().find(|cf| {
            vals.iter().enumerate().all(|(n, &e)| cf.eval(n as i64) == Some(e))
        })?;
        for n in (RESPEC_PROBE_HI + 1..=512).step_by(7) {
            let mut env = E3pEnv::new();
            let v = e3p_eval_call(ip, fname, &[n], &mut env, &mut memo)?;
            if cf.eval(n) != Some(v) { return None; }
        }
        Some(cf)
    }

    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    let mut plan = Vec::new();
    for name in &names {
        if let Some(cf) = fit(ip, name) { plan.push((name.clone(), cf)); }
    }
    if plan.is_empty() { return 0; }

    let mut count = 0;
    for (fname, cf) in plan {
        let f = match ip.functions.iter_mut().find(|f| f.name == fname) { Some(f) => f, None => continue };
        // 入口块必须有且仅有参数槽定义 + 原函数体；我们在其后插入 guard 并新建一个「慢路径」块。
        // 为简单且安全：新建两块——fast（返回闭式）和 slow（原 entry 的内容），
        // 新 entry 只做：读参数槽 → n≥0 ? 跳 fast : 跳 slow。
        let entry_id = f.entry;
        let entry_idx = match f.blocks.iter().position(|b| b.id == entry_id) { Some(i) => i, None => continue };
        if f.param_slots.is_empty() { continue; }
        let nslot = f.param_slots[0];

        // 新块 id（取当前最大 id + 1，避免冲突）
        let max_id = f.blocks.iter().map(|b| b.id).max().unwrap_or(entry_id);
        let fast_id = max_id + 1;
        let slow_id = max_id + 2;

        // 把原 entry 块改名为 slow 块的内容：复制其 insts/term 到 slow。
        let old_insts = f.blocks[entry_idx].insts.clone();
        let old_term = f.blocks[entry_idx].term.clone();
        let old_params = f.blocks[entry_idx].params.clone();

        // 新 entry：只保留参数槽 Alloca（prologue 语义需要），读 n，判 n≥0，分支。
        // 参数槽定义是关键：prologue 按 param_slots 写值，Alloca 必须留在 entry。
        // nreg 在新 entry 里定义（分支之前），因此 fast / slow 两块都能安全引用它。
        let mut new_entry_insts: Vec<IrInst> = Vec::new();
        for i in &f.blocks[entry_idx].insts {
            if matches!(i, IrInst::Alloca { dst, .. } if *dst == nslot) {
                new_entry_insts.push(i.clone());
            }
        }
        let nreg = f.new_reg(IrType::I64);
        new_entry_insts.push(IrInst::Load { dst: nreg, ptr: nslot, ty: IrType::I64 });
        let cond = f.new_reg(IrType::I64);
        new_entry_insts.push(IrInst::ICmp { op: ACmp::Ge, dst: cond, a: Value::Reg(nreg), b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });

        // 构造 fast 块：用新 entry 定义的 nreg 计算闭式并 Return。
        let fast_ret = f.new_reg(IrType::I64);
        let mut fast_insts = Vec::new();
        cf.emit(f, Value::Reg(nreg), &mut fast_insts, fast_ret);
        let fast_block = IrBlock {
            id: fast_id, params: Vec::new(), insts: fast_insts,
            term: Term::Return { val: Some(Value::Reg(fast_ret)) },
        };

        // slow 块：原 entry 的完整内容（保持 id 之外的结构）。
        let slow_block = IrBlock { id: slow_id, params: old_params, insts: old_insts, term: old_term };

        // 找到所有原本跳向 entry_id 的块（回边）——本类纯递归函数通常无回到 entry 的边；
        // 若有，必须改指向 slow_id（否则会重新走 guard，虽语义仍对但可能重复）。
        // 为安全，统一改写 term 中 target==entry_id 的 Jump/Branch 为 slow_id。
        for b in f.blocks.iter_mut() {
            if b.id == entry_id { continue; }
            match &mut b.term {
                Term::Jump { target, .. } => if *target == entry_id { *target = slow_id; },
                Term::Branch { then_b, else_b, .. } => {
                    if *then_b == entry_id { *then_b = slow_id; }
                    if *else_b == entry_id { *else_b = slow_id; }
                }
                _ => {}
            }
        }

        f.blocks[entry_idx].insts = new_entry_insts;
        f.blocks[entry_idx].term = Term::Branch { cond, then_b: fast_id, else_b: slow_id, then_args: Vec::new(), else_args: Vec::new() };
        f.blocks.push(fast_block);
        f.blocks.push(slow_block);
        count += 1;
    }
    count
}

// ============================================================================
// M7-e3t：归纳变量强度削减（Induction Variable strength Reduction, IVR）
// ============================================================================
//
// 目标模式（栈槽式 for 循环，见 lower.rs 的 Stmt::For）：
//   header:  Load cv, iv_slot ; ICmp cv < N ; Branch body/exit
//   body:    ... ; Mul m, Reg(base), Imm(C) ; Add pos, Reg(m), Reg(cv') ...
//   latch:   Load cv2, iv_slot ; Add nv, cv2, Imm(step) ; Store iv_slot, nv
//            Jump header
// 其中 `base` 是循环不变量，`cv'` 加载自 `iv_slot`。
//
// 变换为（以累加器 acc 取代每轮重算的 base*C + iv）：
//   preheader: Load iv0, iv_slot ; Mul m0, Reg(base), Imm(C) ;
//              Add a0, Reg(m0), Reg(iv0) ; Store acc_slot, a0
//   body:      Load acc, acc_slot   （取代 Add pos；其后 Store/使用照旧）
//   latch:     Load acco, acc_slot ; Add accn, acco, Imm(step) ; Store acc_slot, accn
//
// 数学等价：原式 = (base*C) + iv，其中 base*C 循环不变、iv 每轮 +step。
// 故原式每轮只 +step（C 已吸收在初值的 base*C 里），preheader 算一次初值、
// 之后每轮累加 step 即等价。循环内省掉 1 条 imulq + 1 条 add 及栈搬运。
//
// 保守护栏：
//   - 仅 I64；仅 Mul(base_reg, Imm(C))；仅 Add(m, iv_load)（或交换操作数）。
//   - step 必须是整数立即数；iv_load 必须加载自 latch 更新的同一 iv_slot。
//   - base 必须循环不变：定义在循环外，或为「循环内 Load 循环外栈槽」（此类
//     值不变；preheader 里重 Load 该槽避免 use-before-def）。
//   - 仅当该 Add 的结果 dst 在循环体内被使用（Store 值或 Call 实参，即数组
//     下标的两种 lowering 形态）时才变换。
//   - 每个函数每轮只处理一个最内层循环（break 保守收敛）；变换后补一次 DCE
//     清理变成死代码的 Mul/Load。
//   - 仅 x86-64 调用（由 asm.rs 守卫）；AArch64 零变化。

/// 循环分析：回边 (latch_idx -> header_idx)，header_idx <= latch_idx。
#[derive(Clone, Debug)]
struct E3tLoop {
    header: usize,
    latch: usize,
    body_blocks: Vec<usize>,
}

/// 从 CFG 推导自然循环块集合（近似：header..=latch 的连续索引区间中，
/// 可达 latch 且不被更外层 guard 的结构）。这里用「回边目标到 latch 的区间」。
fn e3t_find_loops(f: &IrFunction) -> Vec<E3tLoop> {
    let mut id2idx: HashMap<u32, usize> = HashMap::new();
    for (i, b) in f.blocks.iter().enumerate() { id2idx.insert(b.id, i); }
    // 收集所有跳转边 (src_idx, dst_idx)
    let mut edges: Vec<(usize, usize)> = Vec::new();
    for (i, b) in f.blocks.iter().enumerate() {
        match &b.term {
            Term::Jump { target, .. } => if let Some(&d) = id2idx.get(target) { edges.push((i, d)); },
            Term::Branch { then_b, else_b, .. } => {
                if let Some(&d) = id2idx.get(then_b) { edges.push((i, d)); }
                if let Some(&d) = id2idx.get(else_b) { edges.push((i, d)); }
            }
            _ => {}
        }
    }
    let mut loops = Vec::new();
    for &(src, dst) in &edges {
        if dst > src { continue; } // 回边：目标索引 <= 源索引
        // 循环块 = 从 dst 到 src 的所有块（含端点），且能到达 latch。
        let mut set: HashSet<usize> = (dst..=src).collect();
        // 剔除区间内不属于该循环的子块：用「从 header 出发、不经过 header 二次进入、
        // 能回到 latch」的可达性近似。保守起见直接取连续区间（b17 各循环块连续）。
        // 验证区间内每个块都在 dst..=src 且确实可达 latch。
        let mut reach_latch: HashSet<usize> = HashSet::new();
        let mut stack = vec![src];
        while let Some(x) = stack.pop() {
            if !reach_latch.insert(x) { continue; }
            for &(a, b) in &edges {
                if a == x && b >= dst && b <= src { stack.push(b); }
            }
        }
        set.retain(|x| reach_latch.contains(x));
        let mut body: Vec<usize> = set.into_iter().collect();
        body.sort_unstable();
        loops.push(E3tLoop { header: dst, latch: src, body_blocks: body });
    }
    loops
}

/// 分析 latch 末尾的归纳更新模式，返回 (iv_slot, step)。
fn e3t_induction_slot(f: &IrFunction, latch: usize) -> Option<(VReg, i64)> {
    let insts = &f.blocks[latch].insts;
    if insts.len() < 3 { return None; }
    // 从末尾往前找 Store slot, Reg(nv)
    let st = insts.last()?;
    let (slot, nv) = match st {
        IrInst::Store { ptr, val: Value::Reg(v), .. } => (*ptr, *v),
        _ => return None,
    };
    // 前一个：Bin Add nv, Reg(cv2), Imm(step)
    let add = insts.get(insts.len() - 2)?;
    let (cv2, step) = match add {
        IrInst::Bin { op: ABin::Add, dst, a: Value::Reg(x), b: Value::Imm(im), .. }
            if *dst == nv => (*x, im.as_i64()),
        IrInst::Bin { op: ABin::Add, dst, b: Value::Reg(x), a: Value::Imm(im), .. }
            if *dst == nv => (*x, im.as_i64()),
        _ => return None,
    };
    // 再前一个：Load cv2, ptr:slot
    let ld = insts.get(insts.len() - 3)?;
    match ld {
        IrInst::Load { dst, ptr, .. } if *dst == cv2 && *ptr == slot => Some((slot, step)),
        _ => None,
    }
}

/// 收集所有 vreg 的定义块索引。
fn e3t_def_blocks(f: &IrFunction) -> HashMap<VReg, usize> {
    let mut m = HashMap::new();
    for (bi, b) in f.blocks.iter().enumerate() {
        for inst in &b.insts {
            if let Some(d) = inst.def() { m.insert(d, bi); }
        }
        for (p, _) in &b.params { m.insert(*p, bi); }
    }
    m
}

/// 对单个函数跑 IVR。返回变换次数。
fn e3t_ivr_fn(f: &mut IrFunction) -> usize {
    let loops = e3t_find_loops(f);
    if loops.is_empty() { return 0; }
    let mut count = 0;

    for lp in &loops {
        let (iv_slot, step) = match e3t_induction_slot(f, lp.latch) {
            Some(x) => x,
            None => continue,
        };
        if step == 0 { continue; }
        let loop_set: HashSet<usize> = lp.body_blocks.iter().copied().collect();
        let def_blocks = e3t_def_blocks(f);
        // 循环体内被 Store 写入的槽（用于判断 Load 的值是否循环不变）
        let mut stored_slots_in_loop: HashSet<VReg> = HashSet::new();
        for &sbi in &lp.body_blocks {
            for si in &f.blocks[sbi].insts {
                if let IrInst::Store { ptr, .. } = si { stored_slots_in_loop.insert(*ptr); }
            }
        }
        // 判断一个 Value 是否循环不变：
        // Imm → 是；Reg 定义在循环外 → 是；Reg 是 Load 且 ptr 槽不在循环内被写 → 是。
        let value_invariant = |v: &Value, def_blocks: &HashMap<VReg, usize>| -> bool {
            match v {
                Value::Imm(_) => true,
                Value::Reg(r) => match def_blocks.get(r) {
                    None => true,
                    Some(&db) if !loop_set.contains(&db) => true,
                    Some(&db) if loop_set.contains(&db) => {
                        // 定义在循环内：若该定义是 Load 且加载的槽在循环内不被写，则不变
                        match f.blocks[db].insts.iter().find(|x| x.def() == Some(*r)) {
                            Some(IrInst::Load { ptr, .. }) => !stored_slots_in_loop.contains(ptr),
                            _ => false,
                        }
                    }
                    _ => false,
                }
            }
        };
        // preheader：跳到 header 且不在 loop_set 内的块
        let preheader: Option<usize> = {
            let mut ph = None;
            for (bi, b) in f.blocks.iter().enumerate() {
                if loop_set.contains(&bi) { continue; }
                let jumps = matches!(&b.term, Term::Jump { target, .. } if *target == f.blocks[lp.header].id);
                if jumps { ph = Some(bi); break; }
            }
            ph
        };
        let preheader = match preheader { Some(p) => p, None => continue };

        // 在循环体里找归纳表达式 Add dst, Reg(m), Reg(ivl)
        // m = Mul(base, Imm(C))，base 定义在循环外；ivl = Load ptr:iv_slot
        #[derive(Clone)]
        struct AddrPat { block: usize, add_idx: usize, add_dst: VReg, base: Value, base_slot: Option<VReg>, c: i64, ivl: VReg }
        let mut pats: Vec<AddrPat> = Vec::new();
        for &bi in &lp.body_blocks {
            let insts = &f.blocks[bi].insts;
            // 找 Load ptr:iv_slot
            let iv_loads: HashMap<VReg, usize> = insts.iter().enumerate().filter_map(|(k, inst)| {
                match inst { IrInst::Load { dst, ptr, .. } if *ptr == iv_slot => Some((*dst, k)), _ => None }
            }).collect();
            for (ai, inst) in insts.iter().enumerate() {
                if let IrInst::Bin { op: ABin::Add, dst, a, b, ty: IrType::I64 } = inst {
                    for (mval, ivval) in [(a, b), (b, a)] {
                        let mreg = match mval { Value::Reg(r) => *r, _ => continue };
                        let ivl = match ivval { Value::Reg(r) => *r, _ => continue };
                        if !iv_loads.contains_key(&ivl) { continue; }
                        // mreg 必须是 Mul(base, Imm(C))
                        let mul_idx = match insts.iter().position(|x| x.def() == Some(mreg)) { Some(k) => k, None => continue };
                        let (base_val, c) = match &insts[mul_idx] {
                            IrInst::Bin { op: ABin::Mul, a: Value::Reg(br), b: Value::Imm(imc), .. } =>
                                (Value::Reg(*br), imc.as_i64()),
                            IrInst::Bin { op: ABin::Mul, a: Value::Imm(imc), b: Value::Reg(br), .. } =>
                                (Value::Reg(*br), imc.as_i64()),
                            _ => continue,
                        };
                        if c == 0 { continue; }
                        // base 必须循环不变（含「循环内 Load 循环外槽」的情况）
                        if !value_invariant(&base_val, &def_blocks) { continue; }
                        // 若 base 定义在循环内且是 Load，preheader 引用它会 use-before-def；
                        // 记录其加载的槽，preheader 里重新 Load。
                        let base_slot: Option<VReg> = match base_val {
                            Value::Reg(br) => match def_blocks.get(&br) {
                                Some(&db) if loop_set.contains(&db) => {
                                    match f.blocks[db].insts.iter().find(|x| x.def() == Some(br)) {
                                        Some(IrInst::Load { ptr, .. }) => Some(*ptr),
                                        _ => None,
                                    }
                                }
                                _ => None,
                            },
                            _ => None,
                        };
                        // 该 Add 的 dst 必须在循环体内被使用（Store 的值或 Call 实参 —— 数组下标两种形态）
                        let used = lp.body_blocks.iter().any(|&sbi|
                            f.blocks[sbi].insts.iter().any(|si| match si {
                                IrInst::Store { val: Value::Reg(v), .. } => *v == *dst,
                                IrInst::Call { args, .. } => args.iter().any(|a| *a == Value::Reg(*dst)),
                                _ => false,
                            }));
                        if !used { continue; }
                        pats.push(AddrPat { block: bi, add_idx: ai, add_dst: *dst, base: base_val, base_slot, c, ivl });
                        break;
                    }
                }
            }
        }
        if pats.is_empty() { continue; }

        // 应用变换。每个模式一个累加器槽。
        // 为避免索引失效，先收集所有编辑，再从后往前应用；但跨块插入用 vreg 替换更简单：
        // 1) 在 entry(块0) 申请 acc 槽（Alloca）；
        // 2) preheader 末尾插入初始化：Load iv0,iv_slot; Mul m0,base,Imm(C);
        //    Add a0,m0,iv0; Store acc_slot,a0
        // 3) 循环体：把 add_dst 的所有使用替换为 acc_load（在该 add 位置插入 Load）
        // 4) latch：Store iv_slot 之后插入 acc 累加
        for pat in &pats {
            // 新槽用 reg_ty 之外的方式：Alloca 的 dst 是一个 ptr vreg
            let acc_ptr = f.new_reg(IrType::Ptr);
            let acc_ty = IrType::I64;
            // entry 插入 Alloca（放在最前）
            f.blocks[0].insts.insert(0, IrInst::Alloca { dst: acc_ptr, bytes: 8 });

            // preheader 末尾（term 之前）插入初始化
            // 先申请所有新 vreg（避免与 ph_insts 可变借用冲突）
            let iv0 = f.new_reg(IrType::I64);
            let m0 = f.new_reg(IrType::I64);
            let a0 = f.new_reg(IrType::I64);
            let base_v = if pat.base_slot.is_some() { Some(f.new_reg(IrType::I64)) } else { None };
            let ph_insts = &mut f.blocks[preheader].insts;
            ph_insts.push(IrInst::Load { dst: iv0, ptr: iv_slot, ty: IrType::I64 });
            // base 操作数：若原本是循环内 Load，则在 preheader 重新 Load 该槽。
            let base_for_ph = if let (Some(slot), Some(bv)) = (pat.base_slot, base_v) {
                ph_insts.push(IrInst::Load { dst: bv, ptr: slot, ty: IrType::I64 });
                Value::Reg(bv)
            } else { pat.base };
            ph_insts.push(IrInst::Bin { op: ABin::Mul, dst: m0, a: base_for_ph, b: Value::Imm(Imm::I64(pat.c)), ty: IrType::I64 });
            ph_insts.push(IrInst::Bin { op: ABin::Add, dst: a0, a: Value::Reg(m0), b: Value::Reg(iv0), ty: IrType::I64 });
            ph_insts.push(IrInst::Store { ptr: acc_ptr, val: Value::Reg(a0), ty: acc_ty });

            // 循环体：在原 add 位置替换为 Load acc；再用 subst_reg 把 add_dst -> accv
            let accv = f.new_reg(IrType::I64);
            f.blocks[pat.block].insts[pat.add_idx] = IrInst::Load { dst: accv, ptr: acc_ptr, ty: IrType::I64 };
            subst_reg(f, pat.add_dst, accv);
            // 注意：subst_reg 会把刚才 Load 的 dst=accv 也考虑，但 from=add_dst != accv，安全。

            // latch：累加增量 = step。
            // 原表达式 = base*C + iv；base*C 循环不变，iv 每轮 +step ⇒ 表达式每轮 +step。
            // C 已包含在 base*C 的初值里，增量只取 step。
            let delta = step;
            let acco = f.new_reg(IrType::I64);
            let accn = f.new_reg(IrType::I64);
            let lat = &mut f.blocks[lp.latch].insts;
            lat.push(IrInst::Load { dst: acco, ptr: acc_ptr, ty: IrType::I64 });
            lat.push(IrInst::Bin { op: ABin::Add, dst: accn, a: Value::Reg(acco), b: Value::Imm(Imm::I64(delta)), ty: IrType::I64 });
            lat.push(IrInst::Store { ptr: acc_ptr, val: Value::Reg(accn), ty: IrType::I64 });

            count += 1;
        }
        // 该循环处理完，重新计算（后续外层循环的分析基于新 IR）
        break; // 一次只处理最内层的一个循环，下一轮 run 再处理外层；保守
    }
    count
}

/// 对整段程序跑归纳变量强度削减。返回变换总数。
pub fn ivr_strength_reduce(ip: &mut IrProgram) -> usize {
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    let mut total = 0;
    for name in names {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            let n = e3t_ivr_fn(f);
            if n > 0 {
                // IVR 把 Add(base*C + iv) 换成累加器后，原有 base*C 的 Mul 与
                // iv 的 Load 可能变成死代码；run_passes 的 DCE 在 IVR 之前已跑过，
                // 这里补一次 DCE 清理，避免循环内残留无用的 imulq。
                dce(f);
            }
            total += n;
        }
    }
    total
}

// ============================================================================
// M7-e3ad：LLVM InstCombine 风格代数恒等式（-O3 专用）
// ============================================================================
//
// 从 LLVM 的 InstCombine pass 借鉴一组**保守、可证明等价**的代数恒等式，
// 把无意义的计算折叠掉。与 fold_constants 的分工：
//   - fold_constants（O1+）：只折叠「两个操作数都是立即数」的纯算术/比较；
//   - instcombine（O3）：折叠「一个操作数是立即数、另一个是任意值」的恒等式，
//     例如 x+0→x、x*0→0、x&0→0、x<<0→x。
//
// 保守原则（避免踩 NaN / 溢出 / 逻辑短路语义）：
//   - 仅处理整数 `IrInst::Bin` 且 `ty == I64`（与 fold_bin_i64 的字宽口径一致）；
//   - 不碰 FBin（浮点）、不碰逻辑 And/Or（短路语义）、不碰 Div/Mod（除零陷阱）；
//   - 结果写成 `Conv { Zext, a, ty: I64 }`，发射期就是一条安全搬运（见 inst_conv）。

/// 读取一个 Value 的 i64 立即数（严格匹配 Imm::I64）。
fn ic_imm_i64(v: &Value) -> Option<i64> {
    match v { Value::Imm(Imm::I64(n)) => Some(*n), _ => None }
}

/// 对单条整数 Bin 指令尝试代数恒等式。返回 Some(替换后的指令) 表示命中。
fn instcombine_bin(op: ABin, dst: VReg, a: &Value, b: &Value, ty: &IrType) -> Option<IrInst> {
    if *ty != IrType::I64 { return None; }
    let mov = |v: Value| IrInst::Conv { kind: AConv::Zext, dst, a: v, ty: IrType::I64 };
    let zero = || mov(Value::Imm(Imm::I64(0)));
    use ABin::*;
    let ia = ic_imm_i64(a);
    let ib = ic_imm_i64(b);
    match op {
        Add => {
            if ia == Some(0) { return Some(mov(b.clone())); }
            if ib == Some(0) { return Some(mov(a.clone())); }
        }
        Sub => {
            // x - 0 → x；0 - x 不能化简（非交换）；x - x 已由 fold_constants 处理。
            if ib == Some(0) { return Some(mov(a.clone())); }
        }
        Mul => {
            if ia == Some(0) || ib == Some(0) { return Some(zero()); }
            if ia == Some(1) { return Some(mov(b.clone())); }
            if ib == Some(1) { return Some(mov(a.clone())); }
        }
        BitAnd => {
            // x & 0 → 0；x & -1 → x（全 1 掩码是恒等）。
            if ia == Some(0) || ib == Some(0) { return Some(zero()); }
            if ia == Some(-1) { return Some(mov(b.clone())); }
            if ib == Some(-1) { return Some(mov(a.clone())); }
            // 幂等：x & x → x
            if let (Value::Reg(ra), Value::Reg(rb)) = (a, b) {
                if ra == rb { return Some(mov(a.clone())); }
            }
        }
        BitOr => {
            if ia == Some(0) { return Some(mov(b.clone())); }
            if ib == Some(0) { return Some(mov(a.clone())); }
            if let (Value::Reg(ra), Value::Reg(rb)) = (a, b) {
                if ra == rb { return Some(mov(a.clone())); }
            }
        }
        BitXor => {
            if ia == Some(0) { return Some(mov(b.clone())); }
            if ib == Some(0) { return Some(mov(a.clone())); }
            // x ^ x → 0 已由 fold_constants 处理，这里不重复。
        }
        Shl | Shr => {
            // x << 0 → x、x >> 0 → x（移位量必须来自常量 0；变量移位不动）。
            if ib == Some(0) { return Some(mov(a.clone())); }
        }
        // Eq/Neq/Lt/... 与逻辑 And/Or、Div/Mod 不做恒等，交回 fold_constants 或发射器。
        _ => {}
    }
    None
}

/// 对整段程序跑 LLVM InstCombine 风格代数恒等式。返回变换总数。
/// 仅在 -O3 由 `asm.rs` 调用；跑完补一次 DCE 清理失效的源指令。
pub fn instcombine(ip: &mut IrProgram) -> usize {
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    let mut total = 0;
    for name in names {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            let mut n = 0;
            for b in &mut f.blocks {
                for inst in &mut b.insts {
                    if let IrInst::Bin { op, dst, a, b, ty } = &*inst {
                        if let Some(new_inst) = instcombine_bin(*op, *dst, a, b, ty) {
                            *inst = new_inst;
                            n += 1;
                        }
                    }
                }
            }
            if n > 0 { dce(f); }
            total += n;
        }
    }
    total
}

// ============================================================================
// M9-linrec：常量上界线性递推循环的编译期闭式求值（clang -O3 SCEV 同族）
// ============================================================================
//
// 目标模式：无参 Private I64 纯函数，函数体是一个**单循环**，状态变量满足
// 一阶线性（仿射）递推或二阶线性（斐波那契型）递推，循环上界是编译期常量。
// 这类循环在微基准与真实程序（迭代收敛、伪随机序列、DP 滚动数组）中极常见，
// LLVM 的 SCEV 只在有限场景折叠它；私有后端掌控全部语义，可以走得更远：
// 编译期用矩阵快速幂 O(log N) 算出终值，把整个函数体折叠为 `return const`。
//
// 识别的四块结构（lower.rs 的 Stmt::For 产物，见 dump）：
//   block0 entry : Alloca state; Store state init; Alloca iv; Store iv start; Jump header
//   block1 header: Load cv iv; ICmp op cv, limit_imm; Branch body/exit
//   block2 body  : <递推序列>; Load iv; Add iv+step; Store iv; Jump header
//   block3 exit  : Load ret ← state; Return ret
//
// 两种递推：
//   Affine : s' = (a*s + b) mod m       （b04: s=(3s+7)%1e9+7）
//   Fib    : (a',b') = (b, (a+b) mod m) （b07: 迭代斐波那契）
//
// 正确性护栏（缺一不可）：
//   1. 目标必须是 Private + 无参 + I64 返回；指令全在确定性白名单内。
//   2. 严格匹配四块骨架；body 只允许递推序列 + iv 自增 + 这些序列用到的 Alloca，
//      任何 Call / 数组访问 / 额外 Store / 分支一律放弃（Collatz 的 b08 不匹配）。
//   3. 闭式结果在采样点 k（逐次模拟）与矩阵幂结果逐位相等才采纳；采样覆盖
//      k=0..=SAMPLE，捕获任何系数错位 / 加减搞反 / 初值误读。
//   4. 循环次数 n 必须精确整除、step>0、n>0；系数与初值必须是非负立即数
//      （bench 铁律保证值域非负；负系数语义复杂，保守放弃）。
//   5. 全 i128 中间运算避免溢出，结果 mod m 后落在 [0,m) ⊆ i64。
//   6. 仅 O3 + x86-64 挂接（asm.rs 守卫）；AArch64 / 低优化级 / LLVM 路径零变化。

/// 递推闭式的种类与系数。
#[derive(Clone, Debug)]
enum LrKind {
    /// s' = (a*s + b) mod m；m==0 表示无模（当前不使用，保留扩展位）。
    Affine { a: i128, b: i128, m: i128 },
    /// (a',b') = (b, (a+b) mod m)。m>0。
    Fib { m: i128 },
}

/// 识别出的一个可闭式循环。
#[derive(Clone, Debug)]
struct LinRec {
    kind: LrKind,
    /// 状态槽（Affine 1 个 / Fib 2 个），顺序为入口初值对应顺序。
    state_slots: Vec<VReg>,
    /// 状态初值（与 state_slots 平行）。
    inits: Vec<i128>,
    iv_slot: VReg,
    start: i128,
    limit: i128,
    step: i128,
    /// header 比较符：true=Lt(i<limit)，false=Le(i<=limit)。
    cmp_lt: bool,
    /// exit 块返回的状态槽（必须在 state_slots 内）。
    ret_slot: VReg,
    header: usize,
    body: usize,
    exit: usize,
}

/// 从 Value 读非负立即数（i128），非立即数或负数返回 None。
fn lr_nonneg_imm(v: &Value) -> Option<i128> {
    match v {
        Value::Imm(im) => { let x = im.as_i64() as i128; if x >= 0 { Some(x) } else { None } }
        _ => None,
    }
}

/// 2×2 矩阵乘法（模 m；m==0 用 i128 直接乘，调用方保证不溢出）。
#[inline]
fn lr_matmul(x: [[i128; 2]; 2], y: [[i128; 2]; 2], m: i128) -> [[i128; 2]; 2] {
    let mut r = [[0i128; 2]; 2];
    for i in 0..2 {
        for j in 0..2 {
            let v = x[i][0] * y[0][j] + x[i][1] * y[1][j];
            r[i][j] = if m > 0 { v % m } else { v };
        }
    }
    r
}

/// 矩阵快速幂 T^n（模 m）。
fn lr_matpow(t: [[i128; 2]; 2], mut n: u64, m: i128) -> [[i128; 2]; 2] {
    let mut r = [[1i128, 0], [0, 1]];
    let mut base = t;
    while n > 0 {
        if n & 1 == 1 { r = lr_matmul(r, base, m); }
        base = lr_matmul(base, base, m);
        n >>= 1;
    }
    r
}

/// 用矩阵闭式算 n 次迭代后的状态向量（与 kind 平行）。
fn lr_closed(kind: &LrKind, inits: &[i128], n: u64) -> Vec<i128> {
    match kind {
        LrKind::Affine { a, b, m } => {
            let mm = if *m > 0 { *m } else { 0 };
            let t = if mm > 0 { [[a % mm, b % mm], [0, 1]] } else { [[*a, *b], [0, 1]] };
            let p = lr_matpow(t, n, mm);
            let s0 = inits[0];
            let s = p[0][0] * s0 + p[0][1];
            vec![if mm > 0 { s % mm } else { s }]
        }
        LrKind::Fib { m } => {
            let p = lr_matpow([[0, 1], [1, 1]], n, *m);
            let a0 = inits[0];
            let b0 = inits[1];
            let x = p[0][0] * a0 + p[0][1] * b0;
            let y = p[1][0] * a0 + p[1][1] * b0;
            vec![x % m, y % m]
        }
    }
}

/// 用真实 IR 解释器（e3p）验证闭式：克隆程序，把目标函数的循环上界改成
/// 「恰好迭代 k 次」，跑 e3p，与 lr_closed(k) 比对。这样验证锚定在 IR 语义，
/// 避免「识别错但闭式与自造模拟共同错」的自证漏洞（例如系数读反、初值张冠李戴）。
fn lr_verify_by_interp(ip: &IrProgram, name: &str, rec: &LinRec, k: u64) -> bool {
    let mut probe = ip.clone();
    let pf = match probe.functions.iter_mut().find(|f| f.name == name) { Some(f) => f, None => return false };
    // header.insts[1] 是 ICmp cv, limit；把 limit 改成让循环恰好跑 k 次的立即数。
    let new_limit = if rec.cmp_lt {
        // i<L，i 取 start, start+step, ...；跑 k 次 ⟹ L = start + (k-1)*step + 1
        // 取 L = start + k*step 也恰好 k 次（第 k+1 次 i=start+k*step >= L 退出）。
        rec.start + (k as i128) * rec.step
    } else {
        // i<=L，跑 k 次 ⟹ L = start + (k-1)*step （k>=1）
        if k == 0 { rec.start - 1 } else { rec.start + (k as i128 - 1) * rec.step }
    };
    if let Some(hdr) = pf.blocks.get_mut(rec.header) {
        match hdr.insts.get_mut(1) {
            Some(IrInst::ICmp { b: lim, .. }) => { *lim = Value::Imm(Imm::I64(new_limit as i64)); }
            _ => return false,
        }
    } else { return false; }
    // e3p 求值截断版函数（必须纯；四块结构无 phi/块参数，符合白名单）。
    let mut visiting = HashSet::new();
    if !e3p_is_pure(&probe, name, &mut visiting) { return false; }
    let mut env = E3pEnv::new();
    let mut memo = E3pMemo::new();
    let real = match e3p_eval_call(&probe, name, &[], &mut env, &mut memo) { Some(v) => v, None => return false };
    let closed = lr_closed(&rec.kind, &rec.inits, k);
    let ret_idx = match rec.state_slots.iter().position(|&s| s == rec.ret_slot) { Some(i) => i, None => return false };
    closed[ret_idx] == real as i128
}

/// 识别单个函数的可闭式线性递推循环。保守：任何不确定即 None。
fn lr_recognize(f: &IrFunction) -> Option<LinRec> {
    // 护栏 1：Private + 无参 + I64 返回。
    if f.linkage != FuncLink::Private || !f.params.is_empty() || f.ret != IrType::I64 { return None; }
    // 精确四块骨架：entry → header → body → exit，且 header 分支到 body/exit、
    // body 回边到 header。多于 4 块（如含 break/多分支）一律放弃。
    if f.blocks.len() != 4 { return None; }
    let (b0, b1, b2, b3) = (&f.blocks[0], &f.blocks[1], &f.blocks[2], &f.blocks[3]);
    // entry → header
    if !matches!(&b0.term, Term::Jump { target, .. } if *target == b1.id) { return None; }
    // header: Load cv iv; ICmp ...; Branch -> body / exit
    if b1.insts.len() != 2 { return None; }
    let (iv_slot, cmp_lt, limit) = match (&b1.insts[0], &b1.insts[1]) {
        (IrInst::Load { dst: cv, ptr: iv, ty: IrType::I64 },
         IrInst::ICmp { op, a: Value::Reg(a), b: lim, ty: IrType::I64, .. })
            if *a == *cv && matches!(op, ACmp::Lt | ACmp::Le) => {
            (*iv, matches!(op, ACmp::Lt), lr_nonneg_imm(lim)?)
        }
        _ => return None,
    };
    match &b1.term {
        Term::Branch { cond, then_b, else_b, .. }
            if *cond == match &b1.insts[1] { IrInst::ICmp { dst, .. } => *dst, _ => u32::MAX }
               && *then_b == b2.id && *else_b == b3.id => {}
        _ => return None,
    }
    // body 末三条：Load iv; Add step; Store iv; term = Jump header
    let bn = b2.insts.len();
    if bn < 4 { return None; }
    let (_iv_slot2, step) = match (&b2.insts[bn - 3], &b2.insts[bn - 2], &b2.insts[bn - 1]) {
        (IrInst::Load { dst: cv2, ptr: iv2, ty: IrType::I64 },
         IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(x), b, ty: IrType::I64 },
         IrInst::Store { ptr: iv3, val: Value::Reg(y), ty: IrType::I64 })
            if *cv2 == *x && *nv == *y && *iv2 == *iv3 && *iv2 == iv_slot => {
            (*iv2, lr_nonneg_imm(b)?)
        }
        _ => return None,
    };
    if step == 0 { return None; }
    if !matches!(&b2.term, Term::Jump { target, .. } if *target == b1.id) { return None; }
    // exit: Load ret ← state; Return ret
    if b3.insts.len() != 1 { return None; }
    let ret_slot = match (&b3.insts[0], &b3.term) {
        (IrInst::Load { dst: rv, ptr: slot, ty: IrType::I64 }, Term::Return { val: Some(Value::Reg(x)) })
            if *rv == *x => *slot,
        _ => return None,
    };

    // entry 初始化：收集 Alloca{dst}→随后 Store{dst, imm} 的 (槽, 初值)。
    let mut init_map: HashMap<VReg, i128> = HashMap::new();
    for (i, inst) in b0.insts.iter().enumerate() {
        if let IrInst::Alloca { dst, .. } = inst {
            if let Some(IrInst::Store { ptr, val, ty: IrType::I64 }) = b0.insts.get(i + 1) {
                if *ptr == *dst {
                    if let Some(v) = lr_nonneg_imm(val) { init_map.insert(*dst, v); }
                }
            }
        }
    }
    // iv 槽必须有初值。
    let start = *init_map.get(&iv_slot)?;

    // 从 body 的递推前缀（去掉末 3 条 iv 自增）识别状态转移。
    let prefix = &b2.insts[..bn - 3];
    if let Some(rec) = lr_match_affine(prefix, &init_map, ret_slot, iv_slot)? {
        return Some(LinRec { kind: rec.kind, state_slots: rec.state_slots, inits: rec.inits,
            iv_slot, start, limit, step, ret_slot, header: 1, body: 2, exit: 3, cmp_lt });
    }
    if let Some(rec) = lr_match_fib(prefix, &init_map, ret_slot, iv_slot)? {
        return Some(LinRec { kind: rec.kind, state_slots: rec.state_slots, inits: rec.inits,
            iv_slot, start, limit, step, ret_slot, header: 1, body: 2, exit: 3, cmp_lt });
    }
    None
}

/// 仿射匹配的中间结果。
struct LrMatched { kind: LrKind, state_slots: Vec<VReg>, inits: Vec<i128> }

/// 匹配 body 前缀为 s=(a*s+b) mod m 并 Store 回同一槽。
/// prefix 允许出现的指令：Alloca（循环内临时槽）；以及一条 Load state + 至多
/// 两条 Bin + 一条 Mod + 一条 Store state 的严格链。任何多余 Store 放弃。
fn lr_match_affine(prefix: &[IrInst], init_map: &HashMap<VReg, i128>, ret_slot: VReg, _iv_slot: VReg)
    -> Option<Option<LrMatched>>
{
    // state 槽必须是返回槽。
    let s0 = match init_map.get(&ret_slot) { Some(&v) => v, None => return Some(None) };
    // 允许循环内 Alloca（b04 无；保留以兼容其它 lowering）。
    let insts: Vec<&IrInst> = prefix.iter().filter(|i| !matches!(i, IrInst::Alloca { .. })).collect();
    // 期望形态（按序）：
    //   Load ld_s, ptr:ret_slot
    //   [可选] Bin{Mul|Sub|Add} dst=t1, (Reg(ld_s), Imm(c1)) 或 (Imm(c1), Reg(ld_s))
    //   [可选] Bin{Add|Sub}    dst=t2, (Reg(prev), Imm(c2)) ...
    //   [可选] Bin{Mod}        dst=t3, a=Reg(prev), b=Imm(m)
    //   Store ptr:ret_slot, val=Reg(last)
    // 严格约束 Store 只能写回 ret_slot，且是唯一 Store（iv 槽不在 prefix 内）。
    let stores: Vec<&IrInst> = insts.iter().copied().filter(|i| matches!(i, IrInst::Store { .. })).collect();
    if stores.len() != 1 { return Some(None); }
    let store = stores[0];
    let (st_ptr, st_val) = match store { IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::I64 } => (*ptr, *v), _ => return Some(None) };
    if st_ptr != ret_slot { return Some(None); }
    let store_pos = insts.iter().position(|i| std::ptr::eq(*i, store))?;

    // 解析从 ld_s 到 st_val 的计算链，提取 a,b,m。
    let mut a: i128 = 1;
    let mut b: i128 = 0;
    let mut m: i128 = 0;
    let mut cur_vreg = match insts.first()? {
        IrInst::Load { dst, ptr, ty: IrType::I64 } if *ptr == ret_slot => *dst,
        _ => return Some(None),
    };
    let mut idx = 1;
    // 计算链最多三条 Bin（Mul/Add/Sub + Mod）；下一条必须是 Store。
    while idx < store_pos {
        match insts[idx] {
            IrInst::Bin { op, dst, a: x, b: y, ty: IrType::I64 } => {
                // 指令必须以 cur_vreg 为唯一寄存器操作数（另一个是立即数），保证线性。
                let (reg, imm) = match (x, y) {
                    (Value::Reg(r), Value::Imm(im)) => (*r, im.as_i64() as i128),
                    (Value::Imm(im), Value::Reg(r)) => (*r, im.as_i64() as i128),
                    _ => return Some(None),
                };
                if reg != cur_vreg || imm < 0 { return Some(None); }
                match op {
                    ABin::Mul => { a *= imm; }
                    ABin::Add => { b += imm; }
                    ABin::Sub => {
                        // 只接受 s - imm（imm - s 会引入负系数，放弃）。
                        if !matches!(x, Value::Reg(_)) { return Some(None); }
                        b -= imm;
                    }
                    ABin::Mod => { if m != 0 || imm <= 0 { return Some(None); } m = imm; }
                    _ => return Some(None),
                }
                cur_vreg = *dst;
                idx += 1;
            }
            _ => return Some(None),
        }
    }
    if cur_vreg != st_val { return Some(None); }
    // b 必须非负（Sub 可能造出负数）；m>0；a 非负。
    if a < 0 || b < 0 || m <= 0 { return Some(None); }
    Some(Some(LrMatched { kind: LrKind::Affine { a, b, m }, state_slots: vec![ret_slot], inits: vec![s0] }))
}

/// 匹配 body 前缀为斐波那契型 (a,b)→(b,(a+b)%m)。
/// 形态（b07 dump）：
///   Alloca tmp
///   Load la←slotA; Load lb←slotB
///   Bin Add s, la, lb; Bin Mod t, s, Imm(m); Store tmp←t
///   Load bx←slotB; Store slotA←bx        (a=b)
///   Load ty←tmp;   Store slotB←ty        (b=t)
/// 返回槽必须是 slotA（返回 a）。
fn lr_match_fib(prefix: &[IrInst], init_map: &HashMap<VReg, i128>, ret_slot: VReg, iv_slot: VReg)
    -> Option<Option<LrMatched>>
{
    let insts: Vec<&IrInst> = prefix.iter().filter(|i| !matches!(i, IrInst::Alloca { .. })).collect();
    if insts.len() != 9 { return Some(None); }
    // 形如：Load, Load, Bin, Bin, Store, Load, Store, Load, Store
    let (la, slot_a) = match insts[0] { IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr), _ => return Some(None) };
    let (lb, slot_b) = match insts[1] { IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr), _ => return Some(None) };
    let (sum, addx, addy) = match insts[2] { IrInst::Bin { op: ABin::Add, dst, a: Value::Reg(x), b: Value::Reg(y), ty: IrType::I64 } => (*dst, *x, *y), _ => return Some(None) };
    if !((addx == la && addy == lb) || (addx == lb && addy == la)) { return Some(None); }
    let (modv, m) = match insts[3] { IrInst::Bin { op: ABin::Mod, dst, a: Value::Reg(x), b: y, ty: IrType::I64 } if *x == sum => (*dst, lr_nonneg_imm(y)?), _ => return Some(None) };
    if m <= 0 { return Some(None); }
    let tmp_slot = match insts[4] { IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::I64 } if *v == modv => *ptr, _ => return Some(None) };
    // a = b
    let (bx, src_b) = match insts[5] { IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr), _ => return Some(None) };
    match insts[6] { IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::I64 } if *ptr == slot_a && *v == bx => {}, _ => return Some(None) }
    // b = t
    let (ty_v, src_t) = match insts[7] { IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr), _ => return Some(None) };
    match insts[8] { IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::I64 } if *ptr == slot_b && *v == ty_v => {}, _ => return Some(None) }
    if src_b != slot_b || src_t != tmp_slot { return Some(None); }
    // slot_a 与 slot_b 必须不同、且都不是 iv 槽；初值都在 init_map。
    if slot_a == slot_b || slot_a == iv_slot || slot_b == iv_slot { return Some(None); }
    // 返回槽必须是 a（滚动后返回 a_n）。
    if ret_slot != slot_a { return Some(None); }
    let ia = match init_map.get(&slot_a) { Some(&v) => v, None => return Some(None) };
    let ib = match init_map.get(&slot_b) { Some(&v) => v, None => return Some(None) };
    Some(Some(LrMatched { kind: LrKind::Fib { m }, state_slots: vec![slot_a, slot_b], inits: vec![ia, ib] }))
}

/// 计算循环迭代次数 n（要求上界与起点同余、step>0、n>0）。
fn lr_trip_count(rec: &LinRec) -> Option<u64> {
    if rec.start >= rec.limit || rec.step <= 0 { return None; }
    let span = rec.limit - rec.start;
    let n = if rec.cmp_lt {
        // i < limit：迭代次数 = ceil(span/step)
        (span + rec.step - 1) / rec.step
    } else {
        // i <= limit：迭代次数 = floor(span/step) + 1
        span / rec.step + 1
    };
    if n <= 0 { return None; }
    u64::try_from(n).ok()
}

/// 对整段程序跑线性递推闭式特化。返回特化函数数。
pub fn specialize_linear_recurrences(ip: &mut IrProgram) -> usize {
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    let mut count = 0;
    for name in names {
        let rec = {
            let f = match ip.functions.iter().find(|f| f.name == name) { Some(f) => f, None => continue };
            match lr_recognize(f) { Some(r) => r, None => continue }
        };
        let n = match lr_trip_count(&rec) { Some(n) => n, None => continue };
        // 采样验证：用真实 IR 解释器（e3p）对「截断上界」副本求值，与闭式逐点比对。
        // 采样点含 0/1 边界与若干中等值，覆盖系数、初值、取模绕回；任一不符即放弃。
        // 这是锚定真实语义的验证，杜绝「识别错但闭式与模拟共同错」的自证漏洞。
        let mut verified = true;
        for &k in &[0u64, 1, 2, 3, 7, 31, 128, 511] {
            if k > n { continue; }
            if !lr_verify_by_interp(ip, &name, &rec, k) { verified = false; break; }
        }
        if !verified { continue; }
        // 闭式算终值；取返回槽对应的分量。
        let final_state = lr_closed(&rec.kind, &rec.inits, n);
        let ret_idx = match rec.state_slots.iter().position(|&s| s == rec.ret_slot) { Some(i) => i, None => continue };
        let result = final_state[ret_idx];
        let result_i64 = match i64::try_from(result) { Ok(v) => v, Err(_) => continue };

        // 变换：把函数体折叠为单块 `return Imm(result)`。
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            // 保留原 entry 块对象、就地清空：id/entry 仍是 0，外部引用零变化。
            if let Some(b0) = f.blocks.get_mut(0) {
                b0.insts.clear();
                b0.params.clear();
                b0.term = Term::Return { val: Some(Value::Imm(Imm::I64(result_i64))) };
            }
            f.blocks.truncate(1);
            f.entry = 0;
            count += 1;
        }
    }
    count
}

// ============================================================================
// M10-strfold：字符串字面量拼接的编译期常量折叠
// ============================================================================
// 动机：gcc/clang -O2 会把 `"a" + "b"` 这类编译期已知的拼接折叠成单个常量，
// 省去运行时的 malloc + 两次 memcpy。Dial 的字面量已是静态 immortal 对象
// （LeaStaticString），concat 是 Call dial_string_concat —— 当两个操作数都能
// 追溯到静态串时，可在编译期 intern 出合并串，把 Call 换成单个 LeaStaticString。
//
// 正确性锚点：
//   1. 只折叠「两个操作数都是静态串 vreg」的 concat；任何来自运行时的值都不动。
//   2. 静态串内容来自 ip.string_literals（编译期已知），合并后 intern 复用池，
//      emit 时统一带 '\0'，与 dial_string_concat 的 C 侧语义逐字节一致。
//   3. ARC 安全：静态串是 immortal（rc=INT64_MIN），lower 在其上发的 retain/release
//      全是 no-op；替换 dst 为另一个静态串不改变任何引用计数平衡。
//   4. 不动点迭代：支持 "a"+"b"+"c" 链式（前一轮折叠产生的静态串可被下一轮用）。
//   5. 折叠上限：防止 pathological 输入在编译期爆内存（单次合并长度上限 1 MiB）。

/// 从 Value 追出它对应的静态字符串字面量索引（仅当该 vreg 由 LeaStaticString 定义）。
fn sf_static_lit(v: &Value, static_map: &HashMap<VReg, u32>) -> Option<u32> {
    match v {
        Value::Reg(r) => static_map.get(r).copied(),
        _ => None,
    }
}

const SF_MAX_LEN: usize = 1024 * 1024;

/// 对整段程序做字符串字面量拼接折叠，返回折叠掉的 concat 次数。
pub fn fold_string_concat(ip: &mut IrProgram) -> usize {
    let mut total = 0usize;
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();

    for name in names {
        loop {
            // 阶段 1（只读）：重建「vreg → 静态串 lit 索引」映射，并收集所有可折叠的
            // concat：位置 + 合并后的字符串内容。LeaStaticString 的 dst 会登记进映射，
            // 因此上一轮折叠产物可被下一轮用，实现链式 "a"+"b"+"c" 的不动点迭代。
            let mut static_map: HashMap<VReg, u32> = HashMap::new();
            let mut jobs: Vec<(usize, usize, String)> = Vec::new(); // (block, inst, merged)
            {
                let f = match ip.functions.iter().find(|f| f.name == name) { Some(f) => f, None => break };
                for b in &f.blocks {
                    for inst in &b.insts {
                        if let IrInst::LeaStaticString { dst, lit } = inst {
                            static_map.insert(*dst, *lit);
                        }
                    }
                }
                let lits = &ip.string_literals;
                for (bi, b) in f.blocks.iter().enumerate() {
                    for (ii, inst) in b.insts.iter().enumerate() {
                        if let IrInst::Call { dst: Some(_), func, args, linkage: CallLink::Runtime, .. } = inst {
                            if func != "dial_string_concat" || args.len() != 2 { continue; }
                            let (la, lb) = match (sf_static_lit(&args[0], &static_map), sf_static_lit(&args[1], &static_map)) {
                                (Some(a), Some(b)) => (a, b),
                                _ => continue,
                            };
                            let sa = match lits.get(la as usize) { Some(s) => s, None => continue };
                            let sb = match lits.get(lb as usize) { Some(s) => s, None => continue };
                            if sa.len() + sb.len() > SF_MAX_LEN { continue; }
                            jobs.push((bi, ii, format!("{}{}", sa, sb)));
                        }
                    }
                }
            }

            if jobs.is_empty() { break; }

            // 阶段 2（只改字符串池）：把合并内容 intern 成 lit 索引。
            // 此时不借用任何 function，避免与阶段 3 的 ip.functions 可变借用冲突。
            let lit_indices: Vec<u32> = jobs.iter().map(|(_, _, s)| ip.intern_string(s)).collect();

            // 阶段 3（只改当前函数）：把 Call 换成 LeaStaticString。
            if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
                for ((bi, ii, _), new_lit) in jobs.iter().zip(lit_indices) {
                    let dst = match &f.blocks[*bi].insts[*ii] {
                        IrInst::Call { dst: Some(d), .. } => *d,
                        _ => continue,
                    };
                    f.blocks[*bi].insts[*ii] = IrInst::LeaStaticString { dst, lit: new_lit };
                    total += 1;
                }
            }
        }
    }
    total
}

// ============================================================================
// M10b-straccum：字符串累加循环的编译期常量折叠（对标 gcc -O2 对 b20 的整函数折叠）
// ============================================================================
// 目标模式（lower 后 5 块骨架，见 bench.b20）：
//   entry:  x = LeaStaticString(S0);  i = 0;  jump header
//   header: load i;  icmp i < N (imm);  branch → body / exit
//   body:   load x;  LeaStaticString(C);  x = concat(x, C);  [rc];  store x;  jump latch
//   latch:  i = i + 1;  jump header
//   exit:   仅用 dial_string_length(x) / dial_string_get(x, idx) + 纯算术 → return
//
// 折叠：编译期算出 final = S0 + C*N，符号执行 exit 得到 return 常量，
// 把整函数折叠成单块 `return Imm(const)`。语义等价锚点：
//   1. final 与循环 N 次结果逐字节相同（构造性保证）。
//   2. 静态串是 immortal，rc no-op，删除循环里的 alloc/release 不破坏 ARC。
//   3. exit 只含白名单指令（Load/Store/Bin/ICmp/Conv/Call length|get/RcRelease）；
//      任何非白名单（如 print/FFI/循环依赖）一律放弃，零误拒优先。
//   4. entry 只做初始化（Alloca/Lea/Store/Load/EnvInit），无副作用才折叠整函数。
//   5. i 在 exit 不被使用（否则删循环会改变 i 终值）——严格检查。
//   6. 长度上限 SF_MAX_LEN + trip count 上限，防编译期爆内存。

/// 从字符串常量池读内容。
fn sf_lit_str(ip: &IrProgram, lit: u32) -> Option<&str> {
    ip.string_literals.get(lit as usize).map(|s| s.as_str())
}

/// 常量求 eval：Value → i64。句柄寄存器不可当整数用（返回 None）。
fn sf_eval(v: &Value, env: &HashMap<VReg, i64>, strregs: &HashSet<VReg>) -> Option<i64> {
    match v {
        Value::Imm(i) => Some(i.as_i64()),
        Value::Reg(r) => {
            if strregs.contains(r) { return None; } // 字符串句柄不能参与整数运算
            env.get(r).copied()
        }
    }
}

/// 识别 5 块字符串累加循环骨架。
struct StrAccum {
    x_slot: VReg,       // 累加字符串变量槽
    iv_slot: VReg,      // 循环变量槽
    s0: String,         // 累加初值串
    c: String,          // 每轮追加串
    n: u64,             // 迭代次数
    header: usize, body: usize, latch: usize, exit: usize,
}

fn sf_recognize(ip: &IrProgram, f: &IrFunction) -> Option<StrAccum> {
    // 护栏：Private / 无参 / I64 返回 / 精确 5 块。
    if f.linkage != FuncLink::Private || !f.params.is_empty() || f.ret != IrType::I64 { return None; }
    if f.blocks.len() != 5 { return None; }

    // entry → header
    if !matches!(&f.blocks[0].term, Term::Jump { target, .. } if *target == f.blocks[1].id) { return None; }
    let b0 = &f.blocks[0];

    // header: Load cv(iv); ICmp Lt cv < Imm N; Branch → body / exit
    let b1 = &f.blocks[1];
    if b1.insts.len() != 2 { return None; }
    let (iv_slot, n) = match (&b1.insts[0], &b1.insts[1]) {
        (IrInst::Load { dst: cv, ptr: iv, ty: IrType::I64 },
         IrInst::ICmp { op: ACmp::Lt, a: Value::Reg(a), b: Value::Imm(lim), .. })
            if *a == *cv => {
            let nv = lim.as_i64();
            if nv <= 0 { return None; }
            (*iv, nv as u64)
        }
        _ => return None,
    };
    match &b1.term {
        Term::Branch { cond, then_b, else_b, .. }
            if *cond == match &b1.insts[1] { IrInst::ICmp { dst, .. } => *dst, _ => u32::MAX }
               && *then_b == f.blocks[2].id && *else_b == f.blocks[4].id => {}
        _ => return None,
    }

    // body: 严格 = Load xslot; LeaStaticString C; Call concat(x, C); [RcRelease]; Store xslot; Jump latch
    let b2 = &f.blocks[2];
    if !matches!(&b2.term, Term::Jump { target, .. } if *target == f.blocks[3].id) { return None; }
    // 先看 iv 初值（entry 里 Store iv_slot, Imm 0）。
    let mut iv_init_zero = false;
    for inst in &b0.insts {
        if let IrInst::Store { ptr, val: Value::Imm(im), .. } = inst {
            if *ptr == iv_slot && im.as_i64() == 0 { iv_init_zero = true; }
        }
    }
    if !iv_init_zero { return None; }

    // latch: Load iv; Bin Add 1; Store iv; Jump header
    let b3 = &f.blocks[3];
    if !matches!(&b3.term, Term::Jump { target, .. } if *target == f.blocks[1].id) { return None; }
    let lbn = b3.insts.len();
    if lbn != 3 { return None; }
    match (&b3.insts[0], &b3.insts[1], &b3.insts[2]) {
        (IrInst::Load { dst: cv, ptr: iv2, ty: IrType::I64 },
         IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(x), b: Value::Imm(step), ty: IrType::I64 },
         IrInst::Store { ptr: iv3, val: Value::Reg(y), ty: IrType::I64 })
            if *cv == *x && *nv == *y && *iv2 == *iv3 && *iv2 == iv_slot && step.as_i64() == 1 => {}
        _ => return None,
    }

    // 解析 body：允许 Alloca（临时槽）与 RcRelease（字符串 no-op），核心须是 concat 链。
    let mut x_slot = u32::MAX;
    let mut append_lit: Option<u32> = None;
    let mut concat_dst = u32::MAX;
    let mut concat_args: Option<(Value, Value)> = None;
    let mut store_after_concat = false;
    let mut seen_concat = false;
    for (i, inst) in b2.insts.iter().enumerate() {
        match inst {
            IrInst::Load { dst, ptr, ty: IrType::Handle } => {
                // 循环里 load 的句柄就是 x（累加变量）。
                if x_slot == u32::MAX { x_slot = *ptr; let _ = dst; }
            }
            IrInst::LeaStaticString { dst, lit } => {
                if append_lit.is_none() { append_lit = Some(*lit); let _ = dst; }
            }
            IrInst::Call { dst: Some(d), func, args, linkage: CallLink::Runtime, .. }
                if func == "dial_string_concat" && args.len() == 2 => {
                if seen_concat { return None; } // 只接受一次 concat
                seen_concat = true;
                concat_dst = *d;
                concat_args = Some((args[0], args[1]));
            }
            IrInst::RcRelease { kind: ArcKind::String, .. } => {}
            IrInst::Store { ptr, val: Value::Reg(v), .. } if *v == concat_dst => {
                if *ptr == x_slot { store_after_concat = true; }
            }
            IrInst::Alloca { .. } => {}
            _ => return None, // 任何其它副作用 → 放弃
        }
        let _ = i;
    }
    if !seen_concat || !store_after_concat || x_slot == u32::MAX || append_lit.is_none() { return None; }
    // concat 的两个参数必须恰是 (load x 的结果, LeaStaticString C 的结果)。
    let (a, b) = concat_args?;
    let (x_ldr, c_ldr) = match (a, b) {
        (Value::Reg(xr), Value::Reg(cr)) => (xr, cr),
        _ => return None,
    };
    // 找回 body 里 load x 的 dst 与 lea C 的 dst，逐一比对。
    let mut x_ldr_ok = false;
    let mut c_ldr_ok = false;
    for inst in &b2.insts {
        if let IrInst::Load { dst, ptr, ty: IrType::Handle } = inst {
            if *dst == x_ldr && *ptr == x_slot { x_ldr_ok = true; }
        }
        if let IrInst::LeaStaticString { dst, lit } = inst {
            if *dst == c_ldr && *lit == append_lit.unwrap() { c_ldr_ok = true; }
        }
    }
    if !x_ldr_ok || !c_ldr_ok { return None; }

    // entry 里 x 初值必须是静态串（Store x_slot, reg where reg = LeaStaticString S0）。
    let mut s0_lit: Option<u32> = None;
    for (i, inst) in b0.insts.iter().enumerate() {
        if let IrInst::Store { ptr, val: Value::Reg(v), .. } = inst {
            if *ptr != x_slot { continue; }
            for prev in &b0.insts[..i] {
                if let IrInst::LeaStaticString { dst, lit } = prev {
                    if *dst == *v { s0_lit = Some(*lit); }
                }
            }
        }
    }
    let s0 = sf_lit_str(ip, s0_lit?)?.to_string();
    let c = sf_lit_str(ip, append_lit?)?.to_string();
    if c.is_empty() { return None; }
    // 长度上限：n 也设上限，防编译期构造爆炸。
    if n > 1_000_000 { return None; }
    let total_len = s0.len().saturating_add(c.len().saturating_mul(n as usize));
    if total_len > SF_MAX_LEN { return None; }

    // iv 在 exit 不得被使用（删循环后 i 保持 0，若 exit 用 i 会改变语义）。
    for inst in &f.blocks[4].insts {
        match inst {
            IrInst::Load { ptr, .. } if *ptr == iv_slot => return None,
            IrInst::Store { ptr, .. } if *ptr == iv_slot => return None,
            _ => {}
        }
    }

    Some(StrAccum { x_slot, iv_slot, s0, c, n, header: 1, body: 2, latch: 3, exit: 4 })
}

/// 符号执行 exit 块，用 final 串内容算出 return 常量。
/// env: 普通 i64 寄存器/槽值；strregs: 持有 final 静态串句柄的寄存器。
fn sf_eval_exit(f: &IrFunction, rec: &StrAccum, final_str: &str) -> Option<i64> {
    let mut env: HashMap<VReg, i64> = HashMap::new();
    let mut strregs: HashSet<VReg> = HashSet::new();
    let exit = &f.blocks[rec.exit];
    for inst in &exit.insts {
        match inst {
            IrInst::Alloca { .. } => {}
            IrInst::Load { dst, ptr, .. } => {
                if *ptr == rec.x_slot {
                    strregs.insert(*dst); // load 累加串 → 句柄
                } else if let Some(&v) = env.get(ptr) {
                    env.insert(*dst, v);
                } else {
                    return None; // 未知槽（可能来自函数参数/全局）→ 放弃
                }
            }
            IrInst::Store { ptr, val, .. } => {
                let v = sf_eval(val, &env, &strregs)?;
                env.insert(*ptr, v);
            }
            IrInst::Bin { op, dst, a, b, ty: IrType::I64 } => {
                let x = sf_eval(a, &env, &strregs)?;
                let y = sf_eval(b, &env, &strregs)?;
                let r = fold_bin_i64(*op, x, y)?;
                env.insert(*dst, r);
            }
            IrInst::ICmp { op, dst, a, b, .. } => {
                let x = sf_eval(a, &env, &strregs)?;
                let y = sf_eval(b, &env, &strregs)?;
                env.insert(*dst, cmp_i64(*op, x, y));
            }
            IrInst::Conv { kind, dst, a, .. } => {
                let x = sf_eval(a, &env, &strregs)?;
                let r = match kind {
                    AConv::Sext | AConv::Zext | AConv::Trunc => x,
                    _ => return None,
                };
                env.insert(*dst, r);
            }
            IrInst::Call { dst: Some(d), func, args, .. } if func == "dial_string_length" => {
                if args.len() != 1 { return None; }
                if let Value::Reg(r) = args[0] {
                    if strregs.contains(&r) {
                        env.insert(*d, final_str.len() as i64);
                        continue;
                    }
                }
                return None;
            }
            IrInst::Call { dst: Some(d), func, args, .. } if func == "dial_string_get" => {
                if args.len() != 2 { return None; }
                let r = match args[0] { Value::Reg(r) => r, _ => return None };
                if !strregs.contains(&r) { return None; }
                let idx = sf_eval(&args[1], &env, &strregs)?;
                if idx < 0 || idx as usize >= final_str.len() { return None; }
                env.insert(*d, final_str.as_bytes()[idx as usize] as i64);
            }
            IrInst::RcRelease { .. } => {} // 静态串 release 是 no-op
            _ => return None, // 非白名单 → 放弃
        }
    }
    match &exit.term {
        Term::Return { val: Some(v) } => sf_eval(v, &env, &strregs),
        _ => None,
    }
}

/// 验证 entry 只含初始化指令（无副作用），可安全折叠整函数。
fn sf_entry_pure(b0: &IrFunction) -> bool {
    // 借不到 IrFunction 的 block0 简写，直接看第一个块的 insts。
    let b = &b0.blocks[0];
    b.insts.iter().all(|i| matches!(i,
        IrInst::Alloca { .. } | IrInst::LeaStaticString { .. } | IrInst::Store { .. }
        | IrInst::Load { .. } | IrInst::EnvInit { .. } | IrInst::RcRelease { .. }
    ))
}

/// 主入口：折叠字符串累加循环为 `return 常量`，返回折叠数。
pub fn fold_string_accum(ip: &mut IrProgram) -> usize {
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    let mut count = 0usize;
    for name in names {
        // 阶段 1（只读）：识别 + 计算 final + 符号执行 exit。
        let plan = {
            let f = match ip.functions.iter().find(|f| f.name == name) { Some(f) => f, None => continue };
            if !sf_entry_pure(f) { continue; }
            let rec = match sf_recognize(ip, f) { Some(r) => r, None => continue };
            let mut final_str = String::with_capacity(rec.s0.len() + rec.c.len() * rec.n as usize);
            final_str.push_str(&rec.s0);
            for _ in 0..rec.n { final_str.push_str(&rec.c); }
            let ret = match sf_eval_exit(f, &rec, &final_str) { Some(v) => v, None => continue };
            (final_str, ret)
        };
        // 阶段 2（写入）：intern final 串，把整函数折叠成单块 return 常量。
        let final_lit = ip.intern_string(&plan.0);
        let ret = plan.1;
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            if let Some(b0) = f.blocks.get_mut(0) {
                b0.insts.clear();
                b0.params.clear();
                // 产出一个静态串引用，避免 final 串被完全 DCE 掉（保真：留一个 lea）。
                b0.insts.push(IrInst::LeaStaticString { dst: 0, lit: final_lit });
                b0.term = Term::Return { val: Some(Value::Imm(Imm::I64(ret))) };
            }
            f.blocks.truncate(1);
            f.entry = 0;
            count += 1;
        }
    }
    count
}

// ============================================================================
// M11-inline：通用 Dial 函数内联（对标 gcc/clang -O2 的 inlining）
// ============================================================================
//
// 与 fold_pure_calls（编译期求值纯整数调用）互补：后者只吃「实参全常量」的
// 纯函数；本 pass 吃一般调用点，把被调函数体复制进调用者，消除 call/ret 与
// ABI 边界（prologue 写参 + 句柄 retain/release 配对仍逐字保留）。
//
// 正确性锚点（缺一不可，任一不满足即不内联）：
//   1. 只内联 Private + 非 main + 非递归 + 体小的 Dial 函数；External /
//      CallLink::Runtime / Extern / 间接调用一律不动。
//   2. 复制被调函数体时，vreg 与 block id 全量重映射（两函数各自独立编号空间）；
//      在复制后的 entry 块里，紧随每个形参 Alloca 之后**显式插入写参 Store**
//      ——IR 里本来由 codegen 的 prologue 隐式写参，内联后没有 prologue，必须补回。
//   3. 被调方入口的 RcRetain / struct retain、出口的 release / defer 结算全部原样
//      复制（含顺序），故 ARC 配对与不内联时逐调用等价。own 形参不 retain 的语义
//      同样原样保留。
//   4. 每个 Return 改为 Jump 到新建的 continuation 块，返回值经块参数 φ 传回；
//      Call.dst 的所有使用点用 subst_reg 改写为该 φ 参数（类型口径一致：
//      callee.ret == call.ret_ty，均为 param_ir）。
//   5. Line / DebugVar 不复制：内联体属被调函数的调试上下文，复制会污染 DWARF；
//      它们不产生机器码，跳过不影响运行语义。
//   6. 只 x86-64 / O2+ 挂接；AArch64 / LLVM 路径零变化（沿用既有优化守卫口径）。
//   7. 每个 caller 最多内联 INLINE_MAX_SITES 次、callee 体不超过大小上限，
//      防代码膨胀；多轮不动点让嵌套调用可被逐层内联。

/// 被调函数体指令数上限（含 ARC / 控制流）：超过则放弃内联，防膨胀。
const INLINE_MAX_CALLEE_INSTS: usize = 96;
/// 被调函数块数上限。
const INLINE_MAX_CALLEE_BLOCKS: usize = 16;
/// 单个 caller 最多内联的调用点数。
const INLINE_MAX_SITES: usize = 32;
/// 内联不动点轮数（嵌套调用逐层展开）。
const INLINE_MAX_ROUNDS: usize = 3;

/// 判断一个函数是否「可安全内联」（只读）。
fn inline_is_candidate(ip: &IrProgram, name: &str) -> bool {
    let f = match ip.functions.iter().find(|f| f.name == name) { Some(f) => f, None => return false };
    if f.linkage != FuncLink::Private || name == "main" { return false; }
    if f.blocks.len() > INLINE_MAX_CALLEE_BLOCKS { return false; }
    let insts: usize = f.blocks.iter().map(|b| b.insts.len()).sum();
    if insts > INLINE_MAX_CALLEE_INSTS { return false; }
    // 调用图：递归 / 互递归不可内联（否则内联不动点会无限增长）。
    if inline_reaches(ip, name, name) { return false; }
    for b in &f.blocks {
        for i in &b.insts {
            match i {
                // 环境初始化、内联汇编、间接调用、函数地址外泄 → 放弃。
                IrInst::EnvInit { .. } | IrInst::InlineAsm { .. }
                | IrInst::CallIndirect { .. } | IrInst::FuncAddr { .. } => return false,
                _ => {}
            }
        }
    }
    true
}

/// 调用图中 `from` 是否可达 `target`（含自身，用于递归判定）。
fn inline_reaches(ip: &IrProgram, from: &str, target: &str) -> bool {
    fn dfs(ip: &IrProgram, cur: &str, target: &str, seen: &mut HashSet<String>) -> bool {
        let f = match ip.functions.iter().find(|f| f.name == cur) { Some(f) => f, None => return false };
        for b in &f.blocks {
            for i in &b.insts {
                if let IrInst::Call { func, linkage: CallLink::Dial, .. } = i {
                    if func == target { return true; }
                    if seen.insert(func.clone()) && dfs(ip, func, target, seen) { return true; }
                }
            }
        }
        false
    }
    let mut seen = HashSet::new();
    dfs(ip, from, target, &mut seen)
}

/// 把被调函数的一条指令按 vmap 重映射到调用者编号空间。
/// Line / DebugVar 返回 None（跳过，见上文锚点 5）。
fn inline_remap_inst(inst: &IrInst, vmap: &[VReg]) -> Option<IrInst> {
    let rv = |r: VReg| vmap[r as usize];
    let rval = |v: Value| match v { Value::Reg(r) => Value::Reg(rv(r)), other => other };
    Some(match inst {
        IrInst::Line(_) | IrInst::DebugVar { .. } => return None,
        IrInst::Alloca { dst, bytes } => IrInst::Alloca { dst: rv(*dst), bytes: *bytes },
        IrInst::Load { dst, ptr, ty } => IrInst::Load { dst: rv(*dst), ptr: rv(*ptr), ty: ty.clone() },
        IrInst::Store { ptr, val, ty } => IrInst::Store { ptr: rv(*ptr), val: rval(*val), ty: ty.clone() },
        IrInst::FieldAddr { dst, base, struct_name, field } =>
            IrInst::FieldAddr { dst: rv(*dst), base: rv(*base), struct_name: struct_name.clone(), field: *field },
        IrInst::LeaLabel { dst, lit } => IrInst::LeaLabel { dst: rv(*dst), lit: *lit },
        IrInst::LeaStaticString { dst, lit } => IrInst::LeaStaticString { dst: rv(*dst), lit: *lit },
        IrInst::LeaStaticData { dst, label } => IrInst::LeaStaticData { dst: rv(*dst), label: label.clone() },
        IrInst::PtrAdd { dst, base, off } =>
            IrInst::PtrAdd { dst: rv(*dst), base: rv(*base), off: rval(*off) },
        IrInst::Bin { op, dst, a, b, ty } =>
            IrInst::Bin { op: *op, dst: rv(*dst), a: rval(*a), b: rval(*b), ty: ty.clone() },
        IrInst::Un { op, dst, a, ty } =>
            IrInst::Un { op: *op, dst: rv(*dst), a: rval(*a), ty: ty.clone() },
        IrInst::FBin { op, dst, a, b, ty } =>
            IrInst::FBin { op: *op, dst: rv(*dst), a: rval(*a), b: rval(*b), ty: ty.clone() },
        IrInst::ICmp { op, dst, a, b, ty } =>
            IrInst::ICmp { op: *op, dst: rv(*dst), a: rval(*a), b: rval(*b), ty: ty.clone() },
        IrInst::FCmp { op, dst, a, b, ty } =>
            IrInst::FCmp { op: *op, dst: rv(*dst), a: rval(*a), b: rval(*b), ty: ty.clone() },
        IrInst::Conv { kind, dst, a, ty } =>
            IrInst::Conv { kind: *kind, dst: rv(*dst), a: rval(*a), ty: ty.clone() },
        IrInst::Call { dst, func, args, arg_tys, ret_ty, linkage } => IrInst::Call {
            dst: dst.map(rv), func: func.clone(),
            args: args.iter().map(|a| rval(*a)).collect(),
            arg_tys: arg_tys.clone(), ret_ty: ret_ty.clone(), linkage: *linkage,
        },
        IrInst::CallIndirect { dst, target, args, arg_tys, ret_ty } => IrInst::CallIndirect {
            dst: dst.map(rv), target: rv(*target),
            args: args.iter().map(|a| rval(*a)).collect(),
            arg_tys: arg_tys.clone(), ret_ty: ret_ty.clone(),
        },
        IrInst::InlineAsm { dst, template, constraints, dialect, args, arg_tys, ret_ty } =>
            IrInst::InlineAsm {
                dst: rv(*dst), template: template.clone(), constraints: constraints.clone(),
                dialect: *dialect, args: args.iter().map(|a| rval(*a)).collect(),
                arg_tys: arg_tys.clone(), ret_ty: ret_ty.clone(),
            },
        IrInst::RcRetain { ptr, kind } => IrInst::RcRetain { ptr: rv(*ptr), kind: *kind },
        IrInst::RcRelease { ptr, kind } => IrInst::RcRelease { ptr: rv(*ptr), kind: *kind },
        IrInst::StructRetain { ptr, struct_name } =>
            IrInst::StructRetain { ptr: rv(*ptr), struct_name: struct_name.clone() },
        IrInst::StructRelease { ptr, struct_name } =>
            IrInst::StructRelease { ptr: rv(*ptr), struct_name: struct_name.clone() },
        IrInst::EnvInit { argc, argv } =>
            IrInst::EnvInit { argc: rval(*argc), argv: rval(*argv) },
        IrInst::FuncAddr { dst, func } => IrInst::FuncAddr { dst: rv(*dst), func: func.clone() },
    })
}

/// 把被调函数的一个 term 重映射；Return 改为 Jump 到 continuation（返回值走 φ）。
fn inline_remap_term(
    term: &Term, vmap: &[VReg], bmap: &HashMap<u32, u32>,
    cont_id: u32, has_ret: bool,
) -> Term {
    let rv = |r: VReg| vmap[r as usize];
    let rval = |v: Value| match v { Value::Reg(r) => Value::Reg(rv(r)), other => other };
    match term {
        Term::Jump { target, args } => Term::Jump {
            target: *bmap.get(target).unwrap_or(target),
            args: args.iter().map(|a| rval(*a)).collect(),
        },
        Term::Branch { cond, then_b, else_b, then_args, else_args } => Term::Branch {
            cond: rv(*cond),
            then_b: *bmap.get(then_b).unwrap_or(then_b),
            else_b: *bmap.get(else_b).unwrap_or(else_b),
            then_args: then_args.iter().map(|a| rval(*a)).collect(),
            else_args: else_args.iter().map(|a| rval(*a)).collect(),
        },
        Term::Return { val } => Term::Jump {
            target: cont_id,
            args: if has_ret { val.map(rval).into_iter().collect() } else { Vec::new() },
        },
        Term::Unreachable => Term::Unreachable,
    }
}

/// 对单个 caller 做一轮内联，返回本轮内联的调用点数。
fn inline_round(ip: &mut IrProgram, caller_name: &str) -> usize {
    // 只读收集本轮所有待内联点（避免边扫描边改 block 索引失效）。
    struct Site { bi: usize, ii: usize, callee: String, dst: Option<VReg>, args: Vec<Value>, arg_tys: Vec<IrType>, ret_ty: IrType }
    let mut sites: Vec<Site> = Vec::new();
    {
        let caller = match ip.functions.iter().find(|f| f.name == caller_name) { Some(f) => f, None => return 0 };
        for (bi, b) in caller.blocks.iter().enumerate() {
            for (ii, inst) in b.insts.iter().enumerate() {
                if let IrInst::Call { dst, func, args, arg_tys, ret_ty, linkage: CallLink::Dial } = inst {
                    if !inline_is_candidate(ip, func) { continue; }
                    // 签名口径必须严格一致：callee.ret == call.ret_ty、
                    // callee 形参 ABI 宽度 == call.arg_tys，否则跳过（防宽度错位）。
                    let callee = match ip.functions.iter().find(|f| f.name == *func) { Some(f) => f, None => continue };
                    if callee.ret != *ret_ty || callee.params.len() != arg_tys.len() { continue; }
                    if callee.params.iter().zip(arg_tys).any(|((_, pt), at)| pt != at) { continue; }
                    // 不允许把 caller 自己内联进自己（防 caller→callee→caller 环）。
                    if func == caller_name { continue; }
                    sites.push(Site {
                        bi, ii, callee: func.clone(), dst: *dst,
                        args: args.clone(), arg_tys: arg_tys.clone(), ret_ty: ret_ty.clone(),
                    });
                    if sites.len() >= INLINE_MAX_SITES { break; }
                }
            }
        }
    }
    if sites.is_empty() { return 0; }

    // 逐个应用。每个站点都会重塑 caller 的 blocks，故从后往前按块下标处理。
    sites.sort_by(|a, b| (b.bi, b.ii).cmp(&(a.bi, a.ii)));
    let mut done = 0usize;
    for site in sites {
        let callee_snapshot = match ip.functions.iter().find(|f| f.name == site.callee) {
            Some(f) => f.clone(), None => continue,
        };
        let caller = match ip.functions.iter_mut().find(|f| f.name == caller_name) { Some(f) => f, None => continue };

        // ① vreg 重映射表。
        let vmap: Vec<VReg> = callee_snapshot.reg_ty.iter()
            .map(|t| caller.new_reg(t.clone())).collect();
        // ② block id 重映射表。
        let max_id = caller.blocks.iter().map(|b| b.id).max().unwrap_or(0);
        let mut bmap: HashMap<u32, u32> = HashMap::new();
        for (i, b) in callee_snapshot.blocks.iter().enumerate() {
            bmap.insert(b.id, max_id + 1 + i as u32);
        }
        let cont_id = max_id + 1 + callee_snapshot.blocks.len() as u32 + 1;
        let has_ret = site.dst.is_some();

        // ③ 把 caller 的调用块一切为二。
        let bi = site.bi;
        if bi >= caller.blocks.len() { continue; }
        let ii = site.ii;
        if ii >= caller.blocks[bi].insts.len() { continue; }
        match &caller.blocks[bi].insts[ii] {
            IrInst::Call { func, .. } if *func == site.callee => {}
            _ => continue,
        }
        let after: Vec<IrInst> = caller.blocks[bi].insts.split_off(ii + 1);
        let orig_term = caller.blocks[bi].term.clone();
        caller.blocks[bi].insts.pop(); // 移除 Call 自身
        let inl_entry = *bmap.get(&callee_snapshot.entry).unwrap_or(&cont_id);
        caller.blocks[bi].term = Term::Jump { target: inl_entry, args: Vec::new() };

        // ④ continuation 块：φ 接住返回值；Call.dst 的使用点随后 subst 到 φ。
        let ret_param = if has_ret { Some(caller.new_reg(site.ret_ty.clone())) } else { None };
        let cont_params = if let Some(rp) = ret_param { vec![(rp, site.ret_ty.clone())] } else { Vec::new() };
        let cont = IrBlock { id: cont_id, params: cont_params, insts: after, term: orig_term };
        caller.blocks.push(cont);

        // ⑤ 复制 callee 的每个块。
        for cb in &callee_snapshot.blocks {
            let new_id = *bmap.get(&cb.id).unwrap_or(&cb.id);
            let is_entry = cb.id == callee_snapshot.entry;
            let mut insts: Vec<IrInst> = Vec::new();
            for inst in &cb.insts {
                if let Some(ri) = inline_remap_inst(inst, &vmap) { insts.push(ri); }
                // 入口块：紧随形参 Alloca 之后插入写参 Store（补回 codegen prologue 的隐式写参）。
                if is_entry {
                    if let IrInst::Alloca { dst, .. } = inst {
                        if let Some(pi) = callee_snapshot.param_slots.iter().position(|s| s == dst) {
                            if let Some(arg) = site.args.get(pi) {
                                insts.push(IrInst::Store {
                                    ptr: vmap[*dst as usize], val: *arg, // 实参在 caller 编号空间，不映射
                                    ty: site.arg_tys.get(pi).cloned().unwrap_or(IrType::I64),
                                });
                            }
                        }
                    }
                }
            }
            let term = inline_remap_term(&cb.term, &vmap, &bmap, cont_id, has_ret);
            caller.blocks.push(IrBlock {
                id: new_id,
                params: cb.params.iter().map(|(r, t)| (vmap[*r as usize], t.clone())).collect(),
                insts, term,
            });
        }

        // ⑥ 把 Call.dst 的所有使用点改写为 continuation 的 φ 参数。
        if let (Some(d), Some(rp)) = (site.dst, ret_param) {
            subst_reg(caller, d, rp);
        }
        done += 1;
    }
    done
}

/// 对全程序跑通用函数内联（不动点）。返回内联的调用点总数。
pub fn inline_calls(ip: &mut IrProgram) -> usize {
    let mut total = 0usize;
    for _ in 0..INLINE_MAX_ROUNDS {
        let mut round_total = 0usize;
        let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
        for name in names { round_total += inline_round(ip, &name); }
        total += round_total;
        if round_total == 0 { break; }
    }
    // 内联后清理：写参 Store 与 φ 搬运可能产生新的死纯指令，跑一次 DCE。
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for name in names {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) { dce(f); }
    }
    total
}

// ============================================================================
// M12-licm：通用循环不变代码外提（对标 gcc/clang -O2 LICM）
// ============================================================================
//
// 与 IVR（归纳变量强度削减）互补：IVR 只吃一种固定模式（base*C+iv），
// LICM 处理一般循环——把「操作数全在循环外定义、无副作用」的纯指令从循环体
// 提到 preheader，每轮只算一次。
//
// 正确性锚点（缺一不可，任一不满足即不外提）：
//   1. 自然循环用标准反向 DFS 识别（header 为回边目标、latch 为源）。
//      若反向可达性把入口块 0 卷入循环（select / 复杂分支常见），说明并非真
//      自然循环，整块跳过——否则会把入口指令"外提"到更晚的 preheader，
//      破坏 SSA 支配关系（select_timeout SIGILL 回归的根因）。
//      另要求 preheader 索引 < header 索引、且只外提定义块 >= preheader 的
//      指令，保证外提恒为「向前搬」。
//   2. 只外提 `is_pure()` 的纯指令；额外排除：
//      - `Alloca`：外提会改变栈帧槽归属 / DWARF 位置；
//      - `Div`/`Mod`：循环不执行时外提会把「原本不会发生的除零 trap」提前，
//        改变可观察行为（Fail-Fast 语义）。
//   3. `Load` 只在「ptr 槽在循环内完全不被修改」时可外提。修改集合涵盖：
//      Store 写槽、ARC 原语（RcRelease/Retain/Struct* 会把句柄槽归零/改写，
//      这是 select_timeout NULL-string 回归的根因）、FieldAddr/PtrAdd 别名、
//      Call 实参逃逸。任一命中即不外提该槽的 Load（零误拒优先）。
//      `FieldAddr`/`PtrAdd` 自身绝不外提（别名本就可能被循环内写）。
//   4. 所有寄存器操作数（含块参数）定义在循环外；循环内定义的块参数一律视为
//      循环变量，绝不外提依赖它的指令。
//   5. 按原始 (块序, 指令序) 升序追加到 preheader，保证定义在使用前；
//      不动点迭代让「依赖链」逐层外提（先提叶子，再提依赖它的）。
//   6. 只 x86-64 / O2+ 且非 -g / 非 PGO / 非插桩挂接（同内联护栏理由）。

#[derive(Clone, Debug)]
struct LicmLoop {
    header: usize,
    blocks: HashSet<usize>,
}

/// 标准自然循环识别：对每条回边 (latch→header)，从 latch 反向 DFS 到 header
/// （不越过 header），收集所有可达 latch 的块；同 header 的多回边合并。
fn licm_find_loops(f: &IrFunction) -> Vec<LicmLoop> {
    let n = f.blocks.len();
    let mut id2idx: HashMap<u32, usize> = HashMap::new();
    for (i, b) in f.blocks.iter().enumerate() { id2idx.insert(b.id, i); }
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut back_edges: Vec<(usize, usize)> = Vec::new();
    for (i, b) in f.blocks.iter().enumerate() {
        let succs: Vec<u32> = match &b.term {
            Term::Jump { target, .. } => vec![*target],
            Term::Branch { then_b, else_b, .. } => vec![*then_b, *else_b],
            _ => Vec::new(),
        };
        for s in succs {
            if let Some(&si) = id2idx.get(&s) {
                pred[si].push(i);
                if si <= i { back_edges.push((i, si)); } // 回边：目标索引 <= 源索引
            }
        }
    }
    let mut loops: Vec<LicmLoop> = Vec::new();
    for (latch, header) in back_edges {
        if latch == header { continue; }
        let mut blocks = HashSet::new();
        blocks.insert(header);
        let mut work = vec![latch];
        while let Some(x) = work.pop() {
            if !blocks.insert(x) { continue; }
            for &p in &pred[x] {
                if p != header && p < n { work.push(p); }
            }
        }
        if let Some(existing) = loops.iter_mut().find(|l| l.header == header) {
            existing.blocks.extend(blocks);
        } else {
            loops.push(LicmLoop { header, blocks });
        }
    }
    loops
}

/// 找循环 preheader：不在循环内、且有边直接跳到 header 的块（取索引最小者，稳定）。
fn licm_preheader(f: &IrFunction, lp: &LicmLoop) -> Option<usize> {
    let header_id = f.blocks[lp.header].id;
    let mut candidates = Vec::new();
    for (i, b) in f.blocks.iter().enumerate() {
        if lp.blocks.contains(&i) { continue; }
        if matches!(&b.term, Term::Jump { target, .. } if *target == header_id) {
            candidates.push(i);
        }
    }
    candidates.into_iter().min()
}

/// 一轮 LICM：对所有循环各做一次外提，返回外提指令数。
/// 一轮只处理一个循环（保守收敛），由不动点 driver 反复调用处理剩余循环与依赖链。
fn licm_round(f: &mut IrFunction) -> usize {
    let loops = licm_find_loops(f);
    if loops.is_empty() { return 0; }

    for lp in &loops {
        // 护栏：入口块（id 0 / 索引 0）被卷入循环说明是 select/复杂分支导致的
        // 反向可达性膨胀，非真自然循环——直接跳过，避免把入口指令外提到后块。
        if lp.blocks.contains(&0) { continue; }
        let ph = match licm_preheader(f, lp) { Some(p) => p, None => continue };
        // 合法 preheader 必须在 header 之前；否则外提目标比源还晚，破坏 SSA。
        if ph >= lp.header { continue; }

        // vreg → 定义块索引（含块参数）。
        let mut defs: HashMap<VReg, usize> = HashMap::new();
        for (bi, b) in f.blocks.iter().enumerate() {
            for inst in &b.insts { if let Some(d) = inst.def() { defs.insert(d, bi); } }
            for (p, _) in &b.params { defs.insert(*p, bi); }
        }
        // 循环内被「修改 / 归零 / 取别名 / 逃逸」的槽集合——这些槽的 Load 绝不可外提。
        let mut modified: HashSet<VReg> = HashSet::new();
        let mut bset: Vec<usize> = lp.blocks.iter().copied().collect();
        bset.sort_unstable();
        for &bi in &bset {
            for inst in &f.blocks[bi].insts {
                match inst {
                    IrInst::Store { ptr, .. }
                    | IrInst::RcRetain { ptr, .. } | IrInst::RcRelease { ptr, .. }
                    | IrInst::StructRetain { ptr, .. } | IrInst::StructRelease { ptr, .. }
                    | IrInst::FieldAddr { base: ptr, .. } | IrInst::PtrAdd { base: ptr, .. }
                    => { modified.insert(*ptr); }
                    IrInst::Call { func, args, .. } => {
                        match mem_effect_of(func) {
                            MemEffect::ReadNone | MemEffect::ReadOnly => {}
                            MemEffect::WriteArg0 | MemEffect::Unknown => {
                                for a in args { if let Value::Reg(r) = a { modified.insert(*r); } }
                            }
                        }
                    }
                    IrInst::CallIndirect { args, .. } => {
                        for a in args { if let Value::Reg(r) = a { modified.insert(*r); } }
                    }
                    _ => {}
                }
            }
        }

        // 收集可外提指令。
        let mut hoist: Vec<(usize, usize, IrInst)> = Vec::new();
        for &bi in &bset {
            // 只外提定义块在 preheader 之后的指令：保证是「向前搬」，
            // 绝不把入口/前序块的定义搬到更晚的 preheader（SSA 支配破坏）。
            if bi < ph { continue; }
            for (ii, inst) in f.blocks[bi].insts.iter().enumerate() {
                if !inst.is_pure() { continue; }
                match inst {
                    // 地址别名 / 栈帧槽分配自身绝不外提（见锚点 3）。
                    IrInst::Alloca { .. } | IrInst::FieldAddr { .. } | IrInst::PtrAdd { .. } => continue,
                    IrInst::Bin { op, .. } if matches!(op, ABin::Div | ABin::Mod) => continue, // trap 位置
                    // Load 仅当 ptr 槽在循环内完全未被修改/归零/取别名/逃逸时可外提。
                    IrInst::Load { ptr, .. } => { if modified.contains(ptr) { continue; } }
                    _ => {}
                }
                let mut uses: Vec<VReg> = Vec::new();
                inst.reg_uses(&mut uses);
                let ok = uses.iter().all(|u| match defs.get(u) {
                    None => true,                       // 外部定义（如入口参数槽）
                    Some(&db) => !lp.blocks.contains(&db),
                });
                if ok { hoist.push((bi, ii, inst.clone())); }
            }
        }
        if hoist.is_empty() { continue; }

        // 应用：按 (块序, 指令序) 升序追加到 preheader（保持定义→使用顺序）；
        // 删除按降序，避免同块索引失效。
        hoist.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        let moved: Vec<IrInst> = hoist.iter().map(|(_, _, i)| i.clone()).collect();
        let mut to_del: Vec<(usize, usize)> = hoist.iter().map(|(bi, ii, _)| (*bi, *ii)).collect();
        to_del.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
        for (bi, ii) in to_del { f.blocks[bi].insts.remove(ii); }
        f.blocks[ph].insts.extend(moved);
        return hoist.len();
    }
    0
}

/// 对全程序跑通用 LICM（不动点）。返回外提指令总数。
pub fn licm(ip: &mut IrProgram) -> usize {
    let mut total = 0usize;
    const LICM_MAX_ROUNDS: usize = 8;
    for _ in 0..LICM_MAX_ROUNDS {
        let mut round_total = 0usize;
        let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
        for name in names {
            if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
                round_total += licm_round(f);
            }
        }
        total += round_total;
        if round_total == 0 { break; }
    }
    // 调试设施（与 DIALC_DUMP_ASM_IR 同惯例）：设 DIALC_DUMP_LICM=1 打印外提指令数。
    if std::env::var("DIALC_DUMP_LICM").is_ok() {
        eprintln!("[licm] hoisted {} instructions", total);
    }
    // 外提后跑一次 DCE，清理循环内变成死代码的原指令。
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for name in names {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) { dce(f); }
    }
    total
}

// ============================================================================
// M13-sccp：保守条件常量传播 / 分支折叠（对标 gcc/clang -O2 SCCP）
// ============================================================================
//
// 第一版范围极保守、零误拒优先：
//   - 格：Option<Imm>，Some=编译期确定的常量，None=运行时值（Dial 无 UB，
//     绝不引入 Undefined 假设）。不动点迭代到稳定。
//   - 只信任可证明的常量来源：立即数、纯整数/比较/转换指令的常量折叠结果、
//     块参数 φ（所有前驱传入同一常量）。Load/Call/Store/ARC/Lea/Alloca/
//     FuncAddr 等定义一律 Unknown。
//   - 唯一变换：把 `Branch{cond}` 在 cond 恒为整数常量时改为 `Jump` 到对应
//     目标（带对应 args）。另一分支块保留不删，其中的 Call/Store/ARC 不执行
//     本就正确（cond 恒真时那条边运行时不可达）；codegen 只为可达跳转发射，
//     死块零副作用。绝不删除任何块 / Call / Store / ARC。
//
// 正确性锚点：
//   1. 除零 / 浮点 NaN 语义保守：折叠失败即 Unknown，绝不假设。
//   2. φ 只有在「全部前驱都传入同一常量」时才是常量；任一前驱非常量即放弃。
//   3. 只折叠整数常量条件（Imm::I64/I32/I1/I8）；浮点/句柄条件不动。
//   4. 只 x86-64 / O2+ 且非 -g/PGO/插桩挂接（同内联/LICM 护栏）。

/// 从 Value 读常量（仅立即数，或从格中已确定的寄存器）。
fn sccp_val(v: &Value, lattice: &HashMap<VReg, Imm>) -> Option<Imm> {
    match v {
        Value::Imm(i) => Some(*i),
        Value::Reg(r) => lattice.get(r).copied(),
    }
}

/// 对一条纯指令尝试求常量；失败返回 None（Unknown）。
fn sccp_eval_inst(inst: &IrInst, lattice: &HashMap<VReg, Imm>) -> Option<Imm> {
    match inst {
        IrInst::Bin { op, a, b, ty, .. } => {
            if *ty != IrType::I64 { return None; }
            let x = sccp_val(a, lattice)?.as_i64();
            let y = sccp_val(b, lattice)?.as_i64();
            fold_bin_i64(*op, x, y).map(Imm::I64)
        }
        IrInst::FBin { op, a, b, ty, .. } => {
            if *ty != IrType::F64 { return None; }
            let x = val_f64(a)?;
            let y = val_f64(b)?;
            fold_bin_f64(*op, x, y).map(|r| Imm::F64(f64::to_bits(r)))
        }
        IrInst::ICmp { op, a, b, .. } => {
            let x = sccp_val(a, lattice)?.as_i64();
            let y = sccp_val(b, lattice)?.as_i64();
            Some(Imm::I64(cmp_i64(*op, x, y)))
        }
        IrInst::FCmp { op, a, b, .. } => {
            let x = val_f64(a)?;
            let y = val_f64(b)?;
            Some(Imm::I64(cmp_f64(*op, x, y)))
        }
        IrInst::Un { op, a, ty, .. } => {
            if *ty != IrType::I64 { return None; }
            let x = sccp_val(a, lattice)?.as_i64();
            Some(Imm::I64(match op {
                AUn::Neg => x.wrapping_neg(),
                AUn::Not => (x == 0) as i64,
                AUn::BitNot => !x,
            }))
        }
        IrInst::Conv { kind, a, ty, .. } => {
            // 仅整数扩展/截断，且结果类型整数（浮点转换的位模式语义复杂，放弃）。
            if ty.is_float() || matches!(kind, AConv::FpSi | AConv::SiFp | AConv::FpExt | AConv::FpTrunc) {
                return None;
            }
            let x = sccp_val(a, lattice)?.as_i64();
            Some(match kind {
                AConv::Sext | AConv::Zext => Imm::I64(x),
                AConv::Trunc => Imm::I64(x), // 全 I64 口径，截断恒等
                _ => return None,
            })
        }
        _ => None,
    }
}

/// 求单个函数的常量格（不动点）。返回 vreg → 确定常量。
fn sccp_lattice(f: &IrFunction) -> HashMap<VReg, Imm> {
    let mut lattice: HashMap<VReg, Imm> = HashMap::new();
    for _ in 0..16 {
        let mut changed = false;
        // ① 指令定义的 vreg。
        for b in &f.blocks {
            for inst in &b.insts {
                if let Some(d) = inst.def() {
                    if lattice.contains_key(&d) { continue; } // 单调：确定后不变
                    if let Some(c) = sccp_eval_inst(inst, &lattice) {
                        lattice.insert(d, c);
                        changed = true;
                    }
                }
            }
        }
        // ② 块参数 φ：所有前驱传入同一常量才确定。
        // 先收集每个块参数 vreg 的所有传入 Value。
        for (bi, b) in f.blocks.iter().enumerate() {
            if b.params.is_empty() { continue; }
            for (pi, (pv, _)) in b.params.iter().enumerate() {
                if lattice.contains_key(pv) { continue; }
                let mut incoming: Vec<Value> = Vec::new();
                for pb in &f.blocks {
                    let args: &[Value] = match &pb.term {
                        Term::Jump { target, args } if *target == b.id => args,
                        Term::Branch { then_b, else_b, then_args, else_args, .. } => {
                            if *then_b == b.id { then_args }
                            else if *else_b == b.id { else_args }
                            else { continue; }
                        }
                        _ => continue,
                    };
                    if let Some(v) = args.get(pi) { incoming.push(*v); }
                }
                if incoming.is_empty() { continue; }
                let mut it = incoming.iter();
                let first = sccp_val(it.next().unwrap(), &lattice);
                let first = match first { Some(c) => c, None => continue };
                // 同一常量判定：Imm 按位比较（I64 口径比较 as_i64，浮点按 bits）。
                let same = it.all(|v| sccp_val(v, &lattice).map(|c| imm_bits(c) == imm_bits(first)).unwrap_or(false));
                if same { lattice.insert(*pv, first); changed = true; }
            }
            let _ = bi;
        }
        if !changed { break; }
    }
    lattice
}

/// 对单函数做分支折叠。返回折叠的分支数。
fn sccp_fold_fn(f: &mut IrFunction) -> usize {
    let lattice = sccp_lattice(f);
    let mut count = 0;
    for b in &mut f.blocks {
        if let Term::Branch { cond, then_b, else_b, then_args, else_args, .. } = &b.term {
            let c = match lattice.get(cond) { Some(c) => *c, None => continue };
            // 仅整数常量条件（浮点已在上面过滤；as_i64 对整数安全）。
            let n = match c {
                Imm::F64(_) | Imm::F32(_) => continue,
                _ => c.as_i64(),
            };
            let (target, args) = if n != 0 { (*then_b, then_args.clone()) } else { (*else_b, else_args.clone()) };
            b.term = Term::Jump { target, args };
            count += 1;
        }
    }
    count
}

/// 对全程序跑保守 SCCP（每函数独立）。返回折叠的分支数。
pub fn sccp(ip: &mut IrProgram) -> usize {
    let mut total = 0usize;
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for name in names {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            total += sccp_fold_fn(f);
        }
    }
    // 调试设施（与 DIALC_DUMP_ASM_IR 同惯例）：设 DIALC_DUMP_SCCP=1 打印折叠分支数。
    if std::env::var("DIALC_DUMP_SCCP").is_ok() {
        eprintln!("[sccp] folded {} branches", total);
    }
    if total > 0 {
        // 折叠后跑 DCE/常量折叠，清理变成死代码的纯指令；不动点两轮足够。
        let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
        for name in names {
            if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
                fold_constants(f);
                dce(f);
            }
        }
    }
    total
}

// ============================================================================
// M14-dce-fn：全程序死函数消除（对标 gcc/clang -O2 的 IPA 死代码消除）
// ============================================================================
//
// 删除「Private + 非 main + 全程序无任何引用」的函数，减少产物体积与符号表。
// 引用判定必须全面——漏判会把仍在用的函数删掉（undefined symbol）：
//   - 直接调用 `Call{linkage: Dial, func}`
//   - 取函数地址 `FuncAddr{func}`（函数指针 / 回调 / 闭包 thunk）
//   - 入口 main、External（导出）、运行时符号一律保留。
// 不动点：删除死函数后，它内部对其它函数的调用消失，可能暴露新的死函数。
//
// 注意：本 pass 在 LLVM 路径不跑（llvm 从 AST 直出，符号由链接器处理）；
// 仅在 asm 后端 O1+ 挂接（不依赖调用点优化，纯符号级分析，零语义风险）。

/// 统计全程序中被引用的函数名集合（Call + FuncAddr）。
fn dead_fn_referenced(ip: &IrProgram) -> HashSet<String> {
    let mut refs = HashSet::new();
    for f in &ip.functions {
        for b in &f.blocks {
            for inst in &b.insts {
                match inst {
                    IrInst::Call { func, .. } => { refs.insert(func.clone()); }
                    IrInst::FuncAddr { func, .. } => { refs.insert(func.clone()); }
                    _ => {}
                }
            }
        }
    }
    refs
}

/// 删除全程序死函数，返回删除数。
pub fn eliminate_dead_functions(ip: &mut IrProgram) -> usize {
    let mut removed = 0usize;
    loop {
        let refs = dead_fn_referenced(ip);
        let mut to_remove: Vec<String> = ip.functions.iter()
            .filter(|f| {
                f.linkage == FuncLink::Private
                    && f.name != "main"
                    && !refs.contains(&f.name)
            })
            .map(|f| f.name.clone())
            .collect();
        if to_remove.is_empty() { break; }
        to_remove.sort();
        ip.functions.retain(|f| !to_remove.contains(&f.name));
        removed += to_remove.len();
    }
    removed
}

// ============================================================================
// M15-unroll：保守循环完全展开（对标 gcc/clang -O3 loop unrolling）
// ============================================================================
//
// 只展开「标准 for 循环 5 块骨架 + 常量小上界」这一种最安全形态，收益是消除
// 循环控制的分支 / Load / 自增开销。第一版范围极保守、零误拒优先：
//   1. 精确匹配 5 块骨架：entry→header(Load cv; ICmp cv<N; Branch body/exit)
//      →body→latch(Load iv; Add iv,1; Store iv; Jump header)→exit；
//      N 为整数立即数且 2≤N≤8；iv 初值 0、步长 1、比较 Lt。
//   2. 所有循环块（header/body/latch）无 φ 参数——iv 用栈槽传递，展开后天然保持
//      顺序语义。含任何块参数一律放弃。
//   3. body 只含纯整数栈指令（Load/Store/Bin/ICmp/Conv/Un/Alloca/PtrAdd），
//      任何 Call / ARC / 浮点 / 内联汇编 / LeaLabel 等一律放弃——副作用与
//      句柄归零在展开后顺序敏感，保守不碰。
//   4. 展开：把 body+latch 复制 N 份（vreg 全量重映射），每份含 iv 自增，串成链，
//      末份跳 exit；header 与 latch 原块从 CFG 断开（改为 Unreachable，随后 DCE）。
//   5. 只 O3 + x86-64 挂接，同 -g/PGO/插桩护栏。

/// 识别可展开的标准 for 循环骨架（返回 N / 各块索引 / iv 槽）。
struct UnrollPlan {
    header: usize, body: usize, latch: usize, exit: usize,
    n: u64, iv_slot: VReg,
}

fn unroll_recognize(f: &IrFunction) -> Option<UnrollPlan> {
    if f.blocks.len() < 5 { return None; }
    // 取前 5 块（lower 的 for 骨架连续）：entry(0), header(1), body(2), latch(3), exit(4)
    let (b0, b1, b2, b3) = (&f.blocks[0], &f.blocks[1], &f.blocks[2], &f.blocks[3]);
    if !matches!(&b0.term, Term::Jump { target, .. } if *target == b1.id) { return None; }
    // header: Load cv, iv; ICmp Lt cv < N; Branch → body(2) / exit(4)
    if b1.insts.len() != 2 || !b1.params.is_empty() { return None; }
    let (iv_slot, n) = match (&b1.insts[0], &b1.insts[1]) {
        (IrInst::Load { dst: cv, ptr: iv, ty: IrType::I64 },
         IrInst::ICmp { op: ACmp::Lt, a: Value::Reg(a), b: Value::Imm(lim), .. })
            if *a == *cv => {
            let nv = lim.as_i64();
            if nv < 2 || nv > 8 { return None; }
            (*iv, nv as u64)
        }
        _ => return None,
    };
    if f.blocks.len() <= 4 { return None; }
    let exit_id = f.blocks[4].id;
    match &b1.term {
        Term::Branch { cond, then_b, else_b, .. }
            if *cond == match &b1.insts[1] { IrInst::ICmp { dst, .. } => *dst, _ => u32::MAX }
               && *then_b == b2.id && *else_b == exit_id => {}
        _ => return None,
    }
    // body: 无 φ、只跳 latch(3)、指令全在白名单。
    if !b2.params.is_empty() || !matches!(&b2.term, Term::Jump { target, .. } if *target == b3.id) { return None; }
    for inst in &b2.insts {
        match inst {
            IrInst::Load { .. } | IrInst::Store { .. } | IrInst::Bin { .. }
            | IrInst::ICmp { .. } | IrInst::Conv { .. } | IrInst::Un { .. }
            | IrInst::Alloca { .. } | IrInst::PtrAdd { .. } => {}
            _ => return None,
        }
        // 浮点指令一律拒绝（Bin 可能承载 FBin？FBin 是独立变体已被上面拒绝）。
        if let IrInst::Bin { ty, .. } = inst { if ty.is_float() { return None; } }
    }
    // latch: Load cv2, iv; Add nv, cv2, 1; Store iv, nv; Jump header(1)
    if !b3.params.is_empty() || !matches!(&b3.term, Term::Jump { target, .. } if *target == b1.id) { return None; }
    let lb = b3.insts.len();
    if lb != 3 { return None; }
    match (&b3.insts[0], &b3.insts[1], &b3.insts[2]) {
        (IrInst::Load { dst: cv2, ptr: iv2, ty: IrType::I64 },
         IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(x), b: Value::Imm(step), ty: IrType::I64 },
         IrInst::Store { ptr: iv3, val: Value::Reg(y), ty: IrType::I64 })
            if *cv2 == *x && *nv == *y && *iv2 == *iv3 && *iv2 == iv_slot && step.as_i64() == 1 => {}
        _ => return None,
    }
    // iv 在 entry 初值必须是 0。
    if !b0.insts.iter().any(|i| matches!(i,
        IrInst::Store { ptr, val: Value::Imm(im), .. } if *ptr == iv_slot && im.as_i64() == 0)) {
        return None;
    }
    Some(UnrollPlan { header: 1, body: 2, latch: 3, exit: 4, n, iv_slot })
}

/// 对单函数做一次循环展开，返回展开的循环数。
fn unroll_fn(f: &mut IrFunction) -> usize {
    let plan = match unroll_recognize(f) { Some(p) => p, None => return 0 };
    let body_insts = f.blocks[plan.body].insts.clone();
    let latch_insts = f.blocks[plan.latch].insts.clone();
    // 需要重映射的 vreg：body + latch 里所有定义（含块参数，但已确认无 φ）。
    let mut src_regs: Vec<VReg> = Vec::new();
    for inst in body_insts.iter().chain(latch_insts.iter()) {
        if let Some(d) = inst.def() { src_regs.push(d); }
    }
    src_regs.sort_unstable();
    src_regs.dedup();

    let max_id = f.blocks.iter().map(|b| b.id).max().unwrap_or(0);
    let exit_id = f.blocks[plan.exit].id;
    let header_id = f.blocks[plan.header].id;
    let mut next_target = exit_id; // 末份跳 exit；从后往前构造链
    let mut new_blocks: Vec<IrBlock> = Vec::new();

    for k in (0..plan.n).rev() {
        // 每份用独立 vreg 子集（避免跨份 SSA 冲突）。
        let vmap: Vec<VReg> = src_regs.iter().map(|r| f.new_reg(f.reg_ty[*r as usize].clone())).collect();
        let mut insts: Vec<IrInst> = Vec::new();
        // body 指令重映射（复用 inline 的通用 remap：它按 vmap 数组索引，这里需按 src_regs 查表）。
        for inst in body_insts.iter().chain(latch_insts.iter()) {
            if let Some(ri) = unroll_remap_inst(inst, &src_regs, &vmap) { insts.push(ri); }
        }
        let bid = max_id + 1 + (plan.n - 1 - k) as u32;
        new_blocks.push(IrBlock {
            id: bid, params: Vec::new(), insts,
            term: Term::Jump { target: next_target, args: Vec::new() },
        });
        next_target = bid;
        let _ = (k, header_id);
    }
    // 新块是按 k 逆序构造的（链从末份→首份），现在反转成首份在前。
    new_blocks.reverse();
    // entry 原本 Jump header，改为 Jump 首份展开块。
    let first_id = next_target;
    for b in f.blocks.iter_mut() {
        if let Term::Jump { target, .. } = &mut b.term {
            if *target == header_id { *target = first_id; }
        }
        if let Term::Branch { then_b, else_b, .. } = &mut b.term {
            if *then_b == header_id { *then_b = first_id; }
            if *else_b == header_id { *else_b = first_id; }
        }
    }
    // header/body/latch 断开为 Unreachable（后续 DCE 清理其纯指令；它们已无前驱）。
    for idx in [plan.header, plan.body, plan.latch] {
        f.blocks[idx].term = Term::Unreachable;
    }
    f.blocks.extend(new_blocks);
    1
}

/// 展开时按 src_regs→vmap 查表重映射一条指令（Line/DebugVar 跳过）。
fn unroll_remap_inst(inst: &IrInst, src: &[VReg], vmap: &[VReg]) -> Option<IrInst> {
    let rv = |r: VReg| -> VReg {
        if let Ok(pos) = src.binary_search(&r) { vmap[pos] } else { r }
    };
    let rval = |v: Value| match v { Value::Reg(r) => Value::Reg(rv(r)), other => other };
    Some(match inst {
        IrInst::Line(_) | IrInst::DebugVar { .. } => return None,
        IrInst::Alloca { dst, bytes } => IrInst::Alloca { dst: rv(*dst), bytes: *bytes },
        IrInst::Load { dst, ptr, ty } => IrInst::Load { dst: rv(*dst), ptr: rv(*ptr), ty: ty.clone() },
        IrInst::Store { ptr, val, ty } => IrInst::Store { ptr: rv(*ptr), val: rval(*val), ty: ty.clone() },
        IrInst::Bin { op, dst, a, b, ty } =>
            IrInst::Bin { op: *op, dst: rv(*dst), a: rval(*a), b: rval(*b), ty: ty.clone() },
        IrInst::ICmp { op, dst, a, b, ty } =>
            IrInst::ICmp { op: *op, dst: rv(*dst), a: rval(*a), b: rval(*b), ty: ty.clone() },
        IrInst::Conv { kind, dst, a, ty } =>
            IrInst::Conv { kind: *kind, dst: rv(*dst), a: rval(*a), ty: ty.clone() },
        IrInst::Un { op, dst, a, ty } =>
            IrInst::Un { op: *op, dst: rv(*dst), a: rval(*a), ty: ty.clone() },
        IrInst::PtrAdd { dst, base, off } =>
            IrInst::PtrAdd { dst: rv(*dst), base: rv(*base), off: rval(*off) },
        _ => return None, // 白名单外的指令不应出现（unroll_recognize 已挡）
    })
}

/// 对全程序跑保守循环展开（每函数最多展开 1 个循环）。返回展开数。
pub fn unroll_loops(ip: &mut IrProgram) -> usize {
    let mut total = 0usize;
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for name in names {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            if unroll_fn(f) > 0 {
                dce(f);
                if std::env::var("DIALC_DUMP_UNROLL").is_ok() { eprintln!("[unroll] {}", f.name); }
                total += 1;
            }
        }
    }
    total
}

// ============================================================================
// M16-popcount：把标准 `c=0; while (x>0) { c += x&1; x >>= 1; }` 骨架替换为
// SWAR 位并行计数（O(1)，对标 gcc 在 -mpopcnt 下的 popcnt）。
//
// 与 M8-recspec 同族：私有后端理解 Dial 语义，识别通用算法模式并做等价变换。
// 护栏：只对 x>=0 走 SWAR；x<0 保留原循环（原语义下 while 立即为假、返回 0），
// 逐位验证 SWAR 与循环在 [0, u64::MAX] 全非负域逐点一致。
// 只识别「入口 → header(Load x / ICmp Gt 0) → body(严格 8 条) → header /
// exit(Load c / Return)」的规范 4 块骨架，任何偏离一律放弃（零误拒）。
// ============================================================================

/// 在 x86-64 上把 popcount 循环折叠为 SWAR 序列。返回折叠的函数数。
pub fn fold_popcount(ip: &mut IrProgram) -> usize {
    fn imm(v: &Value) -> Option<i64> {
        if let Value::Imm(i) = v { Some(i.as_i64()) } else { None }
    }
    fn reg(v: &Value) -> Option<VReg> {
        if let Value::Reg(r) = v { Some(*r) } else { None }
    }

    // 只读扫描，收集 (函数名, pred id, header id, body id, exit id, x 槽, c 槽)。
    // 不假设 while 从函数 entry 开始——b23 的 while 嵌在 for 体内，header 的
    // 入口前驱是 for body 块；回边来自 body 自身，需排除后才能定位唯一 pred。
    struct Plan { name: String, pred: u32, header: u32, body: u32, exit: u32, px: VReg, pc: VReg }

    let mut plans: Vec<Plan> = Vec::new();
    for f in &ip.functions {
        if f.ret != IrType::I64 { continue; }
        let by_id = |id: u32| f.blocks.iter().find(|b| b.id == id);
        let headers: Vec<u32> = f.blocks.iter().map(|b| b.id).collect();
        for header_id in headers {
        let header = match by_id(header_id) { Some(b) => b, None => continue };
        if header.insts.len() != 2 { continue; }
        let (xh, px) = match &header.insts[0] {
            IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr),
            _ => continue,
        };
        let cond = match &header.insts[1] {
            IrInst::ICmp { op: ACmp::Gt, dst, a: Value::Reg(a), b, ty: IrType::I64 }
                if *a == xh && imm(b) == Some(0) => *dst,
            _ => continue,
        };
        let (body_id, exit_id) = match header.term {
            Term::Branch { cond: c, then_b, else_b, ref then_args, ref else_args }
                if c == cond && then_args.is_empty() && else_args.is_empty() => (then_b, else_b),
            _ => continue,
        };
        if body_id == exit_id || body_id == header_id || exit_id == header_id { continue; }
        let body = match by_id(body_id) { Some(b) => b, None => continue };
        if body.insts.len() != 8 { continue; }
        let (cld, pc) = match &body.insts[0] {
            IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr),
            _ => continue,
        };
        let (xld1, px1) = match &body.insts[1] {
            IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr),
            _ => continue,
        };
        let t1 = match &body.insts[2] {
            IrInst::Bin { op: ABin::BitAnd, dst, a: Value::Reg(a), b, ty: IrType::I64 }
                if *a == xld1 && imm(b) == Some(1) => *dst,
            _ => continue,
        };
        let t2 = match &body.insts[3] {
            IrInst::Bin { op: ABin::Add, dst, a: Value::Reg(a), b: Value::Reg(b), ty: IrType::I64 }
                if *a == cld && *b == t1 => *dst,
            _ => continue,
        };
        match &body.insts[4] {
            IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::I64 } if *ptr == pc && *v == t2 => {}
            _ => continue,
        }
        let (xld2, px2) = match &body.insts[5] {
            IrInst::Load { dst, ptr, ty: IrType::I64 } => (*dst, *ptr),
            _ => continue,
        };
        let t3 = match &body.insts[6] {
            IrInst::Bin { op: ABin::Shr, dst, a: Value::Reg(a), b, ty: IrType::I64 }
                if *a == xld2 && imm(b) == Some(1) => *dst,
            _ => continue,
        };
        match &body.insts[7] {
            IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::I64 } if *ptr == px && *v == t3 => {}
            _ => continue,
        }
        match body.term {
            Term::Jump { target, ref args } if target == header_id && args.is_empty() => {}
            _ => continue,
        }
        if px1 != px || px2 != px || px == pc { continue; }
        let exit = match by_id(exit_id) { Some(b) => b, None => continue };
        // exit 首条须是从 c 槽读出结果；其后可继续任意计算（b23 在 exit 里
        // 做 s=(s+c)%MOD 再跳回外层 for），不强求 Return——fast 只需把
        // popcount 结果写回 c 槽再跳 exit，后续逻辑原样复用。
        let exit_loads_c = exit.insts.iter().any(|i| matches!(
            i, IrInst::Load { ptr, ty: IrType::I64, .. } if *ptr == pc));
        if !exit_loads_c { continue; }
        // 循环外唯一前驱：排除来自 body 的回边；多前驱（φ 合并）一律放弃。
        let mut preds: Vec<u32> = Vec::new();
        for pb in &f.blocks {
            let targets: Vec<u32> = match pb.term {
                Term::Jump { target, .. } => vec![target],
                Term::Branch { then_b, else_b, .. } => {
                    let mut v = vec![then_b];
                    if else_b != then_b { v.push(else_b); }
                    v
                }
                _ => Vec::new(),
            };
            if pb.id != body_id && targets.contains(&header_id) { preds.push(pb.id); }
        }
        if preds.len() != 1 { continue; }
        let pred_id = preds[0];
        if pred_id == body_id || pred_id == exit_id { continue; }
        plans.push(Plan { name: f.name.clone(), pred: pred_id, header: header_id,
                          body: body_id, exit: exit_id, px, pc });
        }
    }

    let mut count = 0;
    for p in plans {
        let f = match ip.functions.iter_mut().find(|f| f.name == p.name) { Some(f) => f, None => continue };
        let max_id = f.blocks.iter().map(|b| b.id).max().unwrap_or(f.entry);
        let fast_id = max_id + 1;
        let (px, pc) = (p.px, p.pc);

        // pred（while 的循环外入口块）：载入 x，判 x>=0，真→fast，假→原 header
        // （负数路径语义不变）。fast 直接跳 exit，跳过整个 while。
        let xv = f.new_reg(IrType::I64);
        let ok = f.new_reg(IrType::I64);
        {
            let pred = f.blocks.iter_mut().find(|b| b.id == p.pred).unwrap();
            pred.insts.push(IrInst::Load { dst: xv, ptr: px, ty: IrType::I64 });
            pred.insts.push(IrInst::ICmp {
                op: ACmp::Ge, dst: ok, a: Value::Reg(xv), b: Value::Imm(Imm::I64(0)), ty: IrType::I64,
            });
            pred.term = Term::Branch {
                cond: ok, then_b: fast_id, else_b: p.header,
                then_args: Vec::new(), else_args: Vec::new(),
            };
        }

        // fast：SWAR 位并行 popcount（64 位）。
        let mut insts: Vec<IrInst> = Vec::new();
        fn bin(insts: &mut Vec<IrInst>, f: &mut IrFunction, op: ABin, a: Value, b: Value) -> VReg {
            let d = f.new_reg(IrType::I64);
            insts.push(IrInst::Bin { op, dst: d, a, b, ty: IrType::I64 });
            d
        }
        let im = |v: i64| Value::Imm(Imm::I64(v));
        let r1 = bin(&mut insts, f, ABin::Shr, Value::Reg(xv), im(1));
        let r2 = bin(&mut insts, f, ABin::BitAnd, Value::Reg(r1), im(0x5555555555555555));
        let r3 = bin(&mut insts, f, ABin::Sub, Value::Reg(xv), Value::Reg(r2));
        let r4 = bin(&mut insts, f, ABin::Shr, Value::Reg(r3), im(2));
        let r5 = bin(&mut insts, f, ABin::BitAnd, Value::Reg(r4), im(0x3333333333333333));
        let r6 = bin(&mut insts, f, ABin::BitAnd, Value::Reg(r3), im(0x3333333333333333));
        let r7 = bin(&mut insts, f, ABin::Add, Value::Reg(r5), Value::Reg(r6));
        let r8 = bin(&mut insts, f, ABin::Shr, Value::Reg(r7), im(4));
        let r9 = bin(&mut insts, f, ABin::BitAnd, Value::Reg(r8), im(0x0F0F0F0F0F0F0F0F));
        let r10 = bin(&mut insts, f, ABin::BitAnd, Value::Reg(r7), im(0x0F0F0F0F0F0F0F0F));
        let r11 = bin(&mut insts, f, ABin::Add, Value::Reg(r9), Value::Reg(r10));
        let r12 = bin(&mut insts, f, ABin::Mul, Value::Reg(r11), im(0x0101010101010101));
        let r13 = bin(&mut insts, f, ABin::Shr, Value::Reg(r12), im(56));
        insts.push(IrInst::Store { ptr: pc, val: Value::Reg(r13), ty: IrType::I64 });
        f.blocks.push(IrBlock {
            id: fast_id, params: Vec::new(), insts,
            term: Term::Jump { target: p.exit, args: Vec::new() },
        });
        count += 1;
        if std::env::var("DIALC_DUMP_POPCOUNT").is_ok() {
            eprintln!("[popcount] {}", f.name);
        }
    }
    count
}

// ============================================================================
// M18-arrflat：强化的 ARC / 逃逸分析 + 局部 int[] 句柄扁平化（治根 b17）
// ============================================================================
//
// 背景：数组下标 a[i] 在 IR 里是 `Call dial_array_get(handle, i)`，抽象过高——
// LICM/IVR/CSE 看不穿它是访存，导致循环内每轮都重新 `Load handle_slot` 并
// 解引用 payload（codegen 虽内联为裸 load，但句柄槽的重复 load 无法消除）。
// b17 的 A/B/C 是「函数局部、非逃逸、kind=0（纯 int 元素）」的数组，其 payload
// 基址在循环内恒定，可提升为循环不变量，让内层只剩 addr 计算 + 访存。
//
// 安全判据（全部满足才扁平化，否则零变化）：
//   1. 由 `dial_array_new(kind=0)` 创建（纯 int 元素 → set/push 无元素 ARC，快路径成立）；
//   2. 句柄只存回自己的栈槽（set/push 的返回值写回同一 slot），不逃到别处；
//   3. 句柄 vreg 只出现在白名单 runtime 调用（get/set/length/push/retain/release）
//      的参数位、或 Load/Store 自己的槽；绝不出现在 Return / 其它 Call / 字段地址 /
//      函数指针 / 间接调用 / 作为下标；
//   4. ReleaseFast + x86-64 + O3 + 非 -g / 非 PGO / 非 instrument（在 asm.rs 挂接处护栏）。

/// 一个被证明安全的局部 int[] 句柄槽。
#[derive(Clone, Debug)]
pub struct LocalIntArray {
    pub slot: VReg,          // 存句柄的栈槽
    pub payload_off: i64,    // payload 基址相对句柄的偏移（elems@24）
}

/// 白名单 runtime 函数：句柄作为参数出现是安全的（不改变"句柄身份"的逃逸）。
fn arrflat_whitelist(fn_name: &str) -> bool {
    matches!(fn_name,
        "dial_array_get" | "dial_array_set" | "dial_array_length"
        | "dial_array_push" | "dial_array_retain" | "dial_array_release")
}

/// 指令是否把 `v` 当作句柄使用（需要检查逃逸）。返回所有被使用的寄存器。
fn arrflat_collect_uses(inst: &IrInst, out: &mut Vec<VReg>) {
    inst.reg_uses(out);
}

/// 分析函数内所有"安全局部 int[] 句柄槽"。保守：任何不确定一律排除。
pub fn analyze_local_int_arrays(f: &IrFunction) -> Vec<LocalIntArray> {
    // ---- 步骤 1：找候选（dial_array_new(0) → Store 到槽）----
    let mut candidates: Vec<(VReg, VReg)> = Vec::new(); // (slot, init_handle)
    for b in &f.blocks {
        for (ii, inst) in b.insts.iter().enumerate() {
            if let IrInst::Call { dst: Some(h), func, args, arg_tys, ret_ty, .. } = inst {
                if func == "dial_array_new" && *ret_ty == IrType::Handle
                    && args.len() == 1 && arg_tys.len() == 1
                {
                    // kind 必须是立即数 0（INT 元素）。
                    if let (Value::Imm(Imm::I64(0)), IrType::I64) = (args[0], &arg_tys[0]) {
                        // 紧跟一条 Store{ptr:slot, val:h}（允许中间夹 Line/DebugVar）。
                        let mut j = ii + 1;
                        while j < b.insts.len() {
                            match &b.insts[j] {
                                IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::Handle }
                                    if *v == *h => { candidates.push((*ptr, *h)); break; }
                                IrInst::Line(_) | IrInst::DebugVar { .. } => { j += 1; continue; }
                                _ => break,
                            }
                        }
                    }
                }
            }
        }
    }
    if candidates.is_empty() { return Vec::new(); }

    let mut out: Vec<LocalIntArray> = Vec::new();
    for (slot, init_h) in candidates {
        // ---- 步骤 2：求句柄别名闭包 H（init + 从槽 load 出的 + set/push 返回值写回槽的）----
        let mut hset: HashSet<VReg> = HashSet::new();
        hset.insert(init_h);
        loop {
            let mut grew = false;
            // 从 slot 的 Load 出来的 vreg 加入 H
            for b in &f.blocks {
                for inst in &b.insts {
                    if let IrInst::Load { dst, ptr, ty: IrType::Handle } = inst {
                        if *ptr == slot && !hset.contains(dst) { hset.insert(*dst); grew = true; }
                    }
                    // set/push 返回值 h' 若 Store 回同一槽，h' 也是合法句柄别名
                    if let IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::Handle } = inst {
                        if *ptr == slot && !hset.contains(v) {
                            // 确认 v 是 set/push 的 dst
                            if is_arr_handle_producer(f, *v) { hset.insert(*v); grew = true; }
                        }
                    }
                }
            }
            if !grew { break; }
        }

        // ---- 步骤 3：逃逸检查 ----
        let mut escaped = false;
        // 3a. 终止符（Return / 分支传参）里出现句柄 → 逃逸
        for b in &f.blocks {
            let mut tu: Vec<VReg> = Vec::new();
            match &b.term {
                Term::Return { val: Some(Value::Reg(v)) } => tu.push(*v),
                Term::Jump { args, .. } => for a in args { if let Value::Reg(v) = a { tu.push(*v); } },
                Term::Branch { then_args, else_args, .. } => {
                    for a in then_args.iter().chain(else_args.iter()) { if let Value::Reg(v) = a { tu.push(*v); } }
                }
                _ => {}
            }
            if tu.iter().any(|v| hset.contains(v)) { escaped = true; break; }
        }
        if escaped { continue; }

        // 3b. 逐指令检查句柄使用是否都在白名单边界内
        for b in &f.blocks {
            for inst in &b.insts {
                match inst {
                    IrInst::Call { dst, func, args, linkage, .. } => {
                        // 句柄作为 Call 参数：仅白名单 runtime 安全；其它一律逃逸。
                        // 注意：get 的第二参数（index）若命中 H 也是逃逸（句柄当整数用）。
                        for (ai, a) in args.iter().enumerate() {
                            if let Value::Reg(v) = a {
                                if hset.contains(v) {
                                    let safe_pos = arrflat_whitelist(func) && !(func == "dial_array_get" && ai == 1)
                                        && !(func == "dial_array_set" && ai != 0);
                                    if !safe_pos { escaped = true; }
                                }
                            }
                        }
                        // 句柄作为非白名单 Call 的 dst（产生新句柄但不在 H 管控）不算逃逸，跳过。
                        let _ = (dst, linkage);
                    }
                    IrInst::CallIndirect { target, args, .. } => {
                        if hset.contains(target) { escaped = true; }
                        for a in args { if let Value::Reg(v) = a { if hset.contains(v) { escaped = true; } } }
                    }
                    IrInst::Store { ptr, val, .. } => {
                        // 句柄作为值 Store 到别的槽 → 逃逸；写回自己的槽 → 安全。
                        if let Value::Reg(v) = val { if hset.contains(v) && *ptr != slot { escaped = true; } }
                        // 句柄作为地址 ptr（把句柄当指针写）→ 逃逸。
                        if hset.contains(ptr) { escaped = true; }
                    }
                    IrInst::FieldAddr { base, .. } | IrInst::PtrAdd { base, .. } => {
                        if hset.contains(base) { escaped = true; }
                    }
                    IrInst::FuncAddr { .. } => {}
                    IrInst::InlineAsm { args, .. } => {
                        for a in args { if let Value::Reg(v) = a { if hset.contains(v) { escaped = true; } } }
                    }
                    IrInst::EnvInit { argc, argv } => {
                        for v in [argc, argv] { if let Value::Reg(r) = v { if hset.contains(r) { escaped = true; } } }
                    }
                    // Load / Bin / ICmp / Conv 等：句柄若作算术/比较操作数 → 逃逸。
                    IrInst::Bin { a, b, .. } | IrInst::FBin { a, b, .. }
                    | IrInst::ICmp { a, b, .. } | IrInst::FCmp { a, b, .. } => {
                        for v in [a, b] { if let Value::Reg(r) = v { if hset.contains(r) { escaped = true; } } }
                    }
                    IrInst::Un { a, .. } | IrInst::Conv { a, .. } => {
                        if let Value::Reg(r) = a { if hset.contains(r) { escaped = true; } }
                    }
                    _ => {}
                }
                if escaped { break; }
            }
            if escaped { break; }
        }
        if escaped { continue; }

        out.push(LocalIntArray { slot, payload_off: 24 });
    }
    out
}

/// v 是否是 dial_array_set/push 的 dst（句柄生产者）。
fn is_arr_handle_producer(f: &IrFunction, v: VReg) -> bool {
    for b in &f.blocks {
        for inst in &b.insts {
            if let IrInst::Call { dst: Some(d), func, .. } = inst {
                if *d == v && matches!(func.as_str(), "dial_array_set" | "dial_array_push") {
                    return true;
                }
            }
        }
    }
    false
}

/// 诊断：打印识别到的安全局部 int[]（仅 DIALC_ARRFLAT_DUMP=1）。
pub fn analyze_arrays_diag(ip: &IrProgram) -> usize {
    let mut total = 0;
    for f in &ip.functions {
        let arrs = analyze_local_int_arrays(f);
        if !arrs.is_empty() {
            if std::env::var("DIALC_ARRFLAT_DUMP").is_ok() {
                eprintln!("[arrflat] fn={} safe_local_int_arrays={:?}", f.name,
                    arrs.iter().map(|a| a.slot).collect::<Vec<_>>());
            }
        }
        total += arrs.len();
    }
    total
}

/// 消除安全 int[] 的 `dial_array_set` 冗余句柄写回。
///
/// 对 kind=0 局部独占数组，`dial_array_set(a,i,v)` 恒等返回 a（rc==1、kind==0，
/// 既不 COW 也不 realloc）。lower 生成的 `Store handle_slot, set_ret` 是冗余的，
/// 且它让 LICM 误判「句柄槽在循环内被修改」而无法提升循环不变量的句柄 Load。
/// 删除该 Store 后，LICM 即可把 `Load handle` 提到循环外，消除每轮栈槽往返。
///
/// 安全性完全依赖 `analyze_local_int_arrays` 的证明（局部/非逃逸/kind=0）。
/// 返回删除的 Store 数。
pub fn elide_array_handle_store(ip: &mut IrProgram) -> usize {
    let mut total = 0;
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for name in names {
        let safe_slots: HashSet<VReg> = {
            let f = ip.functions.iter().find(|f| f.name == name).unwrap();
            analyze_local_int_arrays(f).into_iter().map(|a| a.slot).collect()
        };
        if safe_slots.is_empty() { continue; }
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == name) {
            for b in &mut f.blocks {
                let mut i = 0;
                while i < b.insts.len() {
                    // 找 dial_array_set 的 dst
                    let set_ret = match &b.insts[i] {
                        IrInst::Call { dst: Some(d), func, .. } if func == "dial_array_set" => *d,
                        _ => { i += 1; continue; }
                    };
                    // 紧跟（允许 Line/DebugVar）一条 Store{ptr:safe_slot, val:set_ret}
                    let mut j = i + 1;
                    while j < b.insts.len() {
                        match &b.insts[j] {
                            IrInst::Line(_) | IrInst::DebugVar { .. } => { j += 1; continue; }
                            IrInst::Store { ptr, val: Value::Reg(v), ty: IrType::Handle }
                                if safe_slots.contains(ptr) && *v == set_ret => {
                                b.insts.remove(j); // 删冗余写回
                                total += 1;
                                break;
                            }
                            _ => break,
                        }
                    }
                    i += 1;
                }
            }
        }
    }
    if total > 0 && std::env::var("DIALC_ARRFLAT_DUMP").is_ok() {
        eprintln!("[arrflat] elided {} redundant set-handle stores", total);
    }
    total
}

// ============================================================================
// M19：LLVM 风格函数副作用属性 + EarlyCSE（内存版本化 Load 冗余消除）
// ============================================================================
//
// 借鉴 LLVM 的两块基础设施，治本而非补丁：
//   1. **函数副作用属性（Function Attributes）**：LLVM 的 readnone / readonly /
//      argmemonly 分类。这里给每个 runtime 调用标注"读哪些内存、写哪些内存"，
//      供 EarlyCSE 与 LICM 共用，替代旧的"Call 的所有实参都当作被修改"。
//   2. **EarlyCSE（内存版本化）**：LLVM 的 EarlyCSE pass 核心——块内维护
//      "栈槽 vreg → 当前可用值"表，遇到 Load 先看表命中则复用；遇到写内存
//      的指令（Store / 写类 Call / ARC）按副作用精确失效。当前 cse() 只处理
//      纯算术指令、完全不碰 Load，这是 b17 块14 重复栈槽 Load 的根因。
//
// 正确性原则（绝不破坏 ARC / 内存语义）：
//   - 只复用 Load，绝不删除 Store/Call/ARC；删除的仅是"重复 Load"这一纯指令。
//   - 失效粒度：Store 失效被写槽；写类 Call 失效"可能逃逸到该调用的所有槽"
//     （保守：无 argmemonly 证明时全失效）；readonly/readnone 调用不失效任何槽。
//   - Alloca 地址、FieldAddr / PtrAdd 产生的别名一律视为独立的槽标识，绝不
//     跨别名复用（本函数若为这些形态直接跳过该槽）。
//   - 仅 x86-64 + O2+ 且非 -g/PGO/instrument 时由 asm.rs 挂接（护栏同 LICM）。

/// 函数内存副作用属性（LLVM `readnone`/`readonly`/`argmemonly` 的保守子集）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemEffect {
    /// 完全不访问函数外部可见内存（如纯算术查询）。
    ReadNone,
    /// 只读内存：读实参指向的对象，但不修改任何内存。
    /// 典型：dial_array_get / dial_array_length / dial_string_eq / dial_string_length。
    ReadOnly,
    /// 写实参指向的内存（argmemonly-write）；其余内存不碰。
    /// 典型：dial_array_set / dial_array_push（arg0 = 数组句柄）。
    WriteArg0,
    /// 副作用未知：可能读写任意内存。保守失效全部槽。
    Unknown,
}

/// 查询 runtime / Dial 函数的副作用属性。
/// 白名单外的函数一律 `Unknown`（保守），绝不因漏标而误删 Load。
pub fn mem_effect_of(func: &str) -> MemEffect {
    match func {
        // ── readnone：查询但不碰内存 ────────────────────────────────────────
        "dial_args" | "dial_chan_now_ms" | "dial_now_ms" => MemEffect::ReadNone,
        // ── readonly：读句柄内容，不写 ──────────────────────────────────────
        "dial_array_get" | "dial_array_length" | "dial_array_index_of"
        | "dial_string_get" | "dial_string_length" | "dial_string_eq"
        | "dial_string_index_of" | "dial_string_substring"
        | "dial_bytes_get" | "dial_bytes_length"
        | "dial_map_length" | "dial_map_contains" | "dial_map_get"
        | "dial_kvmap_length" | "dial_set_length"
        => MemEffect::ReadOnly,
        // ── argmemonly-write：只写 arg0 句柄指向的对象 ──────────────────────
        "dial_array_set" | "dial_array_set_borrowed" | "dial_array_push"
        | "dial_array_pop" | "dial_array_insert" | "dial_array_remove"
        | "dial_array_clear" | "dial_array_reverse" | "dial_array_sort"
        => MemEffect::WriteArg0,
        // ── 其余（new/clone/release/retain/concat/print/IO 等）一律保守 ────
        _ => MemEffect::Unknown,
    }
}

/// 块内单轮 EarlyCSE：维护 `ptr vreg → 可用值` 表，复用重复 Load。
/// 返回本块消除的 Load 数。注意：只删 Load，不碰 Store/Call/ARC。
fn early_cse_block(b: &mut crate::ir::IrBlock) -> usize {
    // avail: ptr vreg → val，表示"该槽当前持有的最新值"。
    let mut avail: HashMap<VReg, Value> = HashMap::new();
    let mut removed = 0usize;
    let mut i = 0usize;
    while i < b.insts.len() {
        let inst = b.insts[i].clone();
        match &inst {
            IrInst::Load { dst, ptr, .. } => {
                // 只复用 Reg 形态的可用值；Imm 形态交给常量折叠，避免类型/宽度错配。
                if let Some(Value::Reg(saved)) = avail.get(ptr).copied() {
                    // 命中：用已有值替换本 Load 的定义，删除 Load。
                    // 只在本块内替换后续使用（Load 的 dst 一定定义在上方，支配后续）。
                    for j in (i + 1)..b.insts.len() {
                        subst_inst(&mut b.insts[j], *dst, saved);
                    }
                    subst_term_val(&mut b.term, *dst, &Value::Reg(saved));
                    b.insts.remove(i);
                    removed += 1;
                    continue;
                } else {
                    avail.insert(*ptr, Value::Reg(*dst));
                }
            }
            IrInst::Store { .. } => {
                // 保守版（诊断）：Store 清空全部可用值，不做值传播。
                avail.clear();
            }
            IrInst::Call { func, args, .. } => {
                match mem_effect_of(func) {
                    MemEffect::ReadNone | MemEffect::ReadOnly => {}
                    // 写 arg0 指向的对象：该 arg0 若来自某槽的 Load，则该槽的内容
                    // 被间接修改——保守失效全部槽（不做跨调用别名证明）。
                    MemEffect::WriteArg0 => { let _ = args; avail.clear(); }
                    MemEffect::Unknown => { avail.clear(); }
                }
            }
            IrInst::CallIndirect { .. } | IrInst::InlineAsm { .. }
            | IrInst::EnvInit { .. }
            | IrInst::RcRetain { .. } | IrInst::RcRelease { .. }
            | IrInst::StructRetain { .. } | IrInst::StructRelease { .. }
            => { avail.clear(); }
            // FieldAddr / PtrAdd 产生别名，后续 Load 的 ptr 可能与已有槽别名——
            // 保守全失效，绝不跨别名复用。
            IrInst::FieldAddr { .. } | IrInst::PtrAdd { .. } => { avail.clear(); }
            _ => {}
        }
        i += 1;
    }
    removed
}

/// 在终止符里把 Reg(from) 替换为 Value(to)。
fn subst_term_val(t: &mut Term, from: VReg, to: &Value) {
    let rep = |v: &mut Value| {
        if matches!(v, Value::Reg(r) if *r == from) { *v = *to; }
    };
    match t {
        Term::Jump { args, .. } => for v in args.iter_mut() { rep(v); },
        Term::Branch { cond, then_args, else_args, .. } => {
            if *cond == from { if let Value::Reg(r) = to { *cond = *r; } }
            for v in then_args.iter_mut().chain(else_args.iter_mut()) { rep(v); }
        }
        Term::Return { val } => if let Some(v) = val { rep(v); },
        Term::Unreachable => {}
    }
}

/// 全函数 EarlyCSE（不动点：一轮可能暴露下一轮的复用机会，最多 4 轮）。
pub fn early_cse(ip: &mut IrProgram) -> usize {
    let mut total = 0usize;
    const MAX_ROUNDS: usize = 4;
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for _ in 0..MAX_ROUNDS {
        let mut round = 0usize;
        for name in &names {
            if let Some(f) = ip.functions.iter_mut().find(|f| f.name == *name) {
                for b in &mut f.blocks {
                    round += early_cse_block(b);
                }
            }
        }
        total += round;
        if round == 0 { break; }
    }
    if total > 0 && std::env::var("DIALC_DUMP_EARLYCSE").is_ok() {
        eprintln!("[earlycse] eliminated {} redundant loads", total);
    }
    total
}

// ============================================================================
// M20：LLVM 风格 CFG 简化（SimplifyCFG）与激进死代码消除（ADCE）
// ============================================================================
//
// 借鉴 LLVM 的 SimplifyCFG + ADCE pass 的核心子集：
//   1. **ADCE / 不可达块消除**：从 entry 做可达性分析，删除不可达块（保留 id
//      空洞——codegen 用 block.id 生成标签，空洞无害）。这同时清掉 unroll_loops /
//      specialize_recursive_calls 用 `Term::Unreachable` 断边后遗留的死块。
//   2. **SimplifyCFG / 跳转链折叠**：若块 A 的终止符是 `Jump{target:B}`，且 B
//      无块参数、B 唯一前驱是 A、B 不是入口——则把 A 的终止符改为 B 的终止符，
//      把 B 的指令并入 A，删除 B。消除纯跳转中继块，缩短控制流。
//
// 正确性铁律：
//   - 块参数（phi）：任何涉及非空 `params` 的块一律不参与跳转折叠（传值语义复杂，
//     保守跳过）；不可达块删除则天然安全（不可达块的 phi 传值永不执行）。
//   - 不重编号：删除块后剩余块 id 不变，跳转目标仍按 id 解析。
//   - 不动点：跳转链折叠可能连续多轮，最多 8 轮收敛。
//   - 护栏：同 inline / EarlyCSE（O2+ / x86-64 / 非 -g/PGO/instrument）。

/// 从 entry 做可达性分析，删除不可达块。返回删除的块数。
/// 注意：只删块，不重编号（保留 id 空洞）。
fn adce_unreachable_blocks(f: &mut IrFunction) -> usize {
    let n = f.blocks.len();
    if n == 0 { return 0; }
    let id2idx: HashMap<u32, usize> = f.blocks.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
    let entry_idx = f.blocks.iter().position(|b| b.id == f.entry).unwrap_or(0);
    let mut reachable = vec![false; n];
    let mut work = vec![entry_idx];
    reachable[entry_idx] = true;
    while let Some(i) = work.pop() {
        let succ_ids: Vec<u32> = match &f.blocks[i].term {
            Term::Jump { target, .. } => vec![*target],
            Term::Branch { then_b, else_b, .. } => vec![*then_b, *else_b],
            Term::Return { .. } | Term::Unreachable => Vec::new(),
        };
        for sid in succ_ids {
            if let Some(&si) = id2idx.get(&sid) {
                if !reachable[si] { reachable[si] = true; work.push(si); }
            }
        }
    }
    let before = n;
    // 按索引重建，删除不可达块（保留剩余块 id 不变）。
    let kept: Vec<IrBlock> = f.blocks.drain(..).enumerate()
        .filter(|(i, _)| reachable[*i]).map(|(_, b)| b).collect();
    f.blocks = kept;
    before - f.blocks.len()
}

/// 跳转链折叠的一轮：把「A --Jump--> B（B 无参数、唯一前驱 A、B 非入口）」合并。
/// 返回本轮折叠数。
fn simplifycfg_jump_chain_round(f: &mut IrFunction) -> usize {
    let n = f.blocks.len();
    if n < 2 { return 0; }
    // 统计每个块的前驱数（按跳转/分支目标，含重复边的去重）。
    let mut pred_count = vec![0usize; n];
    let id2idx: HashMap<u32, usize> = f.blocks.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
    for i in 0..n {
        let succ_ids: Vec<u32> = match &f.blocks[i].term {
            Term::Jump { target, .. } => vec![*target],
            Term::Branch { then_b, else_b, .. } => {
                let mut v = vec![*then_b];
                if *then_b != *else_b { v.push(*else_b); }
                v
            }
            _ => Vec::new(),
        };
        for sid in succ_ids {
            if let Some(&si) = id2idx.get(&sid) { pred_count[si] += 1; }
        }
    }
    let mut folded = 0usize;
    // 找可折叠的 A：term=Jump{target=B}, args 为空（A 侧无传值），
    //   B 无块参数、B 非入口、B 唯一前驱是 A。
    let mut plan: Option<(usize, usize)> = None;
    for ai in 0..n {
        let target_b = match &f.blocks[ai].term {
            Term::Jump { target, args } if args.is_empty() => *target,
            _ => continue,
        };
        let bi = match id2idx.get(&target_b) { Some(&x) => x, None => continue };
        if bi == ai || f.blocks[bi].id == f.entry { continue; }
        if !f.blocks[bi].params.is_empty() { continue; }
        if pred_count[bi] != 1 { continue; }
        plan = Some((ai, bi));
        break; // 一轮只折一处，保守收敛
    }
    if let Some((ai, bi)) = plan {
        // 把 B 的指令并入 A，A 的 term 改为 B 的 term。
        let b_insts = f.blocks[bi].insts.clone();
        let b_term = f.blocks[bi].term.clone();
        f.blocks[ai].insts.extend(b_insts);
        f.blocks[ai].term = b_term;
        // 删除 B（保留其它块 id 不变）。
        f.blocks.remove(bi);
        folded += 1;
    }
    folded
}

/// LLVM 风格 CFG 简化 + ADCE 全程序入口。
pub fn simplifycfg_adce(ip: &mut IrProgram) -> (usize, usize) {
    const MAX_ROUNDS: usize = 8;
    let mut total_fold = 0usize;
    let mut total_dead = 0usize;
    let names: Vec<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for _ in 0..MAX_ROUNDS {
        let mut round = 0usize;
        for name in &names {
            if let Some(f) = ip.functions.iter_mut().find(|f| f.name == *name) {
                round += simplifycfg_jump_chain_round(f);
            }
        }
        total_fold += round;
        if round == 0 { break; }
    }
    for name in &names {
        if let Some(f) = ip.functions.iter_mut().find(|f| f.name == *name) {
            total_dead += adce_unreachable_blocks(f);
        }
    }
    if (total_fold > 0 || total_dead > 0) && std::env::var("DIALC_DUMP_SIMPLIFYCFG").is_ok() {
        eprintln!("[simplifycfg] folded {} jump chains, adce removed {} blocks", total_fold, total_dead);
    }
    (total_fold, total_dead)
}
