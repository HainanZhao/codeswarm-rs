//! Reusable CodeSwarm agent contracts and protocol adapters.
//!
//! Applications can use the normalized event vocabulary, deterministic relay,
//! and ACP/native adapters without depending on CodeSwarm's terminal UI.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use serde::{Deserialize, Serialize};

pub mod adapters;
pub mod agents;
pub mod collaboration;
pub mod contract;
pub mod details;
pub mod history;
pub mod launcher;
pub mod persistence;
pub mod policy;
pub mod relay;
pub mod resources;
pub mod settings;
pub mod trace;
pub use adapters::*;
pub use relay::{Relay, RelayDecision};

/// Stable roster position in the coordinator-managed agent list.
pub type RosterSlot = usize;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Mode {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentCapabilities {
    pub supports_cancel: bool,
    pub supports_modes: bool,
    pub supports_permissions: bool,
    pub supports_terminals: bool,
    pub supports_session_load: bool,
    #[serde(default)]
    pub supports_models: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolUpdate {
    pub id: String,
    pub title: String,
    pub status: ToolStatus,
    pub detail: Option<String>,
}

/// A slash command advertised by an ACP session.  The renderer intentionally
/// keeps only the command name at the shared event boundary; descriptions and
/// input hints are provider-specific presentation data and are not needed to
/// dispatch or complete a command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentCommand {
    pub name: String,
}

/// The latest context-window counters reported by an ACP session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageUpdate {
    pub used: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionRequest {
    pub id: String,
    pub title: String,
    pub options: Vec<String>,
    /// Protocol identities aligned by index with `options`. Empty entries
    /// fall back to the visible option label for legacy/native adapters.
    #[serde(default)]
    pub option_ids: Vec<String>,
}

/// The normalized answer to an adapter permission request. Native adapters
/// may reject this explicitly when their protocol has no permission control.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PermissionAnswer {
    Selected { option_id: String },
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TerminalEvent {
    Created { id: String, command: String },
    Output { id: String, text: String },
    Exited { id: String, code: i32 },
    Released { id: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RosterUpdate {
    Added {
        slot: RosterSlot,
        name: String,
        identity: String,
    },
    Reloaded {
        slot: RosterSlot,
    },
    Dropped {
        slot: RosterSlot,
    },
    Swapped {
        first: RosterSlot,
        second: RosterSlot,
    },
    Rejected {
        action: String,
        detail: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AgentEvent {
    RosterUpdated {
        update: RosterUpdate,
    },
    Ready {
        slot: RosterSlot,
        capabilities: AgentCapabilities,
    },
    TurnStarted {
        slot: RosterSlot,
    },
    ModesReplaced {
        slot: RosterSlot,
        modes: Vec<Mode>,
        current_mode: Option<String>,
    },
    ModeUpdated {
        slot: RosterSlot,
        current_mode: String,
    },
    ModelsReplaced {
        slot: RosterSlot,
        config_id: String,
        models: Vec<Mode>,
        current_model: Option<String>,
    },
    ModelUpdated {
        slot: RosterSlot,
        current_model: String,
    },
    UserText {
        slot: RosterSlot,
        text: String,
    },
    CommandsReplaced {
        slot: RosterSlot,
        commands: Vec<AgentCommand>,
    },
    UsageUpdated {
        slot: RosterSlot,
        usage: UsageUpdate,
    },
    Text {
        slot: RosterSlot,
        text: String,
    },
    Thought {
        slot: RosterSlot,
        text: String,
    },
    Tool {
        slot: RosterSlot,
        update: ToolUpdate,
    },
    Permission {
        slot: RosterSlot,
        request: PermissionRequest,
    },
    Terminal {
        slot: RosterSlot,
        event: TerminalEvent,
    },
    TurnComplete {
        slot: RosterSlot,
    },
    /// A provider plan is exhausted for this slot. The relay routes around
    /// the agent until it is recharged or reloaded.
    UsageLimitReached {
        slot: RosterSlot,
        detail: String,
    },
    Failed {
        slot: RosterSlot,
        started: bool,
        detail: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Effect {
    Render,
    DispatchPrompt { slot: RosterSlot, prompt: String },
    OfferReload { slot: RosterSlot, crashed: bool },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSlot {
    pub active: bool,
    pub capabilities: AgentCapabilities,
    pub modes: Vec<Mode>,
    pub current_mode: Option<String>,
    #[serde(default)]
    pub models: Vec<Mode>,
    #[serde(default)]
    pub current_model: Option<String>,
    #[serde(default)]
    pub commands: Vec<AgentCommand>,
    #[serde(default)]
    pub usage: Option<UsageUpdate>,
}

impl Default for AgentSlot {
    fn default() -> Self {
        Self {
            active: true,
            capabilities: AgentCapabilities::default(),
            modes: Vec::new(),
            current_mode: None,
            models: Vec::new(),
            current_model: None,
            commands: Vec::new(),
            usage: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionState {
    pub slots: Vec<AgentSlot>,
    pub active_slot: Option<RosterSlot>,
    pub queued_prompts: VecDeque<(RosterSlot, String)>,
    pub public_text: Vec<(RosterSlot, String)>,
}

impl SessionState {
    pub fn new(roster_size: usize) -> Self {
        Self {
            slots: (0..roster_size).map(|_| AgentSlot::default()).collect(),
            active_slot: None,
            queued_prompts: VecDeque::new(),
            public_text: Vec::new(),
        }
    }
}

/// Apply one normalized event. I/O and rendering are represented by effects,
/// never performed in the reducer.
pub fn reduce(state: &mut SessionState, event: AgentEvent) -> Vec<Effect> {
    match event {
        AgentEvent::RosterUpdated { .. } => vec![Effect::Render],
        AgentEvent::Ready { slot, capabilities } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.capabilities = capabilities;
            }
            vec![Effect::Render]
        }
        AgentEvent::TurnStarted { slot } => {
            state.active_slot = Some(slot);
            vec![Effect::Render]
        }
        AgentEvent::ModesReplaced {
            slot,
            modes,
            current_mode,
        } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.modes = modes;
                agent.current_mode =
                    current_mode.filter(|id| agent.modes.iter().any(|mode| mode.id == *id));
            }
            vec![Effect::Render]
        }
        AgentEvent::CommandsReplaced { slot, commands } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.commands = commands;
            }
            vec![Effect::Render]
        }
        AgentEvent::ModeUpdated { slot, current_mode } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.current_mode = Some(current_mode);
            }
            vec![Effect::Render]
        }
        AgentEvent::ModelsReplaced {
            slot,
            models,
            current_model,
            ..
        } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.models = models;
                agent.current_model =
                    current_model.filter(|id| agent.models.iter().any(|model| model.id == *id));
            }
            vec![Effect::Render]
        }
        AgentEvent::ModelUpdated {
            slot,
            current_model,
        } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.current_model = Some(current_model);
            }
            vec![Effect::Render]
        }
        AgentEvent::UsageUpdated { slot, usage } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.usage = Some(usage);
            }
            vec![Effect::Render]
        }
        AgentEvent::Text { slot, text } => {
            state.active_slot = Some(slot);
            state.public_text.push((slot, text));
            vec![Effect::Render]
        }
        AgentEvent::UserText { slot, .. } => {
            state.active_slot = Some(slot);
            vec![Effect::Render]
        }
        AgentEvent::Thought { slot, .. }
        | AgentEvent::Tool { slot, .. }
        | AgentEvent::Permission { slot, .. }
        | AgentEvent::Terminal { slot, .. } => {
            state.active_slot = Some(slot);
            vec![Effect::Render]
        }
        AgentEvent::TurnComplete { .. } => {
            state.active_slot = None;
            let next =
                state
                    .queued_prompts
                    .pop_front()
                    .map(|(target, prompt)| Effect::DispatchPrompt {
                        slot: target,
                        prompt,
                    });
            let mut effects = vec![Effect::Render];
            if let Some(effect) = next {
                effects.push(effect);
            }
            effects
        }
        AgentEvent::UsageLimitReached { slot, .. } => {
            if state.active_slot == Some(slot) {
                state.active_slot = None;
            }
            vec![Effect::Render]
        }
        AgentEvent::Failed {
            slot,
            started,
            detail: _,
        } => {
            if let Some(agent) = state.slots.get_mut(slot) {
                agent.active = false;
            }
            if state.active_slot == Some(slot) {
                state.active_slot = None;
            }
            vec![
                Effect::Render,
                Effect::OfferReload {
                    slot,
                    crashed: started,
                },
            ]
        }
    }
}

