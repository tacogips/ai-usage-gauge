use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::aggregation::blocks::local_midnight_utc;
use crate::aggregation::collector::MachineCollector;
use crate::aggregation::merge::CostSnapshotMerge;
use crate::cache::{DashboardDirectoryNameStore, DashboardStateStore, DirectoryNameError};
use crate::dashboard::query::{DashboardQueryService, DirectoryFilter};
use crate::paths::AppPaths;
use crate::rate_limits::service::RateLimitUsageService;
use crate::remote::registry::MachineRegistryStore;
use crate::types::{
    AppConfiguration, AppState, DashboardDataDisposition, DashboardUIState, MachineRegistry,
    RateLimitsResponse,
};

#[derive(Debug, Clone)]
pub struct RouterRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub is_trusted_ipc: bool,
    pub expected_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct RouterResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl RouterResponse {
    pub fn json(status: u16, value: &impl Serialize) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
        }
    }

    pub fn error(status: u16, code: &str, message: &str) -> Self {
        Self::json(status, &json!({ "error": code, "message": message }))
    }
}

pub struct MachineDashboardRouter {
    pub paths: AppPaths,
    pub config: RwLock<AppConfiguration>,
    pub state: RwLock<AppState>,
    pub collector: Arc<MachineCollector>,
    pub rate_limits: Option<Arc<RateLimitUsageService>>,
    pub dashboard_state_store: DashboardStateStore,
    pub directory_name_store: DashboardDirectoryNameStore,
    pub registry_store: MachineRegistryStore,
    pub legacy_local_days: RwLock<BTreeSet<String>>,
    pub timezone: Tz,
    pub(super) registry_revision: RwLock<u64>,
}

