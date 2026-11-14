//! 自研后端：IR → 目标汇编（.s）。
//!
//! 设计基线：**全 spill（full-spill）**。每个虚拟寄存器在栈帧里有一个固定的 8 字节槽，
//! 指令用少量固定的 caller-saved 临时寄存器搬运，不跨指令保持值。这牺牲一部分性能，
//! 但让「块参数（SSA phi）」「调用约定」「ARC 释放点」三件最容易出错的事变得可验证：
//! phi 就是在跳转前把实参写进目标块 params 对应的槽。
//!
//! 两个目标共享同一套 IR / Pass / 栈帧布局 / C ABI 参数分配，只有助记符与寄存器名分叉：
//!   - x86-64 System V AMD64（首版严格验证目标）
//!   - AArch64 AAPCS（同构实现；结构体一律按栈内值语义处理，绕开 HFA/HVA 规则）
//!
//! 栈对齐：x86 入口 rsp ≡ 8 (mod 16)，故 frame_size ≡ 8 (mod 16) 才能让 call 时
//! rsp ≡ 0；AArch64 入口 sp 已 16 对齐，frame_size 取 16 的倍数即可。

use crate::ir::*;
use crate::target::EmitTarget;
use std::collections::{HashMap, HashSet};

/// 参数落在哪个 C ABI 位置。
#[derive(Clone, Copy, Debug)]
enum ArgLoc { Int(usize), Fp(usize), Stack(usize) }

/// 按 C ABI 给一串参数分配位置：整型/指针与浮点各自独立编号，超限者按原顺序进栈。
fn abi_assign(tys: &[IrType], target: EmitTarget) -> Vec<ArgLoc> {
    let int_n = target.int_arg_regs().len();
    let fp_n = target.fp_arg_regs().len();
    let (mut ii, mut fi, mut si) = (0usize, 0usize, 0usize);
    tys.iter()
        .map(|t| {
            if t.is_float() {
                if fi < fp_n { let x = fi; fi += 1; ArgLoc::Fp(x) }
                else { let x = si; si += 1; ArgLoc::Stack(x) }
            } else if ii < int_n {
                let x = ii; ii += 1; ArgLoc::Int(x)
            } else { let x = si; si += 1; ArgLoc::Stack(x) }
        })
        .collect()
}

pub fn emit(ip: &IrProgram, target: EmitTarget) -> Result<String, String> {
    // 预检：FieldAddr.field 是字段序号，必须能按 struct_layout 解析出字节偏移。
    for f in &ip.functions {
        for b in &f.blocks {
            for i in &b.insts {
                if let IrInst::FieldAddr { struct_name, field, .. } = i {
                    let (offs, _, _) = ip.struct_layout(struct_name, false);
                    if (*field as usize) >= offs.len() {
                        return Err(format!(
                            "[asm 后端] 结构体 `{}` 的字段序号 {} 越界（共 {} 个字段）",
                            struct_name, field, offs.len()
                        ));
                    }
                }
            }
        }
    }

    let mut out = String::new();
    out.push_str("# 由 dialc 自研后端生成（IR -> .s）；请勿手工编辑\n");
    out.push_str("\t.section .rodata\n");
    for (idx, s) in ip.string_literals.iter().enumerate() {
        // 用 .byte 序列而非 .asciz，彻底规避引号/反斜杠/UTF-8 的转义歧义。
        let mut bytes: Vec<String> = s.as_bytes().iter().map(|b| b.to_string()).collect();
        bytes.push("0".to_string());
        out.push_str(&format!(".Lstr{}:\n\t.byte {}\n", idx, bytes.join(", ")));
    }
    out.push_str("\t.text\n");

    let local_funcs: HashSet<String> = ip.functions.iter().map(|f| f.name.clone()).collect();
    for f in &ip.functions {
        emit_function(&mut out, ip, f, target, &local_funcs)?;
    }
    // 避免 GNU ld 的 "missing .note.GNU-stack section implies executable stack" 警告。
    out.push_str("\t.section .note.GNU-stack,\"\",@progbits\n");
    Ok(out)
}

fn align_up(x: i64, a: i64) -> i64 { (x + a - 1) / a * a }

struct FnEmit<'a> {
    ip: &'a IrProgram,
    f: &'a IrFunction,
    target: EmitTarget,
    local_funcs: &'a HashSet<String>,
    /// vreg → 相对 rsp/sp 的字节偏移。
    reg_off: Vec<i64>,
    /// Alloca 定义的 vreg → 该栈区相对 rsp/sp 的偏移。
    alloca_off: HashMap<VReg, i64>,
    frame_size: i64,
    prefix: String,
    out: String,
}

impl<'a> FnEmit<'a> {
    fn new(ip: &'a IrProgram, f: &'a IrFunction, target: EmitTarget, local: &'a HashSet<String>) -> Self {
        let prefix = format!("Ldial_{}", {
            let s: String = f.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
            s
        });
        FnEmit {
            ip, f, target, local_funcs: local,
            reg_off: Vec::new(), alloca_off: HashMap::new(), frame_size: 0,
            prefix, out: String::new(),
        }
    }

    // ---- 基础工具 ----

    fn line(&mut self, s: &str) { self.out.push_str(s); self.out.push('\n'); }

    /// 寄存器名前缀：x86 用 `%`，AArch64 不加。调用方一律传「裸名」。
    fn r(&self, reg: &str) -> String {
        match self.target { EmitTarget::X86_64SysV => format!("%{}", reg), EmitTarget::AArch64 => reg.to_string() }
    }
    fn slot(&self, v: VReg) -> i64 { self.reg_off[v as usize] }
    fn rty(&self, v: VReg) -> IrType { self.f.reg_ty[v as usize].clone() }
    fn vty(&self, v: Value) -> IrType { v.ty(&self.f.reg_ty) }

