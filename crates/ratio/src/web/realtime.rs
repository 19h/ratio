use super::*;
use flate2::{Compression, write::GzEncoder};
use futures_util::{SinkExt, StreamExt};
use ratio_protocol::RunMetrics;
use serde::Serialize;
use std::{
    io::Write,
    sync::{Mutex as StdMutex, Weak},
};
use tokio::{
    sync::{mpsc, watch},
    time::{interval, timeout},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::server::create_response_with_body,
        protocol::{Role, WebSocketConfig},
    },
};

const TICK: Duration = Duration::from_millis(100);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const CHUNK: usize = 128 * 1024;
// A 64 KiB run request can grow to 384 KiB when JSON escapes its contents.
const CLIENT_MESSAGE_LIMIT: usize = 400 * 1024;
const WARM: &[&str] = &[
    "history?collapse_tools=1",
    "history?kind=ipc",
    "history?kind=tool",
    "history?kind=mcp",
    "history?kind=job",
    "history?kind=tool&path=*",
    "inbox",
    "admissions",
    "checkpoints",
];

type Data = BTreeMap<String, Arc<Value>>;
type Feed = watch::Sender<Arc<Data>>;
type Grant = watch::Receiver<Option<Option<Vec<u8>>>>;

struct LiveSnapshot {
    run: String,
    operator: Arc<Value>,
    share: Arc<Value>,
}

pub(super) struct Hub {
    feeds: StdMutex<BTreeMap<Option<String>, Weak<Feed>>>,
    grants: StdMutex<BTreeMap<String, (u64, Grant)>>,
    grant_reads: Arc<Semaphore>,
    changes: watch::Sender<u64>,
    live: watch::Sender<Option<Arc<LiveSnapshot>>>,
    operator: Arc<Semaphore>,
    share: Arc<Semaphore>,
    hot: Arc<Semaphore>,
    queries: Arc<Semaphore>,
    guest_queries: Arc<Semaphore>,
    encoding: Arc<Semaphore>,
    guest_encoding: Arc<Semaphore>,
    encoded: Arc<StdMutex<EncodingCache>>,
}

impl Hub {
    pub(super) fn new() -> Self {
        Self {
            feeds: StdMutex::new(BTreeMap::new()),
            grants: StdMutex::new(BTreeMap::new()),
            grant_reads: Arc::new(Semaphore::new(2)),
            changes: watch::Sender::new(0),
            live: watch::Sender::new(None),
            operator: Arc::new(Semaphore::new(8)),
            share: Arc::new(Semaphore::new(24)),
            hot: Arc::new(Semaphore::new(2)),
            queries: Arc::new(Semaphore::new(2)),
            guest_queries: Arc::new(Semaphore::new(2)),
            encoding: Arc::new(Semaphore::new(2)),
            guest_encoding: Arc::new(Semaphore::new(2)),
            encoded: Arc::default(),
        }
    }

    fn subscribe(app: &Arc<Dashboard>, run: Option<String>) -> watch::Receiver<Arc<Data>> {
        let mut feeds = app.realtime.feeds.lock().expect("dashboard feeds");
        feeds.retain(|_, feed| feed.strong_count() != 0);
        if let Some(feed) = feeds.get(&run).and_then(Weak::upgrade) {
            return feed.subscribe();
        }
        let feed = Arc::new(Feed::new(Arc::default()));
        let receiver = feed.subscribe();
        feeds.insert(run.clone(), Arc::downgrade(&feed));
        tokio::spawn(project(app.clone(), run, feed));
        receiver
    }

    pub(super) fn invalidate(&self) {
        self.changes.send_modify(|revision| *revision += 1);
    }

    fn publish_runner(&self, status: ControlReply) {
        let operator = json!({ "data": status });
        let previous = self.live.borrow().clone();
        if previous
            .as_ref()
            .is_some_and(|previous| *previous.operator == operator)
        {
            return;
        }
        let share = json!({"data": {
            "paused": status.paused,
            "generations": status.generations,
            "token_usage": status.token_usage
        }});
        let share = previous
            .filter(|previous| previous.run == status.run_id && *previous.share == share)
            .map_or_else(|| Arc::new(share), |previous| previous.share.clone());
        self.live.send_replace(Some(Arc::new(LiveSnapshot {
            run: status.run_id,
            operator: Arc::new(operator),
            share,
        })));
    }

    fn query_pool(&self, surface: Surface) -> Arc<Semaphore> {
        match surface {
            Surface::Operator => self.queries.clone(),
            Surface::Share => self.guest_queries.clone(),
        }
    }
}

