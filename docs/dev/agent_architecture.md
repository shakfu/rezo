# Agent Architecture: Single-Agent Multi-Tool vs Multi-Agent

Notes on what the ReAct prototype in `crates/rezon-web/examples/react_agent.rs` actually is, and how it compares to richer multi-agent patterns. Useful context for deciding what rezon should build next.

## What the prototype is

The prototype is **single-agent, multi-tool**:

- **One agent** - one LLM identity, one message history, one system prompt, one loop.

- **Multi-tool** - the agent is given several tools (`calculator`, `current_time`) and picks which to call. It can call them across loop iterations, and a single response can request multiple parallel calls (which is why `tool_calls` is a `Vec`).

There is no second model and no inter-agent communication.

## Are the tools "composed"?

In a sense, yes - but the composition lives in the **model's reasoning**, not in the tool definitions. The tools themselves are independent leaf functions: they do not know about each other and cannot call each other.

Example trace:

```
user: "17 * 23 plus the current hour"
assistant: tool_calls=[calculator("17*23"), current_time()]   # parallel
tool: {result: 391}
tool: {hour: 14}
assistant: tool_calls=[calculator("391+14")]                  # sequential, depends on prior results
tool: {result: 405}
assistant: "405"                                              # final
```

The model orchestrates; the tools are dumb. That is the orthodox tool-calling pattern.

## What would make it multi-agent

- A second LLM call with a *different* system prompt, role, or model (e.g. a "planner" that decomposes the task and a "worker" that executes, or a "critic" that reviews the worker's answer).

- Some shared state or message bus between them (AutoAgents' "typed pub/sub" is exactly this).

- Often: the agents are themselves exposed *as tools* to a parent agent. In that framing, "multi-agent" is really "multi-tool where some tools happen to be other LLMs."

## What would make tools genuinely composable

- Tools that call other tools internally (a `web_research` tool that fans out to `search` + `fetch_url` + `summarize`).

- A typed pipeline or DAG runtime where tool A's output is plumbed into tool B without round-tripping through the LLM. This is **not** what OpenAI tool-calling gives you - every step goes through the model.

## When each pattern is appropriate

| Pattern | Pick when |
|---|---|
| Single-agent, multi-tool | The chat needs to run code, search files, hit APIs - one identity is doing all the work, just with extra capabilities. Most rezon use cases live here. |
| Multi-agent | You have distinct *roles* (planner / executor / critic), distinct *contexts* (so one agent's huge context does not pollute another's), or distinct *models* (cheap model triages, expensive model reasons). |
| Composable tools (sub-tool / DAG) | You have repeatable multi-step procedures where the LLM's reasoning at each step adds little value but adds latency and cost. |

## Implication for rezon

If the eventual goal is "the chat can run code, search files, hit APIs," **single-agent multi-tool is sufficient**. Multi-agent is justified only when there is a concrete reason for separate roles, contexts, or models.

Both rig and AutoAgents support both patterns. AutoAgents leans harder into the multi-agent orchestration story (typed pub/sub); rig's framing is more "agent = LLM + tools + memory, compose externally." Either framework is overkill if rezon only ever needs the single-agent pattern.

## Decision posture

Default to single-agent multi-tool. Promote a feature to multi-agent only when there is a named role or context-isolation reason that a single agent cannot satisfy. "It feels more sophisticated" is not a reason.
