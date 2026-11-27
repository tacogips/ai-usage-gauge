use ai_usage_core::remote::ccusage_client::RemoteCCUsageClient;
use rust_decimal::Decimal;
use std::str::FromStr;

const DAILY: &[u8] = br#"{"daily":[{"period":"2026-11-26","totalCost":999,"agents":[{"agent":"claude","modelBreakdowns":[{"modelName":"m","cost":0.12,"inputTokens":1,"outputTokens":2,"cacheCreationTokens":3,"cacheReadTokens":4}]}]}]}"#;

#[test]
fn decoder_uses_model_costs_once() {
    let rows = RemoteCCUsageClient::decode_daily(DAILY, "remote").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cost_usd, Decimal::from_str("0.12").unwrap());
    assert_eq!(rows[0].total_tokens, 10);
    assert_eq!(rows[0].machine, "remote");
    let blocks =
        br#"{"blocks":[{"startTime":"2026-11-26T00:00:00Z","costUSD":0.5,"models":["m"]}]}"#;
    assert_eq!(
        RemoteCCUsageClient::decode_blocks(blocks, "remote")
            .unwrap()
            .len(),
        1
    );
    let sessions = br#"{"session":[{"agent":"claude","metadata":{"lastActivity":"2026-11-26T00:00:00Z"},"modelBreakdowns":[{"modelName":"m","cost":0.1,"inputTokens":1,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0}]}]}"#;
    assert_eq!(
        RemoteCCUsageClient::decode_sessions(sessions, "remote")
            .unwrap()
            .len(),
        1
    );
    assert!(RemoteCCUsageClient::decode_daily(b"{}", "remote").is_err());
}

#[test]
fn daily_fallback_is_cached_after_nonempty_response() {
    let mut client = RemoteCCUsageClient::new();
    let mut modes = Vec::new();
    let result = client
        .daily("remote", |by_agent| {
            modes.push(by_agent);
            Ok(if by_agent {
                DAILY.to_vec()
            } else {
                br#"{"daily":[{"period":"2026-11-26"}]}"#.to_vec()
            })
        })
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(modes, vec![false, true]);
    modes.clear();
    client
        .daily("remote", |mode| {
            modes.push(mode);
            Ok(DAILY.to_vec())
        })
        .unwrap();
    assert_eq!(modes, vec![true]);
}

use ai_usage_core::remote::registry::{MachineRegistryStore, MachineValidation};
use ai_usage_core::types::{
    MachineDescriptor, MachineKind, MachineSessionSourcePlan, SSHConnection,
};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;

fn descriptor() -> MachineDescriptor {
    MachineDescriptor {
        id: "remote-a".into(),
        display_name: "Remote A".into(),
        kind: MachineKind::Ssh,
        enabled: true,
        ssh: Some(SSHConnection {
            host: "example.com".into(),
            port: 22,
            user: "user".into(),
            identity_file: None,
            extra_options: vec![],
            remote_ccusage_path: "ccusage".into(),
            proxy: None,
        }),
        codex_session_dirs: vec![],
        claude_config_dirs: vec![],
        include_default_codex_dir: true,
        include_default_claude_dir: true,
    }
}

#[test]
fn registry_migrates_and_rejects_invalid_documents() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().join("machines.json");
    let store = MachineRegistryStore::new(path.clone());
    assert!(store.load().unwrap().machines.is_empty());
    for version in [1, 2] {
        let data = format!(
            r#"{{"schemaVersion":{version},"machines":[{{"id":"remote-a","displayName":"Remote A","kind":"ssh","enabled":true,"ssh":{{"host":"example.com","port":22,"user":"user","extraOptions":[],"remoteCcusagePath":"ccusage"}}}}]}}"#
        );
        std::fs::write(&path, data).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(store.load().unwrap().schema_version, 3);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()
                ["schemaVersion"],
            3
        );
    }
    let invalid =
        br#"{"schemaVersion":3,"schemaVersion":3,"machines":[],"localSessionSources":{}}"#;
    std::fs::write(&path, invalid).unwrap();
    assert!(store.load().is_err());
    assert_eq!(std::fs::read(&path).unwrap(), invalid);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load().is_err());
}

