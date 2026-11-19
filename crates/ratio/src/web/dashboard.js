"use strict";

const $ = (selector, scope = document) => scope.querySelector(selector);
const main = $("#content");
const launch = new URLSearchParams(location.hash.slice(1)).get("access");
if (launch && /^[a-f0-9]{64}$/.test(launch)) {
  sessionStorage.setItem("ratio-access", launch);
  history.replaceState(null, "", location.pathname + location.search);
}
const token = sessionStorage.getItem("ratio-access");
const labels = {
  overview: ["Overview", "◈"], history: ["Timeline", "▤"], agents: ["Agents", "◉"],
  ipc: ["Communication", "↔"], tools: ["Tools & files", "⌘"], triage: ["Triage", "◇"],
};
const state = { runs: [], summary: null, live: null, route: {}, version: 0, stream: null, follow: true, refreshPending: false };
const number = new Intl.NumberFormat();
const clock = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });

function node(tag, props = {}, ...children) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") element.className = value;
    else if (key.startsWith("on")) element.addEventListener(key.slice(2), value);
    else element.setAttribute(key, value === true ? "" : value);
  }
  element.append(...children.flat(Infinity).filter(child => child !== null && child !== undefined)
    .map(child => child instanceof Node ? child : String(child)));
  return element;
}

function link(text, href, props = {}) { return node("a", { href, ...props }, text); }
function button(text, action, props = {}) { return node("button", { type: "button", onclick: action, ...props }, text); }
function code(text) { return node("code", {}, text ?? ""); }
function pre(text, props = {}) { return node("pre", props, text ?? ""); }
function muted(text) { return node("p", { class: "muted" }, text); }
function pill(value) { return node("span", { class: "pill", "data-state": String(value).toLowerCase() }, title(value)); }
function title(value) { return String(value ?? "").replaceAll("_", " ").replace(/([a-z])([A-Z])/g, "$1 $2").replace(/^./, letter => letter.toUpperCase()); }
function asText(value) { return typeof value === "string" ? value : value?.text ?? ""; }
function time(ms) { return ms ? node("time", { datetime: new Date(ms).toISOString(), title: new Date(ms).toLocaleString() }, clock.format(ms)) : "—"; }

function panel(label, ...content) {
  return node("section", { class: "panel" }, node("h2", {}, label), node("div", { class: "panel-body" }, content));
}

function heading(label, category, ...actions) {
  return node("div", { class: "title-row" },
    node("div", {}, node("p", { class: "section-label" }, category), node("h1", {}, label)),
    node("div", {}, actions));
}

function table(columns, rows, className = "") {
  if (!rows.length) return node("p", { class: "empty" }, "No matching entries.");
  return node("div", { class: "table-wrap" }, node("table", { class: className },
    node("thead", {}, node("tr", {}, columns.map(label => node("th", { scope: "col" }, label)))),
    node("tbody", {}, rows.map(cells => node("tr", {}, cells.map(value => node("td", {}, value)))))));
}

function runUrl(page = "overview", query = {}, id = state.route.run) {
  const params = new URLSearchParams(Object.entries(query).filter(([, value]) => value !== undefined && value !== null && value !== ""));
  return id ? `/run/${encodeURIComponent(id)}/${page}${params.size ? "?" + params : ""}` : "/";
}
function apiRun(view, query = {}) {
  const params = new URLSearchParams(Object.entries(query).filter(([, value]) => value !== undefined && value !== null && value !== ""));
  return `/api/run/${encodeURIComponent(state.route.run)}/${view}${params.size ? "?" + params : ""}`;
}
function actorName(id) { return state.summary?.event?.workflow?.agents?.find(agent => agent.id === id)?.name ?? id; }
function actorLink(id, label = actorName(id)) {
  return id && id !== "user" ? link(label, runUrl("agent/" + encodeURIComponent(id))) : (label || "Ratio");
}
function entryUrl(key, page = "history", extra = {}) {
  return runUrl(page, { ...extra, entry: key });
}
function captureError(error) {
  $("#notice").textContent = error.message;
  $("#notice").hidden = false;
}

async function api(path, options = {}) {
  const response = await fetch(path, { ...options, headers: { Authorization: "Bearer " + (token ?? ""), ...options.headers } });
  if (!response.ok) {
    const payload = await response.json().catch(() => ({}));
    throw new Error(payload.error ?? `Request failed (${response.status})`);
  }
  return response.json();
}

function route() {
  const [, prefix, run, page, actor] = location.pathname.split("/");
  let decoded;
  try { decoded = actor ? decodeURIComponent(actor) : undefined; } catch { decoded = undefined; }
  return { run: prefix === "run" ? run : undefined, page: page || "overview", actor: decoded, query: Object.fromEntries(new URLSearchParams(location.search)) };
}

function navigate(href) {
  history.pushState(null, "", href);
  load();
}

