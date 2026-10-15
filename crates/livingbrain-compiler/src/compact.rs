//! Compaction: the session as the compiler *sends* it, shrunk.
//!
//! A coding-agent session is mostly tool output — test runs, builds, file
//! listings, the same failing command five times — and the wiki needs very
//! little of it: what ran, what failed and why, which files moved, what was
//! decided. This pass keeps exactly that and drops the rest **from the
//! compile input only**: the source in the ledger is never touched, stays
//! the citation every page carries, and a session can always be compiled
//! again without compaction ("Compile fully").
//!
//! What it keeps, always: every user message whole, the agent's own text
//! (decisions live there), every tool call's name, file, status and exit
//! code, the command itself, and from each output the lines that carry
//! signal — errors, failures, panics, warnings, `file:line` locations, test
//! summaries — plus a short head and tail. What it drops: runs of similar
//! lines (collapsed to one, with a count), the middle of long outputs, the
//! bodies of patches (kept as their file headers and hunk counts), and the
//! second and later copies of an identical call (a retry loop becomes one
//! call and a count).
//!
//! It is deterministic and model-free, so it costs nothing and runs inside
//! any Worker tick. A [`Keep`] decision from the triage classifier (Jev, when
//! one is wired) can mark individual outputs as needed word for word, or as
//! safe to cut down to their signal lines only.

use std::collections::BTreeMap;

use crate::record::{Session, ToolCall, Turn, cap};

/// The longest output kept word for word when a call's output is marked
/// needed.
const VERBATIM_CHARS: usize = 1_200;
/// Below this an output is not worth compacting.
const SMALL_OUTPUT_CHARS: usize = 160;
/// The longest command or argument list kept.
const MAX_INPUT_CHARS: usize = 240;
/// The longest one compacted call renders — the uncompacted renderer's own
/// per-call cap, so compaction never makes a call longer.
const MAX_CALL_CHARS: usize = 600;
/// The longest one subagent call renders: its task, the calls that changed
/// or failed something, and its answer. Longer than [`MAX_CALL_CHARS`] —
/// the uncompacted renderer cuts a subagent's whole session to that and
/// loses it — but bounded.
const MAX_SUBAGENT_CHARS: usize = 1_500;
/// The longest reasoning summary kept (the agent's own text is kept whole).
const MAX_REASONING_CHARS: usize = 600;
/// The longest single kept line.
const MAX_LINE_CHARS: usize = 200;

/// Tools whose output is what they read — a file, a listing, a search, a
/// page. The wiki needs that they ran and on what, not what came back:
/// the source has it.
const READING_TOOLS: [&str; 14] = [
    "read",
    "read_file",
    "view",
    "glob",
    "grep",
    "ls",
    "list_dir",
    "list_files",
    "search",
    "find",
    "webfetch",
    "websearch",
    "web_fetch",
    "web_search",
];

/// How much of an output a call keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    head: usize,
    tail: usize,
    signals: usize,
}

impl Shape {
    /// A failed call: its errors, and the tail where the summary is.
    const FAILED: Self = Self {
        head: 0,
        tail: 2,
        signals: 10,
    };
    /// A call that worked: any warnings, and the tail (test totals, the
    /// last line of a build).
    const OK: Self = Self {
        head: 0,
        tail: 2,
        signals: 6,
    };
    /// A reading call that worked: nothing but how much it read.
    const READ: Self = Self {
        head: 0,
        tail: 0,
        signals: 0,
    };
    /// Marked not needed by the classifier: errors only.
    const SIGNAL_ONLY: Self = Self {
        head: 0,
        tail: 0,
        signals: 6,
    };

    fn of(call: &ToolCall, keep: Option<Keep>) -> Self {
        if keep == Some(Keep::SignalOnly) {
            return Self::SIGNAL_ONLY;
        }
        if call.failed() {
            return Self::FAILED;
        }
        if READING_TOOLS.contains(&call.name.to_ascii_lowercase().as_str()) {
            return Self::READ;
        }
        Self::OK
    }
}

/// What the classifier said about one call's output, by the call's
/// position in the session ([`CallRef`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// Needed word for word (up to [`VERBATIM_CHARS`]).
    Verbatim,
    /// Not needed: only the signal lines survive, no head or tail.
    SignalOnly,
}

/// A call's position: turn index, call index within the turn.
pub type CallRef = (usize, usize);

