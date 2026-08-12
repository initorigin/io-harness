//! The project's own checker, run after an edit, so the model reads the error it
//! just wrote.
//!
//! Until 0.25.0 the only way an agent learned that its edit did not compile was to
//! decide to find out: it had to choose to call `exec`, wait for a build, and read
//! the log — a whole step, and a step it frequently does not take, because a model
//! that has just written a plausible-looking function has no signal telling it to
//! doubt one. So a type error introduced at step 4 is discovered at step 20 by the
//! verification gate, after sixteen steps were spent building on top of it. This
//! module closes that gap: after a successful `edit_file` or `write_file` the
//! project's checker runs and its findings are appended to the observation the
//! model was going to read anyway. The error arrives in the same turn as the edit
//! that caused it, and costs the model no decision.
//!
//! ## Deliberately not a language server
//!
//! The obvious implementation is an LSP client, and it was researched and rejected
//! for four separate reasons, any one of which is disqualifying.
//!
//! Push diagnostics (`textDocument/publishDiagnostics`) have **no completion
//! signal**. The server sends them when it feels like it, so a harness that wants
//! "the diagnostics for this file" can only wait an arbitrary interval and hope; an
//! empty result is indistinguishable from a slow one, and that is exactly the
//! distinction this feature exists to report. The 3.17 pull model
//! (`textDocument/diagnostic`) fixes that and is **not portable** — it is optional,
//! and a large share of the servers a user's project would actually be running do
//! not implement it. Where it *is* implemented, rust-analyzer's pull path answers
//! from its own analysis and **never consults flycheck**, so it omits borrow-check
//! errors, monomorphisation errors and every clippy lint: precisely the errors a
//! model writes. And a language server's **cold start is minutes** on a real
//! repository, which is not a cost that can be paid inside one tool call.
//!
//! What a language server does to produce the errors that matter is shell out to
//! the compiler and read its machine-readable output. This module reads the same
//! stream directly. That removes the process, the protocol, the handshake, the
//! indexing wait and the per-language client, and gives up nothing that was ever
//! going to be used here: this is a one-shot question asked after a write, not an
//! interactive editing session, so incremental analysis has nothing to be
//! incremental about.
//!
//! ## The check is workspace-wide, which is why it is bounded
//!
//! `cargo check`, `tsc --noEmit` and `go build ./...` compile the *project*, not the
//! file that was edited. That is not a shortcut — none of these tools can
//! meaningfully type-check one file in isolation, because the edited file's
//! correctness depends on every file that uses it, and a caller broken by a changed
//! signature is the most valuable finding of all. It does mean the cost of a check
//! is the cost of the project's build, so every path through this module is bounded
//! twice: a wall-clock timeout, and a character cap on what comes back.
//!
//! ## Never fatal
//!
//! A checker that is missing from `PATH`, times out, or fails for a reason of its
//! own produces no findings and is reported as [`Outcome::Failed`]. It cannot turn
//! a successful edit into a failed one, and no path here returns `Err`: the edit
//! already happened, and reporting it as a failure because a checker was not
//! installed would be a lie about what the harness did to the workspace.

use std::path::Path;
use std::time::Duration;

use crate::toolchain::Toolchain;

use super::exec::{head_and_tail, Exec, ExecOutcome};

/// What running the project's checker after an edit produced.
///
/// The four variants exist because the caller must be able to tell **"ran and found
/// nothing" from "never ran"**. Collapsing those two into an empty string would
/// make a clean build and an absent toolchain look identical in the observation,
/// and the model would read silence as approval in both cases — including the case
/// where nothing checked its work at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The checker ran and reported something. The payload is finished text, capped
    /// and headed, ready to append to the observation as-is.
    Found(String),
    /// The checker ran and reported nothing. The edit compiles, as far as this
    /// project's own checker is concerned.
    Clean,
    /// No checker ran, and none was going to: the root has no marker file, or its
    /// ecosystem has no check command cheap enough to run after every edit. Not a
    /// problem, and not worth telling the model about.
    Skipped(String),
    /// A checker was chosen and could not produce an answer — not installed, killed
    /// by the timeout, or exited non-zero while saying nothing. The reason is short
    /// and is meant for the observation, so the model knows its edit is unverified
    /// rather than verified clean.
    Failed(String),
}

