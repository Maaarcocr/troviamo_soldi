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
    model: &ModelClient,
    notices: Vec<Notice>,
    options: &RunOptions,
) -> Result<RunSummary> {
    ensure!(options.limit > 0, "--limit must be greater than zero");
    std::fs::create_dir_all(&options.cache_dir)?;
    let mut summary = RunSummary {
        discovered: notices.len(),
        ..Default::default()
    };
    let mut work_items: Vec<WorkItem> = Vec::new();
    // Phase 1a: download + prepare documents in parallel (I/O bound, no DB access).
    // The portal downloads are slow; doing them serially was the bottleneck.
    let mut unique: Vec<&Notice> = Vec::new();
    {
        let mut seen_ids = HashSet::new();
        for notice in &notices {
            if seen_ids.insert(notice.id.clone()) {
                unique.push(notice);
            }
        }
    }
    let io_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .thread_name(|i| format!("dl-{i}"))
        .build()
        .context("Failed to build download thread pool")?;
    // Prepare no more candidates than the remaining attempt budget. Cached
    // notices do not consume slots, so keep advancing until useful work fills it.
    let mut candidates = unique.into_iter();
    while summary.processed < options.limit {
        let batch: Vec<_> = candidates
            .by_ref()
            .take((options.limit - summary.processed).min(8))
            .collect();
        if batch.is_empty() {
            break;
        }
        let downloaded: Vec<_> = io_pool.install(|| {
            batch.par_iter().map(|notice| (*notice, prepare(client, notice, options))).collect()
        });
        for (notice, prepared) in downloaded {
        ensure!(!notice.id.is_empty(), "Invalid notice ID");
        let previous = store.previous(&notice.id)?;
        let documents = match prepared {
            Ok(docs) => docs,
            Err(error) => {
                let fingerprint = sha256(
                    format!("download-failure:{}", serde_json::to_string(&notice)?).as_bytes(),
                );
                if !options.retry_failed
                    && let Some((version, _)) = store.cached(&notice.id, &fingerprint)?
                {
                    store.select_cached_version(notice, version)?;
                    summary.unchanged += 1;
                    continue;
                }
                summary.processed += 1;
                summary.failed += 1;
                let version = store.begin_version(notice, &fingerprint, &[])?;
                store.finish_version(version, "failed", None, Some(&format!("{error:#}")))?;
                eprintln!(
                    "{}: needs review (download/input error: {error:#})",
                    notice.id
                );
                continue;
            }
        };
        let fingerprint = fingerprint(notice, &documents)?;
        let cached = store.cached(&notice.id, &fingerprint)?;
        if let Some((version, state)) = &cached {
            let retryable =
                ["failed", "in_flight", "pending", "needs_review"].contains(&state.as_str());
            if !options.retry_failed || !retryable {
                store.select_cached_version(notice, *version)?;
                summary.unchanged += 1;
                continue;
            }
        }
        // An attempted notice consumes exactly one slot, even on failure or review.
        summary.processed += 1;
        let version = store.begin_version(notice, &fingerprint, &documents)?;
        let body = match model::request_body(notice, &documents, &options.cache_dir) {
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

    }

    // Workers only make requests. The caller consumes completed responses and
    // writes SQLite immediately, even while another request is still waiting.
    std::thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = std::sync::mpsc::channel();
        scope.spawn(|| {
            io_pool.install(|| {
                work_items.into_par_iter().for_each_with(sender, |sender, item| {
                    let response = model.send(&item.body);
                    let _ = sender.send((item, response));
                });
            });
        });
        for (work_item, response) in receiver {
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
                                            !document_still_present(d, &documents)
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
                                .filter(|d| !model::supported_mime(&d.mime_type) && d.mime_type != "application/zip")
                                .map(|d| d.filename.clone())
                                .collect();
                            if !unsupported.is_empty() {
                                extraction["review_reasons"].as_array_mut().unwrap().push(json!(format!("Original files not readable by this native PDF/image path: {}. Requirements may be incomplete.",unsupported.join(", "))));
                            }

                            for document in &documents {
                                if let Some(warning) = &document.coverage_warning {
                                    extraction["review_reasons"].as_array_mut().unwrap().push(json!(warning));
                                }
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
        Ok(())
    })?;
    summary.finish();
    store.save_run(&serde_json::to_value(&summary)?)?;
    Ok(summary)
}
fn document_still_present(previous: &DocumentVersion, current: &[DocumentVersion]) -> bool {
    current.iter().any(|document| {
        if document.url != previous.url {
            return false;
        }
        if previous.archive_member.is_some() {
            return document.archive_member == previous.archive_member;
        }
        // Historical expanded members lacked a member identity. Do not let the
        // newly retained archive root mask a removed child on the first upgrade.
        // Exact filenames retain known flat members; ambiguous old paths review.
        if current.iter().any(|root| root.url == previous.url && root.mime_type == "application/zip") {
            document.filename == previous.filename
        } else {
            document.archive_member.is_none()
        }
    })
}

fn fingerprint(notice: &Notice, docs: &[DocumentVersion]) -> Result<String> {
    let mut ordered: Vec<_> = docs
        .iter()
        .map(|d| {
            let mut identity = json!({"url":d.url,"sha256":d.sha256,"mime_type":d.mime_type});
            if let Some(member) = &d.archive_member {
                identity["archive_member"] = json!(member);
            }
            identity
        })
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
        // Keep the original archive as identity even when every member is read.
        // This also detects changes to entries excluded by decompression guards.
        if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
            let expanded = unzip_originals(&bytes);
            let (files, warnings) = match expanded {
                Ok(contents) => (contents.files, contents.warnings),
                Err(error) => (Vec::new(), vec![format!("Archive could not be read: {error:#}")]),
            };
            let mut original = cache_document(
                &document.url, document.filename.clone(), &bytes, None, options,
            )?;
            original.mime_type = "application/zip".into();
            if !warnings.is_empty() {
                original.coverage_warning = Some(format!(
                    "Archive {} was not completely read: {}. Requirements may be incomplete.",
                    document.filename, warnings.join("; ")
                ));
            }
            documents.push(original);
            for (member, data) in files {
                documents.push(cache_document(
                    &document.url,
                    format!("{}/{}", document.filename, member),
                    &data,
                    Some(member),
                    options,
                )?);
            }
        } else {
            documents.push(cache_document(
                &document.url, document.filename.clone(), &bytes, None, options,
            )?);
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
fn cache_document(
    url: &str,
    filename: String,
    data: &[u8],
    archive_member: Option<String>,
    options: &RunOptions,
) -> Result<DocumentVersion> {
    let mime = sniff_type(data).unwrap_or("application/octet-stream");
    let hash = sha256(data);
    let path = options.cache_dir.join(&hash);
    if !path.exists() || sha256(&std::fs::read(&path)?) != hash {
        std::fs::write(&path, data)?;
    }
    Ok(DocumentVersion {
        url: url.into(),
        filename: if filename.trim().is_empty() { "document".into() } else { filename },
        mime_type: mime.into(),
        sha256: hash,
        bytes: data.len() as u64,
        archive_member,
        coverage_warning: None,
    })
}

struct ArchiveContents {
    files: Vec<(String, Vec<u8>)>,
    warnings: Vec<String>,
}

/// Expand original files without converting them. Resource guards are explicit
/// coverage gaps, never silent omissions. Full member paths preserve identity.
fn unzip_originals(bytes: &[u8]) -> Result<ArchiveContents> {
    const MAX_ENTRY_BYTES: u64 = 10 * 1024 * 1024;
    const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("Unreadable zip archive")?;
    let mut contents = ArchiveContents { files: Vec::new(), warnings: Vec::new() };
    let mut total = 0u64;
    for i in 0..archive.len() {
        let entry = match archive.by_index(i) {
            Ok(entry) => entry,
            Err(error) => {
                contents.warnings.push(format!("Entry {i} unreadable: {error}"));
                continue;
            }
        };
        if entry.is_dir() {
            continue;
        }
        let name = match entry.name() {
            Ok(name) => name.into_owned(),
            Err(error) => {
                contents.warnings.push(format!("Entry {i} has an unreadable name: {error}"));
                continue;
            }
        };
        let remaining = MAX_TOTAL_BYTES.saturating_sub(total);
        let allowed = MAX_ENTRY_BYTES.min(remaining);
        if entry.size() > allowed {
            contents.warnings.push(format!("{name}: exceeds archive decompression limit"));
            continue;
        }
        let mut buf = Vec::new();
        if let Err(error) = entry.take(allowed + 1).read_to_end(&mut buf) {
            contents.warnings.push(format!("{name}: could not decompress ({error})"));
            continue;
        }
        if buf.len() as u64 > allowed {
            contents.warnings.push(format!("{name}: exceeds archive decompression limit"));
            continue;
        }
        total += buf.len() as u64;
        contents.files.push((name, buf));
    }
    if contents.files.is_empty() && contents.warnings.is_empty() {
        contents.warnings.push("Archive contains no files".into());
    }
    Ok(contents)
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
    fn zip_expansion_retains_original_members() {
        // Readable and unsupported originals are both retained for coverage.
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
        let out = unzip_originals(&buf).unwrap().files;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "avviso.pdf");
        assert!(out[0].1.starts_with(b"%PDF-"));
        // Non-zip bytes fail to open as an archive (the caller checks magic first).
        assert!(unzip_originals(b"%PDF-1.4 fake").is_err());
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
            archive_member: None,
            coverage_warning: None,
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
    fn historical_archive_children_do_not_match_the_new_archive_root() {
        let legacy = |filename: &str| -> DocumentVersion {
            serde_json::from_value(json!({"url":"https://example.org/archive.zip",
                "filename":filename,"mime_type":"application/pdf","sha256":"hash","bytes":10})).unwrap()
        };
        let old_a = legacy("archive.zip/a.pdf");
        let old_b = legacy("archive.zip/b.pdf");
        assert!(old_a.archive_member.is_none());
        assert!(old_a.coverage_warning.is_none());
        let mut root = legacy("archive.zip");
        root.mime_type = "application/zip".into();
        let mut new_a = old_a.clone();
        new_a.archive_member = Some("a.pdf".into());
        let current = [root, new_a];
        assert!(document_still_present(&old_a, &current));
        assert!(!document_still_present(&old_b, &current));
    }

    #[test]
    fn no_html_masquerading_as_pdf() {
        assert!(sniff_type(b"<html>wrong</html>").is_none());
        assert_eq!(sniff_type(b"%PDF-1.4 raw bytes"), Some("application/pdf"));
    }
}
