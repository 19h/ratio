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
use ring::digest::{SHA256, digest};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Semaphore, watch},
};

mod dictionary;
mod legacy;
mod realtime;
mod training;

pub use training::train_dictionaries;

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
    assets: Assets,
    launch_args: Vec<OsString>,
    host: String,
    authorization: String,
    share: Option<ShareSite>,
    actions: Mutex<ActionBook>,
    lifecycle: Arc<Semaphore>,
    controls: Arc<Semaphore>,
    sharing: Arc<Semaphore>,
    provider: Arc<Semaphore>,
    http_reads: Arc<Semaphore>,
    guest_reads: Arc<Semaphore>,
    runner_pid: AtomicU32,
    realtime: realtime::Hub,
}

type ActionResult = std::result::Result<Value, String>;

#[derive(Default)]
struct ActionBook {
    pending: BTreeMap<String, watch::Receiver<Option<ActionResult>>>,
    completed: VecDeque<(String, ActionResult)>,
}

struct ShareSite {
    loopback: String,
    host: String,
    origin: String,
}

impl ShareSite {
    fn new(loopback: String, public_origin: Option<&str>) -> Result<Self> {
        let Some(origin) = public_origin else {
            return Ok(Self {
                origin: format!("http://{loopback}"),
                host: loopback.clone(),
                loopback,
            });
        };
        let url = url::Url::parse(origin).context("Invalid --web-share-origin")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "--web-share-origin must be an HTTPS origin without a path, credentials, query, or fragment"
        );
        Ok(Self {
            loopback,
            host: url[url::Position::BeforeHost..url::Position::AfterPort].to_string(),
            origin: url.origin().ascii_serialization(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    Operator,
    Share,
}

fn new_token() -> Result<String> {
    let mut token = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut token)
        .map_err(|_| anyhow::anyhow!("cannot generate dashboard token"))?;
    Ok(token.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub async fn serve(
    workspace: PathBuf,
    port: u16,
    share_port: Option<u16>,
    share_origin: Option<&str>,
) -> Result<u8> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let host = listener.local_addr()?.to_string();
    let (share_listener, share) = if let Some(port) = share_port {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .context("bind read-only dashboard listener")?;
        let site = ShareSite::new(listener.local_addr()?.to_string(), share_origin)?;
        eprintln!("Read-only dashboard listening on {}", site.loopback);
        (Some(listener), Some(site))
    } else {
        (None, None)
    };
    let (assets, dictionaries) =
        tokio::task::spawn_blocking(|| (Assets::new(), dictionary::Dictionaries::load())).await?;
    let token = new_token()?;
    println!("http://{host}/#access={token}");
    let app = Arc::new(Dashboard {
        workspace,
        assets,
        launch_args: runner_args(std::env::args_os().skip(1)),
        host,
        authorization: format!("Bearer {token}"),
        share,
        actions: Mutex::new(ActionBook::default()),
        lifecycle: Arc::new(Semaphore::new(1)),
        controls: Arc::new(Semaphore::new(1)),
        sharing: Arc::new(Semaphore::new(1)),
        provider: Arc::new(Semaphore::new(1)),
        http_reads: Arc::new(Semaphore::new(2)),
        guest_reads: Arc::new(Semaphore::new(2)),
        runner_pid: AtomicU32::new(0),
        realtime: realtime::Hub::new(dictionaries)?,
    });
    realtime::observe(app.clone());
    let public = async {
        match share_listener {
            Some(listener) => listen(listener, app.clone(), Surface::Share).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        signal = tokio::signal::ctrl_c() => signal?,
        result = listen(listener, app.clone(), Surface::Operator) => result?,
        result = public => result?,
    }
    Ok(0)
}

async fn listen(listener: TcpListener, app: Arc<Dashboard>, surface: Surface) -> Result<()> {
    let capacity = Arc::new(Semaphore::new(48));
    loop {
        let (socket, _) = listener.accept().await?;
        let Ok(permit) = capacity.clone().try_acquire_owned() else {
            continue;
        };
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request| {
                let app = app.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(app, surface, request).await) }
            });
            let mut connection = http1::Builder::new();
            connection
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(8));
            let _ = connection
                .serve_connection(TokioIo::new(socket), service)
                .with_upgrades()
                .await;
        });
    }
}

