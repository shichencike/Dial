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
use std::collections::HashMap;

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
) -> Result<IrProgram, String> {
    let mut ip = IrProgram::new(functions_sig, structs, goto_depths);
    for func in &prog.functions {
        let mut l = Lower::new(&mut ip);
        l.lower_function(func)?;
    }
    Ok(ip)
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
}

impl<'a> Lower<'a> {
    fn new(ip: &'a mut IrProgram) -> Self {
        let label_depths = ip.goto_depths.clone();
        Lower {
            ip,
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
    fn drain_defers_from(&mut self, depth: usize) -> Result<(), String> {
        loop {
            let hit = matches!(self.defers.last(), Some((d, _)) if *d >= depth);
            if !hit { break; }
            let (_, st) = self.defers.pop().unwrap();
            self.lower_stmt(&st)?;
        }
        Ok(())
    }

    /// 离开当前最内层作用域：LIFO defer + 释放该层堆局部。
    fn leave_scope(&mut self) -> Result<(), String> {
        let d = self.scopes.len();
        self.drain_defers_from(d)?;
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
            for (n, t) in &f.params {
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
                    self.emit(IrInst::StructRetain { ptr: content, struct_name: sn.clone() });
                    continue;
                }
                let slot = self.emit_alloca(reg_ir(t));
                let is_h = rc_kind(t).is_some();
                self.declare(n, Local { ptr: slot, ty: reg_ir(t), dt: t.clone(), is_handle: is_h, moved: false });
                self.param_slots.push(slot);
                // 堆形参入口 retain（与调用方共享所有权，对称借用）。
                if let Some(k) = rc_kind(t) {
                    self.emit(IrInst::RcRetain { ptr: slot, kind: k });
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

    fn lower_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::VarDecl { name, ty, value, move_hint, .. } => {
                let dt = ty.clone();
                let slot = self.emit_alloca(reg_ir(&dt));
                let (v, is_copy) = self.lower_expr(value, &dt)?;
                if let Type::Struct(_) = &dt {
                    // 值语义结构体：StructLit 返回一块临时栈区地址，整体拷进变量槽。
                    self.emit_memcpy(slot, v, self.struct_byte_size(&dt));
                } else {
                    self.store_value(slot, v, is_copy, &dt, false);
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
                self.store_value(l.ptr, v, is_copy, &dt, true);
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
                // 字段在内存里的宽度（field_ir）；bool 字段是 1 字节，需从内部 I64 截断。
                let mty = field_ir(&field_ty);
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
                if let Some(k) = rc_kind(&vdt) {
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
                let exit = self.new_block(Vec::new());
                self.terminate(Term::Jump { target: header, args: Vec::new() });
                self.set_cur(header);
                let cv = self.new_reg(IrType::I64);
                self.emit(IrInst::Load { dst: cv, ptr: slot, ty: IrType::I64 });
                let (ev, _) = self.lower_expr(end, &dt)?;
                let step_is_pos = matches!(step_v, Value::Imm(Imm::I64(n)) if n >= 0);
                let op = if step_is_pos { ACmp::Lt } else { ACmp::Gt };
                let cmp = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op, dst: cmp, a: Value::Reg(cv), b: ev, ty: IrType::I64 });
                self.terminate(Term::Branch { cond: cmp, then_b: body_b, else_b: exit, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(body_b);
                let body_depth = self.scopes.len();
                self.push_scope();
                self.loops.push((header, exit, body_depth));
                for st in body { self.lower_stmt(st)?; }
                self.loops.pop();
                self.leave_scope()?;
                if !self.cur_terminated() {
                    let cv2 = self.new_reg(IrType::I64);
                    self.emit(IrInst::Load { dst: cv2, ptr: slot, ty: IrType::I64 });
                    let nv = self.new_reg(IrType::I64);
                    self.emit(IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(cv2), b: step_v, ty: IrType::I64 });
                    self.emit(IrInst::Store { ptr: slot, val: Value::Reg(nv), ty: IrType::I64 });
                    self.terminate(Term::Jump { target: header, args: Vec::new() });
                }
                self.set_cur(exit);
                Ok(())
            }
            Stmt::ForEach { value, coll, body, .. } => {
                let (arr_v, _) = self.lower_expr(coll, &Type::Array(Box::new(Type::Int)))?;
                let idx_slot = self.emit_alloca(IrType::I64);
                self.emit(IrInst::Store { ptr: idx_slot, val: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                let len = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(len), func: "dial_array_length".to_string(), args: vec![arr_v], arg_tys: vec![IrType::Handle], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let val_slot = self.emit_alloca(IrType::I64);
                self.declare(value, Local { ptr: val_slot, ty: IrType::I64, dt: Type::Int, is_handle: false, moved: false });
                let header = self.new_block(Vec::new());
                let body_b = self.new_block(Vec::new());
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
                self.loops.push((header, exit, body_depth));
                let vv = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(vv), func: "dial_array_get".to_string(), args: vec![arr_v, Value::Reg(iv)], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                self.emit(IrInst::Store { ptr: val_slot, val: Value::Reg(vv), ty: IrType::I64 });
                for st in body { self.lower_stmt(st)?; }
                self.loops.pop();
                self.leave_scope()?;
                if !self.cur_terminated() {
                    let iv2 = self.new_reg(IrType::I64);
                    self.emit(IrInst::Load { dst: iv2, ptr: idx_slot, ty: IrType::I64 });
                    let nv = self.new_reg(IrType::I64);
                    self.emit(IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(iv2), b: Value::Imm(Imm::I64(1)), ty: IrType::I64 });
                    self.emit(IrInst::Store { ptr: idx_slot, val: Value::Reg(nv), ty: IrType::I64 });
                    self.terminate(Term::Jump { target: header, args: Vec::new() });
                }
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
                self.loops.push((header, exit, body_depth));
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
                    let iv2 = self.new_reg(IrType::I64);
                    self.emit(IrInst::Load { dst: iv2, ptr: idx_slot, ty: IrType::I64 });
                    let nv = self.new_reg(IrType::I64);
                    self.emit(IrInst::Bin { op: ABin::Add, dst: nv, a: Value::Reg(iv2), b: Value::Imm(Imm::I64(1)), ty: IrType::I64 });
                    self.emit(IrInst::Store { ptr: idx_slot, val: Value::Reg(nv), ty: IrType::I64 });
                    self.terminate(Term::Jump { target: header, args: Vec::new() });
                }
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
            Stmt::Select { .. } => self.err("asm 后端暂不支持 `select`（后续相位，通道并发）"),
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
                let p = self.new_reg(IrType::Ptr);
                self.emit(IrInst::LeaLabel { dst: p, lit });
                let h = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(h), func: "dial_string_new".to_string(), args: vec![Value::Reg(p)], arg_tys: vec![IrType::Ptr], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                Ok((Value::Reg(h), false)) // 新建对象 +1 自有，存槽不 retain
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
                        // 内存宽度 field_ir（bool=1B）；内部宽度 reg_ir（bool=I64）。
                        let mty = field_ir(other);
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
                    if let Some(k) = rc_kind(&ety) {
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
                for (fname, fe) in fields {
                    let (off, fty) = self.struct_field(&dt, fname)
                        .ok_or_else(|| format!("[asm 后端] 结构体 {} 无字段 {}", name, fname))?;
                    let fa = self.new_reg(IrType::Ptr);
                    self.emit(IrInst::FieldAddr { dst: fa, base: s2, struct_name: name.clone(), field: off as u32 });
                    let (fv, is_copy) = self.lower_expr(fe, &fty)?;
                    if let Type::Struct(_) = &fty {
                        // 嵌套结构体字段：按值拷贝**内容**（fv 是内容区地址），
                        // 不能只存 8 字节指针——那会让读侧把指针当成字段内容，
                        // 嵌套访问 r.origin.x 于是读到指针位模式（垃圾值）。
                        self.emit_memcpy(fa, fv, self.struct_byte_size(&fty));
                        // 值拷贝不带所有权：字段里的堆字段需对称 +1。
                        self.emit(IrInst::StructRetain { ptr: fa, struct_name: struct_name_of(&fty) });
                        continue;
                    }
                    // 字段内存宽度 field_ir（bool=1B）；内部 I64 需截断。
                    let mty = field_ir(&fty);
                    let fv_store = if mty != reg_ir(&fty) {
                        let t = self.new_reg(mty.clone());
                        self.emit(IrInst::Conv { kind: AConv::Trunc, dst: t, a: fv, ty: mty.clone() });
                        Value::Reg(t)
                    } else { fv };
                    self.emit(IrInst::Store { ptr: fa, val: fv_store, ty: mty });
                    if is_copy {
                        if let Some(k) = rc_kind(&fty) {
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
        let is_string_concat = op == BinOp::Add && self.expr_dt(a) == Type::String && self.expr_dt(b) == Type::String;
        if is_string_concat {
            let (av, _) = self.lower_expr(a, &Type::String)?;
            let (bv, _) = self.lower_expr(b, &Type::String)?;
            let r = self.new_reg(IrType::Handle);
            self.emit(IrInst::Call { dst: Some(r), func: "dial_string_concat".to_string(), args: vec![av, bv], arg_tys: vec![IrType::Handle, IrType::Handle], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
            return Ok((Value::Reg(r), false));
        }
        let da = self.expr_dt(a);
        let db = self.expr_dt(b);
        let is_float = matches!(da, Type::Float | Type::Float32) || matches!(db, Type::Float | Type::Float32);
        let (av, _) = self.lower_expr(a, &da)?;
        let (bv, _) = self.lower_expr(b, &db)?;
        match op {
            BinOp::And => {
                // 短路：a ? b : false
                let cc = self.cond_reg(av)?;
                let r = self.lower_value_if(cc, b, &Expr::BoolLit(false), &Type::Bool)?;
                Ok((Value::Reg(r), false))
            }
            BinOp::Or => {
                // 短路：a ? true : b
                let cc = self.cond_reg(av)?;
                let r = self.lower_value_if(cc, &Expr::BoolLit(true), b, &Type::Bool)?;
                Ok((Value::Reg(r), false))
            }
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
                let ty = if is_float { IrType::F64 } else { IrType::I64 };
                let r = self.new_reg(ty.clone());
                if is_float {
                    self.emit(IrInst::FBin { op: abin(op), dst: r, a: av, b: bv, ty: IrType::F64 });
                } else {
                    self.emit(IrInst::Bin { op: abin(op), dst: r, a: av, b: bv, ty: IrType::I64 });
                }
                Ok((Value::Reg(r), false))
            }
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
        // remove / pop 返回「被删元素」而非新数组指针，且要写回 COW 新指针。
        if matches!(od, Type::Array(_)) && matches!(method, "remove" | "pop") {
            return self.lower_array_remove(obj, &od, method, args);
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
        for (i, a) in args.iter().enumerate() {
            let abi = spec.args.get(i).copied().unwrap_or(builtin::Abi::Auto);
            let bit_mode = spec.elem_arg == Some(i) || spec.is_kv;
            let hint = if bit_mode || abi == builtin::Abi::Auto {
                self.expr_dt(a)
            } else {
                builtin::abi_hint(abi)
            };
            let (v, _) = self.lower_expr(a, &hint)?;
            let v2 = if bit_mode {
                // 数组元素 / KvMap 键与值：按实参静态类型转成 C 的 int64 位模式。
                self.coerce_elem_bits(v, &self.expr_dt(a))?
            } else {
                self.coerce_to_abi(v, a, abi).0
            };
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
            out = self.reinterpret_bits(out, reg_ir(&et))?;
        }
        // COW：只写回新句柄，绝不 release 旧值（C 侧已结算）。
        if let (Some(slot), Some(d)) = (obj_slot, dst) {
            self.emit(IrInst::Store { ptr: slot, val: Value::Reg(d), ty: IrType::Handle });
        }
        Ok((out, false))
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
            | "ptr_cast" =>
                return self.lower_special_call(name, args),
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
            let at = sig.params.get(i).cloned().unwrap_or(Type::Int);
            let (v, _) = self.lower_expr(a, &at)?;
            let v = self.adapt_param(v, &at, &at);
            if let Type::Struct(_) = &at {
                // 结构体实参按值传递：先把内容拷到调用方栈上的一块临时区，
                // 再把**临时区地址**传给被调方（被调方会 memcpy 出自己的副本）。
                // 直接传原内容区地址会让被调方在拷贝完成前看到调用方可能已改动的
                // 内容，且被调方的副本与调用方变量共享存储，破坏值语义。
                let tmp = self.emit_alloca(IrType::Struct(struct_name_of(&at)));
                self.emit_memcpy(tmp, v, self.struct_byte_size(&at));
                arg_vals.push(Value::Reg(tmp));
            } else {
                arg_vals.push(v);
            }
            arg_tys.push(param_ir(&at));
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
                    other => return self.err(&format!(
                        "asm 后端：close 暂不支持 {} 类型（仅 file；thread/channel 后续相位）", other.display())),
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
    fn coerce_elem_bits(&mut self, v: Value, dt: &Type) -> Result<Value, String> {
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
        let mut acc: Option<Value> = None;
        for part in parts {
            let seg: Value = match part {
                InterpPart::Text(t) => {
                    let lit = self.ip.intern_string(t);
                    let p = self.new_reg(IrType::Ptr);
                    self.emit(IrInst::LeaLabel { dst: p, lit });
                    let h = self.new_reg(IrType::Handle);
                    self.emit(IrInst::Call { dst: Some(h), func: "dial_string_new".to_string(), args: vec![Value::Reg(p)], arg_tys: vec![IrType::Ptr], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                    Value::Reg(h)
                }
                InterpPart::Expr(e) => {
                    let et = self.expr_dt(e);
                    let (ev, _) = self.lower_expr(e, &et)?;
                    match et {
                        Type::String | Type::Bytes => ev, // 已是字符串，直接复用
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
                            Value::Reg(h)
                        }
                    }
                }
            };
            acc = Some(match acc {
                None => seg,
                Some(prev) => {
                    let r = self.new_reg(IrType::Handle);
                    self.emit(IrInst::Call { dst: Some(r), func: "dial_string_concat".to_string(), args: vec![prev, seg], arg_tys: vec![IrType::Handle, IrType::Handle], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                    Value::Reg(r)
                }
            });
        }
        match acc {
            Some(v) => Ok((v, false)),
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
                        if matches!(da, Type::Float | Type::Float32) || matches!(db, Type::Float | Type::Float32) {
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
            Expr::Method(obj, method, _) => {
                let od = self.expr_dt(obj);
                // remove/pop 返回被删元素（不是新数组指针）。
                if matches!(method.as_str(), "remove" | "pop") {
                    if let Type::Array(e) = &od { return *e.clone(); }
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
