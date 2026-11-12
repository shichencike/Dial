//! dialc 工程工具链（v1.04 增补）：项目清单 `dial.toml`、`.dialpkg` 打包、项目骨架与包管理。
//!
//! ## 为什么需要这个模块
//!
//! 在 v1.04 之前，「包」只有**消费端**：`dialc install` 能装、`import` 能用，但仓库里
//! 没有任何一条**通用**的产包路径——`tools/make_stdlib_pkg.py` 写死了包名 `stdlib`，
//! `tools/make_sample_pkg.sh` 写死了示例包 `demo`。普通作者想把自己的 C 库发布出去，
//! 只能复制这两个脚本再改，或者手工拼 zip + manifest + SHA256（极易拼错，且错包会
//! 安装期才暴露）。`REPORT_v1.04_gaps.md` 的 A2/A6 与用户实测的「并没有打包 dialpkg」
//! 指的都是这个洞。
//!
//! 这里把产包能力做成**编译器一等公民**：`dialc pack <目录>` 读目录里的 `dial.toml`，
//! 校验接口与静态库，生成 `manifest.txt`（字段与 `package::parse_manifest` 完全对齐）
//! 与逐文件 SHA256，压成 `.dialpkg`。产物直接能被 `dialc install` 吃下——
//! 打包与安装共用同一套校验规则，不存在「自己打的包自己装不上」。
//!
//! ## `dial.toml` 格式（刻意只用 TOML 的一个小子集）
//!
//! ```toml
//! name = "mylib"
//! version = "1.0.0"
//! abi = 6
//! iface = "mylib.diali"
//! license = "MIT OR Apache-2.0"
//!
//! [targets]
//! x86_64-linux-gnu = "lib/x86_64-linux-gnu/libmylib.a"
//!
//! [src]
//! mylib_oo = "src/mylib_oo.dial"
//! ```
//!
//! 只支持 `key = "字符串"`、`key = 整数` 与 `[targets]` / `[src]` 两个段，其余 TOML 特性
//! （数组、内联表、多行字符串、点分键）一律**显式报错**而不是悄悄忽略——
//! 悄悄忽略配置项是配置类工具最危险的失败模式。路径既是磁盘上相对项目根的源路径，
//! 也是 `.dialpkg` 内的相对路径，二者一致，不需要额外映射表。
//!
//! 没有引入新的 TOML 依赖：清单是**本地作者文件**、字段固定且极少，解析面比
//! 「读外部不可信输入」小得多；相反，多一个依赖就多一份供应链与交叉编译负担
//! （本项目要为三个 target 静态构建）。`package.rs` 里读外部 `manifest.txt` 仍用
//! `serde_json`，二者边界清晰。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

use crate::package;

/// 一份解析并校验过的项目清单。
#[derive(Debug, Clone)]
pub struct PkgSpec {
    pub name: String,
    pub version: String,
    pub abi: u64,
    pub iface: String,
    pub license: String,
    /// Target Triple -> 包内（同时也是磁盘上）的静态库相对路径。
    pub targets: BTreeMap<String, String>,
    /// Dial 源码模块名 -> 包内（同时也是磁盘上）的 `.dial` 相对路径（v1.10 增补）。
    ///
    /// 键是 `import "模块名";` 用的**裸名**，值一律位于 `src/` 目录下。这些文件
    /// 与 `.diali` 不同：它们是**真正的 Dial 源码**，会由前端按本地源码模块的方式
    /// 合并编译（见 `main.rs::collect_sources` 的三级优先）。`.dialpkg` 因此从
    /// 「机器码 + 签名」扩展为「机器码 + 签名 + 随包 Dial 源码模块」。
    pub src: BTreeMap<String, String>,
}

/// 去掉一行中字符串字面量之外的 `#` 注释。
fn strip_comment(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
        } else if c == b'#' {
            return &raw[..i];
        }
        i += 1;
    }
    raw
}

/// 解析一个带双引号的字符串字面量（支持 `\"` / `\\` / `\n` / `\t`）。
fn parse_string(v: &str, line: usize) -> Result<String, String> {
    let t = v.trim();
    if !t.starts_with('"') || !t.ends_with('"') || t.len() < 2 {
        return Err(format!(
            "dial.toml 第 {} 行：值必须是双引号字符串，收到 '{}'",
            line, t
        ));
    }
    let inner = &t[1..t.len() - 1];
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => {
                return Err(format!(
                    "dial.toml 第 {} 行：不支持的转义 \\{}（清单只接受 \\\" \\\\ \\n \\t）",
                    line, other
                ))
            }
            None => {
                return Err(format!("dial.toml 第 {} 行：字符串以孤立的反斜杠结尾", line))
            }
        }
    }
    Ok(out)
}

