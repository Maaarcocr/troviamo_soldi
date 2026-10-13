//! Read-only views of saved research. Rendering never starts research or a model call.
use crate::{research_store::ResearchStore, store::Store};
use chrono::Utc;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fmt::Write};
use url::Url;

const PACHINO: &str = "089014";
const COVERAGE_NOTE: &str = "La ricerca salvata non è un catalogo completo. Una vista vuota non dimostra che non esistano bandi. Aprire questa pagina non aggiorna le fonti e non avvia chiamate al modello.";

pub(crate) struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}
impl Reply {
    fn html(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body,
        }
    }
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: value.to_string(),
        }
    }
    fn error(status: u16, message: &str, api: bool) -> Self {
        let mut reply = if api {
            Self::json(json!({"error": message}))
        } else {
            Self::html(shell(
                "Ricerca non disponibile",
                &format!(
                    "<main class=\"shell\"><section class=\"empty panel\"><h1>Ricerca non disponibile</h1><p>{}</p><a href=\"/\">Torna alla ricerca</a></section></main>",
                    escape(message)
                ),
            ))
        };
        reply.status = status;
        reply
    }
}

/// Return None for routes owned by the legacy interface.
pub(crate) fn route(store: &Store, research: &ResearchStore, target: &str) -> Option<Reply> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if !matches!(path, "/" | "/api/research/opportunities" | "/research.css") {
        return None;
    }
    let api = path.starts_with("/api/");
    if target.len() > 8192 {
        return Some(Reply::error(
            400,
            "Indirizzo della richiesta troppo lungo.",
            api,
        ));
    }
    if path == "/research.css" {
        return Some(Reply {
            status: 200,
            content_type: "text/css; charset=utf-8",
            body: CSS.into(),
        });
    }
    let query = match Query::parse(query) {
        Ok(query) => query,
        Err(message) => return Some(Reply::error(400, message, api)),
    };
    let municipalities = match store.municipalities() {
        Ok(rows) => rows,
        Err(error) => return Some(storage_error(error, api)),
    };
    let municipality = municipalities
        .iter()
        .find(|row| text(row, "istatCode") == query.municipality);
    if municipality.is_none() && (!municipalities.is_empty() || query.explicit_municipality) {
        return Some(Reply::error(
            400,
            "Comune non presente nel registro locale.",
            api,
        ));
    }
    let as_of = Utc::now().to_rfc3339();
    let opportunities = if municipality.is_some() {
        match research.opportunities(&query.municipality, &as_of, query.archive) {
            Ok(rows) => rows,
            Err(error) => return Some(storage_error(error, api)),
        }
    } else {
        Vec::new()
    };
    if api {
        return Some(Reply::json(json!({
            "municipality": municipality,
            "as_of": as_of,
            "archive": query.archive,
            "opportunities": opportunities,
            "coverage_note": COVERAGE_NOTE
        })));
    }
    Some(Reply::html(render(
        &municipalities,
        municipality,
        &opportunities,
        &query,
        &as_of,
    )))
}

fn storage_error(error: anyhow::Error, api: bool) -> Reply {
    eprintln!("Research UI storage error: {error:#}");
    Reply::error(
        500,
        "Non riesco a leggere la ricerca salvata. Controlla il servizio e riprova.",
        api,
    )
}

struct Query {
    municipality: String,
    explicit_municipality: bool,
    archive: bool,
}
impl Query {
    fn parse(query: &str) -> Result<Self, &'static str> {
        let mut params = BTreeMap::new();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if matches!(key.as_ref(), "municipality" | "archive")
                && params
                    .insert(key.into_owned(), value.into_owned())
                    .is_some()
            {
                return Err("Un filtro è ripetuto nella richiesta.");
            }
        }
        let explicit = params
            .remove("municipality")
            .filter(|value| !value.is_empty());
        let archive = match params.remove("archive").as_deref() {
            None | Some("") | Some("0") => false,
            Some("1") => true,
            _ => return Err("Il filtro archivio deve essere 0 oppure 1."),
        };
        Ok(Self {
            explicit_municipality: explicit.is_some(),
            municipality: explicit.unwrap_or_else(|| PACHINO.into()),
            archive,
        })
    }
}

