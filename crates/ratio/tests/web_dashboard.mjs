import { gunzipSync } from "node:zlib";
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

test("run dashboard manages a live run, triage, and deep-linked MCP audit", { timeout: 90_000 }, async () => {
  const dir = await mkdtemp(join(tmpdir(),"ratio-web-"));
  const profile = join(dir,"ratio.toml");
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  let streamMore;
  const nextChunk = new Promise(resolve => { streamMore = resolve; });
  let finishStream;
  const usageGate = new Promise(resolve => { finishStream = resolve; });
  let retryRouting;
  const retryGate = new Promise(resolve => { retryRouting = resolve; });
  let triageAttempts = 0;
  let finishWorker;
  const workGate = new Promise(resolve => { finishWorker = resolve; });
  let endFork;
  const forkGate = new Promise(resolve => { endFork = resolve; });
  let firstWork;
  const started = new Promise(resolve => { firstWork = resolve; });
  let workerCalled = false;
  let sawGuidance = false;
  let sawSteering = false;
  let reportedTotal = 0;
  const provider = createServer(async (request,response) => {
    try {
      const body = [];
      for await (const part of request) body.push(part);
      const input = JSON.parse(Buffer.concat(body));
      const tools = new Map((input.tools ?? []).map(item => [item.function.name,item.function.parameters]));
      const system = input.messages[0].content;
      assert.equal(input.model,"browser-cli-model");
      sawSteering ||= JSON.stringify(input.messages).includes("User steering:\\nPrefer the narrower interface");
      let output;
      if (system.includes("Inspect the forked evidence")) {
        await forkGate;
        output = completion("Fork recorded.");
      }
      else if (tools.has("ping")) output = completion(null,call("ping",{}));
      else if (tools.has("submit_team")) output = completion(null,call("submit_team",{
        goal:"Inspect project interfaces",message:"One worker",
        workers:[{name:"Integrator",persona:"Inspect the MCP fixture"}],
        reviewers:[],stakeholders:[],acceptance:["Inspect MCP evidence"],
      }));
      else if (tools.has("route_information")) {
        assert.match(input.messages[1].content, /worker_1: Integrator/);
        assert.match(input.messages[1].content, /Additional interface evidence for Integrator/);
        const recipient = ++triageAttempts === 1 ? "lead_reviewer" : "worker_1";
        output = completion(null,call("route_information",{
          category:"evidence",priority:"high",reason:"Relevant to observation",recipients:[recipient],
        }));
      }
      else if (tools.has("submit_worker_extension")) {
        const corrected = input.messages.at(-1).content.includes("new task must be assigned to worker_2");
        output = completion(null,call("submit_worker_extension",{
          name:"Evidence auditor",persona:"Check integration evidence",mentor:"worker_1",stakeholder:null,paths:[],
          task:{id:"audit",worker:corrected ? "worker_2" : "worker_3",description:"Check integration evidence.\n".repeat(35)+"Last checkpoint.",depends_on:[],acceptance:["Evidence checked"]},
        }));
      }
      else if (system.includes("Read-only onboarding")) {
        assert.ok(!tools.has("create_file"));
        if (input.messages.some(message => message.role === "tool")) output = completion("Onboarding evidence recorded.");
        else if (system.includes("incoming worker")) output = completion(null,
          call("send_message",{to:"worker_1",message:"interface: request contract "+"integration contract ".repeat(30)},"mentor"),
          call("send_message",{to:"stakeholder_1",message:"evidence: request acceptance"},"adviser"));
        else output = completion(null,call("send_message",{to:"worker_2",message:"acceptance: verify evidence"}));
      }
      else if (tools.has("submit_plan")) output = completion(null,call("submit_plan",{
        plan:"Inspect interfaces",ownership:{worker_1:[]},
        tasks:[{id:"inspect",worker:"worker_1",description:"Inspect interfaces",depends_on:[],acceptance:["Evidence captured"]}],
      }));
      else if (tools.has("ack_plan")) output = completion(null,call("ack_plan",{plan_id:tools.get("ack_plan").properties.plan_id.enum[0]}));
      else if (system.startsWith("Investigate the question")) output = completion("Public fixture specialist conclusion.");
      else if (tools.has("submit_verdict")) {
        const previous = JSON.stringify(input.messages);
        if (previous.includes("Public fixture specialist conclusion.")) {
          output = completion(null, call("submit_verdict", {verdict:"approved", assessment:"MCP evidence verified"}));
        } else if (previous.includes("verify-fixture")) {
          output = completion(null, call("await_jobs", {jobs:[1]}));
        } else {
          output = completion(null, call("delegate", {role:"research", task:"Inspect the recorded fixture evidence."}, "verify-fixture"));
        }
      }
      else if (system.includes("Phase: execute")) {
        if (system.includes("worker_2")) output = completion("Reviewed integration evidence.");
        else if (!workerCalled) {
          workerCalled = true;
          assert.equal(tools.get("tool_search").properties.query.type, "array");
          response.setHeader("content-type", "text/event-stream");
          const delta = (data, finish = null) =>
            response.write(`data: ${JSON.stringify({choices:[{index:0,delta:data,finish_reason:finish}]})}\n\n`);
          delta({content:"Inspect tool metadata and evidence."});
          firstWork();
          await nextChunk;
          delta({reasoning_content:"evidence ".repeat(60)});
          await gate;
          response.write('data: {"choices":[],"usage":{"prompt_tokens":20,"prompt_tokens_details":{"cached_tokens":7}}}\n\n');
          await usageGate;
          const calls = [call("mcp_list",{server:"fixture"}),call("tool_search",{server:"fixture",query:["spe","inspect"]}),
            call("mcp_call",{server:"fixture",name:"inspect",arguments:{subject:"interface"}})];
          delta({tool_calls:calls.map((item,index) => ({ ...item,index }))}, "tool_calls");
          reportedTotal += 25;
          response.write('data: {"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":5}}\n\n');
          response.end("data: [DONE]\n\n");
          return;
        } else if (triageAttempts === 1) {
          await retryGate;
          output = completion(null, call("list_directory", { path: "." }, "check-before-reroute"));
        } else {
          sawGuidance = JSON.stringify(input.messages).includes("Additional interface evidence");
          await workGate;
          output = completion("Interface inspected.");
        }
      } else output = completion("Inspect the MCP evidence.");
      response.setHeader("content-type","application/json");
      // The provider check runs outside any run, so its usage is not metered there.
      if (!tools.has("ping")) reportedTotal += 1;
      response.end(JSON.stringify({...output, usage:{prompt_tokens:1,completion_tokens:0,total_tokens:1}}));
    } catch (error) {
      response.statusCode = 400;
      response.end(String(error));
    }
  });
  provider.listen(0,"127.0.0.1");
  await once(provider,"listening");
  await writeFile(profile,`goal = "Profile goal for the override test"
endpoint = "http://127.0.0.1:${provider.address().port}/v1"
model = "fixture-model"
context_window = 8192
output_tokens = 256
[provider.headers]
x-fixture = "header-secret-value"
[mcp_servers.fixture]
command = ${JSON.stringify(process.execPath)}
args = ["-e", ${JSON.stringify(mcp)}]
tools = ["inspect"]
`);
  const child = spawn(bin,["-C",dir,"--config",profile,"--web","--web-port=0","--web-share-port=0","--model","browser-cli-model"],
    {stdio:["ignore","pipe","pipe"],env:{...process.env,RATIO_API_KEY:"fixture-secret-key"}});
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
    const browserRequests = [];
    page.on("request", request => {
      if (new URL(request.url()).pathname.startsWith("/api/")) browserRequests.push(request.url());
    });
    const streamTraffic = [];
    let compressedFrames = 0;
    let operatorWelcome;
    page.on("websocket", websocket => {
      websocket.on("framesent", ({ payload }) => {
        if (typeof payload === "string") streamTraffic.push(JSON.parse(payload).type);
      });
      websocket.on("framereceived", ({ payload }) => {
        if (typeof payload === "string") {
          const frame = JSON.parse(payload);
          if (frame.type === "welcome") operatorWelcome = frame;
        } else if (payload.subarray(13, 17).equals(Buffer.from([0x28, 0xb5, 0x2f, 0xfd]))) {
          compressedFrames++;
        }
      });
    });
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
    const summary = await (await fetch(address.origin+"/api/config",{headers:{Authorization:"Bearer "+token}})).text();
    const effective = JSON.parse(summary);
    assert.deepEqual([effective.ok,effective.models.worker,effective.provider.key,effective.provider.headers],
      [true,"browser-cli-model",true,["x-fixture"]]);
    assert.ok(!summary.includes("fixture-secret-key") && !summary.includes("header-secret-value"));
    await page.getByRole("heading",{name:"Configuration",exact:true}).waitFor();
    await page.getByRole("button",{name:"Test provider",exact:true}).click();
    await page.getByText("Reachable. The model returned the test tool call.",{exact:true}).waitFor();
    await page.locator("#new-run").click();
    await page.getByLabel("Request",{exact:true}).fill("Inspect fixture MCP evidence");
    await page.locator("#submit-action").click();
    await page.getByRole("heading",{name:"Run overview"}).waitFor();
  await page.getByText("Inspect fixture MCP evidence", { exact: true }).first().waitFor({ state: "attached" });
    await started;
    await page.locator('[data-preview-actor="worker_1"]').filter({hasText:"Inspect tool metadata and evidence."}).waitFor();
    assert.match(await page.locator("#connection").innerText(), /\b1 generating\b/i);
    assert.match(await page.locator("#agent-count").innerText(), /^[1-9]\d*$/);
    assert.doesNotMatch(await page.locator("#status").innerText(), /\b(?:agents|generating)\b/);
    const tokenCounter = page.locator("[data-token-usage]");
    await tokenCounter.filter({hasText:/^~/}).waitFor();
    const initialTokens = Number(await tokenCounter.getAttribute("data-token-usage"));
    const incoming = page.locator('[data-token-flow="output"]');
    const initialIncoming = Number(await incoming.getAttribute("data-token-count"));
    assert.equal(await page.locator('[data-token-flow="cached_input"]').getAttribute("data-token-count"), null);
    streamMore();
    await page.waitForFunction(({ initialTokens, initialIncoming }) => {
      const total = document.querySelector("[data-token-usage]");
      const output = document.querySelector('[data-token-flow="output"]');
      const rate = document.querySelector("[data-output-tps]");
      return Number(total?.dataset.tokenUsage) > initialTokens &&
        Number(output?.dataset.tokenCount) > initialIncoming && Number(rate?.dataset.outputTps) > 0;
    }, {initialTokens, initialIncoming});
    await shot("overview");
    const run = new URL(page.url()).pathname.split("/")[2];
    const queryCount = streamTraffic.filter(type => type === "query").length;
    for (const name of ["Agents", "Timeline", "Communication", "Tools & files", "Triage", "Overview"]) {
      await page.locator("#navigation").getByRole("link", { name, exact: true }).click();
      await page.getByRole("heading", { name: name === "Overview" ? "Run overview" : name, exact: true }).waitFor();
    }
    assert.equal(streamTraffic.filter(type => type === "query").length, queryCount, "warm navigation uses cached datasets");
    const asset = await fetch(address.origin + "/dashboard.js", { headers: { "Accept-Encoding": "gzip" } });
    assert.equal(asset.headers.get("content-encoding"), "gzip");
    assert.match(await asset.text(), /connectDashboard/);
    const identity = await fetch(address.origin + "/dashboard.js", { headers: { "Accept-Encoding": "gzip;q=0, *;q=1" } });
    assert.equal(identity.headers.get("content-encoding"), null);
    await identity.arrayBuffer();
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
    await page.getByRole("button", { name: "Share", exact: true }).click();
    await page.getByRole("button", { name: "Create link", exact: true }).click();
    await page.locator("#share-url").waitFor();
    const shared = new URL(await page.locator("#share-url").inputValue());
    const shareToken = url => new URLSearchParams(url.hash.slice(1)).get("share");
    assert.notEqual(shared.origin, address.origin);
    assert.match(shareToken(shared), /^[a-f0-9]{64}$/);
    const publicPath = `/api/run/${run}`;
    const publicRead = (path, credential = shareToken(shared)) => fetch(new URL(path, shared), {
      headers: { Authorization: "Bearer " + credential }, signal: AbortSignal.timeout(5000),
    });
    assert.equal((await publicRead(publicPath + "/overview")).status, 200);
    const sharedLive = await (await publicRead(publicPath + "/live")).json();
    assert.deepEqual(Object.keys(sharedLive).sort(), ["generations", "paused", "token_usage"]);
    assert.equal((await publicRead(publicPath + "/overview", token)).status, 401);
    assert.equal((await publicRead("/api/run/1-2/overview")).status, 401);
    for (const path of ["/api/runs", "/api/action", publicPath + "/share", publicPath + "/checkpoint",
      publicPath + "/history?legacy=1", publicPath + "/entry/legacy-1"]) {
      assert.equal((await publicRead(path)).status, 403, path);
    }
    assert.equal((await fetch(address.origin + publicPath + "/overview", {
      headers: { Authorization: "Bearer " + shareToken(shared) },
    })).status, 401);
    for (const credential of [shareToken(shared), token]) {
      for (const method of ["POST", "PUT", "PATCH", "DELETE"]) {
        const control = await fetch(new URL("/api/action", shared), {
          method,
          headers: { Authorization: "Bearer " + credential, Origin: shared.origin, "Content-Type": "application/json" },
          body: JSON.stringify({ id: "forbidden", action: "pause", run }),
        });
        assert.equal(control.status, 403, method);
      }
    }
    assert.equal((await fetch(new URL("/api/config", shared),
      { headers: { Authorization: "Bearer " + shareToken(shared) } })).status, 403);
    const visitor = await browser.newPage();
    visitor.setDefaultTimeout(10_000);
    let visitorSocket;
    const sharedWelcomes = [];
    let dictionaryBootstraps = 0;
    await visitor.routeWebSocket("**/api/dashboard/socket", socket => {
      const upstream = socket.connectToServer();
      upstream.onMessage(frame => {
        if (typeof frame === "string") {
          const message = JSON.parse(frame);
          if (message.type === "welcome") {
            visitorSocket = socket;
            sharedWelcomes.push(message);
          }
        } else if (frame[0] === 6) {
          dictionaryBootstraps++;
        }
        socket.send(frame);
      });
    });
    await visitor.goto(shared.href);
    await visitor.getByRole("heading", { name: "Run overview" }).waitFor();
    assert.equal(operatorWelcome.version, 2);
    assert.equal(sharedWelcomes[0].version, 2);
    if (process.env.RATIO_EXPECT_DICTIONARIES === "1") {
      assert.equal(operatorWelcome.codec, "zstd-dict");
      assert.equal(sharedWelcomes[0].codec, "zstd-dict");
    }
    if (operatorWelcome.dictionary && sharedWelcomes[0].dictionary) {
      assert.notEqual(operatorWelcome.dictionary.sha256, sharedWelcomes[0].dictionary.sha256);
    }
    const invalid = await visitor.evaluate(({ run, dictionary }) => new Promise((resolve, reject) => {
      const socket = new WebSocket(location.origin.replace(/^http/, "ws") + "/api/dashboard/socket");
      const messages = [];
      socket.addEventListener("open", () => socket.send(JSON.stringify({
        type: "hello", version: 2, token: "0".repeat(64), run, dictionary,
      })));
      socket.addEventListener("message", event => {
        messages.push(typeof event.data === "string" ? JSON.parse(event.data) : "unexpected binary frame");
      });
      socket.addEventListener("close", () => resolve(messages), { once: true });
      socket.addEventListener("error", reject);
    }), { run, dictionary: operatorWelcome.dictionary?.sha256 });
    assert.deepEqual(invalid, [{ type: "unauthorized" }]);
    await visitorSocket.close({ code: 1012 });
    await visitor.waitForFunction(() => document.body.hasAttribute("data-stream-stale"));
    await visitor.waitForFunction(() => !document.body.hasAttribute("data-stream-stale"));
    assert.equal(sharedWelcomes.length, 2);
    assert.deepEqual(sharedWelcomes[0].dictionary, sharedWelcomes[1].dictionary);
    assert.equal(dictionaryBootstraps, sharedWelcomes[0].dictionary ? 1 : 0);
    if (sharedWelcomes[0].dictionary) {
      const rotated = await visitor.evaluate(({ run, token }) => new Promise((resolve, reject) => {
        const socket = new WebSocket(location.origin.replace(/^http/, "ws") + "/api/dashboard/socket");
        let welcome;
        socket.binaryType = "arraybuffer";
        socket.addEventListener("open", () => socket.send(JSON.stringify({
          type: "hello", version: 2, token, run, dictionary: "0".repeat(64),
        })));
        socket.addEventListener("message", event => {
          if (typeof event.data === "string") {
            welcome = JSON.parse(event.data);
            return;
          }
          const header = new DataView(event.data);
          resolve({ dictionary: welcome.dictionary, flags: header.getUint8(0), bytes: header.getUint32(9) });
          socket.close();
        });
        socket.addEventListener("error", reject);
      }), { run, token: shareToken(shared) });
      assert.deepEqual(rotated, {
        dictionary: sharedWelcomes[0].dictionary, flags: 6, bytes: sharedWelcomes[0].dictionary.bytes,
      });
    }
    assert.equal(new URL(visitor.url()).hash, "");
    await visitor.getByText("Read only", { exact: true }).waitFor();
    assert.equal(await visitor.locator("#new-run").isVisible(), false);
    assert.equal(await visitor.locator("#controls button").count(), 0);
    await visitor.locator("#navigation").getByRole("link", { name: "Timeline" }).click();
    await visitor.getByRole("heading", { name: "Timeline" }).waitFor();
    assert.equal(await visitor.getByRole("link", { name: "Checkpoint archive" }).count(), 0);
    await visitor.locator("#navigation").getByRole("link", { name: "Triage" }).click();
    await visitor.getByRole("heading", { name: "Triage" }).waitFor();
    assert.equal(await visitor.getByRole("button", { name: /Submit information|Request worker|Retry triage/ }).count(), 0);
    const streamBeforeRotation = await publicRead(publicPath + "/stream");
    assert.equal(streamBeforeRotation.status, 200);
    const publicEvents = streamBeforeRotation.body.getReader();
    assert.equal((await publicEvents.read()).done, false);
    await page.getByRole("button", { name: "Replace link", exact: true }).click();
    await page.waitForFunction(previous => document.querySelector("#share-url").value !== previous, shared.href);
    const replacement = new URL(await page.locator("#share-url").inputValue());
    assert.notEqual(shareToken(replacement), shareToken(shared));
    assert.equal((await publicRead(publicPath + "/overview")).status, 401);
    assert.equal((await publicRead(publicPath + "/overview", shareToken(replacement))).status, 200);
    while (!(await publicEvents.read()).done) {}
    await visitor.getByRole("heading", { name: "Share unavailable" }).waitFor();
    await visitor.close();
    await page.getByRole("button", { name: "Revoke link", exact: true }).click();
    await page.getByText("No active link.", { exact: true }).waitFor();
    assert.equal((await publicRead(publicPath + "/overview", shareToken(replacement))).status, 401);
    await page.getByRole("button", { name: "Close", exact: true }).click();
    await page.locator("#navigation").getByRole("link",{name:"Triage"}).click();
    await page.getByRole("button",{name:"Submit information"}).click();
    await page.getByLabel("Information",{exact:true}).fill("Additional interface evidence for Integrator");
    await page.locator("#submit-action").click();
    await page.getByText("Additional interface evidence for Integrator",{exact:true}).waitFor();
    await page.getByText("Waiting for triage at the next runner checkpoint.", { exact: true }).waitFor();
    await page.getByRole("button",{name:"Send guidance",exact:true}).click();
    await page.getByLabel("Guidance",{exact:true}).fill("Prefer the narrower interface");
    await page.locator("#submit-action").click();
    await page.getByText("guidance queued",{exact:true}).waitFor();
    release();
    await page.locator('[data-token-flow="cached_input"][data-token-count="7"]').waitFor();
    finishStream();
    await page.getByText("Awaiting delivery", { exact: true }).waitFor();
    assert.equal(await page.getByText("Delivered to", { exact: true }).count(), 0);
    await page.getByRole("button", { name: "Retry triage", exact: true }).click();
    await page.getByText("Information queued for retry", { exact: true }).waitFor();
    retryRouting();
    await page.getByText("Relevant to observation",{exact:true}).waitFor();
    await page.getByText("Delivered to",{exact:false}).waitFor();
    assert.equal(triageAttempts, 2);
    await page.getByRole("button",{name:"Request worker",exact:true}).click();
    await page.getByLabel("Assignment and proposed scope").fill("Add an evidence auditor");
    await page.locator("#submit-action").click();
    await page.getByText("Add an evidence auditor",{exact:true}).waitFor();
    finishWorker();
    await page.getByText("Admitted",{exact:true}).waitFor();
    await shot("triage");
    await page.getByRole("link",{name:"Lead proposal activity",exact:true}).click();
    await page.locator('[data-kind="contract_error"]').getByText(/proposal attempt 1: new task must be assigned to worker_2/).waitFor();
    await page.locator('[data-kind="proposal"]').getByText(/proposed Evidence auditor \(worker_2\) with mentor worker_1/).waitFor();
    await page.locator("#navigation").getByRole("link",{name:"Triage"}).click();
    await page.getByRole("link",{name:"Onboarding IPC",exact:true}).click();
    await page.getByText("evidence: request acceptance",{exact:true}).first().waitFor();
    await shot("sequence");
    const sequenceIds = (await page.locator(".seq-id").allTextContents()).map(Number);
    assert.ok(sequenceIds.length > 1);
    assert.deepEqual(sequenceIds, [...sequenceIds].sort((a, b) => b - a));
    assert.equal(await page.locator(".seq-row:first-of-type.selected").count(), 1);
    assert.equal(await page.locator(".view-scroll").evaluate(element => element.scrollTop), 0);
    const sequencePage = await browser.newPage({ viewport: { width: 1500, height: 900 } });
    await sequencePage.addInitScript(() => { window.Worker = undefined; });
    await sequencePage.routeWebSocket("**/api/dashboard/socket", socket => {
      const upstream = socket.connectToServer();
      let multipart = false;
      upstream.onMessage(bytes => {
        if (typeof bytes === "string") return socket.send(bytes);
        const first = !multipart;
        multipart = !(bytes[0] & 2);
        if (!first || multipart) return socket.send(bytes);
        const content = bytes.subarray(9);
        const update = JSON.parse((bytes[0] & 1 ? gunzipSync(content) : content).toString());
        const agents = update.values?.[`/api/run/${run}/overview`]?.data?.event?.workflow?.agents;
        if (!agents) return socket.send(bytes);
        while (agents.length < 14) {
          const id = `stakeholder_${agents.length}`;
          agents.push({ id, name: id });
        }
        const header = Buffer.from(bytes.subarray(0, 9));
        header[0] = 2;
        socket.send(Buffer.concat([header, Buffer.from(JSON.stringify(update))]));
      });
    });
    const sequenceUrl = new URL(page.url());
    sequenceUrl.hash = address.hash;
    await sequencePage.goto(sequenceUrl.href);
    const diagram = sequencePage.locator(".seq-panel");
    await diagram.locator(".seq-head .alias").nth(13).waitFor();
    await sequencePage.locator(".inspector").waitFor();
    for (const [width, shouldScroll] of [[1500, false], [1100, true], [480, true]]) {
      await sequencePage.setViewportSize({ width, height: 900 });
      const layout = await diagram.evaluate(panel => {
        const scroll = panel.parentElement;
        const row = panel.querySelector(".seq-row");
        const lanes = row.querySelector(".seq-lanes");
        const lastAlias = panel.querySelector(".seq-head .alias:last-child");
        const overflows = scroll.scrollWidth > scroll.clientWidth;
        scroll.scrollLeft = scroll.scrollWidth;
        const viewport = scroll.getBoundingClientRect();
        return {
          overflows,
          lastLaneVisible: lastAlias.getBoundingClientRect().right <= viewport.right + 1,
          lanesWithinRow: lanes.getBoundingClientRect().right <= row.getBoundingClientRect().right,
          headerMatchesRow: panel.querySelector(".seq-head").clientWidth === row.clientWidth,
          controlsVisible: [...panel.querySelectorAll(".panel-head, .seq-summary, .seq-legend")].every(element =>
            element.getBoundingClientRect().left >= viewport.left - 1),
        };
      });
      assert.deepEqual(layout, {
        overflows: shouldScroll, lastLaneVisible: true, lanesWithinRow: true, headerMatchesRow: true, controlsVisible: true,
      }, `Sequence geometry at ${width}px`);
    }
    await sequencePage.close();
    await page.keyboard.press("ArrowDown");
    await page.waitForURL(url => url.searchParams.has("entry"));
    await page.locator(".inspector").getByRole("link",{name:"Close selection"}).waitFor();
    assert.equal(Number(await page.locator(".seq-row.selected .seq-id").textContent()), sequenceIds[1]);
    await shot("sequence-selected");
    await page.getByRole("link",{name:"Agent lanes",exact:true}).click();
    await page.getByRole("heading",{name:"Agent lanes"}).waitFor();
    const lanes = page.getByRole("region",{name:"Agent lanes"});
    const message = lanes.locator(".lane-message").filter({hasText:"interface: request contract"});
    await message.waitFor();
    const laneIds = await lanes.locator(".lane-message").evaluateAll(links =>
      links.map(link => Number(link.getAttribute("aria-label").split(" ")[1])));
    assert.deepEqual(laneIds, sequenceIds);
    assert.equal(await message.count(), 1);
    assert.ok(await message.evaluate(element =>
      element.scrollHeight > element.clientHeight && element.clientWidth >= 240));
    await message.locator("xpath=../..").getByText("Delivered",{exact:true}).waitFor();
    await shot("lanes");
    await page.getByRole("link",{name:"Route matrix",exact:true}).click();
    const grid = page.getByRole("region",{name:"Route matrix"});
    await grid.getByRole("link",{name:/Evidence auditor to .*1 delivered, 0 pending, 0 dropped/}).first().waitFor();
    assert.ok(await grid.evaluate(element => element.scrollWidth <= element.clientWidth));
    await page.getByRole("searchbox",{name:"Search entries"}).fill("request acceptance");
    await page.getByRole("button",{name:"Apply",exact:true}).click();
    await page.waitForURL(url => url.searchParams.get("q") === "request acceptance");
    await page.waitForFunction(() => document.querySelectorAll(".route-matrix td a").length === 1);
    await grid.getByRole("link",{name:/Evidence auditor to .*1 delivered, 0 pending, 0 dropped/}).first().waitFor();
    await page.locator("#agent-nav").getByRole("link",{name:"Evidence auditor"}).click();
    const persona = page.locator('pre[aria-label="Persona"]').filter({hasText:"Check integration evidence"});
    await persona.waitFor({state:"visible"});
    const task = page.locator('pre[aria-label="Task"]');
    assert.ok(await task.evaluate(element => element.scrollHeight > element.clientHeight));
    assert.equal(await task.evaluate(element => getComputedStyle(element).maxHeight),
      await persona.evaluate(element => getComputedStyle(element).maxHeight));
    assert.ok(await page.locator('.view-scroll[data-scroll="body"]').evaluate(element => element.clientHeight > 200));
    await page.getByRole("link",{name:"Incoming & outgoing IPC"}).click();
    await page.getByRole("link",{name:"Incoming",exact:true}).click();
    await page.getByText("acceptance: verify evidence",{exact:true}).first().waitFor();
    assert.equal(await persona.isVisible(), true);
    await shot("agent");
    await page.locator("#navigation").getByRole("link",{name:"Tools & files"}).click();
    await page.getByRole("link",{name:"MCP",exact:true}).click();
    await page.getByRole("heading",{name:"MCP calls",exact:true}).waitFor();
    await page.getByRole("link",{name:"fixture / tool_search",exact:true}).waitFor();
    await page.getByRole("searchbox",{name:"Search entries",exact:true}).fill("AuditInteriorMarker");
    await page.getByRole("button",{name:"Apply",exact:true}).click();
    await page.waitForURL(url => url.searchParams.get("q") === "AuditInteriorMarker");
    await page.getByRole("link",{name:"fixture / mcp_list",exact:true}).waitFor({state:"detached"});
    await page.getByRole("link",{name:"fixture / inspect",exact:true}).click();
    assert.equal(sawGuidance,true);
    assert.equal(sawSteering,true);
    await page.getByText("Full result",{exact:false}).first().waitFor();
    await page.locator("summary").filter({hasText:"Full result"}).click();
    await page.getByText("AuditInteriorMarker",{exact:false}).waitFor();
    await shot("tools");
    const visible = page.locator("details").filter({has:page.locator("summary").filter({hasText:"Model-visible result"})});
    assert.doesNotMatch(await visible.textContent(),/AuditInteriorMarker/);
    await page.locator(".inspector").getByRole("link",{name:"Jump to chat"}).click();
    await page.locator(".entry.selected").getByText("Inspect tool metadata and evidence.").waitFor();
    assert.equal(await page.locator(".entry.selected .highlight").textContent(),"mcp_call");
    assert.equal(streamTraffic.filter(type => type === "hello").length, 1);
    await page.reload();
    await page.locator(".entry.selected .highlight").waitFor();
    await shot("timeline");
    assert.equal(await page.locator('.timeline .entry[data-kind="tool_result"]').count(), 0);
    const timeline = page.locator(".timeline");
    const emptyTurns = timeline.locator('.entry[data-kind="chat"] .entry-body').filter({
      has: page.locator(".call-list"), hasNot: page.locator("pre"),
    });
    assert.equal(await emptyTurns.count(), 0);
    const messageCall = timeline.locator('.entry[data-kind="tool"]').filter({
      has: page.getByRole("link",{name:"send_message",exact:true}),
    }).first();
    await messageCall.getByRole("link",{name:"Jump to chat"}).click();
    await timeline.locator(".entry.selected .highlight").filter({hasText:/^send_message$/}).waitFor();
    assert.equal(await emptyTurns.count(), 1);
    await page.goBack();
    await timeline.locator(".entry.selected .highlight").filter({hasText:"mcp_call"}).waitFor();
    await page.locator(".entry.selected .highlight").click();
    const inspection = page.locator(".inspector");
    const running = inspection.getByRole("link",{name:"Running",exact:true});
    await running.waitFor();
    await running.click();
    await page.locator('.entry.selected[data-kind="tool_result"]').getByText("Running",{exact:true}).waitFor();
    await inspection.getByRole("link",{name:"Jump to chat",exact:true}).click();
    await page.locator(".entry.selected .highlight").waitFor();
    await page.locator("#controls").getByRole("button",{name:"Fork",exact:true}).waitFor();
    await page.getByRole("button", {name:"Share", exact:true}).click();
    await page.getByRole("button", {name:"Create link", exact:true}).click();
    await page.locator("#share-url").waitFor();
    const jobShare = new URL(await page.locator("#share-url").inputValue());
    const sharedJobs = await (await publicRead(publicPath + "/history?kind=job", shareToken(jobShare))).json();
    assert.equal(sharedJobs.rows.length, 1);
    assert.equal(sharedJobs.rows[0].data.state, "completed");
    assert.equal(sharedJobs.rows[0].data.specialist, "job-1");
    const jobDetail = await (await publicRead(publicPath + "/entry/job-1", shareToken(jobShare))).json();
    assert.equal(jobDetail.outcome.data.output.text, "Public fixture specialist conclusion.");
    assert.ok(jobDetail.turn.data.calls.some(call => call.tool === "delegate"));
    const jobVisitor = await browser.newPage();
    await jobVisitor.goto(jobShare.href);
    await jobVisitor.locator("#navigation").getByRole("link",{name:"Tools & files"}).click();
    await jobVisitor.getByRole("link",{name:"Jobs",exact:true}).click();
    await jobVisitor.waitForURL(url => url.searchParams.get("tab") === "jobs");
    await jobVisitor.getByRole("link",{name:"Specialist conversation",exact:true}).click();
    await jobVisitor.getByRole("heading",{name:"Research",exact:true}).waitFor();
    await jobVisitor.getByText("Public fixture specialist conclusion.",{exact:true}).first().waitFor();
    assert.equal(await jobVisitor.locator("#controls button").count(), 0);
    await jobVisitor.close();
    await page.getByRole("button",{name:"Close",exact:true}).click();
    await page.setViewportSize({width:480,height:845});
    await shot("narrow");
    await page.getByRole("button",{name:"Toggle navigation"}).click();
    await shot("narrow-menu");
    await page.locator("#navigation").getByRole("link",{name:"Overview"}).click();
    await page.getByRole("heading",{name:"Run overview"}).waitFor();
    const assignment = page.getByLabel("Task details for Evidence auditor");
    await assignment.waitFor();
    const taskPreview = assignment.locator(".plan-task-preview");
    assert.ok(await taskPreview.evaluate(element =>
      element.scrollHeight > element.clientHeight && element.clientHeight < 65));
    await assignment.click();
    await page.getByLabel("Task description",{exact:true}).filter({hasText:"Last checkpoint."}).waitFor({state:"visible"});
    await assignment.click();
    await page.locator("#controls").getByRole("button",{name:"Fork",exact:true}).waitFor();
    await page.waitForFunction(total => {
      const counter = document.querySelector("[data-token-usage]");
      return Number(counter?.dataset.tokenUsage) === total && !counter.textContent.startsWith("~");
    }, reportedTotal);
    assert.equal(await incoming.getAttribute("data-token-count"), "5");
    assert.equal(await page.locator('[data-token-flow="input"]').getAttribute("data-token-count"), String(reportedTotal - 5));
    const cache = page.locator('[data-token-flow="cached_input"]');
    assert.equal(await cache.getAttribute("data-token-count"), "7");
    assert.match(await cache.textContent(), /7\+$/);
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
    assert.deepEqual(browserRequests, [], "the dashboard should use no HTTP data endpoints");
    assert.equal(streamTraffic.filter(type => type === "hello").length, 2);
    assert.ok(streamTraffic.includes("action"));
    assert.ok(compressedFrames > 0, "realtime data was compressed");
    assert.deepEqual(exceptions,[]);
  } finally {
    streamMore();
    release();
    finishStream();
    retryRouting();
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

test("a broken profile is reported before a run starts", { timeout: 60_000 }, async () => {
  const dir = await mkdtemp(join(tmpdir(), "ratio-config-"));
  const profile = join(dir, "ratio.toml");
  const settings = 'endpoint = "http://127.0.0.1:9/v1"\nmodel = "fixture-model"\n';
  await writeFile(profile, settings);
  const child = spawn(bin, ["-C", dir, "--config", profile, "--web", "--web-port=0"], { stdio: ["ignore", "pipe", "pipe"] });
  let browser;
  try {
    const [launch] = await once(createInterface({ input: child.stdout }), "line");
    const address = new URL(launch);
    const token = new URLSearchParams(address.hash.slice(1)).get("access");
    const refused = await fetch(address.origin + "/api/action", {
      method: "POST",
      headers: { Authorization: "Bearer " + token, Origin: address.origin, "Content-Type": "application/json" },
      body: JSON.stringify({ id: "refused-start", action: "start", text: "Inspect the project" }),
    });
    assert.equal(refused.status, 409);
    assert.equal((await refused.json()).error, "set --context-window to the model's token limit");
    browser = await chromium.launch();
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    page.setDefaultTimeout(10_000);
    await page.goto(launch);
    await page.getByText("set --context-window to the model's token limit", { exact: true }).waitFor();
    assert.equal(await page.getByRole("button", { name: "Test provider", exact: true }).count(), 0);
    const escapedRequest = await page.evaluate(token => new Promise((resolve, reject) => {
      const socket = new WebSocket(location.origin.replace(/^http/, "ws") + "/api/dashboard/socket");
      socket.addEventListener("close", () => reject(new Error("The large request closed the stream")));
      socket.addEventListener("open", () => {
        socket.send(JSON.stringify({ type: "hello", version: 1, token, run: null, gzip: false }));
      });
      socket.addEventListener("message", event => {
        if (typeof event.data !== "string") return;
        const message = JSON.parse(event.data);
        if (message.type === "welcome") {
          socket.send(JSON.stringify({ type: "action", action: {
            id: "escaped-start", action: "start", text: "z" + "\u0000".repeat(65534) + "z",
          } }));
        } else if (message.type === "receipt" && message.id === "escaped-start") {
          resolve(message.result.error);
          socket.close();
        }
      });
    }), token);
    assert.equal(escapedRequest, "set --context-window to the model's token limit");
    await page.locator("#new-run").click();
    assert.equal(await page.locator("#submit-action").isDisabled(), true);
    const useGoal = page.getByRole("button", { name: "Use TOML goal", exact: true });
    assert.equal(await useGoal.isDisabled(), true);
    await page.getByRole("button", { name: "Cancel", exact: true }).click();
    await writeFile(profile, settings + "context_window = 8192\n");
    await page.getByRole("button", { name: "Recheck", exact: true }).click();
    await page.getByText("8,192 tokens, replies up to 2,048", { exact: true }).waitFor();
    await page.getByText("None. A run continues until the reviewer approves or rejects it.", { exact: true }).waitFor();
    await page.locator("#new-run").click();
    assert.equal(await page.locator("#submit-action").isDisabled(), false);
    await useGoal.click();
    await page.getByText("Set a nonempty top-level goal in the TOML profile", { exact: true }).waitFor();
    await writeFile(profile, settings + 'context_window = 8192\ngoal = "   "\n');
    const blank = await fetch(address.origin + "/api/action", {
      method: "POST",
      headers: { Authorization: "Bearer " + token, Origin: address.origin, "Content-Type": "application/json" },
      body: JSON.stringify({ id: "blank-config-goal", action: "start_config" }),
    });
    assert.equal(blank.status, 409);
    assert.equal((await blank.json()).error, "Set a nonempty top-level goal in the TOML profile");
  } finally {
    if (browser) await browser.close();
    const exited = once(child, "exit");
    child.kill();
    await exited;
    await rm(dir, { recursive: true, force: true });
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
        output = completion("Initial inspection.", call("tool_search", {
          server: "fixture", query: "inspect",
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
          output = completion(null, call("mcp_call", args, "stale-one"),
            call("tool_search", { server: "fixture", query: "inspect" }, "current-schema"),
            call("mcp_call", args, "stale-two"));
        } else {
          for (const id of ["stale-one", "stale-two"]) {
            assert.match(result(id), /^REFUSED: Fetch the current schema with tool_search/);
            assert.doesNotMatch(result(id), /new_subject/);
          }
          assert.match(result("current-schema"), /new_subject/);
          if (!result("current")) {
            output = completion(null,
              call("mcp_call", { server: "fixture", name: "inspect", arguments: { new_subject: "current" } }, "current"),
              call("shell", { command: ["echo inspection-after-restart"], workdir: null }, "shell-after-restart"));
          } else {
            assert.match(result("current"), /accepted-current-subject/);
            assert.match(result("shell-after-restart"), /inspection-after-restart/);
            output = completion("Continue the original run.");
          }
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

  const writeProfile = async (goal = "Inspect a fixture") => {
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
    await writeFile(profile, `goal = ${JSON.stringify(goal)}
endpoint = "http://127.0.0.1:${provider.address().port}/v1"
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
  const launch = async (sharePort = 0) => {
    const child = spawn(bin, ["-C", dir, "--config", profile, "--fixed-team", "--web", `--web-share-port=${sharePort}`],
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
    await writeProfile("Earlier fixture goal");
    const first = await launch();
    browser = await chromium.launch();
    const starter = await browser.newPage();
    starter.setDefaultTimeout(10_000);
    await starter.goto(first.href);
    await starter.getByRole("heading", { name: "Runs", exact: true }).waitFor();
    await starter.locator("#new-run").click();
    assert.equal(await starter.getByLabel("Request", { exact: true }).inputValue(), "");
    await writeProfile();
    await starter.getByRole("button", { name: "Use TOML goal", exact: true }).click();
    await starter.waitForURL(/\/run\/[\d-]+\/overview$/);
    activeRun = new URL(starter.url()).pathname.split("/")[2];
    await starter.getByText("Inspect a fixture", { exact: true }).first().waitFor();
    await starter.close();
    await ready;
    const shared = await api(first, "/api/action", { id: "share", action: "create_share", run: activeRun });
    const shareUrl = new URL(shared.url);
    const previous = await api(first, `/api/run/${activeRun}/live`);
    await api(first, "/api/action", { id: "pause", action: "pause", run: activeRun });
    const exited = once(dashboards[0], "exit");
    dashboards[0].kill();
    await exited;
    updated = true;
    await writeProfile();
    const second = await launch(shareUrl.port);
    const sharing = await api(second, `/api/run/${activeRun}/share`);
    assert.equal(sharing.active, true);
    assert.equal(sharing.origin, shareUrl.origin);
    const paused = await api(second, `/api/run/${activeRun}/live`);
    assert.equal(paused.paused, true);
    assert.equal(paused.pid, previous.pid);
    const visitor = await browser.newPage();
    let streamRequests = 0;
    await visitor.routeWebSocket("**/api/dashboard/socket", socket => {
      if (++streamRequests === 1) socket.close({ code: 1011 });
      else socket.connectToServer();
    });
    await visitor.goto(shareUrl.href);
    await visitor.getByRole("heading", { name: "Run overview" }).waitFor({ timeout: 10_000 });
    await visitor.getByText("Read only", { exact: true }).waitFor();
    assert.equal(streamRequests, 2);
    await visitor.close();
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
