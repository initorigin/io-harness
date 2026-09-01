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
//! - `socket`, and only for a run whose network the rung is restricting — every
//!   family but `AF_UNIX`, and `AF_INET`/`AF_INET6` for anything but a stream.
//!   Landlock's network rights name `bind` and `connect` over **TCP** and
//!   nothing else, so a run that denied egress could still open a datagram
//!   socket and `sendto` any host it liked, with DNS as the channel that needs
//!   nothing installed. This rule is the other half of that denial. A run that
//!   may reach the network keeps every socket it ever had: the filter is built
//!   without the rule for it.
//!
//! None of the first five is a syscall a compiler, linker or package manager
//! performs. That is the whole argument for a deny-list being safe here, and it
//! is why the list is short and stays short. The sixth is the exception, and
//! what it costs a run is stated where the rule is assembled.
//!
//! ## What it does NOT do
//!
//! It returns `EPERM` rather than killing. A killed process is indistinguishable
//! from a crash in the payload, and the model reading the outcome would be told
//! nothing; a failed call with an errno is diagnosable.
//!
//! The filter is written in the **host architecture's** syscall numbers, and a
//! call that does not arrive under that numbering is refused rather than passed
//! through. There are two shapes of it and both once meant that every denied
//! call was allowed: a process running under a foreign personality — a 32-bit
//! ELF on a 64-bit kernel, which `CONFIG_IA32_EMULATION` makes ordinary — reports
//! a different `AUDIT_ARCH` and matched no comparison; and an x32 call reports
//! the host's own `AUDIT_ARCH` with `__X32_SYSCALL_BIT` set on the syscall
//! number, which likewise matched nothing. Refusing both is the fail-closed
//! reading, and its price is concrete: a 32-bit binary cannot run under this
//! rung at all, because every syscall it makes returns `EPERM`.
//!
//! It still says nothing about the thousands of syscalls it does not name. This
//! layer hardens; the boundary is the Landlock rule set it is installed beside.

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

// The BPF opcodes this filter uses, from `linux/bpf_common.h`. Written as their
// composed values with the derivation beside them, rather than as an `|` chain:
// two of the three classes are zero, and an expression like `0x00 | 0x00 | 0x20`
// is `identity_op` and `eq_op` to clippy however well it documents itself.
const LD_W_ABS: u16 = 0x20; // BPF_LD (0x00) | BPF_W (0x00) | BPF_ABS (0x20)
const JMP_JEQ_K: u16 = 0x15; // BPF_JMP (0x05) | BPF_JEQ (0x10) | BPF_K (0x00)
const JMP_JGE_K: u16 = 0x35; // BPF_JMP (0x05) | BPF_JGE (0x30) | BPF_K (0x00)
const ALU_AND_K: u16 = 0x54; // BPF_ALU (0x04) | BPF_AND (0x50) | BPF_K (0x00)
const RET_K: u16 = 0x06; // BPF_RET (0x06) | BPF_K (0x00)

/// Byte offsets into `struct seccomp_data`.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
/// The half of `args[0]` a 32-bit load has to read.
///
/// The six arguments are `__u64` from byte sixteen and a BPF program loads four
/// bytes at a time, so which half carries the value depends on the host's byte
/// order. The kernel takes `socket`'s domain and type as `int` — it reads the
/// same half — so comparing it is comparing what the syscall will act on.
const OFF_ARG0: u32 = if cfg!(target_endian = "big") { 20 } else { 16 };
/// `args[1]`, the argument after it.
const OFF_ARG1: u32 = OFF_ARG0 + 8;

/// `__X32_SYSCALL_BIT`.
///
/// An x32 call arrives with the host's own `AUDIT_ARCH_X86_64` and this bit set
/// on the syscall number, so it passes the architecture guard and then matches
/// no comparison in the table — which is every denied call allowed. Nothing this
/// crate spawns is an x32 binary, so the number is refused outright rather than
/// translated.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// `SOCK_TYPE_MASK`. The `type` argument to `socket` carries `SOCK_NONBLOCK` and
/// `SOCK_CLOEXEC` in its high bits and the socket type in its low four, so the
/// comparison masks before it compares or a `SOCK_DGRAM | SOCK_CLOEXEC` walks
/// past it. Written out because `libc` does not export it — unlike every syscall
/// number here, which is taken from `libc` and never written out.
const SOCK_TYPE_MASK: u32 = 0xf;

