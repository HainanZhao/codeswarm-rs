# Daily-driver delivery and review

Date: 2026-09-06. Base: `eed074c` (CodeSwarm 0.9.9).

## Delegation record

All implementation workers ran through the installed OpenCode CLI with the
explicit model `opencode-go/glm-5.3-flash`, using the connected OpenCode Go
account. Primary, exploration, and small-model defaults were pinned to that
same model. Workers used isolated worktrees; none published or pushed.

| Package | Worker branch | Delegated commit |
| --- | --- | --- |
| Session archive | `work/daily-driver-archive` | `805fb7ae06fe28a564f5388612354a7290e210a0` |
| Browser/status/summary UI | `work/daily-driver-ui` | `bdef845b30acbf5cf21f55be34be1e25d5b185dc` |
| Pair roles and completion model | `work/daily-driver-workflow` | `473e7200cce56dce52bd18f8363e835137eb7c5b` |

The first workflow-worker request ended without an implementation and was
resumed with the same model. Codex reviewed the delivered patches, integrated
them into the application, and made the corrections below. Delegated commits
are provenance; the integrated working tree contains the reviewed result.

## Delivered behavior

- `/sessions` lists multiple project archives with title, roster, activity time,
  and a response preview. Selection follows stable IDs through list replacement.
- `/resume` and session selection open local history without starting providers.
  A new normal/private prompt reconnects the selected saved provider. Local
  history and summaries remain readable with the provider executable absent.
- `/status` reports this process's version/executable, connection, roster,
  recipient, permissions, and available pair-role/handoff information.
- `/summary` reports the last response, observed tool results, and optional
  working-tree evidence. Prose is not treated as proof of passing tests.
- Existing Pair review is the worker/reviewer workflow; no duplicate mode was
  added. Roles stay fixed through review/fix cycles. Worker changes return to
  the reviewer, and only that reviewer can stop a multi-agent pair batch.
- Normal and queued messages use the footer's recipient. Standalone adapters
  also accept the explicit slot-0 routing used by this unified path.
- Routine `working` ribbon flashes are suppressed. Completed collapsed tools
  tuck away during thought streaming; active tool output and manually expanded
  details stay visible. Completed evidence remains available in `/summary`.

## Review corrections

1. Replaced index-only browser selection with stable-ID selection.
2. Scoped completion tool IDs to their provider slot; explicit empty tool
   evidence clears old output. Distinguished a checked-clean working tree from
   missing working-tree evidence.
3. Anchored pair roles to the human task's first responder, including returning
   worker turns and a new task starting at another slot. Kept worker changes
   subject to another reviewer turn.
4. Retained prepared metadata snapshots when archive edits fail, so retry cannot
   lose an already-consumed edit closure.
5. Separated unterminated/torn journal tails before append, preserving subsequent
   valid records. Added periodic and nonblocking durability checkpoints.
6. Propagated coordinator metadata directly to the archive rather than racing
   asynchronous writes to the legacy last-session file. Preserved local evidence
   fields when provider metadata is refreshed.
7. Kept history replay separate from live timers, permissions, and dispatch;
   avoided duplicate provider history when a local transcript is already loaded.
8. Routed the displayed recipient explicitly and froze queued targets. Fixed
   standalone command handling so this path cannot silently ignore prompts.
9. Kept Ctrl+C usable inside product panels, routed their mouse wheel input, and
   reserved the existing status ribbon for runtime feedback.
10. Applied the user's active-detail and redundant-indicator design decisions,
    with retained sources and cache-aware suppression rather than deletion.

## Validation

`make verify` passed: 432 Cargo tests, formatting, Clippy with warnings denied,
release build, workspace packaging, and whitespace checks.

Additional local mock-provider PTY checks passed:

- Create two distinct archives; open the latest and select the older one without
  any provider start; continue using that older provider handle; view `/status`
  and `/summary`; confirm no duplicate archive is created on continuation.
- Remove the mock provider executable, then reopen archived history and summary
  with zero provider starts.
- Select slot 1 for a task, finish review at slot 0, then submit an untagged task
  and confirm its first provider request still goes to the arrow's slot 1.

These exercise real terminal input/output and local mock ACP processes. They do
not claim exhaustive compatibility with every live provider. No new telemetry,
sharing service, paid-provider validation, release, or installation was added.

Legacy provider-only sessions may lack local transcript data until explicitly
continued. Missing or expired provider handles can prevent continuation but do
not prevent reading an existing local archive. Working-tree summaries describe
observed paths and may include changes that predate the agent's work.
