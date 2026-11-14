# Isma

**Python's syntax. Kotlin's strictness. Rust's speed.**

Isma is a compiled programming language written for fun. Programs read like Python, are checked as strictly as Kotlin, and compile ahead of time into native binaries through LLVM, the same optimizer Rust uses.

```python
struct Vec2:
    var x: Float
    var y: Float

    fn length(self) -> Float:
        return sqrt(self.x * self.x + self.y * self.y)

fn fib(n: Int) -> Int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

fn main():
    val v = Vec2(x=3.0, y=4.0)
    var xs: List[Int] = []
    for i in 0..10:
        xs.push(fib(i))
    print(f"|v| = {v.length()}, fib(9) = {xs[9]}")
```

```
$ isma run demo.isma
|v| = 5.0, fib(9) = 34
```

## Install

You need Rust (to build the compiler) and clang. Isma uses clang to produce native code, and looks for it in `$ISMA_CC`, then `PATH`, then `/usr/lib/llvm-*/bin`.

```bash
./install.sh                         # builds and installs to ~/.local/bin/isma
PREFIX=/usr/local sudo -E ./install.sh   # or system-wide
./install.sh --uninstall             # removes the binary and the cache
```
`cargo install --path .` also works; that puts `isma` in `~/.cargo/bin`.

## Usage: just like Python

```bash
isma hello.isma                  # compile + run, like `python hello.py`
isma greet.isma Ana Budi         # everything after the script goes to the script
```

Scripts can carry a shebang line and run directly:
```python
#!/usr/bin/env isma
fn main():
    for name in args():
        print(f"Hello, {name}!")
```
```bash
chmod +x greet.isma
./greet.isma Ana                 # Hello, Ana!
```

The first run of a script compiles it to native code (about 80 ms). Later runs of the unchanged script reuse the cached binary and start in about **2 ms**, faster than Python's own startup. Editing the script, or upgrading Isma, triggers a recompile automatically. The cache lives in `~/.cache/isma` (override it with `ISMA_CACHE_DIR`), and `isma clean` empties it.

Other commands:
```bash
isma build fib.isma -o fib       # standalone native binary; ship it without Isma
isma check app.isma              # type-check only
isma build x.isma --emit-llvm    # also write the LLVM IR (x.ll)
isma run --debug x.isma          # -O0 -g instead of -O3
```

## Language tour

### Variables: `val` and `var`
```python
val name = "Isma"        # immutable, type inferred as String
var count: Int = 0       # mutable, type written explicitly
count += 1
name = "other"           # compile error: cannot assign twice to immutable variable
```
Every variable has to be initialized; there is no null. Declaring a name twice in the same scope is an error. An inner block may shadow an outer name.

### Types
| Type | Notes |
|---|---|
| `Int` | 64-bit signed. Overflow **panics** instead of silently wrapping. |
| `Float` | 64-bit IEEE 754. |
| `Bool` | `true` / `false`. |
| `String` | Immutable UTF-8. `+` concatenates. `.len()` counts bytes. |
| `List[T]` | Growable, bounds-checked, reference semantics. |
| `struct`s | User-defined, reference semantics (like Python and Kotlin objects). |

### Strict typing (the Kotlin part)
Isma never converts types for you:

```python
val a = 1 + 2.0          # error: `+` cannot be applied to `Int` and `Float`
val b = 1.to_float() + 2.0   # ok
if count:                # error: condition must be a `Bool`. There's no truthiness.
if count != 0:           # ok
print("n = " + 5)        # error. Use f"n = {5}" or 5.to_string()
```

On top of that:
- every path through a function with a return type must `return`;
- `==` only works on `Int`, `Float`, `Bool` and `String`;
- comparisons don't chain;
- an expression used as a statement must be a call, so a typo like `x == 2` on its own line is caught.

Errors look like Rust's:

```
Error: cannot assign twice to immutable variable `count`
   ╭─[ demo.isma:3:5 ]
   │
 3 │     count = count + 2.5
   │     ──┬──
   │       ╰──── cannot assign twice to immutable variable
   │
 2 │     val count = 10
   │         ──┬──
   │           ╰──── `count` is declared with `val` here
   │
   │ Help: make it mutable with `var count`
───╯
Error: operator `+` cannot be applied to `Int` and `Float`
   ...
   │ Help: Isma never converts implicitly; use `.to_float()` on the `Int` side
```

### Functions
```python
fn clamp(x: Int, low: Int, high: Int) -> Int:
    if x < low:
        return low
    elif x > high:
        return high
    return x

clamp(15, 0, 10)                 # positional arguments
clamp(x=15, high=10, low=0)      # named arguments, any order
```
Functions can be called before the point where they're defined. A function without `-> T` returns nothing.

### Control flow
```python
if a and not b: ...
elif c or d: ...
else: ...

while i < 10:
    i += 1

for i in 0..10:      # 0 to 9
for i in 1..=10:     # 1 to 10
for x in my_list:    # loop over elements

break, continue, return, pass
```
A block can also sit on the same line: `if x > 0: return x`.

