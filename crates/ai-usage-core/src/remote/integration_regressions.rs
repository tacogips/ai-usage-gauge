use super::*;

#[test]
fn wrapped_diagnostics_are_classified_without_leaks() {
    use ai_usage_core::remote::diagnostics::MachineDiagnosticClassifier;
    use ai_usage_core::remote::ssh_runner::{CommandFailure, FailurePhase};
    for (message, code) in [
        (
            "Host key\r\nverification\tfailed secret",
            "host_key_verification_failed",
        ),
        ("Connection   refused secret", "tunnel_unreachable"),
        ("Permission\tdenied secret", "auth_failed"),
    ] {
        let d = MachineDiagnosticClassifier::classify(&CommandFailure {
            phase: FailurePhase::Transport,
            exit_status: None,
            stderr: message.as_bytes().to_vec(),
        });
        assert_eq!(d.code, code);
        assert!(!format!("{d:?}").contains("secret"));
    }
    let mut stderr = vec![b'x'; 4096];
    stderr.extend_from_slice(b" permission denied");
    assert_eq!(
        MachineDiagnosticClassifier::classify(&CommandFailure {
            phase: FailurePhase::Transport,
            exit_status: None,
            stderr
        })
        .code,
        "transport_failed"
    );
    assert_eq!(MachineDiagnosticClassifier::diagnostic("auth_failed").remediation.as_deref(), Some("Verify the configured user, identity-file reference and permissions, and server-side authorization."));
}

#[test]
fn ssh_path_and_executable_validation_matches_runner() {
    use ai_usage_core::remote::ssh_runner::{SSHCCUsageCommandRunner, TokioProcessExecutor};
    use ai_usage_core::types::{SSHJumpProxy, SSHProxy};
    let check = |connection: SSHConnection, expected: bool| {
        assert_eq!(
            MachineValidation::validate_connection(&connection).is_ok(),
            expected
        );
        let args = SSHCCUsageCommandRunner::new(connection, TokioProcessExecutor)
            .ssh_arguments(&[], std::time::Duration::from_secs(1));
        assert_eq!(args.is_ok(), expected);
        if let Ok(args) = args {
            assert!(args.windows(2).any(|w| w == ["-F", "/dev/null"]));
        }
    };
    for path in ["/dev/null", "/tmp/%h"] {
        let mut c = descriptor().ssh.unwrap();
        c.extra_options = vec![
            "-o StrictHostKeyChecking=accept-new".into(),
            format!("-o UserKnownHostsFile={path}"),
        ];
        check(c, false);
        let mut c = descriptor().ssh.unwrap();
        c.proxy = Some(SSHProxy::Jump(SSHJumpProxy {
            host: "jump.example".into(),
            port: 22,
            user: "bob".into(),
            identity_file: None,
            known_hosts_file: Some(path.into()),
        }));
        check(c, false);
    }
    let mut c = descriptor().ssh.unwrap();
    c.identity_file = Some("/tmp/%h".into());
    check(c, false);
    let mut c = descriptor().ssh.unwrap();
    c.proxy = Some(SSHProxy::Command {
        executable: "/tmp/%h".into(),
    });
    check(c, false);
    let mut c = descriptor().ssh.unwrap();
    c.proxy = Some(SSHProxy::Jump(SSHJumpProxy {
        host: "jump.example".into(),
        port: 22,
        user: "bob".into(),
        identity_file: Some("/tmp/%h".into()),
        known_hosts_file: None,
    }));
    check(c, false);
    let mut c = descriptor().ssh.unwrap();
    c.extra_options = vec!["-o UserKnownHostsFile=/tmp/known_hosts".into()];
    check(c, true);
    for (path, valid) in [
        ("bin/ccusage", false),
        ("./ccusage", false),
        ("/usr//bin/ccusage", false),
        ("/usr/bin/", false),
        ("ccusage", true),
        ("/usr/local/bin/ccusage", true),
    ] {
        let mut c = descriptor().ssh.unwrap();
        c.remote_ccusage_path = path.into();
        check(c, valid);
    }
}

