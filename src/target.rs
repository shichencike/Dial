//! 目标平台差异抽象：x86-64 SysV 与 AArch64 AAPCS。
//!
//! 自研后端把 IR 发射成汇编。两个目标共享同一套 IR 与 Pass，只有寄存器名 /
//! 数量 / 栈对齐不同。本 trait 把所有平台相关量集中在这里，发射器主体只认 trait。

use crate::ir::IrType;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmitTarget {
    /// x86-64 System V AMD64 ABI（首版严格验证目标）。
    X86_64SysV,
    /// AArch64 AAPCS（首版同构实现：所有结构体按指针 ByVal 传递，绕开 HFA 规则）。
    AArch64,
}

impl EmitTarget {
    /// 整型 / 指针参数的传参寄存器（按顺序）。
    pub fn int_arg_regs(self) -> &'static [&'static str] {
        match self {
            EmitTarget::X86_64SysV => &["rdi", "rsi", "rdx", "rcx", "r8", "r9"],
            EmitTarget::AArch64 => &["x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7"],
        }
    }

    /// 浮点参数的传参寄存器。
    pub fn fp_arg_regs(self) -> &'static [&'static str] {
        match self {
            EmitTarget::X86_64SysV => &["xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7"],
            EmitTarget::AArch64 => &["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"],
        }
    }

    /// 整型 callee-saved 寄存器（被本函数使用须入栈保存）。
    pub fn callee_saved_int(self) -> &'static [&'static str] {
        match self {
            EmitTarget::X86_64SysV => &["rbx", "r12", "r13", "r14", "r15"],
            EmitTarget::AArch64 => &["x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28"],
        }
    }

    /// 调用时栈必须对齐到的字节数（两平台都是 16）。
    pub fn call_align(self) -> u32 { 16 }

    /// 红区（red zone）：x86-64 为 128 字节（本后端不依赖它，恒 0 使用），AArch64 为 0。
    pub fn red_zone(self) -> u32 { 0 }

    /// 返回寄存器：整型 / 指针 / 句柄。
    pub fn ret_int_reg(self) -> &'static str {
        match self { EmitTarget::X86_64SysV => "rax", EmitTarget::AArch64 => "x0" }
    }
    /// 返回寄存器：浮点。
    pub fn ret_fp_reg(self) -> &'static str {
        match self { EmitTarget::X86_64SysV => "xmm0", EmitTarget::AArch64 => "v0" }
    }

    /// 调用指令助记符（x86 `call` / aarch64 `blr`）。
    pub fn call_mnem(self) -> &'static str {
        match self { EmitTarget::X86_64SysV => "call", EmitTarget::AArch64 => "blr" }
    }

    /// 函数返回指令。
    pub fn ret_mnem(self) -> &'static str {
        match self { EmitTarget::X86_64SysV => "ret", EmitTarget::AArch64 => "ret" }
    }

    /// 把 IrType 映射为汇编里的「数据宽度后缀 / 操作宽度」：返回 (move_mnem, suffix)。
    /// x86 用 `mov` + `b/w/l/q` 后缀；aarch64 用 `ldr/str` + 字节宽度隐含在寄存器。
    pub fn x86_mov_suffix(self, t: IrType) -> &'static str {
        match t {
            IrType::I64 | IrType::Ptr | IrType::Handle | IrType::FnPtr | IrType::Struct(_) => "q",
            IrType::I32 => "l",
            IrType::F64 => "sd",
            IrType::F32 => "ss",
            IrType::I1 | IrType::I8 => "b",
            IrType::Void => "q",
        }
    }

    /// aarch64 的 load/store 助记符（按字节宽度）。
    pub fn aarch64_ldst(self, t: IrType) -> (&'static str, &'static str) {
        match t {
            IrType::I64 | IrType::Ptr | IrType::Handle | IrType::FnPtr | IrType::Struct(_) => ("ldr", "str"),
            IrType::I32 => ("ldr", "str"),
            IrType::F64 => ("ldr", "str"),
            IrType::F32 => ("ldr", "str"),
            IrType::I1 | IrType::I8 => ("ldrb", "strb"),
            IrType::Void => ("ldr", "str"),
        }
    }
}