impl MachineDashboardRouter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: AppPaths,
        config: AppConfiguration,
        state: AppState,
        collector: Arc<MachineCollector>,
        rate_limits: Option<Arc<RateLimitUsageService>>,
        timezone: Tz,
    ) -> Self {
        let dashboard_state_store = DashboardStateStore::new(&paths.dashboard_state_file);
        let directory_name_store = DashboardDirectoryNameStore::new(&paths.dashboard_state_file);
        let registry_store = MachineRegistryStore::new(paths.machines_file.clone());
        Self {
            paths,
            config: RwLock::new(config),
            state: RwLock::new(state),
            collector,
            rate_limits,
            dashboard_state_store,
            directory_name_store,
            registry_store,
            legacy_local_days: RwLock::new(BTreeSet::new()),
            timezone,
            registry_revision: RwLock::new(1),
        }
    }

    pub async fn route(&self, req: RouterRequest) -> RouterResponse {
        // 1. Fail-closed security gate BEFORE any body decoding, validation, or store access.
        if let Some(rejection) = self.check_security_gate(&req) {
            return rejection;
        }

        let method = req.method.as_str();
        let path = req.path.trim_end_matches('/');
        let path = if path.is_empty() { "/" } else { path };
        let params = parse_query(&req.query);
        let now = Utc::now();

        match (method, path) {
            ("GET", "/api/health") => {
                let statuses = self.collector.store.statuses().await;
                RouterResponse::json(
                    200,
                    &json!({
                        "status": "ok",
                        "version": crate::version::APP_VERSION,
                        "machines": statuses.len(),
                        "timestamp": now,
                    }),
                )
            }
            ("GET", "/api/chart-colors") => {
                let cfg = self.config.read().await;
                RouterResponse::json(200, &cfg.chart_colors)
            }
            ("GET", "/api/rate-limits") => self.handle_rate_limits(now).await,
            ("GET", "/api/machine-status") => {
                let statuses = self.collector.store.statuses().await;
                RouterResponse::json(200, &json!({ "machines": statuses }))
            }
            ("GET", "/api/load-status") => {
                let progress = self.collector.store.range_progress().await;
                RouterResponse::json(200, &json!({ "progress": progress }))
            }
            ("GET", "/api/refresh") => {
                self.collector.refresh_all(None, now).await;
                let statuses = self.collector.store.statuses().await;
                RouterResponse::json(200, &json!({ "status": "ok", "machines": statuses }))
            }
            ("GET", "/api/recent") => self.handle_recent(&params, now).await,
            ("GET", "/api/day") => self.handle_day(&params, now).await,
            ("GET", "/api/period") => self.handle_period(&params, now).await,
            ("GET", "/api/metrics") => self.handle_metrics(&params, now).await,
            ("GET", "/api/cost-series") => self.handle_cost_series(&params, now).await,
            ("GET", "/api/budget") => self.handle_budget(&params, now).await,
            ("GET", "/api/subdirectories") => self.handle_subdirectories(&params, now).await,
            ("PUT", "/api/subdirectories/name") => {
                self.handle_set_directory_name(&req.body, now).await
            }
            ("GET", "/api/dashboard-state") => match self.dashboard_state_store.load() {
                Ok(Some(s)) => RouterResponse::json(200, &s),
                Ok(None) => RouterResponse::json(200, &Value::Null),
                Err(_) => RouterResponse::error(
                    503,
                    "dashboard_state_unavailable",
                    "Dashboard state storage is unavailable.",
                ),
            },
            ("PUT", "/api/dashboard-state") => {
                let parsed: DashboardUIState = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => {
                        return RouterResponse::error(
                            400,
                            "invalid_dashboard_state",
                            "Invalid dashboard state payload.",
                        )
                    }
                };
                let ts = now.timestamp_millis() as f64 / 1000.0;
                match self.dashboard_state_store.save(&parsed, ts) {
                    Ok(()) => RouterResponse::json(200, &parsed),
                    Err(crate::cache::DashboardStateError::InvalidState) => RouterResponse::error(
                        400,
                        "invalid_dashboard_state",
                        "Dashboard state failed validation.",
                    ),
                    Err(_) => RouterResponse::error(
                        503,
                        "dashboard_state_unavailable",
                        "Dashboard state storage is unavailable.",
                    ),
                }
            }
            ("DELETE", "/api/cache") => self.handle_clear_cache(&params).await,
            ("GET", "/api/machines") => self.handle_list_machines().await,
            ("POST", "/api/machines") => self.handle_add_machine(&req.body).await,
            _ if path.starts_with("/api/machines/") => {
                self.handle_machine_item(method, path, &req.body, now).await
            }
            _ => RouterResponse::error(404, "not_found", "API route not found."),
        }
    }

    fn check_security_gate(&self, req: &RouterRequest) -> Option<RouterResponse> {
        if req.method.eq_ignore_ascii_case("OPTIONS") {
            return Some(RouterResponse::error(
                405,
                "method_not_allowed",
                "OPTIONS is not supported.",
            ));
        }
        if req.is_trusted_ipc {
            return None;
        }
        let is_mutation = matches!(req.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE")
            || req.path.trim_end_matches('/') == "/api/refresh";

        if let Some(host) = req.headers.get("host") {
            if !is_valid_loopback_host(host, req.expected_port) {
                return Some(RouterResponse::error(
                    403,
                    "forbidden_host",
                    "Host header must target loopback.",
                ));
            }
        }
        if let Some(origin) = req.headers.get("origin") {
            if !is_valid_loopback_origin(origin, req.expected_port) {
                return Some(RouterResponse::error(
                    403,
                    "forbidden_origin",
                    "Cross-origin requests are forbidden.",
                ));
            }
        }
        if let Some(site) = req.headers.get("sec-fetch-site") {
            if site != "same-origin" && site != "none" {
                return Some(RouterResponse::error(
                    403,
                    "forbidden_fetch_site",
                    "Cross-site browser requests are forbidden.",
                ));
            }
        }
        if is_mutation {
            let header = req
                .headers
                .get("x-ai-usage-gauge-mutation")
                .or_else(|| req.headers.get("x-ai-usage-gauge-mutation"))
                .map(String::as_str);
            if header != Some("1") {
                return Some(RouterResponse::error(
                    403,
                    "mutation_header_required",
                    "X-AI-Usage-Gauge-Mutation: 1 header is required.",
                ));
            }
        }
        None
    }

    async fn handle_rate_limits(&self, now: DateTime<Utc>) -> RouterResponse {
        if let Some(service) = &self.rate_limits {
            let resp = service.current().await;
            RouterResponse::json(200, &resp)
        } else {
            RouterResponse::json(
                200,
                &RateLimitsResponse {
                    generated_at: now,
                    scope: "local".into(),
                    providers: vec![],
                },
            )
        }
    }

    async fn select_machines(
        &self,
        params: &QueryParams,
    ) -> Result<(Vec<String>, String), RouterResponse> {
        let requested_vals = params.get_all("machine");
        let statuses = self.collector.store.statuses().await;
        let by_id: HashMap<String, _> = statuses.iter().map(|s| (s.id.clone(), s)).collect();

        if requested_vals.is_empty() || (requested_vals.len() == 1 && requested_vals[0] == "all") {
            let ids = statuses
                .iter()
                .filter(|s| s.enabled)
                .map(|s| s.id.clone())
                .collect();
            return Ok((ids, "all".into()));
        }

        if requested_vals.iter().any(|v| *v == "all") {
            return Err(RouterResponse::error(
                400,
                "invalid_machine_selection",
                "Cannot mix machine=all with specific machine IDs.",
            ));
        }

        let mut selected = Vec::new();
        for id in requested_vals {
            let Some(st) = by_id.get(id) else {
                return Err(RouterResponse::error(
                    404,
                    "machine_not_found",
                    "Requested machine ID was not found.",
                ));
            };
            if !st.enabled {
                return Err(RouterResponse::error(
                    409,
                    "machine_disabled",
                    "Requested machine is disabled.",
                ));
            }
            if !selected.contains(id) {
                selected.push(id.clone());
            }
        }
        let label = selected.join(",");
        Ok((selected, label))
    }

    async fn parse_directory_filter(
        &self,
        params: &QueryParams,
        selected_machines: &[String],
    ) -> Result<DirectoryFilter, RouterResponse> {
        let raw_dirs = params.get_all("directory");
        if raw_dirs.is_empty() {
            return Ok(DirectoryFilter::default());
        }
        let known: BTreeSet<String> = self
            .collector
            .store
            .statuses()
            .await
            .into_iter()
            .map(|s| s.id)
            .collect();
        let mut filter = DirectoryFilter::default();
        for raw in raw_dirs {
            let decoded = percent_decode_str(raw).decode_utf8_lossy().into_owned();
            let Some((machine, path)) = decoded.split_once(':') else {
                return Err(RouterResponse::error(
                    400,
                    "invalid_directory_filter",
                    "Directory filter must be formatted as <machine>:<path>.",
                ));
            };
            if machine.is_empty() || path.is_empty() {
                return Err(RouterResponse::error(
                    400,
                    "invalid_directory_filter",
                    "Directory filter machine and path must be non-empty.",
                ));
            }
            if !known.contains(machine) {
                return Err(RouterResponse::error(
                    404,
                    "machine_not_found",
                    "Directory filter references an unknown machine.",
                ));
            }
            if selected_machines.iter().any(|m| m == machine) {
                filter
                    .by_machine
                    .entry(machine.to_owned())
                    .or_default()
                    .insert(path.to_owned());
            }
        }
        Ok(filter)
    }

    async fn merged_for_selection(
        &self,
        selected: &[String],
        requested: &str,
        disposition: DashboardDataDisposition,
        now: DateTime<Utc>,
    ) -> crate::aggregation::merge::MergedDashboardSnapshot {
        let all_inputs = self.collector.store.inputs().await;
        let filtered: Vec<_> = all_inputs
            .into_iter()
            .filter(|i| selected.contains(&i.status.id))
            .collect();
        let state = self.state.read().await.clone();
        CostSnapshotMerge::merge(
            &filtered,
            &state,
            requested,
            disposition,
            now,
            self.timezone,
        )
    }

    async fn handle_recent(&self, params: &QueryParams, now: DateTime<Utc>) -> RouterResponse {
        let (selected, label) = match self.select_machines(params).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let hours = params
            .get_one("hours")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(12);
        let merged = self
            .merged_for_selection(&selected, &label, DashboardDataDisposition::Current, now)
            .await;
        let resp = DashboardQueryService::recent(&merged.snapshot, hours, now);
        RouterResponse::json(
            200,
            &json!({
                "generatedAt": resp.generated_at,
                "points": resp.points,
                "scope": merged.scope,
                "machineLatestEvents": merged.machine_latest_events,
            }),
        )
    }

    async fn handle_day(&self, params: &QueryParams, now: DateTime<Utc>) -> RouterResponse {
        let (selected, label) = match self.select_machines(params).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let day = match params.get_one("date") {
            Some(s) => match NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                Ok(d) => d,
                Err(_) => {
                    return RouterResponse::error(400, "invalid_date", "Invalid date parameter.")
                }
            },
            None => now.with_timezone(&self.timezone).date_naive(),
        };
        let today = now.with_timezone(&self.timezone).date_naive();
        let disp = if day < today {
            DashboardDataDisposition::Historical
        } else {
            DashboardDataDisposition::Current
        };
        let merged = self
            .merged_for_selection(&selected, &label, disp, now)
            .await;
        let resp = DashboardQueryService::day(&merged.snapshot, day, self.timezone);
        RouterResponse::json(
            200,
            &json!({
                "generatedAt": resp.generated_at,
                "points": resp.points,
                "scope": merged.scope,
                "machineLatestEvents": merged.machine_latest_events,
            }),
        )
    }

    async fn handle_period(&self, params: &QueryParams, now: DateTime<Utc>) -> RouterResponse {
        let (selected, label) = match self.select_machines(params).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let (start, end, disp) = match resolve_range_bounds(params, now, self.timezone) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let merged = self
            .merged_for_selection(&selected, &label, disp, now)
            .await;
        let resp = DashboardQueryService::period(&merged.snapshot, start, end);
        RouterResponse::json(
            200,
            &json!({
                "generatedAt": resp.generated_at,
                "points": resp.points,
                "scope": merged.scope,
                "machineLatestEvents": merged.machine_latest_events,
            }),
        )
    }

    async fn handle_metrics(&self, params: &QueryParams, now: DateTime<Utc>) -> RouterResponse {
        let (selected, label) = match self.select_machines(params).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let dir_filter = match self.parse_directory_filter(params, &selected).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let (start, end, disp) = match resolve_range_bounds(params, now, self.timezone) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let use_sessions = params.get_one("range") == Some("recent12h");
        let merged = self
            .merged_for_selection(&selected, &label, disp, now)
            .await;
        let resp = DashboardQueryService::metrics(
            &merged.snapshot,
            start,
            end,
            use_sessions,
            &dir_filter,
            self.timezone,
        );
        RouterResponse::json(
            200,
            &json!({
                "generatedAt": resp.generated_at,
                "totalCostUsd": resp.total_cost_usd,
                "totalTokens": resp.total_tokens,
                "inputTokens": resp.input_tokens,
                "outputTokens": resp.output_tokens,
                "cacheCreationTokens": resp.cache_creation_tokens,
                "cacheReadTokens": resp.cache_read_tokens,
                "records": resp.records,
                "scope": merged.scope,
                "machineLatestEvents": merged.machine_latest_events,
            }),
        )
    }

    async fn handle_cost_series(&self, params: &QueryParams, now: DateTime<Utc>) -> RouterResponse {
        let (selected, label) = match self.select_machines(params).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let dir_filter = match self.parse_directory_filter(params, &selected).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let directory_breakdown = match params.parse_strict_bool("directoryBreakdown") {
            Ok(v) => v.unwrap_or(false),
            Err(r) => return r,
        };
        let effort_breakdown = match params.parse_strict_bool("effortBreakdown") {
            Ok(v) => v.unwrap_or(false),
            Err(r) => return r,
        };
        let granularity = params.get_one("granularity").unwrap_or("daily");
        if !["15min", "hourly", "6hour", "daily"].contains(&granularity) {
            return RouterResponse::error(400, "invalid_granularity", "Invalid granularity.");
        }
        let (start, end, disp) = match resolve_range_bounds(params, now, self.timezone) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let merged = self
            .merged_for_selection(&selected, &label, disp, now)
            .await;
        let resp = DashboardQueryService::cost_series(
            &merged.snapshot,
            start,
            end,
            granularity,
            directory_breakdown,
            effort_breakdown,
            &dir_filter,
            self.timezone,
        );
        RouterResponse::json(
            200,
            &json!({
                "generatedAt": resp.generated_at,
                "granularity": resp.granularity,
                "points": resp.points,
                "scope": merged.scope,
                "machineLatestEvents": merged.machine_latest_events,
            }),
        )
    }

    async fn handle_budget(&self, params: &QueryParams, now: DateTime<Utc>) -> RouterResponse {
        let (selected, label) = match self.select_machines(params).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let dir_filter = match self.parse_directory_filter(params, &selected).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let merged = self
            .merged_for_selection(&selected, &label, DashboardDataDisposition::Current, now)
            .await;
        let state = self.state.read().await.clone();
        let legacy = self.legacy_local_days.read().await.clone();
        let resp = DashboardQueryService::budget(
            &merged.snapshot,
            &state,
            &dir_filter,
            &legacy,
            now,
            self.timezone,
        );
        RouterResponse::json(
            200,
            &json!({
                "generatedAt": resp.generated_at,
                "activeBoundaryAt": resp.active_boundary_at,
                "resetCycle": resp.reset_cycle,
                "budget": resp.budget,
                "scope": merged.scope,
            }),
        )
    }

    async fn handle_subdirectories(
        &self,
        params: &QueryParams,
        now: DateTime<Utc>,
    ) -> RouterResponse {
        let (selected, label) = match self.select_machines(params).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        let merged = self
            .merged_for_selection(&selected, &label, DashboardDataDisposition::Historical, now)
            .await;
        let mut by_machine: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for s in &merged.snapshot.dashboard_sessions {
            if let Some(dir) = &s.directory {
                by_machine
                    .entry(s.machine.clone())
                    .or_default()
                    .insert(dir.clone());
            }
        }
        let directories: BTreeMap<String, Vec<String>> = by_machine
            .iter()
            .map(|(m, dirs)| (m.clone(), dirs.iter().cloned().collect()))
            .collect();

        let stored_names = match self.directory_name_store.names(&selected) {
            Ok(map) => map,
            Err(_) => {
                return RouterResponse::error(
                    503,
                    "directory_name_unavailable",
                    "Directory name storage is unavailable.",
                )
            }
        };

        let mut filtered_names: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for (m, map) in stored_names {
            if let Some(discovered) = by_machine.get(&m) {
                let kept: BTreeMap<String, String> = map
                    .into_iter()
                    .filter(|(dir, _)| discovered.contains(dir))
                    .collect();
                if !kept.is_empty() {
                    filtered_names.insert(m, kept);
                }
            }
        }

        let mut body = json!({
            "directories": directories,
        });
        if !filtered_names.is_empty() {
            body["names"] = json!(filtered_names);
        }
        RouterResponse::json(200, &body)
    }

    async fn handle_set_directory_name(&self, body: &[u8], now: DateTime<Utc>) -> RouterResponse {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Req {
            machine: String,
            directory: String,
            name: Option<String>,
        }
        let parsed: Req = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                return RouterResponse::error(
                    400,
                    "invalid_directory_name",
                    "Invalid directory name payload.",
                )
            }
        };
        let known: BTreeSet<String> = self
            .collector
            .store
            .statuses()
            .await
            .into_iter()
            .map(|s| s.id)
            .collect();
        if !known.contains(&parsed.machine) {
            return RouterResponse::error(404, "machine_not_found", "Machine not found.");
        }
        let ts = now.timestamp_millis() as f64 / 1000.0;
        match self.directory_name_store.set_name(
            &parsed.machine,
            &parsed.directory,
            parsed.name.as_deref(),
            ts,
        ) {
            Ok(saved) => RouterResponse::json(
                200,
                &json!({
                    "machine": parsed.machine,
                    "directory": parsed.directory,
                    "name": saved,
                }),
            ),
            Err(DirectoryNameError::InvalidInput) => RouterResponse::error(
                400,
                "invalid_directory_name",
                "Invalid directory or display name.",
            ),
            Err(DirectoryNameError::MachineDeletionPending) => {
                RouterResponse::error(404, "machine_not_found", "Machine not found.")
            }
            Err(DirectoryNameError::Database) => RouterResponse::error(
                503,
                "directory_name_unavailable",
                "Directory name storage is unavailable.",
            ),
        }
    }

    async fn handle_clear_cache(&self, params: &QueryParams) -> RouterResponse {
        let targets: Vec<String> = match self.select_machines(params).await {
            Ok((ids, _)) => ids,
            Err(r) => return r,
        };
        let mut results = Vec::new();
        let mut ok_count = 0usize;
        let mut err_count = 0usize;
        for id in targets {
            match self.paths.aggregation_cache_file_for_machine(&id) {
                Ok(path) => {
                    let cleared = if path.exists() {
                        fs::remove_file(&path)
                    } else {
                        Ok(())
                    };
                    match cleared {
                        Ok(()) => {
                            ok_count += 1;
                            results.push(json!({ "machine": id, "status": "ok" }));
                        }
                        Err(_) => {
                            err_count += 1;
                            results.push(json!({
                                "machine": id,
                                "status": "failed",
                                "error": "cache_failed",
                            }));
                        }
                    }
                }
                Err(_) => {
                    err_count += 1;
                    results.push(json!({
                        "machine": id,
                        "status": "failed",
                        "error": "cache_failed",
                    }));
                }
            }
        }
        let status = if err_count == 0 {
            200
        } else if ok_count > 0 {
            207
        } else {
            500
        };
        RouterResponse::json(status, &json!({ "results": results }))
    }
}

