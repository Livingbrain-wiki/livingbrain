//! One tool call, as a reader wants to see it: what was run or changed, and
//! what came back.
//!
//! The agents name their tools differently and shape the inputs differently
//! — Claude Code's `Bash {command}`, `Edit {file_path, old_string,
//! new_string}`; Codex's `exec_command {cmd}`, `shell {command: [..]}`,
//! `apply_patch` with the patch as raw text — so [`describe`] folds each into
//! the same three fields: a `format` a renderer can pick a code fence by, the
//! file `path` when there is one, and the `input` as text. Anything it does
//! not know is the input object as compact JSON.
//!
//! Every text here is clipped ([`super::clean::clip`]) before it can reach a
//! record: a tool's output to [`MAX_TOOL_OUTPUT_BYTES`], an input to
//! [`MAX_TOOL_INPUT_BYTES`]. The full bytes stay in the log on this machine.

use serde_json::Value;

use super::ToolCall;
use super::clean::clip;

/// The most of one tool result a record keeps: its first and last 2 KiB.
pub(super) const MAX_TOOL_OUTPUT_BYTES: usize = 4 * 1024;

/// The most of one tool input a record keeps: a command, a patch, a file
/// written whole — its first and last 8 KiB.
pub(super) const MAX_TOOL_INPUT_BYTES: usize = 16 * 1024;

/// A fence a renderer can show a call's input under.
pub(super) const SHELL: &str = "shell";
pub(super) const DIFF: &str = "diff";
pub(super) const JSON: &str = "json";
pub(super) const TEXT: &str = "text";

/// A call by `name` with `input`, described.
pub(super) fn describe(name: &str, input: &Value) -> ToolCall {
    let (format, path, text) = shape(name, input);
    ToolCall {
        name: name.to_owned(),
        format: text.as_ref().map(|_| format),
        path: path.filter(|path| !path.is_empty()),
        input: text
            .filter(|text| !text.is_empty())
            .map(|text| clip(&text, MAX_TOOL_INPUT_BYTES)),
        ..ToolCall::default()
    }
}

