//! Read-only research tools. Network content is untrusted evidence, never instructions.
//! Originals are content-addressed; the model receives opaque file IDs, not paths.

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{blocking::Client, header};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Cursor, Read, Write},
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};
use url::Url;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const TEXT_WINDOW: usize = 60_000;
const OFFICIAL_HOSTS: &[&str] = &[
    "europa.eu",
    "regione.sicilia.it",
    "euroinfosicilia.it",
    "gov.it",
    "invitalia.it",
    "gazzettaufficiale.it",
    "comune.pachino.sr.it",
];
const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceVersion {
    pub file_id: String,
    pub sha256: String,
    pub archive_path: String,
    pub source_url: String,
    pub final_url: String,
    pub fetched_at: String,
    pub mime_type: String,
    pub bytes: usize,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub rendered: bool,
}

pub struct ResearchSources {
    root: PathBuf,
    search_endpoint: String,
    search_key: Option<String>,
    render_endpoint: Option<String>,
    render_key: Option<String>,
    workspace_files: BTreeMap<String, PathBuf>,
}

impl ResearchSources {
    pub fn from_env(workspace: &Path) -> Result<Self> {
        fs::create_dir_all(workspace).context("Create research workspace")?;
        ensure!(
            !fs::symlink_metadata(workspace)?.file_type().is_symlink(),
            "Workspace cannot be a symlink"
        );
        let root = workspace.canonicalize()?;
        let archive = root.join("source_versions");
        if archive.exists() {
            ensure!(
                !fs::symlink_metadata(&archive)?.file_type().is_symlink(),
                "Archive cannot be a symlink"
            );
        }
        fs::create_dir_all(&archive)?;
        Ok(Self {
            root,
            search_endpoint: std::env::var("FUNDING_SEARCH_ENDPOINT")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| BRAVE_ENDPOINT.into()),
            search_key: std::env::var("BRAVE_SEARCH_API_KEY")
                .ok()
                .filter(|x| !x.is_empty()),
            render_endpoint: std::env::var("FUNDING_RENDER_ENDPOINT")
                .ok()
                .filter(|x| !x.is_empty()),
            render_key: std::env::var("FUNDING_RENDER_API_KEY")
                .ok()
                .filter(|x| !x.is_empty()),
            workspace_files: BTreeMap::new(),
        })
    }

    /// Brave is optional when the model request enables OpenRouter's native
    /// server-side web search with the existing OpenRouter account.
    pub fn has_search_backend(&self) -> bool {
        self.search_endpoint != BRAVE_ENDPOINT || self.search_key.is_some()
    }

    /// Only these read-only operations can be invoked by the research model.
    pub fn execute(&mut self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "search_web" => {
                self.search_web(string(args, "query")?, args["count"].as_u64().unwrap_or(10))
            }
            "fetch_page" => self.fetch_page(string(args, "url")?),
            "render_page" => self.render_page(string(args, "url")?),
            "read_document" => self.read_document(string(args, "file_id")?, args),
            "search_workspace" => self.search_workspace(args["query"].as_str().unwrap_or("")),
            "read_workspace_file" => self.read_document(string(args, "file_id")?, args),
            "calculate" => Ok(
                json!({"expression":string(args,"expression")?, "result":calculate(string(args,"expression")?)?}),
            ),
            _ => bail!("Unknown read-only research tool: {name}"),
        }
    }

    pub fn search_web(&mut self, query: &str, count: u64) -> Result<Value> {
        ensure!(
            !query.trim().is_empty() && query.len() <= 4000,
            "Search query must contain 1–4000 bytes"
        );
        ensure!(
            (1..=20).contains(&count),
            "Search count must be between 1 and 20"
        );
        if self.search_endpoint == BRAVE_ENDPOINT {
            ensure!(
                self.search_key.is_some(),
                "Web search unavailable: configure BRAVE_SEARCH_API_KEY (or a Brave-compatible FUNDING_SEARCH_ENDPOINT)"
            );
        }
        let mut url = public_url(&self.search_endpoint)?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("count", &count.to_string());
        let headers = self
            .search_key
            .as_deref()
            .map(|key| vec![("X-Subscription-Token", key)])
            .unwrap_or_default();
        let fetched = request(&url, &headers, None, false)?;
        let source = self.archive(&fetched, false)?;
        let data: Value = serde_json::from_slice(&fetched.body)
            .context("Search backend must return Brave-compatible JSON")?;
        let items = data["web"]["results"]
            .as_array()
            .context("Search response missing web.results")?;
        let mut results = Vec::new();
        for item in items.iter().take(count as usize) {
            let Some(raw_url) = item["url"].as_str() else {
                continue;
            };
            let Ok(url) = public_url(raw_url) else {
                continue;
            };
            results.push(json!({
                "title":item["title"], "url":url.as_str(), "description":item["description"],
                "official_domain":official(&url), "evidence_status":"search_snippet_only; fetch the original before citing"
            }));
        }
        Ok(json!({"untrusted":true,"query":query,"results":results,"search_response":source}))
    }

    pub fn fetch_page(&mut self, raw_url: &str) -> Result<Value> {
        let url = public_url(raw_url)?;
        let fetched = request(&url, &[], None, true)?;
        let source = self.archive(&fetched, false)?;
        self.describe(&source, &fetched.body, &json!({}))
    }

    /// Optional service contract: POST {url}; reply {final_url,html}.
    /// The operator's renderer MUST enforce public-only egress on every browser
    /// navigation, redirect and subresource. This client also checks both URLs.
    /// It never pretends static HTTP is a rendered browser.
    pub fn render_page(&mut self, raw_url: &str) -> Result<Value> {
        let endpoint = self.render_endpoint.as_deref().context("Rendered browser unavailable: FUNDING_RENDER_ENDPOINT is not configured; static fetch cannot execute JavaScript")?;
        let url = public_url(raw_url)?;
        public_addresses(&url)?;
        let endpoint = public_url(endpoint)?;
        let headers = self
            .render_key
            .as_deref()
            .map(|key| vec![("Authorization", format!("Bearer {key}"))])
            .unwrap_or_default();
        let headers: Vec<_> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let fetched = request(
            &endpoint,
            &headers,
            Some(&json!({"url":url.as_str(),"network_policy":"public-only"})),
            false,
        )?;
        let backend_record = self.archive(&fetched, false)?;
        let data: Value = serde_json::from_slice(&fetched.body)
            .context("Renderer must return JSON {final_url,html}")?;
        let final_url = public_url(string(&data, "final_url")?)?;
        public_addresses(&final_url)?;
        let rendered = Fetched {
            requested_url: url.to_string(),
            final_url: final_url.to_string(),
            mime_type: "text/html".into(),
            body: string(&data, "html")?.as_bytes().to_vec(),
            etag: None,
            last_modified: None,
        };
        let source = self.archive(&rendered, true)?;
        let mut result = self.describe(&source, &rendered.body, &json!({}))?;
        result["render_backend_response"] = json!(backend_record);
        Ok(result)
    }

    fn archive(&self, fetched: &Fetched, rendered: bool) -> Result<SourceVersion> {
        let sha = hash(&fetched.body);
        let path = self.safe_child(&PathBuf::from("source_versions").join(&sha), false)?;
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(&fetched.body)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    hash(&read_bounded(&path)?) == sha,
                    "Archived original hash mismatch"
                );
            }
            Err(error) => return Err(error).context("Archive original source"),
        }
        let mut source = SourceVersion {
            file_id: String::new(),
            sha256: sha.clone(),
            archive_path: format!("source_versions/{sha}"),
            source_url: fetched.requested_url.clone(),
            final_url: fetched.final_url.clone(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
            mime_type: fetched.mime_type.clone(),
            bytes: fetched.body.len(),
            etag: fetched.etag.clone(),
            last_modified: fetched.last_modified.clone(),
            rendered,
        };
        // Keep every fetch observation, even when two URLs return identical bytes.
        let observation_id = hash(&serde_json::to_vec(&source)?);
        source.file_id = format!("source:{sha}:{observation_id}");
        let observation = serde_json::to_vec(&source)?;
        let metadata_name = format!("{sha}-{observation_id}.json");
        let metadata_path =
            self.safe_child(&PathBuf::from("source_versions").join(metadata_name), false)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(metadata_path)
        {
            Ok(mut file) => file.write_all(&observation)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("Archive source observation"),
        }
        Ok(source)
    }

    pub fn read_document(&mut self, file_id: &str, args: &Value) -> Result<Value> {
        let (path, source) = self.resolve_file(file_id)?;
        let body = read_bounded(&path)?;
        let source = if file_id.starts_with("workspace:") {
            self.archive(
                &Fetched {
                    requested_url: source.source_url,
                    final_url: source.final_url,
                    mime_type: source.mime_type,
                    body: body.clone(),
                    etag: None,
                    last_modified: None,
                },
                false,
            )?
        } else {
            source
        };
        self.describe(&source, &body, args)
    }

    fn describe(&self, source: &SourceVersion, body: &[u8], args: &Value) -> Result<Value> {
        let mut result = json!({"untrusted":true,"source":source});
        // The requested canonical URL is the logical identity: a changed redirect
        // target must update this source and invalidate its existing dependents.
        result["source"]["source_id"] = json!(source.source_url);
        result["source"]["url"] = json!(source.final_url);
        result["source"]["title"] = json!(
            Url::parse(&source.final_url)
                .ok()
                .and_then(|u| u
                    .path_segments()
                    .and_then(|mut segments| segments.next_back())
                    .map(str::to_owned))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| source.final_url.clone())
        );
        result["source"]["body"] = json!("");
        result["source"]["retrieved_at"] = json!(source.fetched_at);
        result["source"]["content_hash"] = json!(source.sha256);
        result["source"]["mime"] = json!(source.mime_type);
        result["source"]["origin_kind"] = json!(if source.final_url.starts_with("workspace:") {
            "local_workspace_unverified"
        } else {
            "public_web"
        });
        match source.mime_type.as_str() {
            "text/html" | "application/xhtml+xml" => {
                let mut document =
                    html_document(&String::from_utf8_lossy(body), &source.final_url)?;
                let text = document["text"].as_str().unwrap_or_default();
                if document["title"]
                    .as_str()
                    .is_some_and(|title| !title.is_empty())
                {
                    result["source"]["title"] = document["title"].clone();
                }
                if std::str::from_utf8(body).is_err() {
                    result["decoding_warning"] = json!(
                        "HTML was not UTF-8; replacement characters may affect quotations. Original bytes remain archived."
                    );
                }
                result["source"]["body"] = json!(text);
                let window = text_window(text, args)?;
                document["text"] = window["text"].clone();
                document["text_window"] = window;
                result["document"] = document;
            }
            "application/pdf" | "image/png" | "image/jpeg" | "image/webp" | "image/gif" => {
                result["native_file_ids"] = json!([source.file_id]);
                result["reading_method"] = json!(
                    "Original bytes supplied to the model as a native PDF/image attachment; cite pages or visible regions. No OCR or text extraction has been claimed."
                );
            }
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.oasis.opendocument.text" => {
                let text = office_text(
                    body,
                    source.mime_type.ends_with("wordprocessingml.document"),
                )?;
                result["source"]["body"] = json!(text);
                result["document"] = text_window(&text, args)?;
                result["reading_method"] = json!(
                    "Document text extracted from original XML; layout/images are not represented"
                );
            }
            mime if mime.starts_with("text/")
                || mime == "application/json"
                || mime.ends_with("+json")
                || mime == "application/xml" =>
            {
                let text = std::str::from_utf8(body)
                    .context("Text document is not UTF-8; original remains archived")?;
                result["source"]["body"] = json!(text);
                result["document"] = text_window(text, args)?;
            }
            _ => {
                result["reading_error"] = json!(
                    "Unsupported document format; original is archived but has not been read"
                );
            }
        }
        Ok(result)
    }

    /// Native model parts use verified archived bytes, never model-provided paths.
    pub fn native_parts(&self, file_ids: &[String]) -> Result<Vec<Value>> {
        file_ids.iter().map(|file_id| {
            let (path, source) = self.resolve_file(file_id)?;
            let bytes = read_bounded(&path)?;
            let data = format!("data:{};base64,{}", source.mime_type, STANDARD.encode(bytes));
            match source.mime_type.as_str() {
                "application/pdf" => Ok(json!({"type":"file","file":{"filename":format!("{}.pdf",source.sha256),"file_data":data}})),
                "image/png" | "image/jpeg" | "image/webp" | "image/gif" => Ok(json!({"type":"image_url","image_url":{"url":data}})),
                other => bail!("No native attachment support for {other}"),
            }
        }).collect()
    }

    pub fn search_workspace(&mut self, query: &str) -> Result<Value> {
        ensure!(query.len() <= 4000, "Workspace query is too long");
        let mut files = Vec::new();
        collect_files(&self.root, &self.root, &mut files)?;
        files.sort();
        let query = query.to_lowercase();
        let mut results = Vec::new();
        for relative in files {
            let path = self.safe_child(&relative, true)?;
            let name = relative.to_string_lossy().into_owned();
            let body = read_bounded(&path)?;
            let mime = detect_mime("", &name, &body);
            let searchable = if mime.starts_with("text/") || mime == "application/json" {
                Some(String::from_utf8_lossy(&body).into_owned())
            } else {
                None
            };
            let matched = query.is_empty()
                || name.to_lowercase().contains(&query)
                || searchable
                    .as_ref()
                    .is_some_and(|text| text.to_lowercase().contains(&query));
            if !matched {
                continue;
            }
            let file_id = format!("workspace:{}", hash(name.as_bytes()));
            self.workspace_files.insert(file_id.clone(), relative);
            results
                .push(json!({"file_id":file_id,"name":name,"mime_type":mime,"bytes":body.len()}));
        }
        let mut observations: BTreeMap<(String, String), SourceVersion> = BTreeMap::new();
        for entry in fs::read_dir(self.safe_child(Path::new("source_versions"), true)?)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || !entry.file_name().to_string_lossy().ends_with(".json")
            {
                continue;
            }
            let relative = PathBuf::from("source_versions").join(entry.file_name());
            let path = self.safe_child(&relative, true)?;
            let source: SourceVersion = serde_json::from_slice(&read_bounded(&path)?)?;
            let key = (source.source_url.clone(), source.sha256.clone());
            let latest = observations
                .get(&key)
                .is_none_or(|old| old.fetched_at < source.fetched_at);
            if latest {
                observations.insert(key, source);
            }
        }
        for source in observations.into_values() {
            let (path, source) = self.resolve_file(&source.file_id)?;
            let body = read_bounded(&path)?;
            let searchable =
                source.mime_type.starts_with("text/") || source.mime_type == "application/json";
            if query.is_empty()
                || source.final_url.to_lowercase().contains(&query)
                || (searchable
                    && String::from_utf8_lossy(&body)
                        .to_lowercase()
                        .contains(&query))
            {
                results.push(json!({"file_id":source.file_id,"name":source.final_url,"mime_type":source.mime_type,"bytes":body.len(),"source":source}));
            }
        }
        Ok(json!({"untrusted":true,"query":query,"files":results}))
    }

    fn resolve_file(&self, file_id: &str) -> Result<(PathBuf, SourceVersion)> {
        if let Some(parts) = file_id.strip_prefix("source:") {
            let (sha, observation_id) = parts.split_once(':').context("Invalid source file ID")?;
            ensure!(
                is_hash(sha) && is_hash(observation_id),
                "Invalid archived file ID"
            );
            let path = self.safe_child(&PathBuf::from("source_versions").join(sha), true)?;
            ensure!(
                hash(&read_bounded(&path)?) == sha,
                "Archived original hash mismatch"
            );
            let metadata = self.safe_child(
                &PathBuf::from("source_versions").join(format!("{sha}-{observation_id}.json")),
                true,
            )?;
            let source: SourceVersion = serde_json::from_slice(&read_bounded(&metadata)?)?;
            ensure!(
                source.sha256 == sha && source.file_id == file_id,
                "Invalid archive metadata"
            );
            let mut observation = source.clone();
            observation.file_id.clear();
            ensure!(
                hash(&serde_json::to_vec(&observation)?) == observation_id,
                "Archived source observation hash mismatch"
            );
            return Ok((path, source));
        }
        let relative = self
            .workspace_files
            .get(file_id)
            .context("Unknown file ID; use search_workspace first")?;
        let path = self.safe_child(relative, true)?;
        let body = read_bounded(&path)?;
        let sha = hash(&body);
        Ok((
            path,
            SourceVersion {
                file_id: file_id.into(),
                sha256: sha,
                archive_path: relative.to_string_lossy().into_owned(),
                source_url: file_id.into(),
                final_url: file_id.into(),
                fetched_at: chrono::Utc::now().to_rfc3339(),
                mime_type: detect_mime("", &relative.to_string_lossy(), &body),
                bytes: body.len(),
                etag: None,
                last_modified: None,
                rendered: false,
            },
        ))
    }

    fn safe_child(&self, relative: &Path, must_exist: bool) -> Result<PathBuf> {
        ensure!(
            !relative.is_absolute(),
            "Absolute workspace paths are forbidden"
        );
        let mut path = self.root.clone();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                bail!("Unsafe workspace path")
            };
            path.push(name);
            match fs::symlink_metadata(&path) {
                Ok(meta) => ensure!(
                    !meta.file_type().is_symlink(),
                    "Workspace symlinks are forbidden"
                ),
                Err(error) if !must_exist && error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("Resolve workspace file"),
            }
        }
        if must_exist {
            ensure!(
                path.canonicalize()?.starts_with(&self.root),
                "File escaped workspace"
            );
        }
        Ok(path)
    }
}

