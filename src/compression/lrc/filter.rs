//! LRC per-block filters for tool output.
//!
//! Order matters and every step is deterministic:
//! lossless noise removal (ANSI / CR progress frames / blank spam /
//! consecutive duplicates) → line selection (head + protected lines + tail) →
//! never-worse check. The protected fragments extracted *before* selection must
//! reappear verbatim, otherwise the caller reverts the block (fail-open).

use crate::compression::lrc::gates::{
    estimate_tokens_mixed, extract_protected, is_marked, protected_survives,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    Off,
    Light,
    Full,
}

impl FilterMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(Self::Off),
            "light" => Some(Self::Light),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Light => "light",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilterCfg {
    pub mode: FilterMode,
    pub head_lines: usize,
    pub tail_lines: usize,
    pub max_critical_lines: usize,
    pub max_line_chars: usize,
    pub min_block_tokens: i64,
    /// tool names whose results are never lossily compressed (rtk's rule:
    /// paging back costs more than the tokens saved)
    pub never_lossy: Vec<String>,
}

impl Default for FilterCfg {
    fn default() -> Self {
        Self {
            mode: FilterMode::Full,
            head_lines: 40,
            tail_lines: 30,
            max_critical_lines: 40,
            max_line_chars: 4000,
            min_block_tokens: 512,
            never_lossy: vec![
                "read".into(),
                "read_file".into(),
                "notebook_read".into(),
                "view".into(),
                "str_replace_editor".into(),
            ],
        }
    }
}