#[derive(Default)]
struct LifecycleAudit {
    rollback: AtomicUsize,
    cancel: AtomicUsize,
    finalize_failures: AtomicUsize,
}
impl DirectoryNameLifecycle for LifecycleAudit {
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
        self.rollback.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn reconcile_machine_registry(&self, _: &[String]) -> Result<(), String> {
        self.cancel.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn purge_deleted(&self, _: &str) -> Result<(), String> {
        self.finalize_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .map_or(Ok(()), |_| Err("purge failed".into()))
    }
}

#[tokio::test]
async fn mutation_crud_persists_and_local_is_immutable() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().join("machines.json");
    let store = MachineRegistryStore::new(path.clone());
    let initial = store.load().unwrap();
    let owner = MachineRegistryMutationOwner::new(
        store,
        initial,
        Arc::new(Runtime {
            failures: AtomicUsize::new(0),
        }),
        Arc::new(LifecycleAudit::default()),
    );
    owner.create(descriptor()).await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        MachineRegistryStore::new(path.clone())
            .load()
            .unwrap()
            .machines,
        vec![descriptor()]
    );
    assert_eq!(
        owner.create(descriptor()).await,
        Err(MutationError::Conflict)
    );
    let mut local = descriptor();
    local.id = "local".into();
    assert_eq!(
        owner.create(local.clone()).await,
        Err(MutationError::Conflict)
    );
    assert_eq!(
        owner.replace("local", local).await,
        Err(MutationError::Conflict)
    );
    assert_eq!(
        owner.patch("local", |_| {}).await,
        Err(MutationError::Conflict)
    );
    assert_eq!(owner.delete("local").await, Err(MutationError::Conflict));
    let mut replacement = descriptor();
    replacement.display_name = "Updated".into();
    owner
        .replace("remote-a", replacement.clone())
        .await
        .unwrap();
    assert_eq!(owner.current().await.machines, vec![replacement.clone()]);
    assert_eq!(
        MachineRegistryStore::new(path.clone())
            .load()
            .unwrap()
            .machines,
        vec![replacement]
    );
    owner
        .patch("remote-a", |m| m.enabled = false)
        .await
        .unwrap();
    let current = owner.current().await;
    assert_eq!(current.machines[0].id, "remote-a");
    assert!(!current.machines[0].enabled);
    assert_eq!(
        MachineRegistryStore::new(path.clone()).load().unwrap(),
        current
    );
    owner.delete("remote-a").await.unwrap();
    assert!(owner.current().await.machines.is_empty());
    assert!(MachineRegistryStore::new(path)
        .load()
        .unwrap()
        .machines
        .is_empty());
}

#[tokio::test]
async fn recoverable_deletion_rollback_and_pending_cleanup_retry() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().join("machines.json");
    let store = MachineRegistryStore::new(path.clone());
    let mut initial = store.load().unwrap();
    initial.machines.push(descriptor());
    store.save(&initial).unwrap();
    let snapshot = std::fs::read(&path).unwrap();
    let runtime = Arc::new(Runtime {
        failures: AtomicUsize::new(1),
    });
    let metadata = Arc::new(LifecycleAudit::default());
    metadata.finalize_failures.store(1, Ordering::SeqCst);
    let owner =
        MachineRegistryMutationOwner::new(store, initial.clone(), runtime, metadata.clone());
    assert_eq!(
        owner.delete("remote-a").await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: false
        })
    );
    assert_eq!(std::fs::read(&path).unwrap(), snapshot);
    assert_eq!(owner.current().await, initial);
    assert_eq!(metadata.rollback.load(Ordering::SeqCst), 1);
    assert_eq!(metadata.cancel.load(Ordering::SeqCst), 1);
    owner.delete("remote-a").await.unwrap();
    assert_eq!(
        owner.metadata_cleanup_pending_machine_ids().await,
        vec!["remote-a".to_owned()]
    );
    assert!(owner.retry_pending_metadata_cleanup().await.is_empty());
    assert!(owner
        .metadata_cleanup_pending_machine_ids()
        .await
        .is_empty());
}

