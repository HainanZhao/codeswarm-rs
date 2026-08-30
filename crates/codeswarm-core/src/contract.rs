//! Tolerant normalization helpers for external adapter state.
//!
//! Adapter protocols are external state: catalogs can be reordered or
//! replaced, and capability fields may be absent or malformed.  Normalize
//! those values at the core boundary before feeding them to the reducer.
//! These helpers intentionally discard invalid entries rather than allowing a
//! single bad catalog item to poison a session.

use serde_json::Value;

use crate::{AgentCapabilities, AgentEvent, Mode, SessionState, reduce};

/// Normalize a protocol capability object. Unknown, omitted, or non-boolean
/// fields resolve to `false`; a malformed object therefore behaves like an
/// adapter with no optional capabilities.
pub fn normalize_capabilities(value: &Value) -> AgentCapabilities {
    let object = value
        .get("agentCapabilities")
        .and_then(Value::as_object)
        .or_else(|| value.as_object());
    let Some(object) = object else {
        return AgentCapabilities::default();
    };
    AgentCapabilities {
        supports_cancel: bool_field(object, &["supports_cancel", "supportsCancel", "cancel"]),
        supports_modes: bool_field(object, &["supports_modes", "supportsModes", "modes"]),
        supports_permissions: bool_field(
            object,
            &["supports_permissions", "supportsPermissions", "permissions"],
        ),
        supports_terminals: bool_field(
            object,
            &["supports_terminals", "supportsTerminals", "terminals"],
        ),
        supports_session_load: bool_field(
            object,
            &[
                "supports_session_load",
                "supportsSessionLoad",
                "loadSession",
            ],
        ),
        supports_models: bool_field(object, &["supports_models", "supportsModels", "models"]),
    }
}

/// Normalize a mode catalog from either a bare array or an ACP-style object
/// containing `availableModes`. Invalid entries and duplicate IDs are dropped
/// while preserving the adapter's remaining order.
pub fn normalize_modes(value: &Value) -> Vec<Mode> {
    let modes = value
        .as_array()
        .or_else(|| value.get("availableModes").and_then(Value::as_array));
    let Some(modes) = modes else {
        return Vec::new();
    };
    let mut normalized = Vec::with_capacity(modes.len());
    for value in modes {
        let Some(object) = value.as_object() else {
            continue;
        };
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || normalized.iter().any(|mode: &Mode| mode.id == id) {
            continue;
        }
        let label = object
            .get("label")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(id);
        normalized.push(Mode {
            id: id.into(),
            label: label.into(),
        });
    }
    normalized
}

/// Replay a normalized trace through the deterministic core reducer.
pub fn replay_trace(roster_size: usize, trace: &[AgentEvent]) -> SessionState {
    let mut state = SessionState::new(roster_size);
    for event in trace {
        reduce(&mut state, event.clone());
    }
    state
}

/// Compare protocol-specific traces after normalization has occurred. The
/// event vocabulary is the compatibility boundary, so equivalent ACP/native
/// traces must replay to equal state.
pub fn equivalent_traces(roster_size: usize, left: &[AgentEvent], right: &[AgentEvent]) -> bool {
    replay_trace(roster_size, left) == replay_trace(roster_size, right)
}

