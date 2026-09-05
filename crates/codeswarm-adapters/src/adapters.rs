//! Protocol adapters for CodeSwarm.
//!
//! ACP and native CLI protocols are intentionally peers here. They emit the
//! same core events and expose capabilities through the same adapter boundary.

use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{
    AgentCapabilities, AgentCommand, AgentEvent, Effect, EventLog, Mode, PermissionAnswer,
    PermissionRequest, RosterSlot, RosterUpdate, SessionState, TerminalEvent, ToolStatus,
    ToolUpdate, UsageUpdate,
    persistence::{BufferedSessionMetadataStore, SessionMetadata},
    reduce,
    relay::{
        CollaborationStrategy, DEFAULT_STOP_ACKNOWLEDGMENT, Relay, RelayDecision, STOP_TOKEN,
        is_usage_limit_response, stop_token_visible_end, strip_stop_token,
    },
    resources,
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};

pub type AdapterResult<T> = Result<T, AdapterError>;

/// Keep a peer from allocating unbounded memory for one newline-delimited
/// protocol frame. The Python client applied the same boundary (10 MiB) to
/// its asyncio reader; ACP messages are normally much smaller than this.
const MAX_ACP_LINE_BYTES: usize = 10 * 1024 * 1024;
const MAX_FILE_READ_BYTES: usize = 4 * 1024 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
struct TerminalProcess {
    child: Arc<AsyncMutex<Option<Child>>>,
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    output_readers: Arc<AtomicUsize>,
}

impl TerminalProcess {
    async fn kill(&self) {
        if let Some(child) = self.child.lock().await.as_mut() {
            #[cfg(unix)]
            if signal_isolated_process_group(child, nix::sys::signal::Signal::SIGTERM) {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                signal_isolated_process_group(child, nix::sys::signal::Signal::SIGKILL);
            }
            let _ = child.start_kill();
        }
    }

    async fn stop(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = terminate_child(&mut child).await;
        }
    }

    async fn wait(&self) -> Option<i32> {
        loop {
            let code = {
                let mut child = self.child.lock().await;
                match child.as_mut() {
                    None => Some(-1),
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
                        Ok(None) => None,
                        // A terminal whose child handle can no longer be
                        // polled must not leave `wait_for_exit` spinning
                        // forever. Preserve the protocol's integer exit
                        // contract with an unknown/failure sentinel.
                        Err(_) => Some(-1),
                    },
                }
            };
            if code.is_some() {
                while self.output_readers.load(Ordering::Acquire) != 0 {
                    tokio::task::yield_now().await;
                }
                return code;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn exit_code(&self) -> Option<i32> {
        let mut child = self.child.lock().await;
        child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .map(|status| status.code().unwrap_or(-1))
    }
}

#[derive(Clone, Debug)]
pub struct HostUpdate {
    pub event: AgentEvent,
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdapterError {
    Unsupported(&'static str),
    Spawn(String),
    Transport(String),
    Protocol(String),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(operation) => write!(formatter, "unsupported operation: {operation}"),
            Self::Spawn(error) => write!(formatter, "unable to launch agent: {error}"),
            Self::Transport(error) => write!(formatter, "agent transport error: {error}"),
            Self::Protocol(error) => write!(formatter, "agent protocol error: {error}"),
        }
    }
}

impl std::error::Error for AdapterError {}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// A shell-free argv parser for commands stored in the agent catalog.
///
/// Agent commands are configuration data, not shell snippets: expansion and
/// pipelines are intentionally unsupported. We still accept the quoting users
/// expect when entering a command (`'...'`, `"..."`, and backslash escapes),
/// so a configured executable or argument containing whitespace is passed to
/// `Command` as one argument and cannot be re-split accidentally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandParseError {
    Empty,
    UnterminatedQuote,
    TrailingEscape,
}

impl std::fmt::Display for CommandParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Empty => "command is empty",
            Self::UnterminatedQuote => "command contains an unterminated quote",
            Self::TrailingEscape => "command ends with an incomplete escape",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CommandParseError {}

/// Parse a configured command into an executable and argv without invoking a
/// shell. This is shared by native and ACP adapters.
pub fn parse_command_line(command: &str) -> Result<(String, Vec<String>), CommandParseError> {
    let mut argv = Vec::new();
    let mut argument = String::new();
    let mut quoted = None;
    let mut escaped = false;
    let mut started = false;

    for character in command.chars() {
        if escaped {
            argument.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match (quoted, character) {
            (_, '\\') if quoted != Some('\'') => {
                escaped = true;
                started = true;
            }
            (None, '\'' | '"') => {
                quoted = Some(character);
                started = true;
            }
            (Some(quote), character) if character == quote => quoted = None,
            (None, character) if character.is_whitespace() => {
                if started {
                    argv.push(std::mem::take(&mut argument));
                    started = false;
                }
            }
            (_, character) => {
                argument.push(character);
                started = true;
            }
        }
    }

    if escaped {
        return Err(CommandParseError::TrailingEscape);
    }
    if quoted.is_some() {
        return Err(CommandParseError::UnterminatedQuote);
    }
    if started {
        argv.push(argument);
    }
    let Some((program, args)) = argv.split_first() else {
        return Err(CommandParseError::Empty);
    };
    Ok((program.clone(), args.to_vec()))
}

/// Kill and reap a child process. Tokio intentionally does not reap a child
/// when its handle is dropped, so every adapter shutdown path must await this
/// helper before releasing the handle.
async fn terminate_child(child: &mut Child) -> AdapterResult<()> {
    #[cfg(unix)]
    if signal_isolated_process_group(child, nix::sys::signal::Signal::SIGTERM) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        signal_isolated_process_group(child, nix::sys::signal::Signal::SIGKILL);
    }
    let kill_error = child.start_kill().err();
    let wait_error = child.wait().await.err();
    if let Some(error) = kill_error.or(wait_error) {
        return Err(AdapterError::Transport(error.to_string()));
    }
    Ok(())
}

#[cfg(unix)]
fn signal_isolated_process_group(child: &Child, signal: nix::sys::signal::Signal) -> bool {
    use nix::{
        sys::signal::killpg,
        unistd::{Pid, getpgid, getpgrp},
    };

    let Some(raw_pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) else {
        return false;
    };
    let pid = Pid::from_raw(raw_pid);
    // `process_group(0)` makes the child its own group leader. Verify that
    // invariant at signal time and also reject CodeSwarm's own group. This
    // keeps descendant cleanup without any possibility of reaching the
    // containing shell or terminal multiplexer.
    if getpgid(Some(pid)).ok() == Some(pid) && pid != getpgrp() {
        let _ = killpg(pid, signal);
        true
    } else {
        false
    }
}

fn isolate_process_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
}

/// Drain diagnostics without allowing a noisy peer to block on stderr or
/// retain an unbounded failure report.
async fn drain_bounded<R>(mut reader: R, limit: usize) -> String
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    while let Ok(count) = reader.read(&mut chunk).await {
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > limit {
            let keep_from = bytes.len() - limit;
            bytes.drain(..keep_from);
        }
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// Read one JSONL frame without allowing a peer to allocate an unbounded
/// string before the size check runs. `AsyncBufReadExt::read_line` checks only
/// after it has appended the complete line, so the bounded fill/consume loop
/// below is intentional.
async fn read_bounded_line<R>(reader: &mut R) -> AdapterResult<String>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::with_capacity(4096);
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))?;
        if buffer.is_empty() {
            if bytes.is_empty() {
                return Err(AdapterError::Transport("ACP stream closed".into()));
            }
            break;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let available = newline.map_or(buffer.len(), |index| index + 1);
        let remaining = MAX_ACP_LINE_BYTES
            .saturating_add(1)
            .saturating_sub(bytes.len());
        if available > remaining {
            reader.consume(remaining);
            return Err(AdapterError::Protocol(format!(
                "ACP protocol line exceeds {MAX_ACP_LINE_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&buffer[..available]);
        reader.consume(available);
        if newline.is_some() {
            break;
        }
    }
    if bytes.len() > MAX_ACP_LINE_BYTES {
        return Err(AdapterError::Protocol(format!(
            "ACP protocol line exceeds {MAX_ACP_LINE_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes).map_err(|error| AdapterError::Protocol(error.to_string()))
}

async fn drain_terminal_output<R>(
    mut reader: R,
    output: Arc<Mutex<Vec<u8>>>,
    truncated: Arc<AtomicBool>,
    output_readers: Arc<AtomicUsize>,
    limit: usize,
) where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 4096];
    while let Ok(count) = reader.read(&mut chunk).await {
        if count == 0 {
            break;
        }
        if let Ok(mut bytes) = output.lock() {
            let remaining = limit.saturating_sub(bytes.len());
            if count > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated.store(true, Ordering::Release);
            } else {
                bytes.extend_from_slice(&chunk[..count]);
            }
        }
    }
    output_readers.fetch_sub(1, Ordering::AcqRel);
}

/// Uniform control plane for ACP and custom command-line adapters.
#[async_trait]
pub trait AgentAdapter: Send {
    fn slot(&self) -> RosterSlot;
    /// Human-readable adapter identity used in the first-turn roster context.
    /// Catalog-backed launchers can override this with their display name;
    /// direct command adapters still get a deterministic fallback.
    fn display_name(&self) -> String {
        format!("Agent {}", self.slot().saturating_add(1))
    }
    /// Return the protocol session handle when this adapter has one. Custom
    /// adapters may omit it; runtime metadata then still preserves roster
    /// identity without claiming the session is resumable.
    fn session_id(&self) -> Option<String> {
        None
    }
    /// Stable protocol label recorded in the runtime session snapshot.
    /// Custom adapters may override this when they expose a resumable
    /// protocol distinct from the built-in native and ACP bridges.
    fn protocol(&self) -> &'static str {
        "custom"
    }
    fn capabilities(&self) -> AgentCapabilities;
    async fn start(&mut self) -> AdapterResult<()>;
    async fn send_prompt(&mut self, prompt: String) -> AdapterResult<()>;
    async fn cancel(&mut self) -> AdapterResult<bool>;
    async fn answer_permission(
        &mut self,
        request_id: String,
        answer: PermissionAnswer,
    ) -> AdapterResult<()>;
    async fn set_mode(&mut self, mode: String) -> AdapterResult<()>;
    async fn set_model(&mut self, _model: String) -> AdapterResult<()> {
        Err(AdapterError::Unsupported("set_model"))
    }
    async fn reload(&mut self) -> AdapterResult<()>;
    async fn stop(&mut self) -> AdapterResult<()>;
    async fn next_event(&mut self) -> Option<AdapterResult<AgentEvent>>;
}

/// Preserve a stable CodeSwarm roster slot when an already-running adapter is
/// moved to another logical position (for example, a roster reorder). Native
/// protocol implementations keep their original slot internally, while this
/// boundary rewrites the normalized event identity seen by the reducer.
struct SlotMappedAdapter {
    logical_slot: RosterSlot,
    inner: Box<dyn AgentAdapter>,
}

impl std::fmt::Debug for SlotMappedAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SlotMappedAdapter")
            .field("logical_slot", &self.logical_slot)
            .field("inner_slot", &self.inner.slot())
            .finish_non_exhaustive()
    }
}

fn map_event_slot(event: AgentEvent, slot: RosterSlot) -> AgentEvent {
    match event {
        AgentEvent::GoalUpdated { goal } => AgentEvent::GoalUpdated { goal },
        AgentEvent::RosterUpdated { update } => AgentEvent::RosterUpdated { update },
        AgentEvent::Ready { capabilities, .. } => AgentEvent::Ready { slot, capabilities },
        AgentEvent::TurnStarted { .. } => AgentEvent::TurnStarted { slot },
        AgentEvent::ModesReplaced {
            modes,
            current_mode,
            ..
        } => AgentEvent::ModesReplaced {
            slot,
            modes,
            current_mode,
        },
        AgentEvent::ModeUpdated { current_mode, .. } => {
            AgentEvent::ModeUpdated { slot, current_mode }
        }
        AgentEvent::ModelsReplaced {
            config_id,
            models,
            current_model,
            ..
        } => AgentEvent::ModelsReplaced {
            slot,
            config_id,
            models,
            current_model,
        },
        AgentEvent::ModelUpdated { current_model, .. } => AgentEvent::ModelUpdated {
            slot,
            current_model,
        },
        AgentEvent::UserText { text, .. } => AgentEvent::UserText { slot, text },
        AgentEvent::CommandsReplaced { commands, .. } => {
            AgentEvent::CommandsReplaced { slot, commands }
        }
        AgentEvent::UsageUpdated { usage, .. } => AgentEvent::UsageUpdated { slot, usage },
        AgentEvent::Text { text, .. } => AgentEvent::Text { slot, text },
        AgentEvent::Thought { text, .. } => AgentEvent::Thought { slot, text },
        AgentEvent::Tool { update, .. } => AgentEvent::Tool { slot, update },
        AgentEvent::Permission { request, .. } => AgentEvent::Permission { slot, request },
        AgentEvent::Terminal { event, .. } => AgentEvent::Terminal { slot, event },
        AgentEvent::TurnComplete { .. } => AgentEvent::TurnComplete { slot },
        AgentEvent::UsageLimitReached { detail, .. } => {
            AgentEvent::UsageLimitReached { slot, detail }
        }
        AgentEvent::Failed {
            started, detail, ..
        } => AgentEvent::Failed {
            slot,
            started,
            detail,
        },
    }
}

#[async_trait]
impl AgentAdapter for SlotMappedAdapter {
    fn slot(&self) -> RosterSlot {
        self.logical_slot
    }

    fn display_name(&self) -> String {
        self.inner.display_name()
    }

    fn session_id(&self) -> Option<String> {
        self.inner.session_id()
    }

    fn protocol(&self) -> &'static str {
        self.inner.protocol()
    }

    fn capabilities(&self) -> AgentCapabilities {
        self.inner.capabilities()
    }

    async fn start(&mut self) -> AdapterResult<()> {
        self.inner.start().await
    }

    async fn send_prompt(&mut self, prompt: String) -> AdapterResult<()> {
        self.inner.send_prompt(prompt).await
    }

    async fn cancel(&mut self) -> AdapterResult<bool> {
        self.inner.cancel().await
    }

    async fn answer_permission(
        &mut self,
        request_id: String,
        answer: PermissionAnswer,
    ) -> AdapterResult<()> {
        self.inner.answer_permission(request_id, answer).await
    }

    async fn set_mode(&mut self, mode: String) -> AdapterResult<()> {
        self.inner.set_mode(mode).await
    }

    async fn set_model(&mut self, model: String) -> AdapterResult<()> {
        self.inner.set_model(model).await
    }

    async fn reload(&mut self) -> AdapterResult<()> {
        self.inner.reload().await
    }

    async fn stop(&mut self) -> AdapterResult<()> {
        self.inner.stop().await
    }

    async fn next_event(&mut self) -> Option<AdapterResult<AgentEvent>> {
        let slot = self.logical_slot;
        self.inner
            .next_event()
            .await
            .map(|result| result.map(|event| map_event_slot(event, slot)))
    }
}

/// Owns one adapter and feeds normalized events through the deterministic core
/// reducer. The UI consumes effects and state snapshots.
pub struct AdapterHost {
    adapter: Box<dyn AgentAdapter>,
    pub state: SessionState,
    pub last_error: Option<String>,
    event_log: Option<EventLog>,
}

impl std::fmt::Debug for AdapterHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdapterHost")
            .field("state", &self.state)
            .field("last_error", &self.last_error)
            .field("event_log", &self.event_log)
            .finish_non_exhaustive()
    }
}

impl AdapterHost {
    pub fn new(adapter: Box<dyn AgentAdapter>, event_log: Option<EventLog>) -> Self {
        let slot = adapter.slot();
        Self {
            adapter,
            state: SessionState::new(slot.saturating_add(1)),
            last_error: None,
            event_log,
        }
    }

    pub async fn start(&mut self) -> AdapterResult<()> {
        self.adapter.start().await
    }

    pub async fn send_prompt(&mut self, prompt: String) -> AdapterResult<()> {
        self.adapter.send_prompt(prompt).await
    }

    pub async fn cancel(&mut self) -> AdapterResult<bool> {
        self.adapter.cancel().await
    }

    pub async fn answer_permission(
        &mut self,
        request_id: String,
        answer: PermissionAnswer,
    ) -> AdapterResult<()> {
        self.adapter.answer_permission(request_id, answer).await
    }

    pub async fn set_mode(&mut self, mode: String) -> AdapterResult<()> {
        self.adapter.set_mode(mode).await
    }

    pub async fn set_model(&mut self, model: String) -> AdapterResult<()> {
        self.adapter.set_model(model).await
    }

    pub async fn reload(&mut self) -> AdapterResult<()> {
        self.adapter.reload().await?;
        let slot = self.adapter.slot();
        if let Some(agent) = self.state.slots.get_mut(slot) {
            agent.active = true;
            agent.capabilities = self.adapter.capabilities();
        }
        self.last_error = None;
        Ok(())
    }

    pub async fn stop(&mut self) -> AdapterResult<()> {
        self.adapter.stop().await
    }

    pub async fn next_effects(&mut self) -> Option<AdapterResult<Vec<Effect>>> {
        Some(self.next_update().await?.map(|update| update.effects))
    }

    pub async fn next_update(&mut self) -> Option<AdapterResult<HostUpdate>> {
        let event = match self.adapter.next_event().await {
            None => return None,
            Some(Err(error)) => {
                self.last_error = Some(error.to_string());
                let slot = self.adapter.slot();
                let failure = AgentEvent::Failed {
                    slot,
                    started: true,
                    detail: error.to_string(),
                };
                let effects = reduce(&mut self.state, failure.clone());
                return Some(Ok(HostUpdate {
                    event: failure,
                    effects,
                }));
            }
            Some(Ok(event)) => event,
        };
        if let Some(log) = &self.event_log
            && let Err(error) = log.append(&event)
        {
            return Some(Err(AdapterError::Transport(error.to_string())));
        }
        let effects = reduce(&mut self.state, event.clone());
        Some(Ok(HostUpdate { event, effects }))
    }

    pub fn adapter(&self) -> &dyn AgentAdapter {
        &*self.adapter
    }

    pub fn session_id(&self) -> Option<String> {
        self.adapter.session_id()
    }

    /// Move this host to a new logical roster slot. The adapter process is not
    /// restarted; only normalized event identities and reducer state move.
    fn remap(self, logical_slot: RosterSlot) -> Self {
        let old_slot = self.adapter.slot();
        if old_slot == logical_slot {
            return self;
        }

        let mut state = self.state;
        if state.slots.len() <= logical_slot {
            state
                .slots
                .resize(logical_slot.saturating_add(1), Default::default());
        }
        if let Some(agent) = state.slots.get(old_slot).cloned() {
            state.slots[logical_slot] = agent;
        }
        if state.active_slot == Some(old_slot) {
            state.active_slot = Some(logical_slot);
        }
        for (slot, _) in &mut state.queued_prompts {
            if *slot == old_slot {
                *slot = logical_slot;
            }
        }
        for (slot, _) in &mut state.public_text {
            if *slot == old_slot {
                *slot = logical_slot;
            }
        }

        Self {
            adapter: Box::new(SlotMappedAdapter {
                logical_slot,
                inner: self.adapter,
            }),
            state,
            last_error: self.last_error,
            event_log: self.event_log,
        }
    }
}

