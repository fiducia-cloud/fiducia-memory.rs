//! Hybrid recall: fuse lexical + vector + trust + freshness signals into a
//! ranked, provenance-diverse, token-bounded context pack — and explain *why*
//! each memory was returned.
//!
//! The pipeline order encodes the brain's central invariant:
//!
//! ```text
//! authorize → tenant/namespace/type/validity HARD filters   (inclusion)
//! → fuse lexical + semantic + trust + freshness             (base ranking)
//! → penalize contradicted memories
//! → exact dedupe → provenance-diverse greedy rerank
//! → token-bounded context pack
//! ```
//!
//! Authorization and validity are **hard filters applied before ranking**, so a
//! high vector-similarity score can never surface an unauthorized, expired, or
//! superseded memory. Provenance diversity is a ranking adjustment, not an
//! authority decision: it reduces correlated context and poisoning leverage but
//! never upgrades an observation into accepted truth.
//!
//! `recall` is a pure function of its inputs (the caller supplies pre-computed
//! lexical/semantic scores from Postgres/pgvector), so ranking is deterministic
//! and unit-testable without a database.

use crate::domain::{AgentId, Memory, MemoryType, TenantId};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// A recall request. Empty `memory_types`/`required_permissions` mean "no filter
/// on that axis".
#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub tenant_id: TenantId,
    pub query: String,
    pub namespace: Option<String>,
    pub memory_types: Vec<MemoryType>,
    pub required_permissions: Vec<String>,
    pub max_tokens: usize,
    pub prefer_recent: bool,
    pub now: DateTime<Utc>,
}

/// A retrieval candidate: a memory plus the raw signals a caller computed
/// (lexical from text search, semantic from vector search), and any accepted
/// claim that contradicts it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub memory: Memory,
    /// Lexical/keyword relevance. Non-finite values become zero and finite
    /// values are clamped to [0,1].
    pub lexical_score: f32,
    /// Vector cosine similarity. Non-finite values become zero and finite values
    /// are clamped to [0,1].
    pub semantic_score: f32,
    /// True if an *accepted* claim contradicts this memory's content.
    pub contradicted_by_accepted_claim: bool,
}

/// Relative weights of the fusion signals. Invalid/negative values are treated
/// as zero; an all-zero/non-finite configuration falls back to the defaults.
#[derive(Debug, Clone, Copy)]
pub struct RecallWeights {
    pub lexical: f32,
    pub semantic: f32,
    pub trust: f32,
    pub freshness: f32,
}

impl Default for RecallWeights {
    fn default() -> Self {
        Self {
            lexical: 0.25,
            semantic: 0.35,
            trust: 0.25,
            freshness: 0.15,
        }
    }
}

/// Repetition penalties used by the greedy diversity reranker. A penalty of
/// zero disables that axis. Penalties are soft: if only one source exists, its
/// remaining memories can still fill the context pack.
#[derive(Debug, Clone, Copy)]
pub struct RecallDiversity {
    pub source_agent: f32,
    pub workflow: f32,
    pub derivation: f32,
    pub memory_type: f32,
}

impl Default for RecallDiversity {
    fn default() -> Self {
        Self {
            // Source diversity is strongest because repeated outputs from one
            // agent are the highest-correlation and poisoning-risk signal.
            source_agent: 0.50,
            workflow: 0.20,
            derivation: 0.10,
            memory_type: 0.05,
        }
    }
}

/// Full deterministic ranking policy. This is deliberately application state,
/// not a `fiducia-node` coordination primitive.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecallPolicy {
    pub weights: RecallWeights,
    pub diversity: RecallDiversity,
}

