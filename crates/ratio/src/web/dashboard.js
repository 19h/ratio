"use strict";

const $ = (selector, scope = document) => scope.querySelector(selector);
const main = $("#content");
const readOnly = document.body.hasAttribute("data-read-only");
const sharingEnabled = document.body.hasAttribute("data-sharing");
const shareRunId = readOnly ? route().run : undefined;
const tokenKey = readOnly ? "ratio-share:" + shareRunId : "ratio-access";
const launch = new URLSearchParams(location.hash.slice(1)).get(readOnly ? "share" : "access");
if (launch && /^[a-f0-9]{64}$/.test(launch)) {
  sessionStorage.setItem(tokenKey, launch);
  history.replaceState(null, "", location.pathname + location.search);
}
const token = sessionStorage.getItem(tokenKey);
$("#share-mode").hidden = !readOnly;
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
const state = { runs: [], displayRuns: [], summary: null, live: null, config: null, providerCheck: null, route: {}, version: 0, follow: true, refreshPending: false };
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

function view(head, body, { aside = null, flush = false } = {}) {
  return [head, node("div", { class: "view-body" },
    node("div", { class: `view-scroll ${flush ? "flush" : "stack"}`, "data-scroll": "body" }, body), aside)];
}

function table(columns, rows, className = "") {
  if (!rows.length) return empty("No matching entries.");
  return node("div", { class: "table-wrap" }, node("table", { class: className },
    node("thead", {}, node("tr", {}, columns.map(text => node("th", { scope: "col" }, text)))),
    node("tbody", {}, rows.map(cells => node("tr", { id: cells.id, class: cells.selected ? "selected" : undefined }, cells.map(value =>
      value?.cell === undefined ? node("td", {}, value) : node(value.scope ? "th" : "td", { class: value.class, scope: value.scope }, value.cell)))))));
}

