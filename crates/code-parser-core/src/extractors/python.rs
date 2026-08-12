/// Python language extractor — walks a tree-sitter Python AST and collects
/// symbols, calls, imports, and diagnostics.
///
/// Single-pass recursive walk. Tracks current function and class context.
use code_parser_ir::{
    CallIR, DiagnosticIR, FileParseIR, ImportIR, ImportKind, ParameterIR, SymbolIR, SymbolKind,
};
use tree_sitter::Node;

use crate::hash;
use crate::language::Language;

use super::utils;
use super::LanguageExtractor;

pub struct PythonExtractor;

impl LanguageExtractor for PythonExtractor {
    fn extract(&self, tree: &tree_sitter::Tree, source: &[u8], file_path: &str) -> FileParseIR {
        let content_hash = hash::hash_bytes(source);
        let line_count = source.iter().filter(|&&b| b == b'\n').count() as u32 + 1;
        let byte_len = source.len() as u64;

        let mut ctx = ExtractCtx::new(source);
        ctx.file_path = file_path;
        ctx.visit_node(&tree.root_node());

        // In-file resolve.
        resolve_in_file(&mut ctx);

        FileParseIR {
            ir_version: 3,
            path: file_path.to_string(),
            language: Language::Python.as_str().to_string(),
            content_hash,
            byte_len,
            line_count,
            metrics: code_parser_ir::FileMetrics::default(),
            symbols: ctx.symbols,
            calls: ctx.calls,
            imports: ctx.imports,
            retrieval_card: Default::default(),
            symbol_cards: Vec::new(),
            diagnostics: ctx.diagnostics,
        }
    }
}

// ── Extraction context ───────────────────────────────────────────────────

struct ExtractCtx<'a> {
    source: &'a [u8],
    symbols: Vec<SymbolIR>,
    calls: Vec<CallIR>,
    imports: Vec<ImportIR>,
    diagnostics: Vec<DiagnosticIR>,
    /// When inside a function/method body, this holds the caller's local_key.
    current_caller: Option<String>,
    /// Current class name stack — used for Method qualified names.
    class_stack: Vec<String>,
    /// Source file path (file-level test detection).
    file_path: &'a str,
}

