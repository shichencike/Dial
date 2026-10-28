//! dialc 编译器入口（v0.3）。
//!
//! 用法：
//!   dialc build <源文件.dial> [-o 可执行文件] [--no-link]
//!   dialc run   <源文件.dial> [-o 可执行文件]
//!
//! 编译流水线（两步式）：
//!   1) 源码 -> 词法 -> 语法 -> 语义(含数据流/逃逸分析) -> inkwell 内存中构建 IR -> write_to_file 直接 emit .o
//!      （这一步需要本机 LLVM，但只产生机器码 .o，绝不依赖 libLLVM 运行库）
//!   2) 用 C 编译器（优先 zig cc，缺失时回退 clang）把 stdlib/ 与 runtime/ 下的 C 代码预编译为
//!      静态库 libdial_std.a，再把用户的 .o 与之链接，得到只依赖 C Runtime(libc) 的原生可执行文件。

mod ast;
mod codegen;
mod error;
mod lexer;
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
    if args.len() < 3 {
        eprintln!("用法: dialc <build|run> <源文件.dial> [-o 输出] [--no-link]");
        std::process::exit(1);
    }
    let command = args[1].as_str();
    let src_path = args[2].clone();

    if !matches!(command, "build" | "run") {
        eprintln!("未知命令 '{}'，仅支持 build / run", command);
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

    // 2) 语法
    let mut program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // 3) 语义 + 数据流/逃逸分析
    let (functions, structs) = match analyze(&mut program) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // 4) 代码生成（第一步：emit .o，需在 LLVM 存在时执行）
    let (obj_path, exe_path) = output_paths(&src_path, out_path.as_deref());
    if let Err(e) = codegen::compile(&program, &functions, &structs, &obj_path, safety) {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    if no_link {
        println!("✅ 已生成目标文件（未链接）: {}", obj_path.display());
        return;
    }

    // 5) 链接（第二步：用 C 编译器把 C 标准库做成静态库并链接）
    //    同一个安全等级必须同时作用于 LLVM IR 与 C Runtime，否则两侧检查会不一致。
    if let Err(e) = link(&obj_path, &exe_path, safety) {
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
fn link(obj: &Path, exe: &Path, safety: SafetyMode) -> Result<(), String> {
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
        .arg(&lib)
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
