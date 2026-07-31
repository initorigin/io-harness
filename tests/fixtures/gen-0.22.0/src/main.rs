//! Writes the three fixture stores that `tests/cross_version.rs` reads back, using
//! a real io-harness 0.22.0 from crates.io — the release before the `rusqlite`
//! 0.32 -> 0.40 upgrade.
//!
//! The point of the whole crate is that the *writer* is the previous dependency
//! line, not this tree with a 0.22.0 label on it. A fixture the current code
//! wrote proves that the current code can read what the current code writes,
//! which is not the question the upgrade raises. So this binary links `=0.22.0`,
//! resolves `rusqlite` 0.32.1 / `libsqlite3-sys` 0.30.1, and produces databases
//! whose every byte — page format, WAL frames, `user_version`, the encoding of
//! every `u64` token counter — came from that stack.
//!
//! Three databases, one per thing the upgrade must not break:
//!
//! * `populated.sqlite3` — a finished run with as much of the row surface filled
//!   as one run can fill: steps, provider calls with a full `Usage`, an
//!   observation ledger, an edit, a policy refusal and the approvals around it,
//!   citations, provider-executed tool calls, and a pending approval resolved by
//!   hand. This is the *reading* half: nothing resumes it, everything is read.
//! * `interrupted.sqlite3` — a sub-agent tree stopped mid-flight with one child
//!   finished and one not, left resumable. This is the *resuming* half.
//! * `deferred-approval.sqlite3` — a run paused on a deferred approval, which is
//!   the other way a run survives a process boundary: not a crash, a decision
//!   nobody has made yet.
//!
//! Both resumable databases keep their workspace directory beside them, because a
//! resume needs the files the interrupted run left as much as it needs the rows.
//!
//! Every fixture gets a JSON sidecar recording what a reader should find. The
//! sidecars are built by reading the finished store back rather than from what
//! this code intended to write, so they cannot describe a row that is not there.
//! Nothing in them is wall-clock derived — ids, counts and token totals only —
//! since a fixture whose expectations drift with the clock is a flaky test with
//! extra steps.
//!
//! Fully offline and deterministic: scripted providers, no network, no API key.
//! Every workspace path is *relative*, and the process runs with its working
//! directory set to the output directory, so the absolute path of the machine
//! that generated the fixtures never lands in `runs.file`.
//!
//! Usage: `gen-0-22-0 <output-dir>`.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_tree, run_with, ApproveAll, Citation, Containment, Policy, Provider, RunOutcome,
    ServerToolCall, Store, TaskContract, Verification,
};
use serde_json::{json, Value};

/// The binary's own error type. The three generators touch `io_harness::Error`,
/// `std::io::Error` and `serde_json::Error`, and a fixture generator that fails
/// has exactly one useful behaviour — print why and exit non-zero — so there is
/// nothing for a typed error to decide.
type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------- providers ----------

/// Plays a fixed list of whole [`CompletionResponse`]s, one per step, in order.
///
/// The scripted providers in the main crate's tests carry `(content, tokens)`
/// pairs, because a test asserting on one thing needs one thing varied. This one
/// scripts the entire response: the populated fixture exists to fill every
/// column the response can reach — `usage` in all six of its counters, `model`,
/// `finish_reason`, `citations`, `server_tools` — and a narrower script would
/// leave the columns it does not know about empty, which is the one outcome this
/// fixture must avoid.
///
/// Past the end of the script it answers with nothing at all, so a run only ever
/// stops where the script says it does.
struct Script {
    steps: Vec<CompletionResponse>,
    at: AtomicUsize,
}

impl Script {
    fn new(steps: Vec<CompletionResponse>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(self.steps.get(i).cloned().unwrap_or_default())
    }
    /// Recorded verbatim in `provider_calls.provider`, so it has to be a fixed
    /// string rather than the `"provider"` default — a fixture whose provider
    /// column reads like a placeholder tells a reader nothing about whether the
    /// column survived the upgrade.
    fn name(&self) -> &str {
        "fixture"
    }
}

/// Writes one fixed `(path, content)` on every turn, forever.
///
/// Stateless on purpose: a step that is replayed on resume behaves identically.
/// Pointed at a contract its content does not satisfy, it produces a run that
/// keeps going until something else stops it — which for the deferred-approval
/// fixture is the approver, on step 1.
struct WriteOnce {
    path: &'static str,
    content: &'static str,
}

