# Retrieval card + ingest policy plan

**Status:** concrete next feature plan (post Phase 1 parser + Phase 2 domain ingest)  
**Date:** 2026-07-30  
**Authority split:** mailbox-parser style — **parser enriches IR; domain owns nodes + policy**

---

## 0. Goal (agent ladder)

```
query (NL or identifier)
  → hybrid/lex rank paths + short cards          [orient]
  → open path / symbol span                      [pin + work]
  → Rhai/graph: children, calls_of, imports_of   [structure > grep]
  → re-parse/edit cycle via content_hash skip    [loop]
```

**Not the goal:** card replaces the graph. Card is a **routing / index projection**.

---

## 1. Who owns what (confirmed)

| Layer | Owns | Does not own |
|-------|------|--------------|
| **code-parser** | Tree-sitter IR, resolve, blake3, **mechanical retrieval cards** in JSON | Cos NodeIds, GraphStore, PathIndex, LexIndex, ANN, promote policy |
| **domain_code** | Map IR → nodes, **ingest policy** (what to promote to children), Path/Lex hooks, optional later embed hook input | Grammars / AST walk / card formatting rules |
| **sdk/server/Rhai** | Producer, hybrid query templates, agent-facing scripts | Re-implementing parse |

Same as mailbox-parser → canonical JSON → SDK/domain ingest.

---

## 2. Feature A — code-parser: retrieval cards in IR

### A.1 IR version

- Prefer **additive required fields** on `FileParseIR` (bump `ir_version` **1 → 2** so Cos can detect card-capable IR).
- Parser **always** builds cards after extract+resolve. There is **no** “parse without cards” product mode.
- Caps / denylist are **format knobs** (how the text is shaped), not “whether cards exist.”
- Cos `domain_code` / ingest policy decides: store card on payload, index it, embed it, or ignore it.

Backward compat reading old JSON dumps is tooling-only (`#[serde(default)]` on *readers* if needed). **Live parser output always includes cards.**

### A.2 New IR fields

```rust
// code-parser-ir

/// Deterministic retrieval projection. Not a Cos node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrievalCard {
    /// Format version for embed/lex cache invalidation (e.g. 1).
    pub card_version: u32,
    /// Estimated token count (heuristic ~4 chars/token).
    pub est_tokens: u32,
    /// Bound ≤ max_chars; ready for Lex + optional later embed.
    pub text: String,
}

pub struct FileParseIR {
    // ...existing...
    /// File-level card — **always populated** by `parse_file` / `parse_repo`.
    pub retrieval_card: RetrievalCard,

    /// Per-symbol cards — **always same length as `symbols`** (index-aligned).
    /// Domain may leave them off Cos symbol nodes when policy skips symbols.
    pub symbol_cards: Vec<RetrievalCard>,
}
```

Invariant after parse:

```text
ir.symbol_cards.len() == ir.symbols.len()
ir.retrieval_card.text starts with "FILE "
```

Empty symbol list → `symbol_cards = []` still OK; file card still has FILE/IMPORTS lines.

V1 **derives card text only from existing IR arrays** (no separate outline type). Structured outline deferred unless policy needs it.

### A.3 Card text grammar (normative)

**File card** (`card_version = 1`), max ~**2048 chars** (~512 tokens @ 4 c/t) default; configurable:

```text
FILE path={path} lang={language} lines={line_count} hash={content_hash[..8]}
IMPORTS {mod1}, {mod2}, … (+N more)
SYMBOLS
  {kind} {qualified_name} L{start}-{end}[ sig={sig}]
  …
  (+N more)
CALLS_OUT {name}×{count} …
PUB {name}, …
```

Rules:

1. Deterministic sort: imports by string; symbols by `(kind_order, qualified_name)`; calls by count desc then name.
2. Caps: e.g. imports 24, symbols 40 lines, call histogram 15 keys.
3. Call noise denylist (shared constant): `unwrap`, `clone`, `into`, `to_string`, `ok`, `some`, `push`, `map`, `and_then`, … (lang-tunable later).
4. Truncate from the bottom (CALLS/PUB first) until under `max_chars`; never drop `FILE` line.
5. Same IR + same `card_version` + same caps → **byte-identical** `text`.
6. Empty extract → card with FILE line + `SYMBOLS` empty (still indexable by path).

**Symbol card** — always emit one per `symbols[i]`:

```text
SYM path={file} qname={qualified_name} kind={kind} L{start}-{end}
SIG {signature}
DOC {docstring first ~240 chars}
CALLS {callee names from this caller, capped, de-noised}
```

### A.4 Implementation placement

| Crate | Work |
|-------|------|
| `code-parser-ir` | Types + pure card builders free of tree-sitter |
| `code-parser-core` | **Always** `build_retrieval_cards(&mut FileParseIR, &CardFormat)` after extract+resolve |
| tests/fixtures | Golden: `simple.*.ir.json` includes cards; assert stable text |

```rust
/// Format knobs only — cards are always built.
pub struct CardFormat {
    pub max_file_chars: usize,    // default 2048
    pub max_symbol_chars: usize,  // default 512
    pub max_symbol_lines: usize,  // default 40
    pub max_imports: usize,
    pub max_call_histogram: usize,
}

impl Default for CardFormat { /* fixed product defaults */ }
```

Call site: **mandatory** end of `parse_file` / `parse_repo` path (after resolve) so every language gets cards. No feature flag to skip.

### A.5 CLI

- JSON **always** includes `retrieval_card` + `symbol_cards`.
- `--card-only` debug: print file card text to stdout (still from full parse).
- No `--no-cards`. Lean dumps are not a parser product mode; strip fields downstream if Cos wants.

### A.6 Acceptance (code-parser)

- [ ] Every successful parse IR has `retrieval_card` and `symbol_cards.len() == symbols.len()`.
- [ ] Rust/TS/JS/Python goldens include cards; golden compare includes card text.
- [ ] office-parser-scale: every IR has `FILE ` prefix; `est_tokens` within cap.
- [ ] Mutation test: comment-only change → hash changes → card hash line changes; cards rebuild.
- [ ] No Cos imports in code-parser; no “skip card” API on `parse_file`.

### A.7 Out of scope (code-parser)

- Promote/skip child nodes
- PathIndex / LexIndex / ANN
- LLM summarization
- Changing resolve quality (see `code-parser-v2.md` §8 separately)

---

## 3. Feature B — domain_code: consume cards + index them

### B.1 Ingest options for cards (Cos chooses use, not parser)

Parser always sends cards. Domain policy decides consumption:

```rust
#[derive(Clone, Debug)]
pub struct CardIngestOpts {
    /// Store file card text on `CodeFilePayload` (default true).
    pub store_file_card: bool,
    /// Store symbol card on each promoted `SymbolPayload` (default true).
    pub store_symbol_cards: bool,
    /// LexIndex file node from file card (default true).
    pub lex_file_card: bool,
    /// LexIndex symbol nodes from symbol cards (default true).
    pub lex_symbol_cards: bool,
    // embed_* later — Feature D
}
```

Sits on `IngestOpts` / `IngestPolicy`. Disabling store/lex still accepts full IR; just doesn’t write or index card fields.

### B.2 Payload fields

```rust
// model.rs — additive when store_* enabled at map time
pub struct CodeFilePayload {
    // ...existing...
    /// Mechanical retrieval card text from IR (when store_file_card).
    #[serde(default)]
    pub retrieval_card: Option<String>,
    #[serde(default)]
    pub card_version: Option<u32>,
}

pub struct SymbolPayload {
    // ...existing...
    #[serde(default)]
    pub retrieval_card: Option<String>,
    #[serde(default)]
    pub card_version: Option<u32>,
}
```

Map (when enabled):

- `ir.retrieval_card.text` → file payload
- `ir.symbol_cards[i]` → symbol payload when that symbol node is promoted

Invariant from parser: `symbol_cards.len() == symbols.len()` — no need for “if present” in parser; only “if we promote this symbol.”

### B.3 Lex hooks (replace weak file text)

Today (`apply_ingest_hooks`):

```text
file lex  = path + language + hash8
symbol lex = name + kind + sig + qname
```

After (when `lex_*` true):

| Node | `index_text` body |
|------|-------------------|
| `code/file` | **file card text** (fallback only if Cos store disabled) |
| `code/symbol` | **symbol card text** (fallback name/kind/sig/qname) |
| `code/call` / `code/import` | keep compact strings **or** skip call lex when policy says noise (Feature C) |

PathIndex unchanged (path → file NodeId).

### B.4 Incremental

Already:

