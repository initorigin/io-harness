//! A small seccomp **deny-list**, installed alongside the Landlock rule set.
//!
//! Named as a deny-list everywhere it is described, because that is what it is
//! and the difference matters. An allow-list is a jail: nothing runs but what
//! was enumerated. This is not that. It refuses a short list of syscalls that
//! would let a payload undo its own confinement or reach into another process,
//! and it says nothing about the thousands it does not name.
//!
//! An allow-list that a real toolchain survives is a research problem with a
//! maintenance cost per architecture and per libc, and its failure mode is a
//! broken build rather than a weakened boundary. That trade was made at
//! specification time and is not re-litigated here.
//!
//! ## What it refuses, and why each one
//!
//! - `mount`, `umount2`, `pivot_root` — change what a path *means*. A payload
//!   that can remount is a payload that can move a granted path over a
//!   forbidden one, which is the confinement undoing itself.
//! - `ptrace`, `process_vm_readv`, `process_vm_writev` — reach into another
//!   process's memory. Landlock governs files and sockets; it says nothing about
//!   a sibling process, and this crate runs many of them concurrently.
//! - `init_module`, `finit_module`, `delete_module`, `kexec_load`,
//!   `kexec_file_load` — load code into the kernel. Nothing a build does.
//! - `bpf` — load a program the kernel runs, including one that could observe
//!   other processes.
//! - `perf_event_open` — a long-standing route to reading memory the caller
//!   should not see.
//!
//! None of these is a syscall a compiler, linker or package manager performs.
//! That is the whole argument for a deny-list being safe here, and it is why the
//! list is short and stays short.
//!
//! ## What it does NOT do
//!
//! It returns `EPERM` rather than killing. A killed process is indistinguishable
//! from a crash in the payload, and the model reading the outcome would be told
//! nothing; a failed call with an errno is diagnosable.
//!
//! And the filter is written in the **host architecture's** syscall numbers. A
//! process running under a foreign personality — 32-bit on a 64-bit kernel — has
//! different numbering, so the filter allows it through rather than denying by
//! coincidence. Stated as a limitation rather than papered over: this layer
//! hardens the common case and is not the boundary. The boundary is the Landlock
//! rule set it is installed beside.

#![cfg(target_os = "linux")]

use std::io;

/// `struct sock_filter` — one BPF instruction.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// `struct sock_fprog` — the program handed to the kernel.
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

// The BPF opcodes this filter uses, from `linux/bpf_common.h`. Four of them.
const LD_W_ABS: u16 = 0x00 | 0x00 | 0x20; // BPF_LD | BPF_W | BPF_ABS
const JMP_JEQ_K: u16 = 0x05 | 0x10 | 0x00; // BPF_JMP | BPF_JEQ | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

/// Byte offsets into `struct seccomp_data`.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;

const RET_ALLOW: u32 = 0x7fff_0000;
/// `SECCOMP_RET_ERRNO` with `EPERM` in the low sixteen bits.
const RET_EPERM: u32 = 0x0005_0000 | (libc::EPERM as u32);

const PR_SET_SECCOMP: libc::c_int = 22;
const SECCOMP_MODE_FILTER: libc::c_int = 2;

/// `AUDIT_ARCH_*` for the architecture this build targets.
///
/// Two entries rather than a table: these are the only two Linux architectures
/// this crate's CI runs, and an architecture with no value here gets no filter
/// at all and reports that it got none.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

/// Whether this build has a filter to install.
///
/// An architecture this module has no `AUDIT_ARCH` for takes the Landlock rung
/// **without** the seccomp layer, which is a weaker thing and is reported as
/// such rather than claimed.
pub(crate) const AVAILABLE: bool = cfg!(any(target_arch = "x86_64", target_arch = "aarch64"));

/// The syscalls this filter refuses.
///
/// Taken from `libc`'s own per-architecture `SYS_*` constants and never written
/// out as numbers. A hand-copied table is the one mistake in this module that
/// would be invisible: a wrong number denies some *other* syscall, on one
/// architecture, and the symptom is a build that fails for no stated reason.
/// Naming them makes a missing constant a compile error instead.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn denied() -> [libc::c_long; 13] {
    [
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
    ]
}

