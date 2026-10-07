use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRef {
    pub url: String,
    pub filename: String,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub id: String,
    pub title: String,
    pub source_url: String,
    #[serde(default)]
    pub source_text: Option<String>,
    #[serde(default)]
    pub documents: Vec<DocumentRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentVersion {
    pub url: String,
    pub filename: String,
    pub mime_type: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub generation_id: Option<String>,
    pub provider: Option<String>,
}
impl Usage {
    pub fn from_response(response: &Value) -> Self {
        let usage = &response["usage"];
        Self {
            prompt_tokens: usage["prompt_tokens"].as_u64(),
            completion_tokens: usage["completion_tokens"].as_u64(),
            reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"].as_u64(),
            cost_usd: usage["cost"]
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0),
            generation_id: response["id"].as_str().map(str::to_owned),
            provider: response["provider"].as_str().map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunSummary {
    pub discovered: usize,
    pub processed: usize,
    pub unchanged: usize,
    pub calls: usize,
    pub extracted: usize,
    pub needs_review: usize,
    pub failed: usize,
    pub prompt_tokens_reported: u64,
    pub completion_tokens_reported: u64,
    pub reasoning_tokens_reported: u64,
    pub reported_cost_usd: f64,
    pub calls_with_unknown_cost: usize,
    pub average_cost_per_called_notice_usd: Option<f64>,
    pub average_cost_per_processed_notice_usd: Option<f64>,
    pub cost_note: String,
}
impl RunSummary {
    pub fn add_usage(&mut self, usage: &Usage) {
        self.prompt_tokens_reported += usage.prompt_tokens.unwrap_or(0);
        self.completion_tokens_reported += usage.completion_tokens.unwrap_or(0);
        self.reasoning_tokens_reported += usage.reasoning_tokens.unwrap_or(0);
        if let Some(cost) = usage.cost_usd {
            self.reported_cost_usd += cost;
        } else {
            self.calls_with_unknown_cost += 1;
        }
    }
    pub fn finish(&mut self) {
        if self.calls > 0 && self.calls_with_unknown_cost == 0 {
            self.average_cost_per_called_notice_usd =
                Some(self.reported_cost_usd / self.calls as f64);
            self.average_cost_per_processed_notice_usd =
                Some(self.reported_cost_usd / self.processed as f64);
        }
        self.cost_note = if self.calls == 0 { "No model calls made; averages unavailable." }
            else if self.calls_with_unknown_cost > 0 { "Reported cost is a partial subtotal. Missing cost is unknown, not zero; averages unavailable. Reasoning is included in completion tokens." }
            else { "Actual usage.cost returned by provider; not a price-table estimate. Reasoning is included in completion tokens." }.into();
    }
}
