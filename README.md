# code-parser

Tree-sitter engine that parses source files into versioned `FileParseIR` — symbols, calls, imports, diagnostics, and **always-present mechanical retrieval cards** — for consumption by downstream code indexes (e.g. CognitiveOS `domain_code`).

Parser always emits cards. Cos / domain chooses whether to store, lex, embed, or ignore them (ingest policy). There is no “parse without cards” mode.

## Components

- `crates/code-parser-ir` — IR types + serde + card builders (zero heavy deps)
- `crates/code-parser-core` — tree-sitter engine: parse, extract, resolve, cards, watch, hash cache
- `crates/code-parser-cli` — thin CLI (`parse`, `parse-repo`, `watch`, `check`)

## Supported languages

| Language | Feature flag | Extensions | Grammar crate |
|----------|-------------|------------|---------------|
| Rust | `rust` (default) | `.rs` | `tree-sitter-rust` |
| TypeScript | `typescript` | `.ts`, `.tsx` | `tree-sitter-typescript` |
| JavaScript | `javascript` | `.js`, `.jsx`, `.mjs`, `.cjs` | `tree-sitter-javascript` |
| Python | `python` | `.py`, `.pyi` | `tree-sitter-python` |

Each language is feature-gated. Build only what you need.

## Quick start

```bash
# Build with all languages
cargo build --features rust,typescript,javascript,python

# Parse a single file → JSON (includes retrieval_card + symbol_cards)
cargo run -- parse src/main.rs --json

# Print only the file retrieval card (debug)
cargo run -- parse src/main.rs --card-only

# Parse a whole repo → newline-delimited JSON
cargo run -- parse-repo . --languages rust --jsonl

# Watch a directory → emit IR on change (requires --features watcher)
cargo run --features watcher -- watch . --emit jsonl

# Validate a file parses without errors
cargo run -- check src/main.rs
```

## IR shape (`ir_version`: 3)

One JSON object per file. Cards are required fields built after extract+resolve:

```json
{
  "ir_version": 3,
  "path": "src/main.rs",
  "language": "Rust",
  "content_hash": "<blake3>",
  "byte_len": 128,
  "line_count": 10,
  "symbols": [{ "local_key": "main", "name": "main", "kind": "Function", "start_line": 1, "is_test": false }],
  "calls":    [{ "caller_local_key": "main", "callee_name": "println", "line": 2 }],
  "imports":  [{ "import_name": "HashMap", "target_module": "std::collections", "kind": "Named" }],
  "diagnostics": [],
  "retrieval_card": {
    "card_version": 1,
    "est_tokens": 42,
    "text": "FILE path=src/main.rs lang=Rust lines=10 hash=abcd1234\nIMPORTS …\nSYMBOLS\n  …"
  },
  "symbol_cards": [
    { "card_version": 1, "est_tokens": 12, "text": "SYM path=src/main.rs qname=main kind=Function L1-3\n…" }
  ]
}
```

Invariant: `symbol_cards.len() == symbols.len()`.

`diagnostics` is populated on broken input (never dead code): each maximal
tree-sitter ERROR region yields one `Error` diagnostic with its line and byte
span; `missing` markers yield `Warning` diagnostics. Bounded at 64 entries
per file — further parse issues collapse into one overflow `Warning`
(`MAX_DIAGNOSTICS_PER_FILE` in `extractors/utils.rs`).

`SymbolIR.is_test` (v3, serde-default false) marks test code so consumers can
answer "which tests cover this symbol" (TESTED_BY mirrors, CognitiveOS
`domain_code` idea 2). Detection per language:

- **Rust** — `#[test]` / `#[cfg(test)]` attributes, symbols inside `mod tests`,
  files under a `tests/` dir or named `tests.rs`
- **Python** — `test_*` / `Test*` names (methods inherit from their `Test*`
  class), `test_*.py` files, `tests/`/`test/` dirs
- **TS/JS** — `.test.` / `.spec.` filenames, `__tests__/` / `test/` / `tests/` dirs

The parser is stateless: it never removes or versions anything — `is_test` is
computed per snapshot; all state (mirrors, sweeps, parity) lives in
`domain_code` on the store side.

See `fixtures/*/simple.ir.json` and `schema/file_parse_ir.v2.json`. Legacy `schema/file_parse_ir.v1.json` remains for old dumps only.

## Retrieval cards

Mechanical, deterministic text for Lex / optional later ANN — **not** Cos nodes:

- **File card** — path, lang, hash, imports, symbol outline, call histogram, pub names (capped ~2k chars)
- **Symbol card** — one per symbol: qname, kind, lines, sig, doc snippet, local calls

Cos `domain_code` maps these onto payloads/indexes under ingest policy (store/lex/promote children). Parser does not know about CognitiveOS nodes.

## Library usage

```rust
use code_parser_core::{parse_file, parse_file_bytes, parse_repo, HashCache};

// Single file — IR always includes cards.
let result = parse_file(Path::new("src/main.rs"))?;
println!("{} symbols", result.ir.symbols.len());
println!("{}", result.ir.retrieval_card.text);

// Bytes-in (no filesystem read) — primary entry point for downstream pipelines.
let ir = parse_file_bytes("src/main.rs", &source_bytes)?;

// Whole repo with cross-file resolution.
let results = parse_repo(Path::new("."), None)?;

// Cached: skip re-parse when content hash unchanged.
let mut cache = HashCache::new();
if let Some(result) = parse_file_cached(Path::new("src/main.rs"), &mut cache)? {
    // File changed — process IR.
}
```

## Resolution

- **In-file:** bare name → same-file symbol `local_key` (e.g. `bar()` → `bar`)
- **Cross-file:** qualified names with `::` → `callee_file` set to declaring file path
- **External heuristic:** qualified name not found in repo → `callee_external: true`

Method calls, field chains, and trait resolution are out of scope for V1.

## Tests

```bash
cargo test --features rust,typescript,javascript,python --all
```

Golden fixtures in `fixtures/` are compared structurally on every test run. Volatile fields (byte offsets, columns, content hash) are excluded; cards are rebuilt with hash cleared so FILE `hash=` line is stable.

## License

MIT
