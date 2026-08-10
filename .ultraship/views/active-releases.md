<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.47.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** Embed this crate on a Linux host and get the containment the API promises — including on the distribution that refuses the primitive the backend reaches for first — or be told, in the trace and in the agent's own prompt, exactly which weaker thing was applied instead.
**0.46.0 made containment the default and shipped with two hosts where the default enforces nothing.** `docs/CONTRACT.md:983` states it as a table: Linux confines writes and denies egress, Windows does neither. The Linux row carries its own footnote — the backend needs an unprivileged user namespace, a stock Ubuntu 24.04 ships `kernel.apparmor_restrict_unprivileged_userns=1`, `ubuntu-latest` is one, and every contained run there takes `Backend::PortableFloor`: resource caps, an ephemeral working directory, and a proxy-environment strip a payload that does not read those variables ignores completely. Both are reported rather than hidden, which is the one thing 0.40.0 and 0.46.0 got right and is also the whole of what they got. **This release closes the Linux row.**
**The Windows row stays open, deliberately and with a destination.** Windows access confinement was specified as the other half of this release and was taken out of it whole on 2026-08-10, on the owner's decision, after ten matrix rounds on `windows-latest` failed to converge from a development host that cannot run the platform. It is 0.59.0 now, and the record is `US-IO-HARNESS-0.47.0-I01`. A Windows run keeps exactly what 0.46.0 gave it — a Job Object, which contains resources and not access — `select` returns `WindowsJobObject` as it has since 0.24.0, and the platform table says "native resource containment only" because that is what is true. `sandbox::appcontainer` remains in the tree, built and unit-tested and selected by nothing, which is the state it has been in since 0.26.0.
**On Linux the backend becomes a chain, and the first rung needs no namespace at all.** Landlock is an unprivileged LSM: a process builds a filesystem ruleset, calls `landlock_restrict_self`, and the restriction applies to it and every descendant, with no user namespace, no mount namespace and no setuid helper anywhere in the path. It is exactly the primitive the current backend's failure mode is missing — Ubuntu restricts the namespace and ships Landlock enabled — and it is applied in the child between fork and exec, so the rung is a `pre_exec` closure through the shared runner rather than a wrapper process. That removes two classes of defect this crate has already paid for: there is no wrapper whose own failure has to be told apart from the payload's (`wrapper_failure`, `src/sandbox/linux.rs:111`), and there is no wrapper that `cd`s on the run's behalf, which is what silently beat `Command::current_dir` for every `shell` stage in 0.46.0. Where the host's kernel has Landlock's network rules (ABI 4 and later) an egress-denying run gets a refused `connect`, which is a kernel boundary and not an environment strip. Below the rungs it cannot serve, the chain falls through: a mount-namespacing helper (`bwrap`) where the host has one and it works, the existing `unshare` backend after that, and the portable floor last — each probed by *attempting the thing*, never by reading a sysctl or a capability list, because 0.40.0's Linux breakage survived three matrix runs behind exactly that mistake.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
