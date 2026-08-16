//! 0.60.0 — the agent mailbox. The first horizontal edge in a tree.
//!
//! A tree could already nest, share one ledger, queue past its concurrency cap and
//! hand a child's report up. Every one of those is a *vertical* edge. Two children
//! investigating two subsystems had no way to tell each other what they found, and
//! a coordinator could not wait on one named child — only spawn and read whatever
//! came back.
//!
//! The tests here are in two layers, and the split is deliberate. The store layer
//! proves the rows: ordering, exactly-once delivery, and that a session delete
//! accounts for the new table. The tree layer proves the address: that a name
//! identifies one agent rather than a role, and that a name resolves inside one
//! tree and nowhere else.
//!
//! Stores are on disk rather than [`Store::memory`](io_harness::Store::memory)
//! wherever a claim is about surviving a process, because an in-memory database
//! cannot be reopened and the resume claim is the one most likely to be wrong.

use io_harness::Store;

/// A store on disk, and the directory that keeps it alive for the test.
fn on_disk() -> (tempfile::TempDir, Store, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("store.db");
    let store = Store::open(&path).expect("a store");
    (dir, store, path)
}

/// **F5, first half — ten messages from three senders come back in row-id order.**
///
/// The senders interleave, which is the point: a per-sender order would pass a
/// fixture where each sender's messages are contiguous and reorder a real tree.
#[test]
fn messages_are_delivered_oldest_first_across_interleaved_senders() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let senders: Vec<(i64, &str)> = ["scout", "critic", "author"]
        .iter()
        .map(|n| (store.start_run(n, "/repo").unwrap(), *n))
        .collect();

    // Ten messages, round-robin over three senders, so no sender's are adjacent.
    let mut sent = Vec::new();
    for i in 0..10u32 {
        let (run, name) = senders[i as usize % senders.len()];
        let body = format!("finding {i}");
        store.send_message(run, me, name, i + 1, &body).unwrap();
        sent.push((name.to_string(), body));
    }

    let inbox = store.read_messages(me, None).unwrap();
    let got: Vec<(String, String)> = inbox
        .iter()
        .map(|m| (m.from_name.clone(), m.body.clone()))
        .collect();
    assert_eq!(got, sent, "delivery order is the order they were sent");
    assert!(
        inbox.iter().all(|m| m.read_at.is_some()),
        "a delivered message carries the mark this read stamped"
    );
}

/// **F5, second half — a second read returns nothing.**
///
/// Exactly-once within one process. The cross-process half is F12.
#[test]
fn a_message_is_delivered_exactly_once() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    store
        .send_message(scout, me, "scout", 1, "src/auth.rs:210")
        .unwrap();

    assert_eq!(store.read_messages(me, None).unwrap().len(), 1);
    assert!(
        store.read_messages(me, None).unwrap().is_empty(),
        "a delivered message is not delivered again"
    );
    // And the audit read still sees it, marked.
    let audit = store.messages_for(me).unwrap();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].read_at.is_some());
}

/// An audit read delivers nothing, which is the whole difference between the two
/// calls. An operator asking what an agent was told must not consume what that
/// agent has not read yet.
#[test]
fn an_audit_read_does_not_deliver() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    store
        .send_message(scout, me, "scout", 1, "waiting")
        .unwrap();

    let audit = store.messages_for(me).unwrap();
    assert_eq!(audit.len(), 1);
    assert!(audit[0].read_at.is_none(), "still waiting");
    assert_eq!(
        store.read_messages(me, None).unwrap().len(),
        1,
        "the audit did not consume it"
    );
}

/// A `from` filter narrows to one sender and leaves the rest waiting — it is a
/// filter on delivery, not a view over it.
#[test]
fn a_from_filter_delivers_only_that_senders_messages() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    let critic = store.start_run("critic", "/repo").unwrap();
    store
        .send_message(scout, me, "scout", 1, "found it")
        .unwrap();
    store
        .send_message(critic, me, "critic", 1, "it is wrong")
        .unwrap();
    store
        .send_message(scout, me, "scout", 2, "and again")
        .unwrap();

    let from_scout = store.read_messages(me, Some("scout")).unwrap();
    assert_eq!(
        from_scout
            .iter()
            .map(|m| m.body.as_str())
            .collect::<Vec<_>>(),
        vec!["found it", "and again"]
    );
    let rest = store.read_messages(me, None).unwrap();
    assert_eq!(rest.len(), 1, "the critic's was left where it was");
    assert_eq!(rest[0].from_name, "critic");
}

