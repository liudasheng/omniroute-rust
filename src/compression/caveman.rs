//! Caveman compression engine (parity: `open-sse/services/compression/caveman.ts`
//! + `cavemanRules.ts`).
//!
//! Rule-based phrase compression with three intensity levels
//! (lite/full/ultra), per-role rule contexts, preserved-block tombstoning,
//! artifact cleanup and sentence recapitalization.

use crate::compression::estimate::estimate_tokens;
use crate::compression::preserve::{
    extract_preserved_blocks, has_protected_structure, restore_preserved_blocks,
};
use fancy_regex::Regex;
use once_cell::sync::Lazy;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Intensity {
    Lite,
    Full,
    Ultra,
}

impl Intensity {
    pub fn parse(s: &str) -> Self {
        match s {
            "ultra" => Intensity::Ultra,
            "full" => Intensity::Full,
            _ => Intensity::Lite,
        }
    }
    pub fn rank(self) -> i32 {
        match self {
            Intensity::Lite => 0,
            Intensity::Full => 1,
            Intensity::Ultra => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctx {
    All,
    User,
    Assistant,
}

/// Replacement semantics (parity: static "" / lookup-map / keep original).
#[derive(Debug, Clone, Copy)]
pub enum Action {
    /// drop the match
    Remove,
    /// replace with a fixed string
    Static(&'static str),
    /// lookup by lowercased trimmed match; keep the original when missing
    Map(&'static [(&'static str, &'static str)]),
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: &'static str,
    pub pattern: &'static str,
    pub anchored_line: bool,
    pub action: Action,
    pub context: Ctx,
    pub min_intensity: Intensity,
}

use Ctx::{All, Assistant, User};
use Intensity::{Full as IFull, Lite as ILite, Ultra as IUltra};

/// The full rule table (parity: `CAVEMAN_RULES`, 34 rules). Patterns mirror the
/// original regex sources; maps mirror the original lookup functions.
pub static RULES: &[Rule] = &[
    // ── Category 1: Filler removal ────────────────────────────────────────
    rule_all_map("redundant_phrasing",
        r"\b(make sure to|be sure to|due to the fact that|the reason is because|it is important to|you should|remember to)\b\s*",
        &[("make sure to", "ensure "), ("be sure to", "ensure "), ("due to the fact that", "because "), ("the reason is because", "because "), ("it is important to", ""), ("you should", ""), ("remember to", "")],
        All, IFull),
    rule_plain("pleasantries",
        r"(?<!make\s)(?<!be\s)\b(i'?d be happy to|i would be happy to|i'?d be glad to|i would be glad to|glad to help|happy to|thank you|thanks|no problem|you'?re welcome|absolutely|certainly|of course|sure)\b[,.!?\s]*",
        Action::Remove, All, ILite),
    rule_plain("polite_framing",
        r"\b(please|kindly|could you please|would you please|can you please|i would like you to|i want you to|i need you to)\b\s*",
        Action::Remove, All, ILite),
    rule_plain("hedging",
        r"\b(it seems like|it appears that|i think that|i believe that|probably|possibly|maybe it)\b\s*",
        Action::Remove, All, ILite),
    rule_all_map("verbose_instructions",
        r"\b(provide a detailed explanation of|give me a comprehensive explanation of|write an in-depth explanation of|create a thorough explanation of|provide a detailed|give me a comprehensive|write an in-depth|create a thorough|explain in detail)\b",
        &[("provide a detailed explanation of", "explain "), ("give me a comprehensive explanation of", "explain "), ("write an in-depth explanation of", "explain "), ("create a thorough explanation of", "explain "), ("provide a detailed", "provide "), ("give me a comprehensive", "give "), ("write an in-depth", "write "), ("create a thorough", "create "), ("explain in detail", "explain ")],
        All, ILite),
    rule_plain("filler_adverbs",
        r"(?<![a-z])\b(basically|essentially|actually|literally|simply|currently)\b\s*",
        Action::Remove, All, ILite),
    rule_plain("articles",
        r"\b(An|an|A|a|The|the)\s+(?=[a-z])",
        Action::Remove, All, IFull),
    rule_anchored("filler_phrases",
        r"^(i want to|i need to|i'?d like to|i'?m looking for)\b\s*",
        Action::Remove, User, ILite),
    rule_anchored("redundant_openers",
        r"^(hi there|hello|good morning|hey)\s*[,.!?\s]?\s*",
        Action::Remove, User, ILite),
    rule_plain("verbose_requests",
        r"\b(i was wondering if you could|would it be possible to)\b\s*",
        Action::Remove, User, ILite),
    rule_anchored("leader_phrases",
        r"^(i'?ll|i will|i can|i'?d|let me|you can|we will|we can|let'?s)\s+(?=[a-z])",
        Action::Remove, All, IFull),
    rule_anchored("self_reference",
        r"^(i am trying to|i am working on|i have been)\b\s*",
        Action::Remove, User, ILite),
    rule_plain("excessive_gratitude",
        r"\b(thank you so much|thanks in advance|i really appreciate)\b[,.!?\s]*",
        Action::Remove, All, ILite),
    rule_plain("qualifier_removal",
        r"\b(a bit|a little|somewhat|kind of|sort of)\b\s*",
        Action::Remove, All, ILite),
    // ── Category 2: Context condensation ──────────────────────────────────
    rule_plain("compound_collapse",
        r"\band any potential\b",
        Action::Remove, All, IFull),
    rule_all_map("explanatory_prefix",
        r"\b(the function appears to be handling|the code seems to|the class is|this module is)\b",
        &[("the function appears to be handling", "Function:"), ("the code seems to", "Code:"), ("the class is", "Class:"), ("this module is", "Module:")],
        All, ILite),
    rule_plain("question_to_directive",
        r"\b(can you explain why|could you show me how|would you tell me|can you tell me)\b\s*",
        Action::Map(&[("can you explain why", "Explain why "), ("could you show me how", "Show how "), ("would you tell me", "Tell me "), ("can you tell me", "Tell me ")]),
        User, ILite),
    rule_static("context_setup",
        r"\b(i have the following code|here is my code|below is the code)\b\s*[:.]?\s*",
        "Code:", User, ILite),
    rule_static("intent_clarification",
        r"\b(what i'?m trying to do is|my objective is to|what i need is|i'?m aiming to)\b\s*",
        "Goal:", User, ILite),
    rule_plain("background_removal",
        r"\b(as you may know,?\s*|as we discussed earlier,?\s*)",
        Action::Remove, All, ILite),
    rule_anchored("meta_commentary",
        r"^(note that|keep in mind that|remember that)\b\s*",
        Action::Remove, All, ILite),
    rule_all_map("purpose_statement",
        r"\b(for the purpose of|with the goal of|in an effort to|for every)\b",
        &[("for the purpose of", "for"), ("with the goal of", "to"), ("in an effort to", "to"), ("for every", "per")],
        All, ILite),
    // ── Category 3: Structural compression ────────────────────────────────
    rule_static("list_conjunction",
        r",\s*and also\s+|,\s*as well as\s+",
        ", ", All, IFull),
    rule_static("purpose_phrases",
        r"\b(in order to|so as to)\b\s*",
        "to ", All, ILite),
    rule_all_map("redundant_quantifiers",
        r"\b(each and every single|each and every|any and all)\b",
        &[("each and every single", "each"), ("each and every", "each"), ("any and all", "all")],
        All, IFull),
    rule_static("verbose_connectors",
        r"\b(furthermore|additionally|moreover|in addition)\b\s*",
        "also ", All, ILite),
    rule_anchored("transition_removal",
        r"^(on the other hand,?\s*|in contrast,?\s*|however,?\s*)",
        Action::Remove, All, ILite),
    rule_plain("emphasis_removal",
        r"\b(very|really|extremely|highly|quite)\s+(?=[a-z])",
        Action::Remove, All, ILite),
    rule_all_map("passive_voice",
        r"\b(is being used|is being called|is being generated|was created|was generated|was implemented)\b",
        &[("is being used", "uses"), ("is being called", "calls"), ("is being generated", "generated"), ("was created", "created"), ("was generated", "generated"), ("was implemented", "implemented")],
        All, IFull),
    // ── Category 4: Multi-turn dedup ───────────────────────────────────────
    rule_static("repeated_context",
        r"\b(as we discussed earlier|as mentioned before|as previously stated|as i said before)\b[,.]?\s*",
        "See above. ", All, ILite),
    rule_static("repeated_question",
        r"\b(same question as before|i asked this earlier|this is the same question)\b[,.]?\s*",
        "[same question] ", User, ILite),
    rule_static("reestablished_context",
        r"\b(going back to the code above|referring back to|returning to)\b\s*",
        "Re: ", All, ILite),
    rule_static("summary_replacement",
        r"\b(to summarize what we'?ve discussed|in summary of our conversation|to recap)\b[,.]?\s*",
        "Summary: ", Assistant, ILite),
    // ── Category 5: Ultra abbreviations ────────────────────────────────────
    rule_all_map("ultra_abbreviations",
        r"\b(database|configuration|function|request|response|implementation|authentication|authorization|application|dependency|dependencies)\b",
        &[("database", "DB"), ("configuration", "config"), ("function", "fn"), ("request", "req"), ("response", "res"), ("implementation", "impl"), ("authentication", "auth"), ("authorization", "authz"), ("application", "app"), ("dependency", "dep"), ("dependencies", "deps")],
        All, IUltra),
];

const fn rule_plain(name: &'static str, pattern: &'static str, action: Action, context: Ctx, min_intensity: Intensity) -> Rule {
    Rule { name, pattern, anchored_line: false, action, context, min_intensity }
}

const fn rule_anchored(name: &'static str, pattern: &'static str, action: Action, context: Ctx, min_intensity: Intensity) -> Rule {
    Rule { name, pattern, anchored_line: true, action, context, min_intensity }
}

const fn rule_static(name: &'static str, pattern: &'static str, replacement: &'static str, context: Ctx, min_intensity: Intensity) -> Rule {
    Rule { name, pattern, anchored_line: false, action: Action::Static(replacement), context, min_intensity }
}

const fn rule_all_map(name: &'static str, pattern: &'static str, pairs: &'static [(&'static str, &'static str)], context: Ctx, min_intensity: Intensity) -> Rule {
    Rule { name, pattern, anchored_line: false, action: Action::Map(pairs), context, min_intensity }
}

static COMPILED: Lazy<Vec<(usize, Regex)>> = Lazy::new(|| {
    RULES
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let flags = if r.anchored_line { "(?im)" } else { "(?i)" };
            Some((i, Regex::new(&format!("{}{}", flags, r.pattern)).ok()?))
        })
        .collect()
});

/// Keyword prefilter (parity: `shouldAttemptRule`).
fn rule_keywords(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "redundant_phrasing" => &["make sure", "be sure", "due to the fact", "the reason is", "it is important", "you should", "remember to"],
        "pleasantries" => &["sure", "certainly", "of course", "happy to", "thanks", "thank you", "glad to help", "glad to", "no problem", "welcome", "absolutely"],
        "polite_framing" => &["please", "kindly", "could you please", "would you please", "can you please", "i would like you", "i want you", "i need you"],
        "hedging" => &["it seems like", "it appears that", "i think that", "i believe that", "probably", "possibly", "maybe it"],
        "verbose_instructions" => &["provide a detailed", "give me a comprehensive", "write an in-depth", "create a thorough", "explain in detail"],
        "filler_adverbs" => &["basically", "essentially", "actually", "literally", "simply", "currently"],
        "redundant_openers" => &["hi there", "hello", "good morning", "hey"],
        "verbose_requests" => &["i was wondering", "would it be possible"],
        "leader_phrases" => &["i'll", "i will", "i can", "i'd", "let me", "you can", "we will", "we can", "let's"],
        "self_reference" => &["i am trying to", "i am working on", "i have been"],
        "excessive_gratitude" => &["thank you so much", "thanks in advance", "i really appreciate"],
        "qualifier_removal" => &["a bit", "a little", "somewhat", "kind of", "sort of"],
        "compound_collapse" => &["and any potential"],
        "explanatory_prefix" => &["the function appears to be handling", "the code seems to", "the class is", "this module is"],
        "question_to_directive" => &["can you explain why", "could you show me how", "would you tell me", "can you tell me"],
        "context_setup" => &["i have the following code", "here is my code", "below is the code"],
        "intent_clarification" => &["what i'm trying to do", "my objective is to", "what i need is", "i'm aiming to"],
        "background_removal" => &["as you may know", "as we discussed earlier"],
        "meta_commentary" => &["note that", "keep in mind", "remember that"],
        "purpose_statement" => &["for the purpose of", "with the goal of", "in an effort to", "for every"],
        "list_conjunction" => &["and also", "as well as"],
        "purpose_phrases" => &["in order to", "so as to"],
        "redundant_quantifiers" => &["each and every", "any and all"],
        "verbose_connectors" => &["furthermore", "additionally", "moreover", "in addition"],
        "transition_removal" => &["on the other hand", "in contrast", "however"],
        "emphasis_removal" => &["very", "really", "extremely", "highly", "quite"],
        "passive_voice" => &["is being used", "is being called", "is being generated", "was created", "was generated", "was implemented"],
        "repeated_context" => &["as we discussed earlier", "as mentioned before", "as previously stated", "as i said before"],
        "repeated_question" => &["same question as before", "i asked this earlier", "this is the same question"],
        "reestablished_context" => &["going back to the code above", "referring back to", "returning to"],
        "summary_replacement" => &["to summarize", "in summary of our conversation", "to recap"],
        "ultra_abbreviations" => &["database", "configuration", "function", "request", "response", "implementation", "authentication", "authorization", "application", "dependency"],
        "articles" => &[" a ", " an ", " the "],
        "filler_phrases" => &["i want to", "i need to", "i'd like to", "i'm looking for"],
        _ => return None,
    })
}

fn should_attempt_rule(name: &str, lower_text: &str) -> bool {
    match rule_keywords(name) {
        None => true,
        Some(kws) => {
            let padded = format!(" {} ", lower_text);
            kws.iter().any(|k| padded.contains(k))
        }
    }
}

/// Apply one rule with replacement semantics (parity: `applyRulesToText`).
fn apply_one_rule(text: &str, re: &Regex, rule: &Rule) -> (String, bool) {
    let mut out = String::new();
    let mut last = 0usize;
    let mut changed = false;
    while let Ok(Some(m)) = re.find_from_pos(text, last) {
        let (s, e) = (m.start(), m.end());
        if s > last {
            out.push_str(&text[last..s]);
        }
        let matched = &text[s..e];
        let replacement = match rule.action {
            Action::Remove => String::new(),
            Action::Static(s) => s.to_string(),
            Action::Map(pairs) => {
                let key = matched.trim().to_lowercase();
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
                    .unwrap_or_else(|| matched.to_string())
            }
        };
        out.push_str(&replacement);
        if replacement != matched {
            changed = true;
        }
        if e == s {
            break;
        }
        last = e;
    }
    out.push_str(&text[last.min(text.len())..]);
    (out, changed)
}

/// Artifact cleanup (parity: `cleanupArtifacts`).
pub fn cleanup_artifacts(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let re_spaces = Regex::new(r"[ \t]{2,}").unwrap();
    let re_before_punct = Regex::new(r"[ \t]+([,.;:!?])").unwrap();
    let re_repeat_punct = Regex::new(r"([.!?]){2,}").unwrap();
    let mut out = re_spaces.replace_all(text, " ").to_string();
    out = re_before_punct.replace_all(&out, "$1").to_string();
    out = re_repeat_punct.replace_all(&out, "$1").to_string();
    let mut per_line = String::new();
    for line in out.split('\n') {
        per_line.push_str(line.trim_end());
        per_line.push('\n');
    }
    let trimmed = per_line.trim_matches('\n').to_string();
    let re_nl = Regex::new(r"\n{3,}").unwrap();
    re_nl.replace_all(&trimmed, "\n\n").to_string()
}

/// Sentence recapitalization (parity: `recapitalizeSentences`).
pub fn recapitalize_sentences(text: &str) -> String {
    let re = Regex::new(r"(^|[.!?][ \t]|\n[ \t]*)([a-z])").unwrap();
    re.replace_all(text, |caps: &fancy_regex::Captures| {
        let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let ch = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        format!("{prefix}{}", ch.to_uppercase())
    })
    .to_string()
}

/// Skip prose normalization when the text is dominated by raw code (parity:
/// `isCodeDominantText` — conservative, biases toward less compression).
pub fn is_code_dominant_text(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 3 {
        return false;
    }
    let code_like = lines
        .iter()
        .filter(|l| is_code_like_line(l))
        .count();
    code_like as f64 / lines.len() as f64 >= 0.3
}

fn is_code_like_line(line: &str) -> bool {
    let l = line.trim_start();
    [
        "import ", "export ", "function ", "class ", "const ", "let ", "var ", "return ",
        "if(", "if (", "for(", "for (", "while(", "while (",
    ]
    .iter()
    .any(|p| l.starts_with(p))
}

/// Compression statistics (parity: CompressionStats essentials).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CompressionStats {
    pub original_tokens: i64,
    pub compressed_tokens: i64,
    pub savings_percent: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub techniques_used: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules_applied: Vec<String>,
}

