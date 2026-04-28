# amux

A terminal multiplexer for long-running headless processes. Manages PTY sessions through a daemon/client architecture over Unix sockets. Originally built for running AI agents in parallel; the underlying primitives are generic.

## Quick Start

```bash
cargo build && cargo install --path .

# Start a session (daemon auto-starts)
amux new -- bash

# Start a named, detached session
amux new --name worker1 --detached -- bash

# List sessions
amux ls

# Attach to a session
amux attach -t worker1

# Detach: Ctrl+B d
```

## Core Commands

### Session Lifecycle

```bash
# Create a session
amux new --name <NAME> --detached -- <CMD>

# Create in a git worktree (isolated branch)
amux new --name <NAME> --worktree <BRANCH> --detached -- <CMD>

# Create and send an initial message (implies --detached)
amux new --name <NAME> --init-message "do the thing" -- <CMD>

# Set env vars for the session
amux new -e "KEY=VALUE" -e "OTHER=VAL" -- <CMD>

# Kill a session
amux kill -t <NAME>

# Kill all sessions
amux kill --all
```

### Interacting with Sessions

```bash
# Attach (interactive, bidirectional terminal)
amux attach -t <NAME>

# Follow output (read-only stream, plain text by default)
amux follow -t <NAME>

# Follow with raw terminal output (ANSI/control chars included)
amux follow -t <NAME> --raw

# Send keystrokes into a session
amux send -t <NAME> "echo hello"

# Send literal text (no trailing newline)
amux send -t <NAME> --literal "partial input"

# Capture scrollback buffer (plain text by default)
amux capture -t <NAME> --lines 100

# Capture with raw terminal output (ANSI/control chars included)
amux capture -t <NAME> --raw
```

### Monitoring

```bash
# List all sessions (long commands are truncated in plain-text mode)
amux ls
amux ls --json

# Interactive dashboard with activity sparklines and preview pane
amux top
# Keybindings: j/k to select, Enter to attach, f to follow, q to quit

# Detailed info for one session
amux info -t <NAME>
amux info -t <NAME> --json

# Wait for a session to exit
amux wait -t <NAME>
amux wait -t <NAME> --exit-code --timeout 60

# Wait for any of several sessions
amux wait --any sess1 sess2 sess3

# Watch sessions and print exit events
amux watch sess1 sess2 sess3
amux watch sess1 --json

# Run a callback when a watched session exits
amux watch sess1 sess2 --on-exit "echo {name} exited with code {code}"
# Template vars: {name}, {code}, {pid}, {duration}

# Check if a session exists (exit 0=yes, 1=no)
amux has -t <NAME>
```

### Session Environment Variables

```bash
amux env set -t <NAME> KEY VALUE
amux env get -t <NAME> KEY
amux env list -t <NAME>
```

### Daemon Management

```bash
amux start-server
amux kill-server
amux ping
```

## Architecture

```
Client ──Unix socket──▶ Daemon
                          ├── Registry (HashMap<String, Session>)
                          ├── Session (PTY + child process + scrollback)
                          └── Reaper (cleans dead sessions every 30s)
```

- **Daemon** forks before creating the tokio runtime. Listens on `/tmp/amux-{uid}/server.sock`.
- **Wire protocol**: 4-byte big-endian length prefix + bincode payload (max 1MB).
- **Attach** uses `Ctrl+B` as the prefix key (like tmux). `Ctrl+B d` detaches.
- **Scrollback** is a 64KB ring buffer per session.
- **Protocol mismatch** between client and daemon produces a helpful error message suggesting `amux kill-server` and restart.

## Building

Requires Rust 1.56+ (edition 2021).

```bash
cargo build
cargo test
cargo build --release
```

## License

See repository for license details.

## See also

- [conductor](https://github.com/jlowzow/conductor) — coordination layer on top of amux for running multiple agent sessions.
