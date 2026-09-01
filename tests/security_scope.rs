//! 0.74.0 audit, LOW sweep — L4 and L13.
//!
//! Two findings about what configuration reaches, rather than about what a run is
//! allowed to do:
//!
//! - **L4** — a credential file was read with `read_to_string` and its mode was
//!   never looked at, so a `0644` `io.local.toml` or `${file:}` target handed a
//!   working key to every account on a shared host and nothing said so.
//! - **L13** — `skills` and `templates` in a plugin manifest, a `[[plugin]]`'s own
//!   `path`, and `run.skills`/`run.templates` in a file inside the workspace were
//!   all taken as written, so an absolute value or a `..` pointed discovery at any
//!   directory on the host and that directory's `*.md` frontmatter reached the
//!   model's system prompt on every turn.
//!
//! Every refusal here is paired with the legitimate case it must not break: a
//! `0600` credential loads in silence, and an ordinary in-root `skills` directory
//! is still discovered and still contributes its skill.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use io_harness::{Config, Skills};

/// Guards `IO_CONFIG_HOME`, which the process has exactly one of.
///
/// `IO_CONFIG` is removed rather than left alone: it names the user-scope *file*
/// outright and wins over `IO_CONFIG_HOME`, so a developer who has one exported
/// would otherwise be running a different test.
static ENV: Mutex<()> = Mutex::new(());

/// Discover `root` against a user scope that holds no file at all.
///
/// The tempdir is dropped on return, which is safe because discovery has already
/// read what it was going to read.
fn discover(root: &Path) -> io_harness::Result<Config> {
    discover_with_user(root, None)
}

/// Discover `root` against a user-scope `io.toml`, or against none.
fn discover_with_user(root: &Path, user_toml: Option<&str>) -> io_harness::Result<Config> {
    let user = tempfile::tempdir().unwrap();
    if let Some(body) = user_toml {
        std::fs::write(user.path().join("io.toml"), body).unwrap();
    }
    discover_at(root, user.path())
}

