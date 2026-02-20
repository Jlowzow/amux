# amux

A terminal multiplexer written in Rust. Manages multiple PTY sessions through a client-daemon architecture with Unix socket IPC.

## Architecture

- **Daemon** (`src/daemon/`) - Background process managing a registry of PTY sessions. Communicates with clients over a Unix domain socket.
- **Client** (`src/client/`) - Connects to the daemon to create, attach to, and manage sessions. Attach mode provides bidirectional terminal streaming.
- **Protocol** (`src/protocol/`) - Length-prefixed binary framing using bincode/serde serialization.
- **Common** (`src/common.rs`) - Shared utilities: socket paths, server health checks.

## Usage

```bash
# Start a new session (auto-starts daemon if needed)
amux new -- bash

# Start a named session, detached
amux new -s work -d -- bash

# List sessions
amux ls

# Attach to a session
amux attach -t work

# Send text to a session
amux send -t work "ls -la"

# Kill a session
amux kill -t work

# Daemon management
amux start-server
amux kill-server
amux ping
```

## Building

Requires Rust 1.56+ (edition 2021).

```bash
cargo build
```

## Testing

```bash
cargo test
```

## Dependencies

- **tokio** - Async runtime for bidirectional streaming
- **clap** - CLI argument parsing (derive macros)
- **nix** - POSIX APIs (PTY, signals, process management)
- **crossterm** - Terminal size detection and raw mode
- **bincode/serde** - Binary wire protocol serialization

## License

See repository for license details.
