//! Deterministic scripted traces for adapter cutover and dogfooding.
//!
//! Protocol adapters are compared only after they have emitted the shared
//! [`AgentEvent`] vocabulary.  This keeps trace-corpus checks independent of
//! ACP/native process details and makes failures replayable in CI or locally.

use serde::{Deserialize, Serialize};

use crate::{AgentEvent, SessionState, contract::replay_trace};

/// A named, serializable normalized trace fixture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScriptedTrace {
    pub name: String,
    pub roster_size: usize,
    pub events: Vec<AgentEvent>,
}

impl ScriptedTrace {
    pub fn new(name: impl Into<String>, roster_size: usize, events: Vec<AgentEvent>) -> Self {
        Self {
            name: name.into(),
            roster_size,
            events,
        }
    }

    pub fn replay(&self) -> SessionState {
        replay_trace(self.roster_size, &self.events)
    }
}

/// The first event mismatch, if two normalized traces are not identical.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceDifference {
    pub index: usize,
    pub left: Option<AgentEvent>,
    pub right: Option<AgentEvent>,
}

/// Comparison result includes both event and replay-state equality. A pair of
/// traces can reach equal state while still differing in protocol behavior;
/// that distinction is useful during adapter cutover.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceComparison {
    pub events_equal: bool,
    pub final_state_equal: bool,
    pub first_difference: Option<TraceDifference>,
}

impl TraceComparison {
    pub fn equivalent(&self) -> bool {
        self.events_equal && self.final_state_equal
    }
}

pub fn compare_traces(left: &ScriptedTrace, right: &ScriptedTrace) -> TraceComparison {
    let first_difference = left
        .events
        .iter()
        .zip(&right.events)
        .position(|(left, right)| left != right)
        .map(|index| TraceDifference {
            index,
            left: left.events.get(index).cloned(),
            right: right.events.get(index).cloned(),
        })
        .or_else(|| {
            (left.events.len() != right.events.len()).then(|| {
                let index = left.events.len().min(right.events.len());
                TraceDifference {
                    index,
                    left: left.events.get(index).cloned(),
                    right: right.events.get(index).cloned(),
                }
            })
        });
    TraceComparison {
        events_equal: first_difference.is_none(),
        final_state_equal: left.roster_size == right.roster_size && left.replay() == right.replay(),
        first_difference,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ScriptedTrace, compare_traces};
    use crate::{AgentCapabilities, AgentEvent, Mode, TerminalEvent, ToolStatus, ToolUpdate};

    fn shared_events() -> Vec<AgentEvent> {
        vec![
            AgentEvent::Ready {
                slot: 0,
                capabilities: AgentCapabilities {
                    supports_cancel: true,
                    supports_modes: true,
                    supports_terminals: true,
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
            AgentEvent::Tool {
                slot: 0,
                update: ToolUpdate {
                    id: "tool-1".into(),
                    title: "shell".into(),
                    status: ToolStatus::Completed,
                    detail: Some("done".into()),
                },
            },
            AgentEvent::Terminal {
                slot: 0,
                event: TerminalEvent::Output {
                    id: "terminal-1".into(),
                    text: "ok".into(),
                },
            },
            AgentEvent::Text {
                slot: 0,
                text: "completed".into(),
            },
            AgentEvent::TurnComplete { slot: 0 },
        ]
    }

    #[test]
    fn shared_scripted_acp_and_native_traces_compare_equal() {
        let acp = ScriptedTrace::new("acp-smoke", 1, shared_events());
        let native = ScriptedTrace::new("native-smoke", 1, shared_events());
        let comparison = compare_traces(&acp, &native);
        assert!(comparison.equivalent());
        assert!(comparison.first_difference.is_none());
    }

    #[test]
    fn comparison_reports_first_event_difference_and_state_difference() {
        let left = ScriptedTrace::new("left", 1, shared_events());
        let mut right_events = shared_events();
        right_events[4] = AgentEvent::Text {
            slot: 0,
            text: "different".into(),
        };
        let right = ScriptedTrace::new("right", 1, right_events);
        let comparison = compare_traces(&left, &right);
        assert!(!comparison.equivalent());
        assert_eq!(comparison.first_difference.expect("difference").index, 4);
        assert!(!comparison.final_state_equal);
    }

    #[test]
    fn trace_fixture_round_trip_and_replay_are_deterministic() {
        let fixture = ScriptedTrace::new("fixture", 1, shared_events());
        let encoded = serde_json::to_string(&fixture).expect("serialize");
        let decoded: ScriptedTrace = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(fixture, decoded);
        let first = decoded.replay();
        for _ in 0..10 {
            assert_eq!(decoded.replay(), first);
        }
    }

    #[test]
    fn comparison_is_json_stable_for_ci_artifacts() {
        let trace = ScriptedTrace::new("fixture", 1, shared_events());
        let comparison = compare_traces(&trace, &trace);
        let value = serde_json::to_value(comparison).expect("json");
        assert_eq!(value["events_equal"], json!(true));
        assert_eq!(value["final_state_equal"], json!(true));
        assert_eq!(value["first_difference"], Value::Null);
    }

    use serde_json::Value;
}
