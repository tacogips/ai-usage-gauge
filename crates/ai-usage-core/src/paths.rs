use std::collections::HashMap;
use std::fs;
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
    pub legacy_config_dir: Option<PathBuf>,
    pub legacy_state_dir: Option<PathBuf>,
    pub legacy_cache_dir: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid machine id")]
pub struct PathError;

fn env_var_with_fallback<'a>(
    env: &'a HashMap<String, String>,
    primary: &str,
    legacy: &str,
) -> Option<&'a str> {
    env.get(primary)
        .or_else(|| env.get(legacy))
        .map(String::as_str)
}

impl AppPaths {
    pub fn production() -> Self {
        let env: HashMap<String, String> = std::env::vars().collect();
        let home = env.get("HOME").map(PathBuf::from).unwrap_or_default();
        Self::from_env(&env, &home)
    }

    pub fn from_env(env: &HashMap<String, String>, home_dir: &Path) -> Self {
        let base = env_var_with_fallback(env, "AI_USAGE_GAUGE_HOME", "CCUSAGE_GAUGE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.to_path_buf());
        let root_pair = |primary: &str, legacy: &str, default: &str| {
            let parent = env_var_with_fallback(env, primary, legacy)
                .map(PathBuf::from)
                .unwrap_or_else(|| base.join(default));
            (parent.join("ai-usage-gauge"), parent.join("ccusage-gauge"))
        };
        let (config_dir, legacy_config_dir) = root_pair(
            "AI_USAGE_GAUGE_CONFIG_HOME",
            "CCUSAGE_GAUGE_CONFIG_HOME",
            ".config",
        );
        let (state_dir, legacy_state_dir) = root_pair(
            "AI_USAGE_GAUGE_STATE_HOME",
            "CCUSAGE_GAUGE_STATE_HOME",
            ".local",
        );
        let (cache_dir, legacy_cache_dir) = root_pair(
            "AI_USAGE_GAUGE_CACHE_HOME",
            "CCUSAGE_GAUGE_CACHE_HOME",
            ".cache",
        );
        Self {
            config_file: config_dir.join("config.json"),
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
            legacy_config_dir: Some(legacy_config_dir),
            legacy_state_dir: Some(legacy_state_dir),
            legacy_cache_dir: Some(legacy_cache_dir),
        }
    }

    /// Best-effort one-way copy migration from legacy `ccusage-gauge` directories into
    /// `ai-usage-gauge` when the new destination file does not yet exist. Never overwrites
    /// existing `ai-usage-gauge` files or deletes the legacy originals.
    pub fn migrate_legacy_if_needed(&self) {
        if let Some(old_cfg) = &self.legacy_config_dir {
            copy_if_missing(&old_cfg.join("ccusage-config.json"), &self.config_file);
            copy_if_missing(&old_cfg.join("machines.json"), &self.machines_file);
        }
        if let Some(old_state) = &self.legacy_state_dir {
            copy_if_missing(&old_state.join("state.json"), &self.state_file);
            copy_if_missing(&old_state.join("rate-limits.json"), &self.rate_limits_file);
        }
        if let Some(old_cache) = &self.legacy_cache_dir {
            copy_if_missing(
                &old_cache.join("dashboard-state.sqlite3"),
                &self.dashboard_state_file,
            );
            copy_if_missing(
                &old_cache.join("litellm-pricing.json"),
                &self.litellm_pricing_cache_file,
            );
            if let Ok(entries) = fs::read_dir(old_cache) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with("aggregates") && name_str.ends_with(".sqlite3") {
                        copy_if_missing(&entry.path(), &self.cache_dir.join(&name));
                    }
                }
            }
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

fn copy_if_missing(src: &Path, dst: &Path) {
    if !src.is_file() || dst.exists() {
        return;
    }
    if let Some(parent) = dst.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    if fs::copy(src, dst).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dst, fs::Permissions::from_mode(0o600));
        }
    }
}