/// 校验包名 / triple / 相对路径，拒绝一切能逃出项目目录的写法。
fn check_component(what: &str, s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err(format!("dial.toml：{} 不能为空", what));
    }
    if s.contains('\\') || s.contains("..") {
        return Err(format!(
            "dial.toml：{} '{}' 含反斜杠或 '..'，不允许（防止路径穿越）",
            what, s
        ));
    }
    if s.starts_with('/') || (s.len() >= 2 && s.as_bytes()[1] == b':') {
        return Err(format!(
            "dial.toml：{} '{}' 必须是相对路径，不允许绝对路径或盘符",
            what, s
        ));
    }
    Ok(())
}

/// 解析 `dial.toml` 的 TOML 子集。
pub fn parse_spec(text: &str) -> Result<PkgSpec, String> {
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut abi: Option<u64> = None;
    let mut iface: Option<String> = None;
    let mut license: Option<String> = None;
    let mut targets: BTreeMap<String, String> = BTreeMap::new();
    let mut src: BTreeMap<String, String> = BTreeMap::new();
    let mut section: Option<String> = None;
    let mut seen_targets = false;
    let mut seen_src = false;

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !line.ends_with(']') {
                return Err(format!("dial.toml 第 {} 行：段头缺少右方括号 ']'", line_no));
            }
            let sec = line[1..line.len() - 1].trim();
            if sec != "targets" && sec != "src" {
                return Err(format!(
                    "dial.toml 第 {} 行：不支持的段 '[{}]'（只支持 [targets] 与 [src]）",
                    line_no, sec
                ));
            }
            let seen = if sec == "targets" {
                &mut seen_targets
            } else {
                &mut seen_src
            };
            if *seen {
                return Err(format!("dial.toml 第 {} 行：[{}] 段重复", line_no, sec));
            }
            *seen = true;
            section = Some(sec.to_string());
            continue;
        }
        let eq = line.find('=').ok_or_else(|| {
            format!(
                "dial.toml 第 {} 行：不是 'key = value' 赋值，也不是段头：'{}'",
                line_no, line
            )
        })?;
        let key = line[..eq].trim();
        let val = line[eq + 1..].trim();
        if key.is_empty() || val.is_empty() {
            return Err(format!("dial.toml 第 {} 行：键或值为空", line_no));
        }
        if key.contains('.') || key.contains('[') || key.contains(']') {
            return Err(format!(
                "dial.toml 第 {} 行：不支持点分键 / 数组键 '{}'",
                line_no, key
            ));
        }

        if section.as_deref() == Some("targets") {
            // triple 是裸键（含 '-'），value 是库路径字符串。
            if targets.contains_key(key) {
                return Err(format!(
                    "dial.toml 第 {} 行：target triple '{}' 重复",
                    line_no, key
                ));
            }
            let lib = parse_string(val, line_no)?;
            check_component(&format!("targets['{}'].lib", key), &lib)?;
            targets.insert(key.to_string(), lib);
            continue;
        }

        if section.as_deref() == Some("src") {
            // 键是模块名（裸名，`import "模块名";` 用它），值是包内 / 磁盘上的
            // `.dial` 相对路径。两处约定都在这里收紧，包作者写错当场报错，
            // 而不是等安装 / 编译期才发现「模块找不到」。
            if src.contains_key(key) {
                return Err(format!(
                    "dial.toml 第 {} 行：[src] 模块名 '{}' 重复",
                    line_no, key
                ));
            }
            if key.contains('/') || key.contains('\\') || key.ends_with(".dial") {
                return Err(format!(
                    "dial.toml 第 {} 行：[src] 模块名 '{}' 必须是裸名（不含路径分隔符、不带 .dial 后缀）",
                    line_no, key
                ));
            }
            check_component(&format!("[src] 模块名 '{}'", key), key)?;
            let rel = parse_string(val, line_no)?;
            check_component(&format!("[src] '{}' 的路径", key), &rel)?;
            if !rel.starts_with("src/") {
                return Err(format!(
                    "dial.toml 第 {} 行：[src] '{}' 的路径 '{}' 必须位于 src/ 目录下（src 目录单独存放 Dial 源码）",
                    line_no, key, rel
                ));
            }
            if !rel.ends_with(".dial") {
                return Err(format!(
                    "dial.toml 第 {} 行：[src] '{}' 的路径 '{}' 必须以 .dial 结尾",
                    line_no, key, rel
                ));
            }
            src.insert(key.to_string(), rel);
            continue;
        }

        match key {
            "name" | "version" | "iface" | "license" => {
                let s = parse_string(val, line_no)?;
                let slot = match key {
                    "name" => &mut name,
                    "version" => &mut version,
                    "iface" => &mut iface,
                    _ => &mut license,
                };
                if slot.is_some() {
                    return Err(format!("dial.toml 第 {} 行：字段 '{}' 重复", line_no, key));
                }
                *slot = Some(s);
            }
            "abi" => {
                if abi.is_some() {
                    return Err(format!("dial.toml 第 {} 行：字段 'abi' 重复", line_no));
                }
                let n: u64 = val.parse().map_err(|_| {
                    format!(
                        "dial.toml 第 {} 行：abi 必须是非负整数，收到 '{}'",
                        line_no, val
                    )
                })?;
                abi = Some(n);
            }
            other => {
                return Err(format!(
                    "dial.toml 第 {} 行：未知字段 '{}'（支持 name/version/abi/iface/license 与 [targets] / [src] 段）",
                    line_no, other
                ))
            }
        }
    }

    let name = name.ok_or("dial.toml 缺少必填字段 'name'")?;
    let version = version.ok_or("dial.toml 缺少必填字段 'version'")?;
    let abi = abi.ok_or("dial.toml 缺少必填字段 'abi'")?;
    let iface = iface.ok_or("dial.toml 缺少必填字段 'iface'")?;
    let license = license.ok_or("dial.toml 缺少必填字段 'license'（§25.8 要求许可可见）")?;

    check_component("name", &name)?;
    if name.contains('/') {
        return Err(format!(
            "dial.toml：包名 '{}' 不能含路径分隔符（包名即缓存目录名）",
            name
        ));
    }
    if version.is_empty() || license.is_empty() {
        return Err("dial.toml：version / license 不能为空".to_string());
    }
    check_component("iface", &iface)?;
    if !iface.ends_with(".diali") {
        return Err(format!(
            "dial.toml：iface '{}' 必须以 .diali 结尾（接口签名文件）",
            iface
        ));
    }
    if targets.is_empty() {
        return Err("dial.toml：[targets] 段至少要登记一个平台的静态库".to_string());
    }

    Ok(PkgSpec {
        name,
        version,
        abi,
        iface,
        license,
        targets,
        src,
    })
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let mut f = fs::File::open(path)
        .map_err(|e| format!("无法读取 {}: {}", path.display(), e))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// 校验静态库：后缀 + ar 归档魔数 `!<arch>\n`（J 组铁律：包分发只认静态归档）。