/// How a checker's output turns into text the model reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// `cargo check --message-format=json`: newline-delimited JSON, one record per
    /// line, from which the pre-rendered terminal text is extracted.
    CargoJson,
    /// The checker's own console output is already the rendered diagnostic, so it
    /// is passed through unchanged.
    ///
    /// This is a deliberate refusal to write four more parsers. `tsc`, `go build`
    /// and `pyright` all have a structured mode, and using it would mean inventing
    /// and maintaining a parser per ecosystem in order to reassemble text that the
    /// tool already formatted better than we would. The rendered form is what a
    /// developer reads and what the model has seen a million examples of; there is
    /// nothing this module wants to *do* with a diagnostic except show it. Parse
    /// only where the raw stream is unreadable — which is cargo's, and only cargo's,
    /// because `--message-format=json` is the only way to get cargo's diagnostics
    /// without also getting its progress bars.
    Rendered,
}

/// The check command per ecosystem, keyed by [`Toolchain::ecosystem`].
///
/// A table rather than a `match` with a case per language, because every entry is
/// the same three facts and the interesting content is which ecosystems are absent.
/// The commands here are the *type-check* command and not the build command: they
/// must be cheap enough to run after every single edit, which is a much stricter
/// bar than [`Toolchain::build`] is held to. That is why `cargo check` appears here
/// rather than `cargo build`, and why maven, gradle, dotnet, swift, cmake, elixir,
/// ruby, php and make appear nowhere: their type-checking step *is* their build,
/// and running it after every edit would make editing unusable. An ecosystem with
/// no entry is [`Outcome::Skipped`] — an absent cheap checker is a fact about the
/// ecosystem, not a failure of this one.
const CHECKERS: &[(&str, &[&str], Format)] = &[
    (
        "cargo",
        &["cargo", "check", "--message-format=json"],
        Format::CargoJson,
    ),
    // `tsc` and not `npx tsc`: `npx` will reach the network and install a package
    // to satisfy an invocation, which is not something an edit should be able to
    // trigger. A project with TypeScript installed has `tsc` reachable; one that
    // does not gets a `Failed` saying so, which is the honest answer.
    ("node", &["tsc", "--noEmit"], Format::Rendered),
    ("deno", &["deno", "check", "."], Format::Rendered),
    ("go", &["go", "build", "./..."], Format::Rendered),
    // No `--outputjson`. Pyright's JSON has to be walked and reassembled into the
    // `file:line:col: message` line that its text mode already prints, so the JSON
    // buys a parser and loses the formatting. See [`Format::Rendered`].
    ("python", &["pyright"], Format::Rendered),
];

/// Run the project's own checker over `root` and turn its output into something to
/// append to the observation for an edit that just succeeded.
///
/// `tc` is the detection the run already performed — passed in rather than
/// re-derived, because [`crate::toolchain::detect`] reads the directory and an edit
/// is a hot path. `None` means the root has no marker file at all and is a
/// [`Outcome::Skipped`], exactly as `detect` returning `None` means "nothing to go
/// on" rather than "guess".
///
/// `timeout` is a wall-clock ceiling on the whole check and `cap` bounds the text
/// that comes back, head-and-tail, at the run's per-observation ceiling. Both are
/// the caller's, so a check obeys the same limits as every other tool result rather
/// than limits of its own.
///
/// **This never returns `Err` and never fails an edit.** Every way a checker can
/// disappoint — absent, wedged, broken — arrives as [`Outcome::Failed`] carrying a
/// sentence, because the write already happened and the model needs to know only
/// whether it was checked.
pub(crate) async fn after_edit(
    root: &Path,
    tc: Option<&Toolchain>,
    timeout: Duration,
    cap: usize,
) -> Outcome {
    match checker(tc) {
        Ok(checker) => checker.run(root, timeout, cap).await,
        Err(why) => Outcome::Skipped(why),
    }
}

/// The checker this project would run, resolved without running it (0.51.0).
///
/// Split out of [`after_edit`] because the `check` tool has to know **what** it
/// is about to spawn before it spawns it: a model-callable path to the project's
/// build command must be an [`Act::Exec`](crate::Act::Exec) check on that
/// command, and a policy cannot be asked about an argv nobody has resolved yet.
/// The automatic post-edit path needs no such split — it is the crate's own
/// reflex after a write the policy already allowed, and it stays ungated.
///
/// `Err` carries the reason there is no checker, which is a [`Outcome::Skipped`]
/// to `after_edit` and an observation to the tool.
pub(crate) fn checker(tc: Option<&Toolchain>) -> std::result::Result<Checker, String> {
    let Some(tc) = tc else {
        return Err("no project marker in the workspace root".into());
    };
    let Some((_, argv, format)) = CHECKERS.iter().find(|(eco, _, _)| *eco == tc.ecosystem) else {
        return Err(format!(
            "no check command cheap enough to run after every edit for a {} project",
            tc.ecosystem
        ));
    };
    Ok(Checker {
        argv: argv.iter().map(|s| (*s).to_string()).collect(),
        format: *format,
    })
}

