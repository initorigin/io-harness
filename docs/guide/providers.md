# Providers: one type for every OpenAI-shaped endpoint

`openai.rs` is 159 lines and `openrouter.rs` is 161, and they are the same file
apart from four strings: the endpoint, the label in the trace, the web flavour,
and two environment variable names. Everything that does work — the request body,
the SSE parsing, the tool-call accumulation — already lived in a shared module
neither of them owns.

So a third OpenAI-shaped vendor is not a file. It is a base URL, an auth style, a
key and a model name, and twenty-one of them are a table. `Compatible` is that
one type, and the vendors are rows.

```rust
use io_harness::{run, Compatible, Store, TaskContract, Verification};

# async fn demo() -> io_harness::Result<()> {
// A hosted vendor, by name. The preset supplies the base URL and the auth style;
// you supply the key and the model, because a guessed model slug is a wrong
// model that ships quietly.
let provider = Compatible::groq(std::env::var("GROQ_API_KEY").unwrap(), "llama-3.3-70b-versatile");

let contract = TaskContract::new(
    "add a hello function returning 42",
    "src/hello.rs",
    Verification::FileContains("fn hello".into()),
);
let result = run(&contract, &provider, &Store::memory()?).await?;
println!("{:?}", result.outcome);
# Ok(()) }
```

And the half that costs nothing to run, which is the same three lines with the
key removed:

```rust
use io_harness::Compatible;

// A model on this machine. No key, no bill, no network beyond localhost — the
// shape to develop a contract against before pointing it at a vendor.
let local = Compatible::ollama("llama3.2");
```

Nothing else in the crate changed to make either work. `Compatible` is a
`Provider` like the three that came before it, so [fallback](resilience.md),
[composition](composition.md), the store, the policy and every tool are the same
code they were.

## The twenty-one presets

Each is a named constructor over one row of a preset table, so a misspelled
vendor is a compile error rather than a runtime `Result`. Hosted vendors take a
key and a model; a local runtime takes a model, because it has no credential to
give.

| Hosted — `Compatible::vendor(key, model)` | Local — `Compatible::runtime(model)` |
| --- | --- |
| `cerebras` `deepseek` `fireworks` `gemini` `groq` `minimax` `mistral` `moonshot` `perplexity` `qwen` `together` `xai` `zhipu` | `jan` `koboldcpp` `llamacpp` `lmstudio` `localai` `ollama` `sglang` `vllm` |

The split is what `Auth` exists for:

```rust
use io_harness::{Auth, Compatible};

assert_eq!(Compatible::groq("gsk-...", "llama-3.3-70b-versatile").auth(), &Auth::Bearer);
assert_eq!(Compatible::ollama("llama3.2").auth(), &Auth::None);
```

`Auth::None` sends **no credential header at all**, rather than an empty bearer —
a wire difference several local runtimes reject outright. `Auth` is
`#[non_exhaustive]`: Azure's `api-key` header is a foreseeable third variant and
this is what lets it arrive without a break.

A caller choosing a vendor at run time — from a configuration file, say — reaches
for `Compatible::preset` instead, which takes the name as a string and fails
naming the presets that do exist:

```rust
use io_harness::Compatible;

let p = Compatible::preset("groq", "gsk-...", "llama-3.3-70b-versatile")?;
let e = Compatible::preset("grok", "k", "m").unwrap_err();  // lists all twenty-one
# Ok::<(), io_harness::Error>(())
```

A local runtime on a port of your own, a proxy, or a gateway is
`Compatible::new`, which is the constructor the presets are built from:

```rust
use io_harness::{Auth, Compatible};

let own = Compatible::new("http://10.0.0.4:8000/v1", Auth::None, "", "my-model")
    .with_name("lab");
```

`with_name` replaces the label recorded in the trace. A preset sets its own
vendor name — `groq`, `ollama`, `zhipu` — so `runs.provider` and every
`provider_calls` row say which vendor served rather than `compatible`. A bare
`Compatible::new` is `"compatible"` until you say otherwise, which is honest
about what is known. `with_timeout` replaces the request deadline and rebuilds
the client, so call it before handing the provider to a run.

## A base URL is not a scheme and a host

Six of the twenty-one are not `https://<host>/v1`, and they do not agree on what
they are instead:

