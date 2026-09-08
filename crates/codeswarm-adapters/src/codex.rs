//! Native adapter for the Codex command-line exec protocol.
//!
//! Codex's `exec --json` command is a JSONL stream for one turn. A process is
//! intentionally created per prompt: Codex persists the thread and exposes a
//! stable thread ID, while `exec resume` restores that thread for the next
//! prompt. Keeping the process boundary here avoids an ACP/Node bridge.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
};

use crate::{
    AgentCapabilities, AgentEvent, Mode, PermissionAnswer, RosterSlot, ToolStatus, ToolUpdate,
};

use super::{
    AdapterError, AdapterResult, AgentAdapter, CANCEL_SETTLE_TIMEOUT, drain_bounded,
    isolate_process_group,
    native::{NativeTurn, spawn_native_turn},
    parse_command_line, terminate_child,
};

const MODE_AUTO: &str = "codeswarm:mode:full-access";
const MODE_PLAN: &str = "codeswarm:mode:plan";
const MODEL_CONFIG_ID: &str = "codex:model";

#[derive(Debug, Default)]
struct ParserState {
    messages: BTreeMap<String, String>,
    thoughts: BTreeMap<String, String>,
    tools: BTreeMap<String, ToolUpdate>,
}

fn text_value(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(text) => (!text.is_empty()).then(|| text.to_owned()),
        Value::Null => None,
        Value::Object(object) => object
            .get("message")
            .and_then(|message| text_value(Some(message)))
            .or_else(|| {
                object
                    .get("detail")
                    .and_then(|detail| text_value(Some(detail)))
            })
            .or_else(|| Some(value.to_string())),
        value => Some(value.to_string()),
    })
}

fn item_text(item: &Value) -> Option<String> {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    if let Some(delta) = item.get("delta").and_then(Value::as_str) {
        return (!delta.is_empty()).then(|| delta.to_owned());
    }
    let summary = item.get("summary").and_then(Value::as_array)?;
    let text = summary
        .iter()
        .filter_map(|entry| {
            entry
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| entry.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn incremental_text(
    previous: &mut BTreeMap<String, String>,
    id: &str,
    text: String,
    is_delta: bool,
) -> Option<String> {
    if is_delta {
        previous
            .entry(id.to_owned())
            .and_modify(|current| current.push_str(&text))
            .or_insert_with(|| text.clone());
        return Some(text);
    }
    let current = previous.entry(id.to_owned()).or_default();
    if current == &text {
        return None;
    }
    let visible = text
        .strip_prefix(current.as_str())
        .map_or_else(|| text.clone(), str::to_owned);
    *current = text;
    (!visible.is_empty()).then_some(visible)
}

fn tool_title(item: &Value, kind: &str) -> String {
    item.get("command")
        .and_then(Value::as_str)
        .or_else(|| item.get("name").and_then(Value::as_str))
        .or_else(|| item.get("tool").and_then(Value::as_str))
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| kind.strip_suffix("_call").unwrap_or(kind).replace('_', " "))
}

fn tool_status(item: &Value, event_type: &str) -> ToolStatus {
    if item
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|exit_code| exit_code != 0)
    {
        return ToolStatus::Failed;
    }
    match item
        .get("status")
        .and_then(Value::as_str)
        .or(Some(event_type))
    {
        Some("completed") | Some("success") | Some("item.completed") => ToolStatus::Completed,
        Some("failed") | Some("error") | Some("errored") | Some("declined") | Some("cancelled")
        | Some("interrupted") | Some("item.failed") => ToolStatus::Failed,
        Some("in_progress") | Some("inProgress") | Some("running") | Some("item.started")
        | Some("item.updated") => ToolStatus::Running,
        _ => ToolStatus::Pending,
    }
}

fn parse_tool(
    slot: RosterSlot,
    event_type: &str,
    item: &Value,
    state: &mut ParserState,
) -> Option<AgentEvent> {
    let kind = item.get("type").and_then(Value::as_str)?;
    if matches!(kind, "agent_message" | "reasoning") {
        return None;
    }
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let title = tool_title(item, kind);
    let update = state
        .tools
        .entry(id.to_owned())
        .or_insert_with(|| ToolUpdate {
            id: id.to_owned(),
            title: title.clone(),
            status: ToolStatus::Pending,
            detail: None,
        });
    update.title = title;
    update.status = tool_status(item, event_type);
    for key in ["aggregated_output", "output", "result", "error", "detail"] {
        if let Some(detail) = text_value(item.get(key)) {
            update.detail = Some(detail);
            break;
        }
    }
    Some(AgentEvent::Tool {
        slot,
        update: update.clone(),
    })
}

/// Parse one Codex JSONL event into the common adapter event vocabulary.
/// Unknown events are ignored so newer Codex event kinds do not break turns.
fn parse_value(slot: RosterSlot, value: &Value, state: &mut ParserState) -> Option<AgentEvent> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(
        event_type,
        "item.started" | "item.updated" | "item.completed" | "item.failed"
    ) {
        let item = value.get("item")?;
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "agent_message" {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("agent-message");
            let text = item_text(item)?;
            let is_delta = item.get("delta").is_some() || value.get("delta").is_some();
            return incremental_text(&mut state.messages, id, text, is_delta)
                .map(|text| AgentEvent::Text { slot, text });
        }
        if kind == "reasoning" {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("reasoning");
            let text = item_text(item)?;
            let is_delta = item.get("delta").is_some() || value.get("delta").is_some();
            return incremental_text(&mut state.thoughts, id, text, is_delta)
                .map(|text| AgentEvent::Thought { slot, text });
        }
        return parse_tool(slot, event_type, item, state);
    }
    // Keep compatibility with a possible Responses-style top-level delta.
    if event_type.ends_with(".delta")
        && let Some(delta) = value.get("delta").and_then(Value::as_str)
        && !delta.is_empty()
    {
        return Some(AgentEvent::Text {
            slot,
            text: delta.to_owned(),
        });
    }
    None
}

