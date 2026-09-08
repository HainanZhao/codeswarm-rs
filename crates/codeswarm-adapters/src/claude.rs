//! Native adapter for Claude Code's print-mode JSONL stream.

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

use super::native::{NativeTurn, spawn_native_turn};
use super::{
    AdapterError, AdapterResult, AgentAdapter, CANCEL_SETTLE_TIMEOUT, drain_bounded,
    isolate_process_group, parse_command_line, terminate_child,
};

#[derive(Debug, Default)]
struct ParserState {
    tools: BTreeMap<String, ToolUpdate>,
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
        Value::Object(content) => content
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .or_else(|| content.get("content").and_then(content_text)),
        _ => None,
    }
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
    let index = event.get("index").and_then(Value::as_u64);
    match event.get("type").and_then(Value::as_str)? {
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
                "thinking_delta" => delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| AgentEvent::Thought {
                        slot,
                        text: text.to_owned(),
                    }),
                _ => None,
            }
        }
        "content_block_start" => {
            let index = index?;
            let block = event.get("content_block")?;
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                return None;
            }
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map_or_else(|| format!("claude-tool-{index}"), str::to_owned);
            let title = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Tool call")
                .replace('_', " ");
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
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
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
            tool.status = if block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                ToolStatus::Failed
            } else {
                ToolStatus::Completed
            };
            tool.detail = block.get("content").and_then(content_text);
            Some(AgentEvent::Tool {
                slot,
                update: tool.clone(),
            })
        })
        .collect()
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
        let mut models = [
            ("fable", "Fable"),
            ("opus", "Opus"),
            ("sonnet", "Sonnet"),
            ("haiku", "Haiku"),
        ]
        .into_iter()
        .map(|(id, label)| Mode {
            id: id.into(),
            label: label.into(),
        })
        .collect::<Vec<_>>();
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
            config_id: "claude:model".into(),
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
        let documented_alias = self.models().iter().any(|candidate| candidate.id == model);
        let full_model_id = model.strip_prefix("claude-").is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        });
        if !documented_alias && !full_model_id {
            return Err(AdapterError::Protocol(
                "model must be a documented Claude Code alias or a full claude-* model ID".into(),
            ));
        }
        self.model = Some(model.clone());
        self.emit(Ok(AgentEvent::ModelUpdated {
            slot: self.slot,
            current_model: model,
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
    use super::{ClaudeAdapter, ParserState, parse_stream_event, parse_tool_results};
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
            parse_stream_event(2, &json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"check"}}}), &mut state),
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

    #[tokio::test]
    async fn advertises_only_noninteractive_modes_and_accepts_full_model_ids() {
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
            ["fable", "opus", "sonnet", "haiku"]
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
        assert!(adapter.set_model("made-up-alias".into()).await.is_err());
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
                "printf '%s\\n' \"$*\" >> '{}'\nsed -n 'p' >> '{}'\nprintf '\\n' >> '{}'\nprintf '%s\\n' '{{\"type\":\"system\",\"session_id\":\"session-native\"}}' '{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"hello\",\"session_id\":\"session-native\"}}'\n",
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
            Some(Ok(AgentEvent::ModelsReplaced { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Ready { .. }))
        ));
        adapter.send_prompt("-first prompt".into()).await.unwrap();
        assert!(
            matches!(adapter.next_event().await, Some(Ok(AgentEvent::Text { text, .. })) if text == "hello")
        );
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        assert_eq!(adapter.session_id(), Some("session-native".into()));
        adapter.send_prompt("second prompt".into()).await.unwrap();
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::Text { .. }))
        ));
        assert!(matches!(
            adapter.next_event().await,
            Some(Ok(AgentEvent::TurnComplete { .. }))
        ));
        let args = std::fs::read_to_string(&args_path).unwrap();
        assert!(args.contains("--resume session-native"), "{args}");
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
}
