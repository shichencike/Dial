//! dialc 编译器入口（v0.3）。
//!
//! 用法：
//!   dialc build <源文件.dial> [-o 可执行文件] [--no-link]
//!   dialc run   <源文件.dial> [-o 可执行文件]
//!   dialc install <包.dialpkg>
//!   dialc version | --version | -V      （显示版本号）
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

    // `dialc version` / `dialc --version` / `dialc -V`：显示版本号。
    //
    // 版本号取自 Cargo.toml 的 `version` 字段（编译期由 env! 注入）——
    // **全站唯一真相**。任何地方都不许再手写一份版本字符串：
    // 手写的版本号必然随发布漂移（这仓库已经因为硬编码漂过三轮）。
    //
    // 放在「参数不足就打印用法」的检查之前：版本查询本来就不带文件参数。
    // 必须在 lsp 分流**之后**：lsp 的 stdout 是协议通道，
    // 版本信息绝不能混进去（会破坏 JSON-RPC 帧）。
    if args.len() >= 2 && matches!(args[1].as_str(), "version" | "--version" | "-V") {
        println!("dialc {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.len() < 3 {
        eprintln!("用法: dialc <build|run|install> <源文件.dial|包.dialpkg|包URL> [-o 输出] [--no-link]");
        eprintln!("      dialc version | --version | -V   显示版本号");
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
    let mut pgo = false; // 剖析引导优化（--fast 下的终极杀器）
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
            "--pgo" => {
                pgo = true;
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
        eprintln!("   --fast 同时开启三层编译器终极优化：");
        eprintln!("     ① 自动向量化(SIMD)+循环展开：LLVM 用 O3 + 宿主 SIMD 指令");
        eprintln!("       （x86 的 AVX2 / ARM64 的 NEON）一次处理多个整数，并展开循环减少分支。");
        eprintln!("     ② LTO 链接时优化：把你的 Dial 代码与 C 运行时的机器码整体重新打散，");
        eprintln!("       把 dial_array_get 这类热函数直接内联进主程序，消灭函数调用开销。");
        if pgo {
            eprintln!("     ③ PGO 剖析引导优化：先插桩跑一遍训练样本，记录最热分支，");
            eprintln!("       再据此把热路径排到 I-Cache 最前、冷错误处理后置，分支预测命中率拉满。");
        } else {
            eprintln!("     ③ PGO 剖析引导优化：追加 --pgo 可再榨一层（需先跑一遍训练样本）。");
        }
        eprintln!();
    }
    if pgo && safety != SafetyMode::ReleaseFast {
        eprintln!("ℹ️  --pgo（剖析引导优化）必须配合 --fast 使用：");
        eprintln!("   非 fast 模式保留运行期检查，PGO 无收益且不插桩。已自动忽略 --pgo。");
        eprintln!();
        pgo = false;
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
    let (externs, pkg_libs, opaque_types) = match resolve_imports(&probe.imports, &mut |m| {
        println!("{}", m)
    }) {
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
    //
    // --fast 模式（ReleaseFast）：不仅关闭运行期检查，还拉满 LLVM 优化（O3 + 宿主 SIMD），
    // 并额外导出 LLVM bitcode（.bc），供后续 LTO / PGO 流水线把 Dial 代码与 C 运行时整体重排。
    let fast = safety == SafetyMode::ReleaseFast;
    let (obj_path, exe_path) = output_paths(&src_path, out_path.as_deref());
    let build_dir = find_project_base()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dialc_build")
        .join(format!("safety-{}", safety.as_c_macro()));
    let bc_path = build_dir.join(obj_path.file_stem().unwrap()).with_extension("bc");
    // --fast（ReleaseFast）会把 Dial 代码导出为 LLVM bitcode（.bc）供 LTO 重排；
    // 必须先把父目录建好，否则 write_bitcode_to_path 在全新检出（无残留 .dialc_build/）时
    // 会因目录不存在而静默失败，进而让所有 --fast 构建在 CI 上崩溃。
    if fast {
        if let Err(e) = fs::create_dir_all(&build_dir) {
            eprintln!("创建 bitcode 构建目录失败: {}", e);
            std::process::exit(1);
        }
    }
    let cg_opts = codegen::CgOpts {
        safety,
        aggressive: fast,
        bc_out: if fast { Some(bc_path.clone()) } else { None },
    };
    if let Err(e) = codegen::compile(
        &program,
        &functions,
        &structs,
        &obj_path,
        &cg_opts,
        &externs,
    ) {
        eprintln!("{}", e);
        std::process::exit(1);
    }

    if no_link {
        println!("✅ 已生成目标文件（未链接）: {}", obj_path.display());
        return;
    }

    // 5) 链接（第二步：用 C 编译器把 C 标准库做成静态库并链接）
    //    同一个安全等级必须同时作用于 LLVM IR 与 C Runtime，否则两侧检查会不一致。
    //
    // LTO / PGO 需要 clang 支持（-flto 与 IR 级 PGO 的 bitcode 格式是 clang 的）。
    // 若用户强制指定了非 clang 的 C 编译器，则优雅降级：保留 O3/SIMD 的 LLVM 侧收益，
    // 但放弃跨模块的 LTO 内联与 PGO（这两步依赖 clang 的 bitcode 链路）。
    let lto_capable = fast && clang_for_lto().is_some();
    let eff_pgo = pgo && lto_capable;
    if fast && !lto_capable && pgo {
        eprintln!("⚠️  当前 C 编译器不支持 LTO/PGO（需要 clang），已降级为「仅 O3 + SIMD」的 --fast。");
    }

    if lto_capable {
        if eff_pgo {
            // —— PGO 两段式 ——
            // 阶段一（instrument）：用插桩 bitcode 编出训练用二进制，跑一遍收集热点，
            //   再把 profile 合并成 profdata。
            // 阶段二（optimize）：用 profdata 重新编译，把热路径排到 I-Cache 最前。
            let profdir = build_dir.join("pgo");
            let _ = fs::create_dir_all(&profdir);
            let gen_exe = build_dir.join("pgo_gen_bin");

            let mut gen = LinkOpts::default();
            gen.lto = true;
            gen.pgo_gen = true;
            if let Err(e) = clang_compile_bc(&bc_path, &obj_path, &gen) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            if let Err(e) = link(&obj_path, &gen_exe, safety, &pkg_libs, &gen) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            // 训练运行：把 profile 落到 profdir 下，文件名带 PID 以免多次运行互相覆盖。
            // 训练输出静默（仅做采样），最终优化后的运行才把真实结果打到终端，避免重复。
            println!("ℹ️  PGO：运行训练样本以收集热点分支（训练输出已静默）…");
            let mut run = Command::new(&gen_exe);
            run.env(
                "LLVM_PROFILE_FILE",
                profdir.join("%p.profraw").to_string_lossy().into_owned(),
            )
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
            let _train_status = run.status();
            let profdata = profdir.join("pgo.profdata");
            // 合并 profraw -> profdata（即便训练进程退出码非 0，只要有数据就继续）。
            let mut merge = Command::new("llvm-profdata");
            merge.arg("merge").arg("-o").arg(&profdata);
            if let Ok(rd) = fs::read_dir(&profdir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.extension().map(|x| x == "profraw").unwrap_or(false) {
                        merge.arg(&p);
                    }
                }
            }
            if !run_ok(&mut merge) {
                eprintln!("❌ PGO：合并 profile 数据失败（程序是否正常运行产出热点？）");
                std::process::exit(1);
            }
            if !profdata.exists() {
                eprintln!("❌ PGO：未生成任何 profile 数据，请确认程序在训练运行中确实执行了热路径。");
                std::process::exit(1);
            }

            // 阶段二：用 profdata 重新编译并链接出最终二进制。
            let mut useo = LinkOpts::default();
            useo.lto = true;
            useo.pgo_use = Some(profdata.clone());
            if let Err(e) = clang_compile_bc(&bc_path, &obj_path, &useo) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            if let Err(e) = link(&obj_path, &exe_path, safety, &pkg_libs, &useo) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            println!("✅ PGO 优化完成（热路径已据训练样本重排）。");
        } else {
            // 单阶段 LTO：把 .bc 经 clang -flto 编成胖对象，再整体链接。
            let mut o = LinkOpts::default();
            o.lto = true;
            if let Err(e) = clang_compile_bc(&bc_path, &obj_path, &o) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            if let Err(e) = link(&obj_path, &exe_path, safety, &pkg_libs, &o) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    } else {
        // 非 LTO 模式（debug / safe，或 fast 但无 clang）：直接用 inkwell 写出的原生 .o 链接。
        if let Err(e) = link(&obj_path, &exe_path, safety, &pkg_libs, &LinkOpts::default()) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
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

/// 「找不到已安装的包」的标准文案。
///
/// **CLI 与 LSP 必须用同一句**：否则用户在编辑器里看到的提示与命令行看到的对不上，
/// 反而更困惑。LSP 侧还会给它挂上稳定错误码 `dialc.package.missing`。
pub fn missing_package_message(name: &str) -> String {
    let root = package::cache_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.dial/packages".to_string());
    format!(
        "找不到已安装的包 '{}'（缓存目录: {}）。\
         请先用 `dialc install <包.dialpkg>` 安装，或把 .dialpkg 放到项目目录",
        name, root
    )
}

/// 只问「包装了没」，不做校验、不读 `.diali`。
///
/// LSP 需要它来给出「找不到已安装的包 'x'」的诊断：`resolve_imports` 的错误只是一个
/// 字符串，丢了「是哪个包」这个关键信息；这里单独问一次，答案才是结构化的。
///
/// 只报**第一个**缺失的包——一次报十个缺失包，等于没报。
pub fn first_missing_package(imports: &[String]) -> Option<String> {
    for name in imports {
        match package::find_installed(name) {
            Ok(Some(_)) => {}
            _ => return Some(name.clone()),
        }
    }
    None
}

/// 解析 `import` 列表：定位已安装的包 → 校验 → 读 `.diali` 签名 → 选出本平台机器码。
///
/// 返回 `(外部函数表, 待链接的静态库列表)`。
///
/// 这一步是 ARCHITECTURE.md §4.2「包处理流水线」的 ②③④：
/// 读 manifest → 按 Triple 查映射字典 → 校验哈希 → 读 .diali 喂给 Sema；
/// 第 ⑤ 步（把库加进链接命令）在 `link` 里完成。
///
/// **日志去向由调用方注入**（`log`）：本函数**绝不**自己往 stdout 写东西。
/// 原因：`dialc lsp` 的 stdout 是 LSP 协议流，混入任何非协议字节都会让客户端
/// 的分帧解析错位（实测表现为「一打开 import 了包的文档就卡死」）。
/// CLI 构建传 stdout（构建日志），LSP 传 `window/logMessage` 通知——由调用方决定，
/// 函数本身无权替调用方选往哪儿写。
pub fn resolve_imports(
    imports: &[String],
    log: &mut dyn FnMut(&str),
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
                return Err(missing_package_message(name));
            }
        };

        // §25.8：让许可**可见**。dialc 不替用户做法务判断，但必须回显到构建日志。
        log(&format!(
            "ℹ️  包 '{}' v{}（ABI {}，许可 {}）",
            manifest.name, manifest.version, manifest.abi, manifest.license
        ));

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
                semantic::FuncInfo {
                    defaults: vec![None; params.len()],
                    params,
                    ret,
                },
            ));
        }

        // 按当前 Triple 选库，并校验该份机器码的哈希
        let lib = package::resolve_lib(&dir, &manifest).map_err(|e| e.to_string())?;
        log(&format!(
            "ℹ️  链接 {} -> {}（目标 {}）",
            manifest.name,
            lib.display(),
            triple
        ));
        libs.push(lib);
    }

    Ok((externs, libs, opaque))
}

