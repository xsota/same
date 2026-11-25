use crate::exec::{build_command, build_wsl_command, ExecTarget};
use crate::protocol::{
    parse_model_state, parse_session_update, AcpModelState, AcpPermissionOption, InitializeResult,
    JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, NewSessionResult,
    PromptResult, SessionUpdate,
};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AcpClientConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub auto_approve: bool,
    /// Where the agent runs (the host, or a WSL distribution).
    pub target: ExecTarget,
}

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Selected { option_id: String },
    Cancelled,
}

pub struct PermissionResponder {
    tx: oneshot::Sender<PermissionDecision>,
}

impl PermissionResponder {
    pub fn respond(self, decision: PermissionDecision) {
        let _ = self.tx.send(decision);
    }
}

pub enum AcpEvent {
    SessionUpdate {
        session_id: String,
        update: SessionUpdate,
    },
    PermissionRequest {
        request_id: String,
        session_id: String,
        tool_call_id: Option<String>,
        title: String,
        description: String,
        options: Vec<AcpPermissionOption>,
        responder: PermissionResponder,
    },
    StderrLine(String),
    ProcessExited(Option<i32>),
}

struct ManagedTerminal {
    output: Arc<Mutex<String>>,
    exit_code: Arc<Mutex<Option<i32>>>,
    child: Arc<Mutex<Option<Child>>>,
}

pub struct AcpClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next_id: AtomicU64,
    child: Arc<Mutex<Option<Child>>>,
    target: ExecTarget,
    pub init_result: InitializeResult,
}

