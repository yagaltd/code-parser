//! # code-parser-scip — SCIP overlay producer for Rust call resolution
//!
//! Runs `rust-analyzer scip` out-of-process (subprocess, no shell, no daemon,
//! no in-process linkage) against a cargo workspace, parses the resulting
//! protobuf SCIP index, and projects it down to a small **resolved-bindings**
//! artifact: for every callable reference occurrence in the index, the
//! `(file, line, column)` of the call site and the `(file, line)` of the
//! callee's definition — the two facts tree-sitter cannot derive (method
//! calls `x.foo()`, associated `Foo::bar` cross-file, trait dispatch).
//!
//! Matching is by **definition position**, never by textual name: consumers
//! resolve `(callee_file, callee_line)` against their own symbol index, so
//! name-grammar drift between SCIP symbols and tree-sitter qualified names is
//! irrelevant. `callee_qualified_name` is carried for diagnostics only and is
//! projected onto the tree-sitter grammar (`A::double`, `Greeter::greet`).
//!
//! The pipeline (see `docs/scip-notes.md` for the D0 spike write-up):
//!
//! ```text
//! bytes ──tree-sitter──▶ FileParseIR ──┐
//!                                      │  ingest-time binding (domain_code)
//! rust-analyzer scip ──code-parser-scip┤  ScipCallBindings artifact
//!   (out-of-process, cached by         │
//!    cargo fingerprint)                ▼
//!                           CallEdge { callee: Some, provenance: Inferred }
//! ```
//!
//! Caching: the artifact is keyed by a cargo fingerprint (bindings version,
//! indexer version, cargo-metadata package identity, source bytes). A cache
//! hit reuses the stored JSON artifact without spawning rust-analyzer.
//! Committed fixture artifacts (`fixtures/scip/index.scip`) let tests run
//! toolchain-free via [`from_scip_artifact`].

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use code_parser_ir::FileParseIR;
use protobuf::Message;
use scip::types::symbol_information::Kind;
use scip::types::Index;
use serde::{Deserialize, Serialize};

/// Version of the `ScipCallBindings` artifact shape. Bump on breaking change.
pub const SCIP_BINDINGS_VERSION: u32 = 1;

/// Byte cap for captured subprocess output (agent-spec kit discipline:
/// bounded output, no unbounded stderr buffering on failure paths).
const MAX_RA_OUTPUT_BYTES: usize = 64 * 1024;

/// One resolved call site: where the call is, where the callee is defined.
///
/// All lines are **1-indexed**; the column is **0-based** (matching
/// `CallIR.column` semantics in code-parser-ir) and optional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipCallBinding {
    /// Call-site file, repo-relative (matches `FileParseIR.path`).
    pub file: String,
    /// 1-indexed call-site line.
    pub line: u32,
    /// 0-based call-site column (callee-name start), when the index had one.
    pub column: Option<u32>,
    /// Definition file, repo-relative.
    pub callee_file: String,
    /// 1-indexed definition line.
    pub callee_line: u32,
    /// Definition name projected onto the tree-sitter qualified-name grammar
    /// (`A::double`, `a_fn`, `Greeter::greet`) — diagnostics only. Matching is
    /// by position, not by this string.
    pub callee_qualified_name: String,
    /// `"static"` for concrete method/associated/function calls,
    /// `"trait"` when the call site resolved to a trait method symbol
    /// (`dyn Trait` / generic receiver — dispatch is genuinely ambiguous).
    pub dispatch: Option<String>,
}

impl ScipCallBinding {
    /// Positional alignment check against a parsed file's IR: does the
    /// binding's `(callee_file, callee_line)` fall inside a symbol span?
    ///
    /// This is the definition-position mapping the whole overlay rests on,
    /// and doubles as the staleness guard for consumers: a binding whose
    /// callee position no longer lands in a symbol span was produced from a
    /// stale index and must be dropped.
    pub fn aligns_with_ir(&self, ir: &FileParseIR) -> bool {
        ir.path == self.callee_file
            && ir
                .symbols
                .iter()
                .any(|s| s.start_line <= self.callee_line && self.callee_line <= s.end_line)
    }
}

