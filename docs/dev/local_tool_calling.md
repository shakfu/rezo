# Local Tool Calling: Findings from the llama-cpp-2 Spike

Empirical results from running `crates/rezon-web/examples/local_tool_spike.rs` against local GGUF models on Apple M1 with `llama-cpp-2` 0.1.146 and `llama-cpp-sys-2` 0.1.146 (Metal backend).

The spike validates whether rezon can support tool-calling on local models with quality and a streaming surface comparable to the cloud path.

## TL;DR

- The local agent path is viable. Streaming tool-call deltas in OpenAI-compatible shape are produced end-to-end on Qwen 3 4B.

- Grammar-constrained sampling (`grammar_lazy`) is currently unstable in this library version and must be skipped.

- Output reliability without grammar is therefore model-dependent. Qwen 3 4B+ and Llama 3.1 8B+ should be reliable; Llama 3.2 1B is not.

- No framework is needed: `llama-cpp-2`'s OpenAI-compat surface emits the same delta shape that `async-openai` produces from the cloud, so the rezon agent loop can be written once and consume either source.

## What `llama-cpp-2` provides

The library has first-class OpenAI-compatible tool-calling support:

| Symbol | Purpose |
|---|---|
| `LlamaModel::apply_chat_template_with_tools_oaicompat` | Render messages + tools into a prompt using the model's native tool-aware chat template; returns grammar, triggers, parser, etc. |
| `LlamaModel::apply_chat_template_oaicompat` (richer variant via `OpenAIChatTemplateParams`) | Adds `tool_choice`, `parallel_tool_calls`, `enable_thinking`, custom grammar override, jinja toggle, generation-prompt control |
| `ChatTemplateResult` | Carries `prompt`, `grammar`, `grammar_lazy`, `grammar_triggers`, `preserved_tokens`, `additional_stops`, `chat_format`, `parser`, `generation_prompt`, `parse_tool_calls` |
| `ChatTemplateResult::streaming_state_oaicompat` | Initialize a streaming parser |
| `ChatTemplateResult::parse_response_oaicompat` | Non-streaming parse for ground-truth comparison |
| `ChatParseStateOaicompat::update(text, is_partial)` | Feed incremental output text; returns `Vec<String>` of OpenAI-shaped JSON deltas |
| `LlamaSampler::grammar_lazy(model, grammar, root, words, tokens)` | Grammar-constrained sampler with lazy trigger activation |

## Test setup

- Hardware: Apple M1, Metal backend, `n_gpu_layers = 999`

- Tools defined: `calculator(expression)` and `current_time()` as an OpenAI-shaped tools JSON array

- Two messages: short system prompt + user request to compute `17 * 23`

- Sampler tail: `temp(0.7)` + `dist(1234)` after grammar (when present)

- `MAX_NEW_TOKENS = 1024`

- The spike feeds each generated token's text through `ChatParseStateOaicompat::update(piece, true)` and prints each resulting JSON delta

## Result 1: Llama-3.2-1B-Instruct-Q8_0

Library output:

```
chat_format       = 2
parse_tool_calls  = true
grammar           = None
grammar_lazy      = false
grammar_triggers  = 0 entries
parser            = Some(9782 chars)
generation_prompt = "<|start_header_id|>assistant<|end_header_id|>\n\n"
```

Model emission:

```
{"type": "function", "function": "calculator", "parameters": {"properties": "{'expression': '17 * 23'}"}}
```

Outcome:

- Library recognized the Llama tool format (`chat_format = 2`, `parser` present) but did **not** synthesize a sampling grammar.

- Generation was unconstrained.

- Model output was a structural approximation, not a valid Llama 3.x tool call: wrong field shapes (`function` should be an object, `arguments` not `parameters`), and arguments encoded as a Python dict string instead of JSON.

- The streaming parser emitted zero deltas; the final flush errored with `ffi error -3` on the unparseable text.

Verdict: **Llama 3.2 1B is too small for reliable tool calling without grammar enforcement.** This is the well-known structured-output reliability cliff for sub-7B models.

## Result 2: Qwen3-4B-Q8_0

Library output:

```
chat_format       = 2
parse_tool_calls  = true
grammar           = Some(1368 chars)
grammar_lazy      = true
grammar_triggers  = 1 entries
  - Word: "<tool_call>\n" (token=None)
preserved_tokens  = ["<think>", "</think>", "<tool_call>", "</tool_call>"]
parser            = Some(9792 chars)
generation_prompt = "<|im_start|>assistant\n"
```

### Run 2a: with `grammar_lazy` enabled

After `<think>\n\n<tool_call>\n` was emitted and the lazy trigger fired, llama.cpp asserted in its grammar engine and aborted the process:

