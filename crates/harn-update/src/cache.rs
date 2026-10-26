//! `update-cache.json`: the latest version seen per npm package, and when an install last failed.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};

const CACHE_FILE: &str = "update-cache.json";
pub(crate) const MS_PER_MIN: u64 = 60_000;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `{ "<npm package>": { "latest": "1.2.3", "checked_at_ms": 1790000000000 } }`
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Cache(BTreeMap<String, Entry>);

#[derive(Debug, Default, Serialize, Deserialize)]
struct Entry {
    latest: String,
    checked_at_ms: u64,
    /// When the last install attempt did not complete (0: it did, or none was made), so launches do
    /// not keep re-running a slow install that will fail the same way.
    #[serde(default)]
    failed_at_ms: u64,
}

impl Cache {
    /// A missing or corrupt file is an empty cache; the next `save` rewrites it.
    pub(crate) fn load(dir: &Path) -> Self {
        let Ok(text) = fs::read_to_string(dir.join(CACHE_FILE)) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|err| {
            tracing::warn!(%err, "ignoring a corrupt update cache");
            Self::default()
        })
    }

    pub(crate) fn save(&self, dir: &Path) {
        let saved = serde_json::to_string_pretty(self)
            .map_err(io::Error::other)
            .and_then(|json| {
                fs::create_dir_all(dir)?;
                fs::write(dir.join(CACHE_FILE), json)
            });
        if let Err(err) = saved {
            tracing::warn!(dir = %dir.display(), %err, "cannot write the update cache");
        }
    }

    /// The cached latest version, if it was looked up less than `max_age_ms` ago.
    pub(crate) fn fresh(&self, package: &str, now_ms: u64, max_age_ms: u64) -> Option<Version> {
        let entry = self.0.get(package)?;
        // A timestamp from the future (the clock moved back) is not trusted.
        if now_ms.checked_sub(entry.checked_at_ms)? >= max_age_ms {
            return None;
        }
        Version::parse(&entry.latest).ok()
    }

    pub(crate) fn record_latest(&mut self, package: &str, latest: &Version, now_ms: u64) {
        let entry = self.0.entry(package.to_owned()).or_default();
        entry.latest = latest.to_string();
        entry.checked_at_ms = now_ms;
    }

    /// `at_ms` 0 clears the marker.
    pub(crate) fn set_failed(&mut self, package: &str, at_ms: u64) {
        self.0.entry(package.to_owned()).or_default().failed_at_ms = at_ms;
    }

    /// Time left before an install may be tried again, if the last one failed less than
    /// `cooldown_ms` ago.
    pub(crate) fn cooldown_left_ms(
        &self,
        package: &str,
        now_ms: u64,
        cooldown_ms: u64,
    ) -> Option<u64> {
        let failed_at = self.0.get(package)?.failed_at_ms;
        if failed_at == 0 {
            return None;
        }
        let age = now_ms.checked_sub(failed_at)?;
        cooldown_ms.checked_sub(age).filter(|left| *left > 0)
    }
}
