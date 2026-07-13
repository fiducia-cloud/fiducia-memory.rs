-- Canonical PostgreSQL schema for the Fiducia shared brain (fiducia-memory).
--
-- This service holds probabilistic, searchable organizational knowledge —
-- memories, versioned claims, provenance, and embeddings. It is deliberately
-- SEPARATE from fiducia-node: the node owns authoritative coordination (who owns
-- a task, whether a lease is valid); this owns cognition (what agents learned).
--
-- The invariant enforced throughout: **vector similarity may surface relevant
-- knowledge, but it never determines authoritative state.** Authoritative facts
-- live in `claims` and only move to `accepted` through an explicit resolution;
-- embeddings only rank retrieval candidates.

create extension if not exists vector;

create or replace function memory_bump_version() returns trigger as $$
begin
  new.version := old.version + 1;
  new.updated_at := now();
  return new;
end;
$$ language plpgsql;

-- Memory types: working (ephemeral workflow state), episodic (what happened),
-- semantic (current beliefs), procedural (how to do things), entity (relations).
create table if not exists memories (
  id uuid primary key default gen_random_uuid(),
  tenant_id uuid not null,
  namespace varchar(255) not null,
  memory_type varchar(24) not null,
  content text not null,
  metadata jsonb default '{}'::jsonb not null,
  -- provenance: who/what produced this and how it was derived.
  source_agent_id uuid,
  source_execution_id uuid,
  workflow_id uuid,
  provenance jsonb default '{}'::jsonb not null,
  -- governance / trust / freshness.
  trust_score real default 0.5 not null,
  importance real default 0.5 not null,
  sensitivity varchar(24) default 'normal' not null,
  valid_from timestamptz default now() not null,
  valid_until timestamptz,
  superseded_by uuid references memories (id) on delete set null,
  forgotten_at timestamptz,
  created_at timestamptz default now() not null,
  updated_at timestamptz default now() not null,
  version bigint default 1 not null,
  constraint memories_type_chk check (memory_type in ('working','episodic','semantic','procedural','entity')),
  constraint memories_metadata_object_chk check (jsonb_typeof(metadata) = 'object'),
  constraint memories_provenance_object_chk check (jsonb_typeof(provenance) = 'object'),
  constraint memories_trust_range_chk check (trust_score >= 0 and trust_score <= 1)
);
create index if not exists memories_tenant_ns_idx on memories (tenant_id, namespace) where forgotten_at is null;
create index if not exists memories_type_idx on memories (tenant_id, memory_type) where forgotten_at is null;
create index if not exists memories_valid_idx on memories (tenant_id, valid_until) where forgotten_at is null;
drop trigger if exists memories_bump on memories;
create trigger memories_bump before update on memories for each row execute function memory_bump_version();

-- Embeddings are stored separately (a memory may have several, per model), so a
-- re-embedding never rewrites the memory row and its version history.
create table if not exists memory_embeddings (
  memory_id uuid not null references memories (id) on delete cascade,
  model varchar(120) not null,
  model_version varchar(80),
  embedding vector(1536) not null,
  created_at timestamptz default now() not null,
  primary key (memory_id, model)
);
create index if not exists memory_embeddings_hnsw_idx on memory_embeddings using hnsw (embedding vector_cosine_ops);

-- The contestable claim ledger: versioned assertions with confidence, evidence,
-- and a status that only an authorized resolution can move to `accepted`.
create table if not exists claims (
  id uuid primary key default gen_random_uuid(),
  tenant_id uuid not null,
  namespace varchar(255) default 'default' not null,
  subject text not null,
  predicate text not null,
  value jsonb not null,
  confidence real,
  author_agent_id uuid,
  status varchar(24) default 'asserted' not null,
  evidence jsonb default '[]'::jsonb not null,
  supporters jsonb default '[]'::jsonb not null,
  contests jsonb default '[]'::jsonb not null,
  resolved_by varchar(320),
  superseded_by uuid references claims (id) on delete set null,
  valid_until timestamptz,
  claim_version bigint default 1 not null,
  created_at timestamptz default now() not null,
  updated_at timestamptz default now() not null,
  version bigint default 1 not null,
  constraint claims_status_chk check (status in ('asserted','contested','accepted','rejected','superseded')),
  constraint claims_evidence_array_chk check (jsonb_typeof(evidence) = 'array'),
  constraint claims_confidence_range_chk check (confidence is null or (confidence >= 0 and confidence <= 1))
);
-- One live claim per (tenant, subject, predicate); history is versioned in-row.
create unique index if not exists claims_subject_predicate_uq on claims (tenant_id, namespace, subject, predicate);
create index if not exists claims_subject_idx on claims (tenant_id, subject);
create index if not exists claims_status_idx on claims (tenant_id, status);
drop trigger if exists claims_bump on claims;
create trigger claims_bump before update on claims for each row execute function memory_bump_version();

