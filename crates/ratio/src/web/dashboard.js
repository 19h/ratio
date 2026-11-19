"use strict";

const $ = (selector, scope = document) => scope.querySelector(selector);
const main = $("#content");
const launch = new URLSearchParams(location.hash.slice(1)).get("access");
if (launch && /^[a-f0-9]{64}$/.test(launch)) {
  sessionStorage.setItem("ratio-access", launch);
  history.replaceState(null, "", location.pathname + location.search);
}
const token = sessionStorage.getItem("ratio-access");
const views = {
  overview: "Overview", history: "Timeline", agents: "Agents",
  ipc: "Communication", tools: "Tools & files", triage: "Triage",
};
const stages = [
  ["Team", ["ProposeTeam"]],
  ["Plan", ["Plan", "PlanConsult", "PlanSynthesize", "Coordinate", "FreezePlan", "Acknowledge"]],
  ["Work", ["Work"]],
  ["Review", ["Review", "ReviewConsult", "ReviewSynthesize"]],
  ["Publish", ["Publish"]],
];
const endings = ["Approved", "Rejected", "Exhausted"];
const state = { runs: [], summary: null, live: null, route: {}, version: 0, stream: null, follow: true, refreshPending: false };
const number = new Intl.NumberFormat();
const clock = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23" });

function children(items) {
  return items.flat(Infinity).filter(child => child !== null && child !== undefined && child !== false)
    .map(child => child instanceof Node ? child : String(child));
}

function node(tag, props = {}, ...content) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") element.className = value;
    else if (key.startsWith("on")) element.addEventListener(key.slice(2), value);
    else element.setAttribute(key, value === true ? "" : value);
  }
  element.append(...children(content));
  return element;
}

function fill(target, ...content) { target.replaceChildren(...children(content)); }
function link(text, href, props = {}) { return node("a", { href, ...props }, text); }
function go(text, href, props = {}) { return link(text, href, { class: "go", ...props }); }
function button(text, action, props = {}) { return node("button", { type: "button", onclick: action, ...props }, text); }
function code(text) { return node("code", {}, text ?? ""); }
function pre(text, props = {}) { return node("pre", props, text ?? ""); }
function muted(text) { return node("p", { class: "muted" }, text); }
function empty(text) { return node("p", { class: "empty" }, text); }
function caption(text) { return node("span", { class: "label" }, text); }
function hush(text) { return node("span", { "aria-hidden": "true" }, text); }
function spacer() { return node("span", { class: "spacer" }); }
function row(...content) { return node("div", { class: "row" }, content); }
function pill(value) { return node("span", { class: "pill", "data-state": String(value).toLowerCase() }, title(value)); }
function tag(value, kind = String(value).toLowerCase()) { return node("span", { class: "tag", "data-kind": kind }, title(value)); }
function title(value) { return String(value ?? "").replaceAll("_", " ").replace(/([a-z])([A-Z])/g, "$1 $2").replace(/^./, letter => letter.toUpperCase()); }
function asText(value) { return typeof value === "string" ? value : value?.text ?? ""; }
function time(ms) { return ms ? node("time", { datetime: new Date(ms).toISOString(), title: new Date(ms).toLocaleString() }, clock.format(ms)) : "—"; }

function facts(rows) {
  const shown = rows.filter(entry => entry && entry[1] !== null && entry[1] !== undefined && entry[1] !== false);
  return shown.length ? node("dl", { class: "kv" }, shown.map(([name, value]) => [node("dt", {}, name), node("dd", {}, value)])) : null;
}

function section(head, ...content) {
  const { label: text, meta, flush } = typeof head === "string" ? { label: head } : head;
  return node("section", { class: "panel" },
    node("header", { class: "panel-head" }, node("h2", {}, hush("[ "), text, hush(" ]")),
      meta === undefined || meta === null ? null : node("span", { class: "muted" }, meta)),
    flush ? content : node("div", { class: "panel-body" }, content));
}

function heading(text, context, ...actions) {
  return node("header", { class: "bar view-head" }, node("h1", {}, text),
    context ? node("span", { class: "context muted" }, context) : null, spacer(), actions);
}

function view(head, body, { aside = null, flush = false, follow = false } = {}) {
  return [head, node("div", { class: "view-body" },
    node("div", { class: `view-scroll ${flush ? "flush" : "stack"}`, "data-scroll": "body", "data-follow": follow ? "end" : undefined }, body), aside)];
}

function table(columns, rows, className = "") {
  if (!rows.length) return empty("No matching entries.");
  return node("div", { class: "table-wrap" }, node("table", { class: className },
    node("thead", {}, node("tr", {}, columns.map(text => node("th", { scope: "col" }, text)))),
    node("tbody", {}, rows.map(cells => node("tr", { class: cells.selected ? "selected" : undefined }, cells.map(value =>
      value?.cell === undefined ? node("td", {}, value) : node("td", { class: value.class }, value.cell)))))));
}

function runUrl(page = "overview", query = {}, id = state.route.run) {
  const params = new URLSearchParams(Object.entries(query).filter(([, value]) => value !== undefined && value !== null && value !== ""));
  return id ? `/run/${encodeURIComponent(id)}/${page}${params.size ? "?" + params : ""}` : "/";
}
function apiRun(view, query = {}) {
  const params = new URLSearchParams(Object.entries(query).filter(([, value]) => value !== undefined && value !== null && value !== ""));
  return `/api/run/${encodeURIComponent(state.route.run)}/${view}${params.size ? "?" + params : ""}`;
}
function roster() { return (state.summary?.event?.workflow?.agents ?? []).filter(agent => agent.id !== "user"); }
function actorName(id) { return state.summary?.event?.workflow?.agents?.find(agent => agent.id === id)?.name ?? id; }
function actorLink(id, label = actorName(id)) {
  return id && id !== "user" ? link(label, runUrl("agent/" + encodeURIComponent(id))) : (label || "Ratio");
}
function entryUrl(key, page = "history", extra = {}) {
  return runUrl(page, { ...extra, entry: key });
}

let noticeTimer;
function notify(text, tone = "receipt") {
  const notice = $("#notice");
  clearTimeout(noticeTimer);
  $("span", notice).textContent = text;
  notice.dataset.tone = tone;
  notice.hidden = false;
  if (tone !== "error") noticeTimer = setTimeout(() => { notice.hidden = true; }, 8000);
}
function captureError(error) { notify(error.message, "error"); }

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

function navigationKey(runs, summary) {
  const flow = summary?.event?.workflow;
  return JSON.stringify([runs.map(({ run_id, phase }) => [run_id, phase]), flow?.agents ?? [],
    (flow?.tasks ?? []).map(entry => [entry.task.worker, entry.state])]);
}

