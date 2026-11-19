// Stages a scripted run against a local mock provider and saves the README screenshots.
//
//   cargo build -p ra
//   node docs/screenshots/capture.mjs [output directory]
//
// A third argument of `web` or `terminal` captures only that half.
// RATIO_PLAYWRIGHT selects the Playwright module and RATIO_TEST_BINARY the `ra` binary.
// RATIO_DEMO_TRACE=1 logs each scripted reply. The terminal captures also need tmux.
// Nothing here talks to a real provider.
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.RATIO_PLAYWRIGHT || "playwright");
const bin = resolve(process.env.RATIO_TEST_BINARY || "target/debug/ra");
const out = resolve(process.argv[2] || dirname(fileURLToPath(import.meta.url)));

const REQUEST = "Fix the duration parser regression: 1h30m now parses as 31 seconds. Verify every supported input.";

const FILES = {
  "package.json": `{
  "name": "durations",
  "version": "1.4.0",
  "type": "module",
  "scripts": { "test": "node --test" }
}
`,
  "README.md": `# durations

\`parseDuration(text)\` returns milliseconds.

| Input | Meaning |
| --- | --- |
| \`250ms\` | milliseconds |
| \`90s\`, \`5m\`, \`2h\`, \`1d\` | one unit |
| \`1.5h\` | fractional amount |
| \`1h30m\`, \`2h 15m 30s\` | compound, largest unit first |

Anything else throws a \`RangeError\`.
`,
  "CHANGELOG.md": `# Changelog

## Unreleased

## 1.4.0

- Accept fractional amounts such as \`1.5h\`.
- Parse every part of a compound duration in one pass.
`,
  "src/duration.js": `const UNITS = { ms: 1, s: 1_000, m: 60_000, h: 3_600_000, d: 86_400_000 };
const PART = /(\\d+(?:\\.\\d+)?)(ms|s|m|h|d)/g;

/** Parses "250ms", "90s", "1h30m" or "1.5h" into milliseconds. */
export function parseDuration(text) {
  const source = String(text).replace(/\\s+/g, "");
  const parts = [...source.matchAll(PART)];
  if (!parts.length) throw new RangeError(\`Unsupported duration: \${text}\`);
  let total = 0;
  for (const [, amount] of parts) total += Number(amount) * UNITS.s;
  return total;
}
`,
  "test/duration.test.js": `import assert from "node:assert/strict";
import test from "node:test";
import { parseDuration } from "../src/duration.js";

test("parses seconds", () => {
  assert.equal(parseDuration("90s"), 90_000);
  assert.equal(parseDuration(" 5s "), 5_000);
});

test("rejects text without a duration", () => {
  assert.throws(() => parseDuration("soon"), RangeError);
});
`,
};

const FIXTURES = `{
  "accept": {
    "250ms": 250,
    "90s": 90000,
    "5m": 300000,
    "1h30m": 5400000,
    "1.5h": 5400000,
    "2h 15m 30s": 8130000,
    "1d": 86400000,
    " 2h ": 7200000
  },
  "reject": ["", "soon", "5 parsecs", "1h later"]
}
`;

const TABLE = `import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { parseDuration } from "../src/duration.js";

const table = JSON.parse(readFileSync(new URL("./durations.json", import.meta.url)));

for (const [input, expected] of Object.entries(table.accept)) {
  test(\`accepts \${JSON.stringify(input)}\`, () => assert.equal(parseDuration(input), expected));
}
for (const input of table.reject) {
  test(\`rejects \${JSON.stringify(input)}\`, () => assert.throws(() => parseDuration(input), RangeError));
}
`;

