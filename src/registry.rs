//! 静态索引与搜索（P4.1）。
//!
//! 设计铁律（见 `rationale/dialpkg-roadmap.md` §4 P4.1）：
//! - **无中心服务**：索引是一个静态 JSON 文件，托管在 GitHub Releases（或任意静态 URL）。
//!   编译器只下载这一个文件，不做账号、不做服务端查询。
//! - **沿用 SHA256 信任链**：索引文件本身可钉 SHA256（`--index-sha256`），不符即拒；
//!   索引内每个包登记 `sha256`，`dialc install <url> --sha256` 可据此钉死下载物。
//! - **零脚本、零新依赖**：下载复用 `package::download_to`（https 强制 + curl 安全参数），
//!   JSON 用已有 `serde_json`，哈希用已有 sha2。
//! - **Fail-Fast**：索引结构非法、哈希不符、缓存缺失（offline）都明确报错，绝不静默降级。
//! - **本地缓存**：索引落到 `~/.dial/cache/index.json`，`--offline` 只读缓存不联网。

use crate::error::{DialcError, Result};
use crate::package;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 默认静态索引 URL（GitHub Releases 托管的单文件，无中心服务）。
/// 可用 `--index <url>` 或环境变量 `DIAL_INDEX_URL` 覆盖（本地镜像 / 自测）。
pub const DEFAULT_INDEX_URL: &str =
    "https://github.com/dial-lang/dial-index/releases/latest/download/index.json";

/// 索引文件格式版本。不匹配即 Fail-Fast（防止新格式被旧编译器误读）。
const SCHEMA_VERSION: u32 = 1;

/// 索引里的单个包条目。
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    /// 该包要求的 STDLIB ABI 整数版本。
    pub abi: u32,
    pub license: String,
    pub desc: String,
    /// 支持的目标 triple（manifest 语义）。
    pub targets: Vec<String>,
    /// `.dialpkg` 下载地址（https）。
    pub url: String,
    /// 整包 SHA256（可选；有则 `install --sha256` 可钉死来源）。
    pub sha256: String,
}

/// 静态索引根节点。
#[derive(Debug)]
pub struct IndexFile {
    // schema 在 parse() 中用于版本门禁；updated 供未来 info 展示索引时效。
    #[allow(dead_code)]
    schema: u64,
    #[allow(dead_code)]
    updated: String,
    packages: Vec<IndexEntry>,
}

/// 索引读取来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 允许联网下载（失败回退缓存）。
    Online,
    /// 只读本地缓存，绝不联网。
    Offline,
}

/// 加载索引：按 `source` 决定联网或只读缓存，返回（索引，来源说明，缓存路径）。
///
/// - Online：下载到临时文件 → 可选钉哈希 → 解析校验 → 原子落缓存 → 返回。
///   下载失败时若已有缓存，打印提示并回退缓存（不静默：stderr 明确说明）。
/// - Offline：只读缓存，缺失则 Fail-Fast 给指引。
pub fn load(
    index_url: &str,
    expected_sha256: Option<&str>,
    source: Source,
) -> Result<(IndexFile, String, PathBuf)> {
    let cache = cache_path()?;
    match source {
        Source::Offline => {
            let text = fs::read_to_string(&cache).map_err(|e| {
                DialcError::new(
                    format!(
                        "--offline：本地索引缓存不存在或不可读：{}（{}）。\n\
                         请先联网执行一次 `dialc search <名>` 生成缓存，或去掉 --offline。",
                        cache.display(),
                        e
                    ),
                    0,
                    0,
                )
            })?;
            // 即便 offline 也校验钉的哈希：缓存被改过且用户钉了哈希时必须拒绝。
            check_pinned(&text, expected_sha256)?;
            let idx = parse(&text)?;
            Ok((idx, format!("本地缓存（offline）{}", cache.display()), cache))
        }
        Source::Online => {
            let (text, origin) = match fetch(index_url, expected_sha256) {
                Ok(t) => (t, index_url.to_string()),
                Err(FetchErr::HashMismatch(e)) => {
                    // 用户显式钉了哈希却不符——这是主动安全信号，绝不回退缓存，
                    // 否则「钉死来源」的意图被静默绕过（Fail-Fast 铁律）。
                    return Err(e);
                }
                Err(FetchErr::Network(e)) => {
                    if cache.is_file() {
                        let cached = fs::read_to_string(&cache).map_err(|ce| {
                            DialcError::new(
                                format!("读取索引缓存失败: {}（{}）", cache.display(), ce),
                                0,
                                0,
                            )
                        })?;
                        // 回退缓存也要满足用户钉的哈希，不能拿旧缓存糊弄。
                        check_pinned(&cached, expected_sha256)?;
                        eprintln!(
                            "⚠️  索引下载失败（{}）；回退本地缓存 {}。",
                            e,
                            cache.display()
                        );
                        (cached, format!("本地缓存（下载失败回退）{}", cache.display()))
                    } else {
                        return Err(e);
                    }
                }
            };
            // 先解析校验再落缓存：绝把非法索引写进缓存。
            let idx = parse(&text)?;
            write_cache_atomic(&cache, &text)?;
            Ok((idx, origin, cache))
        }
    }
}

