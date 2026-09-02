//! 0.74.0 — the three Windows containment findings, asserted where a host that
//! is not Windows can still see them.
//!
//! Almost nothing in this file is `cfg`-gated, and that is the point. The
//! Windows half of `sandbox/appcontainer.rs` compiles on one of the three matrix
//! legs, so a unit test beside it is a test that runs a third of the time; and
//! the parts of these three fixes that are *decisions* rather than syscalls are
//! deliberately written as portable functions, unit-tested in
//! `sandbox::windows`'s own module on every leg.
//!
//! What is left over is glue — a count that must match a number of calls, a call
//! that must exist in another module — and glue is exactly what a refactor
//! silently undoes while every behavioural test still passes. Those are derived
//! from the source text here, in the shape `tests/one_runtime_path.rs` and
//! `tests/security_landlock.rs` already use, each with a negative control so a
//! rename cannot leave the assertion matching nothing.
//!
//! The behaviour these stand in for is proven on the Windows leg by
//! `sandbox::appcontainer`'s own `#[cfg(windows)]` tests, which need a live
//! container to say anything at all.

const APPCONTAINER: &str = include_str!("../src/sandbox/appcontainer.rs");
const WINDOWS: &str = include_str!("../src/sandbox/windows.rs");
const SHELL: &str = include_str!("../src/tools/shell.rs");
const GIT: &str = include_str!("../src/tools/git.rs");

/// H11 — the two tools that own their own `Child` consult the backend before
/// they spawn one.
///
/// `wrap_argv` and `contain_command` are the whole of how a caller that does not
/// go through `Sandbox::run` reaches containment, and neither has a Windows
/// branch — an AppContainer is entered at `CreateProcessW` through a
/// process-thread attribute list, so there is no argv to prepend and no
/// `pre_exec` to install. With `access_confinement` set, `shell`, `shell_start`
/// and the git built-ins therefore spawned a completely unwrapped child while
/// the run wrote `windows-appcontainer` into its `SandboxEvent` rows and into the
/// agent's own boundary prompt.
///
/// The fix is a refusal at both sites, and a refusal is one `if` that a later
/// edit can drop without failing anything else in the suite. This is what fails
/// when it does.
#[test]
fn h11_the_tools_that_spawn_their_own_child_refuse_a_backend_they_cannot_apply() {
    for (path, text) in [("src/tools/shell.rs", SHELL), ("src/tools/git.rs", GIT)] {
        assert!(
            text.contains("applied_only_by_sandbox_run"),
            "{path} spawns a `Child` of its own, so it is confined by whatever `wrap_argv` and \
             `contain_command` can express and by nothing else. It must consult \
             `sandbox::windows::applied_only_by_sandbox_run` and refuse, or a run with \
             `access_confinement` set runs this tool's children with no filesystem and no \
             network boundary while `ExecContainment::backend()` answers `WindowsAppContainer`"
        );
    }
    // The negative control: the guard has to exist to be called, and a rename
    // that moved it would otherwise leave both assertions above matching a
    // comment.
    assert!(
        WINDOWS.contains("fn applied_only_by_sandbox_run(backend: Backend) -> bool"),
        "src/sandbox/windows.rs no longer defines the decision the two tools consult"
    );
}

/// M8 — the profile is the run's own, and it is deleted when the run ends.
///
/// A profile's SID is `DeriveAppContainerSidFromAppContainerName` of its name, so
/// a name written into this crate is a SID any process on the machine can derive
/// and spawn itself into. Between 0.59.0 and 0.74.0 there was such a name, and it
/// opted out of `Drop` so that the container — and every grant any run had ever
/// added to it — outlived every process that used it.
#[test]
fn m8_the_container_profile_belongs_to_one_run_and_does_not_outlive_it() {
    assert!(
        !APPCONTAINER.contains("delete_on_drop"),
        "a profile that opts out of its own `Drop` is a container left registered on the \
         operator's machine holding whatever the run granted it, addressed by a name this \
         crate wrote down"
    );
    assert!(
        !APPCONTAINER.contains("fn shared(") && !WINDOWS.contains("Profile::shared("),
        "`Profile::shared` was the machine-wide profile: one name, one SID, and every run's \
         grants accumulating under it"
    );
    assert!(
        WINDOWS.contains("Profile::create(&run_profile("),
        "the contained run must build its profile from `run_profile`, which draws a fresh \
         unguessable name per run — see `run_profile_name`"
    );
    // The negative control for the first assertion: the deletion has to be there
    // for its being unconditional to mean anything.
    assert!(
        APPCONTAINER.contains("DeleteAppContainerProfile(self.name.as_ptr())"),
        "nothing deletes the profile at all, so every run leaves one behind"
    );
}

/// M9 — the process-thread attribute list is sized for exactly what is set.
///
/// `CreateProcessW` is called with `bInheritHandles` true, because the redirect
/// and the C-runtime descriptor table both need it. On its own that is a blanket
/// grant: every handle this process holds that is marked inheritable is
/// duplicated into a default-deny container carrying the access it was opened
/// with, past a token whose whole job is to refuse what was not granted by name.
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` is what turns the blanket into a list.
///
/// The list's *contents* are a portable function, asserted in
/// `sandbox::windows`'s own tests. What cannot be asserted there is the count:
/// a list initialised for one and given two fails the second update, and a list
/// initialised for two and given one is read only as far as it was filled — so
/// the number and the calls have to agree, and nothing but this notices when
/// they stop.
#[test]
fn m9_the_attribute_list_is_sized_for_every_attribute_the_container_spawn_sets() {
    let calls = APPCONTAINER.matches("UpdateProcThreadAttribute(").count();
    assert!(
        APPCONTAINER.contains(&format!("const ATTRIBUTES: u32 = {calls};")),
        "the attribute list is sized by `ATTRIBUTES` and {calls} attributes are set, and the \
         two no longer agree. A list given more than it was sized for fails the extra update \
         and the spawn returns an error; a list given fewer is read only as far as it was \
         filled, which is the failure that is silent"
    );

    let sizings = APPCONTAINER
        .matches("InitializeProcThreadAttributeList(")
        .count();
    assert_eq!(
        sizings, 2,
        "the list is sized by the API and then initialised, which is two calls; a third would \
         be a second list this test says nothing about"
    );
    // The leading space is load-bearing: it keeps `SID_AND_ATTRIBUTES` and
    // `FILE_READ_ATTRIBUTES` out of the count.
    assert_eq!(
        APPCONTAINER.matches(" ATTRIBUTES,").count(),
        sizings,
        "one of those two calls is sized by a literal rather than by `ATTRIBUTES`, so the \
         assertion above no longer decides anything: the size the API is asked for and the \
         size the list is built with can drift apart in silence"
    );

    assert!(
        APPCONTAINER.contains("PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize"),
        "without the handle-list attribute every inheritable handle the harness holds — a \
         store file, a socket, a log — crosses into the container carrying its open-time \
         access, and nothing in the container's ACL sees it, because a handle does not go \
         back through an access check"
    );
    assert!(
        APPCONTAINER.contains("windows::inheritable_handles(handle, inherited)"),
        "the handle list must be the portable one, or what may cross into a container is \
         decided in code only the Windows leg compiles and only a live container can run"
    );
    // The negative control: every assertion above is a substring search over one
    // file, and a file that stopped containing the spawn would satisfy the two
    // negative ones for free.
    assert!(
        APPCONTAINER.contains("CreateProcessW("),
        "src/sandbox/appcontainer.rs no longer spawns anything"
    );
}
