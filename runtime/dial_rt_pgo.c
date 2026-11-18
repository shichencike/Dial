#include "dial_rt_pgo.h"

#include <stdio.h>
#include <stdlib.h>

/*
 * 自研 PGO 的 profile 写回（R4，唯一后端收回路线图第 4 项）。
 *
 * 训练版二进制在 main 返回前调用 dial_pgo_dump，把计数器数组写成 .dialprof：
 *   DIALPGO1\n
 *   N\n
 *   0 <count0>\n ... N-1 <countN-1>\n
 *
 * 格式与 Rust 侧 src/pgo.rs 的 ProfileData::parse 共享同一契约。
 * idx → 语义由编译器（codegen_asm::PgoPlan）按 IR 纯函数重算，文件不存符号表。
 *
 * 路径来自环境变量 DIAL_PGO_FILE（镜像 LLVM 的 LLVM_PROFILE_FILE）。
 * 未设置或为空 → no-op（普通运行不受影响）。
 * 用 %llu 打印 uint64_t：各平台 uint64_t 至少 64 位，强转后宽度一致、格式确定。
 */
void dial_pgo_dump(const unsigned long long *counters, unsigned int n) {
    const char *path = getenv("DIAL_PGO_FILE");
    if (path == NULL || path[0] == '\0') {
        return; /* 未启用 profile：普通运行零开销（仅一次 getenv） */
    }

    FILE *fp = fopen(path, "w");
    if (fp == NULL) {
        return; /* 写不开就静默跳过：profile 收集失败不应影响训练程序语义 */
    }

    fputs("DIALPGO1\n", fp);
    fprintf(fp, "%u\n", n);
    for (unsigned int i = 0; i < n; i++) {
        fprintf(fp, "%u %llu\n", i, counters[i]);
    }
    fclose(fp);
}
