use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::session::WritePath;

type Manifest = BTreeMap<PathBuf, Fingerprint>;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    digest: String,
    mode: u32,
}

#[derive(Clone)]
pub(crate) struct Staging {
    root: PathBuf,
    live: PathBuf,
}

impl Staging {
    pub(crate) fn open(workspace: &Path, run_id: &str, resume: bool) -> Result<Self> {
        let directory = workspace.join(".ratio/staging");
        let root = directory.join(run_id);
        if !root.is_dir() {
            ensure!(!resume, "staging directory for resumed run is missing");
            let top = git(workspace, &["rev-parse", "--show-prefix"])?;
            ensure!(
                top.iter().all(u8::is_ascii_whitespace),
                "staged runs require the workspace to be the root of a Git working tree"
            );
            private_directory(&directory)?;
            let temp = tempfile::tempdir_in(&directory)?;
            let candidate = temp.path().join("candidate");
            let baseline = snapshot(workspace, None)?;
            copy_tree(workspace, &candidate, &baseline)?;
            save(&temp.path().join("baseline.json"), &baseline)?;
            fs::rename(temp.path(), &root).context("install run staging directory")?;
            sync_directory(&directory)?;
        }
        Ok(Self {
            root,
            live: workspace.to_owned(),
        })
    }

    pub(crate) fn candidate(&self) -> PathBuf {
        self.root.join("candidate")
    }

    fn worker_root(&self, cycle: usize, worker: usize) -> PathBuf {
        self.root
            .join("workers")
            .join(cycle.to_string())
            .join(worker.to_string())
    }

    pub(crate) fn worker(&self, cycle: usize, worker: usize) -> Result<PathBuf> {
        let root = self.worker_root(cycle, worker);
        if !root.is_dir() {
            let parent = root.parent().context("worker staging parent")?;
            fs::create_dir_all(parent)?;
            let temp = tempfile::tempdir_in(parent)?;
            let candidate = self.candidate();
            let baseline: Manifest = load(&self.root.join("baseline.json"))?;
            let base = snapshot(&candidate, Some(&baseline))?;
            copy_tree(&candidate, &temp.path().join("tree"), &base)?;
            save(&temp.path().join("base.json"), &base)?;
            fs::rename(temp.path(), &root).context("install worker staging directory")?;
            sync_directory(parent)?;
        }
        Ok(root.join("tree"))
    }

    pub(crate) fn merged(&self, cycle: usize, worker: usize) -> bool {
        self.worker_root(cycle, worker)
            .join("merged.json")
            .is_file()
    }

    pub(crate) fn merge(
        &self,
        cycle: usize,
        worker: usize,
        ownership: Option<&[WritePath]>,
    ) -> Result<String> {
        let root = self.worker_root(cycle, worker);
        let receipt = root.join("merged.json");
        if receipt.is_file() {
            return load(&receipt);
        }
        let before: Manifest = load(&root.join("base.json"))?;
        let tree = root.join("tree");
        let after = snapshot(&tree, Some(&before))?;
        if let Some(paths) = ownership {
            for path in changed(&before, &after) {
                ensure!(
                    paths.iter().any(|entry| entry.includes(path)),
                    "worker_{} changed unowned path {:?} in staging",
                    worker + 1,
                    path
                );
            }
        }
        reconcile(
            &tree,
            &self.candidate(),
            &before,
            &after,
            &root.join("merge.json"),
        )?;
        let digest = digest_manifest(&after)?;
        save(&receipt, &digest)?;
        Ok(digest)
    }

    pub(crate) fn candidate_digest(&self) -> Result<String> {
        self.digest_at(&self.candidate())
    }

    pub(crate) fn digest_at(&self, tree: &Path) -> Result<String> {
        let baseline = load(&self.root.join("baseline.json"))?;
        digest_manifest(&snapshot(tree, Some(&baseline))?)
    }

    pub(crate) fn verification_copy(&self) -> Result<tempfile::TempDir> {
        let temp = tempfile::tempdir_in(&self.root)?;
        let candidate = self.candidate();
        let baseline = load(&self.root.join("baseline.json"))?;
        let current = snapshot(&candidate, Some(&baseline))?;
        copy_tree(&candidate, temp.path(), &current)?;
        let origin = candidate.to_str().context("non-UTF-8 candidate location")?;
        git(
            temp.path(),
            &["fetch", "--quiet", "--no-tags", origin, "HEAD"],
        )?;
        git(temp.path(), &["reset", "--quiet", "--mixed", "FETCH_HEAD"])?;
        Ok(temp)
    }

