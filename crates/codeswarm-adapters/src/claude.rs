//! Native adapter for Claude Code's print-mode JSONL stream.

use std::{
    collections::{BTreeMap, BTreeSet},
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

use super::native::{NativeTurn, spawn_native_turn};
use super::{
    AdapterError, AdapterResult, AgentAdapter, CANCEL_SETTLE_TIMEOUT, drain_bounded,
    isolate_process_group, parse_command_line, terminate_child,
};

const MODEL_CONFIG_ID: &str = "claude:model";

fn model_label(model: &str) -> String {
    match model {
        "default" => "Default".into(),
        "best" => "Best".into(),
        "fable" => "Fable".into(),
        "opus" => "Opus".into(),
        "sonnet" => "Sonnet".into(),
        "haiku" => "Haiku".into(),
        "sonnet[1m]" => "Sonnet (1M)".into(),
        "opus[1m]" => "Opus (1M)".into(),
        "opusplan" => "Opus plan".into(),
        _ => model.to_owned(),
    }
}

#[derive(Debug, Default)]
struct ParserState {
    tools: BTreeMap<String, ToolUpdate>,
    finished_tools: BTreeSet<String>,
    streamed_thoughts: BTreeMap<u64, String>,
}

fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => (!text.is_empty()).then(|| text.to_owned()),
        Value::Array(content) => {
            let text = content
                .iter()
                .filter_map(content_text)
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        Value::Object(content) => {
            let direct = [
                "text",
                "stdout",
                "stderr",
                "error",
                "error_code",
                "error_message",
                "message",
            ]
            .into_iter()
            .filter_map(|key| content.get(key).and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            (!direct.is_empty())
                .then_some(direct)
                .or_else(|| content.get("content").and_then(content_text))
        }
        _ => None,
    }
}

fn is_tool_use_type(kind: &str) -> bool {
    matches!(kind, "tool_use" | "server_tool_use" | "mcp_tool_use")
}

fn is_tool_result_type(kind: &str) -> bool {
    matches!(
        kind,
        "tool_result"
            | "tool_search_tool_result"
            | "web_fetch_tool_result"
            | "web_search_tool_result"
            | "code_execution_tool_result"
            | "bash_code_execution_tool_result"
            | "text_editor_code_execution_tool_result"
            | "mcp_tool_result"
    )
}

fn tool_title(block: &Value) -> String {
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Tool call")
        .replace('_', " ");
    let description = block
        .get("input")
        .and_then(|input| input.get("description"))
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty());
    description.map_or(name.clone(), |description| format!("{name}: {description}"))
}

fn parse_tool_uses(slot: RosterSlot, value: &Value, state: &mut ParserState) -> Vec<AgentEvent> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| {
            block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(is_tool_use_type)
        })
        .filter_map(|block| {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())?;
            let title = tool_title(block);
            if state.finished_tools.contains(id) {
                return None;
            }
            let update = state
                .tools
                .entry(id.to_owned())
                .or_insert_with(|| ToolUpdate {
                    id: id.to_owned(),
                    title: title.clone(),
                    status: ToolStatus::Running,
                    detail: None,
                });
            update.title = title;
            Some(AgentEvent::Tool {
                slot,
                update: update.clone(),
            })
        })
        .collect()
}