/// Discover `root` against a user scope the caller owns and has already populated.
///
/// Separate from [`discover_with_user`] because `${file:}` joins its argument onto
/// the directory of the file that declared it, and since 0.74.0 that file may only
/// be the user-scope one — so a test about a credential file has to place the
/// credential beside an `io.toml` whose path it knows and whose mode it sets.
fn discover_at(root: &Path, user: &Path) -> io_harness::Result<Config> {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("IO_CONFIG");
    std::env::set_var("IO_CONFIG_HOME", user);
    Config::discover(root)
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

/// A bundle whose manifest contributes `skills = <value>` and nothing else — the
/// contribution any scope is allowed to declare.
fn bundle_with_skills(dir: &Path, value: &str) {
    write(
        &dir.join("plugin.toml"),
        &format!("name = \"kit\"\ndescription = \"a bundle\"\nskills = {value:?}\n"),
    );
}

// ---------------------------------------------------------------------------
// L4 — a credential file readable by other accounts is named
// ---------------------------------------------------------------------------

/// A `tracing::Subscriber` that keeps every event's message.
///
/// Written out rather than pulled from `tracing-subscriber`, which is not a
/// dependency of this crate and is not worth becoming one for seven method
/// bodies. `tracing` itself is already in the tree, so a test target can name it.
#[cfg(unix)]
mod capture {
    use std::fmt;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    pub struct Capture(pub Arc<Mutex<Vec<String>>>);

    /// The `message` field of a `tracing::warn!("…")` arrives as `fmt::Arguments`
    /// through `record_debug`, whose `Debug` is its `Display` — so this is the
    /// formatted text, not a quoted rendering of it.
    struct Message(String);

    impl Visit for Message {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl Subscriber for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0.lock().unwrap().push(message.0);
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }
}

/// Run `f` with a capturing subscriber installed on this thread, and hand back
/// what it returned beside every message it logged.
#[cfg(unix)]
fn logged<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    use std::sync::{Arc, Mutex};

    let log = Arc::new(Mutex::new(Vec::new()));
    let out = tracing::subscriber::with_default(capture::Capture(Arc::clone(&log)), f);
    let messages = log.lock().unwrap().clone();
    (out, messages)
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// **L4.** A `0644` `io.local.toml` and a `0644` `${file:}` target are each named,
/// with the mode that is wrong and the command that fixes it.
///
/// Fails on 0.73.0's behaviour by construction: nothing anywhere read a mode, so
/// both searches below come back empty and there is no message to assert about.
/// The two call sites are asserted separately because they are two reads —
/// `read_scope` for the file itself, `expand` for what a `${file:}` names — and a
/// check wired into one of them satisfies neither half of the finding.
///
/// The two reads sit in different scopes on purpose, and not for convenience:
/// 0.74.0 refuses `${file:}` in every file inside the workspace, so the credential
/// is named from the user-scope `io.toml` — which is left at `0600` here, so the
/// only config the check may complain about is the workspace one.
#[cfg(unix)]
#[test]
fn l4_an_exposed_config_and_an_exposed_credential_file_are_each_named_with_their_mode() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let local = project.path().join("io.local.toml");
    let user_toml = user.path().join("io.toml");
    let credential = user.path().join("cred.txt");
    write(&credential, "skills\n");
    write(&user_toml, "[run]\nskills = \"${file:cred.txt}\"\n");
    write(&local, "[run]\nmax_steps = 3\n");
    chmod(&user_toml, 0o600);
    chmod(&local, 0o644);
    chmod(&credential, 0o640);

    let (config, log) = logged(|| discover_at(project.path(), user.path()));
    let config =
        config.expect("a readable file is still loaded — this is a warning, not a refusal");
    assert_eq!(
        config.origin("run.skills")[0].path,
        user_toml,
        "the `${{file:}}` was actually expanded, so a missing warning would be a \
         missing check and not a skipped read"
    );

    let named = |needle: &str| -> String {
        log.iter()
            .find(|m| m.contains(needle))
            .unwrap_or_else(|| panic!("nothing warned about {needle}: {log:?}"))
            .clone()
    };

    let about_config = named("io.local.toml");
    assert!(
        about_config.contains("0644"),
        "the mode an operator has to change is in the message: {about_config}"
    );
    assert!(
        about_config.contains("chmod 600"),
        "the fix is in the message: {about_config}"
    );

    let about_credential = named("cred.txt");
    assert!(
        about_credential.contains("0640"),
        "a group-readable credential counts, not only a world-readable one: {about_credential}"
    );
}

/// **L4, the companion.** A `0600` config and a `0600` credential load in silence,
/// and the value read through `${file:}` is the one that lands.
///
/// The half that decides whether the fix is shippable. A check that warned on
/// every file would pass the test above and make the warning worthless.
///
/// Both checked scopes are present at `0600` — the workspace `io.local.toml` that
/// `read_scope` looks at, and the user `io.toml` that declares the `${file:}` —
/// so silence here is silence from both call sites and not from one that was
/// never reached.
#[cfg(unix)]
#[test]
fn l4_a_private_credential_file_loads_with_nothing_said() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let local = project.path().join("io.local.toml");
    let user_toml = user.path().join("io.toml");
    let credential = user.path().join("cred.txt");
    write(&credential, "skills\n");
    write(&user_toml, "[run]\nskills = \"${file:cred.txt}\"\n");
    write(&local, "[run]\nmax_steps = 3\n");
    chmod(&user_toml, 0o600);
    chmod(&local, 0o600);
    chmod(&credential, 0o600);

    let (config, log) = logged(|| discover_at(project.path(), user.path()));
    let config = config.expect("a private config loads");
    assert_eq!(
        config.origin("run.skills")[0].path,
        user_toml,
        "the value was actually read, so silence means silence and not a skipped read"
    );
    assert_eq!(
        config.origin("run.max_steps")[0].path,
        local,
        "the workspace file was read too, so its silence is a mode that passed the check"
    );
    assert!(
        log.iter().all(|m| !m.contains("chmod")),
        "nothing is wrong with any of the three files: {log:?}"
    );
}

