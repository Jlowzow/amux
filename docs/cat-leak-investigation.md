# Investigation: orphan `cat` PTY leak (bd-muz)

## Verdict

**amux IS the source** of the orphan `cat` processes that exhausted
`kern.tty.ptmx_max=511`.

The leak is reproducible with `cargo test commands::attach` (or any
parallel test invocation that exercises the four `cat`-using integration
tests). Each run leaks ~4 `cat` processes — one per test that calls
`Session::spawn` with `cat`.

## Root cause

`src/daemon/session.rs::Session::spawn` does **not** set `FD_CLOEXEC` on
the PTY master (or slave) returned by `nix::pty::openpty()`. The master
fd is held by the parent process for the lifetime of the session
(io_loop owns it via `AsyncFd<OwnedFd>`).

When a *new* session is later spawned in the same process, its
`unistd::fork()` child inherits **every** prior session's master fd.
The child only explicitly drops the master for the *current* session
before `exec()`:

```rust
ForkResult::Child => {
    drop(pty.master);              // closes ONLY the new session's master
    unistd::setsid().ok();
    unsafe { libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0); }
    unistd::dup2(slave_fd, 0).ok();
    unistd::dup2(slave_fd, 1).ok();
    unistd::dup2(slave_fd, 2).ok();
    if slave_fd > 2 { drop(pty.slave); }
    let err = command.exec();      // inherited master fds survive into the exec'd process
}
```

The exec'd command (`cat` in tests, but any program — `claude`, `bash`,
…) ends up holding open every PTY master allocated for previously-spawned
sessions. That keeps each older session's *slave* attached to a master
that is impossible to fully close: even when the parent (daemon or test
process) drops *its* copy of the master, the master fd lives on inside
the unrelated descendant child. Without master close there is no
`POLLHUP` / SIGHUP / EOF on the slave, so older `cat` processes never
notice their session is dead and run forever — orphaned to `launchd`
once the original parent exits.

There is also a **concurrency window** where this affects slaves too:
when two `Session::spawn` calls overlap, the parent has both PTYs open
(master_A + slave_A + master_B + slave_B) at the moment one of the two
forks. The forked child inherits the *other* session's slave as well as
its master. We see this in production orphans (e.g. cat 30890 below
holds /dev/ttys005 as fd 61 even though its own slave is /dev/ttys004).

## Evidence

### 1 · Inspection of existing orphans

```text
$ ps -eo pid,ppid,comm | awk '$2==1 && $3=="cat"' | wc -l
48

$ lsof -p 30889
cat  30889 claude    0u  CHR 16,3   0t12   5725 /dev/ttys003   ← own slave
cat  30889 claude    1u  CHR 16,3   0t12   5725 /dev/ttys003
cat  30889 claude    2u  CHR 16,3   0t12   5725 /dev/ttys003
cat  30889 claude   47u  CHR 15,1   0t5     605 /dev/ptmx     ← inherited master
cat  30889 claude   48u  CHR 15,0   0t10    605 /dev/ptmx     ← inherited master
cat  30889 claude   56u  CHR 15,4   0t73    605 /dev/ptmx     ← inherited master
```

A normal `Session::spawn` child should hold *only* the slave for
fd 0/1/2. Every additional `/dev/ptmx` entry on a higher fd is a leaked
master from a sibling/earlier session.

`cwd` of every orphan inspected is a worktree of this repo
(`/Users/claude/Code/amux/.worktrees/bd-pmk`, etc.), confirming the
parent process was a `cargo test` run inside the amux source tree.

### 2 · Live reproduction

```text
$ ps -eo pid,ppid,comm | awk '$2==1 && $3=="cat"' | wc -l
48

$ cargo test --release commands::attach
running 11 tests
… all passing …

$ ps -eo pid,ppid,comm | awk '$2==1 && $3=="cat"' | wc -l
52   ← +4 in 6 seconds, ages 00:06

$ lsof -p 39787
cat  39787    0u CHR 16,35    0t0  6097 /dev/ttys035
cat  39787    1u CHR 16,35    0t0  6097 /dev/ttys035
cat  39787    2u CHR 16,35    0t0  6097 /dev/ttys035
cat  39787   45u CHR 15,36   0t33   605 /dev/ptmx   ← inherited master
cat  39787   51u CHR 15,37   0t62   605 /dev/ptmx   ← inherited master
```