    pub(crate) fn publish(&self, approved_digest: &str) -> Result<()> {
        let before = load(&self.root.join("baseline.json"))?;
        let candidate = self.candidate();
        let after = snapshot(&candidate, Some(&before))?;
        ensure!(
            digest_manifest(&after)? == approved_digest,
            "candidate changed after verification or review"
        );
        reconcile(
            &candidate,
            &self.live,
            &before,
            &after,
            &self.root.join("publication.json"),
        )
    }
}

pub(crate) fn is_root_with_head(workspace: &Path) -> bool {
    git(
        workspace,
        &["rev-parse", "--is-inside-work-tree", "--show-prefix"],
    )
    .is_ok_and(|output| String::from_utf8_lossy(&output).trim_end() == "true")
        && git(workspace, &["rev-parse", "--verify", "HEAD"]).is_ok()
}

pub(crate) fn git(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(workspace)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("run Git for staging")?;
    ensure!(
        output.status.success(),
        "Git {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn snapshot(root: &Path, required: Option<&Manifest>) -> Result<Manifest> {
    let entries = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut paths = BTreeSet::new();
    for entry in entries
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let path = PathBuf::from(std::str::from_utf8(entry).context("non-UTF-8 staged path")?);
        if !reserved(&path) {
            paths.insert(path);
        }
    }
    if let Some(required) = required {
        paths.extend(required.keys().cloned());
    }
    let mut manifest = Manifest::new();
    for path in paths {
        if let Some(fingerprint) = fingerprint(root, &path)? {
            manifest.insert(path, fingerprint);
        }
    }
    Ok(manifest)
}

fn reserved(path: &Path) -> bool {
    path.components()
        .any(|part| matches!(part, Component::Normal(name) if name == ".git" || name == ".ratio"))
}