/// A resolved checker: the argv a policy can be asked about, and how to read
/// what it prints (0.51.0).
pub(crate) struct Checker {
    /// The command, program first. Public to the crate because the gate needs it.
    pub(crate) argv: Vec<String>,
    format: Format,
}

impl Checker {
    /// Run it. Never `Err`, for the reason [`after_edit`] never is.
    pub(crate) async fn run(&self, root: &Path, timeout: Duration, cap: usize) -> Outcome {
        run(root, &self.argv, self.format, timeout, cap).await
    }
}

/// Spawn one checker and classify what it produced.
///
/// Separate from [`after_edit`] so the table lookup and the running are testable
/// apart: a test can drive this with a program it chose, which is the only way to
/// exercise the missing-from-`PATH` and cap paths without depending on which
/// toolchains happen to be installed on the machine running the tests.
async fn run(
    root: &Path,
    argv: &[String],
    format: Format,
    timeout: Duration,
    cap: usize,
) -> Outcome {
    // `usize::MAX` and not `cap`: [`Exec`] would otherwise cut the *raw* stream, and
    // for `CargoJson` that means cutting NDJSON in half — the surviving text would be
    // an unparseable fragment measured in JSON bytes rather than in diagnostics. It
    // costs nothing to defer, because `Exec` reads the child to completion into
    // memory either way; the cap is applied below, to the rendered findings, which
    // is the text whose size actually matters to the prompt.
    //
    // `Exec` also owns the wall-clock ceiling — it is a `tokio::time::timeout` around
    // a `kill_on_drop` child — so this module does not wrap a second one around it.
    // Two timeouts over one child is two answers to the question of what killed it.
    let outcome = Exec::new(root, timeout, usize::MAX).run(argv).await;

    let (code, stdout, stderr) = match outcome {
        Ok(ExecOutcome::Ran {
            code,
            stdout,
            stderr,
            ..
        }) => (code, stdout, stderr),
        Ok(ExecOutcome::TimedOut { after }) => {
            return Outcome::Failed(format!(
                "`{}` did not finish within {}s, so this edit is unchecked",
                argv.join(" "),
                after.as_secs()
            ));
        }
        // Unreachable today: diagnostics build their own uncontained `Exec`, so no
        // backend is selected and no cap can be reported. Handled rather than
        // `unreachable!()` so that pointing this path at a sandbox later is a
        // compile-time decision instead of a panic in someone's run.
        Ok(ExecOutcome::Capped { cap, .. }) => {
            return Outcome::Failed(format!(
                "`{}` was killed by the {} cap, so this edit is unchecked",
                argv.join(" "),
                cap.as_str()
            ));
        }
        Ok(ExecOutcome::Unavailable { reason }) => {
            return Outcome::Failed(format!(
                "this edit is unchecked: {reason}, so `{}` could not run",
                argv.join(" ")
            ));
        }
        // An I/O error starting the child. It is the caller's edit that must survive
        // this, not this module's dignity, so it is reported and not propagated.
        Err(e) => {
            return Outcome::Failed(format!(
                "this edit is unchecked: `{}` could not be started ({e})",
                argv.join(" ")
            ));
        }
    };

    let findings = match format {
        Format::CargoJson => cargo_rendered(&stdout),
        // stderr first, because the compilers that write diagnostics there
        // (`go build`, `rustc`) write nothing else there, while the ones that write
        // to stdout (`tsc`, `pyright`) leave stderr empty. Checking both in this
        // order needs no per-tool knowledge of which stream a given checker chose.
        Format::Rendered => {
            let s = if stderr.trim().is_empty() {
                &stdout
            } else {
                &stderr
            };
            s.trim().to_string()
        }
    };

    if findings.trim().is_empty() {
        // Nothing to report, and the two reasons for that are not the same thing. A
        // checker that exited cleanly checked the workspace and found it sound. A
        // checker that exited non-zero and said nothing failed at its own job —
        // an unresolvable dependency, a missing config file, a broken install — and
        // calling that "clean" would tell the model its edit was verified when
        // nothing verified it.
        return match code {
            Some(0) => Outcome::Clean,
            other => Outcome::Failed(format!(
                "this edit is unchecked: `{}` exited with {} and reported nothing",
                argv.join(" "),
                other.map_or_else(|| "a signal".to_string(), |c| c.to_string())
            )),
        };
    }

    let (text, _elided) = head_and_tail(&findings, cap);
    Outcome::Found(format!(
        "Diagnostics from `{}`, run over the whole workspace after this edit. They are \
         the project's own checker talking, and they may name files this edit did not \
         touch:\n{text}",
        argv.join(" ")
    ))
}