impl CompressionStats {
    pub fn compute(original: i64, compressed: i64, mut techniques: Vec<String>, rules: Vec<String>) -> Self {
        let savings = if original > 0 {
            ((original - compressed) as f64 / original as f64 * 10000.0).round() / 100.0
        } else {
            0.0
        };
        techniques.sort();
        techniques.dedup();
        let mut uniq_rules: Vec<String> = rules;
        uniq_rules.sort();
        uniq_rules.dedup();
        Self {
            original_tokens: original,
            compressed_tokens: compressed,
            savings_percent: savings,
            techniques_used: techniques,
            rules_applied: uniq_rules,
        }
    }
}

/// Caveman config (parity: `DEFAULT_CAVEMAN_CONFIG`).
#[derive(Debug, Clone)]
pub struct CavemanConfig {
    pub intensity: Intensity,
    pub compress_roles: Vec<String>,
    pub skip_rules: Vec<String>,
    pub min_message_length: usize,
}

impl Default for CavemanConfig {
    fn default() -> Self {
        Self {
            intensity: Intensity::Lite,
            compress_roles: vec!["user".to_string()],
            skip_rules: Vec::new(),
            min_message_length: 50,
        }
    }
}

fn role_selected(config: &CavemanConfig, role: &str) -> bool {
    config.compress_roles.iter().any(|r| r == role)
}