/// Sequential multi-adapter runner. It intentionally never polls two
/// adapters concurrently: the next prompt depends on the prior response.
pub struct RelayHost {
    goal: Option<crate::goal::Goal>,
    hosts: Vec<AdapterHost>,
    relay: Relay,
    introduced: Vec<bool>,
    roster_names: Vec<String>,
    roster_identities: Vec<String>,
    roster_launch_specs: Vec<(String, String)>,
    desired_policy: String,
    metadata_writer: Option<BufferedSessionMetadataStore>,
    metadata_workspace: Option<String>,
    dispatches: Vec<(RosterSlot, String)>,
    event_sink: Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    cancel_requested: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

/// A clonable signal used by a terminal control loop to interrupt the active
/// relay turn without borrowing the relay while its adapter is being polled.
#[derive(Clone, Debug)]
pub struct RelayCancellation {
    requested: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

/// A permission response delivered while a relay turn is still streaming.
///
/// Relay turns own the active adapter mutably, so permission answers must be
/// consumed inside that turn rather than deferred by the outer coordinator.
#[derive(Debug)]
pub struct RelayPermissionAnswer {
    pub slot: RosterSlot,
    pub request_id: String,
    pub answer: PermissionAnswer,
}

#[cfg(test)]
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(not(test))]
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
const CANCEL_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(20);
#[cfg(not(test))]
const CANCEL_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Bound third-party cancellation hooks so a broken adapter cannot freeze the
/// terminal control loop.  Native and ACP adapters normally return promptly;
/// the timeout is for custom adapters whose cancellation future may never
/// resolve.
async fn cancel_with_timeout(host: &mut AdapterHost) -> AdapterResult<bool> {
    tokio::time::timeout(CANCEL_TIMEOUT, host.cancel())
        .await
        .map_err(|_| AdapterError::Transport("adapter cancellation timed out".into()))?
}

fn canonical_policy_id(policy: &str) -> &str {
    match policy {
        "plan" => "codeswarm:mode:plan",
        "default" | "manual" => "codeswarm:mode:manual",
        "accept-edits" => "codeswarm:mode:accept-edits",
        "full-access" | "auto" | "autopilot" => "codeswarm:mode:full-access",
        other => other,
    }
}

async fn apply_policy_to_host(host: &mut AdapterHost, policy: &str) -> AdapterResult<()> {
    if !host.adapter().capabilities().supports_modes {
        return Ok(());
    }
    let policy_id = canonical_policy_id(policy);
    let slot = host.adapter().slot();
    let advertised = host
        .state
        .slots
        .get(slot)
        .map(|agent| agent.modes.as_slice())
        .unwrap_or_default();
    let native = if advertised.is_empty() {
        match policy_id {
            "codeswarm:mode:plan" => "plan".into(),
            "codeswarm:mode:manual" => "default".into(),
            "codeswarm:mode:accept-edits" => "accept-edits".into(),
            "codeswarm:mode:full-access" => "full-access".into(),
            other => other.into(),
        }
    } else {
        crate::policy::resolve(policy_id, advertised)
            .map(|mode| mode.id)
            .ok_or(AdapterError::Unsupported(
                "desired policy is unavailable for adapter",
            ))?
    };
    host.set_mode(native).await
}

async fn refresh_mode_catalog(
    host: &mut AdapterHost,
    event_sink: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> AdapterResult<bool> {
    if !host.adapter().capabilities().supports_modes {
        return Ok(false);
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut ready_seen = false;
        loop {
            let update = host.next_update().await.ok_or_else(|| {
                AdapterError::Transport("adapter ended before advertising modes".into())
            })??;
            let catalog_ready = matches!(update.event, AgentEvent::ModesReplaced { .. });
            ready_seen |= matches!(update.event, AgentEvent::Ready { .. });
            if let Some(sink) = event_sink {
                sink(update.event);
            }
            if catalog_ready {
                return Ok(ready_seen);
            }
        }
    })
    .await
    .map_err(|_| AdapterError::Transport("adapter mode catalog timed out".into()))?
}

async fn refresh_adapter_startup(
    host: &mut AdapterHost,
    event_sink: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
) -> AdapterResult<()> {
    let ready_seen = refresh_mode_catalog(host, event_sink).await?;
    if ready_seen || !matches!(host.adapter().protocol(), "native" | "acp") {
        return Ok(());
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let update = host.next_update().await.ok_or_else(|| {
                AdapterError::Transport("adapter ended before becoming ready".into())
            })??;
            let ready = matches!(update.event, AgentEvent::Ready { .. });
            if let Some(sink) = event_sink {
                sink(update.event);
            }
            if ready {
                return Ok(());
            }
        }
    })
    .await
    .map_err(|_| AdapterError::Transport("adapter ready handshake timed out".into()))?
}

fn public_context_speaker(name: &str) -> String {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    format!("{name} {:02}:{:02}", now.hour(), now.minute())
}

impl RelayCancellation {
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.notify.notify_one();
    }
}

impl std::fmt::Debug for RelayHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayHost")
            .field("hosts", &self.hosts)
            .field("relay", &self.relay)
            .field("dispatches", &self.dispatches)
            .field("event_sink", &self.event_sink.is_some())
            .field(
                "cancel_requested",
                &self.cancel_requested.load(Ordering::Acquire),
            )
            .finish()
    }
}

impl RelayHost {
    pub fn new(hosts: Vec<AdapterHost>, max_rounds: usize) -> Result<Self, AdapterError> {
        if hosts.is_empty() {
            return Err(AdapterError::Unsupported("relay requires an adapter"));
        }
        Ok(Self {
            relay: Relay::new(hosts.len(), max_rounds),
            introduced: vec![false; hosts.len()],
            roster_names: hosts
                .iter()
                .map(|host| host.adapter().display_name())
                .collect(),
            roster_identities: hosts
                .iter()
                .map(|host| host.adapter().display_name())
                .collect(),
            roster_launch_specs: Vec::new(),
            desired_policy: crate::policy::DEFAULT_POLICY_ID.into(),
            metadata_writer: None,
            metadata_workspace: None,
            hosts,
            dispatches: Vec::new(),
            goal: None,
            event_sink: None,
            cancel_requested: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(Notify::new()),
        })
    }

    /// Send each normalized event to a client while a turn is being drained.
    /// The callback runs synchronously on the relay task and should only
    /// enqueue the event; expensive rendering must happen outside the callback.
    pub fn set_event_sink<F>(&mut self, sink: F)
    where
        F: Fn(AgentEvent) + Send + Sync + 'static,
    {
        self.event_sink = Some(Arc::new(sink));
    }

    /// Install catalog-backed names for first-turn introductions. Direct
    /// scripted/custom adapters retain deterministic `Agent N` fallbacks.
    pub fn set_roster_names(&mut self, names: Vec<String>) {
        if names.len() == self.hosts.len() {
            self.roster_names = names;
        }
    }

    /// Install catalog identities used by the launcher and persisted session
    /// metadata. Names remain a separate display concern, so custom adapters
    /// can retain friendly labels while still restoring by stable identity.
    pub fn set_roster_identities(&mut self, identities: Vec<String>) {
        if identities.len() == self.hosts.len() {
            self.roster_identities = identities;
        }
    }

    pub fn set_roster_launch_specs(&mut self, specs: Vec<(String, String)>) {
        if specs.len() == self.hosts.len() {
            self.roster_launch_specs = specs;
        }
    }

    /// Attach a background metadata writer. Runtime changes enqueue complete
    /// snapshots; no metadata filesystem work runs on the terminal thread.
    pub fn set_session_metadata_writer(&mut self, writer: BufferedSessionMetadataStore) {
        self.metadata_writer = Some(writer);
    }

    pub fn set_session_metadata_workspace(&mut self, workspace: impl Into<String>) {
        self.metadata_workspace = Some(workspace.into());
    }

    /// Build the coordinator-owned session snapshot for the active roster.
    pub fn session_metadata(&self) -> SessionMetadata {
        let active = self.relay.active_slots().collect::<Vec<_>>();
        let mut data = serde_json::Map::new();
        data.insert(
            "goal".into(),
            serde_json::to_value(&self.goal).expect("goal serializes"),
        );
        if let Some(workspace) = &self.metadata_workspace {
            data.insert("cwd".into(), serde_json::Value::String(workspace.clone()));
        }
        data.insert(
            "title".into(),
            serde_json::Value::String("CodeSwarm".into()),
        );
        data.insert(
            "agents".into(),
            serde_json::Value::Array(
                active
                    .into_iter()
                    .filter_map(|slot| {
                        let host = self.hosts.get(slot)?;
                        let (protocol, command) = self.roster_launch_specs.get(slot)?;
                        let mut agent = serde_json::Map::new();
                        agent.insert(
                            "name".into(),
                            serde_json::Value::String(
                                self.roster_names
                                    .get(slot)
                                    .cloned()
                                    .unwrap_or_else(|| host.adapter().display_name()),
                            ),
                        );
                        agent.insert(
                            "identity".into(),
                            serde_json::Value::String(
                                self.roster_identities
                                    .get(slot)
                                    .cloned()
                                    .unwrap_or_else(|| host.adapter().display_name()),
                            ),
                        );
                        agent.insert(
                            "protocol".into(),
                            serde_json::Value::String(protocol.clone()),
                        );
                        agent.insert("command".into(), serde_json::Value::String(command.clone()));
                        agent.insert(
                            "supports_load_session".into(),
                            serde_json::Value::Bool(
                                host.adapter().capabilities().supports_session_load,
                            ),
                        );
                        if let Some(session_id) = host.session_id() {
                            agent
                                .insert("session_id".into(), serde_json::Value::String(session_id));
                        }
                        Some(serde_json::Value::Object(agent))
                    })
                    .collect(),
            ),
        );
        SessionMetadata::new(data)
    }

    fn queue_session_metadata(&self) -> AdapterResult<()> {
        if let Some(writer) = &self.metadata_writer {
            writer
                .write(self.session_metadata())
                .map_err(|error| AdapterError::Transport(error.to_string()))?;
        }
        Ok(())
    }

    pub fn restore_goal(&mut self, goal: Option<crate::goal::Goal>) {
        self.goal = goal;
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::GoalUpdated {
                goal: self.goal.clone(),
            });
        }
    }

    pub fn apply_goal(
        &mut self,
        command: crate::goal::GoalCommand,
    ) -> Result<Option<String>, String> {
        let task = crate::goal::apply(&mut self.goal, command)?;
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::GoalUpdated {
                goal: self.goal.clone(),
            });
        }
        self.queue_session_metadata()
            .map_err(|error| error.to_string())?;
        Ok(task)
    }

    pub fn roster_names(&self) -> &[String] {
        &self.roster_names
    }

    pub fn session_ids(&self) -> Vec<Option<String>> {
        self.hosts.iter().map(AdapterHost::session_id).collect()
    }

    pub async fn start(&mut self) -> AdapterResult<()> {
        let event_sink = self.event_sink.clone();
        let startups = self.hosts.iter_mut().map(|host| {
            let event_sink = event_sink.clone();
            async move {
                host.start().await?;
                refresh_adapter_startup(host, &event_sink).await
            }
        });
        let results = futures::future::join_all(startups).await;
        if let Some(error) = results.into_iter().find_map(Result::err) {
            // Startup is transactional even though independent adapters are
            // warmed concurrently. Every host gets a cleanup attempt.
            for host in &mut self.hosts {
                let _ = host.stop().await;
            }
            return Err(error);
        }
        if let Err(error) = self
            .set_policy(crate::policy::DEFAULT_POLICY_ID.into())
            .await
        {
            for host in &mut self.hosts {
                let _ = host.stop().await;
            }
            return Err(error);
        }
        let _ = self.queue_session_metadata();
        Ok(())
    }

    pub async fn stop(&mut self) -> AdapterResult<()> {
        // A third-party adapter can fail during shutdown (for example after
        // its transport has already disappeared). Always give every roster
        // member a chance to clean up, then return the first error so callers
        // still get an actionable failure without leaking later processes.
        let mut first_error = None;
        for host in &mut self.hosts {
            if let Err(error) = host.stop().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(writer) = &self.metadata_writer
            && let Err(error) = writer.flush()
            && first_error.is_none()
        {
            first_error = Some(AdapterError::Transport(error.to_string()));
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Forward a normalized permission answer to the adapter owning `slot`.
    /// This keeps protocol-specific response framing out of the relay and UI.
    pub async fn answer_permission(
        &mut self,
        slot: RosterSlot,
        request_id: String,
        answer: PermissionAnswer,
    ) -> AdapterResult<()> {
        let host = self
            .hosts
            .get_mut(slot)
            .ok_or_else(|| AdapterError::Transport("permission target is missing".into()))?;
        host.answer_permission(request_id, answer).await
    }

    pub fn pause(&mut self) {
        self.relay.pause();
    }

    pub fn resume(&mut self) {
        self.relay.resume();
    }

    /// Select how future non-direct turns are routed. Queued prompts and the
    /// shared context remain intact when a user changes this setting.
    pub fn set_strategy(&mut self, strategy: CollaborationStrategy) {
        self.relay.set_strategy(strategy);
    }

    pub fn strategy(&self) -> CollaborationStrategy {
        self.relay.strategy()
    }

    pub fn roster_identity(&self, slot: RosterSlot) -> Option<&str> {
        self.roster_identities.get(slot).map(String::as_str)
    }

    pub fn active_slot_for_identity(&self, identity: &str) -> Option<RosterSlot> {
        self.relay.active_slots().find(|slot| {
            self.roster_identity(*slot)
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(identity))
        })
    }

    /// Apply one semantic mode to every active adapter that advertises mode
    /// support. Native adapters may translate the policy to their own IDs.
    pub async fn set_mode(&mut self, mode: String) -> AdapterResult<()> {
        let active = self.relay.active_slots().collect::<Vec<_>>();
        for slot in active {
            let Some(host) = self.hosts.get_mut(slot) else {
                continue;
            };
            if host.adapter().capabilities().supports_modes {
                host.set_mode(mode.clone()).await?;
            }
        }
        Ok(())
    }

    pub async fn set_model(&mut self, slot: RosterSlot, model: String) -> AdapterResult<()> {
        let host = self
            .hosts
            .get_mut(slot)
            .ok_or_else(|| AdapterError::Transport("model target is missing".into()))?;
        if !host.adapter().capabilities().supports_models {
            return Err(AdapterError::Unsupported("set_model"));
        }
        host.set_model(model.clone()).await?;
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::ModelUpdated {
                slot,
                current_model: model,
            });
        }
        Ok(())
    }

    /// Translate a CodeSwarm semantic policy to each adapter's currently
    /// advertised native mode, falling back to conventional IDs before the
    /// first catalog update arrives.
    pub async fn set_policy(&mut self, policy: String) -> AdapterResult<()> {
        self.desired_policy = canonical_policy_id(&policy).to_owned();
        let desired_policy = self.desired_policy.clone();
        let mut first_error = None;
        let active = self.relay.active_slots().collect::<Vec<_>>();
        for active_slot in active {
            let Some(host) = self.hosts.get_mut(active_slot) else {
                continue;
            };
            if let Err(error) = apply_policy_to_host(host, &desired_policy).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub async fn reload(&mut self, slot: RosterSlot) -> AdapterResult<()> {
        let desired_policy = self.desired_policy.clone();
        let event_sink = self.event_sink.clone();
        let host = self
            .hosts
            .get_mut(slot)
            .ok_or_else(|| AdapterError::Transport("reload target is missing".into()))?;
        host.reload().await?;
        refresh_adapter_startup(host, &event_sink).await?;
        apply_policy_to_host(host, &desired_policy).await?;
        if let Some(introduced) = self.introduced.get_mut(slot) {
            *introduced = false;
        }
        self.relay
            .reactivate(slot)
            .map_err(|error| AdapterError::Transport(error.into()))?;
        let _ = self.queue_session_metadata();
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::RosterUpdated {
                update: RosterUpdate::Reloaded { slot },
            });
        }
        let _ = self.relay.clear_limited(slot);
        Ok(())
    }

    /// Stop and tombstone an agent while preserving its stable roster slot.
    pub async fn drop_agent(&mut self, slot: RosterSlot) -> AdapterResult<()> {
        self.relay
            .drop_agent(slot)
            .map_err(|error| AdapterError::Transport(error.into()))?;
        let _stop_result = if let Some(host) = self.hosts.get_mut(slot) {
            host.stop().await
        } else {
            Ok(())
        };
        // Persist the tombstone even when a third-party process reports a
        // shutdown error; otherwise the next launch can resurrect a slot the
        // user explicitly removed.
        let _ = self.queue_session_metadata();
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::RosterUpdated {
                update: RosterUpdate::Dropped { slot },
            });
        }
        Ok(())
    }

    /// Start and append a new adapter in the next stable roster slot. The
    /// adapter is started before it becomes visible to the relay, so a failed
    /// add cannot leave a half-active slot behind.
    pub async fn add_agent(
        &mut self,
        mut host: AdapterHost,
        name: impl Into<String>,
        identity: impl Into<String>,
        command: impl Into<String>,
    ) -> AdapterResult<RosterSlot> {
        let slot = self.hosts.len();
        if host.adapter().slot() != slot {
            return Err(AdapterError::Transport(
                "new adapter slot must append after the existing roster".into(),
            ));
        }
        if let Err(error) = host.start().await {
            let _ = host.stop().await;
            return Err(error);
        }
        if let Err(error) = refresh_adapter_startup(&mut host, &self.event_sink).await {
            let _ = host.stop().await;
            return Err(error);
        }
        if let Err(error) = apply_policy_to_host(&mut host, &self.desired_policy).await {
            let _ = host.stop().await;
            return Err(error);
        }
        let capabilities = host.adapter().capabilities();
        self.hosts.push(host);
        self.relay.add_agent();
        self.introduced.push(false);
        let name = name.into();
        let identity = identity.into();
        self.roster_names.push(name.clone());
        self.roster_identities.push(identity.clone());
        self.roster_launch_specs
            .push((self.hosts[slot].adapter().protocol().into(), command.into()));
        let _ = self.queue_session_metadata();
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::RosterUpdated {
                update: RosterUpdate::Added {
                    slot,
                    name,
                    identity,
                },
            });
            sink(AgentEvent::Ready { slot, capabilities });
        }
        Ok(slot)
    }

    /// Reorder two live adapters without restarting either process. The
    /// logical slot wrapper keeps normalized events, reducer state, and
    /// permission routing aligned with the new roster order.
    pub fn swap_agents(&mut self, first: RosterSlot, second: RosterSlot) -> AdapterResult<()> {
        if first == second {
            return Ok(());
        }
        if first >= self.hosts.len() || second >= self.hosts.len() {
            return Err(AdapterError::Transport("roster slot out of range".into()));
        }
        self.relay
            .swap_agents(first, second)
            .map_err(|error| AdapterError::Transport(error.into()))?;
        let low = first.min(second);
        let high = first.max(second);
        let high_host = self.hosts.remove(high);
        let low_host = self.hosts.remove(low);
        self.hosts.insert(low, high_host.remap(low));
        self.hosts.insert(high, low_host.remap(high));
        self.roster_names.swap(first, second);
        self.roster_identities.swap(first, second);
        if self.roster_launch_specs.len() == self.hosts.len() {
            self.roster_launch_specs.swap(first, second);
        }
        self.introduced.swap(first, second);
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::RosterUpdated {
                update: RosterUpdate::Swapped { first, second },
            });
            sink(AgentEvent::Ready {
                slot: first,
                capabilities: self.hosts[first].adapter().capabilities(),
            });
            sink(AgentEvent::Ready {
                slot: second,
                capabilities: self.hosts[second].adapter().capabilities(),
            });
        }
        let _ = self.queue_session_metadata();
        Ok(())
    }

    pub fn relay(&self) -> &Relay {
        &self.relay
    }

    pub fn next_slot(&self) -> RosterSlot {
        self.hosts.len()
    }

    pub fn relay_mut(&mut self) -> &mut Relay {
        &mut self.relay
    }

    pub fn cancellation(&self) -> RelayCancellation {
        RelayCancellation {
            requested: Arc::clone(&self.cancel_requested),
            notify: Arc::clone(&self.cancel_notify),
        }
    }

    /// Prompts sent to adapters, in causal dispatch order. This is useful to
    /// diagnostics and makes the context-routing boundary observable without
    /// exposing protocol-specific adapter internals.
    pub fn dispatches(&self) -> &[(RosterSlot, String)] {
        &self.dispatches
    }

    pub async fn run_turn(
        &mut self,
        task: impl Into<String>,
        first_slot: RosterSlot,
    ) -> AdapterResult<RelayDecision> {
        self.run_turn_inner(task.into(), first_slot, None).await
    }

    pub async fn run_turn_with_permissions(
        &mut self,
        task: impl Into<String>,
        first_slot: RosterSlot,
        permissions: &mut tokio::sync::mpsc::UnboundedReceiver<RelayPermissionAnswer>,
    ) -> AdapterResult<RelayDecision> {
        self.run_turn_inner(task.into(), first_slot, Some(permissions))
            .await
    }

    async fn run_turn_inner(
        &mut self,
        task: String,
        first_slot: RosterSlot,
        mut permissions: Option<&mut tokio::sync::mpsc::UnboundedReceiver<RelayPermissionAnswer>>,
    ) -> AdapterResult<RelayDecision> {
        let decision = self.relay.begin(task, first_slot);
        let RelayDecision::Dispatch {
            slot,
            prompt,
            direct,
            can_stop,
        } = &decision
        else {
            return Ok(decision);
        };
        let speaker_name = self
            .roster_names
            .get(*slot)
            .cloned()
            .unwrap_or_else(|| self.hosts[*slot].adapter().display_name());
        let unseen = self.relay.unseen_context(*slot);
        // A non-direct prompt with text is a new public human turn. Record it
        // after collecting the recipient's existing unseen context so that
        // the selected agent receives the prompt once, while every peer gets
        // it through the shared journal on its next turn. Recording before
        // adapter I/O also preserves the job when the selected turn is
        // cancelled.
        if !*direct && !prompt.trim().is_empty() {
            if self.relay.shared_task().is_none() {
                self.relay.set_shared_task(prompt.clone());
            }
            self.relay
                .record_public(public_context_speaker("User"), prompt.clone());
        }
        let prompt = if unseen.is_empty() {
            prompt.clone()
        } else {
            format!("{prompt}\n\nPublic updates:\n{unseen}")
        };
        let introduction = if !self.introduced.get(*slot).copied().unwrap_or(false) {
            let self_name = speaker_name.clone();
            let roster = self
                .relay
                .active_slots()
                .map(|candidate| {
                    let name = self
                        .roster_names
                        .get(candidate)
                        .cloned()
                        .unwrap_or_else(|| self.hosts[candidate].adapter().display_name());
                    let role = if candidate == *slot { " — you" } else { "" };
                    format!("{}. {name}{role}", candidate.saturating_add(1))
                })
                .collect::<Vec<_>>();
            let shared_task = self
                .relay
                .shared_task()
                .filter(|task| *task != prompt)
                .map(|task| format!("\n\nShared task:\n{task}"))
                .unwrap_or_default();
            format!(
                "You are {self_name}.\nCodeSwarm roster (ordered):\n{}\n\
                 Turns relay sequentially through this roster. Treat the user request as the shared task; use timestamped public updates as conversation context.{shared_task}",
                roster.join("\n")
            )
        } else {
            String::new()
        };
        let prompt = format!(
            "{introduction}{separator}{prompt}\n\n{}",
            if *can_stop {
                format!(
                    "You are reviewing another agent. If no meaningful correction is needed,\nreply with an optional emoji followed by {STOP_TOKEN} on the final line.\nCodeSwarm hides the token and ends this review batch."
                )
            } else {
                format!(
                    "Do not use {STOP_TOKEN} on this turn. Your response must be offered to another agent for review."
                )
            },
            separator = if introduction.is_empty() { "" } else { "\n\n" },
        );
        let event_sink = self.event_sink.clone();
        let host = self
            .hosts
            .get_mut(*slot)
            .ok_or_else(|| AdapterError::Transport("relay selected missing adapter".into()))?;
        let prompt = crate::goal::prompt(self.goal.as_ref(), &prompt);
        if let Err(error) = host.send_prompt(prompt.clone()).await {
            let limited =
                report_relay_failure(&mut self.relay, &event_sink, *slot, true, error.to_string());
            if limited {
                self.relay.finish(*slot, *direct, false);
            }
            let _ = self.queue_session_metadata();
            if limited {
                return Ok(decision);
            }
            return Err(error);
        }
        if let Some(sink) = &self.event_sink {
            sink(AgentEvent::TurnStarted { slot: *slot });
        }
        if let Some(introduced) = self.introduced.get_mut(*slot) {
            *introduced = true;
        }
        self.dispatches.push((*slot, prompt));
        let mut response = String::new();
        let mut emitted_text = 0usize;
        let completion_event = loop {
            if self.cancel_requested.swap(false, Ordering::AcqRel) {
                if let Err(error) = cancel_with_timeout(host).await {
                    let limited = report_relay_failure(
                        &mut self.relay,
                        &event_sink,
                        *slot,
                        true,
                        error.to_string(),
                    );
                    if limited {
                        self.relay.finish(*slot, *direct, false);
                    }
                    let _ = self.queue_session_metadata();
                    if limited {
                        return Ok(decision);
                    }
                    return Err(error);
                }
                return Err(AdapterError::Transport("relay turn cancelled".into()));
            }
            let update = tokio::select! {
                update = host.next_update() => match update {
                    Some(Ok(update)) => update,
                    Some(Err(error)) => {
                        let limited = report_relay_failure(
                            &mut self.relay,
                            &event_sink,
                            *slot,
                            true,
                            error.to_string(),
                        );
                        if limited {
                            self.relay.finish(*slot, *direct, false);
                        }
                        let _ = self.queue_session_metadata();
                        if limited {
                            return Ok(decision);
                        }
                        return Err(error);
                    }
                    None => {
                        let error = AdapterError::Transport("adapter ended during turn".into());
                        let limited = report_relay_failure(
                            &mut self.relay,
                            &event_sink,
                            *slot,
                            true,
                            error.to_string(),
                        );
                        if limited {
                            self.relay.finish(*slot, *direct, false);
                        }
                        let _ = self.queue_session_metadata();
                        if limited {
                            return Ok(decision);
                        }
                        return Err(error);
                    }
                },
                _ = self.cancel_notify.notified() => {
                    if !self.cancel_requested.swap(false, Ordering::AcqRel) {
                        continue;
                    }
                    if let Err(error) = cancel_with_timeout(host).await {
                        let limited = report_relay_failure(
                            &mut self.relay,
                            &event_sink,
                            *slot,
                            true,
                            error.to_string(),
                        );
                        if limited {
                            self.relay.finish(*slot, *direct, false);
                        }
                        let _ = self.queue_session_metadata();
                        if limited {
                            return Ok(decision);
                        }
                        return Err(error);
                    }
                    return Err(AdapterError::Transport("relay turn cancelled".into()));
                },
                permission = async {
                    match permissions.as_mut() {
                        Some(receiver) => receiver.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(permission) = permission else {
                        permissions = None;
                        continue;
                    };
                    if permission.slot != *slot {
                        return Err(AdapterError::Transport(
                            "permission response targets an inactive relay slot".into(),
                        ));
                    }
                    host.answer_permission(permission.request_id, permission.answer).await?;
                    continue;
                },
            };
            match &update.event {
                AgentEvent::Text { text, .. } => response.push_str(text),
                AgentEvent::TurnComplete { .. } => {
                    let visible_response = response.replace(STOP_TOKEN, "");
                    let visible_start = emitted_text.min(visible_response.len());
                    let visible_start = floor_char_boundary(&visible_response, visible_start);
                    if visible_start < visible_response.len()
                        && let Some(sink) = &self.event_sink
                    {
                        sink(AgentEvent::Text {
                            slot: *slot,
                            text: visible_response[visible_start..].to_owned(),
                        });
                    }
                    self.cancel_requested.store(false, Ordering::Release);
                    break update.event.clone();
                }
                AgentEvent::Failed {
                    started, detail, ..
                } => {
                    let limited = report_relay_failure(
                        &mut self.relay,
                        &event_sink,
                        *slot,
                        *started,
                        detail.clone(),
                    );
                    if limited {
                        self.relay.finish(*slot, *direct, false);
                    }
                    let _ = self.queue_session_metadata();
                    if limited {
                        return Ok(decision);
                    }
                    return Err(AdapterError::Transport(detail.clone()));
                }
                _ => {}
            }
            if let AgentEvent::Text { .. } = &update.event {
                // Only a possible split marker needs to wait for another chunk.
                let visible_response = response.replace(STOP_TOKEN, "");
                let visible_end = stop_token_visible_end(&visible_response);
                if emitted_text < visible_end {
                    if let Some(sink) = &self.event_sink {
                        sink(AgentEvent::Text {
                            slot: *slot,
                            text: visible_response[emitted_text..visible_end].to_owned(),
                        });
                    }
                    emitted_text = visible_end;
                }
            } else if let Some(sink) = &self.event_sink {
                sink(update.event.clone());
            }
        };
        let (response, requested_stop) = strip_stop_token(&response);
        let response = response.replace(STOP_TOKEN, "");
        let accepted_stop = requested_stop && *can_stop;
        let needs_stop_acknowledgment = accepted_stop && response.is_empty();
        let response = if needs_stop_acknowledgment {
            DEFAULT_STOP_ACKNOWLEDGMENT.to_owned()
        } else {
            response
        };
        // A token-only reviewer response is intentionally hidden, but the
        // UI still needs the documented visible acknowledgement. Streamed
        // text is normally emitted above; this synthetic acknowledgement is
        // the one case where there was no visible adapter chunk to forward.
        if needs_stop_acknowledgment && let Some(sink) = &self.event_sink {
            sink(AgentEvent::Text {
                slot: *slot,
                text: response.clone(),
            });
        }
        if let Some(sink) = &self.event_sink {
            sink(completion_event);
        }
        // A provider plan that ran out mid-turn routes future turns around
        // the agent instead of back into the exhausted quota.
        if is_usage_limit_response(&response) {
            let detail = response.clone();
            let _ = self.relay.mark_limited(*slot);
            // A normal finish: the ring cursor advances past the limited
            // agent so the batch continues with a healthy peer.
            self.relay.finish(*slot, *direct, false);
            self.queue_session_metadata()?;
            if let Some(sink) = &self.event_sink {
                sink(AgentEvent::UsageLimitReached {
                    slot: *slot,
                    detail,
                });
            }
            return Ok(decision);
        }
        if !*direct && !response.is_empty() {
            self.relay
                .record_public(public_context_speaker(&speaker_name), response);
        }
        self.relay.mark_context_seen(*slot);
        self.relay.finish(*slot, *direct, accepted_stop);
        self.queue_session_metadata()?;
        Ok(decision)
    }
}

