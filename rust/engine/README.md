# stirling-engine (Rust)

Rust replacement for the Python AI engine in `engine/` (FastAPI + pydantic-ai).
Same HTTP contract, same `STIRLING_*` env vars, same port (default `5001`).
**Cut over**: `task dev:all`, `task dev:strangler`, and `task engine:bg` all
run this binary; the Python engine remains under `task engine:*` as the
parity oracle until it is retired.

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
    pure Rust: embeddings are normalized f32-le blobs in plain tables,
    scored by a rayon-parallel dot-product scan over an in-memory decoded
    cache. Equivalent to the old vendored sqlite-vec `vec0` tables, which
    ran the same brute-force search without an ANN index. See
    "Vector store" below.
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
- **Full agent surface** — orchestrator (NDJSON streaming + heartbeats),
  pdf-edit, pdf-questions, pdf-create, pdf-review, agent drafts/revise,
  execution planning, math-auditor ledger (examine/deliberate),
  contradiction detection, chunked reasoner/mapper, whole-doc reader,
  user-spec extraction (`src/agents/`).
- **Config push + encrypted cache** — `src/config_push.rs` +
  `src/config_cache.rs`: Fernet-compatible encryption (verified against
  Python-produced tokens), HKDF key derivation, atomic 0600 writes,
  mtime-poll adoption across workers.
- **Wire parity** — every contract field accepts both the camelCase alias
  and the snake_case field name (pydantic `validate_by_name` +
  `validate_by_alias`); `expires_at` accepts pydantic's wider ISO-8601
  range; request models enforce the pydantic `Field(ge/min_length/max)`
  constraints as 422s; malformed JSON bodies reject as 422
  `{"detail": ...}` via `src/extract.rs`.

### Planned (not yet ported)

- Agent runtime gaps: `MockModel` test double for scripted agent tests;
  remaining Python pytest files (agent/route-level behaviour suites
  under `engine/tests/agents`, `tests/contradiction`, `tests/docparse`,
  `tests/ledger`, `tests/pdf_comment`).
- OpenTelemetry beyond request tracing.
- Python engine retirement once test parity is deemed sufficient.

## Vector store (SqliteVecStore)

`src/documents/sqlite_vec_store.rs` is a pure-Rust replacement for the
Python store's vendored sqlite-vec `vec0` extension — no C code, no
`build.rs`, no loadable extension.

**Storage.** Each `(collection, owner_id)` pair gets a plain table
`vec_<owner>_<collection>` (names sanitized to `[A-Za-z0-9_]`; on
collision a hash suffix is appended so distinct pairs never share a
table) holding one `embedding BLOB` column: `dim` little-endian f32s,
L2-normalized at write time. `collections(dim, table_name)` records the
layout; `documents`, `document_pages`, `document_acl`,
`documents_meta` are unchanged from the Python schema.

**Similarity.** Stored and query vectors are unit-normalized, so the dot
product *is* the cosine similarity; scores are `max(0, dot)` returned as
`f64` — the same `max(0, 1 - L2²/2)` the vec0 `MATCH` query produced.
Normalization sums squares in f64 (f32 accumulation could overflow to an
infinite norm); a zero vector passes through unchanged and scores 0.
Top-k uses `select_nth_unstable_by` with a `(score desc, rowid asc)`
tiebreak, so results are deterministic. Dimension mismatches are
rejected both at ingest (per-batch consistency) and at query time.

**Search path.** The first search on a collection decodes every blob
into a row-major `Vec<f32>` matrix cached under the table name; repeat
searches skip SQLite entirely for scoring and only hydrate the top-k
`documents` rows. The decoded cache is an LRU bounded by
`STIRLING_DOCUMENTS_CACHE_MB` (default 256 MiB) accounting `rowids` +
vector payload bytes; the newest entry is always kept even when it alone
exceeds the budget, and evicted entries are transparently rebuilt on next
use. It is invalidated on every mutation — `add_documents`,
`delete_collection`, `purge_owner`, `reap_expired`.

