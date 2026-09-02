//! What a run cost, and what a corpus of runs adds up to (0.62.0 split).
use super::*;

impl Store {
    /// Record one call to a provider (0.18.0).
    ///
    /// Called once per attempt, by the run loop, for a call that answered and
    /// for one that failed alike. See [`ProviderCall`] for why the failures are
    /// kept.
    pub fn record_provider_call(&self, run_id: i64, call: &ProviderCall) -> Result<()> {
        let u = call.usage;
        self.conn.execute(
            "INSERT INTO provider_calls
                 (run_id, step, attempt, provider, model, prompt_tokens, completion_tokens,
                  total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                  server_tool_requests, latency_ms, ttft_ms, finish_reason, failure)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            rusqlite::params![
                run_id,
                call.step,
                call.attempt,
                &call.provider,
                &call.model,
                u.map(|u| u.prompt_tokens),
                u.map(|u| u.completion_tokens),
                u.map(|u| u.total_tokens),
                u.map(|u| u.cache_read_tokens),
                // (0.75.0) `and_then`, not `map`: the column has always been
                // nullable and now the counter is too, so a wire that reports no
                // cache write writes SQL NULL rather than a zero the reader
                // cannot tell from a measured one.
                u.and_then(|u| u.cache_write_tokens),
                u.map(|u| u.reasoning_tokens),
                u.map(|u| u.server_tool_requests),
                call.latency_ms,
                call.ttft_ms,
                &call.finish_reason,
                &call.failure,
            ],
        )?;
        Ok(())
    }

