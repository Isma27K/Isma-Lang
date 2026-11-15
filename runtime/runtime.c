// Isma runtime v0.3
// Linked into every Isma program. Kept deliberately small.

#include <errno.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// Every heap object starts with its reference count. String literals are
// static objects with an "immortal" count so large it never reaches zero.
#define RC_IMMORTAL ((int64_t)1 << 62)

typedef struct {
    int64_t rc;
    int64_t len;
    int64_t cap;
    char data[];
} IsmaStr;

typedef struct {
    int64_t rc;
    int64_t len;
    int64_t cap;
    char *data;
} IsmaList;

typedef void (*IsmaDropFn)(void *);

extern void isma_main(void);

// ---------------------------------------------------------------- panics

// Every panic carries the source file (the user's file or a standard
// library module such as <std>/time.isma), line and column.
static void isma_die(const char *file, int line, int col, const char *msg) {
    fflush(stdout);
    fprintf(stderr, "panic at %s:%d:%d: %s\n", file, line, col, msg);
    exit(101);
}

void isma_panic_msg(IsmaStr *msg, const char *file, int line, int col) {
    fflush(stdout);
    fprintf(stderr, "panic at %s:%d:%d: %.*s\n", file, line, col, (int)msg->len, msg->data);
    exit(101);
}

void isma_panic_overflow(const char *file, int line, int col) { isma_die(file, line, col, "integer overflow"); }
void isma_panic_divzero(const char *file, int line, int col) { isma_die(file, line, col, "division by zero"); }
void isma_assert_fail(const char *file, int line, int col) { isma_die(file, line, col, "assertion failed"); }

void isma_panic_bounds(int64_t idx, int64_t len, const char *file, int line, int col) {
    char buf[128];
    snprintf(buf, sizeof buf, "index %lld out of bounds for list of length %lld", (long long)idx, (long long)len);
    isma_die(file, line, col, buf);
}

void isma_exit(int64_t code) {
    fflush(stdout);
    exit((int)code);
}

// ---------------------------------------------------------------- memory
// Objects are freed as soon as their reference count drops to zero.
// Small objects (up to 256 bytes) come from per-size free lists carved out
// of 1 MiB chunks; larger ones use malloc. The compiler always knows an
// object's size, so `isma_free` is told it instead of storing it.
// With -DISMA_SANITIZE every object is a plain malloc (so ASan can see it).

#define ARENA_CHUNK (1 << 20)
#define SMALL_MAX 256
static char *arena_ptr = NULL;
static char *arena_end = NULL;
static void *free_lists[SMALL_MAX / 16 + 1];

static int64_t st_allocs, st_frees, st_live, st_live_bytes, st_peak_bytes;
static int dropping;

static void *oom(void) {
    fputs("fatal: out of memory\n", stderr);
    exit(134);
}

static int64_t round16(int64_t size) { return (size + 15) & ~(int64_t)15; }

