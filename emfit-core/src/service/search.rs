//! Executing a [`Query`] against one or more indexes.
//!
//! One linear pass over the flat node array per volume — no tree walk, no
//! per-name allocation. Pattern needles are folded **once per volume** (case
//! rules are per-volume, features.md §2); every candidate name is then folded
//! on the fly against the arena slice.
//!
//! Interruptible: the pass checks a [`CancellationToken`] every few thousand
//! nodes, so an in-flight search abandons quickly when the next keystroke
//! arrives (features.md §2 "incremental / debounced search").

use crate::model::caps::VolumeCaps;
use crate::model::entry::EntryFlags;
use crate::model::index::{Index, NodeId};
use crate::service::filetype::extension_of;
use crate::service::fold::CaseFold;
use crate::service::query::{KindFilter, Pattern, Query};
use crate::service::task::CancellationToken;

/// One search result: which volume slot, which node.
pub type Hit = (u16, NodeId);

/// How often the scan looks at the cancellation token.
const CANCEL_STRIDE: usize = 8192;

/// Search one volume, appending hits to `out` in node order.
///
/// Returns `false` when interrupted (results are incomplete and must be
/// discarded by the caller).
pub fn search_volume(
    index: &Index,
    fold: &CaseFold,
    query: &Query,
    volume_slot: u16,
    out: &mut Vec<Hit>,
    cancel: &CancellationToken,
) -> bool {
    let prepared = match Prepared::build(index, fold, query) {
        Some(prepared) => prepared,
        // The path scope names a subtree this volume does not have: no hits,
        // but not an interruption.
        None => return true,
    };

    for (i, id) in index.ids().enumerate() {
        if i % CANCEL_STRIDE == 0 && cancel.is_cancelled() {
            return false;
        }
        if prepared.matches(index, id) {
            out.push((volume_slot, id));
        }
    }
    true
}

/// Search every volume in slot order. `None` when interrupted.
pub fn search_all(
    volumes: &[(&Index, &CaseFold)],
    query: &Query,
    cancel: &CancellationToken,
) -> Option<Vec<Hit>> {
    let mut out = Vec::new();
    for (slot, (index, fold)) in volumes.iter().enumerate() {
        if !search_volume(index, fold, query, slot as u16, &mut out, cancel) {
            return None;
        }
    }
    Some(out)
}

/// Resolve a display path (`C:\Users\Admin`) to a node on this volume.
///
/// The volume's root label must prefix the path (compared under the volume's
/// folding); the remaining segments descend through the CSR children.
pub fn resolve_path(index: &Index, fold: &CaseFold, path: &str) -> Option<NodeId> {
    let caps = index.caps();
    let rest = strip_label(path, caps, fold)?;

    let mut cur = index.root();
    for segment in rest.split(caps.path_separator) {
        if segment.is_empty() {
            continue;
        }
        cur = *index
            .children(cur)
            .iter()
            .find(|&&child| fold.eq(index.name(child), segment))?;
    }
    Some(cur)
}

/// Whether `id` is `scope` or lies underneath it.
pub fn is_under(index: &Index, id: NodeId, scope: NodeId) -> bool {
    let mut cur = id;
    loop {
        if cur == scope {
            return true;
        }
        let parent = index.node(cur).parent();
        if parent == cur {
            return false; // reached the root
        }
        cur = parent;
    }
}

/// Strip the root label off a path, under the volume's folding.
fn strip_label<'a>(path: &'a str, caps: &VolumeCaps, fold: &CaseFold) -> Option<&'a str> {
    let mut chars = path.char_indices();
    for expected in caps.root_label.chars() {
        let (_, got) = chars.next()?;
        if fold.fold(got) != fold.fold(expected) {
            return None;
        }
    }
    Some(chars.as_str())
}

// ---------------------------------------------------------------------------
// prepared, per-volume form
// ---------------------------------------------------------------------------

/// A pattern with its needle folded for this volume (or kept raw when the
/// query is case-sensitive).
enum Needle {
    Substring(String),
    Prefix(String),
    Suffix(String),
    Glob(Vec<char>),
}

