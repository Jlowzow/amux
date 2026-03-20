# amux

A terminal multiplexer for AI agents. Manages PTY sessions through a daemon/client architecture over Unix sockets. Designed to let an orchestrator agent spawn, monitor, and communicate with multiple worker agents running in parallel.

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

---

# Beads (br) — Agent Issue Tracker

`br` is a local-first issue tracker stored in SQLite + JSONL. It tracks bugs, features, and tasks as "beads" with short IDs.

## Quick Reference

```bash
br init                    # Initialize in a repo
br create --title "Fix the thing" --type bug --priority 2
br list                    # List all issues
br ready                   # List open, unblocked, not-deferred issues
br show <ID>               # View issue details
br update <ID> --status in_progress
br close <ID> --reason "Fixed by doing X"
br search "keyword"        # Full-text search
```

## Issue Fields

| Field       | Values                                    |
|-------------|-------------------------------------------|
| type        | `bug`, `feature`, `task`                  |
| priority    | `1` (critical), `2` (normal), `3` (low), `4` (backlog) |
| status      | `open`, `in_progress`, `closed`           |

## Workflow for Agents

```bash
# 1. Find work
br ready

# 2. Claim it
br update <ID> --status in_progress

# 3. Read the details
br show <ID>

# 4. Do the work, then close
br close <ID> --reason "Added the feature, tests pass"
```

## Dependencies & Epics

```bash
br dep add <ID> --blocked-by <OTHER_ID>
br blocked                 # List blocked issues
br epic create --title "Big initiative"
br epic add <EPIC_ID> <ISSUE_ID>
```

## Syncing to Git

`br` stores state in `.beads/` (JSONL files). Sync and commit periodically:

```bash
br sync --flush-only
git add .beads/ && git commit -m "sync beads"
```

---

# Agent Mail (MCP) — Inter-Agent Messaging

Agent Mail is an MCP server that provides messaging and file reservation coordination between agents. It runs as a sidecar configured in `.mcp.json`.

## Setup

Add to `.mcp.json`:
```json
{
  "mcpServers": {
    "agent-mail": {
      "command": "python",
      "args": ["-m", "mcp_agent_mail.cli", "serve-stdio"],
      "env": {
        "DATABASE_URL": "sqlite+aiosqlite:///path/to/mail.sqlite3",
        "STORAGE_ROOT": "/path/to/mailbox"
      }
    }
  }
}
```

## Key Operations

All operations use `project_key` (the absolute repo path) to scope messages to a project.

### Register

Every agent registers on startup:
```
ensure_project(human_key="/path/to/repo")
register_agent(project_key="/path/to/repo", program="claude-code", model="opus-4.6")
```

Registration returns a unique agent name (e.g., "BlueLake").

### Send Messages

```
send_message(
  project_key="/path/to/repo",
  sender_name="BlueLake",
  to=["GreenCastle"],
  subject="Status update",
  body_md="Work complete. Tests pass.",
  thread_id="bd-abc"           # optional, groups related messages
)
```

### Receive Messages

```
fetch_inbox(
  project_key="/path/to/repo",
  agent_name="GreenCastle",
  include_bodies=true
)
```

Poll periodically. Use `since_ts` for incremental fetches.

### File Reservations

Prevent two agents from editing the same file:

```
# Reserve files before editing
file_reservation_paths(
  project_key="/path/to/repo",
  agent_name="BlueLake",
  paths=["src/main.rs", "src/lib.rs"],
  reason="bd-abc"
)

# Release when done
release_file_reservations(
  project_key="/path/to/repo",
  agent_name="BlueLake"
)
```

### Other Tools

- `list_contacts` — see registered agents
- `acknowledge_message` — mark a message as read
- `search_messages` — search message history
- `fetch_topic` — filter inbox by topic tag

---

# Multi-Agent Workflow

The three tools work together in a standard pattern:

```
┌─────────────┐         ┌──────────┐        ┌──────────┐
│ Orchestrator │────────▶│  Worker1 │        │  Worker2 │
│  (amux +    │────────▶│ (claude) │        │ (claude) │
│   br + mail)│         │ worktree │        │ worktree │
└─────────────┘         └──────────┘        └──────────┘
```

1. **Orchestrator** runs `br ready` to find available work
2. For each bead, it launches a worker via `amux new --worktree <branch> --detached`
3. Workers get their assignment via `amux send` or agent-mail messages
4. Workers do the work in isolated git worktrees, commit locally
5. Orchestrator monitors via `amux capture`, `amux ls`, and agent-mail inbox
6. When done, orchestrator reviews the diff, merges the worktree branch, and kills the session

### Dispatch Example

```bash
# Mark the bead
br update bd-abc --status in_progress

# Launch worker in isolated worktree
amux new \
  --name "bead-bd-abc" \
  --worktree "SwiftFalcon" \
  --detached \
  -e "BEAD_ID=bd-abc" \
  -- claude --dangerously-skip-permissions \
     --system-prompt "$(cat agents/worker.md)"

# Send assignment
amux send -t "bead-bd-abc" "Your bead is bd-abc. Run br show bd-abc and fix it."

# Monitor
amux capture -t "bead-bd-abc" --lines 30

# When done, review and merge
git diff main...SwiftFalcon
git merge SwiftFalcon --no-ff -m "feat: description (bd-abc)"
amux kill -t "bead-bd-abc"
```

## Building

Requires Rust 1.56+ (edition 2021).

```bash
cargo build
cargo test
cargo build --release
```

## License

See repository for license details.
