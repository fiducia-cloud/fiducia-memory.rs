//! Hybrid recall: fuse lexical + vector + trust + freshness signals into a
//! ranked, deduplicated, token-bounded context pack — and explain *why* each
//! memory was returned.
//!
//! The pipeline order encodes the brain's central invariant:
//!
//! ```text
//! authorize → tenant/namespace/type/validity HARD filters   (inclusion)
//! → fuse lexical + semantic + trust + freshness             (ranking)
//! → penalize contradicted memories
//! → rerank → dedupe/diversify → token-bounded pack
//! ```
//!
//! Authorization and validity are **hard filters applied before ranking**, so a
//! high vector-similarity score can never surface an unauthorized, expired, or
//! superseded memory. *Vectors suggest; they never control authoritative state.*
//!
//! `recall` is a pure function of its inputs (the caller supplies pre-computed
//! lexical/semantic scores from Postgres/pgvector), so ranking is fully
//! deterministic and unit-testable without a database.

use crate::domain::{Memory, MemoryType, TenantId};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeSet;

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
/// (lexical from text search, semantic from a vector search), and any claims
/// that contradict it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub memory: Memory,
    /// Lexical/keyword relevance in [0,1].
    pub lexical_score: f32,
    /// Vector cosine similarity in [0,1].
    pub semantic_score: f32,
    /// True if an *accepted* claim contradicts this memory's content.
    pub contradicted_by_accepted_claim: bool,
}

/// Relative weights of the fusion signals. Must be tuned per deployment.
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

/// A returned memory with its full score breakdown and the reason it was chosen.
#[derive(Debug, Clone, Serialize)]
pub struct RetrievedMemory {
    pub memory_id: uuid::Uuid,
    pub content: String,
    pub memory_type: MemoryType,
    pub lexical_score: f32,
    pub semantic_score: f32,
    pub trust_score: f32,
    pub freshness_score: f32,
    pub combined_score: f32,
    pub contradicted: bool,
    /// Human-readable explanation of the dominant reason for inclusion.
    pub reason: String,
    pub estimated_tokens: usize,
}

/// The bounded result of a recall.
#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub memories: Vec<RetrievedMemory>,
    pub total_tokens: usize,
    /// True if candidates were dropped to fit `max_tokens`.
    pub truncated: bool,
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

/// Run the hybrid recall pipeline with the default weights.
pub fn recall(query: &RecallQuery, candidates: Vec<Candidate>) -> ContextPack {
    recall_with_weights(query, candidates, RecallWeights::default())
}

pub fn recall_with_weights(
    query: &RecallQuery,
    candidates: Vec<Candidate>,
    weights: RecallWeights,
) -> ContextPack {
    // 1. HARD FILTERS (authorization + validity), applied before any ranking.
    let mut scored: Vec<RetrievedMemory> = candidates
        .into_iter()
        .filter(|c| authorized_and_valid(query, c))
        .map(|c| score(query, &c, weights))
        .collect();

    // 2. Rerank by combined score (deterministic tie-break by id).
    scored.sort_by(|a, b| {
        b.combined_score
            .partial_cmp(&a.combined_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.memory_id.cmp(&b.memory_id))
    });

    // 3. Dedupe by content (near-identical memories add no information).
    let mut seen: BTreeSet<String> = BTreeSet::new();
    scored.retain(|m| seen.insert(m.content.trim().to_lowercase()));

    // 4. Token-bounded greedy fill.
    let mut memories = Vec::new();
    let mut total_tokens = 0usize;
    let mut truncated = false;
    for memory in scored {
        if total_tokens + memory.estimated_tokens > query.max_tokens {
            truncated = true;
            continue;
        }
        total_tokens += memory.estimated_tokens;
        memories.push(memory);
    }

    ContextPack {
        memories,
        total_tokens,
        truncated,
    }
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
    for (k, v) in &memory.metadata {
        if k == "permission" && !query.required_permissions.iter().any(|p| p == v) {
            return false;
        }
    }
    true
}

fn score(query: &RecallQuery, candidate: &Candidate, weights: RecallWeights) -> RetrievedMemory {
    let memory = &candidate.memory;
    let freshness_score = freshness(memory.valid_from, query.now);
    let trust_score = memory.trust_score.clamp(0.0, 1.0);

    // prefer_recent tilts weight toward freshness without discarding the others.
    let weights = if query.prefer_recent {
        RecallWeights {
            freshness: weights.freshness + 0.2,
            semantic: (weights.semantic - 0.2).max(0.05),
            ..weights
        }
    } else {
        weights
    };
    let total_w = weights.lexical + weights.semantic + weights.trust + weights.freshness;
    let mut combined = (weights.lexical * candidate.lexical_score
        + weights.semantic * candidate.semantic_score
        + weights.trust * trust_score
        + weights.freshness * freshness_score)
        / total_w;

    // A memory contradicted by an ACCEPTED claim is heavily penalized — an
    // authoritative fact outranks a similar-but-stale memory.
    if candidate.contradicted_by_accepted_claim {
        combined *= 0.25;
    }

    RetrievedMemory {
        memory_id: memory.id,
        content: memory.content.clone(),
        memory_type: memory.memory_type,
        lexical_score: candidate.lexical_score,
        semantic_score: candidate.semantic_score,
        trust_score,
        freshness_score,
        combined_score: combined,
        contradicted: candidate.contradicted_by_accepted_claim,
        reason: explain(candidate, trust_score, freshness_score),
        estimated_tokens: estimate_tokens(&memory.content),
    }
}