const TEAM = {
  goal: "Restore per-unit arithmetic in parseDuration and prove every documented input form with table tests.",
  message: "Three workers, two reviewers and two stakeholders.",
  workers: [
    { name: "Parser engineer", persona: "Own src/duration.js. Fix the unit arithmetic with the smallest change and keep the exported signature." },
    { name: "Fixture author", persona: "Own the test fixtures. Build a table of accepted and rejected inputs from the README and the CLI contract." },
    { name: "Integrator", persona: "Run the whole suite on the combined change, update the changelog and report the evidence." },
  ],
  reviewers: [
    { name: "Correctness reviewer", persona: "Check the grammar, the unit table and numeric edge cases against the tests.", phases: ["planning", "review"] },
    { name: "API reviewer", persona: "Guard the public contract: exports, error types and behaviour callers rely on.", phases: ["planning", "review"] },
  ],
  stakeholders: [
    { name: "CLI maintainer", persona: "Speak for the command-line tool that passes user input to parseDuration.", phases: ["planning", "review"] },
    { name: "Docs owner", persona: "Keep the README and changelog accurate for released behaviour.", phases: ["planning", "review"] },
  ],
  acceptance: [
    "1h30m parses to 5,400,000 ms",
    "Every input form in the README has a table test",
    "node --test passes on the combined change",
  ],
};

const PLAN = {
  plan: [
    "worker_1: Use each part's own unit in parseDuration and reject input the grammar does not fully cover. Keep the signature and the RangeError.",
    "Acceptance: 1h30m => 5400000 | existing tests pass",
    "",
    "worker_2: Add test/durations.json and a table test covering every README form plus rejected input.",
    "Acceptance: one case per documented form | rejected inputs listed",
    "",
    "worker_3: After both land, run the combined suite and record the change in CHANGELOG.md.",
    "Acceptance: node --test passes | changelog names the fix and the stricter input",
  ].join("\n"),
  ownership: {
    worker_1: [{ path: "src/duration.js", directory: false }],
    worker_2: [{ path: "test/durations.json", directory: false }, { path: "test/table.test.js", directory: false }],
    worker_3: [{ path: "CHANGELOG.md", directory: false }],
  },
  tasks: [
    { id: "parser", worker: "worker_1", description: "Fix per-unit arithmetic in parseDuration", depends_on: [],
      acceptance: ["1h30m parses to 5,400,000 ms", "Existing tests pass"] },
    { id: "fixtures", worker: "worker_2", description: "Add table tests for every documented input form", depends_on: [],
      acceptance: ["One case per README form", "Rejected inputs are listed"] },
    { id: "integrate", worker: "worker_3", description: "Run the combined suite and update the changelog", depends_on: ["parser", "fixtures"],
      acceptance: ["node --test passes on the combined change", "Changelog names the fix and the stricter input"] },
  ],
};

const THINKING = "The loop multiplies every part by UNITS.s, so the captured unit never reaches the sum: 1h30m becomes 1 + 30 seconds. " +
  "Switching to UNITS[unit] restores the arithmetic. stakeholder_1 requires input without a unit to be rejected, " +
  "so the guard now compares the matched parts with the whole source instead of only checking that something matched. " +
  "Padded values stay valid because whitespace is stripped before matching. Applying both edits to src/duration.js.";
const REPORTING = "The table is red against the unfixed parser for five accepted forms: 250ms, 5m, 1h30m, 1.5h and 1d. " +
  "That is the regression, so the cases stay as written. '1h later' is accepted today and should be rejected. " +
  "Telling worker_1 which rows fail so the fix can be checked against them.";
// Replies that stream word by word and then hold until the screenshot is taken.
const HELD = new Set([THINKING, REPORTING]);

function latch() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return { promise, resolve };
}

function call(id, name, args) {
  return { id, type: "function", function: { name, arguments: JSON.stringify(args) } };
}

const read = (id, path) => call(id, "read_file", { path });
const shell = (id, ...command) => call(id, "shell", { command, workdir: null });
const send = (id, to, message) => call(id, "send_message", { to, message });
const edit = (id, path, old_text, new_text) => call(id, "edit_file", { path, old_text, new_text });

