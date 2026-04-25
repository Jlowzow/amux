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

1. Daemon listens on Unix socket at `/tmp/amux-{uid}/server.sock` (or `/tmp/amux-{uid}-{instance}/server.sock` when an instance is selected — see below).
2. Client sends `ClientMessage`, daemon replies with `DaemonMessage`
3. Wire format: 4-byte big-endian length prefix + bincode payload (max 1MB)
4. Simple commands (new, ls, kill, ping) use sync request/response (`client::request()`)
5. Attach mode upgrades to async bidirectional streaming (tokio)

## Instances

By default amux runs a single per-uid daemon. Pass `--instance <name>` (or set `AMUX_INSTANCE=<name>`) to give an invocation its own daemon, socket, pid file, and session registry under `/tmp/amux-{uid}-{name}/`. Used to run multiple orchestrators (e.g. one per project) side-by-side without their workers showing up in each other's `amux ls`. The flag wins over the env var when both are set; `main.rs` propagates the flag into `AMUX_INSTANCE` before dispatch so forked daemon children and any nested `amux` calls in scripts see the same instance.

## Adding a New Subcommand

1. **Add variant to `ClientMessage`** in `src/protocol/messages.rs`
2. **Add variant to `DaemonMessage`** if a new response type is needed
3. **Add handler** in `src/daemon/server.rs` `handle_connection()` match arm
4. **Add CLI variant** to the `Command` enum in `src/main.rs`
5. **Add dispatch** in `main()` match arm — typically calls `client::request()`

## Development Process

Use TDD (Test-Driven Development) for all changes:

1. **Red** — Write a failing test that defines the desired behavior
2. **Green** — Write the minimum code to make the test pass
3. **Refactor** — Clean up while keeping tests green

Run `cargo test` after each step to confirm state. Tests go in the same file as the code they test (inline `#[cfg(test)]` modules), following existing convention.

## Architecture Notes

- Daemon forks before creating tokio runtime (fork safety)
- Sessions use real PTYs via `openpty()` + fork/exec
- Attach uses `Ctrl+B` as prefix key (like tmux's `Ctrl+B`), `Ctrl+B d` to detach
- Server spawns a reaper task every 30s to clean dead sessions
- Logs go to `/tmp/amux-{uid}/daemon.log` (tracing with env filter)

## Multi-agent orchestration (conductor)

This repo dispatches beads to parallel worker agents using **conductor** (`~/Code/conductor`), which sits on top of amux + a file-based mailbox at `/tmp/conductor-mail/`.

To enter orchestrator mode in this session, run the `/conductor` slash command — it loads the playbook from `~/Code/conductor/templates/CLAUDE.md`. The skill makes you the orchestrator: you survey `br ready`, assign beads via `conductor assign <Name> "..."`, and spawn workers via `conductor spawn <Name> /Users/claude/Code/amux`. Workers run `claude --dangerously-skip-permissions` in their own amux session and report DONE/FAIL back via `mail send orchestrator ...`.

Key files:
- `~/Code/conductor/conductor` — `init|spawn|assign|status|kill|mail` wrapper around amux.
- `~/Code/conductor/templates/CLAUDE.md` — the orchestrator playbook (canonical).
- `~/Code/conductor/.claude/agents/worker.md` — worker subagent definition; encodes the "mail orchestrator, never the user" rule.

House rules for orchestration in this repo:
- **Workers run in their own git worktree** under `.worktrees/<bd-id>` on a `<bd-id>-work` branch. Never spawn two agents in the same tree.
- **Workers commit before any `kill-server` or `cargo build`.** Agents working on `src/daemon/**` can self-kill via daemon restart and lose uncommitted work otherwise.
- **One bead per worker.** Don't double-book. If beads touch overlapping files, dispatch sequentially.
- **The worktree's `.beads/` is empty by design.** Workers must use `br --db /Users/claude/Code/amux/.beads/beads.db show <bd-id>` to read beads from inside a worktree.
- **Don't restart the amux daemon while workers are alive** — every session shares one daemon.
