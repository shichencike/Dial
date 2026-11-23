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

use crate::safety::SafetyMode;
use crate::ir::*;
use crate::target::{EmitTarget, TargetCpu};
use std::collections::{HashMap, HashSet};

/// 参数落在哪个 C ABI 位置。
#[derive(Clone, Copy, Debug)]
enum ArgLoc { Int(usize), Fp(usize), Stack(usize) }

/// 按 C ABI 给一串参数分配位置。
/// SysV：整型/指针与浮点各自独立编号（rdi,rsi,… / xmm0,xmm1,…），超限进栈。
/// MS ABI：统一位置槽——第 i 个参数占槽 i（槽 0..3 是寄存器，整/浮二选一：
///   整走 rcx/rdx/r8/r9、浮走 xmm0/xmm1/xmm2/xmm3），第 4 个起进栈。
///   这是 Windows x64 与 SysV 最根本的 ABI 差异（寄存器不能"各自数"）。
fn abi_assign(tys: &[IrType], target: EmitTarget) -> Vec<ArgLoc> {
    if target.is_ms_abi() {
        let gp_n = target.int_arg_regs().len();   // 4
        let mut slot = 0usize;
        let mut si = 0usize;
        return tys.iter().map(|t| {
            let i = slot;
            slot += 1;
            if i < gp_n {
                if t.is_float() { ArgLoc::Fp(i) } else { ArgLoc::Int(i) }
            } else {
                let x = si; si += 1; ArgLoc::Stack(x)
            }
        }).collect();
    }
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

/// M7-e3w：把 i64 编码成 DWARF SLEB128 字节序列（DW_OP_fbreg 操作数用）。
fn sleb128(mut v: i64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && (byte & 0x40) == 0) || (v == -1 && (byte & 0x40) != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done { break; }
    }
    out
}

/// M7-e3z：IR 类型 → DWARF 分类（形参走这条；IR 已丢失 bool/string 的细分，
/// 只能粗分——局部变量走 dwarf_type(&ast::Type) 保留完整信息）。
fn dwarf_type_ir(t: &IrType) -> DwarfType {
    match t {
        IrType::F64 => DwarfType::Float,
        // M7-e3ab：形参层的精确宽度类型（IR 的 I32/F32 无 bool 歧义）。
        IrType::F32 => DwarfType::F32,
        IrType::I32 => DwarfType::I32,
        IrType::Handle => DwarfType::Str, // string/bytes 句柄优先按 char* 呈现
        // I8/I1：IR 丢失 byte/bool 细分，且 bool 形参走这里——保持 Int（8 字节有符号），
        // 避免把 bool 形参误显示成无符号字节；局部变量走 dwarf_type(&ast::Type) 保留精确信息。
        IrType::I64 | IrType::I8 | IrType::I1 => DwarfType::Int,
        _ => DwarfType::Ptr,
    }
}

/// M7-e3z：类型分类 → 类型 DIE 符号标签（均为 .Ldwarf_t_* 标签，ref4 引用）。
fn dwarf_type_sym(t: DwarfType) -> &'static str {
    match t {
        DwarfType::Int => ".Ldwarf_t_int",
        DwarfType::Float => ".Ldwarf_t_float",
        DwarfType::I32 => ".Ldwarf_t_i32",
        DwarfType::Byte => ".Ldwarf_t_byte",
        DwarfType::F32 => ".Ldwarf_t_f32",
        DwarfType::Bool => ".Ldwarf_t_bool",
        DwarfType::Str => ".Ldwarf_t_str",
        DwarfType::Ptr => ".Ldwarf_t_ptr",
    }
}

/// P1 平台对等：是否发射 DWARF。三处（emit 顶部的 .file 表 / emit() 末尾的 CU /
/// emit_function 的 end_label）必须共因调用本函数——尤其 end_label 与 CU 必须同 gate，
/// 否则 high_pc 引用未定义标签会链接失败。当前 x86-64 与 AArch64 都要 DWARF（token
/// 与生成主体是目标无关的 GAS 汇编），故直接取 debug_info；保留函数是为了单一真相源，
/// 未来若某目标不支持 DWARF 只在此处收紧即可，杜绝门控漂移。
fn dwarf_enabled(debug_info: bool, _target: EmitTarget) -> bool {
    debug_info
}

/// x86-64 调用外部 C 函数时的 PLT 重定位后缀。ELF（Linux）用 `@PLT` 走过程链接表；
/// Windows COFF（MinGW）没有 PLT 概念，符号名带 `@PLT` 会被链接器当成不同符号，
/// 报 "undefined reference to `dial_env_init@PLT'"。用编译期 cfg! 判断：
/// Linux 构建逐字节不变（仍 @PLT），Windows 构建返回空串（直接 call 符号名）。
fn plt_suffix() -> &'static str {
    if cfg!(windows) { "" } else { "@PLT" }
}

/// M7-e3w：生成手写 DWARF v5 CU（.debug_abbrev + .debug_info），供 gdb 源码级调试。
/// 行表（.debug_line）由汇编器的 .file/.loc 自动生成（见 emit() 顶部），本函数
/// 只负责建立 CU + subprogram + formal_parameter + variable + base_type DIE，
/// 并把 stmt_list 指向汇编器行表（offset 0）。与 LLVM 后端 -g 结构同构。
/// DWARF 常量编码经 /tmp POC 逐字节验证（abbrev code/form/属性码/版本匹配）。
/// P1：变量/形参 location 用 DW_OP_bregN + 相对当前栈指针的偏移（绕开 frame_base）；
/// 栈指针的 DWARF 寄存器号随目标不同——x86-64 rsp=7 (0x77)，AArch64 sp=31 (0x8f)。
fn emit_dwarf_cu(funcs: &[FnDwarfInfo], src_path: &str, target: EmitTarget) -> String {
    let breg: u8 = match target {
        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => 0x77, // DW_OP_breg7 = rsp
        EmitTarget::AArch64 => 0x8f,   // DW_OP_breg31 = sp (DWARF for AArch64)
    };
    let mut o = String::new();

    // ---------- .debug_abbrev ----------
    o.push_str("\t.section .debug_abbrev,\"\",@progbits\n");
    o.push_str(".Ldial_abbrev_start:\n");
    // code 1: DW_TAG_compile_unit (children=yes)
    o.push_str("\t.uleb128 1\n\t.byte 0x11,0x01\n");
    o.push_str("\t.byte 0x25,0x08\n"); // DW_AT_producer, string
    o.push_str("\t.byte 0x13,0x05\n"); // DW_AT_language, data2
    o.push_str("\t.byte 0x03,0x08\n"); // DW_AT_name, string
    o.push_str("\t.byte 0x10,0x17\n"); // DW_AT_stmt_list, sec_offset(0x17) [DWARF v5]
    o.push_str("\t.byte 0x11,0x01\n"); // DW_AT_low_pc, addr
    o.push_str("\t.byte 0x12,0x01\n"); // DW_AT_high_pc, addr
    o.push_str("\t.byte 0,0\n");
    // code 2: DW_TAG_subprogram (children=yes)
    o.push_str("\t.uleb128 2\n\t.byte 0x2e,0x01\n");
    o.push_str("\t.byte 0x03,0x08\n"); // name
    o.push_str("\t.byte 0x3a,0x0b\n"); // decl_file, data1
    o.push_str("\t.byte 0x3b,0x0b\n"); // decl_line, data1
    o.push_str("\t.byte 0x11,0x01\n"); // low_pc, addr
    o.push_str("\t.byte 0x12,0x01\n"); // high_pc, addr
    o.push_str("\t.byte 0,0\n");
    // code 3: DW_TAG_formal_parameter (no children)
    o.push_str("\t.uleb128 3\n\t.byte 0x05,0x00\n");
    o.push_str("\t.byte 0x02,0x18\n"); // location, exprloc(0x18)
    o.push_str("\t.byte 0x03,0x08\n"); // name
    o.push_str("\t.byte 0x49,0x13\n"); // M7-e3z: DW_AT_type, ref4
    o.push_str("\t.byte 0,0\n");
    // code 4: DW_TAG_variable (no children)
    o.push_str("\t.uleb128 4\n\t.byte 0x34,0x00\n");
    o.push_str("\t.byte 0x02,0x18\n"); // location, exprloc
    o.push_str("\t.byte 0x03,0x08\n"); // name
    o.push_str("\t.byte 0x3a,0x0b\n"); // decl_file
    o.push_str("\t.byte 0x3b,0x0b\n"); // decl_line
    o.push_str("\t.byte 0x49,0x13\n"); // M7-e3z: DW_AT_type, ref4
    o.push_str("\t.byte 0,0\n");
    // code 5: DW_TAG_base_type (no children)
    o.push_str("\t.uleb128 5\n\t.byte 0x24,0x00\n");
    o.push_str("\t.byte 0x03,0x08\n"); // name
    o.push_str("\t.byte 0x0b,0x0b\n"); // byte_size, data1
    o.push_str("\t.byte 0x3e,0x0b\n"); // encoding, data1
    o.push_str("\t.byte 0,0\n");
    // code 6: DW_TAG_pointer_type (no children)  M7-e3z
    o.push_str("\t.uleb128 6\n\t.byte 0x0f,0x00\n");
    o.push_str("\t.byte 0x49,0x13\n"); // DW_AT_type, ref4
    o.push_str("\t.byte 0x0b,0x0b\n"); // byte_size, data1
    o.push_str("\t.byte 0,0\n");
    o.push_str("\t.byte 0\n"); // end abbrev table

    // ---------- .debug_info (1 CU, DWARF v5) ----------
    o.push_str("\t.section .debug_info,\"\",@progbits\n");
    o.push_str(".Ldwarf_info_start:\n");
    o.push_str("\t.long .Ldwarf_info_end-.Ldwarf_info_start-4\n"); // unit_length
    // 2 字节常量用 .2byte（GAS 跨平台伪指令）；旧 .value 在 aarch64 GNU as 下是
    // unknown pseudo-op（P1 平台对等实测暴露）。.value/.short/.hword/.2byte 同为
    // 2 字节同义词，二进制等价，x86 输出零变化。
    o.push_str("\t.2byte 0x0005\n"); // version 5
    o.push_str("\t.byte 0x01\n");    // unit_type = DW_UT_compile
    o.push_str("\t.byte 0x08\n");    // address_size
    o.push_str("\t.long .Ldial_abbrev_start\n"); // abbrev offset: 链接器算合并段内真实偏移
    // CU DIE (code 1)
    o.push_str("\t.uleb128 1\n");
    o.push_str("\t.asciz \"dialc asm backend\"\n");
    o.push_str("\t.2byte 0x0002\n"); // DW_LANG_C
    o.push_str(&format!("\t.asciz \"{}\"\n", src_path.replace('\\', "\\\\").replace('"', "\\\"")));
    o.push_str("\t.long 0\n");       // stmt_list = 0 (assembler-generated line table)
    // low_pc / high_pc of CU: use first function .. last function (TLS-style bounds)
    if let (Some(first), Some(last)) = (funcs.first(), funcs.last()) {
        o.push_str(&format!("\t.quad {}\n", first.name));
        o.push_str(&format!("\t.quad {}\n", last.end_label));
    } else {
        o.push_str("\t.quad 0\n\t.quad 0\n");
    }

    for f in funcs {
        // subprogram DIE (code 2)
        o.push_str("\t.uleb128 2\n");
        o.push_str(&format!("\t.asciz \"{}\"\n", f.name));
        o.push_str(&format!("\t.byte {}\n", f.file_index)); // M7-e3aa: decl_file = 本函数源文件
        o.push_str(&format!("\t.byte {}\n", (f.decl_line.min(255)) as u8)); // decl_line
        o.push_str(&format!("\t.quad {}\n", f.name));
        // high_pc: 精确引用 emit_function 追加的函数末尾标签。
        o.push_str(&format!("\t.quad {}\n", f.end_label));
        // formal_parameter DIEs (code 3): location = breg7 + 真实参数槽偏移。
        // M7-e3ax：此前用 8*i 占位值，与 prologue 实际写参位置（alloca_off）
        // 不符，gdb print 形参读到栈上垃圾（如 add 的 a/b 显示 11 而非 3/4）。
        // 现在偏移由 emit_function 从 alloca_off[param_slot] 取真值传入。
        for (pname, off, pty) in &f.params {
            o.push_str("\t.uleb128 3\n");
            let sb = sleb128(*off);
            let mut loc = vec![breg]; // DW_OP_breg7(rsp)/breg31(sp) + offset，绕开 frame_base
            loc.extend_from_slice(&sb);
            o.push_str(&format!("\t.byte {},{}\n", loc.len(), loc.iter().map(|b| format!("0x{:02x}", b)).collect::<Vec<_>>().join(",")));
            o.push_str(&format!("\t.asciz \"{}\"\n", pname));
            o.push_str(&format!("\t.long {}-.Ldwarf_info_start\n", dwarf_type_sym(*pty)));
        }
        // variable DIEs (code 4)
        for (vname, vline, voff, vty) in &f.vars {
            o.push_str("\t.uleb128 4\n");
            let sb = sleb128(*voff);
            let mut loc = vec![breg]; // DW_OP_breg7(rsp)/breg31(sp) + offset，绕开 frame_base
            loc.extend_from_slice(&sb);
            o.push_str(&format!("\t.byte {},{}\n", loc.len(), loc.iter().map(|b| format!("0x{:02x}", b)).collect::<Vec<_>>().join(",")));
            o.push_str(&format!("\t.asciz \"{}\"\n", vname));
            o.push_str(&format!("\t.byte {}\n", f.file_index)); // M7-e3aa: 变量随所属函数的源文件
            o.push_str(&format!("\t.byte {}\n", (*vline).min(255) as u8)); // decl_line
            o.push_str(&format!("\t.long {}-.Ldwarf_info_start\n", dwarf_type_sym(*vty)));
        }
        o.push_str("\t.byte 0\n"); // end subprogram children
    }
    // ---- M7-e3z：类型 DIE（必须位于 CU children 内、end 之前，ref4 才有效）----
    // int: 有符号 8 字节
    o.push_str(".Ldwarf_t_int:\n\t.uleb128 5\n\t.asciz \"int\"\n\t.byte 8\n\t.byte 0x05\n");
    // float: 浮点 8 字节（DW_ATE_float=0x04）
    o.push_str(".Ldwarf_t_float:\n\t.uleb128 5\n\t.asciz \"float\"\n\t.byte 8\n\t.byte 0x04\n");
    // bool: 底层 i64 存 0/1，8 字节 boolean → gdb 显示 true/false
    o.push_str(".Ldwarf_t_bool:\n\t.uleb128 5\n\t.asciz \"bool\"\n\t.byte 8\n\t.byte 0x02\n");
    // char: 供 string 指针指向（DW_ATE_signed=0x05，gdb 按 C 字符串解读）
    o.push_str(".Ldwarf_t_char:\n\t.uleb128 5\n\t.asciz \"char\"\n\t.byte 1\n\t.byte 0x05\n");
    // M7-e3ab：精确宽度标量 DIE。栈槽里这些值只写低 N 字节（movl/movb/movss），
    // 高字节未定义；DIE 宽度报准后 gdb 只读有效位，print/ptype 才不串入垃圾高位。
    // int32: 有符号 4 字节（DW_ATE_signed=0x05）
    o.push_str(".Ldwarf_t_i32:\n\t.uleb128 5\n\t.asciz \"int32\"\n\t.byte 4\n\t.byte 0x05\n");
    // byte: 无符号 1 字节（DW_ATE_unsigned=0x07）——与 char(有符号)区分，byte 0~255 显示正确
    o.push_str(".Ldwarf_t_byte:\n\t.uleb128 5\n\t.asciz \"byte\"\n\t.byte 1\n\t.byte 0x07\n");
    // float32: 浮点 4 字节（DW_ATE_float=0x04）
    o.push_str(".Ldwarf_t_f32:\n\t.uleb128 5\n\t.asciz \"float32\"\n\t.byte 4\n\t.byte 0x04\n");
    // str: pointer → char，gdb 显示 char* 并解引用成字符串
    o.push_str(".Ldwarf_t_str:\n\t.uleb128 6\n\t.long .Ldwarf_t_char-.Ldwarf_info_start\n\t.byte 8\n");
    // ptr: pointer → char（数组/map/结构体等不透明句柄，gdb 显示为指针）
    o.push_str(".Ldwarf_t_ptr:\n\t.uleb128 6\n\t.long .Ldwarf_t_char-.Ldwarf_info_start\n\t.byte 8\n");
    o.push_str("\t.byte 0\n"); // end CU children
    o.push_str(".Ldwarf_info_end:\n");
    o
}

// ===========================================================================
// R4：自研 PGO 计划（instrument / optimize 两态；baseline 恒为 None，零回归）
// ===========================================================================

/// PGO 三态中的发射期计划。由 `emit()` 依 instrument / profile 构建，透传给
/// 每个 `emit_function`。baseline（profile=None 且 !instrument）下 `instrument=false`
/// 且 `profile=None`，所有分支短路，输出与引入 PGO 前逐字节一致。
struct PgoPlan<'a> {
    /// 阶段一插桩：发 .comm 计数器、函数/循环入口 incq、main 返回前 dump。
    instrument: bool,
    /// 阶段二优化：读回 profile，热函数分段 + 只对齐热循环。
    profile: Option<&'a crate::pgo::ProfileData>,
    /// 计数器总数 N（函数数 + 回边循环头数）。instrument 态用于 .comm 与 dump 实参。
    counter_count: usize,
    /// 函数名 → 函数计数器 idx（0..F-1，源码序）。
    fn_counter_idx: std::collections::HashMap<String, usize>,
    /// (函数名, 块 id) → 循环计数器 idx（F + 序号）。仅回边循环头有条目。
    loop_counter_idx: std::collections::HashMap<(String, u32), usize>,
    /// 热函数集合。profile=None 时为空但 `is_hot()` 恒 true（走原布局）。
    hot_fns: std::collections::HashSet<String>,
    /// 是否处于 optimize 态（profile.is_some()）。
    optimize: bool,
}

impl<'a> PgoPlan<'a> {
    /// 该函数是否热。baseline/instrument 恒 true（保留原全对齐 + 单 .text）；
    /// 只有 optimize 态才按 profile 判定。
    fn is_hot(&self, name: &str) -> bool {
        !self.optimize || self.hot_fns.contains(name)
    }
}

/// 从 IR 判定回边循环头：跳转目标的块下标 < 当前块下标（与文本级
/// align_loop_headers 的"目标定义早于跳转"口径一致）。返回按源码序、
/// 块 id 升序排列的 (函数名, 块 id) 列表。
fn ir_backedge_loop_headers(ip: &IrProgram) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for f in &ip.functions {
        // 块下标 → block id
        let idx_of: std::collections::HashMap<u32, usize> =
            f.blocks.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
        let mut heads: Vec<u32> = Vec::new();
        for (i, b) in f.blocks.iter().enumerate() {
            let targets: Vec<u32> = match &b.term {
                Term::Jump { target, .. } => vec![*target],
                Term::Branch { then_b, else_b, .. } => vec![*then_b, *else_b],
                _ => Vec::new(),
            };
            for t in targets {
                if let Some(&td) = idx_of.get(&t) {
                    if td < i && !heads.contains(&b.id) {
                        // b 是回边源；其跳转目标 t 所在块是循环头
                        let header_id = f.blocks[td].id;
                        if !heads.contains(&header_id) { heads.push(header_id); }
                    }
                }
            }
        }
        heads.sort_unstable();
        for h in heads { out.push((f.name.clone(), h)); }
    }
    out
}

/// 依 profile 计算热函数集合。函数热度 = 入口计数 + 其所有循环头计数之和
/// （一个只在循环里干活的函数，入口只调 1 次但循环跑 N 次，理应判为热函数）。
/// 判热用相对阈值：分数 ≥ 最高分 × 10% 即热（宽松判热——"把热函数误判为冷、
/// 甩进 .text.unlikely"的代价，远大于"把冷函数误判为热、多占一点热段"），
/// 且天然避免固定 top-K 把多个同等热函数中的一部分误判为冷。强制 main 入热；
/// 全零 → 全部入热（回退 baseline 布局）。
fn compute_hot_fns(
    ip: &IrProgram,
    prof: &crate::pgo::ProfileData,
    loop_idx: &std::collections::HashMap<(String, u32), usize>,
) -> std::collections::HashSet<String> {
    let mut hot = std::collections::HashSet::new();
    if prof.total() == 0 {
        for f in &ip.functions { hot.insert(f.name.clone()); }
        return hot;
    }
    // 先把每个循环头的计数累加到所属函数。
    let mut loop_bonus: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for ((fname, _bid), &idx) in loop_idx.iter() {
        let c = prof.counts.get(idx).copied().unwrap_or(0);
        let slot = loop_bonus.entry(fname.clone()).or_insert(0);
        *slot = slot.saturating_add(c);
    }
    let scored: Vec<(&str, u64)> = ip.functions.iter().enumerate()
        .map(|(i, f)| {
            let entry = prof.counts.get(i).copied().unwrap_or(0);
            let bonus = loop_bonus.get(&f.name).copied().unwrap_or(0);
            (f.name.as_str(), entry.saturating_add(bonus))
        })
        .collect();
    let max_score = scored.iter().map(|x| x.1).max().unwrap_or(0);
    let threshold = (max_score / 10).max(1); // 最高分的 10%，至少 1
    for (name, score) in scored {
        if score >= threshold { hot.insert(name.to_string()); }
    }
    hot.insert("main".to_string()); // 强制 main 入热
    hot
}

pub fn emit(ip: &IrProgram, target: EmitTarget, debug_info: bool, src_path: &str, fn_origin: &std::collections::HashMap<String, String>, opt_level: OptLevel, safety: SafetyMode, target_cpu: TargetCpu, instrument: bool, profile: Option<&crate::pgo::ProfileData>) -> Result<String, String> {
    // M7-e3ay：--target-cpu 仅控制新增高级 ISA 融合的开关；baseline 下不发射任何
    // 新指令，产物与引入本参数前逐字节一致。当前批次只建立开关基础设施，后续
    // 各 targeted 融合（LZCNT/BMI/AVX2/FMA…）各自读取 target_cpu 的 has_* 门控。
    let _ = target_cpu;
    // M7-e3as / P4.5：内联汇编在 x86-64 与 AArch64 双目标均支持。模板由开发者
    // 按目标汇编语法书写（AArch64 用 AAPCS64 寄存器名 x0-x7），后端负责 $N 占位
    // 符到物理寄存器的渲染与操作数进出；方言切换（ATT/Intel）仅 x86-64 有意义。
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

    // R4/P4.5：PGO 在 x86-64 与 AArch64 双目标均生效（插桩已 target-aware）；
    // -g 调试态一律退化为 baseline（插桩会扰动调试体验与 DWARF 行号）。
    let pgo_active = !debug_info;
    let eff_instrument = instrument && pgo_active;
    let eff_profile = if pgo_active { profile } else { None };

    let loops = if eff_instrument { ir_backedge_loop_headers(ip) } else { Vec::new() };
    let fn_count = ip.functions.len();
    let mut fn_counter_idx = std::collections::HashMap::new();
    for (i, f) in ip.functions.iter().enumerate() {
        fn_counter_idx.insert(f.name.clone(), i);
    }
    let mut loop_counter_idx = std::collections::HashMap::new();
    for (j, (fname, bid)) in loops.iter().enumerate() {
        loop_counter_idx.insert((fname.clone(), *bid), fn_count + j);
    }
    let counter_count = if eff_instrument { fn_count + loops.len() } else { 0 };
    let hot_fns = eff_profile
        .map(|p| compute_hot_fns(ip, p, &loop_counter_idx))
        .unwrap_or_default();
    let pgo = PgoPlan {
        instrument: eff_instrument,
        profile: eff_profile,
        counter_count,
        fn_counter_idx,
        loop_counter_idx,
        hot_fns,
        optimize: eff_profile.is_some(),
    };

    let mut out = String::new();
    out.push_str("# 由 dialc 自研后端生成（IR -> .s）；请勿手工编辑\n");
    // M7-e3w：-g 时声明源文件（汇编器据此生成 DWARF v5 行表）。
    // P1：AArch64 与 x86-64 同标准生成 .file/.loc/.debug_*（源码级调试平台对等）。
    // M7-e3aa：多文件 import 合并编译时，为每个不同源文件分配一个 .file 编号：
    //   索引 0（.file 1）恒为主源文件 src_path，其余按函数出现顺序稳定分配。
    // 单文件 / 非 -g 路径不受影响（文件表仍只含主文件、编号恒为 1）。
    // P1：x86-64 与 AArch64 共用 dwarf_enabled 单一真相源。
    let dwarf_on = dwarf_enabled(debug_info, target);
    let mut file_table: Vec<String> = Vec::new();
    if dwarf_on {
        file_table.push(src_path.to_string());
        for f in &ip.functions {
            if let Some(origin) = fn_origin.get(&f.name) {
                let abs = std::fs::canonicalize(origin)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| origin.clone());
                if !file_table.iter().any(|x| x == &abs) {
                    file_table.push(abs);
                }
            }
        }
        for (i, path) in file_table.iter().enumerate() {
            out.push_str(&format!("\t.file {} \"{}\"\n", i + 1, path.replace('\\', "\\\\").replace('"', "\\\"")));
        }
    }
    out.push_str("\t.section .rodata\n");
    for (idx, s) in ip.string_literals.iter().enumerate() {
        // 用 .byte 序列而非 .asciz，彻底规避引号/反斜杠/UTF-8 的转义歧义。
        let mut bytes: Vec<String> = s.as_bytes().iter().map(|b| b.to_string()).collect();
        bytes.push("0".to_string());
        out.push_str(&format!(".Lstr{}:\n\t.byte {}\n", idx, bytes.join(", ")));
        // 静态 immortal DialString：布局 [rc=INT64_MIN][len][data\0]，8 字节对齐。
        // DialString handle 指向 len 字段（= 对象 +8），dial_rc_retain/release/clone
        // 对 INT64_MIN 哨兵 no-op，故可安全共享、永不释放。字面量编译期已知且不可变，
        // 用它替代运行时 dial_string_new，消除循环内反复的 malloc+strlen+memcpy。
        out.push_str(&format!(
            "\t.balign 8\n.Ldstr{}:\n\t.quad -9223372036854775808\n\t.quad {}\n\t.byte {}\n",
            idx, s.as_bytes().len(), bytes.join(", ")
        ));
    }
    // v1.21 字段级 ARC：struct 的 desc 表（[nfields][(kind,offset)×n]）。
    // 与 runtime/dial_rt_mem.c 的 DialArcFieldDesc 口径一致，供 box 归零时逐字段释放。
    for (label, table) in &ip.arc_descs {
        let quads = table.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ");
        out.push_str(&format!("\t.balign 8\n{}:\n\t.quad {}\n", label, quads));
    }
    // R4 阶段一：插桩计数器数组（.comm 零初始化、8 字节对齐）。
    if pgo.instrument {
        out.push_str(&format!(
            "\t.comm dial_pgo_counters,{},{}\n",
            pgo.counter_count * 8, 8
        ));
    }
    // R4 阶段二：optimize 态不在此处发单 .text；每个函数按冷热各发自己的 .section。
    if !pgo.optimize {
        out.push_str("\t.text\n");
    }

    let local_funcs: HashSet<String> = ip.functions.iter().map(|f| f.name.clone()).collect();

    // M7-e3v：构建调用图，标记「递归 SCC 成员」（自己可达自己）。
    // 递归函数每层调用都要在 prologue/epilogue 保存恢复 callee-saved，
    // 若再做全函数全局驻留，栈帧膨胀会被递归深度放大成数量级回归
    // （evR/odR 互递归实测 2.5x）。这类函数必须退回逐块 / 零驻留路径。
    let mut call_graph: HashMap<String, Vec<String>> = HashMap::new();
    for f in &ip.functions {
        let mut callees = Vec::new();
        for b in &f.blocks {
            for i in &b.insts {
                if let IrInst::Call { func, .. } = i { callees.push(func.clone()); }
                if let IrInst::CallIndirect { .. } = i { /* 间接调用目标未知，不建边 */ }
            }
        }
        call_graph.insert(f.name.clone(), callees);
    }
    let mut recursive_funcs: HashSet<String> = HashSet::new();
    for f in &ip.functions {
        // f 能否经任意长度调用链回到自己（深度有界，防病态环）。
        fn reaches_self(graph: &HashMap<String, Vec<String>>, start: &str) -> bool {
            let mut seen = HashSet::new();
            let mut stack = vec![start.to_string()];
            let mut steps = 0usize;
            while let Some(cur) = stack.pop() {
                if steps > 10000 { return false; }
                steps += 1;
                if let Some(cs) = graph.get(&cur) {
                    for c in cs {
                        if c == start { return true; }
                        if seen.insert(c.clone()) { stack.push(c.clone()); }
                    }
                }
            }
            false
        }
        if reaches_self(&call_graph, &f.name) { recursive_funcs.insert(f.name.clone()); }
    }
    let mut dwarf_funcs: Vec<FnDwarfInfo> = Vec::new();
    for f in &ip.functions {
        // M7-e3aa：本函数源文件编号（.file N / DW_AT_decl_file N）。
        // 主源文件恒为 1；子模块函数取其 origin 在 file_table 中的位置。
        let file_index: u8 = if dwarf_on {
            let origin_abs = fn_origin.get(&f.name).and_then(|origin| {
                std::fs::canonicalize(origin)
                    .map(|p| p.to_string_lossy().into_owned())
                    .ok().or_else(|| Some(origin.clone()))
            });
            match origin_abs.and_then(|a| file_table.iter().position(|x| x == &a)) {
                Some(i) => (i + 1) as u8,
                None => 1,
            }
        } else {
            1
        };
        // R4 optimize：按冷热切段，令 linker 聚拢热函数（冷函数甩 .text.unlikely）。
        if pgo.optimize {
            let sec = if pgo.is_hot(&f.name) {
                "\t.section .text.hot,\"ax\",@progbits\n"
            } else {
                "\t.section .text.unlikely,\"ax\",@progbits\n"
            };
            out.push_str(sec);
        }
        let di = emit_function(&mut out, ip, f, target, &local_funcs, &recursive_funcs, debug_info, file_index, opt_level, safety, &pgo)?;
        dwarf_funcs.push(di);
    }
    // 避免 GNU ld 的 "missing .note.GNU-stack section implies executable stack" 警告。
    // 该段是 **ELF 专用**（section 名含 '-'、类型 @progbits），Windows 的 MinGW/COFF
    // 汇编器会把 `.note.GNU-stack` 里的 '-' 当成 junk 报 "first unrecognized character is `-'"。
    // 故仅非 Windows 构建发射；Windows 目标下 COFF 本身默认非可执行栈，无需此标记。
    // 用编译期 cfg! 判断：Linux 构建逐字节不变，Windows 构建跳过该行。
    if !cfg!(windows) {
        out.push_str("\t.section .note.GNU-stack,\"\",@progbits\n");
    }
    // M7-e3q：尾调用消除（递归 SCC 内的内部尾调用 call+ret → jmp）。
    // 必须是模块级 pass：需要跨函数的调用图判定「callee 可达 caller」，逐函数
    // 发射时看不到全局。仅 x86-64 生效；AArch64 零变化。
    let final_asm = if matches!(target, EmitTarget::X86_64SysV) {
        elide_tail_calls(&out)
    } else {
        out
    };
    // M7-e3w：-g 且 x86-64 时追加手写 DWARF CU（行表由 .file/.loc 已在汇编器生成）。
    // P1：AArch64 与 x86-64 同标准生成 DWARF CU（行表由 .file/.loc 已在汇编器生成）。
    if dwarf_on {
        Ok(final_asm + &emit_dwarf_cu(&dwarf_funcs, src_path, target))
    } else {
        Ok(final_asm)
    }
}

