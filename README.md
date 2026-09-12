# ✈ CodeSwarm

CodeSwarm is a fast terminal workspace for one or more coding agents. It is a
Rust application built around Ratatui, with ACP and native adapter support,
sequential relay turns, lazy transcript details, and a full-screen terminal
interface. It collects no telemetry.

`codeswarm-adapters` is also published as a reusable Rust library for
applications that need CodeSwarm's normalized agent events, ACP/native
adapters, and sequential relay host without the terminal UI.

## Install

Install the published binary with Cargo:

```bash
cargo install codeswarm --locked
```

Claude and Codex use their locally installed `claude` and `codex` CLIs directly;
CodeSwarm does not install or invoke an npm ACP bridge. Other built-in agents
use their own native or ACP CLI commands.

Or build the release binary from this repository:

```bash
cargo build --release -p codeswarm --locked
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/codeswarm "$HOME/.local/bin/codeswarm"
```

CodeSwarm supports macOS and Linux with a recent stable Rust toolchain.

## Run

```bash
codeswarm
codeswarm resume
```

Inside chat, `/sessions` browses saved project conversations and `/resume` opens
previous history without starting providers. Send a new message to reconnect
and continue. `/status` shows the running version, executable, roster and
connection state; `/summary` shows observed tool outcomes and the last response.
Pair review uses stable worker/reviewer roles within the existing pair mode.

The first launch opens agent selection. A saved roster is restored on later
launches. For a deterministic preview or smoke test:

```bash
codeswarm --demo
codeswarm --agy "describe the repository"
codeswarm --acp "codex-acp" "review the current changes"
codeswarm --project-dir ~/projects/example
# A directory may also be supplied positionally.
codeswarm ~/projects/example

codeswarm --help
```

Launch a mixed roster with repeated `--roster` arguments:

```bash
codeswarm --roster "acp:codex-acp" --roster "agy:agy" "review the patch"
```

Catalog agents can also be selected by name with repeated `-a`/`--agent`
options, for example `codeswarm -a claude -a codex "review the patch"`.

Claude and Codex use their locally installed `claude` and `codex` CLIs directly.
CodeSwarm does not invoke `npx` or contact an npm registry during startup.

Adapters are intentionally not forced through ACP. Native adapters and custom
ACP commands can coexist in one roster.

ACP agents can request workspace file reads/writes and client-mediated
terminals. CodeSwarm keeps those paths under the selected workspace, rejects
escapes and symlinks, and caps file/terminal output to protect tmux latency.

### Configure custom agents

The agent store reads `~/.config/codeswarm/codeswarm.json` (or
`$XDG_CONFIG_HOME/codeswarm/codeswarm.json`). Add an `agents` array or object;
entries replace built-ins with the same identity or add a new agent:

```json
{
  "agents": {
    "reviewer.local": {
      "name": "Local Reviewer",
      "short_name": "reviewer",
      "adapter": "acp",
      "command": "my-reviewer --acp",
      "active": true
    }
  }
}
```

Use `adapter: "native"` for a native command. Bare `codeswarm` displays these
entries in the store; `Space` adds or removes slots, `Alt+↑/↓` changes roster order,
`Ctrl+S` saves without launching, and `Enter` saves and launches the selection.
The store writes an ordered array of independent slots to `launcher.roster`
without overwriting other settings. Repeating an agent is supported, and each
slot may retain its own advertised model:

```json
{
  "launcher": {
    "roster": [
      { "agent": "anthropic.com", "model": "claude-opus-4-1" },
      { "agent": "anthropic.com", "model": "claude-sonnet-4-5" }
    ]
  }
}
```

Legacy newline-separated `launcher.roster` values are migrated when saved.

## Commands

Inside the conversation prompt:

- `/help` shows keyboard and command help.
- `/goal OBJECTIVE` sets a shared goal and starts work; `/goal` shows its status.
- `/settings` opens settings, including the
  slot-based roster editor (Space adds/removes a slot, ←/→ selects that
  running slot's model, Alt+↑/↓ reorders, Ctrl+S saves
  and applies idle-session changes when possible).
- `/export` writes the retained conversation to Markdown.
- `/cancel` cancels active work and reports when nothing is running.
- `/reload` cancels and restarts a silent agent, or retries the most recently
  crashed agent in its roster slot.