    // 固定的临时寄存器（都不属于 C ABI 传参寄存器，避免互相踩踏）。
    fn g1(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV => "rax", EmitTarget::AArch64 => "x9" } }
    fn g2(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV => "r10", EmitTarget::AArch64 => "x10" } }
    fn g3(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV => "r11", EmitTarget::AArch64 => "x11" } }
    fn f1(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV => "xmm8", EmitTarget::AArch64 => "d16" } }
    fn f2(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV => "xmm9", EmitTarget::AArch64 => "d17" } }

    /// 32 位 / 8 位 / 浮点单精度视图。
    fn r32(&self, reg: &str) -> String {
        match self.target {
            EmitTarget::X86_64SysV => match reg {
                "rax" => "eax".into(), "rdi" => "edi".into(), "rsi" => "esi".into(),
                "rdx" => "edx".into(), "rcx" => "ecx".into(), "r8" => "r8d".into(),
                "r9" => "r9d".into(), "r10" => "r10d".into(), "r11" => "r11d".into(),
                other => other.to_string(),
            },
            EmitTarget::AArch64 => reg.replacen('x', "w", 1),
        }
    }
    fn r8(&self, reg: &str) -> String {
        match self.target {
            EmitTarget::X86_64SysV => match reg {
                "rax" => "al".into(), "rdi" => "dil".into(), "rsi" => "sil".into(),
                "rdx" => "dl".into(), "rcx" => "cl".into(), "r8" => "r8b".into(),
                "r9" => "r9b".into(), "r10" => "r10b".into(), "r11" => "r11b".into(),
                other => other.to_string(),
            },
            // AArch64 的 strb 用 w 视图。
            EmitTarget::AArch64 => self.r32(reg),
        }
    }
    fn fs(&self, reg: &str) -> String {
        match self.target {
            EmitTarget::X86_64SysV => reg.to_string(),         // movss 用 xmm 名
            EmitTarget::AArch64 => reg.replacen('d', "s", 1),  // d16 -> s16
        }
    }

    /// 把 64 位立即数装入整数寄存器（AArch64 用 movz/movk 拼接，不用 literal pool）。
    fn mov_imm64(&mut self, reg: &str, bits: u64) {
        match self.target {
            EmitTarget::X86_64SysV => {
                self.line(&format!("\tmovabsq ${}, {}", bits as i64, self.r(reg)));
            }
            EmitTarget::AArch64 => {
                let w = if reg.starts_with('x') { reg.to_string() } else { "x9".to_string() };
                self.line(&format!("\tmovz {}, #{}", w, bits & 0xffff));
                for sh in [16u32, 32, 48] {
                    let part = (bits >> sh) & 0xffff;
                    if part != 0 {
                        self.line(&format!("\tmovk {}, #{}, lsl #{}", w, part, sh));
                    }
                }
            }
        }
    }

    /// 把 Value（整数/指针/句柄）读入整数临时寄存器 `reg`，统一零扩展到 64 位。
    fn lg(&mut self, v: Value, reg: &str) {
        match v {
            Value::Imm(i) => {
                let n = match i {
                    Imm::I64(n) => n, Imm::I32(n) => n as i64, Imm::I8(n) => n as i64,
                    Imm::I1(b) => b as i64,
                    Imm::F64(bits) => bits as i64, Imm::F32(bits) => bits as i64,
                };
                self.mov_imm64(reg, n as u64);
            }
            Value::Reg(r) => {
                let off = self.slot(r);
                match self.target {
                    EmitTarget::X86_64SysV => self.line(&format!("\tmovq {}({}), {}", off, self.r("rsp"), self.r(reg))),
                    EmitTarget::AArch64 => self.line(&format!("\tldr {}, [sp, #{}]", reg, off)),
                }
            }
        }
    }

    /// 把 Value（浮点）读入浮点临时寄存器 `reg`。
    fn lf(&mut self, v: Value, reg: &str) {
        match v {
            Value::Imm(Imm::F64(bits)) => match self.target {
                EmitTarget::X86_64SysV => {
                    self.line(&format!("\tmovabsq ${}, {}", bits as i64, self.r("rax")));
                    self.line(&format!("\tmovq {}, {}", self.r("rax"), self.r(reg)));
                }
                EmitTarget::AArch64 => {
                    self.mov_imm64("x9", bits);
                    self.line(&format!("\tfmov {}, x9", reg));
                }
            },
            Value::Imm(Imm::F32(bits)) => match self.target {
                EmitTarget::X86_64SysV => {
                    self.line(&format!("\tmovl ${}, {}", bits as i32, self.r("eax")));
                    self.line(&format!("\tmovd {}, {}", self.r("eax"), self.r(reg)));
                }
                EmitTarget::AArch64 => {
                    self.mov_imm64("x9", bits as u64);
                    self.line(&format!("\tfmov {}, w9", self.fs(reg)));
                }
            },
            Value::Imm(i) => { // 整数常量出现在浮点上下文：按位模式搬移
                let n = match i { Imm::I64(n) => n, Imm::I32(n) => n as i64, Imm::I8(n) => n as i64, Imm::I1(b) => b as i64, _ => 0 };
                match self.target {
                    EmitTarget::X86_64SysV => {
                        self.line(&format!("\tmovabsq ${}, {}", n, self.r("rax")));
                        self.line(&format!("\tmovq {}, {}", self.r("rax"), self.r(reg)));
                    }
                    EmitTarget::AArch64 => { self.mov_imm64("x9", n as u64); self.line(&format!("\tfmov {}, x9", reg)); }
                }
            }
            Value::Reg(r) => {
                let off = self.slot(r);
                let f32 = self.rty(r) == IrType::F32;
                match self.target {
                    EmitTarget::X86_64SysV => {
                        let m = if f32 { "movss" } else { "movsd" };
                        self.line(&format!("\t{} {}({}), {}", m, off, self.r("rsp"), self.r(reg)));
                    }
                    EmitTarget::AArch64 => {
                        let rr = if f32 { self.fs(reg) } else { reg.to_string() };
                        self.line(&format!("\tldr {}, [sp, #{}]", rr, off));
                    }
                }
            }
        }
    }

    fn sg(&mut self, dst: VReg, reg: &str) {
        let off = self.slot(dst);
        match self.target {
            EmitTarget::X86_64SysV => self.line(&format!("\tmovq {}, {}({})", self.r(reg), off, self.r("rsp"))),
            EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", reg, off)),
        }
    }

    fn sf(&mut self, dst: VReg, reg: &str) {
        let off = self.slot(dst);
        let f32 = self.rty(dst) == IrType::F32;
        match self.target {
            EmitTarget::X86_64SysV => {
                let m = if f32 { "movss" } else { "movsd" };
                self.line(&format!("\t{} {}, {}({})", m, self.r(reg), off, self.r("rsp")));
            }
            EmitTarget::AArch64 => {
                let rr = if f32 { self.fs(reg) } else { reg.to_string() };
                self.line(&format!("\tstr {}, [sp, #{}]", rr, off));
            }
        }
    }

    /// 把任意 Value 写进某个 vreg 的槽（块参数传递 / 跳转实参用）。
    fn sv(&mut self, dst: VReg, v: Value) {
        if self.rty(dst).is_float() {
            let f = self.f1().to_string();
            self.lf(v, &f);
            self.sf(dst, &f);
        } else {
            let g = self.g1().to_string();
            self.lg(v, &g);
            self.sg(dst, &g);
        }
    }

    /// 取栈上某偏移的地址到整数寄存器。
    fn addr(&mut self, reg: &str, off: i64) {
        match self.target {
            EmitTarget::X86_64SysV => self.line(&format!("\tleaq {}({}), {}", off, self.r("rsp"), self.r(reg))),
            EmitTarget::AArch64 => {
                self.mov_imm64("x12", off as u64);
                self.line(&format!("\tadd {}, sp, x12", reg));
            }
        }
    }

    /// 从 `[addr_reg]` 按 ty 宽度读到 `reg`（整数用 g1，浮点用 f1）。
    fn load_mem(&mut self, addr_reg: &str, val_reg: &str, ty: IrType) {
        match self.target {
            EmitTarget::X86_64SysV => {
                let a = self.r(addr_reg);
                match ty {
                    IrType::I1 | IrType::I8 => self.line(&format!("\tmovzbq ({}), {}", a, self.r(val_reg))),
                    IrType::I32 => self.line(&format!("\tmovl ({}), {}", a, self.r(&self.r32(val_reg)))),
                    IrType::F64 => self.line(&format!("\tmovsd ({}), {}", a, self.r(val_reg))),
                    IrType::F32 => self.line(&format!("\tmovss ({}), {}", a, self.r(val_reg))),
                    _ => self.line(&format!("\tmovq ({}), {}", a, self.r(val_reg))),
                }
            }
            EmitTarget::AArch64 => {
                let a = addr_reg.to_string();
                let rr = match ty {
                    IrType::I1 | IrType::I8 | IrType::I32 => self.r32(val_reg),
                    IrType::F32 => self.fs(val_reg),
                    _ => val_reg.to_string(),
                };
                let m = if matches!(ty, IrType::I1 | IrType::I8) { "ldrb" } else { "ldr" };
                self.line(&format!("\t{} {}, [{}]", m, rr, a));
            }
        }
    }

    /// 把 `val_reg` 按 ty 宽度写到 `[addr_reg]`。
    fn store_mem(&mut self, addr_reg: &str, val_reg: &str, ty: IrType) {
        match self.target {
            EmitTarget::X86_64SysV => {
                let a = self.r(addr_reg);
                match ty {
                    IrType::I1 | IrType::I8 => self.line(&format!("\tmovb {}, ({})", self.r(&self.r8(val_reg)), a)),
                    IrType::I32 => self.line(&format!("\tmovl {}, ({})", self.r(&self.r32(val_reg)), a)),
                    IrType::F64 => self.line(&format!("\tmovsd {}, ({})", self.r(val_reg), a)),
                    IrType::F32 => self.line(&format!("\tmovss {}, ({})", self.r(val_reg), a)),
                    _ => self.line(&format!("\tmovq {}, ({})", self.r(val_reg), a)),
                }
            }
            EmitTarget::AArch64 => {
                let a = addr_reg.to_string();
                let rr = match ty {
                    IrType::I1 | IrType::I8 | IrType::I32 => self.r32(val_reg),
                    IrType::F32 => self.fs(val_reg),
                    _ => val_reg.to_string(),
                };
                let m = if matches!(ty, IrType::I1 | IrType::I8) { "strb" } else { "str" };
                self.line(&format!("\t{} {}, [{}]", m, rr, a));
            }
        }
    }

    // ---- 栈帧 ----

    fn compute_layout(&mut self) {
        let nreg = self.f.reg_ty.len() as i64;
        // outbound 区：所有 Call 里最多需要几个栈传参。
        let mut max_stack = 0usize;
        for b in &self.f.blocks {
            for i in &b.insts {
                if let IrInst::Call { arg_tys, .. } = i {
                    let n = abi_assign(arg_tys, self.target).iter()
                        .filter(|l| matches!(l, ArgLoc::Stack(_))).count();
                    max_stack = max_stack.max(n);
                }
            }
        }
        let outbound = (max_stack as i64) * 8;
        let mut off = outbound;
        for _ in 0..nreg { self.reg_off.push(off); off += 8; }
        for b in &self.f.blocks {
            for i in &b.insts {
                if let IrInst::Alloca { dst, bytes } = i {
                    self.alloca_off.insert(*dst, off);
                    off += align_up((*bytes as i64).max(8), 8);
                }
            }
        }
        let total = off;
        self.frame_size = match self.target {
            // 入口 rsp ≡ 8 (mod 16)；sub 后需 ≡ 0 (mod 16) 才能 call。
            EmitTarget::X86_64SysV => align_up(total, 16) + 8,
            EmitTarget::AArch64 => align_up(total, 16).max(16),
        };
    }

    fn prologue(&mut self) {
        let f = self.f;
        if f.linkage == FuncLink::External {
            self.line(&format!("\t.globl {}", f.name));
        }
        let lbl = format!("{}:", f.name);
        self.line(&lbl);
        match self.target {
            EmitTarget::X86_64SysV => self.line(&format!("\tsubq ${}, {}", self.frame_size, self.r("rsp"))),
            EmitTarget::AArch64 => {
                self.mov_imm64("x12", self.frame_size as u64);
                self.line("\tsub sp, sp, x12");
            }
        }
        // 把 C ABI 传参寄存器 / 栈参数写进 param_slots 对应的槽。
        let tys: Vec<IrType> = f.params.iter().map(|(_, t)| t.clone()).collect();
        let locs = abi_assign(&tys, self.target);
        for (idx, loc) in locs.iter().enumerate() {
            let slot = match f.param_slots.get(idx) { Some(s) => *s, None => continue };
            let ty = tys[idx].clone();
            // param_slots 里存的是 Alloca 的 dst vreg：它的「值」是栈区地址，
            // 真正的参数数据要写进那块 alloca 区（不是 vreg 自己的 8 字节槽）。
            let off = *self.alloca_off.get(&slot).unwrap_or(&0);
            let src_reg: String;
            match loc {
                ArgLoc::Int(k) => src_reg = self.target.int_arg_regs()[*k].to_string(),
                ArgLoc::Fp(k) => src_reg = self.target.fp_arg_regs()[*k].to_string(),
                ArgLoc::Stack(k) => {
                    // x86：call 压了返回地址，故被调函数看到的栈参数在 [rsp+frame+8+k*8]；
                    // AArch64：bl 不动 sp，栈参数就在 [sp+frame+k*8]。
                    let extra = match self.target { EmitTarget::X86_64SysV => 8, EmitTarget::AArch64 => 0 };
                    let so = self.frame_size + extra + (*k as i64) * 8;
                    let g = self.g1().to_string();
                    match self.target {
                        EmitTarget::X86_64SysV => self.line(&format!("\tmovq {}({}), {}", so, self.r("rsp"), self.r(&g))),
                        EmitTarget::AArch64 => self.line(&format!("\tldr {}, [sp, #{}]", g, so)),
                    }
                    self.sg(slot, &g);
                    continue;
                }
            }
            // 形参槽按 reg_ir 分配、统一 8 字节对齐；函数体里的 Load 也按 reg_ir 读
            // （bool/byte 是 I64，int32/float32 只读低 4 字节）。因此这里必须：
            //   · 窄整型先把 ABI 寄存器里的值零扩展到 64 位；
            //   · 一律写满 8 字节，绝不能只写 1/4 字节留下高字节垃圾。
            if ty.is_float() {
                let fr = match self.target {
                    EmitTarget::X86_64SysV => src_reg,
                    EmitTarget::AArch64 => src_reg.replacen('v', "d", 1),
                };
                match self.target {
                    EmitTarget::X86_64SysV =>
                        self.line(&format!("\tmovsd {}, {}({})", self.r(&fr), off, self.r("rsp"))),
                    EmitTarget::AArch64 =>
                        self.line(&format!("\tstr {}, [sp, #{}]", fr, off)),
                }
            } else {
                match self.target {
                    EmitTarget::X86_64SysV => {
                        match ty {
                            IrType::I32 =>
                                self.line(&format!("\tmovl {}, {}", self.r(&self.r32(&src_reg)), self.r(&self.r32(&src_reg)))),
                            IrType::I1 | IrType::I8 =>
                                self.line(&format!("\tmovzbl {}, {}", self.r(&self.r8(&src_reg)), self.r(&self.r32(&src_reg)))),
                            _ => {}
                        }
                        self.line(&format!("\tmovq {}, {}({})", self.r(&src_reg), off, self.r("rsp")));
                    }
                    EmitTarget::AArch64 => {
                        match ty {
                            IrType::I32 => self.line(&format!("\tmov {}, {}", self.r32(&src_reg), self.r32(&src_reg))),
                            IrType::I1 | IrType::I8 => self.line(&format!("\tand {}, {}, #255", src_reg, src_reg)),
                            _ => {}
                        }
                        self.line(&format!("\tstr {}, [sp, #{}]", src_reg, off));
                    }
                }
            }
        }
    }

    fn epilogue(&mut self) {
        match self.target {
            EmitTarget::X86_64SysV => self.line(&format!("\taddq ${}, {}", self.frame_size, self.r("rsp"))),
            EmitTarget::AArch64 => {
                self.mov_imm64("x12", self.frame_size as u64);
                self.line("\tadd sp, sp, x12");
            }
        }
        self.line(&format!("\t{}", self.target.ret_mnem()));
    }

    // ---- 指令 ----

    fn inst(&mut self, i: &IrInst) {
        match i {
            IrInst::Alloca { dst, .. } => {
                let off = *self.alloca_off.get(dst).unwrap_or(&0);
                let g = self.g2().to_string();
                self.addr(&g, off);
                self.sg(*dst, &g);
            }
            IrInst::LeaLabel { dst, lit } => {
                match self.target {
                    EmitTarget::X86_64SysV => {
                        self.line(&format!("\tleaq .Lstr{}({}), {}", lit, self.r("rip"), self.r(self.g2())));
                    }
                    EmitTarget::AArch64 => {
                        self.line(&format!("\tadrp {}, .Lstr{}", self.g3(), lit));
                        self.line(&format!("\tadd {}, {}, :lo12:.Lstr{}", self.g2(), self.g3(), lit));
                    }
                }
                let g = self.g2().to_string();
                self.sg(*dst, &g);
            }
            IrInst::FieldAddr { dst, base, struct_name, field } => {
                let (offs, _, _) = self.ip.struct_layout(struct_name, false);
                let byte_off = offs[*field as usize];
                let g = self.g2().to_string();
                self.lg(Value::Reg(*base), &g);
                match self.target {
                    EmitTarget::X86_64SysV => self.line(&format!("\taddq ${}, {}", byte_off, self.r(&g))),
                    EmitTarget::AArch64 => {
                        self.mov_imm64("x12", byte_off as u64);
                        self.line(&format!("\tadd {}, {}, x12", g, g));
                    }
                }
                self.sg(*dst, &g);
            }
            IrInst::PtrAdd { dst, base, off } => {
                // dst = base + off（off 为 64 位字节数，放在寄存器里）。
                let b = self.g2().to_string();
                let o = self.g1().to_string();
                self.lg(Value::Reg(*base), &b);
                self.lg(*off, &o);
                match self.target {
                    EmitTarget::X86_64SysV => self.line(&format!("\taddq {}, {}", self.r(&o), self.r(&b))),
                    EmitTarget::AArch64 => self.line(&format!("\tadd {}, {}, {}", o, b, b)),
                }
                self.sg(*dst, &b);
            }
            IrInst::Load { dst, ptr, ty } => {
                let a = self.g2().to_string();
                self.lg(Value::Reg(*ptr), &a);
                if ty.is_float() {
                    let f = self.f1().to_string();
                    self.load_mem(&a, &f, ty.clone());
                    self.sf(*dst, &f);
                } else {
                    let g = self.g1().to_string();
                    self.load_mem(&a, &g, ty.clone());
                    self.sg(*dst, &g);
                }
            }
            IrInst::Store { ptr, val, ty } => {
                let a = self.g2().to_string();
                self.lg(Value::Reg(*ptr), &a);
                if ty.is_float() {
                    let f = self.f1().to_string();
                    self.lf(*val, &f);
                    self.store_mem(&a, &f, ty.clone());
                } else {
                    let g = self.g1().to_string();
                    self.lg(*val, &g);
                    self.store_mem(&a, &g, ty.clone());
                }
            }
            IrInst::Bin { op, dst, a, b, ty } => self.inst_bin(*op, *dst, *a, *b, ty.clone()),
            IrInst::FBin { op, dst, a, b, ty } => self.inst_fbin(*op, *dst, *a, *b, ty.clone()),
            IrInst::ICmp { op, dst, a, b, .. } => self.inst_icmp(*op, *dst, *a, *b),
            IrInst::FCmp { op, dst, a, b, .. } => self.inst_fcmp(*op, *dst, *a, *b),
            IrInst::Un { op, dst, a, ty } => self.inst_un(*op, *dst, *a, ty.clone()),
            IrInst::Conv { kind, dst, a, ty } => self.inst_conv(*kind, *dst, *a, ty.clone()),
            IrInst::Call { dst, func, args, arg_tys, ret_ty, .. } =>
                self.inst_call(*dst, func, args, arg_tys, ret_ty.clone()),
            IrInst::CallIndirect { dst, target, args, arg_tys, ret_ty } =>
                self.inst_call_indirect(*dst, *target, args, arg_tys, ret_ty.clone()),
            IrInst::FuncAddr { dst, func } => {
                // 取函数代码地址：leaq <符号>(%rip), %reg（PIE 位置无关）。
                let g = self.g1().to_string();
                match self.target {
                    EmitTarget::X86_64SysV => self.line(&format!("\tleaq {}(%rip), {}", func, self.r(&g))),
                    EmitTarget::AArch64 => {
                        self.line(&format!("\tadrp {}, {}", g, func));
                        self.line(&format!("\tadd {}, {}, :lo12:{}", g, g, func));
                    }
                }
                self.sg(*dst, &g);
            }
            IrInst::RcRetain { ptr, kind } => self.inst_arc(*ptr, *kind, true),
            IrInst::RcRelease { ptr, kind } => self.inst_arc(*ptr, *kind, false),
            IrInst::StructRetain { ptr, struct_name } => self.inst_struct_arc(*ptr, struct_name, true),
            IrInst::StructRelease { ptr, struct_name } => self.inst_struct_arc(*ptr, struct_name, false),
            IrInst::EnvInit { argc, argv } => {
                let d = self.target.int_arg_regs()[0].to_string();
                let s = self.target.int_arg_regs()[1].to_string();
                self.lg(*argc, &d);
                self.lg(*argv, &s);
                self.do_call("dial_env_init");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn inst_bin(&mut self, op: ABin, dst: VReg, a: Value, b: Value, _ty: IrType) {
        let g1 = self.g1().to_string();
        let g2 = self.g2().to_string();
        self.lg(a, &g1);
        self.lg(b, &g2);
        match self.target {
            EmitTarget::X86_64SysV => {
                let a_reg = self.r(&g1); let b_reg = self.r(&g2);
                match op {
                    ABin::Add => self.line(&format!("\taddq {}, {}", b_reg, a_reg)),
                    ABin::Sub => self.line(&format!("\tsubq {}, {}", b_reg, a_reg)),
                    ABin::Mul => self.line(&format!("\timulq {}, {}", b_reg, a_reg)),
                    ABin::Div => { self.line("\tcqo"); self.line(&format!("\tidivq {}", b_reg)); }
                    ABin::Mod => { self.line("\tcqo"); self.line(&format!("\tidivq {}", b_reg)); self.line(&format!("\tmovq {}, {}", self.r("rdx"), a_reg)); }
                    ABin::BitAnd => self.line(&format!("\tandq {}, {}", b_reg, a_reg)),
                    ABin::BitOr => self.line(&format!("\torq {}, {}", b_reg, a_reg)),
                    ABin::BitXor => self.line(&format!("\txorq {}, {}", b_reg, a_reg)),
                    ABin::Shl => { self.line(&format!("\tmovq {}, {}", b_reg, self.r("rcx"))); self.line(&format!("\tshlq {}, {}", self.r("cl"), a_reg)); }
                    ABin::Shr => { self.line(&format!("\tmovq {}, {}", b_reg, self.r("rcx"))); self.line(&format!("\tsarq {}, {}", self.r("cl"), a_reg)); }
                    ABin::Eq | ABin::Neq | ABin::Lt | ABin::Gt | ABin::Le | ABin::Ge => {
                        self.line(&format!("\tcmpq {}, {}", b_reg, a_reg));
                        self.line(&format!("\t{} {}", x86_setcc(abin_cmp(op)), self.r("al")));
                        self.line(&format!("\tmovzbq {}, {}", self.r("al"), a_reg));
                    }
                    ABin::And => self.line(&format!("\tandq {}, {}", b_reg, a_reg)),
                    ABin::Or => self.line(&format!("\torq {}, {}", b_reg, a_reg)),
                }
                self.sg(dst, &g1);
            }
            EmitTarget::AArch64 => {
                match op {
                    ABin::Add => self.line(&format!("\tadd {}, {}, {}", g1, g1, g2)),
                    ABin::Sub => self.line(&format!("\tsub {}, {}, {}", g1, g1, g2)),
                    ABin::Mul => self.line(&format!("\tmul {}, {}, {}", g1, g1, g2)),
                    ABin::Div => self.line(&format!("\tsdiv {}, {}, {}", g1, g1, g2)),
                    ABin::Mod => {
                        self.line(&format!("\tsdiv x11, {}, {}", g1, g2));
                        self.line(&format!("\tmsub {}, x11, {}, {}", g1, g2, g1));
                    }
                    ABin::BitAnd => self.line(&format!("\tand {}, {}, {}", g1, g1, g2)),
                    ABin::BitOr => self.line(&format!("\torr {}, {}, {}", g1, g1, g2)),
                    ABin::BitXor => self.line(&format!("\teor {}, {}, {}", g1, g1, g2)),
                    ABin::Shl => self.line(&format!("\tlslv {}, {}, {}", g1, g1, g2)),
                    ABin::Shr => self.line(&format!("\tasrv {}, {}, {}", g1, g1, g2)),
                    ABin::Eq | ABin::Neq | ABin::Lt | ABin::Gt | ABin::Le | ABin::Ge => {
                        self.line(&format!("\tcmp {}, {}", g1, g2));
                        self.line(&format!("\tcset {}, {}", g1, arm_cond(abin_cmp(op))));
                    }
                    ABin::And => self.line(&format!("\tand {}, {}, {}", g1, g1, g2)),
                    ABin::Or => self.line(&format!("\torr {}, {}, {}", g1, g1, g2)),
                }
                self.sg(dst, &g1);
            }
        }
    }

    fn inst_fbin(&mut self, op: ABin, dst: VReg, a: Value, b: Value, _ty: IrType) {
        let f1 = self.f1().to_string();
        let f2 = self.f2().to_string();
        self.lf(a, &f1);
        self.lf(b, &f2);
        match self.target {
            EmitTarget::X86_64SysV => {
                let a_reg = self.r(&f1); let b_reg = self.r(&f2);
                match op {
                    ABin::Add => self.line(&format!("\taddsd {}, {}", b_reg, a_reg)),
                    ABin::Sub => self.line(&format!("\tsubsd {}, {}", b_reg, a_reg)),
                    ABin::Mul => self.line(&format!("\tmulsd {}, {}", b_reg, a_reg)),
                    ABin::Div => self.line(&format!("\tdivsd {}, {}", b_reg, a_reg)),
                    ABin::Mod => { // 浮点取模首版不走这里（lower 未生成）
                        self.line(&format!("\tdivsd {}, {}", b_reg, a_reg));
                    }
                    _ => self.line(&format!("\taddsd {}, {}", b_reg, a_reg)),
                }
                self.sf(dst, &f1);
            }
            EmitTarget::AArch64 => {
                match op {
                    ABin::Add => self.line(&format!("\tfadd {}, {}, {}", f1, f1, f2)),
                    ABin::Sub => self.line(&format!("\tfsub {}, {}, {}", f1, f1, f2)),
                    ABin::Mul => self.line(&format!("\tfmul {}, {}, {}", f1, f1, f2)),
                    ABin::Div => self.line(&format!("\tfdiv {}, {}, {}", f1, f1, f2)),
                    _ => self.line(&format!("\tfadd {}, {}, {}", f1, f1, f2)),
                }
                self.sf(dst, &f1);
            }
        }
    }

    fn inst_icmp(&mut self, op: ACmp, dst: VReg, a: Value, b: Value) {
        let g1 = self.g1().to_string();
        let g2 = self.g2().to_string();
        self.lg(a, &g1);
        self.lg(b, &g2);
        match self.target {
            EmitTarget::X86_64SysV => {
                self.line(&format!("\tcmpq {}, {}", self.r(&g2), self.r(&g1)));
                self.line(&format!("\t{} {}", x86_setcc(op), self.r("al")));
                self.line(&format!("\tmovzbq {}, {}", self.r("al"), self.r(&g1)));
            }
            EmitTarget::AArch64 => {
                self.line(&format!("\tcmp {}, {}", g1, g2));
                self.line(&format!("\tcset {}, {}", g1, arm_cond(op)));
            }
        }
        self.sg(dst, &g1);
    }

    fn inst_fcmp(&mut self, op: ACmp, dst: VReg, a: Value, b: Value) {
        let f1 = self.f1().to_string();
        let f2 = self.f2().to_string();
        self.lf(a, &f1);
        self.lf(b, &f2);
        match self.target {
            EmitTarget::X86_64SysV => {
                self.line(&format!("\tucomisd {}, {}", self.r(&f2), self.r(&f1)));
                self.line(&format!("\t{} {}", x86_float_setcc(op), self.r("al")));
                self.line(&format!("\tmovzbq {}, {}", self.r("al"), self.r(self.g1())));
            }
            EmitTarget::AArch64 => {
                self.line(&format!("\tfcmp {}, {}", f1, f2));
                self.line(&format!("\tcset {}, {}", self.g1(), arm_cond(op)));
            }
        }
        self.sg(dst, &self.g1().to_string());
    }

    fn inst_un(&mut self, op: AUn, dst: VReg, a: Value, ty: IrType) {
        if ty.is_float() {
            let f1 = self.f1().to_string();
            self.lf(a, &f1);
            match self.target {
                EmitTarget::X86_64SysV => {
                    self.line(&format!("\tmovabsq $0, {}", self.r("rax")));
                    self.line(&format!("\tmovq {}, {}", self.r("rax"), self.r("xmm9")));
                    self.line(&format!("\tsubsd {}, {}", self.r("xmm9"), self.r(&f1)));
                }
                EmitTarget::AArch64 => {
                    self.line("\tfmov d17, xzr");
                    self.line(&format!("\tfsub {}, d17, {}", f1, f1));
                }
            }
            self.sf(dst, &f1);
            return;
        }
        let g1 = self.g1().to_string();
        self.lg(a, &g1);
        match self.target {
            EmitTarget::X86_64SysV => match op {
                AUn::Neg => self.line(&format!("\tnegq {}", self.r(&g1))),
                AUn::Not => { self.line(&format!("\ttestq {}, {}", self.r(&g1), self.r(&g1))); self.line(&format!("\tsete {}", self.r("al"))); self.line(&format!("\tmovzbq {}, {}", self.r("al"), self.r(&g1))); }
                AUn::BitNot => self.line(&format!("\tnotq {}", self.r(&g1))),
            },
            EmitTarget::AArch64 => match op {
                AUn::Neg => self.line(&format!("\tneg {}, {}", g1, g1)),
                AUn::Not => { self.line(&format!("\tcmp {}, #0", g1)); self.line(&format!("\tcset {}, eq", g1)); }
                AUn::BitNot => self.line(&format!("\tmvn {}, {}", g1, g1)),
            },
        }
        self.sg(dst, &g1);
    }

    fn inst_conv(&mut self, kind: AConv, dst: VReg, a: Value, ty: IrType) {
        let src_ty = self.vty(a);
        match kind {
            AConv::SiFp => {
                let g1 = self.g1().to_string();
                let f1 = self.f1().to_string();
                self.lg(a, &g1);
                match self.target {
                    EmitTarget::X86_64SysV => {
                        let m = if ty == IrType::F32 { "cvtsi2ssq" } else { "cvtsi2sdq" };
                        self.line(&format!("\t{} {}, {}", m, self.r(&g1), self.r(&f1)));
                    }
                    EmitTarget::AArch64 => {
                        let rr = if ty == IrType::F32 { self.fs(&f1) } else { f1.clone() };
                        self.line(&format!("\tscvtf {}, {}", rr, g1));
                    }
                }
                self.sf(dst, &f1);
            }
            AConv::FpSi => {
                let f1 = self.f1().to_string();
                let g1 = self.g1().to_string();
                self.lf(a, &f1);
                match self.target {
                    EmitTarget::X86_64SysV => {
                        let m = if src_ty == IrType::F32 { "cvttss2siq" } else { "cvttsd2siq" };
                        self.line(&format!("\t{} {}, {}", m, self.r(&f1), self.r(&g1)));
                    }
                    EmitTarget::AArch64 => {
                        let rr = if src_ty == IrType::F32 { self.fs(&f1) } else { f1 };
                        self.line(&format!("\tfcvtzs {}, {}", g1, rr));
                    }
                }
                self.sg(dst, &g1);
            }
            AConv::FpExt => {
                let f1 = self.f1().to_string();
                self.lf(a, &f1);
                match self.target {
                    EmitTarget::X86_64SysV => self.line(&format!("\tcvtss2sd {}, {}", self.r(&f1), self.r(&f1))),
                    EmitTarget::AArch64 => self.line(&format!("\tfcvt {}, {}", f1, self.fs(&f1))),
                }
                self.sf(dst, &f1);
            }
            AConv::FpTrunc => {
                let f1 = self.f1().to_string();
                self.lf(a, &f1);
                match self.target {
                    EmitTarget::X86_64SysV => self.line(&format!("\tcvtsd2ss {}, {}", self.r(&f1), self.r(&f1))),
                    EmitTarget::AArch64 => self.line(&format!("\tfcvt {}, {}", self.fs(&f1), f1)),
                }
                self.sf(dst, &f1);
            }
            // 整数之间：槽统一 8 字节，Zext/Sext 直接搬运，Trunc 按目标宽度掩码。
            AConv::Sext | AConv::Zext | AConv::Trunc => {
                let g1 = self.g1().to_string();
                self.lg(a, &g1);
                // Sext 必须真正符号扩展：窄值在槽里是零扩展存放的（int32 负数需还原符号）。
                if kind == AConv::Sext {
                    match src_ty {
                        IrType::I32 => match self.target {
                            EmitTarget::X86_64SysV => self.line(&format!("\tmovslq {}, {}", self.r(&self.r32(&g1)), self.r(&g1))),
                            EmitTarget::AArch64 => self.line(&format!("\tsxtw {}, {}", g1, self.r32(&g1))),
                        },
                        IrType::I1 | IrType::I8 => match self.target {
                            EmitTarget::X86_64SysV => self.line(&format!("\tmovsbq {}, {}", self.r(&self.r8(&g1)), self.r(&g1))),
                            EmitTarget::AArch64 => self.line(&format!("\tsxtb {}, {}", g1, self.r32(&g1))),
                        },
                        _ => {}
                    }
                }
                if kind == AConv::Trunc {
                    match ty {
                        IrType::I32 => match self.target {
                            EmitTarget::X86_64SysV => self.line(&format!("\tmovl {}, {}", self.r(&self.r32(&g1)), self.r(&self.r32(&g1)))),
                            EmitTarget::AArch64 => self.line(&format!("\tmov {}, {}", self.r32(&g1), self.r32(&g1))),
                        },
                        IrType::I1 | IrType::I8 => match self.target {
                            EmitTarget::X86_64SysV => self.line(&format!("\tandq $255, {}", self.r(&g1))),
                            EmitTarget::AArch64 => self.line(&format!("\tand {}, {}, #255", g1, g1)),
                        },
                        _ => {}
                    }
                }
                self.sg(dst, &g1);
            }
        }
    }

    /// ARC 原语：`ptr` 是「存放句柄的槽地址」，先从槽里取出句柄，再调 C 运行时。
    fn inst_arc(&mut self, ptr: VReg, kind: ArcKind, retain: bool) {
        let a = self.g2().to_string();
        self.lg(Value::Reg(ptr), &a); // 槽地址
        let sym = arc_symbol(kind, retain);
        match self.target {
            EmitTarget::X86_64SysV => {
                self.line(&format!("\tmovq ({}), {}", self.r(&a), self.r(self.target.int_arg_regs()[0])));
            }
            EmitTarget::AArch64 => {
                self.line(&format!("\tldr {}, [{}]", self.target.int_arg_regs()[0], a));
            }
        }
        self.do_call(sym);
    }

    /// 结构体 ARC：遍历值语义结构体里的句柄字段，逐个 retain/release。
    fn inst_struct_arc(&mut self, ptr: VReg, struct_name: &str, retain: bool) {
        let (offs, _, _) = self.ip.struct_layout(struct_name, false);
        let fields = match self.ip.structs.get(struct_name) {
            Some((f, _)) => f.clone(), None => return,
        };
        let a = self.g2().to_string();
        self.lg(Value::Reg(ptr), &a); // 结构体基址
        for (idx, (_, ft)) in fields.iter().enumerate() {
            let k = match handle_arc_kind(ft) { Some(k) => k, None => continue };
            let off = offs.get(idx).copied().unwrap_or(0);
            let sym = arc_symbol(k, retain);
            let arg = self.target.int_arg_regs()[0].to_string();
            match self.target {
                EmitTarget::X86_64SysV => {
                    self.line(&format!("\tmovq {}({}), {}", off, self.r(&a), self.r(&arg)));
                }
                EmitTarget::AArch64 => {
                    self.mov_imm64("x12", off as u64);
                    self.line(&format!("\tadd x12, {}, x12", a));
                    self.line(&format!("\tldr {}, [x12]", arg));
                }
            }
            self.do_call(sym);
        }
    }

    fn inst_call(&mut self, dst: Option<VReg>, func: &str, args: &[Value], arg_tys: &[IrType], ret_ty: IrType) {
        let locs = abi_assign(arg_tys, self.target);
        let g1 = self.g1().to_string();

        // 1) 先写栈传参（低位 outbound 区），避免后续寄存器装载踩到它。
        for (idx, loc) in locs.iter().enumerate() {
            if let ArgLoc::Stack(k) = loc {
                let off = (*k as i64) * 8;
                if arg_tys[idx].is_float() {
                    let f = self.f1().to_string();
                    self.lf(args[idx], &f);
                    match self.target {
                        EmitTarget::X86_64SysV => self.line(&format!("\tmovsd {}, {}({})", self.r(&f), off, self.r("rsp"))),
                        EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", f, off)),
                    }
                } else {
                    self.lg(args[idx], &g1);
                    match self.target {
                        EmitTarget::X86_64SysV => self.line(&format!("\tmovq {}, {}({})", self.r(&g1), off, self.r("rsp"))),
                        EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", g1, off)),
                    }
                }
            }
        }
        // 2) 再把寄存器参数直接装进各自的 ABI 寄存器（源都是栈槽/立即数，互不干扰）。
        for (idx, loc) in locs.iter().enumerate() {
            match loc {
                ArgLoc::Int(k) => {
                    let reg = self.target.int_arg_regs()[*k].to_string();
                    self.lg(args[idx], &reg);
                }
                ArgLoc::Fp(k) => {
                    let raw = self.target.fp_arg_regs()[*k].to_string();
                    let reg = match self.target {
                        EmitTarget::X86_64SysV => raw.clone(),
                        // AArch64 的 vN 在标量与双精度视图里用 dN。
                        EmitTarget::AArch64 => raw.replacen('v', "d", 1),
                    };
                    let is_f32 = arg_tys[idx] == IrType::F32;
                    self.lf(args[idx], &reg);
                    if is_f32 {
                        // lf 对 Reg 已按 F32 用 movss / ldr s；对立即数装在 fpr 低 32 位，OK。
                    }
                    let _ = reg;
                }
                ArgLoc::Stack(_) => {}
            }
        }
        // 3) SysV variadic 约定：%al = 使用的向量寄存器个数。对非 variadic 无副作用。
        if self.target == EmitTarget::X86_64SysV {
            self.line(&format!("\txorl {}, {}", self.r("eax"), self.r("eax")));
        }
        self.do_call(func);

        // 4) 返回值落回 dst 槽。
        if let Some(d) = dst {
            if ret_ty.is_float() {
                let rr = match self.target {
                    EmitTarget::X86_64SysV => "xmm0".to_string(),
                    EmitTarget::AArch64 => "d0".to_string(),
                };
                self.sf(d, &rr);
            } else {
                let rr = self.target.ret_int_reg().to_string();
                self.sg(d, &rr);
            }
        }
    }

    /// 间接调用：实参按同一套 ABI 规则装载，最后 `call *%reg`。
    /// 与直接调用的差别只在「调用目标来自寄存器而非符号名」。
    fn inst_call_indirect(
        &mut self,
        dst: Option<VReg>,
        target: VReg,
        args: &[Value],
        arg_tys: &[IrType],
        ret_ty: IrType,
    ) {
        let locs = abi_assign(arg_tys, self.target);
        let g1 = self.g1().to_string();
        // 1) 栈传参（低位 outbound 区）
        for (idx, loc) in locs.iter().enumerate() {
            if let ArgLoc::Stack(k) = loc {
                let off = (*k as i64) * 8;
                if arg_tys[idx].is_float() {
                    let f = self.f1().to_string();
                    self.lf(args[idx], &f);
                    match self.target {
                        EmitTarget::X86_64SysV => self.line(&format!("\tmovsd {}, {}({})", self.r(&f), off, self.r("rsp"))),
                        EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", f, off)),
                    }
                } else {
                    self.lg(args[idx], &g1);
                    match self.target {
                        EmitTarget::X86_64SysV => self.line(&format!("\tmovq {}, {}({})", self.r(&g1), off, self.r("rsp"))),
                        EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", g1, off)),
                    }
                }
            }
        }
        // 2) 寄存器参数
        for (idx, loc) in locs.iter().enumerate() {
            match loc {
                ArgLoc::Int(k) => {
                    let reg = self.target.int_arg_regs()[*k].to_string();
                    self.lg(args[idx], &reg);
                }
                ArgLoc::Fp(k) => {
                    let raw = self.target.fp_arg_regs()[*k].to_string();
                    let reg = match self.target {
                        EmitTarget::X86_64SysV => raw.clone(),
                        EmitTarget::AArch64 => raw.replacen('v', "d", 1),
                    };
                    self.lf(args[idx], &reg);
                }
                ArgLoc::Stack(_) => {}
            }
        }
        // 3) 调用目标放进一个**不与实参寄存器冲突**的寄存器。
        //    x86-64 用 %r11（caller-saved，且不在 int_arg_regs 里）；
        //    AArch64 用 x16（IP0， caller-saved）。
        // 裸寄存器名（lg / sg 内部会自己加 '%' 前缀）。
        let call_reg = match self.target {
            EmitTarget::X86_64SysV => "r11",
            EmitTarget::AArch64 => "x16",
        };
        self.lg(Value::Reg(target), call_reg);
        match self.target {
            EmitTarget::X86_64SysV => {
                // SysV variadic 约定：%al = 使用的向量寄存器个数（对非 variadic 无副作用）。
                self.line(&format!("\txorl {}, {}", self.r("eax"), self.r("eax")));
                self.line(&format!("\tcall *{}", self.r(call_reg)));
            }
            EmitTarget::AArch64 => {
                self.line(&format!("\tblr {}", call_reg));
            }
        }
        // 4) 返回值落回 dst 槽。
        if let Some(d) = dst {
            if ret_ty.is_float() {
                let rr = match self.target {
                    EmitTarget::X86_64SysV => "xmm0".to_string(),
                    EmitTarget::AArch64 => "d0".to_string(),
                };
                self.sf(d, &rr);
            } else {
                let rr = self.target.ret_int_reg().to_string();
                self.sg(d, &rr);
            }
        }
    }

    /// 发一条调用：本编译单元内定义的 Dial 函数用直接 call，其余外部 C 符号走 PLT。
    fn do_call(&mut self, func: &str) {
        match self.target {
            EmitTarget::X86_64SysV => {
                if self.local_funcs.contains(func) {
                    self.line(&format!("\tcall {}", func));
                } else {
                    self.line(&format!("\tcall {}@PLT", func));
                }
            }
            EmitTarget::AArch64 => self.line(&format!("\tbl {}", func)),
        }
    }

    // ---- 终结符 ----

    fn term(&mut self, block_id: u32, t: &Term) {
        let jmp_mnem = match self.target {
            EmitTarget::X86_64SysV => "jmp",
            EmitTarget::AArch64 => "b",
        };
        match t {
            Term::Jump { target, args } => {
                self.write_block_args(*target, args);
                self.line(&format!("\t{} .{}_b{}", jmp_mnem, self.prefix, target));
            }
            Term::Branch { cond, then_b, else_b, then_args, else_args } => {
                let g1 = self.g1().to_string();
                self.lg(Value::Reg(*cond), &g1);
                let else_lbl = format!(".{}_b{}_else", self.prefix, block_id);
                match self.target {
                    EmitTarget::X86_64SysV => {
                        self.line(&format!("\ttestq {}, {}", self.r(&g1), self.r(&g1)));
                        self.line(&format!("\tjz {}", else_lbl));
                    }
                    EmitTarget::AArch64 => self.line(&format!("\tcbz {}, {}", g1, else_lbl)),
                }
                self.write_block_args(*then_b, then_args);
                self.line(&format!("\t{} .{}_b{}", jmp_mnem, self.prefix, then_b));
                self.line(&format!("{}:", else_lbl));
                self.write_block_args(*else_b, else_args);
                self.line(&format!("\t{} .{}_b{}", jmp_mnem, self.prefix, else_b));
            }
            Term::Return { val } => {
                if let Some(v) = val {
                    if self.f.ret.is_float() {
                        let rr = match self.target {
                            EmitTarget::X86_64SysV => "xmm0".to_string(),
                            EmitTarget::AArch64 => "d0".to_string(),
                        };
                        self.lf(*v, &rr);
                    } else {
                        self.lg(*v, self.target.ret_int_reg());
                    }
                }
                self.epilogue();
            }
            Term::Unreachable => match self.target {
                EmitTarget::X86_64SysV => self.line("\tud2"),
                EmitTarget::AArch64 => self.line("\tbrk #0"),
            },
        }
    }

    /// 把跳转实参写进目标块 params 的槽（full-spill 版的 phi）。
    fn write_block_args(&mut self, target: u32, args: &[Value]) {
        if args.is_empty() { return; }
        let params: Vec<VReg> = self.f.blocks.iter()
            .find(|b| b.id == target)
            .map(|b| b.params.iter().map(|(v, _)| *v).collect())
            .unwrap_or_default();
        for (i, a) in args.iter().enumerate() {
            if let Some(dst) = params.get(i) { self.sv(*dst, *a); }
        }
    }
}

// ============================================================================
// 指令助记符映射
// ============================================================================

fn abin_cmp(op: ABin) -> ACmp {
    match op {
        ABin::Eq => ACmp::Eq, ABin::Neq => ACmp::Neq, ABin::Lt => ACmp::Lt,
        ABin::Gt => ACmp::Gt, ABin::Le => ACmp::Le, ABin::Ge => ACmp::Ge,
        _ => ACmp::Eq,
    }
}

fn x86_setcc(op: ACmp) -> &'static str {
    match op {
        ACmp::Eq => "sete", ACmp::Neq => "setne", ACmp::Lt => "setl",
        ACmp::Gt => "setg", ACmp::Le => "setle", ACmp::Ge => "setge",
    }
}

fn x86_float_setcc(op: ACmp) -> &'static str {
    // ucomisd dst, src 之后：CF=1 表示 dst < src；ZF=1 表示相等。
    match op {
        ACmp::Eq => "sete", ACmp::Neq => "setne", ACmp::Lt => "setb",
        ACmp::Gt => "seta", ACmp::Le => "setbe", ACmp::Ge => "setae",
    }
}

