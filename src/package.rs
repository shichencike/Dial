//! 包分发（v0.92）：`.dialpkg` 容器读取、缓存管理、manifest 解析、`.diali` 接口解析。
//!
//! 规范见 [ARCHITECTURE.md](../ARCHITECTURE.md) 与 README §25。要点回顾：
//!
//! - `.dialpkg` 是**专属扩展名**的 zip 容器，内含 `manifest.txt`（JSON 映射字典）、
//!   `xxx.diali`（唯一的接口签名，平台无关）、`lib/`（全平台预编译机器码）。
//! - 解压目标固定为 `~/.dial/packages/<name>/`，**绝不污染用户项目目录**。
//! - 解压时**拒绝路径穿越**——包是外部输入，不能给它写入任意路径的能力。
//! - SHA256 **校验先于一切**：残缺或被篡改的二进制绝不允许流到链接阶段。

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::error::{DialcError, Result};

// ---------------------------------------------------------------------------
// 目录与平台
// ---------------------------------------------------------------------------

/// 编译器当前可接受的官方标准库（`stdlib.dialpkg`）ABI 版本号。
///
/// 标准库独立发版（tag `stdlib-v*`，与编译器解耦）。dialc 在链接期加载标准库时
/// 会严格校验包内 `manifest.abi` 是否等于本常量；**不等即报错并要求重装**，
/// 绝不静默链入不兼容的二进制（见 `main.rs::link`）。
///
/// 约定：标准库发生破坏性 ABI 变更时，本常量与对应发布的 `abi` 同步递增。
///
/// v1.01 起为 **2**：标准库新增了 `dial_env.c`（`dial_env_init` / `dial_args` /
/// `dial_now` / `dial_random` / `dial_sleep`）与 `dial_array_sort` 等导出符号，
/// 且编译器生成的 `main` 入口会**调用** `dial_env_init`——这意味着旧版标准库包
/// （abi=1）链接新编译器必然缺少 `dial_env_init`，报出难懂的
/// `undefined symbol: dial_env_init`。因此本次变更属于破坏性 ABI 变更，
/// 递增到 2，让旧包在链接期就被**明确拦下**并给出重装指引。
///
/// v1.02 起为 **3**：标准库新增 `dial_raw.c`（`dial_raw_alloc` / `dial_raw_free`），
/// 供裸指针 `raw_alloc` / `raw_free` / `drop` / `with` 使用。与 v1.01 不同，这两个
/// 符号是**条件引用**（只有用户代码用到裸指针时才产生调用），但装 abi=2 的旧包时
/// 只要写了裸指针程序，仍会撞上难懂的 `undefined symbol: dial_raw_alloc`。
/// 判据是「编译器是否可能要求旧包没有的符号」，因此同样递增，让旧包在链接期
/// 就被明确拦下并给出重装指引——ABI 校验这道安全网不能因为引用是条件性的就放过。
///
/// v1.04 增补（B/C 组缺口）起为 **6**：新增 `dial_eprint_*` stderr 输出族、
/// `dial_map_get_or` / `dial_kvmap_get_or`，以及后续 C 组标准库 API 的一批
/// `dial_*` 导出符号。这些全是**条件引用**（只有用到 eprint / get_or / 新 API
/// 的程序才产生调用），但判据不变：装 abi=5 旧包的用户一旦写了 `eprint(...)`，
/// 就会撞上 `undefined symbol: dial_eprint_int`。因此递增到 6，让旧包在链接期
/// 被明确拦下并给出重装指引，而不是把 ABI 不匹配藏成一句难懂的链接错误。
/// abi=7（v1.21 字段级 ARC）：新增 runtime 符号 `dial_struct_box_arc` /
/// `dial_struct_box_arc_release`，并扩展数组元素 kind（DIAL_ELEM_BOXARC=7）
/// 与 map 值 kind（KV_STRUCT_ARC=7）。装 abi=6 旧包的用户一旦让含句柄字段
/// struct 进容器，就会撞上 `undefined symbol: dial_struct_box_arc`，故递增拦下。
pub const STDLIB_ABI: u64 = 7;

/// 全局包缓存根目录：`~/.dial/packages/`（Windows：`%USERPROFILE%\.dial\packages\`）。
///
/// 缓存是**可重建的派生数据**，不是权威来源：用户可以整个删掉，编译器会重新解压。
pub fn cache_root() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(".dial").join("packages"))
}

// ---------------------------------------------------------------------------
// 跨进程安装锁（P4.4 工程加固）
// ---------------------------------------------------------------------------

/// 锁文件被视为陈旧的秒数。安装/卸载是短事务，30 分钟阈值足够宽松；
/// 一旦持有者崩溃，锁会在此后自动失效，避免死锁（崩溃自愈）。
const STALE_LOCK_SECS: u64 = 30 * 60;

/// 跨进程安装/卸载互斥锁。
///
/// 零新依赖、跨平台（Linux/Windows/macOS 通用）——只用 std 的 `create_new`
/// 原子创建语义：同一时刻只有一个进程能成功新建锁文件，其余进程立即拿到
/// `AlreadyExists`。锁文件UTF-8 内容是 `pid:nonce`，nonce 用于防止「陈旧锁被
/// 别的进程接管后，旧持有者 Drop 时误删新锁」。崩溃残留靠 mtime 超时自愈。
pub(crate) struct InstallLock {
    path: PathBuf,
    nonce: String,
}

impl InstallLock {
    /// 获取全局安装锁；锁被其他进程持有时 Fail-Fast 并给出处理指引。
    pub(crate) fn acquire() -> Result<Self> {
        let home = dirs_home()?;
        let dial_dir = home.join(".dial");
        fs::create_dir_all(&dial_dir).map_err(|e| {
            DialcError::new(format!("创建锁目录失败 {}: {}", dial_dir.display(), e), 0, 0)
        })?;
        let path = dial_dir.join(".install.lock");
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let nonce = format!("{}:{}", pid, nanos);

        // 至多两次：首次尝试；若锁陈旧则清理后再试一次。
        for attempt in 0..2 {
            match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_f) => {
                    // 句柄 _f 离开作用域即关闭；内容写入用 nonce 供 Drop 比对。
                    let _ = fs::write(&path, &nonce);
                    return Ok(InstallLock { path, nonce });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && Self::is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    let holder = fs::read_to_string(&path).unwrap_or_default();
                    return Err(DialcError::new(
                        format!(
                            "另一个 dialc 安装/卸载正在进行（锁文件 {}，持有者 {}），本次操作已拒绝以避免并发写坏包缓存。\n\
                             若确认没有其它 dialc 进程在运行，删除该锁文件后重试：\n\
                               rm -f '{}'",
                            path.display(),
                            holder.trim(),
                            path.display()
                        ),
                        0,
                        0,
                    ));
                }
                Err(e) => {
                    return Err(DialcError::new(
                        format!("访问锁文件 {} 失败: {}", path.display(), e),
                        0,
                        0,
                    ));
                }
            }
        }
        unreachable!("循环至多两次且每次都返回或 continue")
    }

    fn is_stale(path: &Path) -> bool {
        match fs::metadata(path).and_then(|m| m.modified()) {
            Ok(mtime) => match mtime.elapsed() {
                Ok(age) => age.as_secs() > STALE_LOCK_SECS,
                Err(_) => true, // 系统时间倒退 → 视为陈旧
            },
            Err(_) => true, // 读不到 mtime（如刚好被删）→ 视为陈旧，重试
        }
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        // 仅当锁文件内容仍是自己的 nonce 才删除——防止误删接管者的新锁。
        if let Ok(s) = fs::read_to_string(&self.path) {
            if s.trim() == self.nonce {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

/// 取用户主目录。优先 `HOME`（Linux / Termux），回退 `USERPROFILE`（Windows），
/// 最后回退当前目录——但回退到当前目录**有风险**（会污染项目），故直接报错更诚实。
pub(crate) fn dirs_home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return Ok(PathBuf::from(h));
        }
    }
    if let Some(h) = std::env::var_os("USERPROFILE") {
        if !h.is_empty() {
            return Ok(PathBuf::from(h));
        }
    }
    Err(DialcError::new(
        "无法确定用户主目录（HOME / USERPROFILE 均未设置），无法定位包缓存 ~/.dial/packages",
        0,
        0,
    ))
}

