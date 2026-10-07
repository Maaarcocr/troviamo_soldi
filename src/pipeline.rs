use crate::{
    engine,
    model::{self, ModelClient},
    store::Store,
    types::{DocumentVersion, Notice, RunSummary, Usage},
};
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io::Read, path::PathBuf};
use url::Url;

pub struct RunOptions {
    pub limit: usize,
    pub cache_dir: PathBuf,
    pub allowed_hosts: Vec<String>,
    pub allow_localhost: bool,
    pub retry_failed: bool,
    pub dry_run: bool,
    pub max_file_bytes: usize,
    pub max_notice_bytes: usize,
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn run(
    store: &mut Store,
    client: &Client,
    model: Option<&ModelClient>,
    notices: Vec<Notice>,
    options: &RunOptions,
) -> Result<RunSummary> {
    ensure!(options.limit > 0, "--limit must be greater than zero");
    std::fs::create_dir_all(&options.cache_dir)?;
    let mut summary = RunSummary {
        discovered: notices.len(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    for notice in notices {
        if summary.processed >= options.limit {
            break;
        }
        if !seen.insert(notice.id.clone()) {
            continue;
        }
        ensure!(
            !notice.id.is_empty() && notice.id.len() <= 300,
            "Invalid notice ID"
        );
        let previous = store.previous(&notice.id)?;
        let prepared = prepare(client, &notice, options);
        let documents = match prepared {
            Ok(docs) => docs,
            Err(error) => {
                let fingerprint = sha256(
                    format!("download-failure:{}", serde_json::to_string(&notice)?).as_bytes(),
                );
                if !options.retry_failed
                    && let Some((version, _)) = store.cached(&notice.id, &fingerprint)?
                {
                    if !options.dry_run {
                        store.select_cached_version(&notice, version)?;
                    }
                    summary.unchanged += 1;
                    continue;
                }
                summary.processed += 1;
                summary.failed += 1;
                if !options.dry_run {
                    let version = store.begin_version(&notice, &fingerprint, &[])?;
                    store.finish_version(version, "failed", None, Some(&format!("{error:#}")))?;
                }
                eprintln!(
                    "{}: needs review (download/input error: {error:#})",
                    notice.id
                );
                continue;
            }
        };
        let fingerprint = fingerprint(&notice, &documents)?;
        let cached = store.cached(&notice.id, &fingerprint)?;
        if let Some((version, state)) = &cached {
            let retryable =
                ["failed", "in_flight", "pending", "needs_review"].contains(&state.as_str());
            if !options.retry_failed || !retryable {
                if !options.dry_run {
                    store.select_cached_version(&notice, *version)?;
                }
                summary.unchanged += 1;
                continue;
            }
        }
        // An attempted notice consumes exactly one slot, even on failure or review.
        summary.processed += 1;
        if options.dry_run {
            eprintln!(
                "{}: would process {} original files",
                notice.id,
                documents.len()
            );
            continue;
        }
        let version = store.begin_version(&notice, &fingerprint, &documents)?;
        let model = model.context("Model client missing for non-dry run")?;
        let body =
            match model::request_body(&notice, &documents, &options.cache_dir, model.max_tokens) {
                Ok(body) => body,
                Err(error) => {
                    summary.failed += 1;
                    store.finish_version(version, "failed", None, Some(&error.to_string()))?;
                    continue;
                }
            };
        let attempt = store.attempt(version, &sha256(serde_json::to_string(&body)?.as_bytes()))?;
        summary.calls += 1;
        let response = model.send(&body);
        match response {
            Err(error) => {
                let usage = Usage::default();
                summary.add_usage(&usage);
                summary.failed += 1;
                let error = format!("{error:#}");
                store.finish_attempt(attempt, "failed", None, &usage, Some(&error))?;
                store.finish_version(version, "failed", None, Some(&error))?;
                eprintln!("{}: failed; no automatic retry", notice.id);
            }
            Ok(response) => {
                let usage = model::usage(&response);
                summary.add_usage(&usage);
                let sources: Vec<_> = std::iter::once(notice.source_url.clone())
                    .chain(documents.iter().map(|d| d.url.clone()))
                    .collect();
                match model::parse_response(&response, &sources)
                    .and_then(|facts| crate::contract::to_stored(&facts))
                {
                    Ok(mut extraction) => {
                        let removed = previous
                            .as_ref()
                            .map(|old| {
                                old.iter()
                                    .filter(|d| {
                                        !documents.iter().any(|current| current.url == d.url)
                                    })
                                    .map(|d| d.filename.clone())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        if !removed.is_empty() {
                            extraction["review_reasons"].as_array_mut().unwrap().push(json!(format!("Previously supplied files are no longer listed: {}. Verify whether their requirements still apply.",removed.join(", "))));
                        }
                        let unsupported: Vec<_> = documents
                            .iter()
                            .filter(|d| !model::supported_mime(&d.mime_type))
                            .map(|d| d.filename.clone())
                            .collect();
                        if !unsupported.is_empty() {
                            extraction["review_reasons"].as_array_mut().unwrap().push(json!(format!("Original files not readable by this native PDF/image path: {}. Requirements may be incomplete.",unsupported.join(", "))));
                        }

                        let reasons = engine::extraction_review_reasons(&extraction);
                        let source_uncertainty = extraction["review_reasons"]
                            .as_array()
                            .is_some_and(|a| !a.is_empty());
                        let state = if source_uncertainty {
                            summary.needs_review += 1;
                            "needs_review"
                        } else {
                            summary.extracted += 1;
                            "extracted"
                        };
                        // Source extraction and local municipality screening are separate steps.
                        store.finish_attempt(attempt, state, Some(&response), &usage, None)?;
                        store.finish_version(version, state, Some(&extraction), None)?;
                        eprintln!(
                            "{}: {state}; cost {} USD",
                            notice.id,
                            usage
                                .cost_usd
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "unknown".into())
                        );
                        for reason in reasons {
                            eprintln!("  unresolved fact: {reason}");
                        }
                    }
                    Err(error) => {
                        summary.needs_review += 1;
                        let error = format!("{error:#}");
                        store.finish_attempt(
                            attempt,
                            "needs_review",
                            Some(&response),
                            &usage,
                            Some(&error),
                        )?;
                        store.finish_version(version, "needs_review", None, Some(&error))?;
                        eprintln!("{}: needs review ({error})", notice.id);
                    }
                }
            }
        }
    }
    summary.finish();
    if !options.dry_run {
        store.save_run(&serde_json::to_value(&summary)?)?;
    }
    Ok(summary)
}
fn fingerprint(notice: &Notice, docs: &[DocumentVersion]) -> Result<String> {
    let mut ordered: Vec<_> = docs
        .iter()
        .map(|d| json!({"url":d.url,"sha256":d.sha256,"mime_type":d.mime_type}))
        .collect();
    ordered.sort_by_key(Value::to_string);
    Ok(sha256(serde_json::to_string(&json!({"id":notice.id,"title":notice.title,"source_url":notice.source_url,"source_text":notice.source_text,"documents":ordered}))?.as_bytes()))
}
fn prepare(client: &Client, notice: &Notice, options: &RunOptions) -> Result<Vec<DocumentVersion>> {
    validate_url(&notice.source_url, options)?;
    ensure!(
        notice.documents.len() <= 20,
        "More than 20 files; application limit requires manual review"
    );
    let has_source_text = notice
        .source_text
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    ensure!(
        has_source_text || !notice.documents.is_empty(),
        "No source content supplied: provide source_text or original documents, or use source discovery; a URL alone is not extraction input. No model call made"
    );
    ensure!(
        notice
            .source_text
            .as_ref()
            .is_none_or(|s| s.len() <= 2 * 1024 * 1024),
        "Source text exceeds application limit; not truncated"
    );
    let mut documents = Vec::new();
    let mut total = 0;
    let mut urls = HashSet::new();
    for document in &notice.documents {
        if !urls.insert(&document.url) {
            continue;
        }
        let (bytes, server_mime) =
            download(client, &document.url, options, options.max_file_bytes)?;
        total += bytes.len();
        ensure!(
            total <= options.max_notice_bytes,
            "Original files exceed per-notice application byte limit; no model call"
        );
        let mime = sniff_type(&bytes).unwrap_or("application/octet-stream");
        ensure!(
            !server_mime.starts_with("text/html")
                && !bytes.starts_with(b"<html")
                && !bytes.starts_with(b"<!DOCTYPE html"),
            "Attachment returned HTML instead of an original document"
        );
        let hash = sha256(&bytes);
        let path = options.cache_dir.join(&hash);
        if !path.exists() || sha256(&std::fs::read(&path)?) != hash {
            std::fs::write(&path, &bytes)?;
        }
        let filename = if document.filename.trim().is_empty() {
            format!(
                "document.{}",
                if mime == "application/pdf" {
                    "pdf"
                } else {
                    "image"
                }
            )
        } else {
            document.filename.clone()
        };
        documents.push(DocumentVersion {
            url: document.url.clone(),
            filename,
            mime_type: mime.into(),
            sha256: hash,
            bytes: bytes.len() as u64,
        });
    }
    ensure!(
        has_source_text
            || documents
                .iter()
                .any(|d| model::supported_mime(&d.mime_type)),
        "No readable source content: all original documents are unsupported and source_text is empty. No model call made"
    );
    Ok(documents)
}
fn sniff_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Some("application/pdf")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}
fn validate_url(value: &str, options: &RunOptions) -> Result<Url> {
    let url = Url::parse(value)?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are forbidden"
    );
    let host = url.host_str().context("URL host missing")?;
    let local = host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1";
    if options.allow_localhost && local {
        ensure!(
            url.scheme() == "http" || url.scheme() == "https",
            "Bad local URL"
        );
        return Ok(url);
    }
    ensure!(url.scheme() == "https", "Only HTTPS sources are allowed");
    ensure!(
        options.allowed_hosts.iter().any(|allowed| allowed == host
            || allowed
                .strip_prefix("*.")
                .is_some_and(|suffix| host.ends_with(&format!(".{suffix}")))),
        "Source host {host} is not in allowed_hosts"
    );
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        ensure!(
            !ip.is_loopback() && !ip.is_unspecified(),
            "Private source URL forbidden"
        );
        bail!("Literal IP source URLs are forbidden");
    }
    Ok(url)
}
fn download(
    client: &Client,
    value: &str,
    options: &RunOptions,
    max_bytes: usize,
) -> Result<(Vec<u8>, String)> {
    let mut url = validate_url(value, options)?;
    for _ in 0..=5 {
        let response = client
            .get(url.clone())
            .send()
            .context("Fetch original source")?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("Redirect without location")?
                .to_str()?;
            url = validate_url(url.join(location)?.as_str(), options)?;
            continue;
        }
        ensure!(
            response.status().is_success(),
            "Download returned HTTP {} for {}",
            response.status(),
            url
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|len| len <= max_bytes as u64),
            "Download exceeds configured application byte limit"
        );
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap()
            .trim()
            .to_owned();
        let mut bytes = Vec::new();
        response
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= max_bytes,
            "Download exceeds configured application byte limit"
        );
        return Ok((bytes, mime));
    }
    bail!("Too many redirects")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fingerprints_ignore_document_order() {
        let n = Notice {
            id: "x".into(),
            title: "x".into(),
            source_url: "https://example.org".into(),
            source_text: None,
            documents: vec![],
        };
        let d = |url: &str| DocumentVersion {
            url: url.into(),
            filename: "x.pdf".into(),
            mime_type: "application/pdf".into(),
            sha256: url.into(),
            bytes: 12,
        };
        assert_eq!(
            fingerprint(&n, &[d("a"), d("b")]).unwrap(),
            fingerprint(&n, &[d("b"), d("a")]).unwrap()
        );
    }
    #[test]
    fn no_html_masquerading_as_pdf() {
        assert!(sniff_type(b"<html>wrong</html>").is_none());
        assert_eq!(sniff_type(b"%PDF-1.4 raw bytes"), Some("application/pdf"));
    }
}
