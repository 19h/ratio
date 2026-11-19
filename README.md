# Ratio

Ratio runs a reviewer-driven coding loop in a local workspace. `ratio-engine` owns execution, tool dispatch, compaction and checkpoints. `ratio-protocol` contains the task graph, run events and local control messages. `ra` provides the CLI and terminal UI.

```text
goal -> reviewer plan -> worker tools -> reviewer verdict
                            ^                  |
                            +-- needs revision-+
```

## Run

Build with Rust 1.85 or later.

```sh
cargo build --release

export RATIO_API_KEY='<provider key>'
./target/release/ra -C /path/to/project \
  --endpoint https://provider.example/v1 \
  --model provider-model \
  --context-window 32768 \
  --goal 'Fix the parser regression and verify all supported inputs.'
```

Chat Completions is the default. Pass a base URL or a full `/chat/completions` URL. Use `--provider-api responses` or `[provider].api = "responses"` for a base URL or a full `/responses` URL. Both transports support streaming and function tools. Responses uses `store: false` and carries returned reasoning items with the corresponding tool results. Chat Completions sends `max_completion_tokens` by default and accepts `--legacy-max-tokens` for gateways that require `max_tokens`. Ratio sends a Bearer header when configured. [Chat Completions reference](https://developers.openai.com/api/reference/resources/chat), [Responses reference](https://developers.openai.com/api/reference/resources/responses/methods/create)

| Setting | CLI | Environment | TOML |
| --- | --- | --- | --- |
| API endpoint | `--endpoint` or `--base-url` | `RATIO_ENDPOINT`, `OPENAI_BASE_URL` | `endpoint` |
| API key | `--api-key` | `RATIO_API_KEY`, `OPENAI_API_KEY` | `api_key` |
| Default model | `--model` | `RATIO_MODEL` | `model` |
| Context window in tokens | `--context-window` | `RATIO_CONTEXT_WINDOW` | `context_window` |
| Output limit in tokens | `--output-tokens` | | `output_tokens` |
| Stream responses | `--stream true\|false` | | `[provider].stream` |
| Provider API | `--provider-api chat-completions\|responses` | | `[provider].api` |
| Sampling temperature | `--temperature` | | `[provider].temperature` |
| Nucleus sampling | `--top-p` | | `[provider].top_p` |
| Provider seed | `--seed` | | `[provider].seed` |
| Provider parallel function calls | `--parallel-tool-calls true\|false` | | `[provider].parallel_tool_calls` |
| Worker and reviewer models | `--worker-model`, `--reviewer-model` | | `[worker].model`, `[reviewer].model` |
| Review limit | `--max-cycles` | | `[orchestration].max_review_cycles` |

`RATIO_*` variables override TOML and yield to explicit CLI flags. TOML endpoint and key settings take precedence over `OPENAI_BASE_URL` and `OPENAI_API_KEY`. Role-specific models override the shared model. Set the context window to the smallest supported window across all selected models. A review limit of zero allows unlimited cycles within the run budgets.

Ratio requests server-sent events by default and also accepts complete JSON replies. The stream carries text, indexed function-call fragments, and usage when the provider supplies it. A complete terminal frame can end at clean EOF. Set `[provider].stream_usage = false` for endpoints that stream but reject `stream_options`, or use `--stream false` for JSON-only endpoints. Put custom HTTP headers under `[provider.headers]`, for example `x-api-key = "..."`. An explicit custom `Authorization` header overrides the generated Bearer header. Ratio masks standard credential header values and custom header values of eight UTF-8 bytes or more in provider errors and live previews. Sampling settings are sent only when configured. A seed can improve repeatability when the provider honors it. The `parallel_tool_calls` setting controls provider tool-call batching within one response. Ratio's agent concurrency is configured separately.

The defaults are three review cycles, 512 provider attempts, 8 million metered tokens, and one hour of active runtime. Set `--max-cycles`, `--max-requests`, `--max-total-tokens`, or `--max-run-secs` to override them. TOML uses `max_review_cycles`, `max_requests`, `max_total_tokens`, and `max_run_secs` under `[orchestration]`. Attempts include retries and compaction. Token reservations use estimated input and requested output. Reported usage replaces the estimate. Explicit rate-limit and context rejections without usage release the token reservation. Ambiguous failures, such as timeouts, retain it.

The runtime budget stops once the run reaches a paused request boundary. Tools that are already running still count until they settle.

Use `--config ratio.example.toml` for file settings. The [stakeholder example](examples/fullstack-stakeholders.toml) shows how to add independent planning and review perspectives. Ratio runs consultations in parallel by default and gives their results to the reviewer for synthesis.

## Interactive setup

Run in a terminal with the provider settings. You can omit `--goal` and type the goal in the TUI. If supplied, the goal and configured stakeholders seed the first proposal.

Review the draft goal, reviewer and stakeholder descriptions, and consultation phases in the Proposal pane. Press Enter to send feedback, change the panel, or answer the setup agent's questions. Press F5 to approve the current proposal and start the run. Use Tab to select a pane and Page Up or Page Down to scroll.

During execution, Activity shows a live preview of the selected generation alongside the transcript. Press F7 to switch among active agents. F6 pauses new model requests and cancels active generations. Ctrl-C cancels active generations and pauses when a model request is active, and exits when paused or between generations. Enter queues guidance. Press F6 again to resume from the conversation and apply queued guidance at the next supported steering point. Partial model output is never added to the conversation or executed. During setup, Esc cancels the current draft request and Ctrl-C exits. Use `--resume` to recover the last checkpoint after exiting, including the approved stakeholders. With `--headless`, streaming is consumed internally and stdout contains the final assessment.

Press **F3** during or after a run to open Communication. The sequence diagram uses one lane per participant. An IPC broadcast appears on one row, with a delivery marker for each recipient. Reviewer briefs, reports and verdicts appear as separate handoffs. The inspector shows the selected message, its phase, its sender and its recipients. Press **Tab** to switch to a directed routes matrix.

| Key | Communication view |
| --- | --- |
| Up / Down, PgUp / PgDn | Select messages or matrix sender rows |
| Left / Right | Pan participant columns |
| Home / End | Select the first or last row. End follows new messages in the sequence view. |
| Ctrl+Up / Ctrl+Down | Scroll the inspector |
| F4 | Cycle between all traffic, BWP IPC and handoffs |
| F2 | Return to Activity |

The view retains the latest 512 exchanges observed by the current TUI session. Resuming also shows IPC still queued in the checkpoint. An IPC delivery marker means Ratio added the message to the recipient's context. Handoffs show the context Ratio prepared for the next phase.

Press **F8** for Workflow. It shows plan acknowledgement, task dependencies and status, acceptance criteria, compaction counts and the verification receipt. For a detached or historical run, `--attach RUN_ID` restores recorded IPC and tool effect observations. In an attached live run, F6 controls requests and Enter queues guidance. Esc detaches.

## Tools and constraints

The lead reviewer submits the final decision through `submit_verdict` with `verdict` and `assessment`. A `needs_revision` decision also requires `revision_instructions`. The tool must be the only call in its completion. Ratio renders the CLI verdict from the recorded decision.

The reviewer and stakeholders can list directories, read ranges of UTF-8 workspace files and inspect Git changes. `read_file` returns lines as `number tag|text`. Use `start_line` and `end_line` for inclusive 1-based ranges, or `offset` for the original byte-range mode. Tagged reads and file edits support files up to 4 MiB.

The worker can pass `edit_file.edits` an object mapping tags to replacement text or null. Use `tag` for one line, `tag..tag` for an inclusive range, `+tag` to insert before a line, and `tag+` to insert after. Ratio resolves the whole batch against one snapshot and rejects it if any tag is stale or two edits overlap. After a write it returns fresh tags near the first change. Edits preserve the file mode. Ratio uses the Assist addressing syntax with its own 64-bit path-seeded tags. Repeated lines include an occurrence and count, so changes to their multiplicity invalidate their prior tags. Range operations validate both ends. `old_text` and `new_text` remain available for one unique substring replacement. `create_file` refuses an existing destination.

Use the optional `line_hints` object to associate tags with their previous 1-based line numbers. A stale-tag error can then show current nearby line numbers, text and tags. Ratio also uses surviving range anchors or matching content hashes when available. Extra fields in ordinary tool calls are ignored. Required fields, types, role permissions, exact plan IDs, and BWP syntax still apply. A refused call returns its error to the model so it can correct the input.

The worker can run local shell commands and inherits the current user's filesystem permissions. Tool input starts in the configured workspace. On Unix, commands run with `/bin/sh -c`. On Windows, they run with `cmd.exe /D /S /C`. Ratio applies command timeouts and bounds captured output. Required tools, allowed paths and other TOML constraints guide the worker and reviewer through their prompts.

A range edit requires the current `snapshot_id` from a tagged read. Any intervening byte change invalidates the snapshot. Native writes to the same resolved path serialize within Ratio. External writers can still change a file between the last check and replacement. Tagged reads follow in-workspace symlinks. To edit a symlink target, address the target directly. Shell and local Git configuration are trusted. On Windows, Ratio assigns commands to a kill-on-close Job Object before they begin executing, so timeout and cancellation clean up their descendant processes. [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)

## Parallel review and IPC

Configure any number up to 64 of `[[reviewers]]` and `[[stakeholders]]` combined. Each entry supports a name, persona, phases, and optional model override. Ratio runs the active panel in parallel, bounded by `orchestration.max_parallel_agents` (default 8). Set `parallel_stakeholders = false` to make requests serial. The lead reviewer synthesizes the panel's reports and delivers the final verdict.

Panel members receive explicit peer IDs, such as `reviewer_1` and `stakeholder_1`. They can call `send_message` with `to` and `message`. Message bodies use BWP and enter the recipient's context at a request boundary. New mail can reactivate a member that already submitted an assessment. Ratio commits mail and recipient context alongside panel progress. Resume restores the consultation, coordination or acknowledgement phase in progress.

## Parallel workers

Add `[[workers]]` in TOML with `name`, `persona`, and an optional `model`. In a multi-worker run, every worker first sends and receives a BWP coordination message in a read-only phase. The lead reviewer uses those reports to produce a frozen plan with explicit worker assignments. Each worker must call `ack_plan` with the exact plan ID. The ID binds the plan text, worker roster and revision cycle. The runtime withholds the shell, create, and edit tools until all workers acknowledge the plan, and rejects calls to those tools during coordination and acknowledgement.

The lead reviewer freezes a plan with `submit_plan`, passing the plan text and an `ownership` map keyed by worker ID. Each entry lists workspace-relative paths with a `directory` flag. Ratio enforces those paths for native create and edit tools. Shell retains the operator's permissions. A worker can use `object_to_plan` with the current plan ID and a reason to return the team to coordination. `max_plan_attempts` defaults to three per review cycle.

After unanimous approval, workers run concurrently up to `orchestration.max_parallel_agents`. Ratio commits each tool result as it completes and marks interrupted workspace tools as unknown. A revision starts a fresh coordination and approval cycle.

Set `[orchestration].require_task_graph = true` to require one typed task per worker. Each task has an ID, its owner, a description, predecessor IDs and acceptance criteria. Ratio rejects unknown predecessors, duplicate assignments and cycles. The ACK digest covers the graph. A worker becomes runnable after its predecessors complete. Independent workers share the configured concurrency limit.

During coordination and implementation, a worker can use `send_message` to ask any configured `stakeholder_N` for clarification. The stakeholder wakes on receipt and answers with a BWP `send_message` addressed to the requester. Ratio broadcasts the answer to every worker, including workers who already submitted a report. Stakeholders can inspect the workspace while answering and share the worker concurrency limit. If a reply changes an assignment or shared contract, the workers report it for replanning. The implementation checkpoint preserves queued mail and delivered context for `--resume`.

Agents can use `remember` with a `topic` and `note` to record evidence, decisions, or open questions. The optional `evidence` list names up to eight recorded tool call IDs from the current context. Ratio validates each reference and hashes the result. `recall` returns author-attributed notes and their references to all agents. Use a case-insensitive `query` to search and `before_id` to page through older matches. Ratio saves up to 128 notes per run.

The [MorphLLM estuary flood example](examples/morph-dsv41flash-abliterated.toml) assigns the numerical kernels, parallel solver, and verification and CLI to separate workers. Its review panel covers coastal hydraulics, numerical validation, Rust architecture, flood mapping, and reproducible performance. Run it in a fresh workspace with `RATIO_API_KEY` set.

```sh
workspace="$(mktemp -d)"
git -C "$workspace" init
cargo run -- --headless --config examples/morph-dsv41flash-abliterated.toml -C "$workspace"
```

## Staged execution

Use staged mode from the root of a Git working tree.

```toml
[orchestration]
stage_workspace = true
verification_commands = ["cargo test --locked --workspace"]
```

Ratio snapshots tracked and unignored working files, including tracked edits and untracked files. Each worker starts from a private copy containing completed predecessor changes. Ratio integrates the worker's delta in worker order at each completion wave and checks it against frozen ownership. Verification runs on a separate copy. A passing receipt binds the reviewed file manifest to its SHA-256 digest. The approved delta is applied to the live worktree with a recoverable publication intent. Ratio compares each affected path against the baseline, preserves unrelated edits, and leaves the Git index and refs alone.

This mode requires regular files and UTF-8 paths. Publication detects dirty-path conflicts and preserves the candidate for inspection and resume. File operations are individually atomic where the filesystem supports replacement. A concurrent external writer can still race the final check. Shell and verification commands retain the operator's host permissions. Keep build artifacts ignored so they stay outside the candidate manifest.

## Context and resume

Ratio reserves the configured output budget and estimates the input cost of messages and tool definitions. Compaction aims for 90% of the available input window and can use the remaining headroom for required content. Reported provider usage adjusts the compaction target. An explicit context rejection can lower the input ceiling for that request's retries. Compaction summarizes older tool exchanges, retains the current instructions and protects the newest call and result IDs. An oversized result can be excerpted to fit. Token estimates are provider-independent approximations. Summaries and excerpts can lose details, so a model can use tools to re-check the workspace.

If a phase completion hits the output limit, Ratio discards the partial reply and retries up to twice, asking for a concise completion and shrinking retained input where possible. The configured output cap and run budgets still apply.

The workspace journal is `.ratio/runs.sqlite3` in WAL mode. It commits run state, IPC and effect observations together, and saves the budget ledger there. Use `--resume` for the most recent checkpoint, or add `--run-id RUN_ID` to select a run. Format-2 JSON checkpoints import when resumed in a workspace with no journaled run. The JSON checkpoint is also maintained for format-2 readers. Time between invocations is excluded. An interrupted shell or workspace tool is recorded as an unknown result and stays available for inspection. Unfinished IPC, shared memory and acknowledgement tools can be retried from the saved state.

```text
ra -C PROJECT --detached --config ratio.toml --goal "Implement the solver"
ra -C PROJECT --list-runs
ra -C PROJECT --events RUN_ID --after 0 --follow
ra -C PROJECT --attach RUN_ID
ra -C PROJECT --control guide --guidance "Keep the reference solver available"
ra -C PROJECT --control pause
ra -C PROJECT --control resume
ra -C PROJECT --control cancel
ra -C PROJECT --fork RUN_ID --config ratio.toml --goal "Evaluate a second approach"
```

Detached runs use a private Unix socket or a Windows named pipe restricted to the operator and SYSTEM. The launch response includes the run ID when the process is active and the path to its output log. `--events` streams JSON Lines by checkpoint sequence. Rust consumers can use `ratio_engine::RunReader` and the types in `ratio-protocol`. Forks use the recorded roster and goal with a fresh worktree baseline, budget and conversation. Keep `.ratio/` private and out of version control because it contains transcripts and workspace snapshots.

## Extensions and evaluation

```toml
[skills.numerics]
directory = "skills/numerics"
description = "Read before changing numerical kernels or reference fixtures."

[mcp_servers.project_tools]
command = "/absolute/path/to/project-mcp"
args = []
tools = ["locate_symbol", "check_interface"]
protocol = "2025-11-25"
```

A skill directory contains `SKILL.md` and any relative references. Paths in a skill configuration resolve relative to the configuration file. Agents discover and read these files through `read_skill`. MCP servers are local stdio subprocesses. Workers can list and call only the names the operator allowlists, after the plan gate opens. Ratio also supports explicit MCP revision `2026-07-28`. Each invocation opens its own process, so a server that requires connection-local state across tool calls needs another integration. [MCP stdio transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports), [MCP versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)

`--metrics RUN_ID` reports provider attempts, metered tokens, active milliseconds, tool outcomes and IPC deliveries. `--otlp-trace RUN_ID` writes an OTLP/HTTP JSON trace derived from checkpoint timestamps. It includes phase changes and tool spans and omits prompts, arguments, results and message bodies. Ratio writes the export to stdout. [OTLP JSON encoding](https://opentelemetry.io/docs/specs/otlp/)

`--evaluate RUN_ID` checks for an approved run with no unknown tool outcomes. Pass `--evaluation rules.json` to check a candidate verification receipt, minimum IPC deliveries, a different final phase, or ceilings on provider attempts, metered tokens and active milliseconds. The command returns 4 when a rule fails. For example, a staged run can use:

```json
{
  "phase": "Approved",
  "require_verified_candidate": true,
  "max_provider_attempts": 200,
  "max_active_ms": 600000
}
```

When redirected, Ratio reports progress on stderr and writes the final assessment to stdout. `--headless` suppresses progress. `--debug` displays request sizes and reported token usage.

| Exit code | Outcome |
| --- | --- |
| 0 | Reviewer approved |
| 1 | Runtime or configuration error |
| 2 | Reviewer rejected |
| 3 | Review limit reached |
| 130 | Interrupted |

## License

MIT
