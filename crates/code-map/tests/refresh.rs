//! refresh tests — real parse of a tiny temp repo, JSONL validity, and the
//! atomic-write failure path (destination untouched on parse failure).

use std::fs;
use std::path::{Path, PathBuf};

use code_map::refresh;
use code_parser_ir::FileParseIR;

fn temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("code-map-test-{}-{}", std::process::id(), label))
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn refresh_writes_valid_relative_jsonl() {
    let dir = temp_dir("refresh-ok");
    let out = dir.join("code-map.jsonl");
    write(&dir.join("src/a.rs"), "fn alpha() {}\n");
    write(&dir.join("src/b.rs"), "fn beta() {\n    alpha();\n}\n");

    let n = refresh::run(&dir, &out, None).expect("refresh succeeds");
    assert_eq!(n, 2);

    let text = fs::read_to_string(&out).unwrap();
    let mut paths = Vec::new();
    for line in text.lines() {
        let ir: FileParseIR = serde_json::from_str(line).expect("valid IR line");
        assert_eq!(ir.ir_version, 4);
        assert!(ir.path.starts_with("src/"), "repo-relative: {}", ir.path);
        assert!(!ir.retrieval_card.text.is_empty(), "cards always present");
        paths.push(ir.path);
    }
    paths.sort();
    assert_eq!(paths, vec!["src/a.rs", "src/b.rs"]);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn refresh_failure_leaves_previous_map_intact() {
    let dir = temp_dir("refresh-fail");
    fs::create_dir_all(&dir).unwrap();
    let out = dir.join("code-map.jsonl");
    fs::write(&out, "previous map\n").unwrap();

    // Nonexistent source dir → parse_repo errors before any write.
    let missing = dir.join("does-not-exist");
    assert!(refresh::run(&missing, &out, None).is_err());

    assert_eq!(fs::read_to_string(&out).unwrap(), "previous map\n");
    assert!(
        !dir.join("code-map.jsonl.tmp").exists(),
        "no partial temp file left behind"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn refresh_filters_languages() {
    use code_map::refresh::parse_languages;

    assert_eq!(parse_languages("rust").unwrap().len(), 1);
    assert_eq!(parse_languages("rust,typescript").unwrap().len(), 2);
    assert_eq!(parse_languages("py, js").unwrap().len(), 2);
    assert!(parse_languages("cobol").is_none());
    assert!(parse_languages("").is_none());
}