/// 校验文本内容（按字节）的 SHA256 是否等于用户钉的值。
fn check_pinned(text: &str, expected: Option<&str>) -> Result<()> {
    if let Some(want) = expected {
        use sha2::{Digest, Sha256};
        let want = want.trim().to_ascii_lowercase();
        let mut h = Sha256::new();
        h.update(text.as_bytes());
        let got = format!("{:x}", h.finalize());
        if got != want {
            return Err(DialcError::new(
                format!(
                    "本地索引缓存 SHA256 不符：期望 {}，实际 {}。\n\
                     缓存可能已被篡改，绝不使用。请删除 {} 后重新联网获取。",
                    want, got,
                    cache_hint()
                ),
                0,
                0,
            ));
        }
    }
    Ok(())
}

fn cache_hint() -> String {
    cache_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.dial/cache/index.json".to_string())
}

/// 下载阶段的两类失败：哈希不符（安全事件，不回退）与网络/其它（可回退缓存）。
enum FetchErr {
    HashMismatch(DialcError),
    Network(DialcError),
}

/// 下载索引并做钉哈希校验，返回原始文本。
fn fetch(url: &str, expected_sha256: Option<&str>) -> std::result::Result<String, FetchErr> {
    let fname = match safe_cache_name(url) {
        Ok(v) => v,
        Err(e) => return Err(FetchErr::Network(e)),
    };
    let tmp = match cache_path() {
        Ok(p) => p.parent().unwrap_or(Path::new("/tmp"))
            .join(format!(".index-{}-{}.part", std::process::id(), fname)),
        Err(e) => return Err(FetchErr::Network(e)),
    };

    let res = (|| -> std::result::Result<String, FetchErr> {
        package::download_to(url, &tmp).map_err(FetchErr::Network)?;
        if let Some(want) = expected_sha256 {
            let want = want.trim().to_ascii_lowercase();
            let got = package::sha256_file(&tmp).map_err(FetchErr::Network)?;
            if got != want {
                return Err(FetchErr::HashMismatch(DialcError::new(
                    format!(
                        "索引 SHA256 不符：期望 {}，实际 {}。\n\
                         下载的索引已删除，且不会回退缓存（你已钉死来源）。\n\
                         请核对 --index-sha256 的值（来自发行页），\n\
                         或去掉该参数仅依赖 HTTPS 传输保护。",
                        want, got
                    ),
                    0,
                    0,
                )));
            }
        }
        let text = fs::read_to_string(&tmp).map_err(|e| {
            FetchErr::Network(DialcError::new(
                format!("索引不是合法 UTF-8 文本: {}（{}）", url, e), 0, 0,
            ))
        })?;
        Ok(text)
    })();
    let _ = fs::remove_file(&tmp); // 成功失败都清临时文件
    res
}

/// 解析 + 结构校验索引文本（手动解析 serde_json::Value，与 parse_manifest 同风格，零新依赖）。
fn parse(text: &str) -> Result<IndexFile> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        DialcError::new(format!("索引 JSON 解析失败: {}。索引文件可能损坏或被篡改。", e), 0, 0)
    })?;
    let obj = v.as_object().ok_or_else(|| {
        DialcError::new("索引根节点必须是 JSON 对象".to_string(), 0, 0)
    })?;

    // schema：缺失/非法一律报错（Fail-Fast）。
    let schema = obj
        .get("schema")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| DialcError::new("索引缺少整数字段 'schema'".to_string(), 0, 0))?;
    if schema != SCHEMA_VERSION as u64 {
        return Err(DialcError::new(
            format!(
                "索引 schema={} 不受支持（本编译器只认 schema={}）。\n\
                 请升级编译器，或改用匹配的索引文件。",
                schema, SCHEMA_VERSION
            ),
            0,
            0,
        ));
    }
    let updated = obj
        .get("updated")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let arr = obj
        .get("packages")
        .and_then(|x| x.as_array())
        .ok_or_else(|| DialcError::new("索引缺少数组字段 'packages'".to_string(), 0, 0))?;

    let get_str = |o: &serde_json::Map<String, serde_json::Value>, k: &str| -> String {
        o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let mut packages = Vec::with_capacity(arr.len());
    // 同一包可有多个版本（info 展示版本列表），故唯一键是 (name, version)。
    let mut seen: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for (i, pv) in arr.iter().enumerate() {
        let p = pv.as_object().ok_or_else(|| {
            DialcError::new(format!("索引第 {} 个包不是 JSON 对象", i + 1), 0, 0)
        })?;
        let name = get_str(p, "name");
        if name.trim().is_empty() {
            return Err(DialcError::new(format!("索引第 {} 个包 name 为空", i + 1), 0, 0));
        }
        let version = get_str(p, "version");
        if version.trim().is_empty() {
            return Err(DialcError::new(format!("包 '{}' 的 version 为空", name), 0, 0));
        }
        if !seen.insert((name.clone(), version.clone())) {
            return Err(DialcError::new(
                format!("索引中 (包名,版本) 重复: '{}' v'{}'", name, version),
                0,
                0,
            ));
        }
        let abi = p
            .get("abi")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| DialcError::new(format!("包 '{}' 缺少整数字段 'abi'", name), 0, 0))?;
        let url = get_str(p, "url");
        if url.trim().is_empty() {
            return Err(DialcError::new(format!("包 '{}' 的 url 为空", name), 0, 0));
        }
        let sha256 = get_str(p, "sha256");
        if !sha256.is_empty() && !is_sha256(&sha256) {
            return Err(DialcError::new(
                format!("包 '{}' 的 sha256 不是有效的 64 位十六进制", name),
                0,
                0,
            ));
        }
        let targets = p
            .get("targets")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        packages.push(IndexEntry {
            name,
            version,
            abi: abi as u32,
            license: get_str(p, "license"),
            desc: get_str(p, "desc"),
            targets,
            url,
            sha256,
        });
    }
    Ok(IndexFile { schema, updated, packages })
}