fn verify_static_lib(path: &Path) -> Result<(), String> {
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(fname.ends_with(".a") || fname.ends_with(".lib")) {
        return Err(format!(
            "静态库 '{}' 必须以 .a 或 .lib 结尾（包分发只接受静态归档，J 组铁律）",
            path.display()
        ));
    }
    let mut f = fs::File::open(path)
        .map_err(|e| format!("无法打开静态库 {}: {}", path.display(), e))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).map_err(|_| {
        format!(
            "静态库 '{}' 内容不足 8 字节，不是合法 ar 归档",
            path.display()
        )
    })?;
    if &magic != b"!<arch>\n" {
        return Err(format!(
            "静态库 '{}' 缺少 ar 归档魔数 `!<arch>\\n`，不是真正的静态库",
            path.display()
        ));
    }
    Ok(())
}

/// `dialc pack <目录> [-o 输出.dialpkg]`：把项目清单描述的接口与静态库打成 `.dialpkg`。
///
/// 返回产出的包路径。打包完成后**就地自检**：用 `package::parse_manifest` 解析刚生成的
/// manifest、再用 `package::verify_archive` 逐文件复核 SHA256——确保产物一定过得了
/// `dialc install` 的两道校验，不把「打出来的包」留给安装期才发现问题。
/// 打包计划：入库文件 + manifest 文本 + 逐文件 SHA256 + 输出路径。
///
/// `pack` / `--dry-run` / `--json` 三条路径共用同一个计划，保证「预览看到的」
/// 和「真正打出来的」逐字节一致——预览与产物分叉是配置类工具最烦人的失败模式。
pub struct PackPlan {
    pub spec: PkgSpec,
    pub staged: Vec<(String, PathBuf)>,
    pub manifest_text: String,
    pub sha: Vec<(String, String)>,
    pub pkg_path: PathBuf,
    /// 磁盘上有、但清单未登记的文件：**绝不自动塞进包**（清单显式声明是硬规则），
    /// 只在预览里提示作者，避免「以为打进去了其实没有」。
    pub unlisted: Vec<String>,
}

