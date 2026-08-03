use fiducia_memory::{Assertion, ClaimError, ClaimLedger, ClaimStatus};
use serde_json::{json, Value};
use uuid::Uuid;

const SUBJECT: &str = "customer:219";
const PREDICATE: &str = "refund_eligible";

fn assertion(
    tenant_id: Uuid,
    namespace: &str,
    value: Value,
    author: &str,
) -> Assertion {
    Assertion {
        tenant_id,
        namespace: namespace.to_owned(),
        subject: SUBJECT.to_owned(),
        predicate: PREDICATE.to_owned(),
        value,
        confidence: 0.9,
        author: author.to_owned(),
        evidence: vec![format!("ticket:{namespace}:88")],
    }
}

fn accept(
    ledger: &mut ClaimLedger,
    tenant_id: Uuid,
    namespace: &str,
    resolver: &str,
) {
    ledger
        .resolve(
            tenant_id,
            namespace,
            SUBJECT,
            PREDICATE,
            true,
            resolver,
        )
        .unwrap();
}

#[test]
fn identical_claim_identity_is_isolated_by_tenant() {
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let mut ledger = ClaimLedger::new();

    ledger
        .assert(assertion(tenant_a, "default", json!(true), "billing-a"))
        .unwrap();
    ledger
        .assert(assertion(tenant_b, "default", json!(false), "billing-b"))
        .unwrap();
    accept(&mut ledger, tenant_a, "default", "supervisor-a");
    accept(&mut ledger, tenant_b, "default", "supervisor-b");

    assert_eq!(
        ledger.consensus(tenant_a, "default", SUBJECT, PREDICATE),
        Some(&json!(true))
    );
    assert_eq!(
        ledger.consensus(tenant_b, "default", SUBJECT, PREDICATE),
        Some(&json!(false))
    );

    let claim_a = ledger
        .get(tenant_a, "default", SUBJECT, PREDICATE)
        .unwrap();
    let claim_b = ledger
        .get(tenant_b, "default", SUBJECT, PREDICATE)
        .unwrap();
    assert_ne!(claim_a.id, claim_b.id);
    assert_eq!(claim_a.resolved_by.as_deref(), Some("supervisor-a"));
    assert_eq!(claim_b.resolved_by.as_deref(), Some("supervisor-b"));
}

#[test]
fn identical_claim_identity_is_isolated_by_namespace() {
    let tenant = Uuid::new_v4();
    let mut ledger = ClaimLedger::new();

    ledger
        .assert(assertion(tenant, "billing", json!(true), "billing"))
        .unwrap();
    ledger
        .assert(assertion(tenant, "fraud", json!(false), "fraud"))
        .unwrap();
    accept(&mut ledger, tenant, "billing", "billing-supervisor");
    accept(&mut ledger, tenant, "fraud", "fraud-supervisor");

    assert_eq!(
        ledger.consensus(tenant, "billing", SUBJECT, PREDICATE),
        Some(&json!(true))
    );
    assert_eq!(
        ledger.consensus(tenant, "fraud", SUBJECT, PREDICATE),
        Some(&json!(false))
    );
}

#[test]
fn mutation_of_one_namespace_never_changes_another() {
    let tenant = Uuid::new_v4();
    let mut ledger = ClaimLedger::new();

    ledger
        .assert(assertion(tenant, "billing", json!(true), "billing"))
        .unwrap();
    ledger
        .assert(assertion(tenant, "fraud", json!(false), "fraud"))
        .unwrap();

    ledger
        .support(tenant, "billing", SUBJECT, PREDICATE, "audit")
        .unwrap();
    ledger
        .contest(
            tenant,
            "billing",
            SUBJECT,
            PREDICATE,
            "fraud",
            "chargeback on file",
        )
        .unwrap();
    ledger
        .assert(assertion(tenant, "billing", json!(false), "billing-v2"))
        .unwrap();

    let billing = ledger
        .get(tenant, "billing", SUBJECT, PREDICATE)
        .unwrap();
    let fraud = ledger
        .get(tenant, "fraud", SUBJECT, PREDICATE)
        .unwrap();

    assert_eq!(billing.claim_version, 2);
    assert_eq!(billing.value, json!(false));
    assert!(billing.supporters.is_empty());
    assert!(billing.contests.is_empty());

    assert_eq!(fraud.claim_version, 1);
    assert_eq!(fraud.value, json!(false));
    assert_eq!(fraud.status, ClaimStatus::Asserted);
    assert_eq!(fraud.author, "fraud");
}

