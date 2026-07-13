# fiducia-memory — the shared brain

Durable, contestable, provenance-tracked **agent memory** with hybrid recall.

`fiducia-memory` is where a fleet of AI agents accumulates what it learns —
observations, playbooks, entity relationships, and versioned beliefs — so the
next agent starts from the fleet's knowledge instead of a blank context. It is
deliberately **separate from `fiducia-node`**:

| | `fiducia-node` | `fiducia-memory` |
|---|---|---|
| Owns | **coordination / authority** | **cognition / knowledge** |
| Answers | *who owns this task? is this lease valid?* | *what have we learned about X?* |
| Storage | Raft-replicated deterministic state | Postgres + pgvector |
| Truth model | linearizable, authoritative | probabilistic, ranked, contestable |

## The one invariant

> **Vector similarity may *surface* relevant knowledge. It must never *determine*
> authoritative state.**

Everything here is built around that line. Concretely:

- **Authoritative facts live in the claim ledger, not in embeddings.** An agent
  cannot write "the fact" into shared memory. It **asserts** a claim; others
  **support** or **contest** it; an authorized process **resolves** it. Only a
  resolved-`accepted` claim is authoritative — see [`src/claims.rs`](src/claims.rs).
  This is what stops one hallucinating agent from poisoning the brain for every
  future agent.
- **Recall applies authorization + validity as HARD filters *before* ranking.**
  A maximal-similarity memory from another tenant, an expired memory, a
  superseded memory, or one the caller lacks permission for is dropped from the
  candidate set *before* any score is computed — similarity can reorder what you
  are allowed to see, never widen it. See `authorized_and_valid` in
  [`src/recall.rs`](src/recall.rs).
- **A memory contradicted by an accepted claim is heavily down-ranked** (×0.25),
  so an authoritative fact outranks a similar-but-stale memory.

## Two layers, one crate

This crate is the **semantic merge of two independent implementations** into one
service. Nothing from either side was dropped.

- **The durable storage floor** (`src/durable/*`) is the real Postgres system of
  record: an **append-only `memory_claims` table** with a generated `tsvector`
  search document, an **HNSW cosine index** over 1536-d embeddings,
  **`content_sha256` active-dedup**, and **temporal supersession** via
  `valid_until` / `supersedes_claim_id`. It is also the candidate-generation
  engine for recall (`sql/recall.sql`).
- **The epistemic layer** (the crate-root modules) is the reasoning on top: the
  contestable **claim ledger** (assert → support → contest → resolve →
  consensus), the five **memory types** + provenance trust, and the explainable
  **hybrid recall fusion** (hard authorization/validity filters *before*
  ranking, then lexical + semantic + trust + freshness, contradiction
  down-rank, token-bounded pack).

The two are joined by `src/fusion.rs`: **durable SQL recall generates
candidates; the epistemic fusion ranks, filters, and explains them.**

## What's in the box

```
src/
  domain.rs      core types: MemoryType, Provenance, Memory, Claim (ledger), ClaimStatus, edges
  claims.rs      the contestable claim ledger (assert/support/contest/resolve/supersede)
  recall.rs      hybrid recall FUSION: lexical+semantic+trust+freshness → ranked, token-bounded pack
  memory.rs      trust scoring + the MemoryStore trait + a deterministic in-memory store
  postgres.rs    epistemic Postgres+pgvector persistence (runtime sqlx, no compile-time DATABASE_URL)
  db.rs          applies the canonical epistemic schema via raw_sql (idempotent)
  fusion.rs      the SEAM: projects durable RecallHits → recall::Candidate for the fusion
  durable/
    model.rs     durable request/row types (durable::model::Claim = a provenance FACT row)
    store.rs     PgPool store over memory_claims: append / atomic supersede / recall / migrate / ping
    api.rs       durable axum handlers (POST /v1/claims, /supersede, /v1/recall)
  main.rs        the unified fiducia-memory HTTP service (axum) over ONE shared PgPool
migrations/                     (applied on boot via sqlx::migrate!)
  0001_memory.sql               durable floor: memory_claims (tsvector, HNSW, sha256 dedup, supersession)
  0002_fiducia_memory.sql       epistemic schema: memories, embeddings, claims ledger, edges, recall log, RLS
  0003_rls_force.sql            tenant policies for every durable table + FORCE RLS owner protection
sql/
  fiducia_memory.sql   canonical epistemic schema (also embedded by db.rs for --migrate parity)
  recall.sql           durable hybrid-recall query (candidate generation)
```

> **`durable::model::Claim` vs `domain::Claim`.** These are deliberately kept
> distinct. A `durable::model::Claim` is a provenance-bearing **fact row** in
> `memory_claims` (subject/predicate/object + embedding + temporal supersession).
> A `domain::Claim` is a **contestable ledger assertion** moving through
> assert→support→contest→resolve. They model different things and both survive
> the merge under clear names (see the `// RECONCILE:` notes in `fusion.rs` and
> `durable/model.rs`).

The **library core is pure and deterministic** — the claim ledger, the recall
fusion, and the durable→fusion bridge are plain functions of their inputs, so
the full lifecycle, ranking, and candidate projection are unit-tested without a
database (`cargo test`, 15 tests, all green offline).

