use funding_rust::{engine, model};
use serde_json::{Value, json};

const SOURCE: &str = "https://example.gov/call";
fn sources() -> Vec<String> {
    vec![SOURCE.into()]
}
fn municipality() -> Value {
    json!({"istatCode":"081001","region":"Sicilia","registryReferenceDate":"2026-10-06"})
}
fn condition(id: &str, field: &str, value: Value) -> Value {
    json!({"id":id,"label":id,"op":"equals","field":field,"value":value,"citation_ids":["c1"]})
}
fn extraction(requirements: Vec<Value>) -> Value {
    json!({"schema_version":2,"title":"Funding call","summary":"Source facts only",
        "status":"open","opens_on":"2026-01-01","closes_at":"2026-12-31T12:00:00Z",
        "review_reasons":[],"requirements":requirements,
        "citations":[{"id":"c1","source_url":SOURCE,"locator":"Eligibility section",
        "quote":"Applicants must be startups registered in Estonia."}]})
}
fn evaluate(extraction: &Value) -> Value {
    engine::evaluate_for_call(
        extraction,
        &municipality(),
        &[],
        None,
        Some("example"),
        "2026-10-06T12:00:00Z",
    )
}
fn startup() -> Value {
    extraction(vec![
        condition("country", "entity.country", json!("EE")),
        condition("type", "entity.kind", json!("startup")),
    ])
}

