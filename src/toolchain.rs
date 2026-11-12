//! 工具链新命令组（v1.10 增补）：doctor / deps / watch / clean / completion。
//!
//! 共用一套「命令元数据」与「本地源码递归」基础设施，避免 5 个命令各写一份
//! 漂移的路径解析。所有命令都**只读或只写明确的目标**，绝不改动包契约、
//! ABI / 版本号，也不碰用户已安装包的内容（doctor 只校验不修复）。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::{import_specs, is_local_import, lexer, local_module_path, package, pkgbuild};

/// 命令元数据：`help` / `completion` / `doctor` 共用的唯一真相。
pub struct CmdSpec {
    pub name: &'static str,
    pub brief: &'static str,
    pub opts: &'static [&'static str],
}

pub const COMMANDS: &[CmdSpec] = &[
    CmdSpec { name: "build", brief: "编译链接 Dial 源文件", opts: &["-o", "-g", "--no-link", "--safety", "--pgo"] },
    CmdSpec { name: "run", brief: "编译并运行", opts: &["-o", "-g", "--no-link", "--safety", "--pgo"] },
    CmdSpec { name: "check", brief: "只做词法/语法/语义检查", opts: &[] },
    CmdSpec { name: "test", brief: "运行顶层 test 块", opts: &["--filter"] },
    CmdSpec { name: "fmt", brief: "间距规范化格式化", opts: &["--check", "--diff"] },
    CmdSpec { name: "new", brief: "生成可打包的项目骨架", opts: &[] },
    CmdSpec { name: "pack", brief: "打包 / 预览 / 校验 .dialpkg", opts: &["-o", "--init", "--dry-run", "--json", "--verify"] },
    CmdSpec { name: "install", brief: "安装包或 URL", opts: &["--sha256"] },
    CmdSpec { name: "list", brief: "列出已安装包", opts: &[] },
    CmdSpec { name: "uninstall", brief: "卸载已安装包", opts: &[] },
    CmdSpec { name: "doctor", brief: "环境自检（编译器/缓存/包完整性/版本）", opts: &["--json"] },
    CmdSpec { name: "deps", brief: "三级 import 依赖图", opts: &["--json"] },
    CmdSpec { name: "watch", brief: "文件变更后自动重跑 check/build/test", opts: &["--action", "--interval", "--max-ticks"] },
    CmdSpec { name: "clean", brief: "清理编译产物（默认 dry-run）", opts: &["--apply", "--json"] },
    CmdSpec { name: "completion", brief: "生成 bash/zsh/fish 补全脚本", opts: &[] },
    CmdSpec { name: "lsp", brief: "编辑器语言服务器", opts: &[] },
    CmdSpec { name: "version", brief: "显示版本号", opts: &[] },
    CmdSpec { name: "help", brief: "显示帮助", opts: &[] },
];

fn packages_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".dial").join("packages")
}

// ---------------------------------------------------------------------------
// 1) dialc doctor：环境自检
// ---------------------------------------------------------------------------

struct Check {
    name: String,
    status: &'static str, // ok / warn / fail
    detail: String,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Check { name: name.to_string(), status: "ok", detail: detail.into() }
    }
    fn warn(name: &str, detail: impl Into<String>) -> Self {
        Check { name: name.to_string(), status: "warn", detail: detail.into() }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Self {
        Check { name: name.to_string(), status: "fail", detail: detail.into() }
    }
}

fn probe(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or("").trim();
    Some(if first.is_empty() { "可用".to_string() } else { first.chars().take(72).collect() })
}

fn sha256_hex(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&data);
    Some(format!("{:x}", h.finalize()))
}

