import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { createRequire } from "node:module";
import test from "node:test";

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.RATIO_PLAYWRIGHT || "playwright");
const bin = resolve(process.env.RATIO_TEST_BINARY || "target/debug/ra");

function call(name, arguments_, id = name) {
  return { id, type: "function", function: { name, arguments: JSON.stringify(arguments_) } };
}

function completion(message, ...calls) {
  return { choices: [{ index: 0, finish_reason: calls.length ? "tool_calls" : "stop",
    message: { role: "assistant", content: message, tool_calls: calls } }] };
}

const mcp = `
const readline = require("node:readline");
forLines();
function forLines() {
  readline.createInterface({input:process.stdin}).on("line",line=>{
    const frame=JSON.parse(line);
    if(frame.id===undefined)return;
    const result=frame.method==="initialize"?
      {protocolVersion:"2025-11-25",capabilities:{tools:{}}}:
      frame.method==="tools/list"?{tools:[{name:"inspect",inputSchema:{type:"object"}}]}:
      {content:[{type:"text",text:"x".repeat(7000)+" AuditInteriorMarker "+"y".repeat(7000)}]};
    process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:frame.id,result})+"\\n");
  });
}
`;

test("run dashboard manages a live run, triage, and deep-linked MCP audit", { timeout: 60_000 }, async () => {
  const dir = await mkdtemp(join(tmpdir(),"ratio-web-"));
  const profile = join(dir,"ratio.toml");
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  let finishWorker;
  const workGate = new Promise(resolve => { finishWorker = resolve; });
  let endFork;
  const forkGate = new Promise(resolve => { endFork = resolve; });
  let firstWork;
  const started = new Promise(resolve => { firstWork = resolve; });
  let workerCalled = false;
  let sawGuidance = false;
  const provider = createServer(async (request,response) => {
    try {
      const body = [];
      for await (const part of request) body.push(part);
      const input = JSON.parse(Buffer.concat(body));
      const tools = new Map((input.tools ?? []).map(item => [item.function.name,item.function.parameters]));
      const system = input.messages[0].content;
      assert.equal(input.model,"browser-cli-model");
      let output;
      if (system.includes("Inspect the forked evidence")) {
        await forkGate;
        output = completion("Fork recorded.");
      }
      else if (tools.has("submit_team")) output = completion(null,call("submit_team",{
        goal:"Inspect project interfaces",message:"One worker",
        workers:[{name:"Integrator",persona:"Inspect the MCP fixture"}],
        reviewers:[],stakeholders:[],acceptance:["Inspect MCP evidence"],
      }));
      else if (tools.has("route_information")) output = completion(null,call("route_information",{
        category:"evidence",priority:"high",reason:"Relevant to observation",recipients:["worker_1"],
      }));
      else if (tools.has("submit_worker_extension")) output = completion(null,call("submit_worker_extension",{
        name:"Evidence auditor",persona:"Check integration evidence",mentor:"worker_1",stakeholder:null,paths:[],
        task:{id:"audit",worker:"worker_2",description:"Check integration evidence",depends_on:[],acceptance:["Evidence checked"]},
      }));
      else if (system.includes("Read-only onboarding")) {
        assert.ok(!tools.has("create_file"));
        if (input.messages.some(message => message.role === "tool")) output = completion("Onboarding evidence recorded.");
        else if (system.includes("incoming worker")) output = completion(null,
          call("send_message",{to:"worker_1",message:"interface: request contract"},"mentor"),
          call("send_message",{to:"stakeholder_1",message:"evidence: request acceptance"},"adviser"));
        else output = completion(null,call("send_message",{to:"worker_2",message:"acceptance: verify evidence"}));
      }
      else if (tools.has("submit_plan")) output = completion(null,call("submit_plan",{
        plan:"Inspect interfaces",ownership:{worker_1:[]},
        tasks:[{id:"inspect",worker:"worker_1",description:"Inspect interfaces",depends_on:[],acceptance:["Evidence captured"]}],
      }));
      else if (tools.has("ack_plan")) output = completion(null,call("ack_plan",{plan_id:tools.get("ack_plan").properties.plan_id.enum[0]}));
      else if (tools.has("submit_verdict")) output = completion(null,call("submit_verdict",{verdict:"approved",assessment:"MCP evidence verified"}));
      else if (system.includes("Phase: execute")) {
        if (system.includes("worker_2")) output = completion("Reviewed integration evidence.");
        else if (!workerCalled) {
          workerCalled = true;
          firstWork();
          await gate;
          output = completion("Inspect tool metadata and evidence.",
            call("mcp_list",{server:"fixture"}),call("mcp_call",{server:"fixture",name:"inspect",arguments:{subject:"interface"}}));
        } else {
          sawGuidance = JSON.stringify(input.messages).includes("Additional interface evidence");
          await workGate;
          output = completion("Interface inspected.");
        }
      } else output = completion("Inspect the MCP evidence.");
      response.setHeader("content-type","application/json");
      response.end(JSON.stringify(output));
    } catch (error) {
      response.statusCode = 400;
      response.end(String(error));
    }
  });
  provider.listen(0,"127.0.0.1");
  await once(provider,"listening");
  await writeFile(profile,`endpoint = "http://127.0.0.1:${provider.address().port}/v1"
model = "fixture-model"
context_window = 8192
output_tokens = 256
[provider]
stream = false
[mcp_servers.fixture]
command = ${JSON.stringify(process.execPath)}
args = ["-e", ${JSON.stringify(mcp)}]
tools = ["inspect"]
`);
  const child = spawn(bin,["-C",dir,"--config",profile,"--web","--web-port=0","--model","browser-cli-model"],{stdio:["ignore","pipe","pipe"]});
  let errors = "";
  child.stderr.on("data",data => { errors += data; });
  const lines = createInterface({input:child.stdout});
  let browser;
  try {
    const [launch] = await Promise.race([once(lines,"line"),once(child,"exit").then(() => {
      throw new Error("Dashboard did not start: "+errors);
    })]);
    const address = new URL(launch);
    const token = new URLSearchParams(address.hash.slice(1)).get("access");
    assert.equal((await fetch(address.origin+"/api/runs")).status,401);
    assert.equal((await fetch(address.origin+"/api/action",{method:"POST",headers:{
      Authorization:"Bearer "+token,Origin:"https://example.net",
    },body:"{}"})).status,403);
    assert.equal((await fetch(address.origin+"/api/action",{method:"POST",headers:{
      Authorization:"Bearer "+token,Origin:address.origin,
    },body:" ".repeat(80*1024)})).status,413);
    browser = await chromium.launch();
    const page = await browser.newPage({viewport:{width:1440,height:900}});
    page.setDefaultTimeout(10_000);
    const exceptions = [];
    page.on("pageerror", error => exceptions.push(error.message));
    page.on("console", message => {
      if (message.type() === "error" && /Content Security Policy/.test(message.text())) exceptions.push(message.text());
    });
    const shot = async name => {
      if (process.env.RATIO_WEB_SCREENSHOTS) await page.screenshot({path:join(process.env.RATIO_WEB_SCREENSHOTS,name+".png")});
    };
    await page.goto(launch);
    await page.getByRole("heading",{name:"Runs",exact:true}).waitFor();
    await shot("runs");
    assert.equal(new URL(page.url()).hash,"");
    await page.locator("#new-run").click();
    await page.getByLabel("Request",{exact:true}).fill("Inspect fixture MCP evidence");
    await page.locator("#submit-action").click();
    await page.getByRole("heading",{name:"Run overview"}).waitFor();
    await started;
    await shot("overview");
    const run = new URL(page.url()).pathname.split("/")[2];
    const response = await fetch(address.origin+`/api/run/${run}/stream?after=0`,{
      headers:{Authorization:"Bearer "+token},signal:AbortSignal.timeout(5_000),
    });
    assert.match(response.headers.get("content-type"),/^text\/event-stream/);
    const events = response.body.getReader();
    const frame = new TextDecoder().decode((await events.read()).value);
    const update = JSON.parse(frame.split("\n").find(line => line.startsWith("data: ")).slice(6));
    assert.ok(update.cursor > 0);
    assert.ok(update.events.some(event => event.kind === "phase"));
    await events.cancel();
    await page.locator("#navigation").getByRole("link",{name:"Triage"}).click();
    await page.getByRole("button",{name:"Submit information"}).click();
    await page.getByLabel("Information",{exact:true}).fill("Additional interface evidence");
    await page.locator("#submit-action").click();
    await page.getByText("Additional interface evidence",{exact:true}).waitFor();
    release();
    await page.getByText("Relevant to observation",{exact:true}).waitFor();
    await page.getByText("Delivered to",{exact:false}).waitFor();
    await page.getByRole("button",{name:"Request worker",exact:true}).click();
    await page.getByLabel("Assignment and proposed scope").fill("Add an evidence auditor");
    await page.locator("#submit-action").click();
    await page.getByText("Add an evidence auditor",{exact:true}).waitFor();
    finishWorker();
    await page.getByText("Admitted",{exact:true}).waitFor();
    await shot("triage");
    await page.getByRole("link",{name:"Onboarding IPC",exact:true}).click();
    await page.getByText("evidence: request acceptance",{exact:true}).first().waitFor();
    await shot("sequence");
    const latest = await page.locator(".seq-row.selected").getAttribute("id");
    await page.keyboard.press("ArrowUp");
    await page.waitForURL(url => url.searchParams.has("entry"));
    await page.locator(".inspector").getByRole("link",{name:"Close selection"}).waitFor();
    assert.notEqual(await page.locator(".seq-row.selected").getAttribute("id"),latest);
    await shot("sequence-selected");
    await page.getByRole("link",{name:"Agent lanes",exact:true}).click();
    await page.getByRole("heading",{name:"Agent lanes"}).waitFor();
    await shot("lanes");
    await page.locator("#agent-nav").getByRole("link",{name:"Evidence auditor"}).click();
    await page.getByRole("link",{name:"Incoming & outgoing IPC"}).click();
    await page.getByRole("link",{name:"Incoming",exact:true}).click();
    await page.getByText("acceptance: verify evidence",{exact:true}).first().waitFor();
    await shot("agent");
    await page.locator("#navigation").getByRole("link",{name:"Tools & files"}).click();
    await page.getByRole("link",{name:"MCP",exact:true}).click();
    await page.getByRole("heading",{name:"MCP calls",exact:true}).waitFor();
    await page.getByRole("searchbox",{name:"Search entries",exact:true}).fill("AuditInteriorMarker");
    await page.getByRole("button",{name:"Apply",exact:true}).click();
    await page.waitForURL(url => url.searchParams.get("q") === "AuditInteriorMarker");
    await page.getByRole("link",{name:"fixture / mcp_list",exact:true}).waitFor({state:"detached"});
    await page.getByRole("link",{name:"fixture / inspect",exact:true}).click();
    assert.equal(sawGuidance,true);
    await page.getByText("Full result",{exact:false}).first().waitFor();
    await page.locator("summary").filter({hasText:"Full result"}).click();
    await page.getByText("AuditInteriorMarker",{exact:false}).waitFor();
    await shot("tools");
    const visible = page.locator("details").filter({has:page.locator("summary").filter({hasText:"Model-visible result"})});
    assert.doesNotMatch(await visible.textContent(),/AuditInteriorMarker/);
    await page.locator(".inspector").getByRole("link",{name:"Jump to chat"}).click();
    await page.locator(".entry.selected").getByText("Inspect tool metadata and evidence.").waitFor();
    assert.equal(await page.locator(".entry.selected .highlight").textContent(),"mcp_call");
    await page.reload();
    await page.locator(".entry.selected .highlight").waitFor();
    await shot("timeline");
    await page.setViewportSize({width:480,height:845});
    await shot("narrow");
    await page.getByRole("button",{name:"Toggle navigation"}).click();
    await shot("narrow-menu");
    await page.locator("#navigation").getByRole("link",{name:"Overview"}).click();
    await page.getByRole("heading",{name:"Run overview"}).waitFor();
    await page.setViewportSize({width:1440,height:900});
    await page.locator("#controls").getByRole("button",{name:"Fork",exact:true}).click();
    await page.getByLabel("Replacement request").fill("Inspect the forked evidence");
    await page.locator("#submit-action").click();
    await page.getByText("Inspect the forked evidence",{exact:true}).waitFor();
    assert.equal(await page.getByRole("link",{name:"Parent run",exact:true}).getAttribute("href"),`/run/${run}/overview`);
    await shot("fork");
    const controls = page.locator("#controls");
    await controls.getByRole("button",{name:"Pause",exact:true}).click();
    await controls.getByRole("button",{name:"Resume",exact:true}).click();
    await controls.getByRole("button",{name:"Stop",exact:true}).click();
    await page.locator("#submit-action").click();
    await controls.getByRole("button",{name:"Resume saved",exact:true}).click();
    await page.locator("#submit-action").click();
    await controls.getByRole("button",{name:"Stop",exact:true}).click();
    await page.locator("#submit-action").click();
    await controls.getByRole("button",{name:"Resume saved",exact:true}).waitFor();
    assert.deepEqual(exceptions,[]);
  } finally {
    release();
    finishWorker();
    endFork();
    if (browser) await browser.close();
    child.kill();
    await once(child,"exit").catch(() => {});
    provider.closeAllConnections();
    await new Promise(resolve => provider.close(resolve));
    await rm(dir,{recursive:true,force:true});
  }
});

