use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use harn_core::{Policy, Profile};
use harn_update::UpdateConfig;
use serde::Deserialize;

const APP_DIR: &str = "harn";
const DEFAULT_ORCA_BIN: &str = "orca";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub policy: Policy,
    pub update: UpdateConfig,
    pub orca: OrcaConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrcaConfig {
    pub bin: PathBuf,
    pub interval_ms: u64,
}

impl Default for OrcaConfig {
    fn default() -> Self {
        Self {
            bin: DEFAULT_ORCA_BIN.into(),
            interval_ms: harn_orca::DEFAULT_INTERVAL_MS,
        }
    }
}

pub struct Paths {
    pub config_file: PathBuf,
    pub profiles_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub log_file: PathBuf,
}

impl Paths {
    pub fn resolve(config_override: Option<PathBuf>) -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("cannot determine the user config directory")?
            .join(APP_DIR);
        let cache_dir = dirs::cache_dir()
            .context("cannot determine the user cache directory")?
            .join(APP_DIR);
        let data_dir = dirs::data_local_dir()
            .context("cannot determine the local data directory")?
            .join(APP_DIR);
        Ok(Self {
            config_file: config_override.unwrap_or_else(|| config_dir.join("config.toml")),
            profiles_dir: config_dir.join("profiles"),
            cache_dir,
            log_file: data_dir.join("harn.log"),
        })
    }
}

/// A missing config file means defaults; a broken one is an error, not a silent fallback.
pub fn load(path: &Path) -> Result<Config> {
    let cfg = match fs::read_to_string(path) {
        Ok(src) => parse(&src).with_context(|| format!("invalid config {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => {
            return Err(e).with_context(|| format!("cannot read config {}", path.display()));
        }
    };
    Ok(cfg)
}

fn parse(src: &str) -> Result<Config> {
    let cfg: Config = toml::from_str(src)?;
    cfg.policy.validate()?;
    Ok(cfg)
}

pub struct LoadedProfile {
    pub profile: Profile,
    pub source: Option<PathBuf>,
}

/// Built-ins, with user files in `dir` replacing a built-in of the same id or adding new agents.
pub fn load_profiles(dir: &Path) -> Result<Vec<LoadedProfile>> {
    let mut profiles: Vec<LoadedProfile> = Profile::builtins()
        .into_iter()
        .map(|profile| LoadedProfile {
            profile,
            source: None,
        })
        .collect();
    for path in user_profile_files(dir)? {
        let src = fs::read_to_string(&path)
            .with_context(|| format!("cannot read profile {}", path.display()))?;
        let profile = Profile::from_toml(&src)
            .with_context(|| format!("invalid profile {}", path.display()))?;
        let loaded = LoadedProfile {
            profile,
            source: Some(path),
        };
        match profiles
            .iter_mut()
            .find(|p| p.profile.id == loaded.profile.id)
        {
            Some(slot) => *slot = loaded,
            None => profiles.push(loaded),
        }
    }
    Ok(profiles)
}

fn user_profile_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot list {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot list {}", dir.display()))?
            .path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_is_default() {
        let cfg = load(Path::new("definitely/not/here/config.toml")).unwrap();
        assert_eq!(cfg.orca.bin, PathBuf::from(DEFAULT_ORCA_BIN));
        assert!(cfg.update.enabled);
    }

    #[test]
    fn partial_config_parses() {
        let cfg: Config = toml::from_str(
            "[update]\nenabled = false\n[orca]\ninterval_ms = 500\n[policy]\nsettle_ms = 1000\n",
        )
        .unwrap();
        assert!(!cfg.update.enabled);
        assert_eq!(cfg.orca.interval_ms, 500);
        assert_eq!(cfg.policy.settle_ms, 1000);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(parse("[orca]\nintervall_ms = 5\n").is_err());
    }

    #[test]
    fn unsafe_policy_values_are_rejected() {
        let err = parse("[policy]\nsettle_ms = 0\n").unwrap_err();
        assert!(format!("{err:#}").contains("settle_ms"), "{err:#}");
    }
}
