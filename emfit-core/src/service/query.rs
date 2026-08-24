//! The search query: what the user typed, parsed into something executable.
//!
//! The text box holds a boolean expression - space is AND, `|` and `;` are OR,
//! `!` negates, `<` `>` group - whose leaves are name patterns and the inline
//! `ext:` `size:` `dm:` `folder:` `file:` operators. [`syntax`] turns text into
//! that [`Expr`]; this module owns the tree's shape and the value grammars
//! (sizes, dates, extension lists) its leaves are built from.
//!
//! The separate filter fields the UI panel carries (regex, size, date,
//! extensions) are **not** part of the expression: they are always ANDed with
//! it, because a form field has no place in a boolean the user is typing.
//!
//! Parsing is **lenient**: a half-typed operator (`size:>10` mid-keystroke)
//! must not make the whole query error out and flash the list empty. Anything
//! unintelligible is skipped and reported in [`Query::warnings`]; the rest of
//! the query still runs.
//!
//! A `Query` is volume-independent - patterns are stored as typed; the
//! searcher folds them per volume, because case rules are a per-volume
//! property ([`CaseFold`]).
//!
//! [`syntax`]: crate::service::syntax
//! [`CaseFold`]: crate::service::fold::CaseFold

use crate::model::entry::EntryFlags;
use crate::service::filetype::FileKind;
use crate::service::{syntax, values};

/// Files, folders, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KindFilter {
    #[default]
    Any,
    Files,
    Folders,
}

/// One search pattern, classified once at parse time so the hot loop never
/// re-inspects the pattern text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// Plain text, or `*text*`: match anywhere in the name.
    Substring(String),
    /// `text*`: match at the start.
    Prefix(String),
    /// `*text` (the `*.ext` idiom): match at the end.
    Suffix(String),
    /// Anything else containing `*` or `?`: full wildcard match over the
    /// whole name.
    Glob(String),
    /// The whole name, exactly - `wholefilename:`.
    Exact(String),
    /// A regular expression over the name - `regex:`. Held as source text
    /// because the matcher compiles it per volume, with that volume's case
    /// rule; validity is checked at parse time.
    Regex(String),
}

impl Pattern {
    /// Classify one term. `*` matches any run of characters, `?` exactly one.
    pub fn classify(term: &str) -> Self {
        let has_question = term.contains('?');
        let stars = term.matches('*').count();

        if !has_question && stars == 0 {
            return Self::Substring(term.to_string());
        }
        if !has_question {
            let inner = term.trim_matches('*');
            if !inner.contains('*') {
                // Only leading/trailing stars: the three cheap shapes.
                return match (term.starts_with('*'), term.ends_with('*')) {
                    (true, true) => Self::Substring(inner.to_string()),
                    (true, false) => Self::Suffix(inner.to_string()),
                    (false, true) => Self::Prefix(inner.to_string()),
                    (false, false) => unreachable!("stars > 0"),
                };
            }
        }
        Self::Glob(term.to_string())
    }

    /// The pattern's text, for per-volume folding.
    pub fn text(&self) -> &str {
        match self {
            Self::Substring(t)
            | Self::Prefix(t)
            | Self::Suffix(t)
            | Self::Glob(t)
            | Self::Exact(t)
            | Self::Regex(t) => t,
        }
    }
}

/// Where a term's text has to sit in the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    #[default]
    Anywhere,
    /// `startwith:`
    Start,
    /// `endwith:`
    End,
}

/// The modifiers in force for one term.
///
/// A modifier written with a value (`case:Report`) applies to that term; one
/// written bare (`case:`) applies to every term after it in its group. Both
/// end up here, resolved, before the matcher sees anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    /// `case:` / `nocase:`. `None` leaves it to the query default, which is
    /// the volume's rule ([`VolumeCaps::case_sensitive`]).
    ///
    /// [`VolumeCaps::case_sensitive`]: crate::model::caps::VolumeCaps::case_sensitive
    pub case: Option<bool>,
    /// `diacritics:` - accented letters stop matching their plain forms.
    pub diacritics: bool,
    /// `wholeword:` / `ww:`
    pub whole_word: bool,
    /// `wholefilename:` / `wfn:`
    pub whole_name: bool,
    /// `path:` - match the full path instead of the name.
    pub path: bool,
    /// `regex:`
    pub regex: bool,
    /// `wildcards:` / `nowildcards:`. `None` is the default: `*` and `?` are
    /// wildcards when they appear.
    pub wildcards: Option<bool>,
    /// `ascii:` / `utf8:` - fast ASCII-only case comparison.
    pub ascii: bool,
    /// `startwith:` / `endwith:`
    pub anchor: Anchor,
}

