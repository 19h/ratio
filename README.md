# Ratio

Ratio is a local coding-agent harness with terminal and web interfaces. Give it a goal, an OpenAI-compatible endpoint, and a workspace. The lead reviewer plans the work, workers use native and stdio MCP tools, and the reviewer records a typed decision. Ratio can coordinate a team, run independent tasks in parallel, and verify a staged candidate before applying it to the live worktree.

## How a run works

```text
  Request from the CLI, terminal setup, or web dashboard
                                      |
                                      v
               Propose a team, use a configured roster, or run solo
                                      |
                                      v
                       +-----------------------------+
                       | Lead reviewer drafts brief  |<-- Planning panel
                       +--------------+--------------+
                                      |
                    +-----------------+------------------+
                    |                                    |
               Solo worker                      Worker roster selected
                    |                                    |
                    |                  +-----------------v----------------+
                    |             +--->| A. Coordinate workers            |
                    |             |    +-----------------+----------------+
                    |             |                      |
                    |             |                      v
                    |             |    +----------------------------------+
                    |             |    | Freeze assignments + write paths |
                    |             |    | Include task DAG when required   |
                    |             |    +-----------------+----------------+
                    |             |                      |
                    |             |                      v
                    |             +<-- objection --- ACK exact plan ID
                    |                                    |
                    |                                unanimous
                    |                                    |
                    +-----------------+------------------+
                                      v
                       +-----------------------------+
                       | B. Dispatch worker tools    |<--> Stakeholder Q&A
                       | Run ready tasks in parallel |<--> Shared run memory
                       +--------------+--------------+
                                      |
                         reports, changes, evidence
                                      |
                                      v
                      [Staged mode: verify candidate]
                                      |
                                      v
                       +-----------------------------+
                       | Lead reviewer: submit       |<-- Review panel
                       | typed verdict               |
                       +--------------+--------------+
                                      |
          +---------------------------+----------------------------+
          |                           |                            |
       approve                     reject                       revise
          |                           |                            |
  staged: publish candidate        Rejected          budget remaining?
  direct: finish in place                              yes: team A / solo B
          |                                             no: Exhausted
       Approved
```

Planning and review panels are optional. The write gate applies when a worker roster is configured. Coordination and acknowledgement expose workspace inspection tools. After the gate opens, workers can create and edit files or run commands. The digest binds the exact plan, roster, cycle, attempt, write ownership, and task graph when present. An objection invalidates the plan and uses another planning attempt. Run budgets can stop execution independently of the verdict.

```text
  Browser <-- loopback HTTP / SSE --> ra --web ------ start / resume / fork ---+
                                        |                                      |
  Attached TUI / control CLI -----------+                                      |
                                        |                                      |
                                 private local control                         |
                                        |                                      |
                                        v                                      v
  Inline TUI / headless CLI ------> +--------------------------------------------+
                                    | Active runner / ratio-engine               |
                                    | Scheduler, mailbox, context compaction     |
                                    +------+------------------------------+------+
                                           |                              |
                     Provider SSE / JSON <-+-> Native tools and stdio MCP |
                                           +-> Staging and verification   |
                                                                          |
                                                             checkpoint transactions
                                                                          |
                                                                          v
                                                       +--------------------------------+
                                                       | .ratio/runs.sqlite3 (WAL)      |
                                                       | State, budget, IPC, tool audit |
                                                       +---------------+----------------+
                                                                       |
                                                                   history reads
                                                                       v
                                             Web dashboard / attached TUI / CLI exports
```

[`ra`](crates/ratio) owns the CLI, terminal UI, browser dashboard, and process control. [`ratio-engine`](crates/ratio-engine) owns execution and persistence. [`ratio-protocol`](crates/ratio-protocol) defines the task graph, run events, and control messages. The browser dashboard and attached TUI read the journal and send live controls through the active runner's private endpoint.

## Build and run

Build with Rust 1.85 or later. Git is required for staged execution and Git inspection tools.

