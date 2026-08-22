//! The query grammar: search text -> [`Expr`].
//!
//! One tokenizer and one recursive-descent parser implement the whole grammar
//! from `advanced-search.md`: space is AND, `|` and `;` are OR, `!` negates,
//! `<` `>` group, and `"` quotes an exact phrase. Wildcards, the escape macros
//! (`quot:`, `#x41:`), the type macros (`audio:`, `exe:`), and the modifiers
//! (`case:`, `path:`, `wholeword:`) are all resolved here, so the matcher never
//! re-inspects query text.
//!
//! A modifier written with a value binds to its term; written bare it governs
//! the rest of its group, which is why the parser carries a [`Mods`] it saves
//! and restores around `<` `>`.
//!
//! Parsing is **lenient**, the same contract [`Query::parse`] has always had: a
//! half-typed query must not flash the list empty. An unclosed group closes at
//! the end, a stray `>` is skipped, a trailing `!` or `|` is dropped, and a
//! malformed operator value becomes a warning while the rest of the query runs.
//!
//! [`Query::parse`]: crate::service::query::Query::parse

use std::iter::Peekable;
use std::str::Chars;

use crate::service::filetype::FileKind;
use crate::service::query::{
    Anchor, ChildKind, Expr, KindFilter, Mods, NameTerm, Term, and_of, or_of, split_list,
    strip_operator,
};
use crate::service::values;

/// Escape macros: `name:` anywhere in a term becomes this character.
const MACROS: &[(&str, char)] = &[
    ("quot:", '"'),
    ("apos:", '\''),
    ("amp:", '&'),
    ("lt:", '<'),
    ("gt:", '>'),
];

/// Type macros: a whole term of `name:` selects that file category.
const CATEGORIES: &[(&str, FileKind)] = &[
    ("audio", FileKind::Audio),
    ("zip", FileKind::Archive),
    ("doc", FileKind::Document),
    ("exe", FileKind::Executable),
    ("pic", FileKind::Image),
    ("video", FileKind::Video),
];

/// Modifiers, and which field of [`Mods`] each one sets. Any of them takes a
/// `no` prefix to turn it off, and `utf8:` is spelled as the negation of
/// `ascii:` because that is what it means.
const MODIFIERS: &[(&str, Modifier)] = &[
    ("case", Modifier::Case),
    ("diacritics", Modifier::Diacritics),
    ("wholeword", Modifier::WholeWord),
    ("ww", Modifier::WholeWord),
    ("wholefilename", Modifier::WholeName),
    ("wfn", Modifier::WholeName),
    ("path", Modifier::Path),
    ("regex", Modifier::Regex),
    ("wildcards", Modifier::Wildcards),
    ("ascii", Modifier::Ascii),
    ("utf8", Modifier::Utf8),
    ("startwith", Modifier::StartWith),
    ("endwith", Modifier::EndWith),
];

#[derive(Debug, Clone, Copy)]
enum Modifier {
    Case,
    Diacritics,
    WholeWord,
    WholeName,
    Path,
    Regex,
    Wildcards,
    Ascii,
    Utf8,
    StartWith,
    EndWith,
}

impl Modifier {
    /// One line for the syntax reference the UI shows.
    fn summary(self) -> &'static str {
        match self {
            Self::Case => "Match case.",
            Self::Diacritics => "Match diacritical marks.",
            Self::WholeWord => "Match whole words only.",
            Self::WholeName => "Match the whole filename.",
            Self::Path => "Match the path as well as the name.",
            Self::Regex => "Read the term as a regular expression.",
            Self::Wildcards => "Enable wildcards.",
            Self::Ascii => "Enable fast ASCII case comparisons.",
            Self::Utf8 => "Disable fast ASCII case comparisons.",
            Self::StartWith => "Filenames starting with the text.",
            Self::EndWith => "Filenames ending with the text.",
        }
    }

    fn apply(self, on: bool, mods: &mut Mods) {
        match self {
            Self::Case => mods.case = Some(on),
            Self::Diacritics => mods.diacritics = on,
            Self::WholeWord => mods.whole_word = on,
            Self::WholeName => mods.whole_name = on,
            Self::Path => mods.path = on,
            Self::Regex => mods.regex = on,
            Self::Wildcards => mods.wildcards = Some(on),
            Self::Ascii => mods.ascii = on,
            Self::Utf8 => mods.ascii = !on,
            Self::StartWith => mods.anchor = if on { Anchor::Start } else { Anchor::Anywhere },
            Self::EndWith => mods.anchor = if on { Anchor::End } else { Anchor::Anywhere },
        }
    }
}