/// The resolved-bindings artifact (JSON, versioned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipCallBindings {
    pub version: u32,
    /// Indexer identity from the SCIP metadata (`rust-analyzer <version>`);
    /// falls back to `"rust-analyzer-scip"` when metadata is absent.
    pub tool: String,
    /// Cache key: blake3 of the cargo fingerprint inputs (see
    /// [`cargo_fingerprint`]). For [`from_scip_artifact`] this is the blake3
    /// of the artifact bytes themselves — the identity of a committed
    /// fixture, so `generate` output can be compared against it.
    pub cargo_fingerprint: String,
    pub bindings: Vec<ScipCallBinding>,
}

/// Run `rust-analyzer scip` against `root` (a cargo workspace) and return the
/// projected bindings, cached under `cache_dir/{cargo_fingerprint}.json`.
///
/// Cached artifacts are reused without spawning rust-analyzer when the
/// fingerprint is unchanged. On a cache miss the subprocess runs with
/// `root` as its working directory (rust-analyzer writes `index.scip` into
/// its cwd; we consume and remove it).
pub fn generate(root: &Path, cache_dir: &Path) -> Result<ScipCallBindings> {
    generate_with_ra(root, cache_dir, "rust-analyzer")
}

/// [`generate`] with an explicit indexer command — the override seam used by
/// tests (and mirroring rust-atlas's `ra_cmd` parameter).
pub fn generate_with_ra(root: &Path, cache_dir: &Path, ra_cmd: &str) -> Result<ScipCallBindings> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot canonicalize root {}", root.display()))?;
    let fingerprint = cargo_fingerprint_with_ra(&root, ra_cmd)?;

    let cache_path = cache_dir.join(format!("{fingerprint}.json"));
    if cache_path.is_file() {
        let text = std::fs::read_to_string(&cache_path)
            .with_context(|| format!("cannot read cached artifact {}", cache_path.display()))?;
        let cached: ScipCallBindings = serde_json::from_str(&text)
            .with_context(|| format!("cached artifact {} is corrupt", cache_path.display()))?;
        if cached.version == SCIP_BINDINGS_VERSION && cached.cargo_fingerprint == fingerprint {
            return Ok(cached);
        }
    }

    let output = Command::new(ra_cmd)
        .arg("scip")
        .arg(".")
        .current_dir(&root)
        .output()
        .map_err(|e| {
            anyhow!(
                "cannot run rust-analyzer (`{ra_cmd} scip .`): {e}. \
                 Install it (`rustup component add rust-analyzer`) or pass a valid --ra <path>."
            )
        })?;
    if !output.status.success() {
        return Err(anyhow!(
            "rust-analyzer scip failed ({}): {}",
            output.status,
            bounded_lossy(&output.stderr)
        ));
    }

    let artifact = root.join("index.scip");
    let bytes = std::fs::read(&artifact).with_context(|| {
        format!(
            "rust-analyzer scip exited 0 but produced no {}",
            artifact.display()
        )
    })?;
    let (tool, bindings) = parse_and_project(&bytes)?;
    // rust-analyzer drops index.scip into its cwd; consume it so a generated
    // workspace stays clean (the artifact lives in cache_dir / fixtures).
    let _ = std::fs::remove_file(&artifact);

    let result = ScipCallBindings {
        version: SCIP_BINDINGS_VERSION,
        tool,
        cargo_fingerprint: fingerprint,
        bindings,
    };
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("cannot create cache dir {}", cache_dir.display()))?;
    std::fs::write(&cache_path, serde_json::to_string_pretty(&result)?)
        .with_context(|| format!("cannot write cached artifact {}", cache_path.display()))?;
    Ok(result)
}

/// Parse a committed `.scip` artifact (protobuf) into bindings — the
/// toolchain-free path used by tests with checked-in fixtures.
///
/// `root` is kept for API symmetry with [`generate`]; paths in the artifact
/// are used as-is (repo-relative).
pub fn from_scip_artifact(scip_path: &Path, _root: &Path) -> Result<ScipCallBindings> {
    let bytes = std::fs::read(scip_path)
        .with_context(|| format!("cannot read SCIP artifact {}", scip_path.display()))?;
    let (tool, bindings) = parse_and_project(&bytes)?;
    // Fixture identity: blake3 over the artifact bytes (rust-atlas's own
    // convention for SCIP index fingerprints), truncated to 16 hex chars.
    let fingerprint = blake3::hash(&bytes).to_hex()[..16].to_string();
    Ok(ScipCallBindings {
        version: SCIP_BINDINGS_VERSION,
        tool,
        cargo_fingerprint: fingerprint,
        bindings,
    })
}

