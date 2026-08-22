//! A newline-delimited JSON-RPC 2.0 client over a child process's stdio.
//!
//! This is deliberately not a general MCP client. keke needs a transport,
//! `initialize`, `tools/list` and `tools/call`; a full client would be an order
//! of magnitude more code for capabilities the engine has nowhere to put.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::sync::oneshot;

/// Why a request did not produce a result.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RpcError {
    /// The peer answered with a JSON-RPC `error` member.
    #[error("{message}")]
    Peer { code: i64, message: String },
    /// The child exited, or its stdio closed, before answering.
    #[error("the server closed its connection before answering `{method}`")]
    Closed { method: String },
    /// The request could not be written to the child.
    #[error("could not send `{method}` to the server: {source}")]
    Transport {
        method: String,
        source: std::io::Error,
    },
    /// The peer answered, but not with something the caller can use.
    #[error("the server answered `{method}` with an unusable result: {detail}")]
    Malformed { method: String, detail: String },
}

/// Waiters keyed by the request id they are owed.
///
/// Shared with the reader task rather than reachable from it through the
/// [`Connection`], so the reader never keeps the child alive: dropping the
/// connection kills the child, the reader sees EOF, and it ends on its own.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// A live connection to one MCP server process.
///
/// Requests are correlated by id, so a slow method never delays a fast one and
/// a late answer cannot be handed to whoever asked next.
pub(crate) struct Connection {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    /// Held only so the child is killed when the connection is dropped.
    _child: Child,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").finish_non_exhaustive()
    }
}

impl Connection {
    /// Take over a spawned child's stdio and start reading its answers.
    pub(crate) fn attach(mut child: Child) -> Result<Self, std::io::Error> {
        let missing = |what: &str| {
            std::io::Error::other(format!("the server was spawned without a pipe on {what}"))
        };
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;

        let pending: Pending = Arc::default();
        tokio::spawn(read_loop(stdout, Arc::clone(&pending)));

        Ok(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            _child: child,
        })
    }

    /// Send a request and wait for the answer bearing its id.
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        // Registered before the write, so an answer that arrives while we are
        // still inside `write_line` has somewhere to go.
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, tx);
        }

        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(source) = self.write_line(&frame).await {
            self.forget(id);
            return Err(RpcError::Transport {
                method: method.to_string(),
                source,
            });
        }

        rx.await.unwrap_or(Err(RpcError::Closed {
            method: method.to_string(),
        }))
    }

    /// Send a notification, which by definition has no id and no answer.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.write_line(&frame)
            .await
            .map_err(|source| RpcError::Transport {
                method: method.to_string(),
                source,
            })
    }

    async fn write_line(&self, frame: &Value) -> Result<(), std::io::Error> {
        let mut line = serde_json::to_vec(frame)?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&line).await?;
        stdin.flush().await
    }

    fn forget(&self, id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&id);
        }
    }
}

/// Route every answer to the waiter that asked for it, until the child is gone.
///
/// Anything unrecognized — a notification, a log line a server wrote to stdout
/// by mistake — is skipped rather than treated as an answer, because guessing
/// which waiter it belongs to is exactly the bug this loop exists to prevent.
async fn read_loop(stdout: ChildStdout, pending: Pending) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let waiter = pending.lock().ok().and_then(|mut map| map.remove(&id));
        let Some(waiter) = waiter else {
            continue;
        };
        let _ = waiter.send(answer(&message));
    }

    // EOF: every outstanding request is owed a reply it will never get.
    if let Ok(mut map) = pending.lock() {
        for (_, waiter) in map.drain() {
            let _ = waiter.send(Err(RpcError::Closed {
                method: "a pending request".to_string(),
            }));
        }
    }
}

fn answer(message: &Value) -> Result<Value, RpcError> {
    if let Some(error) = message.get("error") {
        return Err(RpcError::Peer {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the server reported an error with no message")
                .to_string(),
        });
    }
    Ok(message.get("result").cloned().unwrap_or(Value::Null))
}
