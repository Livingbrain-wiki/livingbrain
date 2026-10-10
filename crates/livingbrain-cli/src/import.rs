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

/// The source formats `livingbrain import` speaks. One variant each, so the
/// next format lands as a sibling without reshaping the command.
#[derive(Subcommand, Debug)]
pub enum Format {
    /// Import a directory of Markdown notes
    Markdown(Markdown),
    /// Import a ChatGPT data export (Settings → Data controls → Export data)
    Chatgpt(ChatExport),
    /// Import a Claude data export (Settings → Privacy → Export data)
    Claude(ChatExport),
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

/// The flags `import chatgpt` and `import claude` take — the same set, since
/// the two exports ask the same questions.
#[derive(Args, Debug)]
pub struct ChatExport {
    /// The export zip, or a bare `conversations.json`
    #[arg(value_name = "EXPORT")]
    export: PathBuf,
    /// Import into the shared brain instead of your personal one
    #[arg(long)]
    shared: bool,
    /// Upload without the confirmation prompt
    #[arg(long)]
    yes: bool,
    /// Keep conversations created on or after this day (YYYY-MM-DD)
    #[arg(long, value_name = "DATE")]
    since: Option<String>,
    /// Keep conversations created on or before this day (YYYY-MM-DD)
    #[arg(long, value_name = "DATE")]
    until: Option<String>,
    /// Keep conversations whose title or messages contain this text, case-insensitively (repeatable: any match keeps a conversation)
    #[arg(long, value_name = "TEXT")]
    keyword: Vec<String>,
    /// Keep the conversation with this id (repeatable)
    #[arg(long, value_name = "ID")]
    id: Vec<String>,
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
        Format::Chatgpt(args) => chat(args, crate::chat_export::Kind::Chatgpt, api_url, out),
        Format::Claude(args) => chat(args, crate::chat_export::Kind::Claude, api_url, out),
    }
}

fn markdown(args: &Markdown, api_url: &str, out: &Out) -> CliResult<()> {
    let plan = plan(args)?;
    let (scope, brain) = scope(args.shared);
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
            .post_source("import", &source.path, &source.body, scope)
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
fn scope(shared: bool) -> (&'static str, &'static str) {
    if shared {
        ("shared", "the shared brain")
    } else {
        ("personal", "your personal brain")
    }
}

// ---------------------------------------------------------------------------
// Chat exports (`import chatgpt`, `import claude`)

/// What a chat import would upload: one rendered, redacted part per entry.
/// The parts known before the confirmation are the struct; the counts only an
/// upload knows are [`ChatPlan::report`]'s arguments.
struct ChatPlan<'a> {
    kind: crate::chat_export::Kind,
    export: &'a Path,
    scope: &'a str,
    selected: usize,
    sources: Vec<Source>,
    /// Secrets redacted out of `sources`, counted per class.
    redaction: BTreeMap<&'a str, usize>,
}

impl ChatPlan<'_> {
    /// The one object `--json` prints.
    fn report(&self, created: usize, unchanged: usize, uploaded: &[Value]) -> Value {
        json!({
            "source": self.kind.as_str(),
            "export": self.export.display().to_string(),
            "scope": self.scope,
            "conversations": self.selected,
            "files": self.sources.len(),
            "created": created,
            "unchanged": unchanged,
            "redacted": self.redaction.values().sum::<usize>(),
            "redaction": self.redaction,
            "sources": uploaded,
        })
    }
}

