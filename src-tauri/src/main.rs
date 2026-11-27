use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};
use tokio::sync::Notify;

use ai_usage_core::aggregation::collector::{
    FetchFuture, MachineCollector, MachineSnapshotFetcher, MachineSnapshotStore, ResetCacheFuture,
};
use ai_usage_core::aggregation::snapshot_service::SnapshotService;
use ai_usage_core::cache::DashboardDirectoryNameStore;
use ai_usage_core::config::{BootstrapLogger, ConfigStore};
use ai_usage_core::dashboard::MachineDashboardRouter;
use ai_usage_core::paths::AppPaths;
use ai_usage_core::pricing::PricingEngine;
use ai_usage_core::rate_limits::service::RateLimitUsageService;
use ai_usage_core::remote::registry::MachineRegistryStore;
use ai_usage_core::state::{
    PieChartSelection, RatioTextSelection, ResetWindowCalculator, StateStore,
};
use ai_usage_core::types::{
    AppConfiguration, AppState, DirectoryNameLifecycle, MachineDescriptor, MachineKind,
    MachineRegistry, MachineRuntimeReconciler, PieChartId, RateLimitProvider, ResetCycle,
    SanitizedCollectionError,
};

use ai_usage_gauge_app::dialogs::render_dialog_html;
use ai_usage_gauge_app::ipc::{dispatch_ipc_request, DashboardIpcRequest, DashboardIpcResponse};
use ai_usage_gauge_app::launch_at_login::{LaunchAtLoginController, LaunchAtLoginState};
use ai_usage_gauge_app::tray_icons::{TrayIconRenderer, TrayPresentationDedup};
use ai_usage_gauge_app::tray_menu::TrayViewModel;