fn parse_stream_event(
    slot: RosterSlot,
    value: &Value,
    state: &mut ParserState,
) -> Option<AgentEvent> {
    if value.get("type").and_then(Value::as_str) != Some("stream_event") {
        return None;
    }
    let event = value.get("event")?;
    let event_type = event.get("type").and_then(Value::as_str)?;
    if event_type == "message_start" {
        state.streamed_thoughts.clear();
        return None;
    }
    let index = event.get("index").and_then(Value::as_u64);
    match event_type {
        "content_block_delta" => {
            let delta = event.get("delta")?;
            match delta.get("type").and_then(Value::as_str)? {
                "text_delta" => delta
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| AgentEvent::Text {
                        slot,
                        text: text.to_owned(),
                    }),
                "thinking_delta" => {
                    let text = delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())?;
                    state
                        .streamed_thoughts
                        .entry(index?)
                        .or_default()
                        .push_str(text);
                    Some(AgentEvent::Thought {
                        slot,
                        text: text.to_owned(),
                    })
                }
                _ => None,
            }
        }
        "content_block_start" => {
            let index = index?;
            let block = event.get("content_block")?;
            if !block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(is_tool_use_type)
            {
                return None;
            }
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map_or_else(|| format!("claude-tool-{index}"), str::to_owned);
            let title = tool_title(block);
            state.finished_tools.remove(&id);
            state.tools.insert(
                id.clone(),
                ToolUpdate {
                    id: id.clone(),
                    title: title.clone(),
                    status: ToolStatus::Running,
                    detail: None,
                },
            );
            Some(AgentEvent::Tool {
                slot,
                update: ToolUpdate {
                    id,
                    title,
                    status: ToolStatus::Running,
                    detail: None,
                },
            })
        }
        "content_block_stop" => {
            // This only closes Claude's streamed `tool_use` input block. The
            // tool is still executing; its later `tool_result` user message
            // carries the actual completion status and output.
            None
        }
        _ => None,
    }
}

fn parse_consolidated_thoughts(
    slot: RosterSlot,
    value: &Value,
    state: &mut ParserState,
) -> Vec<AgentEvent> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let events = content
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            if block.get("type").and_then(Value::as_str) != Some("thinking") {
                return None;
            }
            let text = block
                .get("thinking")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())?;
            let streamed = state
                .streamed_thoughts
                .get(&(index as u64))
                .map(String::as_str)
                .unwrap_or_default();
            let remainder = text.strip_prefix(streamed).unwrap_or(text);
            (!remainder.is_empty()).then(|| AgentEvent::Thought {
                slot,
                text: remainder.to_owned(),
            })
        })
        .collect();
    state.streamed_thoughts.clear();
    events
}

fn parse_tool_results(slot: RosterSlot, value: &Value, state: &mut ParserState) -> Vec<AgentEvent> {
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return Vec::new();
    }
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| {
            block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(is_tool_result_type)
        })
        .filter_map(|block| {
            let id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())?;
            let tool = state
                .tools
                .entry(id.to_owned())
                .or_insert_with(|| ToolUpdate {
                    id: id.to_owned(),
                    title: "Tool call".into(),
                    status: ToolStatus::Running,
                    detail: None,
                });
            let result_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let result = block.get("content");
            let structured_result_type = result
                .and_then(|result| result.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let nonzero_exit = result.is_some_and(|result| {
                ["return_code", "exit_code"]
                    .into_iter()
                    .filter_map(|key| result.get(key).and_then(Value::as_i64))
                    .any(|code| code != 0)
            });
            tool.status = if block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || result_type.ends_with("_error")
                || structured_result_type.ends_with("_error")
                || nonzero_exit
            {
                ToolStatus::Failed
            } else {
                ToolStatus::Completed
            };
            tool.detail = result.and_then(content_text);
            state.finished_tools.insert(id.to_owned());
            Some(AgentEvent::Tool {
                slot,
                update: tool.clone(),
            })
        })
        .collect()
}

fn parse_tool_progress(
    slot: RosterSlot,
    value: &Value,
    state: &mut ParserState,
) -> Option<AgentEvent> {
    if value.get("type").and_then(Value::as_str) != Some("tool_progress") {
        return None;
    }
    let reported = value.get("tool_use_id").and_then(Value::as_str);
    let parent = value.get("parent_tool_use_id").and_then(Value::as_str);
    let id = reported
        .filter(|id| state.tools.contains_key(*id) && !state.finished_tools.contains(*id))
        .or_else(|| {
            parent.filter(|id| state.tools.contains_key(*id) && !state.finished_tools.contains(*id))
        })?;
    let update = state.tools.get_mut(id)?;
    update.status = ToolStatus::Running;
    if let Some(seconds) = value.get("elapsed_time_seconds").and_then(Value::as_u64) {
        update.detail = Some(format!("running for {seconds}s"));
    }
    Some(AgentEvent::Tool {
        slot,
        update: update.clone(),
    })
}

