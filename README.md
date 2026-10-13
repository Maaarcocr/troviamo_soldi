# troviamo_soldi

A Rust research assistant for finding funding that a municipality can actually pursue. One Rig agent discovers current calls, follows official documents, investigates missing facts, and records its work in SQLite and a local research workspace. Pachino (ISTAT `089014`) is the default; another municipality uses the same tools and knowledge structure.

The research path replaces the fixed “one PDF batch → extracted fields → universal rules” approach. The previous ingestion path is retained for existing data and diagnostics, without silently treating its imports as researched opportunities.

## What the agent does

- Searches for current calls, rather than processing the oldest stored IDs first.
- Opens official pages, follows relevant documents and amendments, and reads originals when needed.
- Checks which documents actually belong to a call. Nearby “related” articles do not establish its terms.
- Researches missing municipal facts and saves dated, scoped evidence for reuse.
- Distinguishes the Comune, associations, specific sites and proposed projects.
- Separates applicant eligibility from whether a real project is ready to apply.
- Saves useful results as it works and resumes from persisted checkpoints.

The core records are calls, evidence, facts and assessments. There is no fixed catalogue of every possible eligibility fact. The agent reuses existing fact names where meanings match and can add a genuinely new one. Rust handles persistence, provenance checks, simple calculations, date handling and access boundaries; the agent interprets the documents. There is no QuickJS or general-purpose shell/SQL tool.

## Build and view

Install the Rust version required by `Cargo.toml`:

```sh
cargo build --release --locked
./target/release/funding-rust init
./target/release/funding-rust serve
```

Open http://127.0.0.1:8080. The main view shows researched opportunities, their amount and qualifications, why they may fit, application and accreditation deadlines, essential unknowns, and next actions. It does not populate the feed with example calls. The previous extracted archive remains separately available at `/archive`.

The UI is read-only and makes no model calls. It binds loopback by default and has no user authentication. Use your own authenticated HTTPS proxy before exposing it publicly. Keys stay in server configuration, never in browser code.

## Run real research

Configure `OPENROUTER_API_KEY` externally on the server. The default uses OpenRouter’s server-side web-search tool with that same key, alongside the agent’s local source and memory tools. Never commit credentials or paste them into a research objective. `--allow-paid` is an explicit operator opt-in; building, serving, and reading existing records do not spend model credits.

```sh
# Externally configure OPENROUTER_API_KEY first.
./target/release/funding-rust research --allow-paid

# Use another model without code changes.
./target/release/funding-rust research --allow-paid --model YOUR_OPENROUTER_MODEL

# Operator-chosen checkpoint budget; this is a call count, not a dollar cap.
./target/release/funding-rust research --allow-paid --max-model-turns 20

# Inspect actual checkpoint, tool trace and reported cost without requests.
./target/release/funding-rust research-status --job JOB_ID --out research-trace.json

# Continue the job ID printed by the earlier run.
./target/release/funding-rust research --allow-paid --job JOB_ID
```

The default model preserves the existing configuration choice, `openai/gpt-6-luna`; `FUNDING_RESEARCH_MODEL` or `--model` overrides it. Availability, capability and pricing must be checked with your provider before a live pilot. The application does not claim that this default has passed a research-quality evaluation.

There is no hidden tiny output-token cap or model-request timeout. Optional `--max-model-turns` is a transparent operator limit. Reaching it means **research incomplete**, not ineligible. Provider-side spending limits are still necessary: a model call count is not a monetary budget. OpenRouter server-side search also consumes provider credits; an optional external search/rendering service has separate charges. Reported costs are retained; missing cost is unknown, not zero.

The job stores the conversation/driver checkpoint and tool results. An interrupted or failed provider request may already have been billed. Do not blindly launch a second job to retry it. Review the saved job state before resuming. Persistence does not provide a distributed exactly-once billing guarantee.

## Source tools and optional rendering

Web search runs through [OpenRouter’s server-side search tool](https://openrouter.ai/docs/guides/features/server-tools/web-search), with the existing OpenRouter key. Results are discovery leads; the agent still fetches the authoritative pages and documents before recording evidence. If you prefer a separate search service, `BRAVE_SEARCH_API_KEY` enables the optional direct Brave tool. `FUNDING_SEARCH_ENDPOINT` can point to a Brave-compatible public HTTPS endpoint.

Static pages are fetched and archived directly. PDF and image originals are delivered to the selected model, so its provider must support native PDFs/images. DOCX/ODT text can be read from their XML; layout and embedded images are not thereby verified. Unsupported formats remain explicit research gaps.

This first slice does not bundle or install a browser. JavaScript-only pages need an optional isolated renderer: `FUNDING_RENDER_ENDPOINT` accepts `POST {"url":"https://…","network_policy":"public-only"}` and returns `{"final_url":"https://…","html":"…"}`. `FUNDING_RENDER_API_KEY` optionally adds Bearer authentication. The renderer must independently block private-network access on every navigation, redirect and subresource. Without it, the agent receives a clear rendering-unavailable result and must report that limitation. Launching a naïve local Chromium process would bypass the source-fetch network boundary, so that is not done automatically.

## Evidence and reuse

Original sources are content-addressed in the research workspace. Evidence records retain source versions, retrieval time and quotations/locators. Facts have explicit subjects and time scope. An assessment depends on the evidence and facts it used, so relevant changes make it stale rather than silently preserving an old conclusion.

An official list omitting “Comune di Pachino” says something about that specific list, entity and period. It does not establish a timeless absence of funding; an award to a Pro Loco cannot be attributed to the Comune. Unknown site ownership or project readiness is not false, and a failed search is not a permanent negative cache.

Date-only deadlines remain date-only. Accreditation and submission are different actions. An open call that admits municipalities is not evidence that a chosen site or proposed project qualifies. Closed calls and unsuccessful imports are not shown as current opportunities.

Retrieved content is untrusted. The model has only read-only public-source tools, workspace-limited reads, calculation tools, and typed research-memory operations. It cannot execute arbitrary commands, issue SQL, send messages, submit applications or modify server settings. Network controls reject non-public targets and unsafe redirects; filesystem tools reject path traversal and symlink escapes.

## Honest verification

This implementation is not proof of research quality. Compilation, linting and checks of real SQLite/filesystem behavior establish software properties only. No fake model replies or synthetic research scenarios are used to claim discovery or interpretation quality.

A meaningful pilot needs actual provider credentials and an explicit spending budget. Run it against the research goal, inspect the real tool trace and source passages, check whether it found useful current calls, and challenge unsupported positive and negative conclusions. Record actual cost and what information it reused. Useful hand-review questions include whether it kept related attachments separate, distinguished the applicant from a project/site, and revisited an unresolved fact after new evidence arrived. Do not preload desired conclusions and call that independent discovery.

No paid research, server deployment, or live quality validation is implied by publishing the code.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

## Existing data and operations

The research tables are additive. Existing notices, versions, evidence, original responses and costs remain intact; they are not automatically promoted into new verified assessments. The old CLI and its diagnostic/replay commands remain available for historical inspection, described in [legacy pipeline documentation](docs/legacy-pipeline.md).

Use `--db /path/funding.sqlite` and `research --workspace /path/research` for persistent storage. Back up SQLite consistently using its backup API or stop writers first; WAL files can contain recent writes. Back up the research workspace too. Research traces can contain private user-supplied objectives and source data: keep the database, logs and backups private.

Nothing installs services, deploys the app, schedules research, merges a pull request or authorizes provider spend automatically.