fn report_relay_failure(
    relay: &mut Relay,
    event_sink: &Option<Arc<dyn Fn(AgentEvent) + Send + Sync>>,
    slot: RosterSlot,
    started: bool,
    detail: String,
) -> bool {
    // A quota rejection is not a crash: route around the agent without
    // tombstoning the slot so a recharge can restore it.
    if is_usage_limit_response(&detail) {
        let _ = relay.mark_limited(slot);
        if let Some(sink) = event_sink {
            sink(AgentEvent::UsageLimitReached { slot, detail });
        }
        return true;
    }
    let _ = relay.tombstone(slot);
    if let Some(sink) = event_sink {
        sink(AgentEvent::Failed {
            slot,
            started,
            detail,
        });
    }
    false
}

/// Deterministic in-memory adapter used for contract and relay tests.
#[derive(Debug)]
pub struct ScriptedAdapter {
    slot: RosterSlot,
    capabilities: AgentCapabilities,
    events: VecDeque<AdapterResult<AgentEvent>>,
    prompts: Vec<String>,
}

impl ScriptedAdapter {
    pub fn new(
        slot: RosterSlot,
        capabilities: AgentCapabilities,
        events: impl IntoIterator<Item = AgentEvent>,
    ) -> Self {
        Self {
            slot,
            capabilities,
            events: events.into_iter().map(Ok).collect(),
            prompts: Vec::new(),
        }
    }

    pub fn prompts(&self) -> &[String] {
        &self.prompts
    }
}

#[async_trait]
impl AgentAdapter for ScriptedAdapter {
    fn slot(&self) -> RosterSlot {
        self.slot
    }

    fn capabilities(&self) -> AgentCapabilities {
        self.capabilities.clone()
    }

    async fn start(&mut self) -> AdapterResult<()> {
        Ok(())
    }

    async fn send_prompt(&mut self, prompt: String) -> AdapterResult<()> {
        self.prompts.push(prompt);
        Ok(())
    }

    async fn cancel(&mut self) -> AdapterResult<bool> {
        Ok(self.capabilities.supports_cancel)
    }

    async fn answer_permission(
        &mut self,
        _request_id: String,
        _answer: PermissionAnswer,
    ) -> AdapterResult<()> {
        if self.capabilities.supports_permissions {
            Ok(())
        } else {
            Err(AdapterError::Unsupported("permission answer"))
        }
    }

    async fn set_mode(&mut self, _mode: String) -> AdapterResult<()> {
        if self.capabilities.supports_modes {
            Ok(())
        } else {
            Err(AdapterError::Unsupported("set_mode"))
        }
    }

    async fn reload(&mut self) -> AdapterResult<()> {
        Ok(())
    }

    async fn stop(&mut self) -> AdapterResult<()> {
        Ok(())
    }

    async fn next_event(&mut self) -> Option<AdapterResult<AgentEvent>> {
        self.events.pop_front()
    }
}

/// A direct stream-JSON adapter for Antigravity. It deliberately does not
/// pretend to be ACP; it translates its documented events into core events.
#[derive(Debug)]
pub struct AgyAdapter {
    slot: RosterSlot,
    cwd: PathBuf,
    command: String,
    mode: String,
    mode_policy: String,
    session_id: Option<String>,
    child: Option<Child>,
    sender: mpsc::Sender<AdapterResult<AgentEvent>>,
    receiver: mpsc::Receiver<AdapterResult<AgentEvent>>,
    /// Antigravity announces its conversation id in the `init` event. Keep it
    /// outside the stream task so the next prompt can resume the same native
    /// conversation without making the adapter reader/UI share a borrow.
    announced_session: Arc<Mutex<Option<String>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl AgyAdapter {
    pub fn new(slot: RosterSlot, cwd: PathBuf, command: impl Into<String>) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        Self {
            slot,
            cwd,
            command: command.into(),
            mode: "default".into(),
            mode_policy: "agy:full-access".into(),
            session_id: None,
            child: None,
            sender,
            receiver,
            announced_session: Arc::new(Mutex::new(None)),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_session_id(
        slot: RosterSlot,
        cwd: PathBuf,
        command: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        let mut adapter = Self::new(slot, cwd, command);
        adapter.session_id = Some(session_id.into());
        adapter
    }

    fn modes() -> Vec<Mode> {
        vec![
            Mode {
                id: "agy:full-access".into(),
                label: "Auto pilot".into(),
            },
            Mode {
                id: "agy:manual".into(),
                label: "Manual".into(),
            },
            Mode {
                id: "accept-edits".into(),
                label: "Accept Edits".into(),
            },
            Mode {
                id: "plan".into(),
                label: "Plan".into(),
            },
        ]
    }

    async fn emit(&self, event: AdapterResult<AgentEvent>) {
        let _ = self.sender.send(event).await;
    }
}

#[async_trait]
impl AgentAdapter for AgyAdapter {
    fn slot(&self) -> RosterSlot {
        self.slot
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    fn protocol(&self) -> &'static str {
        "native"
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            supports_cancel: true,
            supports_modes: true,
            supports_permissions: false,
            supports_terminals: true,
            supports_session_load: true,
            supports_models: false,
        }
    }

    async fn start(&mut self) -> AdapterResult<()> {
        self.cancel_requested.store(false, Ordering::Release);
        self.emit(Ok(AgentEvent::Ready {
            slot: self.slot,
            capabilities: self.capabilities(),
        }))
        .await;
        self.emit(Ok(AgentEvent::ModesReplaced {
            slot: self.slot,
            modes: Self::modes(),
            current_mode: Some(self.mode_policy.clone()),
        }))
        .await;
        Ok(())
    }

    async fn send_prompt(&mut self, prompt: String) -> AdapterResult<()> {
        if self.child.is_some() {
            return Err(AdapterError::Transport(
                "agent is already handling a turn".into(),
            ));
        }
        if self.session_id.is_none()
            && let Ok(session) = self.announced_session.lock()
        {
            self.session_id = session.clone();
        }
        self.cancel_requested.store(false, Ordering::Release);
        let (program, args) = parse_command_line(&self.command)
            .map_err(|error| AdapterError::Spawn(format!("invalid agent command: {error}")))?;
        let mut command = Command::new(program);
        isolate_process_group(&mut command);
        command
            .args(args)
            .arg("--print")
            .arg(prompt)
            .arg("--print-timeout")
            .arg("60m")
            .arg("--output-format")
            .arg("stream-json")
            .current_dir(&self.cwd)
            .env("CODESWARM_CWD", &self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session_id) = &self.session_id {
            command.arg("--conversation").arg(session_id);
        }
        if self.mode != "default" {
            command.arg("--mode").arg(&self.mode);
        }
        let mut child = command
            .spawn()
            .map_err(|error| AdapterError::Spawn(error.to_string()))?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = terminate_child(&mut child).await;
                return Err(AdapterError::Transport("agent has no stdout".into()));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = terminate_child(&mut child).await;
                return Err(AdapterError::Transport("agent has no stderr".into()));
            }
        };
        let sender = self.sender.clone();
        let slot = self.slot;
        let announced_session = Arc::clone(&self.announced_session);
        let cancel_requested = Arc::clone(&self.cancel_requested);
        tokio::spawn(async move {
            let stderr_task = tokio::spawn(async move {
                const MAX_STDERR: usize = 32 * 1024;
                let mut stderr = BufReader::new(stderr);
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 4096];
                while let Ok(count) = stderr.read(&mut chunk).await {
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    if bytes.len() > MAX_STDERR {
                        let keep_from = bytes.len() - MAX_STDERR;
                        bytes.drain(..keep_from);
                    }
                }
                String::from_utf8_lossy(&bytes).trim().to_owned()
            });
            let mut lines = BufReader::new(stdout).lines();
            let mut result: Option<Value> = None;
            let mut streamed_response = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let value = match serde_json::from_str::<Value>(&line) {
                    Ok(value) => value,
                    Err(_) => {
                        // Native stream-json can contain diagnostic junk on
                        // stdout. Match the Python adapter's tolerant stream
                        // behavior and wait for the final result instead of
                        // turning one malformed line into a dead turn.
                        continue;
                    }
                };
                if value.get("event").and_then(Value::as_str) == Some("init")
                    && let Some(session_id) = value
                        .get("conversation_id")
                        .or_else(|| value.get("conversationId"))
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    && let Ok(mut announced) = announced_session.lock()
                {
                    *announced = Some(session_id.to_owned());
                }
                if value.get("event").and_then(Value::as_str) == Some("result") {
                    result = value.get("result").cloned();
                }
                match parse_agy_value(slot, &value) {
                    Ok(Some(event)) => {
                        if matches!(event, AgentEvent::Text { .. }) {
                            streamed_response = true;
                        }
                        if sender.send(Ok(event)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                    }
                }
            }
            let stderr = stderr_task.await.ok().unwrap_or_default();
            let succeeded = cancel_requested.load(Ordering::Acquire)
                || result
                    .as_ref()
                    .and_then(|result| result.get("status"))
                    .and_then(Value::as_str)
                    == Some("SUCCESS");
            if succeeded {
                // Some native stream-json wrappers emit only lifecycle events
                // and put the complete answer in the final result object. The
                // Python adapter surfaced that answer; do the same, while
                // avoiding duplication when token chunks were already sent.
                if !streamed_response
                    && let Some(response) = result
                        .as_ref()
                        .and_then(|result| result.get("response"))
                        .and_then(Value::as_str)
                        .filter(|response| !response.is_empty())
                {
                    let _ = sender
                        .send(Ok(AgentEvent::Text {
                            slot,
                            text: response.to_owned(),
                        }))
                        .await;
                }
                let _ = sender.send(Ok(AgentEvent::TurnComplete { slot })).await;
            } else {
                let detail = result
                    .as_ref()
                    .and_then(|result| result.get("error"))
                    .and_then(Value::as_str)
                    .filter(|detail| !detail.is_empty())
                    .map(str::to_owned)
                    .or_else(|| (!stderr.is_empty()).then_some(stderr))
                    .unwrap_or_else(|| "native stream ended before a successful result".into());
                let _ = sender
                    .send(Ok(AgentEvent::Failed {
                        slot,
                        started: true,
                        detail,
                    }))
                    .await;
            }
        });
        self.child = Some(child);
        Ok(())
    }

    async fn cancel(&mut self) -> AdapterResult<bool> {
        self.cancel_requested.store(true, Ordering::Release);
        let Some(mut child) = self.child.take() else {
            return Ok(false);
        };
        // `Child` does not reap itself when dropped. Awaiting wait after the
        // kill keeps repeated prompts from accumulating zombies, especially
        // when cancellation happens before the stream reader observes EOF.
        terminate_child(&mut child).await?;
        let _ = tokio::time::timeout(CANCEL_SETTLE_TIMEOUT, async {
            while let Some(event) = self.receiver.recv().await {
                if matches!(
                    event,
                    Ok(AgentEvent::TurnComplete { .. } | AgentEvent::Failed { .. })
                ) {
                    break;
                }
            }
        })
        .await;
        Ok(true)
    }

    async fn answer_permission(
        &mut self,
        _request_id: String,
        _answer: PermissionAnswer,
    ) -> AdapterResult<()> {
        Err(AdapterError::Unsupported("permission answer"))
    }

    async fn set_mode(&mut self, mode: String) -> AdapterResult<()> {
        let (mode, mode_policy) = match mode.as_str() {
            "full-access" | "codeswarm:mode:full-access" | "auto" | "autopilot" => {
                ("default".to_owned(), "agy:full-access".to_owned())
            }
            "codeswarm:mode:plan" | "readonly" | "plan" => ("plan".to_owned(), "plan".to_owned()),
            "codeswarm:mode:accept-edits" | "acceptedits" | "accept-edits" => {
                ("accept-edits".to_owned(), "accept-edits".to_owned())
            }
            "codeswarm:mode:manual" | "manual" | "ask" | "default" => {
                ("default".to_owned(), "agy:manual".to_owned())
            }
            "agy:full-access" => ("default".to_owned(), "agy:full-access".to_owned()),
            "agy:manual" => ("default".to_owned(), "agy:manual".to_owned()),
            _ => return Err(AdapterError::Unsupported("requested Agy mode")),
        };
        self.mode = mode;
        self.mode_policy = mode_policy.clone();
        self.emit(Ok(AgentEvent::ModesReplaced {
            slot: self.slot,
            modes: Self::modes(),
            current_mode: Some(mode_policy),
        }))
        .await;
        Ok(())
    }

    async fn reload(&mut self) -> AdapterResult<()> {
        self.stop().await?;
        self.start().await
    }

    async fn stop(&mut self) -> AdapterResult<()> {
        let _ = self.cancel().await?;
        Ok(())
    }

    async fn next_event(&mut self) -> Option<AdapterResult<AgentEvent>> {
        let event = self.receiver.recv().await;
        if matches!(event.as_ref(), Some(Ok(AgentEvent::TurnComplete { .. })))
            && self.session_id.is_none()
            && let Ok(session) = self.announced_session.lock()
        {
            self.session_id = session.clone();
        }
        if matches!(event.as_ref(), Some(Ok(AgentEvent::TurnComplete { .. })))
            && let Some(mut child) = self.child.take()
        {
            let _ = child.wait().await;
        }
        event
    }
}

#[cfg(test)]
#[cfg_attr(not(test), allow(dead_code))]
fn parse_agy_line(slot: RosterSlot, line: &str) -> AdapterResult<Option<AgentEvent>> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| AdapterError::Protocol(error.to_string()))?;
    parse_agy_value(slot, &value)
}