fn arm_cond(op: ACmp) -> &'static str {
    match op {
        ACmp::Eq => "eq", ACmp::Neq => "ne", ACmp::Lt => "lt",
        ACmp::Gt => "gt", ACmp::Le => "le", ACmp::Ge => "ge",
    }
}

fn arc_symbol(kind: ArcKind, retain: bool) -> &'static str {
    match (kind, retain) {
        (ArcKind::String, true) => "dial_string_retain",
        (ArcKind::String, false) => "dial_string_release",
        (ArcKind::Array, true) => "dial_array_retain",
        (ArcKind::Array, false) => "dial_array_release",
        (ArcKind::Error, true) => "dial_error_retain",
        (ArcKind::Error, false) => "dial_error_release",
        (ArcKind::Bytes, true) => "dial_bytes_retain",
        (ArcKind::Bytes, false) => "dial_bytes_release",
        (ArcKind::Generic, true) => "dial_rc_retain",
        (ArcKind::Generic, false) => "dial_rc_release",
    }
}

/// 结构体字段若是不透明堆句柄，返回其 ARC 种类（与 lower.rs 的 rc_kind 同构）。
fn handle_arc_kind(t: &crate::ast::Type) -> Option<ArcKind> {
    use crate::ast::Type as T;
    match t {
        T::String => Some(ArcKind::String),
        T::Bytes => Some(ArcKind::Bytes),
        T::Array(_) => Some(ArcKind::Array),
        T::Error => Some(ArcKind::Error),
        T::Map | T::KvMap(..) | T::Set(_) | T::Channel(_) => Some(ArcKind::Generic),
        _ => None,
    }
}

// ============================================================================
// 函数发射
// ============================================================================

fn emit_function(
    out: &mut String,
    ip: &IrProgram,
    f: &IrFunction,
    target: EmitTarget,
    local: &HashSet<String>,
) -> Result<(), String> {
    let mut e = FnEmit::new(ip, f, target, local);
    e.compute_layout();
    e.prologue();
    for b in &f.blocks {
        let lbl = format!(".{}_b{}:", e.prefix, b.id);
        e.line(&lbl);
        for inst in &b.insts { e.inst(inst); }
        e.term(b.id, &b.term);
    }
    out.push_str(&e.out);
    Ok(())
}