impl AcpClient {
    pub async fn spawn(
        config: AcpClientConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<AcpEvent>)> {
        let mut cmd = build_command(
            &config.target,
            &config.command,
            &config.args,
            &config.cwd,
            &config.env,
        )
        .await;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn ACP agent '{} {}'",
                config.command,
                config.args.join(" ")
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to capture ACP stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture ACP stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture ACP stderr"))?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let terminals: Arc<Mutex<HashMap<String, ManagedTerminal>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let child_arc = Arc::new(Mutex::new(Some(child)));
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Stderr reader task
        {
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let _ = event_tx.send(AcpEvent::StderrLine(line));
                }
            });
        }

        // Stdout JSON-RPC reader task
        {
            let stdin = stdin.clone();
            let pending = pending.clone();
            let terminals = terminals.clone();
            let event_tx = event_tx.clone();
            let child_arc = child_arc.clone();
            let base_cwd = config.cwd.clone();
            let target = config.target.clone();
            let auto_approve = config.auto_approve;

            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let msg: Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(err) => {
                            debug!("Non-JSON ACP stdout line ({err}): {trimmed}");
                            continue;
                        }
                    };

                    let has_method = msg.get("method").and_then(Value::as_str).is_some();
                    let has_id = msg.get("id").is_some() && !msg.get("id").unwrap().is_null();

                    if !has_method && has_id {
                        // Response to our request
                        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                            let tx = pending.lock().await.remove(&id);
                            if let Some(tx) = tx {
                                if let Some(err) = msg.get("error") {
                                    let message = err
                                        .get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("ACP error")
                                        .to_string();
                                    let _ = tx.send(Err(anyhow!("ACP RPC error: {message}")));
                                } else {
                                    let result = msg.get("result").cloned().unwrap_or(Value::Null);
                                    let _ = tx.send(Ok(result));
                                }
                            }
                        }
                    } else if has_method && !has_id {
                        // Notification from agent
                        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                        if method == "session/update" {
                            if let Some(params) = msg.get("params") {
                                let session_id = params
                                    .get("sessionId")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                if let Some(update_val) = params.get("update") {
                                    let update = parse_session_update(update_val);
                                    let _ = event_tx
                                        .send(AcpEvent::SessionUpdate { session_id, update });
                                }
                            }
                        }
                    } else if has_method && has_id {
                        // Request from agent to client
                        let id = msg.get("id").cloned().unwrap_or(Value::Null);
                        let method = msg
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);

                        let stdin = stdin.clone();
                        let terminals = terminals.clone();
                        let event_tx = event_tx.clone();
                        let base_cwd = base_cwd.clone();
                        let target = target.clone();

                        tokio::spawn(async move {
                            let response = handle_agent_request(
                                id,
                                &method,
                                params,
                                &base_cwd,
                                &target,
                                auto_approve,
                                terminals,
                                event_tx,
                            )
                            .await;
                            if let Err(err) = write_json_line(&stdin, &response).await {
                                warn!("Failed to send ACP response: {err}");
                            }
                        });
                    }
                }

                // Process stdout closed: fail any pending requests and report exit
                let mut map = pending.lock().await;
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err(anyhow!("ACP agent process exited unexpectedly")));
                }
                let exit_code = if let Some(mut c) = child_arc.lock().await.take() {
                    c.wait().await.ok().and_then(|s| s.code())
                } else {
                    None
                };
                let _ = event_tx.send(AcpEvent::ProcessExited(exit_code));
            });
        }

        let mut client = Self {
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            child: child_arc,
            target: config.target.clone(),
            init_result: InitializeResult {
                protocol_version: json!(1),
                agent_capabilities: Value::Null,
                agent_info: None,
                auth_methods: vec![],
            },
        };

        let init_val = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {
                            "readTextFile": true,
                            "writeTextFile": true
                        },
                        "terminal": true
                    },
                    "clientInfo": {
                        "name": "same",
                        "title": "SAME",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await
            .context("ACP initialize failed")?;

        client.init_result = serde_json::from_value(init_val).unwrap_or(InitializeResult {
            protocol_version: json!(1),
            agent_capabilities: Value::Null,
            agent_info: None,
            auth_methods: vec![],
        });

        Ok((client, event_rx))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: json!(id),
            method: method.to_string(),
            params: Some(params),
        };
        if let Err(err) = write_json_line(&self.stdin, &req).await {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        rx.await
            .map_err(|_| anyhow!("ACP request '{method}' cancelled before response"))?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: Some(params),
        };
        write_json_line(&self.stdin, &notif).await
    }

    pub async fn new_session(&self, cwd: &Path) -> Result<NewSessionResult> {
        let val = self
            .request(
                "session/new",
                json!({
                    "cwd": self.target.to_target_path(cwd),
                    "mcpServers": []
                }),
            )
            .await?;
        let mut res: NewSessionResult =
            serde_json::from_value(val.clone()).context("Invalid session/new response")?;
        res.model_state = parse_model_state(&val);
        Ok(res)
    }

    /// Resumes a session; returns its model choice if the agent reports one.
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: &Path,
    ) -> Result<Option<AcpModelState>> {
        let val = self
            .request(
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": self.target.to_target_path(cwd),
                    "mcpServers": []
                }),
            )
            .await?;
        Ok(parse_model_state(&val))
    }

    /// Switches the session's model. Uses `session/set_config_option` when the
    /// agent exposes the model as a config option, else `session/set_model`.
    /// Returns the updated model state when the agent sends one back.
    pub async fn set_model(
        &self,
        session_id: &str,
        state: Option<&AcpModelState>,
        model_id: &str,
    ) -> Result<Option<AcpModelState>> {
        if let Some(config_id) = state.and_then(|s| s.config_id.as_deref()) {
            let val = self
                .request(
                    "session/set_config_option",
                    json!({
                        "sessionId": session_id,
                        "configId": config_id,
                        "value": model_id
                    }),
                )
                .await?;
            return Ok(parse_model_state(&val));
        }
        let val = self
            .request(
                "session/set_model",
                json!({
                    "sessionId": session_id,
                    "modelId": model_id
                }),
            )
            .await?;
        Ok(parse_model_state(&val))
    }

    /// Sets a session config option (e.g. the reasoning effort). Returns the
    /// updated model state when the agent sends one back.
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<Option<AcpModelState>> {
        let val = self
            .request(
                "session/set_config_option",
                json!({
                    "sessionId": session_id,
                    "configId": config_id,
                    "value": value
                }),
            )
            .await?;
        Ok(parse_model_state(&val))
    }

    pub async fn prompt(&self, session_id: &str, text: &str) -> Result<PromptResult> {
        let val = self
            .request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [
                        {
                            "type": "text",
                            "text": text
                        }
                    ]
                }),
            )
            .await?;
        Ok(serde_json::from_value(val).unwrap_or(PromptResult { stop_reason: None }))
    }

    pub async fn cancel(&self, session_id: &str) -> Result<()> {
        self.notify(
            "session/cancel",
            json!({
                "sessionId": session_id
            }),
        )
        .await
    }

    pub async fn shutdown(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }
    }
}