pub(super) fn observe(app: Arc<Dashboard>) {
    let workspace = app.workspace.clone();
    let hub = app.clone();
    tokio::spawn(async move {
        let mut timer = interval(TICK);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut source = None::<RunReader>;
        let mut version = None;
        let mut created = None;
        loop {
            timer.tick().await;
            let workspace = workspace.clone();
            let result = tokio::task::spawn_blocking(move || {
                let identity = std::fs::metadata(workspace.join(".ratio/runs.sqlite3"))
                    .ok()
                    .and_then(|meta| meta.created().ok());
                let replaced = created != identity;
                if source.is_none() || replaced {
                    source = Store::reader(&workspace).ok();
                }
                let next = source
                    .as_ref()
                    .and_then(|reader| reader.data_version().ok());
                // Reopen after an I/O error, including journal removal.
                if next.is_none() {
                    source = None;
                }
                (source, next, identity, replaced)
            })
            .await;
            let Ok((reader, next, identity, replaced)) = result else {
                return;
            };
            source = reader;
            created = identity;
            if replaced || version != next {
                version = next;
                hub.realtime.changes.send_modify(|revision| *revision += 1);
            }
        }
    });
    tokio::spawn(sample_runner(app));
}

async fn sample_runner(app: Arc<Dashboard>) {
    let mut run = None;
    loop {
        if app.realtime.live.receiver_count() == 0 {
            app.realtime
                .live
                .send_if_modified(|snapshot| snapshot.take().is_some());
            run = None;
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }
        if let Ok(mut telemetry) = control::telemetry(&app.workspace).await {
            while app.realtime.live.receiver_count() != 0 {
                let Ok(status) = control::read_telemetry(&mut telemetry).await else {
                    break;
                };
                app.realtime.publish_runner(status);
            }
            continue;
        }
        let request = ControlRequest {
            action: ControlAction::Status,
            guidance: None,
            run_id: run.clone(),
        };
        let status = control::request_control(&app.workspace, &request)
            .await
            .ok()
            .filter(|status| status.accepted || run.is_some());
        let delay = if let Some(status) = status {
            let matching = status.accepted && run.as_deref() == Some(status.run_id.as_str());
            run = Some(status.run_id.clone());
            if matching {
                app.realtime.publish_runner(status);
                TICK
            } else {
                Duration::from_millis(10)
            }
        } else {
            run = None;
            app.realtime
                .live
                .send_if_modified(|snapshot| snapshot.take().is_some());
            Duration::from_millis(250)
        };
        tokio::time::sleep(delay).await;
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Counts {
    cursor: u64,
    digest: Option<String>,
    totals: RunMetrics,
}

#[derive(Default)]
struct Projection {
    data: Data,
    checkpoint: Option<u64>,
    audit: Option<u64>,
    counts: Option<Counts>,
    index: Option<ratio_engine::web_index::DashboardIndex>,
    index_at: Option<u64>,
    saved_cursor: u64,
    full_at: Option<u64>,
    profile_stamp: Option<Option<(std::time::SystemTime, u64)>>,
    revision: u64,
}

fn put(data: &mut Data, path: String, value: Result<Value>) -> bool {
    let value = match value {
        Ok(data) => json!({"data":data}),
        Err(error) => json!({"error":error.to_string()}),
    };
    if data.get(&path).is_none_or(|previous| **previous != value) {
        data.insert(path, Arc::new(value));
        return true;
    }
    false
}

impl Projection {
    fn refresh(&mut self, app: &Dashboard, run: Option<&str>) -> Result<bool> {
        let journal = app.workspace.join(".ratio/runs.sqlite3");
        let Some(run) = run else {
            let runs = if journal.exists() {
                Store::reader(&app.workspace)?.dashboard_runs(None)?
            } else {
                Vec::new()
            };
            put(&mut self.data, "/api/runs".into(), Ok(json!({"runs":runs})));
            let cli = Cli::try_parse_from(
                std::iter::once(OsString::from("ra")).chain(app.launch_args.iter().cloned()),
            );
            let stamp = cli
                .ok()
                .and_then(|cli| Config::profile_candidate(cli.config.as_deref()))
                .and_then(|path| std::fs::metadata(path).ok())
                .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
            if self.profile_stamp != Some(stamp) {
                self.profile_stamp = Some(stamp);
                put(
                    &mut self.data,
                    "/api/config".into(),
                    Ok(app.config_summary()),
                );
            }
            return Ok(false);
        };
        let reader = Store::reader(&app.workspace)?;
        reader.dashboard_deadline(Duration::from_millis(500))?;
        let record = reader.run(run)?.context("Run not found")?;
        let current = record.last_sequence;
        let head = reader.audit_head(run)?;
        let prefix = format!("/api/run/{run}/");
        let cache = app
            .workspace
            .join(".ratio")
            .join(format!("web-metrics-v1-{run}.json"));
        let counts = self.counts.get_or_insert_with(|| {
            let stored = std::fs::read(&cache)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Counts>(&bytes).ok())
                .filter(|stored| {
                    stored.cursor <= current
                        && stored.digest.is_some()
                        && reader.checkpoint_digest(run, stored.cursor).ok().flatten()
                            == stored.digest
                })
                .unwrap_or_default();
            self.saved_cursor = stored.cursor;
            stored
        });
        if counts.cursor > current {
            *counts = Counts::default();
            self.full_at = None;
            self.saved_cursor = 0;
        }
        let before = counts.cursor;
        if counts.cursor != current {
            reader.advance_metrics(run, &mut counts.cursor, current, &mut counts.totals, 128)?;
        }
        reader.refresh_metric_budget(run, &mut counts.totals)?;
        let complete = counts.cursor == current;
        if counts.cursor != self.saved_cursor
            && (counts.cursor.saturating_sub(self.saved_cursor) >= 512
                || self.full_at.is_none() && complete)
        {
            counts.digest = reader.checkpoint_digest(run, counts.cursor)?;
            if let Ok(mut file) = tempfile::NamedTempFile::new_in(app.workspace.join(".ratio")) {
                if serde_json::to_writer(&mut file, counts).is_ok() && file.persist(&cache).is_ok()
                {
                    self.saved_cursor = counts.cursor;
                }
            }
        }
        let mut revision_needed = self.audit != Some(head) || self.checkpoint != Some(current);
        let overview_path = format!("{prefix}overview");
        if self.checkpoint != Some(current)
            || self
                .data
                .get(&overview_path)
                .is_none_or(|record| record.get("error").is_some())
        {
            let mut summary = overview(&reader, run, Some(&counts.totals));
            if let Ok(summary) = &mut summary {
                summary
                    .as_object_mut()
                    .expect("overview object")
                    .remove("metrics");
            }
            put(&mut self.data, overview_path, summary);
        }
        put(
            &mut self.data,
            format!("{prefix}metrics"),
            Ok(json!({
                "metrics":counts.totals, "pending":!complete,
            })),
        );
        if complete {
            self.full_at = Some(current);
        }
        if before == counts.cursor && !complete {
            anyhow::bail!("Run history changed while indexing");
        }
        let changed = if let Some(after) = self.audit.filter(|old| *old < head) {
            let changes = reader.audit_updates(run, after)?;
            (changes.last().and_then(|item| item["sequence"].as_u64()) == Some(head))
                .then_some(changes)
        } else if self.audit == Some(head) {
            Some(Vec::new())
        } else {
            None
        };
        let index_was_current = self.index_at == self.audit;
        let mut index_failed = None;
        if self.index_at != Some(head) {
            let advance = (|| -> Result<()> {
                let index = match &mut self.index {
                    Some(index) => index,
                    None => self
                        .index
                        .insert(ratio_engine::web_index::DashboardIndex::open(
                            &app.workspace.join(".ratio"),
                            &reader,
                            run,
                        )?),
                };
                if index.advance(&reader, run, head)? {
                    put(
                        &mut self.data,
                        format!("{prefix}routes"),
                        Ok(json!({"rows":index.routes()?})),
                    );
                    self.index_at = Some(head);
                }
                Ok(())
            })();
            if let Err(error) = advance {
                index_failed = Some(error);
            }
        }
        for spec in WARM {
            let (view, query) = split_query(spec);
            let needed = match view {
                "inbox" | "admissions" => true,
                "checkpoints" => self.checkpoint != Some(current),
                "history" if self.audit == Some(0) && head == 0 => self.checkpoint != Some(current),
                "history" => changed.as_ref().is_none_or(|changes| {
                    changes.iter().any(|event| {
                        let kind = event["kind"].as_str().unwrap_or_default();
                        match query.get("kind").map(String::as_str) {
                            Some("ipc") => kind == "ipc",
                            Some("tool" | "job" | "mcp") => matches!(kind, "tool" | "tool_result"),
                            _ => true,
                        }
                    })
                }),
                _ => true,
            };
            let key = format!("{prefix}{spec}");
            if view == "history" && head != 0 {
                if self.index_at == Some(head)
                    && (needed || !index_was_current || !self.data.contains_key(&key))
                {
                    let bucket = match query.get("kind").map(String::as_str) {
                        Some("tool") if query.contains_key("path") => "file",
                        Some(kind) => kind,
                        None => "timeline",
                    };
                    let index = self.index.as_ref().expect("current dashboard index");
                    put(
                        &mut self.data,
                        key,
                        index
                            .window(&reader, run, bucket)
                            .map(|rows| json!({"rows":rows})),
                    );
                } else if !self.data.contains_key(&key) {
                    put(&mut self.data, key, select(&reader, run, view, &query));
                }
            } else if needed || !self.data.contains_key(&key) {
                let value = if view == "checkpoints" {
                    reader
                        .dashboard_checkpoints(run, 0)
                        .map(|rows| json!({"rows":rows}))
                        .map_err(Into::into)
                } else {
                    select(&reader, run, view, &query)
                };
                let changed = put(&mut self.data, key, value);
                revision_needed |= changed && matches!(view, "inbox" | "admissions");
            }
        }
        if let Some(error) = index_failed {
            let key = format!("{prefix}routes");
            if !self.data.contains_key(&key) {
                put(&mut self.data, key, Err(error));
            }
        }
        if app.share.is_some() {
            put(
                &mut self.data,
                format!("{prefix}share"),
                reader
                    .web_share_digest(run)
                    .map(|digest| json!({"active":digest.is_some()}))
                    .map_err(Into::into),
            );
        }
        if revision_needed {
            self.revision += 1;
            put(
                &mut self.data,
                format!("{prefix}revision"),
                Ok(json!(self.revision)),
            );
        }
        self.checkpoint = Some(current);
        self.audit = Some(head);
        Ok(!complete || self.index_at != Some(head))
    }
}

async fn project(app: Arc<Dashboard>, run: Option<String>, feed: Arc<Feed>) {
    let mut changes = app.realtime.changes.subscribe();
    let mut projection = Projection::default();
    loop {
        let permit = tokio::select! {
            permit = app.realtime.hot.clone().acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return },
            _ = feed.closed() => return,
        };
        let worker = app.clone();
        let selected = run.clone();
        changes.borrow_and_update();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = projection.refresh(&worker, selected.as_deref());
            (projection, result)
        })
        .await;
        let Ok((next, result)) = result else { return };
        projection = next;
        let more = match result {
            Ok(more) => more,
            Err(error) => {
                if let Some(run) = &run {
                    if projection.checkpoint.is_none() {
                        put(
                            &mut projection.data,
                            format!("/api/run/{run}/overview"),
                            Err(error),
                        );
                    }
                } else if projection.data.is_empty() {
                    put(&mut projection.data, "/api/runs".into(), Err(error));
                }
                true
            }
        };
        feed.send_if_modified(|current| {
            if same_data(current, &projection.data) {
                return false;
            }
            *current = Arc::new(projection.data.clone());
            true
        });
        tokio::select! {
            _ = feed.closed() => return,
            _ = changes.changed() => {},
            _ = tokio::time::sleep(Duration::from_millis(if more { 25 } else { 1000 })) => {},
        }
    }
}

