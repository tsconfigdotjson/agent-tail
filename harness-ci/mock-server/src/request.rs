//! Reading a model request just enough to match a scenario step.

use crate::scenario::Expect;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Anthropic,
    OpenAi,
    OpenAiChat,
}

#[derive(Clone, Debug, Default)]
pub struct ToolResult {
    pub call_id: String,
    pub text: String,
}

/// The request, flattened to the things a scenario can check.
#[derive(Clone, Debug, Default)]
pub struct RequestView {
    /// All text in the conversation, including tool results.
    pub all_text: String,
    /// Text the user wrote (not tool results).
    pub user_text: String,
    pub tool_results: Vec<ToolResult>,
}

impl RequestView {
    pub fn from_anthropic(body: &Value) -> RequestView {
        let mut view = RequestView::default();
        append_system(&mut view, body.get("system"));
        let Some(messages) = body.get("messages").and_then(Value::as_array) else { return view };
        for message in messages {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            match message.get("content") {
                Some(Value::String(text)) => {
                    append(&mut view.all_text, text);
                    if role == "user" {
                        append(&mut view.user_text, text);
                    }
                }
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                        match kind {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    append(&mut view.all_text, text);
                                    if role == "user" {
                                        append(&mut view.user_text, text);
                                    }
                                }
                            }
                            "tool_result" => {
                                let call_id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                                let text = content_text(block.get("content"));
                                append(&mut view.all_text, &text);
                                view.tool_results.push(ToolResult { call_id: call_id.into(), text });
                            }
                            "tool_use" => {
                                // A previous assistant call is part of the conversation a
                                // scenario may want to recognise by tool name.
                                if let Some(name) = block.get("name").and_then(Value::as_str) {
                                    append(&mut view.all_text, name);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        view
    }

    pub fn from_openai_chat(body: &Value) -> RequestView {
        let mut view = RequestView::default();
        let Some(messages) = body.get("messages").and_then(Value::as_array) else { return view };
        for message in messages {
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "system" => append(&mut view.all_text, &content_text(message.get("content"))),
                "user" => {
                    let text = content_text(message.get("content"));
                    append(&mut view.all_text, &text);
                    append(&mut view.user_text, &text);
                }
                "assistant" => {
                    let text = content_text(message.get("content"));
                    append(&mut view.all_text, &text);
                    for call in message.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                            append(&mut view.all_text, name);
                        }
                    }
                }
                "tool" => {
                    let call_id = message.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                    let text = content_text(message.get("content"));
                    append(&mut view.all_text, &text);
                    view.tool_results.push(ToolResult { call_id: call_id.into(), text });
                }
                _ => {}
            }
        }
        view
    }

    pub fn from_openai(body: &Value) -> RequestView {
        let mut view = RequestView::default();
        match body.get("input") {
            Some(Value::String(text)) => append(&mut view.user_text, text),
            Some(Value::Array(items)) => {
                for item in items {
                    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                    match kind {
                        "message" | "" => {
                            let role = item.get("role").and_then(Value::as_str).unwrap_or("");
                            let text = content_text(item.get("content"));
                            append(&mut view.all_text, &text);
                            if role == "user" {
                                append(&mut view.user_text, &text);
                            }
                        }
                        "function_call_output" | "custom_tool_call_output" => {
                            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                            let text = content_text(item.get("output"));
                            append(&mut view.all_text, &text);
                            view.tool_results.push(ToolResult { call_id: call_id.into(), text });
                        }
                        "function_call" | "custom_tool_call" => {
                            if let Some(name) = item.get("name").and_then(Value::as_str) {
                                append(&mut view.all_text, name);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        view
    }
}

/// Anthropic and OpenAI both use a string or an array of `{type, text}` parts.
fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text").or_else(|| part.get("content")).and_then(Value::as_str).map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(_)) => {
            value.and_then(|v| v.get("text")).and_then(Value::as_str).unwrap_or_default().to_string()
        }
        _ => String::new(),
    }
}

fn append_system(view: &mut RequestView, system: Option<&Value>) {
    match system {
        Some(Value::String(text)) => append(&mut view.all_text, text),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(text) = part.as_str() {
                    append(&mut view.all_text, text);
                } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                    append(&mut view.all_text, text);
                }
            }
        }
        _ => {}
    }
}

