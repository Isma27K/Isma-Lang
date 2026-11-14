// Isma runtime v0.1
// Linked into every Isma program. Kept deliberately small.

#include <errno.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    int64_t len;
    char data[];
} IsmaStr;

typedef struct {
    int64_t len;
    int64_t cap;
    char *data;
} IsmaList;

extern const char isma_source_file[];
extern void isma_main(void);

// ---------------------------------------------------------------- panics

static void isma_die(int line, int col, const char *msg) {
    fflush(stdout);
    fprintf(stderr, "panic at %s:%d:%d: %s\n", isma_source_file, line, col, msg);
    exit(101);
}

void isma_panic_msg(IsmaStr *msg, int line, int col) {
    fflush(stdout);
    fprintf(stderr, "panic at %s:%d:%d: %.*s\n", isma_source_file, line, col, (int)msg->len, msg->data);
    exit(101);
}

void isma_panic_overflow(int line, int col) { isma_die(line, col, "integer overflow"); }
void isma_panic_divzero(int line, int col) { isma_die(line, col, "division by zero"); }
void isma_assert_fail(int line, int col) { isma_die(line, col, "assertion failed"); }

void isma_panic_bounds(int64_t idx, int64_t len, int line, int col) {
    char buf[128];
    snprintf(buf, sizeof buf, "index %lld out of bounds for list of length %lld", (long long)idx, (long long)len);
    isma_die(line, col, buf);
}

void isma_exit(int64_t code) {
    fflush(stdout);
    exit((int)code);
}

// ---------------------------------------------------------------- memory
// v0.1: a bump arena. Nothing is ever freed (ARC is on the roadmap).

#define ARENA_CHUNK (1 << 20)
static char *arena_ptr = NULL;
static char *arena_end = NULL;

static void *oom(void) {
    fputs("fatal: out of memory\n", stderr);
    exit(134);
}

void *isma_alloc(int64_t size) {
    size = (size + 15) & ~(int64_t)15;
    if (size > ARENA_CHUNK / 4) {
        void *p = malloc((size_t)size);
        return p ? p : oom();
    }
    if (arena_end - arena_ptr < size) {
        arena_ptr = malloc(ARENA_CHUNK);
        if (!arena_ptr) oom();
        arena_end = arena_ptr + ARENA_CHUNK;
    }
    void *p = arena_ptr;
    arena_ptr += size;
    return p;
}

// ---------------------------------------------------------------- strings

static IsmaStr *str_new(int64_t len) {
    IsmaStr *s = isma_alloc((int64_t)sizeof(IsmaStr) + len);
    s->len = len;
    return s;
}

static IsmaStr *str_from(const char *data, int64_t len) {
    IsmaStr *s = str_new(len);
    memcpy(s->data, data, (size_t)len);
    return s;
}

IsmaStr *isma_str_concat(IsmaStr *a, IsmaStr *b) {
    if (a->len == 0) return b;
    if (b->len == 0) return a;
    IsmaStr *s = str_new(a->len + b->len);
    memcpy(s->data, a->data, (size_t)a->len);
    memcpy(s->data + a->len, b->data, (size_t)b->len);
    return s;
}

int64_t isma_str_eq(IsmaStr *a, IsmaStr *b) {
    return a->len == b->len && memcmp(a->data, b->data, (size_t)a->len) == 0;
}

int64_t isma_str_cmp(IsmaStr *a, IsmaStr *b) {
    int64_t n = a->len < b->len ? a->len : b->len;
    int c = memcmp(a->data, b->data, (size_t)n);
    if (c != 0) return c < 0 ? -1 : 1;
    return a->len < b->len ? -1 : (a->len > b->len ? 1 : 0);
}

IsmaStr *isma_int_to_str(int64_t v) {
    char buf[32];
    int n = snprintf(buf, sizeof buf, "%lld", (long long)v);
    return str_from(buf, n);
}

// Shortest representation that round-trips, formatted like Python's repr:
// 0.1 -> "0.1", 3.0 -> "3.0", 1e+16 -> "1e+16", 1.5e-07 -> "1.5e-07".
static int format_float(double d, char *out, size_t cap) {
    if (isnan(d)) return snprintf(out, cap, "nan");
    if (isinf(d)) return snprintf(out, cap, d < 0 ? "-inf" : "inf");
    if (d == 0) return snprintf(out, cap, signbit(d) ? "-0.0" : "0.0");
    char sci[40];
    int prec;
    for (prec = 0; prec < 17; prec++) {
        snprintf(sci, sizeof sci, "%.*e", prec, d);
        if (strtod(sci, NULL) == d) break;
    }
    // sci looks like "-d.ddde+XX": split digits and exponent.
    char digits[32];
    int nd = 0;
    int neg = sci[0] == '-';
    char *p = sci + neg;
    char *e = strchr(p, 'e');
    for (char *q = p; q < e; q++)
        if (*q != '.') digits[nd++] = *q;
    digits[nd] = 0;
    int exp = atoi(e + 1);
    int n = 0;
    if (neg) out[n++] = '-';
    if (exp < -4 || exp >= 16) {
        out[n++] = digits[0];
        if (nd > 1) {
            out[n++] = '.';
            for (int i = 1; i < nd; i++) out[n++] = digits[i];
        }
        n += snprintf(out + n, cap - n, "e%c%02d", exp < 0 ? '-' : '+', exp < 0 ? -exp : exp);
        return n;
    }
    if (exp < 0) {
        out[n++] = '0';
        out[n++] = '.';
        for (int i = 0; i < -exp - 1; i++) out[n++] = '0';
        for (int i = 0; i < nd; i++) out[n++] = digits[i];
    } else {
        for (int i = 0; i <= exp; i++) out[n++] = i < nd ? digits[i] : '0';
        out[n++] = '.';
        if (nd > exp + 1) {
            for (int i = exp + 1; i < nd; i++) out[n++] = digits[i];
        } else {
            out[n++] = '0';
        }
    }
    out[n] = 0;
    return n;
}

