#!/usr/bin/env bash
#
# 0.70.0's sabotage pass: break one thing, run the test that claims to catch it,
# and require that it fails.
#
# A test nobody has tried to break is a test that may assert nothing. Four
# releases running have had an arm expose a criterion whose fixture proved
# nothing — 0.67.0's F3 asserted an absence the fixture made unreachable, 0.68.0's
# F5 arm survived against the fan-out test because the rule it aimed at was
# over-determined end to end, and 0.69.0 moved F6 into a function of its own so
# the arm hit the rule rather than a restatement of it. The arms are the
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

# ---- F1: a disabled server contributes nothing and is still visible.
# The skip moves out of the connect loop, which is exactly the cheap version the
# design rejected: the server is spawned and its tools reach the roster, so it is
# switched off in name only. The listing half still passes, which is why the arm
# has to reach the roster half to prove anything.
arm "F1 the disabled server is started anyway" \
    src/mcp.rs \
    's/            if !server\.enabled \{/            if false {/' \
    mcp a_disabled_server_contributes_no_tools_and_is_still_configured

# ---- F2: a disabled bundle contributes to none of the six.
# Everything is routed to `loaded` regardless of the flag — the bundle contributes
# all six and is missing from `disabled()`. A bundle half-off is worse than one
# fully on, so the test names the subsystem that leaked.
arm "F2 the flag is read and then ignored" \
    src/plugin.rs \
    's/                    if decl\.enabled \{/                    if true {/' \
    plugin a_disabled_bundle_contributes_none_of_the_six_and_stays_visible

# ---- F3a: every `[[mcp]]` file written before this release means what it meant.
# The serde default flips, so a file that never mentioned `enabled` silently
# switches every server off. Asserted separately from F1 rather than trusted to
# `#[serde(default)]`, because this is the failure that would reach every existing
# operator at once.
arm "F3a an absent mcp enabled key defaults to off" \
    src/mcp.rs \
    's/fn default_enabled\(\) -> bool \{\n    true\n\}/fn default_enabled() -> bool {\n    false\n}/' \
    mcp a_server_declared_without_the_enabled_key_offers_the_same_roster

# ---- F3b: the same promise for `[[plugin]]`, which has its own default.
# Two defaults, two arms. One shared helper would have been one arm; they are in
# different modules on purpose and so the guarantee needs proving twice.
arm "F3b an absent plugin enabled key defaults to off" \
    src/plugin.rs \
    's/fn default_enabled\(\) -> bool \{\n    true\n\}/fn default_enabled() -> bool {\n    false\n}/' \
    plugin an_absent_enabled_key_is_indistinguishable_from_switched_on

# ---- F4: the near-miss check catches the typo without closing the exemption.
# Every unknown key becomes a near miss, which is what "just close the exemption"
# amounts to in practice. The typo is still caught — so only the second half of
# F4, the unrelated key that must still be accepted, can see this. That half is
# the trade the exemption exists to make.
arm "F4 the near-miss check rejects every unknown key" \
    src/mcp.rs \
    's/fn near_miss\(key: &str\) -> bool \{/fn near_miss(key: \&str) -> bool {\n    if true {\n        return true;\n    }/' \
    config an_unrelated_unknown_key_in_an_mcp_table_is_still_accepted

# ---- F5: the probe shuts the server down again.
# `std::mem::forget`, NOT a removal of the `cancel()` call. The first version of
# this arm replaced `cancel()` with a no-op and SURVIVED, and the survival was
# right: rmcp's `ChildWithCleanup` kills the child on `Drop`, so deleting the
# explicit shutdown swaps one correct mechanism for another and breaks nothing.
# Leaking the handle is what actually leaves the server running, and it is the
# only mutation this criterion's property can see. Recorded rather than quietly
# re-aimed: the explicit `cancel()` is belt-and-braces for the stdio case and is
# load-bearing only for a transport with no child process to reap.
arm "F5 the probe leaks the server handle" \
    src/mcp.rs \
    's/    let _ = service\.cancel\(\)\.await;/    std::mem::forget(service);/' \
    mcp a_probe_leaves_no_child_process_behind

