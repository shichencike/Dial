@echo off
rem ============================================================================
rem  Windows 上的 llvm-config 垫片（仅用于 CI 在 MinGW 目标下构建 dialc）
rem
rem  背景：dialc 绑定 LLVM 18（inkwell 181）。2025 年之后，MSYS2 已不再提供
rem  LLVM 18、WinLibs 的 18.x 也下架；而 Alpine 3.22 恰好只提供 llvm18，因此
rem  LLVM 18 必须保留（否则 Termux/musl 路线会断）。Windows 上唯一确定可得的
rem  LLVM 18 是官方 MSVC 全量包（clang+llvm-18.1.8-x86_64-pc-windows-msvc）。
rem
rem  该包提供 C API 的「导入库 LLVM-C.lib + 运行时 LLVM-C.dll」，二者是纯 C ABI，
rem  不存在 C++ 名字改写冲突——所以 MinGW 目标完全可以直接链接它（与官方 MSVC
rem  构建的 LLVM 静态库那种 C++ ABI 冲突不同）。本垫片让 llvm-sys 把 LLVM 当成
rem  C 接口来链：dialc.exe 运行期只需同目录的 LLVM-C.dll，用户程序（由 MinGW gcc
rem  + winpthreads 编译）完全不碰 LLVM。
rem
rem  本文件是 llvm-sys 181.3.1 build.rs 实际查询的精确回应集合，不含任何额外逻辑。
rem  通过 LLVM_SYS_181_PREFIX 指向本文件所在目录（llvm-sys 会找 <prefix>/bin/llvm-config）。
rem ============================================================================

setlocal EnableExtensions
if not defined LLVM18_DIR set "LLVM18_DIR=C:\llvm18"
set "ARG1=%~1"
set "ARG2=%~2"

if "%ARG1%"=="--version"        ( echo 18.1.8 & exit /b 0 )
if "%ARG1%"=="--build-mode"     ( echo Release & exit /b 0 )
if "%ARG1%"=="--libdir"         ( echo %LLVM18_DIR%\lib & exit /b 0 )
if "%ARG1%"=="--host-target"    ( echo x86_64-pc-windows-msvc & exit /b 0 )
if "%ARG1%"=="--targets-built"  ( echo X86;AArch64;ARM;WebAssembly & exit /b 0 )
if "%ARG1%"=="--shared-mode"    ( echo shared & exit /b 0 )
if "%ARG1%"=="--has-rtti"       ( echo YES & exit /b 0 )
if "%ARG1%"=="--cflags"         ( echo -I%LLVM18_DIR%\include -D__STDC_CONSTANT_MACROS -D__STDC_FORMAT_MACROS -D__STDC_LIMIT_MACROS & exit /b 0 )
rem --system-libs：C API 不依赖额外的系统库（zlib 等已由 LLVM-C.dll 内部处理）
if "%ARG1%"=="--system-libs"    ( exit /b 0 )
rem --libnames --link-static | --link-shared：两种模式都返回 C 接口的导入库名。
rem   extract_library() 在 static/dynamic 两种分支下都会把 "LLVM-C.lib" 解析成 "-lLLVM-C"，
rem   从而链接到 LLVM-C.lib 导入库，运行期加载 LLVM-C.dll。
if "%ARG1%"=="--libnames"       ( echo LLVM-C.lib & exit /b 0 )

rem 未知查询：输出空，避免 llvm-sys 解析到意外内容
echo.
exit /b 0
