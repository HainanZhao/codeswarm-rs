# CodeSwarm agent-development notes

## Project identity

- CodeSwarm is the current project name and the only supported package identity.
- The published Rust binary and Cargo workspace identity are `codeswarm`. There
  is no compatibility launcher or alternate runtime.
- User-facing branding uses CodeSwarm and the `✈` symbol.
- No telemetry is collected. The upstream sponsor tile and testimonial/about
  UI were removed; `©` attribution to Will McGugan remains in the license.

## ACP relay behavior

- Every session defaults to CodeSwarm's **Auto pilot** permission policy. After
  all active agents advertise their mode catalogs, CodeSwarm translates the
  policy to each native mode ID and synchronizes the complete roster. A later
  user selection becomes the new desired roster-wide policy; `Mixed` is not a
  user-facing mode.
- An unlimited-size roster of ACP agents relay turns sequentially in a ring
  (`crates/codeswarm-core/src/relay.rs`, `Relay`), never concurrently — a relay
  has a causal dependency on the previous response. Solo sessions (roster size
  1) never construct a relay; `Conversation._relay_active` gates every relay
  code path so the common single-agent case is untouched.
- The coordinator owns the session record. Roster positions only express
  order; any agent may be dropped as long as at least one remains active.
  Each active agent persists its own optional provider session handle.
- A failed adapter is tombstoned immediately so nothing can be dispatched to
  a dead process, and the user is then offered a reload. Reloading puts a
  fresh adapter in the same roster slot, reuses the session id only when the
  dead adapter advertised session loading, and rewinds that slot's
  shared-context watermark so its next turn replays the conversation it
  missed. Declining leaves the agent dropped for the rest of the session.
  Failures that happen after start-up carry `help="crashed"`; the default
  `help="fail"` text is about installing the agent and must not be shown for
  an adapter that started and then stopped.
- Each agent receives the ordered public human and agent-message updates it has
  not seen since its previous turn. Only streamed message text enters this
  journal; tool calls, thoughts, terminal output, and UI history stay local.
- `/goal OBJECTIVE` sets and starts a coordinator-owned session goal; bare
  `/goal` shows it, `run` resumes, `done` marks completion, and `clear` removes
  it. Goals travel as ordinary prompt context to all adapter types, change at
  turn boundaries, and persist only through explicit session resume. They do
  not override permissions, turn limits, or current user instructions.
- Each agent's first prompt includes a brief roster introduction identifying
  itself and its active collaborators.
- Replacement adapters also receive the retained original public task, even
  after its journal entry has been pruned. Private prompts never replace it.
- Untagged human messages submitted while an agent is working are queued back
  to that same agent, in FIFO order, before the relay advances. The next agent
  receives the active agent's latest response as context. An explicit roster
  selection overrides that target: the prompt footer names the selected agent
  as the next recipient, so a queued message has to be delivered to it rather
  than to whichever agent happened to be working when it was submitted.
- Clicking an agent beside the prompt selects it as the first recipient for
  the next normal relay message. Duplicate names display their roster number.
- `[CODESWARM:STOP]` is the safe word, but only an agent reviewing a different
  agent's response may use it. The first responder after any human message and
  direct/private turns cannot stop peer review. In roster routing, stopping
  also requires every currently routable member to have participated in the
  batch, including the current reviewer. An eligible reviewer with
  nothing meaningful to add may send an emoji followed by the token; a
  token-only response is displayed as `👍`. CodeSwarm always hides the token.
- While work is active, the first `Ctrl+C` requests cancellation and a second
  press within three seconds quits; while idle, `Ctrl+C` quits immediately.
  `codeswarm resume [PATH]` starts the last provider-backed session saved for
  that project; a normal launch always starts a fresh provider session.
  Chat `/resume` switches to the project session retained before the current
  adapters started, after shutting down the current workers. It appears in
  local help/completion, accepts no arguments, and rejects active work, pending
  permissions, or queued prompts via the status ribbon. Fresh startup metadata
  must never replace the retained resume target.
  Resume is history-only until a new human prompt: ACP session/load replay is
  display-only history, never live activity. It must not start timers, set a
  busy turn, queue input behind phantom work, or automatically dispatch agents.