/// Assemble the filter.
///
/// Layout, in order: load the architecture, allow outright if it is not the one
/// these numbers belong to, load the syscall number, compare it against each
/// denied call jumping to the refusal, allow, refuse.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn program() -> Vec<SockFilter> {
    let denied = denied();
    let n = denied.len() as u8;
    let mut prog = Vec::with_capacity(denied.len() + 5);

    prog.push(SockFilter {
        code: LD_W_ABS,
        jt: 0,
        jf: 0,
        k: OFF_ARCH,
    });
    // A foreign personality is allowed through rather than denied by
    // coincidence: its syscall numbers are not these. The jump lands on the
    // `allow` return, which is the second-to-last instruction.
    prog.push(SockFilter {
        code: JMP_JEQ_K,
        jt: 0,
        jf: n + 1,
        k: AUDIT_ARCH,
    });
    prog.push(SockFilter {
        code: LD_W_ABS,
        jt: 0,
        jf: 0,
        k: OFF_NR,
    });

    // Each comparison jumps forward to the refusal, which sits after the
    // remaining comparisons and the `allow`.
    for (i, nr) in denied.iter().enumerate() {
        let remaining = n - 1 - i as u8;
        prog.push(SockFilter {
            code: JMP_JEQ_K,
            jt: remaining + 1,
            jf: 0,
            k: *nr as u32,
        });
    }
    prog.push(SockFilter {
        code: RET_K,
        jt: 0,
        jf: 0,
        k: RET_ALLOW,
    });
    prog.push(SockFilter {
        code: RET_K,
        jt: 0,
        jf: 0,
        k: RET_EPERM,
    });
    prog
}

/// Install the filter on this process and every descendant.
///
/// Called from a `pre_exec` closure, after the Landlock rule set has been
/// applied and after `PR_SET_NO_NEW_PRIVS` — which this needs too, and which
/// Landlock has already set by the time this runs.
///
/// A build for an architecture with no `AUDIT_ARCH` here installs nothing and
/// returns `Ok`, because the alternative is failing every contained run on that
/// architecture to enforce a hardening layer. What must never happen is
/// *claiming* it, and [`AVAILABLE`] is how a caller knows.
///
/// # Safety
///
/// Must be called in a forked child before `exec`.
#[allow(unreachable_code, unused_variables)]
pub(crate) unsafe fn install() -> io::Result<()> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let prog = program();
        let fprog = SockFprog {
            len: prog.len() as u16,
            filter: prog.as_ptr(),
        };
        // SAFETY: `fprog` points at `prog`, which is live for the whole call,
        // and `len` is its true length. `PR_SET_SECCOMP` with
        // `SECCOMP_MODE_FILTER` is the documented install path and needs
        // `no_new_privs`, which the caller set before applying Landlock.
        if libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &fprog as *const SockFprog,
            0,
            0,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The jump arithmetic is the one part of an assembled BPF program that is
    /// silently wrong rather than loudly wrong: a bad offset produces a filter
    /// the kernel accepts and that refuses the wrong thing. So every comparison
    /// is checked to land exactly on the refusal.
    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn every_comparison_jumps_to_the_refusal() {
        let prog = program();
        let deny_at = prog.len() - 1;
        let allow_at = prog.len() - 2;

        assert_eq!(prog[deny_at].code, RET_K);
        assert_eq!(
            prog[deny_at].k, RET_EPERM,
            "the refusal is an errno, not a kill"
        );
        assert_eq!(prog[allow_at].k, RET_ALLOW);

        // The architecture guard lands on `allow`, so a foreign personality is
        // passed through rather than refused by coincidence.
        let arch_guard = 1;
        assert_eq!(arch_guard + 1 + prog[arch_guard].jf as usize, allow_at);

        // Each syscall comparison lands on `deny`.
        for (i, insn) in prog.iter().enumerate() {
            if i <= 2 || i >= allow_at {
                continue;
            }
            assert_eq!(insn.code, JMP_JEQ_K);
            assert_eq!(
                i + 1 + insn.jt as usize,
                deny_at,
                "comparison at {i} does not land on the refusal"
            );
            assert_eq!(
                insn.jf, 0,
                "a non-match falls through to the next comparison"
            );
        }
    }

    /// The list is the list the module documents. A syscall added to the code
    /// without a paragraph saying why is the thing this catches.
    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn the_denied_set_is_the_documented_one() {
        let d = denied();
        assert_eq!(d.len(), 13);
        // Nothing a compiler, linker or package manager calls is in here, and
        // the two that would be catastrophic to include are asserted absent.
        assert!(!d.contains(&libc::SYS_openat));
        assert!(!d.contains(&libc::SYS_execve));
    }
}
