//! OpenAI Chat Completions wire format, streaming and not. OpenCode's openai-compatible
//! provider is the consumer: it reads `choices[].delta` text, `tool_calls`, `finish_reason`
//! and `usage` from SSE chunks.

use crate::scenario::{Block, Fault, Reply, Usage};
use serde_json::{Value, json};

pub fn completion(index: usize, model: &str, reply: &Reply, stop_reason: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for (block_index, block) in reply.content.iter().enumerate() {
        match block {
            Block::Text { text: value } => text.push_str(value),
            Block::Thinking { .. } => {}
            Block::ToolUse { id, name, input } => tool_calls.push(tool_call(
                block_index,
                id.clone().unwrap_or_else(|| tool_call_id(index, block_index)),
                name,
                &serde_json::to_string(input).expect("input serializes"),
            )),
        }
    }
    let mut message = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        message["content"] = Value::Null;
        message["tool_calls"] = Value::Array(tool_calls);
    }
    json!({
        "id": format!("chatcmpl_mock_{index}"),
        "object": "chat.completion",
        "created": now_unix(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason(stop_reason),
        }],
        "usage": usage_json(reply.usage),
    })
}

fn usage_json(usage: Usage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "prompt_tokens_details": {"cached_tokens": usage.cache_read_input_tokens},
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn tool_call_id(index: usize, block: usize) -> String {
    format!("call_mock_{index}_{block}")
}

fn finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "content_filter" => "content_filter",
        _ => "stop",
    }
}

fn tool_call(block: usize, id: String, name: &str, arguments: &str) -> Value {
    json!({
        "index": block,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    })
}

fn chunk(index: usize, model: &str, delta: Value, finish: Option<&'static str>, usage: Option<Usage>) -> String {
    let mut value = json!({
        "id": format!("chatcmpl_mock_{index}"),
        "object": "chat.completion.chunk",
        "created": now_unix(),
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }],
    });
    if let Some(usage) = usage {
        value["usage"] = usage_json(usage);
    }
    sse(value)
}

/// SSE frames as `(delay_ms_before_event, full_sse_frame)`.
pub fn stream_events(index: usize, model: &str, reply: &Reply, stop_reason: &str) -> Vec<(u64, String)> {
    let mut events = Vec::new();
    let delay = match reply.fault {
        Some(Fault::Slow { delay_ms }) => delay_ms,
        _ => 0,
    };
    let slow_first = match reply.fault {
        Some(Fault::SlowFirstToken { delay_ms }) => delay_ms,
        _ => 0,
    };

    events.push((delay, chunk(index, model, json!({"role": "assistant"}), None, None)));
    if matches!(reply.fault, Some(Fault::BrokenStream)) {
        return events;
    }

    let mut content_sent = false;
    let mut tool_index = 0usize;
    for block in reply.content.iter() {
        let delay = if content_sent { 0 } else { slow_first };
        match block {
            Block::Text { text } => {
                content_sent = true;
                events.push((delay, chunk(index, model, json!({"content": text}), None, None)));
            }
            Block::Thinking { thinking, .. } => {
                content_sent = true;
                events.push((delay, chunk(index, model, json!({"reasoning_content": thinking}), None, None)));
            }
            Block::ToolUse { id, name, input } => {
                content_sent = true;
                let id = id.clone().unwrap_or_else(|| tool_call_id(index, tool_index));
                let arguments = serde_json::to_string(input).expect("input serializes");
                events.push((
                    delay,
                    chunk(
                        index,
                        model,
                        json!({"tool_calls": [{
                            "index": tool_index,
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": ""},
                        }]}),
                        None,
                        None,
                    ),
                ));
                events.push((
                    0,
                    chunk(
                        index,
                        model,
                        json!({"tool_calls": [{
                            "index": tool_index,
                            "function": {"arguments": arguments},
                        }]}),
                        None,
                        None,
                    ),
                ));
                tool_index += 1;
            }
        }
    }

    events.push((0, chunk(index, model, json!({}), Some(finish_reason(stop_reason)), Some(reply.usage))));
    events.push((0, "data: [DONE]\n\n".to_string()));
    events
}

fn sse(data: Value) -> String {
    format!("data: {data}\n\n")
}

pub fn error(status: u16, message: &str) -> Value {
    let kind = match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        500 => "server_error",
        529 => "overloaded_error",
        _ => "server_error",
    };
    json!({"error": {"message": message, "type": kind, "param": Value::Null, "code": Value::Null}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{Block, Reply, Usage};

    fn reply(blocks: Vec<Block>) -> Reply {
        Reply { model: None, content: blocks, stop_reason: None, usage: Usage::default(), fault: None }
    }

    #[test]
    fn text_turn_has_chat_completion_chunks() {
        let events = stream_events(0, "test-model", &reply(vec![Block::Text { text: "hi".into() }]), "end_turn");
        let all: String = events.into_iter().map(|(_, event)| event).collect();
        for event in ["role", "content", "finish_reason", "usage", "[DONE]"] {
            assert!(all.contains(event), "missing {event} in {all}");
        }
    }

    #[test]
    fn tool_turn_streams_tool_calls_and_maps_the_finish_reason() {
        let reply = reply(vec![Block::ToolUse {
            id: Some("call_1".into()),
            name: "bash".into(),
            input: json!({"command": "true"}),
        }]);
        let all: String =
            stream_events(0, "test-model", &reply, "tool_use").into_iter().map(|(_, event)| event).collect();
        assert!(all.contains("tool_calls"), "{all}");
        assert!(all.contains("call_1"), "{all}");
        assert!(all.contains("tool_calls"), "{all}");
        assert!(all.contains("\"finish_reason\":\"tool_calls\""), "{all}");
        let completion = completion(0, "test-model", &reply, "tool_use");
        assert_eq!(completion["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(completion["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "bash");
    }
}