// One provider per run. Every branch keys on the agent, its phase and the tool results already in its history.
function provider() {
  const gates = { replied: latch(), advised: latch(), streaming: latch(), release: latch(), deciding: latch(), decide: latch() };
  const state = { frozen: false, inspected: false, drafts: 0, reviewed: false, known: new Set() };

  async function script(input, tools, system) {
    const has = id => input.messages.some(message => message.tool_call_id === id);
    const heard = text => input.messages.some(message => typeof message.content === "string" && message.content.includes(text));
    // An agent that already reported is only being reopened by new mail.
    const fresh = !heard("Previous assessment before new peer evidence") &&
      !input.messages.some(message => message.role === "assistant" && message.content && !message.tool_calls?.length);
    const worker = /^You are parallel worker (worker_\d+),/.exec(system)?.[1];
    const member = /^Your ID: (\w+)$/m.exec(system)?.[1];

    if (tools.has("submit_team")) return ["setup", null, call("team", "submit_team", TEAM)];

    if (worker && system.includes("Phase: coordinate")) {
      const next = { worker_1: "worker_2", worker_2: "worker_3", worker_3: "worker_1" }[worker];
      const offer = {
        worker_1: "task: parser fix; paths: src/duration.js; need: your table as red tests first",
        worker_2: "task: fixtures; paths: test/durations.json test/table.test.js; you run the combined suite after both land",
        worker_3: "task: integrate; paths: CHANGELOG.md; depends: parser fixtures; no edits under src or test",
      }[worker];
      if (!has("coordinate")) return [worker, null, send("coordinate", next, offer)];
      return [worker, fresh ? `Assignment agreed. ${offer}.` : "Assignment unchanged after the peer message."];
    }
    if (worker && system.includes("Phase: acknowledge")) {
      return [worker, null, call("ack", "ack_plan", { plan_id: tools.get("ack_plan").properties.plan_id.enum[0] })];
    }

    if (worker === "worker_1") {
      if (!has("w1-read")) return [worker, null, read("w1-read", "src/duration.js"), read("w1-tests", "test/duration.test.js")];
      if (!has("w1-ask")) return [worker, "The loop ignores the captured unit. One contract question before I touch the guard.",
        send("w1-ask", "stakeholder_1", "bare-number: '90' without unit ? reject | treat as seconds"),
        call("w1-note", "remember", { topic: "Regression cause", note: "parseDuration multiplies every part by UNITS.s; the unit capture is unused since 1.4.0.", evidence: ["w1-read"] })];
      if (!has("w1-base")) {
        await gates.replied.promise;
        return [worker, null, shell("w1-base", "node --test")];
      }
      if (!has("w1-fix")) return [worker, THINKING,
        edit("w1-fix", "src/duration.js", "for (const [, amount] of parts) total += Number(amount) * UNITS.s;",
          "for (const [, amount, unit] of parts) total += Number(amount) * UNITS[unit];"),
        edit("w1-strict", "src/duration.js", "if (!parts.length) throw",
          "if (!source || parts.map(([part]) => part).join(\"\") !== source) throw")];
      if (!has("w1-test")) return [worker, null, shell("w1-test", "node --test")];
      return [worker, fresh
        ? "Fixed src/duration.js. Each part now uses its own unit, and input that the grammar does not fully cover throws RangeError, including bare numbers. Padded values still parse. node --test passes in my tree."
        : "No further change to src/duration.js after the latest message."];
    }

    if (worker === "worker_2") {
      if (!has("w2-list")) return [worker, null, call("w2-list", "list_directory", { path: "test" }), read("w2-readme", "README.md")];
      if (!has("w2-table")) return [worker, "Building the table from the README forms.",
        call("w2-fixtures", "create_file", { path: "test/durations.json", content: FIXTURES }),
        call("w2-table", "create_file", { path: "test/table.test.js", content: TABLE })];
      if (!has("w2-run")) return [worker, null, shell("w2-run", "node --test test/table.test.js")];
      if (!has("w2-msg")) return [worker, REPORTING, send("w2-msg", "worker_1",
        "fixtures: test/durations.json; 8 accept 4 reject; red until per-unit fix: 250ms 5m 1h30m 1.5h 1d")];
      if (heard("From stakeholder_1") && !has("w2-reject")) return [worker, "The bare-number rule needs a rejected case.",
        edit("w2-reject", "test/durations.json", "\"1h later\"]", "\"1h later\", \"90\"]")];
      return [worker, fresh
        ? "Added test/durations.json and test/table.test.js: one case per README form, padded input, and rejected input. The table is red against the unfixed parser, as expected."
        : "Fixtures unchanged after the latest message."];
    }

    if (worker === "worker_3") {
      if (!has("w3-suite")) return [worker, null, shell("w3-suite", "node --test"),
        send("w3-ask", "stakeholder_2", "changelog: fix + stricter input ? one entry | Fixed and Changed")];
      if (!has("w3-read")) {
        await gates.advised.promise;
        return [worker, null, read("w3-read", "CHANGELOG.md")];
      }
      if (!has("w3-log")) return [worker, null, edit("w3-log", "CHANGELOG.md", "## Unreleased\n",
        "## Unreleased\n\n### Fixed\n\n- Compound durations such as `1h30m` use each part's own unit again.\n\n### Changed\n\n- Input without a unit, such as `90`, now throws a `RangeError`.\n")];
      if (!has("w3-verify")) return [worker, null, shell("w3-verify", "node --test 2>&1 | tail -n 8")];
      return [worker, fresh
        ? "Combined suite passes: 15 tests, 0 failures. CHANGELOG.md has a Fixed entry for the arithmetic and a Changed entry for unit-less input."
        : "Integration evidence unchanged after the latest message."];
    }

    if (member && !system.includes("critique this phase's draft")) {
      const [asker, answer] = member === "stakeholder_1"
        ? ["worker_1", "bare-number: reject with RangeError; requirement. --timeout help promises an explicit unit; 1.5h and padded ' 2h ' stay valid. suggestion: keep the 'Unsupported duration' wording"]
        : ["worker_3", "changelog: two entries; Fixed for the 1h30m arithmetic; Changed for unit-less input; requirement. suggestion: quote inputs in backticks"];
      if (!has("answer")) {
        const gate = member === "stakeholder_1" ? gates.replied : gates.advised;
        setTimeout(gate.resolve, 1200);
        return [member, null, send("answer", asker, answer)];
      }
      return [member, `Answered ${asker}. Ratio broadcast the reply to every worker.`];
    }

    if (member && !state.frozen) {
      const opening = {
        reviewer_1: [read("look", "src/duration.js"), send("note", "reviewer_2", "grammar: PART is unanchored; '1h later' => 3600000 today; put a full-coverage check in scope ?")],
        reviewer_2: [read("look", "README.md"), send("note", "reviewer_1", "contract: keep parseDuration(text) => ms and RangeError; no new exports; stricter input is a behaviour change")],
        stakeholder_1: [send("note", "stakeholder_2", "cli: --timeout and --interval call parseDuration; users report 1h30m sleeping 31 s since 1.4.0")],
        stakeholder_2: [read("look", "CHANGELOG.md"), send("note", "reviewer_1", "docs: README promises ms s m h d and compound forms; table tests should mirror that list")],
      }[member];
      if (!has("note")) return [member, null, ...opening];
      return [member, fresh ? {
        reviewer_1: "The draft finds the cause: the loop drops the captured unit. Add a check that the matched parts cover the whole input, otherwise '1h later' keeps parsing.",
        reviewer_2: "Keep the export and the RangeError. Rejecting more input is a behaviour change, so the plan needs a changelog entry and an owner for it.",
        stakeholder_1: "The CLI needs 1h30m and padded values to work. It never documented unit-less input, so rejecting it is acceptable.",
        stakeholder_2: "The README table is the contract. Ask for one table test per row so the docs and the tests cannot drift.",
      }[member] : "Assessment unchanged after the peer message."];
    }

    if (member) {
      const opening = {
        reviewer_1: [read("look", "src/duration.js"), send("note", "reviewer_2", "evidence: loop uses UNITS[unit]; coverage guard rejects '1h later' and '90'; 15/15 in the candidate")],
        reviewer_2: [read("look", "CHANGELOG.md")],
      }[member];
      if (opening && !has("look")) return [member, null, ...opening];
      return [member, fresh ? {
        reviewer_1: "Approve. The arithmetic uses each part's unit and the guard compares the matched parts with the whole input. The table covers every README row.",
        reviewer_2: "Approve. The signature and error type are unchanged, and the changelog names the stricter input as a change.",
        stakeholder_1: "Accept. --timeout 1h30m resolves to 5,400,000 ms and unit-less input fails early, which matches the CLI help.",
        stakeholder_2: "Accept. The changelog has Fixed and Changed entries with quoted inputs, and the README table needs no edit.",
      }[member] : "Assessment unchanged after the peer message."];
    }

    if (tools.has("submit_plan")) {
      state.frozen = true;
      return ["lead_reviewer", null, call("plan", "submit_plan", PLAN)];
    }
    if (tools.has("submit_verdict")) {
      gates.deciding.resolve();
      await gates.decide.promise;
      return ["lead_reviewer", null, call("verdict", "submit_verdict", {
        verdict: "approved",
        assessment: "1h30m parses to 5,400,000 ms again. Every README form has a table test, unit-less input is rejected, and node --test passes on the verified candidate (15 tests).",
      })];
    }
    if (!state.frozen) {
      if (!state.inspected) {
        state.inspected = true;
        return ["lead_reviewer", null, call("lead-list", "list_directory", { path: "." }), read("lead-source", "src/duration.js")];
      }
      return ["lead_reviewer", state.drafts++ === 0
        ? "Draft: parseDuration multiplies every part by UNITS.s. Fix the loop to use the captured unit, add table tests for each README form, and run the full suite on the combined change."
        : "Final instructions: fix the per-unit arithmetic, reject input the grammar does not fully cover, add one table test per README row plus rejected input, and record both the fix and the stricter input in the changelog."];
    }
    if (!state.reviewed) {
      state.reviewed = true;
      return ["lead_reviewer", null, call("lead-status", "git_status", {}), read("lead-final", "src/duration.js")];
    }
    return ["lead_reviewer", "Draft review: the candidate fixes the arithmetic, adds the coverage guard and the table, and verification passed. No open findings."];
  }

  const server = createServer(async (request, response) => {
    try {
      const body = [];
      for await (const part of request) body.push(part);
      const input = JSON.parse(Buffer.concat(body));
      const tools = new Map((input.tools ?? []).map(item => [item.function.name, item.function.parameters]));
      const system = input.messages[0].content;
      const [agent, content, ...calls] = await script(input, tools, system);
      if (process.env.RATIO_DEMO_TRACE) console.error(agent, calls.map(item => item.id).join(" ") || content.slice(0, 48));
      const prompt = Math.round(JSON.stringify(input).length / 3.4);
      const completion = Math.round(JSON.stringify([content, calls]).length / 3) + 12;
      const usage = { prompt_tokens: prompt, completion_tokens: completion, total_tokens: prompt + completion,
        prompt_tokens_details: { cached_tokens: state.known.has(agent) ? Math.round(prompt * 0.72) : 0 } };
      state.known.add(agent);
      if (HELD.has(content)) {
        response.setHeader("content-type", "text/event-stream");
        const delta = (data, finish = null) =>
          response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: data, finish_reason: finish }] })}\n\n`);
        gates.streaming.resolve();
        for (const word of content.split(" ")) {
          delta({ content: word + " " });
          await sleep(90);
        }
        await gates.release.promise;
        delta({ tool_calls: calls.map((item, index) => ({ ...item, index })) }, "tool_calls");
        response.write(`data: ${JSON.stringify({ choices: [], usage })}\n\n`);
        response.end("data: [DONE]\n\n");
        return;
      }
      await sleep(250 + (prompt % 7) * 60);
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ usage, choices: [{ index: 0, finish_reason: calls.length ? "tool_calls" : "stop",
        message: { role: "assistant", content, tool_calls: calls } }] }));
    } catch (error) {
      console.error("provider:", error);
      response.statusCode = 400;
      response.end(String(error));
    }
  });
  return { gates, server };
}

async function workspace() {
  const root = await mkdtemp(join(tmpdir(), "ratio-demo-"));
  const project = join(root, "durations");
  for (const [path, content] of Object.entries(FILES)) {
    await mkdir(dirname(join(project, path)), { recursive: true });
    await writeFile(join(project, path), content);
  }
  const git = (...args) => execFileSync("git", ["-C", project, "-c", "user.name=Demo", "-c", "user.email=demo@example.invalid", ...args]);
  git("init", "--quiet");
  git("add", ".");
  git("commit", "--quiet", "-m", "durations 1.4.0");
  return { root, project };
}

async function profile(root, server) {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const path = join(root, "ratio.toml");
  await writeFile(path, `endpoint = "http://127.0.0.1:${server.address().port}/v1"
