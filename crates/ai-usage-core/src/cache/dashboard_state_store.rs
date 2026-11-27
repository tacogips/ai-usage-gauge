use crate::types::DashboardUIState;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum DashboardStateError {
    #[error("dashboard database unavailable")]
    Database(#[from] rusqlite::Error),
    #[error("dashboard storage unavailable")]
    Io(#[from] std::io::Error),
    #[error("invalid dashboard state")]
    InvalidState,
}

pub struct DashboardStateStore {
    pub file_path: PathBuf,
}
impl DashboardStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: path.into(),
        }
    }
    pub fn load(&self) -> Result<Option<DashboardUIState>, DashboardStateError> {
        if !self.file_path.exists() {
            return Ok(None);
        }
        let db = open_database(&self.file_path)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS dashboard_state (id INTEGER PRIMARY KEY CHECK (id = 1), value BLOB NOT NULL, updated_at REAL NOT NULL)")?;
        let bytes: Option<Vec<u8>> = db
            .query_row("SELECT value FROM dashboard_state WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        bytes
            .map(|v| {
                let state: DashboardUIState =
                    serde_json::from_slice(&v).map_err(|_| DashboardStateError::InvalidState)?;
                validate(&state)?;
                Ok(state)
            })
            .transpose()
    }
    pub fn save(&self, state: &DashboardUIState, now: f64) -> Result<(), DashboardStateError> {
        validate(state)?;
        let data = serde_json::to_vec(state).map_err(|_| DashboardStateError::InvalidState)?;
        let db = open_database(&self.file_path)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS dashboard_state (id INTEGER PRIMARY KEY CHECK (id = 1), value BLOB NOT NULL, updated_at REAL NOT NULL)")?;
        db.execute("INSERT INTO dashboard_state(id,value,updated_at) VALUES (1,?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at", params![data, now])?;
        Ok(())
    }
}
fn validate(s: &DashboardUIState) -> Result<(), DashboardStateError> {
    let day = |v: &str| v.len() == 10 && chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok();
    let strings = |v: &[String], max, bytes| {
        v.len() <= max && v.iter().all(|s| !s.is_empty() && s.len() <= bytes)
    };
    if !["recent12h", "today", "yesterday", "week", "month", "custom"].contains(&s.range.as_str())
        || !["15min", "hourly", "6hour", "daily"].contains(&s.granularity.as_str())
        || ![
            "costUSD",
            "totalTokens",
            "inputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheCreationTokens",
        ]
        .contains(&s.chart_metric.as_str())
        || !["model", "machine", "subdirectory", "modelEffort"].contains(&s.stack_by.as_str())
        || !day(&s.custom_start)
        || !day(&s.custom_end)
        || s.custom_start > s.custom_end
        || !strings(&s.selected_models, 500, 500)
        || !strings(&s.selected_agents, 50, 100)
        || !strings(&s.selected_machines, 500, 100)
        || s.pie_charts.is_empty()
        || s.pie_charts.len() > crate::types::PieChartId::ALL.len()
        || s.hidden_pie_ratios.len() > crate::types::PieChartId::ALL.len()
    {
        return Err(DashboardStateError::InvalidState);
    }
    Ok(())
}
pub(crate) fn open_database(path: &Path) -> Result<Connection, DashboardStateError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let db = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(db)
}