/// The query, specialized to one volume: needles folded, scope resolved.
struct Prepared<'q> {
    query: &'q Query,
    fold: CaseFold,
    /// True: compare bytes as-is; the fold above is unused.
    case_sensitive: bool,
    needles: Vec<Needle>,
    extensions: Vec<String>,
    scope: Option<NodeId>,
}

impl<'q> Prepared<'q> {
    /// `None` when a path scope is set but absent from this volume — the
    /// whole volume is out of scope.
    fn build(index: &Index, fold: &CaseFold, query: &'q Query) -> Option<Self> {
        let case_sensitive = query.case_sensitive.unwrap_or(index.caps().case_sensitive);

        let scope = match &query.path_scope {
            Some(path) => Some(resolve_path(index, fold, path)?),
            None => None,
        };

        let prepare = |text: &str| -> String {
            if case_sensitive {
                text.to_string()
            } else {
                let mut folded = String::new();
                fold.fold_str(text, &mut folded);
                folded
            }
        };

        let needles = query
            .patterns
            .iter()
            .map(|p| match p {
                Pattern::Substring(t) => Needle::Substring(prepare(t)),
                Pattern::Prefix(t) => Needle::Prefix(prepare(t)),
                Pattern::Suffix(t) => Needle::Suffix(prepare(t)),
                Pattern::Glob(t) => Needle::Glob(prepare(t).chars().collect()),
            })
            .collect();

        let extensions = query.extensions.iter().map(|e| prepare(e)).collect();

        Some(Self {
            query,
            fold: fold.clone(),
            case_sensitive,
            needles,
            extensions,
            scope,
        })
    }

    fn matches(&self, index: &Index, id: NodeId) -> bool {
        let node = index.node(id);
        let flags = node.flags();
        let q = self.query;

        if !q.include_hidden && flags.contains(EntryFlags::HIDDEN) {
            return false;
        }
        if !q.include_system && flags.contains(EntryFlags::SYSTEM) {
            return false;
        }
        match q.kind {
            KindFilter::Any => {}
            KindFilter::Files if node.is_directory() => return false,
            KindFilter::Folders if !node.is_directory() => return false,
            _ => {}
        }

        if let Some((lo, hi)) = q.size {
            // Directories filter on their subtree total — "show me what's
            // over a gigabyte" should surface the folders too.
            let size = if node.is_directory() {
                node.total_size()
            } else {
                node.size()
            };
            if size < lo || size > hi {
                return false;
            }
        }
        if let Some((lo, hi)) = q.modified {
            let mtime = node.times().mtime;
            if mtime < lo || mtime > hi {
                return false;
            }
        }

        let name = index.name(id);

        if !self.extensions.is_empty() {
            if node.is_directory() {
                return false;
            }
            let ext = extension_of(name);
            let hit = self.extensions.iter().any(|want| {
                if self.case_sensitive {
                    ext == want
                } else {
                    self.fold.eq(ext, want)
                }
            });
            if !hit {
                return false;
            }
        }

        if !self.needles.is_empty() && !self.needles.iter().any(|n| self.name_matches(name, n)) {
            return false;
        }

        if let Some(re) = &q.regex
            && !re.is_match(name)
        {
            return false;
        }

        if let Some(scope) = self.scope
            && !is_under(index, id, scope)
        {
            return false;
        }

        true
    }

    fn name_matches(&self, name: &str, needle: &Needle) -> bool {
        if self.case_sensitive {
            return match needle {
                Needle::Substring(t) => name.contains(t.as_str()),
                Needle::Prefix(t) => name.starts_with(t.as_str()),
                Needle::Suffix(t) => name.ends_with(t.as_str()),
                Needle::Glob(pattern) => glob_match(name, pattern, None),
            };
        }
        match needle {
            Needle::Substring(t) => self.fold.contains_prefolded(name, t),
            Needle::Prefix(t) => self.fold.starts_with_prefolded(name, t),
            Needle::Suffix(t) => self.fold.ends_with_prefolded(name, t),
            Needle::Glob(pattern) => glob_match(name, pattern, Some(&self.fold)),
        }
    }
}