/// What compaction did, for the job row and the queue card.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Characters of the transcript as the uncompacted renderer sends it.
    pub chars_before: usize,
    /// Characters after compaction.
    pub chars_after: usize,
    /// Calls collapsed into an earlier identical one.
    pub repeats_collapsed: usize,
    /// Output lines dropped.
    pub lines_dropped: usize,
}

/// The session with its turns compacted, and what that saved. `keep`
/// overrides the default for individual outputs.
#[must_use]
pub fn compact(session: &Session, keep: &BTreeMap<CallRef, Keep>) -> (Session, Stats) {
    let mut stats = Stats {
        chars_before: rendered_chars(session),
        ..Stats::default()
    };
    // The first time each distinct call was seen, and how often since.
    let mut seen: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut turns = Vec::with_capacity(session.turns.len());
    for (t, turn) in session.turns.iter().enumerate() {
        let text = match turn.role.as_str() {
            "reasoning" => cap(&squeeze_blank_lines(&turn.text), MAX_REASONING_CHARS),
            _ => squeeze_blank_lines(&turn.text),
        };
        let mut tools = Vec::with_capacity(turn.calls.len());
        if turn.calls.is_empty() {
            // A record whose calls did not parse into fields keeps its lines.
            tools.clone_from(&turn.tools);
        }
        let mut last: Option<(String, usize)> = None;
        for (c, call) in turn.calls.iter().enumerate() {
            // A subagent's work is never "the same call again".
            let key = if call.subagent.is_some() {
                format!("subagent\u{1f}{t}\u{1f}{c}")
            } else {
                identity(call)
            };
            let entry = seen.entry(key.clone()).or_insert((t + 1, 0));
            entry.1 += 1;
            if entry.1 > 1 {
                stats.repeats_collapsed += 1;
                // Back to back in the same step: one line with a count.
                if let Some((previous, count)) = &mut last
                    && *previous == key
                {
                    *count += 1;
                    if let Some(line) = tools.last_mut() {
                        *line = with_count(line, *count);
                    }
                    continue;
                }
                tools.push(format!(
                    "{} — same call as in turn {}, again{} (#{})",
                    call.name,
                    entry.0,
                    outcome_suffix(call),
                    entry.1
                ));
                last = Some((key, 1));
                continue;
            }
            let (line, dropped) = render_call(call, keep.get(&(t, c)).copied());
            stats.lines_dropped += dropped;
            tools.push(line);
            last = Some((key, 1));
        }
        turns.push(Turn {
            role: turn.role.clone(),
            at: turn.at.clone(),
            text,
            tools,
            calls: turn.calls.clone(),
        });
    }
    let compacted = Session {
        turns,
        ..session.clone()
    };
    stats.chars_after = rendered_chars(&compacted);
    (compacted, stats)
}

/// The characters [`Session::chunks`] would render, with no cap.
#[must_use]
pub fn rendered_chars(session: &Session) -> usize {
    session.rendered_chars()
}

/// One call, compacted: a header line (name, status, exit code, file), the
/// command, and the output's signal. The second value is the output lines
/// dropped.
fn render_call(call: &ToolCall, keep: Option<Keep>) -> (String, usize) {
    let mut line = call.name.clone();
    if let Some(status) = &call.status {
        line.push_str(&format!(" · {status}"));
    }
    if let Some(code) = call.exit_code {
        line.push_str(&format!(" · exit {code}"));
    }
    if let Some(path) = &call.path {
        line.push_str(&format!(" · {path}"));
    }
    let verbatim = keep == Some(Keep::Verbatim);
    if let Some(input) = call
        .input
        .as_deref()
        .filter(|input| !input.trim().is_empty())
    {
        let input = if call.format.as_deref() == Some("diff") || looks_like_a_patch(input) {
            patch_outline(input)
        } else {
            cap(input.trim(), MAX_INPUT_CHARS)
        };
        for (i, part) in input.lines().enumerate() {
            line.push_str(if i == 0 { "\n    $ " } else { "\n      " });
            line.push_str(part);
        }
    }
    let mut dropped = 0;
    if let Some(output) = call
        .output
        .as_deref()
        .filter(|output| !output.trim().is_empty())
    {
        let (kept, lost) = if verbatim {
            compact_output(output, Some(Keep::Verbatim), Shape::OK)
        } else {
            compact_output(output, keep, Shape::of(call, keep))
        };
        dropped = lost;
        for part in kept.lines() {
            line.push_str("\n    | ");
            line.push_str(part);
        }
    }
    if let Some(extra) = &call.extra {
        line.push_str(&format!("\n    + {}", cap(extra, 120)));
    }
    if let Some(agent) = &call.subagent {
        line.push_str(&subagent_outline(agent));
    }
    let ceiling = if verbatim {
        VERBATIM_CHARS + MAX_INPUT_CHARS
    } else if call.subagent.is_some() {
        MAX_SUBAGENT_CHARS
    } else {
        MAX_CALL_CHARS
    };
    (cap(&line, ceiling), dropped)
}

