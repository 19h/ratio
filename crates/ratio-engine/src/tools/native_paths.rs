use std::{ffi::OsString, fs::File, path::Path};

use anyhow::{Context, Result, ensure};

#[cfg(unix)]
use std::{ffi::OsStr, os::fd::OwnedFd, path::Component};

#[cfg(unix)]
use rustix::fs::{AtFlags, Mode, OFlags, linkat, open, openat, renameat, unlinkat};

#[cfg(unix)]
pub(super) fn open_directory(workspace: &Path, path: &Path) -> Result<OwnedFd> {
    let relative = path
        .strip_prefix(workspace)
        .context("path leaves the workspace")?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = open(workspace, flags, Mode::empty())?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            anyhow::bail!("path is not resolved inside the workspace");
        };
        directory = openat(&directory, name, flags, Mode::empty())?;
    }
    Ok(directory)
}

pub(super) enum EntryKind {
    File,
    Directory,
    Other,
}

pub(super) fn entries(
    workspace: &Path,
    path: &Path,
) -> Result<impl Iterator<Item = Result<(OsString, EntryKind)>> + use<>> {
    #[cfg(unix)]
    {
        use rustix::fs::{Dir, FileType, statat};
        use std::os::unix::ffi::OsStrExt;

        let directory = open_directory(workspace, path)?;
        Ok(Dir::read_from(&directory)?.filter_map(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error.into())),
            };
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                return None;
            }
            let kind = if entry.file_type() == FileType::Unknown {
                statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_or(FileType::Unknown, |entry| {
                        FileType::from_raw_mode(entry.st_mode)
                    })
            } else {
                entry.file_type()
            };
            let kind = match kind {
                FileType::RegularFile => EntryKind::File,
                FileType::Directory => EntryKind::Directory,
                _ => EntryKind::Other,
            };
            Some(Ok((OsStr::from_bytes(name.to_bytes()).to_owned(), kind)))
        }))
    }
    #[cfg(not(unix))]
    {
        ensure!(path.starts_with(workspace), "path leaves the workspace");
        Ok(std::fs::read_dir(path)?.map(|entry| {
            let entry = entry?;
            let kind = entry.file_type()?;
            let kind = if kind.is_dir() {
                EntryKind::Directory
            } else if kind.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            Ok((entry.file_name(), kind))
        }))
    }
}

pub(super) struct FileTarget<'a> {
    #[cfg(unix)]
    directory: OwnedFd,
    #[cfg(unix)]
    name: &'a OsStr,
    #[cfg(not(unix))]
    path: &'a Path,
}

impl<'a> FileTarget<'a> {
    pub(super) fn new(workspace: &Path, path: &'a Path) -> Result<Self> {
        #[cfg(unix)]
        {
            let directory = open_directory(workspace, path.parent().context("file parent")?)?;
            let name = path.file_name().context("file name")?;
            Ok(Self { directory, name })
        }
        #[cfg(not(unix))]
        {
            ensure!(path.starts_with(workspace), "path leaves the workspace");
            Ok(Self { path })
        }
    }

    pub(super) fn is_regular(&self) -> bool {
        #[cfg(unix)]
        {
            use rustix::fs::{FileType, statat};

            statat(&self.directory, self.name, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|entry| FileType::from_raw_mode(entry.st_mode) == FileType::RegularFile)
        }
        #[cfg(not(unix))]
        {
            std::fs::symlink_metadata(self.path).is_ok_and(|entry| entry.is_file())
        }
    }

    pub(super) fn open(&self) -> Result<File> {
        #[cfg(unix)]
        let file = File::from(
            openat(
                &self.directory,
                self.name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| {
                if error == rustix::io::Errno::LOOP {
                    anyhow::anyhow!(
                        "symlink leaf is unavailable; use the workspace target directly"
                    )
                } else {
                    anyhow::Error::from(error).context("open workspace file")
                }
            })?,
        );
        #[cfg(not(unix))]
        let file = {
            ensure!(
                !std::fs::symlink_metadata(self.path)?
                    .file_type()
                    .is_symlink(),
                "open workspace file without following symlinks"
            );
            File::open(self.path)?
        };
        ensure!(file.metadata()?.is_file(), "expected a regular file");
        Ok(file)
    }

