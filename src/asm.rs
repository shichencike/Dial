//! 自研后端编排：AST → lower → Pass → 发射 .s → `cc -c` 成 .o。
//!
//! 链路终点只到 `.o`：后续的「构建 libdial_std.a + 链接」完全复用现有 `link()` /
//! `final_link()`，因此 FFI 包、--tiny、--single-file、rpath 等既有能力零改动即可生效。
//!
//! LLVM 路径（`src/codegen.rs`）与这里完全并列，由 `main.rs` 的 `--backend` 分派。

use crate::ast::Program;
use crate::safety::SafetyMode;
use crate::ir::OptLevel;
use crate::semantic::{FuncInfo, StructTable};
use crate::target::EmitTarget;
use crate::{codegen_asm, lower, pass};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

/// 按宿主架构选择发射目标。首版只认 x86-64 与 AArch64。
///
/// `DIAL_ASM_TARGET=x86_64|aarch64` 可显式覆盖（交叉汇编 / 检视另一个目标的 .s 用）。
/// 交叉目标需调用方自行保证 `cc` 具备该目标的汇编器（通常配合
/// `DIAL_C_COMPILER="clang --target=aarch64-linux-gnu"` 使用）。
pub fn detect_target() -> Result<EmitTarget, String> {
    if let Ok(v) = std::env::var("DIAL_ASM_TARGET") {
        return match v.trim() {
            "x86_64" | "x86-64" => Ok(EmitTarget::X86_64SysV),
            // MS ABI 显式覆盖值：供交叉检视 Windows .s、本地 CI 验证用。
            "x86_64-ms" | "x86-64-ms" | "x86_64-windows" | "windows" => Ok(EmitTarget::X86_64Ms),
            "aarch64" | "arm64" => Ok(EmitTarget::AArch64),
            other => Err(format!(
                "[asm 后端] 未知 DIAL_ASM_TARGET `{}`（可选 x86_64 / x86_64-ms / aarch64）", other
            )),
        };
    }
    // 宿主架构 + OS 共同决定 x86-64 的 ABI：Windows（含 MinGW 交叉编译目标）
    // 用 MS ABI，其余用 SysV。cfg!(windows) 在 --target x86_64-pc-windows-gnu
    // 交叉编译时也为 true（判断的是编译目标 OS，正是我们需要的）。
    match std::env::consts::ARCH {
        "x86_64" => Ok(if cfg!(windows) { EmitTarget::X86_64Ms } else { EmitTarget::X86_64SysV }),
        "aarch64" => Ok(EmitTarget::AArch64),
        other => Err(format!(
            "[asm 后端] 暂不支持宿主架构 `{}`（首版仅 x86-64 SysV/MS / AArch64）", other
        )),
    }
}

