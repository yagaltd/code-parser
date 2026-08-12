//! code-parser CLI — parse, parse-repo, watch, check.
//!
//! Usage:
//!   code-parser parse <FILE> --json
//!   code-parser parse-repo <DIR> --jsonl
//!   code-parser watch <DIR> --emit jsonl       (requires --features watcher)
//!   code-parser check <FILE>

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "code-parser", version = "0.1.0")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a single file and emit IR as JSON.
    Parse {
        /// Path to the source file.
        file: PathBuf,
        /// Emit IR as JSON to stdout.
        #[arg(long)]
        json: bool,
        /// Print only the file retrieval card text (debug).
        #[arg(long)]
        card_only: bool,
    },
    /// Parse all source files in a directory.
    ParseRepo {
        /// Root directory of the repository.
        dir: PathBuf,
        /// Emit results as newline-delimited JSON (JSONL).
        #[arg(long)]
        jsonl: bool,
        /// Comma-separated list of languages (rust,typescript,python).
        #[arg(long)]
        languages: Option<String>,
    },
    /// Watch a directory for changes and emit IR on file modification.
    Watch {
        /// Root directory to watch.
        dir: PathBuf,
        /// Output format: jsonl (line-delimited JSON).
        #[arg(long, default_value = "jsonl")]
        emit: String,
    },
    /// Validate a file parses without errors (no IR dump).
    Check {
        /// Path to the source file.
        file: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Parse {
            file,
            json,
            card_only,
        } => cmd_parse(file, json, card_only),
        Command::ParseRepo {
            dir,
            jsonl,
            languages,
        } => cmd_parse_repo(dir, jsonl, languages),
        Command::Watch { dir, emit: _ } => cmd_watch(dir),
        Command::Check { file } => cmd_check(file),
    }
}

fn cmd_parse(file: PathBuf, json: bool, card_only: bool) -> anyhow::Result<()> {
    let result = code_parser_core::parse_file(&file)
        .with_context(|| format!("Failed to parse {}", file.display()))?;

    if card_only {
        println!("{}", result.ir.retrieval_card.text);
    } else if json {
        let output = serde_json::to_string_pretty(&result.ir)?;
        println!("{output}");
    } else {
        println!("path: {}", result.ir.path);
        println!("language: {}", result.ir.language);
        println!("symbols: {}", result.ir.symbols.len());
        println!("calls: {}", result.ir.calls.len());
        println!("imports: {}", result.ir.imports.len());
        println!("diagnostics: {}", result.ir.diagnostics.len());
    }

    for e in &result.errors {
        eprintln!("warning: {e}");
    }
    Ok(())
}

fn cmd_parse_repo(dir: PathBuf, jsonl: bool, languages: Option<String>) -> anyhow::Result<()> {
    let languages = languages.map(|s| {
        s.split(',')
            .filter_map(|l| match l.trim() {
                "rust" => Some(code_parser_core::language::Language::Rust),
                "typescript" | "ts" => Some(code_parser_core::language::Language::TypeScript),
                "javascript" | "js" => Some(code_parser_core::language::Language::JavaScript),
                "python" | "py" => Some(code_parser_core::language::Language::Python),
                _ => None,
            })
            .collect::<Vec<_>>()
    });

    let results = code_parser_core::parse_repo(&dir, languages)
        .with_context(|| format!("Failed to parse repo {}", dir.display()))?;

    if jsonl {
        for r in &results {
            let line = serde_json::to_string(&r.ir)?;
            println!("{line}");
        }
    } else {
        let total_symbols: usize = results.iter().map(|r| r.ir.symbols.len()).sum();
        let total_calls: usize = results.iter().map(|r| r.ir.calls.len()).sum();
        println!("files: {}", results.len());
        println!("total symbols: {total_symbols}");
        println!("total calls: {total_calls}");
    }

    for r in &results {
        for e in &r.errors {
            eprintln!("error: {e}");
        }
    }
    Ok(())
}

fn cmd_watch(dir: PathBuf) -> anyhow::Result<()> {
    #[cfg(feature = "watcher")]
    {
        use code_parser_core::watcher::FileWatcher;
        use code_parser_core::HashCache;

        let watcher = FileWatcher::new(&dir).context("Failed to start file watcher")?;
        let mut cache = HashCache::new();

        eprintln!("Watching {} for changes...", dir.display());

        loop {
            let paths = match watcher.next_changes() {
                Some(p) => p,
                None => break,
            };

            for path in paths {
                // Filter by extension.
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !matches!(
                    ext,
                    "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "py" | "pyi"
                ) {
                    continue;
                }

                match code_parser_core::parse_file_cached(&path, &mut cache) {
                    Ok(Some(result)) => {
                        let line = serde_json::to_string(&result.ir)?;
                        println!("{line}");
                    }
                    Ok(None) => {
                        // Hash unchanged — skipped.
                    }
                    Err(e) => {
                        eprintln!("error: {path}: {e}", path = path.display());
                    }
                }
            }
        }
    }

    #[cfg(not(feature = "watcher"))]
    {
        let _ = dir;
        anyhow::bail!(
            "File watching requires the 'watcher' feature. Rebuild with: cargo build --features watcher"
        );
    }

    #[cfg(feature = "watcher")]
    Ok(())
}

fn cmd_check(file: PathBuf) -> anyhow::Result<()> {
    match code_parser_core::parse_file(&file) {
        Ok(result) => {
            let mut ok = true;
            for d in &result.ir.diagnostics {
                let sev = d.severity.as_str();
                eprintln!(
                    "{}:{} {}: {}",
                    file.display(),
                    d.line.unwrap_or(0),
                    sev,
                    d.message
                );
                if d.severity == code_parser_ir::DiagnosticSeverity::Error {
                    ok = false;
                }
            }
            for e in &result.errors {
                eprintln!("error: {e}");
                ok = false;
            }
            if !ok {
                std::process::exit(1);
            }
            println!("OK: {}", file.display());
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}
