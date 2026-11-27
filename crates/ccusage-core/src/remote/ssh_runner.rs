use std::{future::Future, pin::Pin, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

use crate::remote::registry::{safe_local_file, MachineValidation};
use crate::remote::source_plan::RemoteSourceProbe;
use crate::types::{MachineDescriptor, SSHConnection, SSHProxy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePhase {
    Transport,
    RemoteCommand,
    Timeout,
    InvalidResponse,
    Cache,
    Internal,
}
#[derive(Debug, Clone)]
pub struct CommandFailure {
    pub phase: FailurePhase,
    pub exit_status: Option<i32>,
    pub stderr: Vec<u8>,
}
#[derive(Debug, Clone)]
pub struct ProcessResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: i32,
}

pub trait ProcessExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        executable: &'a str,
        args: &'a [String],
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessResult, CommandFailure>> + Send + 'a>>;
}
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioProcessExecutor;
impl ProcessExecutor for TokioProcessExecutor {
    fn execute<'a>(
        &'a self,
        executable: &'a str,
        args: &'a [String],
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<ProcessResult, CommandFailure>> + Send + 'a>> {
        Box::pin(async move {
            let failure = || CommandFailure {
                phase: FailurePhase::Transport,
                exit_status: None,
                stderr: vec![],
            };
            let mut child = Command::new(executable)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| failure())?;
            let mut stdout = child.stdout.take().ok_or_else(failure)?;
            let mut stderr = child.stderr.take().ok_or_else(failure)?;
            let mut out = tokio::spawn(async move {
                let mut bytes = Vec::new();
                stdout.read_to_end(&mut bytes).await.map(|_| bytes)
            });
            let mut err = tokio::spawn(async move {
                let mut bytes = Vec::new();
                stderr.read_to_end(&mut bytes).await.map(|_| bytes)
            });
            let deadline = tokio::time::Instant::now() + timeout;
            let status = match tokio::time::timeout_at(deadline, child.wait()).await {
                Ok(result) => result.map_err(|_| failure())?,
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    out.abort();
                    err.abort();
                    return Err(CommandFailure {
                        phase: FailurePhase::Timeout,
                        exit_status: None,
                        stderr: vec![],
                    });
                }
            };
            let outputs =
                tokio::time::timeout_at(deadline, async { tokio::join!(&mut out, &mut err) }).await;
            let (stdout, stderr) = match outputs {
                Ok((stdout, stderr)) => (
                    stdout.map_err(|_| failure())?.map_err(|_| failure())?,
                    stderr.map_err(|_| failure())?.map_err(|_| failure())?,
                ),
                Err(_) => {
                    out.abort();
                    err.abort();
                    return Err(CommandFailure {
                        phase: FailurePhase::Timeout,
                        exit_status: None,
                        stderr: vec![],
                    });
                }
            };
            let exit_status = status.code().unwrap_or(255);
            if exit_status != 0 {
                return Err(CommandFailure {
                    phase: if exit_status == 255 {
                        FailurePhase::Transport
                    } else {
                        FailurePhase::RemoteCommand
                    },
                    exit_status: Some(exit_status),
                    stderr,
                });
            }
            Ok(ProcessResult {
                stdout,
                stderr,
                exit_status,
            })
        })
    }
}

pub struct SSHCCUsageCommandRunner<E = TokioProcessExecutor> {
    pub connection: SSHConnection,
    pub executable: String,
    pub executor: E,
}

pub fn quote_remote_token(token: &str) -> String {
    format!("'{}'", token.replace('\'', "'\\''"))
}
fn destination(user: &str, host: &str) -> String {
    if host.contains(':') {
        format!("{user}@[{host}]")
    } else {
        format!("{user}@{host}")
    }
}