impl Provider for WriteOnce {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            tool_calls: vec![call(
                "write_file",
                json!({ "path": self.path, "content": self.content }),
            )],
            usage: Some(usage(90, 10)),
            model: Some(MODEL.into()),
            ..Default::default()
        })
    }
    fn name(&self) -> &str {
        "fixture"
    }
}

/// A stateless tree provider that decides purely from the prompt, so a resumed or
/// replayed step behaves identically: a COORDINATOR agent fans out to two
/// children, and a child whose goal reads `FILE=x CONTENT=y` writes `y` into `x`.
///
/// The two children are deliberately asymmetric. One is asked for the content its
/// own verification wants and finishes; the other is asked for content that can
/// never satisfy it and runs out of steps. That asymmetry is the fixture: a tree
/// where everything finished is not interrupted, and one where nothing finished
/// does not prove that a *completed* child is adopted rather than re-run.
struct TreeProvider;

impl Provider for TreeProvider {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if req.user.contains("COORDINATOR") {
            return Ok(CompletionResponse {
                tool_calls: vec![
                    spawn("FILE=a.txt CONTENT=ALPHA", "a.txt", "ALPHA"),
                    // Asked for DRAFT, verified on BETA: this child cannot win.
                    spawn("FILE=b.txt CONTENT=DRAFT", "b.txt", "BETA"),
                ],
                usage: Some(usage(120, 40)),
                model: Some(MODEL.into()),
                ..Default::default()
            });
        }
        if let Some(idx) = req.user.find("FILE=") {
            let rest = &req.user[idx + 5..];
            let file = rest
                .split_whitespace()
                .next()
                .unwrap_or("x.txt")
                .to_string();
            let content = rest
                .find("CONTENT=")
                .map(|c| {
                    rest[c + 8..]
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            return Ok(CompletionResponse {
                tool_calls: vec![call(
                    "write_file",
                    json!({ "path": file, "content": format!("{content}\n") }),
                )],
                usage: Some(usage(60, 20)),
                model: Some(MODEL.into()),
                ..Default::default()
            });
        }
        Ok(CompletionResponse::default())
    }
    fn name(&self) -> &str {
        "fixture"
    }
}

/// An approver that always defers, so the run pauses instead of deciding.
struct Defer;

impl Approver for Defer {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::Defer })
    }
}

// ---------- shapes shared by the three fixtures ----------

/// The model name every scripted response reports. Fixed rather than absent: a
/// `NULL` `provider_calls.model` and a model that round-tripped are different
/// facts, and only the second one proves the column survived.
const MODEL: &str = "fixture-model";

/// One completion's token report, with every counter the 0.18.0 schema widened to
/// carry set to something distinguishable. The breakdown counters are deliberately
/// *not* proportional to each other, so a reader that transposes two of them
/// fails instead of accidentally agreeing.
fn usage(prompt: u64, completion: u64) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cache_read_tokens: 7,
        cache_write_tokens: 3,
        reasoning_tokens: 5,
        server_tool_requests: 1,
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn spawn(goal: &str, file: &str, needle: &str) -> ToolCall {
    call(
        "spawn_agent",
        json!({
            "goal": goal,
            "verify_file": file,
            "verify_contains": needle,
            // Bounded explicitly: the losing child would otherwise burn the
            // workspace contract's default budget writing DRAFT twelve times, and
            // the fixture wants a short, fixed trace, not a long one.
            "max_steps": 2,
        }),
    )
}

/// The boundary the populated fixture runs under: reads open, execs named, and
/// the secret paths closed outright. Writes are left on the tiered default, which
/// is Ask — so the allowed write below produces an approval *decision* row as well
/// as an edit, and the run exercises both halves of the policy trace.
fn guarded() -> Policy {
    Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_exec("cargo")
        .deny_read("secrets/*")
        .deny_write("secrets/*")
}

/// A workspace contract over `root` satisfied by `needle` landing in `out.txt`.
///
/// Workspace mode rather than single-file, deliberately: the observation ledger
/// and the policy gate only exist on that path, so a single-file contract would
/// leave two of the tables this fixture is meant to populate empty.
fn out_contract(root: &str, needle: &str, max_steps: u32) -> TaskContract {
    TaskContract::workspace(
        "write out.txt",
        root,
        Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: needle.into(),
        },
    )
    .with_max_steps(max_steps)
}

