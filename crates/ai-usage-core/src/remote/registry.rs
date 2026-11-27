use crate::types::{
    LocalSessionSources, MachineDescriptor, MachineKind, MachineRegistry, SSHConnection, SSHProxy,
};
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("registry error: {0}")]
pub struct RegistryError(pub String);
fn err(message: impl Into<String>) -> RegistryError {
    RegistryError(message.into())
}

struct StrictValue;
impl<'de> DeserializeSeed<'de> for StrictValue {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(StrictVisitor)
    }
}
struct StrictVisitor;
impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value without duplicate keys")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        StrictValue.deserialize(d)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(value) = seq.next_element_seed(StrictValue)? {
            out.push(value);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(A::Error::custom("duplicate key"));
            }
            out.insert(key, map.next_value_seed(StrictValue)?);
        }
        Ok(Value::Object(out))
    }
}
fn keys(value: &Value, required: &[&str], allowed: &[&str]) -> Result<(), RegistryError> {
    let obj = value.as_object().ok_or_else(|| err("expected object"))?;
    if required.iter().any(|key| !obj.contains_key(*key))
        || obj.keys().any(|key| !allowed.contains(&key.as_str()))
    {
        return Err(err("invalid object fields"));
    }
    Ok(())
}
fn default_sources() -> LocalSessionSources {
    LocalSessionSources {
        codex_session_dirs: vec![],
        claude_config_dirs: vec![],
        include_default_codex_dir: true,
        include_default_claude_dir: true,
    }
}
fn shape(value: &Value) -> Result<u64, RegistryError> {
    let version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .filter(|v| (1..=3).contains(v))
        .ok_or_else(|| err("invalid schema version"))?;
    let top = if version == 3 {
        vec!["schemaVersion", "localSessionSources", "machines"]
    } else {
        vec!["schemaVersion", "machines"]
    };
    keys(value, &top, &top)?;
    let sources = [
        "codexSessionDirs",
        "claudeConfigDirs",
        "includeDefaultCodexDir",
        "includeDefaultClaudeDir",
    ];
    if version == 3 {
        keys(&value["localSessionSources"], &sources, &sources)?;
    }
    for machine in value["machines"]
        .as_array()
        .ok_or_else(|| err("invalid machines"))?
    {
        let mut fields = vec!["id", "displayName", "kind", "enabled", "ssh"];
        if version == 3 {
            fields.extend(sources);
        }
        keys(machine, &fields, &fields)?;
        let ssh = &machine["ssh"];
        let required = ["host", "port", "user", "extraOptions", "remoteCcusagePath"];
        let mut allowed = required.to_vec();
        allowed.push("identityFile");
        if version >= 2 {
            allowed.push("proxy");
        }
        keys(ssh, &required, &allowed)?;
        if ssh.get("identityFile").is_some_and(Value::is_null) {
            return Err(err("null identity"));
        }
        if let Some(proxy) = ssh.get("proxy") {
            let (required, allowed): (&[&str], &[&str]) =
                match proxy.get("kind").and_then(Value::as_str) {
                    Some("direct") => (&["kind"], &["kind"]),
                    Some("command") => (&["kind", "executable"], &["kind", "executable"]),
                    Some("jump") => (
                        &["kind", "host", "port", "user"],
                        &[
                            "kind",
                            "host",
                            "port",
                            "user",
                            "identityFile",
                            "knownHostsFile",
                        ],
                    ),
                    _ => return Err(err("invalid proxy")),
                };
            keys(proxy, required, allowed)?;
            if ["identityFile", "knownHostsFile"]
                .iter()
                .any(|key| proxy.get(*key).is_some_and(Value::is_null))
            {
                return Err(err("null proxy path"));
            }
        }
    }
    Ok(version)
}