fn runner_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args = args.into_iter();
    let mut launch = Vec::new();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--web") => {}
            Some("--web-port" | "--web-share-port" | "--web-share-origin") => {
                args.next();
            }
            Some(value)
                if ["--web-port=", "--web-share-port=", "--web-share-origin="]
                    .iter()
                    .any(|prefix| value.starts_with(prefix)) => {}
            _ => launch.push(arg),
        }
    }
    launch
}

async fn handle(app: Arc<Dashboard>, surface: Surface, request: Request<Incoming>) -> Reply {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let valid_host = match surface {
        Surface::Operator => host == Some(&app.host),
        Surface::Share => app.share.as_ref().is_some_and(|site| {
            host.is_some_and(|host| host == site.loopback || host.eq_ignore_ascii_case(&site.host))
        }),
    };
    if !valid_host {
        return error(StatusCode::FORBIDDEN, "Invalid dashboard host");
    }
    let path = request.uri().path().to_owned();
    if path == "/api/dashboard/socket" {
        return realtime::upgrade(app, surface, request);
    }
    if !path.starts_with("/api/") {
        if request.method() != Method::GET {
            return error(StatusCode::METHOD_NOT_ALLOWED, "Read-only asset");
        }
        return app
            .assets
            .serve(&request, &path, surface, app.share.is_some());
    }
    if surface == Surface::Share {
        return read_shared(app, request, &path).await;
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
        return read_json(app.http_reads.clone(), move || {
            let journal = app.workspace.join(".ratio/runs.sqlite3");
            if !journal.exists() {
                return Ok(json!({"runs":[] }));
            }
            let reader = Store::reader(&app.workspace)?;
            Ok(json!({"runs":reader.runs()? }))
        })
        .await;
    }
    if path == "/api/config" {
        return read_json(app.http_reads.clone(), move || Ok(app.config_summary())).await;
    }
    let Some((run, view)) = run_route(&path) else {
        return error(StatusCode::NOT_FOUND, "Unknown run or endpoint");
    };
    if view == "share" {
        let Some(site) = &app.share else {
            return error(StatusCode::NOT_FOUND, "Sharing listener is disabled");
        };
        let origin = site.origin.clone();
        let run = run.to_owned();
        return read_json(app.http_reads.clone(), move || {
            let active = Store::reader(&app.workspace)?
                .web_share_digest(&run)?
                .is_some();
            Ok(json!({"active":active,"origin":origin}))
        })
        .await;
    }
    read_run(app, run, view, parse_query(&request), None).await
}

struct Asset {
    mime: &'static str,
    body: Bytes,
    gzip: Option<Bytes>,
    etag: header::HeaderValue,
}

impl Asset {
    fn new(mime: &'static str, body: impl Into<Bytes>) -> Self {
        let body = body.into();
        let gzip = (body.len() >= 1024)
            .then(|| realtime::gzip(&body).ok())
            .flatten()
            .filter(|compressed| compressed.len() * 10 <= body.len() * 9)
            .map(Bytes::from);
        let hash = digest(&SHA256, &body);
        let hash: String = hash
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let etag = format!("W/\"{hash}\"").parse().expect("static asset tag");
        Self {
            mime,
            body,
            gzip,
            etag,
        }
    }

    fn serve(&self, request: &Request<Incoming>, asset: bool) -> Reply {
        let cached = asset
            && request
                .headers()
                .get(header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|tags| {
                    tags.split(',')
                        .any(|tag| tag.trim() == "*" || tag.trim() == self.etag)
                });
        let compressed = accepts_gzip(request.headers())
            .then_some(self.gzip.as_ref())
            .flatten();
        let mut reply = if cached {
            response(StatusCode::NOT_MODIFIED, self.mime, Bytes::new())
        } else {
            response(
                StatusCode::OK,
                self.mime,
                compressed.unwrap_or(&self.body).clone(),
            )
        };
        let headers = reply.headers_mut();
        headers.insert(
            header::VARY,
            header::HeaderValue::from_static("Accept-Encoding"),
        );
        if asset {
            headers.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-cache"),
            );
            headers.insert(header::ETAG, self.etag.clone());
        }
        if compressed.is_some() {
            headers.insert(
                header::CONTENT_ENCODING,
                header::HeaderValue::from_static("gzip"),
            );
        }
        reply
    }
}

