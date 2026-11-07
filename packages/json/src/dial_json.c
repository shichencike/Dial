/*
 * dial_json.c —— json.dialpkg 的 FFI 包装层。
 *
 * 解析器 / 序列化器在 dial_json_codec.c（纯 C）。本文件只做 DialString 转换、
 * 不透明句柄生命周期、访问器与构造器、线程局部错误槽。
 *
 * 所有权（与 json.diali 一致）：所有返回 Json 的函数都返回**独立拥有**的句柄，
 * 调用方一律 dial_json_free；array_push / object_set 深拷贝 child，故 child
 * 仍需调用方释放。这样没有借用句柄 / 双重释放的灰色地带。
 */
#include "dial_json.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static _Thread_local int64_t g_err = DIAL_JSON_OK;
static _Thread_local char    g_err_msg[256] = {0};

static void je_set(int64_t code, const char *msg) {
    g_err = code;
    if (!msg) msg = "";
    strncpy(g_err_msg, msg, sizeof(g_err_msg) - 1);
    g_err_msg[sizeof(g_err_msg) - 1] = '\0';
}
static void je_ok(void) { je_set(DIAL_JSON_OK, ""); }

int64_t dial_json_last_error(void) { return g_err; }
DialString dial_json_last_error_msg(void) {
    return dial_string_new(g_err_msg[0] ? g_err_msg : "无错误");
}

/* DialString → (字节指针, 长度)：一律按长度处理，不假设内容。 */
static const char *j_data(DialString s, int64_t *len) {
    if (!s) { *len = 0; return ""; }
    *len = dial_string_length(s);
    return dial_string_cstr(s);
}
/* 复制 DialString 成独立 C 串（NUL 结尾）。 */
static char *j_dup(DialString s) {
    int64_t n = 0;
    const char *p = j_data(s, &n);
    char *out = (char *)malloc((size_t)n + 1);
    if (!out) return NULL;
    memcpy(out, p, (size_t)n);
    out[n] = '\0';
    return out;
}

/* 空句柄统一检查：返回 1 表示可用。 */
static int j_ready(const JsonNode *v) {
    if (!v) { je_set(DIAL_JSON_ERR_TYPE, "空 Json 句柄"); return 0; }
    return 1;
}

/* ---------------------------------------------------------------- parse / free / stringify */

JsonNode *dial_json_parse(DialString text) {
    int64_t n = 0;
    const char *p = j_data(text, &n);
    char err[256];
    int  epos = -1;
    JsonNode *v = js_parse(p, (int)n, err, (int)sizeof(err), &epos);
    if (!v) {
        char full[300];
        snprintf(full, sizeof(full), "JSON 解析失败（字节偏移 %d）：%s", epos, err);
        je_set(DIAL_JSON_ERR_PARSE, full);
        return NULL;
    }
    je_ok();
    return v;
}

int64_t dial_json_free(JsonNode *v) {
    if (!v) { je_set(DIAL_JSON_ERR_TYPE, "空 Json 句柄"); return -1; }
    js_free(v);
    je_ok();
    return 0;
}

DialString dial_json_stringify(JsonNode *v) {
    if (!j_ready(v)) return dial_string_new("");
    char *s = js_stringify(v);
    if (!s) {
        je_set(DIAL_JSON_ERR_NOMEM, "内存不足：序列化失败");
        return dial_string_new("");
    }
    DialString out = dial_string_new(s);
    free(s);
    je_ok();
    return out;
}

/* ---------------------------------------------------------------- 类型查询 */

DialString dial_json_type(JsonNode *v) {
    if (!j_ready(v)) return dial_string_new("");
    const char *t = "null";
    switch (v->type) {
        case JS_NULL: t = "null";   break;
        case JS_BOOL: t = "bool";   break;
        case JS_NUM:  t = "number"; break;
        case JS_STR:  t = "string"; break;
        case JS_ARR:  t = "array";  break;
        case JS_OBJ:  t = "object"; break;
        default: break;
    }
    je_ok();
    return dial_string_new(t);
}

