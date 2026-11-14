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
- [Enums and `match`](#enums-and-match)
- [Lists](#lists)
- [Strings and f-strings](#strings-and-f-strings)
- [Null safety](#null-safety)
- [Memory](#memory)
- [Modules: `import`](#modules-import)
  - [Your own files](#your-own-files)
  - [The standard library](#the-standard-library)
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

- Every variable must be initialized. A variable can only hold `null` if its type says so (`Int?`), see [Null safety](#null-safety).
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
| enums | User-defined: one of several variants, which may carry data. See [Enums and `match`](#enums-and-match). |
| `T?` | A `T` or `null`, e.g. `Int?`, `String?`, `User?`. See [Null safety](#null-safety). |

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
- `==` only works on `Int`, `Float`, `Bool`, `String` and enums without data, and on their nullable versions (where `null == null`);
- a `match` must handle every possible value;
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
| `` unused import `time` `` | Remove the `import` line (or the name, from a `from` import) |
| ``unnecessary `?.` `` / `` `?:` `` / `` `!!` `` | The value is never `null` here, often because of a [smart cast](#smart-casts): use `.`, or drop the `?:` or `!!` |
| `` `n` is never `null`, so this is always `false` `` | Remove the check |
| ``unreachable `case` `` / `unreachable pattern` | Earlier cases already match everything it would: remove it, or move it up |

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

## Enums and `match`

An enum's values are one of several **variants**. A variant can carry data, in named fields like a struct's:

```python
enum Shape:
    Circle(radius: Float)
    Rect(w: Float, h: Float)
    Empty

    fn area(self) -> Float:                # enums have methods too
        match self:
            case Circle(r):
                return 3.14159 * r * r
            case Rect(w, h):
                return w * h
            case Empty:
                return 0.0

val shapes = [Shape.Circle(1.0), Shape.Rect(w=2.0, h=3.0), Shape.Empty]
```

- Build a value with `Shape.Circle(1.0)`, passing the fields by position or by name as for structs. A variant without data is just `Shape.Empty`.
- Variant fields can't be changed. To read them, take the value apart with `match`.
- Enums can be recursive: `enum Expr:` with `Num(value: Int)` and `Add(a: Expr, b: Expr)` describes expression trees.
- An enum whose variants carry no data is just a number underneath:

  ```python
  enum Color:
      Red
      Green
      Blue

  val c = Color.Green
  print(c == Color.Green)       # true: `==` works on these
  print(f"{c}")                 # Green
  ```

### `match`

`match` tries each `case` from top to bottom and runs the first one that matches. Cases never fall through.

```python
match command:
    case Move(Point(0, 0)):                # nested patterns
        go_home()
    case Move(Point(x, y)) if x == y:      # a guard: the case only matches if it's true
        print(f"diagonal {x}")
    case Move(p):                          # `p` is bound to the field
        go(p)
    case Say(text) | Shout(text):          # either one
        print(text)
    case Quit:
        break
```

| Pattern | Matches |
|---|---|
| `_` | anything |
| `name` (lowercase) | anything, and binds it to `name` |
| `Circle(r)`, `Shape.Circle(r)` | that variant; each field is matched by its own pattern |
| `Rect(w, _)`, `Rect(h=height)` | fields by position (all of them) or by name (any of them) |
| `Circle` | any `Circle`, whatever its fields |
| `Point(x, 0)` | a struct, field by field |
| `0`, `-1`, `"quit"`, `true` | that exact value |
| `null` | `null`. Any other pattern on a `T?` matches only non-null values |
| `p \| q` | `p` or `q` (both must bind the same names) |
| `p as name` | `p`, and binds the whole value to `name` |

Floats can't be patterns: bind the value and use a guard, as in `case x if x > 0.5:`.

### Exhaustiveness

Every `match` must handle every value the type allows. The compiler checks, and names what's missing:

```
Error: this `match` doesn't handle every value: `Rect(_, _)` and `Empty` are not covered
   ╭─[ shapes.isma:7:11 ]
   │
 7 │     match s:
   │           ┬
   │           ╰── this is `Shape`
   │
   │ Help: add a case for it:
   │
   │       case Rect(_, _):
   │           ...
   │
   │       or end with `case _:` to handle everything else
───╯
```

- **Nested patterns are checked too.** A tree match with only `Leaf`, `Node(Leaf, _, Leaf)` and `Node(Node(_, _, _), _, _)` reports that `Node(Leaf, _, Node(_, _, _))` is missing.
- **Guarded cases don't count**, because the guard might be false.
- **Numbers and strings need a catch-all** (`_` or a name), since no list of literals can cover them.
- **Unreachable cases are flagged.** A case that the cases above it already cover gets an "unreachable `case`" warning.

The checker uses the usefulness algorithm from Luc Maranget's *Warnings for pattern matching*, the same one Rust and OCaml use.

Since one case always runs, a function whose cases all `return` needs no `return` after the `match`.

`match` works with [smart casts](#smart-casts). Inside a case that can't match `null`, the matched variable isn't null. After `case null: return`, it isn't null for the rest of the function. A name bound in a case after `case null:` gets the non-null type.

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
| `to_int_or_null()`, `to_float_or_null()` | `-> Int?` / `-> Float?`: `null` instead of a panic on bad input |

`Int`, `Float` and `Bool` have `.to_string()`; `Int` has `.to_float()`; `Float` has `.to_int()`, which truncates.

## Null safety

A value can only be `null` if its type says so. `T?` means "a `T` or `null`", and a plain `T` is never null:

```python
val a: Int = null                        # error: `Int` can't be `null`
val b: Int? = null                       # ok
val port = "8080".to_int_or_null()       # Int?: null if the text isn't a number
```

You can't use a `T?` where a `T` is needed until you've dealt with the `null`. The error says how:

```
Error: `user` might be null
   ╭─[ app.isma:9:11 ]
   │
 9 │     print(user.name)
   │           ──┬─
   │             ╰─── this is `User?`
   │
   │ Help: use `user?.name` to get `null` instead, or check `if user != null:` first
   │
   │ Note: if it can never be null here, assert that with `user!!.name` (it panics if it is)
───╯
```

The tools, as in Kotlin:

| | |
|---|---|
| `x?.field`, `x?.method(...)` | **Safe call.** `null` if `x` is null (and then the arguments aren't evaluated), otherwise the member. `user?.name` is a `String?`. Chain them: `order?.customer?.address`. |
| `x ?: default` | **Elvis.** `x` if it isn't null, otherwise `default`, which is only evaluated when needed. `user?.name ?: "anonymous"` is a `String`. |
| `x ?: return` | Or `?: return value`, `?: break`, `?: continue`, `?: panic("...")`: leave instead. The result is then never null. |
| `x!!` | "I know this isn't null": the value as a `T`, or a panic if it is null. |
| `x == null`, `x != null` | Checks, which smart-cast `x` (below). |

`print` and f-strings show a null value as `null`. `==` works between `Int?` values (`null == null` is true, and `null` equals nothing else). A list literal with a `null` in it is a list of nullables: `[1, null, 3]` is a `List[Int?]`. Lists don't convert, though: a `List[Int]` is not a `List[Int?]`, because then you could push a `null` into it.

### Smart casts

Once the compiler has seen that a value isn't null, it treats it as non-null for as long as that's certain. You don't have to write `!!` or repeat the check:

```python
fn greet(user: User?) -> String:
    if user == null:
        return "hello, stranger"
    return f"hello, {user.name}"             # `user` is a `User` from here on

fn length(head: Node?) -> Int:
    var n = 0
    var cur = head
    while cur != null:
        n += 1
        cur = cur.next                       # `cur` is a `Node` inside the loop
    return n
```

A value is smart-cast:
- inside `if x != null:`, and in the `else` of `if x == null:`;
- after a check that leaves: `if x == null: return` (or `break`, `continue`, `panic`), `val v = x ?: return`, `x!!` and `assert(x != null)`;
- on the right of `and` / `or`: `if x != null and x.ready:`;
- inside `while x != null:`, and after a loop `while x == null:` without a `break`;
- after `x == 5` is true, and after `x?.field != null` is true, `x` too;
- after it's assigned a non-null value: `var best: Int? = null`, then `best = 3`, and `best + 1` works.

The `if` branches join: after `if x == null: x = 0`, `x` is non-null either way.

Smart casts apply to local variables, parameters, and chains of `val` fields (`order.customer.address`). They don't apply to `var` fields, which might change between the check and the use. The error says so, and suggests copying the field into a `val` first. A `var` local that is reassigned inside a loop also isn't smart-cast from a check made before the loop. Hover over a smart-cast name in the editor to see its narrowed type.

Null safety costs nothing at run time. A `String?`, `List[T]?` or `User?` is a pointer that may be null. An `Int?`, `Float?` or `Bool?` is the value plus a flag, which stays in registers, so nothing is ever allocated ("boxed") for it.

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
- **Cycles.** A struct that can end up containing itself, such as `struct Node:` with `var children: List[Node]` followed by `n.children.push(n)`, is freed by a small cycle collector. The compiler only enables it for types that can actually form a cycle, which takes a reference that can be replaced after the value is built (a `var` field or a list). Strings, `List[Int]`, immutable trees such as an `enum Expr` and most structs never pay for it.
- **Enums.** A variant without data is a constant shared by the whole program, so `Shape.Empty` never allocates. An enum without any data (`Color`) is a plain number and isn't counted at all.

To see what's happening:

```console
$ isma run --arc-stats bench/particles.isma
5000000
-12497502500
isma arc: 3 allocations, 3 frees, 0 live, peak 96 bytes, 0 cycle collections freed 0
```

`isma build app.isma --emit-mir` writes the mid-level IR, with every `dup` (a new reference), `drop` (a reference released) and `drop-reuse` (memory recycled in place) the compiler inserted.

## Modules: `import`

Every `.isma` file is a module. A program is one file plus everything it imports.

### Your own files

```
app/
├── main.isma
├── geometry.isma
└── shapes/
    ├── registry.isma
    └── util.isma
```

```python
# main.isma
import geometry                          # geometry.isma, next to this file
import geometry as geo                   # the same module, under another name
import shapes.registry                   # shapes/registry.isma, bound as `registry`
from geometry import Point, Shape, area  # names you can use without `geometry.`
from shapes import util                  # a file of the folder `shapes`
from geometry import origin as zero      # renamed

fn main():
    val p = Point(3.0, 4.0)              # a struct from another file
    print(geometry.area(geo.Shape.Circle(p, 2.0)))
    print(area(Shape.Empty))
    print(registry.count())
```

```python
# shapes/registry.isma
from ..geometry import Shape             # `..` is the folder above this file
from . import util                       # `.` is this file's folder
```

- **Where modules are found.** `import a.b` reads `a/b.isma`, starting from the folder of the file that contains the `import`; every file finds its imports the same way, whichever file you run. `from .x` and `from ..x` (`from .. import x`) start from this folder or the one above.
- **What a plain `import` binds.** The last part of the path: `import shapes.registry` gives you `registry.count()`. With `as`, the name you choose.
- **What a module contains.** Functions, structs and enums. It doesn't need a `main`: `isma check` and the editor check it like any other file, and only `isma run` / `isma build` ask for an entry point. If it has a `fn main()`, that only runs when you run the file itself, so a module can carry its own demo or tests.
- **Privacy.** Names starting with `_` are private to their module: functions, types, methods and fields. A struct with private fields can only be created inside its module, so a module can guarantee its structs are valid.
- **Cycles are fine.** Two modules may import each other, and a module may import the file you run. Modules contain only declarations, so there is no order of initialization to get wrong.
- **Your files come first.** A `time.isma` next to your file is imported instead of the standard library's `time`.
- **Errors show where they are.** A type error in `shapes/registry.isma` is reported in that file. In VS Code, it also shows on the `import` line that leads to it.
- **One program, one binary.** Everything is compiled together into a single native executable, and `isma app.isma` rebuilds when any of its files changes.

`import *` is not supported: name what you use, or import the module.

### The standard library

```python
import time

fn main():
    val now = time.now()        # module functions: module.function(...)
    val t: time.DateTime = now  # module types: module.Type
```

- **Written in Isma.** Standard-library modules live in [`stdlib/`](../stdlib) and are compiled together with your program.
- **Construction.** A standard-library struct can only be created through its module's functions, which validate the values. You can't build a `DateTime` for February 30.
- **Panic locations.** A panic inside a standard-library function is reported at *your* line that called it, like Rust's `#[track_caller]`. (A panic inside your own module is reported where it happened.)
  ```
  panic at app.isma:4:11: invalid date 2023-02-29: February 2023 has 28 days
  ```

Available modules: [`time`](time.md).

## Built-in functions

| Function | |
|---|---|
| `print(x)` | Print an `Int`, `Float`, `Bool`, `String` or enum without data (its variant's name), followed by a newline. A nullable one prints `null` when it is null |
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
- a failed `assert`, a failed parse (`"abc".to_int()`), and `panic(...)`;
- `x!!` when `x` is null.

## Known limitations

- **No packages to install.** Modules come from your own folders and the standard library; there is no package manager or search path.
- **No top-level constants.** Everything lives inside functions.
- **Byte-based strings.** `String.len()` and `substring()` count bytes, and there's no `s[i]` indexing; use `chars()`.
- **No negative indexing.** `xs[-1]` panics.
- **No operator overloading.** Compare structs with methods, e.g. `a.equals(b)` or `a.millis_since(b)`.
- **`match` is a statement.** It doesn't produce a value; assign or `return` inside its cases.
- **No generic enums yet.** You can't write `Option[T]` yourself (but `T?` covers the most common use).
