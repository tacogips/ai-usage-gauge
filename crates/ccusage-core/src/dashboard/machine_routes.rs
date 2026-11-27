use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::remote::registry::MachineValidation;
use crate::types::{DirectoryNameLifecycle, MachineDescriptor, MachineKind, MachineRuntimeReconciler};

use super::router::{MachineDashboardRouter, RouterResponse};

impl MachineDashboardRouter {
    pub(super) async fn handle_list_machines(&self) -> RouterResponse {
        match self.registry_store.load() {
            Ok(reg) => RouterResponse::json(200, &reg),
            Err(e) => RouterResponse::error(500, "registry_error", &e.to_string()),
        }
    }

    pub(super) async fn handle_add_machine(&self, body: &[u8]) -> RouterResponse {
        let desc: MachineDescriptor = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => {
                return RouterResponse::error(400, "invalid_machine", "Invalid machine descriptor.")
            }
        };
        if let Err(e) = MachineValidation::validate(&desc) {
            return RouterResponse::error(400, "invalid_machine", &e.to_string());
        }
        let mut reg = match self.registry_store.load() {
            Ok(r) => r,
            Err(e) => return RouterResponse::error(500, "registry_error", &e.to_string()),
        };
        if reg.machines.iter().any(|m| m.id == desc.id) {
            return RouterResponse::error(409, "machine_exists", "Machine ID already exists.");
        }
        reg.machines.push(desc.clone());
        if let Err(e) = self.registry_store.save(&reg) {
            return RouterResponse::error(400, "invalid_machine", &e.to_string());
        }
        let rev = {
            let mut g = self.registry_revision.write().await;
            *g += 1;
            *g
        };
        let _ = self.collector.reconcile_registry(&reg, rev).await;
        RouterResponse::json(201, &desc)
    }

    pub(super) async fn handle_machine_item(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        now: DateTime<Utc>,
    ) -> RouterResponse {
        let rest = path.trim_start_matches("/api/machines/");
        let mut parts = rest.split('/');
        let Some(id) = parts.next().filter(|s| !s.is_empty()) else {
            return RouterResponse::error(404, "not_found", "Machine ID missing.");
        };
        let action = parts.next();

        match (method, action) {
            ("GET", None) => {
                if id == "local" {
                    let reg = match self.registry_store.load() {
                        Ok(r) => r,
                        Err(e) => {
                            return RouterResponse::error(500, "registry_error", &e.to_string())
                        }
                    };
                    return RouterResponse::json(
                        200,
                        &json!({
                            "id": "local",
                            "displayName": "Local",
                            "kind": MachineKind::Local,
                            "enabled": true,
                            "localSessionSources": reg.local_session_sources,
                        }),
                    );
                }
                let reg = match self.registry_store.load() {
                    Ok(r) => r,
                    Err(e) => return RouterResponse::error(500, "registry_error", &e.to_string()),
                };
                match reg.machines.into_iter().find(|m| m.id == id) {
                    Some(m) => RouterResponse::json(200, &m),
                    None => RouterResponse::error(404, "machine_not_found", "Machine not found."),
                }
            }
            ("PATCH" | "PUT", None) => {
                let patch: Value = match serde_json::from_slice(body) {
                    Ok(v) => v,
                    Err(_) => {
                        return RouterResponse::error(400, "invalid_payload", "Invalid JSON body.")
                    }
                };
                let mut reg = match self.registry_store.load() {
                    Ok(r) => r,
                    Err(e) => return RouterResponse::error(500, "registry_error", &e.to_string()),
                };
                if id == "local" {
                    if let Some(dirs) = patch.get("codexSessionDirs").and_then(Value::as_array) {
                        reg.local_session_sources.codex_session_dirs = dirs
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect();
                    }
                    if let Some(dirs) = patch.get("claudeConfigDirs").and_then(Value::as_array) {
                        reg.local_session_sources.claude_config_dirs = dirs
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect();
                    }
                    if let Some(b) = patch.get("includeDefaultCodexDir").and_then(Value::as_bool) {
                        reg.local_session_sources.include_default_codex_dir = b;
                    }
                    if let Some(b) = patch
                        .get("includeDefaultClaudeDir")
                        .and_then(Value::as_bool)
                    {
                        reg.local_session_sources.include_default_claude_dir = b;
                    }
                    if let Err(e) = self.registry_store.save(&reg) {
                        return RouterResponse::error(400, "invalid_machine", &e.to_string());
                    }
                    return RouterResponse::json(200, &reg.local_session_sources);
                }
                let Some(existing) = reg.machines.iter_mut().find(|m| m.id == id) else {
                    return RouterResponse::error(404, "machine_not_found", "Machine not found.");
                };
                if let Some(name) = patch.get("displayName").and_then(Value::as_str) {
                    existing.display_name = name.to_owned();
                }
                if let Some(enabled) = patch.get("enabled").and_then(Value::as_bool) {
                    existing.enabled = enabled;
                }
                let updated = existing.clone();
                if let Err(e) = self.registry_store.save(&reg) {
                    return RouterResponse::error(400, "invalid_machine", &e.to_string());
                }
                let rev = {
                    let mut g = self.registry_revision.write().await;
                    *g += 1;
                    *g
                };
                let _ = self.collector.reconcile_registry(&reg, rev).await;
                RouterResponse::json(200, &updated)
            }
            ("DELETE", None) => {
                if id == "local" {
                    return RouterResponse::error(
                        400,
                        "cannot_delete_local",
                        "Local machine cannot be deleted.",
                    );
                }
                let mut reg = match self.registry_store.load() {
                    Ok(r) => r,
                    Err(e) => return RouterResponse::error(500, "registry_error", &e.to_string()),
                };
                let before = reg.machines.len();
                reg.machines.retain(|m| m.id != id);
                if reg.machines.len() == before {
                    return RouterResponse::error(404, "machine_not_found", "Machine not found.");
                }
                let _ = self.directory_name_store.prepare_delete(id);
                if let Err(e) = self.registry_store.save(&reg) {
                    let _ = self.directory_name_store.rollback_delete(id);
                    return RouterResponse::error(500, "registry_error", &e.to_string());
                }
                let _ = self.directory_name_store.commit_delete(id);
                let _ = self.directory_name_store.purge_deleted(id);
                let rev = {
                    let mut g = self.registry_revision.write().await;
                    *g += 1;
                    *g
                };
                let _ = self.collector.reconcile_registry(&reg, rev).await;
                RouterResponse::json(200, &json!({ "status": "ok", "id": id }))
            }
            ("POST", Some("refresh")) => {
                let statuses = self.collector.store.statuses().await;
                if !statuses.iter().any(|s| s.id == id) {
                    return RouterResponse::error(404, "machine_not_found", "Machine not found.");
                }
                let ok = self.collector.refresh_machine(id, None, now).await;
                if ok {
                    RouterResponse::json(200, &json!({ "status": "ok", "machine": id }))
                } else {
                    RouterResponse::json(200, &json!({ "status": "failed", "machine": id }))
                }
            }
            ("POST", Some("test-connection")) => {
                let statuses = self.collector.store.statuses().await;
                if !statuses.iter().any(|s| s.id == id) {
                    return RouterResponse::error(404, "machine_not_found", "Machine not found.");
                }
                let ok = self.collector.refresh_machine(id, None, now).await;
                RouterResponse::json(
                    200,
                    &json!({
                        "status": if ok { "ok" } else { "failed" },
                        "machine": id,
                    }),
                )
            }
            _ => RouterResponse::error(404, "not_found", "Machine route not found."),
        }
    }
}