fn same_data(a: &Data, b: &Data) -> bool {
    a.len() == b.len()
        && a.iter().all(|(key, value)| {
            b.get(key)
                .is_some_and(|current| Arc::ptr_eq(value, current))
        })
}

pub(super) fn upgrade(
    app: Arc<Dashboard>,
    surface: Surface,
    mut request: Request<Incoming>,
) -> Reply {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok());
    let allowed_origin = match surface {
        Surface::Operator => origin == Some(&format!("http://{}", app.host)),
        Surface::Share => app.share.as_ref().is_some_and(|site| {
            origin == Some(&site.origin) || origin == Some(&format!("http://{}", site.loopback))
        }),
    };
    if !allowed_origin {
        return error(StatusCode::FORBIDDEN, "Invalid dashboard origin");
    }
    let pool = match surface {
        Surface::Operator => &app.realtime.operator,
        Surface::Share => &app.realtime.share,
    };
    let Ok(permit) = pool.clone().try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Dashboard stream capacity reached",
        );
    };
    let Ok(mut response) = create_response_with_body(&request, || ()) else {
        return error(StatusCode::BAD_REQUEST, "Invalid WebSocket upgrade");
    };
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let _permit = permit;
        if let Ok(Ok(io)) = timeout(SEND_TIMEOUT, upgrade).await {
            let config = WebSocketConfig::default()
                .max_message_size(Some(CLIENT_MESSAGE_LIMIT))
                .max_frame_size(Some(CLIENT_MESSAGE_LIMIT))
                .write_buffer_size(0)
                .max_write_buffer_size(2 * CHUNK);
            let socket =
                WebSocketStream::from_raw_socket(TokioIo::new(io), Role::Server, Some(config))
                    .await;
            session(app, surface, socket).await;
        }
    });
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response.map(|_| Full::new(Bytes::new()).boxed_unsync())
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Client {
    Hello {
        version: u32,
        token: String,
        run: Option<String>,
        gzip: bool,
    },
    Select {
        run: Option<String>,
    },
    Query {
        id: u64,
        paths: Vec<String>,
    },
    Action {
        action: Action,
    },
    Ack {
        sequence: u64,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Server {
    Welcome { version: u32 },
    Unauthorized,
    Data { scope: Option<String>, values: Data },
    Query { id: u64, values: Data },
    Receipt { id: String, result: Value },
}

struct Packet {
    body: Server,
    compress: bool,
}

#[derive(Clone)]
struct Outbound {
    data: mpsc::Sender<Packet>,
    priority: mpsc::Sender<Server>,
}

fn packet(tx: &Outbound, body: Server, compress: bool) -> bool {
    match body {
        Server::Welcome { .. } | Server::Unauthorized | Server::Receipt { .. } => {
            tx.priority.try_send(body).is_ok()
        }
        _ => tx.data.try_send(Packet { body, compress }).is_ok(),
    }
}

pub(super) async fn authorized_share(app: &Arc<Dashboard>, run: &str, hash: &[u8]) -> bool {
    let version = *app.realtime.changes.borrow();
    let mut grant = {
        let mut grants = app.realtime.grants.lock().expect("dashboard share grants");
        grants.retain(|_, (prior, _)| *prior == version);
        if let Some((_, grant)) = grants.get(run) {
            grant.clone()
        } else {
            let (sender, grant) = watch::channel(None);
            // Keep memory bounded even if somebody probes thousands of unknown run IDs.
            if grants.len() < 64 {
                grants.insert(run.to_owned(), (version, grant.clone()));
            }
            let run = run.to_owned();
            let app = app.clone();
            tokio::spawn(async move {
                let Ok(permit) = app.realtime.grant_reads.clone().acquire_owned().await else {
                    return;
                };
                let workspace = app.workspace.clone();
                let hash = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let reader = Store::reader(&workspace).ok()?;
                    reader.dashboard_deadline(Duration::from_millis(150)).ok()?;
                    reader.web_share_digest(&run).ok().flatten()
                })
                .await
                .ok()
                .flatten();
                sender.send_replace(Some(hash));
            });
            grant
        }
    };
    match timeout(Duration::from_secs(1), grant.wait_for(Option::is_some)).await {
        Ok(Ok(value)) => value.as_ref().and_then(Option::as_deref) == Some(hash),
        _ => false,
    }
}