struct Fetched {
    requested_url: String,
    final_url: String,
    mime_type: String,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
}

fn request(
    initial: &Url,
    headers: &[(&str, &str)],
    payload: Option<&Value>,
    follow_redirects: bool,
) -> Result<Fetched> {
    let mut url = initial.clone();
    for hop in 0..=8 {
        // Resolve once, reject mixed public/private DNS, then pin the approved
        // addresses in reqwest. Disable environment proxies and automatic redirects.
        let addresses = public_addresses(&url)?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .resolve_to_addrs(url.host_str().context("URL missing host")?, &addresses)
            .user_agent("FundingResearch/1.0 (read-only source retrieval)")
            .build()?;
        let mut request = if let Some(payload) = payload {
            client.post(url.clone()).json(payload)
        } else {
            client.get(url.clone())
        };
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().context("Research source request failed")?;
        if response.status().is_redirection() {
            ensure!(
                follow_redirects && headers.is_empty() && payload.is_none(),
                "Authenticated/backend redirects are not followed"
            );
            ensure!(hop < 8, "Too many source redirects");
            let target = response
                .headers()
                .get(header::LOCATION)
                .context("Redirect missing Location")?
                .to_str()?;
            url = public_url(url.join(target)?.as_str())?;
            continue;
        }
        let response = response
            .error_for_status()
            .context("Source returned an HTTP error; no content was read")?;
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_BYTES),
            "Source exceeds 64 MiB download safety limit"
        );
        let claimed_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let last_modified = response
            .headers()
            .get(header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        response.take(MAX_BYTES + 1).read_to_end(&mut body)?;
        ensure!(
            body.len() as u64 <= MAX_BYTES,
            "Source exceeds 64 MiB download safety limit"
        );
        return Ok(Fetched {
            requested_url: initial.to_string(),
            final_url: url.to_string(),
            mime_type: detect_mime(&claimed_type, url.path(), &body),
            body,
            etag,
            last_modified,
        });
    }
    bail!("Source redirect limit exceeded")
}

