//! OpenAI Responses API wire format, streaming and not.
//!
//! Codex is the consumer. The events are the subset its client reads for text, reasoning,
//! function calls and completion.

use crate::scenario::{Block, Fault, Reply, Usage};
use serde_json::{Value, json};

pub fn response(index: usize, model: &str, reply: &Reply, stop_reason: &str) -> Value {
    let output: Vec<Value> = reply.content.iter().enumerate().map(|(i, block)| output_item(index, i, block)).collect();
    let mut response = json!({
        "id": format!("resp_mock_{index}"),
        "object": "response",
        "created_at": now_unix(),
        "status": if stop_reason == "end_turn" { "completed" } else { "incomplete" },
        "model": model,
        "output": output,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "usage": usage_json(reply.usage),
    });
    if stop_reason != "end_turn" {
        response["incomplete_details"] = json!({"reason": stop_reason});
    }
    response
}

fn usage_json(usage: Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "input_tokens_details": {"cached_tokens": usage.cache_read_input_tokens},
        "output_tokens": usage.output_tokens,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": usage.input_tokens + usage.output_tokens,
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn tool_call_id(index: usize, block: usize) -> String {
    format!("call_mock_{index}_{block}")
}

fn output_item(index: usize, block_index: usize, block: &Block) -> Value {
    match block {
        Block::Text { text } => json!({
            "id": format!("msg_mock_{index}_{block_index}"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }),
        Block::Thinking { thinking, .. } => json!({
            "id": format!("rs_mock_{index}_{block_index}"),
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": thinking}],
            "content": Value::Null,
        }),
        Block::ToolUse { id, name, input } => json!({
            "id": format!("fc_mock_{index}_{block_index}"),
            "type": "function_call",
            "status": "completed",
            "call_id": id.clone().unwrap_or_else(|| tool_call_id(index, block_index)),
            "name": name,
            "arguments": serde_json::to_string(input).expect("input serializes"),
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

    let mut skeleton = response(index, model, reply, stop_reason);
    skeleton["status"] = json!("in_progress");
    skeleton["output"] = json!([]);
    events.push((
        delay,
        sse("response.created", json!({"type": "response.created", "sequence_number": 0, "response": skeleton})),
    ));
    if matches!(reply.fault, Some(Fault::BrokenStream)) {
        return events;
    }

    for (i, block) in reply.content.iter().enumerate() {
        delay = if i == 0 { slow_first } else { 0 };
        match block {
            Block::Text { text } => {
                let item_id = format!("msg_mock_{index}_{i}");
                events.push((
                    delay,
                    sse(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": i,
                            "item": {"id": item_id, "type": "message", "status": "in_progress", "role": "assistant", "content": []},
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.content_part.added",
                        json!({
                            "type": "response.content_part.added",
                            "item_id": item_id,
                            "output_index": i,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.output_text.delta",
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": item_id,
                            "output_index": i,
                            "content_index": 0,
                            "delta": text,
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.output_text.done",
                        json!({
                            "type": "response.output_text.done",
                            "item_id": item_id,
                            "output_index": i,
                            "content_index": 0,
                            "text": text,
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.content_part.done",
                        json!({
                            "type": "response.content_part.done",
                            "item_id": item_id,
                            "output_index": i,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": text, "annotations": []},
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": i,
                            "item": output_item(index, i, block),
                        }),
                    ),
                ));
            }
            Block::Thinking { thinking, .. } => {
                let item_id = format!("rs_mock_{index}_{i}");
                events.push((
                    delay,
                    sse(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": i,
                            "item": {"id": item_id, "type": "reasoning", "summary": [], "content": Value::Null},
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.reasoning_summary_part.added",
                        json!({
                            "type": "response.reasoning_summary_part.added",
                            "item_id": item_id,
                            "output_index": i,
                            "summary_index": 0,
                            "part": {"type": "summary_text", "text": ""},
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.reasoning_summary_text.delta",
                        json!({
                            "type": "response.reasoning_summary_text.delta",
                            "item_id": item_id,
                            "output_index": i,
                            "summary_index": 0,
                            "delta": thinking,
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.reasoning_summary_text.done",
                        json!({
                            "type": "response.reasoning_summary_text.done",
                            "item_id": item_id,
                            "output_index": i,
                            "summary_index": 0,
                            "text": thinking,
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.reasoning_summary_part.done",
                        json!({
                            "type": "response.reasoning_summary_part.done",
                            "item_id": item_id,
                            "output_index": i,
                            "summary_index": 0,
                            "part": {"type": "summary_text", "text": thinking},
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": i,
                            "item": output_item(index, i, block),
                        }),
                    ),
                ));
            }
            Block::ToolUse { id, name, input } => {
                let item_id = format!("fc_mock_{index}_{i}");
                let call_id = id.clone().unwrap_or_else(|| tool_call_id(index, i));
                let arguments = serde_json::to_string(input).expect("input serializes");
                events.push((
                    delay,
                    sse(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": i,
                            "item": {"id": item_id, "type": "function_call", "status": "in_progress", "call_id": call_id, "name": name, "arguments": ""},
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item_id,
                            "output_index": i,
                            "delta": arguments,
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.function_call_arguments.done",
                        json!({
                            "type": "response.function_call_arguments.done",
                            "item_id": item_id,
                            "output_index": i,
                            "arguments": arguments,
                        }),
                    ),
                ));
                events.push((
                    0,
                    sse(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": i,
                            "item": output_item(index, i, block),
                        }),
                    ),
                ));
            }
        }
    }

    events.push((
        0,
        sse(
            "response.completed",
            json!({"type": "response.completed", "response": response(index, model, reply, stop_reason)}),
        ),
    ));
    events
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
    fn tool_turn_streams_a_function_call() {
        let r = reply(vec![Block::ToolUse {
            id: Some("call_1".into()),
            name: "exec_command".into(),
            input: json!({"cmd": "true"}),
        }]);
        let all: String = stream_events(0, "gpt", &r, "tool_use").into_iter().map(|(_, e)| e).collect();
        for event in [
            "response.created",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.output_item.done",
            "response.completed",
        ] {
            assert!(all.contains(event), "missing {event} in {all}");
        }
    }
}
