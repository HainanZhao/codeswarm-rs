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

`codeswarm resume [PATH]` opens the most recent local project archive without
starting providers. Inside chat, `/sessions` lists saved conversations with
their title, roster, last activity and response preview. Navigate with arrows
or the mouse wheel, press Enter to open, and Esc to return. `/resume` opens the
previous saved session. Switching is blocked during active work or queued input.

History is owned by CodeSwarm and remains readable if a provider is missing or
its session has expired. A new message reconnects the selected saved provider;
Ctrl+Enter continues privately. Older provider-only sessions may have no local
transcript until you continue and the provider replays it. A normal `codeswarm`
launch still starts a fresh provider session and a separate archive.

Archives live under `$XDG_STATE_HOME/codeswarm/archive` (normally
`~/.local/state/codeswarm/archive`) as per-session metadata and ordered events.
Writes are buffered away from rendering, checkpointed at turn boundaries and
periodically during work, and flushed on exit. No telemetry or automatic sharing
is added.

`/status` shows the running binary version/path, workspace, provider connection,
resumable slots, selected recipient, role/handoff context and current app state.
This identifies an old running process even after a newer binary is installed.
`/summary` shows the last agent response, actual observed tool outcomes and an
optional working-tree snapshot. Agent prose is not proof of passing tests, and
missing execution evidence is shown as unknown. Working-tree paths may include
pre-existing changes. Both views support scrolling and Esc; Ctrl+C retains its
normal cancel/exit behavior.

Pair review is the worker/reviewer workflow, not an additional routing mode.
The first agent responding to a human task is its worker; its partner reviews
concrete changes and hands defects back to the same worker. A new human task
can select a new worker. Only the reviewer may end a multi-agent pair batch;
worker changes return for review. Solo and manual routing are unchanged.

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
- Click 💭 for a thought or 🔧 for a tool to toggle its details. `Ctrl+O`
  toggles the focused detail. Collapsed
  thoughts show their newest two word-wrapped lines, scrolling up as each new
  line begins. Tools show a single-line tail preview. Both follow the agent's
  message and use faint gray italics. While thinking, finished tool summaries
  move out of the live preview and remain in `/summary`; running tools keep
  their output visible. Manually expanded details stay open.
- `Ctrl+Enter` sends to the selected roster agent.
- `Ctrl+C` cancels active work; while idle it exits.
- `Esc` closes the active picker, help, permission, or settings surface.

## Slash commands

- `/help` — show the complete keyboard and command guide.
- `/goal OBJECTIVE` — set a shared goal and start work. `/goal` shows its status;
  `/goal run` resumes an active goal, `/goal done` marks it complete, and
  `/goal clear` removes it. Goal changes apply at the next turn boundary.
- `/settings` — open settings, including the roster, modes, and theme.
- `/export` — write the retained transcript to a timestamped Markdown file.
- `/reload` — cancel and restart a silent adapter, or retry a crashed adapter
  in place. Two minutes without activity shows a warning in the status ribbon;
  silence never cancels a turn automatically.
- `/agent SLOT` — select an active agent by zero-based roster slot for the next message.
- `/select` — temporarily enable terminal text selection.
- `/clear` — clear the local transcript.
- `/cancel` — cancel the active turn when an adapter supports cancellation.
- `/exit` — leave the session.

Extra arguments to commands that take none show a usage hint.
Agent-advertised commands are sent to the agent; unrecognized
slash commands are reported locally.

Goals are sent as plain-text context to each roster agent, including adapters
without native goal support. They preserve permissions and the relay turn
limit. Mark completion explicitly with `/goal done`. Session resume restores
the goal without starting work automatically; a fresh launch has no goal.

The configuration panel includes a slot-based roster. The same catalog agent
can occupy multiple slots, and each running slot can select and persist a
different advertised model. Space adds/removes a slot, Left/Right changes its
model, and Alt+Up/Down changes slot order. The panel also includes
compact/comfortable density, thought visibility,
tool-detail expansion, diff view, and a notification policy. Ctrl+S saves the roster (and
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

## Thought and tool appearance

Thought and tool details follow their agent message. Clickable 💭/🔧 icons
identify them without text labels, with one space between each icon and its
text. Empty completed tool summaries are hidden; running and failed tools
remain visible.
Thoughts use a rolling
two-line preview: words fill a line, completed lines move up, and older lines
leave the preview. Paragraph breaks act as spaces in the preview, so they
cannot clear a visible line; expansion preserves the original formatting.
Tools keep a single-line tail preview. Both use faint gray
italics so they stay
visually secondary to messages. Their lower contrast is intentional in all
three themes; thought/tool details are exempt from the ordinary text contrast
target.

## Color roles

Teal is reserved for human messages and interface controls. Agent output uses
neutral text, bold headings, and underlined links, with faint italic thoughts.
Diff additions and deletions stay green and red; roster identities keep their
distinct non-teal colors. This Codex-inspired hierarchy applies to all themes.

## Privacy

CodeSwarm collects no telemetry. Prompts, responses, tool calls, and terminal
activity remain subject to the policies of the agent and provider you choose.

## Long-running turns

Native Antigravity turns allow up to 24 hours. ACP prompt turns have no
CodeSwarm wall-clock limit. The two-minute inactivity warning does not cancel
work; use Ctrl+C to cancel a running turn. Startup and control requests keep
their separate deadlines.
