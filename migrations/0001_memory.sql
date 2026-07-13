CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE IF NOT EXISTS memory_claims (
 claim_id uuid PRIMARY KEY DEFAULT gen_random_uuid(), tenant_id uuid NOT NULL,
 subject text NOT NULL CHECK (length(trim(subject)) > 0), predicate text NOT NULL CHECK (length(trim(predicate)) > 0),
 object jsonb NOT NULL, source jsonb NOT NULL, confidence real NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
 content text NOT NULL CHECK (length(trim(content)) > 0), content_sha256 text NOT NULL, embedding vector(1536) NOT NULL,
 valid_from timestamptz NOT NULL DEFAULT now(), valid_until timestamptz,
 supersedes_claim_id uuid REFERENCES memory_claims(claim_id),
 search_document tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
 created_at timestamptz NOT NULL DEFAULT now(), CHECK (valid_until IS NULL OR valid_until >= valid_from)
);
CREATE INDEX IF NOT EXISTS memory_claims_tenant_valid_idx ON memory_claims (tenant_id, valid_from DESC) WHERE valid_until IS NULL;
CREATE INDEX IF NOT EXISTS memory_claims_search_idx ON memory_claims USING gin (search_document);
CREATE INDEX IF NOT EXISTS memory_claims_embedding_idx ON memory_claims USING hnsw (embedding vector_cosine_ops);
CREATE UNIQUE INDEX IF NOT EXISTS memory_claims_active_dedup_idx ON memory_claims (tenant_id, content_sha256) WHERE valid_until IS NULL;
