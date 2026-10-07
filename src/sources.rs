//! Deterministic, bounded discovery. Records are source notices/articles, not proven
//! unique grants or eligibility decisions. An interrupted page scan is an error.
//! EU excludes procurement, translations, and the stale CENTRICITY index. The
//! regional category union is NOT all Regione Sicilia funding; manual feeds are
//! operator-maintained additions, not a complete Italian national catalogue.

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::blocking::{Client, Response, multipart};
use scraper::{Html, Selector};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, io::Read, thread, time::Duration};
use url::Url;

use crate::types::{DocumentRef, Notice};

const EU_API: &str =
    "https://api.tech.ec.europa.eu/search-api/prod/rest/search?apiKey=SEDIA&text=***";
// Reference labels verified 2026-10-06 against the official SEDIA FACET API
// (apiVersion 2.155, POST with the same query as the search API). The API guide
// identifies this endpoint as the authority for reference-code descriptions:
// https://ec.europa.eu/info/funding-tenders/opportunities/portal/screen/support/apis
const EU_FACET_API: &str =
    "https://api.tech.ec.europa.eu/search-api/prod/rest/facet?apiKey=SEDIA&text=***";
const WP_API: &str = "https://www.euroinfosicilia.it/wp-json/wp/v2/posts";
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const EU_HOSTS: &[&str] = &["ec.europa.eu", "commission.europa.eu", "eur-lex.europa.eu"];
const SICILY_HOSTS: &[&str] = &["euroinfosicilia.it", "regione.sicilia.it"];

/// Config is {"sources": [{"kind": "eu_sedia" | "euroinfosicilia" |
/// "manual_json", "enabled": true, ...}]}. All enabled sources must complete.
/// This prevents callers from persisting a partial scan as a successful refresh.
/// No beneficiary/keyword rejection or paid model call occurs in discovery.
pub fn discover(client: &Client, config: &Value) -> Result<Vec<Notice>> {
    let sources = config["sources"]
        .as_array()
        .context("config.sources must be an array")?;
    let mut notices = Vec::new();
    let mut ids = BTreeSet::new();
    for source in sources {
        if source.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let kind = required_str(source, "kind")?;
        let batch = match kind {
            "eu_sedia" => discover_eu(client, source),
            "euroinfosicilia" => discover_sicily(client, source),
            "manual_json" => discover_manual(client, source),
            _ => bail!("unsupported source kind: {kind}"),
        }
        .with_context(|| format!("source {kind} failed; discovery is incomplete"))?;
        for notice in batch {
            ensure!(
                ids.insert(notice.id.clone()),
                "duplicate notice id across sources: {}",
                notice.id
            );
            notices.push(notice);
        }
    }
    notices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(notices)
}

