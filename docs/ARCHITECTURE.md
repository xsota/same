# SAME Architecture

SAME is structured as a modular Rust workspace with strict layer separation so the UI never calls provider-specific implementations directly.

## Layers

1. **Presentation Layer (`same-app`)**
   - Native GPU-accelerated desktop GUI built with `GPUI` + `gpui-component`.
   - Dedicated Provider Connection & Management screen (Antigravity, Claude Code, Codex, OpenCode, and Custom ACP agents).
   - Never blocks the GUI thread on agent I/O; communicates with the background Tokio runtime via async channels.
   - Adapts available controls based on each provider's `ProviderCapabilities`.

2. **SAME Core (`same-core`)**
   - Domain entities: `Workspace`, `Project`, `Quest`, `Session`, `Message`, `ToolCallRecord`, `ProviderConfig`.
   - `EventBus`: Broadcasts typed `SameEvent` notifications (streaming deltas, thinking chunks, tool calls, permission requests, session state changes) to UI and future orchestrators/plugins.
   - `Storage`: Local SQLite persistence (`rusqlite` with bundled SQLite) and TOML configuration.

3. **Agent Runtime (`same-agent` & `same-acp`)**
   - `same-acp`: Spec-compliant ACP v1 client over stdio (NDJSON JSON-RPC 2.0). Supports `initialize`, `session/new`, `session/load`, `session/prompt`, `session/cancel`, model selection (`session/set_model`, or `session/set_config_option` when the agent exposes the model as a config option), `session/update` notifications, `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, and `terminal/*`.
   - `same-agent`: Provider-agnostic `AgentRuntime` trait, `AgentManager`, and concrete adapters (`AcpAgentAdapter`, `ClaudeCliAdapter`, `CodexCliAdapter`).

## Local Data Locations

- Config: `$XDG_CONFIG_HOME/same/config.toml` (or OS equivalent)
- Database: `$XDG_DATA_HOME/same/same.db` (or OS equivalent)