fn failure_detail(value: &Value) -> Option<String> {
    ["error", "message", "detail", "reason"]
        .into_iter()
        .find_map(|key| text_value(value.get(key)))
        .or_else(|| {
            value.get("item").and_then(|item| {
                ["error", "message", "detail"]
                    .into_iter()
                    .find_map(|key| text_value(item.get(key)))
            })
        })
}

fn thread_id(value: &Value) -> Option<String> {
    value
        .get("thread_id")
        .or_else(|| value.get("threadId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn cached_models_at(path: &std::path::Path) -> Vec<Mode> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(cache) = serde_json::from_str::<Value>(&contents) else {
        return Vec::new();
    };
    let Some(models) = cache.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut catalog = Vec::new();
    for model in models {
        if model.get("visibility").and_then(Value::as_str) == Some("hide") {
            continue;
        }
        let Some(id) = model
            .get("slug")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if catalog.iter().any(|candidate: &Mode| candidate.id == id) {
            continue;
        }
        let label = model
            .get("display_name")
            .and_then(Value::as_str)
            .filter(|label| !label.is_empty())
            .unwrap_or(id);
        catalog.push(Mode {
            id: id.to_owned(),
            label: label.to_owned(),
        });
    }
    catalog
}

fn load_codex_models() -> Vec<Mode> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join(".codex"))
        });
    codex_home
        .map(|path| cached_models_at(&path.join("models_cache.json")))
        .unwrap_or_default()
}

/// Native process-per-turn Codex adapter.
#[derive(Debug)]
pub struct CodexAdapter {
    slot: RosterSlot,
    cwd: PathBuf,
    command: String,
    mode: String,
    model: Option<String>,
    models: Vec<Mode>,
    session_id: Option<String>,
    child: Option<Child>,
    sender: mpsc::Sender<AdapterResult<AgentEvent>>,
    receiver: mpsc::Receiver<AdapterResult<AgentEvent>>,
    announced_session: Arc<Mutex<Option<String>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl CodexAdapter {
    pub fn new(slot: RosterSlot, cwd: PathBuf, command: impl Into<String>) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        Self {
            slot,
            cwd,
            command: command.into(),
            mode: MODE_AUTO.into(),
            model: None,
            models: load_codex_models(),
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
                id: MODE_AUTO.into(),
                label: "Auto pilot".into(),
            },
            Mode {
                id: MODE_PLAN.into(),
                label: "Plan".into(),
            },
        ]
    }

    fn retain_selected_model(&mut self) {
        let Some(model) = self.model.as_ref() else {
            return;
        };
        if !self.models.iter().any(|candidate| candidate.id == *model) {
            self.models.push(Mode {
                id: model.clone(),
                label: model.clone(),
            });
        }
    }

    async fn emit(&self, event: AdapterResult<AgentEvent>) {
        let _ = self.sender.send(event).await;
    }

    fn append_mode_flags(&self, command: &mut Command, fresh: bool) {
        // `exec resume --help` does not expose --sandbox or --approve-for-me;
        // a resumed thread inherits its Codex policy. The bypass flag is
        // accepted by both forms and is the only deterministic Auto setting.
        if self.mode == MODE_AUTO {
            command.arg("--dangerously-bypass-approvals-and-sandbox");
        } else if self.mode == MODE_PLAN {
            if fresh {
                command.arg("--sandbox").arg("read-only");
            } else {
                // `exec resume` does not expose --sandbox, but its config
                // override remains available and applies to this turn.
                command.arg("-c").arg("sandbox_mode=\"read-only\"");
            }
        }
    }
}

