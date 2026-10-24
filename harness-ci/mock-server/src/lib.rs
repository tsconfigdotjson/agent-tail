//! A scripted model server for harness CI.
//!
//! It answers the Anthropic Messages API and the OpenAI Responses API, streaming or not,
//! from a [`Scenario`] file. Every request is logged. A request that doesn't match the
//! next scripted step gets a 500 with a human-readable explanation, so a harness behaviour
//! change fails the run instead of hanging it.

pub mod anthropic;
pub mod openai;
pub mod request;
pub mod scenario;

use crate::request::{Api, RequestView};
use crate::scenario::{Fault, Scenario, Step};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
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
        self.scenario.steps.len().saturating_sub(self.inner.lock().expect("mock state").next_step)
    }

    pub fn requests(&self) -> u64 {
        self.inner.lock().expect("mock state").requests
    }

    fn take_step(&self, api: Api, path: &str, body: &Value) -> Result<(usize, Step), MockFailure> {
        let view = match api {
            Api::Anthropic => RequestView::from_anthropic(body),
            Api::OpenAi => RequestView::from_openai(body),
        };
        let mut inner = self.inner.lock().expect("mock state");
        let request_index = inner.requests as usize;
        inner.requests += 1;
        let next_step = inner.next_step;
        let Some(step) = self.scenario.steps.get(next_step).cloned() else {
            let message = format!(
                "unexpected request #{request_index} to {path}: scenario {:?} has no step {}",
                self.scenario.name, next_step
            );
            self.write_log(json!({
                "request_index": request_index,
                "api": api_name(api),
                "path": path,
                "body": body,
                "error": message,
            }));
            return Err(MockFailure { status: StatusCode::INTERNAL_SERVER_ERROR, message });
        };
        if let Err(message) = step.expect.check(&view, request_index) {
            let message = format!("request #{request_index} to {path} did not match step {next_step}: {message}");
            self.write_log(json!({
                "request_index": request_index,
                "api": api_name(api),
                "path": path,
                "body": body,
                "expected_step": next_step,
                "expect": step.expect,
                "error": message,
            }));
            return Err(MockFailure { status: StatusCode::INTERNAL_SERVER_ERROR, message });
        }
        inner.next_step += 1;
        self.write_log(json!({
            "request_index": request_index,
            "api": api_name(api),
            "path": path,
            "body": body,
            "matched_step": next_step,
            "remaining": self.scenario.steps.len() - inner.next_step,
        }));
        Ok((request_index, step))
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
    }
}

pub fn router(state: Arc<MockState>) -> Router {
    Router::new()
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(openai_responses))
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
        return title_response(&body);
    }
    let (index, step) = match state.take_step(Api::Anthropic, "/v1/messages", &body) {
        Ok(found) => found,
        Err(error) => return failure_response(error, Api::Anthropic),
    };
    if let Some(Fault::Http { status, message }) = step.reply.fault {
        return error_response(status, message.as_deref().unwrap_or("scripted failure"), Api::Anthropic);
    }
    let model = state.scenario.model(&step, "claude-sonnet-4-5");
    let stop_reason = state.scenario.stop_reason(&step);
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
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

fn title_response(body: &Value) -> Response {
    let reply = Step {
        expect: Default::default(),
        reply: scenario::Reply {
            model: None,
            content: vec![scenario::Block::Text { text: "Mock session title".into() }],
            stop_reason: None,
            usage: Default::default(),
            fault: None,
        },
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
    let input_tokens = body.len().div_ceil(4).max(1);
    Json(json!({"input_tokens": input_tokens})).into_response()
}

async fn openai_responses(State(state): State<Arc<MockState>>, body: Bytes) -> Response {
    let body: Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(error) => {
            return fail(StatusCode::BAD_REQUEST, format!("request is not JSON: {error}"), Api::OpenAi);
        }
    };
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
        MockState::new(Scenario { name: "test".into(), description: None, steps })
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
    async fn a_scripted_http_fault_returns_that_status() {
        let mut step = text_step("unused");
        step.reply.fault = Some(Fault::Http { status: 529, message: Some("overloaded".into()) });
        let app = router(state(vec![step]));
        let (status, body) = post(app, "/v1/messages", json!({"stream": false, "messages": []})).await;
        assert_eq!(status, StatusCode::from_u16(529).unwrap());
        assert!(body.contains("overloaded_error"), "{body}");
    }

    #[tokio::test]
    async fn answers_an_openai_turn() {
        let app = router(state(vec![text_step("hello")]));
        let (status, body) = post(app, "/v1/responses", json!({"stream": false, "input": "hi"})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("resp_mock_0"), "{body}");
    }
}
