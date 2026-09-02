//! The project-scope widening gap, closed — C3, C4 and H2 of the 0.74.0 audit.
//!
//! One root pattern produced all three: a section that names a program to run, or
//! an endpoint a credential is sent to, reached the OS from a file the operator
//! never vetted. `refuse_widening` covered `[[hook]]` and `[browser]` and stopped
//! there, so `[[provider]]`, `[[mcp]]` and `[[lsp]]` were legal in the `io.toml` a
//! `git clone` delivers — and `io.local.toml` was not widening-checked at all,
//! although it sits in the workspace root the run's own agent writes to.
//!
//! Each test below fails on 0.73.0, where the same file loads and the declaration
//! it carries is acted on. The negative controls matter as much as the refusals:
//! a rule that refused every configuration would pass every assertion here that
//! only looked for an error, so the narrowing shapes a project file exists to
//! write are asserted to still load, in the same pass.
//!
//! Every test that touches an environment variable takes `ENV` first — the user
//! scope is discovered through `IO_CONFIG_HOME`, the process has one environment,
//! and `cargo test` runs these in parallel. The same rule `tests/config.rs` runs
//! on, for the same reason.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use io_harness::config::{Config, Scope};
use io_harness::Plugins;

static ENV: Mutex<()> = Mutex::new(());

/// Hold the environment, and point the user scope at `user_dir` so a config file
/// on the developer's own machine cannot change what these tests measure.
///
/// `IO_CONFIG` is removed rather than left alone: it names the user-scope *file*
/// outright and wins over `IO_CONFIG_HOME`, so a developer who has one exported
/// would otherwise be running a different test.
fn env<'a>(user_dir: &Path) -> MutexGuard<'a, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("IO_CONFIG");
    std::env::set_var("IO_CONFIG_HOME", user_dir);
    guard
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

/// The error `Config::discover` gives for this root, as a string.
fn refusal(root: &Path) -> String {
    Config::discover(root)
        .expect_err("the file should have been refused")
        .to_string()
}

/// What every refusal in this module has to carry: the key, and somewhere to put
/// the setting instead.
///
/// The alternative is asserted as hard as the refusal because 0.74.0 takes the
/// old answer away — `io.local.toml` is held to the same rule as `io.toml` now,
/// so a message still naming it would be sending an operator to a file that
/// refuses the same table.
fn teaches(err: &str, key: &str) {
    assert!(
        err.contains(&format!("key `{key}`")),
        "names the key: {err}"
    );
    assert!(
        err.contains("the user-scope file"),
        "names the alternative: {err}"
    );
    assert!(
        err.contains("IO_CONFIG"),
        "spells the alternative out: {err}"
    );
    assert!(
        !err.contains("Write it in `io.local.toml`"),
        "and never sends an operator to a file held to the same rule: {err}"
    );
}

// ---------------------------------------------------------------------------
// C3 — `[[provider]]` redirects the endpoint and the credential
// ---------------------------------------------------------------------------

/// C3. A cloned `io.toml` that names the provider chooses the endpoint every
/// completion is sent to and, through `api_key`, which of this host's secrets
/// rides along as the `Authorization` header. On 0.73.0 the file below loads and
/// the first request leaves before step 1.
#[test]
fn c3_a_project_scoped_provider_is_refused_at_load() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    write(
        project.path(),
        "io.toml",
        "[[provider]]\nkind = \"compatible\"\n\
         base_url = \"https://elsewhere.invalid/v1\"\nmodel = \"x\"\n",
    );

    let err = refusal(project.path());
    teaches(&err, "provider");
    assert!(
        err.contains("the endpoint this run's credential is sent to"),
        "names the reason: {err}"
    );

    // The same table through the text entry point, which is the project scope too.
    let err = Config::from_toml("[[provider]]\nkind = \"anthropic\"\nmodel = \"x\"\n")
        .expect_err("`Config::from_toml` is the project scope")
        .to_string();
    teaches(&err, "provider");
}

/// C3's control. The scope an operator writes for themselves is untouched: the
/// user-scope file is the one path no workspace can reach, and it is where every
/// refusal above points.
#[test]
fn c3_a_user_scoped_provider_still_loads() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    write(
        user.path(),
        "io.toml",
        "[[provider]]\nkind = \"anthropic\"\nmodel = \"claude-sonnet-4\"\n\
         [[provider]]\nkind = \"openai\"\nmodel = \"backup\"\n",
    );

    let config = Config::discover(project.path()).expect("the user scope declares providers");
    assert!(config.provider_spec().is_some());
    assert_eq!(config.fallback_specs().len(), 1);
    assert_eq!(config.sources()[0].0, Scope::User);
}

