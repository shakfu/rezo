// Cloud provider: wraps async-openai's chat-completions streaming
// endpoint. Works against any OpenAI-compatible base URL (OpenAI
// proper, Anthropic's compat endpoint, OpenRouter, Ollama, ...).
//
// The mapping from `ChatCompletionStreamResponseDelta` chunks onto
// our normalized `AgentDelta` enum is the bulk of this file.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageArgs,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
    ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequestArgs, FinishReason as OaiFinish, FunctionCall, FunctionObject,
};
use async_openai::Client;
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use serde_json::Value;

use crate::agent::delta::{AgentDelta, FinishReason, StreamStats};
use crate::agent::message::ChatMessage;
use crate::agent::provider::{Provider, ProviderOpts};
use crate::agent::tool::ToolCall;

/// Identifier surfaced in `StreamStats.provider`. Set per-instance so
/// downstream code can distinguish OpenAI / Anthropic / OpenRouter.
pub struct CloudProvider {
    client: Client<OpenAIConfig>,
    provider_label: String,
}

impl CloudProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        let cfg = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(base_url);
        Self {
            client: Client::with_config(cfg),
            provider_label: label.into(),
        }
    }
}

#[async_trait]
impl Provider for CloudProvider {
    async fn stream(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        opts: &ProviderOpts,
    ) -> Result<BoxStream<'static, Result<AgentDelta>>> {
        let oai_msgs = to_openai_messages(messages)?;
        let oai_tools = to_openai_tools(tools)?;

        let mut req = CreateChatCompletionRequestArgs::default();
        req.model(&opts.model)
            .messages(oai_msgs)
            .stream_options(ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            });
        if !oai_tools.is_empty() {
            req.tools(oai_tools);
        }
        if let Some(max) = opts.max_tokens {
            req.max_tokens(max.max(1));
        }
        if let Some(t) = opts.temperature {
            req.temperature(t.clamp(0.0, 2.0));
        }
        if let Some(p) = opts.top_p {
            req.top_p(p.clamp(0.0, 1.0));
        }
        let request = req.build().context("build chat request")?;

        let started = std::time::Instant::now();
        let upstream = self
            .client
            .chat()
            .create_stream(request)
            .await
            .context("create_stream")?;

        let cancel = opts.cancel.clone();
        let provider_label = self.provider_label.clone();

        // Per-chunk state kept across the unfold:
        // - `seen_indexes`: which tool-call indexes have already received
        //   a Start delta. The first chunk for an index carries `id`
        //   and `function.name`; subsequent chunks carry only argument
        //   fragments.
        // - `pending_done`: queued Done delta to emit after Stats.
        //   We keep emission single-yield-per-poll to keep the stream
        //   shape simple.
        let state = ChunkState {
            upstream: upstream.boxed(),
            cancel,
            provider_label,
            started,
            queue: Vec::new(),
            seen_indexes: Vec::new(),
            saw_finish: None,
            done_emitted: false,
        };

        let stream = stream::unfold(state, |mut s| async move {
            loop {
                if let Some(d) = s.queue.pop() {
                    return Some((Ok(d), s));
                }
                if s.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    if !s.done_emitted {
                        s.done_emitted = true;
                        return Some((
                            Ok(AgentDelta::Done {
                                finish_reason: FinishReason::Cancelled,
                            }),
                            s,
                        ));
                    }
                    return None;
                }

                let next = s.upstream.next().await;
                match next {
                    None => {
                        if s.done_emitted {
                            return None;
                        }
                        s.done_emitted = true;
                        let reason = s
                            .saw_finish
                            .take()
                            .map(map_finish_reason)
                            .unwrap_or(FinishReason::Stop);
                        return Some((
                            Ok(AgentDelta::Done {
                                finish_reason: reason,
                            }),
                            s,
                        ));
                    }
                    Some(Err(e)) => {
                        return Some((Err(anyhow!("upstream stream error: {e}")), s));
                    }
                    Some(Ok(resp)) => {
                        // Drain choices into queued deltas. Reverse
                        // because we pop from the back.
                        let mut produced: Vec<AgentDelta> = Vec::new();
                        for choice in resp.choices {
                            if let Some(content) = choice.delta.content {
                                if !content.is_empty() {
                                    produced.push(AgentDelta::Content(content));
                                }
                            }
                            if let Some(tcs) = choice.delta.tool_calls {
                                for chunk in tcs {
                                    let idx = chunk.index;
                                    let is_first = !s.seen_indexes.contains(&idx);
                                    if is_first {
                                        s.seen_indexes.push(idx);
                                        let id = chunk.id.unwrap_or_default();
                                        let name = chunk
                                            .function
                                            .as_ref()
                                            .and_then(|f| f.name.clone())
                                            .unwrap_or_default();
                                        produced.push(AgentDelta::ToolCallStart {
                                            index: idx,
                                            id,
                                            name,
                                        });
                                        if let Some(args) = chunk
                                            .function
                                            .as_ref()
                                            .and_then(|f| f.arguments.clone())
                                        {
                                            if !args.is_empty() {
                                                produced.push(AgentDelta::ToolCallArgs {
                                                    index: idx,
                                                    fragment: args,
                                                });
                                            }
                                        }
                                    } else if let Some(args) =
                                        chunk.function.and_then(|f| f.arguments)
                                    {
                                        if !args.is_empty() {
                                            produced.push(AgentDelta::ToolCallArgs {
                                                index: idx,
                                                fragment: args,
                                            });
                                        }
                                    }
                                }
                            }
                            if let Some(reason) = choice.finish_reason {
                                s.saw_finish = Some(reason);
                            }
                        }

                        if let Some(usage) = resp.usage {
                            let stats = StreamStats {
                                provider: s.provider_label.clone(),
                                prompt_tokens: Some(usage.prompt_tokens),
                                cached_tokens: usage
                                    .prompt_tokens_details
                                    .as_ref()
                                    .and_then(|d| d.cached_tokens),
                                gen_tokens: usage.completion_tokens,
                                duration_ms: s.started.elapsed().as_millis() as u64,
                            };
                            produced.push(AgentDelta::Stats(stats));
                        }

                        // Push in reverse so Vec::pop yields in original order.
                        s.queue.extend(produced.into_iter().rev());
                        if !s.queue.is_empty() {
                            let d = s.queue.pop().unwrap();
                            return Some((Ok(d), s));
                        }
                        // Empty chunk (e.g. just a heartbeat) — loop and pull next.
                    }
                }
            }
        });

        Ok(stream.boxed())
    }
}