void *isma_alloc(int64_t size) {
    size = round16(size);
    st_allocs++;
    st_live++;
    st_live_bytes += size;
    if (st_live_bytes > st_peak_bytes) st_peak_bytes = st_live_bytes;
#ifndef ISMA_SANITIZE
    if (size <= SMALL_MAX) {
        void **list = &free_lists[size >> 4];
        if (*list) {
            void *p = *list;
            *list = *(void **)p;
            return p;
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
#endif
    void *p = malloc((size_t)size);
    return p ? p : oom();
}

void isma_free(void *p, int64_t size) {
    size = round16(size);
    st_frees++;
    st_live--;
    st_live_bytes -= size;
#ifndef ISMA_SANITIZE
    if (size <= SMALL_MAX) {
        void **list = &free_lists[size >> 4];
        *(void **)p = *list;
        *list = p;
        return;
    }
#endif
    free(p);
}

void isma_collect_cycles(void);
static void collect_if_due(void);

// Freeing an object releases its children. To keep deep structures (a
// million-node chain) from overflowing the stack, children that die while
// another object is being freed are queued and freed by the outermost call.
typedef struct {
    void *obj;
    IsmaDropFn drop;
} PendingDrop;
static PendingDrop *pending;
static int64_t n_pending, cap_pending;

void isma_drop_slow(void *obj, IsmaDropFn drop) {
    if (dropping) {
        if (n_pending == cap_pending) {
            cap_pending = cap_pending ? cap_pending * 2 : 64;
            pending = realloc(pending, (size_t)cap_pending * sizeof(PendingDrop));
            if (!pending) oom();
        }
        pending[n_pending++] = (PendingDrop){obj, drop};
        return;
    }
    dropping = 1;
    drop(obj);
    while (n_pending > 0) {
        PendingDrop d = pending[--n_pending];
        d.drop(d.obj);
    }
    dropping = 0;
    collect_if_due();
}

// ---------------------------------------------------------------- cycle collector
// Reference counting can't free a cycle (a node whose children list contains
// the node itself). The compiler knows which types can ever form a cycle
// (`src/cycles.rs`); only values of those types come through here. Their
// reference count word holds the count in bits 3.. and, in bits 0-2, a color
// and a "buffered" flag for synchronous trial deletion (Bacon & Rajan, 2001):
// when such a value's count drops but stays above zero, it may have just
// become garbage held up only by a cycle, so it is remembered as a possible
// root. When enough roots pile up, the collector subtracts the references
// that come from inside the candidate subgraph; whatever ends up at zero is
// unreachable garbage and is freed. Traversals use explicit stacks, so deep
// structures can't overflow the C stack.

typedef struct IsmaTypeInfo IsmaTypeInfo;
typedef void (*IsmaVisitFn)(void *child, const IsmaTypeInfo *ti);
struct IsmaTypeInfo {
    void (*trace)(void *obj, IsmaVisitFn visit); // visit each child of a cyclic type
    IsmaDropFn drop;                             // count hit zero: release children, then isma_cyclic_free
    IsmaDropFn free_white;                       // member of a garbage cycle: release acyclic children, free memory
    IsmaDropFn free_mem;                         // free just the memory
};

#define RC_UNIT 8
#define COLOR_MASK 3
#define BLACK 0  // in use (or freshly freed: count zero)
#define PURPLE 1 // possible root of a garbage cycle
#define GRAY 2   // being trial-deleted
#define WHITE 3  // garbage
#define BUFFERED 4

typedef struct {
    void *obj;
    const IsmaTypeInfo *ti;
} Root;
typedef struct {
    Root *items;
    int64_t n, cap;
} RootStack;

static RootStack roots, work, work2, whites;
static int64_t roots_threshold = 10000;
static int collecting;
static int64_t st_collections, st_cycle_frees, traced;

static inline int64_t *rc_word(void *o) { return (int64_t *)o; }
static inline int color_of(void *o) { return (int)(*rc_word(o) & COLOR_MASK); }
static inline void set_color(void *o, int c) { *rc_word(o) = (*rc_word(o) & ~(int64_t)COLOR_MASK) | c; }
static inline int64_t count_of(void *o) { return *rc_word(o) >> 3; }

static void stack_push(RootStack *st, void *obj, const IsmaTypeInfo *ti) {
    if (st->n == st->cap) {
        st->cap = st->cap ? st->cap * 2 : 256;
        st->items = realloc(st->items, (size_t)st->cap * sizeof(Root));
        if (!st->items) oom();
    }
    st->items[st->n++] = (Root){obj, ti};
}

// Trace callbacks (they can't take a context argument, so they use `visit_to`).
// A child may be null (a nullable field or list element), which is skipped.
static RootStack *visit_to;
static void visit_push(void *t, const IsmaTypeInfo *ti) {
    if (t) stack_push(visit_to, t, ti);
}
static void visit_mark_gray(void *t, const IsmaTypeInfo *ti) {
    if (!t) return;
    *rc_word(t) -= RC_UNIT; // subtract the reference coming from inside the subgraph
    stack_push(visit_to, t, ti);
}
static void visit_scan_black(void *t, const IsmaTypeInfo *ti) {
    if (!t) return;
    *rc_word(t) += RC_UNIT; // restore it: the parent turned out to be alive
    if (color_of(t) != BLACK) {
        set_color(t, BLACK);
        stack_push(visit_to, t, ti);
    }
}

static void mark_gray(Root r) {
    visit_to = &work;
    stack_push(&work, r.obj, r.ti);
    while (work.n > 0) {
        Root x = work.items[--work.n];
        if (color_of(x.obj) != GRAY) {
            set_color(x.obj, GRAY);
            traced++;
            x.ti->trace(x.obj, visit_mark_gray);
        }
    }
}

static void scan_black(Root r) {
    set_color(r.obj, BLACK);
    stack_push(&work2, r.obj, r.ti);
    while (work2.n > 0) {
        Root x = work2.items[--work2.n];
        visit_to = &work2;
        x.ti->trace(x.obj, visit_scan_black);
    }
}

static void scan(Root r) {
    stack_push(&work, r.obj, r.ti);
    while (work.n > 0) {
        Root x = work.items[--work.n];
        if (color_of(x.obj) != GRAY) continue;
        if (count_of(x.obj) > 0) {
            scan_black(x); // referenced from outside the subgraph: alive
        } else {
            set_color(x.obj, WHITE);
            visit_to = &work;
            x.ti->trace(x.obj, visit_push);
        }
    }
}

static void collect_white(Root r) {
    stack_push(&work, r.obj, r.ti);
    while (work.n > 0) {
        Root x = work.items[--work.n];
        if (color_of(x.obj) != WHITE || (*rc_word(x.obj) & BUFFERED)) continue;
        set_color(x.obj, BLACK);
        stack_push(&whites, x.obj, x.ti);
        visit_to = &work;
        x.ti->trace(x.obj, visit_push);
    }
}

void isma_collect_cycles(void) {
    if (collecting || dropping) return;
    collecting = 1;
    st_collections++;
    // Mark: trial-delete the subgraph under every root that is still a candidate.
    int64_t n = 0;
    for (int64_t i = 0; i < roots.n; i++) {
        Root r = roots.items[i];
        if (color_of(r.obj) == PURPLE) {
            mark_gray(r);
            roots.items[n++] = r;
        } else {
            *rc_word(r.obj) &= ~(int64_t)BUFFERED;
            if (color_of(r.obj) == BLACK && count_of(r.obj) == 0) r.ti->free_mem(r.obj); // died while buffered
        }
    }
    // Scan: anything still referenced from outside is alive, and so is all it reaches.
    for (int64_t i = 0; i < n; i++) scan(roots.items[i]);
    // Collect: the rest is garbage.
    whites.n = 0;
    for (int64_t i = 0; i < n; i++) {
        *rc_word(roots.items[i].obj) &= ~(int64_t)BUFFERED;
        collect_white(roots.items[i]);
    }
    roots.n = 0;
    // Free only after the whole traversal, so no freed object is ever traced.
    for (int64_t i = 0; i < whites.n; i++) {
        st_cycle_frees++;
        whites.items[i].ti->free_white(whites.items[i].obj);
    }
    // If it traced a lot of live data for little garbage, wait longer next time.
    if (whites.n * 4 < traced && roots_threshold < ((int64_t)1 << 30)) roots_threshold *= 2;
    traced = 0;
    collecting = 0;
}

// Drop buffer entries that can no longer be garbage-cycle roots (used again
// since, or already dead) without tracing anything. Returns how many remain.
static int64_t compact_roots(void) {
    int64_t n = 0;
    for (int64_t i = 0; i < roots.n; i++) {
        Root r = roots.items[i];
        if (color_of(r.obj) == PURPLE) {
            roots.items[n++] = r;
        } else {
            *rc_word(r.obj) &= ~(int64_t)BUFFERED;
            if (count_of(r.obj) == 0) r.ti->free_mem(r.obj);
        }
    }
    roots.n = n;
    return n;
}

// Once enough candidate roots pile up, first discard the stale ones cheaply;
// trace only if many real candidates remain. (Never in the middle of freeing
// something: `isma_drop_slow` calls this again when it's done.)
static void collect_if_due(void) {
    if (roots.n < roots_threshold || dropping || collecting) return;
    if (compact_roots() >= roots_threshold / 2) isma_collect_cycles();
}

void isma_release_cyclic(void *p, const IsmaTypeInfo *ti) {
    int64_t rc = *rc_word(p) - RC_UNIT;
    if ((rc >> 3) == 0) {
        *rc_word(p) = rc & BUFFERED; // black, count zero (the collector frees it if buffered)
        isma_drop_slow(p, ti->drop);
        return;
    }
    if ((rc & COLOR_MASK) != PURPLE) {
        rc = (rc & ~(int64_t)COLOR_MASK) | PURPLE;
        if (!(rc & BUFFERED)) {
            *rc_word(p) = rc | BUFFERED;
            stack_push(&roots, p, ti);
            collect_if_due();
            return;
        }
    }
    *rc_word(p) = rc;
}

// The end of a cyclic value's drop: free it, unless the root buffer still
// points at it (then the collector frees it).
void isma_cyclic_free(void *p, const IsmaTypeInfo *ti) {
    if (*rc_word(p) & BUFFERED) return;
    ti->free_mem(p);
}

// ---------------------------------------------------------------- strings

static IsmaStr *str_dup(IsmaStr *s) {
    s->rc++;
    return s;
}

// A fresh string with room for at least `cap` bytes (the size class's slack
// becomes extra capacity for in-place appends).
static IsmaStr *str_alloc(int64_t cap) {
    int64_t size = round16((int64_t)sizeof(IsmaStr) + cap);
    IsmaStr *s = isma_alloc(size);
    s->rc = 1;
    s->len = 0;
    s->cap = size - (int64_t)sizeof(IsmaStr);
    return s;
}

static IsmaStr *str_new(int64_t len) {
    IsmaStr *s = str_alloc(len);
    s->len = len;
    return s;
}

static IsmaStr *str_from(const char *data, int64_t len) {
    IsmaStr *s = str_new(len);
    memcpy(s->data, data, (size_t)len);
    return s;
}

// Drop functions take `void *`: the generated code calls them through IsmaDropFn.
void isma_str_free(void *p) {
    IsmaStr *s = p;
    isma_free(s, (int64_t)sizeof(IsmaStr) + s->cap);
}

IsmaStr *isma_str_concat(IsmaStr *a, IsmaStr *b) {
    if (a->len == 0) return str_dup(b);
    if (b->len == 0) return str_dup(a);
    IsmaStr *s = str_new(a->len + b->len);
    memcpy(s->data, a->data, (size_t)a->len);
    memcpy(s->data + a->len, b->data, (size_t)b->len);
    return s;
}

// `a + b` where the caller gives up its reference to `a`. When nobody else
// holds `a`, `b` is appended in place (with doubling capacity), which makes
// `s += x` loops and f-strings linear instead of quadratic.
IsmaStr *isma_str_append_owned(IsmaStr *a, IsmaStr *b) {
    if (b->len == 0) return a;
    int64_t blen = b->len, n = a->len + blen;
    if (a->rc != 1) {
        IsmaStr *s = str_alloc(n + n / 2);
        s->len = n;
        memcpy(s->data, a->data, (size_t)a->len);
        memcpy(s->data + a->len, b->data, (size_t)blen);
        a->rc--; // shared: someone else still holds it, so it can't reach zero
        return s;
    }
    if (n > a->cap) {
        int64_t cap = a->cap * 2 > n ? a->cap * 2 : n;
        IsmaStr *s = str_alloc(cap);
        s->len = a->len;
        memcpy(s->data, a->data, (size_t)a->len);
        if (b == a) b = s;
        isma_str_free(a);
        a = s;
    }
    memcpy(a->data + a->len, b->data, (size_t)blen);
    a->len = n;
    return a;
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

static void bad_number(IsmaStr *s, const char *ty, const char *file, int line, int col) {
    char buf[160];
    snprintf(buf, sizeof buf, "cannot parse \"%.*s\" as %s", s->len > 64 ? 64 : (int)s->len, s->data, ty);
    isma_die(file, line, col, buf);
}

int64_t isma_str_to_int(IsmaStr *s, const char *file, int line, int col) {
    char buf[64], *end;
    if (!str_to_cbuf(s, buf, sizeof buf)) bad_number(s, "Int", file, line, col);
    errno = 0;
    long long v = strtoll(buf, &end, 10);
    if (*end != 0 || errno == ERANGE) bad_number(s, "Int", file, line, col);
    return (int64_t)v;
}

// `to_int_or_null()`: 1 and the value in *out if `s` is a valid Int, else 0 (and *out = 0).
int64_t isma_str_parse_int(IsmaStr *s, int64_t *out) {
    char buf[64], *end;
    *out = 0;
    if (!str_to_cbuf(s, buf, sizeof buf)) return 0;
    errno = 0;
    long long v = strtoll(buf, &end, 10);
    if (*end != 0 || errno == ERANGE) return 0;
    *out = (int64_t)v;
    return 1;
}

int64_t isma_str_parse_float(IsmaStr *s, double *out) {
    char buf[128], *end;
    *out = 0;
    if (!str_to_cbuf(s, buf, sizeof buf)) return 0;
    double v = strtod(buf, &end);
    if (*end != 0) return 0;
    *out = v;
    return 1;
}

double isma_str_to_float(IsmaStr *s, const char *file, int line, int col) {
    char buf[128], *end;
    if (!str_to_cbuf(s, buf, sizeof buf)) bad_number(s, "Float", file, line, col);
    double v = strtod(buf, &end);
    if (*end != 0) bad_number(s, "Float", file, line, col);
    return v;
}

// ---------------------------------------------------------------- string utilities

static int is_char_boundary(IsmaStr *s, int64_t i) {
    return i == 0 || i == s->len || (i > 0 && i < s->len && ((unsigned char)s->data[i] & 0xC0) != 0x80);
}

IsmaStr *isma_str_substring(IsmaStr *s, int64_t start, int64_t end, const char *file, int line, int col) {
    char buf[200];
    if (start < 0 || end < start || end > s->len) {
        snprintf(buf, sizeof buf, "substring(%lld, %lld) out of range for a string of length %lld", (long long)start, (long long)end, (long long)s->len);
        isma_die(file, line, col, buf);
    }
    if (!is_char_boundary(s, start) || !is_char_boundary(s, end)) {
        snprintf(buf, sizeof buf, "substring(%lld, %lld) would split a multi-byte character", (long long)start, (long long)end);
        isma_die(file, line, col, buf);
    }
    if (start == 0 && end == s->len) return str_dup(s);
    return str_from(s->data + start, end - start);
}

static int64_t find_bytes(IsmaStr *h, IsmaStr *n) {
    if (n->len == 0) return 0;
    for (int64_t i = 0; i + n->len <= h->len; i++)
        if (h->data[i] == n->data[0] && memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    return -1;
}

int64_t isma_str_contains(IsmaStr *h, IsmaStr *n) { return find_bytes(h, n) >= 0; }

int64_t isma_str_starts_with(IsmaStr *s, IsmaStr *p) {
    return p->len <= s->len && memcmp(s->data, p->data, (size_t)p->len) == 0;
}

int64_t isma_str_ends_with(IsmaStr *s, IsmaStr *p) {
    return p->len <= s->len && memcmp(s->data + s->len - p->len, p->data, (size_t)p->len) == 0;
}

IsmaList *isma_list_new(int64_t elem_size, int64_t cap);
void *isma_list_push_slot(IsmaList *l, int64_t elem_size);

// Each UTF-8 character as its own string.
IsmaList *isma_str_chars(IsmaStr *s) {
    IsmaList *l = isma_list_new((int64_t)sizeof(IsmaStr *), s->len);
    int64_t i = 0;
    while (i < s->len) {
        unsigned char c = (unsigned char)s->data[i];
        int64_t n = c < 0x80 ? 1 : (c >> 5) == 0x6 ? 2 : (c >> 4) == 0xE ? 3 : (c >> 3) == 0x1E ? 4 : 1;
        if (i + n > s->len) n = s->len - i;
        IsmaStr **slot = isma_list_push_slot(l, (int64_t)sizeof(IsmaStr *));
        *slot = str_from(s->data + i, n);
        i += n;
    }
    return l;
}

// ---------------------------------------------------------------- clocks (for the time module)

int64_t isma_time_realtime_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

int64_t isma_time_monotonic_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

void isma_time_sleep_ns(int64_t ns) {
    fflush(stdout);
    struct timespec req = {(time_t)(ns / 1000000000), (long)(ns % 1000000000)}, rem;
    while (nanosleep(&req, &rem) != 0 && errno == EINTR) req = rem;
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
// A list header is a small object; its element buffer is malloc'd so it can grow.

IsmaList *isma_list_new(int64_t elem_size, int64_t cap) {
    IsmaList *l = isma_alloc((int64_t)sizeof(IsmaList));
    l->rc = 1;
    l->len = 0;
    l->cap = cap;
    l->data = NULL;
    if (cap > 0) {
        l->data = malloc((size_t)(cap * elem_size));
        if (!l->data) oom();
    }
    return l;
}

// Frees the header and buffer (the elements were already released).
void isma_list_free(void *p) {
    IsmaList *l = p;
    free(l->data);
    isma_free(l, (int64_t)sizeof(IsmaList));
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

void *isma_list_pop_slot(IsmaList *l, int64_t elem_size, const char *file, int line, int col) {
    if (l->len == 0) isma_die(file, line, col, "pop from empty list");
    l->len--;
    return l->data + l->len * elem_size;
}

// ---------------------------------------------------------------- list utilities

void isma_list_insert_slot_check(IsmaList *l, int64_t i, const char *file, int line, int col) {
    if (i < 0 || i > l->len) {
        char buf[128];
        snprintf(buf, sizeof buf, "insert index %lld out of bounds for list of length %lld", (long long)i, (long long)l->len);
        isma_die(file, line, col, buf);
    }
}

// Room for one element at index `i` (already checked), moving the rest up.
void *isma_list_insert_slot(IsmaList *l, int64_t i, int64_t elem_size) {
    isma_list_push_slot(l, elem_size);
    char *at = l->data + i * elem_size;
    memmove(at + elem_size, at, (size_t)((l->len - 1 - i) * elem_size));
    return at;
}

// Takes element `i` out (into `out`), moving the rest down.
void isma_list_remove_at(IsmaList *l, int64_t i, int64_t elem_size, void *out, const char *file, int line, int col) {
    if (i < 0 || i >= l->len) {
        char buf[128];
        snprintf(buf, sizeof buf, "index %lld out of bounds for list of length %lld", (long long)i, (long long)l->len);
        isma_die(file, line, col, buf);
    }
    char *at = l->data + i * elem_size;
    memcpy(out, at, (size_t)elem_size);
    memmove(at, at + elem_size, (size_t)((l->len - 1 - i) * elem_size));
    l->len--;
}

void isma_list_reverse(IsmaList *l, int64_t elem_size) {
    char tmp[16];
    for (int64_t i = 0, j = l->len - 1; i < j; i++, j--) {
        char *a = l->data + i * elem_size, *b = l->data + j * elem_size;
        memcpy(tmp, a, (size_t)elem_size);
        memcpy(a, b, (size_t)elem_size);
        memcpy(b, tmp, (size_t)elem_size);
    }
}

static int cmp_int(const void *a, const void *b) {
    int64_t x = *(const int64_t *)a, y = *(const int64_t *)b;
    return (x > y) - (x < y);
}
// NaN sorts last, like Python's sorted() would if it were consistent.
static int cmp_float(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    if (isnan(x) || isnan(y)) return isnan(x) - isnan(y);
    return (x > y) - (x < y);
}
int64_t isma_str_cmp(IsmaStr *a, IsmaStr *b);
static int cmp_str(const void *a, const void *b) { return (int)isma_str_cmp(*(IsmaStr *const *)a, *(IsmaStr *const *)b); }

// Sort in place: kind 0 Int, 1 Float, 2 String. (qsort isn't stable, but
// equal Ints, Floats and Strings are indistinguishable.)
void isma_list_sort(IsmaList *l, int64_t kind) {
    if (l->len < 2) return;
    qsort(l->data, (size_t)l->len, 8, kind == 0 ? cmp_int : kind == 1 ? cmp_float : cmp_str);
}

// ---------------------------------------------------------------- more string utilities

IsmaStr *isma_str_list_join(IsmaList *l, IsmaStr *sep) {
    IsmaStr **parts = (IsmaStr **)l->data;
    if (l->len == 1) return str_dup(parts[0]);
    int64_t n = 0;
    for (int64_t i = 0; i < l->len; i++) n += parts[i]->len + (i > 0 ? sep->len : 0);
    IsmaStr *s = str_new(n);
    char *p = s->data;
    for (int64_t i = 0; i < l->len; i++) {
        if (i > 0) {
            memcpy(p, sep->data, (size_t)sep->len);
            p += sep->len;
        }
        memcpy(p, parts[i]->data, (size_t)parts[i]->len);
        p += parts[i]->len;
    }
    return s;
}

static void list_push_str(IsmaList *l, const char *data, int64_t len) {
    IsmaStr **slot = isma_list_push_slot(l, (int64_t)sizeof(IsmaStr *));
    *slot = str_from(data, len);
}

static int64_t find_from(IsmaStr *h, IsmaStr *n, int64_t from) {
    if (n->len == 0) return from;
    for (int64_t i = from; i + n->len <= h->len; i++)
        if (h->data[i] == n->data[0] && memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    return -1;
}

IsmaList *isma_str_split(IsmaStr *s, IsmaStr *sep, const char *file, int line, int col) {
    if (sep->len == 0) isma_die(file, line, col, "split() with an empty separator; use chars() to split into characters");
    IsmaList *l = isma_list_new((int64_t)sizeof(IsmaStr *), 0);
    int64_t start = 0;
    for (;;) {
        int64_t at = find_from(s, sep, start);
        if (at < 0) break;
        list_push_str(l, s->data + start, at - start);
        start = at + sep->len;
    }
    list_push_str(l, s->data + start, s->len - start);
    return l;
}

static int is_space(unsigned char c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v'; }

// Like Python's split() without arguments: runs of whitespace separate words,
// and there are no empty words.
IsmaList *isma_str_split_whitespace(IsmaStr *s) {
    IsmaList *l = isma_list_new((int64_t)sizeof(IsmaStr *), 0);
    int64_t i = 0;
    while (i < s->len) {
        while (i < s->len && is_space((unsigned char)s->data[i])) i++;
        int64_t start = i;
        while (i < s->len && !is_space((unsigned char)s->data[i])) i++;
        if (i > start) list_push_str(l, s->data + start, i - start);
    }
    return l;
}

// Lines without their "\n" or "\r\n"; a final newline doesn't start another line.
IsmaList *isma_str_lines(IsmaStr *s) {
    IsmaList *l = isma_list_new((int64_t)sizeof(IsmaStr *), 0);
    int64_t start = 0;
    for (int64_t i = 0; i < s->len; i++) {
        if (s->data[i] == '\n') {
            int64_t end = i > start && s->data[i - 1] == '\r' ? i - 1 : i;
            list_push_str(l, s->data + start, end - start);
            start = i + 1;
        }
    }
    if (start < s->len) list_push_str(l, s->data + start, s->len - start);
    return l;
}

static IsmaStr *str_slice(IsmaStr *s, int64_t a, int64_t b) {
    if (a == 0 && b == s->len) return str_dup(s);
    return str_from(s->data + a, b - a);
}

IsmaStr *isma_str_trim(IsmaStr *s, int64_t start, int64_t end) {
    int64_t a = 0, b = s->len;
    if (start)
        while (a < b && is_space((unsigned char)s->data[a])) a++;
    if (end)
        while (b > a && is_space((unsigned char)s->data[b - 1])) b--;
    return str_slice(s, a, b);
}

// ASCII letters only; other characters are left alone.
IsmaStr *isma_str_case(IsmaStr *s, int64_t upper) {
    int64_t i = 0;
    for (; i < s->len; i++) {
        char c = s->data[i];
        if (upper ? (c >= 'a' && c <= 'z') : (c >= 'A' && c <= 'Z')) break;
    }
    if (i == s->len) return str_dup(s);
    IsmaStr *r = str_from(s->data, s->len);
    for (; i < r->len; i++) {
        char c = r->data[i];
        if (upper && c >= 'a' && c <= 'z') r->data[i] = (char)(c - 32);
        if (!upper && c >= 'A' && c <= 'Z') r->data[i] = (char)(c + 32);
    }
    return r;
}

IsmaStr *isma_str_replace(IsmaStr *s, IsmaStr *from, IsmaStr *to, const char *file, int line, int col) {
    if (from->len == 0) isma_die(file, line, col, "replace() with an empty string to find");
    int64_t count = 0;
    for (int64_t at = find_from(s, from, 0); at >= 0; at = find_from(s, from, at + from->len)) count++;
    if (count == 0) return str_dup(s);
    IsmaStr *r = str_new(s->len + count * (to->len - from->len));
    char *p = r->data;
    int64_t start = 0;
    for (int64_t at = find_from(s, from, 0); at >= 0; at = find_from(s, from, start)) {
        memcpy(p, s->data + start, (size_t)(at - start));
        p += at - start;
        memcpy(p, to->data, (size_t)to->len);
        p += to->len;
        start = at + from->len;
    }
    memcpy(p, s->data + start, (size_t)(s->len - start));
    return r;
}

// The string as it's written in Isma code, quotes and escapes included.
IsmaStr *isma_str_repr(IsmaStr *s) {
    int64_t n = 2;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        n += (c == '"' || c == '\\' || c == '\n' || c == '\t' || c == '\r' || c == 0) ? 2 : 1;
    }
    IsmaStr *r = str_new(n);
    char *p = r->data;
    *p++ = '"';
    for (int64_t i = 0; i < s->len; i++) {
        char c = s->data[i];
        const char *esc = c == '"' ? "\\\"" : c == '\\' ? "\\\\" : c == '\n' ? "\\n" : c == '\t' ? "\\t" : c == '\r' ? "\\r" : c == 0 ? "\\0" : NULL;
        if (esc) {
            *p++ = esc[0];
            *p++ = esc[1];
        } else {
            *p++ = c;
        }
    }
    *p++ = '"';
    return r;
}

// Byte index of the first `n` in `h`, or -1.
int64_t isma_str_find(IsmaStr *h, IsmaStr *n) { return find_from(h, n, 0); }

// Byte index of the last `n` in `h`, or -1.
int64_t isma_str_rfind(IsmaStr *h, IsmaStr *n) {
    for (int64_t i = h->len - n->len; i >= 0; i--)
        if (memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    return -1;
}

// Non-overlapping occurrences (an empty string: one more than the length, like Python).
int64_t isma_str_count(IsmaStr *h, IsmaStr *n) {
    if (n->len == 0) return h->len + 1;
    int64_t c = 0, at = find_from(h, n, 0);
    while (at >= 0) {
        c++;
        at = find_from(h, n, at + n->len);
    }
    return c;
}

IsmaStr *isma_str_repeat(IsmaStr *s, int64_t n, const char *file, int line, int col) {
    if (n < 0) {
        char buf[96];
        snprintf(buf, sizeof buf, "repeat(%lld): the count can't be negative", (long long)n);
        isma_die(file, line, col, buf);
    }
    if (n == 1) return str_dup(s);
    if (s->len > 0 && n > ((int64_t)1 << 40) / s->len) isma_die(file, line, col, "repeat(): the result would be too large");
    IsmaStr *r = str_new(s->len * n);
    for (int64_t i = 0; i < n; i++) memcpy(r->data + i * s->len, s->data, (size_t)s->len);
    return r;
}

static int64_t utf8_count(IsmaStr *s) {
    int64_t n = 0;
    for (int64_t i = 0; i < s->len; i++) n += ((unsigned char)s->data[i] & 0xC0) != 0x80;
    return n;
}

// Pad to `width` characters with copies of `fill` (one character) on the left or right.
IsmaStr *isma_str_pad(IsmaStr *s, int64_t width, IsmaStr *fill, int64_t at_start, const char *file, int line, int col) {
    if (utf8_count(fill) != 1) isma_die(file, line, col, "pad_start()/pad_end(): the fill must be exactly one character");
    int64_t have = utf8_count(s);
    if (have >= width) return str_dup(s);
    int64_t k = width - have;
    IsmaStr *r = str_new(s->len + k * fill->len);
    char *p = r->data;
    if (!at_start) {
        memcpy(p, s->data, (size_t)s->len);
        p += s->len;
    }
    for (int64_t i = 0; i < k; i++, p += fill->len) memcpy(p, fill->data, (size_t)fill->len);
    if (at_start) memcpy(p, s->data, (size_t)s->len);
    return r;
}

// ---------------------------------------------------------------- hashing

// splitmix64's finalizer: every input bit affects every output bit.
static inline uint64_t mix64(uint64_t x) {
    x ^= x >> 30;
    x *= 0xbf58476d1ce4e5b9ULL;
    x ^= x >> 27;
    x *= 0x94d049bb133111ebULL;
    x ^= x >> 31;
    return x;
}

static uint64_t hash_bytes(const char *p, int64_t n) {
    uint64_t h = 0x9E3779B97F4A7C15ULL ^ (uint64_t)n;
    while (n >= 8) {
        uint64_t w;
        memcpy(&w, p, 8);
        h = (h ^ w) * 0xbf58476d1ce4e5b9ULL;
        h ^= h >> 29;
        p += 8;
        n -= 8;
    }
    if (n > 0) {
        uint64_t w = 0;
        memcpy(&w, p, (size_t)n);
        h = (h ^ w) * 0x94d049bb133111ebULL;
    }
    return mix64(h);
}

int64_t isma_hash_int(int64_t v) { return (int64_t)mix64((uint64_t)v); }

int64_t isma_hash_float(double d) {
    if (d == 0) d = 0; // -0.0 == 0.0
    if (isnan(d)) d = NAN;
    uint64_t bits;
    memcpy(&bits, &d, 8);
    return (int64_t)mix64(bits);
}

int64_t isma_hash_str(IsmaStr *s) { return (int64_t)hash_bytes(s->data, s->len); }

// `hash(a, b)`: fold each part's hash into the running one.
int64_t isma_hash_combine(int64_t h, int64_t part) { return (int64_t)mix64((uint64_t)h * 31 + (uint64_t)part); }

// ---------------------------------------------------------------- maps
// A map keeps its entries in an array in insertion order, so looping over
// one is deterministic (like Python's dicts), and finds them through an
// open-addressing index of entry numbers (linear probing, at most 2/3 full).
// An entry is { hash, key, value }; the compiler picks the layout and tells
// the runtime its size. The runtime looks only at hashes and keys: a key is
// an Int-like number (kind 0), a String (kind 1), or an object of a user
// type that compiler-generated functions hash and compare (kind 2). A
// removed entry stays in the array with hash -1 until the next rebuild.
// Keys and values are reference counted by the generated code, not here.

typedef struct {
    int64_t kind;
    int64_t stride;
    int64_t (*hash)(void *key);
    int64_t (*eq)(void *stored, void *key); // `stored` is the key in the map
} IsmaMapType;

typedef struct {
    int64_t rc;
    int64_t len;  // live entries
    int64_t used; // entries[0..used) are live or removed
    int64_t cap;  // entries allocated
    char *entries;
    int64_t *index; // mask + 1 slots: SLOT_EMPTY, SLOT_REMOVED, or a tag (see `slot_tag`)
    int64_t mask;
    // The last lookup, so the update that usually follows it (`m[k] = m.get(k, 0) + 1`,
    // `m[k] += 1`) skips the search: the entry found (or -1), or for a missing
    // Int-like key, the key, its hash and the free slot where it would go.
    int64_t hint_ix;
    int64_t hint_missing; // 1 if the fields below describe a missing key
    int64_t hint_key;
    int64_t hint_hash;
    int64_t hint_slot;
} IsmaMap;

#define SLOT_EMPTY (-1)
#define SLOT_REMOVED (-2)

// A used index slot holds the entry's number and the top bits of its hash, so
// a search can skip other keys without reading their entries (a cache miss each).
static inline int64_t slot_tag(uint64_t h, int64_t ix) { return (int64_t)((h >> 32) << 32 | (uint64_t)ix); }
static inline int64_t slot_ix(int64_t v) { return v & 0xffffffff; }
static inline int slot_matches(int64_t v, uint64_t h) { return ((uint64_t)v >> 32) == (h >> 32); }
#define MAP_MAX_ENTRIES ((int64_t)1 << 30)

static inline int64_t entry_hash(const char *e) { return *(const int64_t *)e; }

static uint64_t key_hash(const IsmaMapType *t, int64_t ikey, void *pkey) {
    uint64_t h;
    switch (t->kind) {
    case 0: h = mix64((uint64_t)ikey); break;
    case 1: h = hash_bytes(((IsmaStr *)pkey)->data, ((IsmaStr *)pkey)->len); break;
    default: h = mix64((uint64_t)t->hash(pkey)); break;
    }
    return h & (uint64_t)INT64_MAX; // live entries have a hash >= 0
}

// Find `key` (with hash `h`): its entry number, or -1 with *slot set to where
// it would go. User `==` may run, and may even change the map; then the
// search starts over.
static int64_t map_lookup(IsmaMap *m, const IsmaMapType *t, uint64_t h, int64_t ikey, void *pkey, int64_t *slot) {
restart:
    if (!m->index) {
        *slot = -1;
        return -1;
    }
    int64_t *index = m->index;
    char *ents = m->entries;
    uint64_t mask = (uint64_t)m->mask;
    uint64_t i = h & mask;
    int64_t tomb = -1;
    for (;;) {
        int64_t v = index[i];
        if (v == SLOT_EMPTY) {
            *slot = tomb >= 0 ? tomb : (int64_t)i;
            return -1;
        }
        if (v == SLOT_REMOVED) {
            if (tomb < 0) tomb = (int64_t)i;
        } else if (slot_matches(v, h)) {
            int64_t ix = slot_ix(v);
            char *e = ents + ix * t->stride;
            if ((uint64_t)entry_hash(e) == h) {
                int eq;
                if (t->kind == 0) {
                    eq = *(int64_t *)(e + 8) == ikey;
                } else if (t->kind == 1) {
                    IsmaStr *a = *(IsmaStr **)(e + 8), *b = pkey;
                    eq = a == b || (a->len == b->len && memcmp(a->data, b->data, (size_t)a->len) == 0);
                } else {
                    eq = t->eq(*(void **)(e + 8), pkey) != 0;
                    if (m->index != index || m->entries != ents) goto restart;
                }
                if (eq) {
                    *slot = (int64_t)i;
                    return ix;
                }
            }
        }
        i = (i + 1) & mask;
    }
}

// Make room for `need` entries: a fresh index and entry array holding only
// the live entries (in order). Hashes are stored, so nothing is rehashed.
static void map_rebuild(IsmaMap *m, const IsmaMapType *t, int64_t need) {
    if (need > MAP_MAX_ENTRIES) {
        fputs("fatal: map too large\n", stderr);
        exit(134);
    }
    int64_t cap = need < 5 ? 5 : need;
    int64_t slots = 8;
    while (slots * 2 / 3 < cap) slots *= 2;
    cap = slots * 2 / 3;
    char *ents = malloc((size_t)(cap * t->stride));
    int64_t *index = malloc((size_t)slots * sizeof(int64_t));
    if (!ents || !index) oom();
    memset(index, 0xff, (size_t)slots * sizeof(int64_t));
    int64_t n = 0;
    uint64_t mask = (uint64_t)slots - 1;
    for (int64_t i = 0; i < m->used; i++) {
        char *e = m->entries + i * t->stride;
        int64_t h = entry_hash(e);
        if (h < 0) continue;
        memcpy(ents + n * t->stride, e, (size_t)t->stride);
        uint64_t j = (uint64_t)h & mask;
        while (index[j] != SLOT_EMPTY) j = (j + 1) & mask;
        index[j] = slot_tag((uint64_t)h, n);
        n++;
    }
    free(m->entries);
    free(m->index);
    m->hint_ix = -1;
    m->hint_missing = 0;
    m->entries = ents;
    m->index = index;
    m->mask = (int64_t)mask;
    m->cap = cap;
    m->used = n;
    m->len = n;
}

IsmaMap *isma_map_new(const IsmaMapType *t, int64_t n) {
    IsmaMap *m = isma_alloc((int64_t)sizeof(IsmaMap));
    m->rc = 1;
    m->len = m->used = m->cap = 0;
    m->entries = NULL;
    m->index = NULL;
    m->mask = 0;
    m->hint_ix = -1;
    m->hint_missing = 0;
    if (n > 0) map_rebuild(m, t, n);
    return m;
}

// Frees the map (its keys and values were already released).
void isma_map_free(void *p) {
    IsmaMap *m = p;
    free(m->entries);
    free(m->index);
    isma_free(m, (int64_t)sizeof(IsmaMap));
}

// `clear()`, after the keys and values were released.
void isma_map_reset(IsmaMap *m) {
    m->len = m->used = 0;
    m->hint_ix = -1;
    m->hint_missing = 0;
    if (m->index) memset(m->index, 0xff, (size_t)(m->mask + 1) * sizeof(int64_t));
}

// Is the live entry `e` the one for this key? (User `==` may run.)
static int entry_is(IsmaMap *m, const IsmaMapType *t, char *e, int64_t ikey, void *pkey) {
    if (entry_hash(e) < 0) return 0;
    if (t->kind == 0) return *(int64_t *)(e + 8) == ikey;
    if (t->kind == 1) {
        IsmaStr *a = *(IsmaStr **)(e + 8), *b = pkey;
        return a == b || (a->len == b->len && memcmp(a->data, b->data, (size_t)a->len) == 0);
    }
    char *ents = m->entries;
    int r = t->eq(*(void **)(e + 8), pkey) != 0;
    return r && m->entries == ents && entry_hash(e) >= 0;
}

static void *map_find(IsmaMap *m, const IsmaMapType *t, int64_t ikey, void *pkey) {
    if (m->len == 0) return NULL;
    uint64_t h = key_hash(t, ikey, pkey);
    int64_t slot, ix = map_lookup(m, t, h, ikey, pkey, &slot);
    m->hint_ix = ix;
    m->hint_missing = ix < 0 && t->kind == 0 && slot >= 0;
    if (m->hint_missing) {
        m->hint_key = ikey;
        m->hint_hash = (int64_t)h;
        m->hint_slot = slot;
    }
    return ix < 0 ? NULL : m->entries + ix * t->stride;
}

void *isma_map_find_i(IsmaMap *m, const IsmaMapType *t, int64_t key) { return map_find(m, t, key, NULL); }
void *isma_map_find_p(IsmaMap *m, const IsmaMapType *t, void *key) { return map_find(m, t, 0, key); }

// The entry for `key`, adding it (with the key stored, the value not yet)
// when it's new: then *is_new is 1.
static void *map_upsert(IsmaMap *m, const IsmaMapType *t, int64_t ikey, void *pkey, int64_t *is_new) {
    // Right after a lookup of the same key, nothing needs to be searched.
    int64_t hint = m->hint_ix;
    if (hint >= 0 && hint < m->used) {
        char *e = m->entries + hint * t->stride;
        if (entry_is(m, t, e, ikey, pkey)) {
            *is_new = 0;
            return e;
        }
    }
    uint64_t h;
    int64_t slot, ix;
    if (m->hint_missing && m->hint_key == ikey && m->used < m->cap) {
        h = (uint64_t)m->hint_hash;
        slot = m->hint_slot;
        ix = -1;
    } else {
        h = key_hash(t, ikey, pkey); // first: it may run user code
        ix = map_lookup(m, t, h, ikey, pkey, &slot);
    }
    m->hint_missing = 0;
    if (ix >= 0) {
        *is_new = 0;
        return m->entries + ix * t->stride;
    }
    if (m->used == m->cap) {
        map_rebuild(m, t, m->len * 2);
        uint64_t mask = (uint64_t)m->mask, j = h & mask;
        while (m->index[j] != SLOT_EMPTY) j = (j + 1) & mask;
        slot = (int64_t)j;
    }
    int64_t n = m->used++;
    char *e = m->entries + n * t->stride;
    *(int64_t *)e = (int64_t)h;
    if (t->kind == 0)
        *(int64_t *)(e + 8) = ikey;
    else
        *(void **)(e + 8) = pkey;
    m->index[slot] = slot_tag(h, n);
    m->len++;
    m->hint_ix = n;
    *is_new = 1;
    return e;
}

void *isma_map_upsert_i(IsmaMap *m, const IsmaMapType *t, int64_t key, int64_t *is_new) { return map_upsert(m, t, key, NULL, is_new); }
void *isma_map_upsert_p(IsmaMap *m, const IsmaMapType *t, void *key, int64_t *is_new) { return map_upsert(m, t, 0, key, is_new); }

// Unlink `key`'s entry and return it (its key and value are the caller's now), or NULL.
static void *map_remove(IsmaMap *m, const IsmaMapType *t, int64_t ikey, void *pkey) {
    if (m->len == 0) return NULL;
    int64_t slot, ix = map_lookup(m, t, key_hash(t, ikey, pkey), ikey, pkey, &slot);
    if (ix < 0) return NULL;
    m->index[slot] = SLOT_REMOVED;
    char *e = m->entries + ix * t->stride;
    *(int64_t *)e = -1;
    m->len--;
    m->hint_ix = -1;
    m->hint_missing = 0;
    return e;
}

void *isma_map_remove_i(IsmaMap *m, const IsmaMapType *t, int64_t key) { return map_remove(m, t, key, NULL); }
void *isma_map_remove_p(IsmaMap *m, const IsmaMapType *t, void *key) { return map_remove(m, t, 0, key); }

// The number of the first live entry at or after `i`, or -1. `len` is the
// size the map had when the loop started.
int64_t isma_map_next(IsmaMap *m, const IsmaMapType *t, int64_t i, int64_t len, const char *file, int line, int col) {
    if (m->len != len) isma_die(file, line, col, "map changed size during iteration");
    for (; i < m->used; i++)
        if (entry_hash(m->entries + i * t->stride) >= 0) return i;
    return -1;
}

// `m[k]` for a missing key. `shown` is the key as text (or NULL), quoted if a String.
void isma_panic_key(IsmaStr *shown, int64_t quoted, const char *file, int line, int col) {
    char buf[200];
    if (!shown)
        snprintf(buf, sizeof buf, "key not found in map");
    else if (quoted)
        snprintf(buf, sizeof buf, "key \"%.*s\" not found in map", shown->len > 64 ? 64 : (int)shown->len, shown->data);
    else
        snprintf(buf, sizeof buf, "key %.*s not found in map", shown->len > 64 ? 64 : (int)shown->len, shown->data);
    isma_die(file, line, col, buf);
}

// ---------------------------------------------------------------- files (for the fs module)

static char fs_error[512];

static void fs_fail(const char *what, IsmaStr *path) {
    snprintf(fs_error, sizeof fs_error, "%s %.*s: %s", what, path->len > 300 ? 300 : (int)path->len, path->data, strerror(errno));
}

// A NUL-terminated copy of an Isma string (paths can't contain NUL).
static char *cpath(IsmaStr *s) {
    char *p = malloc((size_t)s->len + 1);
    if (!p) oom();
    memcpy(p, s->data, (size_t)s->len);
    p[s->len] = 0;
    return p;
}

static IsmaStr *read_all(FILE *f) {
    IsmaStr *s = str_alloc(4096 - (int64_t)sizeof(IsmaStr));
    for (;;) {
        if (s->len == s->cap) {
            IsmaStr *g = str_alloc(s->cap * 2);
            g->len = s->len;
            memcpy(g->data, s->data, (size_t)s->len);
            isma_str_free(s);
            s = g;
        }
        size_t n = fread(s->data + s->len, 1, (size_t)(s->cap - s->len), f);
        if (n == 0) break;
        s->len += (int64_t)n;
    }
    return s;
}

IsmaStr *isma_fs_read(IsmaStr *path) {
    char *p = cpath(path);
    FILE *f = fopen(p, "rb");
    free(p);
    if (!f) {
        fs_fail("cannot read", path);
        return NULL;
    }
    IsmaStr *s = read_all(f);
    int bad = ferror(f);
    fclose(f);
    if (bad) {
        fs_fail("cannot read", path);
        isma_str_free(s);
        return NULL;
    }
    return s;
}

int64_t isma_fs_write(IsmaStr *path, IsmaStr *text, int64_t append) {
    char *p = cpath(path);
    FILE *f = fopen(p, append ? "ab" : "wb");
    free(p);
    if (!f) {
        fs_fail("cannot write", path);
        return 0;
    }
    size_t n = fwrite(text->data, 1, (size_t)text->len, f);
    int bad = n != (size_t)text->len || ferror(f);
    if (fclose(f) != 0) bad = 1;
    if (bad) fs_fail("cannot write", path);
    return !bad;
}

#include <sys/stat.h>
#include <dirent.h>
#include <unistd.h>

// 0: doesn't exist, 1: a file, 2: a directory, 3: something else.
int64_t isma_fs_kind(IsmaStr *path) {
    char *p = cpath(path);
    struct stat st;
    int r = stat(p, &st);
    free(p);
    if (r != 0) return 0;
    return S_ISREG(st.st_mode) ? 1 : S_ISDIR(st.st_mode) ? 2 : 3;
}

static int cmp_names(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

// The names in a directory (without "." and ".."), sorted, or NULL.
IsmaList *isma_fs_list_dir(IsmaStr *path) {
    char *p = cpath(path);
    DIR *d = opendir(p);
    free(p);
    if (!d) {
        fs_fail("cannot list", path);
        return NULL;
    }
    char **names = NULL;
    int64_t n = 0, cap = 0;
    struct dirent *ent;
    while ((ent = readdir(d)) != NULL) {
        if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0) continue;
        if (n == cap) {
            cap = cap ? cap * 2 : 16;
            names = realloc(names, (size_t)cap * sizeof(char *));
            if (!names) oom();
        }
        names[n++] = strdup(ent->d_name);
    }
    closedir(d);
    qsort(names, (size_t)n, sizeof(char *), cmp_names);
    IsmaList *l = isma_list_new((int64_t)sizeof(IsmaStr *), n);
    for (int64_t i = 0; i < n; i++) {
        list_push_str(l, names[i], (int64_t)strlen(names[i]));
        free(names[i]);
    }
    free(names);
    return l;
}

// Like `mkdir -p`: creates missing parents; an existing directory is fine.
int64_t isma_fs_make_dir(IsmaStr *path) {
    char *p = cpath(path);
    for (char *q = p + 1; *q; q++) {
        if (*q != '/') continue;
        *q = 0;
        if (mkdir(p, 0777) != 0 && errno != EEXIST) {
            *q = '/';
            free(p);
            fs_fail("cannot create directory", path);
            return 0;
        }
        *q = '/';
    }
    int ok = mkdir(p, 0777) == 0 || (errno == EEXIST && isma_fs_kind(path) == 2);
    free(p);
    if (!ok) fs_fail("cannot create directory", path);
    return ok;
}

// A file, or an empty directory.
int64_t isma_fs_remove(IsmaStr *path) {
    char *p = cpath(path);
    int ok = remove(p) == 0;
    free(p);
    if (!ok) fs_fail("cannot remove", path);
    return ok;
}

int64_t isma_fs_rename(IsmaStr *from, IsmaStr *to) {
    char *a = cpath(from), *b = cpath(to);
    int ok = rename(a, b) == 0;
    free(a);
    free(b);
    if (!ok) fs_fail("cannot rename", from);
    return ok;
}

IsmaStr *isma_fs_last_error(void) { return str_from(fs_error, (int64_t)strlen(fs_error)); }

IsmaStr *isma_fs_cwd(void) {
    char buf[4096];
    if (!getcwd(buf, sizeof buf)) return str_new(0);
    return str_from(buf, (int64_t)strlen(buf));
}

IsmaStr *isma_fs_read_stdin(void) {
    fflush(stdout);
    return read_all(stdin);
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

// `ISMA_ARC_STATS=1` (set by `isma run --arc-stats`) prints memory counters at
// exit; `ISMA_ARC_CHECK=1` reports leaked objects (used by the test suite).
static int env_flag(const char *name) {
    const char *v = getenv(name);
    return v && *v && *v != '0';
}

static void arc_report(void) {
    int check = env_flag("ISMA_ARC_CHECK");
    if (check) isma_collect_cycles(); // garbage cycles still waiting for a collection
    if (env_flag("ISMA_ARC_STATS")) {
        fprintf(stderr, "isma arc: %lld allocations, %lld frees, %lld live, peak %lld bytes, %lld cycle collections freed %lld\n",
                (long long)st_allocs, (long long)st_frees, (long long)st_live, (long long)st_peak_bytes,
                (long long)st_collections, (long long)st_cycle_frees);
    }
    if (check && st_live != 0) {
        fprintf(stderr, "isma: %lld objects leaked (%lld bytes)\n", (long long)st_live, (long long)st_live_bytes);
    }
}

int main(int argc, char **argv) {
    saved_argc = argc;
    saved_argv = argv;
    isma_main();
    fflush(stdout);
    arc_report();
    return 0;
}
