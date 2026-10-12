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
use rayon::prelude::*;
use std::{collections::HashSet, io::Read, path::PathBuf};
use url::Url;

pub struct RunOptions {
    pub limit: usize,
    pub cache_dir: PathBuf,
    pub allowed_hosts: Vec<String>,
    pub allow_localhost: bool,
    pub retry_failed: bool,
    pub dry_run: bool,
}

struct WorkItem {
    version: i64,
    attempt: i64,
    body: Value,
    notice: Notice,
    documents: Vec<DocumentVersion>,
    previous: Option<Vec<DocumentVersion>>,
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
    let mut work_items: Vec<WorkItem> = Vec::new();
    // Phase 1: serial preparation (DB reads/writes, document download)
    for notice in notices {
        if summary.processed >= options.limit {
            break;
        }
        if !seen.insert(notice.id.clone()) {
            continue;
        }
        ensure!(!notice.id.is_empty(), "Invalid notice ID");
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
        let body = match model::request_body(&notice, &documents, &options.cache_dir) {
            Ok(body) => body,
            Err(error) => {
                summary.failed += 1;
                store.finish_version(version, "failed", None, Some(&error.to_string()))?;
                continue;
            }
        };
        let attempt = store.attempt(version, &sha256(serde_json::to_string(&body)?.as_bytes()))?;
        summary.calls += 1;
        work_items.push(WorkItem {
            version,
            attempt,
            body,
            notice: notice.clone(),
            documents: documents.clone(),
            previous: previous.clone(),
        });
    }

    // Phase 2: parallel AI calls (rayon)
    let model_client = model.context("Model client missing for non-dry run")?;
    let responses: Vec<_> = work_items
        .par_iter()
        .map(|w| model_client.send(&w.body))
        .collect();

    // Phase 3: serial result processing (DB writes)
    for (work_item, response) in work_items.into_iter().zip(responses.into_iter()) {
        let WorkItem { version, attempt, notice, documents, previous, .. } = work_item;
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
    let has_source_text = notice
        .source_text
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    ensure!(
        has_source_text || !notice.documents.is_empty(),
        "No source content supplied: provide source_text or original documents, or use source discovery; a URL alone is not extraction input. No model call made"
    );
    let mut documents = Vec::new();
    let mut urls = HashSet::new();
    for document in &notice.documents {
        if !urls.insert(&document.url) {
            continue;
        }
        let (bytes, server_mime) = download(client, &document.url, options)?;
        ensure!(
            !server_mime.starts_with("text/html")
                && !bytes.starts_with(b"<html")
                && !bytes.starts_with(b"<!DOCTYPE html"),
            "Attachment returned HTML instead of an original document"
        );
        // Regione portals bundle the bando PDFs inside zip archives served as
        // application/octet-stream; expand them so the model actually sees them.
        let files: Vec<(String, Vec<u8>)> = if bytes.starts_with(b"PK\x03\x04") {
            let mut expanded: Vec<(String, Vec<u8>)> = unzip_pdfs(&bytes)
                .unwrap_or_default()
                .into_iter()
                .map(|(name, data)| (format!("{}/{}", document.filename, name), data))
                .collect();
            if expanded.is_empty() {
                // Unreadable archive or no PDFs inside: keep the archive itself as an
                // unsupported document so the review path flags it instead of
                // silently dropping it.
                expanded.push((document.filename.clone(), bytes));
            }
            expanded
        } else {
            vec![(document.filename.clone(), bytes)]
        };
        for (filename, data) in files {
            let mime = sniff_type(&data).unwrap_or("application/octet-stream");
            let hash = sha256(&data);
            let path = options.cache_dir.join(&hash);
            if !path.exists() || sha256(&std::fs::read(&path)?) != hash {
                std::fs::write(&path, &data)?;
            }
            let filename = if filename.trim().is_empty() {
                format!(
                    "document.{}",
                    if mime == "application/pdf" {
                        "pdf"
                    } else {
                        "image"
                    }
                )
            } else {
                filename
            };
            documents.push(DocumentVersion {
                url: document.url.clone(),
                filename,
                mime_type: mime.into(),
                sha256: hash,
                bytes: data.len() as u64,
            });
        }
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
/// Extract readable PDFs from a zip archive. Skips non-PDF entries, entries that
/// fail to decompress, and oversized files (zip-bomb guard).
fn unzip_pdfs(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    const MAX_ENTRY_BYTES: u64 = 10 * 1024 * 1024;
    const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("Unreadable zip archive")?;
    let mut out = Vec::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).context("Unreadable zip entry")?;
        if entry.is_dir() || total >= MAX_TOTAL_BYTES {
            continue;
        }
        let name = entry.name().map(|s| s.into_owned()).unwrap_or_default();
        if !name.to_lowercase().ends_with(".pdf") {
            continue;
        }
        let mut buf = Vec::new();
        entry
            .take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut buf)
            .context("Unreadable zip entry")?;
        if buf.len() as u64 > MAX_ENTRY_BYTES || !buf.starts_with(b"%PDF-") {
            continue;
        }
        total += buf.len() as u64;
        let filename = name
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("document.pdf")
            .to_string();
        out.push((filename, buf));
    }
    Ok(out)
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
fn download(client: &Client, value: &str, options: &RunOptions) -> Result<(Vec<u8>, String)> {
    let mut url = validate_url(value, options)?;
    for _ in 0..=5 {
        let mut response = client
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
        response.read_to_end(&mut bytes)?;
        return Ok((bytes, mime));
    }
    bail!("Too many redirects")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn zip_expansion_extracts_pdfs() {
        // Build a small zip in memory: one pdf, one txt, one oversized entry name.
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("avviso.pdf", opts).unwrap();
            w.write_all(b"%PDF-1.4 fake").unwrap();
            w.start_file("note.txt", opts).unwrap();
            w.write_all(b"hello").unwrap();
            w.finish().unwrap();
        }
        let out = unzip_pdfs(&buf).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "avviso.pdf");
        assert!(out[0].1.starts_with(b"%PDF-"));
        // Non-zip bytes fail to open as an archive (the caller checks magic first).
        assert!(unzip_pdfs(b"%PDF-1.4 fake").is_err());
    }
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
