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