#[test]
fn source_validation_and_missing_to_present_fingerprint() {
    let temp = tempfile::tempdir().unwrap();
    let mut machine = descriptor();
    machine.include_default_codex_dir = false;
    machine.include_default_claude_dir = false;
    machine.codex_session_dirs = vec![temp.path().to_string_lossy().into_owned()];
    let first = MachineSessionSourcePlan::resolve_local(&machine, &HashMap::new());
    assert!(first.codex_roots.is_empty());
    std::fs::create_dir(temp.path().join("sessions")).unwrap();
    let second = MachineSessionSourcePlan::resolve_local(&machine, &HashMap::new());
    assert_eq!(second.codex_roots.len(), 1);
    assert_ne!(first.fingerprint(), second.fingerprint());
    machine.codex_session_dirs = vec!["~other/dir".into()];
    assert!(MachineValidation::validate(&machine)
        .unwrap_err()
        .to_string()
        .contains("codexSessionDirs[0]"));
    machine.codex_session_dirs.clear();
    machine
        .ssh
        .as_mut()
        .unwrap()
        .extra_options
        .push("-o ProxyCommand=evil".into());
    assert!(MachineValidation::validate(&machine).is_err());
}

use ai_usage_core::remote::mutation_owner::{
    DirectoryNameLifecycle, MachineRegistryMutationOwner, MachineRuntimeReconciler, MutationError,
};
use ai_usage_core::types::MachineRegistry;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
struct Runtime {
    failures: AtomicUsize,
}
impl MachineRuntimeReconciler for Runtime {
    fn reconcile_registry<'a>(
        &'a self,
        _: &'a MachineRegistry,
        _: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                Err("failure".into())
            } else {
                Ok(())
            }
        })
    }
}
struct Metadata;
impl DirectoryNameLifecycle for Metadata {
    fn purge_for_reuse(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn prepare_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn commit_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn rollback_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn reconcile_machine_registry(&self, _: &[String]) -> Result<(), String> {
        Ok(())
    }
    fn purge_deleted(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}
#[tokio::test]
async fn failed_reconciliation_restores_absent_registry_and_latches_on_double_failure() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().join("machines.json");
    let store = MachineRegistryStore::new(path.clone());
    let initial = store.load().unwrap();
    let runtime = Arc::new(Runtime {
        failures: AtomicUsize::new(1),
    });
    let owner = MachineRegistryMutationOwner::new(
        store,
        initial.clone(),
        runtime.clone(),
        Arc::new(Metadata),
    );
    assert_eq!(
        owner.create(descriptor()).await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: false
        })
    );
    assert!(!path.exists());
    assert_eq!(owner.current().await, initial);
    runtime.failures.store(2, Ordering::SeqCst);
    assert_eq!(
        owner.create(descriptor()).await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: true
        })
    );
    assert_eq!(
        owner.create(descriptor()).await,
        Err(MutationError::ReconciliationRequired)
    );
}

#[test]
fn ssh_argv_and_diagnostics_are_safe() {
    use ai_usage_core::remote::diagnostics::MachineDiagnosticClassifier;
    use ai_usage_core::remote::ssh_runner::{
        CommandFailure, FailurePhase, SSHCCUsageCommandRunner, TokioProcessExecutor,
    };
    use std::time::Duration;
    let mut connection = descriptor().ssh.unwrap();
    connection.proxy = Some(ai_usage_core::types::SSHProxy::Command {
        executable: "/opt/proxy command".into(),
    });
    let runner = SSHCCUsageCommandRunner::new(connection, TokioProcessExecutor);
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
    assert!(args.iter().any(|v| v == "'/tmp/a'\\'';echo hacked'"));
    let failure = CommandFailure {
        phase: FailurePhase::Transport,
        exit_status: Some(255),
        stderr: b"secret-host permission denied; host key verification failed".to_vec(),
    };
    let result = MachineDiagnosticClassifier::classify(&failure);
    assert_eq!(result.code, "host_key_verification_failed");
    assert!(!format!("{result:?}").contains("secret-host"));
}

