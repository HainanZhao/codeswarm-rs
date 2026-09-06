//! Local product-facing projections shared by live and restored conversations.
//! No provider requests are made by these helpers.

use codeswarm::tui::App;
use codeswarm_adapters::persistence::SessionMetadata;
use codeswarm_adapters::{AgentEvent, HistoryContent};

#[derive(Clone, Debug)]
pub(super) enum SavedEvent {
    Human { text: String, direct: bool },
    Agent(AgentEvent),
}

#[derive(Clone, Debug)]
pub(super) struct SavedConversation {
    pub id: Option<String>,
    pub metadata: SessionMetadata,
    pub events: Vec<SavedEvent>,
    pub warnings: Vec<String>,
}

pub(super) fn replay_conversation(app: &mut App, conversation: &SavedConversation) {
    seed_saved_roster(app, &conversation.metadata);
    for event in &conversation.events {
        match event {
            SavedEvent::Human { text, direct } => app.record_human_message(text, *direct),
            SavedEvent::Agent(event) => replay_display_event(app, event),
        }
    }
    seed_saved_roster(app, &conversation.metadata);
    if !conversation.warnings.is_empty() {
        app.status = format!(
            "history loaded with {} unreadable journal record(s)",
            conversation.warnings.len()
        );
    }
}

/// Restore transcript content without replaying live activity, permissions,
/// cancellation, or scheduler effects. Turn boundaries close history chunks.
pub(super) fn replay_display_event(app: &mut App, event: &AgentEvent) {
    let history = match event {
        AgentEvent::History { .. } => Some(event.clone()),
        AgentEvent::Text { slot, text } => Some(AgentEvent::History {
            slot: *slot,
            content: HistoryContent::Text(text.clone()),
        }),
        AgentEvent::UserText { slot, text } => Some(AgentEvent::History {
            slot: *slot,
            content: HistoryContent::UserText(text.clone()),
        }),
        AgentEvent::Thought { slot, text } => Some(AgentEvent::History {
            slot: *slot,
            content: HistoryContent::Thought(text.clone()),
        }),
        AgentEvent::Tool { slot, update } => Some(AgentEvent::History {
            slot: *slot,
            content: HistoryContent::Tool(update.clone()),
        }),
        AgentEvent::TurnComplete { slot }
        | AgentEvent::Failed { slot, .. }
        | AgentEvent::UsageLimitReached { slot, .. } => {
            app.apply_event(&AgentEvent::Ready {
                slot: *slot,
                capabilities: Default::default(),
            });
            None
        }
        _ => None,
    };
    if let Some(history) = history {
        app.apply_event(&history);
    }
}

/// Display identity is available even if the provider handle is expired or
/// absent. Reading local history must not depend on provider resumability.
pub(super) fn seed_saved_roster(app: &mut App, metadata: &SessionMetadata) {
    if let Some(agents) = metadata.get("agents").and_then(serde_json::Value::as_array) {
        for (slot, agent) in agents.iter().enumerate() {
            let name = agent
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Saved agent");
            app.set_agent_name(slot, name);
            if let Some(identity) = agent.get("identity").and_then(serde_json::Value::as_str) {
                app.set_agent_identity(slot, identity);
            }
            app.apply_event(&AgentEvent::Ready {
                slot,
                capabilities: Default::default(),
            });
        }
    }
    let goal = metadata
        .get("goal")
        .and_then(codeswarm_adapters::goal::Goal::from_metadata);
    app.apply_event(&AgentEvent::GoalUpdated { goal });
    app.status = "saved history · providers disconnected · send a message to continue".into();
}

pub(super) fn metadata_roster(metadata: &SessionMetadata) -> Vec<String> {
    metadata
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|agent| agent.get("name").or_else(|| agent.get("identity")))
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect()
}

pub(super) fn task_title(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(100)
        .collect()
}

pub(super) fn resumable_slots(metadata: &SessionMetadata) -> Vec<usize> {
    metadata
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(slot, agent)| {
            let load = agent
                .get("supports_load_session")
                .and_then(serde_json::Value::as_bool)
                == Some(true);
            let handle = agent
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.trim().is_empty());
            (load && handle).then_some(slot)
        })
        .collect()
}