/// Wildcard match over the whole name: `*` any run, `?` exactly one char.
/// The pattern is prefolded; the name folds on the fly when `fold` is given.
fn glob_match(name: &str, pattern: &[char], fold: Option<&CaseFold>) -> bool {
    match pattern.first() {
        None => name.is_empty(),
        Some('*') => {
            // Collapse a run of stars, then try the rest at every suffix.
            let rest: &[char] = {
                let mut p = &pattern[1..];
                while p.first() == Some(&'*') {
                    p = &p[1..];
                }
                p
            };
            if rest.is_empty() {
                return true;
            }
            let mut s = name;
            loop {
                if glob_match(s, rest, fold) {
                    return true;
                }
                let mut it = s.chars();
                if it.next().is_none() {
                    return false;
                }
                s = it.as_str();
            }
        }
        Some('?') => {
            let mut it = name.chars();
            it.next().is_some() && glob_match(it.as_str(), &pattern[1..], fold)
        }
        Some(&want) => {
            let mut it = name.chars();
            match it.next() {
                Some(got) => {
                    let got = fold.map_or(got, |f| f.fold(got));
                    got == want && glob_match(it.as_str(), &pattern[1..], fold)
                }
                None => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::builder::IndexBuilder;
    use crate::model::entry::{RawEntry, Times};
    use crate::model::sink::EntrySink;
    use crate::service::query::RawQuery;

    fn entry(
        fs_id: u64,
        parent: u64,
        name: &str,
        size: u64,
        mtime: i64,
        flags: EntryFlags,
    ) -> RawEntry<'_> {
        RawEntry {
            fs_id,
            parent_id: parent,
            name,
            size,
            allocated: size,
            times: Times { mtime, crtime: 0 },
            flags,
        }
    }

    /// A small volume:
    /// ```text
    /// C:\
    /// ├── Users\            (dir)
    /// │   ├── Report.PDF    2 MiB
    /// │   └── notes.txt     10 B, hidden
    /// ├── pagefile.sys      4 GiB, system
    /// └── Setup.exe         50 MiB
    /// ```
    fn index() -> Index {
        let caps = crate::service::scan::ntfs_caps("C:".to_string());
        let mut b = IndexBuilder::new(caps, CancellationToken::new());
        let day = 86_400 * 1_000_000_000i64;
        let _ = b.push_batch(&[
            entry(5, 5, "", 0, 0, EntryFlags::DIRECTORY),
            entry(16, 5, "Users", 0, 0, EntryFlags::DIRECTORY),
            entry(
                17,
                16,
                "Report.PDF",
                2 << 20,
                100 * day,
                EntryFlags::empty(),
            ),
            entry(18, 16, "notes.txt", 10, 200 * day, EntryFlags::HIDDEN),
            entry(
                19,
                5,
                "pagefile.sys",
                4 << 30,
                300 * day,
                EntryFlags::SYSTEM,
            ),
            entry(20, 5, "Setup.exe", 50 << 20, 400 * day, EntryFlags::empty()),
        ]);
        b.finish().0
    }

    fn run(index: &Index, raw: RawQuery) -> Vec<String> {
        let query = Query::parse(&raw);
        let fold = CaseFold::Simple;
        let hits = search_all(&[(index, &fold)], &query, &CancellationToken::new()).unwrap();
        hits.iter()
            .map(|&(_, id)| index.name(id).to_string())
            .collect()
    }

    fn q(text: &str) -> RawQuery {
        RawQuery {
            text: text.to_string(),
            include_hidden: true,
            include_system: true,
            ..RawQuery::default()
        }
    }

    #[test]
    fn substring_search_is_case_insensitive_by_default() {
        let index = index();
        assert_eq!(run(&index, q("report")), vec!["Report.PDF"]);
        assert_eq!(run(&index, q("REPORT")), vec!["Report.PDF"]);
    }

    #[test]
    fn case_sensitivity_can_be_forced() {
        let index = index();
        let mut raw = q("report");
        raw.case_sensitive = Some(true);
        assert!(run(&index, raw).is_empty(), "no lowercase `report` exists");
    }

    #[test]
    fn wildcards_and_or_terms_work() {
        let index = index();
        assert_eq!(run(&index, q("*.pdf")), vec!["Report.PDF"]);
        assert_eq!(run(&index, q("set*")), vec!["Setup.exe"]);
        assert_eq!(run(&index, q("n?tes.txt")), vec!["notes.txt"]);
        assert_eq!(run(&index, q(".pdf;.exe")), vec!["Report.PDF", "Setup.exe"]);
    }

    #[test]
    fn hidden_and_system_respect_the_toggles() {
        let index = index();
        let mut raw = q("");
        raw.include_hidden = false;
        raw.include_system = false;
        let names = run(&index, raw);
        assert!(!names.contains(&"notes.txt".to_string()));
        assert!(!names.contains(&"pagefile.sys".to_string()));
        assert!(names.contains(&"Setup.exe".to_string()));
    }

    #[test]
    fn size_and_date_filters_constrain() {
        let index = index();
        assert_eq!(run(&index, q("size:>1gb file:")), vec!["pagefile.sys"]);
        assert_eq!(
            run(&index, q("size:1mb..100mb")),
            vec!["Users", "Report.PDF", "Setup.exe"],
            "directories filter on their subtree total"
        );
        // Days 100..=200 in nanoseconds cover Report.PDF and notes.txt.
        assert_eq!(
            run(&index, q("dm:1970-04-10..1970-07-20")),
            vec!["Report.PDF", "notes.txt"]
        );
    }

    #[test]
    fn extension_filter_and_folder_kind() {
        let index = index();
        assert_eq!(
            run(&index, q("ext:pdf;txt")),
            vec!["Report.PDF", "notes.txt"]
        );
        assert_eq!(run(&index, q("folder:")), vec!["", "Users"]);
    }

    #[test]
    fn regex_field_matches_names() {
        let index = index();
        let mut raw = q("");
        raw.regex = r"^[ps]".to_string();
        assert_eq!(run(&index, raw), vec!["pagefile.sys", "Setup.exe"]);
    }

    #[test]
    fn path_scope_restricts_to_a_subtree() {
        let index = index();
        assert_eq!(
            run(&index, q("`C:\\Users`")),
            vec!["Users", "Report.PDF", "notes.txt"]
        );
        assert!(run(&index, q("`C:\\Missing` ")).is_empty());
        assert!(
            run(&index, q("`D:\\Users`")).is_empty(),
            "a scope on another volume excludes this one entirely"
        );
    }

    #[test]
    fn resolve_path_descends_case_insensitively() {
        let index = index();
        let fold = CaseFold::Simple;
        let id = resolve_path(&index, &fold, "c:\\users\\report.pdf").expect("resolves");
        assert_eq!(index.name(id), "Report.PDF");
        assert!(resolve_path(&index, &fold, "C:\\nope").is_none());
        assert!(resolve_path(&index, &fold, "D:\\Users").is_none());
    }

    #[test]
    fn an_interrupted_search_returns_none() {
        let index = index();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let fold = CaseFold::Simple;
        assert!(search_all(&[(&index, &fold)], &Query::default(), &cancel).is_none());
    }

    #[test]
    fn glob_matching_covers_the_corner_cases() {
        let fold = CaseFold::Simple;
        let pat = |s: &str| -> Vec<char> {
            let mut folded = String::new();
            fold.fold_str(s, &mut folded);
            folded.chars().collect()
        };
        assert!(glob_match("Report.PDF", &pat("r*.pdf"), Some(&fold)));
        assert!(glob_match("abc", &pat("a**c"), Some(&fold)));
        assert!(glob_match("abc", &pat("***"), Some(&fold)));
        assert!(!glob_match("abc", &pat("a?c?"), Some(&fold)));
        assert!(glob_match("", &pat("*"), Some(&fold)));
        assert!(!glob_match("", &pat("?"), Some(&fold)));
    }
}
