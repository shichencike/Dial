#ifndef DIAL_ERROR_H
#define DIAL_ERROR_H

#include <stdint.h>

/*
 * 可恢复异常（Error）：带引用计数的不透明指针，与 string / map 同等对待（ARC）。
 * 内部实现含一条消息字符串（同样是 ARC）——共享所有权而非裸拷贝，杜绝复制导致的 double free。
 *
 * IR 层看到的是 i8*，类型系统里是内置的 error 类型。
 */
typedef void *DialError;

/* 用一条消息字符串（DialString，ARC）构造 Error；内部会对 msg 做 retain。 */
DialError dial_error_new(void *msg);

/* 取出消息字符串（DialString）。 */
void     *dial_error_msg(DialError e);

/* ARC：引用计数 +1 / -1（归零时释放消息并释放 Error 本身）。 */
void      dial_error_retain(DialError e);
void      dial_error_release(DialError e);

#endif /* DIAL_ERROR_H */