/// **N3 — a deleted session leaves no message at either end.**
///
/// The table is in `RUN_TABLES` keyed by the recipient, and the argument that the
/// sender end is covered too — a mailbox lives inside one tree, and a session's run
/// list is that whole tree — is an argument rather than a guarantee. So this counts
/// the table directly after the delete instead of enumerating `sqlite_master`,
/// which 0.58.0 proved cannot fail for a table the fixture never wrote to.
#[test]
fn a_deleted_session_leaves_no_message_at_either_end() {
    let (_dir, store, path) = on_disk();
    let session = store.create_session("/repo").unwrap();
    let parent = store.start_run("coordinate", "/repo").unwrap();
    store.record_turn(session, None, parent, "go").unwrap();
    let child = store.start_run("scout", "/repo").unwrap();
    store.record_turn(session, None, child, "look").unwrap();

    // Both directions, so a cascade that covered only one end still fails.
    store.send_message(child, parent, "scout", 1, "up").unwrap();
    store
        .send_message(parent, child, "root", 1, "down")
        .unwrap();
    assert_eq!(store.messages_for(parent).unwrap().len(), 1);
    assert_eq!(store.messages_for(child).unwrap().len(), 1);

    store.delete_session(session).unwrap();

    // Counted straight off the table rather than through either run id, because a
    // row orphaned by a missed cascade is exactly a row no run id reaches.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let left: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0, "the mailbox is accounted for by the cascade");
}

/// **N5, the measurement half — what a drain costs at 1, 100 and 1,000 unread.**
///
/// The plan assertion lives in `src/state.rs`, where the connection is reachable;
/// this is the number that goes in the record. Ignored by default because it is a
/// measurement and not an assertion, in the shape 0.47.0 and 0.48.0 already use:
/// run it with `-- --ignored --nocapture`.
///
/// On disk, never in memory: the read commits a transaction, and an in-memory
/// database would measure SQLite's page cache instead of the WAL commit an agent
/// actually pays for.
#[test]
#[ignore = "measurement, not an assertion: run with --ignored --nocapture"]
fn n5_what_a_drain_costs() {
    const SAMPLES: usize = 20;
    for unread in [1usize, 100, 1_000] {
        let mut medians = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let (_dir, store, _path) = on_disk();
            let me = store.start_run("coordinate", "/repo").unwrap();
            let scout = store.start_run("scout", "/repo").unwrap();
            for i in 0..unread {
                store
                    .send_message(
                        scout,
                        me,
                        "scout",
                        i as u32 + 1,
                        "a finding of ordinary length",
                    )
                    .unwrap();
            }
            let t = std::time::Instant::now();
            let got = store.read_messages(me, None).unwrap();
            medians.push(t.elapsed().as_secs_f64() * 1_000.0);
            assert_eq!(got.len(), unread, "the measurement read what it seeded");
        }
        medians.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "mailbox drain unread={unread} median_ms={:.3}",
            medians[SAMPLES / 2]
        );
    }
}

/// A body is text and nothing more, so it survives being text: newlines, quotes and
/// non-ASCII come back byte for byte. Cheap, and it is the column an embedder will
/// put a JSON document in on the first day.
#[test]
fn a_body_survives_being_arbitrary_text() {
    let store = Store::memory().unwrap();
    let me = store.start_run("coordinate", "/repo").unwrap();
    let scout = store.start_run("scout", "/repo").unwrap();
    let body = "line one\nline \"two\"\n\tthird — ünïcode 漢字\n{\"json\": [1, 2]}";
    store.send_message(scout, me, "scout", 1, body).unwrap();
    assert_eq!(store.read_messages(me, None).unwrap()[0].body, body);
}
