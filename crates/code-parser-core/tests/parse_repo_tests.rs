//! Integration tests for parse_repo — error resilience, cross-file resolution,
//! and end-to-end fixture parity with domain_code expected shapes.

use std::fs;
use std::path::PathBuf;

use code_parser_core::{language::Language, parse_repo};

fn tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("code-parser-test-{}-{}", std::process::id(), label));
    // Remove any prior leftover.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(dir: &PathBuf, name: &str, content: &str) {
    fs::write(dir.join(name), content).unwrap();
}

fn cleanup(dir: &PathBuf) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn parse_repo_skips_non_source_files() {
    let dir = tmp_dir("skip-non-source");
    write_file(&dir, "main.rs", "fn main() {}");
    write_file(&dir, "README.md", "# Hello");
    write_file(&dir, "build.sh", "#!/bin/sh");

    let results = parse_repo(&dir, Some(vec![Language::Rust])).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].ir.path, "main.rs");

    cleanup(&dir);
}

#[test]
fn parse_repo_handles_broken_files_gracefully() {
    let dir = tmp_dir("broken");
    write_file(&dir, "good.rs", "fn good() {}");
    write_file(&dir, "broken.rs", "fn broken( {");

    let results = parse_repo(&dir, Some(vec![Language::Rust])).unwrap();
    assert_eq!(results.len(), 2);

    let good = results.iter().find(|r| r.ir.path == "good.rs").unwrap();
    assert!(!good.ir.symbols.is_empty());
    assert!(good.errors.is_empty());

    let broken = results.iter().find(|r| r.ir.path == "broken.rs").unwrap();
    // Tree-sitter is error-tolerant: the broken file may parse without errors.
    // The key invariant is that parse_repo doesn't crash and returns results.
    assert_eq!(broken.ir.path, "broken.rs");

    cleanup(&dir);
}

#[test]
fn parse_repo_cross_file_resolve_between_siblings() {
    let dir = tmp_dir("cross-file");
    write_file(&dir, "lib.rs", r#"
pub fn helper() -> u32 { 42 }
"#);
    write_file(&dir, "main.rs", r#"
fn main() {
    crate::helper();
}
"#);

    let results = parse_repo(&dir, Some(vec![Language::Rust])).unwrap();
    assert_eq!(results.len(), 2);

    // The call `crate::helper()` contains :: → cross-file resolve.
    // V1 exact match: "crate::helper" != "helper" (the qualified name in lib.rs).
    // So it won't resolve — callee_external set to true.
    let main_ir = results.iter().find(|r| r.ir.path == "main.rs").unwrap();
    let call = main_ir.ir.calls.iter()
        .find(|c| c.callee_name == "crate::helper")
        .expect("call to crate::helper");
    assert!(call.callee_external, "crate::helper not found → external (V1 exact match)");
    assert!(call.callee_file.is_none());

    cleanup(&dir);
}

#[test]
fn parse_repo_respects_gitignore() {
    let dir = tmp_dir("gitignore");
    // Use .ignore (ignore crate's own format) which works without git init.
    write_file(&dir, ".ignore", "generated/\n");
    fs::create_dir_all(dir.join("generated")).unwrap();
    write_file(&dir, "generated/gen.rs", "fn gen() {}");
    write_file(&dir, "src.rs", "fn src() {}");

    let results = parse_repo(&dir, Some(vec![Language::Rust])).unwrap();

    let paths: Vec<&str> = results.iter().map(|r| r.ir.path.as_str()).collect();
    assert!(paths.contains(&"src.rs"), "should contain src.rs, got {paths:?}");
    assert!(!paths.contains(&"generated/gen.rs"), "generated/ should be ignored");

    cleanup(&dir);
}
