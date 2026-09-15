//! Jotter: meeting capture and, eventually, note extraction.
//!
//! Everything lives in the library so there is exactly one crate root owning
//! `src/`. `src/main.rs` is a thin entry point that dispatches to either `ui`
//! or `cli`. Splitting modules across a lib and a bin root in the same
//! directory is legal but confuses tooling, and forces intra-project imports to
//! go through the crate name instead of `crate::`.
//!
//! `audio` is unconditional; the two front ends are feature-gated so a
//! `--no-default-features --features cli` build never compiles the GUI stack.

pub mod audio;

#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "gui")]
pub mod ui;
