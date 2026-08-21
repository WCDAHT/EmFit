//! The search query: what the user typed, parsed into something executable.
//!
//! One `Query` captures the whole feature set of features.md sec 2 that M2
//! ships: substring and wildcard patterns, semicolon-OR multi-patterns,
//! backtick path scoping, a separate regex field, size / date / extension
//! filters, and the inline `ext:` `size:` `dm:` `folder:` / `file:`
//! operators the `Filters.csv` presets already use.
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
//! [`CaseFold`]: crate::service::fold::CaseFold

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
            Self::Substring(t) | Self::Prefix(t) | Self::Suffix(t) | Self::Glob(t) => t,
        }
    }
}

/// Raw filter strings as the UI holds them. The shell passes this across IPC
/// verbatim; [`Query::parse`] is the single place they become semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawQuery {
    /// The main search box: patterns, `;`-separated OR terms, inline
    /// operators, optional backtick-quoted path scope.
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
    /// OR: a name matching any pattern passes. Empty means "match everything".
    pub patterns: Vec<Pattern>,
    /// Extensions (without the dot). Empty means no extension filter.
    pub extensions: Vec<String>,
    /// Inclusive logical-size range in bytes.
    pub size: Option<(u64, u64)>,
    /// Inclusive mtime range, nanoseconds since the Unix epoch.
    pub modified: Option<(i64, i64)>,
    pub kind: KindFilter,
    /// Restrict results to this subtree, e.g. `C:\Users`.
    pub path_scope: Option<String>,
    pub regex: Option<regex::Regex>,
    pub include_hidden: bool,
    pub include_system: bool,
    pub case_sensitive: Option<bool>,
    /// Parts of the input that were skipped, for the UI to surface.
    pub warnings: Vec<String>,
}

