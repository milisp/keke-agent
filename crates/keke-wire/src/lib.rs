//! One HTTP client for the three inference wire formats.
//!
//! Chat completions, Responses, and Anthropic Messages differ in how they frame
//! a request and how they frame a reply, but a vendor plugin should not have to
//! care: it picks a [`WireApi`], hands over neutral
//! [`keke_provider_api`] types, and gets neutral [`StreamChunk`]s back. Adding
//! a vendor that speaks one of these three then costs a `ProviderInfo` and a
//! constructor, not another SSE state machine.
//!
//! What the formats *do* share is the part the engine depends on, so it is
//! enforced here rather than per format:
//!
//! * a successful stream ends with exactly one [`StreamChunk::Done`], and one
//!   that ends without it is a [`ProviderError::Protocol`] — never a silently
//!   truncated success;
//! * failures are classified by what the caller should do next, not by which
//!   vendor produced them;
//! * [`AuthProvider::headers`] is called once per request and never cached, so
//!   a token refreshed after a 401 reaches the very next call.
//!
//! Anything a format cannot express is dropped rather than approximated: see
//! the module docs for what each one loses and why forging it would be worse.

mod chat_completions;
mod decode;
mod http;
mod messages;
mod responses;

pub use chat_completions::chat_completions_body;
pub use messages::messages_body;
pub use responses::responses_body;

use std::sync::Arc;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::TryStreamExt;
use keke_auth_api::AuthProvider;
use keke_protocol::ContentBlock;
use keke_protocol::ToolResult;
use keke_provider_api::ModelInfo;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
use serde::Deserialize;
use serde_json::Value;

/// An HTTP client for one endpoint, speaking whichever wire format it is asked
/// for.
///
/// The wire format is a per-call argument rather than a field because a vendor
/// may serve several — NVIDIA NIM offers both OpenAI shapes — and a client per
/// format would mean a connection pool per format for the same host.
pub struct WireClient {
    http: reqwest::Client,
    base_url: String,
    auth: Arc<dyn AuthProvider>,
    /// Whether this endpoint lets a request name its own sampling controls.
    ///
    /// A subscription backend decides the reply budget itself and rejects a
    /// request that states one — `{"detail":"Unsupported parameter:
    /// max_output_tokens"}`, and the same for `temperature` — where the
    /// pay-per-token API of the same shape accepts both. Which kind an address
    /// is cannot be read off the wire format, so the composition root says.
    sampling_is_fixed: bool,
    /// Static headers sent with every request, e.g. a gateway's
    /// caller-identification header. Applied before the credential's own
    /// headers, so a request never loses its authorization to a
    /// misconfigured extra one.
    extra_headers: Vec<(String, String)>,
}

impl WireClient {
    /// `base_url` is the API root, e.g. `https://api.x.ai/v1`; the endpoint path
    /// for the chosen format is appended to it.
    #[must_use]
    pub fn new(base_url: String, auth: Arc<dyn AuthProvider>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            sampling_is_fixed: false,
            extra_headers: Vec::new(),
        }
    }

    /// Mark this endpoint as one that fixes sampling itself — see
    /// [`WireClient::sampling_is_fixed`].
    #[must_use]
    pub fn with_fixed_sampling(mut self) -> Self {
        self.sampling_is_fixed = true;
        self
    }

    /// Attach static headers sent with every request — see
    /// [`WireClient::extra_headers`].
    #[must_use]
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Reuse an existing client, so a host sharing a connection pool and
    /// timeouts across providers does not get one pool per vendor.
    #[must_use]
    pub fn with_http_client(
        base_url: String,
        auth: Arc<dyn AuthProvider>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            sampling_is_fixed: false,
            extra_headers: Vec::new(),
        }
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Translate `request`, send it, and normalize the reply into
    /// [`StreamChunk`]s.
    ///
    /// The error returned here is the one that happened *before* any chunk: a
    /// failure that develops mid-stream arrives as an `Err` item inside the
    /// stream instead, because by then part of the reply already reached the
    /// caller.
    pub async fn stream(
        &self,
        api: WireApi,
        request: ModelRequest,
    ) -> Result<StreamEvent, ProviderError> {
        let (path, body) = match api {
            WireApi::ChatCompletions => {
                ("/chat/completions", chat_completions_body(&request, true))
            }
            WireApi::Responses => (
                "/responses",
                responses_body(&request, true, self.sampling_is_fixed),
            ),
            WireApi::Messages => ("/messages", messages_body(&request, true)),
            WireApi::Custom => return Err(custom_unsupported()),
        };

        let mut builder = self
            .http
            .post(self.url(path))
            .header("accept", "text/event-stream")
            .json(&body);
        if matches!(api, WireApi::Messages) {
            builder = builder.header("anthropic-version", messages::ANTHROPIC_VERSION);
        }

        let response = self
            .authorize(builder)
            .await?
            .send()
            .await
            .map_err(http::transport_error)?;
        let response = http::check_status(response).await?;

        let frames = response
            .bytes_stream()
            .eventsource()
            // A break mid-stream is a transport failure, not a malformed reply:
            // the engine may retry it.
            .map_err(|error| ProviderError::Transient(error.to_string()))
            .map_ok(|event| event.data)
            .boxed();

        Ok(match api {
            WireApi::ChatCompletions => decode::run(frames, chat_completions::Decoder::default()),
            WireApi::Responses => decode::run(frames, responses::Decoder::default()),
            WireApi::Messages => decode::run(frames, messages::Decoder::default()),
            WireApi::Custom => return Err(custom_unsupported()),
        })
    }

    /// Enumerate the endpoint's models.
    ///
    /// A failure is reported rather than flattened to an empty list: the empty
    /// list means "this provider cannot enumerate", and returning it for a
    /// rejected key would present an authentication problem as an account with
    /// no models.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let body = self.fetch("/models").await?;
        let listing: ModelListing = serde_json::from_str(&body)
            .map_err(|error| ProviderError::Protocol(format!("undecodable model list: {error}")))?;
        Ok(listing.data.into_iter().map(ModelInfo::from).collect())
    }

    /// `GET` `path` with this client's credentials, returning the body as text.
    ///
    /// Exposed because a vendor whose listing is richer than the plain one
    /// decodes it in its own crate — the shape is the vendor's, but the
    /// endpoint, the credential, and the retry behaviour are not, and a plugin
    /// that built its own HTTP client to read one JSON document would drop all
    /// three.
    pub async fn fetch(&self, path: &str) -> Result<String, ProviderError> {
        let builder = self.http.get(self.url(path));
        let response = self
            .authorize(builder)
            .await?
            .send()
            .await
            .map_err(http::transport_error)?;
        http::check_status(response)
            .await?
            .text()
            .await
            .map_err(|error| ProviderError::Protocol(error.to_string()))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Attach credentials to `builder`.
    ///
    /// Headers are fetched per request and never held between them, which is the
    /// only reason a token refreshed after a 401 reaches the next call.
    async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let builder = self
            .extra_headers
            .iter()
            .fold(builder, |builder, (name, value)| {
                builder.header(name, value)
            });
        let headers = self
            .auth
            .headers()
            .await
            .map_err(|error| ProviderError::Unauthorized(error.to_string()))?;
        Ok(headers.iter().fold(builder, |builder, (name, value)| {
            builder.header(name, value)
        }))
    }
}

