//! The composed system prompt (0.45.0): what it says, who may change it, and the
//! one sentence nobody may.
//!
//! Every assertion here reads the `system` string off a request a fixture provider
//! actually received, so what is tested is what is sent rather than what a helper
//! returns.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_tree, run_with, ApproveAll, Containment, Policy, Provider, Session, Store, SystemPrompt,
    TaskContract, Verification,
};
use serde_json::json;

// ------------------------------------------------------------- 0.44.0 baselines
//
// Literal, and captured from 0.44.0's own output rather than rebuilt from the
// helpers under test — a control built out of the thing it controls is not one.

const V0440_WORKSPACE: &str = "You are an agent working across a repository to meet a stated specification. Use `grep` to search file contents and `find` to locate files by name, then `read_file` to inspect a file before changing it, and `write_file` with the file's path and full new contents to edit it. You may edit several files. Work in small steps; after each of your steps the whole set is checked against the success criterion. Do not explain; call tools.";

const V0440_SINGLE: &str = "You are an agent that edits exactly one file to meet a stated specification. Call the `write_file` tool with the file's full new contents. Do not explain; make the edit. The file will be checked against the success criterion after each write.";

const V0440_CONVERSATIONAL: &str = "You are an agent working across a repository to meet a stated specification. Use `grep` to search file contents and `find` to locate files by name, then `read_file` to inspect a file before changing it, and `write_file` with the file's path and full new contents to edit it. You may edit several files. Work in small steps; after each of your steps the whole set is checked against the success criterion. What the operator has said may not be work at all — it may be a greeting, a question about you or what you can do, or a remark that wants nothing done. If a plain answer is the whole of what is wanted, write that answer and call no tool. If any part of it needs the repository read or changed, call a tool and start: do not describe what you are about to do instead of doing it, and do not promise to act in prose. When the two readings are both possible, act.";

const V0440_TREE: &str = "You are an agent working across a repository to meet a stated specification. Use `grep`, `find`, `read_file`, and `write_file` as in a normal run. You may also decompose the work: call `spawn_agent` to launch a sub-agent that pursues a smaller goal over the same workspace, and its result is reported back to you. A sub-agent inherits your permissions and can only be more restricted, never less. Prefer spawning when parts of the task are independent. Work in small steps; the whole set is checked against the success criterion after each. Do not explain; call tools.";

/// The workspace prompt of a run that also carries a skills catalogue, which is
/// where the relocation is visible: 0.44.0 put the ending *before* this catalogue.
const V0440_WORKSPACE_SKILLS: &str = "You are an agent working across a repository to meet a stated specification. Use `grep` to search file contents and `find` to locate files by name, then `read_file` to inspect a file before changing it, and `write_file` with the file's path and full new contents to edit it. You may edit several files. Work in small steps; after each of your steps the whole set is checked against the success criterion. Do not explain; call tools. These extra tools are also available and work the same way: read_skill. Each tool's result appears in the observations below; once a tool has returned what you asked for, move on rather than calling it again.\n\nSkills available to you — instructions written for this repository. Only each skill's name and description is shown; call `read_skill` with a name to read that skill's full text when its description matches what you are doing.\n- alpha: how to alpha";

/// The ending every prompt but a classifying turn's carries.
const CALL_TOOLS_ENDING: &str = " Do not explain; call tools.";

/// The sentence that decides what a turn is, and the one no caller may weaken.
const CONVERSATIONAL_ENDING: &str = " What the operator has said may not be work at all — it may be a greeting, a question about you or what you can do, or a remark that wants nothing done. If a plain answer is the whole of what is wanted, write that answer and call no tool. If any part of it needs the repository read or changed, call a tool and start: do not describe what you are about to do instead of doing it, and do not promise to act in prose. When the two readings are both possible, act.";

// ----------------------------------------------------------------- scaffolding

/// Records every request and plays a fixed script.
struct Rec {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Rec {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The system block of the first request, which is the composed prompt.
    fn system(&self) -> String {
        self.seen.lock().unwrap()[0].system.clone()
    }
}

impl Provider for Rec {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req);
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "rec"
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    dir
}