```sh
cargo install --locked --path crates/ratio

export RATIO_API_KEY='<provider key>'
ra -C /path/to/project \
  --endpoint https://provider.example/v1 \
  --model provider-model \
  --context-window 32768 \
  --goal 'Fix the parser regression and verify all supported inputs.'
```

For repeat use, save your provider settings in `$XDG_CONFIG_HOME/ratio/config.toml` or `~/.config/ratio/config.toml`. On Windows, use `%APPDATA%\ratio\config.toml`. An explicit `--config` selects a different file. With a provider profile and no configured worker roster, Ratio proposes workers, reviewers, stakeholders, and acceptance criteria. Use `--web` to enter a request in a browser, or supply it directly through the CLI.

```sh
ra -C PROJECT 'Implement the solver and verify convergence'
ra -C PROJECT --review-plan 'Implement the solver and verify convergence'
ra -C PROJECT --plan-only 'Implement the solver and verify convergence'
cat request.txt | ra -C PROJECT --headless --goal-stdin
```

In a terminal, start `ra` to enter a request. Use `--review-plan` to inspect and refine the proposal before starting with **Run proposal** or Ctrl+R. `--plan-only` prints the proposed team as JSON and exits before implementation. For CI or scripts, `--headless` writes progress to stderr and the final assessment to stdout. An explicit worker roster takes precedence. Use `--fixed-team` to select the built-in single worker when the profile has no worker roster.

Use [`ratio.example.toml`](ratio.example.toml) as a starting point and pass `--config path/to/ratio.toml`. The [stakeholder example](examples/fullstack-stakeholders.toml) configures separate application and UX perspectives. The [estuary simulation example](examples/morph-dsv41flash-abliterated.toml) uses MorphLLM and a staged three-worker DAG. The numerical kernels and parallel solver can run concurrently. The integration and verification worker depends on both. Run it in an empty Git workspace with the credential available in `RATIO_API_KEY`.

```sh
workspace="$(mktemp -d)"
git -C "$workspace" init
ra --headless --config examples/morph-dsv41flash-abliterated.toml -C "$workspace"
```

## Provider configuration

| Setting | CLI | Environment | TOML |
| --- | --- | --- | --- |
| Endpoint | `--endpoint`, `--base-url` | `RATIO_ENDPOINT`, `OPENAI_BASE_URL` | `endpoint` |
| Credential | `--api-key` | `RATIO_API_KEY`, `OPENAI_API_KEY` | `api_key` |
| Default model | `--model` | `RATIO_MODEL` | `model` |
| Role models | `--worker-model`, `--reviewer-model` | | `[worker].model`, `[reviewer].model` |
| Context window | `--context-window` | `RATIO_CONTEXT_WINDOW` | `context_window` |
| Output cap | `--output-tokens` | | `output_tokens` |
| Transport | `--provider-api chat-completions\|responses` | | `[provider].api` |
| Streaming | `--stream true\|false` | | `[provider].stream`, `[provider].stream_usage` |
| Sampling | `--temperature`, `--top-p`, `--seed` | | `[provider].temperature`, `top_p`, `seed` |
| Provider tool batching | `--parallel-tool-calls true\|false` | | `[provider].parallel_tool_calls` |
| Extra HTTP headers | | | `[provider.headers]` |

Explicit flags take precedence over `RATIO_*` variables, then TOML, then `OPENAI_*` fallbacks where applicable. Set the context window to a token budget supported by every model in the run. Role-specific and per-agent model settings can override the shared model.

Chat Completions is the default. Pass the base URL or full method URL for the selected transport. Choose Responses with `--provider-api responses` or `[provider].api = "responses"`. Both transports support function tools and SSE, with complete JSON accepted as a fallback. Ratio waits for a complete model reply before dispatching any tool. Responses uses `store: false` and carries returned reasoning items alongside the corresponding tool results.

For Chat Completions gateways that reject `stream_options`, set `[provider].stream_usage = false`. Use `--stream false` for JSON-only endpoints. `--legacy-max-tokens` selects `max_tokens` for older Chat Completions gateways. `seed` is a Chat Completions setting whose effect depends on the provider. Provider tool batching controls calls within one completion. Ratio's agent concurrency is configured separately.