function renderNavigation() {
  const selected = state.route.run;
  const current = state.route.page;
  const select = $("#run-select");
  fill(select, node("option", { value: "" }, "All runs"),
    state.runs.map(run => node("option", { value: run.run_id }, `${title(run.phase)} · ${run.run_id.slice(-10)}`)));
  select.value = selected ?? "";
  $("#navigation").hidden = !selected;
  fill($("#navigation"), Object.entries(views).map(([page, name], index) =>
    link([name, node("kbd", { "aria-hidden": "true" }, index + 1)], runUrl(page), {
      "aria-current": selected && (current === page || (page === "history" && current === "archive")) ? "page" : undefined,
    })));
  const agents = roster();
  const tasks = state.summary?.event?.workflow?.tasks ?? [];
  $("#agent-heading").hidden = !selected || !agents.length;
  $("#agent-count").textContent = agents.length;
  fill($("#agent-nav"), agents.map(agent => {
    const task = tasks.find(entry => entry.task.worker === agent.id);
    return link([node("span", { class: `dot${generation(agent.id) ? " live" : ""}`, "data-agent-dot": agent.id }),
      node("span", { class: "who" }, node("span", { class: "truncate" }, agent.name), node("span", {}, node("small", {}, agent.id),
        task ? node("small", { class: "task-state", "data-state": task.state }, title(task.state)) : null))],
    runUrl("agent/" + encodeURIComponent(agent.id)), { "aria-current": state.route.actor === agent.id ? "page" : undefined });
  }));
  const here = !selected ? "Runs" : current === "agent" ? actorName(state.route.actor)
    : current === "archive" ? "Checkpoint archive" : views[current] ?? "Run";
  const slash = () => node("span", { class: "slash", "aria-hidden": "true" }, "/");
  fill($("#breadcrumb"), link(node("strong", {}, "ratio"), "/"), slash(),
    selected ? [link(node("strong", {}, selected.slice(-10)), runUrl("overview"), { title: "Run " + selected }), slash()] : null,
    node("strong", { class: "here" }, here));
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
    button("Stop", () => openAction("stop"), { class: "danger" }),
  ] : state.route.run && state.summary ? [
    button("Resume saved", () => openAction("resume_saved"), { disabled: endings.includes(state.summary.run.phase) }),
    button("Fork", () => openAction("fork")),
  ] : [];
  if (controls.dataset.mode !== status || controls.dataset.phase !== state.summary?.run?.phase) {
    fill(controls, actions);
    controls.dataset.mode = status;
    controls.dataset.phase = state.summary?.run?.phase ?? "";
  }
  for (const card of document.querySelectorAll("[data-preview-actor]")) {
    const active = generation(card.dataset.previewActor);
    card.hidden = !active;
    card.textContent = active?.text || (active ? active.label : "");
  }
  for (const dot of document.querySelectorAll("[data-agent-dot]")) dot.classList.toggle("live", Boolean(generation(dot.dataset.agentDot)));
  renderStatus();
}

function renderStatus() {
  const run = state.route.run ? state.summary?.run : null;
  const metrics = state.summary?.metrics ?? {};
  const usage = state.live?.token_usage;
  const breakdown = usage?.breakdown ?? metrics.token_breakdown;
  const tokens = usage?.tokens ?? metrics.metered_tokens ?? 0;
  const rate = usage?.output_tokens_per_second ?? 0;
  const cell = (text, name) => node("span", { class: name }, text);
  const tone = !run ? "idle" : endings.includes(run.phase) ? run.phase.toLowerCase() : state.live?.paused ? "paused" : "active";
  const mode = node("span", { class: "status-mode", "data-tone": tone }, run ? title(run.phase) : "Workspace");
  fill($("#status"), mode, run ? [
    cell(`cycle ${run.cycle}`), cell(`run ${run.run_id}`, "wide"),
    spacer(),
    node("span", { "data-token-usage": tokens, title: usage?.estimated ? "Includes estimated in-flight tokens" : "Metered tokens" },
      `${usage?.estimated ? "~" : ""}${number.format(tokens)} tokens`),
    tokenFlow("input", "↑", "Outgoing input tokens, including cache hits", breakdown?.input),
    tokenFlow("cached_input", "↑", "Cached outgoing input tokens", breakdown?.cached_input),
    tokenFlow("output", "↓", "Incoming output tokens", breakdown?.output),
    usage ? node("span", { "data-output-tps": rate, title: "Estimated streamed output tokens per second over the last 2 seconds" },
      `${rate ? "~" : ""}${number.format(rate)} tok/s`) : null,
    cell(`${number.format(metrics.tool_receipts ?? 0)} tool calls`, "wide"),
    cell(`${number.format(metrics.ipc_deliveries ?? 0)} deliveries`, "wide"),
    cell([node("kbd", {}, "/"), " search ", node("kbd", {}, "1–6"), " views"], "keys"),
  ] : [cell(`${state.runs.length} runs`), spacer()]);
}

function tokenFlow(kind, arrow, label, count) {
  const known = count?.tokens != null;
  const text = known ? `${count.estimated ? "~" : ""}${number.format(count.tokens)}${count.partial ? "+" : ""}` : "—";
  const value = known ? `${count.estimated ? "approximately " : ""}${number.format(count.tokens)}${count.partial ? " with some requests missing data" : ""}` : "unavailable";
  const description = `${label} · ${value}`;
  return node("span", { class: "token-flow", "data-token-flow": kind, "data-token-count": count?.tokens, title: description, "aria-label": description },
    node("span", { class: "token-arrow", "aria-hidden": "true" }, arrow,
      kind === "cached_input" ? node("span", { class: "token-cache" }) : null),
    text);
}

function workspace() {
  if (!state.runs.length) {
    return view(heading("Runs"), node("button", { type: "button", class: "empty-drop", onclick: () => openAction("start") },
      node("strong", {}, "Start a run"),
      node("span", {}, "Describe the outcome and its constraints. Ratio proposes a team, plans the work, and records every step here.")));
  }
  return view(heading("Runs", `${state.runs.length} in this workspace`),
    section({ label: "Run history", flush: true }, table(["Run", "Phase", "Cycle", "Last checkpoint"], state.runs.map(run =>
      [link(run.run_id, runUrl("overview", {}, run.run_id)), pill(run.phase), run.cycle, code(run.last_sequence)]))));
}