    /// Every provider call recorded for a run, in the order they were made.
    ///
    /// A run that predates 0.18.0 has no rows, and this returns an empty vector
    /// rather than zeros — an unrecorded run and a free one are different facts.
    pub fn provider_calls(&self, run_id: i64) -> Result<Vec<ProviderCall>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, attempt, provider, model, prompt_tokens, completion_tokens,
                    total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    server_tool_requests, latency_ms, ttft_ms, finish_reason, failure
             FROM provider_calls WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            // `total_tokens` decides whether the provider reported anything at
            // all: a NULL there is the `None` the caller stored, and reading it
            // back as a zeroed `Usage` would turn "unknown" into "free".
            let total: Option<u64> = r.get(6)?;
            Ok(ProviderCall {
                step: r.get(0)?,
                attempt: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                usage: match total {
                    Some(total_tokens) => Some(Usage {
                        prompt_tokens: r.get::<_, Option<u64>>(4)?.unwrap_or(0),
                        completion_tokens: r.get::<_, Option<u64>>(5)?.unwrap_or(0),
                        total_tokens,
                        cache_read_tokens: r.get::<_, Option<u64>>(7)?.unwrap_or(0),
                        // (0.75.0) Read straight through rather than collapsed:
                        // a NULL here is a wire that reports no cache write, and
                        // every row written before this release is a NULL for a
                        // reason nobody recorded — which is the same "unknown",
                        // not a measured zero.
                        cache_write_tokens: r.get::<_, Option<u64>>(8)?,
                        reasoning_tokens: r.get::<_, Option<u64>>(9)?.unwrap_or(0),
                        server_tool_requests: r.get::<_, Option<u64>>(10)?.unwrap_or(0),
                    }),
                    None => None,
                },
                latency_ms: r.get(11)?,
                ttft_ms: r.get(12)?,
                finish_reason: r.get(13)?,
                failure: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every provider call in the store, with the run and the day it belongs to.
    ///
    /// The grouped views are built from this one read: pricing is arithmetic the
    /// database cannot do, so the rows come back and the grouping happens in
    /// Rust rather than in half-SQL that would still need a second pass.
    fn all_provider_calls(&self) -> Result<Vec<(i64, String, ProviderCall)>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, date(at), step, attempt, provider, model, prompt_tokens,
                    completion_tokens, total_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, server_tool_requests, latency_ms, ttft_ms, finish_reason,
                    failure
             FROM provider_calls ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            let total: Option<u64> = r.get(8)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                ProviderCall {
                    step: r.get(2)?,
                    attempt: r.get(3)?,
                    provider: r.get(4)?,
                    model: r.get(5)?,
                    usage: match total {
                        Some(total_tokens) => Some(Usage {
                            prompt_tokens: r.get::<_, Option<u64>>(6)?.unwrap_or(0),
                            completion_tokens: r.get::<_, Option<u64>>(7)?.unwrap_or(0),
                            total_tokens,
                            cache_read_tokens: r.get::<_, Option<u64>>(9)?.unwrap_or(0),
                            // As above: NULL is unknown, not zero.
                            cache_write_tokens: r.get::<_, Option<u64>>(10)?,
                            reasoning_tokens: r.get::<_, Option<u64>>(11)?.unwrap_or(0),
                            server_tool_requests: r.get::<_, Option<u64>>(12)?.unwrap_or(0),
                        }),
                        None => None,
                    },
                    latency_ms: r.get(13)?,
                    ttft_ms: r.get(14)?,
                    finish_reason: r.get(15)?,
                    failure: r.get(16)?,
                },
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Spend grouped by the model that served each call, priced by `prices`
    /// (0.18.0).
    ///
    /// Calls whose provider named no model group under `"(unknown model)"` and
    /// are counted in [`Spend::unpriced_calls`], because attributing them to
    /// anything else would be a guess.
    ///
    /// ```
    /// use io_harness::pricing::{Price, PriceTable};
    /// use io_harness::{ProviderCall, Store, Usage};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// # let store = Store::memory()?;
    /// # let run_id = store.start_run("goal", "NOTES.md")?;
    /// # store.record_provider_call(run_id, &ProviderCall {
    /// #     step: 1, provider: "anthropic".into(), model: Some("m".into()),
    /// #     usage: Some(Usage { prompt_tokens: 1_000_000, total_tokens: 1_000_000,
    /// #                         ..Default::default() }), ..Default::default() })?;
    /// let cheap = PriceTable::new("2026-07-29").with("m", Price { input: 1_000_000, ..Price::ZERO });
    /// let dear = PriceTable::new("2026-07-29").with("m", Price { input: 2_000_000, ..Price::ZERO });
    ///
    /// // The same unchanged trace, two price tables, two answers — which is what
    /// // "correcting a price repairs the whole history" means in practice.
    /// assert_eq!(store.spend_by_model(&cheap)?[0].cost_micros, 1_000_000);
    /// assert_eq!(store.spend_by_model(&dear)?[0].cost_micros, 2_000_000);
    /// # Ok(())
    /// # }
    /// ```
    pub fn spend_by_model(&self, prices: &PriceTable) -> Result<Vec<Spend>> {
        self.grouped(prices, |_, _, call| {
            Some(call.model.clone().unwrap_or_else(|| UNKNOWN_MODEL.into()))
        })
    }

    /// Spend grouped by day (`YYYY-MM-DD`, UTC, from the database clock), priced
    /// by `prices` (0.18.0).
    pub fn spend_by_day(&self, prices: &PriceTable) -> Result<Vec<Spend>> {
        self.grouped(prices, |_, day, _| Some(day.to_string()))
    }

    /// Spend grouped by run id, priced by `prices` (0.18.0).
    pub fn spend_by_run(&self, prices: &PriceTable) -> Result<Vec<Spend>> {
        self.grouped(prices, |run_id, _, _| Some(run_id.to_string()))
    }

    /// Spend grouped by session id, priced by `prices` (0.75.0).
    ///
    /// A durable conversation is this crate's headline shape and until now its
    /// cost was reachable only by folding [`Store::spend_by_run`] over the turns
    /// by hand: every turn of a session is its own run, and the session tables
    /// carry no token columns. This joins the two, so "what did this conversation
    /// cost, and how much of it was served from cache" is one read.
    ///
    /// A run that belongs to no session is **absent** rather than grouped under a
    /// sentinel. That is the difference from [`Store::spend_by_model`], where an
    /// unknown model is still a call somebody paid for and must stay visible: a
    /// one-shot run is not an unattributed session, it is not a session at all,
    /// and inventing a group for it would make the sum of the sessions disagree
    /// with the sum of the runs for a reason no reader could see.
    ///
    /// ```
    /// use io_harness::pricing::{Price, PriceTable};
    /// use io_harness::{ProviderCall, Store, Usage};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// # let store = Store::memory()?;
    /// # let session_id = store.create_session("/tmp")?;
    /// # let run_id = store.start_run("goal", "NOTES.md")?;
    /// # store.record_turn(session_id, None, run_id, "hello")?;
    /// # store.record_provider_call(run_id, &ProviderCall {
    /// #     step: 1, provider: "anthropic".into(), model: Some("m".into()),
    /// #     usage: Some(Usage { prompt_tokens: 1_000_000, total_tokens: 1_000_000,
    /// #                         cache_read_tokens: 750_000, ..Default::default() }),
    /// #     ..Default::default() })?;
    /// let prices = PriceTable::new("2026-09-02").with("m", Price { input: 1_000_000, ..Price::ZERO });
    ///
    /// let spend = store.spend_by_session(&prices)?;
    /// assert_eq!(spend[0].key, session_id.to_string());
    /// // Three quarters of the conversation's prompt came off the cache.
    /// assert_eq!(spend[0].cache_hit_rate(), Some(0.75));
    /// # Ok(())
    /// # }
    /// ```
    pub fn spend_by_session(&self, prices: &PriceTable) -> Result<Vec<Spend>> {
        // Read the run-to-session map once rather than asking per call: a session
        // of forty turns is forty runs, and a lookup per provider call would be a
        // query per attempt.
        let mut stmt = self
            .conn
            .prepare("SELECT run_id, session_id FROM session_turns")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let sessions: std::collections::HashMap<i64, i64> =
            rows.collect::<std::result::Result<_, _>>()?;
        self.grouped(prices, |run_id, _, _| {
            sessions.get(&run_id).map(|id| id.to_string())
        })
    }

    /// The shared body of the groupings: read once, key by `key`, sum and price
    /// each group. Rows come back ordered by key, which is the only ordering
    /// promised.
    ///
    /// (0.75.0) `key` returns `Option`, and a `None` **drops** the call rather
    /// than bucketing it. Only [`Store::spend_by_session`] uses that: the other
    /// three key every call there is, and a grouping that silently omitted one
    /// would be a floor pretending to be a total.
    fn grouped(
        &self,
        prices: &PriceTable,
        key: impl Fn(i64, &str, &ProviderCall) -> Option<String>,
    ) -> Result<Vec<Spend>> {
        let calls = self.all_provider_calls()?;
        let mut groups: std::collections::BTreeMap<String, Vec<&ProviderCall>> =
            std::collections::BTreeMap::new();
        for (run_id, day, call) in &calls {
            let Some(k) = key(*run_id, day, call) else {
                continue;
            };
            groups.entry(k).or_default().push(call);
        }
        Ok(groups
            .into_iter()
            .map(|(k, calls)| crate::pricing::group(k, &calls, prices))
            .collect())
    }

    // ---- 0.30.0: outcome, gate and recovery aggregates ----
    //
    // The shape `src/pricing.rs` established in 0.18.0 and this release holds to
    // without exception: grouped rows out, no rendering, no derived opinion. What
    // is different from the spend groupings is where the work happens — those read
    // every call row and group in Rust because the price table is a Rust value the
    // SQL cannot see, and these have no such excuse, so each is one indexed
    // `GROUP BY` and stays flat as the trace grows.

    /// Finished runs grouped by the outcome they ended with (0.30.0).
    ///
    /// The raw outcome strings, not a success/failure collapse: "ran out of
    /// steps", "stalled" and "a human refused" are different endings and the
    /// distinction is the reason [`RunSummary`] keeps both the string and the
    /// flag. Rows come back ordered by outcome, which is the only ordering
    /// promised.
    ///
    /// A run that has not finished is not here — it has no ending yet — and a run
    /// that crashed mid-loop never reached one at all.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// for outcome in ["success", "success", "stalled"] {
    ///     let run = store.start_run("goal", "/repo")?;
    ///     store.finish_run(run, outcome)?;
    /// }
    ///
    /// let tally = store.runs_by_outcome()?;
    /// assert_eq!(tally[0].key, "stalled");
    /// assert_eq!(tally[0].count, 1);
    /// assert_eq!(tally[1].key, "success");
    /// assert_eq!(tally[1].count, 2);
    /// # Ok(())
    /// # }
    /// ```
    pub fn runs_by_outcome(&self) -> Result<Vec<Tally>> {
        self.tally("SELECT outcome, COUNT(*) FROM run_outcomes GROUP BY outcome ORDER BY outcome")
    }

    /// Finished runs grouped by the day they finished (`YYYY-MM-DD`, UTC, from
    /// the database clock) (0.30.0).
    ///
    /// The same clock `spend_by_day` groups on, so a cost row and an outcome row
    /// for one day describe the same day rather than two that can disagree.
    ///
    /// ```
    /// use io_harness::Store;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("goal", "/repo")?;
    /// store.finish_run(run, "success")?;
    ///
    /// let days = store.runs_by_day()?;
    /// assert_eq!(days.len(), 1, "one run, one day");
    /// assert_eq!(days[0].count, 1);
    /// assert_eq!(days[0].key.len(), 10, "YYYY-MM-DD");
    /// # Ok(())
    /// # }
    /// ```
    pub fn runs_by_day(&self) -> Result<Vec<Tally>> {
        self.tally(
            "SELECT date(finished_at), COUNT(*) FROM run_outcomes
             GROUP BY date(finished_at) ORDER BY date(finished_at)",
        )
    }

    /// How often a run was verified without a gate ever failing first (0.30.0).
    ///
    /// Three counts rather than a rate, because the denominator is a judgement
    /// the consumer makes: *first_try / succeeded* is "when we got there, how
    /// often first time", *first_try / runs* is "how often does this work at all
    /// on the first attempt", and both are legitimate. Returning one number would
    /// be picking for them and hiding which was picked.
    ///
    /// "First try" means finished successfully with no `gate_phase_failed` event
    /// recorded against the run. A run whose gate never ran at all — a contract
    /// with [`Verification::None`](crate::Verification::None) — counts as first
    /// try, because it is a run that succeeded with nothing failing.
    ///
    /// ```
    /// use io_harness::{SandboxEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let clean = store.start_run("goal", "/repo")?;
    /// store.finish_run(clean, "success")?;
    ///
    /// let retried = store.start_run("goal", "/repo")?;
    /// store.record_sandbox_event(&SandboxEvent::gate_phase_failed(retried, 2, "test-run"))?;
    /// store.finish_run(retried, "success")?;
    ///
    /// let first = store.first_try()?;
    /// assert_eq!((first.runs, first.succeeded, first.first_try), (2, 2, 1));
    /// # Ok(())
    /// # }
    /// ```
    pub fn first_try(&self) -> Result<FirstTry> {
        // A `NOT EXISTS` correlated per finished run, and measured against the
        // alternatives rather than assumed: a LEFT JOIN onto a DISTINCT subquery
        // reads far worse (25s at 20,000 runs against 7.6ms here), because the
        // subquery is materialised without an index and every outcome row then
        // probes it linearly. The correlated form probes
        // `sandbox_events (run_id, kind)` instead, which is one index seek per run.
        Ok(self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(success), 0),
                    COALESCE(SUM(success = 1 AND NOT EXISTS (
                        SELECT 1 FROM sandbox_events e
                        WHERE e.run_id = run_outcomes.run_id
                          AND e.kind = 'gate_phase_failed')), 0)
             FROM run_outcomes",
            [],
            |r| {
                Ok(FirstTry {
                    runs: r.get(0)?,
                    succeeded: r.get(1)?,
                    first_try: r.get(2)?,
                })
            },
        )?)
    }

