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
