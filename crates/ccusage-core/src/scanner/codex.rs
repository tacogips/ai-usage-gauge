use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use serde::Deserialize;

use crate::types::TimestampedUsageEvent;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexScanContext {
    pub session_id: String,
    pub model: String,
    pub directory: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    timestamp: DateTime<Utc>,
    #[serde(rename = "type")]
    kind: String,
    payload: Payload,
}

#[derive(Debug, Default, Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
    #[serde(default, deserialize_with = "optional_string")]
    effort: Option<String>,
    info: Option<TokenInfo>,
}

#[derive(Debug, Deserialize)]
struct TokenInfo {
    last_token_usage: Option<TokenUsage>,
    total_token_usage: Option<TokenUsage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

fn optional_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().map(str::to_owned))
}

fn directory(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.trim().is_empty())
}

fn effort(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let value = raw.trim().to_ascii_lowercase();
        if !value.is_empty()
            && value.len() <= 32
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            Some(value)
        } else {
            None
        }
    })
}

fn day(timestamp: DateTime<Utc>, timezone: Tz) -> String {
    timestamp
        .with_timezone(&timezone)
        .format("%Y-%m-%d")
        .to_string()
}

fn update_meta(context: &mut CodexScanContext, payload: Payload) {
    if let Some(id) = payload.id.filter(|id| !id.is_empty()) {
        context.session_id = id;
    }
    context.directory = directory(payload.cwd);
}

fn event(envelope: &Envelope, context: &CodexScanContext) -> Option<TimestampedUsageEvent> {
    let info = envelope.payload.info.as_ref()?;
    let last = info.last_token_usage.as_ref()?;
    let total = info.total_token_usage.as_ref()?;
    let request_id = format!(
        "{}:{}:{}:{}:{}",
        total.input_tokens,
        total.cached_input_tokens,
        total.output_tokens,
        total.reasoning_output_tokens,
        total.total_tokens
    );
    let identity = format!("{}\u{1f}{}\u{1f}", context.session_id, request_id);
    Some(TimestampedUsageEvent {
        identity,
        timestamp: envelope.timestamp,
        agent: "codex".into(),
        session_id: context.session_id.clone(),
        request_id,
        message_id: String::new(),
        model: context.model.clone(),
        input_tokens: last.input_tokens.saturating_sub(last.cached_input_tokens),
        output_tokens: last.output_tokens,
        cache_creation_tokens: 0,
        cache_read_tokens: last.cached_input_tokens,
        cache_creation_5m_tokens: 0,
        cache_creation_1h_tokens: 0,
        directory: context.directory.clone(),
        effort: context.effort.clone(),
    })
}

pub struct CodexForwardParser {
    pub context: CodexScanContext,
}

impl CodexForwardParser {
    pub fn new(context: CodexScanContext) -> Self {
        Self { context }
    }

    pub fn consume(
        &mut self,
        line: &[u8],
        timezone: Tz,
    ) -> Option<(String, TimestampedUsageEvent)> {
        let envelope: Envelope = serde_json::from_slice(line).ok()?;
        match envelope.kind.as_str() {
            "session_meta" => update_meta(&mut self.context, envelope.payload),
            "turn_context" => {
                if let Some(model) = envelope.payload.model.filter(|model| !model.is_empty()) {
                    self.context.model = model;
                    self.context.effort = effort(envelope.payload.effort);
                }
            }
            "event_msg"
                if envelope.payload.kind.as_deref() == Some("token_count")
                    && !self.context.model.is_empty() =>
            {
                return Some((
                    day(envelope.timestamp, timezone),
                    event(&envelope, &self.context)?,
                ));
            }
            _ => {}
        }
        None
    }
}

pub type CodexEvents = HashMap<String, (String, TimestampedUsageEvent)>;

/// Stateful reverse parser. A false return signals that earlier lines are unnecessary.
pub struct CodexReverseParser {
    context: CodexScanContext,
    timezone: Tz,
    since: String,
    scan_floor: Option<String>,
    newest: Option<(String, Option<String>)>,
    pending: Vec<Envelope>,
    resolved: Vec<(Envelope, String, Option<String>)>,
    events: CodexEvents,
    stop_requested: bool,
}