fn discover_eu(client: &Client, config: &Value) -> Result<Vec<Notice>> {
    let page_size = number(config, "page_size", 100, 1, 100)?;
    let max_pages = number(config, "max_pages", 1000, 1, 10000)?;
    // These constraints deliberately cannot be overridden into a stale index.
    let query = json!({"bool":{"must":[
        {"terms":{"type":["1","2","8"]}},
        {"terms":{"status":["31094501","31094502"]}},
        {"term":{"language":"en"}}, {"term":{"DATASOURCE":"SEDIA"}}
    ]}});
    let mut notices = Vec::new();
    let mut ids = BTreeSet::new();
    let mut expected_total = None;
    for page in 1..=max_pages {
        let response = retry_response(|| {
            let form = multipart::Form::new()
                .text("pageSize", page_size.to_string())
                .text("pageNumber", page.to_string())
                .part(
                    "query",
                    multipart::Part::text(query.to_string()).mime_str("application/json")?,
                )
                .part(
                    "sort",
                    multipart::Part::text(r#"{"field":"identifier","order":"ASC"}"#)
                        .mime_str("application/json")?,
                );
            Ok(client.post(EU_API).multipart(form).send()?)
        })
        .with_context(|| format!("SEDIA page {page}"))?;
        let body = read_json(response)?;
        let total = body["totalResults"]
            .as_u64()
            .context("SEDIA response missing totalResults")?;
        ensure!(
            expected_total.is_none() || expected_total == Some(total),
            "SEDIA catalogue changed during pagination: expected {expected_total:?}, got {total} on page {page}; retry the scan"
        );
        expected_total = Some(total);
        let results = body["results"]
            .as_array()
            .context("SEDIA response missing results array")?;
        ensure!(
            results.len() as u64 <= page_size,
            "SEDIA returned oversized page"
        );
        for result in results {
            let notice = parse_eu_notice(result)?;
            ensure!(
                ids.insert(notice.id.clone()),
                "SEDIA repeated notice {} across pages; retry the scan",
                notice.id
            );
            notices.push(notice);
        }
        if notices.len() as u64 == total {
            return Ok(notices);
        }
        ensure!(
            (notices.len() as u64) < total,
            "SEDIA returned more records than totalResults"
        );
        ensure!(
            !results.is_empty(),
            "SEDIA ended before totalResults was reached"
        );
    }
    bail!("SEDIA max_pages={max_pages} reached before all records were fetched")
}

fn parse_eu_notice(result: &Value) -> Result<Notice> {
    let metadata = result
        .get("metadata")
        .context("SEDIA record missing metadata")?;
    ensure!(
        first(metadata, "DATASOURCE") == Some("SEDIA"),
        "unexpected SEDIA index"
    );
    ensure!(
        first(metadata, "language") == Some("en"),
        "unexpected SEDIA language"
    );
    let status_code = first(metadata, "status").context("SEDIA record missing status")?;
    let (status, status_label) = eu_status_label(status_code)
        .with_context(|| format!("unexpected SEDIA status {status_code}"))?;
    let kind = first(metadata, "type").context("SEDIA record missing type")?;
    let reference = required_str(result, "reference")?;
    let id = match kind {
        "1" => format!(
            "eu:topic:{}",
            id_component(first(metadata, "identifier").context("topic missing identifier")?)
        ),
        // A cascade call inherits a parent topic identifier: never dedupe on it.
        "2" => format!("eu:external:{}", id_component(reference)),
        "8" => format!("eu:cascade:{}", id_component(reference)),
        _ => bail!("unexpected SEDIA funding type {kind}"),
    };
    let funding_type = match kind {
        "1" => "Grant",
        "2" => "Calls for proposals",
        "8" => "Cascade funding calls",
        _ => unreachable!("funding type was validated when assigning the identity"),
    };
    let title = if kind == "8" {
        // A missing child title must not turn its parent's title into a new call.
        first(metadata, "callTitle")
            .map(html_text)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| format!("Cascade funding call {reference} (title unavailable)"))
    } else {
        html_text(first(metadata, "title").context("SEDIA record missing title")?)
    };
    let source_url = allowed_url(required_str(result, "url")?, None, EU_HOSTS)?;
    let mut documents = Vec::new();
    let mut sections = vec![format!(
        "SEDIA opportunity record: {reference}\nOpportunity ID: {id}\nOpportunity title: {title}\nFunding type: {funding_type} (SEDIA type code {kind})\nSource: {source_url}\nSource status: {status} ({status_label}; SEDIA status code {status_code})\nReference-code labels: {EU_FACET_API}"
    )];
    // Do not hide a contradictory or unrecognised additional status value if
    // the source supplies more than one; all raw values retain their meaning.
    for other_code in strings(&metadata["status"])
        .into_iter()
        .filter(|code| code != status_code)
    {
        let explanation = match eu_status_label(&other_code) {
            Some((other_status, label)) => format!("{other_status} ({label})"),
            None => "unknown (unrecognised source code; no mapping inferred)".into(),
        };
        sections.push(format!(
            "Additional source status: {explanation}; SEDIA status code {other_code}"
        ));
    }
    // Keep both authoritative structured dates and narratives; discrepancies
    // require later review rather than silently discarding the older deadline.
    for field in [
        "identifier",
        "callIdentifier",
        "title",
        "callTitle",
        "caName",
        "projectName",
        "projectAcronym",
        "projectId",
        "startDate",
        "deadlineDate",
        "closingDate",
        "deadlineModel",
        "duration",
        "esDA_IngestDate",
        "frameworkProgramme",
        "programmeDivision",
        "programmePeriod",
        "geographicalZones",
        "budget",
        "currency",
        "descriptionByte",
        "description",
        "topicConditions",
        "beneficiaryAdministration",
        "furtherInformation",
        "destinationDetails",
        "latestInfos",
        "actions",
        "budgetOverview",
    ] {
        if let Some(value) = metadata.get(field) {
            let text = readable_value(value);
            if !text.trim().is_empty() {
                sections.push(format!("{}:\n{text}", eu_field_label(kind, field)));
            }
            for html in strings(value) {
                documents.extend(documents_from_html(&html, &source_url, EU_HOSTS));
            }
        }
    }
    // External-action documents are JSON serialized inside a metadata string,
    // and their official download URLs have no file extension.
    if let Some(values) = metadata.get("publicationDocuments") {
        for value in strings(values) {
            let decoded: Value =
                serde_json::from_str(&value).context("malformed SEDIA publicationDocuments")?;
            for doc in decoded
                .as_array()
                .context("publicationDocuments is not an array")?
            {
                let url = allowed_url(required_str(doc, "docUrl")?, Some(&source_url), EU_HOSTS)?;
                let extension = doc
                    .get("typeDoc")
                    .and_then(Value::as_str)
                    .unwrap_or("bin")
                    .to_ascii_lowercase();
                let name = doc
                    .get("nameDoc")
                    .and_then(Value::as_str)
                    .unwrap_or("document");
                documents.push(DocumentRef {
                    url: url.to_string(),
                    filename: format!(
                        "{}.{}",
                        filename_component(name),
                        filename_component(&extension)
                    ),
                    mime_type: mime_for_extension(&extension).map(str::to_owned),
                });
            }
        }
    }
    dedupe_documents(&mut documents);
    Ok(Notice {
        id,
        title,
        source_url: source_url.to_string(),
        source_text: Some(sections.join("\n\n")),
        documents,
    })
}

