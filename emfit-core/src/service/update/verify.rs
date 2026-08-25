//! Checking that a downloaded release is the file the manifest described.
//!
//! **Developer note - this currently verifies nothing, by design of the site
//! rather than of this module.** The published manifest carries no checksum,
//! so [`Release::sha256`](super::manifest::Release::sha256) is always `None`
//! and [`check`] returns `Ok` without hashing. The plumbing is here so that the
//! day the site publishes a `sha256` field per release, integrity checking
//! turns on with no change to this crate and no change to the shell.
//!
//! Why it is worth closing that gap: HTTPS authenticates the *host*, not the
//! bytes it serves. EmFit runs elevated for raw volume access, so a release
//! swapped on the origin - or served by anything that can terminate TLS for
//! that name - would be executed with Administrator rights. A hash in the
//! manifest raises the bar to "compromise the manifest too"; an Authenticode
//! signature on the binary raises it further and also stops SmartScreen
//! warning users on first run.
//!
//! Nothing here is surfaced to the user. A mismatch reports as a download that
//! did not complete correctly, which is both the honest common case and the
//! right instruction either way: try again.

use std::fs;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Read chunk for hashing. Matches the download chunk; the file is warm in the
/// page cache at this point either way.
const CHUNK: usize = 64 * 1024;

/// Verify `path` against the manifest's expectation.
///
/// `expected` is the hex SHA-256 from the manifest, or `None` when the
/// manifest does not publish one - in which case there is nothing to check and
/// this succeeds.
pub fn check(path: &Path, expected: Option<&str>) -> Result<()> {
    let Some(expected) = expected.map(str::trim).filter(|e| !e.is_empty()) else {
        tracing::debug!(
            path = %path.display(),
            "update: no checksum published for this release; nothing to verify"
        );
        return Ok(());
    };

    let actual = sha256(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        tracing::info!(path = %path.display(), "update: checksum matches");
        return Ok(());
    }

    tracing::error!(
        path = %path.display(),
        expected,
        actual,
        "update: checksum mismatch; discarding the download"
    );
    // Never leave a file we could not vouch for sitting where a user might
    // run it by hand.
    let _ = fs::remove_file(path);
    Err(Error::Network {
        url: path.display().to_string(),
        message: "the download did not complete correctly".to_string(),
    })
}

/// Hex SHA-256 of a file, read in chunks so a large release is not held in
/// memory twice.
pub fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = file.read(&mut buffer).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("emfit-verify-{}-{tag}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        d.join("release.bin")
    }

    #[test]
    fn hashes_match_the_known_vector() {
        let path = tmp("known");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn an_unpublished_checksum_is_not_a_failure() {
        let path = tmp("none");
        fs::write(&path, b"abc").unwrap();
        assert!(check(&path, None).is_ok());
        assert!(check(&path, Some("")).is_ok());
        assert!(path.exists(), "nothing to check must not delete the file");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_published_checksum_is_honoured_once_the_site_carries_one() {
        let path = tmp("match");
        fs::write(&path, b"abc").unwrap();
        assert!(
            check(
                &path,
                Some("BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD")
            )
            .is_ok(),
            "hex case must not decide the outcome"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_mismatch_removes_the_file() {
        let path = tmp("mismatch");
        fs::write(&path, b"abc").unwrap();
        assert!(check(&path, Some("00".repeat(32).as_str())).is_err());
        assert!(
            !path.exists(),
            "a file we cannot vouch for must not survive"
        );
    }
}