#[test]
fn remote_probe_fingerprint_includes_unavailable_states() {
    use ai_usage_core::remote::source_plan::RemoteSourceProbe;
    let mut probe = RemoteSourceProbe {
        scan_scope: "/remote/sessions".into(),
        raw_execution_value: Some("/remote".into()),
        exists: false,
        is_directory: false,
        is_default: true,
    };
    let machine = descriptor();
    let missing =
        MachineSessionSourcePlan::from_remote_probe_states(&machine, &[probe.clone()], &[]);
    probe.exists = true;
    let file = MachineSessionSourcePlan::from_remote_probe_states(&machine, &[probe.clone()], &[]);
    probe.is_directory = true;
    let directory = MachineSessionSourcePlan::from_remote_probe_states(&machine, &[probe], &[]);
    assert_ne!(missing.fingerprint(), file.fingerprint());
    assert_ne!(file.fingerprint(), directory.fingerprint());
    assert!(file.codex_roots.is_empty());
    assert_eq!(directory.codex_roots.len(), 1);
}

#[test]
fn equivalent_configured_paths_have_equal_fingerprints() {
    let mut first = descriptor();
    first.codex_session_dirs = vec!["/missing/a/../root".into()];
    let mut second = first.clone();
    second.codex_session_dirs = vec!["/missing/root".into()];
    assert_eq!(
        MachineSessionSourcePlan::resolve_local(&first, &HashMap::new()).fingerprint(),
        MachineSessionSourcePlan::resolve_local(&second, &HashMap::new()).fingerprint()
    );
}

#[tokio::test]
async fn partial_local_patch_preserves_omitted_fields() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = MachineRegistryStore::new(temp.path().join("machines.json"));
    let initial = store.load().unwrap();
    let owner = MachineRegistryMutationOwner::new(
        store,
        initial,
        Arc::new(Runtime {
            failures: AtomicUsize::new(0),
        }),
        Arc::new(Metadata),
    );
    let result = owner
        .patch_local_with(|sources| sources.include_default_codex_dir = false)
        .await
        .unwrap();
    assert!(!result.local_session_sources.include_default_codex_dir);
    assert!(result.local_session_sources.include_default_claude_dir);
}

#[test]
fn coalesced_points_are_order_independent() {
    use ai_usage_core::remote::ccusage_client::coalesce_points;
    let a = RemoteCCUsageClient::decode_blocks(
        br#"{"blocks":[{"startTime":"2026-11-26T00:00:00Z","costUSD":1,"models":["z"]}]}"#,
        "a",
    )
    .unwrap()
    .remove(0);
    let b = RemoteCCUsageClient::decode_blocks(
        br#"{"blocks":[{"startTime":"2026-11-26T00:00:00Z","costUSD":2,"models":["a"]}]}"#,
        "b",
    )
    .unwrap()
    .remove(0);
    assert_eq!(
        coalesce_points(vec![a.clone(), b.clone()], "remote").unwrap(),
        coalesce_points(vec![b, a], "remote").unwrap()
    );
}

struct LocalProbeExecutor;
impl ai_usage_core::remote::ssh_runner::ProcessExecutor for LocalProbeExecutor {
    fn execute<'a>(
        &'a self,
        _: &'a str,
        args: &'a [String],
        _: std::time::Duration,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        ai_usage_core::remote::ssh_runner::ProcessResult,
                        ai_usage_core::remote::ssh_runner::CommandFailure,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let position = args.iter().position(|v| v == "--").unwrap();
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(args[position + 2..].join(" "))
                .output()
                .unwrap();
            assert!(output.status.success());
            Ok(ai_usage_core::remote::ssh_runner::ProcessResult {
                stdout: output.stdout,
                stderr: output.stderr,
                exit_status: 0,
            })
        })
    }
}