/// A subagent, as its outline: what it was asked, what it changed and what
/// failed (one line per call that did either), and its answer — the last
/// thing it said. Its reads and passing runs are left to the source.
fn subagent_outline(agent: &crate::record::Subagent) -> String {
    let mut out = format!("\n    ↳ subagent {}", agent.name);
    if let Some(status) = &agent.status {
        out.push_str(&format!(" · {status}"));
    }
    if let Some(description) = &agent.description {
        out.push_str(&format!(": {}", cap(description, 160)));
    }
    let mut calls = 0usize;
    for turn in &agent.turns {
        for call in turn.calls.iter().flat_map(ToolCall::with_nested) {
            calls += 1;
            if !call.failed() && !crate::triage::changes_something(call) {
                continue;
            }
            let mut item = format!("\n      - {}", call.name);
            if call.failed() {
                item.push_str(&outcome_suffix(call));
            }
            if let Some(path) = &call.path {
                item.push_str(&format!(" · {path}"));
            }
            if let Some(input) = call
                .input
                .as_deref()
                .filter(|input| !input.trim().is_empty())
            {
                let input = input.split_whitespace().collect::<Vec<_>>().join(" ");
                item.push_str(&format!(" · $ {}", cap(&input, 100)));
            }
            if call.failed()
                && let Some(error) = call
                    .output
                    .as_deref()
                    .and_then(|output| output.lines().find(|line| is_error_like(line)))
            {
                item.push_str(&format!(" · {}", cap(error.trim(), 140)));
            }
            out.push_str(&item);
        }
    }
    out.push_str(&format!("\n      ({calls} calls)"));
    if let Some(answer) = agent
        .turns
        .iter()
        .rev()
        .find(|turn| turn.role == "assistant" && !turn.text.trim().is_empty())
    {
        let answer = answer.text.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push_str(&format!("\n      answer: {}", cap(&answer, 500)));
    }
    out
}

/// One output, compacted. An output marked [`Keep::Verbatim`] stays
/// (capped); a short one stays; the rest keep `shape`'s signal lines, head
/// and tail, and runs of similar lines collapse to one with a count. The
/// second value is the lines dropped.
fn compact_output(output: &str, keep: Option<Keep>, shape: Shape) -> (String, usize) {
    let output = output.trim_end();
    let lines: Vec<&str> = output.lines().collect();
    let all: Vec<usize> = (0..lines.len()).collect();
    if keep == Some(Keep::Verbatim) {
        let (collapsed, dropped) = collapse_similar(&lines, &all);
        return (cap(&collapsed, VERBATIM_CHARS), dropped);
    }
    if output.chars().count() <= SMALL_OUTPUT_CHARS && shape != Shape::READ {
        return collapse_similar(&lines, &all);
    }
    let mut chosen: Vec<usize> = (0..lines.len().min(shape.head)).collect();
    // Errors first, then locations, up to the shape's share.
    let strong: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_error_like(line))
        .map(|(i, _)| i)
        .take(shape.signals)
        .collect();
    let room = shape.signals - strong.len();
    chosen.extend(
        lines
            .iter()
            .enumerate()
            .filter(|(i, line)| !strong.contains(i) && is_signal(line))
            .map(|(i, _)| i)
            .take(room),
    );
    chosen.extend(strong);
    chosen.extend(lines.len().saturating_sub(shape.tail)..lines.len());
    chosen.sort_unstable();
    chosen.dedup();
    if chosen.is_empty() {
        let n = lines.len();
        return (format!("({n} line{})", if n == 1 { "" } else { "s" }), n);
    }
    collapse_similar(&lines, &chosen)
}

