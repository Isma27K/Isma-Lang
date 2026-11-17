# The `env`, `signal` and `url` modules

[← Back to the README](../README.md) · [Language guide](language.md)

## `env`

Environment variables.

```python
import env

fn main():
    val port = env.get("PORT")?.to_int_or_null() ?: 8080
    val mode = env.get_or("APP_MODE", "development")
    env.set("APP_MODE", "test")       # for this program and the programs it starts
    env.remove("APP_MODE")
    for name, value in env.all():
        print(f"{name}={value}")
```

| | |
|---|---|
| `get(name) -> String?` | The value, or `null` if it isn't set |
| `get_or(name, default) -> String` | The value, or `default` |
| `set(name, value)` | Set it. Panics if `name` is empty or contains `=` |
| `remove(name)` | Unset it |
| `all() -> Map[String, String]` | Every variable |

## `signal`

Signals are how the system, or you with `Ctrl-C` or `kill`, asks a running program to stop (or reload, or...). An uncaught signal does its default thing, which for most is stopping the program at once. A caught one is only recorded, so the program can finish what it's doing:

```python
import signal
from signal import Signal

fn main():
    signal.catch(Signal.Interrupt)
    signal.catch(Signal.Terminate)
    while signal.received() == null:
        do_some_work()
    print("stopping cleanly")
```

While a caught signal waits to be received, calls that wait (`time.sleep_ms`, a socket's `accept`, `read` and `connect`) return at once, so a program blocked in one notices the signal right away.

| | |
|---|---|
| `catch(s: Signal)` | Record `s` instead of stopping |
| `reset(s: Signal)` | Back to the default |
| `received() -> Signal?` | A caught signal that arrived and hasn't been received yet, or `null`. Doesn't wait |
| `wait() -> Signal` | Wait for a caught signal. Panics if none is caught |
| `send(s: Signal)` | Send `s` to this program |

`Signal` is `Interrupt` (`Ctrl-C`, SIGINT), `Terminate` (SIGTERM), `Hangup` (SIGHUP), `User1`, `User2` and `Quit` (`Ctrl-\`).

## `url`

Percent-encoding and query strings.

```python
import url

fn main():
    print(url.encode("a b&c"))                          # a%20b%26c
    print(url.encode_path("/files/my notes.txt"))       # /files/my%20notes.txt
    print(url.decode("caf%C3%A9") ?: "malformed")       # café
    print(url.parse_query("q=tarn+lang&page=2"))        # {"q": "tarn lang", "page": "2"}
    print(url.query({"q": "tarn lang", "page": "2"}))   # q=tarn%20lang&page=2
```

`decode` gives `null` for a `%` that isn't followed by two hex digits, or for bytes that aren't UTF-8. In query strings, `+` is a space.
