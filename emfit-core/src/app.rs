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

/// Where the release manifest lives (`service::update`). One JSON document
/// listing every product the site hosts, each with its current version and the
/// path to its binary.
///
/// Paths inside it are relative to this URL, so the releases resolve under
/// `https://wcdaht.github.io/content/<product>/`.
///
/// Overridable per install through `config.toml` (`[update] manifest_url`),
/// which is how a staging site is tested without a rebuild.
pub const MANIFEST_URL: &str = "https://wcdaht.github.io/manifest.json";

/// The key EmFit is filed under in that manifest.
///
/// Separate from [`PRODUCT`] because the two are not the same kind of name:
/// the manifest keys are slugs (`Message-Maestro`), the product name is what
/// the window title says. They coincide here and may not in the next app.
pub const MANIFEST_KEY: &str = "EmFit";
