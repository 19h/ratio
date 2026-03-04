# ratio

Ratio orchestrates LLM agents through iterative review cycles. A **reviewer** agent formulates precise work instructions and evaluates output quality. A **worker** agent executes the coding tasks. Optional **stakeholder** agents bring additional perspectives (security, UX, ops, domain expertise) into planning and review phases. All agents are [opencode](https://opencode.ai) instances communicating over the [Agent Client Protocol (ACP)](https://agentclientprotocol.com).

The orchestrator enforces user-specified constraints (required tools, forbidden patterns, path restrictions, custom quality rules) and runs a structured approve/revise/reject loop until the reviewer is satisfied or the cycle limit is reached.

```
                    ┌─────────────┐
                    │    User     │
                    │  (goal +    │
                    │ constraints)│
                    └──────┬──────┘
                           │
                    ┌──────▼─────────┐
                    │      ratio     │
                    │  Orchestrator  │
                    └─┬────┬───────┬─┘
              ┌───────▼┐ ┌─▼────┐ ┌▼─────────────┐
              │reviewer│ │worker│ │ stakeholders │
              │  (LLM) │ │(LLM) │ │   (LLM x N)  │
              └────────┘ └──────┘ └──────────────┘
```

## How it works

### Core loop

1. You provide a **goal** (natural-language description of what to build or fix) and optional **constraints** (required tools, forbidden patterns, path restrictions, quality rules).

2. **Planning** — The reviewer receives the goal, reads the codebase, and produces a detailed, actionable work instruction. If stakeholders participate in the planning phase, they review the draft instruction and provide input from their perspectives. The reviewer synthesizes their feedback into the final instruction.

3. **Working** — The worker executes the instruction: reading files, editing code, running commands.

4. **Reviewing** — The reviewer inspects the worker's output against the original goal, all constraints, and all mandatory quality rules. If stakeholders participate in the review phase, they review the output and draft assessment. The reviewer synthesizes their input into the final verdict:
   - **APPROVED** — work meets all requirements, orchestration ends
   - **NEEDS_REVISION** — specific feedback is sent back to the worker
   - **REJECTED** — the approach is fatally flawed

5. Steps 3-4 repeat until approved, rejected, or the cycle limit is reached.

All agents are real LLM instances. The reviewer is not a rule-based checker — it uses LLM reasoning to evaluate quality, correctness, and constraint compliance.

### Verdict parsing

The orchestrator parses the reviewer's response for a structured `VERDICT:` line. The parser is intentionally conservative:

- Only a standalone `VERDICT: APPROVED` line triggers approval — the word "APPROVED" appearing in prose does not
- Format template echoes (`APPROVED|NEEDS_REVISION|REJECTED`) are ignored
- If no valid verdict line is found, the default is NEEDS_REVISION
- The fallback never produces APPROVED — the reviewer must be unambiguous

### Shared workspace

Both agents (and all stakeholders) share a workspace protocol:

- **`agents.md`** — implementation notes, decisions, progress. Both agents read and update it
- **`.ratio/research/*.md`** — research and analysis files. Agents write findings to named markdown files and reference them in handoffs, preventing duplicate work across cycles
- **Todo list** — shared via the `TodoWrite` tool, visible in the TUI in real time

### Stakeholders

Stakeholders are additional LLM agents that bring specialized perspectives into the orchestration loop without doing implementation work. Each stakeholder:

- Gets its own opencode subprocess with a **clean, unpolluted context**
- Has a unique **persona** that defines what they care about and how they evaluate things
- Participates in **planning**, **review**, or both
- Can use its own **model** (e.g., a cheaper model for simple reviews)
- Writes analysis to `.ratio/research/` files that the team can reference

During planning, stakeholders see the reviewer's draft work instruction and provide input. During review, they see the worker's output and the reviewer's draft assessment. The reviewer always makes the final verdict, but must address stakeholder concerns — unresolved stakeholder concerns block approval.

Stakeholders are defined in the config file as `[[stakeholders]]` entries:

```toml
[[stakeholders]]
name = "Security Auditor"
persona = """
You are a security engineer. Review for auth flaws, injection
vulnerabilities, data exposure, and insecure crypto.
"""
phases = ["planning", "review"]  # or just ["review"]

# Optional: use a different model than the reviewer
[stakeholders.agent]
binary = "opencode"
model = "anthropic/claude-sonnet-4-5"
```

See [`examples/fullstack-stakeholders.toml`](examples/fullstack-stakeholders.toml) for a comprehensive example with three stakeholders (Security Auditor, UX Engineer, SRE).

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

In headless mode, worker text streams to stdout. Reviewer text, stakeholder text, orchestrator status, and logs go to stderr.

### Debug mode

```sh
ratio --config ratio.toml --headless --debug 2>debug.log
```

Logs all ACP protocol messages (`[acp:worker]`, `[acp:reviewer]`) and subprocess stderr (`[stderr:worker]`, `[stderr:reviewer]`, `[stderr:stakeholder]`) to stderr.

### Resume a previous session

```sh
ratio --config ratio.toml --resume
```

Restores session state (cycle count, agent sessions, UI state) from the saved `.ratio-session.json` in the working directory.

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

# Working directory for all agents. Defaults to current directory.
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
max_review_cycles = 5            # Maximum review-revise cycles (default: 5)

# Custom system prompts override the defaults. These shape agent behavior
# across all cycles — use them for project-specific review criteria,
# domain expertise, and quality standards.
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

# Free-form rules expressed as sentences. These are injected as
# MANDATORY QUALITY RULES — the reviewer cannot approve unless
# every single rule is satisfied.
custom_rules = [
    "Do not add new dependencies without explicit approval",
    "Preserve existing public API signatures",
]

# ── Stakeholders (optional) ───────────────────────────────────
# Each [[stakeholders]] entry adds a persona that participates in
# planning and/or review phases.

# [[stakeholders]]
# name = "Security Reviewer"
# persona = "You are a security engineer. Review for auth flaws..."
# phases = ["planning", "review"]    # Default: both
#
# # Override the agent config (binary, model, env). Default: reviewer's config.
# [stakeholders.agent]
# binary = "opencode"
# model = "anthropic/claude-sonnet-4-5"
```

### Stakeholder configuration fields

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes | — | Display name (e.g. "Security Auditor") |
| `persona` | string | yes | — | Who this stakeholder is, what they care about, how they evaluate |
| `phases` | list | no | `["planning", "review"]` | Which phases to participate in |
| `agent` | table | no | reviewer's config | Agent subprocess config (binary, model, env, etc.) |

## CLI reference

```
ratio — LLM agent orchestrator

Usage: ratio [OPTIONS]

Options:
  -g, --goal <GOAL>                The goal to accomplish
  -c, --config <FILE>              Path to TOML configuration file
  -C, --cwd <DIR>                  Working directory for all agents
      --worker-model <MODEL>       LLM model for the worker agent
      --reviewer-model <MODEL>     LLM model for the reviewer agent
      --max-cycles <N>             Maximum review cycles
      --headless                   Run without TUI (output to stdout/stderr)
      --debug                      Log ACP protocol messages to stderr
      --resume                     Resume a previous session from saved state
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
| **Log** | Orchestrator messages, stakeholder output (`[Name] ...`), timestamps, severity |

Stakeholder output is routed to the Log pane with a `[Name]` prefix (e.g. `[Security Auditor] Found SQL injection risk in...`).

### Keyboard shortcuts

| Key | Action |
|---|---|
| `Ctrl+K` | Emergency kill — immediately terminates all agents |
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

### Session persistence

When the session is interrupted (Ctrl+C, Ctrl+K), the TUI state (todos, logs) and session identifiers are saved to `.ratio-session.json` in the working directory. Use `--resume` to restore.

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
   ├─ spawn_local: stakeholder ACP I/O loops (one per stakeholder)
   ├─ spawn_local: stakeholder event forwarders (one per stakeholder)
   ├─ spawn_local: stderr drain tasks (one per subprocess)
   └─ orchestrator.run() — drives the planning/review loop
       ↕ mpsc channels ↕
   TUI event loop (select! on terminal input + orchestrator events + timer)
```

### Agent lifecycle

Each agent (reviewer, worker, stakeholder) is spawned as:

```sh
opencode acp --cwd <working_dir> [--model <model>] [--agent <agent>] [extra_args...]
```

Communication is over **stdin/stdout** using newline-delimited JSON-RPC (the ACP wire format). The handshake sequence is:

1. `initialize` — exchange protocol version and client info
2. `set_session_model` — forward the configured model to opencode
3. `new_session` — create a session scoped to the working directory
4. `prompt` — send instructions, receive streaming updates via `session_notification`
5. `cancel` — abort the current turn (on emergency stop)

Subprocess stderr is drained asynchronously to prevent pipe buffer deadlocks. In `--debug` mode, stderr lines are printed with `[stderr:<role>]` prefixes.

### Orchestration flow

```
             Planning                    Work-Review Loop (cycles)
           ┌─────────┐                 ┌─────────────────────────┐
           │         ▼                 │                         ▼
  goal ──► reviewer ──► stakeholders ──► reviewer synthesis ──► worker
                        (planning)      (final instruction)       │
                                                                  │
           ┌──────────────────────────────────────────────────────┘
           │
           ▼
       reviewer ──► stakeholders ──► reviewer synthesis ──► verdict
       (draft)      (review)         (final verdict)          │
           ▲                                                  │
           │         NEEDS_REVISION                           │
           └──────────────────────────────────────────────────┘
```

### Event flow

```
opencode subprocess
    │ ACP session_notification (JSON-RPC)
    ▼
OrchestratorClient (implements acp::Client)
    │ AgentEvent (TextChunk, ThoughtChunk, ToolCallStarted, PlanUpdated, ...)
    ▼
mpsc channel → Orchestrator
    │ OrchestratorEvent (PhaseChanged, WorkerEvent, ReviewerEvent,
    │                    StakeholderEvent, Log, CycleCompleted, Finished)
    ▼
mpsc channel → TUI App / headless printer
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
| `TodoWrite` | `TodoUpdated` — shared todo list |
| Other variants | `ProtocolMessage` — forwarded as debug info |

### Source layout

```
crates/ratio/src/
├── main.rs          — CLI args, agent spawning, TUI/headless modes
├── config.rs        — Config, AgentConfig, StakeholderConfig, Constraints
├── orchestrator.rs  — Orchestrator state machine, prompt construction,
│                      verdict parsing, stakeholder consultation
├── protocol.rs      — ACP client implementation, AgentEvent types,
│                      WorkerConnection (prompt, cancel, set_model)
├── session.rs       — SessionState, UIState persistence for resume
├── subprocess.rs    — Agent process spawning (opencode acp subprocess)
└── ui/
    ├── app.rs       — App state, event handling, stream management
    ├── events.rs    — TUI event loop (crossterm + orchestrator events)
    ├── render.rs    — ratatui rendering dispatch
    └── widgets.rs   — Pane rendering (reviewer, worker, tools, log)
```

## Logging

Tracing output is written to `$TMPDIR/ratio.log` to avoid interfering with the TUI. The log level is controlled by the `RUST_LOG` environment variable (default: `ratio=info`).

```sh
# Watch logs in another terminal
tail -f "$TMPDIR/ratio.log"

# Enable debug-level logging
RUST_LOG=ratio=debug ratio --config ratio.toml
```

In headless mode with `--debug`, ACP protocol messages and subprocess stderr are printed to stderr in addition to the log file.

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

### Multi-stakeholder review

```toml
goal = "Add a user invitation system with email tokens and role assignment."

[worker]
model = "anthropic/claude-sonnet-4-5"

[reviewer]
model = "anthropic/claude-sonnet-4-5"

[orchestration]
max_review_cycles = 6

[[stakeholders]]
name = "Security Auditor"
persona = """
Review for auth flaws, injection vulnerabilities, insecure token
generation, and data exposure risks. Think like an attacker.
"""
phases = ["planning", "review"]

[[stakeholders]]
name = "SRE"
persona = """
Review for observability (logging, metrics), failure modes, migration
safety, and operational risk. What breaks at 3am?
"""
phases = ["review"]

[constraints]
required_tools = ["cargo test"]
custom_rules = [
    "Invitation tokens must use OsRng, not thread_rng",
    "All database queries must use parameterized statements",
]
```

See [`examples/fullstack-stakeholders.toml`](examples/fullstack-stakeholders.toml) for a fully annotated example with three stakeholders and detailed personas.

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

### Debug a failing session

```sh
# Full protocol trace
ratio --config ratio.toml --headless --debug 2>debug.log

# In debug.log you'll see:
# [acp:worker] send request id=1 method=initialize params={...}
# [acp:worker] recv response id=1 result={...}
# [acp:reviewer] send request id=1 method=prompt params={...}
# [stderr:worker] Loading model anthropic/claude-sonnet-4-5...
# [ratio] [Info] Parsed verdict: NEEDS_REVISION (feedback: 2847 chars)
```

## License

MIT