fn render(
    municipalities: &[Value],
    municipality: Option<&Value>,
    opportunities: &[Value],
    query: &Query,
    as_of: &str,
) -> String {
    let name = municipality
        .map(|row| text(row, "name"))
        .unwrap_or("il tuo Comune");
    let mut body = String::with_capacity(24_000);
    body.push_str("<a class=\"skip\" href=\"#opportunita\">Vai alle opportunità</a><header><nav class=\"shell nav\" aria-label=\"Navigazione principale\"><a class=\"brand\" href=\"/\"><span class=\"mark\" aria-hidden=\"true\">c</span>comuni <span>/ ricerca</span></a><a href=\"/archive\">Archivio importazioni</a></nav></header><main class=\"shell\">");
    let _ = write!(
        body,
        "<section class=\"intro\"><p class=\"eyebrow\">BANDI, FONTI, PROSSIMI PASSI</p><h1>Che cosa può finanziare<br>{}?</h1><p class=\"lede\">Le opportunità emerse dalla ricerca, le ragioni per approfondirle e quello che serve prima di candidarsi.</p></section>",
        escape(name)
    );
    body.push_str("<section class=\"context panel\" aria-label=\"Contesto della ricerca\"><form method=\"get\" action=\"/\"><div class=\"select-group\"><label for=\"municipality\">Comune</label><select name=\"municipality\" id=\"municipality\">");
    if municipalities.is_empty() {
        body.push_str("<option value=\"\">Registro non ancora importato</option>");
    }
    for row in municipalities {
        let id = text(row, "istatCode");
        let _ = write!(
            body,
            "<option value=\"{}\"{}>{} ({})</option>",
            escape(id),
            if id == query.municipality {
                " selected"
            } else {
                ""
            },
            escape(text(row, "name")),
            escape(text(row, "provinceAbbreviation"))
        );
    }
    let _ = write!(
        body,
        "</select></div><label class=\"checkbox\"><input type=\"checkbox\" name=\"archive\" value=\"1\"{}> Includi bandi chiusi e non compatibili</label><button type=\"submit\">Mostra ricerca</button></form>",
        if query.archive { " checked" } else { "" }
    );
    if let Some(row) = municipality {
        let _ = write!(
            body,
            "<p class=\"context-foot\"><strong>{}</strong> · {} · ISTAT {} <span>Registro del {}</span></p>",
            escape(name),
            escape(text(row, "province")),
            escape(text(row, "istatCode")),
            escape(text(row, "registryReferenceDate"))
        );
    }
    body.push_str("</section>");
    let display_time = chrono::DateTime::parse_from_rfc3339(as_of)
        .map(|date| date.format("%d/%m/%Y, %H:%M UTC").to_string())
        .unwrap_or_else(|_| as_of.to_owned());
    let _ = write!(
        body,
        "<div class=\"summary-strip\"><p><strong>{}</strong> opportunità nella vista</p><p>Date ricalcolate al {}.<br>Le fonti hanno una propria data di verifica.</p></div><div class=\"workspace\"><section id=\"opportunita\" aria-labelledby=\"opportunity-title\"><div class=\"section-title\"><p class=\"eyebrow\">{}</p><h2 id=\"opportunity-title\">Opportunità da approfondire</h2></div>",
        opportunities.len(),
        escape(&display_time),
        if query.archive {
            "RICERCA E STORICO"
        } else {
            "DALLA RICERCA SALVATA"
        }
    );
    if opportunities.is_empty() {
        body.push_str("<section class=\"empty panel\"><span class=\"empty-icon\" aria-hidden=\"true\">↗</span><h3>Nessuna opportunità ricercata in questa vista.</h3>");
        if municipalities.is_empty() {
            body.push_str("<p>Importa il registro dei Comuni con il comando di inizializzazione prima di avviare la ricerca.</p>");
        } else {
            let _ = write!(
                body,
                "<p>Non ci sono risultati salvati da mostrare per {} con questi filtri. La ricerca può non essere stata avviata o non aver ancora prodotto una valutazione utilizzabile.</p>",
                escape(name)
            );
        }
        body.push_str("<p>Questa pagina non aggiunge bandi di esempio. I tentativi d’importazione falliti restano nell’archivio e non diventano opportunità.</p></section>");
    }
    for opportunity in opportunities {
        render_opportunity(&mut body, opportunity, name);
    }
    body.push_str("</section><aside><section class=\"panel guide\"><p class=\"eyebrow\">COME DECIDERE</p><h2>Due verifiche distinte.</h2><ol><li><strong>Il Comune può partecipare?</strong><p>Verifica soggetti ammessi, territorio e requisiti dell’ente.</p></li><li><strong>Il progetto è pronto?</strong><p>Verifica sito, documenti, cofinanziamento e condizioni della candidatura.</p></li></ol><p class=\"help\">Una compatibilità preliminare non certifica l’ammissibilità e non sostituisce il testo ufficiale.</p></section><section class=\"coverage\"><p class=\"eyebrow\">COPERTURA E LIMITI</p><h3>La ricerca resta verificabile.</h3>");
    let _ = write!(
        body,
        "<p>{}</p><p>Le evidenze mancanti o discordanti restano aperte. Prima di presentare domanda, controlla avviso, allegati, rettifiche e termine vigente.</p></section></aside></div><footer><p>Comuni / ricerca · SQLite · sola lettura</p><p>Servizio locale senza autenticazione. Non esporlo pubblicamente senza controlli di accesso.</p></footer></main>",
        escape(COVERAGE_NOTE)
    );
    shell(&format!("Opportunità per {name} · Comuni"), &body)
}

