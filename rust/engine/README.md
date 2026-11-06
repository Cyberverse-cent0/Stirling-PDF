# stirling-engine (Rust)

Rust replacement for the Python AI engine in `engine/` (FastAPI + pydantic-ai).
Same HTTP contract, same `STIRLING_*` env vars, same port (default `5001`).
The Python engine remains the parity oracle until cutover; see
`../../rust/MIGRATION.md` and the migration plan for phase tracking.

## Status

### Implemented

- **HTTP scaffold** — Axum service: `/health` (Python-identical shape),
  `X-User-Id` propagation, `X-Engine-Auth` shared-secret middleware
  (constant-time compare, public paths `/health`, `/docs`, `/redoc`,
  `/openapi.json`, 503 fail-closed when `REQUIRE_AUTH` set without secret),
  FastAPI-style `{"detail": ...}` errors, CORS, request tracing.
- **Contracts** — serde ports of all `stirling.contracts.*` modules
  (`src/contracts/`): discriminated unions are internally tagged on
  `outcome`/`kind`/`phase`/`type` exactly like the pydantic
  `Field(discriminator=...)` unions; `enabled_endpoints` drops unknown tool
  paths like `drop_unknown_tool_endpoints`. `ToolEndpoint`/`AgentToolId`/
  `ToolId` path enums live in `src/models/tool_models.rs`.
- **Chunking** — `src/documents/chunker.rs` + `src/docparse/chunking.rs`:
  paragraph→sentence packing, word-boundary overlap tails, heading-aware
  block packing, page metadata. All lengths are **character counts**
  (Python `len()` semantics), never UTF-8 bytes.
- **Embedder** — `src/documents/embedder.rs`: OpenAI `/embeddings` wire
  format via reqwest; Voyage (`input_type` forwarded), OpenAI, Ollama/custom
  base URLs; config-push resolution mirrors `_build_embedder`.
- **Document stores** — `DocumentStore` trait (`src/documents/store.rs`)
  with two backends:
  - `SqliteVecStore` — schema-identical port incl. WAL, busy_timeout,
    per-`(collection, owner_id)` vector tables, ACL-gated reads. KNN is
    pure Rust: embeddings are f32-le blobs in plain tables, scored by a
    rayon-parallel dot-product scan over an in-memory decoded cache —
    equivalent to the old vendored sqlite-vec `vec0` tables, which ran
    the same brute-force search without an ANN index.
  - `PgVectorStore` — sqlx pool, lazy schema bootstrap (`CREATE EXTENSION
    vector` first), vectors bound as pgvector text literals.
- **DocumentService** — `src/documents/service.rs`: replace-ingest,
  pre-chunked ingest (embed-before-wipe ordering preserved), ACL search,
  ordered page reads, owner purge, expiry reaping.
- **Routes** — `/api/v1/health`, `/api/v1/documents` (ingest, delete
  by-id/by-owner, classify), `/api/v1/docparse` (`/capabilities`, `/ingest`
  with the same 422 validations).
- **LLM provider layer** — `src/llm/`: Anthropic Messages and
  OpenAI-compatible chat completions over reqwest; structured output via a
  forced `final_result` tool call on both wire formats; provider resolution
  mirrors `runtime._build_model` (`anthropic:`/`openai:`/`ollama:` prefixes,
  explicit provider/key/base_url for config push); shared
  `model_max_concurrency` semaphore across fast+smart tiers.
- **Agent runtime** — `src/agents/runtime.rs`: multi-turn tool-calling loop
  over both provider wire formats (OpenAI `tool_calls`/`role:"tool"`,
  Anthropic `tool_use`/`tool_result` blocks), `final_result` tool for
  structured output (the `ToolOutput` mechanism), iteration bound,
  per-tool error surfacing.
- **RAG capability** — `src/documents/rag_capability.rs`: `search_knowledge`
  tool + dynamic/static instructions, per-run search budget.
- **Agents (single-shot)** — `src/agents/document_classifier.rs` +
  `POST /api/v1/documents/classify`, and `src/agents/pdf_comment.rs` +
  `POST /api/v1/ai/pdf-comment-agent/generate`: prompt build → structured
  call → output validation, covered by live HTTP roundtrip tests against a
  stub provider.
- **Parity harness** — `scripts/parity_check.sh` diffs Python vs Rust JSON
  on the deterministic endpoints (`/health`, docparse ingest + 422s,
  auth status). Run `task rust:engine:parity` with both engines up.