IsmaStr *isma_float_to_str(double d) {
    char buf[64];
    int n = format_float(d, buf, sizeof buf);
    return str_from(buf, n);
}

IsmaStr *isma_bool_to_str(int64_t b) {
    return b ? str_from("true", 4) : str_from("false", 5);
}

IsmaStr *isma_input(void) {
    char *line = NULL;
    size_t cap = 0;
    fflush(stdout);
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) {
        free(line);
        return str_new(0);
    }
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) n--;
    IsmaStr *s = str_from(line, n);
    free(line);
    return s;
}

// Parsing (like Kotlin's toInt(): invalid input is a panic, not a silent 0).
static int str_to_cbuf(IsmaStr *s, char *buf, size_t cap) {
    int64_t a = 0, b = s->len;
    while (a < b && (s->data[a] == ' ' || s->data[a] == '\t')) a++;
    while (b > a && (s->data[b - 1] == ' ' || s->data[b - 1] == '\t')) b--;
    if (b - a <= 0 || (size_t)(b - a) >= cap) return 0;
    memcpy(buf, s->data + a, (size_t)(b - a));
    buf[b - a] = 0;
    return 1;
}

static void bad_number(IsmaStr *s, const char *ty, int line, int col) {
    char buf[160];
    snprintf(buf, sizeof buf, "cannot parse \"%.*s\" as %s", s->len > 64 ? 64 : (int)s->len, s->data, ty);
    isma_die(line, col, buf);
}

int64_t isma_str_to_int(IsmaStr *s, int line, int col) {
    char buf[64], *end;
    if (!str_to_cbuf(s, buf, sizeof buf)) bad_number(s, "Int", line, col);
    errno = 0;
    long long v = strtoll(buf, &end, 10);
    if (*end != 0 || errno == ERANGE) bad_number(s, "Int", line, col);
    return (int64_t)v;
}

double isma_str_to_float(IsmaStr *s, int line, int col) {
    char buf[128], *end;
    if (!str_to_cbuf(s, buf, sizeof buf)) bad_number(s, "Float", line, col);
    double v = strtod(buf, &end);
    if (*end != 0) bad_number(s, "Float", line, col);
    return v;
}

// ---------------------------------------------------------------- printing

void isma_print_int(int64_t v) { printf("%lld\n", (long long)v); }

void isma_print_float(double d) {
    char buf[64];
    format_float(d, buf, sizeof buf);
    puts(buf);
}

void isma_print_bool(int64_t b) { puts(b ? "true" : "false"); }

void isma_print_str(IsmaStr *s) {
    fwrite(s->data, 1, (size_t)s->len, stdout);
    putchar('\n');
}

// ---------------------------------------------------------------- lists
// List headers live in the arena; element buffers are malloc'd so they can grow.

IsmaList *isma_list_new(int64_t elem_size, int64_t cap) {
    IsmaList *l = isma_alloc((int64_t)sizeof(IsmaList));
    l->len = 0;
    l->cap = cap;
    l->data = NULL;
    if (cap > 0) {
        l->data = malloc((size_t)(cap * elem_size));
        if (!l->data) oom();
    }
    return l;
}

void *isma_list_push_slot(IsmaList *l, int64_t elem_size) {
    if (l->len == l->cap) {
        int64_t ncap = l->cap < 4 ? 4 : l->cap * 2;
        char *nd = realloc(l->data, (size_t)(ncap * elem_size));
        if (!nd) oom();
        l->data = nd;
        l->cap = ncap;
    }
    return l->data + (l->len++) * elem_size;
}

void *isma_list_pop_slot(IsmaList *l, int64_t elem_size, int line, int col) {
    if (l->len == 0) isma_die(line, col, "pop from empty list");
    l->len--;
    return l->data + l->len * elem_size;
}

// ---------------------------------------------------------------- entry

static int saved_argc;
static char **saved_argv;

// Command-line arguments after the program name, as List[String].
IsmaList *isma_args(void) {
    IsmaList *l = isma_list_new((int64_t)sizeof(IsmaStr *), saved_argc > 1 ? saved_argc - 1 : 0);
    for (int i = 1; i < saved_argc; i++) {
        IsmaStr **slot = isma_list_push_slot(l, (int64_t)sizeof(IsmaStr *));
        *slot = str_from(saved_argv[i], (int64_t)strlen(saved_argv[i]));
    }
    return l;
}

int main(int argc, char **argv) {
    saved_argc = argc;
    saved_argv = argv;
    isma_main();
    fflush(stdout);
    return 0;
}
