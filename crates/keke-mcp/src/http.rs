//! Talking to a server that lives behind a URL rather than behind a pipe.
//!
//! Two transports, because the ecosystem has two and a person adding
//! `https://mcp.vercel.com` should not have to know which one it implements:
//!
//! - **Streamable HTTP** — every request is a `POST`, and the answer comes back
//!   either as JSON or as a one-message SSE stream on that same response.
//! - **HTTP+SSE**, the older shape — a long-lived `GET` stream carries every
//!   answer, and the first event on it names the URL requests are posted to.
//!
//! What is shared with the stdio transport is the JSON-RPC framing and the
//! correlation of an answer to the request that is owed it; only the pipe
//! differs. So the `GET`-stream case reuses [`crate::client::Pending`]
//! wholesale, and the `POST`-per-request case needs no waiter map at all — the
//! answer arrives on the response to the request that asked.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use futures::StreamExt as _;
use serde_json::Value;
use serde_json::json;

use crate::auth::ServerAuth;
use crate::client::Pending;
use crate::client::RpcError;
use crate::client::answer;

/// The header a server uses to hand out, and then to recognize, a session.
const SESSION_HEADER: &str = "mcp-session-id";

/// One connection to a remote MCP server.
pub(crate) struct HttpConnection {
    client: reqwest::Client,
    /// Where requests are posted. For streamable HTTP this is the configured
    /// URL; for HTTP+SSE it is whatever the stream's `endpoint` event named.
    post_url: String,
    /// Configured headers, with `${VAR}` references already expanded.
    headers: Vec<(String, String)>,
    /// The session the server assigned, once it has assigned one. Sent back on
    /// every later request, which is what lets a server keep state for us.
    session: Mutex<Option<String>>,
    /// Waiters, for the `GET`-stream transport only. Streamable HTTP leaves
    /// this empty because its answers never arrive out of band.
    pending: Pending,
    /// Whether answers come back on the long-lived stream.
    out_of_band: bool,
    next_id: AtomicU64,
    /// The OAuth credential for this server, when it has one. `None` is an
    /// unauthenticated server, which plenty of local and internal ones are.
    auth: Option<Arc<ServerAuth>>,
    /// The server's name, so a failure names the thing a person would act on
    /// rather than a URL they would have to match up themselves.
    label: String,
}

impl std::fmt::Debug for HttpConnection {
    /// Header *names* only: a configured `Authorization` is a secret, and this
    /// value lives for the whole session.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpConnection")
            .field("post_url", &self.post_url)
            .field(
                "headers",
                &self.headers.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl HttpConnection {
    /// Open a streamable-HTTP connection. Nothing is sent until the first
    /// request, so this cannot fail on the network.
    pub(crate) fn streamable(
        url: &str,
        headers: Vec<(String, String)>,
        auth: Option<Arc<ServerAuth>>,
        label: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            client: client()?,
            post_url: url.to_string(),
            headers,
            session: Mutex::new(None),
            pending: Pending::default(),
            out_of_band: false,
            next_id: AtomicU64::new(1),
            auth,
            label: label.to_string(),
        })
    }

    /// Open an HTTP+SSE connection: start the stream, and wait for it to name
    /// the endpoint requests go to.
    ///
    /// A stream that never names one is a failure here rather than later,
    /// because there is nowhere to send the first request until it does.
    pub(crate) async fn sse(
        url: &str,
        headers: Vec<(String, String)>,
        auth: Option<Arc<ServerAuth>>,
        label: &str,
        open_timeout_millis: u64,
    ) -> Result<Self, String> {
        let client = client()?;
        let mut request = client.get(url).header("accept", "text/event-stream");
        for (name, value) in &headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(auth) = &auth
            && let Some(token) = auth.bearer().await
        {
            request = request.bearer_auth(token);
        }

        let response = request
            .send()
            .await
            .map_err(|error| format!("could not open the event stream at {url}: {error}"))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(sign_in_required(label, response.headers()));
        }
        if !response.status().is_success() {
            return Err(format!(
                "the event stream at {url} answered {}",
                response.status()
            ));
        }

        let pending = Pending::default();
        let (endpoint_tx, endpoint_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(read_stream(
            response,
            Arc::clone(&pending),
            Some(endpoint_tx),
        ));

        let deadline = std::time::Duration::from_millis(open_timeout_millis);
        let endpoint = match tokio::time::timeout(deadline, endpoint_rx).await {
            Ok(Ok(endpoint)) => endpoint,
            Ok(Err(_)) => {
                return Err(format!(
                    "the event stream at {url} closed before it named an endpoint"
                ));
            }
            Err(_) => {
                return Err(format!(
                    "the event stream at {url} named no endpoint within {open_timeout_millis}ms"
                ));
            }
        };

        Ok(Self {
            client,
            post_url: absolute(url, &endpoint),
            headers,
            session: Mutex::new(None),
            pending,
            out_of_band: true,
            next_id: AtomicU64::new(1),
            auth,
            label: label.to_string(),
        })
    }

