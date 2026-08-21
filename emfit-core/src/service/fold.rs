//! Case folding, per volume rather than per program.
//!
//! Search must compare names the way the *filesystem* does, not the way Rust's
//! `to_lowercase` does. On NTFS the authority is `$UpCase` - a 128 KiB table
//! in MFT record 10 mapping every UTF-16 code unit to its uppercase form -
//! which is what the filesystem itself consults for name comparisons. Other
//! filesystems supply their own rule or none, so folding is a value handed
//! over by the scan, not a hardcoded function (features.md sec 2).
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

    /// Whether `haystack` contains `needle` under this folding, no allocation.
    ///
    /// `needle` should already be folded (via [`CaseFold::fold_str`]) - the
    /// query does that once; the haystack is folded on the fly per character.
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
        let mut h = haystack.chars();
        for n in needle.chars() {
            match h.next() {
                Some(c) if self.fold(c) == n => {}
                _ => return false,
            }
        }
        true
    }

    /// Whether `haystack` ends with prefolded `needle`.
    pub fn ends_with_prefolded(&self, haystack: &str, needle: &str) -> bool {
        let mut h = haystack.chars().rev();
        for n in needle.chars().rev() {
            match h.next() {
                Some(c) if self.fold(c) == n => {}
                _ => return false,
            }
        }
        true
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let fold = CaseFold::Simple;
        let mut needle = String::new();
        fold.fold_str("port", &mut needle);

        assert!(fold.contains_prefolded("Report.PDF", &needle));
        assert!(fold.contains_prefolded("PORTFOLIO", &needle));
        assert!(!fold.contains_prefolded("prt", &needle));
        assert!(fold.contains_prefolded("anything", ""));
    }

    #[test]
    fn prefix_and_suffix_checks_fold_too() {
        let fold = CaseFold::Simple;
        let mut folded = String::new();

        fold.fold_str("rep", &mut folded);
        assert!(fold.starts_with_prefolded("REPORT", &folded));
        assert!(!fold.starts_with_prefolded("PORT", &folded));

        fold.fold_str(".pdf", &mut folded);
        assert!(fold.ends_with_prefolded("Report.PDF", &folded));
        assert!(!fold.ends_with_prefolded("Report.pdfx", &folded));
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
