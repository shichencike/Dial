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
) -> Result<IrProgram, String> {
    let mut ip = IrProgram::new(functions_sig, structs);
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
    /// 函数参数槽（与 func.params 平行），供 prologue 填值。
    param_slots: Vec<VReg>,
}

impl<'a> Lower<'a> {
    fn new(ip: &'a mut IrProgram) -> Self {
        Lower {
            ip,
            func: IrFunction::new(String::new(), Vec::new(), IrType::Void, FuncLink::Private),
            cur: 0,
            scopes: Vec::new(),
            loops: Vec::new(),
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

    /// 释放从当前作用域深度 down_to（含）之前的所有作用域里的堆局部。
    fn release_scopes_upto(&mut self, down_to: usize) {
        while self.scopes.len() > down_to {
            let scope = self.scopes.pop().unwrap();
            self.release_scope(&scope);
        }
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
            self.emit(IrInst::EnvInit {
                argc: Value::Reg(a_ptr),
                argv: Value::Reg(v_ptr),
            });
        } else {
            for (n, t) in &f.params {
                // 内部宽度统一用 reg_ir（I64），栈槽 8 字节；emit 的 prologue 把
                // 边界宽度的实参寄存器填进来（写 32/8 位会自动零扩展到 64 位）。
                let slot = self.emit_alloca(reg_ir(t));
                let is_h = rc_kind(t).is_some();
                self.declare(n, Local { ptr: slot, ty: reg_ir(t), dt: t.clone(), is_handle: is_h, moved: false });
                self.param_slots.push(slot);
                // 堆形参入口 retain（与调用方共享所有权，对称借用）。
                if let Some(k) = rc_kind(t) {
                    self.emit(IrInst::RcRetain { ptr: slot, kind: k });
                } else if let Type::Struct(sn) = t {
                    self.emit(IrInst::StructRetain { ptr: slot, struct_name: sn.clone() });
                }
            }
        }

        for s in &f.body { self.lower_stmt(s)?; }

        if !self.cur_terminated() {
            self.release_scopes_upto(0);
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

        self.ip.functions.push(self.func.clone());
        Ok(())
    }

    // ---- 内存辅助 ----

    fn emit_alloca(&mut self, ty: IrType) -> VReg {
        let p = self.new_reg(IrType::Ptr);
        self.emit(IrInst::Alloca { dst: p, bytes: slot_bytes(ty) });
        p
    }

    /// 把一个值写入槽（含 ARC：释放旧值、按 is_copy 决定是否 retain 新值）。
    fn store_value(&mut self, slot: VReg, val: Value, val_is_copy: bool, val_dt: &Type) {
        if let Some(k) = rc_kind(val_dt) {
            self.emit(IrInst::RcRelease { ptr: slot, kind: k });
        } else if let Type::Struct(sn) = val_dt {
            self.emit(IrInst::StructRelease { ptr: slot, struct_name: sn.clone() });
        }
        if val_is_copy {
            if let Some(k) = rc_kind(val_dt) {
                self.emit(IrInst::RcRetain { ptr: slot, kind: k });
            }
        }
        self.emit(IrInst::Store { ptr: slot, val, ty: self.val_slot_ty(val_dt) });
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
                self.store_value(slot, v, is_copy, &dt);
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
                self.store_value(l.ptr, v, is_copy, &dt);
                if let Some(sl) = self.scopes.last_mut().unwrap().get_mut(name) { sl.moved = *move_hint; }
                if *move_hint {
                    if let Expr::Var(src_name) = value {
                        if let Some(sl) = self.scopes.last_mut().unwrap().get_mut(src_name) { sl.moved = true; }
                    }
                }
                Ok(())
            }
            Stmt::AssignField { obj, field, value, .. } => {
                let obj_name = match obj.as_ref() {
                    Expr::Var(n) => n.clone(),
                    _ => return self.err("asm 后端：AssignField 的 obj 仅支持对变量取字段"),
                };
                let base_loc = match self.lookup(&obj_name).cloned() {
                    Some(l) => l,
                    None => return self.err(&format!("未声明的变量 '{}'", obj_name)),
                };
                let (field_off, field_ty) = self.struct_field(&base_loc.dt, field)
                    .ok_or_else(|| format!("[asm 后端] 结构体 {} 无字段 {}", base_loc.dt.display(), field))?;
                let fa = self.new_reg(IrType::Ptr);
                self.emit(IrInst::FieldAddr { dst: fa, base: base_loc.ptr, struct_name: struct_name_of(&base_loc.dt), field: field_off as u32 });
                let (v, is_copy) = self.lower_expr(value, &field_ty)?;
                if let Some(k) = rc_kind(&field_ty) {
                    self.emit(IrInst::RcRelease { ptr: fa, kind: k });
                    if is_copy { self.emit(IrInst::RcRetain { ptr: fa, kind: k }); }
                }
                // 字段在内存里的宽度（field_ir）；bool 字段是 1 字节，需从内部 I64 截断。
                let mty = field_ir(&field_ty);
                let v_store = if mty != reg_ir(&field_ty) {
                    let t = self.new_reg(mty.clone());
                    self.emit(IrInst::Conv { kind: AConv::Trunc, dst: t, a: v, ty: mty.clone() });
                    Value::Reg(t)
                } else { v };
                self.emit(IrInst::Store { ptr: fa, val: v_store, ty: mty });
                Ok(())
            }
            Stmt::AssignDeref { .. } => self.err("asm 后端暂不支持裸指针解引用赋值（后续相位）"),
            Stmt::AssignIndex { .. } => self.err("asm 后端暂不支持数组元素赋值 `arr[i] = v`（后续相位）"),
            Stmt::If { cond, then_branch, else_branch, .. } => {
                let (cond_v, _) = self.lower_expr(cond, &Type::Bool)?;
                let then_b = self.new_block(Vec::new());
                let else_b = if else_branch.is_some() { self.new_block(Vec::new()) } else { 0 };
                let merge_b = self.new_block(Vec::new());
                let cond = self.cond_reg(cond_v)?;
                self.terminate(Term::Branch { cond, then_b, else_b, then_args: Vec::new(), else_args: Vec::new() });
                self.set_cur(then_b);
                self.push_scope();
                for st in then_branch { self.lower_stmt(st)?; }
                let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
                if !self.cur_terminated() { self.terminate(Term::Jump { target: merge_b, args: Vec::new() }); }
                if let Some(eb) = else_branch {
                    self.set_cur(else_b);
                    self.push_scope();
                    for st in eb { self.lower_stmt(st)?; }
                    let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
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
                let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
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
                let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
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
                let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
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
                let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
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
            Stmt::ForEachMap { .. } => self.err("asm 后端暂不支持 `for (k, v in m)`（后续相位）"),
            Stmt::Break(_, _) => {
                if let Some((_, exit, depth)) = self.loops.last().copied() {
                    self.release_scopes_upto(depth);
                    self.terminate(Term::Jump { target: exit, args: Vec::new() });
                    Ok(())
                } else { self.err("break 不在循环内") }
            }
            Stmt::Continue(_, _) => {
                if let Some((cont, _, depth)) = self.loops.last().copied() {
                    self.release_scopes_upto(depth);
                    self.terminate(Term::Jump { target: cont, args: Vec::new() });
                    Ok(())
                } else { self.err("continue 不在循环内") }
            }
            Stmt::Match { subject, arms, .. } => self.lower_match(subject, arms),
            Stmt::Defer(_, _, _) => self.err("asm 后端暂不支持 `defer`（后续相位）"),
            Stmt::With { .. } => self.err("asm 后端暂不支持 `with`（后续相位）"),
            Stmt::Label(_, _, _) => self.err("asm 后端暂不支持 `label`（后续相位）"),
            Stmt::Goto(_, _, _) => self.err("asm 后端暂不支持 `goto`（后续相位）"),
            Stmt::Return(val, _, _) => {
                self.release_scopes_upto(0);
                match val {
                    Some(e) => {
                        let dt = self.expr_dt(e);
                        let (v, _) = self.lower_expr(e, &dt)?;
                        let v = self.adapt_param(v, &dt, &dt);
                        self.terminate(Term::Return { val: Some(v) });
                    }
                    None => self.terminate(Term::Return { val: None }),
                }
                Ok(())
            }
            Stmt::Throw(_, _, _) => self.err("asm 后端暂不支持 `throw`（后续相位，异常 unwinding）"),
            Stmt::Try { .. } => self.err("asm 后端暂不支持 `try`（后续相位，异常 unwinding）"),
            Stmt::Select { .. } => self.err("asm 后端暂不支持 `select`（后续相位，通道并发）"),
            Stmt::Expr(e, _, _) => { self.lower_expr(e, &Type::Void)?; Ok(()) }
            Stmt::Block { body, .. } => {
                self.push_scope();
                for st in body { self.lower_stmt(st)?; }
                let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
                Ok(())
            }
        }
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
                    None => return self.err(&format!("未声明的变量 '{}'", name)),
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
                let (bv, _) = self.lower_expr(base, &Type::Void)?;
                let base_dt = self.expr_struct_dt(base);
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
                let (av, _) = self.lower_expr(arr, &Type::Array(Box::new(Type::Int)))?;
                let (iv, _) = self.lower_expr(idx, &Type::Int)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_get".to_string(), args: vec![av, iv], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            Expr::ArrayLit(elems, _) => {
                let mut h = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(h), func: "dial_array_new".to_string(), args: vec![Value::Imm(Imm::I64(0))], arg_tys: vec![IrType::I64], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                for el in elems {
                    let (ev, _) = self.lower_expr(el, &Type::Int)?;
                    let h2 = self.new_reg(IrType::Handle);
                    self.emit(IrInst::Call { dst: Some(h2), func: "dial_array_push".to_string(), args: vec![Value::Reg(h), ev], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                    h = h2;
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
                    if let Some(k) = rc_kind(&fty) { if is_copy { self.emit(IrInst::RcRetain { ptr: fa, kind: k }); } }
                    // 字段内存宽度 field_ir（bool=1B）；内部 I64 需截断。
                    let mty = field_ir(&fty);
                    let fv_store = if mty != reg_ir(&fty) {
                        let t = self.new_reg(mty.clone());
                        self.emit(IrInst::Conv { kind: AConv::Trunc, dst: t, a: fv, ty: mty.clone() });
                        Value::Reg(t)
                    } else { fv };
                    self.emit(IrInst::Store { ptr: fa, val: fv_store, ty: mty });
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
            Expr::TypeLit(_) => self.err("asm 后端暂不支持 `ptr_cast` 类型字面量（后续相位）"),
            Expr::FuncRef(_) => self.err("asm 后端暂不支持 `spawn`（后续相位，线程）"),
            // 其余（Slice / 未覆盖构造）：明确报错，不静默降级。
            _ => self.err("asm 后端暂不支持该表达式形态（后续相位）"),
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
                let addr = self.new_reg(IrType::Ptr);
                self.emit(IrInst::Load { dst: addr, ptr: av.as_reg(), ty: IrType::Ptr });
                let inner = da.strip_ptr();
                let r = self.new_reg(reg_ir(&inner));
                self.emit(IrInst::Load { dst: r, ptr: addr, ty: reg_ir(&inner) });
                Ok((Value::Reg(r), true))
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
        let (ov, _) = self.lower_expr(obj, &od)?;
        match (od, method) {
            (Type::String, "len") => {
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_string_length".to_string(), args: vec![ov], arg_tys: vec![IrType::Handle], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::String, "get") => {
                let (iv, _) = self.lower_expr(&args[0], &Type::Int)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_string_get".to_string(), args: vec![ov, iv], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::String, "substring") => {
                let (a, _) = self.lower_expr(&args[0], &Type::Int)?;
                let (b, _) = self.lower_expr(&args[1], &Type::Int)?;
                let r = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_string_substring".to_string(), args: vec![ov, a, b], arg_tys: vec![IrType::Handle, IrType::I64, IrType::I64], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::String, "eq") => {
                let (bv, _) = self.lower_expr(&args[0], &Type::String)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_string_eq".to_string(), args: vec![ov, bv], arg_tys: vec![IrType::Handle, IrType::Handle], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                let r1 = self.new_reg(IrType::I64);
                self.emit(IrInst::ICmp { op: ACmp::Neq, dst: r1, a: Value::Reg(r), b: Value::Imm(Imm::I64(0)), ty: IrType::I64 });
                Ok((Value::Reg(r1), false))
            }
            (Type::Array(_), "len") | (Type::Array(_), "size") => {
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_length".to_string(), args: vec![ov], arg_tys: vec![IrType::Handle], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::Array(_), "get") => {
                let (iv, _) = self.lower_expr(&args[0], &Type::Int)?;
                let r = self.new_reg(IrType::I64);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_get".to_string(), args: vec![ov, iv], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::I64, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::Array(_), "push") => {
                let (ev, _) = self.lower_expr(&args[0], &Type::Int)?;
                let r = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_push".to_string(), args: vec![ov, ev], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::Array(_), "set") => {
                let (iv, _) = self.lower_expr(&args[0], &Type::Int)?;
                let (ev, _) = self.lower_expr(&args[1], &Type::Int)?;
                let r = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_set".to_string(), args: vec![ov, iv, ev], arg_tys: vec![IrType::Handle, IrType::I64, IrType::I64], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::Array(_), "pop") => {
                let r = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_pop".to_string(), args: vec![ov], arg_tys: vec![IrType::Handle], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            (Type::Array(_), "remove") => {
                let (iv, _) = self.lower_expr(&args[0], &Type::Int)?;
                let r = self.new_reg(IrType::Handle);
                self.emit(IrInst::Call { dst: Some(r), func: "dial_array_remove".to_string(), args: vec![ov, iv], arg_tys: vec![IrType::Handle, IrType::I64], ret_ty: IrType::Handle, linkage: CallLink::Runtime });
                Ok((Value::Reg(r), false))
            }
            _ => self.err(&format!("asm 后端：方法 `{}` 暂不支持（后续相位）", method)),
        }
    }

    // ---- 函数调用 ----

    fn lower_call(&mut self, name: &str, args: &[Expr]) -> Result<(Value, bool), String> {
        if name == "print" || name == "eprint" || name == "print_raw" || name == "eprint_raw" || name == "write_char" {
            if args.len() != 1 { return self.err(&format!("{} 需要一个参数", name)); }
            return self.lower_print(name, &args[0]);
        }
        let sig = match self.ip.functions_sig.get(name) {
            Some(s) => s.clone(),
            None => return self.err(&format!("asm 后端：未找到函数 `{}`（FFI/内建未覆盖）", name)),
        };
        let ret_dt = sig.ret.clone();
        let mut arg_vals = Vec::new();
        let mut arg_tys = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let at = sig.params.get(i).cloned().unwrap_or(Type::Int);
            let (v, _) = self.lower_expr(a, &at)?;
            let v = self.adapt_param(v, &at, &at);
            arg_vals.push(v);
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

    fn lower_print(&mut self, name: &str, arg: &Expr) -> Result<(Value, bool), String> {
        let dt = self.expr_dt(arg);
        let (v, _) = self.lower_expr(arg, &dt)?;
        let (func, aty) = match dt {
            Type::Int | Type::Byte => ("dial_print_int", IrType::I64),
            Type::Float => ("dial_print_float", IrType::F64),
            Type::Int32 => ("dial_print_int", IrType::I32),
            Type::Float32 => ("dial_print_float", IrType::F32),
            Type::Bool => ("dial_print_bool", IrType::I64),
            Type::Char => ("dial_print_char", IrType::I64),
            Type::String | Type::Bytes => ("dial_print_string", IrType::Handle),
            _ => return self.err(&format!("asm 后端：不能直接 print {}", dt.display())),
        };
        let v = self.adapt_param(v, &dt, &dt);
        self.emit(IrInst::Call { dst: None, func: func.to_string(), args: vec![v], arg_tys: vec![aty], ret_ty: IrType::Void, linkage: CallLink::Runtime });
        let _ = name;
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
                                Type::Int => "dial_int_to_string",
                                Type::Float => "dial_float_to_string",
                                Type::Bool => "dial_bool_to_string",
                                Type::Char => "dial_char_to_string",
                                Type::Byte => "dial_byte_to_string",
                                _ => return self.err(&format!("asm 后端：插值不支持 {}", et.display())),
                            };
                            let farg = self.adapt_param(ev, &et, &et);
                            let faty = arg_ty_for_conv(&et);
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

    fn lower_match(&mut self, subject: &Expr, arms: &[MatchArm]) -> Result<(), String> {
        let sd = self.expr_dt(subject);
        let (sv, _) = self.lower_expr(subject, &sd)?;
        let merge = self.new_block(Vec::new());
        // 顺序 if 链（self.cur 已是当前块，逐分支串联）。
        let mut has_wild = false;
        for arm in arms {
            let (lit_v, cmp_op): (Value, ACmp) = match &arm.pattern {
                MatchPattern::Int(v) => (Value::Imm(Imm::I64(*v)), ACmp::Eq),
                MatchPattern::Bool(v) => (Value::Imm(Imm::I64(if *v { 1 } else { 0 })), ACmp::Eq),
                MatchPattern::Char(v) => (Value::Imm(Imm::I64(*v)), ACmp::Eq),
                MatchPattern::Str(_) => return self.err("asm 后端：match 字符串模式暂未实现（后续相位）"),
                MatchPattern::Wildcard => { has_wild = true; continue; }
            };
            let cmp = self.new_reg(IrType::I64);
            self.emit(IrInst::ICmp { op: cmp_op, dst: cmp, a: sv, b: lit_v, ty: IrType::I64 });
            let body_b = self.new_block(Vec::new());
            let next_b = self.new_block(Vec::new());
            self.terminate(Term::Branch { cond: cmp, then_b: body_b, else_b: next_b, then_args: Vec::new(), else_args: Vec::new() });
            self.set_cur(body_b);
            self.push_scope();
            for st in &arm.body { self.lower_stmt(st)?; }
            let scope = self.scopes.pop().unwrap();
            self.release_scope(&scope);
            if !self.cur_terminated() { self.terminate(Term::Jump { target: merge, args: Vec::new() }); }
            self.set_cur(next_b);
        }
        if has_wild {
            // 兜底：直接在 cursor 执行 _ 分支体。
            for arm in arms {
                if let MatchPattern::Wildcard = arm.pattern {
                    self.push_scope();
                    for st in &arm.body { self.lower_stmt(st)?; }
                    let scope = self.scopes.pop().unwrap();
                self.release_scope(&scope);
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
            Expr::IntLit(_) | Expr::CharLit(_) | Expr::ByteLit(_) => Type::Int,
            Expr::FloatLit(_) => Type::Float,
            Expr::BoolLit(_) => Type::Bool,
            Expr::StringLit(_) | Expr::Interp(_) => Type::String,
            Expr::Var(name) => self.lookup(name).map(|l| l.dt.clone()).unwrap_or(Type::Int),
            Expr::Binary(_, op, _) => match op {
                BinOp::And | BinOp::Or | BinOp::Eq | BinOp::Neq | BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => Type::Bool,
                _ => Type::Int,
            },
            Expr::Unary(op, _) => match op { UnOp::Not => Type::Bool, UnOp::Deref => e.strip_ptr_of(), _ => Type::Int },
            Expr::Ternary(..) => Type::Int,
            Expr::Call(name, _) => self.ip.functions_sig.get(name).map(|s| s.ret.clone()).unwrap_or(Type::Int),
            Expr::Index(..) => Type::Int,
            Expr::Field(base, field) => {
                let bd = self.expr_dt(base);
                if let Some((_, ft)) = self.struct_field(&bd, field) { ft } else { Type::Int }
            }
            Expr::Method(obj, method, _) => {
                let od = self.expr_dt(obj);
                match (od, method.as_str()) {
                    (_, "len" | "size" | "get") => Type::Int,
                    (_, "substring") => Type::String,
                    (o, "push" | "set" | "pop" | "remove") => o,
                    _ => Type::Int,
                }
            }
            Expr::StructLit(name, _) => Type::Struct(name.clone()),
            Expr::ArrayLit(_, _) => Type::Array(Box::new(Type::Int)),
            _ => Type::Int,
        }
    }

    fn expr_struct_dt(&self, e: &Expr) -> Type {
        match e {
            Expr::Var(name) => self.lookup(name).map(|l| l.dt.clone()).unwrap_or(Type::Struct("?".to_string())),
            Expr::Field(base, _) => self.expr_struct_dt(base),
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

fn slot_bytes(t: IrType) -> u32 {
    match t {
        IrType::I32 | IrType::F32 => 4,
        IrType::I1 | IrType::I8 => 1,
        _ => 8,
    }
}

fn arg_ty_for_conv(dt: &Type) -> IrType {
    match dt {
        Type::Int | Type::Byte | Type::Char => IrType::I64,
        Type::Float => IrType::F64,
        Type::Int32 => IrType::I32,
        Type::Float32 => IrType::F32,
        Type::Bool => IrType::I64,
        _ => IrType::I64,
    }
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