/// 扫描项目目录，列出磁盘上有但清单未登记的 `.diali` / `.a` / `src/*.dial`。
fn discover_unlisted(project_dir: &Path, staged: &[(String, PathBuf)]) -> Vec<String> {
    let listed: HashSet<&str> = staged.iter().map(|(p, _)| p.as_str()).collect();
    let mut out: Vec<String> = Vec::new();
    let push = |rel: String, out: &mut Vec<String>| {
        if !listed.contains(rel.as_str()) {
            out.push(rel);
        }
    };
    // 项目根下的 .diali 接口文件
    if let Ok(rd) = fs::read_dir(project_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("diali") {
                if let Some(n) = p.file_name() {
                    push(n.to_string_lossy().to_string(), &mut out);
                }
            }
        }
    }
    // src/ 下的 Dial 源码模块
    let src_dir = project_dir.join("src");
    if let Ok(rd) = fs::read_dir(&src_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("dial") {
                if let Some(n) = p.file_name() {
                    push(format!("src/{}", n.to_string_lossy()), &mut out);
                }
            }
        }
    }
    // lib/<triple>/ 下的静态库
    let lib_dir = project_dir.join("lib");
    if let Ok(rd) = fs::read_dir(&lib_dir) {
        for triple in rd.flatten() {
            let tp = triple.path();
            if !tp.is_dir() {
                continue;
            }
            let tname = match tp.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };
            if let Ok(files) = fs::read_dir(&tp) {
                for e in files.flatten() {
                    let p = e.path();
                    if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("a") {
                        if let Some(n) = p.file_name() {
                            push(format!("lib/{}/{}", tname, n.to_string_lossy()), &mut out);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 生成打包计划（不写任何文件）。所有 Fail-Fast 校验都集中在这里。
pub fn plan(project_dir: &Path, out: Option<&Path>) -> Result<PackPlan, String> {
    let spec_path = project_dir.join("dial.toml");
    let text = fs::read_to_string(&spec_path).map_err(|e| {
        format!(
            "找不到或无法读取项目清单 {}: {}（可先用 `dialc new <目录>` 生成骨架，\n\
             或在已有项目里跑 `dialc pack --init <目录>` 自动扫描生成清单草稿）",
            spec_path.display(),
            e
        )
    })?;
    let spec = parse_spec(&text)?;

    let iface_src = project_dir.join(&spec.iface);
    if !iface_src.is_file() {
        return Err(format!(
            "接口文件不存在: {}（dial.toml 的 iface = \"{}\"）\n\
             提示：项目根下的 .diali 文件可用 `dialc pack --dry-run <目录>` 列出。",
            iface_src.display(),
            spec.iface
        ));
    }

    // 收集所有入库文件：(包内相对路径, 磁盘绝对路径)
    let mut staged: Vec<(String, PathBuf)> = Vec::new();
    staged.push((spec.iface.clone(), iface_src.clone()));
    for (triple, lib) in &spec.targets {
        let p = project_dir.join(lib);
        if !p.is_file() {
            return Err(format!(
                "平台 '{}' 的静态库不存在: {}（dial.toml [targets] 登记为 '{}'）",
                triple,
                p.display(),
                lib
            ));
        }
        verify_static_lib(&p)?;
        staged.push((lib.clone(), p));
    }
    for (module, rel) in &spec.src {
        let p = project_dir.join(rel);
        if !p.is_file() {
            return Err(format!(
                "[src] 模块 '{}' 的源码文件不存在: {}（dial.toml [src] 登记为 '{}'）",
                module,
                p.display(),
                rel
            ));
        }
        staged.push((rel.clone(), p));
    }
    // 包内路径必须唯一：zip 允许重名项，但那会让 install 解压时后者覆盖前者。
    let mut seen_paths: HashSet<&str> = HashSet::new();
    for (zip_path, _) in &staged {
        if !seen_paths.insert(zip_path.as_str()) {
            return Err(format!(
                "包内路径 '{}' 重复（iface / [targets] / [src] 不能指向同一个包内路径）",
                zip_path
            ));
        }
    }

    // manifest：字段与 package::parse_manifest 一一对应，一个都不能少。
    let mut targets_obj = serde_json::Map::new();
    let mut src_obj = serde_json::Map::new();
    let mut sha_obj = serde_json::Map::new();
    for (module, rel) in &spec.src {
        src_obj.insert(module.clone(), serde_json::Value::String(rel.clone()));
    }
    for (triple, lib) in &spec.targets {
        targets_obj.insert(triple.clone(), serde_json::json!({ "lib": lib }));
    }
    let mut sha: Vec<(String, String)> = Vec::new();
    for (zip_path, real_path) in &staged {
        let h = sha256_of(real_path)?;
        sha_obj.insert(zip_path.clone(), serde_json::Value::String(h.clone()));
        sha.push((zip_path.clone(), h));
    }
    let manifest = serde_json::json!({
        "name": spec.name,
        "version": spec.version,
        "abi": spec.abi,
        "iface": spec.iface,
        "license": spec.license,
        "targets": targets_obj,
        "src": src_obj,
        "sha256": sha_obj,
    });
    let manifest_text = serde_json::to_string_pretty(&manifest)
        .map_err(|e| format!("序列化 manifest.txt 失败: {}", e))?
        + "\n";

    let pkg_path: PathBuf = match out {
        Some(p) => p.to_path_buf(),
        None => project_dir.join(format!("{}.dialpkg", spec.name)),
    };

    Ok(PackPlan {
        spec,
        unlisted: discover_unlisted(project_dir, &staged),
        staged,
        manifest_text,
        sha,
        pkg_path,
    })
}

fn write_archive(p: &PackPlan) -> Result<(), String> {
    if let Some(parent) = p.pkg_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("创建输出目录 {} 失败: {}", parent.display(), e))?;
    }
    let file = fs::File::create(&p.pkg_path)
        .map_err(|e| format!("无法创建 {}: {}", p.pkg_path.display(), e))?;
    let mut zw = zip::ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let write_entry = |zw: &mut zip::ZipWriter<fs::File>,
                       name: &str,
                       data: &[u8]|
     -> Result<(), String> {
        zw.start_file(name, opts)
            .map_err(|e| format!("写入包内 '{}' 失败: {}", name, e))?;
        zw.write_all(data)
            .map_err(|e| format!("写入包内 '{}' 内容失败: {}", name, e))?;
        Ok(())
    };
    write_entry(&mut zw, "manifest.txt", p.manifest_text.as_bytes())?;
    for (zip_path, real_path) in &p.staged {
        let data = fs::read(real_path)
            .map_err(|e| format!("读取 {} 失败: {}", real_path.display(), e))?;
        write_entry(&mut zw, zip_path, &data)?;
    }
    zw.finish()
        .map_err(|e| format!("收尾 .dialpkg 失败: {}", e))?;
    Ok(())
}

pub fn pack(project_dir: &Path, out: Option<&Path>) -> Result<PathBuf, String> {
    let p = plan(project_dir, out)?;
    write_archive(&p)?;
    // 自检：产物必须能被同一套安装校验代码吃下。
    let mf = package::parse_manifest(&p.manifest_text).map_err(|e| e.to_string())?;
    let check_file = fs::File::open(&p.pkg_path)
        .map_err(|e| format!("自检时无法打开 {}: {}", p.pkg_path.display(), e))?;
    let mut archive = zip::ZipArchive::new(check_file)
        .map_err(|e| format!("自检：产物不是合法 zip 容器: {}", e))?;
    package::verify_archive(&mut archive, &mf).map_err(|e| e.to_string())?;
    Ok(p.pkg_path)
}

/// `dialc pack --dry-run [--json]`：预览将入库的文件与哈希，**不产出 .dialpkg**。
pub fn dry_run(project_dir: &Path, out: Option<&Path>, as_json: bool) -> Result<(), String> {
    let p = plan(project_dir, out)?;
    if as_json {
        let files: Vec<serde_json::Value> = p
            .sha
            .iter()
            .map(|(path, h)| serde_json::json!({ "path": path, "sha256": h }))
            .collect();
        let obj = serde_json::json!({
            "name": p.spec.name,
            "version": p.spec.version,
            "abi": p.spec.abi,
            "iface": p.spec.iface,
            "license": p.spec.license,
            "targets": p.spec.targets.keys().collect::<Vec<_>>(),
            "src": p.spec.src,
            "files": files,
            "unlisted": p.unlisted,
            "output": p.pkg_path.to_string_lossy(),
            "written": false,
        });
        println!("{}", serde_json::to_string_pretty(&obj).map_err(|e| e.to_string())?);
        return Ok(());
    }
    println!("[dry-run] 项目: {}", project_dir.display());
    println!(
        "[dry-run] name={} version={} abi={} iface={} license={}",
        p.spec.name, p.spec.version, p.spec.abi, p.spec.iface, p.spec.license
    );
    println!("[dry-run] 将入库 {} 个文件：", p.staged.len());
    for (path, h) in &p.sha {
        let bytes = fs::metadata(
            p.staged
                .iter()
                .find(|(x, _)| x == path)
                .map(|(_, q)| q.as_path())
                .unwrap_or_else(|| Path::new("")),
        )
        .map(|m| m.len())
        .unwrap_or(0);
        println!("  {:<48} sha256={}  {} bytes", path, &h[..16], bytes);
    }
    if !p.spec.src.is_empty() {
        println!("[dry-run] manifest.src：{} 个随包 Dial 源码模块", p.spec.src.len());
        for (m, rel) in &p.spec.src {
            println!("  {} -> {}", m, rel);
        }
    }
    if !p.unlisted.is_empty() {
        println!("[dry-run] ⚠️  磁盘上有但清单未登记（不会进包）：");
        for u in &p.unlisted {
            println!("  {}", u);
        }
        println!("  如需登记：编辑 dial.toml 的 [targets] / [src] 段后重新预览。");
    }
    println!("[dry-run] 输出路径（未写出）：{}", p.pkg_path.display());
    Ok(())
}

/// `dialc pack --verify <包.dialpkg>`：用安装期同一套规则校验已有包。
pub fn verify_pkg(path: &Path) -> Result<String, String> {
    let f = fs::File::open(path)
        .map_err(|e| format!("无法打开 {}: {}", path.display(), e))?;
    let mut archive = zip::ZipArchive::new(f)
        .map_err(|e| format!("不是合法 zip 容器: {}", e))?;
    let mut mf_text = String::new();
    {
        let mut mf = archive
            .by_name("manifest.txt")
            .map_err(|_| "包内缺少 manifest.txt".to_string())?;
        mf.read_to_string(&mut mf_text)
            .map_err(|e| format!("读取 manifest.txt 失败: {}", e))?;
    }
    let mf = package::parse_manifest(&mf_text).map_err(|e| e.to_string())?;
    package::verify_archive(&mut archive, &mf).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&mf_text)
        .map_err(|e| format!("manifest.txt 不是合法 JSON: {}", e))?;
    let sha = v.get("sha256").and_then(|x| x.as_object()).map(|m| m.len()).unwrap_or(0);
    let src = v.get("src").and_then(|x| x.as_object()).map(|m| m.len()).unwrap_or(0);
    let targets = v.get("targets").and_then(|x| x.as_object()).map(|m| m.len()).unwrap_or(0);
    Ok(format!(
        "name={} version={} abi={} iface={} license={} targets={} src={} files={} 逐文件 SHA256 全部通过",
        v.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
        v.get("version").and_then(|x| x.as_str()).unwrap_or("?"),
        v.get("abi").and_then(|x| x.as_u64()).unwrap_or(0),
        v.get("iface").and_then(|x| x.as_str()).unwrap_or("?"),
        v.get("license").and_then(|x| x.as_str()).unwrap_or("?"),
        targets,
        src,
        sha
    ))
}

/// `dialc pack --init <目录>`：扫描目录自动生成 `dial.toml` 草稿。
///
/// 只写清单、绝不自动把发现的文件塞进包——清单显式声明仍是唯一的入库依据，
/// 草稿里每一项都是作者可核对、可删改的显式条目。
pub fn init_spec(project_dir: &Path) -> Result<PathBuf, String> {
    let spec_path = project_dir.join("dial.toml");
    if spec_path.exists() {
        return Err(format!(
            "{} 已存在，`--init` 拒绝覆盖（请手动编辑，或先备份再删除）",
            spec_path.display()
        ));
    }
    let name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "mypkg".to_string());
    if name.contains('/') || name.contains('\\') || name.is_empty() {
        return Err(format!("无法从目录名 '{}' 推断合法包名", name));
    }
    // 自动发现：项目根 .diali / lib/<triple>/*.a / src/*.dial
    let mut ifaces: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(project_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("diali") {
                if let Some(n) = p.file_name() {
                    ifaces.push(n.to_string_lossy().to_string());
                }
            }
        }
    }
    ifaces.sort();
    let mut libs: Vec<(String, String)> = Vec::new();
    let lib_dir = project_dir.join("lib");
    if let Ok(rd) = fs::read_dir(&lib_dir) {
        for t in rd.flatten() {
            let tp = t.path();
            if !tp.is_dir() {
                continue;
            }
            let tname = match tp.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => continue,
            };
            if let Ok(files) = fs::read_dir(&tp) {
                for e in files.flatten() {
                    let p = e.path();
                    if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("a") {
                        if let Some(n) = p.file_name() {
                            libs.push((
                                tname.clone(),
                                format!("lib/{}/{}", tname, n.to_string_lossy()),
                            ));
                        }
                    }
                }
            }
        }
    }
    libs.sort();
    let mut srcs: Vec<(String, String)> = Vec::new();
    let src_dir = project_dir.join("src");
    if let Ok(rd) = fs::read_dir(&src_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("dial") {
                if let Some(n) = p.file_name() {
                    let file = n.to_string_lossy().to_string();
                    let module = file.trim_end_matches(".dial").to_string();
                    srcs.push((module, format!("src/{}", file)));
                }
            }
        }
    }
    srcs.sort();

    let iface = ifaces.first().cloned().unwrap_or_else(|| format!("{}.diali", name));
    let mut text = String::new();
    text.push_str("# 由 `dialc pack --init` 自动生成的清单草稿：请逐项核对后再打包。\n");
    text.push_str("# 每一项都是显式声明——`dialc pack` 只会打进这里登记的文件。\n");
    text.push_str(&format!("name = \"{}\"\n", name));
    text.push_str("version = \"0.1.0\"\n");
    text.push_str(&format!("# abi 取当前标准库 ABI（STDLIB_ABI={}），见 src/package.rs\n", package::STDLIB_ABI));
    text.push_str(&format!("abi = {}\n", package::STDLIB_ABI));
    text.push_str(&format!("iface = \"{}\"\n", iface));
    text.push_str("license = \"MIT\"\n\n");
    text.push_str("[targets]\n");
    if libs.is_empty() {
        text.push_str("# 未发现 lib/<triple>/*.a：请把静态库放好后按下面格式登记。\n");
        text.push_str("# x86_64-linux-gnu = \"lib/x86_64-linux-gnu/libmypkg.a\"\n");
    } else {
        for (t, rel) in &libs {
            text.push_str(&format!("{} = \"{}\"\n", t, rel));
        }
    }
    text.push('\n');
    text.push_str("[src]\n");
    if srcs.is_empty() {
        text.push_str("# 未发现 src/*.dial；如有随包 Dial 源码模块，按下面格式登记。\n");
        text.push_str("# mylib_oo = \"src/mylib_oo.dial\"\n");
    } else {
        for (m, rel) in &srcs {
            text.push_str(&format!("{} = \"{}\"\n", m, rel));
        }
    }
    fs::write(&spec_path, &text)
        .map_err(|e| format!("写入 {} 失败: {}", spec_path.display(), e))?;
    Ok(spec_path)
}

