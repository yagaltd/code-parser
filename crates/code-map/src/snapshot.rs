//! Budgeted map snapshot — the frozen-prefix render for agent system prompts.
//!
//! A consumer that injects a repo map into a system prompt wants the prefix
//! **byte-identical across requests** so provider prompt caches hit (explicit
//! `cache_control` or the implicit prefix caches of OpenAI/DeepSeek-class
//! providers). [`render_with_meta`] is a pure function of the IR set: no
//! wall-clock, no randomness, independent of input order. Hold the result in
//! [`Snapshot`] for idle-TTL semantics (hot while read, expires after
//! inactivity), and let per-query [`crate::search`] deliver fresh precision —
//! the snapshot gives global awareness, search gives exactness.
//!
//! Budget model: tokens estimated as `len / 4` (the retrieval-card
//! convention). When the full render would exceed the budget, files degrade
//! from the lowest-priority end — symbol lists drop first, then the file line
//! itself moves to an omitted-count footer. Priority is (non-test symbol
//! count desc, path asc): a deterministic hub heuristic that keeps the
//! files with the most surface first, until something smarter is measured.

use code_parser_ir::FileParseIR;

pub const SNAPSHOT_VERSION: u32 = 1;
pub const DEFAULT_BUDGET: u32 = 2500;
pub const MIN_BUDGET: u32 = 500;
pub const MAX_BUDGET: u32 = 8000;
/// Symbol names listed per file before `+k more`.
pub const MAX_SYMS_PER_FILE: usize = 12;
/// Tokens reserved for header + footer during allocation (real strings are
/// measured after assembly; this only prevents the common off-by-footer).
const RESERVED_OVERHEAD_TOKENS: u32 = 48;

/// Token estimate — same convention as retrieval cards (`len / 4`).
pub fn est_tokens(s: &str) -> u32 {
    s.len().div_ceil(4) as u32
}

/// Render the map under `budget` (clamped to `MIN_BUDGET..=MAX_BUDGET`).
pub fn render(irs: &[FileParseIR], budget: u32) -> String {
    render_with_meta(irs, budget).0
}

/// Machine-readable facts about a render (the `--json` surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderMeta {
    pub snapshot_version: u32,
    pub budget: u32,
    pub est_tokens: u32,
    pub files: usize,
    pub omitted: usize,
}

/// Render the budgeted snapshot plus its metadata. Deterministic: the same
/// IR set (in any order) yields byte-identical content.
pub fn render_with_meta(irs: &[FileParseIR], budget: u32) -> (String, RenderMeta) {
    let budget = budget.clamp(MIN_BUDGET, MAX_BUDGET);

    let mut entries: Vec<Entry> = irs.iter().map(Entry::from_ir).collect();
    // Priority: symbol-rich hubs first; path breaks ties both ways
    // (sort key and output order stay content-derived).
    entries.sort_by(|a, b| b.symbols.cmp(&a.symbols).then_with(|| a.path.cmp(&b.path)));

    // Greedy allocation in priority order against budget minus overhead.
    let mut used = RESERVED_OVERHEAD_TOKENS;
    for e in entries.iter_mut() {
        let full_t = est_tokens(&e.full_line);
        let bare_t = est_tokens(&e.bare_line);
        if used + full_t <= budget {
            e.state = Degrade::Full;
            used += full_t;
        } else if used + bare_t <= budget {
            e.state = Degrade::Bare;
            used += bare_t;
        } else {
            e.state = Degrade::Omitted;
        }
    }

    // Assemble; the safety loop only fires if real header/footer overran
    // the reserved overhead — degrade the lowest-priority entry and retry.
    loop {
        let (content, meta) = assemble(&entries, budget);
        if meta.est_tokens <= budget || !degrade_one(&mut entries) {
            return (content, meta);
        }
    }
}

/// A frozen snapshot artifact with idle-TTL semantics.
///
/// The whole point: the consumer re-serves `content` byte-identically for
/// many requests, so the system-prompt prefix stays cache-stable. Lifetime
/// is **idle-TTL, not birth-TTL**: every [`Snapshot::read`] bumps
/// `last_accessed_ms`; the snapshot expires only after `ttl_ms` of
/// inactivity. An hour of continuous use stays hot; five idle minutes
/// expires it. The clock is injected — no hidden wall-clock reads, fully
/// testable.
///
/// File edits must NOT mutate a held snapshot (that breaks the cache
/// prefix). Freshness comes from re-rendering after expiry, plus per-query
/// search — never from patching the frozen content.
pub struct Snapshot {
    content: String,
    paths: Vec<String>,
    ttl_ms: u64,
    last_accessed_ms: u64,
}

