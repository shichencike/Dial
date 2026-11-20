//! 语义版本与版本区间（P4.2）。
//!
//! 纯函数、零新依赖（手写 semver 子集）。用途：
//! - `dial.toml [dependencies]` 的版本区间（caret `^` / tilde `~` / 精确 `=`/裸版本 / 通配 `*`）；
//! - 静态索引里每个包版本的依赖声明；
//! - 依赖仲裁时对同一包的多个区间求**交集**，交集为空即冲突（Fail-Fast）。
//!
//! 与 ABI 的关系（见 README §25.3 / dialpkg-roadmap §4 P4.2）：
//! `abi` 是最终**链接期**的二进制兼容判据；语义版本是**选取期**的功能版本判据。
//! 两者职责不同、互不替代——版本区间决定「装哪个版本」，ABI 决定「能不能链」。

use crate::error::{DialcError, Result};
use std::cmp::Ordering;
use std::fmt;

/// 语义版本：major.minor.patch[-prerelease]。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// 预发布标识（不含前导 `-`）。空串表示正式版。
    pub pre: String,
}

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let s = s.trim();
        if s.is_empty() {
            return Err(bad(s, "版本为空"));
        }
        // 拆出预发布（只取第一个 '-'，且 '.' 不属于预发布分隔符的话也无妨）。
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p.to_string()),
            None => (s, String::new()),
        };
        let nums: Vec<&str> = core.split('.').collect();
        if nums.len() != 3 {
            return Err(bad(s, "版本必须是 major.minor.patch 三段（如 1.2.3）"));
        }
        let mut parts = [0u64; 3];
        for (i, n) in nums.iter().enumerate() {
            if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad(s, "版本各段必须是非负十进制整数"));
            }
            parts[i] = n.parse::<u64>().map_err(|_| bad(s, "版本数字溢出"))?;
        }
        if !pre.is_empty() && !valid_pre(&pre) {
            return Err(bad(s, "预发布标识含非法字符"));
        }
        Ok(Version { major: parts[0], minor: parts[1], patch: parts[2], pre })
    }

    /// 是否正式版（无预发布）。
    pub fn is_release(&self) -> bool {
        self.pre.is_empty()
    }
}

fn valid_pre(pre: &str) -> bool {
    !pre.is_empty()
        && pre
            .split('.')
            .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// semver 预发布优先级：数字标识按数值、字母标识按字典序，数字 < 字母；
/// 前缀相同则更短者优先；正式版 > 任何预发布。
fn cmp_pre(a: &str, b: &str) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let ai: Vec<&str> = a.split('.').collect();
            let bi: Vec<&str> = b.split('.').collect();
            for (x, y) in ai.iter().zip(bi.iter()) {
                let xnum = x.bytes().all(|c| c.is_ascii_digit());
                let ynum = y.bytes().all(|c| c.is_ascii_digit());
                let o = match (xnum, ynum) {
                    (true, true) => {
                        let xr = x.trim_start_matches('0');
                        let yr = y.trim_start_matches('0');
                        xr.len().cmp(&yr.len()).then_with(|| xr.cmp(yr))
                    }
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            ai.len().cmp(&bi.len())
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| cmp_pre(&self.pre, &other.pre))
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.pre.is_empty() {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
        } else {
            write!(f, "{}.{}.{}-{}", self.major, self.minor, self.patch, self.pre)
        }
    }
}

/// 端点是否含等号。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Inclusive {
    Yes,
    No,
}

/// 版本区间约束：`[min, max]`（各端可选、可含/不含）。
/// 求交集 = min 取更严者、max 取更严者；空集即冲突。
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Constraint {
    min: Option<(Version, Inclusive)>,
    max: Option<(Version, Inclusive)>,
}

impl Constraint {
    /// 区间是否包含某版本。
    pub fn matches(&self, v: &Version) -> bool {
        if let Some((lo, inc)) = &self.min {
            let ord = v.cmp(lo);
            if ord == Ordering::Less || (ord == Ordering::Equal && *inc == Inclusive::No) {
                return false;
            }
        }
        if let Some((hi, inc)) = &self.max {
            let ord = v.cmp(hi);
            if ord == Ordering::Greater || (ord == Ordering::Equal && *inc == Inclusive::No) {
                return false;
            }
        }
        true
    }

    /// 与另一约束求交集；None 表示空集（冲突）。
    pub fn intersect(&self, other: &Constraint) -> Option<Constraint> {
        let min = match (&self.min, &other.min) {
            (None, o) => o.clone(),
            (s, None) => s.clone(),
            (Some(s), Some(o)) => Some(if tighter_lower(&s.0, &o.0) { s.clone() } else { o.clone() }),
        };
        let max = match (&self.max, &other.max) {
            (None, o) => o.clone(),
            (s, None) => s.clone(),
            (Some(s), Some(o)) => Some(if tighter_upper(&s.0, &o.0) { s.clone() } else { o.clone() }),
        };
        // 判空：min > max，或 min==max 但有一端开区间。
        if let (Some((lo, li)), Some((hi, hi_inc))) = (&min, &max) {
            match lo.cmp(hi) {
                Ordering::Greater => return None,
                Ordering::Equal if *li == Inclusive::No || *hi_inc == Inclusive::No => return None,
                _ => {}
            }
        }
        Some(Constraint { min, max })
    }