fn write_call() -> Vec<ToolCall> {
    vec![ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": "a.txt", "content": "ok" }),
    }]
}

/// The workspace prompt this build composes, for a contract the caller shaped.
async fn workspace_system(contract: &TaskContract) -> String {
    let provider = Rec::new(vec![write_call()]);
    let store = Store::memory().unwrap();
    let _ = run_with(
        contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await;
    provider.system()
}

/// The prompt a classifying session turn's first completion is made with.
async fn conversational_system(contract: &TaskContract, root: &std::path::Path) -> String {
    let provider = Rec::new(vec![vec![]]);
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, root).unwrap();
    let _ = session
        .turn_bounded(
            contract,
            &provider,
            &store,
            &Policy::permissive(),
            &ApproveAll,
        )
        .await;
    provider.system()
}

/// The tree loop's prompt for the root agent.
async fn tree_system(contract: &TaskContract) -> String {
    let provider = Rec::new(vec![write_call()]);
    let store = Store::memory().unwrap();
    let _ = run_tree(
        contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(4, 2, 2, 100_000),
    )
    .await;
    provider.system()
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("do the thing", root).with_max_steps(1)
}

/// 0.44.0's string with its ending sentence taken out — the prefix 0.45.0 must
/// still start with. `replacen` and not `replace`: the sentence appears once, and a
/// test that quietly removed a second occurrence would be asserting less than it
/// says.
fn without_ending(baseline: &str, ending: &str) -> String {
    assert!(baseline.contains(ending), "baseline carries its ending");
    baseline.replacen(ending, "", 1)
}

/// Both halves of F3's claim, for one shape.
fn relocated(composed: &str, baseline: &str, ending: &str) {
    let prefix = without_ending(baseline, ending);
    assert!(
        composed.starts_with(&prefix),
        "0.44.0's sentences are no longer the start of the prompt.\n--- expected prefix ---\n{prefix}\n--- composed ---\n{composed}"
    );
    assert!(
        composed.ends_with(ending),
        "the crate's own ending is not last.\n--- composed ---\n{composed}"
    );
}

// ------------------------------------------------------------------------- F3