function agentRow(agent, task, describe = true) {
  const active = generation(agent.id);
  return node("article", { class: "agent" },
    node("span", { class: `dot${active ? " live" : ""}`, "data-agent-dot": agent.id }),
    node("div", { class: "who" }, actorLink(agent.id, agent.name), code(agent.id)),
    task ? pill(task.state) : node("span"),
    describe && task?.task ? node("p", {}, task.task.description) : null,
    pre(active?.text || "", { class: "preview", hidden: !active, "data-preview-actor": agent.id }));
}

function phaseRail(phase) {
  const order = stages.flatMap(([, steps]) => steps);
  const done = endings.includes(phase);
  const at = order.indexOf(phase === "OnboardWorker" ? "Work" : phase);
  return node("ol", { class: "rail", "aria-label": "Run phases", "data-end": done ? phase.toLowerCase() : undefined },
    stages.map(([name, steps]) => {
      const current = !done && steps.includes(order[at]);
      return node("li", { class: `g${steps.length}${current ? " on" : ""}`, "aria-current": current ? "step" : undefined },
        node("div", { class: "steps" }, steps.map(step => {
          const index = order.indexOf(step);
          return node("i", { "data-step": done || index < at ? "past" : index === at ? "now" : "next", title: title(step) });
        })),
        node("span", {}, name));
    }));
}

function overview() {
  const data = state.summary;
  const flow = data.event?.workflow ?? {};
  const metrics = data.metrics ?? {};
  const tasks = flow.tasks ?? [];
  const agents = roster();
  const phase = data.run.phase;
  const acknowledged = flow.acknowledgements ?? [];
  return view(heading("Run overview", code(data.run.run_id),
    data.parent_run ? go("Parent run", runUrl("overview", {}, data.parent_run)) : null), [
    node("section", { class: "band" },
      node("div", { class: "hero" }, caption("Goal"), node("p", { class: "goal" }, data.goal || "Waiting for a goal"),
        data.request && data.request !== data.goal ? node("details", {}, node("summary", {}, "Original request"), pre(data.request)) : null),
      node("div", { class: "hero-field" }, caption("Phase"),
        node("strong", { class: "hero-title", "data-tone": endings.includes(phase) ? phase.toLowerCase() : "active" }, title(phase)),
        node("span", { class: "hero-sub" }, `cycle ${data.run.cycle}`, flow.stopped ? pill(flow.stopped) : null)),
      phaseRail(phase)),
    node("div", { class: "columns" },
      node("div", { class: "col" },
        section({ label: "Agents", meta: agents.length || null, flush: true }, agents.length
          ? agents.map(agent => agentRow(agent, tasks.find(task => task.task.worker === agent.id), false))
          : empty("The roster appears once a team is proposed.")),
        section({ label: "Execution plan", meta: flow.plan_id ? code(flow.plan_id) : null, flush: true },
          tasks.length ? table(["Task", "Worker", "State", "Depends on"], tasks.map(({ task, state: status }) =>
            [task.description, actorLink(task.worker), pill(status), task.depends_on.length ? code(task.depends_on.join(", ")) : "—"]))
            : empty("Waiting for a frozen plan."),
          (data.acceptance ?? []).length ? node("details", {}, node("summary", {}, "Acceptance criteria"), node("ul", {}, data.acceptance.map(text => node("li", {}, text)))) : null,
          data.plan?.ownership ? node("details", {}, node("summary", {}, "Write ownership"),
            table(["Worker", "Paths"], data.plan.ownership.map((paths, index) => [actorLink(`worker_${index + 1}`), paths.map(path => code(path.path + (path.directory ? "/" : "")))]))) : null)),
      node("div", { class: "col" },
        section("Run state", facts([
          ["Acknowledged", acknowledged.length ? acknowledged.map(id => actorLink(id)) : "None yet"],
          ["Worker request", flow.worker_addition ? go(flow.worker_addition, runUrl("triage")) : "None yet"],
          ["Verification", flow.verification ? [pill(flow.verification.passed ? "completed" : "failed"), code(flow.verification.candidate)] : "No receipt yet"],
          ["Verdict", data.verdict ? pill(data.verdict.verdict) : null],
          ["Assessment", data.assessment ? pre(data.assessment) : null],
          ["Failure", data.latest_error ? [pre(data.latest_error.data.text), go("Open failure in timeline", entryUrl(data.latest_error.key))] : null],
        ])),
        section("Usage", facts(Object.entries(metrics).map(([name, value]) => [title(name), number.format(value)])) ?? muted("Nothing metered yet.")))),
  ]);
}

function agentsView() {
  const tasks = state.summary.event?.workflow?.tasks ?? [];
  const agents = roster();
  return view(heading("Agents"), section({ label: "Roster", meta: agents.length || null, flush: true }, agents.length
    ? agents.map(agent => agentRow(agent, tasks.find(task => task.task.worker === agent.id)))
    : empty("The roster appears once a team is proposed.")));
}

function filters(route, { actor = true, kind = false } = {}) {
  const query = route.query;
  const form = node("form", { class: "filters", onsubmit: event => {
    event.preventDefault();
    navigate(runUrl(location.pathname.split("/").slice(3).join("/"), {
      tab: query.tab, legacy: query.legacy, worker_request: query.worker_request, path: query.path,
      from: query.from, to: query.to, direction: query.direction, ...Object.fromEntries(new FormData(event.currentTarget)),
    }));
  } });
  const select = (name, label, options) => {
    const element = node("select", { name, "aria-label": label },
      node("option", { value: "" }, label), options.map(([value, text]) => node("option", { value }, text)));
    element.value = query[name] ?? "";
    return element;
  };
  form.append(node("input", { type: "search", name: "q", value: query.q ?? "", placeholder: "Search entries", "aria-label": "Search entries", spellcheck: "false" }),
    ...(actor ? [select("actor", "All agents", (state.summary?.event?.workflow?.agents ?? []).map(agent => [agent.id, agent.name]))] : []),
    ...(kind ? [select("kind", "All event types", ["chat", "tool", "tool_result", "ipc", "phase", "brief", "proposal", "guidance", "triage", "triage_delivery", "compaction", "verification", "contract_error", "error", "control"].map(type => [type, title(type)]))] : []),
    node("input", { type: "number", min: "1", name: "cycle", value: query.cycle ?? "", placeholder: "Cycle", "aria-label": "Cycle" }),
    select("phase", "All phases", ["ProposeTeam", "Plan", "PlanConsult", "PlanSynthesize", "Coordinate", "FreezePlan", "Acknowledge", "Work", "OnboardWorker", "Review", "ReviewConsult", "ReviewSynthesize", "Publish"].map(phase => [phase, title(phase)])),
    node("button", { type: "submit" }, "Apply"));
  return form;
}