Ratio sends a Bearer header when a key is configured. A custom `Authorization` header takes precedence. Standard credential header values and custom header values of at least eight UTF-8 bytes are masked in provider errors and live previews.

## Teams, communication, and review

- **Panel.** Configure up to 64 combined `[[reviewers]]` and `[[stakeholders]]`, with names, personas, planning or review phases, and optional model overrides. The lead reviewer synthesizes their reports. `[orchestration].max_parallel_agents` defaults to 8. Set `parallel_stakeholders = false` for serial consultation.
- **Workers.** Add `[[workers]]` with names, personas, and optional model overrides. With multiple workers, each must send and receive a peer coordination message. The lead reviewer freezes the plan with `submit_plan`, including workspace-relative write ownership. Every worker calls `ack_plan` with the same plan ID or uses `object_to_plan` to return the team to coordination. The default is three plan attempts per review cycle.
- **Dependencies.** Generated teams use a typed task graph. To require it for a configured team, set `[orchestration].require_task_graph = true`. Each worker gets one task with an ID, description, predecessors, and acceptance criteria. Ratio validates references and acyclicity. Workers become runnable when their predecessors complete.
- **Decision.** The lead reviewer issues `submit_verdict` as the sole tool call in its completion. The call carries a verdict and assessment. A `needs_revision` verdict also carries revision instructions.

Plan repair identifies missing, duplicate, and unknown worker IDs. If bounded full-plan repairs fail, Ratio asks for one typed assignment per worker and validates the assembled plan before requesting acknowledgements.

Agents use `send_message` with a recipient ID and a BWP body. Ratio validates syntax and bounds the payload before enqueueing it. BWP keeps exact tool names, paths, and IDs. Flat lists use spaces, clauses use `;`, and multi-word options use `|`. Braces, code fences, and prose parentheses are refused. Function names may use `fn()` notation.

```text
  Worker 1 -- BWP question --> Mailbox --> Stakeholder
                                 |
  Stakeholder -- BWP reply ----->+
                                 +--> Worker 1 (requester)
                                 +--> Worker 2
                                 +--> Worker 3

  Example reply body
  boundary: west tide east wall; tide missing => REFUSED
```

A stakeholder addresses the reply to the requester. Ratio broadcasts it to all workers. Workers who already submitted a report can resume when new mail arrives. Delivery occurs at a request boundary, when the message enters the recipient's model context.

`remember` stores author-attributed, run-scoped notes. An optional `evidence` list references up to eight tool results from the current context. Ratio checks the references and hashes the results. `recall` searches or pages through the shared notes. The checkpoint preserves up to 128 notes along with queued and delivered IPC.

## Workspace and publication

The reviewer and stakeholders can list directories, read workspace files, and inspect Git changes. Native file tools confine paths to the workspace. After the team gate, native writes also check frozen path ownership.

| Operation | Behavior |
| --- | --- |
| `read_file` | Returns UTF-8 lines as `number tag\|text`. Supports inclusive 1-based line ranges or byte offsets. |
| `edit_file` | Applies one batch against a single snapshot. Use `tag` for a line, `tag..tag` for an inclusive range, `+tag` to insert before, or `tag+` to insert after. Range edits require the latest `snapshot_id`. |
| `create_file` | Creates a file only if its destination is absent. |
| `shell` | Starts in the active workspace, with command timeouts and bounded output. Runs through `/bin/sh` on Unix or `cmd.exe` on Windows, with descendant process cleanup. |

Tagged reads and edits support files up to 4 MiB. Ratio uses Assist-style addressing with path-seeded 64-bit tags. Repeated lines carry occurrence information. A stale tag or overlap rejects the batch. Optional `line_hints` help locate nearby current lines when an anchor goes stale. Successful edits preserve file mode and return fresh tags. A unique substring replacement is also available via `old_text` and `new_text`.