#[test]
fn remote_decoders_and_coalescers_preserve_totals_and_provenance() {
    use ai_usage_core::remote::ccusage_client::{
        coalesce_metrics, coalesce_points, coalesce_sessions,
    };
    use ai_usage_core::types::UsageDataQuality;
    let blocks = RemoteCCUsageClient::decode_blocks(
        br#"{"blocks":[{"startTime":"2026-11-26T00:00:00Z","costUSD":0.5,"models":["m"]}]}"#,
        "remote",
    )
    .unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].cost_usd, Decimal::from_str("0.5").unwrap());
    assert_eq!(blocks[0].models, vec!["m"]);
    assert_eq!(blocks[0].machine, "remote");
    let mut second = blocks[0].clone();
    second.cost_usd = Decimal::from_str("1.25").unwrap();
    second.models = vec!["z".into(), "a".into()];
    let points = coalesce_points(vec![blocks[0].clone(), second.clone()], "remote").unwrap();
    assert_eq!(
        points,
        coalesce_points(vec![second, blocks[0].clone()], "remote").unwrap()
    );
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].cost_usd, Decimal::from_str("1.75").unwrap());
    assert_eq!(points[0].models, vec!["a", "m", "z"]);
    assert_eq!(points[0].machine, "remote");

    let daily = br#"{"daily":[{"period":"2026-11-26","totalCost":999,"agents":[{"agent":"claude","modelBreakdowns":[{"modelName":"m","cost":0.12,"inputTokens":1,"outputTokens":2,"cacheCreationTokens":3,"cacheReadTokens":4},{"modelName":"n","cost":0.2,"inputTokens":5,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0}]}]}]}"#;
    let rows = RemoteCCUsageClient::decode_daily(daily, "source").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().map(|r| r.cost_usd).sum::<Decimal>(),
        Decimal::from_str("0.32").unwrap()
    );
    let metrics = coalesce_metrics(
        vec![rows[1].clone(), rows[0].clone(), rows[0].clone()],
        "remote",
    )
    .unwrap();
    let reversed_metrics = coalesce_metrics(
        vec![rows[0].clone(), rows[0].clone(), rows[1].clone()],
        "remote",
    )
    .unwrap();
    assert_eq!(metrics, reversed_metrics);
    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[0].model, "m");
    assert_eq!(metrics[0].cost_usd, Decimal::from_str("0.24").unwrap());
    assert_eq!(
        (
            metrics[0].input_tokens,
            metrics[0].output_tokens,
            metrics[0].cache_creation_tokens,
            metrics[0].cache_read_tokens,
            metrics[0].total_tokens
        ),
        (2, 4, 6, 8, 20)
    );
    assert!(metrics.iter().all(|r| r.machine == "remote"));

    let sessions = br#"{"session":[{"agent":"claude","totalCost":999,"metadata":{"lastActivity":"2026-11-26T00:00:00Z"},"modelBreakdowns":[{"modelName":"m","cost":0.1,"inputTokens":1,"outputTokens":0,"cacheCreationTokens":0,"cacheReadTokens":0},{"modelName":"n","cost":0.2,"inputTokens":0,"outputTokens":2,"cacheCreationTokens":0,"cacheReadTokens":0}]}]}"#;
    let rows = RemoteCCUsageClient::decode_sessions(sessions, "source").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].cost_usd, Decimal::from_str("0.1").unwrap());
    assert_eq!(rows[0].total_tokens, 1);
    assert_eq!(rows[0].data_quality, UsageDataQuality::SessionEstimated);
    assert_eq!(rows[0].machine, "source");
    assert_eq!(
        rows.iter().map(|r| r.cost_usd).sum::<Decimal>(),
        Decimal::from_str("0.3").unwrap()
    );
    let merged = coalesce_sessions(
        vec![rows[1].clone(), rows[0].clone(), rows[0].clone()],
        "remote",
    )
    .unwrap();
    assert_eq!(
        merged,
        coalesce_sessions(
            vec![rows[0].clone(), rows[0].clone(), rows[1].clone()],
            "remote"
        )
        .unwrap()
    );
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].model, "m");
    assert_eq!(merged[0].cost_usd, Decimal::from_str("0.2").unwrap());
    assert_eq!(
        (
            merged[0].input_tokens,
            merged[0].output_tokens,
            merged[0].cache_creation_tokens,
            merged[0].cache_read_tokens,
            merged[0].total_tokens
        ),
        (2, 0, 0, 0, 2)
    );
    assert!(merged.iter().all(|r| r.machine == "remote"));
}