/// `(format, path, input text)` for one call.
fn shape(name: &str, input: &Value) -> (&'static str, Option<String>, Option<String>) {
    let s = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    let path = s("file_path")
        .or_else(|| s("notebook_path"))
        .or_else(|| s("path"));
    match name {
        // Shells, in each agent's spelling.
        "Bash" | "exec_command" | "shell" | "container.exec" | "local_shell" | "shell_command"
        | "unified_exec" => {
            let command = input
                .get("command")
                .or_else(|| input.get("cmd"))
                .map(command_text)
                .unwrap_or_default();
            (SHELL, None, Some(command))
        }
        "write_stdin" => (TEXT, None, s("chars")),
        // File edits: a diff of the change, under the file it changes.
        "Edit" => (DIFF, path, Some(edit_diff(input))),
        "MultiEdit" => {
            let edits = input
                .get("edits")
                .and_then(Value::as_array)
                .map(|edits| edits.iter().map(edit_diff).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            (DIFF, path, Some(edits))
        }
        "Write" => (TEXT, path, s("content")),
        "NotebookEdit" => (TEXT, path, s("new_source")),
        "apply_patch" => (
            DIFF,
            None,
            input
                .as_str()
                .map(str::to_owned)
                .or_else(|| s("input"))
                .or_else(|| s("patch")),
        ),
        // A read names its file; that is the whole of its input.
        "Read" => (TEXT, path, None),
        "Task" | "Agent" => (TEXT, None, s("prompt")),
        _ => match input {
            Value::Null => (JSON, path, None),
            Value::String(text) => (TEXT, path, Some(text.clone())),
            Value::Object(map) if map.is_empty() => (JSON, path, None),
            other => (JSON, path, Some(other.to_string())),
        },
    }
}

/// A command as a reader would have typed it: a string as is; an argv array
/// unwrapped from `bash -lc <script>` when that is all it is, else joined.
fn command_text(command: &Value) -> String {
    match command {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            if let [shell, flag, script] = words.as_slice()
                && shell.ends_with("sh")
                && flag.starts_with('-')
                && flag.ends_with('c')
            {
                return (*script).to_owned();
            }
            words
                .iter()
                .map(|word| {
                    if word.is_empty() || word.contains(char::is_whitespace) {
                        format!("'{}'", word.replace('\'', r"'\''"))
                    } else {
                        (*word).to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => String::new(),
    }
}

/// One `old_string`/`new_string` pair as diff lines.
fn edit_diff(edit: &Value) -> String {
    let side = |key: &str, mark: char| {
        edit.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .map(|line| format!("{mark}{line}"))
            .collect::<Vec<_>>()
    };
    let mut lines = side("old_string", '-');
    lines.extend(side("new_string", '+'));
    lines.join("\n")
}

/// What a Codex tool result says: the output text, the exit code when it
/// names one, and whether the call failed. Codex has written three shapes:
/// a JSON string `{"output", "metadata": {"exit_code"}}`, an object
/// `{content, success}`, and (`exec_command`) plain text with a header —
/// `Process exited with code N` … `Output:` — before the output itself.
pub(super) fn codex_result(raw: &Value) -> (String, Option<i64>, Option<&'static str>) {
    let (text, success) = match raw {
        Value::String(text) => (text.clone(), None),
        Value::Object(map) => (
            map.get("content")
                .or_else(|| map.get("output"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            map.get("success").and_then(Value::as_bool),
        ),
        _ => (String::new(), None),
    };
    // The JSON-in-a-string shape.
    if text.trim_start().starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(&text)
        && let Some(output) = parsed.get("output").and_then(Value::as_str)
    {
        let exit = parsed
            .get("metadata")
            .and_then(|meta| meta.get("exit_code"))
            .and_then(Value::as_i64);
        return (output.to_owned(), exit, status_of(exit, success));
    }
    // The header shape.
    let mut exit = None;
    let mut body = text.as_str();
    if text.starts_with("Command: ") || text.starts_with("Chunk ID: ") {
        for line in text.lines() {
            if let Some(code) = line.strip_prefix("Process exited with code ") {
                exit = code.trim().parse::<i64>().ok();
            }
        }
        if let Some(at) = text.find("\nOutput:\n") {
            body = &text[at + "\nOutput:\n".len()..];
        }
    }
    (body.to_owned(), exit, status_of(exit, success))
}

/// `error` for a non-zero exit or an explicit failure, `ok` for a zero exit
/// or an explicit success, nothing when the log does not say.
fn status_of(exit: Option<i64>, success: Option<bool>) -> Option<&'static str> {
    match (exit, success) {
        (Some(0), _) | (None, Some(true)) => Some("ok"),
        (Some(_), _) | (None, Some(false)) => Some("error"),
        (None, None) => None,
    }
}

/// A Claude Code `tool_result` block's text: a string, or the text blocks
/// of an array (an image becomes `[image]`).
pub(super) fn claude_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block.get("text").and_then(Value::as_str).map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Claude Code's Bash reports a failure as `Exit code N` on the result's
/// first line.
pub(super) fn claude_exit_code(text: &str) -> Option<i64> {
    text.lines()
        .next()?
        .strip_prefix("Exit code ")?
        .trim()
        .parse()
        .ok()
}

/// Attach a result to its call: output clipped, status and exit code set.
pub(super) fn settle(
    call: &mut ToolCall,
    output: &str,
    exit: Option<i64>,
    status: Option<&'static str>,
) {
    if !output.is_empty() {
        call.output = Some(clip(output, MAX_TOOL_OUTPUT_BYTES));
    }
    call.exit_code = exit;
    call.status = status;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{DIFF, SHELL, claude_exit_code, codex_result, command_text, describe};

    #[test]
    fn shells_show_the_command() {
        let call = describe("Bash", &json!({"command": "ls -la", "description": "list"}));
        assert_eq!(call.format, Some(SHELL));
        assert_eq!(call.input.as_deref(), Some("ls -la"));
        let call = describe(
            "exec_command",
            &json!({"cmd": "rg --files", "workdir": "/x"}),
        );
        assert_eq!(call.input.as_deref(), Some("rg --files"));
        assert_eq!(command_text(&json!(["bash", "-lc", "echo hi"])), "echo hi");
        assert_eq!(
            command_text(&json!(["git", "commit", "-m", "a b"])),
            "git commit -m 'a b'"
        );
    }

    #[test]
    fn edits_are_diffs_under_their_file() {
        let call = describe(
            "Edit",
            &json!({"file_path": "/x/a.rs", "old_string": "a\nb", "new_string": "c"}),
        );
        assert_eq!(call.format, Some(DIFF));
        assert_eq!(call.path.as_deref(), Some("/x/a.rs"));
        assert_eq!(call.input.as_deref(), Some("-a\n-b\n+c"));
        let patch = describe("apply_patch", &json!("*** Begin Patch\n*** End Patch"));
        assert_eq!(
            patch.input.as_deref(),
            Some("*** Begin Patch\n*** End Patch")
        );
        let read = describe("Read", &json!({"file_path": "/x/a.rs"}));
        assert_eq!(read.input, None);
        assert_eq!(read.format, None);
    }

    #[test]
    fn codex_results_in_all_three_shapes() {
        let header = json!(
            "Command: /bin/zsh -lc pwd\nChunk ID: b3\nWall time: 0.0 seconds\nProcess exited with code 2\nOriginal token count: 11\nOutput:\nno such file\n"
        );
        assert_eq!(
            codex_result(&header),
            ("no such file\n".to_owned(), Some(2), Some("error"))
        );
        let wrapped = json!(r#"{"output":"README.md\n","metadata":{"exit_code":0}}"#);
        assert_eq!(
            codex_result(&wrapped),
            ("README.md\n".to_owned(), Some(0), Some("ok"))
        );
        assert_eq!(
            codex_result(&json!({"content": "done", "success": true})),
            ("done".to_owned(), None, Some("ok"))
        );
        assert_eq!(
            codex_result(&json!("plain")),
            ("plain".to_owned(), None, None)
        );
    }

    #[test]
    fn claude_exit_codes_are_read_off_the_first_line() {
        assert_eq!(claude_exit_code("Exit code 127\nnot found"), Some(127));
        assert_eq!(claude_exit_code("fine"), None);
    }
}