/// A returned memory with its full score breakdown and the reason it was chosen.
#[derive(Debug, Clone, Serialize)]
pub struct RetrievedMemory {
    pub memory_id: Uuid,
    pub content: String,
    pub memory_type: MemoryType,
    pub lexical_score: f32,
    pub semantic_score: f32,
    pub trust_score: f32,
    pub freshness_score: f32,
    /// Base fused score after contradiction handling and before diversity.
    pub combined_score: f32,
    /// Score used for the greedy selection step after repetition penalties.
    pub reranked_score: f32,
    /// Difference between base and reranked score for this selection position.
    pub diversity_penalty: f32,
    pub contradicted: bool,
    /// Human-readable explanation of the dominant inclusion and diversity
    /// signals. This is explanatory only and is not parsed as authority.
    pub reason: String,
    pub estimated_tokens: usize,
}

/// The bounded result of a recall.
#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub memories: Vec<RetrievedMemory>,
    pub total_tokens: usize,
    /// True if otherwise eligible candidates were dropped to fit `max_tokens`.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
struct ScoredCandidate {
    retrieved: RetrievedMemory,
    source_agent_id: Option<AgentId>,
    workflow_id: Option<Uuid>,
    derivation: Option<String>,
}

#[derive(Default)]
struct DiversityCounts {
    source_agents: BTreeMap<Option<AgentId>, usize>,
    workflows: BTreeMap<Option<Uuid>, usize>,
    derivations: BTreeMap<Option<String>, usize>,
    memory_types: BTreeMap<String, usize>,
}

/// Rough token estimate (~4 chars/token) for token-budgeting the pack.
pub fn estimate_tokens(content: &str) -> usize {
    content.chars().count().div_ceil(4)
}

/// Freshness in [0,1]: 1.0 at `valid_from`, halving roughly every 7 days.
fn freshness(valid_from: DateTime<Utc>, now: DateTime<Utc>) -> f32 {
    let age_days = (now - valid_from).num_seconds().max(0) as f32 / 86_400.0;
    1.0 / (1.0 + age_days / 7.0)
}

/// Run the hybrid recall pipeline with the default weights and diversity policy.
pub fn recall(query: &RecallQuery, candidates: Vec<Candidate>) -> ContextPack {
    recall_with_policy(query, candidates, RecallPolicy::default())
}

/// Preserve the existing weight-tuning API while applying the default diversity
/// policy. Use [`recall_with_policy`] to tune or disable diversity axes.
pub fn recall_with_weights(
    query: &RecallQuery,
    candidates: Vec<Candidate>,
    weights: RecallWeights,
) -> ContextPack {
    recall_with_policy(
        query,
        candidates,
        RecallPolicy {
            weights,
            ..RecallPolicy::default()
        },
    )
}

pub fn recall_with_policy(
    query: &RecallQuery,
    candidates: Vec<Candidate>,
    policy: RecallPolicy,
) -> ContextPack {
    let weights = normalized_weights(policy.weights, query.prefer_recent);
    let diversity = sanitized_diversity(policy.diversity);

    // 1. HARD FILTERS (authorization + validity), applied before any ranking.
    let mut scored: Vec<ScoredCandidate> = candidates
        .into_iter()
        .filter(|candidate| authorized_and_valid(query, candidate))
        .map(|candidate| score(query, &candidate, weights))
        .collect();

    // 2. Base rerank by fused score (deterministic tie-break by id).
    scored.sort_by(|left, right| {
        right
            .retrieved
            .combined_score
            .total_cmp(&left.retrieved.combined_score)
            .then(left.retrieved.memory_id.cmp(&right.retrieved.memory_id))
    });

    // 3. Exact content dedupe keeps the best base-ranked representative.
    let mut seen = BTreeSet::<String>::new();
    scored.retain(|candidate| {
        seen.insert(candidate.retrieved.content.trim().to_lowercase())
    });

    // 4. Greedy diversity-aware, token-bounded selection. Repetition penalties
    // are computed only from memories that actually entered the context pack.
    let mut counts = DiversityCounts::default();
    let mut memories = Vec::new();
    let mut total_tokens = 0usize;
    let mut truncated = false;

    while !scored.is_empty() {
        let (best_index, reranked_score, penalty) = scored
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let reranked = diversity_adjusted_score(candidate, &counts, diversity);
                let penalty = (candidate.retrieved.combined_score - reranked).max(0.0);
                (index, reranked, penalty)
            })
            .max_by(|left, right| {
                left.1
                    .total_cmp(&right.1)
                    .then_with(|| {
                        scored[left.0]
                            .retrieved
                            .combined_score
                            .total_cmp(&scored[right.0].retrieved.combined_score)
                    })
                    // `max_by` selects the larger ordering; reverse the UUID
                    // comparison so the lower UUID remains the deterministic tie.
                    .then_with(|| {
                        scored[right.0]
                            .retrieved
                            .memory_id
                            .cmp(&scored[left.0].retrieved.memory_id)
                    })
            })
            .expect("non-empty candidate pool");

        let mut candidate = scored.remove(best_index);
        candidate.retrieved.reranked_score = reranked_score;
        candidate.retrieved.diversity_penalty = penalty;
        if penalty > f32::EPSILON {
            candidate.retrieved.reason.push_str(&format!(
                "; provenance diversity penalty {:.3}",
                penalty
            ));
        }

        if total_tokens + candidate.retrieved.estimated_tokens > query.max_tokens {
            truncated = true;
            continue;
        }

        total_tokens += candidate.retrieved.estimated_tokens;
        counts.record(&candidate);
        memories.push(candidate.retrieved);
    }

    ContextPack {
        memories,
        total_tokens,
        truncated,
    }
}

