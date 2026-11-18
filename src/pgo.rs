//! 自研 PGO profile：`.dialprof` 纯文本格式（R4，唯一后端收回路线图第 4 项）。
//!
//! 设计目标：简单、确定性、可读、可合并、零依赖（仅 std）。
//! 不依赖 LLVM profraw/profdata，也不依赖 bitcode——asm 后端完全自持。
//!
//! 格式（C 侧 runtime/dial_rt_pgo.c 与 Rust 侧本文件共享同一契约）：
//! ```
//! DIALPGO1\n        <- magic + 版本（第 1 行）
//! N\n               <- 计数器个数（第 2 行）
//! 0 <count0>\n      <- idx 从 0 递增，行序确定
//! 1 <count1>\n
//! ...
//! N-1 <countN-1>\n
//! ```
//!
//! idx → 语义（哪个函数 / 哪个循环头）由 codegen_asm 的 PgoPlan 按 IR 纯函数
//! 重算得出，文件本身不存符号表：训练版与优化版同源码 → 同编号，天然对齐。

use std::fs;
use std::path::Path;

pub const PGO_MAGIC: &str = "DIALPGO1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileData {
    /// 计数器数组，下标即 codegen_asm::PgoPlan 分配的 idx。
    pub counts: Vec<u64>,
}

impl ProfileData {
    /// 从 `.dialprof` 文件读取；格式非法（magic/版本/N/缺行/非数值）一律 Fail-Fast。
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let text = fs::read_to_string(path.as_ref())
            .map_err(|e| format!("[pgo] 无法读取 profile {:?}：{}", path.as_ref().display(), e))?;
        Self::parse(&text)
    }

    /// 解析 profile 文本。公开以便单测与内存态使用。
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let magic = lines.next().ok_or("[pgo] profile 为空，缺 magic 行")?.trim_end();
        if magic != PGO_MAGIC {
            return Err(format!(
                "[pgo] profile magic/版本不符：期望 {}, 实际 {}",
                PGO_MAGIC, magic
            ));
        }
        let n_str = lines.next().ok_or("[pgo] 缺计数器个数 N")?.trim();
        let n: usize = n_str
            .parse()
            .map_err(|_| format!("[pgo] 计数器个数 N 非数值：{}", n_str))?;

        let mut counts = Vec::with_capacity(n);
        for idx in 0..n {
            let line = lines
                .next()
                .ok_or_else(|| format!("[pgo] 计数器行不足：期望 {} 行，实际到 idx {}", n, idx))?;
            let mut it = line.split_whitespace();
            let got_idx: usize = it
                .next()
                .ok_or_else(|| format!("[pgo] idx {} 行缺索引", idx))?
                .parse()
                .map_err(|_| format!("[pgo] idx {} 行索引非数值", idx))?;
            if got_idx != idx {
                return Err(format!(
                    "[pgo] 计数器索引非递增：期望 {}, 实际 {}",
                    idx, got_idx
                ));
            }
            let cnt: u64 = it
                .next()
                .ok_or_else(|| format!("[pgo] idx {} 行缺计数值", idx))?
                .parse()
                .map_err(|_| format!("[pgo] idx {} 计数值非数值", idx))?;
            // 忽略行尾多余空白/字段，保持与 C 侧 fprintf 的宽松兼容。
            counts.push(cnt);
        }
        Ok(ProfileData { counts })
    }

    /// 序列化为确定的文本（供将来工具/测试生成 profile）。行序与 load 契约一致。
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        s.push_str(PGO_MAGIC);
        s.push('\n');
        s.push_str(&self.counts.len().to_string());
        s.push('\n');
        for (idx, c) in self.counts.iter().enumerate() {
            s.push_str(&format!("{} {}\n", idx, c));
        }
        s
    }

    /// 逐项相加合并（为将来多次训练运行合并预留）。长度不一致 Fail-Fast。
    pub fn merge(&mut self, other: &ProfileData) -> Result<(), String> {
        if self.counts.len() != other.counts.len() {
            return Err(format!(
                "[pgo] merge 计数器长度不一致：{} vs {}",
                self.counts.len(),
                other.counts.len()
            ));
        }
        for (a, b) in self.counts.iter_mut().zip(other.counts.iter()) {
            *a = a.saturating_add(*b);
        }
        Ok(())
    }

    /// 计数总和（用于判定全零 → 回退 baseline 布局）。
    pub fn total(&self) -> u64 {
        self.counts.iter().fold(0u64, |a, b| a.saturating_add(*b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let p = ProfileData {
            counts: vec![0, 7, 123456789, 1],
        };
        let text = p.to_text();
        let back = ProfileData::parse(&text).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn bad_magic_fails() {
        let text = "NOTPGO\n0\n";
        assert!(ProfileData::parse(text).is_err());
    }

    #[test]
    fn short_lines_fail() {
        let text = "DIALPGO1\n3\n0 1\n1 2\n"; // 缺 idx2
        assert!(ProfileData::parse(text).is_err());
    }

    #[test]
    fn non_monotonic_idx_fails() {
        let text = "DIALPGO1\n2\n0 1\n2 3\n";
        assert!(ProfileData::parse(text).is_err());
    }

    #[test]
    fn merge_sums() {
        let mut a = ProfileData { counts: vec![1, 2, 3] };
        let b = ProfileData { counts: vec![10, 20, 30] };
        a.merge(&b).unwrap();
        assert_eq!(a.counts, vec![11, 22, 33]);
    }

    #[test]
    fn merge_len_mismatch_fails() {
        let mut a = ProfileData { counts: vec![1] };
        let b = ProfileData { counts: vec![1, 2] };
        assert!(a.merge(&b).is_err());
    }

    #[test]
    fn total_zero_detection() {
        assert_eq!(ProfileData { counts: vec![0, 0] }.total(), 0);
        assert!(ProfileData { counts: vec![0, 1] }.total() > 0);
    }
}
