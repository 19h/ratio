use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::mpsc::{SyncSender, TrySendError},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use ratio_protocol::{ControlAction, ControlReply, ControlRequest};
use serde_json::from_slice;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::timeout,
};

use ratio_engine::session::Store;
use ratio_engine::{
    budget::Budget,
    events::{GenerationControl, LiveGenerations},
};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

const REQUEST_CAP: usize = 16 * 1024;
// A status response can include a redacted preview for each active request.
const RESPONSE_CAP: usize = 512 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

pub use platform::Listener;

pub async fn telemetry(workspace: &Path) -> Result<impl AsyncRead + Unpin> {
    timeout(REQUEST_TIMEOUT, platform::connect(workspace, true)).await?
}

pub async fn read_telemetry(stream: &mut (impl AsyncRead + Unpin)) -> Result<ControlReply> {
    let bytes = timeout(Duration::from_secs(3), read_frame(stream, RESPONSE_CAP)).await??;
    Ok(from_slice(&bytes)?)
}

pub async fn launch(
    workspace: &Path,
    request_text: Option<&str>,
) -> Result<(Option<ControlReply>, PathBuf)> {
    launch_with_args(
        workspace,
        std::env::args_os()
            .skip(1)
            .filter(|argument| argument != "--detached"),
        request_text,
    )
    .await
}

