use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;

use ccusage_core::aggregation::collector::{
    FetchFuture, MachineCollector, MachineSnapshotFetcher, MachineSnapshotStore,
};
use ccusage_core::aggregation::snapshot_service::SnapshotService;
use ccusage_core::cache::DashboardDirectoryNameStore;
use ccusage_core::config::{BootstrapLogger, ConfigStore};
use ccusage_core::dashboard::{
    resolve_desktop_executable, DashboardHTTPServer, MachineDashboardRouter, StaticAssetResolver,
};
use ccusage_core::paths::AppPaths;
use ccusage_core::pricing::PricingEngine;
use ccusage_core::rate_limits::service::RateLimitUsageService;
use ccusage_core::remote::registry::MachineRegistryStore;
use ccusage_core::state::StateStore;
use ccusage_core::types::{
    AppConfiguration, AppState, CostSnapshot, DirectoryNameLifecycle, MachineDescriptor,
    MachineKind, MachineRegistry, MachineRuntimeReconciler, ResetCycle, SanitizedCollectionError,
};

pub fn local_timezone() -> Tz {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

pub fn default_cycle_from_term(term: &str) -> ResetCycle {
    match term {
        "hourly" => ResetCycle::Hourly,
        "weekly" => ResetCycle::Weekly,
        "monthly" => ResetCycle::Monthly,
        _ => ResetCycle::Daily,
    }
}

pub fn activate_bootstrap_logger(paths: &AppPaths) -> BootstrapLogger {
    let logger = BootstrapLogger::new(
        paths.log_dir.clone(),
        None,
        "ccusage-cli",
        Utc::now,
        5 * 1024 * 1024,
        Duration::from_secs(7 * 86_400),
    );
    logger.activate();
    logger
}

fn local_descriptor_from_registry(reg: &MachineRegistry) -> MachineDescriptor {
    MachineDescriptor {
        id: "local".into(),
        display_name: "Local".into(),
        kind: MachineKind::Local,
        enabled: true,
        ssh: None,
        codex_session_dirs: reg.local_session_sources.codex_session_dirs.clone(),
        claude_config_dirs: reg.local_session_sources.claude_config_dirs.clone(),
        include_default_codex_dir: reg.local_session_sources.include_default_codex_dir,
        include_default_claude_dir: reg.local_session_sources.include_default_claude_dir,
    }
}

fn antigravity_root_from_env(env: &HashMap<String, String>) -> Option<PathBuf> {
    if let Some(p) = env.get("CCUSAGE_GAUGE_ANTIGRAVITY_DIR") {
        let trimmed = p.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(PathBuf::from(trimmed));
    }
    env.get("HOME")
        .map(|h| PathBuf::from(h).join(".gemini/antigravity-cli"))
}

pub fn run_config_check(paths: &AppPaths) -> Result<(), i32> {
    let _logger = activate_bootstrap_logger(paths);
    let config_store = ConfigStore::new(paths.config_file.clone());
    let config = match config_store.load_or_create() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return Err(1);
        }
    };
    let state_store = StateStore::new(paths.state_file.clone());
    let default_cycle = default_cycle_from_term(&config.default_reset_term);
    if let Err(e) = state_store.load(default_cycle) {
        eprintln!("error: {e}");
        return Err(1);
    }
    println!("status: ok");
    println!("dashboardPort: {}", config.dashboard_port);
    println!("dashboardAutostart: {}", config.dashboard_autostart);
    println!("defaultResetTerm: {}", config.default_reset_term);
    println!("pollIntervalSeconds: {}", config.poll_interval_seconds);
    Ok(())
}

pub fn run_usage_snapshot(paths: &AppPaths, json: bool) -> Result<(), i32> {
    let _logger = activate_bootstrap_logger(paths);
    let config_store = ConfigStore::new(paths.config_file.clone());
    let config = match config_store.load_or_create() {
        Ok(c) => c,
        Err(e) => {
            emit_snapshot_error(&e.to_string(), json);
            return Err(1);
        }
    };
    let state_store = StateStore::new(paths.state_file.clone());
    let state = match state_store.load(default_cycle_from_term(&config.default_reset_term)) {
        Ok(s) => s,
        Err(e) => {
            emit_snapshot_error(&e.to_string(), json);
            return Err(1);
        }
    };
    let registry_store = MachineRegistryStore::new(paths.machines_file.clone());
    let registry = match registry_store.load() {
        Ok(r) => r,
        Err(e) => {
            emit_snapshot_error(&e.to_string(), json);
            return Err(1);
        }
    };
    let env: HashMap<String, String> = std::env::vars().collect();
    let cache_path = match paths.aggregation_cache_file_for_machine("local") {
        Ok(p) => p,
        Err(e) => {
            emit_snapshot_error(&e.to_string(), json);
            return Err(1);
        }
    };
    let mut service = SnapshotService::new(
        cache_path,
        PricingEngine::default(),
        antigravity_root_from_env(&env),
        env,
    );
    let desc = local_descriptor_from_registry(&registry);
    let tz = local_timezone();
    let now = Utc::now();
    let snapshot = match service.refresh_local(&desc, &state, &config, now, tz, None) {
        Ok(s) => s,
        Err(e) => {
            emit_snapshot_error(&e, json);
            return Err(1);
        }
    };
    if json {
        match serde_json::to_string_pretty(&snapshot) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                emit_snapshot_error(&e.to_string(), true);
                return Err(1);
            }
        }
    } else {
        println!("generatedAt: {}", snapshot.generated_at.to_rfc3339());
        println!("costSinceResetUSD: {}", snapshot.cost_since_reset_usd);
        println!("points: {}", snapshot.points.len());
    }
    Ok(())
}

