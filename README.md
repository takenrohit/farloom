# farloom

A learning-oriented multithreaded HTTP/1.1 server written in Rust using only the
standard library (no tokio, hyper, or other crates). One acceptor thread hands
each TCP connection to a fixed-size worker pool through a bounded queue. When the
queue is full, the acceptor answers `503 Service Unavailable` itself.

Features:

- Static files from a directory (GET only), with `index.html` for directories and
  MIME types by extension.
- Path-traversal protection: `..`, backslashes, NUL, drive letters / colons, and
  leading `//` are rejected in raw and percent-encoded form, and every resolved
  file must canonicalize to a path inside the static root.
- Built-in routes: `GET /health` returns `{"status":"ok"}`; `POST /api/echo`
  returns the request body with the request's `Content-Type`.
- Request bodies delimited by `Content-Length` (chunked encoding gets 400).
- Status codes 200, 400, 404, 405 (with `Allow`), 413, 500, and 503
  (with `Retry-After: 1`). Every response carries `Connection: close`.
- Socket read/write timeouts, graceful shutdown that drains queued work, and
  one `key=value` log line per request.

## Layout

```
Cargo.toml          workspace: webserver, loadtest, testkit
public/             default static root (index.html, style.css)
webserver/src/
  main.rs           CLI entry point and exit codes
  config.rs         command-line options and defaults
  pool.rs           ThreadPool<J> on mpsc::sync_channel
  server.rs         bind, acceptor loop, 503 rejection, shutdown
  connection.rs     per-connection parse -> dispatch -> write -> close
  http/             request types, parser, response writer, percent decoding
  router.rs         exact-path router; routes.rs has /health and /api/echo
  static_files.rs   static handler and traversal checks; mime.rs for types
  log.rs            single-write log lines to stdout/stderr
webserver/tests/    integration tests over real TCP on an ephemeral port
loadtest/           standalone load-test client
testkit/            tiny seeded RNG and property-check runner (dev-dependency)
```

## Build, test, run (PowerShell)

```powershell
cargo build --workspace
cargo test --workspace
cargo run -p webserver                         # serves ./public on 127.0.0.1:7878
cargo run -p webserver -- --port 0 --workers 8 # ephemeral port, printed in the startup line
cargo run -p webserver -- --help
```

Run from the workspace root so the default `./public` resolves.

### Server options

Both `--opt value` and `--opt=value` work; the last occurrence wins.

| Option | Default | Notes |
|---|---|---|
| `--host` | `127.0.0.1` | bind address |
| `--port` | `7878` | `0` picks an ephemeral port |
| `--workers` | `4` | must be >= 1 |
| `--queue-capacity` | `64` | connections waiting for a worker; must be >= 1 |
| `--static-root` | `./public` | must be an existing directory |
| `--read-timeout-secs` | `5` | must be >= 1 |
| `--write-timeout-secs` | `5` | must be >= 1 |
| `--max-body-bytes` | `1048576` | larger `Content-Length` gets 413 |
| `--max-requests` | none | shut down gracefully after N accepted connections |

Exit codes: `0` success or `--help`, `2` invalid options, `1` startup failure
(static root missing, port in use).

### Trying it by hand

In PowerShell `curl` is an alias for `Invoke-WebRequest`, so call `curl.exe`:

```powershell
curl.exe -i http://127.0.0.1:7878/health
curl.exe -i -X POST -H "Content-Type: text/plain" --data "hello" http://127.0.0.1:7878/api/echo
curl.exe -i --path-as-is "http://127.0.0.1:7878/..%2fCargo.toml"   # 404, logged as path_traversal
```

`--path-as-is` stops curl from normalizing `..` before sending.

### Load tester

```powershell
cargo build --release
cargo run --release -p webserver -- --workers 4 --queue-capacity 16
# second terminal
cargo run --release -p loadtest -- --addr 127.0.0.1:7878 --path /health --requests 5000 --concurrency 64
```

| Option | Default |
|---|---|
| `--addr` | `127.0.0.1:7878` |
| `--path` | `/` (must start with `/`) |
| `--requests` | `1000` |
| `--concurrency` | `8` |

The report lists total requests, counts per status code, connection errors,
elapsed time, and requests per second. The load tester exits with `1` when no
request got a response (for example, the server is not running) and `2` for
invalid options.

Things to try: compare `--workers 1` with `--workers 8`, shrink
`--queue-capacity` until 503s appear, or request a large static file.

## Logs

Access and lifecycle lines go to stdout; errors and traversal warnings go to stderr:

```
ts=1718000000456 level=INFO event=access worker=worker-2 client=127.0.0.1:54012 method=GET path=/index.html status=200 dur_ms=3
ts=1718000000460 level=WARN event=path_traversal worker=worker-0 client=127.0.0.1:54013 path=/..%2fCargo.toml rule=dotdot
```

Filter them with `Select-String`, e.g. `... 2>&1 | Select-String "status=503"`.

## Known limitations

- Ctrl+C is not handled (std has no signal API); it ends the process
  immediately. Use `--max-requests` for a graceful stop in experiments.
- One request per connection (no keep-alive), no HTTPS, no chunked bodies.
- The read timeout applies per read, so a client that sends one byte just under
  every timeout can hold a worker for a long time.
- Static files are read fully into memory before sending.
- Each load-test request opens a new TCP connection. On Windows, very large runs
  can exhaust ephemeral ports; keep runs at about 10,000 requests or wait
  between runs. Use `127.0.0.1` rather than `localhost`, which may try `::1`
  first and add a delay per connection.
