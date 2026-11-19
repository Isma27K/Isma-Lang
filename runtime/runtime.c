// Tarn runtime v0.3
// Linked into every Tarn program. Kept deliberately small.

#if defined(__linux__) && !defined(_GNU_SOURCE)
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <stdatomic.h>
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
} TarnStr;

typedef struct {
    int64_t rc;
    int64_t len;
    int64_t cap;
    char *data;
} TarnList;

typedef void (*TarnDropFn)(void *);

extern void tarn_main(void);

// ---------------------------------------------------------------- panics

// Every panic carries the source file (the user's file or a standard
// library module such as <std>/time.tr), line and column. In `main` it ends
// the program; in another task, just that task (see "panics in tasks").
static void panic_task(const char *file, int line, int col, const char *msg, int64_t len);
static void tarn_die(const char *file, int line, int col, const char *msg) { panic_task(file, line, col, msg, (int64_t)strlen(msg)); }

// A `fn drop(self)` stored a reference to the value it was dropping, which
// is about to be freed anyway.
void tarn_drop_kept(void) {
    fflush(stdout);
    fprintf(stderr, "panic: a `fn drop` kept a reference to the value it was dropping (drop runs as the value is freed: it can't keep `self`)\n");
    exit(101);
}

void tarn_panic_msg(TarnStr *msg, const char *file, int line, int col) { panic_task(file, line, col, msg->data, msg->len); }

void tarn_panic_overflow(const char *file, int line, int col) { tarn_die(file, line, col, "integer overflow"); }
void tarn_panic_divzero(const char *file, int line, int col) { tarn_die(file, line, col, "division by zero"); }
void tarn_assert_fail(const char *file, int line, int col) { tarn_die(file, line, col, "assertion failed"); }

void tarn_panic_bounds(int64_t idx, int64_t len, const char *file, int line, int col) {
    char buf[128];
    snprintf(buf, sizeof buf, "index %lld out of bounds for list of length %lld", (long long)idx, (long long)len);
    tarn_die(file, line, col, buf);
}

void tarn_exit(int64_t code) {
    fflush(stdout);
    exit((int)code);
}

// ---------------------------------------------------------------- memory
// Objects are freed as soon as their reference count drops to zero.
// Small objects (up to 256 bytes) come from per-size free lists carved out
// of 1 MiB chunks; larger ones use malloc. The compiler always knows an
// object's size, so `tarn_free` is told it instead of storing it.
// With -DTARN_SANITIZE every object is a plain malloc (so ASan can see it).
//
// Tasks run on several threads, so the free lists (and the counters) are
// per thread. A block freed on another thread than the one that allocated
// it just joins that thread's lists: blocks have no home thread.

#define ARENA_CHUNK (1 << 20)
#define SMALL_MAX 256

typedef struct ThreadState {
    char *arena_ptr, *arena_end;
    void *free_lists[SMALL_MAX / 16 + 1];
    int64_t allocs, frees, live, live_bytes, peak_bytes, collections, cycle_frees;
    struct ThreadState *next; // every thread's state, for the totals at exit
} ThreadState;

static ThreadState *all_thread_states;
static pthread_mutex_t thread_states_lock = PTHREAD_MUTEX_INITIALIZER;
static __thread ThreadState *tl_thread;

static ThreadState *new_thread_state(void) {
    ThreadState *t = calloc(1, sizeof *t);
    if (!t) {
        fputs("fatal: out of memory\n", stderr);
        exit(134);
    }
    pthread_mutex_lock(&thread_states_lock);
    t->next = all_thread_states;
    all_thread_states = t;
    pthread_mutex_unlock(&thread_states_lock);
    return t;
}

static inline ThreadState *thread_state(void) {
    ThreadState *t = tl_thread;
    if (__builtin_expect(!t, 0)) t = tl_thread = new_thread_state();
    return t;
}

static void *oom(void) {
    fputs("fatal: out of memory\n", stderr);
    exit(134);
}

static int64_t round16(int64_t size) { return (size + 15) & ~(int64_t)15; }

void *tarn_alloc(int64_t size) {
    size = round16(size);
    ThreadState *t = thread_state();
    t->allocs++;
    t->live++;
    t->live_bytes += size;
    if (t->live_bytes > t->peak_bytes) t->peak_bytes = t->live_bytes;
#ifndef TARN_SANITIZE
    if (size <= SMALL_MAX) {
        void **list = &t->free_lists[size >> 4];
        if (*list) {
            void *p = *list;
            *list = *(void **)p;
            return p;
        }
        if (t->arena_end - t->arena_ptr < size) {
            t->arena_ptr = malloc(ARENA_CHUNK);
            if (!t->arena_ptr) oom();
            t->arena_end = t->arena_ptr + ARENA_CHUNK;
        }
        void *p = t->arena_ptr;
        t->arena_ptr += size;
        return p;
    }
#endif
    void *p = malloc((size_t)size);
    return p ? p : oom();
}

void tarn_free(void *p, int64_t size) {
    size = round16(size);
    ThreadState *t = thread_state();
    t->frees++;
    t->live--;
    t->live_bytes -= size;
#ifndef TARN_SANITIZE
    if (size <= SMALL_MAX) {
        void **list = &t->free_lists[size >> 4];
        *(void **)p = *list;
        *list = p;
        return;
    }
#endif
    free(p);
}

// ---------------------------------------------------------------- per-task state
// Freeing (with its queue of pending drops) and the cycle collector's root
// buffer belong to the task doing them, not to the thread: a `fn drop` may
// wait (send on a channel, take a lock), and the task may then go on on
// another thread while this thread runs other tasks.

typedef struct TarnTypeInfo TarnTypeInfo;
typedef void (*TarnVisitFn)(void *child, const TarnTypeInfo *ti);
struct TarnTypeInfo {
    void (*trace)(void *obj, TarnVisitFn visit); // visit each child of a cyclic type
    TarnDropFn drop;                             // count hit zero: release children, then tarn_cyclic_free
    TarnDropFn free_white;                       // member of a garbage cycle: release acyclic children, free memory
    TarnDropFn free_mem;                         // free just the memory
};

typedef struct {
    void *obj;
    const TarnTypeInfo *ti;
} Root;
typedef struct {
    Root *items;
    int64_t n, cap;
} RootStack;

typedef struct {
    void *obj;
    TarnDropFn drop;
} PendingDrop;

typedef struct TarnCtx {
    // Possible roots of garbage cycles: `roots`, or while a Mutex is held,
    // that Mutex's own buffer (its value is reachable from other tasks).
    RootStack roots;
    RootStack *cur_roots;
    RootStack work, work2, whites;
    int64_t roots_threshold, traced;
    int collecting, dropping;
    // Children that died while another object was being freed.
    PendingDrop *pending;
    int64_t n_pending, cap_pending;
} TarnCtx;

static TarnCtx main_ctx;
static __thread TarnCtx *tl_ctx;

static inline TarnCtx *ctx(void) {
    TarnCtx *c = tl_ctx;
    if (__builtin_expect(!c, 0)) c = tl_ctx = &main_ctx;
    if (__builtin_expect(!c->cur_roots, 0)) {
        c->cur_roots = &c->roots;
        c->roots_threshold = 10000;
    }
    return c;
}

static void ctx_free(TarnCtx *c) {
    free(c->roots.items);
    free(c->work.items);
    free(c->work2.items);
    free(c->whites.items);
    free(c->pending);
    memset(c, 0, sizeof *c);
}

void tarn_collect_cycles(void);
static void collect_if_due(TarnCtx *c);

