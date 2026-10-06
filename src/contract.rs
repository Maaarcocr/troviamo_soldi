//! Compact model-facing facts. The evaluator's legacy rule representation stays private.
use crate::engine;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const TOP_KEYS: &[&str] = &[
    "schema_version",
    "title",
    "summary",
    "status",
    "opens_on",
    "closes_at",
    "review_reasons",
    "requirements",
    "citations",
];
const COMMON: &[&str] = &["id", "label", "op", "citation_ids"];

fn arguments(op: &str) -> Result<&'static [&'static str]> {
    Ok(match op {
        "equals" => &["field", "value"],
        "one_of" => &["field", "values"],
        "range" => &["field", "min", "max"],
        "compare" => &["left", "right", "relation"],
        "min_days" => &["field", "days"],
        "manual" => &["note"],
        _ => anyhow::bail!("Unsupported operator: {op}"),
    })
}
fn exact_keys(value: &Value, keys: &[&str], name: &str) -> Result<()> {
    let object = value
        .as_object()
        .with_context(|| format!("{name} must be an object"))?;
    ensure!(
        object.len() == keys.len() && keys.iter().all(|k| object.contains_key(*k)),
        "{name} has missing or unknown fields; expected {}",
        keys.join(", ")
    );
    Ok(())
}

/// Translate only a structurally complete v2 extraction; never repair or infer source facts.
/// Scope and exclusion level come from the trusted field catalogue, never the model.
pub fn to_legacy(value: &Value) -> Result<Value> {
    exact_keys(value, TOP_KEYS, "Extraction v2")?;
    ensure!(value["schema_version"] == 2, "Unsupported schema_version");
    let rules = value["requirements"]
        .as_array()
        .context("requirements must be an array")?;
    let mut expanded = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        let expand = || -> Result<Value> {
            let op = rule["op"].as_str().context("op must be a string")?;
            let mut keys = COMMON.to_vec();
            keys.extend(arguments(op)?);
            if rule["field"] == "population" {
                keys.extend(["population_basis", "reference_date"]);
            }
            exact_keys(rule, &keys, "Requirement")?;
            let scope = if op == "compare" {
                let mut scope = "municipality";
                for side in ["left", "right"] {
                    for term in rule[side]
                        .as_array()
                        .context("Comparison side must be an array")?
                    {
                        let field = term["field"]
                            .as_str()
                            .context("Comparison field must be a string")?;
                        ensure!(
                            field != "population",
                            "Population comparisons require a manual condition"
                        );
                        let current = engine::fields()[field]["scope"]
                            .as_str()
                            .context("Unknown comparison field")?;
                        if current == "application"
                            || (current == "project" && scope == "municipality")
                        {
                            scope = current;
                        }
                    }
                }
                scope
            } else if op == "manual" {
                "application"
            } else {
                let field = rule["field"].as_str().context("field must be a string")?;
                engine::fields()[field]["scope"]
                    .as_str()
                    .context("Unknown field")?
            };
            let mut out = json!({"id":null,"label":null,"op":null,"field":null,"scope":scope,
                "blocker":if scope == "municipality" { "applicant" } else { scope },
                "value":null,"values":null,"min":null,"max":null,"left":null,"right":null,
                "relation":null,"days":null,"citation_ids":null,"note":null,
                "population_basis":null,"reference_date":null});
            for key in keys {
                out[key] = rule[key].clone();
            }
            Ok(out)
        };
        expanded
            .push(expand().with_context(|| format!("Requirement {} ({})", index + 1, rule["id"]))?);
    }
    let reasons = value["review_reasons"]
        .as_array()
        .context("review_reasons must be an array")?;
    let mut out = value.clone();
    out.as_object_mut().unwrap().remove("schema_version");
    out["needs_review"] = json!(!reasons.is_empty());
    out["requirements"] = json!(expanded);
    Ok(out)
}