fn change_set(now: &Arc<Data>, last: &mut Arc<Data>, surface: Surface) -> Data {
    let values = now
        .iter()
        .filter_map(|(key, value)| {
            if last.get(key).is_some_and(|old| Arc::ptr_eq(old, value)) {
                return None;
            }
            if surface == Surface::Share {
                let view = run_route(key.split('?').next().unwrap_or(key)).map(|(_, view)| view);
                match view {
                    Some("share" | "checkpoints") => return None,
                    Some("history") if value["data"]["legacy"] == true => {
                        return Some((key.clone(), Arc::new(json!({"data":{"rows":[]}}))));
                    }
                    Some("overview") if value.get("data").is_some() => {
                        let mut value = (**value).clone();
                        value["data"]["parent_run"] = Value::Null;
                        return Some((key.clone(), Arc::new(value)));
                    }
                    _ => {}
                }
            }
            Some((key.clone(), value.clone()))
        })
        .collect();
    *last = now.clone();
    values
}

async fn session(
    app: Arc<Dashboard>,
    surface: Surface,
    socket: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
) {
    let (sink, mut source) = socket.split();
    let Ok(Some(Ok(Message::Text(first)))) = timeout(SEND_TIMEOUT, source.next()).await else {
        return;
    };
    let Ok(Client::Hello {
        version: 1,
        token,
        run,
        gzip,
    }) = serde_json::from_str(&first)
    else {
        return;
    };
    let mut selected = run.filter(|run| valid_run(run));
    let share_hash = if surface == Surface::Share {
        Some(digest(&SHA256, token.as_bytes()).as_ref().to_vec())
    } else {
        None
    };
    let allowed = if let Some(hash) = &share_hash {
        if let Some(run) = &selected {
            token.len() == 64
                && token.bytes().all(|b| b.is_ascii_hexdigit())
                && authorized_share(&app, run, hash).await
        } else {
            false
        }
    } else {
        app.authorization.strip_prefix("Bearer ") == Some(token.as_str())
    };
    let (data, rx) = mpsc::channel(24);
    let (priority, urgent) = mpsc::channel(8);
    let tx = Outbound { data, priority };
    let credits = Arc::new(Semaphore::new(2));
    let pending = Arc::new(StdMutex::new(VecDeque::new()));
    let encoding = match surface {
        Surface::Operator => app.realtime.encoding.clone(),
        Surface::Share => app.realtime.guest_encoding.clone(),
    };
    let mut writer = tokio::spawn(write_packets(
        sink,
        rx,
        urgent,
        encoding,
        app.realtime.encoded.clone(),
        credits.clone(),
        pending.clone(),
        gzip,
    ));
    if !allowed {
        packet(&tx, Server::Unauthorized, false);
        drop(tx);
        let _ = writer.await;
        return;
    }
    packet(&tx, Server::Welcome { version: 1 }, false);
    let mut workspace = (surface == Surface::Operator).then(|| Hub::subscribe(&app, None));
    let mut run_feed = selected.clone().map(|run| Hub::subscribe(&app, Some(run)));
    let mut live = app.realtime.live.subscribe();
    let mut changes = app.realtime.changes.subscribe();
    let mut last_workspace = Arc::default();
    let mut last_run = Arc::default();
    let mut send_view = true;
    let mut send_workspace = true;
    let mut send_live = true;
    let mut last_live: Option<Option<Arc<Value>>> = None;
    let mut queries = tokio::task::JoinSet::new();
    let mut actions = tokio::task::JoinSet::new();

    loop {
        // Check revocation before enqueuing more data for a shared connection.
        if let Some(hash) = &share_hash {
            if changes.has_changed().unwrap_or(false) {
                changes.borrow_and_update();
                let Some(run) = &selected else { break };
                if !authorized_share(&app, run, hash).await {
                    packet(&tx, Server::Unauthorized, false);
                    drop(tx);
                    let _ = timeout(SEND_TIMEOUT, &mut writer).await;
                    writer.abort();
                    return;
                }
            }
        }
        if send_workspace {
            send_workspace = false;
            if let Some(feed) = &mut workspace {
                let values = change_set(&feed.borrow_and_update(), &mut last_workspace, surface);
                if !send_data(&tx, None, values) {
                    break;
                }
            }
        }
        if send_view {
            send_view = false;
            if let Some(feed) = &mut run_feed {
                let values = change_set(&feed.borrow_and_update(), &mut last_run, surface);
                if !send_data(&tx, selected.clone(), values) {
                    break;
                }
            }
        }
        if send_live {
            send_live = false;
            let snapshot = live.borrow_and_update();
            if let Some(run) = &selected {
                let data = snapshot
                    .as_ref()
                    .filter(|snapshot| snapshot.run == *run)
                    .map(|snapshot| match surface {
                        Surface::Operator => snapshot.operator.clone(),
                        Surface::Share => snapshot.share.clone(),
                    });
                let same = match (&last_live, &data) {
                    (Some(Some(last)), Some(current)) => Arc::ptr_eq(last, current),
                    (Some(None), None) => true,
                    _ => false,
                };
                if !same {
                    last_live = Some(data.clone());
                    let values = BTreeMap::from([(
                        format!("/api/run/{run}/live"),
                        data.unwrap_or_else(|| Arc::new(json!({"data":null}))),
                    )]);
                    if !send_data(&tx, selected.clone(), values) {
                        break;
                    }
                }
            }
        }

        tokio::select! {
            _ = &mut writer => return,
            _ = wait_feed(&mut workspace) => send_workspace = true,
            _ = wait_feed(&mut run_feed) => send_view = true,
            _ = live.changed() => send_live = true,
            _ = changes.changed(), if share_hash.is_some() => {
                // changed() marks the revision seen, so validate immediately here.
                if let (Some(run), Some(hash)) = (&selected, &share_hash) {
                    if !authorized_share(&app, run, hash).await {
                        packet(&tx, Server::Unauthorized, false);
                        drop(tx);
                        let _ = timeout(SEND_TIMEOUT, &mut writer).await;
                        writer.abort();
                        return;
                    }
                }
            },
            Some(_) = queries.join_next(), if !queries.is_empty() => {},
            Some(_) = actions.join_next(), if !actions.is_empty() => {},
            incoming = source.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Close(_) => break,
                    Message::Text(text) => match serde_json::from_str(&text) {
                        Ok(Client::Ack { sequence }) => {
                            let mut pending = pending.lock().expect("dashboard frame acknowledgements");
                            if pending.front() == Some(&sequence) {
                                pending.pop_front();
                                credits.add_permits(1);
                            }
                        }
                        Ok(Client::Select { run }) if surface == Surface::Operator => {
                            let run = run.filter(|run| valid_run(run));
                            if selected != run {
                                selected = run;
                                run_feed = selected.clone().map(|run| Hub::subscribe(&app, Some(run)));
                                queries.abort_all();
                                last_run = Arc::default();
                                last_live = None;
                                send_view = true;
                                send_live = true;
                            }
                        }
                        Ok(Client::Query { id, paths }) if queries.len() < 4 && paths.len() <= 12 => {
                            queries.spawn(deliver_query(app.clone(), surface, selected.clone(), share_hash.clone(), tx.clone(), id, paths));
                        }
                        Ok(Client::Action { action }) if surface == Surface::Operator => {
                            let app = app.clone();
                            let tx = tx.clone();
                            let id = action.id.clone();
                            if actions.len() >= 4 {
                                packet(&tx, Server::Receipt { id, result: json!({"error":"Dashboard action capacity reached"}) }, false);
                            } else {
                                actions.spawn(async move {
                                    let result = match app.action(action).await {
                                        Ok(value) => json!({"data":value}),
                                        Err(error) => json!({"error":format!("{error:#}")}),
                                    };
                                    packet(&tx, Server::Receipt { id, result }, false);
                                });
                            }
                        }
                        _ => break,
                    },
                    _ => {}
                }
            }
        }
    }
    writer.abort();
}