### Memory types

`working` (ephemeral workflow state) · `episodic` (what happened) · `semantic`
(current beliefs, usually backed by a claim) · `procedural` (how to do things) ·
`entity` (a lightweight knowledge-graph node). They differ in lifecycle and how
much they're trusted — a validated procedure or an accepted claim starts far
higher than a raw observation (`Provenance::base_trust`).

### Hybrid recall pipeline

```text
authorize → tenant / namespace / type / validity   HARD filters   (what you may see)
→ fuse lexical + semantic + trust + freshness       weighted        (ranking)
→ penalize memories contradicted by accepted claims
→ rerank → dedupe by content → token-bounded greedy pack
```

Each returned memory carries its full score breakdown **and a human-readable
reason** ("top signal: semantic similarity (0.91)"), so retrieval is
explainable and memory-poisoning is investigable. The caller supplies
pre-computed lexical/semantic scores (from Postgres full-text + pgvector), which
keeps `recall()` a pure, testable function.

## Bring-your-own Postgres

Like the rest of Fiducia, the datastore is the customer's choice. Point
`DATABASE_URL` at:

- **the customer's own Postgres** (cheaper for them, their data residency), or
- **the Fiducia-hosted default** (turnkey, priced higher).

The only requirement is the **pgvector** extension (`create extension vector`) —
the schema does this for you. Queries use runtime `sqlx` binding, so the crate
builds with no database reachable at compile time.

Tenant isolation is enforced twice. Every query carries an explicit tenant
predicate, and every tenant-scoped database operation runs in a transaction that
binds `fiducia.tenant_id` with `SET LOCAL` semantics on that same pooled
connection. The schema enables and **forces** row-level security on the durable
fact ledger, memories, embeddings, contestable claims, graph edges, and recall
audit log. A missing tenant binding therefore sees or writes no tenant rows,
including when the service connects as the table owner.

## Running it

```bash
# The service applies ALL migrations on boot (sqlx::migrate! over migrations/).
# To apply the schema and exit (idempotent; needs pgvector):
DATABASE_URL=postgres://user:pass@host/db  cargo run -- --migrate

# Serve (also migrates on boot):
DATABASE_URL=postgres://user:pass@host/db  cargo run
# listens on 127.0.0.1:8100 (override with FIDUCIA_MEMORY_BIND)
```

The same non-secret settings are available through the pinned, audited
`flags-2-env` launcher:

```bash
make -B -C vendor/flags-2-env all
DATABASE_URL=postgres://user:pass@host/db scripts/with-flags2env.sh --bind=127.0.0.1:8100 -- cargo run --locked
DATABASE_URL=postgres://user:pass@host/db scripts/with-flags2env.sh --migrate -- cargo run --locked
```

`DATABASE_URL` is deliberately environment-only because it may contain
credentials.

### HTTP API

One router, mounted over a single shared `PgPool`, exposes **both** endpoint
sets:

| Method + path | Layer | Purpose |
|---|---|---|
| `GET  /healthz` | — | liveness |
| `GET  /readyz` | — | readiness (pings Postgres) |
| `POST /v1/claims` | durable | append an immutable fact + its 1536-d embedding to `memory_claims` |
| `POST /v1/claims/{id}/supersede` | durable | atomically close an active fact and append its replacement |
| `POST /v1/recall` | durable | raw hybrid semantic + lexical recall (`sql/recall.sql`) |
| `POST /v1/memories` | epistemic | store a memory (trust derived from provenance) |
| `POST /v1/recall/fused` | seam | durable SQL recall → epistemic fusion (filtered, ranked, explained pack) |
| `POST /v1/claims/assert` | epistemic | assert / re-assert a ledger claim (a hypothesis, not truth) |
| `POST /v1/claims/support` | epistemic | independently support a live claim |
| `POST /v1/claims/contest` | epistemic | contest a live claim (moves it to `contested`) |
| `POST /v1/claims/resolve` | epistemic | **authorized** accept/reject — the only path to authoritative truth |
| `GET  /v1/claims/consensus` | epistemic | the authoritative value, or `null` if not yet accepted |

> Note: `POST /v1/claims` (durable append of a fact) and
> `POST /v1/claims/assert` (open a contestable ledger claim) are distinct
> operations on the two different `Claim` models — see the two-layers note above.

**`/v1/recall/fused`** is the merge in action: the durable store runs the
index-accelerated candidate generation (`sql/recall.sql`: tenant + temporal
filter, `ts_rank_cd` lexical, HNSW cosine), and those hits are projected into
`recall::Candidate`s that the epistemic fusion then hard-filters
(authorization/validity), ranks (lexical+semantic+trust+freshness), down-ranks
if contradicted, dedupes, and packs to a token budget — each returned memory
carrying its score breakdown and a human-readable reason.