fn parse_agy_value(slot: RosterSlot, value: &Value) -> AdapterResult<Option<AgentEvent>> {
    let event = value.get("event").and_then(Value::as_str);
    if let Some(terminal) = parse_terminal_event(value, event) {
        return Ok(Some(AgentEvent::Terminal {
            slot,
            event: terminal,
        }));
    }
    match event {
        Some("step_update") => {
            let Some(update) = value.get("step_update") else {
                return Ok(None);
            };
            let is_response = update
                .get("step_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "agent_response");
            let text = update.get("text_delta").and_then(Value::as_str);
            let response = is_response
                .then(|| text.map(str::to_owned))
                .flatten()
                .filter(|text| !text.is_empty())
                .map(|text| AgentEvent::Text { slot, text });
            Ok(response.or_else(|| parse_agy_tool(slot, value)))
        }
        _ => Ok(None),
    }
}

fn parse_agy_tool(slot: RosterSlot, value: &Value) -> Option<AgentEvent> {
    let update = value.get("step_update")?;
    if update.get("step_type")?.as_str()? != "tool" {
        return None;
    }
    let step_index = update.get("step_index")?.as_i64()?;
    let title = update
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("Tool call")
        .replace('_', " ");
    let status = match update.get("state").and_then(Value::as_str) {
        Some("DONE") => ToolStatus::Completed,
        Some("FAILED") => ToolStatus::Failed,
        Some("ACTIVE") => ToolStatus::Running,
        _ => ToolStatus::Pending,
    };
    let detail = update
        .get("tool_info")
        .and_then(|info| info.get("output"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(AgentEvent::Tool {
        slot,
        update: ToolUpdate {
            id: format!("agy-tool-{step_index}"),
            title,
            status,
            detail,
        },
    })
}

/// Stdio ACP transport. Protocol-specific response handling belongs here,
/// keeping JSON-RPC framing outside the core and terminal renderer.
#[derive(Debug)]
pub struct AcpAdapter {
    slot: RosterSlot,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    child: Option<Child>,
    reader: Option<BufReader<ChildStdout>>,
    capabilities: AgentCapabilities,
    modes: Vec<Mode>,
    models: Vec<Mode>,
    model_config_id: Option<String>,
    session_id: Option<String>,
    next_request_id: u64,
    prompt_request_id: Option<u64>,
    queued_events: VecDeque<AdapterResult<AgentEvent>>,
    stderr_task: Option<tokio::task::JoinHandle<String>>,
    terminals: BTreeMap<String, TerminalProcess>,
    next_terminal_id: u64,
}

impl AcpAdapter {
    pub fn new(
        slot: RosterSlot,
        cwd: PathBuf,
        program: impl Into<String>,
        args: Vec<String>,
    ) -> Self {
        Self {
            slot,
            program: program.into(),
            args,
            cwd,
            child: None,
            reader: None,
            capabilities: AgentCapabilities::default(),
            modes: Vec::new(),
            models: Vec::new(),
            model_config_id: None,
            session_id: None,
            next_request_id: 1,
            prompt_request_id: None,
            queued_events: VecDeque::new(),
            stderr_task: None,
            terminals: BTreeMap::new(),
            next_terminal_id: 1,
        }
    }

    pub fn with_session_id(
        slot: RosterSlot,
        cwd: PathBuf,
        program: impl Into<String>,
        args: Vec<String>,
        session_id: impl Into<String>,
    ) -> Self {
        let mut adapter = Self::new(slot, cwd, program, args);
        adapter.session_id = Some(session_id.into());
        adapter
    }

    async fn request(&mut self, method: &str, params: Value) -> AdapterResult<Value> {
        self.request_with_timeout(method, params, std::time::Duration::from_secs(30))
            .await
    }

    async fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        deadline: std::time::Duration,
    ) -> AdapterResult<Value> {
        tokio::time::timeout(deadline, self.request_inner(method, params))
            .await
            .map_err(|_| {
                AdapterError::Transport(format!(
                    "ACP {method} timed out; reload the agent to retry"
                ))
            })?
    }

    async fn request_inner(&mut self, method: &str, params: Value) -> AdapterResult<Value> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.write_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }))
        .await?;
        loop {
            let line = self.read_line().await?;
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => {
                    // Keep the transport alive when a peer writes a stray
                    // diagnostic line. This is common with CLI wrappers and
                    // matches the baseline client's tolerant stream loop.
                    continue;
                }
            };
            if self.reject_empty_permission_request(&value).await? {
                continue;
            }
            if self.handle_client_request(&value).await? {
                continue;
            }
            if value
                .get("id")
                .is_some_and(|id| rpc_id_to_string(id) == request_id.to_string())
            {
                if let Some(error) = value.get("error") {
                    return Err(AdapterError::Protocol(error.to_string()));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| AdapterError::Protocol("response has no result".into()));
            }
            if let Some(event) = parse_acp_notification(self.slot, &line)? {
                self.queued_events.push_back(Ok(event));
            }
        }
    }

    async fn write_json(&mut self, value: Value) -> AdapterResult<()> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| AdapterError::Transport("ACP agent is not running".into()))?;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| AdapterError::Transport("ACP agent has no stdin".into()))?;
        stdin
            .write_all(value.to_string().as_bytes())
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|error| AdapterError::Transport(error.to_string()))
    }

    /// ACP permission requests are JSON-RPC requests, not fire-and-forget
    /// notifications. An empty option list is invalid and must be answered
    /// with an error so the peer does not wait forever for a decision. This is
    /// the same validation performed by the Python ACP server.
    async fn reject_empty_permission_request(&mut self, value: &Value) -> AdapterResult<bool> {
        if value.get("method").and_then(Value::as_str) != Some("session/request_permission")
            || value.get("id").is_none()
        {
            return Ok(false);
        }
        let valid = value
            .get("params")
            .and_then(|params| params.get("options"))
            .and_then(Value::as_array)
            .is_some_and(|options| !options.is_empty());
        if valid {
            return Ok(false);
        }
        self.write_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": value.get("id").cloned().unwrap_or(Value::Null),
            "error": {
                "code": -32602,
                "message": "Permission request requires at least one option",
            },
        }))
        .await?;
        Ok(true)
    }

    fn workspace_path(&self, path: &str) -> Result<PathBuf, String> {
        let root = self
            .cwd
            .canonicalize()
            .map_err(|error| format!("unable to resolve workspace: {error}"))?;
        let requested = Path::new(path);
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            root.join(requested)
        };
        let resolved = if !candidate.exists() {
            let parent = candidate
                .parent()
                .ok_or_else(|| "file path has no parent".to_owned())?
                .canonicalize()
                .map_err(|error| format!("unable to resolve parent directory: {error}"))?;
            parent.join(
                candidate
                    .file_name()
                    .ok_or_else(|| "file path has no filename".to_owned())?,
            )
        } else {
            candidate
                .canonicalize()
                .map_err(|error| format!("unable to resolve file path: {error}"))?
        };
        if !resolved.starts_with(&root) {
            return Err("file path is outside the project".into());
        }
        Ok(resolved)
    }

    fn read_workspace_text(
        &self,
        path: &str,
        line: Option<i64>,
        limit: Option<i64>,
    ) -> Result<String, String> {
        if line.is_some_and(|line| line < 1) {
            return Err("line must be positive".into());
        }
        if limit.is_some_and(|limit| limit < 0) {
            return Err("limit must not be negative".into());
        }
        let path = self.workspace_path(path)?;
        let mut bytes = Vec::new();
        let mut source = match std::fs::File::open(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
            Err(error) => return Err(error.to_string()),
        };
        source
            .by_ref()
            .take((MAX_FILE_READ_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        bytes.truncate(MAX_FILE_READ_BYTES);
        let text = String::from_utf8_lossy(&bytes);
        if line.is_none() && limit.is_none() {
            return Ok(text.into_owned());
        }
        let start = line.map_or(0, |line| line as usize - 1);
        let limit = limit.unwrap_or(i64::MAX) as usize;
        let selected = text
            .split_inclusive('\n')
            .skip(start)
            .take(limit)
            .collect::<String>();
        if line.is_some() {
            Ok(selected.trim_end_matches('\n').to_owned())
        } else {
            Ok(selected)
        }
    }

    fn write_workspace_text(&self, params: &Value) -> Result<(), String> {
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or("path must be a non-empty string")?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or("content must be a string")?;
        let path = self.workspace_path(path)?;
        std::fs::write(path, content).map_err(|error| error.to_string())
    }

    async fn terminal_create(&mut self, params: &Value) -> Result<Value, String> {
        let command = params
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| "terminal command is required".to_owned())?;
        let cwd = params.get("cwd").and_then(Value::as_str).unwrap_or(".");
        let cwd = self.workspace_path(cwd)?;
        if !cwd.is_dir() {
            return Err("terminal cwd is not a directory".into());
        }
        let mut process = Command::new(command);
        isolate_process_group(&mut process);
        if let Some(args) = params.get("args").and_then(Value::as_array) {
            process.args(args.iter().filter_map(Value::as_str));
        }
        process
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = params.get("env") {
            if let Some(entries) = env.as_array() {
                for entry in entries {
                    if let (Some(name), Some(value)) = (
                        entry.get("name").and_then(Value::as_str),
                        entry.get("value").and_then(Value::as_str),
                    ) {
                        process.env(name, value);
                    }
                }
            } else if let Some(entries) = env.as_object() {
                for (name, value) in entries {
                    if let Some(value) = value.as_str() {
                        process.env(name, value);
                    }
                }
            }
        }
        let mut child = process.spawn().map_err(|error| error.to_string())?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            let _ = terminate_child(&mut child).await;
            return Err("terminal has no output pipes".into());
        };
        let output = Arc::new(Mutex::new(Vec::new()));
        let truncated = Arc::new(AtomicBool::new(false));
        let output_readers = Arc::new(AtomicUsize::new(2));
        let output_limit = params
            .get("outputByteLimit")
            .and_then(Value::as_u64)
            .map_or(MAX_TERMINAL_OUTPUT_BYTES, |limit| {
                usize::try_from(limit)
                    .unwrap_or(MAX_TERMINAL_OUTPUT_BYTES)
                    .min(MAX_TERMINAL_OUTPUT_BYTES)
            });
        tokio::spawn(drain_terminal_output(
            stdout,
            Arc::clone(&output),
            Arc::clone(&truncated),
            Arc::clone(&output_readers),
            output_limit,
        ));
        tokio::spawn(drain_terminal_output(
            stderr,
            Arc::clone(&output),
            Arc::clone(&truncated),
            Arc::clone(&output_readers),
            output_limit,
        ));
        let id = format!("terminal-{}", self.next_terminal_id);
        self.next_terminal_id = self.next_terminal_id.saturating_add(1);
        let state = TerminalProcess {
            child: Arc::new(AsyncMutex::new(Some(child))),
            output,
            truncated,
            output_readers,
        };
        self.terminals.insert(id.clone(), state);
        self.queued_events.push_back(Ok(AgentEvent::Terminal {
            slot: self.slot,
            event: TerminalEvent::Created {
                id: id.clone(),
                command: std::iter::once(command)
                    .chain(
                        params
                            .get("args")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str),
                    )
                    .collect::<Vec<_>>()
                    .join(" "),
            },
        }));
        Ok(serde_json::json!({"terminalId": id}))
    }

    async fn terminal_output(&mut self, id: &str) -> Result<Value, String> {
        let terminal = self
            .terminals
            .get(id)
            .ok_or_else(|| "terminal not found".to_owned())?
            .clone();
        let output = terminal
            .output
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let exit_code = terminal.exit_code().await;
        self.queued_events.push_back(Ok(AgentEvent::Terminal {
            slot: self.slot,
            event: TerminalEvent::Output {
                id: id.to_owned(),
                text: output.clone(),
            },
        }));
        let mut response = serde_json::json!({
            "output": output,
            "truncated": terminal.truncated.load(Ordering::Acquire),
        });
        if let Some(code) = exit_code {
            response["exitStatus"] = serde_json::json!({"exitCode": code});
        }
        Ok(response)
    }

    async fn terminal_wait(&mut self, id: &str) -> Result<Value, String> {
        let terminal = self
            .terminals
            .get(id)
            .ok_or_else(|| "terminal not found".to_owned())?
            .clone();
        let exit_code = terminal.wait().await;
        self.queued_events.push_back(Ok(AgentEvent::Terminal {
            slot: self.slot,
            event: TerminalEvent::Exited {
                id: id.to_owned(),
                code: exit_code.unwrap_or(-1),
            },
        }));
        Ok(serde_json::json!({"exitCode": exit_code, "signal": Value::Null}))
    }

    /// Handle requests initiated by an ACP agent against the client. File
    /// access is mediated through the configured workspace root; unsupported
    /// requests receive a JSON-RPC error instead of hanging the agent.
    async fn handle_client_request(&mut self, value: &Value) -> AdapterResult<bool> {
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            return Ok(false);
        };
        let Some(id) = value.get("id").cloned() else {
            return Ok(false);
        };
        // Permission requests are consumed by the normalized event parser and
        // answered later through the focused UI action, not here.
        if method == "session/request_permission" {
            return Ok(false);
        }
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "fs/read_text_file" => {
                let path = params.get("path").and_then(Value::as_str).unwrap_or("");
                let line = params.get("line").and_then(Value::as_i64);
                let limit = params.get("limit").and_then(Value::as_i64);
                match self.read_workspace_text(path, line, limit) {
                    Ok(content) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": content},
                    }),
                    Err(message) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32602, "message": message},
                    }),
                }
            }
            "fs/write_text_file" => {
                let result = self.write_workspace_text(&params);
                match result {
                    Ok(()) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    Err(message) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32602, "message": message},
                    }),
                }
            }
            "terminal/create" => match self.terminal_create(&params).await {
                Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(message) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": message},
                }),
            },
            "terminal/output" => {
                let terminal_id = params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match self.terminal_output(terminal_id).await {
                    Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(message) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32602, "message": message},
                    }),
                }
            }
            "terminal/wait_for_exit" => {
                let terminal_id = params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match self.terminal_wait(terminal_id).await {
                    Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(message) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32602, "message": message},
                    }),
                }
            }
            "terminal/kill" => {
                let terminal_id = params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(terminal) = self.terminals.get(terminal_id) {
                    terminal.kill().await;
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32602, "message": "terminal not found"},
                    })
                }
            }
            "terminal/release" => {
                let terminal_id = params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(terminal) = self.terminals.remove(terminal_id) {
                    terminal.stop().await;
                    self.queued_events.push_back(Ok(AgentEvent::Terminal {
                        slot: self.slot,
                        event: TerminalEvent::Released {
                            id: terminal_id.to_owned(),
                        },
                    }));
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32602, "message": "terminal not found"},
                    })
                }
            }
            _ => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unsupported client method: {method}")},
            }),
        };
        self.write_json(response).await?;
        Ok(true)
    }

    async fn read_line(&mut self) -> AdapterResult<String> {
        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| AdapterError::Transport("ACP agent has no stdout".into()))?;
        read_bounded_line(reader).await
    }

    async fn start(&mut self) -> AdapterResult<()> {
        // Starting an adapter twice must never orphan the first transport.
        if self.child.is_some() {
            self.stop().await?;
        }
        let mut command = Command::new(&self.program);
        isolate_process_group(&mut command);
        command
            .args(&self.args)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("CODESWARM_CWD", &self.cwd);
        if self.program.to_ascii_lowercase().contains("gemini")
            || self
                .args
                .iter()
                .any(|arg| arg.to_ascii_lowercase().contains("gemini"))
        {
            command.env("GEMINI_TELEMETRY_ENABLED", "false");
        }
        let mut child = command
            .spawn()
            .map_err(|error| AdapterError::Spawn(error.to_string()))?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = terminate_child(&mut child).await;
                return Err(AdapterError::Transport("ACP agent has no stdout".into()));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                let _ = terminate_child(&mut child).await;
                return Err(AdapterError::Transport("ACP agent has no stderr".into()));
            }
        };
        self.child = Some(child);
        self.reader = Some(BufReader::new(stdout));
        self.stderr_task = Some(tokio::spawn(drain_bounded(stderr, 32 * 1024)));

        let initialize = match self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": true, "writeTextFile": true},
                        "terminal": true,
                    },
                    "clientInfo": {
                        "name": "CodeSwarm",
                        "title": "CodeSwarm",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let _ = self.stop().await;
                return Err(error);
            }
        };
        let agent_capabilities = initialize
            .get("agentCapabilities")
            .cloned()
            .unwrap_or(Value::Null);
        self.capabilities = AgentCapabilities {
            supports_cancel: true,
            supports_modes: true,
            supports_permissions: true,
            supports_terminals: true,
            supports_session_load: agent_capabilities
                .get("loadSession")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            supports_models: false,
        };
        let session = if let Some(session_id) = self.session_id.clone() {
            if !self.capabilities.supports_session_load {
                let _ = self.stop().await;
                return Err(AdapterError::Unsupported("session/load"));
            }
            match self
                .request(
                    "session/load",
                    serde_json::json!({
                        "cwd": self.cwd,
                        "mcpServers": [],
                        "sessionId": session_id,
                    }),
                )
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    let _ = self.stop().await;
                    return Err(error);
                }
            }
        } else {
            let session = match self
                .request(
                    "session/new",
                    serde_json::json!({"cwd": self.cwd, "mcpServers": []}),
                )
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    let _ = self.stop().await;
                    return Err(error);
                }
            };
            self.session_id = session
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if self.session_id.is_none() {
                let _ = self.stop().await;
                return Err(AdapterError::Protocol(
                    "session/new returned no sessionId".into(),
                ));
            }
            session
        };
        self.capabilities.supports_modes = false;
        if let Some(modes) = session.get("modes") {
            let available = modes
                .get("availableModes")
                .and_then(Value::as_array)
                .map(|modes| {
                    modes
                        .iter()
                        .filter_map(|mode| {
                            Some(Mode {
                                id: mode.get("id")?.as_str()?.to_owned(),
                                label: mode.get("name")?.as_str()?.to_owned(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.modes = available.clone();
            self.capabilities.supports_modes = !available.is_empty();
            self.queued_events.push_back(Ok(AgentEvent::ModesReplaced {
                slot: self.slot,
                modes: available,
                current_mode: modes
                    .get("currentModeId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }));
        }
        self.models.clear();
        self.model_config_id = None;
        let current_model =
            parse_model_config(&session).and_then(|(config_id, models, current)| {
                self.model_config_id = Some(config_id);
                self.models = models;
                current
            });
        self.capabilities.supports_models =
            self.model_config_id.is_some() && !self.models.is_empty();
        if let Some(config_id) = self.model_config_id.clone()
            && !self.models.is_empty()
        {
            self.queued_events.push_back(Ok(AgentEvent::ModelsReplaced {
                slot: self.slot,
                config_id,
                models: self.models.clone(),
                current_model,
            }));
        }
        self.queued_events.push_back(Ok(AgentEvent::Ready {
            slot: self.slot,
            capabilities: self.capabilities(),
        }));
        Ok(())
    }
}

fn prompt_resource_paths(prompt: &str) -> Vec<String> {
    let characters = prompt.chars().collect::<Vec<_>>();
    let mut paths = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] != '@' {
            index += 1;
            continue;
        }
        index += 1;
        let quoted = characters.get(index) == Some(&'"');
        if quoted {
            index += 1;
        }
        let start = index;
        while index < characters.len()
            && if quoted {
                characters[index] != '"'
            } else {
                !characters[index].is_whitespace()
            }
        {
            index += 1;
        }
        if index > start {
            paths.push(characters[start..index].iter().collect());
        }
        if quoted && index < characters.len() {
            index += 1;
        }
    }
    paths
}

fn prompt_content_blocks(cwd: &Path, prompt: &str) -> Vec<Value> {
    let mut blocks = vec![serde_json::json!({"type": "text", "text": prompt})];
    for path in prompt_resource_paths(prompt) {
        if path.ends_with('/') {
            continue;
        }
        let Ok(resource) = resources::load(cwd, &path) else {
            continue;
        };
        let uri = format!("file://{}", resource.path.display());
        let resource_value = if let Some(text) = resource.text {
            serde_json::json!({
                "uri": uri,
                "text": text,
                "mimeType": resource.mime_type,
            })
        } else if let Some(data) = resource.data {
            serde_json::json!({
                "uri": uri,
                "blob": BASE64.encode(data),
                "mimeType": resource.mime_type,
            })
        } else {
            continue;
        };
        blocks.push(serde_json::json!({
            "type": "resource",
            "resource": resource_value,
        }));
    }
    blocks
}

#[async_trait]
impl AgentAdapter for AcpAdapter {
    fn slot(&self) -> RosterSlot {
        self.slot
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.clone()
    }

    fn protocol(&self) -> &'static str {
        "acp"
    }

    fn capabilities(&self) -> AgentCapabilities {
        self.capabilities.clone()
    }

    async fn start(&mut self) -> AdapterResult<()> {
        // Use the inherent implementation, which owns the cleanup boundary
        // around the multi-step ACP handshake.
        AcpAdapter::start(self).await
    }

    async fn send_prompt(&mut self, prompt: String) -> AdapterResult<()> {
        let session_id = self
            .session_id
            .as_ref()
            .ok_or_else(|| AdapterError::Transport("ACP session is not initialized".into()))?;
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let prompt_blocks = prompt_content_blocks(&self.cwd, &prompt);
        self.write_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": prompt_blocks,
            },
        }))
        .await?;
        self.prompt_request_id = Some(request_id);
        Ok(())
    }

    async fn cancel(&mut self) -> AdapterResult<bool> {
        let Some(session_id) = &self.session_id else {
            return Ok(false);
        };
        self.write_json(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session_id, "_meta": {}},
        }))
        .await?;
        let settled = tokio::time::timeout(CANCEL_SETTLE_TIMEOUT, async {
            loop {
                match <Self as AgentAdapter>::next_event(self).await {
                    Some(Ok(AgentEvent::TurnComplete { .. })) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        })
        .await
        .is_ok();
        if !settled {
            // A peer that never acknowledges cancellation cannot safely share
            // its stream with the next prompt. Restart the transport while
            // preserving a loadable provider session when supported.
            self.reload().await?;
        }
        Ok(true)
    }

    async fn answer_permission(
        &mut self,
        request_id: String,
        answer: PermissionAnswer,
    ) -> AdapterResult<()> {
        let id = request_id
            .parse::<u64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(request_id));
        let outcome = match answer {
            PermissionAnswer::Selected { option_id } => {
                serde_json::json!({"outcome": "selected", "optionId": option_id})
            }
            PermissionAnswer::Cancelled => serde_json::json!({"outcome": "cancelled"}),
        };
        self.write_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            // RequestPermissionResponse wraps the selected/cancelled
            // discriminator in its `outcome` field. Keep this nested shape
            // compatible with the Python ACP server and ACP schema.
            "result": {"outcome": outcome},
        }))
        .await
    }

    async fn set_mode(&mut self, mode: String) -> AdapterResult<()> {
        let session_id = self
            .session_id
            .as_ref()
            .ok_or_else(|| AdapterError::Transport("ACP session is not initialized".into()))?;
        let policy = match mode.as_str() {
            "plan" => "codeswarm:mode:plan",
            "default" | "manual" => "codeswarm:mode:manual",
            "accept-edits" => "codeswarm:mode:accept-edits",
            "full-access" | "auto" | "autopilot" => "codeswarm:mode:full-access",
            other => other,
        };
        let native_mode = crate::policy::resolve(policy, &self.modes)
            .map(|mode| mode.id)
            .unwrap_or(mode);
        let _ = self
            .request(
                "session/set_mode",
                serde_json::json!({"sessionId": session_id, "modeId": native_mode.clone()}),
            )
            .await?;
        self.queued_events.push_back(Ok(AgentEvent::ModeUpdated {
            slot: self.slot,
            current_mode: native_mode,
        }));
        Ok(())
    }

    async fn set_model(&mut self, model: String) -> AdapterResult<()> {
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| AdapterError::Transport("ACP session is not initialized".into()))?;
        let config_id = self
            .model_config_id
            .clone()
            .ok_or(AdapterError::Unsupported("set_model"))?;
        if !self.models.iter().any(|candidate| candidate.id == model) {
            return Err(AdapterError::Protocol(
                "model is not advertised by the agent".into(),
            ));
        }
        let _ = self
            .request(
                "session/set_config_option",
                serde_json::json!({
                    "sessionId": session_id,
                    "configId": config_id,
                    "value": model,
                }),
            )
            .await?;
        Ok(())
    }

    async fn reload(&mut self) -> AdapterResult<()> {
        // `stop` tears down the process and clears its transport-owned
        // session handle. A reload is different from a final shutdown: ACP
        // peers advertising `loadSession` must receive the prior ID so the
        // replacement process can resume the same conversation.
        let session_id = self
            .capabilities
            .supports_session_load
            .then(|| self.session_id.clone())
            .flatten();
        self.stop().await?;
        self.session_id = session_id.clone();
        let result = self.start().await;
        if result.is_err() {
            // `start` cleans up a partially initialized transport by calling
            // `stop`, which also clears the handle. Keep it available for a
            // subsequent retry after the coordinator reports the failure.
            self.session_id = session_id;
        }
        result
    }

    async fn stop(&mut self) -> AdapterResult<()> {
        let terminals = std::mem::take(&mut self.terminals);
        for terminal in terminals.values() {
            terminal.stop().await;
        }
        if let Some(mut child) = self.child.take() {
            terminate_child(&mut child).await?;
        }
        self.reader = None;
        self.session_id = None;
        self.prompt_request_id = None;
        if let Some(task) = self.stderr_task.take() {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Option<AdapterResult<AgentEvent>> {
        if let Some(event) = self.queued_events.pop_front() {
            return Some(event);
        }
        loop {
            let line = match self.read_line().await {
                Ok(line) => line,
                Err(error) => return Some(Err(error)),
            };
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            match self.reject_empty_permission_request(&value).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => return Some(Err(error)),
            }
            match self.handle_client_request(&value).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => return Some(Err(error)),
            }
            match parse_acp_notification(self.slot, &line) {
                Ok(Some(event)) => {
                    if let AgentEvent::ModelsReplaced {
                        config_id, models, ..
                    } = &event
                    {
                        self.model_config_id = Some(config_id.clone());
                        self.models = models.clone();
                        self.capabilities.supports_models = !models.is_empty();
                    }
                    return Some(Ok(event));
                }
                Ok(None) => {}
                Err(error) => return Some(Err(error)),
            }
            if value.get("id").is_some_and(|id| {
                self.prompt_request_id
                    .is_some_and(|expected| rpc_id_to_string(id) == expected.to_string())
            }) {
                if let Some(error) = value.get("error") {
                    self.prompt_request_id = None;
                    return Some(Err(AdapterError::Protocol(error.to_string())));
                }
                self.prompt_request_id = None;
                return Some(Ok(AgentEvent::TurnComplete { slot: self.slot }));
            }
        }
    }
}