impl DiversityCounts {
    fn record(&mut self, candidate: &ScoredCandidate) {
        *self
            .source_agents
            .entry(candidate.source_agent_id)
            .or_default() += 1;
        *self.workflows.entry(candidate.workflow_id).or_default() += 1;
        *self
            .derivations
            .entry(candidate.derivation.clone())
            .or_default() += 1;
        *self
            .memory_types
            .entry(candidate.retrieved.memory_type.as_str().to_string())
            .or_default() += 1;
    }
}

fn diversity_adjusted_score(
    candidate: &ScoredCandidate,
    counts: &DiversityCounts,
    policy: RecallDiversity,
) -> f32 {
    let source_repeats = counts
        .source_agents
        .get(&candidate.source_agent_id)
        .copied()
        .unwrap_or_default() as f32;
    let workflow_repeats = counts
        .workflows
        .get(&candidate.workflow_id)
        .copied()
        .unwrap_or_default() as f32;
    let derivation_repeats = counts
        .derivations
        .get(&candidate.derivation)
        .copied()
        .unwrap_or_default() as f32;
    let type_repeats = counts
        .memory_types
        .get(candidate.retrieved.memory_type.as_str())
        .copied()
        .unwrap_or_default() as f32;

    let repetition = policy.source_agent * source_repeats
        + policy.workflow * workflow_repeats
        + policy.derivation * derivation_repeats
        + policy.memory_type * type_repeats;
    candidate.retrieved.combined_score / (1.0 + repetition)
}

/// The hard inclusion gate: tenant match, live validity window, namespace, type,
/// and permission checks. A candidate that fails ANY of these is excluded no
/// matter how similar it is.
fn authorized_and_valid(query: &RecallQuery, candidate: &Candidate) -> bool {
    let memory = &candidate.memory;
    if memory.tenant_id != query.tenant_id {
        return false;
    }
    if !memory.is_live(query.now) {
        return false;
    }
    if let Some(namespace) = &query.namespace {
        if &memory.namespace != namespace {
            return false;
        }
    }
    if !query.memory_types.is_empty() && !query.memory_types.contains(&memory.memory_type) {
        return false;
    }
    // Permission model: a memory tagged `permission:<name>` in metadata requires
    // the caller to hold `<name>`. Every required tag on the memory must be held.
    for (key, value) in &memory.metadata {
        if key == "permission" && !query.required_permissions.iter().any(|item| item == value) {
            return false;
        }
    }
    true
}