function pagination(label, ...links) {
  const shown = links.filter(Boolean);
  return shown.length ? node("nav", { class: "pagination", "aria-label": label }, shown) : null;
}

function historyCard(event, selected, call, anchor = true) {
  const data = event.data ?? {};
  let body;
  if (event.kind === "chat") {
    body = [asText(data.text) ? pre(asText(data.text)) : null,
      (data.calls ?? []).length ? node("div", { class: "call-list" }, data.calls.map(item =>
        link(item.tool, entryUrl(item.key), { class: call === item.key ? "highlight" : undefined, "data-call": item.key }))) : null];
  } else if (event.kind === "tool" || event.kind === "tool_result") {
    body = row(link(toolName(data), entryUrl(data.call ?? event.key)), pill(data.state), data.path ? code(data.path) : null,
      data.turn ? go("Jump to chat", entryUrl(data.turn, "history", { call: data.call ?? event.key })) : null);
  } else if (event.kind === "ipc") {
    body = [row(caption("To"), (data.to ?? []).map(id => actorLink(id)), pill(data.delivery)), pre(data.body)];
  } else if (event.kind === "triage") {
    body = [row(go(`Information #${data.item}`, runUrl("triage", { item: data.item, inbox_before: data.item + 1 })),
      pill(data.state), data.decision?.priority ? pill(data.decision.priority) : null), pre(data.reason),
    data.decision?.recipients?.length ? row(caption("Route"), data.decision.recipients.map(id => actorLink(id))) : null];
  } else {
    body = pre(asText(data.text) || data.label || data.reason || "");
  }
  return node("article", { id: anchor ? "event-" + event.key : undefined, class: `entry${event.key === selected ? " selected" : ""}`, "data-kind": event.kind },
    node("header", {}, time(event.timestamp_ms), node("span", { class: "truncate" }, actorLink(event.actor)), tag(event.kind)),
    node("div", { class: "entry-body" }, body),
    node("footer", {}, tag(event.phase, "phase"), link("#" + event.sequence, entryUrl(event.key))));
}

function toolName(data) {
  return data.server ? `${data.server} / ${data.name ?? data.tool}` : data.tool || "Tool";
}

function toolTable(rows, route, page) {
  return table(["Time", "Tool", "Agent", "Path", "State", "Duration", "Conversation"], rows.map(event => {
    const call = event.data;
    const cells = [time(event.timestamp_ms), link(toolName(call), entryUrl(event.key, page, route.query)),
      actorLink(event.actor), call.path ? link(call.path, runUrl("tools", { tab: "files", path: call.path })) : "—", pill(call.state),
      call.elapsed_ms === null || call.elapsed_ms === undefined ? "—" : `${call.elapsed_ms} ms`,
      go("Jump to chat", entryUrl(call.turn, "history", { call: event.key }))];
    cells.selected = event.key === route.query.entry;
    return cells;
  }), "tools");
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
  const preview = shown.length > 160_000 ? shown.slice(0, 160_000) : shown;
  const output = pre(preview || "No payload recorded.");
  return node("details", { class: "payload", open: label === "Arguments" },
    node("summary", {}, label, value?.bytes === undefined ? null : node("span", { class: "muted" }, ` · ${number.format(value.bytes)} bytes`)),
    node("div", { class: "payload-body" },
      value?.omitted ? muted(`${number.format(value.omitted)} bytes beyond the audit capture limit.`) : null,
      row(button("Copy", () => navigator.clipboard.writeText(shown).catch(captureError)),
        button("Download", () => download(shown, `ratio-${key}-${label.toLowerCase().replaceAll(" ", "-")}.txt`)),
        preview.length !== shown.length ? button("Expand entire payload", event => { output.textContent = shown; event.currentTarget.remove(); }) : null),
      output));
}

function inspector(detail, close) {
  if (!detail) return null;
  const event = detail.event;
  const data = event.data;
  const outcome = detail.outcome?.data ?? data;
  let rows;
  if (event.kind === "ipc") {
    rows = messageInspector(detail);
  } else if (event.kind === "tool" || event.kind === "tool_result") {
    rows = [node("div", { class: "inspector-section" }, facts([
      ["Tool", code(toolName(data))], ["Agent", actorLink(event.actor)], ["State", pill(outcome.state)],
      ["Time", time(event.timestamp_ms)], ["Path", data.path ? code(data.path) : null]]),
    go("Jump to chat", entryUrl(data.turn, "history", { call: data.call ?? event.key }))),
    data.arguments ? payload("Arguments", data.arguments, event.key) : null,
    data.legacy ? node("div", { class: "inspector-section" }, muted("Historical checkpoint. Full tool result was not recorded.")) : payload("Full result", outcome.full, event.key),
    payload("Model-visible result", outcome.output, event.key)];
  } else {
    rows = [historyCard(event, null, null, false), node("div", { class: "inspector-section" },
      detail.origin ? go("Open original message", entryUrl(detail.origin.key, "ipc")) :
        event.kind === "ipc" && data.reply_to ? muted(`Reply addressed to ${actorName(data.reply_to)}`) : null,
      data.worker_request ? muted(`Onboarding request ${data.worker_request}`) : null),
    detail.deliveries?.length ? table(["Recipient", "Delivery", "Time"], detail.deliveries.map(item => [
      actorLink(item.recipient), link(title(item.state), entryUrl(item.key, "ipc")), time(item.timestamp_ms)])) : null];
  }
  return node("aside", { class: "inspector", "aria-label": "Inspector", "data-scroll": "inspector" },
    node("header", { class: "bar" }, node("h2", {}, hush("[ "), event.kind === "ipc" ? "Message inspector" : "Inspector", hush(" ]")), spacer(),
      close ? [button("Copy link", () => navigator.clipboard.writeText(location.href).catch(captureError)),
        go("×", close, { "aria-label": "Close selection", title: "Close selection" })] : node("span", { class: "muted" }, "latest")),
    rows);
}

function alias(id) {
  for (const [prefix, letter] of [["worker_", "W"], ["reviewer_", "R"], ["stakeholder_", "S"]]) {
    if (String(id).startsWith(prefix)) return letter + id.slice(prefix.length);
  }
  return id === "lead_reviewer" ? "Lead" : id === "user" ? "You" : "Agent";
}
function role(id) {
  const head = String(id).split("_")[0];
  return head === "worker" ? "worker" : head === "reviewer" || head === "lead" ? "reviewer" : head === "stakeholder" ? "stakeholder" : "other";
}
function who(id) { return [node("strong", { class: "alias", "data-role": role(id) }, alias(id)), actorLink(id)]; }
function mark(status, direction) {
  return node("b", { class: "mark", "data-state": status, "data-dir": direction, "aria-hidden": "true" },
    status === "queued" ? "?" : status === "dropped" ? "×" : "");
}
function place(element, column) {
  element.style.gridColumn = column;
  return element;
}

