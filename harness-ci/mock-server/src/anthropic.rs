//! Anthropic Messages API wire format, streaming and not.

use crate::scenario::{Block, Fault, Reply, Usage};
use serde_json::{Value, json};

pub fn message(index: usize, model: &str, reply: &Reply, stop_reason: &str) -> Value {
    let content: Vec<Value> = reply.content.iter().enumerate().map(|(i, block)| block_json(index, i, block)).collect();
    json!({
        "id": format!("msg_mock_{index}"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": usage_json(reply.usage),
    })
}

fn usage_json(usage: Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_creation_input_tokens": usage.cache_creation_input_tokens,
        "cache_read_input_tokens": usage.cache_read_input_tokens,
    })
}

fn tool_id(index: usize, block: usize) -> String {
    format!("toolu_mock_{index}_{block}")
}

fn block_json(index: usize, block_index: usize, block: &Block) -> Value {
    match block {
        Block::Text { text } => json!({"type": "text", "text": text}),
        Block::Thinking { thinking, signature } => json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature.clone().unwrap_or_else(|| "mock-signature".into()),
        }),
        Block::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id.clone().unwrap_or_else(|| tool_id(index, block_index)),
            "name": name,
            "input": input,
        }),
    }
}

/// SSE events as `(delay_ms_before_event, full_sse_frame)`.
pub fn stream_events(index: usize, model: &str, reply: &Reply, stop_reason: &str) -> Vec<(u64, String)> {
    let mut events = Vec::new();
    let mut delay = match reply.fault {
        Some(Fault::Slow { delay_ms }) => delay_ms,
        _ => 0,
    };
    let slow_first = match reply.fault {
        Some(Fault::SlowFirstToken { delay_ms }) => delay_ms,
        _ => 0,
    };

    events.push((
        delay,
        sse(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": format!("msg_mock_{index}"),
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": usage_json(reply.usage),
                }
            }),
        ),
    ));
    if matches!(reply.fault, Some(Fault::BrokenStream)) {
        return events;
    }

    for (i, block) in reply.content.iter().enumerate() {
        delay = if i == 0 { slow_first } else { 0 };
        match block {
            Block::Text { text } => {
                events.push((delay, content_start(i, json!({"type": "text", "text": ""}))));
                events.push((0, content_delta(i, json!({"type": "text_delta", "text": text}))));
                events.push((0, content_stop(i)));
            }
            Block::Thinking { thinking, signature } => {
                events.push((delay, content_start(i, json!({"type": "thinking", "thinking": "", "signature": ""}))));
                events.push((0, content_delta(i, json!({"type": "thinking_delta", "thinking": thinking}))));
                events.push((
                    0,
                    content_delta(
                        i,
                        json!({
                            "type": "signature_delta",
                            "signature": signature.clone().unwrap_or_else(|| "mock-signature".into()),
                        }),
                    ),
                ));
                events.push((0, content_stop(i)));
            }
            Block::ToolUse { id, name, input } => {
                events.push((
                    delay,
                    content_start(
                        i,
                        json!({
                            "type": "tool_use",
                            "id": id.clone().unwrap_or_else(|| tool_id(index, i)),
                            "name": name,
                            "input": {},
                        }),
                    ),
                ));
                events.push((
                    0,
                    content_delta(
                        i,
                        json!({
                            "type": "input_json_delta",
                            "partial_json": serde_json::to_string(input).expect("input serializes"),
                        }),
                    ),
                ));
                events.push((0, content_stop(i)));
            }
        }
    }

    events.push((
        0,
        sse(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
                "usage": {"output_tokens": reply.usage.output_tokens},
            }),
        ),
    ));
    events.push((0, sse("message_stop", json!({"type": "message_stop"}))));
    events
}

fn content_start(index: usize, block: Value) -> String {
    sse("content_block_start", json!({"type": "content_block_start", "index": index, "content_block": block}))
}

fn content_delta(index: usize, delta: Value) -> String {
    sse("content_block_delta", json!({"type": "content_block_delta", "index": index, "delta": delta}))
}

fn content_stop(index: usize) -> String {
    sse("content_block_stop", json!({"type": "content_block_stop", "index": index}))
}

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

pub fn error(status: u16, message: &str) -> Value {
    let kind = match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        500 => "api_error",
        529 => "overloaded_error",
        _ => "api_error",
    };
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{Block, Reply, Usage};

    fn reply(blocks: Vec<Block>) -> Reply {
        Reply { model: None, content: blocks, stop_reason: None, usage: Usage::default(), fault: None }
    }

    #[test]
    fn text_turn_has_the_sse_shape() {
        let events = stream_events(0, "m", &reply(vec![Block::Text { text: "hi".into() }]), "end_turn");
        let all: String = events.into_iter().map(|(_, e)| e).collect();
        for event in [
            "message_start",
            "content_block_start",
            "text_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(all.contains(event), "missing {event} in {all}");
        }
    }

    #[test]
    fn broken_stream_stops_after_start() {
        let mut r = reply(vec![Block::Text { text: "hi".into() }]);
        r.fault = Some(Fault::BrokenStream);
        let events = stream_events(0, "m", &r, "end_turn");
        assert_eq!(events.len(), 1);
        assert!(events[0].1.contains("message_start"));
    }
}