/// 校验一个已安装包的 manifest 与逐文件 SHA256（与 install 期同一套口径）。
fn check_installed(dir: &Path) -> (usize, Vec<String>) {
    let mf_path = dir.join("manifest.txt");
    let text = match fs::read_to_string(&mf_path) {
        Ok(t) => t,
        Err(e) => return (0, vec![format!("manifest.txt 读取失败: {}", e)]),
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return (0, vec![format!("manifest.txt 不是合法 JSON: {}", e)]),
    };
    let mut errors = Vec::new();
    let mut n = 0usize;
    if let Some(map) = v.get("sha256").and_then(|x| x.as_object()) {
        for (rel, want) in map {
            let p = dir.join(rel);
            match sha256_hex(&p) {
                Some(got) => {
                    n += 1;
                    if got != want.as_str().unwrap_or("") {
                        errors.push(format!("{} SHA256 不符（文件被改动）", rel));
                    }
                }
                None => errors.push(format!("{} 缺失", rel)),
            }
        }
    }
    (n, errors)
}

/// 返回失败项数量（>0 时调用方以非零码退出）。
pub fn doctor(as_json: bool) -> Result<i32, String> {
    let mut checks: Vec<Check> = Vec::new();

    checks.push(Check::ok("编译器版本", format!("dialc {}", env!("CARGO_PKG_VERSION"))));

    // C 编译器：zig cc 优先，其次 clang / cc。
    let mut cc = None;
    for (prog, args) in [
        ("zig", vec!["cc", "--version"]),
        ("clang", vec!["--version"]),
        ("cc", vec!["--version"]),
    ] {
        if let Some(v) = probe(prog, &args) {
            cc = Some(format!("{} {}", prog, v));
            break;
        }
    }
    match cc {
        Some(v) => checks.push(Check::ok("C 编译器", v)),
        None => checks.push(Check::fail("C 编译器", "未找到 zig cc / clang / cc，无法链接产物")),
    }
    match probe("ar", &["--version"]) {
        Some(v) => checks.push(Check::ok("静态库归档器 ar", v)),
        None => checks.push(Check::fail("静态库归档器 ar", "未找到 ar，无法生成 .a / 安装校验")),
    }

    // 包缓存目录
    let root = packages_root();
    if root.is_dir() {
        match fs::metadata(&root) {
            Ok(m) if m.permissions().readonly() => {
                checks.push(Check::warn("包缓存目录", format!("{} 只读，可能无法安装新包", root.display())));
            }
            Ok(_) => checks.push(Check::ok("包缓存目录", root.display().to_string())),
            Err(e) => checks.push(Check::fail("包缓存目录", e.to_string())),
        }
    } else {
        checks.push(Check::ok("包缓存目录", format!("{} 尚不存在（安装包时自动创建）", root.display())));
    }

    // 已安装包完整性
    let mut pkg_count = 0usize;
    let mut pkg_files = 0usize;
    let mut pkg_errors: Vec<String> = Vec::new();
    if let Ok(rd) = fs::read_dir(&root) {
        for e in rd.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            pkg_count += 1;
            let (n, errs) = check_installed(&e.path());
            pkg_files += n;
            for x in errs {
                pkg_errors.push(format!("{}: {}", e.file_name().to_string_lossy(), x));
            }
        }
    }
    if pkg_errors.is_empty() {
        checks.push(Check::ok(
            "已安装包完整性",
            format!("{} 个包 / {} 个登记文件，manifest + SHA256 全部完好", pkg_count, pkg_files),
        ));
    } else {
        checks.push(Check::fail("已安装包完整性", pkg_errors.join("；")));
    }

    // 项目清单（当前目录有 dial.toml 才检查）
    let spec_path = Path::new("dial.toml");
    if spec_path.is_file() {
        match fs::read_to_string(spec_path).map_err(|e| e.to_string()).and_then(|t| pkgbuild::parse_spec(&t)) {
            Ok(sp) => checks.push(Check::ok(
                "项目清单 dial.toml",
                format!("name={} version={} abi={} iface={}", sp.name, sp.version, sp.abi, sp.iface),
            )),
            Err(e) => checks.push(Check::fail("项目清单 dial.toml", e)),
        }
    } else {
        checks.push(Check::warn("项目清单 dial.toml", "当前目录不是项目根（已跳过）"));
    }

    // 包内 src 模块能力（编译器侧自检）
    match package::find_src_module("__dialc_doctor_probe__") {
        Ok(_) => checks.push(Check::ok("包内 src 模块解析", "三级 import 解析可用")),
        Err(e) => checks.push(Check::fail("包内 src 模块解析", e.to_string())),
    }

    let fails = checks.iter().filter(|c| c.status == "fail").count();
    let warns = checks.iter().filter(|c| c.status == "warn").count();

    if as_json {
        let arr: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| serde_json::json!({ "name": c.name, "status": c.status, "detail": c.detail }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "tool": "dialc doctor",
                "version": env!("CARGO_PKG_VERSION"),
                "checks": arr,
                "failures": fails,
                "warnings": warns,
                "healthy": fails == 0,
            }))
            .map_err(|e| e.to_string())?
        );
    } else {
        println!("[dialc doctor] Dial {} · 环境自检", env!("CARGO_PKG_VERSION"));
        for c in &checks {
            let mark = match c.status {
                "ok" => "✅",
                "warn" => "⚠️ ",
                _ => "❌",
            };
            println!("  {} {:<20} {}", mark, c.name, c.detail);
        }
        println!(
            "[doctor] 结论：{}（{} 项失败 / {} 项警告）",
            if fails == 0 { "健康" } else { "需修复" },
            fails,
            warns
        );
    }
    Ok(fails as i32)
}

