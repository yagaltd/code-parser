//! code-parser-core — tree-sitter engine.
//!
//! Parses source files into `FileParseIR` using tree-sitter grammars.
//! Provides per-file parsing, repository-wide parsing with cross-file
//! resolution, and optional file watching.

use std::path::Path;

use anyhow::Context;
use code_parser_ir::{DiagnosticIR, DiagnosticSeverity, FileParseIR};

pub mod cache;
pub mod extractors;
pub mod file_collect;
pub mod hash;
pub mod imports;
pub mod language;
pub mod metrics;
pub mod parser;
pub mod repo_state;
pub mod resolve;
pub mod watcher;

#[cfg(feature = "javascript")]
use extractors::javascript::JavaScriptExtractor;
#[cfg(feature = "python")]
use extractors::python::PythonExtractor;
#[cfg(feature = "rust")]
use extractors::rust::RustExtractor;
#[cfg(feature = "typescript")]
use extractors::typescript::TypeScriptExtractor;
use extractors::LanguageExtractor;
use language::Language;

pub use cache::HashCache;
pub use repo_state::{ChangeEvent, FileChange, RepoState};

// ── Per-file extraction caps (borrow 2 hardening) ───────────────────────

/// Upper bound on symbols extracted per file. Extraction runs single-threaded
/// and is bounded by these caps, so no file — however hostile — can exceed
/// bounded memory/time; overflows become visible truncation Warnings.
/// Matches codegraph-rs's 4k symbols / 8k relations per-file caps.
pub const MAX_SYMBOLS_PER_FILE: usize = 4096;

/// Upper bound on call sites extracted per file.
pub const MAX_CALLS_PER_FILE: usize = 8192;

/// Upper bound on imports extracted per file.
pub const MAX_IMPORTS_PER_FILE: usize = 2048;

/// Truncate `items` to `cap`; returns how many were dropped (0 = no-op).
fn truncate_to_cap<T>(items: &mut Vec<T>, cap: usize) -> usize {
    let over = items.len().saturating_sub(cap);
    if over > 0 {
        items.truncate(cap);
    }
    over
}

/// Push a cap Warning diagnostic, keeping the total diagnostics vector within
/// [`extractors::utils::MAX_DIAGNOSTICS_PER_FILE`]: when full, the last parse
/// diagnostic yields its slot (cap Warnings are more actionable than the
/// tail of a bounded parse-issue list).
fn push_cap_warning(diags: &mut Vec<DiagnosticIR>, message: String) {
    if diags.len() >= extractors::utils::MAX_DIAGNOSTICS_PER_FILE {
        diags.pop();
    }
    diags.push(DiagnosticIR {
        severity: DiagnosticSeverity::Warning,
        message,
        line: None,
        byte_span: None,
    });
}

