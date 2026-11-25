<p align="center">
  <img src="assets/brand/same-mascot.svg" alt="SAME mascot" width="160">
</p>

# SAME (Shared Agentic Multi-provider Environment)

A lightweight, local-first, multi-provider AI agent desktop workspace written in Rust.

## Features (v0.1)

- **ACP First, Not ACP Only**: Built-in Agent Client Protocol (ACP) v1 client over stdio, plus native CLI adapters for agents without a standalone ACP wrapper installed.
- **Multi-Provider Support**:
  - OpenCode (`opencode acp`)
  - Claude Code (`claude-agent-acp` or native `claude` CLI adapter)
  - Codex (`codex-acp` or native `codex` CLI adapter)
  - Antigravity (`agy acp`)
  - Custom ACP agents (configurable command, args, env, cwd)
- **Structured Workspace**: Organize work into `Workspace -> Project -> Quest -> Session` (with quick sessions supported without forcing hierarchy).
- **Streaming & Tool Visibility**: Real-time streaming responses, collapsible thinking/reasoning blocks, tool call cards, interactive permission approval dialogs (with live auto-approve toggle), and file/image attachments.
- **Local-First Storage**: All configuration (TOML) and conversation history (SQLite) are stored locally. Zero telemetry, no mandatory account.
- **Built-in Workspace Tools**: File browser, file viewer/editor, and optional Git status/diff viewer for software development projects.
- **Customizable & Bilingual UI**: Dark/Light themes, 6 accent color presets, configurable UI/monospace fonts and font size, configurable submit key, and Japanese/English UI support.
- **In-App Updates**: Built-in updater that checks GitHub Releases and updates in place (`same --version` / `-V` to print the installed version).

## Windows + WSL

On Windows, agents run where the project lives. A project folder inside WSL (`\\wsl.localhost\<distro>\...`) runs its agents inside that distribution, with your Linux login shell's `PATH`, so the CLIs you installed and logged in to there are used. Other folders use the Windows CLIs. Override it per project from the `⚙` menu in the chat header (Auto / Windows / WSL).

## Keyboard Shortcuts

| Key | Action |
| --- | --- |
| `Enter` / `Shift+Enter` | Send prompt / insert newline (configurable to `Ctrl+Enter` / `Cmd+Enter` in Settings) |
| `Esc` | Stop the running agent (or cancel a rename) |
| `Ctrl+N` | New session |
| `Ctrl+S` | Save the file open in the file panel |
| `Ctrl+L` | Focus the prompt |
| `Ctrl+B` | Show / hide the Files · Git · Logs panel |

## Picking the Agent and Model

The row under the prompt has an agent picker, a model picker, and a reasoning effort picker (when supported by the selected model). Models are discovered dynamically from each provider (ACP `session/new`, Claude Code `initialize`, and Codex `app-server`). You can also type `/model <name>` (or `/model default`) in the prompt; typing `/` lists the commands, and `/model ` lists the models.

Double-click a session (in the sidebar or its title) to rename it. Delete buttons ask for a second click to confirm.

## Architecture

```
crates/
├── same-core/    # Domain models, SQLite storage, TOML config, EventBus
├── same-acp/     # ACP v1 stdio JSON-RPC 2.0 client & protocol types
├── same-agent/   # Provider-agnostic AgentRuntime, Capabilities, Provider adapters
└── same-app/     # Native GPU-accelerated desktop GUI (GPUI + gpui-component)
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for details.

## Quick Start

```bash
# Build and run SAME (optionally pass a project directory to open immediately)
cargo run -p same-app -- /path/to/your/project

# Run tests
cargo test --workspace
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
