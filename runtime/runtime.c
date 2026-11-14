// Isma runtime v0.2
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
