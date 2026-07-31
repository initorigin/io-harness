//! A live run over a real spreadsheet, under a policy.
//!
//! ```text
//! cargo run --example documents_live --features documents           # both runs
//! cargo run --example documents_live --features documents positive  # just the edit
//! cargo run --example documents_live --features documents control   # just the refusal
//! ```
//!
//! The workspace holds one workbook, `ledger.xlsx`, written by this example with
//! the same [`xlsx::write_new`] the agent's `xlsx_write` tool calls — a genuine
//! zip of OOXML, not a text file wearing an extension. The agent has to read it,
//! total a column, and put the answer in one cell of the workbook that is already
//! there.
//!
//! # What each run is evidence for, and what it is not
//!
//! **Run 1 (positive).** The gate is
//! [`Verification::DocumentContains`], which reads the workbook's *extracted*
//! text. That is the criterion this release added, and it is the only one that
//! can pass on a document at all — [`Verification::WorkspaceFileContains`] reads
//! with `read_to_string(..).unwrap_or_default()`, so on a zip it reads the empty
//! string and answers "does not contain" for every workbook ever written.
//!
//! The gate on its own is weak in a way worth naming: `DocumentContains` puts its
//! needle in the criterion the model is shown, so the total is not a secret, and
//! an agent could satisfy it by calling `xlsx_write` to *replace* the workbook
//! with a single cell holding that number. So the example checks two more things
//! afterwards, in Rust, that no prompt can talk its way past: whether the other
//! rows survived, and whether the number landed in the cell it was asked for. A
//! passing gate with a wrecked workbook is a result this example will print as
//! exactly that.
//!
//! **Run 2 (control).** The same contract under a policy that denies writing the
//! workbook. It is the negative control for the release's actual claim: a
//! document tool is a built-in dispatched on the path the model named, so
//! `deny_write` stops `xlsx_set_cell` in the same place it stops `write_file`.
//! Without this arm, run 1 shows only that the tools work — not that anything
//! governs them. The bytes are hashed before and after, because "the policy
//! refused" and "the file is unchanged" are two claims and only the second one
//! matters to whoever owns the file.

use std::path::Path;

use io_harness::tools::documents::xlsx;
use io_harness::tools::Workspace;
use io_harness::{run_with, ApproveAll, Policy, RunResult, Store, TaskContract, Verification};

/// The workbook, relative to the workspace root.
const BOOK: &str = "ledger.xlsx";
/// The one sheet in it.
const SHEET: &str = "Q3";
/// Where the agent is asked to put the total: the Tonnage column of the `TOTAL`
/// row [`sheet_rows`] lays out. Named in the goal and checked afterwards, because
/// the gate can only see *whether* the number is in the workbook, never where.
const TOTAL_CELL: &str = "C12";

/// The ledger. Amounts that no model has memorised and that do not round, so a
/// total arrived at by guessing is not a total.
const ROWS: &[(&str, &str, u32)] = &[
    ("Jul", "Kattegat", 41_293),
    ("Jul", "Skagerrak", 17_884),
    ("Jul", "Oresund", 9_442),
    ("Aug", "Kattegat", 38_017),
    ("Aug", "Skagerrak", 22_651),
    ("Aug", "Oresund", 11_309),
    ("Sep", "Kattegat", 44_760),
    ("Sep", "Skagerrak", 19_223),
    ("Sep", "Oresund", 13_875),
];

/// Sum of the amount column, computed here so the goal, the gate and the report
/// cannot drift from the data the workbook actually holds.
fn total() -> u32 {
    ROWS.iter().map(|(_, _, amount)| amount).sum()
}

/// The workbook as `write_new` wants it: a header row, the data, then a labelled
/// row with the total cell left empty. Empty because a cell the agent must fill
/// is a different task from a cell it must correct, and this example is about the
/// first one.
fn sheet_rows() -> Vec<Vec<String>> {
    let mut rows = vec![vec![
        "Month".to_string(),
        "Berth".to_string(),
        "Tonnage".to_string(),
    ]];
    rows.extend(ROWS.iter().map(|(month, berth, amount)| {
        vec![month.to_string(), berth.to_string(), amount.to_string()]
    }));
    rows.push(vec![String::new(), String::new(), String::new()]);
    rows.push(vec!["TOTAL".to_string(), String::new(), String::new()]);
    rows
}

/// A workspace holding the workbook and nothing else.
fn workspace() -> io_harness::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    xlsx::write_new(&Workspace::new(dir.path()), BOOK, SHEET, &sheet_rows())?;
    Ok(dir)
}

/// What the model is asked to do. It names the tool for the edit on purpose: the
/// question this run answers is whether a live model can drive the document tools
/// against a real workbook under a policy, not whether it can guess which of
/// twelve new tool names to reach for.
fn goal() -> String {
    format!(
        "The workspace holds a spreadsheet {BOOK} with one sheet named {SHEET:?}. \
         Read it, add up every value in the Tonnage column, and write that total \
         into cell {TOTAL_CELL} using the xlsx_set_cell tool, which keeps the rest \
         of the workbook as it is. Write the number on its own, with no thousands \
         separator, no currency symbol and no formula. Do not create a new \
         workbook and do not change any other cell."
    )
}