async fn write_json_line<T: serde::Serialize>(
    stdin: &Arc<Mutex<ChildStdin>>,
    payload: &T,
) -> Result<()> {
    let mut line = serde_json::to_string(payload)?;
    line.push('\n');
    let mut guard = stdin.lock().await;
    guard.write_all(line.as_bytes()).await?;
    guard.flush().await?;
    Ok(())
}

async fn handle_agent_request(
    id: Value,
    method: &str,
    params: Value,
    base_cwd: &Path,
    target: &ExecTarget,
    auto_approve: bool,
    terminals: Arc<Mutex<HashMap<String, ManagedTerminal>>>,
    event_tx: mpsc::UnboundedSender<AcpEvent>,
) -> JsonRpcResponse {
    let result = match method {
        "session/request_permission" => {
            handle_permission_request(params, auto_approve, event_tx).await
        }
        "fs/read_text_file" => handle_read_text_file(params, base_cwd, target).await,
        "fs/write_text_file" => handle_write_text_file(params, base_cwd, target).await,
        "terminal/create" => handle_terminal_create(params, base_cwd, target, terminals).await,
        "terminal/output" => handle_terminal_output(params, terminals).await,
        "terminal/wait_for_exit" => handle_terminal_wait(params, terminals).await,
        "terminal/kill" => handle_terminal_kill(params, terminals).await,
        "terminal/release" => handle_terminal_release(params, terminals).await,
        _ => Err((-32601, format!("Method not supported: {method}"))),
    };

    match result {
        Ok(res) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(res),
            error: None,
        },
        Err((code, message)) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
        },
    }
}

async fn handle_permission_request(
    params: Value,
    auto_approve: bool,
    event_tx: mpsc::UnboundedSender<AcpEvent>,
) -> std::result::Result<Value, (i64, String)> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let options: Vec<AcpPermissionOption> = params
        .get("options")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    if auto_approve {
        if let Some(opt) = options
            .iter()
            .find(|o| o.kind.contains("allow"))
            .or_else(|| options.first())
        {
            return Ok(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": opt.option_id
                }
            }));
        }
    }

    let tool_call = params.get("toolCall");
    let tool_call_id = tool_call
        .and_then(|t| t.get("toolCallId"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let title = tool_call
        .and_then(|t| t.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("Permission requested")
        .to_string();
    let description = tool_call
        .and_then(|t| t.get("rawInput"))
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        .unwrap_or_default();

    let (tx, rx) = oneshot::channel();
    let _ = event_tx.send(AcpEvent::PermissionRequest {
        request_id: Uuid::new_v4().to_string(),
        session_id,
        tool_call_id,
        title,
        description,
        options,
        responder: PermissionResponder { tx },
    });

    match rx.await {
        Ok(PermissionDecision::Selected { option_id }) => Ok(json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id
            }
        })),
        _ => Ok(json!({
            "outcome": {
                "outcome": "cancelled"
            }
        })),
    }
}

async fn handle_read_text_file(
    params: Value,
    base_cwd: &Path,
    target: &ExecTarget,
) -> std::result::Result<Value, (i64, String)> {
    let raw_path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing path".to_string()))?;
    let path = target.to_host_path(base_cwd, raw_path);
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| (-32000, format!("Failed to read {}: {e}", path.display())))?;

    let line_start = params.get("line").and_then(Value::as_u64).unwrap_or(1) as usize;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize);

    let sliced = if line_start > 1 || limit.is_some() {
        let lines: Vec<&str> = content.lines().collect();
        let start = line_start.saturating_sub(1).min(lines.len());
        let end = match limit {
            Some(l) => (start + l).min(lines.len()),
            None => lines.len(),
        };
        lines[start..end].join("\n")
    } else {
        content
    };

    Ok(json!({ "content": sliced }))
}