Cached searches take the index `Arc` under the connection lock, release
it for the rayon scan, then re-acquire the lock and verify the cached
entry is still the same `Arc` before hydrating — a committed write always
invalidates the entry, so pointer equality proves the snapshot is
current. Stale snapshots retry (up to 3 times, then a fully serialized
fallback), which lets concurrent cached searches run in parallel while
writes, hydration and index builds stay serialized on the connection
mutex.

**Legacy migration.** Databases written by the Python engine contain
`vec0` virtual tables that this binary cannot even `DROP` (the module
isn't registered, so prepare fails). On open, `init_schema` finds them
via `sqlite_master ... LIKE '%USING vec0%'` and removes the catalog rows
directly with `PRAGMA writable_schema` — the vtab row plus its shadow
tables (`<name>_chunks`, `<name>_rowids`, `<name>_vector_chunksNN`, …,
matched by name prefix and excluding names registered in `collections`)
— then bumps `schema_version` and deletes the derived `documents` /
`collections` rows in one transaction. `documents_meta`, `document_pages`,
`document_acl` and unrelated tables survive; orphaned shadow-table pages
remain until `VACUUM`. The operation is idempotent, and a `collections`
row whose table is missing (partial migration) is reset on next ingest.
Only derived embeddings are lost — the collection must be re-ingested.

**Benchmarks** (`cargo test --release --test vec_store_bench -- --ignored
--test-threads=1`, ephemeral DB, 8 threads, LCG vectors, 1 024-doc ingest
batches):

| n × dim | ingest | first search | cached search | 4-way concurrent |
| --- | --- | --- | --- | --- |
| 1 000 × 384 | 37 ms | 3.4 ms | 1.2 ms | 0.9 ms/op |
| 20 000 × 768 | 0.90 s | 84 ms | 12 ms | 14 ms/op |
| 50 000 × 768 | 1.79 s | 265 ms | 32 ms | 33 ms/op |

The scan is memory-bandwidth-bound dot products (~22–28k docs/s ingest);
there is no ANN index — brute force is the documented design, matching
the vec0 behavior it replaces. Concurrent cached searches now scan in
parallel; for large collections the scan saturates all cores anyway, so
per-op latency under 4-way concurrency is roughly the cached latency,
while small collections benefit ~3× from overlapping scans.

## Configuration

Identical env vars to the Python engine (`STIRLING_*`); `.env` then
`.env.local` are loaded from `rust/engine/` (local wins). Notable:

| Var | Default | Notes |
| --- | --- | --- |
| `PORT` | `5001` | bind port (bind addr `0.0.0.0`) |
| `STIRLING_DOCUMENTS_BACKEND` | `sqlite` | `sqlite` \| `pgvector` |
| `STIRLING_DOCUMENTS_SQLITE_PATH` | `data/documents.db` | relative to crate root |
| `STIRLING_DOCUMENTS_PGVECTOR_DSN` | — | required for pgvector |
| `STIRLING_DOCUMENTS_CACHE_MB` | `256` | decoded vector-index cache budget |
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

`tool_params.rs` is committed (≈200 types from the full component map);
regenerate it whenever the Java OpenAPI spec changes. The generator
handles the two OpenAPI-3.0-vs-JSON-Schema gaps itself: boolean
`exclusiveMinimum`/`exclusiveMaximum` are rewritten to numeric bounds,
and `#/components/schemas/*` `$ref`s are retargeted into a synthetic
`definitions` map (typify only emits titled root types).

`ToolOperationStep.parameters` / execution `parameters` still stay
`serde_json::Value` — the typed models exist for schema-driven prompt
construction in the agent layer, which is not yet wired to them.

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

- Agent *output* schema constraints (contradiction, ledger, pdf-comments
  response models) are validated agent-side, not at the serde layer —
  matching Python's split between wire validation and
  `NativeOutput`/`ToolOutput` schema enforcement.
- Naive `expires_at` values (no offset) are interpreted as UTC; pydantic
  keeps them naive and lets the store compare them.
- 422 bodies carry a single `detail` string rather than pydantic's
  per-field error list — Java only surfaces the status + detail upstream.
- Voyage `input_type` is sent as a top-level JSON field (same effective
  wire shape as pydantic-ai's `extra_body` merge).