impl<E: ProcessExecutor> SSHCCUsageCommandRunner<E> {
    pub fn new(connection: SSHConnection, executor: E) -> Self {
        Self {
            connection,
            executable: "/usr/bin/ssh".into(),
            executor,
        }
    }
    pub fn probe_arguments(
        &self,
        descriptor: &MachineDescriptor,
        timeout: Duration,
    ) -> Result<Vec<String>, String> {
        if descriptor.ssh.as_ref() != Some(&self.connection)
            || descriptor
                .codex_session_dirs
                .iter()
                .chain(&descriptor.claude_config_dirs)
                .any(|p| {
                    !crate::remote::registry::MachineValidation::is_valid_session_source_path(p)
                })
        {
            return Err("invalid probe descriptor".into());
        }
        let mut result = self.transport_arguments(timeout)?;
        result.extend([
            "--".into(),
            destination(&self.connection.user, &self.connection.host),
        ]);
        let mut tokens = vec![
            "sh".to_owned(),
            "-c".into(),
            PROBE_ADAPTER.into(),
            "ccusage-gauge-probe".into(),
            u8::from(descriptor.include_default_codex_dir).to_string(),
            u8::from(descriptor.include_default_claude_dir).to_string(),
            descriptor.codex_session_dirs.len().to_string(),
            descriptor.claude_config_dirs.len().to_string(),
        ];
        tokens.extend(descriptor.codex_session_dirs.iter().cloned());
        tokens.extend(descriptor.claude_config_dirs.iter().cloned());
        result.extend(tokens.iter().map(|v| quote_remote_token(v)));
        Ok(result)
    }
    pub async fn probe_sources(
        &self,
        descriptor: &MachineDescriptor,
        timeout: Duration,
    ) -> Result<(Vec<RemoteSourceProbe>, Vec<RemoteSourceProbe>), CommandFailure> {
        let argv = self
            .probe_arguments(descriptor, timeout)
            .map_err(|_| probe_failure(FailurePhase::Internal))?;
        self.check_local_files()?;
        if !descriptor.include_default_codex_dir
            && !descriptor.include_default_claude_dir
            && descriptor.codex_session_dirs.is_empty()
            && descriptor.claude_config_dirs.is_empty()
        {
            return Ok((Vec::new(), Vec::new()));
        }
        let result = self
            .executor
            .execute(&self.executable, &argv, timeout)
            .await?;
        decode_probe(&result.stdout, descriptor)
            .map_err(|_| probe_failure(FailurePhase::InvalidResponse))
    }
    pub fn ssh_arguments(
        &self,
        arguments: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>, String> {
        let mut result = self.transport_arguments(timeout)?;
        result.extend([
            "--".into(),
            destination(&self.connection.user, &self.connection.host),
            quote_remote_token(&self.connection.remote_ccusage_path),
        ]);
        result.extend(arguments.iter().map(|s| quote_remote_token(s)));
        Ok(result)
    }
    pub fn source_arguments(
        &self,
        arguments: &[String],
        agent: &str,
        source_kind: &str,
        source: &str,
        mtime_cutoff: &str,
        timeout: Duration,
    ) -> Result<Vec<String>, String> {
        if !["codex", "claude"].contains(&agent)
            || !["default", "captured-default", "explicit"].contains(&source_kind)
            || (!mtime_cutoff.is_empty()
                && !mtime_cutoff
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.'))
        {
            return Err("invalid source adapter arguments".into());
        }
        let mut result = self.transport_arguments(timeout)?;
        result.extend([
            "--".into(),
            destination(&self.connection.user, &self.connection.host),
        ]);
        let tokens = [
            "sh",
            "-c",
            SOURCE_ADAPTER,
            "ccusage-gauge-source",
            agent,
            source_kind,
            source,
            mtime_cutoff,
            &self.connection.remote_ccusage_path,
        ];
        result.extend(tokens.iter().map(|s| quote_remote_token(s)));
        result.extend(arguments.iter().map(|s| quote_remote_token(s)));
        Ok(result)
    }
    fn transport_arguments(&self, timeout: Duration) -> Result<Vec<String>, String> {
        let c = &self.connection;
        MachineValidation::validate_connection(c)
            .map_err(|_| "invalid SSH connection".to_owned())?;
        // Reject values that can alter SSH argv or expand ProxyCommand placeholders.
        let safe_host = |s: &str| {
            !s.is_empty()
                && !s.starts_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))
        };
        let safe_user = |s: &str| {
            !s.is_empty()
                && !s.starts_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        };
        let safe_path = |s: &str| {
            s.starts_with('/')
                && !s.contains('%')
                && !s.bytes().any(|b| b < 32 || b == 127)
                && !s.split('/').any(|p| p == "." || p == "..")
        };
        if !safe_host(&c.host)
            || !safe_user(&c.user)
            || c.port == 0
            || c.remote_ccusage_path.is_empty()
            || c.remote_ccusage_path.bytes().any(|b| b < 32 || b == 127)
            || c.identity_file.as_deref().is_some_and(|p| !safe_path(p))
        {
            return Err("invalid SSH connection".into());
        }
        let mut result: Vec<String> = [
            "-F",
            "/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if let Some(identity) = &c.identity_file {
            result.extend(["-i".into(), identity.clone()]);
        }
        result.extend(["-p".into(), c.port.to_string()]);
        let mut names = std::collections::HashSet::new();
        let mut family = false;
        let mut strict = false;
        let mut connect_timeout = false;
        for option in &c.extra_options {
            if option == "-4" || option == "-6" {
                if family {
                    return Err("duplicate address family".into());
                }
                family = true;
                result.push(option.clone());
                continue;
            }
            let value = option.strip_prefix("-o ").ok_or("invalid SSH option")?;
            let (key, value_part) = value.split_once('=').ok_or("invalid SSH option")?;
            if !names.insert(option) {
                return Err("duplicate SSH option".into());
            }
            let bounded = |lo, hi| {
                value_part
                    .parse::<u32>()
                    .ok()
                    .is_some_and(|v| v >= lo && v <= hi && v.to_string() == value_part)
            };
            let allowed = match key {
                "ConnectTimeout" => {
                    connect_timeout = true;
                    bounded(1, 600)
                }
                "ConnectionAttempts" => bounded(1, 10),
                "ServerAliveInterval" => bounded(0, 3600),
                "ServerAliveCountMax" => bounded(0, 100),
                "LogLevel" => ["ERROR", "QUIET", "FATAL"].contains(&value_part),
                "StrictHostKeyChecking" => {
                    strict = true;
                    ["yes", "accept-new"].contains(&value_part)
                }
                "UserKnownHostsFile" => safe_path(value_part) && value_part != "/dev/null",
                _ => false,
            };
            if !allowed {
                return Err("unsupported SSH option".into());
            }
            result.extend(["-o".into(), value.into()]);
        }
        if !strict {
            result.extend(["-o".into(), "StrictHostKeyChecking=yes".into()]);
        }
        if !connect_timeout {
            result.extend([
                "-o".into(),
                format!(
                    "ConnectTimeout={}",
                    timeout
                        .as_secs()
                        .saturating_add(u64::from(timeout.subsec_nanos() > 0))
                        .clamp(1, 600)
                ),
            ]);
        }
        let proxy_tokens: Option<Vec<String>> = match &c.proxy {
            None | Some(SSHProxy::Direct) => None,
            Some(SSHProxy::Command { executable }) => {
                if !safe_path(executable) {
                    return Err("invalid proxy executable".into());
                }
                Some(vec![
                    executable.clone(),
                    "connect".into(),
                    "--host".into(),
                    c.host.clone(),
                    "--port".into(),
                    c.port.to_string(),
                ])
            }
            Some(SSHProxy::Jump(j)) => {
                if !safe_host(&j.host)
                    || !safe_user(&j.user)
                    || j.port == 0
                    || j.identity_file.as_deref().is_some_and(|p| !safe_path(p))
                    || j.known_hosts_file
                        .as_deref()
                        .is_some_and(|p| !safe_path(p) || p == "/dev/null")
                {
                    return Err("invalid jump proxy".into());
                }
                let mut t: Vec<String> = [
                    "/usr/bin/ssh",
                    "-F",
                    "/dev/null",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "IdentitiesOnly=yes",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect();
                if let Some(path) = &j.identity_file {
                    t.extend(["-i".into(), path.clone()]);
                }
                if let Some(path) = &j.known_hosts_file {
                    t.extend(["-o".into(), format!("UserKnownHostsFile={path}")]);
                }
                t.extend([
                    "-o".into(),
                    "StrictHostKeyChecking=yes".into(),
                    "-p".into(),
                    j.port.to_string(),
                    "-W".into(),
                    "%h:%p".into(),
                    destination(&j.user, &j.host),
                ]);
                Some(t)
            }
        };
        if let Some(tokens) = proxy_tokens {
            result.extend([
                "-o".into(),
                format!(
                    "ProxyCommand={}",
                    tokens
                        .iter()
                        .map(|s| quote_remote_token(s))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            ]);
        }
        Ok(result)
    }
    pub async fn run_source(
        &self,
        arguments: &[String],
        agent: &str,
        source_kind: &str,
        source: &str,
        mtime_cutoff: &str,
        timeout: Duration,
    ) -> Result<ProcessResult, CommandFailure> {
        let argv = self
            .source_arguments(arguments, agent, source_kind, source, mtime_cutoff, timeout)
            .map_err(|_| CommandFailure {
                phase: FailurePhase::Internal,
                exit_status: None,
                stderr: vec![],
            })?;
        self.check_local_files()?;
        self.executor
            .execute(&self.executable, &argv, timeout)
            .await
    }
    pub async fn run(
        &self,
        arguments: &[String],
        timeout: Duration,
    ) -> Result<ProcessResult, CommandFailure> {
        let argv = self
            .ssh_arguments(arguments, timeout)
            .map_err(|_| CommandFailure {
                phase: FailurePhase::Internal,
                exit_status: None,
                stderr: vec![],
            })?;
        self.check_local_files()?;
        self.executor
            .execute(&self.executable, &argv, timeout)
            .await
    }
    fn check_local_files(&self) -> Result<(), CommandFailure> {
        let c = &self.connection;
        let identity_safe = c
            .identity_file
            .as_deref()
            .is_none_or(|p| safe_local_file(p, false));
        let proxy_safe = match &c.proxy {
            Some(SSHProxy::Jump(j)) => j
                .identity_file
                .as_deref()
                .is_none_or(|p| safe_local_file(p, false)),
            Some(SSHProxy::Command { executable }) => safe_local_file(executable, true),
            _ => true,
        };
        if identity_safe && proxy_safe {
            Ok(())
        } else {
            Err(probe_failure(FailurePhase::Internal))
        }
    }
}
pub struct RetryingSSHRunner<E> {
    pub runner: SSHCCUsageCommandRunner<E>,
    pub max_attempts: u32,
}
impl<E: ProcessExecutor> RetryingSSHRunner<E> {
    pub async fn run_source(
        &self,
        arguments: &[String],
        agent: &str,
        source_kind: &str,
        source: &str,
        mtime_cutoff: &str,
        timeout: Duration,
    ) -> Result<ProcessResult, CommandFailure> {
        for _ in 1..self.max_attempts.clamp(1, 10) {
            match self
                .runner
                .run_source(arguments, agent, source_kind, source, mtime_cutoff, timeout)
                .await
            {
                Err(e) if e.phase == FailurePhase::Transport => {}
                result => return result,
            }
        }
        self.runner
            .run_source(arguments, agent, source_kind, source, mtime_cutoff, timeout)
            .await
    }
    pub async fn probe_sources(
        &self,
        descriptor: &MachineDescriptor,
        timeout: Duration,
    ) -> Result<(Vec<RemoteSourceProbe>, Vec<RemoteSourceProbe>), CommandFailure> {
        for _ in 1..self.max_attempts.clamp(1, 10) {
            match self.runner.probe_sources(descriptor, timeout).await {
                Err(e) if e.phase == FailurePhase::Transport => {}
                result => return result,
            }
        }
        self.runner.probe_sources(descriptor, timeout).await
    }
    pub async fn run(
        &self,
        arguments: &[String],
        timeout: Duration,
    ) -> Result<ProcessResult, CommandFailure> {
        for attempt in 1..self.max_attempts.clamp(1, 10) {
            match self.runner.run(arguments, timeout).await {
                Err(e) if e.phase == FailurePhase::Transport => {
                    let _ = attempt;
                }
                result => return result,
            }
        }
        self.runner.run(arguments, timeout).await
    }
}
fn probe_failure(phase: FailurePhase) -> CommandFailure {
    CommandFailure {
        phase,
        exit_status: None,
        stderr: vec![],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeDecodeError;

pub fn decode_probe(
    data: &[u8],
    descriptor: &MachineDescriptor,
) -> Result<(Vec<RemoteSourceProbe>, Vec<RemoteSourceProbe>), ProbeDecodeError> {
    let fields: Vec<&[u8]> = if data.is_empty() {
        Vec::new()
    } else {
        if data.last() != Some(&0) {
            return Err(ProbeDecodeError);
        }
        data[..data.len() - 1].split(|b| *b == 0).collect()
    };
    let expected_codex =
        descriptor.codex_session_dirs.len() + usize::from(descriptor.include_default_codex_dir);
    let expected_claude =
        descriptor.claude_config_dirs.len() + usize::from(descriptor.include_default_claude_dir);
    if fields.len() != (expected_codex + expected_claude) * 7 {
        return Err(ProbeDecodeError);
    }
    let mut codex = Vec::new();
    let mut claude = Vec::new();
    for (index, fields) in fields.as_chunks::<7>().0.iter().enumerate() {
        let text = |i: usize| std::str::from_utf8(fields[i]).map_err(|_| ProbeDecodeError);
        let boolean = |i: usize| match fields[i] {
            b"0" => Ok(false),
            b"1" => Ok(true),
            _ => Err(ProbeDecodeError),
        };
        let agent = text(0)?;
        let is_default = boolean(1)?;
        let expected_agent = if index < expected_codex {
            "codex"
        } else {
            "claude"
        };
        let expected_default = if agent == "codex" {
            index == 0 && descriptor.include_default_codex_dir
        } else {
            index == expected_codex && descriptor.include_default_claude_dir
        };
        let raw = text(2)?.to_owned();
        let _root = text(3)?;
        let scope = text(4)?;
        let exists = boolean(5)?;
        let is_directory = boolean(6)?;
        if agent != expected_agent
            || is_default != expected_default
            || !scope.starts_with('/')
            || (is_directory && !exists)
        {
            return Err(ProbeDecodeError);
        }
        let probe = RemoteSourceProbe {
            scan_scope: scope.into(),
            raw_execution_value: Some(raw),
            exists,
            is_directory,
            is_default,
        };
        if agent == "codex" {
            codex.push(probe);
        } else {
            claude.push(probe);
        }
    }
    Ok((codex, claude))
}

const PROBE_ADAPTER: &str = r#"include_codex=$1; include_claude=$2; codex_count=$3; claude_count=$4; shift 4
probe_base=$(pwd -P)
probe_source() {
 agent=$1; is_default=$2; source_value=$3
 if [ "$is_default" = 0 ]; then
  if [ "$source_value" = '~' ]; then source_value=$HOME; elif [ "${source_value#\~/}" != "$source_value" ]; then source_value=$HOME/${source_value#\~/}; fi
 fi
 if [ "$agent" = codex ]; then scan_name=sessions; else scan_name=projects; fi
 root=$source_value; root_for_cd=$source_value
 case "$root_for_cd" in /*) ;; *) root_for_cd=./$root_for_cd ;; esac
 if [ -d "$root_for_cd" ]; then root=$(cd -P "$root_for_cd" && pwd -P); else case "$root" in /*) ;; *) root=$probe_base/$root ;; esac; fi
 scan=$root/$scan_name; exists=0; directory=0
 if [ -d "$scan" ]; then scan=$(cd -P "$scan" && pwd -P); exists=1; directory=1
 elif [ -e "$scan" ] || [ -L "$scan" ]; then exists=1; fi
 printf '%s\0%s\0%s\0%s\0%s\0%s\0%s\0' "$agent" "$is_default" "$source_value" "$root" "$scan" "$exists" "$directory"
}
if [ "$include_codex" = 1 ]; then probe_source codex 1 "${CODEX_HOME-"$HOME/.codex"}"; fi
while [ "$codex_count" -gt 0 ]; do probe_source codex 0 "$1"; shift; codex_count=$((codex_count - 1)); done
if [ "$include_claude" = 1 ]; then probe_source claude 1 "${CLAUDE_CONFIG_DIR-"$HOME/.claude"}"; fi
while [ "$claude_count" -gt 0 ]; do probe_source claude 0 "$1"; shift; claude_count=$((claude_count - 1)); done"#;

const SOURCE_ADAPTER: &str = r#"agent=$1; kind=$2; value=$3; cutoff=$4; executable=$5; shift 5
if [ "$kind" = default ]; then
 if [ "$agent" = codex ]; then value=${CODEX_HOME-"$HOME/.codex"}; else value=${CLAUDE_CONFIG_DIR-"$HOME/.claude"}; fi
elif [ "$kind" = explicit ]; then
 if [ "$value" = '~' ]; then value=$HOME; elif [ "${value#\~/}" != "$value" ]; then value=$HOME/${value#\~/}; fi
fi
if [ "$agent" = codex ]; then scan=sessions; else scan=projects; fi
if [ ! -d "$value/$scan" ]; then case "$1" in blocks) printf '{"blocks":[]}' ;; daily) case " $* " in *" --sections "*) printf '{"daily":[],"session":[]}' ;; *) printf '{"daily":[]}' ;; esac ;; session) printf '{"session":[]}' ;; *) printf '{}' ;; esac; exit 0; fi
prune=
if [ -n "$cutoff" ]; then prune=$(mktemp -d "${TMPDIR:-/tmp}/ccusage-gauge-pruned.XXXXXX" 2>/dev/null) || prune=; fi
if [ -n "$prune" ] && touch -t "$cutoff" "$prune/cutoff" 2>/dev/null && mkdir -p "$prune/root/$scan"; then
 if (cd "$value/$scan" && find . -name '*.jsonl' -type f -newer "$prune/cutoff" | while IFS= read -r rel; do rel=${rel#./}; case "$rel" in */*) mkdir -p "$prune/root/$scan/${rel%/*}" || exit 1 ;; esac; ln "$value/$scan/$rel" "$prune/root/$scan/$rel" 2>/dev/null || cp "$value/$scan/$rel" "$prune/root/$scan/$rel" || exit 1; done); then value=$prune/root; fi
fi
disabled=${TMPDIR:-/tmp}/ccusage-gauge-disabled
mkdir -p "$disabled/codex/sessions" "$disabled/claude/projects"
if [ "$agent" = codex ]; then CODEX_HOME=$value CLAUDE_CONFIG_DIR=$disabled/claude "$executable" "$@"; else CODEX_HOME=$disabled/codex CLAUDE_CONFIG_DIR=$value "$executable" "$@"; fi
status=$?; if [ -n "$prune" ]; then rm -rf "$prune"; fi; exit $status"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> SSHConnection {
        SSHConnection {
            host: "host.example".into(),
            port: 22,
            user: "alice".into(),
            identity_file: None,
            extra_options: vec![],
            remote_ccusage_path: "ccusage".into(),
            proxy: None,
        }
    }

    #[test]
    fn positional_tokens_and_structured_proxies_are_quoted() {
        let mut c = connection();
        c.proxy = Some(SSHProxy::Command {
            executable: "/opt/proxy command".into(),
        });
        let runner = SSHCCUsageCommandRunner::new(c, TokioProcessExecutor);
        let args = runner
            .source_arguments(
                &["daily".into()],
                "codex",
                "explicit",
                "/tmp/a';echo hacked",
                "",
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(args.windows(2).any(|w| w == ["-F", "/dev/null"]));
        assert!(args.iter().any(|v| v == "StrictHostKeyChecking=yes"));
        assert!(args
            .iter()
            .any(|v| v.contains("ProxyCommand='/opt/proxy command' 'connect'")));
        assert!(args.iter().any(|v| v == "'/tmp/a'\\'';echo hacked'"));
        assert!(args.iter().any(|v| v == "'codex'"));
    }

    #[test]
    fn rejects_unstructured_proxy_and_percent_expansion() {
        let mut c = connection();
        c.extra_options.push("-o ProxyCommand=evil".into());
        assert!(SSHCCUsageCommandRunner::new(c, TokioProcessExecutor)
            .ssh_arguments(&[], Duration::from_secs(1))
            .is_err());
        let mut c = connection();
        c.proxy = Some(SSHProxy::Command {
            executable: "/tmp/%h".into(),
        });
        assert!(SSHCCUsageCommandRunner::new(c, TokioProcessExecutor)
            .ssh_arguments(&[], Duration::from_secs(1))
            .is_err());
    }
}
