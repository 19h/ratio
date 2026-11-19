// Stages a scripted run against a local mock provider and saves the README screenshots.
//
//   cargo build -p ra
//   node docs/screenshots/capture.mjs [output directory]
//
// RATIO_PLAYWRIGHT selects the Playwright module and RATIO_TEST_BINARY the `ra` binary.
// The terminal captures also need tmux. Nothing here talks to a real provider.
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
const NOTE = "The CLI passes values with padding, for example ' 2h '. Keep those valid.";

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

const table = JSON.parse(readFileSync(new URL("./fixtures/durations.json", import.meta.url)));

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
    "worker_2: Add test/fixtures/durations.json and a table test covering every README form plus rejected input.",
    "Acceptance: one case per documented form | rejected inputs listed",
    "",
    "worker_3: After both land, run the combined suite and record the change in CHANGELOG.md.",
    "Acceptance: node --test passes | changelog names the fix and the stricter input",
  ].join("\n"),
  ownership: {
    worker_1: [{ path: "src/duration.js", directory: false }],
    worker_2: [{ path: "test/fixtures", directory: true }, { path: "test/table.test.js", directory: false }],
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
  const gates = { replied: latch(), advised: latch(), fixtures: latch(), streaming: latch(), release: latch() };
  const state = { frozen: false, inspected: false, drafts: 0, reviewed: false, known: new Set() };

  async function script(input, tools, system) {
    const has = id => input.messages.some(message => message.tool_call_id === id);
    const heard = text => input.messages.some(message => typeof message.content === "string" && message.content.includes(text));
    const fresh = input.messages.at(-1).role === "tool";
    const worker = /^You are parallel worker (worker_\d+),/.exec(system)?.[1];
    const member = /^Your ID: (\w+)$/m.exec(system)?.[1];

    if (tools.has("submit_team")) return ["setup", null, call("team", "submit_team", TEAM)];
    if (tools.has("route_information")) return ["lead_reviewer", null, call("route", "route_information", {
      category: "constraint", priority: "normal", reason: "Constrains the input the parser fix must keep accepting",
      recipients: ["worker_1", "worker_2"],
    })];

    if (worker && system.includes("Phase: coordinate")) {
      const next = { worker_1: "worker_2", worker_2: "worker_3", worker_3: "worker_1" }[worker];
      const offer = {
        worker_1: "task: parser fix; paths: src/duration.js; need: your table as red tests first",
        worker_2: "task: fixtures; paths: test/fixtures/ test/table.test.js; you run the combined suite after both land",
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
        call("w2-fixtures", "create_file", { path: "test/fixtures/durations.json", content: FIXTURES }),
        call("w2-table", "create_file", { path: "test/table.test.js", content: TABLE })];
      if (!has("w2-run")) return [worker, null, shell("w2-run", "node --test test/table.test.js")];
      if (!has("w2-msg")) return [worker, null, send("w2-msg", "worker_1",
        "fixtures: test/fixtures/durations.json; 8 accept 4 reject; red until per-unit fix: 250ms 5m 1h30m 1.5h 1d")];
      if (heard("From stakeholder_1") && !has("w2-reject")) return [worker, "The bare-number rule needs a rejected case.",
        edit("w2-reject", "test/fixtures/durations.json", "\"1h later\"]", "\"1h later\", \"90\"]")];
      if (has("w2-reject") && fresh) gates.fixtures.resolve();
      return [worker, fresh
        ? "Added test/fixtures/durations.json and test/table.test.js: one case per README form, padded input, and rejected input. The table is red against the unfixed parser, as expected."
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
      return [member, fresh || !opening ? {
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
    if (tools.has("submit_verdict")) return ["lead_reviewer", null, call("verdict", "submit_verdict", {
      verdict: "approved",
      assessment: "1h30m parses to 5,400,000 ms again. Every README form has a table test, unit-less input is rejected, and node --test passes on the verified candidate (15 tests).",
    })];
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
      const prompt = Math.round(JSON.stringify(input).length / 3.4);
      const completion = Math.round(JSON.stringify([content, calls]).length / 3) + 12;
      const usage = { prompt_tokens: prompt, completion_tokens: completion, total_tokens: prompt + completion,
        prompt_tokens_details: { cached_tokens: state.known.has(agent) ? Math.round(prompt * 0.72) : 0 } };
      state.known.add(agent);
      if (content === THINKING) {
        await gates.fixtures.promise;
        response.setHeader("content-type", "text/event-stream");
        const delta = (data, finish = null) =>
          response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: data, finish_reason: finish }] })}\n\n`);
        gates.streaming.resolve();
        for (const word of THINKING.split(" ")) {
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

async function web() {
  const { root, project } = await workspace();
  const { gates, server } = provider();
  const config = await profile(root, server);
  const child = spawn(bin, ["-C", project, "--config", config, "--web", "--web-port=0"], { stdio: ["ignore", "pipe", "inherit"] });
  const browser = await chromium.launch();
  try {
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

    await view("Triage");
    await page.getByRole("button", { name: "Submit information" }).click();
    await page.getByLabel("Information", { exact: true }).fill(NOTE);
    await page.locator("#submit-action").click();
    await page.getByText(NOTE, { exact: true }).waitFor();
    gates.release.resolve();

    const run = new URL(page.url()).pathname.split("/")[2];
    for (let phase; phase !== "Approved";) {
      await sleep(500);
      const listing = await (await fetch(address.origin + "/api/runs", { headers: { Authorization: "Bearer " + token } })).json();
      phase = listing.runs.find(item => item.run_id === run)?.phase;
      if (phase === "Rejected" || phase === "Exhausted") throw new Error("demo run ended as " + phase);
    }
    await page.getByText("Delivered to", { exact: false }).first().waitFor();
    await shot("triage");

    await view("Overview");
    await page.getByRole("heading", { name: "Run overview" }).waitFor();
    await sleep(1600);
    await shot("approved");

    await view("Communication");
    await page.locator(".seq-row").first().waitFor();
    await shot("sequence-latest");
    await page.locator(".seq-row").filter({ has: page.locator(".seq-kind", { hasText: "FAN" }) }).first().click();
    await page.locator(".inspector").getByRole("link", { name: "Close selection" }).waitFor();
    await shot("sequence");

    await view("Tools & files");
    await page.getByRole("heading").first().waitFor();
    await shot("tools-list");

    await view("Timeline");
    await page.locator(".entry").first().waitFor();
    await shot("timeline");

    await page.locator("#agent-nav").getByRole("link", { name: "Parser engineer" }).click();
    await page.locator(".entry").first().waitFor();
    await shot("agent");
  } finally {
    for (const gate of Object.values(gates)) gate.resolve();
    await browser.close();
    child.kill();
    await once(child, "exit").catch(() => {});
    server.closeAllConnections();
    await new Promise(done => server.close(done));
    await rm(root, { recursive: true, force: true });
  }
}

await mkdir(out, { recursive: true });
await web();
