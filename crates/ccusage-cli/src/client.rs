use serde_json::{json, Value};

use ccusage_core::config::ConfigStore;
use ccusage_core::dashboard::DashboardAPIClient;
use ccusage_core::paths::AppPaths;

use crate::cli::{ClientCommands, DashboardActions, MachineActions};
use crate::rendering::render_value;

pub async fn run_client(
    paths: &AppPaths,
    port_override: Option<u16>,
    json_out: bool,
    cmd: ClientCommands,
) -> Result<(), i32> {
    let port = match port_override {
        Some(p) => p,
        None => ConfigStore::new(paths.config_file.clone())
            .load_or_create()
            .map(|c| c.dashboard_port)
            .unwrap_or(18081),
    };
    let client = DashboardAPIClient::new(port).map_err(|e| {
        eprintln!("error: {e}");
        1
    })?;

    match cmd {
        ClientCommands::Dashboard { action } => {
            run_dashboard_action(&client, action, json_out).await
        }
        ClientCommands::Machines { action } => run_machine_action(&client, action, json_out).await,
    }
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn encode_query_list(pairs: &[(&str, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect();
    format!("?{}", parts.join("&"))
}

async fn run_dashboard_action(
    client: &DashboardAPIClient,
    action: DashboardActions,
    json_out: bool,
) -> Result<(), i32> {
    let result: Result<Value, ccusage_core::dashboard::ApiClientError> = match action {
        DashboardActions::Health => client.get_json("/api/health").await,
        DashboardActions::MachineStatus => client.get_json("/api/machine-status").await,
        DashboardActions::LoadStatus => client.get_json("/api/load-status").await,
        DashboardActions::Refresh => client.get_json("/api/refresh").await,
        DashboardActions::Budget { machine, directory } => {
            let mut q = Vec::new();
            for m in machine {
                q.push(("machine", m));
            }
            for d in directory {
                q.push(("directory", d));
            }
            client
                .get_json(&format!("/api/budget{}", encode_query_list(&q)))
                .await
        }
        DashboardActions::Recent { hours, machine } => {
            let mut q = vec![("hours", hours.to_string())];
            for m in machine {
                q.push(("machine", m));
            }
            client
                .get_json(&format!("/api/recent{}", encode_query_list(&q)))
                .await
        }
        DashboardActions::Day { date, machine } => {
            let mut q = Vec::new();
            if let Some(d) = date {
                q.push(("date", d));
            }
            for m in machine {
                q.push(("machine", m));
            }
            client
                .get_json(&format!("/api/day{}", encode_query_list(&q)))
                .await
        }
        DashboardActions::Period {
            range,
            start,
            end,
            machine,
        } => {
            let mut q = vec![("range", range)];
            if let Some(s) = start {
                q.push(("start", s));
            }
            if let Some(e) = end {
                q.push(("end", e));
            }
            for m in machine {
                q.push(("machine", m));
            }
            client
                .get_json(&format!("/api/period{}", encode_query_list(&q)))
                .await
        }
        DashboardActions::Metrics {
            range,
            start,
            end,
            machine,
            directory,
        } => {
            let mut q = vec![("range", range)];
            if let Some(s) = start {
                q.push(("start", s));
            }
            if let Some(e) = end {
                q.push(("end", e));
            }
            for m in machine {
                q.push(("machine", m));
            }
            for d in directory {
                q.push(("directory", d));
            }
            client
                .get_json(&format!("/api/metrics{}", encode_query_list(&q)))
                .await
        }
        DashboardActions::CostSeries {
            range,
            granularity,
            start,
            end,
            machine,
            directory,
            directory_breakdown,
            effort_breakdown,
        } => {
            let mut q = vec![("range", range), ("granularity", granularity)];
            if let Some(s) = start {
                q.push(("start", s));
            }
            if let Some(e) = end {
                q.push(("end", e));
            }
            if directory_breakdown {
                q.push(("directoryBreakdown", "true".into()));
            }
            if effort_breakdown {
                q.push(("effortBreakdown", "true".into()));
            }
            for m in machine {
                q.push(("machine", m));
            }
            for d in directory {
                q.push(("directory", d));
            }
            client
                .get_json(&format!("/api/cost-series{}", encode_query_list(&q)))
                .await
        }
        DashboardActions::CacheClear { machine } => {
            let mut q = Vec::new();
            for m in machine {
                q.push(("machine", m));
            }
            client
                .mutate_json(
                    "DELETE",
                    &format!("/api/cache{}", encode_query_list(&q)),
                    None,
                )
                .await
                .map(|(_, v)| v)
        }
    };

    match result {
        Ok(val) => {
            println!("{}", render_value(&val, json_out));
            Ok(())
        }
        Err(e) => {
            eprintln!("{e}");
            Err(e.exit_code())
        }
    }
}

async fn run_machine_action(
    client: &DashboardAPIClient,
    action: MachineActions,
    json_out: bool,
) -> Result<(), i32> {
    let result: Result<Value, ccusage_core::dashboard::ApiClientError> = match action {
        MachineActions::List => client.get_json("/api/machines").await,
        MachineActions::Show { id } => client.get_json(&format!("/api/machines/{id}")).await,
        MachineActions::Add(args) => {
            let payload = json!({
                "id": args.id,
                "displayName": args.display_name,
                "kind": "ssh",
                "enabled": true,
                "ssh": {
                    "host": args.host,
                    "port": args.ssh_port,
                    "user": args.user,
                    "identityFile": args.identity_file,
                    "extraOptions": [],
                    "remoteCcusagePath": args.remote_ccusage_path,
                },
                "codexSessionDirs": args.codex_session_dir,
                "claudeConfigDirs": args.claude_config_dir,
                "includeDefaultCodexDir": !args.exclude_default_codex_dir,
                "includeDefaultClaudeDir": !args.exclude_default_claude_dir,
            });
            client
                .mutate_json("POST", "/api/machines", Some(&payload))
                .await
                .map(|(_, v)| v)
        }
        MachineActions::Update(args) => {
            let mut map = serde_json::Map::new();
            if let Some(name) = args.display_name {
                map.insert("displayName".into(), Value::String(name));
            }
            if args.clear_codex_session_dirs {
                map.insert("codexSessionDirs".into(), json!([]));
            } else if !args.codex_session_dir.is_empty() {
                map.insert("codexSessionDirs".into(), json!(args.codex_session_dir));
            }
            if args.clear_claude_config_dirs {
                map.insert("claudeConfigDirs".into(), json!([]));
            } else if !args.claude_config_dir.is_empty() {
                map.insert("claudeConfigDirs".into(), json!(args.claude_config_dir));
            }
            if args.include_default_codex_dir {
                map.insert("includeDefaultCodexDir".into(), json!(true));
            } else if args.exclude_default_codex_dir {
                map.insert("includeDefaultCodexDir".into(), json!(false));
            }
            if args.include_default_claude_dir {
                map.insert("includeDefaultClaudeDir".into(), json!(true));
            } else if args.exclude_default_claude_dir {
                map.insert("includeDefaultClaudeDir".into(), json!(false));
            }
            client
                .mutate_json(
                    "PATCH",
                    &format!("/api/machines/{}", args.id),
                    Some(&Value::Object(map)),
                )
                .await
                .map(|(_, v)| v)
        }
        MachineActions::Enable { id } => client
            .mutate_json(
                "PATCH",
                &format!("/api/machines/{id}"),
                Some(&json!({ "enabled": true })),
            )
            .await
            .map(|(_, v)| v),
        MachineActions::Disable { id } => client
            .mutate_json(
                "PATCH",
                &format!("/api/machines/{id}"),
                Some(&json!({ "enabled": false })),
            )
            .await
            .map(|(_, v)| v),
        MachineActions::Remove { id } => client
            .mutate_json("DELETE", &format!("/api/machines/{id}"), None)
            .await
            .map(|(_, v)| v),
        MachineActions::TestConnection { id } => client
            .mutate_json("POST", &format!("/api/machines/{id}/test-connection"), None)
            .await
            .map(|(_, v)| v),
        MachineActions::Refresh { id } => client
            .mutate_json("POST", &format!("/api/machines/{id}/refresh"), None)
            .await
            .map(|(_, v)| v),
    };

    match result {
        Ok(val) => {
            if val.get("status").and_then(Value::as_str) == Some("failed") {
                eprintln!("{}", render_value(&val, json_out));
                return Err(4);
            }
            println!("{}", render_value(&val, json_out));
            Ok(())
        }
        Err(e) => {
            eprintln!("{e}");
            Err(e.exit_code())
        }
    }
}
