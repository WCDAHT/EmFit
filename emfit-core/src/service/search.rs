//! Executing a [`Query`] against one or more indexes.
//!
//! One linear pass over the flat node array per volume - no tree walk, no
//! per-name allocation. Pattern needles are folded **once per volume** (case
//! rules are per-volume, features.md sec 2); every candidate name is then folded
//! on the fly against the arena slice.
//!
//! Interruptible: the pass checks a [`CancellationToken`] every few thousand
//! nodes, so an in-flight search abandons quickly when the next keystroke
//! arrives (features.md sec 2 "incremental / debounced search").

use rayon::prelude::*;

use crate::model::caps::VolumeCaps;
use crate::model::entry::EntryFlags;
use crate::model::index::{Index, Node, NodeId};
use crate::service::filetype::{FileKind, extension_of};
use crate::service::fold::{CaseFold, Folder};
use crate::service::query::{ChildKind, Expr, KindFilter, NameTerm, Pattern, Query, Term};
use crate::service::task::CancellationToken;

/// One search result: which volume slot, which node.
pub type Hit = (u16, NodeId);

/// Nodes per parallel work unit - also the cancellation granularity.
const CHUNK: usize = 8192;

/// Search one volume, appending hits to `out` in node order.
///
/// The pass is chunk-parallel: node ids split into fixed ranges matched
/// across the rayon pool, each collecting hits locally; flattening the
/// chunks in range order preserves the node-order contract. (A profiled
/// single-threaded pass spent 93% of ~850ms in the matcher on a 3.2M-node
/// volume while every other core idled.)
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

    let len = index.len();
    let chunks: Vec<Vec<Hit>> = (0..len.div_ceil(CHUNK))
        .into_par_iter()
        .map(|chunk| {
            let mut local = Vec::new();
            if cancel.is_cancelled() {
                return local; // discarded below; just stop matching
            }
            // Reused across the chunk: a `path:` term needs a path per
            // candidate node, and one allocation per node is what the
            // arena exists to avoid.
            let mut path = String::new();
            let start = chunk * CHUNK;
            let end = (start + CHUNK).min(len);
            for i in start..end {
                let id = NodeId::new(i as u32);
                if prepared.matches(index, id, &mut path) {
                    local.push((volume_slot, id));
                }
            }
            local
        })
        .collect();

    if cancel.is_cancelled() {
        return false;
    }
    for chunk in chunks {
        out.extend(chunk);
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
    // `count:` caps what the user sees. It cannot cap the work: which nodes
    // survive is only known once every volume has been swept.
    if let Some(limit) = query.limit {
        out.truncate(limit);
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

/// A pattern with its needle folded for this volume, or kept as typed when
/// the term folds nothing.
enum Needle {
    Substring(String),
    Prefix(String),
    Suffix(String),
    Exact(String),
    Glob(Vec<char>),
    Regex(regex::Regex),
}

/// A compiled name condition: the needle, how it compares characters, and
/// what it compares them against.
struct NameOp {
    needle: Needle,
    folder: Folder,
    /// Match the full path instead of the name (`path:`).
    path: bool,
    /// The match must sit on word boundaries (`wholeword:`).
    whole_word: bool,
}

/// One compiled condition: [`Expr`] with every needle and extension already
/// folded for this volume.
enum Op {
    All,
    /// Nothing on this volume can match - a folder term naming a path the
    /// volume does not have.
    None,
    Name(NameOp),
    Ext(Vec<String>),
    Category(FileKind),
    Size(u64, u64),
    Modified(i64, i64),
    Created(i64, i64),
    Kind(KindFilter),
    Length {
        lo: u64,
        hi: u64,
        path: bool,
    },
    Depth(u64, u64),
    Children {
        what: ChildKind,
        lo: u64,
        hi: u64,
    },
    Child(Box<NameOp>),
    InFolder(NodeId),
    Empty,
    Root,
    Attributes(EntryFlags),
    Not(Box<Op>),
    And(Vec<Op>),
    Or(Vec<Op>),
}

impl Op {
    /// Rough evaluation cost, cheapest first.
    ///
    /// Ordering an AND by this is most of what keeps a four-term query near
    /// the speed of a one-term one: a size or date compare reads a field
    /// already in the cache line and rejects the node before any name is
    /// touched, and the name pass is 93% of the matcher (see `search_volume`).
    fn cost(&self) -> u8 {
        match self {
            Self::All | Self::None => 0,
            Self::Size(..)
            | Self::Modified(..)
            | Self::Created(..)
            | Self::Kind(_)
            | Self::Children { .. }
            | Self::Empty
            | Self::Root
            | Self::InFolder(_)
            | Self::Attributes(_) => 1,
            Self::Ext(_) | Self::Category(_) => 2,
            // A name length reads the name; a depth walks the parent
            // chain, one cache miss per level.
            Self::Length { path, .. } => {
                if *path {
                    6
                } else {
                    2
                }
            }
            Self::Depth(..) => 4,
            // Scans a folder's children: the most expensive thing here.
            Self::Child(_) => 7,
            // A path term builds a path; a regex and a glob both walk the
            // whole candidate. All three are worth deferring.
            Self::Name(op) if op.path => 6,
            Self::Name(op) => match op.needle {
                Needle::Regex(_) => 5,
                Needle::Glob(_) => 4,
                _ => 3,
            },
            Self::Not(inner) => inner.cost(),
            Self::And(parts) | Self::Or(parts) => parts.iter().map(Self::cost).max().unwrap_or(0),
        }
    }
}

/// Turns a [`Query`] expression into [`Op`]s for one volume.
struct Compiler<'a> {
    index: &'a Index,
    fold: &'a CaseFold,
    /// What a term without a `case:` modifier uses.
    case_sensitive: bool,
}

impl Compiler<'_> {
    fn compile(&self, expr: &Expr) -> Op {
        match expr {
            Expr::All => Op::All,
            Expr::Term(Term::Name(term)) => self.name_op(term),
            Expr::Term(Term::Ext(list)) => Op::Ext(list.iter().map(|e| self.prepare(e)).collect()),
            Expr::Term(Term::Category(kind)) => Op::Category(*kind),
            Expr::Term(Term::Size(lo, hi)) => Op::Size(*lo, *hi),
            Expr::Term(Term::Modified(lo, hi)) => Op::Modified(*lo, *hi),
            Expr::Term(Term::Created(lo, hi)) => Op::Created(*lo, *hi),
            Expr::Term(Term::Kind(kind)) => Op::Kind(*kind),
            Expr::Term(Term::Length { lo, hi, path }) => Op::Length {
                lo: *lo,
                hi: *hi,
                path: *path,
            },
            Expr::Term(Term::Depth(lo, hi)) => Op::Depth(*lo, *hi),
            Expr::Term(Term::Children { what, lo, hi }) => Op::Children {
                what: *what,
                lo: *lo,
                hi: *hi,
            },
            Expr::Term(Term::Child(term)) => match self.name_op(term) {
                Op::Name(op) => Op::Child(Box::new(op)),
                other => other,
            },
            // Resolved once per volume, so the test is one comparison per
            // node instead of a path walk.
            Expr::Term(Term::InFolder(path)) => match resolve_path(self.index, self.fold, path) {
                Some(id) => Op::InFolder(id),
                None => Op::None,
            },
            Expr::Term(Term::Empty) => Op::Empty,
            Expr::Term(Term::Root) => Op::Root,
            Expr::Term(Term::Attributes(flags)) => Op::Attributes(*flags),
            Expr::Not(inner) => Op::Not(Box::new(self.compile(inner))),
            Expr::And(parts) => Op::And(self.compile_sorted(parts)),
            Expr::Or(parts) => Op::Or(self.compile_sorted(parts)),
        }
    }

    /// Compile a branch cheapest-first, so short-circuiting pays off.
    fn compile_sorted(&self, parts: &[Expr]) -> Vec<Op> {
        let mut ops: Vec<Op> = parts.iter().map(|part| self.compile(part)).collect();
        ops.sort_by_key(Op::cost);
        ops
    }

    fn name_op(&self, term: &NameTerm) -> Op {
        let mods = term.mods;
        let case_sensitive = mods.case.unwrap_or(self.case_sensitive);
        let folder = Folder::new(self.fold, case_sensitive, mods.diacritics, mods.ascii);

        let mut folded = String::new();
        folder.fold_str(term.pattern.text(), &mut folded);

        let needle = match &term.pattern {
            Pattern::Substring(_) => Needle::Substring(folded),
            Pattern::Prefix(_) => Needle::Prefix(folded),
            Pattern::Suffix(_) => Needle::Suffix(folded),
            Pattern::Exact(_) => Needle::Exact(folded),
            Pattern::Glob(_) => Needle::Glob(folded.chars().collect()),
            Pattern::Regex(source) => match regex::RegexBuilder::new(source)
                .case_insensitive(!case_sensitive)
                .size_limit(1 << 20)
                .build()
            {
                Ok(re) => Needle::Regex(re),
                // Already reported as a warning when the query was parsed.
                Err(_) => return Op::All,
            },
        };

        Op::Name(NameOp {
            needle,
            folder,
            path: mods.path,
            whole_word: mods.whole_word,
        })
    }

    /// Fold text that is compared with the volume default rather than a
    /// term rule - the extension lists.
    fn prepare(&self, text: &str) -> String {
        if self.case_sensitive {
            return text.to_string();
        }
        let mut folded = String::new();
        self.fold.fold_str(text, &mut folded);
        folded
    }
}

