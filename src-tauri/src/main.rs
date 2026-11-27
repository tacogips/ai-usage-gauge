use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};

use ccusage_core::aggregation::collector::{
    FetchFuture, MachineCollector, MachineSnapshotFetcher, MachineSnapshotStore,
};
use ccusage_core::aggregation::snapshot_service::SnapshotService;
use ccusage_core::cache::DashboardDirectoryNameStore;
use ccusage_core::config::{BootstrapLogger, ConfigStore};
use ccusage_core::dashboard::MachineDashboardRouter;
use ccusage_core::paths::AppPaths;
use ccusage_core::pricing::PricingEngine;
use ccusage_core::rate_limits::service::RateLimitUsageService;
use ccusage_core::remote::registry::MachineRegistryStore;
use ccusage_core::state::StateStore;
use ccusage_core::types::{
    AppConfiguration, AppState, DirectoryNameLifecycle, MachineDescriptor, MachineKind,
    MachineRegistry, MachineRuntimeReconciler, ResetCycle, SanitizedCollectionError,
};

use ccusage_gauge_app::dialogs::render_dialog_html;
use ccusage_gauge_app::ipc::{dispatch_ipc_request, DashboardIpcRequest, DashboardIpcResponse};
use ccusage_gauge_app::launch_at_login::{LaunchAtLoginController, LaunchAtLoginState};
use ccusage_gauge_app::tray_icons::{TrayIconRenderer, TrayPresentationDedup};
use ccusage_gauge_app::tray_menu::TrayViewModel;

struct AppRuntime {
    router: Arc<MachineDashboardRouter>,
    collector: Arc<MachineCollector>,
    rate_limits: Option<Arc<RateLimitUsageService>>,
    state_store: StateStore,
    config: AppConfiguration,
    tz: Tz,
}

#[tauri::command]
async fn dashboard_request(
    window: tauri::WebviewWindow,
    runtime: tauri::State<'_, Arc<AppRuntime>>,
    request: DashboardIpcRequest,
) -> Result<DashboardIpcResponse, String> {
    if window.label() != "main" {
        return Err("Unknown dashboard window".into());
    }
    dispatch_ipc_request(&runtime.router, request).await
}

struct LocalTrayFetcher {
    service: Arc<std::sync::Mutex<SnapshotService>>,
    state: AppState,
    config: AppConfiguration,
    tz: Tz,
}