/// Running-binary information is deliberately read from this process, not a
/// PATH lookup that might point at a newly installed version.
pub(super) fn runtime_diagnostics(metadata: Option<&SessionMetadata>, offline: bool) -> String {
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unavailable".into());
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "unavailable".into());
    let resumable = metadata.map(resumable_slots).unwrap_or_default();
    let mut report = format!(
        "CodeSwarm {}\nRunning executable: {}\nWorkspace: {}\nConnection: {}\nSaved resumable slots: {}\n",
        env!("CARGO_PKG_VERSION"),
        executable,
        cwd,
        if offline {
            "offline history (no provider processes started)"
        } else {
            "live session"
        },
        if resumable.is_empty() {
            "none".into()
        } else {
            resumable
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    if let Some(workflow) = metadata.and_then(|metadata| metadata.get("workflow")) {
        if let Some(worker) = workflow.get("worker").and_then(serde_json::Value::as_str) {
            report.push_str(&format!("Pair workflow worker: {worker}\n"));
        }
        if let Some(previous) = workflow
            .get("handoff_from")
            .and_then(serde_json::Value::as_str)
        {
            report.push_str(&format!("Last handoff from: {previous}\n"));
        }
    }
    report
}

use codeswarm_adapters::session_archive::{
    ArchiveEvent, ArchiveState, BufferedSessionArchive, CreateSession, SessionArchive,
};
use codeswarm_adapters::workflow::CompletionSummary;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

pub(super) fn archive_store() -> SessionArchive {
    SessionArchive::open(super::state_directory().join("archive"))
}

pub(super) fn load_conversation(id: &str, cwd: &Path) -> Result<SavedConversation, String> {
    let archived = archive_store()
        .load(id)
        .map_err(|error| error.to_string())?;
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if archived.entry.cwd != cwd {
        return Err("saved session belongs to another project".into());
    }
    Ok(SavedConversation {
        id: Some(archived.entry.id),
        metadata: archived.metadata,
        warnings: archived.warnings,
        events: archived
            .events
            .into_iter()
            .map(|event| match event {
                ArchiveEvent::Human { text, direct } => SavedEvent::Human { text, direct },
                ArchiveEvent::Agent(event) => SavedEvent::Agent(event),
            })
            .collect(),
    })
}

pub(super) fn latest_conversation(
    cwd: &Path,
    exclude: Option<&str>,
) -> Result<Option<SavedConversation>, String> {
    let listing = archive_store()
        .list(cwd)
        .map_err(|error| error.to_string())?;
    for entry in listing.entries {
        if Some(entry.id.as_str()) != exclude {
            return load_conversation(&entry.id, cwd).map(Some);
        }
    }
    Ok(None)
}

pub(super) fn session_entries(
    cwd: &Path,
) -> Result<(Vec<codeswarm::tui::SessionListEntry>, usize), String> {
    let store = archive_store();
    let listing = store.list(cwd).map_err(|error| error.to_string())?;
    let failures = listing.failures.len();
    let mut entries = Vec::new();
    for entry in listing.entries {
        let updated_at = entry
            .updated_time()
            .map(|t| {
                let offset =
                    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
                let t = t.to_offset(offset);
                format!("{} {:02}:{:02}", t.date(), t.hour(), t.minute())
            })
            .unwrap_or_else(|| "unknown".into());
        // Titles are saved from the first human prompt and form a cheap preview;
        // listing never reads/replays every transcript in the archive.
        entries.push(codeswarm::tui::SessionListEntry {
            id: entry.id,
            preview: if entry.preview.is_empty() {
                format!("{:?}", entry.state)
            } else {
                format!("{:?} · {}", entry.state, entry.preview)
            },
            title: if entry.title.is_empty() {
                "Untitled session".into()
            } else {
                entry.title
            },
            updated_at,
            roster: entry.roster,
        });
    }
    Ok((entries, failures))
}

pub(super) struct ConversationJournal {
    writer: BufferedSessionArchive,
    errors: Receiver<String>,
    pub metadata: SessionMetadata,
    pub summary: CompletionSummary,
    titled: bool,
    changes: Option<Receiver<Result<Vec<String>, String>>>,
}

impl ConversationJournal {
    pub fn open(
        cwd: &Path,
        saved: Option<&SavedConversation>,
        roster: Vec<String>,
    ) -> Result<Self, String> {
        let store = archive_store();
        let metadata = saved
            .map(|saved| saved.metadata.clone())
            .unwrap_or_else(|| {
                let mut metadata = SessionMetadata::empty();
                metadata.insert("cwd", cwd.display().to_string());
                metadata
            });
        let id = if let Some(id) = saved.and_then(|saved| saved.id.clone()) {
            id
        } else {
            store
                .create(
                    CreateSession::new(cwd)
                        .state(ArchiveState::Idle)
                        .roster(roster)
                        .metadata(metadata.clone()),
                )
                .map_err(|error| error.to_string())?
                .id
        };
        let (send, errors) = mpsc::channel();
        let writer = store
            .buffered_with_errors(&id, move |error| {
                let _ = send.send(error);
            })
            .map_err(|error| error.to_string())?;
        let summary = saved.map(summary_for).unwrap_or_default();
        Ok(Self {
            writer,
            errors,
            metadata,
            summary,
            titled: saved.is_some(),
            changes: None,
        })
    }
    pub fn id(&self) -> &str {
        self.writer.id()
    }
    pub fn human(&mut self, text: &str, direct: bool) -> Result<(), String> {
        self.writer
            .append_human(text, direct)
            .map_err(|error| error.to_string())?;
        self.summary.begin_task(text);
        self.metadata.remove("working_tree_paths");
        self.metadata.remove("completion_summary");
        self.writer
            .update_metadata(|metadata| {
                metadata.remove("working_tree_paths");
                metadata.remove("completion_summary");
            })
            .map_err(|error| error.to_string())?;
        if !self.titled {
            let title = task_title(text);
            self.writer
                .update_entry(move |entry| entry.title = title)
                .map_err(|error| error.to_string())?;
            self.titled = true;
        }
        self.writer
            .update_entry(|entry| entry.state = ArchiveState::Active)
            .map_err(|error| error.to_string())?;
        self.writer.checkpoint().map_err(|error| error.to_string())
    }
    pub fn event(&mut self, event: &AgentEvent) -> Result<(), String> {
        self.writer
            .append_agent(event)
            .map_err(|error| error.to_string())?;
        self.summary.observe(event);
        if matches!(event, AgentEvent::TurnStarted { .. }) {
            self.writer
                .update_entry(|entry| entry.state = ArchiveState::Active)
                .map_err(|error| error.to_string())?;
        }
        if let AgentEvent::SessionMetadataUpdated { metadata } = event
            && let Some(data) = metadata.as_object()
        {
            for (key, value) in data {
                self.metadata.insert(key, value.clone());
            }
            let roster = metadata_roster(&self.metadata);
            self.writer
                .replace_metadata(self.metadata.clone())
                .map_err(|error| error.to_string())?;
            self.writer
                .update_entry(move |entry| entry.roster = roster)
                .map_err(|error| error.to_string())?;
            self.writer
                .checkpoint()
                .map_err(|error| error.to_string())?;
        }
        if matches!(
            event,
            AgentEvent::TurnComplete { .. }
                | AgentEvent::Failed { .. }
                | AgentEvent::UsageLimitReached { .. }
        ) {
            let summary = self.summary.render_text();
            self.metadata.insert("completion_summary", summary.clone());
            self.writer
                .update_metadata(move |metadata| {
                    metadata.insert("completion_summary", summary);
                })
                .map_err(|error| error.to_string())?;
            let state = if matches!(
                event,
                AgentEvent::Failed { .. } | AgentEvent::UsageLimitReached { .. }
            ) {
                ArchiveState::Failed
            } else {
                ArchiveState::Completed
            };
            let preview = self
                .summary
                .last_response()
                .map(task_title)
                .unwrap_or_default();
            self.writer
                .update_entry(move |entry| {
                    entry.state = state;
                    if !preview.is_empty() {
                        entry.preview = preview;
                    }
                })
                .map_err(|error| error.to_string())?;
            self.writer
                .checkpoint()
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
    pub fn flush(&self) -> Result<(), String> {
        self.writer.flush().map_err(|error| error.to_string())
    }
    pub fn poll_error(&self) -> Option<String> {
        self.errors.try_iter().last()
    }
    pub fn inspect_changes(&mut self, cwd: &Path) {
        if self.changes.is_some() {
            return;
        }
        let cwd = cwd.to_path_buf();
        let (send, recv) = mpsc::channel();
        self.changes = Some(recv);
        std::thread::spawn(move || {
            let result = std::process::Command::new("git")
                .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
                .current_dir(cwd)
                .output()
                .map_err(|error| error.to_string())
                .and_then(|output| {
                    if output.status.success() {
                        Ok(parse_changed_paths(&output.stdout))
                    } else {
                        Err("working-tree evidence unavailable (not a Git repository)".into())
                    }
                });
            let _ = send.send(result);
        });
    }
    pub fn poll_changes(&mut self) -> Option<Result<(), String>> {
        let result = self.changes.as_ref()?.try_recv().ok()?;
        self.changes = None;
        Some(result.map(|paths| {
            self.summary.set_changed_paths(paths.clone());
            self.metadata
                .insert("working_tree_paths", serde_json::json!(paths));
            let summary = self.summary.render_text();
            let _ = self.writer.update_metadata(move |metadata| {
                metadata.insert("working_tree_paths", serde_json::json!(paths));
                metadata.insert("completion_summary", summary);
            });
            let _ = self.writer.checkpoint();
        }))
    }
}

pub(super) fn summary_for(saved: &SavedConversation) -> CompletionSummary {
    let mut summary = CompletionSummary::new();
    for event in &saved.events {
        match event {
            SavedEvent::Human { text, .. } => summary.begin_task(text),
            SavedEvent::Agent(event) => summary.observe(event),
        }
    }
    if let Some(paths) = saved
        .metadata
        .get("working_tree_paths")
        .and_then(serde_json::Value::as_array)
    {
        summary.set_changed_paths(paths.iter().filter_map(serde_json::Value::as_str));
    }
    summary
}

fn parse_changed_paths(output: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = output.split(|byte| *byte == 0);
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        paths.push(String::from_utf8_lossy(&entry[3..]).into_owned());
        if entry[..2].iter().any(|byte| matches!(byte, b'R' | b'C')) {
            let _ = entries.next();
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_history_displays_without_resumable_handles_or_live_work() {
        let metadata = SessionMetadata::new(
            serde_json::json!({"agents":[{"name":"Offline agent","identity":"offline"}]})
                .as_object()
                .unwrap()
                .clone(),
        );
        let mut app = App::default();
        seed_saved_roster(&mut app, &metadata);
        assert!(resumable_slots(&metadata).is_empty());
        replay_display_event(
            &mut app,
            &AgentEvent::Text {
                slot: 0,
                text: "saved answer".into(),
            },
        );
        replay_display_event(&mut app, &AgentEvent::TurnComplete { slot: 0 });
        replay_display_event(
            &mut app,
            &AgentEvent::Text {
                slot: 0,
                text: "second answer".into(),
            },
        );
        assert!(app.export_markdown().contains("saved answer"));
        assert!(app.export_markdown().contains("second answer"));
        assert!(!app.cancellation_pending());
        assert!(
            app.inactive_agent(std::time::Instant::now() + std::time::Duration::from_secs(600))
                .is_none()
        );
        assert_eq!(app.queued_count(), 0);
    }

    #[test]
    fn resumability_rejects_missing_empty_and_invalid_external_handles() {
        let metadata = SessionMetadata::new(
            serde_json::json!({"agents":[
                {"name":"valid","supports_load_session":true,"session_id":"saved"},
                {"supports_load_session":true,"session_id":""},
                {"supports_load_session":false,"session_id":"saved"},
                {"supports_load_session":true,"session_id":42}
            ]})
            .as_object()
            .unwrap()
            .clone(),
        );
        assert_eq!(resumable_slots(&metadata), vec![0]);
        assert_eq!(metadata_roster(&metadata), vec!["valid"]);
        assert_eq!(task_title("  fix\n login   screen "), "fix login screen");
    }
}