/// **L4.** The committed `io.toml` is deliberately not checked.
///
/// It arrives from a `git clone` world-readable by design, and a warning on every
/// run is the one an operator learns to scroll past. Stated as a test because it
/// is a decision, not an oversight: an implementation that checked every scope
/// would pass the two tests above and quietly make the feature useless.
#[cfg(unix)]
#[test]
fn l4_a_committed_project_file_is_not_warned_about() {
    let project = tempfile::tempdir().unwrap();
    let committed = project.path().join("io.toml");
    write(&committed, "[run]\nmax_steps = 3\n");
    chmod(&committed, 0o644);

    let (config, log) = logged(|| discover(project.path()));
    config.expect("a project file loads");
    assert!(
        log.iter().all(|m| !m.contains("io.toml")),
        "the committed file is world-readable by design: {log:?}"
    );
}

// ---------------------------------------------------------------------------
// L13 — discovery stays inside the workspace
// ---------------------------------------------------------------------------

/// **L13.** A manifest may not point `skills` or `templates` out of its bundle.
///
/// Fails on 0.73.0's behaviour: `Plugin::skills_dir` joined the value onto the
/// plugin root, where an absolute one replaced that root outright and a relative
/// one climbed out with `..`, and neither was refused — the bundle loaded, the
/// directory was read at run start, and the frontmatter of every `*.md` under it
/// went into the system prompt of every turn.
#[test]
fn l13_a_manifest_may_not_point_skills_or_templates_out_of_the_bundle() {
    for key in ["skills", "templates"] {
        for value in ["/etc", "../../elsewhere", "sub/../../elsewhere"] {
            let project = tempfile::tempdir().unwrap();
            let root = project.path();
            write(
                &root.join("bundles/kit/plugin.toml"),
                &format!("name = \"kit\"\n{key} = {value:?}\n"),
            );
            write(
                &root.join("io.local.toml"),
                "[[plugin]]\npath = \"bundles/kit\"\n",
            );

            let plugins = discover(root).unwrap().plugins();
            assert!(
                plugins.get("kit").is_none(),
                "{key} = {value}: the bundle must contribute nothing"
            );
            assert_eq!(plugins.dropped().len(), 1, "{key} = {value}");
            let why = &plugins.dropped()[0].error;
            assert!(
                why.contains(&format!("key `{key}`")),
                "{key} = {value}: the refusal names the key: {why}"
            );
        }
    }
}

/// **L13.** A `[[plugin]]`'s own `path` is resolved under the discovery root.
///
/// Fails on 0.73.0's behaviour: `Plugins::load` took an absolute `path` as written
/// and joined a relative one without checking where the join landed, so a file
/// inside the workspace could name a bundle anywhere on the host — including one a
/// previous step had downloaded.
///
/// The symbolic-link case is the one a lexical rule cannot see, and it is why this
/// resolves through `contain_under_root` rather than scanning components.
#[test]
fn l13_a_declared_plugin_path_may_not_escape_the_discovery_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("project");
    let outside = parent.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    bundle_with_skills(&outside, "skills");
    write(&outside.join("skills/leak.md"), "# leak\n");

    // `mut` is used on unix only, where the symbolic-link case is appended.
    #[allow(unused_mut)]
    let mut cases: Vec<String> = vec![
        "../outside".into(),
        outside.to_string_lossy().into_owned(),
        "./sub/../../outside".into(),
    ];
    // A link inside the workspace pointing out of it: lexically an ordinary
    // relative path, and the reason the check is not lexical.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.as_path(), root.join("linked")).unwrap();
        cases.push("linked".into());
    }

    for declared in cases {
        write(
            &root.join("io.local.toml"),
            &format!("[[plugin]]\npath = {declared:?}\n"),
        );
        let plugins = discover(&root).unwrap().plugins();
        assert!(
            plugins.get("kit").is_none(),
            "{declared}: the bundle must contribute nothing"
        );
        assert_eq!(plugins.dropped().len(), 1, "{declared}");
        let why = &plugins.dropped()[0].error;
        assert!(
            why.contains("outside the workspace root"),
            "{declared}: the refusal says where the boundary is: {why}"
        );
    }
}

