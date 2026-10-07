use funding_rust::{
    model::{MODEL, ModelClient},
    pipeline::{self, RunOptions},
    store::Store,
    types::{DocumentRef, Notice},
};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tempfile::TempDir;
use tiny_http::{Header, Response, Server};

struct Mock {
    base: String,
    docs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    fetches: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    response_mode: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Mock {
    fn new() -> Self {
        let server = Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr());
        let docs = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let fetches = Arc::new(Mutex::new(Vec::<String>::new()));
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let response_mode = Arc::new(Mutex::new("ok".to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let (d, f, r, m, s) = (
            docs.clone(),
            fetches.clone(),
            requests.clone(),
            response_mode.clone(),
            stop.clone(),
        );
        let handle = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let Some(mut request) = server.recv_timeout(Duration::from_millis(50)).unwrap()
                else {
                    continue;
                };
                if request.url() == "/chat" {
                    let mut raw = String::new();
                    request.as_reader().read_to_string(&mut raw).unwrap();
                    let body: Value = serde_json::from_str(&raw).unwrap();
                    let prompt = body["messages"][0]["content"][0]["text"].as_str().unwrap();
                    let data: Value =
                        serde_json::from_str(prompt.split("INPUT DATA:\n").nth(1).unwrap())
                            .unwrap();
                    let mut extraction: Value =
                        serde_json::from_str(include_str!("fixtures/extraction.json")).unwrap();
                    extraction.as_object_mut().unwrap().remove("needs_review");
                    extraction.as_object_mut().unwrap().remove("review_reasons");
                    extraction["schema_version"] = json!(3);
                    for rule in extraction["requirements"].as_array_mut().unwrap() {
                        for key in [
                            "scope",
                            "blocker",
                            "values",
                            "min",
                            "max",
                            "left",
                            "right",
                            "relation",
                            "days",
                            "note",
                            "population_basis",
                            "reference_date",
                        ] {
                            rule.as_object_mut().unwrap().remove(key);
                        }
                    }
                    for citation in extraction["citations"].as_array_mut().unwrap() {
                        citation["source_url"] = data["source_url"].clone();
                    }
                    r.lock().unwrap().push(body);
                    let mode = m.lock().unwrap().clone();
                    if mode == "estonian_startup" {
                        extraction.as_object_mut().unwrap().remove("needs_review");
                        extraction.as_object_mut().unwrap().remove("review_reasons");
                        extraction["schema_version"] = json!(3);
                        extraction["closes_at"] = Value::Null;
                        extraction["requirements"] = json!([
                            {"id":"country","label":"Estonian applicant","op":"equals","field":"entity.country","value":"EE","citation_ids":["c1"]},
                            {"id":"kind","label":"Startup applicant","op":"equals","field":"entity.kind","value":"startup","citation_ids":["c1"]},
                            {"id":"project","label":"Unrelated technical requirement","op":"manual","note":"Technical certificate required","citation_ids":["c1"]}
                        ]);
                    }
                    if mode == "individual" {
                        extraction["closes_at"] = Value::Null;
                        extraction["requirements"] = json!([
                            {"id":"kind","label":"Individual applicant","op":"equals","field":"entity.kind","value":"individual","citation_ids":["c1"]},
                            {"id":"course","label":"Degree course","op":"manual","note":"Check the specified degree courses","citation_ids":["c1"]}
                        ]);
                        extraction["citations"][0]["quote"] =
                            json!("Students in the specified degree courses may apply.");
                    }
                    if mode == "large_response" {
                        extraction["summary"] = json!("x".repeat(8 * 1024 * 1024 + 1));
                    }
                    let mut response = json!({"id":"gen-mock","model":MODEL,"provider":"OpenAI","choices":[{"finish_reason":"stop","message":{"content":extraction.to_string()}}],"usage":{"prompt_tokens":1000,"completion_tokens":200,"completion_tokens_details":{"reasoning_tokens":150},"cost":0.002}});
                    if mode == "unknown_cost" {
                        response["usage"].as_object_mut().unwrap().remove("cost");
                    }
                    if mode == "truncated" {
                        response["choices"][0]["finish_reason"] = json!("length");
                        response["choices"][0]["message"]["content"] = json!("");
                    }
                    if mode == "invalid" {
                        response["choices"][0]["message"]["content"] = json!("{bad");
                    }
                    if mode == "parsed" {
                        response["choices"][0]["message"]["annotations"] = json!([{"type":"file"}]);
                    }
                    if mode == "array_error"
                        || mode == "string_error"
                        || mode == "malformed_provider"
                    {
                        let raw = match mode.as_str() {
                            "array_error" => "[]",
                            "string_error" => "\"rate limited\"",
                            _ => "{invalid",
                        };
                        request
                            .respond(Response::from_string(raw).with_status_code(429))
                            .unwrap();
                        continue;
                    }
                    request
                        .respond(Response::from_string(response.to_string()).with_header(
                            Header::from_bytes("Content-Type", "application/json").unwrap(),
                        ))
                        .unwrap();
                } else {
                    f.lock().unwrap().push(request.url().to_owned());
                    let data = d.lock().unwrap().get(request.url()).cloned();
                    let response = match data {
                        Some(bytes) => Response::from_data(bytes),
                        None => Response::from_data(b"missing".to_vec()).with_status_code(404),
                    };
                    request.respond(response).unwrap();
                }
            }
        });
        Self {
            base,
            docs,
            fetches,
            requests,
            response_mode,
            stop,
            thread: Some(handle),
        }
    }
    fn notice(&self, id: &str, files: &[&str]) -> Notice {
        Notice {
            id: id.into(),
            title: format!("Bando {id}"),
            source_url: format!("{}/notice/{id}", self.base),
            source_text: Some("Official source body".into()),
            documents: files
                .iter()
                .map(|file| DocumentRef {
                    url: format!("{}{file}", self.base),
                    filename: format!("{}.pdf", file.trim_start_matches('/')),
                    mime_type: Some("application/pdf".into()),
                })
                .collect(),
        }
    }
    fn put(&self, path: &str, bytes: &[u8]) {
        self.docs
            .lock()
            .unwrap()
            .insert(path.into(), bytes.to_vec());
    }
    fn model(&self) -> ModelClient {
        ModelClient {
            http: http(),
            endpoint: format!("{}/chat", self.base),
            api_key: None,
        }
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn http() -> Client {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}
fn options(temp: &TempDir, limit: usize) -> RunOptions {
    RunOptions {
        limit,
        cache_dir: temp.path().join("files"),
        allowed_hosts: vec![],
        allow_localhost: true,
        retry_failed: false,
        dry_run: false,
    }
}
fn db(temp: &TempDir) -> Store {
    let mut db = Store::open(&temp.path().join("funding.sqlite")).unwrap();
    db.seed().unwrap();
    db
}

#[test]
fn limit_cache_native_bytes_and_complete_changed_version() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    mock.put("/original", b"%PDF-1.4 ORIGINAL EXACT BYTES\x00");
    mock.put("/annex", b"%PDF-1.4 ANNEX v1");
    mock.put("/second", b"%PDF-1.4 SECOND");
    let notices = vec![
        mock.notice("one", &["/original", "/annex"]),
        mock.notice("two", &["/second"]),
    ];
    let run = |store: &mut Store| {
        pipeline::run(
            store,
            &http(),
            Some(&mock.model()),
            notices.clone(),
            &options(&temp, 1),
        )
        .unwrap()
    };
    let first = run(&mut store);
    assert_eq!((first.processed, first.calls, first.failed), (1, 1, 0));
    assert_eq!(first.reported_cost_usd, 0.002);
    let request = &mock.requests.lock().unwrap()[0].clone();
    assert_eq!(request["plugins"][0]["pdf"]["engine"], "native");
    assert_eq!(request["reasoning"]["effort"], "max");
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        assert!(request.get(key).is_none(), "unexpected {key}");
        assert!(request["reasoning"].get(key).is_none());
    }
    assert_eq!(request["provider"]["allow_fallbacks"], false);
    assert_eq!(request["response_format"]["json_schema"]["strict"], true);
    let instruction = request["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .split("INPUT DATA:")
        .next()
        .unwrap();
    assert!(instruction.split_whitespace().count() < 140);
    assert!(!instruction.contains("review_reasons"));
    assert!(!instruction.contains("previous"));
    assert!(
        request["response_format"]["json_schema"]["schema"]["properties"]
            .get("review_reasons")
            .is_none()
    );
    let metadata: Value = serde_json::from_str(
        request["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .split("INPUT DATA:\n")
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        request["response_format"]["json_schema"]["schema"]["properties"]["citations"]["items"]["properties"]
            ["source_url"]["enum"],
        json!([
            notices[0].source_url,
            notices[0].documents[0].url,
            notices[0].documents[1].url
        ])
    );
    assert!(chrono::DateTime::parse_from_rfc3339(metadata["as_of"].as_str().unwrap()).is_ok());
    assert!(metadata.get("allowed_citation_urls").is_none());
    assert!(
        metadata["fields"]
            .as_object()
            .unwrap()
            .values()
            .all(|definition| definition.get("scope").is_none()
                && definition.get("readOnly").is_none())
    );

    let data = request["messages"][0]["content"][1]["file"]["file_data"]
        .as_str()
        .unwrap();
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data.split(',').nth(1).unwrap())
            .unwrap(),
        b"%PDF-1.4 ORIGINAL EXACT BYTES\x00"
    );
    let second = run(&mut store);
    assert_eq!(
        (second.unchanged, second.processed, second.calls),
        (1, 1, 1)
    );
    let third = run(&mut store);
    assert_eq!((third.unchanged, third.calls), (2, 0));
    mock.put("/annex", b"%PDF-1.4 ANNEX v2 AMENDMENT");
    let fourth = run(&mut store);
    assert_eq!(fourth.calls, 1);
    let requests = mock.requests.lock().unwrap();
    let amended = requests.last().unwrap();
    assert_eq!(
        amended["messages"][0]["content"].as_array().unwrap().len(),
        3,
        "Every current original is sent, including the unchanged main document"
    );
    let text = amended["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    let data: Value = serde_json::from_str(text.split("INPUT DATA:\n").nth(1).unwrap()).unwrap();
    assert!(data.get("prior_extraction_unverified").is_none());
    assert!(data.get("changed_documents").is_none());
    assert_eq!(data["documents"].as_array().unwrap().len(), 2);
    for (part, expected) in amended["messages"][0]["content"].as_array().unwrap()[1..]
        .iter()
        .zip([
            b"%PDF-1.4 ORIGINAL EXACT BYTES\x00".as_slice(),
            b"%PDF-1.4 ANNEX v2 AMENDMENT".as_slice(),
        ])
    {
        let data = part["file"]["file_data"].as_str().unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data.split(',').nth(1).unwrap())
                .unwrap(),
            expected
        );
    }
    assert_eq!(store.stats().unwrap()["versions"], 3);
    drop(requests);
    // Reappearance of byte-identical prior version uses that exact cached extraction.
    mock.put("/annex", b"%PDF-1.4 ANNEX v1");
    let reverted = run(&mut store);
    assert_eq!(reverted.calls, 0);
    assert_eq!(store.latest_notices().unwrap()[0]["version"], 1);
}
#[test]
fn failures_consume_limit_and_never_reveal_old_result() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    mock.put("/bad", b"<html>not a PDF</html>");
    mock.put("/good", b"%PDF-1.4 valid");
    let notices = vec![
        mock.notice("bad", &["/bad"]),
        mock.notice("good", &["/good"]),
        mock.notice("later", &["/good"]),
    ];
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        notices,
        &options(&temp, 2),
    )
    .unwrap();
    assert_eq!(
        (summary.processed, summary.failed, summary.calls),
        (2, 1, 1)
    );
    assert_eq!(store.latest_notices().unwrap().len(), 2);
    mock.put("/good", b"%PDF-1.4 revised");
    *mock.response_mode.lock().unwrap() = "truncated".into();
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![mock.notice("good", &["/good"])],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!((summary.needs_review, summary.calls), (1, 1));
    assert_eq!(summary.reported_cost_usd, 0.002);
    let latest = store.latest_notices().unwrap();
    assert!(latest.iter().find(|n| n["id"] == "good").unwrap()["extraction"].is_null());
    let again = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![mock.notice("good", &["/good"])],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(again.calls, 0);
}
#[test]
fn unknown_cost_and_invalid_output_are_persisted() {
    for mode in ["unknown_cost", "invalid", "parsed"] {
        let mock = Mock::new();
        let temp = TempDir::new().unwrap();
        let mut store = db(&temp);
        *mock.response_mode.lock().unwrap() = mode.into();
        mock.put("/pdf", b"%PDF-1.4 test");
        let summary = pipeline::run(
            &mut store,
            &http(),
            Some(&mock.model()),
            vec![mock.notice("one", &["/pdf"])],
            &options(&temp, 1),
        )
        .unwrap();
        if mode == "unknown_cost" {
            assert_eq!(summary.calls_with_unknown_cost, 1);
            assert_eq!(summary.average_cost_per_called_notice_usd, None);
        } else {
            assert_eq!(summary.needs_review, 1);
            assert_eq!(summary.reported_cost_usd, 0.002);
        }
        let usage: String = store
            .conn
            .query_row("SELECT usage FROM attempts", [], |r| r.get(0))
            .unwrap();
        assert!(usage.contains("prompt_tokens"));
    }
}
#[test]
fn no_pdf_uses_real_source_text_without_fake_attachment() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![mock.notice("text-only", &[])],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(summary.calls, 1);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[0]["messages"][0]["content"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        requests[0]["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Official source body")
    );
}