function renderNavigation() {
  const selected = state.route.run;
  const current = state.route.page;
  const select = $("#run-select");
  select.replaceChildren(node("option", { value: "" }, "All runs"),
    ...state.runs.map(run => node("option", { value: run.run_id }, `${title(run.phase)} · ${run.run_id.slice(-10)}`)));
  select.value = selected ?? "";
  $("#navigation").replaceChildren(...Object.entries(labels).map(([page, [name, icon]]) =>
    link([node("span", { class: "nav-icon", "aria-hidden": "true" }, icon), name], runUrl(page), {
      "aria-current": selected && current === page ? "page" : undefined,
    })));
  const agents = (state.summary?.event?.workflow?.agents ?? []).filter(agent => agent.id !== "user");
  $("#agent-nav").replaceChildren(...agents.map(agent =>
    link([node("span", { class: `dot${generation(agent.id) ? " live" : ""}`, "data-agent-dot":agent.id }), node("span", { class: "truncate" }, agent.name)],
      runUrl("agent/" + encodeURIComponent(agent.id)), { "aria-current": state.route.actor === agent.id ? "page" : undefined })));
  $("#breadcrumb").textContent = selected ? (current === "agent" ? actorName(state.route.actor) : labels[current]?.[0] ?? "Run") : "Workspace";
  $("#global-search").hidden = !selected;
  renderLive();
}

function generation(id) { return state.live?.generations?.find(item => item.actor === id); }

function renderLive() {
  const connection = $("#connection");
  const status = state.live ? state.live.paused ? "paused" : "live" : "history";
  connection.dataset.state = status;
  $("span", connection).textContent = state.live
    ? state.live.paused ? "Paused" : `Live · ${state.live.generations?.length ?? 0} generating`
    : state.route.run ? "Recorded run" : "Local workspace";
  const controls = $("#controls");
  const actions = state.live ? [
    button("Add worker", () => openAction("add_worker"), { disabled: state.summary?.run?.phase !== "Work" }),
    button(state.live.paused ? "Resume" : "Pause", () => sendAction(state.live.paused ? "resume" : "pause").catch(captureError)),
    button("Stop", () => openAction("stop")),
  ] : state.route.run && state.summary ? [
    button("Resume saved", () => openAction("resume_saved"), { disabled: ["Approved", "Rejected", "Exhausted"].includes(state.summary.run.phase) }),
    button("Fork", () => openAction("fork")),
  ] : [];
  if (controls.dataset.mode !== status || controls.dataset.phase !== state.summary?.run?.phase) {
    controls.replaceChildren(...actions);
    controls.dataset.mode = status;
    controls.dataset.phase = state.summary?.run?.phase ?? "";
  }
  for (const card of document.querySelectorAll("[data-preview-actor]")) {
    const active = generation(card.dataset.previewActor);
    card.hidden = !active;
    card.textContent = active?.text || (active ? active.label : "");
  }
  for (const dot of document.querySelectorAll("[data-agent-dot]")) dot.classList.toggle("live",Boolean(generation(dot.dataset.agentDot)));
}

function workspace() {
  return [
    heading("Runs", "Workspace", button("Start run", () => openAction("start"), { class: "accent" })),
    panel("Run history", table(["Run", "Phase", "Cycle", "Last checkpoint"], state.runs.map(run =>
      [link(run.run_id, runUrl("overview", {}, run.run_id), { class: "mono" }), pill(run.phase), run.cycle, code(run.last_sequence)]))),
  ];
}

function agentCard(agent, task) {
  const active = generation(agent.id);
  return node("article", { class: "agent-card" },
    node("div", { class: "clusters" }, node("span", { class: `dot${active ? " live" : ""}` }), actorLink(agent.id, agent.name)),
    node("div", { class: "clusters" }, code(agent.id), task ? pill(task.state) : null),
    task?.task ? muted(task.task.description) : null,
    pre(active?.text || "", { class: "preview inset", hidden: !active, "data-preview-actor": agent.id }));
}

