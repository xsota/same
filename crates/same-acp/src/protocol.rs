use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: Value,
    #[serde(default)]
    pub agent_capabilities: Value,
    #[serde(default)]
    pub agent_info: Option<AgentInfo>,
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    pub session_id: String,
    #[serde(default)]
    pub models: Option<Value>,
    #[serde(default)]
    pub modes: Option<Value>,
    /// Parsed from `models` / `configOptions` by [`parse_model_state`].
    #[serde(skip)]
    pub model_state: Option<AcpModelState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpModel {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// The models an ACP session offers and the one it uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpModelState {
    pub models: Vec<AcpModel>,
    pub current: Option<String>,
    /// Set when the models come from a session config option: the model is
    /// then changed with `session/set_config_option` on this id instead of
    /// `session/set_model`.
    pub config_id: Option<String>,
    /// Reasoning effort levels the agent offers (a `thought_level` config
    /// option), without its own "default" entry.
    pub efforts: Vec<AcpModel>,
    /// The effort in use, as the agent reports it (may be "default").
    pub current_effort: Option<String>,
    /// Config option id that changes the effort.
    pub effort_config_id: Option<String>,
}

/// Reads the model choice from a `session/new` / `session/load` result, a
/// `session/set_config_option` result or a `config_option_update`.
///
/// Accepts both forms agents use: a `configOptions` entry with category
/// `model` (preferred), or the older `models` object
/// (`currentModelId` + `availableModels`).
pub fn parse_model_state(val: &Value) -> Option<AcpModelState> {
    let mut state = parse_model_part(val);
    if let Some(opt) = val
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|opts| {
            opts.iter()
                .find(|o| o.get("category").and_then(Value::as_str) == Some("thought_level"))
        })
    {
        let mut efforts = Vec::new();
        collect_config_values(opt.get("options"), &mut efforts);
        // SAME's picker has its own "Default" row.
        efforts.retain(|e| e.id != "default");
        if !efforts.is_empty() {
            let st = state.get_or_insert_with(AcpModelState::default);
            st.efforts = efforts;
            // Kept as the agent reports it ("default" included) so it can
            // be restored later.
            st.current_effort = opt
                .get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_string);
            st.effort_config_id = opt.get("id").and_then(Value::as_str).map(str::to_string);
        }
    }
    state
}

