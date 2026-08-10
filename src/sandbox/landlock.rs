//! Linux Landlock — the **namespace-free** rung, and the first one the chain
//! offers.
//!
//! Landlock is an unprivileged LSM. A process describes a filesystem ruleset,
//! calls `landlock_restrict_self`, and from that moment the restriction applies
//! to it and to every descendant it ever forks. No user namespace, no mount
//! namespace, no setuid helper: which is the entire reason this rung exists.
//! [`super::linux`] needs an unprivileged user namespace, a stock Ubuntu 24.04
//! ships `kernel.apparmor_restrict_unprivileged_userns=1` and refuses one, and
//! `ubuntu-latest` is a stock Ubuntu 24.04 — so on the commonest Linux CI image
//! in the world every contained run took the portable floor and the confinement
//! this crate documents was applied nowhere. The same host ships Landlock
//! enabled.
//!
//! ## There is no wrapper process, and that is worth two paragraphs
//!
//! Every other unix rung spawns something else — `sandbox-exec`, `unshare`,
//! `bwrap` — which then spawns the payload. This one installs the restriction
//! **in the child, between fork and exec**, through the `pre_exec` hook the
//! shared runner already exposes as its `configure` closure. The argv spawned is
//! the argv the caller asked for.
//!
//! Two defects this crate has already paid for cannot recur here. A wrapper's
//! own setup failure has to be told apart from the payload's failure, which is
//! what [`super::linux::wrapper_failure`] exists for and what made the 0.40.0
//! Linux breakage need a CI log to diagnose. And a wrapper that `cd`s on the
//! run's behalf beat `Command::current_dir` in 0.46.0: `MOUNT_SETUP` ends
//! `cd "$wd"; exec "$@"`, inside the namespace and after the spawn, so a `shell`
//! stage whose own `cd src` was expressed through `current_dir` ran in the
//! workspace root instead. Here `current_dir` means what it says, because
//! nothing between the caller and the payload is a program.
//!
//! ## What the rung grants
//!
//! The same list the mount setup binds, rendered as path rules instead of
//! mounts: read and execute over `/`, and the full write set over the run's own
//! writable roots — the workdir when the [`ExecMode`] grants it and never under
//! [`ExecMode::ReadOnly`], the roots the run resolved, and the system temporary
//! directory. A unix run has never confined *reads* (the macOS profile allows
//! them, and a mount namespace remounts `/` read-only rather than unreadable),
//! so granting `/` read-and-execute is this rung expressing the same claim and
//! not a weakening of it.
//!
//! ## ABI negotiation, and why there is no kernel-version table
//!
//! Requesting an access right the running kernel does not know fails the whole
//! ruleset creation, so the supported version is asked for
//! (`LANDLOCK_CREATE_RULESET_VERSION`) and every request is masked down to it.
//! That is the documented forward-compatible shape and it means this module
//! never has to know which distribution ships which kernel.
//!
//! Egress is the one thing the mask cannot paper over. `CONNECT_TCP` arrives at
//! ABI 4, so on an older kernel this rung confines the filesystem and can say
//! nothing about the network — and [`super::linux::rung`] therefore refuses to
//! hand it a run that denies egress, rather than letting it report a boundary it
//! did not apply.

// On a host that is not Linux this module is compiled for its tests and for
// nothing else — `landlock_run` is a stub returning `None` — so the whole
// portable half reads as dead code there. Allowed for that target only, so that
// a genuinely unused constant on Linux is still a warning.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::ExecMode;