// One exchange per sender and wire sequence: later transitions update each recipient's state.
function exchanges(rows) {
  const found = new Map();
  for (const event of [...rows].reverse()) {
    const data = event.data;
    const id = JSON.stringify([event.phase, event.cycle, data.attempt, data.worker_request, event.actor, data.sequence]);
    let item = found.get(id);
    if (!item) found.set(id, item = { event, keys: [], to: new Map() });
    item.keys.push(event.key);
    for (const recipient of data.to ?? []) {
      if (data.delivery !== "queued" || !item.to.has(recipient)) item.to.set(recipient, data.delivery);
    }
  }
  return [...found.values()];
}

function sequence(items, query, focus) {
  if (!items.length) return section({ label: "Message sequence", flush: true }, empty("Waiting for agent traffic."));
  const ids = [...new Set([...(state.summary?.event?.workflow?.agents ?? []).map(agent => agent.id),
    ...items.flatMap(item => [item.event.actor, ...item.to.keys()])])];
  const tally = { delivered: 0, queued: 0, dropped: 0 };
  for (const item of items) for (const status of item.to.values()) tally[status] += 1;
  const lanes = (...content) => node("span", { class: "seq-lanes", "aria-hidden": "true" }, content);
  const diagram = node("div", { class: "seq" },
    node("div", { class: "seq-head" }, node("span", {}, "Message"), node("span"),
      lanes(ids.map((id, index) => place(node("span", { class: "alias", "data-role": role(id), title: actorName(id) }, alias(id)), `${index * 2 + 1} / span 2`))),
      node("span")),
    items.map(item => {
      const { event } = item;
      const from = ids.indexOf(event.actor);
      const ends = [...item.to.keys()].map(id => ids.indexOf(id));
      const fan = Boolean(event.data.reply_to) || item.to.size > 1;
      const selected = item.keys.includes(focus);
      const [left, right] = [Math.min(from, ...ends), Math.max(from, ...ends)];
      return link([node("span", { class: "seq-id" }, event.sequence), node("span", { class: "seq-kind" }, fan ? "FAN" : "IPC"),
        lanes(left < right ? place(node("i", { class: `wire${fan ? " fan" : ""}` }), `${left * 2 + 2} / ${right * 2 + 2}`) : null,
          [...item.to].map(([id, status]) => place(mark(status, ids.indexOf(id) > from ? "right" : "left"), `${ids.indexOf(id) * 2 + 1} / span 2`)),
          place(mark("sender"), `${from * 2 + 1} / span 2`)),
        node("span", { class: "seq-body truncate" }, event.data.body)],
      entryUrl(item.keys[0], "ipc", query), {
        id: "event-" + (selected ? focus : item.keys[0]), class: `seq-row${selected ? " selected" : ""}`, "data-role": role(event.actor),
        "aria-label": `Message ${event.sequence} from ${actorName(event.actor)} to ${[...item.to].map(([id, status]) => `${actorName(id)} (${status})`).join(", ")}`,
      });
    }));
  diagram.style.setProperty("--lanes", ids.length);
  return section({ label: "Message sequence", meta: `${ids.length} participants | ${items.length} messages`, flush: true },
    node("div", { class: "seq-summary" },
      node("span", { "data-state": "delivered" }, mark("delivered"), `${tally.delivered} delivered`),
      node("span", { "data-state": "queued" }, mark("queued"), `${tally.queued} queued`),
      node("span", { "data-state": "dropped" }, mark("dropped"), `${tally.dropped} dropped`)),
    diagram,
    node("div", { class: "seq-legend" }, node("span", {}, mark("sender"), "sender"), node("span", {}, mark("delivered"), "delivered"),
      node("span", {}, mark("queued"), "queued"), node("span", {}, mark("dropped"), "dropped"),
      node("span", {}, node("i", { class: "wire fan" }), "fan-out"), spacer(),
      node("span", {}, node("kbd", {}, "↑/↓"), "select")));
}

function messageInspector(detail) {
  const { event } = detail;
  const data = event.data;
  const recipients = new Map((data.to ?? []).map(id => [id, { recipient: id, state: data.delivery, timestamp_ms: event.timestamp_ms }]));
  for (const item of detail.deliveries ?? []) recipients.set(item.recipient, item);
  const count = status => [...recipients.values()].filter(item => item.state === status).length;
  return [node("div", { class: "inspector-section" },
    node("strong", { class: "message-title" }, `#${event.sequence}  ${data.reply_to ? "Stakeholder broadcast" : "Direct IPC"}`),
    muted(`Cycle ${event.cycle} / attempt ${data.attempt ?? 1} / ${title(event.phase)}`),
    data.worker_request ? muted(`Worker request ${data.worker_request}`) : null,
    facts([["From", who(event.actor)], ["Reply to", data.reply_to ? who(data.reply_to) : null], ["Sent", time(event.timestamp_ms)]]),
    muted(`${count("delivered")} delivered / ${count("queued")} queued / ${count("dropped")} dropped`),
    detail.origin ? go("Open original message", entryUrl(detail.origin.key, "ipc")) : null),
  node("div", { class: "inspector-section" }, pre(data.body)),
  node("div", { class: "inspector-section" }, caption(`Recipients (${recipients.size})`),
    [...recipients.values()].map(item => row(mark(item.state), who(item.recipient), spacer(), time(item.timestamp_ms))))];
}

function communication(rows, route, routes, focus) {
  const query = route.query;
  if (query.tab === "matrix") {
    const ids = [...new Set(routes.flatMap(entry => [entry.from, entry.to]))];
    const counts = new Map();
    for (const entry of routes) {
      const key = entry.from + ">" + entry.to;
      const tally = counts.get(key) ?? { queued: 0, delivered: 0, dropped: 0 };
      tally[entry.state] = entry.count;
      counts.set(key, tally);
    }
    const peak = Math.max(1, ...[...counts.values()].map(tally => tally.delivered));
    return section({ label: "Route matrix", meta: "delivered / queued / dropped", flush: true }, table(["From / To", ...ids.map(actorName)], ids.map(from =>
      [actorLink(from), ...ids.map(to => {
        const tally = counts.get(from + ">" + to);
        return tally ? { class: `h${Math.ceil(tally.delivered / peak * 5)}`, cell: link(`${tally.delivered}/${tally.queued}/${tally.dropped}`,
          runUrl("ipc", { ...query, tab: undefined, from, to }), { title: "Delivered / queued / dropped" }) } : "—";
      })]), "matrix"));
  }
  const selected = rows.filter(event => (!query.from || event.actor === query.from) && (!query.to || event.data.to?.includes(query.to)));
  if (query.tab === "lanes") {
    const agents = [...new Set(selected.flatMap(event => [event.actor, ...(event.data.to ?? [])]))];
    return section({ label: "Agent lanes", meta: selected.length || null, flush: true }, table(["Time", ...agents.map(actorName), "Message"], [...selected].reverse().map(event => {
      const cells = [time(event.timestamp_ms), ...agents.map(agent => agent === event.actor ? pill("Sent")
        : event.data.to?.includes(agent) ? pill(event.data.delivery) : node("span", { class: "lane-mark", "aria-hidden": "true" }, "·")),
      link(event.data.body, entryUrl(event.key, "ipc", query))];
      cells.selected = event.key === query.entry;
      return cells;
    }), "matrix lanes"));
  }
  return sequence(exchanges(selected), query, focus);
}

