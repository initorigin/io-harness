# Driving a browser

*Added in 0.53.0. Behind the `browser` cargo feature, which is not in `default`.*

A run can open a page, use it, and look at what it rendered — and with every
action, read the console output and the uncaught errors that action produced.

This is the first capability in the crate that observes something the crate did
not itself produce, which is why the boundary comes first here and the
convenience second. A browser executes untrusted code from whatever host it lands
on, and one click can navigate anywhere.

## Turning it on

Two switches, both off by default. The cargo feature:

```toml
[dependencies]
io-harness = { version = "0.53", features = ["browser"] }
```

and a browser on the contract, or in the user-scope file — the one scope
`[browser]` may be declared from since 0.74.0:

```rust
use io_harness::{BrowserConfig, TaskContract};

let contract = TaskContract::workspace("check the login page renders", "/repo")
    .with_browser(BrowserConfig::default().with_viewport(1440, 900));
```

```toml
[browser]
binary = "/usr/bin/chromium"   # optional; omitted means the documented list
headless = true
width = 1280
height = 800
timeout_secs = 30
```

A run that enables the feature and configures no browser is byte-identical to one
built before this release: no tool schema, no process, no event.

## The six tools

| Tool | What it does |
| --- | --- |
| `browser_navigate` | Open a URL |
| `browser_read` | The text the page *renders*, after its scripts have run — optionally one element's |
| `browser_screenshot` | A PNG of the viewport, which the model is shown |
| `browser_click` | A trusted click at the element a CSS selector resolves to |
| `browser_type` | Focus an element and type into it |
| `browser_scroll` | Scroll vertically |

There is no seventh tool for the console. Whatever the page logged or threw is
appended to the observation of the action that produced it, because a model
should not have to remember to ask what the page said. An action that produced
nothing says so, rather than omitting the section — a section that disappears when
empty is indistinguishable from one that was never collected.

## Where the boundary is

**Every document navigation is an `Act::Net` check against its `host:port`,
decided at the paused request rather than at the URL a tool was handed.**

That distinction is the whole capability. Checking the URL the model typed is easy
and insufficient: a click on a link, a redirect, and a script assigning `location`
are all navigations the model never typed, and all three are gated by exactly the
same code as the one it did. Each decision is one `BrowserNavigated { host,
permitted }` event, so a trace records every place the browser went **and every
place it was stopped from going**.

**A URL that reaches no host is decided by its scheme, before the navigation is
issued (0.74.0).** Only `http`, `https`, `ws` and `wss` reduce to a `host:port`,
so only they reach the check above; everything else opened no request for the gate
to pause, was therefore permitted by default, and was recorded nowhere. That is how
`browser_navigate` to a `file:` URL read a local file past `Act::Read` and past
every secret deny with no row saying it happened. The rule is an allowlist and not
a list of known-bad schemes: `about:blank` is permitted — it is the empty page the
browser opens on, and a run leaving a page has nowhere else to go — while `file:`,
`data:`, `blob:`, `javascript:` and every scheme nobody has considered are refused.
An unrecognised scheme is not a harmless one.

Each of those is recorded too, and what reaches the trace is the **scheme** and
never the URL: a `data:` URL is its own payload and a `javascript:` URL is a
program, so writing either into the trace and into the model's observation would
copy the thing that was refused into two places it was refused from reaching. This
also closes the subresource question by construction rather than by interception —
`Fetch.enable` pauses documents only, so a `data:` page's `<img>` was never going
to be intercepted; the document simply never loads.

Starting the browser is an `Act::Exec` check on its binary, through the same gate
a configured MCP or language server child goes through. Configuring a browser does
not grant access to it.

Under containment, the browser is pointed at the loopback proxy the run already
owns, so a contained run has one egress path rather than two.

## The transport, and why it is a pipe

The browser is driven over a pipe on the child's own descriptors, not over a
remote debugging port. A debugging port is a TCP listener that **any other process
on the machine** can connect to and drive with complete control of the browser,
including reading whatever the page can read. This crate opens no such port.

It also costs no dependency: NUL-framed JSON over two descriptors needs no
websocket client, no TLS to localhost and no protocol crate. `cargo tree` does not
move.

## The limits, stated plainly

- **Every supported platform drives a browser, since 0.59.0.** The transport is
  the same pipe on all of them and the difference is one function: unix installs
  the two ends as descriptors 3 and 4 in `pre_exec`, and Windows writes them into
  the child's C-runtime descriptor table through `lpReserved2` — because Chromium
  turns the descriptors it is handed into handles with `_get_osfhandle`, and a
  descriptor number is not something a handle list can carry. The pipes are
  anonymous on both, so no other local process has a name to open.
- **Subresources are not individually policy-checked.** Images, stylesheets, fonts
  and XHR are the page's own traffic to a host already permitted. Document
  navigations bound where the browser *goes*. Under containment everything it
  sends takes the run's egress proxy like every other contained command, but a
  run that is not contained does not get a per-asset decision.
- **Nothing is ever downloaded.** No driver, no version manager, no runtime fetch.
  The browser is one already installed, and its absence is a refusal naming what
  was looked for. A binary named in `[browser]` that does not exist is a failure
  rather than a quiet fall back to some other browser — falling back would drive
  something other than what was asked for and make the trace a lie.
- **One page per run.** No tabs, windows, downloads, uploads, PDF printing, device
  emulation, request mocking, or cookie and storage manipulation.
- **No waiting on arbitrary page conditions.** There is no `wait_for_selector` and
  no polling predicate. An action settles on the page's own load state, bounded by
  `timeout_secs`; the bound expiring is a normal outcome that still returns the
  page and says the page had not finished, never an error. A general wait
  primitive invites a run to spend its clock on a page that will never settle.
- **A selector that matches nothing fails, naming the selector.** It is never
  reported as a click that happened. This is the one failure a model genuinely
  cannot detect for itself: it would read a successful result and reason forward
  from a state that never existed.
- **`[browser]` is refused in any file inside the workspace**, like `[[hook]]`,
  because it names a program to execute: `io.toml` arrives with a `git clone`, and
  `io.local.toml` — refused since 0.74.0 — is a path the run's own agent can write.
  Write it in the user-scope file.
- **A screenshot is not free.** Measured on one page: 44 bytes as text against
  13,166 bytes as a PNG. Take one when how the page *looks* is the question —
  text says a heading exists, a screenshot says it is off-screen or behind a
  dialog — and read the text when the content is the question.
