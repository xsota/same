//! Unified diffs of the file changes an agent asks to make, for approval
//! prompts.

use serde_json::Value;
use similar::TextDiff;

/// A unified diff of `old` → `new` for `path`, with three lines of context.
pub fn unified(path: &str, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

fn str_field<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Reads an existing file's text so `Write` overwrites show deletions as well
/// as additions; returns an empty string for new, oversized, or non-UTF-8 files.
fn read_existing_for_diff(path: &str) -> String {
    const MAX_DIFF_FILE_BYTES: u64 = 1024 * 1024;
    let p = std::path::Path::new(path);
    if p.is_absolute()
        && std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= MAX_DIFF_FILE_BYTES)
    {
        std::fs::read_to_string(p).unwrap_or_default()
    } else {
        String::new()
    }
}

/// The diff a Claude Code file tool would make, from its input. `Edit`
/// shows the replaced snippet (the whole file is not known here); `Write`
/// shows the new content.
pub fn claude_tool_diff(tool: &str, input: &Value) -> Option<String> {
    let path = str_field(input, "file_path")
        .or_else(|| str_field(input, "notebook_path"))
        .unwrap_or("file");
    let diff = match tool {
        "Edit" => unified(
            path,
            str_field(input, "old_string")?,
            str_field(input, "new_string")?,
        ),
        "MultiEdit" => input
            .get("edits")?
            .as_array()?
            .iter()
            .filter_map(|e| {
                Some(unified(
                    path,
                    str_field(e, "old_string")?,
                    str_field(e, "new_string")?,
                ))
            })
            .collect::<Vec<_>>()
            .join(""),
        "Write" => {
            let old = read_existing_for_diff(path);
            unified(path, &old, str_field(input, "content")?)
        }
        "NotebookEdit" => unified(path, "", str_field(input, "new_source")?),
        _ => return None,
    };
    (!diff.trim().is_empty()).then_some(diff)
}

/// Whether a Claude Code tool edits files.
pub fn is_claude_edit_tool(tool: &str) -> bool {
    matches!(tool, "Edit" | "MultiEdit" | "Write" | "NotebookEdit")
}

/// The diff in an ACP tool call's `content` (`{"type": "diff", "path",
/// "oldText", "newText"}` entries).
pub fn acp_tool_diff(tool_call: &Value) -> Option<String> {
    let diff: String = tool_call
        .get("content")?
        .as_array()?
        .iter()
        .filter(|c| str_field(c, "type") == Some("diff"))
        .filter_map(|c| {
            Some(unified(
                str_field(c, "path").unwrap_or("file"),
                str_field(c, "oldText").unwrap_or(""),
                str_field(c, "newText")?,
            ))
        })
        .collect();
    (!diff.trim().is_empty()).then_some(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edit_shows_replaced_lines() {
        let d = claude_tool_diff(
            "Edit",
            &json!({"file_path": "src/a.rs", "old_string": "let x = 1;\n", "new_string": "let x = 2;\n"}),
        )
        .unwrap();
        assert!(d.contains("--- a/src/a.rs"));
        assert!(d.contains("-let x = 1;"));
        assert!(d.contains("+let x = 2;"));
    }

    #[test]
    fn write_and_multi_edit() {
        let d =
            claude_tool_diff("Write", &json!({"file_path": "n.txt", "content": "hi\n"})).unwrap();
        assert!(d.contains("+hi"));
        let d = claude_tool_diff(
            "MultiEdit",
            &json!({"file_path": "m", "edits": [
                {"old_string": "a\n", "new_string": "b\n"},
                {"old_string": "c\n", "new_string": "d\n"}
            ]}),
        )
        .unwrap();
        assert!(d.contains("-a") && d.contains("+b") && d.contains("-c") && d.contains("+d"));
        assert!(claude_tool_diff("Bash", &json!({"command": "ls"})).is_none());
        assert!(is_claude_edit_tool("Write") && !is_claude_edit_tool("Bash"));
    }

    #[test]
    fn acp_diff_content() {
        let d = acp_tool_diff(&json!({"content": [
            {"type": "content", "content": {"type": "text", "text": "x"}},
            {"type": "diff", "path": "/w/f", "oldText": "1\n", "newText": "2\n"}
        ]}))
        .unwrap();
        assert!(d.contains("-1") && d.contains("+2"));
        assert!(acp_tool_diff(&json!({"content": []})).is_none());
    }
}
