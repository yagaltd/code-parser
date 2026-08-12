//! Per-file line metrics (code/comment/blank) — the tokei-style pass
//! (code-v3 "metrics pass", 2026).
//!
//! Computed from the tree-sitter tree + source after extraction, so no
//! extra parsing pass and no new dependencies. Heuristics (documented):
//! - a line counts as **comment** when a comment node starts at its first
//!   non-whitespace character (indented `// …` inside functions included;
//!   trailing `code(); // note` does NOT count as a comment line);
//! - block comments mark every line they span;
//! - **blank** = whitespace-only lines outside comments;
//! - **code** = total − comment − blank.

use std::collections::HashSet;

use code_parser_ir::FileMetrics;
use tree_sitter::{Node, Tree};

/// Comment node kinds per language family.
pub fn comment_kinds(language: &str) -> &'static [&'static str] {
    match language {
        "Rust" => &["line_comment", "block_comment"],
        _ => &["comment"], // python, typescript, javascript, …(generic)
    }
}

/// Compute line metrics for a parsed file.
pub fn compute(tree: &Tree, source: &[u8], comment_kinds: &[&str]) -> FileMetrics {
    if source.is_empty() {
        return FileMetrics::default();
    }
    let total = source.iter().filter(|&&b| b == b'\n').count() as u32 + 1;

    let mut comment: HashSet<u32> = HashSet::new();
    collect_comment_lines(tree.root_node(), source, comment_kinds, &mut comment);

    let mut blank = 0u32;
    for (i, line) in source.split(|&b| b == b'\n').enumerate() {
        let line_no = i as u32 + 1;
        if comment.contains(&line_no) {
            continue;
        }
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            blank += 1;
        }
    }

    let comment_lines = comment.len() as u32;
    let code_lines = total.saturating_sub(comment_lines).saturating_sub(blank);
    FileMetrics {
        code_lines,
        comment_lines,
        blank_lines: blank,
    }
}

fn collect_comment_lines(node: Node<'_>, source: &[u8], kinds: &[&str], out: &mut HashSet<u32>) {
    if kinds.contains(&node.kind()) {
        let start = node.start_byte();
        // Line start of the comment's first byte.
        let line_start = source[..start]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        // The comment must start at the line's first non-whitespace char.
        let first_non_ws = source[line_start..start]
            .iter()
            .position(|b| !b.is_ascii_whitespace());
        if first_non_ws.is_none() {
            for l in node.start_position().row as u32 + 1..=node.end_position().row as u32 + 1 {
                out.insert(l);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_comment_lines(child, source, kinds, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_file_bytes;

    #[test]
    fn rust_metrics_code_comment_blank() {
        let src = b"// leading\nfn foo() {\n    // indented\n    let x = 1; // trailing\n\n    return x;\n}\n";
        let ir = parse_file_bytes("src/lib.rs", src).unwrap();
        let m = &ir.metrics;
        // 8 lines: leading comment, fn foo {, indented comment,
        // let + trailing comment, blank, return, }, blank (trailing \n).
        assert_eq!(m.comment_lines, 2, "leading + indented: {m:?}");
        assert_eq!(m.blank_lines, 2, "line 5 + trailing empty: {m:?}");
        assert_eq!(m.code_lines, 4, "code lines: {m:?}");
        assert_eq!(m.comment_lines + m.blank_lines + m.code_lines, 8, "{m:?}");
    }

    #[test]
    fn block_comment_spans_lines() {
        let src = b"/*\n * block\n * comment\n */\nfn f() {}\n";
        let ir = parse_file_bytes("src/lib.rs", src).unwrap();
        let m = &ir.metrics;
        assert_eq!(m.comment_lines, 4, "{m:?}");
        assert_eq!(m.code_lines, 1, "{m:?}");
    }

    #[test]
    fn empty_file_zero_metrics() {
        let ir = parse_file_bytes("src/lib.rs", b"").unwrap();
        assert_eq!(ir.metrics, FileMetrics::default());
    }
}