/// Syntactic boundary before DNS. Public HTTPS sources are allowed; "official"
/// is a source-quality hint, never proof that content or links are trustworthy.
pub fn public_url(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw).context("Invalid source URL")?;
    ensure!(url.scheme() == "https", "Research sources must use HTTPS");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URL credentials are forbidden"
    );
    ensure!(
        url.port_or_known_default() == Some(443),
        "Only the HTTPS default port is allowed"
    );
    let host = url.host_str().context("URL missing host")?;
    ensure!(
        host.contains('.') || host.starts_with('['),
        "Single-label/private hostnames are forbidden"
    );
    let lower = host.trim_end_matches('.').to_lowercase();
    ensure!(
        ![
            "localhost",
            "local",
            "internal",
            "home",
            "lan",
            "onion",
            "test",
            "invalid",
            "example"
        ]
        .iter()
        .any(|suffix| lower == *suffix || lower.ends_with(&format!(".{suffix}"))),
        "Private hostnames are forbidden"
    );
    if let Some(url::Host::Ipv4(ip)) = url.host() {
        ensure!(
            public_ip(IpAddr::V4(ip)),
            "Non-public source IP is forbidden"
        );
    }
    if let Some(url::Host::Ipv6(ip)) = url.host() {
        ensure!(
            public_ip(IpAddr::V6(ip)),
            "Non-public source IP is forbidden"
        );
    }
    url.set_fragment(None);
    Ok(url)
}