/// One node, as the evaluator sees it. The path is built at most once, and
/// only if a `path:` term is actually reached.
struct Ctx<'a> {
    index: &'a Index,
    id: NodeId,
    node: &'a Node,
    name: &'a str,
    path: &'a mut String,
    path_ready: bool,
}

impl Ctx<'_> {
    /// How many direct children satisfy `keep`.
    fn direct(&self, keep: impl Fn(&Node) -> bool) -> u64 {
        self.index
            .children(self.id)
            .iter()
            .filter(|&&child| keep(self.index.node(child)))
            .count() as u64
    }

    fn haystack(&mut self, want_path: bool) -> &str {
        if !want_path {
            return self.name;
        }
        if !self.path_ready {
            self.index.write_path(self.id, self.path);
            self.path_ready = true;
        }
        self.path
    }
}

/// The query, specialized to one volume: needles folded, scope resolved.
struct Prepared<'q> {
    query: &'q Query,
    fold: CaseFold,
    /// True: compare bytes as-is; the fold above is unused.
    case_sensitive: bool,
    op: Op,
    /// Extensions from the filter field, ANDed with the expression.
    extensions: Vec<String>,
    scope: Option<NodeId>,
}

impl<'q> Prepared<'q> {
    /// `None` when a path scope is set but absent from this volume - the
    /// whole volume is out of scope.
    fn build(index: &Index, fold: &CaseFold, query: &'q Query) -> Option<Self> {
        let case_sensitive = query.case_sensitive.unwrap_or(index.caps().case_sensitive);

        let scope = match &query.path_scope {
            Some(path) => Some(resolve_path(index, fold, path)?),
            None => None,
        };

        let compiler = Compiler {
            index,
            fold,
            case_sensitive,
        };
        let op = compiler.compile(&query.expr);
        let extensions = query
            .extensions
            .iter()
            .map(|e| compiler.prepare(e))
            .collect();

        Some(Self {
            query,
            fold: fold.clone(),
            case_sensitive,
            op,
            extensions,
            scope,
        })
    }

