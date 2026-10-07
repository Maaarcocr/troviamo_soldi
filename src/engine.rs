//! A closed, data-only eligibility screener. This is not a legal eligibility decision.
//! Model citations are reported by the model; their quotations are not verified locally.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet};
use std::sync::OnceLock;

const OPS: &[&str] = &["equals", "one_of", "range", "compare", "min_days", "manual"];
const SCOPES: &[&str] = &["municipality", "project", "application"];
const RULE_KEYS: &[&str] = &[
    "id",
    "label",
    "op",
    "field",
    "scope",
    "blocker",
    "value",
    "values",
    "min",
    "max",
    "left",
    "right",
    "relation",
    "days",
    "citation_ids",
    "note",
    "population_basis",
    "reference_date",
];
const TOP_KEYS: &[&str] = &[
    "title",
    "summary",
    "status",
    "opens_on",
    "closes_at",
    "needs_review",
    "review_reasons",
    "requirements",
    "citations",
];

pub fn fields() -> &'static Value {
    static FIELDS: OnceLock<Value> = OnceLock::new();
    FIELDS.get_or_init(|| {
        serde_json::from_str(include_str!("../data/fields.json")).expect("bundled field catalogue")
    })
}

fn closed_object<'a>(
    value: &'a Value,
    keys: &[&str],
    name: &str,
) -> Result<&'a Map<String, Value>> {
    let object = value
        .as_object()
        .with_context(|| format!("{name} must be an object"))?;
    ensure!(
        object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key)),
        "{name} has missing or unknown fields"
    );
    Ok(object)
}
fn has_unsafe_controls(value: &str) -> bool {
    value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}
fn text<'a>(value: &'a Value, name: &str, min: usize, max: usize) -> Result<&'a str> {
    let value = value
        .as_str()
        .with_context(|| format!("{name} must be a string"))?;
    let length = value.trim().chars().count();
    ensure!(
        length >= min && length <= max && !has_unsafe_controls(value),
        "{name} has invalid length/content"
    );
    Ok(value)
}
fn identifier<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    text(value, name, 1, 160)
}
fn enumeration<'a>(value: &'a Value, options: &[&str], name: &str) -> Result<&'a str> {
    let value = value
        .as_str()
        .with_context(|| format!("{name} must be a string"))?;
    ensure!(options.contains(&value), "Unsupported {name}: {value}");
    Ok(value)
}
fn date(value: &str) -> Option<NaiveDate> {
    if value.len() != 10 || value.as_bytes()[4] != b'-' || value.as_bytes()[7] != b'-' {
        return None;
    }
    let parsed = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    (parsed.format("%Y-%m-%d").to_string() == value).then_some(parsed)
}
fn nullable_date(value: &Value, name: &str) -> Result<()> {
    ensure!(
        value.is_null() || value.as_str().and_then(date).is_some(),
        "{name} must be null or an actual YYYY-MM-DD date"
    );
    Ok(())
}
fn numeric(value: &Value, name: &str, min: f64, max: f64) -> Result<f64> {
    let number = value
        .as_f64()
        .with_context(|| format!("{name} must be numeric"))?;
    ensure!(
        number.is_finite() && number >= min && number <= max,
        "{name} outside supported bounds"
    );
    Ok(number)
}
fn valid_value(value: &Value, definition: &Value) -> bool {
    if let Some(values) = definition["canonical_values"].as_array() {
        return values.contains(value);
    }
    match definition["type"].as_str() {
        Some("boolean") => value.is_boolean(),
        Some("number") => value.as_f64().is_some_and(|v| {
            v.is_finite()
                && (0.0..=1e15).contains(&v)
                && (definition["integer"] != true || v.fract() == 0.0)
        }),
        Some("date") => value.as_str().and_then(date).is_some(),
        Some("enum") => definition["options"]
            .as_array()
            .is_some_and(|options| options.iter().any(|option| option["value"] == *value)),
        Some("string") => value.as_str().is_some_and(|v| {
            !v.trim().is_empty() && v.chars().count() <= 3000 && !has_unsafe_controls(v)
        }),
        _ => false,
    }
}
fn equal_values(left: &Value, right: &Value) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}
fn definition(field: &str) -> Result<&'static Value> {
    fields()
        .get(field)
        .with_context(|| format!("Unknown field: {field}"))
}
fn require_null(rule: &Value, keys: &[&str]) -> Result<()> {
    for key in keys {
        ensure!(
            rule[*key].is_null(),
            "{} must be null for operator {}",
            key,
            rule["op"]
        );
    }
    Ok(())
}
fn rule_fields(rule: &Value) -> Vec<&str> {
    if rule["op"] == "compare" {
        ["left", "right"]
            .into_iter()
            .flat_map(|side| rule[side].as_array().into_iter().flatten())
            .filter_map(|term| term["field"].as_str())
            .collect()
    } else {
        rule["field"].as_str().into_iter().collect()
    }
}

/// Enforces the schema *and* operator-specific types, references, dates, and scope.
/// `allowed_sources` must come from the trusted call/configuration, not the model.
pub fn validate_extraction(value: &Value, allowed_sources: &[String]) -> Result<()> {
    if matches!(value["schema_version"].as_u64(), Some(2 | 3)) {
        let normalized = crate::contract::to_legacy(value)?;
        validate_extraction_inner(&normalized, Some(allowed_sources), false)
    } else {
        validate_extraction_inner(value, Some(allowed_sources), true)
    }
}

/// Offline structural diagnosis only: does not authenticate citation destinations.
/// Never use this to accept a live response or import recovered facts.
pub fn validate_extraction_structure(value: &Value) -> Result<()> {
    if matches!(value["schema_version"].as_u64(), Some(2 | 3)) {
        validate_extraction_inner(&crate::contract::to_legacy(value)?, None, false)
    } else {
        validate_extraction_inner(value, None, true)
    }
}