fn align_up(x: i64, a: i64) -> i64 { (x + a - 1) / a * a }

/// x86-64 里可供「无 call 基本块」独占的整数寄存器池。
///
/// 前 6 个是 SysV callee-saved（含 rbp；本后端用 rsp 直接寻址，rbp 空闲），
/// 必须在 prologue/epilogue 保存恢复；其后是 caller-saved，因为启用驻留的
/// 块内一条 call 都没有，它们不会被任何被调方破坏，故无需保存。
/// 末两个 rsi / rdi 是 C ABI 传参寄存器：仅在「全函数无 call」的统一模式里
/// 才安全（prologue 读完参数后即空闲），逐块模式只用前 RES_PER_BLOCK 个，
/// 避免与 inst_call 的传参互相踩踏。
/// 刻意避开 rax / rcx / rdx / r10 / r11：这些是发射器的临时寄存器或
/// div / 移位 / setcc 的隐式操作数。
const RES_POOL: [&str; 10] = ["rbx", "r12", "r13", "r14", "r15", "rbp", "r8", "r9", "rsi", "rdi"];
/// 逐块独立驻留（含 call 的函数）可用的寄存器数：不含 rsi / rdi。
const RES_PER_BLOCK: usize = 8;
/// M7-e3v：含 call 函数做全函数全局驻留时，只能用 callee-saved 寄存器——
/// 它们由被调方保证返回时原值不变，故可跨 call 持有最新值。刻意不含
/// r8/r9/rsi/rdi（caller-saved，会被 call 破坏）与 rax/rcx/rdx/r10/r11
/// （发射器临时 / div / 移位隐式操作数）。
const CALLEE_SAVED_POOL: [&str; 6] = ["rbx", "r12", "r13", "r14", "r15", "rbp"];
/// M7-e3av：无 call 的纯计算函数里供浮点值独占的 xmm 驻留池。
/// xmm0 = 返回值、xmm1-7 = ABI 浮点传参、xmm8/xmm9 = f1/f2 固定 scratch——
/// 全部排除，故只剩 xmm10-15。SysV 下它们虽是 caller-saved，但在「全函数
/// 无 call」前提下无被调方踩踏，故无需 prologue/epilogue 保存（绝不进 used_phys）。
const FP_RES_POOL: [&str; 13] = ["xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7"];

fn is_callee_saved(p: &str) -> bool {
    matches!(p, "rbx" | "rbp" | "r12" | "r13" | "r14" | "r15")
}

/// 能整槽提升为寄存器值的类型：8 字节、非浮点、非结构体。
/// 窄整型走 load_stack 的 movzbq/movl 零扩展、浮点走 xmm，语义不同，不提升。
fn promotable_ty(t: &IrType) -> bool {
    matches!(t, IrType::I64 | IrType::Ptr | IrType::Handle | IrType::FnPtr)
}

/// 一个基本块的寄存器驻留方案（编译期确定，emit 时按块装载 / 回写）。
#[derive(Clone, Default)]
struct BlockPlan {
    /// vreg → 物理寄存器（裸名，不含 `%`）。
    resident: HashMap<VReg, &'static str>,
    /// 块入口需从栈槽装载的 (vreg, 物理寄存器)，按 vreg 升序。
    loads: Vec<(VReg, &'static str)>,
    /// 需要在块尾写回栈槽的普通 vreg（跨块仍被读取的那些）。
    /// 提升槽不在此列——它们无条件回写，因为别的块/含 call 块走内存路径。
    keep_flush: HashSet<VReg>,
    /// M7-e3av：浮点 vreg → 物理 xmm 裸名（与整数 resident 完全独立）。
    fp_resident: HashMap<VReg, &'static str>,
    /// M7-e3av：块入口需 movsd/movss 装载的 (浮点 vreg, 物理 xmm)，按 vreg 升序。
    /// 与整数 loads 分开——begin_block 对整数 loads 发 movq，浮点必须发 movsd/movss。
    fp_loads: Vec<(VReg, &'static str)>,
    /// M7-e3av：浮点块尾回写集合。第一版（unified 模式）恒空，为逐块模式预留。
    fp_keep_flush: HashSet<VReg>,
}

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
    /// 块内寄存器驻留：当前生效的映射 / 脏集合 / 开关。
    resident: HashMap<VReg, &'static str>,
    dirty: HashSet<VReg>,
    residency_on: bool,
    /// M7-e3av：浮点驻留映射 / 回写掩码。与整数 resident 共用 residency_on 开关，
    /// 但映射表完全独立——phys/val_phys 只查整数表，fp_phys/val_fp_phys 只查此表。
    fp_resident: HashMap<VReg, &'static str>,
    fp_flush_mask: HashSet<VReg>,
    /// 每个基本块的驻留方案，与 f.blocks 一一对应。
    plans: Vec<BlockPlan>,
    /// 本函数实际用到的 callee-saved 寄存器（prologue 保存 / epilogue 恢复）。
    used_phys: Vec<&'static str>,
    /// 帧内保存区起始偏移（x86-64 固定预留 6×8 字节；AArch64 为 0）。
    save_off: i64,
    /// AArch64 链接寄存器 x30(LR) 的帧内保存槽（相对 sp）；-1 表示 x86-64（不保存）。
    /// x30 是 caller-saved，`bl`/`blr` 调用会覆盖它；AArch64 所有函数统一在 prologue
    /// 保存、epilogue 恢复，否则末尾 `ret` 跳回最后一次调用的返回地址形成死循环
    /// （实机 qemu 验证抓到；交叉汇编语法校验看不出）。发射调用处有十余种 IR 指令
    /// （含 codegen 插入的 EnvInit），逐指令枚举易遗漏，故统一保存。
    /// x86-64 的 call/ret 走栈天然隔离，恒为 -1（零变化）。
    lr_slot: i64,
    /// 函数级判定：地址不逃逸、且所有 Load/Store 都是整槽宽度的 Alloca，
    /// 可以把它的「值」提升进寄存器池（key 是 Alloca 的 dst vreg）。
    promotable: HashSet<VReg>,
    /// 当前块需要回写栈槽的普通 vreg（跨块活跃集合）。
    flush_mask: HashSet<VReg>,
    /// M7-e1：定义了但全函数从未被读取的普通 vreg——其 spill store 可整条省略。
    dead_defs: HashSet<VReg>,
    /// M7-e3b：ICmp 结果「全函数仅被一处 Branch.cond 使用」时，可免物化，
    /// 直接在 Branch 处发 `cmp; jcc`。value = (比较算子, a, b)。
    fusable: HashMap<VReg, (ACmp, Value, Value)>,
    /// M7-e3k：`(a % 2^k) == 0 / != 0` 且 Mod 结果与 ICmp 结果都只被这一条
    /// Branch.cond 链式使用时，跳过 Mod + ICmp 物化，直接在 Branch 处发
    /// `testq $(2^k-1), a; jcc`。key = ICmp 的 dst，value = (算子, Mod 输入 a, k)。
    pow2_mod_test: HashMap<VReg, (ACmp, Value, u32)>,
    /// M7-e3k：上述 Mod 指令的 dst——发射时整条跳过（结果不再有使用者）。
    skip_pow2_mod: HashSet<VReg>,
    /// M7-e3l：`(base * c) + disp`（c∈{2,3,5,9}）融合成单条
    /// `leaq disp(base, base, c-1), dst`。key = Add 的 dst，
    /// value = (乘法变量因子, scale = c-1 ∈ {1,2,4,8}, disp32)。
    lea_fuse: HashMap<VReg, (Value, u32, i64)>,
    /// M7-e3l：上述 Mul 指令的 dst——发射时整条跳过（结果不再有使用者）。
    skip_lea_mul: HashSet<VReg>,
    /// M7-e3m：popcount 内层 `c += x & 1; x >>= 1` 的 8 条 IR 窗口融合。
    /// key = (块下标, 窗口起始指令下标)，value = (c 槽 Alloca vreg, x 槽 Alloca vreg)。
    /// 窗口是块内任意连续 8 条（不限整块，故 for 循环体含 index 递增也能命中）；
    /// 命中处发 `sarq $1, phys(x); adcq $0, phys(c)` 并跳过窗口内 8 条 IR。
    /// 仅 x86-64 填充。发射期还会二次确认两槽都驻留、物理寄存器不同，
    /// 任一不满足就逐条退回原路径（见 inst_popcnt_fused）。
    popcnt_fuse: HashMap<(usize, usize), (VReg, VReg)>,
    /// M7-e3y：popcnt_fuse 的子集——窗口经确认是 `while(x>0){c+=x&1;x>>=1}`
    /// 的完整自循环（x 是循环变量、回边到读 x 的 cond、循环后 x 归零即自然结束）。
    /// 这类窗口可用单条 `popcnt` 一次算完并把 x 归零让 cond 自然退出，无需改 CFG。
    /// 其余窗口（如 for 循环体的一部分）仍走 e3m 的 sarq+adcq 逐位路径。
    /// 仅 x86-64 填充；发射期二次确认同 popcnt_fuse。
    popcnt_native: HashSet<(usize, usize)>,
    /// M7-e3n：xorshift 步的寄存器内融合。key = (块下标, 窗口起始指令下标)，
    /// value = (融合计划, 窗口 IR 条数)。覆盖 b24 的两种形态：
    ///   A `x = x ^ ((x << k) & 0xFFFFFFFF)`（6 条 IR）；
    ///   B `x = x ^ (x >> k)`（5 条 IR，Shr 算术右移）。
    /// 直接在 x 槽的物理寄存器上就地算，消除中间值的 spill / reload 与搬运。
    /// 仅 x86-64 填充；发射期二次确认 x 槽确实驻留，否则退回原路径。
    xorshift_fuse: HashMap<(usize, usize), (XorshiftPlan, usize)>,
    /// M7-e3v：本函数位于递归 SCC 内——禁止全局跨 call 驻留
    /// （递归栈帧会放大 callee-saved 保存恢复开销），退回逐块模式。
    gra_blocked: bool,
    /// M7-e3ac：优化级别。O0/O1 禁用 GRA 驻留与 ISA 指令融合（popcnt/xorshift），
    /// 与 -g 一样走全 spill 路径；O2（默认）/O3 保持既有全部优化。
    opt_level: OptLevel,
    /// M7-e3w：-g 调试模式。为让 DWARF 变量 location（DW_OP_fbreg 指向栈槽）
    /// 准确，-g 时禁用一切寄存器驻留 / 提升优化，所有变量留在 alloca_off 栈槽
    /// （与 LLVM 后端 -g 强制 O0 同构）。同时发射 .loc / .debug_* 调试信息。
    debug_info: bool,
    /// M7-e3aa：本函数源文件的 .file 编号（1-based），供 .loc N 使用。
    file_index: u8,
    /// M7-e3ae：Fail-Fast 安全等级。Debug/Safe 为整数除零插入分级陷阱，
    /// ReleaseFast 不插检查（与 LLVM 后端 emit_div_zero_check 同口径）。
    safety: SafetyMode,
    /// M7-e3ae：本函数内除零检查的 ok 标签序号，保证标签唯一。
    dchk_seq: u32,
    /// R4：本函数的 PGO 计划（引用 emit 期构建的整体计划）。
    pgo: &'a PgoPlan<'a>,
    /// R4：本函数是否热（baseline/instrument 恒 true）。
    pgo_hot: bool,
}

/// M7-e3n：xorshift 一步的融合计划。`slot` 是 x 的 Alloca 槽 vreg（驻留在
/// 物理寄存器里），`k` 是移位量（1..=63）。两种形态的窗口长度不同，故连同
/// 窗口条数一起存进 map，供发射主循环跳过余下 IR。
#[derive(Clone, Copy)]
enum XorshiftPlan {
    /// `x = x ^ ((x << k) & 0xFFFFFFFF)`：shlq + movl(eax,eax) + xorq。
    ShiftMaskXor { slot: VReg, k: u32 },
    /// `x = x ^ (x >> k)`：sarq（算术右移，与通用 Shr 语义一致）+ xorq。
    ShiftXor { slot: VReg, k: u32 },
}

impl<'a> FnEmit<'a> {
    fn new(ip: &'a IrProgram, f: &'a IrFunction, target: EmitTarget, local: &'a HashSet<String>, gra_blocked: bool, debug_info: bool, file_index: u8, opt_level: OptLevel, safety: SafetyMode, pgo: &'a PgoPlan<'a>) -> Self {
        let pgo_hot = pgo.is_hot(&f.name);
        let prefix = format!("Ldial_{}", {
            let s: String = f.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
            s
        });
        FnEmit {
            ip, f, target, local_funcs: local,
            pgo, pgo_hot,
            reg_off: Vec::new(), alloca_off: HashMap::new(), frame_size: 0,
            prefix, out: String::new(),
            resident: HashMap::new(), dirty: HashSet::new(), residency_on: false,
            fp_resident: HashMap::new(), fp_flush_mask: HashSet::new(),
            plans: Vec::new(), used_phys: Vec::new(), save_off: 0, lr_slot: -1,
            promotable: HashSet::new(), flush_mask: HashSet::new(),
            dead_defs: HashSet::new(), fusable: HashMap::new(),
            pow2_mod_test: HashMap::new(), skip_pow2_mod: HashSet::new(),
            lea_fuse: HashMap::new(), skip_lea_mul: HashSet::new(),
            popcnt_fuse: HashMap::new(), popcnt_native: HashSet::new(),
            xorshift_fuse: HashMap::new(), gra_blocked, opt_level, debug_info, file_index,
            safety, dchk_seq: 0,
        }
    }

    // ---- 基础工具 ----

    fn line(&mut self, s: &str) { self.out.push_str(s); self.out.push('\n'); }

    /// 寄存器名前缀：x86 用 `%`，AArch64 不加。调用方一律传「裸名」。
    fn r(&self, reg: &str) -> String {
        match self.target { EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => format!("%{}", reg), EmitTarget::AArch64 => reg.to_string() }
    }
    fn slot(&self, v: VReg) -> i64 { self.reg_off[v as usize] }
    fn rty(&self, v: VReg) -> IrType { self.f.reg_ty[v as usize].clone() }
    fn vty(&self, v: Value) -> IrType { v.ty(&self.f.reg_ty) }

