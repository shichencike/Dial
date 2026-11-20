//! 依赖求解器与 lockfile（P4.2）。
//!
//! 设计（见 `rationale/dialpkg-roadmap.md` §4 P4.2）：
//! - 从项目 `dial.toml [dependencies]` 出发，查询静态索引，选满足区间的**最高版本**；
//! - 递归解析传递依赖；同一包收到多条约束时求**交集**，交集为空即冲突（Fail-Fast，
//!   打印每条约束的来源路径，给出可读冲突路径）；
//! - 选版偏好：正式版 > 预发布；同级别按语义版本最高；
//! - 生成 `dial.lock` 锁定精确版本/URL/SHA256，保证可复现构建；
//! - **不回溯**：semver 区间单调，贪心选最高 + 交集判冲突即可收敛；冲突明确报错而非
//!   静默换版本——这与「Fail-Fast」「不静默覆盖」铁律一致。
//!
//! 与 ABI 的关系：这里按语义版本选「装哪个版本」；最终链接仍由 `abi` 字段把关
//! （README §25.3）。两者不冲突。

use crate::error::{DialcError, Result};
use crate::registry::{IndexEntry, IndexFile};
use crate::version::{self, Constraint};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

/// lockfile 格式版本。
pub const LOCK_VERSION: u32 = 1;

/// 一条约束及其来源路径（用于可读冲突诊断）。
#[derive(Debug, Clone)]
struct Pin {
    constraint: Constraint,
    /// 例如 "root" 或 "http@1.2.0 -> json"
    origin: String,
}

/// 求解结果：按包名排序的选中条目。
#[derive(Debug)]
pub struct Resolution {
    pub packages: Vec<IndexEntry>,
}

/// 从直接依赖出发求解完整依赖图。
///
/// `roots`：dial.toml [dependencies] 的 (包名, 区间字符串)。
pub fn resolve(idx: &IndexFile, roots: &BTreeMap<String, String>) -> Result<Resolution> {
    // name -> 累积约束（每条带来源路径）
    let mut pins: BTreeMap<String, Vec<Pin>> = BTreeMap::new();
    let mut queue: VecDeque<(String, Constraint, String)> = VecDeque::new();

    for (name, req) in roots {
        let c = parse_req(name, req)?;
        queue.push_back((name.clone(), c, "root".to_string()));
    }

    let mut selected: BTreeMap<String, IndexEntry> = BTreeMap::new();
    let mut processed_deps_of: BTreeSet<String> = BTreeSet::new();

    while let Some((name, constraint, origin)) = queue.pop_front() {
        pins.entry(name.clone()).or_default().push(Pin { constraint, origin });

        // 合并该包当前所有约束 → 交集
        let all = &pins[&name];
        let mut combined = all[0].constraint.clone();
        for p in &all[1..] {
            combined = match combined.intersect(&p.constraint) {
                Some(c) => c,
                None => return Err(conflict(&name, all)),
            };
        }

        // 选满足交集的最高版本（正式优先）
        let candidates: Vec<&IndexEntry> = idx
            .packages()
            .iter()
            .filter(|e| e.name == name && combined.matches(
                &version::Version::parse(&e.version).unwrap_or_else(|_| version::Version {
                    major: 0, minor: 0, patch: 0, pre: String::new(),
                })
            ))
            .collect();
        let best = pick_best(&candidates).ok_or_else(|| {
            let mut msg = format!("依赖 '{}' 无满足区间 {} 的版本。\n该区间来自：\n", name, combined.describe());
            for p in &pins[&name] {
                msg.push_str(&format!("  - {}（约束 {}）\n", p.origin, p.constraint.describe()));
            }
            msg.push_str("索引中可用版本：");
            let mut vers: Vec<String> = idx.packages().iter()
                .filter(|e| e.name == name).map(|e| e.version.clone()).collect();
            vers.sort();
            vers.dedup();
            msg.push_str(&if vers.is_empty() { "（索引中不存在该包）".to_string() } else { vers.join(", ") });
            DialcError::new(msg, 0, 0)
        })?;

        // 若之前选过不同版本，检查是否仍满足所有约束（贪心下新约束只会更严，
        // 若旧版本不满足则需换到 best；记录变更以便其下游约束重算）。
        let changed = match selected.get(&name) {
            Some(old) => old.version != best.version,
            None => true,
        };
        selected.insert(name.clone(), best.clone());

        // 传播该版本的传递依赖（每个版本只处理一次，避免环导致死循环）
        let dep_key = format!("{}@{}", name, best.version);
        if changed || !processed_deps_of.contains(&dep_key) {
            processed_deps_of.insert(dep_key.clone());
            for (dep_name, req) in &best.deps {
                let c = parse_req(dep_name, req)?;
                queue.push_back((dep_name.clone(), c, format!("{} -> {}", dep_key, dep_name)));
            }
        }
    }

    let mut packages: Vec<IndexEntry> = selected.into_values().collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Resolution { packages })
}

