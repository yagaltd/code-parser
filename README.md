# code-parser

Tree-sitter engine that parses source files into versioned `FileParseIR` — symbols, calls, imports, and diagnostics — for consumption by downstream code indexes.

## Components

- `crates/code-parser-ir` — IR types + serde, zero heavy deps
- `crates/code-parser-core` — tree-sitter engine: parse, extract, resolve, watch, hash cache
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

# Parse a single file → JSON
cargo run -- parse src/main.rs --json

# Parse a whole repo → newline-delimited JSON
cargo run -- parse-repo . --languages rust --jsonl

# Watch a directory → emit IR on change (requires --features watcher)
cargo run --features watcher -- watch . --emit jsonl

# Validate a file parses without errors
cargo run -- check src/main.rs
```

## IR shape

One JSON object per file:

```json
{
  "ir_version": 1,
  "path": "src/main.rs",
  "language": "Rust",
  "content_hash": "<blake3>",
  "symbols": [{ "local_key": "main", "name": "main", "kind": "Function", "start_line": 1, ... }],
  "calls":    [{ "caller_local_key": "main", "callee_name": "println", "line": 2, ... }],
  "imports":  [{ "import_name": "HashMap", "target_module": "std::collections", "kind": "Named", ... }],
  "diagnostics": []
}
```

See `fixtures/` for per-language golden examples and `schema/file_parse_ir.v1.json` for the full schema.

## Library usage

```rust
use code_parser_core::{parse_file, parse_repo, HashCache};

// Single file.
let result = parse_file(Path::new("src/main.rs"))?;
println!("{} symbols", result.ir.symbols.len());

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

Golden fixtures in `fixtures/` are compared structurally on every test run. Volatile fields (byte offsets, columns, content hash) are excluded from comparison.

## License

MIT