/// Apply per-file extraction caps after extract, before cards: truncates the
/// symbol/call/import vectors past their caps and records one Warning
/// diagnostic per truncated kind. Card building and the domain graph stay
/// bounded for hostile inputs. No-op on every normal file (parity guard).
fn apply_extraction_caps(ir: &mut FileParseIR) {
    let dropped_symbols = truncate_to_cap(&mut ir.symbols, MAX_SYMBOLS_PER_FILE);
    if dropped_symbols > 0 {
        push_cap_warning(
            &mut ir.diagnostics,
            format!("truncated: {dropped_symbols} symbols (cap {MAX_SYMBOLS_PER_FILE})"),
        );
    }
    let dropped_calls = truncate_to_cap(&mut ir.calls, MAX_CALLS_PER_FILE);
    if dropped_calls > 0 {
        push_cap_warning(
            &mut ir.diagnostics,
            format!("truncated: {dropped_calls} calls (cap {MAX_CALLS_PER_FILE})"),
        );
    }
    let dropped_imports = truncate_to_cap(&mut ir.imports, MAX_IMPORTS_PER_FILE);
    if dropped_imports > 0 {
        push_cap_warning(
            &mut ir.diagnostics,
            format!("truncated: {dropped_imports} imports (cap {MAX_IMPORTS_PER_FILE})"),
        );
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Result of parsing a single file.
#[derive(Debug)]
pub struct ParseResult {
    pub ir: FileParseIR,
    pub errors: Vec<String>,
}

/// Parse a single file into `FileParseIR`.
///
/// Language is detected from the file extension. Returns an error
/// if the language is unsupported or parsing fails.
#[allow(unreachable_patterns)]
pub fn parse_file(path: &Path) -> Result<ParseResult, anyhow::Error> {
    let path_str = path.to_string_lossy().to_string();
    let source = std::fs::read(path).with_context(|| format!("Failed to read {path_str}"))?;
    let ir = parse_file_bytes(&path_str, &source)?;
    Ok(ParseResult {
        ir,
        errors: Vec::new(),
    })
}

/// Parse all source files in a directory, with cross-file resolution.
///
/// Collects source files via gitignore-aware walk, parses each,
/// then applies the cross-file resolution pass.
pub fn parse_repo(
    root: &Path,
    languages: Option<Vec<Language>>,
) -> Result<Vec<ParseResult>, anyhow::Error> {
    let full_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_str = full_root.to_string_lossy().to_string();
    let languages = languages.unwrap_or_else(|| {
        vec![
            Language::Rust,
            Language::TypeScript,
            Language::JavaScript,
            Language::Python,
        ]
    });

    let (paths, skipped) = file_collect::collect_source_files(&root_str, &languages)
        .context("Failed to collect source files")?;

    let mut results: Vec<ParseResult> = Vec::new();

    // Size-gate skipped files (borrow 2): each yields a diagnostic-only IR
    // with a Warning — visible to consumers, never `Err`.
    for skipped_file in &skipped {
        let mut ir = FileParseIR::empty(skipped_file.path.as_str(), "unknown");
        ir.byte_len = skipped_file.byte_len;
        ir.diagnostics.push(DiagnosticIR {
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "skipped: {} bytes > cap {} (MAX_FILE_BYTES)",
                skipped_file.byte_len,
                file_collect::MAX_FILE_BYTES
            ),
            line: None,
            byte_span: None,
        });
        results.push(ParseResult {
            ir,
            errors: Vec::new(),
        });
    }

    for rel_path in &paths {
        let full_path = full_root.join(rel_path);
        match parse_file(&full_path) {
            Ok(mut result) => {
                // Normalize path in IR to be relative.
                result.ir.path = rel_path.clone();
                results.push(result);
            }
            Err(e) => {
                // Emit a diagnostic-only IR for unparseable files.
                let ir = FileParseIR::empty(rel_path.as_str(), "unknown");
                results.push(ParseResult {
                    ir,
                    errors: vec![format!("{rel_path}: {e}")],
                });
            }
        }
    }

    // Cross-file resolve pass.
    let mut irs: Vec<FileParseIR> = results.iter().map(|r| r.ir.clone()).collect();
    resolve::resolve_cross_file(&mut irs);
    // TS/JS import specifier resolution (fix D): fills ImportIR.resolved for
    // relative paths / tsconfig aliases that map to real repo files.
    imports::resolve_import_paths(&mut irs, &full_root);
    for (i, ir) in irs.into_iter().enumerate() {
        // Rebuild cards after resolve so they reflect resolved calls.
        let (file_card, symbol_cards) = code_parser_ir::build_all_cards(&ir, &Default::default());
        let mut ir = ir;
        ir.retrieval_card = file_card;
        ir.symbol_cards = symbol_cards;
        results[i].ir = ir;
    }

    Ok(results)
}

/// Return just the IR for a given file path (used by domain_code's
/// `ingest-via-parser` feature bridge).
#[allow(unreachable_patterns)]
pub fn parse_file_bytes(path: &str, source: &[u8]) -> Result<FileParseIR, anyhow::Error> {
    let language =
        Language::from_path(path).with_context(|| format!("Unsupported file extension: {path}"))?;

    let tree =
        parser::parse(language, source).with_context(|| format!("Failed to parse {path}"))?;

    let mut ir = match language {
        #[cfg(feature = "rust")]
        Language::Rust => {
            let extractor = RustExtractor;
            extractor.extract(&tree, source, path)
        }
        #[cfg(not(feature = "rust"))]
        Language::Rust => anyhow::bail!("Rust extractor not enabled. Build with --features rust"),

        #[cfg(feature = "typescript")]
        Language::TypeScript => {
            let extractor = TypeScriptExtractor;
            extractor.extract(&tree, source, path)
        }
        #[cfg(not(feature = "typescript"))]
        Language::TypeScript => {
            anyhow::bail!("TypeScript extractor not enabled. Build with --features typescript")
        }

        Language::JavaScript => {
            #[cfg(feature = "javascript")]
            {
                let extractor = JavaScriptExtractor;
                extractor.extract(&tree, source, path)
            }
            #[cfg(not(feature = "javascript"))]
            anyhow::bail!("JavaScript extractor not enabled. Build with --features javascript")
        }

        #[cfg(feature = "python")]
        Language::Python => {
            let extractor = PythonExtractor;
            extractor.extract(&tree, source, path)
        }
        #[cfg(not(feature = "python"))]
        Language::Python => {
            anyhow::bail!("Python extractor not enabled. Build with --features python")
        }

        _ => anyhow::bail!("Extractor for {language:?} not yet implemented"),
    };

    // A0: per-file line metrics (code/comment/blank) from the tree.
    ir.metrics = metrics::compute(&tree, source, metrics::comment_kinds(&ir.language));

    // A1: always build retrieval cards after extract+resolve.
    ir.ir_version = 4;

    // Borrow 2: per-file caps — truncate + Warning diagnostics before cards,
    // so card building and downstream graph ingest stay bounded.
    apply_extraction_caps(&mut ir);

    let (file_card, symbol_cards) = code_parser_ir::build_all_cards(&ir, &Default::default());
    ir.retrieval_card = file_card;
    ir.symbol_cards = symbol_cards;

    Ok(ir)
}

/// Parse a file, skipping if the blake3 hash matches the cache.
///
/// Returns `None` when the file hasn't changed (cache hit).
/// Returns `Some(ParseResult)` when the file is new or changed.
/// On parse error, emits a diagnostic-only IR (never returns `Err`).
#[allow(unreachable_patterns)]
pub fn parse_file_cached(
    path: &Path,
    cache: &mut HashCache,
) -> Result<Option<ParseResult>, anyhow::Error> {
    let path_str = path.to_string_lossy().to_string();

    // Read bytes for hash check.
    let source = std::fs::read(path).with_context(|| format!("Failed to read {path_str}"))?;
    let hash = hash::hash_bytes(&source);

    // Short-circuit: same hash → skip.
    if cache.is_unchanged(&path_str, &hash) {
        return Ok(None);
    }

    let ir = parse_file_bytes(&path_str, &source)?;
    // Only record the hash after a successful parse — a failed parse must
    // be retried on the next event, not silently swallowed by the cache.
    cache.update(&path_str, &hash);
    Ok(Some(ParseResult {
        ir,
        errors: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "rust")]
    fn parse_rust(source: &[u8], path: &str) -> FileParseIR {
        parse_file_bytes(path, source).expect("synthetic rust source must parse")
    }

    /// Synthetic hostile file: 5_000 functions (each with 2 calls) + 2_500
    /// imports — over every cap at once.
    #[cfg(feature = "rust")]
    fn hostile_source() -> String {
        let mut src = String::new();
        for i in 0..5000 {
            src.push_str(&format!("fn f{i}() {{ helper(); helper(); }}\n"));
        }
        for i in 0..2500 {
            src.push_str(&format!("use foo_{i};\n"));
        }
        src
    }

    #[cfg(feature = "rust")]
    #[test]
    fn extraction_caps_truncate_and_emit_warnings() {
        let ir = parse_rust(hostile_source().as_bytes(), "hostile.rs");

        // Symbols: 5_000 → 4_096 with a truncation Warning.
        assert_eq!(
            ir.symbols.len(),
            MAX_SYMBOLS_PER_FILE,
            "symbols must be capped at {MAX_SYMBOLS_PER_FILE}"
        );
        // Calls: 10_000 → 8_192.
        assert_eq!(
            ir.calls.len(),
            MAX_CALLS_PER_FILE,
            "calls must be capped at {MAX_CALLS_PER_FILE}"
        );
        // Imports: 2_500 → 2_048.
        assert_eq!(
            ir.imports.len(),
            MAX_IMPORTS_PER_FILE,
            "imports must be capped at {MAX_IMPORTS_PER_FILE}"
        );

        // One Warning per truncated kind, naming the dropped count + cap.
        let warnings: Vec<&str> = ir
            .diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Warning)
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("truncated: 904 symbols (cap 4096)")),
            "symbol truncation warning missing: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("truncated: 1808 calls (cap 8192)")),
            "call truncation warning missing: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|m| m.contains("truncated: 452 imports (cap 2048)")),
            "import truncation warning missing: {warnings:?}"
        );

        // Card invariant survives truncation: symbol_cards index-aligns with
        // the truncated symbol list.
        assert_eq!(ir.symbol_cards.len(), ir.symbols.len());
        assert!(!ir.retrieval_card.text.is_empty());
    }

    #[cfg(feature = "rust")]
    #[test]
    fn small_files_are_untouched_by_caps() {
        // Parity guard: a normal file stays byte-equivalent to a run without
        // the caps pass — zero diagnostics, zero truncation, same counts.
        let src = b"fn main() { helper(); }\n";
        let ir = parse_rust(src, "small.rs");
        assert_eq!(ir.symbols.len(), 1);
        assert_eq!(ir.calls.len(), 1);
        assert_eq!(ir.imports.len(), 0);
        assert!(
            ir.diagnostics.is_empty(),
            "clean small file must have zero diagnostics: {:?}",
            ir.diagnostics
        );
    }
}