fn explain(candidate: &Candidate, trust: f32, freshness: f32) -> String {
    if candidate.contradicted_by_accepted_claim {
        return "down-ranked: contradicted by an accepted claim".to_string();
    }
    let mut signals: Vec<(&str, f32)> = vec![
        ("semantic similarity", candidate.semantic_score),
        ("keyword match", candidate.lexical_score),
        ("trust", trust),
        ("recency", freshness),
    ];
    signals.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    format!("top signal: {} ({:.2})", signals[0].0, signals[0].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provenance;
    use std::collections::BTreeMap;
    use uuid::Uuid;

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
            semantic_score: 1.0, // maximal similarity
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
        let t = Uuid::new_v4();
        let mut expired = memory(t, "stale", 0.9, 1);
        expired.valid_until = Some(now() - chrono::Duration::hours(1));
        let mut superseded = memory(t, "old", 0.9, 1);
        superseded.superseded_by = Some(Uuid::new_v4());
        let candidates = vec![
            Candidate {
                memory: expired,
                lexical_score: 1.0,
                semantic_score: 1.0,
                contradicted_by_accepted_claim: false,
            },
            Candidate {
                memory: superseded,
                lexical_score: 1.0,
                semantic_score: 1.0,
                contradicted_by_accepted_claim: false,
            },
        ];
        assert!(recall(&query(t, 10_000), candidates).memories.is_empty());
    }

    #[test]
    fn permission_gated_memory_requires_the_permission() {
        let t = Uuid::new_v4();
        let mut gated = memory(t, "payment internals", 0.9, 0);
        gated
            .metadata
            .insert("permission".into(), "payments:read".into());
        let candidate = |m: Memory| Candidate {
            memory: m,
            lexical_score: 0.9,
            semantic_score: 0.9,
            contradicted_by_accepted_claim: false,
        };

        let mut without = query(t, 10_000);
        without.required_permissions = vec![];
        assert!(recall(&without, vec![candidate(gated.clone())])
            .memories
            .is_empty());

        let mut with = query(t, 10_000);
        with.required_permissions = vec!["payments:read".into()];
        assert_eq!(recall(&with, vec![candidate(gated)]).memories.len(), 1);
    }

    #[test]
    fn higher_trust_and_similarity_rank_first_and_contradicted_sinks() {
        let t = Uuid::new_v4();
        let strong = memory(t, "resolved: refund via original method", 0.95, 1);
        let weak = memory(t, "guess: maybe refund", 0.3, 1);
        let contradicted = memory(t, "old belief now false", 0.9, 1);
        let candidates = vec![
            Candidate {
                memory: weak,
                lexical_score: 0.5,
                semantic_score: 0.5,
                contradicted_by_accepted_claim: false,
            },
            Candidate {
                memory: strong.clone(),
                lexical_score: 0.9,
                semantic_score: 0.9,
                contradicted_by_accepted_claim: false,
            },
            Candidate {
                memory: contradicted,
                lexical_score: 0.95,
                semantic_score: 0.95,
                contradicted_by_accepted_claim: true,
            },
        ];
        let pack = recall(&query(t, 10_000), candidates);
        assert_eq!(
            pack.memories[0].memory_id, strong.id,
            "strongest ranks first"
        );
        // The contradicted memory is penalized below the weak one despite high similarity.
        assert!(pack.memories.last().unwrap().contradicted);
        assert!(pack.memories[0].reason.starts_with("top signal"));
    }

    #[test]
    fn recall_is_bounded_by_max_tokens() {
        let t = Uuid::new_v4();
        // Each memory is a 50-char string ≈ 13 tokens (⌈53/4⌉); a 13-token budget
        // fits exactly one and truncates the rest.
        let content = |i: i64| format!("incident {i} was resolved by rolling back the deploy");
        let budget = estimate_tokens(&content(0));
        let candidates: Vec<Candidate> = (0..5)
            .map(|i| Candidate {
                memory: memory(t, &content(i), 0.9, i),
                lexical_score: 0.9,
                semantic_score: 0.9,
                contradicted_by_accepted_claim: false,
            })
            .collect();
        let pack = recall(&query(t, budget), candidates);
        assert_eq!(pack.memories.len(), 1);
        assert!(pack.truncated);
        assert!(pack.total_tokens <= budget);
    }
}