/// 根据源文件路径推导目标文件与可执行文件路径（同目录、同名）。
fn output_paths(src_path: &str, out: Option<&Path>) -> (PathBuf, PathBuf) {
    let src = Path::new(src_path);
    // parent() 对裸文件名（如 "hello.dial"）返回 Some("") 而非 None，
    // 直接 join 会得到 "hello"（不含 "./"），Linux 的 execve 会去 PATH 里找 -> "No such file or directory"，
    // 更糟的是 "test.dial" 会静默执行系统 /bin/test（CVE-2024-DIAL-002 根因）。
    // 这里把空 parent 归一为 "."，保证 join 出 "./hello" 这种明确相对路径。
    let parent_raw = src.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent_raw.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent_raw
    };
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "a".to_string());
    let obj = parent.join(format!("{}.o", stem));
    let exe = match out {
        Some(p) => p.to_path_buf(),
        None => parent.join(&stem),
    };
    // Windows 可执行文件需要 .exe 后缀；显式给了 -o 则尊重用户（除非它已带后缀）。
    let exe = if cfg!(windows) && exe.extension().is_none() {
        exe.with_extension("exe")
    } else {
        exe
    };
    (obj, exe)
}

/// 定位工程根目录（包含 stdlib/ 与 runtime/）。依次尝试若干候选基址。
fn find_project_base() -> Option<PathBuf> {
    // 1) 显式环境变量优先：支持源码树外自定义布局（例如把 stdlib/ 与 runtime/ 单独部署）。
    if let Ok(dir) = env::var("DIALC_STDLIB_DIR") {
        let p = PathBuf::from(dir);
        if p.join("stdlib/dial_string.c").exists() {
            return Some(p);
        }
    }
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

/// 返回 (程序名, 前置参数)：按平台挑选可用的 C 编译器。
///
/// - Windows（MinGW）：优先 `gcc`（MSYS2 mingw64 的 `x86_64-w64-mingw32-gcc`），
///   其次 `clang` / `zig cc` / `cc`。MinGW 自带 winpthreads，故 stdlib 的 pthread
///   代码无需改动即可编译（阶段 6 的 Win32 后端因此可延后）。
/// - 其它平台：优先 `zig cc`（统一交叉能力），其次 `clang` / `cc` / `gcc`。
fn cc_command() -> (String, Vec<String>) {
    // 允许用户用 DIAL_C_COMPILER 强制指定 C 编译器（CVE-2024-DIAL-008 新增）。
    // Termux / 某些交叉环境下默认的 `zig cc` 会失败，用户可设为 `clang`、`gcc`，
    // 甚至带参数的 `zig cc --target=...`（空格分隔程序与参数）。
    if let Ok(val) = env::var("DIAL_C_COMPILER") {
        let val = val.trim();
        if !val.is_empty() {
            let mut parts = val.split_whitespace();
            if let Some(prog) = parts.next() {
                let args: Vec<String> = parts.map(|s| s.to_string()).collect();
                return (prog.to_string(), args);
            }
        }
    }
    let candidates: Vec<(&'static str, Vec<String>)> = if cfg!(windows) {
        vec![
            ("gcc", vec![]),
            ("clang", vec![]),
            ("zig", vec!["cc".to_string()]),
            ("cc", vec![]),
        ]
    } else {
        vec![
            ("zig", vec!["cc".to_string()]),
            ("clang", vec![]),
            ("cc", vec![]),
            ("gcc", vec![]),
        ]
    };
    for (prog, args) in candidates {
        // 不同编译器识别的版本参数不完全一致，两种都试一下
        let ok = Command::new(prog)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            || Command::new(prog)
                .arg("version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        if ok {
            return (prog.to_string(), args);
        }
    }
    ("cc".to_string(), vec![])
}

/// 链接阶段的优化选项。
///
/// - `lto`：链接时优化。要求用户 .o 为「含 LTO bitcode 的胖对象」（由
///   `clang_compile_bc` 用 `clang -flto` 编译 `.bc` 得到），且 C 运行时也以 `-flto`
///   编译成 .o（而非 .a），链接时由 clang 重新打散、跨 Dial+C 整体内联。
/// - `pgo_gen` / `pgo_use`：剖析引导优化的插桩阶段 / 使用阶段。
#[derive(Clone, Default)]
struct LinkOpts {
    lto: bool,
    pgo_gen: bool,
    pgo_use: Option<PathBuf>,
}

/// 返回可用于 LTO / PGO 的 clang（程序名, 前置参数）。
///
/// LTO 的 bitcode 格式是 clang 的，`-flto` 跨语言重排也依赖 clang 驱动；
/// 若强制指定的 `DIAL_C_COMPILER` 不是 clang，则返回 None（调用方优雅降级为「仅 O3+SIMD」）。
fn clang_for_lto() -> Option<(String, Vec<String>)> {
    if let Ok(val) = env::var("DIAL_C_COMPILER") {
        let val = val.trim();
        if !val.is_empty() {
            let mut parts = val.split_whitespace();
            if let Some(prog) = parts.next() {
                if !prog.contains("clang") {
                    return None;
                }
                return Some((prog.to_string(), parts.map(|s| s.to_string()).collect()));
            }
        }
    }
    // 探测系统 clang（含版本参数两种写法）
    for prog in ["clang", "clang-18", "clang-17", "clang-16"] {
        let ok = Command::new(prog)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some((prog.to_string(), vec![]));
        }
    }
    None
}