fn accepts_gzip(headers: &header::HeaderMap) -> bool {
    let mut gzip = None;
    let mut wildcard = None;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for item in value.split(',') {
            let mut fields = item.split(';');
            let coding = fields.next().unwrap_or_default().trim();
            let enabled = fields
                .find_map(|part| {
                    let (key, value) = part.trim().split_once('=')?;
                    key.trim().eq_ignore_ascii_case("q").then(|| {
                        value
                            .trim()
                            .parse::<f32>()
                            .is_ok_and(|q| q > 0.0 && q <= 1.0)
                    })
                })
                .unwrap_or(true);
            if coding.eq_ignore_ascii_case("gzip") {
                gzip = Some(enabled);
            } else if coding == "*" {
                wildcard = Some(enabled);
            }
        }
    }
    gzip.or(wildcard).unwrap_or(false)
}

struct Assets {
    operator: Asset,
    sharing_operator: Asset,
    shared: Asset,
    static_files: BTreeMap<String, Asset>,
}

impl Assets {
    fn new() -> Self {
        let mut static_files = BTreeMap::from([
            (
                "/dashboard.css".into(),
                Asset::new("text/css; charset=utf-8", CSS),
            ),
            (
                "/dashboard.js".into(),
                Asset::new("text/javascript; charset=utf-8", JS),
            ),
            (
                "/dashboard-decoder.js".into(),
                Asset::new(
                    "text/javascript; charset=utf-8",
                    include_str!("web/dashboard-decoder.js"),
                ),
            ),
            (
                "/dashboard-decoder.wasm".into(),
                Asset::new(
                    "application/wasm",
                    include_bytes!("web/dashboard-decoder.wasm").as_slice(),
                ),
            ),
            ("/favicon.svg".into(), Asset::new("image/svg+xml", ICON)),
        ]);
        for (name, font) in FONTS {
            static_files.insert(format!("/fonts/{name}"), Asset::new("font/ttf", font));
        }
        let mime = "text/html; charset=utf-8";
        Self {
            operator: Asset::new(mime, HTML),
            sharing_operator: Asset::new(mime, HTML.replacen("<body>", "<body data-sharing>", 1)),
            shared: Asset::new(mime, HTML.replacen("<body>", "<body data-read-only>", 1)),
            static_files,
        }
    }

    fn serve(
        &self,
        request: &Request<Incoming>,
        path: &str,
        surface: Surface,
        sharing: bool,
    ) -> Reply {
        if let Some(asset) = self.static_files.get(path) {
            let mut reply = asset.serve(request, true);
            if path == "/dashboard-decoder.js" {
                reply.headers_mut().insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static(
                    "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; object-src 'none'; base-uri 'none'"
                ));
            }
            reply
        } else if path == "/" || path.starts_with("/run/") {
            let page = match surface {
                Surface::Share => &self.shared,
                Surface::Operator if sharing => &self.sharing_operator,
                Surface::Operator => &self.operator,
            };
            page.serve(request, false)
        } else {
            error(StatusCode::NOT_FOUND, "Page not found")
        }
    }
}

fn run_route(path: &str) -> Option<(&str, &str)> {
    path.strip_prefix("/api/run/")?
        .split_once('/')
        .filter(|(run, _)| valid_run(run))
}

