//! Offline diagnostics only: this module has no database or HTTP client.
use anyhow::{Context, Result, ensure};
use funding_rust::{
    engine, model,
    store::{MAX_DIAGNOSTIC_ATTEMPTS, MAX_DIAGNOSTIC_BYTES},
    types::DocumentVersion,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

pub fn from_file(path: &Path) -> Result<Value> {
    let file = File::open(path).with_context(|| format!("Read replay file {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "Replay input must be a regular file"
    );
    ensure!(
        file.metadata()?.len() <= MAX_DIAGNOSTIC_BYTES as u64,
        "Replay input exceeds 64 MiB"
    );
    let mut bytes = Vec::new();
    file.take(MAX_DIAGNOSTIC_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_DIAGNOSTIC_BYTES,
        "Replay input exceeds 64 MiB"
    );
    let raw = std::str::from_utf8(&bytes).context("Replay input is not UTF-8 JSON")?;
    let input = model::parse_json(raw).context("Replay input is not valid unambiguous JSON")?;
    replay(&input)
}

pub fn replay(input: &Value) -> Result<Value> {
    let rows = input
        .as_array()
        .context("Replay input must be an exported JSON array")?;
    ensure!(
        rows.len() <= MAX_DIAGNOSTIC_ATTEMPTS,
        "Replay input exceeds {MAX_DIAGNOSTIC_ATTEMPTS} attempts"
    );
    // Check shape before producing any output. Content failures remain individual
    // diagnoses; invalid container records are operator input errors.
    for (index, row) in rows.iter().enumerate() {
        ensure!(
            row.is_object(),
            "Replay row {} must be an object",
            index + 1
        );
        ensure!(
            row["attempt_id"].as_i64().is_some_and(|id| id > 0),
            "Replay row {} needs a positive integer attempt_id",
            index + 1
        );
        ensure!(
            row["notice_id"]
                .as_str()
                .is_some_and(|id| !id.trim().is_empty() && id.len() <= 300),
            "Replay row {} needs a nonempty notice_id (maximum 300 bytes)",
            index + 1
        );
        ensure!(
            row.get("response").is_some(),
            "Replay row {} needs response (null allowed)",
            index + 1
        );
    }
    let results: Vec<Value> = rows.iter().map(diagnose).collect();
    let mut counts = BTreeMap::<String, usize>::new();
    for result in &results {
        *counts
            .entry(result["category"].as_str().unwrap().into())
            .or_default() += 1;
    }
    Ok(json!({
        "report_schema": "funding-replay-v1",
        "offline": true,
        "model_calls": 0,
        "database_writes": 0,
        "count": results.len(),
        "counts": counts,
        "results": results,
        "note": "Validation is against supplied export metadata only. Quotations and legal interpretation are not independently verified. No municipality eligibility is inferred, and no recovered extraction or facts are imported. Historical reported usage is not new replay spending."
    }))
}

fn failure(row: &Value, category: &str, stage: &str, message: String) -> Value {
    json!({
        "attempt_id": row["attempt_id"],
        "notice_id": row["notice_id"],
        "category": category,
        "stage": stage,
        "reason": message,
        "stored_validation_error": row["validation_error"],
        "stored_version_error": row["version"]["error"],
        "saved_usage": row["usage"],
        "contract_validation": "not_run",
        "citation_allowlist_validated": false,
        "quotations_verified": false,
        "legal_clearance": false
    })
}

fn diagnose(row: &Value) -> Value {
    let raw = &row["response"];
    if raw.is_null() || raw.as_str().is_some_and(|s| s.trim().is_empty()) {
        return failure(row, "no_saved_response", "stored_response", "No provider response was saved. Offline replay cannot recover missing bytes or resolve uncertain prior billing; inspect the stored error.".into());
    }
    let response = if let Some(raw) = raw.as_str() {
        match model::parse_json(raw) {
            Ok(value) => value,
            Err(error) => {
                return failure(
                    row,
                    "json_error",
                    "provider_json",
                    format!("Saved provider response is not valid unambiguous JSON: {error:#}"),
                );
            }
        }
    } else {
        raw.clone()
    };
    if let Some(raw_provider) = response["_raw_response"].as_str()
        && let Err(error) = model::parse_json(raw_provider)
    {
        return failure(
            row,
            "json_error",
            "provider_json",
            format!("Preserved raw provider response is not valid unambiguous JSON: {error:#}"),
        );
    }
    if let Some(reason) = provider_problem(&response) {
        return failure(row, "provider_error", "provider_envelope", reason);
    }
    let content = response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap();
    let extraction_content = match model::parse_json(content) {
        Ok(extraction) => extraction,
        Err(error) => {
            return failure(
                row,
                "json_error",
                "extraction_json",
                format!("Model content is not valid unambiguous JSON: {error:#}"),
            );
        }
    };
    let sources = match input_sources(row) {
        Ok(sources) => sources,
        Err(error) => {
            let mut result = failure(
                row,
                "diagnosis_only",
                "source_metadata",
                format!("Provider envelope and content JSON are readable. {error:#}"),
            );
            result["contract_validation"] = json!("not_run_missing_source_metadata");
            match engine::validate_extraction_structure(&extraction_content) {
                Ok(()) => {
                    result["structural_validation"] = json!("passed");
                    result["structural_error"] = Value::Null;
                }
                Err(error) => {
                    result["structural_validation"] = json!("failed");
                    result["structural_error"] = json!(format!("{error:#}"));
                }
            }
            result["reported_content"] = reported_content(&extraction_content);
            result["saved_usage"] = row
                .get("usage")
                .cloned()
                .unwrap_or_else(|| response["usage"].clone());
            return result;
        }
    };
    let extraction = match model::parse_saved_response(&response, &sources) {
        Ok(extraction) => extraction,
        Err(error) => {
            let mut result = failure(
                row,
                "contract_error",
                "extraction_contract",
                format!("{error:#}"),
            );
            result["contract_validation"] = json!("failed");
            return result;
        }
    };
    let reasons = engine::extraction_review_reasons(&extraction);
    let manual_clauses: Vec<Value> = extraction["requirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rule| rule["op"] == "manual")
        .map(|rule| json!({"id":rule["id"],"label":rule["label"],"reason":rule["note"]}))
        .collect();
    let mut unknowns = Vec::new();
    if extraction["status"] == "unknown" {
        unknowns.push("Application status is unknown; confirm against the original call.");
    }
    if extraction["status"] == "open" && extraction["closes_at"].is_null() {
        unknowns.push(
            "Application closing instant is unknown; an open-ended funding window is not assumed.",
        );
    }
    let unsupported_documents: Vec<Value> = row["version"]["documents"].as_array().into_iter().flatten()
        .filter(|document| !model::supported_mime(document["mime_type"].as_str().unwrap_or("")))
        .map(|document| json!({"filename":document["filename"],"mime_type":document["mime_type"],"reason":"Original attachment is unsupported by the native PDF/image path; requirements may be incomplete."})).collect();
    // Stored successful extraction can include deterministic pipeline review flags
    // (e.g. removed attachments) absent from the untouched provider response.
    let saved_review_reasons = row["version"]["saved_extraction"]["review_reasons"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let review = !reasons.is_empty()
        || !manual_clauses.is_empty()
        || !unknowns.is_empty()
        || !unsupported_documents.is_empty()
        || !saved_review_reasons.is_empty()
        || extraction["needs_review"] == true;
    json!({
        "attempt_id": row["attempt_id"],
        "notice_id": row["notice_id"],
        "category": if review { "valid_extraction_review" } else { "valid_extraction" },
        "stage": "extraction_contract",
        "reason": if review { "Extraction satisfies the current contract, with explicit manual or unresolved review items." } else { "Extraction satisfies the current contract; this is not an applicant eligibility decision." },
        "contract_validation": "passed",
        "citation_allowlist_validated": true,
        "citation_validation_basis": "exported_notice_and_version_document_urls",
        "quotations_verified": false,
        "review_reasons": reasons,
        "manual_clauses": manual_clauses,
        "unknowns": unknowns,
        "unsupported_documents": unsupported_documents,
        "saved_pipeline_review_reasons": saved_review_reasons,
        "stored_validation_error": row["validation_error"],
        "saved_usage": row.get("usage").unwrap_or(&response["usage"]),
        "extraction": extraction,
        "legal_clearance": false
    })
}

/// Source-less SQL exports can still explain what the model reported. These
/// fields never become verified source facts or an executable eligibility result.
fn reported_content(extraction: &Value) -> Value {
    let mut operators = BTreeMap::<String, usize>::new();
    let clauses = extraction["requirements"].as_array();
    let mut manual = Vec::new();
    for clause in clauses.into_iter().flatten() {
        *operators
            .entry(clause["op"].as_str().unwrap_or("invalid_or_missing").into())
            .or_default() += 1;
        if clause["op"] == "manual" {
            manual.push(json!({"id":clause["id"],"label":clause["label"],"reason":clause["note"]}));
        }
    }
    json!({
        "basis": "untrusted_model_output_not_source_verified",
        "title": extraction["title"],
        "schema_version": extraction["schema_version"],
        "status": extraction["status"],
        "closes_at": extraction["closes_at"],
        "requirement_count": clauses.map(Vec::len),
        "operators": operators,
        "review_reasons": engine::extraction_review_reasons(extraction),
        "manual_clauses": manual,
        "note": "Reported clauses and unknowns are diagnostic only, including when structural_validation fails. Nothing is repaired, imported, or treated as verified eligibility."
    })
}

/// Keep the same envelope restrictions as model::parse_saved_response. Splitting them
/// here prevents provider/truncation failures from being called contract failures.
fn provider_problem(response: &Value) -> Option<String> {
    if !response.is_object() {
        return Some("Saved provider response must be an object.".into());
    }
    if response.get("error").is_some() || response.get("_http_status").is_some() {
        return Some(format!(
            "Provider returned an error (HTTP status: {}). Inspect the saved response; no retry was made.",
            response
                .get("_http_status")
                .map(Value::to_string)
                .unwrap_or_else(|| "not recorded".into())
        ));
    }
    if response["model"].as_str() != Some(model::MODEL) {
        return Some("Unexpected or missing response model.".into());
    }
    let choice = &response["choices"][0];
    if choice["finish_reason"] != "stop" {
        return Some(format!(
            "Incomplete model output: finish_reason must be stop (saved: {}).",
            choice["finish_reason"]
        ));
    }
    let message = &choice["message"];
    if !message["refusal"].is_null() {
        return Some("Model refused the request.".into());
    }
    if message["annotations"]
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x["type"] == "file"))
    {
        return Some(
            "Unexpected parsed-file annotation; native-only processing requires review.".into(),
        );
    }
    if !message["content"].is_string() {
        return Some("Model content is absent or not a string.".into());
    }
    None
}

fn input_sources(row: &Value) -> Result<Vec<String>> {
    ensure!(
        row["export_schema"] == "funding-attempt-v1",
        "Trusted input source metadata is missing. SQL-only rows are diagnosis-only; citation URLs from model output are never treated as input sources."
    );
    ensure!(
        row["version"]["is_latest"] == true,
        "This attempt is for an older version. The exported notice URL is current metadata, not a historical snapshot, so complete historical source validation was not run."
    );
    let source = row["notice"]["source_url"]
        .as_str()
        .context("Export is missing its recorded notice source URL")?;
    let documents: Vec<DocumentVersion> =
        serde_json::from_value(row["version"]["documents"].clone())
            .context("Export has missing or invalid version document metadata")?;
    ensure!(
        documents.len() <= 20,
        "Export has more than 20 documents for one version"
    );
    let mut sources = vec![source.to_owned()];
    sources.extend(documents.into_iter().map(|document| document.url));
    for source in &sources {
        let url = url::Url::parse(source).context("Invalid recorded source URL")?;
        ensure!(
            ["http", "https"].contains(&url.scheme())
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none(),
            "Recorded source must be HTTP(S) without credentials"
        );
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extraction() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/extraction.json")).unwrap()
    }
    fn row(extraction: &Value) -> Value {
        json!({
            "export_schema":"funding-attempt-v1", "attempt_id":1,"notice_id":"notice-1",
            "notice":{"source_url":"https://example.gov/call.pdf"},
            "version":{"is_latest":true,"documents":[]},
            "response":{"model":model::MODEL,"choices":[{"finish_reason":"stop","message":{"content":extraction.to_string()}}]},
            "validation_error":"old contract failure"
        })
    }
    fn result(row: Value) -> Value {
        replay(&json!([row])).unwrap()["results"][0].clone()
    }

    #[test]
    fn replays_saved_legacy_output_without_inventing_eligibility() {
        let actual = result(row(&extraction()));
        assert_eq!(actual["category"], "valid_extraction");
        assert_eq!(actual["contract_validation"], "passed");
        assert_eq!(actual["citation_allowlist_validated"], true);
        assert_eq!(actual["quotations_verified"], false);
        assert_eq!(actual["stored_validation_error"], "old contract failure");
        assert!(actual.get("eligibility").is_none());
    }

    #[test]
    fn separates_provider_json_contract_and_absent_response_failures() {
        let valid = row(&extraction());
        let mut failed = valid.clone();
        failed["response"]["_http_status"] = json!(429);
        assert_eq!(result(failed)["category"], "provider_error");
        let mut truncated = valid.clone();
        truncated["response"]["choices"][0]["finish_reason"] = json!("length");
        assert!(
            result(truncated)["reason"]
                .as_str()
                .unwrap()
                .contains("length")
        );
        let mut json = valid.clone();
        json["response"]["choices"][0]["message"]["content"] = json!("{broken");
        assert_eq!(result(json)["category"], "json_error");
        let mut raw = valid.clone();
        raw["response"] = json!("non-json provider bytes");
        assert_eq!(result(raw)["stage"], "provider_json");
        let malformed = result(row(&json!({"title":"not a complete extraction"})));
        assert_eq!(malformed["category"], "contract_error");
        assert_eq!(malformed["contract_validation"], "failed");
        let mut absent = valid;
        absent["response"] = Value::Null;
        assert_eq!(result(absent)["category"], "no_saved_response");
    }

    #[test]
    fn sql_rows_are_diagnosis_only_and_do_not_trust_model_citations() {
        let mut exported = row(&extraction());
        let response = exported["response"].to_string();
        let sql = json!({"attempt_id":4,"notice_id":"notice-1","response":response,"validation_error":"Unsupported rule type"});
        let actual = result(sql);
        assert_eq!(actual["category"], "diagnosis_only");
        assert_eq!(actual["structural_validation"], "passed");
        assert_eq!(actual["reported_content"]["requirement_count"], 1);
        assert_eq!(actual["citation_allowlist_validated"], false);
        assert_eq!(
            actual["contract_validation"],
            "not_run_missing_source_metadata"
        );
        exported["notice"]["source_url"] = json!("https://other.gov/other.pdf");
        assert_eq!(result(exported)["category"], "contract_error");
    }

    #[test]
    fn sql_diagnosis_shows_genuine_review_and_structural_failure_separately() {
        let mut manual = extraction();
        manual["needs_review"] = json!(true);
        manual["status"] = json!("unknown");
        manual["closes_at"] = Value::Null;
        manual["review_reasons"] = json!(["Unclear source identity and multiple deadlines"]);
        manual["requirements"][0]["op"] = json!("manual");
        manual["requirements"][0]["field"] = Value::Null;
        manual["requirements"][0]["value"] = Value::Null;
        manual["requirements"][0]["note"] = json!("Verify whether the applicant is a student");
        let make_sql = |content: &Value| json!({"attempt_id":1,"notice_id":"notice-1","response":row(content)["response"],"validation_error":null});
        let valid = result(make_sql(&manual));
        assert_eq!(valid["category"], "diagnosis_only");
        assert_eq!(valid["structural_validation"], "passed");
        assert_eq!(valid["citation_allowlist_validated"], false);
        assert_eq!(
            valid["reported_content"]["manual_clauses"][0]["reason"],
            "Verify whether the applicant is a student"
        );
        assert!(
            valid["reported_content"]["review_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().contains("Opening status is unknown"))
        );

        manual["requirements"][0]["field"] = json!("entity.kind");
        let invalid = result(make_sql(&manual));
        assert_eq!(invalid["category"], "diagnosis_only");
        assert_eq!(invalid["structural_validation"], "failed");
        assert!(
            invalid["structural_error"]
                .as_str()
                .unwrap()
                .contains("field must be null")
        );
        assert_eq!(
            invalid["reported_content"]["manual_clauses"][0]["reason"],
            "Verify whether the applicant is a student"
        );
        assert_eq!(invalid["citation_allowlist_validated"], false);
    }

    #[test]
    fn historical_or_bad_input_sources_do_not_claim_validation() {
        let mut historical = row(&extraction());
        historical["version"]["is_latest"] = json!(false);
        assert_eq!(result(historical)["category"], "diagnosis_only");
        let mut wrong_docs = row(&extraction());
        wrong_docs["version"]["documents"] = json!("corrupt database json");
        assert_eq!(result(wrong_docs)["citation_allowlist_validated"], false);
        let mut credentials = row(&extraction());
        credentials["notice"]["source_url"] = json!("https://user:password@example.gov/call.pdf");
        assert_eq!(result(credentials)["citation_allowlist_validated"], false);
    }

    #[test]
    fn valid_manual_and_unknown_output_has_explicit_review_reasons() {
        let mut manual = extraction();
        manual["needs_review"] = json!(true);
        manual["review_reasons"] = json!(["Complex legal condition needs manual verification"]);
        manual["requirements"][0]["op"] = json!("manual");
        manual["requirements"][0]["field"] = Value::Null;
        manual["requirements"][0]["value"] = Value::Null;
        manual["requirements"][0]["note"] =
            json!("Check the consortium exception with the responsible office");
        manual["status"] = json!("unknown");
        manual["closes_at"] = Value::Null;
        let actual = result(row(&manual));
        assert_eq!(actual["category"], "valid_extraction_review", "{actual}");
        assert_eq!(
            actual["manual_clauses"][0]["reason"],
            "Check the consortium exception with the responsible office"
        );
        assert!(!actual["unknowns"].as_array().unwrap().is_empty());
    }

    #[test]
    fn compact_v2_remains_valid_with_manual_and_unknown_population_details() {
        let mut compact = extraction();
        compact.as_object_mut().unwrap().remove("needs_review");
        compact["schema_version"] = json!(2);
        compact["requirements"] = json!([
            {"id":"population","label":"Population threshold","op":"range","field":"population","min":null,"max":5000,"population_basis":null,"reference_date":null,"citation_ids":["c1"]},
            {"id":"legal-exception","label":"Consortium exception","op":"manual","note":"Check the applicable consortium exemption","citation_ids":["c1"]}
        ]);
        let actual = result(row(&compact));
        assert_eq!(actual["category"], "valid_extraction_review", "{actual}");
        assert_eq!(actual["contract_validation"], "passed");
        assert_eq!(actual["extraction"]["schema_version"], 2);
        assert_eq!(
            actual["extraction"]["requirements"][0]["population_basis"],
            Value::Null
        );
        assert_eq!(
            actual["manual_clauses"][0]["reason"],
            "Check the applicable consortium exemption"
        );
        assert!(
            actual["review_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason
                    .as_str()
                    .unwrap()
                    .contains("population basis/reference date"))
        );
    }

    #[test]
    fn replay_accepts_factual_v3_without_a_global_verdict() {
        let mut facts = extraction();
        facts.as_object_mut().unwrap().remove("needs_review");
        facts.as_object_mut().unwrap().remove("review_reasons");
        facts["schema_version"] = json!(3);
        facts["requirements"] = json!([]);
        let actual = result(row(&facts));
        assert_eq!(actual["category"], "valid_extraction_review");
        assert_eq!(actual["contract_validation"], "passed");
        assert_eq!(actual["extraction"], facts);
        assert!(
            actual["review_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason.as_str().unwrap().contains("No requirements"))
        );
    }

    #[test]
    fn replay_preserves_pipeline_coverage_review() {
        let mut saved = row(&extraction());
        saved["version"]["saved_extraction"] =
            json!({"review_reasons":["Previously supplied files are no longer listed"]});
        let actual = result(saved);
        assert_eq!(actual["category"], "valid_extraction_review");
        assert_eq!(
            actual["saved_pipeline_review_reasons"][0],
            "Previously supplied files are no longer listed"
        );
    }

    #[test]
    fn preserved_malformed_provider_bytes_remain_json_error() {
        let mut wrapper = row(&extraction());
        wrapper["response"] =
            json!({"_raw_response":"{not-json","error":{"message":"Invalid provider JSON"}});
        let actual = result(wrapper);
        assert_eq!(actual["category"], "json_error");
        assert_eq!(actual["stage"], "provider_json");
        assert!(
            actual["reason"]
                .as_str()
                .unwrap()
                .contains("Preserved raw provider response")
        );
    }

    #[test]
    fn duplicate_keys_are_rejected_in_export_response_and_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("duplicate-export.json");
        std::fs::write(
            &path,
            r#"[{"attempt_id":1,"attempt_id":2,"notice_id":"notice-1","response":null}]"#,
        )
        .unwrap();
        let error = from_file(&path).unwrap_err();
        assert!(
            format!("{error:#}").contains("Duplicate")
                || format!("{error:#}").contains("duplicate")
        );

        let mut duplicate_response = row(&extraction());
        duplicate_response["response"] = json!(format!(
            r#"{{"model":"{}","model":"{}","choices":[]}}"#,
            model::MODEL,
            model::MODEL
        ));
        let actual = result(duplicate_response);
        assert_eq!(actual["category"], "json_error");
        assert_eq!(actual["stage"], "provider_json");

        let mut duplicate_content = row(&extraction());
        duplicate_content["response"]["choices"][0]["message"]["content"] =
            json!(r#"{"status":"open","status":"closed"}"#);
        let actual = result(duplicate_content);
        assert_eq!(actual["category"], "json_error");
        assert_eq!(actual["stage"], "extraction_json");
        assert_eq!(actual["citation_allowlist_validated"], false);
    }

    #[test]
    fn rejects_invalid_container_ids_and_oversized_files() {
        assert!(replay(&json!({})).is_err());
        assert!(replay(&json!([null])).is_err());
        assert!(replay(&json!([{"attempt_id":0,"notice_id":"x","response":null}])).is_err());
        assert!(replay(&json!([{"attempt_id":1,"notice_id":"","response":null}])).is_err());
        assert!(replay(&json!([{"attempt_id":1,"notice_id":"x"}])).is_err());
        assert!(
            replay(&json!(vec![
                row(&extraction());
                MAX_DIAGNOSTIC_ATTEMPTS + 1
            ]))
            .is_err()
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oversized.json");
        File::create(&path)
            .unwrap()
            .set_len(MAX_DIAGNOSTIC_BYTES as u64 + 1)
            .unwrap();
        assert!(from_file(&path).is_err());
        assert_eq!(replay(&json!([])).unwrap()["count"], 0);
    }
}