// Freeing an object releases its children. To keep deep structures (a
// million-node chain) from overflowing the stack, children that die while
// another object is being freed are queued and freed by the outermost call.
void tarn_drop_slow(void *obj, TarnDropFn drop) {
    TarnCtx *c = ctx();
    if (c->dropping) {
        if (c->n_pending == c->cap_pending) {
            c->cap_pending = c->cap_pending ? c->cap_pending * 2 : 64;
            c->pending = realloc(c->pending, (size_t)c->cap_pending * sizeof(PendingDrop));
            if (!c->pending) oom();
        }
        c->pending[c->n_pending++] = (PendingDrop){obj, drop};
        return;
    }
    c->dropping = 1;
    drop(obj);
    while (c->n_pending > 0) {
        PendingDrop d = c->pending[--c->n_pending];
        d.drop(d.obj);
    }
    c->dropping = 0;
    collect_if_due(c);
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
// structures can't overflow the C stack. Each task collects its own roots
// (tasks share no objects), and a Mutex collects the roots found while it's held.

#define RC_UNIT 8
#define COLOR_MASK 3
#define BLACK 0  // in use (or freshly freed: count zero)
#define PURPLE 1 // possible root of a garbage cycle
#define GRAY 2   // being trial-deleted
#define WHITE 3  // garbage
#define BUFFERED 4

static inline int64_t *rc_word(void *o) { return (int64_t *)o; }
static inline int color_of(void *o) { return (int)(*rc_word(o) & COLOR_MASK); }
static inline void set_color(void *o, int c) { *rc_word(o) = (*rc_word(o) & ~(int64_t)COLOR_MASK) | c; }
static inline int64_t count_of(void *o) { return *rc_word(o) >> 3; }

static void stack_push(RootStack *st, void *obj, const TarnTypeInfo *ti) {
    if (st->n == st->cap) {
        st->cap = st->cap ? st->cap * 2 : 256;
        st->items = realloc(st->items, (size_t)st->cap * sizeof(Root));
        if (!st->items) oom();
    }
    st->items[st->n++] = (Root){obj, ti};
}

// Trace callbacks (they can't take a context argument, so they use `visit_to`).
// A child may be null (a nullable field or list element), which is skipped.
static __thread RootStack *visit_to;
static void visit_push(void *t, const TarnTypeInfo *ti) {
    if (t) stack_push(visit_to, t, ti);
}
static void visit_mark_gray(void *t, const TarnTypeInfo *ti) {
    if (!t) return;
    *rc_word(t) -= RC_UNIT; // subtract the reference coming from inside the subgraph
    stack_push(visit_to, t, ti);
}
static void visit_scan_black(void *t, const TarnTypeInfo *ti) {
    if (!t) return;
    *rc_word(t) += RC_UNIT; // restore it: the parent turned out to be alive
    if (color_of(t) != BLACK) {
        set_color(t, BLACK);
        stack_push(visit_to, t, ti);
    }
}

static void mark_gray(TarnCtx *c, Root r) {
    stack_push(&c->work, r.obj, r.ti);
    while (c->work.n > 0) {
        Root x = c->work.items[--c->work.n];
        if (color_of(x.obj) != GRAY) {
            set_color(x.obj, GRAY);
            c->traced++;
            visit_to = &c->work;
            x.ti->trace(x.obj, visit_mark_gray);
        }
    }
}

static void scan_black(TarnCtx *c, Root r) {
    set_color(r.obj, BLACK);
    stack_push(&c->work2, r.obj, r.ti);
    while (c->work2.n > 0) {
        Root x = c->work2.items[--c->work2.n];
        visit_to = &c->work2;
        x.ti->trace(x.obj, visit_scan_black);
    }
}

static void scan(TarnCtx *c, Root r) {
    stack_push(&c->work, r.obj, r.ti);
    while (c->work.n > 0) {
        Root x = c->work.items[--c->work.n];
        if (color_of(x.obj) != GRAY) continue;
        if (count_of(x.obj) > 0) {
            scan_black(c, x); // referenced from outside the subgraph: alive
        } else {
            set_color(x.obj, WHITE);
            visit_to = &c->work;
            x.ti->trace(x.obj, visit_push);
        }
    }
}

static void collect_white(TarnCtx *c, Root r) {
    stack_push(&c->work, r.obj, r.ti);
    while (c->work.n > 0) {
        Root x = c->work.items[--c->work.n];
        if (color_of(x.obj) != WHITE || (*rc_word(x.obj) & BUFFERED)) continue;
        set_color(x.obj, BLACK);
        stack_push(&c->whites, x.obj, x.ti);
        visit_to = &c->work;
        x.ti->trace(x.obj, visit_push);
    }
}

// Trial-delete the roots in `roots` (the task's own, or a Mutex's).
static void collect_roots(TarnCtx *c, RootStack *roots) {
    if (c->collecting || c->dropping) return;
    c->collecting = 1;
    ThreadState *t = thread_state();
    t->collections++;
    // Mark: trial-delete the subgraph under every root that is still a candidate.
    int64_t n = 0;
    for (int64_t i = 0; i < roots->n; i++) {
        Root r = roots->items[i];
        if (color_of(r.obj) == PURPLE) {
            mark_gray(c, r);
            roots->items[n++] = r;
        } else {
            *rc_word(r.obj) &= ~(int64_t)BUFFERED;
            if (color_of(r.obj) == BLACK && count_of(r.obj) == 0) r.ti->free_mem(r.obj); // died while buffered
        }
    }
    // Scan: anything still referenced from outside is alive, and so is all it reaches.
    for (int64_t i = 0; i < n; i++) scan(c, roots->items[i]);
    // Collect: the rest is garbage.
    c->whites.n = 0;
    for (int64_t i = 0; i < n; i++) {
        *rc_word(roots->items[i].obj) &= ~(int64_t)BUFFERED;
        collect_white(c, roots->items[i]);
    }
    roots->n = 0;
    // Free only after the whole traversal, so no freed object is ever traced.
    // (While freeing, `dropping` queues what dies meanwhile and keeps
    // another collection from starting.)
    c->collecting = 0;
    c->dropping = 1;
    int64_t freed = c->whites.n;
    for (int64_t i = 0; i < c->whites.n; i++) {
        t->cycle_frees++;
        c->whites.items[i].ti->free_white(c->whites.items[i].obj);
    }
    c->whites.n = 0;
    while (c->n_pending > 0) {
        PendingDrop d = c->pending[--c->n_pending];
        d.drop(d.obj);
    }
    c->dropping = 0;
    // If it traced a lot of live data for little garbage, wait longer next time.
    if (freed * 4 < c->traced && c->roots_threshold < ((int64_t)1 << 30)) c->roots_threshold *= 2;
    c->traced = 0;
}

void tarn_collect_cycles(void) {
    TarnCtx *c = ctx();
    collect_roots(c, c->cur_roots);
}

// Drop buffer entries that can no longer be garbage-cycle roots (used again
// since, or already dead) without tracing anything. Returns how many remain.
static int64_t compact_roots(RootStack *roots) {
    int64_t n = 0;
    for (int64_t i = 0; i < roots->n; i++) {
        Root r = roots->items[i];
        if (color_of(r.obj) == PURPLE) {
            roots->items[n++] = r;
        } else {
            *rc_word(r.obj) &= ~(int64_t)BUFFERED;
            if (count_of(r.obj) == 0) r.ti->free_mem(r.obj);
        }
    }
    roots->n = n;
    return n;
}

// Once enough candidate roots pile up, first discard the stale ones cheaply;
// trace only if many real candidates remain. (Never in the middle of freeing
// something: `tarn_drop_slow` calls this again when it's done.)
static void collect_if_due(TarnCtx *c) {
    RootStack *roots = c->cur_roots;
    if (roots->n < c->roots_threshold || c->dropping || c->collecting) return;
    if (compact_roots(roots) >= c->roots_threshold / 2) collect_roots(c, roots);
}

void tarn_release_cyclic(void *p, const TarnTypeInfo *ti) {
    int64_t rc = *rc_word(p) - RC_UNIT;
    if ((rc >> 3) == 0) {
        *rc_word(p) = rc & BUFFERED; // black, count zero (the collector frees it if buffered)
        tarn_drop_slow(p, ti->drop);
        return;
    }
    if ((rc & COLOR_MASK) != PURPLE) {
        rc = (rc & ~(int64_t)COLOR_MASK) | PURPLE;
        if (!(rc & BUFFERED)) {
            *rc_word(p) = rc | BUFFERED;
            TarnCtx *c = ctx();
            stack_push(c->cur_roots, p, ti);
            collect_if_due(c);
            return;
        }
    }
    *rc_word(p) = rc;
}

// The end of a cyclic value's drop: free it, unless the root buffer still
// points at it (then the collector frees it).
void tarn_cyclic_free(void *p, const TarnTypeInfo *ti) {
    if (*rc_word(p) & BUFFERED) return;
    ti->free_mem(p);
}

// ---------------------------------------------------------------- tasks
// Lightweight tasks. Each task runs on a stack of its own, and the scheduler
// only switches between tasks where one waits: for I/O, a timer, a channel,
// a lock, or another task. So waiting code reads like ordinary blocking code,
// with no async/await. Tasks run on a pool of threads, one per core (or
// $TARN_THREADS); a thread with nothing to do steals tasks from the others,
// or waits for the operating system's readiness events (epoll on Linux,
// kqueue on macOS) and the timers, on behalf of every thread.
//
// Nothing of this exists until the first `spawn`: until then a program is a
// single thread running `main` on its own stack, exactly as before.
//
// Tasks share no objects (values given to another task are moved or copied:
// see `tarn.xfer.*` in the generated code), so reference counts stay plain
// integers. The only objects several tasks can reach are the handles below
// (Task, Channel, Mutex, Atomic), whose counts are atomic.

#include <sys/mman.h>
#include <unistd.h>
#include <poll.h>
#include <signal.h>
#include <unwind.h>
#ifdef __linux__
#include <sys/epoll.h>
#include <sys/eventfd.h>
#else
#include <sys/event.h>
#endif
#if defined(TARN_SANITIZE)
#include <sanitizer/asan_interface.h>
#endif

typedef struct Task Task;
static TarnStr *str_dup(TarnStr *s);
void tarn_str_free(void *p);

// The kinds of handle objects (word 1 of each, after the count).
enum { HANDLE_TASK = 1, HANDLE_CHANNEL, HANDLE_MUTEX, HANDLE_ATOMIC };

struct Task {
    _Atomic int64_t rc; // the count of `Task` handles (plus one while it runs)
    int64_t kind;
    int64_t id;
    void *sp;             // saved stack pointer, while switched out
    char *stack;          // the mapping: a guard page, then the stack
    size_t stack_size;
    TarnCtx *ctxp;        // its freeing and cycle-collection state (`ctx` below, or main's)
    TarnCtx ctx;
    // What it runs: `run(closure, result)` calls the function value.
    void *closure;
    void (*run)(void *closure, void *out);
    void (*release_closure)(void *);
    void (*release_result)(void *); // for a result that holds a reference
    // Run by the scheduler right after the task has switched away (unlock
    // what the task waits on, register it with the poller...).
    void (*after)(void *);
    void *after_arg;
    Task *next; // in a run queue, or one wait list
    int pinned; // only worker 0 runs it (main: it runs on the process's own stack)
    // A wait that a timer, the poller or a signal ends: even = waiting
    // (wait number * 2), odd = ended. Whoever ends it first wakes the task.
    _Atomic int64_t wait_state;
    int64_t wait_count;
    // Result (guarded by `lock`).
    pthread_mutex_t lock;
    int done, panicked, taken;
    Task *joiners;
    TarnStr *panic_message, *panic_location;
    int64_t result[2];
    // Why the last failed `net` / `fs` call failed.
    int64_t net_error;
    char fs_error[512];
    struct _Unwind_Exception unwind;
#if defined(TARN_SANITIZE)
    void *asan_fake;
#endif
};

static Task main_task = {.rc = 1, .kind = HANDLE_TASK, .pinned = 1};
static __thread Task *tl_task;
static int sched_on; // set by the first spawn

// The current task. Never inlined: a task can stop on one thread and go on
// on another, so a thread-local read must not be reused across a switch.
static __attribute__((noinline)) Task *cur_task(void) {
    Task *t = tl_task;
    __asm__ volatile("" ::: "memory");
    return t ? t : &main_task;
}

// ---------------- switching stacks
// `ctx_switch(&save, to)` saves the callee-saved registers on the current
// stack, stores the stack pointer in `save`, and resumes the stack `to`
// (which was saved the same way, or prepared by `init_stack`).

#if defined(__x86_64__)
__attribute__((naked, noinline)) static void ctx_switch(void **save, void *to) {
    __asm__ volatile(
        "pushq %rbp\n\t"
        "pushq %rbx\n\t"
        "pushq %r12\n\t"
        "pushq %r13\n\t"
        "pushq %r14\n\t"
        "pushq %r15\n\t"
        "subq $8, %rsp\n\t"
        "stmxcsr (%rsp)\n\t"
        "fnstcw 4(%rsp)\n\t"
        "movq %rsp, (%rdi)\n\t"
        "movq %rsi, %rsp\n\t"
        "ldmxcsr (%rsp)\n\t"
        "fldcw 4(%rsp)\n\t"
        "addq $8, %rsp\n\t"
        "popq %r15\n\t"
        "popq %r14\n\t"
        "popq %r13\n\t"
        "popq %r12\n\t"
        "popq %rbx\n\t"
        "popq %rbp\n\t"
        "retq\n\t");
}

// A fresh stack whose first switch "returns" into `entry`.
static void *init_stack(char *top, void (*entry)(void)) {
    uint64_t *sp = (uint64_t *)((uintptr_t)top & ~(uintptr_t)15);
    *--sp = 0;                   // `entry`'s return address: nothing
    *--sp = (uint64_t)entry;     // where the switch's `ret` goes
    for (int i = 0; i < 6; i++) *--sp = 0; // rbp rbx r12 r13 r14 r15
    *--sp = 0x037F00001F80ull;   // mxcsr (low half) and the x87 control word (bytes 4-5)
    return sp;
}
#elif defined(__aarch64__)
__attribute__((naked, noinline)) static void ctx_switch(void **save, void *to) {
    __asm__ volatile(
        "sub sp, sp, #176\n\t"
        "stp x19, x20, [sp, #0]\n\t"
        "stp x21, x22, [sp, #16]\n\t"
        "stp x23, x24, [sp, #32]\n\t"
        "stp x25, x26, [sp, #48]\n\t"
        "stp x27, x28, [sp, #64]\n\t"
        "stp x29, x30, [sp, #80]\n\t"
        "stp d8, d9, [sp, #96]\n\t"
        "stp d10, d11, [sp, #112]\n\t"
        "stp d12, d13, [sp, #128]\n\t"
        "stp d14, d15, [sp, #144]\n\t"
        "mov x9, sp\n\t"
        "str x9, [x0]\n\t"
        "mov sp, x1\n\t"
        "ldp x19, x20, [sp, #0]\n\t"
        "ldp x21, x22, [sp, #16]\n\t"
        "ldp x23, x24, [sp, #32]\n\t"
        "ldp x25, x26, [sp, #48]\n\t"
        "ldp x27, x28, [sp, #64]\n\t"
        "ldp x29, x30, [sp, #80]\n\t"
        "ldp d8, d9, [sp, #96]\n\t"
        "ldp d10, d11, [sp, #112]\n\t"
        "ldp d12, d13, [sp, #128]\n\t"
        "ldp d14, d15, [sp, #144]\n\t"
        "add sp, sp, #176\n\t"
        "ret\n\t");
}

static void *init_stack(char *top, void (*entry)(void)) {
    uint64_t *sp = (uint64_t *)((uintptr_t)top & ~(uintptr_t)15);
    sp -= 22; // 176 bytes
    memset(sp, 0, 176);
    sp[11] = (uint64_t)entry; // x30: where the switch's `ret` goes
    return sp;
}
#else
#error "tasks need x86-64 or arm64"
#endif

// ---------------- stacks
// Each task gets 1 MiB (or $TARN_TASK_STACK bytes) of address space with a
// guard page below it; memory is only used as the stack grows into it.
// Finished tasks' stacks are kept for the next ones.

static size_t task_stack_size;
static pthread_mutex_t stacks_lock = PTHREAD_MUTEX_INITIALIZER;
static char *free_stacks[64];
static int n_free_stacks;

static char *stack_alloc(void) {
    pthread_mutex_lock(&stacks_lock);
    char *s = n_free_stacks > 0 ? free_stacks[--n_free_stacks] : NULL;
    pthread_mutex_unlock(&stacks_lock);
    if (s) return s;
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    int flags = MAP_PRIVATE | MAP_ANONYMOUS;
#ifdef MAP_NORESERVE
    flags |= MAP_NORESERVE;
#endif
    s = mmap(NULL, task_stack_size, PROT_READ | PROT_WRITE, flags, -1, 0);
    if (s == MAP_FAILED) oom();
    mprotect(s, page, PROT_NONE);
    return s;
}

static void stack_free(char *s) {
    pthread_mutex_lock(&stacks_lock);
    if (n_free_stacks < 64) {
        free_stacks[n_free_stacks++] = s;
        s = NULL;
    }
    pthread_mutex_unlock(&stacks_lock);
    if (s) munmap(s, task_stack_size);
}

// ---------------- workers and run queues

typedef struct Worker {
    int id;
    void *sched_sp;          // the scheduler loop's stack, while a task runs
    Task *current;
    pthread_mutex_t lock;    // guards the queues
    Task *head, *tail;       // runnable tasks (others may steal them)
    Task *pinned_head, *pinned_tail; // (worker 0) runnable tasks only it may run
    pthread_cond_t wake;
    int sleeping;            // (guarded by idle_lock)
#if defined(TARN_SANITIZE)
    void *stack_bottom;
    size_t stack_size;
#endif
} Worker;

static Worker *workers;
static int n_workers;
static __thread Worker *tl_worker;

static __attribute__((noinline)) Worker *cur_worker(void) {
    Worker *w = tl_worker;
    __asm__ volatile("" ::: "memory");
    return w;
}

static Task *gq_head, *gq_tail; // runnable tasks from outside the workers (the poller, timers)
static pthread_mutex_t gq_lock = PTHREAD_MUTEX_INITIALIZER;
static _Atomic int64_t n_queued;        // runnable tasks in any queue
static pthread_mutex_t idle_lock = PTHREAD_MUTEX_INITIALIZER;
static int n_sleeping;                  // workers asleep (guarded by idle_lock)
static _Atomic int poller_waiting;      // a worker is blocked in the poller
static _Atomic int64_t live_tasks;      // spawned and not finished
static _Atomic int64_t next_task_id;

static void wake_poller(void);

// Wake a sleeping worker, preferably `prefer`, or the poller.
static void kick(Worker *prefer) {
    pthread_mutex_lock(&idle_lock);
    Worker *w = NULL;
    if (prefer && prefer->sleeping) w = prefer;
    for (int i = 0; !w && !prefer && i < n_workers; i++)
        if (workers[i].sleeping) w = &workers[i];
    if (w) {
        w->sleeping = 0;
        n_sleeping--;
        pthread_cond_signal(&w->wake);
    }
    pthread_mutex_unlock(&idle_lock);
    if (!w) wake_poller();
}

static void push(Task **head, Task **tail, Task *t) {
    t->next = NULL;
    if (*tail) (*tail)->next = t;
    else *head = t;
    *tail = t;
}

static Task *pop(Task **head, Task **tail) {
    Task *t = *head;
    if (t) {
        *head = t->next;
        if (!*head) *tail = NULL;
        t->next = NULL;
    }
    return t;
}

// Make a waiting task runnable. Only call this once the task has switched
// away (its `after` ran): see `park`.
static void ready(Task *t) {
    atomic_fetch_add(&n_queued, 1);
    if (t->pinned) {
        Worker *w0 = &workers[0];
        pthread_mutex_lock(&w0->lock);
        push(&w0->pinned_head, &w0->pinned_tail, t);
        pthread_mutex_unlock(&w0->lock);
        kick(w0);
        return;
    }
    Worker *w = cur_worker();
    if (w) {
        pthread_mutex_lock(&w->lock);
        push(&w->head, &w->tail, t);
        pthread_mutex_unlock(&w->lock);
    } else {
        pthread_mutex_lock(&gq_lock);
        push(&gq_head, &gq_tail, t);
        pthread_mutex_unlock(&gq_lock);
    }
    kick(NULL);
}

static Task *take_from(Worker *w, int pinned_too) {
    if (!w->head && !(pinned_too && w->pinned_head)) return NULL;
    pthread_mutex_lock(&w->lock);
    Task *t = pinned_too ? pop(&w->pinned_head, &w->pinned_tail) : NULL;
    if (!t) t = pop(&w->head, &w->tail);
    pthread_mutex_unlock(&w->lock);
    return t;
}

static void fire_due_timers(void);

static Task *find_task(Worker *w) {
    fire_due_timers();
    Task *t = take_from(w, w->id == 0);
    if (!t && gq_head) {
        pthread_mutex_lock(&gq_lock);
        t = pop(&gq_head, &gq_tail);
        pthread_mutex_unlock(&gq_lock);
    }
    // Steal from the others.
    for (int i = 1; !t && i < n_workers; i++) t = take_from(&workers[(w->id + i) % n_workers], 0);
    if (t) atomic_fetch_sub(&n_queued, 1);
    return t;
}

// ---------------- running and parking

#if defined(TARN_SANITIZE)
static void asan_switch_to(void **fake, void *bottom, size_t size) { __sanitizer_start_switch_fiber(fake, bottom, size); }
static void asan_arrived(void *fake) { __sanitizer_finish_switch_fiber(fake, NULL, NULL); }
#endif

static void run_task(Worker *w, Task *t) {
    w->current = t;
    tl_task = t;
    tl_ctx = t->ctxp;
#if defined(TARN_SANITIZE)
    void *fake = NULL;
    if (t->stack) asan_switch_to(&fake, t->stack, task_stack_size);
    else asan_switch_to(&fake, NULL, 0);
#endif
    ctx_switch(&w->sched_sp, t->sp);
#if defined(TARN_SANITIZE)
    asan_arrived(fake);
#endif
    tl_task = NULL;
    tl_ctx = NULL;
    w->current = NULL;
    void (*after)(void *) = t->after;
    void *arg = t->after_arg;
    t->after = NULL;
    // (From here on another worker may already be running `t`.)
    if (after) after(arg);
}

// Switch away from the current task; the scheduler runs `after(arg)` once
// it's off this stack. A task is woken by `ready`: to make sure nobody can
// before it has switched away, whatever lets others find it (unlocking the
// list it waits in, registering with the poller) happens in `after`.
static void park(void (*after)(void *), void *arg) {
    Task *t = cur_task();
    Worker *w = cur_worker();
    t->after = after;
    t->after_arg = arg;
#if defined(TARN_SANITIZE)
    asan_switch_to(&t->asan_fake, w->stack_bottom, w->stack_size);
#endif
    ctx_switch(&t->sp, w->sched_sp);
#if defined(TARN_SANITIZE)
    asan_arrived(t->asan_fake);
#endif
}

static void unlock_mutex(void *m) { pthread_mutex_unlock((pthread_mutex_t *)m); }

// ---------------- the poller and timers

static int poll_fd = -1, wake_rd = -1, wake_wr = -1;
static pthread_mutex_t poll_lock = PTHREAD_MUTEX_INITIALIZER;
static _Atomic int64_t n_io_waits; // tasks registered with the poller

typedef struct {
    int64_t deadline;
    Task *task;
    int64_t wait; // the task's wait number it belongs to
} Timer;
static Timer *timers;
static int64_t n_timers, cap_timers;
static pthread_mutex_t timers_lock = PTHREAD_MUTEX_INITIALIZER;

static int64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

// End the task's wait number `wait` (-1: whichever it's in), if it's still in it.
static void end_wait(Task *t, int64_t wait) {
    int64_t s = atomic_load(&t->wait_state);
    if (s & 1) return;
    if (wait >= 0 && s != wait * 2) return;
    if (atomic_compare_exchange_strong(&t->wait_state, &s, s + 1)) ready(t);
}

static void timer_add(int64_t deadline, Task *t, int64_t wait) {
    pthread_mutex_lock(&timers_lock);
    if (n_timers == cap_timers) {
        cap_timers = cap_timers ? cap_timers * 2 : 64;
        timers = realloc(timers, (size_t)cap_timers * sizeof(Timer));
        if (!timers) oom();
    }
    // A binary min-heap on the deadline.
    int64_t i = n_timers++;
    while (i > 0 && timers[(i - 1) / 2].deadline > deadline) {
        timers[i] = timers[(i - 1) / 2];
        i = (i - 1) / 2;
    }
    timers[i] = (Timer){deadline, t, wait};
    pthread_mutex_unlock(&timers_lock);
}

static Timer timer_pop_locked(void) {
    Timer top = timers[0], last = timers[--n_timers];
    int64_t i = 0;
    for (;;) {
        int64_t c = 2 * i + 1;
        if (c >= n_timers) break;
        if (c + 1 < n_timers && timers[c + 1].deadline < timers[c].deadline) c++;
        if (timers[c].deadline >= last.deadline) break;
        timers[i] = timers[c];
        i = c;
    }
    if (n_timers > 0) timers[i] = last;
    return top;
}

static void fire_due_timers(void) {
    if (n_timers == 0) return;
    int64_t now = now_ns();
    for (;;) {
        pthread_mutex_lock(&timers_lock);
        if (n_timers == 0 || timers[0].deadline > now) {
            pthread_mutex_unlock(&timers_lock);
            return;
        }
        Timer t = timer_pop_locked();
        pthread_mutex_unlock(&timers_lock);
        end_wait(t.task, t.wait);
    }
}

// Milliseconds until the next timer (-1: none).
static int next_timer_ms(void) {
    pthread_mutex_lock(&timers_lock);
    int64_t d = n_timers ? timers[0].deadline : -1;
    pthread_mutex_unlock(&timers_lock);
    if (d < 0) return -1;
    int64_t ms = (d - now_ns() + 999999) / 1000000;
    return ms < 0 ? 0 : ms > 1000000 ? 1000000 : (int)ms;
}

static void wake_fd_write(void) {
#ifdef __linux__
    uint64_t one = 1;
    ssize_t r = write(wake_wr, &one, sizeof one);
#else
    char one = 1;
    ssize_t r = write(wake_wr, &one, 1);
#endif
    (void)r;
}

static void wake_poller(void) {
    if (atomic_load(&poller_waiting)) wake_fd_write();
}

static int signal_any_pending(void);
static void wake_all_waiters(void);

// Register `fd` so the task (which has switched away) is woken when it's
// readable (`writable`: writable).
static void poller_add(int fd, int writable, Task *t) {
#ifdef __linux__
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = (writable ? EPOLLOUT : EPOLLIN) | EPOLLONESHOT | EPOLLRDHUP;
    ev.data.ptr = t;
    if (epoll_ctl(poll_fd, EPOLL_CTL_ADD, fd, &ev) != 0 && errno == EEXIST) epoll_ctl(poll_fd, EPOLL_CTL_MOD, fd, &ev);
#else
    struct kevent ev;
    EV_SET(&ev, fd, writable ? EVFILT_WRITE : EVFILT_READ, EV_ADD | EV_ONESHOT, 0, 0, t);
    kevent(poll_fd, &ev, 1, NULL, 0, NULL);
#endif
}

static void poller_remove(int fd, int writable) {
#ifdef __linux__
    (void)writable;
    epoll_ctl(poll_fd, EPOLL_CTL_DEL, fd, NULL);
#else
    struct kevent ev;
    EV_SET(&ev, fd, writable ? EVFILT_WRITE : EVFILT_READ, EV_DELETE, 0, 0, NULL);
    kevent(poll_fd, &ev, 1, NULL, 0, NULL);
#endif
}

static void deadlock(void);

// Wait for readiness events (and timers), up to `ms` milliseconds (-1: no limit).
static void poll_events(int ms) {
#ifdef __linux__
    struct epoll_event evs[64];
    int n = epoll_wait(poll_fd, evs, 64, ms);
    for (int i = 0; i < n; i++) {
        if (evs[i].data.ptr == NULL) {
            uint64_t buf;
            while (read(wake_rd, &buf, sizeof buf) > 0) {}
            if (signal_any_pending()) wake_all_waiters();
            continue;
        }
        end_wait((Task *)evs[i].data.ptr, -1);
    }
#else
    struct kevent evs[64];
    struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000}, *tp = ms < 0 ? NULL : &ts;
    int n = kevent(poll_fd, NULL, 0, evs, 64, tp);
    for (int i = 0; i < n; i++) {
        if (evs[i].udata == NULL) {
            char buf[64];
            while (read(wake_rd, buf, sizeof buf) > 0) {}
            if (signal_any_pending()) wake_all_waiters();
            continue;
        }
        end_wait((Task *)evs[i].udata, -1);
    }
#endif
    fire_due_timers();
}

