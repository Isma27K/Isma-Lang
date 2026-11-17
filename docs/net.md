# The `net` module

[← Back to the README](../README.md) · [Language guide](language.md)

TCP networking: listen for connections, or connect to a server, and send and receive [`Bytes`](language.md#bytes).

```python
import net

fn talk(server: net.Listener) -> Result[String, net.Error]:
    val client = net.connect("127.0.0.1", server.port())?
    val conn = server.accept()?
    client.write("ping".encode())?
    val got = conn.read(1024)?
    conn.write(got + " pong".encode())?
    conn.close()
    return Ok(client.read_all()?.decode())

fn main():
    val server = net.listen("127.0.0.1", 0)!!   # any free port
    print(talk(server))                          # Ok(ping pong)
```

- **Errors are values.** Everything that can fail returns a [`Result`](language.md#recoverable-errors) with a `net.Error`: its `kind` and a message (`connect to 127.0.0.1:1: Connection refused`).
- **Sockets close themselves** when nothing uses them any more (a variable keeps one until the end of its block), through [`fn drop`](language.md#cleaning-up-fn-drop). `close()` closes one sooner.
- **Calls wait** until they can finish. `set_timeout(time.seconds(5))` limits the wait (then the error's kind is `TimedOut`), and a [caught signal](system.md#signal) ends it (`Interrupted`).
- Writing never raises `SIGPIPE`: writing to a closed connection is an `Err` with kind `Reset`.

One connection is handled at a time: Tarn has no concurrency yet. [`examples/echo.tr`](../examples/echo.tr) is a whole echo server that stops cleanly on `Ctrl-C`.

## Functions

| | |
|---|---|
| `net.listen(host: String, port: Int) -> Result[Listener, Error]` | Listen on an address of this machine: `"127.0.0.1"` (this machine only), `"0.0.0.0"` or `"::"` (any). Port 0 picks a free port |
| `net.connect(host: String, port: Int) -> Result[Conn, Error]` | Connect to a host name or address |

## `Listener`

| | |
|---|---|
| `accept() -> Result[Conn, Error]` | Wait for the next connection |
| `address() -> String`, `port() -> Int` | Where it listens, like `127.0.0.1:8080` |
| `close()` | Stop listening |

## `Conn`

| | |
|---|---|
| `read(max: Int) -> Result[Bytes, Error]` | Up to `max` bytes, as soon as some arrive; empty when the other side has closed |
| `read_all() -> Result[Bytes, Error]` | Everything until the other side closes |
| `write(data: Bytes) -> Result[Unit, Error]` | Send all of `data` |
| `set_timeout(limit: time.Duration)` | Give up reading or writing after `limit`; zero waits forever |
| `peer_address() -> String` | The other side, like `127.0.0.1:54321` |
| `close()` | Close the connection |

## `Error` and `ErrorKind`

`Error` has `kind: ErrorKind` and `message: String`. The kinds are `Refused`, `Reset`, `TimedOut`, `Interrupted`, `AddressInUse`, `NotFound` (the host name doesn't exist), `Unreachable`, `Closed` and `Other`.
