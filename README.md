# Rusta

<p align="center">
  <img src="assets/logo.png" alt="Rusta logo" width="200">
</p>

A lean, native Rust coding agent for the terminal, built for **Kimi**,
**GLM** and **DeepSeek** models, with first-class
[Devscriptor](docs/mcp.md#devscriptor) code intelligence over MCP.

- **Native providers** for Kimi (Moonshot AI), GLM (Z.AI), DeepSeek, and any
  OpenAI-compatible endpoint — each request shaped to what its endpoint
  documents, with live model catalogues, Preserved Thinking and prompt-cache
  awareness.
- **Three front-ends:** headless runs for scripts and CI (with a JSON event
  stream), an interactive REPL, and an optional terminal UI.
- **Agentic tooling:** file, shell, web and media tools; background tasks;
  sub-agents and agent swarms; scheduled prompts; goals; skills; plan mode.
- **MCP client** for stdio and HTTP servers, with OAuth and progressive tool
  disclosure, so large servers don't flood the tool table.
- **Safety by default:** permission modes and rules, guards for dangerous
  commands and secrets, a write guard, and workspace trust for project config.
- **Durable sessions:** JSONL records, resume, fork, export, and automatic
  context compaction.
- A **single self-contained binary** for Linux, macOS and Windows.

## Install

Download the archive for your platform from the
[latest release](https://github.com/vladimirpesic/rusta/releases/latest): Linux
x64 and ARM64 (glibc 2.17 or newer), macOS on Apple Silicon and Intel (macOS 11
or newer), Windows x64. Each has a `.sha256` beside it. The archive holds a
folder, `rusta-<version>-<target>/`, with the one file `rusta` (`rusta.exe` on
Windows) in it; put that on your `PATH`:

```bash
sha256sum -c rusta-1.0.0-x86_64-unknown-linux-gnu.tar.gz.sha256   # macOS: shasum -a 256 -c
tar -xzf rusta-1.0.0-x86_64-unknown-linux-gnu.tar.gz
sudo install rusta-1.0.0-x86_64-unknown-linux-gnu/rusta /usr/local/bin/
```

The macOS binaries are not signed, so the first launch may need
`xattr -d com.apple.quarantine "$(command -v rusta)"`.

Or build from source with Rust 1.88 or newer:

```bash
git clone https://github.com/vladimirpesic/rusta.git rusta && cd rusta
cargo build --release -p rusta-cli      # target/release/rusta
```

Add `--features tui` for the terminal UI. See [Building](docs/building.md) for
portable release builds and the supported platforms.

## Quick start

```bash
rusta init                              # pick a provider, plan and key; verified and saved
rusta                                   # interactive REPL
rusta -p "Explain this repository"      # one headless prompt
```

Or configure the provider from the environment and skip `init`:

```bash
export KIMI_API_KEY=sk-...              # Kimi Coding Plan, the default provider
rusta -p "List the files in this directory" --yes

export ZAI_API_KEY=...                  # GLM / Z.AI
rusta --provider glm

export DEEPSEEK_API_KEY=sk-...          # DeepSeek
rusta --provider deepseek
```

`rusta doctor` checks the setup: provider, endpoint, key, model, MCP servers and
Devscriptor.

## Providers

| `--provider` | `--billing` | Default model | Base URL |
| --- | --- | --- | --- |
| `kimi` | `subscription` *(default)* | `kimi-for-coding` | `https://api.kimi.com/coding/v1` |
| `kimi` | `api` | `kimi-k3.1-pro` | `https://api.moonshot.ai/v1` |
| `glm` (`zai`) | `subscription` *(default)* | `glm-5.3` | `https://api.z.ai/api/coding/paas/v4` |
| `glm` (`zai`) | `api` | `glm-5.3` | `https://api.z.ai/api/paas/v4` |
| `deepseek` | — | `deepseek-flash` | `https://api.deepseek.com` |
| `openai` | — | `gpt-4o-mini` | `https://api.openai.com/v1` |

A Coding Plan key and a pay-per-token key are not interchangeable, so
`--billing` picks the endpoint that accepts yours; `--region china` selects the
mainland deployments. See [Providers](docs/providers.md) for the models, effort
levels, caching and each vendor's specifics.

## Documentation

| Guide | What it covers |
| --- | --- |
| [Usage](docs/usage.md) | Setup, headless runs and `--stream-json`, the REPL and the TUI, slash commands, sessions, subcommands |
| [Providers](docs/providers.md) | Kimi, GLM, DeepSeek and OpenAI-compatible endpoints: models, effort, thinking, caching |
| [Configuration](docs/configuration.md) | Config files and layering, a complete example, environment variables |
| [Permissions and safety](docs/permissions.md) | Permission modes and rules, dangerous commands, guardrails, plan mode, write safety, workspace trust |
| [Tools](docs/tools.md) | The built-in tools, web and media handling, scheduled prompts, output limits |
| [Sub-agents](docs/agents.md) | Profiles, delegation, forks, agent swarms, the model pool, stopped turns |
| [MCP servers](docs/mcp.md) | Configuring servers, on-demand tools, approvals, OAuth, Devscriptor |
| [Hooks](docs/hooks.md) | Running your own commands on session, turn and tool events |
| [Architecture](docs/architecture.md) | The crates, how a turn runs, context management, and the design decisions |
| [Building](docs/building.md) | Building from source, tests, release builds |
| [Devscriptor MCP guide](docs/devscriptor-mcp-guide.md) | Getting the most out of Devscriptor from any MCP-capable agent |

## Project structure

```text
crates/
├── lm-bridge/    # Provider abstraction and the Kimi, GLM, DeepSeek and OpenAI-compatible providers
├── agent-core/   # Agent, turn loop, tools, permissions, context, records
├── mcp-client/   # MCP connections, tool adaptation, OAuth
└── rusta-cli/    # The `rusta` binary
```

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

## License

Rusta is licensed under the [Apache License, Version 2.0](LICENSE).