#[test]
fn empty_daily_does_not_lock_compatibility_mode() {
    let mut client = RemoteCCUsageClient::new();
    let mut modes = Vec::new();
    assert!(client
        .daily("remote", |mode| {
            modes.push(mode);
            Ok(br#"{"daily":[]}"#.to_vec())
        })
        .unwrap()
        .is_empty());
    assert_eq!(modes, vec![false]);
    modes.clear();
    let result = client
        .daily("remote", |mode| {
            modes.push(mode);
            Ok(if mode {
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

#[test]
fn direct_and_command_argv_are_exactly_structured() {
    use ai_usage_core::remote::ssh_runner::{SSHCCUsageCommandRunner, TokioProcessExecutor};
    use ai_usage_core::types::SSHProxy;
    let mut connection = descriptor().ssh.unwrap();
    let args = SSHCCUsageCommandRunner::new(connection.clone(), TokioProcessExecutor)
        .ssh_arguments(
            &["daily".into(), "--json".into()],
            std::time::Duration::from_secs(3),
        )
        .unwrap();
    assert!(args.windows(2).any(|x| x == ["-F", "/dev/null"]));
    for value in [
        "BatchMode=yes",
        "IdentitiesOnly=yes",
        "StrictHostKeyChecking=yes",
    ] {
        assert!(args.iter().any(|x| x == value));
    }
    assert!(!args.iter().any(|x| x.starts_with("ProxyCommand=")));
    assert_eq!(
        &args[args.len() - 5..],
        ["--", "user@example.com", "'ccusage'", "'daily'", "'--json'"]
    );
    connection.proxy = Some(SSHProxy::Command {
        executable: "/opt/proxy command".into(),
    });
    let args = SSHCCUsageCommandRunner::new(connection.clone(), TokioProcessExecutor)
        .ssh_arguments(&[], std::time::Duration::from_secs(3))
        .unwrap();
    assert!(args
        .iter()
        .any(|x| x
            == "ProxyCommand='/opt/proxy command' 'connect' '--host' 'example.com' '--port' '22'"));
    connection.proxy = Some(SSHProxy::Command {
        executable: "/opt/%h".into(),
    });
    assert!(
        SSHCCUsageCommandRunner::new(connection, TokioProcessExecutor)
            .ssh_arguments(&[], std::time::Duration::from_secs(3))
            .is_err()
    );
}

#[tokio::test]
async fn sourced_adapter_isolates_agent_environments() {
    use ai_usage_core::remote::ssh_runner::SSHCCUsageCommandRunner;
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("sessions")).unwrap();
    std::fs::create_dir(temp.path().join("projects")).unwrap();
    let root = temp.path().to_str().unwrap();
    let mut connection = descriptor().ssh.unwrap();
    connection.remote_ccusage_path = "sh".into();
    let runner = SSHCCUsageCommandRunner::new(connection, LocalProbeExecutor);
    let args = vec![
        "-c".into(),
        "printf '%s\\n%s' \"$CODEX_HOME\" \"$CLAUDE_CONFIG_DIR\"".into(),
    ];
    for agent in ["codex", "claude"] {
        let output = runner
            .run_source(
                &args,
                agent,
                "explicit",
                root,
                "",
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        let text = std::str::from_utf8(&output.stdout).unwrap();
        let (codex, claude) = text.split_once('\n').unwrap();
        if agent == "codex" {
            assert_eq!(codex, root);
            assert!(claude.ends_with("/ai-usage-gauge-disabled/claude"));
        } else {
            assert_eq!(claude, root);
            assert!(codex.ends_with("/ai-usage-gauge-disabled/codex"));
        }
    }
}

#[tokio::test]
async fn unsafe_local_files_and_executable_fail_before_launch() {
    use ai_usage_core::remote::ssh_runner::{FailurePhase, SSHCCUsageCommandRunner};
    use ai_usage_core::types::{SSHJumpProxy, SSHProxy};
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = temp.path().join("key");
    let link = temp.path().join("link");
    let proxy = temp.path().join("proxy");
    std::fs::write(&identity, "key").unwrap();
    std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o644)).unwrap();
    symlink(&identity, &link).unwrap();
    std::fs::write(&proxy, "proxy").unwrap();
    std::fs::set_permissions(&proxy, std::fs::Permissions::from_mode(0o777)).unwrap();
    let mut cases = Vec::new();
    for path in [&identity, &link] {
        let mut c = descriptor().ssh.unwrap();
        c.identity_file = Some(path.to_str().unwrap().into());
        cases.push(c);
    }
    let mut c = descriptor().ssh.unwrap();
    c.proxy = Some(SSHProxy::Jump(SSHJumpProxy {
        host: "jump.example".into(),
        port: 22,
        user: "bob".into(),
        identity_file: Some(identity.to_str().unwrap().into()),
        known_hosts_file: None,
    }));
    cases.push(c);
    let mut c = descriptor().ssh.unwrap();
    c.proxy = Some(SSHProxy::Command {
        executable: proxy.to_str().unwrap().into(),
    });
    cases.push(c);
    let mut c = descriptor().ssh.unwrap();
    c.remote_ccusage_path = "ccusage;bad".into();
    cases.push(c);
    for connection in cases {
        let mut machine = descriptor();
        machine.ssh = Some(connection.clone());
        let path = temp.path().join("machines.json");
        let store = MachineRegistryStore::new(path.clone());
        let mut registry = store.load().unwrap();
        registry.machines.push(machine.clone());
        assert!(store.save(&registry).is_err());
        std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.load().is_err());
        std::fs::remove_file(&path).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runner = SSHCCUsageCommandRunner::new(
            connection,
            FailingExecutor {
                phase: FailurePhase::Transport,
                calls: calls.clone(),
            },
        );
        let timeout = std::time::Duration::from_secs(1);
        assert_eq!(
            runner.run(&[], timeout).await.unwrap_err().phase,
            FailurePhase::Internal
        );
        assert_eq!(
            runner
                .run_source(&[], "codex", "explicit", "/tmp/source", "", timeout)
                .await
                .unwrap_err()
                .phase,
            FailurePhase::Internal
        );
        assert_eq!(
            runner
                .probe_sources(&machine, timeout)
                .await
                .unwrap_err()
                .phase,
            FailurePhase::Internal
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
struct SaveAndCancelFail(std::path::PathBuf);
impl DirectoryNameLifecycle for SaveAndCancelFail {
    fn purge_for_reuse(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn prepare_delete(&self, _: &str) -> Result<(), String> {
        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        Ok(())
    }
    fn commit_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn rollback_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn reconcile_machine_registry(&self, _: &[String]) -> Result<(), String> {
        Err("cancel failed".into())
    }
    fn purge_deleted(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn latched_owner_rejects_every_mutation_before_validation_or_closure() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = MachineRegistryStore::new(temp.path().join("machines.json"));
    let mut initial = store.load().unwrap();
    initial.machines.push(descriptor());
    store.save(&initial).unwrap();
    let owner = MachineRegistryMutationOwner::new(
        store,
        initial,
        Arc::new(Runtime {
            failures: AtomicUsize::new(2),
        }),
        Arc::new(Metadata),
    );
    assert_eq!(
        owner.patch("remote-a", |m| m.enabled = false).await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: true
        })
    );
    let mut invalid = descriptor();
    invalid.id = "invalid id".into();
    for input in [descriptor(), invalid] {
        assert_eq!(
            owner.create(input).await,
            Err(MutationError::ReconciliationRequired)
        );
    }
    for id in ["local", "unknown"] {
        assert_eq!(
            owner.replace(id, descriptor()).await,
            Err(MutationError::ReconciliationRequired)
        );
    }
    let invoked = AtomicUsize::new(0);
    assert_eq!(
        owner
            .patch("unknown", |_| {
                invoked.fetch_add(1, Ordering::SeqCst);
            })
            .await,
        Err(MutationError::ReconciliationRequired)
    );
    assert_eq!(
        owner
            .patch_local_with(|s| {
                invoked.fetch_add(1, Ordering::SeqCst);
                s.codex_session_dirs.push("relative".into());
            })
            .await,
        Err(MutationError::ReconciliationRequired)
    );
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    for id in ["local", "unknown"] {
        assert_eq!(
            owner.delete(id).await,
            Err(MutationError::ReconciliationRequired)
        );
    }
}

#[tokio::test]
async fn failed_save_and_cancellation_reports_triggering_failure_then_latches() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = MachineRegistryStore::new(temp.path().join("machines.json"));
    let mut initial = store.load().unwrap();
    initial.machines.push(descriptor());
    store.save(&initial).unwrap();
    let owner = MachineRegistryMutationOwner::new(
        store,
        initial,
        Arc::new(Runtime {
            failures: AtomicUsize::new(0),
        }),
        Arc::new(SaveAndCancelFail(temp.path().into())),
    );
    assert_eq!(
        owner.delete("remote-a").await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: true
        })
    );
    assert_eq!(
        owner.delete("unknown").await,
        Err(MutationError::ReconciliationRequired)
    );
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[derive(Default)]
struct RevisionRuntime {
    revisions: std::sync::Mutex<Vec<u64>>,
    failures: AtomicUsize,
    break_restore: std::sync::Mutex<Option<std::path::PathBuf>>,
}
impl MachineRuntimeReconciler for RevisionRuntime {
    fn reconcile_registry<'a>(
        &'a self,
        _: &'a MachineRegistry,
        revision: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.revisions.lock().unwrap().push(revision);
            if self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                if let Some(path) = self.break_restore.lock().unwrap().take() {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
                }
                Err("runtime failure".into())
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn real_directory_store_preserves_names_and_revisioned_rollback() {
    use ai_usage_core::cache::DashboardDirectoryNameStore;
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().join("machines.json");
    let db = temp.path().join("names.sqlite3");
    let store = MachineRegistryStore::new(path.clone());
    let initial = store.load().unwrap();
    let names = Arc::new(DashboardDirectoryNameStore::new(db.clone()));
    let runtime = Arc::new(RevisionRuntime::default());
    let owner = MachineRegistryMutationOwner::new(store, initial, runtime.clone(), names.clone());
    owner.create(descriptor()).await.unwrap();
    names
        .set_name("local", "/local", Some("Local"), 1.0)
        .unwrap();
    names
        .set_name("remote-a", "/remote", Some("Remote"), 1.0)
        .unwrap();
    runtime.failures.store(1, Ordering::SeqCst);
    assert_eq!(
        owner.delete("remote-a").await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: false
        })
    );
    assert_eq!(
        MachineRegistryStore::new(path.clone())
            .load()
            .unwrap()
            .machines,
        vec![descriptor()]
    );
    let ids = vec!["local".to_owned(), "remote-a".to_owned()];
    assert_eq!(names.names(&ids).unwrap()["remote-a"]["/remote"], "Remote");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let markers: i64 = conn.query_row("SELECT count(*) FROM dashboard_directory_name_machine_deletions WHERE machine_id='remote-a'", [], |row| row.get(0)).unwrap();
    assert_eq!(markers, 0);
    owner.delete("remote-a").await.unwrap();
    let rows = names.names(&ids).unwrap();
    assert!(!rows.contains_key("remote-a"));
    assert_eq!(rows["local"]["/local"], "Local");
    assert_eq!(*runtime.revisions.lock().unwrap(), vec![1, 2, 3, 4]);

    owner.create(descriptor()).await.unwrap();
    names
        .set_name("remote-a", "/remote", Some("Again"), 1.0)
        .unwrap();
    *runtime.break_restore.lock().unwrap() = Some(path.clone());
    runtime.failures.store(1, Ordering::SeqCst);
    assert_eq!(
        owner.delete("remote-a").await,
        Err(MutationError::ReconciliationFailed {
            reconciliation_required: true
        })
    );
    let phase: String = conn.query_row("SELECT phase FROM dashboard_directory_name_machine_deletions WHERE machine_id='remote-a'", [], |row| row.get(0)).unwrap();
    assert_eq!(phase, "rollback");
    assert_eq!(
        owner.delete("remote-a").await,
        Err(MutationError::ReconciliationRequired)
    );
    assert_eq!(
        *runtime.revisions.lock().unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7]
    );
}
