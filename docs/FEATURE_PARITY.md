# Python-to-Rust feature parity matrix

The deleted Python client at `4e66ca9^` is the comparison baseline. This
matrix records behavior, not widget-level implementation details.

| Capability | Rust status | Evidence / next work |
| --- | --- | --- |
| Native and ACP adapters | Implemented | Shared `AgentAdapter`; native Agy and ACP adapters. Catalog entries preserve native `full_access_startup_argument` without forcing custom adapters through ACP. |
| ACP workspace file mediation | Implemented | `fs/read_text_file` and `fs/write_text_file` requests are root-bound, symlink-safe, and capped at 4 MiB. |
| Custom adapter commands | Implemented | JSON catalog plus shell-free quoted argv parsing. |
| Legacy CLI entry points | Implemented | `run`/`acp` aliases, `-h`/`--help`, `-v`/`--version`, and optional standalone prompts are accepted. |
| Named CLI agent selection | Implemented | Repeated `-a`/`--agent` options resolve catalog identities, aliases, and short names with one-based `--first-agent`. |
| Bare launch and saved roster | Implemented | Rust catalog/store and atomic `launcher.roster` persistence. |
| Agent store selection/order | Implemented | Ratatui store; Space, Ctrl+S, Alt+Up/Down, Enter. |
| Prompt editing/history/completion | Implemented | `tui-textarea`, bounded history, slash completion. |
| Cached transcript/long output | Implemented | Logical blocks, lazy details, viewport cache, deterministic benchmark. |
| Tool/terminal/thought lifecycle | Implemented | Normalized events, collapsed detail blocks, root-bound ACP filesystem mediation, and client-mediated terminal create/output/wait/kill/release. |
| Permission prompts | Implemented | Keyboard focus, readable option labels, stable ACP `optionId` routing, and cancellation. |
| Relay roster mode | Implemented | Sequential automatic ring with max-round safety. |
| Relay manual mode | Implemented | Explicitly targeted follow-ups; no implicit handoff. |
| Relay pair mode | Implemented | Owner/reviewer alternation. |
| Reviewer stop token | Implemented | Prompt guidance, filtering, and batch termination tests. |
| Resume/cancel/queue | Implemented | Project-session resume, turn cancellation, and queued prompts. |
| Mode policy synchronization | Implemented | Advertised catalogs drive the config picker, Auto pilot synchronizes once per loaded slot, and semantic selections translate through adapter-native IDs. |
| First-turn roster guidance | Implemented | Each relay agent receives a one-time identity/collaborator introduction; reloads receive it again. |
| Adapter crash attribution | Implemented | Native result failures, ACP transport errors, and relay EOFs tombstone their slot and emit a reloadable failure event; Unix children run in isolated process groups for descendant cleanup. |
| Crash tombstone/reload UX | Implemented | Core tombstones failed slots and exposes `/reload`; roster removal is available in `/settings`. |
| Project-directory selection | Implemented | Rust supports `--project-dir PATH`, positional paths, and `Ctrl+D` in the agent store before launch. |
| Prompt path/resource completion | Implemented (bounded) | Rust has a root-safe asynchronous workspace index, `.gitignore` filtering, Python's three-character threshold, fuzzy `@path` popup with directory/quoted insertion, keyboard/mouse dismissal, compact-pane rendering, ACP text/binary attachment expansion, fuzzy-match highlighting, and stale-generation protection. The picker is intentionally a lightweight Ratatui surface rather than a mounted Textual widget tree. |
| Live roster reconfiguration | Implemented (bounded) | The catalog-backed `/settings` editor adds, removes, and reorders roster slots at turn boundaries, supports repeated agents with per-slot models, and persists the roster for the next launch. `/agent SLOT` selects the next recipient. Runtime session snapshots are written off-thread, with resumability gated by adapter capability. |
| Persistent prompt/settings preferences | Implemented | Roster, custom agents, project-scoped prompt history, prompt placeholder, follow-tail, collapsed-details, notification policy, thoughts, tool-expansion, density, scrollbar, diff view, title-blink, and Terminal/Light/Dark theme preferences persist. Terminal focus remains transient runtime state. |
| Rich Markdown/diff views | Implemented (lightweight) | Tool payloads are retained for lazy expansion/export, ACP tool updates replace their original card, unified patches support inline or side-by-side views with line colors, and headings, fences, emphasis, lists, quotes, links, tables, rules, and source references are styled lazily. |
| OS notifications/sounds | Implemented (portable) | Rust preserves Python's `blur`/`always`/`never` policy (plus legacy boolean settings), emits completion and permission-request notifications through `notify-send`/`osascript`, supports a sanitized OSC terminal title with reference-counted blinking alerts, and persists the blink toggle. Permission requests use the sound toggle with a portable terminal BEL (the Python `question.wav` is 658 KB and requires `notifypy`/platform audio integration); turn-over notifications intentionally remain silent as in Python. |
| Session history browser | Not applicable | The Python baseline persisted session rows for adapter resume, but exposed no session browser, picker, CLI flag, or store action: `session_get_recent` is only exercised by persistence tests. Rust preserves the exposed behavior with event replay, capability-gated adapter session loading, and bounded project-scoped prompt history. |

The Rust client is functionally complete for the exposed full-screen relay and
standalone paths. Remaining differences are deliberate implementation choices:
the Rust renderer is a bounded Ratatui projection rather than a pixel-for-pixel
Textual widget tree, and it uses a portable terminal BEL instead of shipping a
platform-specific audio asset. These choices preserve the observable command,
adapter, persistence, and performance contracts while keeping the binary
lightweight.