function runUrl(page = "overview", query = {}, id = state.route.run) {
  const params = new URLSearchParams(Object.entries(query).filter(([, value]) => value !== undefined && value !== null && value !== ""));
  return id ? `/run/${encodeURIComponent(id)}/${page}${params.size ? "?" + params : ""}` : "/";
}
const queryKeys = {
  history: ["actor", "after", "before", "collapse_tools", "cycle", "from", "kind", "legacy", "path", "phase", "q", "to", "worker_request"],
  routes: ["actor", "cycle", "from", "phase", "q", "to", "worker_request"],
  inbox: ["before"], checkpoints: ["after"], checkpoint: ["sequence"],
};
function apiRun(view, query = {}, run = state.route.run) {
  const params = new URLSearchParams();
  for (const key of queryKeys[view] ?? []) {
    const value = query[key];
    if (value !== undefined && value !== null && value !== "") params.set(key, value);
  }
  return `/api/run/${encodeURIComponent(run)}/${view}${params.size ? "?" + params : ""}`;
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

const warmViews = new Set([
  "overview", "live", "metrics", "history?collapse_tools=1", "history?kind=ipc", "history?kind=tool",
  "history?kind=mcp", "history?kind=job", "history?kind=tool&path=*", "routes", "inbox", "admissions",
  "checkpoints", "share",
]);
const terminalToolStates = new Set(["completed", "failed", "refused", "unknown"]);
const values = new Map();
const waiting = new Map();
const queries = new Map();
const actions = new Map();
let socket, connected = false, reconnectTimer, retry = 0, nextQuery = 0, selected;
let flushScheduled = false;
let activeReads = new Set();
let rendered = false;
let liveChangePending = false;
let targetedRefresh = false;

function warm(path) {
  return path === "/api/runs" || path === "/api/config" ||
    warmViews.has(path.replace(/^\/api\/run\/[^/]+\//, ""));
}

function valueOf(result) {
  if (result.error !== undefined) throw new Error(result.error);
  return result.data;
}

function send(message) {
  if (!connected || socket?.readyState !== WebSocket.OPEN) return false;
  socket.send(JSON.stringify(message));
  return true;
}

function selectRun(run) {
  if (selected === run) return;
  for (const [path, waiter] of waiting) {
    if (!path.startsWith("/api/run/")) continue;
    waiter.reject(new Error("Run selection changed"));
    waiting.delete(path);
  }
  for (const [id, paths] of queries) {
    if (paths.some(path => path.startsWith("/api/run/"))) queries.delete(id);
  }
  for (const path of values.keys()) {
    if (path.startsWith("/api/run/")) values.delete(path);
  }
  selected = run;
  send({ type: "select", run });
}

function flushQueries() {
  flushScheduled = false;
  if (!connected) return;
  const paths = [...waiting].filter(([, item]) => item.query && !item.sent).map(([path]) => path);
  while (paths.length && queries.size < 4) {
    const batch = paths.splice(0, 12);
    const id = ++nextQuery;
    if (!send({ type: "query", id, paths: batch })) return;
    for (const path of batch) waiting.get(path).sent = true;
    queries.set(id, batch);
  }
}

function queueQueries() {
  if (flushScheduled) return;
  flushScheduled = true;
  queueMicrotask(flushQueries);
}

async function api(path, { refresh = false } = {}) {
  activeReads.add(path);
  if (!refresh && values.has(path)) return valueOf(values.get(path));
  if (!waiting.has(path)) {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    waiting.set(path, { promise, resolve, reject, query: refresh || !warm(path), sent: false });
    queueQueries();
  }
  return waiting.get(path).promise;
}

function settle(path, result) {
  const waiter = waiting.get(path);
  if (!waiter) return;
  waiting.delete(path);
  if (result.error !== undefined) waiter.reject(new Error(result.error));
  else waiter.resolve(result.data);
}

function shareUnavailable() {
  clearTimeout(reconnectTimer);
  socket?.close();
  socket = null;
  connected = false;
  for (const waiter of waiting.values()) waiter.reject(new Error("Invalid or revoked run share link"));
  waiting.clear();
  queries.clear();
  values.clear();
  state.version++;
  state.summary = null;
  state.live = null;
  renderNavigation();
  fill(main, view(heading(readOnly ? "Share unavailable" : "Run unavailable"), section("Access",
    muted(readOnly ? "Invalid or revoked run share link" : "Open the private URL printed by ra --web"))));
  main.inert = false;
  main.setAttribute("aria-busy", "false");
}

function onStreamMessage(message) {
  switch (message.type) {
    case "welcome":
      connected = true;
      retry = 0;
      document.body.removeAttribute("data-stream-stale");
      liveChangePending = true;
      scheduleUpdate();
      const wantedRun = state.route.run ?? null;
      if (!readOnly && selected !== wantedRun) selectRun(wantedRun);
      queueQueries();
      for (const action of actions.values()) send(action.message);
      break;
    case "unauthorized":
      shareUnavailable();
      break;
    case "query": {
      const paths = queries.get(message.id);
      if (!paths) break;
      queries.delete(message.id);
      for (const path of paths) {
        const result = message.values[path] ?? { error: "Dashboard result is unavailable" };
        if ((warm(path) || activeReads.has(path)) && result.error === undefined) values.set(path, result);
        settle(path, result);
      }
      queueQueries();
      break;
    }
    case "receipt": {
      const action = actions.get(message.id);
      if (!action) break;
      actions.delete(message.id);
      if (message.result.error !== undefined) action.reject(new Error(message.result.error));
      else action.resolve(message.result.data);
      break;
    }
    case "data": {
      if (message.scope !== null && message.scope !== selected) break;
      let refresh = false;
      for (const [path, result] of Object.entries(message.values)) {
        if (path.endsWith("/revision")) {
          targetedRefresh = true;
          continue;
        }
        const hadValue = values.has(path);
        values.set(path, result);
        settle(path, result);
        if (path === "/api/config" && result.error === undefined) state.config = result.data;
        if (!activeReads.has(path) || !hadValue || !rendered) continue;
        if (path.endsWith("/live") && result.error === undefined) {
          const previous = state.live;
          state.live = result.data;
          liveChangePending = true;
          if (Boolean(previous) !== Boolean(state.live) || previous?.paused !== state.live?.paused) refresh = true;
        } else {
          refresh = true;
        }
      }
      if (refresh) state.refreshPending = true;
      scheduleUpdate();
      break;
    }
  }
}

let decoderReady, decoderDisabled = false, cachedDictionary = null;

function dashboardDecoder() {
  if (decoderDisabled || !globalThis.Worker || !globalThis.WebAssembly || !globalThis.crypto?.subtle) {
    return Promise.resolve(null);
  }
  decoderReady ??= new Promise(resolve => {
    const worker = new Worker("/dashboard-decoder.js");
    const requests = new Map();
    let serial = 0;
    const timeout = setTimeout(() => unavailable(), 2500);
    function unavailable() {
      clearTimeout(timeout);
      decoderDisabled = true;
      worker.terminate();
      for (const { reject } of requests.values()) reject(new Error("Dashboard decoder stopped"));
      requests.clear();
      resolve(null);
    }
    worker.onerror = unavailable;
    worker.onmessageerror = unavailable;
    worker.onmessage = ({ data }) => {
      if (data.ready === false) { unavailable(); return; }
      if (data.ready === true) {
        clearTimeout(timeout);
        resolve((frame, decoded, id, dictionary) => new Promise((resolve, reject) => {
          if (decoderDisabled) { reject(new Error("Dashboard decoder unavailable")); return; }
          requests.set(++serial, { resolve, reject });
          worker.postMessage({ serial, frame, decoded, id, dictionary }, [frame]);
        }));
        return;
      }
      const pending = requests.get(data.serial);
      if (!pending) return;
      requests.delete(data.serial);
      if (data.error) pending.reject(new Error(data.error));
      else pending.resolve(data.body);
    };
  }).catch(() => { decoderDisabled = true; return null; });
  return decoderReady;
}

function connectDashboard() {
  clearTimeout(reconnectTimer);
  const url = new URL("/api/dashboard/socket", location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  const stream = new WebSocket(url);
  let initialRun;
  socket = stream;
  stream.binaryType = "arraybuffer";
  let decoding = Promise.resolve();
  let chunks = [], length = 0;
  let decoder, welcome, received = false, lastSequence = 0;
  const decoderBoot = dashboardDecoder();
  const start = () => {
    if (socket !== stream) return;
    if (initialRun !== selected && !readOnly) {
      connected = true;
      send({ type: "select", run: selected });
    }
    onStreamMessage(welcome);
  };
  const ready = async () => {
    if (socket !== stream || stream.readyState !== WebSocket.OPEN) return;
    stream.send(JSON.stringify({ type: "ready", sha256: welcome.dictionary?.sha256 ?? null }));
    start();
  };
  stream.addEventListener("open", async () => {
    decoder = await decoderBoot;
    if (socket !== stream || stream.readyState !== WebSocket.OPEN) return;
    initialRun = selected ?? null;
    stream.send(JSON.stringify({ type: "hello", version: decoder ? 2 : 1, token: token ?? "",
      run: initialRun, gzip: typeof DecompressionStream === "function", dictionary: cachedDictionary }));
  });
  stream.addEventListener("message", event => {
    decoding = decoding.then(async () => {
      if (socket !== stream) return;
      if (typeof event.data === "string") {
        const message = JSON.parse(event.data);
        if (message.type === "welcome") {
          welcome = message;
          if (welcome.version !== 2) { start(); return; }
          if (!decoder || !["zstd", "zstd-dict"].includes(welcome.codec) ||
              (welcome.codec === "zstd-dict") !== Boolean(welcome.dictionary)) {
            throw new Error("Invalid dashboard codec");
          }
          if (!welcome.dictionary || welcome.dictionary.sha256 === cachedDictionary) await ready();
          return;
        }
        onStreamMessage(message);
        return;
      }
      const bytes = new Uint8Array(event.data);
      if (bytes.length < 9) throw new Error("Invalid dashboard frame");
      const flags = bytes[0];
      const sequence = Number(new DataView(event.data).getBigUint64(1));
      let body;
      if (welcome?.version === 2) {
        if (bytes.length < 14 || flags & ~6) throw new Error("Invalid Zstandard envelope");
        const decoded = new DataView(event.data).getUint32(9);
        if (flags & 4) {
          const dictionary = welcome.dictionary;
          if (flags !== 6 || sequence !== 0 || !dictionary || decoded !== dictionary.bytes ||
              decoded > 64 * 1024 || connected || !/^[0-9a-f]{64}$/.test(dictionary.sha256)) {
            throw new Error("Invalid dashboard dictionary envelope");
          }
          cachedDictionary = null;
          await decoder(event.data, decoded, 0, dictionary);
          cachedDictionary = dictionary.sha256;
          await ready();
          return;
        }
        if (!connected || sequence !== ++lastSequence) throw new Error("Unexpected dashboard sequence");
        body = await decoder(event.data, decoded, welcome.dictionary?.id ?? 0);
      } else if (flags & 1) {
        body = bytes.subarray(9);
        const reader = new Blob([body]).stream().pipeThrough(new DecompressionStream("gzip"));
        body = new Uint8Array(await new Response(reader).arrayBuffer());
      } else {
        body = bytes.subarray(9);
      }
      if (socket !== stream) return;
      length += body.length;
      received = true;
      if (length > 64 * 1024 * 1024) throw new Error("Dashboard message is too large");
      chunks.push(body);
      if (flags & 2) {
        const joined = new Uint8Array(length);
        let offset = 0;
        for (const chunk of chunks) { joined.set(chunk, offset); offset += chunk.length; }
        chunks = [];
        length = 0;
        onStreamMessage(JSON.parse(new TextDecoder().decode(joined)));
      }
      if (stream.readyState === WebSocket.OPEN) stream.send(JSON.stringify({ type: "ack", sequence }));
    }).catch(() => {
      if (decoder && !received) decoderDisabled = true;
      stream.close();
    });
  });
  stream.addEventListener("close", () => {
    if (socket !== stream) return;
    if (decoder && !welcome) decoderDisabled = true;
    connected = false;
    document.body.setAttribute("data-stream-stale", "");
    liveChangePending = true;
    scheduleUpdate();
    queries.clear();
    for (const waiter of waiting.values()) waiter.sent = false;
    reconnectTimer = setTimeout(connectDashboard, Math.min(5000, 250 * 2 ** Math.min(retry++, 5)));
  });
}

let updateScheduled = false;

function detailCanChange(detail) {
  const { event, outcome } = detail;
  if (event?.kind === "ipc") return true;
  if (event?.kind !== "tool" && event?.kind !== "tool_result") return false;
  return !terminalToolStates.has((outcome ?? event).data?.state);
}

function scheduleUpdate() {
  if (updateScheduled) return;
  updateScheduled = true;
  requestAnimationFrame(() => {
    updateScheduled = false;
    if (liveChangePending) {
      liveChangePending = false;
      renderLive();
    }
    if (targetedRefresh) {
      targetedRefresh = false;
      for (const path of activeReads) {
        if (!warm(path) && path.startsWith("/api/run/") && !path.includes("/checkpoint?")) {
          const previous = values.get(path);
          if (!previous) continue;
          if (path.includes("/entry/") && !detailCanChange(previous.data)) continue;
          api(path, { refresh: true }).then(value => {
            if (activeReads.has(path) && JSON.stringify(previous.data) !== JSON.stringify(value)) {
              state.refreshPending = true;
              scheduleUpdate();
            }
          }).catch(() => {});
        }
      }
    }
    refreshLivePage();
  });
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
  const runs = state.summary?.run && !state.runs.some(run => run.run_id === selected)
    ? [state.summary.run, ...state.runs] : state.runs;
  fill(select, node("option", { value: "" }, "All runs"),
    runs.map(run => node("option", { value: run.run_id }, `${title(run.phase)} · ${run.run_id.slice(-10)}`)));
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
  const here = !selected ? readOnly ? "Shared run" : "Runs" : current === "agent" ? actorName(state.route.actor)
    : current === "archive" ? "Checkpoint archive" : views[current] ?? "Run";
  const slash = () => node("span", { class: "slash", "aria-hidden": "true" }, "/");
  fill($("#breadcrumb"), link(node("strong", {}, "ratio"), readOnly ? runUrl("overview") : "/"), slash(),
    selected ? [link(node("strong", {}, selected.slice(-10)), runUrl("overview"), { title: "Run " + selected }), slash()] : null,
    node("strong", { class: "here" }, here));
  $("#global-search").hidden = !selected;
  renderLive();
}

function generation(id) { return state.live?.generations?.find(item => item.actor === id); }

function renderLive() {
  const connection = $("#connection");
  const stale = document.body.hasAttribute("data-stream-stale");
  const status = stale ? "stale" : state.live ? state.live.paused ? "paused" : "live" : "history";
  connection.dataset.state = status;
  $("span", connection).textContent = stale ? "Reconnecting" : state.live
    ? state.live.paused ? "Paused" : `Live · ${state.live.generations?.length ?? 0} generating`
    : state.route.run ? "Recorded run" : readOnly ? "Shared run" : "Local workspace";
  const controls = $("#controls");
  const actions = readOnly || stale ? [] : state.live ? [
    button("Guide", () => openAction("guide")),
    button("Add worker", () => openAction("add_worker"), { disabled: state.summary?.run?.phase !== "Work" }),
    button(state.live.paused ? "Resume" : "Pause", () => sendAction(state.live.paused ? "resume" : "pause").catch(captureError)),
    button("Stop", () => openAction("stop"), { class: "danger" }),
  ] : state.route.run && state.summary ? [
    button("Resume saved", () => openAction("resume_saved"), {
      disabled: endings.includes(state.summary.run.phase) && !state.summary.resume_phase,
    }),
    ...(state.summary.run.phase === "Approved"
      ? [button("Continue run", () => openAction("continue"))] : []),
    button("Fork", () => openAction("fork")),
  ] : [];
  const phase = state.summary?.run?.phase ?? "";
  const currentRun = state.route.run ?? "";
  if (controls.dataset.mode !== status || controls.dataset.phase !== phase || controls.dataset.run !== currentRun) {
    if (!readOnly && !stale && sharingEnabled && state.summary) actions.push(button("Share", () => openShare().catch(captureError)));
    fill(controls, actions);
    controls.dataset.mode = status;
    controls.dataset.phase = phase;
    controls.dataset.run = currentRun;
  }
  for (const card of document.querySelectorAll("[data-preview-actor]")) {
    const active = generation(card.dataset.previewActor);
    if (card.hidden === Boolean(active)) card.hidden = !active;
    const text = active?.text || (active ? active.label : "");
    if (card.textContent !== text) card.textContent = text;
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
  const items = run ? [
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
  ] : [cell(`${state.runs.length}${state.runs.length === 100 ? "+" : ""} runs`), spacer()];
  patchChildren($("#status"), children([mode, items]));
}

function tokenCount(count) {
  return count?.tokens != null ? `${count.estimated ? "~" : ""}${number.format(count.tokens)}${count.partial ? "+" : ""}` : "—";
}

function tokenFlow(kind, arrow, label, count) {
  const known = count?.tokens != null;
  const text = tokenCount(count);
  const value = known ? `${count.estimated ? "approximately " : ""}${number.format(count.tokens)}${count.partial ? " with some requests missing data" : ""}` : "unavailable";
  const description = `${label} · ${value}`;
  return node("span", { class: "token-flow", "data-token-flow": kind, "data-token-count": count?.tokens, title: description, "aria-label": description },
    node("span", { class: "token-arrow", "aria-hidden": "true" }, arrow,
      kind === "cached_input" ? node("span", { class: "token-cache" }) : null),
    text);
}

async function testProvider() {
  state.providerCheck = { pending: true };
  load(false);
  try { state.providerCheck = await postAction("check_provider"); }
  catch (error) { state.providerCheck = { error: error.message }; }
  load(false);
}

function providerCheck() {
  const check = state.providerCheck;
  if (!check) return null;
  if (check.pending) return muted("Sending one small request to each configured model…");
  if (check.error) return node("p", { class: "action-error", role: "alert" }, check.error);
  return table(["Model", "Result", "Latency"], check.checks.map(item => [code(item.model),
    !item.ok ? node("span", { class: "action-error" }, item.error)
      : item.tool_call ? "Reachable. The model returned the test tool call."
        : "Reachable, but the model did not call the test tool. Check that it supports function calling.",
    item.ok ? `${number.format(item.latency_ms)} ms` : "—"]));
}

function configPanel() {
  const config = state.config;
  if (!config) return null;
  const actions = row(button("Recheck", async () => {
    state.providerCheck = null;
    try { await api("/api/config", { refresh: true }); await load(false); }
    catch (error) { captureError(error); }
  }),
    config.ok ? button("Test provider", () => testProvider(), { disabled: Boolean(state.providerCheck?.pending) }) : null);
  const note = muted("Each run reads the profile when it starts. Flags and environment variables are fixed when the dashboard starts.");
  if (!config.ok) {
    return section("Configuration", node("p", { class: "action-error", role: "alert" }, config.error),
      muted("Runs cannot start until this is fixed. Edit the profile, then choose Recheck."), note, actions);
  }
  const { provider, models, team, staging, limits } = config;
  const model = (name, effort) => [code(name), effort ? ` (${effort} effort)` : null];
  const caps = [
    limits.max_review_cycles ? `${number.format(limits.max_review_cycles)} review cycles` : null,
    limits.max_total_tokens ? `${number.format(limits.max_total_tokens)} tokens` : null,
    limits.max_run_secs ? `${number.format(limits.max_run_secs)} seconds` : null,
    limits.max_requests ? `${number.format(limits.max_requests)} requests` : null,
  ].filter(Boolean);
  return section({ label: "Configuration", meta: config.profile ?? "No profile file" }, facts([
    ["Endpoint", [code(provider.endpoint), " ", tag(provider.api)]],
    ["Key", provider.key ? "Set" : "Not set"],
    ["Models", models.worker === models.reviewer && models.worker_effort === models.reviewer_effort
      ? model(models.worker, models.worker_effort)
      : ["Workers ", model(models.worker, models.worker_effort), ", reviewer ", model(models.reviewer, models.reviewer_effort)]],
    ["Context", `${number.format(config.context_window)} tokens, replies up to ${number.format(config.output_tokens)}`],
    ["Team", team.mode === "generated" ? "Proposed for each request"
      : team.mode === "fixed" ? "One built-in worker" : `Configured: ${team.workers.join(", ")}`],
    ["Publication", [staging.enabled ? "Staged." : "Direct. Workers edit the workspace in place.",
      staging.verification_commands.length ? [" Verified by ", code(staging.verification_commands.join(" && "))] : null]],
    ["Limits", caps.length ? caps.join(", ") : "None. A run continues until the reviewer approves or rejects it."],
    ["MCP servers", config.mcp_servers.length ? config.mcp_servers.join(", ") : null],
  ]), providerCheck(), note, actions);
}

function workspace() {
  const runs = state.displayRuns;
  const older = state.route.query.runs_before;
  if (!older && !runs.length) {
    return view(heading("Runs"), [node("button", { type: "button", class: "empty-drop", onclick: () => openAction("start") },
      node("strong", {}, "Start a run"),
      node("span", {}, "Describe the outcome and its constraints. Ratio proposes a team, plans the work, and records every step here.")),
    configPanel()]);
  }
  return view(heading("Runs"), [
    section({ label: "Run history", flush: true }, table(["Run", "Phase", "Cycle", "Last checkpoint"], runs.map(run =>
      [link(run.run_id, runUrl("overview", {}, run.run_id)), pill(run.phase), run.cycle, code(run.last_sequence)]))),
    pagination("Run history pagination",
      older ? link("Latest runs", "/") : null,
      runs.length === 100 ? link("Older runs", "/?runs_before=" + runs.at(-1).last_sequence) : null),
    configPanel()]);
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

function phaseRail(phase, continuation) {
  const visibleStages = continuation
    ? [["Continuation", ["ContinuationReview", "ContinuationDiscuss", "ContinuationDecision"]], ...stages.slice(1)]
    : stages;
  const order = visibleStages.flatMap(([, steps]) => steps);
  const done = endings.includes(phase);
  const at = order.indexOf(phase === "OnboardWorker" ? "Work" : phase);
  return node("ol", { class: "rail", "aria-label": "Run phases", "data-end": done ? phase.toLowerCase() : undefined },
    visibleStages.map(([name, steps]) => {
      const current = !done && steps.includes(order[at]);
      return node("li", { class: `g${steps.length}${current ? " on" : ""}`, "aria-current": current ? "step" : undefined },
        node("div", { class: "steps" }, steps.map(step => {
          const index = order.indexOf(step);
          return node("i", { "data-step": done || index < at ? "past" : index === at ? "now" : "next", title: title(step) });
        })),
        node("span", {}, name));
    }));
}

function executionPlan(tasks, flow, data) {
  const taskTable = table(["Task", "Worker", "State", "Depends on"], tasks.map(({ task, state: status }) => [
    node("details", { class: "plan-task" },
      node("summary", { "aria-label": `Task details for ${actorName(task.worker)}` },
        node("span", { class: "plan-task-preview" }, task.description)),
      pre(task.description, { tabindex: 0, "aria-label": "Task description" })),
    actorLink(task.worker), pill(status), task.depends_on.length ? code(task.depends_on.join(", ")) : "—",
  ]), "plan");
  return section({ label: "Execution plan", meta: flow.plan_id ? code(flow.plan_id) : null, flush: true },
    tasks.length ? taskTable : empty("Waiting for a frozen plan."),
    (data.acceptance ?? []).length ? node("details", {}, node("summary", {}, "Acceptance criteria"),
      node("ul", {}, data.acceptance.map(text => node("li", {}, text)))) : null,
    data.plan?.ownership ? node("details", {}, node("summary", {}, "Write ownership"),
      table(["Worker", "Paths"], data.plan.ownership.map((paths, index) => [
        actorLink(`worker_${index + 1}`), paths.map(path => code(path.path + (path.directory ? "/" : ""))),
      ]))) : null);
}

function continuationOverview(data) {
  const continuation = data.continuation;
  if (!continuation) return null;
  const count = data.stakeholders?.length ?? 0;
  const decision = continuation.decision;
  return section("Continuation", facts([
    ["Follow-up", pre(continuation.prompt)],
    ["Prior approval", readOnly ? code(continuation.approved_checkpoint) :
      go("Checkpoint #" + continuation.approved_checkpoint, runUrl("archive", { sequence: continuation.approved_checkpoint }))],
    ["Stakeholder review", `${continuation.reviewed} / ${count}`],
    ["Stakeholder discussion", `${continuation.discussed} / ${count}`],
    ["Decision", decision ? pill(decision.decision) : "Pending"],
    ["Rationale", decision ? pre(decision.reason) : null],
    ["Continuation plan", decision?.plan ? pre(decision.plan) : null],
    ["Timeline", [
      go("Open follow-up request", entryUrl("continuation-request-" + continuation.id)),
      decision ? go("Open decision", entryUrl("continuation-decision-" + continuation.id)) : null,
    ]],
  ]));
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
        node("span", { class: "hero-sub" }, `cycle ${data.run.cycle}`, flow.stopped ? pill(flow.stopped) : null),
        phase === "ProposeTeam" ? go("Open setup timeline", runUrl("history", { phase: "ProposeTeam" })) : null),
      phaseRail(phase, data.continuation)),
    continuationOverview(data),
    node("div", { class: "columns" },
      section({ label: "Agents", meta: agents.length || null, flush: true }, agents.length
        ? agents.map(agent => agentRow(agent, tasks.find(task => task.task.worker === agent.id), false))
        : empty("The roster appears once a team is proposed.")),
      node("div", { class: "col" },
        section("Run state", facts([
          ["Acknowledged", acknowledged.length ? acknowledged.map(id => actorLink(id)) : "None yet"],
          ["Worker request", flow.worker_addition ? go(flow.worker_addition, runUrl("triage")) : "None yet"],
          ["Verification", flow.verification ? [pill(flow.verification.passed ? "completed" : "failed"),
            flow.verification.candidate ? code(flow.verification.candidate) : null] : "No receipt yet"],
          ["Verdict", data.verdict ? pill(data.verdict.verdict) : null],
          ["Assessment", data.assessment ? pre(data.assessment) : null],
          ["Failure", data.latest_error ? [pre(asText(data.latest_error.data.text)), go("Open failure in timeline", entryUrl(data.latest_error.key))] : null],
        ])),
        section("Usage", data.metricsPending ? muted("Indexing checkpoint totals…") : null, facts(Object.entries(metrics).flatMap(([name, value]) => name === "token_breakdown"
          ? Object.entries(value).map(([flow, count]) => [title(flow) + " tokens", tokenCount(count)])
          : [[title(name), number.format(value)]])) ?? muted("Nothing metered yet.")))),
    data.continuation && !data.continuation.decision ? null : executionPlan(tasks, flow, data),
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
    ...(kind ? [select("kind", "All event types", ["chat", "tool", "tool_result", "job", "job_delivery", "ipc", "phase", "brief", "proposal", "guidance", "triage", "triage_delivery", "compaction", "verification", "contract_error", "error", "control", "continuation_request", "continuation_decision"].map(type => [type, title(type)]))] : []),
    node("input", { type: "number", min: "1", name: "cycle", value: query.cycle ?? "", placeholder: "Cycle", "aria-label": "Cycle" }),
    select("phase", "All phases", ["ProposeTeam", "ContinuationReview", "ContinuationDiscuss", "ContinuationDecision", "Plan", "PlanConsult", "PlanSynthesize", "Coordinate", "FreezePlan", "Acknowledge", "Work", "OnboardWorker", "Review", "ReviewConsult", "ReviewSynthesize", "Publish"].map(phase => [phase, title(phase)])),
    node("button", { type: "submit" }, "Apply"));
  return form;
}

function pagination(label, ...links) {
  const shown = links.filter(Boolean);
  return shown.length ? node("nav", { class: "pagination", "aria-label": label }, shown) : null;
}

function jobLink(data) {
  if (data.specialist) return actorLink(data.specialist, "Specialist conversation");
  if (data.job && ["delegate", "start_tool", "jobs"].includes(data.tool))
    return go(`Job #${data.job}`, entryUrl(`job-${data.job}`, "tools", { tab: "jobs" }));
  return null;
}

function historyCard(event, selected, call, anchor = true) {
  const data = event.data ?? {};
  let body;
  if (event.kind === "chat") {
    body = [asText(data.text) ? pre(asText(data.text)) : null,
      (data.calls ?? []).length ? node("div", { class: "call-list" }, data.calls.map(item =>
        link(item.tool, entryUrl(item.key), { class: call === item.key ? "highlight" : undefined, "data-call": item.key }))) : null];
  } else if (event.kind === "tool" || event.kind === "tool_result") {
    const tool = toolName(data);
    const key = data.call ?? event.key;
    body = node("div", { class: "tool-event" },
      link(tool, entryUrl(key), { class: "truncate", title: tool }),
      node("span", { class: "tool-event-state", "data-state": data.state }, title(data.state)),
      data.path ? node("code", { class: "truncate tool-event-path", title: data.path }, data.path) : jobLink(data),
      data.turn ? link("Chat", entryUrl(data.turn, "history", { call: key }),
        { class: "tool-event-chat", title: "Open original chat turn", "aria-label": "Jump to chat" }) : null);
  } else if (event.kind === "job_delivery") {
    body = row(go(`Job #${data.job}`, entryUrl(data.call, "tools", { tab: "jobs" })), muted("Result delivered"));
  } else if (event.kind === "ipc") {
    body = [row(caption("To"), (data.to ?? []).map(id => actorLink(id)), pill(data.delivery)), pre(data.body)];
  } else if (event.kind === "triage") {
    body = [row(go(`Information #${data.item}`, runUrl("triage", { item: data.item, inbox_before: data.item + 1 })),
      pill(data.state), data.decision?.priority ? pill(data.decision.priority) : null), pre(data.reason),
    data.decision?.recipients?.length ? row(caption("Route"), data.decision.recipients.map(id => actorLink(id))) : null];
  } else {
    body = pre(asText(data.text) || data.label || data.reason || "");
  }
  const kind = tag(event.kind);
  kind.title = title(event.kind);
  return node("article", { id: anchor ? "event-" + event.key : undefined, class: `entry${event.key === selected ? " selected" : ""}`, "data-kind": event.kind },
    node("header", {}, time(event.timestamp_ms), kind),
    node("div", { class: "entry-body" }, body),
    node("div", { class: "entry-actor truncate", title: actorName(event.actor) }, actorLink(event.actor)),
    node("footer", {}, tag(event.phase, "phase"), link("#" + event.sequence, entryUrl(event.key))));
}

function toolName(data) {
  return data.server ? `${data.server} / ${data.name ?? data.tool}` : data.tool || "Tool";
}

function toolTable(rows, route, page) {
  const jobs = page === "tools" && route.query.tab === "jobs";
  const columns = jobs ? ["Time", "Job", "Caller", "State", "Conversation"] :
    ["Time", "Tool", "Agent", "Path", "State", "Duration", "Conversation"];
  return table(columns, rows.map(event => {
    const call = event.data;
    const path = call.path ? link(call.path, runUrl("tools", { tab: "files", path: call.path })) : "—";
    const duration = call.elapsed_ms == null ? "—" : `${call.elapsed_ms} ms`;
    const cells = [time(event.timestamp_ms), link(toolName(call), entryUrl(event.key, page, route.query)),
      actorLink(event.actor), ...(jobs ? [] : [path]), pill(call.state), ...(jobs ? [] : [duration]),
      row(jobLink(call), call.turn ? go("Jump to chat", entryUrl(call.turn, "history", { call: event.key })) : null)];
    cells.selected = event.key === route.query.entry;
    return cells;
  }), jobs ? "jobs-table" : "tools");
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
    row(jobLink({ ...data, ...outcome }),
      data.turn ? go("Jump to chat", entryUrl(data.turn, "history", { call: data.call ?? event.key })) : null)),
    detail.tool_stages?.length > 1 ? node("div", { class: "inspector-section" },
      caption("Tool lifecycle"), detail.tool_stages.map(stage => row(
        stage.key === event.key ? pill(stage.state) : go(title(stage.state), entryUrl(stage.key, "history", { kind: stage.state === "queued" ? "tool" : "tool_result" })),
        spacer(), time(stage.timestamp_ms)))) : null,
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
      close ? [button("Copy link", () => {
        const url = new URL(location.href);
        if (readOnly) url.hash = "share=" + token;
        navigator.clipboard.writeText(url.href).catch(captureError);
      }),
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
function agentBadge(id) {
  const label = alias(id);
  return node("abbr", { class: "alias", "data-role": role(id), title: actorName(id) }, label === "Agent" ? id : label);
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
  return [...found.values()].reverse();
}

function sequence(items, query, focus) {
  if (!items.length) return section({ label: "Message sequence", flush: true }, empty("Waiting for agent traffic."));
  const ids = [...new Set([...(state.summary?.event?.workflow?.agents ?? []).map(agent => agent.id),
    ...items.flatMap(item => [item.event.actor, ...item.to.keys()])])];
  const tally = { delivered: 0, queued: 0, dropped: 0 };
  for (const item of items) for (const status of item.to.values()) tally[status] += 1;
  const lanes = (...content) => node("span", { class: "seq-lanes", "aria-hidden": "true" }, content);
  const diagram = node("div", { class: "seq" },
    node("div", { class: "seq-head" }, node("span", {}, "Message"),
      lanes(ids.map((id, index) => place(node("span", { class: "alias", "data-role": role(id), title: actorName(id) }, alias(id)), `${index * 2 + 1} / span 2`)))),
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
  const panel = section({ label: "Message sequence", meta: `${ids.length} participants | ${items.length} messages`, flush: true },
    node("div", { class: "seq-summary" },
      node("span", { "data-state": "delivered" }, mark("delivered"), `${tally.delivered} delivered`),
      node("span", { "data-state": "queued" }, mark("queued"), `${tally.queued} queued`),
      node("span", { "data-state": "dropped" }, mark("dropped"), `${tally.dropped} dropped`)),
    diagram,
    node("div", { class: "seq-legend" }, node("span", {}, mark("sender"), "sender"), node("span", {}, mark("delivered"), "delivered"),
      node("span", {}, mark("queued"), "queued"), node("span", {}, mark("dropped"), "dropped"),
      node("span", {}, node("i", { class: "wire fan" }), "fan-out"), spacer(),
      node("span", {}, node("kbd", {}, "↑/↓"), "select")));
  panel.classList.add("seq-panel");
  panel.style.setProperty("--lanes", ids.length);
  return panel;
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

function routeMatrix(routes, query) {
  const head = { label: "Route matrix", meta: "delivered / pending / dropped", flush: true };
  if (!routes.length) return section(head, empty("No matching routes."));
  const participants = new Set(routes.flatMap(entry => [entry.from, entry.to]));
  const ids = [...new Set([...roster().map(agent => agent.id), ...participants])].filter(id => participants.has(id));
  const counts = new Map();
  for (const entry of routes) {
    const key = entry.from + ">" + entry.to;
    const tally = counts.get(key) ?? { queued: 0, delivered: 0, dropped: 0 };
    tally[entry.state] = entry.count;
    counts.set(key, tally);
  }
  const total = tally => tally.delivered + tally.queued + tally.dropped;
  const peak = Math.max(1, ...[...counts.values()].map(total));
  const grid = table(["From / To", ...ids.map(agentBadge)], ids.map(from => [
    { scope: "row", cell: node("span", { class: "route-source" }, agentBadge(from), actorLink(from)) },
    ...ids.map(to => {
      const tally = counts.get(from + ">" + to);
      if (!tally) return "—";
      const description = `${actorName(from)} to ${actorName(to)}: ${tally.delivered} delivered, ${tally.queued} pending, ${tally.dropped} dropped`;
      return { class: `h${Math.ceil(total(tally) / peak * 5)}`, cell: link(`${tally.delivered}/${tally.queued}/${tally.dropped}`,
        runUrl("ipc", { ...query, tab: undefined, before: undefined, entry: undefined, from, to }), { title: description, "aria-label": description }) };
    }),
  ]), "matrix route-matrix");
  $("table", grid).style.setProperty("--recipient-count", ids.length);
  grid.dataset.scroll = "route-matrix";
  grid.tabIndex = 0;
  grid.setAttribute("role", "region");
  grid.setAttribute("aria-label", "Route matrix");
  return section(head, grid);
}

function communication(messages, query, focus) {
  if (query.tab !== "lanes") return sequence(messages, query, focus);
  const head = { label: "Agent lanes", meta: messages.length || null, flush: true };
  if (!messages.length) return section(head, empty("No matching messages."));
  const participants = new Set(messages.flatMap(({ event, to }) => [event.actor, ...to.keys()]));
  const agents = [...new Set([...roster().map(agent => agent.id), ...participants])].filter(id => participants.has(id));
  const grid = table(["Time", "Message", ...agents.map(agentBadge)], messages.map(({ event, keys, to }) => {
    const messageLink = link(event.data.body, entryUrl(event.key, "ipc", query), {
      class: "lane-message", "aria-label": `Message ${event.sequence} from ${actorName(event.actor)}`,
    });
    const cells = [time(event.timestamp_ms), messageLink, ...agents.map(agent => agent === event.actor ? pill("Sent")
      : to.has(agent) ? pill(to.get(agent)) : node("span", { class: "lane-mark", "aria-hidden": "true" }, "·"))];
    cells.selected = keys.includes(focus);
    cells.id = "event-" + (cells.selected ? focus : event.key);
    return cells;
  }), "matrix lanes");
  $("table", grid).style.setProperty("--agent-count", agents.length);
  grid.dataset.scroll = "agent-lanes";
  grid.tabIndex = 0;
  grid.setAttribute("role", "region");
  grid.setAttribute("aria-label", "Agent lanes");
  return section(head, grid);
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
    !readOnly ? [
      button("Submit information", () => openAction("triage"), { class: "primary", disabled: !state.live }),
      button("Send guidance", () => openAction("guide"), { disabled: !state.live }),
      button("Request worker", () => openAction("add_worker"), { disabled: !working }),
    ] : null), [
    workflow?.worker_addition ? section("Worker admission", node("p", {}, workflow.worker_addition),
      plan?.id ? row(caption("Amended plan"), code(plan.id)) : null,
      plan?.ownership ? node("details", {}, node("summary", {}, "Proposed write ownership"), pre(JSON.stringify(plan.ownership, null, 2))) : null) : null,
    section({ label: "Worker requests", meta: admissions.rows.length || null, flush: true }, admissions.rows.length
      ? table(["Request", "State", "Amended plan", "Activity"], admissions.rows.map(item => {
        const scope = { phase: "OnboardWorker", worker_request: item.id };
        return [node("div", {}, code(`#${item.id}`), pre(item.request), item.detail ? muted(item.detail) : null), pill(item.state),
          item.plan?.id ? node("details", {}, node("summary", {}, code(item.plan.id)), pre(JSON.stringify(item.plan.ownership, null, 2))) : "—",
          item.has_proposal_activity || item.has_ipc ? node("div", { class: "entry-body" },
            item.has_proposal_activity ? go("Lead proposal activity", runUrl("agent/lead_reviewer", scope)) : null,
            item.has_ipc ? go("Onboarding IPC", runUrl("ipc", scope)) : null) : "—"];
      }))
      : empty(readOnly ? "No worker has been requested." : "No worker has been requested. Request one while the run is in the work phase.")),
    section({ label: "Information inbox", meta: inbox.rows.length || null, flush: true },
      inbox.rows.map(item => {
        const awaitingDelivery = item.state === "routed" && !item.delivered?.length;
        return node("article", { class: `entry${Number(query.item) === item.id ? " selected" : ""}`, id: `inbox-${item.id}` },
          node("header", {}, time(item.submitted_ms), code(`#${item.id}`), pill(awaitingDelivery ? "Awaiting delivery" : item.state)),
          node("div", { class: "entry-body" }, pre(item.text),
            item.state === "queued" && state.live ? muted(state.live.paused
              ? "Triage resumes when the run resumes."
              : "Waiting for triage at the next runner checkpoint.") : null,
            item.decision?.reason ? muted(item.decision.reason) : null,
            item.decision?.recipients?.length ? row(caption("Route"), item.decision.recipients.map(id => actorLink(id))) : null,
            item.delivered?.length ? row(caption("Delivered to"), item.delivered.map(id => actorLink(id))) : null,
            item.decision?.worker_recommendation ? row(muted(item.decision.worker_recommendation),
              !readOnly ? button("Use in worker request", () => openAction("add_worker", item.decision.worker_recommendation), { disabled: !working }) : null) : null,
            !readOnly && (item.state === "failed" || awaitingDelivery) ? row(button("Retry triage", () => sendAction("retry_triage", String(item.id)).catch(captureError), { disabled: !state.live })) : null),
          node("footer", {}, item.decision?.category ? tag(item.decision.category, "category") : null,
            item.decision?.priority ? tag(item.decision.priority, "priority") : null));
      }),
      !inbox.rows.length ? empty(readOnly ? "Nothing submitted yet." : "Nothing submitted yet. Submit information and the lead reviewer routes it to the right agents.") : null,
      inbox.rows.length >= 60 ? pagination("Inbox pagination", link("Older information", runUrl("triage", { inbox_before: inbox.rows.at(-1).id }))) : null),
  ]);
}

async function historyView(route) {
  const { page, actor, query } = route;
  if (page === "triage") return triageView(route);
  const local = page === "agent" ? "agent/" + encodeURIComponent(actor) : page;
  const tab = query.tab ?? "all";
  const matrix = page === "ipc" && tab === "matrix";
  const kind = page === "ipc" || (page === "agent" && tab === "ipc") ? "ipc" :
    page === "tools" ? ({ mcp: "mcp", jobs: "job" }[tab] ?? "tool") :
      page === "agent" && tab === "tools" ? "tool" : query.kind;
  const [historyData, chosen, specialistEntry] = await Promise.all([
    matrix ? api(apiRun("routes", query)) : api(apiRun("history", { ...query, actor: actor ?? query.actor, kind,
      collapse_tools: page === "history" && !kind ? 1 : undefined,
      from: actor && tab === "ipc" && query.direction === "outgoing" ? actor : query.from,
      to: actor && tab === "ipc" && query.direction === "incoming" ? actor : query.to,
      path: tab === "files" ? query.path ?? "*" : query.path })),
    query.entry ? api(apiRun("entry/" + encodeURIComponent(query.entry))).catch(() => null) : null,
    page === "agent" && /^job-\d+$/.test(actor) ? api(apiRun("entry/" + actor)).catch(() => null) : null,
  ]);
  let { rows } = historyData;
  const messages = page === "ipc" && !matrix ? exchanges(rows.filter(event =>
    (!query.from || event.actor === query.from) && (!query.to || event.data.to?.includes(query.to)))) : [];
  // Inspect the first message in the sequence until the operator selects another.
  const diagram = page === "ipc" && tab === "all";
  const focus = query.entry ?? (diagram ? messages[0]?.event.key : undefined);
  const firstMessage = diagram && !query.entry ? messages[0] : null;
  const detail = chosen ?? (firstMessage ? { event: firstMessage.event, deliveries: [...firstMessage.to].map(([to, state]) => ({
    to, state, key: firstMessage.event.key, timestamp_ms: firstMessage.event.timestamp_ms,
  })) } : null);
  const head = [];
  if (page === "agent") {
    const flow = state.summary.event?.workflow;
    const specialist = specialistEntry?.event;
    const agent = flow?.agents?.find(entry => entry.id === actor) ??
      { id: actor, name: specialist ? toolName(specialist.data) : actor };
    const task = flow?.tasks?.find(entry => entry.task.worker === actor);
    const match = /^(worker|reviewer|stakeholder)_(\d+)$/.exec(actor ?? "");
    const persona = match ? state.summary[match[1] + "s"]?.[Number(match[2]) - 1]?.persona : null;
    const active = generation(actor);
    const field = (label, body) => body ? node("div", { class: "hero-field wide" }, caption(label),
      pre(body, { class: "agent-field-text", tabindex: 0, "aria-label": label, "data-scroll": `agent-${label.toLowerCase()}` })) : null;
    head.push(node("section", { class: "band agent-band", "data-scroll": "agent-band" },
      node("div", { class: "hero" }, caption("Agent"), node("h1", { class: "hero-title" }, agent.name),
        node("span", { class: "hero-sub" }, node("span", { class: `dot${active ? " live" : ""}`, "data-agent-dot": actor }), code(actor), task ? pill(task.state) : null,
          specialist ? go("Originating job", entryUrl(specialist.key, "tools", { tab: "jobs" })) : null)),
      field("Task", task?.task?.description ?? asText(specialist?.data.arguments)), field("Persona", persona),
      pre(active?.text || "", { class: "preview", hidden: !active, "data-preview-actor": actor })),
    tabs(local, [["all", "Conversation"], ["ipc", "Incoming & outgoing IPC"], ["tools", "Tools"]], tab, query));
    if (tab === "ipc") head.push(node("nav", { class: "tabs sub", "aria-label": "Message direction" },
      [["all", "All messages"], ["incoming", "Incoming"], ["outgoing", "Outgoing"]].map(([direction, label]) =>
        link(label, runUrl(local, { ...query, direction, entry: undefined, before: undefined }),
          { "aria-current": (query.direction ?? "all") === direction ? "page" : undefined }))));
  } else if (page === "history") {
    head.push(heading("Timeline", historyData.legacy ? "Recoverable checkpoint entries. Coverage depends on what the older run retained." : null,
      !readOnly ? [
        go("Checkpoint archive", runUrl("archive")),
        go(query.legacy ? "Audit timeline" : "Recoverable checkpoint entries", runUrl("history", { legacy: query.legacy ? undefined : 1 })),
      ] : null,
      state.live ? button(state.follow ? "Live follow" : "Follow paused", () => {
        state.follow = !state.follow;
        load(false);
      }, { "aria-pressed": String(state.follow) }) : null));
  } else {
    head.push(heading(views[page] ?? "Timeline"));
  }
  if (page === "ipc") head.push(tabs(local, [["all", "Sequence"], ["lanes", "Agent lanes"], ["matrix", "Route matrix"]], tab, query));
  if (page === "tools") head.push(tabs(local, [["all", "All tools"], ["jobs", "Jobs"], ["mcp", "MCP"], ["files", "File activity"]], tab, query));
  if (query.path && tab === "files") head.push(node("div", { class: "scope" }, caption("File"), code(query.path), go("All files", runUrl("tools", { tab: "files" }))));
  if ((page === "ipc" && (query.from || query.to || query.worker_request)) ||
      (page === "agent" && query.worker_request)) head.push(node("div", { class: "scope" },
    query.from ? pill("From " + actorName(query.from)) : null, query.to ? pill("To " + actorName(query.to)) : null,
    query.worker_request ? pill("Worker request " + query.worker_request) : null,
    go(page === "ipc" ? "Clear route" : "Clear scope", runUrl(local, { tab: query.tab }))));
  head.push(filters(route, { actor: page !== "agent", kind: page === "history" }));
  if (detail && page === "history" && !rows.some(entry => entry.key === detail.event.key)) {
    rows = [detail.event, ...rows].sort((a, b) => b.sequence - a.sequence);
  }
  let content;
  if (page === "ipc") content = matrix ? routeMatrix(rows, query) : communication(messages, query, focus);
  else if (page === "tools" || tab === "tools") content = section({ label: tab === "jobs" ? "Jobs" : tab === "mcp" ? "MCP calls" : "Tool calls", meta: rows.length || null, flush: true }, toolTable(rows, route, local));
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
  return view(head, matrix ? content : [content, pages], {
    aside: inspector(detail, query.entry ? runUrl(local, { ...query, entry: undefined }) : null), flush: true,
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
  const active = document.activeElement;
  return main.contains(active) && active.matches("input, textarea, select");

}

function patchChildren(parent, desired) {
  const prior = [...parent.childNodes];
  const keyed = new Map(prior.filter(child => child.nodeType === Node.ELEMENT_NODE && child.id)
    .map(child => [child.id, child]));
  const claimed = new Set();
  for (const [index, next] of desired.entries()) {
    const existing = next.id ? keyed.get(next.id) : prior[index];
    let current = next;
    if (existing && !claimed.has(existing) && existing.nodeType === next.nodeType &&
        existing.nodeName === next.nodeName && (existing.id ?? "") === (next.id ?? "")) {
      if (existing.isEqualNode(next)) current = existing;
      else if (existing.nodeType === Node.TEXT_NODE) {
        existing.textContent = next.textContent;
        current = existing;
      } else if (!existing.matches("a, button, input, textarea, select, option") &&
          existing.className === next.className) {
        const open = existing.tagName === "DETAILS" && existing.open;
        for (const name of existing.getAttributeNames()) {
          if (!next.hasAttribute(name) && !(name === "open" && open)) existing.removeAttribute(name);
        }
        for (const name of next.getAttributeNames()) {
          if (existing.getAttribute(name) !== next.getAttribute(name)) existing.setAttribute(name, next.getAttribute(name));
        }
        patchChildren(existing, [...next.childNodes]);
        current = existing;
      }
    }
    claimed.add(current);
    if (parent.childNodes[index] !== current) parent.insertBefore(current, parent.childNodes[index] ?? null);
  }
  for (const child of [...parent.childNodes]) if (!claimed.has(child)) child.remove();
}

async function load(restart = true, preserveInteraction = false) {
  const version = ++state.version;
  const previous = state.route.run;
  state.route = route();
  const wanted = state.route;
  if (restart) {
    state.refreshPending = false;
    selectRun(wanted.run ?? null);
    main.inert = true;
    if (previous !== wanted.run) { state.summary = null; state.live = null; }
  }
  main.setAttribute("aria-busy", "true");
  activeReads = new Set();
  for (const path of values.keys()) {
    if (!warm(path) && (restart || !path.startsWith("/api/run/" + wanted.run + "/"))) values.delete(path);
  }
  try {
    const [runs, summary, live, config, olderRuns, usage] = await Promise.all([
      readOnly ? { runs: [] } : api("/api/runs"),
      wanted.run ? api(apiRun("overview")) : null,
      wanted.run ? api(apiRun("live")).catch(() => null) : null,
      wanted.run || readOnly ? state.config : api("/api/config").catch(() => null),
      !wanted.run && !readOnly && wanted.query.runs_before
        ? api("/api/runs?before=" + encodeURIComponent(wanted.query.runs_before)) : null,
      wanted.run ? api(apiRun("metrics")).catch(() => null) : null,
    ]);
    if (version !== state.version) return;
    state.config = config;
    const navigationChanged = navigationKey(state.runs, state.summary) !== navigationKey(runs.runs, summary);
    state.runs = runs.runs;
    state.displayRuns = olderRuns?.runs ?? runs.runs;
    state.summary = summary ? { ...summary, metrics: usage?.metrics, metricsPending: usage?.pending } : null;
    state.live = live;
    if (restart || navigationChanged || !wanted.run) renderNavigation();
    else renderLive();
    const nodes = !wanted.run ? readOnly ? view(heading("Shared run"), muted("Open a run’s share link.")) : workspace()
      : wanted.page === "overview" ? overview()
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
      [element.dataset.scroll, element.scrollLeft, element.scrollTop]);
    if (!restart && settled) patchChildren(main, children(nodes));
    else fill(main, nodes);
    for (const [key, left, top] of scrolls) {
      const element = key ? $(`[data-scroll="${key}"]`, main) : main;
      if (!element || (restart && key === "inspector")) continue;
      element.scrollTo(left, top);
    }
    if (restart && wanted.query.entry) {
      document.getElementById("event-" + wanted.query.entry)?.scrollIntoView({ block: settled ? "nearest" : "center" });
    }
    if (restart && wanted.page === "triage" && wanted.query.item) {
      document.getElementById("inbox-" + wanted.query.item)?.scrollIntoView({ block: "center" });
    }
    rendered = true;
  } catch (error) {
    if (version === state.version) fill(main, view(heading("Run unavailable"), section("Connection", muted(error.message), !readOnly ? go("Workspace runs", "/") : null)));
  } finally {
    if (version === state.version) {
      rendered = true;
      main.inert = false;
      main.setAttribute("aria-busy", "false");
      if (state.refreshPending) scheduleUpdate();
    }
  }
}

function refreshLivePage() {
  const { page } = state.route;
  if (main.getAttribute("aria-busy") === "true") return;
  if (!state.refreshPending || (page === "history" && !state.follow) || hasPageInteraction()) return;
  state.refreshPending = false;
  load(false, true);
}

let actionState;
const descriptions = {
  start: ["Start a run", "Describe the outcome and constraints. Ratio will propose a roster and start the run.", "Request", true],
  continue: ["Continue run", "Stakeholders review and discuss your request. If the lead reviewer accepts it, work starts automatically in this run.", "Follow-up prompt", true],
  fork: ["Fork this run", "Start a new run with this roster. Leave the request empty to keep the original goal.", "Replacement request", false],
  resume_saved: ["Resume saved run", "Continue from the latest committed checkpoint with the current runner configuration.", "", false],
  add_worker: ["Request a worker", "Describe the role and scope. Writes remain gated until unanimous acknowledgement.", "Assignment and proposed scope", true],
  triage: ["Submit information", "The lead reviewer will classify and route this item automatically.", "Information", true],
  guide: ["Send guidance", "Sent verbatim to every agent active in the current phase. Use Submit information when the lead reviewer should decide who needs it.", "Guidance", true],
  stop: ["Stop this run", "Cancel active work and retain the last checkpoint.", "", false],
};

function openAction(name, initial = "") {
  if (readOnly) return;
  const [label, hint, field, required] = descriptions[name];
  actionState = { name, run: state.route.run, id: crypto.randomUUID(),
    checkpoint: name === "continue" ? state.summary?.run.last_sequence : undefined };
  $("#action-title").textContent = label;
  $("#action-hint").textContent = name === "resume_saved" && state.summary?.resume_phase
    ? `Continue from ${title(state.summary.resume_phase)} in the same run with the current limits and accumulated usage.`
    : hint;
  $("#action-label").textContent = field;
  $("#action-label").hidden = !field;
  const input = $("#action-text");
  input.value = initial;
  input.hidden = !field;
  input.required = required;
  input.maxLength = name === "start" || name === "fork" ? 65536 : 4096;
  $("#submit-action").textContent = label;
  // A known-bad profile cannot start a run. Say why before the request is typed.
  const blocked = (name === "start" || name === "fork" || name === "continue") && state.config?.ok === false;
  $("#submit-action").disabled = blocked;
  $("#start-config").hidden = name !== "start";
  $("#start-config").disabled = blocked;
  $(".action-error").textContent = blocked ? state.config.error : "";
  $(".action-error").hidden = !blocked;
  $("#action-dialog").showModal();
  (field ? input : $("#submit-action")).focus();
}

function postAction(action, text, run = state.route.run, id = crypto.randomUUID(), checkpoint) {
  const message = { type: "action", action: { action, text, run, id, checkpoint } };
  return new Promise((resolve, reject) => {
    if (!send(message)) {
      reject(new Error("Dashboard stream is reconnecting"));
      return;
    }
    actions.set(id, { message, resolve, reject });
  });
}

async function sendAction(action, text, run = state.route.run, id = crypto.randomUUID(), checkpoint) {
  if (readOnly) return;
  const receipt = await postAction(action, text, run, id, checkpoint);
  notify(receipt.detail);
  if (receipt.run_id && receipt.run_id !== state.route.run) navigate(runUrl("overview", {}, receipt.run_id));
  else load(false);
}

let shareTarget;
function showShareState({ active, url }) {
  $("#share-state").textContent = url
    ? "Copy this link before closing the dialog."
    : active ? "A link is active. Replacing it invalidates the previous link." : "No active link.";
  $("#share-link").hidden = !url;
  $("#share-url").value = url ?? "";
  $("#revoke-share").hidden = !active;
  $("#create-share").textContent = active ? "Replace link" : "Create link";
}

async function openShare() {
  const run = state.route.run;
  const status = await api(apiRun("share"));
  if (state.route.run !== run || $("#share-dialog").open) return;
  shareTarget = run;
  $("#share-error").hidden = true;
  showShareState(status);
  $("#share-dialog").showModal();
}

async function updateShare(action) {
  const run = shareTarget;
  const controls = [$("#create-share"), $("#revoke-share"), $("#close-share")];
  for (const control of controls) control.disabled = true;
  const errorLine = $("#share-error");
  errorLine.hidden = true;
  try {
    showShareState(await postAction(action, undefined, run));
  } catch (error) {
    errorLine.textContent = error.message;
    errorLine.hidden = false;
  } finally {
    for (const control of controls) control.disabled = false;
  }
}

$("#close-share").addEventListener("click", () => $("#share-dialog").close());
$("#share-dialog").addEventListener("cancel", event => {
  if ($("#close-share").disabled) event.preventDefault();
});
$("#create-share").addEventListener("click", () => updateShare("create_share"));
$("#revoke-share").addEventListener("click", () => updateShare("revoke_share"));
$("#copy-share").addEventListener("click", async () => {
  const input = $("#share-url");
  input.select();
  try {
    await navigator.clipboard.writeText(input.value);
    $("#share-state").textContent = "Link copied. Replacing it invalidates the previous link.";
  } catch {
    $("#share-state").textContent = "Copy the selected link.";
  }
});

document.addEventListener("click", event => {
  if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  const anchor = event.target.closest("a[href]");
  const href = anchor?.getAttribute("href");
  if (href === "/" || href?.startsWith("/?") || href?.startsWith("/run/")) {
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
$("#action-dialog").addEventListener("cancel", event => {
  if ($("#close-action").disabled) event.preventDefault();
});
$("#action-form").addEventListener("submit", async event => {
  event.preventDefault();
  const controls = [$("#submit-action"), $("#start-config"), $("#close-action")];
  for (const control of controls) control.disabled = true;
  const fromConfig = event.submitter?.id === "start-config";
  const { name, run, id, checkpoint } = actionState;
  const errorLine = $(".action-error");
  errorLine.hidden = true;
  try {
    await sendAction(fromConfig ? "start_config" : name, fromConfig ? undefined : $("#action-text").value, run, id, checkpoint);
    $("#action-dialog").close();
  } catch (error) {
    errorLine.textContent = error.message;
    errorLine.hidden = false;
  } finally {
    for (const control of controls) control.disabled = false;
  }
});
document.addEventListener("keydown", event => {
  if ($("#action-dialog").open || $("#share-dialog").open || event.metaKey || event.ctrlKey || event.altKey
    || event.target.matches?.("input, textarea, select") || !state.route.run) return;
  const page = Object.keys(views)[Number(event.key) - 1];
  const messages = [...document.querySelectorAll(".seq-row")];
  const step = { ArrowUp: -1, k: -1, ArrowDown: 1, j: 1 }[event.key];
  if (step && messages.length) {
    const current = messages.findIndex(message => message.classList.contains("selected"));
    const target = messages[current < 0 ? 0 : Math.max(0, Math.min(messages.length - 1, current + step))];
    event.preventDefault();
    if (target !== messages[current]) navigate(target.getAttribute("href"));
  } else if (event.key === "/") {
    event.preventDefault();
    $("#global-search input").focus();
  } else if (page && event.key.trim()) navigate(runUrl(page));
});
for (const event of ["focusout", "toggle", "close"]) document.addEventListener(event, scheduleUpdate, true);
load();
connectDashboard();