/// The chosen lines in order, with a marker for every gap (`… N lines`) and
/// runs of similar chosen lines collapsed (`… ×N similar`).
fn collapse_similar(lines: &[&str], chosen: &[usize]) -> (String, usize) {
    let mut out: Vec<String> = Vec::new();
    let mut dropped = 0usize;
    let mut previous: Option<usize> = None;
    let mut run: Option<(String, usize)> = None;
    let flush = |out: &mut Vec<String>, run: &mut Option<(String, usize)>| {
        if let Some((_, count)) = run.take()
            && count > 1
        {
            out.push(format!("… ×{count} similar"));
        }
    };
    for &i in chosen {
        let gap = previous.map_or(i, |p| i - p - 1);
        if gap > 0 {
            flush(&mut out, &mut run);
            dropped += gap;
            out.push(format!("… {gap} line{}", if gap == 1 { "" } else { "s" }));
        }
        previous = Some(i);
        let shape = shape_of(lines[i]);
        match &mut run {
            Some((last, count)) if *last == shape => {
                *count += 1;
                dropped += 1;
            }
            _ => {
                flush(&mut out, &mut run);
                out.push(cap(lines[i].trim_end(), MAX_LINE_CHARS));
                run = Some((shape, 1));
            }
        }
    }
    flush(&mut out, &mut run);
    let tail = previous.map_or(lines.len(), |p| lines.len() - p - 1);
    if tail > 0 {
        dropped += tail;
        out.push(format!("… {tail} line{}", if tail == 1 { "" } else { "s" }));
    }
    (out.join("\n"), dropped)
}

/// A line's shape: digits and hex runs folded, whitespace squeezed — so
/// `test a::b ... ok` and `test a::c ... ok` differ, but two progress lines
/// that only count differ in nothing.
fn shape_of(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_number = false;
    for ch in line.trim().chars() {
        if ch.is_ascii_digit() {
            if !in_number {
                out.push('#');
            }
            in_number = true;
        } else {
            in_number = false;
            if ch.is_whitespace() {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            } else {
                out.push(ch);
            }
        }
    }
    out
}

/// Whether a line carries signal: an error, a failure, a warning, a
/// location, a test summary, an exit status.
#[must_use]
pub fn is_signal(line: &str) -> bool {
    const WORDS: [&str; 30] = [
        "error",
        "fail",
        "panic",
        "exception",
        "traceback",
        "fatal",
        "denied",
        "not found",
        "no such file",
        "warning",
        "exit code",
        "exit status",
        "test result",
        " passed",
        "assert",
        "expected",
        "undefined",
        "cannot",
        "can't",
        "unable to",
        "timed out",
        "timeout",
        "conflict",
        "rejected",
        "refused",
        "killed",
        "segmentation fault",
        "out of memory",
        "abort",
        "deprecated",
    ];
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    WORDS.iter().any(|word| lower.contains(word))
        || trimmed.starts_with("--> ")
        || trimmed.starts_with("✗")
        || trimmed.starts_with("✘")
        || trimmed.starts_with("× ")
        // A passing line names its file too; only a location that is not a
        // pass is signal.
        || (has_location(trimmed) && !is_pass(trimmed))
}

/// The strong kind of signal: something went wrong.
fn is_error_like(line: &str) -> bool {
    const WORDS: [&str; 12] = [
        "error",
        "fail",
        "panic",
        "exception",
        "traceback",
        "fatal",
        "denied",
        "not found",
        "no such file",
        "refused",
        "timed out",
        "killed",
    ];
    let lower = line.trim().to_ascii_lowercase();
    !lower.is_empty() && !is_pass(&lower) && WORDS.iter().any(|word| lower.contains(word))
}

/// A line that reports a pass: `✓ …`, `ok …`, `… ... ok`, `PASS …`.
fn is_pass(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('✓')
        || trimmed.starts_with("ok ")
        || trimmed.starts_with("PASS")
        || trimmed.ends_with(" ok")
        || trimmed.ends_with("... ok")
}

/// `path/to/file.ext:12` somewhere in the line.
fn has_location(line: &str) -> bool {
    line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '\'' | '"' | ','))
        .any(|token| {
            let Some((path, rest)) = token.split_once(':') else {
                return false;
            };
            rest.starts_with(|c: char| c.is_ascii_digit())
                && path.contains('.')
                && (path.contains('/') || path.rsplit('.').next().is_some_and(is_code_extension))
        })
}

fn is_code_extension(extension: &str) -> bool {
    matches!(
        extension,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "mjs"
            | "jsx"
            | "py"
            | "go"
            | "rb"
            | "java"
            | "kt"
            | "swift"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cs"
            | "php"
            | "sql"
            | "toml"
            | "yml"
            | "yaml"
            | "json"
            | "sh"
            | "md"
    )
}

fn looks_like_a_patch(input: &str) -> bool {
    input.starts_with("*** Begin Patch")
        || input.starts_with("--- ")
        || input.starts_with("diff --git")
}

