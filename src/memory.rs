//! Memory storage + governance: trust scoring, promotion, supersession, and
//! expiry. The store is a trait so the deterministic in-memory implementation
//! backs tests while `postgres` backs production.

use crate::domain::{Memory, MemoryId, Provenance, TenantId};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory not found")]
    NotFound,
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// Compute a memory's trust from its provenance plus support/contest signals.
/// A resolved-claim/human/validated-procedure origin starts high; independent
/// support raises it; contests lower it. Bounded to [0,1].
pub fn trust_from(provenance: &Provenance, supporters: usize, contests: usize) -> f32 {
    let base = provenance.base_trust();
    let support_boost = (supporters as f32 * 0.08).min(0.25);
    let contest_penalty = (contests as f32 * 0.15).min(0.6);
    (base + support_boost - contest_penalty).clamp(0.0, 1.0)
}

/// Durable memory storage. Tenancy is enforced by every method taking a
/// `tenant_id`; production additionally sets a per-request RLS GUC.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn insert(&self, memory: Memory) -> Result<(), MemoryError>;
    async fn get(&self, tenant: TenantId, id: MemoryId) -> Result<Option<Memory>, MemoryError>;
    /// Every live (non-superseded, unexpired) memory for a tenant/namespace at
    /// `now` — the candidate set a recall then ranks.
    async fn live(
        &self,
        tenant: TenantId,
        namespace: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Memory>, MemoryError>;
    /// Mark `old` superseded by `new` (governance: replace stale knowledge).
    async fn supersede(
        &self,
        tenant: TenantId,
        old: MemoryId,
        new: MemoryId,
    ) -> Result<(), MemoryError>;
    /// Forget a memory (deletion lineage kept durably in Postgres).
    async fn forget(&self, tenant: TenantId, id: MemoryId) -> Result<(), MemoryError>;
}

/// A deterministic in-process store for tests and single-node dev.
#[derive(Clone, Default)]
pub struct InMemoryStore {
    memories: Arc<Mutex<HashMap<MemoryId, Memory>>>,
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    async fn insert(&self, memory: Memory) -> Result<(), MemoryError> {
        self.memories.lock().await.insert(memory.id, memory);
        Ok(())
    }

    async fn get(&self, tenant: TenantId, id: MemoryId) -> Result<Option<Memory>, MemoryError> {
        Ok(self
            .memories
            .lock()
            .await
            .get(&id)
            .filter(|m| m.tenant_id == tenant)
            .cloned())
    }

    async fn live(
        &self,
        tenant: TenantId,
        namespace: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Memory>, MemoryError> {
        Ok(self
            .memories
            .lock()
            .await
            .values()
            .filter(|m| m.tenant_id == tenant)
            .filter(|m| namespace.is_none_or(|ns| m.namespace == ns))
            .filter(|m| m.is_live(now))
            .cloned()
            .collect())
    }

    async fn supersede(
        &self,
        tenant: TenantId,
        old: MemoryId,
        new: MemoryId,
    ) -> Result<(), MemoryError> {
        let mut memories = self.memories.lock().await;
        let memory = memories
            .get_mut(&old)
            .filter(|m| m.tenant_id == tenant)
            .ok_or(MemoryError::NotFound)?;
        memory.superseded_by = Some(new);
        Ok(())
    }

    async fn forget(&self, tenant: TenantId, id: MemoryId) -> Result<(), MemoryError> {
        let mut memories = self.memories.lock().await;
        if memories.get(&id).is_some_and(|m| m.tenant_id == tenant) {
            memories.remove(&id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MemoryType;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[test]
    fn trust_reflects_provenance_support_and_contests() {
        let resolved = Provenance {
            derivation: Some("resolved_claim".into()),
            ..Default::default()
        };
        let observation = Provenance {
            derivation: Some("observation".into()),
            ..Default::default()
        };
        assert!(trust_from(&resolved, 0, 0) > trust_from(&observation, 0, 0));
        // Contests lower trust; support raises it.
        assert!(trust_from(&observation, 3, 0) > trust_from(&observation, 0, 0));
        assert!(trust_from(&observation, 0, 3) < trust_from(&observation, 0, 0));
    }

    #[tokio::test]
    async fn store_isolates_tenants_and_hides_superseded() {
        let store = InMemoryStore::default();
        let t = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mem = Memory {
            id: Uuid::new_v4(),
            tenant_id: t,
            namespace: "default".into(),
            memory_type: MemoryType::Semantic,
            content: "x".into(),
            metadata: BTreeMap::new(),
            provenance: Provenance::default(),
            trust_score: 0.5,
            importance: 0.5,
            valid_from: Utc::now() - chrono::Duration::minutes(1),
            valid_until: None,
            superseded_by: None,
        };
        store.insert(mem.clone()).await.unwrap();
        // Another tenant cannot see it.
        assert!(store.get(other, mem.id).await.unwrap().is_none());
        assert_eq!(store.live(t, None, Utc::now()).await.unwrap().len(), 1);
        // Superseding removes it from the live set.
        store.supersede(t, mem.id, Uuid::new_v4()).await.unwrap();
        assert!(store.live(t, None, Utc::now()).await.unwrap().is_empty());
    }
}