fn parse_req(name: &str, req: &str) -> Result<Constraint> {
    version::parse_requirement(req).map_err(|e| {
        DialcError::new(format!("依赖 '{}' 的版本区间非法：{}", name, e.message), 0, 0)
    })
}

/// 选最高版本：先按「正式 > 预发布」分组，再按语义版本取最大。
fn pick_best<'a>(candidates: &[&'a IndexEntry]) -> Option<&'a IndexEntry> {
    candidates
        .iter()
        .copied()
        .max_by(|a, b| {
            let va = version::Version::parse(&a.version).ok();
            let vb = version::Version::parse(&b.version).ok();
            match (va, vb) {
                (Some(x), Some(y)) => {
                    // 正式版优先；同为正式/同为预发布时按版本高低
                    x.is_release().cmp(&y.is_release()).then_with(|| x.cmp(&y))
                }
                _ => a.version.cmp(&b.version),
            }
        })
}

/// 构造可读冲突错误：列出所有相互冲突的约束及其来源路径。
fn conflict(name: &str, pins: &[Pin]) -> DialcError {
    let mut msg = format!("依赖 '{}' 的版本约束冲突（无法同时满足）：\n", name);
    for p in pins {
        msg.push_str(&format!("  - {} 要求 {}（来源: {}）\n",
            p.origin, p.constraint.describe(), p.origin));
    }
    msg.push_str("请统一版本区间，或升级其中某个依赖以消除冲突。");
    DialcError::new(msg, 0, 0)
}

/// 生成 `dial.lock` 文本（稳定排序、确定性）。
pub fn lock_text(res: &Resolution, root_name: &str) -> Result<String> {
    let packages: Vec<serde_json::Value> = res.packages.iter().map(|e| serde_json::json!({
        "name": e.name,
        "version": e.version,
        "abi": e.abi,
        "url": e.url,
        "sha256": e.sha256,
    })).collect();
    let obj = serde_json::json!({
        "lock_version": LOCK_VERSION,
        "root": root_name,
        "packages": packages,
    });
    Ok(serde_json::to_string_pretty(&obj)
        .map_err(|e| DialcError::new(format!("序列化 dial.lock 失败: {}", e), 0, 0))? + "\n")
}