/// Cargo fingerprint for `root`: blake3 over the bindings version, the
/// indexer version, cargo-metadata key fields (package name/version/features)
/// and the workspace source bytes. Unchanged inputs ⇒ unchanged fingerprint ⇒
/// cached artifact reuse.
pub fn cargo_fingerprint(root: &Path) -> Result<String> {
    cargo_fingerprint_with_ra(root, "rust-analyzer")
}

fn cargo_fingerprint_with_ra(root: &Path, ra_cmd: &str) -> Result<String> {
    let version_output = Command::new(ra_cmd)
        .arg("--version")
        .output()
        .map_err(|e| {
            anyhow!(
                "cannot run rust-analyzer (`{ra_cmd} --version`): {e}. \
                 Install it (`rustup component add rust-analyzer`) or pass a valid --ra <path>."
            )
        })?;
    if !version_output.status.success() {
        return Err(anyhow!(
            "rust-analyzer --version failed ({}): {}",
            version_output.status,
            bounded_lossy(&version_output.stderr)
        ));
    }
    let ra_version = String::from_utf8_lossy(&version_output.stdout)
        .trim()
        .to_string();

    let manifest = root.join("Cargo.toml");
    let metadata = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&manifest)
        .current_dir(root)
        .output()
        .with_context(|| format!("cannot run `cargo metadata` for {}", root.display()))?;
    if !metadata.status.success() {
        return Err(anyhow!(
            "cargo metadata failed ({}): {}",
            metadata.status,
            bounded_lossy(&metadata.stderr)
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let mut packages: Vec<String> = metadata
        .get("packages")
        .and_then(|p| p.as_array())
        .map(|pkgs| {
            pkgs.iter()
                .filter_map(|p| {
                    let name = p.get("name")?.as_str()?;
                    let version = p.get("version")?.as_str()?;
                    let mut features: Vec<String> =
                        p.get("features")?.as_object()?.keys().cloned().collect();
                    features.sort();
                    Some(format!("{name}@{version}[{}]", features.join(",")))
                })
                .collect()
        })
        .unwrap_or_default();
    packages.sort();

    let files = source_files(root);
    Ok(fingerprint_inputs(
        SCIP_BINDINGS_VERSION,
        &ra_version,
        &packages,
        &files,
    ))
}

/// Pure fingerprint over explicit inputs — the unit-tested core of the cache
/// key. `files` are `(repo-relative path, bytes)`; sorted internally so input
/// order can never change the hash.
fn fingerprint_inputs(
    version: u32,
    ra_version: &str,
    packages: &[String],
    files: &[(String, Vec<u8>)],
) -> String {
    let mut files: Vec<&(String, Vec<u8>)> = files.iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut buf = format!("scip-bindings-version:{version}\nrust-analyzer:{ra_version}\n");
    for p in packages {
        buf.push_str(&format!("package:{p}\n"));
    }
    for (path, bytes) in files {
        buf.push_str(&format!("file:{path}:{}:", bytes.len()));
        buf.push_str(&String::from_utf8_lossy(bytes));
        buf.push('\n');
    }
    blake3::hash(buf.as_bytes()).to_hex()[..16].to_string()
}

/// Repo-relative source files of a workspace: `*.rs`, `Cargo.toml`
/// (excluding `target/`, `.git`, `node_modules`). `Cargo.lock` is excluded —
/// it is a derived artifact and `--no-deps` metadata already pins package
/// identity.
fn source_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if matches!(name.as_str(), "target" | ".git" | "node_modules") {
                    continue;
                }
                stack.push(path);
            } else if name.ends_with(".rs") || name == "Cargo.toml" {
                if let Ok(bytes) = std::fs::read(&path) {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    out.push((rel, bytes));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Decode a SCIP protobuf index and project it to call bindings.
///
/// Selection rules (documented in `docs/scip-notes.md`, D0 findings):
/// - definition vs reference: `symbol_roles & 1` (syntax kinds are always
///   `Unspecified` in rust-analyzer output — they cannot distinguish calls);
///   ranges are normalized from SCIP's `[line, start_col, end_col]` form
///   (rust-analyzer omits the trailing end-line for single-line spans);
/// - callable references: target symbol kind ∈ {Method, StaticMethod,
///   TraitMethod, Function, Macro} (type/value references carry Struct/Trait/
///   Field/… kinds and are dropped);
/// - `local N` symbols are dropped (no index-level definitions);
/// - a reference binds only when its symbol has exactly **one** definition
///   occurrence in the index: zero defs ⇒ external, several defs ⇒ ambiguous
///   definition position — both skipped rather than guessed;
/// - trait-method references (`dyn Trait` / generic receivers) carry
///   `dispatch: "trait"`; concrete ones `"static"`.
fn parse_and_project(bytes: &[u8]) -> Result<(String, Vec<ScipCallBinding>)> {
    let index = Index::parse_from_bytes(bytes)
        .map_err(|e| anyhow!("cannot decode SCIP protobuf index: {e}"))?;
    let tool = index
        .metadata
        .as_ref()
        .and_then(|m| m.tool_info.as_ref())
        .map(|t| format!("{} {}", t.name, t.version))
        .unwrap_or_else(|| "rust-analyzer-scip".to_string());

    let mut kinds: HashMap<&str, Kind> = HashMap::new();
    for doc in &index.documents {
        for sym in &doc.symbols {
            kinds.insert(sym.symbol.as_str(), sym.kind.enum_value_or_default());
        }
    }

    let mut defs: HashMap<&str, Vec<(String, u32, u32)>> = HashMap::new();
    for doc in &index.documents {
        for occ in &doc.occurrences {
            if occ.symbol_roles & 1 == 1
                && occ.range.len() >= 2
                && !occ.symbol.starts_with("local ")
            {
                let pos = (
                    doc.relative_path.clone(),
                    occ.range[0] as u32,
                    occ.range[1] as u32,
                );
                let list = defs.entry(occ.symbol.as_str()).or_default();
                if !list.contains(&pos) {
                    list.push(pos);
                }
            }
        }
    }

    let mut bindings = Vec::new();
    for doc in &index.documents {
        for occ in &doc.occurrences {
            if occ.symbol_roles & 1 == 1 || occ.range.len() < 2 || occ.symbol.starts_with("local ")
            {
                continue;
            }
            let Some(&kind) = kinds.get(occ.symbol.as_str()) else {
                continue;
            };
            if !is_callable(kind) {
                continue;
            }
            let Some(ds) = defs.get(occ.symbol.as_str()) else {
                continue;
            };
            if ds.len() != 1 {
                continue;
            }
            let (callee_file, line0, _col0) = &ds[0];
            bindings.push(ScipCallBinding {
                file: doc.relative_path.clone(),
                line: occ.range[0] as u32 + 1,
                column: Some(occ.range[1] as u32),
                callee_file: callee_file.clone(),
                callee_line: *line0 + 1,
                callee_qualified_name: scip_symbol_to_qualified_name(&occ.symbol),
                dispatch: Some(if kind == Kind::TraitMethod {
                    "trait".to_string()
                } else {
                    "static".to_string()
                }),
            });
        }
    }

    bindings.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.line.cmp(&b.line))
            .then(a.column.cmp(&b.column))
    });
    bindings.dedup();
    Ok((tool, bindings))
}

