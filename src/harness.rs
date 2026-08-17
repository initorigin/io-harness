//! The host, bound once.
//!
//! Every entry point in [`crate::run`] and every [`Session`] turn takes the same
//! four or five things — a provider, a store, a policy, an approver, and often an
//! observer — and most callers pass the same four or five every time. Beside
//! them, [`TaskContract`] carries ten settings that are properties of the *host*
//! rather than of a task: the toolbox, MCP and LSP servers, the browser config,
//! the skills directory, the plugin bundles, the agent roster, the responder and
//! web access. A caller with twenty tasks builds those ten twenty times, and every
//! one of them is a place they can be built differently by accident.
//!
//! [`Harness`] binds all of it once. Nothing else changes: the free functions and
//! the [`Session`] turn methods keep their exact signatures and keep working, and
//! a `Harness` call reaches the same loop by calling the same function a caller
//! would have called themselves.

use crate::approve::{ApproveAll, Approver};
use crate::containment::Containment;
use crate::error::Result;
use crate::observe::{Ignore, Observer};
use crate::policy::Policy;
use crate::provider::Provider;
use crate::run::RunResult;
use crate::session::{Session, TurnResult};
use crate::state::Store;
use crate::{TaskContract, Verification};
use std::path::{Path, PathBuf};

/// The default approver, so [`Harness::new`] has something with a `'static`
/// lifetime to point at rather than a temporary.
static APPROVE_ALL: ApproveAll = ApproveAll;

/// The default observer, for the reason [`APPROVE_ALL`] exists.
static IGNORE: Ignore = Ignore;

/// A provider, a store, a boundary and the host's own configuration, bound once
/// and used for as many runs as the program takes.
///
/// Borrowed rather than owned, deliberately: [`Store`] holds a
/// `rusqlite::Connection`, which is `Send` and not `Sync`, and every existing
/// entry point already takes these by reference — so borrowing is the shape that
/// composes with what a caller already holds. An owning variant can be added
/// later without breaking anyone; taking ownership away later could not be.
///
/// Generic over the provider because [`Provider::complete`] returns
/// `impl Future`, which makes the trait not dyn-compatible. There is no
/// `Box<dyn Provider>` to be had, and making the trait dyn-compatible to get one
/// would break every implementor.
///
/// ```no_run
/// use io_harness::{ApproveAll, Harness, OpenRouter, Policy, Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// let provider = OpenRouter::from_env()?;
/// let store = Store::open("runs.db")?;
/// let policy = Policy::default().layer("app").allow_read("*").allow_write("src/*");
///
/// // The host's own configuration, built once. Every `with_*` on `TaskContract`
/// // is available here, because the template *is* a `TaskContract`.
/// let host = TaskContract::workspace("", "/repo")
///     .with_skills("/repo/.io/skills")
///     .with_max_steps(40);
///
/// let harness = Harness::new(&provider, &store)
///     .with_policy(policy)
///     .with_approver(&ApproveAll)
///     .with_defaults(host);
///
/// // Two tasks, no host configuration re-supplied for either.
/// harness.run(&harness.workspace("bring the docs up to date", "/repo")).await?;
/// harness.run(&harness.task("make tests/parse.rs pass", "/repo", Verification::Command {
///     argv: vec!["cargo".into(), "test".into()],
///     expect_exit: 0,
/// })).await?;
/// # Ok(()) }
/// ```
pub struct Harness<'a, P: Provider> {
    provider: &'a P,
    store: &'a Store,
    policy: Policy,
    approver: &'a dyn Approver,
    observer: &'a dyn Observer,
    defaults: TaskContract,
}

