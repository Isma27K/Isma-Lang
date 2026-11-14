<p align="center">
  <img src="docs/media/hero.svg" alt="ISMA: Python's syntax, Kotlin's strictness, Rust's speed" width="100%">
</p>

<p align="center">
  <img src="docs/media/badge-native.svg" alt="compiles to native code">
  <img src="docs/media/badge-llvm.svg" alt="optimizer: LLVM -O3">
  <img src="docs/media/badge-tests.svg" alt="test programs passing">
  <img src="docs/media/badge-editor.svg" alt="editor: VS Code">
  <img src="docs/media/badge-fun.svg" alt="built for fun">
</p>

<p align="center">
  <b>Isma</b> is a small programming language that <b>reads like Python</b>, <b>checks like Kotlin</b><br>
  and <b>compiles to native machine code</b> through LLVM, the optimizer behind Rust.
</p>

<p align="center">
  <a href="#a-taste">A taste</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="docs/language.md">Language guide</a> ·
  <a href="docs/time.md"><code>time</code> module</a> ·
  <a href="#editor-support">VS Code</a> ·
  <a href="#performance">Performance</a> ·
  <a href="#how-it-works">How it works</a>
</p>

<br>

<a id="a-taste"></a>
## 🌱 A taste

```python
import time

struct Task:
    name: String
    var done: Bool

    fn finish(self):
        self.done = true

fn main():
    val todo = [Task("write a compiler", false), Task("add a time module", false), Task("take a nap", false)]
    for task in todo:
        if task.name != "take a nap":
            task.finish()

    var finished = 0
    for task in todo:
        if task.done:
            finished += 1
    val today = time.now()
    print(f"{finished}/{todo.len()} done on {today.format("%A, %d %B %Y")}")
```

```console
$ isma tasks.isma
2/3 done on Saturday, 14 November 2026
```

Run a script like Python, or build a standalone native binary:

<p align="center">
  <img src="docs/media/terminal-run.svg" alt="Terminal: `isma hello.isma` prints Hello from Isma!; `isma build fib.isma -o fib` then `./fib` prints the Fibonacci results; `file` shows it's a native ELF x86-64 executable" width="660">
</p>

<a id="why-isma"></a>
## 💡 Why Isma?

<table>
<tr>
<td width="33%" valign="top">

### 🐍 Reads like Python

Indentation instead of braces. `for x in list`, `and` / `or` / `not`, f-strings. Run a file with `isma app.isma`, just like `python app.py`, or give it a `#!/usr/bin/env isma` line and run it directly.

</td>
<td width="33%" valign="top">

### 🛡️ Checks like Kotlin

`val` vs `var`. Null safety: only a `T?` can be `null`, and smart casts know when it isn't. Enums with data, and a `match` that must handle every case. No implicit conversions, no "truthy" values, and every path must `return`. Mistakes are caught before the program runs, with messages that tell you the fix.

</td>
<td width="33%" valign="top">

### ⚡ Runs like Rust

Compiled ahead of time to a native binary with LLVM `-O3`. No garbage collector: memory is freed the moment it's no longer used. Integer overflow and out-of-bounds indexing still stop the program safely, and it ran **2–54× faster than Python** in the [benchmarks](#performance).

</td>
</tr>
</table>

<a id="errors"></a>
## 🧑‍🏫 Errors that teach

Isma's checker is strict, but it explains itself: every error points at the exact spot and usually says how to fix it. Bugs that only show up at run time stop with a clear message and the line they came from:

<p align="center">
  <img src="docs/media/terminal-errors.svg" alt="Terminal: `isma check oops.isma` reports 'mismatched types: expected Float, found Int' with a hint to use .to_float(), and 'if condition must be a Bool, found Int' with a hint to compare explicitly; `isma crash.isma` prints 10, 20, 30 then 'panic at crash.isma:5:15: index 3 out of bounds for list of length 3'" width="860">
</p>

<a id="quick-start"></a>
## 🚀 Quick start

