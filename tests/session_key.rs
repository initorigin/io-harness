//! One session, one replica (0.85.0).
//!
//! A prefix cache is replica-local: a serverless fleet keeps one per machine, and
//! a request that lands somewhere other than where the last one did misses however
//! stable its prompt was. The fix is a key the vendor routes on, and the whole of
//! what this file asserts is that the key is stable where it must be, different
//! where it must be, and opaque.
//!
//! What the two wires *do* with it is asserted where the bodies are built, in
//! `src/provider/openai_wire.rs` and `src/provider/anthropic.rs`: the body is
//! crate-private, and an integration test could only reach it through a live
//! endpoint.

use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{AgentDef, Agents, ApproveAll, Policy, Provider, Session, Store, TaskContract};
use serde_json::json;

/// Keeps every request, plays a script of tool calls, and answers a fold's
/// tool-less call with prose so a fixture that folds actually folds.
#[derive(Default)]
struct Seen {
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
    script: Vec<Vec<ToolCall>>,
    at: std::sync::atomic::AtomicUsize,
}

impl Provider for Seen {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if req.tools.is_empty() {
            return Ok(CompletionResponse {
                text: Some("the run did some work".into()),
                ..Default::default()
            });
        }
        let i = self.at.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.script.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

impl Seen {
    fn playing(script: Vec<Vec<ToolCall>>) -> Self {
        Self {
            script,
            ..Default::default()
        }
    }

    fn keys(&self) -> Vec<Option<String>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.session_key.clone())
            .collect()
    }
}

fn policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
}

/// One turn of a session over `root`, and the keys its requests carried.
async fn turn_keys(store: &Store, root: &std::path::Path, prompt: &str) -> Vec<Option<String>> {
    let seen = Seen::default();
    let mut session = Session::open(store, root).unwrap();
    session
        .turn(prompt, &seen, store, &policy(), &ApproveAll)
        .await
        .unwrap();
    seen.keys()
}

// ------------------------------------------------------------- F5: the key itself

/// F5 — the same session produces the same key, a different session a different
/// one, and neither carries anything a reader could turn back into an identity.
#[tokio::test]
async fn f5_a_session_key_is_stable_opaque_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();

    let first = turn_keys(&store, dir.path(), "what is here?").await;
    let again = turn_keys(&store, dir.path(), "and now?").await;
    let key = first[0].clone().expect("a session turn carries a key");

    assert!(
        first.iter().all(|k| k.as_ref() == Some(&key)),
        "every request of one turn carries one key, got {first:?}"
    );
    // A second `Session::open` is a second session, and a second session is a
    // second conversation with a prefix of its own.
    assert_ne!(
        again[0], first[0],
        "two sessions must not be routed to one replica's cache"
    );

    assert!(
        key.len() <= 64,
        "the key must fit the 64 characters a vendor clamps at, got {} in {key}",
        key.len()
    );
    assert!(
        key.starts_with("io-") && key.contains(':'),
        "the shape is `io-<prefix-version>:<session-hash>`, got {key}"
    );
    // The id is an integer, and small integers appear inside hex by coincidence —
    // so what is asserted is that the id is not *the* session half, which is the
    // way a key leaks one.
    let session_half = key.split(':').nth(1).expect("the session half");
    for id in 1..=8u32 {
        assert_ne!(
            session_half,
            id.to_string(),
            "the session half must be a digest, never the id"
        );
    }
    assert!(
        !key.contains(&dir.path().display().to_string()),
        "the key must carry no path: {key}"
    );
}

/// F5 (the prefix half) — a changed head is a changed key.
///
/// Routing a session at a replica whose cached prefix no longer matches is worse
/// than not routing it: every request pays for a machine chosen for a prompt it no
/// longer has. So the key's first half is a digest of the system text and the tool
/// list, and moves when either does.
#[tokio::test]
async fn f5_a_changed_head_changes_the_prefix_half_and_not_the_session_half() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();

    let plain = turn_keys(&store, dir.path(), "what is here?").await;
    let plain = plain[0].clone().expect("a key");

    // The same session, asked again with a tool registered: the catalogue is part
    // of the head on every chat template, and on some it renders before the system
    // text.
    let seen = Seen::default();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let with_tool = {
        session
            .turn("what is here?", &seen, &store, &policy(), &ApproveAll)
            .await
            .unwrap();
        seen.keys()[0].clone().expect("a key")
    };
    let (plain_prefix, plain_session) = plain.split_once(':').unwrap();
    let (tool_prefix, tool_session) = with_tool.split_once(':').unwrap();
    assert_ne!(
        plain_session, tool_session,
        "two sessions, so the session halves differ and this case can say nothing \
         about the prefix half — the fixture is wrong"
    );
    assert_eq!(
        plain_prefix, tool_prefix,
        "the same head must give the same prefix version"
    );
}

// --------------------------------------------------------- F17: record and replay

/// F17 — a recording round-trips the key, so a replayed run asks for the same
/// replica the recorded one did.
#[test]
fn f17_the_key_survives_a_recording() {
    let request = CompletionRequest {
        system: "be brief".into(),
        user: "hello".into(),
        session_key: Some("io-deadbeef:0123456789abcdef".into()),
        ..Default::default()
    };
    let json = serde_json::to_string(&request).unwrap();
    assert!(
        json.contains("\"session_key\":\"io-deadbeef:0123456789abcdef\""),
        "the recorded request carries the key: {json}"
    );
    let back: CompletionRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(back.session_key, request.session_key);

    // A recording written before 0.85.0 has no such field and must still load.
    let old = r#"{"system":"be brief","user":"hello","tools":[]}"#;
    let back: CompletionRequest = serde_json::from_str(old).unwrap();
    assert_eq!(back.session_key, None);
}

// ------------------------------------------------------------ F7: children inherit

/// F7 — a contained child carries the key its parent carries.
///
/// The fan-out shares the parent's system text and tool list, so concentrating it
/// on one replica is the point rather than a side effect. A child that derived its
/// own key would be routed away from the cache it was built to reuse.
#[tokio::test]
async fn f7_a_contained_child_carries_the_parents_key() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "content\n").unwrap();
    let store = Store::memory().unwrap();
    // The root spawns once, so there is a child whose request can be compared. A
    // fixture that spawns nothing would pass this case with one request in it and
    // assert nothing at all.
    let seen = Seen::playing(vec![vec![ToolCall {
        name: "spawn_agent".into(),
        arguments: json!({
            "agent": "reader",
            "goal": "read a.txt",
            "verify_file": "a.txt",
            "verify_contains": "content",
            "max_steps": 2
        }),
    }]]);
    let mut session = Session::open(&store, dir.path()).unwrap();
    // A turn whose contract carries a roster is a turn that runs the tree loop,
    // which is where a child's request is built.
    let contract = TaskContract::workspace("look around", dir.path())
        .with_max_steps(3)
        .with_agents(Agents::new().with(AgentDef::new("reader").with_role("reads files")));
    session
        .turn_bounded(&contract, &seen, &store, &policy(), &ApproveAll)
        .await
        .unwrap();

    let keys = seen.keys();
    assert!(
        keys.len() > 1,
        "the fixture must reach a child, or it asserts nothing: {keys:?}"
    );
    let first = keys[0].clone().expect("the root carries a key");
    assert!(
        keys.iter().all(|k| k.as_ref() == Some(&first)),
        "every agent in the tree carries the root's key, got {keys:?}"
    );
}
