//! RFC 8628 device authorization grant.
//!
//! The fallback whenever a browser on this machine cannot reach a loopback
//! port: an SSH session, a container, an editor on the other side of a remote.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use keke_auth_api::AuthError;
use keke_auth_api::LoginUi;
use reqwest::Client;
use serde::Deserialize;

use crate::token::TokenOutcome;
use crate::token::TokenResponse;
use crate::token::post_token;

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

const DEFAULT_INTERVAL_SECS: u64 = 5;
/// RFC 8628 leaves `expires_in` optional; a code that outlives its own deadline
/// is better than one that expires before the person finishes typing it.
const FALLBACK_LIFETIME_SECS: u64 = 600;

/// Waiting between polls, as a seam.
///
/// The interval schedule *is* the behavior being specified — honour `interval`,
/// widen on `slow_down` — so a test has to observe it rather than live through
/// it. Implementers return once `duration` has passed, or at once if they only
/// record it.
pub trait Delay: Send + Sync {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

pub struct TokioDelay;

impl Delay for TokioDelay {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Where and as whom to run the grant — everything the issuer decides.
pub struct DeviceFlow<'a> {
    pub device_authorization_endpoint: &'a str,
    pub token_endpoint: &'a str,
    pub client_id: &'a str,
    pub scope: &'a str,
    /// How much a `slow_down` widens the interval. RFC 8628 says five seconds;
    /// it is the caller's so a deployment can say otherwise.
    pub slow_down_increment: Duration,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
}

/// Show a user code, then poll until the person approves it, denies it, or it
/// expires.
pub async fn run(
    http: &Client,
    flow: &DeviceFlow<'_>,
    ui: &dyn LoginUi,
    delay: &dyn Delay,
) -> Result<TokenResponse, AuthError> {
    let grant = authorize(http, flow).await?;

    ui.show_device_code(&grant.user_code, &grant.verification_uri);
    // The completed URI carries the code already, so the browser is worth
    // offering even though the code has been shown for a manual entry.
    if let Some(complete) = grant.verification_uri_complete.as_deref() {
        ui.open_browser(complete);
    }
    ui.notice("waiting for authorization");

    poll(http, flow, ui, delay, grant).await
}

async fn authorize(http: &Client, flow: &DeviceFlow<'_>) -> Result<DeviceAuthorization, AuthError> {
    let response = http
        .post(flow.device_authorization_endpoint)
        .form(&[("client_id", flow.client_id), ("scope", flow.scope)])
        .send()
        .await
        .map_err(|err| AuthError::Other(format!("device authorization unreachable: {err}")))?;

    if !response.status().is_success() {
        return Err(AuthError::Other(format!(
            "device authorization refused with HTTP {}",
            response.status().as_u16()
        )));
    }

    let grant: DeviceAuthorization = response
        .json()
        .await
        .map_err(|_| AuthError::Other("device authorization response was unreadable".into()))?;

    // The user code is rendered to a terminal; an issuer that could put control
    // characters in it could rewrite what the person thinks they are approving.
    if grant.user_code.is_empty()
        || !grant
            .user_code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AuthError::Other(
            "device authorization returned a malformed user code".into(),
        ));
    }

    Ok(grant)
}

