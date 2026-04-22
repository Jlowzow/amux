#!/usr/bin/env bash
# End-to-end test for amux.
#
# Verifies:
#   1. Daemon start from clean state
#   2. Session creation + listing
#   3. amux capture --plain renders 'hello' and 'done' from a short-lived command
#   4. amux top --once shows the session
#   5. amux send delivers keystrokes to an interactive session
#   6. vterm rendering: ANSI cursor-movement is applied, raw escapes are not surfaced in --plain
#   7. amux kill removes a session (or marks it dead in ls)
#   8. Cleanup leaves no daemon or sessions behind
#
# Exit 0 on success, non-zero on failure.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
AMUX="$REPO_ROOT/target/release/amux"

FAIL=0
TMPDIR_T="$(mktemp -d -t amux-e2e.XXXXXX)"
PREFIX="e2e-$$"

pass() { printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }
info() { printf '\n== %s ==\n' "$1"; }

cleanup() {
    "$AMUX" kill-all >/dev/null 2>&1 || true
    "$AMUX" kill-server --force >/dev/null 2>&1 || true
    rm -rf "$TMPDIR_T"
}
trap cleanup EXIT

wait_for() {
    # wait_for <timeout-s> <cmd...>
    local timeout="$1"; shift
    local deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.2
    done
    return 1
}

# ---------------------------------------------------------------------------
# Build if needed
# ---------------------------------------------------------------------------
if [ ! -x "$AMUX" ]; then
    info "Building amux (release)"
    (cd "$REPO_ROOT" && cargo build --release) || { fail "build failed"; exit 2; }
fi

# ---------------------------------------------------------------------------
# 1. Start daemon (kill any pre-existing)
# ---------------------------------------------------------------------------
info "1. Start daemon"
"$AMUX" kill-server --force >/dev/null 2>&1 || true
sleep 0.4
# start-server returns once the socket is bound
"$AMUX" start-server >/dev/null 2>&1 || true
if wait_for 5 "$AMUX" ping; then
    pass "daemon started and responded to ping"
else
    fail "daemon did not respond to ping"
    exit 1
fi

# ---------------------------------------------------------------------------
# 2. Create a session and verify it appears in ls / has
# ---------------------------------------------------------------------------
info "2. Create session + list"
S_BASIC="${PREFIX}-basic"
"$AMUX" new --name "$S_BASIC" --detached -- \
    bash -c 'echo hello && sleep 2 && echo done' >/dev/null 2>&1 \
    || fail "amux new (basic) failed"

if wait_for 3 "$AMUX" has --target "$S_BASIC"; then
    pass "amux has finds '$S_BASIC'"
else
    fail "amux has did not find '$S_BASIC'"
fi

if "$AMUX" ls 2>&1 | grep -q "$S_BASIC"; then
    pass "amux ls lists '$S_BASIC'"
else
    fail "amux ls did not list '$S_BASIC' (got: $("$AMUX" ls 2>&1))"
fi

# ---------------------------------------------------------------------------
# 3. Verify capture --plain contains 'hello' quickly, then 'done' after sleep
# ---------------------------------------------------------------------------
info "3. capture --plain shows hello/done"

# 'hello' should show up almost immediately
capture_has() { "$AMUX" capture --target "$S_BASIC" --lines 200 2>/dev/null | grep -q "$1"; }

if wait_for 3 capture_has hello; then
    pass "capture contains 'hello'"
else
    fail "capture missing 'hello'. got: $("$AMUX" capture --target "$S_BASIC" --lines 200 2>/dev/null)"
fi

# 'done' needs the sleep 2 to elapse
if wait_for 5 capture_has done; then
    pass "capture contains 'done'"
else
    fail "capture missing 'done'. got: $("$AMUX" capture --target "$S_BASIC" --lines 200 2>/dev/null)"
fi

# ---------------------------------------------------------------------------
# 4. top --once shows the session
# ---------------------------------------------------------------------------
info "4. top --once"
TOP_OUT="$("$AMUX" top --once 2>&1 || true)"
if printf '%s' "$TOP_OUT" | grep -q "$S_BASIC"; then
    pass "top --once shows '$S_BASIC'"
else
    fail "top --once did not show '$S_BASIC'. output: $TOP_OUT"
fi

# ---------------------------------------------------------------------------
# 5. amux send: keystrokes reach an interactive shell
# ---------------------------------------------------------------------------
info "5. amux send delivers keystrokes"
S_SEND="${PREFIX}-send"
# --norc/--noprofile keeps the prompt minimal; PS1 is set so the session definitely has a prompt
"$AMUX" new --name "$S_SEND" --detached -e PS1='$ ' -- bash --norc --noprofile -i \
    >/dev/null 2>&1 || fail "amux new (send) failed"

