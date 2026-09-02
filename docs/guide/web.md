# Provider-executed web search and fetch

Three of the vendors this crate talks to will run a web search for the model,
inside the completion, and bill it per request. `WebAccess` is the one
vendor-neutral way to ask for that: one declaration on the contract, which each
provider translates into its own shape, plus the citations every one of them
returns and this crate records.

The provider executes it. That single fact decides everything else on this page —
what the domain lists mean, why `Act::Net` never fires, why a citation is a weaker
claim than it looks, and why a paused turn can cost you a search twice.

```rust
use io_harness::{run_with, ApproveAll, Anthropic, Policy, Store, TaskContract,
                 Verification, WebAccess};

let contract = TaskContract::workspace(
    "update the README's install line to the current release",
    "/path/to/repo",
)
.with_verification(Verification::WorkspaceFileContains {
    file: "README.md".into(),
    needle: "install".into(),
})
.with_web(WebAccess::search().max_uses(3).allow("crates.io"));

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;

// What the answer drew on, readable after the process that ran it is gone.
for source in store.citations(result.run_id)? {
    println!("{}", source.url);
}
```

Nothing here is on by default. `WebAccess::default()` is every switch off,
`TaskContract::web` is `None` unless you set it, and a request that declares
nothing sends the body every release before 0.22.0 sent — byte for byte.

## The declaration

```rust
use io_harness::WebAccess;

let web = WebAccess::search()   // search on, fetch off
    .with_fetch()               // and let the provider dial a URL for the model
    .max_uses(5)                // cap the provider-executed requests
    .allow("docs.rs")           // hosts the vendor may reach
    .block("evil.test");        // hosts it may not
```

`search` and `fetch` are separate switches because they are separate decisions.
Search is the half a model reaches for unprompted; fetch is the half a human asks
for by name, pointing the model at a page. `WebAccess::enabled()` is true when
either is on — a declaration carrying only a domain filter declares nothing, and a
boundary around a capability nobody asked for is not a capability.

`max_uses` is the cost control, and it is worth setting. These requests are billed
**per request, not per token**, and a model that likes searching will make several
in one step. The cap is handed to the vendor and enforced by the vendor; the run's
own step, time and token budgets are separate and still apply.

The two domain lists are handed to the vendor's own filter. A vendor takes an
allow-list **or** a block-list, never both, so `vendor_filter()` resolves a
declaration carrying both: the allow-list wins, with anything also blocked removed
from it, which is exactly the set the two lists together described.

```rust
let web = WebAccess::search().allow("docs.rs").allow("evil.test").block("evil.test");
assert_eq!(web.vendor_filter(), (vec!["docs.rs".to_string()], vec![]));
```

## The three vendors, and what each refuses

| | search | fetch | allow-list | block-list |
| --- | --- | --- | --- | --- |
| Anthropic | yes | yes | yes | yes |
| OpenAI (Chat Completions) | yes | **refused** | yes | **refused** |
| OpenRouter | yes | **refused** | **refused** | **refused** |

**Anthropic** gets dated server-tool entries in `tools` —
`web_search_20250305` and `web_fetch_20250910` — each carrying `max_uses` and
whichever domain list survived `vendor_filter`. Fetch is beta-gated, so a request
asking for it carries an `anthropic-beta: web-fetch-2025-09-10` header that a
search-only request does not.

**OpenAI** gets `web_search_options`, with the allow-list as
`filters.allowed_domains` and nothing else. Its filter is allow-list only and its
Chat Completions API has no provider-executed fetch tool.

**OpenRouter** gets a `plugins` entry — `{"id": "web", "max_results": n}`. Its web
plugin takes no domain filter at all, and `max_uses` lands on `max_results`, which
caps the number of *results* returned rather than the number of requests made.

What a vendor cannot express is **refused, not dropped**. Pairing a fetch or a
block-list with a provider that cannot carry it is an `Error::Config` before
anything is sent:

```text
provider "openai" cannot block individual domains — its web search filter is
allow-list only; state the hosts to allow instead of the ones to block. No
request was sent.
```

A boundary silently discarded is worse than no boundary, because the caller
believes in it. The consequence is that a `WebAccess` is not automatically
portable: a declaration written for Anthropic may refuse on OpenRouter, and it
refuses loudly, on the first request, rather than after the run has already
searched everywhere you meant to exclude.

