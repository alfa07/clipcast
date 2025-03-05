# Clipcast

A Rust-based tool for synchronizing clipboards between local and remote machines over SSH. It continuously monitors clipboard changes on both ends and automatically syncs the content, making it easy to copy-paste between machines.

## Features

- Bidirectional clipboard synchronization
- Works over SSH
- Configurable clipboard commands for different platforms
- Automatic reconnection on connection loss
- Ping/pong mechanism to ensure connection health
- Support for customizable clipboard commands

## Requirements

### Local Machine
- Rust toolchain or rust-script installed
- SSH client
- Clipboard command-line tools:
  - macOS: `pbcopy` and `pbpaste` (built-in)
  - Linux: `xclip` or similar (`apt install xclip`)
  - Windows: TBD

### Remote Machine
- Rust toolchain or rust-script installed
- X server running (for headless servers, you can use Xvfb)
- `xclip` or similar clipboard tool
- Proper environment variables set (DISPLAY, etc.)

## Installation

1. Install `clipcast` on local and remote machines
```bash
# Install locally
cargo install clipcast
# Copy to remote machine (replace 'remote-host' with your host)
ssh remote-host "cargo install clipcast"
```

2. For headless servers, ensure X server is running:
```bash
# Install Xvfb if not present
sudo apt install xvfb xclip

# Start Xvfb (typically on display :99)
Xvfb :99 -screen 0 1024x768x16 &

# Set DISPLAY variable
export DISPLAY=:99
```

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
- `--ssh-args`: Arguments for SSH session invoked by clipcast (default: )
- `--write-clipboard-cmd`: Local command to write to clipboard (default: "pbcopy")
- `--read-clipboard-cmd`: Local command to read from clipboard (default: "pbpaste")
- `--remote-server-cmd`: Remote clipcast command (default: "clipcast")
- `--remote-write-clipboard-cmd`: Remote command to write to clipboard (default: "xclip -selection clipboard")
- `--remote-read-clipboard-cmd`: Remote command to read from clipboard (default: "xclip -selection clipboard -o")

### Example Usage

Advanced usage with custom environment:
```bash
# On local machine, connecting to remote with specific environment setup
clipcast client \
  --host remote-host \
  --remote-server-cmd "source ~/.cargo/env && DISPLAY=:99 clipcast"
```

## How It Works

1. The client establishes an SSH connection to the remote server and launches server
2. Both sides monitor their local clipboards for changes
3. When a change is detected, the new clipboard content is sent to the other side
4. The receiving side updates its local clipboard
5. Regular ping/pong messages ensure the connection stays alive
6. On connection loss, the client automatically attempts to reconnect

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
