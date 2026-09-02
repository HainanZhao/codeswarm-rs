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

The first launch opens agent selection. A saved roster is restored on later
launches. For a deterministic preview or smoke test:

```bash
codeswarm --demo
codeswarm --agy "describe the repository"
codeswarm --acp "codex-acp" "review the current changes"
codeswarm --project-dir ~/projects/example
# A directory may also be supplied positionally.
codeswarm ~/projects/example

# Legacy entry-point spellings remain accepted:
codeswarm run ~/projects/example
codeswarm acp "codex-acp" ~/projects/example
codeswarm --help
```

Launch a mixed roster with repeated `--roster` arguments:

```bash
codeswarm --roster "acp:codex-acp" --roster "agy:agy" "review the patch"
```

Catalog agents can also be selected by name with repeated `-a`/`--agent`
options, for example `codeswarm run -a claude -a codex "review the patch"`.

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
- `/config` opens settings, including the
  slot-based roster editor (Space adds/removes a slot, ←/→ selects that
  running slot's model, Alt+↑/↓ reorders, Ctrl+S saves
  and applies idle-session changes when possible).
- `/export` writes the retained conversation to Markdown.
- `/cancel` cancels active work and reports when nothing is running.
- `/reload` retries the most recently crashed agent in its roster slot.
- `/to SLOT` selects any active roster slot for the next message.
- `/clear` clears the local transcript; `/close` exits the session.

Typing `/` opens a compact command palette with descriptions. The conversation
chrome stays fixed: a transient one-line system banner, an unlabeled composer,
a lower separator, and the footer. Readiness notices disappear after three
seconds; errors take priority and disappear after six. The footer timer starts
as soon as an agent accepts a prompt, including silent reasoning time.

`/config` is also where live ACP models are selected. Highlight a running
agent and use `←/→` to cycle the model catalog advertised by that agent, then
press `Ctrl+S`. Agents that do not advertise model configuration show no
synthetic choices.

Relay context is incremental. Each agent receives the new human prompt and
only public human/agent messages it has not seen since its previous turn. Tool
output, thoughts, terminal output, and the local UI transcript are never
replayed to peers. The roster introduction is sent once per adapter process.

The interface keeps streamed output coalesced and transcript rows cached, so a
5,000-word response remains interactive in constrained terminals.

Prompt history is persisted locally and capped at the last 50 entries.

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