fn render_opportunity(out: &mut String, opportunity: &Value, municipality_name: &str) {
    let stale = opportunity["is_stale"].as_bool().unwrap_or(false);
    let (status, tone) = match (stale, text(opportunity, "application_status")) {
        (true, _) => ("Termine da riconfermare", "warn"),
        (false, "open") => ("Domande aperte secondo i dati salvati", "good"),
        (false, "closed") => ("Termine chiuso · storico", "muted"),
        (false, "forthcoming") => ("In apertura", "warn"),
        _ => ("Apertura da verificare", "warn"),
    };
    let _ = write!(
        out,
        "<article class=\"opportunity panel\"><div class=\"card-top\"><span class=\"badge {tone}\">{status}</span><span class=\"small\">{}</span></div><h3>{}</h3>",
        escape(text(opportunity, "authority")),
        escape(text(opportunity, "title"))
    );
    if stale {
        out.push_str("<p class=\"warning\"><strong>Valutazione da aggiornare.</strong> Le fonti o i fatti da cui dipende sono cambiati o non sono più attuali. Un precedente esito favorevole non vale come conferma.</p>");
    }
    let amount = if opportunity["amount"].is_null() {
        &opportunity["bando_amount"]
    } else {
        &opportunity["amount"]
    };
    let _ = write!(
        out,
        "<dl class=\"key-facts\"><div><dt>Risorse e contributo</dt><dd>{}</dd></div><div><dt>Termine per la domanda</dt><dd>{}</dd></div><div><dt>Accreditamento e termini preliminari</dt><dd>{}</dd></div><div><dt>Ambito della valutazione</dt><dd>{}</dd></div></dl>",
        escape(&describe(amount)),
        escape(&deadline(&opportunity["deadline"])),
        escape(&describe(&opportunity["accreditation"])),
        escape(&scope_label(&opportunity["scope"], municipality_name))
    );
    let rationale = text(opportunity, "why_fit");
    let summary = text(opportunity, "summary");
    if !rationale.is_empty() {
        let _ = write!(
            out,
            "<section class=\"fit\"><h4>Perché approfondire per {}</h4><p>{}</p></section>",
            escape(municipality_name),
            escape(rationale)
        );
        if !summary.is_empty() && summary != rationale {
            let _ = write!(out, "<p class=\"summary\">{}</p>", escape(summary));
        }
    } else if !summary.is_empty() {
        let _ = write!(
            out,
            "<section class=\"fit\"><h4>Valutazione per {}</h4><p>{}</p></section>",
            escape(municipality_name),
            escape(summary)
        );
    } else {
        out.push_str("<p class=\"warning\">La motivazione specifica per questo Comune non è ancora documentata.</p>");
    }
    let eligibility = if stale {
        "unknown"
    } else {
        text(opportunity, "eligibility")
    };
    let readiness = if stale {
        "unknown"
    } else {
        text(opportunity, "project_readiness")
    };
    let (eligibility_label, eligibility_tone, eligibility_note) = match eligibility {
        "eligible" => (
            "Compatibilità documentata",
            "good",
            "La ricerca trova riscontro nei requisiti dell’ente esaminati. Restano da validare domanda e condizioni del progetto.",
        ),
        "ineligible" => (
            "Ente non compatibile",
            "muted",
            "La valutazione salvata rileva un requisito dell’ente non soddisfatto.",
        ),
        "conditional" => (
            "Compatibilità condizionata",
            "warn",
            "La partecipazione dipende da condizioni ancora da risolvere.",
        ),
        _ => (
            "Ammissibilità da verificare",
            "warn",
            "Le informazioni disponibili non bastano a confermare i requisiti dell’ente.",
        ),
    };
    let (readiness_label, readiness_tone, readiness_note) = match readiness {
        "ready" => (
            "Preparazione documentata",
            "good",
            "La ricerca ha verificato le condizioni del progetto nel perimetro indicato. L’invio resta da validare con l’ufficio competente.",
        ),
        "needs_work" => (
            "Preparazione da completare",
            "warn",
            "Prima della domanda restano attività o condizioni progettuali da completare.",
        ),
        "not_assessed" => (
            "Progetto non valutato",
            "warn",
            "La ricerca non ha ancora valutato un progetto o sito specifico.",
        ),
        _ => (
            "Preparazione da verificare",
            "warn",
            "La compatibilità dell’ente non dimostra che il progetto sia pronto per la domanda.",
        ),
    };
    let _ = write!(
        out,
        "<div class=\"checks\"><section class=\"check\"><h4>1. Ammissibilità dell’ente</h4><span class=\"badge {eligibility_tone}\">{eligibility_label}</span><p>{eligibility_note}</p></section><section class=\"check\"><h4>2. Preparazione del progetto</h4><span class=\"badge {readiness_tone}\">{readiness_label}</span><p>{readiness_note}</p></section></div>"
    );
    if let Some(unknowns) = opportunity["unknowns"]
        .as_array()
        .filter(|values| !values.is_empty())
    {
        out.push_str(
            "<section class=\"unknowns\"><h4>Le questioni essenziali ancora aperte</h4><ul>",
        );
        for unknown in unknowns {
            let _ = write!(out, "<li>{}</li>", escape(&describe(unknown)));
        }
        out.push_str("</ul></section>");
    }
    out.push_str("<section class=\"next\"><h4>Prossimo passo</h4>");
    if let Some(actions) = opportunity["next_actions"]
        .as_array()
        .filter(|values| !values.is_empty())
    {
        if actions.len() == 1 {
            let _ = write!(out, "<p>{}</p>", escape(&describe(&actions[0])));
        } else {
            out.push_str("<ol>");
            for action in actions {
                let _ = write!(out, "<li>{}</li>", escape(&describe(action)));
            }
            out.push_str("</ol>");
        }
    } else {
        out.push_str("<p>Il prossimo passo non è ancora definito nella ricerca salvata.</p>");
    }
    out.push_str("</section>");
    let _ = write!(
        out,
        "<p class=\"small\">Valutazione riferita al {} · salvata il {}</p>",
        escape(&date_time(text(opportunity, "assessed_as_of"))),
        escape(&date_time(text(opportunity, "saved_at")))
    );
    render_evidence(out, opportunity);
    out.push_str("</article>");
}