/// 把 inkwell 导出的 LLVM bitcode 经 clang `-flto -O3` 编成「胖对象」。
///
/// 这一步是关键：只有 clang 编出的 .o 才内嵌 LTO bitcode，链接器才能据此把 Dial 代码
/// 与 C 运行时整体重排（内联 `dial_array_get` 等热函数）。PGO 阶段则附带插桩 / 使用标记。
fn clang_compile_bc(bc: &Path, obj: &Path, opts: &LinkOpts) -> Result<(), String> {
    let (clang, prefix) = clang_for_lto()
        .ok_or_else(|| "LTO/PGO 需要 clang，但当前环境未找到 clang".to_string())?;
    let mut cmd = Command::new(&clang);
    cmd.args(&prefix)
        .arg("-flto")
        .arg("-O3")
        .arg("-march=native") // 与 inkwell 的 host_cpu() 对齐：让 C 运行时也吃到本机 SIMD
        .arg("-Wno-override-module") // .bc 的 triple 与 clang 默认目标一致，仅消除噪音告警
        .arg("-c")
        .arg(bc)
        .arg("-o")
        .arg(obj);
    if opts.pgo_gen {
        cmd.arg("-fprofile-generate");
    }
    if let Some(pd) = &opts.pgo_use {
        cmd
            .arg(format!("-fprofile-use={}", pd.display()))
            // PGO 数据过期 / 未覆盖的分支只是提示，不应升级成硬错误阻断构建。
            .arg("-Wno-error=profile-instr-out-of-date")
            .arg("-Wno-error=profile-instr-unprofiled");
    }
    if !run_ok(&mut cmd) {
        return Err(format!("编译 bitcode 为 LTO 对象失败: {}", bc.display()));
    }
    Ok(())
}