fn is_callable(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::Method | Kind::StaticMethod | Kind::TraitMethod | Kind::Function | Kind::Macro
    )
}

/// Project a SCIP symbol's descriptor onto the tree-sitter qualified-name
/// grammar (file-relative, `impl`-prefixed):
///
/// | SCIP descriptor | ours |
/// |---|---|
/// | `a/a_fn().` | `a_fn` |
/// | `a/impl#[A]double().` | `A::double` |
/// | `impl#[English][Greeter]greet().` | `English::greet` |
/// | `Greeter#greet().` | `Greeter::greet` |
/// | `mac!.` | `mac` |
///
/// Implemented on the SCIP symbol grammar itself (`scip::symbol::parse_symbol`):
/// rust-analyzer encodes impl methods as `impl#[Type]name` → a `Type` marker
/// named `impl` plus a `TypeParameter` for the impl type (and one for the
/// trait in trait impls); module path segments are dropped (our names are
/// file-relative). Macros (`name!.`) fail the strict parser — a defensive
/// string fallback covers them.
///
/// Diagnostics only — consumers match by definition position.
fn scip_symbol_to_qualified_name(symbol: &str) -> String {
    let Ok(parsed) = scip::symbol::parse_symbol(symbol) else {
        return legacy_symbol_projection(symbol);
    };
    use scip::types::descriptor::Suffix;
    let mut parts: Vec<(&str, Suffix)> = Vec::new();
    for d in &parsed.descriptors {
        let suffix = d.suffix.enum_value_or_default();
        if suffix != Suffix::Namespace {
            parts.push((d.name.as_str(), suffix));
        }
    }
    let Some((idx, (name, _))) = parts
        .iter()
        .enumerate()
        .rev()
        .find(|(_, (_, s))| matches!(s, Suffix::Method | Suffix::Term | Suffix::Macro))
    else {
        return legacy_symbol_projection(symbol);
    };
    let ty = match parts[..idx]
        .iter()
        .position(|(n, s)| *s == Suffix::Type && *n == "impl")
    {
        // RA encodes impl methods as `impl#[Type]name`: an `impl` Type marker
        // followed by the impl type (then the trait, for trait impls).
        Some(i) => parts[i + 1..idx]
            .iter()
            .find(|(_, s)| *s == Suffix::TypeParameter),
        None => parts[..idx]
            .iter()
            .rev()
            .find(|(_, s)| matches!(s, Suffix::Type | Suffix::TypeParameter)),
    };
    match ty {
        Some((ty, _)) => format!("{}::{name}", ty.trim_matches('`')),
        None => (*name).to_string(),
    }
}