/// 当前编译目标的 **Target Triple**（唯一的寻址键）。
///
/// 用 `DIAL_TARGET` 可显式覆盖（交叉编译或测试用）；否则由编译期常量推导。
/// 之所以不另造「arch」字段：同为 x86_64，Linux/glibc、Linux/musl、Windows/gnu、
/// Windows/MSVC 的 ABI 互不兼容，「架构」不足以定位一份机器码。
///
/// 推导完全基于编译期 `cfg!` 宏——dialc 在各平台都**原生构建**（CI 用对应宿主
/// runner/容器），因此编译时的 target 即运行时的真实平台，无需运行时猜测。
pub fn host_triple() -> String {
    if let Ok(t) = std::env::var("DIAL_TARGET") {
        if !t.trim().is_empty() {
            return t.trim().to_string();
        }
    }
    // ---- Windows：用户选择 MinGW（gnu），与 ARCHITECTURE §0.1 一致 ----
    if cfg!(target_os = "windows") {
        return if cfg!(target_env = "gnu") {
            "x86_64-pc-windows-gnu".to_string()
        } else {
            "x86_64-pc-windows-msvc".to_string()
        };
    }
    // ---- Termux / Android（bionic）----
    if cfg!(target_os = "android") {
        return "aarch64-linux-android".to_string();
    }
    // ---- aarch64 Linux：musl 静态（Termux 推荐形态）vs glibc ----
    if cfg!(target_arch = "aarch64") {
        return if cfg!(target_env = "musl") {
            "aarch64-unknown-linux-musl".to_string()
        } else {
            "aarch64-linux-gnu".to_string()
        };
    }
    // ---- x86_64 Linux：musl 静态 vs glibc ----
    if cfg!(target_env = "musl") {
        "x86_64-unknown-linux-musl".to_string()
    } else {
        // 保持与既有测试 / 包清单键一致：glibc 用非 Rust 风格短名
        "x86_64-linux-gnu".to_string()
    }
}

// ---------------------------------------------------------------------------
// manifest.txt
// ---------------------------------------------------------------------------

/// `manifest.txt`：映射字典 + 元信息 + 哈希。
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    /// 包作者的版本号——**仅用于展示与人工核对**，编译器不做版本求解（§25.3）。
    pub version: String,
    /// ABI 版本号——这才是编译器唯一关心的兼容性判据。
    pub abi: u64,
    /// 唯一的接口声明文件名。
    pub iface: String,
    /// SPDX 许可表达式（§25.8 要求必填，用于让许可可见）。
    pub license: String,
    /// Target Triple -> 包内机器码相对路径。
    pub targets: HashMap<String, String>,
    /// Dial 源码模块名 -> 包内 `src/*.dial` 相对路径（v1.10 增补，可选）。
    ///
    /// 键是 `import "模块名";` 用的裸名。**缺失即空映射**：旧包没有这个字段，
    /// 必须照常能装（§25 的向后兼容）。这些文件同样在 `sha256` 字典里登记，
    /// 因此安装期 `verify_archive` 会把它们逐个校验一遍，与其它文件同等保护。
    pub src: HashMap<String, String>,
    /// 包内相对路径 -> SHA256（十六进制小写）。
    pub sha256: HashMap<String, String>,
    /// P4.0：C 源码声明（可选，旧包无此字段 → None，向后兼容）。
    /// prebuilt 包用它随包分发源码供审计；source 包用它 install 期就地编译。
    pub c: Option<CInfo>,
    /// P4.3：显式声明「本包允许安装在哪些平台」（Target Triple 列表）。
    /// 空 = 不限制（旧包无此字段 → 空，向后兼容）；非空时 install 期强制校验。
    pub platforms: Vec<String>,
    /// P4.4：本包的直接依赖（包名 → 语义版本区间）。由打包时写入 manifest.txt，
    /// 安装后供「缺失依赖」校验与 `--with-deps` 递归安装使用。
    /// 空 = 无依赖（旧包无此字段 → 空，向后兼容）。
    pub dependencies: BTreeMap<String, String>,
}

/// P4.0 manifest 中的 C 源码声明（与 pkgbuild::CSpec 对应，序列化在 manifest.txt）。
#[derive(Debug, Clone)]
pub struct CInfo {
    pub sources: Vec<String>,
    pub includes: Vec<String>,
    pub flags: Vec<String>,
    pub defines: Vec<String>,
    /// "prebuilt" | "source"
    pub mode: String,
}

impl Manifest {
    /// 本包支持哪些 Target Triple（报错时列给用户看）。
    pub fn supported_triples(&self) -> Vec<String> {
        let mut v: Vec<String> = self.targets.keys().cloned().collect();
        v.sort();
        v
    }
}

/// 解析 `manifest.txt`（JSON）。字段缺失即报错——绝不静默补默认值，
/// 因为「abi 缺失」与「abi = 0」是两回事，猜错会让不兼容的二进制悄悄链进来。
pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| DialcError::new(format!("manifest.txt 不是合法 JSON: {}", e), 0, 0))?;

    let get_str = |k: &str| -> Result<String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| DialcError::new(format!("manifest.txt 缺少字符串字段 '{}'", k), 0, 0))
    };

    let name = get_str("name")?;
    // version 与 name / abi / iface / license 一致：**缺失一律报错**，绝不静默补默认值
    // （见 README §25.2「字段缺失一律报错」铁律）。即便它目前只用于展示，也不能开例外，
    // 否则「一律」二字就失去意义，且静默默认值会掩盖包作者漏写版本的真实失误。
    let version = get_str("version")?;
    let abi = v
        .get("abi")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| DialcError::new("manifest.txt 缺少整数字段 'abi'（ABI 版本号是唯一的兼容性判据）", 0, 0))?;
    let iface = get_str("iface")?;
    // §25.8：许可必须可见。缺失即拒绝加载该包。
    let license = get_str("license")?;

    let mut targets = HashMap::new();
    let t = v
        .get("targets")
        .and_then(|x| x.as_object())
        .ok_or_else(|| DialcError::new("manifest.txt 缺少对象字段 'targets'（Target Triple 映射字典）", 0, 0))?;
    for (triple, val) in t {
        let lib = val
            .get("lib")
            .and_then(|x| x.as_str())
            .ok_or_else(|| DialcError::new(format!("manifest.txt 的 targets['{}'] 缺少 'lib' 字段", triple), 0, 0))?;
        // §25：targets 映射的库文件**只允许静态库**（.a / .lib），
        // 拒绝一切 .so / .dll 动态库——保证 Dial 产物依赖的纯粹性（只链 libc，绝不引入运行时动态依赖）。
        if !lib.ends_with(".a") && !lib.ends_with(".lib") {
            return Err(DialcError::new(
                format!(
                    "manifest.txt 的 targets['{}'].lib = '{}'：Dial 只接受静态库（.a / .lib），\
                     拒绝动态库（.so / .dll），以保证产物依赖纯粹性",
                    triple, lib
                ),
                0,
                0,
            ));
        }
        targets.insert(triple.clone(), lib.to_string());
    }

    // src：可选字段。旧 manifest 没有它 → 空映射，包照常可装（向后兼容）。
    // 但一旦给了，每个值都必须是 src/ 下、无路径穿越的 .dial 相对路径——
    // manifest 是外部不可信输入，不能让它把模块解析引到任意目录去。
    let mut src = HashMap::new();
    if let Some(s) = v.get("src").and_then(|x| x.as_object()) {
        for (module, val) in s {
            let rel = val.as_str().ok_or_else(|| {
                DialcError::new(
                    format!("manifest.txt 的 src['{}'] 不是字符串", module),
                    0,
                    0,
                )
            })?;
            let n = rel.replace('\\', "/");
            if !n.starts_with("src/")
                || !n.ends_with(".dial")
                || n.starts_with('/')
                || n.contains("..")
                || (n.len() >= 2 && n.as_bytes()[1] == b':')
            {
                return Err(DialcError::new(
                    format!(
                        "manifest.txt 的 src['{}'] = '{}' 非法：必须是 src/ 下、\
                         无路径穿越成分的 .dial 相对路径",
                        module, rel
                    ),
                    0,
                    0,
                ));
            }
            src.insert(module.clone(), rel.to_string());
        }
    }

    let mut sha256 = HashMap::new();
    if let Some(s) = v.get("sha256").and_then(|x| x.as_object()) {
        for (file, val) in s {
            if let Some(h) = val.as_str() {
                sha256.insert(file.clone(), h.to_lowercase());
            }
        }
    }

    // P4.0：c 可选字段。旧包无 → None（向后兼容）。一旦给出，校验结构与路径安全。
    let c = match v.get("c") {
        None => None,
        Some(cv) => {
            let obj = cv.as_object().ok_or_else(|| {
                DialcError::new("manifest.txt 的 'c' 字段必须是对象", 0, 0)
            })?;
            let str_list = |key: &str| -> Result<Vec<String>> {
                match obj.get(key) {
                    None => Ok(Vec::new()),
                    Some(val) => {
                        let arr = val.as_array().ok_or_else(|| {
                            DialcError::new(format!("manifest.txt 的 c.{} 必须是字符串数组", key), 0, 0)
                        })?;
                        arr.iter().map(|x| {
                            x.as_str().map(|s| s.to_string()).ok_or_else(|| {
                                DialcError::new(format!("manifest.txt 的 c.{} 含非字符串项", key), 0, 0)
                            })
                        }).collect()
                    }
                }
            };
            let sources = str_list("sources")?;
            let includes = str_list("includes")?;
            let flags = str_list("flags")?;
            let defines = str_list("defines")?;
            let mode = obj.get("mode").and_then(|x| x.as_str()).unwrap_or("prebuilt").to_string();
            if mode != "prebuilt" && mode != "source" {
                return Err(DialcError::new(
                    format!("manifest.txt 的 c.mode = '{}' 非法（只能 prebuilt/source）", mode),
                    0, 0,
                ));
            }
            // sources / includes 必须是 c/ 下的相对路径，防路径穿越。
            for s in sources.iter().chain(includes.iter()) {
                let n = s.replace('\\', "/");
                if !n.starts_with("c/") || n.starts_with('/') || n.contains("..")
                    || (n.len() >= 2 && n.as_bytes()[1] == b':')
                {
                    return Err(DialcError::new(
                        format!("manifest.txt 的 c 源路径 '{}' 非法：必须位于 c/ 下且无路径穿越", s),
                        0, 0,
                    ));
                }
            }
            if sources.is_empty() {
                return Err(DialcError::new("manifest.txt 的 c.sources 不能为空", 0, 0));
            }
            Some(CInfo { sources, includes, flags, defines, mode })
        }
    };

    // P4.3：platforms 可选字段。旧包无 → 空（不限制，向后兼容）。
    // 一旦给出，必须是非空字符串数组，且每项形似 Target Triple（无路径穿越/空白）。
    let mut platforms = Vec::new();
    if let Some(pv) = v.get("platforms") {
        let arr = pv.as_array().ok_or_else(|| {
            DialcError::new("manifest.txt 的 'platforms' 必须是字符串数组", 0, 0)
        })?;
        for item in arr {
            let t = item.as_str().ok_or_else(|| {
                DialcError::new("manifest.txt 的 platforms 含非字符串项", 0, 0)
            })?;
            if t.trim().is_empty() || t.chars().any(|c| c.is_whitespace()) {
                return Err(DialcError::new(
                    format!("manifest.txt 的 platforms 平台 '{}' 不是合法 triple（空或含空白）", t),
                    0, 0,
                ));
            }
            if t.contains("..") || t.starts_with('/') {
                return Err(DialcError::new(
                    format!("manifest.txt 的 platforms 平台 '{}' 不是合法 triple", t), 0, 0,
                ));
            }
            platforms.push(t.to_string());
        }
    }

    // P4.4：dependencies 可选字段——包名 → 语义版本区间。旧包无此字段视为空（无依赖）。
    // 逐条校验区间语法（Fail-Fast）：畸形的依赖区间绝不能悄悄进入求解器。
    let mut dependencies = BTreeMap::new();
    if let Some(dv) = v.get("dependencies") {
        let dobj = dv.as_object().ok_or_else(|| {
            DialcError::new("manifest.txt 的 'dependencies' 必须是对象（包名→版本区间）", 0, 0)
        })?;
        for (dep_name, req_val) in dobj {
            if dep_name.trim().is_empty() {
                return Err(DialcError::new(
                    "manifest.txt 的 dependencies 含空包名".to_string(), 0, 0,
                ));
            }
            let req = req_val.as_str().ok_or_else(|| {
                DialcError::new(
                    format!("manifest.txt 的依赖 '{}' 版本区间必须是字符串", dep_name), 0, 0,
                )
            })?;
            if let Err(e) = crate::version::parse_requirement(req) {
                return Err(DialcError::new(
                    format!("manifest.txt 的依赖 '{}' 区间 '{}' 非法：{}", dep_name, req, e.message),
                    0, 0,
                ));
            }
            dependencies.insert(dep_name.clone(), req.to_string());
        }
    }

    Ok(Manifest {
        name,
        version,
        abi,
        iface,
        license,
        targets,
        src,
        sha256,
        c,
        platforms,
        dependencies,
    })
}

