//! Windows AppContainer — the **access** half of containment.
//!
//! [`WindowsSandbox`](super::windows::WindowsSandbox) contains *resources* and
//! says so in its own first paragraphs: a job object has no filesystem facility
//! and no network facility, because those are not options the API has. This
//! module is the other half. An AppContainer is a low-box security context whose
//! token answers *no* to every securable object by default, and reaches only what
//! has been granted to its own container SID.
//!
//! Two columns close for the price of one mechanism:
//!
//! - **Filesystem** — default-deny. A path the payload may touch is a path
//!   something granted, by name, with an explicit ACE — see `grant` below, which
//!   is deliberately named in plain text rather than linked: everything in this
//!   module is `cfg(windows)`, docs.rs renders on Linux, and an intra-doc link
//!   into code that does not exist on the rendering host is a broken link on the
//!   only page a reader actually sees.
//! - **Network** — the capability array carries `internetClient` when the run's
//!   policy permits egress and is **empty** when it does not. Empty is the
//!   denial: `internetClient` is the capability that buys a socket to the
//!   outside, so without it there is no route off the machine. This is the
//!   absence of a permission rather than the presence of a filter, which is the
//!   same shape as an empty network namespace on Linux and as a Landlock rule
//!   set that handles `CONNECT_TCP` and permits no port, and is why it is worth
//!   preferring over a filesystem-ACL scheme with a separate network story
//!   bolted beside it.
//!
//! ## Why this module owns its own spawn
//!
//! The container SID reaches a child through
//! `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` on a process-thread attribute
//! list, and that is the only documented route. `tokio::process::Command` cannot
//! carry one on stable Rust — `raw_attribute` is still gated behind the
//! unstabilized `windows_process_extensions_raw_attribute` — and neither can
//! `std`'s. So there is no version of this feature, network-only included, that
//! does not mean calling `CreateProcessW` here.
//!
//! What that costs is re-implementing what the shared runner at
//! `crate::sandbox::run_capped_hooked` gives free: standard handles, collecting
//! output, the wall clock, and killing what is left. Each is re-implemented for
//! **this backend only** — the shared runner is not touched, because a spawn-path
//! change that reaches macOS and Linux is a change to two shipped backends.
//!
//! ## Output goes to a file, not a pipe
//!
//! The runner pipes stdout and stderr and drains them concurrently with the wait.
//! This module redirects both to one temporary file instead, and reads it after
//! the process is reaped.
//!
//! That is a smaller mechanism for the same result, and it is the design
//! `crate::tools::handles` already argues for: a pipe that nobody drains fills
//! and blocks the payload, so a pipe obliges you to drain it, and draining it
//! obliges you to do so concurrently with the wait. A file has neither problem —
//! the kernel writes it, nothing has to be pumping for the payload to make
//! progress, and the whole of it is still there afterwards. One file rather than
//! two also gives the two streams a shared offset, so they interleave the way a
//! terminal shows them; the runner's separate `stdout`/`stderr` strings are
//! reproduced by putting the combined text in `stdout` and leaving `stderr`
//! empty, which is stated here because it is a real difference from the other
//! backends rather than an accident.

