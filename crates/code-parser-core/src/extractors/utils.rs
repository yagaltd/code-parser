/// Shared tree-sitter traversal utilities.
use tree_sitter::Node;

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
