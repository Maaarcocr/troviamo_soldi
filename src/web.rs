//! Small, read-only, server-rendered interface. No browser credentials or JavaScript.
use crate::{engine, store::Store};
use anyhow::Result;
use chrono::Utc;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fmt::Write, path::Path};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use url::Url;

const CSP: &str = "default-src 'none'; style-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'";

/// The caller defaults to 127.0.0.1. This is an unauthenticated local interface,
/// not a public deployment; all routes are GET-only and perform no model calls.
pub fn serve(db_path: &Path, bind: &str) -> Result<()> {
    let store = Store::open(db_path)?;
    let server = Server::http(bind).map_err(|e| anyhow::anyhow!("Cannot listen on {bind}: {e}"))?;
    eprintln!("Read-only funding UI: http://{bind} (no authentication; keep it local)");
    for request in server.incoming_requests() {
        let host = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("Host"))
            .map(|h| h.value.as_str());
        let reply = if !host_allowed(host, bind) {
            Reply::error(
                403,
                "Host non consentito. Apri l’indirizzo locale del servizio.",
                false,
            )
        } else if request.method() != &Method::Get {
            Reply::error(
                405,
                "Interfaccia di sola lettura: sono consentite soltanto richieste GET.",
                request.url().starts_with("/api/"),
            )
        } else {
            route(&store, request.url())
        };
        let mut response =
            Response::from_string(reply.body).with_status_code(StatusCode(reply.status));
        for (name, value) in [
            ("Content-Type", reply.content_type),
            ("Content-Security-Policy", CSP),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            ("Cache-Control", "no-store"),
            ("X-Frame-Options", "DENY"),
            (
                "Permissions-Policy",
                "camera=(), microphone=(), geolocation=()",
            ),
        ] {
            response.add_header(
                Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header"),
            );
        }
        if reply.status == 405 {
            response.add_header(Header::from_bytes("Allow", "GET").expect("static header"));
        }
        if let Err(error) = request.respond(response) {
            eprintln!("UI response interrupted: {error}");
        }
    }
    Ok(())
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
}
impl Reply {
    fn html(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body,
        }
    }
    fn json(value: &Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: value.to_string(),
        }
    }
    fn error(status: u16, message: &str, api: bool) -> Self {
        let mut reply = if api {
            Self::json(&json!({"error":message}))
        } else {
            Self::html(shell(
                "Richiesta non disponibile",
                &format!(
                    "<main class=\"shell\"><section class=\"empty\"><p class=\"eyebrow\">Richiesta non disponibile</p><h1>Riproviamo da qui.</h1><p>{}</p><a class=\"button\" href=\"/\">Torna ai bandi</a></section></main>",
                    escape(message)
                ),
            ))
        };
        reply.status = status;
        reply
    }
}

fn route(store: &Store, target: &str) -> Reply {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let api = path.starts_with("/api/");
    if target.len() > 8192 || !path.starts_with('/') || path.starts_with("//") {
        return Reply::error(400, "Indirizzo della richiesta non valido.", api);
    }
    if path == "/style.css" {
        return Reply {
            status: 200,
            content_type: "text/css; charset=utf-8",
            body: CSS.into(),
        };
    }
    if !matches!(path, "/" | "/api/municipalities" | "/api/opportunities") {
        return Reply::error(404, "Questa pagina non esiste.", api);
    }
    if path == "/api/municipalities" {
        return match store.municipalities() {
            Ok(municipalities) => Reply::json(&json!({"municipalities":municipalities})),
            Err(error) => storage_error(error, true),
        };
    }
    let query = match Query::parse(query) {
        Ok(query) => query,
        Err(message) => return Reply::error(400, &message, api),
    };
    match load_view(store, query) {
        Ok(view) => {
            if api {
                Reply::json(&view.as_json())
            } else {
                Reply::html(render(&view))
            }
        }
        Err(ViewError::Input(message)) => Reply::error(400, &message, api),
        Err(ViewError::Storage(error)) => storage_error(error, api),
    }
}

fn storage_error(error: anyhow::Error, api: bool) -> Reply {
    eprintln!("UI storage error: {error:#}");
    Reply::error(
        500,
        "Non riesco a leggere i dati locali. Controlla il servizio e riprova.",
        api,
    )
}

// Prevent a foreign hostname from rebinding to the default loopback service.
// An explicit wildcard bind opts into network access and needs its own access controls.
fn host_allowed(host: Option<&str>, bind: &str) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Ok(actual) = Url::parse(&format!("http://{host}")) else {
        return false;
    };
    let Ok(expected) = Url::parse(&format!("http://{bind}")) else {
        return false;
    };
    if !actual.username().is_empty()
        || actual.password().is_some()
        || actual.path() != "/"
        || actual.query().is_some()
        || actual.fragment().is_some()
    {
        return false;
    }
    if expected.port_or_known_default() != actual.port_or_known_default() {
        return false;
    }
    let expected_host = expected.host_str().unwrap_or("");
    let actual_host = actual.host_str().unwrap_or("");
    matches!(expected_host, "0.0.0.0" | "[::]")
        || expected_host == actual_host
        || (matches!(expected_host, "127.0.0.1" | "[::1]" | "localhost")
            && matches!(actual_host, "127.0.0.1" | "[::1]" | "localhost"))
}

