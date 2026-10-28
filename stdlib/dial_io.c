#include "dial_io.h"
#include "dial_string.h"
#include <stdio.h>
#include <string.h>

void dial_print_int(int64_t v)   { printf("%lld\n", (long long)v); }
void dial_print_float(double v)  { printf("%lf\n", v); }
void dial_print_bool(int64_t b)  { printf("%s\n", b ? "true" : "false"); }

DialString dial_read_line(void) {
    char buf[4096];
    if (!fgets(buf, sizeof(buf), stdin)) return dial_string_new("");
    size_t n = strlen(buf);
    while (n > 0 && (buf[n - 1] == '\n' || buf[n - 1] == '\r')) buf[--n] = 0;
    return dial_string_new(buf);
}