struct ChunkState {
    upstream: BoxStream<
        'static,
        std::result::Result<
            async_openai::types::chat::CreateChatCompletionStreamResponse,
            async_openai::error::OpenAIError,
        >,
    >,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    provider_label: String,
    started: std::time::Instant,
    /// Buffer in *reverse* emission order; pop from the back.
    queue: Vec<AgentDelta>,
    seen_indexes: Vec<u32>,
    saw_finish: Option<OaiFinish>,
    done_emitted: bool,
}

fn map_finish_reason(r: OaiFinish) -> FinishReason {
    match r {
        OaiFinish::Stop => FinishReason::Stop,
        OaiFinish::Length => FinishReason::Length,
        OaiFinish::ToolCalls => FinishReason::ToolCalls,
        OaiFinish::ContentFilter => FinishReason::Other("content_filter".to_string()),
        OaiFinish::FunctionCall => FinishReason::Other("function_call".to_string()),
    }
}

fn to_openai_tools(tools: &[Value]) -> Result<Vec<ChatCompletionTools>> {
    tools
        .iter()
        .map(|t| {
            let func = t
                .get("function")
                .ok_or_else(|| anyhow!("tool schema missing `function`"))?;
            let name = func
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("tool schema missing function.name"))?
                .to_string();
            let description = func
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            let parameters = func.get("parameters").cloned();
            Ok(ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObject {
                    name,
                    description,
                    parameters,
                    strict: None,
                },
            }))
        })
        .collect()
}

