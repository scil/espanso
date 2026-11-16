//! `snipctl`: command line access to snip-core. Every command prints JSON.
//! `snipctl serve` speaks JSON-RPC on standard input and output.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::json;
use snip_core::{rpc, SnipError, Workspace};

#[derive(Parser)]
#[command(
    name = "snipctl",
    version,
    about = "Manage espanso snippets without losing comments"
)]
struct Cli {
    /// espanso base folder (the one that holds `match/` and `config/`).
    /// Default: $SNIP_BASE, then espanso's standard folder.
    #[arg(long, global = true)]
    base: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the base folder and version.
    Info,
    /// List match and config files.
    Files,
    /// List snippets, optionally filtered.
    List {
        /// Case-insensitive text to search in triggers, labels, text and file names.
        #[arg(short, long)]
        query: Option<String>,
        /// Only this file, for example `match/base.yml`.
        #[arg(short, long)]
        file: Option<String>,
    },
    /// Show one snippet with its YAML.
    Get { file: String, index: usize },
    /// Append a snippet; the YAML is read from standard input.
    Create { file: String },
    /// Replace a snippet; the YAML is read from standard input.
    Update {
        file: String,
        index: usize,
        hash: String,
    },
    /// Delete a snippet.
    Delete {
        file: String,
        index: usize,
        hash: String,
    },
    /// Run espanso's loader on the whole folder and print its problems.
    Validate,
    /// Serve JSON-RPC 2.0 over standard input and output, one message per line.
    Serve,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let out = json!({ "error": { "code": e.code(), "message": e.to_string(), "data": e.data() } });
            eprintln!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), SnipError> {
    let ws = Workspace::open(cli.base)?;
    let value = match cli.command {
        Command::Info => json!(ws.info()),
        Command::Files => json!(ws.list_files()?),
        Command::List { query, file } => {
            json!(ws.list_snippets(query.as_deref(), file.as_deref())?)
        }
        Command::Get { file, index } => json!(ws.get_snippet(&file, index)?),
        Command::Create { file } => json!(ws.create_snippet(&file, &stdin()?)?),
        Command::Update { file, index, hash } => {
            json!(ws.update_snippet(&file, index, &hash, &stdin()?)?)
        }
        Command::Delete { file, index, hash } => {
            ws.delete_snippet(&file, index, &hash)?;
            json!({ "deleted": true })
        }
        Command::Validate => json!({ "problems": ws.validate()? }),
        Command::Serve => {
            let stdin = std::io::stdin();
            rpc::serve(&ws, stdin.lock(), std::io::stdout().lock())?;
            return Ok(());
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    );
    Ok(())
}

fn stdin() -> Result<String, SnipError> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    Ok(text)
}
