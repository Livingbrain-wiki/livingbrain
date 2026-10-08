//! `livingbrain import markdown <dir>` — bring a directory of notes in, one
//! page source per file.
//!
//! The order of the work in this module is the point of it: walk the
//! directory, read and redact every candidate locally, print what *would* be
//! uploaded, and only then — after the reader has answered — read a token and
//! open a socket. Nothing before the confirmation can reach the network, so
//! `livingbrain import markdown ~/notes` answered with `n` leaves the machine
//! exactly as it found it.
//!
//! Redaction is local and not optional. `livingbrain-redact` is the same
//! library the server runs on every ingest path, run here first so a secret in
//! a vault never crosses the socket at all. The relative path is redacted for
//! the same reason a body is: a filename carries an API key just as well.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};
use livingbrain_redact::{MAX_INPUT_BYTES, Policy, redact};
use serde_json::{Value, json};

use crate::api::Client;
use crate::{CliResult, Out, err, text};

/// The extensions a candidate file may have, lowercased. Everything else on
/// disk is somebody's build output, and a page source is prose.
const TEXT_EXTENSIONS: &[&str] = &["md", "markdown", "txt"];

/// The source formats `livingbrain import` speaks. One variant each, so
/// `import chatgpt` can follow without reshaping the command.
#[derive(Subcommand, Debug)]
pub enum Format {
    /// Import a directory of Markdown notes
    Markdown(Markdown),
}

/// The flags `livingbrain import markdown` takes.
#[derive(Args, Debug)]
pub struct Markdown {
    /// The directory to import
    #[arg(value_name = "DIR")]
    dir: PathBuf,
    /// Import into the shared brain instead of your personal one
    #[arg(long)]
    shared: bool,
    /// Upload without the confirmation prompt
    #[arg(long)]
    yes: bool,
    /// Skip files matching this glob (repeatable), e.g. `--exclude 'drafts/**'`
    #[arg(long, value_name = "GLOB")]
    exclude: Vec<String>,
    /// Skip files larger than this many bytes
    #[arg(
        long,
        value_name = "N",
        default_value_t = MAX_INPUT_BYTES as u64,
        // Capped at the redactor's own input limit: a file over it cannot be
        // scanned at all, so a larger flag would only ever buy a 413.
        value_parser = clap::value_parser!(u64).range(1..=MAX_INPUT_BYTES as u64),
    )]
    max_bytes: u64,
}

/// Everything the walk found, before any of it is uploaded.
#[derive(Default)]
struct Plan {
    sources: Vec<Source>,
    skipped: Skipped,
}

/// One file, read and redacted, ready to send.
struct Source {
    /// Relative to the root, `/`-separated: the path the server will store.
    path: String,
    body: String,
    /// Secrets found in the body and the path together.
    secrets: usize,
}

/// The files the walk passed over, and why. Every reason here is one the
/// reader can fix, which is why they are counted rather than swallowed.
#[derive(Default)]
struct Skipped {
    not_text: usize,
    too_large: usize,
    binary: usize,
}

/// Run one `import` subcommand.
pub fn run(format: &Format, api_url: &str, out: &Out) -> CliResult<()> {
    match format {
        Format::Markdown(args) => markdown(args, api_url, out),
    }
}

fn markdown(args: &Markdown, api_url: &str, out: &Out) -> CliResult<()> {
    let plan = plan(args)?;
    let (scope, brain) = scope(args);
    preview(args, &plan, brain);

    // A directory with nothing to say is a successful no-op, not a failure:
    // `import` is often run from a script over a vault that may be empty.
    if plan.sources.is_empty() {
        out.json_or(&report(args, &plan, scope, 0, 0, &[]), || {
            println!("Nothing to import.");
        });
        return Ok(());
    }
    if !args.yes && !confirm(plan.sources.len(), brain)? {
        return Err(err("import cancelled — nothing was uploaded"));
    }

    // The first socket in the command, and the first read of the token. Both
    // sit after the confirmation on purpose: a declined import asks the
    // keychain for nothing.
    let client = Client::new(api_url).with_token(crate::auth::token_for(api_url)?);

    let mut created = 0usize;
    let mut uploaded = Vec::new();
    for source in &plan.sources {
        // Stop at the first failure rather than push on: half an import that
        // the reader was not told about is worse than a clear stop.
        let value = client
            .post_source(&source.path, &source.body, scope)
            .map_err(|e| err(format!("{}: {e}", source.path)))?;
        let is_new = value["created"].as_bool().unwrap_or(false);
        created += usize::from(is_new);
        uploaded.push(json!({
            "path": source.path,
            "id": text(&value, "id"),
            "created": is_new,
        }));
    }

    let unchanged = uploaded.len() - created;
    let redacted: usize = plan.sources.iter().map(|source| source.secrets).sum();
    out.json_or(
        &report(args, &plan, scope, created, unchanged, &uploaded),
        || println!("Imported {created} new, {unchanged} already present ({redacted} redacted)."),
    );
    Ok(())
}