By default, workers operate directly on the live worktree. To defer publication until review, enable staging at the root of a Git worktree.

```toml
[orchestration]
stage_workspace = true
verification_commands = ["cargo test --locked --workspace"]
```

```text
  Live Git worktree
          |
          +-- snapshot tracked and unignored files
          |
          v
      Candidate ------ copy when task is ready ------> Private worker tree
          ^                                                    |
          +-------------- ownership-checked delta -------------+
          |
          +-- copy for verification --> Commands + manifest check
          |                                         |
          |                                  SHA-256 receipt
          |                                         |
          |                                  typed reviewer approval
          |                                         |
          +-------------- current digest ---------->+
                                                    v
                                    Match digest, reconcile with baseline
                                                    |
                                                    v
                                      Apply delta to live Git worktree
```

Staging snapshots tracked edits and unignored untracked files. Worker deltas are merged in worker order within each completion wave. Verification runs in a separate copy and must pass against the candidate manifest for publication. Publication checks affected files against the baseline, leaves Git history and the index intact, and preserves unrelated local edits. Use regular files and UTF-8 paths, and ignore generated artifacts. File replacement is individually atomic where supported. An external process can still race the final check.

Shell commands and local MCP subprocesses inherit the operator's host permissions. Staged ownership checks cover changes in the staged tree. Shell can act outside that tree or bypass native path ownership in a direct run. Keep the workspace, commands, and local Git configuration within your trust boundary.

## Interfaces and run history

### Web dashboard

```sh
ra -C PROJECT --web
ra -C PROJECT --config ratio.toml --web --web-port 7350
```

Open the private URL printed by Ratio. The dashboard binds to `127.0.0.1` and chooses a free port by default. It uses the provider configuration and CLI overrides supplied at startup. Use **Start run** to enter a request, or select a run from the workspace history. Live controls let you pause, resume, and stop a run. Saved runs can be resumed from their latest checkpoint or forked into a new run. Closing the dashboard leaves its managed runner active.

| View | Use it to |
| --- | --- |
| Overview | Inspect the live roster, task dependencies, ownership, acknowledgements, verification, and verdict |
| Timeline | Search recorded conversations, IPC, tools, phases, and operator actions |
| Agents | Follow each agent's conversation, file operations, tools, and incoming or outgoing messages |
| Communication | Switch between message sequence, agent lanes, and the route matrix. Inspect reply links and each recipient's delivery. |
| Tools & files | Browse MCP and native tools, expand arguments and captured output, and jump to the exact assistant turn |
| Triage | Submit information for automatic routing, retry failed classification, or track a worker request |

The journal preserves tool invocations and receipts independently of conversation compaction. Expand **Full result** for the captured audit payload and **Model-visible result** for what the model received. Payloads can contain source and command output. Per-tool limits apply within the 8 MiB per-payload ceiling, with omission markers for truncated captures. Older journals expose the messages and tool receipts their checkpoints retained.

Automatic triage uses the lead reviewer to classify and route information within the existing run permissions. The inbox shows the decision, recipients, and delivery receipts. Pending items are marked deferred if the run finishes before delivery. **Add worker** requests a separate, read-only onboarding phase followed by unanimous acknowledgement of the amended plan. The worker request links to its onboarding IPC.

To exercise the dashboard with Playwright, build `target/debug/ra`, install Playwright in your test environment, and run `node --test crates/ratio/tests/web_dashboard.mjs`. Set `RATIO_PLAYWRIGHT` to the module path if it is installed outside this workspace. Set `RATIO_WEB_SCREENSHOTS` to an existing directory to save a screenshot of each view during the run.

The dashboard embeds DM Mono and Martian Mono under the SIL Open Font License. The license texts are in [`crates/ratio/src/web/fonts`](crates/ratio/src/web/fonts).

### Terminal UI

**Activity** shows a grid of agent cards above the shared transcript. Each card shows its current generation, running tool, or latest observed result. Active agents appear first. Use Up/Down or the mouse wheel over the grid to see more agents. **Communication** shows the message sequence, route matrix, and inspector. **Workflow** shows acknowledgements, task state, acceptance criteria, compaction, and verification evidence.

