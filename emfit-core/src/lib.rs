//! Public API for the UI crate.
//!
//! Per STANDARDS sec 1, this crate holds all logic and depends on no UI or
//! Tauri types. `cargo check -p emfit-core` must succeed with `src-tauri`
//! deleted.

pub mod app;
pub mod error;
pub mod model;
pub mod parser;
pub mod service;

pub use error::{Error, Result};