function overview() {
  const data = state.summary;
  const flow = data.event?.workflow ?? {};
  const metrics = data.metrics ?? {};
  const tasks = flow.tasks ?? [];
  const agents = (flow.agents ?? []).filter(agent => agent.id !== "user");
  const stats = [
    ["Phase", title(data.run.phase)], ["Cycle", data.run.cycle], ["Agents", agents.length],
    ["Metered tokens", number.format(metrics.metered_tokens ?? 0)],
    ["Tool calls", number.format(metrics.tool_receipts ?? 0)], ["IPC deliveries", number.format(metrics.ipc_deliveries ?? 0)],
  ];
  return [
    heading("Run overview", "Workspace / Run", data.parent_run ? link("Parent run",runUrl("overview",{},data.parent_run)) : null,
      code(data.run.run_id), pill(flow.stopped ?? data.run.phase)),
    node("section", { class: "hero" }, node("span", { class: "section-label" }, "Goal"), node("p", {}, data.goal || "Waiting for a goal"),
      data.request && data.request !== data.goal ? node("details", {}, node("summary", {}, "Original request"), pre(data.request)) : null),
    node("div", { class: "metrics" }, stats.map(([label, value]) =>
      node("div", { class: "metric" }, node("span", { class: "section-label" }, label), node("strong", {}, value)))),
    panel("Agent activity", node("div", { class: "agent-grid" }, agents.map(agent => agentCard(agent, tasks.find(task => task.task.worker === agent.id))))),
    node("div", { class: "split" },
      panel("Execution plan", flow.plan_id ? code(flow.plan_id) : muted("Waiting for a frozen plan."),
        tasks.length ? table(["Task", "Worker", "State", "Depends on"], tasks.map(({ task, state: status }) =>
          [task.description, actorLink(task.worker), pill(status), task.depends_on.length ? code(task.depends_on.join(", ")) : "—"])) : null,
        (data.acceptance ?? []).length ? node("details", {}, node("summary", {}, "Acceptance criteria"), node("ul", {}, data.acceptance.map(text => node("li", {}, text)))) : null,
        data.plan?.ownership ? node("details", {}, node("summary", {}, "Write ownership"),
          table(["Worker", "Paths"], data.plan.ownership.map((paths, index) => [actorLink(`worker_${index + 1}`), paths.map(path => code(path.path + (path.directory ? "/" : "")))]))) : null),
      panel("Run state", node("div", { class: "clusters" }, (flow.acknowledgements ?? []).map(id => pill(id + " acknowledged"))),
        flow.worker_addition ? link(flow.worker_addition, runUrl("triage")) : muted("No worker request yet."),
        flow.verification ? node("div", { class: "clusters" }, pill(flow.verification.passed ? "completed" : "failed"), code(flow.verification.candidate)) : muted("No verification receipt yet."),
        data.verdict ? pill(data.verdict.verdict) : null, data.assessment ? pre(data.assessment) : null,
        data.latest_error ? node("div",{ class:"clusters" },pill("Failed"),pre(data.latest_error.data.text),
          link("Open failure in timeline",entryUrl(data.latest_error.key))) : null,
        node("details", {}, node("summary", {}, "Usage details"), table(["Metric", "Value"], Object.entries(metrics).map(([name, value]) => [title(name), number.format(value)]))))),
  ];
}

function filters(route, { actor = true, kind = false } = {}) {
  const query = route.query;
  const form = node("form", { class: "filters", onsubmit: event => {
    event.preventDefault();
    navigate(runUrl(location.pathname.split("/").slice(3).join("/"), {
      tab:query.tab,legacy:query.legacy,worker_request:query.worker_request,path:query.path,
      from:query.from,to:query.to,direction:query.direction,...Object.fromEntries(new FormData(event.currentTarget)),
    }));
  }});
  const select = (name, label, options) => {
    const element = node("select", { name, "aria-label": label },
      node("option", { value: "" }, label), options.map(([value, caption]) => node("option", { value }, caption)));
    element.value = query[name] ?? "";
    return element;
  };
  form.append(node("input", { type: "search", name: "q", value: query.q ?? "", placeholder: "Search entries", "aria-label": "Search entries" }),
    ...(actor ? [select("actor", "All agents", (state.summary?.event?.workflow?.agents ?? []).map(agent => [agent.id, agent.name]))] : []),
    ...(kind ? [select("kind", "All event types", ["chat","tool","tool_result","ipc","phase","brief","proposal","guidance","triage","triage_delivery","compaction","verification","contract_error","error","control"].map(type => [type,title(type)]))] : []),
    node("input", { type: "number", min: "1", name: "cycle", value: query.cycle ?? "", placeholder: "Cycle", "aria-label": "Cycle" }),
    select("phase", "All phases", ["ProposeTeam","Plan","PlanConsult","PlanSynthesize","Coordinate","FreezePlan","Acknowledge","Work","OnboardWorker","Review","ReviewConsult","ReviewSynthesize","Publish"].map(phase => [phase,title(phase)])),
    node("button", { type: "submit" }, "Apply"));
  return form;
}

function pager(rows, page, query) {
  const oldest = rows.at(-1)?.sequence;
  return node("nav", { class: "pagination", "aria-label": "History pagination" },
    query.before ? link("Latest", runUrl(page, { ...query, before: undefined, entry: undefined })) : null,
    rows.length >= 60 ? link("Older entries", runUrl(page, { ...query, before: oldest, entry: undefined })) : null);
}

