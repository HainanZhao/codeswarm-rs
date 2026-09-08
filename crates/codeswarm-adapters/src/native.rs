//! Shared process plumbing for native, one-turn CLI adapters.
//!
//! Antigravity speaks a long-lived JSON protocol and owns its own stdin
//! framing. Claude and Codex both use a process per prompt, however, so they
//! share this boundary for process-group setup, piped prompt delivery, and
//! bounded stdio extraction. Keeping it here prevents their cancellation and
//! argument handling from drifting apart.

use std::process::Stdio;

use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStderr, ChildStdout, Command},
};

use super::{AdapterError, AdapterResult, terminate_child};

pub(super) struct NativeTurn {
    pub child: Child,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// Spawn a native one-turn process and deliver its full prompt over stdin.
///
/// Prompt text deliberately never becomes an argv element: large context can
/// exceed the platform's argument-size limit, and a prompt beginning with
/// `-` must not be parsed as a provider flag. Native CLIs close stdin after
/// the write, which tells print/exec modes that the prompt is complete.
pub(super) async fn spawn_native_turn(
    mut command: Command,
    prompt: String,
) -> AdapterResult<NativeTurn> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| AdapterError::Spawn(error.to_string()))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = terminate_child(&mut child).await;
            return Err(AdapterError::Transport("native agent has no stdout".into()));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = terminate_child(&mut child).await;
            return Err(AdapterError::Transport("native agent has no stderr".into()));
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = terminate_child(&mut child).await;
        return Err(AdapterError::Transport("native agent has no stdin".into()));
    };
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    Ok(NativeTurn {
        child,
        stdout,
        stderr,
    })
}