#[async_trait]
impl AgentAdapter for CodexAdapter {
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
            supports_terminals: false,
            supports_session_load: true,
            supports_models: !self.models.is_empty(),
        }
    }

    async fn start(&mut self) -> AdapterResult<()> {
        if self.child.is_some() {
            self.stop().await?;
        }
        self.cancel_requested.store(false, Ordering::Release);
        let refreshed_models = load_codex_models();
        if !refreshed_models.is_empty() {
            self.models = refreshed_models;
        }
        self.retain_selected_model();
        self.emit(Ok(AgentEvent::ModesReplaced {
            slot: self.slot,
            modes: Self::modes(),
            current_mode: Some(self.mode.clone()),
        }))
        .await;
        if !self.models.is_empty() {
            self.emit(Ok(AgentEvent::ModelsReplaced {
                slot: self.slot,
                config_id: MODEL_CONFIG_ID.into(),
                models: self.models.clone(),
                current_model: self.model.clone(),
            }))
            .await;
        }
        self.emit(Ok(AgentEvent::Ready {
            slot: self.slot,
            capabilities: self.capabilities(),
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
        self.cancel_requested.store(false, Ordering::Release);
        let fresh = self.session_id.is_none();
        let (program, args) = parse_command_line(&self.command)
            .map_err(|error| AdapterError::Spawn(format!("invalid agent command: {error}")))?;
        let mut command = Command::new(program);
        isolate_process_group(&mut command);
        command.args(args).arg("exec");
        if !fresh {
            command.arg("resume");
        }
        command
            .arg("--json")
            // `exec --json` reports reasoning token usage but omits reasoning
            // items under Codex's default `none` summary policy. These
            // invocation-local overrides make the provider's own reasoning
            // summaries available to CodeSwarm's Thought event parser.
            .arg("-c")
            .arg("show_raw_agent_reasoning=true")
            .arg("-c")
            .arg("model_reasoning_summary=\"detailed\"");
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        self.append_mode_flags(&mut command, fresh);
        if !fresh && let Some(session_id) = &self.session_id {
            command.arg(session_id);
        }
        command
            .arg("-")
            .current_dir(&self.cwd)
            .env("CODESWARM_CWD", &self.cwd);
        let NativeTurn {
            child,
            stdout,
            stderr,
        } = spawn_native_turn(command, prompt).await?;
        let sender = self.sender.clone();
        let slot = self.slot;
        let announced_session = Arc::clone(&self.announced_session);
        let cancel_requested = Arc::clone(&self.cancel_requested);
        tokio::spawn(async move {
            let stderr_task = tokio::spawn(drain_bounded(stderr, 32 * 1024));
            let mut lines = BufReader::new(stdout).lines();
            let mut state = ParserState::default();
            let mut turn_completed = false;
            let mut failure = None;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let event_type = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if event_type == "thread.started"
                    && let Some(id) = thread_id(&value)
                    && let Ok(mut announced) = announced_session.lock()
                {
                    *announced = Some(id);
                }
                if event_type == "turn.completed" {
                    turn_completed = true;
                }
                if event_type == "turn.failed" || event_type == "error" {
                    failure = failure_detail(&value);
                }
                if let Some(event) = parse_value(slot, &value, &mut state)
                    && sender.send(Ok(event)).await.is_err()
                {
                    break;
                }
            }
            let stderr = stderr_task.await.ok().unwrap_or_default();
            if turn_completed || cancel_requested.load(Ordering::Acquire) {
                let _ = sender.send(Ok(AgentEvent::TurnComplete { slot })).await;
            } else {
                let detail = failure
                    .or_else(|| (!stderr.is_empty()).then_some(stderr))
                    .unwrap_or_else(|| "Codex stream ended before a successful turn".into());
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
        let mode = match mode.as_str() {
            "full-access" | "auto" | "autopilot" | MODE_AUTO => MODE_AUTO,
            "plan" | "readonly" | MODE_PLAN => MODE_PLAN,
            _ => return Err(AdapterError::Unsupported("requested Codex mode")),
        };
        self.mode = mode.into();
        self.emit(Ok(AgentEvent::ModesReplaced {
            slot: self.slot,
            modes: Self::modes(),
            current_mode: Some(self.mode.clone()),
        }))
        .await;
        Ok(())
    }

    async fn set_model(&mut self, model: String) -> AdapterResult<()> {
        let model = model.trim();
        if model.is_empty() {
            return Err(AdapterError::Protocol("model must not be empty".into()));
        }
        self.model = Some(model.to_owned());
        self.retain_selected_model();
        self.emit(Ok(AgentEvent::ModelsReplaced {
            slot: self.slot,
            config_id: MODEL_CONFIG_ID.into(),
            models: self.models.clone(),
            current_model: self.model.clone(),
        }))
        .await;
        Ok(())
    }

    async fn reload(&mut self) -> AdapterResult<()> {
        let session_id = self.session_id.clone();
        self.stop().await?;
        self.session_id = session_id;
        self.start().await
    }

    async fn stop(&mut self) -> AdapterResult<()> {
        let _ = self.cancel().await?;
        while self.receiver.try_recv().is_ok() {}
        Ok(())
    }

    async fn next_event(&mut self) -> Option<AdapterResult<AgentEvent>> {
        let event = self.receiver.recv().await;
        if matches!(
            event.as_ref(),
            Some(Ok(
                AgentEvent::TurnComplete { .. } | AgentEvent::Failed { .. }
            ))
        ) {
            if self.session_id.is_none()
                && let Ok(session) = self.announced_session.lock()
            {
                self.session_id = session.clone();
            }
            if let Some(mut child) = self.child.take() {
                let _ = child.wait().await;
            }
        }
        event
    }
}

#[cfg(test)]
mod tests {
    use super::{CodexAdapter, ParserState, cached_models_at, parse_value};
    use crate::{AgentAdapter, AgentEvent, ToolStatus};
    use serde_json::json;

    fn unique_test_path(stem: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{stem}-{}-{nonce}", std::process::id()))
    }

    async fn start_adapter(adapter: &mut CodexAdapter) {
        adapter.start().await.expect("start");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { modes, current_mode, .. }))
                if modes.len() == 2
                    && modes.iter().any(|mode| mode.label == "Auto pilot")
                    && modes.iter().any(|mode| mode.label == "Plan")
                    && current_mode.as_deref() == Some("codeswarm:mode:full-access")
        ));
        let event = adapter.next_event().await;
        if matches!(event, Some(Ok(AgentEvent::ModelsReplaced { .. }))) {
            assert!(matches!(
                adapter.next_event().await,
                Some(Ok(AgentEvent::Ready { .. }))
            ));
        } else {
            assert!(matches!(event, Some(Ok(AgentEvent::Ready { .. }))));
        }
    }

    #[test]
    fn reads_visible_models_from_codex_cache() {
        let cache_path = unique_test_path("codeswarm-codex-model-cache");
        std::fs::write(
            &cache_path,
            r#"{"models":[
                {"slug":"gpt-visible","display_name":"GPT Visible","visibility":"list"},
                {"slug":"gpt-hidden","display_name":"GPT Hidden","visibility":"hide"},
                {"slug":"gpt-fallback"},
                {"slug":"gpt-visible","display_name":"Duplicate"},
                {"display_name":"Missing slug"}
            ]}"#,
        )
        .expect("cache");
        let models = cached_models_at(&cache_path);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-visible");
        assert_eq!(models[0].label, "GPT Visible");
        assert_eq!(models[1].id, "gpt-fallback");
        assert_eq!(models[1].label, "gpt-fallback");
        std::fs::remove_file(cache_path).expect("cleanup");
    }

    #[tokio::test]
    async fn exposes_only_noninteractive_codex_modes() {
        let mut adapter = CodexAdapter::new(0, std::env::current_dir().expect("cwd"), "codex");
        start_adapter(&mut adapter).await;
        assert!(adapter.set_mode("manual".into()).await.is_err());
        assert!(adapter.set_mode("accept-edits".into()).await.is_err());
        adapter.set_mode("plan".into()).await.expect("plan mode");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { modes, current_mode, .. }))
                if modes.len() == 2
                    && current_mode.as_deref() == Some("codeswarm:mode:plan")
        ));
    }

    #[test]
    fn parses_codex_item_lifecycle_and_deduplicates_snapshots() {
        let mut state = ParserState::default();
        assert!(
            parse_value(
                1,
                &json!({"type":"thread.started","thread_id":"t1"}),
                &mut state
            )
            .is_none()
        );
        assert!(
            parse_value(
                1,
                &json!({"type":"item.started","item":{"id":"m1","type":"agent_message"}}),
                &mut state
            )
            .is_none()
        );
        assert!(matches!(
            parse_value(1, &json!({"type":"item.updated","item":{"id":"m1","type":"agent_message","text":"Hello"}}), &mut state),
            Some(AgentEvent::Text { slot: 1, text }) if text == "Hello"
        ));
        assert!(parse_value(1, &json!({"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"Hello"}}), &mut state).is_none());
        assert!(matches!(
            parse_value(1, &json!({"type":"item.completed","item":{"id":"r1","type":"reasoning","summary":[{"type":"summary_text","text":"Checked the patch"}]}}), &mut state),
            Some(AgentEvent::Thought { text, .. }) if text == "Checked the patch"
        ));
        assert!(matches!(
            parse_value(1, &json!({"type":"item.started","item":{"id":"c1","type":"command_execution","command":"cargo test","status":"in_progress"}}), &mut state),
            Some(AgentEvent::Tool { update, .. }) if update.status == ToolStatus::Running && update.title == "cargo test" && update.detail.is_none()
        ));
        assert!(matches!(
            parse_value(1, &json!({"type":"item.completed","item":{"id":"c1","type":"command_execution","status":"completed","aggregated_output":"ok"}}), &mut state),
            Some(AgentEvent::Tool { update, .. }) if update.status == ToolStatus::Completed && update.detail.as_deref() == Some("ok")
        ));
        assert!(matches!(
            parse_value(1, &json!({"type":"item.completed","item":{"id":"c2","type":"command_execution","exit_code":1,"aggregated_output":"command failed"}}), &mut state),
            Some(AgentEvent::Tool { update, .. }) if update.status == ToolStatus::Failed && update.detail.as_deref() == Some("command failed")
        ));
        assert!(matches!(
            parse_value(1, &json!({"type":"item.failed","item":{"id":"c3","type":"command_execution","status":"error","error":"spawn failed"}}), &mut state),
            Some(AgentEvent::Tool { update, .. }) if update.status == ToolStatus::Failed && update.detail.as_deref() == Some("spawn failed")
        ));
        assert!(matches!(
            parse_value(1, &json!({"type":"item.updated","item":{"id":"c4","type":"command_execution","status":"inProgress"}}), &mut state),
            Some(AgentEvent::Tool { update, .. }) if update.status == ToolStatus::Running
        ));
        assert!(parse_value(1, &json!({"type":"turn.completed"}), &mut state).is_none());
        assert!(
            parse_value(
                1,
                &json!({"type":"turn.failed","error":{"message":"rate limit"}}),
                &mut state
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn native_codex_process_captures_thread_and_resumes_it() {
        let args_path = unique_test_path("codeswarm-codex-args");
        let prompts_path = unique_test_path("codeswarm-codex-prompts");
        let script_path = unique_test_path("codeswarm-codex-script");
        let script = format!(
            r#"printf '%s\n' "$*" >> '{}'
cat >> '{}'
printf '%s\n' '{{"type":"thread.started","thread_id":"thread-native"}}' '{{"type":"item.completed","item":{{"id":"m1","type":"agent_message","text":"hello"}}}}' '{{"type":"turn.completed"}}'
"#,
            args_path.display(),
            prompts_path.display(),
        );
        std::fs::write(&script_path, script).expect("script");
        let cwd = std::env::current_dir().expect("cwd");
        let mut adapter = CodexAdapter::new(0, cwd, format!("sh {}", script_path.display()));
        start_adapter(&mut adapter).await;
        adapter
            .send_prompt("first".into())
            .await
            .expect("first prompt");
        assert!(
            matches!(adapter.next_event().await, Some(Ok(AgentEvent::Text { text, .. })) if text == "hello")
        );
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        assert_eq!(adapter.session_id(), Some("thread-native".into()));
        adapter.set_mode("plan".into()).await.expect("plan mode");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { current_mode: Some(mode), .. }))
                if mode == "codeswarm:mode:plan"
        ));
        adapter
            .send_prompt("follow-up".into())
            .await
            .expect("resume prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Text { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        let args = std::fs::read_to_string(&args_path).expect("captured arguments");
        assert!(
            args.lines()
                .any(|line| line.contains("exec --json") && line.ends_with(" -"))
                && args.lines().any(|line| line.contains("exec resume --json")
                    && line.contains("thread-native")
                    && line.ends_with(" -"))
                && args.lines().any(|line| {
                    line.contains("-c sandbox_mode=\"read-only\"")
                        && line.contains("exec resume --json")
                })
                && !args.contains("first")
                && !args.contains("follow-up"),
            "{args}"
        );
        assert_eq!(
            std::fs::read_to_string(&prompts_path).expect("captured prompts"),
            "firstfollow-up"
        );
        adapter.stop().await.expect("stop");
        std::fs::remove_file(args_path).expect("cleanup");
        std::fs::remove_file(prompts_path).expect("cleanup");
        std::fs::remove_file(script_path).expect("cleanup");
    }

    #[tokio::test]
    async fn native_codex_forwards_model_and_auto_approval_flags() {
        let args_path = unique_test_path("codeswarm-codex-model");
        let prompt_path = unique_test_path("codeswarm-codex-model-prompt");
        let script_path = unique_test_path("codeswarm-codex-model-script");
        let script = format!(
            r#"printf '%s\n' "$*" > '{}'
cat > '{}'
printf '%s\n' '{{"type":"thread.started","thread_id":"thread-model"}}' '{{"type":"turn.completed"}}'
"#,
            args_path.display(),
            prompt_path.display(),
        );
        std::fs::write(&script_path, script).expect("script");
        let mut adapter = CodexAdapter::new(
            0,
            std::env::current_dir().expect("cwd"),
            format!("sh {}", script_path.display()),
        );
        start_adapter(&mut adapter).await;
        adapter.set_model("gpt-test".into()).await.expect("model");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModelsReplaced { config_id, models, current_model, .. }))
                if config_id == "codex:model"
                    && models.iter().any(|model| model.id == "gpt-test")
                    && current_model.as_deref() == Some("gpt-test")
        ));
        let prompt = "task with\nmultiple lines\nand leading -flags";
        adapter.send_prompt(prompt.into()).await.expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        let args = std::fs::read_to_string(&args_path).expect("captured arguments");
        assert!(args.contains("--model gpt-test"), "{args}");
        assert!(args.contains("show_raw_agent_reasoning=true"), "{args}");
        assert!(
            args.contains("model_reasoning_summary=\"detailed\""),
            "{args}"
        );
        assert!(args.ends_with(" -\n"), "{args}");
        assert!(!args.contains("task with"), "{args}");
        assert!(
            args.contains("--dangerously-bypass-approvals-and-sandbox"),
            "{args}"
        );
        assert_eq!(
            std::fs::read_to_string(&prompt_path).expect("captured prompt"),
            prompt
        );
        adapter.stop().await.expect("stop");
        std::fs::remove_file(args_path).expect("cleanup");
        std::fs::remove_file(prompt_path).expect("cleanup");
        std::fs::remove_file(script_path).expect("cleanup");
    }

    #[tokio::test]
    async fn native_codex_surfaces_turn_failure_with_nested_message() {
        let script_path = unique_test_path("codeswarm-codex-failure-script");
        std::fs::write(
            &script_path,
            r#"printf '%s\n' '{"type":"turn.failed","error":{"message":"rate limit"}}'
"#,
        )
        .expect("script");
        let mut adapter = CodexAdapter::new(
            0,
            std::env::current_dir().expect("cwd"),
            format!("sh {}", script_path.display()),
        );
        start_adapter(&mut adapter).await;
        adapter.send_prompt("task".into()).await.expect("prompt");
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Failed { detail, started: true, .. })) if detail == "rate limit"
        ));
        adapter.stop().await.expect("stop");
        std::fs::remove_file(script_path).expect("cleanup");
    }

    #[tokio::test]
    async fn native_codex_cancellation_reaps_the_turn_process() {
        let mut adapter =
            CodexAdapter::new(0, std::env::current_dir().expect("cwd"), "sh -c 'sleep 10'");
        start_adapter(&mut adapter).await;
        adapter
            .send_prompt("long task".into())
            .await
            .expect("prompt");
        assert!(adapter.cancel().await.expect("cancel"));
        assert!(adapter.child.is_none());
    }
}