/// Fallback for symbols the strict SCIP parser rejects (macros): take the
/// last path segment (or, for crate-root symbols with no `/`, the last
/// whitespace token) and strip the `().` / `!.` suffix.
fn legacy_symbol_projection(symbol: &str) -> String {
    let tail = match symbol.rfind('/') {
        Some(i) => &symbol[i + 1..],
        None => symbol.rsplit(' ').next().unwrap_or(symbol),
    };
    tail.strip_suffix("().")
        .or_else(|| tail.strip_suffix("!."))
        .unwrap_or(tail)
        .to_string()
}

fn bounded_lossy(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() > MAX_RA_OUTPUT_BYTES {
        format!("{}… (truncated)", &text[..MAX_RA_OUTPUT_BYTES])
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/scip-workspace")
    }

    fn artifact_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/scip/index.scip")
    }

    fn binding<'a>(
        bindings: &'a [ScipCallBinding],
        file: &str,
        line: u32,
        column: u32,
    ) -> &'a ScipCallBinding {
        bindings
            .iter()
            .find(|b| b.file == file && b.line == line && b.column == Some(column))
            .unwrap_or_else(|| panic!("no binding for {file}:{line}:{column} in {bindings:#?}"))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("code-parser-scip-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // ── artifact parse (toolchain-free, committed fixture) ──────────────

    #[test]
    fn artifact_parses_and_covers_all_dispatch_shapes() {
        let bindings = from_scip_artifact(&artifact_path(), &fixture_root()).unwrap();
        assert_eq!(bindings.version, SCIP_BINDINGS_VERSION);
        assert!(
            bindings.tool.starts_with("rust-analyzer"),
            "tool: {}",
            bindings.tool
        );
        assert!(!bindings.bindings.is_empty());

        // method call through impl, same file
        let m = binding(&bindings.bindings, "src/lib.rs", 46, 16);
        assert_eq!(m.callee_file, "src/lib.rs");
        assert_eq!(m.callee_line, 31);
        assert_eq!(m.callee_qualified_name, "Cache::get");
        assert_eq!(m.dispatch.as_deref(), Some("static"));

        // associated fn, same file
        let n = binding(&bindings.bindings, "src/lib.rs", 45, 23);
        assert_eq!(n.callee_qualified_name, "Cache::new");
        assert_eq!(n.callee_line, 27);
        assert_eq!(n.dispatch.as_deref(), Some("static"));

        // associated fn cross-file
        let mk = binding(&bindings.bindings, "src/b.rs", 4, 15);
        assert_eq!(mk.callee_file, "src/a.rs");
        assert_eq!(mk.callee_line, 6);
        assert_eq!(mk.callee_qualified_name, "A::make");

        // method call cross-file
        let d = binding(&bindings.bindings, "src/b.rs", 5, 6);
        assert_eq!(d.callee_file, "src/a.rs");
        assert_eq!(d.callee_line, 10);
        assert_eq!(d.callee_qualified_name, "A::double");

        // free fn cross-file
        let f = binding(&bindings.bindings, "src/b.rs", 5, 17);
        assert_eq!(f.callee_file, "src/a.rs");
        assert_eq!(f.callee_line, 15);
        assert_eq!(f.callee_qualified_name, "a_fn");

        // trait dispatch via `dyn Trait`
        let t = binding(&bindings.bindings, "src/lib.rs", 37, 6);
        assert_eq!(t.callee_qualified_name, "Greeter::greet");
        assert_eq!(t.callee_line, 5);
        assert_eq!(t.dispatch.as_deref(), Some("trait"));

        // trait dispatch via generic receiver
        let g = binding(&bindings.bindings, "src/lib.rs", 41, 6);
        assert_eq!(g.callee_qualified_name, "Greeter::greet");
        assert_eq!(g.callee_line, 5);
        assert_eq!(g.dispatch.as_deref(), Some("trait"));

        // every binding stays inside the workspace (no external callees)
        for b in &bindings.bindings {
            assert!(
                b.callee_file.starts_with("src/"),
                "binding escaped the workspace: {b:?}"
            );
        }
    }

    #[test]
    fn artifact_parse_is_deterministic() {
        let a = from_scip_artifact(&artifact_path(), &fixture_root()).unwrap();
        let b = from_scip_artifact(&artifact_path(), &fixture_root()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn definition_positions_land_inside_ir_symbol_spans() {
        let bindings = from_scip_artifact(&artifact_path(), &fixture_root()).unwrap();
        let mut irs = HashMap::new();
        for file in ["src/lib.rs", "src/a.rs", "src/b.rs"] {
            let source = std::fs::read(fixture_root().join(file)).unwrap();
            let ir = code_parser_core::parse_file_bytes(file, &source).unwrap();
            irs.insert(file.to_string(), ir);
        }
        for b in &bindings.bindings {
            let ir = irs.get(&b.callee_file).unwrap_or_else(|| {
                panic!(
                    "callee_file {} of {b:?} is not a fixture file",
                    b.callee_file
                )
            });
            assert!(
                b.aligns_with_ir(ir),
                "callee position {}:{} of {b:?} lands outside every symbol span of {}",
                b.callee_file,
                b.callee_line,
                b.callee_file
            );
        }
    }

    // ── qualified-name projection ───────────────────────────────────────

    #[test]
    fn symbol_projection_matches_our_grammar() {
        assert_eq!(
            scip_symbol_to_qualified_name("rust-analyzer cargo scip_fixture 0.1.0 a/a_fn()."),
            "a_fn"
        );
        assert_eq!(
            scip_symbol_to_qualified_name(
                "rust-analyzer cargo scip_fixture 0.1.0 a/impl#[A]double()."
            ),
            "A::double"
        );
        assert_eq!(
            scip_symbol_to_qualified_name(
                "rust-analyzer cargo scip_fixture 0.1.0 impl#[English][Greeter]greet()."
            ),
            "English::greet"
        );
        assert_eq!(
            scip_symbol_to_qualified_name(
                "rust-analyzer cargo scip_fixture 0.1.0 Greeter#greet()."
            ),
            "Greeter::greet"
        );
        assert_eq!(
            scip_symbol_to_qualified_name("rust-analyzer cargo scip_fixture 0.1.0 mac!."),
            "mac"
        );
        // generic impl types carry backticks in RA symbols — trimmed
        assert_eq!(
            scip_symbol_to_qualified_name(
                "rust-analyzer cargo alloc 0.0.0 vec/impl#[`Vec<T>`]new()."
            ),
            "Vec<T>::new"
        );
    }

    // ── fingerprint (unit: equality is the cache contract) ──────────────

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_source() {
        let files = vec![
            ("src/lib.rs".to_string(), b"fn a() {}".to_vec()),
            ("src/a.rs".to_string(), b"fn b() {}".to_vec()),
        ];
        let packages = vec!["scip_fixture@0.1.0[]".to_string()];
        let fp1 = fingerprint_inputs(1, "rust-analyzer 1.95.0", &packages, &files);
        let fp2 = fingerprint_inputs(1, "rust-analyzer 1.95.0", &packages, &files);
        assert_eq!(fp1, fp2, "identical inputs must hash identically");
        assert_eq!(fp1.len(), 16, "fingerprint is a truncated blake3 hex");

        let moved = vec![
            ("src/a.rs".to_string(), b"fn b() {}".to_vec()),
            ("src/lib.rs".to_string(), b"fn a() {}".to_vec()),
        ];
        assert_eq!(
            fingerprint_inputs(1, "rust-analyzer 1.95.0", &packages, &moved),
            fp1,
            "order-insensitive: sorted input order must not matter"
        );

        let edited = vec![
            ("src/lib.rs".to_string(), b"fn a() { let x = 1; }".to_vec()),
            ("src/a.rs".to_string(), b"fn b() {}".to_vec()),
        ];
        assert_ne!(
            fingerprint_inputs(1, "rust-analyzer 1.95.0", &packages, &edited),
            fp1,
            "source edit must change the fingerprint"
        );

        let bumped = fingerprint_inputs(2, "rust-analyzer 1.95.0", &packages, &files);
        assert_ne!(
            bumped, fp1,
            "bindings version bump must change the fingerprint"
        );

        let new_ra = fingerprint_inputs(1, "rust-analyzer 1.96.0", &packages, &files);
        assert_ne!(
            new_ra, fp1,
            "indexer version change must change the fingerprint"
        );

        let new_pkg = fingerprint_inputs(
            1,
            "rust-analyzer 1.95.0",
            &["other@2.0.0[rust]".to_string()],
            &files,
        );
        assert_ne!(
            new_pkg, fp1,
            "package identity change must change the fingerprint"
        );
    }

    #[test]
    fn source_files_walk_excludes_target_and_orders_by_path() {
        let tmp = temp_dir("walk");
        std::fs::create_dir_all(tmp.join("src/nested")).unwrap();
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(tmp.join("src/z.rs"), "fn z() {}").unwrap();
        std::fs::write(tmp.join("src/nested/a.rs"), "fn a() {}").unwrap();
        std::fs::write(tmp.join("target/built.rs"), "fn built() {}").unwrap();
        std::fs::write(tmp.join("notes.txt"), "not source").unwrap();

        let files = source_files(&tmp);
        let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec!["Cargo.toml", "src/nested/a.rs", "src/z.rs"],
            "target/ and non-source files must be excluded, paths sorted"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── subprocess path (toolchain tests, run explicitly) ───────────────

    #[test]
    fn generate_with_missing_binary_errors_cleanly() {
        let tmp = temp_dir("missing");
        let err = generate_with_ra(
            &fixture_root(),
            &tmp.join("cache"),
            "/nonexistent/rust-analyzer-xyz",
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("rust-analyzer") && text.contains("rustup component add rust-analyzer"),
            "error must be actionable: {text}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[ignore = "requires the rustup rust-analyzer component (D0 spike installed it)"]
    fn generate_matches_committed_artifact_and_reuses_cache() {
        let root = fixture_root();
        let tmp = temp_dir("cache");
        let cache = tmp.join("cache");
        let first = generate(&root, &cache)
            .expect("generate requires rust-analyzer (rustup component add rust-analyzer)");
        assert!(!first.bindings.is_empty());
        assert_eq!(first.version, SCIP_BINDINGS_VERSION);

        // The committed artifact and a fresh generate must project identical
        // bindings (byte-deterministic RA output ⇒ same artifact).
        let committed = from_scip_artifact(&artifact_path(), &root).unwrap();
        assert_eq!(
            first.bindings, committed.bindings,
            "fresh generate diverges from committed fixture artifact"
        );

        // Second run: swap the indexer for a sentinel that fails on `scip`
        // but proxies `--version`. A cache hit must never spawn `scip`.
        let marker = tmp.join("spawned.marker");
        let script = tmp.join("fake-ra.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"scip\" ]; then echo spawned >> \"{}\"; exit 1; fi\nexec rust-analyzer \"$@\"\n",
                marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let second = generate_with_ra(&root, &cache, script.to_str().unwrap()).expect("cache hit");
        assert_eq!(
            second, first,
            "cached artifact must be byte-identical to the first generate"
        );
        assert!(
            !marker.exists(),
            "rust-analyzer `scip` was spawned despite an unchanged fingerprint"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
