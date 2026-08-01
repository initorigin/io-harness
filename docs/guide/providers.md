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