    /// Failed verification gates grouped by the phase that failed (0.30.0).
    ///
    /// The phase, not the criterion's text: `"compile"`, `"criterion-compile"`,
    /// `"test-run"` are what the gate records, and reporting them as criteria
    /// would be dressing up a column as something it is not. `criterion-compile`
    /// is the one to look for — see
    /// [`SandboxEvent::gate_phase_failed`](SandboxEvent::gate_phase_failed).
    ///
    /// Counted per event, so a run that failed the same phase three times is
    /// three. "How many *runs* failed this phase" is a different question and is
    /// deliberately not answered here rather than answered ambiguously.
    ///
    /// ```
    /// use io_harness::{SandboxEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("goal", "/repo")?;
    /// store.record_sandbox_event(&SandboxEvent::gate_phase_failed(run, 1, "test-run"))?;
    /// store.record_sandbox_event(&SandboxEvent::gate_phase_failed(run, 4, "test-run"))?;
    ///
    /// let failures = store.gate_failures_by_phase()?;
    /// assert_eq!(failures[0].key, "test-run");
    /// assert_eq!(failures[0].count, 2, "per failure, not per run");
    /// # Ok(())
    /// # }
    /// ```
    pub fn gate_failures_by_phase(&self) -> Result<Vec<Tally>> {
        // Grouped on `detail` itself rather than on a `COALESCE` of it: a function
        // in the GROUP BY makes the (kind, detail) index unusable and SQLite falls
        // back to a scan plus a temp B-tree. The NULL is handled where it costs
        // nothing, in `tally`.
        self.tally(
            "SELECT detail, COUNT(*) FROM sandbox_events
             WHERE kind = 'gate_phase_failed'
             GROUP BY detail ORDER BY detail",
        )
    }

