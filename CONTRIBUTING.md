# Contributing to CodeSwarm

CodeSwarm is a two-package Rust workspace: the reusable `codeswarm-adapters`
library and the `codeswarm` application. Cargo is the canonical build, test, lint, and
package tool; no interpreter or generated dependency environment is required.

## Verification

Run the complete gate before opening a pull request:

```bash
make verify
```

This runs `cargo fmt`, all workspace tests, Clippy with warnings denied, a
locked release build, and package archive validation for both packages.

For UI, adapter, or CLI changes, add a regression at the integration boundary
that failed. Use Ratatui's `TestBackend` for deterministic rendering. Keep
transcript tests focused on logical blocks and bounded viewport work rather
than exact terminal cell counts.

## Performance rules

The common path must remain tmux-safe and non-blocking:

- stream chunks into existing logical blocks instead of creating one row per
  token;
- render only a cached viewport slice;
- keep tool, thought, terminal, and diff details collapsed until focused;
- coalesce resize and repaint requests;
- avoid blocking adapter or filesystem work in the draw/input loop.