/// **L13, the companion.** An ordinary in-root bundle still loads, its `skills`
/// directory is still the one inside it, and the skill in it is still discovered.
///
/// The half that decides whether the two refusals above are shippable. A loader
/// that dropped every declaration would satisfy both of them.
#[test]
fn l13_an_ordinary_in_root_skills_directory_is_still_discovered() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let dir = root.join("bundles/kit");
    bundle_with_skills(&dir, "skills");
    write(
        &dir.join("skills/review.md"),
        "# review\n\nRead the diff.\n",
    );
    write(
        &root.join("io.local.toml"),
        "[[plugin]]\npath = \"bundles/kit\"\n",
    );

    let plugins = discover(root).unwrap().plugins();
    assert!(plugins.dropped().is_empty(), "{:?}", plugins.dropped());
    let kit = plugins.get("kit").expect("the bundle loaded");
    let skills_dir = kit.skills_dir().expect("it contributes a skills directory");
    assert_eq!(skills_dir, dir.join("skills"));
    assert_eq!(
        Skills::discover(skills_dir).unwrap().names(),
        vec!["review"],
        "the skill inside the bundle is still read"
    );
}

/// **L13.** A file inside the workspace may not point `run.skills` or
/// `run.templates` out of it.
///
/// The route the finding's own sentence describes, and the one that needs no
/// bundle at all: both keys are joined onto the discovery root, so a cloned
/// `io.toml` saying `skills = "/home/you/.ssh"` put that directory's `*.md`
/// frontmatter into the system prompt of every turn. Fails on 0.73.0's behaviour,
/// where `Config::discover` returned the configuration without complaint.
///
/// Both workspace scopes, because `io.local.toml` has been held to `io.toml`'s
/// rule since this release and a check on one of them is half a rule.
#[test]
fn l13_a_workspace_file_may_not_point_run_skills_out_of_the_workspace() {
    for scope_file in ["io.toml", "io.local.toml"] {
        for key in ["skills", "templates"] {
            for value in ["/etc", "../elsewhere", "sub/../../elsewhere"] {
                let project = tempfile::tempdir().unwrap();
                write(
                    &project.path().join(scope_file),
                    &format!("[run]\n{key} = {value:?}\n"),
                );
                let err = discover(project.path())
                    .expect_err(&format!("{scope_file}: {key} = {value}"))
                    .to_string();
                assert!(
                    err.contains(&format!("run.{key}")),
                    "{scope_file}: {key} = {value}: the refusal names the key: {err}"
                );
            }
        }
    }
}

/// **L13.** The same key hidden in a `[profile]` body is the same refusal.
///
/// A profile is an overlay that reaches the merged table by a second route, so a
/// check placed anywhere but inside `refuse_widening` would close the front door
/// and leave this one open.
#[test]
fn l13_a_profile_body_cannot_smuggle_an_escaping_skills_path() {
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join("io.toml"),
        "[profile.ci.run]\nskills = \"../elsewhere\"\n",
    );
    let err = discover(project.path())
        .expect_err("a profile body is checked too")
        .to_string();
    assert!(err.contains("run.skills"), "{err}");
}

/// **L13, the companion.** A relative `run.skills` still works from a workspace
/// file, and the user scope may still name an absolute one.
///
/// The second half is what keeps the refusal narrow: a shared skills directory
/// kept outside every project is the reason an operator writes an absolute path,
/// and the user-scope file is the one no workspace can reach.
#[test]
fn l13_a_relative_run_skills_works_and_the_user_scope_may_still_leave_the_workspace() {
    let project = tempfile::tempdir().unwrap();
    write(
        &project.path().join("io.toml"),
        "[run]\nskills = \"skills\"\n",
    );
    let config = discover(project.path()).expect("a relative path is the ordinary case");
    assert_eq!(config.origin("run.skills").len(), 1);

    let elsewhere = tempfile::tempdir().unwrap();
    let absolute: PathBuf = elsewhere.path().join("skills");
    let bare = tempfile::tempdir().unwrap();
    let config = discover_with_user(
        bare.path(),
        Some(&format!("[run]\nskills = {absolute:?}\n")),
    )
    .expect("the operator's own file still points wherever the operator wants");
    assert_eq!(config.origin("run.skills").len(), 1);
}