fn render_evidence(out: &mut String, opportunity: &Value) {
    let evidence = opportunity["evidence"].as_array();
    let Some(evidence) = evidence.filter(|rows| !rows.is_empty()) else {
        out.push_str("<p class=\"warning\">Le evidenze di questa valutazione non sono disponibili nella vista. Il risultato non è verificabile da questa pagina.</p>");
        return;
    };
    let _ = write!(
        out,
        "<details><summary>Fonti e passaggi a supporto ({})</summary><p class=\"help\">Controlla che documento, periodo e soggetto citato corrispondano a questa candidatura. Le citazioni salvate non sostituiscono la lettura dell’originale.</p><ol>",
        evidence.len()
    );
    for row in evidence {
        let title = text(row, "source_title");
        let _ = write!(
            out,
            "<li><p>{}</p>",
            source_link(
                text(row, "source_url"),
                if title.is_empty() {
                    "Documento originale ↗"
                } else {
                    title
                }
            )
        );
        if let Some(locator) = row["locator"].as_str().filter(|value| !value.is_empty()) {
            let _ = write!(
                out,
                "<p class=\"small\">Riferimento: {}</p>",
                escape(locator)
            );
        }
        if !text(row, "quote").is_empty() {
            let _ = write!(
                out,
                "<blockquote>{}</blockquote>",
                escape(text(row, "quote"))
            );
        }
        if text(row, "quote_verification") == "original_not_text_verified" {
            out.push_str("<p class=\"small\">Passaggio interpretato dall’originale PDF/immagine; corrispondenza testuale non verificata automaticamente.</p>");
        }
        if text(row, "origin_trust") == "workspace_unverified" {
            out.push_str(
                "<p class=\"warning\">Documento locale: provenienza ufficiale non verificata.</p>",
            );
        }
        let _ = write!(
            out,
            "<p class=\"small\">Fonte acquisita il {} · Relazione: {}</p></li>",
            escape(&date_time(text(row, "retrieved_at"))),
            escape(&relation_label(text(row, "relation")))
        );
    }
    out.push_str("</ol></details>");
}