// The wiring landed in 0.47.0, so the `allow(dead_code)` this module carried
// since 0.26.0 has come off: `Profile`, `grant` and `Spawned` are now reached by
// `super::windows`, which selects this backend rather than only testing it.
#[cfg(windows)]
pub(crate) mod win {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, ERROR_SUCCESS,
        GENERIC_ALL, GENERIC_EXECUTE, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
        GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_GROUP, TRUSTEE_IS_SID,
        TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeleteAppContainerProfile,
        DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, FreeSid, GetAce, WinCapabilityInternetClientSid,
        ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION, PSID, SID_AND_ATTRIBUTES,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject, CREATE_SUSPENDED,
        CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES,
        STARTUPINFOEXW, STARTUPINFOW,
    };

    /// A NUL-terminated UTF-16 string, kept alive by the caller.
    ///
    /// Every Win32 `W` entry point takes one, and the single most common way to
    /// get this wrong is to build the buffer inline and let it drop before the
    /// call reads it. Returning an owned `Vec` makes the lifetime the caller's
    /// problem, which is where it can actually be seen.
    fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// What a grant lets the container do with a path.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Access {
        /// Read and execute. What a binary, a toolchain or a read-only input
        /// tree needs, and the most that should ever be given to one.
        ReadExecute,
        /// Everything. The workspace, and deliberately nothing else — this is the
        /// one directory the payload is *meant* to be able to change.
        Full,
    }

    impl Access {
        fn mask(self) -> u32 {
            match self {
                Access::ReadExecute => GENERIC_READ | GENERIC_EXECUTE,
                Access::Full => GENERIC_ALL,
            }
        }
    }

    /// How far into a directory a grant is meant to reach.
    ///
    /// Mirrors `super::super::windows::Reach`, for the same reason `Access`
    /// mirrors `Grant`: the decision is portable data asserted on the build host
    /// and this is where it becomes a Win32 flag.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Reach {
        /// The directory and everything already inside it.
        Tree,
        /// The directory itself. What it *later* contains inherits the ACE; what
        /// it already contains keeps whatever it had.
        DirectoryOnly,
    }

    /// Apply one entry of the grant set `super::super::windows::grants` derived.
    ///
    /// The bridge exists so the *decision* and the *ACE mask* stay separate
    /// types: the decision is portable data asserted on the build host, and this
    /// is the one place it becomes Win32. A single `match` rather than making the
    /// portable half depend on a `cfg(windows)` enum.
    pub(crate) fn grant_for(
        path: &Path,
        sid: PSID,
        g: crate::sandbox::windows::Grant,
        r: crate::sandbox::windows::Reach,
    ) -> io::Result<()> {
        let access = match g {
            crate::sandbox::windows::Grant::ReadExecute => Access::ReadExecute,
            crate::sandbox::windows::Grant::Full => Access::Full,
        };
        let reach = match r {
            crate::sandbox::windows::Reach::Tree => Reach::Tree,
            crate::sandbox::windows::Reach::DirectoryOnly => Reach::DirectoryOnly,
        };
        grant(path, sid, access, reach)
    }

    /// An AppContainer profile, and the SID it is addressed by.
    ///
    /// The profile is a registry-backed, named object with a lifetime longer than
    /// this process, which is why `Drop` deletes it: a run that leaves one behind
    /// leaves state on the operator's machine, and a thousand runs leave a
    /// thousand. Creation tolerates `ERROR_ALREADY_EXISTS` and falls back to
    /// deriving the SID from the name, so a profile stranded by a crashed run is
    /// reused rather than turned into a permanent failure.
    pub(crate) struct Profile {
        name: Vec<u16>,
        sid: PSID,
    }

    // SAFETY: both members are plain data — an owned UTF-16 buffer and a SID,
    // which is a process-wide allocation addressed by pointer and not a
    // thread-affine resource. Every API used here accepts the SID from any
    // thread. `PSID` is a raw pointer typedef, which is the only reason the auto
    // traits are withheld, and the sandbox future must be `Send` to satisfy the
    // `Sandbox` trait.
    unsafe impl Send for Profile {}
    unsafe impl Sync for Profile {}

    impl Profile {
        /// Create (or adopt) the profile called `name`.
        ///
        /// **The capability array is the network boundary, in both directions.**
        ///
        /// Empty is the denial: no `internetClient`, no `internetClientServer`,
        /// no `privateNetworkClientServer`, so a payload in this container holds
        /// no capability that grants it a socket to anywhere and the refusal is
        /// the token's own rather than a rule something has to keep enforcing.
        /// That is the same shape as an empty network namespace on Linux and as
        /// a Landlock rule set that handles `CONNECT_TCP` and permits no port.
        ///
        /// A run whose policy *permits* egress is the other direction, and it is
        /// why this takes an argument at all (0.47.0). Before it, selecting this
        /// backend would have silently broken every network-permitting run,
        /// which is a good reason not to select a backend and a bad reason to
        /// leave one unwired. Exactly `internetClient` is requested — the
        /// outbound capability — and never the server or private-network ones:
        /// the crate's own authority on the network is the run's `Policy`, and
        /// nothing here widens what that already decided.
        pub(crate) fn create(name: &str, allow_network: bool) -> io::Result<Self> {
            let name = wide(name);
            let display = wide("io-harness sandbox");
            let mut sid: PSID = std::ptr::null_mut();

            // The capability SID buffer, built before the create call and kept
            // alive across it. `SECURITY_MAX_SID_SIZE` is 68; a fixed buffer
            // avoids an allocation whose lifetime would be one more thing to get
            // right.
            let mut cap_sid = [0u8; 68];
            let mut caps: [SID_AND_ATTRIBUTES; 1] = unsafe { std::mem::zeroed() };
            let cap_count = if allow_network {
                let mut len = cap_sid.len() as u32;
                // SAFETY: `cap_sid` is a live buffer of at least
                // `SECURITY_MAX_SID_SIZE` bytes and `len` is its true length as a
                // live in/out parameter. A null domain SID is what a
                // capability SID takes.
                if unsafe {
                    CreateWellKnownSid(
                        WinCapabilityInternetClientSid,
                        std::ptr::null_mut(),
                        cap_sid.as_mut_ptr().cast::<core::ffi::c_void>(),
                        &mut len,
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                caps[0].Sid = cap_sid.as_mut_ptr().cast::<core::ffi::c_void>();
                // `SE_GROUP_ENABLED`. The attribute that makes the capability
                // present rather than merely listed.
                caps[0].Attributes = 0x0000_0004;
                1u32
            } else {
                0
            };
            let cap_ptr = if cap_count == 0 {
                std::ptr::null_mut()
            } else {
                caps.as_mut_ptr()
            };

            // SAFETY: `name` and `display` are live NUL-terminated UTF-16 buffers
            // owned by this frame and outliving the call. The capability array is
            // null with a count of zero, which is the documented way to ask for
            // no capabilities at all. `sid` is a live out-parameter.
            let hr = unsafe {
                CreateAppContainerProfile(
                    name.as_ptr(),
                    display.as_ptr(),
                    display.as_ptr(),
                    cap_ptr,
                    cap_count,
                    &mut sid,
                )
            };

            if hr < 0 {
                // A profile left behind by a previous run is not an error: the
                // name is deterministic precisely so it can be re-entered.
                // `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)` is the one code that
                // means "this exists", and anything else is a real failure.
                let already = hr == hresult_from_win32(ERROR_ALREADY_EXISTS);
                if !already {
                    return Err(io::Error::from_raw_os_error(win32_from_hresult(hr)));
                }
                // SAFETY: as above; `name` is live and `sid` is a live
                // out-parameter that this call fills on success.
                let hr =
                    unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
                if hr < 0 {
                    return Err(io::Error::from_raw_os_error(win32_from_hresult(hr)));
                }
            }

            Ok(Profile { name, sid })
        }

        /// The container SID. Valid for as long as this `Profile` is.
        pub(crate) fn sid(&self) -> PSID {
            self.sid
        }
    }

    impl Drop for Profile {
        fn drop(&mut self) {
            // SAFETY: `self.sid` came from `CreateAppContainerProfile` or
            // `DeriveAppContainerSidFromAppContainerName`, is never copied out of
            // this type, and this is the only free. `self.name` is still live.
            unsafe {
                FreeSid(self.sid);
                DeleteAppContainerProfile(self.name.as_ptr());
            }
        }
    }

    /// `HRESULT_FROM_WIN32`, which is a macro in the SDK and therefore has no
    /// symbol to import.
    fn hresult_from_win32(code: u32) -> i32 {
        if code == 0 {
            return 0;
        }
        ((code & 0x0000_FFFF) | 0x8007_0000) as i32
    }

    /// The inverse, for turning a failed `HRESULT` back into something
    /// `io::Error` can render. A non-Win32 facility is returned whole rather than
    /// masked into a plausible-looking and wrong error number.
    fn win32_from_hresult(hr: i32) -> i32 {
        if (hr as u32) & 0xFFFF_0000 == 0x8007_0000 {
            (hr as u32 & 0x0000_FFFF) as i32
        } else {
            hr
        }
    }

    /// What `path`'s DACL already allows `sid`, if it names it at all.
    ///
    /// `None` is "this SID appears in no allow-ACE on this object", which is the
    /// only thing that separates a grant that never reached a file from a grant
    /// that reached it and was not enough. Every Windows failure this release
    /// has debugged was one of those two, and until this existed the difference
    /// was inferred from a payload that failed.
    pub(crate) fn granted_mask(path: &Path, sid: PSID) -> Option<u32> {
        let wpath = wide(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();
        // SAFETY: `wpath` is a live NUL-terminated path and every out-parameter
        // is a live local; the owner, group and SACL outs are null, which the
        // API documents as "do not return this".
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let _sd = LocalGuard(sd);

        if dacl.is_null() {
            return None;
        }
        // SAFETY: `dacl` points into the descriptor `_sd` still owns.
        let count = unsafe { (*dacl).AceCount };
        for i in 0..count {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `i` is below the ACE count just read from this ACL.
            if unsafe { GetAce(dacl, u32::from(i), &mut ace) } == 0 {
                continue;
            }
            let allow = ace.cast::<ACCESS_ALLOWED_ACE>();
            // Only an allow-ACE (`ACCESS_ALLOWED_ACE_TYPE`, 0) carries a SID at
            // this offset; reading another type through this layout would be
            // reading the wrong bytes.
            // SAFETY: `allow` is the ACE `GetAce` just returned.
            if unsafe { (*allow).Header.AceType } != 0 {
                continue;
            }
            // SAFETY: `SidStart` is the first word of the ACE's inline SID.
            let ace_sid = unsafe { std::ptr::addr_of!((*allow).SidStart) } as PSID;
            // SAFETY: both are live SIDs, one inside the ACL and one the
            // caller's.
            if unsafe { EqualSid(ace_sid, sid) } != 0 {
                // SAFETY: as above.
                return Some(unsafe { (*allow).Mask });
            }
        }
        None
    }

    /// Grant `sid` `access` to `path`, by adding one ACE to its DACL.
    ///
    /// Adding, never replacing: `GRANT_ACCESS` merges with what is already there,
    /// so a workspace the operator can read stays readable by the operator. The
    /// ACE is inheritable (`3` is `CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE`)
    /// because a grant on a directory that did not reach its contents would be a
    /// grant that looks applied and does nothing — the failure mode this whole
    /// module has to avoid.
    ///
    /// This is the expensive half of the feature. An AppContainer is default-deny
    /// for reads, so every path the payload legitimately needs has to be named:
    /// the workspace, the binary it executes, its toolchain, and its temporary
    /// directory. A missing grant surfaces as a payload that cannot start, which
    /// reads like a broken payload rather than a missing grant — hence the
    /// tracing below.
    pub(crate) fn grant(path: &Path, sid: PSID, access: Access, reach: Reach) -> io::Result<()> {
        // **A grant this SID already has is not applied again, and that is a
        // correctness fix before it is a saving.**
        //
        // Re-propagating rewrites the DACL of every object under `path`. Doing
        // that to a shared tree while another process is reading one of those
        // DACLs is how a file that demonstrably carries the ACE is refused a
        // moment later: the rewrite recomputes each child from the parent's
        // inheritable set, and a reader in the window between sees the object
        // mid-flight. `windows-latest` runs twenty test processes at once, each
        // of them granting `%TEMP%` and `CARGO_HOME`, which is exactly that
        // window twenty times over — and it is what the depth test caught,
        // reading the ACE off a file that then could not be executed.
        //
        // The container SID is derived from a fixed profile name, so it is the
        // same SID on every run of every process on the machine: the first run
        // pays for the walk and no later one repeats it.
        //
        // The comparison is against the mask this function itself writes, which
        // is why it can be a plain bit test rather than a generic-rights
        // mapping: the ACE being looked for is the one a previous run added to
        // this very path, in the same form. An ACE that says the same thing in
        // its mapped form is not recognised and costs one more walk, which is
        // the safe direction to be wrong in.
        if let Some(have) = granted_mask(path, sid) {
            let want = access.mask();
            if have & want == want {
                tracing::debug!(
                    path = %path.display(), ?access,
                    "sandbox: the AppContainer already has this grant"
                );
                return Ok(());
            }
        }

        let wpath = wide(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = std::ptr::null_mut();

        // SAFETY: `wpath` is a live NUL-terminated UTF-16 path. Every out-
        // parameter is a live local. The owner, group and SACL outs are null,
        // which the API documents as "do not return this".
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        // The security descriptor owns the DACL that was just handed back, so it
        // has to outlive `SetEntriesInAclW` and be freed exactly once afterwards.
        let _sd = LocalGuard(sd);

        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: access.mask(),
            grfAccessMode: GRANT_ACCESS,
            // CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE.
            grfInheritance: 3,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_GROUP,
                ptstrName: sid.cast(),
            },
        };

        let mut merged: *mut ACL = std::ptr::null_mut();
        // SAFETY: one live `EXPLICIT_ACCESS_W` is passed with a count of one, the
        // old DACL is the one just read and still owned by `_sd`, and `merged` is
        // a live out-parameter that the call allocates into on success.
        let rc = unsafe { SetEntriesInAclW(1, &ea, dacl, &mut merged) };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let _merged = LocalGuard(merged.cast());

        // **`UNPROTECTED_DACL_SECURITY_INFORMATION`, and it is the whole grant.**
        //
        // Windows inheritance is *static*: a child object carries its own
        // materialised DACL, copied from its parent's inheritable ACEs at the
        // moment it was created. Adding an inheritable ACE to a directory
        // therefore reaches the directory and **nothing already inside it** — the
        // system re-propagates to existing children only when the change is made
        // with this flag.
        //
        // Without it every grant here looked applied and did almost nothing, which
        // is the exact failure mode the comment above `grant` warns about. The
        // workspace was granted and the source file already in it was not, so
        // `rustc a.rs` came back "Access is denied"; `%TEMP%` was granted and the
        // temporary directory created before the run was not. Thirty-one tests on
        // `windows-latest`, one flag.
        //
        // The cost is real and is not hidden: propagation walks the granted tree,
        // so the *first* grant of a large directory is O(entries in it). It is
        // paid once per machine rather than once per run — the check at the head
        // of this function returns early on every later run — and `Reach` is what
        // decides whether a path is worth walking at all. That is what N5
        // measures on this backend.
        //
        // SAFETY: `wpath` is live, `merged` is the ACL just built and still
        // owned by `_merged`, and the owner, group and SACL arguments are null,
        // which with the DACL bits alone means "change only the DACL".
        let propagate = match reach {
            Reach::Tree => UNPROTECTED_DACL_SECURITY_INFORMATION,
            Reach::DirectoryOnly => 0,
        };
        let rc = unsafe {
            SetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | propagate,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                merged,
                std::ptr::null(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        tracing::debug!(path = %path.display(), ?access, "sandbox: granted the AppContainer");
        Ok(())
    }

    /// Frees a `LocalAlloc`-allocated block exactly once, on every path out.
    struct LocalGuard(*mut core::ffi::c_void);

    impl Drop for LocalGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the pointer came from an API documented to return
                // `LocalAlloc` memory, is not null, is owned solely by this
                // guard, and is freed once.
                unsafe { LocalFree(self.0) };
            }
        }
    }

    /// A process running inside an AppContainer.
    ///
    /// Holds the process handle, so [`Drop`] can be the kill-on-drop the shared
    /// runner gets from `tokio`. Nothing this type spawns can outlive it by
    /// accident, which is the same guarantee and for the same reason.
    pub(crate) struct Spawned {
        process: HANDLE,
        thread: HANDLE,
        reaped: bool,
    }

    // SAFETY: a Win32 kernel handle is a process-wide table index that every API
    // used here accepts from any thread; `HANDLE` is a raw pointer typedef, which
    // is the only reason the auto traits are withheld.
    unsafe impl Send for Spawned {}
    unsafe impl Sync for Spawned {}

    impl Spawned {
        /// Start `cmdline` inside the container `sid`, in `cwd`, with both
        /// standard streams going to `out`.
        ///
        /// `out` must be an inheritable handle; this makes it one rather than
        /// requiring the caller to remember, because a non-inheritable handle
        /// here produces a process that starts, runs and writes nothing, which is
        /// the worst available failure — it looks like a quiet payload rather
        /// than a broken redirect.
        pub(crate) fn start(
            cmdline: &str,
            cwd: &Path,
            sid: PSID,
            out: &std::fs::File,
        ) -> io::Result<Self> {
            let handle = out.as_raw_handle() as HANDLE;
            // SAFETY: `handle` belongs to `out`, which is borrowed for this whole
            // call and therefore outlives it.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(io::Error::last_os_error());
            }

            // The attribute list, sized by the API rather than guessed. The first
            // call is *expected* to fail with ERROR_INSUFFICIENT_BUFFER — that is
            // how it reports the size — so its return value is deliberately not
            // checked and `size` is what is read instead.
            let mut size: usize = 0;
            // SAFETY: a null list with a live `size` out-parameter is the
            // documented way to ask how large the list must be.
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
            if size == 0 {
                return Err(io::Error::other(
                    "the process attribute list reported a size of zero",
                ));
            }
            // `Vec<usize>` rather than `Vec<u8>`: the list has pointer alignment
            // requirements that a byte vector does not promise.
            let mut buf: Vec<usize> = vec![0; size.div_ceil(size_of::<usize>())];
            let list = buf.as_mut_ptr().cast::<core::ffi::c_void>() as LPPROC_THREAD_ATTRIBUTE_LIST;

            // SAFETY: `list` points at `buf`, which is at least `size` bytes and
            // outlives every use below; the count matches the one attribute set
            // immediately after.
            if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let _attrs = AttrGuard(list);

            // This is the whole feature: the one attribute that carries a
            // container SID into a child. It must outlive `CreateProcessW`,
            // because the list stores a pointer to it rather than a copy.
            let caps = windows_sys::Win32::Security::SECURITY_CAPABILITIES {
                AppContainerSid: sid,
                Capabilities: std::ptr::null_mut(),
                CapabilityCount: 0,
                Reserved: 0,
            };
            // SAFETY: `list` is initialised, the attribute constant names the
            // `SECURITY_CAPABILITIES` type that `caps` is, the size passed is
            // that type's own `size_of`, and `caps` lives until after the spawn
            // below.
            if unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    std::ptr::from_ref(&caps).cast(),
                    size_of_val(&caps),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }

            let mut si = STARTUPINFOEXW {
                StartupInfo: STARTUPINFOW {
                    cb: size_of::<STARTUPINFOEXW>() as u32,
                    dwFlags: STARTF_USESTDHANDLES,
                    hStdInput: std::ptr::null_mut(),
                    hStdOutput: handle,
                    hStdError: handle,
                    ..Default::default()
                },
                lpAttributeList: list,
            };
            let mut pi = PROCESS_INFORMATION::default();
            // `CreateProcessW` may write to the command line buffer, so it gets a
            // mutable one of its own rather than a shared literal.
            let mut cmd = wide(cmdline);
            let wcwd = wide(cwd);

            // SAFETY: `cmd`, `wcwd` and `si` are all live for the call. Handle
            // inheritance is on, which is required for the redirect above to
            // reach the child. `EXTENDED_STARTUPINFO_PRESENT` is what makes the
            // kernel read `si` as a `STARTUPINFOEXW` and consult its attribute
            // list, and without it the container SID would be silently ignored.
            let ok = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    cmd.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    // `CREATE_SUSPENDED` since 0.47.0, and it is the same
                    // correctness argument `super::super::windows` makes for the
                    // Job Object — now holding twice, because this process has
                    // to join the job *as well as* the container. A process that
                    // runs even briefly outside the job can spawn a descendant
                    // that is never a member, which then outlives the run and
                    // ignores every limit, and nothing reports a failure. The
                    // caller assigns and then calls `resume`.
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                    std::ptr::null(),
                    wcwd.as_ptr(),
                    std::ptr::from_mut(&mut si).cast::<STARTUPINFOW>(),
                    &mut pi,
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(Spawned {
                process: pi.hProcess,
                thread: pi.hThread,
                reaped: false,
            })
        }

        /// The process handle, for the caller that has to assign this process to
        /// a Job Object before it runs.
        ///
        /// Borrowed, never owned: `Spawned` closes both handles in `Drop` and
        /// stays the only owner. A caller that closed this would leave `Drop`
        /// closing a handle it does not have.
        pub(crate) fn process(&self) -> HANDLE {
            self.process
        }

        /// Let the initial thread run.
        ///
        /// Called after the process has been assigned to its job, which is the
        /// only ordering under which there is no instant where the process is
        /// both running and unassigned.
        ///
        /// Unlike `super::super::windows::job`, this needs no ToolHelp thread
        /// snapshot: `CreateProcessW` handed back the initial thread's handle and
        /// this type kept it. That detour exists only on the `std::Command` path,
        /// which closes the thread handle before returning a `Child`.
        pub(crate) fn resume(&self) -> io::Result<()> {
            // SAFETY: `self.thread` is this type's own still-open handle to the
            // initial thread of a suspended process.
            if unsafe { ResumeThread(self.thread) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Wait up to `ms` for the process, killing it if the ceiling is reached.
        ///
        /// `Ok(Some(code))` is a process that ended by itself; `Ok(None)` is the
        /// wall clock firing, and by the time it is returned the process has been
        /// terminated rather than left running — which is the distinction the
        /// shared runner's own wall-clock path exists to keep.
        pub(crate) fn wait(&mut self, ms: u32) -> io::Result<Option<i32>> {
            // SAFETY: `self.process` is the handle from `CreateProcessW`, still
            // open, and this type is the only owner.
            let w = unsafe { WaitForSingleObject(self.process, ms) };
            if w != WAIT_OBJECT_0 {
                self.kill();
                return Ok(None);
            }
            let mut code: u32 = 0;
            // SAFETY: as above, with a live out-parameter.
            if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            self.reaped = true;
            Ok(Some(code as i32))
        }

        /// Terminate the process. Idempotent, and safe on one that is already
        /// gone — Windows answers with an error, which is the best-effort
        /// contract this shares with `kill_tree`.
        pub(crate) fn kill(&mut self) {
            if !self.reaped {
                // SAFETY: `self.process` is this type's own still-open handle.
                unsafe { TerminateProcess(self.process, 1) };
                self.reaped = true;
            }
        }
    }

    impl Drop for Spawned {
        fn drop(&mut self) {
            self.kill();
            // SAFETY: both handles came from `CreateProcessW`, are owned solely
            // by this type, and are closed exactly once.
            unsafe {
                CloseHandle(self.thread);
                CloseHandle(self.process);
            }
        }
    }

    /// Deletes the attribute list exactly once, on every path out of `start`.
    struct AttrGuard(LPPROC_THREAD_ATTRIBUTE_LIST);

    impl Drop for AttrGuard {
        fn drop(&mut self) {
            // SAFETY: the list was initialised by `InitializeProcThreadAttributeList`
            // and is deleted once. The buffer behind it is freed by its `Vec`.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
}

/// The Windows runner is the only machine that can answer any of this, so these
/// are written to **fail loudly** rather than to skip.
///
/// A skipped test on the one platform a release is about is indistinguishable
/// from a passing one, and this crate has already paid once for that: 0.9.1 found
/// `Backend::WindowsJobObject` being reported by a backend that created no job,
/// on a matrix that was green.
///
/// Every containment claim below carries its **negative control** — the identical
/// payload, the identical target, run *outside* the container — because a denial
/// that would also have been a denial outside proves nothing about the boundary.
/// Where a control cannot run at all the test says so and fails, rather than
/// quietly recording the denial as evidence.
#[cfg(all(test, windows))]
mod tests {
    use super::win::{grant, granted_mask, Access, Profile, Reach, Spawned};
    use std::io::Read;
    use std::os::windows::process::CommandExt;

    /// A profile name unique to this test process and this call site. Deleted by
    /// `Profile`'s `Drop`, and re-enterable if a crash ever leaves one behind.
    fn name(tag: &str) -> String {
        format!("io-harness-test-{}-{tag}", std::process::id())
    }

    /// Run `cmdline` inside a container that has `Full` access to `cwd`, and
    /// return its exit code and combined output.
    fn in_container(tag: &str, cmdline: &str, cwd: &std::path::Path) -> (Option<i32>, String) {
        let profile = Profile::create(&name(tag), false).unwrap_or_else(|e| {
            panic!(
                "F1: could not create an AppContainer profile on this host ({e}). This is \
                 fallback_scope Trigger A: the release's central mechanism is unavailable here."
            )
        });
        grant(cwd, profile.sid(), Access::Full, Reach::Tree)
            .unwrap_or_else(|e| panic!("could not grant the workspace to the container: {e}"));

        let out_path = cwd.join("io-harness-out.txt");
        let file = std::fs::File::create(&out_path).expect("create the capture file");
        let mut child = Spawned::start(cmdline, cwd, profile.sid(), &file)
            .unwrap_or_else(|e| panic!("F1: CreateProcessW into the AppContainer failed: {e}"));
        // 0.47.0 spawns suspended so the backend can put the process in its
        // job object before it runs an instruction. A caller that only starts
        // and waits gets a frozen process and a wall-clock kill.
        child.resume().expect("resume the contained process");
        drop(file);

        let code = child.wait(30_000).expect("wait");
        let mut text = String::new();
        std::fs::File::open(&out_path)
            .expect("reopen the capture file")
            .read_to_string(&mut text)
            .ok();
        (code, text)
    }

    /// The same command, with no container at all. This is the negative control,
    /// and it is what makes every denial below mean something.
    fn outside(cmdline: &str, cwd: &std::path::Path) -> Option<i32> {
        std::process::Command::new("cmd.exe")
            // `raw_arg`, not `args`. `Command` escapes each argument for the
            // *C runtime's* rules, and `cmd.exe` does not follow them: a single
            // argument of `type "C:\...\x.txt"` comes back out re-quoted as
            // `"type \"C:\...\x.txt\""`, which cmd parses as a program called
            // `type "C:\` and reports as a failure. The container runs its line
            // through `CreateProcessW` verbatim, so the control has to be
            // verbatim too — otherwise the two halves are not the same command
            // and the comparison between them means nothing.
            .raw_arg(format!("/c {cmdline}"))
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("the control command must at least run")
            .code()
    }

    /// F1 — an AppContainer can be created and entered on this host.
    #[test]
    fn an_appcontainer_can_be_created_and_entered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (code, out) = in_container(
            "smoke",
            "cmd.exe /c echo io-harness-appcontainer-smoke",
            dir.path(),
        );
        assert_eq!(
            code,
            Some(0),
            "the payload ran inside the container but did not exit 0; output was {out:?}"
        );
        assert!(
            out.contains("io-harness-appcontainer-smoke"),
            "the redirect did not reach the capture file; a handle that is not inheritable \
             produces exactly this — a process that starts, runs and writes nothing. Got {out:?}"
        );
    }

    /// F2 — a payload inside an AppContainer cannot read what it was not granted.
    ///
    /// The secret lives in a *second* temporary directory that nothing grants, so
    /// the only difference between the two runs is the container.
    #[test]
    fn a_payload_cannot_read_what_it_was_not_granted() {
        let work = tempfile::tempdir().expect("tempdir");
        let vault = tempfile::tempdir().expect("tempdir");
        let secret = vault.path().join("secret.txt");
        std::fs::write(&secret, b"io-harness-secret").expect("write the secret");
        let line = format!("cmd.exe /c type \"{}\"", secret.display());

        // The control first. If the operator cannot read their own file, the
        // test can conclude nothing at all about the container.
        assert_eq!(
            outside(&format!("type \"{}\"", secret.display()), work.path()),
            Some(0),
            "the negative control failed: this file is unreadable even outside the container, \
             so a refusal inside it would prove nothing"
        );

        let (code, out) = in_container("read", &line, work.path());
        assert_ne!(
            code,
            Some(0),
            "the container read a file it was never granted; output was {out:?}"
        );
        assert!(
            !out.contains("io-harness-secret"),
            "the container printed the secret's contents: {out:?}"
        );
    }

    /// F3 — a payload inside an AppContainer has no route off the machine.
    ///
    /// Asserted against a socket the payload opens itself, never against the
    /// floor's proxy-environment strip, which is documented best-effort and which
    /// a payload that does not read those variables ignores completely.
    #[test]
    fn a_payload_has_no_route_off_the_machine() {
        let work = tempfile::tempdir().expect("tempdir");
        let probe = "curl.exe -s -m 15 -o NUL https://example.com";

        // The control. A runner with no egress at all cannot demonstrate that
        // the container is what removed it.
        assert_eq!(
            outside(probe, work.path()),
            Some(0),
            "the negative control failed: this host has no outbound network even outside a \
             container (or no curl.exe), so a failure inside one would prove nothing"
        );

        let (code, out) = in_container("net", &format!("cmd.exe /c {probe}"), work.path());
        assert_ne!(
            code,
            Some(0),
            "the container reached the network with an empty capability array; there is no \
             `internetClient` on this profile, so this means the capability set is not being \
             applied. Output was {out:?}"
        );
    }

    /// The measurement `fallback_scope` Trigger B turns on, taken rather than
    /// guessed at.
    ///
    /// `cmd.exe` runs inside a container because Windows puts an
    /// `ALL APPLICATION PACKAGES` ACE on its own system directories. Nothing else
    /// on the machine has one. So the question this release actually has to
    /// answer is not "does a container run a process" — F1 settled that — but
    /// "does a container run *the payload a caller would give it*", which is a
    /// binary in a toolchain nobody blessed.
    ///
    /// The subject is the running test binary itself: a real, freshly compiled
    /// executable under `target\`, with no ACE for any container, which is
    /// exactly the shape of the thing this crate would be asked to sandbox. It
    /// is invoked with a filter that matches no test, so it exits promptly and
    /// runs none of this suite inside a container.
    ///
    /// The grant is deliberately **only its own directory**. If that is enough,
    /// the mechanism scales to real payloads and what is left is deciding which
    /// paths to name. If it is not, the grant set is a discovery problem rather
    /// than a configuration one, and that is the finding.
    #[test]
    fn a_binary_nothing_blessed_runs_once_its_own_directory_is_granted() {
        let exe = std::env::current_exe().expect("the test binary knows where it is");
        let bin_dir = exe.parent().expect("it has a directory").to_path_buf();
        let work = tempfile::tempdir().expect("tempdir");

        let profile = Profile::create(&name("toolchain"), false).expect("profile");
        grant(work.path(), profile.sid(), Access::Full, Reach::Tree).expect("grant the workspace");
        grant(&bin_dir, profile.sid(), Access::ReadExecute, Reach::Tree)
            .expect("grant the binary's directory");

        let out_path = work.path().join("o.txt");
        let file = std::fs::File::create(&out_path).expect("capture");
        // `--list` enumerates and exits; the filter matches nothing, so no test
        // in this suite is re-entered inside a container.
        let line = format!(
            "\"{}\" --list --exact io-harness-no-such-test",
            exe.display()
        );
        let mut child =
            Spawned::start(&line, work.path(), profile.sid(), &file).expect("spawn the payload");
        child.resume().expect("resume the contained process");
        let code = child.wait(60_000).expect("wait");

        let mut text = String::new();
        std::fs::File::open(&out_path)
            .and_then(|mut f| f.read_to_string(&mut text))
            .ok();
        assert_eq!(
            code,
            Some(0),
            "a payload nothing had blessed could not execute inside the container even with \
             its own directory granted read-and-execute. This is fallback_scope Trigger B: the \
             grant set is a discovery problem rather than a configuration one. Output: {text:?}"
        );
    }

    /// **How deep a grant on a directory actually goes**, read off the ACL.
    ///
    /// `grant` adds one inheritable ACE and relies on the system to re-propagate
    /// it to what is already inside — the whole of the fix that took
    /// `windows-latest` from thirty-six failures to thirteen. Every one of the
    /// thirteen that is left has the same shape: the payload it could not reach
    /// is **two** levels under a granted directory (`%TEMP%\.tmpXXXX\ok.bat`,
    /// `<workspace>\src\lib.rs`) while every case that works is one
    /// (`<workspace>\a.rs`, `<bindir>\test.exe`).
    ///
    /// So this measures the depth rather than arguing about it: one file at each
    /// of three levels, the ACL read back at each, and the payload run at each.
    /// A failure here names the level, which is the fact the next fix needs.
    #[test]
    fn a_grant_on_a_directory_reaches_every_depth_under_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let mid = root.path().join("mid");
        let deep = mid.join("deeper");
        std::fs::create_dir_all(&deep).expect("the two directories under the root");

        let payload = |dir: &std::path::Path| {
            let p = dir.join("depth.bat");
            std::fs::write(&p, "@echo off\r\necho io-harness-depth\r\n")
                .expect("write the payload");
            p
        };
        let at0 = payload(root.path());
        let at1 = payload(&mid);
        let at2 = payload(&deep);

        let profile = Profile::create(&name("depth"), false).expect("profile");
        grant(root.path(), profile.sid(), Access::Full, Reach::Tree).expect("grant the root");

        // The ACL first: a payload that fails cannot say whether the ACE was
        // missing or insufficient, and that is the question.
        let masks = [
            granted_mask(&at0, profile.sid()),
            granted_mask(&at1, profile.sid()),
            granted_mask(&at2, profile.sid()),
        ];
        assert!(
            masks.iter().all(Option::is_some),
            "a grant on a directory did not reach every file under it — depth 0/1/2 \
             masks {masks:?}. A `None` names the level the system's re-propagation \
             stopped at, and the level below it is where every remaining Windows \
             failure lives."
        );

        // And then the behaviour, so the ACL is not being read as a proxy for it.
        for (level, script) in [at0, at1, at2].iter().enumerate() {
            let out_path = root.path().join(format!("o{level}.txt"));
            let file = std::fs::File::create(&out_path).expect("capture");
            let line = format!("cmd.exe /c \"{}\"", script.display());
            let mut child = Spawned::start(&line, root.path(), profile.sid(), &file)
                .expect("spawn the payload");
            child.resume().expect("resume");
            let code = child.wait(30_000).expect("wait");
            drop(file);
            let mut text = String::new();
            std::fs::File::open(&out_path)
                .and_then(|mut f| f.read_to_string(&mut text))
                .ok();
            assert_eq!(
                code,
                Some(0),
                "the payload {level} level(s) under the granted directory did not run: {text:?}"
            );
        }
    }

    /// F4 — the wall clock fires, and does not fire on a payload that finishes.
    ///
    /// Both halves, because a ceiling that always fires and a ceiling that never
    /// fires are equally wrong and only one of them looks broken.
    #[test]
    fn the_wall_clock_kills_only_what_overruns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = Profile::create(&name("wall"), false).expect("profile");
        grant(dir.path(), profile.sid(), Access::Full, Reach::Tree).expect("grant");
        let file = std::fs::File::create(dir.path().join("o.txt")).expect("capture");

        // A `cmd` builtin loop, and every obvious alternative is wrong here:
        //
        // - `ping -n 30 127.0.0.1`, the usual Windows sleep, exits *immediately*
        //   inside a container. An AppContainer blocks loopback as well as
        //   egress, so the ping fails rather than waits. That is the boundary
        //   working, and it makes ping useless as a clock.
        // - `timeout /t 30` refuses to run at all when stdin is redirected, and
        //   this spawn always redirects it.
        // - `waitfor` would work but is one more binary to have to be present.
        //
        // A `for /L` loop over a builtin needs no file, no socket and no console,
        // so it is the one sleep that is unaffected by the thing being tested.
        let mut slow = Spawned::start(
            "cmd.exe /c for /L %i in (1,1,2000000000) do @rem",
            dir.path(),
            profile.sid(),
            &file,
        )
        .expect("spawn the slow payload");
        slow.resume().expect("resume the contained process");
        assert_eq!(
            slow.wait(1_000).expect("wait"),
            None,
            "a payload that runs for thirty seconds must be capped by a one-second ceiling"
        );

        let mut quick =
            Spawned::start("cmd.exe /c exit 7", dir.path(), profile.sid(), &file).expect("spawn");
        quick.resume().expect("resume the contained process");
        assert_eq!(
            quick.wait(30_000).expect("wait"),
            Some(7),
            "a payload that finishes inside the ceiling must report its own exit code, not a cap"
        );
    }
}
