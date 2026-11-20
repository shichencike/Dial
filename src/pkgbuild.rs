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
use std::process::Command;

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
    /// P4.0：C 源码声明（`[c]` 段）。`None` 表示这是纯机器码包（现状，零 C 源），
    /// `Some` 表示 pack 需自动交叉编译 C 源成三平台 `.a`（prebuilt），或随包分发
    /// C 源供 install 期就地编译（source）。字段全部来自清单显式声明，零脚本。
    pub c: Option<CSpec>,
    /// P4.2：`[dependencies]` 段——依赖包名 → 语义版本区间。空 map 表示无依赖。
    pub dependencies: BTreeMap<String, String>,
    /// P4.3：显式声明「本包允许安装在哪些平台」（Target Triple 列表）。
    /// 空列表表示**不限制**（向后兼容旧包：旧清单无此字段 → 空 → 任何平台都能装，
    /// 平台兼容性仍由 `targets` 在链接期把关）。非空时 install 期强制校验，
    /// host 不在其中即拒绝安装并列出支持的平台。
    pub platforms: Vec<String>,
}

/// P4.0：`[c]` 段的解析结果——声明式 C 源码编译参数（零构建脚本）。
#[derive(Debug, Clone)]
pub struct CSpec {
    /// 待编译的 `.c` 源文件相对路径（位于 `c/` 目录下），按声明顺序编译。
    pub sources: Vec<String>,
    /// 头文件搜索目录相对路径（位于 `c/` 目录下，`-I`）。
    pub includes: Vec<String>,
    /// 额外编译 flag（如 `-O2`、`-Wall`）。禁止那些会引入副作用/脚本的项。
    pub flags: Vec<String>,
    /// 预处理器宏定义（如 `MYLIB_VERSION=1`，编译期加 `-D`）。
    pub defines: Vec<String>,
    /// 分发模式：`prebuilt`（pack 期交叉编译三平台 `.a`，默认）或
    /// `source`（install 期就地声明式编译，需用户工具链）。
    pub mode: CMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CMode {
    Prebuilt,
    Source,
}

impl CMode {
    fn parse(s: &str, line_no: usize) -> Result<Self, String> {
        match s {
            "prebuilt" => Ok(CMode::Prebuilt),
            "source" => Ok(CMode::Source),
            other => Err(format!(
                "dial.toml 第 {} 行：[c] mode 只能是 'prebuilt' 或 'source'，收到 '{}'",
                line_no, other
            )),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            CMode::Prebuilt => "prebuilt",
            CMode::Source => "source",
        }
    }
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

/// 解析逗号分隔的带引号字符串列表，如 `"a.c", "b.c"`（P4.0 `[c]` 段）。
/// 空串返回空列表。用 parse_string 复用转义与引号校验，杜绝裸 token。
fn parse_string_list(v: &str, line: usize) -> Result<Vec<String>, String> {
    let t = v.trim();
    if t.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in t.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return Err(format!(
                "dial.toml 第 {} 行：[c] 列表中出现空项（逗号之间不能有空元素）",
                line
            ));
        }
        out.push(parse_string(p, line)?);
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
fn parse_spec_inner(text: &str, require_targets: bool) -> Result<PkgSpec, String> {
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut abi: Option<u64> = None;
    let mut iface: Option<String> = None;
    let mut license: Option<String> = None;
    let mut targets: BTreeMap<String, String> = BTreeMap::new();
    let mut src: BTreeMap<String, String> = BTreeMap::new();
    // P4.0 [c] 段字段
    let mut c_sources: Vec<String> = Vec::new();
    let mut c_includes: Vec<String> = Vec::new();
    let mut c_flags: Vec<String> = Vec::new();
    let mut c_defines: Vec<String> = Vec::new();
    let mut c_mode: Option<CMode> = None;
    // P4.2 [dependencies] 段：包名 → 语义版本区间
    let mut dependencies: BTreeMap<String, String> = BTreeMap::new();
    // P4.3 顶层 platforms 字段：允许安装的平台（triple）列表，空=不限制
    let mut platforms: Vec<String> = Vec::new();
    let mut section: Option<String> = None;
    let mut seen_targets = false;
    let mut seen_src = false;
    let mut seen_c = false;
    let mut seen_dependencies = false;

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
            if sec != "targets" && sec != "src" && sec != "c" && sec != "dependencies" {
                return Err(format!(
                    "dial.toml 第 {} 行：不支持的段 '[{}]'（只支持 [targets] / [src] / [c] / [dependencies]）",
                    line_no, sec
                ));
            }
            let seen = match sec {
                "targets" => &mut seen_targets,
                "src" => &mut seen_src,
                "c" => &mut seen_c,
                _ => &mut seen_dependencies,
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

        if section.as_deref() == Some("c") {
            // P4.0：[c] 段声明 C 源码的声明式编译参数。列表用逗号分隔的带引号
            // 字符串（如 sources = "c/a.c", "c/b.c"），保持 TOML 子集零依赖。
            let list = |val: &str, line: usize| parse_string_list(val, line);
            match key {
                "sources" | "includes" | "flags" | "defines" => {
                    let vals = list(val, line_no)?;
                    let slot = match key {
                        "sources" => &mut c_sources,
                        "includes" => &mut c_includes,
                        "flags" => &mut c_flags,
                        _ => &mut c_defines,
                    };
                    if !slot.is_empty() {
                        return Err(format!(
                            "dial.toml 第 {} 行：[c] 字段 '{}' 重复",
                            line_no, key
                        ));
                    }
                    // sources / includes 是相对路径，做穿越校验；flags / defines
                    // 是编译参数，禁止引入会执行Shell的片段（分号/反引号/管道）。
                    for v in &vals {
                        if key == "sources" || key == "includes" {
                            check_component(&format!("[c].{} 项", key), v)?;
                            if key == "sources" && !v.ends_with(".c") {
                                return Err(format!(
                                    "dial.toml 第 {} 行：[c] sources 项 '{}' 必须以 .c 结尾",
                                    line_no, v
                                ));
                            }
                        } else {
                            // 零脚本铁律：编译参数里不允许出现 shell 元字符。
                            if v.contains(';') || v.contains('`') || v.contains('|')
                                || v.contains('$') || v.contains('\n') || v.contains("&&")
                            {
                                return Err(format!(
                                    "dial.toml 第 {} 行：[c].{} 项 '{}' 含 shell 元字符，\
                                     包内绝不执行脚本（零脚本铁律）",
                                    line_no, key, v
                                ));
                            }
                        }
                    }
                    *slot = vals;
                }
                "mode" => {
                    if c_mode.is_some() {
                        return Err(format!("dial.toml 第 {} 行：[c] mode 重复", line_no));
                    }
                    let m = parse_string(val, line_no)?;
                    c_mode = Some(CMode::parse(&m, line_no)?);
                }
                other => {
                    return Err(format!(
                        "dial.toml 第 {} 行：[c] 未知字段 '{}'（支持 sources/includes/flags/defines/mode）",
                        line_no, other
                    ))
                }
            }
            continue;
        }

        if section.as_deref() == Some("dependencies") {
            // P4.2：[dependencies] 段——键是依赖包名（裸名），值是语义版本区间
            // （"^1.2.0" / "~1.2" / "=1.2.3" / "*"）。区间语法在此 Fail-Fast 校验，
            // 不让非法区间流到求解阶段。
            if dependencies.contains_key(key) {
                return Err(format!(
                    "dial.toml 第 {} 行：[dependencies] 依赖 '{}' 重复",
                    line_no, key
                ));
            }
            check_component(&format!("[dependencies] 包名 '{}'", key), key)?;
            let req = parse_string(val, line_no)?;
            if let Err(e) = crate::version::parse_requirement(&req) {
                return Err(format!(
                    "dial.toml 第 {} 行：[dependencies] '{}' 的版本区间非法：{}",
                    line_no, key, e.message
                ));
            }
            dependencies.insert(key.to_string(), req);
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
            "platforms" => {
                if !platforms.is_empty() {
                    return Err(format!("dial.toml 第 {} 行：字段 'platforms' 重复", line_no));
                }
                // 逗号分隔的带引号 triple 列表（与 [c] 段列表同风格，零依赖）。
                platforms = parse_string_list(val, line_no)?;
                for t in &platforms {
                    check_component(&format!("platforms 平台 '{}'", t), t)?;
                    if t.chars().any(|c| c.is_whitespace()) {
                        return Err(format!(
                            "dial.toml 第 {} 行：platforms 平台 '{}' 含空白字符，非法 triple",
                            line_no, t
                        ));
                    }
                }
            }
            other => {
                return Err(format!(
                    "dial.toml 第 {} 行：未知字段 '{}'（支持 name/version/abi/iface/license/platforms 与 [targets] / [src] / [c] / [dependencies] 段）",
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
    // P4.0：声明 [c] 段时 [targets] 可为空——prebuilt 由 pack 自动交叉编译填充，
    // source 由 install 期就地编译；纯机器码包（无 [c]）仍必须显式登记 ≥1 个 target。
    let c = if seen_c {
        if c_sources.is_empty() {
            return Err("dial.toml：[c] 段必须声明至少一个 sources（.c 源文件）".to_string());
        }
        for s in &c_sources {
            if !s.starts_with("c/") {
                return Err(format!(
                    "dial.toml：[c] sources 项 '{}' 必须位于 c/ 目录下",
                    s
                ));
            }
        }
        for s in &c_includes {
            if !s.starts_with("c/") {
                return Err(format!(
                    "dial.toml：[c] includes 项 '{}' 必须位于 c/ 目录下",
                    s
                ));
            }
        }
        Some(CSpec {
            sources: c_sources,
            includes: c_includes,
            flags: c_flags,
            defines: c_defines,
            mode: c_mode.unwrap_or(CMode::Prebuilt),
        })
    } else {
        if c_mode.is_some() || !c_includes.is_empty() || !c_flags.is_empty() || !c_defines.is_empty() {
            return Err("dial.toml：[c] 段字段（mode/includes/flags/defines）出现在 [c] 段外，或缺少 sources".to_string());
        }
        if require_targets && targets.is_empty() {
            return Err("dial.toml：[targets] 段至少要登记一个平台的静态库（纯机器码包），或声明 [c] 段让 pack 自动编译".to_string());
        }
        None
    };

    Ok(PkgSpec {
        name,
        version,
        abi,
        iface,
        license,
        targets,
        src,
        c,
        dependencies,
        platforms,
    })
}

/// 打包用解析：要求纯机器码包至少登记一个 target（保持 pack 既有校验）。
pub fn parse_spec(text: &str) -> Result<PkgSpec, String> {
    parse_spec_inner(text, true)
}

/// 依赖解析用解析（P4.2）：项目只需 [dependencies]，不要求 [targets]（项目不是待打包的库）。
pub fn parse_spec_for_resolve(text: &str) -> Result<PkgSpec, String> {
    parse_spec_inner(text, false)
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

/// P4.0 三平台交叉编译目标表：(zig cc -target 名, manifest/host triple 键)。
///
/// 与 `packages/{http,gui,...}/build.sh` 完全同一套映射——前者给 `zig cc`，后者
/// 逐字等于 `package::host_triple()` 返回、也是 manifest `[targets]` 的键。
const CROSS_TARGETS: &[(&str, &str)] = &[
    ("x86_64-linux-gnu", "x86_64-linux-gnu"),
    ("aarch64-linux-musl", "aarch64-unknown-linux-musl"),
    ("x86_64-windows-gnu", "x86_64-pc-windows-gnu"),
];

/// 找到交叉编译用的 zig 可执行文件。
///
/// 允许 `DIAL_C_COMPILER` 覆盖为带 zig 语义的编译器（例如自定义位置的 zig），
/// 但默认必须能调起 `zig cc -target` 与 `zig ar`——prebuilt 编 C 源是包作者责任，
/// 缺 zig 时 Fail-Fast 给出安装指引（零脚本、无隐式下载）。
fn find_zig() -> Result<PathBuf, String> {
    // DIAL_C_COMPILER 若首段就是 zig，直接采用其路径，尊重作者环境。
    if let Ok(val) = std::env::var("DIAL_C_COMPILER") {
        let prog = val.split_whitespace().next().unwrap_or("").trim();
        if !prog.is_empty() && Path::new(prog).is_file() {
            return Ok(PathBuf::from(prog));
        }
    }
    for cand in ["zig", "/usr/local/bin/zig", "/usr/bin/zig"] {
        if let Some(p) = which_like(cand) {
            return Ok(p);
        }
    }
    Err("找不到 zig 交叉编译器——pack 编 C 源需要 zig（zig cc -target + zig ar）。\
         请安装 zig 0.11+ 并加入 PATH，或设 DIAL_C_COMPILER 指向 zig。"
        .to_string())
}

/// 极简 PATH 查找（零依赖，避免引入 which crate）：含 '/' 时直接查文件，
/// 否则遍历 PATH 各段找可执行文件。
fn which_like(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = Path::new(name);
        return p.is_file().then(|| p.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// P4.0：把 `[c]` 段声明的 C 源交叉编译成三平台静态库，写入项目 `lib/<triple>/`。
///
/// 返回 (triple 键, 包内相对路径) 列表，供 plan 登记进 `[targets]`。可复现三件套：
/// `-g0`（去 COFF CodeView 随机 obj 名）、`-ffile-prefix-map`（规范化 __FILE__ 的
/// 临时目录路径）、`SOURCE_DATE_EPOCH=0`（固定 COFF 时间戳）——与仓库现成 build.sh
/// 同款，保证同一输入两次 pack 产物逐字节一致。
fn cross_compile_c(project_dir: &Path, spec: &PkgSpec) -> Result<BTreeMap<String, String>, String> {
    let cspec = spec.c.as_ref().ok_or("cross_compile_c 要求 spec.c 已声明")?;
    let zig = find_zig()?;

    // 源文件在声明时已统一要求位于 c/ 下；这里解析成绝对路径并确认存在。
    let mut abs_sources: Vec<PathBuf> = Vec::new();
    for rel in &cspec.sources {
        let p = project_dir.join(rel);
        if !p.is_file() {
            return Err(format!("[c] 源文件不存在: {}（声明为 '{}'）", p.display(), rel));
        }
        abs_sources.push(p);
    }
    for inc in &cspec.includes {
        let p = project_dir.join(inc);
        if !p.is_dir() {
            return Err(format!("[c] 头文件目录不存在: {}（声明为 '{}'）", p.display(), inc));
        }
    }

    let mut result = BTreeMap::new();
    for (zig_target, triple) in CROSS_TARGETS {
        let work = temp_build_dir(&format!("dialpack_{}_{}", spec.name, triple));
        let obj_dir = work.join("obj");
        fs::create_dir_all(&obj_dir)
            .map_err(|e| format!("创建交叉编译临时目录失败: {}", e))?;

        // -ffile-prefix-map：把项目根与临时目录都规范化为稳定前缀。
        let mut cc_args: Vec<String> = vec![
            "cc".to_string(),
            "-target".to_string(),
            (*zig_target).to_string(),
            "-c".to_string(),
            "-O2".to_string(),
            "-fPIC".to_string(),
            "-g0".to_string(),
            format!("-ffile-prefix-map={}=.", project_dir.display()),
            format!("-ffile-prefix-map={}=.", work.display()),
        ];
        for inc in &cspec.includes {
            cc_args.push("-I".to_string());
            cc_args.push(project_dir.join(inc).display().to_string());
        }
        for d in &cspec.defines {
            cc_args.push(format!("-D{}", d));
        }
        for f in &cspec.flags {
            cc_args.push(f.clone());
        }

        let mut objs: Vec<PathBuf> = Vec::new();
        for (i, src) in abs_sources.iter().enumerate() {
            let obj = obj_dir.join(format!("src{}.o", i));
            let mut args = cc_args.clone();
            args.push(src.display().to_string());
            args.push("-o".to_string());
            args.push(obj.display().to_string());
            let status = Command::new(&zig)
                .args(&args)
                .env("SOURCE_DATE_EPOCH", "0")
                .current_dir(project_dir)
                .status()
                .map_err(|e| format!("调用 zig cc 编译 {} 失败: {}", triple, e))?;
            if !status.success() {
                return Err(format!(
                    "zig cc 交叉编译 '{}' 到 {} 失败（退出码 {:?}）——\
                     请检查 [c] sources 的 C 代码是否可移植到该平台",
                    src.display(), triple, status.code()
                ));
            }
            objs.push(obj);
        }

        // 归档：zig ar rcs lib<name>.a obj...
        let lib_name = format!("lib{}.a", spec.name);
        let out_lib = obj_dir.join(&lib_name);
        let mut ar_args: Vec<String> = vec![
            "ar".to_string(), "rcs".to_string(), out_lib.display().to_string()
        ];
        for obj in &objs {
            ar_args.push(obj.display().to_string());
        }
        let status = Command::new(&zig)
            .args(&ar_args)
            .env("SOURCE_DATE_EPOCH", "0")
            .status()
            .map_err(|e| format!("调用 zig ar 归档 {} 失败: {}", triple, e))?;
        if !status.success() {
            return Err(format!("zig ar 归档 {} 失败（退出码 {:?}）", triple, status.code()));
        }

        // 落到项目 lib/<triple>/lib<name>.a（包内路径与磁盘路径一致）。
        let dest_rel = format!("lib/{}/{}", triple, lib_name);
        let dest = project_dir.join(&dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建库目录 {} 失败: {}", parent.display(), e))?;
        }
        fs::copy(&out_lib, &dest)
            .map_err(|e| format!("复制交叉编译产物到 {} 失败: {}", dest.display(), e))?;
        result.insert(triple.to_string(), dest_rel);

        let _ = fs::remove_dir_all(&work);
    }
    Ok(result)
}

/// 生成一个确定命名的临时构建目录名（内容哈希无关，仅用于放 .o；
/// __FILE__ 已被 -ffile-prefix-map 规范化，故目录名不影响产物字节）。
fn temp_build_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("dialc_cross_{}", tag));
    // 复用同一路径覆盖写，避免随机目录名进入任何调试段。
    let _ = fs::remove_dir_all(&base);
    base
}

/// 递归收集目录下的所有普通文件为 (包内相对路径, 绝对路径) 列表。
/// 用于 P4.0 native-src/ 源码存档随包分发（审计用，不进编译路径）。
fn collect_tree_recursive(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("读取目录 {} 失败: {}", dir.display(), e))?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_tree_recursive(base, &p, out)?;
        } else if p.is_file() {
            let rel = p.strip_prefix(base)
                .map_err(|_| format!("路径 {} 不在 {} 下", p.display(), base.display()))?
                .to_string_lossy().replace('\\', "/");
            out.push((rel, p));
        }
    }
    Ok(())
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
    let mut spec = parse_spec(&text)?;

    // P4.0 prebuilt：声明了 [c] 且 mode=prebuilt 时，先交叉编译三平台 .a，
    // 自动生成 [targets] 登记（用户零手填哈希/路径）。source 模式不在这里编译。
    if let Some(cspec) = spec.c.as_ref() {
        if cspec.mode == CMode::Prebuilt {
            let built = cross_compile_c(project_dir, &spec)?;
            for (triple, rel) in built {
                spec.targets.insert(triple, rel);
            }
        }
    }

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
    // P4.0：声明了 [c] 时，C 源与头文件随包分发：
    //   - prebuilt：随包供审计（用户拿到机器码 + 可审阅源码）；
    //   - source：随包供 install 期就地编译。
    // 同时把 native-src/ 源码存档目录整体纳入（仅供审计，不进编译路径）。
    if let Some(cspec) = spec.c.as_ref() {
        let mut c_files: Vec<(String, PathBuf)> = Vec::new();
        for rel in cspec.sources.iter().chain(cspec.includes.iter()) {
            let p = project_dir.join(rel);
            if !p.is_file() && !p.is_dir() {
                return Err(format!("[c] 声明的文件/目录不存在: {}", p.display()));
            }
            if p.is_file() {
                c_files.push((rel.clone(), p));
            } else {
                // includes 声明的是目录，递归纳入其下头文件
                collect_tree_recursive(project_dir, &p, &mut c_files)?;
            }
        }
        staged.extend(c_files);
        let native_src = project_dir.join("native-src");
        if native_src.is_dir() {
            collect_tree_recursive(project_dir, &native_src, &mut staged)?;
        }
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
    // P4.0：c 字段（可选）。有 [c] 段时写入声明供 install 期使用/审计，
    // 无 [c] 段时完全不写该键（保持旧包 manifest 逐字段兼容）。
    let c_json = spec.c.as_ref().map(|c| serde_json::json!({
        "sources": c.sources,
        "includes": c.includes,
        "flags": c.flags,
        "defines": c.defines,
        "mode": c.mode.as_str(),
    }));
    let mut manifest = serde_json::json!({
        "name": spec.name,
        "version": spec.version,
        "abi": spec.abi,
        "iface": spec.iface,
        "license": spec.license,
        "targets": targets_obj,
        "src": src_obj,
        "sha256": sha_obj,
    });
    if let Some(cj) = c_json {
        manifest["c"] = cj;
    }
    // P4.3：platforms 非空才写入 manifest（空=不限制，旧包无此字段照常兼容）。
    if !spec.platforms.is_empty() {
        manifest["platforms"] = serde_json::to_value(&spec.platforms)
            .map_err(|e| format!("序列化 platforms 失败: {}", e))?;
    }
    // P4.2：dependencies 可选记录到 manifest 供审计（空则不写，保持旧包兼容）。
    if !spec.dependencies.is_empty() {
        manifest["dependencies"] = serde_json::to_value(&spec.dependencies)
            .map_err(|e| format!("序列化 dependencies 失败: {}", e))?;
    }
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
            "dependencies": p.spec.dependencies,
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
    text.push_str("\n# P4.3 平台声明（可选）：只允许这些 triple 安装，其它平台 install 期即拒绝。\n");
    text.push_str("# 不写或留空则不限制（兼容旧包），平台兼容性仍由 [targets] 在链接期把关。\n");
    text.push_str("# platforms = \"x86_64-linux-gnu\", \"aarch64-linux-gnu\"\n\n");
    text.push_str("[dependencies]\n");
    text.push_str("# P4.2：依赖包名 = 语义版本区间（^1.2.0 / ~1.2.3 / =1.2.3 / *）。\n");
    text.push_str("# 用 `dialc resolve .` 从静态索引解析并生成 dial.lock。\n");
    text.push_str("# json = \"^2.0.0\"\n");
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
    // P4.4：与安装共用同一把跨进程锁，防止「一边装一边删」的竞态。
    let _lock = package::InstallLock::acquire().map_err(|e| e.message)?;
    let root = package::cache_root().map_err(|e| e.to_string())?;
    let dir = root.join(name);
    if !dir.is_dir() {
        return Err(format!(
            "包 '{}' 未安装（缓存目录 {} 下没有它）。可用 `dialc list` 查看已安装包。",
            name,
            root.display()
        ));
    }
    // P4.4：反依赖检查——若其它已安装包声明了对 name 的依赖，拒绝破坏性卸载。
    let rd = fs::read_dir(&root).map_err(|e| format!("读取包目录失败: {}", e))?;
    let mut dependents: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        let d = entry.path();
        if d == dir || !d.is_dir() {
            continue;
        }
        let mf = d.join("manifest.txt");
        if !mf.is_file() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&mf) {
            if let Ok(m) = package::parse_manifest(&text) {
                if m.dependencies.contains_key(name) {
                    dependents.push(m.name);
                }
            }
        }
    }
    if !dependents.is_empty() {
        dependents.sort();
        dependents.dedup();
        return Err(format!(
            "无法卸载 '{}'：以下已安装包声明了对它的依赖：\n  {}\n\
             请先卸载这些包（或逐一 `dialc uninstall <包名>`）后再卸载 '{}'。\n\
             若确认要强制卸载（可能导致上述包链接失败），请手动删除 {}。",
            name,
            dependents.join(", "),
            name,
            dir.display()
        ));
    }
    fs::remove_dir_all(&dir)
        .map_err(|e| format!("删除 {} 失败: {}", dir.display(), e))?;
    Ok(dir)
}
