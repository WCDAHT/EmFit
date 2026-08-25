//! The update check against a real manifest (STANDARDS sec 5.1: every format
//! gets a parser test with a real sample).
//!
//! `fixtures/release_manifest.json` is the document the website actually
//! serves today, verbatim - six products, none of them EmFit. That is the more
//! interesting half of this test: the app has to read the shared manifest of a
//! site it is not yet published on and say so, rather than fail.

use emfit_core::app;
use emfit_core::service::update::manifest;
use emfit_core::service::update::version;

const MANIFEST_URL: &str = "https://brunchtools.com/releases/manifest.json";

fn fixture() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/release_manifest.json"),
    )
    .expect("the release manifest fixture is missing")
}

#[test]
fn the_live_manifest_parses_whole() {
    let parsed = manifest::parse(&fixture()).expect("the site's manifest must parse");
    assert_eq!(parsed.len(), 6);
    for (name, release) in &parsed {
        assert!(
            version::parse(&release.version).is_some(),
            "{name} publishes a version this app cannot compare: {}",
            release.version
        );
        assert!(!release.release_file.is_empty());
    }
}

#[test]
fn emfit_is_not_published_yet_and_that_is_not_an_error() {
    let parsed = manifest::parse(&fixture()).unwrap();
    assert!(
        !parsed.contains_key(app::MANIFEST_KEY),
        "when EmFit is published, update this test to assert the offer instead"
    );
}

#[test]
fn a_published_emfit_entry_resolves_to_a_download() {
    // The shape a release will take once the site carries one.
    let published = r#"{
      "EmFit": {
        "version": "v0.4.0",
        "released_at": "2026-08-17",
        "original_filename": "EmFit-windows-x64.exe",
        "release_file": "content/EmFit/EmFit-windows-x64.exe",
        "readme_file": "content/EmFit/README.md"
      }
    }"#;
    let parsed = manifest::parse(published).unwrap();
    let release = &parsed[app::MANIFEST_KEY];

    assert!(version::is_newer(&release.version, "0.1.0"));
    assert!(!version::is_newer(&release.version, "0.4.0"));
    assert_eq!(
        manifest::resolve(MANIFEST_URL, &release.release_file),
        "https://brunchtools.com/releases/content/EmFit/EmFit-windows-x64.exe"
    );
    assert_eq!(release.file_name(), "EmFit-windows-x64.exe");
}

#[test]
fn every_products_release_url_resolves_under_the_manifest() {
    let parsed = manifest::parse(&fixture()).unwrap();
    for release in parsed.values() {
        let url = manifest::resolve(MANIFEST_URL, &release.release_file);
        assert!(
            url.starts_with("https://brunchtools.com/releases/content/"),
            "a relative release path must resolve under the manifest, got {url}"
        );
    }
}