/// Canonical extraction status plus the EC's exact human-readable facet label.
/// Discovery still requests only forthcoming/open records; an explicitly closed
/// response can be represented honestly without widening that search query.
fn eu_status_label(code: &str) -> Option<(&'static str, &'static str)> {
    match code {
        "31094501" => Some(("forthcoming", "Forthcoming")),
        "31094502" => Some(("open", "Open for submission")),
        "31094503" => Some(("closed", "Closed")),
        _ => None,
    }
}

fn eu_field_label<'a>(kind: &str, field: &'a str) -> &'a str {
    match (kind, field) {
        ("8", "title") => "Parent grant topic title (metadata.title)",
        ("8", "identifier") => "Parent grant topic identifier (metadata.identifier)",
        ("8", "callTitle") => "Actual cascade opportunity title (metadata.callTitle)",
        ("8", "caName") => "Cascade opportunity name (metadata.caName)",
        ("8", "projectName") => "Parent project title (metadata.projectName)",
        ("8", "projectAcronym") => "Parent project acronym (metadata.projectAcronym)",
        ("8", "projectId") => "Parent project identifier (metadata.projectId)",
        ("1", "title") => "Actual opportunity topic title (metadata.title)",
        ("1", "identifier") => "Actual opportunity topic identifier (metadata.identifier)",
        ("1", "callTitle") => "Parent call title (metadata.callTitle)",
        ("1", "callIdentifier") => "Parent call identifier (metadata.callIdentifier)",
        (_, "frameworkProgramme") => {
            "Funding programme reference code (metadata.frameworkProgramme)"
        }
        (_, "esDA_IngestDate") => "Source index snapshot timestamp (metadata.esDA_IngestDate)",
        (_, "closingDate") => {
            "Source closingDate metadata (preserved independently of deadlineDate)"
        }
        (_, "duration") => "Source duration/application-window narrative (metadata.duration)",
        _ => field,
    }
}

fn discover_sicily(client: &Client, config: &Value) -> Result<Vec<Notice>> {
    let page_size = number(config, "page_size", 100, 1, 100)?;
    let max_pages = number(config, "max_pages", 1000, 1, 10000)?;
    let categories = config["categories"]
        .as_array()
        .context("euroinfosicilia.categories must be an array")?
        .iter()
        .map(|v| {
            v.as_u64()
                .context("category must be a positive integer")
                .and_then(|v| {
                    ensure!(v > 0, "category must be positive");
                    Ok(v.to_string())
                })
        })
        .collect::<Result<Vec<_>>>()?
        .join(",");
    ensure!(
        !categories.is_empty(),
        "regional categories must not be empty"
    );
    let fetch_details = config
        .get("fetch_detail_pages")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let delay_ms = number(config, "detail_delay_ms", 100, 0, 60000)?;
    let mut notices = Vec::new();
    let mut ids = BTreeSet::new();
    let mut expected = None;
    for page in 1..=max_pages {
        let response = retry_response(|| Ok(client.get(WP_API).query(&[
            ("categories", categories.clone()), ("per_page", page_size.to_string()),
            ("page", page.to_string()), ("orderby", "id".into()), ("order", "asc".into()),
            ("_fields", "id,date,modified,link,title,content,excerpt,categories,destinatari,programmi,meta,acf".into()),
        ]).send()?)).with_context(|| format!("EuroInfoSicilia page {page}"))?;
        let total = header_u64(&response, "X-WP-Total")?;
        let pages = header_u64(&response, "X-WP-TotalPages")?;
        ensure!(
            expected.is_none() || expected == Some((total, pages)),
            "EuroInfoSicilia catalogue changed during pagination; retry the scan"
        );
        expected = Some((total, pages));
        ensure!(
            pages <= max_pages,
            "EuroInfoSicilia needs {pages} pages, exceeding max_pages={max_pages}"
        );
        let body = read_json(response)?;
        let records = body
            .as_array()
            .context("WordPress posts response is not an array")?;
        for record in records {
            let mut notice = parse_sicily_notice(record)?;
            ensure!(
                ids.insert(notice.id.clone()),
                "WordPress repeated notice {} across pages; retry the scan",
                notice.id
            );
            if fetch_details {
                if delay_ms > 0 {
                    thread::sleep(Duration::from_millis(delay_ms));
                }
                let response = retry_response(|| Ok(client.get(&notice.source_url).send()?))
                    .with_context(|| format!("article attachments: {}", notice.source_url))?;
                // Keep the audited article, not footer/navigation downloads.
                let article_html = read_body(response)?;
                let document = Html::parse_document(&article_html);
                let article = document
                    .select(&Selector::parse("article").unwrap())
                    .next()
                    .context("regional article HTML no longer contains article element")?;
                let base = Url::parse(&notice.source_url)?;
                notice
                    .documents
                    .extend(documents_from_html(&article.html(), &base, SICILY_HOSTS));
                dedupe_documents(&mut notice.documents);
            }
            notices.push(notice);
        }
        if page >= pages {
            ensure!(
                notices.len() as u64 == total,
                "WordPress fetched {} records but X-WP-Total={total}",
                notices.len()
            );
            return Ok(notices);
        }
        ensure!(
            !records.is_empty(),
            "WordPress ended before X-WP-TotalPages"
        );
    }
    bail!("EuroInfoSicilia max_pages reached before scan finished")
}

