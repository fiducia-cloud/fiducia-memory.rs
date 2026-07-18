//! Safe pgvector text encoding for SeaORM-bound raw SQL statements.

/// Validate finite coordinates and encode PostgreSQL's vector text format.
pub(crate) fn pgvector_literal(embedding: &[f32]) -> Result<String, &'static str> {
    if embedding.iter().any(|value| !value.is_finite()) {
        return Err("embedding values must be finite");
    }
    let mut out = String::with_capacity(embedding.len() * 8 + 2);
    out.push('[');
    for (index, value) in embedding.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&value.to_string());
    }
    out.push(']');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::pgvector_literal;

    #[test]
    fn embeddings_format_as_pgvector_literals() {
        assert_eq!(pgvector_literal(&[1.0, 2.5, -3.0]).unwrap(), "[1,2.5,-3]");
        assert_eq!(pgvector_literal(&[]).unwrap(), "[]");
    }

    #[test]
    fn non_finite_values_are_rejected() {
        assert!(pgvector_literal(&[f32::NAN]).is_err());
        assert!(pgvector_literal(&[f32::INFINITY]).is_err());
    }
}