function tabs(page, options, current, query = {}) {
  return node("nav", { class: "tabs", "aria-label": "View options" }, options.map(([name, label]) =>
    link(label, runUrl(page, { ...query, tab: name, before: undefined, entry: undefined }), { "aria-current": current === name ? "page" : undefined })));
}

async function triageView(route) {
  const { query } = route;
  const workflow = state.summary.event?.workflow;
  const plan = state.summary.onboarding?.plan;
  const [inbox, admissions] = await Promise.all([api(apiRun("inbox", { before: query.inbox_before })), api(apiRun("admissions"))]);
  const working = state.live && state.summary.run.phase === "Work";
  return view(heading("Triage", null,
    button("Submit information", () => openAction("triage"), { class: "primary", disabled: !state.live }),
    button("Request worker", () => openAction("add_worker"), { disabled: !working })), [
    workflow?.worker_addition ? section("Worker admission", node("p", {}, workflow.worker_addition),
      plan?.id ? row(caption("Amended plan"), code(plan.id)) : null,
      workflow.active_worker_request ? go("Open onboarding IPC", runUrl("ipc", { phase: "OnboardWorker", worker_request: workflow.active_worker_request })) : null,
      plan?.ownership ? node("details", {}, node("summary", {}, "Proposed write ownership"), pre(JSON.stringify(plan.ownership, null, 2))) : null) : null,
    section({ label: "Worker requests", meta: admissions.rows.length || null, flush: true }, admissions.rows.length
      ? table(["Request", "State", "Amended plan", "Communication"], admissions.rows.map(item =>
        [node("div", {}, code(`#${item.id}`), pre(item.request), item.detail ? muted(item.detail) : null), pill(item.state),
          item.plan?.id ? node("details", {}, node("summary", {}, code(item.plan.id)), pre(JSON.stringify(item.plan.ownership, null, 2))) : "—",
          go("Onboarding IPC", runUrl("ipc", { phase: "OnboardWorker", worker_request: item.id }))]))
      : empty("No worker has been requested. Request one while the run is in the work phase.")),
    section({ label: "Information inbox", meta: inbox.rows.length || null, flush: true },
      inbox.rows.map(item => node("article", { class: `entry${Number(query.item) === item.id ? " selected" : ""}`, id: `inbox-${item.id}` },
        node("header", {}, time(item.submitted_ms), code(`#${item.id}`), pill(item.state)),
        node("div", { class: "entry-body" }, pre(item.text),
          item.decision?.reason ? muted(item.decision.reason) : null,
          item.decision?.recipients?.length ? row(caption("Route"), item.decision.recipients.map(id => actorLink(id))) : null,
          item.delivered?.length ? row(caption("Delivered to"), item.delivered.map(id => actorLink(id))) : null,
          item.decision?.worker_recommendation ? row(muted(item.decision.worker_recommendation),
            button("Use in worker request", () => openAction("add_worker", item.decision.worker_recommendation), { disabled: !working })) : null,
          item.state === "failed" ? row(button("Retry triage", () => sendAction("retry_triage", String(item.id)).catch(captureError), { disabled: !state.live })) : null),
        node("footer", {}, item.decision?.category ? tag(item.decision.category, "category") : null,
          item.decision?.priority ? tag(item.decision.priority, "priority") : null))),
      !inbox.rows.length ? empty("Nothing submitted yet. Submit information and the lead reviewer routes it to the right agents.") : null,
      inbox.rows.length >= 60 ? pagination("Inbox pagination", link("Older information", runUrl("triage", { inbox_before: inbox.rows.at(-1).id }))) : null),
  ]);
}