impl<'a> ExtractCtx<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            symbols: Vec::new(),
            calls: Vec::new(),
            imports: Vec::new(),
            diagnostics: Vec::new(),
            current_caller: None,
            class_stack: Vec::new(),
            file_path: "",
        }
    }

    /// Test detection: test_* / Test* names + test files (python).
    fn is_test_symbol(&self, node: &Node) -> bool {
        if is_test_file_path(self.file_path) {
            return true;
        }
        if let Some(name) = node
            .child_by_field_name("name")
            .map(|n| self.text(&n).to_string())
        {
            return name.starts_with("test_") || name.starts_with("Test");
        }
        false
    }

    fn text(&self, node: &Node) -> &str {
        utils::node_text(node, self.source)
    }

    // ── recursive walk ───────────────────────────────────────────────

    fn visit_node(&mut self, node: &Node<'a>) {
        match node.kind() {
            // Container nodes — recurse into children.
            "module"
            | "block"
            | "parameters"
            | "expression_list"
            | "argument_list"
            | "pattern_list"
            | "tuple"
            | "parenthesized_expression"
            | "list"
            | "dictionary"
            | "set"
            | "string"
            | "comment"
            | "decorator" => {
                self.visit_children(node);
            }

            "function_definition" => {
                let class_name = self.class_stack.last().cloned();
                self.visit_function(node, class_name.as_deref());
            }
            "class_definition" => self.visit_class(node),
            "decorated_definition" => {
                // Unwrap: decorated_definition contains the actual function/class.
                // Look for function_definition or class_definition child.
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "function_definition" => {
                            let class_name = self.class_stack.last().cloned();
                            self.visit_function(&child, class_name.as_deref());
                        }
                        "class_definition" => self.visit_class(&child),
                        _ => self.visit_node(&child),
                    }
                }
            }

            // Calls
            "call" => self.visit_call(node),

            // Imports
            "import_statement" => self.visit_import(node),
            "import_from_statement" => self.visit_import_from(node),

            // Recurse into everything else (expressions, statements, etc.)
            _ => {
                self.visit_children(node);
            }
        }
    }

    fn visit_children(&mut self, node: &Node<'a>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit_node(&child);
        }
    }

    // ── symbol visitors ──────────────────────────────────────────────

    fn visit_function(&mut self, node: &Node<'a>, class_name: Option<&str>) {
        let name = node
            .child_by_field_name("name")
            .map(|n| self.text(&n).to_string())
            .unwrap_or_default();

        let params = extract_parameters(node, self.source);
        let return_type = node
            .child_by_field_name("return_type")
            .map(|n| self.text(&n).to_string());
        let sig = build_signature(node, self.source);
        let doc = extract_docstring(node, self.source);

        let (qualified_name, local_key, kind) = if let Some(cls) = class_name {
            (
                format!("{cls}.{name}"),
                format!("{cls}.{name}"),
                SymbolKind::Method,
            )
        } else {
            (name.clone(), name.clone(), SymbolKind::Function)
        };

        // Check if it's async (V1: recorded but not used in IR yet).
        let _is_async = node.child(0).map_or(false, |c| c.kind() == "async");

        self.symbols.push(SymbolIR {
            local_key: local_key.clone(),
            name,
            qualified_name,
            kind,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: sig,
            parameters: params,
            return_type,
            is_test: self.is_test_symbol(node),
            docstring: doc,
        });

        // Visit body with caller context.
        let prev = self.current_caller.replace(local_key);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }
        self.current_caller = prev;
    }

    fn visit_class(&mut self, node: &Node<'a>) {
        let name = node
            .child_by_field_name("name")
            .map(|n| self.text(&n).to_string())
            .unwrap_or_default();
        let doc = extract_docstring(node, self.source);

        self.symbols.push(SymbolIR {
            local_key: name.clone(),
            name: name.clone(),
            qualified_name: name.clone(),
            kind: SymbolKind::Class,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: None,
            parameters: Vec::new(),
            return_type: None,
            is_test: self.is_test_symbol(node),
            docstring: doc,
        });

        // Push class context so methods get qualified names.
        self.class_stack.push(name);

        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }

        self.class_stack.pop();
    }

    // ── call visitor ─────────────────────────────────────────────────

    fn visit_call(&mut self, node: &Node<'a>) {
        let line = utils::point_line(node);
        let callee_name = extract_call_callee(node, self.source);

        if let Some(ref caller) = self.current_caller {
            self.calls.push(CallIR {
                caller_local_key: caller.clone(),
                callee_name,
                callee_local_key: None,
                callee_file: None,
                callee_external: false,
                line,
                column: Some(utils::point_column(node)),
            });
        }
    }

    // ── import visitors ──────────────────────────────────────────────

    fn visit_import(&mut self, node: &Node<'a>) {
        let line = utils::point_line(node);
        // import foo, bar
        // `name` field can be aliased_import or dotted_name
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "dotted_name" => {
                    let name = self.text(&child).to_string();
                    self.imports.push(ImportIR {
                        import_name: name.clone(),
                        target_module: String::new(),
                        kind: ImportKind::Named,
                        line: Some(line),
                        column: Some(utils::point_column(node)),
                    });
                }
                "aliased_import" => {
                    let name_node = child.child_by_field_name("name");
                    let name = name_node
                        .as_ref()
                        .map(|n| self.text(n).to_string())
                        .unwrap_or_default();
                    self.imports.push(ImportIR {
                        import_name: name,
                        target_module: String::new(),
                        kind: ImportKind::Named,
                        line: Some(line),
                        column: Some(utils::point_column(node)),
                    });
                }
                _ => {}
            }
        }
    }

    fn visit_import_from(&mut self, node: &Node<'a>) {
        let line = utils::point_line(node);
        // from module import name1, name2
        let module = node
            .child_by_field_name("module_name")
            .map(|n| self.text(&n).to_string())
            .unwrap_or_default();

        // `from X import *` — wildcard_import is a direct named child with no field name.
        let is_star = utils::child_by_kind(node, "wildcard_import").is_some();

        if is_star {
            self.imports.push(ImportIR {
                import_name: "*".to_string(),
                target_module: module,
                kind: ImportKind::Star,
                line: Some(line),
                column: Some(utils::point_column(node)),
            });
            return;
        }

        // Named imports.
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "dotted_name" => {
                    let name = self.text(&child).to_string();
                    // Only push if it's not the module_name itself.
                    if Some(name.as_str())
                        != node
                            .child_by_field_name("module_name")
                            .as_ref()
                            .map(|n| self.text(n))
                    {
                        self.imports.push(ImportIR {
                            import_name: name,
                            target_module: module.clone(),
                            kind: ImportKind::Named,
                            line: Some(line),
                            column: Some(utils::point_column(node)),
                        });
                    }
                }
                "aliased_import" => {
                    let name_node = child.child_by_field_name("name");
                    let name = name_node
                        .as_ref()
                        .map(|n| self.text(n).to_string())
                        .unwrap_or_default();
                    self.imports.push(ImportIR {
                        import_name: name,
                        target_module: module.clone(),
                        kind: ImportKind::Named,
                        line: Some(line),
                        column: Some(utils::point_column(node)),
                    });
                }
                _ => {}
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Extract callee expression text from a call node.
fn extract_call_callee(node: &Node, source: &[u8]) -> String {
    let func = node.child_by_field_name("function");
    match func {
        Some(f) => {
            let mut parts: Vec<String> = Vec::new();
            collect_call_name_parts(&f, source, &mut parts);
            if parts.is_empty() {
                utils::node_text(&f, source).to_string()
            } else {
                parts.join(".")
            }
        }
        None => utils::node_text(node, source).to_string(),
    }
}

/// Recursively collect dotted name parts (e.g. self.data.get → ["self","data","get"]).
fn collect_call_name_parts(node: &Node, source: &[u8], parts: &mut Vec<String>) {
    match node.kind() {
        "attribute" => {
            if let Some(object) = node.child_by_field_name("object") {
                collect_call_name_parts(&object, source, parts);
            }
            if let Some(attr) = node.child_by_field_name("attribute") {
                parts.push(utils::node_text(&attr, source).to_string());
            }
        }
        "identifier" => {
            parts.push(utils::node_text(node, source).to_string());
        }
        "call" => {
            // Chained call: foo().bar() — push resolved then recurse.
            let text = utils::node_text(node, source);
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
        _ => {
            let text = utils::node_text(node, source);
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
    }
}

/// Build human-readable signature from a function_definition node.
fn build_signature(node: &Node, source: &[u8]) -> Option<String> {
    // Use source text from start of function up to ':' at end of def line.
    let start = node.start_byte() as usize;
    let params = node.child_by_field_name("parameters")?;
    let end = params.end_byte() as usize;
    if end > start && end <= source.len() {
        let sig = String::from_utf8_lossy(&source[start..end]).into_owned();
        Some(sig.trim().to_string())
    } else {
        None
    }
}

/// Extract parameters from a function_definition.
fn extract_parameters(node: &Node, source: &[u8]) -> Vec<ParameterIR> {
    let params_node = node.child_by_field_name("parameters");
    let params_node = match params_node {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut params = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                // Bare parameter: `def foo(x)`
                params.push(ParameterIR {
                    name: utils::node_text(&child, source).to_string(),
                    type_annotation: None,
                    default_value: None,
                });
            }
            "typed_parameter" | "typed_default_parameter" => {
                let name = extract_param_name(&child, source);
                let type_ann = child
                    .child_by_field_name("type")
                    .map(|n| utils::node_text(&n, source).to_string());
                let default = child
                    .child_by_field_name("default_value")
                    .or_else(|| child.child_by_field_name("value"))
                    .map(|n| utils::node_text(&n, source).to_string());
                params.push(ParameterIR {
                    name,
                    type_annotation: type_ann,
                    default_value: default,
                });
            }
            "default_parameter" => {
                let name = extract_param_name(&child, source);
                let default = child
                    .child_by_field_name("default_value")
                    .or_else(|| child.child_by_field_name("value"))
                    .map(|n| utils::node_text(&n, source).to_string());
                params.push(ParameterIR {
                    name,
                    type_annotation: None,
                    default_value: default,
                });
            }
            "list_splat_pattern" | "dictionary_splat_pattern" => {
                // *args / **kwargs
                let name = child
                    .child_by_field_name("pattern")
                    .or_else(|| child.child(0))
                    .map(|n| utils::node_text(&n, source).to_string())
                    .unwrap_or_default();
                params.push(ParameterIR {
                    name,
                    type_annotation: None,
                    default_value: None,
                });
            }
            _ => {}
        }
    }
    params
}

fn extract_param_name(node: &Node, source: &[u8]) -> String {
    // Try field name first, then fall back to first identifier child.
    node.child_by_field_name("name")
        .or_else(|| utils::child_by_kind(node, "identifier"))
        .map(|n| utils::node_text(&n, source).to_string())
        .unwrap_or_default()
}

/// Extract docstring from function/class body.
fn extract_docstring(node: &Node, source: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    // First statement in body may be an expression_statement containing a string.
    let first_stmt = body.child(0)?;
    if first_stmt.kind() == "expression_statement" {
        let expr = first_stmt.child(0)?;
        if expr.kind() == "string" {
            let text = utils::node_text(&expr, source);
            // Strip surrounding quotes.
            let inner = text
                .strip_prefix("\"\"\"")
                .or_else(|| text.strip_prefix("'''"))
                .and_then(|t| t.strip_suffix("\"\"\"").or_else(|| t.strip_suffix("'''")));
            if let Some(s) = inner {
                return Some(s.trim().to_string());
            }
            // Single-line string doc.
            let inner = text
                .strip_prefix('"')
                .or_else(|| text.strip_prefix('\''))
                .and_then(|t| t.strip_suffix('"').or_else(|| t.strip_suffix('\'')));
            if let Some(s) = inner {
                return Some(s.to_string());
            }
        }
    }
    None
}

// ── In-file resolution ───────────────────────────────────────────────────

fn resolve_in_file(ctx: &mut ExtractCtx) {
    let mut by_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut by_qualified: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for sym in &ctx.symbols {
        by_name
            .entry(sym.name.clone())
            .or_insert_with(|| sym.local_key.clone());
        by_qualified
            .entry(sym.qualified_name.clone())
            .or_insert_with(|| sym.local_key.clone());
    }

    for call in &mut ctx.calls {
        if call.callee_local_key.is_some() {
            continue;
        }
        // 1. Exact bare name match.
        if let Some(key) = by_name.get(&call.callee_name) {
            call.callee_local_key = Some(key.clone());
            call.callee_external = false;
            continue;
        }
        // 2. Exact qualified_name match.
        if let Some(key) = by_qualified.get(&call.callee_name) {
            call.callee_local_key = Some(key.clone());
            call.callee_external = false;
            continue;
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

/// File-level test detection (python): test_*.py files and tests/ dirs.
fn is_test_file_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.starts_with("test_")
        || base.ends_with("_test.py")
        || path.contains("/tests/")
        || path.contains("/test/")
        || path.starts_with("tests/")
        || path.starts_with("test/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_python(source: &[u8], path: &str) -> FileParseIR {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let ir = PythonExtractor.extract(&tree, source, path);
        let mut ir = ir;
        ir.metrics =
            crate::metrics::compute(&tree, source, crate::metrics::comment_kinds(&ir.language));
        ir.ir_version = 3;
        ir
    }

    const SIMPLE_PY: &str = r#"import os

class Greeter:
    """A friendly class."""
    def greet(self, name: str) -> str:
        return f"Hello, {name}"

def main():
    g = Greeter()
    g.greet("world")
"#;

    #[test]
    fn simple_fixture_symbols() {
        let ir = parse_python(SIMPLE_PY.as_bytes(), "simple.py");

        assert_eq!(ir.language, "Python");

        let sym_names: Vec<&str> = ir.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            sym_names.contains(&"Greeter"),
            "missing class Greeter: {sym_names:?}"
        );
        assert!(sym_names.contains(&"greet"), "missing method greet");
        assert!(sym_names.contains(&"main"), "missing fn main");

        // Class
        let cls = ir.symbols.iter().find(|s| s.name == "Greeter").unwrap();
        assert_eq!(cls.kind, SymbolKind::Class);
        assert!(cls
            .docstring
            .as_ref()
            .map_or(false, |d| d.contains("friendly")));

        // Method
        let greet = ir.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert_eq!(greet.kind, SymbolKind::Method);
        assert_eq!(greet.qualified_name, "Greeter.greet");
        assert_eq!(greet.local_key, "Greeter.greet");
        assert_eq!(greet.parameters.len(), 2); // self, name
        assert_eq!(greet.parameters[0].name, "self");
        assert_eq!(greet.parameters[1].name, "name");
        assert_eq!(greet.return_type.as_deref(), Some("str"));

        // Function
        let main_sym = ir.symbols.iter().find(|s| s.name == "main").unwrap();
        assert_eq!(main_sym.kind, SymbolKind::Function);
    }

    #[test]
    fn simple_fixture_calls() {
        let ir = parse_python(SIMPLE_PY.as_bytes(), "simple.py");

        // main calls Greeter() and g.greet()
        let main_calls: Vec<&CallIR> = ir
            .calls
            .iter()
            .filter(|c| c.caller_local_key == "main")
            .collect();
        assert!(main_calls.len() >= 2, "expected >=2 calls from main");

        let constructor = main_calls
            .iter()
            .find(|c| c.callee_name == "Greeter")
            .unwrap();
        assert_eq!(constructor.line, 9);

        let method_call = main_calls
            .iter()
            .find(|c| c.callee_name == "g.greet")
            .unwrap();
        assert_eq!(method_call.line, 10);
        // g.greet is in-file resolved (greet is a method in Greeter)
        // V1: bare name match won't find g.greet → stays unresolved, not external
        assert!(!method_call.callee_external);
    }

    #[test]
    fn simple_fixture_imports() {
        let ir = parse_python(SIMPLE_PY.as_bytes(), "simple.py");

        assert!(
            ir.imports.iter().any(|i| i.import_name == "os"),
            "missing os import"
        );
    }

    #[test]
    fn from_import() {
        let src = b"from collections import defaultdict, Counter\n";
        let ir = parse_python(src, "t.py");

        let dd = ir
            .imports
            .iter()
            .find(|i| i.import_name == "defaultdict")
            .unwrap();
        assert_eq!(dd.target_module, "collections");
        assert_eq!(dd.kind, ImportKind::Named);

        let ct = ir
            .imports
            .iter()
            .find(|i| i.import_name == "Counter")
            .unwrap();
        assert_eq!(ct.target_module, "collections");
    }

    #[test]
    fn star_import() {
        let src = b"from os import *\n";
        let ir = parse_python(src, "t.py");

        assert_eq!(ir.imports.len(), 1);
        assert_eq!(ir.imports[0].import_name, "*");
        assert_eq!(ir.imports[0].target_module, "os");
        assert_eq!(ir.imports[0].kind, ImportKind::Star);
    }

    #[test]
    fn decorated_function() {
        let src = b"@staticmethod\ndef foo():\n    pass\n";
        let ir = parse_python(src, "t.py");

        assert_eq!(ir.symbols.len(), 1);
        assert_eq!(ir.symbols[0].name, "foo");
        assert_eq!(ir.symbols[0].kind, SymbolKind::Function);
    }

    #[test]
    fn async_function() {
        let src = b"async def fetch():\n    pass\n";
        let ir = parse_python(src, "t.py");

        assert_eq!(ir.symbols.len(), 1);
        assert_eq!(ir.symbols[0].name, "fetch");
        assert_eq!(ir.symbols[0].kind, SymbolKind::Function);
    }

    #[test]
    fn method_with_default_param() {
        let src = b"class A:\n    def foo(self, x=42):\n        pass\n";
        let ir = parse_python(src, "t.py");

        let foo = ir.symbols.iter().find(|s| s.name == "foo").unwrap();
        assert_eq!(foo.kind, SymbolKind::Method);
        assert_eq!(foo.qualified_name, "A.foo");
        assert_eq!(foo.parameters.len(), 2);
        assert_eq!(foo.parameters[1].default_value.as_deref(), Some("42"));
    }

    #[test]
    fn chained_call() {
        let src = b"def foo():\n    obj.method().another()\n";
        let ir = parse_python(src, "t.py");

        let calls: Vec<&CallIR> = ir
            .calls
            .iter()
            .filter(|c| c.caller_local_key == "foo")
            .collect();
        assert!(calls.len() >= 1, "expected calls from foo");

        // The tree-sitter might expose the chained call as nested call nodes.
        // V1: best-effort.
    }

    #[test]
    fn golden_ir_matches_fixture() {
        use crate::extractors::golden;

        let source = include_str!("../../../../fixtures/python/simple.py");
        let actual_ir = parse_python(source.as_bytes(), "fixtures/python/simple.py");
        let expected_json = include_str!("../../../../fixtures/python/simple.ir.json");
        let mut expected_ir: FileParseIR =
            serde_json::from_str(expected_json).expect("valid golden JSON");

        // Rebuild cards with cleared hash, then validate card invariants.
        golden::normalize_for_compare(&mut expected_ir);
        let mut actual_norm = actual_ir.clone();
        golden::normalize_for_compare(&mut actual_norm);

        golden::validate_schema(&actual_norm).expect("actual IR violates schema");
        golden::validate_schema(&expected_ir).expect("golden IR violates schema");

        assert_eq!(
            serde_json::to_string_pretty(&actual_norm).unwrap(),
            serde_json::to_string_pretty(&expected_ir).unwrap(),
            "Golden IR mismatch for Python"
        );
    }

    #[test]
    fn is_test_flag_detects_python_test_conventions() {
        let src = "def test_compute():\n    return 1\n\ndef helper():\n    return 2\n\nclass TestWidget:\n    def test_click(self):\n        pass\n\n    def render(self):\n        pass\n";
        let ir = parse_python(src.as_bytes(), "src/widget.py");
        let flag = |k: &str| {
            ir.symbols
                .iter()
                .find(|s| s.local_key == k)
                .map(|s| s.is_test)
                .unwrap_or_else(|| panic!("symbol {k}"))
        };
        assert!(flag("test_compute"), "test_* fn must be test");
        assert!(!flag("helper"));
        assert!(flag("TestWidget"), "Test* class must be test");
        assert!(
            flag("TestWidget.test_click"),
            "method in Test class must be test"
        );
        assert!(
            !flag("TestWidget.render"),
            "non-test method in Test class must NOT be test"
        );

        let file_ir = parse_python(b"def f():\n    pass\n", "tests/test_widget.py");
        assert!(file_ir.symbols[0].is_test, "test_*.py file must be test");
    }
}
