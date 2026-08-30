# ✈ CodeSwarm

CodeSwarm is a fast terminal workspace for one or more coding agents. It is a
Rust application built around Ratatui, with ACP and native adapter support,
sequential relay turns, lazy transcript details, and a full-screen terminal
interface. It collects no telemetry.

## Install

Build the release binary with Cargo:

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
entries in the store; `Space` selects them, `Alt+↑/↓` changes roster order,
`Ctrl+S` saves without launching, and `Enter` saves and launches the selection.
The store writes the selected identities back
to `launcher.roster` without overwriting other settings.

## Commands

Inside the conversation prompt:

- `/help` shows keyboard and command help.
- `/config` opens settings, including the
  catalog-backed roster editor (Enter toggles, Alt+↑/↓ reorders, Ctrl+S saves
  and applies idle-session changes when possible).
- `/agents` opens the roster section in settings without stopping the session.
- `/add AGENT`, `/add agy:COMMAND`, or `/add acp:COMMAND` starts a new peer in
  the live roster.
- `/export` writes the retained conversation to Markdown.
- `/diff split|unified` switches the lazy diff view.
- `/mode` and `/mode chat` select or show the current mode state.
- `/collab roster|manual|pair` selects collaboration routing.
- `/cancel` cancels active work and reports when nothing is running.
- `/reload` retries the most recently crashed agent in its roster slot.
- `/drop` removes the most recently crashed peer; `/drop SLOT` removes a
  peer by zero-based roster slot (the owner is protected).
- `/promote SLOT` transfers ownership to an active peer without restarting
  it; the former owner remains in its stable slot and is tombstoned.
- `/swap A B` reorders two active roster slots without restarting their
  adapters; queued work and response colors follow the agents.
- `/to SLOT` selects any active roster slot for the next message.
- `/clear` clears the local transcript; `/close` exits the session.

The interface keeps streamed output coalesced and transcript rows cached, so a
5,000-word response remains interactive in constrained terminals.

Prompt history is persisted locally and capped at the last 50 entries.

## Development

Cargo is the canonical build and test tool:

```bash
make verify
```

This runs formatting, workspace tests, Clippy, a locked release build, and
workspace package archive validation.

## License

CodeSwarm is licensed under
[AGPL-3.0](https://github.com/HainanZhao/codeswarm/blob/main/LICENSE). See the
[commercial license notice](https://github.com/HainanZhao/codeswarm/blob/main/COMMERCIAL_LICENSE.md)
for commercial licensing.
