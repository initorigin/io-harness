//! Who is driving a run, and what a driver that has lost it may still write
//! (0.62.0).
use super::*;

impl Store {
    /// This handle's opaque lease owner id (0.62.0).
    ///
    /// Two `Store` handles over one database file are two owners, whether or not
    /// they are in one process. Nothing parses this value; it is compared for
    /// equality and printed in a conflict.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Take the lease on a run, or be refused (0.62.0).
    ///
    /// Succeeds when the run has no lease, when this handle already holds it, when
    /// the holding lease has lapsed, or when the process that holds it is gone. The
    /// last three are *takeovers* and raise the generation by one, which is what
    /// makes the previous holder's next step commit refusable. Re-acquiring a lease
    /// this handle already holds keeps the generation, so a driver reconnecting to
    /// its own run does not invalidate the work it is in the middle of committing.
    ///
    /// Refused with [`Error::Conflict`] only while a different owner's lease is
    /// unlapsed **and** that owner's process is still running (see `owner_is_alive`,
    /// which errs towards "alive" and answers "alive" for everything it cannot
    /// check, including all of Windows). The error names the holder and when its
    /// lease lapses, so a caller can choose between backing off and waiting without
    /// parsing a message.
    ///
    /// **A dead owner's run is takeable at once, deliberately.** Waiting out the ttl
    /// after a `kill -9` would make this crate's oldest promise — a run survives the
    /// death of its driver and resumes immediately — wait half an hour.
    ///
    /// The decision and the write are one transaction, and the write is a
    /// compare-and-swap on the exact row the decision was made against, so two
    /// acquires cannot both land and the loser reads the winner's row back rather
    /// than reasoning about who was first. That is the shape [`crate::Attach`]'s
    /// three answer methods already use to resolve a race.
    ///
    /// ```
    /// use io_harness::{Error, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let dir = std::env::temp_dir().join("io-harness-doc-lease");
    /// std::fs::create_dir_all(&dir).unwrap();
    /// let path = dir.join("s.sqlite3");
    /// let _ = std::fs::remove_file(&path);
    ///
    /// let first = Store::open(&path)?;
    /// let run_id = first.start_run("port it", "openrouter")?;
    /// let held = first.acquire_lease(run_id, 300)?;
    ///
    /// // A second driver over the same store is refused, and told by whom.
    /// let second = Store::open(&path)?;
    /// match second.acquire_lease(run_id, 300) {
    ///     Err(Error::Conflict { owner, .. }) => assert_eq!(owner, first.owner()),
    ///     other => panic!("a live lease must refuse a second driver, got {other:?}"),
    /// }
    ///
    /// // Released — by hand here, on drop everywhere else — and it is free again.
    /// held.release()?;
    /// let taken = second.acquire_lease(run_id, 300)?;
    /// assert_eq!(taken.generation(), 1, "a released run is acquired, not taken over");
    /// # let _ = std::fs::remove_file(&path);
    /// # Ok(())
    /// # }
    /// ```
    pub fn acquire_lease(&self, run_id: i64, ttl_secs: i64) -> Result<Lease<'_>> {
        // One transaction around the whole decision: what is read here is what the
        // conditional write below is keyed on, so a takeover cannot be decided
        // against one row and land on another.
        let tx = self.conn.unchecked_transaction()?;
        let held = Self::read_lease(&tx, run_id)?;
        if let Some(row) = &held {
            let mine = row.owner == self.owner;
            // **A dead owner's lease is takeable at once, without waiting out the
            // ttl.** This is what keeps 0.7.0's promise intact: `kill -9` and
            // resume has always been this crate's headline, and a lease that made
            // a killed run unresumable for half an hour would have traded a silent
            // corruption for an outage — which the release says outright it must
            // not do. The ttl is the fallback for the case liveness cannot answer,
            // not the primary rule.
            if !mine && !row.expired && owner_is_alive(&row.owner) {
                tx.commit()?;
                return Err(conflict_from(run_id, held));
            }
        }
        let taken = match &held {
            // Free: an insert that lands only if nobody raced us to the row.
            None => tx.execute(
                "INSERT INTO run_leases (run_id, owner, generation, ttl_secs)
                 VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(run_id) DO NOTHING",
                (run_id, &self.owner, ttl_secs),
            )?,
            // Held by us, lapsed, or held by a process that is gone. The `WHERE`
            // is a compare-and-swap on the exact row the decision above was made
            // against, so a concurrent acquire cannot slip between the two.
            Some(row) => {
                let mine = row.owner == self.owner;
                tx.execute(
                    "UPDATE run_leases
                        SET owner       = ?2,
                            generation  = generation + ?4,
                            acquired_at = CASE WHEN ?4 = 0 THEN acquired_at
                                          ELSE strftime('%Y-%m-%dT%H:%M:%fZ','now') END,
                            renewed_at  = strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                            ttl_secs    = ?3
                      WHERE run_id = ?1 AND owner = ?5 AND generation = ?6",
                    (
                        run_id,
                        &self.owner,
                        ttl_secs,
                        i64::from(!mine),
                        &row.owner,
                        row.generation,
                    ),
                )?
            }
        };
        if taken == 0 {
            let held = Self::read_lease(&tx, run_id)?;
            tx.commit()?;
            return Err(conflict_from(run_id, held));
        }
        let generation: i64 = tx.query_row(
            "SELECT generation FROM run_leases WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        self.leases.borrow_mut().insert(run_id, generation);
        Ok(Lease {
            store: self,
            run_id,
            generation,
            released: std::cell::Cell::new(false),
        })
    }

