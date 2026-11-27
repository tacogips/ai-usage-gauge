use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use ai_usage_core::types::{
    AppState, CostSnapshot, MenuBarStatusEntry, PieChartId, RateLimitProvider, RateLimitsResponse,
};

use crate::tray_icons::TrayRenderState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageIconMenuItem {
    pub id: PieChartId,
    pub label: String,
    pub checked: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuStructureFingerprint {
    pub selected_charts: Vec<PieChartId>,
    pub has_error: bool,
    pub has_custom_refresh: bool,
    pub launch_requires_approval: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrayViewModel {
    pub render_state: TrayRenderState,
    pub status_title: String,
    pub tooltip: String,
    pub budget_summary_line: String,
    pub rate_limit_lines: Vec<String>,
    pub usage_icon_items: Vec<UsageIconMenuItem>,
    pub dashboard_toggle_label: String,
    pub refresh_interval_label: String,
    pub structure: MenuStructureFingerprint,
}

impl TrayViewModel {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        state: &AppState,
        snapshot: Option<&CostSnapshot>,
        rate_limits: Option<&RateLimitsResponse>,
        error_message: Option<&str>,
        dashboard_open: bool,
        config_poll_interval: u32,
        launch_requires_approval: bool,
    ) -> Self {
        let has_error = error_message.is_some();
        let effective_refresh = state
            .refresh_interval_seconds
            .unwrap_or(config_poll_interval);

        let status_title = if !state.menu_bar_show_cost {
            String::new()
        } else {
            match (snapshot, has_error) {
                (Some(s), false) => format_cost_title(s.cost_since_reset_usd, false),
                (Some(s), true) => format_cost_title(s.cost_since_reset_usd, true),
                (None, true) => "$!".into(),
                (None, false) => "$—".into(),
            }
        };

        let budget_fraction =
            snapshot.and_then(|s| s.budget.visual_fraction.and_then(|d| d.to_f64()));
        let is_budget_unset = state.budget_usd.is_none();
        let is_budget_unknown = snapshot.is_none() && !is_budget_unset;

        let mut entries = Vec::new();
        for id in PieChartId::menu_bar_cases() {
            let show_icon = !state.menu_bar_cost_only && state.menu_bar_pie_charts.contains(&id);
            let show_ratio = state.menu_bar_ratio_labels.contains(&id);
            if !show_icon && !show_ratio {
                continue;
            }
            let fraction = match id {
                PieChartId::Budget => budget_fraction,
                _ => rate_limit_fraction(rate_limits, id),
            };
            let ratio_label = if show_ratio {
                fraction.map(|f| format!("{}%", (f * 100.0).round() as i64))
            } else {
                None
            };
            entries.push(MenuBarStatusEntry {
                id,
                fraction,
                show_icon,
                ratio_label,
                is_budget_unset: id == PieChartId::Budget && is_budget_unset,
                is_budget_unknown: id == PieChartId::Budget && is_budget_unknown,
            });
        }

        let render_state = TrayRenderState {
            entries,
            is_usage_unavailable: snapshot.is_none(),
            has_warnings: has_error,
        };

        let budget_summary_line = match snapshot {
            Some(s) => match s.budget.budget_usd {
                Some(b) => format!("Spent ${:.2} / ${:.2}", s.cost_since_reset_usd, b),
                None => format!("Spent ${:.2} (no budget set)", s.cost_since_reset_usd),
            },
            None => "Usage unavailable".into(),
        };

        let mut rate_limit_lines = Vec::new();
        let mut credit_lines = Vec::new();
        if let Some(rl) = rate_limits {
            for p in &rl.providers {
                let name = match p.provider {
                    RateLimitProvider::Claude => "Claude Code",
                    RateLimitProvider::Codex => "Codex",
                    RateLimitProvider::Gemini => "Gemini",
                };
                if let Some(w) = &p.five_hour {
                    rate_limit_lines.push(format!("{name} 5h: {:.0}%", w.used_percent));
                }
                if let Some(w) = &p.weekly {
                    rate_limit_lines.push(format!("{name} weekly: {:.0}%", w.used_percent));
                }
                if let Some(c) = &p.credits {
                    if c.enabled {
                        if let Some(pct) = c.used_percent {
                            credit_lines.push(format!("{name} credit: {pct:.0}%"));
                        } else if let Some(bal) = c.balance {
                            credit_lines.push(format!("{name} credit: {bal:.0} balance"));
                        }
                    }
                }
            }
        }

        let tooltip = if let Some(err) = error_message {
            format!("AI Usage Gauge — Warning: {err}")
        } else {
            let mut lines = vec![format!("AI Usage Gauge — {budget_summary_line}")];
            lines.extend(rate_limit_lines.iter().cloned());
            lines.extend(credit_lines);
            lines.join("\n")
        };

        let checked_count = state.menu_bar_pie_charts.len();
        let usage_icon_items = PieChartId::menu_bar_cases()
            .into_iter()
            .map(|id| {
                let checked = state.menu_bar_pie_charts.contains(&id);
                UsageIconMenuItem {
                    id,
                    label: pie_chart_label(id).to_owned(),
                    checked,
                    // Disable unchecking when it is the sole remaining checked icon.
                    enabled: !(checked && checked_count <= 1),
                }
            })
            .collect();

        let dashboard_toggle_label = if dashboard_open {
            "Close dashboard".into()
        } else {
            "Open dashboard window".into()
        };

        let refresh_interval_label = format!("Every {effective_refresh} seconds");

        let structure = MenuStructureFingerprint {
            selected_charts: state.menu_bar_pie_charts.clone(),
            has_error,
            has_custom_refresh: state.refresh_interval_seconds.is_some(),
            launch_requires_approval,
        };

        Self {
            render_state,
            status_title,
            tooltip,
            budget_summary_line,
            rate_limit_lines,
            usage_icon_items,
            dashboard_toggle_label,
            refresh_interval_label,
            structure,
        }
    }
}

fn format_cost_title(cost: Decimal, warning: bool) -> String {
    let rounded = cost.round_dp(0);
    if warning {
        format!("${rounded} !")
    } else {
        format!("${rounded}")
    }
}

fn rate_limit_fraction(rl: Option<&RateLimitsResponse>, id: PieChartId) -> Option<f64> {
    let rl = rl?;
    let target_provider = id.provider()?;
    let p = rl
        .providers
        .iter()
        .find(|p| p.provider == target_provider)?;
    match id {
        PieChartId::Claude5h | PieChartId::Codex5h | PieChartId::Gemini5h => {
            p.five_hour.as_ref().map(|w| w.used_percent / 100.0)
        }
        PieChartId::ClaudeWeekly | PieChartId::CodexWeekly | PieChartId::GeminiWeekly => {
            p.weekly.as_ref().map(|w| w.used_percent / 100.0)
        }
        PieChartId::ClaudeCredit | PieChartId::CodexCredit => p
            .credits
            .as_ref()
            .and_then(|c| c.used_percent)
            .map(|v| v / 100.0),
        PieChartId::Budget => None,
    }
}

fn pie_chart_label(id: PieChartId) -> &'static str {
    match id {
        PieChartId::Budget => "Budget",
        PieChartId::Claude5h => "Claude 5h",
        PieChartId::ClaudeWeekly => "Claude Weekly",
        PieChartId::ClaudeCredit => "Claude Credit",
        PieChartId::Codex5h => "Codex 5h",
        PieChartId::CodexWeekly => "Codex Weekly",
        PieChartId::CodexCredit => "Codex Credit",
        PieChartId::Gemini5h => "Gemini 5h",
        PieChartId::GeminiWeekly => "Gemini Weekly",
    }
}