    /// 展示为可读区间（诊断用）。
    pub fn describe(&self) -> String {
        match (&self.min, &self.max) {
            (None, None) => "*".to_string(),
            (Some((lo, li)), None) => format!("{}{}", lo, if *li == Inclusive::Yes { "<=" } else { "<" }),
            (None, Some((hi, hi_inc))) => format!("{}{}", if *hi_inc == Inclusive::Yes { "<=" } else { "<" }, hi),
            (Some((lo, li)), Some((hi, hi_inc))) => format!(
                "{}{} v {} {}",
                if *li == Inclusive::Yes { ">=" } else { ">" },
                lo,
                if *hi_inc == Inclusive::Yes { "<=" } else { "<" },
                hi
            ),
        }
    }
}

/// 同版本时开区间更紧；否则按版本比较。
fn tighter_lower(a: &Version, b: &Version) -> bool {
    a > b
}
fn tighter_upper(a: &Version, b: &Version) -> bool {
    a < b
}

/// 解析版本区间字符串。
///
/// - `*` / 空 → 通配
/// - `^x.y.z` → caret：锁定最左非零位的 maj/minor（Cargo 语义）
/// - `~x.y.z` → tilde：锁定 minor
/// - `=x.y.z` 或 `x.y.z` → 精确
/// - 复合 `>=a, <b` 暂不支持（Fail-Fast 明确报错，避免半吊子语法）
pub fn parse_requirement(s: &str) -> Result<Constraint> {
    let s = s.trim();
    if s.is_empty() || s == "*" {
        return Ok(Constraint { min: None, max: None });
    }
    if s.contains(',') {
        return Err(bad(s, "暂不支持复合区间（如 '>=1.0, <2.0'）；请用 ^ / ~ / 精确版本"));
    }
    if let Some(rest) = s.strip_prefix('^') {
        let v = Version::parse(rest)?;
        let upper = caret_upper(&v);
        return Ok(Constraint {
            min: Some((v, Inclusive::Yes)),
            max: Some((upper, Inclusive::No)),
        });
    }
    if let Some(rest) = s.strip_prefix('~') {
        let v = Version::parse(rest)?;
        let upper = Version { major: v.major, minor: v.minor + 1, patch: 0, pre: String::new() };
        return Ok(Constraint {
            min: Some((v, Inclusive::Yes)),
            max: Some((upper, Inclusive::No)),
        });
    }
    let exact = s.strip_prefix('=').unwrap_or(s);
    let v = Version::parse(exact)?;
    Ok(Constraint {
        min: Some((v.clone(), Inclusive::Yes)),
        max: Some((v, Inclusive::Yes)),
    })
}

/// caret 上界：最左非零段 +1，其后归零。
/// ^1.2.3 → 2.0.0；^0.2.3 → 0.3.0；^0.0.3 → 0.0.4；^0.0.0 → 0.0.1。
fn caret_upper(v: &Version) -> Version {
    if v.major > 0 {
        Version { major: v.major + 1, minor: 0, patch: 0, pre: String::new() }
    } else if v.minor > 0 {
        Version { major: 0, minor: v.minor + 1, patch: 0, pre: String::new() }
    } else {
        Version { major: 0, minor: 0, patch: v.patch + 1, pre: String::new() }
    }
}

fn bad(s: &str, why: &str) -> DialcError {
    DialcError::new(format!("非法版本约束 '{}'：{}", s, why), 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_order() {
        assert!(Version::parse("1.2.3").is_ok());
        assert!(Version::parse("1.2").is_err());
        assert!(Version::parse("1.2.x").is_err());
        assert!(Version::parse("1.2.3-alpha.1").is_ok());
        let a = Version::parse("1.2.3-alpha").unwrap();
        let b = Version::parse("1.2.3").unwrap();
        assert!(a < b, "预发布 < 正式版");
        assert!(Version::parse("1.2.3-alpha.2").unwrap() > a);
    }

    #[test]
    fn caret_matches() {
        let c = parse_requirement("^1.2.3").unwrap();
        assert!(c.matches(&Version::parse("1.2.3").unwrap()));
        assert!(c.matches(&Version::parse("1.9.0").unwrap()));
        assert!(!c.matches(&Version::parse("2.0.0").unwrap()));
        assert!(!c.matches(&Version::parse("1.2.2").unwrap()));
        let c0 = parse_requirement("^0.2.3").unwrap();
        assert!(c0.matches(&Version::parse("0.2.9").unwrap()));
        assert!(!c0.matches(&Version::parse("0.3.0").unwrap()));
    }

    #[test]
    fn tilde_and_exact() {
        let c = parse_requirement("~1.2.3").unwrap();
        assert!(c.matches(&Version::parse("1.2.9").unwrap()));
        assert!(!c.matches(&Version::parse("1.3.0").unwrap()));
        let e = parse_requirement("=1.2.3").unwrap();
        assert!(e.matches(&Version::parse("1.2.3").unwrap()));
        assert!(!e.matches(&Version::parse("1.2.4").unwrap()));
        let w = parse_requirement("*").unwrap();
        assert!(w.matches(&Version::parse("99.0.0").unwrap()));
    }

    #[test]
    fn intersect_conflict() {
        let a = parse_requirement("^1.0.0").unwrap();
        let b = parse_requirement("^1.5.0").unwrap();
        let inter = a.intersect(&b).unwrap();
        assert!(inter.matches(&Version::parse("1.6.0").unwrap()));
        assert!(!inter.matches(&Version::parse("1.4.0").unwrap()));
        // ^1.x 与 ^2.x 无交集 → 冲突
        let c = parse_requirement("^2.0.0").unwrap();
        assert!(a.intersect(&c).is_none());
    }
}