    /// Extend a lease this handle holds at `generation`, keeping the generation.
    ///
    /// Refused with [`Error::Conflict`] once the run has been taken over — the
    /// generation moved, so this owner is no longer entitled to the run and
    /// renewing must not be a way back in.
    pub fn renew_lease(&self, run_id: i64, generation: i64) -> Result<()> {
        let renewed = self.conn.execute(
            "UPDATE run_leases
                SET renewed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE run_id = ?1 AND owner = ?2 AND generation = ?3",
            (run_id, &self.owner, generation),
        )?;
        if renewed == 0 {
            return Err(self.conflict_for(run_id)?);
        }
        Ok(())
    }

    /// Release a lease this handle holds at `generation`.
    ///
    /// Deleting the row rather than marking it expired is what makes "released"
    /// and "expired" two distinguishable states: a released run has no lease row
    /// at all, and an expired one has a row a takeover reads and increments.
    ///
    /// Releasing a lease this handle no longer holds is a no-op and not an error:
    /// the run has already moved on, and the drop that calls this must not turn a
    /// takeover somebody else performed correctly into a failure here.
    pub fn release_lease(&self, run_id: i64, generation: i64) -> Result<()> {
        // The handle stops enforcing before the row goes, not after: if the delete
        // fails, this handle has still given the run up, and going on checking a
        // generation it no longer claims would refuse its own later commits.
        if self.leases.borrow().get(&run_id) == Some(&generation) {
            self.leases.borrow_mut().remove(&run_id);
        }
        self.conn.execute(
            "DELETE FROM run_leases WHERE run_id = ?1 AND owner = ?2 AND generation = ?3",
            (run_id, &self.owner, generation),
        )?;
        Ok(())
    }

    /// Who holds a run right now, and whether that lease has lapsed (0.62.0).
    ///
    /// `None` when the run has no lease — it was never driven under one, or its
    /// driver released it. Expiry is evaluated by the database, against the same
    /// clock every acquire uses, so a caller never compares two clocks.
    pub fn run_lease(&self, run_id: i64) -> Result<Option<LeaseRow>> {
        Self::read_lease(&self.conn, run_id)
    }

