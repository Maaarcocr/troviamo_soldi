# troviamo_soldi

Comuni / bandi, in Rust

One binary. SQLite. Original files go to Luna. No PDF extraction, OCR, agents, Python, workflow service, or JavaScript frontend.

The daily command discovers notices, hashes their original attachments, processes only new/changed versions with `openai/gpt-6-luna` at `reasoning.effort=max`, validates the returned factual conditions, and saves usage. The same binary serves a small read-only Italian interface. Municipality/project changes are evaluated locally and make **no AI calls**.

## Run locally

Install current stable Rust, then:

```sh
cargo build --release --locked
./target/release/funding-rust init
./target/release/funding-rust serve
```

Open http://127.0.0.1:8080. The initial archive is empty, with the 391 Sicilian municipalities and 511 previously collected official evidence records bundled. No example funding claims are loaded as real opportunities.

```sh
# Free source discovery, without AI or attachment processing:
./target/release/funding-rust discover --out notices.json

# Fetch/hash up to 5 new/changed candidates; no AI requests:
./target/release/funding-rust run --limit 5 --dry-run

# Billable only after YOU configure OPENROUTER_API_KEY and a provider budget:
./target/release/funding-rust run --limit 5 --allow-paid

# Reuse a saved/manual notice array, skipping live discovery:
./target/release/funding-rust run --input notices.json --limit 5 --allow-paid

./target/release/funding-rust status
```

`daily` is an alias of `run`. The limit is mandatory and counts **bandi attempted**, including download failures, provider failures and review cases. Unchanged cached versions do not consume a slot. Each attempted bando makes at most one model request in that run. There is no automatic larger-model fallback or paid retry.

A timeout may have been billed upstream. Its outcome is stored as uncertain/failed and unchanged versions are not automatically retried. `--retry-failed` is an explicit operator choice that can duplicate uncertain charges. It also retries review cases. The SQLite writer lock prevents overlapping CLI runs; an interrupted in-flight attempt remains visible for review.

The limit caps notices, **not money**. Max-effort reasoning and the size of originals affect cost. `--max-tokens` defaults to 32,768 for reasoning and visible JSON combined; a truncated response is retained for review, not silently retried or treated as a success. Set an OpenRouter spending limit before real runs. No paid model request was used to develop/test this project.

## Input and evidence

`examples/notices.json` documents the notice format. Use `config/sources.json` to enable adapters or add explicit manual feeds. `allowed_hosts` limits original-file fetches, including redirect targets. Production sources must be HTTPS; local HTTP is allowed only with the test-only loopback `--mock-url` option. Files are bounded by application-configured size limits (25 MiB/file, 50 MiB/notice by default, maximum 20 files); these are **not provider guarantees**.

Documents are downloaded as original bytes, SHA-256 hashed and stored under `var/files/<hash>`. PDFs and PNG/JPEG/WebP/GIF are sent as base64 original bytes. Unsupported files such as DOCX, ZIP or P7M are cached and listed as a coverage gap; they are not unpacked, converted or sent through an OCR fallback. Their presence deterministically forces `needs_review`. A download failure, excess size, or HTML response instead of an attachment stops that notice before a model call.

For an amendment, only changed files are sent with the previous structured extraction and complete current file manifest. If no valid prior extraction exists, all supported files are sent. The prompt requires preserving prior requirements and review for unresolved clauses, removed files, contradictions or missing annexes. This is cheaper than resending originals but is not independent verification of the prior interpretation. Changes to source content or attachment lists also create versions. Reappearing identical versions reuse their own cached result.

Without attachments, real source HTML/text/JSON is sent as text input. No pretend PDF is made. There is no OCR or local PDF parsing at any stage.

```sh
# Local facts import; no AI call, no external transmission:
./target/release/funding-rust import-facts examples/facts.json
```

Facts are scoped to ISTAT municipality, project and, for application-specific facts, call ID. Imports are always marked `user_declared`, even if the JSON claims `official`; they cannot overwrite bundled official evidence. Registry fields are read-only. The UI is deliberately read-only; edit a local facts JSON and import again to update declarations.