// ---------------------------------------------------------------------------
// 安装：解压 .dialpkg
// ---------------------------------------------------------------------------

/// 把 `.dialpkg` 解压到缓存目录，返回其 manifest。
///
/// 重复安装规则（§25.6）：
/// - 缓存 `~/.dial/packages/<name>/` 下已存在同名包时，先读出旧 manifest 比对 `abi`；
/// - **ABI 相同** → 直接覆盖（整目录删除后重新解压，避免残留旧文件）；
/// - **ABI 不同** → 报错，要求用户先卸载旧包（`rm -rf ~/.dial/packages/<name>/`）再装，
///   防止不兼容的二进制悄悄链进用户程序。
pub fn install(pkg_path: &Path) -> Result<(PathBuf, Manifest)> {
    install_inner(pkg_path)
}

/// 从 **URL** 安装包（阶段 2：`dialc install <URL>`）。
///
/// 流程严格按 ARCHITECTURE.md §3.1 / §5.1：
///
/// 1. **下载**到临时文件（`curl`）——https 强制（仅 localhost 例外）；
/// 2. **整包哈希校验**（若给了 `expected_sha256`）——**在解压之前**，不符即删临时文件并报错；
/// 3. **解压前逐文件校验**（`verify_archive`）——把包内每个文件按 manifest 登记的
///    SHA256 校验一遍，**一个字节都不落到磁盘**；
/// 4. 解压到 `~/.dial/packages/<name>/`（复用本地安装的同一套逻辑：路径穿越防护 +
///    ABI 覆盖规则）。
///
/// `expected_sha256` 为 `None` 时仍会做第 3 步（能发现残缺与篡改），
/// 但**无法**证明「这就是我要的那一份包」——此时会打印一条明确提示，
/// 绝不静默降级（§5.1：失败即终止，不做降级）。
/// 保留原签名（P4.1/P4.2 调用点不变）：联机下载，不使用 offline。
pub fn install_from_url(url: &str, expected_sha256: Option<&str>) -> Result<(PathBuf, Manifest)> {
    install_from_url_opts(url, expected_sha256, false, true)
}

/// P4.3：带下载策略的 URL 安装。
///
/// - `offline=true`：只从下载缓存取包（`~/.dial/cache/downloads/`），缺缓存即 Fail-Fast，
///   绝不联网；并联校验缓存文件与 `expected_sha256`（若给）。
/// - `offline=false`：优先复用缓存（若给了 --sha256 会校验，不符则重新下载）；否则
///   带断点续传下载到 `<cache>.part`，成功后原子 rename 成缓存文件。
/// - `progress=true`：TTY 显示 curl 进度条；`--json` 场景传 false 保持输出干净。
pub fn install_from_url_opts(
    url: &str,
    expected_sha256: Option<&str>,
    offline: bool,
    progress: bool,
) -> Result<(PathBuf, Manifest)> {
    let cache_file = download_cache_path(url)?;
    let want = expected_sha256.map(|s| s.trim().to_ascii_lowercase());

    // 1) 取到本地缓存文件（联机时缺失才下载）。
    if cache_file.is_file() {
        if let Some(ref w) = want {
            let got = sha256_file(&cache_file)?;
            if &got != w {
                if offline {
                    return Err(DialcError::new(
                        format!(
                            "--offline：缓存文件 {} 的 SHA256={} 与指定 {} 不符，拒绝使用。\n\
                             请删除该缓存后用联机模式重新下载。",
                            cache_file.display(), got, w
                        ),
                        0, 0,
                    ));
                }
                // 联机且缓存哈希不符：删除缓存，走重新下载。
                let _ = fs::remove_file(&cache_file);
            }
        }
    }
    if !cache_file.is_file() {
        if offline {
            return Err(DialcError::new(
                format!(
                    "--offline：下载缓存中找不到 '{}' 对应的包（{}）。\n\
                     请先联机执行一次 `dialc install <url> --sha256 <hash>` 生成缓存，\n\
                     或去掉 --offline 联网安装。",
                    url, cache_file.display()
                ),
                0, 0,
            ));
        }
        download_package_cached(url, &cache_file, want.as_deref(), progress)?;
    }

    // 2) 整包哈希校验（offline 命中且上面已校验过；这里统一再验一次，幂等）。
    if let Some(ref w) = want {
        let got = sha256_file(&cache_file)?;
        if &got != w {
            return Err(DialcError::new(
                format!(
                    "下载的包 SHA256 不符：期望 {}，实际 {}\n\
                     文件保留在 {} 供排查，绝不安装未经校验的文件。\n\
                     请核对哈希来源（发行页给出的校验值）。",
                    w, got, cache_file.display()
                ),
                0, 0,
            ));
        }
    } else if !offline {
        // 提示走 stderr：即使 --json 且未给 --sha256，也不污染 stdout 的 JSON。
        eprintln!(
            "ℹ️  未指定 --sha256：本次只校验包内各文件与 manifest.txt 的一致性\n\
             （能发现残缺与篡改），**未**校验整包来源。\n\
             若需强保证，请追加 --sha256 <发行页给出的哈希>。"
        );
    }

    // 3) 从缓存文件安装（install_inner 只读，不改缓存）。
    install_inner(&cache_file)
}

