use crate::types::{AppState, BudgetSummary, ResetBaseline, ResetCycle};
use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;
use rust_decimal::Decimal;

#[derive(Debug, thiserror::Error)]
#[error("reset boundary unavailable")]
pub struct ResetWindowError;
pub struct ResetWindowCalculator {
    tz: Tz,
}
impl ResetWindowCalculator {
    pub fn new(tz: Tz) -> Self {
        Self { tz }
    }
    fn local(&self, date: NaiveDateTime) -> Result<DateTime<Utc>, ResetWindowError> {
        match self.tz.from_local_datetime(&date) {
            LocalResult::Single(v) => Ok(v.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, b) => Ok(a.min(b).with_timezone(&Utc)),
            LocalResult::None => {
                for seconds in 1..86_400 {
                    let candidate = date
                        .checked_add_signed(Duration::seconds(seconds))
                        .ok_or(ResetWindowError)?;
                    match self.tz.from_local_datetime(&candidate) {
                        LocalResult::Single(v) => return Ok(v.with_timezone(&Utc)),
                        LocalResult::Ambiguous(a, b) => return Ok(a.min(b).with_timezone(&Utc)),
                        LocalResult::None => {}
                    }
                }
                Err(ResetWindowError)
            }
        }
    }
    pub fn aggregation_interval(
        &self,
        cycle: ResetCycle,
        now: DateTime<Utc>,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), ResetWindowError> {
        if let ResetCycle::CustomHours(n) = cycle {
            if n == 0 {
                return Err(ResetWindowError);
            }
            return Ok((
                now.checked_sub_signed(Duration::hours(i64::from(n)))
                    .ok_or(ResetWindowError)?,
                now,
            ));
        }
        let local = now.with_timezone(&self.tz);
        if cycle == ResetCycle::Hourly {
            let start = now
                - Duration::minutes(i64::from(local.minute()))
                - Duration::seconds(i64::from(local.second()))
                - Duration::nanoseconds(i64::from(local.nanosecond()));
            return Ok((start, start + Duration::hours(1)));
        }
        let date = local.date_naive();
        let (start, end) = match cycle {
            ResetCycle::Daily => (date, date.succ_opt().ok_or(ResetWindowError)?),
            ResetCycle::Weekly => {
                let start = date
                    .checked_sub_signed(Duration::days(i64::from(
                        date.weekday().num_days_from_monday(),
                    )))
                    .ok_or(ResetWindowError)?;
                (
                    start,
                    start
                        .checked_add_signed(Duration::days(7))
                        .ok_or(ResetWindowError)?,
                )
            }
            ResetCycle::Monthly => {
                let start = NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
                    .ok_or(ResetWindowError)?;
                let (y, m) = if date.month() == 12 {
                    (date.year() + 1, 1)
                } else {
                    (date.year(), date.month() + 1)
                };
                (
                    start,
                    NaiveDate::from_ymd_opt(y, m, 1).ok_or(ResetWindowError)?,
                )
            }
            _ => return Err(ResetWindowError),
        };
        Ok((
            self.local(start.and_hms_opt(0, 0, 0).ok_or(ResetWindowError)?)?,
            self.local(end.and_hms_opt(0, 0, 0).ok_or(ResetWindowError)?)?,
        ))
    }
    pub fn scheduled_boundary(
        &self,
        cycle: ResetCycle,
        now: DateTime<Utc>,
    ) -> Result<DateTime<Utc>, ResetWindowError> {
        Ok(self.aggregation_interval(cycle, now)?.0)
    }
    pub fn baseline(
        &self,
        state: &AppState,
        now: DateTime<Utc>,
    ) -> Result<ResetBaseline, ResetWindowError> {
        let boundary = self.scheduled_boundary(state.reset_cycle.clone(), now)?;
        Ok(ResetBaseline {
            scheduled_boundary_at: boundary,
            active_boundary_at: boundary,
            cycle: state.reset_cycle.clone(),
            calendar_identifier: "gregorian".into(),
            time_zone_identifier: self.tz.to_string(),
            computed_at: now,
        })
    }
    pub fn validated_state(
        &self,
        state: &AppState,
        now: DateTime<Utc>,
    ) -> Result<AppState, ResetWindowError> {
        let expected = self.baseline(state, now)?;
        let mut result = state.clone();
        if !state.baseline.as_ref().is_some_and(|b| {
            b.scheduled_boundary_at == expected.scheduled_boundary_at
                && b.active_boundary_at == expected.active_boundary_at
                && b.cycle == expected.cycle
                && b.calendar_identifier == expected.calendar_identifier
                && b.time_zone_identifier == expected.time_zone_identifier
        }) {
            result.baseline = Some(expected);
        }
        Ok(result)
    }
    pub fn changing(
        &self,
        state: &AppState,
        cycle: ResetCycle,
        now: DateTime<Utc>,
    ) -> Result<AppState, ResetWindowError> {
        let mut state = state.clone();
        state.reset_cycle = cycle;
        state.baseline = None;
        self.validated_state(&state, now)
    }
}
impl BudgetSummary {
    pub fn new(spent_usd: Decimal, budget_usd: Option<Decimal>) -> Self {
        let spent = spent_usd.max(Decimal::ZERO);
        let mut result = Self {
            budget_usd,
            spent_usd: spent,
            remaining_usd: None,
            overage_usd: Decimal::ZERO,
            usage_percentage: None,
            visual_fraction: None,
        };
        if let Some(budget) = budget_usd.filter(|v| *v > Decimal::ZERO) {
            let ratio = spent / budget;
            result.remaining_usd = Some((budget - spent).max(Decimal::ZERO));
            result.overage_usd = (spent - budget).max(Decimal::ZERO);
            result.usage_percentage = Some(ratio * Decimal::from(100));
            result.visual_fraction = Some(ratio.min(Decimal::ONE));
        }
        result
    }
}