    /// Send a request and wait for the answer bearing its id.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});

        // Registered *before* the request goes out. The answer to a fast method
        // can be on the stream before `send` has even returned.
        let waiter = self.out_of_band.then(|| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if let Ok(mut pending) = self.pending.lock() {
                pending.insert(id, tx);
            }
            rx
        });

        let response = match self.post(&frame, method).await {
            Ok(response) => response,
            Err(error) => {
                self.forget(id);
                return Err(error);
            }
        };

        if let Some(waiter) = waiter {
            // The POST only acknowledges; the answer comes down the stream.
            return waiter.await.unwrap_or(Err(RpcError::Closed {
                method: method.to_string(),
            }));
        }
        self.read_answer(response, method, id).await
    }

    /// Send a notification, which by definition has no id and no answer.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.post(&frame, method).await.map(|_| ())
    }

    async fn post(&self, frame: &Value, method: &str) -> Result<reqwest::Response, RpcError> {
        let transport = |detail: String| RpcError::Transport {
            method: method.to_string(),
            source: std::io::Error::other(detail),
        };

        let bearer = match &self.auth {
            Some(auth) => auth.bearer().await,
            None => None,
        };
        let response = self
            .send(frame, bearer.as_deref())
            .await
            .map_err(|error| transport(error.to_string()))?;

        // A 401 is the one status worth acting on rather than reporting: the
        // server may simply have decided the token is done before its stated
        // expiry. One refresh, one retry — a loop here would spend the refresh
        // token against a server that will never accept it.
        let response = if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            match &self.auth {
                Some(auth) => match auth.refresh().await {
                    Some(token) => self
                        .send(frame, Some(&token))
                        .await
                        .map_err(|error| transport(error.to_string()))?,
                    None => {
                        return Err(transport(sign_in_required(&self.label, response.headers())));
                    }
                },
                None => return Err(transport(sign_in_required(&self.label, response.headers()))),
            }
        } else {
            response
        };

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(transport(sign_in_required(&self.label, response.headers())));
        }

        // A session id is handed out once, on the response to `initialize`, and
        // is required on everything after it. Reading it off every response is
        // simpler than knowing which request was the handshake, and a server
        // that never issues one leaves this untouched.
        if let Some(id) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            && let Ok(mut session) = self.session.lock()
        {
            *session = Some(id.to_string());
        }

        let status = response.status();
        if !status.is_success() {
            return Err(transport(format!("the server answered {status}")));
        }
        Ok(response)
    }

    /// One attempt at the request, with whatever credential was decided on.
    async fn send(
        &self,
        frame: &Value,
        bearer: Option<&str>,
    ) -> reqwest::Result<reqwest::Response> {
        let mut request = self
            .client
            .post(&self.post_url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(frame);
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(session) = self.session.lock().ok().and_then(|held| held.clone()) {
            request = request.header(SESSION_HEADER, session);
        }
        request.send().await
    }

    /// Read the answer off the response to the request that asked for it.
    ///
    /// The body is JSON, or an SSE stream carrying JSON — the spec lets a
    /// server choose per response, so both are read here rather than the choice
    /// being configured.
    async fn read_answer(
        &self,
        response: reqwest::Response,
        method: &str,
        id: u64,
    ) -> Result<Value, RpcError> {
        let malformed = |detail: String| RpcError::Malformed {
            method: method.to_string(),
            detail,
        };

        let is_event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));

        if !is_event_stream {
            let body = response
                .text()
                .await
                .map_err(|error| malformed(error.to_string()))?;
            let message: Value = serde_json::from_str(&body)
                .map_err(|error| malformed(format!("its body is not JSON: {error}")))?;
            return answer(&message);
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| malformed(error.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            for event in take_events(&mut buffer) {
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                // A server may interleave notifications and requests of its own
                // on this stream. Only the answer we are owed ends the wait.
                if message.get("id").and_then(Value::as_u64) == Some(id) {
                    return answer(&message);
                }
            }
        }
        Err(RpcError::Closed {
            method: method.to_string(),
        })
    }

    fn forget(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }
}

/// What to tell a person a `401` means, in the terms they can act on.
///
/// The header is read only to confirm this is an OAuth challenge rather than
/// something else answering 401; where to sign in is discovered by the login
/// flow itself, which is the only thing that can act on it.
fn sign_in_required(label: &str, headers: &reqwest::header::HeaderMap) -> String {
    let challenged = headers
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("bearer"));
    if challenged {
        format!("`{label}` needs authorization — run `keke mcp login {label}`")
    } else {
        format!("`{label}` refused the request as unauthorized (401)")
    }
}

