//! record: moved out of `src/run.rs` in 0.63.0.
//!
//! Private machinery only. Every name re-exported from `src/lib.rs` stays
//! defined in the parent, because `docs/public-api.txt` records each one's
//! defining file and moving one would rewrite a line of the snapshot.

use super::*;

/// Record one file change and the lines it added and removed (0.18.0).
///
/// Swallowed on a store failure for the same reason a provider call is: an edit
/// that reached the disk is not undone by failing to write its bookkeeping row,
/// and turning the run into an error here would lose the work as well as the
/// row.
/// `file` is the whole file's text before and after, for the hunk, and is
/// deliberately not the pair the counts are measured from (0.51.0). An
/// `edit_file` measures the fragment it replaced — that is what its counts have
/// meant since 0.18.0 — and a hunk needs the file's own line numbers or it is
/// anchored to nothing. `None` when the previous contents could not be read, so
/// a diff is never taken against a file wrongly believed to be empty.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_edit(
    store: &Store,
    run_id: i64,
    step: u32,
    tool: &str,
    path: &str,
    before: &str,
    after: &str,
    file: Option<(&str, &str)>,
) {
    let mut edit = crate::state::Edit::measure(step, tool, path, before, after);
    if let Some((was, now)) = file {
        edit = edit.with_hunk(was, now);
    }
    if let Err(e) = store.record_edit(run_id, &edit) {
        tracing::warn!("could not record the edit to {path} at step {step}: {e}");
    }
}

/// What is in a file before a write, as both things the loop needs from one read
/// (0.28.0).
///
/// The `String` is the measurement half, for [`crate::state::Edit::measure`], and
/// is `""` for every case that is not readable text — exactly what the
/// `read_to_string(..).ok().unwrap_or_default()` this replaced produced, so no
/// line count changes. The [`Kept`] is the restore-point half, and is where the
/// cases the `String` cannot express go.
///
/// Those cases are the reason this exists rather than one `read_to_string`.
/// Reading a binary or unreadable file as an empty one is harmless for a line
/// count and is data loss for a rewind: the restore point would say "this file
/// was empty", and putting it back would truncate it.
///
/// One read, not two. A `metadata()` for the size followed by a read would be
/// two syscalls and a race — the file can change between them — where reading the
/// bytes and then measuring what came back cannot disagree with itself.
pub(super) fn read_before(ws: &Workspace, path: &str) -> (String, Kept) {
    // A path that does not resolve and a path that does not exist are the same
    // answer: there is nothing there, so putting it back means it should not be
    // there. A resolve failure is an escape attempt the write gate is about to
    // refuse anyway, so no restore point is lost by folding the two.
    let Ok(abs) = ws.resolve(path) else {
        return (String::new(), Kept::Absent);
    };
    let bytes = match std::fs::read(abs) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (String::new(), Kept::Absent),
        // Something is there and could not be read — a directory, a permission,
        // a device. Not `Absent`, deliberately: `Absent` means "putting this
        // back means deleting it", and deleting a path whose contents could not
        // even be read is the one outcome this feature must never produce.
        Err(e) => {
            return (
                String::new(),
                Kept::Unkept(format!("could not be read: {e}")),
            )
        }
    };
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return (
            String::new(),
            Kept::Unkept(format!(
                "{} bytes, over the 1 MiB snapshot cap",
                bytes.len()
            )),
        );
    }
    match String::from_utf8(bytes) {
        Ok(text) => (text.clone(), Kept::Text(text)),
        Err(_) => (String::new(), Kept::Unkept("not valid UTF-8".to_string())),
    }
}

/// Record what a file held before this run first wrote it (0.28.0).
///
/// Swallowed on a store failure for the same reason [`record_edit`] is: the write
/// has already reached the disk by the time this runs, and reporting it as failed
/// because a bookkeeping row would not land would lose the work as well as the
/// row. The cost of the warning is that the file has no restore point, which
/// [`rewind`] reports honestly as [`Rewind::NotRecorded`].
pub(super) fn record_snapshot(store: &Store, run_id: i64, step: u32, path: &str, kept: Kept) {
    let snap = Snapshot {
        step,
        path: path.to_string(),
        kept,
    };
    if let Err(e) = store.record_snapshot(run_id, &snap) {
        tracing::warn!("could not record the state of {path} before step {step}: {e}");
    }
}

