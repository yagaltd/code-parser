//! Learning loop (mailbox-parser's improve/mine pattern, for retrieval).
//!
//! - Every fresh gate run appends a **trail** row (query, candidate terms,
//!   gate values) to `~/.cache/code-parser/trails.jsonl`.
//! - The consumer reports what it actually used via `code-map mark`
//!   (appended to `usage.jsonl`) — the ground truth the gate can't see.
//! - `code-map learn` folds trails × usage into a **lexicon** of per-term
//!   promote/demote boosts, with mailbox guardrails: holdout split (default
//!   0.4), min-samples before a rule is measured, holdout flip-check (a rule
//!   that inverts on the holdout is dropped), unmeasured terms stay neutral.
//! - The lexicon is consumed by default by `search`: deterministic boosts,
//!   zero API — the TypeSafe gate only judges what the lexicon can't settle.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

// ── Guardrail constants (mailbox-parser defaults) ────────────────────────

/// Fraction of sample groups held out to verify learned rules.
pub const HOLDOUT_FRAC: f64 = 0.4;
/// A term needs at least this many labeled candidates before it's measured.
pub const MIN_SAMPLES: usize = 5;
/// Learn-split precision at or above this → promote candidate.
pub const PROMOTE_PRECISION: f64 = 0.7;
/// Learn-split precision at or below this → demote candidate.
pub const DEMOTE_PRECISION: f64 = 0.3;
/// Score bonus/malus a measured rule contributes (additive).
pub const BOOST: f64 = 0.2;

pub const LEXICON_VERSION: u32 = 1;

// ── Paths ────────────────────────────────────────────────────────────────

pub fn cache_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| Path::new(&h).join(".cache/code-parser"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/code-parser-cache"))
}

pub fn trails_path() -> PathBuf {
    cache_dir().join("trails.jsonl")
}

pub fn usage_path() -> PathBuf {
    cache_dir().join("usage.jsonl")
}

pub fn default_lexicon_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| Path::new(&h).join(".config/code-parser/learned/gate-lexicon.json"))
        .unwrap_or_else(|_| PathBuf::from("/tmp/code-parser-gate-lexicon.json"))
}

fn append_jsonl(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{value}")?;
    Ok(())
}

/// Normalize a query for joining trails and usage rows.
pub fn norm_query(q: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for w in q.split_whitespace() {
        for t in w.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '_') {
            if !t.is_empty() {
                parts.push(t.to_string());
            }
        }
    }
    parts.join(" ")
}

// ── Trail (gate → disk) ──────────────────────────────────────────────────

/// Record one fresh gate run. `candidates` = (path, card/name terms).
pub fn append_trail(query: &str, candidates: &[(String, Vec<String>)], values: &[f64]) -> Result<()> {
    let row = json!({
        "query": query,
        "candidates": candidates
            .iter()
            .map(|(path, terms)| json!({"path": path, "terms": terms}))
            .collect::<Vec<_>>(),
        "values": values,
    });
    append_jsonl(&trails_path(), &row)
}

// ── Usage feed (consumer → disk) ─────────────────────────────────────────

/// Record which candidates the consumer actually used for a query.
pub fn mark_used(query: &str, paths: &[String]) -> Result<()> {
    append_jsonl(
        &usage_path(),
        &json!({"query": query, "used": paths}),
    )
}

// ── Lexicon ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Lexicon {
    pub version: u32,
    /// term → weight (currently always 1.0; reserved for precision-scaled).
    #[serde(default)]
    pub promote: BTreeMap<String, f64>,
    #[serde(default)]
    pub demote: BTreeMap<String, f64>,
    #[serde(default)]
    pub stats: BTreeMap<String, u64>,
}

impl Lexicon {
    pub fn load(path: &Path) -> Result<Option<Lexicon>> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(None);
        };
        let lex = serde_json::from_str(&text).with_context(|| format!("bad lexicon {}", path.display()))?;
        Ok(Some(lex))
    }

    /// Default location; missing file = no lexicon (silent).
    pub fn load_default() -> Option<Lexicon> {
        Self::load(&default_lexicon_path()).unwrap_or(None)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Additive score adjustment for a unit's term list, clamped to ±BOOST.
    pub fn boost_for(&self, terms: &[String]) -> f64 {
        let mut b = 0.0;
        for t in terms {
            if let Some(w) = self.promote.get(t) {
                b += BOOST * w;
            }
            if let Some(w) = self.demote.get(t) {
                b -= BOOST * w;
            }
        }
        b.clamp(-BOOST, BOOST)
    }
}

