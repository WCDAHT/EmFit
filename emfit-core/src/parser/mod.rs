//! Input format handling, one file per format.
//!
//! Per STANDARDS §4.3:
//! - For N <= 2 formats: an `enum` plus `match`.
//! - For N >= 3 (or when variants come from elsewhere): the `Parser` trait
//!   below plus an explicit `ParserRegistry`. Registration is explicit,
//!   not reflective.
//!
//! Apps with one or two formats should delete this module's contents and
//! use an enum. Apps with three or more keep the trait and register their
//! parsers in `ParserRegistry::default()`.
//!
//! For EmFit the "formats" are filesystems. Two orthogonal axes live here
//! (`architecture.md` §4): [`block`] is *where bytes come from* — a volume, a
//! physical drive, an image — and the filesystem scanners are *how to
//! interpret them*. The `FsScanner` trait that generalizes the second axis is
//! deliberately not written until a second scanner exists; until then NTFS
//! talks to `IndexBuilder` directly, never touching the index's internals.

pub mod block;
pub mod ntfs;

use std::path::Path;

use crate::error::Result;

/// A parser for one input file format.
///
/// `can_parse` peeks at the file's start (and optionally the path) to decide
/// whether to claim it. `parse` does the real work. Both run on whatever
/// thread the caller chose; the trait is `Send + Sync` so parsers can live
/// behind shared state.
pub trait Parser: Send + Sync {
    /// Human-readable name of the format (e.g. "Discord DM export").
    fn name(&self) -> &str;

    /// File extensions this parser handles, without leading dots. Used by
    /// open-file dialogs and as a coarse pre-filter.
    fn extensions(&self) -> &[&str];

    /// Cheap content sniff. `sample` is the first few KB of the file; the
    /// path is provided in case extension or filename is part of the
    /// detection rule. Implementations should return `true` only if they
    /// are confident they can parse the file.
    fn can_parse(&self, sample: &[u8], path: &Path) -> bool;

    /// Parse the file at `path`. The parsed output type is per-app; this
    /// stub returns `()` and apps redefine the trait with a concrete
    /// document type once they have one.
    fn parse(&self, path: &Path) -> Result<()>;
}

/// A registry of parsers tried in registration order.
///
/// Construct with `ParserRegistry::default()` once at startup; pass by
/// `&` to consumers. To add a format, write a parser in a sibling module
/// and register it after construction:
///
/// ```ignore
/// let mut registry = ParserRegistry::default();
/// registry.register(Box::new(discord::DiscordParser));
/// registry.register(Box::new(twitter::TwitterParser));
/// ```
///
/// Apps with a fixed set of parsers can wrap this in a free function in
/// their `parser` module so the registration list lives in one place.
#[derive(Default)]
pub struct ParserRegistry {
    parsers: Vec<Box<dyn Parser>>,
}

impl ParserRegistry {
    /// Add a parser to the registry. Later calls do not displace earlier
    /// registrations of the same name; first match wins.
    pub fn register(&mut self, parser: Box<dyn Parser>) {
        self.parsers.push(parser);
    }

    /// Find the first parser that claims `sample` / `path`. Returns `None`
    /// if no registered parser matches.
    pub fn find_parser(&self, sample: &[u8], path: &Path) -> Option<&dyn Parser> {
        self.parsers
            .iter()
            .find(|p| p.can_parse(sample, path))
            .map(|b| b.as_ref())
    }

    /// All registered parsers, in registration order. Useful for building
    /// open-file dialog filter strings.
    pub fn all(&self) -> impl Iterator<Item = &dyn Parser> {
        self.parsers.iter().map(|b| b.as_ref())
    }
}