fn public_addresses(url: &Url) -> Result<Vec<SocketAddr>> {
    let host = url
        .host_str()
        .context("URL missing host")?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let addresses: Vec<_> = (host, 443)
        .to_socket_addrs()
        .context("Resolve source DNS")?
        .collect();
    ensure!(!addresses.is_empty(), "Source DNS returned no addresses");
    ensure!(
        addresses.iter().all(|addr| public_ip(addr.ip())),
        "Source DNS resolves to a private/reserved address"
    );
    Ok(addresses)
}

pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            // Global-unicast only. Exclude transition, documentation and special
            // assignments rather than treating IPv4-mapped/NAT64 as public.
            (segments[0] & 0xe000) == 0x2000
                && !(segments[0] == 0x2001 && segments[1] < 0x0200)
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                && segments[0] != 0x2002
                && !(segments[0] == 0x3fff && segments[1] < 0x1000)
        }
    }
}

fn official(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    OFFICIAL_HOSTS
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

fn detect_mime(claimed: &str, filename: &str, body: &[u8]) -> String {
    if body.starts_with(b"%PDF-") {
        return "application/pdf".into();
    }
    if body.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png".into();
    }
    if body.starts_with(b"\xff\xd8\xff") {
        return "image/jpeg".into();
    }
    if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
        return "image/gif".into();
    }
    if body.starts_with(b"RIFF") && body.get(8..12) == Some(b"WEBP".as_slice()) {
        return "image/webp".into();
    }
    let prefix = String::from_utf8_lossy(&body[..body.len().min(512)])
        .trim_start()
        .to_ascii_lowercase();
    if prefix.starts_with("<!doctype html") || prefix.starts_with("<html") {
        return "text/html".into();
    }
    let claimed = claimed
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !claimed.is_empty() && claimed != "application/octet-stream" {
        if claimed == "application/pdf" || claimed.starts_with("image/") {
            return "application/octet-stream".into();
        }
        return claimed;
    }
    match Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html",
        "txt" | "md" | "csv" | "tsv" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "odt" => "application/vnd.oasis.opendocument.text",
        _ => "application/octet-stream",
    }
    .into()
}

