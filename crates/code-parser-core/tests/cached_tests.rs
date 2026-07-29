//! Integration tests for cached parsing and hash short-circuit.

use std::fs;
use std::path::PathBuf;

use code_parser_core::HashCache;

fn tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("code-parser-test-{}-{}", std::process::id(), label));
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
fn parse_file_cached_skips_unchanged() {
    let dir = tmp_dir("cache-skip");
    let path = dir.join("main.rs");
    write_file(&dir, "main.rs", "fn main() {}");

    let mut cache = HashCache::new();

    // First parse — should parse.
    let r1 = code_parser_core::parse_file_cached(&path, &mut cache).unwrap();
    assert!(r1.is_some(), "first parse should produce result");

    // Second parse — same content, should skip.
    let r2 = code_parser_core::parse_file_cached(&path, &mut cache).unwrap();
    assert!(r2.is_none(), "unchanged file should be skipped");

    cleanup(&dir);
}

#[test]
fn parse_file_cached_reparses_on_change() {
    let dir = tmp_dir("cache-change");
    let path = dir.join("lib.rs");
    write_file(&dir, "lib.rs", "fn old() {}");

    let mut cache = HashCache::new();

    // First parse.
    let r1 = code_parser_core::parse_file_cached(&path, &mut cache).unwrap();
    assert!(r1.is_some());

    // Modify file.
    write_file(&dir, "lib.rs", "fn new() {}");

    // Should re-parse.
    let r2 = code_parser_core::parse_file_cached(&path, &mut cache).unwrap();
    assert!(r2.is_some(), "changed file should re-parse");
    assert_eq!(r2.unwrap().ir.symbols[0].name, "new");

    cleanup(&dir);
}

#[test]
fn parse_file_cached_tracks_multiple_files() {
    let dir = tmp_dir("cache-multi");
    let path_a = dir.join("a.rs");
    let path_b = dir.join("b.rs");
    write_file(&dir, "a.rs", "fn a() {}");
    write_file(&dir, "b.rs", "fn b() {}");

    let mut cache = HashCache::new();

    let ra = code_parser_core::parse_file_cached(&path_a, &mut cache).unwrap();
    assert!(ra.is_some());
    let rb = code_parser_core::parse_file_cached(&path_b, &mut cache).unwrap();
    assert!(rb.is_some());

    // Both cached.
    let ra2 = code_parser_core::parse_file_cached(&path_a, &mut cache).unwrap();
    assert!(ra2.is_none());
    let rb2 = code_parser_core::parse_file_cached(&path_b, &mut cache).unwrap();
    assert!(rb2.is_none());

    // Change only a.
    write_file(&dir, "a.rs", "fn a_new() {}");
    let ra3 = code_parser_core::parse_file_cached(&path_a, &mut cache).unwrap();
    assert!(ra3.is_some());
    // b still skipped.
    let rb3 = code_parser_core::parse_file_cached(&path_b, &mut cache).unwrap();
    assert!(rb3.is_none());

    cleanup(&dir);
}