    // 固定的临时寄存器（都不属于 C ABI 传参寄存器，避免互相踩踏）。
    fn g1(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "rax", EmitTarget::AArch64 => "x9" } }
    fn g2(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "r10", EmitTarget::AArch64 => "x10" } }
    fn g3(&self) -> &'static str { match self.target { EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "r11", EmitTarget::AArch64 => "x11" } }
    // MS ABI：xmm6-xmm15 全是 callee-saved，固定 scratch 必须避开，改用
    // volatile 的 xmm4/xmm5（xmm0-3 是参数/返回寄存器，不能在装载参数期间占用）。
    fn f1(&self) -> &'static str { match self.target { EmitTarget::X86_64Ms => "xmm4", EmitTarget::X86_64SysV => "xmm8", EmitTarget::AArch64 => "d16" } }
    fn f2(&self) -> &'static str { match self.target { EmitTarget::X86_64Ms => "xmm5", EmitTarget::X86_64SysV => "xmm9", EmitTarget::AArch64 => "d17" } }

    /// 32 位 / 8 位 / 浮点单精度视图。
    fn r32(&self, reg: &str) -> String {
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => match reg {
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
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => match reg {
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
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => reg.to_string(),         // movss 用 xmm 名
            EmitTarget::AArch64 => reg.replacen('d', "s", 1),  // d16 -> s16
        }
    }

    /// 把 64 位立即数装入整数寄存器（AArch64 用 movz/movk 拼接，不用 literal pool）。
    fn mov_imm64(&mut self, reg: &str, bits: u64) {
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
                // M7-a2：vreg 若驻留在物理寄存器里，直接寄存器搬运，省掉内存 load。
                if let Some(p) = self.val_phys(r) {
                    if p != reg {
                        self.line(&format!("\tmovq {}, {}", self.r(p), self.r(reg)));
                    }
                    return;
                }
                let off = self.slot(r);
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovq {}({}), {}", off, self.r("rsp"), self.r(reg))),
                    EmitTarget::AArch64 => self.line(&format!("\tldr {}, [sp, #{}]", reg, off)),
                }
            }
        }
    }

    /// 把 Value（浮点）读入浮点临时寄存器 `reg`。
    fn lf(&mut self, v: Value, reg: &str) {
        match v {
            Value::Imm(Imm::F64(bits)) => match self.target {
                EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                    self.line(&format!("\tmovabsq ${}, {}", bits as i64, self.r("rax")));
                    self.line(&format!("\tmovq {}, {}", self.r("rax"), self.r(reg)));
                }
                EmitTarget::AArch64 => {
                    self.mov_imm64("x9", bits);
                    self.line(&format!("\tfmov {}, x9", reg));
                }
            },
            Value::Imm(Imm::F32(bits)) => match self.target {
                EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                        self.line(&format!("\tmovabsq ${}, {}", n, self.r("rax")));
                        self.line(&format!("\tmovq {}, {}", self.r("rax"), self.r(reg)));
                    }
                    EmitTarget::AArch64 => { self.mov_imm64("x9", n as u64); self.line(&format!("\tfmov {}, x9", reg)); }
                }
            }
            Value::Reg(r) => {
                let f32 = self.rty(r) == IrType::F32;
                // M7-e3av：驻留浮点值直接从物理 xmm 读，不再走栈槽。
                // 仅 x86-64；非 x86 的 val_fp_phys 恒 None，原路径不变。
                if let Some(p) = self.val_fp_phys(r) {
                    if p != reg {
                        let m = if f32 { "movss" } else { "movsd" };
                        self.line(&format!("\t{} {}, {}", m, self.r(p), self.r(reg)));
                    }
                    return;
                }
                let off = self.slot(r);
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
        // M7-a2：驻留 vreg 的最新值留在物理寄存器里，块尾统一回写栈槽。
        // 这样块内一串运算不再反复 store/load 同一个栈槽。
        if let Some(p) = self.val_phys(dst) {
            if p != reg {
                self.line(&format!("\tmovq {}, {}", self.r(reg), self.r(p)));
            }
            self.dirty.insert(dst);
            return;
        }
        // M7-e1：死 def 的 spill store 整条省略。驻留 / phi / alloca / param
        // 均已在 analyze_dead_defs 排除或先于此处返回，落到这里的必是
        // 「定义了却再无人读」的普通全 spill 值，跳过不影响任何读取路径。
        if self.dead_defs.contains(&dst) { return; }
        let off = self.slot(dst);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovq {}, {}({})", self.r(reg), off, self.r("rsp"))),
            EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", reg, off)),
        }
    }

    fn sf(&mut self, dst: VReg, reg: &str) {
        let f32 = self.rty(dst) == IrType::F32;
        // M7-e3av：驻留浮点 dst 写回物理 xmm + 标脏，块尾必要时才落栈。
        // 仅 x86-64；非 x86 的 val_fp_phys 恒 None，原路径不变。
        if let Some(p) = self.val_fp_phys(dst) {
            if p != reg {
                let m = if f32 { "movss" } else { "movsd" };
                self.line(&format!("\t{} {}, {}", m, self.r(reg), self.r(p)));
            }
            self.dirty.insert(dst);
            return;
        }
        let off = self.slot(dst);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                let m = if f32 { "movss" } else { "movsd" };
                self.line(&format!("\t{} {}, {}({})", m, self.r(reg), off, self.r("rsp")));
            }
            EmitTarget::AArch64 => {
                let rr = if f32 { self.fs(reg) } else { reg.to_string() };
                self.line(&format!("\tstr {}, [sp, #{}]", rr, off));
            }
        }
    }

    /// M7-e3av：强制写浮点 vreg 的栈槽（绕过驻留映射）。 phi 实参必须落目标块
    /// param 的栈槽，否则后继块入口 reload 读到旧值——与整数 sg_force_stack 对称。
    fn sf_force_stack(&mut self, dst: VReg, reg: &str) {
        let off = self.slot(dst);
        let f32 = self.rty(dst) == IrType::F32;
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                let m = if f32 { "movss" } else { "movsd" };
                self.line(&format!("\t{} {}, {}({})", m, self.r(reg), off, self.r("rsp")));
            }
            EmitTarget::AArch64 => {
                let rr = if f32 { self.fs(reg) } else { reg.to_string() };
                self.line(&format!("\tstr {}, [sp, #{}]", rr, off));
            }
        }
    }

    /// 强制写回 vreg 的栈槽（绕过驻留映射）。phi 实参必须落到目标块 param
    /// 的槽里，否则后继块从槽里读到的是旧值。
    fn sg_force_stack(&mut self, dst: VReg, reg: &str) {
        let off = self.slot(dst);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovq {}, {}({})", self.r(reg), off, self.r("rsp"))),
            EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", reg, off)),
        }
    }

    /// 把任意 Value 写进某个 vreg 的槽（块参数传递 / 跳转实参用）。
    fn sv(&mut self, dst: VReg, v: Value) {
        if self.rty(dst).is_float() {
            let f = self.f1().to_string();
            self.lf(v, &f);
            // M7-e3av：phi 实参必须强制落栈，不能被 sf 的驻留快路径劫持。
            self.sf_force_stack(dst, &f);
        } else {
            let g = self.g1().to_string();
            self.lg(v, &g);
            self.sg_force_stack(dst, &g);
        }
    }

    /// x86-64：把 Value 折叠成「可直接作二元/比较指令源操作数」的字符串。
    ///
    /// 全 spill 布局里每个 vreg 的值就在 `off(%rsp)`，而 x86 的算术指令允许一个
    /// 内存操作数，因此 `lg(b, g2)` 那条搬运在多数情况下可以完全消掉：
    /// `movq off_b(%rsp), %r10; addq %r10, %rax` → `addq off_b(%rsp), %rax`。
    /// 立即数仅在落在 imm32 范围内、且对应指令支持 `reg, imm` 形式时才折叠
    /// （imul/idiv 没有立即数形式，浮点也没有立即数操作数）。
    /// AArch64 是 load/store 架构，一律先 ldr 进临时寄存器。
    fn bin_src(&mut self, v: Value, scratch: &str, allow_imm: bool, fp: bool) -> String {
        if matches!(self.target, EmitTarget::X86_64SysV) {
            match v {
                Value::Imm(i) if allow_imm && !matches!(i, Imm::F64(_) | Imm::F32(_)) => {
                    let n = match i {
                        Imm::I64(n) => n, Imm::I32(n) => n as i64,
                        Imm::I8(n) => n as i64, Imm::I1(b) => b as i64,
                        _ => 0,
                    };
                    if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
                        return format!("${}", n);
                    }
                }
                Value::Reg(r) => {
                    // 驻留 vreg 的栈槽可能已过期，必须读物理寄存器而不是内存。
                    // M7-e3av：浮点操作数查浮点驻留表（xmm），整数查整数表（GPR）。
                    let p = if fp { self.val_fp_phys(r) } else { self.val_phys(r) };
                    if let Some(p) = p { return self.r(p); }
                    // F32 槽只写了低 4 字节，整槽 8 字节读会带入垃圾高位，
                    // 保持 lf 的 movss 路径、不折叠。
                    if !fp || self.rty(r) == IrType::F64 {
                        let off = self.slot(r);
                        return format!("{}({})", off, self.r("rsp"));
                    }
                }
                _ => {}
            }
        }
        if fp { self.lf(v, scratch); } else { self.lg(v, scratch); }
        self.r(scratch)
    }

    /// 取栈上某偏移的地址到整数寄存器。
    fn addr(&mut self, reg: &str, off: i64) {
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tleaq {}({}), {}", off, self.r("rsp"), self.r(reg))),
            EmitTarget::AArch64 => {
                self.mov_imm64("x12", off as u64);
                self.line(&format!("\tadd {}, sp, x12", reg));
            }
        }
    }

    /// x86-64：把 `off(%rsp)` 当作**内存操作数**直接读/写。
    /// 局部变量槽的地址是编译期已知的栈偏移，无需先算地址到寄存器——
    /// 相比「地址寄存器 + 间接访存」每次访问省一条指令。AArch64 的
    /// 立即数偏移有范围限制，不走这条捷径。
    fn load_stack(&mut self, off: i64, val_reg: &str, ty: &IrType) {
        let m = format!("{}({})", off, self.r("rsp"));
        match ty {
            IrType::I1 | IrType::I8 => self.line(&format!("\tmovzbq {}, {}", m, self.r(val_reg))),
            IrType::I32 => self.line(&format!("\tmovl {}, {}", m, self.r(&self.r32(val_reg)))),
            IrType::F64 => self.line(&format!("\tmovsd {}, {}", m, self.r(val_reg))),
            IrType::F32 => self.line(&format!("\tmovss {}, {}", m, self.r(val_reg))),
            _ => self.line(&format!("\tmovq {}, {}", m, self.r(val_reg))),
        }
    }

    fn store_stack(&mut self, off: i64, val_reg: &str, ty: &IrType) {
        let m = format!("{}({})", off, self.r("rsp"));
        match ty {
            IrType::I1 | IrType::I8 => self.line(&format!("\tmovb {}, {}", self.r(&self.r8(val_reg)), m)),
            IrType::I32 => self.line(&format!("\tmovl {}, {}", self.r(&self.r32(val_reg)), m)),
            IrType::F64 => self.line(&format!("\tmovsd {}, {}", self.r(val_reg), m)),
            IrType::F32 => self.line(&format!("\tmovss {}, {}", self.r(val_reg), m)),
            _ => self.line(&format!("\tmovq {}, {}", self.r(val_reg), m)),
        }
    }

    /// 从 `[addr_reg]` 按 ty 宽度读到 `reg`（整数用 g1，浮点用 f1）。
    fn load_mem(&mut self, addr_reg: &str, val_reg: &str, ty: IrType) {
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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

    // ---- 块内寄存器驻留（M7-a2/a3，仅 x86-64） ----

    fn phys(&self, v: VReg) -> Option<&'static str> {
        if self.residency_on { self.resident.get(&v).copied() } else { None }
    }

    /// lg / sg / bin_src 只处理「值」vreg。Alloca 的 dst vreg 存的是地址，
    /// 即便它的**值槽**被提升，也绝不能把地址读成句柄值。
    fn val_phys(&self, v: VReg) -> Option<&'static str> {
        if self.alloca_off.contains_key(&v) { None } else { self.phys(v) }
    }

    /// 驻留项在帧内的回写/装载偏移：提升槽落 Alloca 区，普通值落 vreg 槽。
    fn resident_off(&self, v: VReg) -> i64 {
        self.alloca_off.get(&v).copied().unwrap_or_else(|| self.slot(v))
    }

    /// 含任何会发 call / 可能 longjmp 的指令的块禁用驻留：
    /// longjmp 会把 callee-saved 恢复到 setjmp 时刻的值，而「寄存器比栈新」
    /// 的前提在那条路径上不成立。纯计算块（微基准热循环）不受影响。
    fn block_can_reside(b: &IrBlock) -> bool {
        !b.insts.iter().any(|i| matches!(i,
            IrInst::Call { .. } | IrInst::CallIndirect { .. }
            | IrInst::RcRetain { .. } | IrInst::RcRelease { .. }
            | IrInst::StructRetain { .. } | IrInst::StructRelease { .. }
            | IrInst::EnvInit { .. }))
    }

    /// 普通 vreg 的驻留资格：非浮点、且不是 Alloca 地址槽。
    fn residency_candidate(&self, v: VReg) -> bool {
        matches!(self.f.reg_ty.get(v as usize), Some(t) if !t.is_float())
            && !self.alloca_off.contains_key(&v)
    }

    /// M8-gra-span：LLVM Greedy 分配器 `priority = span × freq` 里的 **span**
    /// ——一个 vreg 作为 def / use / phi 实参出现过的不同基本块数。
    ///
    /// 跨块越多 ⇒ live range 越长 ⇒ 越应优先驻留：长命的循环携带 phi / 累加器
    /// 若被挤出物理寄存器，会在每个块边界反复 spill/reload（b05 的 s、a 每轮
    /// 外层迭代都走 248/256(%rsp)），而单块短命中间值挤占寄存器后几条指令就死。
    /// 本函数只产出跨度，不改变任何绑定/发射语义；排序权重的用法见 analyze_unified。
    fn compute_live_span(&self) -> HashMap<VReg, usize> {
        let mut span: HashMap<VReg, usize> = HashMap::new();
        for b in &self.f.blocks {
            let mut here: HashSet<VReg> = HashSet::new();
            for i in &b.insts {
                if let Some(d) = i.def() { here.insert(d); }
                let mut uses = Vec::new();
                i.reg_uses(&mut uses);
                here.extend(uses);
            }
            match &b.term {
                Term::Jump { args, .. } =>
                    for a in args { if let Value::Reg(r) = a { here.insert(*r); } },
                Term::Branch { cond, then_args, else_args, .. } => {
                    here.insert(*cond);
                    for a in then_args.iter().chain(else_args.iter()) {
                        if let Value::Reg(r) = a { here.insert(*r); }
                    }
                }
                Term::Return { val } =>
                    if let Some(Value::Reg(r)) = val { here.insert(*r); },
                Term::Unreachable => {}
            }
            for v in here { *span.entry(v).or_insert(0) += 1; }
        }
        span
    }

    // ---- M7-e3av：浮点驻留查询（与整数 phys/val_phys 完全独立） ----

    /// 浮点 vreg 是否驻留在物理 xmm。仅 x86-64；非 x86 恒 None（AArch64 零变化）。
    fn fp_phys(&self, v: VReg) -> Option<&'static str> {
        if matches!(self.target, EmitTarget::X86_64SysV) && self.residency_on {
            self.fp_resident.get(&v).copied()
        } else { None }
    }

    /// lf/sf/bin_src 用：同 val_phys，Alloca 的 dst 存地址、绝不当浮点值读。
    fn val_fp_phys(&self, v: VReg) -> Option<&'static str> {
        if self.alloca_off.contains_key(&v) { None } else { self.fp_phys(v) }
    }

    /// 浮点 vreg 的驻留资格：浮点、且不是 Alloca 地址槽（整数侧取反）。
    fn fp_residency_candidate(&self, v: VReg) -> bool {
        matches!(self.f.reg_ty.get(v as usize), Some(t) if t.is_float())
            && !self.alloca_off.contains_key(&v)
    }

    /// 判定哪些 Alloca 可以整槽提升：地址不逃逸、且所有 Load/Store 都是
    /// 8 字节整数宽度。逃逸 = 被当作值传给其它指令 / 块参数 / 返回值，
    /// 或经 FieldAddr / PtrAdd / ARC 间接访问。
    fn collect_promotable(&self) -> HashSet<VReg> {
        let mut cand: HashSet<VReg> = self.alloca_off.keys().copied().collect();
        for b in &self.f.blocks {
            for i in &b.insts {
                match i {
                    IrInst::Load { ptr, ty, .. } => {
                        if !promotable_ty(ty) { cand.remove(ptr); }
                    }
                    IrInst::Store { ptr, val, ty } => {
                        if !promotable_ty(ty) { cand.remove(ptr); }
                        if let Value::Reg(r) = val { cand.remove(r); }
                    }
                    IrInst::Alloca { .. } => {}
                    _ => {
                        let mut uses = Vec::new();
                        i.reg_uses(&mut uses);
                        for u in uses { cand.remove(&u); }
                    }
                }
            }
            match &b.term {
                Term::Jump { args, .. } =>
                    for a in args { if let Value::Reg(r) = a { cand.remove(r); } },
                Term::Branch { cond, then_args, else_args, .. } => {
                    cand.remove(cond);
                    for a in then_args.iter().chain(else_args.iter()) {
                        if let Value::Reg(r) = a { cand.remove(r); }
                    }
                }
                Term::Return { val } =>
                    if let Some(Value::Reg(r)) = val { cand.remove(r); },
                Term::Unreachable => {}
            }
        }
        cand
    }

    /// 寄存器驻留分析总入口（仅 x86-64）。
    ///
    /// 纯计算函数（所有块都不含 call / ARC / EnvInit）走 M7-b 的「全函数统一
    /// 寄存器文件」：至多 10 个池寄存器在整个函数内绑定固定位置，块与块之间不再
    /// flush / load，循环边界的栈往返被彻底消掉。含 call 的非递归函数走 M7-e3v
    /// 的「callee-saved 全局驻留」（6 个 callee-saved 跨 call 持有最新值，消掉
    /// 数组密集内核每轮几十条栈搬运）；递归 SCC 成员与含 EnvInit 的函数仍走
    /// M7-a 的逐块独立驻留（以内存为块边界权威，避免递归栈帧放大保存开销 /
    /// longjmp 破坏「寄存器比栈新」前提）。
    /// M7-e1：全函数活跃性扫描。收集所有「被读取」的 vreg（每条指令的
    /// reg_uses + term 的 cond / 跳转实参 / 返回值），凡是「有 def 却从不
    /// 出现在读取集合」的普通 vreg，其 spill store 就是死代码，sg 整条省略。
    ///
    /// 安全边界：Alloca 的 dst（地址槽，后续 Load/Store 靠它寻址）与
    /// param_slots（C ABI 参数）一律排除；phi 实参走 sv/sg_force_stack，
    /// 不经过 sg；驻留路径（写物理寄存器）在 sg 里先于本判断返回，不改。
    /// reg_uses 的 match 由编译器保证穷尽，覆盖了所有带 Value 操作数的变体，
    /// 因此「∉ live」等价于「函数内没有任何指令 / term 会读它的栈槽」。
    fn analyze_dead_defs(&mut self) {
        let mut live: HashSet<VReg> = HashSet::new();
        for b in &self.f.blocks {
            for i in &b.insts {
                let mut uses = Vec::new();
                i.reg_uses(&mut uses);
                live.extend(uses);
            }
            match &b.term {
                Term::Jump { args, .. } =>
                    for a in args { if let Value::Reg(r) = a { live.insert(*r); } },
                Term::Branch { cond, then_args, else_args, .. } => {
                    live.insert(*cond);
                    for a in then_args.iter().chain(else_args.iter()) {
                        if let Value::Reg(r) = a { live.insert(*r); }
                    }
                }
                Term::Return { val } =>
                    if let Some(Value::Reg(r)) = val { live.insert(*r); },
                Term::Unreachable => {}
            }
        }
        let mut dead = HashSet::new();
        for b in &self.f.blocks {
            for i in &b.insts {
                if let Some(d) = i.def() {
                    if !live.contains(&d)
                        && !self.alloca_off.contains_key(&d)
                        && !self.f.param_slots.iter().any(|s| *s == d)
                    {
                        dead.insert(d);
                    }
                }
            }
        }
        self.dead_defs = dead;
    }

    /// M7-e3b：标记「ICmp 结果全函数仅被一处 Branch.cond 使用」的 vreg。
    /// 这些 ICmp 不必物化为 bool 寄存器（省 setcc/movzbq/testq 三条），直接在
    /// Branch 处发 `cmp; jcc`。条件是 result 的 use 计数恰好为 1 且那唯一一次
    /// 使用就是某个 Branch 的 cond——否则（被 store / call 实参 / 其它计算 /
    /// phi 实参使用，或多次使用）必须保留物化结果。
    fn analyze_fusable(&mut self) {
        // fuse 优化（ICmp 不物化、Branch 处直接 cmp+jcc）只实现了 x86-64 路径。
        // AArch64 的 Branch 走 cbz cond，依赖 ICmp 结果已物化到 cond 槽；若此处
        // 把 ICmp 标为 fusable 跳过物化，cbz 会读未写入的槽（随机值）→ 含 while
        // 条件的程序死循环。实机 qemu 验证抓到；交叉汇编语法校验看不出。
        // AArch64 直接返回，fusable 恒空，所有 ICmp 正常物化（无性能损失，本就无 fuse）。
        if matches!(self.target, EmitTarget::AArch64) {
            return;
        }
        let mut defs: HashMap<VReg, (ACmp, Value, Value)> = HashMap::new();
        for b in &self.f.blocks {
            for i in &b.insts {
                if let IrInst::ICmp { op, dst, a, b, .. } = i {
                    defs.insert(*dst, (*op, *a, *b));
                }
            }
        }
        let mut use_count: HashMap<VReg, usize> = HashMap::new();
        let mut branch_cond: HashSet<VReg> = HashSet::new();
        for b in &self.f.blocks {
            for i in &b.insts {
                let mut uses = Vec::new();
                i.reg_uses(&mut uses);
                for u in uses { *use_count.entry(u).or_insert(0) += 1; }
            }
            match &b.term {
                Term::Jump { args, .. } =>
                    for a in args { if let Value::Reg(r) = a { *use_count.entry(*r).or_insert(0) += 1; } }
                Term::Branch { cond, then_args, else_args, .. } => {
                    *use_count.entry(*cond).or_insert(0) += 1;
                    branch_cond.insert(*cond);
                    for a in then_args.iter().chain(else_args.iter()) {
                        if let Value::Reg(r) = a { *use_count.entry(*r).or_insert(0) += 1; }
                    }
                }
                Term::Return { val } =>
                    if let Some(Value::Reg(r)) = val { *use_count.entry(*r).or_insert(0) += 1; }
                Term::Unreachable => {}
            }
        }
        for (r, info) in &defs {
            if use_count.get(r) == Some(&1) && branch_cond.contains(r) {
                self.fusable.insert(*r, *info);
            }
        }

        // M7-e3k：把 `(a % 2^k) == 0 / != 0` 融合成单条 `testq $(2^k-1), a` + jcc。
        // 与 LLVM/Clang 的 InstCombine 同类变换。等价性：有符号余数向零截断，
        // 余数符号跟随被除数，故 `a % 2^k == 0` ⟺ `2^k | a` ⟺ 补码低 k 位全 0
        // ⟺ `a & (2^k-1) == 0`，对负数同样成立（-4 % 4 == 0、-3 % 4 == -3 != 0）。
        // 命中条件极严：① ICmp 是 Eq/Neq 且一侧为 Imm(0)；② 另一侧恰是某条
        // Mod 的结果 vreg，且该 Mod 的除数是 2 的幂（复用 const_div_pow2 的
        // k∈[1,30] 判定，保证 testq 立即数 2^k-1 是合法正 imm32）；③ 该 Mod
        // 结果全函数只被这一条 ICmp 使用（use_count==1），④ 该 ICmp 结果也只
        // 被这一处 Branch.cond 使用（已在 fusable 中）。任一不满足都必须保留
        // 物化结果供其它使用者读取。仅 x86-64，AArch64 路径逐字节不变。
        if matches!(self.target, EmitTarget::X86_64SysV) {
            let mut mod_defs: HashMap<VReg, (Value, Value)> = HashMap::new();
            for b in &self.f.blocks {
                for i in &b.insts {
                    if let IrInst::Bin { op: ABin::Mod, dst, a, b, .. } = i {
                        mod_defs.insert(*dst, (*a, *b));
                    }
                }
            }
            for (r, (op, ca, cb)) in &defs {
                if !matches!(op, ACmp::Eq | ACmp::Neq) { continue; }
                if !self.fusable.contains_key(r) { continue; }
                let mod_side = if imm_is_zero(*ca) {
                    *cb
                } else if imm_is_zero(*cb) {
                    *ca
                } else {
                    continue;
                };
                let m = match mod_side { Value::Reg(m) => m, _ => continue };
                let (x, c) = match mod_defs.get(&m) { Some(v) => *v, None => continue };
                let k = match const_div_pow2(c) { Some(k) => k, None => continue };
                if use_count.get(&m) != Some(&1) { continue; }
                self.pow2_mod_test.insert(*r, (*op, x, k));
                self.skip_pow2_mod.insert(m);
            }
        }

        // M7-e3l：`(base * c) + disp` → 单条 `leaq disp(base, base, c-1), dst`。
        // 与 LLVM/Clang 的 InstCombine / DAGCombiner 同类变换（b08 的 `3*x+1`
        // 原来发 movabsq $3 + imulq + addq $1，LLVM 发单条 leaq）。
        // 等价性：x86-64 两操作数 `imulq` 与 `leaq` 都只保留 64 位低半（模 2^64
        // 回绕）；`addq $imm32` 与 LEA 的 disp32 都把立即数有符号扩展到 64 位再
        // 相加，因此负数、i64::MIN/MAX 溢出回绕、负 disp 的行为完全一致；LEA
        // 不写标志位，本路径也不依赖标志位。命中条件极严：
        //   ① Mul 与 Add/Sub 在同一基本块内严格相邻（中间不允许任何指令，杜绝
        //      base 被重定义、控制权分叉或 call 破坏临时寄存器）；
        //   ② Mul 的常量因子 c∈{2,3,5,9}（scale = c-1 ∈ {1,2,4,8}）；
        //   ③ Add/Sub 的一侧恰是该 Mul 的 dst、另一侧是 i32 范围内的整数
        //      立即数（Sub 只接受 `m - n`，等价编码为 disp = -n）；
        //   ④ 该 Mul 结果全函数仅被这一条 Add/Sub 使用（use_count == 1）；
        //   ⑤ Add/Sub 的 dst 与该 Mul 的 dst 不同。
        // 任一不满足都保留原来的通用路径。仅 x86-64：AArch64 的分析与发射都
        // 不参与，.s 逐字节不变。
        if matches!(self.target, EmitTarget::X86_64SysV) {
            for b in &self.f.blocks {
                for w in b.insts.windows(2) {
                    let (m, base, scale) = match &w[0] {
                        IrInst::Bin { op: ABin::Mul, dst, a, b: mb, .. } =>
                            match lea_mul_scale(*a, *mb) {
                                Some((base, scale)) => (*dst, base, scale),
                                None => continue,
                            },
                        _ => continue,
                    };
                    let (d, disp) = match &w[1] {
                        IrInst::Bin { op: ABin::Add, dst, a, b: ab, .. } => {
                            let disp = match (a, ab) {
                                (Value::Reg(r), x) if *r == m => lea_disp(*x),
                                (x, Value::Reg(r)) if *r == m => lea_disp(*x),
                                _ => None,
                            };
                            match disp { Some(n) => (*dst, n), None => continue }
                        }
                        // `m - n` == `m + (-n)`：disp32 直接写负数。减法不可交换，
                        // 只接受 a=Reg(m), b=Imm(n)；`n - m` 不命中。
                        IrInst::Bin { op: ABin::Sub, dst, a, b: ab, .. } => {
                            let disp = match (a, ab) {
                                (Value::Reg(r), x) if *r == m => lea_sub_disp(*x),
                                _ => None,
                            };
                            match disp { Some(n) => (*dst, n), None => continue }
                        }
                        _ => continue,
                    };
                    if d == m || use_count.get(&m) != Some(&1) { continue; }
                    self.lea_fuse.insert(d, (base, scale, disp));
                    self.skip_lea_mul.insert(m);
                }
            }
        }
    }

    /// M7-e3m：在每个块内扫描任意「连续 8 条」窗口，识别 popcount 内层
    /// `c = c + (x & 1); x = x >> 1` 形态，登记到 popcnt_fuse。窗口不限整块，
    /// 故 for 循环体即便含 index 递增（块 >8 条）也能命中——与 LLVM 在循环体
    /// 块内任意位置做 DAGCombine 的口径一致。必须在 analyze_residency 之后
    /// 调用（要用 promotable）。命中还需：
    ///   · c / x 两槽都是 promotable 提升槽（发射期还会再确认实际驻留）；
    ///   · 6 个中间 vreg 全函数 use_count 恰为 1（窗口外无人读取，跳过才安全）；
    ///   · 命中后贪心跳过整个窗口（i += 8），避免登记重叠窗口。
    /// 仅 x86-64：AArch64 直接返回，popcnt_fuse 恒空，.s 逐字节不变。
    fn analyze_popcnt(&mut self) {
        if !matches!(self.target, EmitTarget::X86_64SysV) { return; }
        // 重算全函数 use_count（含 term 的 cond / args / return），只用于校验
        // 窗口内 6 个中间 vreg 是否「全函数仅一次使用」。Store 的 ptr 也会
        // 计入，但 pc/px 是槽、不参与本校验，无妨。
        let mut use_count: HashMap<VReg, usize> = HashMap::new();
        for b in &self.f.blocks {
            for i in &b.insts {
                let mut u = Vec::new();
                i.reg_uses(&mut u);
                for v in u { *use_count.entry(v).or_insert(0) += 1; }
            }
            match &b.term {
                Term::Jump { args, .. } =>
                    for a in args { if let Value::Reg(r) = a { *use_count.entry(*r).or_insert(0) += 1; } },
                Term::Branch { cond, then_args, else_args, .. } => {
                    *use_count.entry(*cond).or_insert(0) += 1;
                    for a in then_args.iter().chain(else_args.iter()) {
                        if let Value::Reg(r) = a { *use_count.entry(*r).or_insert(0) += 1; }
                    }
                }
                Term::Return { val } =>
                    if let Some(Value::Reg(r)) = val { *use_count.entry(*r).or_insert(0) += 1; },
                Term::Unreachable => {}
            }
        }
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let n = b.insts.len();
            if n < 8 { continue; }
            let mut i = 0;
            while i + 8 <= n {
                if let Some((pc, px, mids)) = match_popcnt_window(&b.insts[i..i + 8]) {
                    let ok = self.promotable.contains(&pc) && self.promotable.contains(&px)
                        && mids.iter().all(|v| use_count.get(v) == Some(&1));
                    if ok {
                        // M7-e3y：若窗口恰是 `while(x>0){c+=x&1;x>>=1}` 的完整自循环，
                        // 则可用单条 popcnt + x 归零一次算完（回落到 cond 自然退出）。
                        // 判据不满足则仍走 e3m 的 sarq+adcq 逐位路径，行为不变。
                        if self.is_popcount_while(i, b, px) {
                            self.popcnt_native.insert((bi, i));
                        }
                        self.popcnt_fuse.insert((bi, i), (pc, px));
                        i += 8; // 贪心跳过整个窗口，避免登记重叠窗口
                        continue;
                    }
                }
                i += 1;
            }
        }
    }

    /// M7-e3y：确认 body 块 `i` 处的 8 条窗口是 `while(x>0){c+=x&1;x>>=1}` 的完整
    /// 自循环体，因而可用「popcnt 一次算完 + x 归零让 cond 自然退出」替代逐位循环。
    /// 严格判据（任一不符返回 false，安全回落到 e3m 的 sarq+adcq）：
    ///   ① 窗口延伸到 body 块末尾（i+8 == insts.len()）——窗口后无别的 IR；
    ///   ② body 的 term 是无参 Jump 回 header H（回边）；
    ///   ③ H 的 term 是 Branch{cond, then_b == body, else_b != body}；
    ///   ④ cond 由 `ICmp(Gt, read_x, 0)` 产生（self.fusable 里有），其中 read_x
    ///      是 H 内 `Load{ptr: px}` 的结果 vreg——即循环条件读的正是 x 槽；
    ///   ⑤ body 只有一个前驱 header（无别处跳进循环体内部）；header 可有 2 个
    ///      前驱（外层入口首次进入 + body 回边），这是正常嵌套循环。
    /// 这些条件保证：进入 body 的 x 恒为正（否则 cond 不会进 then），逐位循环
    /// 跑完 x 必然为 0；因此 popcnt(x) 一次累加 + x=0 与原循环在所有变量上等价，
    /// 且不改 CFG。
    fn is_popcount_while(&self, i: usize, body: &IrBlock, px: VReg) -> bool {
        // ① 窗口须占满 body 块尾部
        if i + 8 != body.insts.len() { return false; }
        // ② body -> header（无参回边）
        let h = match &body.term {
            Term::Jump { target, args } if args.is_empty() => *target,
            _ => return false,
        };
        let header = match self.f.blocks.iter().find(|b| b.id == h) { Some(b) => b, None => return false };
        // ③ header 的 Branch 条件真时进 body
        let cond = match &header.term {
            Term::Branch { cond, then_b, else_b, .. }
                if *then_b == body.id && *else_b != body.id => *cond,
            _ => return false,
        };
        // ④ cond = ICmp(Gt, read_x, 0)，read_x 是 H 内 Load{ptr: px} 的 dst
        let (op, a, b) = match self.fusable.get(&cond) { Some(v) => *v, None => return false };
        if op != ACmp::Gt { return false; }
        let zero_is_rhs = matches!(b, Value::Imm(Imm::I64(0)) | Value::Imm(Imm::I32(0)));
        let zero_is_lhs = matches!(a, Value::Imm(Imm::I64(0)) | Value::Imm(Imm::I32(0)));
        if !zero_is_rhs && !zero_is_lhs { return false; }
        let read_val = if zero_is_rhs { a } else { b };
        let read_x = match read_val { Value::Reg(r) => r, _ => return false };
        let x_loaded = header.insts.iter().any(|inst| matches!(inst,
            IrInst::Load { dst, ptr, ty } if *dst == read_x && *ptr == px && *ty == IrType::I64));
        if !x_loaded { return false; }
        // ⑤ body 块的唯一前驱是 header（防止别处跳进循环体内部）。
        //    header 自身可有 2 个前驱（外层入口首次进入 + body 回边）——正常嵌套循环。
        let body_preds = self.f.blocks.iter().filter(|blk| match &blk.term {
            Term::Jump { target, .. } => *target == body.id,
            Term::Branch { then_b, else_b, .. } => *then_b == body.id || *else_b == body.id,
            _ => false,
        }).count();
        body_preds == 1
    }

    /// M7-e3m：发射 popcount 融合指令。发射期二次确认——只有 x86-64、驻留态
    /// 开启、c / x 两槽都实际驻留且物理寄存器不同，才发 `sarq $1, x;
    /// adcq $0, c` 并返回 true（调用方跳过整块 8 条 IR）。任一不满足返回
    /// false、且不发射任何指令，由调用方逐条走原路径，行为完全不变。
    /// 两条紧邻：sarq 移出的 bit0（== x & 1，对负数 / i64::MIN 同样成立）
    /// 进 CF，adcq 立即把 CF 加进 c；sarq 同时完成 x >>= 1（算术右移）。
    ///
    /// M7-e3y：native=true 时改发单条 `popcnt` 一次算完整个 x 的置位数并累加到
    /// c，随后把 x 归零——由 is_popcount_while 保证 x 是 `while(x>0)` 的循环变量、
    /// 进入体时恒正、逐位跑完必为 0，故归零即自然结束、cond 读到 0 退出，与逐位
    /// 循环在所有变量上等价且不改 CFG。scratch 用 g1(rax)，须与 rc/rx 都不同，
    /// 否则退回 e3m 逐位路径（x86-64 驻留池是 callee-saved，rax 恒空闲）。
    fn inst_popcnt_fused(&mut self, pc: VReg, px: VReg, native: bool) -> bool {
        if !matches!(self.target, EmitTarget::X86_64SysV) || !self.residency_on { return false; }
        let (rc, rx) = match (self.phys(pc), self.phys(px)) {
            (Some(a), Some(b)) if a != b => (a, b),
            _ => return false,
        };
        if native {
            let scratch = self.g1().to_string(); // "rax"
            if rc != scratch && rx != scratch {
                // popcnt src,dst：dst = popcount(src)，不修改 src。
                self.line(&format!("\tpopcnt {}, {}", self.r(rx), self.r(&scratch)));
                self.line(&format!("\taddq {}, {}", self.r(&scratch), self.r(rc)));
                // x 归零：下一次 cond 读 x==0 自然退出；等价于逐位右移到 0。
                self.line(&format!("\tmovq $0, {}", self.r(rx)));
                self.dirty.insert(pc);
                self.dirty.insert(px);
                return true;
            }
            // scratch 冲突（理论不会发生）：回落逐位路径
        }
        self.line(&format!("\tsarq $1, {}", self.r(rx)));
        self.line(&format!("\tadcq $0, {}", self.r(rc)));
        // 融合直接改了驻留物理寄存器，标记脏以便块尾 flush 正确回写。
        self.dirty.insert(pc);
        self.dirty.insert(px);
        true
    }

    /// M7-e3n：分析并登记 xorshift 步融合。依赖 promotable，须在
    /// analyze_residency 之后。全函数重算 use_count（含 term），窗口内所有
    /// 中间 vreg 必须 use_count==1（跳过才安全），且 x 槽必须 promotable。
    /// 扫描块内任意连续窗口（贪心 i+=win），命中登记到 xorshift_fuse。
    /// 仅 x86-64；AArch64 直接返回。
    fn analyze_xorshift(&mut self) {
        if !matches!(self.target, EmitTarget::X86_64SysV) { return; }
        let mut use_count: HashMap<VReg, usize> = HashMap::new();
        for b in &self.f.blocks {
            for i in &b.insts {
                let mut u = Vec::new();
                i.reg_uses(&mut u);
                for v in u { *use_count.entry(v).or_insert(0) += 1; }
            }
            match &b.term {
                Term::Jump { args, .. } =>
                    for a in args { if let Value::Reg(r) = a { *use_count.entry(*r).or_insert(0) += 1; } },
                Term::Branch { cond, then_args, else_args, .. } => {
                    *use_count.entry(*cond).or_insert(0) += 1;
                    for a in then_args.iter().chain(else_args.iter()) {
                        if let Value::Reg(r) = a { *use_count.entry(*r).or_insert(0) += 1; }
                    }
                }
                Term::Return { val } =>
                    if let Some(Value::Reg(r)) = val { *use_count.entry(*r).or_insert(0) += 1; },
                Term::Unreachable => {}
            }
        }
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let n = b.insts.len();
            let mut i = 0;
            while i < n {
                let mut hit = None;
                for (plan, mids, wlen) in [
                    match_xorshift_mask(&b.insts[i..]),
                    match_xorshift_shift(&b.insts[i..]),
                ].into_iter().flatten() {
                    if self.promotable.contains(&plan_slot(&plan))
                        && mids.iter().all(|v| use_count.get(v) == Some(&1)) {
                        hit = Some((plan, wlen));
                        break;
                    }
                }
                if let Some((plan, wlen)) = hit {
                    self.xorshift_fuse.insert((bi, i), (plan, wlen));
                    i += wlen; // 贪心跳过整个窗口，避免登记重叠窗口
                } else {
                    i += 1;
                }
            }
        }
    }

    /// M7-e3n：发射 xorshift 融合。发射期二次确认——仅 x86-64、驻留态开启、
    /// x 槽确实驻留到物理寄存器才发；否则返回 false、不发射任何指令，由调用
    /// 方逐条走原路径。两种形态都只碰 g1=rax（不在驻留 RES_POOL，安全）与
    /// x 的物理寄存器；结果直接落回 x 的物理寄存器并标记 dirty，块尾回写。
    ///
    /// 形态 A `x = x ^ ((x<<k) & 0xFFFFFFFF)`：
    ///   movq rx,rax; shlq $k,rax; movl %eax,%eax; xorq rax,rx
    /// 形态 B `x = x ^ (x>>k)`（算术右移，与通用 Shr 的 sarq 一致）：
    ///   movq rx,rax; sarq $k,rax; xorq rax,rx
    /// 注意 x 的两个 Load 读到同一值（窗口内两 Load 之间无 Store），故用
    /// rx 的当前值既作 xor 左操作数、又作移位源，与原 IR 完全等价。
    fn inst_xorshift_fused(&mut self, plan: XorshiftPlan) -> bool {
        if !matches!(self.target, EmitTarget::X86_64SysV) || !self.residency_on { return false; }
        let slot = plan_slot(&plan);
        let rx = match self.phys(slot) { Some(p) => p.to_string(), None => return false };
        let g1 = self.g1().to_string(); // rax
        if rx == g1 { return false; }
        self.line(&format!("\tmovq {}, {}", self.r(&rx), self.r(&g1)));
        match plan {
            XorshiftPlan::ShiftMaskXor { k, .. } => {
                self.line(&format!("\tshlq ${}, {}", k, self.r(&g1)));
                let e = self.r(&self.r32(&g1));
                self.line(&format!("\tmovl {}, {}", e, e)); // & 0xFFFFFFFF，零扩展清高位
            }
            XorshiftPlan::ShiftXor { k, .. } => {
                self.line(&format!("\tsarq ${}, {}", k, self.r(&g1)));
            }
        }
        self.line(&format!("\txorq {}, {}", self.r(&g1), self.r(&rx)));
        self.dirty.insert(slot);
        true
    }

    fn analyze_residency(&mut self) {
        self.plans.clear();
        self.used_phys.clear();
        // M7-e3as：含内联汇编的函数禁用一切驻留（全 spill）。内联汇编的模板是
        // 开发者手写的任意汇编，会在发射期独占 rax/rcx/rdx/rsi/rdi/r8/r9 等
        // caller-saved 临时寄存器；若同函数的值驻留在其中会被踩踏。走全 spill
        // 让所有值留在 alloca_off / reg_off 栈槽，asm 操作数池与栈槽互不干扰。
        // 内联汇编是稀有底层原语，性能不敏感；不含 asm 的函数汇编输出零变化。
        let has_asm = self.f.blocks.iter().any(|b|
            b.insts.iter().any(|i| matches!(i, IrInst::InlineAsm { .. })));
        if !matches!(self.target, EmitTarget::X86_64SysV) || self.debug_info
            || self.opt_level < OptLevel::O2 || has_asm {
            // M7-e3w：-g 时禁用一切驻留，所有值留在 alloca_off / reg_off 栈槽，
            // 保证 DWARF 的 DW_OP_fbreg location 指向的就是权威最新值。
            // M7-e3ac：O0/O1 同样禁用驻留（走全 spill）——低优化级别不跨指令保持
            // 值，得到"所写即所得"的代码；popcnt/xorshift 融合依赖 promotable，
            // 随之自动失效。O2（默认）/O3 不受影响。
            self.plans = (0..self.f.blocks.len()).map(|_| BlockPlan::default()).collect();
            return;
        }
        self.promotable = self.collect_promotable();
        let unified = self.f.blocks.len() >= 2
            && self.f.blocks.iter().all(|b| Self::block_can_reside(b));
        if unified {
            self.analyze_unified();
        } else if self.f.blocks.len() >= 2
            && !self.gra_blocked
            // longjmp 会把 callee-saved 恢复到 setjmp 时刻，跨 call 驻留的
            // 「寄存器比栈新」前提在 EnvInit 函数中不成立——保守退回逐块模式。
            && !self.f.blocks.iter().any(|b| b.insts.iter().any(|i| matches!(i, IrInst::EnvInit { .. })))
        {
            self.analyze_unified_with_calls();
        } else {
            self.analyze_per_block();
        }
        self.used_phys.sort_by_key(|p| RES_POOL.iter().position(|x| x == p).unwrap_or(99));
    }

    /// M7-b：全函数统一寄存器文件。所有块的 resident 完全一致，因此任意两
    /// 块之间都是「兼容边」——值天然留在物理寄存器里，无需块尾回写、无需
    /// 块头装载。函数内没有任何 call，池寄存器全程不会被破坏，p 就是提升槽
    /// 与热 vreg 的唯一权威；内存里的 Alloca 区仅在入口处作为参数来源。
    fn analyze_unified(&mut self) {
        let id_to_idx: HashMap<u32, usize> = self.f.blocks.iter().enumerate()
            .map(|(i, b)| (b.id, i)).collect();

        // 回边区间 → 每个块的循环嵌套深度。最内层循环（如 popcount 的 while
        // 体）动态执行次数远高于外层与入口，若按静态指令数平权选 top-N，
        // 热块的中间值会被冷块的值挤出寄存器、退化成栈槽往返。这里按嵌套
        // 深度加权（深度 0/1/2 → 1/8/64），让 top-N 优先覆盖最内层循环。
        let mut intervals: Vec<(usize, usize)> = Vec::new();
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let mut tg = Vec::new();
            match &b.term {
                Term::Jump { target, .. } => tg.push(*target),
                Term::Branch { then_b, else_b, .. } => { tg.push(*then_b); tg.push(*else_b); }
                _ => {}
            }
            for t in tg {
                if let Some(&ti) = id_to_idx.get(&t) {
                    if ti <= bi { intervals.push((ti, bi)); }
                }
            }
        }
        let depth_of = |bi: usize| -> usize {
            intervals.iter().filter(|(lo, hi)| *lo <= bi && bi <= *hi).count()
        };

        // 全局热度：提升槽加权 2、普通 vreg +1，均乘循环深度权重。
        // M7-e3av：浮点 vreg 用独立的 fp_score 并行统计，共用同一深度权重与指令遍历。
        let mut score: HashMap<VReg, usize> = HashMap::new();
        let mut fp_score: HashMap<VReg, usize> = HashMap::new();
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let w = 1usize << (3 * depth_of(bi).min(2));
            for i in &b.insts {
                match i {
                    IrInst::Load { ptr, .. } if self.promotable.contains(ptr) =>
                        { *score.entry(*ptr).or_insert(0) += 2 * w; }
                    IrInst::Store { ptr, .. } if self.promotable.contains(ptr) =>
                        { *score.entry(*ptr).or_insert(0) += 2 * w; }
                    _ => {}
                }
                let mut uses = Vec::new();
                i.reg_uses(&mut uses);
                for u in uses {
                    if self.residency_candidate(u) { *score.entry(u).or_insert(0) += w; }
                    if self.fp_residency_candidate(u) { *fp_score.entry(u).or_insert(0) += w; }
                }
                if let Some(d) = i.def() {
                    if self.residency_candidate(d) { *score.entry(d).or_insert(0) += w; }
                    if self.fp_residency_candidate(d) { *fp_score.entry(d).or_insert(0) += w; }
                }
            }
        }
        // M8-gra-span：LLVM Greedy 的 priority = span × freq。score 已是
        // 「use/def 次数 × 循环深度频率权重」，这里再乘 live-range 跨块跨度，
        // 让长命的循环携带 phi / 累加器优先于单块短命中间值占据池寄存器。
        // baseline（无 profile）与未加权的差别仅在「同深度、多块 vs 单块」的值
        // 之间重排；不改变池大小、块间契约或输出语义。
        let live_span = self.compute_live_span();
        let mut hot: Vec<VReg> = score.keys().copied().collect();
        hot.sort_by(|x, y| {
            let px = score[x] * live_span.get(x).copied().unwrap_or(1);
            let py = score[y] * live_span.get(y).copied().unwrap_or(1);
            py.cmp(&px).then(x.cmp(y))
        });
        let bind: HashMap<VReg, &'static str> = hot.into_iter().take(RES_POOL.len())
            .enumerate().map(|(k, v)| (v, RES_POOL[k])).collect();
        for p in bind.values() {
            if is_callee_saved(p) && !self.used_phys.contains(p) { self.used_phys.push(p); }
        }
        // M7-e3av：浮点值绑定到独立的 xmm10-15 池。绝不进 used_phys——
        // prologue/epilogue 对它一律发 movq 保存，xmm 进去会生成非法指令。
        let mut fp_hot: Vec<VReg> = fp_score.keys().copied().collect();
        fp_hot.sort_by(|x, y| fp_score[y].cmp(&fp_score[x]).then(x.cmp(y)));
        let fp_bind: HashMap<VReg, &'static str> = fp_hot.into_iter().take(FP_RES_POOL.len())
            .enumerate().map(|(k, v)| (v, FP_RES_POOL[k])).collect();

        // 入口块 = 没有任何前驱指向的块。它的参数提升槽由 prologue 写进
        // Alloca 区（内存），需要在入口一次性装载到 p。
        let mut has_pred = vec![false; self.f.blocks.len()];
        for b in &self.f.blocks {
            let mut tg = Vec::new();
            match &b.term {
                Term::Jump { target, .. } => tg.push(*target),
                Term::Branch { then_b, else_b, .. } => { tg.push(*then_b); tg.push(*else_b); }
                _ => {}
            }
            for t in tg { if let Some(&i) = id_to_idx.get(&t) { has_pred[i] = true; } }
        }
        let param_slot: HashSet<VReg> = self.f.param_slots.iter().copied().collect();

        for (idx, b) in self.f.blocks.iter().enumerate() {
            let mut plan = BlockPlan::default();
            plan.resident = bind.clone();
            if !has_pred[idx] {
                for (v, p) in &bind {
                    if param_slot.contains(v) { plan.loads.push((*v, *p)); }
                }
            }
            // phi 参数：write_block_args 把实参写进 param 的栈槽，必须从同一槽
            // 装载到 p，否则块内会读到过期的寄存器值。
            for (pv, _) in &b.params {
                if let Some(p) = bind.get(pv) {
                    if !plan.loads.iter().any(|(v, _)| *v == *pv) {
                        plan.loads.push((*pv, *p));
                    }
                }
            }
            plan.loads.sort_by_key(|(v, _)| *v);
            // M7-e3av：浮点驻留绑定 + phi 装载，与整数同构但用独立池/表。
            plan.fp_resident = fp_bind.clone();
            for (pv, _) in &b.params {
                if let Some(p) = fp_bind.get(pv) {
                    if !plan.fp_loads.iter().any(|(v, _)| *v == *pv) {
                        plan.fp_loads.push((*pv, *p));
                    }
                }
            }
            plan.fp_loads.sort_by_key(|(v, _)| *v);
            // keep_flush / fp_keep_flush 留空：p 全程权威，不回写内存。
            self.plans.push(plan);
        }
    }

    /// M7-e3v：含 call（但无 EnvInit/longjmp）函数的全函数全局驻留。
    /// 逻辑与 analyze_unified 完全一致，唯一差别是绑定池限定为 6 个
    /// callee-saved 寄存器（CALLEE_SAVED_POOL）——它们跨 call 保持原值，
    /// 故「p 全程权威、块间零回写」的前提在含 call 函数里依然成立。
    /// call 的实参由 lg 从绑定寄存器读取、返回值由 sg 写回绑定寄存器，
    /// 且实参寄存器 (rdi/rsi/...) 与临时寄存器 (rax/r10/...) 均不在绑定池，
    /// 不会踩踏驻留值。AArch64 不进此路径（analyze_residency 已挡）。
    fn analyze_unified_with_calls(&mut self) {
        let id_to_idx: HashMap<u32, usize> = self.f.blocks.iter().enumerate()
            .map(|(i, b)| (b.id, i)).collect();

        // 回边区间 → 循环嵌套深度权重（与 analyze_unified 同）。
        let mut intervals: Vec<(usize, usize)> = Vec::new();
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let mut tg = Vec::new();
            match &b.term {
                Term::Jump { target, .. } => tg.push(*target),
                Term::Branch { then_b, else_b, .. } => { tg.push(*then_b); tg.push(*else_b); }
                _ => {}
            }
            for t in tg {
                if let Some(&ti) = id_to_idx.get(&t) {
                    if ti <= bi { intervals.push((ti, bi)); }
                }
            }
        }
        let depth_of = |bi: usize| -> usize {
            intervals.iter().filter(|(lo, hi)| *lo <= bi && bi <= *hi).count()
        };

        let mut score: HashMap<VReg, usize> = HashMap::new();
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let w = 1usize << (3 * depth_of(bi).min(2));
            for i in &b.insts {
                match i {
                    IrInst::Load { ptr, .. } if self.promotable.contains(ptr) =>
                        { *score.entry(*ptr).or_insert(0) += 2 * w; }
                    IrInst::Store { ptr, .. } if self.promotable.contains(ptr) =>
                        { *score.entry(*ptr).or_insert(0) += 2 * w; }
                    _ => {}
                }
                let mut uses = Vec::new();
                i.reg_uses(&mut uses);
                for u in uses {
                    if self.residency_candidate(u) { *score.entry(u).or_insert(0) += w; }
                }
                if let Some(d) = i.def() {
                    if self.residency_candidate(d) { *score.entry(d).or_insert(0) += w; }
                }
            }
        }
        // 注：span×freq 权重只用于无-call 的 analyze_unified。含-call 路径的
        // 6 个 callee-saved 极紧张，实测（M8-gra-span A/B）跨块跨度大的值未必
        // 是热点，乘跨度会挤掉真正热的数组/字符串中转值（b17 +6.4%、b15 +4.7%、
        // b21 +9.4%），故此处保持原 use×depth 排序，不引入 span 因子。
        let mut hot: Vec<VReg> = score.keys().copied().collect();
        hot.sort_by(|x, y| score[y].cmp(&score[x]).then(x.cmp(y)));
        let bind: HashMap<VReg, &'static str> = hot.into_iter().take(CALLEE_SAVED_POOL.len())
            .enumerate().map(|(k, v)| (v, CALLEE_SAVED_POOL[k])).collect();
        for p in bind.values() {
            if !self.used_phys.contains(p) { self.used_phys.push(p); }
        }

        let mut has_pred = vec![false; self.f.blocks.len()];
        for b in &self.f.blocks {
            let mut tg = Vec::new();
            match &b.term {
                Term::Jump { target, .. } => tg.push(*target),
                Term::Branch { then_b, else_b, .. } => { tg.push(*then_b); tg.push(*else_b); }
                _ => {}
            }
            for t in tg { if let Some(&i) = id_to_idx.get(&t) { has_pred[i] = true; } }
        }
        let param_slot: HashSet<VReg> = self.f.param_slots.iter().copied().collect();

        for (idx, b) in self.f.blocks.iter().enumerate() {
            let mut plan = BlockPlan::default();
            plan.resident = bind.clone();
            if !has_pred[idx] {
                for (v, p) in &bind {
                    if param_slot.contains(v) { plan.loads.push((*v, *p)); }
                }
            }
            for (pv, _) in &b.params {
                if let Some(p) = bind.get(pv) {
                    if !plan.loads.iter().any(|(v, _)| *v == *pv) {
                        plan.loads.push((*pv, *p));
                    }
                }
            }
            plan.loads.sort_by_key(|(v, _)| *v);
            self.plans.push(plan);
        }
    }

    /// 逐块独立驻留（M7-a2/a3）：含 call 的函数里，块边界以内存为权威。
    fn analyze_per_block(&mut self) {
        // 计算每个 vreg 在哪些块被读取：只有跨块仍被读的普通 vreg 才需要在
        // 块尾回写栈槽；块内临时值写完就丢，回写纯属浪费。
        let mut used_in: HashMap<VReg, Vec<usize>> = HashMap::new();
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let mut here: HashSet<VReg> = HashSet::new();
            for i in &b.insts {
                let mut uses = Vec::new();
                i.reg_uses(&mut uses);
                here.extend(uses);
            }
            match &b.term {
                Term::Jump { args, .. } =>
                    for a in args { if let Value::Reg(r) = a { here.insert(*r); } },
                Term::Branch { cond, then_args, else_args, .. } => {
                    here.insert(*cond);
                    for a in then_args.iter().chain(else_args.iter()) {
                        if let Value::Reg(r) = a { here.insert(*r); }
                    }
                }
                Term::Return { val } =>
                    if let Some(Value::Reg(r)) = val { here.insert(*r); },
                Term::Unreachable => {}
            }
            for v in here { used_in.entry(v).or_default().push(bi); }
        }
        for (bi, b) in self.f.blocks.iter().enumerate() {
            let mut plan = BlockPlan::default();
            // 太短的块（递归函数体、纯跳转块）装载/回写开销大于收益，不驻留。
            if Self::block_can_reside(b) && b.insts.len() >= 6 {
                let mut score: HashMap<VReg, usize> = HashMap::new();
                let mut first_is_def: HashMap<VReg, bool> = HashMap::new();
                for i in &b.insts {
                    // 提升槽加权 2：消掉一次 Load/Store 的内存往返比临时值更值钱。
                    match i {
                        IrInst::Load { ptr, .. } => if self.promotable.contains(ptr) {
                            *score.entry(*ptr).or_insert(0) += 2;
                            first_is_def.entry(*ptr).or_insert(false);
                        },
                        IrInst::Store { ptr, .. } => if self.promotable.contains(ptr) {
                            *score.entry(*ptr).or_insert(0) += 2;
                            first_is_def.entry(*ptr).or_insert(true);
                        },
                        _ => {}
                    }
                    let mut uses = Vec::new();
                    i.reg_uses(&mut uses);
                    for u in uses {
                        if self.residency_candidate(u) {
                            *score.entry(u).or_insert(0) += 1;
                            first_is_def.entry(u).or_insert(false);
                        }
                    }
                    if let Some(d) = i.def() {
                        if self.residency_candidate(d) {
                            *score.entry(d).or_insert(0) += 1;
                            first_is_def.entry(d).or_insert(true);
                        }
                    }
                }
                let mut hot: Vec<VReg> = score.keys().copied().collect();
                hot.sort_by(|x, y| score[y].cmp(&score[x]).then(x.cmp(y)));
                for (k, v) in hot.into_iter().take(RES_PER_BLOCK).enumerate() {
                    let p = RES_POOL[k];
                    plan.resident.insert(v, p);
                    if is_callee_saved(p) && !self.used_phys.contains(&p) {
                        self.used_phys.push(p);
                    }
                    if !*first_is_def.get(&v).unwrap_or(&true) {
                        plan.loads.push((v, p));
                    }
                }
                plan.loads.sort_by_key(|(v, _)| *v);
                for (v, _) in &plan.resident {
                    if self.alloca_off.contains_key(v) {
                        // 提升槽：别的块 / 含 call 块仍走内存路径，无条件回写。
                        plan.keep_flush.insert(*v);
                    } else if let Some(bs) = used_in.get(v) {
                        if bs.iter().any(|k| *k != bi) { plan.keep_flush.insert(*v); }
                    }
                }
            }
            self.plans.push(plan);
        }
    }

    fn begin_block(&mut self, idx: usize) {
        self.residency_on = false;
        self.resident.clear();
        self.dirty.clear();
        self.flush_mask.clear();
        self.fp_resident.clear();
        self.fp_flush_mask.clear();
        let plan = self.plans.get(idx).cloned().unwrap_or_default();
        if plan.resident.is_empty() && plan.fp_resident.is_empty() { return; }
        for (v, p) in &plan.loads {
            let off = self.resident_off(*v);
            self.line(&format!("\tmovq {}({}), {}", off, self.r("rsp"), self.r(p)));
        }
        // M7-e3av：浮点块入口装载，按 vrty 选 movss/movsd（整数 loads 是 movq）。
        if matches!(self.target, EmitTarget::X86_64SysV) {
            for (v, p) in &plan.fp_loads {
                let off = self.resident_off(*v);
                let m = if self.rty(*v) == IrType::F32 { "movss" } else { "movsd" };
                self.line(&format!("\t{} {}({}), {}", m, off, self.r("rsp"), self.r(p)));
            }
        }
        self.flush_mask = plan.keep_flush;
        self.resident = plan.resident;
        self.fp_flush_mask = plan.fp_keep_flush;
        self.fp_resident = plan.fp_resident;
        self.residency_on = true;
    }

    /// 块尾只回写 plan.keep_flush 里、且确实被弄脏的位置。
    /// M7-b 统一寄存器文件模式下该集合为空——p 是全程权威，块与块之间
    /// 直接继承，零回写；逐块模式下则含提升槽与跨块活跃的普通 vreg。
    /// **不关闭驻留**：term 的 phi 实参 / 返回值要继续从寄存器里读。
    fn flush_residency(&mut self) {
        if !self.residency_on { return; }
        let mut items: Vec<(VReg, &'static str)> =
            self.resident.iter().map(|(v, p)| (*v, *p)).collect();
        items.sort_by_key(|(v, _)| *v);
        for (v, p) in items {
            if self.dirty.contains(&v) && self.flush_mask.contains(&v) {
                let off = self.resident_off(v);
                self.line(&format!("\tmovq {}, {}({})", self.r(p), off, self.r("rsp")));
            }
        }
        self.dirty.clear();
    }

    /// term 发射完毕才真正关闭驻留、清空映射。
    fn end_block(&mut self) {
        self.residency_on = false;
        self.resident.clear();
        self.dirty.clear();
        self.flush_mask.clear();
        self.fp_resident.clear();
        self.fp_flush_mask.clear();
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
        // MS ABI：caller 必须为 4 个寄存器参数预留 32 字节 shadow space，即使
        // 实参全部走寄存器。它位于出站区最底端 [rsp, rsp+32)，栈传参紧随其后。
        // 被调方看到的栈参数偏移因此是 frame+8+32+k*8（见 prologue 的 extra）。
        let outbound = (max_stack as i64) * 8 + self.target.shadow_space() as i64;
        // x86-64：固定预留 5 个 callee-saved 保存槽。固定大小让栈帧布局不依赖
        // 之后的块分析结果；用帧内 mov 保存而非 push，避免 rsp 变动令所有
        // off(%rsp) 偏移失效。
        let save_bytes = match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms =>
                (RES_POOL.iter().filter(|p| is_callee_saved(p)).count() as i64) * 8,
            EmitTarget::AArch64 => 0,
        };
        self.save_off = outbound;
        let mut off = outbound + save_bytes;
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
        // AArch64：x30(LR) 是 caller-saved，任何 bl/blr 调用（Call / CallIndirect /
        // EnvInit→dial_env_init / 浮点转换 / 除零陷阱 do_call 等十余处）都会覆盖它，
        // 末尾 ret 依赖 x30——一旦被覆盖就跳回最后一次调用返回地址、死循环。
        // 因此**统一给所有 AArch64 函数留槽保存/恢复 x30**，不逐指令枚举（易遗漏，
        // 且 EnvInit 这类由 codegen 插入的调用在 IR 扫描里看不见）。
        // 槽放在帧尾（total 处），不影响上面任何 reg_off/alloca_off 的偏移。
        // x86-64 的 call/ret 走栈天然隔离，lr_slot 恒 -1、帧布局零变化。
        self.frame_size = match self.target {
            // 入口 rsp ≡ 8 (mod 16)；sub 后需 ≡ 0 (mod 16) 才能 call。
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => align_up(total, 16) + 8,
            EmitTarget::AArch64 => {
                self.lr_slot = total;
                align_up(total + 8, 16).max(16)
            }
        };
    }

    /// R4/P4.5 插桩：给计数器 idx 加一。两个目标都在「驻留开启前」调用，
    /// 此时 scratch 寄存器无活跃值，且 must-not-clobber 的 callee-saved / SP / LR
    /// 均不碰。
    /// - x86-64：pushfq/popfq 保护 flags，用 incq 内存操作数（不改任何 GPR）。
    /// - AArch64：用 x9/x10（发射器 scratch，非 callee-saved、非传参），adrp+add
    ///   取 .comm 符号地址，ldr/add/str 完成自增；add（非 adds）不改 NZCV，
    ///   无需保存 flags。x9/x10 在插桩点无活跃值（入口在 prologue 后、驻留前；
    ///   循环头在 begin_block 前），用完即弃，不影响后续发射。
    fn pgo_counter_inc(&mut self, idx: usize) {
        if !self.pgo.instrument { return; }
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                self.line("\tpushfq");
                self.line(&format!("\tincq dial_pgo_counters+{}(%rip)", idx * 8));
                self.line("\tpopfq");
            }
            EmitTarget::AArch64 => {
                // 用寄存器偏移而非立即数：AArch64 ldr/str 无符号立即数上限 32760
                // （4095*8），大程序计数器多时可超；寄存器偏移无此限制。
                self.line("\tadrp x9, dial_pgo_counters");
                self.line("\tadd x9, x9, :lo12:dial_pgo_counters");
                self.mov_imm64("x11", (idx * 8) as u64);
                self.line("\tadd x9, x9, x11");
                self.line("\tldr x10, [x9]");
                self.line("\tadd x10, x10, #1");
                self.line("\tstr x10, [x9]");
            }
        }
    }

    /// R4 插桩：main 返回前把计数器写到 profile 文件。
    /// 必须完整保护易失寄存器——返回值的 Value::Reg 可能仍驻留在 rdi/rsi/rax 等
    /// （lg 在 val_phys 命中时直接从物理寄存器搬运，flush_residency 只回写
    /// flush_mask 内的值），故先 push 全部 caller-saved，call 后逆序 pop 复原，
    /// 再让原有的 lg(v)/epilogue 照常执行，返回值不受影响。
    fn pgo_emit_dump(&mut self) {
        if !self.pgo.instrument || self.f.name != "main" { return; }
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                let saved = ["rax", "rcx", "rdx", "rsi", "rdi", "r8", "r9", "r10", "r11"];
                for r in saved.iter() { self.line(&format!("\tpushq {}", self.r(r))); }
                let a0 = self.target.int_arg_regs()[0];
                let a1 = self.target.int_arg_regs()[1];
                self.line(&format!("\tleaq dial_pgo_counters(%rip), {}", self.r(a0)));
                self.line(&format!("\tmovl ${}, {}", self.pgo.counter_count, self.r(&self.r32(a1))));
                self.line(&format!("\tcall dial_pgo_dump{}", plt_suffix()));
                for r in saved.iter().rev() { self.line(&format!("\tpopq {}", self.r(r))); }
            }
            EmitTarget::AArch64 => {
                // 保护全部 caller-saved GPR(x0-x17) + LR(x30)：返回值 val 的 lg
                // 在 dump 之后执行，若其值驻留物理寄存器，bl 会踩踏；且 bl 覆盖
                // x30，而 main 可能无 lr_slot（无 call 的最小程序），必须自保。
                // 20 槽(x0..x17 + x30 + 1 padding)=160 字节，SP 保持 16 对齐。
                self.line("\tsub sp, sp, #160");
                for i in (0..18).step_by(2) {
                    self.line(&format!("\tstp x{}, x{}, [sp, #{}]", i, i + 1, i * 8));
                }
                self.line("\tstr x30, [sp, #144]");
                self.line("\tadrp x0, dial_pgo_counters");
                self.line("\tadd x0, x0, :lo12:dial_pgo_counters");
                self.mov_imm64("x1", self.pgo.counter_count as u64);
                self.line("\tbl dial_pgo_dump");
                self.line("\tldr x30, [sp, #144]");
                for i in (0..18).step_by(2).rev() {
                    self.line(&format!("\tldp x{}, x{}, [sp, #{}]", i, i + 1, i * 8));
                }
                self.line("\tadd sp, sp, #160");
            }
        }
    }

    fn prologue(&mut self) {
        let f = self.f;
        if f.linkage == FuncLink::External {
            self.line(&format!("\t.globl {}", f.name));
        }
        let lbl = format!("{}:", f.name);
        self.line(&lbl);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tsubq ${}, {}", self.frame_size, self.r("rsp"))),
            EmitTarget::AArch64 => {
                self.mov_imm64("x12", self.frame_size as u64);
                self.line("\tsub sp, sp, x12");
                // 含调用的函数保存 LR：bl/blr 会覆盖 x30，末尾 ret 依赖它。
                if self.lr_slot >= 0 {
                    self.line(&format!("\tstr x30, [sp, #{}]", self.lr_slot));
                }
            }
        }
        // 保存本函数用到的 callee-saved 寄存器（帧内槽，不动 rsp）。
        // 参数寄存器（rdi/rsi/...）不在池里，故保存动作不会踩到入参。
        let saved = self.used_phys.clone();
        if self.target.is_x86_64() {
            for (k, p) in saved.iter().enumerate() {
                let off = self.save_off + (k as i64) * 8;
                self.line(&format!("\tmovq {}, {}({})", self.r(p), off, self.r("rsp")));
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
            // M7-e3an：若该槽的 Alloca 已被 DCE 删除（典型：回调从不读的 rawptr<void>
            // state 参数），函数体内不可能有 Load 读它——必须跳过写入。旧代码
            // unwrap_or(&0) 会写到偏移 0，恰好覆盖 x86 的 callee-saved 保存位
            // （save_off=0），破坏 C 运行时回调链里的 rbx/r12/r13，导致 sort_by
            // 插入排序错乱甚至 SIGSEGV。普通程序所有参数槽都被读取而保活，off 必
            // 命中，此分支零影响。
            let off = match self.alloca_off.get(&slot) {
                Some(o) => *o,
                None => continue,
            };
            let src_reg: String;
            match loc {
                ArgLoc::Int(k) => src_reg = self.target.int_arg_regs()[*k].to_string(),
                ArgLoc::Fp(k) => src_reg = self.target.fp_arg_regs()[*k].to_string(),
                ArgLoc::Stack(k) => {
                    // x86：call 压了返回地址，故被调函数看到的栈参数在 [rsp+frame+8+k*8]；
                    // MS ABI 另有 32 字节 shadow space，栈参数在 [rsp+frame+8+32+k*8]；
                    // AArch64：bl 不动 sp，栈参数就在 [sp+frame+k*8]。
                    let extra = match self.target {
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => 8 + self.target.shadow_space() as i64,
                        EmitTarget::AArch64 => 0,
                    };
                    let so = self.frame_size + extra + (*k as i64) * 8;
                    let g = self.g1().to_string();
                    match self.target {
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovq {}({}), {}", so, self.r("rsp"), self.r(&g))),
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => src_reg,
                    EmitTarget::AArch64 => src_reg.replacen('v', "d", 1),
                };
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms =>
                        self.line(&format!("\tmovsd {}, {}({})", self.r(&fr), off, self.r("rsp"))),
                    EmitTarget::AArch64 =>
                        self.line(&format!("\tstr {}, [sp, #{}]", fr, off)),
                }
            } else {
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
        let saved = self.used_phys.clone();
        if self.target.is_x86_64() {
            for (k, p) in saved.iter().enumerate() {
                let off = self.save_off + (k as i64) * 8;
                self.line(&format!("\tmovq {}({}), {}", off, self.r("rsp"), self.r(p)));
            }
        }
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\taddq ${}, {}", self.frame_size, self.r("rsp"))),
            EmitTarget::AArch64 => {
                // 恢复 LR（必须与 prologue 的 str x30 配对，且在任何 sp 变动之前）。
                if self.lr_slot >= 0 {
                    self.line(&format!("\tldr x30, [sp, #{}]", self.lr_slot));
                }
                self.mov_imm64("x12", self.frame_size as u64);
                self.line("\tadd sp, sp, x12");
            }
        }
        self.line(&format!("\t{}", self.target.ret_mnem()));
    }

    // ---- 指令 ----

    fn inst(&mut self, i: &IrInst) {
        match i {
            IrInst::Line(line) => {
                // M7-e3w：-g 时发 .loc（行表由汇编器生成，零机器码）。P1：x86-64 与
                // AArch64 同标准发射，共因调用 dwarf_enabled。
                if dwarf_enabled(self.debug_info, self.target) {
                    self.line(&format!("\t.loc {} {} 0", self.file_index, line));
                }
            }
            IrInst::DebugVar { .. } => {
                // M7-e3w：调试变量名在 emit() 末尾统一生成 DIE（此处零机器码）。
            }
            IrInst::Alloca { dst, .. } => {
                // M7-e3e：可提升（promotable）的 Alloca，其地址值在函数内无任何
                // 消费者——所有 Load/Store 都已提升进寄存器、直接读写提升槽，
                // 不经指针寻址（collect_promotable 已排除地址逃逸 / 间接访问）。
                // 旧实现无条件 addr() 算地址 + sg() 把地址存进 dst 槽：sg 的
                // store 被 elide_dead_stores 当死槽删掉，却留下孤儿 leaq。这里
                // 整段跳过，从源头不发射。AArch64 下 residency 不启用、
                // promotable 恒空，Load 仍按指针寻址，走原路径不受影响。
                if !self.promotable.contains(dst) {
                    let off = *self.alloca_off.get(dst).unwrap_or(&0);
                    let g = self.g2().to_string();
                    self.addr(&g, off);
                    self.sg(*dst, &g);
                }
            }
            IrInst::LeaLabel { dst, lit } => {
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
            IrInst::LeaStaticString { dst, lit } => {
                // 静态 immortal DialString 的对外句柄指向 len 字段 = 对象起点 +8。
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                        self.line(&format!("\tleaq .Ldstr{}+8({}), {}", lit, self.r("rip"), self.r(self.g2())));
                    }
                    EmitTarget::AArch64 => {
                        self.line(&format!("\tadrp {}, .Ldstr{}", self.g3(), lit));
                        self.line(&format!("\tadd {}, {}, :lo12:.Ldstr{}", self.g2(), self.g3(), lit));
                        self.line(&format!("\tadd {}, {}, #8", self.g2(), self.g2()));
                    }
                }
                let g = self.g2().to_string();
                self.sg(*dst, &g);
            }
            IrInst::LeaStaticData { dst, label } => {
                // v1.21：取 .rodata 静态数据标签地址（ARC desc 表）。
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                        self.line(&format!("\tleaq {}({}), {}", label, self.r("rip"), self.r(self.g2())));
                    }
                    EmitTarget::AArch64 => {
                        self.line(&format!("\tadrp {}, {}", self.g3(), label));
                        self.line(&format!("\tadd {}, {}, :lo12:{}", self.g2(), self.g3(), label));
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\taddq ${}, {}", byte_off, self.r(&g))),
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\taddq {}, {}", self.r(&o), self.r(&b))),
                    // dst = base + off，且 sg(dst) 存的是 b：两条目标都要更新 b。
                    // 旧代码误写 `add o, b, b`（o=b+b、b 不变），既算错也没存对新地址
                    // ——实机 qemu 下 `p.offset(1)` 完全失效。修正为 b = b + o。
                    EmitTarget::AArch64 => self.line(&format!("\tadd {}, {}, {}", b, b, o)),
                }
                self.sg(*dst, &b);
            }
            IrInst::Load { dst, ptr, ty } => {
                // M7-a3：局部变量槽已提升进寄存器时，Load 退化成一次寄存器
                // 搬运；dst 若也驻留，连中间寄存器都省掉。
                if self.residency_on && self.alloca_off.contains_key(ptr) {
                    if let Some(pp) = self.phys(*ptr) {
                        if let Some(pd) = self.val_phys(*dst) {
                            if pp != pd {
                                self.line(&format!("\tmovq {}, {}", self.r(pp), self.r(pd)));
                            }
                            self.dirty.insert(*dst);
                            return;
                        }
                        let g = self.g1().to_string();
                        self.line(&format!("\tmovq {}, {}", self.r(pp), self.r(&g)));
                        self.sg(*dst, &g);
                        return;
                    }
                }
                // **局部变量直接栈寻址**：ptr 若是 Alloca 的 dst，其地址是编译期
                // 已知的栈偏移；x86-64 可直接把 off(%rsp) 当内存操作数读，
                // 省掉「先算地址到寄存器、再间接访存」里的一条指令。
                if matches!(self.target, EmitTarget::X86_64SysV) {
                    if let Some(off) = self.alloca_off.get(ptr).copied() {
                        if ty.is_float() {
                            let f = self.f1().to_string();
                            self.load_stack(off, &f, ty);
                            self.sf(*dst, &f);
                        } else {
                            let g = self.g1().to_string();
                            self.load_stack(off, &g, ty);
                            self.sg(*dst, &g);
                        }
                        return;
                    }
                }
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
                // M7-a3：提升槽的 Store 直接写物理寄存器，块尾统一回写内存。
                if self.residency_on && self.alloca_off.contains_key(ptr) {
                    if let Some(pp) = self.phys(*ptr) {
                        let g = self.g1().to_string();
                        let src = self.bin_src(*val, &g, true, false);
                        if src != self.r(pp) {
                            self.line(&format!("\tmovq {}, {}", src, self.r(pp)));
                        }
                        self.dirty.insert(*ptr);
                        return;
                    }
                }
                if matches!(self.target, EmitTarget::X86_64SysV) {
                    if let Some(off) = self.alloca_off.get(ptr).copied() {
                        if ty.is_float() {
                            let f = self.f1().to_string();
                            self.lf(*val, &f);
                            self.store_stack(off, &f, ty);
                        } else {
                            let g = self.g1().to_string();
                            self.lg(*val, &g);
                            self.store_stack(off, &g, ty);
                        }
                        return;
                    }
                }
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
            IrInst::Bin { op, dst, a, b, ty } => {
                // M7-e3l：Add 命中常量乘加融合时，整条 Mul 已由 skip_lea_mul
                // 跳过，这里直接发单条 leaq；否则走通用 inst_bin。
                let mut done = false;
                let mut skip = self.skip_pow2_mod.contains(dst);
                if matches!(self.target, EmitTarget::X86_64SysV) {
                    if let Some((base, scale, disp)) = self.lea_fuse.get(dst).copied() {
                        self.inst_lea_fused(*dst, base, scale, disp);
                        done = true;
                    } else if self.skip_lea_mul.contains(dst) {
                        skip = true;
                    }
                }
                // M7-e3k：Mod 结果只用于一处 `(..) == 0 / != 0` 融合分支时整条跳过，
                // 由 Branch 处直接发 `testq $(2^k-1), a; jcc`（见 term()）。
                if !done && !skip {
                    self.inst_bin(*op, *dst, *a, *b, ty.clone());
                }
            }
            IrInst::FBin { op, dst, a, b, ty } => self.inst_fbin(*op, *dst, *a, *b, ty.clone()),
            IrInst::ICmp { op, dst, a, b, .. } => {
                // M7-e3b：结果只被一处 Branch.cond 用的 ICmp 不物化，等到 Branch
                // 处直接 `cmp; jcc`，省 setcc/movzbq/testq 三条。
                if !self.fusable.contains_key(dst) {
                    self.inst_icmp(*op, *dst, *a, *b);
                }
            }
            IrInst::FCmp { op, dst, a, b, .. } => self.inst_fcmp(*op, *dst, *a, *b),
            IrInst::Un { op, dst, a, ty } => self.inst_un(*op, *dst, *a, ty.clone()),
            IrInst::Conv { kind, dst, a, ty } => self.inst_conv(*kind, *dst, *a, ty.clone()),
            IrInst::Call { dst, func, args, arg_tys, ret_ty, .. } =>
                self.inst_call(*dst, func, args, arg_tys, ret_ty.clone()),
            IrInst::CallIndirect { dst, target, args, arg_tys, ret_ty } =>
                self.inst_call_indirect(*dst, *target, args, arg_tys, ret_ty.clone()),
            IrInst::InlineAsm { dst, template, constraints, dialect, args, arg_tys, ret_ty } =>
                self.inst_inline_asm(*dst, template, constraints, *dialect, args, arg_tys, ret_ty.clone()),
            IrInst::FuncAddr { dst, func } => {
                // 取函数代码地址：leaq <符号>(%rip), %reg（PIE 位置无关）。
                let g = self.g1().to_string();
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tleaq {}(%rip), {}", func, self.r(&g))),
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

    /// M7-e3as：发射内联汇编。对齐 LLVM gen_inline_asm 的调用约定：
    /// 约束串逗号分隔，首项是输出（`=r`/`+r`/`=&r`），其后是输入（`r`）或
    /// 绑定（`0` = 与操作数 0 同寄存器）。操作数编号 = 约束串里的位置：
    /// `$0` 是输出、`$1` 起是输入。`$$` 转义成字面 `$`。
    /// 物理寄存器用 x86-64 的整数实参寄存器池 rdi/rsi/rdx/rcx/r8/r9（不属
    /// GRA 驻留池、也不属发射临时 rax/r10/r11）；含 asm 的函数已强制全 spill，
    /// 故不会踩踏任何 vreg 栈槽宽度之外的值。ATT 方言 `$N` 渲染成 `%reg`，
    /// Intel 方言渲染成裸 `reg`，宽度按操作数 IrType（i8→al/i32→eax/i64→rax）。
    fn inst_inline_asm(&mut self, dst: VReg, template: &str, constraints: &str,
                       dialect: AsmDialect, args: &[Value], arg_tys: &[IrType], ret_ty: IrType) {
        // 实参寄存器池（caller-saved，跨 call 无需保存；asm 块内独占）。
        // x86-64：整数实参寄存器 rdi/rsi/rdx/rcx/r8/r9；
        // AArch64：整数实参寄存器 x0-x5（x6/x7 留给 ABI 边界；含 asm 函数全 spill，
        // 这些寄存器无驻留值，独占安全）。两目标都不用发射器 scratch（x86 rax/r10/r11、
        // AArch64 x9/x10/x11/x12）与 callee-saved。
        let pool: Vec<&str> = match self.target {
            EmitTarget::X86_64SysV => vec!["rdi", "rsi", "rdx", "rcx", "r8", "r9"],
            // MS ABI：rdi/rsi 是 callee-saved，绝不能当 asm 临时；rcx/rdx/r8/r9
            // 是前 4 参数槽，r10/r11 是剩余 caller-saved，全部可安全独占。
            EmitTarget::X86_64Ms => vec!["rcx", "rdx", "r8", "r9", "r10", "r11"],
            EmitTarget::AArch64 => vec!["x0", "x1", "x2", "x3", "x4", "x5"],
        };
        let fallback = match self.target {
            EmitTarget::X86_64SysV => "rdi",
            EmitTarget::X86_64Ms => "rcx",
            EmitTarget::AArch64 => "x0",
        };
        let is_x86 = matches!(self.target, EmitTarget::X86_64SysV | EmitTarget::X86_64Ms);

        // 约束串解析：每项形如 "=r" / "=&r" / "+r" / "r" / "0"。
        // 提取每项的「绑定编号」：纯数字 = 与该编号操作数同寄存器；否则新分配。
        let specs: Vec<&str> = constraints.split(',').map(|s| s.trim()).collect();
        let mut op_reg: Vec<String> = Vec::with_capacity(specs.len());
        let mut pool_idx = 0usize;
        for spec in &specs {
            let tied = spec.parse::<usize>().ok();
            if let Some(t) = tied {
                if t < op_reg.len() {
                    op_reg.push(op_reg[t].clone());
                } else {
                    // 绑定了尚未分配的操作数——防御性退回新寄存器（语义层已保证合法）。
                    op_reg.push(pool[pool_idx.min(pool.len() - 1)].to_string());
                    pool_idx += 1;
                }
            } else {
                op_reg.push(pool[pool_idx.min(pool.len() - 1)].to_string());
                pool_idx += 1;
            }
        }
        // 操作数 0 必须有寄存器（输出）；约束串为空时兜底。
        if op_reg.is_empty() { op_reg.push(fallback.to_string()); }

        // 输入实参从操作数 1 开始（$1, $2, ...），把它们装进各自寄存器。
        // arg_tys[i] 与 args[i] 一一对应，位于操作数 i+1。
        for (i, v) in args.iter().enumerate() {
            let oi = i + 1;
            if oi >= op_reg.len() { break; }
            let reg = op_reg[oi].clone();
            // 按操作数宽度装载：i8/i1 零扩展，其余 lg 统一 64 位。
            // x86：movzbq 从 8 位视图零扩展到 64 位；
            // AArch64：lg 已装入 64 位，用 uxtb wN, wN 截低字节并零扩展到 32 位
            // （写 w 寄存器自动把高 32 位清零，等价于零扩展到 64 位）。
            let ty = arg_tys.get(i).cloned().unwrap_or(IrType::I64);
            if ty == IrType::I8 || ty == IrType::I1 {
                if is_x86 {
                    self.lg(*v, &self.g1().to_string());
                    self.line(&format!("\tmovzbq {}, {}", self.r(&self.r8(&self.g1())), self.r(&reg)));
                } else {
                    self.lg(*v, &reg);
                    let w = self.r32(&reg);
                    self.line(&format!("\tuxtb {}, {}", w, w));
                }
            } else {
                self.lg(*v, &reg);
            }
        }

        // 模板占位符替换：$N → 操作数 N 的寄存器名（按 dialect + 操作数宽度），
        // $$ → 字面 $。逐字符扫描，模板里的换行 / 制表符保持原样。
        let rendered = self.render_asm_template(template, &op_reg, &arg_tys, &ret_ty, dialect);
        // Intel 方言切换仅 x86-64 有意义（AArch64 只有一种汇编语法，忽略方言差异）。
        if is_x86 && dialect == AsmDialect::Intel {
            self.line("\t.intel_syntax noprefix");
        }
        // 模板可能含 \n 多行；按行发出（line() 会加 \n）。跳过空行以免多余空行。
        for ln in rendered.split('\n') {
            if ln.is_empty() { continue; }
            self.line(ln);
        }
        if is_x86 && dialect == AsmDialect::Intel {
            self.line("\t.att_syntax prefix");
        }

        // 输出：操作数 0 的寄存器 → dst 槽。i8 结果高 8 字节可能为垃圾，
        // 用零扩展取低 8 位（对齐 LLVM zext）；其余整槽搬运。
        let out_reg = op_reg[0].clone();
        if ret_ty == IrType::I8 || ret_ty == IrType::I1 {
            if is_x86 {
                self.line(&format!("\tmovzbq {}, {}", self.r(&self.r8(&out_reg)), self.r(&self.g1().to_string())));
                self.sg(dst, &self.g1().to_string());
            } else {
                let w = self.r32(&out_reg);
                self.line(&format!("\tuxtb {}, {}", w, w));
                self.sg(dst, &out_reg);
            }
        } else {
            self.sg(dst, &out_reg);
        }
    }

    /// 把 ASM 模板里的 `$N` / `$$` 替换成物理寄存器名。
    /// ATT 加 `%` 前缀、Intel 不加；宽度按操作数 IrType（i8→r8、i32→r32、其余 r64）。
    /// 操作数 0 的宽度取 ret_ty；操作数 i≥1 取 arg_tys[i-1]（缺失按 i64）。
    fn render_asm_template(&self, template: &str, op_reg: &[String],
                           arg_tys: &[IrType], ret_ty: &IrType, dialect: AsmDialect) -> String {
        let mut out = String::with_capacity(template.len() + 16);
        let bytes = template.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'$' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                    out.push('$');
                    i += 2;
                    continue;
                }
                // 读取 $ 后面的十进制操作数编号（可能多位）。
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() { j += 1; }
                if j > i + 1 {
                    let idx: usize = template[i + 1..j].parse().unwrap_or(0);
                    if let Some(base) = op_reg.get(idx) {
                        let ty: IrType = if idx == 0 {
                            ret_ty.clone()
                        } else {
                            arg_tys.get(idx - 1).cloned().unwrap_or(IrType::I64)
                        };
                        let named = match ty {
                            IrType::I8 | IrType::I1 => self.r8(base),
                            IrType::I32 | IrType::F32 => self.r32(base),
                            _ => base.clone(),
                        };
                        // ATT 的 `%` 前缀是 x86 语法；AArch64 GAS 用裸寄存器名
                        // （x0/w0/s0），任何 dialect 都不加 `%`。
                        let shown = match self.target {
                            t if t.is_x86_64() && dialect == AsmDialect::Att => format!("%{}", named),
                            _ => named,
                        };
                        out.push_str(&shown);
                    } else {
                        // 编号越界：原样保留，交由汇编器报错（语义层已保证合法）。
                        out.push_str(&template[i..j]);
                    }
                    i = j;
                    continue;
                }
            }
            // 逐字节复制（模板是 ASCII 汇编文本；非 ASCII 按字节保留）。
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    /// 发射「有符号 64 位整数 / 常量 d」的乘法逆元序列（仅 x86-64）。
    /// 只用 rax/rdx/r10/r11，均不属 RES_POOL，驻留态下不会破坏任何活跃值；
    /// 被除数常驻 r11（单操作数 imulq 不写它），商留在 rdx。
    fn inst_const_div(&mut self, op: ABin, dst: VReg, a: Value,
                      m: i64, s: u32, add: bool, d: i64) {
        let n = self.g3().to_string();     // r11：被除数，全程保留
        let mr = self.g1().to_string();    // rax：魔数 / 余数结果
        let tmp = self.g2().to_string();   // r10：符号掩码 / q*d
        self.lg(a, &n);
        self.line(&format!("\tmovabsq ${}, {}", m, self.r(&mr)));
        self.line(&format!("\timulq {}", self.r(&n))); // rdx:rax = m*n
        if add {
            self.line(&format!("\taddq {}, {}", self.r(&n), self.r("rdx")));
        }
        if s > 0 {
            self.line(&format!("\tsarq ${}, {}", s, self.r("rdx")));
        }
        self.line(&format!("\tmovq {}, {}", self.r(&n), self.r(&tmp)));
        self.line(&format!("\tsarq $63, {}", self.r(&tmp)));
        self.line(&format!("\tsubq {}, {}", self.r(&tmp), self.r("rdx"))); // rdx = 商
        match op {
            ABin::Div => self.sg(dst, "rdx"),
            ABin::Mod => {
                // d <= i32::MAX，imulq 三操作数立即数为 imm32。
                self.line(&format!("\timulq ${}, {}, {}", d, self.r("rdx"), self.r(&tmp)));
                self.line(&format!("\tmovq {}, {}", self.r(&n), self.r(&mr)));
                self.line(&format!("\tsubq {}, {}", self.r(&tmp), self.r(&mr)));
                self.sg(dst, &mr);
            }
            _ => unreachable!(),
        }
    }

    /// M7-e3i：常量除数为 2 的幂（2..=2^30）时的有符号向零截断位移序列。
    /// 与 gcc -O2 / clang 同构，只用 r11(被除数)/rax(结果)/r10(bias)，均不属
    /// RES_POOL，驻留态下不会破坏任何活跃值。`bias = n<0 ? 2^k-1 : 0`：
    ///   Div: q = (n + bias) >> k            （算术右移 sarq）
    ///   Mod: t = (n + bias) & (2^k-1); r = t - bias
    /// 相比 M7-d 魔数逆元省掉 movabsq(64 位立即数) + imulq(乘法依赖链) +
    /// 符号修正等 8~10 条指令，是 3n+1 这类「内层每轮都要 %2 / /2」负载的关键。
    fn inst_const_div_pow2(&mut self, op: ABin, dst: VReg, a: Value, k: u32) {
        let n = self.g3().to_string();    // r11：被除数，全程保留
        let res = self.g1().to_string();  // rax：商 / 余数结果
        let bias = self.g2().to_string(); // r10：符号修正 bias
        self.lg(a, &n);
        // bias = ((n 算术>> 63) 逻辑>> (64-k)) = n<0 ? (2^k-1) : 0
        self.line(&format!("\tmovq {}, {}", self.r(&n), self.r(&bias)));
        self.line(&format!("\tsarq $63, {}", self.r(&bias)));
        self.line(&format!("\tshrq ${}, {}", 64 - k, self.r(&bias)));
        match op {
            ABin::Div => {
                self.line(&format!("\tmovq {}, {}", self.r(&n), self.r(&res)));
                self.line(&format!("\taddq {}, {}", self.r(&bias), self.r(&res)));
                self.line(&format!("\tsarq ${}, {}", k, self.r(&res)));
            }
            ABin::Mod => {
                let mask = (1i64 << k) - 1; // k<=30，是合法 imm32 正数
                self.line(&format!("\tmovq {}, {}", self.r(&n), self.r(&res)));
                self.line(&format!("\taddq {}, {}", self.r(&bias), self.r(&res)));
                self.line(&format!("\tandq ${}, {}", mask, self.r(&res)));
                self.line(&format!("\tsubq {}, {}", self.r(&bias), self.r(&res)));
            }
            _ => unreachable!(),
        }
        self.sg(dst, &res);
    }

    /// M7-e3j：常量位与掩码 n∈[2^31, 2^32-1] 的 32 位强度削减。
    /// 用 g1=rax 及其 32 位视图 eax：lg 把 a 装入 rax，andl $n,%eax（32 位
    /// 立即数不符号扩展，且 32 位写自动零扩展清高 32 位，正是 x & n 的语义），
    /// 再 sg 回写。n=0xFFFFFFFF 时用一条 movl %eax,%eax（LLVM 对「取低 32 位」
    /// 的标准同构输出）。只碰 g1，不触碰 RES_POOL，驻留态下无需额外保存。
    fn inst_and_u32(&mut self, dst: VReg, a: Value, n: i64) {
        let g1 = self.g1().to_string();          // rax
        let d = self.r(&self.r32(&g1));          // %eax
        self.lg(a, &g1);
        if n == 0xFFFF_FFFFi64 {
            self.line(&format!("\tmovl {}, {}", d, d));
        } else {
            self.line(&format!("\tandl ${}, {}", n, d));
        }
        self.sg(dst, &g1);
    }

    /// M7-e3l：把 `(base * c) ± disp` 发成单条 `leaq disp(base, base, c-1), dst`
    /// （Sub 形态已由 `lea_sub_disp` 折算成负 disp）。
    /// base 若是驻留 vreg 可直接用它的物理寄存器作 LEA 的 base/index；dst 若
    /// 也驻留则结果直接落进它的物理寄存器——两者同址时即最优的就地更新
    /// `leaq 1(%rbx, %rbx, 2), %rbx`（与 LLVM 对 `3*x+1` 的发射一致）。否则
    /// 先把 base 读进 g1，再由 sg 负责把结果落到栈槽 / 物理寄存器。
    fn inst_lea_fused(&mut self, dst: VReg, base: Value, scale: u32, disp: i64) {
        let g1 = self.g1().to_string();
        let base_phys = if self.residency_on {
            match base { Value::Reg(v) => self.val_phys(v).map(|p| p.to_string()), _ => None }
        } else { None };
        let base_r = match base_phys {
            Some(p) => self.r(&p),
            None => { self.lg(base, &g1); self.r(&g1) }
        };
        let dst_reg = if self.residency_on {
            self.val_phys(dst).map(|p| p.to_string()).unwrap_or_else(|| g1.clone())
        } else { g1.clone() };
        let disp_s = if disp == 0 { String::new() } else { disp.to_string() };
        self.line(&format!("\tleaq {}({}, {}, {}), {}",
            disp_s, base_r, base_r, scale, self.r(&dst_reg)));
        self.sg(dst, &dst_reg);
    }

    /// M7-e3ae：Fail-Fast 整数除零陷阱。Debug 用可被调试器当断点的 int3 / brk #1
    /// （对齐 LLVM `llvm.debugtrap` → SIGTRAP），Safe 用更短的 ud2 / brk #0
    /// （对齐 `llvm.trap` → SIGILL）。刻意与 `Term::Unreachable` 的无条件
    /// ud2 / brk #0 分开实现，后者不动，避免影响非除零程序的黄金 `.s`。
    fn emit_div_trap(&mut self) {
        let debug = self.safety == SafetyMode::Debug;
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(if debug { "\tint3" } else { "\tud2" }),
            EmitTarget::AArch64 => self.line(if debug { "\tbrk #1" } else { "\tbrk #0" }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn inst_bin(&mut self, op: ABin, dst: VReg, a: Value, b: Value, _ty: IrType) {
        // M7-e3j：常量位与掩码 n∈[2^31, 2^32-1] 时，andq $imm32 会把立即数
        // 符号扩展成负数（0xFFFFFFFF_8xxxxxxx）而语义错误，通用路径只能退回
        // movabsq 装 64 位掩码再 andq（2 条、13 字节）。但这类 n 的高 32 位
        // 本就为 0，x & n 的结果高 32 位必为 0 —— 恰好等于「低 32 位按位与 +
        // 零扩展清高位」，一条 32 位 andl（或 n=0xFFFFFFFF 时的 movl）即可，
        // 与 LLVM/clang 同构。const_and_u32 内含 i64 逐值自校验。仅 x86-64。
        if matches!(self.target, EmitTarget::X86_64SysV)
            && matches!(op, ABin::BitAnd) {
            if let Some(n) = const_and_u32(b) {
                self.inst_and_u32(dst, a, n);
                return;
            }
        }
        // M7-e3i：常量除数为 2 的幂时优先走位移序列，比魔数逆元少 8~10 条
        // 指令且无 imulq 乘法依赖链；const_div_pow2 内含逐值自校验，不通过
        // 或非 2 的幂就继续往下走 M7-d 魔数逆元 / idivq。仅 x86-64。
        if matches!(self.target, EmitTarget::X86_64SysV)
            && matches!(op, ABin::Div | ABin::Mod) {
            if let Some(k) = const_div_pow2(b) {
                self.inst_const_div_pow2(op, dst, a, k);
                return;
            }
        }
        // M7-d：常量除数的乘法逆元。idivq 是 20–90 周期的微码指令，而
        // 每个基准的收尾几乎都有 `% 1000000007`；仅 x86-64、自校验通过才启用。
        if matches!(self.target, EmitTarget::X86_64SysV)
            && matches!(op, ABin::Div | ABin::Mod) {
            if let Some((m, s, add, d)) = const_div_magic(b) {
                self.inst_const_div(op, dst, a, m, s, add, d);
                return;
            }
        }
        // M7-e3au：imul 亦有 imm32 形式——x86-64 的 `imulq $imm32, r/m64, r64`
        // （GAS 对两操作数 `imulq $imm32, r/m64` 自动编码为隐含目的的三操作数形式），
        // 且 imm32 按符号扩展参与 64 位乘法，与「先 movabsq 装 64 位再 imulq reg」
        // 在 int64 低 64 位语义上完全等价（已用 cc 汇编 + objdump 验证正/负 imm32）。
        // 因此把 Mul 一并纳入立即数折叠：常量落在 imm32 时省掉一条 movabsq（10 字节）
        // 与一个临时寄存器。idiv/浮点仍无立即数形式，维持装寄存器；移位走下方
        // shift_imm 折叠路径（不在 allow_imm 集合内）。
        // AArch64 的 bin_src 不处理 Imm（恒走 lg/lf），此开关只作用于 x86-64。
        let allow_imm = matches!(op,
            ABin::Add | ABin::Sub | ABin::Mul | ABin::And | ABin::Or |
            ABin::BitAnd | ABin::BitOr | ABin::BitXor |
            ABin::Eq | ABin::Neq | ABin::Lt | ABin::Gt | ABin::Le | ABin::Ge);
        // M7-a2 快路径：dst 驻留时把结果直接算进它的物理寄存器。
        // `acc = acc + x` 这类累加只需一条 `addq`，不再两头搬运栈槽。
        if matches!(self.target, EmitTarget::X86_64SysV) && self.residency_on {
            if let Some(pd) = self.val_phys(dst) {
                // Div/Mod 强制 rax:rdx，交给通用路径。
                if !matches!(op, ABin::Div | ABin::Mod) {
                    let scratch = self.g2().to_string();
                    let a_src = self.bin_src(a, &scratch, true, false);
                    let pd_r = self.r(pd);
                    if a_src != pd_r {
                        self.line(&format!("\tmovq {}, {}", a_src, pd_r));
                    }
                    // M7-c1：常量移位量直接编码为 imm8，跳过 %cl 变址装载。
                    let simm = if matches!(op, ABin::Shl | ABin::Shr) { shift_imm(b) } else { None };
                    if let Some(n) = simm {
                        let m = if matches!(op, ABin::Shl) { "shlq" } else { "sarq" };
                        self.line(&format!("\t{} ${}, {}", m, n, pd_r));
                        self.dirty.insert(dst);
                        return;
                    }
                    let b_src = self.bin_src(b, &scratch, allow_imm, false);
                    match op {
                        ABin::Add => self.line(&format!("\taddq {}, {}", b_src, pd_r)),
                        ABin::Sub => self.line(&format!("\tsubq {}, {}", b_src, pd_r)),
                        ABin::Mul => self.line(&format!("\timulq {}, {}", b_src, pd_r)),
                        ABin::BitAnd | ABin::And => self.line(&format!("\tandq {}, {}", b_src, pd_r)),
                        ABin::BitOr | ABin::Or => self.line(&format!("\torq {}, {}", b_src, pd_r)),
                        ABin::BitXor => self.line(&format!("\txorq {}, {}", b_src, pd_r)),
                        ABin::Shl => {
                            self.line(&format!("\tmovq {}, {}", b_src, self.r("rcx")));
                            self.line(&format!("\tshlq {}, {}", self.r("cl"), pd_r));
                        }
                        ABin::Shr => {
                            self.line(&format!("\tmovq {}, {}", b_src, self.r("rcx")));
                            self.line(&format!("\tsarq {}, {}", self.r("cl"), pd_r));
                        }
                        ABin::Eq | ABin::Neq | ABin::Lt | ABin::Gt | ABin::Le | ABin::Ge => {
                            self.line(&format!("\tcmpq {}, {}", b_src, pd_r));
                            self.line(&format!("\t{} {}", x86_setcc(abin_cmp(op)), self.r("al")));
                            self.line(&format!("\tmovzbq {}, {}", self.r("al"), pd_r));
                        }
                        ABin::Div | ABin::Mod => unreachable!(),
                    }
                    self.dirty.insert(dst);
                    return;
                }
            }
        }
        let g1 = self.g1().to_string();
        let g2 = self.g2().to_string();
        self.lg(a, &g1);
        // M7-c1 / P4.6：常量移位量折叠双目标共用。shift_imm 已保证 n ∈ [0,63]，
        // 正是 x86 imm8 与 AArch64 立即数移位都接受的范围（超出则退回原路径，
        // 保留 count&63 的变址语义）。
        //   x86-64 : shlq/sarq $n, reg       —— 跳过 %cl 装载
        //   AArch64: lsl/asr reg, reg, #n    —— 跳过把移位量装 g2 再 lslv/asrv
        let simm = if matches!(op, ABin::Shl | ABin::Shr) { shift_imm(b) } else { None };
        if let Some(n) = simm {
            match self.target {
                EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                    let a_reg = self.r(&g1);
                    let m = if matches!(op, ABin::Shl) { "shlq" } else { "sarq" };
                    self.line(&format!("\t{} ${}, {}", m, n, a_reg));
                }
                EmitTarget::AArch64 => {
                    let m = if matches!(op, ABin::Shl) { "lsl" } else { "asr" };
                    self.line(&format!("\t{} {}, {}, #{}", m, g1, g1, n));
                }
            }
            self.sg(dst, &g1);
            return;
        }
        let b_reg = self.bin_src(b, &g2, allow_imm, false);
        // M7-e3ae：Fail-Fast 整数除零检查，与 LLVM 后端 emit_div_zero_check 同口径。
        // safety != ReleaseFast 时：除数为 0 当场陷阱，绝不静默产出 poison（LLVM）
        // 或让 AArch64 sdiv 返回 0（静默错误数据，违反 §20）。常量除数 0/1 已被
        // const_div_pow2 / const_div_magic（均要求 d>=2）挡在通用路径之外，故只有
        // 真正落到 idivq/sdiv 的非常量除数才会走到这里——检查精准且零误报。
        if matches!(op, ABin::Div | ABin::Mod) && self.safety != SafetyMode::ReleaseFast {
            let ok_lbl = format!(".{}_dchk_ok_{}", self.prefix, self.dchk_seq);
            self.dchk_seq += 1;
            match self.target {
                EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                    // 必须用 cmpq $0, r/m64：b_reg 可能是 `off(%rsp)` 内存操作数，
                    // testq b_reg,b_reg 在双内存操作数时是非法汇编。
                    self.line(&format!("\tcmpq $0, {}", b_reg));
                    self.line(&format!("\tjne {}", ok_lbl));
                    self.emit_div_trap();
                    self.line(&format!("{}:", ok_lbl));
                }
                EmitTarget::AArch64 => {
                    // AArch64 的 bin_src 恒把 b 装进 g2（寄存器），cbnz 直接可用。
                    self.line(&format!("\tcbnz {}, {}", g2, ok_lbl));
                    self.emit_div_trap();
                    self.line(&format!("{}:", ok_lbl));
                }
            }
        }
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                let a_reg = self.r(&g1);
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

    fn inst_fbin(&mut self, op: ABin, dst: VReg, a: Value, b: Value, ty: IrType) {
        let f1 = self.f1().to_string();
        let f2 = self.f2().to_string();
        // F32 运算必须用单精度指令（addss/subss/mulss/divss）：此前恒用双精度
        // addsd 等，把 f32 位模式当 double 读——`float32(1.5)+float32(0.25)` 得 0.0
        // （scalar32_probe.dial line 38，FFI32 门禁红点）。F32 操作数已在 xmm 低 32 位
        // （lf 对 F32 用 movss），只差运算指令后缀。
        let suf = if ty == IrType::F32 { "ss" } else { "sd" };
        let fop = |op: ABin| -> &'static str {
            match op {
                ABin::Add => "add", ABin::Sub => "sub", ABin::Mul => "mul",
                ABin::Div | ABin::Mod => "div", _ => "add",
            }
        };
        // M7-e3av：dst 驻留快路径（镜像整数 inst_bin）——a 直接读进 dst 的物理
        // xmm，运算落在物理寄存器，结果不经过 xmm8 scratch、也不写回栈槽。
        // b 若是常量仍由 bin_src 物化到 f2=xmm9；若驻留则直接返回其物理 xmm。
        if matches!(self.target, EmitTarget::X86_64SysV) {
            if let Some(pd) = self.val_fp_phys(dst) {
                self.lf(a, pd);
                let b_reg = self.bin_src(b, &f2, false, true);
                let pd_r = self.r(pd);
                self.line(&format!("\t{}{} {}, {}", fop(op), suf, b_reg, pd_r));
                self.dirty.insert(dst);
                return;
            }
        }
        self.lf(a, &f1);
        let b_reg = self.bin_src(b, &f2, false, true);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                let a_reg = self.r(&f1);
                self.line(&format!("\t{}{} {}, {}", fop(op), suf, b_reg, a_reg));
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
        // M7-a2 快路径：a 与 dst 都驻留时，cmp 直接用 a 的物理寄存器，
        // 结果 zero-extend 到 dst 的物理寄存器，全程不碰栈。
        if matches!(self.target, EmitTarget::X86_64SysV) && self.residency_on {
            if let Some(pd) = self.val_phys(dst) {
                if let Value::Reg(ar) = a {
                    if let Some(pa) = self.val_phys(ar) {
                        let scratch = self.g2().to_string();
                        let b_src = self.bin_src(b, &scratch, true, false);
                        self.line(&format!("\tcmpq {}, {}", b_src, self.r(pa)));
                        self.line(&format!("\t{} {}", x86_setcc(op), self.r("al")));
                        self.line(&format!("\tmovzbq {}, {}", self.r("al"), self.r(pd)));
                        self.dirty.insert(dst);
                        return;
                    }
                }
            }
        }
        let g1 = self.g1().to_string();
        let g2 = self.g2().to_string();
        self.lg(a, &g1);
        let b_reg = self.bin_src(b, &g2, true, false);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                self.line(&format!("\tcmpq {}, {}", b_reg, self.r(&g1)));
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
        let b_reg = self.bin_src(b, &f2, false, true);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                self.line(&format!("\tucomisd {}, {}", b_reg, self.r(&f1)));
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
                EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                    let zero = self.f2().to_string(); // SysV xmm9 / MS xmm5（避开 callee-saved）
                    self.line(&format!("\tmovabsq $0, {}", self.r("rax")));
                    self.line(&format!("\tmovq {}, {}", self.r("rax"), self.r(&zero)));
                    self.line(&format!("\tsubsd {}, {}", self.r(&zero), self.r(&f1)));
                }
                EmitTarget::AArch64 => {
                    self.line("\tfmov d17, xzr");
                    self.line(&format!("\tfsub {}, d17, {}", f1, f1));
                }
            }
            self.sf(dst, &f1);
            return;
        }
        // M7-a2 快路径：dst 驻留时一元运算就地改它的物理寄存器。
        if matches!(self.target, EmitTarget::X86_64SysV) && self.residency_on {
            if let Some(pd) = self.val_phys(dst) {
                let scratch = self.g2().to_string();
                let a_src = self.bin_src(a, &scratch, true, false);
                let pd_r = self.r(pd);
                if a_src != pd_r {
                    self.line(&format!("\tmovq {}, {}", a_src, pd_r));
                }
                match op {
                    AUn::Neg => self.line(&format!("\tnegq {}", pd_r)),
                    AUn::BitNot => self.line(&format!("\tnotq {}", pd_r)),
                    AUn::Not => {
                        self.line(&format!("\ttestq {}, {}", pd_r, pd_r));
                        self.line(&format!("\tsete {}", self.r("al")));
                        self.line(&format!("\tmovzbq {}, {}", self.r("al"), pd_r));
                    }
                }
                self.dirty.insert(dst);
                return;
            }
        }
        let g1 = self.g1().to_string();
        self.lg(a, &g1);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => match op {
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tcvtss2sd {}, {}", self.r(&f1), self.r(&f1))),
                    EmitTarget::AArch64 => self.line(&format!("\tfcvt {}, {}", f1, self.fs(&f1))),
                }
                self.sf(dst, &f1);
            }
            AConv::FpTrunc => {
                let f1 = self.f1().to_string();
                self.lf(a, &f1);
                match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tcvtsd2ss {}, {}", self.r(&f1), self.r(&f1))),
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
                            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovslq {}, {}", self.r(&self.r32(&g1)), self.r(&g1))),
                            EmitTarget::AArch64 => self.line(&format!("\tsxtw {}, {}", g1, self.r32(&g1))),
                        },
                        IrType::I1 | IrType::I8 => match self.target {
                            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovsbq {}, {}", self.r(&self.r8(&g1)), self.r(&g1))),
                            EmitTarget::AArch64 => self.line(&format!("\tsxtb {}, {}", g1, self.r32(&g1))),
                        },
                        _ => {}
                    }
                }
                if kind == AConv::Trunc {
                    match ty {
                        IrType::I32 => match self.target {
                            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovl {}, {}", self.r(&self.r32(&g1)), self.r(&self.r32(&g1)))),
                            EmitTarget::AArch64 => self.line(&format!("\tmov {}, {}", self.r32(&g1), self.r32(&g1))),
                        },
                        IrType::I1 | IrType::I8 => match self.target {
                            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tandq $255, {}", self.r(&g1))),
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
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
                EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
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
        // ── 热路径内联：dial_array_get(a, i) ────────────────────────────────
        // 背景：数组下标 a[i] 在 lower 里降级为对 C runtime 的 dial_array_get 调用。
        // 其函数体在 ReleaseFast 下（DIAL_SAFETY=0，边界检查编译期消除）等价于一条
        // `load [a + 24 + i*8]`——数组 payload 布局为 [kind@0][len@8][cap@16][elems@24]。
        // 手写汇编调用预编译 C 函数无法被链接期 LTO 内联，故每次读都要付 call/ret 与
        // ABI 开销；这正是 LLVM 后端靠 --fast 的 LTO 吃掉、而 asm 长期落后的部分
        // （数组内存类微基准慢 ~2x）。在「LLVM 不存在」的总纲下，正解是 asm 自己内联。
        //
        // 只在语义完全等价时内联：
        //   · x86-64 SysV：发射序列针对该 ABI；AArch64 保持原 call（语义不变）。
        //   · ReleaseFast：此时 C 侧边界检查已被编译期消除（越界=UB），纯 load 与之一致；
        //     safe/debug 保留原 call，Fail-Fast 边界检查绝不绕过。
        //   · 签名严格为 [Handle, I64] -> I64，且 dst 存在（读值必被使用）。
        // 用 g1/g2（rax/r10）发射临时——它们不在 GRA 驻留池，不会踩踏任何 vreg。
        if func == "dial_array_get"
            && self.target == EmitTarget::X86_64SysV
            && self.safety == SafetyMode::ReleaseFast
            && dst.is_some()
            && args.len() == 2
            && arg_tys.len() == 2
            && arg_tys[0] == IrType::Handle
            && arg_tys[1] == IrType::I64
            && !ret_ty.is_float()
        {
            let g1 = self.g1().to_string();
            let g2 = self.g2().to_string();
            // rax = a（payload 指针，Handle 位模式即地址）
            self.lg(args[0], &g1);
            // r10 = i
            self.lg(args[1], &g2);
            // r10 = i * 8（元素宽 8 字节）
            self.line(&format!("\tshlq $3, {}", self.r(&g2)));
            // rax = a + i*8
            self.line(&format!("\taddq {}, {}", self.r(&g2), self.r(&g1)));
            // rax = load[rax + 24]（跳过 [kind][len][cap] 三个 int64 头）
            self.line(&format!("\tmovq 24({}), {}", self.r(&g1), self.r(&g1)));
            // 返回值落回 dst 槽——与原 call 的 `sg(dst, ret_int_reg)` 语义完全一致。
            self.sg(dst.unwrap(), &g1);
            return;
        }
        // M7-e3 续：ReleaseFast 下内联 dial_array_set 的「独占 int[] 快路径」。
        // dial_array_set(a,i,v) 在独占(refcount==1)且元素为 int(kind==0)时，C 侧既不
        // COW 克隆也不 retain/release 元素，热路径等价于 `a_elems(a)[i]=v; return a;`
        // ——即一条 `mov v,[a+24+i*8]`。但共享或引用元素必须回退原 call 保留 COW/ARC。
        // 故生成条件快路径：rdi/rsi/rdx 装好实参（回退 call 直接复用），检查 rc==1 且
        // kind==0，满足则直写并返回 a，否则 call。仅 x86-64 + ReleaseFast；safe/debug
        // 保留原 call（Fail-Fast 边界检查不绕过），AArch64 原样。
        // 受控实验：冒泡内层 set 调用开销占 ~80%，是本类别最高 ROI 点。
        if func == "dial_array_set"
            && self.target == EmitTarget::X86_64SysV
            && self.safety == SafetyMode::ReleaseFast
            && dst.is_some()
            && args.len() == 3
            && arg_tys.len() == 3
            && arg_tys[0] == IrType::Handle
            && arg_tys[1] == IrType::I64
            && arg_tys[2] == IrType::I64
            && !ret_ty.is_float()
        {
            let slow = format!(".{}_set_slow_{}", self.prefix, self.dchk_seq);
            let done = format!(".{}_set_done_{}", self.prefix, self.dchk_seq + 1);
            self.dchk_seq += 2;
            // 实参装进 SysV ABI 寄存器：rdi=a, rsi=i, rdx=v（回退 call 直接复用）。
            self.lg(args[0], &"rdi".to_string());
            self.lg(args[1], &"rsi".to_string());
            self.lg(args[2], &"rdx".to_string());
            // 独占检查：a[-8] == 1（refcount），否则共享 → COW 回退。
            self.line("\tmovq -8(%rdi), %rax");
            self.line("\tcmpq $1, %rax");
            self.line(&format!("\tjne {}", slow));
            // 元素类型检查：a[0] == 0（DIAL_ELEM_INT），否则引用元素 → ARC 回退。
            self.line("\tcmpq $0, (%rdi)");
            self.line(&format!("\tjne {}", slow));
            // 快路径：a_elems(a)[i] = v（跳过 [kind][len][cap] 24 字节头）。
            self.line("\tmovq %rdx, 24(%rdi,%rsi,8)");
            self.line("\tmovq %rdi, %rax"); // 返回 a
            self.line(&format!("\tjmp {}", done));
            // 慢路径：实参仍在 rdi/rsi/rdx，直接调用原函数保留 COW/ARC。
            self.line(&format!("{}:", slow));
            self.do_call(func);
            self.line(&format!("{}:", done));
            self.sg(dst.unwrap(), &"rax".to_string());
            return;
        }
        let locs = abi_assign(arg_tys, self.target);
        let g1 = self.g1().to_string();

        // 1) 先写栈传参（低位 outbound 区），避免后续寄存器装载踩到它。
        for (idx, loc) in locs.iter().enumerate() {
            if let ArgLoc::Stack(k) = loc {
                // MS ABI：shadow space 占据出站区底部 32 字节，栈传参从第 33 字节起。
                let off = self.target.shadow_space() as i64 + (*k as i64) * 8;
                if arg_tys[idx].is_float() {
                    let f = self.f1().to_string();
                    self.lf(args[idx], &f);
                    match self.target {
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovsd {}, {}({})", self.r(&f), off, self.r("rsp"))),
                        EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", f, off)),
                    }
                } else {
                    self.lg(args[idx], &g1);
                    match self.target {
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovq {}, {}({})", self.r(&g1), off, self.r("rsp"))),
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
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => raw.clone(),
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
        // 2b) MS ABI：位置槽 i (0..3) 上的浮点实参除了进 xmmN，还须镜像到
        //     同一个槽的 GP 寄存器（rcx/rdx/r8/r9），这样 callee 用 va_arg 取
        //     浮点时能从 GP 影子寄存器读到值。非变参调用写入也无害。
        if self.target.is_ms_abi() {
            for (idx, loc) in locs.iter().enumerate() {
                if let ArgLoc::Fp(k) = loc {
                    if *k < 4 {
                        let xmm = self.target.fp_arg_regs()[*k];
                        let gp = self.target.int_arg_regs()[*k];
                        self.line(&format!("\tmovq {}, {}", self.r(xmm), self.r(gp)));
                    }
                }
                let _ = idx;
            }
        }
        // 3) SysV variadic 约定：%al = 传给向量寄存器（xmm0-7）的参数个数。
        //    恒置 0 会让变参函数（如 printf 系列）的 va_arg 读浮点时拿到错误值
        //    （ffi_varargs：3.5→0.0、bool→垃圾）。非变参调用 %al 是 caller-saved，
        //    写入正确值也无害。栈传的浮点（ArgLoc::Stack）不计入。
        if self.target == EmitTarget::X86_64SysV {
            let n_fp = locs.iter().filter(|l| matches!(l, ArgLoc::Fp(_))).count();
            if n_fp == 0 {
                self.line(&format!("\txorl {}, {}", self.r("eax"), self.r("eax")));
            } else {
                self.line(&format!("\tmovl ${}, {}", n_fp, self.r("eax")));
            }
        }
        self.do_call(func);

        // 4) 返回值落回 dst 槽。
        if let Some(d) = dst {
            if ret_ty.is_float() {
                let rr = match self.target {
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "xmm0".to_string(),
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
                // MS ABI：shadow space 占据出站区底部 32 字节，栈传参从第 33 字节起。
                let off = self.target.shadow_space() as i64 + (*k as i64) * 8;
                if arg_tys[idx].is_float() {
                    let f = self.f1().to_string();
                    self.lf(args[idx], &f);
                    match self.target {
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovsd {}, {}({})", self.r(&f), off, self.r("rsp"))),
                        EmitTarget::AArch64 => self.line(&format!("\tstr {}, [sp, #{}]", f, off)),
                    }
                } else {
                    self.lg(args[idx], &g1);
                    match self.target {
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line(&format!("\tmovq {}, {}({})", self.r(&g1), off, self.r("rsp"))),
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
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => raw.clone(),
                        EmitTarget::AArch64 => raw.replacen('v', "d", 1),
                    };
                    self.lf(args[idx], &reg);
                }
                ArgLoc::Stack(_) => {}
            }
        }
        // 2b) MS ABI 变参浮点镜像到同槽 GP 寄存器（见 inst_call 注释）。
        if self.target.is_ms_abi() {
            for loc in locs.iter() {
                if let ArgLoc::Fp(k) = loc {
                    if *k < 4 {
                        let xmm = self.target.fp_arg_regs()[*k];
                        let gp = self.target.int_arg_regs()[*k];
                        self.line(&format!("\tmovq {}, {}", self.r(xmm), self.r(gp)));
                    }
                }
            }
        }
        // 3) 调用目标放进一个**不与实参寄存器冲突**的寄存器。
        //    x86-64 用 %r11（caller-saved，且不在 int_arg_regs 里）；
        //    AArch64 用 x16（IP0， caller-saved）。
        // 裸寄存器名（lg / sg 内部会自己加 '%' 前缀）。
        let call_reg = match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "r11",
            EmitTarget::AArch64 => "x16",
        };
        self.lg(Value::Reg(target), call_reg);
        match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                // SysV variadic 约定：%al = 传给向量寄存器（xmm0-7）的参数个数。
                let n_fp = locs.iter().filter(|l| matches!(l, ArgLoc::Fp(_))).count();
                if n_fp == 0 {
                    self.line(&format!("\txorl {}, {}", self.r("eax"), self.r("eax")));
                } else {
                    self.line(&format!("\tmovl ${}, {}", n_fp, self.r("eax")));
                }
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
                    EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "xmm0".to_string(),
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
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                if self.local_funcs.contains(func) {
                    self.line(&format!("\tcall {}", func));
                } else {
                    self.line(&format!("\tcall {}{}", func, plt_suffix()));
                }
            }
            EmitTarget::AArch64 => self.line(&format!("\tbl {}", func)),
        }
    }

    // ---- 终结符 ----

    fn term(&mut self, block_id: u32, t: &Term) {
        let jmp_mnem = match self.target {
            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "jmp",
            EmitTarget::AArch64 => "b",
        };
        match t {
            Term::Jump { target, args } => {
                self.write_block_args(*target, args);
                self.line(&format!("\t{} .{}_b{}", jmp_mnem, self.prefix, target));
            }
            Term::Branch { cond, then_b, else_b, then_args, else_args } => {
                let else_lbl = format!(".{}_b{}_else", self.prefix, block_id);
                // M7-e3b：ICmp 结果仅被此处使用的，把比较直接并入分支——
                // `cmpq b,a; jcc(不成立) else`，省去 setcc/movzbq/testq 物化。
                // 仅 x86-64：AArch64 的 bin_src 行为未在此路径验证，保守走 cbz。
                if matches!(self.target, EmitTarget::X86_64SysV) && self.fusable.contains_key(cond) {
                    if let Some((op, x, k)) = self.pow2_mod_test.get(cond).copied() {
                        // M7-e3k：`(a % 2^k) == 0 / != 0` → 单条 testq + jcc，
                        // Mod 整条已被 skip_pow2_mod 跳过。testq 置 ZF=1 ⟺
                        // (x & (2^k-1))==0 ⟺ x % 2^k == 0（负数亦成立），故
                        // Eq 用 jne（ZF=0 跳 else）、Neq 用 je，与下方 jcc 表同向。
                        let g1 = self.g1().to_string();
                        self.lg(x, &g1);
                        let mask = (1i64 << k) - 1;
                        self.line(&format!("\ttestq ${}, {}", mask, self.r(&g1)));
                        let jcc = match op {
                            ACmp::Eq => "jne", ACmp::Neq => "je", _ => unreachable!(),
                        };
                        self.line(&format!("\t{} {}", jcc, else_lbl));
                    } else {
                        let (op, a, b) = self.fusable.get(cond).unwrap().clone();
                        let g1 = self.g1().to_string();
                        let g2 = self.g2().to_string();
                        self.lg(a, &g1);
                        let b_src = self.bin_src(b, &g2, true, false);
                        self.line(&format!("\tcmpq {}, {}", b_src, self.r(&g1)));
                        let jcc = match op {
                            ACmp::Eq => "jne", ACmp::Neq => "je", ACmp::Lt => "jge",
                            ACmp::Gt => "jle", ACmp::Le => "jg", ACmp::Ge => "jl",
                        };
                        self.line(&format!("\t{} {}", jcc, else_lbl));
                    }
                } else {
                    let g1 = self.g1().to_string();
                    self.lg(Value::Reg(*cond), &g1);
                    match self.target {
                        EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => {
                            self.line(&format!("\ttestq {}, {}", self.r(&g1), self.r(&g1)));
                            self.line(&format!("\tjz {}", else_lbl));
                        }
                        EmitTarget::AArch64 => self.line(&format!("\tcbz {}, {}", g1, else_lbl)),
                    }
                }
                self.write_block_args(*then_b, then_args);
                self.line(&format!("\t{} .{}_b{}", jmp_mnem, self.prefix, then_b));
                self.line(&format!("{}:", else_lbl));
                self.write_block_args(*else_b, else_args);
                self.line(&format!("\t{} .{}_b{}", jmp_mnem, self.prefix, else_b));
            }
            Term::Return { val } => {
                // R4 插桩：main 返回前写 profile。dump 内部 push/pop 全部易失
                // 寄存器，现场完全复原，故放在 lg(v) 之前也不破坏返回值。
                self.pgo_emit_dump();
                if let Some(v) = val {
                    if self.f.ret.is_float() {
                        let rr = match self.target {
                            EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => "xmm0".to_string(),
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
                EmitTarget::X86_64SysV | EmitTarget::X86_64Ms => self.line("\tud2"),
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

/// 解析 x86-64 的 `\tmovq %reg, off(%rsp)`（整 8 字节栈写），返回 (reg, off)。
fn parse_rsp_store(line: &str) -> Option<(&str, &str)> {
    let s = line.strip_prefix("\tmovq ")?;
    let (l, r) = s.split_once(", ")?;
    if !l.starts_with('%') { return None; }
    let off = r.strip_suffix("(%rsp)")?;
    if off.is_empty() || !off.bytes().all(|b| b.is_ascii_digit() || b == b'-') { return None; }
    Some((l, off))
}

/// 解析 x86-64 的 `\tmovq off(%rsp), %reg`（整 8 字节栈读），返回 (off, reg)。
fn parse_rsp_load(line: &str) -> Option<(&str, &str)> {
    let s = line.strip_prefix("\tmovq ")?;
    let (l, r) = s.split_once(", ")?;
    if !r.starts_with('%') { return None; }
    let off = l.strip_suffix("(%rsp)")?;
    if off.is_empty() || !off.bytes().all(|b| b.is_ascii_digit() || b == b'-') { return None; }
    Some((off, r))
}

/// M7-c2：消除全 spill 里「刚把物理寄存器整 8 字节写进栈槽、紧接下一行又
/// 从同一槽读回同一寄存器」的冗余 load。
///
/// 正确性：`movq %reg, off(%rsp)` 不修改源寄存器，删掉紧邻的
/// `movq off(%rsp), %reg` 后，`reg` 里仍是刚写入的值、内存槽里也是同一值，
/// 语义完全等价；且 `movq` 不影响标志位。匹配被严格限定为：
///   · 两行文本严格相邻（label / term / 其它指令都会打断，绝不跨基本块）；
///   · 必须是同一个 `%reg`、同一个 `off`；
///   · 只认 `movq`——窄整型（movl/movzbq/movb）、浮点（movsd/movss）、
///     立即数（`$imm`）、AArch64（str/ldr）一律不动。
/// 仅用于 x86-64；AArch64 原样返回。
fn elide_redundant_reloads(asm: &str) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    let mut out = String::with_capacity(asm.len());
    let mut i = 0;
    while i < lines.len() {
        let mut skip_next = false;
        if i + 1 < lines.len() {
            if let (Some((sreg, soff)), Some((loff, lreg))) =
                (parse_rsp_store(lines[i]), parse_rsp_load(lines[i + 1]))
            {
                if sreg == lreg && soff == loff { skip_next = true; }
            }
        }
        out.push_str(lines[i]);
        out.push('\n');
        i += if skip_next { 2 } else { 1 };  // 保留 store、丢弃紧邻的冗余 load
    }
    out
}

/// M7-e3d（借循环对齐复活 M7-e3c 的转发）：紧邻异寄存器 store→load 转发。
///
/// M7-e3c 曾实现并被否决：单独做时，删 store 造成的字节平移让 bench.s 单一
/// main 函数里后续超热循环（b23 popcount）入口跨 16/32B 取指边界，稳定退化
/// −27%，−3.7% 静态收益被对齐噪声淹没。M7-e3d 先在最终布局给每个回边循环头
/// 加 `.p2align 4`（见 align_loop_headers），循环头地址低 4 位恒为 0、对删指令
/// 免疫，转发才得以安全落地：`.p2align` 用 NOP padding 吸收被删 store 的字节，
/// 循环体取指边界不变。
///
/// 做法：`movq %rA, off(%rsp)` 紧接 `movq off(%rsp), %rB`（A≠B）时，把 load
/// 改写为 `movq %rA, %rB`：store 不改动 %rA、两行之间无任何指令破坏它，故
/// %rA 里仍是刚写入槽的值，直搬到 %rB 与「写内存→读回内存」语义等价。store
/// 行保留，由随后的 elide_dead_stores 按「只写不读」判定删除（若该槽另有跨块
/// 读者，② 保守保留所有 store，转发仍正确）。
///
/// 严格限定（与 ② 共用 `[lo,hi)` spill 槽区边界）：
///   · 两行文本严格相邻（label / 其它指令打断即不跨基本块）；
///   · 同一 off 且 off ∈ `[lo, hi)`，排除 outbound 传参区 / callee-saved 保存区 /
///     可能被 leaq 间接读写的 Alloca 区；
///   · 只认整 8 字节 movq、源与目标均为寄存器（parse_rsp_* 保证），排除 %rsp；
///   · A≠B（A==B 由 elide_redundant_reloads 删除 load，本 pass 不重复处理）。
/// 仅用于 x86-64，必须排在 ① 之后、② 之前、⑤ 之前（对齐在最后统一兜底）。
fn forward_store_load(asm: &str, lo: i64, hi: i64) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    let mut out = String::with_capacity(asm.len());
    let mut i = 0;
    while i < lines.len() {
        if i + 1 < lines.len() {
            if let (Some((sreg, soff)), Some((loff, lreg))) =
                (parse_rsp_store(lines[i]), parse_rsp_load(lines[i + 1]))
            {
                let in_spill = soff.parse::<i64>().ok()
                    .map_or(false, |o| o >= lo && o < hi);
                let safe = sreg != "%rsp" && lreg != "%rsp";
                if sreg != lreg && soff == loff && in_spill && safe {
                    out.push_str(lines[i]);                       // 保留 store（② 判定删除）
                    out.push('\n');
                    out.push_str(&format!("\tmovq {}, {}\n", sreg, lreg));  // 改写 load
                    i += 2;
                    continue;
                }
            }
        }
        out.push_str(lines[i]);
        out.push('\n');
        i += 1;
    }
    out
}

/// M7-e1：全函数「死 store」消除，仅作用于 vreg spill 槽区 `[lo, hi)`。
/// 该区间由 compute_layout 的 reg_off 精确圈定，天然排除：outbound 栈传参区
/// （更低偏移、被外部被调方读取）、callee-saved 帧内保存区（epilogue 读回）、
/// 以及 Alloca 区（更高偏移、可能被 leaq 取地址后间接读写）。
///
/// 判定：某槽 off 若被 `movq %reg, off(%rsp)` 写过，却在整个函数里从未以
/// 任何其它形式出现（load / 算术内存源 / leaq / test / cmp …），说明该 vreg
/// 的值从未经栈槽读回——这些 store 全是死代码。只要 off 出现过一次非 store
/// 形式，就整体保留它的所有 store（保守，绝不跨函数/间接访问推断）。
/// 与 elide_redundant_reloads 配合：必须先删掉紧邻的冗余 load，才会暴露这些
/// 已无人读的孤立 store。
fn rsp_offsets(line: &str) -> Vec<i64> {
    let mut res = Vec::new();
    let b = line.as_bytes();
    let pat = b"(%rsp)";
    if b.len() < pat.len() { return res; }
    for i in 0..=b.len() - pat.len() {
        if &b[i..i + pat.len()] == &pat[..] {
            let mut j = i;
            while j > 0 && (b[j - 1].is_ascii_digit() || b[j - 1] == b'-') { j -= 1; }
            if j < i {
                if let Ok(o) = line[j..i].parse::<i64>() { res.push(o); }
            }
        }
    }
    res
}

fn elide_dead_stores(asm: &str, lo: i64, hi: i64) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    let mut writes: HashMap<i64, Vec<usize>> = HashMap::new();
    let mut touched: HashSet<i64> = HashSet::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some((_, off)) = parse_rsp_store(line) {
            if let Ok(o) = off.parse::<i64>() {
                writes.entry(o).or_default().push(i);
                continue; // 纯 movq store：只记它的目标槽
            }
        }
        for o in rsp_offsets(line) { touched.insert(o); }
    }
    let mut remove: HashSet<usize> = HashSet::new();
    for (off, idxs) in &writes {
        if *off >= lo && *off < hi && !touched.contains(off) {
            remove.extend(idxs.iter().copied());
        }
    }
    let mut out = String::with_capacity(asm.len());
    for (i, line) in lines.iter().enumerate() {
        if remove.contains(&i) { continue; }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// 解析形如 `  .Lxxx:` 的局部标签行，返回含点、不含冒号的标签名。
fn parse_label(line: &str) -> Option<&str> {
    let t = line.trim();
    if !t.starts_with('.') { return None; }
    let name = t.strip_suffix(':')?;
    match name.get(1..) {
        Some(rest) if !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => Some(name),
        _ => None,
    }
}

/// 解析直接跳转，返回 (助记符, 目标标签)。
///
/// 本后端会发射三类直接跳转：
///  - 无条件 `jmp .Lxxx`（fallthrough 消除 / 死 trampoline 删除的主体）；
///  - 非融合分支的 `jz .Lxxx`（testq;jz 路径）；
///  - M7-e3b 融合分支的 `jne/je/jge/jle/jg/jl` 等条件跳转（cmpq;jcc 路径）。
///
/// **关键**：条件跳转的目标标签同样是「被引用」的，必须全部计入 `refs`，
/// 否则 `elide_dead_trampolines` 会把 else 标签（`.Lxxx_bN_else:` 后紧跟
/// `jmp .Lxxx_bM`）误判为无引用死 trampoline 而删除，造成悬空符号。因此这里
/// 放行 `jmp` 与所有 `j<cc>` 条件跳转（x86-64 中 `j` 开头且非 `jmp` 的助记符
/// 必为条件跳转），其它助记符、间接跳转、带额外操作数的一律不参与重定向。
fn parse_jmp(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    let mut it = t.split_whitespace();
    let m = it.next()?;
    if m != "jmp" && !m.starts_with('j') { return None; }
    let tgt = it.next()?;
    if it.next().is_some() || !tgt.starts_with('.') { return None; }
    Some((m, tgt))
}

/// 空行 / 注释 / 汇编指示（`.` 开头但非标签）都不算真实指令。
fn is_skip(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
        || (t.starts_with('.') && parse_label(line).is_none())
}

fn is_instr(line: &str) -> bool {
    !is_skip(line) && parse_label(line).is_none()
}

/// 跟随 trampoline 链到最终目标；有环时停在环内（结果仍是真实标签，不会悬空）。
fn resolve_jump(x: &str, tramp: &HashMap<String, String>) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let mut cur = x.to_string();
    while let Some(t) = tramp.get(&cur) {
        if !seen.insert(cur.clone()) { break; }
        cur = t.clone();
    }
    cur
}

/// 紧邻 `lines[i]` 之后的「标签前缀」：跳过空行/指示/连续标签，直到第一条
/// 真实指令。返回的标签都指向同一条指令（彼此是别名）。
fn labels_prefix<'a>(lines: &'a [String], i: usize) -> Vec<&'a str> {
    let mut res = Vec::new();
    let mut j = i + 1;
    while j < lines.len() {
        if let Some(l) = parse_label(&lines[j]) { res.push(l); j += 1; continue; }
        if is_skip(&lines[j]) { j += 1; continue; }
        break;
    }
    res
}

/// M7-e2：跳转目标重定向（jump threading）+ fallthrough `jmp` 消除，仅 x86-64。
///
/// - **trampoline 块** = 标签后第一条真实指令就是无条件 `jmp T`、中间无任何
///   指令（因此绝无 phi 参数写入副作用）的块。跟随纯 jmp 链不产生任何状态
///   变化，把 `jmp/jz L`（L 为 trampoline）改写成链末目标 `resolve(L)`，运行时
///   即可跳过中间跳转——递归热路径常见 2~3 跳、`bench.dial` 里最长链达 26 跳。
/// - `jmp X` 且 X 恰是紧邻的下一个标签时删掉该 jmp，让控制流自然落入；
///   fallthrough 与跳到 X 语义等价（无条件 jmp 无副作用）。
///
/// 不删除任何标签、不改变跳转目标集合，resolve 结果必为真实存在的标签。
/// 逐函数对 `e.out` 运行，局部标签自成体系，绝不跨函数重定向。
fn thread_jumps(asm: &str) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    // 1) 构建 trampoline 表：标签 -> 其块首 jmp 的目标标签。
    let mut tramp: HashMap<String, String> = HashMap::new();
    for i in 0..lines.len() {
        if let Some(l) = parse_label(lines[i]) {
            let mut j = i + 1;
            while j < lines.len() && !is_instr(lines[j]) { j += 1; }
            if j < lines.len() {
                if let Some((m, t)) = parse_jmp(lines[j]) {
                    if m == "jmp" { tramp.insert(l.to_string(), t.to_string()); }
                }
            }
        }
    }
    // 2) 重定向所有 jmp/jz 目标到链末。
    let mut stage: Vec<String> = Vec::with_capacity(lines.len());
    for line in &lines {
        let mut s = (*line).to_string();
        if let Some((_, tgt)) = parse_jmp(line) {
            let r = resolve_jump(tgt, &tramp);
            if r.as_str() != tgt { s = line.replacen(tgt, r.as_str(), 1); }
        }
        stage.push(s);
    }
    // 3) 删 fallthrough jmp，输出。
    let mut out = String::with_capacity(asm.len());
    for i in 0..stage.len() {
        if let Some((m, tgt)) = parse_jmp(&stage[i]) {
            if m == "jmp" && labels_prefix(&stage, i).contains(&tgt) { continue; }
        }
        out.push_str(&stage[i]);
        out.push('\n');
    }
    out
}

/// M7-e3a：删除 M7-e2 重定向后已无引用、且无 fallthrough 进入的死 trampoline
/// 块，并在删块后的紧凑布局上再消一次新暴露的 fallthrough `jmp`。
///
/// 安全性：
///  - **trampoline 块** = 紧邻若干标签 / 汇编指示后紧跟一条 `jmp`、块内无任何
///    其它真实指令。M7-e2 重定向后这些标签已无任何跳转引用；
///  - 仅当「块前最后一条真实指令是无条件控制转移（`jmp`/`ret`/`ud2`）」时才删
///    ——若是普通指令或条件跳转，控制流会 fallthrough 进入该块，删除即改变语义；
///  - 别名标签（多个标签指向同一 jmp）须全部无引用才整体删除；函数最前的块
///    （前面无可判定指令）一律保守保留；
///  - 不删任何仍被引用的标签，删块后全函数无悬空跳转目标。
///
/// 删掉死块后，原本被它隔开的 `jmp X` 与目标 X 变成紧邻，会产生新的
/// fallthrough 机会——这些是**运行时执行路径上的**跳转（不是死代码），再消一次
/// 有真实收益。仅 x86-64，逐函数运行。
fn elide_dead_trampolines(asm: &str) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    let n = lines.len();
    // 所有跳转引用的标签集合。
    let mut refs: HashSet<String> = HashSet::new();
    for l in &lines {
        if let Some((_, t)) = parse_jmp(l) { refs.insert(t.to_string()); }
    }
    // 识别可删 trampoline 块：从每条块首 jmp 往前回退连续的标签 / skip 行，
    // 直到遇到前一块的最后一条真实指令。
    let mut remove: HashSet<usize> = HashSet::new();
    for j in 0..n {
        if !matches!(parse_jmp(lines[j]), Some(("jmp", _))) { continue; }
        let mut k = j as isize - 1;
        let mut group: Vec<usize> = Vec::new();
        while k >= 0 {
            if parse_label(lines[k as usize]).is_some() {
                group.push(k as usize); k -= 1; continue;
            }
            if is_skip(lines[k as usize]) { k -= 1; continue; }
            break;
        }
        if group.is_empty() { continue; }                 // jmp 前无标签：不是块首
        // 任一别名标签仍被跳转引用 → 整个块保守保留。
        if group.iter().any(|&g| refs.contains(parse_label(lines[g]).unwrap())) {
            continue;
        }
        if k < 0 { continue; }                            // 函数最前，保守保留
        let t = lines[k as usize].trim();
        let no_fallthrough = t.starts_with("jmp ")
            || t.starts_with("ret") || t.starts_with("ud2");
        if !no_fallthrough { continue; }                 // 有 fallthrough 进入，保留
        for g in group { remove.insert(g); }
        remove.insert(j);
    }
    // 生成删块后的行序列。
    let stage: Vec<String> = lines.iter().enumerate()
        .filter(|(i, _)| !remove.contains(i))
        .map(|(_, l)| (*l).to_string())
        .collect();
    // 删块后再做一次 fallthrough 消除：死块消失后原本被隔开的 `jmp X` 与 X
    // 变成紧邻，这些跳转运行时会执行，消除有真实收益。
    let mut out = String::with_capacity(asm.len());
    for i in 0..stage.len() {
        if let Some((m, tgt)) = parse_jmp(&stage[i]) {
            if m == "jmp" && labels_prefix(&stage, i).contains(&tgt) { continue; }
        }
        out.push_str(&stage[i]);
        out.push('\n');
    }
    out
}

/// M7-e3d：回边循环头 16 字节对齐（x86-64 纯文本 pass，挂在四个既有 pass 之后）。
///
/// 背景（M7-e3c 探索结论）：bench.s 把多个微基准内联进单一 main 函数，任何改变
/// 指令字节数的文本 pass 都会平移后续超热循环的入口地址；objdump 实测 popcount
/// 内循环地址会在 0x3524/0x3230/0x341b 间漂移（低 6 位 36/48/27），跨 16/32B
/// 取指边界造成 ±5% 甚至 -27% 的性能噪声，淹没薄收益优化的测量。
///
/// 做法：在最终指令流（前四个 pass 已把跳转 / 删块定型）上识别「回边」——某跳转
/// 指令的目标 label 定义位置严格早于该跳转指令位置，该目标即循环头。在每个
/// 循环头 label 组的首行前插入 `.p2align 4`：此后无论循环头之前的指令字节数
/// 如何变化，汇编器都用 NOP padding 吸收，循环头地址低 4 位恒为 0，循环体
/// 取指边界稳定；同时也修正了原来「碰巧跨边界」的慢循环。
///
/// 严格限定（与既有文本 pass 同口径，绝不改程序语义）：
///   · 仅 x86-64 生效（AArch64 走原路径，不插任何 directive）；
///   · 仅认 parse_jmp 识别的直接跳转（jmp / j<cc>），间接跳转不参与回边判定；
///   · 仅「向后跳」（目标定义行 < 跳转行）的目标算循环头；前向跳转目标
///     （else / case 等）不插，避免给直线代码加无谓 padding；
///   · .p2align 只插在「连续标签组」的首行之前，绝不插进别名标签之间
///     （否则 NOP 会让原本同址的别名 label 分离）；
///   · 保留所有原有行，只新增 directive 行；填充的 NOP 位于循环头 label
///     之前、循环之外，每次进入循环前执行一次，不在热循环体内；
///   · 逐函数对本函数局部标签自成体系，回边判定基于本函数内的文本行序。
fn align_loop_headers(asm: &str) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    // ① 局部标签首次定义行号。
    let mut def_line: HashMap<&str, usize> = HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(name) = parse_label(l) { def_line.entry(name).or_insert(i); }
    }
    // ② 回边目标 → 其「连续标签组」首行。
    let mut heads: HashSet<usize> = HashSet::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some((_, tgt)) = parse_jmp(l) {
            if let Some(&d) = def_line.get(tgt) {
                if d < i {
                    let mut h = d;
                    while h > 0 && parse_label(lines[h - 1]).is_some() { h -= 1; }
                    heads.insert(h);
                }
            }
        }
    }
    if heads.is_empty() { return asm.to_string(); }
    // ③ 输出：循环头组首行前插 .p2align 4（16B），其余行原样保留。
    let mut out = String::with_capacity(asm.len() + heads.len() * 16);
    for (i, l) in lines.iter().enumerate() {
        if heads.contains(&i) { out.push_str("\t.p2align 4\n"); }
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// M7-e3q：x86-64 尾调用消除（sibling call / tail-call elimination）。
///
/// 自研后端当前为「函数级 Return」发射的固定序列是：
/// ```text
///   call <callee>        ; 实参已在 rdi/rsi/...（call 前由 inst_call 装好）
///   addq $N, %rsp        ; epilogue 销毁本函数栈帧（N = prologue 的 subq 值）
///   ret                  ; 返回到本函数调用者
/// ```
/// 若该 `call` 正处于尾位置（返回值不经任何加工直接 Return），则它语义上
/// 等价于 sibling call：栈帧在跳转前销毁后，rsp 恰好回到「本函数入口刚被
/// call 压入返回地址」的状态，此时 `jmp <callee>` 令被调方直接在同一个栈帧
/// 上执行、并在自己的 ret 处**直接返回到本函数的调用者**——省去一层
/// call/ret 与递归深度的栈帧累积。LLVM `-O` 的 tailcallelim / sibling-call
/// 即做此变换，这是递归类（b12 互递归）长期落后 LLVM 的标准优化缺口。
///
/// 变换：`call G / addq $N,%rsp / ret` → `addq $N,%rsp / jmp G`（G 后接括号
/// 由汇编器解析）。净省一条 call 与一条 ret，且递归不再逐层压栈。
///
/// **严格护栏（任一不满足即整处不动，宁可不优化，绝不发错代码）**：
///   1. 仅 x86-64 调用本 pass（AArch64 走原路径，`.s` 逐字节不变）；
///   2. 函数 prologue 形如 `subq $N,%rsp`，且不存在 callee-saved 寄存器的
///      栈保存/恢复（`movq %rbx/%rbp/%r12-%r15, off(%rsp)`）——否则 epilogue
///      先恢复它们再 ret，直接 jmp 会跳过恢复、破坏调用者的 callee-saved；
///   3. `call G` 为内部 Dial 函数（无 `@PLT`、无 `.L` 前缀），实参全在寄存器；
///   4. `addq $N,%rsp` 的 N 与 prologue 的 subq 值**逐字相等**；
///   5. `ret` 紧随 addq 之后，且是该 Return 块的结尾（下一行为标签/块末）；
///   6. G 与本函数在**同一递归 SCC**（G 可达本函数入口）——只消除递归尾调用，
///      绝不碰 `run → bXX` 这类一次性 dispatch（无收益且徒增布局扰动）；
///   7. 本函数与 G 的参数保存数一致且都 ≤6（保证调用约定兼容、实参寄存器布局相同）。
///
/// 这是「语义保持」变换：尾调用的返回值本就原样透传，jmp 后由 G 的 ret 直接
/// 返回同一值；`xorl %eax,%eax`（变参 ABI 清零 %al）对 Dial 内部调用无意义，
/// 保留不动不影响正确性。
fn elide_tail_calls(asm: &str) -> String {
    let lines: Vec<&str> = asm.lines().collect();
    let nlines = lines.len();

    // ---- ① 切分函数：顶格 `name:` 是函数入口（局部 label 以 '.' 开头，排除）。
    let mut fn_starts: Vec<(String, usize)> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(name) = l.strip_suffix(':') {
            if !name.starts_with('.')
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                fn_starts.push((name.to_string(), i));
            }
        }
    }
    if fn_starts.len() < 2 { return asm.to_string(); }
    let mut bounds: Vec<(String, usize, usize)> = Vec::new();
    for k in 0..fn_starts.len() {
        let end = if k + 1 < fn_starts.len() { fn_starts[k + 1].1 } else { nlines };
        bounds.push((fn_starts[k].0.clone(), fn_starts[k].1 + 1, end));
    }

    // ---- ② 每函数：栈帧大小 N、是否含 callee-saved 栈保存、内部 callee 集合。
    #[derive(Default)]
    struct FInfo {
        frame: Option<i64>,
        has_callee_saved: bool,
        calls: Vec<String>,
    }
    let mut info: HashMap<String, FInfo> = HashMap::new();
    for (name, s, e) in &bounds {
        let mut fi = FInfo::default();
        for l in &lines[*s..*e] {
            let t = l.trim();
            if fi.frame.is_none() {
                if let Some(rest) = t.strip_prefix("subq $") {
                    if let Some(v) = rest.split(", %rsp").next().and_then(|x| x.parse::<i64>().ok()) {
                        fi.frame = Some(v);
                    }
                }
            }
            // callee-saved 寄存器写栈：prologue 保存（epilogue 会读回）。
            for csr in ["%rbx", "%rbp", "%r12", "%r13", "%r14", "%r15"] {
                if t.contains(&format!("movq {},", csr)) && t.contains("(%rsp)") {
                    fi.has_callee_saved = true;
                }
            }
            // 内部 Dial 调用：`call name`（无 @PLT / 无 .L / 合法标识符）。
            if let Some(rest) = t.strip_prefix("call ") {
                let callee = rest.trim();
                if !callee.contains('@') && !callee.starts_with('.')
                    && !callee.is_empty()
                    && callee.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    fi.calls.push(callee.to_string());
                }
            }
        }
        info.insert(name.clone(), fi);
    }

    // ---- ③ 可达性：callee 能否回到 caller（同递归 SCC）。深度有界。
    fn reaches(info: &HashMap<String, FInfo>, from: &str, target: &str) -> bool {
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = vec![from.to_string()];
        let mut steps = 0usize;
        while let Some(cur) = stack.pop() {
            if steps > 10_000 { return false; }
            steps += 1;
            if !seen.insert(cur.clone()) { continue; }
            if let Some(fi) = info.get(&cur) {
                for c in &fi.calls {
                    if c == target { return true; }
                    if !seen.contains(c) { stack.push(c.clone()); }
                }
            }
        }
        false
    }

    // ---- ④ 逐函数扫描尾调用三连并登记替换（按行号，避免索引失效）。
    let mut drop: HashSet<usize> = HashSet::new();   // 要删除的 `call G` 行
    let mut jmp_at: HashMap<usize, String> = HashMap::new(); // ret 行号 → 替换为 `jmp G`
    // M7-互递归帧复用：caller 与 callee 同帧大小、无 callee-saved 时，尾跳可复用
    // 同一栈帧——删掉 addq（不销毁帧）、跳到 callee 的 prologue 后别名标签（跳过
    // subq），令 N 层递归只建/拆一次帧，而非每层 subq/addq。evR/odR 互递归 4.26x
    // 的根因即每层 120B 栈帧反复加减；帧复用后递归链 rsp 不变。
    let mut drop_add: HashSet<usize> = HashSet::new();        // 要删除的 `addq $N,%rsp` 行
    let mut fr_jmp_at: HashMap<usize, String> = HashMap::new(); // ret 行 → `jmp G_dial_fr`
    let mut fr_labels: HashSet<String> = HashSet::new();      // 需插入 `_dial_fr` 标签的函数
    for (name, s, e) in &bounds {
        let fi = match info.get(name) { Some(x) => x, None => continue };
        if fi.has_callee_saved || fi.frame.is_none() { continue; }
        let frame = fi.frame.unwrap();
        // 参数保存数（prologue 里 movq %rdi/%rsi/%rdx/%rcx/%r8/%r9 → 栈）。
        let mut nparams = 0usize;
        for l in &lines[*s..*e] {
            let t = l.trim();
            if ["%rdi", "%rsi", "%rdx", "%rcx", "%r8", "%r9"].iter()
                .any(|r| t.starts_with(&format!("movq {},", r)) && t.contains("(%rsp)"))
            {
                nparams += 1;
            }
        }
        if nparams > 6 { continue; }

        let mut i = *s;
        while i + 2 < *e {
            let tcall = lines[i].trim();
            let callee = match tcall.strip_prefix("call ") {
                Some(c) if !c.contains('@') && !c.starts_with('.')
                    && c.chars().all(|x| x.is_ascii_alphanumeric() || x == '_') => c,
                _ => { i += 1; continue; }
            };
            // addq $N, %rsp 必须紧随，N 与 frame 相等。
            let add_ok = lines[i + 1].trim()
                .strip_prefix("addq $")
                .and_then(|r| r.split(", %rsp").next())
                .and_then(|x| x.parse::<i64>().ok())
                .map_or(false, |v| v == frame);
            let ret_ok = lines[i + 2].trim() == "ret";
            if !add_ok || !ret_ok { i += 1; continue; }
            // callee 必须已知、与本函数同递归 SCC、参数数一致且 ≤6。
            let ginfo = match info.get(callee) { Some(x) => x, None => { i += 1; continue; } };
            let gparams = {
                let mut c = 0usize;
                if let Some((gs, ge)) = bounds.iter().find(|(n, _, _)| n == callee).map(|(_, a, b)| (*a, *b)) {
                    for l in &lines[gs..ge] {
                        let tt = l.trim();
                        if ["%rdi", "%rsi", "%rdx", "%rcx", "%r8", "%r9"].iter()
                            .any(|r| tt.starts_with(&format!("movq {},", r)) && tt.contains("(%rsp)"))
                        { c += 1; }
                    }
                }
                c
            };
            let same_sig = gparams == nparams && nparams <= 6;
            let in_scc = reaches(&info, callee, name);
            if !ginfo.has_callee_saved && same_sig && in_scc {
                // 帧复用判定：callee 帧大小必须 == caller 帧大小。此时尾跳可保持 rsp
                // 不变、跳到 callee prologue 后别名标签，N 层递归只建/拆一次帧。
                if ginfo.frame == Some(frame) && frame > 0 {
                    drop.insert(i);                       // 删 `call G`
                    drop_add.insert(i + 1);               // 删 `addq $N,%rsp`（帧不拆）
                    fr_jmp_at.insert(i + 2, format!("\tjmp {}_dial_fr", callee));
                    fr_labels.insert(callee.to_string()); // callee 插入别名标签
                } else {
                    // 帧大小不同：退回普通尾调用消除（拆帧后 jmp，rsp 归位）。
                    drop.insert(i);
                    jmp_at.insert(i + 2, format!("\tjmp {}", callee));
                }
                i += 3; // 跳过整个三连，避免相邻窗口重复处理
            } else {
                i += 1;
            }
        }
    }
    if drop.is_empty() { return asm.to_string(); }

    // ---- ⑤ 应用替换。帧复用分支还需在 callee prologue 内插入 `_dial_fr` 别名标签。
    // 标签必须插在 `subq $N,%rsp` **之后、参数保存 `movq %rdi,off(%rsp)` 之前**：
    // 帧复用跳来时 caller 的帧仍在、rsp 不变，绝不能再 subq；但必须重新执行参数保存
    // （尾跳前 caller 已把新实参放进 rdi/rsi/...），把新参写进复用的同一帧参数槽。
    // 若误插在 movq 之后， callee 读到的是 caller 未更新的旧参数 → 递归不终止（实测踩过）。
    let mut fr_insert_before: HashMap<usize, String> = HashMap::new();
    if !fr_labels.is_empty() {
        for (name, s, e) in &bounds {
            if !fr_labels.contains(name) { continue; }
            // 在 [s,e) 内找第一条 `movq %<argreg>,...(%rsp)`（prologue 参数保存），插其前。
            for i in *s..*e {
                let t = lines[i].trim();
                if ["%rdi", "%rsi", "%rdx", "%rcx", "%r8", "%r9"].iter()
                    .any(|r| t.starts_with(&format!("movq {},", r)) && t.contains("(%rsp)"))
                {
                    fr_insert_before.insert(i, format!("{}_dial_fr:", name));
                    break;
                }
            }
        }
    }
    let mut out = String::with_capacity(asm.len());
    for (i, l) in lines.iter().enumerate() {
        if drop.contains(&i) || drop_add.contains(&i) { continue; }
        // 标签插在参数保存行之前（跳来时执行保存，但不执行它前面的 subq）。
        if let Some(lbl) = fr_insert_before.get(&i) {
            out.push_str(lbl);
            out.push('\n');
        }
        if let Some(rep) = jmp_at.get(&i).or_else(|| fr_jmp_at.get(&i)) {
            out.push_str(rep);
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    out
}

/// M7-c1 / P4.6：移位指令立即数折叠的量判定（x86-64 与 AArch64 双目标共用）。
/// 两个 ISA 的 64 位移位都按 `count & 63` 生效，立即数形式（x86 `shlq/sarq $n`、
/// AArch64 `lsl/asr #n`）都只接受 `[0,63]`；当移位量是该范围的整数常量时，
/// 折叠为立即数形式与「先装寄存器再变址移位」语义完全一致。其余（寄存器 /
/// 越界 / 浮点位模式）一律返回 None，退回原变址移位路径（保留 &63 语义）。
fn shift_imm(v: Value) -> Option<i64> {
    let n = match v {
        Value::Imm(Imm::I64(n)) => n,
        Value::Imm(Imm::I32(n)) => n as i64,
        Value::Imm(Imm::I8(n)) => n as i64,
        Value::Imm(Imm::I1(b)) => b as i64,
        _ => return None,
    };
    if (0..=63).contains(&n) { Some(n) } else { None }
}

/// 编译期为「有符号 64 位整数 / 正常量 d」求 Granlund–Montgomery 乘法逆元。
///
/// 返回 `(M, s, add, d)`，对应即将发射的序列（`n` 在 r11）：
/// ```text
///   movabsq $M, %rax ; imulq %r11   ; rdx = 高 64 位 (M×n)
///   [add] addq %r11, %rdx
///   [s>0] sarq $s, %rdx
///   movq %r11, %r10 ; sarq $63, %r10 ; subq %r10, %rdx  ; rdx = 商
/// ```
/// `add` 为真表示 M 的位模式 ≥ 2^63（打印为负立即数），此时需 `rdx += n`。
/// 该序列与 gcc -O2 的常量除法输出同构，复现 `idivq` 的向零截断语义。
///
/// **自校验**：只有 `2 <= d <= i32::MAX`（Mod 里 `imulq` 的立即数必须是
/// imm32）且「搜索出的 (M,s,add) 通过全边界逐值模拟」才返回 Some；任何一步
/// 不确定都返回 None，由调用方原样退回 `idivq`。宁可不优化，绝不发错代码。
fn const_div_magic(b: Value) -> Option<(i64, u32, bool, i64)> {
    let d = match b {
        Value::Imm(Imm::I64(n)) => n,
        Value::Imm(Imm::I32(n)) => n as i64,
        Value::Imm(Imm::I8(n)) => n as i64,
        Value::Imm(Imm::I1(v)) => v as i64,
        _ => return None, // 浮点位模式 / 寄存器除数：交给 idivq
    };
    if d < 2 || d > i32::MAX as i64 { return None; }

    // 用 i128 精确模拟 128 位有符号乘法（imulq 单操作数），返回商。
    fn sim(n: i64, m: i64, s: u32, add: bool) -> i64 {
        let full = (n as i128) * (m as i128);
        let mut hi = (full >> 64) as i64;
        if add { hi = hi.wrapping_add(n); }
        let q = hi >> s;
        q.wrapping_sub(n >> 63) // n>>63 = -1 (n<0) 或 0
    }
    // 端点、k·d 临界区、2 的幂、伪随机大数——魔数误差是系统性的，会在
    // k·d 边界立刻暴露；`all` 短路让错误候选几乎立刻失败。
    fn verify(d: i64, m: i64, s: u32, add: bool) -> bool {
        let mut pts: Vec<i64> = vec![
            i64::MIN, i64::MIN + 1, i64::MIN + 2,
            i64::MAX, i64::MAX - 1, i64::MAX - 2,
            0, 1, -1, 2, -2, 3, -3,
            d, -d, d - 1, -d + 1, d + 1, -d - 1,
        ];
        let qm = i64::MAX / d;
        for k in [1i64, 2, 3, 7, 100, 1000, qm, qm - 1] {
            for dl in -2i64..=2 {
                for base in [k.wrapping_mul(d), (-k).wrapping_mul(d)] {
                    pts.push(base.wrapping_add(dl));
                }
            }
        }
        for j in [1u32, 2, 3, 15, 16, 31, 32, 62] {
            let p = 1i64 << j;
            pts.extend([p, -p, p - 1, p + 1]);
        }
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..600 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            let v = (x >> 1) as i64;
            pts.push(v); pts.push(-v);
        }
        pts.iter().all(|&n| sim(n, m, s, add) == n / d)
    }

    for s in 0..63u32 {
        let num = 1u128 << (64 + s);
        let fl = num / (d as u128);
        for m0 in [fl, fl + 1] {
            if m0 >= (1u128 << 64) { continue; }
            let m = m0 as i64;          // ≥2^63 时位模式解释为负立即数
            let add = m < 0;
            if verify(d, m, s, add) { return Some((m, s, add, d)); }
        }
    }
    None
}

