#!/usr/bin/env bash
#
# 0.69.0's sabotage pass: break one thing, run the test that claims to catch it,
# and require that it fails.
#
# A test nobody has tried to break is a test that may assert nothing. Three
# releases running have had an arm expose a criterion whose fixture proved
# nothing — 0.67.0's F3 asserted an absence the fixture made unreachable, 0.68.0's
# F5 arm survived against the fan-out test because the rule it aimed at was
# over-determined end to end, and this release moved F6 into a function of its own
# so the arm below hits the rule rather than a restatement of it. The arms are the
# release's evidence that its own tests discriminate, not a formality run at the
# end.
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

    # `--lib` is a target selector rather than a test binary's name, so it cannot
    # be spelled `--test --lib`. Some rules are only assertable in the crate's own
    # `#[cfg(test)] mod tests` — see the F5 arm — so the runner has to reach it.
    if [ "$target" = "--lib" ]; then
        cargo test --no-fail-fast --lib "$test_name" >"$log" 2>&1
    else
        cargo test --no-fail-fast --test "$target" "$test_name" >"$log" 2>&1
    fi
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

# ---- F1: a fold sent mid-turn lands at the next step boundary.
# The drained request is thrown away at the one site that reads it. Every other
# part of folding is intact — the threshold, `fold_now`, the summariser — so only
# a test that asserts the OPERATOR caused a fold can notice.
arm "F1 the drained fold is dropped" \
    src/run/step.rs \
    's/\*fold_asked = true;/let _ = \&steered.fold;/' \
    session_steering a_fold_asked_for_mid_turn_lands_at_the_next_boundary

# ---- F2: the same turn with nothing sent does not fold.
# Every drain asks for a fold. F2 is the control that says the fold in F1 came
# from the operator rather than from a fixture that folds on its own.
arm "F2 every boundary asks for a fold" \
    src/run/step.rs \
    's/if steered\.fold \{/if true {/' \
    session_steering without_a_send_the_same_turn_does_not_fold

# ---- F3: an off setting stays off.
# The `enabled()` gate is skipped whenever a fold is forced, which is the exact
# "an explicit request beats an explicit off" reading the criterion refuses.
arm "F3 forced folds bypass an off setting" \
    src/run/memory.rs \
    's/    if !folding\.enabled\(\) \{\n        return Ok\(0\);\n    \}/    if !folding.enabled() \&\& !forced {\n        return Ok(0);\n    }/' \
    session_steering a_fold_asked_for_does_not_override_an_off_setting

# ---- F4: one send, one fold.
# Read instead of taken, so the flag stays true and every step after the first
# send folds. Only a turn long enough for a second fold can see it.
arm "F4 the request is read, not consumed" \
    src/run/memory.rs \
    's/let asked_now = depth == 0 && std::mem::take\(asked\);/let asked_now = depth == 0 \&\& *asked;/' \
    session_steering each_send_folds_once_and_not_every_step_after_it

# ---- F5: an interrupt beside a fold ends the turn and buys no summary.
# The interrupt stops winning when a fold is in the same drain — the plausible
# bug, not an absurd one: an operator who asked for a summary could be read as
# wanting the turn to go on. The turn then runs to its bound and folds.
arm "F5 a fold overrides the interrupt" \
    src/run/step.rs \
    's/if steered\.interrupted \{/if steered.interrupted \&\& !steered.fold {/' \
    session_steering an_interrupt_beside_a_fold_stops_the_turn_and_buys_no_summary

# ---- F6: a child has no inbox to hear the operator's fold in.
#
# Aimed at the unit assertion over `extras_for`, which is why that function exists
# (0.69.0). 0.68.0's equivalent arm SURVIVED against the fan-out test because a
# child's contract is fresh *and* its extras are empty — two locks on one door, so
# removing either leaves the end-to-end test green. The rule is asserted where it
# lives, and this arm hands a child the root's inbox.
arm "F6 a child is handed the root's extras" \
    src/run.rs \
    's/        _ => &NO_EXTRAS,/        _ => turn.unwrap_or(\&NO_EXTRAS),/' \
    --lib a_child_has_no_inbox_to_hear_the_operators_fold_in

# ---- F7: the drained state is complete.
# The fold is dropped on its way out of the inbox, which is precisely what a
# `pending()` that kept returning a 2-tuple would have done silently.
arm "F7 the drain forgets the fold" \
    src/session.rs \
    's/Steered::Fold => out\.fold = true,/Steered::Fold => {}/' \
    session_steering a_fold_nobody_read_is_still_in_the_inbox_and_a_late_one_is_an_error

# ---- F8: the turn after a fold is seeded with the summary.
# The walk stops finding folds at all, which is 0.68.0's behaviour: every fold is
# undone at the next turn's first step.
arm "F8 the seed never looks for a fold" \
    src/session.rs \
    's/            if rows\.is_empty\(\) \{/            if true {/' \
    compaction the_turn_after_a_fold_is_seeded_with_the_summary_and_what_the_fold_left

# ---- F9: a session that never folded is seeded exactly as it was.
# A session with no folds is seeded with an empty paragraph in front of a
# conversation nothing replaced. Only the control can see it.
arm "F9 a paragraph is seeded whether or not one exists" \
    src/session.rs \
    's/        let Some\(\(consumed, text\)\) = self\.carried_fold\(store, &history, &starts\)\? else \{\n            return Ok\(out\);\n        \};/        let (consumed, text) = self.carried_fold(store, \&history, \&starts)?.unwrap_or((0, String::new()));/' \
    compaction a_session_that_never_folded_is_seeded_exactly_as_it_was_before

# ---- F10: the transcript holds it all and a reopened session seeds the same.
# The summary replaces every entry before the folding turn rather than the ones
# the fold consumed, which throws away the tail `keep_recent` kept whole.
arm "F10 the summary replaces every earlier turn" \
    src/session.rs \
    's/            let reached = consumed\.saturating_add\(raw\)\.min\(starts\[at\]\);/            let reached = starts[at];/' \
    compaction the_transcript_still_holds_it_all_and_a_reopened_session_seeds_the_same

# ---- F11: a turn that folded twice seeds the right remainder.
# The off-by-one: a later fold's prefix begins with the paragraph the earlier one
# wrote, and forgetting that consumes one conversation entry too many. Invisible
# to any fixture that folds once.
arm "F11 a later fold reaches one entry too far" \
    src/session.rs \
    's/                    \(row\.folded as usize\)\.saturating_sub\(1\)/                    row.folded as usize/' \
    compaction a_turn_that_folded_twice_seeds_the_newest_paragraph_and_the_second_folds_remainder

# ---- F12: a fold in a later turn stands in for what the first one replaced.
# The two index spaces are counted as one — `folded` measures the folding turn's
# own ledger, whose first entry may be the paragraph an earlier turn left behind.
# Forgetting to carry the earlier fold's reach is the defect a review found in
# this release's first implementation: everything the first fold replaced comes
# back beside a paragraph claiming to stand in for it.
arm "F12 an inherited fold's reach is not carried forward" \
    src/session.rs \
    's/            let reached = consumed\.saturating_add\(raw\)\.min\(starts\[at\]\);/            let reached = raw.min(starts[at]);/' \
    compaction a_fold_in_a_later_turn_stands_in_for_what_the_first_one_replaced

echo
echo "arms killed: $pass   survived: $survived   broken: $broken"
git status --short -- src tests
[ "$survived" -eq 0 ] && [ "$broken" -eq 0 ]
