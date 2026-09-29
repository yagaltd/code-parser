//! code-map CLI — refresh, callers, callees, path.
//!
//! Usage:
//!   code-map refresh <DIR> [-o OUT] [--languages rust,ts]
//!   code-map callers <SYMBOL> [-m MAP]
//!   code-map callees <SYMBOL> [-m MAP]
//!   code-map path <FROM> <TO> [--max-hops N] [-m MAP]

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use code_map::graph::Graph;
use code_map::refresh;

#[derive(Parser)]
#[command(
    name = "code-map",
    version,
    about = "Query layer over a code-parser JSONL map — zero infra, stateless per run"
)]
struct Cli {
    /// Path to the map (JSONL of FileParseIR).
    #[arg(short, long, global = true, default_value = "code-map.jsonl")]
    map: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Regenerate the map snapshot from a repo (atomic tmp-then-rename).
    Refresh {
        /// Repository root to parse.
        dir: PathBuf,
        /// Output map path (default: the global --map value).
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
        /// Comma-separated languages (rust,typescript,python,javascript).
        #[arg(long)]
        languages: Option<String>,
    },
    /// Who calls this symbol (reverse call edges).
    Callers { symbol: String },
    /// What this symbol calls (forward call edges).
    Callees { symbol: String },
    /// Shortest call path between two symbols (BFS, hop-capped).
    Path {
        from: String,
        to: String,
        #[arg(long, default_value_t = 8)]
        max_hops: usize,
    },
    /// Rank map content for a query (IDF + fuzzy; `re:` for regex mode).
    Search {
        query: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Emit machine-readable JSON (results + tokens_est).
        #[arg(long)]
        json: bool,
        /// Print the matching card text under each result.
        #[arg(long)]
        cards: bool,
    },
    /// Filter a `search --json` shortlist with TypeSafe judgments (stdin → stdout).
    Gate {
        query: String,
        /// Emit machine-readable JSON with verdicts.
        #[arg(long)]
        json: bool,
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
    /// One-time credential setup: read the TypeSafe API key from stdin.
    Typesafe {
        #[command(subcommand)]
        cmd: TypesafeCmd,
    },
    /// Record which candidates you actually used for a query (feeds `learn`).
    Mark {
        /// The query the results came from (joined by normalized text).
        query: String,
        /// Paths that proved useful.
        paths: Vec<String>,
    },
    /// Fold gate trails × usage marks into the learned lexicon
    /// (~/.config/code-parser/learned/gate-lexicon.json, consumed by search).
    Learn {
        /// Fraction of sample groups held out to verify rules.
        #[arg(long, default_value_t = code_map::lexicon::HOLDOUT_FRAC)]
        holdout: f64,
        /// Minimum labeled candidates per term before it's measured.
        #[arg(long, default_value_t = code_map::lexicon::MIN_SAMPLES)]
        min_samples: usize,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum TypesafeCmd {
    /// Read the API key from stdin: `echo $TYPESAFE_API_KEY | code-map typesafe setup`
    Setup {
        #[arg(long)]
        key_file: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Refresh {
            dir,
            out,
            languages,
        } => {
            let out = out.unwrap_or_else(|| cli.map.clone());
            let langs = languages.as_deref().and_then(refresh::parse_languages);
            let n = refresh::run(&dir, &out, langs)?;
            println!("wrote {} ({} files)", out.display(), n);
        }
        Command::Callers { symbol } => {
            let g = Graph::load(&cli.map)?;
            let matches = g.resolve(&symbol);
            match matches.len() {
                0 => println!("no matches for '{symbol}'"),
                1 => print_edges(&g, "callers", matches[0], |n| g.callers(n)),
                _ => {
                    println!(
                        "note: '{symbol}' is ambiguous ({} matches) — showing all:",
                        matches.len()
                    );
                    for &m in &matches {
                        println!(
                            "# {} {} L{}-{}",
                            g.nodes[m].path,
                            g.nodes[m].qualified_name,
                            g.nodes[m].start_line,
                            g.nodes[m].end_line
                        );
                        print_edges(&g, "callers", m, |n| g.callers(n));
                    }
                }
            }
        }
        Command::Callees { symbol } => {
            let g = Graph::load(&cli.map)?;
            let matches = g.resolve(&symbol);
            match matches.len() {
                0 => println!("no matches for '{symbol}'"),
                1 => print_edges(&g, "callees", matches[0], |n| g.callees(n)),
                _ => {
                    println!(
                        "note: '{symbol}' is ambiguous ({} matches) — showing all:",
                        matches.len()
                    );
                    for &m in &matches {
                        println!(
                            "# {} {} L{}-{}",
                            g.nodes[m].path,
                            g.nodes[m].qualified_name,
                            g.nodes[m].start_line,
                            g.nodes[m].end_line
                        );
                        print_edges(&g, "callees", m, |n| g.callees(n));
                    }
                }
            }
        }
        Command::Path { from, to, max_hops } => {
            let g = Graph::load(&cli.map)?;
            let a = unique(&g, &from)?;
            let b = unique(&g, &to)?;
            let Some(path) = g.shortest_path(a, b, max_hops) else {
                println!("no path within {max_hops} hops");
                return Ok(());
            };
            if path.len() == 1 {
                println!(
                    "path found (0 hops): {} {}",
                    g.nodes[a].path, g.nodes[a].qualified_name
                );
                return Ok(());
            }
            println!("path found ({} hops):", path.len() - 1);
            for w in path.windows(2) {
                let (c, d) = (w[0], w[1]);
                println!(
                    "  {}:{}  {} → {}",
                    g.nodes[c].path,
                    g.call_line(c, d),
                    g.nodes[c].qualified_name,
                    g.nodes[d].qualified_name
                );
            }
        }
        Command::Search {
            query,
            limit,
            json,
            cards,
        } => {
            let irs = code_map::load_irs(&cli.map)?;
            let lex = code_map::lexicon::Lexicon::load_default();
            let hits = code_map::search::run_with_lexicon(&irs, &query, limit, lex.as_ref())?;
            if json {
                let tokens_est: u32 = hits.iter().map(|h| h.tokens_est).sum();
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "results": hits,
                        "tokens_est": tokens_est
                    }))?
                );
            } else {
                for h in &hits {
                    println!("{:.3} {}:{} {} {}", h.score, h.path, h.line, h.kind, h.name);
                    if cards {
                        println!("  {}", h.card.replace('\n', "\n  "));
                    }
                }
            }
        }
        Command::Gate {
            query,
            json,
            key_file,
        } => {
            #[cfg(feature = "typesafe")]
            {
                use code_map::typesafe as ts;
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                let v: serde_json::Value = serde_json::from_str(&text)
                    .context("stdin is not JSON — pipe `code-map search --json` output")?;
                let candidates: Vec<ts::Candidate> =
                    serde_json::from_value(v["results"].clone())
                        .context("stdin JSON has no \"results\" array")?;
                let input = ts::GateInput { query, candidates };
                let key = ts::load_key(key_file.as_deref())?;
                let gated = ts::gate(input, &key, ts::post, true)?;
                if json {
                    let results: Vec<serde_json::Value> = gated
                        .iter()
                        .map(|(c, v, verdict)| {
                            serde_json::json!({
                                "path": c.path, "line": c.line, "name": c.name,
                                "gate_value": v, "verdict": verdict,
                                "inline": *verdict == "inline",
                            })
                        })
                        .collect();
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({ "results": results }))?
                    );
                } else {
                    for (c, v, verdict) in &gated {
                        println!("{:.3} {:8} {}:{} {}", v, verdict, c.path, c.line, c.name);
                    }
                }
                return Ok(());
            }
            #[cfg(not(feature = "typesafe"))]
            {
                let _ = (query, json, key_file);
                anyhow::bail!(
                    "gate requires the 'typesafe' feature: cargo build --features typesafe"
                )
            }
        }
        Command::Typesafe { cmd } => {
            #[cfg(feature = "typesafe")]
            match cmd {
                TypesafeCmd::Setup { key_file } => {
                    code_map::typesafe::run_setup(key_file.as_deref())?;
                }
            }
            #[cfg(not(feature = "typesafe"))]
            {
                let _ = cmd;
                anyhow::bail!(
                    "typesafe requires the 'typesafe' feature: cargo build --features typesafe"
                )
            }
        }
        Command::Mark { query, paths } => {
            if paths.is_empty() {
                anyhow::bail!("mark needs at least one path");
            }
            code_map::lexicon::mark_used(&query, &paths)?;
            println!("marked {} used path(s) for '{query}'", paths.len());
        }
        Command::Learn {
            holdout,
            min_samples,
            out,
        } => {
            let lex = code_map::lexicon::learn(
                &code_map::lexicon::trails_path(),
                &code_map::lexicon::usage_path(),
                holdout,
                min_samples,
            )?;
            let path = out.unwrap_or_else(code_map::lexicon::default_lexicon_path);
            lex.save(&path)?;
            println!(
                "lexicon written to {} (groups={}, promoted={}, demoted={})",
                path.display(),
                lex.stats.get("groups").copied().unwrap_or(0),
                lex.stats.get("promoted").copied().unwrap_or(0),
                lex.stats.get("demoted").copied().unwrap_or(0),
            );
        }
    }
    Ok(())
}

fn unique(g: &Graph, name: &str) -> Result<usize> {
    let m = g.resolve(name);
    match m.len() {
        0 => anyhow::bail!("no matches for '{name}'"),
        1 => Ok(m[0]),
        _ => anyhow::bail!(
            "'{name}' is ambiguous ({} matches) — use a qualified name",
            m.len()
        ),
    }
}

fn print_edges(g: &Graph, dir_label: &str, node: usize, f: impl Fn(usize) -> Vec<(usize, u32)>) {
    let mut edges = f(node);
    edges.sort();
    if edges.is_empty() {
        println!("  (no {dir_label})");
        return;
    }
    for (other, line) in edges {
        let inferred = match dir_label {
            "callers" => g.is_inferred(other, node),
            _ => g.is_inferred(node, other),
        };
        let mark = if inferred { "  (inferred)" } else { "" };
        match dir_label {
            "callers" => println!(
                "  {}:{}  {}{}",
                g.nodes[other].path, line, g.nodes[other].qualified_name, mark
            ),
            _ => println!(
                "  {}:{}  {} → {}{}",
                g.nodes[node].path,
                line,
                g.nodes[node].qualified_name,
                g.nodes[other].qualified_name,
                mark
            ),
        }
    }
}
