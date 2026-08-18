//! TS/JS import specifier resolution — repo-level pass (fix D).
//!
//! `ImportIR.target_module` is never rewritten: it keeps the raw specifier.
//! The new `ImportIR.resolved` field carries the repo-relative target when
//! the specifier maps to a real file in the repo — relative (`./` `../`)
//! paths normalized against the importing file, extension probing
//! (`.ts` `.tsx` `.js` `.jsx`, then `/index.*`), and `tsconfig.json`
//! `baseUrl`/`paths` aliases. Package specifiers (`react`, `lodash`) and
//! missing tsconfigs behave exactly as before (`resolved: None`).
//!
//! Wired into [`crate::parse_repo`] right after `resolve_cross_file`.
//! `parse_file_bytes` (single-file path) stays syntactic-only by design —
//! repo-level ingests fill `resolved` via [`resolve_import_paths`].

use std::collections::HashMap;
use std::path::{Component, Path};

use code_parser_ir::{FileParseIR, ImportIR};

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
pub fn resolve_import_paths(irs: &mut [FileParseIR], root: &Path) {
    let cfg = load_tsconfig(root);
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
            let ri = resolve_import(&spec, &from_file, cfg.as_ref());
            let Some(candidate) = ri.target_path else { continue };
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
        let dir = std::env::temp_dir().join(format!(
            "code-parser-imports-probe-{}",
            std::process::id()
        ));
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
        let results = crate::parse_repo(
            &root,
            Some(vec![crate::language::Language::TypeScript]),
        )
        .unwrap();

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
}