fn html_document(raw: &str, base: &str) -> Result<Value> {
    let document = Html::parse_document(raw);
    let title_selector = Selector::parse("title").unwrap();
    let title = document
        .select(&title_selector)
        .next()
        .map(text_of)
        .unwrap_or_default();
    let main_selector = Selector::parse("main, [role=main]").unwrap();
    let article_selector = Selector::parse("article").unwrap();
    let body_selector = Selector::parse("body").unwrap();
    let root = document
        .select(&main_selector)
        .next()
        .or_else(|| document.select(&article_selector).next())
        .or_else(|| document.select(&body_selector).next())
        .unwrap_or_else(|| document.root_element());
    let heading_selector = Selector::parse("h1,h2,h3,h4,h5,h6").unwrap();
    let headings: Vec<_> = root
        .select(&heading_selector)
        .filter(|node| !peripheral(*node))
        .map(|node| json!({"level":node.value().name(),"text":text_of(node)}))
        .collect();
    // Read all visible main text, including div/span-only layouts. Preserve text
    // node order and line boundaries without duplicating nested paragraphs.
    let sections: Vec<String> = root
        .descendants()
        .filter_map(|node| {
            let text = node.value().as_text()?;
            if node
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(peripheral)
            {
                return None;
            }
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() { None } else { Some(text) }
        })
        .collect();
    let link_selector = Selector::parse("a[href], h1,h2,h3,h4,h5,h6").unwrap();
    let mut heading = String::new();
    let mut links = Vec::new();
    let base = Url::parse(base).ok();
    for node in document.select(&link_selector) {
        if node.value().name() != "a" {
            heading = text_of(node);
            continue;
        }
        let href = node.value().attr("href").unwrap_or_default();
        let url = base
            .as_ref()
            .and_then(|base| base.join(href).ok())
            .or_else(|| Url::parse(href).ok());
        let Some(url) = url.and_then(|url| public_url(url.as_str()).ok()) else {
            continue;
        };
        let in_main = node.ancestors().any(|ancestor| ancestor.id() == root.id());
        links.push(json!({"url":url.as_str(),"label":text_of(node),"section_heading":heading,
            "relationship":if peripheral(node) || !in_main { "navigation_or_related_link" } else { "main_content_link" },
            "official_domain":official(&url),"document_hint":document_extension(url.path())}));
    }
    Ok(
        json!({"title":title,"headings":headings,"text":sections.join("\n\n"),"links":links,
        "link_warning":"Links retain their page context. A related-link or navigation link is not evidence that two notices share eligibility, deadlines, or legal terms."}),
    )
}