pub struct MachineValidation;
impl MachineValidation {
    pub fn is_valid_session_source_path(path: &str) -> bool {
        !path.is_empty()
            && path.len() <= 4096
            && (path == "~" || path.starts_with("~/") || path.starts_with('/'))
            && !path.bytes().any(|b| b < 32 || b == 127)
    }
    pub fn validate(descriptor: &MachineDescriptor) -> Result<(), RegistryError> {
        let id = descriptor.id.as_bytes();
        if descriptor.kind != MachineKind::Ssh
            || id.is_empty()
            || id.len() > 63
            || descriptor.id == "local"
            || descriptor.id == "all"
            || !id[0].is_ascii_alphanumeric()
            || !id[id.len() - 1].is_ascii_alphanumeric()
            || !id
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        {
            return Err(err("invalid id or kind"));
        }
        if descriptor.display_name.is_empty()
            || descriptor.display_name.chars().count() > 80
            || descriptor.display_name.len() > 256
            || descriptor
                .display_name
                .chars()
                .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
        {
            return Err(err("invalid displayName"));
        }
        for (field, paths) in [
            ("codexSessionDirs", &descriptor.codex_session_dirs),
            ("claudeConfigDirs", &descriptor.claude_config_dirs),
        ] {
            for (i, path) in paths.iter().enumerate() {
                if !Self::is_valid_session_source_path(path) {
                    return Err(err(format!("{field}[{i}]")));
                }
            }
        }
        Self::validate_connection(descriptor.ssh.as_ref().ok_or_else(|| err("ssh required"))?)
    }
    pub fn validate_connection(ssh: &SSHConnection) -> Result<(), RegistryError> {
        validate_host_user(&ssh.host, &ssh.user, ssh.port)?;
        if !valid_executable(&ssh.remote_ccusage_path) {
            return Err(err("ssh.remoteCcusagePath"));
        }
        if ssh.identity_file.as_ref().is_some_and(|p| !absolute(p)) {
            return Err(err("ssh.identityFile"));
        }
        let mut seen = HashSet::new();
        let mut family = false;
        for option in &ssh.extra_options {
            if !seen.insert(option) {
                return Err(err("ssh.extraOptions"));
            }
            if option == "-4" || option == "-6" {
                if family {
                    return Err(err("ssh.extraOptions"));
                }
                family = true;
                continue;
            }
            let (key, value) = option
                .strip_prefix("-o ")
                .and_then(|o| o.split_once('='))
                .ok_or_else(|| err("ssh.extraOptions"))?;
            let valid = match key {
                "ConnectTimeout" => bounded(value, 1, 600),
                "ConnectionAttempts" => bounded(value, 1, 10),
                "ServerAliveInterval" => bounded(value, 0, 3600),
                "ServerAliveCountMax" => bounded(value, 0, 100),
                "LogLevel" => ["ERROR", "QUIET", "FATAL"].contains(&value),
                "StrictHostKeyChecking" => ["yes", "accept-new"].contains(&value),
                "UserKnownHostsFile" => valid_known_hosts_path(value),
                _ => false,
            };
            if !valid {
                return Err(err("ssh.extraOptions"));
            }
        }
        match &ssh.proxy {
            Some(SSHProxy::Jump(j)) => {
                validate_host_user(&j.host, &j.user, j.port)?;
                if j.identity_file.as_ref().is_some_and(|p| !absolute(p))
                    || j.known_hosts_file
                        .as_ref()
                        .is_some_and(|p| !valid_known_hosts_path(p))
                {
                    return Err(err("ssh.proxy path"));
                }
            }
            Some(SSHProxy::Command { executable }) if !absolute(executable) => {
                return Err(err("ssh.proxy.executable"))
            }
            _ => {}
        }
        Ok(())
    }
}
fn bounded(s: &str, min: u32, max: u32) -> bool {
    s.parse::<u32>()
        .ok()
        .is_some_and(|v| v >= min && v <= max && v.to_string() == s)
}
pub(crate) fn safe_local_file(path: &str, executable: bool) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_file()
        && metadata.uid() == unsafe { libc::getuid() }
        && if executable {
            metadata.mode() & 0o022 == 0 && metadata.mode() & 0o100 != 0
        } else {
            metadata.mode() & 0o077 == 0 && metadata.mode() & 0o400 != 0
        }
}
fn absolute(s: &str) -> bool {
    s.starts_with('/')
        && !s.contains('%')
        && !s.bytes().any(|b| b < 32 || b == 127)
        && Path::new(s)
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && !s.contains("//")
        && (s == "/" || !s.ends_with('/'))
}
fn valid_known_hosts_path(s: &str) -> bool {
    absolute(s) && s != "/dev/null"
}
fn valid_executable(s: &str) -> bool {
    if !s.starts_with('/') && s.contains('/') {
        return false;
    }
    let value = s.strip_prefix('/').unwrap_or(s);
    !value.is_empty()
        && value.split('/').all(|c| {
            !c.is_empty()
                && c.as_bytes()[0].is_ascii_alphanumeric()
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        })
}
fn validate_host_user(host: &str, user: &str, port: u16) -> Result<(), RegistryError> {
    let valid_host = host.parse::<std::net::IpAddr>().is_ok()
        || (!host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|p| {
                !p.is_empty()
                    && p.len() <= 63
                    && !p.starts_with('-')
                    && !p.ends_with('-')
                    && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }));
    let mut chars = user.bytes();
    let valid_user = chars
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && chars.all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !valid_host || !valid_user || port == 0 {
        return Err(err("invalid ssh host, user or port"));
    }
    Ok(())
}