fn parse_sicily_notice(record: &Value) -> Result<Notice> {
    let id = record["id"]
        .as_u64()
        .context("WordPress post missing numeric id")?;
    let title = record["title"]["rendered"]
        .as_str()
        .context("WordPress post missing title.rendered")?;
    let html = record["content"]["rendered"]
        .as_str()
        .context("WordPress post missing content.rendered")?;
    ensure!(
        record["content"]["protected"] != true,
        "WordPress content is protected"
    );
    let source_url = allowed_url(required_str(record, "link")?, None, SICILY_HOSTS)?;
    let mut documents = documents_from_html(html, &source_url, SICILY_HOSTS);
    dedupe_documents(&mut documents);
    let mut sections = vec![format!("Source: {source_url}\n{}", html_text(html))];
    for field in ["date", "modified", "categories", "destinatari", "programmi"] {
        if let Some(value) = record.get(field) {
            sections.push(format!("{field}: {value}"));
        }
    }
    sections.push("Category labels are discovery metadata, not proof that a call is open or that a municipality is eligible.".into());
    Ok(Notice {
        id: format!("euroinfosicilia:post:{id}"),
        title: html_text(title),
        source_url: source_url.to_string(),
        source_text: Some(sections.join("\n\n")),
        documents,
    })
}