- hash short-circuit → skip graph + hooks  
- on change → delete old lex ids + reindex  

When only **card template** changes (`card_version` bump) without content_hash change: treat as Cos migration (re-ingest repo once) or bump a stored `card_version` check in policy. Document `RETRIEVAL_CARD_VERSION` in both repos (copy constant; no crate dep).

### B.5 Acceptance (domain)

- [ ] Default ingest **stores + lexes** file card from IR (parser always sent it).
- [ ] can set `store_file_card=false` / `lex_file_card=false` without parser changes.
- [ ] Symbol cards mapped only when symbols promoted.
- [ ] Hash skip still works; content change replaces card + lex.
- [ ] Existing child-node tests still pass (promote default = all).

### B.6 Out of scope (B)

- ANN / embed wiring (Feature D stub only)
- Dropping child nodes (Feature C)
- Asking parser to omit cards

---

## 4. Feature C — domain_code: ingest policy (node fan-out)

**Yes — this is domain_code (or sdk opts), not code-parser.**  

Parser still emits full IR + cards always (like mailbox full canonical). Policy decides Cos density.

### C.1 `IngestPolicy` (new)

```rust
#[derive(Clone, Debug)]
pub struct IngestPolicy {
    /// Always create code/file.
    // file always true

    /// Create code/symbol children.
    pub promote_symbols: PromoteSymbols,
    /// Create code/call children under promoted symbols.
    pub promote_calls: PromoteCalls,
    /// Create code/import children.
    pub promote_imports: PromoteImports,

    /// When symbols stay payload-only, still fill CodeFilePayload.symbols/imports summaries.
    pub keep_payload_summaries: bool, // default true
}

pub enum PromoteSymbols {
    /// Current behavior — all IR symbols → nodes.
    Always,
    /// symbols.len() >= min OR any "export-like" heuristic later.
    When { min_symbols: usize },
    /// Never — file node + card only (structure via IR re-fetch not stored).
    Never,
}

pub enum PromoteCalls {
    Always,
    /// Only calls with callee_local_key or callee_file resolved; drop noise names.
    ResolvedOrNonNoise,
    /// Only if symbols promoted for this file.
    OnlyIfSymbolsPromoted,
    Never,
}

pub enum PromoteImports {
    Always,
    When { min_imports: usize },
    Never,
}
```

Default for backward compat:

```rust
IngestPolicy {
  promote_symbols: Always,
  promote_calls: Always, // or ResolvedOrNonNoise once denylist shared
  promote_imports: Always,
  keep_payload_summaries: true,
}
```

Suggested **prod starter** (after metrics):

```rust
promote_symbols: When { min_symbols: 3 },  // 1–2 fn files → file+card only
promote_calls: ResolvedOrNonNoise,
promote_imports: Always, // cheap, high value
```

Wire: `IngestOpts.policy: IngestPolicy`.

### C.2 Map changes (`map_ir_to_parse_output`)

1. Always build `CodeFilePayload` (+ card).
2. If promote symbols → fill `symbols`/`symbol_ids` else empty vecs (names still on payload.symbols).
3. Calls only if promote + caller symbol was promoted (caller NodeId required today).
4. Imports per policy.
5. `apply_parse_replace` unchanged.

### C.3 Query impacts

| Policy | `calls_of` | Symbol lex | Agent ladder |
|--------|------------|------------|--------------|
| Always (now) | works | per-symbol | full |
| When min_symbols | only rich files | partial | card routes tiny files |
| Never symbols | N/A | file card only | orient OK; trace weak |

Document Rhai: if no symbol node, agent opening file by path still works; trace APIs return empty → fall back to read file.

### C.4 Acceptance

- [ ] Default policy = bit-identical node tree to today (modulo cards).
- [ ] `When { min_symbols: 3 }` fixture: 2-fn file → 1 file node, no children; 10-fn file → children.
- [ ] Card always on file; lex finds tiny file via card text (symbol names in card).
- [ ] Metrics helper optional: count nodes/file under policy for smoke.

### C.5 Deferred thresholds

Tune `min_symbols` with smoke repos (office-parser, agent-spec); no magic final number in v1.

---

## 5. Feature D — optional ANN on cards (later)