/// 包的持久缓存路径：`download_dir()/<sha256(url)[..16]>_<filename>`。
/// 用**原始 URL**（镜像重写前）做哈希——换镜像不会让缓存失效，内容由 SHA256 把关。
fn download_cache_path(url: &str) -> Result<PathBuf> {
    let fname = url_filename(url)?;
    let key = sha256_str(url)?;
    let short: String = key.chars().take(16).collect();
    Ok(download_dir()?.join(format!("{}_{}", short, fname)))
}

/// 应用镜像重写：`DIAL_MIRROR` = "原前缀=镜像前缀"（可多条，逗号分隔）。
/// 只做字符串前缀替换；镜像 URL 仍须满足 download_to 的 https 强制（localhost 除外）。
/// 返回 (重写后的 URL, 是否发生重写)。
fn apply_mirror(url: &str) -> (String, bool) {
    let raw = match std::env::var("DIAL_MIRROR") {
        Ok(v) => v,
        Err(_) => return (url.to_string(), false),
    };
    for rule in raw.split(',') {
        let rule = rule.trim();
        if rule.is_empty() {
            continue;
        }
        if let Some((from, to)) = rule.split_once('=') {
            if !from.is_empty() && !to.is_empty() && url.starts_with(from) {
                let rewritten = format!("{}{}", to, &url[from.len()..]);
                return (rewritten, true);
            }
        }
    }
    (url.to_string(), false)
}

/// 带断点续传的缓存下载：下载到 `<cache>.part`，成功后 rename 成 cache。
/// 续传用 curl `-C -`（若 .part 存在且非空），失败保留 .part 供下次续传。
fn download_package_cached(
    url: &str,
    cache: &Path,
    expected_sha256: Option<&str>,
    progress: bool,
) -> Result<()> {
    let (fetch_url, mirrored) = apply_mirror(url);
    let part = cache.with_extension("part");

    // 若 .part 存在但 SHA256 已等于目标（上次下载完未 rename 就中断），直接 rename。
    if part.is_file() {
        if let Some(w) = expected_sha256 {
            if sha256_file(&part)? == w {
                fs::rename(&part, cache).map_err(|e| {
                    DialcError::new(format!("提交缓存文件失败: {}", e), 0, 0)
                })?;
                return Ok(());
            }
        }
    }

    let curl = which("curl").ok_or_else(|| {
        DialcError::new(
            "未找到 curl，无法下载包。请先安装 curl，或手动下载后用 `dialc install <本地文件>` 安装。",
            0, 0,
        )
    })?;
    let is_https = fetch_url.starts_with("https://");
    if !is_https && !http_allowed(&fetch_url) {
        return Err(DialcError::new(
            format!("镜像/源地址只允许 https:// 或本机地址（当前: {}）", fetch_url),
            0, 0,
        ));
    }
    let allow_http = http_allowed(&fetch_url);
    let proto = if allow_http { "=https,http" } else { "=https" };
    let proto_redir = if allow_http { "=https,http" } else { "=https" };

    let can_resume = part.is_file() && fs::metadata(&part).map(|m| m.len() > 0).unwrap_or(false);
    let mut cmd = Command::new(&curl);
    cmd.arg("--fail")
        .arg("--location")
        .arg("--proto").arg(proto)
        .arg("--proto-redir").arg(proto_redir)
        .arg("--tlsv1.2")
        .arg("--connect-timeout").arg("15")
        .arg("--max-time").arg("600")
        .arg("--retry").arg("2");
    if can_resume {
        cmd.arg("--continue-at").arg("-");
    }
    if progress && std::io::stderr().is_terminal() {
        cmd.arg("--progress-bar");
    } else {
        cmd.arg("--silent").arg("--show-error");
    }
    cmd.arg("--output").arg(&part).arg(&fetch_url);

    if mirrored {
        eprintln!("ℹ️  镜像重写: {} -> {}", url, fetch_url);
    }
    if can_resume {
        eprintln!("ℹ️  检测到未完成下载，断点续传: {}", part.display());
    }
    let out = cmd.output().map_err(|e| {
        DialcError::new(format!("调用 curl 失败（{}）: {}", curl.display(), e), 0, 0)
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(DialcError::new(
            format!(
                "下载失败: {}\n{}\n（部分文件保留在 {}，下次可断点续传）",
                fetch_url,
                err.trim().lines().last().unwrap_or("（无详细输出）"),
                part.display()
            ),
            0, 0,
        ));
    }
    if fs::metadata(&part).map(|m| m.len() == 0).unwrap_or(true) {
        return Err(DialcError::new(format!("下载得到空文件: {}", fetch_url), 0, 0));
    }
    // 下载成功：若给了 SHA256，先验 .part 再提交缓存（避免把坏文件缓存下来）。
    if let Some(w) = expected_sha256 {
        let got = sha256_file(&part)?;
        if got != w {
            return Err(DialcError::new(
                format!(
                    "下载的包 SHA256 不符：期望 {}，实际 {}。\n\
                     部分文件保留在 {} 供排查，绝不缓存或安装未经校验的文件。",
                    w, got, part.display()
                ),
                0, 0,
            ));
        }
    }
    fs::rename(&part, cache).map_err(|e| {
        DialcError::new(format!("提交下载缓存失败: {}", e), 0, 0)
    })?;
    Ok(())
}

/// 下载缓存目录：`~/.dial/cache/downloads/`。
fn download_dir() -> Result<PathBuf> {
    let home = dirs_home()?;
    let d = home.join(".dial").join("cache").join("downloads");
    fs::create_dir_all(&d)
        .map_err(|e| DialcError::new(format!("创建下载缓存目录失败 {}: {}", d.display(), e), 0, 0))?;
    Ok(d)
}

/// 从 URL 里取出文件名（用于临时文件命名）。
fn url_filename(url: &str) -> Result<String> {
    let tail = url.split(['?', '#']).next().unwrap_or(url);
    let name = tail.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err(DialcError::new(
            format!("无法从 URL 推断文件名: {}（请用 --sha256 之外的方式确认下载内容）", url),
            0,
            0,
        ));
    }
    Ok(name.to_string())
}

/// 是否允许用明文 http 下载。
///
/// 只放行 **localhost / 127.0.0.1 / ::1**——那是本机自测与本地镜像；
/// 其余一律要求 https，避免「链接被改写成 http 就静默降级到明文」这类中间人攻击。
fn http_allowed(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, r)) => r,
        None => return false,
    };
    let host = rest.split(['/', ':']).next().unwrap_or("").to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// 用 `curl` 把 URL 下载到 `dest`。
