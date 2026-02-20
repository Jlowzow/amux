# amux - Project Report

**Polecat smoke test report for bead am-bo8**

## What is amux?

**amux** (AI Agent Multiplexer) is a terminal multiplexer written in Rust, designed for managing multiple PTY (pseudo-terminal) sessions from a single CLI. It follows a client-daemon architecture similar to tmux or GNU screen.

## Architecture

- **Client** (`src/client/`): Connects to the daemon over a Unix domain socket. Handles attach mode with bidirectional terminal streaming.
- **Daemon** (`src/daemon/`): Forks into the background, manages a registry of PTY sessions, and serves client requests.
- **Protocol** (`src/protocol/`): Length-prefixed binary framing using bincode/serde for serialization. Defines client-to-daemon and daemon-to-client message types.
- **Common** (`src/common.rs`): Shared utilities (socket paths, server health checks).

## CLI Subcommands

| Command | Description |
|---------|-------------|
| `amux new -- <cmd>` | Create a new PTY session running `<cmd>` |
| `amux attach -t <name>` | Attach to an existing session |
| `amux ls` | List active sessions |
| `amux kill -t <name>` | Kill a session |
| `amux send -t <name> <text>` | Send text to a session |
| `amux start-server` | Start the background daemon |
| `amux kill-server` | Stop daemon and all sessions |
| `amux ping` | Health check (returns "pong") |

## Key Dependencies

- **tokio**: Async runtime for bidirectional streaming
- **clap**: CLI argument parsing with derive macros
- **nix**: POSIX APIs (PTY, signals, process management)
- **crossterm**: Terminal size detection and raw mode
- **bincode/serde**: Binary serialization for the wire protocol

## Test Status

13 tests passing — covers scrollback buffer logic and protocol codec round-trips.

## Notes

- No README.md exists in the repo; this report is based on AGENTS.md, Cargo.toml, and source code analysis.
- The project is at v0.1.0, in active early development (Phase 2 per git history).
