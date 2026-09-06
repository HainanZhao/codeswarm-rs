# CodeSwarm daily-driver implementation plan

Date: 2026-09-06
Owner and reviewer: Codex
Delegated implementation: OpenCode, `opencode-go/glm-5.3-flash` (OpenCode Go)

## Product outcome

A user can start a coding task, understand which agent is working, leave, return
to readable saved history without starting agents, continue deliberately, and
inspect a concrete result summary. Preserve sequential relay, user cancellation,
provider permissions, no telemetry, and the current transcript design.

## Scope and acceptance criteria

1. **Reliable state and diagnostics.** Add `/status` with running version,
   executable path, workspace, roster state, selected agent, provider connection
   and resumability. Restoring history must stay idle. A running turn must have
   exactly one active recipient; permission/cancellation/disconnection remain
   distinguishable. Missing provider capabilities are ordinary external state.
2. **Session browser.** Add `/sessions` with multiple project sessions, titles,
   last activity, roster and a preview. Preserve existing last-session metadata
   as migration/fallback. New sessions must not overwrite archived conversations.
3. **History first.** CodeSwarm owns a local transcript archive. Opening a saved
   session displays history without launching adapters. Start/reconnect providers
   only when the user explicitly continues with a new prompt. Missing/expired
   provider handles must not prevent reading history.
4. **Understandable team workflow.** Make implementer/reviewer responsibilities
   and handoffs explicit for pair review; keep solo and manual routing semantics.
   Reviewer prompts request concrete defects or a concise approval, preserving
   reviewer-only stop-token eligibility and user instructions.
5. **Completion summary.** Provide `/summary` with the last response, actual tool
   outcomes and available working-tree changes. Never infer passing tests or
   successful execution from an agent's prose. Unavailable evidence is labelled
   unknown. Keep this available after restoring local history.

## Work packages and ownership

### A — session archive (OpenCode)

Own `crates/codeswarm-adapters/src/session_archive.rs` and its module export.
Build a reusable, local-only archive with immutable session IDs, per-session
metadata and append-only JSONL human/agent events. APIs for create, append,
update metadata, list by canonical project, and load; tolerate an incomplete
last journal line, skip/report malformed session metadata without losing other
sessions, validate IDs to prevent path traversal, and retain unknown metadata.
Use private/atomic metadata files. Keep streaming writes off the render thread
through a buffered archive handle. Cargo-native persistence tests required.

Data contract (agent may refine ergonomics, retain semantics):
- `ArchiveEntry`: id, cwd, title, created/updated timestamp, roster, state.
- `ArchiveEvent`: `Human { text, direct }` or `Agent(AgentEvent)`.
- `ArchivedSession`: entry, provider `SessionMetadata`, ordered events.
- `SessionArchive`: constructed from explicit root; no environment mutation.
- A buffered writer accepts events/metadata and flushes at boundaries/drop.

### B — browser and diagnostics UI (OpenCode)

Own `crates/codeswarm-tui/src/lib.rs` (and optional presentation modules).
Add local `/sessions`, `/status`, `/summary` actions to parser/help/completion.
Provide native Ratatui session browser and read-only diagnostics/summary panes,
keyboard navigation, escape, selection, and TestBackend regressions. These are
user-opened panels, not new notification overlays. Runtime feedback stays in
single-line ribbon. UI data types remain provider-independent and are supplied
by the CLI: `SessionListEntry { id, title, updated_at, roster, preview }`,
`open_sessions(entries)`, an action/request returning selected stable session ID,
`open_status(text)`, `open_summary(text)`. Add derived state descriptions without
rewriting relay scheduling. No filesystem or process I/O in rendering.

### C — workflow roles and completion model (OpenCode)

Own new `crates/codeswarm-adapters/src/workflow.rs`, focused relay prompt changes
in adapters.rs, and its module export. Implement pure role/handoff helpers and
completion accumulation from actual AgentEvent/ArchiveEvent-equivalent inputs.
Summary tracks actual tool IDs/status and response text, uses explicit evidence
labels, and supports optional caller-supplied changed paths. No filesystem access
or prompts to providers merely to summarize. Preserve public-context privacy,
solo/manual behavior and stop eligibility. Tests required.

### D — integration, review and fixes (Codex)

Integrate archive events and metadata with session lifecycle, connect UI actions,
implement offline history loading and deferred provider startup, wire diagnostics
and completion view, retain legacy resume, and resolve cross-package conflicts.
Review all delegated code for correctness, scope, privacy and performance.
Update manual and changelog. Do not publish automatically.

## Execution and checks

- Delegated work uses separate git worktrees and the exact requested model.
- OpenCode workers may inspect other modules but edit only their ownership area.
- Workers report commit IDs, tests, and limitations; Codex reviews before merge.
- Test fresh/resumed/missing-provider sessions, corrupt metadata/torn journals,
  explicit selection, no auto-prompt on history open, cancellation, and reordered
  adapter updates. Keep Unicode/long-transcript/mouse regressions intact.
- Run `make verify` and a mock-provider PTY flow: create two sessions, browse one
  offline, confirm zero provider starts, continue to the selected saved provider,
  and inspect summary/status. No paid/live provider tests besides authorized
  OpenCode implementation work.

## User steering during implementation

- Suppress routine `working` ribbon flashes: the footer already owns activity
  and timers. Keep the ribbon for actionable feedback and warnings.

- Snapshot the footer arrow recipient at message submission and keep that
  target through queuing/handoff, rather than falling back to a different agent.

- Worker/reviewer is the replacement behavior of existing Pair review, not a
  separate routing mode. Roles remain stable through review/fix cycles.
- While thoughts stream, hide completed collapsed tool summaries in the live
  transcript; keep them in `/summary`. Show running tool output and preserve
  manually expanded details.

## Progress

- [x] Confirm exact OpenCode Go model and existing authentication.
- [x] Write plan and delegation contracts.
- [x] A: archive implementation and review.
- [x] B: session/status/summary UI and review.
- [x] C: roles/summary model and review.
- [x] D: offline-resume/CLI integration and fixes.
- [x] Full verification, documentation, and review report.

Delivery and review details: [DAILY_DRIVER_REVIEW.md](DAILY_DRIVER_REVIEW.md).