struct FailingExecutor {
    phase: ai_usage_core::remote::ssh_runner::FailurePhase,
    calls: Arc<AtomicUsize>,
}
impl ai_usage_core::remote::ssh_runner::ProcessExecutor for FailingExecutor {
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: &'a [String],
        _: std::time::Duration,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        ai_usage_core::remote::ssh_runner::ProcessResult,
                        ai_usage_core::remote::ssh_runner::CommandFailure,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ai_usage_core::remote::ssh_runner::CommandFailure {
                phase: self.phase,
                exit_status: Some(255),
                stderr: vec![],
            })
        })
    }
}
#[tokio::test]
async fn source_retries_only_transport_failures() {
    use ai_usage_core::remote::ssh_runner::{
        FailurePhase, RetryingSSHRunner, SSHCCUsageCommandRunner,
    };
    for (phase, expected_calls) in [
        (FailurePhase::Transport, 3),
        (FailurePhase::RemoteCommand, 1),
        (FailurePhase::Timeout, 1),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = RetryingSSHRunner {
            runner: SSHCCUsageCommandRunner::new(
                descriptor().ssh.unwrap(),
                FailingExecutor {
                    phase,
                    calls: calls.clone(),
                },
            ),
            max_attempts: 3,
        };
        assert!(runner
            .run_source(
                &["daily".into()],
                "codex",
                "explicit",
                "/tmp/source",
                "",
                std::time::Duration::from_secs(1)
            )
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
}

#[tokio::test]
async fn process_executor_classifies_exit_and_timeout() {
    use ai_usage_core::remote::ssh_runner::{FailurePhase, ProcessExecutor, TokioProcessExecutor};
    for (script, expected) in [
        ("exit 255", FailurePhase::Transport),
        ("exit 2", FailurePhase::RemoteCommand),
        ("exec sleep 2", FailurePhase::Timeout),
    ] {
        let args = vec!["-c".into(), script.into()];
        let error = TokioProcessExecutor
            .execute("/bin/sh", &args, std::time::Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(error.phase, expected);
    }
}

#[tokio::test]
async fn fixed_probe_preserves_paths_and_missing_states() {
    use ai_usage_core::remote::ssh_runner::{decode_probe, SSHCCUsageCommandRunner};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("a' b");
    std::fs::create_dir_all(root.join("sessions")).unwrap();
    let mut machine = descriptor();
    machine.include_default_codex_dir = false;
    machine.include_default_claude_dir = false;
    machine.codex_session_dirs = vec![
        root.to_string_lossy().into_owned(),
        temp.path().join("missing").to_string_lossy().into_owned(),
    ];
    let runner = SSHCCUsageCommandRunner::new(machine.ssh.clone().unwrap(), LocalProbeExecutor);
    let (codex, claude) = runner
        .probe_sources(&machine, std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(codex.len(), 2);
    assert!(codex[0].is_directory);
    assert_eq!(
        codex[0].raw_execution_value.as_deref(),
        Some(machine.codex_session_dirs[0].as_str())
    );
    assert!(!codex[1].exists);
    assert!(claude.is_empty());
    assert!(decode_probe(b"codex\0", &machine).is_err());
    let plan = MachineSessionSourcePlan::from_remote_probe_states(&machine, &codex, &claude);
    assert_eq!(plan.codex_roots.len(), 1);
}

#[test]
fn source_environment_and_aliases_are_deduplicated() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    std::fs::create_dir_all(root.join("sessions")).unwrap();
    let alias = temp.path().join("alias");
    symlink(&root, &alias).unwrap();
    let mut machine = descriptor();
    machine.include_default_claude_dir = false;
    machine.codex_session_dirs = vec![alias.to_string_lossy().into_owned()];
    let env = HashMap::from([
        ("HOME".into(), "/missing".into()),
        ("CODEX_HOME".into(), root.to_string_lossy().into_owned()),
    ]);
    let plan = MachineSessionSourcePlan::resolve_local(&machine, &env);
    assert_eq!(plan.codex_roots.len(), 1);
    assert_eq!(
        plan.codex_roots[0],
        std::fs::canonicalize(root.join("sessions")).unwrap()
    );
}

#[tokio::test]
async fn concurrent_partial_patches_are_serialized() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = MachineRegistryStore::new(temp.path().join("machines.json"));
    let initial = store.load().unwrap();
    let owner = MachineRegistryMutationOwner::new(
        store,
        initial,
        Arc::new(Runtime {
            failures: AtomicUsize::new(0),
        }),
        Arc::new(Metadata),
    );
    let (a, b) = tokio::join!(
        owner.patch_local_with(|s| s.include_default_codex_dir = false),
        owner.patch_local_with(|s| s.include_default_claude_dir = false)
    );
    a.unwrap();
    b.unwrap();
    let result = owner.current().await;
    assert!(!result.local_session_sources.include_default_codex_dir);
    assert!(!result.local_session_sources.include_default_claude_dir);
}

#[test]
fn accepted_ssh_options_are_usable_by_runner() {
    use ai_usage_core::remote::ssh_runner::{SSHCCUsageCommandRunner, TokioProcessExecutor};
    let mut machine = descriptor();
    machine.ssh.as_mut().unwrap().extra_options =
        vec!["-o ConnectTimeout=1".into(), "-o ConnectTimeout=2".into()];
    MachineValidation::validate(&machine).unwrap();
    let runner = SSHCCUsageCommandRunner::new(machine.ssh.unwrap(), TokioProcessExecutor);
    assert!(runner
        .ssh_arguments(&[], std::time::Duration::from_secs(5))
        .is_ok());
}

struct FailRestoreRuntime(std::path::PathBuf, AtomicUsize);
impl MachineRuntimeReconciler for FailRestoreRuntime {
    fn reconcile_registry<'a>(
        &'a self,
        _: &'a MachineRegistry,
        _: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if self.1.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644)).unwrap();
                Err("failed".into())
            } else {
                Ok(())
            }
        })
    }
}
struct TrackedMetadata(AtomicUsize);
impl DirectoryNameLifecycle for TrackedMetadata {
    fn purge_for_reuse(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn prepare_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn commit_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn rollback_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn reconcile_machine_registry(&self, _: &[String]) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn purge_deleted(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}
#[tokio::test]
async fn failed_disk_rollback_keeps_deletion_marker_and_latches() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().join("machines.json");
    let store = MachineRegistryStore::new(path.clone());
    let mut initial = store.load().unwrap();
    initial.machines.push(descriptor());
    store.save(&initial).unwrap();
    let metadata = Arc::new(TrackedMetadata(AtomicUsize::new(0)));
    let owner = MachineRegistryMutationOwner::new(
        store,
        initial,
        Arc::new(FailRestoreRuntime(path, AtomicUsize::new(0))),
        metadata.clone(),
    );
    assert_eq!(
        owner.delete("remote-a").await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: true
        })
    );
    assert_eq!(metadata.0.load(Ordering::SeqCst), 0);
    assert_eq!(
        owner.delete("remote-a").await,
        Err(MutationError::ReconciliationRequired)
    );
}

