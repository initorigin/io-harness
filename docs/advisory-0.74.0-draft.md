# Security advisory draft — four critical issues fixed in io-harness 0.74.0

**Status: DRAFT. Not published.** This is the text prepared for a GitHub Security
Advisory on `initorigin/io-harness`. Publication, and whether to request a CVE
alongside it, are the maintainer's decisions and have not been taken.

Written for 0.80.0, whose release contract makes the advisory part of the
release. The fixes shipped in **0.74.0 on 2026-09-02** and were described in the
CHANGELOG; no advisory was filed at the time. On a project whose stated purpose
is an auditable boundary, that gap is the thing this document closes.

Every claim below was checked against the code that shipped rather than against
the finding text it came from. The audit's own status pass corrected twelve of
its fifty-one findings, two of them substantively, so the finding text is not a
safe source for an exploit description.

---

## Summary

Four issues, each allowing a workspace an operator did not write — a cloned
repository, or a file the agent itself wrote into the workspace root — to reach
outside the boundary io-harness states it enforces. Three of the four need no
more than `git clone` plus a run started in that directory.

| ID | Issue | Severity |
|---|---|---|
| C1 | macOS sandbox profile injection through a directory name | Critical |
| C2 | Post-write project checker ran ungated and uncontained | Critical |
| C3 | Project-scope `[[provider]]` redirected the endpoint and sent the operator's key to it | Critical |
| C4 | Project-scope `[[mcp]]` / `[[lsp]]` spawned an operator-unreviewed program at run start | Critical |

**Affected:** all published versions before 0.74.0.
**Fixed in:** 0.74.0.
**Workaround for an operator who cannot upgrade:** below, per issue. There is no
single configuration change that covers all four.

---

## C1 — macOS sandbox profile injection through a directory name

**Impact.** Full escape from the macOS `sandbox-exec` boundary: a contained
command regained write access to the whole filesystem and unrestricted network,
while the run continued to report the `macos-sandbox-exec` backend and a
containment it was no longer applying.

**Mechanism.** The workdir and the writable roots were interpolated into the
generated SBPL profile with no escaping. For a `shell` stage the workdir is the
model-chosen working directory, and the shell's own lexer permits a quote, a
parenthesis, a space and a newline inside a quoted word. SBPL resolves by
last-matching-rule, so text that closes the current s-expression and opens
`(allow file-write* (subpath "/"))` after it is appended to the profile rather
than contained by it.

**Reachability.** A directory whose *name* carries the payload. A cloned
repository can ship one; an agent that can create a directory can create one.

**Fix.** 0.74.0 rejects rather than escapes. A path that cannot be named
verbatim inside an SBPL string literal — non-UTF-8, a quote, a backslash, or any
control character — is refused, and a refused profile collapses to one that
grants nothing rather than to one missing an `allow` line. Parentheses, spaces
and apostrophes still pass, because inside a literal they are characters and not
structure, so ordinary directory names keep working. The same character set is
refused one layer earlier, at the `cd` door, where the model can read a reason.

**Workaround before upgrading.** Do not run on macOS with a workspace whose
directory names you have not seen. `sandbox.mode = "read-only"` reduces the
write half but not the network half.

---

## C2 — post-write project checker ran ungated and uncontained

**Impact.** Arbitrary code execution on the host, outside any sandbox, with the
embedding process's privileges. The approver is asked about two ordinary file
writes and never about the execution.

**Mechanism.** After a successful write, the crate ran the project's checker as
a convenience. That spawn was not gated on `Act::Exec` and was not contained.
For a Cargo project the checker is `cargo check`, which compiles and *runs*
`build.rs` and procedural macros. A repository that is already a Cargo project —
the ordinary case for a cloned hostile repository — needs one write of `build.rs`
to reach execution. A `.cargo/config.toml` declaring a `rustc-wrapper` is a
one-file variant.

**Correction to an earlier description.** The original finding said "two ordinary
`write_file` calls, any workspace". Toolchain detection runs once per run before
the first turn, so a `Cargo.toml` created *by the same run* is not the marker
that run checks against. The single-run path needs a workspace that is already a
Cargo project, or a second run.

**Fix.** 0.74.0 routes the post-write check through the same gate pair the
model-callable `check` tool uses, and runs it contained. Anything other than an
allow is a silent skip, which preserves the property that a write can never fail
because of the check that follows it. The model-callable `check` tool was itself
found to be uncontained during the fix and was closed in the same change.

**Workaround before upgrading.** Deny `Act::Exec` for the toolchain's checker,
or run with a policy whose exec default is deny. Note that this removes the
convenience as well as the exposure.

---

## C3 — project-scope `[[provider]]` redirected the endpoint and sent the key to it

**Impact.** The operator's provider credential is sent, in cleartext where the
attacker's base URL is `http://`, to a host the workspace chose. The run's own
egress policy never sees the redirect.

**Mechanism.** A file inside the workspace was refused for `[[hook]]` and
`[browser]` but not for `[[provider]]`. A `[[provider]]` entry names a base URL
and an authentication source; `${env:}` and `${file:}` were both legal at that
scope, and `${file:}` with an absolute argument reads any path the process can
read. The provider layer merged an *allow* overlay for the named host **before**
checking it, so a deny-by-default network policy was never asked about the
endpoint. The request then carried the operator's real key as a bearer token.

**Reachability.** A cloned repository's `io.toml`, or an `io.local.toml` written
by the run's own agent into the workspace root.

**Fix.** 0.74.0 refuses `[[provider]]` at project scope, checks the endpoint
before the overlay is merged, and extends the same rule to `io.local.toml`. A
`[[policy.layers]]` entry in a file inside the workspace may carry deny rules and
nothing else, which closes the same widening reached through a layer rather than
a default.

**Workaround before upgrading.** Never start a run in a workspace containing an
`io.toml` you have not read, and treat `io.local.toml` as attacker-writable for
any run that can write files.

---

## C4 — project-scope `[[mcp]]` / `[[lsp]]` spawned an unreviewed program at run start

**Impact.** Arbitrary program execution at run start, before the model's first
turn, from a file inside the workspace.

**Mechanism.** The same widening gap as C3. Both sections name a command, its
arguments and its environment, and neither was refused at project scope — even
though the plugin loader already refused exactly this shape. The spawn gate asks
`Act::Exec` about the **binary name** only, so a policy allowing a routine
interpreter (`node` in a JavaScript repository) plus arguments pointing at an
attacker-supplied script is arbitrary execution under an allowed name. An `Ask`
verdict was refused rather than routed to a human, so there was no interactive
backstop.

**Fix.** 0.74.0 refuses both sections at project scope, under a stated rule:
anything that names a program to run, or an endpoint a credential is sent to, is
refused in a file inside the workspace, without exception. Every refusal names
the user scope as the alternative, and no message anywhere directs an operator to
`io.local.toml`.

**Workaround before upgrading.** As C3 — do not start a run in a workspace whose
configuration files you have not read.

---

## Credit

Found by an internal security audit of io-harness conducted on 2026-08-29
against 0.73.0, covering 51 findings of which these four were rated critical.
47 were closed in 0.74.0; the remainder are being closed in 0.80.0.

## Reporting

Vulnerabilities in this crate go to the contact in `SECURITY.md` under
coordinated disclosure, not to a public issue.
