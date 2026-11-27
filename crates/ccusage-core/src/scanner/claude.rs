//! Claude Code assistant usage records.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

use crate::types::{TimestampedUsageEvent, UsageEffort};

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    timestamp: DateTime<Utc>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(rename = "requestId", default)]
    request_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "optional_string")]
    effort: Option<String>,
    #[serde(rename = "perTurnEffort", default, deserialize_with = "optional_string")]
    per_turn_effort: Option<String>,
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    id: String,
    role: String,
    model: String,
    usage: Usage,
}

#[derive(Default, Deserialize)]
struct Usage {
    #[serde(default, deserialize_with = "null_as_zero")]
    input_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    output_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    cache_creation_input_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation: Option<CacheCreation>,
}

#[derive(Default, Deserialize)]
struct CacheCreation {
    #[serde(default, deserialize_with = "null_as_zero")]
    ephemeral_5m_input_tokens: u64,
    #[serde(default, deserialize_with = "null_as_zero")]
    ephemeral_1h_input_tokens: u64,
}

fn null_as_zero<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or_default())
}

fn optional_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Option<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("optional string or ignored json value")
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_owned()))
        }
        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map
                .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }
    deserializer.deserialize_any(Visitor)
}

/// Decode an assistant usage line; unrelated or malformed JSON is ignored.
pub fn parse_line(line: &str) -> Option<TimestampedUsageEvent> {
    if !line.contains("\"assistant\"") || !line.contains("\"usage\"") {
        return None;
    }
    let envelope: Envelope = serde_json::from_str(line).ok()?;
    if envelope.kind != "assistant"
        || envelope.message.role != "assistant"
        || envelope.message.model.is_empty()
    {
        return None;
    }
    let session_id = envelope.session_id.unwrap_or_default();
    let message_id = envelope.message.id;
    let request_id = envelope.request_id.unwrap_or_else(|| message_id.clone());
    let identity = format!("{session_id}\x1f{request_id}\x1f{message_id}");
    let usage = envelope.message.usage;
    let cache = usage.cache_creation.unwrap_or_default();
    let effort = envelope
        .effort
        .as_deref()
        .or(envelope.per_turn_effort.as_deref())
        .and_then(UsageEffort::normalize);
    Some(TimestampedUsageEvent {
        identity,
        timestamp: envelope.timestamp,
        agent: "claude".into(),
        session_id,
        request_id,
        message_id,
        model: envelope.message.model,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_tokens: usage.cache_creation_input_tokens,
        cache_read_tokens: usage.cache_read_input_tokens,
        cache_creation_5m_tokens: cache.ephemeral_5m_input_tokens,
        cache_creation_1h_tokens: cache.ephemeral_1h_input_tokens,
        directory: envelope.cwd.filter(|s| !s.trim().is_empty()),
        effort,
    })
}

/// Retain the most recent version of each message identity.
pub fn insert_latest(
    events: &mut HashMap<String, TimestampedUsageEvent>,
    event: TimestampedUsageEvent,
) {
    if events
        .get(&event.identity)
        .is_some_and(|existing| existing.timestamp > event.timestamp)
    {
        return;
    }
    events.insert(event.identity.clone(), event);
}
