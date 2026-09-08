use std::time::Duration;

use codeswarm_adapters::{AgentAdapter, AgentEvent, CodexAdapter};

async fn drain_startup(adapter: &mut CodexAdapter) {
    loop {
        let event = adapter
            .next_event()
            .await
            .expect("startup event")
            .expect("startup");
        eprintln!("STARTUP {event:?}");
        if matches!(event, AgentEvent::Ready { .. }) {
            break;
        }
    }
}

async fn run_prompt(adapter: &mut CodexAdapter, prompt: &str) -> Vec<AgentEvent> {
    adapter
        .send_prompt(prompt.to_owned())
        .await
        .expect("send prompt");
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(120), adapter.next_event())
            .await
            .expect("Codex event timeout")
            .expect("Codex stream ended")
            .expect("Codex adapter error");
        eprintln!("EVENT {event:?}");
        let terminal = matches!(
            event,
            AgentEvent::TurnComplete { .. } | AgentEvent::Failed { .. }
        );
        events.push(event);
        if terminal {
            return events;
        }
    }
}

#[tokio::test]
#[ignore = "uses the installed authenticated Codex CLI"]
async fn real_codex_adapter_trace() {
    let mut adapter = CodexAdapter::new(0, std::env::current_dir().unwrap(), "codex");
    adapter.start().await.expect("start adapter");
    drain_startup(&mut adapter).await;

    let greeting = run_prompt(
        &mut adapter,
        "Respond exactly READY. Do not call any tools.",
    )
    .await;
    assert!(
        !greeting
            .iter()
            .any(|event| matches!(event, AgentEvent::Tool { .. })),
        "a no-tool response produced a tool event"
    );
    assert!(matches!(
        greeting.last(),
        Some(AgentEvent::TurnComplete { .. })
    ));
    assert!(
        !greeting
            .iter()
            .any(|event| matches!(event, AgentEvent::Failed { .. }))
    );
    assert!(greeting.iter().any(|event| matches!(
        event,
        AgentEvent::Text { text, .. } if text == "READY"
    )));

    let command = run_prompt(
        &mut adapter,
        "Reason briefly about the current directory, use the shell tool to run pwd, then respond exactly DONE.",
    )
    .await;
    assert!(command.iter().any(|event| matches!(
        event,
        AgentEvent::Tool { update, .. }
            if update.title.contains("pwd")
                && update.status == codeswarm_adapters::ToolStatus::Running
                && update.detail.is_none()
    )));
    assert!(command.iter().any(|event| matches!(
        event,
        AgentEvent::Tool { update, .. }
            if update.title.contains("pwd")
                && update.status == codeswarm_adapters::ToolStatus::Completed
    )));
    assert!(matches!(
        command.last(),
        Some(AgentEvent::TurnComplete { .. })
    ));
    assert!(
        !command
            .iter()
            .any(|event| matches!(event, AgentEvent::Failed { .. }))
    );
    assert!(command.iter().any(|event| matches!(
        event,
        AgentEvent::Text { text, .. } if text == "DONE"
    )));
    adapter.stop().await.expect("stop adapter");
}
