//! Replay-safe state for expensive tool, thought, terminal, and diff details.
//!
//! The model stores source text but does not parse, wrap, or render it. New
//! records start collapsed, so a renderer can show a cheap summary and defer
//! materializing the detail until the user explicitly expands it. All state
//! changes are represented as serializable events and can be replayed without
//! a terminal or UI.

use serde::{Deserialize, Serialize};

/// The kinds of content that may be expensive to display.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DetailKind {
    Tool,
    Thought,
    Terminal,
    Diff,
}

/// A detail record. `content` remains unparsed until a caller asks for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetailRecord {
    pub id: String,
    pub kind: DetailKind,
    pub summary: String,
    pub content: String,
    expanded: bool,
}

impl DetailRecord {
    /// Construct a collapsed record; callers must opt into expansion.
    pub fn collapsed(
        id: impl Into<String>,
        kind: DetailKind,
        summary: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            summary: summary.into(),
            content: content.into(),
            expanded: false,
        }
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// Return only the cheap text needed for the current collapsed/expanded
    /// projection. No parsing or wrapping occurs here.
    pub fn projected_text(&self) -> &str {
        if self.expanded {
            &self.content
        } else {
            &self.summary
        }
    }

    /// Access the original source for copy/export only when it is requested.
    pub fn source(&self) -> &str {
        &self.content
    }
}

/// The complete event vocabulary for detail state. Events are pure data and
/// can be persisted beside the normalized agent event stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DetailEvent {
    Upsert { record: DetailRecord },
    SetExpanded { id: String, expanded: bool },
}

/// Ordered detail state with stable IDs. Upserting an existing record keeps
/// its insertion position and its expansion state, which prevents a streaming
/// terminal/tool update from unexpectedly re-expanding expensive output.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetailModel {
    records: Vec<DetailRecord>,
}

impl DetailModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&DetailRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &DetailRecord> {
        self.records.iter()
    }

    /// Apply a detail event. `false` means a SetExpanded target was absent.
    pub fn apply(&mut self, event: DetailEvent) -> bool {
        match event {
            DetailEvent::Upsert { mut record } => {
                if let Some(existing) = self.records.iter().find(|item| item.id == record.id) {
                    record.expanded = existing.expanded;
                }
                if let Some(existing) = self.records.iter_mut().find(|item| item.id == record.id) {
                    *existing = record;
                } else {
                    self.records.push(record);
                }
                true
            }
            DetailEvent::SetExpanded { id, expanded } => {
                let Some(record) = self.records.iter_mut().find(|record| record.id == id) else {
                    return false;
                };
                record.expanded = expanded;
                true
            }
        }
    }

    pub fn set_expanded(&mut self, id: impl Into<String>, expanded: bool) -> bool {
        self.apply(DetailEvent::SetExpanded {
            id: id.into(),
            expanded,
        })
    }

    pub fn toggle(&mut self, id: &str) -> bool {
        let Some(expanded) = self.get(id).map(|record| !record.expanded) else {
            return false;
        };
        self.set_expanded(id, expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::{DetailEvent, DetailKind, DetailModel, DetailRecord};

    fn terminal() -> DetailRecord {
        DetailRecord::collapsed(
            "terminal-1",
            DetailKind::Terminal,
            "$ cargo test (output hidden)",
            "full terminal output\n".repeat(100),
        )
    }

    fn diff() -> DetailRecord {
        DetailRecord::collapsed(
            "diff-1",
            DetailKind::Diff,
            "3 files changed",
            "--- a/src/lib.rs\n+++ b/src/lib.rs\n",
        )
    }

    #[test]
    fn expensive_terminal_and_diff_details_start_collapsed() {
        let mut model = DetailModel::new();
        model.apply(DetailEvent::Upsert { record: terminal() });
        model.apply(DetailEvent::Upsert { record: diff() });
        assert_eq!(model.len(), 2);
        assert!(!model.get("terminal-1").expect("terminal").is_expanded());
        assert_eq!(
            model.get("terminal-1").expect("terminal").projected_text(),
            "$ cargo test (output hidden)"
        );
        assert_eq!(
            model.get("diff-1").expect("diff").projected_text(),
            "3 files changed"
        );
    }

    #[test]
    fn explicit_toggle_is_the_only_expansion_path() {
        let mut model = DetailModel::new();
        model.apply(DetailEvent::Upsert { record: terminal() });
        assert!(model.toggle("terminal-1"));
        assert!(model.get("terminal-1").expect("terminal").is_expanded());
        assert!(
            model
                .get("terminal-1")
                .expect("terminal")
                .projected_text()
                .contains("full terminal output")
        );
        assert!(model.toggle("terminal-1"));
        assert!(!model.get("terminal-1").expect("terminal").is_expanded());
        assert!(!model.toggle("missing"));
    }

    #[test]
    fn replacing_streaming_detail_preserves_position_and_expansion() {
        let mut model = DetailModel::new();
        model.apply(DetailEvent::Upsert { record: terminal() });
        model.apply(DetailEvent::Upsert { record: diff() });
        assert!(model.set_expanded("terminal-1", true));
        let mut update = terminal();
        update.content = "new output".into();
        update.summary = "updated terminal".into();
        model.apply(DetailEvent::Upsert { record: update });
        let ids = model
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["terminal-1", "diff-1"]);
        assert!(model.get("terminal-1").expect("terminal").is_expanded());
        assert_eq!(
            model.get("terminal-1").expect("terminal").source(),
            "new output"
        );
    }

    #[test]
    fn detail_events_replay_to_identical_state() {
        let events = vec![
            DetailEvent::Upsert { record: terminal() },
            DetailEvent::Upsert { record: diff() },
            DetailEvent::SetExpanded {
                id: "diff-1".into(),
                expanded: true,
            },
        ];
        let encoded = serde_json::to_string(&events).expect("serialize");
        let decoded: Vec<DetailEvent> = serde_json::from_str(&encoded).expect("deserialize");
        let mut first = DetailModel::new();
        let mut second = DetailModel::new();
        for event in events {
            first.apply(event);
        }
        for event in decoded {
            second.apply(event);
        }
        assert_eq!(first, second);
    }

    #[test]
    fn absent_expansion_target_is_a_safe_replay_noop() {
        let mut model = DetailModel::new();
        assert!(!model.apply(DetailEvent::SetExpanded {
            id: "late-detail".into(),
            expanded: true,
        }));
        assert!(model.is_empty());
    }
}
