//! TS/JS import specifier resolution — repo-level pass (fix D).
//!
//! `ImportIR.target_module` is never rewritten: it keeps the raw specifier.
//! The new `ImportIR.resolved` field carries the repo-relative target when
//! the specifier maps to a real file in the repo — relative (`./` `../`)
//! paths normalized against the importing file, extension probing
//! (`.ts` `.tsx` `.js` `.jsx`, then `/index.*`), and `tsconfig.json`
//! `baseUrl`/`paths` aliases from the **nearest** tsconfig on the importing
//! file's ancestor chain (`TsConfigSet` — monorepo / project-references
//! aware, `extends` merged; see the README Resolution section for
//! semantics). Package specifiers (`react`, `lodash`) and missing tsconfigs
//! behave exactly as before (`resolved: None`).
//!
//! Wired into [`crate::parse_repo`] right after `resolve_cross_file`.
//! `parse_file_bytes` (single-file path) stays syntactic-only by design —
//! repo-level ingests fill `resolved` via [`resolve_import_paths`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use code_parser_ir::FileParseIR;

/// `tsconfig.json` compiler options that matter for import resolution.
#[derive(Debug, Clone, Default)]
pub struct TsConfig {
    /// `compilerOptions.baseUrl` — directory (relative to the tsconfig's
    /// location) that `paths` targets are resolved against.
    pub base_url: Option<String>,
    /// `compilerOptions.paths` — alias prefix → target patterns (in the order
    /// written). `*` in a key matches any suffix; the first matching key wins.
    pub paths: HashMap<String, Vec<String>>,
}

/// Resolved import: repo-relative target when it maps to a file in the repo,
/// raw specifier otherwise (external package).
///
/// [`resolve_import`] computes the normalized candidate path without touching
/// the filesystem (extension/index probing and existence checks happen in
/// [`resolve_import_paths`], which has the repo root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImport {
    pub specifier: String,
    pub target_path: Option<String>,
}

