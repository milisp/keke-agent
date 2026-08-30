use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use axum::Router;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::reply::Reply;
use crate::reply::ReplyBody;
use crate::reply::Script;
use crate::sse::SseFrame;
use crate::wire;

/// One route the mock serves, and the wire format it answers in.
///
/// The variants name wire formats rather than vendors: the same endpoint stands
/// in for every vendor that speaks that format, which is the point of testing
/// against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Endpoint {
    /// `POST /v1/chat/completions` — OpenAI-compatible chat completions.
    ChatCompletions,
    /// `POST /v1/responses` — the OpenAI Responses API.
    Responses,
    /// `POST /v1/messages` — the Anthropic Messages API.
    Messages,
    /// `GET /v1/models`.
    Models,
}

impl Endpoint {
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
            Self::Messages => "/v1/messages",
            Self::Models => "/v1/models",
        }
    }
}

/// One request the mock received, captured before any reply was chosen.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub endpoint: Endpoint,
    pub path: String,
    /// The parsed JSON body, or [`Value::Null`] for a body-less request.
    pub body: Value,
    /// Header names are lowercased, as HTTP/2 requires anyway.
    pub headers: Vec<(String, String)>,
}

impl RecordedRequest {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The `authorization` header, which is what most auth assertions are about.
    #[must_use]
    pub fn authorization(&self) -> Option<&str> {
        self.header("authorization")
    }

    /// The requested model, when the body named one.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.body.get("model").and_then(Value::as_str)
    }
}

struct MockState {
    scripts: Mutex<HashMap<Endpoint, VecDeque<Reply>>>,
    log: Mutex<Vec<RecordedRequest>>,
    models: Mutex<Vec<String>>,
}

/// Locks are only contended between the server task and the test thread, and a
/// panic in either fails the test on its own; recovering the guard keeps a
/// poisoned lock from turning one failure into a confusing second one.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An inference backend on an ephemeral localhost port.
///
/// Scripted replies are queued per endpoint and consumed in order; everything
/// that arrives is logged; dropping the server stops its task. Tests hold one
/// per case rather than sharing a fixture, so a leaked reply from one test
/// cannot change another's outcome.
pub struct MockInferenceServer {
    addr: SocketAddr,
    state: Arc<MockState>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl MockInferenceServer {
    /// Bind and serve. Panics rather than returning an error: a mock that
    /// cannot start is a broken test environment, not a case to handle.
    ///
    /// # Panics
    /// If the ephemeral port cannot be bound.
    pub async fn start() -> Self {
        let state = Arc::new(MockState {
            scripts: Mutex::new(HashMap::new()),
            log: Mutex::new(Vec::new()),
            models: Mutex::new(vec!["mock-model".to_owned()]),
        });

        let app = Router::new()
            .route(Endpoint::ChatCompletions.path(), post(chat_completions))
            .route(Endpoint::Responses.path(), post(responses))
            .route(Endpoint::Messages.path(), post(messages))
            .route(Endpoint::Models.path(), get(models))
            .with_state(state.clone());

        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) => panic!("mock inference server could not bind a port: {error}"),
        };
        let addr = match listener.local_addr() {
            Ok(addr) => addr,
            Err(error) => panic!("mock inference server has no local address: {error}"),
        };

        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(error) = served {
                tracing::warn!("mock inference server stopped: {error}");
            }
        });

        Self {
            addr,
            state,
            shutdown: Some(shutdown),
            task,
        }
    }

    /// e.g. `http://127.0.0.1:53412/v1` — what a provider's `base_url` wants.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// The origin without the `/v1` prefix.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Queue one reply for `endpoint`. Replies are consumed in order, one per
    /// request, so a multi-turn test scripts the whole conversation up front.
    pub fn script(&self, endpoint: Endpoint, reply: Reply) {
        lock(&self.state.scripts)
            .entry(endpoint)
            .or_default()
            .push_back(reply);
    }

    /// Replace the ids served by `GET /v1/models`.
    pub fn set_models<S: Into<String>>(&self, models: impl IntoIterator<Item = S>) {
        *lock(&self.state.models) = models.into_iter().map(Into::into).collect();
    }

    /// Every request so far, oldest first. Readable while the server runs.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.state.log).clone()
    }

    /// Requests to one endpoint, oldest first.
    #[must_use]
    pub fn requests_to(&self, endpoint: Endpoint) -> Vec<RecordedRequest> {
        lock(&self.state.log)
            .iter()
            .filter(|request| request.endpoint == endpoint)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn request_count(&self) -> usize {
        lock(&self.state.log).len()
    }

    /// Replies queued but never served, so a test can assert it scripted
    /// exactly what it meant to.
    #[must_use]
    pub fn pending_scripts(&self, endpoint: Endpoint) -> usize {
        lock(&self.state.scripts)
            .get(&endpoint)
            .map_or(0, VecDeque::len)
    }
}