```
Grammar still awaiting trigger after token 271 (`\n`)
Grammar still awaiting trigger after token 151657 (`<tool_call>`)
Grammar triggered on regex: '<tool_call>\n'
GGML_ASSERT(!stacks.empty()) failed
  src/llama-grammar.cpp:940
```

This is an **upstream llama.cpp grammar bug**, triggered the moment constrained sampling activates after the lazy trigger word. The library warning mentions PR #17869 (a backtrace fix), suggesting active work in this area. The crash bypasses Drop, which is exactly the failure mode rezon's existing teardown discipline was built to avoid.

Verdict for grammar route: **not viable on `llama-cpp-2` 0.1.146**. Skip grammar entirely until the upstream fix lands.

### Run 2b: with grammar disabled (`NO_GRAMMAR=1`)

Model emission (full):

```
<think>\n\n<tool_call>\n{"name": "calculator", "arguments": {"expression": "17 * 23"}}\n</tool_call>
```

Streaming parser produced 13 deltas in OpenAI shape:

```
{"content":"<think>"}
{"content":"\n\n"}
{"tool_calls":[{"index":0,"id":"...","type":"function","function":{"name":"calculator","arguments":"{"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":"\""}}]}
{"tool_calls":[{"index":0,"function":{"arguments":"expression"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":"\":"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":" \""}}]}
{"tool_calls":[{"index":0,"function":{"arguments":"1"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":"7"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":" *"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":" 2"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":"3"}}]}
{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]}
```

The non-streaming reference parse produced:

```json
{
  "role": "assistant",
  "content": "<think>\n\n",
  "tool_calls": [{
    "type": "function",
    "function": {
      "name": "calculator",
      "arguments": "{\"expression\": \"17 * 23\"}"
    },
    "id": "..."
  }]
}
```

EOG fired cleanly. Drop ran in the right order. No crash.

Verdict: **end-to-end working**, byte-compatible with `async-openai`'s `ChatCompletionMessageToolCallChunk` shape.

## Architectural implications for rezon

1. **The agent loop is provider-agnostic for free.** The cloud and local backends emit the same OpenAI-shape delta stream, so the loop consumes one type and does not branch on provider.

2. **Skip grammar in 0.1.146.** Build the local adapter without `grammar_lazy`. Revisit when the upstream `GGML_ASSERT` bug is fixed (PR #17869 area).

3. **Model selection is the quality lever.** Without grammar enforcement, output validity depends on the model. Tag tool-capable models in the picker; warn or disable tool-calling on small models. Concrete guidance:

   - Reliable: Qwen 3 4B+, Qwen 2.5 7B+, Llama 3.1 8B+, Llama 3.3, Hermes 3, Mistral Nemo Instruct.

   - Unreliable: Llama 3.2 1B, base or non-instruct models, anything <4B parameters.

4. **Reasoning blocks need UX handling.** Qwen 3 streams `<think>...</think>` as content deltas. The loop should either render thinking inline with distinct styling, or hide until the tool-call / final-answer portion begins.

5. **No framework is required to deliver the stated rezon target** (single-agent, multi-tool, OpenAI + Anthropic + OpenRouter + local, streaming, almost-invisible UI). The hand-rolled abstraction is the right shape for a future framework migration if multi-agent ever becomes a real requirement.

## Open items

- **Multi-turn local tool execution.** The spike is single-turn (no tool dispatch + second model call). Almost certainly works since the chat template handles `tool` role messages natively, but unverified.

- **Mid-tool-call cancellation.** Cancelling while a `<tool_call>` is partially emitted may leave the streaming parser in a state where the final flush errors. Loop needs a graceful abort path.

- **Pattern-trigger sampler variant.** `LlamaSampler::grammar_lazy` takes word + token triggers; a separate regex-pattern variant exists. The crash log says "Grammar triggered on regex" even when fed words, suggesting internal regex matching. Probing the pattern-aware API may or may not avoid the bug; not structural given we are skipping grammar regardless.

- **Behavior on non-tool-capable templates.** Models without a tool-aware chat template (older Llama 2, base models) should degrade to text-only. Verify the loop refuses to enable tools when `apply_chat_template_with_tools_oaicompat` returns `parse_tool_calls = false` or `parser = None`.

- **Library upgrade.** Track `llama-cpp-2` and the underlying `llama.cpp` for the grammar fix; re-evaluate constrained sampling when an updated version is available.

## Reference

- Spike source: `crates/rezon-web/examples/local_tool_spike.rs`

- Runner: `crates/rezon-web/examples/run_local_tool_spike.sh`

- Disable grammar to avoid the upstream crash: `NO_GRAMMAR=1 cargo run --example local_tool_spike -- <model.gguf>`

- Library version: `llama-cpp-2 = 0.1.146` (Metal feature)

- llama.cpp grammar PR referenced by warning: ggml-org/llama.cpp#17869
