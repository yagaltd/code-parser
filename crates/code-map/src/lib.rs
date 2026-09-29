//! code-map — query layer over code-parser JSONL maps.
//!
//! A `code-map.jsonl` is the output of `code-parser parse-repo . --jsonl`
//! (one `FileParseIR` per line). code-map is deliberately stateless: every
//! command loads the map, answers, exits. Refresh regenerates the snapshot
//! atomically; graph commands resolve call edges already present in the IR;
//! search ranks card/symbol text (IDF + fuzzy); `typesafe gate` (feature-
//! gated) filters a shortlist with typed judgments.

pub mod graph;
pub mod lexicon;
pub mod refresh;
pub mod search;
#[cfg(feature = "typesafe")]
pub mod typesafe;

use std::path::Path;

use anyhow::{Context, Result};
use code_parser_ir::FileParseIR;

/// Load every IR from a JSONL map (blank lines skipped).
pub fn load_irs(path: &Path) -> Result<Vec<FileParseIR>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read map {}", path.display()))?;
    let mut irs = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let ir: FileParseIR = serde_json::from_str(line)
            .with_context(|| format!("map line {} is not a FileParseIR", i + 1))?;
        irs.push(ir);
    }
    Ok(irs)
}