///
/// 为什么用外部 `curl` 而不是引入 Rust 的 HTTP 库：
///
/// - 本编译器**本来就依赖外部工具链**（`zig cc` / `clang` / `ar`），下载器属同一类依赖，
///   不新增「运行期还要装个东西」的新负担；
/// - 引入 TLS 栈（rustls 等）会显著拉长编译时间与依赖树，与 §21「不引入无用依赖」相悖；
/// - **TLS 绝不手写**——证书校验、主机名校验、重定向全部交给 curl 背后的成熟实现。
///
/// 安全约束（宁可失败，也不降级）：
///
/// - 只允许 `https`（localhost 除外，见 [`http_allowed`]）；
/// - `--proto '=https'` 禁止 curl 降级协议、禁止跟随到 `file://`；
/// - `-f` 让 HTTP 错误码（404/500…）直接失败；
/// - 下载后必须**非空**。
pub(crate) fn download_to(url: &str, dest: &Path) -> Result<()> {
    let is_https = url.starts_with("https://");
    if !is_https && !http_allowed(url) {
        return Err(DialcError::new(
            format!(
                "只允许 https:// 下载（当前: {}）。\n\
                 明文 http 会被中间人改写，SHA256 也挡不住「换了内容还带着正确哈希」的伪造，\
                 因此非本机地址一律拒绝。",
                url
            ),
            0,
            0,
        ));
    }
    let curl = which("curl").ok_or_else(|| {
        DialcError::new(
            "未找到 curl，无法下载包。\n\
             请先安装 curl（Debian/Ubuntu: apt install curl；Fedora: dnf install curl；\
             Termux: pkg install curl），\n\
             或手动下载后用 `dialc install <本地文件>` 安装。",
            0,
            0,
        )
    })?;

    let allow_http = http_allowed(url);
    // 安全约束（与函数文档一致，且堵上「https 被 302 降级到 http」的中间人窗口）：
    //   - 默认只允许 https；只有本机自测 / 本地镜像（localhost / 127.0.0.1 / ::1）
    //     才额外放行 http（见 [`http_allowed`]）。
    //   - `--proto-redir` 限制**重定向**协议：真实主机即便初始是 https，也禁止其
    //     重定向到 http；本机地址才允许（本地镜像常见 http 重定向）。
    let proto = if allow_http { "=https,http" } else { "=https" };
    let proto_redir = if allow_http { "=https,http" } else { "=https" };
    let mut cmd = Command::new(&curl);
    cmd.arg("--fail")
        .arg("--location")
        .arg("--silent")
        .arg("--show-error")
        .arg("--proto")
        .arg(proto)
        .arg("--proto-redir")
        .arg(proto_redir)
        .arg("--tlsv1.2")
        .arg("--connect-timeout")
        .arg("15")
        .arg("--max-time")
        .arg("600")
        .arg("--retry")
        .arg("2")
        .arg("--output")
        .arg(dest)
        .arg(url);

    let out = cmd.output().map_err(|e| {
        DialcError::new(format!("调用 curl 失败（{}）: {}", curl.display(), e), 0, 0)
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(DialcError::new(
            format!(
                "下载失败: {}\n{}",
                url,
                err.trim().lines().last().unwrap_or("（无详细输出）")
            ),
            0,
            0,
        ));
    }
    let meta = fs::metadata(dest)
        .map_err(|e| DialcError::new(format!("下载后找不到文件 {}: {}", dest.display(), e), 0, 0))?;
    if meta.len() == 0 {
        return Err(DialcError::new(format!("下载得到空文件: {}", url), 0, 0));
    }
    Ok(())
}

/// 在 `PATH` 里找可执行文件（跨平台：Windows 需额外试 `.exe`）。
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        if cfg!(windows) {
            let cand = dir.join(format!("{}.exe", name));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// 解压**之前**逐文件校验包内内容（§5.1「校验先于一切」「每个文件都要校验」）。
///
/// 只读 zip 内容做哈希，**不写任何字节到磁盘**——校验通过之前，
/// 包里的机器码一个也不会落到文件系统里。
///
/// 规则：
///
/// - `manifest.txt` 自身不校验（它是其余哈希的来源，不能自证）；
/// - 目录项跳过；
/// - 其余每个文件**必须**在 `manifest.sha256` 中登记，且哈希**必须**相符；
///   未登记 → 拒绝（无法校验的文件不允许进入缓存）；登记但不符 → 拒绝（已被篡改）。
pub fn verify_archive<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    manifest: &Manifest,
) -> Result<()> {
    let mut checked = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| DialcError::new(format!("读取包内第 {} 项失败: {}", i, e), 0, 0))?;
        let name = entry.name().to_string();
        if name.ends_with('/') || name == "manifest.txt" {
            continue;
        }
        let expected = match manifest.sha256.get(&name) {
            Some(h) => h.clone(),
            None => {
                return Err(DialcError::new(
                    format!(
                        "包 '{}' 的文件 '{}' 未在 manifest.txt 的 sha256 中登记。\n\
                         每个文件都必须登记哈希——未登记的文件无法校验，\
                         残缺或被篡改的机器码绝不允许进入链接阶段。",
                        manifest.name, name
                    ),
                    0,
                    0,
                ));
            }
        };
        let mut h = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = entry
                .read(&mut buf)
                .map_err(|e| DialcError::new(format!("读取包内 {} 失败: {}", name, e), 0, 0))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        let actual = format!("{:x}", h.finalize());
        if actual != expected {
            return Err(DialcError::new(
                format!(
                    "包 '{}' 的 SHA256 校验失败: {}（期望 {}，实际 {}）——\
                     文件已残缺或被篡改，绝不允许进入链接阶段",
                    manifest.name, name, expected, actual
                ),
                0,
                0,
            ));
        }
        checked += 1;
    }
    if checked == 0 {
        return Err(DialcError::new(
            format!("包 '{}' 内没有任何可校验的文件，拒绝安装", manifest.name),
            0,
            0,
        ));
    }
    Ok(())
}

/// 本地安装的真正实现（`install` 与 `install_from_url` 共用）。
fn install_inner(pkg_path: &Path) -> Result<(PathBuf, Manifest)> {
    // P4.4：全程持有跨进程安装锁，直到本函数返回（发布完成/失败回滚）才释放。
    // 防止两个进程并发安装/卸载同名包造成 TOCTOU（校验后旧包被替换、备份目录互相踩踏）。
    let _install_lock = InstallLock::acquire()?;
    if !pkg_path.exists() {
        return Err(DialcError::new(
            format!("找不到包文件: {}", pkg_path.display()),
            0,
            0,
        ));
    }
    let file = fs::File::open(pkg_path)
        .map_err(|e| DialcError::new(format!("无法打开包 {}: {}", pkg_path.display(), e), 0, 0))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| DialcError::new(format!("{} 不是合法的 .dialpkg（zip 容器）: {}", pkg_path.display(), e), 0, 0))?;

    // 先窥一眼 manifest 以拿到包名（决定缓存子目录名）
    let manifest = {
        let mut mf = archive
            .by_name("manifest.txt")
            .map_err(|_| DialcError::new(format!("{} 内缺少 manifest.txt", pkg_path.display()), 0, 0))?;
        let mut s = String::new();
        mf.read_to_string(&mut s)
            .map_err(|e| DialcError::new(format!("读取 manifest.txt 失败: {}", e), 0, 0))?;
        parse_manifest(&s)?
    };

    // P4.3 平台声明校验（Fail-Fast，早于解压、零字节落盘）：
    // 包若显式声明了 platforms，则当前 host 必须在此白名单内，否则拒绝安装，
    // 并明确列出「当前平台」+「本包支持的平台」+「可用 DIAL_TARGET 交叉安装」的指引。
    // platforms 为空（旧包 / 未声明）时不限制——向后兼容，平台兼容性仍由 targets 在链接期把关。
    if !manifest.platforms.is_empty() {
        let host = host_triple();
        if !manifest.platforms.iter().any(|t| t == &host) {
            let mut supported = manifest.platforms.clone();
            supported.sort();
            supported.dedup();
            return Err(DialcError::new(
                format!(
                    "包 '{}' 不支持当前平台 '{}'，已拒绝安装（一个字节都未写入）。\n\
                     该包声明支持的平台：{}\n\
                     当前平台：{}（由 DIAL_TARGET 环境变量或编译目标决定）。\n\
                     处理方式：\n\
                       · 换用与当前平台匹配的包版本；或\n\
                       · 若你确实要为目标平台安装，设置 DIAL_TARGET=<上表某个平台> 后重试；或\n\
                       · 用 `dialc info {}` 查看该包详情与支持的平台。",
                    manifest.name,
                    host,
                    supported.join(", "),
                    host,
                    manifest.name,
                ),
                0,
                0,
            ));
        }
    }

    // **校验先于解压**（§5.1）：先把包里每个文件按 manifest 登记的哈希验一遍，
    // 一个字节都还没落到磁盘上。校验不过就地失败，绝不留下半个残缺的包。
    verify_archive(&mut archive, &manifest)?;

    let cache = cache_root()?;
    let dest = cache.join(&manifest.name);

    // 已安装：先读出旧 manifest 比对 ABI（在动任何文件之前，冲突就直接拒绝）。
    if dest.join("manifest.txt").exists() {
        let old_text = fs::read_to_string(dest.join("manifest.txt")).map_err(|e| {
            DialcError::new(format!("读取已安装包 manifest 失败 {}: {}", dest.display(), e), 0, 0)
        })?;
        let old = parse_manifest(&old_text)?;
        if old.abi != manifest.abi {
            return Err(DialcError::new(
                format!(
                    "包 '{}' 已安装（abi={}），但待安装包 abi={} 与之不同。\n\
                     为避免 ABI 冲突，请先卸载旧包再安装：\n\
                       rm -rf ~/.dial/packages/{}\n\
                     卸载后重新运行：dialc install {}",
                    manifest.name, old.abi, manifest.abi, manifest.name, pkg_path.display()
                ),
                0,
                0,
            ));
        }
    }

    // P4.0 原子安装：先解压到同文件系统下的唯一临时目录，全部成功后
    // 用 rename 原子发布。覆盖安装时先把旧目录 rename 成备份，发布成功后
    // 再删备份；任一步失败都回滚，绝不留下半包或让旧包消失。
    let tmp = atomic_tmp_path(&cache, &manifest.name);
    let backup = atomic_backup_path(&cache, &manifest.name);
    // 防御：残留的上次临时/备份目录先清掉（正常路径不会留下，这里仅为健壮）。
    let _ = fs::remove_dir_all(&tmp);
    let _ = fs::remove_dir_all(&backup);

    let mut unpack = || -> Result<()> {
        fs::create_dir_all(&tmp)
            .map_err(|e| DialcError::new(format!("创建临时安装目录失败 {}: {}", tmp.display(), e), 0, 0))?;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| DialcError::new(format!("读取包内第 {} 项失败: {}", i, e), 0, 0))?;
            let out = safe_join(&tmp, entry.name())?;
            if entry.name().ends_with('/') {
                fs::create_dir_all(&out).map_err(|e| {
                    DialcError::new(format!("创建目录失败 {}: {}", out.display(), e), 0, 0)
                })?;
                continue;
            }
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    DialcError::new(format!("创建目录失败 {}: {}", parent.display(), e), 0, 0)
                })?;
            }
            let mut w = fs::File::create(&out)
                .map_err(|e| DialcError::new(format!("写入 {} 失败: {}", out.display(), e), 0, 0))?;
            std::io::copy(&mut entry, &mut w)
                .map_err(|e| DialcError::new(format!("解压 {} 失败: {}", out.display(), e), 0, 0))?;
        }
        Ok(())
    };

    if let Err(e) = unpack() {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }

    // P4.0 source 模式：若包声明了 C 源且当前 triple 缺预编译 .a，就在临时目录里
    // 就地声明式编译（零脚本），并把 .a 登记进 manifest.targets、重写 tmp 内的
    // manifest.txt。编译在 tmp 里完成——失败只删 tmp，不触碰已安装的旧包（原子性延续）。
    let mut manifest = manifest;
    let cinfo_clone = manifest.c.clone();
    if let Some(cinfo) = cinfo_clone.as_ref() {
        let host = host_triple();
        let need_build = cinfo.mode == "source" || !manifest.targets.contains_key(&host);
        if need_build {
            if let Err(e) = build_c_for_host(&tmp, &host, cinfo, &mut manifest) {
                let _ = fs::remove_dir_all(&tmp);
                return Err(e);
            }
        }
    }

    // 发布阶段：旧目录 → backup，tmp → dest；失败逐步回滚。
    let had_old = dest.exists();
    if had_old {
        fs::rename(&dest, &backup).map_err(|e| {
            let _ = fs::remove_dir_all(&tmp);
            DialcError::new(format!("备份旧包失败 {}: {}", dest.display(), e), 0, 0)
        })?;
    }
    if let Err(e) = fs::rename(&tmp, &dest) {
        // 回滚：backup → dest（若有），清理 tmp。
        if had_old {
            let _ = fs::rename(&backup, &dest);
        }
        let _ = fs::remove_dir_all(&tmp);
        return Err(DialcError::new(
            format!("原子发布新包失败（已回滚旧包）: {}", e),
            0,
            0,
        ));
    }
    // 发布成功，删除旧备份；删不掉不影响正确性（下次安装会清理），仅提示。
    if had_old && fs::remove_dir_all(&backup).is_err() {
        // 尽力而为：备份残留不影响已发布的新包，静默忽略。
    }

    Ok((dest, manifest))
}