impl Snapshot {
    pub fn new(content: String, paths: Vec<String>, ttl_ms: u64, now_ms: u64) -> Self {
        Self {
            content,
            paths,
            ttl_ms,
            last_accessed_ms: now_ms,
        }
    }

    /// Build from IRs: render under budget and freeze with a TTL.
    pub fn build(irs: &[FileParseIR], budget: u32, ttl_ms: u64, now_ms: u64) -> Self {
        let (content, _) = render_with_meta(irs, budget);
        let paths: Vec<String> = irs.iter().map(|ir| ir.path.clone()).collect();
        Self::new(content, paths, ttl_ms, now_ms)
    }

    /// Returns the frozen content and bumps `last_accessed_ms`.
    pub fn read(&mut self, now_ms: u64) -> &str {
        self.last_accessed_ms = now_ms;
        &self.content
    }

    /// True when the snapshot has been idle for `ttl_ms` or longer.
    pub fn is_idle_expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_accessed_ms) >= self.ttl_ms
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }
}

// ── internals ─────────────────────────────────────────────────────────────

enum Degrade {
    Full,
    Bare,
    Omitted,
}

struct Entry {
    path: String,
    full_line: String,
    bare_line: String,
    symbols: usize,
    state: Degrade,
}

impl Entry {
    fn from_ir(ir: &FileParseIR) -> Self {
        // Test symbols are excluded: they bloat the map and agents ask for
        // production surface; test files still appear as file lines.
        let syms: Vec<&str> = ir
            .symbols
            .iter()
            .filter(|s| !s.is_test && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        let mut names = String::new();
        for (i, name) in syms.iter().take(MAX_SYMS_PER_FILE).enumerate() {
            if i > 0 {
                names.push_str(", ");
            }
            names.push_str(name);
        }
        if syms.len() > MAX_SYMS_PER_FILE {
            names.push_str(&format!(" +{}", syms.len() - MAX_SYMS_PER_FILE));
        }
        let bare_line = format!("{} {} {}L", ir.path, ir.language, ir.line_count);
        let full_line = if names.is_empty() {
            bare_line.clone()
        } else {
            format!("{bare_line}  {names}")
        };
        Entry {
            path: ir.path.clone(),
            full_line,
            bare_line,
            symbols: syms.len(),
            state: Degrade::Omitted, // set by allocation
        }
    }
}

fn assemble(entries: &[Entry], budget: u32) -> (String, RenderMeta) {
    let included: Vec<&Entry> = entries
        .iter()
        .filter(|e| !matches!(e.state, Degrade::Omitted))
        .collect();
    let omitted = entries.len() - included.len();
    let total_lines: u32 = entries.iter().filter_map(|e| line_count_of(e)).sum();
    let total_syms: usize = entries.iter().map(|e| e.symbols).sum();

    let mut out = String::new();
    out.push_str(&format!(
        "code-map snapshot v{SNAPSHOT_VERSION}: {} files, {total_lines} lines, {total_syms} symbols (budget {budget} tokens)\n",
        included.len()
    ));
    // Path order: stable, diff-friendly, input-order independent.
    let mut ordered = included.clone();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));
    for e in ordered {
        match e.state {
            Degrade::Full => {
                out.push_str(&e.full_line);
                out.push('\n');
            }
            _ => {
                out.push_str(&e.bare_line);
                out.push('\n');
            }
        }
    }
    if omitted > 0 {
        out.push_str(&format!(
            "+ {omitted} more files omitted (budget) — find them: code-map search \"<query>\"\n"
        ));
    }
    let meta = RenderMeta {
        snapshot_version: SNAPSHOT_VERSION,
        budget,
        est_tokens: est_tokens(&out),
        files: included.len(),
        omitted,
    };
    (out, meta)
}

fn line_count_of(e: &Entry) -> Option<u32> {
    // Parse the trailing `NNNL` back off the bare line — cheaper than
    // carrying the whole IR in the entry.
    let idx = e.bare_line.rfind(' ')?;
    let tail = &e.bare_line[idx + 1..];
    tail.strip_suffix('L').and_then(|n| n.parse().ok())
}

/// Degrade the lowest-priority non-omitted entry one step. Returns false
/// when everything is already omitted.
fn degrade_one(entries: &mut [Entry]) -> bool {
    // Entries are still in priority order (sort happened before allocation).
    for e in entries.iter_mut().rev() {
        match e.state {
            Degrade::Full => {
                e.state = Degrade::Bare;
                return true;
            }
            Degrade::Bare => {
                e.state = Degrade::Omitted;
                return true;
            }
            Degrade::Omitted => continue,
        }
    }
    false
}
