//! Case folding, per volume rather than per program.
//!
//! Search must compare names the way the *filesystem* does, not the way Rust's
//! `to_lowercase` does. On NTFS the authority is `$UpCase` - a 128 KiB table
//! in MFT record 10 mapping every UTF-16 code unit to its uppercase form -
//! which is what the filesystem itself consults for name comparisons. Other
//! filesystems supply their own rule or none, so folding is a value handed
//! over by the scan, not a hardcoded function (features.md sec 2).
//!
//! Case is not the only thing folded. A term also decides whether diacritical
//! marks and non-ASCII case count, via the `diacritics:`, `case:`, and `ascii:`
//! modifiers; [`Folder`] is the volume rule and those per-term rules resolved
//! into the one value the matcher carries.
//!
//! Everything here is **allocation-free per comparison**: the matcher walks
//! arena slices and folds characters on the fly. v1 called `to_lowercase()`
//! on every name for every keystroke - millions of `String`s per query.

/// A volume's case-folding rule.
#[derive(Debug, Clone, Default)]
pub enum CaseFold {
    /// Fold via Unicode simple uppercase. The fallback when no volume table
    /// is available, and a close match for NTFS's default table.
    #[default]
    Simple,
    /// Fold via the volume's own `$UpCase` table (one `u16` per BMP code
    /// unit). Supplementary-plane characters are compared as-is, exactly as
    /// NTFS does - it compares UTF-16 units and the table only covers the BMP.
    Table(std::sync::Arc<[u16]>),
}

/// Entries in a full `$UpCase` table: one per UTF-16 code unit.
pub const UPCASE_LEN: usize = 0x1_0000;

impl CaseFold {
    /// Wrap a `$UpCase` table read off a volume. Refused (falling back to
    /// [`CaseFold::Simple`]) when the table is not the documented shape.
    pub fn from_upcase(table: Vec<u16>) -> Self {
        if table.len() == UPCASE_LEN {
            Self::Table(table.into())
        } else {
            tracing::warn!(
                len = table.len(),
                "unexpected $UpCase size; folding with the simple rule"
            );
            Self::Simple
        }
    }

    /// Fold one character the way this volume compares it.
    #[inline]
    pub fn fold(&self, c: char) -> char {
        match self {
            Self::Table(table) => {
                let code = c as u32;
                if code < UPCASE_LEN as u32 {
                    // The table maps BMP units to BMP units; a mapping into
                    // the surrogate range would be corrupt, so keep the
                    // original rather than fabricate a char.
                    char::from_u32(u32::from(table[code as usize])).unwrap_or(c)
                } else {
                    c
                }
            }
            Self::Simple => {
                // Simple (1:1) uppercase. Full case folding can expand one
                // char to several (sharp s -> SS), which filesystems don't do.
                let mut up = c.to_uppercase();
                match (up.next(), up.next()) {
                    (Some(single), None) => single,
                    _ => c,
                }
            }
        }
    }