/// 原子安装的临时/备份目录名（同 cache 根下，保证 rename 同文件系统）。
fn atomic_tmp_path(cache: &Path, name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    // 用纳秒时间戳 + 序号 + pid，避免并发安装同名包时冲突。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    cache.join(format!(".install-{}-{}-{}-{}", name, pid, nanos, n))
}

fn atomic_backup_path(cache: &Path, name: &str) -> PathBuf {
    cache.join(format!(".backup-{}", name))
}

/// manifest/host triple → zig cc -target 名（与 pkgbuild::CROSS_TARGETS 对应）。
/// 未知 triple 返回 None（该包无法在此平台就地编译，Fail-Fast 给指引）。
fn zig_target_for_triple(triple: &str) -> Option<&'static str> {
    match triple {
        "x86_64-linux-gnu" => Some("x86_64-linux-gnu"),
        "aarch64-unknown-linux-musl" => Some("aarch64-linux-musl"),
        "x86_64-pc-windows-gnu" => Some("x86_64-windows-gnu"),
        _ => None,
    }
}

/// P4.0：install 期就地声明式编译 C 源（零脚本）。
///
/// 在已解压的包目录 `pkg_dir` 内把 `cinfo.sources` 编成 `lib/<triple>/lib<name>.a`，
/// 更新 `manifest.targets` 并重写 `pkg_dir/manifest.txt`。编译参数全部来自 manifest
/// 的静态声明，没有钩子/ configure / CMake——守住零脚本铁律。失败返回 Err，
/// 由调用方回滚（删除临时目录、不动旧包）。
fn build_c_for_host(pkg_dir: &Path, host: &str, cinfo: &CInfo, manifest: &mut Manifest) -> Result<()> {
    let zig_target = zig_target_for_triple(host).ok_or_else(|| {
        DialcError::new(
            format!(
                "包 '{}' 为 source 模式，但当前平台 '{}' 没有对应的 zig 交叉目标，\
                 无法就地编译 C 源。请在受支持的平台（x86_64-linux-gnu / \
                 aarch64-unknown-linux-musl / x86_64-pc-windows-gnu）安装，\
                 或选用含该平台预编译 .a 的 prebuilt 包。",
                manifest.name, host
            ),
            0,
            0,
        )
    })?;

    // 工具链前置检查：source 模式必须要有 zig，缺失 Fail-Fast 给安装指引。
    let zig = find_zig_for_install().ok_or_else(|| {
        DialcError::new(
            "该包为 source 模式，安装期需就地编译 C 源，但找不到 zig 编译器。\n\
             请安装 zig 0.11+ 并加入 PATH；或改用含本机预编译 .a 的 prebuilt 包。"
                .to_string(),
            0,
            0,
        )
    })?;

    let mut objs: Vec<PathBuf> = Vec::new();
    for (i, rel) in cinfo.sources.iter().enumerate() {
        let src = safe_join(pkg_dir, rel)?;
        if !src.is_file() {
            return Err(DialcError::new(
                format!("source 模式：C 源文件 {} 不存在于包内", rel),
                0, 0,
            ));
        }
        let obj = pkg_dir.join(format!(".build_src{}.o", i));
        let mut args: Vec<String> = vec![
            "cc".to_string(), "-target".to_string(), zig_target.to_string(),
            "-c".to_string(), "-O2".to_string(), "-fPIC".to_string(), "-g0".to_string(),
            format!("-ffile-prefix-map={}=.", pkg_dir.display()),
        ];
        for inc in &cinfo.includes {
            args.push("-I".to_string());
            args.push(pkg_dir.join(inc).display().to_string());
        }
        for d in &cinfo.defines {
            args.push(format!("-D{}", d));
        }
        for f in &cinfo.flags {
            args.push(f.clone());
        }
        args.push(src.display().to_string());
        args.push("-o".to_string());
        args.push(obj.display().to_string());
        let status = Command::new(&zig).args(&args).env("SOURCE_DATE_EPOCH", "0")
            .status()
            .map_err(|e| DialcError::new(format!("source 模式调用 zig cc 失败: {}", e), 0, 0))?;
        if !status.success() {
            let _ = fs::remove_file(&obj);
            return Err(DialcError::new(
                format!("source 模式编译 {} 失败（退出码 {:?}）", rel, status.code()), 0, 0,
            ));
        }
        objs.push(obj);
    }

    let lib_name = format!("lib{}.a", manifest.name);
    let out_lib = pkg_dir.join(format!(".build_{}", lib_name));
    let mut ar_args: Vec<String> = vec!["ar".to_string(), "rcs".to_string(), out_lib.display().to_string()];
    for obj in &objs { ar_args.push(obj.display().to_string()); }
    let status = Command::new(&zig).args(&ar_args).env("SOURCE_DATE_EPOCH", "0")
        .status()
        .map_err(|e| DialcError::new(format!("source 模式调用 zig ar 失败: {}", e), 0, 0))?;
    for obj in &objs { let _ = fs::remove_file(obj); }
    if !status.success() {
        let _ = fs::remove_file(&out_lib);
        return Err(DialcError::new("source 模式 zig ar 归档失败".to_string(), 0, 0));
    }

    // 落到 lib/<triple>/lib<name>.a
    let rel_lib = format!("lib/{}/{}", host, lib_name);
    let dest = safe_join(pkg_dir, &rel_lib)?;
    if let Some(parent) = dest.parent() { fs::create_dir_all(parent).map_err(|e| {
        DialcError::new(format!("创建 source 库目录失败: {}", e), 0, 0)
    })?; }
    fs::rename(&out_lib, &dest).map_err(|e| {
        DialcError::new(format!("放置 source 编译产物失败: {}", e), 0, 0)
    })?;

    // 更新 manifest.targets 并重写 manifest.txt（让 resolve_lib 找到新 .a）。
    manifest.targets.insert(host.to_string(), rel_lib.clone());
    let mf_path = safe_join(pkg_dir, "manifest.txt")?;
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&mf_path)
        .map_err(|e| DialcError::new(format!("读取 manifest.txt 失败: {}", e), 0, 0))?)
        .map_err(|e| DialcError::new(format!("解析 manifest.txt 失败: {}", e), 0, 0))?;
    let mut v = v;
    let targets = v.as_object_mut().and_then(|o| o.get_mut("targets")).and_then(|t| t.as_object_mut())
        .ok_or_else(|| DialcError::new("manifest.txt 缺少 targets 对象".to_string(), 0, 0))?;
    targets.insert(host.to_string(), serde_json::json!({ "lib": rel_lib }));
    let text = serde_json::to_string_pretty(&v)
        .map_err(|e| DialcError::new(format!("序列化 manifest.txt 失败: {}", e), 0, 0))? + "\n";
    fs::write(&mf_path, text)
        .map_err(|e| DialcError::new(format!("写回 manifest.txt 失败: {}", e), 0, 0))?;
    Ok(())
}