/// A name condition and the modifiers that shape it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameTerm {
    pub pattern: Pattern,
    pub mods: Mods,
}

impl NameTerm {
    /// Build the pattern one term's text and modifiers call for.
    pub fn new(text: &str, mods: Mods) -> Self {
        let pattern = if mods.regex {
            Pattern::Regex(text.to_string())
        } else {
            let wild = mods.wildcards.unwrap_or_else(|| text.contains(['*', '?']));
            match (mods.whole_name, mods.anchor, wild) {
                (true, _, true) => Pattern::Glob(text.to_string()),
                (true, _, false) => Pattern::Exact(text.to_string()),
                // An anchored wildcard term is still a glob, just with the
                // free end spelled out.
                (false, Anchor::Start, true) => Pattern::Glob(format!("{text}*")),
                (false, Anchor::End, true) => Pattern::Glob(format!("*{text}")),
                (false, Anchor::Start, false) => Pattern::Prefix(text.to_string()),
                (false, Anchor::End, false) => Pattern::Suffix(text.to_string()),
                (false, Anchor::Anywhere, true) => Pattern::classify(text),
                (false, Anchor::Anywhere, false) => Pattern::Substring(text.to_string()),
            }
        };
        Self { pattern, mods }
    }
}

/// One condition on a node - the leaf of an [`Expr`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    /// The file name matches this pattern.
    Name(NameTerm),
    /// The extension is in this list (stored without the dot).
    Ext(Vec<String>),
    /// The file-type macros: `audio:`, `exe:`, `pic:`.
    Category(FileKind),
    /// Inclusive logical-size range in bytes.
    Size(u64, u64),
    /// Inclusive mtime range, nanoseconds since the Unix epoch.
    Modified(i64, i64),
    /// Inclusive creation-time range, same units.
    Created(i64, i64),
    /// Files only or folders only.
    Kind(KindFilter),
    /// `len:` - name length in characters, or path length under `path:`.
    Length { lo: u64, hi: u64, path: bool },
    /// `parents:` - how many folders sit above this one.
    Depth(u64, u64),
    /// `childcount:` and its files-only and folders-only variants.
    Children { what: ChildKind, lo: u64, hi: u64 },
    /// `child:` - a folder holding a child whose name matches.
    Child(Box<NameTerm>),
    /// `infolder:` / `parent:` - one folder's direct contents.
    InFolder(String),
    /// `empty:` - a folder with nothing in it.
    Empty,
    /// `root:` - a node with no parent folders.
    Root,
    /// `attrib:` - every one of these bits set.
    Attributes(EntryFlags),
}

/// Which children `childcount:` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    All,
    Files,
    Folders,
}

/// The typed query, as a boolean tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Expr {
    /// Constrains nothing. Also what an empty query parses to.
    #[default]
    All,
    Term(Term),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

impl Expr {
    /// True when this half of the query constrains nothing.
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// The name patterns and extension lists that can make a node *match*.
    ///
    /// Anything under a negation is skipped: a row is not in the results
    /// because of what it failed to contain, so highlighting it would lie.
    /// Path terms are skipped for the same reason - what they matched is not
    /// in the name. Used by the result list, never by the matcher.
    pub fn positives<'a>(&'a self, patterns: &mut Vec<&'a Pattern>, extensions: &mut Vec<&'a str>) {
        match self {
            Self::All | Self::Not(_) => {}
            Self::Term(Term::Name(term)) if !term.mods.path => patterns.push(&term.pattern),
            Self::Term(Term::Ext(list)) => extensions.extend(list.iter().map(String::as_str)),
            Self::Term(_) => {}
            Self::And(parts) | Self::Or(parts) => {
                for part in parts {
                    part.positives(patterns, extensions);
                }
            }
        }
    }
}

