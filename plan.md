# CodeSwarm Rust Terminal Rewrite — Coding Plan

**Goal:** Deliver a tmux-first Rust terminal
application that stays responsive while streaming and while scrolling a
5,000-word reply. Preserve CodeSwarm's product behavior: ACP support, native
non-ACP adapters, sequential multi-agent relay, session recovery, and the
`codeswarm` package/executable identity.

**Non-goal:** Pixel-for-pixel parity with the retired UI. The transcript is a fast,
compact terminal surface, not a permanently mounted Markdown/widget tree.

See [FEATURE_PARITY.md](FEATURE_PARITY.md) for the current behavior matrix and
the remaining parity work.

## Operating Rules

- Use one guarded full-screen alternate-screen experience.
- The scroll hot path may read cached rows and draw the viewport; it may not
  parse Markdown, rewrap historical text, rebuild a transcript tree, inspect
  SQLite, or await an adapter.
- The core owns domain state. Renderers and adapters are replaceable clients of
  that core; ACP is an adapter, not the domain model.
- Every adapter reports capabilities. A missing, reordered, or replaced mode
  catalog must be handled as normal external state.
- Preserve the current documented relay contracts in `AGENTS.md` and prove
  them with translated tests before changing behavior intentionally.
- Rich UI is welcome only if it is lazy and has a measured cost. No animation
  or per-token redraw is allowed on the default path.

## Deliverable Layout

Create a Cargo workspace exposing the single `codeswarm` binary; do not add a
compatibility executable or interpreter launcher.

```text
Cargo.toml
crates/
  codeswarm-core/       # events, reducer, relay, persistence interfaces
  codeswarm-adapters/   # AgentAdapter trait, ACP and native implementations
  codeswarm-transcript/ # immutable blocks, wrapping cache, viewport index
  codeswarm-tui/        # Ratatui/Crossterm full-screen UI
  codeswarm-cli/        # `codeswarm` command, config and migration wiring
crate-local tests/      # behavior, adapter-contract, transcript, renderer tests
```

Cargo-native unit/integration tests and tmux black-box checks are the behavior
oracle.

## Implementation Status — 2026-08-29