/// One HTTP client for every remote server in the process.
///
/// Shared so connection pooling and TLS setup happen once. A client that
/// cannot be built is a broken TLS configuration, which is worth naming.
fn client() -> Result<reqwest::Client, String> {
    static SHARED: std::sync::OnceLock<Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            reqwest::Client::builder()
                .build()
                .map_err(|error| format!("could not build an HTTP client: {error}"))
        })
        .clone()
}

/// Route every answer on a long-lived stream to the waiter that asked for it.
async fn read_stream(
    response: reqwest::Response,
    pending: Pending,
    mut endpoint: Option<tokio::sync::oneshot::Sender<String>>,
) {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(Ok(chunk)) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        for event in take_events(&mut buffer) {
            if event.name.as_deref() == Some("endpoint") {
                if let Some(tx) = endpoint.take() {
                    let _ = tx.send(event.data.trim().to_string());
                }
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            let Some(id) = message.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let waiter = pending.lock().ok().and_then(|mut map| map.remove(&id));
            if let Some(waiter) = waiter {
                let _ = waiter.send(answer(&message));
            }
        }
    }

    if let Ok(mut map) = pending.lock() {
        for (_, waiter) in map.drain() {
            let _ = waiter.send(Err(RpcError::Closed {
                method: "a pending request".to_string(),
            }));
        }
    }
}

/// One server-sent event: its `event:` name, if it had one, and its data.
struct Event {
    name: Option<String>,
    data: String,
}

/// Take every complete event out of `buffer`, leaving any partial one behind.
///
/// Written out rather than pulled in as a dependency: the subset SSE is used
/// for here is two field names and a blank-line terminator, and the framing is
/// the part that has to be right whoever writes it.
fn take_events(buffer: &mut String) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        // An event ends at a blank line, which is `\n\n` — or `\r\n\r\n` from a
        // server that writes CRLF. Normalizing first keeps the split single.
        let Some(end) = buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n")) else {
            return events;
        };
        let terminator = if buffer[end..].starts_with("\r\n") {
            4
        } else {
            2
        };
        let block: String = buffer.drain(..end + terminator).collect();

        let mut name = None;
        let mut data: Vec<String> = Vec::new();
        for line in block.lines() {
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            // The spec strips one leading space from a field's value.
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => name = Some(value.to_string()),
                "data" => data.push(value.to_string()),
                _ => {}
            }
        }
        if !data.is_empty() {
            events.push(Event {
                name,
                data: data.join("\n"),
            });
        }
    }
}

/// Resolve the endpoint an SSE stream named against the stream's own URL.
///
/// The spec allows a relative path, which is what most servers send. Joining is
/// done by hand rather than with a URL parser: the only forms that occur are an
/// absolute URL and a path, and taking on a parser to tell them apart would be
/// the larger risk.
fn absolute(stream_url: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    let origin_end = stream_url
        .find("://")
        .and_then(|scheme| {
            stream_url[scheme + 3..]
                .find('/')
                .map(|rest| scheme + 3 + rest)
        })
        .unwrap_or(stream_url.len());
    let origin = &stream_url[..origin_end];
    if endpoint.starts_with('/') {
        format!("{origin}{endpoint}")
    } else {
        format!("{origin}/{endpoint}")
    }
}

/// Expand `${VAR}` references in configured header values.
///
/// Same rule as a stdio server's environment: what a manifest or `.mcp.json`
/// holds is a reference, and the value is read here, at the moment it is used,
/// so no resolved secret is ever stored or logged.
pub(crate) fn expand(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| (name.clone(), crate::server::expand_vars(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_taken_only_once_complete() {
        let mut buffer = String::from("event: endpoint\ndata: /messages?id=1\n\ndata: {\"id\"");
        let events = take_events(&mut buffer);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name.as_deref(), Some("endpoint"));
        assert_eq!(events[0].data, "/messages?id=1");
        // The partial event stays put, to be completed by the next chunk.
        assert_eq!(buffer, "data: {\"id\"");
    }

    #[test]
    fn multi_line_data_is_rejoined_with_newlines() {
        let mut buffer = String::from("data: {\r\ndata: \"a\": 1}\r\n\r\n");
        let events = take_events(&mut buffer);
        assert_eq!(events[0].data, "{\n\"a\": 1}");
    }

    #[test]
    fn a_relative_endpoint_resolves_against_the_stream_origin() {
        assert_eq!(
            absolute("https://example.com/mcp/sse", "/messages?s=1"),
            "https://example.com/messages?s=1"
        );
        assert_eq!(
            absolute("https://example.com/mcp/sse", "https://other.example/m"),
            "https://other.example/m"
        );
    }
}
