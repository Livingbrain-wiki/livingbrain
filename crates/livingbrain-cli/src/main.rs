//! `livingbrain` — the Living Brain CLI.
//!
//! A native client of the Living Brain API (the wire contract is in [`api`]),
//! not a module: it holds no memory, runs no agent loop, and is never a
//! dependency of the Worker. Tokens live in the OS keychain, never a dotfile
//! ([`auth`]).

#![forbid(unsafe_code)]

use std::fmt;
use std::io::{IsTerminal, Read};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use api::Client;

mod api;
mod auth;
mod import;
mod mcp;
mod telemetry;

const DEFAULT_API_URL: &str = "https://api.livingbrain.wiki";

/// Every failure, phrased for a human. `NotLoggedIn` exits 2 with a login hint.
#[derive(Debug)]
enum CliError {
    NotLoggedIn(String),
    Api(String),
    Other(String),
}

/// A command's result.
type CliResult<T> = Result<T, CliError>;

/// An [`CliError::Other`].
fn err(message: impl Into<String>) -> CliError {
    CliError::Other(message.into())
}

impl CliError {
    fn exit_code(&self) -> i32 {
        if matches!(self, Self::NotLoggedIn(_)) {
            2
        } else {
            1
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoggedIn(m) | Self::Api(m) | Self::Other(m) => f.write_str(m),
        }
    }
}

/// The output sink: one `--json` flag decides how every command prints.
struct Out {
    json: bool,
}

impl Out {
    /// Print the server object as JSON, or run `human` to print it for a person.
    fn json_or(&self, value: &Value, human: impl FnOnce()) {
        if self.json {
            println!("{value}");
        } else {
            human();
        }
    }

    /// An error on stderr: `{"error":…}` in JSON mode, `error: …` otherwise.
    fn error(&self, message: &str) {
        if self.json {
            eprintln!("{}", json!({ "error": message }));
        } else {
            eprintln!("error: {message}");
        }
    }
}

#[derive(Parser)]
#[command(
    name = "livingbrain",
    version,
    about = "Ask, search and write your team's brain from the terminal"
)]
struct Cli {
    /// Print a machine-readable JSON result (accepted by every command)
    #[arg(long, global = true)]
    json: bool,
    /// Living Brain API base URL
    #[arg(long, global = true, env = "LIVINGBRAIN_API_URL", default_value = DEFAULT_API_URL)]
    api_url: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log in with the OAuth device flow (RFC 8628)
    Login,
    /// Forget the stored token
    Logout,
    /// Ask the brain a question and get a cited answer
    Ask {
        /// Scope the question to one project
        #[arg(long)]
        project: Option<String>,
        /// The question
        #[arg(required = true)]
        question: Vec<String>,
    },
    /// Search the brain
    Search {
        /// Scope the search to one project
        #[arg(long)]
        project: Option<String>,
        /// Maximum number of results
        #[arg(long)]
        limit: Option<u32>,
        /// The search query
        #[arg(required = true)]
        query: Vec<String>,
    },
    /// Add a note; reads the body from stdin when it is piped
    Note {
        /// Scope the note to one project
        #[arg(long)]
        project: Option<String>,
        /// The note text (or `-` to read stdin)
        text: Vec<String>,
    },
    /// Print a wiki page's Markdown
    Page {
        /// The page slug
        slug: String,
    },
    /// Export the whole brain as a zip
    Export {
        /// The export format
        #[arg(long, default_value = "obsidian")]
        format: String,
        /// Where to write the zip
        #[arg(long)]
        out: Option<String>,
        /// Overwrite the output file if it already exists
        #[arg(long)]
        force: bool,
    },
    /// Run a stdio MCP server (its output is always JSON; `--json` is a no-op)
    Mcp,
    /// Import notes from elsewhere into the brain
    Import {
        #[command(subcommand)]
        format: import::Format,
    },
    /// Turn anonymous usage data on or off, and show what would be sent
    Telemetry {
        #[command(subcommand)]
        action: telemetry::Action,
    },
    /// What Living Brain is built with, from the vendored Factory Zero stack registry
    About,
}

fn main() {
    auth::init_backend();
    let cli = Cli::parse();
    let out = Out { json: cli.json };
    if let Err(error) = run(&cli, &out) {
        out.error(&error.to_string());
        std::process::exit(error.exit_code());
    }
}

