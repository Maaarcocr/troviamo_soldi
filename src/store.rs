use crate::types::{DocumentVersion, Notice, Usage};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

/// Hard bounds shared by read-only export and offline replay.
pub const MAX_DIAGNOSTIC_ATTEMPTS: usize = 1000;
pub const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024 * 1024;

pub struct Store {
    pub conn: Connection,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS notices(id TEXT PRIMARY KEY,title TEXT NOT NULL,source_url TEXT NOT NULL,updated_at TEXT NOT NULL,latest_version INTEGER);
CREATE TABLE IF NOT EXISTS versions(id INTEGER PRIMARY KEY,notice_id TEXT NOT NULL REFERENCES notices(id),fingerprint TEXT NOT NULL,state TEXT NOT NULL,documents TEXT NOT NULL,source_text TEXT,extraction TEXT,error TEXT,created_at TEXT NOT NULL,UNIQUE(notice_id,fingerprint));
CREATE TABLE IF NOT EXISTS attempts(id INTEGER PRIMARY KEY,version_id INTEGER NOT NULL REFERENCES versions(id),state TEXT NOT NULL,model TEXT NOT NULL,request_hash TEXT NOT NULL,response TEXT,usage TEXT,error TEXT,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS runs(id INTEGER PRIMARY KEY,created_at TEXT NOT NULL,summary TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS municipalities(id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS projects(municipality_id TEXT NOT NULL REFERENCES municipalities(id),id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(municipality_id,id));
CREATE TABLE IF NOT EXISTS evidence(municipality_id TEXT NOT NULL REFERENCES municipalities(id),id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(municipality_id,id));")?;
        Ok(Self { conn })
    }
    /// Open an existing database without creating it, migrating it, or seeding facts.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        ensure!(
            path.is_file(),
            "Diagnostic database must be an existing file"
        );
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("Open read-only database {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA query_only=ON;")?;
        Ok(Self { conn })
    }

    /// Latest attempts, newest first. Provider response strings are exported exactly
    /// as saved, including malformed JSON; request bodies, credentials, and files
    /// are never read. This method is also safe on a read-only connection.
    pub fn export_attempts(&self, limit: usize) -> Result<Vec<Value>> {
        ensure!(
            (1..=MAX_DIAGNOSTIC_ATTEMPTS).contains(&limit),
            "--limit must be between 1 and {MAX_DIAGNOSTIC_ATTEMPTS}"
        );
        let mut stmt = self.conn.prepare(
            "SELECT a.id,a.version_id,a.state,a.model,a.request_hash,a.response,a.usage,a.error,a.created_at,
                    n.id,n.title,n.source_url,n.updated_at,n.latest_version,
                    v.fingerprint,v.state,v.documents,v.source_text,v.extraction,v.error,v.created_at,
                    length(CAST(coalesce(a.response,'') AS BLOB)) +
                    length(CAST(coalesce(a.usage,'') AS BLOB)) +
                    length(CAST(coalesce(a.error,'') AS BLOB)) +
                    length(CAST(coalesce(v.documents,'') AS BLOB)) +
                    length(CAST(coalesce(v.source_text,'') AS BLOB)) +
                    length(CAST(coalesce(v.extraction,'') AS BLOB)) +
                    length(CAST(coalesce(v.error,'') AS BLOB)) +
                    length(CAST(n.title AS BLOB)) + length(CAST(n.source_url AS BLOB))
             FROM attempts a JOIN versions v ON v.id=a.version_id
             JOIN notices n ON n.id=v.notice_id ORDER BY a.id DESC LIMIT ?1",
        )?;
        let mut rows = stmt.query([limit as i64])?;
        let mut export = Vec::new();
        let mut bytes = 2usize;
        while let Some(row) = rows.next()? {
            let payload_bytes: i64 = row.get(21)?;
            ensure!(
                payload_bytes >= 0 && payload_bytes as u64 <= MAX_DIAGNOSTIC_BYTES as u64,
                "Attempt exceeds the 64 MiB diagnostic export limit"
            );
            let version_id: i64 = row.get(1)?;
            let latest_version: Option<i64> = row.get(13)?;
            let json_or_raw = |column| -> rusqlite::Result<Value> {
                Ok(row
                    .get::<_, Option<String>>(column)?
                    .map(|raw| serde_json::from_str(&raw).unwrap_or(Value::String(raw)))
                    .unwrap_or(Value::Null))
            };
            let entry = json!({
                "export_schema": "funding-attempt-v1",
                "attempt_id": row.get::<_, i64>(0)?,
                "notice_id": row.get::<_, String>(9)?,
                "version_id": version_id,
                "attempt_state": row.get::<_, String>(2)?,
                "model": row.get::<_, String>(3)?,
                "request_hash": row.get::<_, String>(4)?,
                "response": row.get::<_, Option<String>>(5)?,
                "usage": json_or_raw(6)?,
                "validation_error": row.get::<_, Option<String>>(7)?,
                "attempt_created_at": row.get::<_, String>(8)?,
                "notice": {
                    "id": row.get::<_, String>(9)?,
                    "title": row.get::<_, String>(10)?,
                    "source_url": row.get::<_, String>(11)?,
                    "updated_at": row.get::<_, String>(12)?,
                    "metadata_scope": "current_notice_record_not_historical_snapshot"
                },
                "version": {
                    "fingerprint": row.get::<_, String>(14)?,
                    "state": row.get::<_, String>(15)?,
                    "documents": json_or_raw(16)?,
                    "source_text": row.get::<_, Option<String>>(17)?,
                    "saved_extraction": json_or_raw(18)?,
                    "error": row.get::<_, Option<String>>(19)?,
                    "created_at": row.get::<_, String>(20)?,
                    "is_latest": latest_version == Some(version_id)
                }
            });
            bytes = bytes.saturating_add(serde_json::to_vec(&entry)?.len() + 1);
            ensure!(
                bytes <= MAX_DIAGNOSTIC_BYTES,
                "Export exceeds 64 MiB; choose a smaller --limit"
            );
            export.push(entry);
        }
        Ok(export)
    }