fn checked_file(root: &Path, relative: &Path) -> Result<Option<(PathBuf, fs::Metadata)>> {
    ensure!(
        !relative.as_os_str().is_empty()
            && !reserved(relative)
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "invalid staged path {relative:?}"
    );
    let mut current = root.to_owned();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        current.push(part);
        match current.symlink_metadata() {
            Ok(metadata) if parts.peek().is_none() => {
                ensure!(metadata.is_file(), "expected a regular file at {current:?}");
                return Ok(Some((current, metadata)));
            }
            Ok(metadata) => ensure!(metadata.is_dir(), "expected a directory at {current:?}"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("a nonempty relative path has components")
}

fn fingerprint(root: &Path, path: &Path) -> Result<Option<Fingerprint>> {
    let Some((full, metadata)) = checked_file(root, path)? else {
        return Ok(None);
    };
    let mut file = File::open(full)?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = u32::from(metadata.permissions().readonly());
    Ok(Some(Fingerprint {
        digest: hex(hash.finish()),
        mode,
    }))
}

fn digest_manifest(state: &Manifest) -> Result<String> {
    Ok(hex(ring::digest::digest(
        &ring::digest::SHA256,
        &serde_json::to_vec(state)?,
    )))
}

fn hex(value: ring::digest::Digest) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn copy_tree(source: &Path, destination: &Path, files: &Manifest) -> Result<()> {
    fs::create_dir_all(destination)?;
    for (path, expected) in files {
        ensure!(
            fingerprint(source, path)?.as_ref() == Some(expected),
            "source changed while staging {path:?}"
        );
        replace_file(source, destination, path, None, expected)?;
        ensure!(
            fingerprint(destination, path)?.as_ref() == Some(expected),
            "source changed while copying {path:?}"
        );
    }
    git(destination, &["init", "--quiet", "--template="])?;
    git(destination, &["add", "--force", "--all", "--", "."])?;
    git(
        destination,
        &[
            "-c",
            "user.name=Ratio",
            "-c",
            "user.email=ratio@localhost",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=.git/ratio-disabled-hooks",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Ratio staging baseline",
        ],
    )?;
    Ok(())
}

fn changed<'a>(before: &'a Manifest, after: &'a Manifest) -> Vec<&'a PathBuf> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .collect()
}

fn reconcile(
    source: &Path,
    destination: &Path,
    before: &Manifest,
    after: &Manifest,
    intent: &Path,
) -> Result<()> {
    let paths = changed(before, after);
    let target = digest_manifest(after)?;
    let recovering = intent.is_file();
    if recovering {
        ensure!(
            load::<String>(intent)? == target,
            "candidate differs from recorded publication intent"
        );
    }
    for path in &paths {
        let current = fingerprint(destination, path)?;
        ensure!(
            current.as_ref() == before.get(*path)
                || (recovering && current.as_ref() == after.get(*path)),
            "publication conflict at {path:?}: working copy changed since the recorded baseline"
        );
    }
    if !recovering {
        save(intent, &target)?;
    }
    for path in paths {
        let current = fingerprint(destination, path)?;
        if current.as_ref() == after.get(path) {
            continue;
        }
        ensure!(
            current.as_ref() == before.get(path),
            "publication conflict at {path:?}"
        );
        if let Some(expected) = after.get(path) {
            ensure!(
                fingerprint(source, path)?.as_ref() == Some(expected),
                "candidate changed at {path:?}"
            );
            replace_file(source, destination, path, current.as_ref(), expected)?;
        } else {
            let output = destination.join(path);
            fs::remove_file(&output)?;
            sync_directory(output.parent().context("deleted file parent")?)?;
        }
        ensure!(
            fingerprint(destination, path)?.as_ref() == after.get(path),
            "publication changed concurrently at {path:?}"
        );
    }
    Ok(())
}

fn replace_file(
    source: &Path,
    destination: &Path,
    path: &Path,
    before: Option<&Fingerprint>,
    after: &Fingerprint,
) -> Result<()> {
    let (input, metadata) = checked_file(source, path)?.context("staged source file is missing")?;
    let output = destination.join(path);
    let parent = output.parent().context("staged file parent")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    io::copy(&mut File::open(input)?, &mut file)?;
    file.as_file().set_permissions(metadata.permissions())?;
    file.as_file().sync_all()?;
    let temporary_path = file.path().strip_prefix(destination)?;
    ensure!(
        fingerprint(destination, temporary_path)?.as_ref() == Some(after),
        "source changed while copying {path:?}"
    );
    ensure!(
        fingerprint(destination, path)?.as_ref() == before,
        "destination changed at {path:?}"
    );
    if before.is_some() {
        file.persist(&output)?;
    } else {
        file.persist_noclobber(&output)?;
    }
    sync_directory(parent)
}

fn private_directory(directory: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    Ok(())
}

fn save<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    let parent = path.parent().context("staging receipt parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, data)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    sync_directory(parent)
}

fn load<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_reader(File::open(path).with_context(|| format!("open {path:?}"))?)
        .with_context(|| format!("decode {path:?}"))
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)?
        .sync_all()
        .context("sync staging directory")
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_conflicts_before_writing_and_resumes_a_partial_publication() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        let candidate = root.path().join("candidate");
        fs::create_dir(&live).unwrap();
        fs::create_dir(&candidate).unwrap();
        let names = ["a", "b"].map(PathBuf::from);
        for name in &names {
            fs::write(live.join(name), b"baseline").unwrap();
            fs::write(candidate.join(name), b"desired").unwrap();
        }
        let manifest = |tree: &Path| -> Manifest {
            names
                .iter()
                .cloned()
                .map(|name| {
                    let hash = fingerprint(tree, &name).unwrap().unwrap();
                    (name, hash)
                })
                .collect()
        };
        let before = manifest(&live);
        let after = manifest(&candidate);
        let intent = root.path().join("intent.json");
        fs::write(live.join("b"), b"concurrent").unwrap();
        assert!(reconcile(&candidate, &live, &before, &after, &intent).is_err());
        assert_eq!(fs::read(live.join("a")).unwrap(), b"baseline");
        assert!(!intent.exists());
        fs::write(live.join("b"), b"baseline").unwrap();
        save(&intent, &digest_manifest(&after).unwrap()).unwrap();
        fs::write(live.join("a"), b"desired").unwrap();
        reconcile(&candidate, &live, &before, &after, &intent).unwrap();
        assert_eq!(manifest(&live), after);
    }
}