impl CodexReverseParser {
    pub fn new(mut context: CodexScanContext, head: &[u8], since: &str, timezone: Tz) -> Self {
        if let Ok(envelope) = serde_json::from_slice::<Envelope>(head) {
            if envelope.kind == "session_meta" {
                update_meta(&mut context, envelope.payload);
            }
        }
        let scan_floor = NaiveDate::parse_from_str(since, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.checked_sub_signed(Duration::days(1)))
            .map(|d| d.format("%Y-%m-%d").to_string());
        Self {
            context,
            timezone,
            since: since.into(),
            scan_floor,
            newest: None,
            pending: Vec::new(),
            resolved: Vec::new(),
            events: HashMap::new(),
            stop_requested: false,
        }
    }

    fn flush(&mut self) {
        for (envelope, model, effort) in self.resolved.drain(..) {
            let mut context = self.context.clone();
            context.model = model;
            context.effort = effort;
            if let Some(event) = event(&envelope, &context) {
                self.events
                    .entry(event.identity.clone())
                    .or_insert_with(|| (day(envelope.timestamp, self.timezone), event));
            }
        }
    }

    pub fn consume(&mut self, line: &[u8]) -> bool {
        // A raw UTC day older than since minus one day is safely older in all time zones.
        if self.pending.is_empty() && self.resolved.is_empty() {
            if let (Some(floor), Some(raw)) = (self.scan_floor.as_deref(), raw_timestamp_day(line))
            {
                if raw < floor {
                    self.stop_requested = true;
                }
            }
        }
        if self.stop_requested && self.newest.is_some() {
            return false;
        }
        let Ok(envelope) = serde_json::from_slice::<Envelope>(line) else {
            return true;
        };
        let local_day = day(envelope.timestamp, self.timezone);
        if local_day.as_str() < self.since.as_str() && self.pending.is_empty() {
            self.stop_requested = true;
        }
        match envelope.kind.as_str() {
            "event_msg" if envelope.payload.kind.as_deref() == Some("token_count") => {
                if local_day.as_str() < self.since.as_str() {
                    if self.pending.is_empty() {
                        self.stop_requested = true;
                    }
                } else if !self.stop_requested {
                    self.pending.push(envelope);
                }
            }
            "turn_context" => {
                if let Some(model) = envelope.payload.model.as_ref().filter(|m| !m.is_empty()) {
                    let model = model.clone();
                    let effort = effort(envelope.payload.effort.clone());
                    if self.newest.is_none() {
                        self.newest = Some((model.clone(), effort.clone()));
                    }
                    self.resolved.extend(
                        self.pending
                            .drain(..)
                            .map(|e| (e, model.clone(), effort.clone())),
                    );
                    if local_day.as_str() < self.since.as_str() && self.resolved.is_empty() {
                        self.stop_requested = true;
                    }
                }
            }
            "session_meta" => {
                update_meta(&mut self.context, envelope.payload);
                self.flush();
                if local_day.as_str() < self.since.as_str() && self.pending.is_empty() {
                    self.stop_requested = true;
                }
            }
            _ => {}
        }
        !(self.stop_requested && self.pending.is_empty() && self.newest.is_some())
    }

    pub fn finish(mut self) -> (CodexEvents, CodexScanContext) {
        self.flush();
        if let Some((model, effort)) = self.newest {
            self.context.model = model;
            self.context.effort = effort;
        }
        (self.events, self.context)
    }
}

fn raw_timestamp_day(line: &[u8]) -> Option<&str> {
    // This fast path is optional: missing or nonstandard formatting simply decodes the line.
    let text = std::str::from_utf8(line).ok()?;
    let rest = text
        .split_once("\"timestamp\"")?
        .1
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let raw = rest.get(..10)?;
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    Some(raw)
}

pub fn bounded_reverse<'a>(
    lines: impl IntoIterator<Item = &'a [u8]>,
    head: &[u8],
    initial_context: CodexScanContext,
    since: &str,
    timezone: Tz,
) -> (CodexEvents, CodexScanContext) {
    let mut parser = CodexReverseParser::new(initial_context, head, since, timezone);
    for line in lines {
        if !parser.consume(line) {
            break;
        }
    }
    parser.finish()
}