/// `dialc new <目录> [包名]`：生成一个可直接 `pack` 的项目骨架。
///
/// 骨架里的静态库路径按**当前宿主 triple** 填好，作者只需把自己的 `.a` 放到那个位置；
/// `main.dial` 与 `.diali` 都是最小可编译示例，保证 `dialc new` 之后立刻有东西能跑。
pub fn new_project(dir: &Path, name: Option<&str>) -> Result<PathBuf, String> {
    let pkg_name = name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("mylib")
    });
    check_component("包名", pkg_name)?;
    if pkg_name.contains('/') {
        return Err(format!("包名 '{}' 不能含路径分隔符", pkg_name));
    }
    let toml = dir.join("dial.toml");
    if toml.exists() {
        return Err(format!(
            "{} 已存在，拒绝覆盖（dialc new 不改动已有项目）",
            toml.display()
        ));
    }
    fs::create_dir_all(dir.join("lib"))
        .map_err(|e| format!("创建项目目录 {} 失败: {}", dir.display(), e))?;
    // src/ 单独存放随包分发的 Dial 源码模块（[src] 段登记的那些）。骨架里先建空目录，
    // 作者把 .dial 放进来并在 dial.toml 的 [src] 段登记后即可随包分发。
    fs::create_dir_all(dir.join("src"))
        .map_err(|e| format!("创建 src/ 目录 {} 失败: {}", dir.join("src").display(), e))?;

    let triple = package::host_triple();
    let lib_rel = format!("lib/{}/lib{}.a", triple, pkg_name);
    let toml_text = format!(
        "# dial.toml —— Dial 包项目清单（dialc pack 读取，见 README §34）\n\
         # 字段缺失 / 未知字段 / 非静态库一律打包期报错，不会悄悄忽略。\n\
         name = \"{name}\"\n\
         version = \"1.0.0\"\n\
         abi = {abi}\n\
         iface = \"{name}.diali\"\n\
         license = \"MIT OR Apache-2.0\"\n\
         \n\
         [targets]\n\
         # 值既是磁盘上相对项目根的路径，也是 .dialpkg 内的路径。\n\
         # 把编译好的静态归档放到该位置后执行 `dialc pack {dir}`。\n\
         {triple} = \"{lib}\"\n\
         \n\
         # [src] 段（可选）：随包分发的 Dial 源码模块，放在 src/ 目录下。\n\
         # 键 = import 用的裸名，值 = src/ 下、以 .dial 结尾的相对路径；\n\
         # 装包后可直接写 import \"mylib_oo\";，编译器会自动合并编译，无需复制源码。\n\
         # [src]\n\
         # mylib_oo = \"src/mylib_oo.dial\"\n",
        name = pkg_name,
        abi = package::STDLIB_ABI,
        triple = triple,
        lib = lib_rel,
        dir = dir.display(),
    );
    fs::write(&toml, toml_text).map_err(|e| format!("写入 {} 失败: {}", toml.display(), e))?;

    let iface_text = format!(
        "// {name}.diali —— 接口签名（纯文本，平台无关）\n\
         // dialc 用它做类型检查；LSP 用它做补全与悬停提示。\n\
         \n\
         /// 返回两数之和\n\
         extern fn {name}_add(a: int, b: int): int;\n",
        name = pkg_name,
    );
    fs::write(dir.join(format!("{}.diali", pkg_name)), iface_text)
        .map_err(|e| format!("写入 .diali 失败: {}", e))?;

    let main_text = format!(
        "// {name} —— 示例程序\n\
         // 打包后的库由 `dialc install {name}.dialpkg` 安装，再用 `import` 引用。\n\
         extern fn {name}_add(a: int, b: int): int;\n\
         link \"{lib}\";\n\
         \n\
         fn main(): int {{\n\
             print({name}_add(20, 22));\n\
             return 0;\n\
         }}\n",
        name = pkg_name,
        lib = lib_rel,
    );
    fs::write(dir.join("main.dial"), main_text)
        .map_err(|e| format!("写入 main.dial 失败: {}", e))?;

    fs::write(dir.join("lib").join(".gitkeep"), "")
        .map_err(|e| format!("写入 lib/.gitkeep 失败: {}", e))?;
    fs::write(dir.join("src").join(".gitkeep"), "")
        .map_err(|e| format!("写入 src/.gitkeep 失败: {}", e))?;

    Ok(dir.to_path_buf())
}

