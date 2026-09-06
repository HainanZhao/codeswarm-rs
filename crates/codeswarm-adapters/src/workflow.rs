//! Pure workflow semantics for pair collaboration and completion evidence.
//!
//! This module has no filesystem or provider I/O. Role helpers classify
//! implementer/reviewer handoffs, and [`CompletionSummary`] accumulates a
//! result summary strictly from observed [`AgentEvent`] data. Agent prose is
//! labelled as a claim, never as evidence, and missing evidence is labelled
//! unknown.

use serde::{Deserialize, Serialize};

use crate::{AgentEvent, RosterSlot, ToolStatus, ToolUpdate};

/// A role inside the two-agent pair review loop. Roster, solo, and manual
/// strategies never assign these roles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairRole {
    /// The agent producing the change that the pair reviewer will inspect.
    Implementer,
    /// The agent inspecting an implementer's handoff for concrete defects.
    Reviewer,
}

impl PairRole {
    pub fn label(self) -> &'static str {
        match self {
            PairRole::Implementer => "Implementer",
            PairRole::Reviewer => "Reviewer",
        }
    }
}

/// Classify a non-direct pair-strategy dispatch.
///
/// Roles are anchored to the first responder of the current human task, so
/// returning to that slot after review means implementation work again.
pub fn pair_role(implementer_slot: Option<RosterSlot>, slot: RosterSlot) -> Option<PairRole> {
    match implementer_slot {
        None => Some(PairRole::Implementer),
        Some(implementer) if implementer == slot => Some(PairRole::Implementer),
        Some(_) => Some(PairRole::Reviewer),
    }
}

/// Explain the pair handoff for one dispatched turn.
///
/// `peer` optionally names the counterpart agent. The reviewer fragment asks
/// for concrete defects or a concise approval; the implementer fragment makes
/// the upcoming review explicit. Neither fragment mentions the stop token, so
/// stop eligibility stays governed by the existing prompt footer.
pub fn role_fragment(role: PairRole, peer: Option<&str>) -> String {
    match role {
        PairRole::Implementer => match peer {
            Some(peer) => format!(
                "Pair role: you are the implementer. Produce the concrete change for the \
                 shared task; your reviewer {peer} will review the result next, so describe \
                 exactly what you changed."
            ),
            None => "Pair role: you are the implementer. Produce the concrete change for the \
                 shared task; your pair reviewer will review the result next, so describe \
                 exactly what you changed."
                .to_owned(),
        },
        PairRole::Reviewer => match peer {
            Some(peer) => format!(
                "Pair role: you are the reviewer. {peer} handed off their work for review. \
                 Reply with concrete defects to fix, or a concise approval if none remain."
            ),
            None => "Pair role: you are the reviewer. The implementer handed off their work for \
                 review. Reply with concrete defects to fix, or a concise approval if none \
                 remain."
                .to_owned(),
        },
    }
}

/// One deduplicated tool outcome, keyed by the adapter's stable tool id.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolOutcome {
    pub id: String,
    pub title: String,
    pub status: ToolStatus,
    pub detail: Option<String>,
    pub slot: RosterSlot,
}

impl ToolOutcome {
    /// Evidence label for the final observed status. A pending or running
    /// tool has no final outcome, so its evidence is unknown; a completed
    /// status proves only that the tool call finished, never that a test
    /// suite or any claimed result succeeded.
    pub fn evidence_label(&self) -> &'static str {
        match self.status {
            ToolStatus::Completed => "completed",
            ToolStatus::Failed => "failed",
            ToolStatus::Running => "unknown (still running)",
            ToolStatus::Pending => "unknown (still pending)",
        }
    }
}

/// Accumulated, evidence-bound completion state for one human task.
///
/// The root CLI resets this per human task, feeds live [`AgentEvent`]s as
/// they arrive, attaches caller-supplied working-tree paths, and renders the
/// `/summary` text. [`AgentEvent::History`] replays are display-only and are
/// never counted as live turns; use [`CompletionSummary::from_events`] to
/// rebuild from archived events deliberately.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionSummary {
    task: Option<String>,
    response: Option<String>,
    response_slot: Option<RosterSlot>,
    pending_response: String,
    pending_slot: Option<RosterSlot>,
    tools: Vec<ToolOutcome>,
    changed_paths: Vec<String>,
    changed_paths_observed: bool,
    turns_observed: usize,
}

