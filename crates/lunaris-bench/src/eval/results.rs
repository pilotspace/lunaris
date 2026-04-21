//! Plan 05-06 D-20 — `eval-results.json` writer + markdown summary printer.
//!
//! Mirrors `crates/lunaris-bench/src/corpus.rs::CorpusFingerprint` write path
//! verbatim per Shared Pattern 8 — `serde_json::to_writer_pretty` to a
//! `std::fs::File`. Markdown summary printed to stdout for human review per
//! CONTEXT.md D-20.

#![forbid(unsafe_code)]

use std::path::Path;

use crate::eval::EvalRow;

/// Write the rows to `path` as pretty-printed JSON. CI greps the resulting
/// file for `"status":"FAIL"` to decide merge gating.
pub fn write_json(path: &Path, rows: &[EvalRow]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok();
    }
    let f = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(f, rows)?;
    Ok(())
}

/// Print a human-readable markdown table to stdout summarising the eval
/// results. CI logs render the table inline so operators can triage without
/// downloading the JSON artifact.
pub fn print_markdown_summary(rows: &[EvalRow]) {
    println!();
    println!("# Eval Gauntlet Results");
    println!();
    println!("| Harness | Metric | Value | Threshold | Status | Duration |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for row in rows {
        let dur = format!("{} ms", row.duration_ms);
        // Status column intentionally plain text (no emoji) to stay readable
        // in JSON-mode CI logs and consistent with CLAUDE.md no-emoji norm.
        println!(
            "| {} | {} | {:.3} | {:.3} | {} | {} |",
            row.harness, row.metric, row.value, row.threshold, row.status, dur
        );
    }
    println!();
    let pass = rows.iter().filter(|r| r.status == "PASS").count();
    let fail = rows.iter().filter(|r| r.status == "FAIL").count();
    let skip = rows.iter().filter(|r| r.status == "SKIPPED").count();
    println!("Summary: {pass} PASS, {fail} FAIL, {skip} SKIPPED");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_json_round_trips() {
        let tmp = std::env::temp_dir().join(format!("lunaris-eval-test-{}.json", std::process::id()));
        let rows = vec![
            EvalRow::pass("h", "m", 1.0, 2.0, 100),
            EvalRow::skipped("h2", "m2", 3.0, "no env"),
        ];
        write_json(&tmp, &rows).unwrap();
        let bytes = std::fs::read(&tmp).unwrap();
        let parsed: Vec<EvalRow> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].status, "PASS");
        assert_eq!(parsed[1].status, "SKIPPED");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn write_json_creates_parent_dir() {
        let dir = std::env::temp_dir().join(format!(
            "lunaris-eval-mkdir-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let path = dir.join("nested/eval.json");
        let rows = vec![EvalRow::pass("h", "m", 1.0, 2.0, 100)];
        write_json(&path, &rows).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