#[test]
fn support_and_contest_retries_are_idempotent_per_agent() {
    let tenant = Uuid::new_v4();
    let mut ledger = ClaimLedger::new();
    ledger
        .assert(assertion(tenant, "default", json!(true), "billing"))
        .unwrap();

    ledger
        .support(tenant, "default", SUBJECT, PREDICATE, "audit")
        .unwrap();
    ledger
        .support(tenant, "default", SUBJECT, PREDICATE, "audit")
        .unwrap();
    ledger
        .contest(
            tenant,
            "default",
            SUBJECT,
            PREDICATE,
            "fraud",
            "first reason",
        )
        .unwrap();
    ledger
        .contest(
            tenant,
            "default",
            SUBJECT,
            PREDICATE,
            "fraud",
            "new evidence replaces the prior reason",
        )
        .unwrap();

    let claim = ledger
        .get(tenant, "default", SUBJECT, PREDICATE)
        .unwrap();
    assert_eq!(claim.supporters, vec!["audit"]);
    assert_eq!(claim.contests.len(), 1);
    assert_eq!(claim.contests[0].agent, "fraud");
    assert_eq!(
        claim.contests[0].reason,
        "new evidence replaces the prior reason"
    );
    assert_eq!(claim.status, ClaimStatus::Contested);
}

#[test]
fn terminal_claims_fail_closed_without_affecting_sibling_tenants() {
    let terminal_tenant = Uuid::new_v4();
    let live_tenant = Uuid::new_v4();
    let mut ledger = ClaimLedger::new();

    ledger
        .assert(assertion(
            terminal_tenant,
            "default",
            json!(true),
            "billing-terminal",
        ))
        .unwrap();
    ledger
        .assert(assertion(
            live_tenant,
            "default",
            json!(false),
            "billing-live",
        ))
        .unwrap();
    accept(
        &mut ledger,
        terminal_tenant,
        "default",
        "supervisor-terminal",
    );

    assert_eq!(
        ledger.support(
            terminal_tenant,
            "default",
            SUBJECT,
            PREDICATE,
            "late-agent",
        ),
        Err(ClaimError::Terminal(ClaimStatus::Accepted))
    );
    assert!(matches!(
        ledger.assert(assertion(
            terminal_tenant,
            "default",
            json!(false),
            "late-author",
        )),
        Err(ClaimError::Terminal(ClaimStatus::Accepted))
    ));

    let live = ledger
        .support(live_tenant, "default", SUBJECT, PREDICATE, "audit")
        .unwrap();
    assert_eq!(live.status, ClaimStatus::Asserted);
    assert_eq!(live.supporters, vec!["audit"]);
}

#[test]
fn forgetting_one_tenant_never_erases_another_tenants_claim() {
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    let mut ledger = ClaimLedger::new();

    let claim_a_id = ledger
        .assert(assertion(tenant_a, "default", json!(true), "billing-a"))
        .unwrap()
        .id;
    let claim_b_id = ledger
        .assert(assertion(tenant_b, "default", json!(false), "billing-b"))
        .unwrap()
        .id;

    let removed = ledger
        .forget(tenant_a, "default", SUBJECT, PREDICATE)
        .unwrap();
    assert_eq!(removed.id, claim_a_id);
    assert!(ledger
        .get(tenant_a, "default", SUBJECT, PREDICATE)
        .is_none());
    assert_eq!(
        ledger
            .get(tenant_b, "default", SUBJECT, PREDICATE)
            .unwrap()
            .id,
        claim_b_id
    );
}
