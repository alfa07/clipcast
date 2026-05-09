# Clipcast

A Rust-based tool for synchronizing clipboards between local and remote machines over SSH, plus a `remote → local` file-open feature: run `open foo.pdf` on the remote and the file streams to your Mac and opens in the native GUI app.

## Features

- Bidirectional clipboard synchronization over SSH
- **Remote open**: `open <file>` on the remote ships the file to the local Mac and launches `open` on it, with an extension allowlist for safety
- One-command deploy (`clipcast deploy --host <HOST>`) that cross-compiles locally and installs the binary + `open` symlink on the remote
- Configurable clipboard commands for different platforms
- Automatic reconnection on connection loss
- Ping/pong mechanism to ensure connection health

## Requirements

### Local Machine
- Rust toolchain or rust-script installed
- SSH client
- Clipboard command-line tools:
  - macOS: `pbcopy` and `pbpaste` (built-in)
  - Linux: `xclip` or similar (`apt install xclip`)
  - Windows: TBD

### Remote Machine
- Linux or macOS (`x86_64` or `aarch64`) — `clipcast deploy` cross-compiles the right binary from your Mac; Rust is **not** required on the remote
- X server running (for headless servers, you can use Xvfb)
- `xclip` or similar clipboard tool
- Proper environment variables set (DISPLAY, etc.)

## Installation

Build and install locally:

```bash
cargo install --path .
# or
cargo build --release && cp target/release/clipcast ~/bin/
```

For the remote, use the built-in deploy command (see below) — it cross-compiles the right binary and installs it for you. You do not need Rust on the remote.

For headless servers, ensure X server is running so the remote clipboard is accessible:

```bash
sudo apt install xvfb xclip
Xvfb :99 -screen 0 1024x768x16 &
export DISPLAY=:99
```

## Deployment

`clipcast deploy` probes the remote for arch/OS, cross-compiles the correct target locally, atomically scps the binary into `~/.clipcast/bin/clipcast`, and creates an `open` symlink next to it:

```bash
clipcast deploy --host ec2
```

Add the install dir to the remote `PATH` (once):

```bash
ssh ec2 'echo "export PATH=\"\$HOME/.clipcast/bin:\$PATH\"" >> ~/.bashrc'
```

### Cross-compile prerequisites

