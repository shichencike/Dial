#include "dial_error.h"
#include "dial_string.h"
#include <stdlib.h>

struct DialErrorImpl {
    int64_t rc;
    void   *msg; /* DialString（ARC） */
};

DialError dial_error_new(void *msg) {
    struct DialErrorImpl *e = (struct DialErrorImpl *)malloc(sizeof(*e));
    e->rc = 1;
    e->msg = msg;
    dial_string_retain((DialString)msg);
    return (DialError)e;
}

void *dial_error_msg(DialError e) {
    if (!e) return NULL;
    return ((struct DialErrorImpl *)e)->msg;
}

void dial_error_retain(DialError e) {
    if (!e) return;
    ((struct DialErrorImpl *)e)->rc++;
}

void dial_error_release(DialError e) {
    if (!e) return;
    struct DialErrorImpl *p = (struct DialErrorImpl *)e;
    if (--(p->rc) <= 0) {
        dial_string_release((DialString)p->msg);
        free(p);
    }
}