## Projecting a policy onto the vendor's filter

A run that already denies `evil.test` in its `Policy` should not have to write the
same hosts twice. `WebAccess::from_policy` reads the policy's `Act::Net` rules and
fills in the domain lists:

```rust
use io_harness::{Policy, WebAccess};

let policy = Policy::default()
    .layer("app")
    .allow_net("docs.rs:443")
    .allow_net("crates.io:443")
    .deny_net("*.example.com");

let web = WebAccess::search().from_policy(&policy)?;
assert_eq!(web.allowed_domains, ["docs.rs", "crates.io"]);
assert_eq!(web.blocked_domains, ["*.example.com"]);
```

A net rule names a host or a `host:port`; a vendor filter names a host, so the port
is dropped. Three cases do something you would not guess, and each is deliberate.

**An allow-everything rule projects to no filter at all.** `allow_net("*")` is a
policy that permits every host, and the honest projection of that is an empty
`allowed_domains` — meaning "no allow-list", the vendor's default of anywhere. It
is emphatically *not* a one-entry list containing `*`, which no vendor treats as a
wildcard, and it is not "an allow-list that happens to be empty", which a vendor
reads as **allow nothing**. That inversion is the failure this case exists to
prevent: it fails closed and in silence, the search returns nothing, and the model
answers from memory believing it looked.

**An `Ask` net rule is an error.** `ask_net("docs.rs")` cannot be honoured here.
The provider dials the URL, so there is no connection in this process to pause and
no point at which an `Approver` could be consulted. `from_policy` returns
`Error::Config` naming the rule. Allow the host, deny it, or leave the feature off.

**A pattern that is not a bare host is an error.** `https://docs.rs/std` carries a
scheme and a path, and a vendor filter has nowhere to put either — sent as a host
it would match nothing, silently. The error names the offending rule and says to
write `docs.rs` or `docs.rs:443` instead.

## Citations, and what a citation is not

```rust
pub struct Citation {
    pub url: String,
    pub title: Option<String>,
    pub cited_text: Option<String>,
}
```

`url` is the only field every vendor supplies. `title` and `cited_text` are `None`
where the vendor said nothing, rather than empty strings, so "not reported" and
"reported as blank" stay different facts. OpenRouter carries the quoted passage;
OpenAI sends offsets into its own answer and no quote, which stays `None` rather
than being reconstructed from indices into text that may have been truncated.

Citations land in the `citations` table, per run and per step, and are read back
with `Store::citations(run_id)`. A url already recorded for the same run and step
is not written twice — a vendor repeats the same source on every sentence it
supports, and a trace that repeats a row is a trace nobody reads. The same page
cited at two different steps is two rows, because they are two facts about when the
run looked at it.

**A citation is what the provider returned.** This crate does not fetch the URL,
does not check that the page says what the model claimed, and does not rank or
filter what it was given. A citation is therefore evidence about the *provider's
answer*, not about the world: it says "the model was shown this and said it drew on
it". That is a weaker claim than a verified source, and it is the only claim the
crate can honestly make, because verifying it would mean opening the socket the
whole design says this process does not open.

## A search that failed, inside a 200 that succeeded

Every one of these vendors reports a broken search **inside an otherwise successful
HTTP response**, as an error object rather than a transport failure. Parsed naively
that is a search which found nothing — an empty result set, an answer with no
sources, and a model that concludes the web had nothing to say.

`ServerToolCall` is what keeps the two apart:

```rust
use io_harness::ServerToolCall;

let ran   = ServerToolCall::ok("anthropic", "web_search");
let broke = ServerToolCall::failed("anthropic", "web_search", "max_uses_exceeded");

assert!(ran.succeeded());
assert_eq!(broke.error.as_deref(), Some("max_uses_exceeded"));
```

The error code is the vendor's own word for it, recorded rather than normalised,
because the vendor's own documentation is what explains it. A failed call produces
three things:

- a `server_tool_calls` row carrying the code, readable afterwards with
  `Store::server_tool_calls(run_id)`;
- an `EventKind::ServerToolUsed { provider, tool, ok: false }` to any observer,
  as it happens;
- an observation the model reads on its next turn:
  `[step 3] provider web tool: web_search failed (max_uses_exceeded)`.

