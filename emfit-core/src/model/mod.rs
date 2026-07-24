//! Data types. One file per concept (e.g. `model/document.rs`).
//!
//! Per STANDARDS §1: don't accumulate everything here under a generic name.
//! Add a sibling file when a new domain concept appears.
//!
//! The index is deliberately filesystem-agnostic. `architecture.md` §2: the
//! data model stays concrete, small, and shared; the variation between
//! filesystems lives one layer down, in `parser/`.

pub mod builder;
pub mod caps;
pub mod entry;
pub mod index;
pub mod volume;
