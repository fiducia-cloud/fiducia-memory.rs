WITH ranked AS (
 SELECT c.*, ts_rank_cd(c.search_document, websearch_to_tsquery('english', $2))::real AS lexical_score,
 (1 - (c.embedding <=> $3::vector))::real AS semantic_score FROM memory_claims c
 WHERE c.tenant_id = $1 AND c.valid_from <= now() AND (c.valid_until IS NULL OR c.valid_until > now())
)
SELECT claim_id, tenant_id, subject, predicate, object, source, confidence, content, content_sha256,
 valid_from, valid_until, supersedes_claim_id, created_at, lexical_score, semantic_score,
 (($4 * semantic_score) + ($5 * lexical_score))::real AS score
FROM ranked ORDER BY score DESC, confidence DESC, created_at DESC LIMIT $6