impl Query {
    /// True when nothing constrains the result: the browse-everything state.
    pub fn is_match_all(&self) -> bool {
        self.patterns.is_empty()
            && self.extensions.is_empty()
            && self.size.is_none()
            && self.modified.is_none()
            && self.kind == KindFilter::Any
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

        // Tokenize; operators peel off, the rest is pattern text.
        let mut pattern_text = String::new();
        for token in text.split_whitespace() {
            if let Some(rest) = strip_operator(token, "ext:") {
                q.extensions.extend(split_list(rest).map(str::to_string));
            } else if let Some(rest) = strip_operator(token, "size:") {
                match parse_size_filter(rest) {
                    Some(range) => q.size = Some(range),
                    None => q
                        .warnings
                        .push(format!("unrecognized size filter `{rest}`")),
                }
            } else if let Some(rest) = strip_operator(token, "dm:") {
                match parse_date_filter(rest) {
                    Some(range) => q.modified = Some(range),
                    None => q
                        .warnings
                        .push(format!("unrecognized date filter `{rest}`")),
                }
            } else if let Some(rest) = strip_operator(token, "folder:") {
                q.kind = KindFilter::Folders;
                push_term(&mut pattern_text, rest);
            } else if let Some(rest) = strip_operator(token, "file:") {
                q.kind = KindFilter::Files;
                push_term(&mut pattern_text, rest);
            } else {
                push_term(&mut pattern_text, token);
            }
        }

        // Multi-pattern: semicolon-separated terms, OR'd. Dots are kept -
        // `.pdf` is a substring pattern, unlike in the `ext:` list.
        for term in pattern_text.split(';') {
            let term = term.trim();
            if !term.is_empty() {
                q.patterns.push(Pattern::classify(term));
            }
        }

        // The separate filter fields, same grammars as the inline operators.
        if !raw.extensions.trim().is_empty() {
            q.extensions
                .extend(split_list(&raw.extensions).map(str::to_string));
        }
        if !raw.size.trim().is_empty() {
            match parse_size_filter(raw.size.trim()) {
                Some(range) => q.size = Some(range),
                None => q
                    .warnings
                    .push(format!("unrecognized size filter `{}`", raw.size.trim())),
            }
        }
        if !raw.modified.trim().is_empty() {
            match parse_date_filter(raw.modified.trim()) {
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
fn strip_operator<'a>(token: &'a str, op: &str) -> Option<&'a str> {
    (token.len() >= op.len() && token.as_bytes()[..op.len()].eq_ignore_ascii_case(op.as_bytes()))
        .then(|| &token[op.len()..])
}

/// Split a `;`-separated list, dropping empties and stray dots.
fn split_list(list: &str) -> impl Iterator<Item = &str> {
    list.split(';')
        .map(|part| part.trim().trim_start_matches('.'))
        .filter(|part| !part.is_empty())
}

fn push_term(text: &mut String, term: &str) {
    if term.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with(';') && !term.starts_with(';') {
        text.push(' ');
    }
    text.push_str(term);
}

/// `>10MB`, `>=10MB`, `<1GB`, `10MB..1GB`, `10MB-1GB`, bare `10MB` (at least).
fn parse_size_filter(text: &str) -> Option<(u64, u64)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix(">=").or_else(|| text.strip_prefix('>')) {
        return Some((parse_size(rest)?, u64::MAX));
    }
    if let Some(rest) = text.strip_prefix("<=").or_else(|| text.strip_prefix('<')) {
        return Some((0, parse_size(rest)?));
    }
    if let Some((lo, hi)) = split_range(text) {
        let lo = if lo.is_empty() { 0 } else { parse_size(lo)? };
        let hi = if hi.is_empty() {
            u64::MAX
        } else {
            parse_size(hi)?
        };
        return (lo <= hi).then_some((lo, hi));
    }
    // Bare value: "at least this big" - the question a space analyzer is
    // usually asked.
    Some((parse_size(text)?, u64::MAX))
}

/// `10`, `10kb`, `1.5GB` - binary units, matching the display side.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    let digits = text.trim_end_matches(char::is_alphabetic);
    let multiplier: u64 = match &text[digits.len()..] {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1u64 << 40,
        _ => return None,
    };
    let number = digits.trim();
    if number.is_empty() {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if value < 0.0 {
        return None;
    }
    Some((value * multiplier as f64) as u64)
}

/// `2024-01-01`, `>2024-01-01`, `<2024-01-01`, `2024-01-01..2024-06-30`.
fn parse_date_filter(text: &str) -> Option<(i64, i64)> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix(">=").or_else(|| text.strip_prefix('>')) {
        return Some((day_start(rest)?, i64::MAX));
    }
    if let Some(rest) = text.strip_prefix("<=").or_else(|| text.strip_prefix('<')) {
        return Some((i64::MIN, day_end(rest)?));
    }
    if let Some((lo, hi)) = split_range(text) {
        let lo = if lo.is_empty() {
            i64::MIN
        } else {
            day_start(lo)?
        };
        let hi = if hi.is_empty() {
            i64::MAX
        } else {
            day_end(hi)?
        };
        return (lo <= hi).then_some((lo, hi));
    }
    // A bare date means that whole day.
    Some((day_start(text)?, day_end(text)?))
}

/// Split on `..`, or on a `-` that isn't part of a date (`2024-01-01`).
fn split_range(text: &str) -> Option<(&str, &str)> {
    if let Some((lo, hi)) = text.split_once("..") {
        return Some((lo.trim(), hi.trim()));
    }
    // A lone `-` separates sizes (`10MB-1GB`); dates contain dashes, so only
    // treat it as a range when neither side starts with a digit-dash pattern
    // resembling a date. Cheap rule: split on `-` only when the text holds
    // exactly one.
    if text.matches('-').count() == 1 {
        let (lo, hi) = text.split_once('-')?;
        return Some((lo.trim(), hi.trim()));
    }
    None
}

