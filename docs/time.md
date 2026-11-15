# The `time` module

[← Back to the README](../README.md) · [Language guide](language.md)

UTC dates, times, durations and clocks. Everything is UTC; there are no time zones. A `DateTime` and a `Duration` are immutable: methods and operators return new values.

```python
import time

fn main():
    val now = time.now()
    print(now)                                        # 2026-11-14T16:05:09.123Z
    print(now.format("%A, %d %B %Y %H:%M"))           # Saturday, 14 November 2026 16:05
    val deadline = time.parse_iso("2026-12-25T00:00:00+07:00")   # offsets are converted to UTC
    print(deadline - now)                             # 958h34m57.710868749s
    print(now + time.days(3) > deadline)              # false
```

The module is [written in Tarn](../stdlib/time.tr) (about 650 lines). Only reading the clock and sleeping go through small C hooks.

## Creating a `DateTime`

| Function | |
|---|---|
| `time.now()` | The current UTC date and time, with nanosecond precision |
| `time.utc(year, month, day, hour, minute, second)` | Validated: panics on e.g. February 30 |
| `time.date(year, month, day)` | Midnight UTC |
| `time.from_unix(seconds)` | From a Unix timestamp (negative = before 1970) |
| `time.from_unix_ms(ms)` | From a Unix timestamp in milliseconds |
| `time.parse_iso(text)` | Parse ISO 8601 / RFC 3339 (see below). Panics if invalid |
| `time.is_valid_iso(text)` | Can `parse_iso` read this? Check before parsing user input |

`parse_iso` accepts:
- dates: `"2026-11-14"`;
- date-times with a `T` or a space: `"2026-11-14T16:05:09Z"`, `"2026-11-14 16:05"`;
- fractional seconds: `"16:05:09.25"`, up to nanoseconds;
- offsets: `Z`, `+02:00` or `-0530`, all converted to UTC.

You can't call `time.DateTime(...)` directly. The functions above are the only way in, so a `DateTime` is always a real moment.

## Reading a `DateTime`

| | |
|---|---|
| fields | `year`, `month`, `day`, `hour`, `minute`, `second`, `nanosecond` (read-only) |
| `unix()`, `unix_ms()` | Seconds / milliseconds since 1970-01-01T00:00:00Z |
| `millisecond()` | The millisecond part (0–999) |
| `weekday()` | `0` = Monday … `6` = Sunday |
| `weekday_name()`, `month_name()` | `"Saturday"`, `"November"` |
| `day_of_year()` | 1–366 |
| `to_iso()` | `"2026-11-14T16:05:09Z"`, adding `.123` (or µs/ns) only when non-zero |
| `date_string()`, `time_string()` | `"2026-11-14"`, `"16:05:09"` |

## Formatting: `format(pattern)`

| Code | Meaning | Code | Meaning |
|---|---|---|---|
| `%Y` | year, `2026` | `%y` | year, 2 digits, `26` |
| `%m` | month, `01`–`12` | `%d` | day, `01`–`31` |
| `%B` | `November` | `%b` | `Nov` |
| `%A` | `Saturday` | `%a` | `Sat` |
| `%H` | hour, `00`–`23` | `%I` | hour, `01`–`12` |
| `%p` | `AM` / `PM` | `%M` | minute, `00`–`59` |
| `%S` | second, `00`–`59` | `%L` | milliseconds, `000`–`999` |
| `%f` | microseconds, `000000`–`999999` | `%j` | day of year, `001`–`366` |
| `%s` | Unix seconds | `%%` | a literal `%` |

An unknown code such as `%Q` panics, with the location pointing at your `format` call.

## Arithmetic and comparison

