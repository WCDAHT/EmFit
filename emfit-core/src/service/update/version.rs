//! Comparing the version we are against the version the manifest offers.
//!
//! The manifest writes `v0.8.0`; `CARGO_PKG_VERSION` gives `0.1.0`. Both are
//! three numbers, and that is the whole of what the site publishes, so this
//! parses exactly that rather than pulling in a full semver implementation.
//!
//! Anything after a `-` or `+` is dropped. Pre-release ordering is therefore
//! not modelled: `0.4.0-beta` compares equal to `0.4.0`. The site publishes no
//! pre-releases, and guessing at an ordering nobody uses would be worse than
//! saying plainly that there isn't one.

/// A parsed `major.minor.patch`. `None` for anything that is not three
/// numbers, which is treated as "cannot compare" rather than as zero - a
/// malformed manifest entry must not read as an ancient version and trigger
/// an update offer.
pub fn parse(text: &str) -> Option<[u64; 3]> {
    let text = text.trim();
    let text = text
        .strip_prefix('v')
        .or_else(|| text.strip_prefix('V'))
        .unwrap_or(text);
    let text = text.split(['-', '+']).next()?;

    let mut parts = text.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Whether `candidate` is a strictly higher version than `current`.
///
/// False when either side is unparseable: an offer we cannot justify is not
/// made.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse(candidate), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Whether `current` is ahead of what the site publishes - a developer build,
/// normally. Reported rather than hidden, so a dev running 0.5.0 against a
/// 0.4.0 manifest is told why nothing is on offer.
pub fn is_ahead(current: &str, candidate: &str) -> bool {
    is_newer(current, candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifests_v_prefix_is_optional() {
        assert_eq!(parse("v0.8.0"), Some([0, 8, 0]));
        assert_eq!(parse("0.8.0"), Some([0, 8, 0]));
        assert_eq!(parse(" v1.0.0 "), Some([1, 0, 0]));
    }

    #[test]
    fn anything_that_is_not_three_numbers_is_uncomparable() {
        assert_eq!(parse("0.8"), None);
        assert_eq!(parse("0.8.0.1"), None);
        assert_eq!(parse("latest"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("0.x.0"), None);
    }

    #[test]
    fn build_metadata_is_dropped() {
        assert_eq!(parse("v1.2.3-beta.1"), Some([1, 2, 3]));
        assert_eq!(parse("1.2.3+build7"), Some([1, 2, 3]));
    }

    #[test]
    fn newer_compares_field_by_field() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
    }

    #[test]
    fn ten_is_above_nine_not_below_it() {
        // The string compare this replaces gets this one wrong.
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(!is_newer("v0.9.0", "0.10.0"));
    }

    #[test]
    fn an_unreadable_version_never_offers_an_update() {
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("v0.2.0", "nightly"));
    }

    #[test]
    fn a_dev_build_is_reported_as_ahead() {
        assert!(is_ahead("0.5.0", "v0.4.0"));
        assert!(!is_ahead("0.4.0", "v0.4.0"));
    }
}
