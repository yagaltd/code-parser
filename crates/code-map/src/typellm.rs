//! TypeLLM provider (typellm.ai) — ported from mailbox-parser `cli/src/typellm.rs`.
//! Type-safe generation on autoregressive LLMs: one endpoint, `POST /v1/generate`
//! with `{context, questions}` → `{result, thinking, usage}`. Beyond the Jev
//! battery it adds free-text answers, number/integer types, thinking mode,
//! image input and `depends_on` dependency graphs.
//!
//! Key chain (file FIRST so two projects on one machine keep separate keys):
//! `~/.config/code-parser/typellm.key` (chmod 600) → `TYPELLM_API_KEY` env.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Hosted TypeLLM API.
pub const HOSTED_URL: &str = "https://api.typellm.ai";

pub fn default_key_file() -> PathBuf {
    #[allow(deprecated)]
    let home = std::env::home_dir();
    home.map(|h| h.join(".config").join("code-parser").join("typellm.key"))
        .unwrap_or_else(|| PathBuf::from("/tmp/code-parser-typellm.key"))
}

/// Key chain: explicit path → default key file → `TYPELLM_API_KEY` env.
pub fn load_key(explicit: Option<&Path>) -> Result<String> {
    let mut tried: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        tried.push(p.to_path_buf());
        if let Ok(k) = std::fs::read_to_string(p) {
            let k = k.trim();
            if !k.is_empty() {
                return Ok(k.into());
            }
        }
    }
    let d = default_key_file();
    tried.push(d.clone());
    if let Ok(k) = std::fs::read_to_string(&d) {
        let k = k.trim();
        if !k.is_empty() {
            return Ok(k.into());
        }
    }
    if let Ok(k) = std::env::var("TYPELLM_API_KEY") {
        let k = k.trim();
        if !k.is_empty() {
            return Ok(k.into());
        }
    }
    bail!(
        "no TypeLLM API key: run `code-map typellm setup`, write it to {} (chmod 600), or set TYPELLM_API_KEY",
        d.display()
    );
}

pub fn write_key_file(path: &Path, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("empty API key");
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// One-time credential setup (mailbox-parser UX): prompts, reads stdin, chmod 600.
pub fn run_setup(explicit: Option<&Path>) -> Result<()> {
    let path = explicit.map(Path::to_path_buf).unwrap_or_else(default_key_file);
    eprintln!(
        "Paste the TypeLLM API key (piped input works too: echo $KEY | code-map typellm setup):"
    );
    let mut key = String::new();
    std::io::stdin().read_line(&mut key)?;
    write_key_file(&path, &key)?;
    let k = key.trim();
    let masked = match (k.get(..4), k.get(k.len().saturating_sub(4)..)) {
        (Some(a), Some(b)) if k.len() >= 8 => format!("{a}…{b}"),
        _ => "***".to_string(),
    };
    eprintln!(
        "key written to {} ({masked}, chmod 600); per-run overrides: --key-file, TYPELLM_API_KEY",
        path.display()
    );
    Ok(())
}

/// POST {base}/v1/generate with the Bearer key. Returns the parsed JSON.
pub fn post_generate(base_url: &str, key: &str, body: &Value) -> Result<Value> {
    let url = format!("{}/v1/generate", base_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(60))
        .send_json(body.clone())
        .map_err(|e| match e {
            ureq::Error::Status(status, r) => {
                let body = r.into_string().unwrap_or_default();
                anyhow::anyhow!("TypeLLM API error (HTTP {status}): {}", &body[..body.len().min(300)])
            }
            e => anyhow::anyhow!("TypeLLM API call failed: {e}"),
        })?;
    let mut raw = Vec::new();
    resp.into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut raw)
        .context("failed to read TypeLLM response")?;
    serde_json::from_slice(&raw).context("TypeLLM API returned non-JSON")
}

/// One small /v1/generate call: proves the key, the endpoint and the typed
/// answers (string + number + boolean + enum) all work with this key.
pub fn run_verify(explicit: Option<&Path>) -> Result<()> {
    let key = load_key(explicit)?;
    let body = json!({
        "context": "Receipt from Cafe Aurora\nFlat white £3.20\nTotal: £12.40\nPaid by card.",
        "questions": {
            "merchant": {"type": "string", "instructions": "Return only the merchant name."},
            "total": {"type": "number", "instructions": "Extract the total amount as a number."},
            "currency_gbp": {"type": "boolean", "instructions": "Is the currency GBP?"},
            "kind": {"type": "string", "enum": ["receipt", "invoice", "quote"],
                     "instructions": "What kind of document is this?"}
        }
    });
    let resp = post_generate(HOSTED_URL, &key, &body)?;
    let result = resp.get("result").cloned().unwrap_or_else(|| resp.clone());
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_key_file_creates_dirs_and_restricts_perms() {
        let dir = std::env::temp_dir().join(format!("typellm_key_test_{}", std::process::id()));
        let path = dir.join("nested").join("typellm.key");
        write_key_file(&path, "  tl-sk-test-12345678  ").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let stored = std::fs::read_to_string(&path).unwrap();
        assert_eq!(stored, "tl-sk-test-12345678");
        assert!(write_key_file(&path, "   ").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