fn discover_manual(client: &Client, config: &Value) -> Result<Vec<Notice>> {
    let name = required_str(config, "id")?;
    let host_values = config["allowed_hosts"].as_array().context(
        "manual_json.allowed_hosts is required; explicitly list verified official hosts",
    )?;
    let hosts = host_values
        .iter()
        .map(|h| h.as_str().context("allowed host must be a string"))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        !hosts.is_empty(),
        "manual feed needs at least one allowed host"
    );
    let input_count = ["notices", "path", "url"]
        .iter()
        .filter(|k| config.get(**k).is_some())
        .count();
    ensure!(
        input_count == 1,
        "manual feed needs exactly one of notices, path, or url"
    );
    let data = if let Some(notices) = config.get("notices") {
        notices.clone()
    } else if let Some(path) = config.get("path").and_then(Value::as_str) {
        serde_json::from_str(
            &fs::read_to_string(path).with_context(|| format!("manual feed {path}"))?,
        )?
    } else {
        let url = allowed_url(required_str(config, "url")?, None, &hosts)?;
        read_json(retry_response(|| Ok(client.get(url.clone()).send()?))?)?
    };
    let records = data
        .as_array()
        .or_else(|| data["notices"].as_array())
        .context("manual feed must be an array or {notices:[...]}")?;
    records
        .iter()
        .map(|record| {
            let source_url = allowed_url(required_str(record, "source_url")?, None, &hosts)?;
            let mut documents = Vec::new();
            if let Some(docs) = record.get("documents") {
                for doc in docs
                    .as_array()
                    .context("manual documents must be an array")?
                {
                    let url = allowed_url(required_str(doc, "url")?, Some(&source_url), &hosts)?;
                    let filename = doc
                        .get("filename")
                        .and_then(Value::as_str)
                        .map(filename_component)
                        .unwrap_or_else(|| filename_from_url(&url));
                    documents.push(DocumentRef {
                        url: url.to_string(),
                        filename,
                        mime_type: doc
                            .get("mime_type")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                }
            }
            dedupe_documents(&mut documents);
            Ok(Notice {
                id: format!(
                    "manual:{}:{}",
                    id_component(name),
                    id_component(required_str(record, "id")?)
                ),
                title: required_str(record, "title")?.to_owned(),
                source_url: source_url.to_string(),
                source_text: record
                    .get("source_text")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                documents,
            })
        })
        .collect()
}

fn documents_from_html(html: &str, base: &Url, hosts: &[&str]) -> Vec<DocumentRef> {
    let fragment = Html::parse_fragment(html);
    let selector = Selector::parse("a[href], a[data-downloadurl]").unwrap();
    let mut documents = Vec::new();
    for anchor in fragment.select(&selector) {
        let raw = anchor
            .value()
            .attr("data-downloadurl")
            .or_else(|| anchor.value().attr("href"))
            .unwrap_or("");
        let Ok(mut url) = allowed_url(raw, Some(base), hosts) else {
            continue;
        };
        let extension = url
            .path()
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_wp_download = url.query_pairs().any(|(key, _)| key == "wpdmdl");
        if mime_for_extension(&extension).is_none() && !is_wp_download {
            continue;
        }
        if is_wp_download {
            // refresh is a changing WordPress nonce, not document identity.
            let query: Vec<_> = url
                .query_pairs()
                .filter(|(k, _)| k != "refresh")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            url.set_query(None);
            url.query_pairs_mut().extend_pairs(query);
        }
        url.set_fragment(None);
        documents.push(DocumentRef {
            filename: filename_from_url(&url),
            url: url.to_string(),
            mime_type: mime_for_extension(&extension).map(str::to_owned),
        });
    }
    documents
}

fn dedupe_documents(documents: &mut Vec<DocumentRef>) {
    documents.sort_by(|a, b| a.url.cmp(&b.url));
    documents.dedup_by(|a, b| a.url == b.url);
}

fn allowed_url(raw: &str, base: Option<&Url>, hosts: &[&str]) -> Result<Url> {
    let url = match base {
        Some(base) => base.join(raw),
        None => Url::parse(raw),
    }?;
    ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "expected public HTTPS URL: {url}"
    );
    let host = url.host_str().context("URL missing host")?;
    ensure!(
        hosts
            .iter()
            .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}"))),
        "unapproved source/document host: {host}"
    );
    ensure!(
        url.port().is_none() || url.port() == Some(443),
        "unexpected HTTPS port"
    );
    Ok(url)
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("missing nonempty {field}"))
}
fn first<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(|v| v.as_str().or_else(|| v.as_array()?.first()?.as_str()))
        .filter(|s| !s.is_empty())
}
fn number(config: &Value, field: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let n = match config.get(field) {
        Some(value) => value
            .as_u64()
            .with_context(|| format!("{field} must be an unsigned integer"))?,
        None => default,
    };
    ensure!((min..=max).contains(&n), "{field} must be in {min}..={max}");
    Ok(n)
}
fn header_u64(response: &Response, name: &str) -> Result<u64> {
    response
        .headers()
        .get(name)
        .with_context(|| format!("missing {name} pagination header"))?
        .to_str()?
        .parse()
        .with_context(|| format!("invalid {name}"))
}
/// Bounded retries for transient failures of read-only discovery requests.
/// Certificate/permission errors and unverified redirects remain hard failures.
fn retry_response(mut send: impl FnMut() -> Result<Response>) -> Result<Response> {
    for attempt in 0..3 {
        let mut delay = 2_u64.pow(attempt + 1);
        let error = match send() {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }
                ensure!(
                    !status.is_redirection(),
                    "unexpected source redirect to {:?}",
                    response.headers().get(reqwest::header::LOCATION)
                );
                if !(status.is_server_error() || status.as_u16() == 429) {
                    bail!("source returned HTTP {status}");
                }
                if let Some(retry_after) = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                {
                    // Preserve HTTP-date Retry-After rather than retrying early.
                    let seconds = retry_after.parse::<u64>().with_context(|| {
                        format!("source requested Retry-After {retry_after}; retry the scan later")
                    })?;
                    ensure!(
                        seconds <= 60,
                        "source requested retry after {seconds} seconds; retry the scan later"
                    );
                    delay = delay.max(seconds);
                }
                response.error_for_status().unwrap_err().into()
            }
            Err(error) => {
                if !error
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|e| e.is_timeout())
                {
                    return Err(error);
                }
                error
            }
        };
        if attempt == 2 {
            return Err(error);
        }
        thread::sleep(Duration::from_secs(delay));
    }
    unreachable!("bounded retry loop always returns")
}

