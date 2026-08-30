//! What a provider serves, remembered on disk between runs.
//!
//! A model list is a network call, and it is on the path of every session that
//! wants to draw a picker. Asking the vendor each time makes opening the
//! interface wait on an endpoint that has nothing new to say; never asking
//! means a model shipped last week is invisible until keke is rebuilt. So the
//! answer is kept, with an age, and the age decides.
//!
//! Two rules make the cache safe to lose and safe to keep:
//!
//! * A cache that cannot be read or written is not an error. It is a
//!   convenience file; failing a session over one would trade the thing a
//!   person asked for against the thing that was meant to make it faster.
//! * A stale entry is still an answer. When the vendor cannot be reached, what
//!   it said yesterday is better than an empty picker, so staleness is reported
//!   rather than hidden and the caller decides.
//!
//! The lifetime is not a constant here. How long a deployment is willing to
//! show a day-old list is exactly the kind of number one deployment sets
//! differently from another, so it arrives as a validated
//! [`keke_config_types::ModelCatalogTtl`](../keke_config_types/struct.ModelCatalogTtl.html)
//! and this crate only obeys it.

use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use keke_paths::AbsPath;
use keke_provider_api::ModelInfo;
use serde::Deserialize;
use serde::Serialize;

/// A catalog read back from disk, and whether it is still within its lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cached {
    pub models: Vec<ModelInfo>,
    /// `false` for an entry past its TTL. Still handed over, because a caller
    /// whose fetch just failed would rather show yesterday's list than none.
    pub fresh: bool,
}

/// One route's stored catalog.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Seconds since the epoch, at the moment the vendor answered.
    fetched_at: u64,
    /// Which keke wrote it. An entry from another build is a miss: the shape
    /// of [`ModelInfo`] is this crate's to change, and a field added since
    /// would silently read back as its default.
    version: String,
    models: Vec<ModelInfo>,
}

/// Model catalogs, filed by route under keke's home.
#[derive(Clone, Debug)]
pub struct CatalogCache {
    dir: PathBuf,
    ttl: Duration,
}

impl CatalogCache {
    /// Cache catalogs under `<home>/cache/models`, keeping each for `ttl`.
    #[must_use]
    pub fn new(home: &AbsPath, ttl: Duration) -> Self {
        Self {
            dir: home.as_path().join("cache").join("models"),
            ttl,
        }
    }

    /// Whatever is stored for `route`, fresh or not.
    ///
    /// Returns `None` for a miss, for an unreadable or undecodable file, and
    /// for an entry another build wrote — all of which mean the same thing to
    /// the caller: ask the vendor.
    #[must_use]
    pub fn load(&self, route: &str) -> Option<Cached> {
        let path = self.path(route);
        let raw = std::fs::read_to_string(&path).ok()?;
        let entry: Entry = serde_json::from_str(&raw)
            .map_err(|error| {
                tracing::debug!(%route, %error, "discarding an undecodable model cache");
            })
            .ok()?;
        if entry.version != version() {
            return None;
        }
        Some(Cached {
            // Strictly less, so a zero lifetime — "ask every time" — is never
            // fresh rather than fresh for the instant it was written.
            fresh: age(entry.fetched_at).is_some_and(|age| age < self.ttl),
            models: entry.models,
        })
    }

    /// Replace what is stored for `route`.
    ///
    /// An empty list is not stored: "the vendor could not be asked" and "the
    /// vendor serves nothing" arrive here identically, and caching the first as
    /// the second would keep a picker empty for the whole TTL over one failed
    /// request.
    pub fn store(&self, route: &str, models: &[ModelInfo]) {
        if models.is_empty() {
            return;
        }
        let entry = Entry {
            fetched_at: now(),
            version: version().to_string(),
            models: models.to_vec(),
        };
        if let Err(error) = self.write(route, &entry) {
            tracing::debug!(%route, %error, "could not cache the model list");
        }
    }

    /// Forget `route`, so the next ask goes to the vendor.
    pub fn clear(&self, route: &str) {
        let _ = std::fs::remove_file(self.path(route));
    }

    fn write(&self, route: &str, entry: &Entry) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let encoded = serde_json::to_vec_pretty(entry)?;
        // Written beside and renamed, so a run interrupted mid-write leaves the
        // previous catalog rather than a truncated one that reads as a miss
        // forever.
        let temporary = self.dir.join(format!("{}.tmp", file_stem(route)));
        std::fs::write(&temporary, encoded)?;
        std::fs::rename(&temporary, self.path(route))
    }

    fn path(&self, route: &str) -> PathBuf {
        self.dir.join(format!("{}.json", file_stem(route)))
    }
}

