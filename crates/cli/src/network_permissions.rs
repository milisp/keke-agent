//! Human grants live only in the composition root's user home, never config layers.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use keke_plugin_api::NetworkPermissionPersistence;

pub(crate) struct Store {
    path: PathBuf,
    lock: Mutex<()>,
}

fn valid(domain: &str) -> bool {
    domain.len() <= 253
        && domain.contains('.')
        && domain.parse::<std::net::IpAddr>().is_err()
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

impl Store {
    pub(crate) fn new(home: &std::path::Path) -> Self {
        Self {
            path: home.join("network-permissions.json"),
            lock: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<BTreeSet<String>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let domains: BTreeSet<String> =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                if !domains.iter().all(|d| valid(d)) {
                    return Err("invalid stored network domain".into());
                }
                Ok(domains)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl NetworkPermissionPersistence for Store {
    fn contains(&self, domain: &str) -> bool {
        valid(domain) && self.read().is_ok_and(|domains| domains.contains(domain))
    }

    fn remember(&self, domain: &str) -> Result<(), String> {
        if !valid(domain) {
            return Err("expected an exact lowercase DNS domain".into());
        }
        let _guard = self.lock.lock().map_err(|e| e.to_string())?;
        let mut domains = self.read()?;
        domains.insert(domain.into());
        let parent = self.path.parent().ok_or("permission file has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temp, &domains).map_err(|e| e.to_string())?;
        temp.as_file().sync_all().map_err(|e| e.to_string())?;
        temp.persist(&self.path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_human_grants_survive_restart() {
        let home = tempfile::tempdir().unwrap();
        Store::new(home.path()).remember("example.com").unwrap();
        let store = Store::new(home.path());
        assert!(store.contains("example.com"));
        assert!(!store.contains("sub.example.com"));
        for invalid in [
            "*.example.com",
            "EXAMPLE.COM",
            "127.0.0.1",
            "example.com/",
            "-bad.com",
        ] {
            assert!(store.remember(invalid).is_err());
        }
    }
    #[test]
    fn corrupt_store_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("network-permissions.json"), b"invalid").unwrap();
        let store = Store::new(home.path());
        assert!(!store.contains("example.com"));
        assert!(store.remember("example.com").is_err());
    }
}