static int idle_checks; // (guarded by poll_lock)

static void idle(Worker *w) {
    // One idle worker polls for everyone; the others sleep until there's work.
    if (pthread_mutex_trylock(&poll_lock) == 0) {
        atomic_store(&poller_waiting, 1);
        int ms = atomic_load(&n_queued) > 0 ? 0 : next_timer_ms();
        if (ms < 0 && atomic_load(&n_io_waits) == 0) {
            // Nothing can wake anyone unless a busy worker does: look again
            // in a moment, and if everyone was idle both times, give up.
            pthread_mutex_lock(&idle_lock);
            int all_idle = n_sleeping == n_workers - 1 && atomic_load(&n_queued) == 0;
            pthread_mutex_unlock(&idle_lock);
            idle_checks = all_idle ? idle_checks + 1 : 0;
            if (idle_checks >= 3 && n_timers == 0) deadlock();
            ms = 100;
        } else {
            idle_checks = 0;
        }
        poll_events(ms);
        atomic_store(&poller_waiting, 0);
        pthread_mutex_unlock(&poll_lock);
        return;
    }
    pthread_mutex_lock(&idle_lock);
    if (atomic_load(&n_queued) == 0) {
        w->sleeping = 1;
        n_sleeping++;
        while (w->sleeping) pthread_cond_wait(&w->wake, &idle_lock);
    }
    pthread_mutex_unlock(&idle_lock);
}

__attribute__((noreturn)) static void worker_loop(Worker *w) {
    for (;;) {
        Task *t = find_task(w);
        if (t) run_task(w, t);
        else idle(w);
    }
}

// ---------------- stack overflow
// A task that recurses deeper than its stack hits the guard page below it.
// Say so, rather than a bare "Segmentation fault", and stop the program (a
// task can't unwind without stack to do it on). The handler runs on a small
// stack of each thread's own.

static size_t page_size;

static void write_str(const char *m) {
    ssize_t r = write(2, m, strlen(m));
    (void)r;
}

static void on_stack_fault(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    Task *t = tl_task;
    char *a = si->si_addr;
    // (A frame bigger than a page can step over the guard: allow some room.)
    if (t && t->stack && a >= t->stack - 16 * page_size && a < t->stack + page_size) {
        if (ftrylockfile(stdout) == 0) {
            fflush(stdout);
            funlockfile(stdout);
        }
        char id[24];
        int n = 0;
        uint64_t v = (uint64_t)t->id;
        char rev[24];
        do rev[n++] = (char)('0' + v % 10), v /= 10; while (v);
        for (int i = 0; i < n; i++) id[i] = rev[n - 1 - i];
        id[n] = 0;
        write_str("stack overflow in task ");
        write_str(id);
        write_str(": its calls went deeper than its stack allows (1 MiB; set TARN_TASK_STACK=<bytes> for more)\n");
        _exit(101);
    }
    signal(sig, SIG_DFL);
    raise(sig);
}

static void fault_stack(void) {
#if !defined(TARN_SANITIZE)
    stack_t ss;
    ss.ss_size = 64 * 1024;
    ss.ss_sp = malloc(ss.ss_size);
    ss.ss_flags = 0;
    if (ss.ss_sp) sigaltstack(&ss, NULL);
#endif
}

static void catch_stack_faults(void) {
#if !defined(TARN_SANITIZE) // (ASan reports stack overflows itself.)
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_stack_fault;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
#endif
}

static void *worker_thread(void *arg) {
    Worker *w = arg;
    tl_worker = w;
    fault_stack();
#if defined(TARN_SANITIZE)
    pthread_attr_t a;
    pthread_getattr_np(pthread_self(), &a);
    pthread_attr_getstack(&a, &w->stack_bottom, &w->stack_size);
    pthread_attr_destroy(&a);
#endif
    worker_loop(w);
}

// Worker 0's loop starts the first time `main` waits.
static void worker0_entry(void) {
#if defined(TARN_SANITIZE)
    asan_arrived(main_task.asan_fake);
#endif
    tl_task = NULL;
    tl_ctx = NULL;
    void (*after)(void *) = main_task.after;
    main_task.after = NULL;
    if (after) after(main_task.after_arg);
    worker_loop(&workers[0]);
}

static int64_t env_int(const char *name, int64_t fallback) {
    const char *v = getenv(name);
    if (!v || !*v) return fallback;
    char *end;
    long long n = strtoll(v, &end, 10);
    return *end || n <= 0 ? fallback : (int64_t)n;
}


// The first `spawn`: start the worker threads. This thread becomes worker 0;
// `main` keeps running on it (and only on it), and the scheduler loop of
// worker 0 gets a stack of its own for when `main` waits.
static void sched_start(void) {
    n_workers = (int)env_int("TARN_THREADS", sysconf(_SC_NPROCESSORS_ONLN));
    if (n_workers < 1) n_workers = 1;
    if (n_workers > 256) n_workers = 256;
    task_stack_size = (size_t)env_int("TARN_TASK_STACK", 1 << 20);
    size_t page = (size_t)sysconf(_SC_PAGESIZE);
    page_size = page;
    task_stack_size = (task_stack_size + 2 * page - 1) / page * page;
    fault_stack();
    catch_stack_faults();
    workers = calloc((size_t)n_workers, sizeof(Worker));
    if (!workers) oom();
#ifdef __linux__
    poll_fd = epoll_create1(EPOLL_CLOEXEC);
    int efd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    wake_rd = wake_wr = efd;
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.ptr = NULL;
    epoll_ctl(poll_fd, EPOLL_CTL_ADD, efd, &ev);
#else
    poll_fd = kqueue();
    int p[2];
    if (pipe(p) != 0) oom();
    fcntl(p[0], F_SETFL, O_NONBLOCK);
    fcntl(p[1], F_SETFL, O_NONBLOCK);
    fcntl(p[0], F_SETFD, FD_CLOEXEC);
    fcntl(p[1], F_SETFD, FD_CLOEXEC);
    wake_rd = p[0];
    wake_wr = p[1];
    struct kevent ev;
    EV_SET(&ev, wake_rd, EVFILT_READ, EV_ADD, 0, 0, NULL);
    kevent(poll_fd, &ev, 1, NULL, 0, NULL);
#endif
    for (int i = 0; i < n_workers; i++) {
        workers[i].id = i;
        pthread_mutex_init(&workers[i].lock, NULL);
        pthread_cond_init(&workers[i].wake, NULL);
    }
    // Worker 0's scheduler loop runs on a stack of its own.
    char *s = stack_alloc();
    workers[0].sched_sp = init_stack(s + task_stack_size, worker0_entry);
#if defined(TARN_SANITIZE)
    workers[0].stack_bottom = s;
    workers[0].stack_size = task_stack_size;
#endif
    tl_worker = &workers[0];
    tl_task = &main_task;
    main_task.ctxp = &main_ctx;
    tl_ctx = &main_ctx;
    sched_on = 1;
    for (int i = 1; i < n_workers; i++) {
        pthread_t th;
        pthread_attr_t a;
        pthread_attr_init(&a);
        pthread_attr_setdetachstate(&a, PTHREAD_CREATE_DETACHED);
        if (pthread_create(&th, &a, worker_thread, &workers[i]) != 0) oom();
        pthread_attr_destroy(&a);
    }
}

// ---------------- waiting for time and I/O

typedef struct {
    Task *task;
    int64_t wait, deadline;
    int fd, writable;
} WaitArg;

static void begin_wait(void *p) {
    WaitArg *a = p;
    Task *t = a->task;
    atomic_store(&t->wait_state, a->wait * 2);
    if (a->deadline > 0) timer_add(a->deadline, t, a->wait);
    if (a->fd >= 0) {
        atomic_fetch_add(&n_io_waits, 1);
        poller_add(a->fd, a->writable, t);
    }
    // (A sleep or I/O wait with neither a deadline nor a poller to end it can't happen.)
    if (atomic_load(&poller_waiting)) wake_poller(); // so it picks up the new deadline
}

// Wait until `deadline` (monotonic ns, 0: none), until `fd` is readable or
// writable (-1: no fd), or until a caught signal arrives. Wakes can be early:
// callers check again.
static int any_signal_caught(void);
static void wait_until(int64_t deadline, int fd, int writable) {
    Task *t = cur_task();
    // While signals are caught, wake up regularly to check for them.
    if (any_signal_caught()) {
        int64_t soon = now_ns() + 100000000;
        if (!deadline || deadline > soon) deadline = soon;
    }
    WaitArg a = {t, ++t->wait_count, deadline, fd, writable};
    park(begin_wait, &a);
    if (fd >= 0) {
        poller_remove(fd, writable);
        atomic_fetch_sub(&n_io_waits, 1);
    }
}

// A caught signal ends every timed wait, so sleeps and I/O waits (which
// wake up regularly while signals are caught) check for it.
static void wake_all_waiters(void) {
    pthread_mutex_lock(&timers_lock);
    for (int64_t i = 0; i < n_timers; i++) end_wait(timers[i].task, timers[i].wait);
    pthread_mutex_unlock(&timers_lock);
}

// ---------------- task lifetime

static pthread_mutex_t live_lock = PTHREAD_MUTEX_INITIALIZER;
static int main_waits_for_all; // (guarded by live_lock)

static void task_finished(void *p);
static _Unwind_Reason_Code unwind_stop(int version, _Unwind_Action actions, _Unwind_Exception_Class cls, struct _Unwind_Exception *ex, struct _Unwind_Context *uc, void *param);

static void release_task_closure(Task **tp) {
    Task *t = *tp;
    if (t->closure) {
        void *c = t->closure;
        t->closure = NULL;
        t->release_closure(c);
    }
}

// Runs the task's function. (A frame of its own, below `task_entry`, so its
// cleanup runs when a panic unwinds the task, before the unwinder stops.)
static __attribute__((noinline)) void task_body(Task *t) {
    __attribute__((cleanup(release_task_closure))) Task *guard = t;
    t->run(t->closure, t->result);
}

// The last things a task does on its own stack: free what its cycles held.
static void task_wrap_up(Task *t) {
    TarnCtx *c = t->ctxp;
    if (c->roots.n) collect_roots(c, &c->roots);
}

static void task_entry(void) {
    Task *t = cur_task();
#if defined(TARN_SANITIZE)
    asan_arrived(NULL);
#endif
    task_body(t);
    task_wrap_up(t);
    park(task_finished, t);
    __builtin_unreachable();
}

// On the scheduler's stack, once the task is done: wake whoever joins it,
// recycle the stack, and drop the scheduler's reference.
static void handle_release(void *p);
static void task_finished(void *p) {
    Task *t = p;
    ctx_free(&t->ctx);
    stack_free(t->stack);
    t->stack = NULL;
    pthread_mutex_lock(&t->lock);
    t->done = 1;
    Task *j = t->joiners;
    t->joiners = NULL;
    pthread_mutex_unlock(&t->lock);
    while (j) {
        Task *next = j->next;
        ready(j);
        j = next;
    }
    handle_release(t);
    // The last task: `main` may be waiting for it at the end of the program.
    pthread_mutex_lock(&live_lock);
    int wake_main = atomic_fetch_sub(&live_tasks, 1) == 1 && main_waits_for_all;
    if (wake_main) main_waits_for_all = 0;
    pthread_mutex_unlock(&live_lock);
    if (wake_main) ready(&main_task);
}

void *tarn_task_spawn(void *closure, void (*run)(void *, void *), void (*release_closure)(void *), void (*release_result)(void *)) {
    if (!sched_on) sched_start();
    Task *t = tarn_alloc(sizeof(Task));
    memset(t, 0, sizeof *t);
    atomic_store(&t->rc, 2); // the handle returned, and the scheduler's while it runs
    t->kind = HANDLE_TASK;
    t->id = atomic_fetch_add(&next_task_id, 1) + 1;
    t->ctxp = &t->ctx;
    t->ctx.cur_roots = &t->ctx.roots;
    t->ctx.roots_threshold = 10000;
    t->closure = closure;
    t->run = run;
    t->release_closure = release_closure;
    t->release_result = release_result;
    t->wait_state = 1;
    pthread_mutex_init(&t->lock, NULL);
    t->stack = stack_alloc();
    t->sp = init_stack(t->stack + task_stack_size, task_entry);
    atomic_fetch_add(&live_tasks, 1);
    ready(t);
    return t;
}

// `task.join()`: wait for it to finish. 1 if it returned, 0 if it panicked.
int64_t tarn_rt_task_wait(void *h) {
    Task *t = h, *me = cur_task();
    pthread_mutex_lock(&t->lock);
    while (!t->done) {
        me->next = t->joiners;
        t->joiners = me;
        park(unlock_mutex, &t->lock);
        pthread_mutex_lock(&t->lock);
    }
    int ok = !t->panicked;
    pthread_mutex_unlock(&t->lock);
    return ok;
}