/// How many instructions the socket rule adds, when a run gets it.
const SOCKET_RULE: u8 = 8;

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

/// One BPF instruction, positionally. The four fields have no useful names at a
/// call site that is already a table.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn insn(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Assemble the filter.
///
/// Layout, in order: load the architecture, refuse outright if it is not the one
/// these numbers belong to, load the syscall number, refuse an x32 number, compare
/// it against each denied call jumping to the refusal, the socket rule when the
/// run has one, allow, refuse.
///
/// `net_restricted` is the run's own answer and not a policy this module holds:
/// it is true for a run that denies egress and for one routed through the
/// loopback proxy, which are the two runs whose outbound TCP the Landlock rule
/// set beside this filter takes control of. Landlock covers TCP and this covers
/// the rest, so the two have to be given the same answer.
///
/// **What the socket rule costs a run that gets it.** Every family but
/// `AF_UNIX` is refused, so no `AF_NETLINK` — glibc's `getaddrinfo` probes the
/// local interfaces over one, treats the failure as "both families present" and
/// carries on — no `AF_PACKET`, no `AF_VSOCK`, and no loopback UDP between two
/// processes of the payload's own. Local IPC over `AF_UNIX` is untouched, and so
/// is a stream socket over `AF_INET`/`AF_INET6`: a proxied run reaches its proxy
/// through one, and a run that denied egress has `connect` and `bind` refused by
/// the rule set anyway.
///
/// The jump arithmetic is written as a distance to one of the last two
/// instructions, so every offset is `n + sock` away from a position that does not
/// move. `tests::every_comparison_jumps_to_the_refusal` asserts each one lands.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn program(net_restricted: bool) -> Vec<SockFilter> {
    let denied = denied();
    let n = denied.len() as u8;
    let sock = if net_restricted { SOCKET_RULE } else { 0 };
    // The `allow` is at `n + sock + 4` and the refusal at `n + sock + 5`, which
    // is what every jump below counts towards.
    let mut prog = Vec::with_capacity(denied.len() + sock as usize + 6);

    prog.push(insn(LD_W_ABS, 0, 0, OFF_ARCH));
    // A foreign personality is refused rather than passed through: its syscall
    // numbers are not these, so a filter that let it past denied nothing at all.
    // The jump lands on the refusal, which is the last instruction.
    prog.push(insn(JMP_JEQ_K, 0, n + sock + 3, AUDIT_ARCH));
    prog.push(insn(LD_W_ABS, 0, 0, OFF_NR));
    // An x32 number carries this architecture and matches nothing in the table,
    // which was the same hole wearing the other face. Refused before the table
    // is consulted.
    prog.push(insn(JMP_JGE_K, n + sock + 1, 0, X32_SYSCALL_BIT));

    // Each comparison jumps forward to the refusal, which sits after the
    // remaining comparisons, the socket rule and the `allow`.
    for (i, nr) in denied.iter().enumerate() {
        prog.push(insn(JMP_JEQ_K, n + sock - i as u8, 0, *nr as u32));
    }

    if net_restricted {
        // Eight instructions, and where each jump lands — `A` is the allow and
        // `D` the refusal, both after the block:
        //
        //   0  is this `socket`?                       no  -> A
        //   1  load the family
        //   2  AF_UNIX?                                yes -> A
        //   3  AF_INET?                                yes -> 5
        //   4  AF_INET6?                               no  -> D
        //   5  load the type
        //   6  mask off SOCK_NONBLOCK and SOCK_CLOEXEC
        //   7  SOCK_STREAM?                            yes -> A, no -> D
        prog.push(insn(JMP_JEQ_K, 0, 7, libc::SYS_socket as u32));
        prog.push(insn(LD_W_ABS, 0, 0, OFF_ARG0));
        prog.push(insn(JMP_JEQ_K, 5, 0, libc::AF_UNIX as u32));
        prog.push(insn(JMP_JEQ_K, 1, 0, libc::AF_INET as u32));
        prog.push(insn(JMP_JEQ_K, 0, 4, libc::AF_INET6 as u32));
        prog.push(insn(LD_W_ABS, 0, 0, OFF_ARG1));
        prog.push(insn(ALU_AND_K, 0, 0, SOCK_TYPE_MASK));
        prog.push(insn(JMP_JEQ_K, 0, 1, libc::SOCK_STREAM as u32));
    }

    prog.push(insn(RET_K, 0, 0, RET_ALLOW));
    prog.push(insn(RET_K, 0, 0, RET_EPERM));
    prog
}