impl CompletionSummary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a fresh task. Clears all observed state and records the task
    /// text shown at the top of every render.
    pub fn begin_task(&mut self, task: impl Into<String>) {
        *self = Self {
            task: Some(task.into()),
            ..Self::default()
        };
    }

    /// Clear all observed state without changing the task text.
    pub fn reset(&mut self) {
        let task = self.task.take();
        *self = Self::default();
        self.task = task;
    }

    /// Observe one live normalized event. Display-only history replay events
    /// ([`AgentEvent::History`]) are ignored so restored conversations never
    /// count as live turns or fabricate outcomes.
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::History { .. } => {}
            AgentEvent::TurnStarted { .. } => {
                self.commit_pending();
                self.turns_observed += 1;
            }
            AgentEvent::Text { slot, text } => {
                self.pending_response.push_str(text);
                self.pending_slot = Some(*slot);
            }
            AgentEvent::Tool { slot, update } => self.record_tool(*slot, update),
            AgentEvent::TurnComplete { .. }
            | AgentEvent::Failed { .. }
            | AgentEvent::UsageLimitReached { .. } => self.commit_pending(),
            _ => {}
        }
    }

    /// Deliberately rebuild a summary from stored or replayed events, such as
    /// an archived journal. Unlike live observation this is an explicit
    /// intent to summarize past activity; [`AgentEvent::History`] wrappers
    /// are still display-only and stay excluded.
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a AgentEvent>) -> Self {
        let mut summary = Self::new();
        for event in events {
            summary.observe(event);
        }
        summary
    }

    /// Attach caller-supplied changed working-tree paths, such as `git
    /// status` output. The summary never inspects the filesystem itself.
    /// Paths are trimmed, deduplicated preserving first-seen order, and
    /// replace any previously attached set.
    pub fn set_changed_paths(&mut self, paths: impl IntoIterator<Item = impl Into<String>>) {
        let mut seen = Vec::new();
        for path in paths {
            let path = path.into().trim().to_owned();
            if !path.is_empty() && !seen.contains(&path) {
                seen.push(path);
            }
        }
        self.changed_paths = seen;
        self.changed_paths_observed = true;
    }

    pub fn task(&self) -> Option<&str> {
        self.task.as_deref()
    }

    /// The final response text of the most recent turn with text. This is
    /// agent-reported prose, never evidence of execution.
    pub fn last_response(&self) -> Option<&str> {
        self.response.as_deref()
    }

    pub fn last_response_slot(&self) -> Option<RosterSlot> {
        self.response_slot
    }

    pub fn tool_outcomes(&self) -> &[ToolOutcome] {
        &self.tools
    }

    pub fn changed_paths(&self) -> &[String] {
        &self.changed_paths
    }

    pub fn observed_turns(&self) -> usize {
        self.turns_observed
    }

    fn commit_pending(&mut self) {
        if !self.pending_response.trim().is_empty() {
            self.response = Some(std::mem::take(&mut self.pending_response));
            self.response_slot = self.pending_slot.take();
        } else {
            self.pending_response.clear();
            self.pending_slot = None;
        }
    }

    fn record_tool(&mut self, slot: RosterSlot, update: &ToolUpdate) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|outcome| outcome.slot == slot && outcome.id == update.id)
        {
            existing.title = update.title.clone();
            existing.status = update.status;
            existing.detail = update.detail.clone();
            existing.slot = slot;
        } else {
            self.tools.push(ToolOutcome {
                id: update.id.clone(),
                title: update.title.clone(),
                status: update.status,
                detail: update.detail.clone(),
                slot,
            });
        }
    }

    /// Render the summary as GitHub-flavored Markdown.
    pub fn render_markdown(&self) -> String {
        self.render(true)
    }

    /// Render the summary as plain text.
    pub fn render_text(&self) -> String {
        self.render(false)
    }

    fn render(&self, markdown: bool) -> String {
        let mut out = String::new();
        if markdown {
            out.push_str("## Completion summary\n\n");
        } else {
            out.push_str("Completion summary\n\n");
        }
        match &self.task {
            Some(task) => {
                if markdown {
                    out.push_str(&format!("**Task:** {task}\n"));
                } else {
                    out.push_str(&format!("Task: {task}\n"));
                }
            }
            None => {
                if markdown {
                    out.push_str("_Task: unknown (no task recorded)._\n");
                } else {
                    out.push_str("Task: unknown (no task recorded).\n");
                }
            }
        }
        out.push('\n');
        match &self.response {
            Some(response) => {
                if markdown {
                    out.push_str("**Last response** (agent-reported, not evidence):\n");
                } else {
                    out.push_str("Last response (agent-reported, not evidence):\n");
                }
                out.push_str(response.trim_end());
                out.push_str("\n\n");
            }
            None => {
                if markdown {
                    out.push_str("_Last response: unknown (no live response observed)._\n\n");
                } else {
                    out.push_str("Last response: unknown (no live response observed).\n\n");
                }
            }
        }
        if markdown {
            out.push_str("**Tool outcomes:**\n");
        } else {
            out.push_str("Tool outcomes:\n");
        }
        if self.tools.is_empty() {
            if markdown {
                out.push_str("_None recorded; execution evidence is unknown._\n");
            } else {
                out.push_str("None recorded; execution evidence is unknown.\n");
            }
        } else {
            for outcome in &self.tools {
                out.push_str(&format!(
                    "- {} (`{}`, slot {}): {}",
                    outcome.title,
                    outcome.id,
                    outcome.slot,
                    outcome.evidence_label()
                ));
                if let Some(detail) = &outcome.detail {
                    for line in detail.lines() {
                        if markdown {
                            out.push_str(&format!("\n  > {line}"));
                        } else {
                            out.push_str(&format!("\n    {line}"));
                        }
                    }
                }
                out.push('\n');
            }
        }
        out.push('\n');
        if markdown {
            out.push_str("**Changed working-tree paths** (caller-provided):\n");
        } else {
            out.push_str("Changed working-tree paths (caller-provided):\n");
        }
        if self.changed_paths.is_empty() && self.changed_paths_observed {
            out.push_str("None (working tree was clean when checked).\n");
        } else if self.changed_paths.is_empty() {
            if markdown {
                out.push_str("_Unknown (not provided)._\n");
            } else {
                out.push_str("Unknown (not provided).\n");
            }
        } else {
            for path in &self.changed_paths {
                out.push_str(&format!("- {path}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{CompletionSummary, PairRole, ToolOutcome, pair_role, role_fragment};
    use crate::{AgentEvent, ToolStatus, ToolUpdate};

    fn tool(id: &str, status: ToolStatus) -> AgentEvent {
        AgentEvent::Tool {
            slot: 1,
            update: ToolUpdate {
                id: id.into(),
                title: format!("tool {id}"),
                status,
                detail: None,
            },
        }
    }

    #[test]
    fn pair_roles_classify_fresh_handoffs_and_repeat_dispatches() {
        assert_eq!(pair_role(None, 0), Some(PairRole::Implementer));
        assert_eq!(pair_role(None, 3), Some(PairRole::Implementer));
        assert_eq!(pair_role(Some(0), 1), Some(PairRole::Reviewer));
        assert_eq!(pair_role(Some(1), 0), Some(PairRole::Reviewer));
        assert_eq!(pair_role(Some(0), 0), Some(PairRole::Implementer));
    }

    #[test]
    fn role_fragments_explain_handoffs_and_request_defects_or_approval() {
        let implementer = role_fragment(PairRole::Implementer, None);
        assert!(implementer.contains("you are the implementer"));
        assert!(implementer.contains("pair reviewer will review the result next"));
        assert!(!implementer.contains(crate::relay::STOP_TOKEN));

        let reviewer = role_fragment(PairRole::Reviewer, Some("Claude"));
        assert!(reviewer.contains("you are the reviewer"));
        assert!(reviewer.contains("Claude handed off"));
        assert!(reviewer.contains("concrete defects"));
        assert!(reviewer.contains("concise approval"));
        assert!(!reviewer.contains(crate::relay::STOP_TOKEN));

        let unnamed = role_fragment(PairRole::Reviewer, None);
        assert!(unnamed.contains("The implementer handed off"));
    }

    #[test]
    fn summary_accumulates_only_the_latest_turn_response() {
        let mut summary = CompletionSummary::new();
        summary.begin_task("fix the build");
        for event in [
            AgentEvent::TurnStarted { slot: 0 },
            AgentEvent::Text {
                slot: 0,
                text: "first ".into(),
            },
            AgentEvent::Text {
                slot: 0,
                text: "response".into(),
            },
            AgentEvent::TurnComplete { slot: 0 },
            AgentEvent::TurnStarted { slot: 1 },
            AgentEvent::Text {
                slot: 1,
                text: "final response".into(),
            },
            AgentEvent::TurnComplete { slot: 1 },
        ] {
            summary.observe(&event);
        }
        assert_eq!(summary.task(), Some("fix the build"));
        assert_eq!(summary.last_response(), Some("final response"));
        assert_eq!(summary.last_response_slot(), Some(1));
        assert_eq!(summary.observed_turns(), 2);

        // A turn that produces no text keeps the previous response.
        summary.observe(&AgentEvent::TurnStarted { slot: 0 });
        summary.observe(&AgentEvent::TurnComplete { slot: 0 });
        assert_eq!(summary.last_response(), Some("final response"));
    }

    #[test]
    fn summary_deduplicates_tool_updates_by_id() {
        let mut summary = CompletionSummary::new();
        summary.observe(&tool("t1", ToolStatus::Running));
        summary.observe(&AgentEvent::Tool {
            slot: 1,
            update: ToolUpdate {
                id: "t1".into(),
                title: "cargo test".into(),
                status: ToolStatus::Completed,
                detail: Some("exit 0".into()),
            },
        });
        summary.observe(&tool("t2", ToolStatus::Failed));
        assert_eq!(
            summary.tool_outcomes(),
            [
                ToolOutcome {
                    id: "t1".into(),
                    title: "cargo test".into(),
                    status: ToolStatus::Completed,
                    detail: Some("exit 0".into()),
                    slot: 1,
                },
                ToolOutcome {
                    id: "t2".into(),
                    title: "tool t2".into(),
                    status: ToolStatus::Failed,
                    detail: None,
                    slot: 1,
                },
            ]
        );
        let text = summary.render_text();
        assert!(text.contains("cargo test (`t1`, slot 1): completed"));
        assert!(text.contains("tool t2 (`t2`, slot 1): failed"));
    }

    #[test]
    fn tool_ids_are_scoped_to_agents_and_explicit_empty_evidence_clears() {
        let mut summary = CompletionSummary::new();
        for slot in [0, 1] {
            summary.observe(&AgentEvent::Tool {
                slot,
                update: ToolUpdate {
                    id: "same".into(),
                    title: "Read".into(),
                    status: ToolStatus::Completed,
                    detail: Some("output".into()),
                },
            });
        }
        assert_eq!(summary.tool_outcomes().len(), 2);
        summary.observe(&AgentEvent::Tool {
            slot: 0,
            update: ToolUpdate {
                id: "same".into(),
                title: "Read".into(),
                status: ToolStatus::Completed,
                detail: None,
            },
        });
        assert_eq!(summary.tool_outcomes()[0].detail, None);
        assert_eq!(summary.tool_outcomes()[1].detail.as_deref(), Some("output"));
        summary.set_changed_paths(Vec::<String>::new());
        assert!(summary.render_text().contains("working tree was clean"));
    }

    #[test]
    fn unresolved_tools_report_unknown_evidence() {
        let mut summary = CompletionSummary::new();
        summary.observe(&tool("t1", ToolStatus::Pending));
        summary.observe(&tool("t2", ToolStatus::Running));
        let text = summary.render_text();
        assert!(text.contains("(`t1`, slot 1): unknown (still pending)"));
        assert!(text.contains("(`t2`, slot 1): unknown (still running)"));
        assert!(!text.to_lowercase().contains("completed"));
    }

    #[test]
    fn history_events_never_count_as_live_turns_or_outcomes() {
        let mut summary = CompletionSummary::new();
        summary.begin_task("task");
        summary.observe(&AgentEvent::History {
            slot: 0,
            content: crate::HistoryContent::Text("replayed".into()),
        });
        summary.observe(&AgentEvent::History {
            slot: 0,
            content: crate::HistoryContent::Tool(ToolUpdate {
                id: "h1".into(),
                title: "replayed tool".into(),
                status: ToolStatus::Completed,
                detail: None,
            }),
        });
        assert_eq!(summary.last_response(), None);
        assert!(summary.tool_outcomes().is_empty());
        assert_eq!(summary.observed_turns(), 0);
        let text = summary.render_text();
        assert!(text.contains("unknown (no live response observed)"));
        assert!(text.contains("None recorded; execution evidence is unknown."));
    }

    #[test]
    fn from_events_rebuilds_archived_activity_deliberately() {
        let events = vec![
            AgentEvent::TurnStarted { slot: 0 },
            AgentEvent::Text {
                slot: 0,
                text: "archived answer".into(),
            },
            AgentEvent::Tool {
                slot: 0,
                update: ToolUpdate {
                    id: "a1".into(),
                    title: "edit".into(),
                    status: ToolStatus::Completed,
                    detail: None,
                },
            },
            AgentEvent::TurnComplete { slot: 0 },
            // Display-only replay content stays excluded even here.
            AgentEvent::History {
                slot: 0,
                content: crate::HistoryContent::Text("replay".into()),
            },
        ];
        let summary = CompletionSummary::from_events(&events);
        assert_eq!(summary.last_response(), Some("archived answer"));
        assert_eq!(summary.tool_outcomes().len(), 1);
        assert_eq!(summary.observed_turns(), 1);
    }

    #[test]
    fn changed_paths_are_trimmed_deduplicated_and_rendered() {
        let mut summary = CompletionSummary::new();
        summary.set_changed_paths([" src/lib.rs ", "", "src/lib.rs", "docs/plan.md"]);
        assert_eq!(
            summary.changed_paths(),
            ["src/lib.rs".to_string(), "docs/plan.md".to_string()]
        );
        let markdown = summary.render_markdown();
        assert_eq!(markdown.matches("- src/lib.rs").count(), 1);
        assert!(markdown.contains("- docs/plan.md"));

        summary.set_changed_paths(Vec::<String>::new());
        assert!(summary.changed_paths().is_empty());
        assert!(
            summary
                .render_text()
                .contains("None (working tree was clean when checked).")
        );
        assert!(
            summary
                .render_markdown()
                .contains("None (working tree was clean when checked).")
        );
    }

    #[test]
    fn agent_prose_is_never_promoted_to_execution_evidence() {
        let mut summary = CompletionSummary::new();
        summary.observe(&AgentEvent::TurnStarted { slot: 0 });
        summary.observe(&AgentEvent::Text {
            slot: 0,
            text: "All tests passed and the build is clean.".into(),
        });
        summary.observe(&AgentEvent::TurnComplete { slot: 0 });
        for render in [summary.render_text(), summary.render_markdown()] {
            assert!(render.contains("agent-reported, not evidence"));
            assert!(render.contains("All tests passed"));
            assert!(!render.to_lowercase().contains("verified"));
            assert!(render.contains("None recorded; execution evidence is unknown."));
        }
    }

    #[test]
    fn renders_cover_task_absent_and_reset_semantics() {
        let mut summary = CompletionSummary::new();
        assert!(summary.render_text().contains("Task: unknown"));
        summary.begin_task("first");
        summary.observe(&AgentEvent::Text {
            slot: 0,
            text: "progress".into(),
        });
        summary.reset();
        assert_eq!(summary.task(), Some("first"));
        assert_eq!(summary.last_response(), None);
        assert_eq!(summary.observed_turns(), 0);
        assert!(summary.tool_outcomes().is_empty());
        summary.begin_task("second");
        assert_eq!(summary.task(), Some("second"));
        assert_eq!(summary.last_response(), None);
    }
}