/// 第二步：把标准库（runtime + stdlib）链入最终可执行文件。
///
/// 阶段 7（官方标准库 `.dialpkg` 化）之后，标准库优先使用**已安装**的
/// `stdlib.dialpkg`（路径 `~/.dial/packages/stdlib/`）里的预编译静态库；
/// 若未安装或不含当前平台的机器码，则**回退**到实时编译 `stdlib/` + `runtime/`
/// 源码（向后兼容 / 离线 / 开发构建）。两种方式产出的都是同一份 `libdial_std.a`，
/// 最终链接步骤完全一致。
///
/// LTO 模式（`opts.lto`）下，为让链接器能跨 Dial+C 整体重排，**放弃**预编译包与 .a 归档，
/// 改为把 C 运行时以 `-flto -O3` 逐文件编成 .o，再与用户胖对象一起交给 clang 链接
/// （这样 `dial_array_get` 等热函数才能被内联进主程序，彻底消灭调用开销）。
///
/// `safety` 通过 -DDIAL_SAFETY=N 注入，仅在「回退源码编译」路径下生效
/// （预编译包已按固定等级烘焙，见 ARCHITECTURE §8 阶段 7：默认 ReleaseSafe=1）。
fn link(
    obj: &Path,
    exe: &Path,
    safety: SafetyMode,
    extra_libs: &[PathBuf],
    opts: &LinkOpts,
) -> Result<(), String> {
    if opts.lto {
        // LTO：C 运行时逐文件编成 LTO .o，与用户胖对象一起整体重排。
        let runtime_objs = compile_runtime_lto(safety, opts)?;
        final_link(obj, exe, extra_libs, &[], &runtime_objs, opts)
    } else {
        // 非 LTO：优先已安装包，否则实时编译源码归档成 .a。
        let stdlib_libs = resolve_stdlib_archive(safety)?;
        final_link(obj, exe, extra_libs, &stdlib_libs, &[], opts)
    }
}