async function historyView(route) {
  const { page, actor, query } = route;
  if (page === "triage") return triageView(route);
  const local = page === "agent" ? "agent/" + encodeURIComponent(actor) : page;
  const tab = query.tab ?? "all";
  const kind = page === "ipc" || (page === "agent" && tab === "ipc") ? "ipc" :
    page === "tools" ? tab === "mcp" ? "mcp" : "tool" :
      page === "agent" && tab === "tools" ? "tool" : query.kind;
  const [historyData, chosen] = await Promise.all([
    api(apiRun("history", { ...query, actor: actor ?? query.actor, kind,
      from: actor && tab === "ipc" && query.direction === "outgoing" ? actor : query.from,
      to: actor && tab === "ipc" && query.direction === "incoming" ? actor : query.to,
      path: tab === "files" ? query.path ?? "*" : query.path })),
    query.entry ? api(apiRun("entry/" + encodeURIComponent(query.entry))).catch(() => null) : null,
  ]);
  let { rows } = historyData;
  // Like the TUI, the sequence diagram inspects the newest message until one is chosen.
  const diagram = page === "ipc" && tab === "all";
  const focus = query.entry ?? (diagram ? rows.find(event => event.data.delivery === "queued")?.key ?? rows[0]?.key : undefined);
  const detail = chosen ?? (focus && !query.entry ? await api(apiRun("entry/" + encodeURIComponent(focus))).catch(() => null) : null);
  const head = [];
  if (page === "agent") {
    const flow = state.summary.event?.workflow;
    const agent = flow?.agents?.find(entry => entry.id === actor) ?? { id: actor, name: actor };
    const task = flow?.tasks?.find(entry => entry.task.worker === actor);
    const match = /^(worker|reviewer|stakeholder)_(\d+)$/.exec(actor ?? "");
    const persona = match ? state.summary[match[1] + "s"]?.[Number(match[2]) - 1]?.persona : null;
    const active = generation(actor);
    head.push(node("section", { class: "band" },
      node("div", { class: "hero" }, caption("Agent"), node("h1", { class: "hero-title" }, agent.name),
        node("span", { class: "hero-sub" }, node("span", { class: `dot${active ? " live" : ""}`, "data-agent-dot": actor }), code(actor), task ? pill(task.state) : null)),
      task?.task ? node("div", { class: "hero-field wide" }, caption("Task"), node("p", {}, task.task.description)) : null,
      persona ? node("div", { class: "hero-field wide" }, caption("Persona"),
        pre(persona, { class: "persona", tabindex: 0, "aria-label": "Persona" })) : null,
      pre(active?.text || "", { class: "preview", hidden: !active, "data-preview-actor": actor })),
    tabs(local, [["all", "Conversation"], ["ipc", "Incoming & outgoing IPC"], ["tools", "Tools"]], tab, query));
    if (tab === "ipc") head.push(node("nav", { class: "tabs sub", "aria-label": "Message direction" },
      [["all", "All messages"], ["incoming", "Incoming"], ["outgoing", "Outgoing"]].map(([direction, label]) =>
        link(label, runUrl(local, { ...query, direction, entry: undefined, before: undefined }),
          { "aria-current": (query.direction ?? "all") === direction ? "page" : undefined }))));
  } else if (page === "history") {
    head.push(heading("Timeline", historyData.legacy ? "Recoverable checkpoint entries. Coverage depends on what the older run retained." : null,
      go("Checkpoint archive", runUrl("archive")),
      go(query.legacy ? "Audit timeline" : "Recoverable checkpoint entries", runUrl("history", { legacy: query.legacy ? undefined : 1 })),
      state.live ? button(state.follow ? "Live follow" : "Follow paused", () => {
        state.follow = !state.follow;
        load(false);
      }, { "aria-pressed": String(state.follow) }) : null));
  } else {
    head.push(heading(views[page] ?? "Timeline"));
  }
  if (page === "ipc") head.push(tabs(local, [["all", "Sequence"], ["lanes", "Agent lanes"], ["matrix", "Route matrix"]], tab, query));
  if (page === "tools") head.push(tabs(local, [["all", "All tools"], ["mcp", "MCP"], ["files", "File activity"]], tab, query));
  if (query.path && tab === "files") head.push(node("div", { class: "scope" }, caption("File"), code(query.path), go("All files", runUrl("tools", { tab: "files" }))));
  if (page === "ipc" && (query.from || query.to || query.worker_request)) head.push(node("div", { class: "scope" },
    query.from ? pill("From " + actorName(query.from)) : null, query.to ? pill("To " + actorName(query.to)) : null,
    query.worker_request ? pill("Worker request " + query.worker_request) : null, go("Clear route", runUrl("ipc", { tab: query.tab }))));
  head.push(filters(route, { actor: page !== "agent", kind: page === "history" }));
  if (detail && page === "history" && !rows.some(entry => entry.key === detail.event.key)) rows = [detail.event, ...rows];
  let content;
  if (page === "ipc") content = communication(rows, route, query.tab === "matrix" ? (await api(apiRun("routes", query))).rows : [], focus);
  else if (page === "tools" || tab === "tools") content = section({ label: tab === "mcp" ? "MCP calls" : "Tool calls", meta: rows.length || null, flush: true }, toolTable(rows, route, local));
  else content = node("section", { class: "timeline", "aria-label": "Recorded timeline" }, rows.length
    ? rows.map(event => historyCard(event, query.entry, query.call))
    : empty("No matching entries."));
  const oldest = historyData.rows.at(-1)?.sequence;
  const pages = historyData.legacy
    ? pagination("Checkpoint history pagination",
      query.before ? link("Latest", runUrl(local, { ...query, before: undefined, entry: undefined })) : null,
      historyData.next_before ? link("Older checkpoints", runUrl(local, { ...query, before: historyData.next_before, entry: undefined })) : null)
    : pagination("History pagination",
      query.before ? link("Latest", runUrl(local, { ...query, before: undefined, entry: undefined })) : null,
      historyData.rows.length >= 60 ? link("Older entries", runUrl(local, { ...query, before: oldest, entry: undefined })) : null);
  return view(head, diagram ? [pages, content] : [content, pages], {
    aside: inspector(detail, query.entry ? runUrl(local, { ...query, entry: undefined }) : null), flush: true, follow: diagram,
  });
}