/// The model half of [`parse_model_state`].
fn parse_model_part(val: &Value) -> Option<AcpModelState> {
    if let Some(opts) = val.get("configOptions").and_then(Value::as_array) {
        let model_opt = opts
            .iter()
            .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
            .or_else(|| {
                opts.iter()
                    .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))
            });
        if let Some(opt) = model_opt {
            let mut models = Vec::new();
            collect_config_values(opt.get("options"), &mut models);
            let config_id = opt.get("id").and_then(Value::as_str).map(str::to_string);
            if !models.is_empty() && config_id.is_some() {
                return Some(AcpModelState {
                    models,
                    current: opt
                        .get("currentValue")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    config_id,
                    ..Default::default()
                });
            }
        }
    }

    let m = val.get("models")?;
    let models: Vec<AcpModel> = m
        .get("availableModels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|it| {
                    let id = it.get("modelId").and_then(Value::as_str)?;
                    Some(AcpModel {
                        id: id.to_string(),
                        name: it
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_string(),
                        description: it
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        return None;
    }
    Some(AcpModelState {
        models,
        current: m
            .get("currentModelId")
            .and_then(Value::as_str)
            .map(str::to_string),
        config_id: None,
        ..Default::default()
    })
}

/// Flattens select-option values, which may be nested in groups.
fn collect_config_values(options: Option<&Value>, out: &mut Vec<AcpModel>) {
    let Some(arr) = options.and_then(Value::as_array) else {
        return;
    };
    for it in arr {
        if let Some(value) = it.get("value").and_then(Value::as_str) {
            out.push(AcpModel {
                id: value.to_string(),
                name: it
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(value)
                    .to_string(),
                description: it
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        } else if it.get("options").is_some() {
            collect_config_values(it.get("options"), out);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPermissionOption {
    pub option_id: String,
    pub name: String,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone)]
pub enum SessionUpdate {
    AgentMessageChunk {
        text: String,
    },
    AgentThoughtChunk {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: Option<String>,
        status: String,
        raw_input: Option<Value>,
    },
    ToolCallUpdate {
        tool_call_id: String,
        title: Option<String>,
        kind: Option<String>,
        status: Option<String>,
        raw_input: Option<Value>,
        raw_output: Option<Value>,
    },
    AvailableCommands {
        commands: Vec<(String, String)>,
    },
    /// The session's model list or current model changed.
    ModelState(AcpModelState),
    Other(Value),
}

pub fn parse_session_update(update: &Value) -> SessionUpdate {
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");

    match kind {
        "agent_message_chunk" => {
            let text = extract_content_text(update.get("content"));
            SessionUpdate::AgentMessageChunk { text }
        }
        "agent_thought_chunk" => {
            let text = extract_content_text(update.get("content"));
            SessionUpdate::AgentThoughtChunk { text }
        }
        "tool_call" => SessionUpdate::ToolCall {
            tool_call_id: update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Tool call")
                .to_string(),
            kind: update
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("in_progress")
                .to_string(),
            raw_input: update.get("rawInput").cloned(),
        },
        "tool_call_update" => SessionUpdate::ToolCallUpdate {
            tool_call_id: update
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: update
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            kind: update
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: update
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            raw_input: update.get("rawInput").cloned(),
            raw_output: update
                .get("rawOutput")
                .cloned()
                .or_else(|| update.get("content").cloned()),
        },
        "available_commands_update" => {
            let mut commands = Vec::new();
            if let Some(arr) = update.get("availableCommands").and_then(Value::as_array) {
                for item in arr {
                    if let Some(name) = item.get("name").and_then(Value::as_str) {
                        let desc = item
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        commands.push((name.to_string(), desc));
                    }
                }
            }
            SessionUpdate::AvailableCommands { commands }
        }
        "config_option_update" => match parse_model_state(update) {
            Some(state) => SessionUpdate::ModelState(state),
            None => SessionUpdate::Other(update.clone()),
        },
        _ => SessionUpdate::Other(update.clone()),
    }
}

#[cfg(test)]
mod model_state_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_models_object() {
        let s = parse_model_state(&json!({
            "sessionId": "s",
            "models": {
                "currentModelId": "a/b",
                "availableModels": [
                    { "modelId": "a/b", "name": "B", "description": "fast" },
                    { "modelId": "a/c" }
                ]
            }
        }))
        .unwrap();
        assert_eq!(s.current.as_deref(), Some("a/b"));
        assert_eq!(s.models[0].description, "fast");
        assert_eq!(s.models[1].name, "a/c");
        assert!(s.config_id.is_none());
    }

    #[test]
    fn prefers_model_config_option_and_flattens_groups() {
        let s = parse_model_state(&json!({
            "models": { "availableModels": [{ "modelId": "old" }] },
            "configOptions": [
                { "id": "mode", "category": "mode", "type": "select", "currentValue": "x",
                  "options": [{ "value": "x", "name": "X" }] },
                { "id": "model", "category": "model", "type": "select", "currentValue": "m2",
                  "options": [
                    { "group": "g", "name": "G", "options": [{ "value": "m1", "name": "M1" }] },
                    { "value": "m2", "name": "M2" }
                  ] }
            ]
        }))
        .unwrap();
        assert_eq!(s.config_id.as_deref(), Some("model"));
        assert_eq!(s.current.as_deref(), Some("m2"));
        let ids: Vec<&str> = s.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["m1", "m2"]);
    }

    #[test]
    fn parses_effort_option() {
        let s = parse_model_state(&json!({
            "configOptions": [
                { "id": "model", "category": "model", "currentValue": "m1",
                  "options": [{ "value": "m1", "name": "M1" }] },
                { "id": "effort", "category": "thought_level", "currentValue": "default",
                  "options": [{ "value": "low", "name": "Low" },
                              { "value": "high", "name": "High" },
                              { "value": "default", "name": "Default" }] }
            ]
        }))
        .unwrap();
        let ids: Vec<&str> = s.efforts.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["low", "high"]);
        assert_eq!(s.effort_config_id.as_deref(), Some("effort"));
        assert_eq!(s.current_effort.as_deref(), Some("default"));
        assert_eq!(s.models.len(), 1);
    }

    #[test]
    fn no_models_is_none() {
        assert!(parse_model_state(&json!({ "sessionId": "s" })).is_none());
        assert!(matches!(
            parse_session_update(&json!({ "sessionUpdate": "config_option_update",
                "configOptions": [] })),
            SessionUpdate::Other(_)
        ));
    }
}

fn extract_content_text(content: Option<&Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    if let Some(s) = c.get("text").and_then(Value::as_str) {
        return s.to_string();
    }
    if let Some(s) = c.as_str() {
        return s.to_string();
    }
    String::new()
}
