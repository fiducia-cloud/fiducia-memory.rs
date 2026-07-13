-- Wire real, defense-in-depth row-level security for tenant isolation.
--
-- Background: 0002 enabled RLS + tenant policies on `memories`, `claims`, and
-- `memory_edges`, but (a) those policies were never FORCED, so they do NOT apply
-- to the table owner — and the service connects as the pool role, which commonly
-- owns the tables, thereby bypassing RLS entirely; and (b) the durable/audit
-- tables `memory_claims` (0001), `memory_embeddings`, and `memory_recall_log`
-- (0002) had no RLS at all.
--
-- This migration:
--   1. Adds tenant-isolation policies to `memory_claims`, `memory_embeddings`,
--      and `memory_recall_log` (mirroring the 0002 policy shape, keyed on the
--      per-request `fiducia.tenant_id` GUC).
--   2. FORCEs RLS on EVERY protected table, so the policy is enforced even when
--      the query runs as the table owner. This is what makes RLS a real backstop
--      once the app sets `SET LOCAL fiducia.tenant_id` per request/transaction.
--
-- All statements are idempotent so existing environments upgrade cleanly.
-- The GUC name (`fiducia.tenant_id`) and the `current_setting(..., true)`
-- (missing_ok) form match the 0002 policies and the app's per-request binding,
-- so maintenance queries that never set the GUC simply see no rows rather than
-- erroring.

-- ---------------------------------------------------------------------------
-- 1. Durable fact ledger (0001): memory_claims. Has its own tenant_id column.
-- ---------------------------------------------------------------------------
alter table memory_claims enable row level security;
drop policy if exists memory_claims_tenant_isolation on memory_claims;
create policy memory_claims_tenant_isolation on memory_claims
  using (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid)
  with check (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid);

-- ---------------------------------------------------------------------------
-- 2. Embeddings (0002): memory_embeddings has NO tenant_id of its own; it is a
--    child of `memories` via memory_id. Scope it through the parent row's tenant
--    (an EXISTS against `memories`, whose own RLS is also forced below, so the
--    parent lookup is itself tenant-scoped).
-- ---------------------------------------------------------------------------
alter table memory_embeddings enable row level security;
drop policy if exists memory_embeddings_tenant_isolation on memory_embeddings;
create policy memory_embeddings_tenant_isolation on memory_embeddings
  using (
    exists (
      select 1 from memories m
      where m.id = memory_embeddings.memory_id
        and m.tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid
    )
  )
  with check (
    exists (
      select 1 from memories m
      where m.id = memory_embeddings.memory_id
        and m.tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid
    )
  );

-- ---------------------------------------------------------------------------
-- 3. Recall audit log (0002): memory_recall_log. Has its own tenant_id column.
-- ---------------------------------------------------------------------------
alter table memory_recall_log enable row level security;
drop policy if exists memory_recall_log_tenant_isolation on memory_recall_log;
create policy memory_recall_log_tenant_isolation on memory_recall_log
  using (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid)
  with check (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid);

-- ---------------------------------------------------------------------------
-- 4. FORCE row-level security on ALL protected tables. Without FORCE, the table
--    OWNER bypasses RLS; the service pool role usually owns these tables, so
--    FORCE is what actually makes the policies enforce at runtime.
--    (0002 already enabled RLS + policies on memories/claims/memory_edges; here
--    we only need to FORCE them.)
-- ---------------------------------------------------------------------------
alter table memories           force row level security;
alter table claims             force row level security;
alter table memory_edges       force row level security;
alter table memory_claims      force row level security;
alter table memory_embeddings  force row level security;
alter table memory_recall_log  force row level security;
