//! Probes for the review findings. Each prints a PASS/FAIL verdict so the
//! claim is checked against the *production* code path, not against prose.
//!
//! Run: cargo test --test lrc_review_probe -- --nocapture

use omniroute_rust::compression::lrc::filter::{filter_tool_output, FilterCfg};
use omniroute_rust::compression::lrc::gates::{extract_protected, is_marked};

/// C1: a JSON tool result must stay parseable after compression.
#[test]
fn c1_json_tool_result_stays_parseable() {
    // Two adjacent identical JSON documents: the deduper folds them and
    // appends a "[x2]" suffix, which is not valid JSON.
    let doc = format!(
        r#"{{"id":"key_a8b14c17","name":"dsh","key":"sk-or-ef3105371166b873bc1b2f13a6b7c3ec","model_access_mode":"all","allowed_models":[{}],"padding":"{}"}}"#,
        (0..12).map(|i| format!("\"m{i}\"" )).collect::<Vec<_>>().join(","),
        "x".repeat(400)
    );
    let text = format!("{doc}\n{doc}\n");
    let cfg = FilterCfg::default();
    let out = filter_tool_output(&text, Some("bash"), &cfg, None);
    match out {
        None => println!("C1: no filtering applied (text below threshold) — vacuous"),
        Some(f) => {
            let parses = serde_json::from_str::<serde_json::Value>(f.text.trim()).is_ok();
            println!("C1: filtered={} parses_as_json={}", f.text.len() < text.len(), parses);
            println!("C1 body: {:?}", &f.text[..f.text.len().min(200)]);
            assert!(parses, "C1: JSON tool result became unparseable");
        }
    }
}

/// C2: a diff deletion line that is only dashes must not be dropped.
#[test]
fn c2_pure_dash_diff_line_survives() {
    // A bare hunk: no `diff --git` header, so the verbatim-diff guard does
    // not fire and the block goes through the ordinary filter path.
    let diff = "\
@@ -1,3 +1,3 @@
 context line
-  ---------------
+  ===============
 unchanged
more context to pass the block threshold so the filter actually runs here
and some more prose so that the block is large enough to be compressed";
    let cfg = FilterCfg::default();
    // Stage 1 (lossless noise removal) runs for every block, including
    // diffs — that is where the progress-bar filter can eat a hunk line.
    let (after_progress, changed) = omniroute_rust::compression::lrc::filter::drop_progress(diff);
    println!("C2: stage1_changed={changed} dash_line_survived={}", after_progress.contains("-  ---------------"));
    assert!(after_progress.contains("-  ---------------"), "C2: progress filter ate a diff deletion line");
    let out = filter_tool_output(diff, Some("bash"), &cfg, None);
    if let Some(f) = out {
        let kept = f.text.contains("-  ---------------");
        println!("C2: kept_removed_dash_line={kept}");
        println!("C2 body: {:?}", f.text);
        assert!(kept, "C2: a diff deletion line was dropped as a progress bar");
    } else {
        println!("C2: not filtered (verbatim diff path)");
    }
}

/// C3: a real error line must survive even when the protection budget is
/// exhausted by benign lines.
#[test]
fn c3_error_line_survives_budget_exhaustion() {
    let mut text = String::new();
    // 60 benign "failed"-ish lines that mention the marker words but are
    // ordinary progress prose.
    for i in 0..60 {
        text.push_str(&format!("test suite::mod{} ... ok (0 failed)\n", i));
    }
    text.push_str("error[E0432]: unresolved import `missing::thing`\n");
    text.push_str("  --> src/lib.rs:12:5\n");
    let protected = extract_protected(&text, 40, 4000);
    let has_real_error = protected.lines.iter().any(|l| l.contains("E0432"));
    let has_location = protected.lines.iter().any(|l| l.contains("src/lib.rs:12:5"));
    println!("C3: protected_lines={} contains_real_error={has_real_error} has_location={has_location}",
        protected.lines.len());
    assert!(has_real_error, "C3: the real error line was squeezed out of the protection budget");
    assert!(has_location, "C3: the --> file:line location was dropped");
}

/// M3: the idempotency marker must be recognised by `is_marked`.
#[test]
fn m3_marker_written_by_filter_is_recognised() {
    let text = "line\n[omni-elided 10 lines · 320 chars · sha8=deadbeef]\n";
    println!("M3: is_marked={}", is_marked(text));
    assert!(is_marked(text), "M3: the filter's own marker is not recognised → no idempotency");
}
