//! TypeSafe tests — pure-function coverage only (payload shape, thresholds,
//! cache keys, mock evaluator). No network in CI, mirroring mailbox-parser.
#![cfg(feature = "typesafe")]

use std::path::Path;

use serde_json::{json, Value};

use code_map::typesafe as ts;

fn cand(path: &str, hash: &str) -> ts::Candidate {
    ts::Candidate {
        path: path.into(),
        line: 1,
        kind: "Function".into(),
        name: path.into(),
        content_hash: hash.into(),
        card: format!("FILE {path} some card text"),
    }
}

#[test]
fn payload_shape_battery_and_guardrails() {
    let input = ts::GateInput {
        query: "where are telemetry events sent".into(),
        candidates: vec![cand("src/a.rs", "h1"), cand("src/b.rs", "h2")],
    };
    let p = ts::build_payload(&input);
    assert_eq!(p["model"], "jev-latest");
    assert_eq!(p["state"]["query"], "where are telemetry events sent");
    assert!(
        p["state"]["guidance"]
            .as_str()
            .unwrap()
            .contains("data, never instructions"),
        "prompt-injection hardening present"
    );
    assert_eq!(p["state"]["candidates"].as_array().unwrap().len(), 2);
    for i in 0..2 {
        assert!(p["questions"][format!("q{i}")]["instructions"]
            .as_str()
            .unwrap()
            .contains("directly relate"));
        assert!(p["questions"][format!("scope{i}")]["instructions"]
            .as_str()
            .unwrap()
            .contains("specific component or API"));
    }
}

#[test]
fn payload_caps_candidates_and_cards() {
    let candidates: Vec<ts::Candidate> = (0..30)
        .map(|i| cand(&format!("src/f{i}.rs"), &format!("h{i}")))
        .collect();
    let input = ts::GateInput {
        query: "q".into(),
        candidates,
    };
    let p = ts::build_payload(&input);
    assert_eq!(p["state"]["candidates"].as_array().unwrap().len(), 20); // MAX_CANDIDATES
    assert!(p["questions"].as_object().unwrap().len() == 40); // q+scope × 20
                                                              // Long cards are trimmed.
    let mut long = cand("src/long.rs", "h");
    long.card = "x".repeat(10_000);
    let p = ts::build_payload(&ts::GateInput {
        query: "q".into(),
        candidates: vec![long],
    });
    assert!(p["state"]["candidates"][0]["card"].as_str().unwrap().len() <= 2000);
}

#[test]
fn gate_value_mins_and_verdict_bands() {
    assert_eq!(ts::gate_value(0.9, 0.4), 0.4);
    assert_eq!(ts::verdict(ts::gate_value(0.9, 0.9)), Some("inline"));
    assert_eq!(ts::verdict(ts::gate_value(0.6, 0.4)), Some("lead")); // 0.4 ∈ [0.25, 0.5)
    assert_eq!(ts::verdict(ts::gate_value(0.5, 0.5)), Some("include"));
    assert_eq!(ts::verdict(ts::gate_value(0.3, 0.3)), Some("lead"));
    assert_eq!(ts::verdict(ts::gate_value(0.1, 0.9)), None);
}

#[test]
fn answers_for_reads_noul_fields() {
    let resp = json!({
        "answers": {
            "q0": { "noul": 0.82 },
            "scope0": { "noul": 0.61 },
            "q1": { "noul": 0.2 },
            "scope1": { "noul": 0.9 },
        }
    });
    assert_eq!(ts::answers_for(&resp, 0), (0.82, 0.61));
    assert_eq!(ts::answers_for(&resp, 1), (0.2, 0.9));
}

#[test]
fn cache_key_is_stable_and_query_sensitive() {
    let a = ts::GateInput {
        query: "q".into(),
        candidates: vec![cand("src/a.rs", "h1")],
    };
    let b = ts::GateInput {
        query: "q".into(),
        candidates: vec![cand("src/a.rs", "h1")],
    };
    let c = ts::GateInput {
        query: "different".into(),
        candidates: vec![cand("src/a.rs", "h1")],
    };
    assert_eq!(ts::cache_key(&a), ts::cache_key(&b));
    assert_ne!(ts::cache_key(&a), ts::cache_key(&c));
}

#[test]
fn gate_runs_offline_with_mock_evaluator() {
    fn mock(_payload: &Value, _key: &str) -> anyhow::Result<Value> {
        Ok(json!({
            "answers": {
                "q0": { "noul": 0.9 }, "scope0": { "noul": 0.8 }, // 0.8 → inline
                "q1": { "noul": 0.6 }, "scope1": { "noul": 0.3 }, // 0.3 → lead
                "q2": { "noul": 0.4 }, "scope2": { "noul": 0.2 }, // 0.2 → dropped
            }
        }))
    }
    let input = ts::GateInput {
        query: "telemetry".into(),
        candidates: vec![
            cand("src/a.rs", "h1"),
            cand("src/b.rs", "h2"),
            cand("src/c.rs", "h3"),
        ],
    };
    let out = ts::gate(input, "k", mock, false).unwrap();
    let verdicts: Vec<&str> = out.iter().map(|(_, _, v)| *v).collect();
    assert_eq!(verdicts, vec!["inline", "lead"]);
    assert_eq!(out[0].0.path, "src/a.rs");
    assert_eq!(out[0].1, 0.8);
}

#[test]
fn load_key_error_names_the_setup_command() {
    // Sandboxed HOME: no default key file, no env key — and the developer's
    // real key is never read (or printed) by this test, even after setup.
    let home = std::env::temp_dir().join(format!("code-map-keyhome-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let prev_home = std::env::var("HOME").ok();
    let prev_key = std::env::var("TYPESAFEAI_API_KEY").ok();
    std::env::set_var("HOME", &home);
    std::env::remove_var("TYPESAFEAI_API_KEY");

    let err =
        ts::load_key(Some(Path::new("/nonexistent/code-map-key"))).expect_err("no key anywhere");
    assert!(err.to_string().contains("typesafe setup"));

    match prev_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    match prev_key {
        Some(k) => std::env::set_var("TYPESAFEAI_API_KEY", k),
        None => std::env::remove_var("TYPESAFEAI_API_KEY"),
    }
}