fn read_body(response: Response) -> Result<String> {
    let mut body = String::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_string(&mut body)?;
    ensure!(
        body.len() as u64 <= MAX_RESPONSE_BYTES,
        "source response exceeds 64 MiB"
    );
    Ok(body)
}
fn read_json(response: Response) -> Result<Value> {
    Ok(serde_json::from_str(&read_body(response)?)?)
}
fn html_text(html: &str) -> String {
    Html::parse_fragment(html)
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}
fn readable_value(value: &Value) -> String {
    match value {
        Value::String(s) => html_text(s),
        Value::Array(a) => a.iter().map(readable_value).collect::<Vec<_>>().join("\n"),
        _ => value.to_string(),
    }
}
fn id_component(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn filename_component(value: &str) -> String {
    let name: String = value
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(180)
        .collect();
    if name.is_empty() || name == "." || name == ".." {
        "document".into()
    } else {
        name
    }
}
fn filename_from_url(url: &Url) -> String {
    filename_component(
        url.path_segments()
            .and_then(|mut parts| parts.rfind(|s| !s.is_empty()))
            .unwrap_or("document"),
    )
}
fn mime_for_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "pdf" => Some("application/pdf"),
        "doc" => Some("application/msword"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xls" => Some("application/vnd.ms-excel"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "odt" => Some("application/vnd.oasis.opendocument.text"),
        "ods" => Some("application/vnd.oasis.opendocument.spreadsheet"),
        "rtf" => Some("application/rtf"),
        "zip" => Some("application/zip"),
        "p7m" => Some("application/pkcs7-mime"),
        "csv" => Some("text/csv"),
        "txt" => Some("text/plain"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eu_record(kind: &str, reference: &str) -> Value {
        json!({"reference":reference,"url":"https://ec.europa.eu/info/topic", "metadata":{
            "DATASOURCE":["SEDIA"],"language":["en"],"status":["31094502"],"type":[kind],
            "identifier":["SHARED-PARENT"],"title":["Parent title"],"callTitle":["Actual cascade title"],
            "topicConditions":["<p>Comuni &amp; partners</p><a href='/docs/call.pdf'>Call</a>"]}})
    }

    #[test]
    fn cascade_uses_own_identity_and_title() {
        let a = parse_eu_notice(&eu_record("8", "123COMPETITIVE_CALLen")).unwrap();
        let b = parse_eu_notice(&eu_record("8", "456COMPETITIVE_CALLen")).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(a.title, "Actual cascade title");
        assert_eq!(a.documents[0].url, "https://ec.europa.eu/docs/call.pdf");
        let text = a.source_text.unwrap();
        assert!(text.contains("Comuni & partners"));
        assert!(text.contains("Opportunity ID: eu:cascade:123COMPETITIVE_CALLen"));
        assert!(text.contains("Opportunity title: Actual cascade title"));
        assert!(text.contains(
            "Actual cascade opportunity title (metadata.callTitle):\nActual cascade title"
        ));
        assert!(text.contains("Parent grant topic title (metadata.title):\nParent title"));
        assert!(
            text.contains("Parent grant topic identifier (metadata.identifier):\nSHARED-PARENT")
        );
        assert!(text.contains("Funding type: Cascade funding calls (SEDIA type code 8)"));
    }

    #[test]
    fn official_sedia_statuses_are_decoded_without_guessing() {
        for (code, status, label) in [
            ("31094501", "forthcoming", "Forthcoming"),
            ("31094502", "open", "Open for submission"),
            ("31094503", "closed", "Closed"),
        ] {
            let mut record = eu_record("8", "123COMPETITIVE_CALLen");
            record["metadata"]["status"] = json!([code]);
            record["metadata"]["esDA_IngestDate"] = json!(["2026-09-28T10:00:00.000+0000"]);
            let text = parse_eu_notice(&record).unwrap().source_text.unwrap();
            assert!(text.contains(&format!(
                "Source status: {status} ({label}; SEDIA status code {code})"
            )));
            assert!(!text.contains(&format!("status:\n{code}")));
            assert!(text.contains(EU_FACET_API));
            assert!(!text.contains("require review"));
            assert!(!text.contains("must not"));
            assert!(!text.contains("Cascade identity:"));
            assert!(text.contains("Source index snapshot timestamp"));
            assert!(text.contains("2026-09-28T10:00:00.000+0000"));
        }
        let mut record = eu_record("1", "1TOPICen");
        record["metadata"]["status"] = json!(["unknown-code"]);
        assert!(parse_eu_notice(&record).is_err());
    }

    #[test]
    fn all_supported_funding_types_have_human_labels() {
        for (kind, label, prefix) in [
            ("1", "Grant", "eu:topic:SHARED-PARENT"),
            ("2", "Calls for proposals", "eu:external:1RECORDen"),
            ("8", "Cascade funding calls", "eu:cascade:1RECORDen"),
        ] {
            let notice = parse_eu_notice(&eu_record(kind, "1RECORDen")).unwrap();
            assert_eq!(notice.id, prefix);
            assert!(
                notice
                    .source_text
                    .unwrap()
                    .contains(&format!("Funding type: {label} (SEDIA type code {kind})"))
            );
        }
    }

    #[test]
    fn source_dates_narratives_and_conflicting_statuses_are_preserved() {
        let mut record = eu_record("8", "123COMPETITIVE_CALLen");
        record["metadata"]["status"] = json!(["31094502", "31094503"]);
        record["metadata"]["deadlineDate"] = json!(["2026-06-28T22:59:00.000+0000"]);
        record["metadata"]["closingDate"] = json!(["2026-05-24T00:00:00Z"]);
        record["metadata"]["duration"] =
            json!(["Applications close on 28 May 2026 at 23:59 CEST."]);
        record["metadata"]["description"] = json!(["<p>Original terms remain evidence.</p>"]);
        record["metadata"]["latestInfos"] =
            json!(["<p>Possible extension; consult the amendment.</p>"]);
        record["metadata"]["projectName"] = json!(["Parent project name"]);
        record["metadata"]["projectId"] = json!(["101000123"]);
        let notice = parse_eu_notice(&record).unwrap();
        let text = notice.source_text.unwrap();
        for evidence in [
            "2026-06-28T22:59:00.000+0000",
            "2026-05-24T00:00:00Z",
            "Applications close on 28 May 2026 at 23:59 CEST.",
            "Original terms remain evidence.",
            "Possible extension; consult the amendment.",
            "Source status: open (Open for submission; SEDIA status code 31094502)",
            "Additional source status: closed (Closed); SEDIA status code 31094503",
            "Parent project title (metadata.projectName):\nParent project name",
            "Parent project identifier (metadata.projectId):\n101000123",
        ] {
            assert!(text.contains(evidence), "missing evidence: {evidence}");
        }
    }

    #[test]
    fn absent_cascade_title_does_not_relabel_parent_as_opportunity() {
        for title in [json!([]), json!([""]), json!(["<p> </p>"])] {
            let mut record = eu_record("8", "123COMPETITIVE_CALLen");
            record["metadata"]["callTitle"] = title;
            let notice = parse_eu_notice(&record).unwrap();
            assert_eq!(
                notice.title,
                "Cascade funding call 123COMPETITIVE_CALLen (title unavailable)"
            );
            assert!(
                notice
                    .source_text
                    .unwrap()
                    .contains("Parent grant topic title (metadata.title):\nParent title")
            );
        }
    }

    #[test]
    fn topic_title_and_parent_call_are_distinct() {
        let notice = parse_eu_notice(&eu_record("1", "1TOPICen")).unwrap();
        assert_eq!(notice.title, "Parent title");
        let text = notice.source_text.unwrap();
        assert!(text.contains("Actual opportunity topic title (metadata.title):\nParent title"));
        assert!(text.contains("Parent call title (metadata.callTitle):\nActual cascade title"));
    }

    #[test]
    fn identical_source_metadata_has_stable_text_for_caching() {
        let record = eu_record("8", "123COMPETITIVE_CALLen");
        let a = parse_eu_notice(&record).unwrap();
        let b = parse_eu_notice(&record).unwrap();
        assert_eq!(a.source_text, b.source_text);
    }

    #[test]
    fn stale_index_is_rejected() {
        let mut record = eu_record("1", "1TOPICen");
        record["metadata"]["DATASOURCE"] = json!(["SEDIA_PRD_CENTRICITY"]);
        assert!(parse_eu_notice(&record).is_err());
    }

    #[test]
    fn parses_external_action_document_shape() {
        let mut record = eu_record("2", "186411PROSPECTSEN");
        record["metadata"]["publicationDocuments"] = json!([
            r#"[{"nameDoc":"Guidelines","typeDoc":"rtf","docUrl":"https://webgate.ec.europa.eu/prospect/internal/noauth/externalDocumentDownload.htm?id=2201116&lang=EN"}]"#
        ]);
        let notice = parse_eu_notice(&record).unwrap();
        assert!(
            notice
                .documents
                .iter()
                .any(|d| d.filename == "Guidelines.rtf"
                    && d.mime_type.as_deref() == Some("application/rtf"))
        );
    }

    #[test]
    fn wp_notices_retain_stale_status_and_decode_title() {
        let record = json!({"id":157082,"link":"https://www.euroinfosicilia.it/call/", "title":{"rendered":"Comuni &#8211; avviso"}, "content":{"rendered":"<p>Scadenza precedente; verificare la proroga.</p>","protected":false},"categories":[341,372]});
        let notice = parse_sicily_notice(&record).unwrap();
        assert_eq!(notice.id, "euroinfosicilia:post:157082");
        assert_eq!(notice.title, "Comuni – avviso");
        assert!(notice.source_text.unwrap().contains("341,372"));
    }

    #[test]
    fn wp_download_nonces_do_not_change_identity() {
        let html = r##"<a href="/download/avviso/?wpdmdl=173570&amp;refresh=abc">Avviso</a><a data-downloadurl="/download/avviso/?wpdmdl=173570&amp;refresh=def" href="#">Download</a><a href="https://untrusted.example/grant.pdf">Skip</a>"##;
        let base = Url::parse("https://www.euroinfosicilia.it/post/").unwrap();
        let mut docs = documents_from_html(html, &base, SICILY_HOSTS);
        dedupe_documents(&mut docs);
        assert_eq!(docs.len(), 1);
        assert_eq!(
            docs[0].url,
            "https://www.euroinfosicilia.it/download/avviso/?wpdmdl=173570"
        );
    }

    #[test]
    fn host_suffix_check_has_label_boundary() {
        assert!(allowed_url("https://ec.europa.eu.evil.example/file.pdf", None, EU_HOSTS).is_err());
        assert!(
            allowed_url(
                "https://bad-euroinfosicilia.it/file.pdf",
                None,
                SICILY_HOSTS
            )
            .is_err()
        );
        assert!(allowed_url("file:///tmp/private", None, EU_HOSTS).is_err());
        assert!(allowed_url("https://webgate.ec.europa.eu/file.pdf", None, EU_HOSTS).is_ok());
    }

    #[test]
    fn linked_native_image_annexes_are_preserved() {
        let base = Url::parse("https://www.euroinfosicilia.it/post/").unwrap();
        let docs = documents_from_html(
            r#"<a href="/annex/one.PNG">One</a><a href="/annex/two.jpg">Two</a><a href="/annex/three.jpeg">Three</a><a href="/annex/four.gif">Four</a><a href="/annex/five.webp">Five</a>"#,
            &base,
            SICILY_HOSTS,
        );
        assert_eq!(docs.len(), 5);
        assert_eq!(docs[0].mime_type.as_deref(), Some("image/png"));
        assert_eq!(docs[1].mime_type.as_deref(), Some("image/jpeg"));
        assert_eq!(docs[2].mime_type.as_deref(), Some("image/jpeg"));
        assert_eq!(docs[3].mime_type.as_deref(), Some("image/gif"));
        assert_eq!(docs[4].mime_type.as_deref(), Some("image/webp"));
    }

    #[test]
    fn manual_inline_feed_has_source_namespace() {
        let config = json!({"sources":[{"kind":"manual_json","id":"italy", "allowed_hosts":["sport.governo.it"],"notices":[{"id":"events-2026", "title":"Eventi sportivi", "source_url":"https://www.sport.governo.it/avviso", "documents":[{"url":"/media/avviso.pdf"}]}]}]});
        let notices = discover(&Client::new(), &config).unwrap();
        assert_eq!(notices[0].id, "manual:italy:events-2026");
        assert_eq!(
            notices[0].documents[0].url,
            "https://www.sport.governo.it/media/avviso.pdf"
        );
    }
}

#[cfg(test)]
mod audited_snapshot_test {
    use super::*;

    /// Optional regression against the external audit corpus, never downloaded
    /// or required by the normal test suite. Set FUNDING_AUDIT_DIR to its root.
    #[test]
    #[ignore = "requires the separately retained source-audit fixtures"]
    fn all_audited_records_parse() -> Result<()> {
        let root = std::env::var("FUNDING_AUDIT_DIR").context("set FUNDING_AUDIT_DIR")?;
        let eu: Value =
            serde_json::from_str(&fs::read_to_string(format!("{root}/eu/catalogue.json"))?)?;
        let mut eu_ids = BTreeSet::new();
        let mut doc_count = 0;
        for record in eu.as_array().context("audit EU array")? {
            if first(&record["metadata"], "DATASOURCE") != Some("SEDIA") {
                continue;
            }
            let notice = parse_eu_notice(record)?;
            ensure!(
                eu_ids.insert(notice.id),
                "audit contains duplicate EU business identity"
            );
            doc_count += notice.documents.len();
        }
        ensure!(!eu_ids.is_empty(), "audit has no canonical EU records");
        let wp: Value = serde_json::from_str(&fs::read_to_string(format!(
            "{root}/region/euro_all_bandi_posts.json"
        ))?)?;
        let mut wp_ids = BTreeSet::new();
        for record in wp.as_array().context("audit WP array")? {
            let notice = parse_sicily_notice(record)?;
            ensure!(
                wp_ids.insert(notice.id),
                "audit contains duplicate WP identity"
            );
        }
        ensure!(!wp_ids.is_empty(), "audit has no regional records");
        eprintln!(
            "Parsed {} canonical EU records ({} document references) and {} regional posts",
            eu_ids.len(),
            doc_count,
            wp_ids.len()
        );
        Ok(())
    }
}