/// 解析标准库静态库。
///
/// - 若已安装 `stdlib` 包且 `abi` 匹配：按当前 Target Triple 选出那一份 `.a`
///   （并强制校验其 SHA256）；
/// - 若 `abi` 不匹配：明确报错并给出重装方案（绝不静默链入不兼容二进制）；
/// - 若未安装 / 当前平台不在包支持列表：回退到编译 `stdlib/` + `runtime/` 源码。
///
/// 返回的标准库归档列表（通常一份 `libdial_std.a`）交给 `final_link` 链接。
fn resolve_stdlib_archive(safety: SafetyMode) -> Result<Vec<PathBuf>, String> {
    if let Some((dir, manifest)) = package::find_installed("stdlib").map_err(|e| e.to_string())? {
        // ABI 兜底：不兼容即报错 + 重装方案，绝不静默崩溃。
        if manifest.abi != package::STDLIB_ABI {
            return Err(format!(
                "已安装的标准库 'stdlib' ABI 不兼容：包 abi={}，编译器要求 abi={}。\n\
                 为避免不兼容的二进制悄悄链入程序，已中断链接。\n\
                 请先卸载旧包再安装匹配版本：\n\
                   rm -rf ~/.dial/packages/stdlib\n\
                 然后重新运行安装脚本，或执行：dialc install <对应版本的 stdlib.dialpkg>",
                manifest.abi, package::STDLIB_ABI
            ));
        }
        // 按当前 Triple 选库（内部校验该份机器码的 SHA256）
        match package::resolve_lib(&dir, &manifest) {
            Ok(lib) => {
                println!(
                    "ℹ️  使用已安装的标准库包: {} v{}（ABI {}，目标 {}）",
                    manifest.name,
                    manifest.version,
                    manifest.abi,
                    package::host_triple()
                );
                return Ok(vec![lib]);
            }
            Err(e) => {
                // 包存在但当前平台不在其 targets 列表：退化为源码编译，保持兼容。
                println!(
                    "⚠️  已安装的标准库包不含当前平台 {} 的机器码，回退源码编译：{}",
                    package::host_triple(),
                    e
                );
            }
        }
    }

    // ---- 回退：实时编译 runtime/ + stdlib/ 源码 → libdial_std.a ----
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

    Ok(vec![lib])
}