```bash
# assert → not authoritative yet
curl -s localhost:8100/v1/claims/assert -H 'content-type: application/json' -d '{
  "tenant_id":"00000000-0000-0000-0000-000000000001",
  "subject":"customer:219","predicate":"refund_eligible","value":true,
  "confidence":0.9,"author":"billing-agent","evidence":["ticket:88"]}'

# consensus is null until an authorized principal resolves it
curl -s 'localhost:8100/v1/claims/consensus?tenant_id=00000000-0000-0000-0000-000000000001&subject=customer:219&predicate=refund_eligible'
# → {"authoritative_value": null, ...}

curl -s localhost:8100/v1/claims/resolve -H 'content-type: application/json' -d '{
  "tenant_id":"00000000-0000-0000-0000-000000000001",
  "subject":"customer:219","predicate":"refund_eligible",
  "accepted":true,"resolver":"supervisor:alex"}'
# now consensus → {"authoritative_value": true, ...}
```

## Environment & configuration

The service is configured entirely through environment variables:

| Variable | Required | Secret | Description |
|---|---|---|---|
| `DATABASE_URL` | **yes** | **yes** | Postgres connection URL (`postgres://user:pass@host/db`). **Carries database credentials** — treat as a secret: never log it, keep it out of shell history and CI logs, inject it from a secret store. Points at the customer's own Postgres or the Fiducia-hosted default; needs the `pgvector` extension. |
| `FIDUCIA_MEMORY_BIND` | no | no | Listen address for the HTTP service. Defaults to `127.0.0.1:8100`. |
| `RUST_LOG` / `fiducia_memory=…` | no | no | Standard `tracing-subscriber` env-filter for log levels (defaults to `fiducia_memory=info,tower_http=info`). |

### CLI flags → env (flags-2-env)

For non-secret settings, the pinned
[`ORESoftware/flags-2-env`](https://github.com/ORESoftware/flags-2-env) parser
(vendored at `vendor/flags-2-env`) maps CLI flags to these env vars from the
`.cli-flags.toml` schema. The schema is audited in CI (`.github/workflows/cli-flags.yml`).

```bash
git submodule update --init --recursive
make -B -C vendor/flags-2-env all
DATABASE_URL="$DATABASE_URL" scripts/with-flags2env.sh --bind=0.0.0.0:8100 -- cargo run
```

`scripts/with-flags2env.sh` runs the parser against `.cli-flags.toml`, exports the
resulting env map, then execs the command. `DATABASE_URL` is deliberately excluded
from the CLI schema; inject it through the environment or a secret store so it
cannot leak through shell history or process listings.

## Security & hardening

CI uses Rust `1.95.0`, locked Cargo resolution, warnings-as-errors Clippy, the
full test suite, and a required advisory scan. The container uses the same exact
Rust release for its build and an explicit numeric non-root distroless runtime;
Dependabot tracks Cargo, actions, and Docker inputs weekly.

- **`cargo audit` is clean** — no known advisories affect the dependency tree
  (218 crates scanned). No advisories are currently accepted/waived.
- **All SQL is parameterized.** Every query uses `sqlx` bind parameters
  (`query`/`query_as` with `.bind(...)`); no SQL is built by string
  concatenation or `format!`. Embeddings are passed as bound `vector` parameters.
- **Tenant isolation is defense in depth.** Queries retain explicit tenant
  predicates (the claims ledger also uses its full namespace key), while a
  transaction-local `fiducia.tenant_id` binding activates FORCEd RLS on every
  durable tenant table. Pool connections cannot retain tenant state across
  requests.
- **Request hardening:** a 2 MiB request-body limit, a 10 s per-request timeout,
  and a 5 s pool-acquire timeout are applied at the service layer; recall
  embeddings and page sizes are range-validated before they reach the database.
- **Secrets stay out of logs:** `DATABASE_URL` is never logged; only the
  (non-secret) bind address is emitted at startup.

## Scope & roadmap

This is a focused, honest first cut. What is **real and tested** today: the
claim-ledger lifecycle and its invariants, the full recall fusion/rerank/token
pipeline with hard authorization filters, provenance-based trust scoring, the
  Postgres/pgvector persistence layer (real SQL, runtime-bound), transaction-
  scoped FORCEd RLS, and the HTTP service.

Deliberately **not yet** wired, and where each goes:

- **Embedding generation.** `postgres.rs` stores and cosine-searches embeddings
  (`upsert_embedding` / `semantic_candidates`), but nothing here *calls* an
  embedding model — that belongs behind a pluggable provider so a customer can
  choose OpenAI, a local model, or their own. The recall math already consumes
  semantic scores; only the producer is external.
- **Ledger hydration on boot.** A single running instance is the authoritative
  ledger; Postgres is its durable mirror + audit log + external query surface.
  Loading claims back into the in-process ledger on startup (for restart-durable
  mutation and multi-instance scale-out) is the next step. `upsert_claim`
  already persists the whole lifecycle, so this is a read-path addition.
- **Live-service integration tests.** The pure core is thoroughly unit-tested;
  the Postgres layer needs a throwaway Postgres (e.g. testcontainers) to test
  end-to-end, which is out of scope for offline `cargo test`.

These are marked as follow-ups rather than hidden — nothing above is stubbed or
faked; the boundaries are simply drawn where an external dependency begins.