// ── Learn (trails × usage → lexicon) ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TrailRow {
    query: String,
    candidates: Vec<TrailCandidate>,
    values: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct TrailCandidate {
    path: String,
    #[serde(default)]
    terms: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UsageRow {
    query: String,
    used: Vec<String>,
}

struct Group {
    /// (path, terms, used)
    candidates: Vec<(String, Vec<String>, bool)>,
}

/// Fold trails × usage into a lexicon. Pure given the file contents —
/// the CLI resolves paths, tests pass temp files.
pub fn learn(trails: &Path, usage: &Path, holdout_frac: f64, min_samples: usize) -> Result<Lexicon> {
    // Load + join by normalized query.
    let mut usage_by_query: HashMap<String, Vec<String>> = HashMap::new();
    for line in read_lines(usage)? {
        let row: UsageRow = serde_json::from_str(&line).context("bad usage row")?;
        usage_by_query
            .entry(norm_query(&row.query))
            .or_default()
            .extend(row.used);
    }

    let mut groups: Vec<Group> = Vec::new();
    for line in read_lines(trails)? {
        let row: TrailRow = serde_json::from_str(&line).context("bad trail row")?;
        let Some(used) = usage_by_query.get(&norm_query(&row.query)) else {
            continue;
        };
        let mut candidates = Vec::new();
        for (i, c) in row.candidates.iter().enumerate() {
            let is_used = used.iter().any(|u| *u == c.path);
            let value = row.values.get(i).copied().unwrap_or(0.0);
            // Only learn from candidates the gate had an opinion about:
            // dropped (< lead) rows carry no verdict worth imitating — but
            // their absence-after-usage is exactly demote evidence, so keep
            // them with their (low) value as the label anchor.
            let _ = value;
            candidates.push((c.path.clone(), c.terms.clone(), is_used));
        }
        groups.push(Group { candidates });
    }

    // Deterministic split: sort groups by normalized query, tail = holdout.
    // Tiny corpora (n < 2) learn on everything — nothing to verify against.
    groups.sort_by(|a, b| {
        a.candidates
            .first()
            .map(|c| c.0.clone())
            .unwrap_or_default()
            .cmp(&b.candidates.first().map(|c| c.0.clone()).unwrap_or_default())
    });
    let holdout_n = if groups.len() >= 2 {
        (((groups.len() as f64) * holdout_frac).ceil() as usize).min(groups.len() - 1)
    } else {
        0
    };
    let (learn_g, holdout_g) = groups.split_at(groups.len().saturating_sub(holdout_n));

    // Per-term usage precision on each split.
    let measure = |gs: &[Group]| -> HashMap<String, (usize, usize)> {
        let mut m: HashMap<String, (usize, usize)> = HashMap::new(); // term → (used, total)
        for g in gs {
            for (_, terms, used) in &g.candidates {
                let mut seen = terms.clone();
                seen.sort();
                seen.dedup();
                for t in seen {
                    let e = m.entry(t).or_default();
                    if *used {
                        e.0 += 1;
                    }
                    e.1 += 1;
                }
            }
        }
        m
    };
    let learn_stats = measure(learn_g);
    let holdout_stats = measure(holdout_g);

    let mut lex = Lexicon {
        version: LEXICON_VERSION,
        ..Default::default()
    };
    let mut promoted = 0u64;
    let mut demoted = 0u64;
    for (term, (used, total)) in &learn_stats {
        if *total < min_samples {
            continue; // unmeasured — stays neutral
        }
        let p_learn = *used as f64 / *total as f64;
        let (h_used, h_total) = holdout_stats.get(term).copied().unwrap_or((0, 0));
        let p_hold = if h_total > 0 {
            Some(h_used as f64 / h_total as f64)
        } else {
            None
        };
        if p_learn >= PROMOTE_PRECISION && p_hold.map_or(true, |p| p >= 0.5) {
            lex.promote.insert(term.clone(), 1.0);
            promoted += 1;
        } else if p_learn <= DEMOTE_PRECISION && p_hold.map_or(true, |p| p <= 0.5) {
            lex.demote.insert(term.clone(), 1.0);
            demoted += 1;
        }
    }
    lex.stats.insert("groups".into(), groups.len() as u64);
    lex.stats.insert("promoted".into(), promoted);
    lex.stats.insert("demoted".into(), demoted);
    Ok(lex)
}

fn read_lines(path: &Path) -> Result<Vec<String>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    Ok(text.lines().map(str::to_string).collect())
}