fn to_openai_messages(messages: &[ChatMessage]) -> Result<Vec<ChatCompletionRequestMessage>> {
    messages
        .iter()
        .map(|m| match m {
            ChatMessage::System { content } => {
                Ok(ChatCompletionRequestSystemMessageArgs::default()
                    .content(content.clone())
                    .build()
                    .context("system message")?
                    .into())
            }
            ChatMessage::User { content } => Ok(ChatCompletionRequestUserMessageArgs::default()
                .content(content.clone())
                .build()
                .context("user message")?
                .into()),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let oai_calls: Vec<ChatCompletionMessageToolCalls> = tool_calls
                    .iter()
                    .map(|tc: &ToolCall| {
                        ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                            id: tc.id.clone(),
                            function: FunctionCall {
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            },
                        })
                    })
                    .collect();
                let mut builder = ChatCompletionRequestAssistantMessageArgs::default();
                builder.content(content.clone());
                if !oai_calls.is_empty() {
                    builder.tool_calls(oai_calls);
                }
                let built: ChatCompletionRequestAssistantMessage =
                    builder.build().context("assistant message")?;
                Ok(built.into())
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => Ok(ChatCompletionRequestToolMessageArgs::default()
                .tool_call_id(tool_call_id.clone())
                .content(content.clone())
                .build()
                .context("tool message")?
                .into()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Serve one scripted SSE response and shut down.
    ///
    /// Tests go over real HTTP rather than constructing
    /// `CreateChatCompletionStreamResponse` values directly. The risk
    /// this module carries is an `async-openai` upgrade changing how
    /// the wire format is *parsed* — a constructed-struct test would
    /// sail straight past that, because it starts downstream of the
    /// parsing. Feeding real bytes through the real client is the only
    /// version that fails when the interpretation drifts.
    async fn serve(chunks: Vec<String>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request; closing with unread data would RST.
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                        Cache-Control: no-cache\r\nConnection: close\r\n\r\n";
            if sock.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for c in chunks {
                let frame = format!("data: {c}\n\n");
                if sock.write_all(frame.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = sock.write_all(b"data: [DONE]\n\n").await;
            let _ = sock.flush().await;
        });
        format!("http://{addr}/v1")
    }

    fn opts(cancel: Arc<AtomicBool>) -> ProviderOpts {
        ProviderOpts {
            model: "fake-model".to_string(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            cancel,
        }
    }

    /// Collect the whole normalized delta stream for a scripted response.
    async fn deltas(chunks: Vec<String>) -> Vec<AgentDelta> {
        deltas_with_cancel(chunks, Arc::new(AtomicBool::new(false))).await
    }

    async fn deltas_with_cancel(chunks: Vec<String>, cancel: Arc<AtomicBool>) -> Vec<AgentDelta> {
        let base = serve(chunks).await;
        let p = CloudProvider::new("test-key", base, "testprovider");
        let msgs = vec![ChatMessage::user("hi")];
        let mut s = p.stream(&msgs, &[], &opts(cancel)).await.unwrap();
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item.expect("stream item"));
        }
        out
    }

    /// Terse shape of a delta, for asserting sequences.
    fn shape(d: &AgentDelta) -> String {
        match d {
            AgentDelta::Content(s) => format!("content:{s}"),
            AgentDelta::Thinking(s) => format!("thinking:{s}"),
            AgentDelta::ToolCallStart { index, id, name } => {
                format!("start:{index}:{id}:{name}")
            }
            AgentDelta::ToolCallArgs { index, fragment } => {
                format!("args:{index}:{fragment}")
            }
            AgentDelta::ToolCallEnd { index } => format!("end:{index}"),
            AgentDelta::Stats(s) => format!("stats:{}", s.provider),
            AgentDelta::Done { finish_reason } => format!("done:{finish_reason:?}"),
        }
    }

    fn shapes(ds: &[AgentDelta]) -> Vec<String> {
        ds.iter().map(shape).collect()
    }

    fn content_chunk(text: &str) -> String {
        format!(
            r#"{{"id":"c","object":"chat.completion.chunk","created":0,"model":"m",
                 "choices":[{{"index":0,"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#
        )
        .replace('\n', "")
    }

    fn finish_chunk(reason: &str) -> String {
        format!(
            r#"{{"id":"c","object":"chat.completion.chunk","created":0,"model":"m",
                 "choices":[{{"index":0,"delta":{{}},"finish_reason":"{reason}"}}]}}"#
        )
        .replace('\n', "")
    }

    // ---- Content ----------------------------------------------------

    #[tokio::test]
    async fn content_chunks_stream_in_order_then_done() {
        let ds = deltas(vec![
            content_chunk("Hel"),
            content_chunk("lo"),
            finish_chunk("stop"),
        ])
        .await;
        assert_eq!(shapes(&ds), vec!["content:Hel", "content:lo", "done:Stop"]);
    }

    #[tokio::test]
    async fn empty_content_deltas_are_skipped() {
        // Providers emit an opening chunk with `"content":""` plus
        // keep-alive chunks with an empty delta. Forwarding those would
        // put empty Token events on the UI for every heartbeat.
        let ds = deltas(vec![
            content_chunk(""),
            content_chunk("real"),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#.to_string(),
            finish_chunk("stop"),
        ])
        .await;
        assert_eq!(shapes(&ds), vec!["content:real", "done:Stop"]);
    }

    // ---- Tool calls -------------------------------------------------

    #[tokio::test]
    async fn tool_call_id_and_name_come_from_the_first_chunk_only() {
        // The core contract of the OpenAI streaming tool-call format:
        // chunk 0 for an index carries `id` + `function.name`, every
        // later chunk for that index carries argument text only. Emit a
        // second Start and the loop overwrites its builder, losing the
        // args accumulated so far.
        let ds = deltas(vec![
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell_exec","arguments":""}}]},"finish_reason":null}]}"#.to_string(),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]},"finish_reason":null}]}"#.to_string(),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]}"#.to_string(),
            finish_chunk("tool_calls"),
        ])
        .await;
        assert_eq!(
            shapes(&ds),
            vec![
                "start:0:call_1:shell_exec",
                r#"args:0:{"cmd":"#,
                r#"args:0:"ls"}"#,
                "done:ToolCalls",
            ]
        );
    }

    #[tokio::test]
    async fn arguments_on_the_first_chunk_are_not_dropped() {
        // Some providers put the opening brace on the same chunk as the
        // name instead of sending an empty `arguments`. Handling only
        // the name there would silently truncate the JSON.
        let ds = deltas(vec![
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"t","arguments":"{\"a\":1"}}]},"finish_reason":null}]}"#.to_string(),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]},"finish_reason":null}]}"#.to_string(),
            finish_chunk("tool_calls"),
        ])
        .await;
        assert_eq!(
            shapes(&ds),
            vec![
                "start:0:call_1:t",
                r#"args:0:{"a":1"#,
                "args:0:}",
                "done:ToolCalls",
            ]
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls_are_routed_by_index() {
        // Two calls in flight at once, fragments interleaved. Routing
        // by index is what keeps their argument JSON from being spliced
        // into each other.
        let ds = deltas(vec![
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"first","arguments":""}}]},"finish_reason":null}]}"#.to_string(),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"second","arguments":""}}]},"finish_reason":null}]}"#.to_string(),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"A1"}},{"index":1,"function":{"arguments":"B1"}}]},"finish_reason":null}]}"#.to_string(),
            finish_chunk("tool_calls"),
        ])
        .await;
        assert_eq!(
            shapes(&ds),
            vec![
                "start:0:a:first",
                "start:1:b:second",
                "args:0:A1",
                "args:1:B1",
                "done:ToolCalls",
            ]
        );
    }

    #[tokio::test]
    async fn multiple_tool_call_chunks_in_one_message_keep_their_order() {
        // The unfold queues deltas per chunk and pops from the back, so
        // it reverses before pushing. A regression there scrambles the
        // order of everything produced by a single chunk.
        let ds = deltas(vec![
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[{"index":0,"delta":{"content":"before","tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"t","arguments":"X"}}]},"finish_reason":null}]}"#.to_string(),
            finish_chunk("tool_calls"),
        ])
        .await;
        assert_eq!(
            shapes(&ds),
            vec![
                "content:before",
                "start:0:a:t",
                "args:0:X",
                "done:ToolCalls"
            ]
        );
    }

    // ---- Finish reasons ---------------------------------------------

    #[tokio::test]
    async fn finish_reasons_map_onto_the_normalized_enum() {
        for (wire, want) in [
            ("stop", "done:Stop"),
            ("tool_calls", "done:ToolCalls"),
            ("length", "done:Length"),
            ("content_filter", "done:Other(\"content_filter\")"),
        ] {
            let ds = deltas(vec![content_chunk("x"), finish_chunk(wire)]).await;
            assert_eq!(
                shapes(&ds).last().unwrap(),
                want,
                "wire finish_reason {wire:?} mapped wrong"
            );
        }
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_finish_reason_defaults_to_stop() {
        // Truncated or non-conforming providers. The loop needs *some*
        // terminator or the turn never completes.
        let ds = deltas(vec![content_chunk("partial")]).await;
        assert_eq!(shapes(&ds), vec!["content:partial", "done:Stop"]);
    }

    #[tokio::test]
    async fn done_is_emitted_exactly_once() {
        let ds = deltas(vec![content_chunk("x"), finish_chunk("stop")]).await;
        let dones = ds
            .iter()
            .filter(|d| matches!(d, AgentDelta::Done { .. }))
            .count();
        assert_eq!(dones, 1);
    }

    // ---- Usage / stats ----------------------------------------------

    #[tokio::test]
    async fn usage_becomes_stats_before_done() {
        // `include_usage` puts a usage-only chunk after the finish
        // chunk. Stats must land before Done, or the UI records the
        // turn as complete and then gets a late stats event for it.
        let ds = deltas(vec![
            content_chunk("hi"),
            finish_chunk("stop"),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":22,"total_tokens":33}}"#.to_string(),
        ])
        .await;
        assert_eq!(
            shapes(&ds),
            vec!["content:hi", "stats:testprovider", "done:Stop"]
        );
        match &ds[1] {
            AgentDelta::Stats(s) => {
                assert_eq!(s.prompt_tokens, Some(11));
                assert_eq!(s.gen_tokens, 22);
                assert_eq!(s.provider, "testprovider");
            }
            other => panic!("expected Stats, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cached_tokens_are_carried_through_when_present() {
        let ds = deltas(vec![
            finish_chunk("stop"),
            r#"{"id":"c","object":"chat.completion.chunk","created":0,"model":"m","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":64}}}"#.to_string(),
        ])
        .await;
        match ds.iter().find(|d| matches!(d, AgentDelta::Stats(_))) {
            Some(AgentDelta::Stats(s)) => assert_eq!(s.cached_tokens, Some(64)),
            _ => panic!("no Stats delta: {:?}", shapes(&ds)),
        }
    }

    // ---- Cancellation -----------------------------------------------

    #[tokio::test]
    async fn a_cancelled_run_terminates_with_done_cancelled() {
        let cancel = Arc::new(AtomicBool::new(true));
        let ds = deltas_with_cancel(vec![content_chunk("never")], cancel).await;
        assert_eq!(shapes(&ds), vec!["done:Cancelled"]);
    }

    // ---- Request construction ---------------------------------------

    #[test]
    fn assistant_tool_calls_and_tool_results_survive_the_outbound_mapping() {
        // The agent loop replays its own history every turn, so a
        // dropped `tool_call_id` here shows up as the model losing
        // track of what it already called.
        let msgs = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("do it"),
            ChatMessage::Assistant {
                content: "calling".to_string(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "shell_exec".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                }],
            },
            ChatMessage::Tool {
                tool_call_id: "call_1".to_string(),
                content: r#"{"ok":true}"#.to_string(),
            },
        ];
        let out = to_openai_messages(&msgs).unwrap();
        assert_eq!(out.len(), 4);
        assert!(matches!(out[0], ChatCompletionRequestMessage::System(_)));
        assert!(matches!(out[1], ChatCompletionRequestMessage::User(_)));
        match &out[2] {
            ChatCompletionRequestMessage::Assistant(a) => {
                let calls = a.tool_calls.as_ref().expect("tool_calls preserved");
                assert_eq!(calls.len(), 1);
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        assert!(matches!(out[3], ChatCompletionRequestMessage::Tool(_)));
    }

    #[test]
    fn an_assistant_turn_with_no_tool_calls_maps_cleanly() {
        let msgs = vec![ChatMessage::Assistant {
            content: "just text".to_string(),
            tool_calls: Vec::new(),
        }];
        let out = to_openai_messages(&msgs).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn registry_schemas_convert_into_openai_tools() {
        let schemas = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "current_time",
                "description": "Current local time.",
                "parameters": {"type": "object", "properties": {}}
            }
        })];
        let out = to_openai_tools(&schemas).unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_malformed_tool_schema_errors_rather_than_being_sent() {
        // Better to fail building the request than to ship a schema the
        // provider rejects with an opaque 400.
        let bad = vec![serde_json::json!({"type": "function"})];
        assert!(to_openai_tools(&bad).is_err());
    }
}