/// The coordinator's contract: it may not write anything itself, and it is judged
/// on what its children left behind.
fn tree_contract(root: &str) -> TaskContract {
    TaskContract::workspace(
        "COORDINATOR: delegate to sub-agents; do not write files yourself.",
        root,
        Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "BETA".into(),
        },
    )
}

/// Generous enough that nothing here is stopped by the tree boundary — the
/// interrupted fixture must be interrupted by its step cap, not by containment,
/// or it would be a fixture about refusing to spawn.
fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

/// Write a sidecar next to its database, pretty-printed and newline-terminated so
/// a diff of a regenerated fixture is readable line by line.
fn sidecar(path: &str, value: &Value) -> Res<()> {
    let mut json = serde_json::to_string_pretty(value)?;
    json.push('\n');
    std::fs::write(path, json)?;
    Ok(())
}

// ---------- fixture 1: a completed run, every table it can reach ----------

/// A four-step workspace run that finishes verified, having touched everything a
/// single run can touch.
///
/// The four steps are chosen for what they leave behind rather than for realism:
/// a read and a grep to fill the observation ledger with two different
/// `ObsKind`s, a write into the denied path to leave a refusal attributable to a
/// named rule and layer, and the write that satisfies the contract — which under
/// the Ask default also leaves an approval decision and an edit row.
///
/// The citations and provider-executed tool calls ride on the last response
/// rather than being inserted afterwards, so `Store::record_citations` and
/// `Store::record_server_tool_calls` are reached the way a real run reaches them,
/// through the loop. The pending approval *is* written by hand, because a run
/// that resolves its own approval never leaves a resolved row behind to read.
async fn populated(ws: &str) -> Res<()> {
    std::fs::create_dir_all(ws)?;
    std::fs::write(
        Path::new(ws).join("notes.txt"),
        "seed line one\nseed line two\n",
    )?;

    let store = Store::open("populated.sqlite3")?;
    let script = Script::new(vec![
        // 1. A read: an observation with a path target.
        CompletionResponse {
            tool_calls: vec![call("read_file", json!({ "path": "notes.txt" }))],
            usage: Some(usage(100, 20)),
            model: Some(MODEL.into()),
            finish_reason: Some("tool_use".into()),
            ..Default::default()
        },
        // 2. A write the policy denies outright: the refusal row.
        CompletionResponse {
            tool_calls: vec![call(
                "write_file",
                json!({ "path": "secrets/key.txt", "content": "exfiltrated\n" }),
            )],
            usage: Some(usage(200, 30)),
            model: Some(MODEL.into()),
            finish_reason: Some("tool_use".into()),
            ..Default::default()
        },
        // 3. A search: a second observation kind, targeted by pattern not path.
        CompletionResponse {
            tool_calls: vec![call("grep", json!({ "pattern": "seed" }))],
            usage: Some(usage(300, 40)),
            model: Some(MODEL.into()),
            finish_reason: Some("tool_use".into()),
            ..Default::default()
        },
        // 4. The write that satisfies the contract, carrying the web surface.
        CompletionResponse {
            text: Some("writing the answer".into()),
            tool_calls: vec![call(
                "write_file",
                json!({ "path": "out.txt", "content": "DONE\n" }),
            )],
            usage: Some(usage(400, 50)),
            model: Some(MODEL.into()),
            finish_reason: Some("end_turn".into()),
            citations: vec![
                Citation {
                    url: "https://example.invalid/one".into(),
                    title: Some("The first source".into()),
                    cited_text: Some("a quoted passage".into()),
                },
                // Title and text absent: a provider that cites a bare URL is the
                // case that turns two columns NULL, and NULL is exactly what a
                // driver upgrade can start reading back as an empty string.
                Citation {
                    url: "https://example.invalid/two".into(),
                    title: None,
                    cited_text: None,
                },
            ],
            server_tools: vec![
                ServerToolCall {
                    provider: "anthropic".into(),
                    tool: "web_search".into(),
                    error: None,
                },
                ServerToolCall {
                    provider: "anthropic".into(),
                    tool: "web_search".into(),
                    error: Some("max_uses_exceeded".into()),
                },
            ],
            ..Default::default()
        },
    ]);

    let result = run_with(
        &out_contract(ws, "DONE", 8),
        &script,
        &store,
        &guarded(),
        &ApproveAll,
    )
    .await?;
    let run_id = result.run_id;
    assert!(
        matches!(result.outcome, RunOutcome::Success { steps: 4 }),
        "the populated fixture must be a finished run: {:?}",
        result.outcome
    );

    // A pending approval that outlived its decision. `run_with` above resolves
    // and forgets its own; this is the row a caller who deferred, decided out of
    // band, and resumed would leave behind.
    let request_id = store.put_pending(
        run_id,
        4,
        "write",
        "deploy/prod.yaml",
        Some("replicas: 4\n"),
    )?;
    store.resolve_pending(request_id, "approve")?;

    // Everything below is read back out of the finished store, never asserted
    // from what the code above meant to write.
    let steps: Vec<Value> = store
        .steps(run_id)?
        .iter()
        .map(|s| json!({ "step": s.step, "tokens": s.tokens, "decision": s.decision }))
        .collect();
    let calls: Vec<Value> = store
        .provider_calls(run_id)?
        .iter()
        .map(|c| {
            json!({
                "step": c.step,
                "attempt": c.attempt,
                "provider": c.provider,
                "model": c.model,
                "finish_reason": c.finish_reason,
                "failure": c.failure,
                "usage": c.usage,
            })
        })
        .collect();
    let observations: Vec<Value> = store
        .observations(run_id)?
        .iter()
        .map(|o| json!({ "step": o.step, "kind": o.kind, "target": o.target }))
        .collect();
    let edits: Vec<Value> = store
        .edits(run_id)?
        .iter()
        .map(|e| {
            json!({
                "step": e.step,
                "tool": e.tool,
                "path": e.path,
                "lines_added": e.lines_added,
                "lines_removed": e.lines_removed,
            })
        })
        .collect();
    let policy_events: Vec<Value> = store
        .events(run_id)?
        .iter()
        .map(|e| {
            json!({
                "step": e.step,
                "kind": e.kind,
                "act": e.act,
                "target": e.target,
                "rule": e.rule,
                "layer": e.layer,
                "decision": e.decision,
                "source": e.source,
                "performed": e.performed,
            })
        })
        .collect();
    let pending = store.pending(request_id)?.expect("the row just written");
    let citations: Vec<Value> = store
        .citations(run_id)?
        .iter()
        .map(|c| json!({ "url": c.url, "title": c.title, "cited_text": c.cited_text }))
        .collect();
    let server_tools: Vec<Value> = store
        .server_tool_calls(run_id)?
        .iter()
        .map(|c| json!({ "provider": c.provider, "tool": c.tool, "error": c.error }))
        .collect();

    sidecar(
        "populated.json",
        &json!({
            "run_id": run_id,
            "goal": "write out.txt",
            "workspace": ws,
            "outcome": store.outcome(run_id)?,
            "status": store.status(run_id)?,
            "step_count": steps.len(),
            "last_step": store.last_step(run_id)?,
            "spent_tokens": store.spent_tokens(run_id)?,
            "steps": steps,
            "provider_calls": calls,
            "observations": observations,
            "edits": edits,
            "policy_events": policy_events,
            "checkpoint_event_count": store.checkpoint_events(run_id)?.len(),
            "pending": {
                "request_id": pending.id,
                "run_id": pending.run_id,
                "step": pending.step,
                "act": pending.act,
                "target": pending.target,
                "content": pending.content,
                "resolved": pending.resolved,
            },
            "citations": citations,
            "server_tool_calls": server_tools,
        }),
    )?;

    // Closing the connection is what checkpoints and removes the WAL, which is
    // what makes the `.sqlite3` file alone a complete database — and this fixture
    // is committed as one file.
    drop(store);
    // The populated run is read, never resumed, so its workspace is scratch and
    // is not committed. Only the two resumable fixtures keep theirs.
    std::fs::remove_dir_all(ws)?;
    Ok(())
}

