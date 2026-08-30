/// One server-sent event: an optional `event:` name plus its `data:` payload.
///
/// Kept as plain strings rather than a typed body so a test can script bytes
/// that no serializer would produce — a half-written JSON frame, an unknown
/// event name — which is exactly what a parser needs to be tested against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

impl SseFrame {
    /// A frame with a `data:` payload only, as chat_completions uses.
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    /// A frame with an `event:` name, as the Responses and Messages APIs use.
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }

    /// The frame's bytes on the wire, terminating blank line included.
    #[must_use]
    pub fn render(&self) -> String {
        match &self.event {
            Some(name) => format!("event: {name}\ndata: {}\n\n", self.data),
            None => format!("data: {}\n\n", self.data),
        }
    }
}