struct AppRuntime {
    router: Arc<MachineDashboardRouter>,
    collector: Arc<MachineCollector>,
    rate_limits: Option<Arc<RateLimitUsageService>>,
    state_store: StateStore,
    config: AppConfiguration,
    tz: Tz,
    wake_render: Arc<Notify>,
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
    store: MachineSnapshotStore,
    wake_render: Arc<Notify>,
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
            let store = self.store.clone();
            let wake = self.wake_render.clone();
            let rt_handle = tokio::runtime::Handle::current();
            let descriptor = descriptor.clone();
            let state = self.state.clone();
            let config = self.config.clone();
            let tz = self.tz;
            tokio::task::spawn_blocking(move || {
                let mut svc = service
                    .lock()
                    .map_err(|_| "local snapshot service lock poisoned".to_owned())?;
                svc.refresh_local_with_progress(
                    &descriptor,
                    &state,
                    &config,
                    now,
                    tz,
                    desired_from,
                    &mut |partial| {
                        let store_clone = store.clone();
                        let wake_clone = wake.clone();
                        rt_handle.spawn(async move {
                            store_clone.publish_progress("local", partial, now).await;
                            wake_clone.notify_one();
                        });
                    },
                )
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

    fn reset_cache<'a>(&'a self, machine_id: &'a str) -> ResetCacheFuture<'a> {
        let is_local = machine_id == "local";
        let service = self.service.clone();
        let wake = self.wake_render.clone();
        Box::pin(async move {
            if !is_local {
                return;
            }
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(mut svc) = service.lock() {
                    svc.reset_cache_state();
                }
            })
            .await;
            wake.notify_one();
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
    if let Some(p) = env
        .get("AI_USAGE_GAUGE_ANTIGRAVITY_DIR")
        .or_else(|| env.get("CCUSAGE_GAUGE_ANTIGRAVITY_DIR"))
    {
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
    paths.migrate_legacy_if_needed();
    let logger = BootstrapLogger::new(
        paths.log_dir.clone(),
        None,
        "ai-usage-gauge-app",
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
    let store = MachineSnapshotStore::new();
    let wake_render = Arc::new(Notify::new());
    let snapshot_service = Arc::new(std::sync::Mutex::new(SnapshotService::new(
        cache_path,
        PricingEngine::default(),
        antigravity_root_from_env(&env),
        env,
    )));
    // Pre-seed SQLite historical cache immediately (< 5ms, zero JSONL I/O) before live scan starts.
    let cached_seed = registry
        .machines
        .iter()
        .find(|m| m.id == "local")
        .and_then(|local_desc| {
            snapshot_service
                .lock()
                .ok()?
                .load_cached_snapshot(local_desc, &state, &config, Utc::now(), tz)
        });
    let fetcher = Arc::new(LocalTrayFetcher {
        service: snapshot_service,
        store: store.clone(),
        wake_render: wake_render.clone(),
        state: state.clone(),
        config: config.clone(),
        tz,
    });
    let collector = Arc::new(MachineCollector::new(
        store.clone(),
        fetcher,
        config.poll_interval_seconds,
    ));

    let reg_clone: MachineRegistry = registry;
    let col_clone = collector.clone();
    tauri::async_runtime::block_on(async move {
        let _ = col_clone.reconcile_registry(&reg_clone, 1).await;
        if let Some(seed) = cached_seed {
            store.publish_progress("local", seed, Utc::now()).await;
        }
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
        wake_render,
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
        .register_uri_scheme_protocol("ai-usage-dialog", |_ctx, req| {
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
        .expect("Unable to run AI Usage Gauge");
}

fn pie_chart_id_key(id: PieChartId) -> &'static str {
    match id {
        PieChartId::Budget => "budget",
        PieChartId::Claude5h => "claude5h",
        PieChartId::ClaudeWeekly => "claudeWeekly",
        PieChartId::ClaudeCredit => "claudeCredit",
        PieChartId::Codex5h => "codex5h",
        PieChartId::CodexWeekly => "codexWeekly",
        PieChartId::CodexCredit => "codexCredit",
        PieChartId::Gemini5h => "gemini5h",
        PieChartId::GeminiWeekly => "geminiWeekly",
    }
}

fn pie_chart_from_key(key: &str) -> Option<PieChartId> {
    match key {
        "budget" => Some(PieChartId::Budget),
        "claude5h" => Some(PieChartId::Claude5h),
        "claudeWeekly" => Some(PieChartId::ClaudeWeekly),
        "codex5h" => Some(PieChartId::Codex5h),
        "codexWeekly" => Some(PieChartId::CodexWeekly),
        "gemini5h" => Some(PieChartId::Gemini5h),
        "geminiWeekly" => Some(PieChartId::GeminiWeekly),
        _ => None,
    }
}

fn pie_chart_menu_label(id: PieChartId) -> &'static str {
    match id {
        PieChartId::Budget => "Budget",
        PieChartId::Claude5h => "Claude Code 5h",
        PieChartId::ClaudeWeekly => "Claude Code weekly",
        PieChartId::ClaudeCredit => "Claude Code credit",
        PieChartId::Codex5h => "Codex 5h",
        PieChartId::CodexWeekly => "Codex weekly",
        PieChartId::CodexCredit => "Codex credit",
        PieChartId::Gemini5h => "Gemini 5h",
        PieChartId::GeminiWeekly => "Gemini weekly",
    }
}

fn format_provider_line(
    rl: Option<&ai_usage_core::types::RateLimitsResponse>,
    provider: RateLimitProvider,
    label: &str,
) -> String {
    let Some(p) = rl.and_then(|r| r.providers.iter().find(|p| p.provider == provider)) else {
        return format!("{label}: unavailable");
    };
    let mut parts = Vec::new();
    if let Some(w) = &p.five_hour {
        parts.push(format!("5h {:.0}%", w.used_percent));
    }
    if let Some(w) = &p.weekly {
        parts.push(format!("Weekly {:.0}%", w.used_percent));
    }
    if let Some(c) = &p.credits {
        if c.enabled {
            if let Some(pct) = c.used_percent {
                parts.push(format!("Credit {:.0}%", pct));
            } else if let Some(bal) = c.balance {
                parts.push(format!("Credit {:.0}", bal));
            }
        }
    }
    if parts.is_empty() {
        format!("{label}: {:?}", p.status)
    } else {
        format!("{label}: {}", parts.join(" | "))
    }
}

fn setup_tray(app: &mut tauri::App, runtime: Arc<AppRuntime>) -> tauri::Result<()> {
    let initial_state = runtime.router.state.blocking_read().clone();

    let summary_item = MenuItem::with_id(
        app,
        "budget_summary",
        "Loading usage...",
        false,
        None::<&str>,
    )?;
    let claude_item = MenuItem::with_id(
        app,
        "provider_claude",
        "Claude Code: loading...",
        false,
        None::<&str>,
    )?;
    let codex_item = MenuItem::with_id(
        app,
        "provider_codex",
        "Codex: loading...",
        false,
        None::<&str>,
    )?;
    let gemini_item = MenuItem::with_id(
        app,
        "provider_gemini",
        "Gemini: loading...",
        false,
        None::<&str>,
    )?;

    // Usage icons submenu: Budget + Claude/Codex/Gemini 5h & weekly meters.
    let checked_icons = initial_state.menu_bar_pie_charts.len();
    let mut icon_check_items: Vec<(PieChartId, CheckMenuItem<tauri::Wry>)> = Vec::new();
    for id in PieChartId::menu_bar_cases() {
        let checked = initial_state.menu_bar_pie_charts.contains(&id);
        let enabled = !(checked && checked_icons <= 1);
        let item = CheckMenuItem::with_id(
            app,
            format!("icon:{}", pie_chart_id_key(id)),
            pie_chart_menu_label(id),
            enabled,
            checked,
            None::<&str>,
        )?;
        icon_check_items.push((id, item));
    }
    let icon_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = icon_check_items
        .iter()
        .map(|(_, item)| item as &dyn tauri::menu::IsMenuItem<tauri::Wry>)
        .collect();
    let usage_icons_submenu = Submenu::with_items(app, "Usage icons", true, &icon_refs)?;

    // Ratio text submenu: Cost (USD) + per-vendor percentage labels beside icons.
    let ratio_cost_item = CheckMenuItem::with_id(
        app,
        "ratio:cost",
        "Cost (USD)",
        true,
        initial_state.menu_bar_show_cost,
        None::<&str>,
    )?;
    let ratio_sep = PredefinedMenuItem::separator(app)?;
    let mut ratio_check_items: Vec<(PieChartId, CheckMenuItem<tauri::Wry>)> = Vec::new();
    for id in PieChartId::menu_bar_cases() {
        let checked = initial_state.menu_bar_ratio_labels.contains(&id)
            && (id != PieChartId::Budget || !initial_state.menu_bar_cost_only);
        let item = CheckMenuItem::with_id(
            app,
            format!("ratio:{}", pie_chart_id_key(id)),
            format!("{} %", pie_chart_menu_label(id)),
            true,
            checked,
            None::<&str>,
        )?;
        ratio_check_items.push((id, item));
    }
    let mut ratio_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        vec![&ratio_cost_item, &ratio_sep];
    for (_, item) in &ratio_check_items {
        ratio_refs.push(item);
    }
    let ratio_text_submenu = Submenu::with_items(app, "Ratio text", true, &ratio_refs)?;

    // Aggregation period submenu.
    let cycle_hourly = CheckMenuItem::with_id(
        app,
        "cycle:hourly",
        "Hourly",
        true,
        matches!(initial_state.reset_cycle, ResetCycle::Hourly),
        None::<&str>,
    )?;
    let cycle_daily = CheckMenuItem::with_id(
        app,
        "cycle:daily",
        "Daily",
        true,
        matches!(initial_state.reset_cycle, ResetCycle::Daily),
        None::<&str>,
    )?;
    let cycle_weekly = CheckMenuItem::with_id(
        app,
        "cycle:weekly",
        "Weekly",
        true,
        matches!(initial_state.reset_cycle, ResetCycle::Weekly),
        None::<&str>,
    )?;
    let cycle_monthly = CheckMenuItem::with_id(
        app,
        "cycle:monthly",
        "Monthly",
        true,
        matches!(initial_state.reset_cycle, ResetCycle::Monthly),
        None::<&str>,
    )?;
    let cycle_submenu = Submenu::with_items(
        app,
        "Aggregation period",
        true,
        &[&cycle_hourly, &cycle_daily, &cycle_weekly, &cycle_monthly],
    )?;

    let open_dash = MenuItem::with_id(
        app,
        "open_dashboard",
        "Open dashboard",
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
    let sep3 = PredefinedMenuItem::separator(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &open_dash,
            &refresh_item,
            &sep1,
            &usage_icons_submenu,
            &ratio_text_submenu,
            &cycle_submenu,
            &launch_item,
            &sep2,
            &summary_item,
            &claude_item,
            &codex_item,
            &gemini_item,
            &sep3,
            &quit_item,
        ],
    )?;

    let renderer = TrayIconRenderer::new();
    // Load cached rate limits synchronously (< 1ms) so vendor fill gauges appear on frame 0.
    let initial_rl = runtime
        .rate_limits
        .as_ref()
        .and_then(|svc| svc.cached_snapshot(Utc::now()));
    let initial_vm = TrayViewModel::build(
        &initial_state,
        None,
        initial_rl.as_ref(),
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

    let wake_render = runtime.wake_render.clone();
    let wake_refresh = Arc::new(Notify::new());
    let wake_rate_limits = Arc::new(Notify::new());
    let latest_rate_limits = Arc::new(tokio::sync::RwLock::new(initial_rl));

    let rt_menu = runtime.clone();
    let wake_render_menu = wake_render.clone();
    let wake_refresh_menu = wake_refresh.clone();
    let wake_rl_menu = wake_rate_limits.clone();
    let icon_items_menu = icon_check_items.clone();
    let ratio_items_menu = ratio_check_items.clone();
    let ratio_cost_menu = ratio_cost_item.clone();
    let cycle_items_menu = (
        cycle_hourly.clone(),
        cycle_daily.clone(),
        cycle_weekly.clone(),
        cycle_monthly.clone(),
    );

    let tray = tray_builder
        .on_menu_event(move |app_handle, event| {
            let id_str = event.id().as_ref();
            if let Some(key) = id_str.strip_prefix("icon:") {
                if let Some(chart_id) = pie_chart_from_key(key) {
                    let rt = rt_menu.clone();
                    let wake = wake_render_menu.clone();
                    let items = icon_items_menu.clone();
                    let handle = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        let updated = {
                            let mut st = rt.router.state.write().await;
                            st.menu_bar_pie_charts =
                                PieChartSelection::toggled(chart_id, &st.menu_bar_pie_charts);
                            let _ = rt.state_store.save(&st);
                            st.menu_bar_pie_charts.clone()
                        };
                        let count = updated.len();
                        let _ = handle.run_on_main_thread(move || {
                            for (cid, item) in &items {
                                let checked = updated.contains(cid);
                                let _ = item.set_checked(checked);
                                let _ = item.set_enabled(!(checked && count <= 1));
                            }
                        });
                        wake.notify_one();
                    });
                }
                return;
            }
            if id_str == "ratio:cost" {
                let rt = rt_menu.clone();
                let wake = wake_render_menu.clone();
                let item = ratio_cost_menu.clone();
                let handle = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    let show_cost = {
                        let mut st = rt.router.state.write().await;
                        st.menu_bar_show_cost = !st.menu_bar_show_cost;
                        let _ = rt.state_store.save(&st);
                        st.menu_bar_show_cost
                    };
                    let _ = handle.run_on_main_thread(move || {
                        let _ = item.set_checked(show_cost);
                    });
                    wake.notify_one();
                });
                return;
            }
            if let Some(key) = id_str.strip_prefix("ratio:") {
                if let Some(chart_id) = pie_chart_from_key(key) {
                    let rt = rt_menu.clone();
                    let wake = wake_render_menu.clone();
                    let items = ratio_items_menu.clone();
                    let handle = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        let (labels, cost_only) = {
                            let mut st = rt.router.state.write().await;
                            if chart_id == PieChartId::Budget {
                                let currently_shown = st
                                    .menu_bar_ratio_labels
                                    .contains(&PieChartId::Budget)
                                    && !st.menu_bar_cost_only;
                                if currently_shown {
                                    st.menu_bar_cost_only = true;
                                    st.menu_bar_ratio_labels
                                        .retain(|id| *id != PieChartId::Budget);
                                } else {
                                    st.menu_bar_cost_only = false;
                                    if !st.menu_bar_ratio_labels.contains(&PieChartId::Budget) {
                                        st.menu_bar_ratio_labels = RatioTextSelection::toggled(
                                            PieChartId::Budget,
                                            &st.menu_bar_ratio_labels,
                                        );
                                    }
                                }
                            } else {
                                st.menu_bar_ratio_labels = RatioTextSelection::toggled(
                                    chart_id,
                                    &st.menu_bar_ratio_labels,
                                );
                            }
                            let _ = rt.state_store.save(&st);
                            (st.menu_bar_ratio_labels.clone(), st.menu_bar_cost_only)
                        };
                        let _ = handle.run_on_main_thread(move || {
                            for (cid, item) in &items {
                                let checked = labels.contains(cid)
                                    && (*cid != PieChartId::Budget || !cost_only);
                                let _ = item.set_checked(checked);
                            }
                        });
                        wake.notify_one();
                    });
                }
                return;
            }
            if let Some(key) = id_str.strip_prefix("cycle:") {
                let next_cycle = match key {
                    "hourly" => Some(ResetCycle::Hourly),
                    "daily" => Some(ResetCycle::Daily),
                    "weekly" => Some(ResetCycle::Weekly),
                    "monthly" => Some(ResetCycle::Monthly),
                    _ => None,
                };
                if let Some(cycle) = next_cycle {
                    let rt = rt_menu.clone();
                    let wake = wake_render_menu.clone();
                    let (ch, cd, cw, cm) = cycle_items_menu.clone();
                    let handle = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        {
                            let mut st = rt.router.state.write().await;
                            if let Ok(next_state) = ResetWindowCalculator::new(rt.tz)
                                .changing(&st, cycle.clone(), Utc::now())
                            {
                                *st = next_state;
                            } else {
                                st.reset_cycle = cycle.clone();
                                st.baseline = None;
                            }
                            let _ = rt.state_store.save(&st);
                        }
                        let _ = handle.run_on_main_thread(move || {
                            let _ = ch.set_checked(matches!(cycle, ResetCycle::Hourly));
                            let _ = cd.set_checked(matches!(cycle, ResetCycle::Daily));
                            let _ = cw.set_checked(matches!(cycle, ResetCycle::Weekly));
                            let _ = cm.set_checked(matches!(cycle, ResetCycle::Monthly));
                        });
                        wake.notify_one();
                    });
                }
                return;
            }
            match id_str {
                "open_dashboard" => {
                    if let Some(win) = app_handle.get_webview_window("main") {
                        let _ = win.unminimize();
                        let _ = win.show();
                        let _ = win.set_focus();
                    }
                }
                "refresh" => {
                    wake_refresh_menu.notify_one();
                    wake_rl_menu.notify_one();
                }
                "launch_at_login" => {
                    let next = LaunchAtLoginController::status() != LaunchAtLoginState::On;
                    let _ = LaunchAtLoginController::set_enabled(next);
                }
                "quit" => {
                    app_handle.exit(0);
                }
                _ => {}
            }
        })
        .build(app)?;