int64_t tarn_rt_task_is_done(void *h) {
    Task *t = h;
    pthread_mutex_lock(&t->lock);
    int d = t->done;
    pthread_mutex_unlock(&t->lock);
    return d;
}

static void tarn_die(const char *file, int line, int col, const char *msg);

// The finished task's result, into `out` (once).
void tarn_task_take(void *h, void *out, const char *file, int line, int col) {
    Task *t = h;
    pthread_mutex_lock(&t->lock);
    int taken = t->taken;
    t->taken = 1;
    if (!taken) memcpy(out, t->result, sizeof t->result);
    pthread_mutex_unlock(&t->lock);
    if (taken) tarn_die(file, line, col, "join() already gave this task's result away");
}

static TarnStr *str_from(const char *data, int64_t len);
TarnStr *tarn_rt_task_panic_message(void *h) {
    Task *t = h;
    TarnStr *m = t->panic_message;
    return m ? str_from(m->data, m->len) : str_from("", 0);
}
TarnStr *tarn_rt_task_panic_location(void *h) {
    Task *t = h;
    TarnStr *m = t->panic_location;
    return m ? str_from(m->data, m->len) : str_from("", 0);
}

// The end of the program: `main` returned. Wait for every task first.
static void wait_for_tasks(void) {
    if (!sched_on) return;
    pthread_mutex_lock(&live_lock);
    while (atomic_load(&live_tasks) > 0) {
        main_waits_for_all = 1;
        park(unlock_mutex, &live_lock);
        pthread_mutex_lock(&live_lock);
    }
    pthread_mutex_unlock(&live_lock);
}

static void deadlock(void) {
    fflush(stdout);
    fprintf(stderr, "deadlock: every task is waiting (for a channel, a lock or another task), and nothing can wake any of them\n");
    _exit(101);
}

// ---------------- panics in tasks
// A panic in a task other than `main` stops just that task: the stack is
// unwound (every reference the task held is released, so `fn drop`s run and
// sockets close) up to `task_entry`, then the task is done, and `join()`
// gives the panic as an error. A panic in `main`, or while a value is being
// freed, still ends the program.

static void panic_exit(void) {
    fflush(stdout);
    exit(101);
}

static void free_unwind(_Unwind_Reason_Code r, struct _Unwind_Exception *e) { (void)r; (void)e; }

static _Unwind_Reason_Code unwind_stop(int version, _Unwind_Action actions, _Unwind_Exception_Class cls, struct _Unwind_Exception *ex, struct _Unwind_Context *uc, void *param) {
    (void)version;
    (void)cls;
    (void)ex;
    Task *t = param;
    if ((actions & _UA_END_OF_STACK) || _Unwind_GetRegionStart(uc) == (uintptr_t)&task_entry) {
        // Everything above `task_entry` is unwound: the task is done.
        t->panicked = 1;
        task_wrap_up(t);
        park(task_finished, t);
        __builtin_unreachable();
    }
    return _URC_NO_REASON;
}

static void panic_task(const char *file, int line, int col, const char *msg, int64_t msg_len) {
    Task *t = cur_task();
    TarnCtx *c = ctx();
    fflush(stdout);
    if (t == &main_task || !sched_on || c->dropping || c->collecting) {
        if (c->dropping || c->collecting) fprintf(stderr, "panic at %s:%d:%d: %.*s\n(while freeing a value: the program can't go on)\n", file, line, col, (int)msg_len, msg);
        else fprintf(stderr, "panic at %s:%d:%d: %.*s\n", file, line, col, (int)msg_len, msg);
        panic_exit();
    }
    fprintf(stderr, "panic in task %lld at %s:%d:%d: %.*s\n", (long long)t->id, file, line, col, (int)msg_len, msg);
    char loc[600];
    snprintf(loc, sizeof loc, "%s:%d:%d", file, line, col);
    t->panic_message = str_from(msg, msg_len);
    t->panic_location = str_from(loc, (int64_t)strlen(loc));
    memset(&t->unwind, 0, sizeof t->unwind);
    t->unwind.exception_class = 0x5441524E50414E43ull; // "TARNPANC"
    t->unwind.exception_cleanup = free_unwind;
    _Unwind_ForcedUnwind(&t->unwind, unwind_stop, t);
    // (Only if there was no unwind information at all.)
    fprintf(stderr, "panic: could not unwind the task\n");
    panic_exit();
}

// ---------------- handles

static void channel_free(void *p);
static void mutex_free(void *p);

static void handle_release(void *p) {
    _Atomic int64_t *rc = p;
    if (atomic_fetch_sub_explicit(rc, 1, memory_order_acq_rel) != 1) return;
    int64_t kind = ((int64_t *)p)[1];
    if (kind == HANDLE_TASK) {
        Task *t = p;
        if (t->done && !t->panicked && !t->taken && t->release_result) t->release_result(t->result);
        if (t->panic_message) tarn_str_free(t->panic_message);
        if (t->panic_location) tarn_str_free(t->panic_location);
        pthread_mutex_destroy(&t->lock);
        tarn_free(t, sizeof(Task));
    } else if (kind == HANDLE_CHANNEL) {
        channel_free(p);
    } else if (kind == HANDLE_MUTEX) {
        mutex_free(p);
    } else {
        tarn_free(p, 24);
    }
}

// The last reference to a handle went away (called by the generated code).
void tarn_handle_free(void *p) {
    atomic_store((_Atomic int64_t *)p, 1);
    handle_release(p);
}

// ---------------- channels
// A queue of values from one task to another. `send` waits while it's full;
// an unbuffered channel (capacity 0) waits until its value is received.
// `receive` waits while it's empty, and gives nothing once it's closed and empty.

typedef struct {
    _Atomic int64_t rc;
    int64_t kind;
    pthread_mutex_t lock;
    int64_t cap, slot;         // capacity (0: unbuffered), bytes per value
    void (*release)(void *);   // releases a value in its slot (null: nothing to release)
    char *buf;
    int64_t head, len, bufcap; // a ring of `bufcap` slots
    int64_t sent, taken;       // values ever sent / received
    int closed;
    Task *senders, *senders_tail;     // waiting for room
    Task *receivers, *receivers_tail; // waiting for a value
    Task *handoffs;                   // (unbuffered) waiting for their value to be taken
} Channel;

void *tarn_chan_new(int64_t cap, int64_t slot, void (*release)(void *), const char *file, int line, int col) {
    if (cap < 0) tarn_die(file, line, col, "a channel's capacity can't be negative");
    Channel *c = tarn_alloc(sizeof(Channel));
    memset(c, 0, sizeof *c);
    atomic_store(&c->rc, 1);
    c->kind = HANDLE_CHANNEL;
    pthread_mutex_init(&c->lock, NULL);
    c->cap = cap;
    c->slot = slot;
    c->release = release;
    c->bufcap = cap > 0 ? cap : 1;
    c->buf = malloc((size_t)(c->bufcap * slot));
    if (!c->buf) oom();
    return c;
}

static void channel_free(void *p) {
    Channel *c = p;
    for (int64_t i = 0; i < c->len && c->release; i++) c->release(c->buf + ((c->head + i) % c->bufcap) * c->slot);
    free(c->buf);
    pthread_mutex_destroy(&c->lock);
    tarn_free(c, sizeof(Channel));
}

static void wake_list(Task **head, Task **tail) {
    Task *t = *head;
    *head = NULL;
    if (tail) *tail = NULL;
    while (t) {
        Task *next = t->next;
        ready(t);
        t = next;
    }
}

static void wake_one(Task **head, Task **tail) {
    Task *t = pop(head, tail);
    if (t) ready(t);
}

// Only one task (no scheduler): it would wait for itself.
static void lone_wait(const char *what, const char *file, int line, int col) {
    char m[200];
    snprintf(m, sizeof m, "%s, and no other task exists to do it: the program would wait forever", what);
    tarn_die(file, line, col, m);
}

// `value` (a slot of `c->slot` bytes, already the channel's to keep) goes in.
void tarn_chan_send(void *h, void *value, const char *file, int line, int col) {
    Channel *c = h;
    Task *me = cur_task();
    pthread_mutex_lock(&c->lock);
    while (!c->closed && c->len == c->bufcap) {
        if (!sched_on) {
            pthread_mutex_unlock(&c->lock);
            lone_wait("this channel is full: `send` waits for a `receive`", file, line, col);
        }
        push(&c->senders, &c->senders_tail, me);
        park(unlock_mutex, &c->lock);
        pthread_mutex_lock(&c->lock);
    }
    if (c->closed) {
        pthread_mutex_unlock(&c->lock);
        if (c->release) c->release(value);
        tarn_die(file, line, col, "send() on a closed channel");
    }
    memcpy(c->buf + ((c->head + c->len) % c->bufcap) * c->slot, value, (size_t)c->slot);
    c->len++;
    int64_t seq = c->sent++;
    wake_one(&c->receivers, &c->receivers_tail);
    // Unbuffered: wait until a receiver has taken it.
    while (c->cap == 0 && c->taken <= seq && !c->closed) {
        if (!sched_on) {
            pthread_mutex_unlock(&c->lock);
            lone_wait("this channel is unbuffered: `send` waits until a `receive` takes the value", file, line, col);
        }
        me->next = c->handoffs;
        c->handoffs = me;
        park(unlock_mutex, &c->lock);
        pthread_mutex_lock(&c->lock);
    }
    pthread_mutex_unlock(&c->lock);
}

// The next value into `out`: 1, or 0 once the channel is closed and empty.
int64_t tarn_chan_receive(void *h, void *out, const char *file, int line, int col) {
    Channel *c = h;
    Task *me = cur_task();
    pthread_mutex_lock(&c->lock);
    while (c->len == 0) {
        if (c->closed) {
            pthread_mutex_unlock(&c->lock);
            return 0;
        }
        if (!sched_on) {
            pthread_mutex_unlock(&c->lock);
            lone_wait("this channel is empty: `receive` waits for a `send`", file, line, col);
        }
        push(&c->receivers, &c->receivers_tail, me);
        park(unlock_mutex, &c->lock);
        pthread_mutex_lock(&c->lock);
    }
    memcpy(out, c->buf + c->head * c->slot, (size_t)c->slot);
    c->head = (c->head + 1) % c->bufcap;
    c->len--;
    c->taken++;
    wake_one(&c->senders, &c->senders_tail);
    wake_list(&c->handoffs, NULL);
    pthread_mutex_unlock(&c->lock);
    return 1;
}

void tarn_rt_chan_close(void *h) {
    Channel *c = h;
    pthread_mutex_lock(&c->lock);
    c->closed = 1;
    wake_list(&c->senders, &c->senders_tail);
    wake_list(&c->receivers, &c->receivers_tail);
    wake_list(&c->handoffs, NULL);
    pthread_mutex_unlock(&c->lock);
}

int64_t tarn_rt_chan_is_closed(void *h) {
    Channel *c = h;
    pthread_mutex_lock(&c->lock);
    int64_t r = c->closed;
    pthread_mutex_unlock(&c->lock);
    return r;
}

int64_t tarn_rt_chan_len(void *h) {
    Channel *c = h;
    pthread_mutex_lock(&c->lock);
    int64_t r = c->len;
    pthread_mutex_unlock(&c->lock);
    return r;
}

// ---------------- mutexes
// A value that several tasks reach, changed by one at a time. A task holds
// it for the length of one call (`lock(fn(v) -> ...)`): what goes in is
// moved or copied in, what comes out is moved or copied out, so nothing in
// it is ever reachable from a task that doesn't hold it. Waiting for it
// parks the task, not the thread.

typedef struct {
    _Atomic int64_t rc;
    int64_t kind;
    pthread_mutex_t lock; // guards the fields below
    int held;
    Task *owner;
    Task *waiters, *waiters_tail;
    int64_t slot;
    void (*release)(void *);
    int64_t value[2];
    RootStack roots; // possible cycle roots found while it was held
} Mutex;

void *tarn_mutex_new(void *value, int64_t slot, void (*release)(void *)) {
    Mutex *m = tarn_alloc(sizeof(Mutex));
    memset(m, 0, sizeof *m);
    atomic_store(&m->rc, 1);
    m->kind = HANDLE_MUTEX;
    pthread_mutex_init(&m->lock, NULL);
    m->slot = slot;
    m->release = release;
    memcpy(m->value, value, (size_t)slot);
    return m;
}

static void mutex_free(void *p) {
    Mutex *m = p;
    if (m->release) m->release(m->value);
    // (Nothing else can reach its roots now: whatever they hold is garbage or gone.)
    if (m->roots.n) collect_roots(ctx(), &m->roots);
    free(m->roots.items);
    pthread_mutex_destroy(&m->lock);
    tarn_free(m, sizeof(Mutex));
}

static void mutex_acquire(Mutex *m, const char *file, int line, int col) {
    Task *me = cur_task();
    pthread_mutex_lock(&m->lock);
    if (!m->held) {
        m->held = 1;
        m->owner = me;
        pthread_mutex_unlock(&m->lock);
        return;
    }
    if (m->owner == me) {
        pthread_mutex_unlock(&m->lock);
        tarn_die(file, line, col, "this task already holds this Mutex: locking it again inside its own `lock` would wait forever");
    }
    push(&m->waiters, &m->waiters_tail, me);
    park(unlock_mutex, &m->lock);
    // (`mutex_release` handed it over: we hold it now.)
}

static void mutex_release(Mutex *m) {
    pthread_mutex_lock(&m->lock);
    Task *next = pop(&m->waiters, &m->waiters_tail);
    m->owner = next;
    m->held = next != NULL;
    pthread_mutex_unlock(&m->lock);
    if (next) ready(next);
}

typedef struct {
    Mutex *m;
    TarnCtx *c;
    RootStack *saved_roots;
    void *closure;
    void (*release_closure)(void *);
} Held;

static void mutex_leave(Held *h) {
    // Cycles that died while it was held, and the function's own captures,
    // are freed while it's still held (its value may be among them).
    if (h->closure) h->release_closure(h->closure);
    if (h->m->roots.n) collect_roots(h->c, &h->m->roots);
    h->c->cur_roots = h->saved_roots;
    mutex_release(h->m);
}

// Hold the Mutex while `run(closure, value, out)` runs (the generated code
// calls the function value and moves or copies its result into `out`).
// The Mutex is let go even if the function panics.
void tarn_mutex_run(void *h, void *closure, void (*release_closure)(void *), void (*run)(void *, void *, void *), void *out, const char *file, int line, int col) {
    Mutex *m = h;
    mutex_acquire(m, file, line, col);
    TarnCtx *c = ctx();
    __attribute__((cleanup(mutex_leave))) Held held = {m, c, c->cur_roots, closure, release_closure};
    c->cur_roots = &m->roots;
    run(closure, m->value, out);
}

// ---------------- atomics

typedef struct {
    _Atomic int64_t rc;
    int64_t kind;
    _Atomic int64_t value;
} AtomicInt;

void *tarn_rt_atomic_new(int64_t v) {
    AtomicInt *a = tarn_alloc(sizeof(AtomicInt));
    atomic_store(&a->rc, 1);
    a->kind = HANDLE_ATOMIC;
    atomic_store(&a->value, v);
    return a;
}
int64_t tarn_rt_atomic_get(void *h) { return atomic_load(&((AtomicInt *)h)->value); }
void tarn_rt_atomic_set(void *h, int64_t v) { atomic_store(&((AtomicInt *)h)->value, v); }
int64_t tarn_rt_atomic_add(void *h, int64_t n) { return atomic_fetch_add(&((AtomicInt *)h)->value, n) + n; }
int64_t tarn_rt_atomic_swap(void *h, int64_t v) { return atomic_exchange(&((AtomicInt *)h)->value, v); }
int64_t tarn_rt_atomic_compare_and_set(void *h, int64_t expected, int64_t v) {
    return atomic_compare_exchange_strong(&((AtomicInt *)h)->value, &expected, v);
}

int64_t tarn_rt_cpu_count(void) { return sched_on ? n_workers : env_int("TARN_THREADS", sysconf(_SC_NPROCESSORS_ONLN)); }

// ---------------- moving and copying values between tasks
// The generated `tarn.xfer.*` functions walk a value about to go to another
// task: an object nobody else references is moved as it is (its children
// walked in turn), anything still referenced here is copied. Copies made in
// one walk are remembered, so shared parts and cycles stay shared and cyclic.

typedef struct {
    void **keys, **vals;
    int64_t n, cap;
} XferMemo;

