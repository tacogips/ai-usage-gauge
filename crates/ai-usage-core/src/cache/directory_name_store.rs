use super::dashboard_state_store::open_database;
use crate::types::DirectoryNameLifecycle;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum DirectoryNameError {
    #[error("invalid input")]
    InvalidInput,
    #[error("machine_not_found")]
    MachineDeletionPending,
    #[error("database unavailable")]
    Database,
}
pub struct DashboardDirectoryNameStore {
    pub file_path: PathBuf,
}
impl DashboardDirectoryNameStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: path.into(),
        }
    }
    fn open(&self) -> Result<Connection, DirectoryNameError> {
        let db = open_database(&self.file_path).map_err(|_| DirectoryNameError::Database)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS dashboard_directory_names (machine_id TEXT NOT NULL, directory TEXT NOT NULL, display_name TEXT NOT NULL, updated_at REAL NOT NULL, PRIMARY KEY(machine_id,directory)); CREATE TABLE IF NOT EXISTS dashboard_directory_name_machine_deletions (machine_id TEXT PRIMARY KEY, deleted_at REAL NOT NULL, phase TEXT NOT NULL DEFAULT 'prepared');").map_err(|_| DirectoryNameError::Database)?;
        let has_phase = {
            let mut stmt = db
                .prepare("PRAGMA table_info(dashboard_directory_name_machine_deletions)")
                .map_err(|_| DirectoryNameError::Database)?;
            let cols = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(|_| DirectoryNameError::Database)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| DirectoryNameError::Database)?;
            cols.iter().any(|s| s == "phase")
        };
        if !has_phase {
            db.execute_batch("ALTER TABLE dashboard_directory_name_machine_deletions ADD COLUMN phase TEXT NOT NULL DEFAULT 'prepared'").map_err(|_| DirectoryNameError::Database)?;
        }
        Ok(db)
    }
    pub fn set_name(
        &self,
        machine: &str,
        directory: &str,
        name: Option<&str>,
        now: f64,
    ) -> Result<Option<String>, DirectoryNameError> {
        validate_machine(machine)?;
        if directory.is_empty() {
            return Err(DirectoryNameError::InvalidInput);
        }
        let name = normalize_name(name)?;
        let mut db = self.open()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| DirectoryNameError::Database)?;
        let marker: Option<String> = tx
            .query_row(
                "SELECT phase FROM dashboard_directory_name_machine_deletions WHERE machine_id=?1",
                [machine],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| DirectoryNameError::Database)?;
        if marker.is_some() {
            return Err(DirectoryNameError::MachineDeletionPending);
        }
        if let Some(ref n) = name {
            tx.execute("INSERT INTO dashboard_directory_names(machine_id,directory,display_name,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(machine_id,directory) DO UPDATE SET display_name=excluded.display_name,updated_at=excluded.updated_at", params![machine,directory,n,now]).map_err(|_| DirectoryNameError::Database)?;
        } else {
            tx.execute(
                "DELETE FROM dashboard_directory_names WHERE machine_id=?1 AND directory=?2",
                params![machine, directory],
            )
            .map_err(|_| DirectoryNameError::Database)?;
        }
        tx.commit().map_err(|_| DirectoryNameError::Database)?;
        Ok(name)
    }
    pub fn names(
        &self,
        machines: &[String],
    ) -> Result<HashMap<String, HashMap<String, String>>, DirectoryNameError> {
        for id in machines {
            validate_machine(id)?;
        }
        if machines.is_empty() {
            return Ok(HashMap::new());
        }
        let db = self.open()?;
        let mut out = HashMap::new();
        for machine in machines {
            let mut stmt = db.prepare("SELECT directory,display_name FROM dashboard_directory_names WHERE machine_id=?1 AND NOT EXISTS (SELECT 1 FROM dashboard_directory_name_machine_deletions WHERE machine_id=?1) ORDER BY directory").map_err(|_| DirectoryNameError::Database)?;
            let rows = stmt
                .query_map([machine], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|_| DirectoryNameError::Database)?;
            for row in rows {
                let (directory, name) = row.map_err(|_| DirectoryNameError::Database)?;
                out.entry(machine.clone())
                    .or_insert_with(HashMap::new)
                    .insert(directory, name);
            }
        }
        Ok(out)
    }
    fn marker(&self, machine: &str, phase: &str) -> Result<(), String> {
        validate_machine(machine).map_err(|e| e.to_string())?;
        let db = self.open().map_err(|e| e.to_string())?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        db.execute("INSERT INTO dashboard_directory_name_machine_deletions(machine_id,deleted_at,phase) VALUES (?1,?2,?3) ON CONFLICT(machine_id) DO UPDATE SET deleted_at=excluded.deleted_at,phase=excluded.phase", params![machine,now,phase]).map_err(|_| "database unavailable".to_owned())?;
        Ok(())
    }
    pub fn cancel_delete(&self, machine: &str) -> Result<(), String> {
        validate_machine(machine).map_err(|e| e.to_string())?;
        let db = self.open().map_err(|e| e.to_string())?;
        db.execute(
            "DELETE FROM dashboard_directory_name_machine_deletions WHERE machine_id=?1",
            [machine],
        )
        .map_err(|_| "database unavailable".to_owned())?;
        Ok(())
    }
}
fn validate_machine(id: &str) -> Result<(), DirectoryNameError> {
    if id.is_empty()
        || id.len() > 63
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || id.starts_with('-')
        || id.ends_with('-')
    {
        return Err(DirectoryNameError::InvalidInput);
    }
    Ok(())
}
pub fn normalize_name(name: Option<&str>) -> Result<Option<String>, DirectoryNameError> {
    let Some(name) = name else { return Ok(None) };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if name.chars().any(char::is_control) || trimmed.chars().count() > 200 {
        return Err(DirectoryNameError::InvalidInput);
    }
    Ok(Some(trimmed.to_owned()))
}
impl DirectoryNameLifecycle for DashboardDirectoryNameStore {
    fn prepare_delete(&self, id: &str) -> Result<(), String> {
        self.marker(id, "prepared")
    }
    fn commit_delete(&self, id: &str) -> Result<(), String> {
        self.marker(id, "committed")
    }
    fn rollback_delete(&self, id: &str) -> Result<(), String> {
        self.marker(id, "rollback")
    }
    fn purge_deleted(&self, id: &str) -> Result<(), String> {
        validate_machine(id).map_err(|e| e.to_string())?;
        let db = self.open().map_err(|e| e.to_string())?;
        db.execute(
            "DELETE FROM dashboard_directory_names WHERE machine_id=?1",
            [id],
        )
        .map_err(|_| "database unavailable".to_owned())?;
        Ok(())
    }
    fn purge_for_reuse(&self, id: &str) -> Result<(), String> {
        validate_machine(id).map_err(|e| e.to_string())?;
        let mut db = self.open().map_err(|e| e.to_string())?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "database unavailable".to_owned())?;
        tx.execute(
            "DELETE FROM dashboard_directory_names WHERE machine_id=?1",
            [id],
        )
        .map_err(|_| "database unavailable".to_owned())?;
        tx.execute(
            "DELETE FROM dashboard_directory_name_machine_deletions WHERE machine_id=?1",
            [id],
        )
        .map_err(|_| "database unavailable".to_owned())?;
        tx.commit().map_err(|_| "database unavailable".to_owned())
    }
    fn reconcile_machine_registry(&self, ids: &[String]) -> Result<(), String> {
        for id in ids {
            validate_machine(id).map_err(|e| e.to_string())?;
        }
        let active: HashSet<_> = ids.iter().collect();
        let mut db = self.open().map_err(|e| e.to_string())?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| "database unavailable".to_owned())?;
        let metadata = {
            let mut stmt = tx.prepare("SELECT machine_id FROM dashboard_directory_names UNION SELECT machine_id FROM dashboard_directory_name_machine_deletions").map_err(|_| "database unavailable".to_owned())?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|_| "database unavailable".to_owned())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| "database unavailable".to_owned())?
        };
        for id in metadata {
            let phase: Option<String> = tx.query_row("SELECT phase FROM dashboard_directory_name_machine_deletions WHERE machine_id=?1", [&id], |r| r.get(0)).optional().map_err(|_| "database unavailable".to_owned())?;
            if !active.contains(&id) || phase.as_deref() == Some("committed") {
                tx.execute(
                    "DELETE FROM dashboard_directory_names WHERE machine_id=?1",
                    [&id],
                )
                .map_err(|_| "database unavailable".to_owned())?;
            }
            if active.contains(&id) {
                tx.execute(
                    "DELETE FROM dashboard_directory_name_machine_deletions WHERE machine_id=?1",
                    [&id],
                )
                .map_err(|_| "database unavailable".to_owned())?;
            } else {
                tx.execute("INSERT INTO dashboard_directory_name_machine_deletions(machine_id,deleted_at,phase) VALUES (?1,0,'committed') ON CONFLICT(machine_id) DO UPDATE SET phase='committed'", [&id]).map_err(|_| "database unavailable".to_owned())?;
            }
        }
        tx.commit().map_err(|_| "database unavailable".to_owned())
    }
}