// ---------- fixture 2: a tree stopped mid-flight, still resumable ----------

/// A coordinator, two children, and a step cap of one on the root.
///
/// The step cap is the crash model, and it is the honest one for a *committed*
/// fixture: a run cut off by dropping its future or by a signal leaves whatever
/// the WAL happened to hold at that instant, which is neither reproducible nor a
/// single file. Reaching the cap stops the run at a step boundary with every
/// committed row flushed, the process exits normally, and the database on disk is
/// exactly what a restarted process would find — a root that stopped short, one
/// child finished, one child not, and a tree budget partly drawn.
async fn interrupted(ws: &str) -> Res<()> {
    std::fs::create_dir_all(ws)?;

    let store = Store::open("interrupted.sqlite3")?;
    let result = run_tree(
        &tree_contract(ws).with_max_steps(1),
        &TreeProvider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await?;
    let root = result.run_id;
    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "the interrupted fixture must stop short, not finish: {:?}",
        result.outcome
    );

    let mut children = Vec::new();
    for id in store.children(root)? {
        children.push(json!({
            "run_id": id,
            // Which child this is, by what it wrote rather than by its goal:
            // 0.22.0 has no public reader for `runs.goal`, and the file a child
            // touched identifies it at least as well.
            "wrote": store.edits(id)?.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            "depth": store.depth(id)?,
            "status": store.status(id)?,
            "outcome": store.outcome(id)?,
            "last_step": store.last_step(id)?,
            "spent_tokens": store.spent_tokens(id)?,
        }));
    }

    sidecar(
        "interrupted.json",
        &json!({
            "root_run_id": root,
            "workspace": ws,
            "root_status": store.status(root)?,
            "root_outcome": store.outcome(root)?,
            "root_last_step": store.last_step(root)?,
            "tree_run_ids": store.tree_run_ids(root)?,
            "agent_count_tree": store.agent_count_tree(root)?,
            "spent_tokens_tree": store.spent_tokens_tree(root)?,
            "children": children,
            // The half of the fan-out that finished left its file; the half that
            // did not left the content it could not make satisfy its verification.
            // A resume that re-ran the finished child would disturb the first, and
            // one that finished the tree must replace the second.
            "workspace_files": {
                "a.txt": std::fs::read_to_string(Path::new(ws).join("a.txt")).ok(),
                "b.txt": std::fs::read_to_string(Path::new(ws).join("b.txt")).ok(),
            },
        }),
    )?;

    drop(store);
    Ok(())
}

