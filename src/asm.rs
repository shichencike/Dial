//! 自研后端编排：AST → lower → Pass → 发射 .s → `cc -c` 成 .o。
//!
//! 链路终点只到 `.o`：后续的「构建 libdial_std.a + 链接」完全复用现有 `link()` /
//! `final_link()`，因此 FFI 包、--tiny、--single-file、rpath 等既有能力零改动即可生效。
//!
//! LLVM 路径（`src/codegen.rs`）与这里完全并列，由 `main.rs` 的 `--backend` 分派。

use crate::ast::Program;
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
            "aarch64" | "arm64" => Ok(EmitTarget::AArch64),
            other => Err(format!(
                "[asm 后端] 未知 DIAL_ASM_TARGET `{}`（可选 x86_64 / aarch64）", other
            )),
        };
    }
    match std::env::consts::ARCH {
        "x86_64" => Ok(EmitTarget::X86_64SysV),
        "aarch64" => Ok(EmitTarget::AArch64),
        other => Err(format!(
            "[asm 后端] 暂不支持宿主架构 `{}`（首版仅 x86-64 SysV / AArch64）", other
        )),
    }
}

/// 走自研后端编译，产物写到 `obj_path`（与 LLVM 后端同一个 .o 路径），
/// 同时保留一份 `.s` 便于检视 / 黄金对比。
pub fn compile_asm(
    prog: &Program,
    functions: HashMap<String, FuncInfo>,
    structs: StructTable,
    obj_path: &Path,
) -> Result<(), String> {
    let target = detect_target()?;

    let mut ip = lower::lower_program(prog, functions, structs)?;
    pass::run_passes(&mut ip);
    let asm_text = codegen_asm::emit(&ip, target)?;

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
