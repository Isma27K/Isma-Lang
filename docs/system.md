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
| `get_bool(name) -> Bool?` | As a `Bool` (`true`/`yes`/`on`/`1`, `false`/`no`/`off`/`0`, any case); `null` if unset or empty; panics on anything else |
| `get_bool_or(name, default) -> Bool` | The same, or `default` |
| `set(name, value)` | Set it. Panics if `name` is empty or contains `=` |
| `remove(name)` | Unset it |
| `all() -> Map[String, String]` | Every variable |
| `load(path) -> Result[Unit, String]` | Read another `.env` file (see below) |

### `.env` files

The first time a program uses `env`, it reads the `.env` in the folder of its main file (`tarn app.tr` reads the one next to `app.tr`, from any folder). A built program that was moved away from its project reads the `.env` in the current folder instead. No `.env` is fine.

```bash
PORT=8080
DATABASE_URL="postgres://localhost/app"   # a comment
export GREETING='hello, world'            # single quotes: as written
MOTD="first line
second line"                              # double quotes: escapes, several lines
LOG_FILE=${HOME}/logs/app.log             # another variable
```

Variables already set when the program starts win over the file. A malformed line stops the program with the file and line. `env.load("config/prod.env")` before using `env` reads that file instead of the default one; it returns an `Err` saying why a file can't be read or which line is wrong.

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

When a caught signal arrives, calls that wait (`time.sleep_ms`, a socket's `accept`, `read` and `connect`) return at once, in every [task](language.md#tasks) waiting in one, and so do new ones while the signal waits to be received. So a program blocked in one notices the signal right away.

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
