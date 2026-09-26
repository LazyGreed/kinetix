//! Golden contract tests shared with provider-adapter guest implementations.

use kinetix::plugins::response_contract::{events_to_json, json_to_events};
use kinetix::types::{FailureKind, FinishReason, StreamEvent, TokenUsage};
use serde_json::{json, Value};

const RESPONSE_SCHEMA_JSON: &str =
    include_str!("../wit/contracts/kinetix.plugin.response.v1.schema.json");
const ALL_EVENTS: &str = include_str!("../wit/fixtures/plugin-response/v1/all-events.json");
const WARNING: &str = include_str!("../wit/fixtures/plugin-response/v1/warning.json");
const PARALLEL_TOOLS_REASONING: &str =
    include_str!("../wit/fixtures/plugin-response/v1/parallel-tools-reasoning.json");
const TERMINAL_ERROR: &str = include_str!("../wit/fixtures/plugin-response/v1/terminal-error.json");
const INVALID_VERSION: &str =
    include_str!("../wit/fixtures/plugin-response/v1/invalid-version.json");
const INVALID_MISSING_TEXT: &str =
    include_str!("../wit/fixtures/plugin-response/v1/invalid-missing-text.json");
const INVALID_ERROR_NOT_TERMINAL: &str =
    include_str!("../wit/fixtures/plugin-response/v1/invalid-error-not-terminal.json");