#[test]
fn estonian_startup_is_valid_extraction_and_definite_local_exclusion() {
    let call = startup();
    engine::validate_extraction(&call, &sources()).unwrap();
    let result = evaluate(&call);
    assert_eq!(result["state"], "excluded");
    assert_eq!(result["visible"], false);
    assert_eq!(result["results"][0]["scope"], "municipality"); // internal evidence scope only
    assert_eq!(result["results"][0]["blocker"], "applicant"); // derived, never model-authored
    assert_eq!(result["results"][0]["state"], "fail");
    assert_eq!(result["results"][1]["state"], "fail");
    assert_eq!(result["legal_clearance"], false);
}
#[test]
fn unrelated_unknown_project_and_deadline_do_not_undo_known_country_mismatch() {
    let mut call = startup();
    call["status"] = json!("unknown");
    call["closes_at"] = Value::Null;
    call["requirements"].as_array_mut().unwrap().push(condition(
        "project",
        "project.publicOwnership",
        json!(true),
    ));
    call["requirements"].as_array_mut().unwrap().push(json!({"id":"extra","label":"Extra project condition","op":"manual","note":"Project technical certification must be checked.","citation_ids":["c1"]}));
    engine::validate_extraction(&call, &sources()).unwrap();
    let result = evaluate(&call);
    assert_eq!(result["state"], "excluded");
    assert_eq!(result["results"][2]["state"], "unknown");
    assert_eq!(result["results"][3]["state"], "review_required");
    assert_eq!(result["window"]["state"], "unknown");
    assert!(!result["review_reasons"].as_array().unwrap().is_empty());
    assert!(!engine::extraction_review_reasons(&call).is_empty());
}
#[test]
fn source_ambiguity_cannot_create_definite_exclusion() {
    let mut call = startup();
    call["review_reasons"] =
        json!(["The unreadable eligibility annex might permit other applicant types/countries."]);
    engine::validate_extraction(&call, &sources()).unwrap();
    let result = evaluate(&call);
    assert_eq!(result["state"], "review_required");
    assert_eq!(result["visible"], true);
    assert_eq!(result["results"][0]["state"], "fail");
}
#[test]
fn open_municipal_and_broad_public_body_calls_match_without_name_special_cases() {
    for required in [
        condition("municipal", "entity.kind", json!("municipality")),
        condition("public", "entity.publicBody", json!(true)),
    ] {
        let call = extraction(vec![
            required,
            condition("country", "entity.country", json!("IT")),
        ]);
        engine::validate_extraction(&call, &sources()).unwrap();
        assert_eq!(evaluate(&call)["state"], "screening_match");
    }
}
#[test]
fn missing_or_ambiguous_facts_never_become_verified_matches() {
    let mut call = extraction(vec![condition(
        "municipal",
        "entity.kind",
        json!("municipality"),
    )]);
    call["closes_at"] = Value::Null;
    assert_eq!(evaluate(&call)["state"], "review_required");
    let call = extraction(vec![
        json!({"id":"unknown","label":"Ambiguous applicant class","op":"manual",
        "note":"The source category cannot be mapped faithfully.","citation_ids":["c1"]}),
    ]);
    engine::validate_extraction(&call, &sources()).unwrap();
    let result = evaluate(&call);
    assert_eq!(result["state"], "review_required");
    assert_eq!(result["visible"], true);
    assert!(!result["review_reasons"].as_array().unwrap().is_empty());
}
#[test]
fn compact_manual_has_no_field_and_no_model_global_blocker_is_accepted() {
    let manual = json!({"id":"manual","label":"Check condition","op":"manual","note":"Check an explicit source condition.","citation_ids":["c1"]});
    engine::validate_extraction(&extraction(vec![manual.clone()]), &sources()).unwrap();
    let mut bad = manual;
    bad["field"] = json!("entity.kind");
    let error = engine::validate_extraction(&extraction(vec![bad]), &sources()).unwrap_err();
    assert!(format!("{error:#}").contains("Requirement 1"));
    assert!(format!("{error:#}").contains("missing or unknown fields"));
    for field in ["project.publicOwnership", "application.noDoubleFunding"] {
        let mut bad = condition("old-pilot-combination", field, json!(true));
        bad["scope"] = json!("application");
        bad["blocker"] = json!("applicant");
        assert!(engine::validate_extraction(&extraction(vec![bad]), &sources()).is_err());
        let valid = extraction(vec![condition("source-condition", field, json!(true))]);
        engine::validate_extraction(&valid, &sources()).unwrap();
        assert_eq!(evaluate(&valid)["state"], "review_required");
    }
    let schema = engine::extraction_schema();
    for shape in schema["properties"]["requirements"]["items"]["anyOf"]
        .as_array()
        .unwrap()
    {
        let props = shape["properties"].as_object().unwrap();
        assert!(!props.contains_key("scope") && !props.contains_key("blocker"));
        if props["op"]["enum"] == json!(["manual"]) {
            assert!(!props.contains_key("field"));
            assert_eq!(props.len(), 5);
        }
    }
}
#[test]
fn schema_and_validation_couple_operator_fields_to_canonical_value_types() {
    for (field, value) in [
        ("entity.country", json!("Estonia")),
        ("entity.country", json!("ZZ")),
        ("entity.kind", json!("Comune")),
        ("entity.kind", json!("public_body")),
        ("entity.publicBody", json!("true")),
    ] {
        assert!(
            engine::validate_extraction(
                &extraction(vec![condition("bad", field, value)]),
                &sources()
            )
            .is_err()
        );
    }
    let schema = engine::extraction_schema();
    let variants = schema["properties"]["requirements"]["items"]["anyOf"]
        .as_array()
        .unwrap();
    let country = variants
        .iter()
        .find(|v| {
            v["properties"]["op"]["enum"] == json!(["equals"])
                && v["properties"]["field"]["enum"] == json!(["entity.country"])
        })
        .unwrap();
    assert!(
        country["properties"]["value"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("EE"))
    );
    assert!(
        !country["properties"]["value"]["enum"]
            .as_array()
            .unwrap()
            .contains(&json!("Estonia"))
    );
}
#[test]
fn compact_operators_and_precise_manual_diagnostics_remain_supported() {
    let cases = vec![
        json!({"id":"one","label":"Countries","op":"one_of","field":"entity.country","values":["IT","EE"],"citation_ids":["c1"]}),
        json!({"id":"range","label":"Cost","op":"range","field":"project.totalCost","min":null,"max":50000,"citation_ids":["c1"]}),
        json!({"id":"population","label":"Population","op":"range","field":"population","min":0,"max":50000,"population_basis":null,"reference_date":null,"citation_ids":["c1"]}),
        json!({"id":"ratio","label":"Grant ratio","op":"compare","left":[{"field":"application.requestedGrant","factor":1}],"right":[{"field":"application.eligibleCost","factor":0.5}],"relation":"lte","citation_ids":["c1"]}),
        json!({"id":"date","label":"Notice period","op":"min_days","field":"application.eventStart","days":30,"citation_ids":["c1"]}),
    ];
    for rule in cases {
        engine::validate_extraction(&extraction(vec![rule]), &sources()).unwrap();
    }
    let mut unknown = extraction(vec![condition("public", "entity.publicBody", json!(true))]);
    unknown["status"] = json!("unknown");
    unknown["closes_at"] = Value::Null;
    assert_eq!(engine::extraction_review_reasons(&unknown).len(), 2);
}
#[test]
fn malformed_or_truncated_provider_output_is_not_a_genuine_unknown_fact() {
    let mut response = json!({"model":model::MODEL,"choices":[{"finish_reason":"stop","message":{"content":startup().to_string()}}]});
    assert!(model::parse_response(&response, &sources()).is_ok());
    response["choices"][0]["message"]["content"] = json!("{invalid");
    assert!(
        model::parse_response(&response, &sources())
            .unwrap_err()
            .to_string()
            .contains("invalid JSON")
    );
    response["choices"][0]["finish_reason"] = json!("length");
    assert!(
        model::parse_response(&response, &sources())
            .unwrap_err()
            .to_string()
            .contains("Incomplete")
    );
}
#[test]
fn legacy_responses_remain_unmodified_and_unsafe_combinations_are_not_repaired() {
    let original: Value = serde_json::from_str(include_str!("fixtures/extraction.json")).unwrap();
    engine::validate_extraction(&original, &["https://example.gov/call.pdf".into()]).unwrap();
    let preserved = original.clone();
    evaluate(&original);
    assert_eq!(original, preserved);
    let mut bad = original.clone();
    bad["requirements"][0]["scope"] = json!("application");
    bad["requirements"][0]["blocker"] = json!("applicant");
    assert!(engine::validate_extraction(&bad, &["https://example.gov/call.pdf".into()]).is_err());
    let mut bad = original;
    bad["requirements"][0]["op"] = json!("manual");
    bad["requirements"][0]["value"] = Value::Null;
    bad["requirements"][0]["note"] = json!("Check the source condition.");
    assert!(engine::validate_extraction(&bad, &["https://example.gov/call.pdf".into()]).is_err());
}

