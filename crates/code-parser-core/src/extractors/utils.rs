/// Shared tree-sitter traversal utilities.
use code_parser_ir::{DiagnosticIR, DiagnosticSeverity};
use tree_sitter::Node;

/// Upper bound on diagnostics emitted per file (Fix A acceptance: bounded at
/// 64 entries; further parse issues collapse into one overflow Warning).
pub const MAX_DIAGNOSTICS_PER_FILE: usize = 64;

/// Return source text of a node, panicking if location cannot be accessed.
/// Only safe after the underlying source buffer is known to live long enough.
pub fn node_text<'a>(node: &Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("<invalid-utf8>")
}

/// Return the start byte of a node. Returns 0 on scope error (edge case).
pub fn start_byte(node: &Node) -> u32 {
    node.start_byte().try_into().unwrap_or(0)
}

pub fn end_byte(node: &Node) -> u32 {
    node.end_byte().try_into().unwrap_or(0)
}

/// Find the first child with a given `kind` string.
pub fn child_by_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
    }
    None
}

/// Find all children matching a kind.
pub fn children_by_kind<'a>(node: &Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|c| c.kind() == kind)
        .collect()
}

/// Convert a tree-sitter `Point` to a 1-indexed line number.
pub fn point_line(node: &Node) -> u32 {
    node.start_position().row as u32 + 1
}

pub fn point_end_line(node: &Node) -> u32 {
    node.end_position().row as u32 + 1
}

pub fn point_column(node: &Node) -> u32 {
    node.start_position().column as u32
}

/// Collect top-level ERROR nodes (and `missing` markers) as `DiagnosticIR`.
///
/// Walk only when `tree.root_node().has_error()` (clean trees return `vec![]`
/// without a traversal). Each maximal ERROR region yields exactly one
/// `Error` diagnostic — nested ERROR nodes are not descended into. `missing`
/// markers outside ERROR regions yield `Warning` diagnostics.
///
/// Bounded: at most [`MAX_DIAGNOSTICS_PER_FILE`] entries. Further parse
/// issues are summarized as one Warning carrying the overflow count, emitted
/// in place of the last slot so the total never exceeds the bound.
pub fn collect_error_diagnostics(tree: &tree_sitter::Tree, source: &[u8]) -> Vec<DiagnosticIR> {
    if !tree.root_node().has_error() {
        return Vec::new();
    }
    let mut diags: Vec<DiagnosticIR> = Vec::new();
    let mut overflow: usize = 0;
    collect_node_diagnostics(&tree.root_node(), source, &mut diags, &mut overflow);
    if overflow > 0 {
        // Free the last slot (when full) so the overflow Warning is visible.
        if diags.len() == MAX_DIAGNOSTICS_PER_FILE {
            diags.pop();
        }
        diags.push(DiagnosticIR {
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "{overflow} more parse issue(s) omitted (bounded at {MAX_DIAGNOSTICS_PER_FILE} per file)"
            ),
            line: None,
            byte_span: None,
        });
    }
    diags
}

fn collect_node_diagnostics(
    node: &Node,
    source: &[u8],
    diags: &mut Vec<DiagnosticIR>,
    overflow: &mut usize,
) {
    if node.is_missing() {
        push_bounded(
            diags,
            overflow,
            DiagnosticIR {
                severity: DiagnosticSeverity::Warning,
                message: format!("missing {}", node.kind()),
                line: Some(point_line(node)),
                byte_span: Some((start_byte(node), end_byte(node))),
            },
        );
        return;
    }
    if node.kind() == "ERROR" {
        // One diagnostic per maximal error region: do not descend into
        // nested ERROR nodes.
        let first_kind = node
            .child(0)
            .map(|c| c.kind().to_string())
            .unwrap_or_else(|| "?".to_string());
        push_bounded(
            diags,
            overflow,
            DiagnosticIR {
                severity: DiagnosticSeverity::Error,
                message: format!("parse error: unexpected {first_kind}"),
                line: Some(point_line(node)),
                byte_span: Some((start_byte(node), end_byte(node))),
            },
        );
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_node_diagnostics(&child, source, diags, overflow);
    }
}

fn push_bounded(diags: &mut Vec<DiagnosticIR>, overflow: &mut usize, diag: DiagnosticIR) {
    if diags.len() < MAX_DIAGNOSTICS_PER_FILE {
        diags.push(diag);
    } else {
        *overflow += 1;
    }
}