fn parse_query(request: &Request<Incoming>) -> Query {
    url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

fn permits_share(reader: &RunReader, run: &str, token_digest: &[u8]) -> bool {
    reader.web_share_digest(run).ok().flatten().as_deref() == Some(token_digest)
}

async fn read_shared(app: Arc<Dashboard>, request: Request<Incoming>, path: &str) -> Reply {
    if request.method() != Method::GET {
        return error(StatusCode::FORBIDDEN, "Shared runs are read-only");
    }
    let Some((run, view)) = run_route(path) else {
        return error(StatusCode::FORBIDDEN, "Share links can only open their run");
    };
    let query = parse_query(&request);
    if !(matches!(
        view,
        "overview" | "history" | "routes" | "stream" | "live" | "inbox" | "admissions"
    ) || view
        .strip_prefix("entry/")
        .is_some_and(|key| !key.starts_with("legacy-")))
        || query.contains_key("legacy")
    {
        return error(
            StatusCode::FORBIDDEN,
            "Endpoint is unavailable for shared runs",
        );
    }
    let unauthorized = || {
        error(
            StatusCode::UNAUTHORIZED,
            "Invalid or revoked run share link",
        )
    };
    let Some(token) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
    else {
        return unauthorized();
    };
    let token_digest = digest(&SHA256, token.as_bytes()).as_ref().to_vec();
    let allowed = realtime::authorized_share(&app, run, &token_digest).await;
    if !allowed {
        return unauthorized();
    }
    read_run(app, run, view, query, Some(token_digest)).await
}

async fn read_run(
    app: Arc<Dashboard>,
    run: &str,
    view: &str,
    query: Query,
    share_digest: Option<Vec<u8>>,
) -> Reply {
    if view == "stream" {
        let checkpoint = query
            .get("checkpoint")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let cursor = query
            .get("after")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        return stream(app, run.into(), checkpoint, cursor, share_digest);
    }
    if view == "live" {
        let pool = if share_digest.is_some() {
            &app.guest_reads
        } else {
            &app.http_reads
        };
        let Ok(_permit) = pool.clone().try_acquire_owned() else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Dashboard read capacity reached",
            );
        };
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
        return if share_digest.is_some() {
            json_reply(json!(status.map(|status| json!({
                "paused":status.paused, "generations":status.generations, "token_usage":status.token_usage
            }))))
        } else {
            json_reply(json!(status))
        };
    }
    let (run, view) = (run.to_owned(), view.to_owned());
    let pool = if share_digest.is_some() {
        &app.guest_reads
    } else {
        &app.http_reads
    };
    read_json(pool.clone(), move || {
        let reader = Store::reader(&app.workspace)?;
        if share_digest.is_some() && view == "history" && reader.audit_head(&run)? == 0 {
            return Ok(json!({"rows":[]}));
        }
        let mut value = select(&reader, &run, &view, &query)?;
        if share_digest.is_some() && view == "overview" {
            value["parent_run"] = Value::Null;
        }
        Ok(value)
    })
    .await
}

fn overview(
    reader: &RunReader,
    run: &str,
    metrics: Option<&ratio_protocol::RunMetrics>,
) -> Result<Value> {
    let session = match reader.dashboard_overview(run)? {
        Some(overview) => overview,
        None => serde_json::from_slice(&reader.snapshot(run, None)?.context("Run not found")?)?,
    };
    let event = if metrics.is_some() {
        reader.dashboard_latest_event(run)?
    } else {
        reader.latest_event(run)?
    };
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
        "run":reader.run(run)?, "event":event, "head":reader.audit_head(run)?,
        "goal":session["goal"], "request":session["request"], "verdict":session["verdict"],
        "parent_run":session["parent_run_id"],
        "resume_phase":resume_phase,
        "latest_error":latest_error,
        "assessment":session["assessment"], "acceptance":session["team_acceptance"],
        "workers":session["workers"], "reviewers":session["reviewers"], "stakeholders":session["stakeholders"],
        "plan":session["worker_plan"], "metrics":match metrics { Some(metrics) => json!(metrics), None => json!(reader.metrics(run)?) },
        "onboarding": {"plan":session["onboarding"]["plan"], "mentor":session["onboarding"]["mentor"],"stakeholder":session["onboarding"]["stakeholder"],"briefed":session["onboarding"]["briefed"]}
    }))
}

