use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use clap::Parser;
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Frame, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use ra::control;
use ratio_engine::{
    RunReader,
    api::Api,
    audit::AuditFilter,
    budget::Budget,
    config::{Cli, Config},
    events::Reporter,
    session::Store,
};
use ratio_protocol::{ControlAction, ControlReply, ControlRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Semaphore},
};

mod legacy;

type Body = UnsyncBoxBody<Bytes, std::convert::Infallible>;
type Reply = Response<Body>;
type Query = BTreeMap<String, String>;

const HTML: &str = include_str!("web/index.html");
const CSS: &str = include_str!("web/dashboard.css");
const JS: &str = include_str!("web/dashboard.js");
const ICON: &str = include_str!("web/favicon.svg");
const FONTS: [(&str, &[u8]); 4] = [
    (
        "dm-mono-regular.ttf",
        include_bytes!("web/fonts/dm-mono-regular.ttf"),
    ),
    (
        "dm-mono-medium.ttf",
        include_bytes!("web/fonts/dm-mono-medium.ttf"),
    ),
    (
        "martian-mono-regular.ttf",
        include_bytes!("web/fonts/martian-mono-regular.ttf"),
    ),
    (
        "martian-mono-medium.ttf",
        include_bytes!("web/fonts/martian-mono-medium.ttf"),
    ),
];

struct Dashboard {
    workspace: PathBuf,
    launch_args: Vec<OsString>,
    host: String,
    authorization: String,
    actions: Mutex<VecDeque<(String, Value)>>,
    runner_pid: AtomicU32,
}

pub async fn serve(workspace: PathBuf, port: u16) -> Result<u8> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let host = listener.local_addr()?.to_string();
    let mut token = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut token)
        .map_err(|_| anyhow::anyhow!("cannot generate dashboard launch token"))?;
    let token = token
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("http://{host}/#access={token}");
    let app = Arc::new(Dashboard {
        workspace,
        launch_args: runner_args(std::env::args_os().skip(1)),
        host,
        authorization: format!("Bearer {token}"),
        actions: Mutex::new(VecDeque::new()),
        runner_pid: AtomicU32::new(0),
    });
    let capacity = Arc::new(Semaphore::new(48));
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => { signal?; return Ok(0); }
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                let Ok(permit) = capacity.clone().try_acquire_owned() else { continue };
                let app = app.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let service = service_fn(move |request| {
                        let app = app.clone();
                        async move { Ok::<_,std::convert::Infallible>(handle(app, request).await) }
                    });
                    let mut connection = http1::Builder::new();
                    connection.timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(8));
                    let _ = connection.serve_connection(TokioIo::new(socket), service).await;
                });
            }
        }
    }
}

fn runner_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args = args.into_iter();
    let mut launch = Vec::new();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--web") => {}
            Some("--web-port") => {
                args.next();
            }
            Some(value) if value.starts_with("--web-port=") => {}
            _ => launch.push(arg),
        }
    }
    launch
}