fn score(query: &RecallQuery, candidate: &Candidate, weights: RecallWeights) -> ScoredCandidate {
    let memory = &candidate.memory;
    let lexical_score = unit_score(candidate.lexical_score);
    let semantic_score = unit_score(candidate.semantic_score);
    let freshness_score = freshness(memory.valid_from, query.now);
    let trust_score = unit_score(memory.trust_score);
    let total_weight = weights.lexical + weights.semantic + weights.trust + weights.freshness;
    let mut combined = (weights.lexical * lexical_score
        + weights.semantic * semantic_score
        + weights.trust * trust_score
        + weights.freshness * freshness_score)
        / total_weight;

    // A memory contradicted by an ACCEPTED claim is heavily penalized — an
    // authoritative fact outranks a similar-but-stale memory.
    if candidate.contradicted_by_accepted_claim {
        combined *= 0.25;
    }

    ScoredCandidate {
        retrieved: RetrievedMemory {
            memory_id: memory.id,
            content: memory.content.clone(),
            memory_type: memory.memory_type,
            lexical_score,
            semantic_score,
            trust_score,
            freshness_score,
            combined_score: combined,
            reranked_score: combined,
            diversity_penalty: 0.0,
            contradicted: candidate.contradicted_by_accepted_claim,
            reason: explain(
                candidate.contradicted_by_accepted_claim,
                lexical_score,
                semantic_score,
                trust_score,
                freshness_score,
            ),
            estimated_tokens: estimate_tokens(&memory.content),
        },
        source_agent_id: memory.provenance.source_agent_id,
        workflow_id: memory.provenance.workflow_id,
        derivation: memory.provenance.derivation.clone(),
    }
}

fn normalized_weights(weights: RecallWeights, prefer_recent: bool) -> RecallWeights {
    let mut normalized = RecallWeights {
        lexical: nonnegative_finite(weights.lexical),
        semantic: nonnegative_finite(weights.semantic),
        trust: nonnegative_finite(weights.trust),
        freshness: nonnegative_finite(weights.freshness),
    };
    if normalized.lexical + normalized.semantic + normalized.trust + normalized.freshness
        <= f32::EPSILON
    {
        normalized = RecallWeights::default();
    }
    if prefer_recent {
        normalized.freshness += 0.2;
        normalized.semantic = (normalized.semantic - 0.2).max(0.05);
    }
    normalized
}

fn sanitized_diversity(policy: RecallDiversity) -> RecallDiversity {
    RecallDiversity {
        source_agent: bounded_penalty(policy.source_agent),
        workflow: bounded_penalty(policy.workflow),
        derivation: bounded_penalty(policy.derivation),
        memory_type: bounded_penalty(policy.memory_type),
    }
}