/// Peel one modifier prefix off a term. `None` when it does not start with one.
fn strip_modifier(text: &str) -> Option<(Modifier, bool, &str)> {
    for (name, modifier) in MODIFIERS {
        if let Some(rest) = strip_operator(text, &format!("{name}:")) {
            return Some((*modifier, true, rest));
        }
        if let Some(rest) = strip_operator(text, &format!("no{name}:")) {
            return Some((*modifier, false, rest));
        }
    }
    None
}

/// What a search string parses to.
pub struct Parsed {
    pub expr: Expr,
    /// `count:` - a cap on results rather than a condition on a node, so
    /// it rides beside the tree instead of inside it.
    pub limit: Option<usize>,
}

/// Parse search text into an expression tree. Never fails; see the module docs.
pub fn parse_expr(text: &str, warnings: &mut Vec<String>) -> Parsed {
    let tokens = tokenize(text);
    let mut parser = Parser {
        tokens: &tokens,
        at: 0,
        mods: Mods::default(),
        limit: None,
        warnings,
    };

    // Top level: one or more expressions, skipping any `>` with no opener.
    let mut parts = Vec::new();
    while parser.at < tokens.len() {
        let before = parser.at;
        if matches!(parser.peek(), Some(Token::Close)) {
            parser.at += 1;
            parser.warn("unmatched `>`");
            continue;
        }
        parts.push(parser.or_expr());
        if parser.at == before {
            break; // no progress: refuse to spin
        }
    }
    Parsed {
        expr: and_of(parts),
        limit: parser.limit,
    }
}

// ---------------------------------------------------------------------------
// tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Or,
    Not,
    /// A term with its macros already expanded. `quoted` means the text was
    /// written inside `"`, which makes it a literal name match; `head` is
    /// then whatever stood before the opening quote, where the modifiers of
    /// a phrase have to live.
    Text {
        text: String,
        head: String,
        quoted: bool,
    },
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            _ if c.is_whitespace() => {
                chars.next();
            }
            '|' | ';' => {
                chars.next();
                out.push(Token::Or);
            }
            '!' => {
                chars.next();
                out.push(Token::Not);
            }
            '<' => {
                chars.next();
                out.push(Token::Open);
            }
            '>' => {
                chars.next();
                out.push(Token::Close);
            }
            _ => out.push(scan_term(&mut chars)),
        }
    }
    out
}

/// Read one term.
///
/// A term ends at whitespace or `|`, and - only before its first colon - at
/// `;`, `<`, or `>`. That one rule is what lets `size:>10mb` and the preset
/// `ext:mp3;flac;wav` survive a grammar in which those characters are
/// operators: past the colon they are part of a value.
fn scan_term(chars: &mut Peekable<Chars>) -> Token {
    let mut raw = String::new();
    let mut head = String::new();
    let mut quoted = false;
    let mut seen_colon = false;

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == '|' {
            break;
        }
        match c {
            ';' | '<' | '>' if !seen_colon => break,
            '"' => {
                chars.next();
                if !quoted {
                    // What came before the quote is not part of the phrase.
                    head = std::mem::take(&mut raw);
                    quoted = true;
                }
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    raw.push(c);
                }
            }
            _ => {
                if c == ':' {
                    seen_colon = true;
                }
                raw.push(c);
                chars.next();
            }
        }
    }

    Token::Text {
        text: expand_macros(&raw),
        head,
        quoted,
    }
}

/// Replace `quot:`-style escapes with the characters they stand for.
fn expand_macros(text: &str) -> String {
    if !text.contains(':') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'scan: while !rest.is_empty() {
        for (name, ch) in MACROS {
            if let Some(tail) = strip_operator(rest, name) {
                out.push(*ch);
                rest = tail;
                continue 'scan;
            }
        }
        if let Some((ch, tail)) = numeric_macro(rest) {
            out.push(ch);
            rest = tail;
            continue;
        }
        let mut chars = rest.chars();
        match chars.next() {
            Some(c) => out.push(c),
            None => break,
        }
        rest = chars.as_str();
    }
    out
}