async fn poll(
    http: &Client,
    flow: &DeviceFlow<'_>,
    ui: &dyn LoginUi,
    delay: &dyn Delay,
    grant: DeviceAuthorization,
) -> Result<TokenResponse, AuthError> {
    let mut interval = Duration::from_secs(grant.interval.unwrap_or(DEFAULT_INTERVAL_SECS).max(1));
    let lifetime =
        Duration::from_secs(grant.expires_in.unwrap_or(FALLBACK_LIFETIME_SECS)).max(interval);
    let mut waited = Duration::ZERO;

    loop {
        // Sleep before the first poll: a code that has existed for no time at
        // all can only be pending, and asking anyway invites `slow_down`.
        delay.sleep(interval).await;
        waited += interval;
        if waited > lifetime {
            return Err(AuthError::Other(
                "the device code expired before it was approved".into(),
            ));
        }

        let outcome = post_token(
            http,
            flow.token_endpoint,
            &[
                ("grant_type", GRANT_TYPE),
                ("device_code", grant.device_code.as_str()),
                ("client_id", flow.client_id),
            ],
        )
        .await?;

        let refusal = match outcome {
            TokenOutcome::Granted(tokens) => return Ok(tokens),
            TokenOutcome::Refused(refusal) => refusal,
        };

        match refusal.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => {
                interval += flow.slow_down_increment;
                ui.notice("the issuer asked us to poll less often");
            }
            "access_denied" => return Err(AuthError::Cancelled),
            "expired_token" => {
                return Err(AuthError::Other(
                    "the device code expired before it was approved".into(),
                ));
            }
            _ => return Err(AuthError::Rejected(refusal.detail().to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::Request;
    use wiremock::Respond;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::*;
    use crate::testing::RecordingDelay;
    use crate::testing::RecordingUi;

    /// Answers from a script, one entry per call, so a poll sequence is
    /// expressed as a sequence rather than as mock precedence rules.
    struct Script {
        responses: Vec<ResponseTemplate>,
        calls: AtomicUsize,
    }

    impl Respond for Script {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses[index.min(self.responses.len() - 1)].clone()
        }
    }

    fn refusal(error: &str) -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(serde_json::json!({ "error": error }))
    }

    async fn issuer(authorization: serde_json::Value, token: impl Respond + 'static) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(authorization))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(token)
            .mount(&server)
            .await;
        server
    }

    async fn login(
        server: &MockServer,
        ui: &RecordingUi,
        delay: &RecordingDelay,
    ) -> Result<TokenResponse, AuthError> {
        let device_authorization_endpoint = format!("{}/device/code", server.uri());
        let token_endpoint = format!("{}/token", server.uri());
        let flow = DeviceFlow {
            device_authorization_endpoint: &device_authorization_endpoint,
            token_endpoint: &token_endpoint,
            client_id: "client-1",
            scope: "openid",
            slow_down_increment: Duration::from_secs(5),
        };
        run(&Client::new(), &flow, ui, delay).await
    }

    #[tokio::test]
    async fn the_code_is_shown_and_the_completed_uri_offered() {
        let server = issuer(
            serde_json::json!({
                "device_code": "dev-code-1",
                "user_code": "ABCD-1234",
                "verification_uri": "https://issuer.test/device",
                "verification_uri_complete": "https://issuer.test/device?user_code=ABCD-1234",
                "interval": 1,
            }),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
            })),
        )
        .await;
        let ui = RecordingUi::new();

        let tokens = login(&server, &ui, &RecordingDelay::new()).await.unwrap();

        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(
            ui.device_codes(),
            vec![(
                "ABCD-1234".to_string(),
                "https://issuer.test/device".to_string()
            )]
        );
        assert_eq!(
            ui.browser_urls(),
            vec!["https://issuer.test/device?user_code=ABCD-1234".to_string()]
        );
    }

    #[tokio::test]
    async fn authorization_pending_is_retried_and_slow_down_widens_the_interval() {
        let server = issuer(
            serde_json::json!({
                "device_code": "dev-code-1",
                "user_code": "ABCD-1234",
                "verification_uri": "https://issuer.test/device",
                "expires_in": 600,
                "interval": 2,
            }),
            Script {
                responses: vec![
                    refusal("authorization_pending"),
                    refusal("slow_down"),
                    refusal("authorization_pending"),
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({ "access_token": "access-1" })),
                ],
                calls: AtomicUsize::new(0),
            },
        )
        .await;
        let ui = RecordingUi::new();
        let delay = RecordingDelay::new();

        login(&server, &ui, &delay).await.unwrap();

        assert_eq!(
            delay.waits(),
            vec![
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(7),
                Duration::from_secs(7),
            ],
            "the issuer's interval is honoured and slow_down widens it once"
        );
        assert!(
            ui.notices()
                .iter()
                .any(|notice| notice.contains("poll less often"))
        );
    }

    #[tokio::test]
    async fn a_denied_device_authorization_reads_as_cancellation() {
        let server = issuer(
            serde_json::json!({
                "device_code": "dev-code-1",
                "user_code": "ABCD-1234",
                "verification_uri": "https://issuer.test/device",
                "interval": 1,
            }),
            refusal("access_denied"),
        )
        .await;

        let err = login(&server, &RecordingUi::new(), &RecordingDelay::new())
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Cancelled), "got {err:?}");
    }

    #[tokio::test]
    async fn a_user_code_with_control_characters_is_refused() {
        let server = issuer(
            serde_json::json!({
                "device_code": "dev-code-1",
                "user_code": "AB\u{1b}[2JCD",
                "verification_uri": "https://issuer.test/device",
            }),
            refusal("authorization_pending"),
        )
        .await;
        let ui = RecordingUi::new();

        assert!(login(&server, &ui, &RecordingDelay::new()).await.is_err());
        assert!(ui.device_codes().is_empty());
    }
}
