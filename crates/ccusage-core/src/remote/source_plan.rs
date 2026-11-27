use crate::types::{MachineDescriptor, MachineSessionSourcePlan};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// One result from the remote fixed path probe, including unavailable scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSourceProbe {
    pub scan_scope: PathBuf,
    pub raw_execution_value: Option<String>,
    pub exists: bool,
    pub is_directory: bool,
    pub is_default: bool,
}

fn probe_parts(probes: &[RemoteSourceProbe]) -> (Vec<PathBuf>, Vec<String>) {
    let mut paths = Vec::new();
    let mut states = Vec::new();
    for probe in probes {
        let scope = normalize(&probe.scan_scope);
        let raw = probe.raw_execution_value.as_deref();
        states.push(
            serde_json::to_string(&(
                scope.to_string_lossy(),
                probe.exists,
                probe.is_directory,
                probe.is_default,
                raw,
            ))
            .expect("probe state serialization"),
        );
        if probe.exists && probe.is_directory {
            paths.push(scope);
        }
    }
    (deduplicate(paths), states)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn deduplicate(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut paths: Vec<_> = paths.iter().map(|p| normalize(p)).collect();
    paths.sort_by_key(|p| p.components().count());
    let mut unique: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !unique.iter().any(|parent| path.starts_with(parent)) {
            unique.push(path);
        }
    }
    unique
}

fn roots(
    configured: &[String],
    default: bool,
    env: &HashMap<String, String>,
    key: &str,
    fallback: &str,
    suffix: &str,
) -> (Vec<PathBuf>, Vec<String>) {
    let home = env.get("HOME").map(String::as_str).unwrap_or("");
    let mut candidates = Vec::new();
    if default {
        candidates.push(
            env.get(key)
                .cloned()
                .unwrap_or_else(|| format!("{home}/{fallback}")),
        );
    }
    candidates.extend(configured.iter().map(|root| {
        if root == "~" {
            home.to_owned()
        } else if let Some(rest) = root.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else {
            root.clone()
        }
    }));
    let mut resolved = Vec::new();
    let mut states = Vec::new();
    for root in candidates {
        let root = normalize(&PathBuf::from(root));
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let scope = root.join(suffix);
        let canonical = std::fs::canonicalize(&scope).unwrap_or(scope);
        let metadata = std::fs::metadata(&canonical);
        let exists = metadata.is_ok();
        let directory = metadata.map(|m| m.is_dir()).unwrap_or(false);
        states.push(format!("{}:{exists}:{directory}", canonical.display()));
        if directory {
            resolved.push(canonical);
        }
    }
    (deduplicate(resolved), states)
}

impl MachineSessionSourcePlan {
    pub fn resolve_local(descriptor: &MachineDescriptor, env: &HashMap<String, String>) -> Self {
        let (codex_roots, codex_states) = roots(
            &descriptor.codex_session_dirs,
            descriptor.include_default_codex_dir,
            env,
            "CODEX_HOME",
            ".codex",
            "sessions",
        );
        let (claude_roots, claude_states) = roots(
            &descriptor.claude_config_dirs,
            descriptor.include_default_claude_dir,
            env,
            "CLAUDE_CONFIG_DIR",
            ".claude",
            "projects",
        );
        Self::from_parts(
            descriptor,
            codex_roots,
            claude_roots,
            &codex_states,
            &claude_states,
        )
    }

    pub fn from_remote_probe(
        descriptor: &MachineDescriptor,
        codex_roots: Vec<PathBuf>,
        claude_roots: Vec<PathBuf>,
    ) -> Self {
        let make_probes = |roots: Vec<PathBuf>| {
            roots
                .into_iter()
                .map(|scan_scope| RemoteSourceProbe {
                    scan_scope,
                    raw_execution_value: None,
                    exists: true,
                    is_directory: true,
                    is_default: false,
                })
                .collect::<Vec<_>>()
        };
        Self::from_remote_probe_states(
            descriptor,
            &make_probes(codex_roots),
            &make_probes(claude_roots),
        )
    }

    pub fn from_remote_probe_states(
        descriptor: &MachineDescriptor,
        codex: &[RemoteSourceProbe],
        claude: &[RemoteSourceProbe],
    ) -> Self {
        let (codex_roots, codex_states) = probe_parts(codex);
        let (claude_roots, claude_states) = probe_parts(claude);
        Self::from_parts(
            descriptor,
            codex_roots,
            claude_roots,
            &codex_states,
            &claude_states,
        )
    }

    fn from_parts(
        descriptor: &MachineDescriptor,
        codex_roots: Vec<PathBuf>,
        claude_roots: Vec<PathBuf>,
        codex_states: &[String],
        claude_states: &[String],
    ) -> Self {
        let mut hash = Sha256::new();
        for value in [
            "session-source-v1",
            if descriptor.include_default_codex_dir {
                "1"
            } else {
                "0"
            },
            if descriptor.include_default_claude_dir {
                "1"
            } else {
                "0"
            },
        ] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        let normalized_codex: Vec<String> = descriptor
            .codex_session_dirs
            .iter()
            .map(|v| normalize(Path::new(v)).to_string_lossy().into_owned())
            .collect();
        let normalized_claude: Vec<String> = descriptor
            .claude_config_dirs
            .iter()
            .map(|v| normalize(Path::new(v)).to_string_lossy().into_owned())
            .collect();
        for values in [
            normalized_codex.as_slice(),
            normalized_claude.as_slice(),
            codex_states,
            claude_states,
        ] {
            hash.update((values.len() as u64).to_be_bytes());
            for value in values {
                hash.update((value.len() as u64).to_be_bytes());
                hash.update(value.as_bytes());
            }
        }
        let fingerprint = format!("{:x}", hash.finalize());
        Self {
            machine_id: descriptor.id.clone(),
            codex_roots,
            claude_roots,
            fingerprint,
        }
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