#[test]
fn every_population_basis_requires_a_known_reference_date_before_matching_or_excluding() {
    for basis in ["resident", "estimate", "legal"] {
        for population in [2000, 6000] {
            let rule = json!({"id":"population","label":"Population threshold","op":"range","field":"population","min":null,"max":5000,"population_basis":basis,"reference_date":null,"citation_ids":["c1"]});
            let mut call = extraction(vec![rule]);
            let evidence = vec![
                json!({"id":"population","municipalityId":"081001","projectId":null,"callId":null,"field":"population","value":population,"provenance":"official","observedAt":"2026-01-01","validUntil":null,"populationBasis":basis,"source":{"title":"Official population"}}),
            ];
            let result = engine::evaluate(
                &call,
                &municipality(),
                &evidence,
                None,
                "2026-10-06T12:00:00Z",
            );
            assert_eq!(result["state"], "review_required", "{basis}: {population}");
            assert_eq!(result["visible"], true);
            assert_eq!(result["results"][0]["state"], "review_required");
            call["requirements"].as_array_mut().unwrap().push(condition(
                "country",
                "entity.country",
                json!("EE"),
            ));
            assert_eq!(
                engine::evaluate(
                    &call,
                    &municipality(),
                    &evidence,
                    None,
                    "2026-10-06T12:00:00Z"
                )["state"],
                "excluded"
            );
        }
    }
}

#[test]
fn region_names_are_canonical_and_aliases_never_cause_false_exclusion() {
    for region in ["Sicily", "sicilia"] {
        let call = extraction(vec![condition("region", "entity.region", json!(region))]);
        assert!(engine::validate_extraction(&call, &sources()).is_err());
        assert_ne!(evaluate(&call)["state"], "excluded");
        assert_eq!(evaluate(&call)["visible"], true);
        assert_eq!(evaluate(&call)["needs_review"], true);
    }
    let call = extraction(vec![condition("region", "entity.region", json!("Sicilia"))]);
    assert_eq!(evaluate(&call)["state"], "screening_match");
}
#[test]
fn identifier_schema_declares_the_same_ascii_syntax_as_validation() {
    let schema = engine::extraction_schema();
    let variants = schema["properties"]["requirements"]["items"]["anyOf"]
        .as_array()
        .unwrap();
    for variant in variants {
        assert_eq!(variant["properties"]["id"]["pattern"], "^[A-Za-z0-9_.:-]+$");
        assert_eq!(
            variant["properties"]["citation_ids"]["items"]["pattern"],
            "^[A-Za-z0-9_.:-]+$"
        );
    }
    assert_eq!(
        schema["properties"]["citations"]["items"]["properties"]["id"]["pattern"],
        "^[A-Za-z0-9_.:-]+$"
    );
    for invalid in ["requisito 1", "città"] {
        let mut call = startup();
        call["requirements"][0]["id"] = json!(invalid);
        assert!(engine::validate_extraction(&call, &sources()).is_err());
    }
}
#[test]
fn new_responses_require_v2_while_replay_can_read_unchanged_legacy() {
    let legacy: Value = serde_json::from_str(include_str!("fixtures/extraction.json")).unwrap();
    let response = json!({"model":model::MODEL,"choices":[{"finish_reason":"stop","message":{"content":legacy.to_string()}}]});
    let sources = vec!["https://example.gov/call.pdf".into()];
    assert!(model::parse_response(&response, &sources).is_err());
    assert_eq!(
        model::parse_saved_response(&response, &sources).unwrap(),
        legacy
    );
}
#[test]
fn duplicate_keys_cannot_erase_eligibility_uncertainty_live_or_during_replay() {
    let raw = startup().to_string().replace(
        "\"review_reasons\":[]",
        "\"review_reasons\":[\"Unclear alternative applicant eligibility\"],\"review_reasons\":[]",
    );
    let response = json!({"model":model::MODEL,"choices":[{"finish_reason":"stop","message":{"content":raw}}]});
    for parsed in [
        model::parse_response(&response, &sources()),
        model::parse_saved_response(&response, &sources()),
    ] {
        let error = parsed.unwrap_err();
        assert!(format!("{error:#}").contains("Duplicate JSON key: review_reasons"));
    }
}
