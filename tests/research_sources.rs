//! Component checks for arithmetic and the real filesystem boundary. These do
//! not call a model, simulate research responses, or encode an eligibility story.
use funding_rust::research_sources::{ResearchSources, calculate, public_ip, public_url};
use serde_json::json;
use std::{fs, net::IpAddr};
use tempfile::tempdir;

#[test]
fn arithmetic_has_no_code_execution_surface() {
    assert_eq!(calculate("(12 + 3) * 2 / 5").unwrap(), 6.0);
    assert_eq!(calculate("-2 * -(4.5 - 1.5)").unwrap(), 6.0);
    for invalid in ["1/0", "NaN", "1; ls", "sqrt(4)", "1 +", "2(3)"] {
        assert!(calculate(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn source_urls_require_public_https() {
    assert!(public_url("https://www.regione.sicilia.it/page#anchor").is_ok());
    for invalid in [
        "http://www.regione.sicilia.it/",
        "https://user:password@www.regione.sicilia.it/",
        "https://www.regione.sicilia.it:8443/",
        "https://127.0.0.1/",
        "https://2130706433/",
        "https://169.254.169.254/",
        "https://[::1]/",
        "https://[::ffff:127.0.0.1]/",
        "https://metadata.google.internal/",
        "https://localhost./",
        "file:///etc/passwd",
    ] {
        assert!(public_url(invalid).is_err(), "{invalid}");
    }
    for private in [
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "100.64.0.1",
        "198.18.0.1",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "fc00::1",
        "fe80::1",
        "2001:db8::1",
        "2002:7f00:1::",
    ] {
        assert!(!public_ip(private.parse::<IpAddr>().unwrap()), "{private}");
    }
    assert!(public_ip("8.8.8.8".parse().unwrap()));
    assert!(public_ip("2001:4860:4860::8888".parse().unwrap()));
}

#[test]
fn workspace_reads_snapshot_bytes_and_preserve_distinct_origins() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("one.txt"), "Arithmetic notes: 2 + 2 = 4.").unwrap();
    fs::write(dir.path().join("two.txt"), "Arithmetic notes: 2 + 2 = 4.").unwrap();
    let mut sources = ResearchSources::from_env(dir.path()).unwrap();
    assert!(
        sources
            .read_document("../../etc/passwd", &json!({}))
            .is_err()
    );
    let listing = sources.search_workspace("Arithmetic").unwrap();
    let first = listing["files"][0]["file_id"].as_str().unwrap();
    let second = listing["files"][1]["file_id"].as_str().unwrap();
    let a = sources.read_document(first, &json!({})).unwrap();
    let b = sources.read_document(second, &json!({})).unwrap();
    assert_eq!(a["source"]["content_hash"], b["source"]["content_hash"]);
    assert_ne!(a["source"]["file_id"], b["source"]["file_id"]);
    assert_ne!(a["source"]["url"], b["source"]["url"]);
    assert_eq!(a["source"]["origin_kind"], "local_workspace_unverified");
    fs::write(
        dir.path().join("one.txt"),
        "Changed after original snapshot.",
    )
    .unwrap();
    let saved = sources
        .read_document(a["source"]["file_id"].as_str().unwrap(), &json!({}))
        .unwrap();
    assert_eq!(saved["source"]["body"], "Arithmetic notes: 2 + 2 = 4.");
    let window = sources
        .read_document(
            a["source"]["file_id"].as_str().unwrap(),
            &json!({"offset":0,"limit":10}),
        )
        .unwrap();
    assert_eq!(window["document"]["next_offset"], 10);
    assert_eq!(window["document"]["truncated"], true);
}

#[cfg(unix)]
#[test]
fn workspace_symlinks_are_not_read() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("outside.txt"), "Outside the workspace").unwrap();
    symlink(
        outside.path().join("outside.txt"),
        dir.path().join("link.txt"),
    )
    .unwrap();
    let mut sources = ResearchSources::from_env(dir.path()).unwrap();
    assert!(
        sources.search_workspace("").unwrap()["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fs::remove_dir(dir.path().join("source_versions")).unwrap();
    symlink(outside.path(), dir.path().join("source_versions")).unwrap();
    assert!(sources.search_workspace("").is_err());
}