// Is this object ours alone, to move? (`cyclic`: its count is in bits 3..,
// and a buffered one is still in this task's root buffer: copy it instead.)
int64_t tarn_xfer_unique(void *p, int64_t cyclic) {
    int64_t rc = *(int64_t *)p;
    if (rc >= RC_IMMORTAL) return 1;
    return cyclic ? (rc >> 3) == 1 && !(rc & BUFFERED) : rc == 1;
}

static uint64_t mix_ptr(void *p) {
    uint64_t x = (uint64_t)(uintptr_t)p;
    x ^= x >> 33;
    x *= 0xff51afd7ed558ccdull;
    x ^= x >> 33;
    return x;
}

void *tarn_xfer_seen(void **memo, void *p) {
    XferMemo *m = *memo;
    if (!m || !m->n) return NULL;
    for (uint64_t i = mix_ptr(p) & (uint64_t)(m->cap - 1);; i = (i + 1) & (uint64_t)(m->cap - 1)) {
        if (m->keys[i] == p) return m->vals[i];
        if (!m->keys[i]) return NULL;
    }
}

void tarn_xfer_remember(void **memo, void *p, void *copy) {
    XferMemo *m = *memo;
    if (!m) {
        m = *memo = calloc(1, sizeof(XferMemo));
        if (!m) oom();
    }
    if ((m->n + 1) * 2 > m->cap) {
        XferMemo old = *m;
        m->cap = old.cap ? old.cap * 2 : 16;
        m->keys = calloc((size_t)m->cap, sizeof(void *));
        m->vals = calloc((size_t)m->cap, sizeof(void *));
        if (!m->keys || !m->vals) oom();
        m->n = 0;
        for (int64_t i = 0; i < old.cap; i++)
            if (old.keys[i]) tarn_xfer_remember(memo, old.keys[i], old.vals[i]);
        free(old.keys);
        free(old.vals);
    }
    uint64_t i = mix_ptr(p) & (uint64_t)(m->cap - 1);
    while (m->keys[i]) i = (i + 1) & (uint64_t)(m->cap - 1);
    m->keys[i] = p;
    m->vals[i] = copy;
    m->n++;
}

void tarn_xfer_done(void **memo) {
    XferMemo *m = *memo;
    if (!m) return;
    free(m->keys);
    free(m->vals);
    free(m);
    *memo = NULL;
}

// Where the value being moved or copied comes from (for the panic below).
static __thread const char *xfer_file;
static __thread int xfer_line, xfer_col;

void tarn_xfer_at(const char *file, int line, int col) {
    xfer_file = file;
    xfer_line = line;
    xfer_col = col;
}

// A value with a `fn drop` (a socket, say) can be moved to another task, but
// not copied: two copies would both clean up the same thing.
void tarn_xfer_cant_copy(const char *type_name) {
    char m[300];
    snprintf(m, sizeof m, "a `%s` can't be copied to another task (it has a `fn drop`), and this task still uses it: give it away on its last use", type_name);
    tarn_die(xfer_file ? xfer_file : "<task>", xfer_line, xfer_col, m);
}

TarnStr *tarn_str_clone(TarnStr *s) { return str_dup(s); }

// ---------------------------------------------------------------- strings

static TarnStr *str_dup(TarnStr *s) {
    s->rc++;
    return s;
}

// A fresh string with room for at least `cap` bytes (the size class's slack
// becomes extra capacity for in-place appends).
static TarnStr *str_alloc(int64_t cap) {
    int64_t size = round16((int64_t)sizeof(TarnStr) + cap);
    TarnStr *s = tarn_alloc(size);
    s->rc = 1;
    s->len = 0;
    s->cap = size - (int64_t)sizeof(TarnStr);
    return s;
}

static TarnStr *str_new(int64_t len) {
    TarnStr *s = str_alloc(len);
    s->len = len;
    return s;
}

static TarnStr *str_from(const char *data, int64_t len) {
    TarnStr *s = str_new(len);
    memcpy(s->data, data, (size_t)len);
    return s;
}

// Drop functions take `void *`: the generated code calls them through TarnDropFn.
void tarn_str_free(void *p) {
    TarnStr *s = p;
    tarn_free(s, (int64_t)sizeof(TarnStr) + s->cap);
}

TarnStr *tarn_str_concat(TarnStr *a, TarnStr *b) {
    if (a->len == 0) return str_dup(b);
    if (b->len == 0) return str_dup(a);
    TarnStr *s = str_new(a->len + b->len);
    memcpy(s->data, a->data, (size_t)a->len);
    memcpy(s->data + a->len, b->data, (size_t)b->len);
    return s;
}

// `a + b` where the caller gives up its reference to `a`. When nobody else
// holds `a`, `b` is appended in place (with doubling capacity), which makes
// `s += x` loops and f-strings linear instead of quadratic.
TarnStr *tarn_str_append_owned(TarnStr *a, TarnStr *b) {
    if (b->len == 0) return a;
    int64_t blen = b->len, n = a->len + blen;
    if (a->rc != 1) {
        TarnStr *s = str_alloc(n + n / 2);
        s->len = n;
        memcpy(s->data, a->data, (size_t)a->len);
        memcpy(s->data + a->len, b->data, (size_t)blen);
        a->rc--; // shared: someone else still holds it, so it can't reach zero
        return s;
    }
    if (n > a->cap) {
        int64_t cap = a->cap * 2 > n ? a->cap * 2 : n;
        TarnStr *s = str_alloc(cap);
        s->len = a->len;
        memcpy(s->data, a->data, (size_t)a->len);
        if (b == a) b = s;
        tarn_str_free(a);
        a = s;
    }
    memcpy(a->data + a->len, b->data, (size_t)blen);
    a->len = n;
    return a;
}

int64_t tarn_str_eq(TarnStr *a, TarnStr *b) {
    return a->len == b->len && memcmp(a->data, b->data, (size_t)a->len) == 0;
}

int64_t tarn_str_cmp(TarnStr *a, TarnStr *b) {
    int64_t n = a->len < b->len ? a->len : b->len;
    int c = memcmp(a->data, b->data, (size_t)n);
    if (c != 0) return c < 0 ? -1 : 1;
    return a->len < b->len ? -1 : (a->len > b->len ? 1 : 0);
}

TarnStr *tarn_int_to_str(int64_t v) {
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

TarnStr *tarn_float_to_str(double d) {
    char buf[64];
    int n = format_float(d, buf, sizeof buf);
    return str_from(buf, n);
}

TarnStr *tarn_bool_to_str(int64_t b) {
    return b ? str_from("true", 4) : str_from("false", 5);
}

TarnStr *tarn_input(void) {
    char *line = NULL;
    size_t cap = 0;
    fflush(stdout);
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) {
        free(line);
        return str_new(0);
    }
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) n--;
    TarnStr *s = str_from(line, n);
    free(line);
    return s;
}

// Parsing (like Kotlin's toInt(): invalid input is a panic, not a silent 0).
static int str_to_cbuf(TarnStr *s, char *buf, size_t cap) {
    int64_t a = 0, b = s->len;
    while (a < b && (s->data[a] == ' ' || s->data[a] == '\t')) a++;
    while (b > a && (s->data[b - 1] == ' ' || s->data[b - 1] == '\t')) b--;
    if (b - a <= 0 || (size_t)(b - a) >= cap) return 0;
    memcpy(buf, s->data + a, (size_t)(b - a));
    buf[b - a] = 0;
    return 1;
}

static void bad_number(TarnStr *s, const char *ty, const char *file, int line, int col) {
    char buf[160];
    snprintf(buf, sizeof buf, "cannot parse \"%.*s\" as %s", s->len > 64 ? 64 : (int)s->len, s->data, ty);
    tarn_die(file, line, col, buf);
}

int64_t tarn_str_to_int(TarnStr *s, const char *file, int line, int col) {
    char buf[64], *end;
    if (!str_to_cbuf(s, buf, sizeof buf)) bad_number(s, "Int", file, line, col);
    errno = 0;
    long long v = strtoll(buf, &end, 10);
    if (*end != 0 || errno == ERANGE) bad_number(s, "Int", file, line, col);
    return (int64_t)v;
}

// A boolean in text: 1 for true/yes/on/1, 0 for false/no/off/0 (any case,
// spaces around ignored), -1 for anything else.
int64_t tarn_str_parse_bool(TarnStr *s) {
    char buf[8];
    if (!str_to_cbuf(s, buf, sizeof buf)) return -1;
    for (char *p = buf; *p; p++)
        if (*p >= 'A' && *p <= 'Z') *p = (char)(*p - 'A' + 'a');
    static const char *yes[] = {"true", "yes", "on", "1"}, *no[] = {"false", "no", "off", "0"};
    for (int i = 0; i < 4; i++) {
        if (strcmp(buf, yes[i]) == 0) return 1;
        if (strcmp(buf, no[i]) == 0) return 0;
    }
    return -1;
}

int64_t tarn_rt_str_to_bool(TarnStr *s, const char *file, int line, int col) {
    int64_t v = tarn_str_parse_bool(s);
    if (v < 0) {
        char buf[200];
        snprintf(buf, sizeof buf, "cannot parse \"%.*s\" as Bool (use true or false, yes or no, on or off, 1 or 0)", s->len > 64 ? 64 : (int)s->len, s->data);
        tarn_die(file, line, col, buf);
    }
    return v;
}

// `to_int_or_null()`: 1 and the value in *out if `s` is a valid Int, else 0 (and *out = 0).
int64_t tarn_str_parse_int(TarnStr *s, int64_t *out) {
    char buf[64], *end;
    *out = 0;
    if (!str_to_cbuf(s, buf, sizeof buf)) return 0;
    errno = 0;
    long long v = strtoll(buf, &end, 10);
    if (*end != 0 || errno == ERANGE) return 0;
    *out = (int64_t)v;
    return 1;
}

int64_t tarn_str_parse_float(TarnStr *s, double *out) {
    char buf[128], *end;
    *out = 0;
    if (!str_to_cbuf(s, buf, sizeof buf)) return 0;
    double v = strtod(buf, &end);
    if (*end != 0) return 0;
    *out = v;
    return 1;
}

double tarn_str_to_float(TarnStr *s, const char *file, int line, int col) {
    char buf[128], *end;
    if (!str_to_cbuf(s, buf, sizeof buf)) bad_number(s, "Float", file, line, col);
    double v = strtod(buf, &end);
    if (*end != 0) bad_number(s, "Float", file, line, col);
    return v;
}

// ---------------------------------------------------------------- string utilities

static int is_char_boundary(TarnStr *s, int64_t i) {
    return i == 0 || i == s->len || (i > 0 && i < s->len && ((unsigned char)s->data[i] & 0xC0) != 0x80);
}

TarnStr *tarn_str_substring(TarnStr *s, int64_t start, int64_t end, const char *file, int line, int col) {
    char buf[200];
    if (start < 0 || end < start || end > s->len) {
        snprintf(buf, sizeof buf, "substring(%lld, %lld) out of range for a string of length %lld", (long long)start, (long long)end, (long long)s->len);
        tarn_die(file, line, col, buf);
    }
    if (!is_char_boundary(s, start) || !is_char_boundary(s, end)) {
        snprintf(buf, sizeof buf, "substring(%lld, %lld) would split a multi-byte character", (long long)start, (long long)end);
        tarn_die(file, line, col, buf);
    }
    if (start == 0 && end == s->len) return str_dup(s);
    return str_from(s->data + start, end - start);
}

