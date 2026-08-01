<!--
Generated from canonical UltraShip state. Do not edit directly.
Run `ultraship views` to regenerate.
-->
# Active releases

## io-harness 0.29.0

**Execution state:** DEVELOPING
**Release fit:** probable
**Target mode:** published
**Outcome:** Run against any OpenAI-shaped endpoint by naming a base URL, a key and a model — so Groq, xAI, Mistral, DeepSeek, Together, Fireworks, Cerebras, Perplexity, Gemini through its compatibility endpoint, Moonshot, Zhipu, Qwen and MiniMax are reachable without writing a provider, and so are the runtimes a developer starts on their own machine: Ollama, llama.cpp, vLLM, LM Studio, LocalAI, Jan, SGLang and KoboldCpp. A model on the developer's laptop is the half that has no equivalent today, and it is the half that costs nothing per token.
It is one type. `openai.rs` (159 lines) and `openrouter.rs` (161) are the same file apart from four strings — `ENDPOINT` (`src/provider/openai.rs:18`, `src/provider/openrouter.rs:18`), the `name()` literal, the `WebFlavor` variant, and two environment variable names — over an `openai_wire` that is already shared and already `pub(crate)` (`src/provider/mod.rs:9`). So `Compatible` carries a base URL and an auth style, with vendor presets behind named constructors, and reuses `openai_wire` verbatim. A vendor added later is a row in a table rather than a file.
A connected provider also says what it can run. `Provider::models()` returns the vendor's own catalogue — ids, and per model whatever the vendor stated: context length, maximum output, whether it takes images or tools, and the price. It is a **defaulted** trait method returning an empty list, because adding a required method to the one externally-implementable trait this crate has would break it — `src/provider/mod.rs:674` ships a user-written `impl Provider` as its own doc example.
And for the vendors that publish no prices, an opt-in reference catalogue fills the gap without a table anybody maintains: `https://openrouter.ai/api/v1/models`, no key, parsed into the same `ModelInfo`. Off by default, and when on its host appears in `Provider::endpoints()` under the same `Act::Net` rules as every other endpoint, so a policy that denies it refuses the run rather than skipping the lookup.


**Constraints** (user-estimate): time —, budget —, capacity Parallel agents authorised; `allow_parallel_agents` is already true in the workspace resource profile


_Canonical sources: products/<id>/execution/active.yaml, products/<id>/releases/<version>.yaml_