function historyCard(event, selected, call, anchor = true) {
  const data = event.data ?? {};
  const isSelected = event.key === selected;
  let body;
  if (event.kind === "chat") {
    body = [asText(data.text) ? pre(asText(data.text)) : null,
      node("div", { class: "call-list" }, (data.calls ?? []).map(item =>
        link(item.tool, entryUrl(item.key), { class: call === item.key ? "highlight" : undefined, "data-call": item.key })))];
  } else if (event.kind === "tool" || event.kind === "tool_result") {
    body = node("div", { class: "clusters" },
      link(toolName(data), entryUrl(data.call ?? event.key)), pill(data.state), data.path ? code(data.path) : null,
      data.turn ? link("Jump to chat", entryUrl(data.turn,"history",{ call: data.call ?? event.key })) : null);
  } else if (event.kind === "ipc") {
    body = [node("div", { class: "clusters" }, "To", (data.to ?? []).map(id => actorLink(id)), pill(data.delivery)), pre(data.body)];
  } else if (event.kind === "triage") {
    body = [node("div",{ class:"clusters" },link(`Information #${data.item}`,runUrl("triage",{ item:data.item,inbox_before:data.item+1 })),
      pill(data.state),data.decision?.priority ? pill(data.decision.priority) : null),pre(data.reason),
      data.decision?.recipients?.length ? node("div",{ class:"clusters" },"Route",data.decision.recipients.map(id => actorLink(id))) : null];
  } else {
    body = pre(asText(data.text) || data.label || data.reason || "");
  }
  return node("article", { id: anchor ? "event-" + event.key : undefined, class: `entry${isSelected ? " selected" : ""}` },
    node("header", {}, actorLink(event.actor), pill(event.kind), pill(event.phase), link("#" + event.sequence, entryUrl(event.key)), time(event.timestamp_ms)), body);
}

function toolName(data) {
  return data.server ? `${data.server} / ${data.name ?? data.tool}` : data.tool || "Tool";
}

function toolTable(rows, route, page) {
  return table(["Time", "Tool", "Agent", "Path", "State", "Duration", "Conversation"], rows.map(event => {
    const call = event.data;
    return [time(event.timestamp_ms), link(toolName(call), entryUrl(event.key,page,route.query),{ class:"mono" }),
      actorLink(event.actor), call.path ? link(call.path,runUrl("tools",{ tab:"files",path:call.path }),{ class:"mono" }) : "—", pill(call.state),
      call.elapsed_ms === null || call.elapsed_ms === undefined ? "—" : `${call.elapsed_ms} ms`,
      link("Jump to chat", entryUrl(call.turn,"history",{ call:event.key }))];
  }));
}

function download(text, filename) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
  node("a", { href: url, download: filename }).click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

function payload(label, value, key) {
  const text = asText(value);
  let shown = text;
  if (label === "Arguments") {
    try { shown = JSON.stringify(JSON.parse(text), null, 2); } catch { /* Raw arguments still show malformed JSON. */ }
  }
  const preview = shown.length > 160_000 ? shown.slice(0,160_000) : shown;
  const output = pre(preview || "No payload recorded.");
  return node("details", { class: "payload", open: label === "Arguments" },
    node("summary", {}, label, value?.bytes === undefined ? "" : ` · ${number.format(value.bytes)} bytes`),
    value?.omitted ? muted(`${number.format(value.omitted)} bytes beyond the audit capture limit.`) : null,
    node("div", { class: "clusters" }, button("Copy", () => navigator.clipboard.writeText(shown).catch(captureError)),
      button("Download", () => download(shown,`ratio-${key}-${label.toLowerCase().replaceAll(" ","-")}.txt`)),
      preview.length !== shown.length ? button("Expand entire payload", event => { output.textContent = shown; event.currentTarget.remove(); }) : null),
    output);
}

function inspector(detail, close) {
  if (!detail) return panel("Inspector", muted("Select an entry to see its details."));
  const event = detail.event;
  const data = event.data;
  const outcome = detail.outcome?.data ?? data;
  let rows;
  if (event.kind === "tool" || event.kind === "tool_result") {
    rows = [node("div", { class: "clusters" }, actorLink(event.actor), pill(outcome.state), time(event.timestamp_ms)),
      node("div", { class: "clusters" }, code(toolName(data)), data.path ? code(data.path) : null,
        link("Jump to chat", entryUrl(data.turn, "history", { call: data.call ?? event.key }))),
      data.arguments ? payload("Arguments", data.arguments,event.key) : null,
      data.legacy ? muted("Historical checkpoint. Full tool result was not recorded.") : payload("Full result",outcome.full,event.key),
      payload("Model-visible result",outcome.output,event.key)];
  } else {
    rows = [historyCard(event,null,null,false), detail.origin ? link("Open original message",entryUrl(detail.origin.key,"ipc")) :
      event.kind === "ipc" && data.reply_to ? muted(`Reply addressed to ${actorName(data.reply_to)}`) : null,
      data.worker_request ? muted(`Onboarding request ${data.worker_request}`) : null,
      detail.deliveries?.length ? table(["Recipient","Delivery","Time"],detail.deliveries.map(item => [
        actorLink(item.recipient),link(title(item.state),entryUrl(item.key,"ipc")),time(item.timestamp_ms)])) : null];
  }
  return node("div", { class: "inspector" }, panel("Inspector", node("div", { class: "clusters" }, link("Close selection",close),
    button("Copy link", () => navigator.clipboard.writeText(location.href).catch(captureError))), rows));
}

