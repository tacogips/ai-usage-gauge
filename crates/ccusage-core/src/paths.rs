use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub machines_file: PathBuf,
    pub state_dir: PathBuf,
    pub state_file: PathBuf,
    pub rate_limits_file: PathBuf,
    pub log_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub legacy_aggregation_cache_file: PathBuf,
    pub dashboard_state_file: PathBuf,
    pub litellm_pricing_cache_file: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid machine id")]
pub struct PathError;

impl AppPaths {
    pub fn production() -> Self {
        let env: HashMap<String, String> = std::env::vars().collect();
        let home = env.get("HOME").map(PathBuf::from).unwrap_or_default();
        Self::from_env(&env, &home)
    }

    pub fn from_env(env: &HashMap<String, String>, home_dir: &Path) -> Self {
        let base = env
            .get("CCUSAGE_GAUGE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.to_path_buf());
        let root = |key: &str, default: &str| {
            env.get(key)
                .map(PathBuf::from)
                .unwrap_or_else(|| base.join(default))
                .join("ccusage-gauge")
        };
        let config_dir = root("CCUSAGE_GAUGE_CONFIG_HOME", ".config");
        let state_dir = root("CCUSAGE_GAUGE_STATE_HOME", ".local");
        let cache_dir = root("CCUSAGE_GAUGE_CACHE_HOME", ".cache");
        Self {
            config_file: config_dir.join("ccusage-config.json"),
            machines_file: config_dir.join("machines.json"),
            state_file: state_dir.join("state.json"),
            rate_limits_file: state_dir.join("rate-limits.json"),
            log_dir: state_dir.join("logs"),
            legacy_aggregation_cache_file: cache_dir.join("aggregates.sqlite3"),
            dashboard_state_file: cache_dir.join("dashboard-state.sqlite3"),
            litellm_pricing_cache_file: cache_dir.join("litellm-pricing.json"),
            config_dir,
            state_dir,
            cache_dir,
        }
    }

    pub fn aggregation_cache_file_for_machine(
        &self,
        machine_id: &str,
    ) -> Result<PathBuf, PathError> {
        if machine_id.is_empty()
            || !machine_id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(PathError);
        }
        Ok(self
            .cache_dir
            .join(format!("aggregates-{machine_id}.sqlite3")))
    }
}