    fn matches(&self, index: &Index, id: NodeId, path: &mut String) -> bool {
        let node = index.node(id);
        let flags = node.flags();
        let q = self.query;

        if !q.include_hidden && flags.contains(EntryFlags::HIDDEN) {
            return false;
        }
        if !q.include_system && flags.contains(EntryFlags::SYSTEM) {
            return false;
        }

        if let Some((lo, hi)) = q.size
            && !size_in(node, lo, hi)
        {
            return false;
        }
        if let Some((lo, hi)) = q.modified {
            let mtime = node.times().mtime;
            if mtime < lo || mtime > hi {
                return false;
            }
        }

        let name = index.name(id);

        if !self.extensions.is_empty()
            && (node.is_directory() || !self.ext_matches(name, &self.extensions))
        {
            return false;
        }

        let mut ctx = Ctx {
            index,
            id,
            node,
            name,
            path,
            path_ready: false,
        };
        if !self.eval(&self.op, &mut ctx) {
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

    fn eval(&self, op: &Op, ctx: &mut Ctx) -> bool {
        match op {
            Op::All => true,
            Op::None => false,
            Op::Size(lo, hi) => size_in(ctx.node, *lo, *hi),
            Op::Modified(lo, hi) => {
                let mtime = ctx.node.times().mtime;
                mtime >= *lo && mtime <= *hi
            }
            Op::Created(lo, hi) => {
                let crtime = ctx.node.times().crtime;
                crtime >= *lo && crtime <= *hi
            }
            Op::Length { lo, hi, path } => {
                let text = ctx.haystack(*path);
                let len = text.chars().count() as u64;
                len >= *lo && len <= *hi
            }
            Op::Depth(lo, hi) => {
                let depth = ctx.index.ancestors(ctx.id).count() as u64;
                depth >= *lo && depth <= *hi
            }
            Op::Children { what, lo, hi } => {
                if !ctx.node.is_directory() {
                    return false;
                }
                let node = ctx.node;
                let count = match what {
                    // The rollup counts a directory as one of its own
                    // folders and counts the whole subtree; children are
                    // the direct ones, so count them directly.
                    ChildKind::All => u64::from(node.child_count()),
                    ChildKind::Files => ctx.direct(|child| !child.is_directory()),
                    ChildKind::Folders => ctx.direct(|child| child.is_directory()),
                };
                count >= *lo && count <= *hi
            }
            Op::Child(op) => {
                ctx.node.is_directory()
                    && ctx
                        .index
                        .children(ctx.id)
                        .iter()
                        .any(|&child| name_matches(op, ctx.index.name(child)))
            }
            Op::InFolder(folder) => ctx.node.parent() == *folder && ctx.id != *folder,
            Op::Empty => ctx.node.is_directory() && ctx.node.child_count() == 0,
            Op::Root => ctx.id == ctx.index.root(),
            Op::Attributes(flags) => ctx.node.flags().contains(*flags),
            Op::Kind(kind) => match kind {
                KindFilter::Any => true,
                KindFilter::Files => !ctx.node.is_directory(),
                KindFilter::Folders => ctx.node.is_directory(),
            },
            Op::Ext(list) => !ctx.node.is_directory() && self.ext_matches(ctx.name, list),
            Op::Category(want) => FileKind::classify(ctx.name, ctx.node.is_directory()) == *want,
            Op::Name(name_op) => {
                let haystack = ctx.haystack(name_op.path);
                name_matches(name_op, haystack)
            }
            Op::Not(inner) => !self.eval(inner, ctx),
            Op::And(parts) => parts.iter().all(|part| self.eval(part, ctx)),
            Op::Or(parts) => parts.iter().any(|part| self.eval(part, ctx)),
        }
    }

    /// Whether the name's extension is in an already-folded list.
    fn ext_matches(&self, name: &str, list: &[String]) -> bool {
        let ext = extension_of(name);
        list.iter().any(|want| {
            if self.case_sensitive {
                ext == want
            } else {
                self.fold.eq(ext, want)
            }
        })
    }
}

/// Test one compiled name condition against a name or a path.
fn name_matches(op: &NameOp, haystack: &str) -> bool {
    let folder = &op.folder;
    match &op.needle {
        Needle::Regex(re) => re.is_match(haystack),
        Needle::Glob(pattern) => glob_match(haystack, pattern, folder),
        Needle::Exact(t) => folder.eq_prefolded(haystack, t),
        Needle::Substring(t) if op.whole_word => folder.contains_word_prefolded(haystack, t),
        Needle::Prefix(t) if op.whole_word => folder.starts_with_word_prefolded(haystack, t),
        Needle::Suffix(t) if op.whole_word => folder.ends_with_word_prefolded(haystack, t),
        // Folding nothing means the standard byte search, which is far faster
        // than walking characters.
        Needle::Substring(t) if folder.is_identity() => haystack.contains(t.as_str()),
        Needle::Prefix(t) if folder.is_identity() => haystack.starts_with(t.as_str()),
        Needle::Suffix(t) if folder.is_identity() => haystack.ends_with(t.as_str()),
        Needle::Substring(t) => folder.contains_prefolded(haystack, t),
        Needle::Prefix(t) => folder.starts_with_prefolded(haystack, t),
        Needle::Suffix(t) => folder.ends_with_prefolded(haystack, t),
    }
}

/// Size test shared by the filter field and the `size:` term. Directories
/// filter on their subtree total - "show me what is over a gigabyte" should
/// surface the folders too.
fn size_in(node: &Node, lo: u64, hi: u64) -> bool {
    let size = if node.is_directory() {
        node.total_size()
    } else {
        node.size()
    };
    size >= lo && size <= hi
}

/// The spans of `name` that made the query match - for the result list to
/// bold. Ranges are **UTF-16 code-unit offsets** (merged, non-overlapping,
/// in order), so the webview can slice its strings directly.
///
/// Covered: substring/prefix/suffix needles (first occurrence each, under
/// the same per-volume folding the matcher uses), the regex's first find,
/// and the extension when an `ext:` filter selected it. Globs contribute
/// nothing - they are anchored over the whole name, and bolding an entire
/// row is noise, not information.
///
/// Meant for the visible row window (~dozens of names), not the full
/// result set.
pub fn highlight_ranges(
    name: &str,
    fold: &CaseFold,
    volume_case_sensitive: bool,
    query: &Query,
) -> Vec<(u32, u32)> {
    let case_sensitive = query.case_sensitive.unwrap_or(volume_case_sensitive);

    // The name's chars with byte offsets, folded unless case-sensitive -
    // folding is 1:1 per char, so positions line up with the raw name.
    let chars: Vec<(usize, char)> = name
        .char_indices()
        .map(|(at, c)| (at, if case_sensitive { c } else { fold.fold(c) }))
        .collect();
    let needle_chars = |text: &str| -> Vec<char> {
        if case_sensitive {
            text.chars().collect()
        } else {
            text.chars().map(|c| fold.fold(c)).collect()
        }
    };
    let end_byte = |char_at: usize| chars.get(char_at).map_or(name.len(), |&(b, _)| b);
    let run_matches = |start: usize, needle: &[char]| {
        start + needle.len() <= chars.len()
            && chars[start..start + needle.len()]
                .iter()
                .map(|&(_, c)| c)
                .eq(needle.iter().copied())
    };

    // Only what could make a row match: a negated term is why *other*
    // rows are absent, so highlighting it would be a lie.
    let mut patterns: Vec<&Pattern> = Vec::new();
    let mut extensions: Vec<&str> = Vec::new();
    query.expr.positives(&mut patterns, &mut extensions);
    extensions.extend(query.extensions.iter().map(String::as_str));

    let mut byte_ranges: Vec<(usize, usize)> = Vec::new();
    for pattern in patterns {
        let (anchored_start, text) = match pattern {
            Pattern::Substring(t) => (None, t),
            Pattern::Prefix(t) => (Some(0), t),
            Pattern::Suffix(t) => {
                let n = needle_chars(t);
                (Some(chars.len().saturating_sub(n.len())), t)
            }
            // Anchored over the whole name: bolding an entire row is noise.
            Pattern::Glob(_) | Pattern::Exact(_) | Pattern::Regex(_) => continue,
        };
        let needle = needle_chars(text);
        if needle.is_empty() {
            continue;
        }
        let starts: Box<dyn Iterator<Item = usize>> = match anchored_start {
            Some(at) => Box::new(std::iter::once(at)),
            None => Box::new(0..chars.len()),
        };
        for start in starts {
            if run_matches(start, &needle) {
                byte_ranges.push((end_byte(start), end_byte(start + needle.len())));
                break;
            }
        }
    }

    if let Some(re) = &query.regex
        && let Some(found) = re.find(name)
    {
        byte_ranges.push((found.start(), found.end()));
    }

    if !extensions.is_empty() {
        let ext = extension_of(name);
        let selected = !ext.is_empty()
            && extensions.iter().any(|want| {
                if case_sensitive {
                    ext == *want
                } else {
                    fold.eq(ext, want)
                }
            });
        if selected {
            byte_ranges.push((name.len() - ext.len(), name.len()));
        }
    }

    // Merge overlaps, then convert byte offsets to UTF-16 units in one walk.
    byte_ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in byte_ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut out = Vec::with_capacity(merged.len());
    let mut byte_at = 0usize;
    let mut utf16_at = 0u32;
    for (start, end) in merged {
        utf16_at += name[byte_at..start].encode_utf16().count() as u32;
        let u16_start = utf16_at;
        utf16_at += name[start..end].encode_utf16().count() as u32;
        byte_at = end;
        out.push((u16_start, utf16_at));
    }
    out
}

/// Wildcard match over the whole name: `*` any run, `?` exactly one char.
/// The pattern is prefolded; the name folds on the fly.
fn glob_match(name: &str, pattern: &[char], fold: &Folder) -> bool {
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
                    let got = fold.fold(got);
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
    /// +-- Users\            (dir)
    /// |   +-- Report.PDF    2 MiB
    /// |   +-- notes.txt     10 B, hidden
    /// +-- pagefile.sys      4 GiB, system
    /// +-- Setup.exe         50 MiB
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
    fn highlight_ranges_cover_what_matched() {
        let fold = CaseFold::Simple;
        let parse = |raw: RawQuery| crate::service::query::Query::parse(&raw);

        // Substring, folded: "port" inside "Report.PDF".
        let query = parse(q("port"));
        assert_eq!(
            highlight_ranges("Report.PDF", &fold, false, &query),
            vec![(2, 6)]
        );

        // An ext: filter bolds the extension it selected.
        let query = parse(RawQuery {
            extensions: "pdf".to_string(),
            ..q("")
        });
        assert_eq!(
            highlight_ranges("Report.PDF", &fold, false, &query),
            vec![(7, 10)]
        );

        // `*.pdf` parses down to a suffix needle - the suffix highlights.
        let query = parse(q("*.pdf"));
        assert_eq!(
            highlight_ranges("Report.PDF", &fold, false, &query),
            vec![(6, 10)]
        );

        // True globs are anchored over the whole name: no highlight.
        let query = parse(q("r?port*"));
        assert_eq!(highlight_ranges("Report.PDF", &fold, false, &query), vec![]);

        // Ranges are UTF-16 units: e-acute is two UTF-8 bytes but one unit,
        // so "sum" in "resume.pdf" (accented) starts at unit 2, not byte 3.
        let query = parse(q("sum"));
        assert_eq!(
            highlight_ranges("r\u{e9}sum\u{e9}.pdf", &fold, false, &query),
            vec![(2, 5)]
        );

        // Overlapping contributions (semicolon multi-pattern) merge.
        let query = parse(q("repo;port"));
        assert_eq!(
            highlight_ranges("Report.PDF", &fold, false, &query),
            vec![(0, 6)]
        );

        // A name that only other rows matched gets nothing.
        let query = parse(q("zzz"));
        assert_eq!(highlight_ranges("Report.PDF", &fold, false, &query), vec![]);
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
    fn boolean_operators_combine_terms() {
        let index = index();
        // Space is AND.
        assert_eq!(run(&index, q("report .pdf")), vec!["Report.PDF"]);
        assert!(run(&index, q("report .exe")).is_empty());
        // `|` is OR, and binds looser than the space.
        assert_eq!(
            run(&index, q("report | setup")),
            vec!["Report.PDF", "Setup.exe"]
        );
        assert_eq!(
            run(&index, q("report | setup .exe")),
            vec!["Report.PDF", "Setup.exe"],
            "`a | b c` is `a OR (b AND c)`"
        );
        // `<>` overrides that precedence.
        assert_eq!(run(&index, q("<report | setup> .exe")), vec!["Setup.exe"]);
        // `!` excludes.
        assert_eq!(run(&index, q("t !notes")), vec!["Report.PDF", "Setup.exe"]);
    }

    #[test]
    fn quotes_macros_and_type_macros_run() {
        let index = index();
        assert_eq!(run(&index, q("\"Report.PDF\"")), vec!["Report.PDF"]);
        assert!(
            run(&index, q("\"*.pdf\"")).is_empty(),
            "a quoted phrase is literal: the star is a star"
        );
        // `#46:` is a literal dot.
        assert_eq!(run(&index, q("#46:pdf")), vec!["Report.PDF"]);
        assert_eq!(
            run(&index, q("exe:")),
            vec!["pagefile.sys", "Setup.exe"],
            "the type macro follows the file-type table"
        );
    }

    #[test]
    fn modifiers_change_what_a_term_matches() {
        let index = index();

        assert_eq!(run(&index, q("case:Report")), vec!["Report.PDF"]);
        assert!(run(&index, q("case:report")).is_empty());

        // Whole words only: `not` is inside `notes`, so it stops matching.
        assert_eq!(run(&index, q("not")), vec!["notes.txt"]);
        assert!(run(&index, q("ww:not")).is_empty());

        assert_eq!(run(&index, q("wfn:notes.txt")), vec!["notes.txt"]);
        assert!(run(&index, q("wfn:notes")).is_empty());

        assert_eq!(run(&index, q("startwith:pa")), vec!["pagefile.sys"]);
        assert_eq!(run(&index, q("endwith:.exe")), vec!["Setup.exe"]);

        assert_eq!(run(&index, q("regex:^set")), vec!["Setup.exe"]);
        assert!(run(&index, q("case:regex:^set")).is_empty());

        // A literal star finds nothing; the wildcard finds the file.
        assert_eq!(run(&index, q("*.pdf")), vec!["Report.PDF"]);
        assert!(run(&index, q("nowildcards:*.pdf")).is_empty());
    }

    #[test]
    fn path_terms_match_the_whole_path() {
        let index = index();
        assert_eq!(
            run(&index, q("path:Users")),
            vec!["Users", "Report.PDF", "notes.txt"],
            "the folder and everything under it"
        );
        assert!(
            run(&index, q("Users file:")).is_empty(),
            "without path:, only the name is searched"
        );
        assert_eq!(
            run(&index, q("path:C:\\Users\\Report.PDF")),
            vec!["Report.PDF"]
        );
    }

    #[test]
    fn structure_functions_read_the_tree() {
        let index = index();
        assert_eq!(run(&index, q("root:")), vec![""]);
        assert_eq!(
            run(&index, q("parents:1")),
            vec!["Users", "pagefile.sys", "Setup.exe"]
        );
        assert_eq!(run(&index, q("childcount:2")), vec!["Users"]);
        assert_eq!(
            run(&index, q("childfilecount:2")),
            vec!["", "Users"],
            "the root holds two files as well"
        );
        assert_eq!(run(&index, q("childfoldercount:1")), vec![""]);
        assert_eq!(run(&index, q("child:notes.txt")), vec!["Users"]);
        assert_eq!(
            run(&index, q("infolder:C:\\Users")),
            vec!["Report.PDF", "notes.txt"]
        );
        assert!(
            run(&index, q("empty:")).is_empty(),
            "no folder here is empty"
        );
        assert!(
            run(&index, q("infolder:C:\\Nowhere")).is_empty(),
            "a folder this volume does not have matches nothing"
        );
    }

    #[test]
    fn metadata_functions_filter_on_the_node() {
        let index = index();
        assert_eq!(run(&index, q("len:9")), vec!["notes.txt", "Setup.exe"]);
        assert_eq!(run(&index, q("attrib:H")), vec!["notes.txt"]);
        assert_eq!(run(&index, q("attrib:D")), vec!["", "Users"]);
        assert_eq!(run(&index, q("type:folder")), vec!["", "Users"]);
        assert_eq!(
            run(&index, q("size:large")),
            vec!["Users", "Report.PDF"],
            "1 MB to 16 MB"
        );
    }

    #[test]
    fn count_caps_the_results() {
        let index = index();
        assert_eq!(run(&index, q("count:2")), vec!["", "Users"]);
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
        let fold = Folder::new(&CaseFold::Simple, false, false, false);
        let pat = |s: &str| -> Vec<char> {
            let mut folded = String::new();
            fold.fold_str(s, &mut folded);
            folded.chars().collect()
        };
        assert!(glob_match("Report.PDF", &pat("r*.pdf"), &fold));
        assert!(glob_match("abc", &pat("a**c"), &fold));
        assert!(glob_match("abc", &pat("***"), &fold));
        assert!(!glob_match("abc", &pat("a?c?"), &fold));
        assert!(glob_match("", &pat("*"), &fold));
        assert!(!glob_match("", &pat("?"), &fold));
    }
}
