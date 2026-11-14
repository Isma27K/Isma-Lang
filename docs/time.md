# The `time` module

[← Back to the README](../README.md) · [Language guide](language.md)

UTC dates, times and clocks. Everything is UTC; there are no time zones. A `DateTime` is immutable, and methods like `add_days` return a new value.

```python
import time

fn main():
    val now = time.now()
    print(now.to_iso())                               # 2026-11-14T16:05:09.123Z
    print(now.format("%A, %d %B %Y %H:%M"))           # Saturday, 14 November 2026 16:05
    val deadline = time.parse_iso("2026-12-25T00:00:00+07:00")   # offsets are converted to UTC
    print(deadline.seconds_since(now) / 86400)        # whole days left
```

The module is [written in Isma](../stdlib/time.isma) (about 460 lines). Only reading the clock and sleeping go through small C hooks.

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

| | |
|---|---|
| `add_seconds(n)`, `add_minutes(n)`, `add_hours(n)`, `add_days(n)` | Exact arithmetic; negative `n` goes back in time |
| `add_months(n)`, `add_years(n)` | Calendar arithmetic. The day is clamped: Jan 31 + 1 month = Feb 28 (29 in a leap year) |
| `seconds_since(other)`, `millis_since(other)` | `self - other`, rounded toward zero |
| `compare(other)` | `-1`, `0` or `1` |
| `is_before(other)`, `is_after(other)`, `equals(other)` | Ordering. Isma has no operator overloading, so `-`, `<` and `==` don't apply to a `DateTime`; the compiler points you to these methods |
| `start_of_day()` | Midnight of the same day |

## Clocks and helpers

| | |
|---|---|
| `time.unix()`, `time.unix_ms()` | The current Unix time |
| `time.monotonic_ms()`, `time.monotonic_ns()` | A clock that only moves forward. Use it to time code; the calendar clock can jump when the system time is adjusted |
| `time.sleep_ms(ms)` | Pause the program |
| `time.is_leap_year(year)` | `true` for 2024, `false` for 1900, `true` for 2000 |
| `time.days_in_month(year, month)` | `29` for February 2024 |

### Timing code

```python
import time

fn main():
    val start = time.monotonic_ms()
    var total = 0
    for i in 0..50_000_000:
        total += i % 7
    print(f"{total} in {time.monotonic_ms() - start} ms")
```

Print the result of the work you're timing. Otherwise LLVM is free to delete a loop whose result is never used, and you'll measure `0 ms`.

## How it's tested

- [`tests/run/time_utc.isma`](../tests/run/time_utc.isma) checks every function. Each of its 42 expected lines was computed independently with Python's `datetime`.
- During development, the module was cross-checked against Python on 3,000 random timestamps between years 2 and 9998, covering every format code, the weekday, `add_months` clamping and parse round-trips. All 3,000 matched.