/// 解析已有 `dial.lock`，返回 (root, 按包名的精确版本映射)。校验格式与哈希形态。
pub fn parse_lock(text: &str) -> Result<(String, BTreeMap<String, LockEntry>)> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| DialcError::new(format!("dial.lock 不是合法 JSON: {}", e), 0, 0))?;
    let obj = v.as_object().ok_or_else(|| DialcError::new("dial.lock 根节点必须是对象", 0, 0))?;
    let lv = obj.get("lock_version").and_then(|x| x.as_u64())
        .ok_or_else(|| DialcError::new("dial.lock 缺少 lock_version", 0, 0))?;
    if lv != LOCK_VERSION as u64 {
        return Err(DialcError::new(
            format!("dial.lock 版本 {} 不受支持（本编译器只认 {}）", lv, LOCK_VERSION), 0, 0));
    }
    let root = obj.get("root").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let arr = obj.get("packages").and_then(|x| x.as_array())
        .ok_or_else(|| DialcError::new("dial.lock 缺少 packages 数组", 0, 0))?;
    let mut map = BTreeMap::new();
    for pv in arr {
        let p = pv.as_object().ok_or_else(|| DialcError::new("dial.lock 包条目不是对象", 0, 0))?;
        let get = |k: &str| p.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = get("name");
        let version = get("version");
        if name.is_empty() || version.is_empty() {
            return Err(DialcError::new("dial.lock 包条目 name/version 不得为空", 0, 0));
        }
        version::Version::parse(&version).map_err(|e| DialcError::new(
            format!("dial.lock 中 '{}' 的版本非法：{}", name, e.message), 0, 0))?;
        let abi = p.get("abi").and_then(|x| x.as_u64())
            .ok_or_else(|| DialcError::new(format!("dial.lock 中 '{}' 缺少 abi", name), 0, 0))?;
        let sha256 = get("sha256");
        if !sha256.is_empty() && !is_sha256(&sha256) {
            return Err(DialcError::new(
                format!("dial.lock 中 '{}' 的 sha256 非法", name), 0, 0));
        }
        map.insert(name.clone(), LockEntry {
            name, version, abi: abi as u32, url: get("url"), sha256,
        });
    }
    Ok((root, map))
}