#[test]
fn bare_url_input_is_not_fetched_and_never_calls_the_model() {
    for source_text in [None, Some(" \n\t".to_owned())] {
        let mock = Mock::new();
        let temp = TempDir::new().unwrap();
        let mut store = db(&temp);
        let mut notice = mock.notice("bare", &[]);
        notice.source_text = source_text;
        mock.put(
            "/notice/bare",
            b"<html><body>Portal app shell</body></html>",
        );
        let summary = pipeline::run(
            &mut store,
            &http(),
            Some(&mock.model()),
            vec![notice],
            &options(&temp, 1),
        )
        .unwrap();
        assert_eq!(
            (summary.processed, summary.failed, summary.calls),
            (1, 1, 0)
        );
        assert_eq!(summary.reported_cost_usd, 0.0);
        assert!(mock.requests.lock().unwrap().is_empty());
        assert!(mock.fetches.lock().unwrap().is_empty());
        let saved = store.latest_notices().unwrap();
        assert!(
            saved[0]["error"]
                .as_str()
                .unwrap()
                .contains("provide source_text or original documents")
        );
        assert!(saved[0]["extraction"].is_null());
        assert_eq!(store.stats().unwrap()["attempts"], 0);
    }
}

#[test]
fn unsupported_originals_without_text_never_call_the_model() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    mock.put("/zip", b"PK\x03\x04unsupported zip");
    let mut notice = mock.notice("unsupported", &["/zip"]);
    notice.source_text = None;
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(
        (summary.processed, summary.failed, summary.calls),
        (1, 1, 0)
    );
    assert_eq!(summary.reported_cost_usd, 0.0);
    assert!(mock.requests.lock().unwrap().is_empty());
    let saved = store.latest_notices().unwrap();
    assert!(
        saved[0]["error"]
            .as_str()
            .unwrap()
            .contains("all original documents are unsupported")
    );
    assert_eq!(store.stats().unwrap()["attempts"], 0);
}