/// M7-e3l：从整数乘法 `a * b` 里分离出「LEA 可表达的常量因子 c」与变量因子。
/// x86-64 的 `leaq disp(base, index, scale), dst` 中 scale 只能是 1/2/4/8；
/// 当 base == index 时它计算 `base * (scale + 1) + disp`，故恰好能表达
/// c ∈ {2,3,5,9}。浮点 Imm 是位模式（`as_i64` 会 panic），显式排除；两侧
/// 都是立即数的常量折叠不归本 pass 管，也显式排除。返回 (base, scale=c-1)。
fn lea_mul_scale(a: Value, b: Value) -> Option<(Value, u32)> {
    fn int_imm(v: Value) -> Option<i64> {
        match v {
            Value::Imm(i) if !matches!(i, Imm::F64(_) | Imm::F32(_)) => Some(i.as_i64()),
            _ => None,
        }
    }
    let (base, c) = match (int_imm(a), int_imm(b)) {
        (Some(c), None) => (b, c),
        (None, Some(c)) => (a, c),
        _ => return None,
    };
    let scale = match c { 2 => 1, 3 => 2, 5 => 4, 9 => 8, _ => return None };
    Some((base, scale))
}

/// M7-e3l：取整数立即数作为 LEA 的 disp32。越出 i32 范围必须退回通用路径：
/// x86-64 的 disp32 会被符号扩展，无法承载 64 位大常量。
fn lea_disp(v: Value) -> Option<i64> {
    match v {
        Value::Imm(i) if !matches!(i, Imm::F64(_) | Imm::F32(_)) => {
            let n = i.as_i64();
            if n >= i32::MIN as i64 && n <= i32::MAX as i64 { Some(n) } else { None }
        }
        _ => None,
    }
}

