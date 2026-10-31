//! dialc 编译器入口（v0.3）。
//!
//! 用法：
//!   dialc build <源文件.dial> [-o 可执行文件] [--no-link]
//!   dialc run   <源文件.dial> [-o 可执行文件]
//!   dialc install <包.dialpkg>
//!   dialc lsp              （编辑器用的 Language Server，走 stdio，不由人手敲）
//!
//! 编译流水线（两步式）：
//!   1) 源码 -> 词法 -> 语法 -> 语义(含数据流/逃逸分析) -> inkwell 内存中构建 IR -> write_to_file 直接 emit .o
//!      （这一步需要本机 LLVM，但只产生机器码 .o，绝不依赖 libLLVM 运行库）
//!   2) 用 C 编译器（优先 zig cc，缺失时回退 clang）把 stdlib/ 与 runtime/ 下的 C 代码预编译为
//!      静态库 libdial_std.a，再把用户的 .o 与之链接，得到只依赖 C Runtime(libc) 的原生可执行文件。

mod ast;
mod codegen;
mod error;
mod index;
mod lexer;
mod lsp;
mod package;
mod parser;
mod semantic;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use codegen::SafetyMode;
use semantic::analyze;

fn main() {
    let args: Vec<String> = env::args().collect();

    // `dialc lsp`：Language Server。它不带文件参数，且**绝不能**往 stdout 打任何
    // 人类可读的提示——stdout 是协议通道，写进去一个字客户端就解析失败。
    // 因此它必须在下面「参数不足就打印用法」的检查之前先分流。
    if args.len() >= 2 && args[1] == "lsp" {
        if let Err(e) = lsp::run() {
            eprintln!("[dialc lsp] 退出：{}", e);
            std::process::exit(1);
        }
        return;
    }

    if args.len() < 3 {
        eprintln!("用法: dialc <build|run|install> <源文件.dial|包.dialpkg|包URL> [-o 输出] [--no-link]");
        eprintln!("      dialc install <URL> [--sha256 <哈希>]   从 URL 安装包（见 README §25.9）");
        std::process::exit(1);
    }
    let command = args[1].as_str();
    let src_path = args[2].clone();

    // `dialc install <包.dialpkg | URL>`：解压到全局缓存，项目目录零侵入（§25.1）。
    //
    // 包从哪来可以是**本地文件**，也可以是 **URL**（§3.1）：URL 会先下载、
    // 再做「整包哈希（若给了 --sha256）」与「解压前逐文件 SHA256」两道校验，
    // 全过才解压（§5.1：校验先于一切）。
    if command == "install" {
        // 解析可选的 --sha256 <hex>（只有 install 用得到）
        let mut expected_sha256: Option<String> = None;
        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--sha256" => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => expected_sha256 = Some(v.clone()),
                        None => {
                            eprintln!("--sha256 后面缺少哈希值");
                            std::process::exit(1);
                        }
                    }
                }
                other => {
                    eprintln!("install: 不认识的参数 '{}'", other);
                    std::process::exit(1);
                }
            }
            i += 1;
        }

        let is_url = src_path.starts_with("http://") || src_path.starts_with("https://");
        let result = if is_url {
            package::install_from_url(&src_path, expected_sha256.as_deref())
        } else {
            if expected_sha256.is_some() {
                eprintln!("--sha256 只对 URL 安装有意义（本地文件安装的校验由包内 manifest 负责）");
                std::process::exit(1);
            }
            package::install(Path::new(&src_path))
        };
        match result {
            Ok((dir, manifest)) => {
                println!("✅ 已安装包 '{}' v{}（ABI {}）", manifest.name, manifest.version, manifest.abi);
                println!("   许可: {}", manifest.license);
                println!("   位置: {}", dir.display());
                println!(
                    "   支持平台: {}",
                    manifest.supported_triples().join(", ")
                );
                // 装完就把「你能调什么」直接列出来。
                // `.diali` 里的 `///` 文档注释在这里第一次派上用场——
                // 否则用户装完包还得自己去找接口文件翻。
                if let Ok(iface_path) = package::verify_iface(&dir, &manifest) {
                    if let Ok(text) = fs::read_to_string(&iface_path) {
                        if let Ok(iface) = package::parse_diali(&text) {
                            if !iface.opaque_types.is_empty() {
                                println!("   不透明类型: {}", iface.opaque_types.join(", "));
                            }
                            println!("   导出函数 ({} 个):", iface.fns.len());
                            for f in &iface.fns {
                                let params = f
                                    .params
                                    .iter()
                                    .map(|(n, t)| format!("{}: {}", n, t))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                println!("     extern fn {}({}): {}", f.name, params, f.ret);
                                for d in &f.doc {
                                    println!("       /// {}", d);
                                }
                            }
                        }
                    }
                }
                return;
            }
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }

    if !matches!(command, "build" | "run") {
        eprintln!("未知命令 '{}'，仅支持 build / run / install", command);
        std::process::exit(1);
    }

    // 解析可选参数
    let mut out_path: Option<PathBuf> = None;
    let mut no_link = false;
    let mut safety = SafetyMode::Debug; // 默认：崩溃保护全开
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--no-link" => {
                no_link = true;
                i += 1;
            }
            // Fail-Fast 安全等级：debug（默认） / safe / fast
            "--safety" => {
                if i + 1 < args.len() {
                    safety = match args[i + 1].as_str() {
                        "debug" => SafetyMode::Debug,
                        "safe" => SafetyMode::ReleaseSafe,
                        "fast" => SafetyMode::ReleaseFast,
                        other => {
                            eprintln!("未知安全等级 '{}'（可选 debug / safe / fast）", other);
                            std::process::exit(1);
                        }
                    };
                    i += 2;
                } else {
                    eprintln!("选项 --safety 缺少等级（debug / safe / fast）");
                    std::process::exit(1);
                }
            }
            // 简写：--release == --safety safe，--fast == --safety fast
            "--release" => {
                safety = SafetyMode::ReleaseSafe;
                i += 1;
            }
            "--fast" => {
                safety = SafetyMode::ReleaseFast;
                i += 1;
            }
            "-o" => {
                if i + 1 < args.len() {
                    out_path = Some(PathBuf::from(args[i + 1].clone()));
                    i += 2;
                } else {
                    eprintln!("选项 -o 缺少输出路径");
                    std::process::exit(1);
                }
            }
            other => {
                eprintln!("未知参数 '{}'", other);
                i += 1;
            }
        }
    }

    // Fail-Fast 铁律的最后一道提醒：选择关闭崩溃保护，必须当场被告知代价。
    if safety == SafetyMode::ReleaseFast {
        eprintln!("⚠️  关于 --fast 模式（ReleaseFast）：");
        eprintln!("   此模式会关闭所有边界检查、除零检查和空指针保护，以换取极致的性能。");
        eprintln!("   除非你是正在编写底层核心算法、且完全确信你的数组下标绝对不会越界，");
        eprintln!("   否则请永远不要使用此模式。");
        eprintln!("   在 --fast 下发生的越界，将退化为 C 语言的未定义行为（UB），");
        eprintln!("   可能导致静默的错误数据甚至灾难性后果。");
        eprintln!();
    }

    let source = match fs::read_to_string(&src_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("无法读取源文件 {}: {}", src_path, e);
            std::process::exit(1);
        }
    };

    // 1) 词法
    let tokens = match lexer::lex(&source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // 2) 语法（**两阶段**解析）
    //
    //    阶段一：先解析一遍，只为拿到 `import` 列表；
    //    阶段二：解析完这些包（从而知道它们声明了哪些不透明类型名）之后再重新解析，
    //            这样 `var h: DemoHandle = demo_new(1);` 里的包类型名
    //            在**语法阶段**就能被识别为 Type::Opaque，而不是被误当成结构体。
    //    多解析一遍的成本可以忽略；换来的是类型名从一开始就正确。
    let probe = match parser::parse(tokens.clone()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // 2.5) 解析 import：定位包 -> 校验哈希 -> 读 .diali 签名 -> 选出本平台机器码
    //      编译器在这里替用户做完**全部**决策：不写路径、不写平台、不写版本。
    let (externs, pkg_libs, opaque_types) = match resolve_imports(&probe.imports) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    let mut program = match parser::parse_with(tokens, &opaque_types) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // 3) 语义 + 数据流/逃逸分析
    let (functions, structs) = match analyze(&mut program, &externs) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // 4) 代码生成（第一步：emit .o，需在 LLVM 存在时执行）
    let (obj_path, exe_path) = output_paths(&src_path, out_path.as_deref());
    if let Err(e) = codegen::compile(&program, &functions, &structs, &obj_path, safety, &externs) {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    if no_link {
        println!("✅ 已生成目标文件（未链接）: {}", obj_path.display());
        return;
    }

    // 5) 链接（第二步：用 C 编译器把 C 标准库做成静态库并链接）
    //    同一个安全等级必须同时作用于 LLVM IR 与 C Runtime，否则两侧检查会不一致。
    if let Err(e) = link(&obj_path, &exe_path, safety, &pkg_libs) {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    match command {
        "build" => {
            println!("✅ 已生成可执行文件: {}", exe_path.display());
        }
        "run" => match Command::new(&exe_path).status() {
            Ok(status) => {
                std::process::exit(status.code().unwrap_or(0));
            }
            Err(e) => {
                eprintln!("运行失败: {}", e);
                std::process::exit(1);
            }
        },
        _ => unreachable!(),
    }
}

/// 解析 `import` 列表：定位已安装的包 → 校验 → 读 `.diali` 签名 → 选出本平台机器码。
///
/// 返回 `(外部函数表, 待链接的静态库列表)`。
///
/// 这一步是 ARCHITECTURE.md §4.2「包处理流水线」的 ②③④：
/// 读 manifest → 按 Triple 查映射字典 → 校验哈希 → 读 .diali 喂给 Sema；
/// 第 ⑤ 步（把库加进链接命令）在 `link` 里完成。
pub fn resolve_imports(
    imports: &[String],
) -> Result<(Vec<(String, semantic::FuncInfo)>, Vec<PathBuf>, Vec<String>), String> {
    let mut externs: Vec<(String, semantic::FuncInfo)> = Vec::new();
    let mut libs: Vec<PathBuf> = Vec::new();
    let mut opaque: Vec<String> = Vec::new();
    if imports.is_empty() {
        return Ok((externs, libs, opaque));
    }

    let triple = package::host_triple();
    for name in imports {
        let found = package::find_installed(name).map_err(|e| e.to_string())?;
        let (dir, manifest) = match found {
            Some(x) => x,
            None => {
                let root = package::cache_root().map_err(|e| e.to_string())?;
                return Err(format!(
                    "找不到已安装的包 '{}'（缓存目录: {}）。\
                     请先用 `dialc install <包.dialpkg>` 安装，或把 .dialpkg 放到项目目录",
                    name,
                    root.display()
                ));
            }
        };

        // §25.8：让许可**可见**。dialc 不替用户做法务判断，但必须回显到构建日志。
        println!(
            "ℹ️  包 '{}' v{}（ABI {}，许可 {}）",
            manifest.name, manifest.version, manifest.abi, manifest.license
        );

        // 校验 .diali 的哈希（被篡改的签名会让类型检查建立在谎言之上）
        let iface_path = package::verify_iface(&dir, &manifest).map_err(|e| e.to_string())?;
        let iface_text = fs::read_to_string(&iface_path)
            .map_err(|e| format!("读取 {} 失败: {}", iface_path.display(), e))?;
        let iface = package::parse_diali(&iface_text).map_err(|e| e.to_string())?;

        // 包声明的不透明句柄名：源码里可以直接当类型名用（如 DemoHandle）
        for t in &iface.opaque_types {
            if opaque.iter().any(|o| o == t) {
                return Err(format!(
                    "包 '{}' 声明的不透明类型 '{}' 与另一个包重名；当前版本要求包名之间类型名不冲突",
                    manifest.name, t
                ));
            }
            opaque.push(t.clone());
        }

        // .diali 的签名 -> Sema 认识的函数表
        for f in &iface.fns {
            let mut params = Vec::new();
            for (_pn, pt) in &f.params {
                let t = package::resolve_type_name(pt, &iface.opaque_types).map_err(|e| e.to_string())?;
                params.push(t);
            }
            let ret = package::resolve_type_name(&f.ret, &iface.opaque_types)
                .map_err(|e| e.to_string())?;
            externs.push((
                f.name.clone(),
                semantic::FuncInfo { params, ret },
            ));
        }

        // 按当前 Triple 选库，并校验该份机器码的哈希
        let lib = package::resolve_lib(&dir, &manifest).map_err(|e| e.to_string())?;
        println!(
            "ℹ️  链接 {} -> {}（目标 {}）",
            manifest.name,
            lib.display(),
            triple
        );
        libs.push(lib);
    }

    Ok((externs, libs, opaque))
}

/// 根据源文件路径推导目标文件与可执行文件路径（同目录、同名）。
fn output_paths(src_path: &str, out: Option<&Path>) -> (PathBuf, PathBuf) {
    let src = Path::new(src_path);
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "a".to_string());
    let obj = parent.join(format!("{}.o", stem));
    let exe = match out {
        Some(p) => p.to_path_buf(),
        None => parent.join(&stem),
    };
    (obj, exe)
}

/// 定位工程根目录（包含 stdlib/ 与 runtime/）。依次尝试若干候选基址。
fn find_project_base() -> Option<PathBuf> {
    if let Ok(exe) = env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            for candidate in [
                exe_dir.join("../../"),     // target/release/dialc -> 工程根
                exe_dir.join("../"),        // 同目录上层
                PathBuf::from("."),         // 当前目录
            ] {
                let c = if candidate.is_absolute() {
                    candidate
                } else {
                    PathBuf::from(env::current_dir().ok()?).join(candidate)
                };
                if c.join("stdlib/dial_string.c").exists() {
                    return Some(c);
                }
            }
        }
    }
    if Path::new("stdlib/dial_string.c").exists() {
        return Some(PathBuf::from("."));
    }
    None
}