// Filesystem access rights, in the order the kernel defines them. Written out
// rather than pulled from a crate: the whole rung has to fit inside the standing
// no-new-dependency constraint, and these are a stable kernel UAPI.
pub(crate) const FS_EXECUTE: u64 = 1 << 0;
pub(crate) const FS_WRITE_FILE: u64 = 1 << 1;
pub(crate) const FS_READ_FILE: u64 = 1 << 2;
pub(crate) const FS_READ_DIR: u64 = 1 << 3;
pub(crate) const FS_REMOVE_DIR: u64 = 1 << 4;
pub(crate) const FS_REMOVE_FILE: u64 = 1 << 5;
pub(crate) const FS_MAKE_CHAR: u64 = 1 << 6;
pub(crate) const FS_MAKE_DIR: u64 = 1 << 7;
pub(crate) const FS_MAKE_REG: u64 = 1 << 8;
pub(crate) const FS_MAKE_SOCK: u64 = 1 << 9;
pub(crate) const FS_MAKE_FIFO: u64 = 1 << 10;
pub(crate) const FS_MAKE_BLOCK: u64 = 1 << 11;
pub(crate) const FS_MAKE_SYM: u64 = 1 << 12;
/// ABI 2.
pub(crate) const FS_REFER: u64 = 1 << 13;
/// ABI 3.
pub(crate) const FS_TRUNCATE: u64 = 1 << 14;
/// ABI 5.
pub(crate) const FS_IOCTL_DEV: u64 = 1 << 15;

/// Network access rights, ABI 4 and later.
pub(crate) const NET_BIND_TCP: u64 = 1 << 0;
pub(crate) const NET_CONNECT_TCP: u64 = 1 << 1;

/// What a *read-only* hierarchy is allowed: look at it and run what is in it.
pub(crate) const READ_SET: u64 = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;

/// Every filesystem right this module ever asks the kernel to handle, before
/// masking to the host's ABI.
const ALL_FS: u64 = FS_EXECUTE
    | FS_WRITE_FILE
    | FS_READ_FILE
    | FS_READ_DIR
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM
    | FS_REFER
    | FS_TRUNCATE
    | FS_IOCTL_DEV;

/// The filesystem rights an ABI of `abi` knows about.
///
/// Masking rather than a kernel-version table: `landlock_create_ruleset` refuses
/// the whole request if it carries one bit the kernel has never heard of, so
/// every release of this crate has to run on every kernel newer than 5.13
/// without being taught about it.
pub(crate) fn fs_rights_for(abi: u32) -> u64 {
    let mut mask = ALL_FS;
    if abi < 5 {
        mask &= !FS_IOCTL_DEV;
    }
    if abi < 3 {
        mask &= !FS_TRUNCATE;
    }
    if abi < 2 {
        mask &= !FS_REFER;
    }
    mask
}

/// The network rights to handle for a run, which is nothing at all unless the
/// run denies egress *and* the kernel is new enough to enforce it.
///
/// A run that permits egress handles no network rights, so this rung adds no
/// network behaviour to it — the crate's one authority on the network is the
/// run's own `Policy`, and a rung that quietly restricted a permitted run would
/// be a second one.
pub(crate) fn net_rights_for(abi: u32, deny_egress: bool) -> u64 {
    if deny_egress && abi >= super::linux::LANDLOCK_NET_ABI {
        NET_BIND_TCP | NET_CONNECT_TCP
    } else {
        0
    }
}

/// One path rule: a hierarchy and what may be done beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathRule {
    pub(crate) path: PathBuf,
    pub(crate) rights: u64,
}

/// The whole rule set for one run, decided before anything is opened.
///
/// A plain value with no file descriptors in it, so the decision — which paths,
/// which rights, masked to which ABI — is testable on a host that has never
/// heard of Landlock. That is most of what can go wrong here, and it is the half
/// that does not need a matrix round to find out about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) handled_fs: u64,
    pub(crate) handled_net: u64,
    pub(crate) rules: Vec<PathRule>,
    /// TCP ports this run may `connect` to (0.48.0). Empty is every release
    /// before it: net access is handled or it is not, and handling it with no
    /// rule denies every outbound connection.
    ///
    /// **Port-scoped and not address-scoped, and that is the honest word for
    /// it.** Landlock's network rules take a port and no address, so allowing the
    /// run's loopback proxy through also allows any other host on that same port
    /// number. The port is ephemeral and chosen per run, which narrows it in
    /// practice and does not make it a proof — `docs/CONTRACT.md` says so rather
    /// than letting a reader assume this is per-host enforcement.
    pub(crate) net_ports: Vec<u16>,
}