pub async fn launch_with_args(
    workspace: &Path,
    args: impl IntoIterator<Item = std::ffi::OsString>,
    request_text: Option<&str>,
) -> Result<(Option<ControlReply>, PathBuf)> {
    let directory = workspace.join(".ratio");
    let (log, error_log, log_path) = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&directory)?;
        let (log, log_path) = tempfile::Builder::new()
            .prefix("runner-")
            .suffix(".log")
            .tempfile_in(directory)?
            .keep()?;
        let error_log = log.try_clone()?;
        Ok::<_, anyhow::Error>((log, error_log, log_path))
    })
    .await??;
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .args(args)
        .arg("--managed-run")
        .stdin(if request_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log));
    platform::detach(command.as_std_mut());
    let mut child = command.spawn().context("start detached Ratio run")?;
    if let Some(request_text) = request_text {
        child
            .stdin
            .take()
            .context("open detached request pipe")?
            .write_all(request_text.as_bytes())
            .await?;
    }
    let request = ControlRequest {
        action: ControlAction::Status,
        guidance: None,
        run_id: None,
    };
    let startup = async {
        loop {
            if let Ok(reply) = request_control(workspace, &request).await {
                if Some(reply.pid) == child.id() {
                    return Ok(Some(reply));
                }
            }
            if let Some(status) = child.try_wait()? {
                ensure!(
                    status.success(),
                    "detached process exited during startup: {status}. See {}",
                    log_path.display()
                );
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    match timeout(Duration::from_secs(10), startup).await {
        Ok(result) => {
            let result = result?;
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            Ok((result, log_path))
        }
        Err(_) => {
            child
                .kill()
                .await
                .context("terminate detached process that failed to start")?;
            let _ = child.wait().await?;
            anyhow::bail!(
                "detached process did not open its control endpoint. See {}",
                log_path.display()
            )
        }
    }
}

pub async fn request_control(workspace: &Path, request: &ControlRequest) -> Result<ControlReply> {
    timeout(REQUEST_TIMEOUT, async {
        let mut connection = platform::connect(workspace, false)
            .await
            .context("connect to active Ratio run")?;
        send_frame(&mut connection, &serde_json::to_vec(request)?, REQUEST_CAP).await?;
        let bytes = read_frame(&mut connection, RESPONSE_CAP).await?;
        Ok(from_slice(&bytes)?)
    })
    .await
    .context("Ratio control request timed out")?
}

pub async fn serve_telemetry(
    mut listener: Listener,
    run_id: &str,
    dashboard_managed: bool,
    generation: watch::Receiver<GenerationControl>,
    live: &LiveGenerations,
    budget: &Budget,
) -> Result<()> {
    let (snapshots, _) = watch::channel(None::<std::sync::Arc<Vec<u8>>>);
    let connections = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_send = tokio::time::Instant::now();
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let mut socket = connection?;
                let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
                let mut frames = snapshots.subscribe();
                tokio::spawn(async move {
                    let _permit = permit;
                    loop {
                        let frame = frames.borrow_and_update().clone();
                        if let Some(frame) = frame {
                            if !matches!(timeout(REQUEST_TIMEOUT, send_frame(&mut socket, &frame, RESPONSE_CAP)).await, Ok(Ok(()))) {
                                break;
                            }
                        }
                        if frames.changed().await.is_err() {
                            break;
                        }
                    }
                });
            }
            _ = tick.tick() => {
                if snapshots.receiver_count() == 0 {
                    continue;
                }
                let mut status = reply(run_id, generation.borrow().paused, true, "run is active");
                status.dashboard_managed = dashboard_managed;
                status.generations = live.snapshot();
                status.token_usage = Some(budget.live_token_usage());
                let bytes = serde_json::to_vec(&status)?;
                let heartbeat = last_send.elapsed() >= Duration::from_secs(1);
                if snapshots.send_if_modified(|old| {
                    if !heartbeat && old.as_ref().is_some_and(|old| **old == bytes) {
                        return false;
                    }
                    *old = Some(std::sync::Arc::new(bytes));
                    true
                }) {
                    last_send = tokio::time::Instant::now();
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    mut listener: Listener,
    run_id: &str,
    dashboard_managed: bool,
    generation: watch::Sender<GenerationControl>,
    guidance: SyncSender<String>,
    live: LiveGenerations,
    store: &Store,
    budget: &Budget,
) -> Result<()> {
    loop {
        let mut connection = listener.accept().await?;
        // The endpoint is private to the operator. Bound each client so an
        // idle or malformed request cannot monopolize it.
        let cancelled = timeout(REQUEST_TIMEOUT, async {
            let bytes = read_frame(&mut connection, REQUEST_CAP).await?;
            let request: ControlRequest = match from_slice(&bytes) {
                Ok(request) => request,
                Err(error) => {
                    let reply = reply(
                        run_id,
                        generation.borrow().paused,
                        false,
                        &format!("invalid request: {error}"),
                    );
                    send_frame(&mut connection, &serde_json::to_vec(&reply)?, RESPONSE_CAP).await?;
                    return Ok::<_, anyhow::Error>(false);
                }
            };
            if request
                .run_id
                .as_deref()
                .is_some_and(|expected| expected != run_id)
            {
                let response = reply(
                    run_id,
                    generation.borrow().paused,
                    false,
                    "requested run is no longer active",
                );
                send_frame(
                    &mut connection,
                    &serde_json::to_vec(&response)?,
                    RESPONSE_CAP,
                )
                .await?;
                return Ok(false);
            }
            let (accepted, detail) = match (request.action, request.guidance) {
                (ControlAction::Triage, Some(text)) => {
                    let (accepted, detail) = match store.submit_triage(run_id, &text) {
                        Ok(id) => (true, format!("Information {id} queued for triage")),
                        Err(error) => (false, format!("{error:#}")),
                    };
                    if accepted {
                        store.record_control(run_id, "Submit information", &detail)?;
                    }
                    let response = reply(run_id, generation.borrow().paused, accepted, &detail);
                    send_frame(
                        &mut connection,
                        &serde_json::to_vec(&response)?,
                        RESPONSE_CAP,
                    )
                    .await?;
                    return Ok(false);
                }
                (ControlAction::RetryTriage, Some(id)) => {
                    let result = id
                        .parse::<i64>()
                        .ok()
                        .and_then(|id| store.retry_triage(run_id, id).ok())
                        .unwrap_or(false);
                    if result {
                        store.record_control(
                            run_id,
                            "Retry information",
                            "Information queued for retry",
                        )?;
                    }
                    let response = reply(
                        run_id,
                        generation.borrow().paused,
                        result,
                        if result {
                            "Information queued for retry"
                        } else {
                            "Item cannot be retried"
                        },
                    );
                    send_frame(
                        &mut connection,
                        &serde_json::to_vec(&response)?,
                        RESPONSE_CAP,
                    )
                    .await?;
                    return Ok(false);
                }
                (ControlAction::Triage | ControlAction::RetryTriage, None) => {
                    (false, "Triage requires information or an item ID")
                }
                (ControlAction::AddWorker, text) => {
                    let result = text
                        .as_deref()
                        .context("add-worker requires a worker request")
                        .and_then(|text| store.request_worker(run_id, text));
                    let (accepted, detail) = match result {
                        Ok(id) => (true, format!("Worker request {id} queued")),
                        Err(error) => (false, format!("{error:#}")),
                    };
                    if accepted {
                        store.record_control(run_id, "Add worker", &detail)?;
                    }
                    let response = reply(run_id, generation.borrow().paused, accepted, &detail);
                    send_frame(
                        &mut connection,
                        &serde_json::to_vec(&response)?,
                        RESPONSE_CAP,
                    )
                    .await?;
                    return Ok(false);
                }
                (ControlAction::Guide, Some(text))
                    if !text.trim().is_empty() && text.len() <= 4096 =>
                {
                    match guidance.try_send(text) {
                        Ok(()) => (true, "guidance queued"),
                        Err(TrySendError::Full(_)) => (false, "guidance queue is full"),
                        Err(TrySendError::Disconnected(_)) => (false, "run has finished"),
                    }
                }
                (ControlAction::Guide, _) => (false, "guide requires 1 to 4096 bytes of text"),
                (_, Some(_)) => (false, "guidance is only valid for guide"),
                (ControlAction::Pause, _) => {
                    generation.send_modify(|state| {
                        if !state.paused {
                            state.paused = true;
                            state.epoch = state.epoch.wrapping_add(1);
                        }
                    });
                    (true, "pause requested")
                }
                (ControlAction::Resume, _) => {
                    if generation.borrow().checkpoint_and_exit {
                        (false, "runner is saving its restart checkpoint")
                    } else {
                        generation.send_modify(|state| state.paused = false);
                        (true, "model requests resumed")
                    }
                }
                (ControlAction::CheckpointAndExit, _) => {
                    if dashboard_managed && generation.borrow().paused {
                        (true, "runner will exit at its next checkpoint")
                    } else {
                        (
                            false,
                            "checkpoint restart requires a paused dashboard runner",
                        )
                    }
                }
                (ControlAction::Cancel, _) => (true, "run cancellation requested"),
                (ControlAction::Status, _) => (true, "run is active"),
            };
            let mut response = reply(run_id, generation.borrow().paused, accepted, detail);
            if accepted && request.action != ControlAction::Status {
                store.record_control(run_id, &format!("{:?}", request.action), detail)?;
            }
            if accepted && request.action == ControlAction::Status && request.run_id.is_some() {
                response.generations = live.snapshot();
                response.token_usage = Some(budget.live_token_usage());
            }
            response.dashboard_managed = dashboard_managed;
            let result = send_frame(
                &mut connection,
                &serde_json::to_vec(&response)?,
                RESPONSE_CAP,
            )
            .await;
            if accepted && request.action == ControlAction::Cancel {
                return Ok(true);
            }
            result?;
            if accepted && request.action == ControlAction::CheckpointAndExit {
                generation.send_modify(|state| state.checkpoint_and_exit = true);
            }
            Ok(false)
        })
        .await;
        if matches!(cancelled, Ok(Ok(true))) {
            return Ok(());
        }
    }
}

fn reply(run_id: &str, paused: bool, accepted: bool, detail: &str) -> ControlReply {
    ControlReply {
        pid: std::process::id(),
        run_id: run_id.into(),
        paused,
        accepted,
        detail: detail.into(),
        dashboard_managed: false,
        generations: Vec::new(),
        token_usage: None,
    }
}

fn workspace_key(workspace: &Path) -> String {
    ring::digest::digest(
        &ring::digest::SHA256,
        workspace.as_os_str().as_encoded_bytes(),
    )
    .as_ref()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin), cap: usize) -> io::Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame too large",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn send_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    cap: usize,
) -> io::Result<()> {
    if bytes.len() > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control frame too large",
        ));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}
