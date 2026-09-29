//! `code-map refresh` — regenerate the JSONL snapshot atomically.
//!
//! Runs `parse_repo` in-process (hash cache skips unchanged files on warm
//! runs), writes to `<out>.tmp`, then renames over the destination. A crash
//! mid-write leaves the previous map intact; readers never see a partial
//! file.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use code_parser_core::language::Language;

/// Parse `dir` and write one FileParseIR per line to `out` (atomic swap).
/// Returns the number of files written. Per-file parse errors go to stderr
/// and are non-fatal (mirrors `code-parser parse-repo`).
pub fn run(dir: &Path, out: &Path, languages: Option<Vec<Language>>) -> Result<usize> {
    let results = code_parser_core::parse_repo(dir, languages)
        .with_context(|| format!("Failed to parse repo {}", dir.display()))?;

    let tmp: PathBuf = out.with_extension("jsonl.tmp");
    if let Some(parent) = tmp.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
    }
    let mut count = 0usize;
    {
        let file = std::fs::File::create(&tmp)
            .with_context(|| format!("Failed to create {}", tmp.display()))?;
        let mut w = std::io::BufWriter::new(file);
        for r in &results {
            for e in &r.errors {
                eprintln!("error: {e}");
            }
            serde_json::to_writer(&mut w, &r.ir).context("Failed to serialize IR")?;
            w.write_all(b"\n").context("Failed to write line")?;
            count += 1;
        }
        w.flush().context("Failed to flush map")?;
    }
    std::fs::rename(&tmp, out)
        .with_context(|| format!("Failed to swap {} into place", tmp.display()))?;
    Ok(count)
}

/// Parse a comma-separated language list ("rust,typescript,py").
/// Mirrors the CLI's accepted names.
pub fn parse_languages(spec: &str) -> Option<Vec<Language>> {
    use Language::*;
    let langs: Vec<Language> = spec
        .split(',')
        .filter_map(|s| match s.trim().to_lowercase().as_str() {
            "rust" => Some(Rust),
            "typescript" | "ts" => Some(TypeScript),
            "javascript" | "js" => Some(JavaScript),
            "python" | "py" => Some(Python),
            _ => None,
        })
        .collect();
    (!langs.is_empty()).then_some(langs)
}