#[derive(Default, Debug)]
struct Query {
    municipality: Option<String>,
    project: Option<String>,
    archive: bool,
}
impl Query {
    fn parse(query: &str) -> std::result::Result<Self, String> {
        let mut params = BTreeMap::new();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if !matches!(key.as_ref(), "municipality" | "project" | "archive") {
                continue;
            }
            if params
                .insert(key.into_owned(), value.into_owned())
                .is_some()
            {
                return Err("Un filtro è ripetuto nella richiesta.".into());
            }
        }
        let archive = match params.remove("archive").as_deref() {
            None | Some("") | Some("0") => false,
            Some("1") => true,
            _ => return Err("Il filtro archivio deve essere 0 oppure 1.".into()),
        };
        Ok(Self {
            municipality: params.remove("municipality").filter(|s| !s.is_empty()),
            project: params.remove("project").filter(|s| !s.is_empty()),
            archive,
        })
    }
}

enum ViewError {
    Input(String),
    Storage(anyhow::Error),
}
impl From<anyhow::Error> for ViewError {
    fn from(error: anyhow::Error) -> Self {
        Self::Storage(error)
    }
}

struct View {
    municipalities: Vec<Value>,
    municipality: Option<Value>,
    projects: Vec<Value>,
    project: Option<String>,
    evidence: Vec<Value>,
    opportunities: Vec<Value>,
    stats: Value,
    archive: bool,
    total: usize,
    hidden: usize,
    as_of: String,
}
impl View {
    fn as_json(&self) -> Value {
        json!({"municipality":self.municipality,"project":self.project,"as_of":self.as_of,
            "archive":self.archive,"total":self.total,"hidden":self.hidden,
            "opportunities":self.opportunities,"evidence":self.evidence,
            "coverage_note":"Fonti iniziali: EU Funding & Tenders ed EuroInfoSicilia. Il catalogo non è completo."})
    }
}

fn load_view(store: &Store, query: Query) -> std::result::Result<View, ViewError> {
    let municipalities = store.municipalities()?;
    let municipality = if let Some(id) = query.municipality.as_deref() {
        Some(
            municipalities
                .iter()
                .find(|m| municipality_id(m) == id)
                .ok_or_else(|| ViewError::Input("Comune non presente nel registro locale.".into()))?
                .clone(),
        )
    } else {
        municipalities.first().cloned()
    };
    let (projects, evidence) = if let Some(municipality) = &municipality {
        let id = municipality_id(municipality);
        (store.projects(id)?, store.evidence(id)?)
    } else {
        (vec![], vec![])
    };
    if let Some(id) = query.project.as_deref()
        && !projects.iter().any(|p| p["id"].as_str() == Some(id))
    {
        return Err(ViewError::Input("Progetto non presente per il Comune selezionato. Scegli di nuovo il Comune per azzerare il filtro.".into()));
    }
    // Exact time matters for application cutoffs on the current date.
    let as_of = Utc::now().to_rfc3339();
    let notices = store.latest_notices()?;
    let total = notices.len();
    let mut opportunities: Vec<Value> = notices
        .into_iter()
        .map(|notice| {
            evaluate_notice(
                notice,
                municipality.as_ref(),
                &evidence,
                query.project.as_deref(),
                &as_of,
            )
        })
        .filter(|notice| should_show(&notice["evaluation"], query.archive))
        .collect();
    opportunities.sort_by_key(|n| {
        (
            state_order(text(&n["evaluation"], "state")),
            text(n, "title").to_lowercase(),
        )
    });
    let hidden = total - opportunities.len();
    Ok(View {
        municipalities,
        municipality,
        projects,
        project: query.project,
        evidence,
        opportunities,
        stats: store.stats()?,
        archive: query.archive,
        total,
        hidden,
        as_of,
    })
}

fn evaluate_notice(
    mut notice: Value,
    municipality: Option<&Value>,
    evidence: &[Value],
    project: Option<&str>,
    as_of: &str,
) -> Value {
    let extraction_ready = matches!(text(&notice, "state"), "screened" | "needs_review")
        && notice["extraction"].is_object();
    let mut evaluation = if extraction_ready {
        if let Some(municipality) = municipality {
            engine::evaluate_for_call(
                &notice["extraction"],
                municipality,
                evidence,
                project,
                notice["id"].as_str(),
                as_of,
            )
        } else {
            json!({"state":"needs_review","visible":true,"reason":"Importa il registro dei Comuni per iniziare la verifica."})
        }
    } else {
        // Never reuse an older successful extraction during a pending/failed retry.
        notice["extraction"] = Value::Null;
        json!({"state":"needs_review","visible":true,"reason":"L’ultima versione non ha un’estrazione utilizzabile. Occorre completare o ripetere l’elaborazione."})
    };
    let source_fresh = source_is_fresh(text(&notice, "updated_at"), as_of);
    evaluation["source_freshness"] = json!({"state":if source_fresh { "fresh" } else { "stale" },"checked_at":notice["updated_at"],"max_age_days":7});
    if !source_fresh {
        if evaluation["state"] == "screening_match" {
            evaluation["state"] = json!("review_required");
            evaluation["visible"] = json!(true);
        }
        evaluation["needs_review"] = json!(true);
        if !evaluation["review_reasons"].is_array() {
            evaluation["review_reasons"] = json!([]);
        }
        evaluation["review_reasons"].as_array_mut().expect("review reasons array").push(json!("Fonte da riconfermare: l’ultimo controllo risale a oltre 7 giorni fa oppure ha una data assente, non valida o futura. Verifica eventuali rettifiche e riaperture."));
    }
    notice["evaluation"] = evaluation;
    // Provider errors belong in local logs, not in an unauthenticated response.
    notice
        .as_object_mut()
        .expect("stored notice object")
        .remove("error");
    notice
}

fn source_is_fresh(checked_at: &str, as_of: &str) -> bool {
    let Ok(checked_at) = chrono::DateTime::parse_from_rfc3339(checked_at) else {
        return false;
    };
    let now = chrono::DateTime::parse_from_rfc3339(as_of)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(as_of, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc())
        });
    now.is_some_and(|now| {
        let age = now.signed_duration_since(checked_at.with_timezone(&Utc));
        age >= chrono::Duration::zero() && age <= chrono::Duration::days(7)
    })
}