fn parse_acp_notification(slot: RosterSlot, line: &str) -> AdapterResult<Option<AgentEvent>> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| AdapterError::Protocol(error.to_string()))?;
    let method = value.get("method").and_then(Value::as_str);
    if method == Some("session/request_permission") {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let request_id = value
            .get("id")
            .map(rpc_id_to_string)
            .unwrap_or_else(|| "permission".into());
        return Ok(parse_permission_event(
            slot,
            &params,
            &request_id,
            params.get("options"),
        ));
    }
    if method != Some("session/update") {
        return Ok(None);
    }
    let Some(update) = value.get("params").and_then(|params| params.get("update")) else {
        return Ok(None);
    };
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    if kind == Some("config_option_update")
        && let Some((config_id, models, current_model)) = parse_model_config(update)
    {
        return Ok(Some(AgentEvent::ModelsReplaced {
            slot,
            config_id,
            models,
            current_model,
        }));
    }
    if kind == Some("request_permission") {
        let request_id = update
            .get("toolCall")
            .and_then(|tool| tool.get("toolCallId"))
            .and_then(Value::as_str)
            .unwrap_or("permission");
        return Ok(parse_permission_event(
            slot,
            update,
            request_id,
            update.get("options"),
        ));
    }
    if kind == Some("available_commands_update") {
        let commands = update
            .get("availableCommands")
            .and_then(Value::as_array)
            .map(|commands| {
                commands
                    .iter()
                    .filter_map(|command| {
                        let name = command.get("name").and_then(Value::as_str)?.trim();
                        (!name.is_empty()).then(|| AgentCommand {
                            name: name.to_owned(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return Ok(Some(AgentEvent::CommandsReplaced { slot, commands }));
    }
    if kind == Some("current_mode_update") {
        if let Some(mode) = update
            .get("currentModeId")
            .and_then(Value::as_str)
            .filter(|mode| !mode.trim().is_empty())
        {
            return Ok(Some(AgentEvent::ModeUpdated {
                slot,
                current_mode: mode.to_owned(),
            }));
        }
        return Ok(None);
    }
    if kind == Some("usage_update") {
        let Some(used) = update.get("used").and_then(Value::as_u64) else {
            return Ok(None);
        };
        let Some(size) = update.get("size").and_then(Value::as_u64) else {
            return Ok(None);
        };
        return Ok(Some(AgentEvent::UsageUpdated {
            slot,
            usage: UsageUpdate { used, size },
        }));
    }
    if let Some(terminal) = parse_terminal_event(update, kind) {
        return Ok(Some(AgentEvent::Terminal {
            slot,
            event: terminal,
        }));
    }
    let text = update
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if kind == Some("user_message_chunk") {
        return Ok(text
            .filter(|text| !text.is_empty())
            .map(|text| AgentEvent::UserText { slot, text }));
    }
    if kind == Some("agent_message_chunk")
        && let Some(mode) = text
            .as_deref()
            .and_then(|text| text.strip_prefix("[MODE_UPDATE]"))
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
    {
        // Gemini's native ACP bridge historically encoded a mode change as a
        // control marker in the message stream. It is state, not transcript
        // content; expose it as a normalized catalog replacement instead of
        // leaking the marker into the conversation.
        return Ok(Some(AgentEvent::ModesReplaced {
            slot,
            modes: vec![Mode {
                id: mode.to_owned(),
                label: mode.to_owned(),
            }],
            current_mode: Some(mode.to_owned()),
        }));
    }
    match (kind, text) {
        (Some("agent_message_chunk"), Some(text)) if !text.is_empty() => {
            Ok(Some(AgentEvent::Text { slot, text }))
        }
        (Some("agent_thought_chunk"), Some(text)) if !text.is_empty() => {
            Ok(Some(AgentEvent::Thought { slot, text }))
        }
        (Some("tool_call"), _) | (Some("tool_call_update"), _) => {
            let id = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("unknown-tool")
                .to_owned();
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Tool call")
                .to_owned();
            let status = match update.get("status").and_then(Value::as_str) {
                Some("completed") => ToolStatus::Completed,
                Some("failed") => ToolStatus::Failed,
                Some("in_progress") => ToolStatus::Running,
                _ => ToolStatus::Pending,
            };
            Ok(Some(AgentEvent::Tool {
                slot,
                update: ToolUpdate {
                    id,
                    title,
                    status,
                    detail: None,
                },
            }))
        }
        _ => Ok(None),
    }
}

fn parse_model_config(value: &Value) -> Option<(String, Vec<Mode>, Option<String>)> {
    let config = value
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|option| {
            option.get("category").and_then(Value::as_str) == Some("model")
                && matches!(
                    option.get("type").and_then(Value::as_str),
                    Some("select" | "enum")
                )
        })?;
    let config_id = config.get("id")?.as_str()?.to_owned();
    let models = config
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|option| {
            let id = option.get("value")?.as_str()?.to_owned();
            let label = option
                .get("name")
                .or_else(|| option.get("label"))
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_owned();
            Some(Mode { id, label })
        })
        .collect::<Vec<_>>();
    (!models.is_empty()).then(|| {
        let current = config
            .get("currentValue")
            .and_then(Value::as_str)
            .map(str::to_owned);
        (config_id, models, current)
    })
}

/// Normalize terminal lifecycle updates emitted by ACP-compatible bridges and
/// native stream adapters. Protocols have used both snake_case update names
/// and a nested `terminal` object, so accept either without leaking that
/// shape beyond the adapter boundary.
fn parse_terminal_event(value: &Value, kind: Option<&str>) -> Option<TerminalEvent> {
    let nested = value.get("terminal").unwrap_or(value);
    let kind = kind.or_else(|| value.get("event").and_then(Value::as_str))?;
    let id = nested
        .get("terminalId")
        .or_else(|| nested.get("terminal_id"))
        .or_else(|| nested.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("terminal")
        .to_owned();
    match kind {
        "terminal_created" | "terminal_create" | "terminal_started" => {
            let command = nested
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Some(TerminalEvent::Created { id, command })
        }
        "terminal_output" | "terminal_output_chunk" => {
            let text = nested
                .get("output")
                .or_else(|| nested.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Some(TerminalEvent::Output { id, text })
        }
        "terminal_exited" | "terminal_exit" => {
            let code = nested
                .get("exitCode")
                .or_else(|| nested.get("exit_code"))
                .or_else(|| nested.get("code"))
                .and_then(Value::as_i64)
                .unwrap_or(0) as i32;
            Some(TerminalEvent::Exited { id, code })
        }
        "terminal_released" | "terminal_release" => Some(TerminalEvent::Released { id }),
        _ => None,
    }
}

fn parse_permission_event(
    slot: RosterSlot,
    value: &Value,
    request_id: &str,
    options: Option<&Value>,
) -> Option<AgentEvent> {
    let tool = value.get("toolCall").unwrap_or(value);
    let title = tool
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Agent requests permission")
        .to_owned();
    let (options, option_ids): (Vec<String>, Vec<String>) = options
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    let label = option
                        .get("name")
                        .or_else(|| option.get("optionId"))
                        .and_then(Value::as_str)?
                        .to_owned();
                    let option_id = option
                        .get("optionId")
                        .or_else(|| option.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| label.clone());
                    Some((label, option_id))
                })
                .unzip()
        })
        .unwrap_or_default();
    if options.is_empty() {
        return None;
    }
    Some(AgentEvent::Permission {
        slot,
        request: PermissionRequest {
            id: request_id.to_owned(),
            title,
            options,
            option_ids,
        },
    })
}

fn rpc_id_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AcpAdapter, AdapterHost, AgentAdapter, AgyAdapter, MAX_ACP_LINE_BYTES, MAX_FILE_READ_BYTES,
        RelayHost, ScriptedAdapter, parse_acp_notification, parse_agy_line, parse_command_line,
        parse_model_config, prompt_content_blocks, read_bounded_line,
    };
    #[cfg(target_os = "linux")]
    use super::{isolate_process_group, terminate_child};
    use crate::TerminalEvent;
    use crate::{
        AgentCapabilities, AgentEvent, EventLog, Mode, PermissionAnswer, ToolStatus,
        persistence::SessionMetadataStore,
        relay::{DEFAULT_STOP_ACKNOWLEDGMENT, RelayDecision, STOP_TOKEN},
    };
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn unique_test_path(stem: &str, extension: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{stem}-{}-{nonce}.{extension}", std::process::id()))
    }

    #[test]
    fn malformed_file_writes_preserve_existing_content() {
        let root = unique_test_path("codeswarm-write-validation", "dir");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("keep.txt");
        std::fs::write(&file, "valuable content").unwrap();
        let adapter = AcpAdapter::new(0, root.clone(), "unused", Vec::new());
        for content in [
            Value::Null,
            serde_json::json!(false),
            serde_json::json!(42),
            serde_json::json!([]),
        ] {
            assert!(
                adapter
                    .write_workspace_text(
                        &serde_json::json!({"path":"keep.txt", "content": content})
                    )
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "valuable content");
        }
        assert!(
            adapter
                .write_workspace_text(&serde_json::json!({"path":"keep.txt"}))
                .is_err()
        );
        assert!(
            adapter
                .write_workspace_text(&serde_json::json!({"path": null, "content":"replacement"}))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "valuable content");
        adapter
            .write_workspace_text(&serde_json::json!({"path":"keep.txt", "content":"replacement"}))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "replacement");
        adapter
            .write_workspace_text(&serde_json::json!({"path":"keep.txt", "content":""}))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn silent_acp_control_request_times_out_and_transport_can_be_stopped() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}'; read _; echo '{"jsonrpc":"2.0","id":"2","result":{"sessionId":"s"}}'; read _; read _"#;
        let mut adapter = AcpAdapter::new(
            0,
            std::env::current_dir().unwrap(),
            "sh",
            vec!["-c".into(), script.into()],
        );
        adapter.start().await.unwrap();
        let error = adapter
            .request_with_timeout(
                "session/set_mode",
                serde_json::json!({}),
                std::time::Duration::from_millis(10),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("session/set_mode timed out"));
        adapter.stop().await.unwrap();
        assert!(adapter.child.is_none());
    }

    #[tokio::test]
    async fn goals_reach_every_roster_slot_without_native_goal_support() {
        use crate::goal::GoalCommand;
        let hosts = (0..3)
            .map(|slot| {
                AdapterHost::new(
                    Box::new(ScriptedAdapter::new(
                        slot,
                        AgentCapabilities::default(),
                        [
                            AgentEvent::TurnComplete { slot },
                            AgentEvent::TurnComplete { slot },
                        ],
                    )),
                    None,
                )
            })
            .collect();
        let mut relay = RelayHost::new(hosts, 10).unwrap();
        relay.start().await.unwrap();
        let task = relay
            .apply_goal(GoalCommand::Set("Ship the settings screen".into()))
            .unwrap()
            .unwrap();
        relay.relay_mut().enqueue_human(task, Some(0));
        for slot in 0..3 {
            relay.run_turn("", 0).await.unwrap();
            let (actual, prompt) = relay.dispatches().last().unwrap();
            assert_eq!(*actual, slot);
            assert!(prompt.contains("Active shared goal: Ship the settings screen"));
        }
        let snapshot = relay.session_metadata();
        let restored = crate::goal::Goal::from_metadata(snapshot.get("goal").unwrap());
        assert!(restored.is_some());
        relay.restore_goal(restored);
        relay.reload(0).await.unwrap();
        relay.run_turn("", 0).await.unwrap();
        assert!(
            relay
                .dispatches()
                .last()
                .unwrap()
                .1
                .contains("Active shared goal: Ship the settings screen")
        );
        relay.apply_goal(GoalCommand::Done).unwrap();
        relay.run_turn("", 0).await.unwrap();
        assert!(
            relay
                .dispatches()
                .last()
                .unwrap()
                .1
                .contains("No active shared goal")
        );
        relay.apply_goal(GoalCommand::Clear).unwrap();
        assert!(relay.session_metadata().get("goal").unwrap().is_null());
    }

    #[tokio::test]
    async fn replacement_agent_receives_task_after_public_journal_pruning() {
        let hosts = (0..2)
            .map(|slot| {
                AdapterHost::new(
                    Box::new(ScriptedAdapter::new(
                        slot,
                        AgentCapabilities::default(),
                        [
                            AgentEvent::Text {
                                slot,
                                text: "progress".into(),
                            },
                            AgentEvent::TurnComplete { slot },
                            AgentEvent::Text {
                                slot,
                                text: "more progress".into(),
                            },
                            AgentEvent::TurnComplete { slot },
                        ],
                    )),
                    None,
                )
            })
            .collect();
        let mut relay = RelayHost::new(hosts, 10).unwrap();
        relay.start().await.unwrap();
        relay
            .relay_mut()
            .enqueue_human("Fix the login bug", Some(0));
        relay.run_turn("", 0).await.unwrap();
        relay.run_turn("", 0).await.unwrap();
        relay.run_turn("", 0).await.unwrap();
        relay.reload(1).await.unwrap();
        assert!(
            !relay
                .relay_mut()
                .unseen_context(1)
                .contains("Fix the login bug")
        );
        relay.run_turn("", 0).await.unwrap();
        assert!(
            relay
                .dispatches()
                .last()
                .unwrap()
                .1
                .contains("Shared task:\nFix the login bug")
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn termination_kills_only_the_verified_isolated_child_group() {
        use nix::unistd::{Pid, getpgid, getpgrp};
        use tokio::io::{AsyncBufReadExt, BufReader};

        let own_group = getpgrp();
        let mut command = tokio::process::Command::new("sh");
        isolate_process_group(&mut command);
        command
            .arg("-c")
            .arg("sleep 60 & echo $!; wait")
            .stdout(std::process::Stdio::piped());
        let mut child = command.spawn().expect("spawn isolated shell");
        let leader = Pid::from_raw(child.id().expect("leader pid") as i32);
        assert_eq!(getpgid(Some(leader)).expect("leader group"), leader);
        assert_ne!(leader, own_group);

        let stdout = child.stdout.take().expect("child stdout");
        let mut lines = BufReader::new(stdout).lines();
        let descendant = lines
            .next_line()
            .await
            .expect("read descendant pid")
            .expect("descendant pid")
            .parse::<i32>()
            .expect("numeric descendant pid");
        let descendant = Pid::from_raw(descendant);
        assert_eq!(getpgid(Some(descendant)).expect("descendant group"), leader);

        terminate_child(&mut child).await.expect("terminate group");
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{descendant}")).exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("descendant {descendant} survived isolated group termination");
    }

    #[test]
    fn parses_configured_commands_with_shell_style_quotes_without_a_shell() {
        assert_eq!(
            parse_command_line(r#"npx -y "@agentclientprotocol/codex-acp" --flag 'two words'"#),
            Ok((
                "npx".into(),
                vec![
                    "-y".into(),
                    "@agentclientprotocol/codex-acp".into(),
                    "--flag".into(),
                    "two words".into(),
                ]
            ),)
        );
        assert_eq!(
            parse_command_line(r#"agent "" escaped\ argument"#),
            Ok(("agent".into(), vec!["".into(), "escaped argument".into()],))
        );
    }

    #[test]
    fn acp_prompt_expands_safe_at_path_resources() {
        let root = unique_test_path("codeswarm-prompt-resource", "dir");
        std::fs::create_dir_all(&root).expect("workspace");
        std::fs::write(root.join("note.md"), "resource text").expect("resource");
        let blocks = prompt_content_blocks(&root, "inspect @note.md");
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "inspect @note.md");
        assert_eq!(blocks[1]["type"], "resource");
        assert_eq!(blocks[1]["resource"]["text"], "resource text");
        assert_eq!(blocks[1]["resource"]["mimeType"], "text/markdown");
        std::fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[tokio::test]
    async fn oversized_acp_frames_are_rejected_before_full_line_allocation() {
        let mut bytes = vec![b'x'; MAX_ACP_LINE_BYTES + 1];
        bytes.push(b'\n');
        let mut reader = tokio::io::BufReader::new(bytes.as_slice());
        assert!(matches!(
            read_bounded_line(&mut reader).await,
            Err(super::AdapterError::Protocol(detail)) if detail.contains("exceeds")
        ));
    }

    #[test]
    fn rejects_malformed_configured_commands_before_spawn() {
        assert_eq!(
            parse_command_line("agent 'unfinished"),
            Err(super::CommandParseError::UnterminatedQuote)
        );
        assert_eq!(
            parse_command_line("agent\\"),
            Err(super::CommandParseError::TrailingEscape)
        );
        assert_eq!(
            parse_command_line("   \t"),
            Err(super::CommandParseError::Empty)
        );
    }

    #[derive(Debug)]
    struct PendingAdapter {
        slot: usize,
        hang_on_cancel: bool,
    }

    #[derive(Debug)]
    struct ConcurrentStartAdapter {
        slot: usize,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl AgentAdapter for ConcurrentStartAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities::default()
        }

        async fn start(&mut self) -> super::AdapterResult<()> {
            self.barrier.wait().await;
            Ok(())
        }

        async fn send_prompt(&mut self, _prompt: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn cancel(&mut self) -> super::AdapterResult<bool> {
            Ok(true)
        }

        async fn answer_permission(
            &mut self,
            _request_id: String,
            _answer: PermissionAnswer,
        ) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn set_mode(&mut self, _mode: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn reload(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn stop(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<super::AdapterResult<AgentEvent>> {
            std::future::pending().await
        }
    }

    #[derive(Debug)]
    struct PermissionBlockingAdapter {
        slot: usize,
        phase: u8,
    }

    #[async_trait]
    impl AgentAdapter for PermissionBlockingAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities {
                supports_permissions: true,
                ..AgentCapabilities::default()
            }
        }

        async fn start(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn send_prompt(&mut self, _prompt: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn cancel(&mut self) -> super::AdapterResult<bool> {
            Ok(true)
        }

        async fn answer_permission(
            &mut self,
            request_id: String,
            answer: PermissionAnswer,
        ) -> super::AdapterResult<()> {
            if self.phase != 1 || request_id != "permission-1" {
                return Err(super::AdapterError::Protocol(
                    "unexpected permission response".into(),
                ));
            }
            assert!(matches!(answer, PermissionAnswer::Selected { .. }));
            self.phase = 2;
            Ok(())
        }

        async fn set_mode(&mut self, _mode: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn reload(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn stop(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<super::AdapterResult<AgentEvent>> {
            match self.phase {
                0 => {
                    self.phase = 1;
                    Some(Ok(AgentEvent::Permission {
                        slot: self.slot,
                        request: crate::PermissionRequest {
                            id: "permission-1".into(),
                            title: "Allow?".into(),
                            options: vec!["Allow".into()],
                            option_ids: vec!["allow".into()],
                        },
                    }))
                }
                1 => std::future::pending().await,
                _ => Some(Ok(AgentEvent::TurnComplete { slot: self.slot })),
            }
        }
    }

    #[async_trait]
    impl AgentAdapter for PendingAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities {
                supports_cancel: true,
                ..AgentCapabilities::default()
            }
        }

        async fn start(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn send_prompt(&mut self, _prompt: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn cancel(&mut self) -> super::AdapterResult<bool> {
            if self.hang_on_cancel {
                return std::future::pending().await;
            }
            Ok(true)
        }

        async fn answer_permission(
            &mut self,
            _request_id: String,
            _answer: PermissionAnswer,
        ) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn set_mode(&mut self, _mode: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn reload(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn stop(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<super::AdapterResult<AgentEvent>> {
            std::future::pending().await
        }
    }

    #[derive(Debug)]
    struct StopTrackingAdapter {
        slot: usize,
        stopped: Arc<AtomicUsize>,
        fail_stop: bool,
    }

    #[derive(Debug)]
    struct ModeOrderAdapter {
        slot: usize,
        log: Arc<Mutex<Vec<String>>>,
        phase: u8,
    }

    #[derive(Debug)]
    struct StartupAcpAdapter {
        slot: usize,
        events: std::collections::VecDeque<AgentEvent>,
    }

    impl StartupAcpAdapter {
        fn new(slot: usize) -> Self {
            Self {
                slot,
                events: [
                    AgentEvent::ModesReplaced {
                        slot,
                        modes: vec![Mode {
                            id: "full-access".into(),
                            label: "Auto pilot".into(),
                        }],
                        current_mode: Some("full-access".into()),
                    },
                    AgentEvent::Ready {
                        slot,
                        capabilities: AgentCapabilities {
                            supports_modes: true,
                            ..AgentCapabilities::default()
                        },
                    },
                ]
                .into(),
            }
        }
    }

    #[async_trait]
    impl AgentAdapter for StartupAcpAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn protocol(&self) -> &'static str {
            "acp"
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities {
                supports_modes: true,
                ..AgentCapabilities::default()
            }
        }

        async fn start(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn send_prompt(&mut self, _prompt: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn cancel(&mut self) -> super::AdapterResult<bool> {
            Ok(true)
        }

        async fn answer_permission(
            &mut self,
            _request_id: String,
            _answer: PermissionAnswer,
        ) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn set_mode(&mut self, _mode: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn reload(&mut self) -> super::AdapterResult<()> {
            self.events = Self::new(self.slot).events;
            Ok(())
        }

        async fn stop(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<super::AdapterResult<AgentEvent>> {
            self.events.pop_front().map(Ok)
        }
    }

    #[async_trait]
    impl AgentAdapter for ModeOrderAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities {
                supports_modes: true,
                ..AgentCapabilities::default()
            }
        }

        async fn start(&mut self) -> super::AdapterResult<()> {
            self.log.lock().expect("log").push("start".into());
            Ok(())
        }

        async fn send_prompt(&mut self, _prompt: String) -> super::AdapterResult<()> {
            self.log.lock().expect("log").push("prompt".into());
            Ok(())
        }

        async fn cancel(&mut self) -> super::AdapterResult<bool> {
            Ok(true)
        }

        async fn answer_permission(
            &mut self,
            _request_id: String,
            _answer: PermissionAnswer,
        ) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn set_mode(&mut self, mode: String) -> super::AdapterResult<()> {
            self.log.lock().expect("log").push(format!("mode:{mode}"));
            Ok(())
        }

        async fn reload(&mut self) -> super::AdapterResult<()> {
            self.log.lock().expect("log").push("reload".into());
            self.phase = 0;
            Ok(())
        }

        async fn stop(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<super::AdapterResult<AgentEvent>> {
            match self.phase {
                0 => {
                    self.phase = 1;
                    Some(Ok(AgentEvent::ModesReplaced {
                        slot: self.slot,
                        modes: vec![Mode {
                            id: "yolo".into(),
                            label: "YOLO".into(),
                        }],
                        current_mode: None,
                    }))
                }
                1 => {
                    self.phase = 2;
                    Some(Ok(AgentEvent::TurnComplete { slot: self.slot }))
                }
                _ => std::future::pending().await,
            }
        }
    }

    #[async_trait]
    impl AgentAdapter for StopTrackingAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities::default()
        }

        async fn start(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn send_prompt(&mut self, _prompt: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn cancel(&mut self) -> super::AdapterResult<bool> {
            Ok(false)
        }

        async fn answer_permission(
            &mut self,
            _request_id: String,
            _answer: PermissionAnswer,
        ) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn set_mode(&mut self, _mode: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn reload(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn stop(&mut self) -> super::AdapterResult<()> {
            self.stopped.fetch_add(1, Ordering::Relaxed);
            if self.fail_stop {
                Err(super::AdapterError::Transport("stop failed".into()))
            } else {
                Ok(())
            }
        }

        async fn next_event(&mut self) -> Option<super::AdapterResult<AgentEvent>> {
            None
        }
    }

    /// Startup can fail after an adapter has allocated resources.  Keep a
    /// fixture that records whether the failing adapter itself receives the
    /// cleanup call, not just the already-started peers.
    #[derive(Debug)]
    struct FailingStartAdapter {
        slot: usize,
        stopped: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentAdapter for FailingStartAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities::default()
        }

        async fn start(&mut self) -> super::AdapterResult<()> {
            Err(super::AdapterError::Spawn("startup failed".into()))
        }

        async fn send_prompt(&mut self, _prompt: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn cancel(&mut self) -> super::AdapterResult<bool> {
            Ok(false)
        }

        async fn answer_permission(
            &mut self,
            _request_id: String,
            _answer: PermissionAnswer,
        ) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn set_mode(&mut self, _mode: String) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn reload(&mut self) -> super::AdapterResult<()> {
            Ok(())
        }

        async fn stop(&mut self) -> super::AdapterResult<()> {
            self.stopped.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn next_event(&mut self) -> Option<super::AdapterResult<AgentEvent>> {
            None
        }
    }

    #[tokio::test]
    async fn relay_stop_attempts_every_adapter_after_one_shutdown_failure() {
        let stopped = Arc::new(AtomicUsize::new(0));
        let relay = RelayHost::new(
            vec![
                AdapterHost::new(
                    Box::new(StopTrackingAdapter {
                        slot: 0,
                        stopped: Arc::clone(&stopped),
                        fail_stop: true,
                    }),
                    None,
                ),
                AdapterHost::new(
                    Box::new(StopTrackingAdapter {
                        slot: 1,
                        stopped: Arc::clone(&stopped),
                        fail_stop: false,
                    }),
                    None,
                ),
            ],
            4,
        )
        .expect("relay");
        let mut relay = relay;

        let error = relay.stop().await.expect_err("first stop failure");
        assert!(error.to_string().contains("stop failed"));
        assert_eq!(stopped.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn relay_start_cleans_up_the_adapter_that_failed_startup() {
        let stopped = Arc::new(AtomicUsize::new(0));
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(
                    Box::new(StopTrackingAdapter {
                        slot: 0,
                        stopped: Arc::clone(&stopped),
                        fail_stop: false,
                    }),
                    None,
                ),
                AdapterHost::new(
                    Box::new(FailingStartAdapter {
                        slot: 1,
                        stopped: Arc::clone(&stopped),
                    }),
                    None,
                ),
            ],
            4,
        )
        .expect("relay");

        assert!(relay.start().await.is_err());
        assert_eq!(stopped.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn parses_acp_text_without_ui_dependency() {
        let event = parse_acp_notification(
            2,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}"#,
        )
        .expect("valid ACP")
        .expect("text event");
        assert_eq!(
            event,
            AgentEvent::Text {
                slot: 2,
                text: "hello".into(),
            }
        );
    }

    #[test]
    fn parses_acp_state_notifications_at_the_adapter_boundary() {
        let commands = parse_acp_notification(
            3,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"Review"},{"name":"","description":"bad"},{"name":7}]}}}"#,
        )
        .expect("valid ACP")
        .expect("commands event");
        assert_eq!(
            commands,
            AgentEvent::CommandsReplaced {
                slot: 3,
                commands: vec![crate::AgentCommand {
                    name: "review".into()
                }]
            }
        );

        let mode = parse_acp_notification(
            3,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"current_mode_update","currentModeId":"review"}}}"#,
        )
        .expect("valid ACP")
        .expect("mode event");
        assert_eq!(
            mode,
            AgentEvent::ModeUpdated {
                slot: 3,
                current_mode: "review".into()
            }
        );

        let usage = parse_acp_notification(
            3,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"usage_update","used":4200,"size":128000}}}"#,
        )
        .expect("valid ACP")
        .expect("usage event");
        assert_eq!(
            usage,
            AgentEvent::UsageUpdated {
                slot: 3,
                usage: crate::UsageUpdate {
                    used: 4200,
                    size: 128000
                }
            }
        );

        let models = parse_acp_notification(
            3,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"config_option_update","configOptions":[{"id":"model","category":"model","type":"select","currentValue":"smart","options":[{"value":"fast","name":"Fast"},{"value":"smart","name":"Smart"}]}]}}}"#,
        )
        .expect("valid ACP")
        .expect("models event");
        assert!(matches!(
            models,
            AgentEvent::ModelsReplaced { slot: 3, models, current_model, .. }
                if models.len() == 2 && current_model.as_deref() == Some("smart")
        ));
        assert_eq!(
            parse_model_config(&serde_json::json!({
                "configOptions": [{"id": "model", "category": "model", "type": "select", "options": [{"name": "missing value"}]}]
            })),
            None
        );

        let user = parse_acp_notification(
            3,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"context"}}}}"#,
        )
        .expect("valid ACP")
        .expect("user event");
        assert_eq!(
            user,
            AgentEvent::UserText {
                slot: 3,
                text: "context".into()
            }
        );
    }

    #[test]
    fn parses_legacy_gemini_mode_marker_as_state_not_agent_text() {
        let event = parse_acp_notification(
            0,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"[MODE_UPDATE] yolo"}}}}"#,
        )
        .expect("valid ACP")
        .expect("mode event");
        assert!(matches!(
            event,
            AgentEvent::ModesReplaced { current_mode: Some(mode), modes, .. }
                if mode == "yolo" && modes[0].id == "yolo"
        ));
    }

    #[test]
    fn parses_native_agy_text_without_acp_bridge() {
        let event = parse_agy_line(
            1,
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"hello"}}"#,
        )
        .expect("valid stream-json")
        .expect("text event");
        assert_eq!(
            event,
            AgentEvent::Text {
                slot: 1,
                text: "hello".into(),
            }
        );
    }

    #[test]
    fn parses_tool_lifecycle_from_each_protocol() {
        let agy = parse_agy_line(
            1,
            r#"{"event":"step_update","step_update":{"step_type":"tool","step_index":4,"tool_name":"run_command","state":"DONE","tool_info":{"output":"ok"}}}"#,
        )
        .expect("valid native tool")
        .expect("tool event");
        assert!(matches!(
            agy,
            AgentEvent::Tool {
                update: crate::ToolUpdate {
                    status: ToolStatus::Completed,
                    ..
                },
                ..
            }
        ));

        let acp = parse_acp_notification(
            1,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"Run tests","status":"failed"}}}"#,
        )
        .expect("valid ACP tool")
        .expect("tool event");
        assert!(matches!(
            acp,
            AgentEvent::Tool {
                update: crate::ToolUpdate {
                    status: ToolStatus::Failed,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn parses_terminal_lifecycle_from_acp_and_native_events() {
        let created = parse_acp_notification(
            0,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"terminal_created","terminalId":"term-1","command":"cargo test"}}}"#,
        )
        .expect("valid ACP terminal")
        .expect("terminal event");
        assert_eq!(
            created,
            AgentEvent::Terminal {
                slot: 0,
                event: TerminalEvent::Created {
                    id: "term-1".into(),
                    command: "cargo test".into(),
                },
            }
        );
        let output = parse_agy_line(
            1,
            r#"{"event":"terminal_output","terminalId":"term-1","output":"ok\n"}"#,
        )
        .expect("valid native terminal")
        .expect("terminal event");
        assert_eq!(
            output,
            AgentEvent::Terminal {
                slot: 1,
                event: TerminalEvent::Output {
                    id: "term-1".into(),
                    text: "ok\n".into(),
                },
            }
        );
        let released = parse_agy_line(1, r#"{"event":"terminal_released","terminalId":"term-1"}"#)
            .expect("valid native release")
            .expect("terminal event");
        assert!(matches!(
            released,
            AgentEvent::Terminal {
                event: TerminalEvent::Released { id },
                ..
            } if id == "term-1"
        ));
    }

    #[test]
    fn parses_acp_permission_requests() {
        let event = parse_acp_notification(
            0,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"request_permission","toolCall":{"toolCallId":"t1","title":"Write file"},"options":[{"name":"Allow once","optionId":"allow-once"},{"name":"Reject","optionId":"reject"}]}}}"#,
        )
        .expect("valid permission")
        .expect("permission event");
        assert!(matches!(
            event,
            AgentEvent::Permission { request, .. }
                if request.id == "t1"
                    && request.title == "Write file"
                    && request.options == ["Allow once", "Reject"]
                    && request.option_ids == ["allow-once", "reject"]
        ));
    }

    #[test]
    fn parses_acp_permission_request_as_json_rpc_request() {
        let event = parse_acp_notification(
            2,
            r#"{"jsonrpc":"2.0","id":17,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"title":"Write file"},"options":[{"optionId":"allow-once"},{"name":"reject"}]}}"#,
        )
        .expect("valid permission request")
        .expect("permission event");
        assert!(matches!(
            event,
            AgentEvent::Permission { request, .. }
                if request.id == "17"
                    && request.title == "Write file"
                    && request.options == ["allow-once", "reject"]
                    && request.option_ids == ["allow-once", "reject"]
        ));
    }

    #[tokio::test]
    async fn native_adapter_explicitly_rejects_permission_answers() {
        let mut adapter = AgyAdapter::new(0, std::env::current_dir().expect("cwd"), "agy");
        assert_eq!(
            adapter
                .answer_permission(
                    "request".into(),
                    PermissionAnswer::Selected {
                        option_id: "allow".into()
                    },
                )
                .await,
            Err(super::AdapterError::Unsupported("permission answer"))
        );
    }

    #[tokio::test]
    async fn native_mode_policy_aliases_resolve_to_its_supported_id() {
        let mut adapter = AgyAdapter::new(0, std::env::current_dir().expect("cwd"), "agy");
        adapter
            .set_mode("full-access".into())
            .await
            .expect("auto-pilot alias");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { current_mode: Some(mode), .. })) if mode == "agy:full-access"
        ));
    }

    #[tokio::test]
    async fn native_stream_persists_announced_conversation_for_follow_up_turns() {
        let script_path = unique_test_path("codeswarm-agy-session", "sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nprintf '%s\\n' '{\"event\":\"init\",\"conversation_id\":\"native-session\"}' '{\"event\":\"step_update\",\"step_update\":{\"step_type\":\"agent_response\",\"text_delta\":\"ok\"}}' '{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"response\":\"ok\"}}'\n",
        )
        .expect("write native test script");
        let mut adapter = AgyAdapter::new(
            0,
            std::env::current_dir().expect("cwd"),
            format!("sh {}", script_path.display()),
        );
        adapter.start().await.expect("start native adapter");
        // Startup emits its mode catalog and readiness before a turn.
        assert!(adapter.next_event().await.is_some());
        assert!(adapter.next_event().await.is_some());
        adapter
            .send_prompt("first".into())
            .await
            .expect("first prompt");
        while !matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ) {}
        assert_eq!(adapter.session_id.as_deref(), Some("native-session"));
        adapter
            .send_prompt("follow up".into())
            .await
            .expect("follow-up prompt");
        while !matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ) {}
        assert_eq!(adapter.session_id.as_deref(), Some("native-session"));
        adapter.stop().await.expect("stop native adapter");
        std::fs::remove_file(script_path).expect("cleanup native script");
    }

    #[tokio::test]
    async fn native_stream_reports_unsuccessful_result_as_crash_not_completion() {
        let script_path = unique_test_path("codeswarm-agy-failure", "sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nprintf '%s\\n' '{\"event\":\"result\",\"result\":{\"status\":\"FAILURE\",\"error\":\"agent failed\"}}'\n",
        )
        .expect("write native test script");
        let mut adapter = AgyAdapter::new(
            0,
            std::env::current_dir().expect("cwd"),
            format!("sh {}", script_path.display()),
        );
        adapter.start().await.expect("start native adapter");
        assert!(adapter.next_event().await.is_some());
        assert!(adapter.next_event().await.is_some());
        adapter.send_prompt("fail".into()).await.expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Failed { started: true, detail, .. }))
                if detail == "agent failed"
        ));
        adapter.stop().await.expect("stop native adapter");
        std::fs::remove_file(script_path).expect("cleanup native script");
    }

    #[tokio::test]
    async fn acp_adapter_initializes_session_and_completes_a_prompt() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"loadSession":true}}}'; read _; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"session-1","modes":{"currentModeId":"plan","availableModes":[{"id":"plan","name":"Plan"}]}}}'; read _; echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}'; echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'"#;
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::new(0, cwd, "sh", vec!["-c".into(), script.into()]);
        adapter.start().await.expect("initialize");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        adapter.send_prompt("hello".into()).await.expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Text { text, .. })) if text == "hello"
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
    }

    #[tokio::test]
    async fn acp_string_prompt_ids_complete_and_allow_a_follow_up_turn() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}'; read _; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"session-1","modes":{"currentModeId":"plan","availableModes":[{"id":"plan","name":"Plan"}]}}}'; read _; echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first"}}}}'; echo '{"jsonrpc":"2.0","id":"3","result":{"stopReason":"end_turn"}}'; read _; echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second"}}}}'; echo '{"jsonrpc":"2.0","id":"4","result":{"stopReason":"end_turn"}}'"#;
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::new(1, cwd, "sh", vec!["-c".into(), script.into()]);
        adapter.start().await.expect("initialize");
        assert!(adapter.next_event().await.is_some());
        assert!(adapter.next_event().await.is_some());

        for (prompt, expected) in [("first prompt", "first"), ("follow up", "second")] {
            adapter.send_prompt(prompt.into()).await.expect("prompt");
            assert!(matches!(
                adapter.next_event().await,
                Some(Ok(AgentEvent::Text { text, .. })) if text == expected
            ));
            assert!(matches!(
                adapter.next_event().await,
                Some(Ok(AgentEvent::TurnComplete { slot: 1 }))
            ));
        }
        adapter.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn empty_acp_mode_catalog_disables_mode_control() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}'; read _; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"session-1","modes":{"availableModes":[]}}}'"#;
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::new(0, cwd, "sh", vec!["-c".into(), script.into()]);
        adapter.start().await.expect("initialize");
        assert!(!adapter.capabilities().supports_modes);
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { modes, .. })) if modes.is_empty()
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { capabilities, .. })) if !capabilities.supports_modes
        ));
        adapter.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn acp_models_are_discovered_live_and_changed_through_session_config() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}'; read _; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"session-1","configOptions":[{"id":"model","category":"model","type":"select","currentValue":"fast","options":[{"value":"fast","name":"Fast"},{"value":"smart","name":"Smart"}]}]}}'; read request; case "$request" in *session/set_config_option*\"value\":\"smart\"*) echo '{"jsonrpc":"2.0","id":3,"result":{}}';; *) echo '{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"wrong model request"}}';; esac"#;
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::new(0, cwd, "sh", vec!["-c".into(), script.into()]);
        adapter.start().await.expect("initialize");
        assert!(adapter.capabilities().supports_models);
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModelsReplaced { config_id, models, current_model, .. }))
                if config_id == "model"
                    && models == [Mode { id: "fast".into(), label: "Fast".into() }, Mode { id: "smart".into(), label: "Smart".into() }]
                    && current_model.as_deref() == Some("fast")
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { capabilities, .. })) if capabilities.supports_models
        ));
        adapter.set_model("smart".into()).await.expect("set model");
        assert!(adapter.set_model("invented".into()).await.is_err());
        adapter.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn acp_mode_change_is_acknowledged_without_provider_notification() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}'; read _; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"session-1","modes":{"currentModeId":"plan","availableModes":[{"id":"plan","name":"Plan"},{"id":"yolo","name":"YOLO"}]}}}'; read _; echo '{"jsonrpc":"2.0","id":3,"result":{}}'"#;
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::new(0, cwd, "sh", vec!["-c".into(), script.into()]);
        adapter.start().await.expect("initialize");
        adapter
            .set_mode(crate::policy::DEFAULT_POLICY_ID.into())
            .await
            .expect("set mode");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModeUpdated { current_mode, .. })) if current_mode == "yolo"
        ));
        adapter.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn acp_reload_preserves_a_loadable_session_id() {
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::with_session_id(
            0,
            cwd,
            "__codeswarm_missing_acp_for_reload_test__",
            Vec::new(),
            "saved-session",
        );
        adapter.capabilities.supports_session_load = true;
        // A failed replacement process still must not erase the session ID:
        // the coordinator can report the startup error and offer another
        // reload, preserving the only handle that can resume the conversation.
        assert!(adapter.reload().await.is_err());
        assert_eq!(adapter.session_id.as_deref(), Some("saved-session"));
    }

    #[tokio::test]
    async fn acp_reload_starts_a_fresh_session_when_loading_is_not_supported() {
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::with_session_id(
            0,
            cwd,
            "__codeswarm_missing_nonloadable_acp__",
            Vec::new(),
            "stale-session",
        );
        adapter.capabilities.supports_session_load = false;
        assert!(adapter.reload().await.is_err());
        assert_eq!(adapter.session_id, None);
    }

    #[tokio::test]
    async fn acp_stream_ignores_diagnostic_junk_and_surfaces_prompt_errors() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}'; read _; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}'; read _; echo 'diagnostic from wrapper'; echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"partial"}}}}'; echo '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"capacity"}}'"#;
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::new(0, cwd, "sh", vec!["-c".into(), script.into()]);
        adapter.start().await.expect("initialize");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        adapter.send_prompt("hello".into()).await.expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Text { text, .. })) if text == "partial"
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Err(super::AdapterError::Protocol(detail))) if detail.contains("capacity")
        ));
    }

    #[tokio::test]
    async fn acp_adapter_loads_existing_session_when_capability_allows_it() {
        let script = r#"read _; echo '{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"loadSession":true}}}'; read _; echo '{"jsonrpc":"2.0","id":2,"result":{}}'"#;
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = AcpAdapter::with_session_id(
            0,
            cwd,
            "sh",
            vec!["-c".into(), script.into()],
            "existing-session",
        );
        adapter.start().await.expect("load existing session");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
    }

    #[tokio::test]
    async fn acp_start_failure_reaps_transport_process() {
        // The child emits an invalid initialize response and exits. The
        // adapter must not retain a live child after protocol startup fails;
        // this is the path used when a configured ACP command is unavailable
        // or speaks a different protocol.
        let mut adapter = AcpAdapter::new(
            0,
            std::env::current_dir().expect("cwd"),
            "sh",
            vec!["-c".into(), "printf 'not-json\\n'".into()],
        );
        assert!(adapter.start().await.is_err());
        assert!(adapter.child.is_none());
        assert!(adapter.reader.is_none());
    }

    #[tokio::test]
    async fn acp_adapter_answers_permission_json_rpc_requests() {
        let path = std::env::temp_dir().join(format!(
            "codeswarm-permission-answer-{}",
            std::process::id()
        ));
        let script = format!(
            r#"read _; echo '{{"jsonrpc":"2.0","id":1,"result":{{"agentCapabilities":{{}}}}}}'; read _; echo '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s1"}}}}'; read _; echo '{{"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{{"toolCall":{{"title":"Write file"}},"options":[{{"optionId":"allow-once","name":"Allow once"}}]}}}}'; read answer; printf '%s' "$answer" > '{}'; echo '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'"#,
            path.display()
        );
        let mut adapter = AcpAdapter::new(
            0,
            std::env::current_dir().expect("cwd"),
            "sh",
            vec!["-c".into(), script],
        );
        adapter.start().await.expect("start ACP");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        adapter.send_prompt("do it".into()).await.expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Permission { request, .. }))
                if request.id == "9"
                    && request.options == ["Allow once"]
                    && request.option_ids == ["allow-once"]
        ));
        adapter
            .answer_permission(
                "9".into(),
                PermissionAnswer::Selected {
                    option_id: "allow-once".into(),
                },
            )
            .await
            .expect("permission answer");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        let answer: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).expect("captured permission answer"),
        )
        .expect("valid JSON-RPC answer");
        assert_eq!(answer["id"], 9);
        assert_eq!(answer["result"]["outcome"]["outcome"], "selected");
        assert_eq!(answer["result"]["outcome"]["optionId"], "allow-once");
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn empty_acp_permission_options_are_not_exposed_as_a_blank_prompt() {
        let event = parse_acp_notification(
            0,
            r#"{"jsonrpc":"2.0","id":17,"method":"session/request_permission","params":{"options":[]}}"#,
        )
        .expect("valid JSON-RPC request");
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn native_stream_uses_success_result_response_when_chunks_are_missing() {
        let script_path = unique_test_path("codeswarm-agy-result-response", "sh");
        std::fs::write(
            &script_path,
            "#!/bin/sh\nprintf '%s\\n' '{\"event\":\"step_update\",\"step_update\":\"malformed\"}' '{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"response\":\"Recovered.\"}}'\n",
        )
        .expect("write native test script");
        let mut adapter = AgyAdapter::new(
            0,
            std::env::current_dir().expect("cwd"),
            format!("sh {}", script_path.display()),
        );
        adapter.start().await.expect("start native adapter");
        assert!(adapter.next_event().await.is_some());
        assert!(adapter.next_event().await.is_some());
        adapter
            .send_prompt("continue".into())
            .await
            .expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Text { text, .. })) if text == "Recovered."
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        adapter.stop().await.expect("stop native adapter");
        std::fs::remove_file(script_path).expect("cleanup native script");
    }

    #[test]
    fn acp_workspace_file_access_is_root_bound_and_size_limited() {
        let root = std::env::temp_dir().join(format!("codeswarm-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("workspace");
        std::fs::write(root.join("inside.txt"), "one\ntwo\nthree\n").expect("inside file");
        let outside =
            std::env::temp_dir().join(format!("codeswarm-outside-{}", std::process::id()));
        std::fs::write(&outside, "secret").expect("outside file");
        let link = root.join("outside-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let adapter = AcpAdapter::new(0, root.clone(), "unused", Vec::new());

        assert_eq!(
            adapter
                .read_workspace_text("inside.txt", Some(2), Some(1))
                .expect("read inside"),
            "two"
        );
        std::fs::write(
            root.join("large.txt"),
            vec![b'x'; MAX_FILE_READ_BYTES + 1024],
        )
        .expect("large file");
        let bounded = adapter
            .read_workspace_text("large.txt", None, None)
            .expect("bounded read");
        assert!(bounded.len() <= MAX_FILE_READ_BYTES);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("inside.txt"), root.join("inside-link"))
                .expect("internal symlink");
            assert_eq!(
                adapter
                    .read_workspace_text("inside-link", None, None)
                    .expect("read internal symlink"),
                "one\ntwo\nthree\n"
            );
        }
        assert!(adapter.workspace_path("../codeswarm-outside").is_err());
        assert!(
            adapter
                .workspace_path(&outside.display().to_string())
                .is_err()
        );
        #[cfg(unix)]
        assert!(adapter.workspace_path("outside-link").is_err());
        #[cfg(unix)]
        std::fs::remove_file(link).expect("cleanup symlink");
        #[cfg(unix)]
        std::fs::remove_file(root.join("inside-link")).expect("internal link cleanup");
        std::fs::remove_file(outside).expect("cleanup outside");
        std::fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[tokio::test]
    async fn running_terminal_output_omits_exit_status_until_completion() {
        let root = unique_test_path("codeswarm-terminal-output", "dir");
        std::fs::create_dir_all(&root).expect("workspace");
        let mut adapter = AcpAdapter::new(0, root.clone(), "unused", Vec::new());
        let result = adapter
            .terminal_create(&serde_json::json!({
                "command": "sh",
                "args": ["-c", "sleep 0.2; printf done"],
                "cwd": ".",
            }))
            .await
            .expect("terminal create");
        let id = result["terminalId"].as_str().expect("terminal id");
        let output = adapter.terminal_output(id).await.expect("terminal output");
        assert!(output.get("exitStatus").is_none());
        if let Some(terminal) = adapter.terminals.remove(id) {
            terminal.stop().await;
        }
        std::fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[tokio::test]
    async fn acp_adapter_answers_workspace_read_requests() {
        let root =
            std::env::temp_dir().join(format!("codeswarm-fs-request-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("workspace");
        let source = root.join("inside.txt");
        let answer = root.join("answer.json");
        std::fs::write(&source, "workspace content").expect("source");
        let script = format!(
            r#"read _; echo '{{"jsonrpc":"2.0","id":1,"result":{{"agentCapabilities":{{}}}}}}'; read _; echo '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s1"}}}}'; read _; echo '{{"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{{"sessionId":"s1","path":"{}"}}}}'; read response; printf '%s' "$response" > '{}'; echo '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'"#,
            source.display(),
            answer.display(),
        );
        let mut adapter = AcpAdapter::new(0, root.clone(), "sh", vec!["-c".into(), script]);
        adapter.start().await.expect("start ACP");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        adapter.send_prompt("read it".into()).await.expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        let response: Value =
            serde_json::from_str(&std::fs::read_to_string(&answer).expect("captured fs response"))
                .expect("response JSON");
        assert_eq!(response["id"], 9);
        assert_eq!(response["result"]["content"], "workspace content");
        adapter.stop().await.expect("stop ACP");
        std::fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[tokio::test]
    async fn acp_adapter_runs_and_reports_client_mediated_terminals() {
        let root =
            std::env::temp_dir().join(format!("codeswarm-terminal-request-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("workspace");
        let create_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "terminal/create",
            "params": {
                "sessionId": "s1",
                "command": "sh",
                "args": ["-c", "sleep 0.1; printf terminal-ok"],
                "cwd": ".",
            },
        });
        let wait_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "terminal/wait_for_exit",
            "params": {"sessionId": "s1", "terminalId": "terminal-1"},
        });
        let output_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "terminal/output",
            "params": {"sessionId": "s1", "terminalId": "terminal-1"},
        });
        let create_answer = root.join("create-answer.json");
        let wait_answer = root.join("wait-answer.json");
        let output_answer = root.join("output-answer.json");
        let script = format!(
            "read _; echo '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"agentCapabilities\":{{}}}}}}'; read _; echo '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessionId\":\"s1\"}}}}'; read _; echo '{}'; read response; printf '%s' \"$response\" > '{}'; echo '{}'; read response; printf '%s' \"$response\" > '{}'; echo '{}'; read response; printf '%s' \"$response\" > '{}'; echo '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"stopReason\":\"end_turn\"}}}}'",
            create_request,
            create_answer.display(),
            wait_request,
            wait_answer.display(),
            output_request,
            output_answer.display(),
        );
        let mut adapter = AcpAdapter::new(0, root.clone(), "sh", vec!["-c".into(), script]);
        adapter.start().await.expect("start ACP");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        adapter
            .send_prompt("run terminal".into())
            .await
            .expect("prompt");
        let mut saw_complete = false;
        for _ in 0..6 {
            match adapter.next_event().await {
                Some(Ok(AgentEvent::TurnComplete { .. })) => {
                    saw_complete = true;
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        assert!(saw_complete, "terminal requests should not stall ACP");
        let create: Value = serde_json::from_str(
            &std::fs::read_to_string(&create_answer).expect("captured create response"),
        )
        .expect("create JSON");
        assert_eq!(create["result"]["terminalId"], "terminal-1");
        let output: Value = serde_json::from_str(
            &std::fs::read_to_string(&output_answer).expect("captured output response"),
        )
        .expect("output JSON");
        assert!(
            output["result"]["output"]
                .as_str()
                .unwrap_or_default()
                .contains("terminal-ok"),
            "output response: {output}"
        );
        adapter.stop().await.expect("stop ACP");
        std::fs::remove_dir_all(root).expect("cleanup workspace");
    }

    #[tokio::test]
    async fn host_reduces_and_persists_adapter_events() {
        let path =
            std::env::temp_dir().join(format!("codeswarm-host-{}.jsonl", std::process::id()));
        let adapter = ScriptedAdapter::new(
            0,
            AgentCapabilities::default(),
            [AgentEvent::Text {
                slot: 0,
                text: "hello".into(),
            }],
        );
        let mut host = AdapterHost::new(Box::new(adapter), Some(EventLog::open(&path)));
        host.start().await.expect("start");
        host.next_effects()
            .await
            .expect("event")
            .expect("valid event");
        assert_eq!(host.state.public_text[0].1, "hello");
        assert_eq!(EventLog::open(&path).read().expect("read").len(), 1);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[tokio::test]
    async fn relay_applies_default_policy_before_the_first_prompt() {
        let first_log = Arc::new(Mutex::new(Vec::new()));
        let second_log = Arc::new(Mutex::new(Vec::new()));
        let first = AdapterHost::new(
            Box::new(ModeOrderAdapter {
                slot: 0,
                log: Arc::clone(&first_log),
                phase: 0,
            }),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ModeOrderAdapter {
                slot: 1,
                log: Arc::clone(&second_log),
                phase: 0,
            }),
            None,
        );
        let mut relay = RelayHost::new(vec![first, second], 4).expect("relay");
        relay.start().await.expect("start and synchronize policy");
        relay.run_turn("task", 0).await.expect("first turn");
        {
            let log = first_log.lock().expect("log");
            assert_eq!(log.as_slice(), ["start", "mode:yolo", "prompt"]);
        }
        assert_eq!(
            second_log.lock().expect("log").as_slice(),
            ["start", "mode:yolo"]
        );
        let added_log = Arc::new(Mutex::new(Vec::new()));
        relay
            .add_agent(
                AdapterHost::new(
                    Box::new(ModeOrderAdapter {
                        slot: 2,
                        log: Arc::clone(&added_log),
                        phase: 0,
                    }),
                    None,
                ),
                "Added",
                "added.example",
                "added-agent",
            )
            .await
            .expect("add with synchronized policy");
        assert_eq!(
            added_log.lock().expect("log").as_slice(),
            ["start", "mode:yolo"]
        );
        relay.drop_agent(2).await.expect("drop added agent");
        added_log.lock().expect("log").clear();
        relay
            .reload(2)
            .await
            .expect("reload with synchronized policy");
        assert_eq!(
            added_log.lock().expect("log").as_slice(),
            ["reload", "mode:yolo"]
        );
    }

    #[tokio::test]
    async fn acp_roster_is_ready_before_any_prompt_is_sent() {
        let hosts = (0..2)
            .map(|slot| AdapterHost::new(Box::new(StartupAcpAdapter::new(slot)), None))
            .collect::<Vec<_>>();
        let startup_events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&startup_events);
        let mut relay = RelayHost::new(hosts, 4).expect("relay");
        relay.set_event_sink(move |event| captured.lock().expect("events").push(event));

        relay.start().await.expect("complete startup handshake");

        assert!(relay.dispatches().is_empty());
        let ready_slots = startup_events
            .lock()
            .expect("events")
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Ready { slot, .. } => Some(*slot),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ready_slots, vec![0, 1]);
    }

    #[tokio::test]
    async fn independent_roster_adapters_start_concurrently() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let hosts = (0..2)
            .map(|slot| {
                AdapterHost::new(
                    Box::new(ConcurrentStartAdapter {
                        slot,
                        barrier: Arc::clone(&barrier),
                    }),
                    None,
                )
            })
            .collect::<Vec<_>>();
        let mut relay = RelayHost::new(hosts, 4).expect("relay");
        tokio::time::timeout(std::time::Duration::from_millis(100), relay.start())
            .await
            .expect("startup should not serialize barrier participants")
            .expect("startup succeeds");
    }

    #[tokio::test]
    async fn relay_host_dispatches_turns_sequentially() {
        let capabilities = AgentCapabilities {
            supports_cancel: true,
            ..AgentCapabilities::default()
        };
        let first = ScriptedAdapter::new(
            0,
            capabilities.clone(),
            [
                AgentEvent::Text {
                    slot: 0,
                    text: "first".into(),
                },
                AgentEvent::TurnComplete { slot: 0 },
            ],
        );
        let second = ScriptedAdapter::new(
            1,
            capabilities,
            [
                AgentEvent::Text {
                    slot: 1,
                    text: "review".into(),
                },
                AgentEvent::TurnComplete { slot: 1 },
            ],
        );
        let hosts = vec![
            AdapterHost::new(Box::new(first), None),
            AdapterHost::new(Box::new(second), None),
        ];
        let mut relay = super::RelayHost::new(hosts, 4).expect("relay");
        relay.set_roster_names(vec!["Claude".into(), "Codex".into()]);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        relay.set_event_sink(move |event| captured.lock().expect("events").push(event));
        relay.start().await.expect("start");
        events.lock().expect("events").clear();
        assert!(matches!(
            relay.run_turn("task", 0).await.expect("first turn"),
            crate::relay::RelayDecision::Dispatch { slot: 0, .. }
        ));
        assert!(matches!(
            relay.run_turn("first", 0).await.expect("second turn"),
            crate::relay::RelayDecision::Dispatch {
                slot: 1,
                can_stop: true,
                ..
            }
        ));
        assert_eq!(
            relay
                .dispatches()
                .iter()
                .map(|(slot, _)| *slot)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(relay.dispatches()[0].1.contains("You are Claude"));
        assert!(
            relay.dispatches()[0]
                .1
                .contains("CodeSwarm roster (ordered)")
        );
        assert!(relay.dispatches()[0].1.contains("1. Claude — you"));
        assert!(relay.dispatches()[0].1.contains("2. Codex"));
        assert!(relay.dispatches()[1].1.contains(STOP_TOKEN));
        assert!(relay.dispatches()[0].1.contains("Do not use"));
        let lifecycle = events.lock().expect("events");
        let positions = lifecycle
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TurnStarted { slot } => Some(("start", *slot)),
                AgentEvent::TurnComplete { slot } => Some(("complete", *slot)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            [("start", 0), ("complete", 0), ("start", 1), ("complete", 1)]
        );
    }

    #[tokio::test]
    async fn relay_host_routes_around_a_usage_limited_agent() {
        let capabilities = AgentCapabilities::default();
        let first = ScriptedAdapter::new(
            0,
            capabilities.clone(),
            [
                AgentEvent::Text {
                    slot: 0,
                    text: "You've hit your usage limit. Visit chatgpt.com to purchase more \
                           credits or try again later."
                        .into(),
                },
                AgentEvent::TurnComplete { slot: 0 },
            ],
        );
        let second = ScriptedAdapter::new(
            1,
            capabilities,
            [
                AgentEvent::Text {
                    slot: 1,
                    text: "review done".into(),
                },
                AgentEvent::TurnComplete { slot: 1 },
            ],
        );
        let hosts = vec![
            AdapterHost::new(Box::new(first), None),
            AdapterHost::new(Box::new(second), None),
        ];
        let mut relay = super::RelayHost::new(hosts, 4).expect("relay");
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        relay.set_event_sink(move |event| captured.lock().expect("events").push(event));
        relay.start().await.expect("start");
        events.lock().expect("events").clear();
        assert!(matches!(
            relay.run_turn("task", 0).await.expect("limited turn"),
            crate::relay::RelayDecision::Dispatch { slot: 0, .. }
        ));
        assert!(
            events
                .lock()
                .expect("events")
                .iter()
                .any(|event| matches!(event, AgentEvent::UsageLimitReached { slot: 0, .. }))
        );
        // The next automatic turn skips the limited agent entirely.
        assert!(matches!(
            relay.run_turn("", 0).await.expect("next turn"),
            crate::relay::RelayDecision::Dispatch { slot: 1, .. }
        ));
        assert!(relay.relay().is_limited(0));
        // A reload restores the agent to the ring. (ScriptedAdapter cannot
        // feed further turns, so the restored routing itself is covered by
        // the relay unit tests.)
        relay.reload(0).await.expect("reload");
        assert!(!relay.relay().is_limited(0));
    }

    #[tokio::test]
    async fn relay_host_routes_around_usage_limit_failures_without_tombstoning() {
        let limited = ScriptedAdapter::new(
            0,
            AgentCapabilities::default(),
            [AgentEvent::Failed {
                slot: 0,
                started: true,
                detail: "request failed: insufficient_quota".into(),
            }],
        );
        let healthy = ScriptedAdapter::new(
            1,
            AgentCapabilities::default(),
            [AgentEvent::TurnComplete { slot: 1 }],
        );
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(limited), None),
                AdapterHost::new(Box::new(healthy), None),
            ],
            4,
        )
        .expect("relay");
        relay.set_event_sink(move |event| captured.lock().expect("events").push(event));
        relay.start().await.expect("start");
        events.lock().expect("events").clear();

        assert!(matches!(
            relay.run_turn("task", 0).await.expect("limited failure"),
            crate::relay::RelayDecision::Dispatch { slot: 0, .. }
        ));
        assert!(relay.relay().is_limited(0));
        assert_eq!(relay.relay().active_slots().collect::<Vec<_>>(), [0, 1]);
        {
            let events = events.lock().expect("events");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::UsageLimitReached { slot: 0, .. }))
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::Failed { .. }))
            );
        }

        assert!(matches!(
            relay.run_turn("", 0).await.expect("healthy peer"),
            crate::relay::RelayDecision::Dispatch { slot: 1, .. }
        ));
    }

    #[tokio::test]
    async fn relay_failure_is_tombstoned_and_reported_to_the_ui_sink() {
        let failed = ScriptedAdapter::new(
            0,
            AgentCapabilities::default(),
            [AgentEvent::Failed {
                slot: 0,
                started: true,
                detail: "connection lost".into(),
            }],
        );
        let healthy = ScriptedAdapter::new(
            1,
            AgentCapabilities::default(),
            [AgentEvent::TurnComplete { slot: 1 }],
        );
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(failed), None),
                AdapterHost::new(Box::new(healthy), None),
            ],
            4,
        )
        .expect("relay");
        relay.set_event_sink(move |event| captured.lock().expect("lock").push(event));
        relay.start().await.expect("start");

        let error = relay.run_turn("task", 0).await.expect_err("failure");
        assert!(error.to_string().contains("connection lost"));
        assert_eq!(relay.relay().active_slots().collect::<Vec<_>>(), vec![1]);
        assert!(events.lock().expect("lock").iter().any(|event| {
            matches!(
                event,
                AgentEvent::Failed {
                    slot: 0,
                    started: true,
                    ..
                }
            )
        }));
    }

    #[tokio::test]
    async fn codex_stop_does_not_skip_later_roster_reviewers() {
        let hosts = (0..3)
            .map(|slot| {
                AdapterHost::new(
                    Box::new(ScriptedAdapter::new(
                        slot,
                        AgentCapabilities::default(),
                        [
                            AgentEvent::Text {
                                slot,
                                text: STOP_TOKEN.into(),
                            },
                            AgentEvent::TurnComplete { slot },
                        ],
                    )),
                    None,
                )
            })
            .collect();
        let mut relay = RelayHost::new(hosts, 10).expect("relay");
        relay.set_roster_names(vec!["Claude".into(), "Codex".into(), "Qwen".into()]);
        relay.start().await.expect("start");
        for expected in 0..3 {
            assert!(matches!(relay.run_turn("task", 0).await.expect("turn"),
                RelayDecision::Dispatch { slot, can_stop, .. } if slot == expected && can_stop == (expected == 2)));
        }
        assert_eq!(
            relay.run_turn("", 0).await.expect("complete"),
            RelayDecision::Complete
        );
    }

    #[tokio::test]
    async fn reviewer_stop_token_ends_the_automatic_relay_sequence() {
        let first = ScriptedAdapter::new(
            0,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 0,
                    text: "done".into(),
                },
                AgentEvent::TurnComplete { slot: 0 },
            ],
        );
        let reviewer = ScriptedAdapter::new(
            1,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 1,
                    text: STOP_TOKEN.into(),
                },
                AgentEvent::TurnComplete { slot: 1 },
            ],
        );
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(first), None),
                AdapterHost::new(Box::new(reviewer), None),
            ],
            10,
        )
        .expect("relay");
        relay.start().await.expect("start");
        let first_decision = relay.run_turn("task", 0).await.expect("first");
        assert!(matches!(
            first_decision,
            RelayDecision::Dispatch { slot: 0, .. }
        ));
        let reviewer_decision = relay.run_turn("", 0).await.expect("reviewer");
        assert!(matches!(
            reviewer_decision,
            RelayDecision::Dispatch {
                slot: 1,
                can_stop: true,
                ..
            }
        ));
        assert_eq!(
            relay.run_turn("", 0).await.expect("complete"),
            RelayDecision::Complete
        );
    }

    #[tokio::test]
    async fn relay_stream_emits_text_and_thought_endings_before_tools() {
        let tool = AgentEvent::Tool {
            slot: 0,
            update: crate::ToolUpdate {
                id: "read".into(),
                title: "Read file".into(),
                status: ToolStatus::Running,
                detail: None,
            },
        };
        let updates = vec![
            AgentEvent::Thought {
                slot: 0,
                text: "Check the buffer. ✈".into(),
            },
            AgentEvent::Text {
                slot: 0,
                text: "Let me check.".into(),
            },
            tool.clone(),
            AgentEvent::Text {
                slot: 0,
                text: "[CODE".into(),
            },
            AgentEvent::Text {
                slot: 0,
                text: " is ordinary.".into(),
            },
            AgentEvent::Text {
                slot: 0,
                text: "[CODESWARM:".into(),
            },
            AgentEvent::Text {
                slot: 0,
                text: "STOP] Done.".into(),
            },
            tool,
            AgentEvent::TurnComplete { slot: 0 },
        ];
        let first = ScriptedAdapter::new(0, AgentCapabilities::default(), updates.clone());
        let reviewer = ScriptedAdapter::new(1, AgentCapabilities::default(), []);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(first), None),
                AdapterHost::new(Box::new(reviewer), None),
            ],
            2,
        )
        .expect("relay");
        relay.set_event_sink(move |event| captured.lock().unwrap().push(event));
        relay.start().await.unwrap();
        relay.run_turn("task", 0).await.unwrap();
        let captured = events.lock().unwrap();
        let visible: Vec<_> = captured
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::Text { .. } | AgentEvent::Thought { .. } | AgentEvent::Tool { .. }
                )
            })
            .cloned()
            .collect();
        assert_eq!(
            visible,
            vec![
                updates[0].clone(),
                updates[1].clone(),
                updates[2].clone(),
                AgentEvent::Text {
                    slot: 0,
                    text: "[CODE is ordinary.".into()
                },
                AgentEvent::Text {
                    slot: 0,
                    text: " Done.".into()
                },
                updates[7].clone(),
            ]
        );
    }

    #[tokio::test]
    async fn stop_token_is_filtered_from_streamed_ui_events() {
        let first = ScriptedAdapter::new(
            0,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 0,
                    text: format!("visible {STOP_TOKEN} trailing"),
                },
                AgentEvent::TurnComplete { slot: 0 },
            ],
        );
        let reviewer = ScriptedAdapter::new(
            1,
            AgentCapabilities::default(),
            [AgentEvent::TurnComplete { slot: 1 }],
        );
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(first), None),
                AdapterHost::new(Box::new(reviewer), None),
            ],
            2,
        )
        .expect("relay");
        relay.set_event_sink(move |event| captured.lock().expect("lock").push(event));
        relay.start().await.expect("start");
        relay.run_turn("task", 0).await.expect("turn");
        let captured = events.lock().expect("lock");
        assert!(captured.iter().all(|event| match event {
            AgentEvent::Text { text, .. } => !text.contains(STOP_TOKEN),
            _ => true,
        }));
        let visible = captured
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(visible, "visible  trailing");
    }

    #[tokio::test]
    async fn token_only_reviewer_response_emits_visible_acknowledgment() {
        let first = ScriptedAdapter::new(
            0,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 0,
                    text: "done".into(),
                },
                AgentEvent::TurnComplete { slot: 0 },
            ],
        );
        let reviewer = ScriptedAdapter::new(
            1,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 1,
                    text: STOP_TOKEN.into(),
                },
                AgentEvent::TurnComplete { slot: 1 },
            ],
        );
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(first), None),
                AdapterHost::new(Box::new(reviewer), None),
            ],
            4,
        )
        .expect("relay");
        relay.set_event_sink(move |event| captured.lock().expect("lock").push(event));
        relay.start().await.expect("start");
        relay.run_turn("task", 0).await.expect("first turn");
        relay.run_turn("", 0).await.expect("review turn");
        let captured = events.lock().expect("lock");
        assert!(captured.iter().any(|event| {
            matches!(
                event,
                AgentEvent::Text { slot: 1, text } if text == DEFAULT_STOP_ACKNOWLEDGMENT
            )
        }));
        assert!(captured.iter().all(|event| match event {
            AgentEvent::Text { text, .. } => !text.contains(STOP_TOKEN),
            _ => true,
        }));
        let acknowledgment = captured
            .iter()
            .position(|event| {
                matches!(
                    event,
                    AgentEvent::Text { slot: 1, text } if text == DEFAULT_STOP_ACKNOWLEDGMENT
                )
            })
            .expect("visible acknowledgment");
        let completion = captured
            .iter()
            .position(|event| matches!(event, AgentEvent::TurnComplete { slot: 1 }))
            .expect("reviewer completion");
        assert!(acknowledgment < completion);
    }

    #[tokio::test]
    async fn explicit_reviewer_acknowledgment_is_not_duplicated_at_stop() {
        let first = ScriptedAdapter::new(
            0,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 0,
                    text: "done".into(),
                },
                AgentEvent::TurnComplete { slot: 0 },
            ],
        );
        let reviewer = ScriptedAdapter::new(
            1,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 1,
                    text: format!("{DEFAULT_STOP_ACKNOWLEDGMENT}\n{STOP_TOKEN}"),
                },
                AgentEvent::TurnComplete { slot: 1 },
            ],
        );
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(first), None),
                AdapterHost::new(Box::new(reviewer), None),
            ],
            4,
        )
        .expect("relay");
        relay.set_event_sink(move |event| captured.lock().expect("lock").push(event));
        relay.start().await.expect("start");
        relay.run_turn("task", 0).await.expect("first turn");
        relay.run_turn("", 0).await.expect("review turn");

        let visible = events
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Text { slot: 1, text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(visible.trim(), DEFAULT_STOP_ACKNOWLEDGMENT);
        assert_eq!(visible.matches(DEFAULT_STOP_ACKNOWLEDGMENT).count(), 1);
    }

    #[tokio::test]
    async fn relay_permission_answer_is_consumed_before_the_turn_completes() {
        let first = AdapterHost::new(
            Box::new(PermissionBlockingAdapter { slot: 0, phase: 0 }),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                1,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 1 }],
            )),
            None,
        );
        let mut relay = super::RelayHost::new(vec![first, second], 4).expect("relay");
        let (seen_sender, mut seen_receiver) = tokio::sync::mpsc::unbounded_channel();
        relay.set_event_sink(move |event| {
            if matches!(event, AgentEvent::Permission { .. }) {
                let _ = seen_sender.send(());
            }
        });
        relay.start().await.expect("start");
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let answer = async move {
            seen_receiver.recv().await.expect("permission request");
            sender
                .send(super::RelayPermissionAnswer {
                    slot: 0,
                    request_id: "permission-1".into(),
                    answer: PermissionAnswer::Selected {
                        option_id: "allow".into(),
                    },
                })
                .expect("queue permission answer");
        };
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            let ((), result) = tokio::join!(
                answer,
                relay.run_turn_with_permissions("task", 0, &mut receiver)
            );
            result
        })
        .await
        .expect("permission-gated turn should not deadlock")
        .expect("turn completes");
    }

    #[tokio::test]
    async fn relay_cancellation_interrupts_a_waiting_adapter_turn() {
        let first = AdapterHost::new(
            Box::new(PendingAdapter {
                slot: 0,
                hang_on_cancel: false,
            }),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                1,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 1 }],
            )),
            None,
        );
        let mut relay = super::RelayHost::new(vec![first, second], 4).expect("relay");
        relay.start().await.expect("start");
        let cancellation = relay.cancellation();
        let error = {
            let turn = relay.run_turn("task", 0);
            tokio::pin!(turn);
            cancellation.request();
            turn.await.expect_err("cancellation should stop turn")
        };
        assert!(error.to_string().contains("relay turn cancelled"));

        assert!(relay.relay_mut().enqueue_human("replacement job", Some(1)));
        relay
            .run_turn("", 1)
            .await
            .expect("replacement job reaches the selected peer");
        let replacement = &relay.dispatches().last().expect("replacement dispatch").1;
        assert!(replacement.contains("replacement job"));
        assert!(replacement.contains("User "));
        assert!(replacement.contains(":\ntask"));
        let owner_updates = relay.relay_mut().unseen_context(0);
        assert!(owner_updates.contains("User "));
        assert!(owner_updates.contains(":\ntask"));
        assert!(owner_updates.contains(":\nreplacement job"));
    }

    #[tokio::test]
    async fn relay_cancellation_does_not_wait_forever_for_a_broken_adapter() {
        let first = AdapterHost::new(
            Box::new(PendingAdapter {
                slot: 0,
                hang_on_cancel: true,
            }),
            None,
        );
        let second = AdapterHost::new(
            Box::new(PendingAdapter {
                slot: 1,
                hang_on_cancel: false,
            }),
            None,
        );
        let mut relay = super::RelayHost::new(vec![first, second], 4).expect("relay");
        relay.start().await.expect("start");
        let cancellation = relay.cancellation();
        let turn = relay.run_turn("task", 0);
        tokio::pin!(turn);
        cancellation.request();
        let error = turn.await.expect_err("cancellation should stop turn");
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn relay_host_pause_and_single_healthy_agent_continues_without_peer_review() {
        let event = [AgentEvent::TurnComplete { slot: 0 }];
        let first = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                0,
                AgentCapabilities::default(),
                event.clone(),
            )),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                1,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 1 }],
            )),
            None,
        );
        let mut relay = super::RelayHost::new(vec![first, second], 4).expect("relay");
        relay.start().await.expect("start");

        relay.pause();
        assert_eq!(
            relay.run_turn("paused", 0).await.expect("paused turn"),
            crate::relay::RelayDecision::Paused
        );
        assert!(relay.dispatches().is_empty());

        relay.resume();
        relay.relay_mut().drop_agent(1).expect("drop reviewer");
        assert!(matches!(
            relay
                .run_turn("solo follow-up", 0)
                .await
                .expect("solo turn"),
            crate::relay::RelayDecision::Dispatch {
                slot: 0,
                can_stop: false,
                ..
            }
        ));
        assert_eq!(relay.dispatches().len(), 1);
    }

    #[tokio::test]
    async fn relay_host_can_append_a_started_adapter_in_a_new_slot() {
        let first = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                0,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 0 }],
            )),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                1,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 1 }],
            )),
            None,
        );
        let mut relay = RelayHost::new(vec![first, second], 4).expect("relay");
        relay.set_roster_names(vec!["First".into(), "Second".into()]);
        relay.set_roster_identities(vec!["owner.example".into(), "peer.example".into()]);
        relay.set_roster_launch_specs(vec![
            ("custom".into(), "owner".into()),
            ("custom".into(), "peer".into()),
        ]);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        relay.set_event_sink(move |event| captured.lock().expect("lock").push(event));
        relay.start().await.expect("start");
        let slot = relay
            .add_agent(
                AdapterHost::new(
                    Box::new(ScriptedAdapter::new(
                        2,
                        AgentCapabilities::default(),
                        [AgentEvent::TurnComplete { slot: 2 }],
                    )),
                    None,
                ),
                "Reviewer",
                "reviewer.example",
                "reviewer --acp",
            )
            .await
            .expect("append agent");
        assert_eq!(slot, 2);
        assert_eq!(
            relay.relay().active_slots().collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            relay
                .session_metadata()
                .get("agents")
                .and_then(|value| value.as_array())
                .map(Vec::len),
            Some(3)
        );
        relay.drop_agent(1).await.expect("drop middle peer");
        let metadata = relay.session_metadata();
        assert_eq!(
            metadata.get("agents"),
            Some(&serde_json::json!([
                {"name": "First", "identity": "owner.example", "protocol": "custom", "command": "owner", "supports_load_session": false},
                {"name": "Reviewer", "identity": "reviewer.example", "protocol": "custom", "command": "reviewer --acp", "supports_load_session": false}
            ]))
        );
        assert!(
            events
                .lock()
                .expect("lock")
                .iter()
                .any(|event| { matches!(event, AgentEvent::Ready { slot: 2, .. }) })
        );
    }

    #[tokio::test]
    async fn relay_host_persists_coordinator_owned_runtime_metadata() {
        let path = unique_test_path("codeswarm-session-metadata", "json");
        let metadata_store = crate::persistence::SessionMetadataStore::open(&path);
        let writer = metadata_store.buffered().expect("metadata writer");
        let first = AdapterHost::new(
            Box::new(ScriptedAdapter::new(0, AgentCapabilities::default(), [])),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(1, AgentCapabilities::default(), [])),
            None,
        );
        let mut relay = RelayHost::new(vec![first, second], 4).expect("relay");
        relay.set_roster_names(vec!["Claude".into(), "Codex".into()]);
        relay.set_roster_identities(vec!["claude.ai".into(), "openai.com".into()]);
        relay.set_roster_launch_specs(vec![
            ("custom".into(), "claude".into()),
            ("custom".into(), "codex".into()),
        ]);
        relay.set_session_metadata_writer(writer);
        relay.start().await.expect("start");
        relay.drop_agent(0).await.expect("drop first agent");
        relay.stop().await.expect("stop");

        let loaded = metadata_store
            .read()
            .expect("read metadata")
            .expect("metadata snapshot");
        assert_eq!(loaded.get("title"), Some(&serde_json::json!("CodeSwarm")));
        assert_eq!(
            loaded.get("agents"),
            Some(&serde_json::json!([{
                "name": "Codex", "identity": "openai.com", "protocol": "custom",
                "command": "codex", "supports_load_session": false
            }]))
        );
        assert!(loaded.get("owner").is_none());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn relay_host_swaps_live_adapters_and_remaps_stream_events() {
        let first = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                0,
                AgentCapabilities::default(),
                [
                    AgentEvent::Text {
                        slot: 0,
                        text: "owner stream".into(),
                    },
                    AgentEvent::TurnComplete { slot: 0 },
                ],
            )),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                1,
                AgentCapabilities::default(),
                [
                    AgentEvent::Text {
                        slot: 1,
                        text: "peer stream".into(),
                    },
                    AgentEvent::TurnComplete { slot: 1 },
                ],
            )),
            None,
        );
        let mut relay = RelayHost::new(vec![first, second], 4).expect("relay");
        relay.set_roster_names(vec!["Owner".into(), "Peer".into()]);
        relay.set_roster_identities(vec!["first.example".into(), "second.example".into()]);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&events);
        relay.set_event_sink(move |event| captured.lock().expect("lock").push(event));
        relay.start().await.expect("start");

        relay.swap_agents(0, 1).expect("swap peers");
        assert_eq!(relay.active_slot_for_identity("first.example"), Some(1));
        assert_eq!(relay.active_slot_for_identity("second.example"), Some(0));
        relay.run_turn("task", 0).await.expect("swapped turn");
        let events = events.lock().expect("events");
        assert!(events.iter().any(|event| {
            matches!(event, AgentEvent::Text { slot: 0, text } if text == "peer stream")
        }));
        assert!(relay.dispatches()[0].1.contains("You are Peer"));
    }

    #[tokio::test]
    async fn relay_host_persists_all_active_agent_metadata_off_thread() {
        let path = unique_test_path("codeswarm-session-metadata", "json");
        let first = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                0,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 0 }],
            )),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                1,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 1 }],
            )),
            None,
        );
        let mut relay = RelayHost::new(vec![first, second], 4).expect("relay");
        relay.set_roster_names(vec!["Claude".into(), "Codex".into()]);
        relay.set_roster_identities(vec!["claude.com".into(), "openai.com".into()]);
        relay.set_roster_launch_specs(vec![
            ("custom".into(), "claude".into()),
            ("custom".into(), "codex".into()),
        ]);
        let writer = SessionMetadataStore::open(&path)
            .buffered()
            .expect("metadata writer");
        relay.set_session_metadata_writer(writer);
        relay.start().await.expect("start");
        relay.stop().await.expect("stop");
        let loaded = SessionMetadataStore::open(&path)
            .read()
            .expect("read metadata")
            .expect("metadata snapshot");
        let agents = loaded
            .get("agents")
            .and_then(|value| value.as_array())
            .expect("agents");
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0]["identity"], "claude.com");
        assert_eq!(agents[1]["identity"], "openai.com");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn relay_host_routes_unseen_public_context_to_next_agent() {
        let first = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                0,
                AgentCapabilities::default(),
                [
                    AgentEvent::Text {
                        slot: 0,
                        text: "implemented the fix".into(),
                    },
                    AgentEvent::TurnComplete { slot: 0 },
                ],
            )),
            None,
        );
        let second = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                1,
                AgentCapabilities::default(),
                [AgentEvent::TurnComplete { slot: 1 }],
            )),
            None,
        );
        let mut relay = super::RelayHost::new(vec![first, second], 4).expect("relay");
        relay.set_roster_names(vec!["Codex".into(), "Qwen".into()]);
        relay.start().await.expect("start");
        relay.run_turn("task", 0).await.expect("first turn");
        relay.run_turn("review this", 0).await.expect("review turn");

        assert_eq!(relay.dispatches().len(), 2);
        assert_eq!(relay.dispatches()[0].0, 0);
        assert!(relay.dispatches()[0].1.contains("task"));
        assert!(relay.dispatches()[0].1.contains("You are Codex"));
        assert!(relay.dispatches()[0].1.contains("2. Qwen"));
        assert_eq!(relay.dispatches()[1].0, 1);
        assert!(relay.dispatches()[1].1.contains("review this"));
        let public = relay.dispatches()[1]
            .1
            .split_once("Public updates:\n")
            .map(|(_, updates)| updates)
            .expect("review receives public context");
        let header = public
            .lines()
            .find(|line| line.starts_with("Codex "))
            .expect("named previous agent");
        let timestamp = header
            .strip_prefix("Codex ")
            .and_then(|value| value.strip_suffix(':'))
            .expect("timestamped header");
        assert_eq!(timestamp.len(), 5);
        assert_eq!(timestamp.as_bytes()[2], b':');
        assert!(
            timestamp
                .bytes()
                .enumerate()
                .all(|(index, byte)| { index == 2 || byte.is_ascii_digit() })
        );
        assert!(public.contains("implemented the fix"));
        assert!(!public.contains("Agent 0"));
    }
}
