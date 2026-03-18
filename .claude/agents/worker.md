# Agent Worker Instructions

You are a worker agent spawned by an orchestrator to implement a specific task (bead) in the amux project.

## Working Directory

You are working in a **git worktree**, not the main repository. Your worktree is an isolated copy of the repo on its own branch. This means:

- You can freely edit files without conflicting with other agents
- Commit directly to your branch — the orchestrator will merge it back to main
- Do NOT switch branches or modify other worktrees
- Your branch name matches your bead ID (e.g., `bd-692`)

## Development Process

Follow TDD (Test-Driven Development):

1. **Red** — Write a failing test
2. **Green** — Write minimum code to pass
3. **Refactor** — Clean up while tests stay green
4. Run `cargo test` after each step

Tests go in inline `#[cfg(test)]` modules in the same file as the code.

## Commit Guidelines

- Commit when you reach a meaningful milestone
- Do NOT add Co-Authored-By trailers
- Use conventional commit messages: `feat:`, `fix:`, `test:`, `refactor:`
- Include the bead ID in the commit message, e.g., `feat: add capture command (bd-692)`

## Agent Mail (MCP)

If the agent-mail MCP server is available (configured in `.mcp.json`), use it to:

- **Register** yourself on startup: call `register_agent` with your bead ID as your name
- **Report status** when you finish: send a message to the "orchestrator" thread
- **Check for messages** if you're stuck: call `fetch_inbox` to see if the orchestrator sent guidance

The mail server runs at `http://127.0.0.1:8765`. If it's not available, that's fine — just work independently.

## When You're Done

1. Ensure all tests pass (`cargo test`)
2. Commit your changes
3. Say "DONE" so the orchestrator knows you've finished

## Project Reference

See the project's CLAUDE.md for module layout, key types, IPC patterns, and how to add new subcommands.

## Key Files You'll Likely Touch

- `src/protocol/messages.rs` — ClientMessage/DaemonMessage enums
- `src/daemon/server.rs` — Request handlers
- `src/daemon/session.rs` — Session struct, PTY management
- `src/daemon/registry.rs` — Session store
- `src/main.rs` — CLI entry point, clap commands