async fn handle_write_text_file(
    params: Value,
    base_cwd: &Path,
    target: &ExecTarget,
) -> std::result::Result<Value, (i64, String)> {
    let raw_path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing path".to_string()))?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing content".to_string()))?;
    let path = target.to_host_path(base_cwd, raw_path);
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| (-32000, format!("Failed to write {}: {e}", path.display())))?;
    Ok(json!({}))
}

async fn handle_terminal_create(
    params: Value,
    base_cwd: &Path,
    target: &ExecTarget,
    terminals: Arc<Mutex<HashMap<String, ManagedTerminal>>>,
) -> std::result::Result<Value, (i64, String)> {
    let command = params
        .get("command")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing command".to_string()))?;
    let args: Vec<String> = params
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let raw_cwd = params.get("cwd").and_then(Value::as_str);
    let mut env = BTreeMap::new();
    if let Some(env_arr) = params.get("env").and_then(Value::as_array) {
        for item in env_arr {
            if let (Some(k), Some(v)) = (
                item.get("name").and_then(Value::as_str),
                item.get("value").and_then(Value::as_str),
            ) {
                env.insert(k.to_string(), v.to_string());
            }
        }
    }

    // The agent speaks in its own environment's terms: in WSL the command
    // and cwd are Linux ones and must run there too.
    let mut cmd = match (target, raw_cwd) {
        (ExecTarget::Wsl { distro }, Some(cwd)) if cwd.starts_with('/') => {
            build_wsl_command(distro, command, &args, Some(cwd), &env).await
        }
        _ => {
            let cwd = raw_cwd
                .map(|p| target.to_host_path(base_cwd, p))
                .unwrap_or_else(|| base_cwd.to_path_buf());
            build_command(target, command, &args, &cwd, &env).await
        }
    };
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| (-32000, format!("Failed to spawn terminal command: {e}")))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let output = Arc::new(Mutex::new(String::new()));
    let exit_code = Arc::new(Mutex::new(None));
    let child_arc = Arc::new(Mutex::new(Some(child)));

    if let Some(out) = stdout {
        let output = output.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let mut buf = output.lock().await;
                buf.push_str(&line);
                buf.push('\n');
            }
        });
    }
    if let Some(err) = stderr {
        let output = output.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(err).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let mut buf = output.lock().await;
                buf.push_str(&line);
                buf.push('\n');
            }
        });
    }

    let term_id = Uuid::new_v4().to_string();
    terminals.lock().await.insert(
        term_id.clone(),
        ManagedTerminal {
            output,
            exit_code,
            child: child_arc,
        },
    );

    Ok(json!({ "terminalId": term_id }))
}

async fn handle_terminal_output(
    params: Value,
    terminals: Arc<Mutex<HashMap<String, ManagedTerminal>>>,
) -> std::result::Result<Value, (i64, String)> {
    let term_id = params
        .get("terminalId")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing terminalId".to_string()))?;
    let map = terminals.lock().await;
    let term = map
        .get(term_id)
        .ok_or((-32000, "Unknown terminalId".to_string()))?;
    let out = term.output.lock().await.clone();
    let code = *term.exit_code.lock().await;
    let exit_status = code.map(|c| json!({ "exitCode": c }));
    Ok(json!({
        "output": out,
        "truncated": false,
        "exitStatus": exit_status
    }))
}

async fn handle_terminal_wait(
    params: Value,
    terminals: Arc<Mutex<HashMap<String, ManagedTerminal>>>,
) -> std::result::Result<Value, (i64, String)> {
    let term_id = params
        .get("terminalId")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing terminalId".to_string()))?;
    let (child_arc, exit_code_arc) = {
        let map = terminals.lock().await;
        let term = map
            .get(term_id)
            .ok_or((-32000, "Unknown terminalId".to_string()))?;
        (term.child.clone(), term.exit_code.clone())
    };
    let mut guard = child_arc.lock().await;
    let code = if let Some(child) = guard.as_mut() {
        child.wait().await.ok().and_then(|s| s.code()).unwrap_or(0)
    } else {
        exit_code_arc.lock().await.unwrap_or(0)
    };
    *exit_code_arc.lock().await = Some(code);
    Ok(json!({ "exitCode": code }))
}

