use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use ccusage_core::dashboard::{MachineDashboardRouter, RouterRequest};

pub const MAX_IPC_BODY_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardIpcRequest {
    pub path: String,
    #[serde(default = "default_get")]
    pub method: String,
    #[serde(default)]
    pub body: Option<String>,
}

fn default_get() -> String {
    "GET".to_owned()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DashboardIpcResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

pub fn validate_ipc_request(
    req: &DashboardIpcRequest,
) -> Result<(String, String, String, Vec<u8>), String> {
    if !req.path.starts_with("/api/")
        || req.path.contains('\r')
        || req.path.contains('\n')
        || req.path.contains('#')
    {
        return Err("Invalid API path".into());
    }
    let method = req.method.trim().to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
        return Err(format!("Unsupported HTTP method: {method}"));
    }
    let body_bytes = req
        .body
        .as_deref()
        .map(|s| s.as_bytes().to_vec())
        .unwrap_or_default();
    if body_bytes.len() > MAX_IPC_BODY_BYTES {
        return Err("Request body exceeds 1 MiB limit".into());
    }
    let (path, query) = req
        .path
        .split_once('?')
        .map(|(p, q)| (p.to_owned(), q.to_owned()))
        .unwrap_or_else(|| (req.path.clone(), String::new()));
    Ok((method, path, query, body_bytes))
}

pub async fn dispatch_ipc_request(
    router: &MachineDashboardRouter,
    req: DashboardIpcRequest,
) -> Result<DashboardIpcResponse, String> {
    let (method, path, query, body) = validate_ipc_request(&req)?;
    let resp = router
        .route(RouterRequest {
            method,
            path,
            query,
            headers: HashMap::new(),
            body,
            is_trusted_ipc: true,
            expected_port: None,
        })
        .await;
    Ok(DashboardIpcResponse {
        status: resp.status,
        content_type: resp.content_type.to_owned(),
        body: String::from_utf8_lossy(&resp.body).into_owned(),
    })
}