    pub fn seed(&mut self) -> Result<()> {
        let municipalities: Vec<Value> =
            serde_json::from_str(include_str!("../data/municipalities.json"))?;
        let evidence: Vec<Value> = serde_json::from_str(include_str!("../data/evidence.json"))?;
        let tx = self.conn.transaction()?;
        for m in municipalities {
            let id = m["istatCode"].as_str().context("registry missing ISTAT")?;
            tx.execute(
                "INSERT OR IGNORE INTO municipalities(id,data) VALUES(?1,?2)",
                params![id, m.to_string()],
            )?;
        }
        for e in evidence {
            tx.execute(
                "INSERT OR IGNORE INTO evidence(municipality_id,id,data) VALUES(?1,?2,?3)",
                params![
                    e["municipalityId"].as_str(),
                    e["id"].as_str(),
                    e.to_string()
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn municipalities(&self) -> Result<Vec<Value>> {
        self.json_rows(
            "SELECT data FROM municipalities ORDER BY json_extract(data,'$.name')",
            [],
        )
    }
    pub fn evidence(&self, id: &str) -> Result<Vec<Value>> {
        self.json_rows(
            "SELECT data FROM evidence WHERE municipality_id=?1 ORDER BY id",
            [id],
        )
    }
    pub fn projects(&self, id: &str) -> Result<Vec<Value>> {
        self.json_rows(
            "SELECT data FROM projects WHERE municipality_id=?1 ORDER BY id",
            [id],
        )
    }
    fn json_rows<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn latest_notices(&self) -> Result<Vec<Value>> {
        let mut stmt=self.conn.prepare("SELECT n.id,n.title,n.source_url,v.state,v.extraction,v.error,n.updated_at,v.id FROM notices n JOIN versions v ON n.latest_version=v.id ORDER BY n.title")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        rows.map(|row|{let (id,title,url,state,extraction,error,updated_at,version)=row?;Ok(json!({"id":id,"title":title,"source_url":url,"state":state,"extraction":extraction.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"error":error,"updated_at":updated_at,"version":version}))}).collect()
    }
    pub fn stats(&self) -> Result<Value> {
        let count = |table: &str| -> Result<i64> {
            Ok(self
                .conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
        };
        let last: Option<String> = self
            .conn
            .query_row(
                "SELECT summary FROM runs ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(
            json!({"municipalities":count("municipalities")?,"notices":count("notices")?,"versions":count("versions")?,"attempts":count("attempts")?,"last_run":last.map(|s|serde_json::from_str::<Value>(&s)).transpose()?}),
        )
    }
    pub fn previous(&self, id: &str) -> Result<Option<(Vec<DocumentVersion>, Option<Value>)>> {
        let row:Option<(String,Option<String>)>=self.conn.query_row("SELECT v.documents,v.extraction FROM notices n JOIN versions v ON n.latest_version=v.id WHERE n.id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(docs, extraction)| {
            Ok((
                serde_json::from_str(&docs)?,
                extraction.map(|s| serde_json::from_str(&s)).transpose()?,
            ))
        })
        .transpose()
    }
    pub fn cached(&self, id: &str, fingerprint: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,state FROM versions WHERE notice_id=?1 AND fingerprint=?2",
                params![id, fingerprint],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
    pub fn select_cached_version(&self, notice: &Notice, version: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE notices SET latest_version=?1,title=?2,source_url=?3,updated_at=?4 WHERE id=?5",
            params![
                version,
                notice.title,
                notice.source_url,
                Utc::now().to_rfc3339(),
                notice.id
            ],
        )?;
        Ok(())
    }
    pub fn begin_version(
        &mut self,
        notice: &Notice,
        fingerprint: &str,
        documents: &[DocumentVersion],
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let now = Utc::now().to_rfc3339();
        tx.execute("INSERT INTO notices(id,title,source_url,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET title=excluded.title,source_url=excluded.source_url,updated_at=excluded.updated_at",params![notice.id,notice.title,notice.source_url,now])?;
        tx.execute("INSERT INTO versions(notice_id,fingerprint,state,documents,source_text,created_at) VALUES(?1,?2,'pending',?3,?4,?5) ON CONFLICT(notice_id,fingerprint) DO UPDATE SET state='pending',error=NULL,extraction=NULL",params![notice.id,fingerprint,serde_json::to_string(documents)?,notice.source_text,now])?;
        let id = tx.query_row(
            "SELECT id FROM versions WHERE notice_id=?1 AND fingerprint=?2",
            params![notice.id, fingerprint],
            |r| r.get::<_, i64>(0),
        )?;
        tx.execute(
            "UPDATE notices SET latest_version=?1 WHERE id=?2",
            params![id, notice.id],
        )?;
        tx.commit()?;
        Ok(id)
    }
    pub fn attempt(&self, version: i64, request_hash: &str) -> Result<i64> {
        self.conn.execute("INSERT INTO attempts(version_id,state,model,request_hash,created_at) VALUES(?1,'in_flight','openai/gpt-6-luna',?2,?3)",params![version,request_hash,Utc::now().to_rfc3339()])?;
        self.conn.execute(
            "UPDATE versions SET state='in_flight' WHERE id=?1",
            [version],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn finish_attempt(
        &self,
        id: i64,
        state: &str,
        response: Option<&Value>,
        usage: &Usage,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET state=?1,response=?2,usage=?3,error=?4 WHERE id=?5",
            params![
                state,
                response.map(Value::to_string),
                serde_json::to_string(usage)?,
                error,
                id
            ],
        )?;
        Ok(())
    }
    pub fn finish_version(
        &self,
        id: i64,
        state: &str,
        extraction: Option<&Value>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE versions SET state=?1,extraction=?2,error=?3 WHERE id=?4",
            params![state, extraction.map(Value::to_string), error, id],
        )?;
        Ok(())
    }
    pub fn save_run(&self, summary: &Value) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs(created_at,summary) VALUES(?1,?2)",
            params![Utc::now().to_rfc3339(), summary.to_string()],
        )?;
        Ok(())
    }
    /// Local imports are declarations, never authority upgrades. Facts remain scoped to one municipality.
    pub fn import_facts(&mut self, payload: &Value) -> Result<()> {
        let id = payload["municipalityId"]
            .as_str()
            .context("municipalityId required")?;
        ensure!(
            self.conn.query_row(
                "SELECT count(*) FROM municipalities WHERE id=?1",
                [id],
                |r| r.get::<_, i64>(0)
            )? == 1,
            "Unknown municipality"
        );
        let projects = payload["projects"]
            .as_array()
            .context("projects array required")?;
        let evidence = payload["evidence"]
            .as_array()
            .context("evidence array required")?;
        ensure!(
            projects.len() <= 100 && evidence.len() <= 2000,
            "Import too large"
        );
        let fields: Value = serde_json::from_str(include_str!("../data/fields.json"))?;
        let known: Vec<String> = self
            .projects(id)?
            .iter()
            .filter_map(|p| p["id"].as_str().map(str::to_owned))
            .chain(
                projects
                    .iter()
                    .filter_map(|p| p["id"].as_str().map(str::to_owned)),
            )
            .collect();
        let tx = self.conn.transaction()?;
        for p in projects {
            let pid = p["id"].as_str().context("project id required")?;
            ensure!(
                valid_id(pid)
                    && p["name"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty() && s.len() <= 160),
                "Invalid project"
            );
            tx.execute("INSERT INTO projects(municipality_id,id,data) VALUES(?1,?2,?3) ON CONFLICT(municipality_id,id) DO UPDATE SET data=excluded.data",params![id,pid,p.to_string()])?;
        }
        for record in evidence {
            let mut e = record.clone();
            ensure!(
                e["municipalityId"] == id,
                "Evidence belongs to a different municipality"
            );
            let eid = e["id"].as_str().context("Evidence id required")?.to_owned();
            ensure!(valid_id(&eid), "Invalid evidence id");
            let field = e["field"].as_str().context("Evidence field required")?;
            let def = fields.get(field).context("Unknown evidence field")?;
            ensure!(def["readOnly"] != true, "Registry fields are read-only");
            let scope = def["scope"].as_str().context("Field scope missing")?;
            if scope == "municipality" {
                ensure!(
                    e["projectId"].is_null() && e["callId"].is_null(),
                    "Wrong municipality scope"
                );
            } else {
                ensure!(
                    e["projectId"]
                        .as_str()
                        .is_some_and(|s| known.iter().any(|p| p == s)),
                    "Unknown project"
                );
                if scope == "application" {
                    ensure!(
                        e["callId"].as_str().is_some_and(|s| !s.is_empty()),
                        "Application facts require callId"
                    );
                } else {
                    ensure!(e["callId"].is_null(), "Project facts must not have callId");
                }
            }
            let observed = e["observedAt"].as_str().context("observedAt required")?;
            let date = chrono::NaiveDate::parse_from_str(observed, "%Y-%m-%d")?;
            ensure!(date <= Utc::now().date_naive(), "Future evidence date");
            if let Some(until) = e["validUntil"].as_str() {
                ensure!(
                    chrono::NaiveDate::parse_from_str(until, "%Y-%m-%d")? >= date,
                    "Expiry before observation"
                );
            }
            ensure!(
                e["source"]["title"]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty()),
                "Evidence source title required"
            );
            let kind = def["type"].as_str().unwrap_or("");
            let value = &e["value"];
            ensure!(
                match kind {
                    "boolean" => value.is_boolean(),
                    "number" => value.as_f64().is_some_and(|x| x.is_finite()
                        && x >= 0.0
                        && (def["integer"] != true || x.fract() == 0.0)),
                    "date" => value
                        .as_str()
                        .is_some_and(|x| chrono::NaiveDate::parse_from_str(x, "%Y-%m-%d").is_ok()),
                    "enum" => def["options"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|x| x["value"] == *value)),
                    _ => value
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 3000),
                },
                "Invalid value type for {field}"
            );
            e["claimedProvenance"] = e["provenance"].clone();
            e["provenance"] = json!("user_declared");
            // User IDs cannot overwrite an official seed record with the same id.
            let eid = format!("user:{eid}");
            e["id"] = json!(eid);
            tx.execute("INSERT INTO evidence(municipality_id,id,data) VALUES(?1,?2,?3) ON CONFLICT(municipality_id,id) DO UPDATE SET data=excluded.data",params![id,eid,e.to_string()])?;
        }
        tx.commit()?;
        Ok(())
    }
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn diagnostic_export_is_read_only_and_lossless_for_responses() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("funding.sqlite");
        let mut writer = Store::open(&path).unwrap();
        let notice = Notice {
            id: "notice-1".into(),
            title: "Saved call".into(),
            source_url: "https://example.gov/call".into(),
            source_text: Some("Original page content".into()),
            documents: vec![],
        };
        let version = writer.begin_version(&notice, "fingerprint", &[]).unwrap();
        let attempt1 = writer.attempt(version, "hash-1").unwrap();
        let attempt2 = writer.attempt(version, "hash-2").unwrap();
        let raw = "{  \"error\": \"provider unavailable\"  }";
        writer
            .conn
            .execute(
                "UPDATE attempts SET response=?1,error='saved failure',usage='{bad' WHERE id=?2",
                params![raw, attempt2],
            )
            .unwrap();
        writer
            .conn
            .execute(
                "UPDATE attempts SET response='not-json' WHERE id=?1",
                [attempt1],
            )
            .unwrap();
        let before = writer.stats().unwrap();
        drop(writer);
        let database_bytes = std::fs::read(&path).unwrap();
        let reader = Store::open_read_only(&path).unwrap();
        let rows = reader.export_attempts(1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["attempt_id"], attempt2);
        assert_eq!(rows[0]["response"], raw);
        assert_eq!(rows[0]["validation_error"], "saved failure");
        assert_eq!(rows[0]["usage"], "{bad");
        assert_eq!(rows[0]["version"]["source_text"], "Original page content");
        assert_eq!(rows[0]["version"]["documents"], json!([]));
        assert_eq!(rows[0]["version"]["is_latest"], true);
        assert_eq!(
            reader.export_attempts(2).unwrap()[1]["response"],
            "not-json"
        );
        assert_eq!(reader.stats().unwrap(), before);
        assert_eq!(reader.stats().unwrap()["municipalities"], 0);
        assert!(reader.conn.execute("DELETE FROM attempts", []).is_err());
        assert!(reader.export_attempts(0).is_err());
        assert!(reader.export_attempts(MAX_DIAGNOSTIC_ATTEMPTS + 1).is_err());
        drop(reader);
        assert_eq!(std::fs::read(&path).unwrap(), database_bytes);
        assert!(!path.with_extension("lock").exists());
    }

    #[test]
    fn read_only_missing_database_does_not_create_it_or_parent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing/funding.sqlite");
        assert!(Store::open_read_only(&path).is_err());
        assert!(!path.parent().unwrap().exists());
    }
}