/// A patch as its outline: the file headers and hunk headers, and how many
/// lines it adds and removes. The patch itself is in the source.
fn patch_outline(input: &str) -> String {
    let mut headers = Vec::new();
    let (mut added, mut removed) = (0usize, 0usize);
    for line in input.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                headers.push(cap(line, 160));
            }
        } else if line.starts_with("*** ") || line.starts_with("diff --git") {
            headers.push(cap(line, 160));
        } else if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    headers.truncate(12);
    headers.push(format!("(patch: +{added} −{removed} lines)"));
    headers.join("\n")
}

/// What makes two calls the same call: tool, input, file and outcome.
fn identity(call: &ToolCall) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{:?}",
        call.name,
        call.input.as_deref().unwrap_or_default().trim(),
        call.path.as_deref().unwrap_or_default(),
        call.status.as_deref().unwrap_or_default(),
        call.exit_code,
    )
}

fn outcome_suffix(call: &ToolCall) -> String {
    match (call.status.as_deref(), call.exit_code) {
        (_, Some(code)) if code != 0 => format!(", failed with exit {code}"),
        (Some("error"), _) => ", failed".to_owned(),
        (Some(status), _) => format!(", {status}"),
        _ => String::new(),
    }
}

/// The rendered call with `(×N in a row)` on its header line.
fn with_count(rendered: &str, count: usize) -> String {
    let (head, rest) = rendered
        .split_once('\n')
        .map_or((rendered, None), |(h, r)| (h, Some(r)));
    let head = head.rsplit_once(" (×").map_or(head, |(head, _)| head);
    match rest {
        Some(rest) => format!("{head} (×{count} in a row)\n{rest}"),
        None => format!("{head} (×{count} in a row)"),
    }
}

fn squeeze_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.trim().lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_build_log_keeps_its_errors_and_drops_the_progress() {
        let mut log = String::new();
        for i in 0..200 {
            log.push_str(&format!("   Compiling crate-{i} v0.{i}.0\n"));
        }
        log.push_str("error[E0308]: mismatched types\n");
        log.push_str("  --> src/lib.rs:42:9\n");
        log.push_str("error: could not compile `demo` (lib) due to 1 previous error\n");
        let (kept, dropped) = compact_output(&log, None, Shape::FAILED);
        assert!(kept.contains("error[E0308]: mismatched types"));
        assert!(kept.contains("--> src/lib.rs:42:9"));
        assert!(kept.contains("could not compile"));
        assert!(dropped >= 190, "dropped {dropped}");
        assert!(kept.chars().count() < log.chars().count() / 10);
    }

    #[test]
    fn similar_lines_collapse_with_a_count() {
        let lines = "warning: unused variable `a`\nwarning: unused variable `a`\nwarning: unused variable `a`\n";
        let (kept, _) = compact_output(&lines.repeat(40), None, Shape::OK);
        assert!(kept.contains("warning: unused variable `a`"));
        assert!(kept.contains("similar"), "{kept}");
        assert!(kept.lines().count() < 10, "{kept}");
    }

    #[test]
    fn a_short_output_is_kept_whole_and_a_verbatim_one_too() {
        assert_eq!(compact_output("ok\n", None, Shape::OK).0, "ok");
        let long = "line of data\n".repeat(30);
        assert!(
            compact_output(&long, Some(Keep::Verbatim), Shape::OK)
                .0
                .contains("line of data")
        );
        assert_eq!(compact_output(&long, None, Shape::READ).0, "(30 lines)");
        let (signal, _) = compact_output(
            &format!("{long}error: boom\n{long}"),
            Some(Keep::SignalOnly),
            Shape::SIGNAL_ONLY,
        );
        assert!(signal.contains("error: boom"));
        assert!(!signal.lines().any(|line| line == "line of data"));
    }

    #[test]
    fn locations_and_test_summaries_are_signal() {
        assert!(is_signal("    at foo (src/app.ts:12:3)"));
        assert!(is_signal("test result: FAILED. 3 passed; 1 failed"));
        assert!(!is_signal("   Compiling serde v1.0.0"));
        assert!(!is_signal("version: 1.2.3"));
    }

    #[test]
    fn a_patch_becomes_its_outline() {
        let patch =
            "--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,3 +1,4 @@\n-old\n+new\n+newer\n context\n";
        let outline = patch_outline(patch);
        assert!(outline.contains("+++ b/src/x.rs"));
        assert!(outline.contains("+2 −1"));
        assert!(!outline.contains("newer"));
    }
}