# ---- F6: the sweep preview equals the sweep.
# The victim set is resolved from the first turn's timestamp instead of the
# session's. Note what this does NOT break: preview and sweep share the selection,
# so they still agree with each other and an equality-only test passes. It is
# caught by the session created before the cutoff whose first turn is after it —
# the ordinary case the issue names, and the reason the fixture has one.
arm "F6 the victim set comes from the first turn, not the session" \
    src/state/sessions.rs \
    's/"SELECT id FROM sessions WHERE created_at < \?1 ORDER BY id"/"SELECT DISTINCT session_id FROM session_turns WHERE created_at < ?1 ORDER BY session_id"/' \
    retention a_sweep_preview_is_exactly_the_receipt_the_sweep_then_produces

# ---- F7: a worktree child's recorded root reads back.
# The reader walks to the parent's row, so it answers with the tree's root rather
# than the child's own worktree — the precise reconstruction the issue says an
# operator is forced into today, now wearing the reader's name. The test compares
# against the filesystem, so it fails; a test comparing against a recomputed path
# would not.
arm "F7 the reader answers with the parent's root" \
    src/state/runs.rs \
    's/"SELECT file FROM runs WHERE id = \?1"/"SELECT file FROM runs WHERE id = (SELECT COALESCE(parent_run_id, id) FROM runs WHERE id = ?1)"/' \
    worktree a_worktree_childs_run_row_names_the_directory_its_files_are_in

# ---- F9: a provider's Debug never prints the credential.
# The hand-written impl gains the field it exists to omit. `#[derive(Debug)]`
# would have been the more obvious mutation and is not usable: it collides with
# the hand-written impl and fails to compile, which is not a kill.
arm "F9 the hand-written Debug prints the key after all" \
    src/provider/openrouter.rs \
    's/            \.field\("model", &self\.model\)\n            \.finish_non_exhaustive\(\)/            .field("model", \&self.model)\n            .field("api_key", \&self.api_key)\n            .finish_non_exhaustive()/' \
    verify openrouter_debug_hides_the_key

# ---- F8: an asking posture is asked on exec.
# The exec target is removed from the gated set, so the program never reaches an
# approver and `Git::run`'s own ungated check refuses it exactly as it did before
# this release — the whole defect, restored in one line.
#
# The first version of this arm flipped `Git::run`'s `Ask` arm to `false` and
# SURVIVED, correctly: the deny-posture test is killed by the `Deny` arm, which
# that mutation left intact, so it never touched the path the test exercises.
# The arm has to remove the GATING, not weaken the backstop underneath it.
arm "F8 the exec target is never gated" \
    src/run/dispatch.rs \
    's/                \(Act::Exec, crate::tools::git::GIT\.to_string\(\)\),\n//' \
    ask_is_not_deny the_default_policy_asks_about_git_and_a_deferral_pauses_the_run

# ---- F10: a step cap with no criterion is still a step cap.
# The new variant is returned whenever the cap is reached, which is the obvious
# wrong version of #212 and passes the criterion's own positive arm. Only a run
# with NO verification can tell the two apart, which is why F10 has a second arm
# and why that arm is the one that matters.
arm "F10 the cap always reports a verification failure" \
    src/run/outcome.rs \
    's/pub\(super\) fn capped_outcome\(judged_and_failed: bool, steps: u32\) -> \(&.static str, RunOutcome\) \{\n    if judged_and_failed \{/pub(super) fn capped_outcome(judged_and_failed: bool, steps: u32) -> (\&'"'"'static str, RunOutcome) {\n    if judged_and_failed || true {/' \
    verification_outcome a_run_with_no_criterion_still_reports_the_plain_step_cap

