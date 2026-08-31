# Rust terminal architecture

CodeSwarm is now a two-package Rust Cargo workspace. The production entry point is
`codeswarm`, backed by Ratatui and Crossterm.

## Package and module boundaries

- `codeswarm-adapters` is the reusable package for normalized events, relay
  scheduling, policies, persistence, adapter-independent contracts, and ACP
  and native adapter implementations.
- The `codeswarm` application package keeps `transcript` as an internal module
  for logical blocks, Markdown export, wrapping, and
  cached viewport rows.
- Its internal `tui` module owns prompt editing, config/help surfaces, permissions, and
  low-churn rendering.
- the `codeswarm` binary owns process startup, terminal input, adapter
  lifecycle, and command routing.

The renderer uses a guarded full-screen alternate screen. The transcript path
is viewport-bounded: long streamed responses do not cause a full-history
redraw, and expensive details remain lazy.

## Build and run

```bash
cargo build --release -p codeswarm
./target/release/codeswarm --demo
```

Use `--agy`, `--acp`, or repeated `--roster` arguments for native, ACP, and
mixed sessions. Custom native adapters remain valid; ACP is not required for
every CLI.

## Verification

```bash
make verify
```

The gate runs Cargo formatting, workspace tests, and Clippy. Deterministic
Ratatui and transcript benchmarks validate cached scrolling and interaction
without managing external tmux servers or sessions.