// ---------------------------------------------------------------------------
// 2) dialc deps：三级 import 依赖图
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DepNode {
    kind: &'static str, // local / pkg-src / package
    spec: String,
    path: String,
    depth: usize,
    cyclic: bool,
}

/// 递归展开依赖：① 本地源码 → ② 包内 src 模块 → ③ 包名。
fn deps_walk(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
    stack: &mut Vec<PathBuf>,
    depth: usize,
    out: &mut Vec<DepNode>,
) -> Result<(), String> {
    let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if stack.contains(&canon) {
        out.push(DepNode {
            kind: "cyclic",
            spec: path.to_string_lossy().to_string(),
            path: canon.to_string_lossy().to_string(),
            depth,
            cyclic: true,
        });
        return Ok(());
    }
    if !visited.insert(canon.clone()) {
        return Ok(()); // 去重，同一文件只展开一次
    }
    let text = fs::read_to_string(path)
        .map_err(|e| format!("无法读取 {}: {}", path.display(), e))?;
    let tokens = lexer::lex(&text).map_err(|e| e.to_string())?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    stack.push(canon.clone());
    for spec in import_specs(&tokens) {
        if is_local_import(&spec, base) {
            let child = local_module_path(&spec, base);
            out.push(DepNode {
                kind: "local",
                spec: spec.clone(),
                path: child.to_string_lossy().to_string(),
                depth: depth + 1,
                cyclic: false,
            });
            deps_walk(&child, visited, stack, depth + 1, out)?;
        } else if let Some((pkg_dir, rel)) =
            package::find_src_module(&spec).map_err(|e| e.to_string())?
        {
            let child = pkg_dir.join(&rel);
            out.push(DepNode {
                kind: "pkg-src",
                spec: spec.clone(),
                path: child.to_string_lossy().to_string(),
                depth: depth + 1,
                cyclic: false,
            });
            deps_walk(&child, visited, stack, depth + 1, out)?;
        } else {
            out.push(DepNode {
                kind: "package",
                spec: spec.clone(),
                path: format!("~/.dial/packages/{}", spec),
                depth: depth + 1,
                cyclic: false,
            });
        }
    }
    stack.pop();
    Ok(())
}