fn result_text(value: &Value) -> Option<String> {
    value
        .get("result")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn session_id(value: &Value) -> Option<String> {
    value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

#[derive(Debug)]
pub struct ClaudeAdapter {
    slot: RosterSlot,
    cwd: PathBuf,
    command: String,
    mode: String,
    model: Option<String>,
    session_id: Option<String>,
    child: Option<Child>,
    sender: mpsc::Sender<AdapterResult<AgentEvent>>,
    receiver: mpsc::Receiver<AdapterResult<AgentEvent>>,
    announced_session: Arc<Mutex<Option<String>>>,
    cancel_requested: Arc<AtomicBool>,
}

impl ClaudeAdapter {
    pub fn new(slot: RosterSlot, cwd: PathBuf, command: impl Into<String>) -> Self {
        let (sender, receiver) = mpsc::channel(256);
        Self {
            slot,
            cwd,
            command: command.into(),
            mode: "bypassPermissions".into(),
            model: None,
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
        [("plan", "Plan"), ("bypassPermissions", "Full Access")]
            .into_iter()
            .map(|(id, label)| Mode {
                id: id.into(),
                label: label.into(),
            })
            .collect()
    }

    fn models(&self) -> Vec<Mode> {
        let ids = [
            "best",
            "opus",
            "sonnet",
            "haiku",
            "sonnet[1m]",
            "opus[1m]",
            "opusplan",
        ];
        let mut models = vec![Mode {
            id: "default".into(),
            label: "Default".into(),
        }];
        for id in ids.map(str::to_owned) {
            if !models.iter().any(|candidate| candidate.id == id) {
                models.push(Mode {
                    label: model_label(&id),
                    id,
                });
            }
        }
        if let Some(model) = &self.model
            && !models.iter().any(|candidate| candidate.id == *model)
        {
            // Claude Code has no model-discovery command. Keep its documented
            // aliases static, but retain an explicitly configured full model
            // ID instead of pretending the alias list is exhaustive.
            models.push(Mode {
                id: model.clone(),
                label: model.clone(),
            });
        }
        models
    }

    async fn emit(&self, event: AdapterResult<AgentEvent>) {
        let _ = self.sender.send(event).await;
    }
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn slot(&self) -> RosterSlot {
        self.slot
    }

    fn display_name(&self) -> String {
        "Claude".into()
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
            supports_models: true,
        }
    }

    async fn start(&mut self) -> AdapterResult<()> {
        self.cancel_requested.store(false, Ordering::Release);
        self.emit(Ok(AgentEvent::ModesReplaced {
            slot: self.slot,
            modes: Self::modes(),
            current_mode: Some(self.mode.clone()),
        }))
        .await;
        self.emit(Ok(AgentEvent::ModelsReplaced {
            slot: self.slot,
            config_id: MODEL_CONFIG_ID.into(),
            models: self.models(),
            current_model: self.model.clone(),
        }))
        .await;
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
        let (program, args) = parse_command_line(&self.command)
            .map_err(|error| AdapterError::Spawn(format!("invalid agent command: {error}")))?;
        let mut command = Command::new(program);
        isolate_process_group(&mut command);
        command
            .args(args)
            .arg("--print")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--include-partial-messages")
            .arg("--permission-prompts")
            .arg("none")
            .arg("--permission-mode")
            .arg(&self.mode)
            .current_dir(&self.cwd)
            .env("CODESWARM_CWD", &self.cwd);
        if self.mode == "bypassPermissions" {
            command.arg("--allow-dangerously-skip-permissions");
        }
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        if let Some(session_id) = &self.session_id {
            command.arg("--resume").arg(session_id);
        }
        let NativeTurn {
            child,
            stdout,
            stderr,
        } = spawn_native_turn(command, prompt).await?;
        let sender = self.sender.clone();
        let slot = self.slot;
        let announced = Arc::clone(&self.announced_session);
        let cancelled = Arc::clone(&self.cancel_requested);
        tokio::spawn(async move {
            let stderr_task = tokio::spawn(drain_bounded(stderr, 32 * 1024));
            let mut lines = BufReader::new(stdout).lines();
            let mut state = ParserState::default();
            let mut result = None;
            let mut streamed = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = session_id(&value)
                    && let Ok(mut current) = announced.lock()
                {
                    *current = Some(id);
                }
                if value.get("type").and_then(Value::as_str) == Some("result") {
                    result = Some(value.clone());
                }
                if let Some(event) = parse_stream_event(slot, &value, &mut state) {
                    streamed |= matches!(event, AgentEvent::Text { .. });
                    if sender.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
                for event in parse_consolidated_thoughts(slot, &value, &mut state) {
                    if sender.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
                for event in parse_tool_uses(slot, &value, &mut state) {
                    if sender.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
                if let Some(event) = parse_tool_progress(slot, &value, &mut state)
                    && sender.send(Ok(event)).await.is_err()
                {
                    break;
                }
                for event in parse_tool_results(slot, &value, &mut state) {
                    if sender.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
            }
            let stderr = stderr_task.await.ok().unwrap_or_default();
            let succeeded = cancelled.load(Ordering::Acquire)
                || result.as_ref().is_some_and(|value| {
                    value.get("subtype").and_then(Value::as_str) == Some("success")
                        && !value
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                });
            if succeeded {
                if !streamed && let Some(text) = result.as_ref().and_then(result_text) {
                    let _ = sender.send(Ok(AgentEvent::Text { slot, text })).await;
                }
                let _ = sender.send(Ok(AgentEvent::TurnComplete { slot })).await;
            } else {
                let detail = result
                    .as_ref()
                    .and_then(result_text)
                    .or_else(|| {
                        result
                            .as_ref()
                            .and_then(|value| value.get("subtype"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .or_else(|| (!stderr.is_empty()).then_some(stderr))
                    .unwrap_or_else(|| "Claude stream ended before a successful result".into());
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
        self.mode = match mode.as_str() {
            "codeswarm:mode:full-access"
            | "full-access"
            | "auto"
            | "autopilot"
            | "bypassPermissions" => "bypassPermissions",
            "codeswarm:mode:plan" | "readonly" | "plan" => "plan",
            _ => return Err(AdapterError::Unsupported("requested Claude mode")),
        }
        .into();
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
        let listed = self.models().iter().any(|candidate| candidate.id == model);
        let full_model_id = model.starts_with("claude-")
            && !model.contains(char::is_whitespace)
            && !model.contains('\0');
        if !listed && !full_model_id {
            return Err(AdapterError::Protocol(
                "model must be a listed Claude alias or a full claude-* model ID".into(),
            ));
        }
        self.model = Some(model.to_owned());
        self.emit(Ok(AgentEvent::ModelsReplaced {
            slot: self.slot,
            config_id: MODEL_CONFIG_ID.into(),
            models: self.models(),
            current_model: self.model.clone(),
        }))
        .await;
        Ok(())
    }

    async fn reload(&mut self) -> AdapterResult<()> {
        self.stop().await?;
        self.start().await
    }

    async fn reset_context(&mut self) -> AdapterResult<()> {
        self.session_id = None;
        if let Ok(mut announced) = self.announced_session.lock() {
            *announced = None;
        }
        self.stop().await?;
        self.session_id = None;
        self.start().await
    }

    async fn stop(&mut self) -> AdapterResult<()> {
        let _ = self.cancel().await?;
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
    use super::{
        ClaudeAdapter, ParserState, parse_consolidated_thoughts, parse_stream_event,
        parse_tool_progress, parse_tool_results, parse_tool_uses,
    };
    use crate::{AgentAdapter, AgentEvent, ToolStatus};
    use serde_json::json;

    #[test]
    fn parses_claude_text_thought_and_tool_events() {
        let mut state = ParserState::default();
        assert!(matches!(
            parse_stream_event(2, &json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}}), &mut state),
            Some(AgentEvent::Text { slot: 2, text }) if text == "hello"
        ));
        assert!(matches!(
            parse_stream_event(2, &json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"check"}}}), &mut state),
            Some(AgentEvent::Thought { text, .. }) if text == "check"
        ));
        assert!(matches!(
            parse_stream_event(2, &json!({"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool-1","name":"Read"}}}), &mut state),
            Some(AgentEvent::Tool { update, .. }) if update.status == ToolStatus::Running && update.title == "Read"
        ));
        assert_eq!(
            parse_stream_event(
                2,
                &json!({"type":"stream_event","event":{"type":"content_block_stop","index":1}}),
                &mut state
            ),
            None
        );

        let completed = parse_tool_results(
            2,
            &json!({
                "type": "user",
                "message": {
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "tool-1",
                        "content": [{"type": "text", "text": "file contents"}]
                    }]
                }
            }),
            &mut state,
        );
        assert!(matches!(
            completed.as_slice(),
            [AgentEvent::Tool { update, .. }]
                if update.status == ToolStatus::Completed
                    && update.title == "Read"
                    && update.detail.as_deref() == Some("file contents")
        ));
    }

    #[test]
    fn failed_claude_tool_result_preserves_error_detail() {
        let mut state = ParserState::default();
        let failed = parse_tool_results(
            4,
            &json!({
                "type": "user",
                "message": {
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "tool-without-partial-start",
                        "is_error": true,
                        "content": "permission denied"
                    }]
                }
            }),
            &mut state,
        );
        assert!(matches!(
            failed.as_slice(),
            [AgentEvent::Tool { slot: 4, update }]
                if update.status == ToolStatus::Failed
                    && update.id == "tool-without-partial-start"
                    && update.detail.as_deref() == Some("permission denied")
        ));
    }

    #[test]
    fn consolidated_tool_use_and_sdk_result_variants_match_claude_acp() {
        let mut state = ParserState::default();
        let refined = parse_tool_uses(
            1,
            &json!({
                "type": "assistant",
                "message": {"content": [{
                    "type": "server_tool_use",
                    "id": "server-tool",
                    "name": "bash_code_execution",
                    "input": {"description": "Compile the project"}
                }]}
            }),
            &mut state,
        );
        assert!(matches!(
            refined.as_slice(),
            [AgentEvent::Tool { update, .. }]
                if update.status == ToolStatus::Running
                    && update.title == "bash code execution: Compile the project"
        ));

        let completed = parse_tool_results(
            1,
            &json!({
                "type": "user",
                "message": {"content": [{
                    "type": "bash_code_execution_tool_result",
                    "tool_use_id": "server-tool",
                    "content": {
                        "type": "bash_code_execution_result",
                        "stdout": "partial output",
                        "stderr": "compiler error",
                        "return_code": 2
                    }
                }]}
            }),
            &mut state,
        );
        assert!(matches!(
            completed.as_slice(),
            [AgentEvent::Tool { update, .. }]
                if update.status == ToolStatus::Failed
                    && update.title == "bash code execution: Compile the project"
                    && update.detail.as_deref() == Some("partial output\ncompiler error")
        ));
        assert!(
            parse_tool_progress(
                1,
                &json!({
                    "type": "tool_progress",
                    "tool_use_id": "server-tool-heartbeat-1",
                    "parent_tool_use_id": "server-tool",
                    "tool_name": "bash_code_execution",
                    "elapsed_time_seconds": 30
                }),
                &mut state,
            )
            .is_none(),
            "a late heartbeat must not reopen a completed tool"
        );
    }

    #[test]
    fn tool_progress_uses_the_real_parent_id_without_creating_phantom_tools() {
        let mut state = ParserState::default();
        let _ = parse_stream_event(
            3,
            &json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "id": "tool-3", "name": "Bash"}
                }
            }),
            &mut state,
        );
        assert!(matches!(
            parse_tool_progress(
                3,
                &json!({
                    "type": "tool_progress",
                    "tool_use_id": "tool-3-heartbeat-1",
                    "parent_tool_use_id": "tool-3",
                    "tool_name": "Bash",
                    "elapsed_time_seconds": 30
                }),
                &mut state,
            ),
            Some(AgentEvent::Tool { update, .. })
                if update.id == "tool-3"
                    && update.status == ToolStatus::Running
                    && update.detail.as_deref() == Some("running for 30s")
        ));
        assert_eq!(state.tools.len(), 1);
    }

    #[test]
    fn structured_sdk_error_results_are_failed_with_their_details() {
        for (outer, inner) in [
            ("tool_search_tool_result", "tool_search_tool_result_error"),
            ("web_fetch_tool_result", "web_fetch_tool_result_error"),
            ("web_search_tool_result", "web_search_tool_result_error"),
            (
                "code_execution_tool_result",
                "code_execution_tool_result_error",
            ),
            (
                "bash_code_execution_tool_result",
                "bash_code_execution_tool_result_error",
            ),
            (
                "text_editor_code_execution_tool_result",
                "text_editor_code_execution_tool_result_error",
            ),
        ] {
            let mut state = ParserState::default();
            let events = parse_tool_results(
                5,
                &json!({
                    "type": "user",
                    "message": {"content": [{
                        "type": outer,
                        "tool_use_id": "failed-tool",
                        "content": {
                            "type": inner,
                            "error_code": "execution_failed",
                            "error_message": "provider rejected the tool"
                        }
                    }]}
                }),
                &mut state,
            );
            assert!(matches!(
                events.as_slice(),
                [AgentEvent::Tool { update, .. }]
                    if update.status == ToolStatus::Failed
                        && update.detail.as_deref()
                            == Some("execution_failed\nprovider rejected the tool")
            ));
        }
    }

    #[test]
    fn consolidated_thoughts_fill_missing_deltas_without_duplication() {
        let mut state = ParserState::default();
        assert!(
            parse_stream_event(
                0,
                &json!({"type":"stream_event","event":{"type":"message_start","message":{}}}),
                &mut state,
            )
            .is_none()
        );
        assert!(matches!(
            parse_stream_event(
                0,
                &json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"first "}}}),
                &mut state,
            ),
            Some(AgentEvent::Thought { text, .. }) if text == "first "
        ));
        let remainder = parse_consolidated_thoughts(
            0,
            &json!({
                "type": "assistant",
                "message": {"content": [{"type": "thinking", "thinking": "first second"}]}
            }),
            &mut state,
        );
        assert!(matches!(
            remainder.as_slice(),
            [AgentEvent::Thought { text, .. }] if text == "second"
        ));

        let fallback = parse_consolidated_thoughts(
            0,
            &json!({
                "type": "assistant",
                "message": {"content": [{"type": "thinking", "thinking": "gateway-only thought"}]}
            }),
            &mut state,
        );
        assert!(matches!(
            fallback.as_slice(),
            [AgentEvent::Thought { text, .. }] if text == "gateway-only thought"
        ));
    }

    #[tokio::test]
    async fn advertises_current_aliases_and_accepts_full_model_ids() {
        let mut adapter = ClaudeAdapter::new(0, std::env::current_dir().unwrap(), "claude");
        assert_eq!(
            ClaudeAdapter::modes()
                .into_iter()
                .map(|mode| mode.id)
                .collect::<Vec<_>>(),
            ["plan", "bypassPermissions"]
        );
        assert_eq!(
            adapter
                .models()
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
            [
                "default",
                "best",
                "opus",
                "sonnet",
                "haiku",
                "sonnet[1m]",
                "opus[1m]",
                "opusplan"
            ]
        );
        assert!(adapter.set_mode("manual".into()).await.is_err());
        adapter
            .set_model("claude-sonnet-4-5-20250929".into())
            .await
            .unwrap();
        assert!(
            adapter
                .models()
                .iter()
                .any(|model| model.id == "claude-sonnet-4-5-20250929")
        );
        assert!(
            adapter
                .set_model("provider/model:latest".into())
                .await
                .is_err()
        );
        assert!(adapter.set_model("   ".into()).await.is_err());
        assert!(adapter.set_model("bad\0model".into()).await.is_err());
    }

    #[tokio::test]
    async fn native_claude_process_captures_session_and_resumes() {
        let args_path =
            std::env::temp_dir().join(format!("codeswarm-claude-args-{}", std::process::id()));
        let stdin_path =
            std::env::temp_dir().join(format!("codeswarm-claude-stdin-{}", std::process::id()));
        let script_path =
            std::env::temp_dir().join(format!("codeswarm-claude-script-{}", std::process::id()));
        std::fs::write(
            &script_path,
            format!(
                "printf '%s\\n' \"$*\" >> '{}'\nsed -n 'p' >> '{}'\nprintf '\\n' >> '{}'\nprintf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"provider-runtime-model\",\"session_id\":\"session-native\"}}' '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"thinking\",\"thinking\":\"checked context\"}}]}}}}' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"hello\",\"session_id\":\"session-native\"}}'\n",
                args_path.display(),
                stdin_path.display(),
                stdin_path.display()
            ),
        )
        .unwrap();
        let mut adapter = ClaudeAdapter::new(
            0,
            std::env::current_dir().unwrap(),
            format!("sh {}", script_path.display()),
        );
        adapter.start().await.unwrap();
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModesReplaced { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModelsReplaced {
                current_model: None,
                ..
            }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        adapter.send_prompt("-first prompt".into()).await.unwrap();
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Thought { text, .. })) if text == "checked context"
        ));
        assert!(
            matches!(adapter.next_event().await, Some(Ok(AgentEvent::Text { text, .. })) if text == "hello")
        );
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        assert_eq!(adapter.session_id(), Some("session-native".into()));
        adapter.set_model("default".into()).await.unwrap();
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::ModelsReplaced { current_model, .. }))
                if current_model.as_deref() == Some("default")
        ));
        adapter.send_prompt("second prompt".into()).await.unwrap();
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Thought { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Text { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        let args = std::fs::read_to_string(&args_path).unwrap();
        let mut argument_lines = args.lines();
        let first_args = argument_lines.next().unwrap_or_default();
        let second_args = argument_lines.next().unwrap_or_default();
        assert!(!first_args.contains("--model"), "{args}");
        assert!(second_args.contains("--model default"), "{args}");
        assert!(second_args.contains("--resume session-native"), "{args}");
        assert!(!args.contains("first prompt"), "{args}");
        assert!(!args.contains("second prompt"), "{args}");
        assert_eq!(
            std::fs::read_to_string(&stdin_path).unwrap(),
            "-first prompt\nsecond prompt\n"
        );
        adapter.stop().await.unwrap();
        let _ = std::fs::remove_file(args_path);
        let _ = std::fs::remove_file(stdin_path);
        let _ = std::fs::remove_file(script_path);
    }

    #[tokio::test]
    async fn native_claude_reload_reaps_a_silent_turn() {
        let mut adapter =
            ClaudeAdapter::new(0, std::env::current_dir().unwrap(), "sh -c 'sleep 10'");
        adapter.start().await.unwrap();
        for _ in 0..3 {
            assert!(adapter.next_event().await.is_some());
        }
        adapter.send_prompt("stuck".into()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), adapter.reload())
            .await
            .expect("reload should not hang")
            .expect("reload should succeed");
        assert!(adapter.child.is_none());
        adapter.stop().await.unwrap();
    }
}