#[test]
fn supplied_web_document_is_not_classified_by_keywords() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    let mut notice = mock.notice("html", &[]);
    let source = "<html><body><p>Enable JavaScript for the application form. Municipalities may apply before 31 December.</p><footer>Cookies</footer><script>initialize();</script></body></html>";
    notice.source_text = Some(source.into());
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!((summary.extracted, summary.calls), (1, 1));
    assert!(mock.fetches.lock().unwrap().is_empty());
    let requests = mock.requests.lock().unwrap();
    let prompt = requests[0]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    let metadata: Value =
        serde_json::from_str(prompt.split("INPUT DATA:\n").nth(1).unwrap()).unwrap();
    assert_eq!(metadata["source_content"], source);
}

#[test]
fn source_text_change_resends_unchanged_originals_without_prior_output() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    mock.put("/original", b"%PDF-1.4 unchanged original");
    let mut notice = mock.notice("one", &["/original"]);
    pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice.clone()],
        &options(&temp, 1),
    )
    .unwrap();
    notice.source_text = Some("Updated source dates and conditions".into());
    let changed = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice.clone()],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!((changed.calls, changed.extracted), (1, 1));
    let unchanged = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!((unchanged.unchanged, unchanged.calls), (1, 0));
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]["messages"][0]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let prompt = requests[1]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    let metadata: Value =
        serde_json::from_str(prompt.split("INPUT DATA:\n").nth(1).unwrap()).unwrap();
    assert!(metadata.get("prior_extraction_unverified").is_none());
    assert_eq!(
        metadata["source_content"],
        "Updated source dates and conditions"
    );
}