- Native Antigravity print turns allow 24 hours (`--print-timeout 1440m`).
  ACP prompt turns have no CodeSwarm wall-clock timeout; startup/control request
  deadlines and user cancellation remain separate.
- The relay defaults to 100 automated turns and can be adjusted with
  `--max-rounds N`. This is a runaway-safety limit, not a per-agent budget —
  it does not scale with roster size.
## Launch flow

- Bare `codeswarm` restores the last-used roster (`launcher.roster` setting,
  written whenever the roster selection changes). If no saved roster resolves,
  it opens the agent store instead of auto-starting anything — detection
  (`agents.detect_preferred_agents`) only pre-selects candidates on that
  screen, it never starts a session by itself.
- In the store, `space` toggles an agent's membership in the roster being
  built; `enter` launches that roster, or the highlighted agent solo if
  nothing is selected. There is no quick-launch row.

## Agent workflow

- When a user requests an actionable repository change, proceed with the
  implementation without asking for a separate approval of the approach.
  Ask a question only when missing information would materially change the
  result or make the action unsafe.

## Terminal notifications

- Before the conversation prompt is available, setup, store, configuration,
  and modal feedback may use the same Ratatui surface.
- Once the conversation prompt is shown, all in-terminal notifications use
  CodeSwarm's single-line, full-width status ribbon. Do not introduce another
  notification style over the conversation UI.
- Optional operating-system notifications sent through `system_notify()` are
  separate from this in-terminal presentation rule and remain supported.
- `/settings` offers Terminal, Light, and Dark themes. Terminal uses the user's
  canvas and ANSI accents; explicit palettes retain readable text contrast.
  Reserve teal exclusively for human input/messages and interface controls.
  Agent output follows a Codex-inspired neutral hierarchy: ordinary foreground
  for prose, headings, code and tables; bold emphasis and underlined links;
  muted metadata and thoughts. Do not reuse teal/cyan for output formatting,
  transcript notices, or diff hunk headers. Red/green diff semantics and the
  distinct non-teal roster identity colors remain supported.
  Within each agent turn, render message text before its thought and tool
  details, regardless of adapter event order. Thoughts use a word-wrapped,
  rolling two-line preview: a full bottom line moves up as a new line begins,
  and only the newest two lines remain visible. Tools keep a one-line tail
  preview. Flatten paragraph whitespace in collapsed thoughts so blank lines
  cannot clear the upper line or force text to start on the second line;
  expanded history keeps the original formatting. Both use faint gray italics.
  Use only clickable 💭 (thought) and 🔧 (tool) icons in the two-column left
  gutter, without "Thought"/"Tool" text labels, generic "Tool call" titles, or
  completion counts. Keep meaningful tool names and running/failure status.
  Each icon toggles expansion;
  keep Ctrl+O as the keyboard shortcut. Controls must not consume
  preview width, and hit targets must be rebuilt on redraw/scroll/resize.
  Preserve stable block IDs and cached earlier turns when ordering details.
  Thought and tool detail text/previews are an intentional exception:
  use faint muted gray and italics, visually lighter than normal messages.
  Lower contrast is a deliberate user decision; do not raise detail contrast
  to satisfy the ordinary text contrast target or map it to the normal terminal
  foreground. Keep other UI text at its normal contrast.
  Redraws follow input, adapter/index updates, and visible timer deadlines.
- After two minutes without adapter activity during a turn, the status ribbon
  shows the silent duration and offers `Ctrl+C` cancellation or `/reload`.
  Silence does not cancel work automatically. Pending permissions and
  cancellation suppress the warning; new activity clears it. Reloading a
  silent agent cancels its current turn before restarting that roster slot.

## Verification

Run the repository quality gate before release:

```bash
make verify
```

For TUI, ACP, and CLI changes, add a Cargo-native regression test at the
integration boundary that failed. Use Ratatui's `TestBackend` for rendering
and Rust unit/integration tests for contracts. Test invalid and replacement
external state as well as the nominal state; adapters may omit, reorder, or
replace values between messages.

`cargo test --workspace` is the regression gate for the relay ring. Preserve
two-agent alternation except when intentionally changing a documented relay
contract, such as reviewer-only stopping; roster tests cover N>2 semantics.
