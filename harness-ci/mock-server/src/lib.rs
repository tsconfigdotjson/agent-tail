//! A scripted model server for harness CI.
//!
//! It answers the Anthropic Messages API and the OpenAI Responses API, streaming or not,
//! from a [`Scenario`] file. Every request is logged. A request that doesn't match the
//! next scripted step gets a 500 with a human-readable explanation, so a harness behaviour
//! change fails the run instead of hanging it.

pub mod anthropic;
pub mod chat;
pub mod gemini;
pub mod openai;
pub mod request;
pub mod scenario;

use crate::request::{Api, RequestView};
use crate::scenario::{Dispatch, Fault, Scenario, Step};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared server state: the scenario, where it is up to, and the request log.
#[derive(Debug)]
pub struct MockState {
    pub scenario: Scenario,
    inner: Mutex<Inner>,
    log: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct Inner {
    next_step: usize,
    requests: u64,
    used: Vec<bool>,
}

#[derive(Debug)]
pub struct MockFailure {
    pub status: StatusCode,
    pub message: String,
}

impl MockState {
    pub fn new(scenario: Scenario) -> Arc<Self> {
        Arc::new(MockState { scenario, inner: Mutex::new(Inner::default()), log: None })
    }

    pub fn with_log(self: Arc<Self>, path: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(MockState {
            scenario: self.scenario.clone(),
            inner: Mutex::new(Inner::default()),
            log: Some(path.into()),
        })
    }

    pub fn remaining(&self) -> usize {
        let inner = self.inner.lock().expect("mock state");
        self.remaining_of(&inner)
    }

    fn remaining_of(&self, inner: &Inner) -> usize {
        match self.scenario.dispatch {
            Dispatch::Sequential => self.scenario.steps.len().saturating_sub(inner.next_step),
            Dispatch::FirstMatch => {
                let used = inner.used.iter().filter(|flag| **flag).count();
                self.scenario.steps.len().saturating_sub(used)
            }
        }
    }

    pub fn requests(&self) -> u64 {
        self.inner.lock().expect("mock state").requests
    }

    fn take_step(&self, api: Api, path: &str, body: &Value) -> Result<(usize, Step), MockFailure> {
        let view = match api {
            Api::Anthropic => RequestView::from_anthropic(body),
            Api::OpenAi => RequestView::from_openai(body),
            Api::OpenAiChat => RequestView::from_openai_chat(body),
            Api::Gemini => RequestView::from_gemini(body),
        };
        let mut inner = self.inner.lock().expect("mock state");
        let request_index = inner.requests as usize;
        inner.requests += 1;
        let picked = match self.scenario.dispatch {
            Dispatch::Sequential => self.pick_sequential(&mut inner, &view, path, request_index),
            Dispatch::FirstMatch => self.pick_first_match(&mut inner, &view, path, request_index),
        };
        let (step_index, step) = match picked {
            Ok(found) => found,
            Err(message) => {
                self.write_log(json!({
                    "request_index": request_index,
                    "api": api_name(api),
                    "path": path,
                    "body": body,
                    "error": message,
                }));
                return Err(MockFailure { status: StatusCode::INTERNAL_SERVER_ERROR, message });
            }
        };
        let remaining = self.remaining_of(&inner);
        self.write_log(json!({
            "request_index": request_index,
            "api": api_name(api),
            "path": path,
            "body": body,
            "matched_step": step_index,
            "remaining": remaining,
        }));
        Ok((request_index, step))
    }

    fn pick_sequential(
        &self,
        inner: &mut Inner,
        view: &RequestView,
        path: &str,
        request_index: usize,
    ) -> Result<(usize, Step), String> {
        let next_step = inner.next_step;
        let Some(step) = self.scenario.steps.get(next_step).cloned() else {
            return Err(format!(
                "unexpected request #{request_index} to {path}: scenario {:?} has no step {next_step}",
                self.scenario.name
            ));
        };
        if let Err(message) = step.expect.check(view, request_index) {
            return Err(format!("request #{request_index} to {path} did not match step {next_step}: {message}"));
        }
        inner.next_step += 1;
        Ok((next_step, step))
    }