#[test]
fn cached_legacy_screened_version_is_not_reprocessed() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    let notice = mock.notice("legacy", &[]);
    pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice.clone()],
        &options(&temp, 1),
    )
    .unwrap();
    store
        .conn
        .execute("UPDATE versions SET state='screened'", [])
        .unwrap();
    let mut opts = options(&temp, 1);
    opts.retry_failed = true;
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice],
        &opts,
    )
    .unwrap();
    assert_eq!(
        (summary.unchanged, summary.processed, summary.calls),
        (1, 0, 0)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
    assert_eq!(store.latest_notices().unwrap()[0]["state"], "screened");
}

#[test]
fn imports_remain_municipality_project_scoped_and_untrusted() {
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    let payload = json!({"municipalityId":"088001","projects":[{"id":"school","name":"Scuola"}],"evidence":[{"id":"design","municipalityId":"088001","projectId":"school","callId":null,"field":"project.verifiedDesign","value":true,"observedAt":"2026-01-01","validUntil":null,"provenance":"official","source":{"title":"User document"}}]});
    store.import_facts(&payload).unwrap();
    let facts = store.evidence("088001").unwrap();
    assert!(
        facts
            .iter()
            .any(|e| e["id"] == "user:design" && e["provenance"] == "user_declared")
    );
    assert!(
        !store
            .evidence("087001")
            .unwrap()
            .iter()
            .any(|e| e["id"] == "user:design")
    );
    let mut wrong = payload.clone();
    wrong["evidence"][0]["municipalityId"] = json!("087001");
    assert!(store.import_facts(&wrong).is_err());
    let mut wrong = payload;
    wrong["evidence"][0]["projectId"] = json!("missing");
    assert!(store.import_facts(&wrong).is_err());
}