/// A route is a registry key, not a filename: anything that is not plainly safe
/// in a path becomes `_`, so a declared route named `../../etc/shadow` cannot
/// decide where the cache is written.
///
/// A single dot survives, because a route may reasonably carry one. A doubled
/// one does not, because that is the only spelling of "somewhere else".
fn file_stem(route: &str) -> String {
    let stem: String = route
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.replace("..", "__");
    if stem.is_empty() {
        "_".to_string()
    } else {
        stem
    }
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// How old an entry is, or `None` when it claims to be from the future — a
/// clock that moved backwards, which is a reason to refetch rather than to
/// trust an entry that will never expire.
fn age(fetched_at: u64) -> Option<Duration> {
    now().checked_sub(fetched_at).map(Duration::from_secs)
}

/// The shape of a vendor crate's bundled catalog file.
///
/// Every compiled-in vendor ships one, because a model list that can only be
/// fetched is a picker that is empty exactly when the network is: on a plane,
/// behind a proxy, on the first run before a login. The file is a floor, not
/// the answer — a fetch that succeeds replaces it.
///
/// It is deliberately the same shape for every vendor even though their
/// listings are not, so this decoding is written once.
#[derive(Debug, Deserialize)]
pub struct BundledModel {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub supports_vision: bool,
    #[serde(default)]
    pub reasoning_efforts: Vec<keke_provider_api::ReasoningEffort>,
    #[serde(default)]
    pub default_reasoning_effort: Option<keke_provider_api::ReasoningEffort>,
}

impl From<BundledModel> for ModelInfo {
    fn from(bundled: BundledModel) -> Self {
        let mut model = ModelInfo::new(bundled.id);
        if let Some(name) = bundled.display_name {
            model.display_name = name;
        }
        model.description = bundled.description;
        model.context_window = bundled.context_window;
        model.supports_vision = bundled.supports_vision;
        model.reasoning_efforts = bundled.reasoning_efforts;
        model.default_reasoning_effort = bundled.default_reasoning_effort;
        model
    }
}

/// Decode a vendor's bundled catalog.
///
/// Panics on malformed input, and deliberately: the file is compiled into the
/// binary, so a failure here is a build that shipped a broken constant rather
/// than anything a person did. The alternative — an empty list — would hide it
/// as "this vendor has no models".
///
/// Call it from a `LazyLock` so the cost is paid once and the panic, if there
/// is one, lands in the crate's own tests.
#[must_use]
#[allow(clippy::expect_used)]
pub fn bundled(json: &str) -> Vec<ModelInfo> {
    let models: Vec<BundledModel> =
        serde_json::from_str(json).expect("a vendor's bundled model catalog must decode");
    models.into_iter().map(ModelInfo::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(ttl: Duration) -> (tempfile::TempDir, CatalogCache) {
        let home = tempfile::tempdir().expect("temp dir");
        let path = AbsPath::new(home.path()).expect("absolute");
        let cache = CatalogCache::new(&path, ttl);
        (home, cache)
    }

    fn models() -> Vec<ModelInfo> {
        let mut model = ModelInfo::new("grok-4.6");
        model.display_name = "Grok 4.6".to_string();
        model.reasoning_efforts = vec![keke_protocol::ReasoningEffort::High];
        vec![model]
    }

    #[test]
    fn a_stored_catalog_reads_back_whole() {
        let (_home, cache) = cache(Duration::from_secs(3600));
        cache.store("grok", &models());
        let cached = cache.load("grok").expect("stored");
        assert!(cached.fresh);
        assert_eq!(cached.models, models());
    }

    /// The point of the age: a caller must be able to tell "this is current"
    /// from "this is the last thing we heard".
    #[test]
    fn an_entry_past_its_lifetime_is_returned_as_stale() {
        let (_home, cache) = cache(Duration::ZERO);
        cache.store("grok", &models());
        let cached = cache.load("grok").expect("stored");
        assert!(!cached.fresh);
        assert_eq!(cached.models, models());
    }

    /// A failed fetch arrives as an empty list. Storing it would keep the
    /// picker empty for the whole TTL over one bad request.
    #[test]
    fn an_empty_list_is_not_stored_over_a_good_one() {
        let (_home, cache) = cache(Duration::from_secs(3600));
        cache.store("grok", &models());
        cache.store("grok", &[]);
        assert_eq!(cache.load("grok").expect("kept").models, models());
    }

    #[test]
    fn a_route_cannot_choose_where_the_cache_is_written() {
        assert_eq!(file_stem("../../etc/passwd"), "______etc_passwd");
        assert_eq!(file_stem(".."), "__");
        assert_eq!(file_stem(""), "_");
        // A route that is merely dotted is left alone.
        assert_eq!(file_stem("vendor.eu"), "vendor.eu");
        assert_eq!(file_stem("grok"), "grok");
    }

    #[test]
    fn a_missing_or_corrupt_entry_is_a_miss_rather_than_a_failure() {
        let (_home, cache) = cache(Duration::from_secs(3600));
        assert!(cache.load("grok").is_none());
        cache.store("grok", &models());
        std::fs::write(cache.path("grok"), b"{not json").expect("write");
        assert!(cache.load("grok").is_none());
    }

    #[test]
    fn clearing_a_route_sends_the_next_ask_to_the_vendor() {
        let (_home, cache) = cache(Duration::from_secs(3600));
        cache.store("grok", &models());
        cache.clear("grok");
        assert!(cache.load("grok").is_none());
    }
}