fn compress_text_for_role(text: &str, role: &str, config: &CavemanConfig, applied: &mut Vec<String>) -> String {
    if text.len() < config.min_message_length {
        return text.to_string();
    }
    let should_preserve = has_protected_structure(text);
    let (working, blocks) = if should_preserve {
        extract_preserved_blocks(text, &[])
    } else {
        (text.to_string(), Vec::new())
    };

    let lower = working.to_lowercase();
    let mut text_out = working;
    for (idx, r) in RULES.iter().enumerate() {
        if !config.skip_rules.is_empty() && config.skip_rules.iter().any(|s| *s == r.name) {
            continue;
        }
        let ctx_ok = r.context == Ctx::All
            || (role == "user" && r.context == Ctx::User)
            || (role == "assistant" && r.context == Ctx::Assistant);
        if !ctx_ok {
            continue;
        }
        if config.intensity.rank() < r.min_intensity.rank() {
            continue;
        }
        if !should_attempt_rule(r.name, &lower) {
            continue;
        }
        if let Some((_, re)) = COMPILED.iter().find(|(i, _)| *i == idx) {
            let (next, changed) = apply_one_rule(&text_out, re, r);
            if changed {
                applied.push(r.name.to_string());
            }
            text_out = next;
        }
    }

    let normalized = if is_code_dominant_text(&text_out) {
        text_out
    } else {
        recapitalize_sentences(&cleanup_artifacts(&text_out))
    };
    if blocks.is_empty() {
        normalized
    } else {
        cleanup_artifacts(&restore_preserved_blocks(&normalized, &blocks))
    }
}