### Structs and methods
```python
struct Account:
    owner: String        # immutable field (the default)
    var balance: Int     # mutable field

    fn deposit(self, amount: Int):
        self.balance += amount

val acc = Account("ana", 0)          # or Account(owner="ana", balance=0)
acc.deposit(50)
acc.owner = "bob"                    # compile error: immutable field
```

### Lists
```python
var xs = [3, 1, 2]                   # List[Int]
var empty: List[String] = []         # an empty list needs a type annotation
xs.push(4)
xs[0] = 10
val last = xs.pop()
print(xs.len())
xs[99]                               # panic: index 99 out of bounds for list of length 3
```

### Strings and f-strings
```python
val s = f"pi is about {3.14159}, 2 + 2 = {2 + 2}, ok = {true}"
"abc" < "abd"            # comparison is lexicographic
" 42 ".to_int()          # 42. Invalid input panics, like Kotlin's toInt().
"2.5".to_float()
```

### Built-ins
| Function | Description |
|---|---|
| `print(x)` | Print any `Int`, `Float`, `Bool` or `String`, followed by a newline |
| `input() -> String` | Read one line from stdin (without the newline) |
| `args() -> List[String]` | Command-line arguments passed to the program |
| `sqrt(x: Float) -> Float` | Square root |
| `assert(cond: Bool)` | Panic if `cond` is false |
| `panic(msg: String)` | Abort with a message |
| `exit(code: Int)` | Exit the process |

Conversions are methods: `.to_string()`, `.to_int()`, `.to_float()`.

### Panics
Runtime errors print where they happened and exit with code 101:
```
panic at examples/calc.isma:9:19: cannot parse "abc" as Float
```
Panics cover integer overflow, division by zero, out-of-bounds indexing, `pop()` on an empty list, failed `assert`, failed parsing and `panic(...)`.

## Performance

`bench/run.sh` runs the same programs in Isma, Rust (`rustc -O`) and CPython. Best of 3 runs on one machine:

| benchmark | Isma | Rust | Python |
|---|---|---|---|
| `fib(35)` (recursion) | 41 ms | 25 ms | 842 ms |
| sieve to 10M (`List[Bool]`) | 38 ms | 34 ms | 2043 ms |

About 20–50× faster than Python and close to Rust. On `fib`, the remaining gap comes from Isma's overflow checks: Rust in release mode silently wraps on overflow. With `rustc -O -C overflow-checks=on`, Rust's `fib` takes about 46 ms, which is slower than Isma.

## How it works

```
source.isma
   │  lexer.rs     tokens + Python-style INDENT/DEDENT
   ▼
   │  parser.rs    recursive descent + precedence climbing → AST
   ▼
   │  check.rs     name resolution, strict types, val/var, definite returns → typed AST
   ▼
   │  codegen.rs   textual LLVM IR (allocas → mem2reg, checked arithmetic, TBAA)
   ▼
clang -O3  program.ll + runtime.c  →  native executable
```

The pieces in more detail:
- **Compiler.** About 4,500 lines of Rust. Its only dependency is [`ariadne`](https://crates.io/crates/ariadne), which renders the error messages.
- **Runtime.** `runtime/runtime.c` (~300 lines) is embedded in the compiler binary. It contains the allocator, string helpers, list growth, printing and panics.
- **Overflow checks.** Integer `+ - *` compile to `llvm.s*.with.overflow`, with branches into cold panic paths.
- **Alias analysis.** The compiler emits TBAA metadata: list headers, elements of each type and each struct field get separate alias classes. Isma has no pointer casts, so this is sound, and it lets LLVM keep values like a list's length in registers inside hot loops.

## Project layout
```
src/            compiler (lexer, parser, checker, codegen, driver, CLI)
runtime/        C runtime linked into every program
examples/       hello, fib, fizzbuzz, structs, primes_sieve, sort, collatz, guess, calc
install.sh      installs `isma` onto your PATH
tests/run/      programs + expected stdout
tests/fail/     programs that must not compile (`# expect-error: ...`)
tests/panic/    programs that must panic (`# expect-panic: ...`)
bench/          Isma vs Rust vs Python
```
`cargo test` runs the unit tests and every `.isma` test case.

## Known limitations (v0.1)
- **Memory is never freed.** Allocation uses a bump arena, which is fine for short programs. ARC is next on the list.
- Programs are a single file: no modules or imports yet.
- No top-level constants. Everything lives inside functions.
- `String.len()` counts bytes, and strings can't be indexed yet.
- No negative indexing (`xs[-1]` panics).

## Roadmap
- [ ] Null safety: `Int?`, `none`, `?.`, the elvis operator `?:`, smart casts
- [ ] Memory management through automatic reference counting
- [ ] `enum` + `match` with exhaustiveness checking
- [ ] Generics for user code (`fn max[T](a: T, b: T) -> T`)
- [ ] Modules and imports
- [ ] Closures and lambdas
- [ ] Top-level `val` constants
- [ ] A larger standard library (maps, string methods, file IO)