/// A newline-delimited event log. It deliberately records normalized events
/// rather than UI operations, so sessions can be replayed by a future renderer
/// or adapter host. Each append closes its file handle but does not force a
/// storage sync; the terminal input/render loop must never block on fsync.
#[derive(Clone, Debug)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: &AgentEvent) -> std::io::Result<()> {
        let encoded = serde_json::to_string(event)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(b"\n")
    }

    /// Append an event and force it to stable storage.
    ///
    /// The regular [`append`](Self::append) path is deliberately lightweight
    /// because it is called from the terminal event loop. Call this only at a
    /// durability boundary such as a completed turn or explicit shutdown.
    pub fn append_durable(&self, event: &AgentEvent) -> std::io::Result<()> {
        let encoded = serde_json::to_string(event)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()
    }

    /// Force already-appended records to stable storage without writing a
    /// duplicate event. This is useful for callers that batch lightweight
    /// appends and checkpoint at turn boundaries.
    pub fn sync(&self) -> std::io::Result<()> {
        let file = OpenOptions::new().read(true).open(&self.path)?;
        file.sync_data()
    }

    /// Start a background writer for event-loop use. The returned handle
    /// queues records in memory and performs all file I/O on its worker
    /// thread. Use [`BufferedEventLog::flush`] at explicit durability
    /// boundaries such as completed turns.
    pub fn buffered(&self) -> std::io::Result<BufferedEventLog> {
        BufferedEventLog::open(self.path.clone())
    }

    pub fn read(&self) -> std::io::Result<Vec<AgentEvent>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        BufReader::new(file)
            .lines()
            .enumerate()
            .filter_map(|(line_number, result)| match result {
                Ok(line) if line.trim().is_empty() => None,
                Ok(line) => Some(serde_json::from_str(&line).map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("event log line {}: {error}", line_number + 1),
                    )
                })),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub fn replay(&self, roster_size: usize) -> std::io::Result<SessionState> {
        let mut state = SessionState::new(roster_size);
        for event in self.read()? {
            reduce(&mut state, event);
        }
        Ok(state)
    }
}