/// 找 install 期就地编译用的 zig（PATH 或 DIAL_C_COMPILER 含 zig 时复用）。
fn find_zig_for_install() -> Option<PathBuf> {
    if let Ok(val) = std::env::var("DIAL_C_COMPILER") {
        let prog = val.split_whitespace().next().unwrap_or("").trim();
        if !prog.is_empty() && Path::new(prog).is_file() {
            return Some(PathBuf::from(prog));
        }
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let p = dir.join("zig");
        if p.is_file() { return Some(p); }
    }
    None
}

/// 路径穿越防护：把包内相对路径 `name` 安全地拼到 `root` 下。
///
/// 包是**外部输入**，绝不能给它写入任意路径的能力：
/// 拒绝绝对路径、`..` 逃逸、以及 Windows 盘符路径。
fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    let n = name.replace('\\', "/");
    if n.starts_with('/') || n.starts_with("../") || n == ".." || n.contains("/../") || n.ends_with("/..") {
        return Err(DialcError::new(
            format!("包内含非法路径（路径穿越）: '{}'，已拒绝解压", name),
            0,
            0,
        ));
    }
    // Windows 盘符（C:/...）
    if n.len() >= 2 && n.as_bytes()[1] == b':' {
        return Err(DialcError::new(
            format!("包内含绝对路径（Windows 盘符）: '{}'，已拒绝解压", name),
            0,
            0,
        ));
    }
    Ok(root.join(&n))
}

/// 在缓存中查找已安装的包。
pub fn find_installed(name: &str) -> Result<Option<(PathBuf, Manifest)>> {
    let dir = cache_root()?.join(name);
    let mf = dir.join("manifest.txt");
    if !mf.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&mf)
        .map_err(|e| DialcError::new(format!("读取 {} 失败: {}", mf.display(), e), 0, 0))?;
    let manifest = parse_manifest(&text)?;
    Ok(Some((dir, manifest)))
}

/// 在所有已安装包里查找一个 **Dial 源码模块**（`[src]` 段声明的裸名）。
///
/// 命中返回 `(包缓存目录, 包内相对路径)`。这是前端 import 解析的**第二级**：
/// `import "gui_oo";` 无需把源码复制到项目目录，直接吃包里自带的那一份，
/// 由 `main.rs::collect_sources` 按本地源码模块的方式合并编译。
///
/// 与 `resolve_imports` 的区别：那边按**包名**找整包（读 `.diali` + 选 `.a`），
/// 这里按**模块名**找单个 `.dial`（只合入源码，不引入任何机器码）。
///
/// 同名模块被多个包声明时**报错**而不是任选一份——静默挑一个等于把
/// 「装了哪个包」变成不可预测的行为，违背 Fail-Fast。坏掉的 manifest 静默跳过
/// （与 `list_installed` 一致）：它要么是无关的包，要么会在后续真正用到时报错，
/// 且 LSP 的协议流不允许在这里往 stdout/stderr 打诊断。
pub fn find_src_module(name: &str) -> Result<Option<(PathBuf, String)>> {
    let root = cache_root()?;
    let rd = match fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return Ok(None), // 缓存目录尚不存在 = 没装过任何包
    };
    let mut hits: Vec<(PathBuf, String)> = Vec::new();
    for entry in rd.flatten() {
        let dir = entry.path();
        let mf = dir.join("manifest.txt");
        if !mf.is_file() {
            continue;
        }
        let text = match fs::read_to_string(&mf) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let m = match parse_manifest(&text) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Some(rel) = m.src.get(name) {
            // 与 `.diali` / `.a` 同级保护：包内 Dial 源码被读取前也过一遍
            // SHA256。源码改动会直接改变程序行为，不能因为它「只是文本」
            // 就允许安装后被悄悄替换（§25.4「校验先于一切」）。
            verify_sha256(&dir.join(rel), rel, &m)?;
            hits.push((dir, rel.clone()));
        }
    }
    if hits.len() > 1 {
        let dirs: Vec<String> = hits.iter().map(|(d, _)| d.display().to_string()).collect();
        return Err(DialcError::new(
            format!(
                "Dial 源码模块 '{}' 在多个已安装包里同时声明（{}），无法确定用哪一份；\n\
                 请卸载冲突的包之一，或改用本地源码模块 `import \"./xxx.dial\";`。",
                name,
                dirs.join("、")
            ),
            0,
            0,
        ));
    }
    Ok(hits.pop())
}

// ---------------------------------------------------------------------------
// 选库与校验
// ---------------------------------------------------------------------------

/// 按当前 Target Triple 选出本平台要链接的那份机器码，并**强制校验其 SHA256**。
///
/// 校验只覆盖「本次实际要用的那一份」——其余平台的哈希用于分发完整性，
/// 不为用不到的机器码付出校验代价（§2.3）。
pub fn resolve_lib(dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    let triple = host_triple();
    let rel = manifest.targets.get(&triple).ok_or_else(|| {
        DialcError::new(
            format!(
                "包 '{}' 不支持当前平台 {}；它支持的平台有: {}",
                manifest.name,
                triple,
                manifest.supported_triples().join(", ")
            ),
            0,
            0,
        )
    })?;
    let lib = dir.join(rel);
    if !lib.exists() {
        return Err(DialcError::new(
            format!(
                "包 '{}' 声明的机器码缺失: {}（manifest 里的 targets['{}']）",
                manifest.name,
                lib.display(),
                triple
            ),
            0,
            0,
        ));
    }
    verify_sha256(&lib, rel, manifest)?;
    Ok(lib)
}

/// 校验接口签名文件 `.diali` 的 SHA256。
///
/// `.diali` 被篡改同样致命——它会让「类型检查通过」建立在谎言之上
/// （签名说返回 int，真实库返回指针）。
pub fn verify_iface(dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    let p = dir.join(&manifest.iface);
    if !p.exists() {
        return Err(DialcError::new(
            format!(
                "包 '{}' 声明的接口文件缺失: {}",
                manifest.name,
                manifest.iface
            ),
            0,
            0,
        ));
    }
    verify_sha256(&p, &manifest.iface, manifest)?;
    Ok(p)
}

fn verify_sha256(path: &Path, key: &str, manifest: &Manifest) -> Result<()> {
    let expected = match manifest.sha256.get(key) {
        Some(h) => h.clone(),
        None => return Ok(()), // 未登记哈希时不阻拦（向后兼容），但下面的分支保证有则必验
    };
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(DialcError::new(
            format!(
                "包 '{}' 的 SHA256 校验失败: {}（期望 {}，实际 {}）——\
                 文件已残缺或被篡改，绝不允许进入链接阶段",
                manifest.name, key, expected, actual
            ),
            0,
            0,
        ));
    }
    Ok(())
}

/// 计算文件的 SHA256（十六进制小写）。
pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .map_err(|e| DialcError::new(format!("读取 {} 失败: {}", path.display(), e), 0, 0))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

/// 计算字符串的 SHA256（十六进制小写）。
///
/// 供 stdlib 持久缓存拼接 key 使用（源码 hash + safety + tiny + 编译器 + C flags）。
pub fn sha256_str(s: &str) -> Result<String> {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    Ok(format!("{:x}", h.finalize()))
}

// ---------------------------------------------------------------------------
// .diali：接口签名（纯文本、平台无关）
// ---------------------------------------------------------------------------

/// 一个 `extern fn` 声明。
#[derive(Debug, Clone)]
pub struct ExternFn {
    pub name: String,
    /// 形参：`(参数名, 类型名)`
    pub params: Vec<(String, String)>,
    /// 返回类型名
    pub ret: String,
    /// C 风格异构变参（v1.03）：形参表末尾写了 `...`。
    /// 这是 FFI 特例，只表示「按 C varargs ABI 直传尾部参数」，
    /// **不**引入任何类型标签或装箱；Dial 自身函数不允许这样写。
    pub variadic: bool,
    /// `///` 文档注释（供 LSP 悬停提示）
    pub doc: Vec<String>,
    /// 声明所在行（1 起）——供 LSP 跳转定义跳进 `.diali`（v0.95）。
    pub line: usize,
}

/// `.diali` 解析结果。
#[derive(Debug, Clone)]
pub struct DialIface {
    /// `type Db;` 声明的不透明句柄名。
    pub opaque_types: Vec<String>,
    /// 同上，但额外带声明行号（v0.95：跳转定义用）。
    pub opaque_lines: Vec<(String, usize)>,
    pub fns: Vec<ExternFn>,
}