fn scope_label(scope: &Value, municipality_name: &str) -> String {
    let mut parts = vec![municipality_name.to_owned()];
    for (key, label) in [
        ("entity_id", "Ente"),
        ("site_id", "Sito"),
        ("project_id", "Progetto"),
    ] {
        if let Some(value) = scope[key].as_str().filter(|value| !value.is_empty()) {
            parts.push(format!("{label}: {value}"));
        }
    }
    parts.join(" · ")
}
fn relation_label(relation: &str) -> String {
    match relation {
        "supports" => "A supporto".into(),
        "contradicts" => "In contrasto".into(),
        "related_only" => "Solo collegamento di contesto".into(),
        "primary" | "official_notice" => "Avviso di riferimento".into(),
        "amendment" => "Rettifica".into(),
        "annex" => "Allegato".into(),
        "" => "Da verificare".into(),
        value => value.replace('_', " "),
    }
}
fn deadline(value: &Value) -> String {
    match text(value, "precision") {
        "date" => chrono::NaiveDate::parse_from_str(text(value, "value"), "%Y-%m-%d")
            .map(|date| format!("{} · ora non indicata", date.format("%d/%m/%Y")))
            .unwrap_or_else(|_| "Da verificare".into()),
        "date_time" => date_time(text(value, "value")),
        _ => "Da verificare".into(),
    }
}
fn date_time(value: &str) -> String {
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(value) {
        date.format("%d/%m/%Y, %H:%M:%S %:z").to_string()
    } else if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        date.format("%d/%m/%Y").to_string()
    } else if value.is_empty() {
        "Data non disponibile".into()
    } else {
        value.into()
    }
}

