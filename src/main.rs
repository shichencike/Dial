//! dialc 编译器入口（v0.3）。
//!
//! 用法：
//!   dialc build <源文件.dial> [-o 可执行文件] [-g] [--no-link]
//!   dialc run   <源文件.dial> [-o 可执行文件] [-g]
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
mod ir; // 自研后端：专属 IR（x86-64 / AArch64 汇编发射器的输入）
mod target; // 自研后端：目标平台差异抽象（SysV / AAPCS）
mod diagnostic;
mod dialfmt;
mod toolchain;
mod error;
mod index;
mod lexer;
mod lsp;
mod package;
mod parser;
mod pkgbuild;
mod semantic;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
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

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }
    let command = args[1].as_str();

    // v1.04 工具链：不需要位置参数的命令先分流（list 无参、help 只打印用法），
    // 其余命令各自在内部校验参数——避免「dialc list」被 args[2] 索引直接 panic。
    match command {
        "help" | "--help" | "-h" => {
            print_usage();
            return;
        }
        "list" => {
            ensure_no_extra(&args, 2);
            cmd_list();
            return;
        }
        "check" | "fmt" | "test" | "new" | "uninstall" | "pack"
          | "doctor" | "deps" | "watch" | "clean" | "completion" => {
            dispatch_tool(command, &args);
            return;
        }
        _ => {}
    }

    // build / run / install 都需要恰好一个位置参数。
    if args.len() < 3 {
        eprintln!("命令 '{}' 缺少参数。", command);
        print_usage();
        std::process::exit(1);
    }
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
                                let va = if f.variadic {
                                    if params.is_empty() { "..." } else { ", ..." }
                                } else {
                                    ""
                                };
                                println!("     extern fn {}({}{}): {}", f.name, params, va, f.ret);
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
        eprintln!("未知命令 '{}'。可用命令见 `dialc help`。", command);
        std::process::exit(1);
    }

    // 解析可选参数
    let mut out_path: Option<PathBuf> = None;
    let mut no_link = false;
    let mut safety = SafetyMode::Debug; // 默认：崩溃保护全开
    let mut pgo = false; // 剖析引导优化（--fast 下的终极杀器）
    let mut debug_info = false; // -g：生成标准 DWARF 调试信息
    // 第 18 层：两种产物形态编译模式（工具链层，非 gui 契约）。
    // --tiny 体积模式：C 侧 -Os + 分段 + 链接期 --gc-sections + strip（去掉 DWARF 与符号表）；
    // --single-file 便携模式：链接期 -static，产物不依赖宿主动态 libc。
    let mut tiny = false;
    let mut single_file = false;
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
            // 第 18 层：--tiny（体积模式）/ --single-file（便携模式）。
            // 两 flag 只置纯值，具体注入分别在 resolve_stdlib_archive（C 侧 -Os/分段）
            // 与 final_link（-static / --gc-sections / -s），冲突在下方集中 Fail-Fast。
            "--tiny" => {
                tiny = true;
                i += 1;
            }
            "--single-file" => {
                single_file = true;
                i += 1;
            }
            // -g：标准 DWARF（GDB/LLDB 可按源码行断点、按 Dial 变量名 print）
            "-g" | "--debug-info" => {
                debug_info = true;
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
                // A3：旧实现只打印一行「未知参数」就继续编译，甚至以 rc=0 产出可执行文件，
                // 让人误以为 `dialc build a.dial b.dial` 把两个文件都编进去了。
                // Fail-Fast 铁律要求：任何不认识的输入一律当场失败，绝不静默吞掉。
                if other.ends_with(".dial") || other.ends_with(".diali") {
                    eprintln!(
                        "未知参数 '{}'：dialc 一次只编译一个源文件。\n\
                         多个 .dial 请改用本地源码模块：`import \"./xxx.dial\";`（见 README §34）。",
                        other
                    );
                } else {
                    eprintln!("未知参数 '{}'（可用选项见 `dialc help`）", other);
                }
                std::process::exit(1);
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

    // 第 18 层：编译模式冲突矩阵（复用 A3「不认识 / 不合法一律当场失败」铁律）。
    // --tiny 的目标是最小体积（-Os + 分段 + --gc-sections + strip），与下列开关
    // 目标相反，明确拒绝而非静默覆盖，避免用户以为两个优化同时生效：
    //   · --fast / --pgo：-O3 + LTO / 插桩会显著增大产物；
    //   · -g：strip 会把刚生成的 DWARF 又删掉，自相矛盾。
    // --single-file 只影响链接期 -static，与 LTO / PGO / -g 正交，允许叠加；
    // 但 -static 会把 libc 静态段拉进产物，反而比默认动态链接更大，故与 --tiny
    // 叠加时如实提示：前者求「最小体积」、后者求「不依赖宿主」，二者诉求不同。
    if tiny {
        if safety == SafetyMode::ReleaseFast {
            eprintln!("❌ --tiny 与 --fast 冲突：体积模式用 -Os + --gc-sections + strip，");
            eprintln!("   --fast 用 -O3 + LTO 拉满性能，两者目标相反。请二选一。");
            std::process::exit(1);
        }
        if pgo {
            eprintln!("❌ --tiny 与 --pgo 冲突：插桩会增大产物，与体积模式目标相反。请二选一。");
            std::process::exit(1);
        }
        if debug_info {
            eprintln!("❌ --tiny 与 -g 冲突：体积模式会 strip 掉 DWARF 与符号表，");
            eprintln!("   刚生成的调试信息又会被删除。请二选一。");
            std::process::exit(1);
        }
    }
    if tiny && single_file {
        eprintln!("ℹ️  --tiny + --single-file：可叠加，但 -static 会把 libc 静态段拉入产物，");
        eprintln!("   通常比 --tiny 单独使用更大；--tiny 求最小体积、--single-file 求不依赖宿主，诉求不同。");
    }
    if single_file {
        eprintln!("ℹ️  --single-file：将用 -static 全静态链接，产物不再依赖宿主动态 libc。");
    }

    // 1~3) 词法 / 语法 / 语义（含本地源码模块合并、包解析、FFI 声明校验）
    //      统一交给 frontend()，让 build / run / check / test 共用同一条前端流水线——
    //      否则「检查通过」和「实际编译」可能因为两套解析逻辑而给出不同结论。
    let fe = match frontend(&src_path, &mut |m| println!("{}", m)) {
        Ok(f) => f,
        Err(e) => {
            diagnostic::report(&e, &src_path);
            std::process::exit(1);
        }
    };

    // test 块是「只在 dialc test 下编译」的测试代码，不进正式产物。
    // 明确告知一声，避免写了 test 却误以为 build 已经跑过它们。
    if !fe.program.tests.is_empty() {
        eprintln!(
            "ℹ️  发现 {} 个 test 块，普通 build/run 不编译它们；用 `dialc test {}` 运行。",
            fe.program.tests.len(),
            src_path
        );
    }

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
    if debug_info && fast {
        eprintln!("ℹ️  -g 与 --fast 同用：LLVM 仍为 O3，部分变量可能显示 <optimized out>；纯调试建议去掉 --fast。");
    }
    let cg_opts = codegen::CgOpts {
        safety,
        aggressive: fast,
        bc_out: if fast { Some(bc_path.clone()) } else { None },
        src_file: fe.src_file.clone(),
        debug_info,
    };
    if let Err(e) = codegen::compile(
        &fe.program,
        &fe.functions,
        &fe.structs,
        &obj_path,
        &cg_opts,
        &fe.externs,
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

    // 第 18 层：把 --tiny / --single-file 做成一份共享 base，下面所有 LinkOpts
    // 构造点一律从它 clone 后再叠各自字段，杜绝「某个分支漏传导致 flag 静默失效」。
    // （--tiny 已由冲突矩阵挡掉 fast/LTO，但 --single-file 可与 fast 叠加，故 LTO
    //  分支同样必须带上 single_file，否则 -static 会在那几条路径里丢失。）
    let mode = LinkOpts {
        tiny,
        single_file,
        ..LinkOpts::default()
    };

    if lto_capable {
        if eff_pgo {
            // —— PGO 两段式 ——
            // 阶段一（instrument）：用插桩 bitcode 编出训练用二进制，跑一遍收集热点，
            //   再把 profile 合并成 profdata。
            // 阶段二（optimize）：用 profdata 重新编译，把热路径排到 I-Cache 最前。
            let profdir = build_dir.join("pgo");
            let _ = fs::create_dir_all(&profdir);
            let gen_exe = build_dir.join("pgo_gen_bin");

            let mut gen = mode.clone();
            gen.lto = true;
            gen.pgo_gen = true;
            if let Err(e) = clang_compile_bc(&bc_path, &obj_path, &gen) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            if let Err(e) = link(&obj_path, &gen_exe, safety, &fe.pkg_libs, &fe.ffi_rpaths, &gen) {
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
            let mut useo = mode.clone();
            useo.lto = true;
            useo.pgo_use = Some(profdata.clone());
            if let Err(e) = clang_compile_bc(&bc_path, &obj_path, &useo) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            if let Err(e) = link(&obj_path, &exe_path, safety, &fe.pkg_libs, &fe.ffi_rpaths, &useo) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            println!("✅ PGO 优化完成（热路径已据训练样本重排）。");
        } else {
            // 单阶段 LTO：把 .bc 经 clang -flto 编成胖对象，再整体链接。
            let mut o = mode.clone();
            o.lto = true;
            if let Err(e) = clang_compile_bc(&bc_path, &obj_path, &o) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
            if let Err(e) = link(&obj_path, &exe_path, safety, &fe.pkg_libs, &fe.ffi_rpaths, &o) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    } else {
        // 非 LTO 模式（debug / safe，或 fast 但无 clang）：直接用 inkwell 写出的原生 .o 链接。
        // 第 18 层：这里必须传 &mode（含 tiny / single_file），不能再用 default()。
        if let Err(e) = link(&obj_path, &exe_path, safety, &fe.pkg_libs, &fe.ffi_rpaths, &mode) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }

    match command {
        "build" => {
            if debug_info {
                println!(
                    "✅ 已生成可执行文件（含 DWARF 调试信息，可用 gdb/lldb 调试）: {}",
                    exe_path.display()
                );
            } else {
                println!("✅ 已生成可执行文件: {}", exe_path.display());
            }
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
/// v1.04 FFI：`link "...";` 声明的库种类。
///
/// 静态归档直接进链接器输入；动态共享库除了进输入，还要额外注入运行期查找路径
/// （见 `final_link` 的 `-Wl,-rpath`）。`.dialpkg` 包分发走另一条链路
/// （`package.rs` + J 组），仍强制静态，不受这里影响。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FfiLibKind {
    Static,
    Dynamic,
}

struct FfiLib {
    path: PathBuf,
    kind: FfiLibKind,
}

/// v1.04 FFI：校验并解析 `link "...";` 声明的库路径。
///
/// 五条硬约束：
///  1. **后缀白名单**：静态 `.a` / `.lib`；动态 `.so`（含 `libx.so.1`）、`.dll`、`.dylib`。
///     其余后缀一律拒绝——系统库（libc / libm / pthread）由 C 编译器按平台默认动态解析，
///     不写 `link`，避免把庞大系统库塞进 LTO 链路。
///  2. 相对路径以**源文件所在目录**为基准解析，并要求文件确实存在（Fail-Fast，
///     绝不静默降级成「链接器自己去找」）。
///  3. 按后缀校验**内容魔数**：静态 `!<arch>\n`；ELF `.so` 为 `\x7fELF`；PE `.dll`
///     为 `MZ`；Mach-O `.dylib` 为 32/64 位或 fat 的 magic。后缀对但内容是文本
///     一律当场拒绝——伪装库混进链接器只会换来一句莫名其妙的 ld 报错。
///  4. 动态库按**扩展名**判定格式（而非宿主平台），交叉编译链接他平台库时同样成立。
///  5. **包分发铁律不动**：`.dialpkg` 的 `targets` 仍只认 `.a` / `.lib`（J 组锁死）。
///     语言层 `link` 动态库是源码作者显式选择，由其自行承担运行期依赖。
fn resolve_ffi_link(spec: &str, src_path: &Path) -> Result<FfiLib, String> {
    let file_part = Path::new(spec)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(spec);
    let lower = file_part.to_ascii_lowercase();

    let (kind, expected) = if lower.ends_with(".a") || lower.ends_with(".lib") {
        (FfiLibKind::Static, "ar 归档")
    } else if lower.ends_with(".so") || lower.contains(".so.") {
        (FfiLibKind::Dynamic, "ELF 共享对象")
    } else if lower.ends_with(".dll") {
        (FfiLibKind::Dynamic, "PE 动态库")
    } else if lower.ends_with(".dylib") {
        (FfiLibKind::Dynamic, "Mach-O 动态库")
    } else {
        return Err(format!(
            "link 的路径必须是静态归档 .a / .lib 或动态库 .so / .dll / .dylib，遇到 '{}'；\
             系统库（libc / libm）由 C 编译器默认链接，不需要也不允许用 link 声明",
            spec
        ));
    };

    let raw = Path::new(spec);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        let base = src_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        base.join(raw)
    };
    let canonical = candidate.canonicalize().map_err(|e| {
        format!(
            "link 声明的库 '{}' 无法读取（解析为 {}）：{}",
            spec,
            candidate.display(),
            e
        )
    })?;

    let mut f = fs::File::open(&canonical)
        .map_err(|e| format!("无法打开库 '{}'：{}", canonical.display(), e))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).map_err(|_| {
        format!(
            "库 '{}' 内容不完整（不足 8 字节），不是合法的 {}",
            canonical.display(),
            expected
        )
    })?;

    let m4 = &magic[..4];
    let ok = match kind {
        FfiLibKind::Static => &magic == b"!<arch>\n",
        FfiLibKind::Dynamic => {
            if lower.ends_with(".so") || lower.contains(".so.") {
                m4 == &b"\x7fELF"[..]
            } else if lower.ends_with(".dll") {
                &magic[..2] == &b"MZ"[..]
            } else {
                // Mach-O：32/64 位薄二进制与 fat 二进制的大小端变体。
                const MACHO: [[u8; 4]; 8] = [
                    [0xfe, 0xed, 0xfa, 0xce], [0xce, 0xfa, 0xed, 0xfe],
                    [0xfe, 0xed, 0xfa, 0xcf], [0xcf, 0xfa, 0xed, 0xfe],
                    [0xca, 0xfe, 0xba, 0xbe], [0xbe, 0xba, 0xfe, 0xca],
                    [0xca, 0xfe, 0xba, 0xbf], [0xbf, 0xba, 0xfe, 0xca],
                ];
                MACHO.iter().any(|m| m4 == &m[..])
            }
        }
    };
    if !ok {
        return Err(format!(
            "{} '{}' 缺少合法的{}魔数，后缀对了但内容不是真正的库（拒绝伪装文件）",
            if kind == FfiLibKind::Static { "静态库" } else { "动态库" },
            canonical.display(),
            expected
        ));
    }

    Ok(FfiLib { path: canonical, kind })
}

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
                    // C 风格异构变参是 `.diali` 的 FFI 特例；Dial 自身函数永远为 false。
                    variadic: f.variadic,
                    // 包外部函数由 C 侧提供，链接名就是它的符号名——等价于「已导出」。
                    exported: true,
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
    /// `dialc test` 会为每个 test 块单独链接一次；逐块打印 stdlib 编译日志
    /// 会把测试结果淹掉，故允许调用方静默这些 ℹ️ 行（不影响错误与警告）。
    quiet: bool,
    /// 第 18 层 --tiny：C 侧 -Os + -ffunction-sections/-fdata-sections，
    /// 链接期 -Wl,--gc-sections + -s（strip），并强制关闭 DWARF（由冲突矩阵保证）。
    tiny: bool,
    /// 第 18 层 --single-file：链接期 -static，产物不依赖宿主动态 libc。
    single_file: bool,
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
    ffi_rpaths: &[PathBuf],
    opts: &LinkOpts,
) -> Result<(), String> {
    if opts.lto {
        // LTO：C 运行时逐文件编成 LTO .o，与用户胖对象一起整体重排。
        let runtime_objs = compile_runtime_lto(safety, opts)?;
        final_link(obj, exe, extra_libs, &[], &runtime_objs, ffi_rpaths, opts)
    } else {
        // 非 LTO：优先已安装包，否则实时编译源码归档成 .a。
        let stdlib_libs = resolve_stdlib_archive(safety, opts.quiet, opts.tiny)?;
        final_link(obj, exe, extra_libs, &stdlib_libs, &[], ffi_rpaths, opts)
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
fn resolve_stdlib_archive(safety: SafetyMode, quiet: bool, tiny: bool) -> Result<Vec<PathBuf>, String> {
    // --tiny 必须让 C 侧 -Os + -ffunction-sections/-fdata-sections 真正生效：
    // 预编译 stdlib 包是 -O2 烘焙、且不带函数分段，--gc-sections 对它无效。
    // 故 tiny 模式强制走下方「源码实时编译」回退路径，绕过已安装包。
    let installed = if tiny {
        None
    } else {
        package::find_installed("stdlib").map_err(|e| e.to_string())?
    };
    if let Some((dir, manifest)) = installed {
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
                if !quiet {
                    println!(
                        "ℹ️  使用已安装的标准库包: {} v{}（ABI {}，目标 {}）",
                        manifest.name,
                        manifest.version,
                        manifest.abi,
                        package::host_triple()
                    );
                }
                return Ok(vec![lib]);
            }
            Err(e) => {
                // 包存在但当前平台不在其 targets 列表：退化为源码编译，保持兼容。
                if !quiet {
                    println!(
                        "⚠️  已安装的标准库包不含当前平台 {} 的机器码，回退源码编译：{}",
                        package::host_triple(),
                        e
                    );
                }
            }
        }
    }

    // ---- 回退：实时编译 runtime/ + stdlib/ 源码 → libdial_std.a ----
    let base = find_project_base()
        .ok_or_else(|| "找不到 stdlib/ 与 runtime/ 目录（请用 DIALC_STDLIB_DIR 指定工程根）".to_string())?;
    let runtime_dir = base.join("runtime");
    let stdlib_dir = base.join("stdlib");

    // 临时构建目录（按安全等级分目录，避免不同等级的 .o 互相污染）。
    // 第 18 层：--tiny 编出的 -Os + 分段 .o / .a 再单独隔一层，避免与默认
    // -O2 的 libdial_std.a 交替构建时互相覆盖（--gc-sections 需要分段版）。
    let build_dir = base
        .join(".dialc_build")
        .join(format!("safety-{}{}", safety.as_c_macro(), if tiny { "-tiny" } else { "" }));
    fs::create_dir_all(&build_dir).map_err(|e| format!("创建构建目录失败: {}", e))?;

    let (cc, cc_prefix) = cc_command();
    if !quiet {
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
    }

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
        cmd.args(&cc_prefix);
        // 第 18 层 --tiny：C 侧按尺寸优化，并把每个函数 / 数据放进独立 section，
        // 供链接期 -Wl,--gc-sections 精确剔除未引用部分。非 tiny 保持 -O2 一字不动。
        if tiny {
            cmd.arg("-Os")
                .arg("-ffunction-sections")
                .arg("-fdata-sections");
        } else {
            cmd.arg("-O2");
        }
        cmd
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

/// 第 18 层 --single-file：挑一个**真正支持 -static** 的系统 C 编译器作链接驱动。
///
/// 为何不复用 `cc_command()`：它默认优先 `zig cc`，而 zig cc 的 `-static` 对 glibc
/// 目标无效（只对 `x86_64-linux-musl` 目标真正静态）。改 ABI 目标会与 inkwell 生成的
/// 原生 .o、zig cc 编出的 stdlib .a 混链失配，故不动目标、只换驱动为 gcc / clang / cc
/// ——它们的 `-static` 直接吃本机 glibc 的 libc.a / libm.a / libpthread.a，且能链接
/// 现有对象（已实测）。尊重 `DIAL_C_COMPILER` 覆盖：若用户显式指定了驱动则沿用。
fn static_link_cc() -> Result<(String, Vec<String>), String> {
    if let Ok(val) = env::var("DIAL_C_COMPILER") {
        let val = val.trim();
        if !val.is_empty() {
            let mut parts = val.split_whitespace();
            if let Some(prog) = parts.next() {
                return Ok((prog.to_string(), parts.map(|s| s.to_string()).collect()));
            }
        }
    }
    for prog in ["gcc", "clang", "cc"] {
        let ok = Command::new(prog)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Ok((prog.to_string(), vec![]));
        }
    }
    Err("--single-file 需要系统 gcc / clang / cc 之一做全静态链接，但都未找到".to_string())
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
    ffi_rpaths: &[PathBuf],
    opts: &LinkOpts,
) -> Result<(), String> {
    // LTO/PGO 依赖 clang 的 bitcode 链路，链接驱动也必须用 clang。
    // 第 18 层 --single-file：zig cc 的 -static 对 glibc 目标**不生效**（实验证实：
    // 连纯 C 的 printf 仍是 dynamically linked，它只对 musl 目标真正静态）。
    // 故单文件便携模式改用系统 gcc/clang 作为链接驱动——它们能静态链接现有
    // zig cc 编出的 .o / .a（对象格式同为 ELF，ABI 一致，已实测 gcc -static
    // 链接 zig cc -c 产物成功）。仅切「链接」这一步，stdlib 编译仍走原路径。
    // 优先级：LTO > --single-file > 常规。
    let (cc, cc_prefix) = if opts.lto {
        clang_for_lto().ok_or_else(|| "LTO/PGO 需要 clang 作为链接驱动".to_string())?
    } else if opts.single_file {
        static_link_cc()?
    } else {
        cc_command()
    };
    let mut link_cmd = Command::new(&cc);
    link_cmd.args(&cc_prefix);
    // 第 18 层 --single-file：-static 必须最早注入（在 obj 之前），让链接器从
    // 一开始就以静态模式解析后续所有归档与系统库。实测 glibc 下 -static 与 gui 包
    // 的 dlopen 平台后端可共存（链接器给 warning 但 dlopen 仍可在运行期装载平台库）。
    if opts.single_file {
        link_cmd.arg("-static");
    }
    link_cmd.arg(obj);
    // C 运行时 .o（仅 LTO 模式存在）：与用户对象一起整体重排。
    for r in runtime_objs {
        link_cmd.arg(r);
    }
    // import 引入的包的机器码：**按 Target Triple 选出的那一份**静态库。
    // 交叉编译到 Windows 时这里自动变成包内的 .lib，无需用户干预（§4.3）。
    //
    // **顺序要紧**：静态归档由链接器**从左到右**扫描，某个符号只有在其归档被扫描时
    // 仍处于「未定义」状态，对应成员才会被拉进来。包会引用标准库符号
    // （典型如 `dial_string_new`——包要向 Dial 返回 string/bytes 就必须构造带
    // 引用计数头的堆对象），所以**包必须排在标准库归档之前**；否则链接器扫完
    // 标准库归档后那些符号仍未定义，包再引用它们就是「undefined symbol」。
    // 标准库从不反向引用包符号，故这个次序是安全的。
    for pl in extra_libs {
        link_cmd.arg(pl);
    }
    // 标准库静态库（已安装包 或 实时编译产物归档）
    for l in stdlib_libs {
        link_cmd.arg(l);
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
    // v1.04：语言层 `link` 声明的动态库需要运行期查找路径。
    // ELF / Mach-O 用 -Wl,-rpath 把库所在目录写进 RUNPATH，产物直接可跑，
    // 不必手工设 LD_LIBRARY_PATH / DYLD_LIBRARY_PATH；PE（Windows）没有 rpath
    // 概念，需把 .dll 放到 exe 同目录或 PATH（见 README §33.8）。
    // 第 18 层：--single-file 全静态产物没有动态装载器，RUNPATH 无意义，跳过。
    if !cfg!(windows) && !opts.single_file {
        for dir in ffi_rpaths {
            link_cmd.arg(format!("-Wl,-rpath,{}", dir.display()));
        }
    }
    // 第 18 层 --tiny：链接期剔除未引用 section + 剥掉符号表 / DWARF。
    // 与 C 侧的 -ffunction-sections/-fdata-sections 成对；即便走预编译 stdlib 包
    // （无分段），-s 仍稳定生效，故 tiny 的 strip 语义不依赖是否命中源码编译路径。
    if opts.tiny {
        link_cmd.arg("-Wl,--gc-sections").arg("-s");
    }
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


// ===========================================================================
//  v1.04 工具链：统一前端流水线 / 本地源码模块 / CLI 子命令
// ===========================================================================
//
//  背景（REPORT_v1.04_gaps.md 的 A 组）：此前 CLI 只有 build/run/install/version，
//  多文件源码无法互相引用，且 `dialc build a.dial b.dial` 会把多余输入静默吞掉。
//  这里把「词法 → 语法 → 包解析 → 语义」抽成 frontend()，供 build/run/check/test
//  共用；并在同一次前端里递归展开 `import "./xxx.dial";` 到本地源码模块。

/// 完整用法。`help` 与参数缺失共用同一份，避免两处文案漂移。
fn print_usage() {
    eprintln!("dialc — Dial 语言编译器 v{}", env!("CARGO_PKG_VERSION"));
    eprintln!();
    eprintln!("用法: dialc <命令> [参数]");
    eprintln!();
    eprintln!("编译与运行");
    eprintln!("  build <文件.dial> [-o 输出] [-g] [--no-link] [--safety debug|safe|fast] [--pgo] [--tiny] [--single-file]");
    eprintln!("  run   <文件.dial> [同上]");
    eprintln!("  -g / --debug-info          生成标准 DWARF（GDB/LLDB 源码行断点 + print 变量名）");
    eprintln!("  --tiny                     体积模式：-Os + 函数/数据分段 + --gc-sections + strip（与 --fast/--pgo/-g 冲突）");
    eprintln!("  --single-file              便携模式：-static 全静态链接，产物不依赖宿主动态 libc");
    eprintln!("  check <文件.dial>                只做词法/语法/语义检查，不产出文件");
    eprintln!("  test  <文件.dial> [--filter 名]  运行顶层 test \"名\" {{ ... }} 块");
    eprintln!();
    eprintln!("工程与源码组织");
    eprintln!("  fmt   <文件.dial>... [--check|--diff] 间距规范化（token 流/注释原文不变、幂等）");
    eprintln!("  new   <目录> [包名]            生成可打包的项目骨架（dial.toml）");
    eprintln!("  pack  <目录> [-o 包.dialpkg] [--dry-run|--json|--init]
                                   打包 / 预览 / 机器可读清单 / 自动生成清单草稿
  pack  --verify <包.dialpkg>     校验已有包的 manifest 与逐文件 SHA256");
    eprintln!("  import \"./util.dial\";         源码内引用同目录模块（见 README §34）");
    eprintln!();
    eprintln!("包管理");
    eprintln!("  install   <包.dialpkg|URL> [--sha256 哈希]");
    eprintln!("  list                           列出已安装包");
    eprintln!("  uninstall <包名>               卸载已安装包");
    eprintln!();
    eprintln!("工具链（v1.10 增补）");
    eprintln!("  doctor [--json]                 环境自检（编译器/缓存/包完整性/清单）");
    eprintln!("  deps   <文件.dial> [--json]     三级 import 依赖图 + 循环检测");
    eprintln!("  watch  <文件.dial> [--action check|build|test] [--interval ms] [--max-ticks N]");
    eprintln!("  clean  [目录] [--apply] [--json] 清理 .o / .dialpkg（默认 dry-run）");
    eprintln!("  completion <bash|zsh|fish>      生成 shell 补全脚本");
    eprintln!();
    eprintln!("其他");
    eprintln!("  version | --version | -V       显示版本号");
    eprintln!("  lsp                            编辑器语言服务器（stdio 协议）");
    eprintln!("  help | --help | -h             显示本帮助");
}

/// 拒绝多余位置参数（A3 的推广：所有子命令都不许静默吞输入）。
fn ensure_no_extra(args: &[String], from: usize) {
    if args.len() > from {
        eprintln!("未知参数 '{}'（见 `dialc help`）", args[from]);
        std::process::exit(1);
    }
}

/// 取唯一位置参数，缺失即报错。
fn one_arg(args: &[String], cmd: &str) -> String {
    if args.len() < 3 {
        eprintln!("命令 '{}' 缺少参数。", cmd);
        print_usage();
        std::process::exit(1);
    }
    args[2].clone()
}

fn cmd_doctor(args: &[String]) {
    let mut as_json = false;
    for a in &args[2..] {
        match a.as_str() {
            "--json" => as_json = true,
            _ => {
                eprintln!("doctor: 未知参数 '{}'（可用: --json）", a);
                std::process::exit(1);
            }
        }
    }
    match toolchain::doctor(as_json) {
        Ok(fails) => {
            if fails > 0 {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_deps(args: &[String]) {
    let mut as_json = false;
    let mut files: Vec<String> = Vec::new();
    for a in &args[2..] {
        match a.as_str() {
            "--json" => as_json = true,
            a if a.starts_with('-') => {
                eprintln!("deps: 未知参数 '{}'（可用: --json）", a);
                std::process::exit(1);
            }
            _ => files.push(a.to_string()),
        }
    }
    if files.len() != 1 {
        eprintln!("用法: dialc deps <文件.dial> [--json]");
        std::process::exit(1);
    }
    if let Err(e) = toolchain::deps(&files[0], as_json) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

fn cmd_watch(args: &[String]) {
    let mut action = "check".to_string();
    let mut interval: u64 = 500;
    let mut max_ticks: Option<u64> = None;
    let mut files: Vec<String> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--action" => {
                i += 1;
                action = args.get(i).cloned().unwrap_or_default();
            }
            "--interval" => {
                i += 1;
                interval = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(500);
            }
            "--max-ticks" => {
                i += 1;
                max_ticks = args.get(i).and_then(|v| v.parse().ok());
            }
            a if a.starts_with('-') => {
                eprintln!("watch: 未知参数 '{}'", a);
                std::process::exit(1);
            }
            _ => files.push(args[i].clone()),
        }
        i += 1;
    }
    if files.len() != 1 {
        eprintln!("用法: dialc watch <文件.dial> [--action check|build|test] [--interval ms] [--max-ticks N]");
        std::process::exit(1);
    }
    if let Err(e) = toolchain::watch(&files[0], &action, interval, max_ticks) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

fn cmd_clean(args: &[String]) {
    let mut apply = false;
    let mut as_json = false;
    let mut dir = ".".to_string();
    for a in &args[2..] {
        match a.as_str() {
            "--apply" => apply = true,
            "--json" => as_json = true,
            a if a.starts_with('-') => {
                eprintln!("clean: 未知参数 '{}'（可用: --apply / --json）", a);
                std::process::exit(1);
            }
            _ => dir = a.to_string(),
        }
    }
    if let Err(e) = toolchain::clean(&dir, apply, as_json) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

fn cmd_completion(args: &[String]) {
    let shells: Vec<String> = args[2..].iter().filter(|a| !a.starts_with('-')).cloned().collect();
    if shells.len() != 1 {
        eprintln!("用法: dialc completion <bash|zsh|fish>");
        std::process::exit(1);
    }
    if let Err(e) = toolchain::completion(&shells[0]) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

fn dispatch_tool(command: &str, args: &[String]) {
    match command {
        "check" => cmd_check(args),
        "fmt" => cmd_fmt(args),
        "test" => cmd_test(args),
        "new" => cmd_new(args),
        "uninstall" => cmd_uninstall(args),
        "pack" => cmd_pack(args),
        "doctor" => cmd_doctor(args),
        "deps" => cmd_deps(args),
        "watch" => cmd_watch(args),
        "clean" => cmd_clean(args),
        "completion" => cmd_completion(args),
        _ => unreachable!("dispatch_tool 只接收已分流的工具命令"),
    }
}

// ---------------------------------------------------------------------------
//  统一前端：本地源码模块收集 + 包解析 + 语义分析
// ---------------------------------------------------------------------------

struct SourceUnit {
    path: PathBuf,
    tokens: Vec<lexer::Token>,
}

struct SourceSet {
    /// 深度优先收集到的所有源码文件，units[0] 恒为主源文件。
    units: Vec<SourceUnit>,
    /// 汇总后的**包名** import（本地源码项不在这里），已去重。
    imports: Vec<String>,
}

struct Frontend {
    program: ast::Program,
    /// `analyze` 之前的 AST 副本。`dialc test` 要为每块测试重新 analyze，
    /// 不能拿已经 desugar 过的 program 反复跑（语义层会就地改写 AST）。
    raw_program: ast::Program,
    functions: HashMap<String, semantic::FuncInfo>,
    structs: semantic::StructTable,
    externs: Vec<(String, semantic::FuncInfo)>,
    pkg_libs: Vec<PathBuf>,
    ffi_rpaths: Vec<PathBuf>,
    src_file: String,
}

/// 只从 token 流里抽 `import "..."` 的字符串，不做完整语法解析。
///
/// 之所以用「扫 token」而不是 `parser::parse`：两阶段解析的本意就是先拿 import
/// 列表、解析包（从而知道不透明类型名）后再正式解析；此时若强行完整 parse，
/// 源码里用到包句柄类型的地方会在第一遍就失败。扫 token 天然免疫这个问题。
fn import_specs(tokens: &[lexer::Token]) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..tokens.len() {
        if matches!(&tokens[i].kind, lexer::TokenKind::Import) {
            if let Some(n) = tokens.get(i + 1) {
                if let lexer::TokenKind::StringLit(s) = &n.kind {
                    out.push(s.clone());
                }
            }
        }
    }
    out
}

/// 判断一条 import 是否指向**本地源码模块**。
///
/// 两种写法都算本地：
///   - 显式相对路径：`import "./util.dial";` / `"../common/util"` / 任意 `.dial` 后缀；
///   - 裸名且同目录确有 `<name>.dial`：`import "util";` 直接可用——这也是
///     「普通程序员写日常程序」最直觉的写法（清单 A1 的原样复现）。
///
/// 其它裸名再依次交给 `collect_sources` 的第二级（已安装包 manifest 的 `src`
/// 映射里声明的 Dial 源码模块）与第三级（按包名读 `.diali` + 选 `.a`）。
/// 两类本地写法都不匹配时，resolve_imports 会给出「找不到已安装的包」。
fn is_local_import(spec: &str, base: &Path) -> bool {
    if spec.ends_with(".dial")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with(".\\")
        || spec.starts_with("..\\")
    {
        return true;
    }
    if !spec.contains('/') && !spec.contains('\\') {
        return base.join(format!("{}.dial", spec)).is_file();
    }
    false
}

fn local_module_path(spec: &str, base: &Path) -> PathBuf {
    let mut rel = PathBuf::from(spec);
    if rel.extension().and_then(|e| e.to_str()) != Some("dial") {
        rel.set_extension("dial");
    }
    if rel.is_absolute() {
        rel
    } else {
        base.join(rel)
    }
}

/// 递归收集源码模块，按**三级优先**解析每条 `import`：
///
/// 1. 本地源码模块（相对路径 / 同目录同名 `.dial`）——用户项目里的文件优先；
/// 2. 已安装包 `src/` 里的 Dial 源码模块（`[src]` 段声明的裸名）——随包分发，
///    装包即用，无需复制；
/// 3. 包名（`~/.dial/packages/<name>/`）——读 `.diali` + 选 `.a`，原有语义。
///
/// 第 1、2 级都走同一套递归展开（循环引用检测 / `visited` 去重 / AST 合并），
/// 只有第 3 级汇总进 `set.imports` 交给 `resolve_imports`。
/// `stack` 用于循环引用检测，`visited` 保证同一文件被多个模块 import 时只展开一次
/// （否则会重复定义函数）。错误一律带**文件路径**，多文件项目才排得清障。
fn collect_sources(
    root: &Path,
    visited: &mut HashSet<PathBuf>,
    stack: &mut Vec<PathBuf>,
    set: &mut SourceSet,
) -> Result<(), String> {
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("无法读取源文件 {}: {}", root.display(), e))?;
    if stack.contains(&canonical) {
        let chain: Vec<String> = stack
            .iter()
            .chain(std::iter::once(&canonical))
            .map(|p| p.display().to_string())
            .collect();
        return Err(format!("本地源码 import 出现循环引用：{}", chain.join(" -> ")));
    }
    if visited.contains(&canonical) {
        return Ok(());
    }
    visited.insert(canonical.clone());
    stack.push(canonical.clone());

    let source = fs::read_to_string(&canonical)
        .map_err(|e| format!("无法读取 {}: {}", canonical.display(), e))?;
    let tokens = lexer::lex(&source)
        .map_err(|e| format!("{}：{}", canonical.display(), e))?;
    let specs = import_specs(&tokens);
    set.units.push(SourceUnit {
        path: canonical.clone(),
        tokens,
    });

    let base = canonical.parent().unwrap_or_else(|| Path::new("."));
    for spec in specs {
        if is_local_import(&spec, base) {
            // 第一级：本地源码模块（相对路径 / 同目录同名 `.dial`）。
            // 保持最高优先——用户项目里自己放的同名文件永远覆盖包内副本，
            // 这样「改一份源码试试」不需要动已安装的包。
            let child = local_module_path(&spec, base);
            if !child.is_file() {
                return Err(format!(
                    "{}：本地源码 import '{}' 指向的文件不存在（解析为 {}）。\n\
                     本地模块请写相对路径（如 import \"./util.dial\";）；\n\
                     随包分发的 Dial 源码模块或已安装的包请写裸包名。",
                    canonical.display(),
                    spec,
                    child.display()
                ));
            }
            collect_sources(&child, visited, stack, set)?;
        } else if let Some((pkg_dir, rel)) =
            package::find_src_module(&spec).map_err(|e| e.to_string())?
        {
            // 第二级：包内 `src/` 的 Dial 源码模块（manifest 的 `src` 映射里
            // 以该裸名声明）。与本地模块走**完全相同**的展开路径——同一套循环
            // 引用检测 / visited 去重 / AST 合并，只是文件来自包缓存。
            // 它不引入任何机器码，也不新增 extern fn；模块内部的 `import "包";`
            // 会照常被汇总到第三级，由 resolve_imports 读 .diali + 选 .a。
            let child = pkg_dir.join(&rel);
            if !child.is_file() {
                return Err(format!(
                    "{}：包缓存 {} 声明了 Dial 源码模块 '{}'（src['{}'] = '{}'），\n\
                     但文件不存在：{}。包可能已损坏，请重新安装。",
                    canonical.display(),
                    pkg_dir.display(),
                    spec,
                    spec,
                    rel,
                    child.display()
                ));
            }
            collect_sources(&child, visited, stack, set)?;
        } else if !set.imports.contains(&spec) {
            // 第三级：按包名解析（读 .diali + 选 .a），v0.93 以来的原有语义。
            set.imports.push(spec);
        }
    }
    stack.pop();
    Ok(())
}

/// build / run / check / test 共用的编译前端。
///
/// 顺序：递归收集源码 → 解析包（拿不透明类型）→ 逐个源码文件正式 parse →
/// 合并 AST（含冲突检测）→ 语言层 extern/link 并入 → 语义分析。
/// 合并时把每个文件 `link` 的相对路径按**该文件所在目录**绝对化，避免子模块
/// 里的库被误当成相对主文件目录解析。
fn frontend(src_path: &str, log: &mut dyn FnMut(&str)) -> Result<Frontend, String> {
    let mut set = SourceSet {
        units: Vec::new(),
        imports: Vec::new(),
    };
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    collect_sources(Path::new(src_path), &mut visited, &mut stack, &mut set)?;

    let (mut externs, mut pkg_libs, opaque_types) = resolve_imports(&set.imports, log).map_err(|e| {
        // D6：旧诊断只说 dialc install，不解释「本地源码模块」这条路。
        // 只在「找不到已安装的包」这一句上追加指引，ABI / 哈希等其它错误原样透出。
        if e.contains("找不到已安装的包") {
            format!(
                "{}\n（提示：import 按三级解析——① 同目录 .dial 源码（写 import \"./xxx.dial\"; 或裸名 import \"xxx\";）；② 已安装包 src/ 里 [src] 段声明的 Dial 源码模块（裸名，装包即用、无需复制）；③ 包名（读 .diali + 链 .a）。① ② 都会自动合并编译。见 README §34）",
                e
            )
        } else {
            e
        }
    })?;

    let mut program = ast::Program {
        imports: set.imports.clone(),
        structs: Vec::new(),
        functions: Vec::new(),
        externs: Vec::new(),
        links: Vec::new(),
        cstructs: Vec::new(),
        tests: Vec::new(),
    };
    let mut fn_origin: HashMap<String, String> = HashMap::new();
    let mut struct_origin: HashMap<String, String> = HashMap::new();

    for unit in &set.units {
        let mut p = parser::parse_with(unit.tokens.clone(), &opaque_types)
            .map_err(|e| format!("{}：{}", unit.path.display(), e))?;
        let origin = unit.path.display().to_string();
        for f in &p.functions {
            if let Some(prev) = fn_origin.get(&f.name) {
                return Err(format!(
                    "函数 '{}' 重复定义：{} 与 {}（本地源码模块合并后不允许同名函数）",
                    f.name, prev, origin
                ));
            }
            fn_origin.insert(f.name.clone(), origin.clone());
        }
        for st in &p.structs {
            if let Some(prev) = struct_origin.get(&st.name) {
                return Err(format!(
                    "结构体 '{}' 重复定义：{} 与 {}",
                    st.name, prev, origin
                ));
            }
            struct_origin.insert(st.name.clone(), origin.clone());
        }
        let base = unit.path.parent().unwrap_or_else(|| Path::new("."));
        for l in &mut p.links {
            let raw = Path::new(&l.path);
            if !raw.is_absolute() {
                l.path = base.join(raw).to_string_lossy().into_owned();
            }
        }
        program.structs.append(&mut p.structs);
        program.functions.append(&mut p.functions);
        program.externs.append(&mut p.externs);
        program.links.append(&mut p.links);
        program.cstructs.append(&mut p.cstructs);
        program.tests.append(&mut p.tests);
    }

    // v1.04 FFI：语言层 extern fn 并入外部函数表（与包 .diali 一视同仁）。
    for e in &program.externs {
        if externs.iter().any(|(n, _)| n == &e.name) {
            return Err(format!(
                "外部函数 '{}' 重复声明（包 .diali 与源码 extern fn 撞名）",
                e.name
            ));
        }
        externs.push((
            e.name.clone(),
            semantic::FuncInfo {
                params: e.params.iter().map(|(_, t)| t.clone()).collect(),
                ret: e.ret.clone(),
                defaults: vec![None; e.params.len()],
                variadic: e.variadic,
                // 外部声明的函数体在 C 侧，符号名即链接名——等价于「已导出」。
                exported: true,
            },
        ));
    }

    // v1.04 FFI：link 声明的库按后缀验魔数，动态库额外收集 rpath。
    let mut ffi_rpaths: Vec<PathBuf> = Vec::new();
    for l in &program.links {
        match resolve_ffi_link(&l.path, Path::new(src_path)) {
            Ok(lib) => {
                if lib.kind == FfiLibKind::Dynamic {
                    if let Some(dir) = lib.path.parent() {
                        if !ffi_rpaths.iter().any(|d| d.as_path() == dir) {
                            ffi_rpaths.push(dir.to_path_buf());
                        }
                    }
                }
                pkg_libs.push(lib.path);
            }
            Err(msg) => {
                return Err(format!("第 {} 行 第 {} 列：{}", l.line, l.col, msg));
            }
        }
    }

    let raw_program = program.clone();
    let (functions, structs) = analyze(&mut program, &externs)
        .map_err(|e| format!("{}：{}", src_path, e))?;

    Ok(Frontend {
        program,
        raw_program,
        functions,
        structs,
        externs,
        pkg_libs,
        ffi_rpaths,
        src_file: src_path.to_string(),
    })
}

// ---------------------------------------------------------------------------
//  子命令实现
// ---------------------------------------------------------------------------

fn cmd_list() {
    match pkgbuild::list_installed() {
        Ok(rows) if rows.is_empty() => {
            println!("尚未安装任何包。用 `dialc install <包.dialpkg>` 安装，或 `dialc pack` 先打一个。");
        }
        Ok(rows) => {
            println!("已安装包 {} 个：", rows.len());
            for (name, ver, abi, dir) in rows {
                println!("  {:<18} v{:<10} ABI {:<3} {}", name, ver, abi, dir.display());
            }
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_check(args: &[String]) {
    ensure_no_extra(args, 3);
    let src = one_arg(args, "check");
    match frontend(&src, &mut |m| println!("{}", m)) {
        Ok(fe) => println!(
            "✅ 类型检查通过：{}（{} 个函数、{} 个 test 块）",
            src,
            fe.program.functions.len(),
            fe.program.tests.len()
        ),
        Err(e) => {
            diagnostic::report(&e, &src);
            std::process::exit(1);
        }
    }
}

fn cmd_uninstall(args: &[String]) {
    ensure_no_extra(args, 3);
    let name = one_arg(args, "uninstall");
    match pkgbuild::uninstall(&name) {
        Ok(dir) => println!("✅ 已卸载包 '{}'（删除 {}）", name, dir.display()),
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_new(args: &[String]) {
    if args.len() < 3 || args.len() > 4 {
        eprintln!("用法: dialc new <目录> [包名]");
        std::process::exit(1);
    }
    let dir = Path::new(&args[2]);
    let name = args.get(3).map(|s| s.as_str());
    match pkgbuild::new_project(dir, name) {
        Ok(p) => {
            println!("✅ 项目骨架已生成: {}", p.display());
            println!("   dial.toml / main.dial / {}.diali / lib/", args.get(3).map(|s| s.as_str()).unwrap_or_else(|| dir.file_name().and_then(|x| x.to_str()).unwrap_or("mylib")));
            println!("   下一步：把编译好的 .a 放进 dial.toml [targets] 登记的路径，再执行 `dialc pack {}`。", p.display());
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_pack(args: &[String]) {
    let mut out: Option<PathBuf> = None;
    let mut dirs: Vec<String> = Vec::new();
    let mut dry_run = false;
    let mut as_json = false;
    let mut init = false;
    let mut verify: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                match args.get(i) {
                    Some(v) => out = Some(PathBuf::from(v)),
                    None => {
                        eprintln!("选项 -o 缺少输出路径");
                        std::process::exit(1);
                    }
                }
            }
            "--dry-run" => dry_run = true,
            "--json" => as_json = true,
            "--init" => init = true,
            "--verify" => {
                i += 1;
                match args.get(i) {
                    Some(v) => verify = Some(v.clone()),
                    None => {
                        eprintln!("选项 --verify 缺少包路径");
                        std::process::exit(1);
                    }
                }
            }
            a if a.starts_with('-') => {
                eprintln!(
                    "pack: 未知参数 '{}'（可用: -o / --dry-run / --json / --init / --verify）",
                    a
                );
                std::process::exit(1);
            }
            _ => dirs.push(args[i].clone()),
        }
        i += 1;
    }

    // --verify 是独立模式：校验已有包，不需要项目目录。
    if let Some(v) = verify {
        if !dirs.is_empty() || out.is_some() || dry_run || as_json || init {
            eprintln!("pack: --verify 是独立模式，不能与其它选项混用");
            std::process::exit(1);
        }
        match pkgbuild::verify_pkg(Path::new(&v)) {
            Ok(summary) => {
                println!("✅ 包校验通过: {}", v);
                println!("   {}", summary);
            }
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    if dirs.len() != 1 {
        eprintln!("用法: dialc pack <项目目录> [-o 输出.dialpkg] [--dry-run] [--json] [--init]");
        eprintln!("      dialc pack --verify <包.dialpkg>");
        std::process::exit(1);
    }
    let dir = Path::new(&dirs[0]);

    if init {
        if dry_run || as_json || out.is_some() {
            eprintln!("pack: --init 不能与 --dry-run / --json / -o 混用");
            std::process::exit(1);
        }
        match pkgbuild::init_spec(dir) {
            Ok(p) => {
                println!("✅ 已生成清单草稿: {}", p.display());
                println!("   自动发现 iface / [targets] / [src] 并写成显式声明；请核对 name / version / license。");
                println!("   下一步: dialc pack --dry-run {} → dialc pack {}", dirs[0], dirs[0]);
            }
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    if dry_run || as_json {
        match pkgbuild::dry_run(dir, out.as_deref(), as_json) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    match pkgbuild::pack(dir, out.as_deref()) {
        Ok(pkg) => {
            println!("✅ 已生成包: {}", pkg.display());
            println!("   manifest / 逐文件 SHA256 已自检通过，可直接 `dialc install {}`", pkg.display());
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_fmt(args: &[String]) {
    let mut check_only = false;
    let mut diff_only = false;
    let mut files: Vec<String> = Vec::new();
    for a in &args[2..] {
        match a.as_str() {
            "--check" => check_only = true,
            "--diff" => diff_only = true,
            a if a.starts_with('-') => {
                eprintln!("fmt: 未知参数 '{}'（可用: --check / --diff）", a);
                std::process::exit(1);
            }
            _ => files.push(a.clone()),
        }
    }
    if files.is_empty() {
        eprintln!("用法: dialc fmt <文件.dial>... [--check | --diff]");
        std::process::exit(1);
    }
    if check_only && diff_only {
        eprintln!("fmt: --check 与 --diff 不能同时使用");
        std::process::exit(1);
    }
    let mut need_fmt = false;
    for f in &files {
        let src = match fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fmt: 无法读取 {}: {}", f, e);
                std::process::exit(1);
            }
        };
        let before = match lexer::lex(&src) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("fmt: {} 存在词法错误，拒绝格式化：{}", f, e);
                std::process::exit(1);
            }
        };
        // 字面量与注释的原文快照：格式化绝不能碰它们（第二条硬契约）。
        let protected_before = dialfmt::protected_spans(&src);
        let formatted = dialfmt::format_source(&src);
        // 幂等自检
        if dialfmt::format_source(&formatted) != formatted {
            eprintln!("fmt: 内部错误——格式化结果不幂等，已拒绝写入 {}", f);
            std::process::exit(1);
        }
        let after = match lexer::lex(&formatted) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("fmt: 格式化后 {} 无法重新词法分析，已拒绝写入：{}", f, e);
                std::process::exit(1);
            }
        };
        let same_tokens = before.len() == after.len()
            && before.iter().zip(after.iter()).all(|(x, y)| x.kind == y.kind);
        if !same_tokens {
            eprintln!("fmt: 格式化改变了 token 流，已拒绝写入 {}", f);
            std::process::exit(1);
        }
        if dialfmt::protected_spans(&formatted) != protected_before {
            eprintln!("fmt: 格式化改变了字面量/注释原文，已拒绝写入 {}", f);
            std::process::exit(1);
        }
        if formatted == src {
            if !check_only && !diff_only {
                println!("  无需改动: {}", f);
            }
            continue;
        }
        need_fmt = true;
        if check_only {
            println!("  需要格式化: {}", f);
        } else if diff_only {
            print_fmt_diff(f, &src, &formatted);
        } else {
            if let Err(e) = fs::write(f, &formatted) {
                eprintln!("fmt: 写入 {} 失败: {}", f, e);
                std::process::exit(1);
            }
            println!("✅ 已格式化: {}", f);
        }
    }
    if check_only || diff_only {
        if need_fmt {
            eprintln!(
                "有文件未格式化（{} 模式不写入）。",
                if check_only { "--check" } else { "--diff" }
            );
            std::process::exit(1);
        }
        println!("✅ 所有文件已符合格式");
    }
}

/// `dialc fmt --diff`：逐行对照，只打印发生变化的行（不写文件）。
fn print_fmt_diff(file: &str, before: &str, after: &str) {
    println!("--- {}", file);
    println!("+++ {}", file);
    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();
    for i in 0..b.len().max(a.len()) {
        let lb = b.get(i).copied().unwrap_or("");
        let la = a.get(i).copied().unwrap_or("");
        if lb != la {
            println!("@@ 第 {} 行", i + 1);
            println!("-{}", lb);
            println!("+{}", la);
        }
    }
}

/// `dialc test`：把每个顶层 `test "名" { ... }` 块**单独**编成一个可执行文件运行。
///
/// 为什么每块一个进程：assert 失败按 Fail-Fast 铁律会立刻发非法指令终止进程，
/// 若把所有测试塞进同一个二进制，第一块失败就跑不到后面的块了。独立进程
/// 让「通过 / 失败」逐块可见，也互不污染（全局状态、ARC 运行时都是干净的）。
fn cmd_test(args: &[String]) {
    let mut files: Vec<String> = Vec::new();
    let mut filter: Option<String> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--filter" => {
                i += 1;
                match args.get(i) {
                    Some(v) => filter = Some(v.clone()),
                    None => {
                        eprintln!("--filter 后面缺少名字");
                        std::process::exit(1);
                    }
                }
            }
            a if a.starts_with('-') => {
                eprintln!("test: 未知参数 '{}'（可用: --filter <名字>）", a);
                std::process::exit(1);
            }
            _ => files.push(args[i].clone()),
        }
        i += 1;
    }
    if files.len() != 1 {
        eprintln!("用法: dialc test <文件.dial> [--filter <名字>]");
        std::process::exit(1);
    }
    let src = files.remove(0);
    let fe = match frontend(&src, &mut |_| {}) {
        Ok(f) => f,
        Err(e) => {
            diagnostic::report(&e, &src);
            std::process::exit(1);
        }
    };
    let tests = fe.raw_program.tests.clone();
    let selected: Vec<_> = tests
        .iter()
        .filter(|t| filter.as_ref().map(|f| t.name.contains(f)).unwrap_or(true))
        .collect();
    if selected.is_empty() {
        println!("没有匹配的 test 块（文件 {} 共 {} 块）。", src, tests.len());
        return;
    }

    let tmp = env::temp_dir().join(format!("dialc-test-{}", std::process::id()));
    if let Err(e) = fs::create_dir_all(&tmp) {
        eprintln!("创建测试临时目录失败: {}", e);
        std::process::exit(1);
    }

    println!("== dialc test: {}（{} 块）==", src, selected.len());
    let mut passed = 0usize;
    let mut failed = 0usize;
    for (idx, t) in selected.iter().enumerate() {
        // 每块都用「analyze 之前」的原始 AST 副本，避免重复 desugar。
        let mut prog = fe.raw_program.clone();
        prog.tests.clear();
        prog.functions.retain(|f| f.name != "main");
        let mut body = t.body.clone();
        body.push(ast::Stmt::Return(Some(ast::Expr::IntLit(0)), t.line, t.col));
        prog.functions.push(ast::Func {
            name: "main".to_string(),
            params: Vec::new(),
            defaults: Vec::new(),
            variadic: None,
            param_pos: Vec::new(),
            ret: ast::Type::Int,
            body,
            line: t.line,
            col: t.col,
            exported: false,
        });
        let (functions, structs) = match analyze(&mut prog, &fe.externs) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  ❌ {} 语义检查失败：{}", t.name, e);
                failed += 1;
                continue;
            }
        };
        let obj = tmp.join(format!("t{}.o", idx));
        let exe = tmp.join(format!("t{}", idx));
        let cg = codegen::CgOpts {
            safety: SafetyMode::Debug,
            aggressive: false,
            bc_out: None,
            src_file: src.clone(),
            debug_info: false, // dialc test 只跑断言，不需要调试符号
        };
        if let Err(e) = codegen::compile(&prog, &functions, &structs, &obj, &cg, &fe.externs) {
            eprintln!("  ❌ {} 代码生成失败：{}", t.name, e);
            failed += 1;
            continue;
        }
        if let Err(e) = link(
            &obj,
            &exe,
            SafetyMode::Debug,
            &fe.pkg_libs,
            &fe.ffi_rpaths,
            &LinkOpts {
                quiet: true,
                ..LinkOpts::default()
            },
        ) {
            eprintln!("  ❌ {} 链接失败：{}", t.name, e);
            failed += 1;
            continue;
        }
        match Command::new(&exe).output() {
            Ok(o) if o.status.success() => {
                println!("  ✅ {}", t.name);
                passed += 1;
            }
            Ok(o) => {
                println!("  ❌ {}", t.name);
                let err = String::from_utf8_lossy(&o.stderr);
                for line in err.lines().take(3) {
                    println!("       {}", line);
                }
                let out = String::from_utf8_lossy(&o.stdout);
                for line in out.lines().take(3) {
                    println!("       | {}", line);
                }
                failed += 1;
            }
            Err(e) => {
                println!("  ❌ {} 无法运行：{}", t.name, e);
                failed += 1;
            }
        }
    }
    let _ = fs::remove_dir_all(&tmp);
    println!("---- 通过 {} / 失败 {} ----", passed, failed);
    if failed > 0 {
        std::process::exit(1);
    }
}