async fn wait_feed(feed: &mut Option<watch::Receiver<Arc<Data>>>) {
    if let Some(feed) = feed {
        let _ = feed.changed().await;
    } else {
        std::future::pending().await
    }
}

fn send_data(tx: &Outbound, scope: Option<String>, mut values: Data) -> bool {
    if let Some(key) = values
        .keys()
        .find(|key| key.ends_with("/overview"))
        .cloned()
    {
        let overview =
            BTreeMap::from([(key.clone(), values.remove(&key).expect("overview present"))]);
        if !packet(
            tx,
            Server::Data {
                scope: scope.clone(),
                values: overview,
            },
            true,
        ) {
            return false;
        }
    }
    values.is_empty() || packet(tx, Server::Data { scope, values }, true)
}

fn split_query(spec: &str) -> (&str, Query) {
    let (name, params) = spec.split_once('?').unwrap_or((spec, ""));
    (
        name,
        url::form_urlencoded::parse(params.as_bytes())
            .into_owned()
            .collect(),
    )
}

async fn deliver_query(
    app: Arc<Dashboard>,
    surface: Surface,
    selected: Option<String>,
    hash: Option<Vec<u8>>,
    tx: Outbound,
    id: u64,
    paths: Vec<String>,
) {
    let pool = app.realtime.query_pool(surface);
    let Ok(permit) = pool.try_acquire_owned() else {
        let mut values = Data::new();
        for path in paths {
            put(
                &mut values,
                path,
                Err(anyhow::anyhow!("Dashboard query capacity reached")),
            );
        }
        packet(&tx, Server::Query { id, values }, false);
        return;
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut values = Data::new();
        for path in paths {
            let result = query(&app, surface, selected.as_deref(), hash.as_deref(), &path);
            put(&mut values, path, result);
        }
        values
    })
    .await;
    if let Ok(values) = result {
        packet(&tx, Server::Query { id, values }, true);
    }
}