fn peripheral(node: ElementRef<'_>) -> bool {
    std::iter::once(node)
        .chain(node.ancestors().filter_map(ElementRef::wrap))
        .any(|node| {
            let tag = node.value().name();
            if [
                "nav", "aside", "footer", "script", "style", "noscript", "template",
            ]
            .contains(&tag)
            {
                return true;
            }
            if node.value().attr("hidden").is_some()
                || node.value().attr("aria-hidden") == Some("true")
            {
                return true;
            }
            let labels = format!(
                "{} {}",
                node.value().attr("class").unwrap_or_default(),
                node.value().attr("id").unwrap_or_default()
            )
            .to_ascii_lowercase();
            ["related", "sidebar", "breadcrumb", "cookie", "correlati"]
                .iter()
                .any(|label| labels.contains(label))
        })
}

fn text_of(node: ElementRef<'_>) -> String {
    node.text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn document_extension(path: &str) -> bool {
    [".pdf", ".docx", ".odt", ".png", ".jpg", ".jpeg", ".webp"]
        .iter()
        .any(|ext| path.to_ascii_lowercase().ends_with(ext))
}

fn office_text(body: &[u8], docx: bool) -> Result<String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(body)).context("Open original office document")?;
    let mut file = archive
        .by_name(if docx {
            "word/document.xml"
        } else {
            "content.xml"
        })
        .context("Document text XML missing")?;
    ensure!(
        file.size() <= MAX_BYTES,
        "Office document text exceeds safety limit"
    );
    let mut raw = String::new();
    (&mut file).take(MAX_BYTES + 1).read_to_string(&mut raw)?;
    ensure!(
        raw.len() as u64 <= MAX_BYTES,
        "Office document text exceeds safety limit"
    );
    let xml = raw
        .replace("</w:p>", "\n</w:p>")
        .replace("</text:p>", "\n</text:p>");
    let document = Html::parse_fragment(&xml);
    Ok(document.root_element().text().collect::<Vec<_>>().join(" "))
}