/// 返回 (程序名, 前置参数)：优先 zig cc，缺失则 clang。
fn cc_command() -> (String, Vec<String>) {
    if Command::new("zig")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        ("zig".to_string(), vec!["cc".to_string()])
    } else {
        ("clang".to_string(), vec![])
    }
}

/// 第二步：编译 C 标准库/运行时为静态库 libdial_std.a，再与用户 .o 链接。
/// `safety` 通过 -DDIAL_SAFETY=N 注入，决定 C 侧的边界/断言检查是否编译进去。
fn link(obj: &Path, exe: &Path, safety: SafetyMode, extra_libs: &[PathBuf]) -> Result<(), String> {
    let base = find_project_base()
        .ok_or_else(|| "找不到 stdlib/ 与 runtime/ 目录（请用 DIALC_STDLIB_DIR 指定工程根）".to_string())?;
    let runtime_dir = base.join("runtime");
    let stdlib_dir = base.join("stdlib");

    // 临时构建目录（按安全等级分目录，避免不同等级的 .o 互相污染）
    let build_dir = base
        .join(".dialc_build")
        .join(format!("safety-{}", safety.as_c_macro()));
    fs::create_dir_all(&build_dir).map_err(|e| format!("创建构建目录失败: {}", e))?;

    let (cc, cc_prefix) = cc_command();
    println!(
        "ℹ️  C 编译器: {} {}（编译 stdlib/ 与 runtime/ 为 libdial_std.a）",
        cc,
        cc_prefix.join(" ")
    );
    println!(
        "ℹ️  安全等级: {:?}（-DDIAL_SAFETY={}）",
        safety,
        safety.as_c_macro()
    );

    // 编译所有 C 源文件 -> .o
    let mut obj_files: Vec<PathBuf> = Vec::new();
    let mut entries: Vec<PathBuf> = Vec::new();
    for dir in [&runtime_dir, &stdlib_dir] {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "c").unwrap_or(false) {
                    entries.push(p);
                }
            }
        }
    }
    for cfile in &entries {
        let out_o = build_dir.join(cfile.file_stem().unwrap()).with_extension("o");
        let mut cmd = Command::new(&cc);
        cmd.args(&cc_prefix)
            .arg("-O2")
            // 并发运行时（stdlib/dial_thread.c、dial_channel.c）用到 pthread：
            // 必须让编译器生成线程安全的代码（并暴露 _REENTRANT 相关声明）。
            .arg("-pthread")
            // Fail-Fast 等级注入 C Runtime：等级 0 时检查代码被编译器完全消除
            .arg(format!("-DDIAL_SAFETY={}", safety.as_c_macro()))
            .arg("-I")
            .arg(&runtime_dir)
            .arg("-I")
            .arg(&stdlib_dir)
            .arg("-c")
            .arg(cfile)
            .arg("-o")
            .arg(&out_o);
        if !run_ok(&mut cmd) {
            return Err(format!("编译 C 源文件失败: {}", cfile.display()));
        }
        obj_files.push(out_o);
    }

    // 归档为静态库 libdial_std.a
    let lib = build_dir.join("libdial_std.a");
    let mut ar = Command::new("ar");
    ar.arg("rcs").arg(&lib);
    for o in &obj_files {
        ar.arg(o);
    }
    if !run_ok(&mut ar) {
        return Err("归档静态库 libdial_std.a 失败（请确认 ar 可用）".to_string());
    }

    // 链接：用户 .o + libdial_std.a -> 可执行文件（仅依赖 libc）
    let mut link_cmd = Command::new(&cc);
    link_cmd
        .args(&cc_prefix)
        .arg(obj)
        .arg(&lib);
    // import 引入的包的机器码：**按 Target Triple 选出的那一份**静态库。
    // 交叉编译到 Windows 时这里自动变成包内的 .lib，无需用户干预（§4.3）。
    for pl in extra_libs {
        link_cmd.arg(pl);
    }
    link_cmd
        // 线程库：glibc ≥ 2.34 已并入 libc（此处是空操作），
        // 保留是为了在老 glibc 上仍能链接，不做任何代价。
        .arg("-lpthread")
        .arg("-o")
        .arg(exe);
    if !run_ok(&mut link_cmd) {
        return Err(format!("链接失败：无法用 {} 生成 {}", cc, exe.display()));
    }

    Ok(())
}

/// 运行命令并返回是否成功退出。
fn run_ok(cmd: &mut Command) -> bool {
    cmd.status().map(|s| s.success()).unwrap_or(false)
}