fn select(reader: &RunReader, run: &str, view: &str, query: &Query) -> Result<Value> {
    let param = |key| {
        query
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    match view {
        "overview" => overview(reader, run, None),
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
                    kind: param("kind")
                        .or_else(|| (param("collapse_tools") == Some("1")).then_some("timeline")),
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
        "routes" => {
            let rows = reader.ipc_routes_filtered(
                run,
                AuditFilter {
                    cycle: param("cycle").and_then(|value| value.parse().ok()),
                    phase: param("phase"),
                    worker_request: param("worker_request").and_then(|value| value.parse().ok()),
                    actor: param("actor"),
                    query: param("q"),
                    from: param("from"),
                    to: param("to"),
                    ..AuditFilter::default()
                },
            )?;
            Ok(json!({"rows":rows}))
        }
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
            let tool_key = match event.kind.as_str() {
                "tool" => Some(key),
                "tool_result" => event.data["call"].as_str(),
                _ => None,
            };
            let tool_stages = tool_key
                .map(|key| reader.tool_stages(run, key))
                .transpose()?
                .unwrap_or_default();
            Ok(
                json!({"event":event,"outcome":outcome,"turn":turn,"origin":origin,"deliveries":transitions,"tool_stages":tool_stages}),
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

fn stream(
    app: Arc<Dashboard>,
    run: String,
    checkpoint: u64,
    cursor: u64,
    share_digest: Option<Vec<u8>>,
) -> Reply {
    let frames = futures_util::stream::unfold(
        (app, run, checkpoint, cursor, true, share_digest),
        |(app, run, previous, cursor, first, share_digest)| async move {
            if !first {
                tokio::time::sleep(Duration::from_millis(900)).await;
            }
            let workspace = app.workspace.clone();
            let run_id = run.clone();
            let check = share_digest.clone();
            let pool = if share_digest.is_some() {
                &app.guest_reads
            } else {
                &app.http_reads
            };
            let permit = pool.clone().acquire_owned().await.ok()?;
            let update = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let reader = Store::reader(&workspace).ok()?;
                reader.dashboard_deadline(Duration::from_millis(250)).ok()?;
                if check.is_some_and(|digest| !permits_share(&reader, &run_id, &digest)) {
                    return None;
                }
                Some((
                    reader.run(&run_id).ok()??.last_sequence,
                    reader.audit_updates(&run_id, cursor).ok()?,
                ))
            })
            .await
            .ok()
            .flatten();
            if share_digest.is_some() && update.is_none() {
                return None;
            }
            let (head, updates) = update.unwrap_or((previous, Vec::new()));
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
                (app, run, head, next, false, share_digest),
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
    /// The configuration a run started now would load, validated the same way.
    fn effective_config(&self) -> Result<Config> {
        let cli = Cli::try_parse_from(
            std::iter::once(OsString::from("ra")).chain(self.launch_args.iter().cloned()),
        )?;
        let config = Config::load(cli)?;
        ensure!(
            config.cwd == self.workspace,
            "The profile selects a different workspace. Reopen the dashboard for this workspace."
        );
        Api::new(&config, Reporter::Silent, Budget::new(&config))?;
        Ok(config)
    }

    fn config_summary(&self) -> Value {
        match self.effective_config() {
            Ok(config) => {
                let mut summary = config.summary();
                summary["ok"] = json!(true);
                summary
            }
            Err(error) => {
                json!({"ok":false,"error":format!("{error:#}"),"workspace":self.workspace})
            }
        }
    }

    /// One small request per configured model, run in a short-lived child
    /// process so a slow provider cannot stall the dashboard.
    async fn check_provider(self: &Arc<Self>) -> Result<Value> {
        let app = self.clone();
        tokio::task::spawn_blocking(move || app.effective_config().map(drop)).await??;
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .args(&self.launch_args)
            .arg("--check-provider")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(60), command.output())
            .await
            .context("The provider did not answer within 60 seconds")??;
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "Provider check failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })
    }

    async fn action(self: &Arc<Self>, action: Action) -> Result<Value> {
        ensure!(
            (1..=80).contains(&action.id.len())
                && action
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "Invalid action ID"
        );
        let mut result = {
            let mut book = self.actions.lock().await;
            if let Some((_, receipt)) = book.completed.iter().find(|(id, _)| id == &action.id) {
                return receipt.clone().map_err(anyhow::Error::msg);
            }
            if let Some(pending) = book.pending.get(&action.id) {
                pending.clone()
            } else {
                let lane = match action.action.as_str() {
                    "create_share" | "revoke_share" => &self.sharing,
                    "check_provider" => &self.provider,
                    "start" | "resume" | "resume_saved" | "fork" => &self.lifecycle,
                    _ => &self.controls,
                };
                let permit = lane
                    .clone()
                    .try_acquire_owned()
                    .context("A related dashboard action is already running")?;
                let (finish, result) = watch::channel(None);
                book.pending.insert(action.id.clone(), result.clone());
                let app = self.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let id = action.id.clone();
                    let receipt = app
                        .execute_action(action)
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let mut book = app.actions.lock().await;
                    book.pending.remove(&id);
                    if receipt.is_ok() {
                        book.completed.push_back((id, receipt.clone()));
                        if book.completed.len() > 256 {
                            book.completed.pop_front();
                        }
                    }
                    finish.send_replace(Some(receipt));
                });
                result
            }
        };
        result
            .wait_for(Option::is_some)
            .await?
            .clone()
            .expect("completed dashboard action")
            .map_err(anyhow::Error::msg)
    }

    async fn execute_action(self: &Arc<Self>, action: Action) -> Result<Value> {
        if action.action == "check_provider" {
            return self.check_provider().await;
        }
        let run = action.run.as_deref().filter(|run| valid_run(run));
        let note = action
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let receipt = match action.action.as_str() {
            "create_share" | "revoke_share" => {
                let site = self
                    .share
                    .as_ref()
                    .context("Sharing listener is disabled")?;
                let run = run.context("Choose a run")?;
                let run_id = run.to_owned();
                let workspace = self.workspace.clone();
                let create = action.action == "create_share";
                let token = tokio::task::spawn_blocking(move || {
                    ensure!(
                        Store::reader(&workspace)?.run(&run_id)?.is_some(),
                        "Run not found"
                    );
                    let token = create.then(new_token).transpose()?;
                    let hash = token
                        .as_ref()
                        .map(|token| digest(&SHA256, token.as_bytes()));
                    Store::set_web_share(
                        &workspace,
                        &run_id,
                        hash.as_ref().map(|hash| hash.as_ref()),
                    )?;
                    Ok::<_, anyhow::Error>(token)
                })
                .await??;
                self.realtime.invalidate();
                let url =
                    token.map(|token| format!("{}/run/{run}/overview#share={token}", site.origin));
                json!({"active": create, "url": url})
            }
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
                let app = self.clone();
                let action_name = action.action.clone();
                let checked_run = run.map(str::to_owned);
                let checked_note = note.map(str::to_owned);
                let args = tokio::task::spawn_blocking(move || {
                    let run = checked_run;
                    let note = checked_note;
                    let mut args = app.launch_args.clone();
                    args.push("--dashboard-managed".into());
                    if action_name != "start" {
                        let run = run.as_deref().context("Choose a saved run")?;
                        let reader = Store::reader(&app.workspace)?;
                        let record = reader.run(run)?.context("Run not found")?;
                        if action_name == "fork" {
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
                    if !resume {
                        // Report a broken profile here instead of in the runner log.
                        app.effective_config()?;
                    }
                    if let Some(note) = note.as_deref() {
                        ensure!(note.len() <= 65_536 && !resume, "Invalid run request");
                        args.push("--goal-stdin".into());
                    }
                    Ok::<_, anyhow::Error>(args)
                })
                .await??;
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
            let workspace = self.workspace.clone();
            let checked_args = args.clone();
            let command_timeout = tokio::task::spawn_blocking(move || {
            let cli = Cli::try_parse_from(
                std::iter::once(OsString::from("ra"))
                    .chain(checked_args)
                    .chain(std::iter::once(OsString::from("--managed-run"))),
            )?;
            let config = Config::load(cli)?;
            ensure!(
                config.cwd == workspace,
                "The updated profile selects a different workspace. Reopen the dashboard for this workspace."
            );
            if reload {
                Api::new(&config, Reporter::Silent, Budget::new(&config))?;
            }
                Ok::<_, anyhow::Error>(config.orchestration.command_timeout_secs)
            }).await??;
            if reload {
                self.control(run, ControlAction::CheckpointAndExit, None)
                    .await?;
                tokio::time::timeout(
                    Duration::from_secs(command_timeout.saturating_add(10)),
                    async {
                        loop {
                            let workspace = self.workspace.clone();
                            if !tokio::task::spawn_blocking(move || {
                                Store::has_live_writer(&workspace)
                            })
                            .await??
                            {
                                break;
                            }
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

async fn read_json(
    pool: Arc<Semaphore>,
    read: impl FnOnce() -> Result<Value> + Send + 'static,
) -> Reply {
    let Ok(permit) = pool.try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Dashboard read capacity reached",
        );
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        read().and_then(|value| Ok(serde_json::to_vec(&value)?))
    })
    .await
    {
        Ok(Ok(bytes)) => response(StatusCode::OK, "application/json; charset=utf-8", bytes),
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
