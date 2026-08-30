//! The `.mcp.json` a person edits directly, rather than one a plugin ships.
//!
//! Reading it is already covered by [`crate::resolve`] — a directory holding a
//! `.mcp.json` is a plugin package as far as resolution is concerned, which is
//! what lets a server added by hand pass through the same trust gate as one a
//! repository shipped. What is here is the other half: *writing* the file, for
//! `keke mcp add` and `keke mcp remove`.
//!
//! Editing preserves what it did not touch. The file is a person's, and a tool
//! that rewrites it from its own model of the world silently discards the key
//! some future keke, or some other harness, understood and this one did not.

use std::path::Path;

use serde_json::Map;
use serde_json::Value;

use crate::contributions::McpFile;
use crate::contributions::McpServerEntry;
use crate::resolve::PluginError;

/// A `.mcp.json` opened for editing, with everything it holds kept.
#[derive(Debug)]
pub struct McpDocument {
    /// The whole document, so keys outside `mcpServers` survive a write.
    root: Map<String, Value>,
    servers: McpFile,
}

impl McpDocument {
    /// Read the file, or start an empty document if there is none.
    ///
    /// A file that exists and cannot be parsed is an error rather than an
    /// empty start: overwriting a file keke could not read would destroy the
    /// servers a person is asking to add one to.
    pub fn open(path: &Path) -> Result<Self, PluginError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    root: Map::new(),
                    servers: McpFile::default(),
                });
            }
            Err(source) => {
                return Err(PluginError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };

        let root: Value = serde_json::from_str(&text).map_err(|source| PluginError::Json {
            path: path.display().to_string(),
            source,
        })?;
        let servers: McpFile =
            serde_json::from_value(root.clone()).map_err(|source| PluginError::Json {
                path: path.display().to_string(),
                source,
            })?;

        Ok(Self {
            root: match root {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            servers,
        })
    }

    /// The servers this document names, in name order.
    pub fn servers(&self) -> impl Iterator<Item = (&String, &McpServerEntry)> {
        self.servers.mcp_servers.iter()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&McpServerEntry> {
        self.servers.mcp_servers.get(name)
    }

    /// Add or replace one server. Returns whether it replaced an existing one,
    /// because a caller that overwrote something a person configured should be
    /// able to say so.
    pub fn insert(&mut self, name: impl Into<String>, entry: McpServerEntry) -> bool {
        self.servers
            .mcp_servers
            .insert(name.into(), entry)
            .is_some()
    }

    /// Remove one server, reporting whether it was there.
    pub fn remove(&mut self, name: &str) -> bool {
        self.servers.mcp_servers.remove(name).is_some()
    }

    /// Set whether one server is started, reporting whether it was there.
    pub fn set_disabled(&mut self, name: &str, disabled: bool) -> bool {
        let Some(entry) = self.servers.mcp_servers.get_mut(name) else {
            return false;
        };
        entry.disabled = disabled;
        true
    }

    /// Write the document back, creating the directory if it is missing.
    pub fn save(&self, path: &Path) -> Result<(), PluginError> {
        let mut root = self.root.clone();
        root.insert(
            "mcpServers".to_string(),
            serde_json::to_value(&self.servers.mcp_servers).unwrap_or(Value::Null),
        );

        let mut text = serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default();
        text.push('\n');

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| PluginError::Read {
                path: parent.display().to_string(),
                source,
            })?;
        }
        std::fs::write(path, text).map_err(|source| PluginError::Read {
            path: path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contributions::McpTransport;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    #[test]
    fn an_absent_file_opens_as_an_empty_document() {
        let dir = temp();
        let doc = McpDocument::open(&dir.path().join("nested/.mcp.json")).expect("opens");
        assert_eq!(doc.servers().count(), 0);
    }

    #[test]
    fn writing_preserves_keys_it_does_not_understand() {
        let dir = temp();
        let path = dir.path().join(".mcp.json");
        std::fs::write(&path, r#"{"note": "mine", "mcpServers": {}}"#).expect("writes");

        let mut doc = McpDocument::open(&path).expect("opens");
        doc.insert(
            "vercel",
            McpTransport::Http {
                url: "https://mcp.vercel.com".to_string(),
                headers: Vec::new(),
            }
            .into(),
        );
        doc.save(&path).expect("saves");

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("reads")).expect("json");
        assert_eq!(written["note"], "mine");
        assert_eq!(
            written["mcpServers"]["vercel"]["url"],
            "https://mcp.vercel.com"
        );
        assert_eq!(written["mcpServers"]["vercel"]["type"], "http");
    }

    #[test]
    fn a_malformed_file_is_an_error_rather_than_a_fresh_start() {
        let dir = temp();
        let path = dir.path().join(".mcp.json");
        std::fs::write(&path, "{not json").expect("writes");
        assert!(McpDocument::open(&path).is_err());
    }

    #[test]
    fn removing_reports_whether_there_was_anything_to_remove() {
        let dir = temp();
        let path = dir.path().join(".mcp.json");
        let mut doc = McpDocument::open(&path).expect("opens");
        doc.insert(
            "local",
            McpTransport::Stdio {
                command: "echo".to_string(),
                args: vec!["hi".to_string()],
                env: Vec::new(),
            }
            .into(),
        );
        assert!(doc.remove("local"));
        assert!(!doc.remove("local"));
    }
}