- `/agent SLOT` selects any active roster slot for the next message.
- Left-drag selects and copies visible text while transcript wheel scrolling
  remains active.
- `/clear` clears the local transcript; `/exit` exits the session.

Typing `/` opens a compact command palette with descriptions. The conversation
chrome stays fixed: a transient one-line system banner, an unlabeled composer,
a lower separator, and the footer. Readiness notices disappear after three
seconds; errors take priority and disappear after six. The footer timer starts
when a prompt is sent, including silent reasoning time. After two minutes
without activity, the ribbon shows the silent duration and cancel/reload
controls. Silence never cancels a turn automatically. ACP control requests
time out after 30 seconds; startup has a 90-second overall deadline and can
be interrupted.

Choose **Terminal**, **Light**, or **Dark** under `/settings` → Theme. Terminal
uses your terminal's canvas and ANSI accent colors. Light and Dark use explicit
palettes with readable text and status colors. `Ctrl+S` saves the choice;
`Esc` restores the previous theme.

`/settings` is also where live agent models are selected. Highlight a running
agent and use `←/→` to cycle the model catalog advertised by that agent, then
press `Ctrl+S`. Agents that do not advertise model configuration show no
synthetic choices. Claude exposes documented aliases and full model IDs;
Codex reads its locally cached model catalog when available.

Relay context is incremental. Each agent receives the new human prompt and
only public human/agent messages it has not seen since its previous turn. Tool
output, thoughts, terminal output, and the local UI transcript are never
replayed to peers. The roster introduction is sent once per adapter process;
replacement agents also receive the original shared task after old journal
entries have been pruned.

In **Roster** mode, agents can choose the next peer by ending their final
message with `[CODESWARM:NEXT:3]`, where `3` is the recipient's one-based roster
number. Each turn's prompt lists the current eligible recipients and their
markers, so duplicate agent names remain unambiguous. CodeSwarm hides markers
from the transcript and shared context. A handoff takes effect only when the
turn completes, after all message, thought, and tool activity. Invalid,
unavailable, or self-targets use normal roster order. Queued user input takes
priority, and automated turn limits and review-stop eligibility still apply.
Handoffs are disabled for private turns, Pair review, Manual, and solo sessions.

The interface keeps streamed output coalesced and transcript rows cached, so a
5,000-word response remains interactive in constrained terminals.

Prompt history is persisted locally and capped at the last 50 entries.

### Shared goals

`/goal Fix login while preserving existing sessions` starts a goal with the
selected agent. `/goal run` resumes an active goal, `/goal done` marks it
completed, and `/goal clear` removes it. Bare `/goal` shows the objective and
status in the status ribbon. An objective can contain multiple lines and may
be up to 16,000 bytes.

CodeSwarm includes the current goal as ordinary text in each agent's prompt,
alongside unseen public updates. No provider-specific goal API is needed.
Changes made during a turn apply at its boundary; they do not interrupt tools.
Goals retain normal permissions and relay limits. A completed agent turn or
review batch does not mark the goal completed: use `/goal done` explicitly.
Goals persist in session metadata and are restored by `codeswarm resume`;
ordinary launches start with no goal. Restoring a goal does not run it until
you send a prompt or use `/goal run`.

## Repeating requests

`/loop 5m Check the build status` runs immediately, then repeats the same
request every five minutes measured from the previous run's start. Bare minute
counts (`/loop 5 ...`) also work. Runs never overlap: if a job takes longer than
the interval, the next starts as soon as it finishes, without catch-up runs.
`/loop Check the build status` repeats immediately after each completed job.
For a roster, a job includes the entire peer-review batch.

`/loop` shows the active loop. `/loop stop` stops future runs and lets current
work finish; `/cancel` stops repetition and cancels current work. A new loop
replaces the previous schedule and waits for active work to finish. Each loop
keeps its original selected recipient. A new manual request, goal change,
settings save, reload, roster change, or agent failure stops repetition. Loops
are local to the live session and are not restored from history; connect an
agent with a normal prompt before starting a loop in an offline archive.

## Development

Cargo is the canonical build and test tool:

```bash
make verify
```

This runs formatting, workspace tests, Clippy, a locked release build, and
package archive validation.

## License

CodeSwarm is licensed under the
[MIT License](https://github.com/HainanZhao/codeswarm-rs/blob/main/LICENSE).