model = "provider-model"
context_window = 65536
output_tokens = 2048
[orchestration]
max_parallel_agents = 4
stage_workspace = true
verification_commands = ["node --test"]
`);
  return path;
}

async function web(browser) {
  const { root, project } = await workspace();
  const { gates, server } = provider();
  const config = await profile(root, server);
  const child = spawn(bin, ["-C", project, "--config", config, "--web", "--web-port=0"], { stdio: ["ignore", "pipe", "inherit"] });
  const expired = sleep(120_000, null, { ref: false }).then(() => { throw new Error("web capture timed out"); });
  try {
    await Promise.race([expired, capture()]);
  } finally {
    for (const gate of Object.values(gates)) gate.resolve();
    // The dashboard leaves its runner alive on exit, so stop that as well.
    await once(spawn(bin, ["-C", project, "--control", "cancel"], { stdio: "ignore" }), "exit");
    child.kill();
    await once(child, "exit").catch(() => {});
    server.closeAllConnections();
    await new Promise(done => server.close(done));
    await rm(root, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 });
  }

  async function capture() {
    const [launch] = await once(createInterface({ input: child.stdout }), "line");
    const address = new URL(launch);
    const token = new URLSearchParams(address.hash.slice(1)).get("access");
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 });
    page.setDefaultTimeout(20_000);
    const shot = async name => {
      await sleep(400);
      await page.screenshot({ path: join(out, name + ".png") });
      console.log("saved", name);
    };
    const view = name => page.locator("#navigation").getByRole("link", { name }).click();

    await page.goto(launch);
    await page.locator("#new-run").click();
    await page.getByLabel("Request", { exact: true }).fill(REQUEST);
    await page.locator("#submit-action").click();
    await page.getByRole("heading", { name: "Run overview" }).waitFor();
    await gates.streaming.promise;
    await page.locator('[data-preview-actor="worker_1"]').filter({ hasText: "stakeholder_1 requires" }).waitFor();
    await shot("overview");

    gates.release.resolve();
    gates.decide.resolve();

    const run = new URL(page.url()).pathname.split("/")[2];
    for (let phase; phase !== "Approved";) {
      await sleep(500);
      const listing = await (await fetch(address.origin + "/api/runs", { headers: { Authorization: "Bearer " + token } })).json();
      phase = listing.runs.find(item => item.run_id === run)?.phase;
      if (phase === "Rejected" || phase === "Exhausted") throw new Error("demo run ended as " + phase);
    }
    await page.locator("#status").getByText("Approved").waitFor();
    await sleep(1600);
    await shot("approved");

    await view("Communication");
    await page.locator(".seq-row").first().waitFor();
    await page.locator(".seq-row").filter({ has: page.locator(".seq-kind", { hasText: "FAN" }) }).first().click();
    await page.locator(".inspector").getByRole("link", { name: "Close selection" }).waitFor();
    await shot("sequence");

    await view("Tools & files");
    await page.getByRole("row").filter({ hasText: "edit_file" }).filter({ hasText: "src/duration.js" }).last().getByRole("link").first().click();
    await page.locator(".inspector").getByRole("link", { name: "Close selection" }).waitFor();
    await shot("tools");

    await view("Timeline");
    await page.getByRole("heading", { name: "Timeline", exact: true }).waitFor();
    await page.getByRole("searchbox", { name: "Search entries", exact: true }).fill("RangeError");
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    await page.waitForURL(url => url.searchParams.get("q") === "RangeError");
    await page.locator(".entry").first().waitFor();
    await shot("timeline");
  }
}

const ANSI = ["#1a1714", "#ff4d3d", "#4ade5e", "#ffb000", "#4dc4ff", "#e85aa4", "#45d1c5", "#cfc8bd",
  "#70685f", "#ff7a6e", "#7df08c", "#ffd166", "#8ad8ff", "#ff7fc0", "#86e8de", "#f5f1ea"];

function indexed(value) {
  if (value < 16) return ANSI[value];
  if (value > 231) return `rgb(${Array(3).fill(8 + (value - 232) * 10)})`;
  const level = step => (step ? 55 + step * 40 : 0);
  return `rgb(${level(Math.floor((value - 16) / 36))},${level(Math.floor((value - 16) / 6) % 6)},${level((value - 16) % 6)})`;
}

// Turns `tmux capture-pane -e` output into HTML, one span per run of identical attributes.
function markup(screen) {
  let pen = {};
  const escape = text => text.replace(/[&<>]/g, glyph => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[glyph]);
  const span = text => {
    if (!text) return "";
    const [front, back] = pen.reverse ? [pen.bg ?? "#12100e", pen.fg ?? "#cfc8bd"] : [pen.fg, pen.bg];
    const style = [front && `color:${front}`, back && `background:${back}`, pen.bold && "font-weight:700",
      pen.dim && "opacity:.6", pen.italic && "font-style:italic", pen.underline && "text-decoration:underline"].filter(Boolean).join(";");
    return style ? `<span style="${style}">${escape(text)}</span>` : escape(text);
  };
  return screen.replace(/[\x0e\x0f]|\x1b[()][0-9A-Z]/g, "").split(/\x1b\[([0-9;]*)m/).map((part, index) => {
    if (index % 2 === 0) return span(part);
    const codes = part ? part.split(";").map(Number) : [0];
    for (let at = 0; at < codes.length; at++) {
      const code = codes[at];
      const extended = () => (codes[at + 1] === 5 ? indexed(codes[at += 2]) : `rgb(${codes[at + 2]},${codes[at + 3]},${codes[at += 4]})`);
      if (code === 0) pen = {};
      else if (code === 1) pen.bold = true;
      else if (code === 2) pen.dim = true;
      else if (code === 3) pen.italic = true;
      else if (code === 4) pen.underline = true;
      else if (code === 7) pen.reverse = true;
      else if (code === 22) pen.bold = pen.dim = false;
      else if (code === 23) pen.italic = false;
      else if (code === 24) pen.underline = false;
      else if (code === 27) pen.reverse = false;
      else if (code >= 30 && code <= 37) pen.fg = ANSI[code - 30];
      else if (code === 38) pen.fg = extended();
      else if (code === 39) pen.fg = undefined;
      else if (code >= 40 && code <= 47) pen.bg = ANSI[code - 40];
      else if (code === 48) pen.bg = extended();
      else if (code === 49) pen.bg = undefined;
      else if (code >= 90 && code <= 97) pen.fg = ANSI[code - 82];
      else if (code >= 100 && code <= 107) pen.bg = ANSI[code - 92];
    }
    return "";
  }).join("");
}

// Runs the same scripted provider under the inline terminal UI inside a private tmux server.
async function terminal(browser) {
  const { root, project } = await workspace();
  const { gates, server } = provider();
  const config = await profile(root, server);
  const tmux = (...args) => execFileSync("tmux", ["-L", "ratio-demo", "-f", "/dev/null", ...args], { encoding: "utf8" });
  const page = await browser.newPage({ deviceScaleFactor: 2 });
  const shot = async name => {
    await sleep(1200);
    await page.setContent(`<body style="margin:0;background:#080706"><pre style="display:inline-block;margin:0;padding:14px 18px;` +
      `background:#12100e;color:#cfc8bd;font:14px Menlo,monospace">${markup(tmux("capture-pane", "-p", "-e", "-t", "demo").trimEnd())}</pre>`);
    await page.locator("pre").screenshot({ path: join(out, name + ".png") });
    console.log("saved", name);
  };
  const expired = sleep(120_000, null, { ref: false }).then(() => { throw new Error("terminal capture timed out"); });
  try {
    tmux("start-server", ";", "set", "-g", "default-terminal", "tmux-256color", ";",
      "new-session", "-d", "-x", "150", "-y", "42", "-s", "demo", bin, "-C", project, "--config", config, REQUEST);
    await Promise.race([expired, capture()]);
  } finally {
    for (const gate of Object.values(gates)) gate.resolve();
    try { tmux("kill-server"); } catch {}
    await once(spawn(bin, ["-C", project, "--control", "cancel"], { stdio: "ignore" }), "exit");
    server.closeAllConnections();
    await new Promise(done => server.close(done));
    await rm(root, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 });
  }

  async function capture() {
    await gates.streaming.promise;
    await sleep(2500);
    await shot("tui-activity");
    gates.release.resolve();
    await gates.deciding.promise;
    tmux("send-keys", "-t", "demo", "Tab");
    await shot("tui-communication");
  }
}

await mkdir(out, { recursive: true });
const browser = await chromium.launch();
try {
  if (process.argv[3] !== "terminal") await web(browser);
  if (process.argv[3] !== "web") await terminal(browser);
} finally {
  await browser.close();
}
