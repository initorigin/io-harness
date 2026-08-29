//! A child agent in its own working tree (0.36.0).
//!
//! Every agent in a tree has shared one checkout since spawning existed, so two
//! children editing the same path were one overwriting the other — the
//! concurrency 0.32.0 bought was usable only for work that did not overlap.
//! `AgentDef::with_worktree` gives a child its own.
//!
//! The claim here is only worth something if the collision it removes is real,
//! so this file runs the collision as a control rather than describing it: the
//! identical tree with the flag off loses one child's write, and that is
//! asserted, not assumed.
//!
//! Every test needing a real `git` returns early when there is none, because git
//! is a runtime capability here and not a build dependency.

use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    resume_tree, run_tree, AgentDef, Agents, ApproveAll, Containment, Policy, Provider, Store,
    TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// A provider that answers by *which agent is asking* rather than by call order.
///
/// A counter-driven script cannot be used here: two children run concurrently,
/// so the order their requests arrive in is not the order they were spawned, and
/// a shared index would hand one child the other's steps. The goal text is in
/// `CompletionRequest::user`, which is what makes the answer a function of the
/// asker.
struct ByGoal {
    /// `(marker in the user turn, the steps to play for it)`.
    scripts: Vec<(String, Vec<Vec<ToolCall>>)>,
    /// How many turns each marker has had, so a child advances through its own
    /// script independently of the other's.
    at: Mutex<std::collections::HashMap<String, usize>>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl ByGoal {
    fn new(scripts: Vec<(&str, Vec<Vec<ToolCall>>)>) -> Self {
        Self {
            scripts: scripts
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            at: Mutex::new(std::collections::HashMap::new()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Provider for ByGoal {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req.user.clone());
        // The most specific marker wins, so a root goal that is a substring of a
        // child's does not steal its script.
        let hit = self
            .scripts
            .iter()
            .filter(|(k, _)| req.user.contains(k.as_str()))
            .max_by_key(|(k, _)| k.len());
        let Some((key, steps)) = hit else {
            return Ok(CompletionResponse::default());
        };
        let i = {
            let mut at = self.at.lock().unwrap();
            let n = at.entry(key.clone()).or_insert(0);
            let i = *n;
            *n += 1;
            i
        };
        Ok(CompletionResponse {
            tool_calls: steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git should be runnable once `have_git` said so")
}

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

/// A real repository with one commit on `main`.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::write(p.join("README.md"), "hello\n").unwrap();
    git(p, &["init", "--initial-branch=main"]);
    git(p, &["add", "README.md"]);
    git(p, &["commit", "-m", "first"]);
    dir
}

/// The one path both children are told to write, so their work overlaps by
/// construction.
const SHARED: &str = "OUT.md";

fn spawn_as(agent: &str, goal: &str, needle: &str) -> ToolCall {
    call(
        "spawn_agent",
        json!({
            "agent": agent,
            "goal": goal,
            "verify_file": SHARED,
            "verify_contains": needle,
            "max_steps": 6
        }),
    )
}

/// One child's turns: write the shared path, stage it, commit it.
fn child_steps(content: &str) -> Vec<Vec<ToolCall>> {
    vec![
        vec![call(
            "write_file",
            json!({ "path": SHARED, "content": content }),
        )],
        vec![call("git_add", json!({ "paths": [SHARED] }))],
        vec![call(
            "git_commit",
            json!({ "message": format!("{content} commit") }),
        )],
    ]
}

/// The whole tree: a root that fans out to two children of one definition in one
/// step, and the two children. `worktree` decides whether they share a checkout.
async fn fan_out(dir: &tempfile::TempDir, worktree: bool) -> (Store, i64) {
    let mut worker = AgentDef::new("worker");
    if worktree {
        worker = worker.with_worktree();
    }
    let contract = TaskContract::workspace("fan out over two files", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "hello".into(),
        })
        .with_max_steps(8)
        .with_agents(Agents::new().with(worker));

    let provider = ByGoal::new(vec![
        (
            "fan out over two files",
            vec![vec![
                spawn_as("worker", "write the alpha part", "alpha"),
                spawn_as("worker", "write the beta part", "beta"),
            ]],
        ),
        ("write the alpha part", child_steps("alpha")),
        ("write the beta part", child_steps("beta")),
    ]);

    let store = Store::open(dir.path().join("trace.db")).unwrap();
    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        // Two slots, so both children are admitted at once rather than one
        // waiting for the other — the concurrency the criterion names.
        &Containment::new(10, 4, 2, 10_000_000),
    )
    .await
    .unwrap();
    (store, result.run_id)
}

/// Every `.worktrees/*` directory that exists under the root.
fn worktrees(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let dir = root.join(".worktrees");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    out.sort();
    out
}

// ------------------------------------------------------------------- F3

/// F3 — two concurrent children of a `worktree = true` definition each get their
/// own checkout, and neither loses the other's write.
#[tokio::test]
async fn two_concurrent_children_with_their_own_worktrees_do_not_collide() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let (_store, _root) = fan_out(&dir, true).await;

    // Two worktrees, one per child, at derived paths under `.worktrees/`.
    let trees = worktrees(dir.path());
    assert_eq!(trees.len(), 2, "one worktree per child: {trees:?}");

    // Each holds its own content, and the two differ. This is the whole claim:
    // both writes survived.
    let mut contents: Vec<String> = trees
        .iter()
        .map(|t| std::fs::read_to_string(t.join(SHARED)).unwrap())
        .collect();
    contents.sort();
    assert_eq!(contents, vec!["alpha".to_string(), "beta".to_string()]);

    // The parent's own working tree holds neither: a child worked in its own
    // checkout, not in the tree it was spawned from.
    assert!(
        !dir.path().join(SHARED).exists(),
        "the parent's tree must not hold a child's file"
    );

    // Each child's branch carries its own commit, named for its own content.
    for t in &trees {
        let branch =
            String::from_utf8_lossy(&git(t, &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
                .trim()
                .to_string();
        assert!(branch.starts_with("worker-"), "{branch}");
        let log = String::from_utf8_lossy(&git(t, &["log", "--oneline", "-1"]).stdout).into_owned();
        let content = std::fs::read_to_string(t.join(SHARED)).unwrap();
        assert!(log.contains(&format!("{content} commit")), "{log}");
    }

    // The parent is still on the branch it started on.
    let head =
        String::from_utf8_lossy(&git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
            .trim()
            .to_string();
    assert_eq!(head, "main");
}

/// F3's control, executed rather than described: the identical tree with the flag
/// off. Both children write the same path in one checkout and one write is lost.
///
/// If this ever passes with two surviving contents, the test above is proving
/// nothing — the collision would not have been there to remove.
#[tokio::test]
async fn the_same_two_children_sharing_one_tree_lose_a_write() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let (_store, _root) = fan_out(&dir, false).await;

    assert!(
        worktrees(dir.path()).is_empty(),
        "the flag is off, so nothing is created"
    );

    // One file, in the parent's own tree, holding one of the two answers. The
    // other child's write is gone.
    let shared = std::fs::read_to_string(dir.path().join(SHARED)).unwrap();
    assert!(
        shared == "alpha" || shared == "beta",
        "one of the two won: {shared:?}"
    );
}

/// What the parent's own `git status` reports once its children's worktrees
/// exist. Measured rather than assumed, and carried into `docs/CONTRACT.md` so an
/// operator meets it in the documentation rather than in their repository.
#[tokio::test]
async fn a_parents_git_status_reports_the_worktree_directory_as_untracked() {
    if !have_git() {
        return;
    }
    let dir = repo();
    fan_out(&dir, true).await;

    let status = String::from_utf8_lossy(
        &git(
            dir.path(),
            &["status", "--porcelain=v1", "--untracked-files=normal"],
        )
        .stdout,
    )
    .into_owned();
    // One line, for the directory rather than for each worktree inside it: git
    // summarises an untracked directory and does not descend into it. The store
    // this fixture keeps beside the repository is the other untracked entry and
    // is not what this measures.
    let worktree_lines: Vec<&str> = status
        .lines()
        .filter(|l| l.contains(".worktrees"))
        .collect();
    assert_eq!(
        worktree_lines,
        ["?? .worktrees/"],
        "the parent sees one untracked directory, not its contents: {status:?}"
    );
}

/// A definition asking for a worktree where there is no repository does not
/// quietly fall back to sharing the parent's tree — the fallback is the
/// collision the flag exists to prevent, so the spawn fails and says why.
#[tokio::test]
async fn a_worktree_that_cannot_be_made_fails_the_spawn_instead_of_sharing_the_tree() {
    // Deliberately NOT a repository: a plain temporary directory.
    let dir = tempfile::tempdir().unwrap();
    let (_store, _root) = fan_out(&dir, true).await;

    assert!(
        worktrees(dir.path()).is_empty(),
        "nothing was created outside a repository"
    );
    assert!(
        !dir.path().join(SHARED).exists(),
        "no child wrote into the parent's tree, which is what a silent fallback \
         would have produced"
    );
}

// -------------------------------------------------------- 0.70.0 F7

/// 0.70.0 F7 — the run row of a `worktree = true` child names the directory that
/// child's files are actually in.
///
/// Checked **against the filesystem**, never against a recomputed path. The slug
/// is derived from the agent name, the parent run, the step and a digest of the
/// goal; deriving it again here would assert that this test can do the same
/// arithmetic as `worktree_for`, which is a different claim and a much weaker
/// one. So the `.worktrees/` entries are enumerated from disk, and what the store
/// recorded has to be that set — and each recorded path has to hold that child's
/// own write, read through the recorded path rather than through the enumerated
/// one.
///
/// Without this reader an operator asking where a child's work went had the
/// value in the row and no way to get it out, and the only alternative was to
/// re-derive a path from three things they would have had to dig out of the
/// trace first.
#[tokio::test]
async fn a_worktree_childs_run_row_names_the_directory_its_files_are_in() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let (store, root) = fan_out(&dir, true).await;

    // What git actually made, off the filesystem.
    let trees = worktrees(dir.path());
    assert_eq!(trees.len(), 2, "one worktree per child: {trees:?}");

    let children: Vec<i64> = store
        .runs()
        .expect("the runs")
        .into_iter()
        .filter(|r| store.parent(*r).expect("a parent") == Some(root))
        .collect();
    assert_eq!(children.len(), 2, "two children: {children:?}");

    let mut recorded: Vec<std::path::PathBuf> = children
        .iter()
        .map(|c| {
            std::path::PathBuf::from(
                store
                    .run_file(*c)
                    .expect("the read")
                    .expect("the child's run row"),
            )
        })
        .collect();
    recorded.sort();

    assert_eq!(
        recorded, trees,
        "the run rows name the worktrees on disk, not the parent's tree"
    );

    // And the recorded path is where that child's write landed. Read through
    // the value the store gave, so a row naming the wrong existing directory
    // fails here rather than passing on the set comparison alone.
    let mut contents: Vec<String> = recorded
        .iter()
        .map(|p| std::fs::read_to_string(p.join(SHARED)).expect("the child's own file"))
        .collect();
    contents.sort();
    assert_eq!(contents, vec!["alpha".to_string(), "beta".to_string()]);

    // The root records the tree's own root, which is the distinction the whole
    // reader exists to make: a child's directory is not its parent's.
    assert_eq!(
        store
            .run_file(root)
            .expect("the read")
            .expect("the root's run row"),
        dir.path().display().to_string()
    );
    assert!(
        !recorded.contains(&dir.path().to_path_buf()),
        "no child recorded the directory it was spawned from: {recorded:?}"
    );
}

// ------------------------------------------------------------------- F4

/// A provider that plays one script per goal and can be told to park forever on
/// one of a child's turns, so a tree can be cut off mid-child.
struct Parking {
    inner: ByGoal,
    /// Park on this turn index of any goal holding this marker.
    park_on: Option<(String, usize)>,
}

impl Provider for Parking {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if let Some((marker, turn)) = &self.park_on {
            if req.user.contains(marker.as_str()) {
                let seen = self
                    .inner
                    .at
                    .lock()
                    .unwrap()
                    .get(marker.as_str())
                    .copied()
                    .unwrap_or(0);
                if seen >= *turn {
                    // Longer than the timeout the test wraps this in. The task is
                    // dropped at the timeout, which is the cut-off.
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            }
        }
        self.inner.complete(req).await
    }
}

/// F4 — a resumed tree reuses the worktree it already made, with the files the
/// child had already written still in it.
///
/// The discriminating assertion is `EARLY.md`, written before the cut-off. A
/// resume that re-created the worktree would still let the child finish and would
/// still leave one directory behind — it would simply have thrown away the work.
#[tokio::test]
async fn a_resumed_child_continues_in_the_worktree_it_already_had() {
    if !have_git() {
        return;
    }
    let dir = repo();
    let db = dir.path().join("trace.db");

    let contract = TaskContract::workspace("fan out over two files", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "hello".into(),
        })
        .with_max_steps(8)
        .with_agents(Agents::new().with(AgentDef::new("worker").with_worktree()));

    // First attempt: the child writes EARLY.md, then parks. The tree is cut off.
    let store = Store::open(&db).unwrap();
    let parking = Parking {
        inner: ByGoal::new(vec![
            (
                "fan out over two files",
                vec![vec![spawn_as("worker", "write the alpha part", "alpha")]],
            ),
            (
                "write the alpha part",
                vec![vec![call(
                    "write_file",
                    json!({ "path": "EARLY.md", "content": "before the kill\n" }),
                )]],
            ),
        ]),
        park_on: Some(("write the alpha part".to_string(), 1)),
    };
    let cut_off = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_tree(
            &contract,
            &parking,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &Containment::new(10, 4, 2, 10_000_000),
        ),
    )
    .await;
    assert!(cut_off.is_err(), "the tree should have been cut off");
    drop(store);

    // The worktree exists and holds the child's first write.
    let before = worktrees(dir.path());
    assert_eq!(before.len(), 1, "one worktree so far: {before:?}");
    let wt = before[0].clone();
    assert!(
        wt.join("EARLY.md").is_file(),
        "the child wrote before the cut"
    );
    let branch_before =
        String::from_utf8_lossy(&git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
            .trim()
            .to_string();

    // Resume with a provider that lets the child finish.
    let store = Store::open(&db).unwrap();
    // The root's spawn step never committed, so the resumed root is asked for it
    // again and must re-issue it — that replay is what reaches `find_spawn`,
    // adopts the child, and puts this test on the adoption path at all.
    let finish = ByGoal::new(vec![
        (
            "fan out over two files",
            vec![vec![spawn_as("worker", "write the alpha part", "alpha")]],
        ),
        (
            "write the alpha part",
            vec![vec![call(
                "write_file",
                json!({ "path": SHARED, "content": "alpha" }),
            )]],
        ),
    ]);
    let resumed = resume_tree(
        &contract,
        &finish,
        &store,
        1,
        &Policy::permissive(),
        &ApproveAll,
        &Containment::new(10, 4, 2, 10_000_000),
    )
    .await
    .unwrap();
    let _ = resumed;

    // Still one worktree, at the same path, on the same branch: reused, not
    // re-created, and a second creation attempt did not error the spawn.
    let after = worktrees(dir.path());
    assert_eq!(after, before, "the same worktree, not a new one: {after:?}");
    assert_eq!(
        String::from_utf8_lossy(&git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).stdout).trim(),
        branch_before
    );

    // The discriminating assertion: the work from before the cut-off survived.
    assert_eq!(
        std::fs::read_to_string(wt.join("EARLY.md")).unwrap(),
        "before the kill\n",
        "a re-created worktree would have thrown this away"
    );
    // And the resumed child carried on in it.
    assert_eq!(std::fs::read_to_string(wt.join(SHARED)).unwrap(), "alpha");
}
