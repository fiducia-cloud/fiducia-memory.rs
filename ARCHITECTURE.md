# fiducia-memory — Architecture

This document explains how the crate actually works, module by module and table
by table, with every statement grounded in the source. For a quick start and the
API cheat-sheet, see [README.md](README.md).

## 1. What this is and where it sits

`fiducia-memory` is the Fiducia platform's **shared brain**: a Rust library plus
an axum HTTP service (one binary, `src/main.rs`) that stores what a fleet of AI
agents has learned — memories, provenance-bearing facts, and contestable claims
— in PostgreSQL + pgvector, and answers recall queries with a ranked,
explainable, token-bounded context pack.

Its place in the platform is defined by what it deliberately does **not** do:

- **Coordination and authority live in `fiducia-node`** (leases, fencing, "who
  owns this task"), backed by Raft-replicated deterministic state. This crate
  never answers authority questions. The split is stated in `src/lib.rs`
  (module docs), `Cargo.toml` (package comment), and
  `sql/fiducia_memory.sql` (header comment), and is visible in the code: there
  is no lease, fencing-token, or node-RPC code anywhere in `src/`.
- **Messaging lives in NATS JetStream** elsewhere in the platform. This crate
  has no NATS dependency (`Cargo.toml` lists none); its only external surfaces
  are the HTTP API and Postgres. Other services integrate with it by calling
  the HTTP endpoints (or by using the library crate directly).
- **State lives in Postgres/Cockroach-class SQL.** The only hard requirement on
  the database is the `pgvector` extension (`create extension if not exists
  vector`, `migrations/0001_memory.sql` and `sql/fiducia_memory.sql`). The
  service is "bring-your-own-Postgres": `DATABASE_URL` may point at a
  customer's database or the Fiducia-hosted default (`src/main.rs`).

### The governing invariant

> **Vector similarity may surface relevant knowledge; it must never determine
> authoritative state.**

This is not just a slogan in the docs — it is enforced in three concrete
places, verified below in §5:

1. Authoritative truth exists only as a **resolved-`accepted` ledger claim**
   (`src/claims.rs`, `ClaimStatus::is_authoritative` in `src/domain.rs`).
2. Recall applies **authorization/validity as hard filters before any score is
   computed** (`authorized_and_valid` in `src/recall.rs`).
3. A memory **contradicted by an accepted claim is multiplied by 0.25** in
   ranking (`score()` in `src/recall.rs`).

## 2. Two layers, one crate

The crate is an explicit semantic merge of two implementations (see the
`// RECONCILE:` notes in `src/fusion.rs` and `src/durable/model.rs`):

- **The durable storage floor** (`src/durable/*`) — the Postgres system of
  record for immutable, provenance-bearing *facts*: the append-only
  `memory_claims` table with full-text and HNSW vector indexes. It is also the
  index-accelerated candidate-generation engine for recall (`sql/recall.sql`).
- **The epistemic layer** (crate-root modules `domain`, `claims`, `memory`,
  `recall`, `postgres`) — the reasoning on top: five memory types with
  provenance-derived trust, the contestable claim ledger
  (assert → support → contest → resolve), and the pure recall *fusion*
  (filter → rank → penalize → dedupe → pack).

`src/fusion.rs` is the seam: it projects durable `RecallHit`s into fusion
`Candidate`s so the durable store generates candidates and the epistemic fusion
ranks, filters, and explains them. `src/main.rs` mounts both endpoint sets over
**one shared `PgPool`**.

Two types are both named "Claim" on purpose:

| Type | Table | Meaning |
|---|---|---|
| `durable::model::Claim` | `memory_claims` | An immutable **fact row**: subject/predicate/object + embedding + temporal supersession. |
| `domain::Claim` | `claims` | A **contestable ledger assertion** moving through assert→support→contest→resolve. |

## 3. Data model

Three migrations under `migrations/` are applied in order on every boot via
`sqlx::migrate!()` (`src/durable/store.rs::migrate`, called unconditionally in
`src/main.rs` before serving). `sql/fiducia_memory.sql` is the canonical
epistemic schema, embedded by `src/db.rs` (`include_str!`) for
`PostgresMemory::migrate` parity; it is the same DDL as migration 0002 plus the
0003 RLS hardening folded in.

### 3.1 `memory_claims` — the durable fact ledger (`migrations/0001_memory.sql`)

Append-only. A fact is never updated in place; it is *closed* and replaced.

- `claim_id uuid PK`, `tenant_id uuid NOT NULL` — identity and tenancy.
- `subject`, `predicate` (non-empty-checked text) + `object jsonb` — the
  triple; `source jsonb` — free-form provenance (the fusion reads an optional
  `"derivation"` key out of it, `src/fusion.rs::provenance_from_source`).
- `confidence real` — checked into `[0,1]`.
- `content text` + `content_sha256 text` — the human/LLM-readable statement and
  its digest. A **partial unique index** `(tenant_id, content_sha256) WHERE
  valid_until IS NULL` dedupes *active* facts only: the same content may recur
  in history, but a tenant can hold at most one live copy.
- `embedding vector(1536) NOT NULL` — indexed with **HNSW cosine**
  (`memory_claims_embedding_idx`).
- `search_document tsvector GENERATED ALWAYS AS (to_tsvector('english',
  content)) STORED` — GIN-indexed; lexical recall needs no application-side
  text processing.
- `valid_from` / `valid_until` / `supersedes_claim_id` — temporal supersession.
  A live fact has `valid_until IS NULL`; superseding sets `valid_until = now()`
  on the old row and inserts the replacement pointing back via
  `supersedes_claim_id` (`src/durable/store.rs::supersede`, both statements in
  one transaction). A `CHECK (valid_until >= valid_from)` guards the window.

### 3.2 Epistemic tables (`migrations/0002_fiducia_memory.sql`)

**`memories`** — the durable unit of agent knowledge:

- `memory_type varchar(24)` checked to the five kinds: `working` (ephemeral
  workflow state), `episodic` (what happened), `semantic` (current beliefs),
  `procedural` (how to do things), `entity` (knowledge-graph node) — mirrored
  in `domain::MemoryType`.
- Provenance columns (`source_agent_id`, `source_execution_id`, `workflow_id`,
  `provenance jsonb`) — who produced it and how it was derived.
- Governance: `trust_score real` (checked to `[0,1]`), `importance`,
  `sensitivity`, `valid_from`/`valid_until`, `superseded_by uuid` (self-FK),
  `forgotten_at` (soft delete — all partial indexes are `WHERE forgotten_at IS
  NULL`).
- `version bigint` bumped by the `memory_bump_version()` trigger on every
  update (also updates `updated_at`), giving in-row optimistic versioning.

**`memory_embeddings`** — embeddings are a separate child table keyed
`(memory_id, model)`, so a memory can carry embeddings from several models and
re-embedding never rewrites the memory row or its version history. HNSW cosine
index over `vector(1536)`.

**`claims`** — the contestable ledger, mirrored durably. One **live claim per
identity** via the unique index `(tenant_id, namespace, subject, predicate)`;
history is versioned in-row (`claim_version`). `status` is checked to
`asserted | contested | accepted | rejected | superseded`; `supporters`,
`contests`, and `evidence` are jsonb arrays; `resolved_by` records the
accepting/rejecting principal.

**`memory_edges`** — a lightweight typed knowledge graph
(`(from_id, relation, to_id)` PK, per-tenant indexes both directions). The
schema and `domain::MemoryEdge` exist; no service handler writes edges yet.

**`memory_recall_log`** — an append-only audit of what was retrieved for whom
and why (`query`, `requested_by`, `returned_memory_ids`, `scoring`). The table
and its RLS policy exist; the service does not yet write to it.

### 3.3 Row-level security (`migrations/0003_rls_force.sql`)

Every table above carries a tenant-isolation policy keyed on the per-request
GUC `fiducia.tenant_id`:

```sql
using (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid)
```

0003 exists because 0002's policies were incomplete in two ways it documents in
its own header: they were never **FORCE**d (so the table owner — typically the
service's pool role — bypassed them), and the durable/audit tables had no RLS
at all. 0003 adds policies to `memory_claims`, `memory_embeddings` (scoped
through the parent `memories` row via `EXISTS`, since it has no `tenant_id`
column), and `memory_recall_log`, and issues `ALTER TABLE ... FORCE ROW LEVEL
SECURITY` on all six tables. The `current_setting(..., true)` (missing_ok) form
means a session that never set the GUC sees zero rows rather than erroring.

## 4. Module map of `src/`

| Module | Role |
|---|---|
| `lib.rs` | Crate docs (the invariant, the two-layer merge) + re-exports of the public library API. |
| `domain.rs` | Core types: `MemoryType`, `Provenance` (+ `base_trust()`), `Memory` (+ `is_live()`), `ClaimStatus` (+ `is_authoritative()`), `Claim`, `MemoryEdge`, `MemoryScope`. |
| `claims.rs` | `ClaimLedger`: pure, in-memory, deterministic contestable ledger — `assert`, `support`, `contest`, `resolve`, `supersede`, `forget`, `get`, `consensus`, `conflicts`. |
| `memory.rs` | `trust_from(provenance, supporters, contests)` scoring; the async `MemoryStore` trait; `InMemoryStore` (deterministic, for tests/dev). |
| `recall.rs` | The pure hybrid-recall fusion: `RecallQuery`, `Candidate`, `RecallWeights`, `recall()` / `recall_with_weights()` → `ContextPack` of `RetrievedMemory`s with score breakdowns and human-readable reasons. |
| `postgres.rs` | `PostgresMemory`: epistemic persistence (runtime `sqlx`, no compile-time `DATABASE_URL`) — `insert_memory`, `upsert_embedding`, `semantic_candidates`, `upsert_claim`, `accepted_claim_value`; `with_tenant` is the single RLS wiring point. |
| `db.rs` | Embeds `sql/fiducia_memory.sql` and applies it idempotently (`raw_sql`). |
| `fusion.rs` | The seam: `candidate_from_hit` / `candidates_from_hits` project durable `RecallHit`s into fusion `Candidate`s. |
| `durable/model.rs` | Durable row/request types: `Claim` (fact row), `AppendClaim`, `SupersedeClaim`, `RecallRequest`, `RecallHit`; input validation (1536-dim embeddings, `confidence`/`semantic_weight` in `[0,1]`, `limit` in `1..=100`). |
| `durable/store.rs` | `durable::store::MemoryStore` over the shared `PgPool`: `append`, atomic `supersede`, `recall` (runs `sql/recall.sql`), `migrate` (`sqlx::migrate!` over `migrations/`), `ping`. |
| `durable/api.rs` | Axum handlers for the durable endpoints, extracting the store from the unified state via `FromRef`. |
| `main.rs` | The unified service: config, pool, migrations, router, the in-process authoritative `ClaimLedger` behind `Arc<Mutex<_>>`, all epistemic handlers, error mapping. |

## 5. Request/data flow for the main operations

### 5.1 HTTP surface (all routes in `src/main.rs`)

| Route | Layer | Handler |
|---|---|---|
| `GET /healthz` | — | inline `"ok"` |
| `GET /readyz` | — | `readyz` → `PostgresMemory::ready()` (`select 1`) |
| `POST /v1/claims` | durable | `durable::api::append_claim` |
| `POST /v1/claims/{id}/supersede` | durable | `durable::api::supersede_claim` |
| `POST /v1/recall` | durable | `durable::api::recall` |
| `POST /v1/memories` | epistemic | `create_memory` |
| `POST /v1/recall/fused` | seam | `fused_recall` |
| `POST /v1/claims/assert\|support\|contest\|resolve` | epistemic | ledger handlers |
| `GET /v1/claims/consensus` | epistemic | `consensus` |

Service-level middleware: 2 MiB `RequestBodyLimitLayer`, a 10 s
`TimeoutLayer` (returns 408), and `TraceLayer` request tracing. The pool is
capped at 20 connections with a 5 s acquire timeout.

### 5.2 Append a durable fact — `POST /v1/claims`

`AppendClaim::validate()` (`durable/model.rs`) rejects empty
subject/predicate/content, out-of-range confidence, and any embedding that is
not exactly 1536 floats — *before* touching the database. `store.append()` then
opens a transaction, binds the tenant GUC (`bind_tenant`), and inserts the row;
the SHA-256 of the content is computed server-side (`sha256_hex`), so the
active-dedup index is always populated consistently. The generated `tsvector`
column needs no application work. Response: `201` with the stored row.

### 5.3 Supersede a fact — `POST /v1/claims/{id}/supersede`

One transaction (`durable/store.rs::supersede`): `UPDATE memory_claims SET
valid_until = COALESCE(valid_until, now()) WHERE claim_id = $1 AND tenant_id =
$2 AND valid_until IS NULL`, and if zero rows were affected the call fails with
`404 claim_not_found` (already-closed or foreign-tenant facts are
indistinguishable from missing ones — no cross-tenant information leak); then
the replacement is inserted with `supersedes_claim_id = old_id`. Both
statements commit atomically, so there is no window where both the old and new
fact are live — which is also what keeps the active-dedup unique index
satisfiable when the replacement has the same content.

### 5.4 Raw durable recall — `POST /v1/recall`

`sql/recall.sql`, executed inside a tenant-bound transaction
(`durable/store.rs::recall`):

```sql
WITH ranked AS (
  SELECT c.*, ts_rank_cd(c.search_document, websearch_to_tsquery('english', $2))::real AS lexical_score,
         (1 - (c.embedding <=> $3))::real AS semantic_score
  FROM memory_claims c
  WHERE c.tenant_id = $1 AND c.valid_from <= now() AND (c.valid_until IS NULL OR c.valid_until > now())
)
SELECT ..., (($4 * semantic_score) + ($5 * lexical_score))::real AS score
FROM ranked ORDER BY score DESC, confidence DESC, created_at DESC LIMIT $6
```

Note the order: the tenant + temporal predicates are in the `WHERE` clause —
inclusion — and similarity only appears in `ORDER BY` — ranking. The invariant
holds at the SQL layer too. `semantic_weight` (default 0.7) and its complement
weight the two signals; `limit` defaults to 20, max 100.

### 5.5 Fused recall — `POST /v1/recall/fused` (the merge in action)

`fused_recall` in `src/main.rs`:

1. **Candidate generation (Postgres).** The flattened durable `RecallRequest`
   is validated and run through `store.recall()` as above.
2. **Projection (the seam).** `candidates_from_hits` (`src/fusion.rs`) maps
   each fact row to a `Memory` of type `Semantic` in the `"default"` namespace
   (`memory_claims` has no namespace column), with `trust_score` = the fact's
   stored `confidence` (clamped), subject/predicate copied into `metadata`,
   provenance derived from `source.derivation` if present, and both raw scores
   clamped into `[0,1]` (`ts_rank_cd` is unbounded above).
   `contradicted_by_accepted_claim` is honestly set `false` — the durable
   store carries no ledger-contradiction signal; that down-rank is exercised
   by the epistemic path (documented in the module's `RECONCILE` note).
3. **Fusion (pure).** `recall()` (`src/recall.rs`) runs the pipeline below and
   the resulting `ContextPack` is returned as JSON.

Because the projection fixes `namespace = "default"` and `memory_type =
Semantic`, a fused-recall body that filters on another namespace or excludes
`semantic` from `memory_types` will (correctly, per the hard filters) return an
empty pack.

### 5.6 The pure recall fusion (`src/recall.rs`)

```text
authorize → tenant / namespace / type / validity / permission   HARD filters
→ fuse lexical + semantic + trust + freshness                    weighted rank
→ ×0.25 penalty if contradicted by an accepted claim
→ sort (deterministic tie-break by id) → dedupe by normalized content
→ token-bounded greedy pack (~4 chars/token, `estimate_tokens`)
```

- `authorized_and_valid` runs **before** `score` in `recall_with_weights` —
  candidates are `filter(...)`ed, then `map(score)`ed. A wrong-tenant, expired
  (`Memory::is_live`), superseded, wrong-namespace, wrong-type, or
  permission-gated candidate never receives a score at all. The permission
  model: a `metadata` entry `permission → <name>` requires the caller's
  `required_permissions` to contain `<name>`.
- Default weights (`RecallWeights::default`): lexical 0.25, semantic 0.35,
  trust 0.25, freshness 0.15, normalized by their sum. `prefer_recent` shifts
  0.2 of weight from semantic to freshness. Freshness halves roughly every 7
  days (`freshness()`).
- Every `RetrievedMemory` carries the full score breakdown plus a
  human-readable `reason` (`explain()`): either `"down-ranked: contradicted by
  an accepted claim"` or `"top signal: <strongest signal> (<value>)"` — this is
  what makes retrieval explainable and poisoning investigable.
- The pack is greedy: items that would exceed `max_tokens` are skipped (and
  `truncated` set), but smaller later items may still fit.

The unit tests in this module directly assert the invariant: a
maximal-similarity cross-tenant memory is never returned; expired/superseded
memories are excluded regardless of score; a contradicted memory sinks below a
weak-but-clean one.

### 5.7 The contestable claim ledger (assert → support → contest → resolve)

The lifecycle lives in `src/claims.rs` and is pure and deterministic (fully
unit-tested without a database):

- **assert** — creates the claim as `Asserted`, or on re-assertion of a live
  claim replaces value/confidence/author/evidence, **clears supporters and
  contests** (they applied to the old value), and bumps `claim_version`.
  Re-asserting a terminal claim is rejected (`ClaimError::Terminal` → HTTP
  409) — supersession is the only way past a resolved claim.
- **support** — appends the agent to `supporters` (idempotent per agent).
- **contest** — replaces any prior contest by the same agent, records the
  reason, and moves the claim to `Contested`.
- **resolve** — sets `Accepted` or `Rejected` and records `resolved_by`. This
  is the **only** path to authoritative truth; `consensus()` returns the value
  only when `status.is_authoritative()` (i.e. exactly `Accepted`). Note the
  ledger records the resolver but does not authenticate it — authorization of
  the resolving principal is the caller's job (stated in the `resolve` doc
  comment); the service currently exposes `POST /v1/claims/resolve` without an
  auth layer.
- **supersede / forget** — terminal supersession with a successor id; removal.

Each HTTP ledger handler in `src/main.rs` follows the same shape: lock the
in-process `Arc<Mutex<ClaimLedger>>`, mutate, **clone the claim out and drop
the guard before any `.await`** (the mutex is `std::sync::Mutex`; holding it
across an await would be a bug — the code comments this explicitly), then
mirror the full row to Postgres via `PostgresMemory::upsert_claim` (an upsert
on the ledger's uniqueness key that round-trips value, confidence, status,
evidence, supporters, contests, resolver, and version).

`GET /v1/claims/consensus` reads **only the in-process ledger**, which is the
authoritative source for the running instance (`src/main.rs` module docs);
`postgres.rs::accepted_claim_value` provides the equivalent durable read
(scoped to the full `(tenant, namespace, subject, predicate)` key) for library
consumers, but the handler does not consult it.

### 5.8 Store a memory — `POST /v1/memories`

`create_memory` derives `trust_score` from provenance when the caller does not
override it (`trust_from(&provenance, 0, 0)` → `Provenance::base_trust`:
resolved-claim / validated-procedure / human origins start at 0.9, claim /
procedure at 0.6, observation / import at 0.4, unknown at 0.5; support raises
it up to +0.25, contests lower it up to −0.6, all clamped to `[0,1]`), clamps
`importance`, and inserts via `PostgresMemory::insert_memory`. Embedding
generation is deliberately not wired (see §7); `upsert_embedding` and
`semantic_candidates` exist for when a provider is attached.

## 6. Concurrency, consistency, and tenancy

**Tenant isolation is defense in depth, and the code is explicit about the
mechanism.** Every tenant-scoped operation in both layers runs inside a
transaction that first executes
`select set_config('fiducia.tenant_id', $1, true)` — the bindable form of `SET
LOCAL`, transaction-scoped and released on commit/rollback — on the *same
pooled connection* the subsequent statements use
(`postgres.rs::with_tenant`/`bind_tenant`; `durable/store.rs::bind_tenant`).
The comments call out the failure mode this prevents: with a pool, a query on a
bare pool handle could land on a different connection than the GUC was set on,
and FORCEd RLS would then hide every row (reads) or reject every insert
(`WITH CHECK`). On top of RLS, every query *also* carries an explicit
`tenant_id = $n` predicate (and the ledger uses its full four-part key), so
queries remain correct even if RLS were disabled. The pure fusion adds a third
layer: `authorized_and_valid` re-checks the tenant on every candidate
(`fusion.rs` has a test proving a cross-tenant durable hit is dropped even if
SQL had leaked it).

**`with_tenant`'s ownership shape** (`postgres.rs`): the closure takes
ownership of the `Transaction<'static, Postgres>` and returns it with the
result so `with_tenant` can commit — the doc comment explains this avoids
higher-ranked lifetime bounds that would force borrowed query arguments to be
`'static`.

**The ledger's consistency model** is single-writer, in-process: one running
instance holds the authoritative `ClaimLedger` behind a mutex; Postgres is its
durable mirror, audit log, and external query surface (`src/main.rs` module
docs). Two consequences, both acknowledged in the code/README rather than
hidden: (a) ledger state is not yet rehydrated from Postgres on boot —
restart-durable mutation and multi-instance scale-out are the stated next step
(README "Scope & roadmap"; `upsert_claim` already persists everything needed);
(b) the ledger mutation and its Postgres mirror write are not one atomic unit —
the in-memory mutation happens first, then the upsert, so a mid-flight crash
can leave the mirror one step behind the (process-lifetime) ledger.

**Durable-floor atomicity** is straightforwardly transactional: append is a
single insert; supersede is close-old + insert-new in one transaction; the
partial unique dedup index and the `valid_until` check constraint are enforced
by Postgres itself.

**Error handling** is uniform and deliberately unrevealing. Epistemic handlers
map `ClaimError::NotFound` → 404, `ClaimError::Terminal` → 409, and any
`sqlx::Error` → 500 with the constant body `"storage backend error"` — the real
error goes to tracing, never to the client (`ApiError` in `src/main.rs`).
Durable handlers return 400 with a static validation detail, 404 for a missing
supersede target, and 503 `storage_unavailable` for storage failures
(`durable/api.rs`). `DATABASE_URL` is never logged; only the bind address is
emitted at startup.

## 7. Deliberately not wired (and where the boundary is)

Grounded in README "Scope & roadmap" and verified against the code:

- **Embedding generation.** The service stores and searches embeddings
  (`upsert_embedding`, `semantic_candidates`, the durable `embedding` column)
  but never calls an embedding model; callers supply 1536-d vectors in request
  bodies. A pluggable provider is the stated plan.
- **Ledger hydration on boot** (see §6).
- **Recall audit-log writes and edge writes.** `memory_recall_log` and
  `memory_edges` exist with RLS in place, but no handler populates them yet.
- **Contradiction signal on the durable path.** `contradicted_by_accepted_claim`
  is only meaningful for candidates built by an epistemic caller; the fused
  endpoint sets it `false` (§5.5).
- **Live-database integration tests.** The 15 offline tests cover the pure
  core (ledger lifecycle, fusion invariants, projection, literals/digests);
  the Postgres paths await a throwaway-database harness.

## 8. Operations

**Configuration** is environment-only (`src/main.rs`): `DATABASE_URL`
(required, secret — deliberately excluded from the CLI-flag schema),
`FIDUCIA_MEMORY_BIND` (default `127.0.0.1:8100`), `RUST_LOG` (default
`fiducia_memory=info,tower_http=info`), and `FIDUCIA_MEMORY_MIGRATE=true` (or
the `--migrate` argv flag) to apply migrations and exit. Non-secret flags map
to env vars through the vendored, pinned `flags-2-env` parser
(`.cli-flags.toml` schema; `scripts/with-flags2env.sh` resolves flags → env and
`exec`s the command; the schema is audited in CI,
`.github/workflows/cli-flags.yml`).

**Migrations** run on every boot: `sqlx::migrate!()` over `migrations/`
(0001 durable floor → 0002 epistemic schema + initial RLS → 0003 RLS force +
durable-table policies). All DDL is idempotent (`IF NOT EXISTS` / `drop policy
if exists`), so existing environments upgrade cleanly. `db.rs::apply_schema`
offers the same end state from the canonical `sql/fiducia_memory.sql` for
library users.

**Deployment** (`Dockerfile`): a two-stage build — pinned-by-digest
`rust:1.97.0-slim-bookworm` builds `--locked --release` and strips the binary;
the runtime is pinned-by-digest `gcr.io/distroless/cc-debian12:nonroot` running
as numeric UID 65532 with the single binary as entrypoint, exposing 8100.
`.dockerignore` keeps the context minimal.

**CI** (`.github/workflows/ci.yml`): Rust 1.95.0, the flags-2-env schema audit,
`cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`,
`cargo test --all-targets --locked`, and `cargo audit` (pinned action SHAs
throughout; Dependabot watches Cargo, GitHub Actions, and Docker inputs).
