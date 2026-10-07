# troviamo_soldi

One Rust binary, SQLite, and a read-only Italian web UI for screening funding calls against Sicilian municipalities.

1. Discover notices and download their original files.
2. Skip a notice if its source content and file hashes are unchanged.
3. For a new or changed notice, send the current text and all supported originals to OpenRouter `openai/gpt-6-luna`, with max reasoning and strict JSON. Make one request at most.
4. Save the facts and actual reported cost. Rust filters municipalities locally, without another AI call.

No OCR, PDF text extraction, classifier stage, model eligibility verdict, larger-model fallback, or automatic paid retry. A changed notice is self-contained: previous model output is never fed back as evidence.

## Run

Install stable Rust, then:

```sh
cargo build --release --locked
./target/release/funding-rust init
./target/release/funding-rust serve
```

Open http://127.0.0.1:8080. The initial archive is empty. The app bundles 391 Sicilian municipalities and 511 previously collected official evidence records; it does not load example grants as real opportunities.

```sh
# Discover source content, without AI calls:
./target/release/funding-rust discover --out notices.json

# Download/hash at most 5 new or changed notices, without AI calls:
./target/release/funding-rust run --limit 5 --dry-run

# After configuring OPENROUTER_API_KEY and an OpenRouter spending limit:
./target/release/funding-rust run --limit 5 --allow-paid

# Use a populated discovery export or manual input instead:
./target/release/funding-rust run --input notices.json --limit 5 --allow-paid

./target/release/funding-rust status
```

`daily` is an alias of `run`. `--limit` is mandatory and counts attempted **bandi**, including input/download/provider failures and review cases. Unchanged cached versions consume no slot and make no model call. Every attempted notice makes at most one model request. A writer lock prevents overlapping CLI runs.

The limit caps notices, **not money**. `--max-tokens` defaults to 32,768 for reasoning plus visible JSON. Truncated output is saved for inspection, not retried. A timeout or interrupted request may have been billed: `--retry-failed` explicitly retries failed, interrupted or review versions and can duplicate charges. Do not use it merely to inspect a failure.

## Source input

Configure adapters and download hosts in `config/sources.json`; `examples/notices.json` shows the manual notice format.

Each notice needs nonblank `source_text` or original documents. A bare ID/title/URL is not sufficient and is rejected before a model call. Use the populated `discover` export, paste the actual source text/HTML/JSON, or supply the original file URLs. The app does not fetch a bare URL and assume a portal shell contains the call. Supplied HTML is accepted without language, length or JavaScript-content guessing.

Original bytes are SHA-256 hashed and cached under `var/files/<hash>`. PDFs and PNG/JPEG/WebP/GIF are sent directly as base64 originals. DOCX, ZIP, P7M and other unsupported files are retained as coverage gaps; they are not unpacked, converted or OCRed. If there is no source text and every file is unsupported, processing stops before a paid request. With usable input plus unsupported files, extraction can proceed but remains under review.

Downloads require an allowed HTTPS host, including redirect targets. Limits are 25 MiB/file, 50 MiB/notice and 20 files by default; these are application limits, not provider guarantees. Download failure, oversize content or HTML returned as an attachment stops the notice before a model request. Loopback HTTP is only for the test `--mock-url` path.

The request includes the current `as_of` time for interpreting dated windows. Time alone does not change the source fingerprint or trigger another call.

Changed notice text, titles or attachments produce a new fingerprint. All current supported originals are then sent again. Removed originals are flagged for review. This can use more input tokens than sending only an amendment, but avoids carrying a previous model interpretation forward. Identical historical versions reuse their own cached result.

### Coverage

- EU SEDIA: current official index, English open/forthcoming records, types 1/2/8. Child cascade calls keep their own IDs and are labelled separately from parent topics/projects. Status/type codes are decoded from the official FACET API. Conflicting source dates and narratives are retained.
- EuroInfoSicilia: audited Bandi category union, including closed results/amendments. Article HTML is checked because WordPress bodies can omit attachment links.
- National sources: explicit/manual feed additions.

This is not a complete all-Italy or all-EU catalogue. External cascade sites are not recursively crawled. External annex hosts outside the configured allowlist are not fetched; some real annexes therefore remain outside coverage. Source errors fail discovery rather than silently claim a complete refresh. A small AI limit still requires configured source discovery. Absence from a listing does not close a call, and an article can contain more than one opportunity.

## Facts and municipality screening

Luna supplies compact source facts, not an eligibility decision. Descriptive budgets and scoring information belong in `summary`. `requirements` contains only mandatory applicant, project or application conditions.

```json
{"id":"country","label":"Sede in Estonia","op":"one_of","field":"entity.country","values":["EE"],"citation_ids":["c1"]}
```

Conditions are ANDed. The six operators are `equals`, `one_of`, `range`, `compare`, `min_days` and `manual`. Each structured condition must be necessary on its own; alternatives that cannot be expressed faithfully stay together in a manual condition. Rust derives evidence scope from the field catalogue; the model never supplies a scope or global blocker.

