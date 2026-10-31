/*
 * Windows 上的 llvm-config 垫片 —— 由 MinGW gcc 编译成真正的 llvm-config.exe
 * ============================================================================
 *
 * 为什么必须是 .exe，而不能是 .cmd/.bat 脚本：
 *   llvm-sys 的 llvm_config_binary_names() 在 Windows 上先尝试 "llvm-config.exe"，
 *   再尝试裸名 "llvm-config"；而 Windows 的 CreateProcess 对**不带扩展名**的程序名
 *   只会补 ".exe"，不会补 ".cmd"/".bat"。因此 .cmd 垫片永远不会被 llvm-sys 找到，
 *   表现为 "No suitable version of LLVM was found"。
 *
 * 为什么需要一个垫片，而不能用官方包自带的 llvm-config.exe：
 *   官方 clang+llvm-18.1.8-x86_64-pc-windows-msvc 包里的 llvm-config.exe 运行时
 *   依赖 LLVM-18.dll（该包未提供，只有 LLVM-C.dll），直接调用会报
 *   "llvm-config: error: LLVM-18.dll is missing"。
 *
 * 垫片让 llvm-sys 把 LLVM 18 当作**纯 C 接口**来链接：只链导入库 LLVM-C.lib，
 *   运行期加载 LLVM-C.dll（纯 C ABI，与 MinGW 无 C++ 名字改写冲突）。
 *   llvm-sys 自己编译的 wrappers/target.c 只用到 llvm-c/Target.h 的 C API
 *   （LLVMInitializeAll*），所以只链 LLVM-C.lib 是充分的。
 *
 * 本文件精确覆盖 llvm-sys 181.3.1 build.rs 实际发出的全部查询：
 *   --version / --build-mode / --libdir / --cflags /
 *   --libnames (--link-static|--link-shared) /
 *   --system-libs (--link-static|--link-shared)
 * ============================================================================
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    const char *dir = getenv("LLVM18_DIR");
    const char *a;

    if (dir == NULL || dir[0] == '\0') {
        dir = "C:\\llvm18";
    }
    if (argc < 2) {
        return 0;
    }
    a = argv[1];

    if (strcmp(a, "--version") == 0) {
        printf("18.1.8\n");
        return 0;
    }
    if (strcmp(a, "--build-mode") == 0) {
        printf("Release\n"); /* 不能是 Debug，否则 llvm-sys 会改用调试运行时 */
        return 0;
    }
    if (strcmp(a, "--libdir") == 0) {
        printf("%s\\lib\n", dir);
        return 0;
    }
    if (strcmp(a, "--includedir") == 0) {
        printf("%s\\include\n", dir);
        return 0;
    }
    if (strcmp(a, "--host-target") == 0) {
        printf("x86_64-pc-windows-msvc\n");
        return 0;
    }
    if (strcmp(a, "--targets-built") == 0) {
        printf("X86;AArch64;ARM;WebAssembly\n");
        return 0;
    }
    /* 声明为 static：llvm-sys 默认 prefer_static，会先走 --link-static 分支 */
    if (strcmp(a, "--shared-mode") == 0) {
        printf("static\n");
        return 0;
    }
    if (strcmp(a, "--has-rtti") == 0) {
        printf("YES\n");
        return 0;
    }
    if (strcmp(a, "--cflags") == 0 || strcmp(a, "--cxxflags") == 0) {
        printf("-I%s\\include -D__STDC_CONSTANT_MACROS -D__STDC_FORMAT_MACROS "
               "-D__STDC_LIMIT_MACROS\n",
               dir);
        return 0;
    }
    /*
     * --libnames：--link-static 与 --link-shared 都返回 C 接口导入库。
     * extract_library()（Static 分支）会把 "LLVM-C.lib" 去掉 ".lib" 变成
     * "-lLLVM-C"，从而链接到 LLVM-C.lib，运行期加载 LLVM-C.dll。
     */
    if (strcmp(a, "--libnames") == 0) {
        printf("LLVM-C.lib\n");
        return 0;
    }
    /* --libs：链接标志形式，与 --libnames 等价，防止 llvm-sys 走 --libs 分支时落空 */
    if (strcmp(a, "--libs") == 0) {
        printf("-L%s\\lib -lLLVM-C\n", dir);
        return 0;
    }
    if (strcmp(a, "--ldflags") == 0) {
        printf("-L%s\\lib\n", dir);
        return 0;
    }
    /* --system-libs：C 接口不依赖额外的系统库（zlib 等已由 LLVM-C.dll 内部处理） */
    if (strcmp(a, "--system-libs") == 0) {
        return 0;
    }
    if (strcmp(a, "--components") == 0) {
        return 0;
    }

    /* 未知查询：输出空，避免 llvm-sys 解析到意外内容 */
    return 0;
}
