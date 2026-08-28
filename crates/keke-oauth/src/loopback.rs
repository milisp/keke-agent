//! Authorization code delivery over a loopback redirect (RFC 8252 §7.3).
//!
//! The port is bound *before* the authorize URL is built, because the port
//! number is part of the `redirect_uri` the issuer will check; binding
//! afterwards would leave a window where the URL names a port we do not own.
//! That ordering is why this is a value a caller holds rather than one function
//! that does the whole flow: between binding and awaiting the code, the caller
//! has an issuer to talk to and a browser to open.

use std::time::Duration;

use keke_auth_api::AuthError;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use url::Url;

/// A request line plus headers; anything larger is not a browser redirect.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// How long one connection has to send its request line. A browser that opens a
/// socket and says nothing must not be able to hold the login open.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title>\
<p>Signed in. You can close this tab and return to the terminal.";

/// A bound loopback port waiting for one authorization redirect.
#[derive(Debug)]
pub struct Loopback {
    listener: TcpListener,
    path: String,
}

impl Loopback {
    /// Claim a loopback port, or report why this machine cannot host a redirect.
    ///
    /// `port` is `0` for any free port, which is what an issuer supporting
    /// dynamic registration allows. A caller passes a fixed port only when its
    /// client is registered at exactly one `redirect_uri` — and then a port
    /// already in use means another login is in flight, which is the caller's
    /// to handle, not something to paper over by binding somewhere else.
    pub async fn bind(port: u16, path: impl Into<String>) -> std::io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(("127.0.0.1", port)).await?,
            path: path.into(),
        })
    }

    /// The port actually held, which may not be the one asked for.
    pub fn port(&self) -> Result<u16, AuthError> {
        Ok(self
            .listener
            .local_addr()
            .map_err(|err| AuthError::Other(format!("loopback address unavailable: {err}")))?
            .port())
    }

    /// The `redirect_uri` to send the issuer, naming the port actually held.
    ///
    /// `127.0.0.1` rather than `localhost`, which is what RFC 8252 §8.3 asks
    /// for. A caller whose client is registered at the literal `localhost`
    /// builds its own URI from [`Self::port`] instead — the two are not
    /// interchangeable to an issuer that compares strings.
    pub fn redirect_uri(&self) -> Result<String, AuthError> {
        Ok(format!("http://127.0.0.1:{}{}", self.port()?, self.path))
    }

    /// Serve exactly one callback and return its `code`.
    ///
    /// Consumes the listener: the port exists for this one redirect, and a
    /// caller holding it afterwards could only misuse it.
    pub async fn await_code(self, state: &str, timeout: Duration) -> Result<String, AuthError> {
        tokio::time::timeout(timeout, self.serve(state))
            .await
            .map_err(|_| AuthError::Cancelled)?
    }

    async fn serve(self, state: &str) -> Result<String, AuthError> {
        loop {
            let (mut socket, _) = self
                .listener
                .accept()
                .await
                .map_err(|err| AuthError::Other(format!("loopback accept failed: {err}")))?;

            let Some(target) = read_request_target(&mut socket).await else {
                continue;
            };
            // Browsers ask for /favicon.ico on the same connection budget; only
            // the callback ends the wait.
            let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
                continue;
            };
            if url.path() != self.path {
                let _ = respond(&mut socket, "404 Not Found", "Not found.").await;
                continue;
            }

            let outcome = classify(&url, state);
            let _ = match &outcome {
                Ok(_) => respond(&mut socket, "200 OK", DONE_PAGE).await,
                Err(err) => respond(&mut socket, "400 Bad Request", &err.to_string()).await,
            };
            return outcome;
        }
    }
}

fn classify(url: &Url, state: &str) -> Result<String, AuthError> {
    let mut code = None;
    let mut returned_state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => returned_state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        let detail = description.unwrap_or_else(|| error.clone());
        return Err(match error.as_str() {
            "access_denied" => AuthError::Cancelled,
            _ => AuthError::Rejected(detail),
        });
    }
    // Anything on 127.0.0.1 can reach this port; without the state check a
    // local process could feed us its own authorization code.
    if returned_state.as_deref() != Some(state) {
        return Err(AuthError::Rejected(
            "the redirect did not carry the state this login issued".into(),
        ));
    }
    code.ok_or_else(|| AuthError::Rejected("the redirect carried no authorization code".into()))
}

/// Read the request target from the first line, bounded so a client that never
/// sends a blank line cannot hold the login open.
async fn read_request_target(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(REQUEST_TIMEOUT, socket.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let mut parts = text.lines().next()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    (method == "GET").then(|| target.to_string())
}

async fn respond(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback(query: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1/callback?{query}")).expect("a url")
    }

    #[test]
    fn a_redirect_with_a_foreign_state_is_rejected() {
        let err = classify(&callback("code=abc&state=someone-else"), "ours")
            .expect_err("a foreign state is refused");
        assert!(matches!(err, AuthError::Rejected(_)), "got {err:?}");
    }

    #[test]
    fn a_denied_redirect_reads_as_cancellation() {
        let err = classify(&callback("error=access_denied&state=ours"), "ours")
            .expect_err("a denial is not a code");
        assert!(matches!(err, AuthError::Cancelled), "got {err:?}");
    }

    #[test]
    fn a_matching_redirect_yields_the_code() {
        assert_eq!(
            classify(&callback("code=abc&state=ours"), "ours").expect("the code"),
            "abc"
        );
    }

    #[tokio::test]
    async fn the_redirect_uri_names_the_port_actually_held() {
        let loopback = Loopback::bind(0, "/callback").await.expect("binds");
        let uri = loopback.redirect_uri().expect("has an address");
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with("/callback"), "{uri}");
        assert!(!uri.contains(":0/"), "port 0 means any port, not that port");
    }

    #[tokio::test]
    async fn a_request_to_another_path_does_not_end_the_wait() {
        let loopback = Loopback::bind(0, "/callback").await.expect("binds");
        let uri = loopback.redirect_uri().expect("has an address");
        let base = uri.trim_end_matches("/callback").to_string();

        let served = tokio::spawn(async move {
            loopback
                .await_code("ours", Duration::from_secs(5))
                .await
                .expect("the code")
        });

        // What a browser does before it follows the redirect.
        let _ = reqwest::get(format!("{base}/favicon.ico")).await;
        let _ = reqwest::get(format!("{base}/callback?code=abc&state=ours")).await;

        assert_eq!(served.await.expect("the task"), "abc");
    }
}