fn query(
    app: &Dashboard,
    surface: Surface,
    selected: Option<&str>,
    hash: Option<&[u8]>,
    path: &str,
) -> Result<Value> {
    ensure!(path.len() <= 4096, "Dashboard query is too long");
    let (path, query) = split_query(path);
    if surface == Surface::Operator && path == "/api/config" {
        return Ok(app.config_summary());
    }
    if surface == Surface::Operator && path == "/api/runs" {
        let reader = Store::reader(&app.workspace)?;
        reader.dashboard_deadline(Duration::from_secs(2))?;
        let before = query
            .get("before")
            .and_then(|before| before.parse::<u64>().ok())
            .filter(|value| *value <= i64::MAX as u64);
        return Ok(json!({"runs":reader.dashboard_runs(before)?}));
    }
    let (run, view) = run_route(path).context("Unknown dashboard query")?;
    ensure!(
        Some(run) == selected,
        "Dashboard query is outside the selected run"
    );
    let reader = Store::reader(&app.workspace)?;
    reader.dashboard_deadline(Duration::from_secs(2))?;
    if let Some(hash) = hash {
        ensure!(
            permits_share(&reader, run, hash),
            "Invalid or revoked run share link"
        );
        ensure!(
            (matches!(view, "history" | "routes" | "inbox" | "admissions")
                || view
                    .strip_prefix("entry/")
                    .is_some_and(|key| !key.starts_with("legacy-")))
                && !query.contains_key("legacy"),
            "Endpoint is unavailable for shared runs"
        );
        if view == "history" && reader.audit_head(run)? == 0 {
            return Ok(json!({"rows":[]}));
        }
    }
    if view == "checkpoints" {
        let after = query
            .get("after")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v <= i64::MAX as u64)
            .unwrap_or(0);
        return Ok(json!({"rows":reader.dashboard_checkpoints(run,after)?}));
    }
    ensure!(
        view != "overview",
        "Overview is supplied by the run subscription"
    );
    select(&reader, run, view, &query)
}