    let handle = app.handle().clone();
    let poll_secs = runtime.config.poll_interval_seconds.max(5);

    // Task A: Independent rate-limit poller — fetches fresh Claude/Codex/Gemini usage
    // immediately on startup without waiting for JSONL log scanning, and wakes tray render.
    if let Some(rl_svc) = runtime.rate_limits.clone() {
        let rl_slot = latest_rate_limits.clone();
        let wake_r = wake_render.clone();
        let wake_rl = wake_rate_limits.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let fresh = rl_svc.current().await;
                *rl_slot.write().await = Some(fresh);
                wake_r.notify_one();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(u64::from(poll_secs))) => {}
                    _ = wake_rl.notified() => {}
                }
            }
        });
    }

    // Task B: Independent usage collector poller — runs progressive 5-worker JSONL scan
    // in the background and wakes tray render on each slice and on final completion.
    {
        let col = runtime.collector.clone();
        let wake_r = wake_render.clone();
        let wake_col = wake_refresh.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                col.refresh_all(None, Utc::now()).await;
                wake_r.notify_one();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(u64::from(poll_secs))) => {}
                    _ = wake_col.notified() => {}
                }
            }
        });
    }

    // Task C: Non-blocking tray renderer — never awaits I/O or network; renders immediately
    // whenever cached rate limits, fresh rate limits, progressive scan slices, or menu toggles fire.
    tauri::async_runtime::spawn(async move {
        let mut dedup = TrayPresentationDedup::default();
        let renderer = TrayIconRenderer::new();
        loop {
            let now = Utc::now();
            let state = runtime.router.state.read().await.clone();
            let merged = runtime
                .collector
                .merged_snapshot(
                    &state,
                    "all",
                    ai_usage_core::types::DashboardDataDisposition::Current,
                    now,
                    runtime.tz,
                )
                .await;
            let rl = latest_rate_limits.read().await.clone();
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
            let claude_line =
                format_provider_line(rl.as_ref(), RateLimitProvider::Claude, "Claude Code");
            let codex_line = format_provider_line(rl.as_ref(), RateLimitProvider::Codex, "Codex");
            let gemini_line =
                format_provider_line(rl.as_ref(), RateLimitProvider::Gemini, "Gemini");

            let update_icon = dedup.update_icon(&vm.render_state);
            let update_title = dedup.update_title(&vm.status_title);
            let update_tooltip = dedup.update_tooltip(&vm.tooltip);
            let tray_ref = tray.clone();
            let summary_ref = summary_item.clone();
            let claude_ref = claude_item.clone();
            let codex_ref = codex_item.clone();
            let gemini_ref = gemini_item.clone();
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
                let _ = claude_ref.set_text(&claude_line);
                let _ = codex_ref.set_text(&codex_line);
                let _ = gemini_ref.set_text(&gemini_line);
            });

            wake_render.notified().await;
        }
    });

    Ok(())
}