impl<'a, P: Provider> Harness<'a, P> {
    /// Bind a provider and a store, with the defaults every unpoliced entry point
    /// already uses: [`Policy::permissive`], [`ApproveAll`] and [`Ignore`].
    ///
    /// The template contract starts as an empty workspace contract, so
    /// [`workspace`](Self::workspace) and [`task`](Self::task) produce exactly what
    /// [`TaskContract::workspace`] and [`TaskContract::new`] produce until
    /// [`with_defaults`](Self::with_defaults) is called.
    ///
    /// ```no_run
    /// use io_harness::{Harness, OpenRouter, Store};
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let provider = OpenRouter::from_env()?;
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&provider, &store);
    /// # let _ = harness; Ok(()) }
    /// ```
    pub fn new(provider: &'a P, store: &'a Store) -> Self {
        Self {
            provider,
            store,
            policy: Policy::permissive(),
            approver: &APPROVE_ALL,
            observer: &IGNORE,
            defaults: TaskContract::workspace("", ""),
        }
    }

    /// Bind the permission boundary every run through this harness is checked
    /// against.
    ///
    /// Taken by value rather than by reference: a `Policy` is a configuration
    /// value built once, and owning it means the harness does not tie its caller
    /// to keeping a separate binding alive.
    ///
    /// ```
    /// use io_harness::{Harness, Policy, Store};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store)
    ///     .with_policy(Policy::default().layer("ops").allow_read("*"));
    /// # let _ = harness; Ok(()) }
    /// ```
    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Bind who answers the grey tier — anything the policy marks
    /// [`Effect::Ask`](crate::Effect::Ask).
    ///
    /// ```
    /// use io_harness::{Harness, StdinApprover, Store};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store).with_approver(&StdinApprover);
    /// # let _ = harness; Ok(()) }
    /// ```
    pub fn with_approver(mut self, approver: &'a dyn Approver) -> Self {
        self.approver = approver;
        self
    }

    /// Bind the observer every run through this harness reports to.
    ///
    /// `&self` on [`Observer`], so one observer serves every run the harness
    /// drives and any tree beneath them; state inside it goes behind a `Mutex`.
    ///
    /// ```
    /// use io_harness::{Flow, Harness, Observer, RunEvent, Store};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// /// One line per event, for every run this harness drives.
    /// struct Trace;
    ///
    /// impl Observer for Trace {
    ///     fn event(&self, event: &RunEvent) -> Flow {
    ///         println!("run {} step {}", event.run_id, event.step);
    ///         Flow::Continue
    ///     }
    /// }
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store).with_observer(&Trace);
    /// # let _ = harness; Ok(()) }
    /// ```
    pub fn with_observer(mut self, observer: &'a dyn Observer) -> Self {
        self.observer = observer;
        self
    }

    /// Bind the host's own configuration as a template contract.
    ///
    /// Every setting on [`TaskContract`] that is a property of the host rather
    /// than of a task — the toolbox, MCP and LSP servers, the browser, the skills
    /// directory, the plugin bundles, the agent roster, the responder, web access,
    /// and the budgets if a caller wants them shared — is set here once, with the
    /// same `with_*` builders a contract already has. There is no second builder
    /// surface to learn and none to keep in step.
    ///
    /// The template's `goal`, `file` and `root` are overwritten by
    /// [`workspace`](Self::workspace) and [`task`](Self::task), so what is put
    /// there does not matter.
    ///
    /// **The template is never merged into a contract a caller passes to
    /// [`run`](Self::run).** A contract handed to the harness is used exactly as
    /// it was built. The alternative — filling in whatever a contract still holds
    /// at its default — cannot tell a caller who set a field to its default value
    /// from one who did not set it, and a rule a caller cannot evaluate at the call
    /// site is worse than typing the setting twice.
    ///
    /// ```
    /// use io_harness::{Harness, Store, TaskContract};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store)
    ///     .with_defaults(TaskContract::workspace("", "/repo").with_max_steps(40));
    ///
    /// // Both contracts carry the bound cap; neither restates it.
    /// assert_eq!(harness.workspace("one", "/repo").max_steps, 40);
    /// assert_eq!(harness.workspace("two", "/repo").max_steps, 40);
    /// # Ok(()) }
    /// ```
    pub fn with_defaults(mut self, defaults: TaskContract) -> Self {
        self.defaults = defaults;
        self
    }

    /// A workspace contract over the bound host configuration.
    ///
    /// The template with `goal`, `root` and `file` replaced — everything else is
    /// what [`with_defaults`](Self::with_defaults) bound.
    ///
    /// ```
    /// use io_harness::{Harness, Store, TaskContract};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store)
    ///     .with_defaults(TaskContract::workspace("", "").with_max_retries(5));
    /// let contract = harness.workspace("bring the docs up to date", "/repo");
    /// assert_eq!(contract.goal, "bring the docs up to date");
    /// assert_eq!(contract.max_retries, 5);
    /// # Ok(()) }
    /// ```
    pub fn workspace(&self, goal: impl Into<String>, root: impl Into<PathBuf>) -> TaskContract {
        let root = root.into();
        let mut contract = self.defaults.clone();
        contract.goal = goal.into();
        contract.file = root.clone();
        contract.root = Some(root);
        contract
    }

    /// A contract over the bound host configuration, with a verification.
    ///
    /// The counterpart to [`workspace`](Self::workspace) for a task that has a
    /// checkable definition of done.
    ///
    /// ```
    /// use io_harness::{Harness, Store, TaskContract, Verification};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store);
    /// let contract = harness.task("make the failing test pass", "/repo", Verification::Command {
    ///     argv: vec!["cargo".into(), "test".into()],
    ///     expect_exit: 0,
    /// });
    /// assert!(matches!(contract.verify, Verification::Command { .. }));
    /// # Ok(()) }
    /// ```
    pub fn task(
        &self,
        goal: impl Into<String>,
        root: impl Into<PathBuf>,
        verify: Verification,
    ) -> TaskContract {
        let mut contract = self.workspace(goal, root);
        contract.verify = verify;
        contract
    }

    /// Run `contract` against the bound provider, store, boundary and observer.
    ///
    /// The contract is used **verbatim**. This is
    /// [`run_with_observed`](crate::run_with_observed) with the harness's
    /// bindings, and it calls exactly that function — there is no second loop for
    /// a facade to diverge from.
    ///
    /// ```no_run
    /// use io_harness::{Harness, OpenRouter, Store};
    ///
    /// # async fn demo() -> io_harness::Result<()> {
    /// let provider = OpenRouter::from_env()?;
    /// let store = Store::open("runs.db")?;
    /// let harness = Harness::new(&provider, &store);
    /// let result = harness.run(&harness.workspace("tidy the imports", "/repo")).await?;
    /// println!("{:?}", result.outcome);
    /// # Ok(()) }
    /// ```
    pub async fn run(&self, contract: &TaskContract) -> Result<RunResult> {
        crate::run::run_with_observed(
            contract,
            self.provider,
            self.store,
            &self.policy,
            self.approver,
            self.observer,
        )
        .await
    }

    /// Continue an interrupted run under its original `run_id`.
    ///
    /// [`resume_with_observed`](crate::resume_with_observed) with the harness's
    /// bindings.
    ///
    /// ```no_run
    /// use io_harness::{Harness, OpenRouter, Store};
    ///
    /// # async fn demo(run_id: i64) -> io_harness::Result<()> {
    /// let provider = OpenRouter::from_env()?;
    /// let store = Store::open("runs.db")?;
    /// let harness = Harness::new(&provider, &store);
    /// harness.resume(&harness.workspace("tidy the imports", "/repo"), run_id).await?;
    /// # Ok(()) }
    /// ```
    pub async fn resume(&self, contract: &TaskContract, run_id: i64) -> Result<RunResult> {
        crate::run::resume_with_observed(
            contract,
            self.provider,
            self.store,
            run_id,
            &self.policy,
            self.approver,
            self.observer,
        )
        .await
    }

    /// Run `contract` as the root of an agent tree, bounded by `containment`.
    ///
    /// [`run_tree_observed`](crate::run_tree_observed) with the harness's
    /// bindings.
    ///
    /// ```no_run
    /// use io_harness::{Containment, Harness, OpenRouter, Store};
    ///
    /// # async fn demo() -> io_harness::Result<()> {
    /// let provider = OpenRouter::from_env()?;
    /// let store = Store::open("runs.db")?;
    /// let harness = Harness::new(&provider, &store);
    /// let containment = Containment::new(4, 2, 2, 100_000);
    /// harness
    ///     .run_tree(&harness.workspace("audit the crate", "/repo"), &containment)
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub async fn run_tree(
        &self,
        contract: &TaskContract,
        containment: &Containment,
    ) -> Result<RunResult> {
        crate::run::run_tree_observed(
            contract,
            self.provider,
            self.store,
            &self.policy,
            self.approver,
            containment,
            self.observer,
        )
        .await
    }

    /// Open a conversation over the bound store.
    ///
    /// [`Session::open`] against the harness's store, so a caller holding a
    /// harness does not also have to hold the store to start one.
    ///
    /// ```
    /// use io_harness::{Harness, Store};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store);
    /// let session = harness.session(std::env::temp_dir())?;
    /// # let _ = session; Ok(()) }
    /// ```
    pub fn session(&self, root: impl AsRef<Path>) -> Result<Session> {
        Session::open(self.store, root)
    }

    /// Take one turn in `session`, against the bound provider and boundary.
    ///
    /// [`Session::turn_observed`] with the harness's bindings. Unbounded in the
    /// verification sense, so the turn may decide it was conversation and answer.
    ///
    /// ```no_run
    /// use io_harness::{Harness, OpenRouter, Store};
    ///
    /// # async fn demo() -> io_harness::Result<()> {
    /// let provider = OpenRouter::from_env()?;
    /// let store = Store::open("runs.db")?;
    /// let harness = Harness::new(&provider, &store);
    /// let mut session = harness.session("/repo")?;
    /// let turn = harness.turn(&mut session, "what does this crate do?").await?;
    /// println!("{:?}", turn.outcome);
    /// # Ok(()) }
    /// ```
    pub async fn turn(&self, session: &mut Session, text: impl Into<String>) -> Result<TurnResult> {
        session
            .turn_observed(
                text,
                self.provider,
                self.store,
                &self.policy,
                self.approver,
                self.observer,
            )
            .await
    }

    /// Take one turn in `session` under a contract the caller shaped.
    ///
    /// [`Session::turn_bounded_observed`] with the harness's bindings. The
    /// contract is used **verbatim**, for the reason
    /// [`with_defaults`](Self::with_defaults) gives — build it with
    /// [`workspace`](Self::workspace) or [`task`](Self::task) to get the bound host
    /// configuration into it.
    ///
    /// ```no_run
    /// use io_harness::{Harness, OpenRouter, Store, Verification};
    ///
    /// # async fn demo() -> io_harness::Result<()> {
    /// let provider = OpenRouter::from_env()?;
    /// let store = Store::open("runs.db")?;
    /// let harness = Harness::new(&provider, &store);
    /// let mut session = harness.session("/repo")?;
    /// let contract = harness.task("fix the failing test", "/repo", Verification::Command {
    ///     argv: vec!["cargo".into(), "test".into()],
    ///     expect_exit: 0,
    /// });
    /// harness.turn_with(&mut session, &contract).await?;
    /// # Ok(()) }
    /// ```
    pub async fn turn_with(
        &self,
        session: &mut Session,
        contract: &TaskContract,
    ) -> Result<TurnResult> {
        session
            .turn_bounded_observed(
                contract,
                self.provider,
                self.store,
                &self.policy,
                self.approver,
                self.observer,
            )
            .await
    }

    /// The store this harness is bound to.
    ///
    /// So a caller can reach the trace — [`Store::run_summary`], the transcript,
    /// the retention methods — without keeping a second binding beside the
    /// harness.
    ///
    /// ```
    /// use io_harness::{Harness, Store};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store);
    /// assert!(harness.store().run_summary(1)?.is_none());
    /// # Ok(()) }
    /// ```
    pub fn store(&self) -> &Store {
        self.store
    }

    /// The boundary this harness is bound to.
    ///
    /// ```
    /// use io_harness::{Harness, Policy, Store};
    /// # use io_harness::{CompletionRequest, CompletionResponse, Provider};
    /// # struct Quiet;
    /// # impl Provider for Quiet {
    /// #     async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
    /// #         Ok(CompletionResponse::default())
    /// #     }
    /// # }
    /// # fn demo() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let harness = Harness::new(&Quiet, &store);
    /// assert!(harness.policy().is_permissive());
    /// # Ok(()) }
    /// ```
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
}