pub(super) fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes)?;
    encoder.finish()
}

#[derive(Default)]
struct EncodingCache {
    entries: VecDeque<EncodingEntry>,
    bytes: usize,
}

struct EncodingEntry {
    keys: Vec<Weak<Value>>,
    use_gzip: bool,
    chunks: Arc<[Encoded]>,
    bytes: usize,
}

impl EncodingCache {
    fn get(&self, values: &Data, use_gzip: bool) -> Option<Arc<[Encoded]>> {
        self.entries
            .iter()
            .rev()
            .find(|entry| {
                entry.use_gzip == use_gzip
                    && entry.keys.len() == values.len()
                    && entry
                        .keys
                        .iter()
                        .zip(values.values())
                        .all(|(key, value)| key.as_ptr() == Arc::as_ptr(value))
            })
            .map(|entry| entry.chunks.clone())
    }

    fn insert(&mut self, values: &Data, use_gzip: bool, chunks: Arc<[Encoded]>) {
        const MAX_BYTES: usize = 4 * 1024 * 1024;
        let bytes = chunks
            .iter()
            .map(|chunk| chunk.content.len())
            .sum::<usize>();
        if bytes > MAX_BYTES || self.get(values, use_gzip).is_some() {
            return;
        }
        while self.entries.len() >= 64 || self.bytes + bytes > MAX_BYTES {
            self.bytes -= self
                .entries
                .pop_front()
                .expect("encoding cache has an entry")
                .bytes;
        }
        self.bytes += bytes;
        self.entries.push_back(EncodingEntry {
            keys: values.values().map(Arc::downgrade).collect(),
            use_gzip,
            chunks,
            bytes,
        });
    }
}