/// `#65:` and `#x41:` - a literal character by code point.
fn numeric_macro(rest: &str) -> Option<(char, &str)> {
    let body = rest.strip_prefix('#')?;
    let (radix, body) = match body.strip_prefix(['x', 'X']) {
        Some(hex) => (16, hex),
        None => (10, body),
    };
    let end = body.find(':')?;
    let digits = &body[..end];
    if digits.is_empty() {
        return None;
    }
    let code = u32::from_str_radix(digits, radix).ok()?;
    Some((char::from_u32(code)?, &body[end + 1..]))
}

// ---------------------------------------------------------------------------
// parser
// ---------------------------------------------------------------------------

struct Parser<'t> {
    tokens: &'t [Token],
    at: usize,
    /// Modifiers a bare `case:` and friends have switched on, in force for the
    /// rest of the current group.
    mods: Mods,
    /// The last `count:` seen.
    limit: Option<usize>,
    warnings: &'t mut Vec<String>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn warn(&mut self, message: &str) {
        let message = message.to_string();
        if !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }

    /// `and (('|' | ';') and)*`
    fn or_expr(&mut self) -> Expr {
        let mut branches = Vec::new();
        if let Some(first) = self.and_expr() {
            branches.push(first);
        }
        while matches!(self.peek(), Some(Token::Or)) {
            self.at += 1;
            // A trailing `|` contributes nothing rather than matching all.
            if let Some(branch) = self.and_expr() {
                branches.push(branch);
            }
        }
        or_of(branches)
    }

    /// `unary+`. `None` when the next token starts nothing.
    fn and_expr(&mut self) -> Option<Expr> {
        let mut parts = Vec::new();
        while let Some(part) = self.unary() {
            parts.push(part);
        }
        (!parts.is_empty()).then(|| and_of(parts))
    }

    /// `'!'* primary`
    fn unary(&mut self) -> Option<Expr> {
        match self.peek()? {
            Token::Or | Token::Close => None,
            Token::Not => {
                self.at += 1;
                // A trailing `!` is dropped, not applied to nothing.
                let inner = self.unary()?;
                Some(Expr::Not(Box::new(inner)))
            }
            Token::Open => {
                self.at += 1;
                // A bare modifier inside a group dies with the group.
                let outer = self.mods;
                let inner = self.or_expr();
                self.mods = outer;
                if matches!(self.peek(), Some(Token::Close)) {
                    self.at += 1;
                } else {
                    self.warn("unclosed `<`");
                }
                Some(inner)
            }
            Token::Text { text, head, quoted } => {
                let (text, head, quoted) = (text.clone(), head.clone(), *quoted);
                self.at += 1;
                Some(self.term(&text, &head, quoted))
            }
        }
    }

    /// One term's text -> the thing it filters on.
    fn term(&mut self, text: &str, head: &str, quoted: bool) -> Expr {
        if text.is_empty() && !quoted {
            return Expr::All;
        }

        // Modifier prefixes stack: `case:wfn:report` is both. A phrase takes
        // them from the text before its opening quote, so `case:"annual
        // report"` is a cased phrase while `"case:foo"` stays literal.
        let mut mods = self.mods;
        let mut rest = if quoted { head } else { text };
        while let Some((modifier, on, tail)) = strip_modifier(rest) {
            modifier.apply(on, &mut mods);
            rest = tail;
        }

        if quoted {
            // `infolder:"C:\Program Files"` - quotes carry a function value
            // that contains spaces, which nothing else in the grammar can.
            if rest.ends_with(':') {
                return self.function(&format!("{rest}{text}"), mods);
            }
            // Otherwise it is a literal phrase: wildcards inside it are just
            // characters, and anything left of the quote that was not a
            // modifier belongs to the phrase - `re"port"` is one word.
            mods.wildcards = Some(false);
            let phrase = format!("{rest}{text}");
            return match phrase.is_empty() {
                true => Expr::All,
                false => Expr::Term(Term::Name(NameTerm::new(&phrase, mods))),
            };
        }

        if rest.is_empty() {
            // A bare modifier: it governs the rest of this group instead.
            self.mods = mods;
            return Expr::All;
        }

        self.function(rest, mods)
    }

    /// A term with its modifiers already peeled: a function if it names one,
    /// otherwise a name search.
    fn function(&mut self, rest: &str, mods: Mods) -> Expr {
        if let Some(kind) = category(rest) {
            return Expr::Term(Term::Category(kind));
        }

        // Standalone functions: a whole term, no value to read.
        match rest.to_ascii_lowercase().as_str() {
            "empty:" => return Expr::Term(Term::Empty),
            "root:" => return Expr::Term(Term::Root),
            _ => {}
        }

        if let Some(list) = strip_operator(rest, "ext:") {
            let list: Vec<String> = split_list(list).map(str::to_string).collect();
            return if list.is_empty() {
                Expr::All
            } else {
                Expr::Term(Term::Ext(list))
            };
        }
        if let Some(value) = first_of(rest, &["size:"]) {
            return self.ranged(value, "size", values::size, Term::Size);
        }
        if let Some(value) = first_of(rest, &["datemodified:", "dm:"]) {
            return self.ranged(value, "date", values::date, Term::Modified);
        }
        if let Some(value) = first_of(rest, &["datecreated:", "dc:"]) {
            return self.ranged(value, "date", values::date, Term::Created);
        }
        if let Some(value) = first_of(rest, &["parents:"]) {
            return self.ranged(value, "count", values::count, Term::Depth);
        }
        if let Some(value) = first_of(rest, &["len:"]) {
            let on_path = mods.path;
            return self.ranged(value, "count", values::count, move |lo, hi| Term::Length {
                lo,
                hi,
                path: on_path,
            });
        }
        if let Some(value) = first_of(rest, &["childcount:"]) {
            return self.children(value, ChildKind::All);
        }
        if let Some(value) = first_of(rest, &["childfilecount:"]) {
            return self.children(value, ChildKind::Files);
        }
        if let Some(value) = first_of(rest, &["childfoldercount:"]) {
            return self.children(value, ChildKind::Folders);
        }
        if let Some(value) = first_of(rest, &["count:"]) {
            return match values::limit(value) {
                Some(limit) => {
                    self.limit = Some(limit);
                    Expr::All
                }
                None => {
                    self.warn(&format!("unrecognized count `{value}`"));
                    Expr::All
                }
            };
        }
        if let Some(value) = first_of(rest, &["child:"]) {
            return match value.is_empty() {
                true => Expr::All,
                false => Expr::Term(Term::Child(Box::new(NameTerm::new(value, mods)))),
            };
        }
        if let Some(value) = first_of(rest, &["infolder:", "parent:"]) {
            let value = value.trim_end_matches(['\\', '/']);
            return match value.is_empty() {
                true => Expr::All,
                false => Expr::Term(Term::InFolder(value.to_string())),
            };
        }
        if let Some(value) = first_of(rest, &["attrib:", "attributes:"]) {
            return self.attributes(value);
        }
        if let Some(value) = first_of(rest, &["type:"]) {
            return match file_kind(value) {
                Some(kind) => Expr::Term(Term::Category(kind)),
                None => {
                    self.warn(&format!("unknown type `{value}`"));
                    Expr::All
                }
            };
        }
        if let Some(value) = strip_operator(rest, "folder:") {
            return self.kind_term(KindFilter::Folders, value, mods);
        }
        if let Some(value) = strip_operator(rest, "file:") {
            return self.kind_term(KindFilter::Files, value, mods);
        }

        self.name_term(rest, mods)
    }

    /// A function whose value is a range, with one warning shape for all of
    /// them.
    fn ranged<T, V, B>(&mut self, value: &str, kind: &str, read: V, build: B) -> Expr
    where
        V: Fn(&str) -> Option<(T, T)>,
        B: FnOnce(T, T) -> Term,
    {
        match read(value) {
            Some((lo, hi)) => Expr::Term(build(lo, hi)),
            None => {
                self.warn(&format!("unrecognized {kind} filter `{value}`"));
                Expr::All
            }
        }
    }

    fn children(&mut self, value: &str, what: ChildKind) -> Expr {
        self.ranged(value, "count", values::count, move |lo, hi| {
            Term::Children { what, lo, hi }
        })
    }

    /// `attrib:HS`. Letters this index has no bit for are reported and
    /// dropped, so the rest of the term still filters.
    fn attributes(&mut self, value: &str) -> Expr {
        let (flags, unknown) = values::attributes(value);
        for c in unknown {
            if values::is_unrecorded_attribute(c) {
                self.warn(&format!("attribute `{c}` is not recorded by EmFit"));
            } else {
                self.warn(&format!("unknown attribute `{c}`"));
            }
        }
        if flags == crate::model::entry::EntryFlags::empty() {
            return Expr::All;
        }
        Expr::Term(Term::Attributes(flags))
    }

    /// `folder:cache` is two conditions, not one: a directory whose name matches.
    fn kind_term(&mut self, kind: KindFilter, rest: &str, mods: Mods) -> Expr {
        let kind = Expr::Term(Term::Kind(kind));
        if rest.is_empty() {
            return kind;
        }
        and_of(vec![kind, self.name_term(rest, mods)])
    }

    /// A name condition, with a bad `regex:` reported and dropped rather than
    /// left to fail silently in the matcher.
    fn name_term(&mut self, text: &str, mods: Mods) -> Expr {
        if mods.regex
            && let Err(e) = regex::Regex::new(text)
        {
            self.warn(&format!("regex: {e}"));
            return Expr::All;
        }
        Expr::Term(Term::Name(NameTerm::new(text, mods)))
    }
}