function communication(rows, route, routes) {
  const query = route.query;
  if (query.tab === "matrix") {
    const ids = [...new Set(routes.flatMap(row => [row.from,row.to]))];
    const counts = new Map();
    for (const row of routes) {
      const key = row.from + ">" + row.to;
      const tally = counts.get(key) ?? { queued:0, delivered:0, dropped:0 };
      tally[row.state] = row.count;
      counts.set(key,tally);
    }
    return panel("Route matrix", table(["From / To", ...ids.map(actorName)],ids.map(from =>
      [actorLink(from), ...ids.map(to => {
        const tally = counts.get(from + ">" + to);
        return tally ? link(`${tally.delivered}/${tally.queued}/${tally.dropped}`, runUrl("ipc",{ ...query,tab:undefined,from,to }),{ title:"Delivered / queued / dropped" }) : "—";
      })]),"matrix"));
  }
  const selected = rows.filter(event => (!query.from || event.actor === query.from) && (!query.to || event.data.to?.includes(query.to)));
  if (query.tab === "lanes") {
    const agents = [...new Set(selected.flatMap(event => [event.actor,...(event.data.to ?? [])]))];
    return panel("Agent lanes",table(["Time",...agents.map(actorName),"Message"],[...selected].reverse().map(event => [
      time(event.timestamp_ms),...agents.map(agent => agent === event.actor ? pill("Sent")
        : event.data.to?.includes(agent) ? pill(event.data.delivery) : node("span",{ class:"lane-mark","aria-hidden":"true" },"·")),
      link(event.data.body,entryUrl(event.key,"ipc",query),{ class:"mono" }),
    ]),"matrix lanes"));
  }
  return panel("Message sequence", table(["Time","Sender","Recipients","State","BWP message"],selected.map(event => {
    const data = event.data;
    return [time(event.timestamp_ms),actorLink(event.actor),(data.to ?? []).map(id => node("div",{},actorLink(id))),
      pill(data.delivery),link(data.body,entryUrl(event.key,"ipc",query),{ class:"mono" })];
  }),"sequence"));
}

function tabs(page, options, current, query = {}) {
  return node("nav", { class: "tabs", "aria-label": "View options" }, options.map(([name, label]) =>
    link(label, runUrl(page, { ...query, tab: name, before: undefined, entry: undefined }), { "aria-current": current === name ? "page" : undefined })));
}

