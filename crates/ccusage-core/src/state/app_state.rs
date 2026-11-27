use crate::types::{AppState, PieChartId, ResetCycle};
use std::fs;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("corrupt state {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error("state {path}: {message}")]
    Io { path: PathBuf, message: String },
}
pub struct StateStore {
    file_path: PathBuf,
}
impl StateStore {
    pub fn new(file_path: PathBuf) -> Self {
        Self { file_path }
    }
    fn error(&self, e: impl std::fmt::Display) -> StateError {
        StateError::Io {
            path: self.file_path.clone(),
            message: e.to_string(),
        }
    }
    pub fn load(&self, default_cycle: ResetCycle) -> Result<AppState, StateError> {
        match fs::read(&self.file_path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e: serde_json::Error| StateError::Corrupt {
                    path: self.file_path.clone(),
                    message: e.to_string(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppState {
                budget_usd: None,
                budget_machine_ids: vec![],
                reset_cycle: default_cycle,
                baseline: None,
                refresh_interval_seconds: None,
                menu_bar_pie_charts: vec![PieChartId::Budget],
                menu_bar_ratio_labels: vec![PieChartId::Budget],
                menu_bar_show_cost: true,
                menu_bar_cost_only: false,
            }),
            Err(e) => Err(self.error(e)),
        }
    }
    pub fn save(&self, state: &AppState) -> Result<(), StateError> {
        if state.budget_usd.is_some_and(|v| v.is_sign_negative())
            || state.refresh_interval_seconds == Some(0)
            || state
                .budget_machine_ids
                .iter()
                .any(|v| v.is_empty() || v.len() > 100)
            || state
                .budget_machine_ids
                .iter()
                .enumerate()
                .any(|(i, v)| state.budget_machine_ids[..i].contains(v))
        {
            return Err(self.error("invalid state"));
        }
        let parent = self.file_path.parent().unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| self.error(e))?;
        let mut temp = tempfile::Builder::new()
            .prefix(".state-")
            .tempfile_in(parent)
            .map_err(|e| self.error(e))?;
        serde_json::to_writer_pretty(&mut temp, state).map_err(|e| self.error(e))?;
        temp.write_all(b"\n").map_err(|e| self.error(e))?;
        temp.persist(&self.file_path)
            .map_err(|e| self.error(e.error))?;
        Ok(())
    }
}
