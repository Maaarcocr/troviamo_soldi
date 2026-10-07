mod replay;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use funding_rust::{
    model::{ENDPOINT, ModelClient},
    pipeline::{self, RunOptions},
    sources,
    store::Store,
    types::Notice,
    web,
};
use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Rust + SQLite funding discovery, original-file analysis, and municipality screening"
)]
struct Cli {
    #[arg(long, global = true, default_value = "var/funding.sqlite")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Initialize the database and import the bundled official municipality evidence.
    Init,
    /// Discover and process at most X new/changed notices. Errors and reviews count toward X.
    #[command(alias = "daily")]
    Run {
        #[arg(long)]
        limit: usize,
        #[arg(long, default_value = "config/sources.json")]
        sources: PathBuf,
        /// JSON array of Notice records; bypass live discovery (files still fetched).
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long, default_value = "var/files")]
        cache: PathBuf,
        /// Fetch/hash and report candidates without any model request.
        #[arg(long)]
        dry_run: bool,
        /// Explicit operator opt-in for billable OpenRouter requests; needs OPENROUTER_API_KEY.
        #[arg(long)]
        allow_paid: bool,
        /// Test-only loopback endpoint, never sends an API key. Cannot combine with --allow-paid.
        #[arg(long)]
        mock_url: Option<String>,
        /// Reattempt an unchanged failed/review/in-flight version; may duplicate uncertain billing.
        #[arg(long)]
        retry_failed: bool,
    },
    /// Discover source records without making model calls.
    Discover {
        #[arg(long, default_value = "config/sources.json")]
        sources: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Serve a read-only, server-rendered UI and JSON endpoints (no browser API key).
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
    },
    /// Import project/fact JSON as user-declared evidence, scoped to its municipality.
    ImportFacts {
        file: PathBuf,
    },
    /// Export saved attempts as JSON, read-only; no discovery, API key, or model calls.
    ExportAttempts {
        /// Latest N attempts, newest first (1 to 1000).
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// New local file; refuses to overwrite any existing file. Default: stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Diagnose saved export/SQL rows offline; never opens or writes the runtime DB.
    Replay {
        file: PathBuf,
        /// New local file; refuses to overwrite any existing file. Default: stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Status,
}
fn client(timeout: Option<Duration>) -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .user_agent("FundingRust/0.1 (official funding source research)")
        .build()?)
}
fn config(path: &Path) -> Result<Value> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("Read {}", path.display()))?,
    )
    .context("Invalid sources JSON")
}
fn lock(path: &Path) -> Result<std::fs::File> {
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))?;
    f.try_lock_exclusive()
        .context("Another writer is running. No work started")?;
    Ok(f)
}
/// Create-new protects the database, its sidecars, and any prior export from
/// accidental replacement. Diagnostic files can contain private source content.
fn write_diagnostic(value: &Value, out: Option<&Path>) -> Result<()> {
    match out {
        Some(path) => {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path).with_context(|| {
                format!(
                    "Create {} (existing files are never overwritten)",
                    path.display()
                )
            })?;
            serde_json::to_writer(&mut file, value)?;
            file.write_all(b"\n")?;
            file.flush()?;
        }
        None => {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            serde_json::to_writer(&mut stdout, value)?;
            stdout.write_all(b"\n")?;
        }
    }
    Ok(())
}

fn reject_database_output(db: &Path, out: Option<&Path>) -> Result<()> {
    let Some(out) = out else {
        return Ok(());
    };
    let identity = |path: &Path| -> Result<PathBuf> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // A writable output's parent must exist. Existing parents are resolved
        // for symlink aliases as well as lexical `..` paths.
        let parent = if parent.exists() {
            parent.canonicalize()?
        } else {
            std::env::current_dir()?.join(parent)
        };
        Ok(parent.join(
            path.file_name()
                .context("Diagnostic path must name a file")?,
        ))
    };
    let out = identity(out)?;
    let mut protected = vec![db.to_path_buf(), db.with_extension("lock")];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut path = db.as_os_str().to_os_string();
        path.push(suffix);
        protected.push(PathBuf::from(path));
    }
    for path in protected {
        ensure!(
            out != identity(&path)?,
            "Diagnostic output must not be the database or one of its sidecar/lock files"
        );
    }
    Ok(())
}