    /// The shared read behind [`Self::run_lease`] and the conflict a refused write
    /// reports, so both see one row shape and one expiry rule.
    fn read_lease(conn: &Connection, run_id: i64) -> Result<Option<LeaseRow>> {
        let row = conn
            .query_row(
                &format!(
                    "SELECT owner, generation, acquired_at, renewed_at, ttl_secs,
                            ({LEASE_EXPIRES_AT}), ({LEASE_EXPIRED})
                       FROM run_leases WHERE run_id = ?1"
                ),
                [run_id],
                |r| {
                    Ok(LeaseRow {
                        run_id,
                        owner: r.get(0)?,
                        generation: r.get(1)?,
                        acquired_at: r.get(2)?,
                        renewed_at: r.get(3)?,
                        ttl_secs: r.get(4)?,
                        expires_at: r.get(5)?,
                        expired: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// The [`Error::Conflict`] describing whoever holds `run_id` now.
    pub(super) fn conflict_for(&self, run_id: i64) -> Result<Error> {
        Ok(conflict_from(run_id, Self::read_lease(&self.conn, run_id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **N4** — the lease lookup the step commit adds is a primary-key search, and
    /// the query plan says so rather than a comment saying so.
    ///
    /// The statements are the ones the crate runs, character for character: the
    /// read-back inside [`Store::acquire_lease`] and the generation check inside
    /// the step commit's transaction. `run_id` is `INTEGER PRIMARY KEY` and so a
    /// rowid alias, which is what makes both a `SEARCH`.
    ///
    /// The negative half matters as much: a `SCAN` of this table is a defect and
    /// not a slow path, because it would be paid once per step for the life of
    /// every run.
    #[test]
    fn the_lease_lookups_are_primary_key_searches_and_never_scans() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "NOTES.md").unwrap();
        let _held = store.acquire_lease(run, 3_600).unwrap();
        store.conn.execute_batch("ANALYZE").unwrap();

        // The parameters are bound rather than inlined: `EXPLAIN QUERY PLAN` still
        // *prepares* the statement, so a placeholder left unbound is a
        // `InvalidParameterCount` and an inlined literal would be explaining a
        // statement the crate does not run.
        let plan = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> String {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let rows = stmt
                .query_map(params, |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            rows.join(" | ")
        };

        let owner = store.owner().to_string();
        for (what, sql, params) in [
            (
                "the acquire's read-back",
                "SELECT generation FROM run_leases WHERE run_id = ?1",
                vec![&run as &dyn rusqlite::ToSql],
            ),
            (
                "the step commit's generation check",
                "SELECT generation FROM run_leases WHERE run_id = ?1 AND owner = ?2",
                vec![&run as &dyn rusqlite::ToSql, &owner],
            ),
        ] {
            let plan = plan(sql, &params);
            assert!(
                plan.contains("SEARCH") && plan.contains("run_leases"),
                "{what} must be a search: {plan}"
            );
            assert!(
                !plan.contains("SCAN"),
                "{what} must never be a scan: {plan}"
            );
        }
    }
    /// A ttl no test here can outlive. Never `1`: a one-second ttl races the second
    /// hand, which is the flake the whole lease suite is written to avoid.
    const LIVE: i64 = 3_600;

    /// **F5's other half, and the one that keeps 0.7.0's promise: a lease whose
    /// owner no longer exists is takeable at once, without waiting out its ttl.**
    ///
    /// This is the mechanism behind `tests/checkpoint.rs`'s real-SIGKILL test. A
    /// lease that made a killed run unresumable for half an hour would have traded
    /// a silent corruption for an outage, which is the one thing this release says
    /// it must not do.
    ///
    /// **The ttl here cannot lapse during the test**, so expiry is not what allows
    /// the takeover — only liveness can be. The dead owner is a real pid that is
    /// really gone: this process's own child, waited on so it is not left a zombie.
    /// Nothing is killed by signal, and nothing sleeps.
    #[test]
    #[cfg(unix)]
    fn a_lease_whose_owner_no_longer_exists_is_taken_over_without_waiting() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("store.db");
        let first = Store::open(&path).expect("a store");
        let second = Store::open(&path).expect("a second store over one file");
        let run = first.start_run("port it", "openrouter").expect("a run");

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("a short-lived child");
        let dead_pid = child.id();
        child.wait().expect("the child exits");

        // Written in the owner-id shape the crate itself writes: `<pid>-<nanos>-<seq>`.
        let dead_owner = format!("{dead_pid}-1-0");
        first
            .conn
            .execute(
                "INSERT INTO run_leases (run_id, owner, generation, ttl_secs)
                 VALUES (?1, ?2, 1, ?3)",
                (run, &dead_owner, LIVE),
            )
            .expect("a lease belonging to a dead process");
        let row = first
            .run_lease(run)
            .expect("a lease read")
            .expect("the dead owner's row");
        assert_eq!(row.owner, dead_owner);
        assert!(
            !row.expired,
            "the ttl must NOT be what allows this takeover, or the test proves nothing"
        );

        let taken = second
            .acquire_lease(run, LIVE)
            .expect("a lease whose owner is gone is takeable at once");
        assert_eq!(taken.generation(), 2, "a takeover, and exactly one of them");

        // The control, in the same test so the two cannot drift: under identical
        // conditions a LIVE owner's lease is still refused. This process is alive.
        let live_owner = format!("{}-1-0", std::process::id());
        let other = first.start_run("port it", "openrouter").expect("a run");
        first
            .conn
            .execute(
                "INSERT INTO run_leases (run_id, owner, generation, ttl_secs)
                 VALUES (?1, ?2, 1, ?3)",
                (other, &live_owner, LIVE),
            )
            .expect("a lease belonging to a living process");
        // The trailing semicolon is load-bearing: the `Result<Lease<'_>, _>` this
        // match scrutinises borrows `second`, and without it the temporary outlives
        // the store it borrows from.
        match second.acquire_lease(other, LIVE) {
            Err(Error::Conflict { owner, .. }) => assert_eq!(owner, live_owner),
            other => panic!("a living owner's lease must still be refused, got {other:?}"),
        };
    }
}