The observation is the point. Told that the search broke, the model can retry or
answer knowingly; not told, it answers as though the web were empty.

A search that **ran and found nothing** is a successful row with no citations, and
nothing is said to the model — because nothing went wrong. Keeping that distinction
is the whole reason the `error` field exists.

## A paused turn is a continuation, not an ending

Anthropic returns a `pause_turn` stop reason when a server-executed tool has been
running long enough that it would rather hand back control than hold the
connection. The response carries partial text and no tool call — which is exactly
the shape of a *finished* answer to a `Verification::None` contract. A loop that
did not look at the stop reason would end the run there and report success, having
searched for nothing.

So the loop takes another step, and the partial text is an observation like any
other. Both turns are charged, and both appear in `provider_calls`: a paused turn
is a completion like any other, and pretending it was free would understate the
bill.

**What this crate does not do is echo the vendor's partial assistant blocks back.**
The request has been one flattened user turn since 0.1.0 — there is no assistant
message array to append the vendor's in-progress search results to. A paused turn
therefore resumes as a **fresh request**, and the provider **may repeat a search it
already charged you for**. This is a real, current limitation, not a corner case:
a run that pauses twice inside a long search can pay for the same search three
times. Bound it with `max_uses` and with the contract's own token budget, and
prefer a narrow task over an open-ended one when the provider is doing the looking.

## What it costs

`Usage::server_tool_requests` exists since 0.18.0 and reads non-zero for the first
time in this release. It is the count of provider-executed requests a completion
reported, it is billed **per request rather than per token**, and it lands in the
per-call accounting rows beside the token counts.

It is populated where the vendor reports it. Anthropic sends per-tool counters
under `server_tool_use` and the crate sums them. On the OpenAI wire the counter is
read from the same place and neither OpenAI nor OpenRouter reports it in that shape
today, so the meter reads zero there while the invoice does not. A cost derived
from a `PriceTable` is a token cost; a search request is a separate line on the
vendor's bill, and the crate does not invent a price for it.

```rust
let requests: u64 = store.provider_calls(run_id)?
    .iter()
    .filter_map(|c| c.usage)
    .map(|u| u.server_tool_requests)
    .sum();
```

## In `io.toml`

```toml
# The user-scope file. `[web]` is refused in `io.toml` and in `io.local.toml`
# since 0.74.0 — see below.
[web]
search = true
fetch = false
max_uses = 3
allowed_domains = ["docs.rs", "crates.io"]
```

`[web]` is top-level, beside `[[agent]]` and `[[mcp]]`, and `Config::apply_to` puts
it on the contract. The table is carried whenever it is **present**, not only when
a switch is on: a file writing `search = false` is stating a decision, and dropping
it would make the contract say "nothing was configured" instead. An unknown key
inside it is an error naming the key, so a misspelled `blocked_domain` is not a
boundary that quietly did not apply.

**Only the user-scope file may declare it (0.74.0).** `[web]` joined the sections
a file inside the workspace may not write at all, which is the same list
`[[hook]]`, `[browser]`, `[[provider]]`, `[[mcp]]` and `[[lsp]]` are on. The reason
is the fact this whole page turns on: the provider dials the URL, so `Act::Net`
never sees it, the run's egress proxy is not on the path, and the domain lists are
a filter this crate states rather than enforces. A committed `io.toml` writing
`[web] search = true` was therefore a repository switching on an egress surface no
rung of this crate mediates, and `fetch = true` beside it is a repository choosing
where the run's context may be sent. `io.local.toml` is refused for the same
reason it is refused everything else: it sits at the workspace root, which is a
path the run's own agent can write.

The cost is real and is not hidden: `[web] search = false` is a *narrowing*
sentence, and a workspace file can no longer write that either — a whole section
is refused whole, for the reason `[[hook]]` is. It narrows nothing in practice,
because the feature is off unless the user scope turned it on. A per-project
declaration that is the application's rather than the repository's goes on the
`TaskContract` in Rust, with `with_web`.

## Reading it back

Two additive tables, created on open like every addition since 0.13.0.
`CHECKPOINT_FORMAT` stays 7, no existing table is altered, and a 0.21.0 database
opens, resumes and is queried unchanged.