fn nonnegative_finite(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn bounded_penalty(value: f32) -> f32 {
    nonnegative_finite(value).min(10.0)
}

fn unit_score(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn explain(
    contradicted: bool,
    lexical: f32,
    semantic: f32,
    trust: f32,
    freshness: f32,
) -> String {
    if contradicted {
        return "down-ranked: contradicted by an accepted claim".to_string();
    }
    let mut signals = vec![
        ("semantic similarity", semantic),
        ("keyword match", lexical),
        ("trust", trust),
        ("recency", freshness),
    ];
    signals.sort_by(|left, right| right.1.total_cmp(&left.1));
    format!("top signal: {} ({:.2})", signals[0].0, signals[0].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provenance;
    use std::collections::BTreeMap;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn memory(tenant: TenantId, content: &str, trust: f32, age_days: i64) -> Memory {
        Memory {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            namespace: "default".into(),
            memory_type: MemoryType::Episodic,
            content: content.into(),
            metadata: BTreeMap::new(),
            provenance: Provenance::default(),
            trust_score: trust,
            importance: 0.5,
            valid_from: now() - chrono::Duration::days(age_days),
            valid_until: None,
            superseded_by: None,
        }
    }

    fn candidate(memory: Memory, score: f32) -> Candidate {
        Candidate {
            memory,
            lexical_score: score,
            semantic_score: score,
            contradicted_by_accepted_claim: false,
        }
    }

    fn query(tenant: TenantId, max_tokens: usize) -> RecallQuery {
        RecallQuery {
            tenant_id: tenant,
            query: "how were payment incidents resolved".into(),
            namespace: None,
            memory_types: vec![],
            required_permissions: vec![],
            max_tokens,
            prefer_recent: false,
            now: now(),
        }
    }

    #[test]
    fn a_high_similarity_memory_from_another_tenant_is_never_returned() {
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let candidates = vec![Candidate {
            memory: memory(theirs, "cross-tenant secret", 0.99, 0),
            lexical_score: 1.0,
            semantic_score: 1.0,
            contradicted_by_accepted_claim: false,
        }];
        let pack = recall(&query(mine, 10_000), candidates);
        assert!(
            pack.memories.is_empty(),
            "similarity must not leak another tenant's memory"
        );
    }

    #[test]
    fn expired_and_superseded_memories_are_excluded_regardless_of_score() {
        let tenant = Uuid::new_v4();
        let mut expired = memory(tenant, "stale", 0.9, 1);
        expired.valid_until = Some(now() - chrono::Duration::hours(1));
        let mut superseded = memory(tenant, "old", 0.9, 1);
        superseded.superseded_by = Some(Uuid::new_v4());
        let candidates = vec![candidate(expired, 1.0), candidate(superseded, 1.0)];
        assert!(recall(&query(tenant, 10_000), candidates)
            .memories
            .is_empty());
    }

    #[test]
    fn permission_gated_memory_requires_the_permission() {
        let tenant = Uuid::new_v4();
        let mut gated = memory(tenant, "payment internals", 0.9, 0);
        gated
            .metadata
            .insert("permission".into(), "payments:read".into());

        let without = query(tenant, 10_000);
        assert!(recall(&without, vec![candidate(gated.clone(), 0.9)])
            .memories
            .is_empty());

        let mut with = query(tenant, 10_000);
        with.required_permissions = vec!["payments:read".into()];
        assert_eq!(recall(&with, vec![candidate(gated, 0.9)]).memories.len(), 1);
    }

    #[test]
    fn higher_trust_and_similarity_rank_first_and_contradicted_sinks() {
        let tenant = Uuid::new_v4();
        let strong = memory(tenant, "resolved: refund via original method", 0.95, 1);
        let weak = memory(tenant, "guess: maybe refund", 0.3, 1);
        let contradicted = memory(tenant, "old belief now false", 0.9, 1);
        let candidates = vec![
            candidate(weak, 0.5),
            candidate(strong.clone(), 0.9),
            Candidate {
                memory: contradicted,
                lexical_score: 0.95,
                semantic_score: 0.95,
                contradicted_by_accepted_claim: true,
            },
        ];
        let pack = recall(&query(tenant, 10_000), candidates);
        assert_eq!(pack.memories[0].memory_id, strong.id);
        assert!(pack.memories.last().unwrap().contradicted);
        assert!(pack.memories[0].reason.starts_with("top signal"));
    }

    #[test]
    fn recall_is_bounded_by_max_tokens() {
        let tenant = Uuid::new_v4();
        let content = |index: i64| {
            format!("incident {index} was resolved by rolling back the deploy")
        };
        let budget = estimate_tokens(&content(0));
        let candidates = (0..5)
            .map(|index| candidate(memory(tenant, &content(index), 0.9, index), 0.9))
            .collect();
        let pack = recall(&query(tenant, budget), candidates);
        assert_eq!(pack.memories.len(), 1);
        assert!(pack.truncated);
        assert!(pack.total_tokens <= budget);
    }

    #[test]
    fn independent_source_is_promoted_before_repeated_source_flood() {
        let tenant = Uuid::new_v4();
        let noisy_agent = Uuid::new_v4();
        let independent_agent = Uuid::new_v4();
        let workflow = Uuid::new_v4();

        let mut noisy_one = memory(tenant, "noisy source result one", 0.99, 0);
        noisy_one.provenance.source_agent_id = Some(noisy_agent);
        noisy_one.provenance.workflow_id = Some(workflow);
        noisy_one.provenance.derivation = Some("observation".into());

        let mut noisy_two = memory(tenant, "noisy source result two", 0.99, 0);
        noisy_two.provenance = noisy_one.provenance.clone();

        let mut independent = memory(tenant, "independent corroboration", 0.86, 0);
        independent.provenance.source_agent_id = Some(independent_agent);
        independent.provenance.workflow_id = Some(workflow);
        independent.provenance.derivation = Some("observation".into());

        let pack = recall(
            &query(tenant, 10_000),
            vec![
                candidate(noisy_one.clone(), 0.99),
                candidate(noisy_two.clone(), 0.98),
                candidate(independent.clone(), 0.86),
            ],
        );

        assert_eq!(pack.memories[0].memory_id, noisy_one.id);
        assert_eq!(
            pack.memories[1].memory_id, independent.id,
            "an independent source should outrank a second correlated result"
        );
        assert!(pack.memories[2].diversity_penalty > 0.0);
        assert!(pack.memories[2].reason.contains("provenance diversity penalty"));
    }

    #[test]
    fn diversity_is_soft_when_only_one_source_exists() {
        let tenant = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let mut candidates = Vec::new();
        for index in 0..3 {
            let mut item = memory(tenant, &format!("single source item {index}"), 0.9, 0);
            item.provenance.source_agent_id = Some(agent);
            item.provenance.derivation = Some("procedure".into());
            candidates.push(candidate(item, 0.9 - index as f32 * 0.01));
        }

        let pack = recall(&query(tenant, 10_000), candidates);
        assert_eq!(pack.memories.len(), 3);
        assert!(pack.memories[1].reranked_score < pack.memories[1].combined_score);
    }

    #[test]
    fn callers_can_disable_diversity_without_disabling_hard_filters() {
        let tenant = Uuid::new_v4();
        let agent = Uuid::new_v4();
        let mut first = memory(tenant, "first", 0.9, 0);
        first.provenance.source_agent_id = Some(agent);
        let mut second = memory(tenant, "second", 0.9, 0);
        second.provenance.source_agent_id = Some(agent);

        let pack = recall_with_policy(
            &query(tenant, 10_000),
            vec![candidate(first, 0.9), candidate(second, 0.8)],
            RecallPolicy {
                diversity: RecallDiversity {
                    source_agent: 0.0,
                    workflow: 0.0,
                    derivation: 0.0,
                    memory_type: 0.0,
                },
                ..RecallPolicy::default()
            },
        );
        assert!(pack
            .memories
            .iter()
            .all(|memory| memory.diversity_penalty == 0.0));
    }

    #[test]
    fn non_finite_scores_and_weights_cannot_poison_ordering() {
        let tenant = Uuid::new_v4();
        let valid = memory(tenant, "valid candidate", 0.8, 0);
        let mut poisoned = memory(tenant, "nan candidate", f32::NAN, 0);
        poisoned.provenance.source_agent_id = Some(Uuid::new_v4());

        let pack = recall_with_weights(
            &query(tenant, 10_000),
            vec![
                candidate(valid.clone(), 0.7),
                Candidate {
                    memory: poisoned,
                    lexical_score: f32::NAN,
                    semantic_score: f32::INFINITY,
                    contradicted_by_accepted_claim: false,
                },
            ],
            RecallWeights {
                lexical: f32::NAN,
                semantic: f32::INFINITY,
                trust: -10.0,
                freshness: 0.0,
            },
        );

        assert_eq!(pack.memories[0].memory_id, valid.id);
        assert!(pack.memories.iter().all(|memory| {
            memory.combined_score.is_finite() && memory.reranked_score.is_finite()
        }));
    }
}
