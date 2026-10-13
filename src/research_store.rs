//! Durable, source-grounded research. All writes are narrow typed operations;
//! this module never exposes SQL to the researcher or modifies legacy tables.
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub municipality_id: String,
    #[serde(default)]
    pub entity_id: Option<String>,
    #[serde(default)]
    pub site_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}
impl Scope {
    pub fn municipality(id: &str) -> Self {
        Self {
            municipality_id: id.into(),
            entity_id: None,
            site_id: None,
            project_id: None,
        }
    }
    fn validate(&self) -> Result<()> {
        nonblank(&self.municipality_id, "municipality_id")?;
        for (name, value) in [
            ("entity_id", &self.entity_id),
            ("site_id", &self.site_id),
            ("project_id", &self.project_id),
        ] {
            if let Some(value) = value {
                nonblank(value, name)?;
            }
        }
        Ok(())
    }
    fn key(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
    /// Facts keep the same municipality and actual applicant. A fact without
    /// site/project restrictions can be reused by that applicant's narrower
    /// assessments; a particular site or project never leaks into another one.
    fn supports(&self, target: &Self) -> bool {
        self.municipality_id == target.municipality_id
            && self.entity_id == target.entity_id
            && self
                .site_id
                .as_ref()
                .is_none_or(|v| target.site_id.as_ref() == Some(v))
            && self
                .project_id
                .as_ref()
                .is_none_or(|v| target.project_id.as_ref() == Some(v))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceVersionInput {
    pub source_id: String,
    pub url: String,
    pub title: String,
    pub body: String,
    pub retrieved_at: String,
    #[serde(default)]
    pub artifact: Option<SourceArtifact>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceArtifact {
    pub path: String,
    pub sha256: String,
    pub mime_type: String,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub final_url: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceVersion {
    pub id: i64,
    pub fingerprint: String,
    /// A fetched URL alone does not establish official authority.
    pub origin_trust: String,
    #[serde(flatten)]
    pub source: SourceVersionInput,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRelation {
    Primary,
    Annex,
    Amendment,
    RelatedOnly,
}
impl EvidenceRelation {
    fn decisive(self) -> bool {
        self != Self::RelatedOnly
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceInput {
    pub source_version_id: i64,
    #[serde(default)]
    pub bando_id: Option<String>,
    #[serde(default)]
    pub scope: Option<Scope>,
    pub relation: EvidenceRelation,
    pub quote: String,
    #[serde(default)]
    pub locator: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub id: i64,
    #[serde(flatten)]
    pub evidence: EvidenceInput,
    /// Native-original quotations retain a locator but are not represented as
    /// independently text-matched when no readable source text exists.
    pub quote_verification: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactInput {
    pub scope: Scope,
    /// Open vocabulary: new grant-specific requirements need no migration.
    pub key: String,
    pub value: Value,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_until: Option<String>,
    #[serde(default)]
    pub evidence_ids: Vec<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: i64,
    #[serde(flatten)]
    pub fact: FactInput,
    pub is_stale: bool,
    pub is_valid_on_date: bool,
}
/// Precision is retained. A day-only deadline never invents a midnight cutoff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "precision", rename_all = "snake_case", deny_unknown_fields)]
pub enum Deadline {
    Date { value: String },
    DateTime { value: String },
}
impl Deadline {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Date { value } => {
                date(value)?;
            }
            Self::DateTime { value } => {
                DateTime::parse_from_rfc3339(value)
                    .context("deadline needs an explicit timezone")?;
            }
        }
        Ok(())
    }
    pub fn application_status(&self, as_of: &str) -> Result<&'static str> {
        let day = as_of_date(as_of)?;
        match self {
            Self::Date { value } => Ok(if day > date(value)? { "closed" } else { "open" }),
            Self::DateTime { value } => {
                let deadline = DateTime::parse_from_rfc3339(value)?;
                if let Ok(now) = DateTime::parse_from_rfc3339(as_of) {
                    Ok(if now > deadline { "closed" } else { "open" })
                } else if day > deadline.date_naive() {
                    Ok("closed")
                } else if day < deadline.date_naive() {
                    Ok("open")
                } else {
                    Ok("unknown")
                }
            }
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BandoStatus {
    Open,
    Forthcoming,
    Closed,
    #[default]
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bando {
    pub id: String,
    pub title: String,
    pub authority: String,
    #[serde(default)]
    pub status: BandoStatus,
    #[serde(default)]
    pub deadline: Option<Deadline>,
    #[serde(default)]
    pub amount: Option<Value>,
    #[serde(default)]
    pub evidence_ids: Vec<i64>,
    #[serde(default)]
    pub details: Value,
}
impl Bando {
    pub fn application_status(&self, as_of: &str) -> Result<&'static str> {
        as_of_date(as_of)?;
        let deadline = self
            .deadline
            .as_ref()
            .map(|d| d.application_status(as_of))
            .transpose()?;
        if self.status == BandoStatus::Closed || deadline == Some("closed") {
            return Ok("closed");
        }
        Ok(match self.status {
            BandoStatus::Closed => "closed",
            BandoStatus::Forthcoming => "forthcoming",
            BandoStatus::Open => {
                if deadline == Some("unknown") {
                    "unknown"
                } else {
                    "open"
                }
            }
            // A future deadline alone does not prove that applications opened.
            BandoStatus::Unknown => "unknown",
        })
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Eligibility {
    Eligible,
    Ineligible,
    Conditional,
    Unknown,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectReadiness {
    Ready,
    NeedsWork,
    Unknown,
    NotAssessed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Dependency {
    /// Record missing facts too, so resolving them invalidates the old result.
    Fact {
        scope: Scope,
        key: String,
        #[serde(default)]
        affects: DependencyEffect,
    },
    Source {
        source_id: String,
    },
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyEffect {
    Applicant,
    Project,
    #[default]
    Context,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentInput {
    pub bando_id: String,
    pub scope: Scope,
    pub as_of: String,
    pub eligibility: Eligibility,
    pub project_readiness: ProjectReadiness,
    pub summary: String,
    /// Why this call is relevant to this municipality and actual applicant.
    #[serde(default)]
    pub why_fit: String,
    /// Suggested keys: status, deadline {precision,value}, summary, evidence_ids.
    #[serde(default)]
    pub accreditation: Value,
    #[serde(default)]
    pub amount: Value,
    #[serde(default)]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub unknowns: Vec<String>,
    #[serde(default)]
    pub evidence_ids: Vec<i64>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assessment {
    pub id: i64,
    #[serde(flatten)]
    pub assessment: AssessmentInput,
    pub is_stale: bool,
    pub invalidated_reason: Option<String>,
    pub saved_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResearchOperation {
    PutSource {
        source: SourceVersionInput,
    },
    AddEvidence {
        evidence: EvidenceInput,
    },
    PutFact {
        fact: FactInput,
    },
    PutBando {
        bando: Bando,
    },
    SaveAssessment {
        assessment: AssessmentInput,
    },
    GetFacts {
        scope: Scope,
        as_of: String,
    },
    ListFacts {
        #[serde(default)]
        municipality_id: Option<String>,
        #[serde(default)]
        key: Option<String>,
        as_of: String,
    },
    ListFactKeys {
        #[serde(default)]
        municipality_id: Option<String>,
    },
    GetSource {
        source_id: String,
    },
    GetEvidence {
        id: i64,
    },
    GetBando {
        id: String,
    },
    ListBandi {},
    GetAssessment {
        id: i64,
    },
    ListAssessments {
        municipality_id: String,
        as_of: String,
        #[serde(default)]
        include_archive: bool,
    },
}

pub struct ResearchStore {
    pub conn: Connection,
}
impl ResearchStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }
    /// Does not create a database, initialize research tables or alter legacy data.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        ensure!(path.is_file(), "Research database must already exist");
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA query_only=ON;")?;
        Ok(Self { conn })
    }
    pub fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS research_sources(id TEXT PRIMARY KEY,current_version INTEGER);
CREATE TABLE IF NOT EXISTS research_source_versions(id INTEGER PRIMARY KEY,source_id TEXT NOT NULL REFERENCES research_sources(id),fingerprint TEXT NOT NULL,data TEXT NOT NULL,UNIQUE(source_id,fingerprint));
CREATE TABLE IF NOT EXISTS research_evidence(id INTEGER PRIMARY KEY,source_version_id INTEGER NOT NULL REFERENCES research_source_versions(id),fingerprint TEXT NOT NULL UNIQUE,data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS research_fact_versions(id INTEGER PRIMARY KEY,scope_key TEXT NOT NULL,fact_key TEXT NOT NULL,data TEXT NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS research_facts(scope_key TEXT NOT NULL,fact_key TEXT NOT NULL,current_version INTEGER NOT NULL REFERENCES research_fact_versions(id),PRIMARY KEY(scope_key,fact_key));
CREATE TABLE IF NOT EXISTS research_bandi(id TEXT PRIMARY KEY,data TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS research_assessments(id INTEGER PRIMARY KEY,bando_id TEXT NOT NULL REFERENCES research_bandi(id),municipality_id TEXT NOT NULL,scope_key TEXT NOT NULL,data TEXT NOT NULL,is_stale INTEGER NOT NULL DEFAULT 0,invalidated_reason TEXT,saved_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS research_dependencies(assessment_id INTEGER NOT NULL REFERENCES research_assessments(id),kind TEXT NOT NULL,dependency_key TEXT NOT NULL,scope_key TEXT NOT NULL DEFAULT '',version_id INTEGER,PRIMARY KEY(assessment_id,kind,dependency_key,scope_key));
CREATE INDEX IF NOT EXISTS research_dependency_lookup ON research_dependencies(kind,dependency_key,scope_key);
CREATE INDEX IF NOT EXISTS research_assessment_lookup ON research_assessments(municipality_id,bando_id,scope_key,id);
CREATE TABLE IF NOT EXISTS research_jobs(id TEXT PRIMARY KEY,state TEXT NOT NULL,status TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS research_events(id INTEGER PRIMARY KEY,job_id TEXT NOT NULL REFERENCES research_jobs(id),kind TEXT NOT NULL,data TEXT NOT NULL,cost REAL,created_at TEXT NOT NULL);")?;
        Ok(())
    }
    pub fn execute(&self, operation: ResearchOperation) -> Result<Value> {
        Ok(match operation {
            ResearchOperation::PutSource { source } => {
                serde_json::to_value(self.put_source(source)?)?
            }
            ResearchOperation::AddEvidence { evidence } => {
                serde_json::to_value(self.add_evidence(evidence)?)?
            }
            ResearchOperation::PutFact { fact } => serde_json::to_value(self.put_fact(fact)?)?,
            ResearchOperation::PutBando { bando } => {
                self.put_bando(&bando)?;
                serde_json::to_value(bando)?
            }
            ResearchOperation::SaveAssessment { assessment } => {
                serde_json::to_value(self.save_assessment(assessment)?)?
            }
            ResearchOperation::GetFacts { scope, as_of } => {
                serde_json::to_value(self.facts(&scope, &as_of)?)?
            }
            ResearchOperation::ListFacts {
                municipality_id,
                key,
                as_of,
            } => serde_json::to_value(self.list_facts(
                municipality_id.as_deref(),
                key.as_deref(),
                &as_of,
            )?)?,
            ResearchOperation::ListFactKeys { municipality_id } => {
                self.fact_keys(municipality_id.as_deref())?
            }
            ResearchOperation::GetSource { source_id } => {
                serde_json::to_value(self.source(&source_id)?)?
            }
            ResearchOperation::GetEvidence { id } => serde_json::to_value(self.evidence(id)?)?,
            ResearchOperation::GetBando { id } => serde_json::to_value(self.bando(&id)?)?,
            ResearchOperation::ListBandi {} => serde_json::to_value(self.bandi()?)?,
            ResearchOperation::GetAssessment { id } => serde_json::to_value(self.assessment(id)?)?,
            ResearchOperation::ListAssessments {
                municipality_id,
                as_of,
                include_archive,
            } => serde_json::to_value(self.opportunities(
                &municipality_id,
                &as_of,
                include_archive,
            )?)?,
        })
    }
    pub fn put_source(&self, source: SourceVersionInput) -> Result<SourceVersion> {
        nonblank(&source.source_id, "source_id")?;
        nonblank(&source.title, "title")?;
        ensure!(
            !source.body.trim().is_empty() || source.artifact.is_some(),
            "A source needs readable text or an original artifact"
        );
        if let Some(artifact) = &source.artifact {
            nonblank(&artifact.path, "artifact path")?;
            nonblank(&artifact.mime_type, "artifact mime_type")?;
            ensure!(
                artifact.sha256.len() == 64
                    && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "Artifact sha256 must have 64 hexadecimal characters"
            );
            let bytes = std::fs::read(&artifact.path).context("Read archived source artifact")?;
            ensure!(
                format!("{:x}", Sha256::digest(&bytes)).eq_ignore_ascii_case(&artifact.sha256),
                "Archived artifact does not match its declared hash"
            );
        }
        let url = url::Url::parse(&source.url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                || (url.scheme() == "workspace" && source.artifact.is_some()),
            "Source URL must be HTTP(S), or an archived workspace original"
        );
        DateTime::parse_from_rfc3339(&source.retrieved_at)
            .context("retrieved_at must be RFC3339")?;
        let fingerprint = digest(
            &json!({"url":source.url,"title":source.title,"body":source.body,"artifact_sha256":source.artifact.as_ref().map(|a| &a.sha256)}),
        )?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO research_sources(id) VALUES(?1)",
            [&source.source_id],
        )?;
        let old: Option<i64> = tx.query_row(
            "SELECT current_version FROM research_sources WHERE id=?1",
            [&source.source_id],
            |r| r.get(0),
        )?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM research_source_versions WHERE source_id=?1 AND fingerprint=?2",
                params![source.source_id, fingerprint],
                |r| r.get(0),
            )
            .optional()?;
        let id = if let Some(id) = existing {
            id
        } else {
            tx.execute(
                "INSERT INTO research_source_versions(source_id,fingerprint,data) VALUES(?1,?2,?3)",
                params![
                    source.source_id,
                    fingerprint,
                    serde_json::to_string(&source)?
                ],
            )?;
            tx.last_insert_rowid()
        };
        if old != Some(id) {
            tx.execute(
                "UPDATE research_sources SET current_version=?1 WHERE id=?2",
                params![id, source.source_id],
            )?;
            invalidate(&tx, "source", &source.source_id, None, "source changed")?;
        }
        tx.commit()?;
        self.source_version(id)?
            .context("Saved source version is missing")
    }
    pub fn source(&self, source_id: &str) -> Result<Option<SourceVersion>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT current_version FROM research_sources WHERE id=?1",
                [source_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        id.map(|id| self.source_version(id))
            .transpose()
            .map(Option::flatten)
    }
    pub fn source_version(&self, id: i64) -> Result<Option<SourceVersion>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT fingerprint,data FROM research_source_versions WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(fingerprint, data)| {
            let source: SourceVersionInput = serde_json::from_str(&data)?;
            let origin_trust = if source.url.starts_with("workspace:") {
                "workspace_unverified"
            } else {
                "public_url_unverified_authority"
            }
            .to_string();
            Ok(SourceVersion {
                id,
                fingerprint,
                origin_trust,
                source,
            })
        })
        .transpose()
    }
    pub fn source_version_for_artifact(
        &self,
        source_id: &str,
        sha256: &str,
    ) -> Result<Option<SourceVersion>> {
        let id: Option<i64> = self.conn.query_row("SELECT id FROM research_source_versions WHERE source_id=?1 AND lower(json_extract(data,'$.artifact.sha256'))=lower(?2) ORDER BY id DESC LIMIT 1",params![source_id,sha256],|r| r.get(0)).optional()?;
        id.map(|id| self.source_version(id))
            .transpose()
            .map(Option::flatten)
    }
    pub fn add_evidence(&self, evidence: EvidenceInput) -> Result<Evidence> {
        if let Some(scope) = &evidence.scope {
            scope.validate()?;
        }
        nonblank(&evidence.quote, "quote")?;
        if let Some(id) = &evidence.bando_id {
            nonblank(id, "bando_id")?;
        }
        if let Some(locator) = &evidence.locator {
            nonblank(locator, "locator")?;
        }
        let source = self
            .source_version(evidence.source_version_id)?
            .context("Unknown source version")?;
        let text_match = source.source.body.contains(&evidence.quote);
        let native_original = source.source.body.trim().is_empty()
            && source.source.artifact.as_ref().is_some_and(|a| {
                a.mime_type == "application/pdf"
                    || matches!(
                        a.mime_type.as_str(),
                        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                    )
            });
        ensure!(
            text_match || native_original,
            "Quotation must occur verbatim in the saved source body; only unreadable native PDF/image originals permit locator-based interpretation"
        );
        if !text_match {
            ensure!(
                evidence.locator.is_some(),
                "Quotations from original artifacts need a page or other precise locator"
            );
        }
        let quote_verification = if text_match {
            "text_match"
        } else {
            "original_not_text_verified"
        }
        .to_string();
        let fingerprint = digest(&evidence)?;
        self.conn.execute("INSERT OR IGNORE INTO research_evidence(source_version_id,fingerprint,data) VALUES(?1,?2,?3)", params![evidence.source_version_id,fingerprint,serde_json::to_string(&evidence)?])?;
        let id = self.conn.query_row(
            "SELECT id FROM research_evidence WHERE fingerprint=?1",
            [&fingerprint],
            |r| r.get(0),
        )?;
        Ok(Evidence {
            id,
            evidence,
            quote_verification,
        })
    }
    pub fn evidence(&self, id: i64) -> Result<Option<Evidence>> {
        let Some(evidence) = self
            .read_json::<EvidenceInput>("SELECT data FROM research_evidence WHERE id=?1", [id])?
        else {
            return Ok(None);
        };
        let source = self
            .source_version(evidence.source_version_id)?
            .context("Evidence source is missing")?;
        let quote_verification = if source.source.body.contains(&evidence.quote) {
            "text_match"
        } else {
            "original_not_text_verified"
        }
        .to_string();
        Ok(Some(Evidence {
            id,
            evidence,
            quote_verification,
        }))
    }
    pub fn put_fact(&self, fact: FactInput) -> Result<Fact> {
        fact.scope.validate()?;
        nonblank(&fact.key, "fact key")?;
        validity(&fact.valid_from, &fact.valid_until)?;
        // Null records are useful as unresolved knowledge, but cannot be decisive.
        ensure!(
            fact.value.is_null() || !fact.evidence_ids.is_empty(),
            "A non-null fact requires source evidence"
        );
        for id in &fact.evidence_ids {
            let evidence = self.evidence(*id)?.context("Unknown fact evidence")?;
            ensure!(
                evidence.evidence.scope.as_ref() == Some(&fact.scope),
                "A fact requires evidence about that exact municipality/entity/site/project scope"
            );
            ensure!(
                self.evidence_current(&evidence)?,
                "Cannot save a fact using a superseded source version"
            );
        }
        let scope = fact.scope.key()?;
        let raw = serde_json::to_string(&fact)?;
        let tx = self.conn.unchecked_transaction()?;
        let old: Option<(i64,String)> = tx.query_row("SELECT v.id,v.data FROM research_facts f JOIN research_fact_versions v ON v.id=f.current_version WHERE f.scope_key=?1 AND f.fact_key=?2", params![scope,fact.key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let id = if let Some((id, data)) = &old {
            if data == &raw { *id } else { 0 }
        } else {
            0
        };
        let id = if id != 0 {
            id
        } else {
            tx.execute("INSERT INTO research_fact_versions(scope_key,fact_key,data,created_at) VALUES(?1,?2,?3,?4)", params![scope,fact.key,raw,Utc::now().to_rfc3339()])?;
            let id = tx.last_insert_rowid();
            tx.execute("INSERT INTO research_facts(scope_key,fact_key,current_version) VALUES(?1,?2,?3) ON CONFLICT(scope_key,fact_key) DO UPDATE SET current_version=excluded.current_version", params![scope,fact.key,id])?;
            invalidate(
                &tx,
                "fact",
                &fact.key,
                Some(&scope),
                "fact changed or resolved",
            )?;
            id
        };
        tx.commit()?;
        self.fact_record(id, &Utc::now().date_naive().to_string())?
            .context("Saved fact is missing")
    }
    /// Exact scope lookup. Callers choose municipal/entity/site/project scope
    /// explicitly rather than accidentally treating a linked institution as town hall.
    pub fn facts(&self, scope: &Scope, as_of: &str) -> Result<Vec<Fact>> {
        scope.validate()?;
        as_of_date(as_of)?;
        let mut stmt = self.conn.prepare(
            "SELECT current_version FROM research_facts WHERE scope_key=?1 ORDER BY fact_key",
        )?;
        let ids = stmt
            .query_map([scope.key()?], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| self.fact_record(id, as_of)?.context("Fact version missing"))
            .collect()
    }
    pub fn list_facts(
        &self,
        municipality_id: Option<&str>,
        key: Option<&str>,
        as_of: &str,
    ) -> Result<Vec<Fact>> {
        as_of_date(as_of)?;
        let mut stmt = self.conn.prepare("SELECT f.current_version FROM research_facts f JOIN research_fact_versions v ON v.id=f.current_version WHERE (?1 IS NULL OR json_extract(v.data,'$.scope.municipality_id')=?1) AND (?2 IS NULL OR instr(f.fact_key,?2)>0) ORDER BY f.fact_key,f.scope_key")?;
        let ids = stmt
            .query_map(params![municipality_id, key], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| self.fact_record(id, as_of)?.context("Fact is missing"))
            .collect()
    }
    pub fn fact_keys(&self, municipality_id: Option<&str>) -> Result<Value> {
        let mut stmt = self.conn.prepare("SELECT f.fact_key,count(*) FROM research_facts f JOIN research_fact_versions v ON v.id=f.current_version WHERE (?1 IS NULL OR json_extract(v.data,'$.scope.municipality_id')=?1) GROUP BY f.fact_key ORDER BY f.fact_key")?;
        let rows = stmt.query_map([municipality_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let result = rows
            .map(|row| {
                let (key, scopes) = row?;
                Ok(json!({"key":key,"scope_count":scopes}))
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!(result))
    }
    fn fact_record(&self, id: i64, as_of: &str) -> Result<Option<Fact>> {
        let Some(fact) = self
            .read_json::<FactInput>("SELECT data FROM research_fact_versions WHERE id=?1", [id])?
        else {
            return Ok(None);
        };
        let day = as_of_date(as_of)?;
        let valid = fact
            .valid_from
            .as_ref()
            .map(|v| date(v).map(|d| day >= d))
            .transpose()?
            .unwrap_or(true)
            && fact
                .valid_until
                .as_ref()
                .map(|v| date(v).map(|d| day <= d))
                .transpose()?
                .unwrap_or(true);
        let mut stale = false;
        for evidence_id in &fact.evidence_ids {
            let evidence = self
                .evidence(*evidence_id)?
                .context("Fact evidence missing")?;
            stale |= !self.evidence_current(&evidence)?;
        }
        Ok(Some(Fact {
            id,
            fact,
            is_stale: stale,
            is_valid_on_date: valid,
        }))
    }
    pub fn put_bando(&self, bando: &Bando) -> Result<()> {
        nonblank(&bando.id, "bando id")?;
        nonblank(&bando.title, "bando title")?;
        nonblank(&bando.authority, "authority")?;
        if let Some(deadline) = &bando.deadline {
            deadline.validate()?;
        }
        ensure!(
            !bando.evidence_ids.is_empty(),
            "A bando requires primary, annex or amendment evidence"
        );
        for id in &bando.evidence_ids {
            self.validate_bando_evidence(*id, &bando.id, None)?;
        }
        let raw = serde_json::to_string(bando)?;
        let tx = self.conn.unchecked_transaction()?;
        let old: Option<String> = tx
            .query_row(
                "SELECT data FROM research_bandi WHERE id=?1",
                [&bando.id],
                |r| r.get(0),
            )
            .optional()?;
        if old.as_ref() != Some(&raw) {
            tx.execute("INSERT INTO research_bandi(id,data,updated_at) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data,updated_at=excluded.updated_at", params![bando.id,raw,Utc::now().to_rfc3339()])?;
            invalidate(&tx, "bando", &bando.id, None, "bando changed")?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn bando(&self, id: &str) -> Result<Option<Bando>> {
        self.read_json("SELECT data FROM research_bandi WHERE id=?1", [id])
    }
    pub fn bandi(&self) -> Result<Vec<Bando>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM research_bandi ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|raw| Ok(serde_json::from_str(&raw?)?)).collect()
    }
    pub fn save_assessment(&self, assessment: AssessmentInput) -> Result<Assessment> {
        assessment.scope.validate()?;
        as_of_date(&assessment.as_of)?;
        nonblank(&assessment.summary, "assessment summary")?;
        let bando = self.bando(&assessment.bando_id)?.context("Unknown bando")?;
        if assessment.eligibility != Eligibility::Unknown {
            ensure!(
                !assessment.evidence_ids.is_empty(),
                "A decisive eligibility conclusion requires bando evidence"
            );
        }
        let mut deps: BTreeMap<(String, String, String), Option<i64>> = BTreeMap::new();
        deps.insert(("bando".into(), bando.id.clone(), String::new()), None);
        for id in bando
            .evidence_ids
            .iter()
            .chain(assessment.evidence_ids.iter())
        {
            let evidence = self.validate_bando_evidence(*id, &bando.id, Some(&assessment.scope))?;
            self.add_source_dependency(&mut deps, &evidence)?;
        }
        let mut unresolved_applicant = false;
        let mut unresolved_project = false;
        for dependency in &assessment.dependencies {
            match dependency {
                Dependency::Fact {
                    scope,
                    key,
                    affects,
                } => {
                    scope.validate()?;
                    nonblank(key, "dependency fact key")?;
                    ensure!(
                        scope.supports(&assessment.scope),
                        "Fact dependencies must keep the same municipality and applicant, with compatible site and project scope"
                    );
                    let scope_key = scope.key()?;
                    let id: Option<i64> = self.conn.query_row("SELECT current_version FROM research_facts WHERE scope_key=?1 AND fact_key=?2", params![scope_key,key], |r| r.get(0)).optional()?;
                    deps.insert(("fact".into(), key.clone(), scope_key), id);
                    if let Some(id) = id {
                        let fact = self
                            .fact_record(id, &assessment.as_of)?
                            .context("Fact is missing")?;
                        let unresolved =
                            fact.fact.value.is_null() || fact.is_stale || !fact.is_valid_on_date;
                        unresolved_applicant |=
                            unresolved && *affects == DependencyEffect::Applicant;
                        unresolved_project |= unresolved && *affects == DependencyEffect::Project;
                        for id in &fact.fact.evidence_ids {
                            self.add_source_dependency(
                                &mut deps,
                                &self.evidence(*id)?.context("Fact evidence is missing")?,
                            )?;
                        }
                    } else {
                        unresolved_applicant |= *affects == DependencyEffect::Applicant;
                        unresolved_project |= *affects == DependencyEffect::Project;
                    }
                }
                Dependency::Source { source_id } => {
                    let source = self
                        .source(source_id)?
                        .context("Unknown dependency source")?;
                    deps.insert(
                        ("source".into(), source_id.clone(), String::new()),
                        Some(source.id),
                    );
                }
            }
        }
        // An interrupted/missing evidence path must never turn into a negative result.
        ensure!(
            !(unresolved_applicant
                && matches!(
                    assessment.eligibility,
                    Eligibility::Eligible | Eligibility::Ineligible
                )),
            "Missing, expired or stale applicant facts require unknown or conditional eligibility"
        );
        ensure!(
            !(unresolved_project && assessment.project_readiness == ProjectReadiness::Ready),
            "Missing, expired or stale project facts cannot establish project readiness"
        );
        let saved_at = Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO research_assessments(bando_id,municipality_id,scope_key,data,saved_at) VALUES(?1,?2,?3,?4,?5)", params![assessment.bando_id,assessment.scope.municipality_id,assessment.scope.key()?,serde_json::to_string(&assessment)?,saved_at])?;
        let id = tx.last_insert_rowid();
        for ((kind, key, scope), version) in deps {
            tx.execute("INSERT INTO research_dependencies(assessment_id,kind,dependency_key,scope_key,version_id) VALUES(?1,?2,?3,?4,?5)", params![id,kind,key,scope,version])?;
        }
        tx.commit()?;
        Ok(Assessment {
            id,
            assessment,
            is_stale: false,
            invalidated_reason: None,
            saved_at,
        })
    }
    pub fn assessment(&self, id: i64) -> Result<Option<Assessment>> {
        let row: Option<(String,bool,Option<String>,String)> = self.conn.query_row("SELECT data,is_stale,invalidated_reason,saved_at FROM research_assessments WHERE id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        row.map(|(data, is_stale, invalidated_reason, saved_at)| {
            Ok(Assessment {
                id,
                assessment: serde_json::from_str(&data)?,
                is_stale,
                invalidated_reason,
                saved_at,
            })
        })
        .transpose()
    }
    /// Current assessment per bando and scope; closed calls are archive-only.
    /// Stale or newly expired evidence downgrades the effective conclusion to unknown.
    pub fn opportunities(
        &self,
        municipality_id: &str,
        as_of: &str,
        include_archive: bool,
    ) -> Result<Vec<Value>> {
        as_of_date(as_of)?;
        let mut stmt = self.conn.prepare("SELECT max(id) FROM research_assessments WHERE municipality_id=?1 GROUP BY bando_id,scope_key ORDER BY max(id) DESC")?;
        let ids = stmt
            .query_map([municipality_id], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut results = Vec::new();
        for id in ids {
            let assessment = self.assessment(id)?.context("Assessment missing")?;
            let bando = self
                .bando(&assessment.assessment.bando_id)?
                .context("Assessment bando missing")?;
            let mut bando_current = true;
            for evidence_id in &bando.evidence_ids {
                bando_current &= self.evidence_current(
                    &self
                        .evidence(*evidence_id)?
                        .context("Bando evidence is missing")?,
                )?;
            }
            let recorded_status = bando.application_status(as_of)?;
            // A source change does not itself reopen a historically closed call.
            // Only a newly evidenced bando update can restore it to the live feed.
            if recorded_status == "closed" && !include_archive {
                continue;
            }
            let status = if bando_current || recorded_status == "closed" {
                recorded_status
            } else {
                "unknown"
            };
            let mut stale = assessment.is_stale;
            let mut unknowns = assessment.assessment.unknowns.clone();
            let mut unresolved_applicant = false;
            let mut unresolved_project = false;
            let mut evidence_ids: BTreeSet<i64> = bando
                .evidence_ids
                .iter()
                .chain(assessment.assessment.evidence_ids.iter())
                .copied()
                .collect();
            for dependency in &assessment.assessment.dependencies {
                if let Dependency::Fact {
                    scope,
                    key,
                    affects,
                } = dependency
                {
                    let fact = self
                        .facts(scope, as_of)?
                        .into_iter()
                        .find(|f| f.fact.key == *key);
                    let original_id: Option<i64> = self.conn.query_row("SELECT version_id FROM research_dependencies WHERE assessment_id=?1 AND kind='fact' AND dependency_key=?2 AND scope_key=?3",params![id,key,scope.key()?],|r| r.get(0)).optional()?.flatten();
                    if let Some(original_id) = original_id
                        && let Some(original_fact) = self.fact_record(original_id, as_of)?
                    {
                        evidence_ids.extend(original_fact.fact.evidence_ids.iter().copied());
                    }
                    if fact
                        .as_ref()
                        .is_none_or(|f| f.is_stale || !f.is_valid_on_date || f.fact.value.is_null())
                    {
                        unresolved_applicant |= *affects == DependencyEffect::Applicant;
                        unresolved_project |= *affects == DependencyEffect::Project;
                        let reason = format!("Fact requires verification: {key}");
                        if !unknowns.contains(&reason) {
                            unknowns.push(reason);
                        }
                    }
                }
            }
            let mut evidence_details = Vec::new();
            for evidence_id in evidence_ids {
                let evidence = self.evidence(evidence_id)?.context("Evidence missing")?;
                stale |= !self.evidence_current(&evidence)?;
                let source = self
                    .source_version(evidence.evidence.source_version_id)?
                    .context("Source missing")?;
                evidence_details.push(json!({"id":evidence.id,"quote":evidence.evidence.quote,"locator":evidence.evidence.locator,"relation":evidence.evidence.relation,"source_url":source.source.url,"source_title":source.source.title,"retrieved_at":source.source.retrieved_at,"source_version_id":source.id,"quote_verification":evidence.quote_verification,"origin_trust":source.origin_trust}));
            }
            let eligibility = if stale
                || (unresolved_applicant
                    && matches!(
                        assessment.assessment.eligibility,
                        Eligibility::Eligible | Eligibility::Ineligible
                    )) {
                Eligibility::Unknown
            } else {
                assessment.assessment.eligibility
            };
            if eligibility == Eligibility::Ineligible && !include_archive {
                continue;
            }
            let readiness = if stale
                || (unresolved_project
                    && assessment.assessment.project_readiness == ProjectReadiness::Ready)
            {
                ProjectReadiness::Unknown
            } else {
                assessment.assessment.project_readiness
            };
            results.push(json!({
                "assessment_id":id,"bando_id":bando.id,"title":bando.title,"authority":bando.authority,
                "scope":assessment.assessment.scope,"municipality_id":municipality_id,
                "eligibility":eligibility,"recorded_eligibility":assessment.assessment.eligibility,
                "project_readiness":readiness,"recorded_project_readiness":assessment.assessment.project_readiness,"application_status":status,
                "deadline":bando.deadline,"amount":assessment.assessment.amount,"bando_amount":bando.amount,
                "accreditation":assessment.assessment.accreditation,"summary":assessment.assessment.summary,"why_fit":assessment.assessment.why_fit,
                "next_actions":assessment.assessment.next_actions,"unknowns":unknowns,
                "evidence_ids":assessment.assessment.evidence_ids,"evidence":evidence_details,"is_stale":stale,
                "invalidated_reason":assessment.invalidated_reason,"as_of":as_of,
                "assessed_as_of":assessment.assessment.as_of,"saved_at":assessment.saved_at
            }));
        }
        Ok(results)
    }
    pub fn research_status(&self) -> Result<Value> {
        let count = |sql: &str| self.conn.query_row(sql, [], |r| r.get::<_, i64>(0));
        let mut stmt = self.conn.prepare(
            "SELECT id,status,updated_at FROM research_jobs ORDER BY updated_at DESC LIMIT 20",
        )?;
        let jobs = stmt.query_map([],|r| Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"updated_at":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let known_cost: f64 = self.conn.query_row(
            "SELECT coalesce(sum(cost),0) FROM research_events",
            [],
            |r| r.get(0),
        )?;
        Ok(
            json!({"bando_count":count("SELECT count(*) FROM research_bandi")?,"assessment_count":count("SELECT count(*) FROM research_assessments")?,"jobs":jobs,"known_cost":known_cost,"unknown_cost_events":count("SELECT count(*) FROM research_events WHERE kind='model_response' AND cost IS NULL")?}),
        )
    }
    pub fn checkpoint(&self, job_id: &str, state: &Value, status: &str) -> Result<()> {
        validate_job(job_id, status)?;
        checkpoint_on(&self.conn, job_id, state, status)
    }
    /// The driver uses this after every completed tool: progress, tool receipt
    /// and provider-reported cost become durable in one SQLite transaction.
    pub fn checkpoint_with_event(
        &self,
        job_id: &str,
        state: &Value,
        status: &str,
        kind: &str,
        event: &Value,
        cost: Option<f64>,
    ) -> Result<i64> {
        validate_job(job_id, status)?;
        validate_event(kind, cost)?;
        let tx = self.conn.unchecked_transaction()?;
        checkpoint_on(&tx, job_id, state, status)?;
        let id = event_on(&tx, job_id, kind, event, cost)?;
        tx.commit()?;
        Ok(id)
    }
    pub fn load_job(&self, job_id: &str) -> Result<Option<Value>> {
        let row: Option<(String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT state,status,created_at,updated_at FROM research_jobs WHERE id=?1",
                [job_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        row.map(|(state,status,created_at,updated_at)| {
            let mut stmt = self.conn.prepare("SELECT id,kind,data,cost,created_at FROM research_events WHERE job_id=?1 ORDER BY id")?;
            let rows = stmt.query_map([job_id], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<f64>>(3)?,r.get::<_,String>(4)?)))?;
            let mut events = Vec::new();
            let mut known_cost = 0.0;
            let mut unknown_cost_events = 0usize;
            for row in rows {
                let (id,kind,data,cost,time) = row?;
                if let Some(cost) = cost { known_cost += cost; } else if kind == "model_response" { unknown_cost_events += 1; }
                events.push(json!({"id":id,"kind":kind,"data":serde_json::from_str::<Value>(&data)?,"cost":cost,"created_at":time}));
            }
            Ok(json!({"id":job_id,"state":serde_json::from_str::<Value>(&state)?,"status":status,"created_at":created_at,"updated_at":updated_at,"events":events,"known_cost":known_cost,"unknown_cost_events":unknown_cost_events}))
        }).transpose()
    }
    pub fn append_event(
        &self,
        job_id: &str,
        kind: &str,
        event: &Value,
        cost: Option<f64>,
    ) -> Result<i64> {
        validate_event(kind, cost)?;
        event_on(&self.conn, job_id, kind, event, cost)
    }
    fn validate_bando_evidence(
        &self,
        id: i64,
        bando_id: &str,
        scope: Option<&Scope>,
    ) -> Result<Evidence> {
        let evidence = self.evidence(id)?.context("Unknown bando evidence")?;
        ensure!(
            evidence.evidence.relation.decisive(),
            "Related-only evidence cannot establish a bando rule, deadline or eligibility conclusion"
        );
        ensure!(
            evidence.evidence.bando_id.as_deref() == Some(bando_id),
            "Evidence belongs to a different bando or has not been linked to this bando"
        );
        if let (Some(target), Some(evidence_scope)) = (scope, evidence.evidence.scope.as_ref()) {
            ensure!(
                evidence_scope.supports(target),
                "Bando evidence belongs to a different research scope"
            );
        }
        ensure!(
            self.evidence_current(&evidence)?,
            "Decisive evidence uses a superseded source version"
        );
        Ok(evidence)
    }
    fn evidence_current(&self, evidence: &Evidence) -> Result<bool> {
        Ok(self.conn.query_row("SELECT s.current_version=v.id FROM research_source_versions v JOIN research_sources s ON s.id=v.source_id WHERE v.id=?1", [evidence.evidence.source_version_id], |r| r.get(0))?)
    }
    fn add_source_dependency(
        &self,
        deps: &mut BTreeMap<(String, String, String), Option<i64>>,
        evidence: &Evidence,
    ) -> Result<()> {
        let source = self
            .source_version(evidence.evidence.source_version_id)?
            .context("Evidence source missing")?;
        // Dependency identity matches the SQL primary key. The original fact
        // version retains its exact historical evidence; this source dependency
        // watches the current logical source once, even when citations span versions.
        let current = self
            .source(&source.source.source_id)?
            .context("Current source missing")?;
        deps.insert(
            ("source".into(), source.source.source_id, String::new()),
            Some(current.id),
        );
        Ok(())
    }
    fn read_json<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<T>> {
        let raw: Option<String> = self.conn.query_row(sql, params, |r| r.get(0)).optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }
}

fn nonblank(value: &str, label: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{label} must not be blank");
    ensure!(
        !value
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
        "{label} must not contain control characters"
    );
    Ok(())
}
fn date(value: &str) -> Result<NaiveDate> {
    let parsed = NaiveDate::parse_from_str(value, "%Y-%m-%d").context("Date must be YYYY-MM-DD")?;
    ensure!(parsed.to_string() == value, "Date must be YYYY-MM-DD");
    Ok(parsed)
}
fn as_of_date(value: &str) -> Result<NaiveDate> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.date_naive());
    }
    date(value)
}
fn validity(from: &Option<String>, until: &Option<String>) -> Result<()> {
    let from = from.as_deref().map(date).transpose()?;
    let until = until.as_deref().map(date).transpose()?;
    if let (Some(from), Some(until)) = (from, until) {
        ensure!(from <= until, "Fact validity interval is reversed");
    }
    Ok(())
}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
fn invalidate(
    conn: &Connection,
    kind: &str,
    key: &str,
    scope: Option<&str>,
    reason: &str,
) -> Result<()> {
    conn.execute("UPDATE research_assessments SET is_stale=1,invalidated_reason=?1 WHERE id IN (SELECT assessment_id FROM research_dependencies WHERE kind=?2 AND dependency_key=?3 AND (?4 IS NULL OR scope_key=?4))",params![reason,kind,key,scope])?;
    Ok(())
}
fn validate_job(job_id: &str, status: &str) -> Result<()> {
    nonblank(job_id, "job_id")?;
    if !matches!(
        status,
        "pending"
            | "running"
            | "paused"
            | "interrupted"
            | "completed"
            | "failed"
            | "blocked"
            | "cancelled"
            | "budget_exhausted"
    ) {
        bail!("Unknown research job status: {status}");
    }
    Ok(())
}
fn validate_event(kind: &str, cost: Option<f64>) -> Result<()> {
    nonblank(kind, "event kind")?;
    if let Some(cost) = cost {
        ensure!(
            cost.is_finite() && cost >= 0.0,
            "Cost must be finite and nonnegative"
        );
    }
    Ok(())
}
fn checkpoint_on(conn: &Connection, job_id: &str, state: &Value, status: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute("INSERT INTO research_jobs(id,state,status,created_at,updated_at) VALUES(?1,?2,?3,?4,?4) ON CONFLICT(id) DO UPDATE SET state=excluded.state,status=excluded.status,updated_at=excluded.updated_at",params![job_id,serde_json::to_string(state)?,status,now])?;
    Ok(())
}
fn event_on(
    conn: &Connection,
    job_id: &str,
    kind: &str,
    event: &Value,
    cost: Option<f64>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO research_events(job_id,kind,data,cost,created_at) VALUES(?1,?2,?3,?4,?5)",
        params![
            job_id,
            kind,
            serde_json::to_string(event)?,
            cost,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Tool schema is deliberately small and append-only. Source registration is
/// absent: the fetch/archive boundary records actual content, never the model.
pub fn operation_schema() -> Value {
    let nullable_string = json!({"type":["string","null"]});
    let scope = json!({"type":"object","additionalProperties":false,"required":["municipality_id"],"properties":{
        "municipality_id":{"type":"string"},"entity_id":nullable_string,"site_id":nullable_string,"project_id":nullable_string
    }});
    let ids = json!({"type":"array","items":{"type":"integer"}});
    let strings = json!({"type":"array","items":{"type":"string"}});
    let deadline = json!({"oneOf":[
        {"type":"object","additionalProperties":false,"required":["precision","value"],"properties":{"precision":{"const":"date"},"value":{"type":"string","description":"Calendar date YYYY-MM-DD; no invented cutoff time."}}},
        {"type":"object","additionalProperties":false,"required":["precision","value"],"properties":{"precision":{"const":"date_time"},"value":{"type":"string","description":"Exact RFC3339 timestamp with original explicit offset."}}}
    ]});
    let evidence = json!({"type":"object","additionalProperties":false,"required":["source_version_id","relation","quote"],"properties":{
        "source_version_id":{"type":"integer"},"bando_id":nullable_string,
        "scope":{"anyOf":[scope,{"type":"null"}],"description":"Null for global call rules. Exact entity/site/project scope for local facts."},
        "relation":{"type":"string","enum":["primary","annex","amendment","related_only"]},
        "quote":{"type":"string","description":"Verbatim quotation. For original PDF/image cite a precise locator; quote matching is not independently verified."},"locator":nullable_string
    }});
    let fact = json!({"type":"object","additionalProperties":false,"required":["scope","key","value"],"properties":{
        "scope":scope,"key":{"type":"string","description":"Reusable semantic key; consult list_fact_keys before adding another spelling."},"value":{},"valid_from":nullable_string,"valid_until":nullable_string,"evidence_ids":ids
    }});
    let bando = json!({"type":"object","additionalProperties":false,"required":["id","title","authority","evidence_ids"],"properties":{
        "id":{"type":"string"},"title":{"type":"string"},"authority":{"type":"string"},"status":{"type":"string","enum":["open","forthcoming","closed","unknown"]},"deadline":{"anyOf":[deadline,{"type":"null"}]},"amount":{},"evidence_ids":ids,"details":{}
    }});
    let dependency = json!({"oneOf":[
        {"type":"object","additionalProperties":false,"required":["kind","scope","key"],"properties":{"kind":{"const":"fact"},"scope":scope,"key":{"type":"string"},"affects":{"type":"string","enum":["applicant","project","context"],"description":"Applicant eligibility and project readiness remain separate. Missing facts are dependencies too."}}},
        {"type":"object","additionalProperties":false,"required":["kind","source_id"],"properties":{"kind":{"const":"source"},"source_id":{"type":"string"}}}
    ]});
    let assessment = json!({"type":"object","additionalProperties":false,"required":["bando_id","scope","as_of","eligibility","project_readiness","summary"],"properties":{
        "bando_id":{"type":"string"},"scope":scope,"as_of":{"type":"string","description":"Current YYYY-MM-DD calendar date or RFC3339 timestamp."},
        "eligibility":{"type":"string","enum":["eligible","ineligible","conditional","unknown"]},
        "project_readiness":{"type":"string","enum":["ready","needs_work","unknown","not_assessed"]},
        "summary":{"type":"string"},"why_fit":{"type":"string","description":"Explain why this municipality and actual applicant fit this call."},
        "accreditation":{"description":"Object with status, deadline {precision,value}, summary and evidence_ids when known; null if unknown."},
        "amount":{"description":"Keep original currencies and bounds, e.g. currency, max_per_project, total, rate, summary; null if unknown."},
        "next_actions":strings,"unknowns":strings,"evidence_ids":ids,"dependencies":{"type":"array","items":dependency}
    }});
    let op = |name: &str, mut properties: Value, required: &[&str]| -> Value {
        properties
            .as_object_mut()
            .expect("schema properties")
            .insert("operation".into(), json!({"type":"string","const":name}));
        let mut required: Vec<&str> = required.to_vec();
        required.push("operation");
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    };
    json!({"type":"object","oneOf":[
        op("add_evidence",json!({"evidence":evidence}),&["evidence"]),
        op("put_fact",json!({"fact":fact}),&["fact"]),
        op("put_bando",json!({"bando":bando}),&["bando"]),
        op("save_assessment",json!({"assessment":assessment}),&["assessment"]),
        op("get_facts",json!({"scope":scope,"as_of":{"type":"string"}}),&["scope","as_of"]),
        op("list_facts",json!({"municipality_id":nullable_string,"key":nullable_string,"as_of":{"type":"string"}}),&["as_of"]),
        op("list_fact_keys",json!({"municipality_id":nullable_string}),&[]),
        op("get_source",json!({"source_id":{"type":"string"}}),&["source_id"]),
        op("get_evidence",json!({"id":{"type":"integer"}}),&["id"]),
        op("get_bando",json!({"id":{"type":"string"}}),&["id"]),
        op("list_bandi",json!({}),&[]),
        op("get_assessment",json!({"id":{"type":"integer"}}),&["id"]),
        op("list_assessments",json!({"municipality_id":{"type":"string"},"as_of":{"type":"string"},"include_archive":{"type":"boolean"}}),&["municipality_id","as_of"])
    ]})
}