    fn pick_first_match(
        &self,
        inner: &mut Inner,
        view: &RequestView,
        path: &str,
        request_index: usize,
    ) -> Result<(usize, Step), String> {
        if inner.used.len() != self.scenario.steps.len() {
            inner.used = vec![false; self.scenario.steps.len()];
        }
        let mut failures = Vec::new();
        for (index, step) in self.scenario.steps.iter().enumerate() {
            if inner.used[index] {
                continue;
            }
            match step.expect.check(view, request_index) {
                Ok(()) => {
                    inner.used[index] = true;
                    return Ok((index, step.clone()));
                }
                Err(message) => failures.push(format!("step {index}: {message}")),
            }
        }
        let detail = if failures.is_empty() { "scenario has no steps left".to_string() } else { failures.join("; ") };
        Err(format!("request #{request_index} to {path} matched no unused step: {detail}"))
    }

    fn write_log(&self, entry: Value) {
        let Some(path) = &self.log else { return };
        let line = serde_json::to_string(&entry).unwrap_or_else(|_| "{}".into());
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            eprintln!("mock-server: create log dir {}: {error}", parent.display());
            return;
        }
        if let Err(error) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()))
        {
            eprintln!("mock-server: write log {}: {error}", path.display());
        }
    }

    fn log_simple(&self, method: &str, path: &str) {
        self.write_log(json!({"method": method, "path": path}));
    }
}

fn api_name(api: Api) -> &'static str {
    match api {
        Api::Anthropic => "anthropic",
        Api::OpenAi => "openai",
        Api::OpenAiChat => "openai_chat",
        Api::Gemini => "gemini",
    }
}

pub fn router(state: Arc<MockState>) -> Router {
    Router::new()
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(openai_responses))
        .route("/v1/chat/completions", post(openai_chat_completions))
        .route("/chat/completions", post(openai_chat_completions))
        .route("/v1beta/models/{*path}", post(gemini_generate))
        .route("/v1/models", get(models))
        .fallback(fallback)
        .with_state(state)
}

async fn anthropic_messages(State(state): State<Arc<MockState>>, body: Bytes) -> Response {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            return fail(StatusCode::BAD_REQUEST, format!("request is not JSON: {error}"), Api::Anthropic);
        }
    };
    if is_title_request(&body) {
        state.log_simple("POST", "/v1/messages?source=title");
        return title_response(&body, state.scenario.title.as_deref());
    }
    let (index, step) = match state.take_step(Api::Anthropic, "/v1/messages", &body) {
        Ok(found) => found,
        Err(error) => return failure_response(error, Api::Anthropic),
    };
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    match &step.reply.fault {
        Some(Fault::Http { status, message }) => {
            return error_response(*status, message.as_deref().unwrap_or("scripted failure"), Api::Anthropic);
        }
        Some(Fault::BrokenStream) if !stream => {
            return error_response(500, "connection closed before a complete message", Api::Anthropic);
        }
        _ => {}
    }
    let requested = body.get("model").and_then(Value::as_str).unwrap_or("claude-sonnet-4-5");
    let model = state.scenario.model(&step, requested);
    let stop_reason = state.scenario.stop_reason(&step);
    if stream {
        let events = anthropic::stream_events(index, &model, &step.reply, &stop_reason);
        sse_response(events)
    } else {
        Json(anthropic::message(index, &model, &step.reply, &stop_reason)).into_response()
    }
}

/// Claude Code asks the model for a session title on its own request. That request is not a
/// scenario step: answer it with a fixed title and leave the script alone. Detected from the
/// title system prompt, which is present regardless of `--bare`.
fn is_title_request(body: &Value) -> bool {
    body.get("system").and_then(Value::as_array).is_some_and(|system| {
        system.iter().any(|part| {
            part.get("text").and_then(Value::as_str).is_some_and(|text| text.contains("naming a coding session"))
        })
    })
}

fn title_text(_body: &Value, override_title: Option<&str>) -> String {
    let title = override_title.unwrap_or("Mock session title");
    json!({"title": title}).to_string()
}