async fn handle(app: Arc<Dashboard>, request: Request<Incoming>) -> Reply {
    if request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        != Some(&app.host)
    {
        return error(StatusCode::FORBIDDEN, "Invalid dashboard host");
    }
    let path = request.uri().path().to_owned();
    if !path.starts_with("/api/") {
        if request.method() != Method::GET {
            return error(StatusCode::METHOD_NOT_ALLOWED, "Read-only asset");
        }
        return match path.as_str() {
            "/dashboard.css" => response(StatusCode::OK, "text/css; charset=utf-8", CSS),
            "/dashboard.js" => response(StatusCode::OK, "text/javascript; charset=utf-8", JS),
            "/favicon.svg" => response(StatusCode::OK, "image/svg+xml", ICON),
            "/" => response(StatusCode::OK, "text/html; charset=utf-8", HTML),
            path if path.starts_with("/fonts/") => FONTS
                .iter()
                .find(|(name, _)| path.strip_prefix("/fonts/") == Some(*name))
                .map_or_else(
                    || error(StatusCode::NOT_FOUND, "Page not found"),
                    |(_, font)| response(StatusCode::OK, "font/ttf", *font),
                ),
            path if path.starts_with("/run/") => {
                response(StatusCode::OK, "text/html; charset=utf-8", HTML)
            }
            _ => error(StatusCode::NOT_FOUND, "Page not found"),
        };
    }
    if request
        .headers()
        .get(header::AUTHORIZATION)
        .map(|value| value.as_bytes())
        != Some(app.authorization.as_bytes())
    {
        return error(
            StatusCode::UNAUTHORIZED,
            "Open the private URL printed by ra --web",
        );
    }
    if request.method() == Method::POST
        && request
            .headers()
            .get(header::ORIGIN)
            .and_then(|origin| origin.to_str().ok())
            != Some(format!("http://{}", app.host).as_str())
    {
        return error(StatusCode::FORBIDDEN, "Invalid dashboard origin");
    }
    let query = url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect::<Query>();
    if request.method() == Method::POST {
        if path != "/api/action" {
            return error(StatusCode::NOT_FOUND, "Endpoint not found");
        }
        let body = tokio::time::timeout(
            Duration::from_secs(5),
            Limited::new(request.into_body(), 72 * 1024).collect(),
        )
        .await;
        let Ok(Ok(body)) = body else {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body is too large or incomplete",
            );
        };
        let Ok(action) = serde_json::from_slice::<Action>(&body.to_bytes()) else {
            return error(StatusCode::BAD_REQUEST, "Invalid action");
        };
        return match app.action(action).await {
            Ok(value) => json_reply(value),
            Err(error_value) => error(StatusCode::CONFLICT, &format!("{error_value:#}")),
        };
    }
    if request.method() != Method::GET {
        return error(StatusCode::METHOD_NOT_ALLOWED, "Unsupported method");
    }
    if path == "/api/runs" {
        return read_json(move || {
            let journal = app.workspace.join(".ratio/runs.sqlite3");
            if !journal.exists() {
                return Ok(json!({"runs":[] }));
            }
            let reader = Store::reader(&app.workspace)?;
            Ok(json!({"runs":reader.runs()? }))
        })
        .await;
    }
    let route = path
        .strip_prefix("/api/run/")
        .and_then(|tail| tail.split_once('/'));
    let Some((run, view)) = route.filter(|(run, _)| valid_run(run)) else {
        return error(StatusCode::NOT_FOUND, "Unknown run or endpoint");
    };
    if view == "stream" {
        let checkpoint = query
            .get("checkpoint")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let cursor = query
            .get("after")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        return stream(app, run.into(), checkpoint, cursor);
    }
    if view == "live" {
        let status = control::request_control(
            &app.workspace,
            &ControlRequest {
                action: ControlAction::Status,
                guidance: None,
                run_id: Some(run.into()),
            },
        )
        .await
        .ok()
        .filter(|reply| reply.accepted && reply.run_id == run);
        return json_reply(json!(status));
    }
    let (run, view) = (run.to_owned(), view.to_owned());
    read_json(move || select(&Store::reader(&app.workspace)?, &run, &view, &query)).await
}