// ---------- fixture 3: a run paused on a decision nobody made ----------

/// A run that asked for permission to write and was answered with "not yet".
///
/// The other way a run outlives its process. Nothing crashed and no budget ran
/// out: the approver deferred, so the action is persisted under a request id, the
/// run status is `paused`, and the file the run was going to write does not
/// exist. What a reader has to be able to do with this database is find the
/// request, read what it was going to do, and continue.
async fn deferred_approval(ws: &str) -> Res<()> {
    std::fs::create_dir_all(ws)?;
    // A paused run has written nothing, so without this the workspace would be an
    // empty directory — which git does not track, so the committed fixture would
    // arrive with no workspace at all. A seed file also makes the resume
    // meaningful: the run reads a real tree, not a void.
    std::fs::write(Path::new(ws).join("notes.txt"), "seed line one\n")?;

    let store = Store::open("deferred-approval.sqlite3")?;
    let result = run_with(
        &out_contract(ws, "DONE", 8),
        &WriteOnce {
            path: "out.txt",
            content: "DONE\n",
        },
        &store,
        // Ask on exactly the path the provider writes, so the pause is this
        // rule's doing and not a tier default a later release might retune.
        &Policy::default()
            .layer("base")
            .allow_read("*")
            .ask_write("out.txt"),
        &Defer,
    )
    .await?;
    let run_id = result.run_id;
    let request_id = match result.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("the deferred fixture must pause awaiting approval, got {other:?}"),
    };
    assert!(
        !Path::new(ws).join("out.txt").exists(),
        "the deferred write must not have happened — a paused run that already \
         wrote is not paused"
    );

    let pending = store.pending(request_id)?.expect("the persisted request");
    sidecar(
        "deferred-approval.json",
        &json!({
            "run_id": run_id,
            "request_id": request_id,
            "workspace": ws,
            "status": store.status(run_id)?,
            "outcome": store.outcome(run_id)?,
            "last_step": store.last_step(run_id)?,
            "pending": {
                "request_id": pending.id,
                "run_id": pending.run_id,
                "step": pending.step,
                "act": pending.act,
                "target": pending.target,
                "content": pending.content,
                "resolved": pending.resolved,
            },
        }),
    )?;

    drop(store);
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Res<()> {
    let out = std::env::args()
        .nth(1)
        .ok_or("usage: gen-0-22-0 <output-dir>")?;
    std::fs::create_dir_all(&out)?;
    // Everything after this point names files relatively. `runs.file` stores the
    // contract root verbatim, so an absolute root would bake this machine's home
    // directory into a committed fixture and make the workspace unfindable on
    // anyone else's.
    std::env::set_current_dir(&out)?;

    populated("populated-workspace").await?;
    interrupted("interrupted-workspace").await?;
    deferred_approval("deferred-approval-workspace").await?;
    Ok(())
}