fn bool_field(object: &serde_json::Map<String, Value>, names: &[&str]) -> bool {
    names
        .iter()
        .find_map(|name| object.get(*name))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{equivalent_traces, normalize_capabilities, normalize_modes, replay_trace};
    use crate::{AgentCapabilities, AgentEvent, Mode, TerminalEvent, ToolStatus, ToolUpdate};

    #[test]
    fn omitted_or_malformed_capabilities_are_safe_defaults() {
        assert_eq!(
            normalize_capabilities(&json!({})),
            AgentCapabilities::default()
        );
        assert_eq!(
            normalize_capabilities(&json!(null)),
            AgentCapabilities::default()
        );
        assert_eq!(
            normalize_capabilities(&json!({
                "supportsCancel": "yes",
                "supportsModes": true,
                "loadSession": false,
            })),
            AgentCapabilities {
                supports_modes: true,
                ..AgentCapabilities::default()
            }
        );
    }

    #[test]
    fn capability_replacement_is_a_full_external_state_update() {
        let first = normalize_capabilities(&json!({
            "supportsCancel": true,
            "supportsModes": true,
            "supportsPermissions": true,
            "supportsTerminals": true,
            "loadSession": true,
        }));
        let replacement = normalize_capabilities(&json!({"supportsCancel": false}));
        assert!(first.supports_session_load);
        assert!(!replacement.supports_session_load);
        assert!(!replacement.supports_modes);
    }

    #[test]
    fn reordered_and_replaced_mode_catalogs_are_tolerated() {
        let reordered = normalize_modes(&json!({
            "availableModes": [
                {"id": "write", "name": "Write"},
                {"id": "plan", "label": "Plan"},
            ]
        }));
        assert_eq!(
            reordered,
            vec![
                Mode {
                    id: "write".into(),
                    label: "Write".into()
                },
                Mode {
                    id: "plan".into(),
                    label: "Plan".into()
                },
            ]
        );
        let replacement = normalize_modes(&json!([
            {"id": "plan", "label": "Plan"},
            {"id": "plan", "label": "duplicate"},
            {"id": "", "label": "invalid"},
            "malformed",
            {"id": "safe"}
        ]));
        assert_eq!(
            replacement,
            vec![
                Mode {
                    id: "plan".into(),
                    label: "Plan".into()
                },
                Mode {
                    id: "safe".into(),
                    label: "safe".into()
                },
            ]
        );
    }

    #[test]
    fn equivalent_adapter_traces_replay_to_the_same_state() {
        let acp_trace = vec![
            AgentEvent::Ready {
                slot: 0,
                capabilities: normalize_capabilities(&json!({"supportsModes": true})),
            },
            AgentEvent::ModesReplaced {
                slot: 0,
                modes: normalize_modes(&json!({
                    "availableModes": [{"id": "plan", "name": "Plan"}]
                })),
                current_mode: Some("plan".into()),
            },
            AgentEvent::Text {
                slot: 0,
                text: "same normalized answer".into(),
            },
            AgentEvent::TurnComplete { slot: 0 },
        ];
        let native_trace = vec![
            AgentEvent::Ready {
                slot: 0,
                capabilities: AgentCapabilities {
                    supports_modes: true,
                    ..AgentCapabilities::default()
                },
            },
            AgentEvent::ModesReplaced {
                slot: 0,
                modes: vec![Mode {
                    id: "plan".into(),
                    label: "Plan".into(),
                }],
                current_mode: Some("plan".into()),
            },
            AgentEvent::Text {
                slot: 0,
                text: "same normalized answer".into(),
            },
            AgentEvent::TurnComplete { slot: 0 },
        ];
        assert!(equivalent_traces(1, &acp_trace, &native_trace));
        assert_eq!(replay_trace(1, &acp_trace), replay_trace(1, &native_trace));
    }

    #[test]
    fn normalized_trace_fixture_covers_tool_terminal_and_completion() {
        let trace = vec![
            AgentEvent::Tool {
                slot: 0,
                update: ToolUpdate {
                    id: "tool-1".into(),
                    title: "shell".into(),
                    status: ToolStatus::Completed,
                    detail: None,
                },
            },
            AgentEvent::Terminal {
                slot: 0,
                event: TerminalEvent::Output {
                    id: "term-1".into(),
                    text: "ok".into(),
                },
            },
            AgentEvent::TurnComplete { slot: 0 },
        ];
        let replayed = replay_trace(1, &trace);
        assert_eq!(replayed.active_slot, None);
        assert!(equivalent_traces(1, &trace, &trace));
    }
}