enum BufferedLogCommand {
    Append(String),
    Flush(Sender<std::io::Result<()>>),
    Shutdown(Sender<std::io::Result<()>>),
}

/// Background event-log writer used to keep terminal input/render handling
/// independent from filesystem latency. The channel is intentionally
/// unbounded: dropping normalized events during a streamed turn would make
/// replay and recovery incomplete, while the writer drains ordinary event
/// rates faster than adapters produce them.
pub struct BufferedEventLog {
    sender: Sender<BufferedLogCommand>,
    worker: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl std::fmt::Debug for BufferedEventLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BufferedEventLog")
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl BufferedEventLog {
    fn open(path: PathBuf) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("codeswarm-event-log".into())
            .spawn(move || buffered_log_worker(path, receiver))?;
        Ok(Self {
            sender,
            worker: Some(worker),
        })
    }

    /// Serialize and queue a normalized event without opening a file or
    /// waiting on a filesystem operation in the caller.
    pub fn append(&self, event: &AgentEvent) -> std::io::Result<()> {
        let encoded = serde_json::to_string(event)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.sender
            .send(BufferedLogCommand::Append(format!("{encoded}\n")))
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "event log background writer stopped",
                )
            })
    }

    /// Drain queued records and force them to stable storage.
    pub fn flush(&self) -> std::io::Result<()> {
        let (reply, result) = mpsc::channel();
        self.sender
            .send(BufferedLogCommand::Flush(reply))
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "event log background writer stopped",
                )
            })?;
        result.recv().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "event log background writer stopped",
            )
        })?
    }
}