fn object(properties: Value) -> Value {
    let required: Vec<_> = properties.as_object().unwrap().keys().cloned().collect();
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn string(min: usize, max: usize) -> Value {
    json!({"type":"string","minLength":min,"maxLength":max})
}
fn identifier_schema() -> Value {
    let mut schema = string(1, 160);
    schema["pattern"] = json!("^[A-Za-z0-9_.:-]+$");
    schema
}
fn number(nullable: bool) -> Value {
    json!({"type":if nullable {json!(["number","null"])} else {json!("number")},"minimum":0,"maximum":1e15})
}
fn rule_schema(op: &str, args: Value) -> Value {
    // arguments() is shared with the read adapter: omitted/unused arguments cannot leak in.
    debug_assert!(
        arguments(op)
            .unwrap()
            .iter()
            .all(|key| args.get(*key).is_some())
    );
    let mut properties = json!({"id":identifier_schema(),"label":string(1,1000),
        "op":{"type":"string","enum":[op]},
        "citation_ids":{"type":"array","items":identifier_schema(),"minItems":1,"maxItems":30}});
    properties
        .as_object_mut()
        .unwrap()
        .extend(args.as_object().unwrap().clone());
    object(properties)
}
fn value_schema(def: &Value) -> Value {
    if let Some(values) = def.get("canonical_values") {
        return json!({"type":"string","enum":values});
    }
    match def["type"].as_str().unwrap() {
        "boolean" => json!({"type":"boolean"}),
        "number" if def["integer"] == true => json!({"type":"integer","minimum":0,"maximum":1e15}),
        "number" => number(false),
        "enum" => {
            json!({"type":"string","enum":def["options"].as_array().unwrap().iter().map(|v|v["value"].clone()).collect::<Vec<_>>()})
        }
        "date" => json!({"type":"string","description":"An actual YYYY-MM-DD calendar date."}),
        _ => string(1, 3000),
    }
}

pub fn schema() -> Value {
    // Group identical value types, keeping the field and value coupled in the schema.
    let mut groups = std::collections::BTreeMap::<String, Vec<String>>::new();
    for (field, def) in engine::fields().as_object().unwrap() {
        if field != "population" {
            groups
                .entry(value_schema(def).to_string())
                .or_default()
                .push(field.clone());
        }
    }
    let mut variants = Vec::new();
    for (value, names) in groups {
        let value: Value = serde_json::from_str(&value).unwrap();
        for op in ["equals", "one_of"] {
            let mut args = json!({"field":{"type":"string","enum":names}});
            if op == "equals" {
                args["value"] = value.clone();
            } else {
                args["values"] = json!({"type":"array","items":value,"minItems":1,"maxItems":100});
            }
            variants.push(rule_schema(op, args));
        }
    }
    let numeric: Vec<_> = engine::fields()
        .as_object()
        .unwrap()
        .iter()
        .filter(|(name, def)| name.as_str() != "population" && def["type"] == "number")
        .map(|(name, _)| name.clone())
        .collect();
    let population = json!({"field":{"type":"string","enum":["population"]},
        "population_basis":{"type":["string","null"],"enum":["estimate","resident","legal",null]},
        "reference_date":{"type":["string","null"],"description":"Exact source-required YYYY-MM-DD reference date; null if unknown."}});
    for op in ["equals", "one_of"] {
        let mut args = population.clone();
        let value = value_schema(&engine::fields()["population"]);
        if op == "equals" {
            args["value"] = value;
        } else {
            args["values"] = json!({"type":"array","items":value,"minItems":1,"maxItems":100});
        }
        variants.push(rule_schema(op, args));
    }
    for fields in [
        json!({"field":{"type":"string","enum":numeric}}),
        population,
    ] {
        // At least one real range bound is guaranteed by these two shapes.
        for required_bound in ["min", "max"] {
            let mut args = fields.clone();
            args["min"] = number(required_bound != "min");
            args["max"] = number(required_bound != "max");
            variants.push(rule_schema("range", args));
        }
    }
    let term = object(
        json!({"field":{"type":"string","enum":numeric},"factor":{"type":"number","exclusiveMinimum":0,"maximum":1e6}}),
    );
    let terms = json!({"type":"array","items":term,"minItems":1,"maxItems":20});
    variants.push(rule_schema(
        "compare",
        json!({"left":terms,"right":terms,"relation":{"type":"string","enum":["lte","gte"]}}),
    ));
    let dates: Vec<_> = engine::fields()
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, def)| def["type"] == "date")
        .map(|(name, _)| name.clone())
        .collect();
    variants.push(rule_schema("min_days",json!({"field":{"type":"string","enum":dates},"days":{"type":"integer","minimum":0,"maximum":36600}})));
    variants.push(rule_schema("manual", json!({"note":string(1,5000)})));
    let citation = object(json!({"id":identifier_schema(),
        "source_url":{"type":"string","description":"Exactly one supplied current source URL."},
        "locator":string(1,1000),"quote":string(8,10000)}));
    object(json!({"schema_version":{"type":"integer","enum":[2]},
        "title":string(1,500),"summary":string(1,10000),
        "status":{"type":"string","enum":["open","forthcoming","closed","unknown"]},
        "opens_on":{"type":["string","null"],"description":"YYYY-MM-DD; null if unknown."},
        "closes_at":{"type":["string","null"],"description":"Application deadline as RFC3339 with explicit timezone; null if unknown. Never invent time/timezone."},
        "review_reasons":{"type":"array","items":string(1,3000),"maxItems":100,"description":"Source/eligibility uncertainty that could invalidate otherwise definite conditions: missing coverage, unreadable annexes, conflicting terms or alternative eligibility routes. Unknown status/deadline or an unrelated manual project condition do not belong here; they are already represented in their own fields."},
        "requirements":{"type":"array","items":{"anyOf":variants},"maxItems":200},
        "citations":{"type":"array","items":citation,"minItems":1,"maxItems":300}}))
}
