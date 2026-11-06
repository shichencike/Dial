/* P 组动态链接专项：编成共享库，由 Dial 侧 `link "...";` 显式链入。
 *
 * 与 ffi_demo.c（静态归档）分开，是为了同时守住两条路径：
 *   - 静态 .a：ar 魔数 + 直接进链接器输入，产物零第三方动态依赖；
 *   - 动态 .so/.dll/.dylib：ELF/MZ/Mach-O 魔数 + -Wl,-rpath 注入，
 *     产物运行时需要能找到这份库（P 组会断言 RUNPATH 已写入）。
 *
 * 注意：这里是**语言层 link** 的用户显式选择，不改变 .dialpkg 包分发
 * 仍强制静态的铁律（J 组三条硬规则一字未动）。
 */
#include <stdint.h>

#if defined(_WIN32)
#define DYN_EXPORT __declspec(dllexport)
#else
#define DYN_EXPORT
#endif

DYN_EXPORT int64_t dyn_add(int64_t a, int64_t b) {
    return a + b;
}

DYN_EXPORT int64_t dyn_scale_five(int64_t v) {
    return v * 5;
}
