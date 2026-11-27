use crate::types::MachineDiagnostic;

use super::ssh_runner::{CommandFailure, FailurePhase};

pub struct MachineDiagnosticClassifier;

impl MachineDiagnosticClassifier {
    pub fn classify(failure: &CommandFailure) -> MachineDiagnostic {
        let input = String::from_utf8_lossy(&failure.stderr[..failure.stderr.len().min(4096)])
            .to_ascii_lowercase();
        let mut stderr = String::new();
        let mut space = false;
        for c in input.chars() {
            if c.is_whitespace() || c.is_ascii_control() {
                space = !stderr.is_empty();
            } else {
                if space {
                    stderr.push(' ');
                }
                space = false;
                stderr.push(c);
            }
        }
        let contains = |signatures: &[&str]| signatures.iter().any(|s| stderr.contains(s));
        let code = match failure.phase {
            FailurePhase::Timeout => "timeout",
            FailurePhase::RemoteCommand => "remote_command_failed",
            FailurePhase::InvalidResponse => "invalid_response",
            FailurePhase::Cache => "cache_failed",
            FailurePhase::Internal => "internal_error",
            FailurePhase::Transport
                if contains(&[
                    "host key verification failed",
                    "remote host identification has changed",
                    "offending ecdsa key in",
                    "offending ed25519 key in",
                    "offending rsa key in",
                    "no host key is known for",
                ]) =>
            {
                "host_key_verification_failed"
            }
            FailurePhase::Transport
                if contains(&[
                    "permission denied",
                    "authentication failed",
                    "no supported authentication methods available",
                    "too many authentication failures",
                    "sign_and_send_pubkey: signing failed",
                ]) =>
            {
                "auth_failed"
            }
            FailurePhase::Transport
                if contains(&["connection timed out", "operation timed out"]) =>
            {
                "timeout"
            }
            FailurePhase::Transport
                if contains(&[
                    "connection refused",
                    "no route to host",
                    "could not resolve hostname",
                    "name or service not known",
                    "connection reset by peer",
                    "connection closed by",
                    "kex_exchange_identification",
                    "stdio forwarding failed",
                ]) =>
            {
                "tunnel_unreachable"
            }
            FailurePhase::Transport => "transport_failed",
        };
        Self::diagnostic(code)
    }

    pub fn diagnostic(code: &str) -> MachineDiagnostic {
        let (code, message, detail, remediation) = match code {
            "host_key_verification_failed" => (code, "SSH host-key verification failed", "The SSH server identity could not be verified.", "Verify the server fingerprint with the machine administrator, then update the configured known-hosts file."),
            "auth_failed" => (code, "SSH authentication failed", "The SSH server rejected the configured credentials.", "Verify the configured user, identity-file reference and permissions, and server-side authorization."),
            "tunnel_unreachable" => (code, "SSH tunnel is unreachable", "The configured SSH endpoint did not accept a connection.", "Verify that the configured proxy or tunnel is running and that its host and port match the active endpoint."),
            "timeout" => (code, "Connection timed out", "The command did not complete before the configured timeout.", "Verify endpoint responsiveness and the configured connection timeout, then retry."),
            "remote_command_failed" => (code, "ccusage command failed", "The configured ccusage executable rejected the requested operation.", "Verify the remote ccusage installation and supported version."),
            "transport_failed" => (code, "Command transport failed", "The command could not be started or completed.", "Verify the executable and connection configuration, then retry."),
            "invalid_response" => (code, "ccusage response was invalid", "ccusage returned an incompatible response.", "Verify the installed ccusage version and retry."),
            "cache_failed" => (code, "Usage cache operation failed", "The host usage cache could not be used.", "Inspect the persistent log and verify state and cache directory ownership."),
            _ => ("internal_error", "Collection failed", "Collection failed without a safe specific diagnostic.", "Inspect the persistent log and retry."),
        };
        MachineDiagnostic {
            code: code.into(),
            message: message.into(),
            detail: Some(detail.into()),
            remediation: Some(remediation.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_classification_is_ordered_and_sanitized() {
        let failure = CommandFailure {
            phase: FailurePhase::Transport,
            exit_status: Some(255),
            stderr: b"secret-host permission denied; host key verification failed".to_vec(),
        };
        let diagnostic = MachineDiagnosticClassifier::classify(&failure);
        assert_eq!(diagnostic.code, "host_key_verification_failed");
        assert!(!format!("{diagnostic:?}").contains("secret-host"));
        let remote = CommandFailure {
            phase: FailurePhase::RemoteCommand,
            ..failure
        };
        assert_eq!(
            MachineDiagnosticClassifier::classify(&remote).code,
            "remote_command_failed"
        );
    }

    #[test]
    fn normalized_signatures_and_boundary_are_classified_safely() {
        for (stderr, expected) in [
            (
                "Host key\r\nverification\tfailed secret",
                "host_key_verification_failed",
            ),
            ("Connection   refused secret", "tunnel_unreachable"),
            ("permission\0denied secret", "auth_failed"),
        ] {
            let failure = CommandFailure {
                phase: FailurePhase::Transport,
                exit_status: None,
                stderr: stderr.as_bytes().to_vec(),
            };
            let result = MachineDiagnosticClassifier::classify(&failure);
            assert_eq!(result.code, expected);
            assert!(!format!("{result:?}").contains("secret"));
        }
        let mut stderr = vec![b'x'; 4096];
        stderr.extend_from_slice(b" permission denied");
        let failure = CommandFailure {
            phase: FailurePhase::Transport,
            exit_status: None,
            stderr,
        };
        assert_eq!(
            MachineDiagnosticClassifier::classify(&failure).code,
            "transport_failed"
        );
    }

    #[test]
    fn diagnostic_table_is_exact() {
        let rows = [
            ("host_key_verification_failed", "SSH host-key verification failed", "The SSH server identity could not be verified.", "Verify the server fingerprint with the machine administrator, then update the configured known-hosts file."),
            ("auth_failed", "SSH authentication failed", "The SSH server rejected the configured credentials.", "Verify the configured user, identity-file reference and permissions, and server-side authorization."),
            ("tunnel_unreachable", "SSH tunnel is unreachable", "The configured SSH endpoint did not accept a connection.", "Verify that the configured proxy or tunnel is running and that its host and port match the active endpoint."),
            ("timeout", "Connection timed out", "The command did not complete before the configured timeout.", "Verify endpoint responsiveness and the configured connection timeout, then retry."),
            ("remote_command_failed", "ccusage command failed", "The configured ccusage executable rejected the requested operation.", "Verify the remote ccusage installation and supported version."),
            ("transport_failed", "Command transport failed", "The command could not be started or completed.", "Verify the executable and connection configuration, then retry."),
            ("invalid_response", "ccusage response was invalid", "ccusage returned an incompatible response.", "Verify the installed ccusage version and retry."),
            ("cache_failed", "Usage cache operation failed", "The host usage cache could not be used.", "Inspect the persistent log and verify state and cache directory ownership."),
            ("internal_error", "Collection failed", "Collection failed without a safe specific diagnostic.", "Inspect the persistent log and retry."),
        ];
        for (code, message, detail, remediation) in rows {
            let d = MachineDiagnosticClassifier::diagnostic(code);
            assert_eq!(
                (
                    d.code.as_str(),
                    d.message.as_str(),
                    d.detail.as_deref(),
                    d.remediation.as_deref()
                ),
                (code, message, Some(detail), Some(remediation))
            );
        }
    }
}