/// Walk `args.dir` and build the plan. Local reads only: no token, no socket.
fn plan(args: &Markdown) -> CliResult<Plan> {
    let root = &args.dir;
    if !root.is_dir() {
        return Err(err(format!("{} is not a directory", root.display())));
    }
    let overrides = overrides(root, &args.exclude)?;
    let walker = WalkBuilder::new(root)
        // A vault's `.gitignore` is a statement of what is not notes, and it
        // is honoured whether or not the vault happens to be a git checkout.
        // Hidden entries go with it, which is also what takes a vault's own
        // bookkeeping (`.obsidian/`, `.trash/`) out of the walk: every name
        // the walk skips this way begins with a dot.
        .hidden(true)
        .require_git(false)
        .overrides(overrides)
        .build();

    let mut plan = Plan::default();
    for entry in walker {
        // `ignore::Error::WithPath` already renders the path it failed on.
        let entry = entry.map_err(|e| err(format!("could not read the directory: {e}")))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        // The order of these three is the order a reader expects to read them
        // in: what it is, how big it is, whether it is even text.
        if !is_text_extension(path) {
            plan.skipped.not_text += 1;
            continue;
        }
        match entry.metadata() {
            Ok(metadata) if metadata.len() > args.max_bytes => {
                plan.skipped.too_large += 1;
                continue;
            }
            Ok(_) => {}
            Err(e) => return Err(err(format!("could not read {}: {e}", path.display()))),
        }
        let bytes = std::fs::read(path)
            .map_err(|e| err(format!("could not read {}: {e}", path.display())))?;
        let Ok(text) = String::from_utf8(bytes) else {
            plan.skipped.binary += 1;
            continue;
        };
        if text.contains('\0') {
            plan.skipped.binary += 1;
            continue;
        }
        let relative = relative_path(root, path)?;
        let (body, findings) = redact(&text, Policy::Redact)
            .map_err(|e| err(format!("could not redact {}: {e}", path.display())))?;
        // The path is redacted for the same reason the body is, so an error
        // here cannot name it either; the ordinal is enough to find the file.
        let ordinal = plan.sources.len() + 1;
        let (safe_path, path_findings) = redact(&relative, Policy::Redact)
            .map_err(|e| err(format!("could not redact the path of file {ordinal}: {e}")))?;
        plan.sources.push(Source {
            path: safe_path,
            body,
            secrets: findings.len() + path_findings.len(),
        });
    }
    // The walker's order is filesystem order; a sorted plan reads the same way
    // twice and gives the run a stable order to report in.
    plan.sources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(plan)
}

/// The `--exclude` globs as an override set, plus nothing else: the vault's
/// own ignores are the walker's business.
fn overrides(root: &Path, globs: &[String]) -> CliResult<Override> {
    let mut builder = OverrideBuilder::new(root);
    for glob in globs {
        // Override syntax: a leading `!` means "ignore", which is what a glob
        // the reader wrote to skip something means here.
        builder
            .add(&format!("!{glob}"))
            .map_err(|e| err(format!("bad --exclude {glob}: {e}")))?;
    }
    builder
        .build()
        .map_err(|e| err(format!("bad --exclude: {e}")))
}

/// Whether a path is a candidate by its extension alone.
fn is_text_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| TEXT_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// The path as the server will store it: relative to the root, `/` separated.
/// A path the root does not prefix is an error rather than a fallback — the
/// fallback would put the reader's own absolute path on the wire.
fn relative_path(root: &Path, path: &Path) -> CliResult<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| err("a file in the walk was not inside the directory being imported"))?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

/// The wire scope, and the words the prompt uses for it.
fn scope(args: &Markdown) -> (&'static str, &'static str) {
    if args.shared {
        ("shared", "the shared brain")
    } else {
        ("personal", "your personal brain")
    }
}

