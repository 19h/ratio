use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

use ratio_engine::events::GenerationControl;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

const MESSAGE_CAP: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) use platform::Listener;

pub(crate) async fn launch(workspace: &Path) -> Result<(Option<ControlReply>, PathBuf)> {
    let directory = workspace.join(".ratio");
    std::fs::create_dir_all(&directory)?;
    let (log, log_path) = tempfile::Builder::new()
        .prefix("runner-")
        .suffix(".log")
        .tempfile_in(directory)?
        .keep()?;
    let error_log = log.try_clone()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(
            std::env::args_os()
                .skip(1)
                .filter(|argument| argument != "--detached"),
        )
        .arg("--managed-run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log));
    platform::detach(&mut command);
    let mut child = command.spawn().context("start detached Ratio run")?;
    let request = ControlRequest {
        action: ControlAction::Status,
        guidance: None,
        run_id: None,
    };
    let startup = async {
        loop {
            if let Ok(reply) = request_control(workspace, &request).await {
                if reply.pid == child.id() {
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
        Ok(result) => Ok((result?, log_path)),
        Err(_) => {
            child
                .kill()
                .context("terminate detached process that failed to start")?;
            let _ = child.wait()?;
            anyhow::bail!(
                "detached process did not open its control endpoint. See {}",
                log_path.display()
            )
        }
    }
}

pub(crate) async fn request_control(
    workspace: &Path,
    request: &ControlRequest,
) -> Result<ControlReply> {
    timeout(REQUEST_TIMEOUT, async {
        let mut connection = platform::connect(workspace)
            .await
            .context("connect to detached Ratio run")?;
        send_frame(&mut connection, &serde_json::to_vec(request)?).await?;
        let bytes = read_frame(&mut connection).await?;
        Ok(from_slice(&bytes)?)
    })
    .await
    .context("detached control request timed out")?
}

pub(crate) async fn serve(
    mut listener: Listener,
    run_id: &str,
    generation: watch::Sender<GenerationControl>,
    guidance: SyncSender<String>,
) -> Result<()> {
    loop {
        let mut connection = listener.accept().await?;
        // The endpoint is private to the operator. Bound each client so an
        // idle or malformed request cannot monopolize it.
        let cancelled = timeout(REQUEST_TIMEOUT, async {
            let bytes = read_frame(&mut connection).await?;
            let request: ControlRequest = match from_slice(&bytes) {
                Ok(request) => request,
                Err(error) => {
                    let reply = reply(
                        run_id,
                        generation.borrow().paused,
                        false,
                        &format!("invalid request: {error}"),
                    );
                    send_frame(&mut connection, &serde_json::to_vec(&reply)?).await?;
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
                send_frame(&mut connection, &serde_json::to_vec(&response)?).await?;
                return Ok(false);
            }
            let (accepted, detail) = match (request.action, request.guidance) {
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
                    generation.send_modify(|state| state.paused = false);
                    (true, "model requests resumed")
                }
                (ControlAction::Cancel, _) => (true, "run cancellation requested"),
                (ControlAction::Status, _) => (true, "run is active"),
            };
            let response = reply(run_id, generation.borrow().paused, accepted, detail);
            let result = send_frame(&mut connection, &serde_json::to_vec(&response)?).await;
            if accepted && request.action == ControlAction::Cancel {
                return Ok(true);
            }
            result?;
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

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length > MESSAGE_CAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control frame too large",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn send_frame(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MESSAGE_CAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control frame too large",
        ));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}