The generic six-operator evaluator uses `data/fields.json`. It does not branch on specific municipality or bando names. Unknown, contradictory, stale, historical and self-declared evidence cannot become verified eligibility. In particular, a population estimate cannot silently satisfy legal-population/reference-date conditions, and historical financial lists do not prove a current ordinary financial status.

## Coverage and interpretation limits

- EU SEDIA: official current index, English open/forthcoming records, types 1/2/8. Topic IDs remain distinct; cascade records keep their own IDs. The prompt receives decoded status/type labels and distinct child-call versus parent-topic/project identities. Status labels were verified against the official SEDIA FACET API; “Open for submission” is a source snapshot observation, not proof of present eligibility. Structured deadlines, closing dates and narrative duration/windows remain separate when they disagree. External cascade sites are not recursively crawled.
- EuroInfoSicilia: audited Bandi category union, including closed results/amendments; article HTML is inspected because WordPress bodies can omit attachment links. It is not every Regione department or all 5,116 regional posts.
- National sources: simple explicit/manual feed additions. No claim of an all-Italy or all-EU complete funding catalogue.
- A source article is not necessarily one unique funding opportunity. Multi-invitation articles and unsupported alternative legal routes must remain review cases. There is no automatic cross-source semantic merging.
- Source/page/count errors fail discovery rather than silently advertise complete refreshes. Metadata or absence from a listing does not itself close an opportunity. Current configured discovery still costs network/time even with a small AI limit.
- Conditions are a flat conjunction of validated `equals`, `one_of`, `range`, `compare`, `min_days`, and `manual` rules. Unsupported logic must be manual/review, not generated executable code.
- Every requirement cites a supplied source URL. The prompt and request-specific JSON schema contain the same citation URL allowlist as Rust validation; links merely mentioned in source content are not separately supplied evidence. Quote/page text is **model-reported, not independently validated against PDFs**. Strict JSON is not proof of legal interpretation. Nothing here grants legal clearance.
- Closed and ineligible records are hidden by default and available through the archive checkbox. Unsettled unknown/review cases stay visible; definite applicant mismatches stay excluded even when unrelated facts are unknown. Old or unavailable source checks need review; the web view does not fetch current documents.

## Extraction contract and zero-cost diagnostics

Luna extracts source facts; Rust decides applicability. Contract version 2 uses compact, cited conditions:

```json
{"id":"country","label":"Sede in Estonia","op":"one_of","field":"entity.country","values":["EE"],"citation_ids":["c1"]}
```

An additional `entity.kind = startup` condition describes a startup-only call. Both conditions can be valid extractions even when an Italian municipality does not match. Country values are ISO alpha-2 codes; entity types and Italian regions use the catalogue's canonical identifiers (`Sicilia`, not `Sicily`). Broad public-body eligibility is `entity.publicBody = true`, not a narrower invented entity type. Other categories stay manual when they cannot be represented faithfully. Numeric financial fields are EUR; other currencies stay manual.

- The model never supplies `scope` or `blocker`. Rust derives them from the evidence field. The legacy internal `municipality` scope means applicant-intrinsic information.
- Each operator has only its own arguments. A manual clause has `id`, `label`, `op`, `note`, and `citation_ids`; it cannot carry a field or unused numeric arguments.
- Conditions are ANDed; `one_of` allows alternative values of one field. Other alternatives/uncertainty remain manual. If missing coverage, contradictions or alternative eligibility routes could invalidate a definite condition, `review_reasons` keeps the entire result under review.
- Unknown deadline/status and unrelated project/manual conditions remain individually unknown. They do not undo a clearly cited, definite applicant country/type mismatch. They do prevent a positive match when there is no settled exclusion.
- `accepted` counts successfully extracted source facts without source-level review flags. It is **not** the number of eligible grants. CLI diagnostics print unresolved facts separately; local matching produces `excluded`, `ineligible`, `screening_match` or review states with reasons.
- Existing SQLite tables, cached versions, raw provider responses and legacy extractions are retained. Legacy review outputs are not silently repaired or promoted. Invalid legacy records remain visible for review; an invalid prior extraction cannot suppress unchanged originals during an already-requested amendment analysis. New provider responses must use version 2; legacy shapes are accepted only by the stored-data/replay path. Duplicate JSON keys are rejected rather than allowing a later key to erase uncertainty. Merely upgrading does not issue paid calls or rewrite old results.