#[test]
fn cached_download_failure_does_not_starve_later_notices() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    mock.put("/good", b"%PDF-1.4 valid");
    let notices = vec![
        mock.notice("bad", &["/missing"]),
        mock.notice("good", &["/good"]),
    ];
    let first = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        notices.clone(),
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!((first.failed, first.calls), (1, 0));
    let second = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        notices,
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(
        (second.unchanged, second.processed, second.calls),
        (1, 1, 1)
    );
}
#[test]
fn review_retry_resends_originals_and_unsupported_files_force_review() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    mock.put("/good", b"%PDF-1.4 valid");
    mock.put("/annex", b"PK\x03\x04unsupported zip");
    let mut notice = mock.notice("one", &["/good", "/annex"]);
    notice.source_text = None;
    let notices = vec![notice];
    let first = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        notices.clone(),
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(first.needs_review, 1);
    assert!(
        !store.latest_notices().unwrap()[0]["extraction"]["review_reasons"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut opts = options(&temp, 1);
    opts.retry_failed = true;
    let second = pipeline::run(&mut store, &http(), Some(&mock.model()), notices, &opts).unwrap();
    assert_eq!(second.calls, 1);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[0]["messages"][0]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        requests[1]["messages"][0]["content"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn recurring_download_failure_never_keeps_recovered_screening_current() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    let notices = vec![mock.notice("one", &["/pdf"])];
    let run = |store: &mut Store| {
        pipeline::run(
            store,
            &http(),
            Some(&mock.model()),
            notices.clone(),
            &options(&temp, 1),
        )
        .unwrap()
    };
    assert_eq!(run(&mut store).failed, 1);
    mock.put("/pdf", b"%PDF-1.4 recovered");
    assert_eq!(run(&mut store).calls, 1);
    mock.docs.lock().unwrap().remove("/pdf");
    assert_eq!(run(&mut store).calls, 0);
    let latest = store.latest_notices().unwrap();
    assert_eq!(latest[0]["state"], "failed");
    assert!(latest[0]["extraction"].is_null());
}

#[test]
fn removed_attachment_forces_review_and_original_cache_self_repairs() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    let original = b"%PDF-1.4 original";
    mock.put("/pdf", original);
    mock.put("/annex", b"%PDF-1.4 annex");
    pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![mock.notice("one", &["/pdf", "/annex"])],
        &options(&temp, 1),
    )
    .unwrap();
    let cache = temp.path().join("files").join(pipeline::sha256(original));
    std::fs::write(&cache, b"corrupted cache").unwrap();
    let changed = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![mock.notice("one", &["/pdf"])],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(changed.needs_review, 1);
    assert_eq!(std::fs::read(cache).unwrap(), original);
    assert!(
        !store.latest_notices().unwrap()[0]["extraction"]["review_reasons"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn valid_incompatible_call_is_extracted_before_local_screening() {
    for mode in ["estonian_startup", "individual"] {
        let mock = Mock::new();
        *mock.response_mode.lock().unwrap() = mode.into();
        let temp = TempDir::new().unwrap();
        let mut store = db(&temp);
        let summary = pipeline::run(
            &mut store,
            &http(),
            Some(&mock.model()),
            vec![mock.notice("startup", &[])],
            &options(&temp, 1),
        )
        .unwrap();
        assert_eq!(
            (
                summary.extracted,
                summary.needs_review,
                summary.failed,
                summary.calls
            ),
            (1, 0, 0, 1)
        );
        let notices = store.latest_notices().unwrap();
        assert_eq!(notices[0]["state"], "extracted");
        assert!(notices[0]["error"].is_null());
        let attempt_state: String = store
            .conn
            .query_row("SELECT state FROM attempts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(attempt_state, "extracted");
        let serialized_summary = serde_json::to_value(&summary).unwrap();
        assert_eq!(serialized_summary["extracted"], 1);
        assert!(serialized_summary.get("accepted").is_none());
        let municipality =
            json!({"istatCode":"081001","region":"Sicilia","registryReferenceDate":"2026-10-06"});
        let result = funding_rust::engine::evaluate(
            &notices[0]["extraction"],
            &municipality,
            &[],
            None,
            "2026-10-06T12:00:00Z",
        );
        assert_eq!(result["state"], "excluded");
        assert_eq!(result["window"]["state"], "unknown");
        assert_eq!(
            result["results"].as_array().unwrap().last().unwrap()["state"],
            "review_required"
        );
        let raw: String = store
            .conn
            .query_row("SELECT response FROM attempts", [], |r| r.get(0))
            .unwrap();
        let raw: Value = serde_json::from_str(&raw).unwrap();
        let content: Value =
            serde_json::from_str(raw["choices"][0]["message"]["content"].as_str().unwrap())
                .unwrap();
        assert_eq!(content["schema_version"], 3);
        assert!(content.get("review_reasons").is_none());
        let mut expected_stored = content.clone();
        expected_stored["schema_version"] = json!(2);
        expected_stored["review_reasons"] = json!([]);
        assert_eq!(expected_stored, notices[0]["extraction"]);
    }
}

#[test]
fn malformed_prior_extraction_is_not_read_or_reused_for_a_changed_version() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    mock.put("/original", b"%PDF-1.4 unchanged");
    mock.put("/annex", b"%PDF-1.4 old annex");
    let notice = mock.notice("one", &["/original", "/annex"]);
    pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice.clone()],
        &options(&temp, 1),
    )
    .unwrap();
    let original = "{malformed prior extraction";
    store
        .conn
        .execute("UPDATE versions SET extraction=?1 WHERE id=1", [original])
        .unwrap();
    mock.put("/annex", b"%PDF-1.4 changed annex");
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(summary.calls, 1);
    let requests = mock.requests.lock().unwrap();
    let amended = requests.last().unwrap();
    assert_eq!(
        amended["messages"][0]["content"].as_array().unwrap().len(),
        3
    );
    let metadata: Value = serde_json::from_str(
        amended["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .split("INPUT DATA:\n")
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    assert!(metadata.get("prior_extraction_unverified").is_none());
    let preserved: String = store
        .conn
        .query_row("SELECT extraction FROM versions WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(preserved, original);
}

#[test]
fn malformed_or_nonobject_provider_error_never_panics_and_retains_raw_bytes() {
    for (mode, raw) in [
        ("array_error", "[]"),
        ("string_error", "\"rate limited\""),
        ("malformed_provider", "{invalid"),
    ] {
        let mock = Mock::new();
        let temp = TempDir::new().unwrap();
        let mut store = db(&temp);
        *mock.response_mode.lock().unwrap() = mode.into();
        let summary = pipeline::run(
            &mut store,
            &http(),
            Some(&mock.model()),
            vec![mock.notice("bad", &[])],
            &options(&temp, 1),
        )
        .unwrap();
        assert_eq!(
            (
                summary.calls,
                summary.needs_review,
                summary.calls_with_unknown_cost
            ),
            (1, 1, 1)
        );
        let saved: String = store
            .conn
            .query_row("SELECT response FROM attempts", [], |row| row.get(0))
            .unwrap();
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["_raw_response"], raw);
        assert_eq!(saved["_http_status"], 429);
    }
}

#[test]
fn complete_large_input_and_response_are_not_cut_off_by_application_caps() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    let files: Vec<_> = (0..21).map(|i| format!("/file{i}")).collect();
    for file in &files {
        mock.put(file, b"%PDF-1.4 Original");
    }
    let files: Vec<_> = files.iter().map(String::as_str).collect();
    let mut notice = mock.notice(&"n".repeat(301), &files);
    notice.source_text = Some("s".repeat(2 * 1024 * 1024 + 1));
    *mock.response_mode.lock().unwrap() = "large_response".into();
    let summary = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        vec![notice.clone()],
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(
        (summary.calls, summary.extracted, summary.failed),
        (1, 1, 0)
    );
    let requests = mock.requests.lock().unwrap();
    let parts = requests[0]["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 22);
    let input: Value = serde_json::from_str(
        parts[0]["text"]
            .as_str()
            .unwrap()
            .split("INPUT DATA:\n")
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(input["source_content"], notice.source_text.unwrap());
    let saved = store.latest_notices().unwrap();
    assert_eq!(
        saved[0]["extraction"]["summary"].as_str().unwrap().len(),
        8 * 1024 * 1024 + 1
    );
}

#[test]
fn limit_three_counts_provider_failures_and_cache_never_adds_calls() {
    let mock = Mock::new();
    let temp = TempDir::new().unwrap();
    let mut store = db(&temp);
    *mock.response_mode.lock().unwrap() = "truncated".into();
    let notices: Vec<_> = (0..4).map(|i| mock.notice(&format!("n{i}"), &[])).collect();
    let first = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        notices.clone(),
        &options(&temp, 3),
    )
    .unwrap();
    assert_eq!(
        (first.processed, first.calls, first.needs_review),
        (3, 3, 3)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
    let cached = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        notices[..3].to_vec(),
        &options(&temp, 3),
    )
    .unwrap();
    assert_eq!(
        (cached.unchanged, cached.processed, cached.calls),
        (3, 0, 0)
    );
    assert_eq!(mock.requests.lock().unwrap().len(), 3);
}