#[test]
fn serializer_matches_v1_golden_fixture() {
    let events = vec![
        StreamEvent::Start {
            upstream_request_id: Some("req_1".into()),
        },
        StreamEvent::ThinkingDelta {
            text: "hmm".into(),
            signature: Some("sig".into()),
        },
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::ToolCallStart {
            index: 2,
            id: Some("call_1".into()),
            name: "fn".into(),
            signature: None,
        },
        StreamEvent::ToolCallArgsDelta {
            index: 2,
            args: "{\"path\":\"src/main.rs\"}".into(),
        },
        StreamEvent::Usage(TokenUsage {
            input: Some(10),
            output: Some(4),
            cached: Some(2),
            cache_write: Some(1),
            thinking: Some(2),
        }),
        StreamEvent::Finish(FinishReason::ToolCalls),
    ];

    let actual: Value = serde_json::from_str(&events_to_json(&events)).unwrap();
    let expected: Value = serde_json::from_str(ALL_EVENTS).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn refusal_content_projects_to_text_for_the_v1_plugin_contract() {
    let encoded = events_to_json(&[StreamEvent::RefusalDelta("not allowed".into())]);
    let value: Value = serde_json::from_str(&encoded).expect("valid response envelope");
    assert_eq!(value.pointer("/events/0/type"), Some(&json!("text_delta")));
    assert_eq!(value.pointer("/events/0/text"), Some(&json!("not allowed")));
}

#[test]
fn v1_golden_fixture_decodes() {
    let events = json_to_events(ALL_EVENTS).expect("valid v1 fixture");
    assert_eq!(events.len(), 7);
    assert!(matches!(
        &events[2],
        StreamEvent::TextDelta(text) if text == "hi"
    ));
    assert!(matches!(
        events[6],
        StreamEvent::Finish(FinishReason::ToolCalls)
    ));
}

#[test]
fn parallel_tools_and_reasoning_keep_stable_plugin_identity() {
    let events = json_to_events(PARALLEL_TOOLS_REASONING).expect("valid parallel fixture");

    assert!(matches!(
        &events[1],
        StreamEvent::ThinkingDelta { text, signature }
            if text == "consider tools" && signature.as_deref() == Some("sig-parallel")
    ));
    assert!(matches!(
        &events[2],
        StreamEvent::ToolCallStart { index: 0, id, name, .. }
            if id.as_deref() == Some("call_a") && name == "get_weather"
    ));
    assert!(matches!(
        &events[4],
        StreamEvent::ToolCallStart { index: 1, id, name, signature }
            if id.as_deref() == Some("call_b")
                && name == "read_file"
                && signature.as_deref() == Some("sig-tool-b")
    ));
    assert!(matches!(
        events[7],
        StreamEvent::Finish(FinishReason::ToolCalls)
    ));
}

#[test]
fn warning_is_validated_but_not_forwarded_as_content() {
    let events = json_to_events(WARNING).expect("valid warning fixture");
    assert!(matches!(
        events.as_slice(),
        [StreamEvent::TextDelta(text)] if text == "ok"
    ));
}

#[test]
fn terminal_error_becomes_typed_upstream_failure() {
    let error = json_to_events(TERMINAL_ERROR).expect_err("terminal error must fail");
    assert_eq!(error.kind, FailureKind::RateLimit);
    assert_eq!(error.status, Some(429));
    assert_eq!(error.retry_after_secs, Some(7));
    assert_eq!(error.message, "slow down");
    assert!(error.quota_reset_at.is_some());
}

#[test]
fn malformed_or_incompatible_payloads_fail_closed() {
    for fixture in [
        INVALID_VERSION,
        INVALID_MISSING_TEXT,
        INVALID_ERROR_NOT_TERMINAL,
        r#"{"schema":"kinetix.plugin.response","schema_version":1,"events":[{"type":"tool_call_start","name":"fn"}]}"#,
        r#"{"schema":"kinetix.plugin.response","schema_version":1,"events":[{"type":"future_event"}]}"#,
    ] {
        assert!(
            json_to_events(fixture).is_err(),
            "fixture must be rejected: {fixture}"
        );
    }
}

#[test]
fn additive_unknown_fields_are_accepted() {
    let fixture = json!({
        "schema": "kinetix.plugin.response",
        "schema_version": 1,
        "future_top_level": true,
        "events": [{
            "type": "text_delta",
            "text": "ok",
            "future_event_field": {"x": 1}
        }]
    })
    .to_string();

    let events = json_to_events(&fixture).expect("additive fields are compatible");
    assert!(matches!(
        events.as_slice(),
        [StreamEvent::TextDelta(text)] if text == "ok"
    ));
}

#[test]
fn schema_u64_bounds_match_runtime_validator() {
    let schema: Value = serde_json::from_str(RESPONSE_SCHEMA_JSON).expect("valid response schema");
    let event_schemas = schema
        .pointer("/$defs/event/oneOf")
        .and_then(Value::as_array)
        .expect("event schemas");

    let event_schema = |event_type: &str| {
        event_schemas
            .iter()
            .find(|event| {
                event
                    .pointer("/properties/type/const")
                    .and_then(Value::as_str)
                    == Some(event_type)
            })
            .expect("event schema")
    };

    let usage = event_schema("usage");
    for field in ["input", "output", "cached", "cache_write", "thinking"] {
        let pointer = format!("/properties/{field}/maximum");
        assert_eq!(
            usage.pointer(&pointer).and_then(Value::as_u64),
            Some(u64::MAX),
            "usage.{field} schema maximum must match the runtime u64 validator"
        );
    }

    let error = event_schema("error");
    assert_eq!(
        error
            .pointer("/properties/retry_after_secs/maximum")
            .and_then(Value::as_u64),
        Some(u64::MAX),
        "error.retry_after_secs schema maximum must match the runtime u64 validator"
    );

    let max_usage = format!(
        r#"{{"schema":"kinetix.plugin.response","schema_version":1,"events":[{{"type":"usage","input":{0},"output":{0},"cached":{0},"cache_write":{0},"thinking":{0}}}]}}"#,
        u64::MAX
    );
    let events = json_to_events(&max_usage).expect("u64::MAX usage must be accepted");
    assert!(matches!(
        events.as_slice(),
        [StreamEvent::Usage(TokenUsage {
            input: Some(input),
            output: Some(output),
            cached: Some(cached),
            cache_write: Some(cache_write),
            thinking: Some(thinking),
        })] if *input == u64::MAX
            && *output == u64::MAX
            && *cached == u64::MAX
            && *cache_write == u64::MAX
            && *thinking == u64::MAX
    ));

    let max_retry_after = format!(
        r#"{{"schema":"kinetix.plugin.response","schema_version":1,"events":[{{"type":"error","kind":"rate_limit","message":"slow down","retry_after_secs":{}}}]}}"#,
        u64::MAX
    );
    let error = json_to_events(&max_retry_after).expect_err("terminal error must fail");
    assert_eq!(error.retry_after_secs, Some(u64::MAX));

    let overflow = r#"{"schema":"kinetix.plugin.response","schema_version":1,"events":[{"type":"usage","input":18446744073709551616}]}"#;
    assert!(json_to_events(overflow).is_err());
}

#[test]
fn legacy_arrays_remain_a_strict_migration_shim() {
    let events =
        json_to_events(r#"[{"type":"text_delta","text":"legacy"}]"#).expect("legacy array");
    assert!(matches!(
        events.as_slice(),
        [StreamEvent::TextDelta(text)] if text == "legacy"
    ));

    assert!(json_to_events(r#"[{"type":"text_delta"}]"#).is_err());
}
