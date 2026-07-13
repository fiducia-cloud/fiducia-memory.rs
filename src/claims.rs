//! The contestable claim ledger.
//!
//! Agents do not write "facts" directly into shared memory. They **assert**
//! claims; others **support** or **contest**; an authorized process **resolves**.
//! Only a resolved-accepted claim is authoritative. This is what stops one
//! hallucinating agent from poisoning the brain for every future agent: a bare
//! assertion is a hypothesis, never truth.
//!
//! This module is pure, in-memory, and deterministic (ids are supplied by the
//! caller / a generator, never asserted on), so the full lifecycle is
//! unit-testable without a database. `postgres` mirrors it durably.

use crate::domain::{Claim, ClaimContest, ClaimId, ClaimStatus, TenantId};
use serde_json::Value;
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClaimError {
    #[error("claim not found")]
    NotFound,
    #[error("claim is terminal ({0:?}) and cannot be mutated")]
    Terminal(ClaimStatus),
}

/// A subject/predicate identity within a tenant + namespace — the ledger holds
/// one live claim per identity, with history versioned in-row.
type Key = (TenantId, String, String, String);

fn key(tenant: TenantId, namespace: &str, subject: &str, predicate: &str) -> Key {
    (
        tenant,
        namespace.to_owned(),
        subject.to_owned(),
        predicate.to_owned(),
    )
}

#[derive(Default)]
pub struct ClaimLedger {
    claims: HashMap<Key, Claim>,
}

/// A new claim assertion.
pub struct Assertion {
    pub tenant_id: TenantId,
    pub namespace: String,
    pub subject: String,
    pub predicate: String,
    pub value: Value,
    pub confidence: f32,
    pub author: String,
    pub evidence: Vec<String>,
}

