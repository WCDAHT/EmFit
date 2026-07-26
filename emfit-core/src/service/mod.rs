//! Operations: export, persistence, network, anything that mutates state.
//!
//! Services hold dependencies (parser registry, config, http client) and
//! expose async-friendly methods the UI crate calls through callbacks.

pub mod benchlog;
pub mod case;
pub mod config;
pub mod elevation;
pub mod export;
pub mod logging;
pub mod scan;
pub mod task;
pub mod volume;
