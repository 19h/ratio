//! Subprocess lifecycle management for opencode agent instances.
//!
//! Both the worker and the reviewer are opencode subprocesses communicating
//! over ACP via stdin/stdout. This module provides generic spawning and
//! lifecycle management for either role.

use std::fmt;
use std::path::Path;
use std::rc::Rc;

use agent_client_protocol as acp;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::config::AgentConfig;
use crate::protocol::{AgentEvent, OrchestratorClient, WorkerConnection};

// ---------------------------------------------------------------------------
// Agent role
// ---------------------------------------------------------------------------

/// Identifies which role a subprocess is fulfilling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRole {
    /// The agent doing the actual coding / implementation work.
    Worker,
    /// The agent reviewing the worker's output and making judgments.
    Reviewer,
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Worker => write!(f, "worker"),
            Self::Reviewer => write!(f, "reviewer"),
        }
    }
}

// ---------------------------------------------------------------------------
// Process handle
// ---------------------------------------------------------------------------

/// Manages the lifecycle of a single opencode subprocess.
pub struct AgentProcess {
    child: Child,
    role: AgentRole,
    /// Stderr handle from the subprocess (must be drained to prevent deadlock).
    pub stderr: Option<tokio::process::ChildStderr>,
}

impl AgentProcess {
    /// Which role this process fulfills.
    pub fn role(&self) -> AgentRole {
        self.role
    }

    /// Kill the subprocess immediately.
    pub fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Check whether the process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait for the process to exit and return its exit status.
    pub async fn wait(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        Ok(self.child.wait().await?)
    }
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

/// Spawn an opencode subprocess in ACP mode for the given role.
///
/// The subprocess is started with `opencode acp --cwd <cwd>`, communicating
/// over stdin/stdout using newline-delimited JSON-RPC (the ACP wire format).
///
/// # Returns
///
/// A tuple of:
/// - [`WorkerConnection`] — the typed ACP connection for sending prompts
/// - [`AgentProcess`] — a handle to kill / wait on the subprocess
/// - A future that must be spawned to drive the ACP I/O loop
pub fn spawn_agent(
    role: AgentRole,
    config: &AgentConfig,
    cwd: &Path,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) -> anyhow::Result<(
    WorkerConnection,
    AgentProcess,
    impl std::future::Future<Output = acp::Result<()>> + use<>,
)> {
    let mut cmd = Command::new(&config.binary);
    cmd.arg("acp");
    cmd.arg("--cwd").arg(cwd);

    // Forward any extra environment variables.
    for env_var in &config.env {
        cmd.env(&env_var.key, &env_var.value);
    }

    // Forward extra args.
    for arg in &config.extra_args {
        cmd.arg(arg);
    }

    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "Failed to spawn opencode ({role}) at '{}': {e}\n\
             Make sure opencode is installed and in your PATH.",
            config.binary
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture {role} stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture {role} stdout"))?;

    let outgoing = stdin.compat_write();
    let incoming = stdout.compat();

    let client = Rc::new(OrchestratorClient::new(event_tx, true));
    let client_for_conn = Rc::clone(&client);

    let (conn, io_task) = acp::ClientSideConnection::new(client, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });

    let stderr = child.stderr.take();

    let worker_conn = WorkerConnection::new(conn, client_for_conn);
    let agent_proc = AgentProcess {
        child,
        role,
        stderr,
    };

    Ok((worker_conn, agent_proc, io_task))
}