pub struct MachineRegistryStore {
    pub file_path: PathBuf,
}
impl MachineRegistryStore {
    pub fn new(file_path: PathBuf) -> Self {
        Self { file_path }
    }
    pub fn snapshot(&self) -> Result<Option<Vec<u8>>, RegistryError> {
        self.ensure_directory()?;
        match fs::symlink_metadata(&self.file_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(err(e.to_string())),
            Ok(_) => {
                self.validate_file()?;
                fs::read(&self.file_path)
                    .map(Some)
                    .map_err(|e| err(e.to_string()))
            }
        }
    }
    pub fn restore(&self, snapshot: Option<&[u8]>) -> Result<(), RegistryError> {
        self.ensure_directory()?;
        if fs::symlink_metadata(&self.file_path).is_ok() {
            self.validate_file()?;
        }
        match snapshot {
            None => match fs::remove_file(&self.file_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(err(e.to_string())),
            },
            Some(bytes) => {
                let temp = self
                    .directory()?
                    .join(format!(".machines.{}.tmp", uuid::Uuid::new_v4()));
                let result = (|| {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&temp)
                        .map_err(|e| err(e.to_string()))?;
                    file.write_all(bytes)
                        .and_then(|_| file.sync_all())
                        .map_err(|e| err(e.to_string()))?;
                    fs::rename(&temp, &self.file_path).map_err(|e| err(e.to_string()))?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = fs::remove_file(temp);
                }
                result
            }
        }
    }
    fn directory(&self) -> Result<&Path, RegistryError> {
        self.file_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| err("missing parent"))
    }
    fn ensure_directory(&self) -> Result<(), RegistryError> {
        let path = self.directory()?;
        if !path.exists() {
            fs::create_dir_all(path).map_err(|e| err(e.to_string()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|e| err(e.to_string()))?;
        }
        let meta = fs::symlink_metadata(path).map_err(|e| err(e.to_string()))?;
        if !meta.file_type().is_dir()
            || meta.uid() != unsafe { libc::getuid() }
            || meta.mode() & 0o7777 != 0o700
        {
            return Err(err("unsafe registry directory"));
        }
        Ok(())
    }
    fn validate_file(&self) -> Result<(), RegistryError> {
        let meta = fs::symlink_metadata(&self.file_path).map_err(|e| err(e.to_string()))?;
        if !meta.file_type().is_file()
            || meta.nlink() != 1
            || meta.uid() != unsafe { libc::getuid() }
            || meta.mode() & 0o7777 != 0o600
        {
            return Err(err("unsafe registry file"));
        }
        Ok(())
    }
    pub fn load(&self) -> Result<MachineRegistry, RegistryError> {
        self.ensure_directory()?;
        match fs::symlink_metadata(&self.file_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(MachineRegistry {
                    schema_version: 3,
                    local_session_sources: default_sources(),
                    machines: vec![],
                })
            }
            Err(e) => return Err(err(e.to_string())),
            Ok(_) => {}
        }
        self.validate_file()?;
        let data = fs::read(&self.file_path).map_err(|e| err(e.to_string()))?;
        if data.len() > 65536 {
            return Err(err("registry too large"));
        }
        let mut de = serde_json::Deserializer::from_slice(&data);
        let mut value = StrictValue
            .deserialize(&mut de)
            .map_err(|e| err(e.to_string()))?;
        de.end().map_err(|e| err(e.to_string()))?;
        let version = shape(&value)?;
        if version < 3 {
            value["schemaVersion"] = Value::from(3);
            value["localSessionSources"] =
                serde_json::to_value(default_sources()).map_err(|e| err(e.to_string()))?;
        }
        let registry: MachineRegistry =
            serde_json::from_value(value).map_err(|e| err(e.to_string()))?;
        self.validate_registry(&registry)?;
        if version < 3 {
            self.save(&registry)?;
        }
        Ok(registry)
    }
    fn validate_registry(&self, registry: &MachineRegistry) -> Result<(), RegistryError> {
        if registry.schema_version != 3 {
            return Err(err("invalid schema version"));
        }
        for (field, paths) in [
            (
                "codexSessionDirs",
                &registry.local_session_sources.codex_session_dirs,
            ),
            (
                "claudeConfigDirs",
                &registry.local_session_sources.claude_config_dirs,
            ),
        ] {
            for (i, path) in paths.iter().enumerate() {
                if !MachineValidation::is_valid_session_source_path(path) {
                    return Err(err(format!("{field}[{i}]")));
                }
            }
        }
        let mut ids = HashSet::new();
        for machine in &registry.machines {
            MachineValidation::validate(machine)?;
            let ssh = machine.ssh.as_ref().ok_or_else(|| err("ssh required"))?;
            if ssh
                .identity_file
                .as_ref()
                .is_some_and(|p| !safe_local_file(p, false))
            {
                return Err(err("unsafe ssh identity"));
            }
            match &ssh.proxy {
                Some(SSHProxy::Jump(jump))
                    if jump
                        .identity_file
                        .as_ref()
                        .is_some_and(|p| !safe_local_file(p, false)) =>
                {
                    return Err(err("unsafe jump identity"))
                }
                Some(SSHProxy::Command { executable }) if !safe_local_file(executable, true) => {
                    return Err(err("unsafe proxy executable"))
                }
                _ => {}
            }
            if !ids.insert(&machine.id) {
                return Err(err("duplicate machine id"));
            }
        }
        Ok(())
    }
    pub fn save(&self, registry: &MachineRegistry) -> Result<(), RegistryError> {
        self.ensure_directory()?;
        self.validate_registry(registry)?;
        if fs::symlink_metadata(&self.file_path).is_ok() {
            self.validate_file()?;
        }
        let mut payload = registry.clone();
        payload.machines.sort_by(|a, b| a.id.cmp(&b.id));
        let mut data = serde_json::to_vec_pretty(&payload).map_err(|e| err(e.to_string()))?;
        data.push(b'\n');
        let temp = self
            .directory()?
            .join(format!(".machines.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)
                .map_err(|e| err(e.to_string()))?;
            file.write_all(&data)
                .and_then(|_| file.sync_all())
                .map_err(|e| err(e.to_string()))?;
            fs::rename(&temp, &self.file_path).map_err(|e| err(e.to_string()))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}