/// M7-e3l：`m - n` 等价于 `m + (-n)`，把 -n 直接编码成 LEA 的 disp32。
/// n 必须落在 i32 范围内（超出时通用路径是 movabsq + subq，不能这样折叠），
/// 且 n != i32::MIN（-n 会溢出 i32，无法编码为 disp32）。由此负 disp 也得到
/// 覆盖，能实证 LEA disp32 的符号扩展与 `subq $imm32` 完全一致。
fn lea_sub_disp(v: Value) -> Option<i64> {
    match v {
        Value::Imm(i) if !matches!(i, Imm::F64(_) | Imm::F32(_)) => {
            let n = i.as_i64();
            if n >= i32::MIN as i64 && n <= i32::MAX as i64 && n != i32::MIN as i64 {
                Some(-n)
            } else { None }
        }
        _ => None,
    }
}

/// M7-e3k：判定 Value 是否为整数立即数 0（用于 `(a % 2^k) == 0` 融合）。
/// 只认整数 Imm 变体；F64/F32 是浮点位模式，不参与整数比较，显式排除。
fn imm_is_zero(v: Value) -> bool {
    matches!(v, Value::Imm(Imm::I64(0)) | Value::Imm(Imm::I32(0))
        | Value::Imm(Imm::I8(0)) | Value::Imm(Imm::I1(false)))
}