/// Strip whichever of these operator prefixes the term starts with.
fn first_of<'t>(text: &'t str, ops: &[&str]) -> Option<&'t str> {
    ops.iter().find_map(|op| strip_operator(text, op))
}

/// A `type:` value: the file-type labels, plus the type-macro spellings.
fn file_kind(text: &str) -> Option<FileKind> {
    const KINDS: [FileKind; 9] = [
        FileKind::Directory,
        FileKind::Executable,
        FileKind::Archive,
        FileKind::Image,
        FileKind::Video,
        FileKind::Audio,
        FileKind::Document,
        FileKind::Code,
        FileKind::Other,
    ];
    KINDS
        .iter()
        .find(|kind| text.eq_ignore_ascii_case(kind.label()))
        .copied()
        .or_else(|| {
            CATEGORIES
                .iter()
                .find(|(name, _)| text.eq_ignore_ascii_case(name))
                .map(|&(_, kind)| kind)
        })
}

/// A whole term of `audio:` and friends.
fn category(text: &str) -> Option<FileKind> {
    let name = text.strip_suffix(':')?;
    CATEGORIES
        .iter()
        .find(|(macro_name, _)| name.eq_ignore_ascii_case(macro_name))
        .map(|&(_, kind)| kind)
}

// ---------------------------------------------------------------------------
// reference
// ---------------------------------------------------------------------------

