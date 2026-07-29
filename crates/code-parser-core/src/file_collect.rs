/// Gitignore-aware source file collection via the `ignore` crate.

use ignore::WalkBuilder;

use crate::language::Language;

/// Collect source file paths from a directory, respecting .gitignore rules.
///
/// Returns paths relative to `root`. Root is canonicalized internally.
pub fn collect_source_files(
    root: &str,
    languages: &[Language],
) -> Result<Vec<String>, anyhow::Error> {
    let root_path = std::path::Path::new(root)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(root));
    let root_str = root_path.to_string_lossy().to_string();

    let mut paths: Vec<String> = Vec::new();

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
                paths.push(rel.to_string_lossy().to_string());
            }
        }
    }

    paths.sort();
    Ok(paths)
}
