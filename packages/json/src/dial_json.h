/*
 * dial_json.h —— json.dialpkg 内部节点结构与 C 接口。
 *
 * 为什么用「不透明句柄 + 访问器」而不是返回 map/数组：`.diali` 的类型白名单里
 * 只有裸 `map`（int→int），没有泛型容器，Dial 侧拿不到 string→任意值的字典。
 * 因此 JSON 值树整体封在 `type Json;` 里，与 http 包的 HttpResponse 同一口径。
 *
 * 所有权（写进 json.diali，务必遵守）：**所有返回 Json 的函数都返回独立的、
 * 拥有所有权的句柄**，调用方一律 dial_json_free。array_push / object_set 对
 * child 做**深拷贝**，所以传进去的 child 仍需由调用方释放，且之后改动 child
 * 不影响父树。这样没有「借用句柄」与双重释放的陷阱，符合「谁申请谁释放」。
 *
 * 协议层自研：递归下降解析 + 紧凑序列化，纯 C 零依赖（README §21.2）。
 */
#ifndef DIAL_JSON_H
#define DIAL_JSON_H

#include <stdint.h>

typedef enum {
    JS_NULL = 0,
    JS_BOOL,
    JS_NUM,
    JS_STR,
    JS_ARR,
    JS_OBJ
} JsonType;

typedef struct JsonNode {
    JsonType          type;
    int               bval;      /* JS_BOOL */
    double            num;       /* JS_NUM：浮点 / 非整数值 */
    int64_t           ival;      /* JS_NUM：整数值单独存，避免 double 丢精度（>2^53）*/
    int               is_int;    /* 数字是否为整数（此时用 ival，不用 num）*/
    char             *str;       /* JS_STR（UTF-8，NUL 结尾）*/
    struct JsonNode **items;     /* JS_ARR */
    int               nitems, icap;
    char            **keys;      /* JS_OBJ：与 vals 一一对应 */
    struct JsonNode **vals;
    int               nkeys, kcap;
} JsonNode;

/* ---- codec（纯 C，不依赖 Dial stdlib）---- */

/* 解析定长 UTF-8 文本。成功返回节点，失败返回 NULL 并写 err / errpos（字节偏移）。 */
JsonNode *js_parse(const char *text, int len, char *err, int errlen, int *errpos);

/* 序列化为紧凑 JSON 文本（malloc，调用方 free）。不含内嵌 NUL。 */
char *js_stringify(const JsonNode *n);

/* 深拷贝 / 递归释放。 */
JsonNode *js_clone(const JsonNode *n);
void      js_free(JsonNode *n);

/* ---- FFI 包装（dial_json.c 实现，签名对应 json.diali）---- */
#include "dial_string.h"

#define DIAL_JSON_OK          0
#define DIAL_JSON_ERR_PARSE   (-1)
#define DIAL_JSON_ERR_TYPE    (-2)
#define DIAL_JSON_ERR_RANGE   (-3)
#define DIAL_JSON_ERR_NOMEM   (-4)

JsonNode  *dial_json_parse(DialString text);
int64_t    dial_json_free(JsonNode *v);
DialString dial_json_type(JsonNode *v);
int32_t    dial_json_is_null(JsonNode *v);
int32_t    dial_json_is_bool(JsonNode *v);
int32_t    dial_json_is_number(JsonNode *v);
int32_t    dial_json_is_string(JsonNode *v);
int32_t    dial_json_is_array(JsonNode *v);
int32_t    dial_json_is_object(JsonNode *v);
int32_t    dial_json_bool(JsonNode *v);
int64_t    dial_json_int(JsonNode *v);
double     dial_json_float(JsonNode *v);
DialString dial_json_string(JsonNode *v);
int64_t    dial_json_len(JsonNode *v);
JsonNode  *dial_json_item(JsonNode *v, int64_t index);
JsonNode  *dial_json_get(JsonNode *v, DialString key);
DialString dial_json_key(JsonNode *v, int64_t index);
DialString dial_json_stringify(JsonNode *v);

JsonNode  *dial_json_new_null(void);
JsonNode  *dial_json_new_bool(int32_t b);
JsonNode  *dial_json_new_int(int64_t n);
JsonNode  *dial_json_new_float(double x);
JsonNode  *dial_json_new_string(DialString s);
JsonNode  *dial_json_new_array(void);
JsonNode  *dial_json_new_object(void);
int64_t    dial_json_array_push(JsonNode *arr, JsonNode *child);
int64_t    dial_json_object_set(JsonNode *obj, DialString key, JsonNode *child);

int64_t    dial_json_last_error(void);
DialString dial_json_last_error_msg(void);

#endif /* DIAL_JSON_H */