static int32_t j_is(JsonNode *v, JsonType t) {
    if (!j_ready(v)) return 0;
    je_ok();
    return v->type == t ? 1 : 0;
}
int32_t dial_json_is_null(JsonNode *v)   { return j_is(v, JS_NULL); }
int32_t dial_json_is_bool(JsonNode *v)   { return j_is(v, JS_BOOL); }
int32_t dial_json_is_number(JsonNode *v) { return j_is(v, JS_NUM); }
int32_t dial_json_is_string(JsonNode *v) { return j_is(v, JS_STR); }
int32_t dial_json_is_array(JsonNode *v)  { return j_is(v, JS_ARR); }
int32_t dial_json_is_object(JsonNode *v) { return j_is(v, JS_OBJ); }

/* ---------------------------------------------------------------- 标量访问 */

int32_t dial_json_bool(JsonNode *v) {
    if (!j_ready(v)) return 0;
    if (v->type != JS_BOOL) { je_set(DIAL_JSON_ERR_TYPE, "该节点不是 bool"); return 0; }
    je_ok();
    return v->bval ? 1 : 0;
}

int64_t dial_json_int(JsonNode *v) {
    if (!j_ready(v)) return 0;
    if (v->type != JS_NUM) { je_set(DIAL_JSON_ERR_TYPE, "该节点不是 number"); return 0; }
    je_ok();
    return v->is_int ? v->ival : (int64_t)v->num;
}

double dial_json_float(JsonNode *v) {
    if (!j_ready(v)) return 0.0;
    if (v->type != JS_NUM) { je_set(DIAL_JSON_ERR_TYPE, "该节点不是 number"); return 0.0; }
    je_ok();
    return v->is_int ? (double)v->ival : v->num;
}

DialString dial_json_string(JsonNode *v) {
    if (!j_ready(v)) return dial_string_new("");
    if (v->type != JS_STR) { je_set(DIAL_JSON_ERR_TYPE, "该节点不是 string"); return dial_string_new(""); }
    je_ok();
    return dial_string_new(v->str ? v->str : "");
}

/* ---------------------------------------------------------------- 容器访问 */

int64_t dial_json_len(JsonNode *v) {
    if (!j_ready(v)) return -1;
    if (v->type == JS_ARR) { je_ok(); return (int64_t)v->nitems; }
    if (v->type == JS_OBJ) { je_ok(); return (int64_t)v->nkeys; }
    je_set(DIAL_JSON_ERR_TYPE, "len 只适用于 array / object");
    return -1;
}

JsonNode *dial_json_item(JsonNode *v, int64_t index) {
    if (!j_ready(v)) return NULL;
    if (v->type != JS_ARR || index < 0 || index >= (int64_t)v->nitems) {
        je_set(DIAL_JSON_ERR_RANGE, "数组下标越界（或该节点不是 array）");
        return NULL;
    }
    JsonNode *c = js_clone(v->items[(int)index]);
    if (!c) { je_set(DIAL_JSON_ERR_NOMEM, "内存不足：克隆节点失败"); return NULL; }
    je_ok();
    return c;
}

JsonNode *dial_json_get(JsonNode *v, DialString key) {
    if (!j_ready(v)) return NULL;
    char *k = j_dup(key);
    if (!k) { je_set(DIAL_JSON_ERR_NOMEM, "内存不足"); return NULL; }
    JsonNode *found = NULL;
    if (v->type == JS_OBJ) {
        for (int i = 0; i < v->nkeys; i++) {
            if (strcmp(v->keys[i], k) == 0) { found = v->vals[i]; break; }
        }
    }
    free(k);
    if (!found) {
        je_set(DIAL_JSON_ERR_RANGE, "对象键不存在（或该节点不是 object）");
        return NULL;
    }
    JsonNode *c = js_clone(found);
    if (!c) { je_set(DIAL_JSON_ERR_NOMEM, "内存不足：克隆节点失败"); return NULL; }
    je_ok();
    return c;
}

DialString dial_json_key(JsonNode *v, int64_t index) {
    if (!j_ready(v)) return dial_string_new("");
    if (v->type != JS_OBJ || index < 0 || index >= (int64_t)v->nkeys) {
        je_set(DIAL_JSON_ERR_RANGE, "对象键下标越界（或该节点不是 object）");
        return dial_string_new("");
    }
    je_ok();
    return dial_string_new(v->keys[(int)index] ? v->keys[(int)index] : "");
}

/* ---------------------------------------------------------------- 构造 */