fn should_show(evaluation: &Value, archive: bool) -> bool {
    // Only affirmative closure or incompatibility may remove an item. Unknown,
    // review and future states remain visible even if a malformed flag is false.
    archive
        || !matches!(
            text(evaluation, "state"),
            "closed" | "ineligible" | "excluded"
        )
}

fn state_order(state: &str) -> u8 {
    match state {
        "screening_match" => 0,
        "closed" | "ineligible" | "excluded" => 2,
        _ => 1,
    }
}

fn state_label(state: &str) -> (&'static str, &'static str) {
    match state {
        "screening_match" => ("Coerente con i dati disponibili", "good"),
        "forthcoming" => ("Di prossima apertura", "warn"),
        "closed" => ("Termine chiuso", "muted"),
        "ineligible" | "excluded" => ("Non compatibile", "muted"),
        "blocked" | "fail" => ("Requisito non soddisfatto", "warn"),
        "pass" => ("Verificato sui dati", "good"),
        "declared" => ("Dato dichiarato", "warn"),
        "conflict" => ("Evidenze in conflitto", "warn"),
        "stale" => ("Da aggiornare", "warn"),
        "unknown" => ("Dato mancante", "warn"),
        _ => ("Da verificare", "warn"),
    }
}

fn render(view: &View) -> String {
    let mut body = String::with_capacity(32_000);
    body.push_str("<a class=\"skip\" href=\"#opportunita\">Vai ai bandi</a><header class=\"topbar\"><div class=\"shell nav\"><a class=\"brand\" href=\"/\"><span class=\"brand-mark\" aria-hidden=\"true\">c</span>comuni<span class=\"brand-light\"> / bandi</span></a><span class=\"local\"><span aria-hidden=\"true\">●</span> Archivio locale · sola lettura</span></div></header><main class=\"shell\"><section class=\"intro\"><p class=\"eyebrow\">FONDI, CONTESTO, EVIDENZE</p><h1>Le opportunità partono<br>dal tuo Comune.</h1><p class=\"lede\">Un punto di partenza per trovare i bandi, capire i requisiti e sapere che cosa manca prima di candidarsi.</p></section>");
    body.push_str("<section class=\"context panel\" aria-labelledby=\"contesto\"><div class=\"section-title\"><div><p class=\"eyebrow\">IL CONTESTO</p><h2 id=\"contesto\">Comune e progetto</h2></div><span class=\"quiet\">Nessun dato viene inviato al modello da questa pagina</span></div><div class=\"filters\"><form method=\"get\" action=\"/\"><label for=\"municipality\">Comune</label><div class=\"input-row\"><select id=\"municipality\" name=\"municipality\">");
    let selected_id = view
        .municipality
        .as_ref()
        .map(municipality_id)
        .unwrap_or("");
    if view.municipalities.is_empty() {
        body.push_str("<option value=\"\">Registro non ancora importato</option>");
    }
    for municipality in &view.municipalities {
        let id = municipality_id(municipality);
        let _ = write!(
            body,
            "<option value=\"{}\"{}>{} ({})</option>",
            escape(id),
            selected(id == selected_id),
            escape(text(municipality, "name")),
            escape(text(municipality, "provinceAbbreviation"))
        );
    }
    body.push_str("</select><button type=\"submit\" class=\"secondary\">Scegli Comune</button></div><p class=\"help\">Cambiare Comune azzera il progetto selezionato.</p></form><form method=\"get\" action=\"/\"><label for=\"project\">Progetto</label><div class=\"input-row\">");
    let _ = write!(
        body,
        "<input type=\"hidden\" name=\"municipality\" value=\"{}\"><select id=\"project\" name=\"project\"><option value=\"\">Solo dati del Comune</option>",
        escape(selected_id)
    );
    for project in &view.projects {
        let id = text(project, "id");
        let _ = write!(
            body,
            "<option value=\"{}\"{}>{}</option>",
            escape(id),
            selected(view.project.as_deref() == Some(id)),
            escape(text(project, "name"))
        );
    }
    let _ = write!(
        body,
        "</select><button type=\"submit\">Aggiorna vista</button></div><label class=\"checkbox\"><input type=\"checkbox\" name=\"archive\" value=\"1\"{}> Includi chiusi e non compatibili</label></form></div>",
        if view.archive { " checked" } else { "" }
    );
    if let Some(municipality) = &view.municipality {
        let _ = write!(
            body,
            "<div class=\"context-foot\"><span><strong>{}</strong> · {} · ISTAT {}</span><span>Registro: {}</span></div>",
            escape(text(municipality, "name")),
            escape(text(municipality, "province")),
            escape(selected_id),
            escape(text(municipality, "registryReferenceDate"))
        );
    }
    body.push_str("</section>");
    let review_count = view
        .opportunities
        .iter()
        .filter(|n| state_order(text(&n["evaluation"], "state")) == 1)
        .count();
    let checked_at = chrono::DateTime::parse_from_rfc3339(&view.as_of)
        .map(|d| d.format("%d/%m/%Y, %H:%M UTC").to_string())
        .unwrap_or_else(|_| view.as_of.clone());
    let _ = write!(
        body,
        "<div class=\"summary-strip\"><div><strong>{}</strong><span>bandi in questa vista</span></div><div><strong>{}</strong><span>da approfondire</span></div><div><strong>{}</strong><span>nascosti dal filtro</span></div><p>Verifica al {}.<br>La coerenza dei dati non certifica l’ammissibilità.</p></div>",
        view.opportunities.len(),
        review_count,
        view.hidden,
        escape(&checked_at)
    );
    body.push_str("<div class=\"workspace\"><section id=\"opportunita\" class=\"opportunities\"><div class=\"section-title\"><div><p class=\"eyebrow\">DA ESPLORARE</p><h2>Bandi e avvisi</h2></div><span class=\"quiet\">Ultima versione disponibile</span></div>");
    if view.opportunities.is_empty() {
        body.push_str(
            "<div class=\"empty panel\"><span class=\"empty-icon\" aria-hidden=\"true\">↗</span>",
        );
        if view.total == 0 {
            body.push_str("<h3>Il tuo archivio è pronto a partire.</h3><p>Non ci sono ancora avvisi elaborati. Esegui la raccolta dal servizio Rust per popolare questa vista.</p>");
        } else {
            body.push_str("<h3>Nessun bando in questa vista.</h3><p>Attiva «Includi chiusi e non compatibili» per consultare anche l’archivio.</p>");
        }
        body.push_str("</div>");
    }
    for notice in &view.opportunities {
        render_notice(&mut body, notice);
    }
    body.push_str("</section><aside><section class=\"panel aside-panel\"><p class=\"eyebrow\">COME LEGGERE I RISULTATI</p><h2>Prima di candidarsi</h2><ol class=\"steps\"><li><strong>Parti dall’avviso ufficiale</strong><p>Controlla allegati, rettifiche e termine vigente.</p></li><li><strong>Completa le evidenze</strong><p>I dati mancanti o scaduti restano da verificare. Non diventano un sì.</p></li><li><strong>Confrontati con l’ufficio</strong><p>Valida condizioni tecniche, finanziarie e giuridiche prima della domanda.</p></li></ol></section><section class=\"coverage\"><p class=\"eyebrow\">COPERTURA INIZIALE</p><h3>Una selezione, in crescita.</h3><p>Fonti iniziali: EU Funding &amp; Tenders ed EuroInfoSicilia. Non è un catalogo completo dei finanziamenti.</p><p>La pagina legge i dati salvati: non aggiorna automaticamente fonti o scadenze.</p></section></aside></div>");
    render_evidence(&mut body, view);
    let attempts = view.stats["attempts"].as_u64().unwrap_or(0);
    let _ = write!(
        body,
        "<footer><p>Comuni / bandi · Rust + SQLite · {} versioni archiviate · {} tentativi di elaborazione</p><p>Servizio locale senza autenticazione. Non esporlo pubblicamente senza controlli di accesso.</p></footer></main>",
        view.stats["versions"].as_u64().unwrap_or(0),
        attempts
    );
    shell("Comuni / bandi", &body)
}

