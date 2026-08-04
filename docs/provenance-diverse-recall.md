# Provenance-diverse recall

Fiducia memory separates **inclusion authority** from **ranking quality**.

The recall pipeline always applies tenant, namespace, memory-type, permission,
validity, expiry, and supersession checks before lexical, vector, trust,
freshness, contradiction, or diversity scores are considered. A diversity
score cannot include a memory that failed a hard filter, and it cannot make a
claim authoritative.

## Why diversity is needed

Without provenance-aware reranking, one agent can dominate a context pack by
writing many distinct but correlated memories with high similarity scores. An
exact-content deduplicator does not catch paraphrases, repeated observations,
or several memories produced by the same workflow.

Fiducia therefore selects memories greedily using the base hybrid score divided
by a repetition factor. Repetition is counted across memories already admitted
to the pack on four axes:

1. source agent;
2. source workflow;
3. derivation class;
4. memory type.

The default source-agent penalty is strongest. Workflow and derivation penalties
reduce correlated campaign/session output, while the smaller memory-type penalty
encourages a mix of episodic, semantic, procedural, working, and entity context.

The policy is soft rather than a hard quota. If a tenant has useful memories
from only one source, those memories can still fill the pack; their reranked
score and penalty remain visible in the response.

## Explainability

Every returned memory includes:

- lexical score;
- semantic score;
- trust score;
- freshness score;
- base combined score;
- diversity-adjusted reranked score;
- diversity penalty;
- contradiction flag;
- a human-readable reason.

Non-finite candidate scores are converted to zero. Negative/non-finite weights
and penalties cannot inject NaNs into ordering; an unusable all-zero weight set
falls back to the default hybrid weights.

## Tuning

`RecallPolicy` combines `RecallWeights` and `RecallDiversity`. Setting every
diversity penalty to zero restores pure base-score ordering while preserving all
hard authorization and validity filters.

Tuning should be benchmarked per deployment. Increasing source-agent penalty is
appropriate when many autonomous agents write observations. Lower penalties may
be appropriate for small, curated memory sets. Diversity never substitutes for
claim contestation, resolution, or provenance trust.

## Guarantee boundary

This protects the composition of the context pack, not the truth of its
contents. Accepted claims and explicit governance remain authoritative. Vector
similarity and provenance diversity only decide which already-authorized live
memories are most useful to show an agent.