- **Not blocking A–C.**
- Embed input = **file** `retrieval_card.text` first.
- Same Cos text embedder (regular); dim unchanged; RRF lex+ANN.
- Skip re-embed when `content_hash` + `card_version` unchanged.
- Symbol ANN only if post-lex conceptual misses on symbol pin.
- Hook sketch: `ProductionHooks.embed: Option<&dyn CodeEmbedHook>` — out of domain_code default.

---

## 6. Phased delivery order

| Phase | Owner | Deliverable | Depends |
|-------|--------|-------------|---------|
| **A0** | code-parser | Spec card grammar + `CardOptions` + tests on unit builder (no extract change) | — |
| **A1** | code-parser | Emit file + symbol cards from `parse_file`; goldens | A0 |
| **B0** | domain_code | Payload fields + map cards | A1 (or hand IR) |
| **B1** | domain_code | Lex hooks prefer card text | B0 |
| **C0** | domain_code | `IngestPolicy` default Always (= today) | B0 |
| **C1** | domain_code | `When { min_symbols }` + call noise option | C0 |
| **S0** | sdk/server | Implement `SourceDomainAdapter` for code in `source_ingest` (same pattern as mailbox/office adapters); persist via `ingest_*_with_hooks` + real Store; UI shows card text | B1 |
| **D0** | sdk | Optional ANN embed on file cards (same Cos text embedder as other domains) | B1 + product |

Suggested ship: **A1 → B1 → S0** first (agent ladder without policy). Then **C1** storage win. Then **D0**.

---

## 7. Shared constants (copy, don’t couple crates)

Document in both READMEs:

| Name | Value |
|------|--------|
| `RETRIEVAL_CARD_VERSION` | `1` |
| Default max file card chars | `2048` |
| Call noise denylist | listed in plan A.3 |

Replicate denylist string list in code-parser (card) and domain_code (call promote filter) — small duplication > inverse dep.

---

## 8. Success criteria (product)

1. Agent hybrid/LEX query on code returns **paths + card snippet**, not grep-only.  
2. Rhai can still drill graph where policy promoted structure.  
3. Tiny files do not force huge child fan-out under opt-in policy.  
4. Full IR always available from parser for re-ingest / debug JSONL.  
5. No LLM in the card path.  
6. Hash short-circuit still avoids parse+index work on unchanged files.

---

## 9. Concrete tickets (checklist)

### code-parser

- [x] A0: `RetrievalCard` (required on IR) + `CardFormat` knobs + pure builders
- [x] A0: noise denylist + sort/cap helpers + unit tests (no tree-sitter)
- [x] A1: **always** build file + symbol cards after resolve (no skip API)
- [x] A1: update goldens + schema `file_parse_ir.v2.json`
- [x] A1: CLI `--card-only` debug only (no `--no-cards`)
- [x] Docs: README — parser always emits cards; Cos decides use

### domain_code

- [x] B0: payload card fields; map from IR under `CardIngestOpts`
- [x] B1: `apply_ingest_hooks` lex from cards when `lex_*` true (default on)
- [x] C0: `IngestPolicy` on `IngestOpts`; default promote Always
- [x] C1: tiered promote + tests (tiny vs rich fixture)
- [x] C1: call promote `ResolvedOrNonNoise`
- [ ] Update `refactor_domain_code_with_parser.md` — cards always in IR; policy is Cos
- [ ] Export policy + card ingest opts in `lib.rs`

### Cos server/sdk (separate but required for “feel it”)

- [x] S0: Implement `SourceDomainAdapter` for code (pattern: mailbox/office adapters in `cognitiveos_v3_sdk`)
- [x] S0: Register code adapter with `SourceIngestService` → real persisted ingest via `ingest_*_with_hooks` + V3 Store
- [x] S0: Code smoke UI shows file card text in detail panel (not just preview graph)
- [ ] Rhai template sketch: search code/file|symbol → open path → calls_of

### Explicitly later

- [ ] D0: `CodeEmbedHook` on `ProductionHooks` — optional ANN embed of file cards via Cos text embedder
- [ ] Symbol ANN (only if post-lex conceptual misses on symbol pin)
- [ ] code-parser-v2.md §8 resolve quality
- [ ] Shared crate for denylist (only if duplication hurts)

---

## 10. North star one-liner

**code-parser always emits full IR plus mailbox-style mechanical retrieval cards; domain_code chooses whether to store/index/embed those cards and applies an ingest policy that may keep or skip child nodes so agents can route cheaply then drill structure when present.**