/// 最终链接：用户 .o + 标准库 .a（可能多份）+ C 运行时 .o（LTO 时）+ import 包 .a + 系统库 → 可执行文件。
///
/// - 非 LTO：`stdlib_libs` 是归档 `.a`，直接链入。
/// - LTO：`runtime_objs` 是逐文件编出的 C 运行时 `.o`（内嵌 LTO bitcode），
///   与用户胖对象一起交给 **clang** 整体重排；此时 `stdlib_libs` 为空。
/// `opts.lto` / `opts.pgo_*` 决定链接器是否追加 `-flto -O3` 与 PGO 标记。
fn final_link(
    obj: &Path,
    exe: &Path,
    extra_libs: &[PathBuf],
    stdlib_libs: &[PathBuf],
    runtime_objs: &[PathBuf],
    opts: &LinkOpts,
) -> Result<(), String> {
    // LTO/PGO 依赖 clang 的 bitcode 链路，链接驱动也必须用 clang。
    let (cc, cc_prefix) = if opts.lto {
        clang_for_lto().ok_or_else(|| "LTO/PGO 需要 clang 作为链接驱动".to_string())?
    } else {
        cc_command()
    };
    let mut link_cmd = Command::new(&cc);
    link_cmd.args(&cc_prefix).arg(obj);
    // C 运行时 .o（仅 LTO 模式存在）：与用户对象一起整体重排。
    for r in runtime_objs {
        link_cmd.arg(r);
    }
    // 标准库静态库（已安装包 或 实时编译产物归档）
    for l in stdlib_libs {
        link_cmd.arg(l);
    }
    // import 引入的包的机器码：**按 Target Triple 选出的那一份**静态库。
    // 交叉编译到 Windows 时这里自动变成包内的 .lib，无需用户干预（§4.3）。
    for pl in extra_libs {
        link_cmd.arg(pl);
    }
    if opts.lto {
        // 触发链接期 LTO：clang 读回所有内嵌 bitcode，跨 Dial+C 整体优化后再发机器码。
        link_cmd.arg("-flto").arg("-O3").arg("-march=native");
    }
    if opts.pgo_gen {
        link_cmd.arg("-fprofile-generate");
    }
    if let Some(pd) = &opts.pgo_use {
        link_cmd
            .arg(format!("-fprofile-use={}", pd.display()))
            .arg("-Wno-error=profile-instr-out-of-date")
            .arg("-Wno-error=profile-instr-unprofiled");
    }
    link_cmd
        // 线程库：
        // - Linux（glibc/musl）：`-lpthread` 链接 pthread（glibc ≥ 2.34 已并入 libc，此处是空操作）。
        // - Windows（MinGW + winpthreads）：`-Wl,-Bstatic -lpthread -Wl,-Bdynamic`
        //   把 winpthreads 静态链入，使最终 .exe 不依赖 libwinpthread-1.dll
        //   （单文件可执行程序）。winpthreads 提供与 pthread 兼容的 API，
        //   因此 stdlib 的线程代码无需为 Windows 单独改写（阶段 6 的 Win32 后端可延后）。
        .arg(if cfg!(windows) {
            "-Wl,-Bstatic"
        } else {
            "-lpthread"
        });
    if cfg!(windows) {
        link_cmd.arg("-lpthread").arg("-Wl,-Bdynamic");
    }
    // v1.00：浮点数学内建（sqrt / pow / floor / ceil / round / fabs）来自 libm。
    link_cmd.arg("-lm");
    link_cmd.arg("-o").arg(exe);
    if !run_ok(&mut link_cmd) {
        return Err(format!("链接失败：无法用 {} 生成 {}", cc, exe.display()));
    }

    Ok(())
}