async fn handle_terminal_kill(
    params: Value,
    terminals: Arc<Mutex<HashMap<String, ManagedTerminal>>>,
) -> std::result::Result<Value, (i64, String)> {
    let term_id = params
        .get("terminalId")
        .and_then(Value::as_str)
        .ok_or((-32602, "Missing terminalId".to_string()))?;
    let child_arc = {
        let map = terminals.lock().await;
        map.get(term_id).map(|t| t.child.clone())
    };
    if let Some(c) = child_arc {
        if let Some(mut child) = c.lock().await.take() {
            let _ = child.kill().await;
        }
    }
    Ok(json!({}))
}

async fn handle_terminal_release(
    params: Value,
    terminals: Arc<Mutex<HashMap<String, ManagedTerminal>>>,
) -> std::result::Result<Value, (i64, String)> {
    if let Some(term_id) = params.get("terminalId").and_then(Value::as_str) {
        terminals.lock().await.remove(term_id);
    }
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_acp_handshake_and_streaming() -> Result<()> {
        // Minimal mock ACP server written in inline python3 / sh-free? Let's use python3 or node.
        let script = r#"
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  const msg = JSON.parse(line);
  if (msg.method === 'initialize') {
    console.log(JSON.stringify({
      jsonrpc: '2.0',
      id: msg.id,
      result: { protocolVersion: 1, agentInfo: { name: 'mock-acp', version: '0.1.0' } }
    }));
  } else if (msg.method === 'session/new') {
    console.log(JSON.stringify({
      jsonrpc: '2.0',
      id: msg.id,
      result: {
        sessionId: 'sess-123',
        models: {
          currentModelId: 'm1',
          availableModels: [{ modelId: 'm1', name: 'Model 1' }, { modelId: 'm2', name: 'Model 2' }]
        }
      }
    }));
  } else if (msg.method === 'session/set_model') {
    console.log(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result: {} }));
  } else if (msg.method === 'session/prompt') {
    console.log(JSON.stringify({
      jsonrpc: '2.0',
      method: 'session/update',
      params: {
        sessionId: 'sess-123',
        update: { sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: 'thinking...' } }
      }
    }));
    console.log(JSON.stringify({
      jsonrpc: '2.0',
      method: 'session/update',
      params: {
        sessionId: 'sess-123',
        update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Hello from mock ACP!' } }
      }
    }));
    console.log(JSON.stringify({
      jsonrpc: '2.0',
      id: msg.id,
      result: { stopReason: 'end_turn' }
    }));
  }
});
"#;
        let (client, mut rx) = AcpClient::spawn(AcpClientConfig {
            command: "node".to_string(),
            args: vec!["-e".to_string(), script.to_string()],
            env: BTreeMap::new(),
            cwd: std::env::temp_dir(),
            auto_approve: true,
            target: ExecTarget::Host,
        })
        .await?;

        assert_eq!(
            client
                .init_result
                .agent_info
                .as_ref()
                .map(|i| i.name.as_str()),
            Some("mock-acp")
        );

        let sess = client.new_session(&std::env::temp_dir()).await?;
        assert_eq!(sess.session_id, "sess-123");
        let state = sess.model_state.clone().expect("models parsed");
        assert_eq!(state.current.as_deref(), Some("m1"));
        assert_eq!(state.models.len(), 2);
        client
            .set_model(&sess.session_id, Some(&state), "m2")
            .await?;

        let res = client.prompt(&sess.session_id, "Hi").await?;
        assert_eq!(res.stop_reason.as_deref(), Some("end_turn"));

        let mut saw_thought = false;
        let mut saw_text = false;
        while let Ok(ev) = rx.try_recv() {
            if let AcpEvent::SessionUpdate { update, .. } = ev {
                match update {
                    SessionUpdate::AgentThoughtChunk { text } if text == "thinking..." => {
                        saw_thought = true;
                    }
                    SessionUpdate::AgentMessageChunk { text } if text == "Hello from mock ACP!" => {
                        saw_text = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_thought);
        assert!(saw_text);
        client.shutdown().await;
        Ok(())
    }
}