# ---- F11: the second attempt is informed by what the gate actually printed.
# The recorded output is dropped and the section carries only the framing line.
# The framing still names the phase and the step, so a test asserting that a
# section arrived would pass; the criterion asserts the gate's own recorded text
# reaches the request, which is the only thing that makes a retry informed.
arm "F11 the section is framing without the output" \
    src/run/outcome.rs \
    's/    let mut key = phase\.unwrap_or_default\(\);\n    if let Some\(output\) = output \{/    let mut key = phase.unwrap_or_default();\n    if let Some(output) = output.filter(|_| false) {/' \
    verification_outcome the_step_after_a_gate_failure_is_told_what_the_gate_said

# ---- F11b: the same failure is carried once, not once per step.
# The dedup key becomes the framed section, which NAMES THE STEP — so two
# identical failures never compare equal and the guard is decorative. This is the
# exact mistake the first implementation made and the arm that catches it: every
# other assertion about the feedback still passes, because the content reaching
# the model is unchanged; only the count moves.
arm "F11b the dedup compares a key that includes the step" \
    src/run/outcome.rs \
    's/    let mut key = phase\.unwrap_or_default\(\);/    let mut key = format!("{step}");/' \
    verification_outcome a_repeated_gate_failure_is_carried_once

# ---- F10b: a resumed run does not un-conclude what a previous attempt judged.
# The seed goes back to `false`, which is what the first implementation had. Every
# other F10 assertion still passes — a run that reaches its cap in one go is
# unaffected — and only a resume at the SAME cap, where the loop body never runs,
# can see it. The durable row is what it corrupts, which is what makes it worth
# an arm of its own.
arm "F10b a resumed run starts having judged nothing" \
    src/run/step.rs \
    's/    let mut criterion_failed = criterion_already_failed\(store, run_id\);\n    \/\/ 0\.70\.0 — the gate-failure section/    let mut criterion_failed = false;\n    \/\/ 0.70.0 — the gate-failure section/' \
    verification_outcome resuming_at_the_same_cap_does_not_rewrite_the_verification_failure

# ---- HIGH 2: the near-miss check reaches into a profile.
# The recursion is removed, which is exactly the state the release shipped to
# review with. The top-level check still passes, so only a profile-declared
# server can see it — and a profile's `[[mcp]]` is merged over the base and
# started like any other.
arm "F4b the near-miss check does not look inside a profile" \
    src/mcp.rs \
    's/    if let Some\(profiles\) = table\.get\("profile"\)\.and_then\(toml::Value::as_table\) \{/    if let Some(profiles) = None::<\&toml::value::Table> {/' \
    config a_near_miss_inside_a_profile_is_refused_and_names_the_profile

# ---- MEDIUM 3: a switched-off bundle claims no id.
# `seen.insert` moves back above the `enabled` branch, so a disabled entry holds
# the id and the enabled twin beside it is dropped. The duplicate-id test still
# passes — two ENABLED bundles still collide — so only the swap can see it.
arm "F2b a disabled bundle reserves its id" \
    src/plugin.rs \
    's/                    if decl\.enabled \{\n                        if !seen\.insert\(plugin\.id\.clone\(\)\) \{/                    if true {\n                        if !seen.insert(plugin.id.clone()) {/' \
    plugin a_disabled_twin_claims_no_id_and_the_enabled_one_loads

# ---- LOW 6: a credential carried in the URL is not printed either.
# The redactor becomes the identity function. Every existing F9 assertion still
# passes, because those providers' default endpoints carry no credential — only
# a caller-supplied base with the key in it can see this.
arm "F9b the endpoint is printed verbatim" \
    src/provider/mod.rs \
    's/pub\(crate\) fn redacted_endpoint\(endpoint: &str\) -> String \{/pub(crate) fn redacted_endpoint(endpoint: \&str) -> String {\n    return endpoint.to_string();/' \
    verify a_credential_carried_in_the_base_url_is_not_printed_either

echo
echo "arms killed: $pass   survived: $survived   broken: $broken"
git status --short -- src tests
[ "$survived" -eq 0 ] && [ "$broken" -eq 0 ]