impl Drop for BufferedEventLog {
    fn drop(&mut self) {
        let (reply, result) = mpsc::channel();
        if self
            .sender
            .send(BufferedLogCommand::Shutdown(reply))
            .is_ok()
        {
            let _ = result.recv();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn buffered_log_worker(
    path: PathBuf,
    receiver: Receiver<BufferedLogCommand>,
) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = BufWriter::new(file);
    while let Ok(command) = receiver.recv() {
        match command {
            BufferedLogCommand::Append(line) => writer.write_all(line.as_bytes())?,
            BufferedLogCommand::Flush(reply) => {
                let result = writer.flush().and_then(|()| writer.get_ref().sync_data());
                let _ = reply.send(result);
            }
            BufferedLogCommand::Shutdown(reply) => {
                let result = writer.flush().and_then(|()| writer.get_ref().sync_data());
                let worker_result = match result {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(error) => {
                        let kind = error.kind();
                        let detail = error.to_string();
                        let _ = reply.send(Err(std::io::Error::new(kind, detail.clone())));
                        Err(std::io::Error::new(kind, detail))
                    }
                };
                return worker_result;
            }
        }
    }
    writer.flush()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{AgentCapabilities, AgentEvent, Effect, Mode, SessionState, reduce};

    #[test]
    fn replacement_catalog_invalidates_stale_mode() {
        let mut state = SessionState::new(1);
        reduce(
            &mut state,
            AgentEvent::ModesReplaced {
                slot: 0,
                modes: vec![Mode {
                    id: "read".into(),
                    label: "Read only".into(),
                }],
                current_mode: Some("write".into()),
            },
        );
        assert_eq!(state.slots[0].current_mode, None);
    }

    #[test]
    fn crash_tombstones_slot_and_uses_crash_copy() {
        let mut state = SessionState::new(2);
        reduce(
            &mut state,
            AgentEvent::Ready {
                slot: 1,
                capabilities: AgentCapabilities::default(),
            },
        );
        let effects = reduce(
            &mut state,
            AgentEvent::Failed {
                slot: 1,
                started: true,
                detail: "process exited".into(),
            },
        );
        assert!(!state.slots[1].active);
        assert!(effects.contains(&Effect::OfferReload {
            slot: 1,
            crashed: true,
        }));
    }

    #[test]
    fn event_log_replays_into_the_same_state() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeswarm-core-{unique}.jsonl"));
        let log = super::EventLog::open(&path);
        let events = [
            AgentEvent::Text {
                slot: 0,
                text: "first".into(),
            },
            AgentEvent::Failed {
                slot: 1,
                started: true,
                detail: "crashed".into(),
            },
        ];
        for event in &events {
            log.append(event).expect("append");
        }
        let replayed = log.replay(2).expect("replay");
        let mut expected = SessionState::new(2);
        for event in events {
            reduce(&mut expected, event);
        }
        assert_eq!(replayed, expected);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn event_log_can_checkpoint_batched_appends() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeswarm-core-checkpoint-{unique}.jsonl"));
        let log = super::EventLog::open(&path);
        log.append(&AgentEvent::Text {
            slot: 0,
            text: "batched".into(),
        })
        .expect("append");
        // `sync` is an explicit checkpoint; it is separate from the hot-path
        // append so render/input latency cannot inherit a storage flush.
        log.sync().expect("checkpoint");
        assert_eq!(log.read().expect("read").len(), 1);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn event_log_durable_append_is_replayable() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeswarm-core-durable-{unique}.jsonl"));
        let log = super::EventLog::open(&path);
        log.append_durable(&AgentEvent::Text {
            slot: 1,
            text: "durable".into(),
        })
        .expect("durable append");
        assert_eq!(
            log.read().expect("read")[0].clone(),
            AgentEvent::Text {
                slot: 1,
                text: "durable".into(),
            }
        );
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn buffered_event_log_drains_and_checkpoints_in_order() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("codeswarm-core-buffered-{unique}.jsonl"));
        let log = super::EventLog::open(&path);
        let buffered = log.buffered().expect("background writer");
        for text in ["one", "two", "three"] {
            buffered
                .append(&AgentEvent::Text {
                    slot: 0,
                    text: text.into(),
                })
                .expect("queue event");
        }
        buffered.flush().expect("checkpoint");
        assert_eq!(
            log.read().expect("read").into_iter().collect::<Vec<_>>(),
            [
                AgentEvent::Text {
                    slot: 0,
                    text: "one".into()
                },
                AgentEvent::Text {
                    slot: 0,
                    text: "two".into()
                },
                AgentEvent::Text {
                    slot: 0,
                    text: "three".into()
                }
            ]
        );
        drop(buffered);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn activity_marks_turn_active_until_completion() {
        let mut state = SessionState::new(1);
        reduce(
            &mut state,
            AgentEvent::Text {
                slot: 0,
                text: "stream".into(),
            },
        );
        assert_eq!(state.active_slot, Some(0));
        reduce(&mut state, AgentEvent::TurnComplete { slot: 0 });
        assert_eq!(state.active_slot, None);
    }
}