async function historyView(route) {
  const { page, actor, query } = route;
  const local = page === "agent" ? "agent/" + encodeURIComponent(actor) : page;
  const tab = query.tab ?? "all";
  const kind = page === "ipc" || (page === "agent" && tab === "ipc") ? "ipc" :
    page === "tools" ? tab === "mcp" ? "mcp" : "tool" :
      page === "agent" && tab === "tools" ? "tool" : page === "triage" ? "triage" : query.kind;
  const [historyData, detail] = await Promise.all([
    api(apiRun("history",{ ...query, actor:actor ?? query.actor, kind,
      from:actor && tab === "ipc" && query.direction === "outgoing" ? actor : query.from,
      to:actor && tab === "ipc" && query.direction === "incoming" ? actor : query.to,
      path:tab === "files" ? query.path ?? "*" : query.path })),
    query.entry ? api(apiRun("entry/" + encodeURIComponent(query.entry))).catch(() => null) : null,
  ]);
  let { rows } = historyData;
  const label = page === "agent" ? actorName(actor) : labels[page]?.[0] ?? "Timeline";
  const actions = page === "triage" ? [
    button("Submit information", () => openAction("triage"), { class: "accent", disabled: !state.live }),
    button("Request worker", () => openAction("add_worker"), { disabled: state.summary.run.phase !== "Work" || !state.live }),
  ] : page === "history" ? [button(state.follow ? "Live follow" : "Follow paused", () => {
    state.follow = !state.follow;
    load(false);
  }, { "aria-pressed": state.follow })] : [];
  const items = [heading(label, page === "agent" ? `Agent / ${actor}` : "Run observability", actions)];
  if (page === "agent") {
    const flow = state.summary.event?.workflow;
    const agent = flow?.agents?.find(entry => entry.id === actor);
    const task = flow?.tasks?.find(entry => entry.task.worker === actor);
    const match = /^(worker|reviewer|stakeholder)_(\d+)$/.exec(actor ?? "");
    const persona = match ? state.summary[match[1] + "s"]?.[Number(match[2])-1]?.persona : null;
    items.push(panel("Agent status", agentCard(agent ?? { id:actor,name:actor },task), persona ? pre(persona) : null),
      tabs(local, [["all","Conversation"],["ipc","Incoming & outgoing IPC"],["tools","Tools"]],tab,query));
    if (tab === "ipc") items.push(node("nav",{ class:"tabs","aria-label":"Message direction" },
      [["all","All messages"],["incoming","Incoming"],["outgoing","Outgoing"]].map(([direction, label]) =>
        link(label,runUrl(local,{ ...query,direction,entry:undefined,before:undefined }),
          { "aria-current":(query.direction ?? "all") === direction ? "page" : undefined }))));
  }
  if (page === "ipc") items.push(tabs(local,[["all","Sequence"],["lanes","Agent lanes"],["matrix","Route matrix"]],tab,query));
  if (page === "tools") items.push(tabs(local,[["all","All tools"],["mcp","MCP"],["files","File activity"]],tab,query));
  if (page === "triage" && state.summary.event?.workflow?.worker_addition) {
    const workflow = state.summary.event.workflow;
    const plan = state.summary.onboarding?.plan;
    items.push(panel("Worker admission", workflow.worker_addition,
      plan?.id ? code(plan.id) : null,
      workflow.active_worker_request ? link("Open onboarding IPC",runUrl("ipc",{ phase:"OnboardWorker",worker_request:workflow.active_worker_request })) : null,
      plan?.ownership ? node("details",{},node("summary",{},"Proposed write ownership"),pre(JSON.stringify(plan.ownership,null,2))) : null));
  }
  if (page === "triage") {
    const [inbox, admissions] = await Promise.all([api(apiRun("inbox",{ before:query.inbox_before })),api(apiRun("admissions"))]);
    items.push(panel("Worker requests",table(["Request","State","Amended plan","Communication"],admissions.rows.map(item =>
      [node("div",{},code(`#${item.id}`),pre(item.request),item.detail ? muted(item.detail) : null),pill(item.state),
        item.plan?.id ? node("details",{},node("summary",{},code(item.plan.id)),pre(JSON.stringify(item.plan.ownership,null,2))) : "—",
        link("Onboarding IPC",runUrl("ipc",{ phase:"OnboardWorker",worker_request:item.id }))]))));
    items.push(panel("Information inbox",...inbox.rows.map(item => node("article",{ class:`entry${Number(query.item) === item.id ? " selected" : ""}`,id:`inbox-${item.id}` },
      node("header",{},code(`#${item.id}`),pill(item.state),item.decision?.category ? pill(item.decision.category) : null,
        item.decision?.priority ? pill(item.decision.priority) : null,time(item.submitted_ms)),
      pre(item.text),
      item.decision?.reason ? muted(item.decision.reason) : null,
      item.decision?.recipients?.length ? node("div",{ class:"clusters" },"Route",item.decision.recipients.map(id => actorLink(id))) : null,
      item.delivered?.length ? node("div",{ class:"clusters" },"Delivered to",item.delivered.map(id => actorLink(id))) : null,
      item.decision?.worker_recommendation ? node("div",{ class:"clusters" },muted(item.decision.worker_recommendation),
        button("Use in worker request",() => openAction("add_worker",item.decision.worker_recommendation),{ disabled:!state.live || state.summary.run.phase !== "Work" })) : null,
      item.state === "failed" ? button("Retry triage",() => sendAction("retry_triage",String(item.id)).catch(captureError),{ disabled:!state.live }) : null)),
      !inbox.rows.length ? muted("No information items have been submitted.") : null,
      inbox.rows.length >= 60 ? link("Older information",runUrl("triage",{ inbox_before:inbox.rows.at(-1).id })) : null));
    return items;
  }
  if (page === "history") items.push(node("nav",{ class:"tabs","aria-label":"History sources" },
    link("Checkpoint archive",runUrl("archive")),
    link(query.legacy ? "Audit timeline" : "Recoverable checkpoint entries",runUrl("history",{ legacy:query.legacy ? undefined : 1 }))));
  if (historyData.legacy) items.push(muted("Showing recoverable checkpoint entries. Coverage depends on what the older run retained."));
  if (query.path && tab === "files") items.push(node("div",{ class:"clusters" },pill(query.path),link("All files",runUrl("tools",{ tab:"files" }))));
  if (page === "ipc" && (query.from || query.to || query.worker_request)) items.push(node("div",{ class:"clusters" },
    query.from ? pill("From " + actorName(query.from)) : null,query.to ? pill("To " + actorName(query.to)) : null,
    query.worker_request ? pill("Worker request " + query.worker_request) : null,link("Clear route",runUrl("ipc",{ tab:query.tab }))));
  items.push(filters(route,{ actor:page !== "agent",kind:page === "history" }));
  if (detail && page === "history" && !rows.some(row => row.key === detail.event.key)) rows = [detail.event,...rows];
  let content;
  if (page === "ipc") content = communication(rows,route,query.tab === "matrix" ? (await api(apiRun("routes",query))).rows : []);
  else if (page === "tools" || tab === "tools") content = panel(tab === "mcp" ? "MCP calls" : "Tool calls",toolTable(rows,route,local));
  else content = node("section",{ class:"timeline","aria-label":"Recorded timeline" },rows.length
    ? rows.map(event => historyCard(event,query.entry,query.call))
    : [node("p",{ class:"empty" },"No matching entries.")]);
  items.push(detail || page === "ipc" || page === "tools"
    ? node("div",{ class:"split" },content,inspector(detail,runUrl(local,{ ...query,entry:undefined }))) : content,
    historyData.legacy
      ? node("nav",{ class:"pagination","aria-label":"Checkpoint history pagination" },
        query.before ? link("Latest",runUrl(local,{ ...query,before:undefined,entry:undefined })) : null,
        historyData.next_before ? link("Older checkpoints",runUrl(local,{ ...query,before:historyData.next_before,entry:undefined })) : null)
      : pager(historyData.rows,local,query));
  return items;
}