/// Install the filter on this process and every descendant.
///
/// Called from a `pre_exec` closure, after the Landlock rule set has been
/// applied and after `PR_SET_NO_NEW_PRIVS` — which this needs too, and which
/// Landlock has already set by the time this runs.
///
/// `net_restricted` is the rule set's own answer — `Plan::restricts_network` —
/// and it decides whether the socket rule is part of the filter. Passing `false`
/// for a run whose network the rule set is restricting leaves that run every
/// datagram socket it asks for, which is the hole the rule exists to close, so
/// the two are read from the same plan at every call site.
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
pub(crate) unsafe fn install(net_restricted: bool) -> io::Result<()> {
    if !AVAILABLE {
        // Never claim a filter that was not installed. The rung is still the
        // Landlock rule set, which is the boundary; this layer is hardening, and
        // an architecture without a table gets the boundary and not the
        // hardening. Said once per process rather than per command.
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            tracing::warn!(
                "sandbox: no seccomp syscall table for this architecture, so the Landlock \
                 rung is applied WITHOUT the deny-list; the filesystem boundary is unaffected"
            )
        });
        return Ok(());
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let prog = program(net_restricted);
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
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod tests {
    use super::*;

    /// `AUDIT_ARCH_I386`, which is neither of the two this module has a table
    /// for. Any value that is not [`AUDIT_ARCH`] would do; this is the one a
    /// stock `CONFIG_IA32_EMULATION=y` kernel actually produces.
    const FOREIGN_ARCH: u32 = 0x4000_0003;

    /// `struct seccomp_data` as the kernel hands it to a filter: the syscall
    /// number, the architecture, the instruction pointer, six arguments.
    fn call(nr: u32, arch: u32, args: [u64; 6]) -> [u8; 64] {
        let mut d = [0u8; 64];
        d[0..4].copy_from_slice(&nr.to_ne_bytes());
        d[4..8].copy_from_slice(&arch.to_ne_bytes());
        for (i, a) in args.iter().enumerate() {
            d[16 + i * 8..24 + i * 8].copy_from_slice(&a.to_ne_bytes());
        }
        d
    }

    /// One syscall as this build's architecture sees it.
    fn native(nr: libc::c_long, args: [u64; 6]) -> [u8; 64] {
        call(nr as u32, AUDIT_ARCH, args)
    }

    /// Run the filter the way the kernel would and answer with the `SECCOMP_RET_*`
    /// it returns.
    ///
    /// Twenty lines of interpreter buy the one thing a shape assertion cannot: a
    /// verdict, for a call the host never has to be able to make. It is also the
    /// only way any of this is provable on a build host that is not Linux at all
    /// — which this module's own architecture gate keeps it from being — so it
    /// is at least provable on every Linux leg without a capable kernel.
    fn verdict(prog: &[SockFilter], data: &[u8; 64]) -> u32 {
        let word = |k: u32| {
            let k = k as usize;
            u32::from_ne_bytes(data[k..k + 4].try_into().unwrap())
        };
        let mut acc: u32 = 0;
        let mut pc = 0usize;
        for _ in 0..prog.len() {
            let op = prog[pc];
            match op.code {
                LD_W_ABS => {
                    acc = word(op.k);
                    pc += 1;
                }
                ALU_AND_K => {
                    acc &= op.k;
                    pc += 1;
                }
                JMP_JEQ_K => pc += 1 + usize::from(if acc == op.k { op.jt } else { op.jf }),
                JMP_JGE_K => pc += 1 + usize::from(if acc >= op.k { op.jt } else { op.jf }),
                RET_K => return op.k,
                other => panic!("an opcode the interpreter does not know: {other:#x}"),
            }
        }
        panic!("the filter ran off its own end without returning")
    }

    /// The jump arithmetic is the one part of an assembled BPF program that is
    /// silently wrong rather than loudly wrong: a bad offset produces a filter
    /// the kernel accepts and that refuses the wrong thing. So every comparison
    /// is checked to land exactly on the refusal, in both shapes of the filter.
    #[test]
    fn every_comparison_jumps_to_the_refusal() {
        for net_restricted in [false, true] {
            let prog = program(net_restricted);
            let deny_at = prog.len() - 1;
            let allow_at = prog.len() - 2;
            let sock = if net_restricted {
                SOCKET_RULE as usize
            } else {
                0
            };

            assert_eq!(prog[deny_at].code, RET_K);
            assert_eq!(
                prog[deny_at].k, RET_EPERM,
                "the refusal is an errno, not a kill"
            );
            assert_eq!(prog[allow_at].k, RET_ALLOW);

            // M7 — the architecture guard lands on the *refusal*. It landed on
            // the allow until 0.74.0, which passed every foreign-personality
            // call through the filter untouched.
            let arch_guard = 1;
            assert_eq!(
                arch_guard + 1 + prog[arch_guard].jf as usize,
                deny_at,
                "a foreign personality is refused, not passed through"
            );
            // M7 — and so does the x32 guard, which sits after the syscall
            // number is loaded and before the table is consulted.
            let x32_guard = 3;
            assert_eq!(prog[x32_guard].code, JMP_JGE_K);
            assert_eq!(prog[x32_guard].k, X32_SYSCALL_BIT);
            assert_eq!(x32_guard + 1 + prog[x32_guard].jt as usize, deny_at);

            // Each syscall comparison lands on `deny`.
            for (i, cmp) in prog.iter().enumerate().take(allow_at - sock).skip(4) {
                assert_eq!(cmp.code, JMP_JEQ_K);
                assert_eq!(
                    i + 1 + cmp.jt as usize,
                    deny_at,
                    "comparison at {i} does not land on the refusal"
                );
                assert_eq!(
                    cmp.jf, 0,
                    "a non-match falls through to the next comparison"
                );
            }
        }
    }

    /// The list is the list the module documents. A syscall added to the code
    /// without a paragraph saying why is the thing this catches.
    #[test]
    fn the_denied_set_is_the_documented_one() {
        let d = denied();
        assert_eq!(d.len(), 13);
        // Nothing a compiler, linker or package manager calls is in here, and
        // the two that would be catastrophic to include are asserted absent.
        assert!(!d.contains(&libc::SYS_openat));
        assert!(!d.contains(&libc::SYS_execve));
    }

    /// M7 — an x32 call reports this architecture and a syscall number no
    /// comparison in the table matches, so before 0.74.0 every denied call made
    /// with `__X32_SYSCALL_BIT` set was allowed. `mount` is the one that matters
    /// most: a payload that can remount can move a granted path over a forbidden
    /// one.
    #[test]
    fn m7_an_x32_syscall_number_is_refused() {
        for net_restricted in [false, true] {
            let prog = program(net_restricted);
            let nr = libc::SYS_mount as u32 | X32_SYSCALL_BIT;
            let x32 = call(nr, AUDIT_ARCH, [0; 6]);
            assert_eq!(
                verdict(&prog, &x32),
                RET_EPERM,
                "an x32 `mount` is refused, not allowed by failing to match"
            );
            // The bit is a floor and not an equality: every number above it is
            // outside the table this filter was written for.
            let high = call(X32_SYSCALL_BIT, AUDIT_ARCH, [0; 6]);
            assert_eq!(verdict(&prog, &high), RET_EPERM);
            // And an ordinary number is still read by the table below it.
            assert_eq!(verdict(&prog, &native(libc::SYS_openat, [0; 6])), RET_ALLOW);
        }
    }

    /// M7 — a 32-bit ELF on a 64-bit kernel reports its own `AUDIT_ARCH` and its
    /// own numbering, which matched nothing here and was allowed wholesale.
    /// `CONFIG_IA32_EMULATION=y` is a stock Ubuntu kernel, so this is not an
    /// exotic host.
    ///
    /// The refusal is the whole filter and not one syscall: such a binary cannot
    /// run at all under this rung, which is the fail-closed side of the trade and
    /// is documented as one.
    #[test]
    fn m7_a_foreign_architecture_is_refused_wholesale() {
        let prog = program(false);
        for nr in [libc::SYS_mount as u32, libc::SYS_openat as u32, 1, 11] {
            assert_eq!(
                verdict(&prog, &call(nr, FOREIGN_ARCH, [0; 6])),
                RET_EPERM,
                "syscall {nr} under a foreign personality must be refused"
            );
        }
    }

    /// H9 — the hole this rule closes. Landlock's network rights cover `bind`
    /// and `connect` over TCP and nothing else, so a run that denied egress could
    /// open a datagram socket and `sendto` any host it liked; DNS needs nothing
    /// installed. Refusing the socket is where that stops.
    #[test]
    fn h9_a_network_restricted_run_gets_no_datagram_socket() {
        let prog = program(true);
        let socket = |domain: libc::c_int, ty: libc::c_int| {
            native(libc::SYS_socket, [domain as u64, ty as u64, 0, 0, 0, 0])
        };

        for ty in [libc::SOCK_DGRAM, libc::SOCK_RAW] {
            assert_eq!(
                verdict(&prog, &socket(libc::AF_INET, ty)),
                RET_EPERM,
                "an AF_INET socket of type {ty} is refused"
            );
            assert_eq!(verdict(&prog, &socket(libc::AF_INET6, ty)), RET_EPERM);
        }
        // The flags ride in the same argument as the type, so the comparison
        // masks before it compares — this is the call a payload actually makes.
        assert_eq!(
            verdict(
                &prog,
                &socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC)
            ),
            RET_EPERM,
            "SOCK_CLOEXEC must not walk a datagram socket past the type check"
        );
        // Every other family is refused too, including the two that reach past
        // this host — `AF_PACKET` at the wire and `AF_VSOCK`, which is 40 and
        // talks to a hypervisor — and anything a kernel adds after this is
        // written.
        let vsock: libc::c_int = 40;
        for family in [libc::AF_NETLINK, libc::AF_PACKET, vsock] {
            assert_eq!(verdict(&prog, &socket(family, libc::SOCK_DGRAM)), RET_EPERM);
        }
    }

    /// H9's other half, and the reason the rule is not a blanket one: local IPC
    /// still works, a proxied run can still reach its proxy, and a run that may
    /// use the network keeps every socket it ever had.
    #[test]
    fn h9_the_sockets_a_restricted_run_still_needs_are_allowed() {
        let restricted = program(true);
        let socket = |domain: libc::c_int, ty: libc::c_int| {
            native(libc::SYS_socket, [domain as u64, ty as u64, 0, 0, 0, 0])
        };

        // Local IPC, which every toolchain that talks to a daemon needs.
        assert_eq!(
            verdict(&restricted, &socket(libc::AF_UNIX, libc::SOCK_DGRAM)),
            RET_ALLOW
        );
        assert_eq!(
            verdict(&restricted, &socket(libc::AF_UNIX, libc::SOCK_STREAM)),
            RET_ALLOW
        );
        // The stream socket a proxied run connects to its proxy with. The rule
        // set decides which port it may reach; this layer only says it may exist.
        assert_eq!(
            verdict(&restricted, &socket(libc::AF_INET, libc::SOCK_STREAM)),
            RET_ALLOW
        );
        assert_eq!(
            verdict(
                &restricted,
                &socket(libc::AF_INET6, libc::SOCK_STREAM | libc::SOCK_CLOEXEC)
            ),
            RET_ALLOW
        );
        // And the rest of the filter is unchanged by the rule sitting after it.
        assert_eq!(
            verdict(&restricted, &native(libc::SYS_mount, [0; 6])),
            RET_EPERM
        );
        assert_eq!(
            verdict(&restricted, &native(libc::SYS_openat, [0; 6])),
            RET_ALLOW
        );

        // A run whose network is not this rung's business is not given the rule
        // at all: it resolves names and opens datagram sockets as before.
        let open = program(false);
        assert_eq!(
            verdict(&open, &socket(libc::AF_INET, libc::SOCK_DGRAM)),
            RET_ALLOW,
            "a run that may reach the network keeps its datagram sockets"
        );
        assert_eq!(open.len() + SOCKET_RULE as usize, restricted.len());
    }
}