fn append(into: &mut String, text: &str) {
    if !into.is_empty() && !text.is_empty() {
        into.push('\n');
    }
    into.push_str(text);
}

impl Expect {
    /// Whether this request is the one the step expected. The error is written for a
    /// human reading a failed CI run.
    pub fn check(&self, view: &RequestView, index: usize) -> Result<(), String> {
        if let Some(want) = self.request_index
            && want != index
        {
            return Err(format!("expected request #{want}, got #{index}"));
        }
        if let Some(want) = &self.contains
            && !view.all_text.contains(want)
        {
            return Err(format!("request did not contain {want:?}; text was {:?}", truncate(&view.all_text)));
        }
        if let Some(want) = &self.user_contains
            && !view.user_text.contains(want)
        {
            return Err(format!("user text did not contain {want:?}; user text was {:?}", truncate(&view.user_text)));
        }
        if let Some(want) = &self.user_also_contains
            && !view.user_text.contains(want)
        {
            return Err(format!("user text did not contain {want:?}; user text was {:?}", truncate(&view.user_text)));
        }
        if let Some(want) = &self.not_user_contains
            && view.user_text.contains(want)
        {
            return Err(format!("user text contained {want:?}, which this step rejects"));
        }
        if let Some(want) = &self.tool_result_id
            && !view.tool_results.iter().any(|r| &r.call_id == want)
        {
            return Err(format!(
                "no tool result for {want:?}; results were {:?}",
                view.tool_results.iter().map(|r| &r.call_id).collect::<Vec<_>>()
            ));
        }
        if let Some(want) = &self.tool_result_contains
            && !view.tool_results.iter().any(|r| r.text.contains(want))
        {
            return Err(format!(
                "no tool result contained {want:?}; results were {:?}",
                view.tool_results.iter().map(|r| truncate(&r.text)).collect::<Vec<_>>()
            ));
        }
        if let Some(want) = self.has_tool_result
            && want == view.tool_results.is_empty()
        {
            return Err(if want {
                "expected a tool result, got a request with none".into()
            } else {
                "expected no tool result, got one".into()
            });
        }
        Ok(())
    }
}

fn truncate(text: &str) -> String {
    const MAX: usize = 400;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_anthropic_tool_results() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "run it"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "ok"}]}]}
        ]});
        let view = RequestView::from_anthropic(&body);
        assert_eq!(view.user_text, "run it");
        assert_eq!(view.tool_results.len(), 1);
        assert_eq!(view.tool_results[0].call_id, "t1");
        assert_eq!(view.tool_results[0].text, "ok");
        assert!(view.all_text.contains("Bash"));
    }

    #[test]
    fn expects_report_the_mismatch() {
        let view = RequestView::from_anthropic(&json!({"messages": [{"role": "user", "content": "hi"}]}));
        let expect = Expect { user_contains: Some("bye".into()), ..Expect::default() };
        let err = expect.check(&view, 0).unwrap_err();
        assert!(err.contains("did not contain \"bye\""), "{err}");
    }

    #[test]
    fn system_text_is_visible_to_contains_and_not_to_user_contains() {
        let body = json!({
            "system": [{"type": "text", "text": "NESTED_AGENT in the system prompt"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let view = RequestView::from_anthropic(&body);
        assert!(view.all_text.contains("NESTED_AGENT"));
        assert!(!view.user_text.contains("NESTED_AGENT"));
        let string_system = RequestView::from_anthropic(&json!({
            "system": "OUTER_AGENT system string",
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert!(string_system.all_text.contains("OUTER_AGENT"));
    }

    #[test]
    fn a_later_user_message_can_be_required_or_rejected() {
        let body = json!({"messages": [
            {"role": "user", "content": "OUTER_AGENT"},
            {"role": "user", "content": "<task-notification><status>completed</status></task-notification>"},
        ]});
        let view = RequestView::from_anthropic(&body);
        let early = Expect {
            user_contains: Some("OUTER_AGENT".into()),
            not_user_contains: Some("<task-notification>".into()),
            ..Expect::default()
        };
        assert!(early.check(&view, 1).is_err());
        let late = Expect {
            user_contains: Some("OUTER_AGENT".into()),
            user_also_contains: Some("<task-notification>".into()),
            ..Expect::default()
        };
        assert!(late.check(&view, 1).is_ok());
    }
}