/// `dialc list`：列出缓存目录中所有已安装的包。
pub fn list_installed() -> Result<Vec<(String, String, u64, PathBuf)>, String> {
    let root = package::cache_root().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let rd = match fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return Ok(out), // 缓存目录尚不存在 = 没装过任何包
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        let mf = dir.join("manifest.txt");
        if !mf.is_file() {
            continue;
        }
        let text = fs::read_to_string(&mf)
            .map_err(|e| format!("读取 {} 失败: {}", mf.display(), e))?;
        match package::parse_manifest(&text) {
            Ok(m) => out.push((m.name, m.version, m.abi, dir)),
            // 坏掉的 manifest 不应该让整个 list 失败，但要显式提示（Fail-Fast 精神）。
            Err(e) => eprintln!("⚠️  跳过损坏的包 {}: {}", dir.display(), e),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// `dialc uninstall <包名>`：删除缓存中的包目录。
pub fn uninstall(name: &str) -> Result<PathBuf, String> {
    check_component("包名", name)?;
    if name.contains('/') {
        return Err(format!("包名 '{}' 不能含路径分隔符", name));
    }
    let root = package::cache_root().map_err(|e| e.to_string())?;
    let dir = root.join(name);
    if !dir.is_dir() {
        return Err(format!(
            "包 '{}' 未安装（缓存目录 {} 下没有它）。可用 `dialc list` 查看已安装包。",
            name,
            root.display()
        ));
    }
    fs::remove_dir_all(&dir)
        .map_err(|e| format!("删除 {} 失败: {}", dir.display(), e))?;
    Ok(dir)
}