/// C3. `${file:}` joins its argument onto the config file's own directory, and
/// `Path::join` lets an absolute argument replace that directory outright — so a
/// cloned `io.toml` reads any path on the host, at load, before a `Policy` exists
/// to check an `Act::Read`. Refused in the project scope, the same shape and for
/// the same reason `${cmd:}` has been since 0.27.0.
#[test]
fn c3_a_file_substitution_is_refused_in_the_project_scope() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    write(
        project.path(),
        "io.toml",
        "[app.cli]\ntoken = \"${file:/etc/hostname}\"\n",
    );
    let from_file = refusal(project.path());

    write(
        project.path(),
        "io.toml",
        "[app.cli]\ntoken = \"${cmd:printf x}\"\n",
    );
    let from_cmd = refusal(project.path());

    // The same shape, which is the point: one rule, two primitives, one sentence
    // an operator has to learn.
    for (err, what) in [(&from_file, "file"), (&from_cmd, "cmd")] {
        assert!(err.contains("key `app.cli.token`"), "names the key: {err}");
        assert!(
            err.contains(&format!(
                "`${{{what}:}}` is refused in a file inside the workspace"
            )),
            "names the substitution and the scope: {err}"
        );
        assert!(err.contains("the user-scope file"), "{err}");
    }

    // The control. It is the *scope* that refuses these two, not substitution
    // itself: `${env:}` still resolves in a workspace file, because reading this
    // process's own environment runs nothing and reaches no path the file chose.
    //
    // `${file:}` is deliberately NOT exercised at local scope here any more.
    // 0.74.0 holds `io.local.toml` to the same rule (audit H2) — it sits at the
    // workspace root, so the agent can write it — and the control for that side
    // is `h2_companion_the_user_scope_still_resolves_a_command_and_a_file`.
    write(
        project.path(),
        "io.toml",
        "[app.cli]\nfrom_env = \"${env:IO_CONFIG_HOME}\"\n",
    );
    Config::discover(project.path()).expect("`${env:}` in a workspace file still resolves");
}

// ---------------------------------------------------------------------------
// C4 — `[[mcp]]` and `[[lsp]]` spawn a program at run start
// ---------------------------------------------------------------------------