test("resume after restarting the dashboard preserves the run and refreshes tools", { timeout: 60_000 }, async () => {
  const dir = await mkdtemp(join(tmpdir(), "ratio-restart-"));
  const profile = join(dir, "ratio.toml");
  const dashboards = [];
  let browser;
  let activeRun;
  let updated = false;
  let attempts = 0;
  let beforeRestart;
  const ready = new Promise(resolve => { beforeRestart = resolve; });
  const failures = [];
  const provider = createServer(async (request, response) => {
    try {
      const body = [];
      for await (const part of request) body.push(part);
      const input = JSON.parse(Buffer.concat(body));
      const functions = new Set(input.tools.map(tool => tool.function.name));
      const history = input.messages.filter(message => message.role === "tool");
      const result = id => history.find(message => message.tool_call_id === id)?.content;
      let output;
      if (!updated) {
        assert.equal(functions.has("shell"), false);
        if (result("old-schema")) {
          assert.match(result("old-schema"), /old_subject/);
          beforeRestart();
          return;
        }
        output = completion("Initial inspection.", call("mcp_list", {
          server: "fixture", name: "inspect",
        }, "old-schema"));
      } else if (functions.has("submit_verdict")) {
        output = completion(null, call("submit_verdict", { verdict: "approved", assessment: "Restart verified" }));
      } else if (input.messages[0].content.startsWith("You are the worker")) {
        output = completion("Work checked.");
      } else {
        assert.equal(functions.has("shell"), true);
        assert.match(input.messages[0].content, /Runner resumed/);
        assert.match(input.messages[0].content, /Current fixture instructions/);
        assert.match(result("old-schema"), /old_subject/);
        assert.equal(input.model, "updated-model");
        attempts++;
        if (!result("stale-one")) {
          const args = { server: "fixture", name: "inspect", arguments: { old_subject: "stale" } };
          output = completion(null, call("mcp_call", args, "stale-one"), call("mcp_call", args, "stale-two"));
        } else if (!result("current")) {
          for (const id of ["stale-one", "stale-two"]) {
            assert.match(result(id), /^REFUSED: MCP schema refreshed after restart/);
            assert.match(result(id), /new_subject/);
          }
          output = completion(null,
            call("mcp_call", { server: "fixture", name: "inspect", arguments: { new_subject: "current" } }, "current"),
            call("shell", { command: "echo inspection-after-restart" }, "shell-after-restart"));
        } else {
          assert.match(result("current"), /accepted-current-subject/);
          assert.match(result("shell-after-restart"), /inspection-after-restart/);
          output = completion("Continue the original run.");
        }
      }
      response.setHeader("Content-Type", "application/json");
      response.end(JSON.stringify(output));
    } catch (error) {
      failures.push(error);
      response.statusCode = 400;
      response.end(String(error));
    }
  });
  provider.listen(0, "127.0.0.1");
  await once(provider, "listening");

  const writeProfile = async () => {
    const field = updated ? "new_subject" : "old_subject";
    const server = `
const readline = require("node:readline");
readline.createInterface({input:process.stdin}).on("line",line=>{
  const request=JSON.parse(line);
  if(request.id===undefined)return;
  const result=request.method==="initialize"?
    {protocolVersion:"2025-11-25",capabilities:{tools:{}}}:
    request.method==="tools/list"?
      {tools:[{name:"inspect",inputSchema:{type:"object",required:["${field}"]}}]}:
      {content:[{type:"text",text:request.params.arguments.new_subject==="current"?
        "accepted-current-subject":"unexpected-stale-dispatch"}]};
  process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:request.id,result})+"\\n");
});
`;
    await writeFile(profile, `endpoint = "http://127.0.0.1:${provider.address().port}/v1"
model = "${updated ? "updated-model" : "original-model"}"
context_window = 8192
output_tokens = 256
[orchestration]
allow_inspection_shell = ${updated}
[mcp_servers.fixture]
command = ${JSON.stringify(process.execPath)}
args = ["-e", ${JSON.stringify(server)}]
tools = ["inspect"]
inspection_tools = ["inspect"]
instructions = "${updated ? "Current" : "Initial"} fixture instructions."
`);
  };
  const launch = async () => {
    const child = spawn(bin, ["-C", dir, "--config", profile, "--fixed-team", "--web"],
      { stdio: ["ignore", "pipe", "pipe"] });
    dashboards.push(child);
    let errors = "";
    child.stderr.on("data", data => { errors += data; });
    const lines = createInterface({ input: child.stdout });
    const [address] = await Promise.race([once(lines, "line"), once(child, "exit").then(() => {
      throw new Error("Dashboard did not start: " + errors);
    })]);
    return new URL(address);
  };
  const api = async (url, path, body) => {
    const authorization = "Bearer " + new URLSearchParams(url.hash.slice(1)).get("access");
    const response = await fetch(new URL(path, url), {
      method: body ? "POST" : "GET",
      headers: { Authorization: authorization, Origin: url.origin, "Content-Type": "application/json" },
      body: body ? JSON.stringify(body) : undefined,
    });
    const json = await response.json();
    assert.equal(response.status, 200, JSON.stringify(json));
    return json;
  };
  try {
    await writeProfile();
    const first = await launch();
    const launched = await api(first, "/api/action", { id: "start", action: "start", text: "Inspect a fixture" });
    activeRun = launched.run_id;
    assert.ok(activeRun);
    await ready;
    const previous = await api(first, `/api/run/${activeRun}/live`);
    await api(first, "/api/action", { id: "pause", action: "pause", run: activeRun });
    const exited = once(dashboards[0], "exit");
    dashboards[0].kill();
    await exited;
    updated = true;
    await writeProfile();
    const second = await launch();
    const paused = await api(second, `/api/run/${activeRun}/live`);
    assert.equal(paused.paused, true);
    assert.equal(paused.pid, previous.pid);
    browser = await chromium.launch();
    const page = await browser.newPage();
    page.setDefaultTimeout(10_000);
    const target = new URL(`/run/${activeRun}/overview`, second);
    target.hash = second.hash;
    await page.goto(target.href);
    await page.locator("#controls").getByRole("button", { name: "Resume", exact: true }).click();
    await page.getByText("Restart verified", { exact: false }).first().waitFor();
    assert.equal(new URL(page.url()).pathname.split("/")[2], activeRun);
    assert.deepEqual(failures, []);
    assert.equal(attempts, 3);
    const runs = await api(second, "/api/runs");
    assert.deepEqual(runs.runs.map(run => [run.run_id, run.phase]), [[activeRun, "Approved"]]);
  } finally {
    if (browser) await browser.close();
    for (const child of dashboards) {
      if (child.exitCode !== null || child.signalCode !== null) continue;
      const exited = once(child, "exit");
      child.kill();
      await exited;
    }
    if (activeRun) {
      const cancel = spawn(bin, ["-C", dir, "--control", "cancel"], { stdio: "ignore" });
      await once(cancel, "exit");
    }
    provider.closeAllConnections();
    await new Promise(resolve => provider.close(resolve));
    await rm(dir, { recursive: true, force: true });
  }
});