/// Codex asks for a one-line task title on its own Responses request. Like Claude's title
/// request, it is not a scenario step: answer it with a fixed title and leave the script alone.
fn is_codex_title_request(body: &Value) -> bool {
    body.get("input").is_some_and(|input| {
        input.as_str().is_some_and(|text| text.contains("single-line task title"))
            || input.as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("content").is_some_and(|content| {
                        content.as_str().is_some_and(|text| text.contains("single-line task title"))
                            || content.as_array().is_some_and(|parts| {
                                parts.iter().any(|part| {
                                    part.get("text")
                                        .and_then(Value::as_str)
                                        .is_some_and(|text| text.contains("single-line task title"))
                                })
                            })
                    })
                })
            })
    })
}

fn openai_title_response(body: &Value, override_title: Option<&str>) -> Response {
    let reply = scenario::Reply {
        model: None,
        content: vec![scenario::Block::Text { text: override_title.unwrap_or("Mock session title").to_string() }],
        stop_reason: None,
        usage: Default::default(),
        fault: None,
    };
    let model = body.get("model").and_then(Value::as_str).unwrap_or("gpt-5");
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(true);
    if stream {
        sse_response(openai::stream_events(9000, model, &reply, "end_turn"))
    } else {
        Json(openai::response(9000, model, &reply, "end_turn")).into_response()
    }
}

fn title_response(body: &Value, override_title: Option<&str>) -> Response {
    let reply = Step {
        expect: Default::default(),
        reply: scenario::Reply {
            model: None,
            content: vec![scenario::Block::Text { text: title_text(body, override_title) }],
            stop_reason: None,
            usage: Default::default(),
            fault: None,
        },
        repeat: 1,
    };
    let model = body.get("model").and_then(Value::as_str).unwrap_or("claude-sonnet-4-5");
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        sse_response(anthropic::stream_events(9000, model, &reply.reply, "end_turn"))
    } else {
        Json(anthropic::message(9000, model, &reply.reply, "end_turn")).into_response()
    }
}

async fn count_tokens(State(state): State<Arc<MockState>>, body: Bytes) -> Response {
    state.log_simple("POST", "/v1/messages/count_tokens");
    let input_tokens = state.scenario.count_tokens.unwrap_or_else(|| body.len().div_ceil(4).max(1) as u64);
    Json(json!({"input_tokens": input_tokens})).into_response()
}

async fn openai_responses(State(state): State<Arc<MockState>>, body: Bytes) -> Response {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            return fail(StatusCode::BAD_REQUEST, format!("request is not JSON: {error}"), Api::OpenAi);
        }
    };
    if is_codex_title_request(&body) {
        state.log_simple("POST", "/v1/responses?source=title");
        return openai_title_response(&body, state.scenario.title.as_deref());
    }
    let (index, step) = match state.take_step(Api::OpenAi, "/v1/responses", &body) {
        Ok(found) => found,
        Err(error) => return failure_response(error, Api::OpenAi),
    };
    if let Some(Fault::Http { status, message }) = step.reply.fault {
        return error_response(status, message.as_deref().unwrap_or("scripted failure"), Api::OpenAi);
    }
    let model = state.scenario.model(&step, "gpt-5");
    let stop_reason = state.scenario.stop_reason(&step);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(true);
    if stream {
        let events = openai::stream_events(index, &model, &step.reply, &stop_reason);
        sse_response(events)
    } else {
        Json(openai::response(index, &model, &step.reply, &stop_reason)).into_response()
    }
}

async fn openai_chat_completions(State(state): State<Arc<MockState>>, body: Bytes) -> Response {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            return fail(StatusCode::BAD_REQUEST, format!("request is not JSON: {error}"), Api::OpenAiChat);
        }
    };
    if is_opencode_title_request(&body) {
        state.log_simple("POST", "/v1/chat/completions?source=title");
        return opencode_title_response(&body, state.scenario.title.as_deref());
    }
    let (index, step) = match state.take_step(Api::OpenAiChat, "/v1/chat/completions", &body) {
        Ok(found) => found,
        Err(error) => return failure_response(error, Api::OpenAiChat),
    };
    if let Some(Fault::Http { status, message }) = step.reply.fault {
        return error_response(status, message.as_deref().unwrap_or("scripted failure"), Api::OpenAiChat);
    }
    let model = state.scenario.model(&step, "test-model");
    let stop_reason = state.scenario.stop_reason(&step);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        sse_response(chat::stream_events(index, &model, &step.reply, &stop_reason))
    } else {
        Json(chat::completion(index, &model, &step.reply, &stop_reason)).into_response()
    }
}

