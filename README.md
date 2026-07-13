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

## What's in the box

```
src/
  domain.rs      core types: MemoryType, Provenance, Memory, Claim, ClaimStatus, edges
  claims.rs      the contestable claim ledger (assert/support/contest/resolve/supersede)
  recall.rs      hybrid recall: fuse lexical+semantic+trust+freshness → ranked, token-bounded pack
  memory.rs      trust scoring + the MemoryStore trait + a deterministic in-memory store
  postgres.rs    Postgres + pgvector persistence (runtime sqlx, no compile-time DATABASE_URL)
  db.rs          applies the canonical schema
  main.rs        the fiducia-memory HTTP service (axum)
sql/
  fiducia_memory.sql   canonical schema: memories, embeddings, claims, edges, recall log, RLS
```

The **library core is pure and deterministic** — the claim ledger and the recall
fusion are plain functions of their inputs, so the full lifecycle and ranking
are unit-tested without a database (`cargo test`, 12 tests, all green offline).

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

Tenant isolation is enforced three ways: every query is tenant-scoped in code,
the service sets a per-request `fiducia.tenant_id` GUC, and **row-level security
policies** on `memories` / `claims` / `memory_edges` are the backstop.

## Running it

```bash
# 1. apply the schema (idempotent; needs pgvector)
DATABASE_URL=postgres://user:pass@host/db  cargo run -- --migrate

# 2. serve
DATABASE_URL=postgres://user:pass@host/db  cargo run
# listens on 127.0.0.1:8100 (override with FIDUCIA_MEMORY_BIND)
```

### HTTP API

| Method + path | Purpose |
|---|---|
| `GET  /healthz` | liveness |
| `GET  /readyz` | readiness (pings Postgres) |
| `POST /v1/memories` | store a memory (trust derived from provenance) |
| `POST /v1/claims/assert` | assert / re-assert a claim (a hypothesis, not truth) |
| `POST /v1/claims/support` | independently support a live claim |
| `POST /v1/claims/contest` | contest a live claim (moves it to `contested`) |
| `POST /v1/claims/resolve` | **authorized** accept/reject — the only path to authoritative truth |
| `GET  /v1/claims/consensus` | the authoritative value, or `null` if not yet accepted |

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

## Scope & roadmap

This is a focused, honest first cut. What is **real and tested** today: the
claim-ledger lifecycle and its invariants, the full recall fusion/rerank/token
pipeline with hard authorization filters, provenance-based trust scoring, the
Postgres/pgvector persistence layer (real SQL, runtime-bound), the schema with
RLS, and the HTTP service.

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

## Compatibility API preserved from `fiducia-memory`

The original non-suffixed repository is merged into this history. Its
tenant-scoped immutable claims API remains available as the
`fiducia-memory-compat` binary, backed by its SQLx migration and pgvector/HNSW
schema:

```text
POST /v1/claims
POST /v1/claims/{claim_id}/supersede
POST /v1/recall
GET  /healthz
GET  /readyz
```

Run it with `cargo run --bin fiducia-memory-compat`. It binds to
`127.0.0.1:8090` by default; `FIDUCIA_MEMORY_COMPAT_BIND` overrides the address.
The caller supplies 1536-dimensional embeddings, and production ingress must
authenticate each request and bind its `tenant_id` to the caller credential.