Completed on branch \`rewrite/rust-ratatui-architecture\`:

- [x] Created the Rust Cargo workspace and the \`codeswarm-core\`,
  \`codeswarm-adapters\`, \`codeswarm-transcript\`, \`codeswarm-tui\`, and
  \`codeswarm\` packages.
- [x] Added deterministic 5,000-word and 100-turn transcript fixtures, a
  benchmark binary, and a cached viewport transcript renderer.
- [x] Added a 5,000-word cached-scroll regression budget (<100ms), bounded
  visible rows, and stream-chunk coalescing into one logical transcript block.
- [x] Added framework-independent normalized events, a replayable JSONL event
  log, a sequential relay scheduler, shared permission-policy resolution, and
  bounded per-agent public-context watermarks.
- [x] Added ACP stdio initialization/session creation/prompt lifecycle and a
  native Agy stream-JSON adapter under one \`AgentAdapter\` contract.
- [x] Added a full-screen Ratatui terminal,
  live adapter event rendering, follow-up prompt dispatch, cancellation, and
  durable user-local event logging.
- [x] Added \`AdapterHost\`, which consumes ACP/native adapter events, reduces
  core session state, persists normalized events, and exposes reducer effects
  without coupling the UI to process I/O.
- [x] Added a latest-state frame scheduler that drops stale terminal deltas
  after backpressure and requires a complete repaint before deltas resume.
- [x] Added deterministic trailing-edge resize coalescing so only final pane
  geometry is applied after a resize burst.
- [x] Added \`AdapterHost\` reload, mode forwarding, and failure tombstoning
  while preserving the stable roster slot and event log.
- [x] Normalized ACP permission requests, including stable tool IDs, titles,
  and selectable option labels.
- [x] ACP adapter loads an existing session when the agent advertises
  \`loadSession\`, and rejects unsupported session restoration explicitly.
- [x] Added replay-safe lazy detail records for tool, thought, terminal, and
  diff content, including stable replacement and expansion state.
- [x] Added focused permission UI state with safe empty-option handling,
  selection, confirmation/cancellation actions, and rendering tests.
- [x] Wired permission answers through the adapter host and relay: ACP
  request IDs and JSON-RPC responses are preserved; native adapters explicitly
  report unsupported permission control.
- [x] Added lazy collapsed tool/thought/terminal details with explicit
  keyboard expansion, keeping expensive detail off the scroll path.
- [x] Core reducer tracks active-turn boundaries across text, thought, tool,
  permission, and terminal events for deterministic relay handoff.
- [x] Added \`RelayHost\` orchestration over live adapter hosts, including
  sequential dispatch, pause/collapse handling, public-context routing, and
  dispatch-history tests.
- [x] Added saved-roster launcher decisions that filter stale identities,
  preserve order, and open the agent store when no saved roster resolves.
- [x] Added versioned, non-destructive event-log and legacy session-metadata
  migration/import with malformed and future-version rejection.
- [x] Added repeated mixed roster CLI parsing with native/ACP startup,
  selected-first routing, max-round control, direct prompts, and live
  normalized event streaming.
- [x] Wired saved-roster launch decisions into the Rust launcher while
  preserving explicit demo, ACP, native, and repeated-roster flags.
- [x] Added adapter-contract normalization and equivalent ACP/native trace
  fixtures covering omitted, malformed, reordered, and replaced external state.
- [x] Added deterministic Ratatui and transcript coverage for 5k-word scroll,
  prompt input, resize settling, and viewport behavior.
- [x] Coalesced streamed thought chunks into one per-turn collapsed detail,
  preventing token-level reasoning output from creating a new line/card for
  every stream event.
- [x] Enhanced the production HUD with typed transcript colors,
  conversation/prompt framing, and status-state colors while keeping styling
  on the cached viewport path.
- [x] Replaced the display-only prompt with a Ratatui/tui-textarea editor:
  multiline Unicode-safe editing, bounded history, slash completion, local
  commands, and readable command/config/help feedback.
- [x] Matched the previous client’s core conversation cues in Rust: visible
  human turns, named agent response starts, per-block markers, identity-aware
  status, and robust narrow-pane fallback.
- [x] Added terminal queue/help interaction state: queued and direct prompts,
  target selection, cancellation, follow-tail behavior, and keyboard
  help rendering.
- [x] Added normalized terminal lifecycle parsing for ACP/native events and a
  deterministic replay/trace comparison command for cross-protocol fixtures.
- [x] Enforced reviewer-only stop-token stripping and acknowledgment behavior
  at relay-host handoff, so internal control tokens cannot leak into UI or
  public context.
- [x] Restored Python-compatible prompt-history records and atomic user settings
  updates, including malformed-input preservation and file-mode safety.
- [x] Added legacy `run`/`acp` entry-point aliases, `--help`, `--version`,
  optional standalone prompts, bounded ACP framing, and bounded stderr.
- [x] Added root-bound ACP filesystem mediation and asynchronous client-side
  terminal create/output/wait/kill/release handling.
- [x] Added per-roster first-turn identity/collaborator introductions and
  reload reintroduction, plus explicit relay failure events for recovery UX.
- [x] Restored Python-compatible named `-a`/`--agent` launch selection and
  one-based `--first-agent` routing.
- [x] Preserved catalog-declared native startup access arguments (including
  Antigravity's `--dangerously-skip-permissions`) while leaving custom adapter
  commands opt-in.
- [x] Added persistent density and tool-expansion controls, with isolated
  tmux performance state so prompt history cannot steal scroll input.
- [x] Isolated Unix agent/terminal process groups and terminated/reaped the
  group on cancellation and shutdown to avoid descendant leaks.
- [x] Added a bounded, root/symlink-safe Rust workspace resource loader as the
  foundation for prompt attachments.
- [x] Expanded ACP `@path` references into bounded text/binary resource blocks
  before prompt dispatch, with prompt-level regression coverage.
- [x] Keep healthy roster peers usable after a failed/dropped adapter by
  allowing single-slot continuation and retargeting untagged prompts.
- [x] Add live `/add` and `/drop SLOT` controls over the relay coordinator,
  with startup rollback and stable slot identity.
- [x] Add live `/promote SLOT` owner transfer and `/swap A B` roster
  reordering, preserving queued targets, context watermarks, adapter event
  slots, and visible agent identity.
- [x] Resolve live `/add` arguments through the configured catalog as well as
  explicit native/ACP command specifications; failed optimistic UI additions
  are rolled back when coordinator startup fails.
- [x] Restore Python-compatible notification policy (`blur`, `always`,
  `never`) for completion and permission events, with independent sound
  control and persisted settings.
- [x] Restore title-blink preference and reference-counted, sanitized terminal
  alerts for unattended permission prompts.
- [x] Restore the Python path-completion threshold and lightweight source-file
  reference styling without putting repository indexing on the scroll path.
- [x] Add a bounded asynchronous workspace path index and picker with
  `.gitignore` filtering, quoted/directory insertion, and stale-generation
  protection across workspace changes.
- [x] Harden session metadata writes with fsynced temporary files and atomic
  replacement, including parent-directory creation.
- [x] Wire asynchronous runtime roster/owner snapshots into the relay host;
  adapter session IDs are captured when a protocol exposes them and reload
  can reuse the owner handle.
- [x] Integrated resize settling and latest-state frame recovery into a
  reusable TUI render loop with deterministic backpressure tests.
- [x] Verified the current branch with `make verify` and `make rust-test`.
- [x] Final verification: legacy compatibility coverage passed, Rust workspace
  tests and Clippy passed, and formatting passed.

Still required before cutover:

- [x] Wire the relay host into the Rust CLI roster UX, including native/ACP
  selection, selected-first routing, direct prompts, cancellation, and live
  normalized event streaming.
- [x] Finish the supported adapter lifecycle slice: terminal process-group
  ownership, mode replacement/synchronization, ACP session reload identity,
  and roster failure/reload branches have translated Rust coverage.
- [x] Production terminal UI parity for the current rewrite slice: queue
  controls, permission answer focus, lazy tool/diff/terminal detail,
  launcher/settings persistence, and inline interaction behavior.
- [x] Add a catalog-backed `/config` roster editor for next-launch order and
  membership; Enter toggles, Alt+↑/↓ reorders, and Ctrl+S persists it.
- [x] Reconcile idle `/config` catalog edits against the running session while
  preserving ad-hoc adapters and coordinator rollback semantics.
- [x] Cover deferred new-owner activation with a Ready-gated promotion and
  rollback cleanup when the replacement or owner transfer fails.
- [x] Complete exposed session metadata compatibility for relay and standalone
  launches, including owner/title/identity/protocol/session aliases, atomic
  off-thread writes, capability-gated restore, and live roster updates. The
  retired SQLite session browser remains intentionally unexposed.
- [ ] Complete real-agent dogfooding, preview release/rollback, and staged
  default cutover (provider credentials and launch policy are environment
  dependent).

Build and verification usage is documented in docs/RUST_REWRITE.md. The Rust
executable is the only supported client; real-agent dogfooding remains an
environment-dependent release gate.

Dogfood note: the installed Claude Code 2.1.251, Codex CLI 0.150.1, Qwen Code
0.21.14, and Gemini CLI 0.29.5 were probed through their ACP startup commands;
Claude, Codex's `codex-acp` wrapper, Qwen, and Gemini all returned valid ACP
`initialize` responses. Gemini's cached credentials then return
`IneligibleTierError` because Gemini Code Assist for individuals is no longer
supported and requires migration to Antigravity; a real model turn therefore
remains provider-account dependent.

## Phase 0 — Baseline and Performance Harness

> The phase checklist below is the original design decomposition retained for
> traceability. Its unchecked boxes are historical planning notes; the
> authoritative current status is the dated implementation checklist above.
> The only remaining release gate is provider-backed real-agent dogfooding and
> staged cutover, which depends on credentials and rollout policy.

**Purpose:** Make “fast in tmux” falsifiable before a renderer exists.

- [ ] Add a deterministic fixture generator for:
  - one 5,000-word agent reply with prose, lists, and code fences;
  - 100 alternating human/agent turns averaging 300 words;
  - active token streaming while the user edits a prompt;
  - repeated pane resize and a stopped/crashed adapter.
- [ ] Add a Rust benchmark binary that feeds those fixtures to the transcript
  model and records render time, input-to-paint latency, RSS, allocations (in
  CI where available), and bytes emitted to the terminal backend.
- [x] Keep terminal verification deterministic and process-local with
  Ratatui's `TestBackend`; do not automate tmux servers or sessions.
- [ ] Establish and enforce these initial budgets on the reference CI machine;
  record machine details with benchmark output rather than comparing machines
  blindly:

  | Scenario | Required result |
  | --- | --- |
  | 5k-word reply, continuous scroll | no event-loop/input stall over 100 ms; p99 render work under 16 ms after cache warm-up |
  | 100 turns, 300 words each | bounded visible-row memory; no history-wide rewrap on scroll |
  | 20 token chunks/second | input remains responsive; redraws are batched to at most 20 Hz |
  | resize storm | only latest geometry is rendered after a short debounce |
  | blocked/slow terminal | bounded pending output; newest complete state wins |

**Exit criterion:** The harness runs locally and in CI, fails on a budget
regression, and produces a baseline for the existing client where practical.

## Phase 1 — Core Event Model and Persistence

**Purpose:** Extract behavior from UI lifecycle and make replay deterministic.

- [ ] Define `AgentCommand`, `AgentEvent`, `AgentCapabilities`, `SessionState`,
  `RelayState`, and `TranscriptEvent` in `codeswarm-core`. Events must cover
  response text, thought text, tools, terminal lifecycle, permission requests,
  mode updates, failures, completion, and adapter replacement.
- [ ] Implement a pure reducer from `(SessionState, AgentEvent)` to next state
  plus explicit effects. UI rendering, process I/O, timers, and SQLite calls
  must not be inside the reducer.
- [ ] Make the transcript append-only and attributable by stable roster slot.
  Keep public relay context separate from local-only tool/thought/UI events,
  matching the existing collaboration journal contract.
- [ ] Implement durable event/session storage with schema versioning and an
  importer for existing CodeSwarm session metadata where feasible. Failed or
  partially imported sessions must remain readable by older CodeSwarm releases.
- [ ] Translate pure relay cases from the legacy relay contract, including N>2
  rotation, queued prompts, direct turns, reviewer-only stop token handling,
  cancellation and maximum-round behavior.

**Exit criterion:** Recorded event traces replay into identical state and
translated relay tests cover every documented `AGENTS.md` relay invariant.

## Phase 2 — Adapter Host and Compatibility

**Purpose:** Retain integrations that do not speak ACP.

- [ ] Define an async `AgentAdapter` trait with `start`, `stop`, `send_prompt`,
  `cancel`, `reload`, `set_mode`, `capabilities`, and a normalized event stream.
  Explicitly model unsupported operations instead of faking ACP capability.
- [ ] Port the generic stdio ACP implementation: JSON-RPC framing,
  initialization, permission answers, terminal operations, session loading,
  mode/command catalog replacement, cancellation, and bounded stream handling.
- [ ] Port the native Antigravity/Agy stream-JSON adapter as its own
  implementation of `AgentAdapter`; do not route it through an invented ACP
  bridge.
- [ ] Inventory current agent definitions and classify each as ACP or native.
  Port only working adapters in the first release, while keeping the adapter
  host extensible for future CLI protocols.
- [ ] Translate lifecycle/recovery regressions: tombstone immediately on crash,
  reload into the same roster slot, reuse a session only when supported, rewind
  that agent's shared-context watermark, and distinguish startup failure from
  mid-turn crash.
- [ ] Add adapter-contract tests that intentionally omit, reorder, and replace
  capabilities/modes between events.

**Exit criterion:** One ACP CLI and the native Agy adapter can both execute the
same scripted turn, emit equivalent normalized traces, and participate in the
same relay state machine.

## Phase 3 — Transcript Engine

**Purpose:** Eliminate the long-message scroll failure at its architectural
source.

- [ ] Define immutable transcript blocks: header, text paragraph, list,
  fenced code, tool summary/detail, diff summary/detail, thought summary, and
  system/permission notice. Store original source for copy/export.
- [ ] Parse new/finalized response text into blocks off the input/render path.
  While streaming, retain an inexpensive plain-text tail; parse and replace
  only the finalized portion without changing prior row offsets unnecessarily.
- [ ] Build a width-keyed row/wrap cache and prefix-sum row index. On scroll,
  binary-search visible rows and render viewport plus overscan only.
- [ ] On terminal resize, invalidate width-dependent wrap entries lazily. Do
  not synchronously rewrap the entire history before accepting input.
- [ ] Make long responses, thoughts, tool output, and diffs collapsed by
  default. Expansion is an explicit state change and only materializes that
  detail's rows.
- [ ] Add transcript tests for 5,000-word single-message scrolling, mixed
  Markdown/code, copy fidelity, resize while scrolled away from tail, and
  incremental streaming without row corruption.

**Exit criterion:** The Phase 0 5k-word benchmark passes using a single long
agent reply; no optimization may rely only on having many small messages.

## Phase 4 — Minimal tmux-First Terminal Client

**Purpose:** Ship one usable end-to-end vertical slice before advanced UI.

- [ ] Build the `codeswarm` CLI and Ratatui renderer. It must retain keyboard
  operation and restore terminal state on every exit path.
- [ ] Implement a compact fixed status line: active agent, permission policy,
  working directory, queued count, streaming state, and elapsed time.
- [ ] Implement prompt editing, history, slash-command completion, submit,
  first/second `Ctrl+C` semantics, cancellation, scroll/follow-tail toggle,
  and keyboard help.
- [ ] Render text as plain cached terminal rows by default. Syntax highlighting,
  rich Markdown decoration, and previews are optional/lazy render modes.
- [ ] Render permission requests as one focused keyboard-driven decision. Keep
  their pending state independent of transcript scrolling.
- [ ] Throttle agent stream paints; the adapter may receive token-sized chunks,
  but the renderer consumes a bounded coalesced stream.
- [ ] Verify the complete path in tmux: prompt → ACP/native adapter → stream →
  cancel/permission → persisted session → resume.

**Exit criterion:** This path passes all Phase 0 budgets in the guarded
full-screen implementation.

### Current implementation status (Rust rewrite)

- The full-screen Ratatui client is the active `codeswarm` release binary and uses
  the existing lightweight `ratatui`/`crossterm` stack plus `tui-textarea` for
  bounded multiline editing.
- `/help`, `/config`, `/export`, `/mode`, `/collab`, `/clear`, `/cancel`, and
  `/close` are handled locally; local commands never
  become adapter prompts.
- `/config` is a keyboard-first modal with follow-tail and collapsed detail
  toggles, plus mode, collaboration, and keyboard
  guidance. It is rendered without touching the transcript cache.
- Markdown export reads logical transcript blocks directly, retaining hidden
  thought/tool details without rewrapping the 5k-word viewport.
- The Rust agent catalog restores the built-in native/ACP entries and accepts
  custom or built-in override definitions from
  `$XDG_CONFIG_HOME/codeswarm/codeswarm.json`.
- Bare launch now opens a real keyboard-driven agent store when no roster is
  saved: `Space` selects, `Alt+↑/↓` reorders, and `Enter` persists and starts
  the highlighted roster. `/agents` returns to that store from a session.
- Ratatui integration tests exercise configuration, export, store selection,
  compact layouts, and terminal state without controlling a tmux server.

## Phase 5 — Collaboration and Lazy Detail Views

**Purpose:** Restore CodeSwarm's differentiators without taxing common use.

- [ ] Implement roster launch, selected-first routing, duplicate-name
  disambiguation, queued-message visibility/cancellation, and sequential relay
  turn status.
- [ ] Implement CodeSwarm's shared policy mapping: default **Auto pilot**,
  native per-adapter mode resolution after catalogs arrive, roster-wide sync,
  no user-facing `Mixed` mode.
- [x] Implement direct/private turns, FIFO steering semantics, and the
  reviewer-only safe-word rules.
- [ ] Add lazy, keyboard-selectable tool output, terminal output, diff, and
  thought detail views. They may be rich, but must not affect idle transcript
  rendering or scroll cost when collapsed.
- [x] Use one guarded full-screen alternate-screen experience.
- [ ] Port store/launcher/settings behavior: restore last valid roster, open
  the store when none resolves, pre-select detected agents but never auto-start
  them, and retain CodeSwarm branding and no-telemetry policy.

**Exit criterion:** A multi-agent session preserves the documented relay and
failure/reload contracts while its collapsed transcript passes deterministic
viewport benchmarks.

## Phase 6 — Cutover

- [ ] Run both clients against a shared scripted adapter trace corpus and
  compare normalized state, relay order, persistence results, and user-visible
  terminal decisions—not byte-for-byte rendering.
- [x] Run `make verify` for formatting, Clippy, unit/integration tests,
  package archive validation, release build, and deterministic benchmarks.
- [ ] Dogfood with ACP and native adapters in constrained terminal sessions;
  capture only local benchmark diagnostics, never telemetry.
- [x] Release the Rust frontend under the existing `codeswarm` identity with a
  single native launcher and no compatibility runtime.

## Completion Checklist

- [x] `codeswarm` has a full-screen Rust implementation and retains its binary
  import, executable, branding, and no-telemetry commitments.
- [ ] A 5,000-word single agent reply scrolls within the defined benchmark
  budget while the agent streams and the prompt remains editable.
- [ ] ACP and at least one native non-ACP adapter use the same normalized core
  and can participate in a relay.
- [ ] All documented relay, permission-policy, recovery/reload, launch, and
  custom-adapter contracts have translated regression coverage.
- [ ] Rich details are lazy and cannot regress collapsed transcript scrolling.
- [ ] tmux/SSH, resize, slow-terminal, cancellation, persistence, and recovery
  integration tests pass before default cutover.
