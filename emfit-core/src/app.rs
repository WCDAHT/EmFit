//! Application identity - the one place to edit when copying this template
//! into a new app.
//!
//! These three constants seed every per-app OS path the core resolves: the
//! config directory (`service::config`) and the log directory
//! (`service::logging`). They are deliberately the *only* hardcoded names in
//! the project so a fork is a single-file change.
//!
//! On Windows the company/product become `%APPDATA%\<COMPANY>\<PRODUCT>\...`;
//! on Linux the product (lower-cased) drives `~/.config/<product>/...`; on macOS
//! the qualifier+company+product form the `.../Library/Application Support/`
//! bundle path. Keep them in step with `productName`/`identifier` in
//! `src-tauri/tauri.conf.json`.

/// Reverse-DNS qualifier (the `com` in `com.icebergforensics.emfit`). Used only
/// on macOS, where paths are bundle-style; ignored on Windows and Linux.
pub const QUALIFIER: &str = "com";

/// Company / organization name. Becomes the first path segment under
/// `%APPDATA%` on Windows.
pub const COMPANY: &str = "IcebergForensics";

/// Product name. Matches `productName` in `tauri.conf.json`.
pub const PRODUCT: &str = "EmFit";