| Key | Action |
| --- | --- |
| Enter | Refine a proposal during setup or queue guidance during a run |
| Ctrl+R or click Run proposal | Approve the setup proposal and start |
| Tab, Shift+Tab, or click a tab | Switch between Activity, Communication, and Workflow |
| PgUp / PgDn in Activity | Scroll the shared transcript |
| Ctrl+T in Communication | Switch between Sequence and Routes |
| Ctrl+F in Communication | Filter all traffic, IPC, or handoffs |
| Ctrl+P or click Pause / Resume | Pause or resume model requests. Pausing cancels active generations. |
| Ctrl+N, click Add worker, or type `/add-worker REQUEST` | Request an additional worker during execution |
| Ctrl-C | During inline generation, cancel and pause. While paused or between generations, exit. |
| Esc | Cancel a setup request or detach from an attached run |

In Communication, use arrows to navigate and Ctrl+Up or Ctrl+Down to scroll the inspector. End follows new traffic in Sequence. An IPC delivery marker records context insertion. Handoff rows show briefs, reports, and verdicts. Inline views keep the latest 512 observed exchanges. An attached live run displays generation previews over the private control endpoint. An attached historical run shows recorded IPC and tool observations from the journal.

```sh
ra -C PROJECT --detached --config ratio.toml --goal 'Implement the solver'
ra -C PROJECT --list-runs
ra -C PROJECT --events RUN_ID --after 0 --follow
ra -C PROJECT --attach RUN_ID
ra -C PROJECT --control status
ra -C PROJECT --control guide --guidance 'Keep the reference solver available'
ra -C PROJECT --control add-worker --worker-request 'Add a worker to own profiler fixtures'
ra -C PROJECT --control pause
ra -C PROJECT --control resume
ra -C PROJECT --control cancel
ra -C PROJECT --config ratio.toml --resume --run-id RUN_ID
ra -C PROJECT --config ratio.toml --fork RUN_ID --goal 'Evaluate a second approach'
```

Inline, foreground headless, and detached runs expose execution controls through a private Unix socket or a Windows named pipe restricted to the operator and SYSTEM. The detached launch response provides the combined progress and assessment log path and the run ID when the process is still active. `--events` emits checkpoint records as JSON Lines. `--resume` defaults to the latest run. A fork starts a new conversation and budget from the recorded roster and goal, using the current workspace as its baseline. Supply `--goal` to override the inherited goal.

During execution under a frozen worker plan, Ratio can queue one additional-worker request at a time. It finishes pending tool batches, proposes disjoint write paths and a new assignment, and requires read-only BWP exchanges with an incumbent worker and a stakeholder or temporary adviser. Every worker then acknowledges the amended plan ID before execution resumes. A declined request returns the incumbent team to its existing plan. The Workflow pane records the result.

```text
  Work under plan A
    |
    +-- operator requests worker --> settle pending tool batches
          |
          v
        Propose one assignment with disjoint write paths
          |
          v
        Incumbent mentor <-- BWP --> New worker <-- BWP --> Stakeholder
          |
          v
        Every worker acknowledges plan B
          +-- unanimous --> Work under plan B with the expanded team
          `-- declined ---> Work under plan A with the incumbent team