| Vendor | Base |
| --- | --- |
| Groq | `https://api.groq.com/openai/v1` |
| Fireworks | `https://api.fireworks.ai/inference/v1` |
| Qwen | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` |
| Zhipu | `https://open.bigmodel.cn/api/paas/v4` |
| Gemini | `https://generativelanguage.googleapis.com/v1beta/openai` |
| Perplexity | `https://api.perplexity.ai` — no version segment at all |

So `base` is **the whole prefix the vendor documents**, and `/chat/completions`
and `/models` are appended to it. A field that assumed `/v1` would silently drop
the rest and 404 against six of the presets. `Compatible::base()` reads it back,
and a trailing slash is trimmed at construction so a base written either way
produces the same URL.

## The local half costs nothing

Eight of the presets are runtimes that run on the machine you are already sitting
at, and each base is that project's own documented default bind:

| Runtime | Default base |
| --- | --- |
| Ollama | `http://localhost:11434/v1` |
| llama.cpp | `http://localhost:8080/v1` |
| LocalAI | `http://localhost:8080/v1` |
| vLLM | `http://localhost:8000/v1` |
| LM Studio | `http://localhost:1234/v1` |
| Jan | `http://localhost:1337/v1` |
| KoboldCpp | `http://localhost:5001/v1` |
| SGLang | `http://localhost:30000/v1` |

A runtime bound somewhere else is `Compatible::new` with the URL, which is one
line and not a release of this crate:

```rust
use io_harness::{Auth, Compatible};

let local = Compatible::new("http://localhost:9001/v1", Auth::None, "", "my-model");
```

What this buys is not a cheaper provider. It is **the whole harness for nothing**:
the [sandbox](sandbox.md), the [policy boundary](permissions.md), the [durable
trace](durable-runs.md), [sub-agents](composition.md) and [hooks](hooks.md) are
the same code against a model on your own laptop as against a vendor — no key, no
invoice, and nothing leaving the machine. It is the cheapest way to find out
whether this crate is for you, and a truer first run than a key and a hope.

## What is not reachable, and what nearly is

`Compatible::gemini` is Gemini's **OpenAI-compatibility endpoint**, not its
native `interactions` API. The native one is a different request and response
shape, and sending this crate's one wire at it would fail rather than degrade.

**AWS Bedrock, Google Vertex and Azure OpenAI are not reachable here.** Bedrock
signs each request with SigV4, Vertex exchanges a service-account JWT, and Azure
authenticates with an `api-key` header against a per-deployment URL — none of
which is a bearer token in a header this provider sends.

The first two are nonetheless reachable **by an application that mints the token
itself**: an app that already holds AWS or Google credentials can obtain a
short-lived bearer token by its own means and pass it as the key, with the
region-specific base URL. What this crate declines to do is grow a signing
dependency per cloud. That is the "no dependency per vendor" property the whole
design rests on, and it is kept by construction here rather than by vigilance.

## There is no `from_env`

`OpenRouter`, `Anthropic` and `OpenAi` each have one, so its absence on
`Compatible` will look like an oversight. It is not: **twenty-one vendor variable
names would be twenty-one guesses this crate made on the operator's behalf**, and
a guessed variable that is unset is a provider that fails to build for a reason
nobody wrote down.

The operator's own file names the variable instead, and has been able to since
0.19.0:

```toml
# The user-scope file. `[[provider]]` is refused in `io.toml` and in
# `io.local.toml` since 0.74.0: `base_url` redirects every completion of the run
# and `api_key` decides which of this host's secrets is sent with it, and the
# endpoint is contacted before the run's first step.
[[provider]]
kind = "compatible"
preset = "groq"                     # or base_url = "http://10.0.0.4:8000/v1", never both
model = "llama-3.3-70b-versatile"
api_key = "${env:GROQ_API_KEY}"     # resolved from the environment, at load
```