/// Build the rule set for a run.
///
/// `writable` is the run's already-resolved writable roots — exists-filtered by
/// the caller, since 0.46.0, for a reason that applies here too: a path that is
/// not there cannot be opened, and a rung that failed on one would degrade the
/// confinement it was added to preserve.
pub(crate) fn plan(
    abi: u32,
    mode: ExecMode,
    deny_egress: bool,
    workdir: &Path,
    writable: &[PathBuf],
    tmp: &Path,
    proxy_port: Option<u16>,
) -> Plan {
    let handled_fs = fs_rights_for(abi);
    // A proxy means the same thing to this rung as a denial does — take control
    // of outbound TCP — and then hands one port back. Without the first half the
    // second would be a rule on an access nothing was restricting.
    let handled_net = net_rights_for(abi, deny_egress || proxy_port.is_some());
    let net_ports: Vec<u16> = if handled_net == 0 {
        Vec::new()
    } else {
        proxy_port.into_iter().collect()
    };

    // Read and execute over the whole tree. A unix run has never confined reads,
    // and this is that same claim in this rung's vocabulary.
    let mut rules = vec![PathRule {
        path: PathBuf::from("/"),
        rights: READ_SET & handled_fs,
    }];

    let mut write_roots: Vec<PathBuf> = Vec::new();
    // The workdir is writable only when the mode grants it, which is what makes
    // `ReadOnly` a mode here rather than a label: the process still runs in the
    // workspace and still cannot write to it.
    if mode != ExecMode::ReadOnly {
        write_roots.push(workdir.to_path_buf());
    }
    write_roots.extend(writable.iter().cloned());
    // The system temporary directory, unconditionally. Not a convenience: it is
    // the same allowance the macOS profile makes for `/private/var/folders` and
    // the mount setup makes for `${TMPDIR:-/tmp}`, and without it most
    // toolchains fail on their first temporary file.
    write_roots.push(tmp.to_path_buf());

    for root in write_roots {
        rules.push(PathRule {
            path: root,
            rights: handled_fs,
        });
    }
    Plan {
        handled_fs,
        handled_net,
        rules,
        net_ports,
    }
}