/// What this run would do, on stderr so `--json` keeps stdout to one object.
fn preview(args: &Markdown, plan: &Plan, brain: &str) {
    let total: usize = plan.sources.iter().map(|s| s.body.len()).sum();
    let mut breakdown: BTreeMap<&str, usize> = BTreeMap::new();
    for source in &plan.sources {
        let extension = source
            .path
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            .unwrap_or("");
        *breakdown.entry(extension).or_default() += 1;
    }
    let by_type = breakdown
        .iter()
        .map(|(extension, count)| format!(".{extension} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let with_secrets = plan.sources.iter().filter(|s| s.secrets > 0).count();
    let secrets: usize = plan.sources.iter().map(|s| s.secrets).sum();

    eprintln!("Import preview for {}:", args.dir.display());
    eprintln!(
        "  {} files, {}{}",
        plan.sources.len(),
        size_label(total),
        if by_type.is_empty() {
            String::new()
        } else {
            format!(" ({by_type})")
        }
    );
    let mut reasons = Vec::new();
    if plan.skipped.not_text > 0 {
        reasons.push(format!("{} not markdown/text", plan.skipped.not_text));
    }
    if plan.skipped.too_large > 0 {
        reasons.push(format!("{} too large", plan.skipped.too_large));
    }
    if plan.skipped.binary > 0 {
        reasons.push(format!("{} binary", plan.skipped.binary));
    }
    if !reasons.is_empty() {
        eprintln!("  skipped: {}", reasons.join(", "));
    }
    eprintln!("  {secrets} secret(s) in {with_secrets} file(s) will be redacted before upload");
    eprintln!("  target: {brain}");
}

/// The one question, answered with one line. Anything but `y`/`yes` — and an
/// EOF, which is what a closed stdin looks like — is a no.
fn confirm(count: usize, brain: &str) -> CliResult<bool> {
    eprint!("Upload {count} files to {brain}? [y/N] ");
    // stderr is line-buffered and this line has no newline on it.
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    let read = std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| err(format!("could not read stdin: {e}")))?;
    Ok(read > 0 && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// The one object `--json` prints.
fn report(
    args: &Markdown,
    plan: &Plan,
    scope: &str,
    created: usize,
    unchanged: usize,
    uploaded: &[Value],
) -> Value {
    json!({
        "root": args.dir.display().to_string(),
        "scope": scope,
        "files": plan.sources.len(),
        "created": created,
        "unchanged": unchanged,
        "redacted": plan.sources.iter().map(|s| s.secrets).sum::<usize>(),
        "skipped": {
            "not_markdown": plan.skipped.not_text,
            "too_large": plan.skipped.too_large,
            "binary": plan.skipped.binary,
        },
        "sources": uploaded,
    })
}

/// A byte count as a person reads it.
fn size_label(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_INPUT_BYTES, is_text_extension, relative_path, size_label};
    use std::path::Path;

    #[test]
    fn a_candidate_is_markdown_or_plain_text_and_nothing_else() {
        for name in ["note.md", "note.MD", "readme.Markdown", "scratch.txt"] {
            assert!(is_text_extension(Path::new(name)), "{name} is a candidate");
        }
        for name in [
            "main.rs",
            "photo.png",
            "notes.md.zip",
            "Makefile",
            "archive.mdx",
        ] {
            assert!(
                !is_text_extension(Path::new(name)),
                "{name} is not a candidate"
            );
        }
    }

    #[test]
    fn the_path_on_the_wire_is_relative_and_slash_separated() {
        let root = Path::new("/home/reader/vault");
        assert_eq!(
            relative_path(root, &root.join("nested/deep/note.md")).expect("inside the root"),
            "nested/deep/note.md"
        );
        assert_eq!(
            relative_path(root, &root.join("note.md")).expect("inside the root"),
            "note.md"
        );
        // Outside the root is an error, never an absolute path on the wire.
        assert!(relative_path(root, Path::new("/etc/hostname")).is_err());
        assert!(relative_path(root, Path::new("note.md")).is_err());
    }

    #[test]
    fn sizes_are_read_in_units_a_person_uses() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(999), "999 B");
        assert_eq!(size_label(1024), "1.0 KiB");
        assert_eq!(size_label(1024 * 1024 * 3 / 2), "1.5 MiB");
        // The redactor's cap reads as one number a reader can compare a file to.
        assert_eq!(size_label(MAX_INPUT_BYTES), "1.0 MiB");
    }
}