/// lockfile 中的一个精确锁定条目。
#[derive(Debug, Clone)]
pub struct LockEntry {
    #[allow(dead_code)] // map 的键即包名，此字段仅为 JSON 忠实往返保留
    pub name: String,
    pub version: String,
    pub abi: u32,
    pub url: String,
    pub sha256: String,
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 校验 lock 是否与当前求解结果一致（--locked 用）：包名、版本、URL、sha256 必须逐项相同。
pub fn lock_matches(res: &Resolution, lock: &BTreeMap<String, LockEntry>) -> Result<()> {
    if res.packages.len() != lock.len() {
        return Err(DialcError::new(
            format!("dial.lock 与依赖解析结果不一致：lock 有 {} 包，解析得 {} 包。\n\
                     请重新运行 `dialc resolve` 更新 lockfile。",
                lock.len(), res.packages.len()), 0, 0));
    }
    for e in &res.packages {
        match lock.get(&e.name) {
            None => return Err(DialcError::new(
                format!("dial.lock 缺少包 '{}'，请重新解析", e.name), 0, 0)),
            Some(l) => {
                if l.version != e.version || l.url != e.url || l.sha256 != e.sha256 || l.abi != e.abi {
                    return Err(DialcError::new(
                        format!("dial.lock 中 '{}' 的锁定值（v{}, abi {}）与解析结果（v{}, abi {}）不符。\n\
                                 请重新运行 `dialc resolve` 更新 lockfile。",
                            e.name, l.version, l.abi, e.version, e.abi), 0, 0));
                }
            }
        }
    }
    Ok(())
}

/// 写 lockfile（原子写：临时文件 + rename）。
pub fn write_lock(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("lock.tmp");
    fs::write(&tmp, text).map_err(|e| DialcError::new(
        format!("写 dial.lock 失败 {}: {}", tmp.display(), e), 0, 0))?;
    fs::rename(&tmp, path).map_err(|e| DialcError::new(
        format!("dial.lock 落盘失败 {}: {}", path.display(), e), 0, 0))?;
    Ok(())
}

/// 求解结果的 JSON（CLI --json 用）。
pub fn resolution_to_json(res: &Resolution) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = res.packages.iter().map(|e| serde_json::json!({
        "name": e.name, "version": e.version, "abi": e.abi,
        "url": e.url, "sha256": e.sha256, "deps": e.deps,
    })).collect();
    serde_json::json!({ "packages": arr, "count": arr.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    /// 从内联索引 JSON 构造 IndexFile（走真实 parse 路径）。
    fn idx(json: &str) -> IndexFile {
        // 借用 load 的 parse 逻辑：写入临时缓存不可行（需 HOME），
        // 故直接在 registry 暴露的 parse 上来做——见下方 pub(crate) parse_for_test。
        registry::parse_for_test(json).unwrap()
    }

    fn roots(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    const INDEX: &str = r#"{
      "schema": 2, "updated": "test",
      "packages": [
        {"name":"json","version":"2.0.0","abi":6,"url":"u1","sha256":"","deps":{"codec":"^1.0.0"}},
        {"name":"json","version":"2.1.0","abi":6,"url":"u2","sha256":"","deps":{"codec":"^1.1.0"}},
        {"name":"json","version":"3.0.0","abi":7,"url":"u3","sha256":"","deps":{}},
        {"name":"codec","version":"1.0.0","abi":6,"url":"u4","sha256":"","deps":{}},
        {"name":"codec","version":"1.2.0","abi":6,"url":"u5","sha256":"","deps":{}},
        {"name":"http","version":"1.0.0","abi":6,"url":"u6","sha256":"","deps":{"json":"^3.0.0"}}
      ]
    }"#;

    #[test]
    fn picks_highest_satisfying_and_transitive() {
        let i = idx(INDEX);
        let r = resolve(&i, &roots(&[("json", "^2.0.0")])).unwrap();
        // ^2.0.0 选 2.1.0（不跨大版本到 3.0.0），并传递拉入 codec ^1.1.0 → 1.2.0
        let m: BTreeMap<_, _> = r.packages.iter().map(|e| (e.name.as_str(), e.version.as_str())).collect();
        assert_eq!(m.get("json"), Some(&"2.1.0"));
        assert_eq!(m.get("codec"), Some(&"1.2.0"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn conflict_is_fail_fast() {
        let i = idx(INDEX);
        // 直接要 ^2.0.0 的 json；http 要 ^3.0.0 的 json——交集空，冲突
        let r = resolve(&i, &roots(&[("json", "^2.0.0"), ("http", "^1.0.0")]));
        assert!(r.is_err());
        let msg = r.err().unwrap().message;
        assert!(msg.contains("冲突") || msg.contains("无满足"), "msg={}", msg);
    }

    #[test]
    fn missing_package_fail_fast() {
        let i = idx(INDEX);
        let r = resolve(&i, &roots(&[("nonexistent", "^1.0.0")]));
        assert!(r.is_err());
        assert!(r.err().unwrap().message.contains("无满足"));
    }

    #[test]
    fn lock_roundtrip_and_mismatch() {
        let i = idx(INDEX);
        let r = resolve(&i, &roots(&[("json", "~2.0.0"), ("codec", "^1.0.0")])).unwrap();
        let text = lock_text(&r, "testroot").unwrap();
        let (root, lock) = parse_lock(&text).unwrap();
        assert_eq!(root, "testroot");
        lock_matches(&r, &lock).unwrap();
        // 篡改一个版本应被 --locked 校验发现
        let mut bad = lock.clone();
        bad.get_mut("codec").unwrap().version = "9.9.9".to_string();
        assert!(lock_matches(&r, &bad).is_err());
    }

    #[test]
    fn cycle_terminates() {
        // a 依赖 b，b 依赖 a：求解应终止（processed_deps_of 去重），不无限循环
        let cyclic = r#"{
          "schema": 2, "packages": [
            {"name":"a","version":"1.0.0","abi":6,"url":"ua","sha256":"","deps":{"b":"^1.0.0"}},
            {"name":"b","version":"1.0.0","abi":6,"url":"ub","sha256":"","deps":{"a":"^1.0.0"}}
          ]
        }"#;
        let i = idx(cyclic);
        let r = resolve(&i, &roots(&[("a", "^1.0.0")])).unwrap();
        assert_eq!(r.packages.len(), 2);
    }
}
