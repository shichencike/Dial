//! 安全等级（Fail-Fast 模式）——跨后端共用的基础设施。
//!
//! 原本定义在 `codegen.rs`（LLVM 后端）。LLVM 后端改为可选 feature 后，
//! 自研 asm 后端与链接层同样需要它，故外置到这个始终编译的模块，
//! 使 `codegen.rs` 可以整模块 `#[cfg(feature = "llvm")]` 门控而不影响其它模块。

/// 安全等级：决定 Fail-Fast 边界检查强度与 C Runtime 的 `-DDIAL_SAFETY=N`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SafetyMode {
    Debug,
    ReleaseSafe,
    ReleaseFast,
}

impl SafetyMode {
    /// 对应 C Runtime 的 -DDIAL_SAFETY=N
    pub fn as_c_macro(self) -> &'static str {
        match self {
            SafetyMode::Debug => "2",
            SafetyMode::ReleaseSafe => "1",
            SafetyMode::ReleaseFast => "0",
        }
    }
}