/// AND of `parts`, dropping the ones that constrain nothing.
pub(crate) fn and_of(mut parts: Vec<Expr>) -> Expr {
    parts.retain(|part| !part.is_all());
    match parts.len() {
        0 => Expr::All,
        1 => parts.pop().expect("length checked"),
        _ => Expr::And(parts),
    }
}

/// OR of `parts`. One unconstrained branch makes the whole thing unconstrained.
pub(crate) fn or_of(mut parts: Vec<Expr>) -> Expr {
    if parts.iter().any(Expr::is_all) {
        return Expr::All;
    }
    match parts.len() {
        0 => Expr::All,
        1 => parts.pop().expect("length checked"),
        _ => Expr::Or(parts),
    }
}

/// Raw filter strings as the UI holds them. The shell passes this across IPC
/// verbatim; [`Query::parse`] is the single place they become semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawQuery {
    /// The main search box: the whole expression grammar, plus an optional
    /// backtick-quoted path scope.
    pub text: String,
    /// The separate regex field (features.md sec 2). Applied to the name, ANDed
    /// with everything else.
    pub regex: String,
    /// Size filter field: `>10MB`, `<1GB`, `10MB..1GB`.
    pub size: String,
    /// Date-modified filter field: `2024-01-01`, `>2024-01-01`,
    /// `2024-01-01..2024-06-30`.
    pub modified: String,
    /// Extension filter field: `pdf;docx`.
    pub extensions: String,
    /// The active preset's search string (`ext:mp3;flac`), from `Filters.csv`.
    pub preset: String,
    pub include_hidden: bool,
    pub include_system: bool,
    /// `Some` overrides the volume default ([`VolumeCaps::case_sensitive`]).
    ///
    /// [`VolumeCaps::case_sensitive`]: crate::model::caps::VolumeCaps::case_sensitive
    pub case_sensitive: Option<bool>,
}

/// A parsed, executable query.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// What the text box says.
    pub expr: Expr,
    /// Extensions from the filter *field* (without the dot), ANDed with `expr`.
    pub extensions: Vec<String>,
    /// Inclusive logical-size range in bytes, from the filter field.
    pub size: Option<(u64, u64)>,
    /// Inclusive mtime range in nanoseconds, from the filter field.
    pub modified: Option<(i64, i64)>,
    /// Restrict results to this subtree, e.g. `C:\Users`.
    pub path_scope: Option<String>,
    pub regex: Option<regex::Regex>,
    pub include_hidden: bool,
    pub include_system: bool,
    pub case_sensitive: Option<bool>,
    /// `count:` - a cap on how many results to keep. Not a condition on
    /// any one node, so it is applied after the search, not inside it.
    pub limit: Option<usize>,
    /// Parts of the input that were skipped, for the UI to surface.
    pub warnings: Vec<String>,
}

impl Query {
    /// True when nothing constrains the result: the browse-everything state.
    pub fn is_match_all(&self) -> bool {
        self.expr.is_all()
            && self.extensions.is_empty()
            && self.size.is_none()
            && self.modified.is_none()
            && self.path_scope.is_none()
            && self.regex.is_none()
    }