/// Compress a text part generically (used by other engines via caveman rules).
pub fn apply_rules_to_text(text: &str, role: &str, config: &CavemanConfig, applied: &mut Vec<String>) -> String {
    compress_text_for_role(text, role, config, applied)
}

/// Apply the caveman rule engine to an openai-shaped `messages` array
/// (parity: `cavemanCompress`). Returns the new messages array + stats.
pub fn caveman_compress(messages: &Value, config: &CavemanConfig) -> (Value, CompressionStats) {
    let empty: Vec<Value> = Vec::new();
    let arr = messages.as_array().unwrap_or(&empty);
    let mut total_original = 0i64;
    let mut total_compressed = 0i64;
    let mut all_applied: Vec<String> = Vec::new();

    let new_messages: Vec<Value> = arr
        .iter()
        .map(|msg| {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            let selected_role = role_selected(config, role);
            let mut msg = msg.clone();

            match msg.get("content") {
                Some(Value::String(s)) => {
                    total_original += estimate_tokens(s);
                    if selected_role && s.len() >= config.min_message_length {
                        let compressed = compress_text_for_role(s, role, config, &mut all_applied);
                        msg["content"] = Value::String(compressed);
                        total_compressed += estimate_tokens(msg["content"].as_str().unwrap_or(""));
                    } else {
                        total_compressed += estimate_tokens(s);
                    }
                }
                Some(Value::Array(_)) => {
                    let joined = stringify_content(&msg["content"]);
                    total_original += estimate_tokens(&joined);
                    if joined.len() >= config.min_message_length && selected_role {
                        if let Some(parts) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                            for p in parts.iter_mut() {
                                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                                        let compressed = compress_text_for_role(text, role, config, &mut all_applied);
                                        p["text"] = Value::String(compressed);
                                    }
                                }
                            }
                        }
                        total_compressed += estimate_tokens(&stringify_content(&msg["content"]));
                    } else {
                        total_compressed += estimate_tokens(&joined);
                    }
                }
                _ => {
                    // non-text content (images etc.) passes through untouched
                }
            }
            msg
        })
        .collect();

    let techniques = if all_applied.is_empty() {
        Vec::new()
    } else {
        vec!["caveman-rules".to_string()]
    };
    let stats = CompressionStats::compute(total_original, total_compressed, techniques, all_applied);
    (Value::Array(new_messages), stats)
}