fn parse_day(text: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

/// Midnight at the start of the day, UTC, in nanoseconds.
fn day_start(text: &str) -> Option<i64> {
    let date = parse_day(text)?;
    date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_nanos_opt()
}

/// The last nanosecond of the day, UTC.
fn day_end(text: &str) -> Option<i64> {
    let start = day_start(text)?;
    Some(start + (24 * 60 * 60 * 1_000_000_000 - 1))
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

    #[test]
    fn plain_text_is_a_substring_pattern() {
        let q = parse_text("report");
        assert_eq!(q.patterns, vec![Pattern::Substring("report".to_string())]);
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
            let q = parse_text(text);
            assert!(
                !q.patterns.is_empty(),
                "`{text}` should produce a pattern"
            );
        }
        let q = parse_text("\u{43f}\u{440} ext:pdf");
        assert_eq!(q.extensions, vec!["pdf".to_string()]);
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
    fn semicolons_or_patterns_together() {
        let q = parse_text(".pdf;.docx");
        assert_eq!(q.patterns.len(), 2);
        assert_eq!(q.patterns[0], Pattern::Substring(".pdf".to_string()));
        assert_eq!(q.patterns[1], Pattern::Substring(".docx".to_string()));
    }

    #[test]
    fn backticks_scope_to_a_subtree() {
        let q = parse_text("`C:\\Users\\Admin` report");
        assert_eq!(q.path_scope.as_deref(), Some("C:\\Users\\Admin"));
        assert_eq!(q.patterns, vec![Pattern::Substring("report".to_string())]);

        // Unclosed backtick still scopes; trailing separator is dropped.
        let q = parse_text("draft `D:\\Evidence\\");
        assert_eq!(q.path_scope.as_deref(), Some("D:\\Evidence"));
        assert_eq!(q.patterns, vec![Pattern::Substring("draft".to_string())]);
    }

    #[test]
    fn inline_operators_peel_off_the_text() {
        let q = parse_text("ext:pdf;docx size:>10mb dm:2024-01-01 report");
        assert_eq!(q.extensions, vec!["pdf", "docx"]);
        assert_eq!(q.size, Some((10 << 20, u64::MAX)));
        assert!(q.modified.is_some());
        assert_eq!(q.patterns, vec![Pattern::Substring("report".to_string())]);
    }

    #[test]
    fn folder_and_file_operators_set_the_kind() {
        assert_eq!(parse_text("folder: cache").kind, KindFilter::Folders);
        assert_eq!(parse_text("file:*.iso").kind, KindFilter::Files);
        // The text after the operator is still a pattern.
        let q = parse_text("folder:node_modules");
        assert_eq!(
            q.patterns,
            vec![Pattern::Substring("node_modules".to_string())]
        );
    }

    #[test]
    fn size_filters_cover_all_three_shapes() {
        assert_eq!(parse_size_filter(">10mb"), Some((10 << 20, u64::MAX)));
        assert_eq!(parse_size_filter("<1gb"), Some((0, 1 << 30)));
        assert_eq!(parse_size_filter("500kb..2mb"), Some((500 << 10, 2 << 20)));
        assert_eq!(parse_size_filter("500kb-2mb"), Some((500 << 10, 2 << 20)));
        assert_eq!(
            parse_size_filter("1.5gb"),
            Some(((1.5 * (1u64 << 30) as f64) as u64, u64::MAX))
        );
        assert_eq!(parse_size_filter("10mb..1kb"), None, "inverted range");
        assert_eq!(parse_size_filter("bogus"), None);
    }

    #[test]
    fn date_filters_cover_days_and_ranges() {
        let (lo, hi) = parse_date_filter("2024-06-15").unwrap();
        assert_eq!(hi - lo, 24 * 60 * 60 * 1_000_000_000 - 1, "one whole day");

        let (lo, hi) = parse_date_filter(">2024-01-01").unwrap();
        assert!(lo > 0 && hi == i64::MAX);

        let (lo, hi) = parse_date_filter("2024-01-01..2024-06-30").unwrap();
        assert!(lo < hi);

        assert_eq!(parse_date_filter("junk"), None);
        assert_eq!(parse_date_filter("2024-06-30..2024-01-01"), None);
    }

    #[test]
    fn bad_operator_values_warn_instead_of_failing() {
        let q = parse_text("size:>wat report");
        assert_eq!(q.size, None);
        assert_eq!(q.warnings.len(), 1);
        assert_eq!(
            q.patterns,
            vec![Pattern::Substring("report".to_string())],
            "the rest of the query still runs"
        );
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
        assert_eq!(q.extensions, vec!["mp3", "flac", "wav"]);
        assert_eq!(q.patterns, vec![Pattern::Substring("intro".to_string())]);
    }
}
