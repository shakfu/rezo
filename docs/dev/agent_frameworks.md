# Agent Framework Evaluation: Rig vs AutoAgents

Evaluation of Rust LLM/agent frameworks for rezon, given that agents, tools, and RAG are on the eventual roadmap.

## Current rezon stack (baseline)

- `async-openai` driving all 4 cloud providers via OpenAI-compatible endpoints (OpenAI, Anthropic, OpenRouter, "other")

- `llama-cpp-2` with a hand-rolled worker thread, KV-cache reuse, and careful Metal teardown to avoid `__cxa_finalize` / `GGML_ASSERT` crashes during static destruction

- Streaming via Tauri events (`chat-token`, `chat-stats`, `chat-done`)

- ~753 lines in `crates/rezon-core/src/llm.rs`

- No tools, no RAG, no agents today

## Side-by-side

| Dimension | Rig | AutoAgents |
|---|---|---|
| Cloud providers | 20+ | ~10 (OpenAI, Anthropic, OpenRouter, Groq, xAI, Google, Azure, DeepSeek, MiniMax, Phind) |
| Local: llama.cpp/GGUF | No | Yes - CPU/CUDA/Metal/Vulkan variants |
| Local: Ollama | Via OpenAI-compat | Native, plus Mistral-rs |
| Vector stores | 10+ (Mongo, LanceDB, Neo4j, Qdrant, SQLite, SurrealDB, Milvus, ScyllaDB, ...) | Qdrant only |
| Tools | Trait-based | Derive-macro typed tools + WASM sandbox runtime |
| Multi-agent | Agent workflows, multi-turn streaming | Typed pub/sub orchestration |
| RAG | Strong, many backends | Thin (Qdrant) |
| WASM (frontend) | Core is WASM-compatible | Not advertised |
| Maturity | Established, larger community | v0.3.7 (~Mar 2026), 623 stars, breaking changes likely |

## Analysis

### Rig

Strengths:
- Largest provider catalog and vector-store ecosystem in Rust

- Native Anthropic client unlocks features the OpenAI-compat shim hides (prompt caching control, extended thinking, citations)

- Mature, well-documented, broader community

Weaknesses for rezon:
- No llama.cpp / GGUF support. The most carefully engineered part of rezon's backend (worker thread, KV-cache reuse, Metal teardown) gets zero help.

- Adopting rig means two paradigms side-by-side: rig for cloud, custom for local. Tools/agents/RAG would only work on cloud unless we implement rig's `CompletionModel` trait over the local worker ourselves.

### AutoAgents

Strengths:
- First-class llama.cpp + Metal support maps directly onto rezon's existing local stack

- Same cloud providers we already use (OpenAI, Anthropic, OpenRouter)

- WASM-sandboxed tool runtime is a genuinely interesting fit for a desktop app that should not run arbitrary tool code in-process

- Typed pub/sub multi-agent orchestration

Weaknesses:
- v0.3.x: expect breaking changes, fewer answers in the wild, occasional need to PR upstream

- Vector-store story is thin (Qdrant only) - weaker if RAG breadth matters

- Smaller community

## Tradeoff summary

- **AutoAgents wins** if multi-agent orchestration, local/cloud parity for agents and tools, and tool sandboxing matter. Cost: early-adopter tax on a v0.3 framework.

- **Rig wins** if RAG breadth and a wider vector-store choice matter, and we are willing to keep `llama-cpp-2` as a separate path that does not participate in agents/tools (or write a `CompletionModel` bridge ourselves).

## Failure modes to test before committing

1. **AutoAgents' llama.cpp wrapper teardown.** Our current code exists *because* of `GGML_ASSERT` crashes during static destruction on Metal. Verify their wrapper drops the `LlamaContext` before the backend, or we end up back where we started.

2. **AutoAgents' streaming surface.** The UI consumes per-token deltas plus `chat-stats` (prompt/cached/gen tokens). Confirm their streaming abstraction exposes provider-specific token counts, or the stats panel regresses.

3. **Rig's local story.** If we go rig, prototype the `CompletionModel` impl over the local worker first - that is the structural piece, and if it is awkward, the rest of the plan is awkward.

## Recommendation

Given that rezon is already a llama.cpp + Metal app and we want agents, tools, and RAG eventually, **AutoAgents is the more structurally appropriate bet** - but only if we accept the v0.3 maturity cost.

A third option worth naming explicitly: **defer the framework decision.** Add tool-calling against the existing `async-openai` path as a small, contained feature first. Once we know what we actually need from a framework, the rig vs AutoAgents call becomes much easier to make on evidence rather than speculation.

## Open questions

- How tightly does AutoAgents' llama.cpp binding handle Metal teardown? (Needs source-level verification.)

- Does AutoAgents' streaming surface expose `cached_tokens` for Anthropic/OpenAI prompt caching?

- For rig: how large is the lift to implement `CompletionModel` over the existing local worker?

- What is the realistic timeline for tools/RAG in rezon? If it is "next feature," integrate now. If it is "someday," defer.
