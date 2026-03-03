# ratio

Ratio orchestrates two LLM agents through iterative review cycles. A **reviewer** agent formulates precise work instructions and evaluates output. A **worker** agent executes the coding tasks. Both are [opencode](https://opencode.ai) instances communicating over the [Agent Client Protocol (ACP)](https://agentclientprotocol.com) — the same protocol used by Zed's AI features.

The orchestrator enforces user-specified constraints (required tools, forbidden patterns, path restrictions) and runs a structured approve/revise/reject loop until the reviewer is satisfied or the cycle limit is reached.

```
                    ┌─────────────┐
                    │    User     │
                    │  (goal +    │
                    │ constraints)│
                    └──────┬──────┘
                           │
                    ┌──────▼──────┐
                    │   ratio     │
                    │ Orchestrator│
                    └───┬─────┬───┘
               ┌────────▼─┐ ┌─▼────────┐
               │ reviewer │ │ worker   │
               │(opencode)│ │(opencode)│
               └──────────┘ └──────────┘
                  LLM #1       LLM #2
```

## How it works

1. You provide a **goal** (natural-language description of what to build or fix) and optional **constraints** (required tools, forbidden patterns, path restrictions, custom rules).

2. The **reviewer** receives the goal and produces a detailed, actionable **work instruction** for the worker.

3. The **worker** executes the instruction — reading files, editing code, running commands — and produces output.

4. The **reviewer** inspects the worker's output against the original goal and all constraints. It returns a structured verdict:
   - **APPROVED** — work meets all requirements, orchestration ends successfully
   - **NEEDS_REVISION** — specific feedback is sent back to the worker for another cycle
   - **REJECTED** — the approach is fatally flawed and cannot be fixed iteratively

5. Steps 3–4 repeat up to the configured maximum cycles.

Both agents are real LLM instances. The reviewer is not a rule-based checker — it uses LLM reasoning to evaluate quality, correctness, and constraint compliance.

## Installation

### Prerequisites

- **Rust 1.85+** (2024 edition)
- **opencode** — install from [opencode.ai](https://opencode.ai) or via `go install github.com/opencode-ai/opencode@latest`
- An LLM API key configured for opencode (e.g. `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`)

### Build

```sh
git clone <repo-url> ratio
cd ratio
cargo build --release
```

The binary is at `target/release/ratio`.

## Quick start

### Minimal invocation

```sh
ratio --goal "Add comprehensive error handling to src/lib.rs" --cwd /path/to/project
```

### With a config file

```sh
ratio --config ratio.toml
```

### Headless mode (for CI/scripts)

```sh
ratio --config ratio.toml --headless > worker_output.txt
```

In headless mode, worker text streams to stdout. Orchestrator status, reviewer text, and logs go to stderr.

## Configuration

Ratio uses a TOML config file. Copy the example to get started:

```sh
cp ratio.example.toml ratio.toml
```

CLI flags override config file values. Only `goal` is required.

### Complete reference

```toml
# The goal — a detailed natural-language description of what to accomplish.
goal = """
Build a REST API server with user authentication, input validation,
and comprehensive test coverage.
"""

# Working directory for both agents. Defaults to current directory.
# cwd = "/path/to/project"

# ── Agent configuration ────────────────────────────────────────
# Both [worker] and [reviewer] accept the same fields.

[worker]
binary = "opencode"              # Path or command name for opencode
# model = "anthropic/claude-sonnet-4-5"  # LLM model identifier
# agent = "custom-agent-name"    # Custom agent name within opencode
# env = [                        # Extra environment variables
#   { key = "ANTHROPIC_API_KEY", value = "sk-ant-..." },
# ]
# extra_args = []                # Additional CLI arguments forwarded to opencode

[reviewer]
binary = "opencode"
# model = "anthropic/claude-sonnet-4-5"

# ── Orchestration behavior ─────────────────────────────────────

[orchestration]
max_review_cycles = 5            # Maximum review-revise cycles before giving up

# Custom system prompts override the defaults.
# reviewer_system_prompt = "You are a senior Rust engineer..."
# worker_system_prompt = "You are a precise, thorough coding agent..."

# ── Enforced constraints ───────────────────────────────────────
# Injected into prompts for both agents. The worker must follow them;
# the reviewer verifies compliance.

[constraints]
# Tools the worker MUST use.
required_tools = [
    "cargo clippy",
    "cargo test",
]

# Tools the worker must NOT use.
# forbidden_tools = ["rm -rf"]

# Coding approaches the worker must follow.
required_approaches = [
    "Use Result<T, E> for all error handling — no unwrap() or expect()",
    "All public types must derive Debug",
]

# Approaches the worker must avoid.
forbidden_approaches = [
    "unsafe code blocks",
]

# File paths the worker may modify (empty = unrestricted).
# allowed_paths = ["src/"]

# File paths the worker must NOT touch.
forbidden_paths = [
    "Cargo.lock",
]

# Free-form rules expressed as sentences.
custom_rules = [
    "Do not add new dependencies without explicit approval",
    "Preserve existing public API signatures",
]
```

## CLI reference

```
ratio — LLM agent orchestrator

Usage: ratio [OPTIONS]

Options:
  -g, --goal <GOAL>                The goal to accomplish
  -c, --config <FILE>              Path to TOML configuration file
  -C, --cwd <DIR>                  Working directory for both agents
      --worker-model <MODEL>       LLM model for the worker agent
      --reviewer-model <MODEL>     LLM model for the reviewer agent
      --max-cycles <N>             Maximum review cycles
      --headless                   Run without TUI (output to stdout/stderr)
  -V, --version                    Print version
  -h, --help                       Print help
```

**Precedence:** CLI flags > config file > defaults.

## TUI

Ratio includes a terminal interface built with [ratatui](https://ratatui.rs).

### Panes

| Pane | Content |
|---|---|
| **Reviewer** | Thinking tokens (dimmed italic), plan entries with status, reviewer output text |
| **Worker** | Thinking tokens (dimmed italic), plan entries with status, worker output text |
| **Tool Calls** | Tool invocations from both agents with kind badges, parameters, and status |
| **Log** | Orchestrator messages with timestamps and severity |

### Keyboard shortcuts

| Key | Action |
|---|---|
| `Ctrl+K` | Emergency kill — immediately terminates both agents |
| `Ctrl+C` x2 | Double-tap abort (within 800ms) |
| `q` | Quit (when orchestration is finished) |
| `Tab` | Focus next pane |
| `Shift+Tab` | Focus previous pane |
| `j` / `Down` | Scroll down |
| `k` / `Up` | Scroll up |
| `PageDown` / `PageUp` | Scroll by 20 lines |
| `End` | Jump to bottom (re-enables auto-scroll) |
| `Home` | Jump to top (disables auto-scroll) |

### Auto-scroll

Each pane auto-scrolls independently. Scrolling up manually disables auto-scroll for that pane. Press `End` to re-enable it.

### Tool call display

Each tool call entry shows:

- **Timestamp** — when the call was initiated
- **Source** — `W` (worker, cyan) or `R` (reviewer, magenta)
- **Status** — `[...]` in progress, `[ ok]` completed, `[ERR]` failed
- **Kind** — two-character badge: `RD` read, `ED` edit, `DL` delete, `MV` move, `SR` search, `EX` execute, `TH` think, `FT` fetch
- **Title** — human-readable description from the agent
- **Parameters** — color-coded JSON key-value pairs from the tool's `raw_input`

For tools with 3 or fewer parameters, values are shown inline. Larger parameter sets expand onto indented lines below.

## Architecture

### Runtime model

Ratio uses a **single-threaded tokio runtime** (`current_thread` flavor) with a `LocalSet`. This is required because the ACP SDK's `Client` trait uses `#[async_trait(?Send)]` — the futures are `!Send`, so types like `Rc<OrchestratorClient>` can be used instead of `Arc`.

```
tokio::main (current_thread)
└─ LocalSet
   ├─ spawn_local: reviewer ACP I/O loop
   ├─ spawn_local: worker ACP I/O loop
   ├─ spawn_local: reviewer event forwarder
   ├─ spawn_local: worker event forwarder
   └─ orchestrator.run() — drives the review loop
       ↕ mpsc channels ↕
   TUI event loop (select! on terminal input + orchestrator events + timer)
```

### Agent lifecycle

Each agent is spawned as:

```sh
opencode acp --cwd <working_dir> [--model <model>] [--agent <agent>] [extra_args...]
```

Communication is over **stdin/stdout** using newline-delimited JSON-RPC (the ACP wire format). The handshake sequence is:

1. `initialize` — exchange protocol version and client info
2. `new_session` — create a session scoped to the working directory
3. `prompt` — send instructions, receive streaming updates via `session_notification`
4. `cancel` — abort the current turn (on emergency stop)

### Event flow

```
opencode subprocess
    │ ACP session_notification (JSON-RPC)
    ▼
OrchestratorClient (implements acp::Client)
    │ AgentEvent (TextChunk, ThoughtChunk, ToolCallStarted, PlanUpdated, ...)
    ▼
mpsc channel → Orchestrator
    │ OrchestratorEvent (PhaseChanged, WorkerEvent, ReviewerEvent, Log, ...)
    ▼
mpsc channel → TUI App
    │ Updates app state (output text, thinking, plan, tool calls, logs)
    ▼
ratatui render (clamped scroll, styled paragraphs)
```

### ACP session notifications handled

| ACP Update | Mapped To |
|---|---|
| `AgentMessageChunk` | `TextChunk` — streaming output text |
| `AgentThoughtChunk` | `ThoughtChunk` — streaming reasoning/thinking |
| `ToolCall` | `ToolCallStarted` — with kind + raw_input |
| `ToolCallUpdate` | `ToolCallUpdated` — with status + content + raw_output |
| `Plan` | `PlanUpdated` — task list with priorities and status |
| Other variants | `ProtocolMessage` — forwarded as debug info |

## Logging

Tracing output is written to `$TMPDIR/ratio.log` to avoid interfering with the TUI. The log level is controlled by the `RUST_LOG` environment variable (default: `ratio=info`).

```sh
# Watch logs in another terminal
tail -f "$TMPDIR/ratio.log"

# Enable debug-level logging
RUST_LOG=ratio=debug ratio --config ratio.toml
```

## Examples

### Code review and cleanup

```toml
goal = """
Review src/ for code quality issues: fix all clippy warnings, add missing
error handling, ensure consistent naming conventions, and verify all tests pass.
"""

[constraints]
required_tools = ["cargo clippy -- -D warnings", "cargo test"]
forbidden_approaches = ["unsafe code blocks", "unwrap() on Results"]
```

### Feature implementation with guardrails

```toml
goal = """
Implement a WebSocket server in src/ws/ that handles authentication via JWT,
supports pub/sub channels, and gracefully handles disconnections. Include
integration tests for all connection lifecycle events.
"""

[worker]
model = "anthropic/claude-sonnet-4-5"

[reviewer]
model = "anthropic/claude-sonnet-4-5"

[orchestration]
max_review_cycles = 8

[constraints]
required_tools = ["cargo test", "cargo clippy"]
required_approaches = [
    "Use tokio-tungstenite for WebSocket handling",
    "All public types must implement Debug and Clone",
    "Use tracing for structured logging",
]
allowed_paths = ["src/ws/", "tests/"]
forbidden_paths = ["Cargo.lock", "src/main.rs"]
custom_rules = [
    "Do not modify any existing modules outside src/ws/",
    "All new dependencies must be justified in code comments",
]
```

### Headless CI pipeline

```sh
#!/bin/bash
set -euo pipefail

ratio \
  --config ratio.toml \
  --headless \
  --max-cycles 3 \
  --cwd ./my-project \
  2>ratio-stderr.log

echo "Orchestration complete"
```

## License

MIT