impl ClaimLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Assert (or re-assert) a claim. A new value on an existing, non-terminal
    /// claim bumps its version and resets it to `Asserted`, clearing prior
    /// support/contests (they applied to the old value). A re-assertion of a
    /// terminal claim is rejected — supersede it instead.
    pub fn assert(&mut self, assertion: Assertion) -> Result<&Claim, ClaimError> {
        let k = key(
            assertion.tenant_id,
            &assertion.namespace,
            &assertion.subject,
            &assertion.predicate,
        );
        // Update-in-place path for an existing claim. The `contains_key` guard
        // scopes the mutable `get_mut` borrow entirely inside a block that always
        // returns, so it never overlaps the `entry` insert on the fall-through
        // path (the current borrow checker rejects the overlap otherwise).
        if self.claims.contains_key(&k) {
            let existing = self.claims.get_mut(&k).expect("checked by contains_key");
            if existing.status.is_terminal() {
                return Err(ClaimError::Terminal(existing.status));
            }
            existing.value = assertion.value;
            existing.confidence = assertion.confidence;
            existing.author = assertion.author;
            existing.evidence = assertion.evidence;
            existing.status = ClaimStatus::Asserted;
            existing.supporters.clear();
            existing.contests.clear();
            existing.claim_version += 1;
            return Ok(existing);
        }

        // First assertion for this identity.
        let claim = Claim {
            id: Uuid::new_v4(),
            tenant_id: assertion.tenant_id,
            namespace: assertion.namespace,
            subject: assertion.subject,
            predicate: assertion.predicate,
            value: assertion.value,
            confidence: assertion.confidence,
            author: assertion.author,
            status: ClaimStatus::Asserted,
            evidence: assertion.evidence,
            supporters: Vec::new(),
            contests: Vec::new(),
            resolved_by: None,
            superseded_by: None,
            valid_until: None,
            claim_version: 1,
        };
        Ok(self.claims.entry(k).or_insert(claim))
    }

    fn live_mut(
        &mut self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
    ) -> Result<&mut Claim, ClaimError> {
        let claim = self
            .claims
            .get_mut(&key(tenant, namespace, subject, predicate))
            .ok_or(ClaimError::NotFound)?;
        if claim.status.is_terminal() {
            return Err(ClaimError::Terminal(claim.status));
        }
        Ok(claim)
    }

    pub fn support(
        &mut self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
        agent: &str,
    ) -> Result<&Claim, ClaimError> {
        let claim = self.live_mut(tenant, namespace, subject, predicate)?;
        if !claim.supporters.iter().any(|s| s == agent) {
            claim.supporters.push(agent.to_owned());
        }
        Ok(claim)
    }

    pub fn contest(
        &mut self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
        agent: &str,
        reason: &str,
    ) -> Result<&Claim, ClaimError> {
        let claim = self.live_mut(tenant, namespace, subject, predicate)?;
        claim.contests.retain(|c| c.agent != agent);
        claim.contests.push(ClaimContest {
            agent: agent.to_owned(),
            reason: reason.to_owned(),
        });
        claim.status = ClaimStatus::Contested;
        Ok(claim)
    }

    /// Authoritatively accept or reject a claim. `resolver` is the authorized
    /// principal (authz is enforced by the caller; the ledger records it). This
    /// is the ONLY path to authoritative truth.
    pub fn resolve(
        &mut self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
        accepted: bool,
        resolver: &str,
    ) -> Result<&Claim, ClaimError> {
        let claim = self.live_mut(tenant, namespace, subject, predicate)?;
        claim.status = if accepted {
            ClaimStatus::Accepted
        } else {
            ClaimStatus::Rejected
        };
        claim.resolved_by = Some(resolver.to_owned());
        Ok(claim)
    }

    /// Supersede a claim with a newer one (terminal). Its successor takes over.
    pub fn supersede(
        &mut self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
        successor: ClaimId,
    ) -> Result<&Claim, ClaimError> {
        let claim = self.live_mut(tenant, namespace, subject, predicate)?;
        claim.status = ClaimStatus::Superseded;
        claim.superseded_by = Some(successor);
        Ok(claim)
    }

    /// Forget a claim entirely (deletion lineage handled durably in Postgres).
    pub fn forget(
        &mut self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
    ) -> Option<Claim> {
        self.claims
            .remove(&key(tenant, namespace, subject, predicate))
    }

    pub fn get(
        &self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
    ) -> Option<&Claim> {
        self.claims.get(&key(tenant, namespace, subject, predicate))
    }

    /// The authoritative value for `subject`/`predicate`, if a claim has been
    /// accepted. Returns `None` for merely asserted/contested claims — semantic
    /// similarity or agent enthusiasm never makes a claim authoritative.
    pub fn consensus(
        &self,
        tenant: TenantId,
        namespace: &str,
        subject: &str,
        predicate: &str,
    ) -> Option<&Value> {
        self.get(tenant, namespace, subject, predicate)
            .filter(|c| c.status.is_authoritative())
            .map(|c| &c.value)
    }

    /// Every currently-contested claim about `subject` (for surfacing disputes).
    pub fn conflicts(&self, tenant: TenantId, subject: &str) -> Vec<&Claim> {
        self.claims
            .values()
            .filter(|c| {
                c.tenant_id == tenant && c.subject == subject && c.status == ClaimStatus::Contested
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tenant() -> TenantId {
        Uuid::new_v4()
    }

    fn assertion(t: TenantId, value: Value, author: &str) -> Assertion {
        Assertion {
            tenant_id: t,
            namespace: "default".into(),
            subject: "customer:219".into(),
            predicate: "refund_eligible".into(),
            value,
            confidence: 0.9,
            author: author.into(),
            evidence: vec!["ticket:88".into()],
        }
    }

    #[test]
    fn assertion_is_not_authoritative_until_resolved() {
        let t = tenant();
        let mut ledger = ClaimLedger::new();
        ledger.assert(assertion(t, json!(true), "billing")).unwrap();
        // A bare assertion — even a supported one — is not consensus.
        ledger
            .support(t, "default", "customer:219", "refund_eligible", "audit")
            .unwrap();
        assert_eq!(
            ledger.consensus(t, "default", "customer:219", "refund_eligible"),
            None
        );

        // Only an authorized resolve makes it authoritative.
        ledger
            .resolve(
                t,
                "default",
                "customer:219",
                "refund_eligible",
                true,
                "supervisor",
            )
            .unwrap();
        assert_eq!(
            ledger.consensus(t, "default", "customer:219", "refund_eligible"),
            Some(&json!(true))
        );
    }

    #[test]
    fn contest_moves_to_contested_and_shows_in_conflicts() {
        let t = tenant();
        let mut ledger = ClaimLedger::new();
        ledger.assert(assertion(t, json!(true), "billing")).unwrap();
        ledger
            .contest(
                t,
                "default",
                "customer:219",
                "refund_eligible",
                "fraud",
                "chargeback on file",
            )
            .unwrap();
        let claim = ledger
            .get(t, "default", "customer:219", "refund_eligible")
            .unwrap();
        assert_eq!(claim.status, ClaimStatus::Contested);
        assert_eq!(ledger.conflicts(t, "customer:219").len(), 1);
    }

    #[test]
    fn reassertion_bumps_version_and_clears_prior_signals() {
        let t = tenant();
        let mut ledger = ClaimLedger::new();
        ledger.assert(assertion(t, json!(true), "billing")).unwrap();
        ledger
            .support(t, "default", "customer:219", "refund_eligible", "audit")
            .unwrap();
        let claim = ledger.assert(assertion(t, json!(false), "fraud")).unwrap();
        assert_eq!(claim.claim_version, 2);
        assert_eq!(claim.value, json!(false));
        assert!(
            claim.supporters.is_empty(),
            "prior support applied to the old value"
        );
        assert_eq!(claim.status, ClaimStatus::Asserted);
    }

    #[test]
    fn a_resolved_claim_is_terminal() {
        let t = tenant();
        let mut ledger = ClaimLedger::new();
        ledger.assert(assertion(t, json!(true), "billing")).unwrap();
        ledger
            .resolve(
                t,
                "default",
                "customer:219",
                "refund_eligible",
                true,
                "supervisor",
            )
            .unwrap();
        // Cannot support/contest/re-assert a resolved claim.
        assert_eq!(
            ledger.support(t, "default", "customer:219", "refund_eligible", "late"),
            Err(ClaimError::Terminal(ClaimStatus::Accepted))
        );
        assert!(matches!(
            ledger.assert(assertion(t, json!(false), "x")),
            Err(ClaimError::Terminal(ClaimStatus::Accepted))
        ));
    }
}