fn render_notice(out: &mut String, notice: &Value) {
    let evaluation = &notice["evaluation"];
    let extraction = &notice["extraction"];
    let (label, color) = state_label(text(evaluation, "state"));
    let _ = write!(
        out,
        "<article class=\"notice panel\"><div class=\"notice-top\"><span class=\"badge {color}\">{label}</span><span class=\"quiet\">Versione {}</span></div><h3>{}</h3>",
        escape(&display_value(&notice["version"])),
        escape(text(notice, "title"))
    );
    if let Some(summary) = extraction["summary"].as_str().filter(|s| !s.is_empty()) {
        let _ = write!(out, "<p class=\"notice-summary\">{}</p>", escape(summary));
    }
    if let Some(reason) = evaluation["reason"].as_str() {
        let _ = write!(out, "<p class=\"reason\">{}</p>", escape(reason));
    }
    if let Some(deadline) = extraction["closes_at"].as_str() {
        let _ = write!(
            out,
            "<p class=\"deadline\">Termine estratto: {}</p>",
            escape(deadline)
        );
    }
    if let Some(reasons) = evaluation["review_reasons"]
        .as_array()
        .filter(|a| !a.is_empty())
    {
        out.push_str("<ul class=\"review-reasons\">");
        for reason in reasons {
            if let Some(reason) = reason.as_str() {
                let _ = write!(out, "<li>{}</li>", escape(reason));
            }
        }
        out.push_str("</ul>");
    }
    let _ = write!(
        out,
        "<div class=\"notice-meta\">{}<span>Acquisito: {}</span></div>",
        source_link(text(notice, "source_url"), "Avviso ufficiale ↗"),
        escape(text(notice, "updated_at"))
    );
    let results = evaluation["results"]
        .as_array()
        .or_else(|| evaluation["requirements"].as_array());
    if let Some(results) = results.filter(|r| !r.is_empty()) {
        let _ = write!(
            out,
            "<details class=\"checks\"><summary>Requisiti e motivazioni <span>{}</span></summary><ul>",
            results.len()
        );
        for result in results {
            let (label, color) = state_label(text(result, "state"));
            let _ = write!(
                out,
                "<li><div><strong>{}</strong><span class=\"badge {color}\">{label}</span></div><p>{}</p>",
                escape(text(result, "label")),
                escape(text(result, "reason"))
            );
            if let Some(source) = result["source"].as_str() {
                out.push_str(&source_link(source, "Fonte del requisito ↗"));
            }
            out.push_str("</li>");
        }
        out.push_str("</ul></details>");
    }
    if let Some(citations) = extraction["citations"].as_array().filter(|a| !a.is_empty()) {
        let _ = write!(
            out,
            "<details class=\"citations\"><summary>Fonti e riferimenti <span>({})</span></summary><p class=\"help\">Citazioni riportate dal modello: confrontale con i documenti originali. Il testo citato non è verificato automaticamente.</p><ol>",
            citations.len()
        );
        for citation in citations {
            let _ = write!(
                out,
                "<li><p>{} · {}</p><blockquote>{}</blockquote><span class=\"quiet\">Riferimento: {}</span></li>",
                source_link(text(citation, "source_url"), "Documento originale ↗"),
                escape(text(citation, "locator")),
                escape(text(citation, "quote")),
                escape(text(citation, "id"))
            );
        }
        out.push_str("</ol></details>");
    }
    let _ = write!(
        out,
        "<details class=\"technical\"><summary>Dati e traccia della verifica</summary><p>Identificativo avviso: {}</p><h4>Valutazione</h4><pre>{}</pre>",
        escape(text(notice, "id")),
        escape(&pretty(evaluation))
    );
    if !extraction.is_null() {
        let _ = write!(
            out,
            "<h4>Estrazione dell’ultima versione</h4><pre>{}</pre>",
            escape(&pretty(extraction))
        );
    } else {
        out.push_str("<p>Nessuna estrazione utilizzabile per l’ultima versione. Un eventuale esito precedente non viene riutilizzato.</p>");
    }
    out.push_str("</details></article>");
}