fn text_window(text: &str, args: &Value) -> Result<Value> {
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(TEXT_WINDOW as u64) as usize;
    ensure!(
        (1..=1_000_000).contains(&limit),
        "Read limit must be 1–1000000 characters"
    );
    let total = text.chars().count();
    ensure!(offset <= total, "Text offset is past end of document");
    let selected: String = text.chars().skip(offset).take(limit).collect();
    let next = offset + selected.chars().count();
    Ok(
        json!({"text":selected,"offset":offset,"total_characters":total,"truncated":next < total,"next_offset":if next < total {Some(next)} else {None}}),
    )
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "source_versions" {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            collect_files(root, &path, files)?;
        } else if kind.is_file() && entry.metadata()?.len() <= MAX_BYTES {
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if [
                "txt", "md", "json", "html", "htm", "xml", "csv", "tsv", "docx", "odt", "pdf",
                "png", "jpg", "jpeg", "gif", "webp",
            ]
            .contains(&extension.as_str())
            {
                files.push(path.strip_prefix(root)?.to_path_buf());
            }
        }
    }
    Ok(())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= MAX_BYTES,
        "File is not a regular file or exceeds safety limit"
    );
    let mut body = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut body)?;
    ensure!(body.len() as u64 <= MAX_BYTES, "File exceeds safety limit");
    Ok(body)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key]
        .as_str()
        .with_context(|| format!("Missing string argument: {key}"))
}

