//! The names tools travel under on the wire.
//!
//! A [`ToolId`](keke_protocol::ToolId) is keke's own name for a tool and may
//! carry any character — MCP tools are `plugin:server:tool`. Vendors are
//! stricter: OpenAI rejects a function name outside `^[a-zA-Z0-9_-]+$` and
//! Anthropic additionally caps it at 64 characters, failing the whole request
//! rather than the one tool. So names are rewritten here, at the boundary, and
//! rewritten back when the model calls one — the engine, its log, and its
//! approvals only ever see the id.
//!
//! The mapping is a pure function of the request, so the body builder and the
//! reply decoder each derive it and cannot disagree. A name that is already
//! valid is sent as itself, which keeps the built-in tools readable to the
//! model and keeps prompts byte-stable for every request that has no MCP tool.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;

use keke_protocol::ContentBlock;
use keke_provider_api::ModelRequest;

/// The longest name every supported vendor accepts.
const MAX_LEN: usize = 64;

/// Hex digits of the disambiguating suffix. 32 bits: tool sets are small, and
/// a residual collision is still resolved by the counter below.
const HASH_LEN: usize = 8;

/// A bijection between the tool names a request uses and names a vendor
/// accepts.
#[derive(Debug, Default)]
pub(crate) struct ToolNames {
    to_wire: HashMap<String, String>,
    from_wire: HashMap<String, String>,
}

impl ToolNames {
    /// Every tool name `request` mentions: the ones it offers, and the ones in
    /// its history, which may name a tool that is no longer offered — a
    /// disconnected MCP server's call is still part of the conversation.
    pub(crate) fn for_request(request: &ModelRequest) -> Self {
        let offered = request.tools.iter().map(|tool| tool.name.as_str());
        let called = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call.name.as_str()),
                _ => None,
            });
        Self::new(offered.chain(called))
    }

    fn new<'a>(names: impl Iterator<Item = &'a str>) -> Self {
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            if seen.insert(name) {
                unique.push(name);
            }
        }

        // Valid names are claimed first so that no rewritten name can take
        // one away from the tool that owns it, whatever the order.
        let mut taken: HashSet<String> = unique
            .iter()
            .filter(|name| is_valid(name))
            .map(|name| (*name).to_string())
            .collect();
        let mut names = Self::default();
        for name in unique {
            let wire = if is_valid(name) {
                name.to_string()
            } else {
                let wire = claim(name, &mut taken);
                names.from_wire.insert(wire.clone(), name.to_string());
                wire
            };
            names.to_wire.insert(name.to_string(), wire);
        }
        names
    }

    /// The name to send for `name`.
    pub(crate) fn to_wire<'a>(&'a self, name: &'a str) -> &'a str {
        self.to_wire.get(name).map_or(name, String::as_str)
    }

    /// The tool the model meant by `wire`. A name this request never sent is
    /// returned unchanged, so the engine reports an unknown tool by the name
    /// the model actually used.
    pub(crate) fn to_id(&self, wire: String) -> String {
        self.from_wire.get(&wire).cloned().unwrap_or(wire)
    }

    /// `request` with every tool name replaced by its wire name, borrowed when
    /// nothing needed replacing.
    pub(crate) fn rename<'a>(&self, request: &'a ModelRequest) -> Cow<'a, ModelRequest> {
        if self.from_wire.is_empty() {
            return Cow::Borrowed(request);
        }
        let mut request = request.clone();
        for tool in &mut request.tools {
            tool.name = self.to_wire(&tool.name).to_string();
        }
        for block in request
            .messages
            .iter_mut()
            .flat_map(|message| &mut message.content)
        {
            if let ContentBlock::ToolCall(call) = block {
                call.name = self.to_wire(&call.name).to_string();
            }
        }
        Cow::Owned(request)
    }
}

/// `request` as the body builders should see it.
pub(crate) fn wire_request(request: &ModelRequest) -> Cow<'_, ModelRequest> {
    ToolNames::for_request(request).rename(request)
}

fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// A valid name for `name` not already in `taken`, which it is added to.
fn claim(name: &str, taken: &mut HashSet<String>) -> String {
    let readable = sanitize(name);
    if readable.len() <= MAX_LEN && !taken.contains(&readable) {
        taken.insert(readable.clone());
        return readable;
    }
    // The suffix hashes the original id rather than the readable form, since
    // the readable forms are exactly what collided.
    let hash = format!("{:0width$x}", fnv1a(name), width = HASH_LEN);
    let hash = &hash[..HASH_LEN];
    let mut attempt = 0u32;
    loop {
        let suffix = if attempt == 0 {
            format!("_{hash}")
        } else {
            format!("_{hash}{attempt}")
        };
        let stem = &readable[..MAX_LEN.saturating_sub(suffix.len()).min(readable.len())];
        let candidate = format!("{stem}{suffix}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        attempt += 1;
    }
}

/// `:` becomes `__` so the namespace stays legible to the model; anything else
/// outside the allowed set becomes `_`. The result is pure ASCII, so it can be
/// truncated at any byte.
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for ch in name.chars() {
        match ch {
            ':' => out.push_str("__"),
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => out.push(ch),
            _ => out.push('_'),
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Stable across builds and platforms, unlike `DefaultHasher`, so a session
/// resumed by a newer keke sends the same names and keeps its prompt cache.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ids: &[&str]) -> ToolNames {
        ToolNames::new(ids.iter().copied())
    }

    #[test]
    fn a_name_the_vendor_accepts_is_sent_as_itself() {
        let names = names(&["bash", "list_dir", "web-fetch"]);
        assert_eq!(names.to_wire("bash"), "bash");
        assert_eq!(names.to_wire("web-fetch"), "web-fetch");
    }

    #[test]
    fn an_mcp_id_becomes_a_valid_name_and_maps_back() {
        let names = names(&["acp:codexia-bots:list_bots"]);
        let wire = names.to_wire("acp:codexia-bots:list_bots");
        assert_eq!(wire, "acp__codexia-bots__list_bots");
        assert_eq!(names.to_id(wire.to_string()), "acp:codexia-bots:list_bots");
    }

    #[test]
    fn a_rewritten_name_never_takes_a_name_a_tool_already_owns() {
        // Listed in both orders: the tool whose own name is valid keeps it.
        for ids in [["a__b", "a:b"], ["a:b", "a__b"]] {
            let names = names(&ids);
            assert_eq!(names.to_wire("a__b"), "a__b");
            let wire = names.to_wire("a:b");
            assert_ne!(wire, "a__b");
            assert!(is_valid(wire), "{wire}");
            assert_eq!(names.to_id(wire.to_string()), "a:b");
        }
    }

    #[test]
    fn two_ids_that_sanitize_alike_stay_distinct() {
        let names = names(&["p:s:t.x", "p:s:t/x"]);
        let first = names.to_wire("p:s:t.x");
        let second = names.to_wire("p:s:t/x");
        assert_ne!(first, second);
        assert_eq!(names.to_id(first.to_string()), "p:s:t.x");
        assert_eq!(names.to_id(second.to_string()), "p:s:t/x");
    }

    #[test]
    fn a_long_id_is_cut_to_the_limit_and_stays_unique() {
        let long_a = format!("plugin:server:{}a", "x".repeat(80));
        let long_b = format!("plugin:server:{}b", "x".repeat(80));
        let names = names(&[&long_a, &long_b]);
        let (a, b) = (names.to_wire(&long_a), names.to_wire(&long_b));
        assert!(a.len() <= MAX_LEN && b.len() <= MAX_LEN);
        assert!(is_valid(a) && is_valid(b));
        assert_ne!(a, b);
        assert_eq!(names.to_id(a.to_string()), long_a);
    }

    #[test]
    fn a_name_the_request_never_sent_comes_back_unchanged() {
        let names = names(&["acp:s:t"]);
        assert_eq!(names.to_id("made_up".to_string()), "made_up");
    }

    #[test]
    fn the_mapping_is_stable_across_calls() {
        let ids = ["p:s:t.x", "p:s:t/x", "p:s:t_x"];
        let first = names(&ids);
        let second = names(&ids);
        for id in ids {
            assert_eq!(first.to_wire(id), second.to_wire(id));
        }
    }
}