/// Run `import chatgpt` / `import claude`. The shape is [`markdown`]'s on
/// purpose: the unzip, the parse, the selection, the render and the redaction
/// are all local; the preview describes the redacted parts; and the token and
/// the socket come only after the answer.
fn chat(
    args: &ChatExport,
    kind: crate::chat_export::Kind,
    api_url: &str,
    out: &Out,
) -> CliResult<()> {
    let selection = crate::chat_export::Selection {
        since: args
            .since
            .as_deref()
            .map(|date| checked_date("--since", date))
            .transpose()?,
        until: args
            .until
            .as_deref()
            .map(|date| checked_date("--until", date))
            .transpose()?,
        keywords: args.keyword.clone(),
        ids: args.id.clone(),
    };
    let json = crate::chat_export::read_conversations_json(&args.export).map_err(err)?;
    let conversations = crate::chat_export::parse(kind, &json).map_err(err)?;
    let mut selected: Vec<&crate::chat_export::Conversation> = conversations
        .iter()
        .filter(|c| selection.keeps(c))
        .collect();
    // Date order, ties by id: the preview reads chronologically, and a second
    // run of the same export uploads in the same order.
    selected.sort_by(|a, b| (&a.created, &a.id).cmp(&(&b.created, &b.id)));

    // Redaction happens here, before the preview: the summary below is what
    // the upload would do, and nothing unredacted survives to the wire.
    let (scope, brain) = scope(args.shared);
    let mut plan = ChatPlan {
        kind,
        export: &args.export,
        scope,
        selected: selected.len(),
        sources: Vec::new(),
        redaction: BTreeMap::new(),
    };
    for conversation in &selected {
        for part in crate::chat_export::render(kind, conversation) {
            let (body, findings) = redact(&part.body, Policy::Redact).map_err(|e| {
                err(format!(
                    "could not redact the body of {}: {e}",
                    conversation.id
                ))
            })?;
            for finding in &findings {
                *plan.redaction.entry(finding.class.as_str()).or_default() += 1;
            }
            plan.sources.push(Source {
                path: crate::chat_export::part_path(kind, conversation, &part),
                body,
                secrets: findings.len(),
            });
        }
    }

    chat_preview(kind, &args.export, &selected, &plan, brain);
    if plan.sources.is_empty() {
        out.json_or(&plan.report(0, 0, &[]), || println!("Nothing to import."));
        return Ok(());
    }
    if !args.yes && !confirm(plan.sources.len(), brain)? {
        return Err(err("import cancelled — nothing was uploaded"));
    }

    // The first socket in the command, and the first read of the token — both
    // after the confirmation, for the same reason [`markdown`]'s are.
    let client = Client::new(api_url).with_token(crate::auth::token_for(api_url)?);
    let mut created = 0usize;
    let mut uploaded = Vec::new();
    for source in &plan.sources {
        // Stop at the first failure rather than push on, as in [`markdown`].
        let value = client
            .post_source("import", &source.path, &source.body, scope)
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
    let redacted: usize = plan.redaction.values().sum();
    out.json_or(&plan.report(created, unchanged, &uploaded), || {
        println!("Imported {created} new, {unchanged} already present ({redacted} redacted).")
    });
    Ok(())
}

/// What this run would upload, on stderr so `--json` keeps stdout to one
/// object: the selected conversations, oldest first, then the per-class
/// redaction summary, then the target.
fn chat_preview(
    kind: crate::chat_export::Kind,
    export: &Path,
    selected: &[&crate::chat_export::Conversation],
    plan: &ChatPlan,
    brain: &str,
) {
    let total: usize = plan.sources.iter().map(|s| s.body.len()).sum();
    let messages: usize = selected.iter().map(|c| c.messages.len()).sum();
    eprintln!(
        "Import preview for {} export {}:",
        kind.as_str(),
        export.display()
    );
    eprintln!(
        "  {} conversation(s), {messages} message(s), {} in {} part(s)",
        selected.len(),
        size_label(total),
        plan.sources.len()
    );
    for conversation in selected {
        let day = conversation
            .created
            .as_deref()
            .and_then(|created| created.get(..10))
            .unwrap_or("undated");
        eprintln!(
            "    {day}  {} ({} message(s))",
            conversation.title,
            conversation.messages.len()
        );
    }
    if !plan.redaction.is_empty() {
        let summary = plan
            .redaction
            .iter()
            .map(|(class, count)| format!("{class}: {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("  redaction: {summary} will be redacted before upload");
    }
    eprintln!("  target: {brain}");
}

/// A `--since`/`--until` value: `YYYY-MM-DD`, checked by hand — the comparison
/// the flags make is lexicographic on exactly this shape, and `time`'s parser
/// sits behind a feature this build does not otherwise need.
fn checked_date(flag: &str, value: &str) -> CliResult<String> {
    let bad = || err(format!("{flag} expects YYYY-MM-DD, got {value}"));
    let bytes = value.as_bytes();
    if bytes.len() != 10 {
        return Err(bad());
    }
    for (index, byte) in bytes.iter().enumerate() {
        let separator = index == 4 || index == 7;
        if separator != (*byte == b'-') || (!separator && !byte.is_ascii_digit()) {
            return Err(bad());
        }
    }
    let month: u8 = value[5..7].parse().map_err(|_| bad())?;
    let day: u8 = value[8..10].parse().map_err(|_| bad())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    Ok(value.to_owned())
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