static int64_t find_bytes(TarnStr *h, TarnStr *n) {
    if (n->len == 0) return 0;
    for (int64_t i = 0; i + n->len <= h->len; i++)
        if (h->data[i] == n->data[0] && memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    return -1;
}

int64_t tarn_str_contains(TarnStr *h, TarnStr *n) { return find_bytes(h, n) >= 0; }

int64_t tarn_str_starts_with(TarnStr *s, TarnStr *p) {
    return p->len <= s->len && memcmp(s->data, p->data, (size_t)p->len) == 0;
}

int64_t tarn_str_ends_with(TarnStr *s, TarnStr *p) {
    return p->len <= s->len && memcmp(s->data + s->len - p->len, p->data, (size_t)p->len) == 0;
}

TarnList *tarn_list_new(int64_t elem_size, int64_t cap);
void *tarn_list_push_slot(TarnList *l, int64_t elem_size);

// Each UTF-8 character as its own string.
// The 128 one-byte ASCII strings, immortal: `for c in s` and `chars()` on
// ASCII text allocate nothing per character.
typedef struct {
    int64_t rc;
    int64_t len;
    int64_t cap;
    char data[8];
} AsciiChar;
static AsciiChar ascii_chars[128];

static void init_ascii_chars(void) {
    for (int i = 0; i < 128; i++) ascii_chars[i] = (AsciiChar){RC_IMMORTAL, 1, 1, {(char)i}};
}

// The length in bytes of the character starting at byte `i` (1 for a stray byte).
static int64_t char_len_at(TarnStr *s, int64_t i) {
    unsigned char c = (unsigned char)s->data[i];
    int64_t n = c < 0x80 ? 1 : (c >> 5) == 0x6 ? 2 : (c >> 4) == 0xE ? 3 : (c >> 3) == 0x1E ? 4 : 1;
    return i + n > s->len ? s->len - i : n;
}

// The character starting at byte `i`, as a string.
TarnStr *tarn_str_char_at(TarnStr *s, int64_t i) {
    unsigned char c = (unsigned char)s->data[i];
    if (c < 0x80) return (TarnStr *)&ascii_chars[c];
    return str_from(s->data + i, char_len_at(s, i));
}

TarnList *tarn_str_chars(TarnStr *s) {
    TarnList *l = tarn_list_new((int64_t)sizeof(TarnStr *), s->len);
    int64_t i = 0;
    while (i < s->len) {
        TarnStr *ch = tarn_str_char_at(s, i);
        TarnStr **slot = tarn_list_push_slot(l, (int64_t)sizeof(TarnStr *));
        *slot = ch;
        i += ch->len;
    }
    return l;
}

// ---------------------------------------------------------------- clocks (for the time module)

int64_t tarn_time_realtime_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

int64_t tarn_time_monotonic_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int signal_any_pending(void);
static atomic_uint signal_count;

// Sleeps the whole time, unless a caught signal arrives (see the signal
// module). In a task, only the task sleeps: its thread runs other tasks.
void tarn_time_sleep_ns(int64_t ns) {
    unsigned seen = atomic_load(&signal_count);
    if (signal_any_pending()) return;
    if (sched_on) {
        int64_t deadline = now_ns() + (ns > 0 ? ns : 0);
        while (now_ns() < deadline && atomic_load(&signal_count) == seen && !signal_any_pending()) wait_until(deadline, -1, 0);
        return;
    }
    fflush(stdout);
    struct timespec req = {(time_t)(ns / 1000000000), (long)(ns % 1000000000)}, rem;
    while (nanosleep(&req, &rem) != 0 && errno == EINTR) {
        if (signal_any_pending()) return;
        req = rem;
    }
}

// ---------------------------------------------------------------- printing

void tarn_print_int(int64_t v) { printf("%lld\n", (long long)v); }

void tarn_print_float(double d) {
    char buf[64];
    format_float(d, buf, sizeof buf);
    puts(buf);
}

void tarn_print_bool(int64_t b) { puts(b ? "true" : "false"); }

// (One line at a time: tasks on other threads print too.)
void tarn_print_str(TarnStr *s) {
    flockfile(stdout);
    fwrite(s->data, 1, (size_t)s->len, stdout);
    putc_unlocked('\n', stdout);
    funlockfile(stdout);
}

// ---------------------------------------------------------------- lists
// A list header is a small object; its element buffer is malloc'd so it can grow.

TarnList *tarn_list_new(int64_t elem_size, int64_t cap) {
    TarnList *l = tarn_alloc((int64_t)sizeof(TarnList));
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
void tarn_list_free(void *p) {
    TarnList *l = p;
    free(l->data);
    tarn_free(l, (int64_t)sizeof(TarnList));
}

void *tarn_list_push_slot(TarnList *l, int64_t elem_size) {
    if (l->len == l->cap) {
        int64_t ncap = l->cap < 4 ? 4 : l->cap * 2;
        char *nd = realloc(l->data, (size_t)(ncap * elem_size));
        if (!nd) oom();
        l->data = nd;
        l->cap = ncap;
    }
    return l->data + (l->len++) * elem_size;
}

void *tarn_list_pop_slot(TarnList *l, int64_t elem_size, const char *file, int line, int col) {
    if (l->len == 0) tarn_die(file, line, col, "pop from empty list");
    l->len--;
    return l->data + l->len * elem_size;
}

// ---------------------------------------------------------------- list utilities

void tarn_list_insert_slot_check(TarnList *l, int64_t i, const char *file, int line, int col) {
    if (i < 0 || i > l->len) {
        char buf[128];
        snprintf(buf, sizeof buf, "insert index %lld out of bounds for list of length %lld", (long long)i, (long long)l->len);
        tarn_die(file, line, col, buf);
    }
}

// Room for one element at index `i` (already checked), moving the rest up.
void *tarn_list_insert_slot(TarnList *l, int64_t i, int64_t elem_size) {
    tarn_list_push_slot(l, elem_size);
    char *at = l->data + i * elem_size;
    memmove(at + elem_size, at, (size_t)((l->len - 1 - i) * elem_size));
    return at;
}

// Takes element `i` out (into `out`), moving the rest down.
void tarn_list_remove_at(TarnList *l, int64_t i, int64_t elem_size, void *out, const char *file, int line, int col) {
    if (i < 0 || i >= l->len) {
        char buf[128];
        snprintf(buf, sizeof buf, "index %lld out of bounds for list of length %lld", (long long)i, (long long)l->len);
        tarn_die(file, line, col, buf);
    }
    char *at = l->data + i * elem_size;
    memcpy(out, at, (size_t)elem_size);
    memmove(at, at + elem_size, (size_t)((l->len - 1 - i) * elem_size));
    l->len--;
}

void tarn_list_reverse(TarnList *l, int64_t elem_size) {
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
int64_t tarn_str_cmp(TarnStr *a, TarnStr *b);
static int cmp_str(const void *a, const void *b) { return (int)tarn_str_cmp(*(TarnStr *const *)a, *(TarnStr *const *)b); }

// Sort in place: kind 0 Int, 1 Float, 2 String. (qsort isn't stable, but
// equal Ints, Floats and Strings are indistinguishable.)
void tarn_list_sort(TarnList *l, int64_t kind) {
    if (l->len < 2) return;
    qsort(l->data, (size_t)l->len, 8, kind == 0 ? cmp_int : kind == 1 ? cmp_float : cmp_str);
}

// `xs.sort_by(key)`: sort `l` in place by the matching `keys` (computed
// once per element by the caller; `kind` as for `sort`), keeping equal keys
// in their order. A bottom-up merge sort of positions, then the elements
// (`size` bytes each) move to their places; no counts change.
void tarn_list_sort_by(TarnList *l, TarnList *keys, int64_t kind, int64_t size) {
    int64_t n = l->len;
    if (n < 2) return;
    int (*cmp)(const void *, const void *) = kind == 0 ? cmp_int : kind == 1 ? cmp_float : cmp_str;
    const char *k = keys->data;
    int64_t *pos = malloc((size_t)n * sizeof(int64_t)), *tmp = malloc((size_t)n * sizeof(int64_t));
    char *moved = malloc((size_t)(n * size));
    if (!pos || !tmp || !moved) oom();
    for (int64_t i = 0; i < n; i++) pos[i] = i;
    for (int64_t width = 1; width < n; width *= 2) {
        for (int64_t lo = 0; lo < n; lo += 2 * width) {
            int64_t mid = lo + width < n ? lo + width : n, hi = lo + 2 * width < n ? lo + 2 * width : n;
            int64_t a = lo, b = mid, o = lo;
            // Take from the right half only when it's strictly smaller: stable.
            while (a < mid && b < hi) tmp[o++] = cmp(k + pos[b] * 8, k + pos[a] * 8) < 0 ? pos[b++] : pos[a++];
            while (a < mid) tmp[o++] = pos[a++];
            while (b < hi) tmp[o++] = pos[b++];
        }
        int64_t *t = pos;
        pos = tmp;
        tmp = t;
    }
    for (int64_t i = 0; i < n; i++) memcpy(moved + i * size, l->data + pos[i] * size, (size_t)size);
    memcpy(l->data, moved, (size_t)(n * size));
    free(pos);
    free(tmp);
    free(moved);
}

// ---------------------------------------------------------------- more string utilities

TarnStr *tarn_str_list_join(TarnList *l, TarnStr *sep) {
    TarnStr **parts = (TarnStr **)l->data;
    if (l->len == 1) return str_dup(parts[0]);
    int64_t n = 0;
    for (int64_t i = 0; i < l->len; i++) n += parts[i]->len + (i > 0 ? sep->len : 0);
    TarnStr *s = str_new(n);
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

static void list_push_str(TarnList *l, const char *data, int64_t len) {
    TarnStr **slot = tarn_list_push_slot(l, (int64_t)sizeof(TarnStr *));
    *slot = str_from(data, len);
}

static int64_t find_from(TarnStr *h, TarnStr *n, int64_t from) {
    if (n->len == 0) return from;
    for (int64_t i = from; i + n->len <= h->len; i++)
        if (h->data[i] == n->data[0] && memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    return -1;
}

TarnList *tarn_str_split(TarnStr *s, TarnStr *sep, const char *file, int line, int col) {
    if (sep->len == 0) tarn_die(file, line, col, "split() with an empty separator; use chars() to split into characters");
    TarnList *l = tarn_list_new((int64_t)sizeof(TarnStr *), 0);
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
TarnList *tarn_str_split_whitespace(TarnStr *s) {
    TarnList *l = tarn_list_new((int64_t)sizeof(TarnStr *), 0);
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
TarnList *tarn_str_lines(TarnStr *s) {
    TarnList *l = tarn_list_new((int64_t)sizeof(TarnStr *), 0);
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

static TarnStr *str_slice(TarnStr *s, int64_t a, int64_t b) {
    if (a == 0 && b == s->len) return str_dup(s);
    return str_from(s->data + a, b - a);
}

TarnStr *tarn_str_trim(TarnStr *s, int64_t start, int64_t end) {
    int64_t a = 0, b = s->len;
    if (start)
        while (a < b && is_space((unsigned char)s->data[a])) a++;
    if (end)
        while (b > a && is_space((unsigned char)s->data[b - 1])) b--;
    return str_slice(s, a, b);
}

// ASCII letters only; other characters are left alone.
TarnStr *tarn_str_case(TarnStr *s, int64_t upper) {
    int64_t i = 0;
    for (; i < s->len; i++) {
        char c = s->data[i];
        if (upper ? (c >= 'a' && c <= 'z') : (c >= 'A' && c <= 'Z')) break;
    }
    if (i == s->len) return str_dup(s);
    TarnStr *r = str_from(s->data, s->len);
    for (; i < r->len; i++) {
        char c = r->data[i];
        if (upper && c >= 'a' && c <= 'z') r->data[i] = (char)(c - 32);
        if (!upper && c >= 'A' && c <= 'Z') r->data[i] = (char)(c + 32);
    }
    return r;
}

TarnStr *tarn_str_replace(TarnStr *s, TarnStr *from, TarnStr *to, const char *file, int line, int col) {
    if (from->len == 0) tarn_die(file, line, col, "replace() with an empty string to find");
    int64_t count = 0;
    for (int64_t at = find_from(s, from, 0); at >= 0; at = find_from(s, from, at + from->len)) count++;
    if (count == 0) return str_dup(s);
    TarnStr *r = str_new(s->len + count * (to->len - from->len));
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

// The string as it's written in Tarn code, quotes and escapes included.
TarnStr *tarn_str_repr(TarnStr *s) {
    int64_t n = 2;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        n += (c == '"' || c == '\\' || c == '\n' || c == '\t' || c == '\r' || c == 0) ? 2 : 1;
    }
    TarnStr *r = str_new(n);
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
int64_t tarn_str_find(TarnStr *h, TarnStr *n) { return find_from(h, n, 0); }

// Byte index of the last `n` in `h`, or -1.
int64_t tarn_str_rfind(TarnStr *h, TarnStr *n) {
    for (int64_t i = h->len - n->len; i >= 0; i--)
        if (memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    return -1;
}

// Non-overlapping occurrences (an empty string: one more than the length, like Python).
int64_t tarn_str_count(TarnStr *h, TarnStr *n) {
    if (n->len == 0) return h->len + 1;
    int64_t c = 0, at = find_from(h, n, 0);
    while (at >= 0) {
        c++;
        at = find_from(h, n, at + n->len);
    }
    return c;
}

TarnStr *tarn_str_repeat(TarnStr *s, int64_t n, const char *file, int line, int col) {
    if (n < 0) {
        char buf[96];
        snprintf(buf, sizeof buf, "repeat(%lld): the count can't be negative", (long long)n);
        tarn_die(file, line, col, buf);
    }
    if (n == 1) return str_dup(s);
    if (s->len > 0 && n > ((int64_t)1 << 40) / s->len) tarn_die(file, line, col, "repeat(): the result would be too large");
    TarnStr *r = str_new(s->len * n);
    for (int64_t i = 0; i < n; i++) memcpy(r->data + i * s->len, s->data, (size_t)s->len);
    return r;
}

static int64_t utf8_count(TarnStr *s) {
    int64_t n = 0;
    for (int64_t i = 0; i < s->len; i++) n += ((unsigned char)s->data[i] & 0xC0) != 0x80;
    return n;
}

int64_t tarn_str_char_count(TarnStr *s) { return utf8_count(s); }

// Pad to `width` characters with copies of `fill` (one character) on the left or right.
TarnStr *tarn_str_pad(TarnStr *s, int64_t width, TarnStr *fill, int64_t at_start, const char *file, int line, int col) {
    if (utf8_count(fill) != 1) tarn_die(file, line, col, "pad_start()/pad_end(): the fill must be exactly one character");
    int64_t have = utf8_count(s);
    if (have >= width) return str_dup(s);
    int64_t k = width - have;
    TarnStr *r = str_new(s->len + k * fill->len);
    char *p = r->data;
    if (!at_start) {
        memcpy(p, s->data, (size_t)s->len);
        p += s->len;
    }
    for (int64_t i = 0; i < k; i++, p += fill->len) memcpy(p, fill->data, (size_t)fill->len);
    if (at_start) memcpy(p, s->data, (size_t)s->len);
    return r;
}

// ---------------------------------------------------------------- UTF-8 and URLs

// Is `p[0..n)` valid UTF-8 (no overlong forms, surrogates or code points past U+10FFFF)?
static int utf8_valid(const unsigned char *p, int64_t n) {
    int64_t i = 0;
    while (i < n) {
        unsigned char c = p[i];
        if (c < 0x80) {
            i++;
            continue;
        }
        int64_t k;
        uint32_t cp;
        if ((c & 0xE0) == 0xC0) k = 1, cp = c & 0x1F;
        else if ((c & 0xF0) == 0xE0) k = 2, cp = c & 0x0F;
        else if ((c & 0xF8) == 0xF0) k = 3, cp = c & 0x07;
        else return 0;
        if (i + k >= n) return 0; // cut short
        for (int64_t j = 1; j <= k; j++) {
            if ((p[i + j] & 0xC0) != 0x80) return 0;
            cp = (cp << 6) | (p[i + j] & 0x3F);
        }
        if ((k == 1 && cp < 0x80) || (k == 2 && cp < 0x800) || (k == 3 && cp < 0x10000)) return 0; // overlong
        if (cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) return 0;
        i += k + 1;
    }
    return 1;
}

// Percent-encoding (RFC 3986): every byte but the unreserved characters
// (letters, digits, `-._~`) and those in `keep` becomes `%XX`.
TarnStr *tarn_rt_url_encode(TarnStr *s, TarnStr *keep) {
    static const char hex[] = "0123456789ABCDEF";
    int64_t n = 0;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        int plain = (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '.' || c == '_' || c == '~' ||
                    (c < 0x80 && memchr(keep->data, c, (size_t)keep->len));
        n += plain ? 1 : 3;
    }
    if (n == s->len) return str_dup(s);
    TarnStr *r = str_new(n);
    char *o = r->data;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        int plain = (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '.' || c == '_' || c == '~' ||
                    (c < 0x80 && memchr(keep->data, c, (size_t)keep->len));
        if (plain) {
            *o++ = (char)c;
        } else {
            *o++ = '%';
            *o++ = hex[c >> 4];
            *o++ = hex[c & 15];
        }
    }
    return r;
}

static int hex_digit(unsigned char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

// `%XX` back to bytes (and `+` to a space, for form data): NULL if a `%` isn't
// followed by two hex digits, or the result isn't valid UTF-8.
TarnStr *tarn_rt_url_decode(TarnStr *s, int64_t plus) {
    if (!memchr(s->data, '%', (size_t)s->len) && !(plus && memchr(s->data, '+', (size_t)s->len))) return str_dup(s);
    TarnStr *r = str_new(s->len);
    int64_t n = 0;
    for (int64_t i = 0; i < s->len; i++) {
        unsigned char c = (unsigned char)s->data[i];
        if (c == '%') {
            int hi = i + 2 < s->len ? hex_digit((unsigned char)s->data[i + 1]) : -1;
            int lo = i + 2 < s->len ? hex_digit((unsigned char)s->data[i + 2]) : -1;
            if (hi < 0 || lo < 0) {
                tarn_str_free(r);
                return NULL;
            }
            r->data[n++] = (char)(hi * 16 + lo);
            i += 2;
        } else {
            r->data[n++] = plus && c == '+' ? ' ' : (char)c;
        }
    }
    r->len = n;
    if (!utf8_valid((unsigned char *)r->data, n)) {
        tarn_str_free(r);
        return NULL;
    }
    return r;
}

// ---------------------------------------------------------------- bytes
// `Bytes` share `TarnStr`'s layout (and its concatenation, search and
// splitting): only the methods that differ are here.

TarnStr *tarn_rt_bytes_new(void) { return str_alloc(0); }

TarnStr *tarn_rt_bytes_from_list(TarnList *l, const char *file, int line, int col) {
    TarnStr *b = str_new(l->len);
    int64_t *v = (int64_t *)l->data;
    for (int64_t i = 0; i < l->len; i++) {
        if (v[i] < 0 || v[i] > 255) {
            char msg[96];
            snprintf(msg, sizeof msg, "Bytes([...]): %lld at index %lld is not a byte (0 to 255)", (long long)v[i], (long long)i);
            tarn_die(file, line, col, msg);
        }
        b->data[i] = (char)v[i];
    }
    return b;
}

int64_t tarn_rt_bytes_get(TarnStr *b, int64_t i, const char *file, int line, int col) {
    if (i < 0 || i >= b->len) {
        char msg[96];
        snprintf(msg, sizeof msg, "index %lld out of bounds for %lld bytes", (long long)i, (long long)b->len);
        tarn_die(file, line, col, msg);
    }
    return (unsigned char)b->data[i];
}

int64_t tarn_rt_bytes_at(TarnStr *b, int64_t i) { return (unsigned char)b->data[i]; }

TarnStr *tarn_rt_bytes_slice(TarnStr *b, int64_t start, int64_t end, const char *file, int line, int col) {
    if (start < 0 || end > b->len || start > end) {
        char msg[128];
        snprintf(msg, sizeof msg, "slice(%lld, %lld) out of range for %lld bytes", (long long)start, (long long)end, (long long)b->len);
        tarn_die(file, line, col, msg);
    }
    if (start == 0 && end == b->len) return str_dup(b);
    return str_from(b->data + start, end - start);
}

// Valid UTF-8 becomes a `String` without copying: the same object, shared.
TarnStr *tarn_rt_bytes_decode_or_null(TarnStr *b) { return utf8_valid((unsigned char *)b->data, b->len) ? str_dup(b) : NULL; }

TarnStr *tarn_rt_bytes_decode(TarnStr *b, const char *file, int line, int col) {
    if (!utf8_valid((unsigned char *)b->data, b->len)) tarn_die(file, line, col, "decode(): the bytes are not valid UTF-8 (use decode_or_null() or decode_lossy())");
    return str_dup(b);
}

// Invalid sequences become U+FFFD (the replacement character), one per bad byte.
TarnStr *tarn_rt_bytes_decode_lossy(TarnStr *b) {
    if (utf8_valid((unsigned char *)b->data, b->len)) return str_dup(b);
    TarnStr *r = str_alloc(b->len * 3);
    int64_t i = 0, n = 0;
    while (i < b->len) {
        int64_t k = char_len_at(b, i);
        if (k > 1 || (unsigned char)b->data[i] < 0x80) {
            if (utf8_valid((unsigned char *)b->data + i, k)) {
                memcpy(r->data + n, b->data + i, (size_t)k);
                n += k;
                i += k;
                continue;
            }
        }
        memcpy(r->data + n, "\xEF\xBF\xBD", 3);
        n += 3;
        i++;
    }
    r->len = n;
    return r;
}

TarnStr *tarn_rt_bytes_hex(TarnStr *b) {
    static const char hex[] = "0123456789abcdef";
    TarnStr *r = str_new(b->len * 2);
    for (int64_t i = 0; i < b->len; i++) {
        unsigned char c = (unsigned char)b->data[i];
        r->data[2 * i] = hex[c >> 4];
        r->data[2 * i + 1] = hex[c & 15];
    }
    return r;
}

TarnList *tarn_rt_bytes_to_list(TarnStr *b) {
    TarnList *l = tarn_list_new((int64_t)sizeof(int64_t), b->len);
    for (int64_t i = 0; i < b->len; i++) *(int64_t *)tarn_list_push_slot(l, (int64_t)sizeof(int64_t)) = (unsigned char)b->data[i];
    return l;
}

// `b"GET /\r\n\xff"`: printable ASCII as it is, the rest escaped.
TarnStr *tarn_rt_bytes_repr(TarnStr *b) {
    static const char hex[] = "0123456789abcdef";
    TarnStr *r = str_alloc(b->len * 4 + 3);
    char *o = r->data;
    *o++ = 'b';
    *o++ = '"';
    for (int64_t i = 0; i < b->len; i++) {
        unsigned char c = (unsigned char)b->data[i];
        switch (c) {
        case '\n': *o++ = '\\', *o++ = 'n'; break;
        case '\r': *o++ = '\\', *o++ = 'r'; break;
        case '\t': *o++ = '\\', *o++ = 't'; break;
        case '"': *o++ = '\\', *o++ = '"'; break;
        case '\\': *o++ = '\\', *o++ = '\\'; break;
        default:
            if (c >= 0x20 && c < 0x7f) {
                *o++ = (char)c;
            } else {
                *o++ = '\\', *o++ = 'x', *o++ = hex[c >> 4], *o++ = hex[c & 15];
            }
        }
    }
    *o++ = '"';
    r->len = o - r->data;
    return r;
}

// `s.encode()`: a `String` is already UTF-8 bytes, so it's the same object.
TarnStr *tarn_rt_str_encode(TarnStr *s) { return str_dup(s); }

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

int64_t tarn_hash_int(int64_t v) { return (int64_t)mix64((uint64_t)v); }

int64_t tarn_hash_float(double d) {
    if (d == 0) d = 0; // -0.0 == 0.0
    if (isnan(d)) d = NAN;
    uint64_t bits;
    memcpy(&bits, &d, 8);
    return (int64_t)mix64(bits);
}

int64_t tarn_hash_str(TarnStr *s) { return (int64_t)hash_bytes(s->data, s->len); }

// `hash(a, b)`: fold each part's hash into the running one.
int64_t tarn_hash_combine(int64_t h, int64_t part) { return (int64_t)mix64((uint64_t)h * 31 + (uint64_t)part); }

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
} TarnMapType;

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
} TarnMap;

#define SLOT_EMPTY (-1)
#define SLOT_REMOVED (-2)

// A used index slot holds the entry's number and the top bits of its hash, so
// a search can skip other keys without reading their entries (a cache miss each).
static inline int64_t slot_tag(uint64_t h, int64_t ix) { return (int64_t)((h >> 32) << 32 | (uint64_t)ix); }
static inline int64_t slot_ix(int64_t v) { return v & 0xffffffff; }
static inline int slot_matches(int64_t v, uint64_t h) { return ((uint64_t)v >> 32) == (h >> 32); }
#define MAP_MAX_ENTRIES ((int64_t)1 << 30)

static inline int64_t entry_hash(const char *e) { return *(const int64_t *)e; }

static uint64_t key_hash(const TarnMapType *t, int64_t ikey, void *pkey) {
    uint64_t h;
    switch (t->kind) {
    case 0: h = mix64((uint64_t)ikey); break;
    case 1: h = hash_bytes(((TarnStr *)pkey)->data, ((TarnStr *)pkey)->len); break;
    default: h = mix64((uint64_t)t->hash(pkey)); break;
    }
    return h & (uint64_t)INT64_MAX; // live entries have a hash >= 0
}

// Find `key` (with hash `h`): its entry number, or -1 with *slot set to where
// it would go. User `==` may run, and may even change the map; then the
// search starts over.
static int64_t map_lookup(TarnMap *m, const TarnMapType *t, uint64_t h, int64_t ikey, void *pkey, int64_t *slot) {
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
                    TarnStr *a = *(TarnStr **)(e + 8), *b = pkey;
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
static void map_rebuild(TarnMap *m, const TarnMapType *t, int64_t need) {
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

TarnMap *tarn_map_new(const TarnMapType *t, int64_t n) {
    TarnMap *m = tarn_alloc((int64_t)sizeof(TarnMap));
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

// A copy of the map's entries and index (for moving a value to another
// task while this one still uses it): the generated code then takes a
// reference to (and copies) each key and value. `stride`: bytes per entry.
TarnMap *tarn_map_clone(TarnMap *m, int64_t stride) {
    TarnMap *c = tarn_alloc((int64_t)sizeof(TarnMap));
    *c = *m;
    c->rc = 1;
    c->hint_ix = -1;
    c->hint_missing = 0;
    if (m->cap) {
        c->entries = malloc((size_t)(m->cap * stride));
        if (!c->entries) oom();
        memcpy(c->entries, m->entries, (size_t)(m->used * stride));
    }
    if (m->index) {
        c->index = malloc((size_t)(m->mask + 1) * sizeof(int64_t));
        if (!c->index) oom();
        memcpy(c->index, m->index, (size_t)(m->mask + 1) * sizeof(int64_t));
    }
    return c;
}

// Frees the map (its keys and values were already released).
void tarn_map_free(void *p) {
    TarnMap *m = p;
    free(m->entries);
    free(m->index);
    tarn_free(m, (int64_t)sizeof(TarnMap));
}

// `clear()`, after the keys and values were released.
void tarn_map_reset(TarnMap *m) {
    m->len = m->used = 0;
    m->hint_ix = -1;
    m->hint_missing = 0;
    if (m->index) memset(m->index, 0xff, (size_t)(m->mask + 1) * sizeof(int64_t));
}

// Is the live entry `e` the one for this key? (User `==` may run.)
static int entry_is(TarnMap *m, const TarnMapType *t, char *e, int64_t ikey, void *pkey) {
    if (entry_hash(e) < 0) return 0;
    if (t->kind == 0) return *(int64_t *)(e + 8) == ikey;
    if (t->kind == 1) {
        TarnStr *a = *(TarnStr **)(e + 8), *b = pkey;
        return a == b || (a->len == b->len && memcmp(a->data, b->data, (size_t)a->len) == 0);
    }
    char *ents = m->entries;
    int r = t->eq(*(void **)(e + 8), pkey) != 0;
    return r && m->entries == ents && entry_hash(e) >= 0;
}

static void *map_find(TarnMap *m, const TarnMapType *t, int64_t ikey, void *pkey) {
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

void *tarn_map_find_i(TarnMap *m, const TarnMapType *t, int64_t key) { return map_find(m, t, key, NULL); }
void *tarn_map_find_p(TarnMap *m, const TarnMapType *t, void *key) { return map_find(m, t, 0, key); }

// The entry for `key`, adding it (with the key stored, the value not yet)
// when it's new: then *is_new is 1.
static void *map_upsert(TarnMap *m, const TarnMapType *t, int64_t ikey, void *pkey, int64_t *is_new) {
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

void *tarn_map_upsert_i(TarnMap *m, const TarnMapType *t, int64_t key, int64_t *is_new) { return map_upsert(m, t, key, NULL, is_new); }
void *tarn_map_upsert_p(TarnMap *m, const TarnMapType *t, void *key, int64_t *is_new) { return map_upsert(m, t, 0, key, is_new); }

// Unlink `key`'s entry and return it (its key and value are the caller's now), or NULL.
static void *map_remove(TarnMap *m, const TarnMapType *t, int64_t ikey, void *pkey) {
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

void *tarn_map_remove_i(TarnMap *m, const TarnMapType *t, int64_t key) { return map_remove(m, t, key, NULL); }
void *tarn_map_remove_p(TarnMap *m, const TarnMapType *t, void *key) { return map_remove(m, t, 0, key); }

// The number of the first live entry at or after `i`, or -1. `len` is the
// size the map had when the loop started.
int64_t tarn_map_next(TarnMap *m, const TarnMapType *t, int64_t i, int64_t len, const char *file, int line, int col) {
    if (m->len != len) tarn_die(file, line, col, "map changed size during iteration");
    for (; i < m->used; i++)
        if (entry_hash(m->entries + i * t->stride) >= 0) return i;
    return -1;
}

// `m[k]` for a missing key. `shown` is the key as text (or NULL), quoted if a String.
void tarn_panic_key(TarnStr *shown, int64_t quoted, const char *file, int line, int col) {
    char buf[200];
    if (!shown)
        snprintf(buf, sizeof buf, "key not found in map");
    else if (quoted)
        snprintf(buf, sizeof buf, "key \"%.*s\" not found in map", shown->len > 64 ? 64 : (int)shown->len, shown->data);
    else
        snprintf(buf, sizeof buf, "key %.*s not found in map", shown->len > 64 ? 64 : (int)shown->len, shown->data);
    tarn_die(file, line, col, buf);
}

// ---------------------------------------------------------------- files (for the fs module)


static void fs_fail(const char *what, TarnStr *path) {
    Task *t = cur_task();
    snprintf(t->fs_error, sizeof t->fs_error, "%s %.*s: %s", what, path->len > 300 ? 300 : (int)path->len, path->data, strerror(errno));
}

// A NUL-terminated copy of a Tarn string (paths can't contain NUL).
static char *cpath(TarnStr *s) {
    char *p = malloc((size_t)s->len + 1);
    if (!p) oom();
    memcpy(p, s->data, (size_t)s->len);
    p[s->len] = 0;
    return p;
}

static TarnStr *read_all(FILE *f) {
    TarnStr *s = str_alloc(4096 - (int64_t)sizeof(TarnStr));
    for (;;) {
        if (s->len == s->cap) {
            TarnStr *g = str_alloc(s->cap * 2);
            g->len = s->len;
            memcpy(g->data, s->data, (size_t)s->len);
            tarn_str_free(s);
            s = g;
        }
        size_t n = fread(s->data + s->len, 1, (size_t)(s->cap - s->len), f);
        if (n == 0) break;
        s->len += (int64_t)n;
    }
    return s;
}

TarnStr *tarn_fs_read(TarnStr *path) {
    char *p = cpath(path);
    FILE *f = fopen(p, "rb");
    free(p);
    if (!f) {
        fs_fail("cannot read", path);
        return NULL;
    }
    TarnStr *s = read_all(f);
    int bad = ferror(f);
    fclose(f);
    if (bad) {
        fs_fail("cannot read", path);
        tarn_str_free(s);
        return NULL;
    }
    return s;
}

int64_t tarn_fs_write(TarnStr *path, TarnStr *text, int64_t append) {
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
int64_t tarn_fs_kind(TarnStr *path) {
    char *p = cpath(path);
    struct stat st;
    int r = stat(p, &st);
    free(p);
    if (r != 0) return 0;
    return S_ISREG(st.st_mode) ? 1 : S_ISDIR(st.st_mode) ? 2 : 3;
}

static int cmp_names(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

// The names in a directory (without "." and ".."), sorted, or NULL.
TarnList *tarn_fs_list_dir(TarnStr *path) {
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
    TarnList *l = tarn_list_new((int64_t)sizeof(TarnStr *), n);
    for (int64_t i = 0; i < n; i++) {
        list_push_str(l, names[i], (int64_t)strlen(names[i]));
        free(names[i]);
    }
    free(names);
    return l;
}

// Like `mkdir -p`: creates missing parents; an existing directory is fine.
int64_t tarn_fs_make_dir(TarnStr *path) {
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
    int ok = mkdir(p, 0777) == 0 || (errno == EEXIST && tarn_fs_kind(path) == 2);
    free(p);
    if (!ok) fs_fail("cannot create directory", path);
    return ok;
}

// A file, or an empty directory.
int64_t tarn_fs_remove(TarnStr *path) {
    char *p = cpath(path);
    int ok = remove(p) == 0;
    free(p);
    if (!ok) fs_fail("cannot remove", path);
    return ok;
}

int64_t tarn_fs_rename(TarnStr *from, TarnStr *to) {
    char *a = cpath(from), *b = cpath(to);
    int ok = rename(a, b) == 0;
    free(a);
    free(b);
    if (!ok) fs_fail("cannot rename", from);
    return ok;
}

TarnStr *tarn_fs_last_error(void) {
    Task *t = cur_task();
    return str_from(t->fs_error, (int64_t)strlen(t->fs_error));
}

TarnStr *tarn_fs_cwd(void) {
    char buf[4096];
    if (!getcwd(buf, sizeof buf)) return str_new(0);
    return str_from(buf, (int64_t)strlen(buf));
}

TarnStr *tarn_fs_read_stdin(void) {
    fflush(stdout);
    return read_all(stdin);
}

// ---------------------------------------------------------------- entry

static int saved_argc;
static char **saved_argv;

// Command-line arguments after the program name, as List[String].
TarnList *tarn_args(void) {
    TarnList *l = tarn_list_new((int64_t)sizeof(TarnStr *), saved_argc > 1 ? saved_argc - 1 : 0);
    for (int i = 1; i < saved_argc; i++) {
        TarnStr **slot = tarn_list_push_slot(l, (int64_t)sizeof(TarnStr *));
        *slot = str_from(saved_argv[i], (int64_t)strlen(saved_argv[i]));
    }
    return l;
}

// `TARN_ARC_STATS=1` (set by `tarn run --arc-stats`) prints memory counters at
// exit; `TARN_ARC_CHECK=1` reports leaked objects (used by the test suite).
// ---------------------------------------------------------------- environment (the env module)

extern char **environ;

// A NUL-terminated copy, or NULL if the text itself contains a NUL byte.
static char *cstr_or_null(TarnStr *s) {
    if (memchr(s->data, 0, (size_t)s->len)) return NULL;
    return cpath(s);
}

TarnStr *tarn_rt_env_get(TarnStr *name) {
    char *n = cstr_or_null(name);
    if (!n) return NULL;
    const char *v = getenv(n);
    free(n);
    return v ? str_from(v, (int64_t)strlen(v)) : NULL;
}

// `name` must be non-empty, without `=` or NUL; `value` without NUL.
int64_t tarn_rt_env_set(TarnStr *name, TarnStr *value) {
    char *n = cstr_or_null(name), *v = cstr_or_null(value);
    int ok = n && v && name->len > 0 && !memchr(name->data, '=', (size_t)name->len) && setenv(n, v, 1) == 0;
    free(n);
    free(v);
    return ok;
}

int64_t tarn_rt_env_remove(TarnStr *name) {
    char *n = cstr_or_null(name);
    int ok = n && name->len > 0 && !memchr(name->data, '=', (size_t)name->len) && unsetenv(n) == 0;
    free(n);
    return ok;
}

// The folder of the program's main file, written into the program by the
// compiler ("" for a program that only existed in memory).
extern const char tarn_root_dir[];

TarnStr *tarn_rt_env_root_dir(void) { return str_from(tarn_root_dir, (int64_t)strlen(tarn_root_dir)); }

// True only the first time: the env module loads the default `.env` then
// (unless `env.load(path)` claimed it first).
int64_t tarn_rt_env_claim_default(void) {
    static int claimed;
    if (claimed) return 0;
    claimed = 1;
    return 1;
}

// Every variable as `NAME=value`, in the order the system keeps them.
TarnList *tarn_rt_env_all(void) {
    TarnList *l = tarn_list_new((int64_t)sizeof(TarnStr *), 0);
    for (char **e = environ; *e; e++) list_push_str(l, *e, (int64_t)strlen(*e));
    return l;
}

// ---------------------------------------------------------------- signals (the signal module)
// A caught signal only bumps a counter: the program asks for it later
// (`received`, `wait`). Handlers are installed without SA_RESTART, so a
// blocking call (`accept`, `read`, `sleep`) returns early when one arrives.

#include <signal.h>

// The `signal.Signal` variants, in order.
static const int signal_numbers[] = {SIGINT, SIGTERM, SIGHUP, SIGUSR1, SIGUSR2, SIGQUIT};
#define N_SIGNALS ((int)(sizeof signal_numbers / sizeof signal_numbers[0]))
static volatile sig_atomic_t signal_pending[N_SIGNALS];
static int signal_caught[N_SIGNALS];

static int signal_any_pending(void) {
    for (int i = 0; i < N_SIGNALS; i++)
        if (signal_pending[i] > 0) return 1;
    return 0;
}

// How many caught signals have arrived: a socket wait that sees it change
// was interrupted, even if another task has taken the signal since.
static atomic_uint signal_count;

static void on_signal(int s) {
    int saved = errno;
    atomic_fetch_add(&signal_count, 1);
    for (int i = 0; i < N_SIGNALS; i++)
        if (signal_numbers[i] == s) signal_pending[i] = signal_pending[i] + 1;
    // Tasks waiting for time or I/O look for it as soon as the poller wakes up.
    if (sched_on) wake_fd_write();
    errno = saved;
}

static int any_signal_caught(void) {
    for (int i = 0; i < N_SIGNALS; i++)
        if (signal_caught[i]) return 1;
    return 0;
}

static int sigisemptyset_portable(const sigset_t *set) {
    for (int i = 0; i < N_SIGNALS; i++)
        if (sigismember(set, signal_numbers[i])) return 0;
    return 1;
}

static void signal_mask(sigset_t *set) {
    sigemptyset(set);
    for (int i = 0; i < N_SIGNALS; i++)
        if (signal_caught[i]) sigaddset(set, signal_numbers[i]);
}

void tarn_rt_signal_catch(int64_t which) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigemptyset(&sa.sa_mask);
    sigaction(signal_numbers[which], &sa, NULL);
    signal_caught[which] = 1;
}

void tarn_rt_signal_reset(int64_t which) {
    signal(signal_numbers[which], SIG_DFL);
    signal_caught[which] = 0;
    signal_pending[which] = 0;
}

// The first caught signal that arrived and hasn't been received yet, or -1.
int64_t tarn_rt_signal_next(void) {
    sigset_t caught, old;
    signal_mask(&caught);
    sigprocmask(SIG_BLOCK, &caught, &old);
    int64_t got = -1;
    for (int i = 0; i < N_SIGNALS && got < 0; i++) {
        if (signal_pending[i] > 0) {
            signal_pending[i] = signal_pending[i] - 1;
            got = i;
        }
    }
    sigprocmask(SIG_SETMASK, &old, NULL);
    return got;
}

// Waits for a caught signal (-1 if none is caught: it would wait forever).
int64_t tarn_rt_signal_wait(void) {
    sigset_t caught, old;
    signal_mask(&caught);
    if (sigisemptyset_portable(&caught)) return -1;
    if (sched_on) {
        // A task waits without holding up its thread (woken by the poller).
        for (;;) {
            int64_t got = tarn_rt_signal_next();
            if (got >= 0) return got;
            wait_until(now_ns() + 1000000000, -1, 0);
        }
    }
    sigprocmask(SIG_BLOCK, &caught, &old);
    int64_t got = -1;
    while (got < 0) {
        for (int i = 0; i < N_SIGNALS && got < 0; i++) {
            if (signal_pending[i] > 0) {
                signal_pending[i] = signal_pending[i] - 1;
                got = i;
            }
        }
        if (got < 0) sigsuspend(&old);
    }
    sigprocmask(SIG_SETMASK, &old, NULL);
    return got;
}

// Sends `which` to this program (for tests, and for a program to stop itself the usual way).
void tarn_rt_signal_send(int64_t which) { raise(signal_numbers[which]); }

// ---------------------------------------------------------------- TCP sockets (the net module)
// Functions return -1 (or NULL) on failure and leave the reason in
// `net_error`: an errno value, or NET_EAI_BASE + a getaddrinfo error.

#include <arpa/inet.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/socket.h>

#define NET_EAI_BASE 100000
// (Per task: the task that failed reads it later.)
#define net_error (cur_task()->net_error)

// Sockets never block their thread: when one isn't ready, the task waits
// for it (see `net_wait`). Each one's timeout (`set_timeout`), by fd.
static int64_t *fd_timeouts;
static int64_t n_fd_timeouts;
static pthread_mutex_t fd_timeouts_lock = PTHREAD_MUTEX_INITIALIZER;

static void set_fd_timeout(int fd, int64_t ns) {
    pthread_mutex_lock(&fd_timeouts_lock);
    if (fd >= n_fd_timeouts) {
        int64_t n = n_fd_timeouts ? n_fd_timeouts : 64;
        while (n <= fd) n *= 2;
        fd_timeouts = realloc(fd_timeouts, (size_t)n * sizeof(int64_t));
        if (!fd_timeouts) oom();
        memset(fd_timeouts + n_fd_timeouts, 0, (size_t)(n - n_fd_timeouts) * sizeof(int64_t));
        n_fd_timeouts = n;
    }
    fd_timeouts[fd] = ns;
    pthread_mutex_unlock(&fd_timeouts_lock);
}

static int64_t fd_deadline(int fd) {
    pthread_mutex_lock(&fd_timeouts_lock);
    int64_t ns = fd < n_fd_timeouts ? fd_timeouts[fd] : 0;
    pthread_mutex_unlock(&fd_timeouts_lock);
    return ns > 0 ? now_ns() + ns : 0;
}

static void set_nonblocking(int fd) { fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK); }

static int net_interrupted(void);

// `fd` wasn't ready: wait until it may be (1), until `deadline` (0, with
// net_error EAGAIN: timed out), or for a caught signal (-1, EINTR).
static int net_wait(int fd, int writable, int64_t deadline) {
    unsigned seen = atomic_load(&signal_count);
    if (net_interrupted()) return -1;
    if (deadline && now_ns() >= deadline) {
        net_error = EAGAIN;
        return 0;
    }
    if (sched_on) {
        wait_until(deadline, fd, writable);
        if (atomic_load(&signal_count) != seen) {
            net_error = EINTR;
            return -1;
        }
        return net_interrupted() ? -1 : 1;
    }
    struct pollfd p = {fd, (short)(writable ? POLLOUT : POLLIN), 0};
    int ms = deadline ? (int)((deadline - now_ns() + 999999) / 1000000) : -1;
    if (poll(&p, 1, ms) == 0) {
        net_error = EAGAIN;
        return 0;
    }
    return net_interrupted() ? -1 : 1;
}

int64_t tarn_rt_net_last_error(void) { return net_error; }

// A fresh socket: closed on exec, and (on macOS) without SIGPIPE when the peer is gone.
static int net_socket(int family) {
    int fd = socket(family, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    fcntl(fd, F_SETFD, FD_CLOEXEC);
    set_nonblocking(fd);
    set_fd_timeout(fd, 0);
#ifdef SO_NOSIGPIPE
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
#endif
    return fd;
}

static struct addrinfo *net_resolve(TarnStr *host, int64_t port, int passive) {
    if (port < 0 || port > 65535) {
        net_error = EINVAL;
        return NULL;
    }
    char *h = cstr_or_null(host);
    if (!h) {
        net_error = EINVAL;
        return NULL;
    }
    char service[16];
    snprintf(service, sizeof service, "%lld", (long long)port);
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_flags = passive ? AI_PASSIVE : 0;
    int rc = getaddrinfo(host->len ? h : NULL, service, &hints, &res);
    free(h);
    if (rc != 0) {
        net_error = rc == EAI_SYSTEM ? errno : NET_EAI_BASE + (rc < 0 ? -rc : rc);
        return NULL;
    }
    return res;
}

// Listen on `host` (an address like "127.0.0.1", "0.0.0.0" or "::", a name,
// or "" for every address) and `port` (0: any free port).
int64_t tarn_rt_net_listen(TarnStr *host, int64_t port) {
    struct addrinfo *res = net_resolve(host, port, 1);
    if (!res) return -1;
    int fd = -1;
    for (struct addrinfo *a = res; a; a = a->ai_next) {
        fd = net_socket(a->ai_family);
        if (fd < 0) {
            net_error = errno;
            continue;
        }
        int one = 1;
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
        if (bind(fd, a->ai_addr, a->ai_addrlen) == 0 && listen(fd, SOMAXCONN) == 0) break;
        net_error = errno;
        close(fd);
        fd = -1;
    }
    freeaddrinfo(res);
    return fd;
}

// A caught signal that hasn't been received yet ends any wait at once.
static int net_interrupted(void) {
    if (!signal_any_pending()) return 0;
    net_error = EINTR;
    return 1;
}

int64_t tarn_rt_net_accept(int64_t lfd) {
    if (net_interrupted()) return -1;
    int64_t deadline = fd_deadline((int)lfd);
    int fd;
    for (;;) {
        fd = accept((int)lfd, NULL, NULL);
        if (fd >= 0) break;
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            if (net_wait((int)lfd, 0, deadline) <= 0) return -1;
            continue;
        }
        if (errno == EINTR && !signal_any_pending()) continue;
        net_error = errno;
        return -1;
    }
    fcntl(fd, F_SETFD, FD_CLOEXEC);
    set_nonblocking(fd);
    set_fd_timeout(fd, 0);
#ifdef SO_NOSIGPIPE
    int one = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, sizeof one);
#endif
    return fd;
}

int64_t tarn_rt_net_connect(TarnStr *host, int64_t port) {
    if (net_interrupted()) return -1;
    struct addrinfo *res = net_resolve(host, port, 0);
    if (!res) return -1;
    int fd = -1;
    for (struct addrinfo *a = res; a; a = a->ai_next) {
        fd = net_socket(a->ai_family);
        if (fd < 0) {
            net_error = errno;
            continue;
        }
        int rc = connect(fd, a->ai_addr, a->ai_addrlen);
        if (rc < 0 && (errno == EINPROGRESS || errno == EINTR)) {
            // Wait until it's connected (or failed), then ask how it went.
            int w = net_wait(fd, 1, 0);
            int err = 0;
            socklen_t len = sizeof err;
            if (w > 0 && getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &len) == 0 && err == 0) rc = 0;
            else if (w > 0) errno = err ? err : errno;
            else errno = (int)net_error;
        }
        if (rc == 0) break;
        net_error = errno;
        close(fd);
        fd = -1;
    }
    freeaddrinfo(res);
    if (fd >= 0) {
        int one = 1;
        setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    }
    return fd;
}

// Up to `max` bytes (empty at the end of the stream), or NULL on failure.
TarnStr *tarn_rt_net_read(int64_t fd, int64_t max) {
    if (net_interrupted()) return NULL;
    if (max <= 0) max = 1;
    TarnStr *b = str_alloc(max);
    int64_t deadline = fd_deadline((int)fd);
    ssize_t n;
    for (;;) {
        n = recv((int)fd, b->data, (size_t)max, 0);
        if (n >= 0) break;
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            if (net_wait((int)fd, 0, deadline) > 0) continue;
        } else if (errno == EINTR && !signal_any_pending()) {
            continue;
        } else {
            net_error = errno;
        }
        tarn_str_free(b);
        return NULL;
    }
    b->len = n;
    return b;
}

// Writes all of `data`: 0, or -1 on failure.
int64_t tarn_rt_net_write(int64_t fd, TarnStr *data) {
#ifdef MSG_NOSIGNAL
    int flags = MSG_NOSIGNAL;
#else
    int flags = 0;
#endif
    int64_t off = 0, deadline = fd_deadline((int)fd);
    while (off < data->len) {
        ssize_t n = send((int)fd, data->data + off, (size_t)(data->len - off), flags);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                if (net_wait((int)fd, 1, deadline) > 0) continue;
                return -1;
            }
            if (errno == EINTR && !signal_any_pending()) continue;
            net_error = errno;
            return -1;
        }
        off += n;
    }
    return 0;
}

// Reads and writes give up after `ns` nanoseconds (0: never).
int64_t tarn_rt_net_set_timeout(int64_t fd, int64_t ns) {
    if (fd < 0 || fcntl((int)fd, F_GETFD) < 0) {
        net_error = EBADF;
        return -1;
    }
    set_fd_timeout((int)fd, ns > 0 ? ns : 0);
    return 0;
}

void tarn_rt_net_close(int64_t fd) {
    if (fd >= 0) close((int)fd);
}

// "address:port" of one end of a connection (`[address]:port` for IPv6).
static TarnStr *net_address(int64_t fd, int peer) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    int rc = peer ? getpeername((int)fd, (struct sockaddr *)&ss, &len) : getsockname((int)fd, (struct sockaddr *)&ss, &len);
    if (rc != 0) {
        net_error = errno;
        return NULL;
    }
    char ip[INET6_ADDRSTRLEN], out[INET6_ADDRSTRLEN + 16];
    if (ss.ss_family == AF_INET6) {
        struct sockaddr_in6 *a = (struct sockaddr_in6 *)&ss;
        inet_ntop(AF_INET6, &a->sin6_addr, ip, sizeof ip);
        snprintf(out, sizeof out, "[%s]:%d", ip, ntohs(a->sin6_port));
    } else {
        struct sockaddr_in *a = (struct sockaddr_in *)&ss;
        inet_ntop(AF_INET, &a->sin_addr, ip, sizeof ip);
        snprintf(out, sizeof out, "%s:%d", ip, ntohs(a->sin_port));
    }
    return str_from(out, (int64_t)strlen(out));
}

TarnStr *tarn_rt_net_peer_address(int64_t fd) { return net_address(fd, 1); }
TarnStr *tarn_rt_net_local_address(int64_t fd) { return net_address(fd, 0); }

// The `net.ErrorKind` of a failure (its variants, in order).
int64_t tarn_rt_net_error_kind(int64_t code) {
    if (code >= NET_EAI_BASE) return 5; // NotFound
    switch (code) {
    case ECONNREFUSED: return 0;        // Refused
    case ECONNRESET: case EPIPE: case ENOTCONN: case ECONNABORTED: return 1; // Reset
    case EAGAIN: case ETIMEDOUT: return 2;
#if EWOULDBLOCK != EAGAIN
    case EWOULDBLOCK: return 2;         // TimedOut
#endif
    case EINTR: return 3;               // Interrupted
    case EADDRINUSE: return 4;          // AddressInUse
    case EADDRNOTAVAIL: case EHOSTUNREACH: case ENETUNREACH: return 6; // Unreachable
    case EBADF: return 7;               // Closed
    default: return 8;                  // Other
    }
}

TarnStr *tarn_rt_net_error_message(int64_t code) {
    const char *m = code >= NET_EAI_BASE ? gai_strerror((int)(code - NET_EAI_BASE)) : strerror((int)code);
    if (code == EAGAIN || code == EWOULDBLOCK) m = "timed out";
    if (code == EINTR) m = "interrupted by a signal";
    return str_from(m, (int64_t)strlen(m));
}

static int env_flag(const char *name) {
    const char *v = getenv(name);
    return v && *v && *v != '0';
}

static void arc_report(void) {
    int check = env_flag("TARN_ARC_CHECK");
    if (check) tarn_collect_cycles(); // garbage cycles still waiting for a collection
    // The totals of every thread (a block freed on another thread than the
    // one that allocated it counts there, so only the sums mean anything).
    int64_t allocs = 0, frees = 0, live = 0, live_bytes = 0, peak = 0, collections = 0, cycle_frees = 0;
    pthread_mutex_lock(&thread_states_lock);
    for (ThreadState *t = all_thread_states; t; t = t->next) {
        allocs += t->allocs;
        frees += t->frees;
        live += t->live;
        live_bytes += t->live_bytes;
        peak += t->peak_bytes;
        collections += t->collections;
        cycle_frees += t->cycle_frees;
    }
    pthread_mutex_unlock(&thread_states_lock);
    if (env_flag("TARN_ARC_STATS")) {
        fprintf(stderr, "tarn arc: %lld allocations, %lld frees, %lld live, peak %lld bytes, %lld cycle collections freed %lld\n",
                (long long)allocs, (long long)frees, (long long)live, (long long)peak, (long long)collections, (long long)cycle_frees);
    }
    if (check && live != 0) {
        fprintf(stderr, "tarn: %lld objects leaked (%lld bytes)\n", (long long)live, (long long)live_bytes);
    }
}

int main(int argc, char **argv) {
    saved_argc = argc;
    saved_argv = argv;
    main_task.ctxp = &main_ctx;
    tl_ctx = &main_ctx;
    init_ascii_chars();
    tarn_main();
    // The program ends when `main` and every task it started are done.
    wait_for_tasks();
    fflush(stdout);
    arc_report();
    return 0;
}