/// OpenCode asks the model for a session title on its own request. Its system prompt starts
/// with this fixed sentence; answer it without consuming a scenario step.
fn is_opencode_title_request(body: &Value) -> bool {
    body.get("messages").and_then(Value::as_array).is_some_and(|messages| {
        messages.iter().any(|message| {
            message.get("role").and_then(Value::as_str) == Some("system")
                && message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.starts_with("You are a title generator."))
        })
    })
}

fn opencode_title_response(body: &Value, override_title: Option<&str>) -> Response {
    let reply = scenario::Reply {
        model: None,
        content: vec![scenario::Block::Text { text: override_title.unwrap_or("Mock session title").to_string() }],
        stop_reason: None,
        usage: Default::default(),
        fault: None,
    };
    let model = body.get("model").and_then(Value::as_str).unwrap_or("test-model");
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        sse_response(chat::stream_events(9000, model, &reply, "end_turn"))
    } else {
        Json(chat::completion(9000, model, &reply, "end_turn")).into_response()
    }
}

async fn gemini_generate(State(state): State<Arc<MockState>>, uri: Uri, body: Bytes) -> Response {
    let path = uri.path().to_string();
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            return fail(StatusCode::BAD_REQUEST, format!("request is not JSON: {error}"), Api::Gemini);
        }
    };
    if is_gemini_classifier_request(&body) {
        state.log_simple("POST", &format!("{path}?source=classifier"));
        return Json(gemini::classifier_response()).into_response();
    }
    if !path.contains(":streamGenerateContent") && is_gemini_auxiliary_request(&body) {
        state.log_simple("POST", &format!("{path}?source=auxiliary"));
        let reply = scenario::Reply {
            model: None,
            content: vec![scenario::Block::Text {
                text: state.scenario.title.clone().unwrap_or_else(|| "Mock session title".into()),
            }],
            stop_reason: None,
            usage: Default::default(),
            fault: None,
        };
        let model = body.get("model").and_then(Value::as_str).unwrap_or("gemini-test");
        return Json(gemini::response(9000, model, &reply, "end_turn")).into_response();
    }
    let (index, step) = match state.take_step(Api::Gemini, &path, &body) {
        Ok(found) => found,
        Err(error) => return failure_response(error, Api::Gemini),
    };
    if let Some(Fault::Http { status, message }) = step.reply.fault {
        return error_response(status, message.as_deref().unwrap_or("scripted failure"), Api::Gemini);
    }
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| path.split("/models/").nth(1).and_then(|rest| rest.split(':').next()))
        .unwrap_or("gemini-test");
    let model = state.scenario.model(&step, requested);
    let stop_reason = state.scenario.stop_reason(&step);
    if path.contains(":streamGenerateContent") {
        sse_response(gemini::stream_events(index, &model, &step.reply, &stop_reason))
    } else {
        Json(gemini::response(index, &model, &step.reply, &stop_reason)).into_response()
    }
}

fn is_gemini_classifier_request(body: &Value) -> bool {
    body.pointer("/generationConfig/responseJsonSchema/properties/complexity_score").is_some()
}

fn is_gemini_auxiliary_request(body: &Value) -> bool {
    if body.get("tools").is_some() {
        return false;
    }
    let system = body
        .pointer("/systemInstruction/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    system.contains("session title") || system.contains("summarize") || system.contains("summary of the conversation")
}

async fn models(State(state): State<Arc<MockState>>) -> Response {
    state.log_simple("GET", "/v1/models");
    Json(json!({"object": "list", "data": [], "has_more": false})).into_response()
}

async fn fallback(State(state): State<Arc<MockState>>, req: Request) -> Response {
    state.log_simple(req.method().as_str(), req.uri().path());
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"message": format!("mock-server has no route for {} {}", req.method(), req.uri().path())}})),
    )
        .into_response()
}