/// 解析 `.diali`。
///
/// 只支持两种声明，这是刻意的（§1.3：`.diali` 只有声明、不含任何实现代码）：
///   - `type Name;` —— 不透明句柄
///   - `extern fn name(a: T, b: U): R;` —— 外部函数签名
/// 其余内容（注释、空行）一律忽略；任何无法识别的**非空行**都报错，
/// 因为「看不懂的声明」比「缺一个声明」危险得多。
pub fn parse_diali(text: &str) -> Result<DialIface> {
    let mut opaque_types = Vec::new();
    let mut opaque_lines = Vec::new();
    let mut fns = Vec::new();
    let mut doc: Vec<String> = Vec::new();

    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let lnum = idx + 1;
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("///") {
            doc.push(rest.trim().to_string());
            continue;
        }
        if line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("type") {
            let rest = rest.trim();
            let name = rest
                .strip_suffix(';')
                .unwrap_or(rest)
                .trim()
                .to_string();
            if name.is_empty() {
                return Err(DialcError::new(
                    format!(".diali 第 {} 行: `type` 后缺少类型名", lnum),
                    0,
                    0,
                ));
            }
            opaque_types.push(name.clone());
            opaque_lines.push((name, lnum));
            doc.clear();
            continue;
        }
        if line.starts_with("extern") {
            let (fname, params, ret, variadic, fline) = parse_extern_fn(line, lnum)?;
            fns.push(ExternFn {
                name: fname,
                params,
                ret,
                variadic,
                doc: std::mem::take(&mut doc),
                line: fline,
            });
            continue;
        }
        return Err(DialcError::new(
            format!(
                ".diali 第 {} 行无法识别: '{}'（只支持 `type X;` 与 `extern fn f(...): R;`）",
                lnum, line
            ),
            0,
            0,
        ));
    }

    Ok(DialIface {
        opaque_types,
        opaque_lines,
        fns,
    })
}

/// 把 `.diali` 里的**类型名**解析为 Dial 的 `Type`。
///
/// `opaque` 是本包 `type X;` 声明过的不透明句柄名集合——它们是包私有的类型，
/// 只以指针形态穿过 FFI 边界，Dial 侧一律视为 `Type::Opaque(name)`。
///
/// 未声明且非内建的类型名**一律报错**：让它悄悄退化成某个默认类型，
/// 等于把「签名不匹配」这个能在编译期抓住的错误推迟成运行期的内存损坏。
/// 按**顶层**逗号切分（`<...>` / `(...)` 内的逗号不切）。
///
/// v1.04：`.diali` 的形参表与 `fn(A, B): R` 的参数表都可能有嵌套泛型/函数类型，
/// 朴素的 `split(',')` 会把 `fn(int, int): int` 切成两段，签名解析当场失真。
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut last = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' | '(' => depth += 1,
            '>' | ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[last..i].trim().to_string());
                last = i + 1;
            }
            _ => {}
        }
    }
    parts.push(s[last..].trim().to_string());
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

pub fn resolve_type_name(name: &str, opaque: &[String]) -> Result<crate::ast::Type> {
    use crate::ast::Type;
    let name = name.trim();
    // channel<T>：规范允许包接口暴露通道（极少见，但签名语法一致）
    if let Some(inner) = name.strip_prefix("channel<").and_then(|s| s.strip_suffix('>')) {
        let elem = resolve_type_name(inner.trim(), opaque)?;
        return Ok(Type::Channel(Box::new(elem)));
    }
    if let Some(inner) = name.strip_suffix("[]") {
        let elem = resolve_type_name(inner.trim(), opaque)?;
        return Ok(Type::Array(Box::new(elem)));
    }
    // v1.04 FFI：rawptr<T> 裸指针（可嵌套，如 rawptr<rawptr<byte>>）
    if let Some(inner) = name.strip_prefix("rawptr<").and_then(|s| s.strip_suffix('>')) {
        let elem = resolve_type_name(inner.trim(), opaque)?;
        return Ok(Type::RawPtr(Box::new(elem)));
    }
    // v1.04 FFI：fn(A, B): R 函数指针（C 回调签名）。
    // 必须找**配对**的 ')'，不能 rsplit_once —— 返回类型或参数里可能还嵌套着括号。
    if let Some(rest) = name.strip_prefix("fn(") {
        let mut depth: i32 = 1;
        let mut close = None;
        for (i, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close.ok_or_else(|| {
            DialcError::new(
                format!(".diali 函数指针类型 '{}' 缺少配对的 ')'", name),
                0, 0,
            )
        })?;
        let params_src = &rest[..close];
        let after = rest[close + 1..].trim();
        let ret_src = after.strip_prefix(':').ok_or_else(|| {
            DialcError::new(
                format!(".diali 函数指针类型 '{}' 缺少 ': R' 返回标注", name),
                0, 0,
            )
        })?;
        let mut ps = Vec::new();
        for p in split_top_level(params_src) {
            ps.push(resolve_type_name(&p, opaque)?);
        }
        let ret = resolve_type_name(ret_src.trim(), opaque)?;
        return Ok(Type::FnPtr(ps, Box::new(ret)));
    }
    let t = match name {
        "int" => Type::Int,
        "float" => Type::Float,
        "bool" => Type::Bool,
        "char" => Type::Char,
        // v1.04 FFI：byte / bytes 放行（此前白名单只认 string / void 等）
        "byte" => Type::Byte,
        "bytes" => Type::Bytes,
        "string" => Type::String,
        "void" => Type::Void,
        "map" => Type::Map,
        "file" => Type::File,
        "error" => Type::Error,
        "thread" => Type::Thread,
        "channel" => Type::Channel(Box::new(Type::Void)),
        other => {
            if opaque.iter().any(|o| o == other) {
                Type::Opaque(other.to_string())
            } else {
                return Err(DialcError::new(
                    format!(
                        ".diali 中的类型名 '{}' 无法识别：既不是内建类型，也未在本包用 `type {};` 声明",
                        name, name
                    ),
                    0,
                    0,
                ));
            }
        }
    };
    Ok(t)
}

/// 解析 `extern fn name(a: T, b: U): R;`，并回传声明行号（v0.95：跳转定义用）。
fn parse_extern_fn(
    line: &str,
    lnum: usize,
) -> Result<(String, Vec<(String, String)>, String, bool, usize)> {
    let body = line
        .strip_prefix("extern")
        .unwrap()
        .trim()
        .strip_prefix("fn")
        .ok_or_else(|| {
            DialcError::new(format!(".diali 第 {} 行: `extern` 后必须是 `fn`", lnum), 0, 0)
        })?
        .trim();
    let body = body.strip_suffix(';').unwrap_or(body).trim();

    // 返回类型：最后一个 ':'
    let (head, ret) = match body.rsplit_once(':') {
        Some((h, r)) => (h.trim(), r.trim().to_string()),
        None => {
            return Err(DialcError::new(
                format!(".diali 第 {} 行: 缺少返回类型（应写作 `): R;`）", lnum),
                0,
                0,
            ))
        }
    };
    // 形参表：head = "name(a: T, b: U)"
    let (fname, params_src) = match head.split_once('(') {
        Some((n, p)) => (
            n.trim().to_string(),
            p.strip_suffix(')').unwrap_or(p).trim().to_string(),
        ),
        None => {
            return Err(DialcError::new(
                format!(".diali 第 {} 行: 函数名后缺少 '(' ", lnum),
                0,
                0,
            ))
        }
    };
    if fname.is_empty() {
        return Err(DialcError::new(
            format!(".diali 第 {} 行: 缺少函数名", lnum),
            0,
            0,
        ));
    }

    let mut params = Vec::new();
    let mut variadic = false;
    if !params_src.is_empty() {
        // v1.04：必须按顶层逗号切——形参类型可能是 fn(int, int): int 这类
        // 自带逗号的函数指针签名，朴素 split(',') 会把它切坏。
        for p in split_top_level(&params_src) {
            let p = p.trim();
            if p.is_empty() {
                continue;
            }
            // `...` 是 C varargs 标记，必须写在形参表最后（v1.03）。
            if p == "..." {
                variadic = true;
                continue;
            }
            if variadic {
                return Err(DialcError::new(
                    format!(".diali 第 {} 行: C 变参标记 `...` 必须是最后一个形参", lnum),
                    0,
                    0,
                ));
            }
            let (pn, pt) = p.split_once(':').ok_or_else(|| {
                DialcError::new(
                    format!(".diali 第 {} 行: 形参 '{}' 缺少类型标注（应写作 `a: T`）", lnum, p),
                    0,
                    0,
                )
            })?;
            params.push((pn.trim().to_string(), pt.trim().to_string()));
        }
    }

    Ok((fname, params, ret, variadic, lnum))
}