impl Drop for MockInferenceServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        // Graceful shutdown waits for in-flight connections, and a test that
        // dropped the server has stopped caring about them; aborting releases
        // the port now so the next test can rebind it.
        self.task.abort();
    }
}

async fn chat_completions(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(&state, Endpoint::ChatCompletions, &headers, &body)
}

async fn responses(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(&state, Endpoint::Responses, &headers, &body)
}

async fn messages(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(&state, Endpoint::Messages, &headers, &body)
}

async fn models(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    record(&state, Endpoint::Models, &headers, Value::Null);
    if let Some(reply) = take_script(&state, Endpoint::Models) {
        return serve(reply, Endpoint::Models, "mock-model", false);
    }
    let data: Vec<Value> = lock(&state.models)
        .iter()
        .map(|id| json!({ "id": id, "object": "model", "created": 0, "owned_by": "keke-mock" }))
        .collect();
    json_response(
        StatusCode::OK,
        &[],
        &json!({ "object": "list", "data": data }),
    )
}

fn handle(
    state: &Arc<MockState>,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    record(state, endpoint, headers, parsed.clone());

    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("mock-model")
        .to_owned();
    // Anthropic and OpenAI both default to non-streaming, and a provider that
    // forgot to ask for a stream should see that mistake rather than a stream.
    let streaming = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let reply = take_script(state, endpoint).unwrap_or_else(|| default_reply(endpoint, &parsed));
    serve(reply, endpoint, &model, streaming)
}

fn record(state: &Arc<MockState>, endpoint: Endpoint, headers: &HeaderMap, body: Value) {
    let headers = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    lock(&state.log).push(RecordedRequest {
        endpoint,
        path: endpoint.path().to_owned(),
        body,
        headers,
    });
}

fn take_script(state: &Arc<MockState>, endpoint: Endpoint) -> Option<Reply> {
    lock(&state.scripts)
        .get_mut(&endpoint)
        .and_then(VecDeque::pop_front)
}

/// What an unscripted endpoint answers.
///
/// Hanging or 500ing would make a forgotten `script` call look like a provider
/// bug, so the mock answers with a well-formed turn that says so and quotes the
/// input back, which shows up intact in whatever the test asserts on.
fn default_reply(endpoint: Endpoint, body: &Value) -> Reply {
    Reply::text(format!(
        "mock: nothing scripted for {}; last input was {:?}",
        endpoint.path(),
        last_input(body).unwrap_or_else(|| "(none)".to_owned())
    ))
}

/// The last user-authored text, across all three request shapes.
fn last_input(body: &Value) -> Option<String> {
    let items = body
        .get("messages")
        .or_else(|| body.get("input"))
        .or_else(|| body.get("prompt"))?;
    if let Some(text) = items.as_str() {
        return Some(text.to_owned());
    }
    let last = items.as_array()?.last()?;
    let content = last.get("content").unwrap_or(last);
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts: Vec<String> = content
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    (!parts.is_empty()).then(|| parts.join(""))
}

fn serve(reply: Reply, endpoint: Endpoint, model: &str, streaming: bool) -> Response {
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match reply.body {
        ReplyBody::Json(value) => json_response(status, &reply.headers, &value),
        ReplyBody::RawSse(frames) => sse_response(status, &reply.headers, frames),
        ReplyBody::Script(script) => {
            if streaming {
                sse_response(status, &reply.headers, render(endpoint, &script, model))
            } else {
                json_response(
                    status,
                    &reply.headers,
                    &render_json(endpoint, &script, model),
                )
            }
        }
    }
}

fn render(endpoint: Endpoint, script: &Script, model: &str) -> Vec<SseFrame> {
    match endpoint {
        Endpoint::Responses => wire::responses_stream(script, model),
        Endpoint::Messages => wire::messages_stream(script, model),
        Endpoint::ChatCompletions | Endpoint::Models => {
            wire::chat_completions_stream(script, model)
        }
    }
}

fn render_json(endpoint: Endpoint, script: &Script, model: &str) -> Value {
    match endpoint {
        Endpoint::Responses => wire::responses_json(script, model),
        Endpoint::Messages => wire::messages_json(script, model),
        Endpoint::ChatCompletions | Endpoint::Models => wire::chat_completions_json(script, model),
    }
}

fn json_response(status: StatusCode, headers: &[(String, String)], body: &Value) -> Response {
    let mut response = (status, body.to_string()).into_response();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    apply_headers(&mut response, headers);
    response
}

/// Frames are streamed one HTTP chunk each rather than as one buffered body, so
/// a client that reads incrementally sees the same boundaries a vendor sends.
fn sse_response(
    status: StatusCode,
    headers: &[(String, String)],
    frames: Vec<SseFrame>,
) -> Response {
    let chunks = frames
        .into_iter()
        .map(|frame| Ok::<_, std::convert::Infallible>(frame.render()));
    let mut response = Response::new(Body::from_stream(futures::stream::iter(chunks)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-cache"));
    apply_headers(&mut response, headers);
    response
}

fn apply_headers(response: &mut Response, headers: &[(String, String)]) {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
}
