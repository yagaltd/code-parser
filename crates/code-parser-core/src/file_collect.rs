/// Gitignore-aware source file collection via the `ignore` crate.
use ignore::WalkBuilder;

use crate::language::Language;

/// Size gate for collected source files (matches codegraph-rs; ProjectAtlas
/// similar). Files larger than this are skipped by the collection pass and
/// surface as diagnostic-only IRs in `parse_repo` (borrow 2 hardening) —
/// visible, never silent, never `Err`.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// A source file skipped by the collection gate (over [`MAX_FILE_BYTES`]).
///
/// `parse_repo` turns each entry into a `FileParseIR::empty` carrying a
/// Warning diagnostic, so oversized files are visible to consumers instead
/// of vanishing silently.
#[derive(Debug, Clone)]
pub struct SkippedFile {
    /// Repository-relative path of the skipped file.
    pub path: String,
    /// Actual byte length (as observed at collection time).
    pub byte_len: u64,
}

/// Collect source file paths from a directory, respecting .gitignore rules.
///
/// Returns `(paths, skipped)` where `paths` are the collectible source files
/// (relative to `root`) and `skipped` are source files over
/// [`MAX_FILE_BYTES`] (relative to `root`, with their byte length). Both
/// lists are sorted for deterministic output. Root is canonicalized
/// internally.
pub fn collect_source_files(
    root: &str,
    languages: &[Language],
) -> Result<(Vec<String>, Vec<SkippedFile>), anyhow::Error> {
    let root_path = std::path::Path::new(root)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(root));
    let root_str = root_path.to_string_lossy().to_string();

    let mut paths: Vec<String> = Vec::new();
    let mut skipped: Vec<SkippedFile> = Vec::new();

    let exts: Vec<&str> = languages
        .iter()
        .flat_map(|l| match l {
            Language::Rust => vec![".rs"],
            Language::TypeScript => vec![".ts", ".tsx"],
            Language::JavaScript => vec![".js", ".jsx", ".mjs", ".cjs"],
            Language::Python => vec![".py", ".pyi"],
        })
        .collect();

    let walker = WalkBuilder::new(&root_str)
        .standard_filters(true) // respect .gitignore
        .hidden(false)
        .build();

    for entry in walker {
        let entry = entry?;
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let ext_with_dot = format!(".{ext}");
        if exts.iter().any(|&e| e == ext_with_dot) {
            if let Ok(rel) = path.strip_prefix(&root_str) {
                let rel = rel.to_string_lossy().to_string();
                // Size gate: metadata is best-effort (permission quirks); a
                // failed stat falls through to collection (never a hard Err).
                let byte_len = entry
                    .metadata()
                    .map(|m| m.len())
                    .or_else(|_| std::fs::metadata(path).map(|m| m.len()))
                    .unwrap_or(0);
                if byte_len > MAX_FILE_BYTES {
                    skipped.push(SkippedFile {
                        path: rel,
                        byte_len,
                    });
                } else {
                    paths.push(rel);
                }
            }
        }
    }

    paths.sort();
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((paths, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "code-parser-file-collect-{}-{}",
            std::process::id(),
            label
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_file_is_skipped_with_byte_len() {
        let dir = tmp_dir("size-gate");
        std::fs::write(dir.join("small.rs"), "fn ok() {}").unwrap();
        // 5 MB of zeros — over MAX_FILE_BYTES (4 MiB).
        let big = vec![0u8; (MAX_FILE_BYTES as usize) + 1];
        std::fs::write(dir.join("big.rs"), &big).unwrap();

        let (paths, skipped) =
            collect_source_files(dir.to_str().unwrap(), &[Language::Rust]).unwrap();
        assert_eq!(paths, vec!["small.rs".to_string()]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].path, "big.rs");
        assert_eq!(skipped[0].byte_len, MAX_FILE_BYTES + 1);

        cleanup(&dir);
    }

    #[test]
    fn exactly_at_cap_is_collected() {
        let dir = tmp_dir("size-gate-boundary");
        let at_cap = vec![0u8; MAX_FILE_BYTES as usize];
        std::fs::write(dir.join("edge.rs"), &at_cap).unwrap();

        let (paths, skipped) =
            collect_source_files(dir.to_str().unwrap(), &[Language::Rust]).unwrap();
        assert_eq!(
            paths,
            vec!["edge.rs".to_string()],
            "at-cap file is not skipped"
        );
        assert!(skipped.is_empty());

        cleanup(&dir);
    }

    #[test]
    fn parse_repo_emits_diagnostic_only_ir_for_oversized_file() {
        use code_parser_ir::DiagnosticSeverity;

        let dir = tmp_dir("size-gate-repo");
        std::fs::write(dir.join("small.rs"), "fn ok() {}").unwrap();
        let big = vec![0u8; (MAX_FILE_BYTES as usize) + 1];
        std::fs::write(dir.join("huge.rs"), &big).unwrap();

        let results = crate::parse_repo(&dir, Some(vec![Language::Rust])).unwrap();
        // Never Err: the oversized file yields a diagnostic-only IR.
        let huge = results.iter().find(|r| r.ir.path == "huge.rs").unwrap();
        assert!(huge.errors.is_empty(), "skipped file is not an error");
        assert!(huge.ir.symbols.is_empty());
        assert!(huge.ir.calls.is_empty());
        assert_eq!(huge.ir.byte_len, MAX_FILE_BYTES + 1);
        assert_eq!(huge.ir.language, "unknown");
        let warn = huge
            .ir
            .diagnostics
            .iter()
            .find(|d| d.severity == DiagnosticSeverity::Warning)
            .expect("skipped file must carry a Warning diagnostic");
        assert!(
            warn.message.contains("skipped:") && warn.message.contains("> cap"),
            "warning must name the skip: {}",
            warn.message
        );

        // Small file still parses normally.
        let small = results.iter().find(|r| r.ir.path == "small.rs").unwrap();
        assert!(!small.ir.symbols.is_empty());

        cleanup(&dir);
    }
}
