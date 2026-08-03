# Claim Governance and Isolation Contract

`fiducia-memory` stores durable, searchable organizational knowledge. It does
not decide which worker owns a task, whether a lease is current, or whether an
external effect is authorized. Those exact decisions remain in `fiducia-node`
and the owning control plane.

## Identity boundary

A live claim identity is the tuple:

```text
(tenant_id, namespace, subject, predicate)
```

The same subject and predicate may exist independently in different tenants or
namespaces. Support, contestation, re-assertion, resolution, supersession, and
forget operations must mutate only the exact identity supplied by the caller.

Database authorization and row-level security must enforce the same boundary;
the pure `ClaimLedger` provides the deterministic state-machine reference.

## Epistemic lifecycle

```text
Asserted
   |
   +-- support --> Asserted
   |
   +-- contest --> Contested
   |
   +-- authorized resolve(true)  --> Accepted
   +-- authorized resolve(false) --> Rejected
   +-- supersede                  --> Superseded
```

Only `Accepted` is authoritative organizational truth. The following are not
authority:

- a raw observation;
- an asserted claim;
- a supported claim;
- a contested claim;
- embedding similarity;
- retrieval rank;
- repeated agreement from correlated agents.

Authorization to resolve is checked by the service boundary. The ledger records
the resolver and makes the resulting state terminal.

## Retry and update semantics

- Supporting twice as the same agent is idempotent.
- Contesting twice as the same agent replaces that agent's prior reason rather
  than creating artificial vote weight.
- Re-asserting a non-terminal identity creates a new claim version and clears
  support/contest signals that applied to the old value.
- Mutating a terminal claim fails closed. A new value requires explicit
  supersession/new-lineage policy rather than silently reopening accepted or
  rejected truth.
- Forget removes one exact identity. Durable implementations must additionally
  preserve deletion lineage and audit evidence according to customer policy.

## Tenant and namespace isolation

No operation in tenant A may alter, erase, resolve, or expose tenant B's claim,
even when namespace, subject, predicate, and value are identical. Within a
tenant, namespaces are also independent authority domains.

Retrieval must apply tenant/namespace permission filters before lexical or
vector ranking. Post-filtering an unauthorized candidate is insufficient because
it can leak existence, score, timing, or graph-neighbor information.

## Recall boundary

Claims and memories may be included in a context pack only after:

1. tenant and namespace authorization;
2. validity and supersession filtering;
3. lexical/vector candidate generation;
4. provenance/trust/contradiction scoring;
5. diversity-aware reranking;
6. bounded context construction.

Recall explains why a record is relevant. It does not promote that record to
truth.

## Validation

Unit tests in `claims.rs` cover the core lifecycle. The black-box integration
suite in `tests/claim_ledger_isolation.rs` verifies the public crate boundary for:

- identical identities in separate tenants;
- identical identities in separate namespaces;
- mutation isolation across namespaces;
- support/contest retry idempotency;
- terminal immutability without sibling-tenant impact;
- tenant-specific forgetting.

Related Linear issues: DEN-866, DEN-865, and DEN-81.