/// Describe flexible research facts without guessing currency, units or whether
/// an amount is a whole-call budget versus a per-project grant.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "Da verificare".into(),
        Value::String(value) if value.trim().is_empty() => "Da verificare".into(),
        Value::String(value) => value.clone(),
        Value::Bool(value) => if *value { "Sì" } else { "No" }.into(),
        Value::Number(value) => value.to_string(),
        Value::Array(values) if values.is_empty() => "Da verificare".into(),
        Value::Array(values) => values.iter().map(describe).collect::<Vec<_>>().join("; "),
        Value::Object(values) if values.is_empty() => "Da verificare".into(),
        Value::Object(values)
            if values.contains_key("precision") && values.contains_key("value") =>
        {
            deadline(value)
        }
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| format!("{}: {}", field_label(key), describe(value)))
            .collect::<Vec<_>>()
            .join("; "),
    }
}
fn field_label(key: &str) -> String {
    match key {
        "currency" => "Valuta".into(),
        "total_budget" => "Dotazione complessiva".into(),
        "total_budget_eur" => "Dotazione complessiva (EUR)".into(),
        "max_grant" | "max_per_project" => "Contributo massimo per progetto".into(),
        "min_grant" | "min_per_project" => "Contributo minimo per progetto".into(),
        "amount" | "value" => "Importo".into(),
        "amount_eur" => "Importo in EUR".into(),
        "description" | "notes" | "summary" => "Note".into(),
        "total" => "Dotazione complessiva".into(),
        "rate" => "Quota o tasso dichiarato".into(),
        "status" => "Stato".into(),
        "required" => "Richiesto".into(),
        "deadline" => "Termine".into(),
        "cofinancing_percent" => "Cofinanziamento (%)".into(),
        "funding_rate_percent" => "Quota finanziata (%)".into(),
        "source_url" => "Fonte".into(),
        value => value.replace('_', " "),
    }
}

