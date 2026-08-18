/// JavaScript extractor — walks a tree-sitter JavaScript AST and collects
/// symbols, calls, imports, and diagnostics.
///
/// Single-pass recursive walk. Tracks current function/class context for call attribution.
use code_parser_ir::{
    CallIR, DiagnosticIR, FileParseIR, ImportIR, ImportKind, ParameterIR, SymbolIR, SymbolKind,
};
use tree_sitter::Node;

use crate::hash;
use crate::language::Language;

use super::utils;
use super::LanguageExtractor;

pub struct JavaScriptExtractor;

impl LanguageExtractor for JavaScriptExtractor {
    fn extract(&self, tree: &tree_sitter::Tree, source: &[u8], file_path: &str) -> FileParseIR {
        let content_hash = hash::hash_bytes(source);
        let line_count = source.iter().filter(|&&b| b == b'\n').count() as u32 + 1;
        let byte_len = source.len() as u64;

        let mut ctx = ExtractCtx::new(source);
        ctx.file_path = file_path;
        ctx.visit_node(&tree.root_node());

        // In-file resolve: map same-file calls to local_keys.
        resolve_in_file(&mut ctx);

        // Diagnostics: top-level ERROR regions + missing markers (bounded).
        ctx.diagnostics = utils::collect_error_diagnostics(tree, source);

        FileParseIR {
            ir_version: 4,
            path: file_path.to_string(),
            language: Language::JavaScript.as_str().to_string(),
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
    /// Current class name when inside a class body (for method qualified names).
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

    /// Test detection (file-level: .test./.spec. names, __tests__ dirs).
    fn is_test_symbol(&self, _node: &Node) -> bool {
        is_test_file_path(self.file_path)
    }

    fn text(&self, node: &Node) -> &str {
        utils::node_text(node, self.source)
    }

    // ── recursive walk ───────────────────────────────────────────────

    fn visit_node(&mut self, node: &Node<'a>) {
        match node.kind() {
            // Container nodes — recurse.
            "program"
            | "statement_block"
            | "object"
            | "array"
            | "parenthesized_expression"
            | "arguments"
            | "formal_parameters"
            | "object_pattern"
            | "array_pattern"
            | "switch_body"
            | "template_substitution"
            | "computed_property_name"
            | "comment"
            | "html_comment"
            | "hash_bang_line" => {
                self.visit_children(node);
            }

            // Declarations.
            "function_declaration" => self.visit_function(node, None),
            "class_declaration" => self.visit_class(node),

            // Variable declarations (may contain arrow functions).
            "lexical_declaration" | "variable_declaration" => {
                self.visit_variable_declaration(node);
            }

            // Method inside class body.
            "method_definition" => self.visit_method(node),

            // Calls.
            "call_expression" => self.visit_call(node),
            "new_expression" => self.visit_new_expr(node),

            // Imports.
            "import_statement" => self.visit_import(node),

            // Export: recurse to find declarations (re-exported items).
            "export_statement" => {
                self.visit_children(node);
            }

            // Expressions that may contain calls — recurse.
            "expression_statement"
            | "return_statement"
            | "binary_expression"
            | "unary_expression"
            | "update_expression"
            | "await_expression"
            | "yield_expression"
            | "conditional_expression"
            | "sequence_expression"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "if_statement"
            | "switch_statement"
            | "for_statement"
            | "for_in_statement"
            | "while_statement"
            | "do_statement"
            | "try_statement"
            | "catch_clause"
            | "finally_clause"
            | "with_statement"
            | "labeled_statement"
            | "throw_statement"
            | "debugger_statement"
            | "empty_statement"
            | "spread_element"
            | "template_string"
            | "string"
            | "regex"
            | "number"
            | "true"
            | "false"
            | "null"
            | "undefined"
            | "this"
            | "super"
            | "identifier"
            | "property_identifier"
            | "shorthand_property_identifier"
            | "statement_identifier"
            | "arrow_function"
            | "generator_function"
            | "generator_function_declaration"
            | "class"
            | "extends_clause"
            | "class_body"
            | "class_heritage"
            | "decorator"
            | "decorator_call_expression"
            | "pair"
            | "jsx_element"
            | "jsx_self_closing_element"
            | "jsx_opening_element"
            | "jsx_closing_element"
            | "jsx_expression"
            | "jsx_fragment"
            | "jsx_attribute"
            | "jsx_text"
            | "jsx_namespace_name" => {
                self.visit_children(node);
            }

            // Skip: types (TypeScript interspersed), semicolons, etc.
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

    // ── function visitor ─────────────────────────────────────────────

    fn visit_function(&mut self, node: &Node<'a>, class_ty: Option<&str>) {
        let name_node = node.child_by_field_name("name");
        let name = name_node
            .as_ref()
            .map(|n| self.text(n).to_string())
            .unwrap_or_default();

        if name.is_empty() {
            // Anonymous function — recurse into body only.
            self.recurse_body_for_calls(node);
            return;
        }

        let params = extract_params(node, self.source);
        let sig = build_sig(node, self.source, &name);

        let (qualified_name, local_key, kind) = if let Some(ty) = class_ty {
            (
                format!("{ty}::{name}"),
                format!("{ty}::{name}"),
                SymbolKind::Method,
            )
        } else {
            (name.clone(), name.clone(), SymbolKind::Function)
        };

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
            return_type: None,
            is_test: self.is_test_symbol(node),
            docstring: extract_jsdoc(node, self.source),
        });

        // Visit body with caller context.
        let prev = self.current_caller.replace(local_key);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }
        self.current_caller = prev;
    }

    fn recurse_body_for_calls(&mut self, node: &Node<'a>) {
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }
    }