/// 是否形如 SHA256（64 位 hex）。
fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 搜索：子串匹配包名或描述（大小写不敏感），**每个包只返回最新版本**，
/// 按包名排序。多版本用 info 查看。（版本比较为字符串序，够用于展示最新版本。）
pub fn search<'a>(idx: &'a IndexFile, query: &str) -> Vec<&'a IndexEntry> {
    let q = query.trim().to_ascii_lowercase();
    // name -> 当前最高版本条目
    let mut latest: BTreeMap<&str, &IndexEntry> = BTreeMap::new();
    for p in &idx.packages {
        if !(q.is_empty()
            || p.name.to_ascii_lowercase().contains(&q)
            || p.desc.to_ascii_lowercase().contains(&q))
        {
            continue;
        }
        let replace = latest
            .get(p.name.as_str())
            .map(|cur| p.version.as_str() > cur.version.as_str())
            .unwrap_or(true);
        if replace {
            latest.insert(p.name.as_str(), p);
        }
    }
    latest.into_values().collect()
}

/// 查询某包的全部版本（精确包名匹配），按版本号降序（字符串排序，够用于展示）。
pub fn info<'a>(idx: &'a IndexFile, name: &str) -> Option<Vec<&'a IndexEntry>> {
    let mut hits: Vec<_> = idx.packages.iter().filter(|p| p.name == name).collect();
    if hits.is_empty() {
        return None;
    }
    hits.sort_by(|a, b| b.version.cmp(&a.version));
    Some(hits)
}

/// 供 --json 输出：把命中项转成稳定字段的 JSON 值。
pub fn entry_to_json(e: &IndexEntry) -> serde_json::Value {
    serde_json::json!({
        "name": e.name,
        "version": e.version,
        "abi": e.abi,
        "license": e.license,
        "desc": e.desc,
        "targets": e.targets,
        "url": e.url,
        "sha256": e.sha256,
    })
}

/// 索引缓存路径：`~/.dial/cache/index.json`。
fn cache_path() -> Result<PathBuf> {
    let d = package::dirs_home()?.join(".dial").join("cache");
    fs::create_dir_all(&d).map_err(|e| {
        DialcError::new(format!("创建索引缓存目录失败 {}: {}", d.display(), e), 0, 0)
    })?;
    Ok(d.join("index.json"))
}

/// 由 URL 派生一个安全的文件名片段（用于临时文件名）。
fn safe_cache_name(url: &str) -> Result<String> {
    let tail = url.split(['?', '#']).next().unwrap_or(url);
    let name = tail.rsplit('/').next().unwrap_or("index.json");
    let stem = name.strip_suffix(".json").unwrap_or(name);
    if stem.is_empty()
        || !stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(DialcError::new(
            format!("无法从索引 URL 派生安全文件名: {}", url),
            0,
            0,
        ));
    }
    Ok(stem.to_string())
}

/// 原子写缓存：先写临时文件再 rename，避免中断留下半截索引。
fn write_cache_atomic(cache: &Path, text: &str) -> Result<()> {
    let tmp = cache.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| {
        DialcError::new(format!("写索引缓存失败 {}: {}", tmp.display(), e), 0, 0)
    })?;
    fs::rename(&tmp, cache).map_err(|e| {
        DialcError::new(format!("索引缓存落盘失败 {}: {}", cache.display(), e), 0, 0)
    })?;
    Ok(())
}

/// 把索引按包名聚合（供 info 展示"版本列表"时使用），返回 name -> 版本列表。
#[allow(dead_code)]
pub fn versions_by_name(idx: &IndexFile) -> BTreeMap<&str, Vec<&str>> {
    let mut m: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for p in &idx.packages {
        m.entry(p.name.as_str()).or_default().push(p.version.as_str());
    }
    m
}
