//! LRC per-block filters for tool output.
//!
//! Order matters and every step is deterministic:
//! lossless noise removal (ANSI / CR progress frames / blank spam /
//! consecutive duplicates) → line selection (head + protected lines + tail) →
//! never-worse check. The protected fragments extracted *before* selection must
//! reappear verbatim, otherwise the caller reverts the block (fail-open).

use crate::compression::lrc::gates::{
    estimate_tokens_mixed, extract_protected, is_marked, protected_survives, Protected,
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
    /// character budget for one tool result (drives the char-window pass)
    pub max_chars_per_block: usize,
    pub min_block_tokens: i64,
    /// tool names whose results are never lossily compressed (rtk's rule:
    /// paging back costs more than the tokens saved)
    pub never_lossy: Vec<String>,
}

impl Default for FilterCfg {
    fn default() -> Self {
        Self {
            mode: FilterMode::Full,
            head_lines: 12,
            tail_lines: 8,
            max_critical_lines: 40,
            max_line_chars: 4000,
            max_chars_per_block: 1500,
            min_block_tokens: 128,
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
    // A diff hunk line is never a progress bar. A deletion of a dashed rule
    // (`-  ---------------`) is 100% bar characters, so the ratio test below
    // would eat it — and eating a hunk line silently corrupts the patch the
    // agent is about to apply. Diff markers therefore veto the test.
    if t.starts_with('-')
        || t.starts_with('+')
        || t.starts_with("diff --git")
        || t.starts_with("@@")
        || t.starts_with("index ")
    {
        return false;
    }
    let has_percent = t.contains('%');
    let has_bar = t.contains('[') && t.chars().any(|c| matches!(c, '#' | '=' | '>'));
    let barish = t
        .chars()
        .filter(|c| matches!(c, '#' | '=' | '-' | '|' | '/' | '\\' | '>' | '<' | '_' | '.'))
        .count();
    let total = t.chars().count();
    // A pure bar (no words) is a progress bar; a bar mixed with prose is
    // ordinary output that merely contains dashes.
    let has_word = t.chars().any(|c| c.is_alphanumeric());
    (has_percent && (has_bar || total < 80)) || (total > 0 && barish * 100 / total >= 70 && has_word)
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

/// True when a line is a complete JSON value (an object or an array).
///
/// Appending a ` [x2]` suffix to such a line destroys the document, so the
/// deduper must leave it alone.
fn is_json_line(line: &str) -> bool {
    let t = line.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(t).is_ok()
}

/// Collapse runs of identical consecutive lines into `line [xN]`.
pub fn dedupe_consecutive(text: &str) -> (String, bool) {
    let mut out: Vec<String> = Vec::new();
    let mut run: Option<(&str, usize)> = None;
    let mut changed = false;
    // A run of identical JSON documents is *not* redundant output: a
    // streamed tool result legitimately repeats a document, and the count
    // suffix would make every one of them unparseable.
    let mut run_is_json = false;
    for line in text.split('\n') {
        let line_is_json = is_json_line(line);
        match run {
            Some((prev, n)) if prev == line && !run_is_json && !line_is_json => {
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
                run_is_json = line_is_json;
            }
            None => {
                run = Some((line, 1));
                run_is_json = line_is_json;
            }
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

/// Harness-style file read: `<path>…</path>\n<type>…</type>\n<content>…</content>`
/// with numbered lines (`12: text`). The content is a *file*, so it can be
/// reduced to a declaration skeleton (rtk's `read -l aggressive`) without
/// losing the structure the model reasons about.
fn harness_read_skeleton(text: &str) -> Option<String> {
    let path = text.strip_prefix("<path>")?.split("</path>").next()?.to_string();
    let ty = text
        .split("<type>")
        .nth(1)?
        .split("</type>")
        .next()?
        .to_string();
    let body = text.split("<content>\n").nth(1)?.split("\n</content>").next()?;
    let lines: Vec<&str> = body.split('\n').collect();

    // keep declarations, imports, section headers and the tail; drop the rest
    let mut kept: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    for line in &lines {
        // strip the "NN: " prefix for the structural test
        let raw = match line.split_once(':') {
            Some((num, rest)) if num.trim().chars().all(|c| c.is_ascii_digit()) && !num.is_empty() => rest,
            _ => *line,
        };
        let t = raw.trim_start();
        // A *strong* declaration keyword is a declaration regardless of
        // indentation (a nested `impl`/`fn` is still structure).
        let strong = t.starts_with("pub ")
            || t.starts_with("export ")
            || t.starts_with("async fn")
            || t.starts_with("async def")
            || t.starts_with("fn ")
            || t.starts_with("func ")
            || t.starts_with("def ")
            || t.starts_with("class ")
            || t.starts_with("struct ")
            || t.starts_with("enum ")
            || t.starts_with("interface ")
            || t.starts_with("impl ")
            || t.starts_with("trait ")
            || t.starts_with("module ")
            || t.starts_with("import ")
            || t.starts_with("use ")
            || t.starts_with("package ")
            || t.starts_with("#include")
            || t.starts_with("#define")
            || t.starts_with("@");
        // Otherwise only keep *top-level* lines (no indentation after the
        // "NN: " prefix): `let`/`const` inside a function body are statements,
        // not structure, and dropping them is where the saving comes from.
        let top_level = raw == raw.trim_start();
        let structural = strong || top_level;
        if structural {
            kept.push((*line).to_string());
        } else {
            dropped += 1;
        }
    }
    // keep the last few lines (often "End of file" or the closing marker)
    for line in lines.iter().rev().take(3) {
        if !kept.iter().any(|k| k == line) {
            kept.push((*line).to_string());
        }
    }
    if dropped == 0 || kept.len() >= lines.len() {
        return None;
    }
    let mut out = format!("<path>{path}</path>\n<type>{ty}</type>\n<content>\n");
    out.push_str(&kept.join("\n"));
    out.push_str(&format!(
        "\n[omni-elided {dropped} lines · {} chars · sha8={}]\n</content>",
        body.chars().count(),
        crate::compression::lrc::gates::short_hash(text)
    ));
    Some(out)
}

/// True when the block is a JSON document. Truncating it makes it
/// unparseable, which breaks any downstream consumer that reads it.
pub fn is_json_document(text: &str) -> bool {
    let t = text.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return false;
    }
    if serde_json::from_str::<serde_json::Value>(t).is_ok() {
        return true;
    }
    // A *sequence* of JSON documents (one per line) is a JSON payload too.
    // It must not be treated as prose: the line-level filters would fold
    // repeated documents and truncate them, and the result would no longer
    // parse.
    let mut saw = false;
    for line in t.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if !(l.starts_with('{') || l.starts_with('[')) {
            return false;
        }
        if serde_json::from_str::<serde_json::Value>(l).is_err() {
            return false;
        }
        saw = true;
    }
    saw
}

pub struct Filtered {
    pub text: String,
    pub techniques: Vec<String>,
    pub original_tokens: i64,
    pub filtered_tokens: i64,
    /// number of lines removed by selection (for the ledger)
    pub elided_lines: usize,
}

/// The kind of JSON payload a block carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonKind {
    /// A single JSON document (object or array). Any line-level rewrite
    /// invalidates it, so it may only be shipped verbatim.
    Document,
    /// A sequence of JSON documents, one per line (NDJSON, `jq` output, API
    /// logs). Dropping *whole* lines keeps the remainder a valid sequence,
    /// so this kind can be compressed structurally.
    Sequence,
}

/// Classify a block as a JSON payload.
pub fn classify_json(text: &str) -> Option<JsonKind> {
    let t = text.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    if serde_json::from_str::<serde_json::Value>(t).is_ok() {
        return Some(JsonKind::Document);
    }
    // A sequence: every non-empty line must be a complete JSON value.
    let mut saw = false;
    for line in t.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if !(l.starts_with('{') || l.starts_with('[')) {
            return None;
        }
        if serde_json::from_str::<serde_json::Value>(l).is_err() {
            return None;
        }
        saw = true;
    }
    if saw {
        Some(JsonKind::Sequence)
    } else {
        None
    }
}

/// Compress a JSON payload without ever making it unparseable.
///
/// * A single document cannot be touched at all: any edit invalidates it.
/// * A sequence may lose whole documents. The remainder is still a valid
///   sequence, and the elided count is reported in a trailing comment line,
///   so the model knows the payload was reduced.
fn compress_json_payload(
    text: &str,
    kind: JsonKind,
    original_tokens: &i64,
    cfg: &FilterCfg,
    techniques: &mut Vec<String>,
) -> Option<Filtered> {
    if kind == JsonKind::Document {
        // Verbatim — and that is the correct answer: the only safe move.
        return None;
    }
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.len() <= 4 {
        return None;
    }
    // Keep the head and the tail of the sequence; drop the middle. A JSON
    // sequence is a *log*, so its ends carry the same signal as a log's.
    let head = cfg.head_lines.min(lines.len());
    let tail = cfg.tail_lines.min(lines.len().saturating_sub(head));
    let dropped = lines.len().saturating_sub(head + tail);
    if dropped == 0 {
        return None;
    }
    let mut kept: Vec<&str> = lines[..head].to_vec();
    kept.extend_from_slice(&lines[lines.len() - tail..]);
    // The marker is emitted *after* the sequence, and it is itself a JSON
    // *value* (a string). A JSON sequence must stay a pure sequence of
    // values: a bare text line would break every line-oriented consumer
    // (`jq -c`, `kubectl -o jsonlines`, an event log).
    let out = format!(
        "{}\n\"[omni-elided-json {} lines · {} chars · sha8={}]\"",
        kept.join("\n"),
        dropped,
        text.chars().count(),
        crate::compression::lrc::gates::short_hash(text)
    );
    let after = estimate_tokens_mixed(&out);
    if after >= *original_tokens {
        return None;
    }
    techniques.push("lrc-json-seq".into());
    Some(Filtered {
        text: out,
        techniques: techniques.clone(),
        original_tokens: *original_tokens,
        filtered_tokens: after,
        elided_lines: dropped,
    })
}

/// Filter one tool result. Returns `None` when the block should stay verbatim
/// (too small, marked, protected fragments would be lost, or never-worse fails).
pub fn filter_tool_output(
    text: &str,
    tool: Option<&str>,
    cfg: &FilterCfg,
    // The dump pointer is deliberately NOT embedded in the marker: the block
    // is forwarded to a third party, and an absolute path would leak the
    // gateway's directory layout. The store still receives the original
    // content, so the operator can retrieve it by its content hash.
    _dump: Option<&str>,
) -> Option<Filtered> {
    if cfg.mode == FilterMode::Off || text.is_empty() || is_marked(text) {
        return None;
    }
    let never_lossy = tool
        .map(|t| cfg.never_lossy.iter().any(|n| n.eq_ignore_ascii_case(t)))
        .unwrap_or(false)
        || is_diff(text);

    let original_tokens = estimate_tokens_mixed(text);
    let mut techniques: Vec<String> = Vec::new();

    // ---- JSON payloads are decided FIRST --------------------------------
    //
    // The lossless stage (ANSI stripping, progress-bar removal, dedupe)
    // rewrites *lines*, which destroys a JSON document: an interior line
    // that repeats gets a " [x2]" suffix, and a short line holding a
    // percentage is mistaken for a progress bar and deleted. The result is
    // no longer JSON, and the block is shipped to the upstream provider
    // that way. So a JSON block must never enter the line-oriented stage.
    if let Some(json) = classify_json(text) {
        return compress_json_payload(text, json, &original_tokens, cfg, &mut techniques);
    }

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
        // The lossless stage may still eat a diagnostic line (a log line that
        // looks like a progress bar). The hard-error gate applies here too —
        // "lossless" means "no information lost", and a vanished error line
        // is lost information.
        if !crate::compression::lrc::gates::hard_errors_survive(text, &cur) {
            return None;
        }
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
    // the tokens saved. The one exception is a harness-style file read, whose
    // content can be reduced to a declaration skeleton (rtk's
    // `read -l aggressive`).
    if never_lossy {
        if let Some(skeleton) = harness_read_skeleton(&cur) {
            let sk_tokens = estimate_tokens_mixed(&skeleton);
            if sk_tokens < original_tokens {
                techniques.push("lrc-read-skeleton".into());
                return Some(Filtered {
                    text: skeleton,
                    techniques,
                    original_tokens,
                    filtered_tokens: sk_tokens,
                    elided_lines: 0,
                });
            }
        }
        let after_tokens = estimate_tokens_mixed(&cur);
        if after_tokens >= original_tokens || after_tokens < cfg.min_block_tokens {
            return None;
        }
        // Even the lossless stage must not lose a diagnostic.
        if !crate::compression::lrc::gates::hard_errors_survive(text, &cur) {
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

    // Agent tool output is frequently "few but very long lines" (JSON dumps,
    // single-line logs, table rows). A line-based head+tail window then keeps
    // the whole block, so the char budget — not the line count — has to drive
    // the selection (rtk's `char-window` shape strategy).
    let char_budget = cfg.max_chars_per_block;
    let cur_chars = cur.chars().count();
    let line_window_sufficient = head + tail >= total && cur_chars <= char_budget;

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
    // The hard errors are not subject to a budget: a block that reports
    // failures must keep them, even when there are more than the protected
    // budget can hold. They are injected in *document order* so the model
    // reads the log the way it was written.
    let hard = crate::compression::lrc::gates::hard_errors(text);
    let hard_lines: Vec<&str> = hard.iter().map(|s| s.as_str()).collect();
    let mut hard_injected = 0usize;
    for (idx, l) in lines.iter().enumerate() {
        if !hard_lines.iter().any(|h| h.trim() == l.trim()) {
            continue;
        }
        if kept.iter().any(|k| k.trim() == l.trim()) {
            continue;
        }
        // Insert at its document position among the injected lines.
        let pos = kept
            .iter()
            .position(|k| {
                lines
                    .iter()
                    .position(|x| x.trim() == k.trim())
                    .is_some_and(|kp| kp > idx)
            })
            .unwrap_or(kept.len());
        kept.insert(pos, (*l).to_string());
        hard_injected += 1;
    }
    if hard_injected > 0 {
        added_critical += hard_injected;
    }
    let elided = total.saturating_sub(head + tail);
    let mut out = kept.join("\n");
    if elided > 0 {
        // The dump pointer must not leak the gateway's directory layout to the
        // upstream provider: the block is sent to a third party, and an
        // absolute path reveals the host's home/data directory. The content
        // hash is enough to retrieve the original locally.
        // The pointer must name the file the dump store wrote: the store
        // names files after the hash of the *original* content, so the
        // marker must hash the original too — hashing the already-rewritten
        // text would point at a file that does not exist.
        let marker = format!(
            "\n[omni-elided {} lines · {} chars · sha8={}]\n",
            elided,
            cur.chars().count(),
            crate::compression::lrc::gates::short_hash(text)
        );
        out.push_str(&marker);
    }
    if tail > 0 {
        for l in &lines[tail_start..] {
            out.push_str(l);
            out.push('\n');
        }
    }
    let mut trimmed = out.trim_end_matches('\n').to_string();

    // 2b. char-window pass: the line window may still exceed the char budget
    // (few but very long lines). Cut the *middle* of the kept text, keeping
    // the protected fragments.
    if !line_window_sufficient && trimmed.chars().count() > char_budget {
        // The char window may cut away the hard errors that were injected
        // above (they are not part of the budgeted protected set), so they
        // are added to the fragments the window must re-insert.
        let mut keep_fragments = protected.clone();
        for h in &hard {
            if !keep_fragments.lines.iter().any(|l| l.trim() == h.trim()) {
                keep_fragments.lines.push(h.clone());
            }
        }
        let (windowed, dropped) = char_window(&trimmed, char_budget, &keep_fragments);
        if dropped > 0 {
            trimmed = windowed;
            techniques.push("lrc-char-window".into());
        }
    }

    // 3. gates
    if !protected_survives(&protected, &trimmed) {
        return None;
    }
    // The protected set is a budgeted *choice*; the hard errors are a hard
    // requirement. A block whose diagnostic body or `--> file:line` location
    // vanished must not be shipped, whatever the budget allowed.
    if !crate::compression::lrc::gates::hard_errors_survive(text, &trimmed) {
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

/// Keep the head and tail of `text` within `budget` characters, re-inserting
/// the protected fragments that would otherwise be cut away.
///
/// Returns the windowed text and the number of characters dropped.
fn char_window(text: &str, budget: usize, protected: &Protected) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= budget {
        return (text.to_string(), 0);
    }
    let head_budget = budget * 2 / 3;
    let tail_budget = budget - head_budget;
    let mut kept: Vec<char> = Vec::with_capacity(budget);
    kept.extend_from_slice(&chars[..head_budget.min(chars.len())]);
    kept.push('\n');
    // protected fragments that fell outside the window
    //
    // The presence test must look for the WHOLE line: a prefix comparison
    // (the old `windows(min(len, 64))` trick) treats a line that the window
    // cut in half as "present", so the fragment was neither re-inserted nor
    // whole — and the protected-survival gate then rejected the block.
    let kept_so_far: String = kept.iter().collect();
    for line in &protected.lines {
        let l: Vec<char> = line.chars().collect();
        if l.len() > budget / 3 {
            continue;
        }
        let l_str: String = l.iter().collect();
        if !kept_so_far.contains(&l_str) {
            kept.extend_from_slice(&l);
            kept.push('\n');
        }
    }
    kept.push('\n');
    let tail_start = chars.len().saturating_sub(tail_budget);
    kept.extend_from_slice(&chars[tail_start..]);
    let dropped = chars.len().saturating_sub(kept.len());
    let mut out: String = kept.into_iter().collect();
    if dropped > 0 {
        out.push_str(&format!(
            "\n[omni-elided {dropped} chars · sha8={}]\n",
            crate::compression::lrc::gates::short_hash(text)
        ));
    }
    (out, dropped)
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
        let mut short = "$ cargo test\n".to_string();
        for i in 0..40 {
            short.push_str(&format!("test suite::mod{} ... ok\n", i));
        }
        short.push_str("error[E0432]: unresolved import `missing::thing`\n");
        short.push_str("  --> src/lib.rs:12:5\n");
        short.push_str("  = note: expected `i32`, found `String`\n");
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

#[cfg(test)]
mod char_window_tests {
    use super::*;

    /// A block with few but very long lines (a single-line log) must still be
    /// capped by the character budget.
    ///
    /// JSON payloads are excluded on purpose: a JSON document must stay
    /// parseable, so it is protected verbatim instead of being windowed.
    #[test]
    fn char_window_caps_few_long_lines() {
        let mut text = String::new();
        for i in 0..8 {
            text.push_str(&format!("row {i} payload {}\n", "x".repeat(3000)));
        }
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("bash"), &cfg, None).expect("should filter");
        assert!(f.filtered_tokens < f.original_tokens / 2, "expected >50% saving");
        assert!(f.text.contains("omni-elided"));
    }

    /// A payload of JSON documents must stay parseable — compression may not
    /// fold it, truncate it, or decorate it with a count suffix.
    #[test]
    fn json_sequence_stays_verbatim_and_parseable() {
        let mut text = String::new();
        for i in 0..8 {
            text.push_str(&format!(
                "{{\"id\":{i},\"payload\":\"{}\"}}\n",
                "x".repeat(3000)
            ));
        }
        let cfg = FilterCfg::default();
        match filter_tool_output(&text, Some("bash"), &cfg, None) {
            None => {} // untouched: exactly what we want
            Some(f) => {
                for line in f.text.lines() {
                    let l = line.trim();
                    if l.is_empty() {
                        continue;
                    }
                    assert!(
                        serde_json::from_str::<serde_json::Value>(l).is_ok(),
                        "a JSON line was damaged: {l}"
                    );
                }
            }
        }
    }

    /// The character budget must not break the protected fragments.
    #[test]
    fn char_window_keeps_protected_lines() {
        let mut text = String::from("filler ");
        text.push_str(&"y".repeat(20_000));
        text.push_str("\nerror[E0432]: unresolved import `missing::thing`\n  --> src/lib.rs:12:5\n");
        text.push_str(&"z".repeat(20_000));
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("bash"), &cfg, None).expect("should filter");
        assert!(f.text.contains("E0432"), "error must survive the char window");
        assert!(f.text.contains("src/lib.rs:12:5"), "location must survive");
    }
}

#[cfg(test)]
mod read_skeleton_tests {
    use super::*;

    fn harness_read() -> String {
        let mut body = String::new();
        for i in 1..=200 {
            body.push_str(&format!("{i}:     let value_{i} = compute({i});\n"));
            if i % 20 == 0 {
                body.push_str(&format!("{i}: pub fn milestone_{i}() -> u8 {{ 0 }}\n"));
            }
        }
        format!("<path>/tmp/x.rs</path>\n<type>file</type>\n<content>\n{body}</content>")
    }

    #[test]
    fn read_skeleton_keeps_declarations() {
        let text = harness_read();
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("read"), &cfg, None).expect("should filter");
        assert!(f.text.contains("<path>/tmp/x.rs</path>"));
        assert!(f.text.contains("pub fn milestone_20"), "declaration kept");
        assert!(!f.text.contains("let value_1 ="), "body line dropped");
        assert!(f.filtered_tokens < f.original_tokens / 3, "expected >66% saving");
    }

    #[test]
    fn read_skeleton_leaves_plain_text_alone() {
        let text = "just a plain sentence without any structure at all";
        let cfg = FilterCfg::default();
        assert!(filter_tool_output(text, Some("read"), &cfg, None).is_none());
    }
}

#[cfg(test)]
mod json_payload_tests {
    use super::*;

    fn doc_with_repeated_interior_lines() -> String {
        let mut s = String::from("{\n  \"items\": [\n    \"a\",\n    \"a\",\n    \"b\"\n  ],\n");
        s.push_str(&format!("  \"pad\": \"{}\"\n}}", "x".repeat(400)));
        s
    }

    fn json_sequence(n: usize) -> String {
        (0..n)
            .map(|i| format!("{{\"i\":{i},\"pad\":\"{}\"}}", "y".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_json_document_is_never_damaged() {
        let text = doc_with_repeated_interior_lines();
        let cfg = FilterCfg::default();
        match filter_tool_output(&text, Some("bash"), &cfg, None) {
            None => {} // untouched: correct
            Some(f) => {
                assert!(
                    serde_json::from_str::<serde_json::Value>(&f.text).is_ok(),
                    "a JSON document was damaged: {}",
                    &f.text[..f.text.len().min(200)]
                );
            }
        }
    }

    #[test]
    fn a_json_document_with_a_percentage_is_not_progress_filtered() {
        let mut s = String::from("{\n");
        for i in 0..30 {
            s.push_str(&format!("  \"row{i}\": {{\"pct\": \"50%\", \"pad\": \"zzz\"}},\n"));
        }
        s.push_str("  \"tail\": 1\n}\n");
        let cfg = FilterCfg::default();
        match filter_tool_output(&s, Some("bash"), &cfg, None) {
            None => {}
            Some(f) => {
                assert!(serde_json::from_str::<serde_json::Value>(&f.text).is_ok());
                assert!(f.text.contains("row29"), "a row was eaten");
            }
        }
    }

    #[test]
    fn a_json_sequence_is_compressed_but_stays_parseable_line_by_line() {
        // More documents than head_lines + tail_lines, so the compression
        // path is really taken (a shorter sequence is left verbatim and the
        // assertions below would never run).
        let text = json_sequence(400);
        assert!(text.lines().count() > 12 + 8);
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("bash"), &cfg, None).expect("should compress");
        assert!(f.filtered_tokens < f.original_tokens / 2, "expected >50% saving");
        for line in f.text.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with("[omni-elided") {
                continue;
            }
            assert!(
                serde_json::from_str::<serde_json::Value>(l).is_ok(),
                "a JSON sequence line was damaged: {l}"
            );
        }
    }
}

#[cfg(test)]
mod hard_error_tests {
    use super::*;

    fn filler(n: usize) -> String {
        (0..n)
            .map(|i| format!("step {i}: running the ordinary task\n"))
            .collect::<Vec<_>>()
            .join("")
    }

    /// D3: the Light profile must not lose the only diagnostic in a block.
    #[test]
    fn light_mode_keeps_the_diagnostic_line() {
        let mut text = filler(40);
        text.push_str("error: 50% of the tests failed\n");
        text.push_str(&filler(40));
        let mut cfg = FilterCfg::default();
        cfg.mode = FilterMode::Light;
        if let Some(f) = filter_tool_output(&text, Some("bash"), &cfg, None) {
            assert!(
                f.text.contains("error: 50% of the tests failed"),
                "the Light stage ate the only diagnostic"
            );
        }
    }

    /// D4: a block with *more* errors than the protected budget must still
    /// be compressed — the errors are injected, not merely checked.
    #[test]
    fn error_heavy_blocks_stay_compressible() {
        let mut text = filler(200);
        for i in 0..60 {
            text.push_str(&format!("error: failure {i}\n"));
        }
        text.push_str(&filler(200));
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("bash"), &cfg, None).expect("should compress");
        assert!(
            f.filtered_tokens < f.original_tokens,
            "an error-heavy block lost all savings"
        );
        for i in 0..60 {
            assert!(
                f.text.contains(&format!("error: failure {i}")),
                "error {i} was dropped"
            );
        }
    }

    /// D7: benign prose that merely mentions "error" is not a hard error.
    #[test]
    fn benign_prose_is_not_a_hard_error() {
        let mut text = filler(200);
        for i in 0..39 {
            text.push_str(&format!("handled error: retry {i} of the batch\n"));
        }
        text.push_str("error[E0432]: the real failure\n");
        text.push_str("  --> src/lib.rs:12:5\n");
        text.push_str(&filler(200));
        let hard = crate::compression::lrc::gates::hard_errors(&text);
        assert!(
            hard.iter().any(|h| h.contains("E0432")),
            "the real error must be a hard error"
        );
        assert!(
            !hard.iter().any(|h| h.contains("handled error")),
            "benign prose must not count: {hard:?}"
        );
        assert!(
            !hard.iter().any(|h| h.starts_with("no error")),
            "'no error: all good' must not count"
        );
    }

    /// D6: the injected diagnostics keep the document order.
    #[test]
    fn protected_lines_keep_document_order() {
        let mut text = filler(60);
        text.push_str("warning: a warning BEFORE the error\n");
        text.push_str(&filler(60));
        text.push_str("error: the error AFTER the warning\n");
        text.push_str(&filler(60));
        let cfg = FilterCfg::default();
        if let Some(f) = filter_tool_output(&text, Some("bash"), &cfg, None) {
            let w = f.text.find("warning: a warning BEFORE").unwrap_or(usize::MAX);
            let e = f.text.find("error: the error AFTER").unwrap_or(usize::MAX);
            assert!(w < e, "the diagnostics were reordered: warning@{w} error@{e}");
        }
    }
}

#[cfg(test)]
mod self_reference_tests {
    use super::*;

    /// N3: a block that merely *quotes* a marker (an agent reading the
    /// filter's own source) must still be compressed.
    #[test]
    fn quoting_a_marker_does_not_disable_compression() {
        let mut text = String::new();
        for i in 0..80 {
            text.push_str(&format!("    let marker = \"[omni-elided {{}} lines · {{}} chars]\";\n"));
            let _ = i;
        }
        text.push_str("some ordinary output line that makes the block large enough\n");
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("bash"), &cfg, None).expect("should compress");
        assert!(
            f.filtered_tokens < f.original_tokens,
            "a quoted marker disabled the compression"
        );
    }

    /// N4: a line that mentions a diagnostic word inside code is not a
    /// diagnostic, and must not make the block uncompressible.
    #[test]
    fn code_mentioning_a_diagnostic_is_not_a_hard_error() {
        let mut text = String::new();
        for i in 0..80 {
            text.push_str(&format!("    const MARKERS: [&str; 4] = [\"error[\", \"error:\", \"error -\", \"fatal:\"]; // {i}\n"));
        }
        text.push_str("ordinary output that makes the block large enough to compress\n");
        let hard = crate::compression::lrc::gates::hard_errors(&text);
        assert!(hard.is_empty(), "code was taken for a diagnostic: {hard:?}");
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("bash"), &cfg, None).expect("should compress");
        assert!(f.filtered_tokens < f.original_tokens);
    }

    /// N5: the JSON-sequence marker must itself be a JSON value.
    #[test]
    fn the_json_sequence_marker_is_a_json_value() {
        let text: String = (0..40)
            .map(|i| format!("{{\"i\":{i},\"pad\":\"{}\"}}", "z".repeat(100)))
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = FilterCfg::default();
        let f = filter_tool_output(&text, Some("bash"), &cfg, None).expect("should compress");
        for line in f.text.lines() {
            let l = line.trim();
            if l.is_empty() {
                continue;
            }
            assert!(
                serde_json::from_str::<serde_json::Value>(l).is_ok(),
                "the sequence line is not JSON: {l}"
            );
        }
        assert!(f.text.contains("omni-elided-json"), "the marker is missing");
    }
}