static JsonNode *j_make(JsonType t) {
    JsonNode *n = (JsonNode *)calloc(1, sizeof(JsonNode));
    if (!n) { je_set(DIAL_JSON_ERR_NOMEM, "内存不足：无法创建节点"); return NULL; }
    n->type = t;
    je_ok();
    return n;
}

JsonNode *dial_json_new_null(void)   { return j_make(JS_NULL); }
JsonNode *dial_json_new_array(void)  { return j_make(JS_ARR); }
JsonNode *dial_json_new_object(void) { return j_make(JS_OBJ); }

JsonNode *dial_json_new_bool(int32_t b) {
    JsonNode *n = j_make(JS_BOOL);
    if (n) n->bval = b ? 1 : 0;
    return n;
}
JsonNode *dial_json_new_int(int64_t x) {
    JsonNode *n = j_make(JS_NUM);
    if (n) { n->is_int = 1; n->ival = x; n->num = (double)x; }
    return n;
}
JsonNode *dial_json_new_float(double x) {
    JsonNode *n = j_make(JS_NUM);
    if (n) { n->is_int = 0; n->num = x; }
    return n;
}
JsonNode *dial_json_new_string(DialString s) {
    JsonNode *n = j_make(JS_STR);
    if (!n) return NULL;
    n->str = j_dup(s);
    if (!n->str) { free(n); je_set(DIAL_JSON_ERR_NOMEM, "内存不足"); return NULL; }
    return n;
}

int64_t dial_json_array_push(JsonNode *arr, JsonNode *child) {
    if (!j_ready(arr) || !child) {
        je_set(DIAL_JSON_ERR_TYPE, "array_push 需要非空的 arr 与 child");
        return -1;
    }
    if (arr->type != JS_ARR) { je_set(DIAL_JSON_ERR_TYPE, "arr 不是 array"); return -1; }
    JsonNode *c = js_clone(child);
    if (!c) { je_set(DIAL_JSON_ERR_NOMEM, "内存不足：克隆节点失败"); return -1; }
    int nc = arr->icap ? arr->icap * 2 : 8;
    if (arr->nitems >= arr->icap) {
        JsonNode **np = (JsonNode **)realloc(arr->items, (size_t)nc * sizeof(JsonNode *));
        if (!np) { js_free(c); je_set(DIAL_JSON_ERR_NOMEM, "内存不足：扩容数组失败"); return -1; }
        arr->items = np; arr->icap = nc;
    }
    arr->items[arr->nitems++] = c;
    je_ok();
    return 0;
}

int64_t dial_json_object_set(JsonNode *obj, DialString key, JsonNode *child) {
    if (!j_ready(obj) || !child) {
        je_set(DIAL_JSON_ERR_TYPE, "object_set 需要非空的 obj 与 child");
        return -1;
    }
    if (obj->type != JS_OBJ) { je_set(DIAL_JSON_ERR_TYPE, "obj 不是 object"); return -1; }
    char *k = j_dup(key);
    JsonNode *c = js_clone(child);
    if (!k || !c) {
        free(k); js_free(c);
        je_set(DIAL_JSON_ERR_NOMEM, "内存不足：构造键值对失败");
        return -1;
    }
    /* 重复键覆盖 */
    for (int i = 0; i < obj->nkeys; i++) {
        if (strcmp(obj->keys[i], k) == 0) {
            free(k);
            js_free(obj->vals[i]);
            obj->vals[i] = c;
            je_ok();
            return 0;
        }
    }
    if (obj->nkeys >= obj->kcap) {
        int nc = obj->kcap ? obj->kcap * 2 : 8;
        char **nk = (char **)realloc(obj->keys, (size_t)nc * sizeof(char *));
        JsonNode **nv = (JsonNode **)realloc(obj->vals, (size_t)nc * sizeof(JsonNode *));
        if (!nk || !nv) {
            free(nk); free(nv); free(k); js_free(c);
            je_set(DIAL_JSON_ERR_NOMEM, "内存不足：扩容对象失败");
            return -1;
        }
        obj->keys = nk; obj->vals = nv; obj->kcap = nc;
    }
    obj->keys[obj->nkeys] = k;
    obj->vals[obj->nkeys] = c;
    obj->nkeys++;
    je_ok();
    return 0;
}
