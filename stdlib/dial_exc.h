#ifndef DIAL_EXC_H
#define DIAL_EXC_H

#include <setjmp.h>
#include <stdlib.h>

/*
 * 异常缓冲区：存放 jmp_buf 与异常载荷。
 * 对 LLVM IR 只暴露为 i8*（DialExBuf*），jmp_buf 的平台相关布局与 setjmp/longjmp 的
 * returns_twice / noreturn 语义全部藏在 C 侧——IR 通过 dial_try / dial_throw_to /
 * dial_exbuf_err / dial_exbuf_take_err 这些普通 C 函数间接使用，避免 IR 直接处理
 * setjmp 的平台相关细节（也绕开各 LLVM 版本属性 API 的差异）。
 *
 * 这样异常机制对 Dial 用户完全透明，且实现稳定可移植。
 */
typedef struct DialExBuf {
    jmp_buf env;
    void   *err; /* 异常载荷（DialError，ARC）；dial_throw_to 写入、dial_exbuf_err/take_err 读出 */
} DialExBuf;

/* 创建 / 销毁（malloc / free）。dial_exbuf_free 会防御性释放残留的 err。 */
DialExBuf *dial_exbuf_new(void);
void       dial_exbuf_free(DialExBuf *b);

/* 返回 jmp_buf 字段的地址（i8*），供 IR **直接**调用 setjmp(env_ptr)。
 *
 * 注意：setjmp 必须由「包含 try 的那个函数」亲自调用，绝不能包在某个 C 助手函数里再返回——
 * jmp_buf 里保存的是调用点的栈指针，助手函数一返回，那个栈帧就没了，之后再 longjmp 回来就是
 * 跳进一个已死的栈帧（未定义行为，实测当场 SIGTRAP 崩溃）。因此这里只把 jmp_buf 的地址交出去，
 * setjmp 仍由 IR 侧发起；而 longjmp 可以由本文件的 dial_throw_to 发起（跳转目标栈帧仍然存活）。 */
void      *dial_exbuf_env_ptr(DialExBuf *b);

/* 等价于 `b->err = err; longjmp(b->env, 1)`：写入异常载荷后跳转回 **IR 侧 setjmp 的调用点**。
 * 覆盖写入前先释放 b->err 的旧值（re-throw 安全）；longjmp 不返回。 */
void       dial_throw_to(DialExBuf *b, void *err);

/* 读出最近一次抛出的错误对象（catch 块据此绑定 e 或释放）。不转移所有权。 */
void      *dial_exbuf_err(DialExBuf *b);

/* 取出错误对象并清空 b->err（转移所有权给调用方），使随后的 dial_exbuf_free 不会重复释放。 */
void      *dial_exbuf_take_err(DialExBuf *b);

/* 未捕获异常（throw 出现在任何 try 之外）：打印消息并终止进程，绝不静默继续。 */
void       dial_panic(const char *msg);

#endif /* DIAL_EXC_H */
