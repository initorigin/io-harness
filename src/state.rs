//! Run state in rusqlite: steps, intermediate results, and decisions, readable
//! back after the run for audit.

use rusqlite::Connection;

use crate::error::Result;

/// A persisted run store. Use [`Store::open`] for a file, or [`Store::memory`]
/// for an ephemeral in-memory database.
pub struct Store {
    conn: Connection,
}

/// One recorded loop step, as read back from the store.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRecord {
    /// 1-based step number within the run.
    pub step: u32,
    /// What the agent decided this step (e.g. "wrote file", "no tool call").
    pub decision: String,
    /// Intermediate result / model text for the step.
    pub result: String,
}

impl Store {
    /// Open (creating if absent) a store at `path` and ensure the schema exists.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::from_conn(Connection::open(path)?)
    }

    /// An in-memory store, for tests and throwaway runs.
    pub fn memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 goal    TEXT NOT NULL,
                 file    TEXT NOT NULL,
                 outcome TEXT
             );
             CREATE TABLE IF NOT EXISTS steps (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL REFERENCES runs(id),
                 step     INTEGER NOT NULL,
                 decision TEXT NOT NULL,
                 result   TEXT NOT NULL
             );",
        )?;
        Ok(Self { conn })
    }

    /// Start a run row; returns its id.
    pub fn start_run(&self, goal: &str, file: &str) -> Result<i64> {
        self.conn
            .execute("INSERT INTO runs (goal, file) VALUES (?1, ?2)", (goal, file))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record one step's decision and intermediate result.
    pub fn record_step(&self, run_id: i64, step: u32, decision: &str, result: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO steps (run_id, step, decision, result) VALUES (?1, ?2, ?3, ?4)",
            (run_id, step, decision, result),
        )?;
        Ok(())
    }

    /// Record the run's final outcome.
    pub fn finish_run(&self, run_id: i64, outcome: &str) -> Result<()> {
        self.conn
            .execute("UPDATE runs SET outcome = ?1 WHERE id = ?2", (outcome, run_id))?;
        Ok(())
    }

    /// Read every step of a run back, in order.
    pub fn steps(&self, run_id: i64) -> Result<Vec<StepRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, decision, result FROM steps WHERE run_id = ?1 ORDER BY step ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(StepRecord {
                step: r.get::<_, i64>(0)? as u32,
                decision: r.get(1)?,
                result: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_persist_and_read_back() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "out.txt").unwrap();
        store.record_step(run, 1, "wrote file", "content v1").unwrap();
        store.record_step(run, 2, "verified", "ok").unwrap();
        store.finish_run(run, "success").unwrap();

        let steps = store.steps(run).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].decision, "wrote file");
        assert_eq!(steps[1].result, "ok");
    }
}