struct Encoded {
    content: Vec<u8>,
    compressed: bool,
    final_chunk: bool,
}

#[allow(clippy::too_many_arguments)]
async fn write_packets<S>(
    mut sink: S,
    mut rx: mpsc::Receiver<Packet>,
    mut urgent: mpsc::Receiver<Server>,
    encoding: Arc<Semaphore>,
    cache: Arc<StdMutex<EncodingCache>>,
    credits: Arc<Semaphore>,
    pending: Arc<StdMutex<VecDeque<u64>>>,
    accepts_gzip: bool,
) where
    S: futures_util::Sink<Message> + Unpin,
{
    let mut sequence: u64 = 0;
    let mut heartbeat = interval(Duration::from_secs(15));
    heartbeat.tick().await;
    loop {
        let packet = tokio::select! {
            biased;
            Some(message) = urgent.recv() => {
                if !send_priority(&mut sink, &message).await { return }
                if matches!(message, Server::Unauthorized) { break }
                continue;
            }
            packet = rx.recv() => match packet { Some(packet) => packet, None => break },
            _ = heartbeat.tick() => {
                if !matches!(timeout(SEND_TIMEOUT, sink.send(Message::Ping(Bytes::new()))).await, Ok(Ok(()))) { return }
                continue;
            }
        };
        let use_gzip = packet.compress && accepts_gzip;
        let cached = if let Server::Data { values, .. } = &packet.body {
            cache
                .lock()
                .expect("dashboard encoding cache")
                .get(values, use_gzip)
        } else {
            None
        };
        let chunks = if let Some(cached) = cached {
            cached
        } else {
            let Ok(permit) = encoding.clone().acquire_owned().await else {
                return;
            };
            let shared = Arc::clone(&cache);
            let result = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                encode_body(packet.body, use_gzip, &shared)
            })
            .await;
            let Ok(Ok(chunks)) = result else { return };
            chunks
        };
        for chunk in chunks.iter() {
            let limit = tokio::time::sleep(SEND_TIMEOUT);
            tokio::pin!(limit);
            let credit = loop {
                tokio::select! {
                    biased;
                    _ = &mut limit => return,
                    Some(message) = urgent.recv() => {
                        if !send_priority(&mut sink, &message).await { return }
                        if matches!(message, Server::Unauthorized) {
                            let _ = timeout(SEND_TIMEOUT, sink.close()).await;
                            return;
                        }
                    }
                    permit = credits.acquire() => match permit { Ok(permit) => break permit, Err(_) => return },
                }
            };
            sequence += 1;
            let flags = u8::from(chunk.compressed) | (u8::from(chunk.final_chunk) << 1);
            let mut bytes = Vec::with_capacity(chunk.content.len() + 9);
            bytes.push(flags);
            bytes.extend_from_slice(&sequence.to_be_bytes());
            bytes.extend_from_slice(&chunk.content);
            pending
                .lock()
                .expect("dashboard frame acknowledgements")
                .push_back(sequence);
            credit.forget();
            if !matches!(
                timeout(SEND_TIMEOUT, sink.send(Message::Binary(bytes.into()))).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    }
    let _ = timeout(SEND_TIMEOUT, sink.close()).await;
}

fn encode_body(
    body: Server,
    use_gzip: bool,
    cache: &StdMutex<EncodingCache>,
) -> serde_json::Result<Arc<[Encoded]>> {
    let reusable = match &body {
        Server::Data { values, .. } => Some(values),
        _ => None,
    };
    if let Some(values) = reusable {
        if let Some(chunks) = cache
            .lock()
            .expect("dashboard encoding cache")
            .get(values, use_gzip)
        {
            return Ok(chunks);
        }
    }
    let bytes = serde_json::to_vec(&body)?;
    let mut chunks = bytes.chunks(CHUNK).peekable();
    let mut encoded = Vec::new();
    while let Some(chunk) = chunks.next() {
        let compressed = (use_gzip && chunk.len() >= 1024)
            .then(|| gzip(chunk).ok())
            .flatten()
            .filter(|data| data.len() * 10 <= chunk.len() * 9);
        let is_compressed = compressed.is_some();
        encoded.push(Encoded {
            content: compressed.unwrap_or_else(|| chunk.to_vec()),
            compressed: is_compressed,
            final_chunk: chunks.peek().is_none(),
        });
    }
    let encoded: Arc<[Encoded]> = encoded.into();
    if let Some(values) = reusable {
        cache
            .lock()
            .expect("dashboard encoding cache")
            .insert(values, use_gzip, encoded.clone());
    }
    Ok(encoded)
}

async fn send_priority<S: futures_util::Sink<Message> + Unpin>(
    sink: &mut S,
    message: &Server,
) -> bool {
    let Ok(text) = serde_json::to_string(message) else {
        return false;
    };
    matches!(
        timeout(SEND_TIMEOUT, sink.send(Message::Text(text.into()))).await,
        Ok(Ok(()))
    )
}
