//! Google Generative Language (Gemini API) wire format, streaming and not.
//!
//! Gemini CLI 0.65.0 sends `:generateContent` for its routing classifier and
//! `:streamGenerateContent?alt=sse` for the main model turn. The mock translates the same
//! scenario blocks used by the other APIs into Gemini candidates.

use crate::scenario::{Block, Fault, Reply, Usage};
use serde_json::{Value, json};

pub fn response(index: usize, model: &str, reply: &Reply, stop_reason: &str) -> Value {
    let parts: Vec<Value> = reply.content.iter().enumerate().map(|(i, block)| part(index, i, block)).collect();
    json!({
        "candidates": [{
            "content": {"parts": parts, "role": "model"},
            "finishReason": finish_reason(stop_reason),
            "index": 0,
        }],
        "usageMetadata": usage_json(reply.usage),
        "modelVersion": model,
        "responseId": format!("gemini_mock_{index}"),
    })
}

fn part(index: usize, block_index: usize, block: &Block) -> Value {
    match block {
        Block::Text { text } => json!({"text": text}),
        Block::Thinking { thinking, .. } => json!({"text": thinking, "thought": true}),
        Block::ToolUse { id, name, input } => json!({
            "functionCall": {
                "id": id.clone().unwrap_or_else(|| tool_call_id(index, block_index)),
                "name": name,
                "args": input,
            }
        }),
    }
}

fn usage_json(usage: Usage) -> Value {
    json!({
        "promptTokenCount": usage.input_tokens,
        "candidatesTokenCount": usage.output_tokens,
        "cachedContentTokenCount": usage.cache_read_input_tokens,
        "totalTokenCount": usage.input_tokens + usage.output_tokens,
    })
}

fn finish_reason(stop_reason: &str) -> &'static str {
    match stop_reason {
        "max_tokens" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        "error" => "OTHER",
        _ => "STOP",
    }
}

fn tool_call_id(index: usize, block: usize) -> String {
    format!("call_mock_{index}_{block}")
}

fn sse(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// SSE frames as `(delay_ms_before_event, full_sse_frame)`.
pub fn stream_events(index: usize, model: &str, reply: &Reply, stop_reason: &str) -> Vec<(u64, String)> {
    let delay = match reply.fault {
        Some(Fault::Slow { delay_ms }) => delay_ms,
        _ => 0,
    };
    let slow_first = match reply.fault {
        Some(Fault::SlowFirstToken { delay_ms }) => delay_ms,
        _ => 0,
    };
    let mut events = Vec::new();
    let mut sent = false;
    for (i, block) in reply.content.iter().enumerate() {
        let wait = if sent { 0 } else { slow_first };
        let mut frame = response(index, model, &Reply { content: vec![block.clone()], ..reply.clone() }, stop_reason);
        frame["usageMetadata"] = Value::Null;
        events.push((if i == 0 { delay.max(wait) } else { wait }, sse(&frame)));
        sent = true;
        if matches!(reply.fault, Some(Fault::BrokenStream)) {
            return events;
        }
    }
    let final_frame = response(index, model, &Reply { content: Vec::new(), ..reply.clone() }, stop_reason);
    events.push((0, sse(&final_frame)));
    events
}

pub fn classifier_response() -> Value {
    let text = json!({"complexity_reasoning": "Simple read-only request.", "complexity_score": 10}).to_string();
    json!({
        "candidates": [{
            "content": {"parts": [{"text": text}], "role": "model"},
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
        "modelVersion": "gemini-3.5-flash-lite",
    })
}

pub fn error(status: u16, message: &str) -> Value {
    let status_text = match status {
        400 => "INVALID_ARGUMENT",
        401 | 403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        _ => "INTERNAL",
    };
    json!({"error": {"code": status, "message": message, "status": status_text}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(blocks: Vec<Block>) -> Reply {
        Reply { model: None, content: blocks, stop_reason: None, usage: Usage::default(), fault: None }
    }

    #[test]
    fn text_turn_has_a_candidate_part() {
        let value = response(0, "gemini-test", &reply(vec![Block::Text { text: "hi".into() }]), "end_turn");
        assert_eq!(value["candidates"][0]["content"]["parts"][0]["text"], "hi");
        assert_eq!(value["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn tool_turn_has_a_function_call() {
        let value = response(
            0,
            "gemini-test",
            &reply(vec![Block::ToolUse {
                id: Some("call_1".into()),
                name: "run_shell_command".into(),
                input: json!({"command": "true"}),
            }]),
            "tool_use",
        );
        assert_eq!(value["candidates"][0]["content"]["parts"][0]["functionCall"]["name"], "run_shell_command");
        assert_eq!(value["candidates"][0]["content"]["parts"][0]["functionCall"]["id"], "call_1");
    }

    #[test]
    fn stream_frames_are_sse_json() {
        let events = stream_events(0, "gemini-test", &reply(vec![Block::Text { text: "hi".into() }]), "end_turn");
        let all: String = events.into_iter().map(|(_, frame)| frame).collect();
        assert!(all.starts_with("data: {"), "{all}");
        assert!(all.contains("\"text\":\"hi\""), "{all}");
        assert!(all.contains("\"usageMetadata\""), "{all}");
    }
}