/// M7-e3m：判定 Value 是否为整数立即数 1（用于 `x & 1` / `x >> 1` 识别）。
/// 与 imm_is_zero 同口径：只认整数 Imm 变体，浮点位模式显式排除。
fn imm_is_one(v: Value) -> bool {
    matches!(v, Value::Imm(Imm::I64(1)) | Value::Imm(Imm::I32(1))
        | Value::Imm(Imm::I8(1)) | Value::Imm(Imm::I1(true)))
}

/// M7-e3n：返回 XorshiftPlan 里的 x 槽 vreg。
fn plan_slot(p: &XorshiftPlan) -> VReg {
    match p { XorshiftPlan::ShiftMaskXor { slot, .. } | XorshiftPlan::ShiftXor { slot, .. } => *slot }
}

/// M7-e3n：判定 Value 是否为 64 位立即数 0xFFFFFFFF（xorshift32 掩码）。
fn imm_is_u32_max(v: Value) -> bool {
    matches!(v, Value::Imm(Imm::I64(n)) if n == 0xFFFF_FFFFi64)
}

/// M7-e3n：识别形态 A `x = x ^ ((x << k) & 0xFFFFFFFF)` 的连续 6 条 IR：
///   Load A,ptr; Load B,ptr; Shl C=B<<k; BitAnd D=C&0xFFFFFFFF;
///   BitXor E=A^D; Store ptr=E
/// 两个 Load 必须读同一槽 ptr，且槽在窗口内不被改写；移位量 k∈[1,63]。
/// 返回 (plan, 5 个中间 vreg, 窗口条数=6)。BitXor 两侧允许交换（异或可交换）。
fn match_xorshift_mask(w: &[IrInst]) -> Option<(XorshiftPlan, Vec<VReg>, usize)> {
    if w.len() < 6 { return None; }
    let is_i64 = |t: &IrType| *t == IrType::I64;
    let (slot, la) = match &w[0] {
        IrInst::Load { dst, ptr, ty } if is_i64(ty) => (*ptr, *dst), _ => return None,
    };
    let lb = match &w[1] {
        IrInst::Load { dst, ptr, ty } if is_i64(ty) && *ptr == slot => *dst, _ => return None,
    };
    let (shc, k) = match &w[2] {
        IrInst::Bin { op: ABin::Shl, dst, a, b, ty }
            if is_i64(ty) && *a == Value::Reg(lb) => {
            let k = shift_imm(*b)?;
            (*dst, k)
        }
        _ => return None,
    };
    if k < 1 || k > 63 { return None; }
    let k = k as u32;
    let andd = match &w[3] {
        IrInst::Bin { op: ABin::BitAnd, dst, a, b, ty }
            if is_i64(ty) && *a == Value::Reg(shc) && imm_is_u32_max(*b) => *dst,
        _ => return None,
    };
    let xore = match &w[4] {
        IrInst::Bin { op: ABin::BitXor, dst, a, b, ty } if is_i64(ty) => {
            let ok = matches!((*a, *b),
                (Value::Reg(x), Value::Reg(y)) if (x == la && y == andd) || (x == andd && y == la));
            if !ok { return None; }
            *dst
        }
        _ => return None,
    };
    match &w[5] {
        IrInst::Store { ptr, val, ty } if is_i64(ty) && *ptr == slot && *val == Value::Reg(xore) => {}
        _ => return None,
    }
    Some((XorshiftPlan::ShiftMaskXor { slot, k }, vec![la, lb, shc, andd, xore], 6))
}

