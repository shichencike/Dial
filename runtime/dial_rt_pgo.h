#ifndef DIAL_RT_PGO_H
#define DIAL_RT_PGO_H

/*
 * 自研 PGO profile 写回（R4）。训练版二进制在 main 返回前调用本函数。
 * counters: 计数字节数组的指针（每个元素 8 字节，uint64）
 * n:        计数器个数
 * 输出路径取自环境变量 DIAL_PGO_FILE；未设置则 no-op。
 *
 * 注意：用 unsigned long long* 而非 uint64_t*，避免各平台 uint64_t typedef 差异；
 * x86-64 SysV 下二者都是 8 字节，Dial 侧 .comm 按 8 字节对齐分配，ABI 一致。
 */
void dial_pgo_dump(const unsigned long long *counters, unsigned int n);

#endif /* DIAL_RT_PGO_H */