/// Record one provider call, answered or failed (0.18.0).
///
/// A failed attempt is recorded too, and deliberately: a model that produced
/// tokens and then hit a broken connection was still billed for them, so a trace
/// that kept only the winning attempt would understate the money.
///
/// A store that cannot take the row is logged and swallowed. The alternative is
/// failing a run that the provider answered because the accounting could not be
/// written, which trades a real answer for a bookkeeping entry.
pub(super) fn record_provider_call(
    store: &Store,
    run_id: i64,
    step: u32,
    attempt: u32,
    provider: &str,
    latency_ms: u64,
    outcome: &Result<CompletionResponse>,
) {
    let call = crate::state::ProviderCall {
        step,
        attempt,
        provider: provider.to_string(),
        model: outcome.as_ref().ok().and_then(|r| r.model.clone()),
        usage: outcome.as_ref().ok().and_then(|r| r.usage),
        latency_ms,
        ttft_ms: outcome.as_ref().ok().and_then(|r| r.ttft_ms),
        finish_reason: outcome.as_ref().ok().and_then(|r| r.finish_reason.clone()),
        // The same short name the retry and escalation rows use, so the two
        // surfaces name one failure identically rather than nearly so.
        failure: outcome.as_ref().err().map(kind_of),
    };
    if let Err(e) = store.record_provider_call(run_id, &call) {
        tracing::warn!("could not record the provider call for step {step}: {e}");
    }
}

/// Record what the provider looked up while serving one call (0.22.0).
///
/// Citations and server-tool rows are written here, beside the `provider_calls`
/// row, because this is the only place that knows which attempt produced them —
/// and because a failed attempt that still ran a search was still billed for it.
///
/// A store that cannot take a row is logged and swallowed, exactly as the
/// accounting row is: failing a run the provider answered because a citation
/// could not be written trades a real answer for a bookkeeping entry.
pub(super) fn record_web_activity(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    step: u32,
    depth: u32,
    outcome: &Result<CompletionResponse>,
) {
    let Ok(response) = outcome else { return };
    if !response.citations.is_empty() {
        if let Err(e) = store.record_citations(run_id, step, &response.citations) {
            tracing::warn!("could not record the citations for step {step}: {e}");
        }
    }
    if response.server_tools.is_empty() {
        return;
    }
    if let Err(e) = store.record_server_tool_calls(run_id, step, &response.server_tools) {
        tracing::warn!("could not record the server-tool calls for step {step}: {e}");
    }
    for call in &response.server_tools {
        watch.emit(RunEvent::at_depth(
            run_id,
            step,
            depth,
            EventKind::ServerToolUsed {
                provider: call.provider.clone(),
                tool: call.tool.clone(),
                ok: call.succeeded(),
            },
        ));
    }
}

/// Whether this failure is worth another attempt. A non-provider error — a bad
/// configuration, an IO failure — is not: it will fail the same way next time.
pub(super) fn retryable(e: &Error) -> bool {
    matches!(e, Error::Provider { kind, .. } if kind.is_retryable())
}

/// What the server asked us to wait, if it asked.
pub(super) fn retry_after(e: &Error) -> Option<std::time::Duration> {
    match e {
        Error::Provider { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// A short name for the trace row, so a reader can tell a wait from a hammer.
pub(super) fn kind_of(e: &Error) -> String {
    match e {
        Error::Provider { kind, status, .. } => match status {
            Some(s) => format!("{kind:?} (HTTP {s})"),
            None => format!("{kind:?}"),
        },
        other => format!("{other}"),
    }
}

/// The outcome string an escalation records, carrying whether the failure was one
/// another attempt could have survived.
///
/// Two strings rather than one because a resumed run and a trace reader have to
/// reach the same conclusion the caller did, and the caller's `Error` does not
/// survive into the store.
pub(super) fn escalation_outcome(e: &Error) -> &'static str {
    if retryable(e) {
        "escalated_retryable"
    } else {
        "escalated_terminal"
    }
}