- **PDF comment agent** — `src/agents/pdf_comment.rs` +
  `POST /api/v1/ai/pdf-comment-agent/generate`: single-shot structured
  output with ordinal→chunk-id mapping and out-of-range drops.
- **Agents router** — `GET /api/v1/agents/capabilities` (manifest with
  schemars-derived `input_schema` per capability, matching Python's
  pydantic `model_json_schema()`) and `POST /api/v1/agents/next-action`
  (stub parity: Python's `ExecutionPlanningAgent` is also unimplemented).
- **JSON Schema derives** — all `src/contracts/*` types and
  `models/tool_models.rs` enums derive `schemars::JsonSchema` (the
  `chrono` feature is enabled for `DateTime<FixedOffset>` fields).

### Planned (not yet ported)

- Agent runtime gaps: streaming responses, `NativeOutput`/`ToolOutput`
  distinction (loop always uses the tool mechanism — fine for every
  supported provider), multi-turn chat-history messages (callers currently
  flatten history into the prompt, matching most Python agents),
  `MockModel` test double, tool `prepare` hooks beyond search budget.
- Remaining agents (orchestrator + NDJSON streaming, pdf-edit/questions/
  review/create, drafts, execution, ledger, contradiction, whole-doc
  reader, chunked reasoner, math auditor, user-spec).
- Remaining routers: orchestrator (NDJSON streaming + heartbeats),
  pdf-edit, pdf-questions, agent-drafts, execution planning,
  math-auditor (ledger), config (push + encrypted cache).
- Config cache (Fernet-compatible encryption, HKDF key derivation, atomic
  0600 writes, mtime polling) and the documents reaper background task.
- Minimal PostHog capture; OpenTelemetry beyond request tracing.
- Python test-suite port (~47 pytest files) and the Java↔Rust parity
  harness.

## Configuration

Identical env vars to the Python engine (`STIRLING_*`); `.env` then
`.env.local` are loaded from `rust/engine/` (local wins). Notable:

| Var | Default | Notes |
| --- | --- | --- |
| `PORT` | `5001` | bind port (bind addr `0.0.0.0`) |
| `STIRLING_DOCUMENTS_BACKEND` | `sqlite` | `sqlite` \| `pgvector` |
| `STIRLING_DOCUMENTS_SQLITE_PATH` | `data/documents.db` | relative to crate root |
| `STIRLING_DOCUMENTS_PGVECTOR_DSN` | — | required for pgvector |
| `STIRLING_RAG_EMBEDDING_MODEL` | — | `voyageai:model`, `openai:…`, or bare name |
| `STIRLING_RAG_CHUNK_SIZE` / `_OVERLAP` | `512` / `64` | characters |
| `STIRLING_RAG_TOP_K` | `5` | |
| `STIRLING_ENGINE_SHARED_SECRET` / `_REQUIRE_AUTH` | — / `false` | Java↔engine auth |
| `STIRLING_REQUIRE_USER_ID` | `false` | per-router 401 gate |

## Tool-parameter types (typify)

`src/models/tool_params.rs` is generated from the Java OpenAPI spec —
the Rust counterpart of `engine/scripts/generate_tool_models.py`:

```bash
# from rust/engine/, with the Java backend running on :8080
./scripts/generate_tool_params.sh
# or against a saved spec:
./scripts/generate_tool_params.sh /path/to/SwaggerDoc.json
```

Until generated, `ToolOperationStep.parameters` / execution `parameters`
are `serde_json::Value` — wire-compatible; typed models are needed only
for schema-driven prompt construction in the agent layer.

## Development

```bash
cargo build -p stirling-engine            # from rust/
cargo test -p stirling-engine             # chunking + store regression tests
cargo clippy -p stirling-engine --all-targets --all-features -- -D warnings
cargo fmt -p stirling-engine -- --check
cargo run -p stirling-engine              # serves on :5001
```

Taskfile: `task rust:engine:check` runs fmt + clippy + tests for this crate.

## Known divergences from Python

- Pydantic `Field(ge=…, min_length=…)` wire validation is not yet enforced
  on request bodies (typed shapes only); agent-side schema validation
  covers LLM outputs. Track before cutover.
- Request bodies accept camelCase (the Java wire format); Python also
  accepts snake_case aliases — not replicated.
- `expires_at` uses RFC 3339 (`DateTime<FixedOffset>`); pydantic accepts
  a wider ISO range.
- Voyage `input_type` is sent as a top-level JSON field (same effective
  wire shape as pydantic-ai's `extra_body` merge).
