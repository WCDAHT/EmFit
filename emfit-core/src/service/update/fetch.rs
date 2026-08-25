//! The only place in EmFit that touches the network.
//!
//! Deliberately one small module. Everything else in the crate reads disks;
//! keeping the socket behind one door means the question "what does this app
//! talk to" has a one-file answer, which matters for a tool that runs elevated
//! on evidence machines.
//!
//! Three rules hold here:
//! - **Nothing is fetched unless the user asked.** There is no call site above
//!   this that runs on a timer.
//! - **Every read is bounded.** A manifest is kilobytes and a release is tens
//!   of megabytes; a server that streams forever gets cut off rather than
//!   filling the disk.
//! - **HTTPS only.** A plain-http URL is refused before a connection is made,
//!   so a redirect or a hand-edited config cannot downgrade the transport.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::service::task::CancellationToken;

/// Ceiling on the manifest. It holds a handful of small records; a megabyte is
/// already two orders of magnitude more than it will ever be.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Ceiling on the release notes shown in the dialog.
const MAX_NOTES_BYTES: u64 = 512 * 1024;

/// Ceiling on a downloaded release. EmFit's own bundle is a few tens of
/// megabytes; this is a runaway guard, not a budget.
const MAX_RELEASE_BYTES: u64 = 512 * 1024 * 1024;

/// How long the whole exchange may take, connection included.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How often a download reports progress. Per-chunk would flood the IPC
/// channel on a fast link for no visible benefit.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Read chunk. Large enough that the syscall cost disappears, small enough
/// that cancelling is felt immediately.
const CHUNK: usize = 64 * 1024;

/// One agent for the process, so connections are pooled across the manifest,
/// the notes, and the download.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = ureq::tls::TlsConfig::builder()
            // Validate against the machine's own certificate store rather than
            // a root set baked into our binary. On a network that inspects TLS
            // - which a county office plausibly runs - the bundled roots would
            // reject every connection and there would be nothing the user
            // could do about it.
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(TIMEOUT))
            .user_agent(format!("EmFit/{}", env!("CARGO_PKG_VERSION")))
            .build();
        ureq::Agent::new_with_config(config)
    })
}

/// Refuse anything that is not HTTPS.
fn require_https(url: &str) -> Result<()> {
    if url.starts_with("https://") {
        return Ok(());
    }
    Err(Error::Network {
        url: url.to_string(),
        message: "refusing a URL that is not https".to_string(),
    })
}

fn net_error(url: &str, e: impl std::fmt::Display) -> Error {
    Error::Network {
        url: url.to_string(),
        message: e.to_string(),
    }
}

/// Fetch a small text document: the manifest, or a release's notes.
pub fn get_text(url: &str, limit: u64) -> Result<String> {
    require_https(url)?;
    tracing::debug!(url, "update: fetching");

    let mut response = agent().get(url).call().map_err(|e| net_error(url, e))?;

    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_string()
        .map_err(|e| net_error(url, e))
}

/// Fetch the manifest.
pub fn manifest(url: &str) -> Result<String> {
    get_text(url, MAX_MANIFEST_BYTES)
}

/// Fetch release notes. Their absence is not a failure - the dialog simply
/// has nothing to show - so this swallows the error and logs it.
pub fn notes(url: &str) -> Option<String> {
    match get_text(url, MAX_NOTES_BYTES) {
        Ok(text) => Some(text),
        Err(e) => {
            tracing::warn!(url, error = %e, "update: release notes unavailable");
            None
        }
    }
}

/// Download a release to `dest`, reporting bytes as it goes.
///
/// Writes to `dest` with a `.part` suffix and renames on success, so a
/// half-finished download is never mistaken for a usable binary - the same
/// discipline the scan cache uses (`caching.md`).
///
/// `progress` receives `(downloaded, total)`, where `total` is `None` when the
/// server sends no content length.
pub fn download(
    url: &str,
    dest: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &CancellationToken,
) -> Result<()> {
    require_https(url)?;

    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }

    let mut response = agent().get(url).call().map_err(|e| net_error(url, e))?;

    let total = response.body().content_length();
    if total.is_some_and(|n| n > MAX_RELEASE_BYTES) {
        return Err(Error::Network {
            url: url.to_string(),
            message: format!("the release is larger than the {MAX_RELEASE_BYTES} byte ceiling"),
        });
    }
    tracing::info!(url, ?total, dest = %dest.display(), "update: download starting");

    let part = dest.with_extension("part");
    let outcome = stream_to_file(url, &mut response, &part, total, progress, cancel);

    if outcome.is_err() {
        // Nothing usable and nothing to resume from; leaving it behind would
        // only accumulate junk in the staging directory.
        let _ = fs::remove_file(&part);
        return outcome;
    }

    fs::rename(&part, dest).map_err(|source| Error::Io {
        path: dest.to_path_buf(),
        source,
    })
}

fn stream_to_file(
    url: &str,
    response: &mut ureq::http::Response<ureq::Body>,
    part: &Path,
    total: Option<u64>,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &CancellationToken,
) -> Result<()> {
    let mut file = fs::File::create(part).map_err(|source| Error::Io {
        path: part.to_path_buf(),
        source,
    })?;

    let mut reader = response
        .body_mut()
        .with_config()
        .limit(MAX_RELEASE_BYTES)
        .reader();

    let mut buffer = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    let mut last_report = std::time::Instant::now();
    progress(0, total);

    loop {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let read = reader.read(&mut buffer).map_err(|e| net_error(url, e))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|source| Error::Io {
                path: part.to_path_buf(),
                source,
            })?;
        done += read as u64;

        if last_report.elapsed() >= PROGRESS_INTERVAL {
            progress(done, total);
            last_report = std::time::Instant::now();
        }
    }

    // A truncated response that the server closed cleanly reads as success
    // here; the length check is what catches it.
    if let Some(expected) = total
        && done != expected
    {
        return Err(Error::Network {
            url: url.to_string(),
            message: format!("received {done} of {expected} bytes"),
        });
    }

    file.sync_all().map_err(|source| Error::Io {
        path: part.to_path_buf(),
        source,
    })?;
    progress(done, total);
    tracing::info!(url, bytes = done, "update: download finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_refused_before_a_connection_is_made() {
        let err = require_https("http://example.com/manifest.json").unwrap_err();
        assert!(matches!(err, Error::Network { .. }));
        assert!(err.to_string().contains("https"));
    }

    #[test]
    fn https_passes() {
        assert!(require_https("https://example.com/manifest.json").is_ok());
    }

    #[test]
    fn a_file_url_is_refused_too() {
        assert!(require_https(r"file:///C:/manifest.json").is_err());
    }
}