    /// How many runs a recovery mechanism carried through something (0.30.0).
    ///
    /// Three counts, and deliberately not a fourth. An **escalation** is recorded
    /// nowhere as an event and is in any case the opposite of a rescue — it is
    /// the run handing the problem back — so it is neither counted here nor
    /// smuggled into the total. An aggregate that cannot be computed honestly is
    /// worse than a missing one; `Spend::unpriced_calls` is the precedent.
    ///
    /// ```
    /// use io_harness::{CheckpointEvent, ContextEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run = store.start_run("goal", "/repo")?;
    /// store.record_context_event(run, &ContextEvent::served(1, "anthropic"))?;
    /// store.record_context_event(run, &ContextEvent::replan(3, "no progress"))?;
    /// store.record_checkpoint_event(&CheckpointEvent::resume(run, 4, "after a crash"))?;
    ///
    /// let recovery = store.recovery()?;
    /// assert_eq!((recovery.fallbacks, recovery.replans, recovery.resumes), (1, 1, 1));
    /// # Ok(())
    /// # }
    /// ```
    pub fn recovery(&self) -> Result<Recovery> {
        let count = |sql: &str| -> Result<u64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        Ok(Recovery {
            // `served` is written only when a `Fallback` moved off its first
            // provider, so the row's existence *is* the fallback.
            fallbacks: count("SELECT COUNT(*) FROM context_events WHERE kind = 'served'")?,
            replans: count("SELECT COUNT(*) FROM context_events WHERE kind = 'replan'")?,
            resumes: count("SELECT COUNT(*) FROM checkpoint_events WHERE kind = 'resume'")?,
        })
    }

