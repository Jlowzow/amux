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

## PTY sizing

The agent's PTY size tracks whoever is actively viewing the session:

- **`amux new` spawns at the invoker's terminal size** (or 80x24 if amux can't read a terminal). `--rows/-r N` (clamped to [10, 500]) overrides explicitly. Spawn size is just a starting point — there is no fixed default ceiling.
- **`amux attach` owns the size while attached.** The initial Attach plus SIGWINCH → AttachResize keep the PTY in lockstep with the attacher's terminal. The session's `attach_count` is incremented for the duration of the attach.
- **`amux top` resizes the previewed session's PTY to its own terminal** when no client is attached and the agent's current size differs. Resize is gated on size mismatch (won't fire every tick) and on `attach_count == 0` (won't fight an attacher). When the top viewer's window is resized (SIGWINCH triggers a re-render), the next render pass picks up the new `terminal::size()` and propagates. See bd-is4.
- **`amux top` defers to active attachers.** When `attach_count > 0` top reads the session's current size from `SessionInfo` and renders against that — the attacher controls the canvas.
- The protocol message used by top is `ClientMessage::ResizeSession { name, cols, rows }`; it's a stateless one-shot resize distinct from `AttachResize` (which only makes sense inside an active attach connection).

## Sending input from top (bd-ly6)

`amux top` exposes two ways to push keystrokes into the highlighted agent without leaving the dashboard:

- **`i` opens a single-line input box** at the bottom of the screen targeting the currently highlighted session. Type, then Enter to send `<text>\r` (same byte sequence `amux send` produces). Esc / Ctrl-C cancels without sending. The input box overlays the summary/help row; the table and preview keep refreshing in the background. The selected row is frozen for the duration of input mode so the target the user saw when they pressed `i` is the target Enter sends to. Empty buffer + Enter sends just `\r` — the "nudge with Enter" shortcut.
- **`amux send` reads stdin when no text args are passed.** `echo hi | amux send -t Worker` forwards `hi\n` verbatim — no `\r` is appended (piped bytes are authoritative; if you wanted no terminator use `echo -n`). With text args the old behavior is unchanged: args are joined with spaces and a trailing `\r` is appended unless `--literal`. As a safety guard, `amux send -t Worker` with no args **and** an interactive stdin errors out instead of blocking on `read_to_end` waiting for Ctrl-D.

## Respawning sessions (RespawnSession)

`amux respawn -n <name> -- <cmd...>` atomically replaces a session's child
process while preserving the session itself. Equivalent to `tmux respawn-pane -k`.

Used by:
- Orchestrator /handoff: cycle the orchestrator's claude with fresh context
  after staging a handoff message — the same amux session keeps its name,
  attached client, and TTY size.
- Worker handoff pipelines: a finishing worker can kick off a different agent
  in the same amux session with a follow-up prompt, chaining work without
  leaking session bookkeeping.

Behavior:
- SIGKILLs the current child, opens a fresh PTY, forks+execs the new command.
- Scrollback and vterm parser are reset (fresh start).
- attach_count, current_size, and the session name carry over so attached
  clients keep their connection and see new output without resizing.
- AMUX_SESSION=<name> is always exported in the new child's env.
- respawn_count increments and is surfaced on SessionInfo for telemetry.

Implementation lives in `Session::respawn` (`src/daemon/session.rs`); the
server holds the registry mutex for the entire respawn so the watchdog
reaper, attach connections, and concurrent `amux respawn` calls see a
coherent mid-swap state. The io_loop checks a shared `respawn_in_progress`
flag and skips death-recording (exit_code, died_at, exit_watch.send(true))
when it's set, so attached clients don't see the swap as a SessionEnded.
A `\x1b[2J\x1b[H` clear-screen sequence is broadcast on the unchanged
output_tx at the boundary so any subscriber whose stream straddles the
respawn gets a clean visual reset rather than mixed pre/post bytes.

## Wrapping the orchestrator in amux for /handoff (bd-uhp)

For `/handoff` to cycle the orchestrator's own claude in place, the
orchestrator must be running inside an amux session. Launch pattern:

```
amux new -n orchestrator -- claude
amux attach -t orchestrator
```

Then in the attached claude, `/handoff` will:

1. Stage `handoff.msg` in the conductor mailbox (existing slash-command
   behavior).
2. Detect `$AMUX_SESSION` and `exec amux handoff --prime /conductor`,
   which respawns the session's claude with a fresh context and
   `/conductor` as its first message.

The detached attach view stays open, the new claude lights up in place,
and the conductor template loads `handoff.msg` as its first action.

When the orchestrator is **not** wrapped in amux, `/handoff` falls back
to the manual flow (user closes the window or runs `/clear`).

Building blocks:

- `AMUX_SESSION=<name>` is exported into every spawned/respawned
  child's env, so any process inside the session — slash commands,
  shell snippets, mailers — can discover its session name.
- `amux current` prints `$AMUX_SESSION` (exit 1 if unset). Pure-stdlib
  helper, no daemon roundtrip.
- `amux handoff -n <name> [--message <text>] [--prime <prompt>]
  [-- <cmd...>]` is the higher-level wrapper around RespawnSession.
  When `-n` is omitted it uses `$AMUX_SESSION`. With `--message` it
  atomically writes `<runtime_dir>/handoff/<name>.msg` (via tempfile +
  rename) before respawning. With no positional command it defaults to
  `claude` (and forwards `--prime` as claude's first message arg).

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
