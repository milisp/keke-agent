/// One server-sent event: an optional `event:` name plus its `data:` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseFrame {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

impl SseFrame {
    /// A frame with a `data:` payload only, as chat_completions uses.
    pub(crate) fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    /// A frame with an `event:` name, as the Responses and Messages APIs use.
    pub(crate) fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }

    /// The frame's bytes on the wire, terminating blank line included.
    #[must_use]
    pub(crate) fn render(&self) -> String {
        match &self.event {
            Some(name) => format!("event: {name}\ndata: {}\n\n", self.data),
            None => format!("data: {}\n\n", self.data),
        }
    }
}
