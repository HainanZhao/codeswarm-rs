//! Internal presentation modules for the CodeSwarm application package.
//!
//! The reusable protocol and relay contract lives in `codeswarm-adapters`.
//! Transcript and TUI code remain application-private modules here.

#[path = "../crates/codeswarm-transcript/src/lib.rs"]
pub mod transcript;
#[path = "../crates/codeswarm-tui/src/lib.rs"]
pub mod tui;