/// Arithmetic only: no variables, functions, shell, evaluation engine or SQL.
pub fn calculate(expression: &str) -> Result<f64> {
    ensure!(
        !expression.is_empty() && expression.len() <= 4096,
        "Expression must contain 1–4096 bytes"
    );
    struct Parser<'a> {
        bytes: &'a [u8],
        position: usize,
        depth: usize,
    }
    impl Parser<'_> {
        fn peek(&mut self) -> Option<u8> {
            while self
                .bytes
                .get(self.position)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.position += 1;
            }
            self.bytes.get(self.position).copied()
        }
        fn expr(&mut self) -> Result<f64> {
            let mut value = self.term()?;
            while let Some(op @ (b'+' | b'-')) = self.peek() {
                self.position += 1;
                let rhs = self.term()?;
                value = if op == b'+' { value + rhs } else { value - rhs };
            }
            Ok(value)
        }
        fn term(&mut self) -> Result<f64> {
            let mut value = self.atom()?;
            while let Some(op @ (b'*' | b'/')) = self.peek() {
                self.position += 1;
                let rhs = self.atom()?;
                ensure!(op != b'/' || rhs != 0.0, "Division by zero");
                value = if op == b'*' { value * rhs } else { value / rhs };
            }
            Ok(value)
        }
        fn atom(&mut self) -> Result<f64> {
            self.depth += 1;
            ensure!(self.depth <= 64, "Arithmetic nesting limit exceeded");
            let value = match self.peek() {
                Some(b'+') => {
                    self.position += 1;
                    self.atom()?
                }
                Some(b'-') => {
                    self.position += 1;
                    -self.atom()?
                }
                Some(b'(') => {
                    self.position += 1;
                    let value = self.expr()?;
                    ensure!(self.peek() == Some(b')'), "Expected closing parenthesis");
                    self.position += 1;
                    value
                }
                Some(b'0'..=b'9' | b'.') => {
                    let start = self.position;
                    while self
                        .bytes
                        .get(self.position)
                        .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
                    {
                        self.position += 1;
                    }
                    std::str::from_utf8(&self.bytes[start..self.position])?
                        .parse::<f64>()
                        .context("Invalid number")?
                }
                _ => bail!("Expected a number or parenthesis"),
            };
            self.depth -= 1;
            ensure!(value.is_finite(), "Arithmetic result is not finite");
            Ok(value)
        }
    }
    let mut parser = Parser {
        bytes: expression.as_bytes(),
        position: 0,
        depth: 0,
    };
    let value = parser.expr()?;
    ensure!(parser.peek().is_none(), "Unexpected expression suffix");
    ensure!(value.is_finite(), "Arithmetic result is not finite");
    Ok(value)
}

pub fn tool_definitions() -> Vec<Value> {
    let definitions = [
        (
            "search_web",
            "Search public web sources with Brave. Snippets are leads, not evidence; fetch originals. Prefer official national, EU, regional and municipal sources.",
            json!({"query":{"type":"string"},"count":{"type":"integer","minimum":1,"maximum":20}}),
            vec!["query"],
        ),
        (
            "fetch_page",
            "Fetch a public HTTPS source, preserving original bytes and provenance. HTML includes main text, headings, and contextual links; PDFs/images return native attachment IDs.",
            json!({"url":{"type":"string"}}),
            vec!["url"],
        ),
        (
            "render_page",
            "Use the explicitly configured rendered-browser backend for JavaScript-dependent pages. Returns an explicit unavailable error if no backend exists.",
            json!({"url":{"type":"string"}}),
            vec!["url"],
        ),
        (
            "read_document",
            "Read an archived PDF/image/document by file ID. PDFs/images are delivered as original native attachments. Text can be paged using character offset and limit.",
            json!({"file_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000000}}),
            vec!["file_id"],
        ),
        (
            "search_workspace",
            "Find readable research workspace files by filename or text; empty query lists files. Returns opaque IDs, never accepts paths.",
            json!({"query":{"type":"string"}}),
            vec!["query"],
        ),
        (
            "read_workspace_file",
            "Read a file ID issued by search_workspace. Text windows explicitly report truncation and next offset.",
            json!({"file_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000000}}),
            vec!["file_id"],
        ),
        (
            "calculate",
            "Compute arithmetic using decimal numbers, parentheses, +, -, *, /. This tool does not execute code.",
            json!({"expression":{"type":"string"}}),
            vec!["expression"],
        ),
    ];
    let brave_configured = std::env::var("BRAVE_SEARCH_API_KEY")
        .is_ok_and(|value| !value.is_empty())
        || std::env::var("FUNDING_SEARCH_ENDPOINT")
            .is_ok_and(|value| !value.is_empty() && value != BRAVE_ENDPOINT);
    definitions.into_iter().filter(|(name, _, _, _)| *name != "search_web" || brave_configured)
        .map(|(name,description,properties,required)| json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})).collect()
}
