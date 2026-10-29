#include "dial_exc.h"
#include "dial_error.h"
#include <stdio.h>
#include <setjmp.h>
#include <stdlib.h>

DialExBuf *dial_exbuf_new(void) {
    DialExBuf *b = (DialExBuf *)malloc(sizeof(DialExBuf));
    b->err = NULL;
    return b;
}

void dial_exbuf_free(DialExBuf *b) {
    /* 防御性：若缓冲区里还残留未取走的错误（理论上正常/捕获路径已 take 走，此处为兜底），释放之。 */
    if (b->err) dial_error_release(b->err);
    free(b);
}

void *dial_exbuf_env_ptr(DialExBuf *b) {
    /* 把 jmp_buf 的地址交给 IR——setjmp 由 IR 侧（包含 try 的函数）自行调用，
     * 这样 jmp_buf 记录的栈帧在整个 try 期间都活着，longjmp 回来才合法。 */
    return (void *)&b->env;
}

void dial_throw_to(DialExBuf *b, void *err) {
    /* re-throw 安全：覆盖写入前先释放上一次残留的错误对象。 */
    if (b->err) dial_error_release(b->err);
    b->err = err;
    longjmp(b->env, 1);
}

void *dial_exbuf_err(DialExBuf *b) {
    return b->err;
}

void *dial_exbuf_take_err(DialExBuf *b) {
    void *e = b->err;
    b->err = NULL; /* 所有权转移给调用方 */
    return e;
}

void dial_panic(const char *msg) {
    fprintf(stderr, "dial: 未捕获的异常（panic）: %s\n", msg ? msg : "(null)");
    abort();
}