`DateTime` has [operator functions](language.md#operator-functions):

| | |
|---|---|
| `a - b` | The [`Duration`](#durations) from `b` to `a` (negative if `a` is earlier) |
| `a + d`, `a - d` | The moment `d` (a `Duration`) later or earlier |
| `a < b`, `a <= b`, `a > b`, `a >= b`, `a <=> b` | Earlier moments come first |
| `a == b`, `a != b` | The same moment |
| `add_seconds(n)`, `add_minutes(n)`, `add_hours(n)`, `add_days(n)` | Like `a + time.seconds(n)`...; negative `n` goes back in time |
| `add_months(n)`, `add_years(n)` | Calendar arithmetic. The day is clamped: Jan 31 + 1 month = Feb 28 (29 in a leap year) |
| `seconds_since(other)`, `millis_since(other)` | `(self - other)` in whole seconds or milliseconds, rounded toward zero |
| `compare(other)`, `is_before(other)`, `is_after(other)`, `equals(other)` | The same as `<=>`, `<`, `>` and `==`, as methods |
| `start_of_day()` | Midnight of the same day |
| `to_string()` | The ISO form: what `print(a)` and `f"{a}"` show |

## Durations

A `time.Duration` is a length of time with nanosecond precision, positive or negative. You get one by subtracting two moments, or from these functions:

| | |
|---|---|
| `time.nanos(n)`, `time.micros(n)`, `time.millis(n)` | `n` of those |
| `time.seconds(n)`, `time.minutes(n)`, `time.hours(n)`, `time.days(n)` | `n` of those (a day is 24 hours) |

```python
val d = time.minutes(1) + time.seconds(30)
print(d)                       # 1m30s
print(d * 2)                   # 3m
print(d * 1.5)                 # 2m15s
print(d / 4)                   # 22.5s
print(d / time.seconds(10))    # 9.0: how many times it fits
print(d > time.seconds(30))    # true
print(d.seconds())             # 90.0
```

| | |
|---|---|
| `a + b`, `a - b`, `-a` | Sums and differences of durations |
| `d * n`, `d * x`, `d / n` | Scaled by an `Int` or a `Float` (rounded toward zero, to the nanosecond) |
| `a / b` | A `Float`: how many times `b` fits in `a` |
| `a % b` | What's left after taking out whole `b`s: `time.seconds(130) % time.minutes(1)` is `10s` |
| `<`, `<=`, `>`, `>=`, `<=>`, `==`, `!=` | Shorter first |
| `nanos()`, `micros()`, `millis()` | Whole units, rounded toward zero, as an `Int` |
| `seconds()`, `minutes()`, `hours()` | With a fraction, as a `Float` |
| `round(unit)`, `truncate(unit)` | To a multiple of `unit`: `d.round(time.millis(1))` turns `40.859266ms` into `41ms`. `round` sends halves away from zero, `truncate` goes toward zero |
| `abs()`, `is_negative()` | |
| `to_string()` | `1h30m`, `2.5s`, `250ms`, `1.5µs`, `42ns`, `-3s`: what `print(d)` shows |

A `Duration` can only be made by these functions and operators: `time.Duration(...)` is an error.

## Clocks and helpers

| | |
|---|---|
| `time.unix()`, `time.unix_ms()` | The current Unix time |
| `time.monotonic()` | A clock that only moves forward, as a `Duration` since an unspecified moment: only the difference between two readings means something. Use it to time code; the calendar clock can jump when the system time is adjusted |
| `time.monotonic_ms()`, `time.monotonic_ns()` | The same clock, as an `Int` |
| `time.sleep_ms(ms)` | Pause the program |
| `time.is_leap_year(year)` | `true` for 2024, `false` for 1900, `true` for 2000 |
| `time.days_in_month(year, month)` | `29` for February 2024 |

### Timing code

```python
import time

fn main():
    val start = time.monotonic()
    var total = 0
    for i in 0..50_000_000:
        total += i % 7
    val took = time.monotonic() - start
    print(f"{total} in {took}")                      # 149999997 in 40.859266ms
    print(f"{total} in {took.round(time.millis(1))}") # 149999997 in 41ms
```

Print the result of the work you're timing. Otherwise LLVM is free to delete a loop whose result is never used, and you'll measure nothing.

## How it's tested

- [`tests/run/time_utc.tr`](../tests/run/time_utc.tr) checks every function. Each of its 42 expected lines was computed independently with Python's `datetime`.
- During development, the module was cross-checked against Python on 3,000 random timestamps between years 2 and 9998, covering every format code, the weekday, `add_months` clamping and parse round-trips. All 3,000 matched.