/// Pull the rendered diagnostics out of `cargo check --message-format=json`.
///
/// The stream is one JSON object per line. Records carry a `reason`, and only
/// `compiler-message` ones hold a diagnostic; `compiler-artifact`, `build-script-executed`
/// and `build-finished` are cargo narrating its own progress and are dropped.
/// What is taken from a `compiler-message` is `message.rendered` — the exact block
/// of text rustc would have printed to a terminal, arrows, notes, colours stripped
/// and all — rather than the structured `spans` beside it, because reassembling
/// that structure into text is work whose only possible output is the string sitting
/// next to it.
///
/// Lines that do not begin with `{` are skipped rather than treated as an error.
/// Cargo's own documentation warns that this stream is not exclusively JSON: a build
/// script's stdout and a procedural macro's `println!` land in it verbatim, and a
/// project with one of those would otherwise have every check fail on someone else's
/// debug output.
fn cargo_rendered(stdout: &str) -> String {
    let mut out = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        // A line that parses as no JSON at all is skipped for the same reason: it is
        // someone else's output that happened to start with a brace, or a fragment of
        // a stream that was cut.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(rendered) = v
            .pointer("/message/rendered")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        out.push_str(rendered.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 100_000;

    /// A detection with only the field this module reads filled in. The rest of
    /// [`Toolchain`] is the model's business, not this module's — nothing here ever
    /// runs `test`, `lint` or `build`.
    fn toolchain(ecosystem: &str) -> Toolchain {
        Toolchain {
            ecosystem: ecosystem.to_string(),
            marker: "marker".into(),
            manager: ecosystem.to_string(),
            install: vec![],
            build: vec![],
            test: vec![],
            lint: vec![],
            format: vec![],
            run: vec![],
        }
    }

    /// The timeout is zero, which is what makes this test about spawning rather than
    /// about the return value: anything that reached [`Exec`] would be killed
    /// immediately and come back as `Failed`, so a `Skipped` here proves no child was
    /// ever started.
    #[tokio::test]
    async fn an_unknown_ecosystem_is_a_skip_and_spawns_nothing() {
        let dir = tempfile::tempdir().unwrap();

        let none = after_edit(dir.path(), None, Duration::ZERO, CAP).await;
        assert!(
            matches!(none, Outcome::Skipped(_)),
            "a root with no marker has nothing to check: {none:?}"
        );

        for eco in ["make", "maven", "gradle", "dotnet", "swift", "ruby"] {
            let out = after_edit(dir.path(), Some(&toolchain(eco)), Duration::ZERO, CAP).await;
            assert!(
                matches!(out, Outcome::Skipped(_)),
                "{eco} has no cheap checker, which is a skip and not a failure: {out:?}"
            );
        }
    }

    /// F8/NF4: a checker that is not installed must not fail the edit, must not be an
    /// `Err`, and must not look like a clean check.
    #[tokio::test]
    async fn a_checker_missing_from_path_is_a_failure_not_an_error_and_not_a_clean_check() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["io-harness-no-such-checker".to_string()];
        let out = run(
            dir.path(),
            &argv,
            Format::Rendered,
            Duration::from_secs(30),
            CAP,
        )
        .await;

        let Outcome::Failed(reason) = &out else {
            panic!("a missing checker is a Failed outcome: {out:?}");
        };
        assert!(reason.contains("io-harness-no-such-checker"), "{reason}");
        assert_ne!(
            out,
            Outcome::Clean,
            "F9: unchecked is not the same as clean"
        );
    }

    /// The other half of F9, and the reason [`Outcome`] has four variants: a checker
    /// that ran and found nothing is a different answer from one that never ran.
    /// `rustc` is the one program guaranteed present wherever `cargo test` runs.
    #[tokio::test]
    async fn a_checker_that_finds_nothing_is_clean_and_not_a_skip() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec!["rustc".to_string(), "--version".to_string()];
        let out = run(
            dir.path(),
            &argv,
            Format::CargoJson,
            Duration::from_secs(60),
            CAP,
        )
        .await;
        assert_eq!(
            out,
            Outcome::Clean,
            "exit 0 with no diagnostics is a clean check"
        );
    }

    /// Fixture text rather than a real `cargo check`: a unit test must not compile a
    /// crate, and every property being asserted is a property of the parser.
    #[test]
    fn the_parser_takes_rendered_from_compiler_messages_and_ignores_everything_else() {
        let stream = concat!(
            // A build script's stdout, which cargo's own docs warn will appear here.
            "cargo:rerun-if-changed=build.rs\n",
            "warning: something a proc macro printed\n",
            // Not a diagnostic.
            r#"{"reason":"compiler-artifact","target":{"name":"io-harness"},"fresh":false}"#,
            "\n",
            r#"{"reason":"build-script-executed","package_id":"x 0.1.0"}"#,
            "\n",
            // The two that count.
            r#"{"reason":"compiler-message","message":{"level":"error","rendered":"error[E0308]: mismatched types\n --> src/x.rs:3:5\n","spans":[]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"warning","rendered":"warning: unused variable: `n`\n"}}"#,
            "\n",
            // A `compiler-message` with no rendered text, and a truncated line.
            r#"{"reason":"compiler-message","message":{"level":"error"}}"#,
            "\n",
            r#"{"reason":"compiler-mess"#,
            "\n",
            r#"{"reason":"build-finished","success":false}"#,
            "\n",
        );

        let out = cargo_rendered(stream);
        assert!(out.contains("error[E0308]: mismatched types"), "{out}");
        assert!(out.contains("--> src/x.rs:3:5"), "{out}");
        assert!(out.contains("warning: unused variable: `n`"), "{out}");

        assert!(!out.contains("compiler-artifact"), "{out}");
        assert!(!out.contains("rerun-if-changed"), "{out}");
        assert!(!out.contains("proc macro"), "{out}");
        assert!(!out.contains("build-finished"), "{out}");
        // Nothing structural leaked: the rendered text is taken, never the record.
        assert!(!out.contains("\"reason\""), "{out}");
        assert_eq!(
            out.lines().count(),
            3,
            "two lines of the E0308 block and one of the warning, and nothing else: {out}"
        );
    }

    #[test]
    fn a_stream_with_no_diagnostics_in_it_renders_to_nothing() {
        let clean = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"x"}}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
            "\n",
        );
        assert_eq!(cargo_rendered(clean), "");
    }

    /// The cap is the run's per-observation ceiling, and a build log is exactly the
    /// output that would blow through it. `rustc` on a file that does not exist is a
    /// real checker failing in a real way, and compiles nothing.
    #[tokio::test]
    async fn the_findings_are_capped_and_say_what_they_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let argv = vec![
            "rustc".to_string(),
            "io-harness-no-such-file.rs".to_string(),
        ];
        let out = run(
            dir.path(),
            &argv,
            Format::Rendered,
            Duration::from_secs(60),
            20,
        )
        .await;

        let Outcome::Found(text) = &out else {
            panic!("rustc reports a missing input on stderr: {out:?}");
        };
        assert!(
            text.contains("characters elided"),
            "the cap was applied and the model is told: {text}"
        );

        // The negative control: the same command, uncapped, keeps the whole message.
        let whole = run(
            dir.path(),
            &argv,
            Format::Rendered,
            Duration::from_secs(60),
            CAP,
        )
        .await;
        let Outcome::Found(text) = &whole else {
            panic!("{whole:?}");
        };
        assert!(!text.contains("characters elided"), "{text}");
        assert!(text.contains("io-harness-no-such-file.rs"), "{text}");
    }

    /// A checker that fails for its own reasons is unchecked, not clean — and the
    /// edit that preceded it still stands.
    #[tokio::test]
    async fn a_checker_that_exits_nonzero_saying_nothing_is_unchecked_rather_than_clean() {
        let dir = tempfile::tempdir().unwrap();
        // Exits non-zero and writes its complaint to stderr, which the JSON parser
        // does not read — so this is the "said nothing" case as far as the format is
        // concerned.
        let argv = vec![
            "rustc".to_string(),
            "io-harness-no-such-file.rs".to_string(),
        ];
        let out = run(
            dir.path(),
            &argv,
            Format::CargoJson,
            Duration::from_secs(60),
            CAP,
        )
        .await;
        assert!(matches!(out, Outcome::Failed(_)), "{out:?}");
    }

    /// Every command in the table names a program and is a type-check rather than a
    /// build, which is the property that makes running one after every edit viable.
    #[test]
    fn every_checker_in_the_table_has_a_program_and_a_distinct_ecosystem() {
        let mut seen = Vec::new();
        for (eco, argv, _) in CHECKERS {
            assert!(!argv.is_empty(), "{eco} has no program");
            assert!(!seen.contains(eco), "{eco} appears twice: first match wins");
            seen.push(eco);
        }
        // The one entry whose output is parsed rather than passed through.
        let json: Vec<_> = CHECKERS
            .iter()
            .filter(|(_, _, f)| *f == Format::CargoJson)
            .collect();
        assert_eq!(json.len(), 1, "only cargo's stream needs a parser");
    }
}