async function archive(route) {
  const rows = (await api(apiRun("checkpoints",{ after:route.query.after }))).rows;
  const snapshot = route.query.sequence ? await api(apiRun("checkpoint",{ sequence:route.query.sequence })) : null;
  return [heading("Checkpoint archive","Run history",link("Timeline",runUrl("history"))),
    muted("Historical snapshots contain the conversation still retained at each checkpoint. Older runs may have gaps."),
    node("div",{ class:"split" },panel("Checkpoints",table(["Sequence","Phase","Cycle","Time"],rows.map(row =>
      [link(row.sequence,runUrl("archive",{ after:route.query.after,sequence:row.sequence })),title(row.phase),row.cycle,time(row.timestamp_ms)]))),
    snapshot ? panel("Retained conversations",...snapshot.dialogs.map(dialog =>
      node("details",{},node("summary",{},actorName(dialog.actor)),dialog.task ? pre(dialog.task) : null,
        dialog.assessment ? pre(dialog.assessment) : null,pre(JSON.stringify(dialog.groups ?? [],null,2))))) : null),
    rows.length === 80 ? link("More checkpoints",runUrl("archive",{ after:rows.at(-1).sequence })) : null];
}

function hasPageInteraction() {
  return $("#action-dialog").open || main.contains(document.activeElement) || $("details[open]", main);
}

async function load(restart = true, preserveInteraction = false) {
  const version = ++state.version;
  const previous = state.route.run;
  state.route = route();
  const wanted = state.route;
  if (restart) {
    state.refreshPending = false;
    state.stream?.abort();
    state.stream = null;
    main.inert = true;
    if (previous !== wanted.run) { state.summary = null; state.live = null; }
  }
  main.setAttribute("aria-busy","true");
  try {
    const [runs, summary, live] = await Promise.all([
      api("/api/runs"),
      wanted.run ? api(apiRun("overview")) : null,
      wanted.run ? api(apiRun("live")).catch(() => null) : null,
    ]);
    if (version !== state.version) return;
    const roster = summary?.event?.workflow?.agents ?? [];
    const previousRoster = state.summary?.event?.workflow?.agents ?? [];
    const runLabels = runs => runs.map(({run_id,phase}) => `${run_id}/${phase}`).join(" ");
    const navigationChanged = JSON.stringify(roster) !== JSON.stringify(previousRoster)
      || runLabels(state.runs) !== runLabels(runs.runs);
    state.runs = runs.runs;
    state.summary = summary;
    state.live = live;
    if (restart || navigationChanged || !wanted.run) renderNavigation();
    else renderLive();
    const nodes = !wanted.run ? workspace() : wanted.page === "overview" ? overview()
      : wanted.page === "agents" ? [heading("Agents","Run observability"),
        panel("Roster",node("div",{ class:"agent-grid" },(summary.event?.workflow?.agents ?? []).filter(agent => agent.id !== "user").map(agent => agentCard(agent))))]
      : wanted.page === "archive" ? await archive(wanted) : await historyView(wanted);
    if (version !== state.version) return;
    if (preserveInteraction && hasPageInteraction()) {
      state.refreshPending = true;
      return;
    }
    main.replaceChildren(...nodes);
    if (restart && wanted.query.entry) {
      document.getElementById("event-" + wanted.query.entry)?.scrollIntoView({ block:"center" });
    }
    if (restart && wanted.page === "triage" && wanted.query.item) {
      document.getElementById("inbox-" + wanted.query.item)?.scrollIntoView({ block:"center" });
    }
    if (wanted.run && !state.stream) connect(wanted.run,summary);
  } catch (error) {
    if (version === state.version) main.replaceChildren(heading("Run unavailable","Ratio"),panel("Connection",muted(error.message),link("Workspace runs","/")));
  } finally {
    if (version === state.version) {
      main.inert = false;
      main.setAttribute("aria-busy","false");
    }
  }
}

