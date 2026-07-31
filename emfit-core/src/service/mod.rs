//! Operations: export, persistence, network, anything that mutates state.
//!
//! Services hold dependencies (parser registry, config, http client) and
//! expose async-friendly methods the UI crate calls through callbacks.

pub mod benchlog;
pub mod breakdown;
pub mod case;
pub mod config;
pub mod elevation;
pub mod export;
pub mod filetype;
pub mod fold;
pub mod logging;
pub mod presets;
pub mod query;
pub mod scan;
pub mod search;
pub mod task;
pub mod tree;
pub mod treemap;
pub mod usn;
pub mod view;
pub mod volume;