    /// Case-insensitive equality, no allocation.
    pub fn eq(&self, a: &str, b: &str) -> bool {
        let mut ca = a.chars();
        let mut cb = b.chars();
        loop {
            match (ca.next(), cb.next()) {
                (None, None) => return true,
                (Some(x), Some(y)) => {
                    if self.fold(x) != self.fold(y) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }

    /// Fold a whole string into `out` (replacing its contents). Used once per
    /// query for the needle, never per candidate name.
    pub fn fold_str(&self, s: &str, out: &mut String) {
        out.clear();
        out.reserve(s.len());
        for c in s.chars() {
            out.push(self.fold(c));
        }
    }

    /// Ordering of two strings under this folding, for sort-by-name.
    pub fn cmp(&self, a: &str, b: &str) -> std::cmp::Ordering {
        let mut ca = a.chars();
        let mut cb = b.chars();
        loop {
            match (ca.next(), cb.next()) {
                (None, None) => return std::cmp::Ordering::Equal,
                (None, Some(_)) => return std::cmp::Ordering::Less,
                (Some(_), None) => return std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => {
                    let ord = self.fold(x).cmp(&self.fold(y));
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
            }
        }
    }
}

/// How one search term compares characters.
///
/// The volume's case rule ([`CaseFold`]) is only half of it; the other half is
/// per term, from the `case:`, `diacritics:`, and `ascii:` modifiers. Both
/// halves collapse into this one value when the query is compiled, so the hot
/// loop never re-reads a modifier.
///
/// The needle is folded once by [`Folder::fold_str`]; candidate names fold on
/// the fly, per character, against the arena.
#[derive(Debug, Clone, Default)]
pub struct Folder {
    /// `None` when the term is case-sensitive.
    case: Option<CaseFold>,
    /// True: `e` does not match an accented `e` - the `diacritics:` modifier.
    strict_marks: bool,
    /// True: fold ASCII case only, ignoring the volume table. The `ascii:`
    /// modifier, which trades correctness on non-ASCII names for speed.
    ascii_only: bool,
}

impl Folder {
    /// Build the rule for one term.
    pub fn new(case: &CaseFold, case_sensitive: bool, diacritics: bool, ascii: bool) -> Self {
        Self {
            case: (!case_sensitive).then(|| case.clone()),
            strict_marks: diacritics,
            ascii_only: ascii,
        }
    }

    /// True when folding changes nothing, so the matcher can compare bytes
    /// directly instead of walking characters.
    pub fn is_identity(&self) -> bool {
        self.case.is_none() && self.strict_marks
    }

    /// Fold one character the way this term compares it.
    #[inline]
    pub fn fold(&self, c: char) -> char {
        // ASCII is the overwhelmingly common case and needs neither step.
        if c.is_ascii() {
            return match &self.case {
                Some(_) => c.to_ascii_uppercase(),
                None => c,
            };
        }
        let c = if self.strict_marks { c } else { base_char(c) };
        match &self.case {
            None => c,
            Some(_) if self.ascii_only => c,
            Some(table) => table.fold(c),
        }
    }

    /// The characters this rule compares `s` by: folded, and with combining
    /// marks dropped unless the term asked for them.
    ///
    /// Dropping is why marks are handled here and not in [`Folder::fold`]:
    /// a name may carry its accents precomposed (one character) or decomposed
    /// (a letter followed by a mark), and only removing the mark entirely
    /// makes both compare equal to the plain letter.
    fn chars<'a>(&'a self, s: &'a str) -> impl DoubleEndedIterator<Item = char> + 'a {
        s.chars()
            .filter(move |&c| self.strict_marks || c.is_ascii() || !is_mark(c))
            .map(move |c| self.fold(c))
    }

    /// Fold a whole string into `out` (replacing its contents). Used once per
    /// term for the needle, never per candidate name.
    pub fn fold_str(&self, s: &str, out: &mut String) {
        out.clear();
        out.reserve(s.len());
        out.extend(self.chars(s));
    }

    /// Whether `haystack` equals prefolded `needle`.
    pub fn eq_prefolded(&self, haystack: &str, needle: &str) -> bool {
        self.chars(haystack).eq(needle.chars())
    }

    /// Whether `haystack` contains `needle` under this folding, no allocation.
    pub fn contains_prefolded(&self, haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let mut starts = haystack.char_indices();
        loop {
            if self.starts_with_prefolded(starts.as_str(), needle) {
                return true;
            }
            if starts.next().is_none() {
                return false;
            }
        }
    }

    /// Whether `haystack` starts with prefolded `needle`.
    pub fn starts_with_prefolded(&self, haystack: &str, needle: &str) -> bool {
        let mut h = self.chars(haystack);
        for n in needle.chars() {
            match h.next() {
                Some(c) if c == n => {}
                _ => return false,
            }
        }
        true
    }

    /// Whether `haystack` ends with prefolded `needle`.
    pub fn ends_with_prefolded(&self, haystack: &str, needle: &str) -> bool {
        let mut h = self.chars(haystack).rev();
        for n in needle.chars().rev() {
            match h.next() {
                Some(c) if c == n => {}
                _ => return false,
            }
        }
        true
    }

    /// [`Folder::contains_prefolded`], but the match must be a whole word -
    /// the `wholeword:` modifier.
    pub fn contains_word_prefolded(&self, haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let len = needle.chars().count();
        let mut prev: Option<char> = None;
        let mut starts = haystack.char_indices();
        loop {
            let rest = starts.as_str();
            if prev.is_none_or(|c| !is_word(c))
                && self.starts_with_prefolded(rest, needle)
                && self.chars(rest).nth(len).is_none_or(|c| !is_word(c))
            {
                return true;
            }
            match starts.next() {
                Some((_, c)) => prev = Some(c),
                None => return false,
            }
        }
    }

    /// [`Folder::starts_with_prefolded`] with a word boundary after the match.
    pub fn starts_with_word_prefolded(&self, haystack: &str, needle: &str) -> bool {
        self.starts_with_prefolded(haystack, needle)
            && self
                .chars(haystack)
                .nth(needle.chars().count())
                .is_none_or(|c| !is_word(c))
    }

    /// [`Folder::ends_with_prefolded`] with a word boundary before the match.
    pub fn ends_with_word_prefolded(&self, haystack: &str, needle: &str) -> bool {
        if !self.ends_with_prefolded(haystack, needle) {
            return false;
        }
        let before = self
            .chars(haystack)
            .count()
            .saturating_sub(needle.chars().count());
        before == 0
            || self
                .chars(haystack)
                .nth(before - 1)
                .is_none_or(|c| !is_word(c))
    }
}

/// True for a character that only carries an accent - one half of a decomposed
/// letter, which diacritic-insensitive matching drops.
fn is_mark(c: char) -> bool {
    unicode_normalization::char::is_combining_mark(c)
}

/// What a character is without its diacritical marks.
///
/// Canonical decomposition puts the base character first, so an accented `e`
/// compares equal to a plain one - which is what a user typing `resume` and
/// expecting to find a file with the accents wants. ASCII short-circuits,
/// so the ordinary name pays nothing for this.
fn base_char(c: char) -> char {
    let mut base = c;
    let mut first = true;
    unicode_normalization::char::decompose_canonical(c, |part| {
        if first {
            base = part;
            first = false;
        }
    });
    base
}

/// What counts as inside a word, for `wholeword:`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default rule: case-insensitive, diacritic-insensitive.
    fn folder() -> Folder {
        Folder::new(&CaseFold::Simple, false, false, false)
    }

    /// A `$UpCase`-shaped table: identity except a->A style ASCII folding,
    /// plus one deliberate quirk so tests can tell it apart from Simple.
    fn quirky_table() -> Vec<u16> {
        let mut table: Vec<u16> = (0..UPCASE_LEN as u32).map(|c| c as u16).collect();
        for c in b'a'..=b'z' {
            table[c as usize] = u16::from(c - 32);
        }
        // The quirk: this volume says U+00B5 (micro sign) uppercases to
        // itself, where Unicode simple case maps it to U+039C (capital mu).
        table[0xB5] = 0xB5;
        table
    }

    #[test]
    fn simple_fold_compares_case_insensitively() {
        let fold = CaseFold::Simple;
        assert!(fold.eq("Report.PDF", "report.pdf"));
        assert!(fold.eq("\u{c9}COLE", "\u{e9}cole"));
        assert!(!fold.eq("a", "b"));
        assert!(!fold.eq("abc", "ab"));
    }

    #[test]
    fn table_fold_follows_the_volume_not_unicode() {
        let fold = CaseFold::from_upcase(quirky_table());
        assert!(matches!(fold, CaseFold::Table(_)));
        assert!(fold.eq("ABC", "abc"));
        // The volume's rule wins over Unicode's (micro sign, capital mu).
        assert_eq!(fold.fold('\u{b5}'), '\u{b5}');
        assert_eq!(CaseFold::Simple.fold('\u{b5}'), '\u{39c}');
    }

    #[test]
    fn a_malformed_table_falls_back_to_simple() {
        assert!(matches!(
            CaseFold::from_upcase(vec![0u16; 100]),
            CaseFold::Simple
        ));
    }

    #[test]
    fn contains_folds_the_haystack_on_the_fly() {
        let fold = folder();
        let mut needle = String::new();
        fold.fold_str("port", &mut needle);

        assert!(fold.contains_prefolded("Report.PDF", &needle));
        assert!(fold.contains_prefolded("PORTFOLIO", &needle));
        assert!(!fold.contains_prefolded("prt", &needle));
        assert!(fold.contains_prefolded("anything", ""));
    }

    #[test]
    fn prefix_and_suffix_checks_fold_too() {
        let fold = folder();
        let mut folded = String::new();

        fold.fold_str("rep", &mut folded);
        assert!(fold.starts_with_prefolded("REPORT", &folded));
        assert!(!fold.starts_with_prefolded("PORT", &folded));

        fold.fold_str(".pdf", &mut folded);
        assert!(fold.ends_with_prefolded("Report.PDF", &folded));
        assert!(!fold.ends_with_prefolded("Report.pdfx", &folded));

        fold.fold_str("Report.PDF", &mut folded);
        assert!(fold.eq_prefolded("report.pdf", &folded));
        assert!(!fold.eq_prefolded("report.pdf.bak", &folded));
    }

    #[test]
    fn diacritics_fold_unless_the_term_asks_for_them() {
        let mut needle = String::new();

        let loose = folder();
        loose.fold_str("resume", &mut needle);
        assert!(
            loose.contains_prefolded("re\u{301}sume\u{301}.doc", &needle),
            "combining marks"
        );
        assert!(
            loose.contains_prefolded("r\u{e9}sum\u{e9}.doc", &needle),
            "precomposed"
        );

        // `diacritics:` makes the accents count again.
        let strict = Folder::new(&CaseFold::Simple, false, true, false);
        strict.fold_str("resume", &mut needle);
        assert!(!strict.contains_prefolded("r\u{e9}sum\u{e9}.doc", &needle));
        assert!(strict.contains_prefolded("resume.doc", &needle));
    }

    #[test]
    fn case_and_ascii_modifiers_change_the_comparison() {
        let mut needle = String::new();

        let strict = Folder::new(&CaseFold::Simple, true, false, false);
        assert!(!strict.is_identity(), "marks still fold");
        strict.fold_str("Report", &mut needle);
        assert!(strict.contains_prefolded("Report.PDF", &needle));
        assert!(!strict.contains_prefolded("REPORT.PDF", &needle));

        // ASCII-only folding leaves non-ASCII case alone.
        let ascii = Folder::new(&CaseFold::Simple, false, true, true);
        ascii.fold_str("\u{c9}COLE", &mut needle);
        assert!(!ascii.contains_prefolded("\u{e9}cole", &needle));
        assert!(ascii.contains_prefolded("\u{c9}cole", &needle));
    }

    #[test]
    fn whole_word_matches_need_boundaries() {
        let fold = folder();
        let mut needle = String::new();

        fold.fold_str("port", &mut needle);
        assert!(fold.contains_word_prefolded("air port.txt", &needle));
        assert!(fold.contains_word_prefolded("port", &needle));
        assert!(!fold.contains_word_prefolded("Report.PDF", &needle));

        fold.fold_str("rep", &mut needle);
        assert!(fold.starts_with_word_prefolded("rep 1.txt", &needle));
        assert!(!fold.starts_with_word_prefolded("report.txt", &needle));

        fold.fold_str("pdf", &mut needle);
        assert!(fold.ends_with_word_prefolded("report.pdf", &needle));
        assert!(!fold.ends_with_word_prefolded("reportpdf", &needle));
    }

    #[test]
    fn ordering_ignores_case_and_respects_length() {
        let fold = CaseFold::Simple;
        assert_eq!(fold.cmp("abc", "ABC"), std::cmp::Ordering::Equal);
        assert_eq!(fold.cmp("ab", "abc"), std::cmp::Ordering::Less);
        assert_eq!(fold.cmp("b", "A"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn supplementary_plane_chars_pass_through() {
        let fold = CaseFold::from_upcase(quirky_table());
        assert_eq!(fold.fold('\u{1f980}'), '\u{1f980}');
        assert!(fold.eq("crab-\u{1f980}", "CRAB-\u{1f980}"));
    }
}