    /// The shared body of the three groupings that are one `GROUP BY`: run it,
    /// read `(key, count)`. One place, so a caller cannot get a differently
    /// shaped row from one of them.
    fn tally(&self, sql: &str) -> Result<Vec<Tally>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(Tally {
                // A NULL group key is a row the trace holds with nothing to name
                // it by. `(none)` says that; dropping the row would quietly lose
                // a count, and inventing a name would be worse.
                key: r
                    .get::<_, Option<String>>(0)?
                    .unwrap_or_else(|| "(none)".into()),
                count: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Real wall-clock seconds elapsed since the run's `started_at`, from the
    /// database clock — so a budget over duration counts time that passed while
    /// the process was down, not just this process's uptime. Zero if the run has
    /// no start stamp (a pre-0.7.0 run).
    pub fn elapsed_secs(&self, run_id: i64) -> Result<f64> {
        let secs: Option<f64> = self.conn.query_row(
            "SELECT (julianday('now') - julianday(started_at)) * 86400.0
             FROM runs WHERE id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(secs.unwrap_or(0.0).max(0.0))
    }

    /// Total tokens recorded across this run's steps — the durable spend, so a
    /// resume restores the token budget instead of restarting it at zero.
    ///
    /// A turn that answered rather than ran (0.37.0) has no `steps` row to sum, so
    /// its spend is read from the one `provider_calls` row its single completion
    /// wrote. Branching on `turn_kind` rather than on "this run has no steps": a
    /// run killed inside its first step also has no step row, and reading its
    /// attempts as a total would change what every pre-0.37.0 caller was told about
    /// an interrupted run.
    pub fn spent_tokens(&self, run_id: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT CASE
                      WHEN (SELECT turn_kind FROM runs WHERE id = ?1) = 'reply'
                      THEN (SELECT COALESCE(SUM(total_tokens), 0)
                            FROM provider_calls WHERE run_id = ?1)
                      ELSE (SELECT COALESCE(SUM(tokens), 0)
                            FROM steps WHERE run_id = ?1)
                    END",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spent_tokens_and_elapsed_are_durable_reads() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(1, "a", "ok").with_trace("p", "t", 30))
            .unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(2, "a", "ok").with_trace("p", "t", 12))
            .unwrap();
        assert_eq!(store.spent_tokens(run).unwrap(), 42);
        assert!(store.elapsed_secs(run).unwrap() >= 0.0);
    }
}