The four newly-orphaned cats correspond exactly to the four
`cat`-using integration tests in `src/commands/attach.rs`:

- `test_follow_streams_output` (line 614)
- `test_attach_input_reaches_session` (line 178)
- `test_attach_input_individual_keys` (line 330)
- `test_attach_sync_write_then_async_read` (line 482)

Each ends with `let _ = shutdown_tx.send(());` then immediately
returns — so the per-test tokio runtime is dropped while the
`run_server` shutdown handler's
`SIGTERM → tokio::time::sleep(2s) → SIGKILL` flow is still in flight.
SIGTERM may or may not have reached the cat by then; in practice
SIGTERM does *not* reliably terminate the cat because — and here is
the fatal interaction with the missing `FD_CLOEXEC` — the cat is
still holding inherited master fds from sibling tests, so closing the
local master_async does not deliver SIGHUP to those sibling cats
either. Even when this test's own cat dies, the *previous* test's cat
is held alive by *this* test's cat through the inherited master.

### 3 · Negative controls

A single isolated `cat` session, daemon SIGKILLed:

```text
amux start-server (pid 40067)
amux new --name s1 --detached -- cat
kill -9 40067
sleep 2
# orphan-cat count unchanged → cat died via SIGHUP
```

Four sessions, daemon SIGKILLed in one shot:

```text
# all four cats clean up via the close-cascade
# (cat_4's master closes first → cat_4 dies → master_3 closes → cat_3
# dies → … → cat_1 dies)
```

The cascade only works when *every* link in the chain dies. In `cargo
test` runs the chain breaks because tests overlap and runtimes are
dropped on different schedules — leaving older cats stranded with
masters held by no-longer-running sibling test workers.

## Why the bead-style "follow + kill" reproducer doesn't show it

`amux follow … & kill $!` with a clean `kill-server --force` walks the
full graceful-shutdown path: each session's `kill_tx` is fired, the
io_loop has time to deliver `SIGTERM`, parent processes exit in the
expected order, and the close-cascade succeeds. The leak only manifests
when the parent drops master fds **out of order** relative to the
inheriting child's lifetime — which is exactly what `cargo test`
produces with parallel tests and abrupt runtime drops, and what would
also happen if a daemon-spawned session leaked into a sibling that was
itself reparented.

## Recommended follow-up (not implemented here)

1. **Set `FD_CLOEXEC` on both PTY fds** immediately after `openpty()`
   in `Session::spawn` (before the fork). The fork then can't leak
   either fd into the new child:

   ```rust
   for fd in [pty.master.as_raw_fd(), pty.slave.as_raw_fd()] {
       let f = libc::fcntl(fd, libc::F_GETFD);
       libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
   }
   ```

   `FD_CLOEXEC` is preserved through `fork()` and only honored at
   `execve()` — so the child can still `dup2(slave_fd, 0..2)` before
   `exec()` (the dups are *new* fds without `FD_CLOEXEC`) and the
   subsequent `exec()` will then close every other inherited fd
   automatically.

2. **Fix the test cleanup race.** Each test that creates a session
   should:
   - Send `KillSession` and read the resulting `Ok` (or `SessionEnded`)
     before signalling `shutdown_tx`, **or**
   - Await the spawned `run_server` task (after `shutdown_tx.send`)
     instead of relying on runtime-drop to cancel it.

   Belt and braces both — and adds a regression check that fails if a
   future change reintroduces the leak.

3. **Regression test** that fails if the leak comes back:
   - Spawn N `cat` sessions, kill the daemon ungracefully, sweep for
     `ps -eo ppid,comm | awk '$1==1 && $2=="cat"'` lines whose PIDs
     match what we forked. (This is OS-dependent and a bit annoying to
     write hermetically; the simpler check is to grep `lsof` of each
     newly-spawned cat for any extra `/dev/ptmx` fds beyond fd 0/1/2.)

A follow-up bead has been filed for the fix; this report is the
investigation deliverable for bd-muz.

## Out of scope

- Writing the fix.
- Killing the existing 48–52 orphan cats (already done by user).
- Bumping `kern.tty.ptmx_max` (treats symptom, not cause).