pub fn deps(src: &str, as_json: bool) -> Result<(), String> {
    let root = PathBuf::from(src);
    if !root.is_file() {
        return Err(format!("依赖图入口不存在: {}", root.display()));
    }
    let mut nodes = vec![DepNode {
        kind: "root",
        spec: src.to_string(),
        path: root.to_string_lossy().to_string(),
        depth: 0,
        cyclic: false,
    }];
    deps_walk(&root, &mut HashSet::new(), &mut Vec::new(), 0, &mut nodes)?;

    if as_json {
        let arr: Vec<serde_json::Value> = nodes
            .iter()
            .map(|n| serde_json::json!({
                "kind": n.kind, "spec": n.spec, "path": n.path,
                "depth": n.depth, "cyclic": n.cyclic,
            }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "root": src, "nodes": arr, "cycles": nodes.iter().any(|n| n.cyclic),
            }))
            .map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("[dialc deps] {}（三级 import：本地源码 / 包内 src / 包名）", src);
    for n in &nodes {
        let tag = match n.kind {
            "root" => "ROOT",
            "local" => "本地",
            "pkg-src" => "包内src",
            "package" => "包名",
            _ => "循环",
        };
        println!(
            "  {}{} [{:<8}] {}  {}",
            "  ".repeat(n.depth),
            if n.depth == 0 { "" } else { "└─ " },
            tag,
            n.spec,
            if n.kind == "package" { format!("→ {}", n.path) } else { n.path.clone() }
        );
    }
    if nodes.iter().any(|n| n.cyclic) {
        println!("[deps] ⚠️  检测到循环引用（已标注并停止递归）");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3) dialc watch：文件变更后自动重跑
// ---------------------------------------------------------------------------

/// 收集主文件 + 递归本地源码模块的路径（watch 的监控集合）。
fn local_file_set(src: &str, out: &mut Vec<PathBuf>) {
    let root = PathBuf::from(src);
    if !root.is_file() {
        return;
    }
    out.push(root.clone());
    let mut seen = HashSet::new();
    let mut queue = vec![root];
    while let Some(p) = queue.pop() {
        let canon = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if !seen.insert(canon) {
            continue;
        }
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let tokens = match lexer::lex(&text) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let base = p.parent().unwrap_or_else(|| Path::new("."));
        for spec in import_specs(&tokens) {
            if is_local_import(&spec, base) {
                let child = local_module_path(&spec, base);
                if child.is_file() {
                    out.push(child.clone());
                    queue.push(child);
                }
            }
        }
    }
    out.sort();
    out.dedup();
}

fn fingerprint(files: &[PathBuf]) -> Vec<(String, u64)> {
    files
        .iter()
        .map(|p| {
            let m = fs::metadata(p);
            let t = m
                .as_ref()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (p.to_string_lossy().to_string(), t ^ m.map(|m| m.len()).unwrap_or(0))
        })
        .collect()
}

pub fn watch(src: &str, action: &str, interval_ms: u64, max_ticks: Option<u64>) -> Result<(), String> {
    if !matches!(action, "check" | "build" | "test") {
        return Err(format!("watch: 未知动作 '{}'（可用 check / build / test）", action));
    }
    if !Path::new(src).is_file() {
        return Err(format!("watch: 入口文件不存在: {}", src));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    local_file_set(src, &mut files);
    let mut last = fingerprint(&files);
    println!(
        "[watch] 监控 {} 个文件（主文件 + 本地源码模块），每 {} ms 轮询，动作={}",
        files.len(), interval_ms, action
    );
    println!("[watch] 首次执行 {} {} ...", action, src);
    let _ = Command::new(&exe).arg(action).arg(src).status();
    let mut ticks = 0u64;
    loop {
        std::thread::sleep(Duration::from_millis(interval_ms));
        ticks += 1;
        let mut now_files = Vec::new();
        local_file_set(src, &mut now_files);
        let now = fingerprint(&now_files);
        if now != last {
            println!("[watch] 检测到变更，重新执行 {} {}（第 {} 次）", action, src, ticks);
            let _ = Command::new(&exe).arg(action).arg(src).status();
            files = now_files;
            last = fingerprint(&files);
        }
        if let Some(m) = max_ticks {
            if ticks >= m {
                println!("[watch] 达到 --max-ticks {}，正常退出", m);
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4) dialc clean：清理编译产物（默认 dry-run）
// ---------------------------------------------------------------------------

pub fn clean(dir: &str, apply: bool, as_json: bool) -> Result<(), String> {
    let root = PathBuf::from(dir);
    if !root.is_dir() {
        return Err(format!("clean: 目录不存在: {}", root.display()));
    }
    let mut targets: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&root) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            // 只清理明确由 dialc 产出的中间/打包产物，绝不碰源码与包缓存。
            if matches!(ext, "o" | "bc" | "ll" | "dialpkg") {
                targets.push(p);
            }
        }
    }
    targets.sort();
    let mut removed = 0usize;
    if apply {
        for p in &targets {
            match fs::remove_file(p) {
                Ok(_) => removed += 1,
                Err(e) => return Err(format!("clean: 删除 {} 失败: {}", p.display(), e)),
            }
        }
    }
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dir": dir,
                "targets": targets.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
                "applied": apply,
                "removed": removed,
            }))
            .map_err(|e| e.to_string())?
        );
    } else {
        println!("[clean] 目录 {} · 发现 {} 个编译产物", root.display(), targets.len());
        for p in &targets {
            println!("  {}", p.display());
        }
        if apply {
            println!("[clean] ✅ 已删除 {} 个", removed);
        } else {
            println!("[clean] dry-run（未删除任何文件）；加 --apply 才真删");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 5) dialc completion：shell 补全脚本
// ---------------------------------------------------------------------------

pub fn completion(shell: &str) -> Result<(), String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
    let joined = names.join(" ");
    match shell {
        "bash" => {
            let _ = writeln!(out, "# dialc bash 补全（由 `dialc completion bash` 生成）");
            let _ = writeln!(out, "_dialc_complete() {{");
            let _ = writeln!(out, "  local cur=\"${{COMP_WORDS[COMP_CWORD]}}\"");
            let _ = writeln!(out, "  if [ \"$COMP_CWORD\" -eq 1 ]; then");
            let _ = writeln!(out, "    COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") )", joined);
            let _ = writeln!(out, "  else");
            let _ = writeln!(out, "    local sub=\"${{COMP_WORDS[1]}}\"");
            let _ = writeln!(out, "    case \"$sub\" in");
            for c in COMMANDS {
                if c.opts.is_empty() {
                    continue;
                }
                let _ = writeln!(out, "      {}) COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") ) ;;", c.name, c.opts.join(" "));
            }
            let _ = writeln!(out, "      *) COMPREPLY=() ;;");
            let _ = writeln!(out, "    esac");
            let _ = writeln!(out, "  fi");
            let _ = writeln!(out, "}}");
            let _ = writeln!(out, "complete -F _dialc_complete dialc");
        }
        "zsh" => {
            let _ = writeln!(out, "#compdef dialc");
            let _ = writeln!(out, "# dialc zsh 补全（由 `dialc completion zsh` 生成）");
            let _ = writeln!(out, "_dialc() {{");
            let _ = writeln!(out, "  local -a commands; commands=(");
            for c in COMMANDS {
                let _ = writeln!(out, "    '{}:{}'", c.name, c.brief.replace('\'', ""));
            }
            let _ = writeln!(out, "  )");
            let _ = writeln!(out, "  _describe 'command' commands");
            let _ = writeln!(out, "}}");
            let _ = writeln!(out, "compdef _dialc dialc");
        }
        "fish" => {
            let _ = writeln!(out, "# dialc fish 补全（由 `dialc completion fish` 生成）");
            let _ = writeln!(out, "complete -c dialc -f");
            for c in COMMANDS {
                let _ = writeln!(out, "complete -c dialc -n '__fish_use_subcommand' -a '{}' -d '{}'", c.name, c.brief.replace('\'', ""));
            }
            for c in COMMANDS {
                for o in c.opts {
                    let _ = writeln!(out, "complete -c dialc -n '__fish_seen_subcommand_from {}' -l '{}'", c.name, o.trim_start_matches('-'));
                }
            }
        }
        _ => return Err(format!("completion: 不支持 '{}'（可用 bash / zsh / fish）", shell)),
    }
    // 一次性写出：调用方 `| head` 提前关闭管道时只忽略一次写错误，
    // 不会像逐行 println! 那样触发 Broken pipe panic。
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
    Ok(())
}