    /// Parse the UI's raw strings. Never fails - see the module docs.
    pub fn parse(raw: &RawQuery) -> Self {
        let mut q = Query {
            include_hidden: raw.include_hidden,
            include_system: raw.include_system,
            case_sensitive: raw.case_sensitive,
            ..Query::default()
        };

        // The preset contributes operators exactly as if they were typed.
        let mut text = String::new();
        if !raw.preset.trim().is_empty() {
            text.push_str(raw.preset.trim());
            text.push(' ');
        }
        text.push_str(&raw.text);

        // Backtick path scope: `C:\Users` restricts to that subtree.
        let text = extract_scope(&text, &mut q);
        let parsed = syntax::parse_expr(&text, &mut q.warnings);
        q.expr = parsed.expr;
        q.limit = parsed.limit;

        // The separate filter fields, same grammars as the inline operators.
        if !raw.extensions.trim().is_empty() {
            q.extensions
                .extend(split_list(&raw.extensions).map(str::to_string));
        }
        if !raw.size.trim().is_empty() {
            match values::size(raw.size.trim()) {
                Some(range) => q.size = Some(range),
                None => q
                    .warnings
                    .push(format!("unrecognized size filter `{}`", raw.size.trim())),
            }
        }
        if !raw.modified.trim().is_empty() {
            match values::date(raw.modified.trim()) {
                Some(range) => q.modified = Some(range),
                None => q.warnings.push(format!(
                    "unrecognized date filter `{}`",
                    raw.modified.trim()
                )),
            }
        }
        if !raw.regex.trim().is_empty() {
            match regex::RegexBuilder::new(raw.regex.trim())
                .case_insensitive(!raw.case_sensitive.unwrap_or(false))
                .size_limit(1 << 20)
                .build()
            {
                Ok(re) => q.regex = Some(re),
                Err(e) => q.warnings.push(format!("regex: {e}")),
            }
        }

        q.extensions.dedup();
        q
    }
}

/// Pull a backtick-quoted scope out of the text, returning what remains.
fn extract_scope(text: &str, q: &mut Query) -> String {
    let Some(open) = text.find('`') else {
        return text.to_string();
    };
    let after = &text[open + 1..];
    let (scope, rest) = match after.find('`') {
        Some(close) => (&after[..close], &after[close + 1..]),
        None => (after, ""),
    };
    let scope = scope.trim().trim_end_matches(['\\', '/']);
    if !scope.is_empty() {
        q.path_scope = Some(scope.to_string());
    }
    format!("{} {}", &text[..open], rest)
}

/// Case-insensitive operator prefix match (`EXT:` works too).
///
/// Compared as BYTES, not a `str` slice: `token[..op.len()]` panics when a
/// multi-byte character straddles the cut (a Cyrillic prefix took down the
/// search worker this way). Operators are pure ASCII, so a byte match also
/// guarantees the split point is a char boundary.
pub(crate) fn strip_operator<'a>(token: &'a str, op: &str) -> Option<&'a str> {
    (token.len() >= op.len() && token.as_bytes()[..op.len()].eq_ignore_ascii_case(op.as_bytes()))
        .then(|| &token[op.len()..])
}