/// Run before writer lock, initialization, seeding, configuration, or HTTP setup.
fn read_only_command(cli: &Cli) -> Result<bool> {
    match &cli.command {
        Command::ExportAttempts { limit, out } => {
            reject_database_output(&cli.db, out.as_deref())?;
            ensure!(
                (1..=funding_rust::store::MAX_DIAGNOSTIC_ATTEMPTS).contains(limit),
                "--limit must be between 1 and 1000"
            );
            let store = Store::open_read_only(&cli.db)?;
            write_diagnostic(
                &serde_json::to_value(store.export_attempts(*limit)?)?,
                out.as_deref(),
            )?;
            Ok(true)
        }
        Command::Replay { file, out } => {
            reject_database_output(&cli.db, out.as_deref())?;
            write_diagnostic(&replay::from_file(file)?, out.as_deref())?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if read_only_command(&cli)? {
        return Ok(());
    }
    if let Command::Serve { bind } = cli.command {
        return web::serve(&cli.db, &bind);
    }
    let _lock = lock(&cli.db)?;
    let mut store = Store::open(&cli.db)?;
    store.seed()?;
    match cli.command {
        Command::Init => println!("Initialized {}", cli.db.display()),
        Command::Status => println!("{}", serde_json::to_string_pretty(&store.stats()?)?),
        Command::ImportFacts { file } => {
            store.import_facts(&config(&file)?)?;
            println!("Imported scoped, user-declared facts");
        }
        Command::Discover {
            sources: source_path,
            out,
        } => {
            let notices = sources::discover(
                &client(Some(Duration::from_secs(120)))?,
                &config(&source_path)?,
            )?;
            std::fs::write(&out, serde_json::to_vec_pretty(&notices)?)?;
            println!(
                "Discovered {} records → {}. Coverage is limited to configured sources.",
                notices.len(),
                out.display()
            );
        }
        Command::Run {
            limit,
            sources: source_path,
            input,
            cache,
            dry_run,
            allow_paid,
            mock_url,
            retry_failed,
        } => {
            ensure!(limit > 0, "--limit must be greater than zero");
            ensure!(
                !(allow_paid && mock_url.is_some()),
                "--allow-paid and --mock-url are mutually exclusive"
            );
            ensure!(
                dry_run || allow_paid || mock_url.is_some(),
                "No model call authorized: use --dry-run, test --mock-url, or explicitly --allow-paid after setting a budget/key"
            );
            let allow_localhost = mock_url.is_some();
            let model = if dry_run {
                None
            } else {
                let (endpoint, api_key) = if let Some(url) = mock_url {
                    let parsed = url::Url::parse(&url)?;
                    ensure!(
                        ["localhost", "127.0.0.1", "[::1]", "::1"]
                            .contains(&parsed.host_str().unwrap_or(""))
                            && ["http", "https"].contains(&parsed.scheme())
                            && parsed.username().is_empty()
                            && parsed.password().is_none(),
                        "Mock endpoint must be loopback HTTP(S) without credentials"
                    );
                    (url, None)
                } else {
                    let key = std::env::var("OPENROUTER_API_KEY").context(
                        "Set OPENROUTER_API_KEY on the server; never in browser or source files",
                    )?;
                    ensure!(!key.trim().is_empty(), "Empty API key");
                    (ENDPOINT.into(), Some(key))
                };
                Some(ModelClient {
                    http: client(None)?,
                    endpoint,
                    api_key,
                })
            };
            if retry_failed {
                eprintln!(
                    "Explicit retry enabled: an earlier uncertain request may already have been billed. One call per notice this run."
                );
            }
            let configuration = config(&source_path)?;
            let allowed_hosts = configuration["allowed_hosts"]
                .as_array()
                .context("sources config needs allowed_hosts array")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .context("allowed_hosts entries must be strings")
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>>>()?;
            let http = client(Some(Duration::from_secs(120)))?;
            let notices: Vec<Notice> = if let Some(input) = input {
                serde_json::from_slice(&std::fs::read(input)?)?
            } else {
                sources::discover(&http, &configuration)?
            };
            let summary = pipeline::run(
                &mut store,
                &http,
                model.as_ref(),
                notices,
                &RunOptions {
                    limit,
                    cache_dir: cache,
                    allowed_hosts,
                    allow_localhost,
                    retry_failed,
                    dry_run,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::Serve { .. } | Command::ExportAttempts { .. } | Command::Replay { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

#[cfg(test)]
mod diagnostic_cli_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_never_creates_or_opens_configured_database() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("saved.json");
        let output = temp.path().join("report.json");
        let db = temp.path().join("nonexistent/runtime.sqlite");
        std::fs::write(&input, r#"[{"attempt_id":1,"notice_id":"notice-1","response":null,"validation_error":"transport error"}]"#).unwrap();
        let cli = Cli {
            db: db.clone(),
            command: Command::Replay {
                file: input,
                out: Some(output.clone()),
            },
        };
        assert!(read_only_command(&cli).unwrap());
        assert!(!db.parent().unwrap().exists());
        let report: Value = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(report["model_calls"], 0);
        assert_eq!(report["database_writes"], 0);
        assert_eq!(report["results"][0]["category"], "no_saved_response");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn exports_and_replays_local_database_without_initialization_or_changes() {
        use funding_rust::types::Usage;
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("runtime.sqlite");
        let export = temp.path().join("attempts.json");
        let report = temp.path().join("report.json");
        let mut store = Store::open(&db).unwrap();
        let notice = Notice {
            id: "call-1".into(),
            title: "Fixture".into(),
            source_url: "https://example.gov/call.pdf".into(),
            source_text: Some("Official source".into()),
            documents: vec![],
        };
        let version = store.begin_version(&notice, "fingerprint", &[]).unwrap();
        let attempt = store.attempt(version, "request-hash").unwrap();
        let extraction: Value =
            serde_json::from_str(include_str!("../tests/fixtures/extraction.json")).unwrap();
        let response = json!({"model":funding_rust::model::MODEL,"choices":[{"finish_reason":"stop","message":{"content":extraction.to_string()}}],"usage":{"cost":0.005}});
        store
            .finish_attempt(
                attempt,
                "needs_review",
                Some(&response),
                &Usage::from_response(&response),
                Some("old contract error"),
            )
            .unwrap();
        let before = store.stats().unwrap();
        drop(store);
        let before_bytes = std::fs::read(&db).unwrap();
        assert!(
            read_only_command(&Cli {
                db: db.clone(),
                command: Command::ExportAttempts {
                    limit: 1,
                    out: Some(export.clone())
                }
            })
            .unwrap()
        );
        assert!(
            read_only_command(&Cli {
                db: db.clone(),
                command: Command::Replay {
                    file: export,
                    out: Some(report.clone())
                }
            })
            .unwrap()
        );
        let actual: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
        assert_eq!(actual["results"][0]["category"], "valid_extraction");
        assert_eq!(
            actual["results"][0]["stored_validation_error"],
            "old contract error"
        );
        assert_eq!(actual["results"][0]["saved_usage"]["cost_usd"], 0.005);
        let store = Store::open_read_only(&db).unwrap();
        assert_eq!(store.stats().unwrap(), before);
        drop(store);
        assert_eq!(std::fs::read(&db).unwrap(), before_bytes);
        assert!(!db.with_extension("lock").exists());
    }

    #[test]
    fn output_cannot_replace_input_database_or_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("existing.json");
        std::fs::write(&existing, b"keep me").unwrap();
        assert!(write_diagnostic(&json!({}), Some(&existing)).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
        let db = temp.path().join("runtime.sqlite");
        assert!(reject_database_output(&db, Some(&db)).is_err());
        assert!(
            reject_database_output(&db, Some(&temp.path().join("runtime.sqlite-wal"))).is_err()
        );
        assert!(reject_database_output(&db, Some(&db.with_extension("lock"))).is_err());
        assert!(reject_database_output(&db, Some(&temp.path().join("report.json"))).is_ok());
    }

    #[test]
    fn invalid_export_limit_does_not_create_missing_database() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("missing/runtime.sqlite");
        for limit in [0, 1001] {
            assert!(
                read_only_command(&Cli {
                    db: db.clone(),
                    command: Command::ExportAttempts { limit, out: None }
                })
                .is_err()
            );
        }
        assert!(!db.parent().unwrap().exists());
    }
}
