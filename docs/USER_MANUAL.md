# CodeSwarm User Manual

CodeSwarm is a Rust terminal workspace for collaborating with coding agents.
It uses the complete terminal in a guarded alternate screen.

## Quick start

```bash
cargo build --release -p codeswarm
./target/release/codeswarm
```

Select agents with the arrow keys and `Space`, then press `Enter`. The selected
roster is saved and restored by the next bare launch. Use `--demo` to exercise
the UI without an external agent.

Use `--project-dir PATH` (or pass a directory positionally) to start the
session in a different workspace.

`codeswarm run PATH` and `codeswarm acp COMMAND [PATH]` remain accepted for
compatibility with the previous launcher. `codeswarm --help` and
`codeswarm --version` work before the terminal UI starts.

`codeswarm resume [PATH]` restores the saved roster and provider conversation
for the project when the previous owner supports session loading. A normal
`codeswarm` launch always starts a fresh provider session.

The store reads custom agents from
`$XDG_CONFIG_HOME/codeswarm/codeswarm.json` (default:
`~/.config/codeswarm/codeswarm.json`). Add an `agents` array or object with
`identity`, `name`, `short_name`, `adapter` (`native` or `acp`), and `command`.
Entries may override a built-in identity or add a new one. In the store,
`Space` toggles membership, `Alt+Up`/`Alt+Down` changes order, `Ctrl+S` saves
without launching, and `Enter` saves and launches the selected roster (or the
highlighted agent when none is selected).

## Agent adapters

CodeSwarm supports both native adapters and ACP adapters. They share the same
normalized event model, but a custom adapter does not need to implement ACP.

```bash
codeswarm --agy "summarize the repository"
codeswarm --acp "codex-acp" "review the patch"
codeswarm --roster "agy:agy" --roster "acp:codex-acp" "review the patch"
```

Agents in a roster run sequentially. Each agent receives public human and
agent messages it has not seen since its previous turn. Tool calls, thoughts,
terminal output, and UI history stay local to the producing agent.

ACP workspace file and terminal requests are mediated by CodeSwarm. File paths
are confined to the selected workspace, and file and terminal output are
bounded to preserve tmux responsiveness.

## Prompt and keyboard controls

- `Enter` submits a prompt; `Shift+Enter` inserts a newline.
- `Up`/`Down` scroll the transcript; `End` follows the live tail.
- On an empty single-line prompt, `Up`/`Down` browse the last 50 persisted prompts.
- `Tab` completes a slash command; `F1` or `?` toggles help.
- `Tab` also completes bounded workspace paths after `@`.
- `Ctrl+O` expands or collapses the latest tool or thought detail. Collapsed
  details keep a one-line preview of their first content line.
- `Ctrl+Enter` sends to the selected roster agent.
- `Ctrl+C` cancels active work; while idle it exits.
- `Esc` closes the active picker, help, permission, or settings surface.

## Slash commands

- `/help` — show the complete keyboard and command guide.
- `/config` — open settings.
- `/agents` — open roster settings without stopping the session.
- `/add AGENT`, `/add agy:COMMAND`, or `/add acp:COMMAND` — append a live peer
  without restarting the current conversation.
- `/export` — write the retained transcript to a timestamped Markdown file.
- `/diff split` and `/diff unified` — choose side-by-side or inline diff rows.
- `/mode` — focus mode settings; `/mode chat` selects chat mode.
- `/collab` — focus collaboration settings;
  `/collab roster`, `/collab manual`, and `/collab pair` select a strategy.
- `/reload` — retry the most recently crashed adapter in place.
- `/drop` — remove the most recently crashed peer; `/drop SLOT` removes a
  peer by zero-based roster slot (slot 0 is the owner).
- `/promote SLOT` — transfer ownership to an active peer without restarting
  it; the former owner remains in its stable slot and is tombstoned.
- `/swap A B` — reorder two active roster slots without restarting their
  adapters; queued work follows the agents.
- `/to SLOT` — select any active roster slot for the next message.
- `/clear` — clear the local transcript.
- `/cancel` — cancel the active turn when an adapter supports cancellation.
- `/close`, `/quit`, and `/exit` — leave the session.

Unknown slash commands are reported locally and are never sent to an agent.

The configuration panel includes the catalog-backed roster,
compact/comfortable density, normal/hidden scrollbar, thought visibility,
tool-detail expansion, diff view, and a notification policy. Enter toggles a
catalog agent, Alt+↑/↓ changes its order, and Ctrl+S saves the roster (and
applies idle-session changes when possible). On Linux
notifications use `notify-send`; on
macOS they use `osascript` when the corresponding system tool is available.
The notification policy and sound toggle are independent. Notifications can be
set to `Never`, `When unfocused`, or `Always`; the default is `When unfocused`.
Completion and permission-request notifications are emitted according to that
policy, while the terminal bell is additionally controlled by the sound
toggle. The panel also controls terminal-title blinking for unattended
permission prompts. Terminals that do not report focus changes keep the app in
the safe focused state.

## Performance model

Transcript blocks are retained as source text and rendered into a cached row
viewport. Streamed chunks extend one logical block, and tool/thought details
start collapsed. Scrolling therefore touches only the visible slice rather than
reparsing the full conversation.

## Development and verification

Cargo is the canonical build and test tool:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
make verify
```

Renderer behavior is covered with deterministic Ratatui backends and bounded
viewport tests.

## Privacy

CodeSwarm collects no telemetry. Prompts, responses, tool calls, and terminal
activity remain subject to the policies of the agent and provider you choose.