fn validate_extraction_inner(
    value: &Value,
    allowed_sources: Option<&[String]>,
    require_deadline_review_flag: bool,
) -> Result<()> {
    closed_object(value, TOP_KEYS, "Extraction")?;
    text(&value["title"], "title", 1, 500)?;
    text(&value["summary"], "summary", 1, 10_000)?;
    let status = enumeration(
        &value["status"],
        &["open", "forthcoming", "closed", "unknown"],
        "status",
    )?;
    nullable_date(&value["opens_on"], "opens_on")?;
    let closes = if value["closes_at"].is_null() {
        None
    } else {
        let raw = text(&value["closes_at"], "closes_at", 20, 64)?;
        Some(
            DateTime::parse_from_rfc3339(raw)
                .context("closes_at must be an RFC3339 timestamp with timezone")?,
        )
    };
    if let (Some(open), Some(close)) = (value["opens_on"].as_str().and_then(date), closes) {
        ensure!(
            close.date_naive() >= open,
            "closes_at cannot precede opens_on"
        );
    }
    let needs_review = value["needs_review"]
        .as_bool()
        .context("needs_review must be boolean")?;
    let reasons = value["review_reasons"]
        .as_array()
        .context("review_reasons must be an array")?;
    ensure!(reasons.len() <= 100, "Too many review reasons");
    for reason in reasons {
        text(reason, "review reason", 1, 3000)?;
    }
    ensure!(
        needs_review == !reasons.is_empty(),
        "needs_review must agree with review_reasons"
    );
    ensure!(
        !require_deadline_review_flag || status != "unknown" || needs_review,
        "Unknown call status requires a review reason"
    );
    ensure!(
        !require_deadline_review_flag || status != "open" || closes.is_some() || needs_review,
        "An open call without a confirmed deadline requires needs_review and a review reason"
    );
    let citations = value["citations"]
        .as_array()
        .context("citations must be an array")?;
    ensure!(citations.len() <= 300, "Too many citations");
    let mut citation_ids = HashSet::new();
    for citation in citations {
        closed_object(
            citation,
            &["id", "source_url", "locator", "quote"],
            "Citation",
        )?;
        let id = identifier(&citation["id"], "citation id")?;
        ensure!(citation_ids.insert(id), "Duplicate citation id: {id}");
        let source = text(&citation["source_url"], "citation source_url", 8, 3000)?;
        ensure!(
            (source.starts_with("https://") || source.starts_with("http://"))
                && !source.chars().any(char::is_whitespace),
            "Citation source must be an HTTP(S) URL"
        );
        ensure!(
            allowed_sources.is_none_or(|sources| sources.iter().any(|allowed| allowed == source)),
            "Citation source is outside the supplied official source list: {source}"
        );
        if !citation["locator"].is_null() {
            text(&citation["locator"], "citation locator", 1, 1000)?;
        }
        text(&citation["quote"], "citation quote", 1, 10_000)?;
    }
    let requirements = value["requirements"]
        .as_array()
        .context("requirements must be an array")?;
    ensure!(requirements.len() <= 200, "Too many requirements");
    ensure!(
        !requirements.is_empty() || needs_review,
        "An empty requirement list requires review"
    );
    let mut ids = HashSet::new();
    for (index, rule) in requirements.iter().enumerate() {
        let mut validate_rule = || -> Result<()> {
            closed_object(rule, RULE_KEYS, "Requirement")?;
            let id = identifier(&rule["id"], "requirement id")?;
            ensure!(ids.insert(id), "Duplicate requirement id: {id}");
            text(&rule["label"], "requirement label", 1, 1000)?;
            let op = enumeration(&rule["op"], OPS, "operator")?;
            let scope = enumeration(&rule["scope"], SCOPES, "scope")?;
            enumeration(
                &rule["blocker"],
                &["applicant", "project", "application"],
                "blocker",
            )?;
            ensure!(
                scope == "municipality" || rule["blocker"] != "applicant",
                "Project/application evidence cannot exclude an applicant globally"
            );
            if !rule["note"].is_null() {
                text(&rule["note"], "note", 1, 5000)?;
            }
            nullable_date(&rule["reference_date"], "reference_date")?;
            if !rule["population_basis"].is_null() {
                enumeration(
                    &rule["population_basis"],
                    &["estimate", "resident", "legal"],
                    "population_basis",
                )?;
            }
            let references = rule["citation_ids"]
                .as_array()
                .context("citation_ids must be an array")?;
            ensure!(
                !references.is_empty() && references.len() <= 30,
                "Each requirement needs 1–30 citations"
            );
            for reference in references {
                let reference = reference.as_str().context("Citation id must be a string")?;
                ensure!(
                    citation_ids.contains(reference),
                    "Missing citation reference: {reference}"
                );
            }
            if op == "manual" {
                require_null(
                    rule,
                    &[
                        "field",
                        "value",
                        "values",
                        "min",
                        "max",
                        "left",
                        "right",
                        "relation",
                        "days",
                        "population_basis",
                        "reference_date",
                    ],
                )?;
                text(&rule["note"], "manual note", 1, 5000)?;
                return Ok(());
            }
            if op == "compare" {
                require_null(rule, &["field", "value", "values", "min", "max", "days"])?;
                enumeration(&rule["relation"], &["lte", "gte"], "relation")?;
                for side in ["left", "right"] {
                    let terms = rule[side]
                        .as_array()
                        .with_context(|| format!("{side} must be a term array"))?;
                    ensure!(
                        !terms.is_empty() && terms.len() <= 20,
                        "Comparison sides require 1–20 terms"
                    );
                    let mut seen = HashSet::new();
                    for term in terms {
                        closed_object(term, &["field", "factor"], "Comparison term")?;
                        let field = term["field"]
                            .as_str()
                            .context("Term field must be string")?;
                        ensure!(
                            definition(field)?["type"] == "number",
                            "Comparison terms must be numeric fields"
                        );
                        ensure!(seen.insert(field), "Duplicate comparison field in one side");
                        let factor = numeric(&term["factor"], "factor", 0.0, 1e6)?;
                        ensure!(factor > 0.0, "Term factor must be positive");
                    }
                }
            } else {
                require_null(rule, &["left", "right", "relation"])?;
                let field = rule["field"]
                    .as_str()
                    .context("field must be a supported field string")?;
                let definition = definition(field)?;
                match op {
                    "equals" => {
                        require_null(rule, &["values", "min", "max", "days"])?;
                        ensure!(
                            valid_value(&rule["value"], definition),
                            "equals value does not match its field type"
                        );
                    }
                    "one_of" => {
                        require_null(rule, &["value", "min", "max", "days"])?;
                        let values = rule["values"]
                            .as_array()
                            .context("values must be an array")?;
                        ensure!(!values.is_empty(), "one_of requires at least one value");
                        for value in values {
                            ensure!(
                                valid_value(value, definition),
                                "one_of value does not match its field type"
                            );
                        }
                    }
                    "range" => {
                        require_null(rule, &["value", "values", "days"])?;
                        ensure!(
                            definition["type"] == "number",
                            "range requires a numeric field"
                        );
                        ensure!(
                            !rule["min"].is_null() || !rule["max"].is_null(),
                            "range needs at least one bound"
                        );
                        for bound in ["min", "max"] {
                            if !rule[bound].is_null() {
                                numeric(&rule[bound], bound, 0.0, 1e15)?;
                            }
                        }
                        if let (Some(min), Some(max)) = (rule["min"].as_f64(), rule["max"].as_f64())
                        {
                            ensure!(min <= max, "range min exceeds max");
                        }
                    }
                    "min_days" => {
                        require_null(rule, &["value", "values", "min", "max"])?;
                        ensure!(
                            definition["type"] == "date",
                            "min_days requires a date field"
                        );
                        ensure!(
                            rule["days"].as_u64().is_some_and(|n| n <= 36_600),
                            "days must be an integer between 0 and 36600"
                        );
                    }
                    _ => unreachable!(),
                }
            }
            let used_fields = rule_fields(rule);
            let inferred_scope = used_fields
                .iter()
                .map(|field| {
                    let scope = fields()[*field]["scope"].as_str().unwrap_or("");
                    SCOPES.iter().position(|item| *item == scope).unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            ensure!(
                scope == SCOPES[inferred_scope],
                "Rule scope must match its field scope (or most specific comparison scope)"
            );
            ensure!(
                scope == "municipality" || rule["blocker"] != "applicant",
                "Project/application evidence cannot exclude an applicant globally"
            );
            ensure!(
                used_fields.contains(&"population") || rule["population_basis"].is_null(),
                "population_basis only applies to population rules"
            );
            Ok(())
        };
        validate_rule().with_context(|| format!("Requirement {} ({})", index + 1, rule["id"]))?;
    }
    Ok(())
}

/// Compact model contract; legacy 18-key rules remain supported only for stored data.
pub fn extraction_schema() -> Value {
    crate::contract::schema()
}

/// All genuine unresolved facts are visible in diagnostics, even when JSON is valid.
pub fn extraction_review_reasons(value: &Value) -> Vec<String> {
    let mut reasons: Vec<String> = value["review_reasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if value["requirements"]
        .as_array()
        .is_some_and(|rules| rules.is_empty())
    {
        reasons.push("No requirements were extracted from the supplied sources".into());
    }
    if value["status"] == "unknown" {
        reasons.push("Opening status is unknown".into());
    }
    if value["status"] != "closed" && value["closes_at"].is_null() {
        reasons.push("Application deadline/timezone is not established".into());
    }
    for rule in value["requirements"].as_array().into_iter().flatten() {
        if rule["op"] == "manual" {
            reasons.push(format!(
                "{}: {}",
                rule["id"].as_str().unwrap_or("manual"),
                rule["note"]
                    .as_str()
                    .unwrap_or("Condition requires manual checking")
            ));
        }
        if rule["field"] == "population"
            && (rule["population_basis"].is_null() || rule["reference_date"].is_null())
        {
            reasons.push(format!(
                "{}: population basis/reference date is not established",
                rule["id"].as_str().unwrap_or("population")
            ));
        }
    }
    reasons.sort();
    reasons.dedup();
    reasons
}

#[derive(Clone)]
struct Instant {
    date: NaiveDate,
    exact: Option<DateTime<Utc>>,
}
fn instant(value: &str) -> Option<Instant> {
    if let Some(date) = date(value) {
        return Some(Instant { date, exact: None });
    }
    let exact = DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&Utc);
    Some(Instant {
        date: exact.date_naive(),
        exact: Some(exact),
    })
}
fn base_evidence(municipality: &Value) -> Vec<Value> {
    let Some(id) = municipality["istatCode"].as_str() else {
        return vec![];
    };
    let Some(observed_at) = municipality["registryReferenceDate"]
        .as_str()
        .filter(|v| date(v).is_some())
    else {
        return vec![];
    };
    let Some(region) = municipality["region"].as_str().filter(|r| !r.is_empty()) else {
        return vec![];
    };
    [("entity.kind", json!("municipality")), ("entity.region", json!(region)), ("entity.country", json!("IT")), ("entity.publicBody", json!(true))]
        .into_iter().map(|(field,value)| json!({
            "id":format!("registry:{id}:{field}"),"municipalityId":id,"projectId":null,"callId":null,
            "field":field,"value":value,"provenance":"official","observedAt":observed_at,"validUntil":null,
            "source":{"title":"ISTAT · Comuni vigenti · SITUAS","url":"https://situas.istat.it/","locator":format!("Codice ISTAT {id}; riferimento {observed_at}")}
        })).collect()
}
fn nullable_matches(value: &Value, expected: Option<&str>) -> bool {
    match expected {
        Some(expected) => value.as_str() == Some(expected),
        None => value.is_null(),
    }
}
fn fact(state: &str, field: &str, records: &[&Value], reason: &str) -> Value {
    json!({"state":state,"field":field,"records":records,"reason":reason})
}
fn resolve_evidence(
    field: &str,
    rule: &Value,
    municipality_id: &str,
    project_id: Option<&str>,
    call_id: Option<&str>,
    evidence: &[Value],
    as_of: &Instant,
) -> Value {
    let definition = &fields()[field];
    let scope = definition["scope"].as_str().unwrap_or("");
    if scope != "municipality" && project_id.is_none() {
        return fact(
            "unknown",
            field,
            &[],
            "No project selected; project facts are never borrowed from another project",
        );
    }
    if scope == "application" && call_id.is_none() {
        return fact(
            "unknown",
            field,
            &[],
            "No call context supplied; application facts are never borrowed from another call",
        );
    }
    let records: Vec<_> = evidence
        .iter()
        .filter(|record| {
            record["field"] == field
                && record["municipalityId"] == municipality_id
                && record.get("projectId").is_some_and(|v| {
                    nullable_matches(
                        v,
                        if scope == "municipality" {
                            None
                        } else {
                            project_id
                        },
                    )
                })
                && record.get("callId").is_some_and(|v| {
                    nullable_matches(
                        v,
                        if scope == "application" {
                            call_id
                        } else {
                            None
                        },
                    )
                })
                && valid_value(&record["value"], definition)
                && ["official", "curated", "user_declared"]
                    .contains(&record["provenance"].as_str().unwrap_or(""))
                && record["observedAt"].as_str().and_then(date).is_some()
                && (record["validUntil"].is_null()
                    || record["validUntil"].as_str().and_then(date).is_some())
                && record["source"]["title"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty())
        })
        .collect();
    if records.is_empty() {
        return fact(
            "unknown",
            field,
            &[],
            "No valid evidence available in this municipality/project/application scope",
        );
    }
    // Population estimates are informational unless the source requirement explicitly accepts them.
    if field == "population" && rule["population_basis"].is_null() {
        return fact(
            "review_required",
            field,
            &records,
            "Population basis is unspecified; an estimate cannot prove an arbitrary demographic or legal criterion",
        );
    }
    if field == "population" && rule["reference_date"].is_null() {
        return fact(
            "review_required",
            field,
            &records,
            "Population requires an explicit reference date; an arbitrary year's value cannot establish the condition",
        );
    }
    let matching: Vec<_> = records
        .iter()
        .copied()
        .filter(|record| {
            let basis_matches = if field != "population" {
                true
            } else {
                let basis = record["populationBasis"]
                    .as_str()
                    .or_else(|| record["quality"].as_str());
                basis == rule["population_basis"].as_str()
            };
            let date_matches = rule["reference_date"].is_null()
                || record["referenceDate"]
                    .as_str()
                    .or_else(|| record["observedAt"].as_str())
                    == rule["reference_date"].as_str();
            basis_matches && date_matches
        })
        .collect();
    if matching.is_empty() {
        return fact(
            "unknown",
            field,
            &records,
            "Available evidence does not establish the required population basis or exact reference date",
        );
    }
    if matching
        .iter()
        .skip(1)
        .any(|record| !equal_values(&record["value"], &matching[0]["value"]))
    {
        return fact(
            "conflict",
            field,
            &matching,
            "Contradictory evidence must be reconciled; source rank does not silently resolve it",
        );
    }
    let mut fresh: Vec<_> = matching
        .iter()
        .copied()
        .filter(|record| {
            let observed = record["observedAt"].as_str().and_then(date).unwrap();
            observed <= as_of.date
                && record["refreshRequired"] != true
                && (record["validUntil"].is_null()
                    || record["validUntil"]
                        .as_str()
                        .and_then(date)
                        .is_some_and(|until| until >= as_of.date && until >= observed))
                && definition["maxAgeDays"]
                    .as_i64()
                    .is_none_or(|max_age| (as_of.date - observed).num_days() <= max_age)
        })
        .collect();
    if fresh.is_empty() {
        return fact(
            "stale",
            field,
            &matching,
            "Evidence is expired, future-dated, too old, or marked for reconfirmation",
        );
    }
    let rank = |record: &Value| match record["provenance"].as_str() {
        Some("official") => 3,
        Some("curated") => 2,
        _ => 1,
    };
    fresh.sort_by(|a, b| {
        rank(b)
            .cmp(&rank(a))
            .then_with(|| b["observedAt"].as_str().cmp(&a["observedAt"].as_str()))
    });
    let chosen = fresh[0];
    json!({"state":"known","field":field,"value":chosen["value"],"provenance":chosen["provenance"],"records":matching,"chosen":chosen})
}
fn evaluate_rule(
    rule: &Value,
    municipality_id: &str,
    project_id: Option<&str>,
    call_id: Option<&str>,
    evidence: &[Value],
    as_of: &Instant,
) -> Value {
    let mut result = rule.clone();
    let mut complete = |state: &str, reason: &str, facts: Value| {
        result["state"] = json!(state);
        result["reason"] = json!(reason);
        result["evidence"] = facts;
        result.clone()
    };
    if rule["op"] == "manual" {
        return complete(
            "review_required",
            rule["note"].as_str().unwrap_or("Office review required"),
            json!([]),
        );
    }
    let unique_fields: BTreeSet<_> = rule_fields(rule).into_iter().collect();
    let facts: Vec<_> = unique_fields
        .iter()
        .map(|field| {
            resolve_evidence(
                field,
                rule,
                municipality_id,
                project_id,
                call_id,
                evidence,
                as_of,
            )
        })
        .collect();
    for state in ["conflict", "stale", "unknown", "review_required"] {
        let reasons: Vec<_> = facts
            .iter()
            .filter(|f| f["state"] == state)
            .map(|f| {
                format!(
                    "{}: {}",
                    f["field"].as_str().unwrap_or(""),
                    f["reason"].as_str().unwrap_or("")
                )
            })
            .collect();
        if !reasons.is_empty() {
            return complete(state, &reasons.join("; "), json!(facts));
        }
    }
    let value = |field: &str| -> &Value {
        &facts.iter().find(|fact| fact["field"] == field).unwrap()["value"]
    };
    let field = rule["field"].as_str().unwrap_or("");
    let passed = match rule["op"].as_str().unwrap_or("") {
        "equals" => equal_values(value(field), &rule["value"]),
        "one_of" => rule["values"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| equal_values(candidate, value(field))),
        "range" => {
            let number = value(field).as_f64().unwrap();
            rule["min"].as_f64().is_none_or(|min| number >= min)
                && rule["max"].as_f64().is_none_or(|max| number <= max)
        }
        "compare" => {
            let sum = |side: &str| -> f64 {
                rule[side]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|term| {
                        value(term["field"].as_str().unwrap()).as_f64().unwrap()
                            * term["factor"].as_f64().unwrap()
                    })
                    .sum()
            };
            let left = sum("left");
            let right = sum("right");
            // Same half-cent arithmetic tolerance as the original data-only screener.
            if rule["relation"] == "lte" {
                left <= right + 0.005
            } else {
                left + 0.005 >= right
            }
        }
        "min_days" => {
            (date(value(field).as_str().unwrap()).unwrap() - as_of.date).num_days()
                >= rule["days"].as_i64().unwrap()
        }
        _ => false,
    };
    let declared = facts
        .iter()
        .any(|fact| fact["provenance"] == "user_declared");
    let state = if !passed {
        "fail"
    } else if declared {
        "declared"
    } else {
        "pass"
    };
    let reason = if !passed {
        "Available evidence does not satisfy this requirement"
    } else if declared {
        "Consistent with user-declared facts; supporting documents still require review"
    } else {
        "Consistent with the available evidence"
    };
    complete(state, reason, json!(facts))
}
fn window(extraction: &Value, as_of: &Instant) -> Value {
    if extraction["status"] == "closed" {
        return json!({"state":"closed","reason":"Source marks the call closed"});
    }
    if let Some(close) = extraction["closes_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        let close = close.with_timezone(&Utc);
        if as_of.exact.is_some_and(|now| now > close) || as_of.date > close.date_naive() {
            return json!({"state":"closed","reason":"The application deadline has passed"});
        }
        if as_of.exact.is_none() && as_of.date == close.date_naive() {
            return json!({"state":"unknown","reason":"The deadline is today; provide an RFC3339 as_of timestamp to check the exact cutoff"});
        }
    }
    if extraction["opens_on"]
        .as_str()
        .and_then(date)
        .is_some_and(|open| open > as_of.date)
        || extraction["status"] == "forthcoming"
    {
        return json!({"state":"forthcoming","reason":"The call is not yet confirmed open"});
    }
    if extraction["status"] == "unknown" {
        return json!({"state":"unknown","reason":"Opening status requires verification"});
    }
    if extraction["closes_at"].is_null() {
        return json!({"state":"unknown","reason":"No confirmed application deadline; current opening status requires review"});
    }
    json!({"state":"open","reason":"Open according to the supplied extraction and evaluation time"})
}

/// Without a call context, application-scoped facts safely remain unknown.
pub fn evaluate(
    extraction: &Value,
    municipality: &Value,
    evidence: &[Value],
    project_id: Option<&str>,
    as_of: &str,
) -> Value {
    evaluate_for_call(extraction, municipality, evidence, project_id, None, as_of)
}

/// Screening result only. `call_id` comes from the trusted store/request context.
/// Never copy it from model output or reuse application facts across calls.
pub fn evaluate_for_call(
    extraction: &Value,
    municipality: &Value,
    evidence: &[Value],
    project_id: Option<&str>,
    call_id: Option<&str>,
    as_of: &str,
) -> Value {
    let is_compact = matches!(extraction["schema_version"].as_u64(), Some(2 | 3));
    let normalized;
    let extraction = if is_compact {
        normalized = match crate::contract::to_legacy(extraction) {
            Ok(value) => value,
            Err(error) => {
                return json!({"state":"invalid_extraction","visible":true,"needs_review":true,"reason":"Stored extraction does not satisfy the current contract; review the original source before matching","error":format!("{error:#}"),"review_reasons":[format!("{error:#}")],"results":[],"legal_clearance":false});
            }
        };
        &normalized
    } else {
        extraction
    };
    let sources: Vec<String> = extraction["citations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["source_url"].as_str().map(str::to_owned))
        .collect();
    // Older stored/external extractions may omit the review flag for a missing
    // deadline. Validate every other invariant, then preserve them as visible
    // review cases below; never turn an absent deadline into perpetual openness.
    if let Err(error) = validate_extraction_inner(extraction, Some(&sources), false) {
        return json!({"state":"invalid_extraction","visible":true,"needs_review":true,"reason":"Stored extraction does not satisfy the current contract; review the original source before matching","error":format!("{error:#}"),"review_reasons":[format!("{error:#}")],"results":[],"legal_clearance":false});
    }
    let Some(time) = instant(as_of) else {
        return json!({"state":"invalid_context","visible":false,"error":"as_of must be YYYY-MM-DD or RFC3339","results":[],"legal_clearance":false});
    };
    let Some(id) = municipality["istatCode"]
        .as_str()
        .filter(|s| s.len() == 6 && s.bytes().all(|c| c.is_ascii_digit()))
    else {
        return json!({"state":"invalid_context","visible":false,"error":"municipality needs a six-digit ISTAT code","results":[],"legal_clearance":false});
    };
    let mut all_evidence = base_evidence(municipality);
    all_evidence.extend(evidence.iter().cloned());
    let results: Vec<_> = extraction["requirements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| evaluate_rule(rule, id, project_id, call_id, &all_evidence, &time))
        .collect();
    let window = window(extraction, &time);
    let applicant_failure = results
        .iter()
        .any(|r| r["state"] == "fail" && r["blocker"] == "applicant");
    let any_failure = results.iter().any(|r| r["state"] == "fail");
    let all_pass = !results.is_empty() && results.iter().all(|r| r["state"] == "pass");
    let missing_deadline = extraction["status"] == "open" && extraction["closes_at"].is_null();
    let mut review_reasons = extraction["review_reasons"].as_array().unwrap().clone();
    if missing_deadline {
        review_reasons.push(json!(
            "No confirmed application deadline; current opening status requires review"
        ));
    }
    // Extraction uncertainty applies to the purported rules and dates themselves.
    // Keep the raw failed-rule/closed-window details, but do not silently hide a
    // notice whose annexes, conditions, or deadline have not been established.
    let state = if extraction["needs_review"] == true || (!is_compact && missing_deadline) {
        "review_required"
    } else if window["state"] == "closed" {
        "closed"
    } else if applicant_failure {
        "excluded"
    } else if any_failure {
        "ineligible"
    } else if window["state"] == "forthcoming" {
        "forthcoming"
    } else if all_pass && window["state"] == "open" {
        "screening_match"
    } else {
        "review_required"
    };
    if window["state"] == "unknown" {
        review_reasons.push(window["reason"].clone());
    }
    for result in &results {
        if !matches!(result["state"].as_str(), Some("pass" | "fail")) {
            review_reasons.push(json!(format!(
                "{}: {}",
                result["label"].as_str().unwrap_or("Condition"),
                result["reason"].as_str().unwrap_or("Unknown")
            )));
        }
    }
    let decision_reason = match state {
        "excluded" => {
            "Verified applicant facts conflict with an explicit cited applicant requirement"
        }
        "ineligible" => "Available project/application facts fail a cited requirement",
        "closed" => "The documented application window is closed",
        "screening_match" => {
            "All extracted conditions match the available evidence; this is not legal clearance"
        }
        "forthcoming" => "The application window is not yet open",
        _ => "Eligibility cannot be established from the available source or applicant facts",
    };
    let visible = !matches!(state, "closed" | "excluded" | "ineligible");
    let mut counts = Map::new();
    for result in &results {
        let state = result["state"].as_str().unwrap_or("unknown");
        let number = counts.get(state).and_then(Value::as_u64).unwrap_or(0) + 1;
        counts.insert(state.to_string(), json!(number));
    }
    json!({
        "state":state,"reason":decision_reason,"visible":visible,"window":window,"results":results,"counts":counts,
        "municipality_id":id,"project_id":project_id,"call_id":call_id,"as_of":as_of,
        "needs_review":matches!(state, "review_required" | "forthcoming"), "review_reasons":review_reasons,
        "legal_clearance":false,"citation_verification":"model_reported_unverified",
        "disclaimer":"Preliminary screening only. Source quotations are model-reported and have not been locally verified. Confirm the complete call, current facts, and legal eligibility with the responsible office."
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = "https://example.gov/call.pdf";
    fn sources() -> Vec<String> {
        vec![SOURCE.to_string()]
    }
    fn municipality() -> Value {
        json!({"istatCode":"081001","name":"Example","region":"Sicilia","registryReferenceDate":"2026-10-06"})
    }
    fn rule(op: &str, field: Option<&str>) -> Value {
        let scope = field
            .map(|field| fields()[field]["scope"].as_str().unwrap())
            .unwrap_or("application");
        json!({
            "id":"r1","label":"Test requirement","op":op,"field":field,"scope":scope,
            "blocker":if scope == "municipality" {"applicant"} else {scope},
            "value":null,"values":null,"min":null,"max":null,"left":null,"right":null,
            "relation":null,"days":null,"citation_ids":["c1"],"note":null,
            "population_basis":null,"reference_date":null
        })
    }
    fn extraction(rule: Value) -> Value {
        json!({
            "title":"Public funding call","summary":"Documented screening requirements.","status":"open",
            "opens_on":"2026-01-01","closes_at":"2026-12-31T12:00:00+01:00",
            "needs_review":false,"review_reasons":[],"requirements":[rule],
            "citations":[{"id":"c1","source_url":SOURCE,"locator":"Article 3, page 4","quote":"Eligible applicants are municipalities."}]
        })
    }
    fn equality(field: &str, value: Value) -> Value {
        let mut rule = rule("equals", Some(field));
        rule["value"] = value;
        rule
    }
    fn record(field: &str, value: Value) -> Value {
        let scope = fields()[field]["scope"].as_str().unwrap();
        json!({
            "id":"e1","municipalityId":"081001",
            "projectId":if scope == "municipality" {Value::Null} else {json!("p1")},
            "callId":if scope == "application" {json!("call-a")} else {Value::Null},
            "field":field,"value":value,"provenance":"official","observedAt":"2026-10-01",
            "validUntil":null,"source":{"title":"Official supporting document","url":SOURCE,"locator":"Page 1"}
        })
    }
    fn run(rule: Value, evidence: Vec<Value>) -> Value {
        evaluate_for_call(
            &extraction(rule),
            &municipality(),
            &evidence,
            Some("p1"),
            Some("call-a"),
            "2026-10-06T12:00:00Z",
        )
    }
    fn result_state(result: &Value) -> &str {
        result["results"][0]["state"].as_str().unwrap()
    }

    #[test]
    fn bundled_data_is_complete_and_unique() {
        let municipalities: Vec<Value> =
            serde_json::from_str(include_str!("../data/municipalities.json")).unwrap();
        let evidence: Vec<Value> =
            serde_json::from_str(include_str!("../data/evidence.json")).unwrap();
        assert_eq!(municipalities.len(), 391);
        assert_eq!(fields().as_object().unwrap().len(), 38);
        assert_eq!(evidence.len(), 511);
        let ids: HashSet<_> = municipalities
            .iter()
            .map(|m| m["istatCode"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 391);
        assert!(
            evidence
                .iter()
                .all(|r| ids.contains(r["municipalityId"].as_str().unwrap()))
        );
    }

    #[test]
    fn schema_objects_are_closed_and_all_properties_required() {
        fn walk(value: &Value) {
            if value["type"] == "object" {
                assert_eq!(value["additionalProperties"], false);
                let properties = value["properties"].as_object().unwrap();
                let required = value["required"].as_array().unwrap();
                assert_eq!(properties.len(), required.len());
                assert!(properties.keys().all(|key| required.contains(&json!(key))));
            }
            match value {
                Value::Object(map) => map.values().for_each(walk),
                Value::Array(items) => items.iter().for_each(walk),
                _ => {}
            }
        }
        walk(&extraction_schema());
    }

    #[test]
    fn municipality_anagraphic_rules_can_screen_but_never_grant_clearance() {
        let output = run(equality("entity.region", json!("Sicilia")), vec![]);
        assert_eq!(output["state"], "screening_match");
        assert_eq!(output["visible"], true);
        assert_eq!(output["legal_clearance"], false);
        assert_eq!(output["citation_verification"], "model_reported_unverified");
    }

    #[test]
    fn rejects_unknown_fields_ops_extra_properties_and_type_coercion() {
        let base = extraction(equality("entity.publicBody", json!(true)));
        let mutations = [
            ("op", json!("eval")),
            ("field", json!("__proto__")),
            ("value", json!("true")),
            ("scope", json!("project")),
            ("code", json!("process.exit()")),
            ("min", json!(3)),
        ];
        for (key, value) in mutations {
            let mut candidate = base.clone();
            candidate["requirements"][0][key] = value;
            assert!(
                validate_extraction(&candidate, &sources()).is_err(),
                "accepted {key}"
            );
        }
        let mut extra = base.clone();
        extra["arbitrary"] = json!(true);
        assert!(validate_extraction(&extra, &sources()).is_err());
        let mut missing = base.clone();
        missing["requirements"][0]
            .as_object_mut()
            .unwrap()
            .remove("note");
        assert!(validate_extraction(&missing, &sources()).is_err());
        let mut scope = extraction(equality("project.publicOwnership", json!(true)));
        scope["requirements"][0]["blocker"] = json!("applicant");
        assert!(validate_extraction(&scope, &sources()).is_err());
    }

    #[test]
    fn rejects_unapproved_or_useless_citations() {
        let base = extraction(equality("entity.kind", json!("municipality")));
        for (key, value) in [
            ("source_url", json!("https://attacker.example/source")),
            ("locator", json!(" ")),
            ("quote", json!("")),
            ("extra", json!(true)),
        ] {
            let mut candidate = base.clone();
            candidate["citations"][0][key] = value;
            assert!(validate_extraction(&candidate, &sources()).is_err());
        }
        for references in [json!([]), json!(["absent"])] {
            let mut candidate = base.clone();
            candidate["requirements"][0]["citation_ids"] = references;
            assert!(validate_extraction(&candidate, &sources()).is_err());
        }
        let mut duplicate = base.clone();
        duplicate["citations"]
            .as_array_mut()
            .unwrap()
            .push(base["citations"][0].clone());
        assert!(validate_extraction(&duplicate, &sources()).is_err());
    }

    #[test]
    fn validates_dates_and_review_consistency() {
        let base = extraction(equality("entity.kind", json!("municipality")));
        for invalid in ["2026-02-30", "2026-2-03", "nonsense"] {
            let mut candidate = base.clone();
            candidate["opens_on"] = json!(invalid);
            assert!(validate_extraction(&candidate, &sources()).is_err());
        }
        for invalid in ["2026-12-31", "2026-12-31T12:00:00", "2025-12-31T00:00:00Z"] {
            let mut candidate = base.clone();
            candidate["closes_at"] = json!(invalid);
            assert!(validate_extraction(&candidate, &sources()).is_err());
        }
        let mut candidate = base.clone();
        candidate["needs_review"] = json!(true);
        assert!(validate_extraction(&candidate, &sources()).is_err());
        candidate["review_reasons"] = json!(["Source needs checking"]);
        assert!(validate_extraction(&candidate, &sources()).is_ok());
    }

    #[test]
    fn one_of_and_range_preserve_types_and_bounds() {
        let mut one = rule("one_of", Some("project.worksProgramme"));
        one["values"] = json!(["included", "integration_started"]);
        assert_eq!(
            result_state(&run(
                one.clone(),
                vec![record("project.worksProgramme", json!("included"))]
            )),
            "pass"
        );
        one["values"] = json!(["unsupported"]);
        assert!(validate_extraction(&extraction(one), &sources()).is_err());
        let mut range = rule("range", Some("project.totalCost"));
        range["min"] = json!(100);
        range["max"] = json!(1000);
        assert_eq!(
            result_state(&run(
                range.clone(),
                vec![record("project.totalCost", json!(500))]
            )),
            "pass"
        );
        assert_eq!(
            result_state(&run(
                range.clone(),
                vec![record("project.totalCost", json!(1001))]
            )),
            "fail"
        );
        for (min, max) in [
            (json!(1001), json!(1000)),
            (json!(-1), json!(100)),
            (Value::Null, Value::Null),
        ] {
            range["min"] = min;
            range["max"] = max;
            assert!(validate_extraction(&extraction(range.clone()), &sources()).is_err());
        }
    }

    fn comparison() -> Value {
        let mut rule = rule("compare", None);
        rule["left"] = json!([{"field":"application.requestedGrant","factor":1}]);
        rule["right"] = json!([{"field":"application.eligibleCost","factor":0.5}]);
        rule["relation"] = json!("lte");
        rule
    }
    #[test]
    fn compare_honours_multipliers_half_cent_and_call_context() {
        let rule = comparison();
        let evidence = vec![
            record("application.requestedGrant", json!(50.004)),
            record("application.eligibleCost", json!(100)),
        ];
        assert_eq!(result_state(&run(rule.clone(), evidence.clone())), "pass");
        let mut failed = evidence.clone();
        failed[0]["value"] = json!(50.01);
        assert_eq!(result_state(&run(rule.clone(), failed)), "fail");
        let output = evaluate_for_call(
            &extraction(rule.clone()),
            &municipality(),
            &evidence,
            Some("p1"),
            Some("call-b"),
            "2026-10-06",
        );
        assert_eq!(result_state(&output), "unknown");
        let output = evaluate(
            &extraction(rule),
            &municipality(),
            &evidence,
            Some("p1"),
            "2026-10-06",
        );
        assert_eq!(result_state(&output), "unknown");
    }

    #[test]
    fn compare_rejects_invalid_terms_relations_and_scope() {
        for (key, value) in [
            ("factor", json!(0)),
            ("factor", json!(-1)),
            ("factor", json!(1e8)),
            ("factor", Value::Null),
            ("field", json!("entity.kind")),
            ("extra", json!(true)),
        ] {
            let mut rule = comparison();
            rule["left"][0][key] = value;
            assert!(validate_extraction(&extraction(rule), &sources()).is_err());
        }
        let mut rule = comparison();
        rule["relation"] = json!("eval");
        assert!(validate_extraction(&extraction(rule), &sources()).is_err());
        let mut rule = comparison();
        rule["right"] = json!([]);
        assert!(validate_extraction(&extraction(rule), &sources()).is_err());
    }

    #[test]
    fn min_days_counts_calendar_dates() {
        let mut rule = rule("min_days", Some("application.eventStart"));
        rule["days"] = json!(30);
        assert_eq!(
            result_state(&run(
                rule.clone(),
                vec![record("application.eventStart", json!("2026-11-05"))]
            )),
            "pass"
        );
        assert_eq!(
            result_state(&run(
                rule.clone(),
                vec![record("application.eventStart", json!("2026-11-04"))]
            )),
            "fail"
        );
        rule["days"] = json!(0.5);
        assert!(validate_extraction(&extraction(rule), &sources()).is_err());
    }

    #[test]
    fn manual_is_always_office_review() {
        let mut rule = rule("manual", None);
        rule["note"] = json!("Verify the required authorization with the office.");
        let output = run(rule, vec![]);
        assert_eq!(result_state(&output), "review_required");
        assert_eq!(output["state"], "review_required");
        assert_eq!(output["visible"], true);
    }

    #[test]
    fn isolate_municipality_project_and_application_facts() {
        let rule = equality("project.publicOwnership", json!(true));
        let good = record("project.publicOwnership", json!(true));
        for (key, value) in [
            ("municipalityId", json!("081002")),
            ("projectId", json!("p2")),
            ("projectId", Value::Null),
            ("callId", json!("call-a")),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert_eq!(result_state(&run(rule.clone(), vec![bad])), "unknown");
        }
        let output = evaluate_for_call(
            &extraction(rule),
            &municipality(),
            &[good],
            None,
            Some("call-a"),
            "2026-10-06",
        );
        assert_eq!(result_state(&output), "unknown");
        let rule = equality("financial.approvedBudget", json!(true));
        let mut bad = record("financial.approvedBudget", json!(true));
        bad["projectId"] = json!("p1");
        assert_eq!(result_state(&run(rule, vec![bad])), "unknown");
    }

    #[test]
    fn missing_false_and_zero_are_distinct() {
        let rule = equality("project.publicOwnership", json!(true));
        assert_eq!(result_state(&run(rule.clone(), vec![])), "unknown");
        assert_eq!(
            result_state(&run(
                rule,
                vec![record("project.publicOwnership", json!(false))]
            )),
            "fail"
        );
        let mut range = rule_for_cost();
        range["min"] = json!(0);
        range["max"] = json!(0);
        assert_eq!(
            result_state(&run(range, vec![record("project.totalCost", json!(0))])),
            "pass"
        );
    }
    fn rule_for_cost() -> Value {
        let mut r = rule("range", Some("project.totalCost"));
        r["max"] = json!(1000);
        r
    }

    #[test]
    fn stale_expired_future_or_refresh_required_never_pass() {
        let rule = equality("financial.approvedBudget", json!(true));
        let good = record("financial.approvedBudget", json!(true));
        for (key, value) in [
            ("observedAt", json!("2020-01-01")),
            ("observedAt", json!("2026-10-07")),
            ("validUntil", json!("2026-10-05")),
            ("refreshRequired", json!(true)),
        ] {
            let mut old = good.clone();
            old[key] = value;
            assert_eq!(result_state(&run(rule.clone(), vec![old])), "stale");
        }
    }

    #[test]
    fn contradictions_are_not_erased_by_source_rank_or_age() {
        let rule = equality("financial.approvedBudget", json!(true));
        let official = record("financial.approvedBudget", json!(true));
        let mut declared = record("financial.approvedBudget", json!(false));
        declared["provenance"] = json!("user_declared");
        declared["observedAt"] = json!("2020-01-01");
        assert_eq!(
            result_state(&run(rule, vec![official, declared])),
            "conflict"
        );
    }

    #[test]
    fn user_declared_pass_stays_declared() {
        let rule = equality("project.publicOwnership", json!(true));
        let mut declared = record("project.publicOwnership", json!(true));
        declared["provenance"] = json!("user_declared");
        let output = run(rule, vec![declared]);
        assert_eq!(result_state(&output), "declared");
        assert_eq!(output["state"], "review_required");
    }

    #[test]
    fn population_estimate_never_automatically_satisfies_legal_or_unknown_basis() {
        let mut rule = rule("range", Some("population"));
        rule["max"] = json!(50_000);
        let mut estimate = record("population", json!(25_000));
        estimate["quality"] = json!("estimate");
        estimate["observedAt"] = json!("2026-01-01");
        assert_eq!(
            result_state(&run(rule.clone(), vec![estimate.clone()])),
            "review_required"
        );
        rule["population_basis"] = json!("legal");
        rule["reference_date"] = json!("2026-01-01");
        assert_eq!(
            result_state(&run(rule.clone(), vec![estimate.clone()])),
            "unknown"
        );
        rule["population_basis"] = json!("resident");
        assert_eq!(
            result_state(&run(rule.clone(), vec![estimate.clone()])),
            "unknown"
        );
        rule["population_basis"] = json!("estimate");
        rule["reference_date"] = json!("2025-01-01");
        assert_eq!(
            result_state(&run(rule.clone(), vec![estimate.clone()])),
            "unknown"
        );
        rule["reference_date"] = json!("2026-01-01");
        assert_eq!(result_state(&run(rule, vec![estimate])), "pass");
    }

    #[test]
    fn legal_population_needs_reference_date_and_matching_explicit_evidence() {
        let mut rule = rule("range", Some("population"));
        rule["max"] = json!(50_000);
        rule["population_basis"] = json!("legal");
        let mut record = record("population", json!(25_000));
        record["populationBasis"] = json!("legal");
        assert_eq!(
            result_state(&run(rule.clone(), vec![record.clone()])),
            "review_required"
        );
        rule["reference_date"] = json!("2026-10-01");
        assert_eq!(result_state(&run(rule, vec![record])), "pass");
    }

    #[test]
    fn reference_dates_do_not_cross_contaminate_population_values() {
        let mut rule = rule("range", Some("population"));
        rule["max"] = json!(50_000);
        rule["population_basis"] = json!("estimate");
        rule["reference_date"] = json!("2026-01-01");
        let mut first = record("population", json!(25_000));
        first["quality"] = json!("estimate");
        first["observedAt"] = json!("2026-01-01");
        let mut second = first.clone();
        second["observedAt"] = json!("2025-01-01");
        second["value"] = json!(30_000);
        assert_eq!(result_state(&run(rule, vec![first, second])), "pass");
    }

    #[test]
    fn closed_and_failed_results_are_hidden_unknown_remains_visible() {
        let bad = run(equality("entity.region", json!("Lazio")), vec![]);
        assert_eq!(bad["state"], "excluded");
        assert_eq!(bad["visible"], false);
        let failed = run(
            equality("project.publicOwnership", json!(true)),
            vec![record("project.publicOwnership", json!(false))],
        );
        assert_eq!(failed["state"], "ineligible");
        assert_eq!(failed["visible"], false);
        let unknown = run(equality("project.publicOwnership", json!(true)), vec![]);
        assert_eq!(unknown["state"], "review_required");
        assert_eq!(unknown["visible"], true);
        let mut closed = extraction(equality("entity.kind", json!("municipality")));
        closed["status"] = json!("closed");
        let output = evaluate(&closed, &municipality(), &[], None, "2026-10-06");
        assert_eq!(output["state"], "closed");
        assert_eq!(output["visible"], false);
    }

    #[test]
    fn deadlines_respect_timezone_and_exact_as_of() {
        let mut call = extraction(equality("entity.kind", json!("municipality")));
        call["closes_at"] = json!("2026-10-06T12:00:00+02:00");
        let open = evaluate(&call, &municipality(), &[], None, "2026-10-06T09:59:59Z");
        assert_eq!(open["state"], "screening_match");
        let closed = evaluate(&call, &municipality(), &[], None, "2026-10-06T10:00:01Z");
        assert_eq!(closed["state"], "closed");
        let uncertain = evaluate(&call, &municipality(), &[], None, "2026-10-06");
        assert_eq!(uncertain["state"], "review_required");
        assert_eq!(uncertain["window"]["state"], "unknown");
    }

    #[test]
    fn forthcoming_and_model_uncertainty_never_become_screening_match() {
        let mut call = extraction(equality("entity.kind", json!("municipality")));
        call["opens_on"] = json!("2026-11-01");
        assert_eq!(
            evaluate(&call, &municipality(), &[], None, "2026-10-06")["state"],
            "forthcoming"
        );
        call["opens_on"] = Value::Null;
        call["needs_review"] = json!(true);
        call["review_reasons"] = json!(["Attachment unreadable"]);
        assert_eq!(
            evaluate(&call, &municipality(), &[], None, "2026-10-06")["state"],
            "review_required"
        );
    }

    #[test]
    fn invalid_inputs_fail_closed_without_panicking() {
        for invalid in [
            Value::Null,
            json!({}),
            json!({"requirements":[{"op":"eval"}]}),
        ] {
            let output = evaluate(&invalid, &municipality(), &[], None, "2026-10-06");
            assert_eq!(output["state"], "invalid_extraction");
            assert_eq!(output["visible"], true);
            assert_eq!(output["needs_review"], true);
        }
        let call = extraction(equality("entity.kind", json!("municipality")));
        assert_eq!(
            evaluate(&call, &municipality(), &[], None, "invalid")["state"],
            "invalid_context"
        );
        assert_eq!(
            evaluate(&call, &json!({}), &[], None, "2026-10-06")["state"],
            "invalid_context"
        );
    }

    #[test]
    fn uncertain_applicant_exclusion_remains_visible_for_review() {
        let mut call = extraction(equality("entity.region", json!("Lazio")));
        call["needs_review"] = json!(true);
        call["review_reasons"] = json!(["Eligibility annex is unreadable"]);
        let result = evaluate(&call, &municipality(), &[], None, "2026-10-06");
        assert_eq!(result["results"][0]["state"], "fail");
        assert_eq!(result["state"], "review_required");
        assert_eq!(result["visible"], true);
        assert_eq!(result["needs_review"], true);
    }

    #[test]
    fn uncertain_elapsed_deadline_remains_visible_for_review() {
        let mut call = extraction(equality("entity.kind", json!("municipality")));
        call["needs_review"] = json!(true);
        call["review_reasons"] = json!(["Deadline is ambiguous in the source"]);
        call["closes_at"] = json!("2026-10-01T12:00:00+02:00");
        let result = evaluate(&call, &municipality(), &[], None, "2026-10-06");
        assert_eq!(result["window"]["state"], "closed");
        assert_eq!(result["state"], "review_required");
        assert_eq!(result["visible"], true);
    }

    #[test]
    fn an_open_call_without_deadline_requires_review_and_never_matches_later() {
        let mut call = extraction(equality("entity.kind", json!("municipality")));
        call["closes_at"] = Value::Null;
        assert!(validate_extraction(&call, &sources()).is_err());
        for as_of in ["2026-10-06", "2036-10-06"] {
            let result = evaluate(&call, &municipality(), &[], None, as_of);
            assert_eq!(result["window"]["state"], "unknown");
            assert_eq!(result["state"], "review_required");
            assert_eq!(result["visible"], true);
            assert_eq!(result["needs_review"], true);
            assert!(!result["review_reasons"].as_array().unwrap().is_empty());
        }
        call["needs_review"] = json!(true);
        call["review_reasons"] = json!(["The official source does not establish a deadline"]);
        assert!(validate_extraction(&call, &sources()).is_ok());
        call["requirements"][0]["value"] = json!("private_company");
        assert_eq!(
            evaluate(&call, &municipality(), &[], None, "2026-10-06")["visible"],
            true
        );
    }

    #[test]
    fn missing_deadline_does_not_bypass_other_validation() {
        let mut call = extraction(equality("entity.kind", json!("municipality")));
        call["closes_at"] = Value::Null;
        call["requirements"][0]["op"] = json!("eval");
        assert_eq!(
            evaluate(&call, &municipality(), &[], None, "2026-10-06")["state"],
            "invalid_extraction"
        );
    }
}