fn run(cli: &Cli, out: &Out) -> CliResult<()> {
    match &cli.command {
        Command::Login => auth::login(&cli.api_url, out),
        Command::Logout => auth::logout(&cli.api_url, out),
        Command::Mcp => mcp::serve(&client(cli)?),
        // The stack is vendored into the binary (issue #61), so this is the
        // one command that needs neither a token nor the API — a reader who
        // asks what the brain is made of should get an answer before signing in.
        Command::About => {
            let value = serde_json::to_value(livingbrain_stack::stack())
                .map_err(|e| err(format!("could not render the stack: {e}")))?;
            out.json_or(&value, || print!("{}", livingbrain_stack::stack().text()));
            Ok(())
        }
        Command::Ask { project, question } => {
            let value = client(cli)?.ask(&question.join(" "), project.as_deref())?;
            out.json_or(&value, || render_ask(&value));
            Ok(())
        }
        Command::Search {
            project,
            limit,
            query,
        } => {
            let value = client(cli)?.search(&query.join(" "), project.as_deref(), *limit)?;
            out.json_or(&value, || render_search(&value));
            Ok(())
        }
        Command::Note { project, text } => {
            let body = read_note_body(text)?;
            let value = client(cli)?.note(&body, project.as_deref())?;
            out.json_or(&value, || render_note(&value));
            Ok(())
        }
        Command::Page { slug } => {
            let value = client(cli)?.page(slug)?;
            out.json_or(&value, || render_page(&value));
            Ok(())
        }
        Command::Export {
            format,
            out: dest,
            force,
        } => {
            let bytes = client(cli)?.export(format)?;
            let path = dest
                .clone()
                .unwrap_or_else(|| "livingbrain-export.zip".to_owned());
            write_export(&path, &bytes, *force)?;
            let value = json!({ "path": path, "bytes": bytes.len() });
            out.json_or(&value, || println!("Wrote {path} ({} bytes)", bytes.len()));
            Ok(())
        }
        // The walk, the preview and the confirmation all happen before the
        // client is built, so a declined import touches neither the keychain
        // nor the network (see `import`).
        Command::Import { format } => import::run(format, &cli.api_url, out),
        // The only command that neither needs a token nor opens a socket: it
        // reads and writes the stored preference and prints the local view.
        Command::Telemetry { action } => telemetry::run(
            *action,
            out,
            &telemetry::KeychainPreference,
            &telemetry::Env::from_process(),
        ),
    }
}

/// The authenticated client every non-auth command uses.
fn client(cli: &Cli) -> CliResult<Client> {
    Ok(Client::new(&cli.api_url).with_token(auth::token_for(&cli.api_url)?))
}

/// The note body: arguments, or stdin when piped (or when the text is `-`).
fn read_note_body(args: &[String]) -> CliResult<String> {
    let from_stdin =
        (args.len() == 1 && args[0] == "-") || (args.is_empty() && !std::io::stdin().is_terminal());
    let text = if from_stdin {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|e| err(format!("could not read stdin: {e}")))?;
        buffer
    } else if args.is_empty() {
        return Err(err(
            "a note needs text: pass it as arguments or pipe it on stdin",
        ));
    } else {
        args.join(" ")
    };
    let text = text.trim().to_owned();
    if text.is_empty() {
        Err(err("the note is empty"))
    } else {
        Ok(text)
    }
}

/// Write the export to `<path>.part`, then rename it into place. A failed write
/// leaves the target untouched; an existing target is refused unless `force`.
fn write_export(path: &str, bytes: &[u8], force: bool) -> CliResult<()> {
    if !force && std::path::Path::new(path).exists() {
        return Err(err(format!(
            "{path} already exists — pass --force to overwrite"
        )));
    }
    let mut part = std::ffi::OsString::from(path);
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    std::fs::write(&part, bytes)
        .map_err(|e| err(format!("could not write {}: {e}", part.display())))?;
    std::fs::rename(&part, path).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        err(format!("could not write {path}: {e}"))
    })
}

/// The string at `key`, or `""`.
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn render_ask(value: &Value) {
    println!("{}", text(value, "answer"));
    match value.get("citations").and_then(Value::as_array) {
        Some(citations) if !citations.is_empty() => {
            println!();
            for (index, citation) in citations.iter().enumerate() {
                println!(
                    "[{}] {} <{}>",
                    index + 1,
                    text(citation, "title"),
                    text(citation, "url")
                );
            }
        }
        _ => eprintln!("warning: the answer has no citations; treat it with caution"),
    }
}

fn render_search(value: &Value) {
    let empty = Vec::new();
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if results.is_empty() {
        println!("No results.");
    }
    for (index, result) in results.iter().enumerate() {
        println!("[{}] {}", index + 1, text(result, "title"));
        println!("    <{}>", text(result, "url"));
        if !text(result, "snippet").is_empty() {
            println!("    {}", text(result, "snippet"));
        }
    }
}

fn render_note(value: &Value) {
    let url = text(value, "url");
    println!(
        "Noted: {}",
        if url.is_empty() {
            text(value, "id")
        } else {
            url
        }
    );
}

fn render_page(value: &Value) {
    let markdown = text(value, "markdown");
    print!("{markdown}");
    if !markdown.ends_with('\n') {
        println!();
    }
}
