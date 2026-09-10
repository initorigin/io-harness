//! The same request twice is the same bytes (0.85.0).
//!
//! A prefix cache matches on bytes. Two requests that differ only in the order a
//! map serialised, or in whitespace a builder chose, are two prefixes to a vendor
//! and one request to a reader — which is the failure mode Manus reported: JSON
//! key ordering that varied with the host's locale, silently halving a hit rate
//! with nothing in the logs to show for it.
//!
//! This holds today. Nothing held it true, which is what this file is for.

use io_harness::provider::{CompletionRequest, Message, ToolSpec};
use serde_json::json;

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("does {name}"),
        parameters: json!({
            "type": "object",
            "properties": {
                "zeta": { "type": "string" },
                "alpha": { "type": "number" },
                "middle": { "type": "boolean" },
            },
        }),
    }
}

fn request(tools: Vec<ToolSpec>) -> CompletionRequest {
    CompletionRequest {
        system: "be brief".into(),
        user: "the whole prompt".into(),
        messages: vec![
            Message::User("the whole prompt".into()),
            Message::Assistant {
                text: Some("on it".into()),
                calls: Vec::new(),
            },
        ],
        tools,
        session_key: Some("io-deadbeef:0123456789abcdef".into()),
        ..Default::default()
    }
}

// ------------------------------------------------------------- F14: byte-identical

/// F14 — serialising one request twice yields identical bytes, and a request
/// whose tools arrive in a different order yields different bytes.
///
/// The second half is the point of the first. Sorting the catalogue behind the
/// caller's back would make this test pass and would be wrong: the order is the
/// contract's, a caller who reorders their tools has changed the head of the
/// prompt, and a crate that hid that would leave them wondering why a stable
/// session stopped hitting.
#[test]
fn f14_one_request_serialises_the_same_way_twice_and_order_is_the_callers() {
    let plain = request(vec![tool("read_file"), tool("write_file")]);

    let once = serde_json::to_string(&plain).unwrap();
    let twice = serde_json::to_string(&plain).unwrap();
    assert_eq!(once, twice, "the same request must serialise the same way");

    // And a schema built with its keys in another order is the same bytes. This
    // is the Manus failure exactly: a host whose map iterated in a different order
    // sent a different prefix for the same tools, halved its hit rate, and had
    // nothing in its logs to show for it. `serde_json`'s map is ordered, so the
    // rendering is a function of the keys and not of how the document was built.
    let shuffled = ToolSpec {
        name: "read_file".into(),
        description: "does read_file".into(),
        parameters: json!({
            "properties": {
                "alpha": { "type": "number" },
                "zeta": { "type": "string" },
                "middle": { "type": "boolean" },
            },
            "type": "object",
        }),
    };
    assert_eq!(
        serde_json::to_string(&shuffled).unwrap(),
        serde_json::to_string(&tool("read_file")).unwrap(),
        "one schema written two ways must reach the wire as one string"
    );

    let swapped = request(vec![tool("write_file"), tool("read_file")]);
    assert_ne!(
        serde_json::to_string(&swapped).unwrap(),
        once,
        "the catalogue's order is the caller's; a crate that sorted it would hide a \
         changed head instead of reporting one"
    );
}

/// F14 (the field half) — an absent option writes no key at all.
///
/// `skip_serializing_if` is what keeps a request that asks for nothing new byte-
/// identical to the one 0.84.0 sent. A field that serialised as `null` would be a
/// byte of difference on every request of every caller who never set it.
#[test]
fn an_unset_field_is_absent_rather_than_null() {
    let bare = CompletionRequest {
        system: "be brief".into(),
        user: "hello".into(),
        ..Default::default()
    };
    let json = serde_json::to_string(&bare).unwrap();
    for absent in [
        "session_key",
        "cache_boundary",
        "cache_through",
        "model",
        "web",
        "effort",
        "output_schema",
        "messages",
    ] {
        assert!(
            !json.contains(absent),
            "{absent} must be absent rather than null: {json}"
        );
    }
}