/// LTO 模式下把 C 运行时（runtime/ + stdlib/）逐文件编成内嵌 bitcode 的 `.o`。
///
/// 不走 .a 归档——直接把这一串 .o 交给 `final_link`，clang 才能跨 Dial+C 整体重排
/// （归档 .a 会切断 LTO 视线，除非用 llvm-ar/gcc-ar 这类 LTO 感知归档器）。
/// 同时注入 Fail-Fast 等级与 `-pthread`（并发运行时用到）。
fn compile_runtime_lto(safety: SafetyMode, opts: &LinkOpts) -> Result<Vec<PathBuf>, String> {
    let (clang, clang_prefix) = clang_for_lto()
        .ok_or_else(|| "LTO 需要 clang 编译 C 运行时".to_string())?;
    let base = find_project_base()
        .ok_or_else(|| "找不到 stdlib/ 与 runtime/ 目录（请用 DIALC_STDLIB_DIR 指定工程根）".to_string())?;
    let runtime_dir = base.join("runtime");
    let stdlib_dir = base.join("stdlib");

    let build_dir = base
        .join(".dialc_build")
        .join(format!("safety-{}", safety.as_c_macro()))
        .join("lto");
    fs::create_dir_all(&build_dir).map_err(|e| format!("创建 LTO 构建目录失败: {}", e))?;

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

    let mut obj_files: Vec<PathBuf> = Vec::new();
    for cfile in &entries {
        let out_o = build_dir.join(cfile.file_stem().unwrap()).with_extension("o");
        let mut cmd = Command::new(&clang);
        cmd.args(&clang_prefix)
            .arg("-flto")
            .arg("-O3")
            .arg("-march=native") // 与 inkwell 的 host_cpu() 对齐：让 C 运行时也吃到本机 SIMD
            .arg("-Wno-override-module")
            .arg("-pthread")
            .arg(format!("-DDIAL_SAFETY={}", safety.as_c_macro()))
            .arg("-I")
            .arg(&runtime_dir)
            .arg("-I")
            .arg(&stdlib_dir)
            .arg("-c")
            .arg(cfile)
            .arg("-o")
            .arg(&out_o);
        if opts.pgo_gen {
            cmd.arg("-fprofile-generate");
        }
        if let Some(pd) = &opts.pgo_use {
            cmd
                .arg(format!("-fprofile-use={}", pd.display()))
                .arg("-Wno-error=profile-instr-out-of-date")
                .arg("-Wno-error=profile-instr-unprofiled");
        }
        if !run_ok(&mut cmd) {
            return Err(format!("LTO 编译 C 源文件失败: {}", cfile.display()));
        }
        obj_files.push(out_o);
    }
    Ok(obj_files)
}

/// 运行命令并返回是否成功退出。
fn run_ok(cmd: &mut Command) -> bool {
    cmd.status().map(|s| s.success()).unwrap_or(false)
}