Countries use ISO alpha-2 codes; applicant types and regions use the catalogue's identifiers. Broad public-body eligibility uses `entity.publicBody=true`. Financial numeric fields use EUR; unsupported currencies/categories remain manual. Unknown values stay unknown, never false or invented zeros.

Every requirement references supplied source citations. A quote can be a single literal value such as `30500`; it must be nonblank and free of unsafe controls. An unknown locator is `null`. An empty extraction may have no citations and remains under review. Duplicate JSON keys, invalid types, missing references and unsupported source URLs are rejected. Quotes are never padded, repaired or independently verified against PDFs. Valid JSON does not prove a correct interpretation.

`extracted` means usable source facts were saved without an input-coverage failure or empty requirement list. It does **not** mean eligible grants. Unknown dates and individual manual conditions are reported separately. Rust's municipality matcher produces excluded, ineligible, screening-match or review results. Definite applicant mismatches can exclude a call despite unrelated unknown project facts; missing evidence never establishes a positive match.

```sh
# Local municipality/project facts, without AI calls:
./target/release/funding-rust import-facts examples/facts.json
```

Imports are always `user_declared` and cannot overwrite bundled official evidence. Facts are scoped to municipality, project and, where needed, call ID. Historical, stale, contradictory or self-declared evidence cannot become verified eligibility. Population basis/reference dates and current financial status remain distinct from estimates and historical lists. The UI is read-only.

## Inspect failures without spending again

```sh
./target/release/funding-rust --db var/funding.sqlite export-attempts --limit 10 --out attempts.json
./target/release/funding-rust replay attempts.json --out replay.json
```

Export opens SQLite read-only. Replay reads only its input file: no database writes, downloads or model calls. Neither overwrites an existing output file. Exports contain original provider responses, source/version metadata and usage; keep them private.

Replay separates provider failures, truncation, invalid JSON, invalid extraction and unresolved facts. Citation URL validation needs independent source metadata; a bare SQL export or historical record without its notice snapshot is diagnosis-only. No replay result verifies quotations or imports recovered facts.

Existing SQLite records, raw responses and review flags remain intact. Legacy extraction shapes are read through internal compatibility adapters. New model responses use version 3; a small storage adapter retains the existing version-2 shape for Rust coverage warnings. New successful rows use `extracted`; old `screened` rows remain readable. Upgrading does not itself issue calls, clean corrupted quotes or rewrite stored results. This release removes generated warning prose from SEDIA source text, so a later explicitly paid run can see affected EU records as changed and re-extract them, bounded by `--limit`. There is no uncapped migration or automatic re-extraction.

## Actual cost

Every response stores its generation ID, provider, raw response and reported usage, including invalid/truncated responses. Reports show prompt/completion/reasoning tokens, returned `usage.cost` in USD, and mean actual cost per called/processed notice when every call reports cost. Reasoning is already included in completion tokens and is not added twice.

Missing cost is unknown, never zero. Any missing cost makes averages `null` and labels the returned-cost sum a partial subtotal. No price-table estimate replaces actual charges. Saved generation IDs can be reconciled with OpenRouter; the app does not automatically retry to recover them.

## Storage and serving

- `GET /`
- `GET /api/municipalities`
- `GET /api/opportunities?municipality=088001&project=school&archive=1`

The web UI escapes text, restricts links to HTTP(S), sets a restrictive CSP and accepts only GET. It binds loopback by default and has no authentication/account separation. Keep it local or behind your own authenticated HTTPS proxy. Closed/ineligible records are hidden unless the archive checkbox is enabled; uncertain results stay visible. The web view does not refresh source documents.

Use `--db /path/funding.sqlite` and `run --cache /path/files` for persistent storage. Back up SQLite with its backup API or stop writers first; WAL sidecars may contain recent writes. Back up cached originals too. SQLite is bundled, and `Cargo.lock` is committed.

`deploy/` contains optional systemd examples. Inspect paths, protect credentials, set a spending cap and enable scheduling yourself. Nothing here installs, deploys or schedules the app automatically.

## Verify

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Tests use local HTTP mocks and fixtures, never paid calls. They cover literal short/missing evidence, strict types and references, unknown versus false, local applicant exclusion, direct original bytes, full current input for changed notices, cache reuse, zero-call input failures, call limits, actual/unknown costs, private offline replay and safe HTML. One optional ignored adapter test needs separate `FUNDING_AUDIT_DIR` snapshots.

Requests select native PDF handling, the `openai` provider family, required-parameter support, no fallback and max-effort Luna. The provider family may include Flex. Unexpected models, parsed-file annotations, refusal, non-stop finish reasons and malformed output remain rejected. Mock tests cannot establish live output quality or future spend; this simplicity pass makes no paid calls.

Provider references: [PDFs](https://openrouter.ai/docs/guides/overview/multimodal/pdfs), [reasoning](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens), [structured output](https://openrouter.ai/docs/guides/features/structured-outputs), [routing](https://openrouter.ai/docs/guides/routing/provider-selection), [Luna](https://openrouter.ai/openai/gpt-6-luna).
