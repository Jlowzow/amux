#!/usr/bin/env bash
# End-to-end test for `amux top`'s preview pane.
#
# Technique:
#   1. Spawn a "target" session that produces known output.
#   2. Spawn a "viewer" session that runs `amux top` (the live TUI).
#   3. Capture the viewer's own output via `amux capture --plain` — this
#      returns the vterm-rendered screen of the top TUI, preview pane and all.
#   4. Extract the preview pane from that capture and verify it:
#        - contains the expected text (not empty, not "(no output)"),
#        - has no raw ESC sequences (vterm actually rendered),
#        - is not garbled (not >50% single-character lines).
#
# Session name convention: target sessions are prefixed "00-ec1-<pid>-"
# so they sort before any non-test session and `amux top` auto-selects
# them at the preview pane. Viewer sessions use "zzz-" to sort last.
#
# Exits 0 on pass, non-zero (with diagnostic output) on fail.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
AMUX="$REPO_ROOT/target/release/amux"

FAIL=0
PID=$$
CREATED=()

pass() { printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$1" >&2; FAIL=$((FAIL + 1)); }
info() { printf '\n== %s ==\n' "$1"; }

cleanup() {
    local s
    for s in "${CREATED[@]:-}"; do
        [ -n "$s" ] && "$AMUX" kill --target "$s" >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT

track() { CREATED+=("$1"); }

wait_for() {
    local timeout="$1"; shift
    local deadline=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.3
    done
    return 1
}

# Extract the preview pane body (between "Preview:" header and summary line)
# from a viewer session's --plain capture.
extract_preview() {
    local viewer="$1"
    "$AMUX" capture --target "$viewer" --lines 100 2>/dev/null | awk '
        /^ *Preview:/          { in_preview = 1; next }
        in_preview && /^[0-9]+ sessions \(/ { exit }
        in_preview && /j\/k:select/         { exit }
        in_preview { print }
    '
}

# Print the session name shown in the viewer's "Preview: <name>" header.
preview_target() {
    local viewer="$1"
    "$AMUX" capture --target "$viewer" --lines 100 2>/dev/null \
        | grep -oE 'Preview: [A-Za-z0-9_-]+' \
        | head -n 1 \
        | awk '{print $2}'
}

# Return output_bytes for a session via ls --json, or -1 if absent.
session_output_bytes() {
    local name="$1"
    "$AMUX" ls --json 2>/dev/null | python3 -c "
import sys, json
target = sys.argv[1]
try:
    data = json.load(sys.stdin)
except Exception:
    print(-1); sys.exit(0)
for s in data:
    if s.get('name') == target:
        print(s.get('output_bytes', -1)); sys.exit(0)
print(-1)
" "$name" 2>/dev/null
}

# Returns 0 (true) if >50% of non-empty lines are a single non-whitespace char.
is_garbled() {
    local text="$1"
    local total=0 single=0 line trimmed
    while IFS= read -r line; do
        trimmed="$(printf '%s' "$line" | tr -d '[:space:]')"
        [ -n "$trimmed" ] || continue
        total=$((total + 1))
        [ "${#trimmed}" -le 1 ] && single=$((single + 1))
    done <<< "$text"
    [ "$total" -gt 0 ] && [ $((single * 2)) -gt "$total" ]
}

# Returns 0 (true) if a raw "ESC [" sequence appears in the text.
has_raw_escapes() {
    printf '%s' "$1" | LC_ALL=C grep -q $'\x1b\['
}

# ---------------------------------------------------------------------------
# Preconditions
# ---------------------------------------------------------------------------
if [ ! -x "$AMUX" ]; then
    info "Building amux (release)"
    (cd "$REPO_ROOT" && cargo build --release) || { echo "build failed" >&2; exit 2; }
fi

if ! "$AMUX" ping >/dev/null 2>&1; then
    "$AMUX" start-server >/dev/null 2>&1 || true
    if ! wait_for 5 "$AMUX" ping; then
        fail "daemon not running and could not be started"
        exit 1
    fi
fi

# ---------------------------------------------------------------------------
# TEST 1 — Plain-text target, preview pane should show its lines verbatim.
#
# We produce 12 uniquely-tagged lines so that the preview has to render a
# *full* screen of content, not just a few lines clinging to the top. This
# catches layout bugs where the summary's absolute cursor jump clobbers the
# last rendered preview row (see bd-9ep).
# ---------------------------------------------------------------------------
info "1. Preview of a plain-text session"
T1="00-ec1-${PID}-1-plain"
V1="zzz-ec1-${PID}-1-viewer"
TOKEN1="UNIQMARKER_ALPHA_BETAGAMMA"
NLINES1=12
NLINES1_PADDED="$(printf '%02d' "$NLINES1")"

# A single printf emits all N tokens, each on its own line. Keeping this in
# one command avoids multi-line parsing woes when quoted through bash -c.
SCRIPT1="for i in \$(seq 1 ${NLINES1}); do printf '%s_%02d\\n' '${TOKEN1}' \"\$i\"; done; sleep 120"

"$AMUX" new --name "$T1" --detached -- bash -c "$SCRIPT1" \
    >/dev/null 2>&1 || fail "spawn target '$T1' failed"
track "$T1"

wait_for 3 "$AMUX" has --target "$T1" || fail "target '$T1' never appeared"

# Wait for the *last* token to land — if we only waited for the first, an
# output truncation bug could pass silently.
target_has_last_token() {
    "$AMUX" capture --target "$T1" --lines 50 2>/dev/null \
        | grep -q "${TOKEN1}_${NLINES1_PADDED}"
}
wait_for 4 target_has_last_token || fail "target '$T1' never produced last token '${TOKEN1}_${NLINES1_PADDED}'"

"$AMUX" new --name "$V1" --detached -- "$AMUX" top >/dev/null 2>&1 \
    || fail "spawn viewer '$V1' failed"
track "$V1"

viewer_has_preview() { "$AMUX" capture --target "$V1" --lines 100 2>/dev/null | grep -q 'Preview:'; }
wait_for 8 viewer_has_preview || fail "viewer '$V1' never rendered 'Preview:' header"

SELECTED="$(preview_target "$V1" || true)"
if [ "$SELECTED" != "$T1" ]; then
    fail "viewer selected '$SELECTED' — expected '$T1' (an earlier-sorting alive session exists?)"
fi

PREVIEW1="$(extract_preview "$V1")"

if printf '%s' "$PREVIEW1" | grep -q "$TOKEN1"; then
    pass "preview contains target's token ($TOKEN1)"
else
    fail "preview missing '$TOKEN1'. preview was:
---
$PREVIEW1
---"
fi

# Count how many of the 12 unique tokens actually landed in the preview body.
# A layout bug that lets the summary line overwrite the last preview row (or
# that short-circuits preview rendering) shows up here as a missing tail
# token, even though earlier tokens would still pass the simple "contains"
# check above. This is the test guard for bd-9ep.
FOUND1=0
for i in $(seq 1 "$NLINES1"); do
    TOK="$(printf '%s_%02d' "$TOKEN1" "$i")"
    if printf '%s' "$PREVIEW1" | grep -q "$TOK"; then
        FOUND1=$((FOUND1 + 1))
    fi
done
if [ "$FOUND1" -ge "$NLINES1" ]; then
    pass "preview contains all $NLINES1 emitted tokens"
else
    fail "preview is missing some of the $NLINES1 tokens — only found $FOUND1. preview was:
---
$PREVIEW1
---"
fi

# Specifically: the LAST token must appear. A classic summary-overwrites-
# preview bug kills exactly the bottom row of the preview pane.
LAST_TOKEN="${TOKEN1}_${NLINES1_PADDED}"
if printf '%s' "$PREVIEW1" | grep -q "$LAST_TOKEN"; then
    pass "preview contains the final token ($LAST_TOKEN) — bottom row survived"
else
    fail "preview missing the final token ($LAST_TOKEN) — summary may be overwriting the last preview row. preview was:
---
$PREVIEW1
---"
fi

if printf '%s' "$PREVIEW1" | grep -q '(no output)'; then
    BYTES="$(session_output_bytes "$T1")"
    if [ "${BYTES:-0}" -gt 0 ]; then
        fail "preview shows '(no output)' despite target reporting $BYTES output bytes"
    else
        pass "'(no output)' shown consistent with 0 bytes"
    fi
else
    pass "preview is not '(no output)'"
fi

if has_raw_escapes "$PREVIEW1"; then
    fail "preview contains raw ESC sequences (vterm did not render)"
else
    pass "preview has no raw ESC sequences"
fi

if is_garbled "$PREVIEW1"; then
    fail "preview looks garbled (>50% single-char lines):
---
$PREVIEW1
---"
else
    pass "preview is not garbled"
fi

"$AMUX" kill --target "$V1" >/dev/null 2>&1 || true
"$AMUX" kill --target "$T1" >/dev/null 2>&1 || true
sleep 0.4

# ---------------------------------------------------------------------------
# TEST 2 — TUI-style cursor-addressed redraws should render, not surface
# raw escape codes, and should show the FINAL state only.
# ---------------------------------------------------------------------------
info "2. Preview of a TUI-style session (cursor-addressed redraws)"
T2="00-ec1-${PID}-2-tui"
V2="zzz-ec1-${PID}-2-viewer"

# Do three draft redraws, then a stable final frame.
SCRIPT2='
for i in 1 2 3; do
  printf "\033[2J\033[1;1HDRAFT_FRAME_%d" "$i"
  sleep 0.2
done
printf "\033[2J\033[1;1HSTABLE_TUI_TITLE\n"
printf "  row_two: FINAL_STATE_MARKER\n"
printf "  row_three: READY_FLAG\n"
sleep 120
'
"$AMUX" new --name "$T2" --detached -- bash -c "$SCRIPT2" >/dev/null 2>&1 \
    || fail "spawn target '$T2' failed"
track "$T2"

wait_for 3 "$AMUX" has --target "$T2" || fail "target '$T2' never appeared"

# Let the redraw sequence settle on the final frame
sleep 1.5

"$AMUX" new --name "$V2" --detached -- "$AMUX" top >/dev/null 2>&1 \
    || fail "spawn viewer '$V2' failed"
track "$V2"

viewer2_has_preview() { "$AMUX" capture --target "$V2" --lines 100 2>/dev/null | grep -q 'Preview:'; }
wait_for 8 viewer2_has_preview || fail "viewer '$V2' never rendered 'Preview:' header"

SELECTED="$(preview_target "$V2" || true)"
if [ "$SELECTED" != "$T2" ]; then
    fail "viewer selected '$SELECTED' — expected '$T2'"
fi

PREVIEW2="$(extract_preview "$V2")"

if printf '%s' "$PREVIEW2" | grep -q 'STABLE_TUI_TITLE' \
   && printf '%s' "$PREVIEW2" | grep -q 'FINAL_STATE_MARKER'; then
    pass "preview shows rendered final frame (STABLE_TUI_TITLE + FINAL_STATE_MARKER)"
else
    fail "preview missing final-frame text. preview was:
---
$PREVIEW2
---"
fi

# Drafts were overwritten by \x1b[2J — none should appear.
if printf '%s' "$PREVIEW2" | grep -q 'DRAFT_FRAME_'; then
    fail "preview still contains pre-clear DRAFT_FRAME_ text — vterm not applying \\x1b[2J"
else
    pass "preview does not contain pre-clear DRAFT_FRAME_ lines"
fi

if has_raw_escapes "$PREVIEW2"; then
    fail "preview contains raw ESC sequences (vterm did not render)"
else
    pass "no raw ESC sequences — vterm rendered output"
fi

if is_garbled "$PREVIEW2"; then
    fail "preview looks garbled (>50% single-char lines):
---
$PREVIEW2
---"
else
    pass "preview is not garbled"
fi

"$AMUX" kill --target "$V2" >/dev/null 2>&1 || true
"$AMUX" kill --target "$T2" >/dev/null 2>&1 || true
sleep 0.4

# ---------------------------------------------------------------------------
# TEST 3 — Optional: interactive `claude` session, preview should contain
# readable 3+char tokens rather than single scattered characters.
# ---------------------------------------------------------------------------
info "3. Preview of an interactive claude session (optional)"
if ! command -v claude >/dev/null 2>&1; then
    pass "skipped (claude not in PATH)"
else
    T3="00-ec1-${PID}-3-claude"
    V3="zzz-ec1-${PID}-3-viewer"

    "$AMUX" new --name "$T3" --detached -- claude --dangerously-skip-permissions \
        >/dev/null 2>&1 || fail "spawn claude target '$T3' failed"
    track "$T3"

    claude_has_output() {
        local c
        c="$("$AMUX" capture --target "$T3" --lines 50 2>/dev/null | wc -c)"
        [ "$c" -gt 100 ]
    }
    if ! wait_for 15 claude_has_output; then
        fail "claude session never produced substantial output (>100 chars)"
    else
        "$AMUX" new --name "$V3" --detached -- "$AMUX" top >/dev/null 2>&1 \
            || fail "spawn viewer '$V3' failed"
        track "$V3"

        viewer3_has_preview() { "$AMUX" capture --target "$V3" --lines 100 2>/dev/null | grep -q 'Preview:'; }
        wait_for 8 viewer3_has_preview || fail "viewer '$V3' never rendered 'Preview:' header"

        SELECTED="$(preview_target "$V3" || true)"
        if [ "$SELECTED" != "$T3" ]; then
            fail "viewer selected '$SELECTED' — expected '$T3'"
        fi

        PREVIEW3="$(extract_preview "$V3")"

        if printf '%s' "$PREVIEW3" | grep -qE '[A-Za-z0-9]{3,}'; then
            pass "claude preview contains readable 3+char tokens"
        else
            fail "claude preview has no 3+char tokens — likely garbled:
---
$PREVIEW3
---"
        fi

        if printf '%s' "$PREVIEW3" | grep -q '(no output)'; then
            BYTES="$(session_output_bytes "$T3")"
            if [ "${BYTES:-0}" -gt 0 ]; then
                fail "claude preview shows '(no output)' despite $BYTES output bytes"
            fi
        fi

        if has_raw_escapes "$PREVIEW3"; then
            fail "claude preview contains raw ESC sequences"
        else
            pass "claude preview has no raw ESC sequences"
        fi

        if is_garbled "$PREVIEW3"; then
            fail "claude preview is garbled (>50% single-char lines):
---
$PREVIEW3
---"
        else
            pass "claude preview is not garbled"
        fi
    fi

    "$AMUX" kill --target "$V3" >/dev/null 2>&1 || true
    "$AMUX" kill --target "$T3" >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# TEST 4 — j/k selection: sending 'j'/'k' to a live `amux top` viewer must
# move the preview selection between sessions. This is the interactive-mode
# counterpart to the unit tests on handle_key() — it proves that the real
# PTY path (amux send -> daemon -> PTY -> crossterm::event::read) actually
# reaches handle_key and that the preview pane re-renders accordingly.
# ---------------------------------------------------------------------------
info "4. j/k moves preview selection between sessions"
T4A="00-pes-${PID}-4-a-alpha"
T4B="00-pes-${PID}-4-b-bravo"
V4="zzz-pes-${PID}-4-viewer"
TOKEN_A="UNIQ_JKSEL_ALPHA_TOKEN"
TOKEN_B="UNIQ_JKSEL_BRAVO_TOKEN"

# Both targets print their distinctive token and then idle. Each runs in bash
# -c so the heredoc-style script survives the exec into PTY.
"$AMUX" new --name "$T4A" --detached -- bash -c "printf '%s\\n' '${TOKEN_A}'; sleep 120" \
    >/dev/null 2>&1 || fail "spawn target '$T4A' failed"
track "$T4A"
"$AMUX" new --name "$T4B" --detached -- bash -c "printf '%s\\n' '${TOKEN_B}'; sleep 120" \
    >/dev/null 2>&1 || fail "spawn target '$T4B' failed"
track "$T4B"

wait_for 3 "$AMUX" has --target "$T4A" || fail "target '$T4A' never appeared"
wait_for 3 "$AMUX" has --target "$T4B" || fail "target '$T4B' never appeared"

# Wait for both tokens to actually hit scrollback so the preview has content
# to display on whichever side we select.
a_has_token() { "$AMUX" capture --target "$T4A" --lines 20 2>/dev/null | grep -q "$TOKEN_A"; }
b_has_token() { "$AMUX" capture --target "$T4B" --lines 20 2>/dev/null | grep -q "$TOKEN_B"; }
wait_for 4 a_has_token || fail "target '$T4A' never produced '$TOKEN_A'"
wait_for 4 b_has_token || fail "target '$T4B' never produced '$TOKEN_B'"

"$AMUX" new --name "$V4" --detached -- "$AMUX" top >/dev/null 2>&1 \
    || fail "spawn viewer '$V4' failed"
track "$V4"

viewer4_has_preview() { "$AMUX" capture --target "$V4" --lines 100 2>/dev/null | grep -q 'Preview:'; }
wait_for 8 viewer4_has_preview || fail "viewer '$V4' never rendered 'Preview:' header"

# Poll the viewer's "Preview: <name>" header until it matches expected, or
# fail after ~6s. amux top polls stdin every 2s so a single keystroke should
# land within that window; we give a couple of cycles of slack.
wait_for_preview_target() {
    local viewer="$1" expected="$2"
    local deadline=$(( $(date +%s) + 6 ))
    local current
    while [ "$(date +%s)" -lt "$deadline" ]; do
        current="$(preview_target "$viewer" || true)"
        [ "$current" = "$expected" ] && return 0
        sleep 0.3
    done
    return 1
}

# Initial state: alpha sorts before bravo, so alpha should be selected.
if wait_for_preview_target "$V4" "$T4A"; then
    pass "initial preview selects '$T4A' (alphabetically first)"
else
    CURRENT="$(preview_target "$V4" || echo '<none>')"
    fail "initial preview selected '$CURRENT' — expected '$T4A'"
fi

PREVIEW4A="$(extract_preview "$V4")"
if printf '%s' "$PREVIEW4A" | grep -q "$TOKEN_A"; then
    pass "initial preview body contains alpha's token ($TOKEN_A)"
else
    fail "initial preview body missing '$TOKEN_A'. preview was:
---
$PREVIEW4A
---"
fi

# Press 'j' — should move selection down to bravo.
"$AMUX" send --target "$V4" --literal j >/dev/null 2>&1 \
    || fail "send 'j' to viewer '$V4' failed"

if wait_for_preview_target "$V4" "$T4B"; then
    pass "after 'j', preview selects '$T4B'"
else
    CURRENT="$(preview_target "$V4" || echo '<none>')"
    fail "after 'j', preview selected '$CURRENT' — expected '$T4B'"
fi

PREVIEW4B="$(extract_preview "$V4")"
if printf '%s' "$PREVIEW4B" | grep -q "$TOKEN_B"; then
    pass "after 'j', preview body contains bravo's token ($TOKEN_B)"
else
    fail "after 'j', preview body missing '$TOKEN_B'. preview was:
---
$PREVIEW4B
---"
fi

# Guard against stale content: alpha's token must NOT still be in the pane
# (the header already shifted, but we want to catch render bugs that leave
# the previous session's scrollback on screen).
if printf '%s' "$PREVIEW4B" | grep -q "$TOKEN_A"; then
    fail "after 'j', preview still shows alpha's token ($TOKEN_A) — preview didn't re-render"
else
    pass "after 'j', preview no longer shows alpha's token"
fi

# Press 'k' — should move selection back to alpha.
"$AMUX" send --target "$V4" --literal k >/dev/null 2>&1 \
    || fail "send 'k' to viewer '$V4' failed"

if wait_for_preview_target "$V4" "$T4A"; then
    pass "after 'k', preview selects '$T4A' again"
else
    CURRENT="$(preview_target "$V4" || echo '<none>')"
    fail "after 'k', preview selected '$CURRENT' — expected '$T4A'"
fi

PREVIEW4C="$(extract_preview "$V4")"
if printf '%s' "$PREVIEW4C" | grep -q "$TOKEN_A"; then
    pass "after 'k', preview body contains alpha's token ($TOKEN_A) again"
else
    fail "after 'k', preview body missing '$TOKEN_A'. preview was:
---
$PREVIEW4C
---"
fi

"$AMUX" kill --target "$V4" >/dev/null 2>&1 || true
"$AMUX" kill --target "$T4A" >/dev/null 2>&1 || true
"$AMUX" kill --target "$T4B" >/dev/null 2>&1 || true
sleep 0.4

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo
if [ "$FAIL" -eq 0 ]; then
    printf '\033[32m=== All top-preview e2e tests passed ===\033[0m\n'
    exit 0
else
    printf '\033[31m=== %d top-preview e2e test(s) failed ===\033[0m\n' "$FAIL"
    exit 1
fi
