//! Jotter: meeting capture and, eventually, note extraction.
//!
//! Everything lives in the library so there is exactly one crate root owning
//! `src/`. `src/main.rs` is a thin entry point and `src/bin/record.rs` is a
//! debugging CLI; both consume this. Splitting modules across a lib and a bin
//! root in the same directory is legal but confuses tooling, and forces
//! intra-project imports to go through the crate name instead of `crate::`.

pub mod audio;
pub mod ui;