```

The SQLite WAL journal at `.ratio/runs.sqlite3` stores run state, IPC, tool outcomes, and budgets. An interrupted workspace tool is recorded as having an unknown outcome and requires inspection before relying on its effects. Interrupted IPC, memory, and acknowledgement operations can resume from their checkpoints. Ratio can import legacy format-2 JSON checkpoints and still maintains a format-2 mirror. Keep `.ratio/` private and outside version control because it contains transcripts and workspace data.

## Context, limits, and automation

Ratio reserves output tokens and estimates the input cost of messages and tool definitions. Compaction summarizes earlier exchanges and retains the active task and recent call/result pairing. It targets 90% of available input space, with headroom for required content. Provider usage calibrates the target. An explicit context rejection can tighten the retry ceiling. If a reply reaches the output cap, Ratio discards partial calls and retries with a concision instruction and a smaller compaction target.

| Limit | Default | Override |
| --- | ---: | --- |
| Review cycles | 3 | `--max-cycles` |
| Provider attempts | 512 | `--max-requests` |
| Metered tokens | 8,000,000 | `--max-total-tokens` |
| Active runtime | 3,600 s | `--max-run-secs` |

Set the same limits in `[orchestration]` with `max_review_cycles`, `max_requests`, `max_total_tokens`, and `max_run_secs`. A review limit of zero allows unlimited revisions within the other budgets. Attempts include retries and compaction. Token reservations use estimated input plus requested output and reconcile against usage when reported. Definite rate-limit and context rejections without usage release the token reservation. Ambiguous failures retain it. The active clock stops after an acknowledged pause reaches a request boundary and running tools settle.

| Command | Output |
| --- | --- |
| `ra -C PROJECT --metrics RUN_ID` | Attempts, metered tokens, active milliseconds, tool outcomes, and IPC deliveries |
| `ra -C PROJECT --otlp-trace RUN_ID` | OTLP/HTTP JSON on stdout, derived from checkpoint timestamps and omitting message and tool payloads |
| `ra -C PROJECT --evaluate RUN_ID` | Pass/fail for approval and absence of unknown tool outcomes |

Pass `--evaluation rules.json` to require a verification receipt, check a different final phase, or set ceilings. For example:

```json
{
  "phase": "Approved",
  "require_verified_candidate": true,
  "max_provider_attempts": 200,
  "max_active_ms": 600000
}
```

Exit codes are `0` for approval or successful inspection, `1` for errors, `2` for rejection, `3` for exhaustion, `4` for failed evaluation, and `130` for interruption. `--debug` reports request size and token usage. Rust consumers can read journal records with `ratio_engine::RunReader`.

## Local extensions

```toml
[skills.numerics]
directory = "skills/numerics"
description = "Read before changing numerical kernels or reference fixtures."

[mcp_servers.project_tools]
command = "/absolute/path/to/project-mcp"
args = []
tools = ["locate_symbol", "check_interface"]
protocol = "2025-11-25"
lifetime = "call"
```

A configured skill directory contains `SKILL.md` and its relative references. The directory path is relative to the configuration file. Agents discover its contents through `read_skill`.

Choose one tool policy per stdio MCP server. The policy applies to both discovery and calls.

| Setting | Tools available |
| --- | --- |
| `tools = ["locate_symbol", "check_interface"]` | Only the named tools |
| `allow_all = true` | Every tool on the server |
| `deny_tools = ["replace_interface"]` | Every tool except the named exclusions |

Tool lists accept any number of entries. `ra --check-mcp project_tools` prints the full permitted catalog and verifies that explicitly allowed names exist. For large catalogs, `mcp_list` returns a page of tool names. Pass the returned `next_offset` as `offset` for the next page. Supply `name` to inspect a single tool's schema.

Workers gain access after the write gate. The default `lifetime = "call"` starts a new server process per invocation. Set `lifetime = "worker"` to reuse a server for a worker during execution. A call with an unknown outcome is recorded without automatic replay. Ratio also supports the explicit MCP revision `2026-07-28`.

The [MorphLLM + Assist example](examples/morph-assist.toml) uses `morph-dsv41flash-abliterated` and launches `/Users/int/.idapro/plugins/assist/bin/assist --mcp` over stdio. It sets `allow_all = true` and reuses an Assist process per worker. Open a database in IDA and set `RATIO_API_KEY` to your MorphLLM credential before starting the run. Run these commands from the Ratio checkout.

```sh
ra --config examples/morph-assist.toml --check-mcp assist
ra -C PROJECT --config examples/morph-assist.toml --web
ra -C PROJECT --config examples/morph-assist.toml --fixed-team \
  'Use Assist to identify the open IDA database and summarize its segments and imports.'
```

## Development

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

MIT
