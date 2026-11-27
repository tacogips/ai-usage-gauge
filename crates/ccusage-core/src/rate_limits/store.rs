use crate::types::RateLimitObservation;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub struct RateLimitObservationFileStore {
    path: PathBuf,
}
impl RateLimitObservationFileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn load(&self) -> io::Result<BTreeMap<String, RateLimitObservation>> {
        match std::fs::read(&self.path) {
            Ok(data) => serde_json::from_slice(&data).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e),
        }
    }
    pub fn save(&self, observations: &BTreeMap<String, RateLimitObservation>) -> io::Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer(&mut temp, observations).map_err(io::Error::other)?;
        temp.flush()?;
        temp.persist(&self.path).map_err(|e| e.error)?;
        Ok(())
    }
}
