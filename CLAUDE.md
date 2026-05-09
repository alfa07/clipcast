# Clipcast — notes for Claude

Bidirectional clipboard sync + remote→local file open over SSH. Written in Rust, single crate, two source files.

## Architecture

Three processes coordinate over a single SSH stdio channel:

```
remote:  `open foo.pdf`  ──unix socket──►  clipcast server  ──ssh stdio──►  clipcast client  ──►  /usr/bin/open
 (argv[0]=open)                             (long-running)                    (local Mac)
```

1. **Mac client** (`clipcast client --host <HOST>`) spawns `ssh <host> clipcast server`, owns the pipe, syncs clipboard in both directions, and handles incoming `OpenBegin`/`OpenChunk`/`OpenResult` messages to write files under `~/.clipcast/remote/<host>/<ts>-<rand>/` and invoke local `open`.
2. **Remote server** (`clipcast server`) reads stdin/writes stdout for the SSH channel, syncs clipboard, **and** binds a unix socket (default `$XDG_RUNTIME_DIR/clipcast-$USER.sock`, mode `0600`) where short-lived `open` clients connect. It acts as a relay: socket-client messages → SSH; `OpenResult` replies → correct socket client via a `request_id` → `mpsc::Sender` map.
3. **Remote open client** — `~/.clipcast/bin/open` is a symlink to `clipcast`. In `main()`, if `basename(argv[0]) != "clipcast"`, we route to `run_open_client`, which classifies args, streams files to the socket, and awaits an `OpenResult`.

Transport: newline-delimited JSON, same enum (`Message`) used on all three sides. Files stream as 256 KiB base64 chunks so the 500 ms clipboard poll + 3 s ping keepalive in `run_message_loop` continue firing during large transfers.

## Files

- **`src/main.rs`** — CLI (clap), `Client`, `Server`, `run_message_loop`, `dispatch_message`, `run_open_client`, `handle_socket_client`, `Message` enum, clipboard helpers, `init_tracing`.
- **`src/deploy.rs`** — the `clipcast deploy` subcommand: remote arch probe, cross-compile orchestration (zig + cargo-zigbuild on macOS→Linux), atomic scp install, symlink creation.
- **`README.md`** — user-facing docs; **this file (`CLAUDE.md`)** — AI/dev orientation.
- **`rustfmt.toml`** — 80-col, `use_small_heuristics = "Max"`, `imports_granularity = "Module"`.

Key entry points (grep if line numbers have drifted):

- `Client::run_connection` in `main.rs` — SSH spawn + argument plumbing for `--remote-*` flags
- `Server::run` in `main.rs` — socket bind + accept loop + relay pending map
- `run_message_loop` + `dispatch_message` in `main.rs` — the 4-arm tokio::select
- `run_open_client` in `main.rs` — argv[0] dispatch target
- `deploy::run` in `deploy.rs` — deploy pipeline entry point

## Build and run

```bash
cargo build --release
./target/release/clipcast client --host <HOST>
```

Single binary, no codegen, no build script.

## Deploy to a remote

```bash
clipcast deploy --host ec2
```

This cross-compiles for the remote target (probed via `ssh <host> 'uname -m; uname -s'`) and installs `~/.clipcast/bin/clipcast` + an `open` symlink. On a fresh macOS dev machine you'll first need `brew install zig`; `--yes` auto-installs everything else (`rustup target add`, `cargo install cargo-zigbuild`). `--dry-run` shows the full pipeline without executing.

Running remote server binaries stay pinned to their old inode via POSIX atomic rename, so deploys while a client is connected don't crash anything — reconnect the client (or `pkill clipcast` on the remote) to pick up new code.

## Conventions

- **Module split**: self-contained features >~100 LOC live in their own `src/<feature>.rs` module (like `deploy.rs`). `main.rs` is for CLI glue + cross-cutting code (the clipboard/open protocol lives there because it straddles client, server, and dispatch). Don't retroactively split working code without being asked.
- **Errors**: `Result<T, Box<dyn std::error::Error>>`, early returns, format strings for context.
- **Logging**: `tracing` (`info!`, `warn!`, `error!`). Level from `RUST_LOG`. For `clipcast deploy`, user-visible step output goes to `println!` so `--dry-run` reads cleanly on stdout.
- **No `unsafe`**. No external process for async wait — use `tokio::process::Command`.
- **No new Cargo deps without a clear reason**. Current deps: `base64`, `clap`, `clap_complete`, `rand`, `serde`/`serde_json`, `shlex`, `tokio`, `tracing`/`tracing-subscriber`.
- **Rustfmt**: `cargo fmt` before committing. The config enforces 80-col and errors on overflow.

## Non-goals (things Claude should not proactively add)

- Published GitHub release binaries + auto-download deploy mode.
- Multi-host batch deploy (`--host h1,h2,h3`).
- Automatic PATH mutation on the remote (editing `~/.bashrc` etc.).
- A long-running local daemon — the client IS the long-running local process.
- Fixing binary clipboard content — the `Clip` message is a `String`; images/binary clipboard are a known limitation, out of scope until specifically asked.
- Encryption/authentication beyond what SSH already provides.