You need [Rust](https://rustup.rs) to build the compiler, and `clang`, which Isma uses to produce native code.

```bash
./install.sh             # pick [1] default or [2] custom
isma hello.isma          # run a script
isma build app.isma -o app && ./app
```

<details>
<summary><b>📺 Watch the installer</b>: a hacker-styled installer with default and custom modes, plus a matching uninstaller</summary>
<br>
<p align="center">
  <img src="docs/media/terminal-install.svg" alt="The Isma installer: a green ISMA banner, recon steps (host, cargo, clang), deploy steps (compile, copy to ~/.local/bin, wire PATH, self-test, write manifest) and an ACCESS GRANTED box" width="640">
</p>
</details>

| Command | |
|---|---|
| `isma app.isma [args...]` | Compile and run. Unchanged scripts are cached and start in **~2 ms** |
| `isma build app.isma -o app` | A standalone native binary that runs without Isma |
| `isma check app.isma` | Type-check and lint only |
| `isma build app.isma --emit-llvm` | Also write the generated LLVM IR |
| `isma build app.isma --emit-mir` | Also write the mid-level IR, with every reference-count operation |
| `isma run --arc-stats app.isma` | Print allocations, frees and peak memory when the program exits |
| `isma run --debug app.isma` | Build without optimizations (`-O0 -g`) |
| `./uninstall.sh` | Remove everything Isma installed (default or pick targets) |

Installation details, options and the uninstaller are covered in [docs/install.md](docs/install.md).

<a id="language"></a>
## 📖 The language in one screen

```python
import time                                  # the standard library is written in Isma

struct Point:                                # structs with typed fields
    var x: Float                             # fields are immutable unless `var`
    var y: Float

    fn length(self) -> Float:                # methods take `self`
        return sqrt(self.x * self.x + self.y * self.y)

    fn +(self, other: Point) -> Point:       # operator functions: `p + q`
        return Point(self.x + other.x, self.y + other.y)

fn clamp(x: Int, low: Int, high: Int) -> Int:
    if x < low:
        return low
    elif x > high:
        return high
    return x

enum Shape:                                  # enums whose variants carry data
    Circle(radius: Float)
    Rect(w: Float, h: Float)

fn area(s: Shape) -> Float:
    match s:                                 # the compiler checks every case is handled
        case Circle(r):
            return 3.14159 * r * r
        case Rect(w, h):
            return w * h

fn first_long(words: List[String]) -> String?:  # `String?`: a String or null
    for w in words:
        if w.len() > 3:
            return w
    return null

fn main():
    val p = Point(x=3.0, y=4.0)              # `val` can't be reassigned...
    var count = 0                            # ...`var` can
    val xs: List[Int] = [5, 3, 8]            # lists are typed and bounds-checked
    xs.push(clamp(42, 0, 10))
    for x in xs:                             # loop over a list...
        count += x
    for i in 0..3:                           # ...or a range
        print(f"{i}: {xs[i]}")               # f-strings
    print(f"|p + p| = {(p + p).length()}, sum = {count}")
    val word = first_long(["a", "tiny", "list"])
    print(word?.len() ?: 0)                  # safe call `?.` and elvis `?:`
    if word != null:
        print(word.len())                    # smart cast: `word` is a String here
    print(area(Shape.Rect(w=2.0, h=3.0)))
    print(time.now().to_iso())
```

Bigger programs split into files. Every `.isma` file is a module:

```python
# geometry.isma
struct Point:
    x: Float
    y: Float

fn _square(x: Float) -> Float:               # `_names` are private to their module
    return x * x

fn dist(a: Point, b: Point) -> Float:
    return sqrt(_square(a.x - b.x) + _square(a.y - b.y))
```

```python
# main.isma
from geometry import Point, dist             # or `import geometry`, then `geometry.dist(...)`

fn main():
    print(dist(Point(0.0, 0.0), Point(3.0, 4.0)))
```

The [**language guide**](docs/language.md) covers everything: types, strictness rules, lints, structs, enums and `match`, lists, strings, null safety, modules, built-ins and panics.

<a id="time"></a>
## 🕰️ Batteries included: `time`

The first standard-library module handles UTC dates, times and clocks, and it's written in Isma itself. It was cross-checked against Python's `datetime` on 3,000 random dates.

```python
import time

fn main():
    val now = time.now()
    print(now.format("%A, %d %B %Y at %H:%M"))
    val landing = time.parse_iso("1969-07-20T20:17:40Z")
    print(f"Moon landing was a {landing.weekday_name()}")
    print(f"{((now - landing) / time.days(365)).to_int()} years ago")   # DateTime - DateTime is a Duration
    print(now + time.hours(36))                                        # 36 hours from now
```

<p align="center">
  <img src="docs/media/terminal-time.svg" alt="Terminal: `isma clock.isma` prints the current UTC time in ISO format, a pretty date, the Unix time, days until new year, the Moon landing date (a Sunday) and how long a summing loop took" width="660">
</p>

Every function, format code and helper is listed in the [`time` reference](docs/time.md).

<a id="editor-support"></a>
## 🧩 Editor support

<img src="docs/media/vscode-icon.png" alt="Isma VS Code extension icon" width="72" align="right">

The VS Code extension (in the companion `isma-vscode` project) gets all its smarts from **`isma lsp`**, a language server built into the compiler. So the editor always agrees with `isma build`. It provides:

- **Live type checking and lints** as you type. Unused and unreachable code is faded out.
- **Hover** shows the type of any variable or expression, including inferred types and smart casts, plus `#` doc comments.
- **Inlay type hints**: `val total = price * qty` shows as `val total: Float = …`.
- **Completion** for members after `.` and `?.`, module functions after `time.`, files and folders after `import` and `from`, a module's names after `from geometry import`, and variables in scope. Completing after `.` on a value that might be null turns it into `?.`.
- **Multi-file aware.** Go to definition jumps into your other files. Editing a module re-checks the open files that import it, even before you save, and an error inside an imported file shows on the `import` line that leads to it.
- **Go to definition**, an **outline view**, syntax highlighting, snippets and a **▶ Run** button.
- **Automatic restarts**: when you reinstall `isma`, the extension notices and restarts the language server by itself.

<a id="performance"></a>
## ⚡ Performance

<p align="center">
  <img src="docs/media/benchmark.svg" alt="Bar chart of run times in milliseconds, lower is better. fib(35): Isma 38, Rust 28, Python 872. Sieve to 10 million: Isma 49, Rust 41, Python 2,012. Building a 15 MB string: Isma 120, Rust 83, Python 278. Moving a particle 5 million steps: Isma 21, Rust 9, Python 1,129. Binary trees with 3 million enum nodes: Isma 41, Rust 92, Python 319." width="760">
</p>

| Benchmark | Isma | Rust (`rustc -O`) | Python 3 |
|---|---:|---:|---:|
| `fib(35)`: 30 million recursive calls | 38 ms | 28 ms | 872 ms |
| Sieve of Eratosthenes to 10 million | 49 ms | 41 ms | 2,012 ms |
| Building a 15 MB string from 2 million f-strings | 120 ms | 83 ms | 278 ms |
| Moving a `Vec2` particle 5 million steps (`pos = pos + vel`) | 21 ms | 9 ms | 1,129 ms |
| Binary trees: building and walking 3 million `enum` nodes | 41 ms | 92 ms | 319 ms |

These are the best of 3 runs on an AMD Ryzen 5 5500. Reproduce them with `./bench/run.sh`.

- **Where the gap to Rust comes from.** Most of it is safety: Isma checks every integer operation for overflow, while Rust in release mode silently wraps. With the same checks (`rustc -O -C overflow-checks=on`), Rust's `fib` takes about 46 ms, slower than Isma.
- **Help for the optimizer.** Isma tells LLVM which memory can never overlap (TBAA metadata). That lets list lengths stay in CPU registers inside hot loops.
- **Memory without a garbage collector.** Values are reference counted, and the compiler removes most of the counting (see [Memory](docs/language.md#memory)). The particle loop allocates 3 objects in total, because each new `Vec2` reuses the memory of the one it replaces. Peak memory stayed within 10% of Rust's on the string, particle and tree benchmarks.
- **Why Isma beats Rust on binary trees.** Both programs allocate one heap node per tree node (`Leaf` costs nothing: it's a shared constant in Isma, and `None` in Rust). Isma recycles freed nodes through its own size-class free lists, while Rust here uses the system `malloc`. Walking the tree does no reference counting at all, because the match borrows the fields.
- **Benchmarking your own code.** LLVM deletes loops whose results are never used, so print what you compute or you'll measure `0 ms`. `isma run --debug` turns optimization off.

<a id="how-it-works"></a>
## 🔧 How it works

<p align="center">
  <img src="docs/media/pipeline.svg" alt="Diagram: your .isma file goes through the lexer, parser, type checker, MIR with automatic reference counting, LLVM IR codegen and clang -O3 with a small C runtime, producing a native binary. Imported .isma files, yours and the standard library's, join at the parser. The type checker also powers `isma lsp`, which feeds VS Code." width="100%">
</p>

- **One front end, two uses.** The same lexer, parser and type checker compile your programs and power the editor through `isma lsp`.
- **Every file is a namespace of one type checker.** [`src/project.rs`](src/project.rs) finds each `import` relative to the file that contains it and loads every file once, so modules may import each other. All files share one offset space for source positions, which is how errors, panic locations and go-to-definition land in the right file. The whole program becomes one LLVM module, so calls across files inline like any others.
- **The standard library is Isma code** ([`stdlib/time.isma`](stdlib/time.isma)), compiled together with your program, so it's as fast as your own functions. A panic inside it reports *your* calling line, like Rust's `#[track_caller]`.
- **Memory management happens on a mid-level IR.** The typed program is lowered to a control-flow graph in which every intermediate value has a name ([`src/mir.rs`](src/mir.rs)). [`src/arc.rs`](src/arc.rs) then adds reference counting in the style of Perceus (Koka, Lean 4). It drops each value right after its last use. Parameters are borrowed unless consumed, and projections stay borrowed while interprocedural write-effect summaries prove their slot can't change. A unique struct's memory is reused for the next one, and unique strings are appended to in place. Types that can form cycles get a trial-deletion cycle collector. [`src/cycles.rs`](src/cycles.rs) decides which, from the type graph: a cycle needs a reference that can be replaced after construction, so immutable trees never pay for it.
- **Operators on your types are method calls.** `a + b` on a struct type-checks to a direct call of its `fn +`, picked by the right operand's type when there are several, and `a < b` to `fn <=>`. The optimizer and the reference counting see an ordinary call: `acc = acc + v` reuses `acc`'s memory, and the particle benchmark runs as fast as with `pos.add(vel)`.
- **`match` is checked with Maranget's usefulness algorithm** ([`src/patterns.rs`](src/patterns.rs)), as in Rust and OCaml. The same pass that proves a `match` exhaustive produces the missing cases for the error message and finds unreachable ones. Cases compile to chains of tag tests, which LLVM merges into a `switch`. Enums without data are plain integers, and data-less variants of other enums are shared constants.
- **Codegen writes LLVM IR as text** and hands it to `clang -O3`, together with a ~780-line C runtime: the allocator, the cycle collector, strings, lists, printing and clocks.
- **Null safety is checked by flow typing.** The checker tracks which locals and `val` field chains are known to be non-null at each point (through `if`, `and`/`or`, loops, early `return`s and `?:`), which is what makes smart casts work. At run time it costs nothing: `User?` is a nullable pointer, and `Int?` is a flag-plus-value pair that stays in registers.
- **Safety stays cheap.**
  - Integer `+ - *` use LLVM's overflow-checking intrinsics.
  - Panic paths are marked cold.
  - List indexing is bounds-checked inline.
- **About 13,800 lines of Rust.** The only dependencies are `ariadne` (error rendering) and `lsp-server` / `lsp-types` (the language server).

<a id="roadmap"></a>
## 🗺️ Roadmap

- [x] Core language: `val`/`var`, functions, structs, lists, f-strings, strict types
- [x] Native compilation through LLVM, with overflow and bounds checks
- [x] Lints, error hints and a VS Code extension with a language server
- [x] `import` and a standard library written in Isma (`time`)
- [x] Null safety: `Int?`, `?.`, the elvis operator `?:`, `!!`, smart casts
- [x] Memory management through automatic reference counting, with a cycle collector
- [x] `enum` + `match` with exhaustiveness checking
- [x] Importing your own `.isma` files: `import`, `from ... import`, `as`, folders and `..`
- [x] Operator functions: `fn +(self, other: T)`, `==`, `<=>`, `[]`, so `time.now() - start` is a `Duration`
- [ ] Generics for user code: `fn max[T](a: T, b: T) -> T`
- [ ] More modules: `math`, `fs`, maps, more string utilities

<a id="development"></a>
## 🧪 Development

```bash
cargo test                          # unit tests + every program in tests/ (also leak-checked and run under ASan)
./bench/run.sh                      # Isma vs Rust vs Python (writes bench/results.json)
python3 docs/media/generate.py      # re-render the README's media from real runs
```

<details>
<summary><b>Project layout</b></summary>

```
src/            compiler: lexer, parser, checker, codegen, driver, language server, CLI
stdlib/         the standard library, written in Isma (time.isma)
runtime/        the small C runtime linked into every program
examples/       hello, fib, fizzbuzz, structs, primes_sieve, sort, collatz, guess, calc, clock
tests/run/      programs + their expected output
tests/fail/     programs that must not compile    (# expect-error: ...)
tests/warn/     programs that must warn            (# expect-warning: ...)
tests/panic/    programs that must panic           (# expect-panic: ...)
tests/arc/      programs with an allocation budget (# expect-max-allocs: ...)
bench/          Isma vs Rust vs Python
docs/           language guide, time reference, install guide, README media
install.sh      the installer (default or custom)
uninstall.sh    the uninstaller (wipe all or pick targets)
```
</details>

<br>

<p align="center"><sub>Made for fun. The terminal recordings above are real runs of Isma and the chart uses real benchmark results. All images are generated by <code>docs/media/generate.py</code>.</sub></p>
