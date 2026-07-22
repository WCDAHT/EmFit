//! Operations: export, persistence, network, anything that mutates state.
//!
//! Services hold dependencies (parser registry, config, http client) and
//! expose async-friendly methods the UI crate calls through callbacks.

pub mod case;
pub mod config;
pub mod export;
pub mod logging;
pub mod task;
