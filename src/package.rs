//! 包分发（v0.92）：`.dialpkg` 容器读取、缓存管理、manifest 解析、`.diali` 接口解析。
//!
//! 规范见 [ARCHITECTURE.md](../ARCHITECTURE.md) 与 README §25。要点回顾：
//!
//! - `.dialpkg` 是**专属扩展名**的 zip 容器，内含 `manifest.txt`（JSON 映射字典）、
//!   `xxx.diali`（唯一的接口签名，平台无关）、`lib/`（全平台预编译机器码）。
//! - 解压目标固定为 `~/.dial/packages/<name>/`，**绝不污染用户项目目录**。
//! - 解压时**拒绝路径穿越**——包是外部输入，不能给它写入任意路径的能力。
//! - SHA256 **校验先于一切**：残缺或被篡改的二进制绝不允许流到链接阶段。

use std::collections::HashMap;
use std::fs;
use std::io::Read;
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
pub const STDLIB_ABI: u64 = 4;

/// 全局包缓存根目录：`~/.dial/packages/`（Windows：`%USERPROFILE%\.dial\packages\`）。
///
/// 缓存是**可重建的派生数据**，不是权威来源：用户可以整个删掉，编译器会重新解压。
pub fn cache_root() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(".dial").join("packages"))
}

/// 取用户主目录。优先 `HOME`（Linux / Termux），回退 `USERPROFILE`（Windows），
/// 最后回退当前目录——但回退到当前目录**有风险**（会污染项目），故直接报错更诚实。
fn dirs_home() -> Result<PathBuf> {
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
    /// 包内相对路径 -> SHA256（十六进制小写）。
    pub sha256: HashMap<String, String>,
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

    let mut sha256 = HashMap::new();
    if let Some(s) = v.get("sha256").and_then(|x| x.as_object()) {
        for (file, val) in s {
            if let Some(h) = val.as_str() {
                sha256.insert(file.clone(), h.to_lowercase());
            }
        }
    }

    Ok(Manifest {
        name,
        version,
        abi,
        iface,
        license,
        targets,
        sha256,
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
pub fn install_from_url(url: &str, expected_sha256: Option<&str>) -> Result<(PathBuf, Manifest)> {
    let tmp_dir = download_dir()?;
    let fname = url_filename(url)?;
    let tmp = tmp_dir.join(format!("{}.part", fname));

    let res = (|| -> Result<(PathBuf, Manifest)> {
        download_to(url, &tmp)?;

        // 整包哈希：钉死「这就是我要的那一份」。
        if let Some(want) = expected_sha256 {
            let want = want.trim().to_ascii_lowercase();
            let got = sha256_file(&tmp)?;
            if got != want {
                return Err(DialcError::new(
                    format!(
                        "下载的包 SHA256 不符：期望 {}，实际 {}\n\
                         下载物已删除，绝不安装未经校验的文件。\n\
                         请核对哈希来源（发行页提供的校验值），或先手动下载后用 \
                         `dialc install <本地文件>` 安装。",
                        want, got
                    ),
                    0,
                    0,
                ));
            }
        } else {
            println!(
                "ℹ️  未指定 --sha256：本次只校验包内各文件与 manifest.txt 的一致性\n\
                 （能发现残缺与篡改），**未**校验整包来源。\n\
                 若需强保证，请追加 --sha256 <发行页给出的哈希>。"
            );
        }

        let r = install_inner(&tmp);
        r
    })();

    // 临时文件一律清掉：成功也删（解压已完成），失败也删（不留残骸）。
    let _ = fs::remove_file(&tmp);
    res
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
fn download_to(url: &str, dest: &Path) -> Result<()> {
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

    // **校验先于解压**（§5.1）：先把包里每个文件按 manifest 登记的哈希验一遍，
    // 一个字节都还没落到磁盘上。校验不过就地失败，绝不留下半个残缺的包。
    verify_archive(&mut archive, &manifest)?;

    let dest = cache_root()?.join(&manifest.name);
    if dest.join("manifest.txt").exists() {
        // 已安装：读出旧 manifest 比对 ABI，决定覆盖还是拒绝。
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
        // ABI 相同：整目录删除后重新解压（覆盖，避免残留旧文件）。
        fs::remove_dir_all(&dest).map_err(|e| {
            DialcError::new(format!("覆盖安装时删除旧目录失败 {}: {}", dest.display(), e), 0, 0)
        })?;
    }
    fs::create_dir_all(&dest)
        .map_err(|e| DialcError::new(format!("创建包缓存目录失败 {}: {}", dest.display(), e), 0, 0))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| DialcError::new(format!("读取包内第 {} 项失败: {}", i, e), 0, 0))?;
        let out = safe_join(&dest, entry.name())?;
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

    Ok((dest, manifest))
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
pub fn resolve_type_name(name: &str, opaque: &[String]) -> Result<crate::ast::Type> {
    use crate::ast::Type;
    // channel<T>：规范允许包接口暴露通道（极少见，但签名语法一致）
    if let Some(inner) = name.strip_prefix("channel<").and_then(|s| s.strip_suffix('>')) {
        let elem = resolve_type_name(inner.trim(), opaque)?;
        return Ok(Type::Channel(Box::new(elem)));
    }
    if let Some(inner) = name.strip_suffix("[]") {
        let elem = resolve_type_name(inner.trim(), opaque)?;
        return Ok(Type::Array(Box::new(elem)));
    }
    let t = match name {
        "int" => Type::Int,
        "float" => Type::Float,
        "bool" => Type::Bool,
        "char" => Type::Char,
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
        for p in params_src.split(',') {
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