/// The bytes of the workbook, as a cheap fingerprint. FNV-1a: the control arm
/// needs to answer "did this file change", which does not need a real hash.
fn fingerprint(root: &Path) -> u64 {
    let bytes = std::fs::read(root.join(BOOK)).unwrap_or_default();
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x1_0000_01b3)
    })
}

fn report(store: &Store, result: &RunResult) -> io_harness::Result<()> {
    println!("  outcome: {:?}", result.outcome);
    for step in store.steps(result.run_id)? {
        let call = step.tool_call.replace('\n', " ");
        println!(
            "  step {}: {} | {}",
            step.step,
            step.decision,
            &call[..call.len().min(160)]
        );
    }
    for e in store
        .events(result.run_id)?
        .iter()
        .filter(|e| e.kind == "refusal")
    {
        println!(
            "  refused: {} {} (rule {})",
            e.act,
            e.target,
            e.rule.clone().unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

/// One cell out of what [`xlsx::read_sheet`] rendered: a tab-separated grid whose
/// first line is the column letters and whose every other line starts with its
/// row number.
///
/// Single-letter columns only, which is all this sheet has. A general A1 parser
/// belongs in the crate if anything ever needs one, not in an example that reads
/// column C.
fn cell_of(text: &str, cell: &str) -> Option<String> {
    let column = cell.chars().next()?;
    let row = cell[1..].parse::<usize>().ok()?;
    let index = (column as u8).checked_sub(b'A')? as usize;
    text.lines()
        .find(|l| l.split('\t').next() == Some(row.to_string().as_str()))?
        .split('\t')
        .nth(index + 1)
        .map(str::to_string)
}

/// The two checks the gate cannot make. The first is whether the workbook is
/// still the workbook; the second is whether the number is where it was asked
/// for.
fn inspect(root: &Path) -> io_harness::Result<()> {
    let ws = Workspace::new(root);
    let text = xlsx::read_sheet(&ws, BOOK, Some(SHEET))?;
    let total = total().to_string();

    let survived = ROWS
        .iter()
        .filter(|(_, berth, amount)| text.contains(berth) && text.contains(&amount.to_string()))
        .count();
    println!(
        "  original rows still readable: {survived}/{} {}",
        ROWS.len(),
        if survived == ROWS.len() {
            "(the edit preserved the workbook)"
        } else {
            "(the workbook was rewritten, not edited — the gate would still pass)"
        }
    );
    println!(
        "  the total {total} is somewhere in the sheet: {}",
        text.contains(&total)
    );
    println!(
        "  and it is in {TOTAL_CELL}, the cell it was asked for: {}",
        cell_of(&text, TOTAL_CELL).as_deref() == Some(total.as_str())
    );
    println!("  sheet as the model would read it back:");
    for line in text.lines() {
        println!("    {line}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let only = std::env::args().nth(1).unwrap_or_default();
    let provider = io_harness::OpenRouter::from_env()?;
    let total = total().to_string();

    println!(
        "workbook: {BOOK}, sheet {SHEET:?}, {} data rows",
        ROWS.len()
    );
    println!("total the agent has to arrive at: {total}");
    println!("cell it has to land in: {TOTAL_CELL}");

    if only != "control" {
        println!("\n=== run 1: edit the workbook, under a policy that allows it ===");
        let dir = workspace()?;
        let contract = TaskContract::workspace(goal(), dir.path())
            .with_verification(Verification::DocumentContains {
                file: BOOK.into(),
                needle: total.clone(),
            })
            .with_max_steps(6)
            .with_token_budget(200_000);

        let policy = Policy::default()
            .layer("app")
            .allow_read("*")
            .allow_write("*");

        let store = Store::open(dir.path().join("runs.db"))?;
        let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
        report(&store, &result)?;
        inspect(dir.path())?;
    }

    if only != "positive" {
        println!("\n=== run 2: the same contract, with the workbook denied to writes ===");
        let dir = workspace()?;
        let before = fingerprint(dir.path());
        let contract = TaskContract::workspace(goal(), dir.path())
            .with_verification(Verification::DocumentContains {
                file: BOOK.into(),
                needle: total,
            })
            .with_max_steps(4)
            .with_token_budget(200_000);

        // Read is still allowed: a run that could not open the workbook would
        // stop for the wrong reason and prove nothing about the write boundary.
        let policy = Policy::default()
            .layer("app")
            .allow_read("*")
            .allow_write("*")
            .deny_write(BOOK);

        let store = Store::open(dir.path().join("runs.db"))?;
        let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
        report(&store, &result)?;
        let after = fingerprint(dir.path());
        println!(
            "  workbook bytes unchanged: {} ({before:016x} -> {after:016x})",
            before == after
        );
    }
    Ok(())
}