impl MachineSnapshotFetcher for LocalTrayFetcher {
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

fn local_timezone() -> Tz {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

fn default_cycle_from_term(term: &str) -> ResetCycle {
    match term {
        "hourly" => ResetCycle::Hourly,
        "weekly" => ResetCycle::Weekly,
        "monthly" => ResetCycle::Monthly,
        _ => ResetCycle::Daily,
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

fn build_runtime(paths: &AppPaths) -> Result<Arc<AppRuntime>, String> {
    let logger = BootstrapLogger::new(
        paths.log_dir.clone(),
        None,
        "ccusage-gauge-app",
        Utc::now,
        5 * 1024 * 1024,
        Duration::from_secs(7 * 86_400),
    );
    logger.activate();

    let config = ConfigStore::new(paths.config_file.clone())
        .load_or_create()
        .map_err(|e| e.to_string())?;
    let state_store = StateStore::new(paths.state_file.clone());
    let state = state_store
        .load(default_cycle_from_term(&config.default_reset_term))
        .map_err(|e| e.to_string())?;
    let mut registry = MachineRegistryStore::new(paths.machines_file.clone())
        .load()
        .map_err(|e| e.to_string())?;
    let dir_store = DashboardDirectoryNameStore::new(&paths.dashboard_state_file);
    let mut ids: Vec<String> = registry.machines.iter().map(|m| m.id.clone()).collect();
    ids.push("local".into());
    let _ = dir_store.reconcile_machine_registry(&ids);

    if !registry.machines.iter().any(|m| m.id == "local") {
        registry.machines.insert(
            0,
            MachineDescriptor {
                id: "local".into(),
                display_name: "Local".into(),
                kind: MachineKind::Local,
                enabled: true,
                ssh: None,
                codex_session_dirs: registry.local_session_sources.codex_session_dirs.clone(),
                claude_config_dirs: registry.local_session_sources.claude_config_dirs.clone(),
                include_default_codex_dir: registry.local_session_sources.include_default_codex_dir,
                include_default_claude_dir: registry
                    .local_session_sources
                    .include_default_claude_dir,
            },
        );
    }

    let env: HashMap<String, String> = std::env::vars().collect();
    let cache_path = paths
        .aggregation_cache_file_for_machine("local")
        .map_err(|e| e.to_string())?;
    let tz = local_timezone();
    let fetcher = Arc::new(LocalTrayFetcher {
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

    let reg_clone: MachineRegistry = registry;
    let col_clone = collector.clone();
    tauri::async_runtime::block_on(async move {
        let _ = col_clone.reconcile_registry(&reg_clone, 1).await;
    });

    let rate_limits = RateLimitUsageService::production(paths).ok().map(Arc::new);
    let router = Arc::new(MachineDashboardRouter::new(
        paths.clone(),
        config.clone(),
        state,
        collector.clone(),
        rate_limits.clone(),
        tz,
    ));

    Ok(Arc::new(AppRuntime {
        router,
        collector,
        rate_limits,
        state_store,
        config,
        tz,
    }))
}

fn main() {
    let mut dashboard_only = false;
    for arg in std::env::args().skip(1) {
        if arg == "--dashboard-only" {
            dashboard_only = true;
        } else {
            eprintln!("Unknown argument: {arg}");
            std::process::exit(2);
        }
    }

    let paths = AppPaths::production();
    let runtime = match build_runtime(&paths) {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    let autostart = runtime.config.dashboard_autostart;
    let runtime_for_setup = runtime.clone();

    tauri::Builder::default()
        .register_uri_scheme_protocol("ccusage-dialog", |_ctx, req| {
            let dialog = req.uri().path().trim_start_matches('/');
            let (ct, html) = render_dialog_html(dialog);
            tauri::http::Response::builder()
                .status(200)
                .header("Content-Type", ct)
                .body(html.into_bytes())
                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
        })
        .setup(move |app| {
            app.manage(runtime_for_setup.clone());

            #[cfg(target_os = "macos")]
            if !dashboard_only {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            if let Some(window) = app.get_webview_window("main") {
                if dashboard_only || autostart {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
                if !dashboard_only {
                    let win_clone = window.clone();
                    window.on_window_event(move |event| {
                        if let WindowEvent::CloseRequested { api, .. } = event {
                            api.prevent_close();
                            let _ = win_clone.hide();
                        }
                    });
                }
            }

            if !dashboard_only {
                setup_tray(app, runtime_for_setup.clone())?;
            } else {
                let col = runtime_for_setup.collector.clone();
                tauri::async_runtime::spawn(async move {
                    col.refresh_all(None, Utc::now()).await;
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![dashboard_request])
        .run(tauri::generate_context!())
        .expect("Unable to run CCUsage Gauge");
}

fn setup_tray(app: &mut tauri::App, runtime: Arc<AppRuntime>) -> tauri::Result<()> {
    let summary_item = MenuItem::with_id(
        app,
        "budget_summary",
        "Loading usage...",
        false,
        None::<&str>,
    )?;
    let toggle_dash = MenuItem::with_id(
        app,
        "toggle_dashboard",
        "Open dashboard window",
        true,
        None::<&str>,
    )?;
    let refresh_item = MenuItem::with_id(app, "refresh", "Refresh", true, None::<&str>)?;
    let launch_enabled = LaunchAtLoginController::status() == LaunchAtLoginState::On;
    let launch_item = CheckMenuItem::with_id(
        app,
        "launch_at_login",
        "Launch at Login",
        true,
        launch_enabled,
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &summary_item,
            &sep1,
            &toggle_dash,
            &refresh_item,
            &launch_item,
            &sep2,
            &quit_item,
        ],
    )?;

    let renderer = TrayIconRenderer::new();
    let initial_state = runtime.router.state.blocking_read().clone();
    let initial_vm = TrayViewModel::build(
        &initial_state,
        None,
        None,
        None,
        false,
        runtime.config.poll_interval_seconds,
        LaunchAtLoginController::status() == LaunchAtLoginState::RequiresApproval,
    );
    let rendered = renderer.render_strip(&initial_vm.render_state);
    let mut tray_builder = TrayIconBuilder::with_id("main-tray")
        .icon_as_template(true)
        .title(&initial_vm.status_title)
        .tooltip(&initial_vm.tooltip)
        .menu(&menu);
    if let Some(r) = rendered {
        tray_builder = tray_builder.icon(Image::new_owned(r.rgba, r.width, r.height));
    }

    let rt_menu = runtime.clone();
    let tray = tray_builder
        .on_menu_event(move |app_handle, event| match event.id().as_ref() {
            "toggle_dashboard" => {
                if let Some(win) = app_handle.get_webview_window("main") {
                    if win.is_visible().unwrap_or(false) {
                        let _ = win.hide();
                    } else {
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
            }
            "refresh" => {
                let col = rt_menu.collector.clone();
                tauri::async_runtime::spawn(async move {
                    col.refresh_all(None, Utc::now()).await;
                });
            }
            "launch_at_login" => {
                let next = LaunchAtLoginController::status() != LaunchAtLoginState::On;
                let _ = LaunchAtLoginController::set_enabled(next);
            }
            "quit" => {
                app_handle.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    let handle = app.handle().clone();
    let poll_secs = runtime.config.poll_interval_seconds.max(5);
    let _state_store_ref = &runtime.state_store;
    tauri::async_runtime::spawn(async move {
        let mut dedup = TrayPresentationDedup::default();
        let renderer = TrayIconRenderer::new();
        loop {
            let now = Utc::now();
            runtime.collector.refresh_all(None, now).await;
            let state = runtime.router.state.read().await.clone();
            let merged = runtime
                .collector
                .merged_snapshot(
                    &state,
                    "all",
                    ccusage_core::types::DashboardDataDisposition::Current,
                    now,
                    runtime.tz,
                )
                .await;
            let rl = match &runtime.rate_limits {
                Some(svc) => Some(svc.current().await),
                None => None,
            };
            let dash_open = handle
                .get_webview_window("main")
                .and_then(|w| w.is_visible().ok())
                .unwrap_or(false);
            let vm = TrayViewModel::build(
                &state,
                Some(&merged.snapshot),
                rl.as_ref(),
                None,
                dash_open,
                runtime.config.poll_interval_seconds,
                LaunchAtLoginController::status() == LaunchAtLoginState::RequiresApproval,
            );

            let update_icon = dedup.update_icon(&vm.render_state);
            let update_title = dedup.update_title(&vm.status_title);
            let update_tooltip = dedup.update_tooltip(&vm.tooltip);
            let tray_ref = tray.clone();
            let summary_ref = summary_item.clone();
            let toggle_ref = toggle_dash.clone();
            let rendered_opt = if update_icon {
                renderer.render_strip(&vm.render_state)
            } else {
                None
            };
            let _ = handle.run_on_main_thread(move || {
                if let Some(r) = rendered_opt {
                    let img = Image::new_owned(r.rgba, r.width, r.height);
                    let _ = tray_ref.set_icon_with_as_template(Some(img), true);
                }
                if update_title {
                    let _ = tray_ref.set_title(Some(&vm.status_title));
                }
                if update_tooltip {
                    let _ = tray_ref.set_tooltip(Some(&vm.tooltip));
                }
                let _ = summary_ref.set_text(&vm.budget_summary_line);
                let _ = toggle_ref.set_text(&vm.dashboard_toggle_label);
            });

            tokio::time::sleep(Duration::from_secs(u64::from(poll_secs))).await;
        }
    });

    Ok(())
}
