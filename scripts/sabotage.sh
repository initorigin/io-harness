#!/usr/bin/env bash
#
# 0.68.0's sabotage pass: break one thing, run the test that claims to catch it,
# and require that it fails.
#
# A test nobody has tried to break is a test that may assert nothing. Two
# releases running have had an arm expose a criterion whose fixture proved
# nothing — 0.67.0's F3 asserted an absence the fixture made unreachable, and this
# release's F5 control failed twice before it measured anything — so the arms
# below are the release's evidence that its own tests discriminate, not a
# formality run at the end.
#
# Three rules this runner enforces because each has cost a release something:
#
#   * It refuses to start against a dirty `src/` or `tests/`. Every arm reverts
#     with `git checkout --`, which destroys uncommitted work. 0.66.0 lost work
#     to a hand-run arm exactly this way.
#   * It counts the `running N tests` line. A zero-test run — a renamed test, a
#     filter that matches nothing — exits non-zero and reads *identically* to a
#     killed test unless the count is checked.
#   * Every arm runs with `--no-fail-fast`. `cargo test` stops at the first
#     failing binary, which makes "only N tests failed" meaningless as a set.
#
# An arm that kills nothing is a FINDING, not a failure of the runner. It is
# reported as SURVIVED and the release has to say why.

set -uo pipefail

cd "$(dirname "$0")/.."

export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
: "${XDG_CONFIG_HOME:?set XDG_CONFIG_HOME to a scratch dir; tests/plugin.rs reads the real one}"

if ! git diff --quiet -- src tests || ! git diff --cached --quiet -- src tests; then
    echo "refusing to run: src/ or tests/ is dirty, and every arm reverts with git checkout --"
    git status --short -- src tests
    exit 2
fi

pass=0
survived=0
broken=0

# One arm: a description, the file to mutate, a perl expression that mutates it,
# the test target, and the test name that must fail.
arm() {
    local what="$1" file="$2" mutation="$3" target="$4" test_name="$5"
    local log
    log="$(mktemp)"

    perl -0pi -e "$mutation" "$file"
    if git diff --quiet -- "$file"; then
        echo "BROKEN  $what — the mutation changed nothing, so this arm tested nothing"
        broken=$((broken + 1))
        rm -f "$log"
        return
    fi

    cargo test --no-fail-fast --test "$target" "$test_name" >"$log" 2>&1
    local status=$?
    # `running N tests` — summed, because a filter may match in more than one
    # binary and a zero total is a filter that matched nothing.
    local ran
    ran=$(grep -oE '^running [0-9]+ tests?' "$log" | grep -oE '[0-9]+' | paste -sd+ - | bc 2>/dev/null || echo 0)
    ran=${ran:-0}

    git checkout -- "$file"

    if [ "$ran" -eq 0 ]; then
        echo "BROKEN  $what — the arm ran 0 tests, which is not a kill however it exited"
        broken=$((broken + 1))
    elif [ "$status" -ne 0 ]; then
        echo "killed  $what ($ran test(s) ran)"
        pass=$((pass + 1))
    else
        echo "SURVIVED $what — $ran test(s) ran and passed with the code broken"
        survived=$((survived + 1))
    fi
    rm -f "$log"
}

# ---- F1: a requested fold lands before the turn's first request.
# The request stops being read at all. Everything else about folding is intact,
# so only a test that asserts the REQUEST caused a fold can notice.
arm "F1 the request is never read" \
    src/run/memory.rs \
    's/let asked_now = depth == 0 && std::mem::take\(asked\);/let asked_now = { let _ = asked; false };/' \
    compaction a_requested_fold_lands_before_the_turns_first_request

# ---- F2: a turn without the flag does not fold.
# Every turn is forced. F2 is the control that says this release changed nothing
# for a caller who never asks, and it is the only test that can see this.
arm "F2 every turn is forced" \
    src/run/memory.rs \
    's/let asked_now = depth == 0 && std::mem::take\(asked\);/let asked_now = { let _ = asked; true };/' \
    compaction without_the_flag_the_same_turn_does_not_fold

# ---- F3: an off setting stays off.
# The `enabled()` gate is skipped whenever a fold is forced, which is the exact
# "an explicit request beats an explicit off" reading the criterion refuses.
arm "F3 forced folds bypass an off setting" \
    src/run/memory.rs \
    's/    if !folding\.enabled\(\) \{\n        return Ok\(0\);\n    \}/    if !folding.enabled() \&\& !forced {\n        return Ok(0);\n    }/' \
    compaction a_requested_fold_does_not_override_an_off_setting

# ---- F4: the request is honoured once, not every step.
# Read instead of taken, so the flag stays true and every step folds. This is the
# bug a bool-on-a-contract invites, and only a multi-step test sees it.
arm "F4 the request is read, not consumed" \
    src/run/memory.rs \
    's/let asked_now = depth == 0 && std::mem::take\(asked\);/let asked_now = depth == 0 \&\& *asked;/' \
    compaction the_request_is_consumed_and_does_not_fold_every_step

# ---- F5: a spawned child does not fold on the root's request.
# The depth gate is removed. NOTE: this arm is expected to survive today, because
# `spawn_child` builds each child a fresh contract, so `fold_now` is already
# false at depth 1 before the gate is consulted. It is run anyway, and its
# survival is reported rather than hidden — the gate is the lock that decides the
# question the day a child inherits its parent's contract.
arm "F5 the depth gate is removed" \
    src/run/memory.rs \
    's/let asked_now = depth == 0 && std::mem::take\(asked\);/let asked_now = { let _ = depth; std::mem::take(asked) };/' \
    session_fanout a_spawned_child_does_not_fold_on_the_roots_request

# ---- F6 and F7: the seed is durable before the first step.
# The seed goes back above the watermark, which is 0.67.0's behaviour: no fold
# can reach the conversation at the first step, on any trigger.
arm "F6/F7 the seed is not persisted before the loop" \
    src/run/step.rs \
    's/    written = persist_ledger\(store, run_id, &ledger, written\)\?;\n    \/\/ 0\.68\.0 — the caller.s standing request/    \/\/ 0.68.0 — the caller'"'"'s standing request/' \
    compaction a_seeded_turn_that_overflows_folds_and_recovers

# ---- F8: a connected MCP server announces its tool count.
# The count is dropped at the one site that has it, which is indistinguishable
# from 0.67.0 unless a test reads the connect event's `tools`.
arm "F8 the connect event drops the count" \
    src/mcp.rs \
    's/Some\(tools\.len\(\) as u32\)/None/' \
    mcp a_connected_server_announces_the_number_of_tools_it_offered

# ---- F9: an event with no count serialises as it did before.
# `skip_serializing_if` goes, so every MCP event gains `"tools":null` — a visible
# change to a stream existing consumers already parse.
arm "F9 the null is written back onto every MCP event" \
    src/observe.rs \
    's/#\[serde\(default, skip_serializing_if = "Option::is_none"\)\]\n        tools:/#[serde(default)]\n        tools:/' \
    observe an_event_with_no_count_serialises_exactly_as_it_did_before

echo
echo "arms killed: $pass   survived: $survived   broken: $broken"
git status --short -- src tests
[ "$survived" -eq 0 ] && [ "$broken" -eq 0 ]