fn shell(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"it\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><meta name=\"color-scheme\" content=\"light\"><title>{}</title><link rel=\"stylesheet\" href=\"/research.css\"></head><body>{body}</body></html>",
        escape(title)
    )
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
fn escape(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}
fn source_link(input: &str, label: &str) -> String {
    let safe = Url::parse(input).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    });
    match safe {
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
:root{font-family:Inter,ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#203b35;background:#f5f5ee;font-synthesis:none;--muted:#66746c;--line:#dfe4d9;--green:#28634c;--paper:#fffefa}*{box-sizing:border-box}body{margin:0;font-size:14px;line-height:1.6}h1,h2,h3,h4,p{margin-top:0}h1,h2,h3,h4{line-height:1.2;letter-spacing:-.035em}h1{font-family:Georgia,serif;font-weight:400;font-size:clamp(38px,5.5vw,64px);margin-bottom:18px}h2{font-size:23px;margin-bottom:0}h3{font-size:23px;margin-bottom:15px}h4{font-size:13px;margin-bottom:8px}a{color:var(--green);text-underline-offset:3px}.shell{width:min(1180px,calc(100% - 64px));margin:0 auto}header{border-bottom:1px solid var(--line);background:#f9faf5}.nav{display:flex;align-items:center;justify-content:space-between;height:84px;gap:20px}.nav>a:last-child{font-size:12px}.brand{display:flex;align-items:center;color:#203b35;font-size:25px;font-weight:700;letter-spacing:-.06em;text-decoration:none}.brand>span:last-child{font-weight:400;color:var(--muted);margin-left:8px}.mark{display:grid;place-items:center;font:30px Georgia,serif;width:35px;height:35px;background:var(--green);color:white;border-radius:10px;margin-right:12px}.intro{padding:50px 0 35px}.eyebrow{font-size:10px;font-weight:700;letter-spacing:.15em;color:var(--muted);margin-bottom:10px}.lede{font-size:17px;max-width:640px;color:var(--muted);margin-bottom:0}.panel{border:1px solid var(--line);background:var(--paper);border-radius:14px}.context{padding:24px 28px}.context form{display:flex;align-items:flex-end;gap:28px}.select-group{flex:1;min-width:0}.select-group>label{display:block;font-size:12px;font-weight:600;margin-bottom:7px}select,button,input{font:inherit}select{width:100%;background:white;border:1px solid #cbd4c7;border-radius:8px;padding:11px 12px;color:inherit}.checkbox{display:flex;align-items:center;gap:8px;font-size:12px;color:var(--muted);padding-bottom:12px}.checkbox input{accent-color:var(--green);width:16px;height:16px}button{padding:11px 18px;border:1px solid var(--green);border-radius:8px;color:white;background:var(--green);font-weight:600;cursor:pointer}button:hover{background:#1e4e3b}:focus-visible{outline:3px solid #bd812c;outline-offset:3px}.context-foot{border-top:1px solid var(--line);margin:21px 0 0;padding-top:15px;font-size:12px;color:var(--muted)}.context-foot span{float:right}.summary-strip{display:flex;justify-content:space-between;align-items:center;gap:20px;padding:25px 3px}.summary-strip p{font-size:12px;color:var(--muted);margin:0}.summary-strip p:last-child{text-align:right;font-size:11px}.summary-strip strong{font:32px Georgia,serif;color:#203b35;vertical-align:middle;margin-right:10px}.workspace{display:grid;grid-template-columns:minmax(0,1fr) 270px;gap:28px}.section-title{margin-bottom:20px}.section-title .eyebrow{margin-bottom:6px}.opportunity{padding:25px 27px;margin-bottom:18px;overflow-wrap:anywhere}.opportunity h3{margin-bottom:12px}.card-top{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-bottom:17px}.badge{display:inline-block;padding:5px 9px;border-radius:5px;background:#f5ecd8;color:#785723;font-size:10px;font-weight:600}.badge.good{background:#eaf0e5;color:#325b42}.badge.muted{background:#eeefe9;color:#647064}.small,.help{font-size:11px;color:var(--muted)}.summary{font-size:13px;color:var(--muted);margin-bottom:18px}.key-facts{display:grid;grid-template-columns:1fr 1fr;gap:18px 23px;border-top:1px solid var(--line);border-bottom:1px solid var(--line);padding:17px 0;margin:0 0 20px}.key-facts dt{font-size:11px;color:var(--muted);margin-bottom:4px}.key-facts dd{margin:0;font-size:13px;font-weight:600}.fit{padding:15px 17px;background:#f1f4ec;border-radius:8px;margin-bottom:20px}.fit p{font-size:13px;margin:0}.checks{display:grid;grid-template-columns:1fr 1fr;gap:22px;margin:18px 0}.check h4{margin-bottom:9px}.check p{font-size:12px;color:var(--muted);margin:9px 0 0}.next{border-left:3px solid #478067;padding:12px 15px;background:#edf3e8;margin:20px 0}.next p{font-size:13px;margin:0}.next ol{padding-left:18px;font-size:13px;margin:0}.next li+li{margin-top:7px}.unknowns{background:#faf5e9;border:1px solid #eadfca;border-radius:8px;padding:14px 17px;margin:18px 0}.unknowns ul{margin:0;padding-left:18px;font-size:12px;color:#785d31}.unknowns li+li{margin-top:7px}.warning{font-size:12px;color:#785d31;background:#faf5e9;padding:12px 14px;border-left:2px solid #c39d5a}.source-line{display:flex;justify-content:space-between;gap:16px;font-size:12px;margin:18px 0 0}.source-line span{font-size:11px;color:var(--muted)}details{border-top:1px solid var(--line);margin-top:18px;padding-top:13px}summary{font-size:12px;font-weight:600;cursor:pointer}details ol{padding-left:20px;margin-bottom:0}details li{font-size:12px;margin-top:15px}details li p{margin:5px 0}blockquote{margin:8px 0;padding:9px 12px;border-left:2px solid #cbd4c7;background:#f5f7f0;color:var(--muted);font-size:12px}pre{font:11px/1.7 ui-monospace,monospace;white-space:pre-wrap;overflow-wrap:anywhere;max-height:320px;overflow:auto;padding:14px;background:#f3f5ef;border-radius:6px}.guide{padding:24px;margin-top:64px}.guide h2{font-size:20px}.guide ol{margin:23px 0;padding-left:20px}.guide li{margin-bottom:20px;padding-left:4px;color:var(--green)}.guide li strong{font-size:12px}.guide li p{margin:6px 0 0;font-size:12px;color:var(--muted)}.guide>.help{margin-bottom:0}.coverage{padding:25px 8px}.coverage h3{font-size:18px}.coverage p:not(.eyebrow){font-size:12px;color:var(--muted)}.empty{padding:38px 28px;text-align:center}.empty-icon{display:inline-grid;place-items:center;font-size:24px;border:1px solid var(--line);border-radius:12px;width:45px;height:45px;background:#eaf0e5;margin-bottom:22px}.empty h3{font-family:Georgia,serif;font-weight:400;font-size:26px}.empty p{font-size:13px;color:var(--muted);max-width:500px;margin:0 auto 16px}.empty p:last-child{margin-bottom:0}footer{margin:32px 0;display:flex;justify-content:space-between;gap:30px;color:var(--muted);font-size:10px}footer p{margin:0;max-width:500px}footer p:last-child{text-align:right}.skip{position:fixed;top:-100px;left:20px;z-index:2;padding:10px 20px;background:white}.skip:focus{top:12px}@media(max-width:980px){.shell{width:calc(100% - 40px)}.context form{gap:16px;flex-wrap:wrap}.select-group{flex-basis:100%}.context .checkbox{flex:1}.workspace{grid-template-columns:minmax(0,1fr) 235px;gap:20px}.guide{padding:20px}.opportunity{padding:22px}}@media(max-width:720px){.shell{width:calc(100% - 32px)}.nav{height:72px}.brand{font-size:22px}.nav>a:last-child{max-width:92px;text-align:right;line-height:1.4;font-size:11px}.intro{padding:35px 0 28px}.lede{font-size:15px}.context{padding:20px}.context form{display:block}.context .checkbox{padding:0;margin:15px 0}.context button{width:100%}.context-foot span{display:block;float:none;margin-top:4px}.summary-strip{align-items:flex-start}.summary-strip p:first-child{max-width:160px}.summary-strip p:last-child{max-width:150px}.workspace{display:block}.opportunity{padding:20px}.opportunity h3{font-size:21px}.key-facts,.checks{gap:16px;grid-template-columns:1fr}.card-top{align-items:flex-start}.source-line{display:block}.source-line span{display:block;margin-top:8px}.guide{margin-top:25px}.coverage{padding-bottom:0}footer{display:block}footer p:last-child{text-align:left;margin-top:8px}}@media print{header,.context form,aside,footer{display:none}.shell{width:100%}.workspace{display:block}.opportunity{break-inside:avoid}.intro{padding:15px 0}h1{font-size:34px}body{background:white;color:black}}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    // Real SQLite and the shipped municipality register. No invented calls,
    // provider doubles or model-output fixtures are used by this UI check.
    #[test]
    fn real_empty_research_is_honest_pachino_first_and_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("research.sqlite");
        let mut store = Store::open(&path).unwrap();
        store.seed().unwrap();
        drop(ResearchStore::open(&path).unwrap());
        let research = ResearchStore::open_read_only(&path).unwrap();
        let version = || {
            store
                .conn
                .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        let before = version();
        let page = route(&store, &research, "/").unwrap();
        assert_eq!(page.status, 200);
        assert!(
            page.body
                .contains("<option value=\"089014\" selected>Pachino (SR)</option>")
        );
        assert!(
            page.body
                .contains("Nessuna opportunità ricercata in questa vista")
        );
        assert!(page.body.contains("non dimostra che non esistano bandi"));
        assert!(page.body.contains("href=\"/archive\""));
        assert!(!page.body.contains("<article class=\"opportunity"));
        assert!(!page.body.contains("<script"));
        let response = route(&store, &research, "/api/research/opportunities").unwrap();
        assert_eq!(response.status, 200);
        let payload: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(payload["municipality"]["istatCode"], PACHINO);
        assert!(payload["opportunities"].as_array().unwrap().is_empty());
        assert_eq!(
            version(),
            before,
            "GET rendering must not write research state"
        );
        for target in [
            "/?municipality=not-a-municipality",
            "/?municipality=089014&municipality=088001",
            "/?archive=invalid",
        ] {
            assert_eq!(route(&store, &research, target).unwrap().status, 400);
        }
        assert!(route(&store, &research, "/archive").is_none());
        assert!(route(&store, &research, "/api/opportunities").is_none());
    }
}
