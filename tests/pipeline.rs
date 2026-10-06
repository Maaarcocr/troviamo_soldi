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
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let response_mode = Arc::new(Mutex::new("ok".to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let (d, r, m, s) = (
            docs.clone(),
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
                    for citation in extraction["citations"].as_array_mut().unwrap() {
                        citation["source_url"] = data["source_url"].clone();
                    }
                    r.lock().unwrap().push(body);
                    let mode = m.lock().unwrap().clone();
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
                    request
                        .respond(Response::from_string(response.to_string()).with_header(
                            Header::from_bytes("Content-Type", "application/json").unwrap(),
                        ))
                        .unwrap();
                } else {
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
            max_tokens: 32768,
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
        max_file_bytes: 1024 * 1024,
        max_notice_bytes: 2 * 1024 * 1024,
    }
}
fn db(temp: &TempDir) -> Store {
    let mut db = Store::open(&temp.path().join("funding.sqlite")).unwrap();
    db.seed().unwrap();
    db
}

#[test]
fn limit_cache_native_bytes_and_amendment_context() {
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
    assert_eq!(request["provider"]["allow_fallbacks"], false);
    assert_eq!(request["response_format"]["json_schema"]["strict"], true);
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
        2,
        "Only changed annex sent"
    );
    let text = amended["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    let data: Value = serde_json::from_str(text.split("INPUT DATA:\n").nth(1).unwrap()).unwrap();
    assert!(data["prior_extraction_unverified"].is_object());
    assert_eq!(data["current_documents"].as_array().unwrap().len(), 2);
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
    let notices = vec![mock.notice("one", &["/good", "/annex"])];
    let first = pipeline::run(
        &mut store,
        &http(),
        Some(&mock.model()),
        notices.clone(),
        &options(&temp, 1),
    )
    .unwrap();
    assert_eq!(first.needs_review, 1);
    assert_eq!(
        store.latest_notices().unwrap()[0]["extraction"]["needs_review"],
        true
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
    assert_eq!(
        store.latest_notices().unwrap()[0]["extraction"]["needs_review"],
        true
    );
}
