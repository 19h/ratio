use std::{
    fs, io,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use tokio::net::{UnixListener, UnixStream};

use super::workspace_key;

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    pub fn bind(workspace: &Path) -> Result<Self> {
        let path = endpoint(workspace, true)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&path).context("bind private Ratio control socket")?;
        Ok(Self { listener, path })
    }

    pub(crate) async fn accept(&mut self) -> Result<UnixStream> {
        let (client, _) = self.listener.accept().await?;
        Ok(client)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn endpoint(workspace: &Path, create: bool) -> Result<PathBuf> {
    // SAFETY: geteuid has no memory or lifetime preconditions.
    let uid = unsafe { libc::geteuid() };
    let directory = std::env::temp_dir().join(format!("ra-{uid}"));
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = directory
        .symlink_metadata()
        .context("open private Ratio runtime directory")?;
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "Ratio control directory must be private to the current user"
    );
    Ok(directory.join(workspace_key(workspace)))
}

pub(super) async fn connect(workspace: &Path) -> Result<UnixStream> {
    Ok(UnixStream::connect(endpoint(workspace, false)?).await?)
}

pub(super) fn detach(command: &mut Command) {
    // SAFETY: the child callback calls only async-signal-safe functions.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