Deploying from macOS to Linux uses [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) + [zig](https://ziglang.org/) as the linker. First-run setup:

```bash
brew install zig                      # manual — Homebrew is never auto-driven
cargo install cargo-zigbuild          # or add --yes to deploy to auto-install this
rustup target add x86_64-unknown-linux-musl   # ditto
```

Subsequent deploys reuse the toolchain and are seconds-fast (incremental cargo + small scp).

### Deploy flags

| Flag | Default | Purpose |
|---|---|---|
| `--host <HOST>` | *(required)* | SSH host |
| `--ssh-args <STR>` | `""` | Forwarded to ssh/scp |
| `--install-dir <PATH>` | `~/.clipcast/bin` | Remote install directory |
| `--symlinks <CSV>` | `open` | Symlink names to create next to the binary |
| `--target <TRIPLE>` | *auto-detect* | Override detected target triple |
| `--yes` | `false` | Auto-install missing user-scope tools (`rustup target`, `cargo-zigbuild`) |
| `--skip-build` | `false` | Reuse existing `target/<triple>/release/clipcast` |
| `--dry-run` | `false` | Print every step without executing |

### Deploying while a client is connected

The deploy uses an atomic `mv` into place, so if a `clipcast client` is already connected to the remote, the running `clipcast server` keeps its old-inode FD and continues working unaffected. Reconnect the client (or `pkill clipcast` on the remote) to pick up the new binary.

### Alternative deploy modes (not implemented)

- **rsync sources + build on remote** — simpler on remotes that already have rustc installed; not implemented in v1 because local cross-compile produces a faster iteration loop.
- **Fetch prebuilt binaries from GitHub releases** — nice for "install from scratch" but can't deploy uncommitted local changes; would need a CI matrix. Future work.

## Usage

The tool has two modes: server and client.

### Server Mode

In server mode server gets messages over stdin and replys over stdout
using one json message per line protocol. Server will be launched by
the client on the remote machine.

```bash
clipcast server [OPTIONS]
```

Options:
- `--write-clipboard-cmd`: Command to write to clipboard (default: "xclip -selection clipboard")
- `--read-clipboard-cmd`: Command to read from clipboard (default: "xclip -selection clipboard -o")

### Client Mode

Client using `ssh` launches server on the remote host and syncs clipboards
between machines using json per line protocol over stdin and stdout.


```bash
clipcast client --host REMOTE_HOST [OPTIONS]
```

Options:
- `--host`: SSH host to connect to (required)
- `--ssh-args`: Arguments for SSH session invoked by clipcast (default: "")
- `--write-clipboard-cmd`: Local command to write to clipboard (default: "pbcopy")
- `--read-clipboard-cmd`: Local command to read from clipboard (default: "pbpaste")
- `--remote-server-cmd`: Remote clipcast command (default: "clipcast")
- `--remote-write-clipboard-cmd`: Remote command to write to clipboard (default: "xclip -selection clipboard")
- `--remote-read-clipboard-cmd`: Remote command to read from clipboard (default: "xclip -selection clipboard -o")

### Example Usage

#### Advanced usage with custom environment:
```bash
# On local machine, connecting to remote with specific environment setup
clipcast client \
  --host remote-host \
  --remote-server-cmd "source ~/.cargo/env && DISPLAY=:99 clipcast"
```

#### Run clipcast automatically alongside a SSH session:

Create the following three files, editing as needed

`~/.ssh/config`
```bash
Include ~/.ssh/default_config

# Overload included default configuration and execute clipcast
Host DOMAIN1* DOMAIN2* DOMAIN3*
  LocalCommand $HOME/.ssh/clipcast.sh %h
```

`~/.ssh/default_config`
```bash
Host DOMAIN1* DOMAIN2* DOMAIN3*
  HostName %h.REMAINING
  User USER
  IdentityFile ~/.ssh/PRIVATE_KEY
  ForwardX11 yes
```

`~/.ssh/clipcast.sh`
```bash
#!/bin/bash

# Get domain from remote host; e.g., domain from domain.example.com
host="${1%%.*}"

# Run clipcast using default config to prevent recursive LocalCommand spawning in ~/.ssh/config
clipcast client --ssh-args "-F $HOME/.ssh/default_config" --host $host > /dev/null 2>&1 &
pid=$!

(
  # Block until parent SSH process no longer exists
  while kill -0 $PPID 2>/dev/null; do
    sleep 1
  done

  # Kill all clipcast child process and created clipcast
  pkill -P $pid
  kill $pid
) &
```

`~/.ssh/default_config` should contain the default setup for your SSH sessions. `~/.ssh/config` overloads `default_config` and runs the `clipcast.sh` script. We use this setup such that when `clipcast.sh` runs clipcast (which itself then runs SSH), the `LocalCommand` to run clipcast will not recursively execute.

## Remote Open

Once clipcast is deployed (`clipcast deploy --host ec2`) and a client is running on your Mac (`clipcast client --host ec2`), you can open remote files in your local macOS GUI apps directly:

```bash
# on the remote:
open report.pdf            # Preview launches on the Mac
open foo.png bar.png       # both open in Preview
open -a Safari https://...  # flags and URLs pass through unchanged
```

### How it works

The `open` command on the remote is a symlink to `clipcast`. When invoked, clipcast's `argv[0]` dispatch routes to the open-client code, which:

1. Classifies each argument — flags (`-...`), URLs (`://`), and non-existent paths pass through literally; existing files get shipped.
2. Connects to the local unix socket (`$XDG_RUNTIME_DIR/clipcast-$USER.sock`) owned by the running `clipcast server`.
3. Streams each file in 256 KiB base64 chunks through the SSH channel to your Mac.
4. The Mac client writes files under `~/.clipcast/remote/<host>/<ts>-<rand>/`, checks them against an extension allowlist, rebuilds the argument vector with local paths, and runs `open` on them.
5. Returns `0` if macOS `open` launched successfully, non-zero with an error message otherwise.

### Limits

- ≤ 50 MiB per file
- ≤ 1024 files per call
- ≤ 250 MiB per call total

Files larger than these limits, or with extensions not in the allowlist (default covers common docs/images/media), are either rejected up front (limits) or saved-but-not-opened (allowlist) with an error returned to the remote caller. Override the allowlist with `clipcast client --open-allowlist pdf,png,txt,...`.

Running `.app` bundles, shell scripts, or unknown extensions is intentionally blocked by default — the remote SSH session is a code-exec surface you should not hand to macOS `open` blindly.

## How It Works

1. The client establishes an SSH connection to the remote server and launches server
2. Both sides monitor their local clipboards for changes
3. When a change is detected, the new clipboard content is sent to the other side
4. The receiving side updates its local clipboard
5. Regular ping/pong messages ensure the connection stays alive
6. On connection loss, the client automatically attempts to reconnect
7. The remote server also binds a unix socket and relays incoming `open` requests onto the same SSH channel (see **Remote Open** above)

## Troubleshooting

1. **Clipboard Not Syncing**
   - Verify X server is running on remote machine
   - Check DISPLAY environment variable is set correctly
   - Ensure clipboard tools (xclip, pbcopy, etc.) are installed and working

2. **Connection Issues**
   - Check SSH connectivity
   - Verify proper permissions on the clipcast binary
   - Check firewall settings

3. **Permission Errors**
   - Ensure the binary is executable (`chmod +x`)
   - Verify user has permission to access clipboard tools

## Environment Variables

- `RUST_LOG`: Controls logging level (default: "info")
  - Available levels: error, warn, info, debug, trace

## Contributing

Feel free to open issues and pull requests for:
- Bug fixes
- New features
- Documentation improvements
- Platform support expansion

## License

This project is open source and available under the MIT License.