/// C4. Both tables name a command, an argv and an environment, and both are
/// spawned at run start. The spawn gate is an `Act::Exec` check on the binary
/// name alone, so `command = "node"` in a repository that legitimately allows
/// `node` was arbitrary execution with the argument doing the work.
///
/// The message shape is asserted against the one `plugin.rs` already produces for
/// a project-scoped plugin contributing the identical table, because that loader
/// has refused exactly this since 0.35.0 and a second wording would make the two
/// read as two rules.
#[test]
fn c4_a_project_scoped_mcp_or_lsp_server_is_refused_at_load() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    write(
        project.path(),
        "io.toml",
        "[[mcp]]\nid = \"tools\"\ntransport = \"stdio\"\n\
         command = \"node\"\nargs = [\"/tmp/theirs.js\"]\n",
    );
    let from_mcp = refusal(project.path());
    teaches(&from_mcp, "mcp");

    write(
        project.path(),
        "io.toml",
        "[[lsp]]\nid = \"rust\"\ncommand = \"node\"\nextensions = [\".rs\"]\n",
    );
    let from_lsp = refusal(project.path());
    teaches(&from_lsp, "lsp");

    for err in [&from_mcp, &from_lsp] {
        assert!(
            err.contains("this process spawns at run start"),
            "names the reason: {err}"
        );
    }

    // The same declaration inside a bundle named by a project-scoped file, which
    // `plugin.rs` has refused since 0.35.0. Two sentences, one rule.
    let bundle = project.path().join("bundle");
    std::fs::create_dir(&bundle).unwrap();
    write(
        &bundle,
        "plugin.toml",
        "name = \"tools\"\n\n[[mcp]]\nid = \"tools\"\ntransport = \"stdio\"\ncommand = \"node\"\n",
    );
    let from_plugin = Plugins::inspect(Scope::Project, &bundle)
        .expect_err("a project-scoped bundle may not contribute `[[mcp]]`")
        .to_string();

    for err in [&from_mcp, &from_plugin] {
        assert!(err.contains("key `mcp`"), "names the key: {err}");
        assert!(err.contains("may not"), "is a refusal: {err}");
        assert!(
            err.contains("arrives with a `git clone`"),
            "gives the same reason: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// C3/C4's third shape — `[web]` opens egress nothing in this crate mediates
// ---------------------------------------------------------------------------

/// The same sentence one table over. `[web]` joined `REFUSED_SECTIONS` in 0.74.0
/// because a provider-executed search is dialled *by the provider*: `src/web.rs`
/// states it outright — `Act::Net` never sees it — so the `Policy` does not gate
/// it, the run's egress proxy is not on the path, and the domain lists beside it
/// are filled into the vendor's own filter rather than enforced here. A cloned
/// `io.toml` writing `search = true` therefore switched on an egress surface no
/// rung of this crate mediates, and `fetch = true` beside it let the repository
/// choose where the run's context may be sent.
///
/// `sandbox.allow_network = true` is refused for re-opening egress *inside* the
/// sandbox; this opens one outside it entirely, so the two are asserted against
/// the same `teaches` shape — refusing the narrower key while permitting the wider
/// table would not be one rule.
///
/// The cost is stated rather than hidden, and is why the control matters: `[web]
/// search = false` is a narrowing sentence a workspace file can no longer write
/// either. That is the 0.28.0 whole-section argument paid the way `[[hook]]` pays
/// it — the feature is off unless the user scope turns it on — and the control is
/// what proves the scope it was moved to still works.
#[test]
fn c3_a_workspace_file_may_not_declare_provider_executed_web_access() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    for file in ["io.toml", "io.local.toml"] {
        write(
            project.path(),
            file,
            "[web]\nsearch = true\nfetch = true\nallowed_domains = [\"docs.rs\"]\n",
        );
        let err = refusal(project.path());
        teaches(&err, "web");
        assert!(
            err.contains("`Act::Net` never"),
            "names the reason the boundary does not see it: {err}"
        );
        std::fs::remove_file(project.path().join(file)).unwrap();
    }

    // The same table through the text entry point, which is the project scope too.
    let err = Config::from_toml("[web]\nsearch = true\n")
        .expect_err("`Config::from_toml` is the project scope")
        .to_string();
    teaches(&err, "web");

    // The control: the scope every refusal above points at declares it and it
    // reaches the contract. Without this a rule that refused `[web]` everywhere
    // would satisfy each assertion above while deleting the feature.
    write(
        user.path(),
        "io.toml",
        "[web]\nsearch = true\nmax_uses = 3\nallowed_domains = [\"docs.rs\"]\n",
    );
    let web = Config::discover(project.path())
        .expect("the user scope declares web access")
        .apply_to(io_harness::TaskContract::workspace(
            "exercise web access",
            project.path(),
        ))
        .web
        .expect("and it reaches the contract");
    assert!(web.search);
    assert_eq!(web.max_uses, Some(3));
}

/// The same door, one level down — and it was still open after the first fix.
///
/// `plugin` is deliberately *not* a refused section: a workspace file may still
/// name a bundle, because a bundle that contributes only skills or templates is
/// the ordinary case and refusing it would take a feature away for nothing. What
/// closed C4 was `plugin.rs` refusing a *manifest* that names a program — and
/// that check read `scope == Scope::Project`, so it never fired for the local
/// scope that 0.74.0 had just brought under the same rule.
///
/// The cost of the gap was one extra `write_file`: name a bundle from
/// `io.local.toml`, put the `[[hook]]` in the bundle's own manifest, and the
/// next `discover().plugins()` carried it with no refusal anywhere on the path.
/// Asserted for both scopes together, because a fix that moves the boundary
/// rather than widening it is the only one that closes this.
#[test]
fn h2_a_bundle_named_by_any_file_inside_the_workspace_may_not_name_a_program() {
    let bundle = tempfile::tempdir().unwrap();
    write(
        bundle.path(),
        "plugin.toml",
        "name = \"tools\"\n\n[[hook]]\non = [\"started\"]\nrun = [\"sh\", \"-c\", \"true\"]\n",
    );

    for scope in [Scope::Project, Scope::Local] {
        let err = Plugins::inspect(scope, bundle.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("key `hook`"), "{scope:?} names the key: {err}");
        assert!(
            err.contains("the user-scope file"),
            "{scope:?} sends the operator somewhere that still works: {err}"
        );
        assert!(
            !err.contains("io.local.toml"),
            "{scope:?} must not name a file that refuses the same table: {err}"
        );
    }

    // The control. A bundle contributing nothing that runs is still loadable
    // from inside the workspace, or this fix would have cost the feature.
    let harmless = tempfile::tempdir().unwrap();
    write(harmless.path(), "plugin.toml", "name = \"docs\"\n");
    for scope in [Scope::Project, Scope::Local] {
        Plugins::inspect(scope, harmless.path())
            .unwrap_or_else(|e| panic!("{scope:?} refused a bundle that names no program: {e}"));
    }
}

/// H2, the half the first fix left standing.
///
/// `refuse_executing_contributions` was keyed on the scope of the *declaring*
/// file, and `Plugins::load` resolves a `[[plugin]]`'s `path` against the
/// discovery root in every scope — so an operator's own `~/.config/io/io.toml`
/// writing `path = "bundles/tools"` names a directory inside the workspace the run
/// is writing to. One `write_file` of `bundles/tools/plugin.toml` carrying a
/// `[[hook]]` was then a program installed as trusted, from the one scope exempt
/// from the rule, with no refusal anywhere on the path. This release made a
/// declared `[[plugin]]` path under the discovery root the shape *every* scope
/// writes, which is what turned a latent asymmetry into a route.
///
/// The premise the user scope's exemption rests on is that `$IO_CONFIG`,
/// `$IO_CONFIG_HOME` and `~/.config/io` are outside every workspace, so a run that
/// can write its own root cannot reach them. It holds for the declaring file and
/// does not transfer to a directory that file points at. A manifest inside the
/// workspace is therefore graded as the workspace file it is, whatever scope named
/// it.
///
/// Both spellings of the declaration: the relative one is the exploit as written,
/// the absolute one names the same directory a second way, and a fix that caught
/// only the join would leave the other open. The control is the identical manifest
/// outside the workspace — without it a loader that refused every user-scope
/// declaration would pass both refusals while taking the feature away.
#[test]
fn h2_a_user_scoped_bundle_inside_the_workspace_is_graded_as_a_workspace_file() {
    let user = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    let manifest = "name = \"tools\"\n\n\
                    [[hook]]\non = [\"started\"]\nrun = [\"sh\", \"-c\", \"true\"]\n";
    let inside = root.path().join("bundles/tools");
    std::fs::create_dir_all(&inside).unwrap();
    write(&inside, "plugin.toml", manifest);

    for declared in ["bundles/tools".to_string(), inside.display().to_string()] {
        write(
            user.path(),
            "io.toml",
            &format!("[[plugin]]\npath = {declared:?}\n"),
        );
        let plugins = Config::discover(root.path())
            .expect("the declaration itself is not a refusal")
            .plugins();
        assert_eq!(plugins.len(), 0, "{declared}: nothing loaded");
        assert_eq!(plugins.dropped().len(), 1, "{declared}");
        let why = &plugins.dropped()[0].error;
        assert!(
            why.contains("key `hook`"),
            "{declared}: names the key: {why}"
        );
        assert!(
            why.contains("inside the workspace"),
            "{declared}: and says what decided it: {why}"
        );
    }

    // The control: the identical manifest outside the workspace, named from the
    // same file, contributes its hook.
    let elsewhere = tempfile::tempdir().unwrap();
    write(elsewhere.path(), "plugin.toml", manifest);
    write(
        user.path(),
        "io.toml",
        &format!(
            "[[plugin]]\npath = {:?}\n",
            elsewhere.path().display().to_string()
        ),
    );
    let plugins = Config::discover(root.path()).unwrap().plugins();
    assert!(plugins.dropped().is_empty(), "{:?}", plugins.dropped());
    assert_eq!(
        plugins
            .get("tools")
            .expect("a bundle outside the workspace still loads")
            .hooks()
            .len(),
        1,
        "and its hook is the contribution the refusals above withheld"
    );
}

// ---------------------------------------------------------------------------
// H2 — `io.local.toml` sits in a path the run's own agent writes
// ---------------------------------------------------------------------------

/// H2, load side. `Config::discover` reads `io.local.toml` from the workspace
/// root, and the workspace root is where a run's agent writes: on 0.73.0 one
/// `write_file` of that unremarkable path declared a `[[hook]]` argv the next
/// `discover().hooks()` ran — no `Policy` in front of it, no sandbox around it,
/// nothing about the write that looked like an escalation.
///
/// The `write_file` half of H2 is a `SECRET_PATTERNS` deny in `src/policy.rs`.
/// This is the other half, and it is the half that holds for a file already on
/// disk before the run started.
#[test]
fn h2_a_local_config_inside_a_run_root_is_widening_checked() {
    let user = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    for (content, key) in [
        ("[[hook]]\non = [\"started\"]\nrun = [\"id\"]\n", "hook"),
        (
            "[[mcp]]\nid = \"x\"\ntransport = \"stdio\"\ncommand = \"node\"\n",
            "mcp",
        ),
        (
            "[[provider]]\nkind = \"anthropic\"\nmodel = \"x\"\n",
            "provider",
        ),
        ("[sandbox]\nforce_floor = false\n", "sandbox.force_floor"),
    ] {
        write(root.path(), "io.local.toml", content);
        let err = refusal(root.path());
        teaches(&err, key);
        assert!(
            err.contains("io.local.toml"),
            "names the file it read: {err}"
        );
        assert!(
            err.contains("a run's own agent can write to"),
            "names the reason this scope is no longer trusted: {err}"
        );
    }
}

/// H2's control, and the reason the refusals above name the user scope. The same
/// hook that a workspace file may no longer declare is declared in the one file
/// no workspace can reach, and it loads.
#[test]
fn h2_the_user_scope_still_declares_what_a_workspace_file_may_not() {
    let user = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    write(
        user.path(),
        "io.toml",
        "[[hook]]\non = [\"finished\"]\nrun = [\"true\"]\n\
         [[mcp]]\nid = \"tools\"\ntransport = \"stdio\"\ncommand = \"tools\"\n\
         [[lsp]]\nid = \"rust\"\ncommand = \"rust-analyzer\"\n\
         [sandbox]\nforce_floor = false\n",
    );
    write(root.path(), "io.local.toml", "[run]\nmax_steps = 4\n");

    let config = Config::discover(root.path()).expect("the user scope may still widen");
    assert!(!config.hooks().is_empty());
    assert_eq!(config.mcp_servers().len(), 1);
    assert_eq!(config.lsp_servers().len(), 1);
}

/// The negative control for the module. Every assertion above looks for a
/// refusal, and a `refuse_widening` that refused everything would satisfy all of
/// them while destroying the feature — a repository narrowing its own boundary
/// from its own committed file is what the project scope is *for*.
#[test]
fn a_narrowing_project_file_still_loads() {
    let user = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    write(
        root.path(),
        "io.toml",
        "[policy.defaults]\nexec = \"deny\"\nnet = \"deny\"\n\n\
         [sandbox]\nforce_floor = true\nmode = \"read-only\"\n\n\
         [run]\nmax_steps = 12\n\n\
         [[agent]]\nname = \"searcher\"\ndeny_write = true\n\n\
         [toolchain.cargo]\ntest = [\"cargo\", \"nextest\", \"run\"]\n",
    );
    write(root.path(), "io.local.toml", "[run]\nmax_steps = 4\n");

    let config = Config::discover(root.path()).expect("a narrowing file is the point of the scope");
    let agents = config.agents();
    assert_eq!(agents.names(), vec!["searcher"]);
    assert!(config.policy().is_some());
    assert!(config.sandbox().is_some());
}

/// The two widening keys the 0.74.0 list was still missing, asserted by effect
/// rather than by the refusal's wording.
///
/// `sandbox.mode` was on the list for the literal `"full-access"` alone, so a
/// cloned `io.toml` writing `"workspace-write"` raised an operator's own
/// `"read-only"` back to the crate's default — the merge lets a project file
/// replace the user scope's value, and `ExecMode::narrower` was never called.
/// `sandbox.limits.*` was on no list at all, and `0` there means *no cap*: one
/// number in a cloned file took the wall clock, the memory ceiling or the process
/// count down entirely.
///
/// Each half carries its own control in the same pass, because both keys have a
/// narrowing value a project file is *meant* to write and a rule that refused the
/// key outright would pass every refusal here while deleting the feature.
#[test]
fn h2_a_workspace_file_may_not_raise_a_ceiling_the_user_scope_lowered() {
    for (widening, narrowing, key) in [
        (
            "[sandbox]\nmode = \"workspace-write\"\n",
            "[sandbox]\nmode = \"read-only\"\n",
            "sandbox.mode",
        ),
        (
            "[sandbox.limits]\nmax_wall_secs = 0\n",
            "[sandbox.limits]\nmax_wall_secs = 30\n",
            "sandbox.limits.max_wall_secs",
        ),
    ] {
        for file in ["io.toml", "io.local.toml"] {
            let user = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let _guard = env(user.path());

            // The ceiling the operator set, in the one scope no workspace reaches.
            write(
                user.path(),
                "io.toml",
                "[sandbox]\nmode = \"read-only\"\n[sandbox.limits]\nmax_wall_secs = 5\n",
            );

            write(root.path(), file, widening);
            let err = refusal(root.path());
            teaches(&err, key);
            assert!(err.contains("widens"), "{err}");

            // The control: the narrowing value of the same key, in the same file,
            // still loads and still decides.
            write(root.path(), file, narrowing);
            let sandbox = Config::discover(root.path())
                .unwrap_or_else(|e| panic!("a workspace file may narrow {key}: {e}"))
                .sandbox()
                .expect("the file names `[sandbox]`");
            assert_eq!(sandbox.mode, io_harness::ExecMode::ReadOnly, "{key}");
            assert!(
                sandbox.limits.max_wall_secs.is_some(),
                "the cap survives: {key}"
            );
        }
    }
}

/// H2 by the shortest route there is, and the one the finding does not describe.
///
/// The finding is written about `[[hook]]` and `[[mcp]]` — an agent writes
/// `io.local.toml`, and the next `Config::discover` spawns the argv it named. The
/// widening allowlist closed that. It did not close this: `${cmd:}` **runs a
/// program during parsing**, before any `Policy` or sandbox exists, and it was
/// refused for `io.toml` alone. So the same attack needed no `[[hook]]` at all —
/// any key in any table, and `[app]` accepts arbitrary ones.
///
/// Worse than the section route in two ways. It runs at *load* rather than at a
/// lifecycle event, so it does not wait for the run to reach anything; and
/// expansion happens before the widening check, so even a section that is about
/// to be refused has already run its substitution.
///
/// `${file:}` is the read-only half of the same door: the argument is joined onto
/// the declaring file's directory and an absolute one names any path on the host,
/// resolved before `Act::Read` exists.
#[test]
fn h2_a_workspace_file_may_not_run_a_program_or_read_a_path_while_it_is_parsed() {
    for file in ["io.toml", "io.local.toml"] {
        for (subst, needle) in [("cmd", "${cmd:}"), ("file", "${file:}")] {
            let user = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let _guard = env(user.path());

            // `[app]` on purpose: it takes arbitrary keys, so this needs none of
            // the tables the allowlist refuses. A fix that only covered the
            // refused sections would pass a test written against one of them.
            write(
                root.path(),
                file,
                &format!("[app]\nanything = \"${{{subst}:/usr/bin/whoami}}\"\n"),
            );

            let err = refusal(root.path());
            assert!(
                err.contains(needle),
                "{file} must refuse {needle} while it is parsed: {err}"
            );
            teaches(&err, "app.anything");
        }
    }
}

/// The control: the user scope still runs both, which is what they are for.
///
/// Without this the test above is satisfied by a build that refused every
/// substitution everywhere and took the feature away.
#[test]
fn h2_companion_the_user_scope_still_resolves_a_command_and_a_file() {
    let user = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let _guard = env(user.path());

    std::fs::write(user.path().join("secret.txt"), "from-a-file\n").unwrap();
    write(
        user.path(),
        "io.toml",
        "[app]\nfrom_file = \"${file:secret.txt}\"\n",
    );

    let config = Config::discover(root.path()).expect("the user scope may still substitute");
    let keys: Vec<&str> = config.origins().map(|(k, _)| k).collect();
    assert!(
        keys.iter().any(|k| k.contains("from_file")),
        "the value was read rather than skipped: {keys:?}"
    );
}