/// M7-e3n：识别形态 B `x = x ^ (x >> k)` 的连续 5 条 IR：
///   Load A,ptr; Load B,ptr; Shr C=B>>k; BitXor E=A^C; Store ptr=E
/// Shr 为算术右移（sarq），与通用 Shr 语义一致。返回 (plan, 4 个中间 vreg, 5)。
fn match_xorshift_shift(w: &[IrInst]) -> Option<(XorshiftPlan, Vec<VReg>, usize)> {
    if w.len() < 5 { return None; }
    let is_i64 = |t: &IrType| *t == IrType::I64;
    let (slot, la) = match &w[0] {
        IrInst::Load { dst, ptr, ty } if is_i64(ty) => (*ptr, *dst), _ => return None,
    };
    let lb = match &w[1] {
        IrInst::Load { dst, ptr, ty } if is_i64(ty) && *ptr == slot => *dst, _ => return None,
    };
    let (shc, k) = match &w[2] {
        IrInst::Bin { op: ABin::Shr, dst, a, b, ty }
            if is_i64(ty) && *a == Value::Reg(lb) => {
            let k = shift_imm(*b)?;
            (*dst, k)
        }
        _ => return None,
    };
    if k < 1 || k > 63 { return None; }
    let k = k as u32;
    let xore = match &w[3] {
        IrInst::Bin { op: ABin::BitXor, dst, a, b, ty } if is_i64(ty) => {
            let ok = matches!((*a, *b),
                (Value::Reg(x), Value::Reg(y)) if (x == la && y == shc) || (x == shc && y == la));
            if !ok { return None; }
            *dst
        }
        _ => return None,
    };
    match &w[4] {
        IrInst::Store { ptr, val, ty } if is_i64(ty) && *ptr == slot && *val == Value::Reg(xore) => {}
        _ => return None,
    }
    Some((XorshiftPlan::ShiftXor { slot, k }, vec![la, lb, shc, xore], 5))
}

