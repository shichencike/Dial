#include "dial_map.h"
#include <stdlib.h>

typedef struct { int64_t key; int64_t val; int8_t used; } Entry;
typedef struct { int64_t cap; int64_t count; Entry *items; } Map;

DialMap dial_map_new(void) {
    Map *m = (Map *)malloc(sizeof(Map));
    m->cap = 16;
    m->count = 0;
    m->items = (Entry *)calloc(16, sizeof(Entry));
    return m;
}

static int64_t idx(Map *m, int64_t k) {
    int64_t h = k < 0 ? -k : k;
    return h % m->cap;
}

void dial_map_put(DialMap mh, int64_t k, int64_t v) {
    Map *m = (Map *)mh;
    int64_t i = idx(m, k);
    while (m->items[i].used && m->items[i].key != k) i = (i + 1) % m->cap;
    if (!m->items[i].used) m->count++;
    m->items[i].key = k;
    m->items[i].val = v;
    m->items[i].used = 1;
}

int64_t dial_map_get(DialMap mh, int64_t k) {
    Map *m = (Map *)mh;
    int64_t i = idx(m, k);
    while (m->items[i].used) {
        if (m->items[i].key == k) return m->items[i].val;
        i = (i + 1) % m->cap;
    }
    return -1;
}