    /// Publish from the same pinned directory, optionally replacing a file.
    pub(super) fn publish(
        &self,
        text: &str,
        permissions: Option<std::fs::Permissions>,
        before_replace: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        use std::io::Write;

        #[cfg(unix)]
        {
            use ring::rand::{SecureRandom, SystemRandom};

            let mut nonce = [0u8; 16];
            SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| anyhow::anyhow!("generate temporary file name"))?;
            let name = format!(".ratio-edit-{:032x}", u128::from_ne_bytes(nonce));
            let mode = if permissions.is_some() { 0o600 } else { 0o666 };
            let mut temporary = File::from(openat(
                &self.directory,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
                Mode::from_raw_mode(mode),
            )?);
            let result = (|| {
                temporary.write_all(text.as_bytes())?;
                if let Some(permissions) = permissions {
                    temporary.set_permissions(permissions)?;
                    before_replace()?;
                    renameat(&self.directory, name.as_str(), &self.directory, self.name)?;
                } else {
                    self.publish_new(&name)?;
                }
                Ok(())
            })();
            let _ = unlinkat(&self.directory, name.as_str(), AtFlags::empty());
            result
        }
        #[cfg(not(unix))]
        {
            let mut temporary =
                tempfile::NamedTempFile::new_in(self.path.parent().context("file parent")?)?;
            temporary.write_all(text.as_bytes())?;
            if let Some(permissions) = permissions {
                temporary.as_file().set_permissions(permissions)?;
                before_replace()?;
                temporary
                    .persist(self.path)
                    .context("replace edited file")?;
            } else {
                temporary
                    .persist_noclobber(self.path)
                    .context("create new file")?;
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    fn publish_new(&self, temporary: &str) -> Result<()> {
        #[cfg(any(target_vendor = "apple", target_os = "linux"))]
        {
            use rustix::{
                fs::{RenameFlags, renameat_with},
                io::Errno,
            };

            match renameat_with(
                &self.directory,
                temporary,
                &self.directory,
                self.name,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => return Ok(()),
                Err(Errno::NOSYS | Errno::INVAL | Errno::NOTSUP) => {}
                Err(error) => return Err(error).context("create new file"),
            }
        }
        linkat(
            &self.directory,
            temporary,
            &self.directory,
            self.name,
            AtFlags::empty(),
        )
        .context("create new file")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, io::Read, os::unix::fs::symlink};

    use super::*;

    #[test]
    fn resolved_paths_cannot_be_redirected_by_a_symlink_swap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let original = workspace.join("directory");
        let sibling = root.join("sibling");
        fs::create_dir_all(&original).unwrap();
        fs::create_dir(&sibling).unwrap();
        fs::write(original.join("existing"), "in workspace").unwrap();
        fs::write(sibling.join("existing"), "outside workspace").unwrap();
        let existing_path = original.join("existing");
        let new_path = original.join("new");
        let existing = FileTarget::new(&workspace, &existing_path).unwrap();
        let new = FileTarget::new(&workspace, &new_path).unwrap();
        let moved = workspace.join("moved");
        fs::rename(&original, &moved).unwrap();
        symlink(&sibling, &original).unwrap();

        assert!(FileTarget::new(&workspace, &existing_path).is_err());
        assert!(entries(&workspace, &original).is_err());
        let mut contents = String::new();
        existing
            .open()
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "in workspace");
        new.publish("new contents", None, || Ok(())).unwrap();
        assert!(new.publish("duplicate", None, || Ok(())).is_err());
        assert_eq!(
            fs::read_to_string(moved.join("new")).unwrap(),
            "new contents"
        );
        assert!(!sibling.join("new").exists());
    }
}
