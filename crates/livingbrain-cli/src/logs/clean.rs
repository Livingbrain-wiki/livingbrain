//! Text hygiene shared by both collectors: what of a "user" line is the
//! person, and what is the agent talking to itself; and how a long text is
//! clipped so its head and its tail both survive.
//!
//! Both agents put machinery into the user role. Codex opens every session
//! with `<permissions instructions>`, `<environment_context>`, an AGENTS.md
//! dump and friends, and writes `<turn_aborted>` when the person interrupts.
//! Claude Code wraps `<system-reminder>` blocks into prompts, echoes slash
//! commands as `<command-name>` and their output as
//! `<local-command-stdout>`, and writes `[Request interrupted by user]`. None
//! of that is the conversation: it is stripped here, and an interruption
//! becomes an event rather than a message nobody typed.

/// What one user line's text turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UserText {
    /// Something the person said, with the boilerplate taken out.
    Said(String),
    /// Not a message: an event in the session (an interruption, a
    /// compaction), described in a few words.
    Event(String),
    /// Boilerplate and nothing else.
    Nothing,
}

/// Blocks that are wrapped context, not conversation, dropped whole wherever
/// they appear in a user turn — open tag to close tag, or to the end of the
/// text when the close is missing.
const DROPPED_BLOCKS: &[&str] = &[
    // Claude Code
    "system-reminder",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "command-message",
    "bash-stdout",
    "bash-stderr",
    "task-notification",
    "user-prompt-submit-hook",
    // Codex
    "permissions instructions",
    "environment_context",
    "user_instructions",
    "developer_instructions",
    "collaboration_mode",
    "skills_instructions",
    "recommended_plugins",
    "personality_spec",
    "INSTRUCTIONS",
    "turn_aborted",
];

/// The text a person typed, separated from the machinery around it.
pub(super) fn user_text(raw: &str) -> UserText {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return UserText::Nothing;
    }
    // Interruptions, in either agent's spelling.
    if trimmed.starts_with("<turn_aborted>") {
        return UserText::Event("Turn interrupted by the user".to_owned());
    }
    if let Some(rest) = trimmed.strip_prefix("[Request interrupted by user") {
        return UserText::Event(if rest.starts_with(" for tool use") {
            "Interrupted by the user during a tool call".to_owned()
        } else {
            "Turn interrupted by the user".to_owned()
        });
    }
    if trimmed.starts_with("This session is being continued from a previous conversation") {
        return UserText::Event(
            "Context compacted: the session continued from a summary".to_owned(),
        );
    }

    let mut text = trimmed.to_owned();
    // A slash command is a person's action: `/name args`, the wrappers gone.
    if let Some(name) = inner(&text, "command-name") {
        let args = inner(&text, "command-args").unwrap_or_default();
        let name = name.trim();
        let command = if args.trim().is_empty() {
            name.to_owned()
        } else {
            format!("{name} {}", args.trim())
        };
        text = drop_block(&text, "command-name");
        text = drop_block(&text, "command-args");
        text = format!("{command}\n{text}");
    }
    // A shell command typed with `!` is one too.
    if let Some(input) = inner(&text, "bash-input") {
        text = drop_block(&text, "bash-input");
        text = format!("! {}\n{text}", input.trim());
    }
    for tag in DROPPED_BLOCKS {
        text = drop_block(&text, tag);
    }
    let text = strip_agents_header(text.trim());
    if text.is_empty() {
        UserText::Nothing
    } else {
        UserText::Said(text)
    }
}

/// Codex introduces an AGENTS.md dump with a heading line of its own
/// (`# AGENTS.md instructions for <dir>`) before the `<INSTRUCTIONS>` block;
/// with the block gone, the heading alone is noise too.
fn strip_agents_header(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("# AGENTS.md instructions") {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_owned()
}

/// The text between `<tag>` and `</tag>`, the first such block.
fn inner(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..]
        .find(&close)
        .map_or(text.len(), |end| start + end);
    Some(text[start..end].to_owned())
}

/// `text` with every `<tag>…</tag>` block removed; an unclosed block runs to
/// the end of the text.
fn drop_block(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        out.push_str(&rest[..start]);
        match rest[start..].find(&close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `text` kept whole when it is at most `max` bytes, else its first and last
/// `max / 2` bytes (on character boundaries) around a line saying how much
/// was left out. Pure, so the same text clips to the same bytes every time.
pub(super) fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let half = max / 2;
    let mut head_end = half;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - half;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    format!(
        "{}\n… {omitted} bytes omitted …\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::{UserText, clip, user_text};

    #[test]
    fn codex_bootstrap_is_not_a_user_turn() {
        for raw in [
            "<environment_context>\n  <cwd>/x</cwd>\n</environment_context>",
            "<permissions instructions>\nsandbox…\n</permissions instructions>",
            "# AGENTS.md instructions for /x\n\n<INSTRUCTIONS>\nBe brief.\n</INSTRUCTIONS>",
            "<user_instructions>\nYou are Codex.\n</user_instructions>",
            "<recommended_plugins>\n- a\n</recommended_plugins>",
        ] {
            assert_eq!(user_text(raw), UserText::Nothing, "{raw}");
        }
    }

    #[test]
    fn interruptions_are_events() {
        assert!(matches!(
            user_text(
                "<turn_aborted>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>"
            ),
            UserText::Event(_)
        ));
        assert!(matches!(
            user_text("[Request interrupted by user for tool use]"),
            UserText::Event(text) if text.contains("tool call")
        ));
    }

    #[test]
    fn claude_wrappers_are_peeled() {
        assert_eq!(
            user_text("Fix it<system-reminder>\nnoise\n</system-reminder>"),
            UserText::Said("Fix it".to_owned())
        );
        assert_eq!(
            user_text(
                "<command-name>/review</command-name>\n<command-message>review</command-message>\n<command-args>12</command-args>"
            ),
            UserText::Said("/review 12".to_owned())
        );
        assert_eq!(
            user_text("<local-command-stdout>ok</local-command-stdout>"),
            UserText::Nothing
        );
        assert_eq!(
            user_text("<bash-input>ls -la</bash-input>"),
            UserText::Said("! ls -la".to_owned())
        );
        assert_eq!(
            user_text("<bash-stdout>a</bash-stdout><bash-stderr></bash-stderr>"),
            UserText::Nothing
        );
    }

    #[test]
    fn a_clip_keeps_head_and_tail() {
        assert_eq!(clip("short", 10), "short");
        let long = format!("{}{}", "a".repeat(100), "b".repeat(100));
        let clipped = clip(&long, 40);
        assert!(clipped.starts_with(&"a".repeat(20)));
        assert!(clipped.ends_with(&"b".repeat(20)));
        assert!(clipped.contains("… 160 bytes omitted …"));
        // Multi-byte characters are never split.
        let accented = "é".repeat(100);
        let clipped = clip(&accented, 31);
        assert!(clipped.contains("bytes omitted"));
    }
}