```rust
for source in store.citations(run_id)? {
    println!("{} — {}", source.url, source.title.unwrap_or_default());
}
for call in store.server_tool_calls(run_id)? {
    match &call.error {
        Some(code) => println!("{} {} failed: {code}", call.provider, call.tool),
        None => println!("{} {} ok", call.provider, call.tool),
    }
}
```

A store row that cannot be written is logged and swallowed, exactly as an
accounting row is: failing a run the provider answered because a citation could not
be recorded would trade a real answer for a bookkeeping entry.

## The limits, stated plainly

**The boundary is declared, not enforced by this process.** The provider dials the
URL. This crate opens no socket for a provider-executed search or fetch, so
`Act::Net` never sees one, no `Approver` is consulted, no rule in your `Policy`
refuses a host, and the sandbox is not involved. `allowed_domains` and
`blocked_domains` fill in the *vendor's* filter and are enforced there. That is the
same arrangement already stated for a stdio MCP server: the harness states a
boundary another process enforces, and records what it stated. A caller who needs
the boundary enforced in this process must not turn this on, and should reach for a
tool it executes itself.

**A citation is not verification.** The crate never fetches the cited URL and never
checks the page against what the model said. A `citations` row records what the
provider returned, which is a weaker and more honest claim than a checked source.
Nothing in the run consults citations, and no `RunOutcome` depends on one.

**A paused turn may repeat a search you already paid for.** The crate does not echo
the vendor's partial assistant blocks back, so a `pause_turn` resumes as a fresh
request and the provider may run the same search again. There is no deduplication
and no continuation token. `max_uses` is the only lever.

**A declaration is not portable across providers.** OpenAI refuses a fetch and a
block-list; OpenRouter refuses a fetch and any domain filter at all. Each is an
`Error::Config` before the first request, not a silently narrowed feature — which
means a contract that runs on Anthropic can fail to start on OpenRouter, by design.
A `Fallback` between providers of different capability will refuse on the one that
cannot carry the declaration.

**`max_uses` is the vendor's cap, and it means slightly different things.** On
Anthropic it is `max_uses` on each declared tool entry, so search and fetch each get
the cap rather than sharing one. On OpenRouter it becomes `max_results`, a cap on
results returned rather than requests made. OpenAI's Chat Completions body has no
cap field, so the value is not sent there at all.

**`server_tool_requests` is only as complete as the vendor's reporting.** It is
non-zero on Anthropic. OpenAI and OpenRouter report no such counter in the shape
the crate reads today, so the meter reads zero on those providers even when a
search ran — the `server_tool_calls` rows still say it happened, and the vendor's
bill still says it happened.

**A spawned sub-agent inherits the root's declaration, and only that one.** The
spawn tool copies the root contract's `WebAccess` onto the child and never reads
one from the spawn arguments, so a child searches under exactly the terms its
parent was given and the *model* cannot grant itself web access by asking — the
same rule the toolbox, the skills catalogue and the agent roster already follow.
There is no per-child narrowing: `AgentDef` has no field for it, so a tree in
which the researcher searches and the writer does not is not expressible today.

**A plain session turn has no web access.** `Session::turn` builds its own
contract, exactly as it builds its own responder. Give the turn a `TaskContract`
through `turn_bounded` to declare web access for it.

**A `Provider` that ignores the field is honestly non-searching, not broken.** An
out-of-tree provider written before this release keeps compiling, receives the
declaration, and returns no citations and no server-tool rows. Its runs complete.
Nothing errors on a provider that simply did not search.

**A run recorded before 0.22.0 has no rows in either table.** Nothing is
backfilled, because the facts were never recorded. Both queries return empty rather
than zeros.

## See also

- [MCP and network egress](mcp-and-network.md) — `Act::Net`, and the same
  declared-here-enforced-there boundary stated for a stdio server
- [Permissions and approval](permissions.md) — the layer stack `from_policy`
  projects from, and the `Ask` effect a provider-executed fetch cannot offer
- [Accounting](accounting.md) — the per-call rows `server_tool_requests` lands in,
  and why a cost is derived rather than stored
- [Observability and replay](observability.md) — the `ServerToolUsed` event and the
  rest of the trace
- [Configuration — `io.toml`](configuration.md) — the one scope `[web]` may be
  declared from, and the rule that put it there
- [Sessions](sessions.md) — the bounded turn that can carry a declaration
- [The public contract](../CONTRACT.md)
- [README](../../README.md)
</content>
</invoke>
