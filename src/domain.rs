//! Core knowledge types for the shared brain.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

pub type MemoryId = Uuid;
pub type ClaimId = Uuid;
pub type TenantId = Uuid;
pub type AgentId = Uuid;

/// The five kinds of memory the brain distinguishes. They differ in lifecycle
/// and how they are trusted: working memory expires fast; a validated procedure
/// or an accepted claim is far more trustworthy than a raw observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    /// Ephemeral, workflow-scoped state (current goal, plan, open questions).
    Working,
    /// What happened: an agent did X, a deployment failed because Y.
    Episodic,
    /// What the system currently believes (usually backed by a claim).
    Semantic,
    /// How to perform a task: playbooks, tool sequences, known failure modes.
    Procedural,
    /// Relationships between entities (a lightweight knowledge graph node).
    Entity,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Working => "working",
            MemoryType::Episodic => "episodic",
            MemoryType::Semantic => "semantic",
            MemoryType::Procedural => "procedural",
            MemoryType::Entity => "entity",
        }
    }
}

/// Where a memory came from and how it was derived — carried on every memory so
/// retrieval can weigh trust and poisoning is investigable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    pub source_agent_id: Option<AgentId>,
    pub source_execution_id: Option<Uuid>,
    pub workflow_id: Option<Uuid>,
    /// How this knowledge was derived: `observation`, `claim`, `resolved_claim`,
    /// `procedure`, `validated_procedure`, `import`, `human`.
    pub derivation: Option<String>,
}

impl Provenance {
    /// A coarse base trust from derivation — a resolved claim or a validated
    /// procedure is far more trustworthy than a bare observation. Callers refine
    /// this with support/contest signals.
    pub fn base_trust(&self) -> f32 {
        match self.derivation.as_deref() {
            Some("resolved_claim") | Some("validated_procedure") | Some("human") => 0.9,
            Some("claim") | Some("procedure") => 0.6,
            Some("observation") | Some("import") => 0.4,
            _ => 0.5,
        }
    }
}

/// A durable unit of knowledge in the brain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Memory {
    pub id: MemoryId,
    pub tenant_id: TenantId,
    pub namespace: String,
    pub memory_type: MemoryType,
    pub content: String,
    pub metadata: BTreeMap<String, String>,
    pub provenance: Provenance,
    pub trust_score: f32,
    pub importance: f32,
    pub valid_from: DateTime<Utc>,
    pub valid_until: Option<DateTime<Utc>>,
    pub superseded_by: Option<MemoryId>,
}

impl Memory {
    /// A memory is live at `now` when not superseded and within its validity
    /// window. Only live memories are eligible for recall — a hard filter, never
    /// a soft score.
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.superseded_by.is_none()
            && self.valid_from <= now
            && self.valid_until.is_none_or(|until| until > now)
    }
}

/// The lifecycle of a claim. Only `resolve` (by an authorized principal) reaches
/// `Accepted` — semantic similarity can surface a claim but never accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    Asserted,
    Contested,
    Accepted,
    Rejected,
    Superseded,
}

impl ClaimStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ClaimStatus::Accepted | ClaimStatus::Rejected | ClaimStatus::Superseded
        )
    }

    /// Whether this status is authoritative organizational truth. **Only**
    /// `Accepted` is — an asserted or contested claim is a hypothesis.
    pub fn is_authoritative(self) -> bool {
        matches!(self, ClaimStatus::Accepted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimContest {
    pub agent: String,
    pub reason: String,
}

/// A versioned, contestable assertion: `subject` has `predicate` = `value`, with
/// confidence and evidence. Re-asserting a new value bumps the version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claim {
    pub id: ClaimId,
    pub tenant_id: TenantId,
    pub namespace: String,
    pub subject: String,
    pub predicate: String,
    pub value: Value,
    pub confidence: f32,
    pub author: String,
    pub status: ClaimStatus,
    pub evidence: Vec<String>,
    pub supporters: Vec<String>,
    pub contests: Vec<ClaimContest>,
    pub resolved_by: Option<String>,
    pub superseded_by: Option<ClaimId>,
    pub valid_until: Option<DateTime<Utc>>,
    pub claim_version: u64,
}

/// A typed edge in the memory graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEdge {
    pub from_id: Uuid,
    pub relation: String,
    pub to_id: Uuid,
    pub tenant_id: TenantId,
    pub weight: Option<f32>,
}

/// The scope a memory is promoted through as it earns trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Working,
    Workflow,
    Tenant,
    Organization,
}