fn emit_snapshot_error(message: &str, json: bool) {
    if json {
        let payload = serde_json::json!({ "error": "snapshot_failed", "message": message });
        eprintln!(
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
        );
    } else {
        eprintln!("error: {message}");
    }
}

struct LiveLocalFetcher {
    service: Arc<std::sync::Mutex<SnapshotService>>,
    state: AppState,
    config: AppConfiguration,
    tz: Tz,
}

impl MachineSnapshotFetcher for LiveLocalFetcher {
    fn fetch<'a>(
        &'a self,
        descriptor: &'a MachineDescriptor,
        desired_from: Option<NaiveDate>,
        now: DateTime<Utc>,
    ) -> FetchFuture<'a> {
        Box::pin(async move {
            if descriptor.kind != MachineKind::Local {
                return Err(SanitizedCollectionError {
                    code: "transport_failed".into(),
                    message: "SSH remote fetch requires configured SSH runner".into(),
                    detail: None,
                    remediation: None,
                });
            }
            let service = self.service.clone();
            let descriptor = descriptor.clone();
            let state = self.state.clone();
            let config = self.config.clone();
            let tz = self.tz;
            tokio::task::spawn_blocking(move || {
                let mut svc = service
                    .lock()
                    .map_err(|_| "local snapshot service lock poisoned".to_owned())?;
                svc.refresh_local(&descriptor, &state, &config, now, tz, desired_from)
            })
            .await
            .map_err(|e| SanitizedCollectionError {
                code: "cache_failed".into(),
                message: format!("local snapshot task failed: {e}"),
                detail: None,
                remediation: None,
            })?
            .map_err(|msg| SanitizedCollectionError {
                code: "cache_failed".into(),
                message: msg,
                detail: None,
                remediation: None,
            })
        })
    }
}

pub async fn run_serve(
    paths: &AppPaths,
    port_override: Option<u16>,
    assets_override: Option<PathBuf>,
) -> Result<(), i32> {
    let _logger = activate_bootstrap_logger(paths);
    let config = ConfigStore::new(paths.config_file.clone())
        .load_or_create()
        .map_err(|e| {
            eprintln!("error: {e}");
            1
        })?;
    let state = StateStore::new(paths.state_file.clone())
        .load(default_cycle_from_term(&config.default_reset_term))
        .map_err(|e| {
            eprintln!("error: {e}");
            1
        })?;
    let mut registry = MachineRegistryStore::new(paths.machines_file.clone())
        .load()
        .map_err(|e| {
            eprintln!("error: {e}");
            1
        })?;
    let dir_store = DashboardDirectoryNameStore::new(&paths.dashboard_state_file);
    let mut ids: Vec<String> = registry.machines.iter().map(|m| m.id.clone()).collect();
    ids.push("local".into());
    let _ = dir_store.reconcile_machine_registry(&ids);

    let local_desc = local_descriptor_from_registry(&registry);
    if !registry.machines.iter().any(|m| m.id == "local") {
        registry.machines.insert(0, local_desc);
    }

    let env: HashMap<String, String> = std::env::vars().collect();
    let cache_path = paths
        .aggregation_cache_file_for_machine("local")
        .map_err(|e| {
            eprintln!("error: {e}");
            1
        })?;
    let tz = local_timezone();
    let fetcher = Arc::new(LiveLocalFetcher {
        service: Arc::new(std::sync::Mutex::new(SnapshotService::new(
            cache_path,
            PricingEngine::default(),
            antigravity_root_from_env(&env),
            env,
        ))),
        state: state.clone(),
        config: config.clone(),
        tz,
    });
    let store = MachineSnapshotStore::new();
    let collector = Arc::new(MachineCollector::new(
        store,
        fetcher,
        config.poll_interval_seconds,
    ));
    let _ = collector.reconcile_registry(&registry, 1).await;
    collector.refresh_all(None, Utc::now()).await;

    let rate_limits = RateLimitUsageService::production(paths).ok().map(Arc::new);
    let router = Arc::new(MachineDashboardRouter::new(
        paths.clone(),
        config.clone(),
        state,
        collector,
        rate_limits,
        tz,
    ));
    let exe = std::env::current_exe().ok();
    let assets = Arc::new(StaticAssetResolver::resolve(
        assets_override.as_deref(),
        exe.as_deref(),
    ));
    let port = port_override.unwrap_or(config.dashboard_port);
    let handle = DashboardHTTPServer::start(port, router, assets)
        .await
        .map_err(|e| {
            eprintln!("error: failed to bind 127.0.0.1:{port}: {e}");
            1
        })?;
    println!("Listening on http://127.0.0.1:{}", handle.port());
    let _ = tokio::signal::ctrl_c().await;
    handle.stop().await;
    Ok(())
}

pub fn run_dashboard(paths: &AppPaths) -> Result<(), i32> {
    let _logger = activate_bootstrap_logger(paths);
    let env: HashMap<String, String> = std::env::vars().collect();
    let cli_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ccusage-gauge"));
    let desktop_exe = match resolve_desktop_executable(&env, &cli_exe) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return Err(1);
        }
    };
    let status = Command::new(&desktop_exe)
        .arg("--dashboard-only")
        .status()
        .map_err(|e| {
            eprintln!("error: failed to launch {}: {e}", desktop_exe.display());
            1
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(status.code().unwrap_or(1))
    }
}

#[allow(dead_code)]
fn _snapshot_type_check(_: &CostSnapshot) {}
