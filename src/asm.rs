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
    // 严格识别 lower 的原始 5 块骨架 → 必须在 inline / licm 之前跑：后者会重塑块结构，
    // 使骨架识别失败（b20 折叠失效回归的根因）。低优化级 / AArch64 零变化。
    if opt_level >= OptLevel::O2 && matches!(target, EmitTarget::X86_64SysV) {
        pass::fold_string_accum(&mut ip);
    }
    // M16-popcount：标准 `c=0; while(x>0){c+=x&1;x>>=1}` 骨架 → SWAR 位并行计数。
    // O2+ / x86-64 only，置于内联前：内联会把 b23 的循环搬进 run，但骨架不变，
    // 前后均可命中；放前面是为了让内联体直接携带 O(1) 计数而非逐位循环。
    // 仅 x>=0 走 SWAR，负数保留原循环；AArch64 / 低优化级零变化。
    if opt_level >= OptLevel::O2 && matches!(target, EmitTarget::X86_64SysV) {
        pass::fold_popcount(&mut ip);
    }
    // M13-sccp：保守条件常量传播 / 分支折叠。O2+ / x86-64，同内联护栏。
    // 置于内联前：先折叠常量分支简化 CFG，让后续 pass 处理更简单的结构。
    if opt_level >= OptLevel::O2 && matches!(target, EmitTarget::X86_64SysV)
        && !debug_info && profile.is_none() && !instrument
    {
        pass::sccp(&mut ip);
    }
    // M11-inline：通用 Dial 函数内联（对标 gcc -O2 inlining）。O2+ / x86-64 only。
    // 置于 fold_pure_calls 之后：常量实参的纯调用先折叠为常量（连递归一并消掉），
    // 剩余一般调用再内联，避免与折叠抢同一站点。严格候选护栏 + ARC 原样保留。
    // 排除 -g / PGO（训练+优化 / 插桩）：
    //   - -g：内联体的 Line/DebugVar 属被调函数上下文，复制会污染 DWARF 行号表，
    //     使子模块源码断点错位（本后端未生成 DW_TAG_inlined_subroutine）；
    //   - PGO/插桩：profile 的 idx 由 IR 纯函数重算，训练版与优化版必须结构一致，
    //     内联改变块结构会让冷热归属错位。与 LLVM/GCC「train/use 同配置」惯例一致。
    if opt_level >= OptLevel::O2 && matches!(target, EmitTarget::X86_64SysV)
        && !debug_info && profile.is_none() && !instrument
    {
        pass::inline_calls(&mut ip);
        // M12-licm：通用循环不变代码外提。置于内联后、IVR 前——先外提不变量，
        // 可能暴露更多 IVR 可识别的 base*C+iv 模式。同内联的 -g/PGO/插桩护栏。
        pass::licm(&mut ip);
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
    // M15-unroll：保守循环完全展开（标准 for 骨架 + 常量小上界 2..8 + 纯整数 body）。
    // O3 + x86-64，同 -g/PGO/插桩护栏。
    if opt_level >= OptLevel::O3 && matches!(target, EmitTarget::X86_64SysV)
        && !debug_info && profile.is_none() && !instrument
    {
        pass::unroll_loops(&mut ip);
    }
    // M14-dce-fn：全程序死函数消除（Private + 零引用）。置于所有变换之后——
    // 内联/折叠消掉调用点后才暴露出真正的死函数；不动点删除。O1+ / x86-64，
    // 同内联的 -g/PGO/插桩护栏（函数集合变化影响 DWARF / profile idx 对齐）。
    if opt_level >= OptLevel::O1 && matches!(target, EmitTarget::X86_64SysV)
        && !debug_info && profile.is_none() && !instrument
    {
        pass::eliminate_dead_functions(&mut ip);
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
