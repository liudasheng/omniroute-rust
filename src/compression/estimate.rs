//! Token estimation (parity: `open-sse/services/compression/stats.ts` —
//! `estimateCompressionTokens` ≈ chars / 4).

/// chars/4 estimate, matching the original's `charTokensOf`.
pub fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    ((text.chars().count() as f64) / 4.0).ceil() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chars_per_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("ab"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2); // 8/4
        assert_eq!(estimate_tokens("abcdefghi"), 3); // 9/4 → ceil 3
    }
}