/// F3 — a permissive, uncontained, uninstructed `Builtin` run is 0.44.0's own
/// sentences, with the ending relocated and nothing else changed.
///
/// The relocation is the release's one cost to an existing caller and it is stated
/// rather than discovered (`US-IO-HARNESS-0.45.0-I01`): 0.44.0 put the ending inside
/// the base string, so the tool and skill catalogues were appended *after* it, and
/// "the crate's rule is the last word" could not be true. The assertion is a
/// byte-exact prefix plus a byte-exact suffix, which still admits no sentence being
/// added, dropped or reworded — only the one move.
#[tokio::test]
async fn a_default_run_is_0_44_0_with_the_ending_moved_to_the_end() {
    let dir = workspace();

    relocated(
        &workspace_system(&contract(dir.path())).await,
        V0440_WORKSPACE,
        CALL_TOOLS_ENDING,
    );

    relocated(
        &conversational_system(&contract(dir.path()), dir.path()).await,
        V0440_CONVERSATIONAL,
        CONVERSATIONAL_ENDING,
    );

    relocated(
        &tree_system(&contract(dir.path())).await,
        V0440_TREE,
        CALL_TOOLS_ENDING,
    );

    // Single-file mode has no ending to relocate: one tool, no policy enforcement
    // and no turn to classify, so there is no rule about how a turn ends. Its
    // prompt is 0.44.0's exactly, which is the stronger claim and is made here
    // rather than left implicit.
    let file = dir.path().join("a.txt");
    let single = TaskContract::new(
        "do the thing",
        &file,
        Verification::FileContains("never".into()),
    )
    .with_max_steps(1);
    let provider = Rec::new(vec![vec![ToolCall {
        name: "write_file".into(),
        arguments: json!({ "content": "ok" }),
    }]]);
    let store = Store::memory().unwrap();
    let _ = run_with(
        &single,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await;
    assert_eq!(provider.system(), V0440_SINGLE);
}

/// F3, and the arm that can actually see the move: a run carrying a skills
/// catalogue.
///
/// 0.44.0 emitted `description + ending + catalogue`; 0.45.0 emits
/// `description + catalogue + ending`. Without a catalogue the two are the same
/// string, so a test with no extras would pass against an implementation that never
/// relocated anything.
#[tokio::test]
async fn the_ending_follows_the_catalogues_it_used_to_precede() {
    let dir = workspace();
    let skills = tempfile::tempdir().unwrap();
    std::fs::write(
        skills.path().join("alpha.md"),
        "---\nname: alpha\ndescription: how to alpha\n---\n\nALPHA BODY LINE\n",
    )
    .unwrap();

    let composed = workspace_system(&contract(dir.path()).with_skills(skills.path())).await;

    relocated(&composed, V0440_WORKSPACE_SKILLS, CALL_TOOLS_ENDING);
    // And the move is real: the catalogue is now before the ending, not after it.
    let catalogue = composed.find("Skills available to you").unwrap();
    let ending = composed.rfind(CALL_TOOLS_ENDING).unwrap();
    assert!(
        catalogue < ending,
        "the skills catalogue still follows the ending"
    );
    // No skill body ever rides in a prompt (0.33.0), asserted here because this
    // test is the one that would notice a catalogue that started carrying one.
    assert!(!composed.contains("ALPHA BODY LINE"));
}

// ------------------------------------------------------------------------- F4

/// F4 — the caller's text sits where it was promised, and the ending survives every
/// variant.
///
/// `Replace("")` is the case that finds the natural mistake: substituting the
/// caller's string for the whole composed prompt passes every non-empty variant and
/// produces a prompt with no rules at all for this one.
#[tokio::test]
async fn no_caller_prompt_can_get_past_the_ending() {
    let dir = workspace();
    const HOUSE: &str = "ACME-HOUSE-STYLE prefer the smallest diff that works.";
    const INSTEAD: &str = "ACME-REPLACED you are Acme's release bot.";

    // Builtin: the crate's description, and its ending last.
    let builtin = conversational_system(&contract(dir.path()), dir.path()).await;
    assert!(builtin.starts_with("You are an agent working across a repository"));
    assert!(builtin.ends_with(CONVERSATIONAL_ENDING));

    // Append: after the crate's description, before the crate's ending.
    let appended = conversational_system(
        &contract(dir.path()).with_system_prompt(SystemPrompt::Append(HOUSE.into())),
        dir.path(),
    )
    .await;
    assert!(appended.starts_with("You are an agent working across a repository"));
    assert!(appended.ends_with(CONVERSATIONAL_ENDING));
    let at = appended.find(HOUSE).expect("the appended text is carried");
    assert!(
        at < appended.rfind(CONVERSATIONAL_ENDING).unwrap(),
        "the appended text was emitted after the crate's ending"
    );

    // Replace: the caller's description instead of the crate's, and the ending
    // still last.
    let replaced = conversational_system(
        &contract(dir.path()).with_system_prompt(SystemPrompt::Replace(INSTEAD.into())),
        dir.path(),
    )
    .await;
    assert!(replaced.starts_with(INSTEAD));
    assert!(replaced.ends_with(CONVERSATIONAL_ENDING));
    assert!(
        !replaced.contains("You are an agent working across a repository"),
        "Replace did not replace the crate's description"
    );

    // Replace(""): nothing of the caller's, and still every rule of the crate's.
    let empty = conversational_system(
        &contract(dir.path()).with_system_prompt(SystemPrompt::Replace(String::new())),
        dir.path(),
    )
    .await;
    assert!(
        empty.ends_with(CONVERSATIONAL_ENDING),
        "an empty replacement swallowed the crate's ending"
    );
}
