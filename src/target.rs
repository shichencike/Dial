//! 目标平台差异抽象：x86-64 SysV 与 AArch64 AAPCS。
//!
//! 自研后端把 IR 发射成汇编。两个目标共享同一套 IR 与 Pass，只有寄存器名 /
//! 数量 / 栈对齐不同。本 trait 把所有平台相关量集中在这里，发射器主体只认 trait。

use crate::ir::IrType;

/// x86-64 微架构级别（--target-cpu，仅作用于 asm 后端；对齐 LLVM 的
/// `x86-64`/`x86-64-v2`/`x86-64-v3`/`x86-64-v4` 命名与 host_cpu() 语义）。
///
/// 默认 Baseline：只用所有 x86-64 CPU 都保证支持的指令（SSE2），生成的 `.s`
/// 与引入本枚举之前逐字节一致（零回归护栏）。显式指定更高级别才启用对应
/// ISA 的 targeted 指令融合（BMI/LZCNT/AVX2/FMA…），每类融合由独立批次实现
/// 并受这些 `has_*` 门控；本批只建立开关基础设施，不发射任何新指令。
///
/// AArch64 忽略本设置（NEON 为标配，无等效分级），零变化。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum TargetCpu {
    /// x86-64 基线（SSE2），默认。
    #[default]
    Baseline,
    /// x86-64-v2：+ SSE4.2 / POPCNT / LZCNT / BMI1 / ABM。
    V2,
    /// x86-64-v3：+ AVX2 / BMI2 / FMA / LZCNT（含 v2 全部）。
    V3,
    /// x86-64-v4：+ AVX-512F（含 v3 全部）。
    V4,
}

impl TargetCpu {
    /// 解析命令行值；别名 `native` 映射最高级（与 LLVM host_cpu() 一致的
    /// 常识性约定，实际仍由用户保证目标机支持）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "baseline" | "x86-64" | "x86_64" => Some(Self::Baseline),
            "x86-64-v2" | "v2" => Some(Self::V2),
            "x86-64-v3" | "v3" => Some(Self::V3),
            "x86-64-v4" | "v4" | "native" => Some(Self::V4),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "x86-64",
            Self::V2 => "x86-64-v2",
            Self::V3 => "x86-64-v3",
            Self::V4 => "x86-64-v4",
        }
    }

    /// 是否启用 POPCNT / LZCNT（v2+）。注：现有 popcnt 融合（e3y）在 baseline
    /// 下也已启用——因为 /proc/cpuinfo 实测目标机支持，且那是有 CPU 特性兜底的
    /// 既有行为；本开关只约束**新增**高级指令融合，不改变 e3y 既有路径。
    pub fn has_lzcnt(self) -> bool { self >= Self::V2 }
    /// BMI1（andn / bextr / blsi 等）v2+。
    pub fn has_bmi1(self) -> bool { self >= Self::V2 }
    /// BMI2（shlx / shrx / sarx / pdep / pext / mulx）v3+。
    pub fn has_bmi2(self) -> bool { self >= Self::V3 }
    /// AVX2（含 FMA）v3+。
    pub fn has_avx2(self) -> bool { self >= Self::V3 }
    /// FMA3 v3+。
    pub fn has_fma(self) -> bool { self >= Self::V3 }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmitTarget {
    /// x86-64 System V AMD64 ABI（Linux / macOS / BSD）。
    X86_64SysV,
    /// x86-64 Microsoft ABI（Windows，含 MinGW-w64）。与 SysV 的关键差异：
    /// 前 4 个参数走 rcx/rdx/r8/r9（统一位置槽，整浮二选一）、调用方须预留
    /// 32 字节 shadow space、callee-saved 含 rdi/rsi/xmm6-15、变参浮点须镜像
    /// 到对应 GP 寄存器（不用 %al）。见 codegen_asm.rs 各 target 分支。
    X86_64Ms,
    /// AArch64 AAPCS（首版同构实现：所有结构体按指针 ByVal 传递，绕开 HFA 规则）。
    AArch64,
}

