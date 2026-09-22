//! Token estimation (parity: `open-sse/services/compression/stats.ts`).

/// Estimate a string the same way JavaScript's `Math.ceil(text.length / 4)`
/// does. `text.length` counts UTF-16 code units, not Unicode scalar values.
pub fn estimate_tokens(text: &str) -> i64 {
    if text.is_empty() {
        return 0;
    }
    ((text.encode_utf16().count() as f64) / 4.0).ceil() as i64
}

/// Estimate a structured request as compact JSON followed by the same
/// character heuristic used by the original compression stats.
pub fn estimate_value_tokens(value: &serde_json::Value) -> i64 {
    if value.is_null() {
        return 0;
    }
    serde_json::to_string(value)
        .map(|serialized| estimate_tokens(&serialized))
        .unwrap_or(0)
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

    #[test]
    fn structured_values_include_json_shape() {
        let value = serde_json::json!({"messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(estimate_value_tokens(&value), estimate_tokens(&serde_json::to_string(&value).unwrap()));
        assert!(estimate_value_tokens(&value) > estimate_tokens("hello"));
    }

    #[test]
    fn unicode_uses_utf16_code_units() {
        assert_eq!(estimate_tokens("😀😀"), 1);
    }
}
