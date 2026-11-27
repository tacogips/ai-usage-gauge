use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use rust_decimal::Decimal;

use crate::aggregation::rolling::{selected_period_cost_ref, RollingInputsRef};
use crate::state::ResetWindowCalculator;
use crate::types::{
    AppState, BudgetSummary, CostRecord, CostSnapshot, DashboardDataDisposition, DashboardScope,
    MachineAvailability, MachineAvailabilityReason, MachineCollectionState, MachineDataGap,
    MachineLatestEvent, MachineStatus, MetricRecord, SessionMetricRecord, UsageDataQuality,
};

#[derive(Debug, Clone)]
pub struct MachineSnapshotInput {
    pub status: MachineStatus,
    pub snapshot: Option<CostSnapshot>,
}

#[derive(Debug, Clone)]
pub struct MergedDashboardSnapshot {
    pub snapshot: CostSnapshot,
    pub scope: DashboardScope,
    pub machine_latest_events: Vec<MachineLatestEvent>,
}

pub struct CostSnapshotMerge;

impl CostSnapshotMerge {
    pub fn merge(
        inputs: &[MachineSnapshotInput],
        state: &AppState,
        requested: &str,
        disposition: DashboardDataDisposition,
        now: DateTime<Utc>,
        tz: Tz,
    ) -> MergedDashboardSnapshot {
        let calc = ResetWindowCalculator::new(tz);
        let interval = calc
            .aggregation_interval(state.reset_cycle.clone(), now)
            .unwrap_or((now - Duration::days(1), now));

        let mut included_machine_ids = Vec::new();
        let mut stale_machine_ids = Vec::new();
        let mut unavailable_machine_ids = Vec::new();
        let mut excluded_from_current_totals_machine_ids = Vec::new();
        let mut machine_availability = Vec::new();
        let mut last_hour_data_gaps = Vec::new();
        let mut machine_latest_events = Vec::new();

        let mut points: Vec<CostRecord> = Vec::new();
        let mut metrics: Vec<MetricRecord> = Vec::new();
        let mut sessions: Vec<SessionMetricRecord> = Vec::new();
        let mut rolling_events = Vec::new();
        let mut latest_generated_at: Option<DateTime<Utc>> = None;

        let one_hour_ago = now - Duration::hours(1);

        for input in inputs {
            let status = &input.status;
            let id = status.id.clone();

            // Always derive machine_latest_events independently of eligibility.
            let latest_event =
                Self::derive_latest_event(&id, input.snapshot.as_ref(), one_hour_ago);
            machine_latest_events.push(latest_event);

            if !status.enabled {
                continue;
            }

            let is_healthy = status.collection_state == MachineCollectionState::Healthy
                && !status.stale
                && input.snapshot.is_some();

            let eligible = match disposition {
                DashboardDataDisposition::Current => is_healthy,
                DashboardDataDisposition::Historical => input.snapshot.is_some(),
            };

            if status.stale || status.collection_state == MachineCollectionState::Stale {
                stale_machine_ids.push(id.clone());
            }
            if !is_healthy {
                unavailable_machine_ids.push(id.clone());
                if disposition == DashboardDataDisposition::Current {
                    excluded_from_current_totals_machine_ids.push(id.clone());
                }
                let reason = Self::availability_reason(status);
                let since = status
                    .unavailable_since
                    .or(status.stale_since)
                    .or(status.last_success_at)
                    .unwrap_or(now);
                machine_availability.push(MachineAvailability {
                    machine: id.clone(),
                    available: false,
                    unavailable_since: since,
                    reason_code: reason,
                });
                if since < now {
                    let gap_start = since.max(one_hour_ago);
                    if gap_start < now {
                        last_hour_data_gaps.push(MachineDataGap {
                            machine: id.clone(),
                            start_at: gap_start,
                            end_at: now,
                            reason_code: reason,
                        });
                    }
                }
            } else {
                machine_availability.push(MachineAvailability {
                    machine: id.clone(),
                    available: true,
                    unavailable_since: now,
                    reason_code: MachineAvailabilityReason::CollectionStale,
                });
            }

            if eligible {
                if let Some(snap) = &input.snapshot {
                    included_machine_ids.push(id.clone());
                    if latest_generated_at.is_none_or(|g| snap.generated_at > g) {
                        latest_generated_at = Some(snap.generated_at);
                    }
                    for mut p in snap.points.clone() {
                        p.machine = id.clone();
                        points.push(p);
                    }
                    for mut m in snap.dashboard_metrics.clone() {
                        m.machine = id.clone();
                        metrics.push(m);
                    }
                    for mut s in snap.dashboard_sessions.clone() {
                        s.machine = id.clone();
                        sessions.push(s);
                    }
                    if id == "local" {
                        rolling_events.extend(snap.rolling_events.clone());
                    }
                }
            }
        }

        points.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then(a.machine.cmp(&b.machine))
        });
        metrics.sort_by(|a, b| {
            a.date
                .cmp(&b.date)
                .then(a.machine.cmp(&b.machine))
                .then(a.agent.cmp(&b.agent))
                .then(a.model.cmp(&b.model))
        });
        sessions.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then(a.machine.cmp(&b.machine))
                .then(a.agent.cmp(&b.agent))
                .then(a.model.cmp(&b.model))
        });

        let today = now.with_timezone(&tz).format("%Y-%m-%d").to_string();
        let floor = (now.with_timezone(&tz).date_naive() - Duration::days(8))
            .format("%Y-%m-%d")
            .to_string();

        // Filter to budgetMachineIDs if configured; otherwise borrow merged slices directly.
        let empty_legacy = BTreeSet::new();
        let cost_since_reset = if state.budget_machine_ids.is_empty() {
            let rolling_inputs = RollingInputsRef {
                local_raw_events: &rolling_events,
                daily_metrics: &metrics,
                session_metrics: &sessions,
                legacy_local_days: &empty_legacy,
                today: &today,
                retention_floor: &floor,
            };
            selected_period_cost_ref(&state.reset_cycle, interval, &rolling_inputs, &[], tz)
                .unwrap_or(Decimal::ZERO)
        } else {
            let budget_sessions: Vec<SessionMetricRecord> = sessions
                .iter()
                .filter(|s| state.budget_machine_ids.contains(&s.machine))
                .cloned()
                .collect();
            let budget_metrics: Vec<MetricRecord> = metrics
                .iter()
                .filter(|m| state.budget_machine_ids.contains(&m.machine))
                .cloned()
                .collect();
            let budget_rolling: &[crate::types::PricedUsageEvent] =
                if state.budget_machine_ids.iter().any(|m| m == "local") {
                    &rolling_events
                } else {
                    &[]
                };
            let rolling_inputs = RollingInputsRef {
                local_raw_events: budget_rolling,
                daily_metrics: &budget_metrics,
                session_metrics: &budget_sessions,
                legacy_local_days: &empty_legacy,
                today: &today,
                retention_floor: &floor,
            };
            selected_period_cost_ref(&state.reset_cycle, interval, &rolling_inputs, &[], tz)
                .unwrap_or(Decimal::ZERO)
        };
        let budget = BudgetSummary::new(cost_since_reset, state.budget_usd);

        let snapshot = CostSnapshot {
            generated_at: latest_generated_at.unwrap_or(now),
            active_boundary_at: interval.0,
            cost_since_reset_usd: cost_since_reset,
            budget,
            reset_cycle: state.reset_cycle.clone(),
            refresh_interval_seconds: state.refresh_interval_seconds.unwrap_or(30),
            points,
            dashboard_metrics: metrics,
            dashboard_sessions: sessions,
            rolling_events,
        };

        let scope = DashboardScope {
            requested: requested.to_owned(),
            data_disposition: disposition,
            included_machine_ids,
            stale_machine_ids,
            unavailable_machine_ids,
            excluded_from_current_totals_machine_ids,
            machine_availability,
            last_hour_data_gaps,
            evaluated_at: now,
            generated_at: latest_generated_at,
        };

        MergedDashboardSnapshot {
            snapshot,
            scope,
            machine_latest_events,
        }
    }

    fn derive_latest_event(
        machine_id: &str,
        snapshot: Option<&CostSnapshot>,
        one_hour_ago: DateTime<Utc>,
    ) -> MachineLatestEvent {
        let Some(snap) = snapshot else {
            return MachineLatestEvent {
                machine: machine_id.to_owned(),
                latest_event_at: None,
                marker_state: "none".into(),
                in_last_hour: false,
                data_quality: None,
            };
        };
        let latest = snap
            .dashboard_sessions
            .iter()
            .max_by_key(|s| s.timestamp)
            .map(|s| (s.timestamp, s.data_quality));
        match latest {
            Some((ts, quality)) => MachineLatestEvent {
                machine: machine_id.to_owned(),
                latest_event_at: Some(ts),
                marker_state: if ts >= one_hour_ago {
                    "recent".into()
                } else {
                    "older".into()
                },
                in_last_hour: ts >= one_hour_ago,
                data_quality: Some(quality),
            },
            None => MachineLatestEvent {
                machine: machine_id.to_owned(),
                latest_event_at: None,
                marker_state: "none".into(),
                in_last_hour: false,
                data_quality: Some(UsageDataQuality::Timestamped),
            },
        }
    }

    fn availability_reason(status: &MachineStatus) -> MachineAvailabilityReason {
        if status.collection_state == MachineCollectionState::NeverCollected {
            return MachineAvailabilityReason::NeverCollected;
        }
        if let Some(err) = &status.last_error {
            return match err.code.as_str() {
                "auth_failed" => MachineAvailabilityReason::AuthFailed,
                "host_key_verification_failed" => {
                    MachineAvailabilityReason::HostKeyVerificationFailed
                }
                "timeout" => MachineAvailabilityReason::Timeout,
                "tunnel_unreachable" => MachineAvailabilityReason::TunnelUnreachable,
                "transport_failed" => MachineAvailabilityReason::TransportFailed,
                "remote_command_failed" => MachineAvailabilityReason::RemoteCommandFailed,
                "invalid_response" => MachineAvailabilityReason::InvalidResponse,
                "cache_failed" => MachineAvailabilityReason::CacheFailed,
                "executable_unavailable" => MachineAvailabilityReason::ExecutableUnavailable,
                _ => MachineAvailabilityReason::InternalError,
            };
        }
        MachineAvailabilityReason::CollectionStale
    }
}
