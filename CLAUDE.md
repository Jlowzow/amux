# amux - AI Agent Multiplexer

A tmux-like terminal multiplexer for AI agents. Daemon manages PTY sessions; clients communicate via Unix socket IPC.

## Build & Test

```bash
cargo build           # Build
cargo test            # Run all tests (13 unit tests)
cargo build --release # Release build
```

## Module Layout

```
src/
├── main.rs              # CLI entry point (clap), command dispatch, do_attach()
├── common.rs            # Shared utilities: runtime_dir(), socket_path(), PID file ops
├── protocol/
│   ├── mod.rs           # Re-exports messages::*
│   ├── messages.rs      # ClientMessage/DaemonMessage enums, SessionInfo struct
│   └── codec.rs         # Length-prefixed bincode framing (sync + async)
├── client/
│   ├── mod.rs           # connect(), request() — sync Unix socket client
│   └── attach.rs        # Interactive attach loop: raw terminal, Ctrl+B prefix key
└── daemon/
    ├── mod.rs           # fork_daemon() — forks, setsid, sets up tokio runtime
    ├── server.rs        # run_server() — accepts connections, dispatches handlers
    ├── registry.rs      # Registry — HashMap<String, Session>, create/kill/list/reap
    └── session.rs       # Session::spawn() — PTY fork, io_loop, Scrollback ring buffer
```

## Key Types

- **`ClientMessage`** (`protocol/messages.rs`) — Request enum: Ping, CreateSession, ListSessions, KillSession, Attach, AttachInput, AttachResize, Detach, SendText, KillServer
- **`DaemonMessage`** (`protocol/messages.rs`) — Response enum: Pong, Ok, Error, SessionCreated, SessionList, Output, SessionEnded
- **`SessionInfo`** (`protocol/messages.rs`) — Session metadata: name, command, pid, alive
- **`Session`** (`daemon/session.rs`) — Holds child PID, channel handles (input_tx, output_tx, resize_tx, kill_tx), scrollback buffer
- **`Registry`** (`daemon/registry.rs`) — Session store with auto-naming and dead-session reaper
- **`Scrollback`** (`daemon/session.rs`) — 64KB ring buffer (`VecDeque<u8>`) for session output

## IPC Pattern

1. Daemon listens on Unix socket at `/tmp/amux-{uid}/server.sock`
2. Client sends `ClientMessage`, daemon replies with `DaemonMessage`
3. Wire format: 4-byte big-endian length prefix + bincode payload (max 1MB)
4. Simple commands (new, ls, kill, ping) use sync request/response (`client::request()`)
5. Attach mode upgrades to async bidirectional streaming (tokio)

## Adding a New Subcommand

1. **Add variant to `ClientMessage`** in `src/protocol/messages.rs`
2. **Add variant to `DaemonMessage`** if a new response type is needed
3. **Add handler** in `src/daemon/server.rs` `handle_connection()` match arm
4. **Add CLI variant** to the `Command` enum in `src/main.rs`
5. **Add dispatch** in `main()` match arm — typically calls `client::request()`

## Architecture Notes

- Daemon forks before creating tokio runtime (fork safety)
- Sessions use real PTYs via `openpty()` + fork/exec
- Attach uses `Ctrl+B` as prefix key (like tmux's `Ctrl+B`), `Ctrl+B d` to detach
- Server spawns a reaper task every 30s to clean dead sessions
- Logs go to `/tmp/amux-{uid}/daemon.log` (tracing with env filter)
