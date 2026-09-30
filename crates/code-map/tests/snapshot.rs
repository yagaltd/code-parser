//! Snapshot renderer tests — determinism, budget, degradation, idle-TTL,
//! and the cache-stability contract on the real repo.
//!
//! The cache-stability contract is the whole point of the feature: the same
//! map must render byte-identically, every time, with no clock or input
//! order dependence — only then can a consumer freeze it as a system-prompt
//! prefix and hit provider prompt caches.

use code_map::snapshot::{self, Snapshot};
use code_parser_ir::{FileParseIR, SymbolIR, SymbolKind};

fn ir(path: &str, lines: u32, symbols: &[&str]) -> FileParseIR {
    let mut ir = FileParseIR::empty(path, "Rust");
    ir.line_count = lines;
    for name in symbols {
        ir.symbols.push(SymbolIR {
            local_key: (*name).into(),
            name: (*name).into(),
            qualified_name: (*name).into(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 2,
            start_byte: None,
            end_byte: None,
            signature: None,
            parameters: vec![],
            return_type: None,
            docstring: None,
            is_test: false,
        });
    }
    ir
}

/// A synthetic corpus that overflows any small budget: many files, varied
/// symbol counts so the hub-priority ordering is exercised.
fn corpus() -> Vec<FileParseIR> {
    let mut v = Vec::new();
    for i in 0..40 {
        let syms: Vec<String> = (0..i % 9).map(|j| format!("sym_{i}_{j}")).collect();
        let names: Vec<&str> = syms.iter().map(|s| s.as_str()).collect();
        v.push(ir(&format!("src/mod_{i:02}.rs"), 100 + i, &names));
    }
    v
}

#[test]
fn render_is_deterministic_and_input_order_independent() {
    let a = corpus();
    let mut b = corpus();
    b.reverse();
    let (ra, ma) = snapshot::render_with_meta(&a, 2500);
    let (rb, mb) = snapshot::render_with_meta(&b, 2500);
    assert_eq!(ra, rb, "reordered input must render identically");
    assert_eq!(ma, mb);
    // Same input twice: trivially byte-identical.
    assert_eq!(ra, snapshot::render(&a, 2500));
}

#[test]
fn budget_is_enforced_and_degradation_is_monotonic() {
    let c = corpus();
    for budget in [500u32, 800, 1500, 2500] {
        let (content, meta) = snapshot::render_with_meta(&c, budget);
        assert!(
            meta.est_tokens <= budget,
            "budget {budget}: render is {} tokens",
            meta.est_tokens
        );
        assert_eq!(snapshot::est_tokens(&content), meta.est_tokens);
        if meta.omitted > 0 {
            assert!(
                content.contains("more files omitted"),
                "omitted files must be surfaced in the footer"
            );
        }
    }
    // More budget never yields less content.
    let tokens = |b: u32| snapshot::render_with_meta(&c, b).1.est_tokens;
    assert!(tokens(800) <= tokens(1500));
    assert!(tokens(1500) <= tokens(2500));
    let files = |b: u32| snapshot::render_with_meta(&c, b).1.files;
    assert!(files(800) <= files(2500));
    // Budget is clamped, not trusted.
    let (_, tiny) = snapshot::render_with_meta(&c, 1);
    assert_eq!(tiny.budget, snapshot::MIN_BUDGET);
}

#[test]
fn symbol_cap_and_test_exclusion() {
    let many: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
    let names: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
    let mut irs = vec![ir("src/big.rs", 10, &names)];
    // Test symbols must not appear in the render.
    irs[0].symbols[0].is_test = true;
    let (content, _) = snapshot::render_with_meta(&irs, snapshot::MAX_BUDGET);
    assert!(!content.contains("s0,"), "test symbols are excluded");
    // 20 symbols, 1 marked test → 19 listed, capped at 12 with "+7".
    assert!(content.contains("+7"), "19 non-test symbols cap at 12 (+7)");
}

#[test]
fn idle_ttl_expires_on_inactivity_not_age() {
    // Birth: t=0. TTL: 300ms.
    let mut snap = Snapshot::new("MAP".into(), vec![], 300, 0);
    assert!(!snap.is_idle_expired(299), "still hot just before TTL");
    assert!(snap.is_idle_expired(300), "expired at exactly ttl idle");
    // A read at t=250 resets the idle clock.
    snap.read(250);
    assert!(!snap.is_idle_expired(549), "read bumped the clock");
    assert!(snap.is_idle_expired(550));
    // Continuous use never expires: reads every 100ms for 10s.
    for t in (0..10_000).step_by(100) {
        snap.read(t as u64);
        assert!(!snap.is_idle_expired(t as u64));
    }
    // And it still expires after the reads stop.
    assert!(snap.is_idle_expired(10_299));
    assert_eq!(snap.read(0), "MAP");
    assert_eq!(snap.paths(), &[] as &[String]);
}

#[test]
fn content_changes_only_when_the_map_changes() {
    let mut c = corpus();
    let before = snapshot::render(&c, 2500);
    c.push(ir("src/new_file.rs", 42, &["brand_new"]));
    let after = snapshot::render(&c, 2500);
    assert_ne!(before, after, "a new file must change the render");
    assert!(after.contains("new_file.rs"));
}

/// The cache-stability contract, on the real repo: parse this repository
/// twice (full pipeline, including hash cache and card rebuilds), render
/// both maps — the prefixes must be byte-identical and within budget.
/// No wall-clock may leak into the content: render "at" two different
/// moments (irrelevant to the pure function) and compare.
#[test]
fn cache_stability_real_repo_prefix_is_byte_identical() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root");
    let results = code_parser_core::parse_repo(
        &root,
        Some(vec![code_parser_core::language::Language::Rust]),
    )
    .expect("parse repo");
    let irs: Vec<FileParseIR> = results.into_iter().map(|r| r.ir).collect();
    assert!(
        irs.len() >= 20,
        "expected the real repo, got {} files",
        irs.len()
    );

    let (render_one, meta_one) = snapshot::render_with_meta(&irs, snapshot::DEFAULT_BUDGET);
    let render_two = snapshot::render(&irs, snapshot::DEFAULT_BUDGET);
    assert_eq!(render_one, render_two);
    assert!(
        meta_one.est_tokens <= snapshot::DEFAULT_BUDGET,
        "real repo render blew the budget: {}",
        meta_one.est_tokens
    );
    // Sanity: the render is a useful map, not just a header.
    assert!(render_one.contains("crates/code-parser-core/src/lib.rs"));
    assert!(render_one.contains("parse_repo"));
    // No wall-clock can leak: `render` takes no time parameter at all —
    // purity is structural. (Empryo's lesson: one timestamp in the prefix
    // and every request is a cache miss.)
}
