# The `math` module

[← Back to the README](../README.md) · [Language guide](language.md)

Constants, powers and logarithms, trigonometry, rounding and comparisons.

```python
import math

fn main():
    print(math.pi)                       # 3.141592653589793
    print(math.sqrt(2.0))                # 1.4142135623730951
    print(math.floor(-2.5))              # -3.0
    print(math.round(2.5).to_int())      # 3
    print(math.max(3, 7))                # 7
    print(math.clamp(1.5, 0.0, 1.0))     # 1.0
    print(math.gcd(12, 18))              # 6
    print(math.degrees(math.atan2(1.0, 1.0)))   # 45.0
```

The module is [written in Tarn](../stdlib/math.tr). The `Float` functions compile to LLVM's math intrinsics or the C library's, so they cost what they cost in C. `min`, `max`, `clamp` and `abs` are [generic](language.md#generics): they work on `Int`, `Float`, `String` (except `abs`) and your own types that have `<=>` (and `-`, for `abs`).

`Float` functions follow IEEE 754, like C and Python's `math`: invalid input gives NaN or an infinity instead of a panic (`math.sqrt(-1.0)` is NaN, `math.ln(0.0)` is `-inf`). The `Int` functions panic on overflow, like `+` and `*` do.

## Constants

These are [constants](language.md#constants): `math.pi` costs nothing at run time.

| Constant | |
|---|---|
| `math.pi` | π = 3.141592653589793 |
| `math.tau` | τ = 2π = 6.283185307179586 |
| `math.e` | e = 2.718281828459045 |
| `math.inf` | Positive infinity (`-math.inf` is negative infinity) |
| `math.nan` | Not a number. It isn't equal to anything, itself included: test with `math.is_nan(x)` |
| `math.max_int` | The largest `Int`, 9223372036854775807 |
| `math.min_int` | The smallest `Int`, -9223372036854775808 |

## Powers and logarithms

| Function | |
|---|---|
| `sqrt(x)` | Square root (the built-in `sqrt` is the same function) |
| `cbrt(x)` | Cube root |
| `pow(x, y)` | `x` to the power `y` |
| `exp(x)` | e to the power `x` |
| `ln(x)` | Natural logarithm |
| `log(x, base)` | Logarithm in any base: `math.log(8.0, 2.0)` is 3.0 |
| `log2(x)`, `log10(x)` | Base-2 and base-10 logarithms |
| `hypot(x, y)` | `sqrt(x * x + y * y)` |
| `pow_int(base, exp)` | On `Int`s: `math.pow_int(2, 10)` is 1024. Panics on overflow and for a negative `exp` |
| `isqrt(n)` | On `Int`s: the largest `r` with `r * r <= n`. Panics for a negative `n` |

## Rounding

All of these return a `Float`; use `.to_int()` to get an `Int`.

| Function | |
|---|---|
| `floor(x)` | Round down: `floor(2.7)` is 2.0, `floor(-2.5)` is -3.0 |
| `ceil(x)` | Round up: `ceil(2.1)` is 3.0 |
| `round(x)` | To the nearest whole number, halves away from zero: `round(2.5)` is 3.0, `round(-2.5)` is -3.0 |
| `trunc(x)` | Drop the fractional part: `trunc(-2.7)` is -2.0 (the same as `.to_int()` does) |

## Trigonometry

Angles are in radians.

| Function | |
|---|---|
| `sin(x)`, `cos(x)`, `tan(x)` | |
| `asin(x)`, `acos(x)`, `atan(x)` | Inverses |
| `atan2(y, x)` | The angle of the point `(x, y)`, from -π to π |
| `sinh(x)`, `cosh(x)`, `tanh(x)` | Hyperbolic functions |
| `degrees(radians)`, `radians(degrees)` | Conversions |

## Comparing and checking

| Function | |
|---|---|
| `min(a, b)`, `max(a, b)` | The smaller or larger value (`a` when they're equal). Any type with `<=>` |
| `clamp(x, low, high)` | `x` moved into `low..=high` |
| `abs(x)` | Absolute value, for `Int` and `Float` (and types with `<=>` and `-`). `math.abs(math.min_int)` panics: it doesn't fit in an `Int` |
| `sign(x)` | -1.0, 0.0 or 1.0 |
| `is_nan(x)`, `is_inf(x)`, `is_finite(x)` | Is `x` NaN, infinite, or an ordinary number? |

## Integers

| Function | |
|---|---|
| `gcd(a, b)` | Greatest common divisor (never negative) |
| `lcm(a, b)` | Least common multiple (never negative; 0 if either is 0) |
| `factorial(n)` | `n!`. Panics from 21! on, which doesn't fit in an `Int` |