/// One row of the syntax reference.
#[derive(Debug, Clone)]
pub struct SyntaxEntry {
    /// What to type, and what clicking the row inserts.
    pub token: String,
    pub summary: String,
}

/// A titled group of rows.
#[derive(Debug, Clone)]
pub struct SyntaxSection {
    pub title: String,
    pub entries: Vec<SyntaxEntry>,
}

fn entry(token: &str, summary: &str) -> SyntaxEntry {
    SyntaxEntry {
        token: token.to_string(),
        summary: summary.to_string(),
    }
}

fn section(title: &str, entries: Vec<SyntaxEntry>) -> SyntaxSection {
    SyntaxSection {
        title: title.to_string(),
        entries,
    }
}

/// The whole query language, for the Search syntax dialog.
///
/// Built from the same tables the parser uses, so the reference cannot promise
/// a macro or modifier the engine would ignore. The operators, functions, and
/// value grammars are listed here because that is where they are defined -
/// this function is their documentation.
pub fn reference() -> Vec<SyntaxSection> {
    let mut out = vec![
        section(
            "Operators",
            vec![
                entry(" ", "AND - both terms must match."),
                entry("|", "OR - either term may match."),
                entry("!", "NOT - exclude what the term matches."),
                entry("<>", "Group terms, to override precedence."),
                entry("\"\"", "Search for an exact phrase."),
                entry("`path`", "Restrict results to one folder tree."),
            ],
        ),
        section(
            "Wildcards",
            vec![
                entry("*", "Matches zero or more characters."),
                entry("?", "Matches one character."),
            ],
        ),
    ];

    let mut macros: Vec<SyntaxEntry> = MACROS
        .iter()
        .map(|(name, ch)| entry(name, &format!("Literal {ch}")))
        .collect();
    macros.push(entry("#<n>:", "Literal unicode character <n> in decimal."));
    macros.push(entry(
        "#x<n>:",
        "Literal unicode character <n> in hexadecimal.",
    ));
    macros.extend(CATEGORIES.iter().map(|(name, kind)| {
        entry(
            &format!("{name}:"),
            &format!("Search for {} files.", kind.label().to_lowercase()),
        )
    }));
    out.push(section("Macros", macros));

    let mut modifiers: Vec<SyntaxEntry> = MODIFIERS
        .iter()
        .map(|(name, modifier)| entry(&format!("{name}:"), modifier.summary()))
        .collect();
    modifiers.push(entry("no", "Prefix any modifier with `no` to turn it off."));
    out.push(section("Modifiers", modifiers));

    out.push(section(
        "Functions",
        vec![
            entry("attrib:<letters>", "Files with these attributes set."),
            entry("child:<filename>", "Folders holding a matching child."),
            entry("childcount:<n>", "Folders with this many entries."),
            entry("childfilecount:<n>", "Folders with this many files."),
            entry("childfoldercount:<n>", "Folders with this many subfolders."),
            entry("count:<n>", "Keep at most this many results."),
            entry("datecreated:<date>", "Created on this date."),
            entry("datemodified:<date>", "Modified on this date."),
            entry("dc:<date>", "Short for datecreated:."),
            entry("dm:<date>", "Short for datemodified:."),
            entry("empty:", "Folders with nothing in them."),
            entry("ext:<e1;e2>", "Files with one of these extensions."),
            entry("file:", "Files only."),
            entry("folder:", "Folders only."),
            entry("infolder:<path>", "Directly inside this folder."),
            entry("len:<n>", "Name of this length, in characters."),
            entry("parent:<path>", "Short for infolder:."),
            entry("parents:<n>", "This many folders above it."),
            entry("root:", "Entries with no parent folder."),
            entry("size:<size>", "Files of this size."),
            entry("type:<type>", "Entries of this file type."),
        ],
    ));

    out.push(section(
        "Comparisons",
        vec![
            entry("value", "Equal to value, or at least it for sizes."),
            entry("=value", "Equal to value."),
            entry("<value", "Less than value."),
            entry("<=value", "Less than or equal to value."),
            entry(">value", "Greater than value."),
            entry(">=value", "Greater than or equal to value."),
            entry("start..end", "Within the range from start to end."),
            entry("start-end", "Within the range from start to end."),
        ],
    ));

    out.push(section(
        "Sizes",
        vec![
            entry("<n>[kb|mb|gb|tb]", "A size, in binary units."),
            entry("empty", "Zero bytes."),
            entry("tiny", "Up to 10 KB."),
            entry("small", "10 KB to 100 KB."),
            entry("medium", "100 KB to 1 MB."),
            entry("large", "1 MB to 16 MB."),
            entry("huge", "16 MB to 128 MB."),
            entry("gigantic", "Over 128 MB."),
        ],
    ));

    out.push(section(
        "Dates",
        vec![
            entry("YYYY", "A whole year."),
            entry("YYYY-MM", "A whole month."),
            entry("YYYY-MM-DD", "A whole day."),
            entry("YYYY-MM-DDThh:mm:ss", "An instant, to whatever precision."),
            entry("today", "Also yesterday and tomorrow."),
            entry("thisweek", "Also thismonth and thisyear."),
            entry("lastweek", "Also last, prev, coming, and next."),
            entry("last7days", "A count of days, weeks, months, or years."),
            entry("last2hours", "Also minutes and seconds, from right now."),
            entry("january", "A month by name, most recent first."),
            entry("monday", "A weekday by name, most recent first."),
        ],
    ));

    out.push(section(
        "Attributes",
        vec![
            entry("C", "Compressed"),
            entry("D", "Directory"),
            entry("H", "Hidden"),
            entry("L", "Reparse point"),
            entry("P", "Sparse file"),
            entry("S", "System"),
        ],
    ));

    out
}
