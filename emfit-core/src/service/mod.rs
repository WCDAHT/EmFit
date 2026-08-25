//! Operations: export, persistence, network, anything that mutates state.
//!
//! Services hold dependencies (parser registry, config, http client) and
//! expose async-friendly methods the UI crate calls through callbacks.

pub mod background;
pub mod benchlog;
pub mod breakdown;
pub mod cache;
pub mod case;
pub mod config;
pub mod elevation;
pub mod export;
pub mod filetype;
pub mod fold;
pub mod lock;
pub mod logging;
pub mod presets;
pub mod query;
pub mod replay;
pub mod scan;
pub mod schedule;
pub mod search;
pub mod syntax;
pub mod task;
pub mod tree;
pub mod treemap;
pub mod update;
pub mod usn;
pub mod values;
pub mod view;
pub mod volume;