fn sse_response(events: Vec<(u64, String)>) -> Response {
    let stream = futures_util::stream::unfold(events.into_iter(), |mut events| async move {
        let (delay, frame) = events.next()?;
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        Some((Ok::<_, Infallible>(Bytes::from(frame)), events))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .expect("valid response")
}

fn fail(status: StatusCode, message: String, api: Api) -> Response {
    failure_response(MockFailure { status, message }, api)
}

fn failure_response(error: MockFailure, api: Api) -> Response {
    eprintln!("mock-server: {}", error.message);
    error_response(error.status.as_u16(), &error.message, api)
}

fn error_response(status: u16, message: &str, api: Api) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = match api {
        Api::Anthropic => anthropic::error(status.as_u16(), message),
        Api::OpenAi => openai::error(status.as_u16(), message),
        Api::OpenAiChat => chat::error(status.as_u16(), message),
        Api::Gemini => gemini::error(status.as_u16(), message),
    };
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use scenario::{Block, Expect, Reply, Usage};
    use tower::ServiceExt;

    fn state(steps: Vec<Step>) -> Arc<MockState> {
        MockState::new(Scenario {
            name: "test".into(),
            description: None,
            dispatch: Dispatch::Sequential,
            title: None,
            count_tokens: None,
            steps,
        })
    }

    fn text_step(text: &str) -> Step {
        Step {
            expect: Expect::default(),
            reply: Reply {
                model: None,
                content: vec![Block::Text { text: text.into() }],
                stop_reason: None,
                usage: Usage::default(),
                fault: None,
            },
            repeat: 1,
        }
    }

    async fn post(app: Router, path: &str, body: Value) -> (StatusCode, String) {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let res = app.oneshot(req).await.expect("response");
        let status = res.status();
        let body = to_bytes(res.into_body(), usize::MAX).await.expect("body");
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn answers_a_nonstreaming_anthropic_turn() {
        let app = router(state(vec![text_step("hello")]));
        let (status, body) =
            post(app, "/v1/messages", json!({"stream": false, "messages": [{"role": "user", "content": "hi"}]})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("msg_mock_0"), "{body}");
    }

    #[tokio::test]
    async fn streams_an_anthropic_turn() {
        let app = router(state(vec![text_step("hello")]));
        let (status, body) = post(app, "/v1/messages?beta=true", json!({"stream": true, "messages": []})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("event: message_start"), "{body}");
        assert!(body.contains("event: message_stop"), "{body}");
    }

    #[tokio::test]
    async fn an_unexpected_request_fails_loudly() {
        let app = router(state(vec![text_step("one")]));
        let (status, _body) = post(
            app.clone(),
            "/v1/messages",
            json!({"stream": false, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = post(app, "/v1/messages", json!({"stream": false, "messages": []})).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("no step 1"), "{body}");
    }

    #[tokio::test]
    async fn a_mismatched_request_fails_loudly() {
        let mut step = text_step("one");
        step.expect.user_contains = Some("wanted".into());
        let app = router(state(vec![step]));
        let (status, body) =
            post(app, "/v1/messages", json!({"stream": false, "messages": [{"role": "user", "content": "other"}]}))
                .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("did not match step 0"), "{body}");
    }

    #[tokio::test]
    async fn a_title_request_does_not_consume_a_step() {
        let app = router(state(vec![text_step("hello")]));
        let title = json!({
            "stream": false,
            "model": "claude-sonnet-4-5",
            "system": [{"type": "text", "text": "You are naming a coding session so the user can pick it out."}],
            "messages": [{"role": "user", "content": "name this"}],
        });
        let (status, body) = post(app.clone(), "/v1/messages", title).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Mock session title"), "{body}");
        let (status, body) =
            post(app, "/v1/messages", json!({"stream": false, "messages": [{"role": "user", "content": "hi"}]})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("msg_mock_0"), "{body}");
    }

    #[tokio::test]
    async fn a_broken_stream_fault_fails_a_nonstreaming_retry() {
        let mut step = text_step("unused");
        step.reply.fault = Some(Fault::BrokenStream);
        let app = router(state(vec![step]));
        let (status, body) = post(app, "/v1/messages", json!({"stream": false, "messages": []})).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.contains("unused"), "{body}");
        assert!(body.contains("api_error"), "{body}");
    }

    #[tokio::test]
    async fn a_scripted_http_fault_returns_that_status() {
        let mut step = text_step("unused");
        step.reply.fault = Some(Fault::Http { status: 529, message: Some("overloaded".into()) });
        let app = router(state(vec![step]));
        let (status, body) = post(app, "/v1/messages", json!({"stream": false, "messages": []})).await;
        assert_eq!(status, StatusCode::from_u16(529).unwrap());
        assert!(body.contains("overloaded_error"), "{body}");
    }

    #[tokio::test]
    async fn answers_gemini_classifier_and_main_turn() {
        let app = router(state(vec![text_step("hello")]));
        let classifier = json!({
            "generationConfig": {
                "responseJsonSchema": {"properties": {"complexity_score": {"type": "INTEGER"}}}
            },
            "contents": [{"role": "user", "parts": [{"text": "say hello"}]}]
        });
        let (status, body) = post(app.clone(), "/v1beta/models/gemini-test:generateContent", classifier).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("complexity_score"), "{body}");
        let (status, body) = post(
            app,
            "/v1beta/models/gemini-test:streamGenerateContent?alt=sse",
            json!({"contents": [{"role": "user", "parts": [{"text": "say hello"}]}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("data: {"), "{body}");
        assert!(body.contains("hello"), "{body}");
    }

    #[tokio::test]
    async fn answers_an_openai_turn() {
        let app = router(state(vec![text_step("hello")]));
        let (status, body) = post(app, "/v1/responses", json!({"stream": false, "input": "hi"})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("resp_mock_0"), "{body}");
    }

    #[tokio::test]
    async fn a_codex_title_request_does_not_consume_a_step() {
        let app = router(state(vec![text_step("hello")]));
        let title = json!({
            "stream": false,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Generate a concise, single-line task title of at most 36 characters."}]
            }]
        });
        let (status, body) = post(app.clone(), "/v1/responses", title).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Mock session title"), "{body}");
        let (status, body) = post(app, "/v1/responses", json!({"stream": false, "input": "hi"})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("hello"), "{body}");
    }

    fn with_user(text: &str, user: &str, tool: Option<bool>) -> Step {
        let mut step = text_step(text);
        step.expect.user_contains = Some(user.into());
        step.expect.has_tool_result = tool;
        step
    }

    #[tokio::test]
    async fn sequential_dispatch_rejects_a_request_that_only_matches_a_later_step() {
        let app = router(state(vec![with_user("one", "first", None), with_user("two", "second", None)]));
        let (status, body) =
            post(app, "/v1/messages", json!({"stream": false, "messages": [{"role": "user", "content": "second"}]}))
                .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("did not match step 0"), "{body}");
    }

    #[tokio::test]
    async fn first_match_answers_a_subagent_before_the_parent_continues() {
        let mut continuation = with_user("parent continues", "delegate the summary", Some(true));
        continuation.expect.not_user_contains = Some("<task-notification>".into());
        let child = with_user("from the subagent", "OUTER_AGENT", Some(false));
        let initial = with_user("launch", "delegate the summary", Some(false));
        let scenario = Scenario {
            name: "bg".into(),
            description: None,
            dispatch: Dispatch::FirstMatch,
            title: None,
            count_tokens: None,
            steps: vec![continuation, child, initial],
        };
        let app = router(MockState::new(scenario));
        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({"stream": false, "messages": [{"role": "user", "content": "delegate the summary"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("launch"), "{body}");
        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({"stream": false, "messages": [{"role": "user", "content": "OUTER_AGENT please summarize"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("from the subagent"), "{body}");
        let (status, body) = post(
            app,
            "/v1/messages",
            json!({"stream": false, "messages": [
                {"role": "user", "content": "delegate the summary"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_mock_0_0", "content": "async_launched"}]}
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("parent continues"), "{body}");
    }

    #[tokio::test]
    async fn the_background_scenario_keeps_parent_and_subagent_replies_apart() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/background_agent.json");
        let scenario = Scenario::from_path(&path).expect("background scenario");
        assert_eq!(scenario.dispatch, Dispatch::FirstMatch);
        let state = MockState::new(scenario);
        let app = router(state.clone());

        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({"stream": false, "messages": [{"role": "user", "content": "delegate the summary"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("OUTER_AGENT"), "{body}");

        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({
                "stream": false,
                "system": [{"type": "text", "text": "OUTER_AGENT system prompt"}],
                "messages": [{"role": "user", "content": "do the delegated work"}],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("NESTED_AGENT"), "{body}");

        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({"stream": false, "messages": [{"role": "user", "content": "NESTED_AGENT: reply with the single word nested-done."}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("nested-done"), "{body}");

        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({
                "stream": false,
                "messages": [
                    {"role": "user", "content": "delegate the summary"},
                    {"role": "assistant", "content": [{"type": "tool_use", "name": "Agent", "input": {"prompt": "OUTER_AGENT: launch"}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_mock_0_0", "content": "async_launched OUTER_AGENT"}]}
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("has started"), "{body}");
        assert_eq!(state.remaining(), 3);
    }

    #[tokio::test]
    async fn an_anthropic_reply_echoes_the_request_model() {
        let app = router(state(vec![text_step("hello")]));
        let (status, body) = post(
            app,
            "/v1/messages",
            json!({"stream": false, "model": "claude-haiku-4-5", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("claude-haiku-4-5"), "{body}");
    }

    #[tokio::test]
    async fn a_non_ascii_prompt_gets_an_emoji_title_unless_overridden() {
        let app = router(state(vec![text_step("hello")]));
        let title = json!({
            "stream": false,
            "system": [{"type": "text", "text": "You are naming a coding session so the user can pick it out."}],
            "messages": [{"role": "user", "content": "こんにちは"}],
        });
        let (status, body) = post(app.clone(), "/v1/messages", title).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Mock session title"), "{body}");
        assert!(body.contains(r#"\"title\""#), "{body}");
        let (status, body) =
            post(app, "/v1/messages", json!({"stream": false, "messages": [{"role": "user", "content": "hi"}]})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("hello"), "{body}");

        let scenario = Scenario {
            name: "titled".into(),
            description: None,
            dispatch: Dispatch::Sequential,
            title: Some("絵文字タイトル 🎉".into()),
            count_tokens: None,
            steps: vec![text_step("hello")],
        };
        let app = router(MockState::new(scenario));
        let (status, body) = post(
            app,
            "/v1/messages",
            json!({
                "stream": false,
                "system": [{"type": "text", "text": "naming a coding session"}],
                "messages": [{"role": "user", "content": "ascii"}],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("絵文字タイトル 🎉"), "{body}");
    }

    #[tokio::test]
    async fn count_tokens_uses_the_scenario_override() {
        let scenario = Scenario {
            name: "counted".into(),
            description: None,
            dispatch: Dispatch::Sequential,
            title: None,
            count_tokens: Some(20_000),
            steps: vec![text_step("hello")],
        };
        let app = router(MockState::new(scenario));
        let (status, body) = post(app, "/v1/messages/count_tokens", json!({"messages": []})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("20000"), "{body}");
    }

    #[tokio::test]
    async fn the_compact_manual_scenario_answers_summary_before_the_follow_up() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios/compact_manual.json");
        let scenario = Scenario::from_path(&path).expect("compact manual");
        assert_eq!(scenario.dispatch, Dispatch::FirstMatch);
        let state = MockState::new(scenario);
        let app = router(state.clone());
        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({"stream": false, "messages": [{"role": "user", "content": "remember COMPACT-TOKEN-ALPHA"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Noted COMPACT-TOKEN-ALPHA"), "{body}");
        let (status, body) = post(
            app.clone(),
            "/v1/messages",
            json!({
                "stream": false,
                "messages": [
                    {"role": "user", "content": "remember COMPACT-TOKEN-ALPHA"},
                    {"role": "user", "content": "Your task is to create a detailed summary of the conversation so far"}
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("SUMMARY-MARKER-BETA"), "{body}");
        let (status, body) = post(
            app,
            "/v1/messages",
            json!({
                "stream": false,
                "messages": [
                    {"role": "user", "content": "remember COMPACT-TOKEN-ALPHA"},
                    {"role": "user", "content": "SUMMARY-MARKER-BETA. The user asked to remember COMPACT-TOKEN-ALPHA."},
                    {"role": "user", "content": "what token did I ask you to remember?"}
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("The token is COMPACT-TOKEN-ALPHA"), "{body}");
        assert_eq!(state.remaining(), 0);
    }
}