fn stringify_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> CavemanConfig {
        CavemanConfig::default()
    }

    #[test]
    fn pleasantries_removed_lite_user() {
        let msgs = json!([{"role": "user", "content": "Hello, sure, thanks — please fix the login bug in the auth module. The session token expires after five minutes and the refresh flow never gets triggered, which breaks every automated test we run nightly."}]);
        let (out, stats) = caveman_compress(&msgs, &CavemanConfig::default());
        let text = out[0]["content"].as_str().unwrap();
        assert!(!text.contains("thanks"));
        assert!(!text.contains("please"));
        assert!(stats.compressed_tokens < stats.original_tokens);
        assert!(stats.rules_applied.iter().any(|r| r == "pleasantries" || r == "polite_framing"));
    }

    #[test]
    fn articles_removed_only_at_full() {
        let msgs = json!([{"role": "user", "content": "the renderer should draw a label under the panel and the icon must scale correctly on every dpi setting we support today"}]);
        let (out, _) = caveman_compress(&msgs, &CavemanConfig { intensity: Intensity::Full, compress_roles: vec!["user".into()], ..Default::default() });
        let text = out[0]["content"].as_str().unwrap();
        assert!(!text.contains(" the "), "articles removed at full: {text}");

        let (out2, _) = caveman_compress(&msgs, &CavemanConfig::default());
        assert!(out2[0]["content"].as_str().unwrap().contains(" the "));
    }

    #[test]
    fn ultra_abbreviations() {
        let msgs = json!([{"role": "user", "content": "the database configuration and the authentication implementation and the dependency injection need review, and the request response cycle too. this function must handle the application startup before anything else runs."}]);
        let (out, stats) = caveman_compress(
            &msgs,
            &CavemanConfig { intensity: Intensity::Ultra, compress_roles: vec!["user".into()], ..Default::default() },
        );
        let text = out[0]["content"].as_str().unwrap();
        assert!(text.contains("DB"), "ultra abbreviations applied: {text}");
        assert!(text.contains("fn"));
        assert!(stats.rules_applied.iter().any(|r| r == "ultra_abbreviations"));
    }

    #[test]
    fn system_role_skipped_by_default() {
        let msgs = json!([
            {"role": "system", "content": "thanks, sure — please remember to check the thing, the system prompt stays intact and unchanged by the caveman rules at any intensity level at all."},
            {"role": "user", "content": "thanks, sure — please fix the bug quickly, the request message gets compressed by the rules that are enabled for the user role by default at runtime."}
        ]);
        let (out, _) = caveman_compress(&msgs, &CavemanConfig::default());
        assert!(out[0]["content"].as_str().unwrap().contains("thanks"));
        assert!(!out[1]["content"].as_str().unwrap().contains("thanks"));
    }

    #[test]
    fn short_messages_untouched() {
        let msgs = json!([{"role": "user", "content": "thanks, please fix it"}]);
        let (out, _) = caveman_compress(&msgs, &CavemanConfig::default());
        assert_eq!(out[0]["content"].as_str().unwrap(), "thanks, please fix it");
    }

    #[test]
    fn code_blocks_preserved() {
        let msgs = json!([{"role": "user", "content": "thanks a lot, please review this snippet:\n```rust\nfn main() { let a = 1; }\n```\nand also the module the function the request implements the thing for the purpose of the test, it is important to note that this is very long filler text around the code block for the caveman rules to trigger properly and the article removal at full intensity would apply here."}]);
        let cfg = CavemanConfig { intensity: Intensity::Full, compress_roles: vec!["user".into()], ..Default::default() };
        let (out, _) = caveman_compress(&msgs, &cfg);
        let text = out[0]["content"].as_str().unwrap();
        assert!(text.contains("```rust\nfn main() { let a = 1; }\n```"), "fence intact: {text}");
    }

    #[test]
    fn assistant_context_rules() {
        let msgs = json!([{"role": "assistant", "content": "To recap the findings: we discussed the parser rewrite and the migration plan and decided to proceed with the staged rollout of the new parser across the workspace next week."}]);
        let (out, stats) = caveman_compress(
            &msgs,
            &CavemanConfig { compress_roles: vec!["assistant".into()], ..Default::default() },
        );
        let text = out[0]["content"].as_str().unwrap();
        assert!(text.contains("Summary:"), "summary rule applied: {text}");
        assert!(stats.rules_applied.iter().any(|r| r == "summary_replacement"));
    }

    #[test]
    fn stats_shape() {
        let msgs = json!([{"role": "user", "content": "thanks thanks thanks, please please fix the thing now, the build is broken and every test fails constantly all the time and it is really quite extremely annoying to deal with this every single day without any end in sight whatsoever."}]);
        let (_, stats) = caveman_compress(&msgs, &CavemanConfig::default());
        assert!(stats.original_tokens > 0);
        assert!(stats.savings_percent > 0.0);
        assert!(!stats.rules_applied.is_empty());
    }
}