impl EmitTarget {
    /// 是否 x86-64（SysV 或 MS ABI）——两者共享指令集、助记符、返回寄存器，
    /// 仅 ABI（传参/栈/callee-saved）不同。用于把大量 `== X86_64SysV` 门控
    /// 统一收口到「是 x86-64 即可」，避免新枚举漏判。
    pub fn is_x86_64(self) -> bool {
        matches!(self, EmitTarget::X86_64SysV | EmitTarget::X86_64Ms)
    }

    /// 是否 Windows MS ABI（需要 shadow space / 统一位置槽 / callee-saved rdi/rsi）。
    pub fn is_ms_abi(self) -> bool {
        matches!(self, EmitTarget::X86_64Ms)
    }

    /// 整型 / 指针参数的传参寄存器（按顺序）。
    pub fn int_arg_regs(self) -> &'static [&'static str] {
        match self {
            EmitTarget::X86_64SysV => &["rdi", "rsi", "rdx", "rcx", "r8", "r9"],
            EmitTarget::X86_64Ms => &["rcx", "rdx", "r8", "r9"],
            EmitTarget::AArch64 => &["x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7"],
        }
    }

    /// 浮点参数的传参寄存器。
    pub fn fp_arg_regs(self) -> &'static [&'static str] {
        match self {
            EmitTarget::X86_64SysV => &["xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7"],
            EmitTarget::X86_64Ms => &["xmm0", "xmm1", "xmm2", "xmm3"],
            EmitTarget::AArch64 => &["v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7"],
        }
    }

    /// 整型 callee-saved 寄存器（被本函数使用须入栈保存）。
    pub fn callee_saved_int(self) -> &'static [&'static str] {
        match self {
            EmitTarget::X86_64SysV => &["rbx", "r12", "r13", "r14", "r15"],
            // MS ABI：rbx/rbp/rdi/rsi/r12-r15 均 callee-saved。
            EmitTarget::X86_64Ms => &["rbx", "rbp", "rdi", "rsi", "r12", "r13", "r14", "r15"],
            EmitTarget::AArch64 => &["x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27", "x28"],
        }
    }

    /// 浮点 callee-saved 寄存器（MS ABI 的 xmm6-15 须由被调方保存）。
    pub fn callee_saved_fp(self) -> &'static [&'static str] {
        match self {
            EmitTarget::X86_64Ms => &["xmm6", "xmm7", "xmm8", "xmm9", "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15"],
            _ => &[],
        }
    }

    /// shadow space 字节数：MS ABI 要求调用方为 4 个寄存器参数预留 32 字节；
    /// SysV / AArch64 无此要求（红区另论，本后端不使用）。
    pub fn shadow_space(self) -> u32 {
        match self { EmitTarget::X86_64Ms => 32, _ => 0 }
    }

    /// 调用时栈必须对齐到的字节数（x86 16；AArch64 16）。
    pub fn call_align(self) -> u32 { 16 }

    /// 红区（red zone）：x86-64 为 128 字节（本后端不依赖它，恒 0 使用），AArch64 为 0。
    pub fn red_zone(self) -> u32 { 0 }

    /// 返回寄存器：整型 / 指针 / 句柄。
    pub fn ret_int_reg(self) -> &'static str {
        match self { EmitTarget::AArch64 => "x0", _ => "rax" }
    }
    /// 返回寄存器：浮点。
    pub fn ret_fp_reg(self) -> &'static str {
        match self { EmitTarget::AArch64 => "v0", _ => "xmm0" }
    }

    /// 调用指令助记符（x86 `call` / aarch64 `blr`）。
    pub fn call_mnem(self) -> &'static str {
        match self { EmitTarget::AArch64 => "blr", _ => "call" }
    }

    /// 函数返回指令（x86 与 AArch64 共用 `ret`）。
    pub fn ret_mnem(self) -> &'static str {
        "ret"
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