async function archive(route) {
  const rows = (await api(apiRun("checkpoints", { after: route.query.after }))).rows;
  const snapshot = route.query.sequence ? await api(apiRun("checkpoint", { sequence: route.query.sequence })) : null;
  return view(heading("Checkpoint archive", null, go("Timeline", runUrl("history"))), [
    muted("Snapshots hold the conversation still retained at each checkpoint. Older runs may have gaps."),
    node("div", { class: "columns" },
      section({ label: "Checkpoints", flush: true }, table(["Sequence", "Phase", "Cycle", "Time"], rows.map(entry => {
        const cells = [link(entry.sequence, runUrl("archive", { after: route.query.after, sequence: entry.sequence })), title(entry.phase), entry.cycle, time(entry.timestamp_ms)];
        cells.selected = String(entry.sequence) === route.query.sequence;
        return cells;
      }))),
      snapshot ? section({ label: "Retained conversations", flush: true }, snapshot.dialogs.map(dialog =>
        node("details", {}, node("summary", {}, actorName(dialog.actor)), dialog.task ? pre(dialog.task) : null,
          dialog.assessment ? pre(dialog.assessment) : null, pre(JSON.stringify(dialog.groups ?? [], null, 2)))))
        : section("Retained conversations", muted("Choose a checkpoint to read what it retained."))),
    rows.length === 80 ? pagination("Checkpoint pagination", link("More checkpoints", runUrl("archive", { after: rows.at(-1).sequence }))) : null,
  ]);
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
  main.setAttribute("aria-busy", "true");
  try {
    const [runs, summary, live] = await Promise.all([
      api("/api/runs"),
      wanted.run ? api(apiRun("overview")) : null,
      wanted.run ? api(apiRun("live")).catch(() => null) : null,
    ]);
    if (version !== state.version) return;
    const navigationChanged = navigationKey(state.runs, state.summary) !== navigationKey(runs.runs, summary);
    state.runs = runs.runs;
    state.summary = summary;
    state.live = live;
    if (restart || navigationChanged || !wanted.run) renderNavigation();
    else renderLive();
    const nodes = !wanted.run ? workspace() : wanted.page === "overview" ? overview()
      : wanted.page === "agents" ? agentsView()
        : wanted.page === "archive" ? await archive(wanted) : await historyView(wanted);
    if (version !== state.version) return;
    if (preserveInteraction && hasPageInteraction()) {
      state.refreshPending = true;
      return;
    }
    // Choosing another entry in the same list keeps the list where it is.
    const spot = location.pathname + "?" + new URLSearchParams(Object.entries(wanted.query).filter(([key]) => key !== "entry" && key !== "call"));
    const settled = spot === state.spot;
    state.spot = spot;
    const scrolls = restart && !settled ? [] : [main, ...main.querySelectorAll("[data-scroll]")].map(element =>
      [element.dataset.scroll, element.scrollLeft, element.scrollTop, element.scrollHeight - element.scrollTop - element.clientHeight < 8]);
    fill(main, nodes);
    for (const [key, left, top, atEnd] of scrolls) {
      const element = key ? $(`[data-scroll="${key}"]`, main) : main;
      if (!element || (restart && key === "inspector")) continue;
      element.scrollTo(left, element.dataset.follow && atEnd && !wanted.query.entry ? element.scrollHeight : top);
    }
    if (!scrolls.length && !wanted.query.entry) {
      const follower = $("[data-follow]", main);
      follower?.scrollTo(0, follower.scrollHeight);
    }
    if (restart && wanted.query.entry) {
      document.getElementById("event-" + wanted.query.entry)?.scrollIntoView({ block: settled ? "nearest" : "center" });
    }
    if (restart && wanted.page === "triage" && wanted.query.item) {
      document.getElementById("inbox-" + wanted.query.item)?.scrollIntoView({ block: "center" });
    }
    if (wanted.run && !state.stream) connect(wanted.run, summary);
  } catch (error) {
    if (version === state.version) fill(main, view(heading("Run unavailable"), section("Connection", muted(error.message), go("Workspace runs", "/"))));
  } finally {
    if (version === state.version) {
      main.inert = false;
      main.setAttribute("aria-busy", "false");
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
        { headers: { Authorization: "Bearer " + token }, signal: controller.signal });
      if (!stream.ok || !stream.body) break;
      const reader = stream.body.pipeThrough(new TextDecoderStream()).getReader();
      let pending = "";
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        pending += chunk.value;
        let boundary;
        while ((boundary = pending.indexOf("\n\n")) >= 0) {
          const packet = pending.slice(0, boundary);
          pending = pending.slice(boundary + 2);
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
    if (!controller.signal.aborted) await new Promise(resolve => setTimeout(resolve, 1200));
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
  start: ["Start a run", "Describe the outcome and constraints. Ratio will propose a roster and start the run.", "Request", true],
  fork: ["Fork this run", "Start a new run with this roster. Leave the request empty to keep the original goal.", "Replacement request", false],
  resume_saved: ["Resume saved run", "Continue from the latest committed checkpoint with the current runner configuration.", "", false],
  add_worker: ["Request a worker", "Describe the role and scope. Writes remain gated until unanimous acknowledgement.", "Assignment and proposed scope", true],
  triage: ["Submit information", "The lead reviewer will classify and route this item automatically.", "Information", true],
  stop: ["Stop this run", "Cancel active work and retain the last checkpoint.", "", false],
};

function openAction(name, initial = "") {
  const [label, hint, field, required] = descriptions[name];
  actionState = { name, run: state.route.run, id: crypto.randomUUID() };
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
  const receipt = await api("/api/action", { method: "POST", headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ action, text, run, id }) });
  notify(receipt.detail);
  if (receipt.run_id && receipt.run_id !== state.route.run) navigate(runUrl("overview", {}, receipt.run_id));
  else load(false);
}

document.addEventListener("click", event => {
  if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  const anchor = event.target.closest("a[href]");
  const href = anchor?.getAttribute("href");
  if (href === "/" || href?.startsWith("/run/")) {
    event.preventDefault();
    $(".side").classList.remove("open");
    $("#menu").setAttribute("aria-expanded", "false");
    navigate(href);
  }
});
window.addEventListener("popstate", () => load());
$("#menu").addEventListener("click", () => {
  const open = $(".side").classList.toggle("open");
  $("#menu").setAttribute("aria-expanded", String(open));
});
$("#new-run").addEventListener("click", () => openAction("start"));
$("#dismiss-notice").addEventListener("click", () => { $("#notice").hidden = true; });
$("#run-select").addEventListener("change", event => navigate(runUrl("overview", {}, event.target.value)));
$("#global-search").addEventListener("submit", event => {
  event.preventDefault();
  navigate(runUrl("history", { q: new FormData(event.target).get("q") }));
});
$("#close-action").addEventListener("click", () => $("#action-dialog").close());
$("#action-form").addEventListener("submit", async event => {
  event.preventDefault();
  const submit = $("#submit-action");
  submit.disabled = true;
  try {
    await sendAction(actionState.name, $("#action-text").value, actionState.run, actionState.id);
    $("#action-dialog").close();
  } catch (error) {
    $(".action-error").textContent = error.message;
    $(".action-error").hidden = false;
  } finally { submit.disabled = false; }
});
document.addEventListener("keydown", event => {
  if ($("#action-dialog").open || event.metaKey || event.ctrlKey || event.altKey
    || event.target.matches?.("input, textarea, select") || !state.route.run) return;
  const page = Object.keys(views)[Number(event.key) - 1];
  const messages = [...document.querySelectorAll(".seq-row")];
  const step = { ArrowUp: -1, k: -1, ArrowDown: 1, j: 1 }[event.key];
  if (step && messages.length) {
    const current = messages.findIndex(message => message.classList.contains("selected"));
    const target = messages[Math.max(0, Math.min(messages.length - 1, (current < 0 ? messages.length - 1 : current) + step))];
    event.preventDefault();
    navigate(target.getAttribute("href"));
  } else if (event.key === "/") {
    event.preventDefault();
    $("#global-search input").focus();
  } else if (page && event.key.trim()) navigate(runUrl(page));
});
let lastTriageRefresh = 0;
async function pollLive() {
  const run = state.route.run;
  const version = state.version;
  try {
    if (!run) return;
    const live = await api(apiRun("live"));
    if (version !== state.version) return;
    if (!live && state.live?.token_usage) {
      const summary = await api(apiRun("overview"));
      if (version !== state.version) return;
      state.summary.metrics = summary.metrics;
    }
    state.live = live;
    renderLive();
    if (state.route.page === "triage" && performance.now() - lastTriageRefresh >= 1400) {
      state.refreshPending = true;
      lastTriageRefresh = performance.now();
    }
    refreshLivePage();
  } catch {
    // A disconnected dashboard can resume on its next poll.
  } finally {
    setTimeout(pollLive, state.live?.generations?.length ? 180 : 1000);
  }
}

load();
setTimeout(pollLive, 1000);