#[cfg(target_os = "linux")]
pub(crate) use imp::{abi, restrict_self, Ruleset};

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;

    use super::*;

    const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
    const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
    const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

    /// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
    /// returns the ABI the running kernel supports.
    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;

    /// `LANDLOCK_RULE_NET_PORT` (0.48.0). Added only for the loopback proxy's
    /// port, and only where the negotiated ABI carries the network rules.
    ///
    /// **Port-scoped, not address-scoped**, because that is the whole of what the
    /// kernel interface offers: the rule names a port and no address, so another
    /// host on that port number is reachable. The port is ephemeral and chosen
    /// per run, which narrows it in practice and is not a proof —
    /// `docs/CONTRACT.md` says so per backend rather than letting a reader assume
    /// this is per-host enforcement.
    const RULE_NET_PORT: i32 = 2;

    /// `LANDLOCK_RULE_PATH_BENEATH`. It is the first rule type this module adds:
    /// the network half is expressed by *handling* the network rights and
    /// granting no port when nothing is proxied.
    const RULE_PATH_BENEATH: i32 = 1;

    /// The ABI 4 layout: filesystem rights, then network rights.
    ///
    /// ABI 6 appends a third `scoped` field this module does not use, and an
    /// ABI 1–3 kernel knew only the first. Both directions are fine and neither
    /// needs a version table, because Landlock uses the kernel's extensible-struct
    /// convention: the size is passed alongside the pointer, a **shorter** struct
    /// than the kernel knows is zero-extended, and a **longer** one is accepted
    /// only if every byte the kernel does not understand is zero. Handing an
    /// older kernel these sixteen bytes with `handled_access_net` at zero is
    /// therefore the documented thing to do, and is exactly what a run that does
    /// not deny egress asks for anyway.
    #[repr(C)]
    #[derive(Default)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    // `landlock_path_beneath_attr` is a **packed** struct in the UAPI: twelve
    // bytes, not sixteen. A `#[repr(C)]` here would pad `parent_fd` to an
    // eight-byte boundary and the kernel would read the fd out of padding, which
    // fails as `EINVAL` on a call that looks correct.
    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: RawFd,
    }

    /// `struct landlock_net_port_attr` (0.48.0): two `__u64` fields and no
    /// padding question, unlike its path sibling — the kernel takes the port as a
    /// 64-bit value in host byte order rather than as a `__u16`.
    #[repr(C)]
    struct NetPortAttr {
        allowed_access: u64,
        port: u64,
    }

    /// The kernel's Landlock ABI, or `None` when this host has no usable
    /// Landlock.
    ///
    /// **This asks the LSM rather than reading `/sys/kernel/security/lsm`**, so
    /// a kernel built with Landlock and booted without it in `lsm=` answers
    /// honestly. What it deliberately does *not* do is apply a restriction:
    /// `landlock_restrict_self` would confine the harness process itself and
    /// every run it ever hosts. So this probe proves the interface exists and
    /// [`Ruleset::build`] proves a real rule set can be created; that a rule set
    /// *enforces* is proven by a test that attempts a write, which is the only
    /// place it can be proven at all.
    pub(crate) fn abi() -> Option<u32> {
        static ABI: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
        *ABI.get_or_init(|| {
            // SAFETY: the documented version query — a null attribute pointer, a
            // zero size, and the version flag. It creates nothing and changes
            // nothing about this process.
            let v = unsafe {
                libc::syscall(
                    SYS_LANDLOCK_CREATE_RULESET,
                    std::ptr::null::<RulesetAttr>(),
                    0usize,
                    LANDLOCK_CREATE_RULESET_VERSION,
                )
            };
            if v <= 0 {
                return None;
            }
            let abi = v as u32;
            // Creating a real rule set is where a kernel that answers the
            // version query and still cannot serve one fails, and it costs one
            // file descriptor that is closed immediately.
            let attr = RulesetAttr {
                handled_access_fs: fs_rights_for(abi),
                handled_access_net: 0,
            };
            // SAFETY: `attr` is a live, fully-initialised value of exactly the
            // size passed, and the flags are zero as the kernel requires when
            // creating rather than querying.
            let fd = unsafe {
                libc::syscall(
                    SYS_LANDLOCK_CREATE_RULESET,
                    &attr as *const RulesetAttr,
                    size_of::<RulesetAttr>(),
                    0u32,
                )
            };
            if fd < 0 {
                return None;
            }
            // SAFETY: `fd` is a descriptor this call just produced and nothing
            // else owns.
            unsafe { libc::close(fd as RawFd) };
            Some(abi)
        })
    }

    /// A built rule set, ready to be applied in a child.
    ///
    /// Built in the **parent**, deliberately. Everything expensive and
    /// allocating — resolving paths, opening a descriptor per hierarchy, adding
    /// the rules — happens before the fork, so what runs between fork and exec
    /// is two syscalls with no allocation in sight. A `pre_exec` closure runs in
    /// a freshly forked child where only async-signal-safe work is sound, and
    /// "open a `Vec` of `CString`s" is not that.
    pub(crate) struct Ruleset {
        fd: OwnedFd,
    }

    impl Ruleset {
        /// Create the rule set described by `plan`, or fail — in which case the
        /// caller takes the next rung down rather than running unconfined.
        pub(crate) fn build(plan: &Plan) -> io::Result<Self> {
            let attr = RulesetAttr {
                handled_access_fs: plan.handled_fs,
                handled_access_net: plan.handled_net,
            };
            // SAFETY: `attr` is live and fully initialised, and its size is what
            // is passed. Flags are zero, which is what creating (rather than
            // querying) requires.
            let fd = unsafe {
                libc::syscall(
                    SYS_LANDLOCK_CREATE_RULESET,
                    &attr as *const RulesetAttr,
                    size_of::<RulesetAttr>(),
                    0u32,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the syscall returned a descriptor this process now owns
            // and nothing else holds.
            let ruleset = Ruleset {
                fd: unsafe { OwnedFd::from_raw_fd(fd as RawFd) },
            };

            for rule in &plan.rules {
                ruleset.add_path(&rule.path, rule.rights & plan.handled_fs)?;
            }
            // Handling `CONNECT_TCP` and `BIND_TCP` while permitting no port is
            // precisely how Landlock spells "no route out" — the denial is the
            // absence of a permission, the same shape as an empty network
            // namespace and as the Windows container's empty capability array.
            //
            // 0.48.0 hands exactly one port back: the run's loopback proxy, which
            // is the only route out a proxied run has. Nothing else is ever added.
            for port in &plan.net_ports {
                ruleset.add_net_port(*port, plan.handled_net & NET_CONNECT_TCP)?;
            }
            Ok(ruleset)
        }

        /// Permit `connect` to one TCP port.
        fn add_net_port(&self, port: u16, rights: u64) -> io::Result<()> {
            if rights == 0 {
                return Ok(());
            }
            let attr = NetPortAttr {
                allowed_access: rights,
                // The kernel takes the port as a 64-bit value in host byte order.
                port: u64::from(port),
            };
            // SAFETY: `attr` is live and fully initialised for the whole call and
            // the size passed is its own.
            let rc = unsafe {
                libc::syscall(
                    SYS_LANDLOCK_ADD_RULE,
                    self.fd.as_raw_fd(),
                    RULE_NET_PORT,
                    &attr as *const NetPortAttr,
                    0u32,
                )
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        fn add_path(&self, path: &Path, rights: u64) -> io::Result<()> {
            if rights == 0 {
                return Ok(());
            }
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::other("a granted path contained an interior NUL"))?;
            // `O_PATH` because the descriptor is only ever a *name* for the
            // hierarchy: it is never read from or written to, and `O_PATH`
            // succeeds on a directory this process may not otherwise open.
            // SAFETY: `c` is a live NUL-terminated path for the whole call.
            let dir = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if dir < 0 {
                return Err(io::Error::last_os_error());
            }
            let attr = PathBeneathAttr {
                allowed_access: rights,
                parent_fd: dir,
            };
            // SAFETY: `attr` is live and fully initialised, `dir` is open for
            // the duration of the call, and the size passed is the packed
            // twelve-byte layout the kernel expects.
            let rc = unsafe {
                libc::syscall(
                    SYS_LANDLOCK_ADD_RULE,
                    self.fd.as_raw_fd(),
                    RULE_PATH_BENEATH,
                    &attr as *const PathBeneathAttr,
                    0u32,
                )
            };
            // SAFETY: `dir` is this function's own descriptor and this is its
            // only close.
            unsafe { libc::close(dir) };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// The raw descriptor, for the `pre_exec` closure that applies it.
        pub(crate) fn raw(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    /// Apply a rule set to *this* process and every descendant it forks.
    ///
    /// Called only from a `pre_exec` closure, in the child, after the fork and
    /// before the exec. Two syscalls and no allocation, which is the whole
    /// reason [`Ruleset::build`] does its work in the parent.
    ///
    /// `PR_SET_NO_NEW_PRIVS` is not optional decoration: `landlock_restrict_self`
    /// refuses without it. It is also the release's first syscall-level
    /// restriction in its own right — a payload that cannot gain privileges
    /// through a setuid binary cannot walk out of the ruleset through one.
    ///
    /// # Safety
    ///
    /// Must be called in a forked child before `exec`, with `fd` still open.
    pub(crate) unsafe fn restrict_self(fd: RawFd) -> io::Result<()> {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, fd, 0u32) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(test)]
    mod layout {
        use super::*;

        /// The one layout mistake in this module that would not look like a
        /// layout mistake.
        ///
        /// `landlock_path_beneath_attr` is packed. A `#[repr(C)]` version of it
        /// is sixteen bytes, with `parent_fd` sitting after four bytes of
        /// padding — so the kernel reads the descriptor out of the padding and
        /// the call fails `EINVAL` while every line of it reads correctly.
        /// Twelve bytes, asserted, is cheaper than diagnosing that.
        #[test]
        fn the_path_rule_attribute_is_packed() {
            assert_eq!(size_of::<PathBeneathAttr>(), 12);
            assert_eq!(align_of::<PathBeneathAttr>(), 1);
        }

        /// The ABI 4 rule-set attribute, whose size is passed to the kernel and
        /// therefore has to be the size the kernel is told about.
        #[test]
        fn the_ruleset_attribute_is_the_abi_4_layout() {
            assert_eq!(size_of::<RulesetAttr>(), 16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The masking is the whole of the forward-compatibility story, so each
    /// boundary is asserted rather than the general shape.
    #[test]
    fn each_abi_gets_exactly_the_rights_its_kernel_knows() {
        // ABI 1 predates REFER, TRUNCATE and IOCTL_DEV.
        let one = fs_rights_for(1);
        assert_eq!(one & FS_REFER, 0);
        assert_eq!(one & FS_TRUNCATE, 0);
        assert_eq!(one & FS_IOCTL_DEV, 0);
        assert_ne!(one & FS_WRITE_FILE, 0, "the write set is ABI 1");

        assert_ne!(fs_rights_for(2) & FS_REFER, 0);
        assert_eq!(fs_rights_for(2) & FS_TRUNCATE, 0);

        assert_ne!(fs_rights_for(3) & FS_TRUNCATE, 0);
        assert_eq!(fs_rights_for(3) & FS_IOCTL_DEV, 0);

        // ABI 4 adds network rights, not filesystem ones.
        assert_eq!(fs_rights_for(4), fs_rights_for(3));

        assert_ne!(fs_rights_for(5) & FS_IOCTL_DEV, 0);
        // A kernel newer than anything this module knows must not be asked for
        // a bit this module invented.
        assert_eq!(fs_rights_for(9), fs_rights_for(5));
    }

    /// The egress half of the honesty rule, at the layer that builds the
    /// request. `super::linux::rung` refuses to give this rung an
    /// egress-denying run below ABI 4; this asserts that even if it did, the
    /// rung would not *claim* a network boundary.
    #[test]
    fn network_rights_are_asked_for_only_when_they_can_be_enforced() {
        assert_eq!(net_rights_for(3, true), 0, "no network rules before ABI 4");
        assert_eq!(
            net_rights_for(4, false),
            0,
            "a permitted run is not touched"
        );
        assert_eq!(
            net_rights_for(4, true),
            NET_BIND_TCP | NET_CONNECT_TCP,
            "an egress-denying run on a capable kernel handles both"
        );
        assert_eq!(net_rights_for(6, true), NET_BIND_TCP | NET_CONNECT_TCP);
    }

    fn roots() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/home/u/.cargo"),
            PathBuf::from("/home/u/.npm"),
        ]
    }

    /// F4/F5's host-free half: the workdir leads the writable set, the resolved
    /// roots follow it, the temp directory is always there, and `/` is
    /// read-and-execute and nothing more.
    #[test]
    fn the_plan_grants_the_same_list_the_mount_setup_binds() {
        let p = plan(
            5,
            ExecMode::WorkspaceWrite,
            false,
            Path::new("/w"),
            &roots(),
            Path::new("/tmp"),
            None,
        );
        let full = fs_rights_for(5);

        assert_eq!(p.rules[0].path, Path::new("/"));
        assert_eq!(
            p.rules[0].rights, READ_SET,
            "the tree is readable and executable and nothing else"
        );
        assert_eq!(p.rules[0].rights & FS_WRITE_FILE, 0);

        let writable: Vec<&Path> = p.rules[1..].iter().map(|r| r.path.as_path()).collect();
        assert_eq!(
            writable,
            [
                Path::new("/w"),
                Path::new("/home/u/.cargo"),
                Path::new("/home/u/.npm"),
                Path::new("/tmp"),
            ]
        );
        assert!(p.rules[1..].iter().all(|r| r.rights == full));
    }

    /// F5 — `ReadOnly` withholds the workspace and keeps the temp directory,
    /// which is the mode's entire difference.
    #[test]
    fn read_only_withholds_the_workdir_and_keeps_the_temp_directory() {
        let p = plan(
            5,
            ExecMode::ReadOnly,
            false,
            Path::new("/w"),
            &[],
            Path::new("/tmp"),
            None,
        );
        let writable: Vec<&Path> = p.rules[1..].iter().map(|r| r.path.as_path()).collect();
        assert_eq!(
            writable,
            [Path::new("/tmp")],
            "a read-only run may still open a temporary file and may not write the workspace"
        );
        // And the workspace is still *readable*, through the `/` rule.
        assert_eq!(p.rules[0].path, Path::new("/"));
        assert_ne!(p.rules[0].rights & FS_READ_FILE, 0);
    }

    /// 0.48.0 — a proxy takes control of outbound TCP and hands exactly one port
    /// back. Port-scoped, never address-scoped: that is the ceiling of the
    /// kernel interface and it is asserted here so nobody reads the rung as
    /// per-host enforcement.
    #[test]
    fn a_proxy_handles_the_network_and_allows_only_its_port() {
        // ABI 4 is where the network rules arrive.
        let with = plan(
            4,
            ExecMode::WorkspaceWrite,
            false,
            Path::new("/w"),
            &[],
            Path::new("/tmp"),
            Some(54321),
        );
        assert_ne!(
            with.handled_net, 0,
            "a proxied run restricts outbound TCP even though its policy permits egress"
        );
        assert_eq!(with.net_ports, vec![54321]);

        // Without one, nothing changes from 0.47.0: a run permitting egress
        // handles no network access at all.
        let without = plan(
            4,
            ExecMode::WorkspaceWrite,
            false,
            Path::new("/w"),
            &[],
            Path::new("/tmp"),
            None,
        );
        assert_eq!(without.handled_net, 0);
        assert!(without.net_ports.is_empty());

        // And below the ABI that carries the network rules there is nothing to
        // hand back, so the port is dropped rather than requested and refused.
        let old = plan(
            3,
            ExecMode::WorkspaceWrite,
            false,
            Path::new("/w"),
            &[],
            Path::new("/tmp"),
            Some(54321),
        );
        assert_eq!(old.handled_net, 0);
        assert!(old.net_ports.is_empty());
    }

    /// Every right named in a rule must be one the rule set said it handles, or
    /// the kernel refuses the rule. Cheap, and it is what a new right added to
    /// `READ_SET` without being added to `ALL_FS` would fail.
    #[test]
    fn no_rule_asks_for_a_right_the_ruleset_does_not_handle() {
        for abi in 1..=6 {
            let p = plan(
                abi,
                ExecMode::WorkspaceWrite,
                true,
                Path::new("/w"),
                &roots(),
                Path::new("/tmp"),
                None,
            );
            for rule in &p.rules {
                assert_eq!(
                    rule.rights & !p.handled_fs,
                    0,
                    "abi {abi}: rule for {:?} asks for an unhandled right",
                    rule.path
                );
            }
        }
    }
}