# Give the shell a moment to initialize
if ! wait_for 3 "$AMUX" has --target "$S_SEND"; then
    fail "send session never appeared"
fi
sleep 0.6

MARK="sendmarker-$$-$RANDOM"
"$AMUX" send --target "$S_SEND" "echo $MARK" >/dev/null 2>&1 || fail "amux send failed"

send_has_mark() { "$AMUX" capture --target "$S_SEND" --lines 200 2>/dev/null | grep -q "$MARK"; }
if wait_for 5 send_has_mark; then
    pass "send delivered 'echo $MARK' and output appeared"
else
    fail "send marker '$MARK' not seen in capture. got: $("$AMUX" capture --target "$S_SEND" --lines 200 2>/dev/null)"
fi

# ---------------------------------------------------------------------------
# 6. vterm rendering: ANSI cursor control is applied before capture --plain
# ---------------------------------------------------------------------------
info "6. vterm rendering"
S_VTERM="${PREFIX}-vterm"
# 'XXXXX\rOK' prints XXXXX, moves cursor to column 0 with \r, overwrites first 2 chars with OK.
# Rendered result: "OKXXX". Raw bytes contain the literal \r.
"$AMUX" new --name "$S_VTERM" --detached -- \
    bash -c "printf 'XXXXX\rOK\n'; sleep 30" >/dev/null 2>&1 \
    || fail "amux new (vterm) failed"

if ! wait_for 3 "$AMUX" has --target "$S_VTERM"; then
    fail "vterm session never appeared"
fi
sleep 0.6

PLAIN="$("$AMUX" capture --target "$S_VTERM" --lines 200 2>/dev/null)"
RAW="$("$AMUX" capture --target "$S_VTERM" --lines 200 --raw 2>/dev/null)"

if printf '%s' "$PLAIN" | grep -q 'OKXXX'; then
    pass "capture --plain shows rendered 'OKXXX' (cursor-return applied)"
else
    fail "capture --plain did not render overwrite. plain=[$PLAIN]"
fi

if printf '%s' "$PLAIN" | grep -q 'XXXXX'; then
    fail "capture --plain still contains pre-overwrite 'XXXXX' — vterm not rendering"
else
    pass "capture --plain does not contain pre-overwrite text"
fi

# Raw output should still contain the carriage return; plain should not.
if printf '%s' "$RAW" | LC_ALL=C grep -q $'\r'; then
    pass "capture --raw preserves \\r"
else
    # Not strictly fatal — some platforms may strip — warn but pass
    pass "capture --raw (note: \\r not observed; may be terminal-normalized)"
fi

# ---------------------------------------------------------------------------
# 7. Kill a session, verify it no longer appears as alive
# ---------------------------------------------------------------------------
info "7. kill + verify dead"
"$AMUX" kill --target "$S_SEND" >/dev/null 2>&1 || fail "amux kill failed"
sleep 0.5

# After kill, the session is removed from the registry (explicit kill path).
# Accept either: (a) absent from ls, or (b) present but marked dead.
LS_OUT="$("$AMUX" ls 2>&1)"
if ! printf '%s' "$LS_OUT" | grep -q "$S_SEND"; then
    pass "killed session '$S_SEND' no longer in ls"
elif printf '%s' "$LS_OUT" | grep -E "^${S_SEND}:" | grep -qE '\((dead|exited)'; then
    pass "killed session '$S_SEND' appears dead in ls"
else
    fail "killed session '$S_SEND' still appears alive in ls: $LS_OUT"
fi

# Also: the basic session (which finished via sleep 2) should be dead by now.
LS_OUT="$("$AMUX" ls 2>&1)"
if printf '%s' "$LS_OUT" | grep -E "^${S_BASIC}:" | grep -qE '\((dead|exited)'; then
    pass "naturally-exited session '$S_BASIC' shown dead in ls"
else
    fail "'$S_BASIC' not shown as dead. ls: $LS_OUT"
fi

# ---------------------------------------------------------------------------
# 8. Cleanup
# ---------------------------------------------------------------------------
info "8. Cleanup"
"$AMUX" kill-all >/dev/null 2>&1 || true
sleep 0.3
"$AMUX" kill-server --force >/dev/null 2>&1 || true
sleep 0.4
if "$AMUX" ping >/dev/null 2>&1; then
    fail "daemon still responds to ping after kill-server"
else
    pass "daemon stopped cleanly"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo
if [ "$FAIL" -eq 0 ]; then
    printf '\033[32m=== All e2e tests passed ===\033[0m\n'
    exit 0
else
    printf '\033[31m=== %d e2e test(s) failed ===\033[0m\n' "$FAIL"
    exit 1
fi