    // ── class visitor ────────────────────────────────────────────────

    fn visit_class(&mut self, node: &Node<'a>) {
        let name_node = node.child_by_field_name("name");
        let name = name_node
            .as_ref()
            .map(|n| self.text(n).to_string())
            .unwrap_or_default();

        if name.is_empty() {
            self.visit_children(node);
            return;
        }

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
            docstring: extract_jsdoc(node, self.source),
        });

        // Push class context so methods get qualified names.
        self.class_stack.push(name.clone());

        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }

        self.class_stack.pop();
    }

    // ── method visitor ───────────────────────────────────────────────

    fn visit_method(&mut self, node: &Node<'a>) {
        let name_node = node.child_by_field_name("name");
        let name = name_node
            .as_ref()
            .map(|n| self.text(n).to_string())
            .unwrap_or_default();

        if name.is_empty() {
            self.recurse_body_for_calls(node);
            return;
        }

        let class_ty = self.class_stack.last().map(|s| s.as_str());

        let (qualified_name, local_key) = if let Some(ty) = class_ty {
            (format!("{ty}::{name}"), format!("{ty}::{name}"))
        } else {
            (name.clone(), name.clone())
        };

        let params = extract_params(node, self.source);
        let sig = build_sig(node, self.source, &name);

        self.symbols.push(SymbolIR {
            local_key: local_key.clone(),
            name,
            qualified_name,
            kind: SymbolKind::Method,
            start_line: utils::point_line(node),
            end_line: utils::point_end_line(node),
            start_byte: Some(utils::start_byte(node)),
            end_byte: Some(utils::end_byte(node)),
            signature: sig,
            parameters: params,
            return_type: None,
            is_test: self.is_test_symbol(node),
            docstring: extract_jsdoc(node, self.source),
        });

        let prev = self.current_caller.replace(local_key);
        if let Some(body) = node.child_by_field_name("body") {
            self.visit_node(&body);
        }
        self.current_caller = prev;
    }

    // ── variable declaration (const/let/var) ─────────────────────────

    fn visit_variable_declaration(&mut self, node: &Node<'a>) {
        // Process each variable_declarator — arrow functions become Function symbols.
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "variable_declarator" => self.visit_variable_declarator(&child),
                _ => self.visit_node(&child),
            }
        }
    }

    fn visit_variable_declarator(&mut self, node: &Node<'a>) {
        let name_node = node.child_by_field_name("name");
        let value_node = node.child_by_field_name("value");

        let name = name_node
            .as_ref()
            .map(|n| self.text(n).to_string())
            .unwrap_or_default();

        if name.is_empty() {
            if let Some(val) = value_node {
                self.visit_node(&val);
            }
            return;
        }

        match value_node {
            Some(val) if val.kind() == "arrow_function" || val.kind() == "function" => {
                // const foo = () => {} — emit as Function
                let params = extract_params(&val, self.source);
                let sig = format!("const {name}(...)");

                self.symbols.push(SymbolIR {
                    local_key: name.clone(),
                    name: name.clone(),
                    qualified_name: name.clone(),
                    kind: SymbolKind::Function,
                    start_line: utils::point_line(node),
                    end_line: utils::point_end_line(node),
                    start_byte: Some(utils::start_byte(node)),
                    end_byte: Some(utils::end_byte(node)),
                    signature: Some(sig),
                    parameters: params,
                    return_type: None,
                    is_test: self.is_test_symbol(node),
                    docstring: None,
                });

                let prev = self.current_caller.replace(name);
                if let Some(body) = val.child_by_field_name("body") {
                    self.visit_node(&body);
                }
                self.current_caller = prev;
            }
            _ => {
                // Regular variable — not a symbol in V1 (domain doesn't need const values).
                // Recurse into value for any nested calls/expressions.
                if let Some(val) = value_node {
                    self.visit_node(&val);
                }
            }
        }
    }

    // ── call visitor ─────────────────────────────────────────────────

    fn visit_call(&mut self, node: &Node<'a>) {
        // Skip calls outside function bodies (top-level expressions).
        let caller = match &self.current_caller {
            Some(c) => c.clone(),
            None => return,
        };
        let line = utils::point_line(node);
        let callee_name = extract_callee_name(node, self.source);

        self.calls.push(CallIR {
            caller_local_key: caller,
            callee_name,
            callee_local_key: None,
            callee_file: None,
            callee_external: false,
            line,
            column: Some(utils::point_column(node)),
        });
    }

    fn visit_new_expr(&mut self, node: &Node<'a>) {
        // Skip constructor calls outside function bodies.
        let caller = match &self.current_caller {
            Some(c) => c.clone(),
            None => return,
        };
        let line = utils::point_line(node);
        let constructor = node.child_by_field_name("constructor");
        let callee_name = match constructor {
            Some(c) => {
                let n = self.text(&c).to_string();
                format!("new {n}")
            }
            None => "new <unknown>".to_string(),
        };

        self.calls.push(CallIR {
            caller_local_key: caller,
            callee_name,
            callee_local_key: None,
            callee_file: None,
            callee_external: false,
            line,
            column: Some(utils::point_column(node)),
        });

        // Recurse into arguments for nested calls.
        if let Some(args) = node.child_by_field_name("arguments") {
            self.visit_node(&args);
        }
    }

    // ── import visitor ───────────────────────────────────────────────

    fn visit_import(&mut self, node: &Node<'a>) {
        let line = Some(utils::point_line(node));
        let source_node = node.child_by_field_name("source");
        let module = source_node
            .as_ref()
            .map(|n| {
                let raw = self.text(n);
                raw.trim_matches('"').trim_matches('\'').to_string()
            })
            .unwrap_or_default();

        let clause = utils::child_by_kind(node, "import_clause");

        match clause {
            None => {
                // import './mod' — side-effect only.
            }
            Some(ref c) => {
                // import_clause wraps different structures.
                // Check for named_imports child first.
                if let Some(named) = utils::child_by_kind(c, "named_imports") {
                    for spec in extract_import_specifiers(&named, self.source) {
                        self.imports.push(ImportIR {
                            import_name: spec,
                            target_module: module.clone(),
                            kind: ImportKind::Named,
                            line,
                            column: Some(utils::point_column(node)),
            resolved: None,
                        });
                    }
                }

                // Check for namespace_import child.
                if let Some(ns) = utils::child_by_kind(c, "namespace_import") {
                    let name = utils::child_by_kind(&ns, "identifier")
                        .map(|n| self.text(&n).to_string())
                        .unwrap_or_default();
                    self.imports.push(ImportIR {
                        import_name: name,
                        target_module: module.clone(),
                        kind: ImportKind::Star,
                        line,
                        column: Some(utils::point_column(node)),
            resolved: None,
                    });
                }

                // Default import: look for identifier child inside import_clause.
                if c.kind() == "identifier" {
                    // Direct identifier (might happen in some tree-sitter versions).
                    let name = self.text(c).to_string();
                    if !name.is_empty() {
                        self.imports.push(ImportIR {
                            import_name: name,
                            target_module: module.clone(),
                            kind: ImportKind::Default,
                            line,
                            column: Some(utils::point_column(node)),
            resolved: None,
                        });
                    }
                } else if let Some(id) = utils::child_by_kind(c, "identifier") {
                    // import_clause wraps an identifier.
                    let name = self.text(&id).to_string();
                    self.imports.push(ImportIR {
                        import_name: name,
                        target_module: module.clone(),
                        kind: ImportKind::Default,
                        line,
                        column: Some(utils::point_column(node)),
            resolved: None,
                    });
                } else {
                    // Fallback: use import_clause source text.
                    let name = self.text(c).to_string();
                    if !name.is_empty() {
                        self.imports.push(ImportIR {
                            import_name: name,
                            target_module: module.clone(),
                            kind: ImportKind::Default,
                            line,
                            column: Some(utils::point_column(node)),
            resolved: None,
                        });
                    }
                }
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Get the name from an import_clause (default or namespace).
#[allow(dead_code)]
fn import_clause_name(node: &Node, source: &[u8]) -> String {
    node.child_by_field_name("name")
        .map(|n| utils::node_text(&n, source).to_string())
        .unwrap_or_default()
}

/// Extract specifier names from named_imports { foo, bar as baz }.
fn extract_import_specifiers(node: &Node, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_specifier" {
            // The identifier child has the actual name.
            if let Some(id) = utils::child_by_kind(&child, "identifier") {
                names.push(utils::node_text(&id, source).to_string());
            } else {
                // Fallback: use source text of the specifier.
                names.push(utils::node_text(&child, source).to_string());
            }
        }
    }
    names
}

/// Extract callee name from a call_expression.
fn extract_callee_name(node: &Node, source: &[u8]) -> String {
    let func = node.child_by_field_name("function");
    match func {
        Some(f) => collect_call_name(&f, source),
        None => String::new(),
    }
}

/// Recursively collect a dotted/member expression name (e.g. console.log, obj.method).
fn collect_call_name(node: &Node, source: &[u8]) -> String {
    match node.kind() {
        "member_expression" => {
            let obj = node.child_by_field_name("object");
            let prop = node.child_by_field_name("property");
            let obj_name = obj
                .as_ref()
                .map(|o| collect_call_name(o, source))
                .unwrap_or_default();
            let prop_name = prop
                .as_ref()
                .map(|p| utils::node_text(p, source).to_string())
                .unwrap_or_default();
            if obj_name.is_empty() {
                prop_name
            } else {
                format!("{obj_name}.{prop_name}")
            }
        }
        "identifier" | "property_identifier" => utils::node_text(node, source).to_string(),
        "this" | "super" => utils::node_text(node, source).to_string(),
        _ => utils::node_text(node, source).to_string(),
    }
}

/// Extract parameters from a function/method/arrow_function.
fn extract_params(node: &Node, source: &[u8]) -> Vec<ParameterIR> {
    let params_node = node.child_by_field_name("parameters");
    let params_node = match params_node {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut params = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.named_children(&mut cursor) {
        let name = match child.kind() {
            "identifier" | "property_identifier" => utils::node_text(&child, source).to_string(),
            "assignment_pattern" => {
                // param = default
                child
                    .child_by_field_name("left")
                    .map(|n| utils::node_text(&n, source).to_string())
                    .unwrap_or_default()
            }
            "rest_pattern" => child
                .child_by_field_name("pattern")
                .or_else(|| child.child(0))
                .map(|n| utils::node_text(&n, source).to_string())
                .unwrap_or_else(|| "...".to_string()),
            "object_pattern" | "array_pattern" => {
                // Destructured — use a placeholder.
                utils::node_text(&child, source).to_string()
            }
            _ => utils::node_text(&child, source).to_string(),
        };
        if !name.is_empty() {
            params.push(ParameterIR {
                name,
                type_annotation: None,
                default_value: None,
            });
        }
    }
    params
}

/// Build a human-readable signature.
fn build_sig(node: &Node, source: &[u8], name: &str) -> Option<String> {
    let params_node = node.child_by_field_name("parameters");
    let params_text = params_node
        .as_ref()
        .map(|p| utils::node_text(p, source))
        .unwrap_or("()");
    Some(format!("function {name}{params_text}"))
}

/// Extract JSDoc comment preceding a node.
fn extract_jsdoc(node: &Node, source: &[u8]) -> Option<String> {
    let mut prev = node.prev_sibling();
    let mut doc_lines = Vec::new();
    while let Some(p) = prev {
        match p.kind() {
            "comment" => {
                let text = utils::node_text(&p, source);
                if text.starts_with("/**") && text.ends_with("*/") {
                    let inner = text
                        .strip_prefix("/**")
                        .and_then(|t| t.strip_suffix("*/"))
                        .unwrap_or(text);
                    for line in inner.lines() {
                        let trimmed = line.trim().strip_prefix('*').unwrap_or(line).trim();
                        doc_lines.push(trimmed.to_string());
                    }
                    break;
                } else if text.starts_with("//") {
                    // Regular comment — stop, doc comments must be JSDoc block.
                    break;
                }
                // Block comment without /** — stop.
                if text.starts_with("/*") {
                    break;
                }
            }
            _ => break,
        }
        prev = p.prev_sibling();
    }
    doc_lines.reverse();
    if doc_lines.is_empty() {
        None
    } else {
        Some(doc_lines.join("\n"))
    }
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
        // 3. Dotted names (console.log) — try to resolve the first part.
        if let Some(dot_pos) = call.callee_name.find('.') {
            let first_part = &call.callee_name[..dot_pos];
            if let Some(key) = by_name.get(first_part) {
                call.callee_local_key = Some(key.clone());
                call.callee_external = false;
                continue;
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

/// File-level test detection (ts/js): .test. / .spec. files, __tests__ dirs.
fn is_test_file_path(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.contains(".test.")
        || base.contains(".spec.")
        || path.contains("/__tests__/")
        || path.contains("/test/")
        || path.contains("/tests/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_parser_ir::DiagnosticSeverity;

    fn parse_js(source: &[u8], path: &str) -> FileParseIR {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let ir = JavaScriptExtractor.extract(&tree, source, path);
        let mut ir = ir;
        ir.metrics =
            crate::metrics::compute(&tree, source, crate::metrics::comment_kinds(&ir.language));
        ir.ir_version = 4;
        ir
    }

    // ── Golden fixture tests ─────────────────────────────────────────

    const SIMPLE_JS: &str = r#"import { add } from './math.js';
import greet from './hello.js';

/**
 * A simple calculator class.
 */
class Calculator {
    constructor(initial) {
        this.value = initial;
    }

    add(n) {
        this.value += n;
    }
}

function multiply(a, b) {
    return a * b;
}

const divide = (a, b) => a / b;

const calc = new Calculator(10);
calc.add(5);
multiply(2, 3);
divide(10, 2);
console.log("done");
"#;

    #[test]
    fn test_symbols() {
        let ir = parse_js(SIMPLE_JS.as_bytes(), "fixtures/javascript/simple.js");

        assert_eq!(ir.language, "JavaScript");

        // Class.
        let calc_class = ir
            .symbols
            .iter()
            .find(|s| s.name == "Calculator")
            .expect("Calculator");
        assert_eq!(calc_class.kind, SymbolKind::Class);
        assert_eq!(calc_class.start_line, 7);

        // Constructor method.
        let ctor = ir
            .symbols
            .iter()
            .find(|s| s.local_key == "Calculator::constructor")
            .expect("constructor");
        assert_eq!(ctor.kind, SymbolKind::Method);
        assert_eq!(ctor.parameters.len(), 1);
        assert_eq!(ctor.parameters[0].name, "initial");

        // Method.
        let add_method = ir
            .symbols
            .iter()
            .find(|s| s.local_key == "Calculator::add")
            .expect("add method");
        assert_eq!(add_method.kind, SymbolKind::Method);
        assert_eq!(add_method.parameters[0].name, "n");

        // Function.
        let multiply = ir
            .symbols
            .iter()
            .find(|s| s.name == "multiply")
            .expect("multiply");
        assert_eq!(multiply.kind, SymbolKind::Function);
        assert_eq!(multiply.parameters.len(), 2);

        // Arrow function (const divide).
        let divide = ir
            .symbols
            .iter()
            .find(|s| s.name == "divide")
            .expect("divide");
        assert_eq!(divide.kind, SymbolKind::Function);
        assert_eq!(divide.parameters.len(), 2);
    }

    #[test]
    fn test_calls() {
        // Use inline source with function body so top-level calls don't get skipped.
        let src = b"function foo() { bar(); baz.qux(); new Thing(); } function bar() {}";
        let ir = parse_js(src, "calls.js");

        // Calls inside function body should be captured.
        let foo_calls: Vec<_> = ir
            .calls
            .iter()
            .filter(|c| c.caller_local_key == "foo")
            .collect();
        assert!(foo_calls.len() >= 1, "expected calls from foo");

        let bar = ir
            .calls
            .iter()
            .find(|c| c.callee_name == "bar")
            .expect("bar");
        assert_eq!(bar.line, 1);

        // baz.qux — dotted method call.
        assert!(ir.calls.iter().any(|c| c.callee_name == "baz.qux"));
    }

    #[test]
    fn test_imports() {
        let ir = parse_js(SIMPLE_JS.as_bytes(), "fixtures/javascript/simple.js");

        // Named import: { add } from './math.js'
        let named = ir
            .imports
            .iter()
            .find(|i| i.import_name == "add")
            .expect("add import");
        assert_eq!(named.kind, ImportKind::Named);
        assert_eq!(named.target_module, "./math.js");
        assert_eq!(named.line, Some(1));

        // Default import: greet from './hello.js'
        let def = ir
            .imports
            .iter()
            .find(|i| i.import_name == "greet")
            .expect("greet import");
        assert_eq!(def.kind, ImportKind::Default);
        assert_eq!(def.target_module, "./hello.js");
        assert_eq!(def.line, Some(2));
    }

    #[test]
    fn test_in_file_resolve() {
        // Use inline source with function body so calls are captured.
        let src = b"function foo() { bar(); } function bar() {}";
        let ir = parse_js(src, "resolve.js");

        let bar_call = ir
            .calls
            .iter()
            .find(|c| c.callee_name == "bar")
            .expect("bar call");
        assert_eq!(bar_call.callee_local_key.as_deref(), Some("bar"));
        assert!(!bar_call.callee_external);
    }

    // ── Edge case tests ─────────────────────────────────────────────

    #[test]
    fn test_empty_file() {
        let ir = parse_js(b"", "empty.js");
        assert!(ir.symbols.is_empty());
        assert!(ir.calls.is_empty());
    }

    #[test]
    fn test_anonymous_function() {
        let src = b"const x = function(a, b) { return a + b; };";
        let _ir = parse_js(src, "anon.js");
        // Anonymous functions are skipped as symbols (no name).
        // But the variable declarator should still pick up the arrow/function.
        // In this case it's a regular function expression, not arrow — so variable becomes the name.
    }

    #[test]
    fn test_require_call_not_treated_as_import() {
        let src = b"function init() { const fs = require('fs'); }";
        let ir = parse_js(src, "require.js");
        // require() is a call_expression, not import_statement. It should appear as a call.
        let req_call = ir
            .calls
            .iter()
            .find(|c| c.callee_name == "require")
            .expect("require call");
        assert_eq!(req_call.line, 1);
        // Not tracked as import.
        assert!(ir.imports.is_empty());
    }

    #[test]
    fn test_namespace_import() {
        let src = br#"import * as utils from './utils.js';"#;
        let ir = parse_js(src, "ns.js");
        let imp = ir
            .imports
            .iter()
            .find(|i| i.import_name == "utils")
            .expect("namespace import");
        assert_eq!(imp.kind, ImportKind::Star);
        assert_eq!(imp.target_module, "./utils.js");
    }

    #[test]
    fn test_jsdoc_extracted() {
        let ir = parse_js(SIMPLE_JS.as_bytes(), "fixtures/javascript/simple.js");
        let calc = ir
            .symbols
            .iter()
            .find(|s| s.name == "Calculator")
            .expect("Calculator");
        assert!(calc
            .docstring
            .as_ref()
            .map_or(false, |d| d.contains("simple calculator")));
    }

    #[test]
    fn test_content_hash_stable() {
        let a = parse_js(SIMPLE_JS.as_bytes(), "t.js");
        let b = parse_js(SIMPLE_JS.as_bytes(), "t.js");
        assert_eq!(a.content_hash, b.content_hash);
    }

    #[test]
    fn test_function_signature_includes_name() {
        let ir = parse_js(SIMPLE_JS.as_bytes(), "fixtures/javascript/simple.js");
        let multiply = ir
            .symbols
            .iter()
            .find(|s| s.name == "multiply")
            .expect("multiply");
        assert!(multiply
            .signature
            .as_ref()
            .map_or(false, |s| s.contains("function multiply")));
    }

    #[test]
    fn test_method_inside_class_qualified() {
        let ir = parse_js(SIMPLE_JS.as_bytes(), "fixtures/javascript/simple.js");
        let add_method = ir
            .symbols
            .iter()
            .find(|s| s.qualified_name == "Calculator::add")
            .expect("Calculator::add");
        assert_eq!(add_method.kind, SymbolKind::Method);
        assert_eq!(add_method.name, "add");
    }

    #[test]
    fn golden_ir_matches_fixture() {
        use crate::extractors::golden;

        let source = include_str!("../../../../fixtures/javascript/simple.js");
        let actual_ir = parse_js(source.as_bytes(), "fixtures/javascript/simple.js");
        let expected_json = include_str!("../../../../fixtures/javascript/simple.ir.json");
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
            "Golden IR mismatch for JavaScript"
        );
    }

    #[test]
    fn is_test_flag_detects_test_filenames() {
        let src = "export function add(a, b) { return a + b; }";
        let normal = parse_js(src.as_bytes(), "src/math.js");
        assert!(!normal.symbols[0].is_test);
        let test_file = parse_js(src.as_bytes(), "src/math.test.js");
        assert!(test_file.symbols[0].is_test, ".test.js file must be test");
        let tests_dir = parse_js(src.as_bytes(), "src/__tests__/math.js");
        assert!(tests_dir.symbols[0].is_test, "__tests__ dir must be test");
    }

    #[test]
    fn broken_input_emits_diagnostics() {
        let broken = b"export function fine() { return 1; }\nexport function broken( {\n  const x = ;\n}\n";
        let ir = parse_js(broken, "broken.js");
        assert!(!ir.diagnostics.is_empty(), "broken JS must emit diagnostics");
        assert!(
            ir.diagnostics
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Error),
            "expected an Error diagnostic: {:?}",
            ir.diagnostics
        );
        assert!(ir.symbols.iter().any(|s| s.name == "fine"));
    }
}
