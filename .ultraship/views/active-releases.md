<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.46.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** Embed this crate and get a run whose commands are confined to the workspace, without having asked for it — and say so on the page when a run genuinely needs the whole machine.
**Today the default is the widest grant the crate makes, and it is spelled as an absence.** `TaskContract::exec_sandbox` is `Option<SandboxConfig>` and it defaults to `None` (`src/contract.rs:417`, `:481`). `None` means every `exec`, every `shell` stage and every verification command runs at the embedding process's own privileges: it can write anywhere the host user can write, delete anything that user owns, and dial anything the network reaches. 0.40.0 built the machinery to stop that — `with_contained_exec(SandboxConfig)`, the workspace root as workdir, the caps, the backend ladder — and left it opt-in. A grant that large, expressed as a field nobody set, is the one grant a reader is most likely to under-read: nothing in a caller's source says "this run may write to your home directory", because the sentence that would say it is a `None` they never typed.
**So containment becomes the default and the exception becomes a sentence.** `ExecMode` names three: `ReadOnly` — the workspace is readable and no command may write into it; `WorkspaceWrite` — commands may write inside the workspace root and nowhere else, the **default**; and `FullAccess` — the host's own privileges, which is exactly what every release up to 0.45.0 did by default. `TaskContract::with_full_access()` is how a caller asks for the third, and the whole point of the method is that it is legible in a diff and greppable in a repository. A run that needs the machine still gets the machine; what changes is that a reader can see it was chosen.
**A default nobody can build under is a default that gets turned off, so the toolchain's caches come with it.** `cargo`, `npm`, `pnpm`, `go`, `pip`, `uv`, `maven` and `gradle` all write outside the project they are building — a registry cache, a module cache, a build cache — and 0.40.0 recorded exactly this as a limitation: under containment on macOS, a toolchain writing `~/.cargo/registry` or `~/.npm` fails. A default-contained run that cannot run the project's own test command is worse than no default, because it teaches every embedder to reach for `with_full_access()` on the first failure. `Toolchain::cache_dirs` derives the cache roots for the ecosystem `toolchain::detect` (`src/toolchain.rs:219`) already found, honouring the ecosystem's own environment variables, and `WorkspaceWrite` grants them alongside the workspace root and the system temp directory. Only roots that exist on this host are granted, which is not a nicety: the Linux wrapper `fail`s on a bind it cannot perform and that failure degrades the whole backend to the floor (`src/sandbox/linux.rs:145`), so a granted path that is not there would silently unwind the confinement it was added to preserve.
**And the mode reaches the place that enforces it, rather than being re-derived per backend.** `RunSpec` (`src/sandbox.rs:338`) carries the mode and the writable roots to whichever backend `select` returned, so the macOS profile's `(allow file-write* (subpath …))` lines (`src/sandbox/macos.rs:61`) and the Linux setup's bind loop (`src/sandbox/linux.rs:150`) are two renderings of one list rather than two independent opinions about what a mode means.



_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
