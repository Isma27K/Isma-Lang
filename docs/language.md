# The Tarn language guide

[← Back to the README](../README.md) · [`time`](time.md) · [`math`](math.md) · [`fs`](fs.md) · [Installing](install.md)

Tarn reads like Python, checks like Kotlin and compiles to native code. This guide covers the whole language as it is today.

- [Running programs](#running-programs)
- [Variables: `val` and `var`](#variables-val-and-var)
- [Constants](#constants)
- [Types](#types)
- [Strict typing](#strict-typing)
- [Warnings (lints)](#warnings-lints)
- [Functions](#functions)
- [Control flow](#control-flow)
- [Structs and methods](#structs-and-methods)
- [Enums and `match`](#enums-and-match)
- [Operator functions](#operator-functions)
- [Generics](#generics)
- [Interfaces](#interfaces)
- [Lists](#lists)
- [Maps](#maps)
- [Strings and f-strings](#strings-and-f-strings)
- [Bytes](#bytes)
- [Null safety](#null-safety)
- [Recoverable errors](#recoverable-errors)
- [Memory](#memory)
- [Cleaning up: `fn drop`](#cleaning-up-fn-drop)
- [Tasks](#tasks)
  - [What a task gets](#what-a-task-gets)
  - [Channels](#channels)
  - [Mutex and Atomic](#mutex-and-atomic)
  - [Panics in tasks](#panics-in-tasks)
  - [How it runs](#how-it-runs)
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
tarn hello.tr                    # compile + run, like `python hello.py`
tarn greet.tr Ana Budi           # everything after the script goes to the script
tarn build app.tr -o app         # a standalone native binary; ship it without Tarn
tarn check app.tr                # type-check only
tarn build app.tr --emit-llvm    # also write the LLVM IR (app.ll)
tarn build app.tr --emit-mir     # also write the mid-level IR, showing every reference-count operation (app.mir)
tarn run --arc-stats app.tr      # print memory counters when the program exits
tarn run --debug app.tr          # -O0 -g instead of -O3
tarn clean                       # empty the compile cache
```

Scripts can start with a shebang line and be run directly:

```python
#!/usr/bin/env tarn
fn main():
    for name in args():
        print(f"Hello, {name}!")
```

```bash
chmod +x greet.tr
./greet.tr Ana                   # Hello, Ana!
```

The first run of a script compiles it (about 80 ms). Later runs of the unchanged script reuse the cached binary and start in about **2 ms**. Editing the script, or upgrading Tarn, triggers a recompile automatically. The cache lives in `~/.cache/tarn`; set `TARN_CACHE_DIR` to move it.

## Variables: `val` and `var`

```python
val name = "Tarn"        # immutable; the type String is inferred
var count: Int = 0       # mutable; the type is written explicitly
count += 1
name = "other"           # error: cannot assign twice to immutable variable `name`
```

- Every variable must be initialized. A variable can only hold `null` if its type says so (`Int?`), see [Null safety](#null-safety).
- Declaring a name twice in the same block is an error, but an inner block may shadow an outer name.
- `_` is a throwaway name: `val _ = compute()` and `for _ in 0..3:` are fine, and `_` can be declared any number of times.

## Constants

A `val` at the top of a file, outside any function, is a constant:

```python
val max_players = 8
val tax_rate = 0.11
val title = "Tarn " + "Quest"            # constants can use operators...
val half = max_players / 2               # ...and other constants

fn main():
    print(f"{title}: up to {max_players} players")
```

- **Numbers, `Bool`s and `String`s only**, made of literals, operators and other constants (of the same module, or imported ones such as `math.pi`). They are worked out when compiling, so `val big = 9223372036854775807 + 1` is a compile error, and every use is replaced by the value: constants cost nothing at run time.
- **Order doesn't matter.** A constant can use one defined further down; one defined in terms of itself is an error.
- **Modules share them** like functions: `math.pi`, or `from math import pi`. A name starting with `_` is private.
- **There are no global variables.** A top-level `var` is an error: keep changing state in `main` (or a struct) and pass it along.

## Types

| Type | Notes |
|---|---|
| `Int` | 64-bit signed. Overflow **panics** instead of silently wrapping. |
| `Float` | 64-bit IEEE 754. Prints like Python: `3.0`, `0.1`, `1e+16`. |
| `Bool` | `true` / `false`. |
| `String` | Immutable UTF-8. `+` concatenates. |
| `List[T]` | Growable and bounds-checked. A reference type. |
| `Map[K, V]` | A hash map from keys to values, in insertion order. A reference type. See [Maps](#maps). |
| structs | User-defined. A reference type, like Python and Kotlin objects. |
| enums | User-defined: one of several variants, which may carry data. See [Enums and `match`](#enums-and-match). |
| `T?` | A `T` or `null`, e.g. `Int?`, `String?`, `User?`. See [Null safety](#null-safety). |
| generic types | Your own structs and enums with type parameters, written like `Box[Int]` or `Pair[String, Float]`. See [Generics](#generics). |

Lists, maps and structs are references: `val b = a` makes `b` point at the same list as `a`. Memory is managed for you, see [Memory](#memory).

## Strict typing

Tarn never converts between types for you:

```python
val a = 1 + 2.0              # error: `+` cannot be applied to `Int` and `Float`
val b = 1.to_float() + 2.0   # ok
if count:                    # error: condition must be a `Bool`. There's no "truthiness".
if count != 0:               # ok
print("n = " + 5)            # error. Write f"n = {5}" or "n = " + 5.to_string()
```

On top of that:
- every path through a function with a return type must `return`;
- `==` only works on `Int`, `Float`, `Bool`, `String`, enums without data and their nullable versions (where `null == null`), and on types that define [`fn ==`](#operator-functions). Tarn never compares references;
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

## Lambdas and function values

Functions are values. A function type is written `fn(Int, String) -> Bool`, or `fn(Int)` for one that returns nothing, and `(fn(Int) -> Int)?` is one that may be `null`.

```python
fn double(x: Int) -> Int:
    return x * 2

fn apply(f: fn(Int) -> Int, x: Int) -> Int:
    return f(x)

fn make_adder(n: Int) -> fn(Int) -> Int:
    return fn(x) -> x + n                 # a lambda: it returns its expression

fn main():
    print(apply(double, 21))              # 42: a named function, as a value
    print(apply(fn(x) -> x * x, 7))       # 49: `x` is an Int, because `apply` says so
    val add10 = make_adder(10)
    print(add10(5))                       # 15: `add10` kept the `n` it was made with

    val classify = fn(n: Int) -> String:  # a lambda with a block body ends its line
        if n < 0:
            return "negative"
        return "not negative"
    print(classify(-2))
```

- **Parameter types come from where a lambda is used.** `apply` takes an `fn(Int) -> Int`, so `x` is an `Int`. Where nothing says, write them: `val f = fn(x: Int) -> x * 2`.
- **One line, or a block.** A one-line lambda returns its expression. A longer one gets a block, with its return type if it returns something (`fn(x: Int) -> Int:`), and must end its line, as after `val f =`. Inside parentheses, use one-line lambdas, or define the function first.
- **A one-line lambda can be an assignment,** and then returns nothing: `fn(m) -> m[key] = value`, `fn(p) -> p.visits += 1`.
- **`val`s are copied, `var`s are shared.** A lambda keeps the `val`s, parameters and loop variables it uses as they are when it's created (each iteration of a loop captures its own). A `var` it captures is shared with the code around it: each sees what the other writes, even after the function that declared it returned (the variable lives in a small cell then). A `var` that a lambda assigns can't be smart-cast. Lists, maps and structs are shared as always.
- **Methods are values too:** `users.add` remembers `users`; `User.describe` takes the `User` first; `App.new` is a static method. A generic method can't be a value yet.
- **Named functions are values too**, including generic ones (`max[Int]`, or picked by the type expected: `val pick: fn(Int, Int) -> Int = max`) and module functions (`xs.map(math.sqrt)`).
- Calls go by position (`f(1, 2)`): a function type doesn't name its parameters. Functions can't be compared with `==` or printed.
- A lambda that captures nothing is a constant and allocates nothing. One that captures is a small reference-counted object, and cycles through closures (a node whose callback captures the node) are collected like any other.

### List methods that take functions

```python
val xs = [5, 3, 8, 1]
xs.map(fn(x) -> x * 2)                    # [10, 6, 16, 2]
xs.filter(fn(x) -> x > 2)                 # [5, 3, 8]
xs.any(fn(x) -> x > 7)                    # true, stopping at 8
xs.find(fn(x) -> x % 2 == 0)              # 8 (an Int?: null if there's none)
xs.fold(0, fn(sum, x) -> sum + x)         # 17
people.sort_by(fn(p) -> p.age)            # in place; equal ages keep their order
people.max_by(fn(p) -> p.score)           # a Person?, null for an empty list
```

| Method | |
|---|---|
| `map(f) -> List[U]` | A new list of `f` of each element |
| `filter(f) -> List[T]` | The elements for which `f` is true |
| `any(f)`, `all(f) -> Bool` | Is `f` true for some element, for every element? They stop as soon as they know |
| `find(f) -> T?` | The first element for which `f` is true, or `null` |
| `count(f) -> Int` | How many elements `f` is true for |
| `fold(initial, f) -> A` | Combine the elements into one value, starting from `initial`: `f(acc, x)` gives the next `acc` |
| `sort_by(key)` | Sort in place by each element's key (`Int`, `Float` or `String`), stably. Each key is computed once |
| `min_by(key)`, `max_by(key) -> T?` | The element with the smallest (largest) key, the first on a tie; `null` for an empty list |

**A lambda written right in the call is inlined**: its body runs in the method's own loop, so nothing is allocated for it and nothing is called. `xs.map(fn(x) -> x * 2).filter(...).fold(...)` runs as fast as the `for` loops you would write by hand. A named function is called directly, and any other function value through the value.

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
for i, x in my_list: # the index too: 0, 1, 2...
    ...
for k, v in my_map:  # a map's keys and values, in insertion order
    ...
for (a, b) in pairs: # each tuple, taken apart
    ...
for x in countdown:  # your own type, through its `next()`: see Iterators
    ...
```

`x in xs` and `x not in xs` test membership: a key of a map, an element of a list (compared with `==`), or a substring of a string.

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

Methods take `self` as their first parameter. A method without `self` is a **static method**, called on the type:

```python
struct Config:
    host: String = "localhost"     # a default value
    port: Int = 8080
    tags: List[String] = []        # computed again for each new value
    name: String

    fn local(name: String) -> Config:      # static: no `self`
        return Config(name=name)

val c = Config.local("api")         # host "localhost", port 8080
val d = Config(name="admin", port=9000)
```

- **Default values** let construction leave a field out (give the others by name). Each default is computed again for every value that needs it, so each `Config` gets its own list. Generic structs can have them: `items: List[T] = []`.
- **Static methods** work on enums and generic types too: `Color.parse("red")`, `Stack.empty()` or `Stack[Int].empty()`.

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
| `(x, 0)` | a [tuple](#tuples), element by element |
| `Circle(r)`, `Circle as c`, `Int as n` on an interface value | the type of the value inside it ([see below](#which-type-is-inside)) |
| `p \| q` | `p` or `q` (both must bind the same names) |
| `p as name` | `p`, and binds the whole value to `name` |

Floats can't be patterns: bind the value and use a guard, as in `case x if x > 0.5:`.

### `match` as a value

Where a value is expected (after `val x =`, `return`, `=`), a `match` gives the value of the case that runs: its expression, or the last line of its block. A case can also leave with `return`, `break`, `continue` or a panic.

```python
val label = match status:
    case 200: "ok"
    case 404: "not found"
    case code if code >= 500:
        log(code)
        "server error"
    case _: "other"

val port = match text.to_int_or_null():
    case null: return Err("not a number")
    case n: n
```

The cases agree on one type (`Int` and `null` make `Int?`), or each is checked against the type expected. Like a block lambda, it must end its line.

### Exhaustiveness

Every `match` must handle every value the type allows. The compiler checks, and names what's missing:

```
Error: this `match` doesn't handle every value: `Rect(_, _)` and `Empty` are not covered
   ╭─[ shapes.tr:7:11 ]
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

## Operator functions

A struct or enum gets operators by defining methods named after them:

```python
struct Vec2:
    x: Float
    y: Float

    fn +(self, other: Vec2) -> Vec2:
        return Vec2(self.x + other.x, self.y + other.y)

    fn *(self, k: Float) -> Vec2:          # vector * number
        return Vec2(self.x * k, self.y * k)

    fn *(self, other: Vec2) -> Float:      # vector * vector: the dot product
        return self.x * other.x + self.y * other.y

    fn -(self) -> Vec2:                    # no parameter: `-v`
        return Vec2(-self.x, -self.y)

    fn ==(self, other: Vec2) -> Bool:      # `==` and `!=`
        return self.x == other.x and self.y == other.y

    fn <=>(self, other: Vec2) -> Int:      # `<`, `<=`, `>`, `>=`: shorter vectors first
        return self * self <=> other * other

    fn to_string(self) -> String:          # for `print` and f-strings
        return f"({self.x}, {self.y})"

fn main():
    val a = Vec2(1.0, 2.0)
    var p = a + Vec2(3.0, 4.0) * 2.0
    p += a                                 # p = p + a
    print(p)                               # (8.0, 12.0)
    print(f"{-a} {a * p} {a < p}")         # (-1.0, -2.0) 32.0 true
```

| Define | To get |
|---|---|
| `fn +(self, other: T) -> R`, and `-`, `*`, `/`, `%` | `a + b`; also `a += b` when `R` is the type of `a` |
| `fn -(self) -> R` | `-a` |
| `fn ==(self, other: Self) -> Bool` | `a == b` and `a != b` |
| `fn <=>(self, other: Self) -> Int` | `a < b`, `a <= b`, `a > b`, `a >= b` and `a <=> b`. Return a negative number when `self` comes first, `0` when they're equal, a positive one when it comes after |
| `fn [](self, i: I) -> T` | `a[i]` |
| `fn []=(self, i: I, value: T)` | `a[i] = v`. With `fn []` too: `a[i] += v` |
| `fn to_string(self) -> String` | `print(a)` and `f"{a}"` (an ordinary method, not an operator) |

- **Overloading.** `+ - * / %`, `[]` and `[]=` can be defined several times with different parameter types: the type of the right operand (or index) picks one. So `time.DateTime - time.DateTime` is a [`time.Duration`](time.md#durations), and `time.DateTime - time.Duration` is a `DateTime`.
- **The left operand decides.** `v * 2.0` calls `Vec2`'s `fn *`, but `2.0 * v` is an error (the compiler suggests swapping them).
- **`<=>` also works on `Int`, `Float` and `String`**, giving `-1`, `0` or `1`, which keeps comparisons short: `return self.name <=> other.name`. It binds tighter than `==`, so `a <=> b == 0` does what it says.
- **`==` and `<=>` compare two values of the same type.** `fn ==` and `fn <=>` are defined once per type; there is no automatic `==` for structs.
- **They're methods.** They live inside a struct or enum, take `self`, and are public. `a + b` compiles to a direct call of `fn +`, so it costs what a method call costs (usually nothing, once inlined), and `acc = acc + v` reuses `acc`'s memory like `acc = acc.add(v)` would.
- **Not definable:** `and`, `or`, `not`, `!=` (the opposite of `==`) and `<`-style operators on their own (they all come from `<=>`).

## Generics

Functions, structs and enums can take type parameters, in brackets after their name:

```python
fn max[T](a: T, b: T) -> T:
    if a > b:
        return a
    return b

struct Stack[T]:
    var items: List[T]

    fn push(self, x: T):
        self.items.push(x)

    fn pop(self) -> T?:
        if self.items.len() == 0:
            return null
        return self.items.pop()

enum Tree[T]:
    Leaf
    Node(left: Tree[T], value: T, right: Tree[T])

    fn contains(self, x: T) -> Bool:
        match self:
            case Leaf:
                return false
            case Node(left, value, right):
                if x == value:
                    return true
                elif x < value:
                    return left.contains(x)
                return right.contains(x)

fn main():
    print(max(3, 7))                         # 7: here `T` is `Int`
    print(max("pear", "apple"))              # pear
    val s = Stack[String]([])                # or: val s: Stack[String] = Stack([])
    s.push("a")
    print(s.pop() ?: "empty")                # a
    val t = Tree.Node(Tree.Leaf, 5, Tree.Node(Tree.Leaf, 8, Tree.Leaf))
    print(t.contains(8))                     # true
```

### What a `T` can do

Operators need no bound. The compiler works out what each type parameter must support from what the code does with it. `max` compares two `T`s with `>`, so `max` works for any type with `<=>`, and only those:

| When generic code does | `T` must support | Which built-in types do |
|---|---|---|
| `a == b`, `a != b` | `==` | `Int`, `Float`, `Bool`, `String`, enums without data |
| `a < b`, `<=`, `>`, `>=`, `a <=> b` | `<=>` | `Int`, `Float`, `String` |
| `a + b`, `-`, `*`, `/`, `%` (also `total += x`) | that operator, giving a `T` | `Int`, `Float`; `String` for `+` |
| `-a` | `-` | `Int`, `Float` |
| `print(a)`, `f"{a}"`, `a.to_string()` | `to_string` | `Int`, `Float`, `Bool`, `String`, enums without data |
| `hash(a)` | `hash` | `Int`, `Float`, `Bool`, `String`, enums without data |
| `Map[T, V]` (a `T` as a map key) | `==` and `hash` | `Int`, `Bool`, `String`, enums without data |

Your own types qualify by defining the matching [operator functions](#operator-functions), `fn to_string(self) -> String` or `fn hash(self) -> Int`; for arithmetic, it's the one that takes and returns the type itself (`fn +(self, other: Vec2) -> Vec2`). Generic code can also store a `T` in lists and fields, return it, compare it with `null`, and pass it to other generic code, which passes on what *that* code needs. To call other methods on a `T`, give it a bound: an [interface](#interfaces) listing them (`fn total[T: Shape]`). A `T` never has fields: it could be any type.

Using a type that falls short is an error at the call, pointing at the line of generic code that needs more:

```text
Error: `max` needs `T` to support `<` and `>`, but `Point` doesn't
    ╭─[ main.tr:11:11 ]
 11 │     print(max(Point(1.0, 2.0), Point(3.0, 4.0)).x)
    │           ──────────────────┬──────────────────
    │                             ╰──────────────────── here `T` is `Point`
  2 │     if a > b:
    │        ──┬──
    │          ╰──── `T` values are compared here
    │ Help: give `struct Point` a method `fn <=>(self, other: Point) -> Int`
```

Hovering a generic function in the editor shows what each of its type parameters must support.

### Type arguments

- **Inferred from the arguments** (`max(3, 7)`, `Stack(["a"])`), or else from the type the result must have: `val xs: List[Int] = empty()`, `val t: Tree[Int] = Tree.Leaf`.
- **Or given explicitly**, in brackets: `max[Float](1.5, 2.5)`, `Stack[String]([])`, `Tree[Int].Leaf`, `geometry.Box[Int](5)`.
- **A generic type is always written with its arguments**: `Box[Int]`, `Pair[String, Float]`, and `Box[T]` inside its own methods. A `Box[Int]` is not a `Box[Int?]`: write `Box[Int?](5)` to get the latter.
- **Methods can have type parameters of their own**: `fn with_first[C](self, c: C) -> Pair[C, B]`.
- **A type argument can be nullable.** `T?` with `T` = `Int?` is just `Int?` (as in Kotlin), so `first([null, 2])` returns `null` either way.

### What it costs

Nothing at run time. Generic code is compiled separately for every set of types it's used with (*monomorphization*, as in Rust and C++): `max[Int]` and `max[String]` are two functions in the program, each exactly what you'd get by writing it out by hand, and a `Box[Int]` stores its `Int` directly, with no boxing and no type information at run time. The price is paid at compile time, in code size, for each type actually used.

## Interfaces

An interface lists methods. Any type that has them fits it, and nothing has to say so:

```python
interface Shape:
    fn area(self) -> Float
    fn name(self) -> String

struct Circle:
    r: Float

    fn area(self) -> Float:
        return 3.14159 * self.r * self.r

    fn name(self) -> String:
        return "circle"

struct Square:
    side: Float

    fn area(self) -> Float:
        return self.side * self.side

    fn name(self) -> String:
        return "square"
```

A type fits when it has every method of the interface, with the same parameters (names and types) and the same return type. `Circle` and `Square` both fit `Shape`. An interface is used in two ways.

### As a bound: `[T: Shape]`

A bound lets generic code call the interface's methods on a `T`:

```python
fn total[T: Shape](xs: List[T]) -> Float:
    var sum = 0.0
    for x in xs:
        sum += x.area()
    return sum

print(total([Circle(1.0), Circle(2.0)]))      # 15.70795
print(total([Square(3.0)]))                   # 9.0
```

Generic code is still compiled for each type it's used with, so `x.area()` is a direct call to `Circle.area`: a bound costs nothing at run time. A type parameter can have several bounds (`[T: Shape + Named]`), and generic types can have them too (`struct Gallery[T: Shape]`, whose methods can then call `area` on its items). Using a type that doesn't fit is an error at the call:

```text
Error: `total` needs `T` to be a `Shape`, but `Int` isn't
    ╭─[ main.tr:11:11 ]
 11 │     print(total([1, 2]))
    │           ──────┬──────
    │                 ╰──────── here `T` is `Int`
  4 │ fn total[T: Shape](xs: List[T]) -> Float:
    │             ──┬──
    │               ╰──── `T` must be a `Shape` here
    │ Note: `Int` has no method `area`
```

Generic code that passes a `T` on to `total` needs it to be a `Shape` too, as with [operators](#what-a-t-can-do); it doesn't have to declare the bound itself.

### As a type: `List[Shape]`

An interface is also a type. A `Shape` value can be a circle, a square or anything else that fits, so one list can hold them all:

```python
fn describe(s: Shape) -> String:
    return f"{s.name()}: {s.area()}"

fn main():
    val shapes: List[Shape] = [Circle(1.0), Square(2.0)]
    shapes.push(Square(3.0))
    for s in shapes:
        print(describe(s))     # circle: 3.14159, then square: 4.0, then square: 9.0
    print(total(shapes))       # 16.14159: here `T` is `Shape`
```

A value becomes a `Shape` wherever a `Shape` is expected: a typed `val`, a parameter, a field, a return value, a list or map of `Shape`s. `Shape?` works like any nullable type. Only the interface's methods can be used on a `Shape`: no fields, no operators, no `==`. To print one, the interface needs `fn to_string(self) -> String`.

A `List[Circle]` isn't a `List[Shape]`, since it couldn't hold squares: make the list a `List[Shape]` where it's created.

### Saying a type fits: `struct Circle(Shape):`

Nothing has to say that a type fits an interface, but it can, after its name: `struct Circle(Shape):`, `enum Tree[T](Container):`. The compiler then checks it there, so a mistake is reported at the type instead of where it's first used as a `Shape`:

```text
Error: `Sq` says it's a `Shape`, but `Sq` has no method `area`
```

### Including other interfaces

`interface Solid(Shape):` has `Shape`'s methods and its own:

```python
interface Solid(Shape):
    fn volume(self) -> Float
```

A `Solid` value can be used where a `Shape` is expected.

### Default methods

An interface method with a body is a **default method**: every type that fits gets it, unless it has its own method of that name. Inside, `self` is a `Self`, the type it runs for.

```python
interface Shape:
    fn area(self) -> Float
    fn name(self) -> String

    fn describe(self) -> String:
        return f"{self.name()} of area {self.area()}"

struct Circle(Shape):           # saying it fits gives it `describe` as a method
    ...

print(Circle(1.0).describe())
```

A type gets the defaults of the interfaces it says it fits as methods of its own; through a bound or an interface value, every type that fits gets them. Its own method wins, also through an interface value, except for a default with type parameters of its own (`map[U]`), which always runs the interface's code there.

### Generic interfaces

```python
interface Source[T]:
    fn next(self) -> T?

fn total[S: Source[Int]](s: S) -> Int: ...
fn first[T, S: Source[T]](s: S) -> T?:     # `T` comes from how the type fits `Source[T]`
    return s.next()
```

### Which type is inside

`match` can test which type an interface value holds, and take it apart:

```python
fn describe(s: Shape) -> String:
    return match s:
        case Circle(r): f"circle {r}"
        case Square as sq: f"square {sq.side}"
        case _: "something else"           # needed: any type that fits could be inside
```

Built-in types (`case Int as n:`) and enum variants work too; generic types and other interfaces can't be tested for.

### Built-in types

Built-in types fit interfaces whose methods take no parameters, when they have those methods. `Int`, `Float`, `Bool`, `String` and enums without data all have `to_string`:

```python
interface Named:
    fn to_string(self) -> String

val things: List[Named] = [42, "hello", true, Color.Green, Point(1, 2)]
for t in things:
    print(t)                   # 42, hello, true, Green, (1, 2)
```

### What it costs

A bound costs nothing. An interface value is a small box holding the value and a pointer to the table of its type's methods: making one is an allocation, and a call goes through the table (a few nanoseconds, like a virtual call in Java or Go). In a hot loop over values of one type, a bound is faster.

## Iterators

`for x in value` works on any value whose type has `fn next(self) -> T?` (the loop calls it until `null`), or `fn iter(self)` giving such an iterator.

```python
struct Countdown(Iterator[Int]):
    var left: Int

    fn next(self) -> Int?:
        if self.left == 0:
            return null
        self.left -= 1
        return self.left + 1

for n in Countdown(3):
    print(n)                                                    # 3, 2, 1
print(Countdown(6).filter(fn(n) -> n % 2 == 0).to_list())      # [6, 4, 2]
```

Saying `(Iterator[Int])` gives the prelude's default methods: lazy `map`, `filter`, `take`, `enumerate` (pairs `(i, x)`), and `to_list`, `count`, `fold`, `any`, `all`, `find`. A `for` loop over an iterator is a direct call per value and allocates nothing; each adapter is one object for the whole chain. An iterator of `T?` stops at its first `null`.

## Lists

```python
val xs = [3, 1, 2]                   # List[Int]. `val` still allows push and xs[0] = ...
val empty: List[String] = []         # an empty list needs a type annotation
xs.push(4)
xs[0] = 10
val last = xs.pop()
print(xs.len())
print(xs)                            # [10, 1, 2]
print(2 in xs)                       # true
xs.sort()                            # [1, 2, 10]
xs[99]                               # panic: index 99 out of bounds for list of length 3
```

| List method | |
|---|---|
| `len() -> Int`, `is_empty() -> Bool` | Number of elements; are there none? |
| `push(value)` | Append to the end |
| `pop() -> T` | Remove and return the last element (panics if the list is empty) |
| `insert(i, value)` | Insert at index `i` (`0` to `len()`), moving the rest up |
| `remove_at(i) -> T` | Remove and return the element at `i`, moving the rest down |
| `clear()` | Remove every element |
| `contains(x) -> Bool` | Is `x` in the list? The same as `x in xs`. Elements are compared with `==`, so they need it (a struct needs [`fn ==`](#operator-functions)) |
| `index_of(x) -> Int?` | The index of the first element equal to `x`, or `null` |
| `reverse()` | Reverse the order, in place |
| `sort()` | Sort in place, smallest first. For lists of `Int`, `Float` and `String` |
| `sort_by(key)`, `map(f)`, `filter(f)`... | The methods that [take a function](#list-methods-that-take-functions) |
| `join(sep) -> String` | For a `List[String]`: the strings with `sep` between them. `["a", "b"].join(", ")` is `"a, b"` |
| `to_string() -> String` | The list as you'd write it, `[1, 2, 3]`. `print(xs)` and `f"{xs}"` use it |

A list prints the way you'd write it in code: `[1, 2, 3]`, `["a", "b"]` (strings quoted), `[[1], []]`, `[1, null]`. Any list whose elements can be printed can be printed.

## Maps

A `Map[K, V]` maps keys of type `K` to values of type `V`, like a Python `dict` or a Kotlin `MutableMap`:

```python
val ages = {"ana": 31, "budi": 25}       # Map[String, Int]
val empty: Map[String, Int] = {}         # an empty map needs a type annotation
ages["citra"] = 40                       # add, or replace
ages["ana"] += 1                         # update (panics if the key is missing)
print(ages["budi"])                      # 25 (panics if the key is missing)
print(ages.get("dewi"))                  # null: `get` gives a `V?`
print(ages.get("dewi", 0))               # 0: or a default
print("ana" in ages)                     # true
ages.remove("budi")
for name, age in ages:                   # in insertion order
    print(f"{name} is {age}")
print(ages)                              # {"ana": 32, "citra": 40}
```

Counting is the classic use:

```python
val counts: Map[String, Int] = {}
for word in text.split():
    counts[word] = counts.get(word, 0) + 1
```

| Map method | |
|---|---|
| `m[k]` | The value for `k`. Panics if `k` isn't in the map: `key "dewi" not found in map` |
| `m[k] = v` | Set the value for `k` (adding the key if it's new). `m[k] += v` updates an existing one |
| `get(k) -> V?` | The value, or `null` if `k` isn't there |
| `get(k, default) -> V` | The value, or `default` if `k` isn't there |
| `contains(k) -> Bool` | Is `k` in the map? The same as `k in m` |
| `remove(k) -> V?` | Remove `k`, returning its value (`null` if it wasn't there) |
| `len() -> Int`, `is_empty() -> Bool` | Number of entries; are there none? |
| `keys() -> List[K]`, `values() -> List[V]` | New lists, in insertion order |
| `clear()` | Remove every entry |
| `to_string() -> String` | The map as you'd write it, `{"a": 1}`. `print(m)` and `f"{m}"` use it |

- **Insertion order.** Looping over a map, `keys()`, `values()` and printing all go in the order the keys were first added, as in Python. Replacing a value keeps the key's place; removing a key and adding it again moves it to the end.
- **Looping.** `for k in m:` gives the keys, `for k, v in m:` the keys and values. Adding or removing keys inside the loop panics with `map changed size during iteration`; changing the values of existing keys is fine.
- **Keys** can be `Int`, `Bool`, `String`, enums without data, and your own structs and enums that define `fn ==` and `fn hash`. `hash(...)` combines the hashes of the fields that `==` compares:

  ```python
  struct Point:
      x: Int
      y: Int

      fn ==(self, other: Point) -> Bool:
          return self.x == other.x and self.y == other.y

      fn hash(self) -> Int:
          return hash(self.x, self.y)

  val grid = {Point(0, 0): "start", Point(3, 4): "goal"}
  ```

  `Float` keys are refused (`0.1 + 0.2 != 0.3`, and NaN isn't equal to itself), and so are nullable keys and lists or maps as keys.
- **Speed.** Maps are hash tables in the runtime, the same design as Python's dicts: entries are kept in an array, in order, with an index of open-addressing slots to find them. Keys and values are stored inline (an `Int` value is 8 bytes, no boxing), lookups with `Int` and `String` keys don't call back into your code, and an update right after a lookup of the same key (`m[k] = m.get(k, 0) + 1`, `m[k] += 1`) doesn't search again.

## Tuples

```python
fn divmod(a: Int, b: Int) -> (Int, Int):
    return (a / b, a % b)

val (q, r) = divmod(17, 5)            # take it apart (`_` skips one; `var` works too)
val t = (1, "one")
print(f"{t.0 + 1} {t.1}")             # elements by position
for (name, n) in [("a", 1), ("b", 2)]: ...
val grid: Map[(Int, Int), String] = {(0, 0): "start"}
match pair:
    case (0, y): ...
```

A tuple has two to six elements and can't be changed. Tuples compare element by element (`==`, `<`, `<=>`), print as `(1, "a")`, and are map keys when their elements can be. Each is a small immutable object (one allocation), like a struct without a name.

## Strings and f-strings

```python
val s = f"pi is about {3.14159}, 2 + 2 = {2 + 2}, ok = {true}"
"abc" < "abd"            # comparison is lexicographic
"abc" <=> "abd"          # -1 (see operator functions)
" 42 ".to_int()          # 42. Invalid input panics, like Kotlin's toInt().
"2.5".to_float()
for c in "héllo":       # each character, as a one-character string
    print(c)
```

| String method | |
|---|---|
| `len() -> Int`, `is_empty() -> Bool` | Length in bytes; is it `""`? |
| `chars() -> List[String]` | Each character as a one-character string |
| `char_count() -> Int` | The number of characters (`len()` counts bytes) |
| `encode() -> Bytes` | The UTF-8 bytes, shared rather than copied |
| `lines() -> List[String]` | The lines, without their `\n` or `\r\n` |
| `split(sep) -> List[String]` | The parts between each `sep`: `"a,b,,c".split(",")` is `["a", "b", "", "c"]` |
| `split() -> List[String]` | The words: split on runs of whitespace, with no empty parts |
| `substring(start, end) -> String` | Bytes `start..end`. Panics if out of range or splitting a character |
| `contains(s)`, `starts_with(s)`, `ends_with(s)` | `-> Bool`. `s in text` is the same as `text.contains(s)` |
| `find(s) -> Int?`, `rfind(s) -> Int?` | The byte index of the first (last) `s`, or `null` |
| `count(s) -> Int` | How many times `s` occurs, without overlapping |
| `replace(old, new) -> String` | Every `old` replaced by `new` |
| `trim()`, `trim_start()`, `trim_end()` | Without whitespace at both ends, the start, or the end |
| `upper()`, `lower()` | In upper or lower case (ASCII letters) |
| `repeat(n) -> String` | `n` copies: `"ab".repeat(3)` is `"ababab"` |
| `pad_start(width)`, `pad_end(width)` | At least `width` characters, adding spaces (or a given character: `"7".pad_start(3, "0")` is `"007"`) |
| `to_int()`, `to_float()`, `to_bool()`, `to_string()` | Conversions. Parsing panics on bad input. `to_bool()` takes `true`/`yes`/`on`/`1` and `false`/`no`/`off`/`0`, in any case |
| `to_int_or_null()`, `to_float_or_null()`, `to_bool_or_null()` | `-> Int?` / `-> Float?` / `-> Bool?`: `null` instead of a panic on bad input |

`Int`, `Float` and `Bool` have `.to_string()`; `Int` has `.to_float()`; `Float` has `.to_int()`, which truncates.

**Bytes and characters.** Lengths and indexes count bytes: they're fast, and what parsing needs. `for c in s` (and `for i, c in s`) goes through the characters, and `char_count()` counts them. There's no `s[i]`, so a byte index and a character index can't be mixed up.

**Building strings.** `s += x` appends in place when nothing else holds `s`, and `s += f"..."` appends each part of the f-string in turn: building a long string in a loop is linear, with no builder type needed.

## Bytes

`Bytes` is raw data, numbers from 0 to 255: what sockets and files carry. `s.encode()` gives a string's UTF-8 bytes and `b.decode()` turns valid UTF-8 back into text; neither copies.

```python
val request = "GET / HTTP/1.1\r\n\r\n".encode()
print(request[0])                              # 71
val end = request.find("\r\n".encode())!!
print(request.slice(0, end).decode())          # GET / HTTP/1.1
var buf = Bytes()                              # or Bytes([72, 105])
buf += request                                 # appends in place
print(Bytes([104, 255]).decode_or_null())      # null: not UTF-8
```

| Method | |
|---|---|
| `len()`, `is_empty()`, `b[i]` | The number of bytes; the byte at `i` (an `Int`, bounds-checked) |
| `slice(start, end) -> Bytes` | The bytes `start..end`, copied |
| `find`, `rfind`, `contains`, `count`, `starts_with`, `ends_with` | Search, with `Bytes` |
| `split(sep)`, `replace(old, new)`, `trim()` | As for strings |
| `decode()`, `decode_or_null()`, `decode_lossy()` | As text: panics, `null`, or `\u{FFFD}` for bytes that aren't UTF-8 |
| `hex()`, `to_list()`, `to_string()` | `"48690a"`, `[72, 105, 10]`, `b"Hi\n"` |

`==`, `!=`, `+` and `for byte in b` work as you'd expect. `Bytes` can't be map keys yet.

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
   ╭─[ app.tr:9:11 ]
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

## Recoverable errors

A function that can fail in an expected way returns a `Result[T, E]`: `Ok(value)` or `Err(error)`. `Result`, `Ok`, `Err` and `Unit` are always available, without an import (they live in the prelude, [`stdlib/prelude.tr`](../stdlib/prelude.tr)).

```python
fn parse_port(text: String) -> Result[Int, String]:
    val n = text.to_int_or_null() ?: return Err(f"not a number: {text}")
    if n < 1 or n > 65535:
        return Err(f"out of range: {n}")
    return Ok(n)

fn parse_address(text: String) -> Result[String, String]:
    val colon = text.find(":") ?: return Err("no port")
    val port = parse_port(text.substring(colon + 1, text.len()))?   # the Err, returned
    return Ok(f"{text.substring(0, colon)} on {port}")

fn main():
    match parse_address("example.com:https"):
        case Ok(a):
            print(a)
        case Err(why):
            print(f"bad address: {why}")    # bad address: not a number: https
```

- **`?`** gives the `Ok`'s value, or returns the `Err` from the current function, which must return a `Result`. When the function's error type is an [interface](#interfaces) the error fits, it's boxed into it, so one function can pass on several kinds of error. On a `T?` in a function returning a nullable type, `x?` returns `null`.
- **`r?.member`** is `(r?).member`.
- **`r!!`** is the value, or a panic that shows the error.
- **`Result[Unit, E]`** is for something that can fail but gives nothing back: `return Ok()`.
- **Methods:** `is_ok()`, `is_err()`, `value_or(default)`, `value_or_null()`, `error_or_null()`, `map(f)`, `map_err(f)`, `to_string()`.
- **A `Result` that's thrown away** (`save(text)` as a statement) gets a warning. Write `val _ = save(text)` when that's really what you mean.

Use a panic for bugs, `null` for "there isn't one", and a `Result` for "it didn't work, and here's why".

## Memory

Tarn frees memory automatically, without a garbage collector and without pauses. Every string, list and struct counts its references, and it is freed the moment the last one goes away. The compiler works out each value's **last use**, so a value dies right after it, not at the end of the block:

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
$ tarn run --arc-stats bench/particles.tr
5000000
-12497502500
tarn arc: 3 allocations, 3 frees, 0 live, peak 96 bytes, 0 cycle collections freed 0
```

`tarn build app.tr --emit-mir` writes the mid-level IR, with every `dup` (a new reference), `drop` (a reference released) and `drop-reuse` (memory recycled in place) the compiler inserted.

## Cleaning up: `fn drop`

A struct that holds something besides memory (a socket, a file, a lock) can release it in `fn drop(self)`, which runs by itself when the last reference goes away:

```python
struct TempFile:
    path: String

    fn drop(self):
        fs.remove(self.path)
```

- **A variable keeps such a value until the end of its block** (or a `return`, `break` or `continue` that leaves it), like C++ and Rust, not just until its last use as with plain memory. Assigning a new value to a `var` drops the old one at once.
- `self` is borrowed inside `drop`. A `drop` that keeps a reference to `self` panics.
- Structs can have one, generic ones too (`struct Guard[T]:`), but enums can't. It can't be called (write a `close()` for that), and the type can't be part of a reference cycle (the compiler checks).
- `drop`s run when a [task](#tasks) panics and unwinds, but not when the program panics or calls `exit`.

The `net` module's sockets close themselves this way.

## Tasks

`spawn` runs a function alongside the code that started it, as a **task**. Tasks are light (a few microseconds to start, a stack that only takes memory as it grows), so a program can run thousands of them, and they run on every core. There's no `async`/`await`: a task that waits (for a socket, a channel, a lock, `time.sleep_ms`, another task) steps aside so others run, and code reads the same as code that never waits.

```python
fn fib(n: Int) -> Int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

fn main():
    val tasks = [25, 27, 30].map(fn(n) -> spawn(fn() -> fib(n)))
    for t in tasks:
        print(t.join()!!)            # 75025, 196418, 832040
```

- `spawn(f)` takes a function without parameters and gives a `Task[T]`, where `T` is what `f` returns (`Unit` when it returns nothing).
- `t.join()` waits for the task and gives a `Result[T, Panic]`: `Ok` with what it returned, or `Err` with the [panic](#panics-in-tasks) that stopped it. A task's result is given once: a second `join()` panics. `t.is_done()` asks without waiting.
- The program ends when `main` and every task it started have finished, joined or not. `exit(code)` still ends it at once.

### What a task gets

**Tasks share no variables.** What a task's function uses is given to the task: *moved* if nothing else uses it any more, or *copied* (deeply, keeping shared parts shared) if the code that started it still does. Either way, the two tasks never see each other's changes, so there's nothing to race on:

```python
val xs = [1, 2, 3]
val t = spawn(fn() -> xs.len())   # a copy: `xs` is used below
xs.push(4)                        # doesn't affect the task's copy
```

So a function given to `spawn` can't use a `var` from around it (the compiler says so). To share state between tasks, use a [`Mutex`](#mutex-and-atomic) or an `Atomic`, or send values through a [channel](#channels); those are the only values tasks share. A value with a [`fn drop`](#cleaning-up-fn-drop) (a socket, say) can be moved to a task, but not copied: give it away on its last use.

### Channels

A `Channel[T]` carries values from some tasks to others, in order:

```python
val jobs: Channel[Int] = Channel.buffered(100)
val worker = spawn(fn() -> jobs.map(fn(n) -> n * n).fold(0, fn(a, n) -> a + n))
for n in 1..=10:
    jobs.send(n)
jobs.close()
print(worker.join()!!)                # 385
```

- `Channel[T]()` is unbuffered: each `send` waits until a `receive` takes the value. `Channel.buffered(n)` holds up to `n` values before `send` waits.
- `receive()` waits for the next value and gives `T?`: `null` once the channel is closed and empty. A channel is an [iterator](#iterators), so `for job in jobs:` and `map`, `filter`, `fold`... take values until it's closed.
- `close()` says no more values are coming; `send` on a closed channel panics. Also `is_closed()` and `len()` (values waiting).
- A value sent is moved, or copied if the sender still uses it, like what a task gets.

### Mutex and Atomic

A `Mutex[T]` holds a value that tasks change one at a time. A task holds the Mutex while a function runs on its value:

```python
val hits: Mutex[Map[String, Int]] = Mutex({})
hits.lock(fn(m) -> m["home"] = m.get("home", 0) + 1)
print(hits.lock(fn(m) -> m.len()))    # 1
```

- `lock(f)` runs `f` on the value while holding it, and gives what `f` returns (moved or copied out, so no task keeps a hold on the value after `lock`). Other tasks that want it wait their turn, first come first served.
- `update(f)` replaces the value with what `f` makes of it: `counter.update(fn(n) -> n + 1)`. `get()` gives a copy, `set(v)` replaces it.
- Locking a Mutex again inside its own `lock` would wait forever, so it panics instead. The function given to `lock` or `update` can't use a `var` from around it.

An `Atomic` is an `Int` that tasks read and change at once, without a lock: `get()`, `set(v)`, `add(n)` (gives the new value), `swap(v)` (gives the old one) and `compare_and_set(expected, v)`.

### Panics in tasks

A panic stops only its own task. The task **unwinds**: every function on its stack releases what it holds and runs its values' `fn drop`s, innermost first, and any Mutex it held is let go. `join()` then gives `Err(Panic)`, with the `message` and the `location` (`app.tr:12:9`):

```python
val t = spawn(fn() -> [1, 2, 3][9])
match t.join():
    case Ok(v): print(v)
    case Err(p): print(f"failed: {p}")   # failed: index 9 out of bounds for list of length 3 (at app.tr:1:23)
```

The panic's message is also printed (`panic in task 2 at app.tr:1:23: ...`). A panic in `main` still ends the program, with exit code 101, as does a panic inside a `fn drop`.

### How it runs

- One worker thread per core (`TARN_THREADS=n` to choose), each with a queue of tasks; idle workers take tasks from busy ones. `main` stays on the thread it started on. Waiting on sockets and timers goes through the operating system's event queue (epoll on Linux, kqueue on macOS).
- A task runs until it waits. Scheduling is cooperative: a task that computes for a long time without waiting keeps its core (the others go on). `fs` calls wait on their thread.
- Each task has 1 MiB of stack (`TARN_TASK_STACK=bytes` for more). Recursing deeper stops the program with `stack overflow in task N`.
- If every task is waiting and nothing can wake any of them, the program stops with `deadlock: every task is waiting...` (exit code 101).
- Starting and joining a task takes about 3 µs; a value through a channel about 100 ns; a `Mutex.update` nobody else wants about 25 ns. A program that never calls `spawn` runs exactly as before: no threads, and none of this costs anything.

## Compile-time code

Code can run while the program compiles: to look at a type's fields, to read attributes written on declarations, and to compute values and reject mistakes before the program ever runs. There's no type information at run time: what the program needs of it is worked out while compiling and put in as plain values, so this code costs nothing when the program runs.

### Reflection: `fields(T)` and `comptime for`

`fields(T)` lists the fields of struct `T`, as `Field`s (with a `name`, a `type_name`, and whether it's `mutable` and `has_default`). A `comptime for` repeats its body once per field, while compiling, and each copy knows which field it's about, so `f.get(value)` reads that field with its own type:

```python
struct User:
    id: Int
    name: String

fn describe[T](value: T) -> String:
    val parts: List[String] = []
    comptime for f in fields(T):
        parts.push(f"{f.name}={f.get(value)}")
    return f"{type_name(T)}({parts.join(", ")})"

fn main():
    print(describe(User(id = 7, name = "Ada")))     # User(id=7, name=Ada)
```

- `comptime for i, f in fields(T)` also gives the index; `break` and `continue` work as in any loop.
- `type_name(T)` is the type's name as Tarn writes it (`List[Int]`, `User`).
- A plain `for f in fields(User)` runs when the program runs, over plain `Field` values: it can print their names, but `f.get(x)` needs to know the field while compiling, so it only works in a `comptime for`.

This is most useful in generic code. A generic function is compiled again for each type it's used with (see [generics](#generics)), and that's where `fields(T)` gets its answer: `describe` above becomes one function per type, with the loop unrolled. In the code as written, the parts that depend on what `T` is (the body of a `comptime for` over `fields(T)`, a `comptime if` or `comptime match` on `T`) are checked in each copy instead, so a mistake there is reported where the generic code is used, with a note saying which copy.

### `comptime match` on a type

`comptime match T:` keeps only the case that fits the type, in each copy of the code:

```python
fn encode[T](v: T) -> String:
    comptime match T:
        case Int | Float | Bool:
            return v.to_string()
        case String:
            return f"\"{v}\""
        case U?:                         # `U`: the type inside, in this case
            if v == null:
                return "null"
            return encode[U](v!!)
        case List[_]:
            val parts: List[String] = []
            for x in v:
                parts.push(encode(x))
            return "[" + parts.join(",") + "]"
        case _:
            compile_error(f"can't encode a `{type_name(T)}`")
```

A case is a type, `_` for any type, and `|` between alternatives: `Int`, `List[_]`, `Map[String, _]`, `_?` (any nullable type), `(_, _)` (a pair), `Box[_]` (a generic type with any type arguments), an interface (any type that fits it), or `_`. A single capital letter that isn't a type here (`U`, `E`, `K2`) matches any type and names it inside the case: `case List[E]:`, `case Map[K, V]:`. If no case fits, it's a compile error.

### Deriving methods

An interface's [default method](#default-methods) is compiled for each type that fits the interface, with `Self` that type, so it can loop over `fields(Self)`. That's how a library gives types methods written once, for all of them:

```python
interface Json:
    fn to_json(self) -> String:
        val parts: List[String] = []
        comptime for f in fields(Self):
            parts.push(f"\"{f.name}\":{encode(f.get(self))}")
        return "{" + parts.join(",") + "}"

struct Point(Json):
    x: Int
    y: Int

fn main():
    print(Point(x = 1, y = 2).to_json())        # {"x":1,"y":2}
```

### Building values: spreads

An argument written `expr for f in list` stands for one argument per element of a list known while compiling. With `fields(T)`, it builds a `T` field by field; each `Field` gives the argument for the field of its name:

```python
fn decode[T](row: Map[String, String]) -> Result[T, String]:
    return Ok(T(read(row[f.name])? for f in fields(T)))
```

- Each value is checked against its field's type, so `read` above is called with the right type each time (`read[Int]`, `read[String]`...). `?` returns from the function, as anywhere.
- `if cond` keeps some elements: `for f in fields(T) if not f.has(skip)`. The fields left out take their default values (it's an error if one has none), or can be given by name next to the spread.
- `T(...)` builds the struct `T` stands for, in each copy of generic code.
- With `params(H)`, it calls a function with one argument per parameter (see [functions as types](#functions-as-types-h-function)).

### `comptime val`, `comptime if`, `comptime fn`

The compiler runs ordinary Tarn code: your functions, the standard library's string, list and map functions, lambdas, `match`, loops. It gives exactly what the compiled program would, including panics, which become compile errors.

```python
fn primes(n: Int) -> List[Int]:
    val out: List[Int] = []
    var k = 2
    while out.len() < n:
        if out.all(fn(p) -> k % p != 0):
            out.push(k)
        k += 1
    return out

fn main():
    comptime val ps = primes(10)       # worked out while compiling
    print(ps)                          # [2, 3, 5, 7, 11, 13, 17, 19, 23, 29]
    comptime if ps.len() > 5:          # only this branch is compiled
        print("many")
```

- `comptime val name = expr`: computed while compiling. Code that runs reads it like any `val`; it then gets the value as a constant (numbers, strings, lists, maps, structs and enums of those; not a function).
- `comptime if cond:` (with `elif` and `else`): the condition is worked out while compiling and only the branch it picks is compiled. The others aren't even checked, so they can use what only exists for some types.
- `comptime fn f(...)`: a function that only runs while compiling. It can take a `Field` or `Parameter` and read its attributes. Called from code that runs, it's run while compiling (its arguments must be known then) and its result is put in.
- Compile-time code can use literals, constants, `comptime val`s, the variables of `comptime for`, and what functions compute from those. It can't touch files, the network, the environment, the clock or the screen, or start tasks.
- An endless loop is stopped after 20 million steps, with an error.

### Attributes

An attribute is declared like a small struct, and written with `@` on the lines above a function, method, struct, enum or field, or before a parameter's name:

```python
attribute get(path: String)
attribute column(name: String = "", indexed: Bool = false)
attribute skip

struct User:
    @column("user_id", indexed = true)
    id: Int
    @skip
    cache: List[Int] = []

@get("/users/{id}")
fn show_user(id: Int, @skip verbose: Bool) -> String:
    ...
```

- Its arguments are checked like a struct's (by position or name, with defaults), and worked out while compiling. Its fields can be `Int`, `Float`, `Bool`, `String`, enums without data, and `T?` or `List[T]` of those.
- An attribute that isn't declared is an error, and so is writing the same one twice on one declaration. From another module: `from routes import get`, or `@routes.get(...)`.
- Reading them: `attribute(X, get)` for a type or function `X` (a `get?`: `null` if it isn't there), `f.attribute(column)` and `f.has(skip)` for a `Field` or `Parameter`. Compile-time code reads them; code that runs gets the value as a constant.
- An attribute does nothing by itself: it's there for libraries to read.

### Functions as types: `[H: Function]`

A type parameter bounded by `Function` stands for one particular named function. Each function passed gets a copy of the generic code, which can read its parameters (`params(H)`, as `Parameter`s) and attributes, and calls it directly:

```python
fn route[H: Function](handler: H) -> String:
    comptime val r = attribute(H, get) ?: compile_error(f"`{type_name(H)}` needs `@get`")
    comptime for name in path_params(r.path):
        comptime if not params(H).any(fn(p) -> p.name == name):
            compile_error(f"`{type_name(H)}` has no parameter `{name}`", at = r)
    return r.path
```

`route(show_user)` passes the function by name (top-level functions and static methods; not lambdas, methods of a value or generic functions). `handler(arg(p) for p in params(H))` calls it with one argument per parameter.

### `compile_error`

`compile_error(message)` stops compiling with an error. In code compiled only in some cases (the branch a `comptime if` picks, a `comptime match` case), it's reported when that code is compiled; in compile-time code (a `comptime val`, a `comptime fn`), when it runs. It's reported where the program's own code uses the library that raised it, or with `at = x` at an attribute, `Field` or `Parameter`:

```
error: `show_user` has no parameter `slug`
  @get("/users/{id}/posts/{slug}")
  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ here
```

### What it costs

Nothing at run time: a `comptime for` becomes straight-line code, and reflection-based code runs exactly as fast as the same code written by hand. While compiling, the interpreter runs about 50 million steps a second.

## Modules: `import`

Every `.tr` file is a module. A program is one file plus everything it imports.

### Your own files

```
app/
├── main.tr
├── geometry.tr
└── shapes/
    ├── registry.tr
    └── util.tr
```

```python
# main.tr
import geometry                          # geometry.tr, next to this file
import geometry as geo                   # the same module, under another name
import shapes.registry                   # shapes/registry.tr, bound as `registry`
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
# shapes/registry.tr
from ..geometry import Shape             # `..` is the folder above this file
from . import util                       # `.` is this file's folder
```

- **Where modules are found.** `import a.b` reads `a/b.tr`, starting from the folder of the file that contains the `import`; every file finds its imports the same way, whichever file you run. `from .x` and `from ..x` (`from .. import x`) start from this folder or the one above.
- **What a plain `import` binds.** The last part of the path: `import shapes.registry` gives you `registry.count()`. With `as`, the name you choose.
- **What a module contains.** Functions, structs and enums. It doesn't need a `main`: `tarn check` and the editor check it like any other file, and only `tarn run` / `tarn build` ask for an entry point. If it has a `fn main()`, that only runs when you run the file itself, so a module can carry its own demo or tests.
- **Privacy.** Names starting with `_` are private to their module: functions, types, methods and fields. A struct with private fields can only be created inside its module, so a module can guarantee its structs are valid.
- **Cycles are fine.** Two modules may import each other, and a module may import the file you run. Modules contain only declarations, so there is no order of initialization to get wrong.
- **Your files come first.** A `time.tr` next to your file is imported instead of the standard library's `time`.
- **Errors show where they are.** A type error in `shapes/registry.tr` is reported in that file. In VS Code, it also shows on the `import` line that leads to it.
- **One program, one binary.** Everything is compiled together into a single native executable, and `tarn app.tr` rebuilds when any of its files changes.

`import *` is not supported: name what you use, or import the module.

### The standard library

```python
import time

fn main():
    val now = time.now()        # module functions: module.function(...)
    val t: time.DateTime = now  # module types: module.Type
```

- **Written in Tarn.** Standard-library modules live in [`stdlib/`](../stdlib) and are compiled together with your program.
- **Construction.** A standard-library struct can only be created through its module's functions, which validate the values. You can't build a `DateTime` for February 30.
- **Panic locations.** A panic inside a standard-library function is reported at *your* line that called it, like Rust's `#[track_caller]`. (A panic inside your own module is reported where it happened.)
  ```
  panic at app.tr:4:11: invalid date 2023-02-29: February 2023 has 28 days
  ```

Available modules:

| Module | |
|---|---|
| [`time`](time.md) | UTC dates and times, durations, clocks |
| [`math`](math.md) | Constants (`pi`, `e`, `inf`), powers and logarithms, trigonometry, rounding, `min`/`max`/`clamp`, `gcd` |
| [`fs`](fs.md) | Reading and writing files, folders, paths |
| [`net`](net.md) | TCP sockets |
| [`env`](system.md#env) | Environment variables, and `.env` files |
| [`signal`](system.md#signal) | Catching signals |
| [`url`](system.md#url) | Percent-encoding and query strings |

`Result`, `Ok`, `Err` and `Unit` come from the prelude, which every file has without importing it.

## Built-in functions

| Function | |
|---|---|
| `print(x)` | Print an `Int`, `Float`, `Bool`, `String`, enum without data (its variant's name), a value whose type has a `fn to_string(self) -> String` method, or a list or map of those, followed by a newline. A nullable one prints `null` when it is null |
| `input() -> String` | Read one line from stdin, without the newline |
| `args() -> List[String]` | Command-line arguments passed to the program |
| `sqrt(x: Float) -> Float` | Square root |
| `assert(cond: Bool)` | Panic if `cond` is false |
| `panic(msg: String)` | Stop the program with a message |
| `exit(code: Int)` | Exit the process |
| `hash(a, b, ...) -> Int` | A hash of the values, mixed into one `Int` (for [`fn hash`](#maps)). Works on numbers, `Bool`, `String`, enums, nullable values, and types with `fn hash(self) -> Int` |
| `spawn(f: fn() -> T) -> Task[T]` | Start a [task](#tasks) running `f` |
| `fields(T) -> List[Field]` | The fields of struct `T`, while compiling (see [compile-time code](#compile-time-code)) |
| `params(H) -> List[Parameter]` | The parameters of function `H` |
| `attribute(X, a) -> a?` | Attribute `@a(...)` on type or function `X`, or `null` |
| `type_name(T) -> String` | The name of type `T` as Tarn writes it, or of function `H` |
| `compile_error(msg, at = x)` | Stop compiling with an error |

## Panics

Runtime errors print where they happened and exit with code 101:

```
panic at crash.tr:5:15: index 3 out of bounds for list of length 3
```

Panics cover:
- integer overflow and division by zero;
- out-of-bounds indexing, `pop()` on an empty list, `insert`/`remove_at` out of range;
- `m[k]` with a key that isn't in the map, and adding or removing keys while looping over the map;
- a failed `assert`, a failed parse (`"abc".to_int()`), and `panic(...)`;
- `x!!` when `x` is null;
- `join()` twice on a task, `send` on a closed channel, locking a `Mutex` inside its own `lock`.

A panic in a [task](#panics-in-tasks) stops only that task.

## Known limitations

- **No packages to install.** Modules come from your own folders and the standard library; there is no package manager or search path.
- **No global variables.** Top-level `val`s are [constants](#constants) (numbers, `Bool`s and `String`s); everything that changes lives in functions.
- **Byte-based strings.** `String.len()` and `substring()` count bytes, and there's no `s[i]` indexing; use `for c in s`, `char_count()` or `chars()`.
- **No TLS yet.**
- **Cooperative tasks.** A task that computes without waiting keeps its core until it waits or ends; `fs` calls hold their thread while they wait.
- **A value that reaches its own `Mutex` or `Channel` isn't freed.** The cycle collector doesn't look through them (`struct Node:` whose `Mutex[List[Node]]` holds the node itself).
- **`fn drop` only on structs,** not enums, and not on types that can be part of a reference cycle. A value with a `fn drop` can be moved to another task but not copied (it panics if the task that sends it still uses it).
- **A `Result` is allocated,** like every enum whose variants carry data.
- **`Bytes` can't be map keys.**
- **No negative indexing.** `xs[-1]` panics.
- **Operators look at the left operand.** `2.0 * v` can't use `Vec2`'s `fn *`; write `v * 2.0`.
- **`==` from `fn ==` needs non-null values.** Check for `null` first (a smart cast then does the rest).
- **Interfaces can't ask for static methods or operators,** `Self` only exists in default methods, and a `match` can't test for a generic type or another interface inside an interface value.
- **Built-in types only fit interface methods without parameters** (`to_string`, `len`...).
- **No sets yet.** Use a `Map[K, Bool]` for a set.
- **A tuple is allocated,** like a struct, and has at most six elements.
- **An iterator of a nullable type ends at its first `null`.**
- **`sort()` only sorts lists of `Int`, `Float` and `String`.** For anything else, sort by a key: `people.sort_by(fn(p) -> p.name)`.
- **Compile-time code that depends on type parameters is checked per type.** Inside a `comptime for` over `fields(T)`, a `comptime if` or `comptime match` on `T`, mistakes show up in the copies of the generic code (where it's used), not where it's written; generic code that's never used isn't checked there.
- **Compile-time code can't do I/O** (files, network, environment, clock, printing) or start tasks, and stops after 20 million steps.
- **No attributes on enum variants, and no reflection over an enum's variants.** `fields` only looks at structs; values are built with spreads (there's no `f.set`).
- **`[H: Function]` takes named functions** (top-level ones and static methods), not lambdas, methods of a value or generic functions.
- **A lambda with a block body, or a `match` used as a value, can't be written inside parentheses:** both must end their line. A generic method can't be used as a value. See [lambdas](#lambdas-and-function-values).