fn select(reader: &RunReader, run: &str, view: &str, query: &Query) -> Result<Value> {
    let param = |key| {
        query
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    match view {
        "overview" => {
            let snapshot = reader.snapshot(run, None)?.context("Run not found")?;
            let session: Value = serde_json::from_slice(&snapshot)?;
            let resume_phase = if session["phase"] == "Exhausted" {
                reader
                    .budget_resume_checkpoint(run)?
                    .map(|(_, phase)| phase)
            } else {
                None
            };
            let latest_error = if session["stopped"] == "failed" {
                reader
                    .audit(
                        run,
                        AuditFilter {
                            kind: Some("error"),
                            ..AuditFilter::default()
                        },
                        1,
                    )?
                    .pop()
            } else {
                None
            };
            Ok(json!({
                "run":reader.run(run)?, "event":reader.latest_event(run)?, "head":reader.audit_head(run)?,
                "goal":session["goal"], "request":session["request"], "verdict":session["verdict"],
                "parent_run":session["parent_run_id"],
                "resume_phase":resume_phase,
                "latest_error":latest_error,
                "assessment":session["assessment"], "acceptance":session["team_acceptance"],
                "workers":session["workers"], "reviewers":session["reviewers"], "stakeholders":session["stakeholders"],
                "plan":session["worker_plan"], "metrics":reader.metrics(run)?,
                "onboarding": {"plan":session["onboarding"]["plan"], "mentor":session["onboarding"]["mentor"],"stakeholder":session["onboarding"]["stakeholder"],"briefed":session["onboarding"]["briefed"]}
            }))
        }
        "history" => {
            if param("legacy") == Some("1") || reader.audit_head(run)? == 0 {
                return legacy::history(reader, run, query);
            }
            let rows = reader.audit(
                run,
                AuditFilter {
                    before: param("before").and_then(|value| value.parse().ok()),
                    after: param("after").and_then(|value| value.parse().ok()),
                    actor: param("actor"),
                    kind: param("kind"),
                    cycle: param("cycle").and_then(|value| value.parse().ok()),
                    phase: param("phase"),
                    query: param("q"),
                    from: param("from"),
                    to: param("to"),
                    worker_request: param("worker_request").and_then(|value| value.parse().ok()),
                    path: query
                        .get("path")
                        .map(|path| if path == "*" { "" } else { path.as_str() }),
                },
                60,
            )?;
            Ok(json!({"rows":rows}))
        }
        "checkpoints" => Ok(
            json!({"rows":reader.events(run,param("after").and_then(|value| value.parse().ok()).unwrap_or(0), 80)?}),
        ),
        "inbox" => Ok(
            json!({"rows":reader.inbox(run,param("before").and_then(|value| value.parse().ok()))?}),
        ),
        "admissions" => Ok(json!({"rows":reader.worker_admissions(run)?})),
        "routes" => Ok(
            json!({"rows":reader.ipc_routes(run,param("cycle").and_then(|value| value.parse().ok()),param("phase"),param("worker_request").and_then(|value| value.parse().ok()))?}),
        ),
        "checkpoint" => {
            let sequence = param("sequence").and_then(|value| value.parse().ok());
            let snapshot = reader
                .snapshot(run, sequence)?
                .context("Checkpoint not found")?;
            Ok(legacy_projection(serde_json::from_slice(&snapshot)?))
        }
        view if view.starts_with("entry/") => {
            let key = view.trim_start_matches("entry/");
            ensure!(
                key.len() <= 128
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                "Invalid event key"
            );
            if key.starts_with("legacy-") {
                return legacy::entry(reader, run, key);
            }
            let event = reader.audit_event(run, key)?.context("Entry not found")?;
            let outcome = (event.kind == "tool")
                .then(|| reader.audit_outcome(run, key))
                .transpose()?
                .flatten();
            let turn = event.data["turn"]
                .as_str()
                .map(|key| reader.audit_event(run, key))
                .transpose()?
                .flatten();
            let origin = (event.kind == "ipc")
                .then(|| reader.ipc_origin(&event, run))
                .transpose()?
                .flatten();
            let transitions = if event.kind == "ipc" {
                reader.ipc_transitions(&event, run)?
            } else {
                Vec::new()
            };
            Ok(
                json!({"event":event,"outcome":outcome,"turn":turn,"origin":origin,"deliveries":transitions}),
            )
        }
        _ => anyhow::bail!("Endpoint not found"),
    }
}

fn legacy_projection(snapshot: Value) -> Value {
    let mut dialogs = Vec::new();
    let add = |rows: &mut Vec<Value>, actor: &str, dialogue: &Value, assessment: &Value| {
        let mut groups = dialogue["groups"].clone();
        if let Some(groups) = groups.as_array_mut() {
            for group in groups.iter_mut().filter_map(Value::as_array_mut) {
                for message in group.iter_mut().filter_map(Value::as_object_mut) {
                    message.remove("reasoning_content");
                    message.remove("responses_items");
                }
            }
        }
        rows.push(json!({"actor":actor, "task":dialogue["task"],"guidance":dialogue["guidance"],"summary":dialogue["summary"],"groups":groups,"assessment":assessment}));
    };
    add(
        &mut dialogs,
        if snapshot["phase"] == "Work" {
            "worker_1"
        } else {
            "lead_reviewer"
        },
        &snapshot["dialogue"],
        &snapshot["assessment"],
    );
    if let Some(workers) = snapshot["worker_progress"].as_array() {
        for (index, worker) in workers.iter().enumerate() {
            add(
                &mut dialogs,
                &format!("worker_{}", index + 1),
                &worker["dialogue"],
                &worker["assessment"],
            );
        }
    }
    if let Some(agents) = snapshot["panel"]["agents"].as_array() {
        for (index, agent) in agents.iter().enumerate() {
            add(
                &mut dialogs,
                snapshot["panel"]["actor_ids"][index]
                    .as_str()
                    .unwrap_or("unknown panel agent"),
                &agent["dialogue"],
                &agent["assessment"],
            );
        }
    }
    json!({"phase":snapshot["phase"],"cycle":snapshot["cycle"],"dialogs":dialogs})
}

fn stream(app: Arc<Dashboard>, run: String, checkpoint: u64, cursor: u64) -> Reply {
    let frames = futures_util::stream::unfold(
        (app, run, checkpoint, cursor, true),
        |(app, run, previous, cursor, first)| async move {
            if !first {
                tokio::time::sleep(Duration::from_millis(900)).await;
            }
            let workspace = app.workspace.clone();
            let run_id = run.clone();
            let (head, updates) = tokio::task::spawn_blocking(move || {
                let reader = Store::reader(&workspace).ok()?;
                Some((
                    reader.run(&run_id).ok()??.last_sequence,
                    reader.audit_updates(&run_id, cursor).ok()?,
                ))
            })
            .await
            .ok()
            .flatten()
            .unwrap_or((previous, Vec::new()));
            let next = updates
                .last()
                .and_then(|item| item["sequence"].as_u64())
                .unwrap_or(cursor);
            let output = if (head, next) == (previous, cursor) {
                ": heartbeat\n\n".into()
            } else {
                format!(
                    "id: {}\ndata: {}\n\n",
                    next,
                    json!({"checkpoint":head,"cursor":next,"events":updates})
                )
            };
            Some((
                Ok(Frame::data(Bytes::from(output))),
                (app, run, head, next, false),
            ))
        },
    );
    let mut reply = base(
        StatusCode::OK,
        "text/event-stream",
        StreamBody::new(frames).boxed_unsync(),
    );
    reply
        .headers_mut()
        .insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    reply
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    id: String,
    action: String,
    run: Option<String>,
    text: Option<String>,
}

impl Dashboard {
    async fn action(&self, action: Action) -> Result<Value> {
        ensure!(
            (1..=80).contains(&action.id.len())
                && action
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "Invalid action ID"
        );
        let mut actions = self.actions.lock().await;
        if let Some((_, receipt)) = actions.iter().find(|(id, _)| id == &action.id) {
            return Ok(receipt.clone());
        }
        let run = action.run.as_deref().filter(|run| valid_run(run));
        let note = action
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let receipt = match action.action.as_str() {
            "start" | "resume" | "resume_saved" | "fork" => {
                let resume = matches!(action.action.as_str(), "resume" | "resume_saved");
                let active = control::request_control(
                    &self.workspace,
                    &ControlRequest {
                        action: ControlAction::Status,
                        guidance: None,
                        run_id: None,
                    },
                )
                .await
                .ok()
                .filter(|reply| reply.accepted);
                if let Some(active) = &active {
                    ensure!(
                        resume && Some(active.run_id.as_str()) == run,
                        "This workspace already has an active run"
                    );
                }
                let mut args = self.launch_args.clone();
                args.push("--dashboard-managed".into());
                if action.action != "start" {
                    let run = run.context("Choose a saved run")?;
                    let reader = Store::reader(&self.workspace)?;
                    let record = reader.run(run)?.context("Run not found")?;
                    if action.action == "fork" {
                        args.extend(["--fork".into(), run.into()]);
                    } else {
                        ensure!(
                            !matches!(record.phase.as_str(), "Approved" | "Rejected")
                                && (record.phase != "Exhausted"
                                    || reader.budget_resume_checkpoint(run)?.is_some()),
                            "This run has already finished"
                        );
                        args.extend(["--resume".into(), "--run-id".into(), run.into()]);
                    }
                } else {
                    ensure!(note.is_some(), "Enter a request to start a run");
                }
                if let Some(note) = note {
                    ensure!(note.len() <= 65_536 && !resume, "Invalid run request");
                    args.push("--goal-stdin".into());
                }
                let reload = active.as_ref().is_some_and(|runner| {
                    runner.paused
                        && runner.dashboard_managed
                        && self.runner_pid.load(Ordering::Relaxed) != runner.pid
                });
                if active.is_some() && !reload {
                    let run = run.context("Choose a run")?;
                    serde_json::to_value(self.control(run, ControlAction::Resume, None).await?)?
                } else {
                    self.launch(args, note, run.filter(|_| resume), reload)
                        .await?
                }
            }
            command => {
                let command = match command {
                    "pause" => ControlAction::Pause,
                    "stop" => ControlAction::Cancel,
                    "guide" => ControlAction::Guide,
                    "triage" => ControlAction::Triage,
                    "retry_triage" => ControlAction::RetryTriage,
                    "add_worker" => ControlAction::AddWorker,
                    _ => anyhow::bail!("Unknown action"),
                };
                let run = run.context("Choose a run")?;
                serde_json::to_value(self.control(run, command, note).await?)?
            }
        };
        actions.push_back((action.id, receipt.clone()));
        if actions.len() > 256 {
            actions.pop_front();
        }
        Ok(receipt)
    }

    async fn control(
        &self,
        run: &str,
        action: ControlAction,
        text: Option<&str>,
    ) -> Result<ControlReply> {
        let reply = control::request_control(
            &self.workspace,
            &ControlRequest {
                action,
                guidance: text.map(str::to_owned),
                run_id: Some(run.into()),
            },
        )
        .await?;
        ensure!(reply.accepted && reply.run_id == run, "{}", reply.detail);
        Ok(reply)
    }

    async fn launch(
        &self,
        args: Vec<OsString>,
        note: Option<&str>,
        resume: Option<&str>,
        reload: bool,
    ) -> Result<Value> {
        if let Some(run) = resume {
            let cli = Cli::try_parse_from(
                std::iter::once(OsString::from("ra"))
                    .chain(args.iter().cloned())
                    .chain(std::iter::once(OsString::from("--managed-run"))),
            )?;
            let config = Config::load(cli)?;
            ensure!(
                config.cwd == self.workspace,
                "The updated profile selects a different workspace. Reopen the dashboard for this workspace."
            );
            if reload {
                Api::new(&config, Reporter::Silent, Budget::new(&config))?;
                self.control(run, ControlAction::CheckpointAndExit, None)
                    .await?;
                tokio::time::timeout(
                    Duration::from_secs(
                        config.orchestration.command_timeout_secs.saturating_add(10),
                    ),
                    async {
                        while Store::has_live_writer(&self.workspace)? {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Ok::<_, anyhow::Error>(())
                    },
                )
                .await
                .context("The paused runner has not reached its restart checkpoint")??;
            }
        }
        let (reply, log) = control::launch_with_args(&self.workspace, args, note).await?;
        self.runner_pid.store(
            reply.as_ref().map_or(0, |reply| reply.pid),
            Ordering::Relaxed,
        );
        let run_id = reply.as_ref().map(|reply| reply.run_id.as_str()).or(resume);
        let detail = if resume.is_some() {
            "Run resumed with current configuration"
        } else {
            "Runner launched"
        };
        Ok(json!({"accepted":true, "run_id":run_id, "detail":detail, "log":log.file_name()}))
    }
}

fn valid_run(run: &str) -> bool {
    !run.is_empty()
        && run.len() <= 80
        && run
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
}

async fn read_json(read: impl FnOnce() -> Result<Value> + Send + 'static) -> Reply {
    match tokio::task::spawn_blocking(read).await {
        Ok(Ok(value)) => json_reply(value),
        Ok(Err(cause)) => error(StatusCode::NOT_FOUND, &cause.to_string()),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Dashboard reader unavailable",
        ),
    }
}

fn json_reply(value: Value) -> Reply {
    response(
        StatusCode::OK,
        "application/json; charset=utf-8",
        value.to_string(),
    )
}
fn error(status: StatusCode, detail: &str) -> Reply {
    response(
        status,
        "application/json; charset=utf-8",
        json!({"error":detail}).to_string(),
    )
}
fn response(status: StatusCode, mime: &str, content: impl Into<Bytes>) -> Reply {
    base(status, mime, Full::new(content.into()).boxed_unsync())
}
fn base(status: StatusCode, mime: &str, body: Body) -> Reply {
    Response::builder().status(status).header(header::CONTENT_TYPE,mime)
        .header(header::CACHE_CONTROL,"no-store").header("x-content-type-options","nosniff")
        .header("referrer-policy","no-referrer")
        .header("content-security-policy","default-src 'self'; connect-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'")
        .body(body).expect("static response headers")
}