fn render_evidence(out: &mut String, view: &View) {
    let records: Vec<&Value> = view
        .evidence
        .iter()
        .filter(|e| e["projectId"].is_null() || e["projectId"].as_str() == view.project.as_deref())
        .collect();
    let fields: Value =
        serde_json::from_str(include_str!("../data/fields.json")).expect("bundled field registry");
    let _ = write!(
        out,
        "<section class=\"evidence panel\" aria-labelledby=\"evidenze\"><div class=\"section-title\"><div><p class=\"eyebrow\">LA BASE DELLA VERIFICA</p><h2 id=\"evidenze\">Evidenze del contesto</h2></div><span class=\"quiet\">{} record</span></div><p>Progetti ed evidenze si aggiungono con l’importazione da CLI. I dati importati dall’utente restano dichiarazioni da verificare; questa pagina non modifica il database.</p>",
        records.len()
    );
    if records.is_empty() {
        out.push_str("<p class=\"reason\">Non ci sono evidenze per questo contesto. I requisiti che le richiedono rimangono da verificare.</p>");
    }
    for record in records {
        let field = text(record, "field");
        let label = fields[field]["label"].as_str().unwrap_or(field);
        let provenance = match text(record, "provenance") {
            "official" => "Fonte ufficiale",
            "user_declared" => "Dichiarazione utente",
            _ => "Provenienza da verificare",
        };
        let _ = write!(
            out,
            "<details class=\"evidence-row\"><summary><span>{}</span><strong>{}</strong><span class=\"quiet\">{provenance}</span></summary><div class=\"evidence-detail\"><p>Rilevato il {} · valido fino al {}</p>",
            escape(label),
            escape(&display_value(&record["value"])),
            escape(text(record, "observedAt")),
            escape(
                record["validUntil"]
                    .as_str()
                    .unwrap_or("termine non indicato")
            )
        );
        let source = &record["source"];
        let _ = write!(
            out,
            "<p>{} · {}</p>",
            source_link(text(source, "url"), text(source, "title")),
            escape(text(source, "locator"))
        );
        if record["refreshRequired"] == true {
            out.push_str("<p class=\"reason\">La fonte richiede una riconferma. Un dato storico non prova la situazione attuale.</p>");
        }
        if let Some(call) = record["callId"].as_str() {
            let _ = write!(out, "<p>Valido per l’avviso: {}</p>", escape(call));
        }
        if let Some(project) = record["projectId"].as_str() {
            let _ = write!(out, "<p>Progetto: {}</p>", escape(project));
        }
        if let Some(note) = record["note"].as_str() {
            let _ = write!(out, "<p>{}</p>", escape(note));
        }
        out.push_str("</div></details>");
    }
    out.push_str("</section>");
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"it\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta name=\"color-scheme\" content=\"light\"><title>{}</title><link rel=\"stylesheet\" href=\"/style.css\"></head><body>{body}</body></html>",
        escape(title)
    )
}
fn municipality_id(value: &Value) -> &str {
    value["istatCode"]
        .as_str()
        .or_else(|| value["id"].as_str())
        .unwrap_or("")
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
fn selected(yes: bool) -> &'static str {
    if yes { " selected" } else { "" }
}
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "Dati non leggibili".into())
}
fn display_value(value: &Value) -> String {
    match value {
        Value::Null => "Non disponibile".into(),
        Value::Bool(true) => "Sì".into(),
        Value::Bool(false) => "No".into(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
fn escape(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}
fn safe_url(input: &str) -> Option<Url> {
    let url = Url::parse(input).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}
fn source_link(url: &str, label: &str) -> String {
    match safe_url(url) {
        Some(url) => format!(
            "<a href=\"{}\" rel=\"noreferrer noopener\">{}</a>",
            escape(url.as_str()),
            escape(label)
        ),
        None => format!(
            "<span>{}</span>",
            escape(if label.is_empty() {
                "Fonte non disponibile"
            } else {
                label
            })
        ),
    }
}

const CSS: &str = r#"
.review-reasons{font-size:12px;color:#785723;padding-left:19px;line-height:1.7}.citations ol{padding-left:20px}.citations li{padding:10px 0;font-size:12px}.citations li p{margin-bottom:8px}.citations blockquote{margin:0 0 8px;padding:8px 12px;border-left:2px solid #cbd4c7;color:#66746c;background:#f5f7f0}.citations summary>span{font-weight:400;color:#66746c}
:root{--bg:#f5f5ee;--paper:#fffefa;--ink:#203b35;--muted:#66746c;--line:#dfe4d9;--green:#28634c;--soft:#eaf0e5;--amber:#785723;--amber-bg:#f5ecd8;font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:var(--ink);background:var(--bg);font-synthesis:none}*{box-sizing:border-box}body{margin:0;font-size:14px;line-height:1.6}a{color:var(--green);text-underline-offset:3px}button,select,input{font:inherit}button,.button{background:var(--green);color:white;border:1px solid var(--green);border-radius:8px;padding:10px 15px;font-weight:600;cursor:pointer;white-space:nowrap;text-decoration:none}button:hover,.button:hover{background:#1e4e3b}button.secondary{background:var(--paper);color:var(--ink);border-color:var(--line)}button.secondary:hover{background:var(--soft)}:focus-visible{outline:3px solid #bd812c;outline-offset:3px}h1,h2,h3,h4,p{margin-top:0}h1,h2,h3,h4{line-height:1.2;letter-spacing:-.035em}h1{font-family:Georgia,"Times New Roman",serif;font-weight:400;font-size:clamp(38px,5.6vw,66px);margin-bottom:18px}h2{font-size:23px;margin-bottom:0}h3{font-size:22px;margin-bottom:14px}h4{font-size:16px;margin-bottom:8px}.shell{width:min(1200px,calc(100% - 64px));margin:0 auto}.topbar{border-bottom:1px solid var(--line);background:#f9faf5}.nav{height:86px;display:flex;align-items:center;justify-content:space-between;gap:20px}.brand{font-size:24px;letter-spacing:-.06em;font-weight:700;display:flex;align-items:center;text-decoration:none;color:var(--ink)}.brand-light{font-weight:400;color:var(--muted);margin-left:6px}.brand-mark{display:inline-grid;place-items:center;width:35px;height:35px;color:var(--paper);background:var(--green);font-family:Georgia,serif;font-size:29px;margin-right:12px;border-radius:10px}.local{font-size:12px;color:var(--muted)}.local span{font-size:9px;color:#4d8464;margin-right:6px}.intro{padding:56px 0 38px}.eyebrow{font-size:10px;letter-spacing:.15em;font-weight:700;color:var(--muted);margin-bottom:11px}.lede{font-size:17px;line-height:1.65;color:var(--muted);max-width:620px;margin-bottom:0}.panel{background:var(--paper);border:1px solid var(--line);border-radius:14px}.context{padding:28px 30px}.section-title{display:flex;align-items:center;justify-content:space-between;gap:18px;margin-bottom:23px}.section-title .eyebrow{margin-bottom:6px}.quiet{color:var(--muted);font-size:11px}.context>.section-title>.quiet{max-width:210px;line-height:1.5;text-align:right}.filters{display:grid;grid-template-columns:1fr 1fr;gap:28px}.filters form{min-width:0}.filters label:not(.checkbox){display:block;font-size:12px;font-weight:600;margin-bottom:8px}.input-row{display:flex;gap:8px}select{width:100%;min-width:0;background:white;border:1px solid #cbd4c7;border-radius:8px;padding:11px 12px;color:var(--ink)}.help{font-size:11px;color:var(--muted);margin:8px 0 0}.checkbox{display:flex;align-items:center;gap:7px;color:var(--muted);font-size:11px;margin-top:9px}.checkbox input{accent-color:var(--green);width:15px;height:15px}.context-foot{border-top:1px solid var(--line);margin-top:24px;padding-top:15px;display:flex;justify-content:space-between;gap:15px;color:var(--muted);font-size:12px}.context-foot strong{color:var(--ink);font-weight:600}.summary-strip{display:flex;align-items:center;padding:27px 5px 31px;gap:30px}.summary-strip>div{display:flex;align-items:center;gap:10px}.summary-strip strong{font-family:Georgia,serif;font-size:30px;font-weight:400;line-height:1}.summary-strip span{font-size:11px;line-height:1.45;max-width:85px;color:var(--muted)}.summary-strip>p{margin:0 0 0 auto;font-size:11px;color:var(--muted);text-align:right}.workspace{display:grid;grid-template-columns:minmax(0,1fr) 285px;gap:30px}.opportunities>.section-title{margin-bottom:18px}.notice{padding:24px 26px;margin-bottom:16px;overflow-wrap:anywhere}.notice-top{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:17px}.badge{font-size:10px;line-height:1.5;font-weight:600;padding:5px 9px;border-radius:5px;display:inline-block;letter-spacing:0}.badge.good{background:var(--soft);color:#325b42}.badge.warn{background:var(--amber-bg);color:var(--amber)}.badge.muted{background:#eeeFE9;color:#647064}.notice-summary{color:var(--muted);font-size:13px;line-height:1.7}.reason{background:#faf5e9;border-left:2px solid #c39d5a;padding:10px 12px;font-size:12px;color:#785d31;line-height:1.7}.deadline{font-weight:600;font-size:12px}.notice-meta{display:flex;align-items:center;justify-content:space-between;gap:14px;font-size:11px;color:var(--muted);margin-top:17px}.notice-meta a{font-size:12px;font-weight:600}.notice-meta>span{max-width:48%;overflow-wrap:anywhere;text-align:right}details{border-top:1px solid var(--line);margin-top:17px;padding-top:12px}summary{cursor:pointer;font-size:12px;font-weight:600}summary:hover{color:var(--green)}.checks summary>span{font-weight:400;color:var(--muted);margin-left:6px}.checks ul{list-style:none;margin:10px 0 0;padding:0}.checks li{padding:12px 0;border-top:1px solid #edf0e8}.checks li>div{display:flex;gap:12px;align-items:center;justify-content:space-between}.checks strong{font-size:12px;font-weight:600}.checks li p{font-size:12px;margin:7px 0;color:var(--muted)}.checks li a{font-size:11px}.technical{color:var(--muted)}.technical p{font-size:11px;margin-top:10px}pre{font:11px/1.65 ui-monospace,SFMono-Regular,Consolas,monospace;white-space:pre-wrap;overflow-wrap:anywhere;background:#f3f5ef;padding:13px;border-radius:6px;max-height:360px;overflow:auto;color:var(--ink)}.aside-panel{padding:24px;margin-top:69px}.aside-panel h2{font-size:20px}.steps{margin:23px 0 0;padding-left:21px}.steps li{padding-left:6px;margin-bottom:21px;color:var(--green)}.steps li:last-child{margin-bottom:0}.steps strong{font-size:12px}.steps p{font-size:12px;color:var(--muted);line-height:1.6;margin:5px 0 0}.coverage{padding:26px 10px}.coverage h3{font-size:18px}.coverage p:not(.eyebrow){font-size:12px;color:var(--muted)}.empty{text-align:center;padding:42px 30px}.empty-icon{display:inline-grid;place-items:center;width:45px;height:45px;border:1px solid var(--line);border-radius:12px;font-size:24px;background:var(--soft);margin-bottom:20px}.empty h3{font-family:Georgia,serif;font-weight:400;font-size:25px}.empty p{color:var(--muted);font-size:13px;max-width:400px;margin:0 auto 20px}.empty .eyebrow{margin-bottom:12px}.evidence{padding:27px 30px;margin-top:27px}.evidence>.section-title{margin-bottom:13px}.evidence>p{color:var(--muted);font-size:12px;max-width:760px}.evidence-row{margin-top:0;padding:15px 0}.evidence-row summary{display:flex;align-items:center;gap:18px;flex-wrap:wrap}.evidence-row summary:before{content:"+";font-size:16px;font-weight:400;color:var(--muted)}.evidence-row[open] summary:before{content:"−"}.evidence-row summary>span:first-child{min-width:min(300px,100%)}.evidence-row summary strong{font-weight:500}.evidence-row summary>.quiet{margin-left:auto;font-weight:400}.evidence-detail{font-size:12px;color:var(--muted);padding:15px 0 0 28px;overflow-wrap:anywhere}.evidence-detail>p{margin-bottom:7px}footer{margin:30px 0 35px;color:var(--muted);font-size:10px;display:flex;justify-content:space-between;gap:30px}footer p{max-width:500px;margin:0}footer p:last-child{text-align:right}.skip{position:fixed;top:-100px;left:20px;z-index:2;background:white;padding:10px 20px}.skip:focus{top:12px}@media(max-width:980px){.shell{width:calc(100% - 40px)}.filters{grid-template-columns:1fr;gap:20px}.workspace{grid-template-columns:minmax(0,1fr) 250px;gap:20px}.summary-strip{gap:19px}.summary-strip>p{max-width:190px}.aside-panel{padding:21px}}@media(max-width:720px){.shell{width:calc(100% - 32px)}.nav{height:72px}.brand{font-size:22px}.local{font-size:10px;max-width:105px;text-align:right;line-height:1.5}.intro{padding:35px 0 30px}.lede{font-size:15px}.context{padding:22px 20px}.context>.section-title>.quiet{display:none}.input-row{flex-wrap:wrap}.input-row select{flex-basis:100%}.input-row button{width:100%}.context-foot{flex-direction:column;gap:3px}.summary-strip{flex-wrap:wrap;gap:20px 13px;padding:25px 0}.summary-strip>div{flex:1;min-width:88px;gap:7px}.summary-strip strong{font-size:27px}.summary-strip span{font-size:10px}.summary-strip>p{max-width:none;flex-basis:100%;text-align:left;margin:0}.workspace{display:block}.opportunities>.section-title>.quiet{max-width:90px;text-align:right}.notice{padding:21px 20px}.notice h3{font-size:20px}.notice-meta{align-items:flex-start}.aside-panel{margin-top:25px}.coverage{padding-bottom:0}.evidence{padding:22px 20px}.evidence h2{font-size:21px}.evidence-row summary{gap:8px}.evidence-row summary>span:first-child{min-width:0;max-width:calc(100% - 22px)}.evidence-row summary>strong{margin-left:18px;flex-basis:100%}.evidence-row summary>.quiet{margin-left:18px;flex-basis:100%}.evidence-detail{padding-left:18px}footer{display:block}footer p:last-child{text-align:left;margin-top:9px}}@media(prefers-reduced-motion:no-preference){button,a,summary{transition:background-color .12s,color .12s}}@media print{.topbar,.filters,.coverage,footer{display:none}.shell{width:100%}.workspace{display:block}aside{display:none}.notice,.panel{break-inside:avoid}body{background:white;color:black}.intro{padding:15px 0}h1{font-size:34px}.summary-strip{padding:12px 0}}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_covers_text_and_quoted_attributes() {
        assert_eq!(
            escape("<script x=\"a&b\">'</script>"),
            "&lt;script x=&quot;a&amp;b&quot;&gt;&#39;&lt;/script&gt;"
        );
        assert_eq!(escape("Aci Sant’Antonio"), "Aci Sant’Antonio");
    }

    #[test]
    fn links_allow_only_http_s_without_credentials() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,test",
            "//evil.test",
            "/local",
            "https://user:secret@example.test/",
        ] {
            assert!(safe_url(bad).is_none(), "{bad}");
        }
        assert!(safe_url("https://example.test/a?q=1&b=2").is_some());
        assert_eq!(
            source_link("javascript:alert(1)", "<b>fonte</b>"),
            "<span>&lt;b&gt;fonte&lt;/b&gt;</span>"
        );
        assert!(source_link("https://example.test/a?a=1&b=2", "fonte").contains("a=1&amp;b=2"));
    }

    #[test]
    fn defaults_hide_only_definitive_closed_or_ineligible() {
        for state in ["closed", "ineligible", "excluded"] {
            assert!(!should_show(&json!({"state":state,"visible":true}), false));
            assert!(should_show(&json!({"state":state}), true));
        }
        for state in [
            "eligible",
            "needs_review",
            "review_required",
            "unknown",
            "stale",
            "conflict",
            "new_future_state",
            "blocked",
        ] {
            assert!(should_show(&json!({"state":state,"visible":false}), false));
        }
        assert!(should_show(&Value::Null, false));
    }

    #[test]
    fn unknown_never_looks_like_approval() {
        assert_eq!(state_label("unknown"), ("Dato mancante", "warn"));
        assert_eq!(state_label("novel-state"), ("Da verificare", "warn"));
    }

    #[test]
    fn source_freshness_rejects_stale_invalid_and_future_dates() {
        let now = "2026-10-06T12:00:00Z";
        assert!(source_is_fresh("2026-09-29T12:00:00Z", now));
        assert!(source_is_fresh("2026-10-06T12:00:00Z", now));
        for date in [
            "2026-09-29T11:59:59Z",
            "2026-10-06T12:00:01Z",
            "",
            "2026-10-06",
            "bad",
        ] {
            assert!(!source_is_fresh(date, now), "{date}");
        }
    }

    #[test]
    fn stale_source_cannot_produce_a_positive_screening() {
        let extraction: Value =
            serde_json::from_str(include_str!("../tests/fixtures/extraction.json")).unwrap();
        let municipality =
            json!({"istatCode":"088001","region":"Sicilia","registryReferenceDate":"2026-10-06"});
        let notice = json!({"id":"call","state":"screened","updated_at":"2026-09-01T12:00:00Z","extraction":extraction});
        let result = evaluate_notice(
            notice.clone(),
            Some(&municipality),
            &[],
            None,
            "2026-10-06T12:00:00Z",
        );
        assert_eq!(result["evaluation"]["state"], "review_required");
        assert_eq!(result["evaluation"]["source_freshness"]["state"], "stale");
        assert_eq!(result["evaluation"]["needs_review"], true);
        let mut fresh = notice.clone();
        fresh["updated_at"] = json!("2026-10-06T12:00:00Z");
        let result = evaluate_notice(
            fresh,
            Some(&municipality),
            &[],
            None,
            "2026-10-06T12:00:00Z",
        );
        assert_eq!(result["evaluation"]["state"], "screening_match");
        let result = evaluate_notice(
            notice,
            Some(&municipality),
            &[],
            None,
            "2027-01-01T12:00:00Z",
        );
        assert_eq!(result["evaluation"]["state"], "closed");
    }

    #[test]
    fn failed_or_pending_attempt_does_not_reuse_extraction() {
        for state in ["failed", "pending", "in_flight", "new-state"] {
            let result = evaluate_notice(
                json!({"id":"call","state":state,"extraction":{"old":"success"},"error":"private diagnostic"}),
                None,
                &[],
                None,
                "2026-10-06",
            );
            assert_eq!(result["evaluation"]["state"], "needs_review");
            assert_eq!(result["evaluation"]["visible"], true);
            assert!(result["extraction"].is_null());
            assert!(result.get("error").is_none());
        }
    }

    #[test]
    fn malformed_query_is_not_silently_reinterpreted() {
        assert!(Query::parse("municipality=a&municipality=b").is_err());
        assert!(Query::parse("archive=banana").is_err());
        let q = Query::parse("municipality=088001&project=foo%3Abar&archive=1").unwrap();
        assert_eq!(q.municipality.as_deref(), Some("088001"));
        assert_eq!(q.project.as_deref(), Some("foo:bar"));
        assert!(q.archive);
    }

    #[test]
    fn loopback_host_check_rejects_foreign_origins() {
        assert!(host_allowed(Some("localhost:8080"), "127.0.0.1:8080"));
        assert!(host_allowed(Some("127.0.0.1:8080"), "127.0.0.1:8080"));
        assert!(!host_allowed(Some("evil.example:8080"), "127.0.0.1:8080"));
        assert!(!host_allowed(Some("127.0.0.1:80"), "127.0.0.1:8080"));
        assert!(!host_allowed(None, "127.0.0.1:8080"));
    }

    #[test]
    fn invalid_municipality_or_project_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("test.sqlite")).unwrap();
        store.seed().unwrap();
        assert!(matches!(
            load_view(
                &store,
                Query {
                    municipality: Some("not-a-municipality".into()),
                    ..Default::default()
                }
            ),
            Err(ViewError::Input(_))
        ));
        assert!(matches!(
            load_view(
                &store,
                Query {
                    municipality: Some("088001".into()),
                    project: Some("unrelated".into()),
                    archive: false
                }
            ),
            Err(ViewError::Input(_))
        ));
        assert_eq!(
            route(&store, "/api/opportunities?municipality=invalid").status,
            400
        );
    }

    #[test]
    fn empty_store_renders_without_invented_results_or_browser_script() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("empty.sqlite")).unwrap();
        let response = route(&store, "/");
        assert_eq!(response.status, 200);
        assert!(
            response
                .body
                .contains("Non ci sono ancora avvisi elaborati")
        );
        assert!(!response.body.contains("<script"));
        assert!(!response.body.contains("OPENROUTER_API_KEY"));
        assert!(response.body.contains("Non è un catalogo completo"));
    }

    #[test]
    fn malicious_notice_cannot_inject_markup() {
        let notice = json!({"id":"<id>","title":"<img src=x onerror=alert(1)>","source_url":"javascript:alert(1)","version":1,"updated_at":"\"><script>","extraction":{"summary":"<svg onload=alert(1)>"},"evaluation":{"state":"needs_review","reason":"<script>alert(1)</script>"}});
        let mut html = String::new();
        render_notice(&mut html, &notice);
        assert!(!html.contains("<script"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("<svg"));
        assert!(!html.contains("href=\"javascript:"));
        assert!(html.contains("&lt;img"));
    }
}