#[test]
fn jump_argv_and_failure_classes_are_safe() {
    use ai_usage_core::remote::diagnostics::MachineDiagnosticClassifier;
    use ai_usage_core::remote::ssh_runner::{
        CommandFailure, FailurePhase, SSHCCUsageCommandRunner, TokioProcessExecutor,
    };
    use ai_usage_core::types::{SSHJumpProxy, SSHProxy};
    let mut connection = descriptor().ssh.unwrap();
    connection.proxy = Some(SSHProxy::Jump(SSHJumpProxy {
        host: "jump.example".into(),
        port: 2222,
        user: "bob".into(),
        identity_file: None,
        known_hosts_file: None,
    }));
    let args = SSHCCUsageCommandRunner::new(connection, TokioProcessExecutor)
        .ssh_arguments(&["daily".into()], std::time::Duration::from_secs(3))
        .unwrap();
    assert!(args
        .iter()
        .any(|s| s.contains("ProxyCommand=") && s.contains("%h:%p")));
    for (phase, stderr, expected) in [
        (
            FailurePhase::Transport,
            "Permission denied secret",
            "auth_failed",
        ),
        (FailurePhase::Timeout, "secret", "timeout"),
        (
            FailurePhase::RemoteCommand,
            "secret",
            "remote_command_failed",
        ),
    ] {
        let diagnostic = MachineDiagnosticClassifier::classify(&CommandFailure {
            phase,
            exit_status: None,
            stderr: stderr.as_bytes().to_vec(),
        });
        assert_eq!(diagnostic.code, expected);
        assert!(!format!("{diagnostic:?}").contains("secret"));
    }
}

#[test]
fn registry_rejects_links_and_preserves_invalid_migration() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().join("machines.json");
    let target = temp.path().join("target");
    let invalid = br#"{"schemaVersion":2,"machines":[],"unexpected":true}"#;
    std::fs::write(&target, invalid).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &path).unwrap();
    assert!(MachineRegistryStore::new(path.clone()).load().is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::hard_link(&target, &path).unwrap();
    assert!(MachineRegistryStore::new(path.clone()).load().is_err());
    std::fs::remove_file(&target).unwrap();
    assert!(MachineRegistryStore::new(path.clone()).load().is_err());
    assert_eq!(std::fs::read(path).unwrap(), invalid);
}
#[path = "../src/remote/integration_regressions.rs"]
mod integration_regressions;