/// Split a `;`-separated list, dropping empties and stray dots.
pub(crate) fn split_list(list: &str) -> impl Iterator<Item = &str> {
    list.split(';')
        .map(|part| part.trim().trim_start_matches('.'))
        .filter(|part| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_text(text: &str) -> Query {
        Query::parse(&RawQuery {
            text: text.to_string(),
            ..RawQuery::default()
        })
    }

    fn name(text: &str) -> Expr {
        Expr::Term(Term::Name(NameTerm::new(text, Mods::default())))
    }

    /// What a quoted phrase parses to: literal, wildcards off.
    fn literal(text: &str) -> Expr {
        let mods = Mods {
            wildcards: Some(false),
            ..Mods::default()
        };
        Expr::Term(Term::Name(NameTerm::new(text, mods)))
    }

    #[test]
    fn plain_text_is_a_substring_pattern() {
        let q = parse_text("report");
        assert_eq!(q.expr, name("report"));
        assert!(q.warnings.is_empty());
        assert!(!q.is_match_all());
    }

    #[test]
    fn non_ascii_tokens_parse_without_panicking() {
        // Regression: the operator-prefix check sliced `token[..op.len()]`,
        // which panics when the cut lands inside a multi-byte char - every
        // Cyrillic keystroke killed the search worker.
        // Cyrillic prefixes, a Japanese word, and one accented letter.
        for text in [
            "\u{43f}\u{440}",
            "\u{43f}\u{440}\u{438}",
            "\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}",
            "\u{65e5}\u{672c}\u{8a9e}",
            "\u{e9}",
            "\u{43f}\u{440} ext:pdf",
        ] {
            assert!(!parse_text(text).expr.is_all(), "`{text}` should constrain");
        }
        let q = parse_text("\u{43f}\u{440} ext:pdf");
        assert_eq!(
            q.expr,
            Expr::And(vec![
                name("\u{43f}\u{440}"),
                Expr::Term(Term::Ext(vec!["pdf".to_string()])),
            ])
        );
    }

    #[test]
    fn an_empty_query_matches_everything() {
        assert!(parse_text("").is_match_all());
        assert!(parse_text("   ").is_match_all());
    }

    #[test]
    fn wildcards_classify_into_the_cheap_shapes() {
        assert_eq!(
            Pattern::classify("*.pdf"),
            Pattern::Suffix(".pdf".to_string())
        );
        assert_eq!(
            Pattern::classify("draft*"),
            Pattern::Prefix("draft".to_string())
        );
        assert_eq!(
            Pattern::classify("*2024*"),
            Pattern::Substring("2024".to_string())
        );
        assert_eq!(
            Pattern::classify("a*b?c"),
            Pattern::Glob("a*b?c".to_string())
        );
    }

    #[test]
    fn spaces_and_bars_are_and_and_or() {
        assert_eq!(
            parse_text("draft report").expr,
            Expr::And(vec![name("draft"), name("report")])
        );
        assert_eq!(
            parse_text("draft | report").expr,
            Expr::Or(vec![name("draft"), name("report")])
        );
        // AND binds tighter than OR.
        assert_eq!(
            parse_text("a b | c").expr,
            Expr::Or(vec![Expr::And(vec![name("a"), name("b")]), name("c")])
        );
    }

    #[test]
    fn semicolons_or_patterns_together() {
        assert_eq!(
            parse_text(".pdf;.docx").expr,
            Expr::Or(vec![name(".pdf"), name(".docx")])
        );
    }

    #[test]
    fn bang_negates_and_angles_group() {
        assert_eq!(
            parse_text("!draft").expr,
            Expr::Not(Box::new(name("draft")))
        );
        assert_eq!(
            parse_text("!<a | b>").expr,
            Expr::Not(Box::new(Expr::Or(vec![name("a"), name("b")])))
        );
        // Grouping changes precedence.
        assert_eq!(
            parse_text("<a | b> c").expr,
            Expr::And(vec![Expr::Or(vec![name("a"), name("b")]), name("c")])
        );
    }

    #[test]
    fn quotes_make_an_exact_phrase() {
        // The space no longer separates terms, and `*` is a literal star.
        assert_eq!(
            parse_text("\"annual report\"").expr,
            literal("annual report")
        );
        assert_eq!(parse_text("\"a*b\"").expr, literal("a*b"));
    }

    #[test]
    fn a_phrase_takes_its_modifiers_from_before_the_quote() {
        let cased = Mods {
            case: Some(true),
            wildcards: Some(false),
            ..Mods::default()
        };
        assert_eq!(
            parse_text("case:\"annual report\"").expr,
            Expr::Term(Term::Name(NameTerm::new("annual report", cased)))
        );
        // Inside the quotes it is text, not a modifier.
        assert_eq!(parse_text("\"case:foo\"").expr, literal("case:foo"));
    }

    #[test]
    fn quotes_carry_a_function_value_with_spaces() {
        assert_eq!(
            parse_text("infolder:\"C:\\Program Files\"").expr,
            Expr::Term(Term::InFolder("C:\\Program Files".to_string()))
        );
        // The modifiers before it still apply to the value.
        let q = parse_text("case:child:\"annual report\"");
        let Expr::Term(Term::Child(term)) = &q.expr else {
            panic!("expected a child term, got {:?}", q.expr);
        };
        assert_eq!(
            term.pattern,
            Pattern::Substring("annual report".to_string())
        );
        assert_eq!(term.mods.case, Some(true));
    }

    #[test]
    fn macros_expand_to_literal_characters() {
        assert_eq!(parse_text("quot:").expr, name("\""));
        assert_eq!(parse_text("a#38:b").expr, name("a&b"));
        assert_eq!(parse_text("#x41:").expr, name("A"));
        // `lt:` and `gt:` are how you search for a literal angle bracket,
        // which the grammar would otherwise read as grouping.
        assert_eq!(parse_text("lt:gt:").expr, name("<>"));
    }

    #[test]
    fn a_modifier_with_a_value_applies_to_its_term_only() {
        let q = parse_text("case:Report draft");
        let Expr::And(parts) = &q.expr else {
            panic!("expected an AND, got {:?}", q.expr);
        };
        assert_eq!(
            parts[0],
            Expr::Term(Term::Name(NameTerm::new(
                "Report",
                Mods {
                    case: Some(true),
                    ..Mods::default()
                }
            )))
        );
        assert_eq!(parts[1], name("draft"), "the next term is unaffected");
    }

    #[test]
    fn a_bare_modifier_governs_the_rest_of_its_group() {
        let case_on = Mods {
            case: Some(true),
            ..Mods::default()
        };
        let cased = |text: &str| Expr::Term(Term::Name(NameTerm::new(text, case_on)));

        assert_eq!(
            parse_text("case: a b").expr,
            Expr::And(vec![cased("a"), cased("b")])
        );
        // ...and dies with the group it was written in.
        assert_eq!(
            parse_text("<case: a> b").expr,
            Expr::And(vec![cased("a"), name("b")])
        );
        // `no` turns any of them back off.
        assert_eq!(
            parse_text("case: a nocase: b").expr,
            Expr::And(vec![
                cased("a"),
                Expr::Term(Term::Name(NameTerm::new(
                    "b",
                    Mods {
                        case: Some(false),
                        ..Mods::default()
                    }
                ))),
            ])
        );
    }

    #[test]
    fn modifiers_shape_the_pattern() {
        let pattern = |text: &str| match parse_text(text).expr {
            Expr::Term(Term::Name(term)) => term.pattern,
            other => panic!("expected one name term, got {other:?}"),
        };
        assert_eq!(pattern("startwith:draft"), Pattern::Prefix("draft".into()));
        assert_eq!(pattern("endwith:.log"), Pattern::Suffix(".log".into()));
        assert_eq!(pattern("wfn:notes.txt"), Pattern::Exact("notes.txt".into()));
        assert_eq!(pattern("regex:^a.c$"), Pattern::Regex("^a.c$".into()));
        // Wildcards can be forced off, which makes the star a literal star.
        assert_eq!(
            pattern("nowildcards:*.pdf"),
            Pattern::Substring("*.pdf".into())
        );
        // Modifiers stack.
        assert_eq!(pattern("case:wfn:Report"), Pattern::Exact("Report".into()));
    }

    #[test]
    fn an_invalid_term_regex_warns_and_is_dropped() {
        let q = parse_text("regex:(unclosed report");
        assert_eq!(q.warnings.len(), 1);
        assert_eq!(q.expr, name("report"));
    }

    #[test]
    fn type_macros_select_a_category() {
        assert_eq!(
            parse_text("audio:").expr,
            Expr::Term(Term::Category(FileKind::Audio))
        );
        assert_eq!(
            parse_text("EXE:").expr,
            Expr::Term(Term::Category(FileKind::Executable))
        );
    }

    #[test]
    fn a_half_typed_query_still_parses() {
        // Every prefix of a real query must produce something runnable.
        for text in ["<", "<a", "<a |", "!", "a !", ">", "\"unclosed"] {
            let _ = parse_text(text);
        }
        assert_eq!(parse_text("<a").expr, name("a"));
        assert_eq!(parse_text("a !").expr, name("a"));
        assert_eq!(parse_text("a |").expr, name("a"));
    }

    #[test]
    fn backticks_scope_to_a_subtree() {
        let q = parse_text("`C:\\Users\\Admin` report");
        assert_eq!(q.path_scope.as_deref(), Some("C:\\Users\\Admin"));
        assert_eq!(q.expr, name("report"));

        // Unclosed backtick still scopes; trailing separator is dropped.
        let q = parse_text("draft `D:\\Evidence\\");
        assert_eq!(q.path_scope.as_deref(), Some("D:\\Evidence"));
        assert_eq!(q.expr, name("draft"));
    }

    #[test]
    fn inline_operators_become_terms() {
        let q = parse_text("ext:pdf;docx size:>10mb dm:2024-01-01 report");
        let Expr::And(parts) = &q.expr else {
            panic!("expected an AND, got {:?}", q.expr);
        };
        assert_eq!(parts.len(), 4);
        assert_eq!(
            parts[0],
            Expr::Term(Term::Ext(vec!["pdf".to_string(), "docx".to_string()])),
            "a `;` inside an operator value is part of the value, not an OR"
        );
        assert_eq!(parts[1], Expr::Term(Term::Size(10 << 20, u64::MAX)));
        assert!(matches!(parts[2], Expr::Term(Term::Modified(_, _))));
        assert_eq!(parts[3], name("report"));
    }

    #[test]
    fn folder_and_file_operators_set_the_kind() {
        assert_eq!(
            parse_text("folder:").expr,
            Expr::Term(Term::Kind(KindFilter::Folders))
        );
        assert_eq!(
            parse_text("file:*.iso").expr,
            Expr::And(vec![
                Expr::Term(Term::Kind(KindFilter::Files)),
                name("*.iso"),
            ])
        );
    }

    #[test]
    fn bad_operator_values_warn_instead_of_failing() {
        let q = parse_text("size:>wat report");
        assert_eq!(q.warnings.len(), 1);
        assert_eq!(q.expr, name("report"), "the rest of the query still runs");
    }

    #[test]
    fn separate_filter_fields_merge_with_the_text() {
        let q = Query::parse(&RawQuery {
            text: "report".to_string(),
            regex: r"^\d{4}-".to_string(),
            size: ">1gb".to_string(),
            modified: "2024-01-01..".to_string(),
            extensions: ".pdf; .docx".to_string(),
            ..RawQuery::default()
        });
        assert_eq!(q.expr, name("report"));
        assert_eq!(q.extensions, vec!["pdf", "docx"]);
        assert_eq!(q.size, Some((1 << 30, u64::MAX)));
        assert!(q.modified.is_some());
        assert!(q.regex.is_some());
    }

    #[test]
    fn an_invalid_regex_warns_and_is_dropped() {
        let q = Query::parse(&RawQuery {
            regex: "(unclosed".to_string(),
            ..RawQuery::default()
        });
        assert!(q.regex.is_none());
        assert_eq!(q.warnings.len(), 1);
    }

    #[test]
    fn presets_contribute_operators_like_typed_text() {
        let q = Query::parse(&RawQuery {
            text: "intro".to_string(),
            preset: "ext:mp3;flac;wav".to_string(),
            ..RawQuery::default()
        });
        assert_eq!(
            q.expr,
            Expr::And(vec![
                Expr::Term(Term::Ext(vec![
                    "mp3".to_string(),
                    "flac".to_string(),
                    "wav".to_string(),
                ])),
                name("intro"),
            ])
        );
    }

    #[test]
    fn a_presets_bare_modifiers_govern_the_typed_text() {
        // A preset whose search starts with a bare modifier - which is how
        // `Filters.csv` says "this filter matches case" (presets.rs).
        let q = Query::parse(&RawQuery {
            text: "report".to_string(),
            preset: "case: ext:log".to_string(),
            ..RawQuery::default()
        });
        assert_eq!(
            q.expr,
            Expr::And(vec![
                Expr::Term(Term::Ext(vec!["log".to_string()])),
                Expr::Term(Term::Name(NameTerm::new(
                    "report",
                    Mods {
                        case: Some(true),
                        ..Mods::default()
                    }
                ))),
            ]),
            "the flag reaches the term the user typed, not just the preset's"
        );
    }

    #[test]
    fn positives_skip_what_is_negated() {
        let q = parse_text("report !draft ext:pdf");
        let (mut patterns, mut extensions) = (Vec::new(), Vec::new());
        q.expr.positives(&mut patterns, &mut extensions);
        assert_eq!(patterns, vec![&Pattern::Substring("report".to_string())]);
        assert_eq!(extensions, vec!["pdf"]);
    }
}
