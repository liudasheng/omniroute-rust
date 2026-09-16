//! Preserved-block extraction/restore (parity:
//! `open-sse/services/compression/preservation.ts`).
//!
//! Signal-carrying structures — fenced code, inline code, URLs, file paths,
//! error lines, stack frames — are tombstoned before rule engines run and
//! re-stitched verbatim afterwards, so compression never mangles them.

use fancy_regex::Regex;
use once_cell::sync::Lazy;

const SENTINEL_PREFIX: &str = "\u{0}OMNI_CAVEMAN";

#[derive(Debug, Clone)]
pub struct PreservedBlock {
    pub placeholder: String,
    pub content: String,
    pub kind: String,
}

static FENCED_CODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)```[\s\S]*?```|~~~[\s\S]*?~~~").unwrap());
static INLINE_CODE: Lazy<Regex> = Lazy::new(|| Regex::new(r"`[^`\n]+`").unwrap());
static URL: Lazy<Regex> = Lazy::new(|| Regex::new(r"https?://\S+").unwrap());
static FILE_PATH: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|\s)(?:\.{0,2}/[\w@./\-\\]+|[A-Za-z]:\\[\w.\\\-]+)").unwrap());
static ERROR_LINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s*(?:Error|TypeError|RangeError|SyntaxError|ReferenceError|EvalError|URIError|Exception)\b.*$").unwrap());
static STACK_FRAME: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^\s+at\s.*$").unwrap());

/// Extract protected structures into placeholders. Order matters: fenced code
/// first (so inline-code regexes do not split fences), then URLs/paths, then
/// error/stack lines.
pub fn extract_preserved_blocks(text: &str, _extra_patterns: &[Regex]) -> (String, Vec<PreservedBlock>) {
    let mut blocks: Vec<PreservedBlock> = Vec::new();
    let mut counter = 0usize;
    let mut add_block = |content: &str, kind: &str| -> String {
        let placeholder = format!("{SENTINEL_PREFIX}{counter}\u{0}");
        counter += 1;
        blocks.push(PreservedBlock {
            placeholder: placeholder.clone(),
            content: content.to_string(),
            kind: kind.to_string(),
        });
        placeholder
    };

    let mut current = text.to_string();
    for (re, kind) in [
        (&*FENCED_CODE, "fence"),
        (&*INLINE_CODE, "inline"),
        (&*URL, "url"),
        (&*FILE_PATH, "path"),
        (&*ERROR_LINE, "error"),
        (&*STACK_FRAME, "stack"),
    ] {
        let mut replaced = String::new();
        let mut last = 0usize;
        while let Ok(Some(m)) = re.find_from_pos(&current, last) {
            let start = m.start();
            let end = m.end();
            replaced.push_str(&current[last..start]);
            let matched = &current[start..end];
            if matched.contains(SENTINEL_PREFIX) {
                replaced.push_str(matched);
            } else {
                replaced.push_str(&add_block(matched, kind));
            }
            last = end;
        }
        replaced.push_str(&current[last..]);
        current = replaced;
    }
    (current, blocks)
}

/// Restore preserved blocks verbatim.
pub fn restore_preserved_blocks(text: &str, blocks: &[PreservedBlock]) -> String {
    let mut out = text.to_string();
    for b in blocks {
        out = out.replace(&b.placeholder, &b.content);
    }
    out
}

/// True when the text contains structures worth preserving (prefilter, then
/// full check — parity: `hasProtectedStructure`).
pub fn has_protected_structure(text: &str) -> bool {
    // prefilter: any of `~[]|$#\./:_()0-9
    if !text
        .chars()
        .any(|c| "`~[]|$#\\/:_()0123456789".contains(c))
    {
        return false;
    }
    FENCED_CODE.is_match(text).unwrap_or(false)
        || INLINE_CODE.is_match(text).unwrap_or(false)
        || URL.is_match(text).unwrap_or(false)
        || FILE_PATH.is_match(text).unwrap_or(false)
        || ERROR_LINE.is_match(text).unwrap_or(false)
        || STACK_FRAME.is_match(text).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_and_inline_code_survive() {
        let text = "Run this:\n```rust\nfn main() {}\n```\nand `cargo test` works";
        let (masked, blocks) = extract_preserved_blocks(text, &[]);
        assert!(!masked.contains("```"));
        let restored = restore_preserved_blocks(&masked, &blocks);
        assert_eq!(restored, text);
    }

    #[test]
    fn urls_and_paths_tombstoned() {
        let (masked, blocks) = extract_preserved_blocks("see https://x.example/a?b=1 and ./src/lib.rs", &[]);
        assert!(!masked.contains("https"));
        let restored = restore_preserved_blocks(&masked, &blocks);
        assert_eq!(restored, "see https://x.example/a?b=1 and ./src/lib.rs");
    }

    #[test]
    fn error_and_stack_lines_protected() {
        let text = "boom\nError: thing failed\n    at fn (a.rs:1)\ndone";
        let (masked, blocks) = extract_preserved_blocks(text, &[]);
        assert!(!masked.contains("Error:"));
        assert!(restore_preserved_blocks(&masked, &blocks) == text);
    }

    #[test]
    fn protected_structure_detector() {
        assert!(has_protected_structure("use `fmt` here"));
        assert!(has_protected_structure("GET https://a.b"));
        assert!(!has_protected_structure("plain prose without any markers"));
    }
}