/// 走自研后端编译，产物写到 `obj_path`（与 LLVM 后端同一个 .o 路径），
/// 同时保留一份 `.s` 便于检视 / 黄金对比。
pub fn compile_asm(
    prog: &Program,
    functions: HashMap<String, FuncInfo>,
    structs: StructTable,
    goto_depths: HashMap<String, usize>,
    obj_path: &Path,
    debug_info: bool,
    src_path: &str,
    fn_origin: &std::collections::HashMap<String, String>,
    opt_level: OptLevel,
    safety: SafetyMode,
    target_cpu: crate::target::TargetCpu,
    instrument: bool,
    profile: Option<&crate::pgo::ProfileData>,
) -> Result<(), String> {
    let target = detect_target()?;
    // M7-e3w：gdb 源码断点用绝对路径（与 LLVM 后端 DW_AT_name 一致）。
    // canonicalize 失败（如虚拟路径）时退回原字符串，不致命。
    let src_abs = fs::canonicalize(src_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| src_path.to_string());

    let mut ip = lower::lower_program(prog, functions, structs, goto_depths, debug_info, src_path)?;
    // 调试设施（对齐 codegen 的 DIALC_DUMP_IR）：设 DIALC_DUMP_ASM_IR=1 在优化前
    // 打印自研后端 IR，便于检视 lower 产物与优化 Pass 的输入。
    if std::env::var("DIALC_DUMP_ASM_IR").is_ok() {
        eprintln!("{:#?}", ip);
    }
    // M7-e3ac：优化级别分级。O2（默认）= 既有全部优化，逐字节零回归；
    // O0 跳过全部 Dial 级 IR pass（配合 -g 得到"所写即所得"的调试体验）；
    // O1 仅基础 IR pass；O3 在 O2 之上追加 LLVM InstCombine 风格代数恒等式。
    if opt_level >= OptLevel::O1 {
        pass::run_passes(&mut ip);
        // M10-strfold：字符串字面量拼接的编译期常量折叠（对齐 gcc -O2 的常量折叠）。
        // 只折叠静态串之间的 concat，内容等价 + ARC no-op，双后端语义一致；AArch64 亦安全。
        pass::fold_string_concat(&mut ip);
    }
    // M7-e3p：过程间常量折叠（纯整数函数调用编译期求值）。O2+ 启用。
    // 仅 x86-64 启用：AArch64 保持既有发射路径，`.s` 零变化。
    if opt_level >= OptLevel::O2 && matches!(target, EmitTarget::X86_64SysV) {
        pass::fold_pure_calls(&mut ip);
    }
    // M10b-straccum：字符串累加循环编译期折叠（对标 gcc -O2 对 bench.b20 的整函数折叠）。
    // 严格识别 + 符号执行验证 + 长度上限；O2+ 且仅 x86-64，低优化级 / AArch64 零变化。
    if opt_level >= OptLevel::O2 && matches!(target, EmitTarget::X86_64SysV) {
        pass::fold_string_accum(&mut ip);
    }
    // M8-recspec：递归纯函数闭式特化。O3 + x86-64 only——这是私有后端专属的
    // 语义级激进优化（LLVM 通用后端不会把互递归识别成奇偶闭式）。用探针解释
    // 执行 + 全域验证，在函数入口插 guard：n>=0 走闭式、n<0 保留原递归体。
    // 只对递归纯整数单参函数生效；其余函数零变化。
    if opt_level >= OptLevel::O3 && matches!(target, EmitTarget::X86_64SysV) {
        pass::specialize_recursive_calls(&mut ip);
    }
    // M7-e3t：归纳变量强度削减（base*C+iv → 累加器）。O2+，仅 x86-64。
    if opt_level >= OptLevel::O2 && matches!(target, EmitTarget::X86_64SysV) {
        pass::ivr_strength_reduce(&mut ip);
    }
    // M9-linrec：常量上界线性递推循环编译期闭式求值（仿射/斐波那契）。O3 + x86-64。
    // 私有后端专属：LLVM SCEV 只在有限场景折叠它；这里用 e3p 同源的严格识别 +
    // 采样验证 + i128 矩阵幂，把无参纯函数的单循环递推折叠为 return const。
    // AArch64 / 低优化级 / LLVM 路径零变化。
    if opt_level >= OptLevel::O3 && matches!(target, EmitTarget::X86_64SysV) {
        pass::specialize_linear_recurrences(&mut ip);
    }
    // M7-e3ad：LLVM InstCombine 风格代数恒等式（x*0→0、x+0→x 等）。O3 only，仅 x86-64。
    if opt_level >= OptLevel::O3 && matches!(target, EmitTarget::X86_64SysV) {
        pass::instcombine(&mut ip);
    }
    // 调试：设 DIALC_DUMP_ASM_IR_POST=1 在全部 Pass 后打印 IR。
    if std::env::var("DIALC_DUMP_ASM_IR_POST").is_ok() {
        eprintln!("{:#?}", ip);
    }
    let asm_text = codegen_asm::emit(&ip, target, debug_info, &src_abs, fn_origin, opt_level, safety, target_cpu, instrument, profile)?;

    let s_path = obj_path.with_extension("s");
    fs::write(&s_path, asm_text)
        .map_err(|e| format!("[asm 后端] 写汇编文件 {} 失败: {}", s_path.display(), e))?;

    // 复用 main.rs 的 C 编译器选择逻辑（DIAL_C_COMPILER 覆盖 / zig cc → clang → cc → gcc）。
    let (cc, extra) = crate::cc_command();
    let mut cmd = Command::new(&cc);
    cmd.args(&extra);
    cmd.arg("-c").arg(&s_path).arg("-o").arg(obj_path);
    let status = cmd.status()
        .map_err(|e| format!("[asm 后端] 调用 C 编译器 `{}` 失败: {}", cc, e))?;
    if !status.success() {
        return Err(format!(
            "[asm 后端] 汇编 {} 失败（{} -c 返回非 0）", s_path.display(), cc
        ));
    }
    Ok(())
}