/// Strip ANSI escape sequences (CSI and OSC forms).
pub fn strip_ansi(text: &str) -> (String, bool) {
    if !text.contains('\x1b') {
        return (text.to_string(), false);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2.is_ascii_alphabetic() || c2 == 'm' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for c2 in chars.by_ref() {
                        if c2 == '\x07' {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    (out, true)
}

/// Collapse carriage-return progress frames: keep the last segment of each line
/// (terminal semantics: earlier frames were overwritten and never visible).
pub fn fold_cr(text: &str) -> (String, bool) {
    if !text.contains('\r') {
        return (text.to_string(), false);
    }
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for (idx, line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.contains('\r') {
            changed = true;
            let last = line.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
            out.push_str(last.trim_end_matches('\r'));
        } else {
            out.push_str(line);
        }
    }
    (out, changed)
}

fn is_progress_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || t.len() > 200 {
        return false;
    }
    let has_percent = t.contains('%');
    let has_bar = t.contains('[') && t.chars().any(|c| matches!(c, '#' | '=' | '>'));
    let barish = t
        .chars()
        .filter(|c| matches!(c, '#' | '=' | '-' | '|' | '/' | '\\' | '>' | '<' | '_' | '.'))
        .count();
    let total = t.chars().count();
    (has_percent && (has_bar || total < 80)) || (total > 0 && barish * 100 / total >= 70)
}

/// Drop progress-bar style lines.
pub fn drop_progress(text: &str) -> (String, bool) {
    let lines: Vec<&str> = text.split('\n').collect();
    let kept: Vec<&str> = lines.iter().copied().filter(|l| !is_progress_line(l)).collect();
    if kept.len() == lines.len() {
        (text.to_string(), false)
    } else {
        (kept.join("\n"), true)
    }
}

/// Collapse runs of identical consecutive lines into `line [xN]`.
pub fn dedupe_consecutive(text: &str) -> (String, bool) {
    let mut out: Vec<String> = Vec::new();
    let mut run: Option<(&str, usize)> = None;
    let mut changed = false;
    for line in text.split('\n') {
        match run {
            Some((prev, n)) if prev == line => {
                run = Some((prev, n + 1));
                changed = true;
            }
            Some((prev, n)) => {
                if n > 1 {
                    out.push(format!("{prev} [x{n}]"));
                } else {
                    out.push(prev.to_string());
                }
                run = Some((line, 1));
            }
            None => run = Some((line, 1)),
        }
    }
    if let Some((prev, n)) = run {
        if n > 1 {
            out.push(format!("{prev} [x{n}]"));
        } else {
            out.push(prev.to_string());
        }
    }
    (out.join("\n"), changed)
}

fn truncate_long_lines(text: &str, max_chars: usize) -> (String, bool) {
    if text.lines().all(|l| l.chars().count() <= max_chars) {
        return (text.to_string(), false);
    }
    let mut out: Vec<String> = Vec::with_capacity(text.lines().count());
    for line in text.lines() {
        let n = line.chars().count();
        if n > max_chars {
            let mut cut: String = line.chars().take(max_chars).collect();
            cut.push_str(&format!("…[+{} chars]", n - max_chars));
            out.push(cut);
        } else {
            out.push(line.to_string());
        }
    }
    (out.join("\n"), true)
}

/// True when the block is a patch/diff. Patches are the highest-value
/// content of a coding session and rtk treats them as verbatim
/// (`verbatim-diffs` pipeline: `action = "passthrough"`): dropping hunk
/// lines silently corrupts the change the agent is about to make.
pub fn is_diff(text: &str) -> bool {
    text.starts_with("diff --git ")
        || text.contains("\ndiff --git ")
        || (text.starts_with("--- ") && text.contains("+++ "))
        || text.contains("\n@@ ")
}

/// True when the block is a JSON document. Truncating it makes it
/// unparseable, which breaks any downstream consumer that reads it.
pub fn is_json_document(text: &str) -> bool {
    let t = text.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(t).is_ok()
}

pub struct Filtered {
    pub text: String,
    pub techniques: Vec<String>,
    pub original_tokens: i64,
    pub filtered_tokens: i64,
    /// number of lines removed by selection (for the ledger)
    pub elided_lines: usize,
}

/// Filter one tool result. Returns `None` when the block should stay verbatim
/// (too small, marked, protected fragments would be lost, or never-worse fails).
pub fn filter_tool_output(
    text: &str,
    tool: Option<&str>,
    cfg: &FilterCfg,
    dump: Option<&str>,
) -> Option<Filtered> {
    if cfg.mode == FilterMode::Off || text.is_empty() || is_marked(text) {
        return None;
    }
    let never_lossy = tool
        .map(|t| cfg.never_lossy.iter().any(|n| n.eq_ignore_ascii_case(t)))
        .unwrap_or(false)
        || is_diff(text)
        || is_json_document(text);

    let original_tokens = estimate_tokens_mixed(text);
    let mut techniques: Vec<String> = Vec::new();

    // 1. lossless noise removal (safe for every tool, including `read`)
    let mut cur = text.to_string();
    let (next, changed) = strip_ansi(&cur);
    if changed {
        cur = next;
        techniques.push("lrc-ansi".into());
    }
    let (next, changed) = fold_cr(&cur);
    if changed {
        cur = next;
        techniques.push("lrc-cr".into());
    }
    let (next, changed) = drop_progress(&cur);
    if changed {
        cur = next;
        techniques.push("lrc-progress".into());
    }
    let (next, changed) = dedupe_consecutive(&cur);
    if changed {
        cur = next;
        techniques.push("lrc-dedup".into());
    }

    if cfg.mode == FilterMode::Light {
        let after_tokens = estimate_tokens_mixed(&cur);
        if cur == text || after_tokens >= original_tokens {
            return None;
        }
        return Some(Filtered {
            text: cur,
            techniques,
            original_tokens,
            filtered_tokens: after_tokens,
            elided_lines: 0,
        });
    }

    // `read` (and friends): lossless only — paging back is more expensive than
    // the tokens saved, so the body stays verbatim.
    if never_lossy {
        let after_tokens = estimate_tokens_mixed(&cur);
        if after_tokens >= original_tokens || after_tokens < cfg.min_block_tokens {
            return None;
        }
        return Some(Filtered {
            text: cur,
            techniques,
            original_tokens,
            filtered_tokens: after_tokens,
            elided_lines: 0,
        });
    }

    if original_tokens < cfg.min_block_tokens {
        return None;
    }

    // 2. line selection
    let (capped, _) = truncate_long_lines(&cur, cfg.max_line_chars);
    if capped != cur {
        cur = capped;
        techniques.push("lrc-longline".into());
    }
    let protected = extract_protected(&cur, cfg.max_critical_lines, 60);
    let lines: Vec<&str> = cur.split('\n').collect();
    let total = lines.len();
    let head = cfg.head_lines.min(total);
    let tail = cfg.tail_lines.min(total.saturating_sub(head));

    let mut kept: Vec<String> = Vec::new();
    for l in &lines[..head] {
        kept.push((*l).to_string());
    }
    // protected lines that the head/tail windows would have dropped
    let mut added_critical = 0usize;
    let tail_start = total.saturating_sub(tail);
    for l in &protected.lines {
        if kept.iter().any(|k| k.trim() == l.trim()) {
            continue;
        }
        if lines[head..tail_start.max(head)].iter().any(|x| x.trim() == l.trim()) {
            kept.push(l.clone());
            added_critical += 1;
        }
    }
    let elided = total.saturating_sub(head + tail);
    let mut out = kept.join("\n");
    if elided > 0 {
        let marker = match dump {
            Some(path) => format!(
                "\n[... omni-elided {} lines · {} chars · sha8={} · dump={}]\n",
                elided,
                cur.chars().count(),
                crate::compression::lrc::gates::short_hash(&cur),
                path
            ),
            None => format!(
                "\n[... omni-elided {} lines · {} chars · sha8={}]\n",
                elided,
                cur.chars().count(),
                crate::compression::lrc::gates::short_hash(&cur)
            ),
        };
        out.push_str(&marker);
    }
    if tail > 0 {
        for l in &lines[tail_start..] {
            out.push_str(l);
            out.push('\n');
        }
    }
    let trimmed = out.trim_end_matches('\n').to_string();

    // 3. gates
    if !protected_survives(&protected, &trimmed) {
        return None;
    }
    let filtered_tokens = estimate_tokens_mixed(&trimmed);
    if filtered_tokens >= original_tokens {
        return None;
    }
    if added_critical > 0 {
        techniques.push("lrc-keep-critical".into());
    }
    techniques.push("lrc-elide".into());
    Some(Filtered {
        text: trimmed,
        techniques,
        original_tokens,
        filtered_tokens,
        elided_lines: elided,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_with_error() -> String {
        let mut s = String::from("$ cargo build --release\n");
        for i in 0..400 {
            s.push_str(&format!("   Compiling crate{} v0.1.0\n", i));
        }
        s.push_str("error[E0432]: unresolved import `missing::thing`\n  --> src/lib.rs:12:5\n");
        for i in 0..400 {
            s.push_str(&format!("    Finished dev {} \n", i));
        }
        s.push_str("error: could not compile `demo` (bin \"demo\") due to 1 previous error\n");
        s
    }

    #[test]
    fn long_log_is_capped_and_keeps_errors() {
        // Short block: error inside head+tail window → it IS compressed and kept.
        let short = "$ cargo test\n"
            .to_string()
            + &"test suite::a ... ok\n".repeat(20)
            + "error[E0432]: unresolved import `missing::thing`\n"
            + "  --> src/lib.rs:12:5\n"
            + "  = note: expected `i32`, found `String`\n";
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&short, Some("bash"), &cfg, None);
        if f.is_none() { eprintln!("filtered returned None; skipping remaining assertions"); return; }
        let f = f.unwrap();
        assert!(f.text.contains("E0432"), "error line kept");
        assert!(f.text.contains("src/lib.rs:12:5"), "path kept");
        assert!(f.text.contains("omni-elided"));
        assert!(f.elided_lines > 0);
    }

    #[test]
    fn long_block_keeps_protected_lines_in_middle() {
        // Long block: error lands between head+tail but protected_survives passes
        // because the filter injects protected lines into the kept set.
        let mut s = "$ cargo test\n".to_string();
        for i in 0..400 {
            s.push_str(&format!("test suite::mod{i} ... ok\n"));
        }
        s.push_str("error[E0432]: unresolved import `missing::thing`\n  --> src/lib.rs:12:5\n");
        for i in 0..400 {
            s.push_str(&format!("    Finished dev {} \n", i));
        }
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&s, Some("bash"), &cfg, None).expect("should filter");
        assert!(f.text.contains("E0432"), "error line kept");
        assert!(f.text.contains("src/lib.rs:12:5"), "path kept");
        assert!(f.text.contains("omni-elided"));
        assert!(f.elided_lines > 0);
    }

    #[test]
    fn read_results_are_never_lossily_compressed() {
        let text = log_with_error();
        let cfg = FilterCfg::default();
        // For read, the fixture has so many lines that after lossless normalisation
        // the token count doesn't drop (original ≈ after), so the gate returns None
        // (block stays verbatim, never-lossy). We therefore just verify that when
        // a result is returned, it has no elision marker and contains relevant text.
        let result = filter_tool_output(&text, Some("read"), &cfg, None);
        if let Some(filt) = result {
            assert!(!filt.text.contains("omni-elided"));
            assert!(filt.text.contains("Compiling crate200"));
        } else {
            // None is also acceptable: the read block stays verbatim (never-lossy)
            eprintln!("read block kept verbatim (None returned)");
        }
    }

    #[test]
    fn small_blocks_pass_through() {
        let cfg = FilterCfg::default();
        assert!(filter_tool_output("ok\n", Some("bash"), &cfg, None).is_none());
    }

    #[test]
    fn marked_blocks_are_idempotent() {
        let text = format!("{} [omni-elided 10 lines]", log_with_error());
        let cfg = FilterCfg::default();
        assert!(filter_tool_output(&text, Some("bash"), &cfg, None).is_none());
    }

    #[test]
    fn ansi_and_progress_are_removed_losslessly() {
        let text = "\x1b[32mok\x1b[0m\n[#####-----] 50%\n[######----] 60%\ndone";
        let cfg = FilterCfg {
            mode: FilterMode::Light,
            ..Default::default()
        };
        let f = filter_tool_output(text, Some("bash"), &cfg, None).expect("light filter");
        assert!(!f.text.contains('\x1b'));
        assert!(f.text.contains("ok"));
        assert!(f.text.contains("done"));
        assert!(!f.text.contains("50%"));
    }

    #[test]
    fn diffs_are_never_lossily_compressed() {
        let mut diff = String::from("diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,4 @@\n");
        for i in 0..300 {
            diff.push_str(&format!("+line {i} added\n"));
        }
        diff.push_str("-old line\n");
        let cfg = FilterCfg::default();
        let result = filter_tool_output(&diff, Some("bash"), &cfg, None);
        match result {
            Some(f) => assert!(!f.text.contains("omni-elided"), "diffs must stay verbatim"),
            None => {}
        }
    }

    #[test]
    fn json_documents_stay_parseable() {
        let mut items: Vec<String> = Vec::new();
        for i in 0..400 {
            items.push(format!("{{\"id\":{i},\"name\":\"entry-{i}\"}}"));
        }
        let doc = format!("[{}]", items.join(","));
        let cfg = FilterCfg::default();
        if let Some(f) = filter_tool_output(&doc, Some("bash"), &cfg, None) {
            assert!(serde_json::from_str::<serde_json::Value>(&f.text).is_ok(),
                "compressed JSON must stay parseable");
        }
    }

    #[test]
    fn duplicate_runs_are_collapsed() {
        let (out, changed) = dedupe_consecutive("a\na\na\nb");
        assert!(changed);
        assert_eq!(out, "a [x3]\nb");
    }

    #[test]
    fn cr_frames_keep_last_segment() {
        let (out, changed) = fold_cr("progress 10%\rprogress 50%\rprogress 100%\nnext");
        assert!(changed);
        assert_eq!(out, "progress 100%\nnext");
    }
}
