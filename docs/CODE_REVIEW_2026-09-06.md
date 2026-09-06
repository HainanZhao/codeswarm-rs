# Code review — 2026-09-06

Reviewed the Rust workspace's adapter startup/reload and notification handling,
relay/context scheduling, resume/history handling, transcript layout and caching,
input/navigation, path indexing, persistence/settings, and attachment loading.
Changes below address confirmed defects; this is not an exhaustive correctness proof.

| Finding | Impact | Fix and regression coverage |
| --- | --- | --- |
| ACP patches were parsed as complete tool snapshots and content was discarded | A status-only update erased the tool name/output and could hide a completed tool | Retain normalized tool state within a turn, apply only valid supplied fields, extract text/diff/raw output, honor empty replacements, and reset reused IDs. Tests cover real subprocess notifications plus omitted, invalid, and replacement fields. |
| ACP stop retained queued events and startup could retain old modes | Reload could deliver stale text/readiness/catalog state from the dead process | Clear pending events/tool state on stop and modes before startup. A subprocess reload test verifies only new readiness survives. |
| `/clear` retained history block IDs and mouse targets | Later history chunks could modify a new human message after IDs were reused | Clear all history indexes and controls along with the transcript. A TUI regression clears midway through multi-agent history replay. |
| Wrapping used character counts and repeatedly recounted built rows | Wide/combined characters could clip or split; long tokens caused unnecessary CPU work | Measure terminal columns, retain grapheme clusters, and track current row width. Tests cover CJK, flags, joined emoji, combining marks, narrow panes, cache widths, and rendered output. |
| Attachment limits relied only on earlier metadata | A growing or special file could exceed the advertised memory limit or block reading | Reject non-regular files and cap actual reads at the limit plus one byte. Tests verify exact-limit success, bounded oversize reads, and directory rejection. |

ACP updates follow the [official tool-call patch contract](https://agentclientprotocol.com/protocol/v1/tool-calls#updating): omitted fields are not replacements. Empty content arrays explicitly clear output. Normalized state is discarded between prompts and when the adapter stops.

## Performance check

Compared the checked-in transcript implementation at `9064f02` with the updated
implementation using `rustc -O` on this machine, with the same release dependency
artifacts. Each measurement performs 20 fresh layouts; counts of generated rows
matched before and after.

| Input | Width | Before | After |
| --- | ---: | ---: | ---: |
| 262,144-character unbroken ASCII token | 4,096 | 453.6 ms | 143.1 ms |
| Existing 5,000-word prose fixture | 80 | 4.736 ms | 4.318 ms |

These are local diagnostic timings, not portable performance guarantees. The
long-token case improved approximately 3.2×. Existing cached-scroll regressions
remain part of the verification gate.

## Validation

Cargo regressions exercise the adapter process boundary and Ratatui TestBackend.
`make verify` is the final gate: formatting, full workspace tests, Clippy with
warnings denied, release build, package validation, and whitespace checks.
Provider interaction tests use local mock processes rather than paid live agents.
The Unicode dependencies were already present transitively in Cargo.lock; this
change declares them directly without upgrading their locked versions.
