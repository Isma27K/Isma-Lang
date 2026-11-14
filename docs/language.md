# The Isma language guide

[← Back to the README](../README.md) · [The `time` module](time.md) · [Installing](install.md)

Isma reads like Python, checks like Kotlin and compiles to native code. This guide covers the whole language as it is today.

- [Running programs](#running-programs)
- [Variables: `val` and `var`](#variables-val-and-var)
- [Types](#types)
- [Strict typing](#strict-typing)
- [Warnings (lints)](#warnings-lints)
- [Functions](#functions)
- [Control flow](#control-flow)
- [Structs and methods](#structs-and-methods)
- [Lists](#lists)
- [Strings and f-strings](#strings-and-f-strings)
- [Memory](#memory)
- [Modules: `import`](#modules-import)
- [Built-in functions](#built-in-functions)
- [Panics](#panics)
- [Known limitations](#known-limitations)

---

## Running programs

Every program needs a `fn main():`.

```bash
isma hello.isma                  # compile + run, like `python hello.py`
isma greet.isma Ana Budi         # everything after the script goes to the script
isma build app.isma -o app       # a standalone native binary; ship it without Isma
isma check app.isma              # type-check only
isma build app.isma --emit-llvm  # also write the LLVM IR (app.ll)
isma build app.isma --emit-mir   # also write the mid-level IR, showing every reference-count operation (app.mir)
isma run --arc-stats app.isma    # print memory counters when the program exits
isma run --debug app.isma        # -O0 -g instead of -O3
isma clean                       # empty the compile cache
```

Scripts can start with a shebang line and be run directly:

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

The first run of a script compiles it (about 80 ms). Later runs of the unchanged script reuse the cached binary and start in about **2 ms**. Editing the script, or upgrading Isma, triggers a recompile automatically. The cache lives in `~/.cache/isma`; set `ISMA_CACHE_DIR` to move it.

## Variables: `val` and `var`

```python
val name = "Isma"        # immutable; the type String is inferred
var count: Int = 0       # mutable; the type is written explicitly
count += 1
name = "other"           # error: cannot assign twice to immutable variable `name`
```

- Every variable must be initialized. There is no `null`.
- Declaring a name twice in the same block is an error, but an inner block may shadow an outer name.
- `_` is a throwaway name: `val _ = compute()` and `for _ in 0..3:` are fine, and `_` can be declared any number of times.

## Types

| Type | Notes |
|---|---|
| `Int` | 64-bit signed. Overflow **panics** instead of silently wrapping. |
| `Float` | 64-bit IEEE 754. Prints like Python: `3.0`, `0.1`, `1e+16`. |
| `Bool` | `true` / `false`. |
| `String` | Immutable UTF-8. `+` concatenates. |
| `List[T]` | Growable and bounds-checked. A reference type. |
| structs | User-defined. A reference type, like Python and Kotlin objects. |

Lists and structs are references: `val b = a` makes `b` point at the same list as `a`. Memory is managed for you, see [Memory](#memory).

## Strict typing

Isma never converts between types for you:

```python
val a = 1 + 2.0              # error: `+` cannot be applied to `Int` and `Float`
val b = 1.to_float() + 2.0   # ok
if count:                    # error: condition must be a `Bool`. There's no "truthiness".
if count != 0:               # ok
print("n = " + 5)            # error. Write f"n = {5}" or "n = " + 5.to_string()
```

On top of that:
- every path through a function with a return type must `return`;
- `==` only works on `Int`, `Float`, `Bool` and `String`;
- comparisons don't chain (`a < b < c` is an error);
- an expression used as a statement must be a call, so a stray `x == 2` on its own line is caught.

Every error comes with an explanation, usually with a suggested fix: `use .to_float()`, `did you mean count?`, `make it mutable with var count`.

## Warnings (lints)

Warnings never stop a program from compiling. They point at code that is probably not what you meant:

| Warning | Fix |
|---|---|
| `unused variable` / `unused parameter` / `unused loop variable` | Remove it, or start the name with `_` |
| `` `x` is declared `var` but never reassigned `` | Use `val`. (`val` still lets you change a list's contents or a struct's `var` fields.) |
| `unreachable code` | Code after `return`, `break`, `continue` or `panic` never runs |
| `` unused import `time` `` | Remove the `import` line |

## Functions

```python
fn clamp(x: Int, low: Int, high: Int) -> Int:
    if x < low:
        return low
    elif x > high:
        return high
    return x

clamp(15, 0, 10)                 # positional arguments
clamp(x=15, high=10, low=0)      # named arguments, in any order
```

- A function can be called before the point where it's defined.
- A function without `-> T` returns nothing.
- A `#` comment directly above a function is its documentation, and editors show it on hover.

## Control flow

```python
if a and not b:
    ...
elif c or d:
    ...
else:
    ...

while i < 10:
    i += 1

for i in 0..10:      # 0 to 9
    ...
for i in 1..=10:     # 1 to 10
    ...
for x in my_list:    # each element
    ...
```

`break`, `continue`, `return` and `pass` work as in Python. A block may also sit on the same line: `if x > 0: return x`. Blocks use `:` plus indentation (4 spaces); braces are a compile error with a hint.

## Structs and methods

```python
struct Account:
    owner: String        # immutable field (the default)
    var balance: Int     # mutable field

    fn deposit(self, amount: Int):
        self.balance += amount

val acc = Account("ana", 0)          # or Account(owner="ana", balance=0)
acc.deposit(50)
acc.owner = "bob"                    # error: cannot assign to immutable field `owner`
```

Methods take `self` as their first parameter.

## Lists

```python
val xs = [3, 1, 2]                   # List[Int]. `val` still allows push and xs[0] = ...
val empty: List[String] = []         # an empty list needs a type annotation
xs.push(4)
xs[0] = 10
val last = xs.pop()
print(xs.len())
xs[99]                               # panic: index 99 out of bounds for list of length 3
```

| List method | |
|---|---|
| `len() -> Int` | Number of elements |
| `push(value)` | Append to the end |
| `pop() -> T` | Remove and return the last element (panics if the list is empty) |

## Strings and f-strings

```python
val s = f"pi is about {3.14159}, 2 + 2 = {2 + 2}, ok = {true}"
"abc" < "abd"            # comparison is lexicographic
" 42 ".to_int()          # 42. Invalid input panics, like Kotlin's toInt().
"2.5".to_float()
for c in "héllo".chars():
    print(c)
```

| String method | |
|---|---|
| `len() -> Int` | Length in bytes |
| `chars() -> List[String]` | Each character as a one-character string |
| `substring(start, end) -> String` | Bytes `start..end`. Panics if out of range or splitting a character |
| `contains(s)`, `starts_with(s)`, `ends_with(s)` | `-> Bool` |
| `to_int()`, `to_float()`, `to_string()` | Conversions. Parsing panics on bad input |

`Int`, `Float` and `Bool` have `.to_string()`; `Int` has `.to_float()`; `Float` has `.to_int()`, which truncates.

## Memory

Isma frees memory automatically, without a garbage collector and without pauses. Every string, list and struct counts its references, and it is freed the moment the last one goes away. The compiler works out each value's **last use**, so a value dies right after it, not at the end of the block:

```python
fn main():
    val words = load_words()     # a big list
    print(words.len())           # last use: the list is freed right here
    long_running_work()          # ...so it doesn't sit in memory during this
```

Most of the counting never happens. The compiler (a Perceus-style optimizer, as in Koka and Lean 4) removes it wherever it can prove it isn't needed:

- **Borrowing.** Parameters are borrowed unless the function keeps them, so calls usually do no counting. Reading `task.name` or looping over `for task in tasks:` does none either, as long as nothing in the loop could replace those values.
- **Moving.** Passing a value on its last use hands over its reference instead of copying it.
- **Updating in place.** When a value has a single owner, `s += x` appends to the existing string, so building a string in a loop takes linear time. Likewise `pos = pos.add(vel)` builds the new struct in the old one's memory, so the loop doesn't allocate at all.
- **Cycles.** A struct that can end up containing itself, such as `struct Node:` with `var children: List[Node]` followed by `n.children.push(n)`, is freed by a small cycle collector. The compiler only enables it for types that can actually form a cycle; strings, `List[Int]` and most structs never pay for it.

To see what's happening:

```console
$ isma run --arc-stats bench/particles.isma
5000000
-12497502500
isma arc: 3 allocations, 3 frees, 0 live, peak 96 bytes, 0 cycle collections freed 0
```

`isma build app.isma --emit-mir` writes the mid-level IR, with every `dup` (a new reference), `drop` (a reference released) and `drop-reuse` (memory recycled in place) the compiler inserted.

## Modules: `import`

```python
import time

fn main():
    val now = time.now()        # module functions: module.function(...)
    val t: time.DateTime = now  # module types: module.Type
```

- **Written in Isma.** Standard-library modules live in [`stdlib/`](../stdlib) and are compiled together with your program.
- **Privacy.** Names starting with `_` are private to their module.
- **Construction.** A module's structs can only be created through its functions, which validate the values. You can't build a `DateTime` for February 30.
- **Panic locations.** A panic inside a standard-library function is reported at *your* line that called it, like Rust's `#[track_caller]`:
  ```
  panic at app.isma:4:11: invalid date 2023-02-29: February 2023 has 28 days
  ```

Available modules: [`time`](time.md).

## Built-in functions

| Function | |
|---|---|
| `print(x)` | Print an `Int`, `Float`, `Bool` or `String`, followed by a newline |
| `input() -> String` | Read one line from stdin, without the newline |
| `args() -> List[String]` | Command-line arguments passed to the program |
| `sqrt(x: Float) -> Float` | Square root |
| `assert(cond: Bool)` | Panic if `cond` is false |
| `panic(msg: String)` | Stop the program with a message |
| `exit(code: Int)` | Exit the process |

## Panics

Runtime errors print where they happened and exit with code 101:

```
panic at crash.isma:5:15: index 3 out of bounds for list of length 3
```

Panics cover:
- integer overflow and division by zero;
- out-of-bounds indexing and `pop()` on an empty list;
- a failed `assert`, a failed parse (`"abc".to_int()`), and `panic(...)`.

## Known limitations

- **One file of your own.** You can `import` standard-library modules, but not your own `.isma` files yet.
- **No top-level constants.** Everything lives inside functions.
- **Byte-based strings.** `String.len()` and `substring()` count bytes, and there's no `s[i]` indexing; use `chars()`.
- **No negative indexing.** `xs[-1]` panics.
- **No operator overloading.** Compare structs with methods, e.g. `a.equals(b)` or `a.millis_since(b)`.