/// M7-e3m：识别 popcount 内层 `c = c + (x & 1); x = x >> 1` 的 8 条连续
/// IR 形态，返回 (c 槽 Alloca vreg, x 槽 Alloca vreg, 6 个中间 vreg)。
/// 这是 GCC/LLVM 对 `while(x){ c+=x&1; x>>=1 }` 的标准变换：
///   `sarq $1, x` 把旧 x 的 bit0 移入 CF（补码最低位即 `x & 1`，对负数 /
///   i64::MIN 同样成立），`adcq $0, c` 用 CF 完成 `c += x & 1`；两条还同时
///   完成 `x >>= 1`（算术右移，与通用 Shr 的 sarq 一致），故 8 条 IR 可压成
///   2 条机器指令。命中条件极严（任一不符返回 None）：
///     ① 恰 8 条、全部 I64；Add 恒为第 4 条、后 4 条次序固定；
///     ② c 槽与 x 槽是两个不同的 Alloca vreg（pc != px）；
///     ③ BitAnd 掩码是立即数 1、源是第一次 Load x 的结果（`x&1` / `1&x`）；
///     ④ Add 两侧恰是「Load c 结果」与「BitAnd 结果」（加法可交换）；
///     ⑤ Shr 源是第二次 Load x 的结果、移位量是立即数 1（不可交换）；
///     ⑥ 两个 Store 分别写回 pc / px 槽，值恰是 Add / Shr 的结果。
/// 前 4 条前缀支持两种等价排列（Dial 按「先左后右」内联求值二元运算）：
///   A `c + (x&1)`  → Load c(0); Load x(1); BitAnd(2); Add(3)
///   B `(x&1) + c`  → Load x(0); BitAnd(1); Load c(2); Add(3)
/// 前 3 条都在任何 Store 之前，读到的都是循环开始时的旧值，故两种排列
/// 语义完全等价；逐一尝试 (Load c 索引, Load x 索引, BitAnd 索引) 映射。
/// 仅 x86-64 使用；AArch64 从不调用本函数。
fn match_popcnt_window(w: &[IrInst]) -> Option<(VReg, VReg, [VReg; 6])> {
    if w.len() != 8 { return None; }
    let is_i64 = |t: &IrType| *t == IrType::I64;
    for (ic, ix, ib) in [(0usize, 1usize, 2usize), (2, 0, 1)] {
        // [ic] c = *pc
        let (pc, lc) = match &w[ic] {
            IrInst::Load { dst, ptr, ty } if is_i64(ty) => (*ptr, *dst),
            _ => continue,
        };
        // [ix] x = *px
        let (px, lx) = match &w[ix] {
            IrInst::Load { dst, ptr, ty } if is_i64(ty) => (*ptr, *dst),
            _ => continue,
        };
        if pc == px { continue; }
        // [ib] t = x & 1（或 1 & x）
        let t = match &w[ib] {
            IrInst::Bin { op: ABin::BitAnd, dst, a, b, ty } if is_i64(ty) => {
                let (aa, bb) = (*a, *b);
                let (rx, ok) = match (aa, bb) {
                    (Value::Reg(r), x) => (r, imm_is_one(x)),
                    (x, Value::Reg(r)) => (r, imm_is_one(x)),
                    _ => continue,
                };
                if rx != lx || !ok { continue; }
                *dst
            }
            _ => continue,
        };
        // [3] c2 = lc + t（或 t + lc）
        let c2 = match &w[3] {
            IrInst::Bin { op: ABin::Add, dst, a, b, ty } if is_i64(ty) => {
                let (aa, bb) = (*a, *b);
                let ok = matches!((aa, bb),
                    (Value::Reg(x), Value::Reg(y)) if (x == lc && y == t) || (x == t && y == lc));
                if !ok { continue; }
                *dst
            }
            _ => continue,
        };
        // [4] *pc = c2
        match &w[4] {
            IrInst::Store { ptr, val, ty } if is_i64(ty) && *ptr == pc && *val == Value::Reg(c2) => {}
            _ => continue,
        }
        // [5] x = *px（第二次）
        let lx2 = match &w[5] {
            IrInst::Load { dst, ptr, ty } if is_i64(ty) && *ptr == px => *dst,
            _ => continue,
        };
        // [6] x2 = lx2 >> 1
        let x2 = match &w[6] {
            IrInst::Bin { op: ABin::Shr, dst, a, b, ty }
                if is_i64(ty) && *a == Value::Reg(lx2) && imm_is_one(*b) => *dst,
            _ => continue,
        };
        // [7] *px = x2
        match &w[7] {
            IrInst::Store { ptr, val, ty } if is_i64(ty) && *ptr == px && *val == Value::Reg(x2) => {}
            _ => continue,
        }
        return Some((pc, px, [lc, lx, t, c2, lx2, x2]));
    }
    None
}

/// M7-e3i：判定常量 b 是否为 `2^k`（`k∈[1,30]`）并逐值自校验位移序列。
///
/// `k<=30` 保证 Mod 序列里 `andq` 的掩码 `2^k-1` 是合法 imm32 正数、Div/Mod
/// 的移位量 `k` 与 Bias 序列的 `64-k`（∈[34,63]）都是合法 imm8。返回 Some(k)
/// 前用 i64 逐值模拟 `inst_const_div_pow2` 发射的 Div/Mod 结果，覆盖
/// i64::MIN/MAX、0、±1、±2^k 边界、k·2^k±δ 临界区、2000 组伪随机大数，
/// 商与余数都必须等于 Rust/C 的向零截断 `/`、`%`；任一点不符即返回 None，
/// 由调用方退回 M7-d 魔数逆元 / idivq。「宁可不优化，绝不发错代码」。
fn const_div_pow2(b: Value) -> Option<u32> {
    let d = match b {
        Value::Imm(Imm::I64(n)) => n,
        Value::Imm(Imm::I32(n)) => n as i64,
        Value::Imm(Imm::I8(n)) => n as i64,
        Value::Imm(Imm::I1(v)) => v as i64,
        _ => return None,
    };
    if d < 2 || d > (1i64 << 30) { return None; }
    if d & (d - 1) != 0 { return None; }  // 非 2 的幂
    let k = d.trailing_zeros();
    // 精确模拟发射序列（wrapping 语义对应 x86 64 位寄存器行为）。
    fn sim(n: i64, k: u32) -> (i64, i64) {
        let bias = ((n >> 63) as u64 >> (64 - k)) as i64; // 算术>>63 得全 1/0，逻辑>>(64-k)
        let mask = (1i64 << k) - 1;
        let sum = n.wrapping_add(bias);
        let q = sum >> k;                       // sarq $k
        let r = (sum & mask).wrapping_sub(bias); // andq mask; subq bias
        (q, r)
    }
    let mut pts: Vec<i64> = vec![
        i64::MIN, i64::MIN + 1, i64::MAX, i64::MAX - 1,
        0, 1, -1, 2, -2, 3, -3,
        d, -d, d - 1, d + 1, -d + 1, -d - 1,
    ];
    let qm = i64::MAX / d;
    for mul in [1i64, 2, 3, 7, 100, 1000, qm, qm - 1] {
        for dl in -2i64..=2 {
            pts.push(mul.wrapping_mul(d).wrapping_add(dl));
            pts.push((-mul).wrapping_mul(d).wrapping_add(dl));
        }
    }
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..2000 {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        let v = (x >> 1) as i64;
        pts.push(v); pts.push(-v);
    }
    if pts.iter().all(|&n| { let (q, r) = sim(n, k); q == n / d && r == n % d }) {
        Some(k)
    } else {
        None
    }
}

/// M7-e3j：判定常量位与掩码 n 是否落在 `[2^31, 2^32-1]` 并逐值自校验。
///
/// 下界 2^31：`n <= i32::MAX` 时 andq 的 imm32 为正、符号扩展高位为 0，
///   `andq $n, %r` 一条已最优，bin_src 直接吃立即数，无需优化；只有
///   `n > i32::MAX` 才会被 materialize 成 movabsq 64 位掩码（本批的目标）。
/// 上界 2^32-1：n 的高 32 位必须为 0，才保证 `x & n` 的结果高 32 位恒为 0，
///   才能用「32 位与 + 零扩展清高位」等价替代；`n >= 2^32` 掩码含高位，
///   32 位与语义不成立，必须退回 movabsq + andq。
/// 返回 Some(n) 前用 i64 逐值模拟 32 位序列 `(x as u32 & n as u32)` 的
/// 零扩展结果，覆盖 i64::MIN/MAX、0、±1、2^31/2^32 边界与 2000 组伪随机
/// 大数，必须等于 Rust/C 的 64 位按位与 `x & n`；任一点不符即返回 None，
/// 由调用方退回通用 andq。「宁可不优化，绝不发错代码」。
fn const_and_u32(b: Value) -> Option<i64> {
    let n = match b {
        Value::Imm(Imm::I64(v)) => v,
        Value::Imm(Imm::I32(v)) => v as i64,
        Value::Imm(Imm::I8(v)) => v as i64,
        Value::Imm(Imm::I1(v)) => v as i64,
        _ => return None,
    };
    if n < (1i64 << 31) || n > 0xFFFF_FFFFi64 { return None; }
    // 精确模拟发射序列：andl $n,%eax / movl %eax,%eax 的结果都是
    // 「低 32 位按位与 n 的低 32 位，再零扩展到 64 位」。
    fn sim(x: i64, n32: u32) -> i64 {
        ((x as u32) & n32) as u64 as i64
    }
    let n32 = n as u32;
    let mut pts: Vec<i64> = vec![
        i64::MIN, i64::MIN + 1, i64::MAX, i64::MAX - 1,
        0, 1, -1, 2, -2, 3, -3,
        n, -n, n - 1, n + 1,
        0x7fff_ffffi64, 0x8000_0000i64, 0xffff_ffffi64, 0x1_0000_0000i64,
    ];
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for _ in 0..2000 {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        let v = (x >> 1) as i64;
        pts.push(v); pts.push(-v);
    }
    if pts.iter().all(|&v| sim(v, n32) == (v & n)) {
        Some(n)
    } else {
        None
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
        // v1.21：KvMap 走专用 ARC——release 逐条目释放值装箱，避免 map<string,Struct>
        // 的 ARC box 泄漏。与 LLVM release_value 的 dial_kvmap_retain/release 对齐。
        (ArcKind::KvMap, true) => "dial_kvmap_retain",
        (ArcKind::KvMap, false) => "dial_kvmap_release",
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
        T::KvMap(..) => Some(ArcKind::KvMap),
        T::Map | T::Set(_) | T::Channel(_) => Some(ArcKind::Generic),
        _ => None,
    }
}

// ============================================================================
// 函数发射
// ============================================================================

/// M7-e3z：DWARF 类型 DIE 的分类。Dial 的 AST 类型很多，但 gdb print 关心的只是
/// 底层机内表示与编码类别——收敛成 5 类，对应 emit_dwarf_cu 里的类型 DIE。
#[derive(Clone, Copy, PartialEq, Eq)]
enum DwarfType {
    /// 有符号 64 位整数（int / char，底层 i64）。
    Int,
    /// 64 位浮点（float，底层 f64）。
    Float,
    /// M7-e3ab：有符号 32 位整数（int32，底层 i32，栈槽 4 字节）。
    I32,
    /// M7-e3ab：无符号 8 位整数（byte，底层 i8，栈槽低 1 字节，DW_ATE_unsigned）。
    Byte,
    /// M7-e3ab：32 位浮点（float32，底层 f32，栈槽低 4 字节）。
    F32,
    /// 布尔（底层 i64 存 0/1，DW_ATE_boolean + byte_size 8 让 gdb 显示 true/false）。
    Bool,
    /// 字符串 / 字节串：堆句柄，按 char* 呈现，gdb 能显示 C 字符串内容。
    Str,
    /// 数组 / map / set / 结构体 / 裸指针 / 函数地址：不透明指针句柄。
    Ptr,
}

/// M7-e3z：把 AST 声明类型映射到 DWARF 分类。单点实现，避免多处散落判断。
fn dwarf_type(t: &crate::ast::Type) -> DwarfType {
    use crate::ast::Type as T;
    match t {
        T::Bool => DwarfType::Bool,
        T::Float => DwarfType::Float,
        // M7-e3ab：局部变量保留精确宽度 / 符号性。
        T::Float32 => DwarfType::F32,
        T::Int32 => DwarfType::I32,
        T::Byte => DwarfType::Byte,
        T::String | T::Bytes => DwarfType::Str,
        T::Int | T::Char => DwarfType::Int,
        // 其余（容器句柄 / 结构体 / 指针 / 函数地址 / void）统一呈现为指针。
        _ => DwarfType::Ptr,
    }
}

/// M7-e3w：一个函数的 DWARF DIE 所需元数据（由 emit_function 在拿到栈布局后收集）。
struct FnDwarfInfo {
    name: String,
    /// 函数末尾标签（emit_function 追加），供 DWARF high_pc 精确引用。
    end_label: String,
    /// M7-e3aa：本函数源文件的 .file 编号（1-based）。单文件恒为 1。
    file_index: u8,
    decl_line: u32,
    /// (形参名, 相对 rsp 的真实栈槽偏移, 类型分类)（main 的 argc/argv 不入 DWARF，
    /// 与 LLVM 后端一致）。M7-e3ax：偏移取自 prologue 写参用的 alloca_off[param_slot]，
    /// 与局部变量的 location 同源；此前误用 8*i 占位值，gdb print 形参读到垃圾。
    params: Vec<(String, i64, DwarfType)>,
    /// (变量名, 声明行, 相对 rsp 的栈槽偏移, 类型分类)。
    vars: Vec<(String, u32, i64, DwarfType)>,
}

fn emit_function(
    out: &mut String,
    ip: &IrProgram,
    f: &IrFunction,
    target: EmitTarget,
    local: &HashSet<String>,
    recursive_funcs: &HashSet<String>,
    debug_info: bool,
    file_index: u8,
    opt_level: OptLevel,
    safety: SafetyMode,
    pgo: &PgoPlan<'_>,
) -> Result<FnDwarfInfo, String> {
    let mut e = FnEmit::new(ip, f, target, local, recursive_funcs.contains(&f.name), debug_info, file_index, opt_level, safety, pgo);
    e.compute_layout();
    e.analyze_dead_defs();
    e.analyze_fusable();
    e.analyze_residency();
    e.analyze_popcnt(); // M7-e3m：依赖 promotable，须在 analyze_residency 之后
    e.analyze_xorshift(); // M7-e3n：同上，两种融合窗口形态互斥、互不重叠
    e.prologue();
    // R4 插桩：函数入口计数（在驻留开启前，clobber 的寄存器无活跃值）。
    if let Some(&fi) = pgo.fn_counter_idx.get(&f.name) {
        e.pgo_counter_inc(fi);
    }
    for (idx, b) in f.blocks.iter().enumerate() {
        let lbl = format!(".{}_b{}:", e.prefix, b.id);
        e.line(&lbl);
        // R4 插桩：回边循环头计数（begin_block 前，避免干扰驻留态）。
        if let Some(&li) = pgo.loop_counter_idx.get(&(f.name.clone(), b.id)) {
            e.pgo_counter_inc(li);
        }
        e.begin_block(idx);
        // M7-e3m/e3n：窗口可在块内任意起点。popcnt 命中处由 inst_popcnt_fused
        // 的两条指令取代整段 8 条 IR（skip 7）；xorshift 命中处由
        // inst_xorshift_fused 取代 5/6 条 IR（skip wlen-1）。发射期二次确认
        // 失败则 skip 保持 0，逐条走原路径，行为完全不变。两类窗口形态互斥
        // （popcnt 是 Load/Add/BitAnd/Shr，xorshift 是 Shl|Shr+BitXor），同一
        // 起点至多命中一类，故共用一个 skip 计数安全。
        let mut skip = 0usize;
        for (ii, inst) in b.insts.iter().enumerate() {
            if skip > 0 { skip -= 1; continue; }
            if let Some((pc, px)) = e.popcnt_fuse.get(&(idx, ii)).copied() {
                let native = e.popcnt_native.contains(&(idx, ii));
                if e.inst_popcnt_fused(pc, px, native) { skip = 7; continue; }
            }
            if let Some((plan, wlen)) = e.xorshift_fuse.get(&(idx, ii)).copied() {
                if e.inst_xorshift_fused(plan) { skip = wlen - 1; continue; }
            }
            e.inst(inst);
        }
        // 只回写必须跨块保留的值；term 仍在驻留态发射，phi 实参 / 返回值
        // 直接从物理寄存器读，随后才关闭驻留。
        e.flush_residency();
        e.term(b.id, &b.term);
        e.end_block();
    }
    // M7-c2 / M7-e1 / M7-e2 / M7-e3a / M7-e3d：x86-64 发射末尾依次做六个纯文本
    // pass，顺序固定：① 相邻冗余 reload 消除 → ①' 紧邻异寄存器 store→load 转发
    // → ② 全函数死 store 消除 → ③ 跳转重定向 + fallthrough jmp 消除 → ④ 死
    // trampoline 块删除 + 二次 fallthrough 消除 → ⑤ 回边循环头 16B 对齐。
    // ①①'② 顺序不可颠倒（先删 / 改写紧邻 load 才暴露孤立 store）；③ 在精简后的
    // 指令流上重定向，清空 trampoline 引用；④ 在无引用前提下安全删死块并消掉
    // 新暴露的 fallthrough；⑤ 必须最后——只有最终跳转 / 删块 / 删 store 定型后，
    // 回边集合才反映真实布局，.p2align 才能用 NOP padding 吸收前面所有 pass 的
    // 字节变化，令每个循环头地址低 4 位恒为 0、取指边界稳定。正是 ⑤ 让 ①' 的
    // 删 store 不再平移后续超热循环、−3.7% 静态收益得以落地（M7-e3c 单独做 ①'
    // 时 b23 稳定退化 −27%）。lo/hi 圈定本函数 vreg spill 槽区，①'② 共用。
    if matches!(target, EmitTarget::X86_64SysV) && !debug_info {
        // M7-e3w：-g 时跳过所有会改写栈槽读写的文本 pass，保证 DWARF
        // DW_OP_fbreg 指向的 alloca_off 槽里始终是该变量的最新值。
        let s = elide_redundant_reloads(&e.out);
        let (lo, hi) = match (e.reg_off.first(), e.reg_off.last()) {
            (Some(&a), Some(&b)) => (a, b + 8),
            _ => (0, 0),
        };
        let f = forward_store_load(&s, lo, hi);
        let d = elide_dead_stores(&f, lo, hi);
        let t = thread_jumps(&d);
        let dt = elide_dead_trampolines(&t);
        // R4：仅 optimize 态的冷函数跳过循环头对齐（省 padding）；baseline /
        // instrument / optimize-hot 一律保留原对齐，故默认输出逐字节不变。
        if pgo.optimize && !e.pgo_hot {
            out.push_str(&dt);
        } else {
            out.push_str(&align_loop_headers(&dt));
        }
    } else {
        out.push_str(&e.out);
    }

    // M7-e3w：收集 DWARF DIE 元数据。变量名 + 栈槽偏移在 e.alloca_off 定型后读取。
    let mut vars = Vec::new();
    for b in &f.blocks {
        for i in &b.insts {
            if let IrInst::DebugVar { name, slot, line, ty } = i {
                let off = e.alloca_off.get(slot).copied().unwrap_or(0);
                vars.push((name.clone(), *line, off, dwarf_type(ty)));
            }
        }
    }
    let params = if f.name == "main" {
        Vec::new() // main 的 argc/argv 与 LLVM 后端一致，不写进 DWARF
    } else {
        // M7-e3ax：形参真实偏移 = prologue 写参用的 alloca_off[param_slot]。
        // param_slots[idx] 是形参 Alloca 的 dst vreg，其 alloca_off 即 ABI 参数
        // 被 mov 写入的栈槽（与函数体 Load 读参、局部变量 location 完全同源）。
        // 栈传参（>6 个）走 prologue 的 ArgLoc::Stack 分支，值经 g1 落到同一 slot，
        // 偏移仍为 alloca_off[slot]。若某槽被 DCE（回调从不读的 state 参数），
        // prologue 已跳过写入，此处同样用 0 占位——该参数 DWARF 不可 print，
        // 与 LLVM 后端对死参数的呈现一致，不影响任何可达读取路径。
        f.params.iter().enumerate().map(|(idx, (n, t))| {
            let off = f.param_slots.get(idx)
                .and_then(|slot| e.alloca_off.get(slot))
                .copied()
                .unwrap_or(0);
            (n.clone(), off, dwarf_type_ir(t))
        }).collect()
    };
    // M7-e3w：函数末尾标签，DWARF high_pc 精确引用（不能用 `符号+常量`，
    // 链接器对全局符号+加法的重定位不可靠）。
    let end_label = format!(".Ldial_funcend_{}", e.prefix);
    // P1：end_label 与 CU 发射必须同 gate（high_pc 引用），共用 dwarf_enabled。
    if dwarf_enabled(debug_info, target) {
        out.push_str(&format!("{}:\n", end_label));
    }
    Ok(FnDwarfInfo {
        name: f.name.clone(),
        end_label,
        file_index,
        decl_line: f.decl_line,
        params,
        vars,
    })
}