async function connect(run, summary) {
  const controller = new AbortController();
  state.stream = controller;
  let checkpoint = summary.run.last_sequence;
  let after = summary.head;
  while (!controller.signal.aborted) {
    try {
      const stream = await fetch(`/api/run/${encodeURIComponent(run)}/stream?checkpoint=${checkpoint}&after=${after}`,
        { headers:{ Authorization:"Bearer " + token },signal:controller.signal });
      if (!stream.ok || !stream.body) break;
      const reader = stream.body.pipeThrough(new TextDecoderStream()).getReader();
      let pending = "";
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        pending += chunk.value;
        let boundary;
        while ((boundary = pending.indexOf("\n\n")) >= 0) {
          const packet = pending.slice(0,boundary);
          pending = pending.slice(boundary+2);
          const line = packet.split("\n").find(line => line.startsWith("data: "));
          if (!line) continue;
          const update = JSON.parse(line.slice(6));
          checkpoint = update.checkpoint;
          after = update.cursor;
          if (state.route.run === run) {
            state.refreshPending = true;
            refreshLivePage();
          }
        }
      }
    } catch (error) {
      if (controller.signal.aborted) break;
    }
    if (!controller.signal.aborted) await new Promise(resolve => setTimeout(resolve,1200));
  }
}

function refreshLivePage() {
  const { page, query } = state.route;
  if (!state.refreshPending || (page === "history" && !state.follow) || query.before || query.entry
    || hasPageInteraction()) return;
  state.refreshPending = false;
  load(false, true);
}

let actionState;
const descriptions = {
  start:["Start a run","Describe the outcome and constraints. Ratio will propose a roster and start the run.","Request",true],
  fork:["Fork this run","Start a new run with this roster. Leave the request empty to keep the original goal.","Replacement request",false],
  resume_saved:["Resume saved run","Continue from the latest committed checkpoint.","",false],
  add_worker:["Request a worker","Describe the role and scope. Writes remain gated until unanimous acknowledgement.","Assignment and proposed scope",true],
  triage:["Submit information","The lead reviewer will classify and route this item automatically.","Information",true],
  stop:["Stop this run","Cancel active work and retain the last checkpoint.","",false],
};

function openAction(name, initial = "") {
  const [label,hint,field,required] = descriptions[name];
  actionState = { name, run:state.route.run, id:crypto.randomUUID() };
  $("#action-title").textContent = label;
  $("#action-hint").textContent = hint;
  $("#action-label").textContent = field;
  $("#action-label").hidden = !field;
  const input = $("#action-text");
  input.value = initial;
  input.hidden = !field;
  input.required = required;
  input.maxLength = name === "start" || name === "fork" ? 65536 : 4096;
  $("#submit-action").textContent = label;
  $(".action-error").hidden = true;
  $("#action-dialog").showModal();
  (field ? input : $("#submit-action")).focus();
}

async function sendAction(action, text, run = state.route.run, id = crypto.randomUUID()) {
  const receipt = await api("/api/action",{ method:"POST",headers:{ "Content-Type":"application/json" },
    body:JSON.stringify({ action,text,run,id }) });
  $("#notice").hidden = false;
  $("#notice").textContent = receipt.detail;
  if (receipt.run_id && receipt.run_id !== state.route.run) navigate(runUrl("overview",{},receipt.run_id));
  else load(false);
}

document.addEventListener("click", event => {
  if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  const anchor = event.target.closest("a[href]");
  const href = anchor?.getAttribute("href");
  if (href === "/" || href?.startsWith("/run/")) {
    event.preventDefault();
    $(".sidebar").classList.remove("open");
    $("#menu").setAttribute("aria-expanded","false");
    navigate(href);
  }
});
window.addEventListener("popstate",() => load());
$("#menu").addEventListener("click",() => {
  const open = $(".sidebar").classList.toggle("open");
  $("#menu").setAttribute("aria-expanded",String(open));
});
$("#new-run").addEventListener("click",() => openAction("start"));
$("#run-select").addEventListener("change",event => navigate(runUrl("overview",{},event.target.value)));
$("#global-search").addEventListener("submit",event => {
  event.preventDefault();
  navigate(runUrl("history",{ q:new FormData(event.target).get("q") }));
});
$("#close-action").addEventListener("click",() => $("#action-dialog").close());
$("#action-form").addEventListener("submit",async event => {
  event.preventDefault();
  const submit = $("#submit-action");
  submit.disabled = true;
  try {
    await sendAction(actionState.name,$("#action-text").value,actionState.run,actionState.id);
    $("#action-dialog").close();
  } catch (error) {
    $(".action-error").textContent = error.message;
    $(".action-error").hidden = false;
  } finally { submit.disabled = false; }
});
document.addEventListener("keydown",event => {
  if (event.key === "/" && !$("#action-dialog").open && !event.target.matches("input, textarea, select")) {
    event.preventDefault();
    $("#global-search input").focus();
  }
});
setInterval(async () => {
  const run = state.route.run;
  if (!run) return;
  const value = await api(apiRun("live")).catch(() => null);
  if (run === state.route.run) {
    state.live = value;
    renderLive();
    if (state.route.page === "triage") state.refreshPending = true;
    refreshLivePage();
  }
},1400);

load();