/// `WireApi::Custom` marks a provider that does its own HTTP; routing it here
/// would mean guessing a schema, so it is refused rather than approximated.
fn custom_unsupported() -> ProviderError {
    ProviderError::InvalidRequest(
        "this provider declares a custom wire format and cannot use the shared client".to_string(),
    )
}

/// The `/models` listing, which all three vendors serve in the OpenAI shape.
#[derive(Debug, Deserialize)]
struct ModelListing {
    #[serde(default)]
    data: Vec<WireModel>,
}

#[derive(Debug, Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    /// Present on richer listings; absent on the plain OpenAI one.
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

impl From<WireModel> for ModelInfo {
    fn from(wire: WireModel) -> Self {
        let supports_vision = wire
            .input_modalities
            .as_ref()
            .is_some_and(|modalities| modalities.iter().any(|kind| kind == "image"));
        let mut model = ModelInfo::new(wire.id);
        if let Some(name) = wire.display_name {
            model.display_name = name;
        }
        model.context_window = wire.context_window;
        model.max_output_tokens = wire.max_output_tokens;
        model.supports_vision = supports_vision;
        // The plain listing says nothing about reasoning levels, so this
        // provider offers none. A ladder invented here would be one a person
        // could select and the endpoint would then reject; a vendor that
        // publishes its levels parses them in its own crate.
        model
    }
}

/// Tool call arguments travel as a JSON *string* on the OpenAI wires, so a
/// structured value has to be re-encoded rather than embedded.
fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

/// Anthropic wants the opposite: an object, so a value that arrived as a JSON
/// string is parsed back out. A string that is not JSON is passed through as
/// itself, since dropping it would lose the call's only arguments.
fn arguments_value(arguments: &Value) -> Value {
    match arguments {
        Value::String(raw) => serde_json::from_str(raw).unwrap_or_else(|_| arguments.clone()),
        other => other.clone(),
    }
}

/// What a tool result shows the model. The structured `value` is deliberately
/// left behind: it exists for replay and for surfaces, not for the model.
fn result_text(result: &ToolResult) -> String {
    let mut text = String::new();
    for block in &result.content {
        match block {
            ContentBlock::Text { text: part } | ContentBlock::Thinking { text: part, .. } => {
                text.push_str(part);
            }
            _ => {}
        }
    }
    text
}

/// `ToolCallEnd` promises parseable arguments, so accumulated fragments are
/// checked before it is emitted rather than left for the dispatcher to trip
/// over. Empty arguments are legitimate — a no-argument tool sends nothing.
fn check_arguments(arguments: &str) -> Result<(), serde_json::Error> {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        return Ok(());
    }
    serde_json::from_str::<Value>(arguments).map(|_| ())
}

#[cfg(test)]
mod tests;