`${env:...}` is the interpolation `io.toml` already had, so the key is not in the
file and the variable is named by the person who chose it. `preset` and
`base_url` are exactly-one — an entry with neither and an entry with both are
each refused naming the entry's index — and `name`, `auth` and
`reference_prices` are the optional rest. See
[Configuration](configuration.md#kind--compatible--any-openai-shaped-endpoint-0290)
for the whole table, and the section above it for how `Config::provider_spec()`
and `Config::fallback_specs()` hand a chain back.

## The catalogue

`Provider::models()` asks a provider what it can run.

```rust
use io_harness::Provider;

# async fn demo(provider: &impl Provider) -> io_harness::Result<()> {
for m in provider.models().await? {
    println!(
        "{} — {:?} ctx, {:?} out, images {:?}, tools {:?}, price {:?} from {:?}",
        m.id, m.context_length, m.max_output_tokens,
        m.accepts_images, m.accepts_tools, m.price, m.price_source,
    );
}
# Ok(()) }
```

Every field but `id` is an `Option` or an empty `Vec`, and the rule throughout is
that **`None` means the vendor did not say**. `accepts_images: None` and
`Some(false)` are different facts; so are `price: None` and a stated zero.

The method is **defaulted to an empty list** on the `Provider` trait, and that is
a decision rather than a convenience. The trait is this crate's one extension
point, and its own doc example is a user-written `impl Provider` — so a required
method would have broken every out-of-tree provider *and* the documentation that
invited people to write one. Defaulted, a provider written before this release
keeps compiling and honestly reports no catalogue. `Compatible` reads the
vendor's own `/models` once per instance and caches it, because `Provider::models`
takes `&self` and nothing else.

`accepts_tools: Some(true)` is the vendor's claim about the *model*. It is not a
promise that the *server* was started in a configuration that emits tool calls —
see the limits below, which is where that distinction costs someone a run.

## Reference prices, and where a number came from

`GET /v1/models` is near-universal and returns *identifiers*. OpenAI, Anthropic,
Groq, DeepSeek, Mistral, Fireworks and every local runtime return no cost data
whatsoever, so a catalogue built from the vendor alone prices almost nothing.

One aggregator publishes per-token pricing for most of the models those vendors
serve, needs no key, and is free to read. `Reference` is that lookup, and it is
**opt-in**:

```rust
use io_harness::{Compatible, Reference};

let provider = Compatible::deepseek("sk-...", "deepseek-chat")
    .with_reference_prices(Reference::new());

// The default source, and a mirror — an internal copy or an air-gapped one is
// configuration rather than a fork.
assert_eq!(Reference::new().host(), Some("openrouter.ai"));
let mirror = Reference::at("https://models.internal.example/v1/models");
assert_eq!(mirror.host(), Some("models.internal.example"));
```

Nothing is baked into this repository: no price constants, no per-vendor mapping
file, no list to update when a vendor moves a number. That is the same argument
[accounting](accounting.md) makes for shipping no prices at all — a number
compiled into this crate is stale the moment a vendor moves, and a number fetched
at run time has no shelf life.

Every price says its own origin:

```rust
use io_harness::PriceSource;

# fn caveat(source: &PriceSource) -> String {
match source {
    PriceSource::Vendor => "the vendor's own published rate".into(),
    PriceSource::Reference(host) => format!("{host}'s rate to serve this model, not the vendor's"),
    _ => "an origin this build does not know".into(),
}
# }
```

`price_source` is `Some` exactly when `price` is `Some`. A vendor that stated its
own price keeps it — **including a stated zero**, which is what a local runtime
reports and which is a fact rather than a gap.

### Matching is exact, or one documented normalisation

The reference says `deepseek/deepseek-chat` where DeepSeek's own API says
`deepseek-chat`, so the lookup tries the exact slug (case-insensitively) and then
one rule: **drop a single leading `vendor/` segment**. One, not a family. Each
additional rule widens what counts as a match, and every widening is another
chance to price a request against the wrong model.

A miss stays `None`. Never a nearest guess — a wrong match is a wrong invoice and
is worse than the gap it filled, and
[`Spend::unpriced_calls`](accounting.md#cost-is-derived-never-stored) already
reports that gap honestly. Two reference entries that normalise to the same key
resolve to **nothing** rather than to whichever was seen first; both stay
reachable by their exact slugs, because the ambiguity is in the normalisation and
not in the catalogue.

## Prices by prompt length

A long-context model is usually not sold at one rate. On the day this release was
cut, **44 of the 336 models the default reference catalogue carried priced by
prompt length**, and the rate typically **doubles** past 200,000 tokens — so a
long agentic run priced against the base row reports about half of what it cost.

`google/gemini-2.5-pro` doubles past 200,000 prompt tokens and
`anthropic/claude-sonnet-4.5` steps at the same floor; `qwen/qwen3-max` steps
twice, at 32,000 and again at 128,000. A long agentic run — a repository in
context, a conversation twenty steps deep — is exactly the shape that crosses
those floors, which is why a base row on its own is not a price.

```rust
use io_harness::pricing::{Price, PriceTable, PriceTier};
use io_harness::Usage;

let base = Price { input: 1_250_000, output: 10_000_000, ..Price::ZERO };
let prices = PriceTable::new("2026-08-01")
    .with("some-vendor/long-context", base)
    .with_tiers(
        "some-vendor/long-context",
        vec![PriceTier {
            min_prompt_tokens: 200_000,
            price: Price { input: 2_500_000, output: 15_000_000, ..Price::ZERO },
        }],
    );

let short = Usage { prompt_tokens: 100_000, total_tokens: 100_000, ..Default::default() };
let long  = Usage { prompt_tokens: 400_000, total_tokens: 400_000, ..Default::default() };

assert_eq!(prices.cost_micros("some-vendor/long-context", &short), Some(125_000));
assert_eq!(prices.cost_micros("some-vendor/long-context", &long), Some(1_000_000));
```

Three properties, each of which someone gets wrong by assumption:

- The threshold is compared against `Usage::prompt_tokens`, and **the highest
  tier reached prices the whole request**. That is how the vendors bill it:
  crossing the line re-rates everything, rather than charging the first tranche
  at the old rate and the remainder at the new one. 400k at $2.50/M is $1.00, not
  200k at each rate.
- A tier's `price` is a **complete** `Price`, never a patch over the base. A tier
  naming only the dimensions it changed would silently price the others at zero,
  so a tier read from the catalogue is merged over the base before it is stored.
- **A model with no tiers prices exactly as it did before tiers existed.**
  `PriceTable::tiers` returns an empty slice for it, `with_tiers(model, vec![])`
  removes them, and a table serialized by 0.28.0 still deserializes.

`PriceTable::tiers(model)` reads them back, lowest threshold first, sorted here
rather than trusted from the caller so the order they were written in cannot
decide which tier applies.

## A price table dated by the moment it was read

```rust
use io_harness::Compatible;

# async fn demo() -> io_harness::Result<()> {
let provider = Compatible::deepseek("sk-...", "deepseek-chat")
    .with_reference_prices(io_harness::Reference::new());

let prices = provider.price_table().await?;
println!("prices as of {}", prices.as_of());
# Ok(()) }
```

This is where derived cost stops depending on a table an operator maintains by
hand with an `as_of` they have to remember to update. The date is the fetch
instant rather than a number typed by a human, which is the whole reason a
run-time catalogue beats a compiled-in price list.

A model the vendor did not price is **absent from the table**, so
`Spend::unpriced_calls` counts it. It is never entered at zero.

## Asking a model to think harder (0.31.0)

`Effort` is a tier — `Low`, `Medium`, `High` — set on a `TaskContract` for the
root agent or on an `AgentDef` for a role:

```rust
use io_harness::{AgentDef, Agents, Effort, TaskContract};

let contract = TaskContract::workspace("port the parser", "/repo")
    .with_effort(Effort::Medium)
    .with_agents(
        Agents::new()
            .with(AgentDef::new("searcher").with_model("cheap").with_effort(Effort::Low))
            .with(AgentDef::new("critic").with_model("strong").with_effort(Effort::High)),
    );
```

The definition's tier wins over the run's, which is the sentence `AgentDef` could
not say before: search cheaply, think hard only where thinking is the work.

Each vendor is asked in its own dialect —`reasoning.effort` on OpenRouter,
`reasoning_effort` on OpenAI and `Compatible`, and a `thinking` budget on
Anthropic, which has no tiers — and the full table, including what each vendor
does *not* do, is in [the contract](../CONTRACT.md). Two things are worth knowing
before you set one:

**OpenAI returns no thinking text.** The tier changes how the model behaves and
`CompletionResponse::reasoning` stays `None`, because the Chat Completions API
does not return it. `Usage::reasoning_tokens` is the only visibility on that path.

**It is a request, not a fact.** A model that does not reason ignores it, and
nothing is refused for asking, because the crate has no way to know which models
reason. Read `Usage::reasoning_tokens` to find out whether anything happened.

Where the thinking *is* returned it reaches an `Observer` as
`EventKind::Reasoning` and goes nowhere else — not to the observation ledger, and
therefore not into the next turn's prompt. A vendor bills thinking once as output;
folding it into the next request would bill it again as input, every turn, for
the rest of the run.

## Not paying twice for the same instructions (0.38.0)

Every request this crate builds opens with the same block: the system
instructions, the skill catalogue folded into them, and the JSON schema of every
tool on offer. It is assembled once per turn and handed to every step of the loop,
so a twenty-step run sends it twenty times. Until 0.38.0 it was billed twenty
times too.

Since 0.38.0 the request marks the end of that block as a cache breakpoint, and
the vendors that cache serve it back instead of re-reading it. There is nothing to
switch on and nothing to configure:

```rust,no_run
use io_harness::{Anthropic, CompletionRequest, Provider};

# async fn demo() -> io_harness::Result<()> {
let provider = Anthropic::new(std::env::var("ANTHROPIC_API_KEY").unwrap(), "claude-x");

// The same instructions on both calls. The second is served from the vendor's
// cache — nothing here asks for that, because the request already did.
let ask = |question: &str| CompletionRequest {
    system: "…several thousand tokens of instructions and skills…".into(),
    user: question.into(),
    ..Default::default()
};
let first = provider.complete(ask("what does this crate do?")).await?;
let second = provider.complete(ask("and what does it refuse to do?")).await?;

// The counter has existed since 0.18.0. Before 0.38.0 it was structurally zero,
// because nothing ever asked. `first` writes the entry and `second` reads it —
// though whether it does is the vendor's decision and its clock, not this crate's.
println!("wrote: {}", first.usage.unwrap().cache_write_tokens);
println!("read:  {}", second.usage.unwrap().cache_read_tokens);
# Ok(())
# }
```

Which vendors are asked, and why the other two are not:

| provider | marker sent | note |
| --- | --- | --- |
| `Anthropic` | yes, on the one `system` block, and on the frozen half of the user turn | that wire orders tools before system, so one marker covers both |
| `OpenRouter` | yes, on both, in the parts shape that wire spells them in | it translates the markers for the vendors that take them |
| `OpenAi` | no | it caches a repeated prefix by itself; there is no request-side control |
| `Compatible` | no | 21 endpoints this crate does not control, where an unknown key is a 400 |

One difference between the two wires is worth stating because it looks like a bug:
a request carrying an **image** is marked on OpenRouter and not on Anthropic.
Anthropic puts image blocks before the text, so a marked text block would write a
one-turn attachment into the cache entry that the next turn could never hit; the
OpenAI-shaped wire puts text first, so the marked span is still a real prefix.

Four things worth knowing before you read an invoice.

**It pays from the second call, not the first.** A cache write is billed above a
fresh read and a cache read far below one, so a block used exactly once costs more
than it used to. Runs make more than one call and sessions more than one turn,
which is why this is unconditional rather than a setting.

**A short block is not cached at all,** silently — every vendor sets a minimum
length and declines below it without saying so. If `cache_read_tokens` stays zero,
the prefix being too short is the first thing to check.

**Since 0.44.0 the transcript is marked too, but only from a compaction boundary.**
The observation ledger is re-derived on every turn — superseded, invalidated,
re-read, re-fitted — so it is not the byte-identical prefix a cache needs. What
0.43.0's fold changed is that everything from the top of the prompt through the
written summary stops changing, and that part gets the request's second breakpoint.
Everything *after* the summary is rewritten exactly as before and is still never
marked, and a run that has not compacted marks nothing in its transcript at all.

**The marker is withheld until the prefix has repeated.** Even after a fold the
prefix is not immutable: the memory block renders ahead of the summary and is
re-read every turn, so a note the run writes moves it. The loop holds the previous
step's candidate and marks only on a byte-identical repeat — which costs one
unmarked step after every fold and after every note, and buys the guarantee that a
marker can never be billed as a write on a prefix that then changes. Watch
`EventKind::CacheMarked` to see when it starts; no event means nothing was marked.
See
[the contract](../CONTRACT.md#what-prompt-caching-asks-for-and-what-it-cannot-promise-0380-0440)
for the full statement.

`examples/cache_live.rs` measures all of this against a real endpoint, with an
unmarked control over the same route so the numbers mean something. Measured there
on `anthropic/claude-haiku-4.5`: the system breakpoint alone served 7,408 tokens of
a 13,113-token prompt from cache, and with the transcript breakpoint the same
request served 13,093.

## The limits, stated plainly

**`Compatible` sends one wire. It is not a compatibility layer.** Every vendor
reachable here diverges from the OpenAI shape somewhere, and there is no
per-vendor request rewriting, no shim, and no normalisation of what comes back.
The divergences that matter to a caller — Groq's 400 on `messages[].name`,
Mistral's nine-character tool-call ids, Zhipu's `finish_reason` values outside the
OpenAI set, and the rest — are stated in [the contract](../CONTRACT.md) rather
than papered over, because a boundary the caller believes in and nobody enforces
is worse than none. **Read that list before you write a base URL**, not after a
run behaves oddly against one.

**vLLM and SGLang emit no tool calls at all** unless the server was started with
a tool-call parser flag — `--enable-auto-tool-choice` and `--tool-call-parser` on
vLLM — which **a client cannot set**. Nothing errors. No request is refused, no
warning is logged, and `accepts_tools` may still read `Some(true)` because that is
a claim about the model and not about how the server was launched. The agent
simply talks, never calls a tool, and the run ends unverified for a reason that
looks like the model being bad at its job. This is the single most important
sentence on this page: if you are pointing a contract at vLLM or SGLang, check
how the server was started before you check anything else.

**Most vendor `/models` endpoints return identifiers and no prices.** `None`
means the vendor did not say. It is never zero, never a default, and never merged
into a neighbouring model's rate. A local runtime's zero is a *stated* zero — the
run really is free — and the two are different facts that this crate keeps apart.

**A reference price is the aggregator's price, not the vendor's.** It is what
that host charges to serve the model, which tracks the vendor's rate closely and
is not identical to it. `PriceSource` is how an operator tells which number they
are reading, and it names the host the price came from. A cost derived from a
reference price is an estimate of an invoice nobody has sent yet.

**Matching is an exact slug or one documented normalisation**, and nothing else:
lowercase, and drop a single leading `vendor/` segment. Not applied in the other
direction — a two-segment vendor id is not stripped to meet a one-segment
reference entry, which would be a second rule and a second way to be wrong. A
miss stays `None` rather than becoming the nearest guess, and an ambiguous
normalisation resolves to nothing rather than to either candidate.
`Spend::unpriced_calls` is where the gap is reported, and a group with it above
zero is stating a floor rather than a total.

**The reference lookup dials a host the caller did not name.** That is why it is
off until `with_reference_prices` asks for it. When it is on, its host appears in
`Provider::endpoints()` alongside the chat endpoint, and the run authorises every
URL there against the policy's `Act::Net` rules before the first step — so a
policy that denies it makes the run **refuse**, not silently skip the lookup. A
failure fetching the catalogue is likewise the caller's to see: swallowing it
would leave a run silently unpriced after the operator asked for prices.

**`Fallback::models()` returns an empty list.** It reports both endpoints, as it
must, but it names no models: which of two vendors' catalogues a chain should
report has no right answer, and picking the primary's would describe a run that
may be served by the secondary. Ask each provider in the chain directly. The same
default covers any out-of-tree `Provider` written before this release — it keeps
compiling, and reports no catalogue rather than a wrong one.

**A price table is a snapshot, and the catalogue is read once per instance.**
Both `Compatible` and `Reference` cache their fetch for the life of the value, so
a price that moved during a long-lived process is not picked up until a new one is
built. `PriceTable::as_of` carries the instant the read happened, which is the
claim the table is allowed to make.

**Twenty-one base URLs are twenty-one defaults that were right on the day this
release was cut.** Each is that vendor's or that project's own documented
endpoint, read once and frozen into a table. A vendor that moves a path, a local
runtime bound elsewhere, or a gateway in front of either is one `base_url` line
in your own file — or one `Compatible::new` in your own code — and never a
release of this crate. That is the reason the constructor is public.

**There is no `Compatible::from_env`.** The other three providers have one; this
one would need twenty-one variable names, and each would be a guess this crate
made on an operator's behalf. Write `api_key = "${env:GROQ_API_KEY}"` under
`[[provider]] kind = "compatible"` instead, which names the variable in the file
of the person who chose it. A preset constructor takes the key; where the key
came from is the caller's business, and this crate does not need an opinion.

**Bedrock, Vertex and Azure OpenAI are not reachable**, and Gemini here is its
compatibility endpoint rather than its native API. An application that mints its
own bearer token can reach the first two by passing it as the key; this crate
grows no signing dependency to do it for you.

## Changing model mid-run (0.34.0)

A role's model is fixed when the roster is written, and `Fallback` moves to its
secondary on a *failure*. Neither is a rule that changes which model answers as a
run goes:

```rust
use io_harness::{Routing, TaskContract};

let contract = TaskContract::workspace("port the parser", "/repo")
    .with_routing(
        Routing::new()
            // Three gates in a row have refused: stop paying the cheap model to
            // fail.
            .escalate_after(3, "big-model")
            // While the change is small, it does not need the expensive one.
            .downshift_under(2_048, "small-model")
            // And do not start an eight-hour unattended job on a fallback.
            .require_primary(),
    );
# let _ = contract;
```

Every rule sets `CompletionRequest.model` on the request that is actually sent, so
no provider changes and nothing new is constructed. `EventKind::Routed { from, to,
why }` is emitted **once**, at the transition — a run that moved is otherwise
indistinguishable from one that always used that model.

**`require_primary` asks `Provider::reachable()` before the first step.** That
method is defaulted to `Ok(true)`, so an out-of-tree provider that does not
override it keeps its 0.33.0 behaviour and makes the rule a no-op rather than a new
precondition. It is a point-in-time answer: a provider that dies mid-run is what
`Fallback` and `RetryPolicy` are for, and this crate runs no health check.

Escalation wins over downshifting, is one-way, and counts *consecutive* failed
gates. A run whose gate keeps refusing is not one to save money on, and a run that
oscillates between two models is a behaviour nobody asked for.

## Reporting a tool call before the completion ends (0.54.0)

`Provider` has three completion methods, and two of them are defaulted:

| Method | What it reports as it goes | Default |
| --- | --- | --- |
| `complete` | nothing | required |
| `complete_streaming` | assistant text | delegates to `complete`, one delta |
| `complete_streaming_calls` | text **and finished tool calls** | delegates to `complete_streaming`, no calls |

The third is what lets a session turn start a read-only tool call while the model
is still speaking — in a turn that streams, which means one of the `_observed` or
`_steered` entry points; `Session::turn_bounded` and its siblings do not stream and
so start nothing early. **Implementing it is optional and the default costs
nothing**:
a provider that does not override it reports no call, the harness starts nothing
early, and the run behaves exactly as it did on 0.53.0. `Record` and `Replay`
override neither streaming method, which is why a replayed run never starts
anything early — a useful property rather than a gap, since it makes a replayed
run identical to the serial one by construction.

If you are writing a provider against a wire that streams tool-call arguments in
fragments, the rule the built-in wires use is worth copying:

- **Report a call when its accumulated arguments parse as a JSON object.** Every
  proper prefix of a JSON object fails to parse — a truncated object is missing
  its brace, a truncated string its quote — so a report cannot fire on half a
  call. You do not need a per-call end event from the vendor, which is just as
  well: Anthropic sends one and the OpenAI wire does not.
- **Report in position order and never past a gap.** The `usize` you pass is the
  call's position in the `CompletionResponse` you will return, counting only the
  calls that response will actually carry. If an earlier call is not finished yet,
  say nothing rather than reporting a later one — its position is not decidable
  until the earlier one is.
- **Reporting eagerly cannot make a run do the wrong thing.** The harness uses a
  reported call only if the completion it finally returns carries that same call,
  with the same name and byte-identical arguments, at that same position. Anything
  else is discarded with nothing recorded. The worst an over-eager implementation
  costs is a wasted read — which `EventKind::Speculated`'s `discarded` count will
  show you.

## See also

- [The public contract](../CONTRACT.md) — the per-vendor divergences, stated
  rather than papered over
- [Accounting](accounting.md) — the rows a price table prices, and why cost is
  derived rather than stored
- [Configuration — `io.toml`](configuration.md) — `[[provider]]`, `${env:...}`,
  and the chain a file describes
- [Resilience](resilience.md) — `Fallback`, and what a chain does and does not
  report
- [MCP and network egress](mcp-and-network.md) — the `Act::Net` rules the
  reference host is authorised against
- [README](../../README.md)