pub struct QueryParams {
    entries: Vec<(String, String)>,
}

impl QueryParams {
    pub fn get_one(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn get_all(&self, key: &str) -> Vec<&String> {
        self.entries
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v)
            .collect()
    }

    pub fn parse_strict_bool(&self, key: &str) -> Result<Option<bool>, RouterResponse> {
        let vals = self.get_all(key);
        if vals.is_empty() {
            return Ok(None);
        }
        if vals.len() > 1 {
            return Err(RouterResponse::error(
                400,
                "invalid_boolean",
                "Duplicate boolean query parameter.",
            ));
        }
        match vals[0].as_str() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(RouterResponse::error(
                400,
                "invalid_boolean",
                "Boolean query parameter must be true or false.",
            )),
        }
    }
}

fn parse_query(query: &str) -> QueryParams {
    let mut entries = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode_str(k).decode_utf8_lossy().into_owned();
        let val = v.to_owned();
        entries.push((key, val));
    }
    QueryParams { entries }
}

fn resolve_range_bounds(
    params: &QueryParams,
    now: DateTime<Utc>,
    tz: Tz,
) -> Result<(DateTime<Utc>, DateTime<Utc>, DashboardDataDisposition), RouterResponse> {
    let today = now.with_timezone(&tz).date_naive();
    let range = params.get_one("range").unwrap_or("today");
    match range {
        "recent12h" => Ok((
            now - Duration::hours(12),
            now,
            DashboardDataDisposition::Current,
        )),
        "today" => Ok((
            local_midnight_utc(today, tz),
            local_midnight_utc(today.succ_opt().unwrap_or(today), tz),
            DashboardDataDisposition::Current,
        )),
        "yesterday" => {
            let y = today.pred_opt().unwrap_or(today);
            Ok((
                local_midnight_utc(y, tz),
                local_midnight_utc(today, tz),
                DashboardDataDisposition::Historical,
            ))
        }
        "week" => {
            let start = today - Duration::days(6);
            Ok((
                local_midnight_utc(start, tz),
                local_midnight_utc(today.succ_opt().unwrap_or(today), tz),
                DashboardDataDisposition::Current,
            ))
        }
        "month" => {
            let start = today - Duration::days(29);
            Ok((
                local_midnight_utc(start, tz),
                local_midnight_utc(today.succ_opt().unwrap_or(today), tz),
                DashboardDataDisposition::Current,
            ))
        }
        "custom" => {
            let s = params.get_one("start").ok_or_else(|| {
                RouterResponse::error(400, "missing_start", "Custom range requires start.")
            })?;
            let e = params.get_one("end").ok_or_else(|| {
                RouterResponse::error(400, "missing_end", "Custom range requires end.")
            })?;
            let sd = NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map_err(|_| RouterResponse::error(400, "invalid_date", "Invalid start date."))?;
            let ed = NaiveDate::parse_from_str(e, "%Y-%m-%d")
                .map_err(|_| RouterResponse::error(400, "invalid_date", "Invalid end date."))?;
            if sd > ed {
                return Err(RouterResponse::error(
                    400,
                    "invalid_range",
                    "start must be <= end.",
                ));
            }
            let disp = if ed < today {
                DashboardDataDisposition::Historical
            } else {
                DashboardDataDisposition::Current
            };
            Ok((
                local_midnight_utc(sd, tz),
                local_midnight_utc(ed.succ_opt().unwrap_or(ed), tz),
                disp,
            ))
        }
        _ => Err(RouterResponse::error(
            400,
            "invalid_range",
            "Unsupported range value.",
        )),
    }
}

fn is_valid_loopback_host(host: &str, expected_port: Option<u16>) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    match expected_port {
        Some(p) => lower == format!("127.0.0.1:{p}") || lower == format!("localhost:{p}"),
        None => {
            lower == "127.0.0.1"
                || lower == "localhost"
                || lower.starts_with("127.0.0.1:")
                || lower.starts_with("localhost:")
        }
    }
}

fn is_valid_loopback_origin(origin: &str, expected_port: Option<u16>) -> bool {
    let lower = origin.trim().to_ascii_lowercase();
    match expected_port {
        Some(p) => {
            lower == format!("http://127.0.0.1:{p}") || lower == format!("http://localhost:{p}")
        }
        None => lower.starts_with("http://127.0.0.1") || lower.starts_with("http://localhost"),
    }
}

#[allow(dead_code)]
fn _registry_type_check(_: &MachineRegistry) {}
