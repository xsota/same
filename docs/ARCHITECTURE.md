# SAME Architecture

SAME is structured as a modular Rust workspace with strict layer separation so the UI never calls provider-specific implementations directly.

## Layers

1. **Presentation Layer (`same-app`)**
   - Native GPU-accelerated desktop GUI built with `GPUI` + `gpui-component`.
   - Dedicated Provider Connection & Management screen (Antigravity, Claude Code, Codex, OpenCode, and Custom ACP agents).
   - Never blocks the GUI thread on agent I/O; communicates with the background Tokio runtime via async channels.
   - Adapts available controls based on each provider's `ProviderCapabilities`.

2. **SAME Core (`same-core`)**
   - Domain entities: `Workspace`, `Project`, `Quest`, `Session`, `Message`, `ToolCallRecord`, `ProviderConfig`, `ProviderSession`, `AgentRun`, `ContextCheckpoint`, `SessionWorkdir`.
   - `ContextManager` (`context.rs`): Provider-agnostic structured context extraction, best-effort secret redaction, internal thinking exclusion, delta synchronization for resumed native sessions, and checkpoint compaction.
   - `worktree` (`worktree.rs`): Git worktree creation, listing, dirty-state protection, merging, and removal (graceful no-op in non-Git projects).
   - `EventBus`: Broadcasts typed `SameEvent` notifications (streaming deltas, thinking chunks, tool calls, permission and user-input requests, provider session bindings, handoff fallbacks, session state changes) to UI and future orchestrators/plugins.
   - `Storage`: Local SQLite persistence (`rusqlite` with bundled SQLite, versioned migrations) and TOML configuration.

3. **Agent Runtime (`same-agent` & `same-acp`)**
   - `same-acp`: Spec-compliant ACP v1 client over stdio (NDJSON JSON-RPC 2.0). Supports `initialize`, `session/new`, `session/load`, `session/prompt`, `session/cancel`, model selection (`session/set_model`, or `session/set_config_option` when the agent exposes the model as a config option), `session/update` notifications, `session/request_permission`, `fs/read_text_file`, `fs/write_text_file`, `terminal/*`, and `spawn_read_only` for read-only sessions.
   - `same-agent`: `AgentManager` and the provider adapters, enforcing per-session duplicate-run guards, optional global concurrency limits, Read Only restrictions, and automatic context-handoff fallback when native session resume fails. ACP agents go through `same-acp`. Claude Code (`claude.rs`) runs `claude -p` with `stream-json` on stdin and stdout and `--permission-prompt-tool stdio`; permission prompts, AskUserQuestion and ExitPlanMode arrive as `can_use_tool` control requests, and `set_permission_mode` / `interrupt` control requests switch the mode and stop a turn. Codex (`codex.rs`) drives `codex app-server` (`thread/start` / `thread/resume`, `turn/start` with the turn's approval policy, sandbox and collaboration mode, `turn/interrupt`) and answers command and file-change approvals and `request_user_input`. Both CLI and ACP pools release processes after 10 idle minutes; the next turn resumes the conversation. `permissions.rs` routes prompts and questions to the UI and applies the session's approval mode; `diff.rs` builds the diffs shown on edit approvals. `discovery.rs` asks each agent for its models and `quota.rs` (the Quota Adapter) for its plan limits (Claude Code `get_usage`, Codex `account/rateLimits/read`), each in a short-lived process without running a turn. `stats.rs` totals token usage per day and model from the CLIs' local history (Claude Code and Codex transcripts, `opencode stats`).

## Local Data Locations

- Config: `$XDG_CONFIG_HOME/same/config.toml` (or OS equivalent)
- Database: `$XDG_DATA_HOME/same/same.db` (or OS equivalent)