-- A lightweight typed knowledge graph (Postgres edges, no graph DB needed yet).
create table if not exists memory_edges (
  from_id uuid not null,
  relation varchar(120) not null,
  to_id uuid not null,
  tenant_id uuid not null,
  weight real,
  provenance jsonb default '{}'::jsonb not null,
  created_at timestamptz default now() not null,
  primary key (from_id, relation, to_id),
  constraint memory_edges_provenance_object_chk check (jsonb_typeof(provenance) = 'object')
);
create index if not exists memory_edges_from_idx on memory_edges (tenant_id, from_id, relation);
create index if not exists memory_edges_to_idx on memory_edges (tenant_id, to_id, relation);

-- Append-only recall audit: what was retrieved for whom and why, so retrieval is
-- explainable and poisoning is investigable.
create table if not exists memory_recall_log (
  id uuid primary key default gen_random_uuid(),
  tenant_id uuid not null,
  namespace varchar(255),
  query text not null,
  requested_by varchar(320),
  returned_memory_ids jsonb default '[]'::jsonb not null,
  scoring jsonb default '{}'::jsonb not null,
  created_at timestamptz default now() not null
);
create index if not exists memory_recall_log_tenant_time_idx on memory_recall_log (tenant_id, created_at desc);

-- Tenant isolation: row-level security keyed on a per-request GUC
-- (`fiducia.tenant_id`), set per request/transaction with `SET LOCAL` (via
-- `set_config(..., true)`) by the service. The service layer ALSO enforces
-- tenancy in code, but RLS is the enforced backstop. RLS is FORCEd on every
-- table so the policy applies even when the app connects as the table owner
-- (see migrations/0003_rls_force.sql, the upgrade path for existing envs).
alter table memories enable row level security;
alter table claims enable row level security;
alter table memory_edges enable row level security;
alter table memory_embeddings enable row level security;
alter table memory_recall_log enable row level security;
drop policy if exists memories_tenant_isolation on memories;
create policy memories_tenant_isolation on memories
  using (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid)
  with check (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid);
drop policy if exists claims_tenant_isolation on claims;
create policy claims_tenant_isolation on claims
  using (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid)
  with check (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid);
drop policy if exists memory_edges_tenant_isolation on memory_edges;
create policy memory_edges_tenant_isolation on memory_edges
  using (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid)
  with check (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid);
-- memory_embeddings has no tenant_id of its own; scope it via its parent memory.
drop policy if exists memory_embeddings_tenant_isolation on memory_embeddings;
create policy memory_embeddings_tenant_isolation on memory_embeddings
  using (exists (select 1 from memories m
                 where m.id = memory_embeddings.memory_id
                   and m.tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid))
  with check (exists (select 1 from memories m
                      where m.id = memory_embeddings.memory_id
                        and m.tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid));
drop policy if exists memory_recall_log_tenant_isolation on memory_recall_log;
create policy memory_recall_log_tenant_isolation on memory_recall_log
  using (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid)
  with check (tenant_id = nullif(current_setting('fiducia.tenant_id', true), '')::uuid);
-- FORCE so the policy applies even to the table owner (the service's pool role).
alter table memories force row level security;
alter table claims force row level security;
alter table memory_edges force row level security;
alter table memory_embeddings force row level security;
alter table memory_recall_log force row level security;
