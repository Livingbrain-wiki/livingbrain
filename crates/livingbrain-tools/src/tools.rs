//! What the model may see of a server: the tool descriptors, the read/write
//! classification, and the offered list with the disabled names removed.
//! Pure: no ports, so the gate's decisions rest on functions that cannot
//! reach the network.

use serde_json::{Value, json};

/// One tool a server listed: its name, its description, the JSON Schema of
/// its input, and its annotations (`readOnlyHint`, `destructiveHint`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub annotations: Value,
}

/// Reads one entry of a `tools/list` answer; `None` for anything without a
/// usable name.
#[must_use]
pub fn descriptor_from_value(value: &Value) -> Option<ToolDescriptor> {
    let name = value.get("name")?.as_str()?;
    if name.is_empty() || name.len() > 200 {
        return None;
    }
    Some(ToolDescriptor {
        name: name.to_owned(),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        input_schema: value
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
        annotations: value.get("annotations").cloned().unwrap_or(json!({})),
    })
}

/// What a tool does to the connection it runs through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// The tool only reads. It may run through a teammate's connection.
    Read,
    /// The tool can change things at the server: it runs on the asker's own
    /// connection, or on an approval the owner granted.
    Write,
}

/// Classifies a tool from its own annotations, failing closed: a tool is a
/// read only when it *claims* `readOnlyHint: true` and does not claim
/// `destructiveHint`, so an unannotated server gets the gate.
#[must_use]
pub fn classify(tool: &ToolDescriptor) -> Effect {
    let read_only = tool
        .annotations
        .get("readOnlyHint")
        .and_then(Value::as_bool);
    let destructive = tool
        .annotations
        .get("destructiveHint")
        .and_then(Value::as_bool);
    if read_only == Some(true) && destructive != Some(true) {
        Effect::Read
    } else {
        Effect::Write
    }
}

/// The tools the model is offered: the server's own list minus every name
/// disabled on the connection or on the workspace. A disabled tool is
/// absent here rather than refused later — the model never learns it
/// exists (acceptance B), and the gate re-checks anyway.
#[must_use]
pub fn offered_tools(
    server_tools: &[ToolDescriptor],
    connection_disabled: &[String],
    workspace_disabled: &[String],
) -> Vec<ToolDescriptor> {
    server_tools
        .iter()
        .filter(|tool| {
            !is_disabled(connection_disabled, &tool.name)
                && !is_disabled(workspace_disabled, &tool.name)
        })
        .cloned()
        .collect()
}

/// Whether a disabled list names `tool`, comparing trimmed and
/// ASCII-lowercased on both sides — the lists are human-typed switches,
/// and `Create_Issue` means to stop `create_issue` too.
pub(crate) fn is_disabled(disabled: &[String], tool: &str) -> bool {
    let tool = tool.trim();
    disabled
        .iter()
        .any(|name| name.trim().eq_ignore_ascii_case(tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, annotations: Value) -> ToolDescriptor {
        ToolDescriptor {
            name: name.to_owned(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            annotations,
        }
    }

    #[test]
    fn a_claimed_read_only_hint_is_a_read() {
        assert_eq!(
            classify(&tool("search", json!({"readOnlyHint": true}))),
            Effect::Read
        );
    }

    #[test]
    fn anything_else_is_a_write() {
        // Missing, false, or paired with destructiveHint: fail closed.
        assert_eq!(classify(&tool("run", json!({}))), Effect::Write);
        assert_eq!(
            classify(&tool("run", json!({"readOnlyHint": false}))),
            Effect::Write
        );
        assert_eq!(
            classify(&tool(
                "run",
                json!({"readOnlyHint": true, "destructiveHint": true})
            )),
            Effect::Write
        );
    }

    #[test]
    fn offered_tools_drops_whatever_either_list_names() {
        let server = [
            tool("search", json!({"readOnlyHint": true})),
            tool("create_page", json!({})),
            tool("list_pages", json!({"readOnlyHint": true})),
        ];
        let offered = offered_tools(&server, &["search".to_owned()], &["create_page".to_owned()]);
        assert_eq!(
            offered.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["list_pages"]
        );
        // The comparison is trimmed and case-blind on both sides.
        let offered = offered_tools(
            &server,
            &[" SEARCH ".to_owned()],
            &["Create_Page".to_owned()],
        );
        assert_eq!(
            offered.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["list_pages"]
        );
    }

    #[test]
    fn a_nameless_entry_is_not_a_tool() {
        assert!(descriptor_from_value(&json!({"description": "no name"})).is_none());
        let named = descriptor_from_value(&json!({"name": "search"})).expect("a name is enough");
        assert_eq!(named.input_schema, json!({"type": "object"}));
    }
}