To diagnose saved attempts without spending again:

```sh
./target/release/funding-rust --db var/funding.sqlite export-attempts --limit 10 --out attempts.json
./target/release/funding-rust replay attempts.json --out replay.json
```

Export opens SQLite read-only. Replay reads only the JSON file; it does not open SQLite, discover sources, fetch originals, call a model or import recovered facts. Both commands refuse to overwrite an existing output file. Exports contain original response bytes as saved, source/version metadata and historical usage, so treat them as private diagnostic files.

Reports distinguish provider failures/truncation, invalid JSON, contract errors and valid-but-unresolved facts. A bare SQL export with only `attempt_id`, `notice_id`, `response` and `validation_error` is accepted for syntax/provider diagnosis; without independent source metadata it reports structural errors and source-reported manual/review reasons separately and cannot validate citation URLs. Historical versions lacking their original notice-URL snapshot also remain diagnosis-only. No quotation is independently verified against original PDFs. Do not use `--retry-failed` to diagnose: that flag can spend money again.

## Actual cost reporting

Every response records provider generation ID, provider, returned usage and raw response in SQLite, including invalid/truncated responses. The CLI prints prompt, completion and reasoning tokens reported, the sum of returned `usage.cost` in USD, and mean actual cost per called/processed notice when all calls supplied cost. Completion tokens already include reasoning tokens; they are not added twice.

Missing cost is **unknown**, never assumed zero. Any missing cost makes aggregate averages `null`, and the returned-cost sum is explicitly a partial subtotal. No forecast is substituted for actual charges. Returned generation IDs can be reconciled with OpenRouter's generation endpoint if needed; the app does not automatically retry or spend to recover them.

## HTTP and storage

- `GET /`: server-rendered interface, no JavaScript
- `GET /api/municipalities`
- `GET /api/opportunities?municipality=088001&project=school&archive=1`

Only GET is accepted. All text is escaped; links use HTTP(S); responses have a restrictive CSP. The service binds loopback by default and has **no authentication or account separation**. Keep it local, or place it behind your own authenticated HTTPS proxy. CLI facts import is for trusted local operators. No public hosting/deployment is configured.

Use `--db /path/funding.sqlite` and `run --cache /path/files` for persistent storage. Back up SQLite with its backup API or stop writers before copying the database; WAL sidecars can hold recent writes. Back up original files as well. `Cargo.lock` is committed to the deliverable. Bundled SQLite removes a system SQLite dependency.

`deploy/` contains optional systemd examples. Inspect paths, create a dedicated service account and protected environment file yourself, set a spending cap, and enable the timer only when ready. Nothing is automatically installed or scheduled.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --locked
```

Tests use local HTTP mocks and fixtures, never paid requests. Compact-contract regressions cover Estonian startup versus Italian municipality, open municipal/public-body calls, genuine eligibility ambiguity, unknown unrelated project/deadline facts, both reported invalid scope/manual combinations, and read-only export/replay. They cover exact original-byte payloads, native-only/max-effort parameters, strict schema, bounded calls, changed files, cached versions, partial/failed usage, unknown cost, isolated facts, all six rule operators and safe HTML filtering. An optional ignored adapter regression accepts the separately retained audit snapshots through `FUNDING_AUDIT_DIR`; those bulky source snapshots are not required or bundled.

Provider docs checked 6 October 2026:
- [Native PDF inputs](https://openrouter.ai/docs/guides/overview/multimodal/pdfs)
- [Reasoning effort and billing](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens)
- [Structured output](https://openrouter.ai/docs/guides/features/structured-outputs)
- [Provider routing](https://openrouter.ai/docs/guides/routing/provider-selection)
- [Luna model](https://openrouter.ai/openai/gpt-6-luna)

The request explicitly selects `file-parser.pdf.engine=native`, `provider.only=["openai"]`, `require_parameters=true`, and `allow_fallbacks=false`. The `openai` provider family may include variants such as Flex; this is not an exact standard-endpoint pin. File annotations, unexpected models, refusal, non-stop finish reasons and malformed output are rejected for review. The exact paid-provider path still needs an operator-authorized pilot; mock tests cannot establish live output quality or spend.