/// Load `tsconfig.json` (nearest to `root`, walking up), parse `baseUrl` +
/// `paths`. Returns `None` when no tsconfig exists or it has neither key
/// (parity guard: resolution then behaves exactly as before).
pub fn load_tsconfig(root: &Path) -> Option<TsConfig> {
    let mut dir = root.to_path_buf();
    loop {
        let candidate = dir.join("tsconfig.json");
        if candidate.is_file() {
            return parse_tsconfig(&candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Cap on `extends` chain depth (cycle guard).
const EXTENDS_MAX_DEPTH: usize = 10;

/// All `tsconfig.json` files under a repo root, keyed by their repo-relative
/// directory — the monorepo / project-references model. For each importing
/// file, [`TsConfigSet::for_file`] returns the **nearest** tsconfig in its
/// ancestor chain (TS project semantics: a file belongs to exactly one
/// project; ancestor configs are not consulted as alias fallbacks).
/// `extends` chains are merged (child overrides; `paths` replace wholesale
/// when declared) and `baseUrl` is resolved against the directory of the
/// config that declares it (TS ≥ 4.1 semantics).
#[derive(Debug, Default, Clone)]
pub struct TsConfigSet {
    /// Repo-relative config dir ("" for the root) → effective config.
    /// `base_url` is already rewritten to the repo-relative prefix that
    /// `paths` targets resolve against ("" = repo root).
    configs: BTreeMap<String, TsConfig>,
    /// Legacy fallback: a config found *above* `root` (the pre-nesting
    /// `load_tsconfig` up-walk) when no tsconfig exists inside the root.
    /// Applies repo-wide with raw (root-relative) semantics.
    fallback: Option<TsConfig>,
}

impl TsConfigSet {
    /// Discover every `tsconfig.json` under `root` (gitignore-aware,
    /// `.git` skipped) and resolve each against its `extends` chain.
    /// Configs with neither `baseUrl` nor `paths` have no alias power and
    /// are skipped — files under them fall through to the next ancestor.
    pub fn discover(root: &Path) -> Self {
        let mut set = TsConfigSet::default();
        let walker = ignore::WalkBuilder::new(root)
            .standard_filters(true)
            .hidden(false)
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if entry.file_name() != "tsconfig.json" {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else {
                continue;
            };
            let dir = rel
                .parent()
                .map(|p| normalize_repo_path(p))
                .unwrap_or_default();
            if let Some(cfg) = load_effective(entry.path(), root) {
                set.configs.insert(dir, cfg);
            }
        }
        if set.configs.is_empty() {
            set.fallback = load_tsconfig(root);
        }
        set
    }

    /// Effective config governing `from_file` (repo-relative path): the
    /// nearest discovered config directory on its ancestor chain, else the
    /// legacy fallback.
    pub fn for_file(&self, from_file: &str) -> Option<&TsConfig> {
        let file_dir = Path::new(from_file)
            .parent()
            .map(normalize_repo_path)
            .unwrap_or_default();
        let mut best: Option<(&String, &TsConfig)> = None;
        for (dir, cfg) in &self.configs {
            let governs =
                dir.is_empty() || file_dir == *dir || file_dir.starts_with(&format!("{dir}/"));
            if governs && best.is_none_or(|(bd, _)| dir.len() > bd.len()) {
                best = Some((dir, cfg));
            }
        }
        best.map(|(_, cfg)| cfg).or(self.fallback.as_ref())
    }

    /// Number of discovered configs (test/debug aid).
    pub fn len(&self) -> usize {
        self.configs.len()
    }

    /// True when no configs were discovered.
    pub fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }
}

/// `compilerOptions` of one tsconfig before `extends` resolution.
#[derive(Default)]
struct RawOpts {
    base_url: Option<String>,
    /// `Some` only when the `paths` key is present (empty object replaces).
    paths: Option<HashMap<String, Vec<String>>>,
    extends: Option<String>,
}

/// Effective config for one `tsconfig.json`: walk its `extends` chain
/// (relative specifiers only; package-style extends targets are external),
/// merge child-first (child overrides; `paths` replace wholesale when the
/// key exists), and rewrite `baseUrl` — resolved against the declaring
/// config's directory — into a repo-relative prefix. When no `baseUrl` is
/// declared anywhere, `paths` targets resolve against the directory of the
/// config that declares them (TS ≥ 4.1).
fn load_effective(config_path: &Path, root: &Path) -> Option<TsConfig> {
    let mut chain: Vec<(PathBuf, RawOpts)> = Vec::new();
    let mut current = config_path.to_path_buf();
    for _ in 0..EXTENDS_MAX_DEPTH {
        let Some(raw) = parse_raw_opts(&current) else {
            break;
        };
        let next = raw.extends.as_ref().map(|ext| {
            // TS appends `.json` when missing; multi-dot names must survive.
            let dir = current.parent().unwrap_or(Path::new(""));
            if ext.ends_with(".json") {
                dir.join(ext)
            } else {
                dir.join(format!("{ext}.json"))
            }
        });
        chain.push((current.clone(), raw));
        match next {
            Some(p) if p.is_file() => current = p,
            _ => break,
        }
    }

    // Merge child → base: the first declaration wins (child overrides).
    // Config dirs are repo-relative (extends targets above `root` degrade
    // gracefully — their targets simply never probe to a real file).
    let rel_dir = |config_file: &Path| -> String {
        let d = config_file.parent().unwrap_or(Path::new(""));
        d.strip_prefix(root)
            .map(normalize_repo_path)
            .unwrap_or_else(|_| normalize_repo_path(d))
    };
    let mut base_decl: Option<(String, String)> = None;
    let mut paths_decl: Option<(HashMap<String, Vec<String>>, String)> = None;
    for (dir, raw) in &chain {
        let dir_rel = rel_dir(dir);
        if base_decl.is_none() {
            if let Some(b) = &raw.base_url {
                base_decl = Some((b.clone(), dir_rel.clone()));
            }
        }
        if paths_decl.is_none() {
            if let Some(p) = &raw.paths {
                paths_decl = Some((p.clone(), dir_rel));
            }
        }
    }

    let (paths, paths_dir) = paths_decl?; // no paths → no alias power
    let base_prefix = match base_decl {
        Some((raw, dir)) => normalize_repo_path(&Path::new(&dir).join(raw)),
        None => paths_dir,
    };

    let mut cfg = TsConfig {
        base_url: None,
        paths,
    };
    if !base_prefix.is_empty() {
        cfg.base_url = Some(base_prefix);
    }
    Some(cfg)
}

/// Read one tsconfig's `compilerOptions.baseUrl` / `paths` and top-level
/// `extends` (string, or first string of a TS 5 array).
fn parse_raw_opts(path: &Path) -> Option<RawOpts> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut raw = RawOpts::default();
    if let Some(opts) = json.get("compilerOptions") {
        if let Some(b) = opts.get("baseUrl").and_then(|v| v.as_str()) {
            raw.base_url = Some(b.trim_end_matches('/').to_string());
        }
        if let Some(p) = opts.get("paths").and_then(|v| v.as_object()) {
            let mut map = HashMap::new();
            for (key, targets) in p {
                let list: Vec<String> = targets
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if !list.is_empty() {
                    map.insert(key.clone(), list);
                }
            }
            raw.paths = Some(map);
        }
    }
    match json.get("extends") {
        Some(serde_json::Value::String(s)) => raw.extends = Some(s.clone()),
        Some(serde_json::Value::Array(arr)) => {
            raw.extends = arr.iter().find_map(|v| v.as_str().map(String::from))
        }
        _ => {}
    }
    Some(raw)
}

fn parse_tsconfig(path: &Path) -> Option<TsConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let opts = json.get("compilerOptions")?;
    let mut cfg = TsConfig::default();
    if let Some(b) = opts.get("baseUrl").and_then(|v| v.as_str()) {
        cfg.base_url = Some(b.trim_end_matches('/').to_string());
    }
    if let Some(p) = opts.get("paths").and_then(|v| v.as_object()) {
        for (key, targets) in p {
            let list: Vec<String> = targets
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if !list.is_empty() {
                cfg.paths.insert(key.clone(), list);
            }
        }
    }
    if cfg.base_url.is_none() && cfg.paths.is_empty() {
        return None;
    }
    Some(cfg)
}

/// Resolve one specifier from a given file.
///
/// - Relative (`./` `../`): normalize against the importing file's directory
///   → candidate repo-relative path (extension probing happens downstream).
/// - tsconfig `paths` match (wildcard or exact): first matching key, first
///   target pattern (resolved against `baseUrl`, or the tsconfig dir when
///   unset) → candidate repo-relative path.
/// - Anything else (package specifier): `target_path: None` (external).
pub fn resolve_import(spec: &str, from_file: &str, cfg: Option<&TsConfig>) -> ResolvedImport {
    let specifier = spec.to_string();
    if spec.starts_with("./") || spec.starts_with("../") {
        let from_dir = Path::new(from_file).parent().unwrap_or(Path::new(""));
        let joined = from_dir.join(spec);
        return ResolvedImport {
            specifier,
            target_path: Some(normalize_repo_path(&joined)),
        };
    }
    if let Some(cfg) = cfg {
        if let Some(first) = alias_candidates(spec, cfg).into_iter().next() {
            return ResolvedImport {
                specifier,
                target_path: Some(first),
            };
        }
    }
    ResolvedImport {
        specifier,
        target_path: None,
    }
}

/// Repo-level pass: fill `ImportIR.resolved` for TypeScript/JavaScript
/// imports whose specifier maps to a real file under `root`.
///
/// Probe order (fix D acceptance): exact candidate, then `.ts` `.tsx` `.js`
/// `.jsx`, then `/index.ts` `/index.tsx` `/index.js` `/index.jsx` — the
/// first existing file wins. Rust/Python imports are left untouched.
/// Aliases come from the **nearest** `tsconfig.json` for each importing
/// file (see [`TsConfigSet`]).
pub fn resolve_import_paths(irs: &mut [FileParseIR], root: &Path) {
    let configs = TsConfigSet::discover(root);
    resolve_import_paths_with(irs, root, &configs);
}

/// [`resolve_import_paths`] with a pre-discovered [`TsConfigSet`] — use
/// when resolving repeatedly against the same tree (e.g. [`crate::RepoState`]).
pub fn resolve_import_paths_with(irs: &mut [FileParseIR], root: &Path, configs: &TsConfigSet) {
    for ir in irs.iter_mut() {
        if ir.language != "TypeScript" && ir.language != "JavaScript" {
            continue;
        }
        let from_file = ir.path.clone();
        for imp in &mut ir.imports {
            if imp.resolved.is_some() {
                continue;
            }
            let spec = imp.target_module.clone();
            let ri = resolve_import(&spec, &from_file, configs.for_file(&from_file));
            let Some(candidate) = ri.target_path else {
                continue;
            };
            if let Some(hit) = probe_existing(root, &candidate) {
                imp.resolved = Some(hit);
            }
        }
    }
}

/// All `paths` mapping candidates for a specifier (sorted keys for
/// determinism; `*` wildcard substitution). Targets are resolved against
/// `baseUrl` (or the tsconfig dir when unset).
fn alias_candidates(spec: &str, cfg: &TsConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut keys: Vec<&String> = cfg.paths.keys().collect();
    keys.sort();
    for key in keys {
        let targets = &cfg.paths[key];
        let matched: Option<String> = if let Some(star) = key.find('*') {
            // Wildcard key: "~/*" matches "~/lib/x" via its "~/" prefix;
            // the matched suffix substitutes the `*` in each target pattern.
            let prefix = &key[..star];
            spec.strip_prefix(prefix).map(|suffix| suffix.to_string())
        } else if spec == key {
            Some(String::new())
        } else {
            None
        };
        let Some(suffix) = matched else { continue };
        for t in targets {
            if let Some(tstar) = t.find('*') {
                let mut cand = String::new();
                cand.push_str(&t[..tstar]);
                cand.push_str(&suffix);
                cand.push_str(&t[tstar + 1..]);
                out.push(cand);
            } else {
                out.push(t.clone());
            }
        }
        // TS semantics: first matching key wins.
        if !out.is_empty() {
            break;
        }
    }
    let base = cfg.base_url.as_deref().unwrap_or(".").trim_end_matches('/');
    if !base.is_empty() && base != "." {
        for c in &mut out {
            *c = format!("{base}/{c}");
        }
    }
    out
}

/// Lexically normalize a repo-relative path (`./`/`../`/`//` collapse).
fn normalize_repo_path(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(c) => parts.push(c.to_string_lossy().into_owned()),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

/// Probe a candidate repo-relative path under `root` with extension/index
/// fallbacks. Returns the first existing repo-relative path, or `None`.
fn probe_existing(root: &Path, candidate: &str) -> Option<String> {
    let last = candidate.rsplit('/').next().unwrap_or(candidate);
    let has_extension = last.contains('.');
    let mut variants: Vec<String> = Vec::new();
    variants.push(candidate.to_string());
    if !has_extension {
        for ext in [".ts", ".tsx", ".js", ".jsx"] {
            variants.push(format!("{candidate}{ext}"));
        }
        for ext in [".ts", ".tsx", ".js", ".jsx"] {
            variants.push(format!("{candidate}/index{ext}"));
        }
    }
    variants.into_iter().find(|v| root.join(v).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_parser_ir::{ImportIR, ImportKind};

    fn ts_cfg() -> TsConfig {
        let mut paths = HashMap::new();
        paths.insert("~/*".to_string(), vec!["src/*".to_string()]);
        paths.insert("@lib/*".to_string(), vec!["src/lib/*".to_string()]);
        TsConfig {
            base_url: Some(".".to_string()),
            paths,
        }
    }

    #[test]
    fn relative_specifier_normalizes_against_from_file() {
        let cfg = ts_cfg();
        let r = resolve_import("./util", "src/a/b.ts", Some(&cfg));
        assert_eq!(r.target_path.as_deref(), Some("src/a/util"));
        let r = resolve_import("../util", "src/a/b.ts", Some(&cfg));
        assert_eq!(r.target_path.as_deref(), Some("src/util"));
        let r = resolve_import("../../x/y", "src/a/b/c.ts", Some(&cfg));
        assert_eq!(r.target_path.as_deref(), Some("src/x/y"));
    }

    #[test]
    fn tsconfig_paths_alias_mapping() {
        let cfg = ts_cfg();
        let r = resolve_import("~/lib/x", "src/a.ts", Some(&cfg));
        assert_eq!(r.target_path.as_deref(), Some("src/lib/x"));
        let r = resolve_import("@lib/y", "src/a.ts", Some(&cfg));
        assert_eq!(r.target_path.as_deref(), Some("src/lib/y"));
    }

    #[test]
    fn package_specifiers_stay_external() {
        let cfg = ts_cfg();
        for spec in ["react", "lodash", "@scope/pkg"] {
            let r = resolve_import(spec, "src/a.ts", Some(&cfg));
            assert_eq!(r.target_path, None, "{spec} must stay external");
        }
    }

    #[test]
    fn missing_tsconfig_parity() {
        // Without a tsconfig: relative resolution still works, aliases don't.
        let r = resolve_import("./util", "src/b.ts", None);
        assert_eq!(r.target_path.as_deref(), Some("src/util"));
        let r = resolve_import("~/lib/x", "src/a.ts", None);
        assert_eq!(r.target_path, None);
    }

    #[test]
    fn extension_probe_order_and_index_fallback() {
        // Real-filesystem probing (no mocks): temp dir as the repo root.
        let dir =
            std::env::temp_dir().join(format!("code-parser-imports-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/util.ts"), "export const util = 1;\n").unwrap();
        std::fs::write(dir.join("src/util.tsx"), "export const util = 1;\n").unwrap();
        std::fs::write(dir.join("src/util.js"), "export const util = 1;\n").unwrap();
        std::fs::create_dir_all(dir.join("src/missing")).unwrap();
        std::fs::write(
            dir.join("src/missing/index.ts"),
            "export const fallback = 1;\n",
        )
        .unwrap();

        let mut ir = FileParseIR::empty("src/b.ts", "TypeScript");
        ir.imports.push(ImportIR {
            import_name: "util".into(),
            target_module: "./util".into(),
            kind: ImportKind::Named,
            line: None,
            column: None,
            resolved: None,
        });
        ir.imports.push(ImportIR {
            import_name: "fallback".into(),
            target_module: "./missing".into(),
            kind: ImportKind::Named,
            line: None,
            column: None,
            resolved: None,
        });
        let mut irs = vec![ir];
        resolve_import_paths(&mut irs, &dir);
        // .ts wins over .tsx/.js; /index.ts is the fallback for ./missing.
        assert_eq!(irs[0].imports[0].resolved.as_deref(), Some("src/util.ts"));
        assert_eq!(
            irs[0].imports[1].resolved.as_deref(),
            Some("src/missing/index.ts")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn e2e_parse_repo_fills_resolved_for_alias_fixture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ts/alias");
        let results =
            crate::parse_repo(&root, Some(vec![crate::language::Language::TypeScript])).unwrap();

        let by_path: HashMap<&str, &FileParseIR> = results
            .iter()
            .map(|r| (r.ir.path.as_str(), &r.ir))
            .collect();
        assert_eq!(by_path.len(), 3, "tsconfig.json must not be collected");

        // Relative specifier from src/a/b.ts → src/util.ts.
        let b = by_path.get("src/a/b.ts").expect("src/a/b.ts");
        let util = b
            .imports
            .iter()
            .find(|i| i.import_name == "util")
            .expect("util import");
        assert_eq!(util.target_module, "../util", "raw specifier must be kept");
        assert_eq!(util.resolved.as_deref(), Some("src/util.ts"));

        // tsconfig paths alias ~/lib/x → src/lib/x.ts.
        let x = b
            .imports
            .iter()
            .find(|i| i.import_name == "x")
            .expect("x import");
        assert_eq!(x.target_module, "~/lib/x", "raw specifier must be kept");
        assert_eq!(x.resolved.as_deref(), Some("src/lib/x.ts"));

        // Package specifier → external, unchanged behavior.
        let react = b
            .imports
            .iter()
            .find(|i| i.target_module == "react")
            .expect("react import");
        assert_eq!(react.resolved, None);
    }

    #[test]
    fn e2e_nested_tsconfigs_nearest_match_and_extends() {
        // Monorepo fixture: packages/a extends tsconfig.base.json (own
        // baseUrl), packages/b has no config → governed by the root one.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ts/nested");
        let results =
            crate::parse_repo(&root, Some(vec![crate::language::Language::TypeScript])).unwrap();

        let by_path: HashMap<&str, &FileParseIR> = results
            .iter()
            .map(|r| (r.ir.path.as_str(), &r.ir))
            .collect();

        // packages/a/src/index.ts: nearest config = packages/a/tsconfig.json
        // → extends base (paths ~/* → src/*), child baseUrl "." → packages/a.
        // Same alias `~/*` resolves DIFFERENTLY per package:
        let a = by_path.get("packages/a/src/index.ts").expect("index.ts");
        let util = a
            .imports
            .iter()
            .find(|i| i.target_module == "~/util")
            .unwrap();
        assert_eq!(
            util.resolved.as_deref(),
            Some("packages/a/src/util.ts"),
            "nearest config with extends must govern package files"
        );
        let react = a
            .imports
            .iter()
            .find(|i| i.target_module == "react")
            .unwrap();
        assert_eq!(react.resolved, None);

        // packages/b/src/i.ts: no nested config → root tsconfig (~/* → root-src/*).
        let b = by_path.get("packages/b/src/i.ts").expect("i.ts");
        let x = b.imports.iter().find(|i| i.target_module == "~/x").unwrap();
        assert_eq!(x.resolved.as_deref(), Some("root-src/x.ts"));
    }

    #[test]
    fn tsconfig_set_discovery_and_extends_merge() {
        // Synthetic monorepo in a temp dir: child extends base, overrides
        // baseUrl; base declares the shared paths.
        let dir = std::env::temp_dir().join(format!("code-parser-tsset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("packages/a/src")).unwrap();
        std::fs::write(
            dir.join("tsconfig.base.json"),
            r#"{"compilerOptions":{"baseUrl":".","paths":{"~/*":["src/*"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("packages/a/tsconfig.json"),
            r#"{"extends":"../../tsconfig.base.json","compilerOptions":{"baseUrl":"."}}"#,
        )
        .unwrap();

        let set = TsConfigSet::discover(&dir);
        assert_eq!(set.len(), 1, "only packages/a has alias power");

        // baseUrl "." declared by the CHILD → prefix is packages/a.
        let cfg = set
            .for_file("packages/a/src/index.ts")
            .expect("effective config");
        assert_eq!(cfg.base_url.as_deref(), Some("packages/a"));
        assert!(cfg.paths.contains_key("~/*"));
        // Files elsewhere are governed by no config (base has no tsconfig.json).
        assert!(set.for_file("other/src/x.ts").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
