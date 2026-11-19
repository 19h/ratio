use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

use crate::{api::ToolCall, context::clip, line_tags};

pub mod mcp;
pub(crate) mod skills;
#[cfg(windows)]
mod windows_job;

#[derive(Clone, Copy)]
pub enum Access {
    Inspect,
    Work,
}

pub struct Tools<'a> {
    pub ownership: Option<&'a [crate::session::WritePath]>,
    pub workspace: &'a Path,
    pub access: Access,
    pub timeout: Duration,
    pub output_bytes: usize,
    pub key: Option<&'a str>,
}

impl Tools<'_> {
    pub fn schemas(&self) -> Vec<Value> {
        let mut schemas = vec![
            schema(
                "read_file",
                "Read a UTF-8 workspace file. Each line is number tag|text. Quote tags in edit_file. Use start_line to continue, or offset for a raw byte range.",
                json!({
                    "path": {"type":"string"},
                    "offset": {"type":"integer","minimum":0},
                    "start_line": {"type":"integer","minimum":1},
                    "end_line": {"type":"integer","minimum":1},
                    "max_bytes": {"type":"integer","minimum":128}
                }),
                &["path"],
            ),
            schema(
                "list_directory",
                "List immediate entries in a workspace directory.",
                json!({
                    "path": {"type":"string"}
                }),
                &["path"],
            ),
            schema(
                "git_status",
                "Inspect tracked and untracked changes in the workspace.",
                json!({}),
                &[],
            ),
            schema(
                "git_diff",
                "Inspect staged or unstaged changes in the workspace.",
                json!({
                    "staged": {"type":"boolean"}
                }),
                &[],
            ),
        ];
        if matches!(self.access, Access::Work) {
            schemas.push(schema(
                "create_file",
                "Create a UTF-8 file in an existing workspace directory. Fails if the path exists.",
                json!({
                    "path": {"type":"string"},
                    "content": {"type":"string"}
                }),
                &["path", "content"],
            ));
            schemas.push(schema("edit_file", "Atomically edit lines by tags from read_file. Use tag, tag..tag inclusive, +tag before, tag+ after; new text preserves explicit indentation, null deletes. One stale tag aborts all edits. Legacy old_text and new_text require a unique exact match.", json!({
                "path": {"type":"string"},
                "edits": {"type":"object","additionalProperties":{"type":["string","null"]}},
                "line_hints": {"type":"object","description":"Optional previous 1-based line by tag for stale-tag diagnostics.","additionalProperties":{"type":"integer","minimum":1}},
                "snapshot_id": {"type":"string","description":"Required for range edits. Copy from read_file."},
                "old_text": {"type":"string"},
                "new_text": {"type":"string"}
            }), &["path"]));
            schemas.push(schema("shell", "Run a shell command from the workspace directory for tasks such as building, searching and testing.", json!({
                "command": {"type":"string"}
            }), &["command"]));
        }
        schemas
    }

    pub fn label(&self, call: &ToolCall) -> String {
        let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
        let detail = args
            .get("path")
            .or_else(|| args.get("command"))
            .and_then(Value::as_str);
        let title = match detail {
            Some(detail) => format!(
                "{} · {}",
                call.function.name,
                detail.lines().next().unwrap_or_default()
            ),
            None => call.function.name.clone(),
        };
        clip(&self.redact(title), 240)
    }

    pub async fn execute(&self, call: &ToolCall) -> String {
        self.execute_audited(call).await.0
    }

    pub(crate) async fn execute_audited(&self, call: &ToolCall) -> (String, String) {
        let mut audit = None;
        let result = match self.dispatch(call, &mut audit).await {
            Ok(result) => result,
            Err(error) => format!("Tool error: {error:#}"),
        };
        let result = self.redact(result);
        let full = audit.map_or_else(|| result.clone(), |audit| self.redact(audit));
        (clip(&result, self.output_bytes), full)
    }

    fn redact(&self, text: String) -> String {
        match self.key {
            Some(key) => text.replace(key, "[redacted]"),
            None => text,
        }
    }

    async fn dispatch(&self, call: &ToolCall, audit: &mut Option<String>) -> Result<String> {
        ensure!(call.kind == "function", "unsupported tool call type");
        let arguments = &call.function.arguments;
        match call.function.name.as_str() {
            "read_file" => {
                let args: ReadFile = crate::api::tool_arguments(arguments)?;
                self.read(&args, audit).await
            }
            "list_directory" => {
                let args: PathArg = crate::api::tool_arguments(arguments)?;
                let (output, full) = self.list(&args.path).await?;
                *audit = Some(format!("Untrusted directory listing\n{full}"));
                Ok(format!("Untrusted directory listing\n{output}"))
            }
            "git_status" => {
                let _: Empty = crate::api::tool_arguments(arguments)?;
                let (_, output, full) = self
                    .command_result(git().args([
                        "status",
                        "--short",
                        "--untracked-files=normal",
                        "--ignore-submodules=all",
                        "--",
                        ".",
                        ":(exclude).ratio",
                    ]))
                    .await?;
                *audit = Some(full);
                Ok(format!("Untrusted Git status\n{output}"))
            }
            "git_diff" => {
                let args: Diff = crate::api::tool_arguments(arguments)?;
                let mut command = git();
                command.args([
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=all",
                ]);
                if args.staged {
                    command.arg("--cached");
                }
                let (_, output, full) = self
                    .command_result(command.args(["--", ".", ":(exclude).ratio"]))
                    .await?;
                *audit = Some(full);
                Ok(format!("Untrusted Git diff\n{output}"))
            }
            "create_file" if matches!(self.access, Access::Work) => {
                let args: CreateFile = crate::api::tool_arguments(arguments)?;
                let path = self.edit_path(&args.path).await?;
                let content_len = args.content.len();
                let snapshot = tokio::task::spawn_blocking(move || {
                    create_file(&path, &args.content)?;
                    Ok::<_, anyhow::Error>(snapshot_id(&args.content))
                })
                .await??;
                let result = format!("Created {} ({} bytes).", args.path.display(), content_len);
                *audit = Some(format!("{result}\nSnapshot ID: {snapshot}"));
                Ok(result)
            }
            "edit_file" if matches!(self.access, Access::Work) => {
                let args: EditFile = crate::api::tool_arguments(arguments)?;
                let path = self.edit_path(&args.path).await?;
                match (args.edits, args.old_text, args.new_text) {
                    (Some(edits), None, None) => {
                        let changed_path = path.clone();
                        let count = edits.len();
                        let (updated, first, patch) = tokio::task::spawn_blocking(move || {
                            let mut patch = String::new();
                            let (updated,first) = change_file(&changed_path, |original| {
                                if edits.keys().any(|tag| tag.contains("..")) || args.snapshot_id.is_some() {
                                    let current = snapshot_id(original);
                                    if args.snapshot_id.as_deref() != Some(&current) {
                                        let lines = line_tags::lines(original).count();
                                        let start = args.line_hints.values().copied().find(|line| {
                                            (1..=lines).contains(line)
                                        }).unwrap_or(1);
                                        let excerpt = line_tags::excerpt(&changed_path.to_string_lossy(), original,
                                            start, Some(start.saturating_add(2)), 700)?;
                                        anyhow::bail!("range edit requires a current snapshot_id from read_file. Current snapshot ID: {current}\nUntrusted current file excerpt:\n{excerpt}");
                                    }
                                }
                                let (updated,first) = line_tags::apply(&changed_path.to_string_lossy(), original, &edits, &args.line_hints)?;
                                patch = edit_receipt(original,&updated);
                                Ok((updated,first))
                            })?;
                            Ok::<_,anyhow::Error>((updated,first,patch))
                        })
                        .await??;
                        let mut result =
                            format!("Edited {} ({} tagged changes).", args.path.display(), count);
                        let line_count = line_tags::lines(&updated).count();
                        result.push_str(&format!("\nSnapshot ID: {}", snapshot_id(&updated)));
                        if line_count > 0 {
                            let start = first.min(line_count);
                            let excerpt = line_tags::excerpt(
                                &path.to_string_lossy(),
                                &updated,
                                start,
                                Some(start + 7),
                                self.output_bytes.saturating_sub(result.len() + 1),
                            )?;
                            result.push('\n');
                            result.push_str(&excerpt);
                        }
                        *audit = Some(format!("{result}\n\n{patch}"));
                        Ok(result)
                    }
                    (None, Some(old), Some(new)) => {
                        ensure!(
                            args.snapshot_id.is_none(),
                            "snapshot_id applies to tagged edits"
                        );
                        let lengths = (old.len(), new.len());
                        let receipt = edit_receipt(&old, &new);
                        tokio::task::spawn_blocking(move || edit_file(&path, &old, &new)).await??;
                        *audit = Some(receipt);
                        Ok(format!(
                            "Edited {} ({} bytes replaced with {} bytes).",
                            args.path.display(),
                            lengths.0,
                            lengths.1
                        ))
                    }
                    _ => anyhow::bail!("provide edits or both old_text and new_text"),
                }
            }
            "shell" if matches!(self.access, Access::Work) => {
                let args: Shell = crate::api::tool_arguments(arguments)?;
                ensure!(!args.command.trim().is_empty(), "shell command is empty");
                let (_, output, full) = self.command_result(&mut shell(&args.command)).await?;
                *audit = Some(full);
                Ok(format!("Untrusted command output\n{output}"))
            }
            name => anyhow::bail!("tool {name} is unavailable in this role"),
        }
    }

    async fn resolve(&self, path: &Path) -> Result<PathBuf> {
        let resolved = fs::canonicalize(self.workspace.join(path))
            .await
            .with_context(|| format!("resolve {}", path.display()))?;
        ensure!(
            resolved.starts_with(self.workspace),
            "path leaves the workspace"
        );
        ensure!(
            !resolved.starts_with(self.workspace.join(".ratio")),
            "path is reserved for Ratio checkpoints"
        );
        Ok(resolved)
    }

    async fn edit_path(&self, path: &Path) -> Result<PathBuf> {
        let name = path.file_name().context("expected a file path")?;
        let parent = self
            .resolve(path.parent().context("expected a parent directory")?)
            .await?;
        let target = parent.join(name);
        ensure!(
            !target.starts_with(self.workspace.join(".ratio")),
            "path is reserved for Ratio checkpoints"
        );
        ensure!(
            !target
                .strip_prefix(self.workspace)?
                .components()
                .any(|part| part.as_os_str() == ".git"),
            "path is reserved for Git metadata"
        );
        if let Some(ownership) = self.ownership {
            let relative = target.strip_prefix(self.workspace)?;
            ensure!(
                ownership.iter().any(|entry| entry.includes(relative)
                    && (!entry.directory || entry.path != relative)),
                "write path is outside this worker's frozen ownership: {}",
                relative.display()
            );
        }
        Ok(target)
    }

    async fn read(&self, args: &ReadFile, audit: &mut Option<String>) -> Result<String> {
        use std::io::SeekFrom;
        use tokio::io::AsyncSeekExt;

        let path = self.resolve(&args.path).await?;
        let metadata = fs::metadata(&path).await?;
        ensure!(metadata.is_file(), "expected a regular file");
        let limit = args
            .max_bytes
            .unwrap_or(self.output_bytes)
            .min(self.output_bytes);
        ensure!(limit >= 128, "max_bytes must be at least 128");
        let Some(offset) = args.offset else {
            let start = args.start_line.unwrap_or(1);
            let end = args.end_line;
            let output_limit = self.output_bytes;
            let audit_limit = args.max_bytes.unwrap_or(AUDIT_LIMIT).min(AUDIT_LIMIT);
            let (visible, full) = tokio::task::spawn_blocking(move || {
                let text = read_edit_file(&path)?;
                let header = format!(
                    "Untrusted workspace file\nSnapshot ID: {}\n",
                    snapshot_id(&text)
                );
                let excerpt = line_tags::excerpt(
                    &path.to_string_lossy(),
                    &text,
                    start,
                    end,
                    limit.min(output_limit.saturating_sub(header.len())),
                )?;
                let full = line_tags::excerpt(
                    &path.to_string_lossy(),
                    &text,
                    start,
                    end,
                    audit_limit.saturating_sub(header.len()),
                )?;
                Ok::<_, anyhow::Error>((header.clone() + &excerpt, header + &full))
            })
            .await??;
            *audit = Some(full);
            return Ok(visible);
        };
        ensure!(
            args.start_line.is_none() && args.end_line.is_none(),
            "offset cannot be combined with a line range"
        );
        ensure!(offset <= metadata.len(), "offset is past end of file");
        let mut file = fs::File::open(path).await?;
        file.seek(SeekFrom::Start(offset)).await?;
        let mut bytes = Vec::new();
        let visible_limit = (limit.saturating_sub(128) / 3).max(4);
        let audit_limit = args
            .max_bytes
            .unwrap_or(AUDIT_LIMIT)
            .min(AUDIT_LIMIT)
            .max(visible_limit);
        file.take(audit_limit as u64)
            .read_to_end(&mut bytes)
            .await?;
        let visible = &bytes[..bytes.len().min(visible_limit)];
        let text = match std::str::from_utf8(visible) {
            Ok(text) => text,
            Err(error)
                if error.error_len().is_none()
                    && offset + (visible.len() as u64) < metadata.len() =>
            {
                // Carry an incomplete trailing code point into the next range.
                std::str::from_utf8(&visible[..error.valid_up_to()])?
            }
            Err(error) => return Err(error).context("read UTF-8 text at the requested offset"),
        };
        let next = offset + text.len() as u64;
        let full = std::str::from_utf8(&bytes).map_or_else(
            |error| std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default(),
            |text| text,
        );
        *audit = Some(format!(
            "Untrusted workspace file. Bytes {offset}..{} of {}.\n{full}",
            offset + full.len() as u64,
            metadata.len()
        ));
        Ok(format!(
            "Untrusted workspace file. Bytes {}..{next} of {}.\n{text}",
            offset,
            metadata.len()
        ))
    }

    async fn list(&self, path: &Path) -> Result<(String, String)> {
        const OMITTED: &str = "[... additional entries omitted ...]";
        const AUDIT_DIRECTORY_BYTES: usize = 1024 * 1024;
        let mut dir = fs::read_dir(self.resolve(path).await?).await?;
        let mut lines = Vec::new();
        let mut size = 0;
        let mut visible_count = None;
        let mut omitted = false;
        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name();
            let mut name = name.to_string_lossy().escape_debug().to_string();
            if entry.file_type().await?.is_dir() {
                name.push('/');
            }
            size += name.len() + 1;
            if size > self.output_bytes.saturating_sub(64) && visible_count.is_none() {
                visible_count = Some(lines.len());
            }
            if size > AUDIT_DIRECTORY_BYTES {
                omitted = true;
                break;
            }
            lines.push(name);
        }
        let mut visible = lines[..visible_count.unwrap_or(lines.len())].to_vec();
        if visible_count.is_some() || omitted {
            visible.push(OMITTED.into());
        }
        visible.sort_unstable();
        if omitted {
            lines.push(OMITTED.into());
        }
        lines.sort_unstable();
        Ok((visible.join("\n"), lines.join("\n")))
    }

    #[cfg(all(test, windows))]
    async fn run(&self, command: &mut Command) -> Result<String> {
        Ok(self.command_result(command).await?.1)
    }

    pub(crate) async fn verify(&self, script: &str) -> Result<(bool, String)> {
        let (passed, output, _) = self.command_result(&mut shell(script)).await?;
        Ok((passed, self.redact(output)))
    }

    async fn command_result(&self, command: &mut Command) -> Result<(bool, String, String)> {
        command
            .current_dir(self.workspace)
            .env_remove("OPENAI_API_KEY")
            .env_remove("RATIO_API_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        #[cfg(windows)]
        let job = windows_job::Job::prepare(command)?;
        let mut child = command.spawn().context("start command")?;
        #[cfg(unix)]
        let mut guard = ProcessGroup(child.id());
        #[cfg(windows)]
        job.start(&child)?;
        let stdout = child.stdout.take().context("open command stdout")?;
        let stderr = child.stderr.take().context("open command stderr")?;
        let cap = self.output_bytes.saturating_sub(128) / 2;
        let (status, stdout, stderr) = timeout(self.timeout, async {
            tokio::try_join!(child.wait(), capture(stdout, cap), capture(stderr, cap))
        })
        .await
        .context("command timed out")??;
        #[cfg(unix)]
        {
            guard.0 = None;
        }
        #[cfg(windows)]
        job.disarm()?;

        let output = |limit| {
            format!(
                "Status: {status}\nstdout:\n{}\nstderr:\n{}",
                stdout.render(limit),
                stderr.render(limit)
            )
        };
        Ok((
            status.success(),
            output(cap),
            format!(
                "Succeeded: {}\n{}",
                status.success(),
                output(AUDIT_LIMIT / 2 - 256)
            ),
        ))
    }
}

const MAX_EDIT_BYTES: usize = 4 * 1024 * 1024;
const AUDIT_LIMIT: usize = 8 * 1024 * 1024;

fn edit_receipt(before: &str, after: &str) -> String {
    let before = before.lines().collect::<Vec<_>>();
    let after = after.lines().collect::<Vec<_>>();
    let start = before
        .iter()
        .zip(&after)
        .take_while(|(left, right)| left == right)
        .count();
    let end = before[start..]
        .iter()
        .rev()
        .zip(after[start..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let removed = &before[start..before.len() - end];
    let added = &after[start..after.len() - end];
    let mut receipt = format!(
        "@@ -{},{} +{},{} @@\n",
        start + 1,
        removed.len(),
        start + 1,
        added.len()
    );
    for (prefix, lines) in [('-', removed), ('+', added)] {
        for line in lines {
            if receipt.len() + line.len() >= AUDIT_LIMIT.saturating_sub(128) {
                receipt.push_str("[... additional edit lines omitted ...]");
                return receipt;
            }
            receipt.push(prefix);
            receipt.push_str(line);
            receipt.push('\n');
        }
    }
    receipt
}
type WriteLock = std::sync::Mutex<()>;
static NATIVE_WRITES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<WriteLock>>>,
> = std::sync::LazyLock::new(Default::default);

fn write_lock(path: &Path) -> Result<std::sync::Arc<WriteLock>> {
    let mut locks = NATIVE_WRITES
        .lock()
        .map_err(|_| anyhow::anyhow!("native write lock table poisoned"))?;
    if let Some(lock) = locks.get(path).and_then(std::sync::Weak::upgrade) {
        return Ok(lock);
    }
    locks.retain(|_, lock| lock.strong_count() != 0);
    let lock = std::sync::Arc::new(WriteLock::default());
    locks.insert(path.to_owned(), std::sync::Arc::downgrade(&lock));
    Ok(lock)
}

fn snapshot_id(text: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn create_file(path: &Path, content: &str) -> Result<()> {
    let lock = write_lock(path)?;
    let _guard = lock
        .lock()
        .map_err(|_| anyhow::anyhow!("native write lock poisoned"))?;
    ensure!(
        content.len() <= MAX_EDIT_BYTES,
        "file would exceed {MAX_EDIT_BYTES} bytes"
    );
    let builder = &mut tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temp = builder.tempfile_in(path.parent().context("create file parent")?)?;
    temp.write_all(content.as_bytes())?;
    temp.persist_noclobber(path).context("create new file")?;
    Ok(())
}

fn edit_file(path: &Path, old: &str, new: &str) -> Result<()> {
    ensure!(!old.is_empty(), "old_text must be nonempty");
    change_file(path, |original| {
        let start = original
            .find(old)
            .context("old_text does not match the file")?;
        let next = start
            + original[start..]
                .chars()
                .next()
                .context("empty match")?
                .len_utf8();
        ensure!(
            !original[next..].contains(old),
            "old_text matches more than once"
        );
        let mut changed = original.to_owned();
        changed.replace_range(start..start + old.len(), new);
        Ok((changed, 0))
    })?;
    Ok(())
}

fn change_file(
    path: &Path,
    plan: impl FnOnce(&str) -> Result<(String, usize)>,
) -> Result<(String, usize)> {
    let lock = write_lock(path)?;
    let _guard = lock
        .lock()
        .map_err(|_| anyhow::anyhow!("native write lock poisoned"))?;
    let metadata = std::fs::symlink_metadata(path).context("inspect file")?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "writes through a symlink leaf are unavailable; edit the workspace target directly"
    );
    ensure!(metadata.is_file(), "expected a regular file");
    let original = read_edit_file(path)?;
    let (changed, first) = plan(&original)?;
    ensure!(
        changed.len() <= MAX_EDIT_BYTES,
        "edited file would exceed {MAX_EDIT_BYTES} bytes"
    );
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("edit file parent")?)?;
    temp.write_all(changed.as_bytes())?;
    temp.as_file().set_permissions(metadata.permissions())?;
    let current = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.dev() == current.dev() && metadata.ino() == current.ino(),
            "file identity changed during edit"
        );
    }
    ensure!(
        current.is_file() && read_edit_file(path)? == original,
        "file changed during the edit. Read it again before retrying"
    );
    temp.persist(path).context("replace edited file")?;
    Ok((changed, first))
}

fn read_edit_file(path: &Path) -> Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX_EDIT_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .context("read editable UTF-8 file")?;
    ensure!(
        text.len() <= MAX_EDIT_BYTES,
        "file exceeds {MAX_EDIT_BYTES} bytes"
    );
    Ok(text)
}

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

#[cfg(unix)]
fn shell(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]);
    command
}

#[cfg(windows)]
fn shell(script: &str) -> Command {
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C"])
        .raw_arg(format!("\"{script}\""));
    command
}

#[cfg(unix)]
struct ProcessGroup(Option<u32>);

#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // The child starts a new process group whose ID is its PID.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

struct CapturedStream {
    bytes: Vec<u8>,
    total: usize,
}

impl CapturedStream {
    fn render(&self, cap: usize) -> String {
        let keep = self.bytes.len().min(cap);
        let mut result = String::from_utf8_lossy(&self.bytes[..keep]).into_owned();
        if self.total > keep {
            result.push_str(&format!("\n[{} bytes omitted]", self.total - keep));
        }
        result
    }
}

async fn capture(
    mut stream: impl AsyncRead + Unpin,
    _cap: usize,
) -> std::io::Result<CapturedStream> {
    let mut buffer = [0; 8192];
    let mut result = Vec::new();
    let mut total = 0usize;
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(AUDIT_LIMIT / 2 - result.len());
        result.extend_from_slice(&buffer[..keep]);
        total = total.saturating_add(count);
    }
    Ok(CapturedStream {
        bytes: result,
        total,
    })
}

fn schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
        },
    })
}

#[derive(Deserialize)]
struct ReadFile {
    path: PathBuf,
    offset: Option<u64>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct CreateFile {
    path: PathBuf,
    content: String,
}

#[derive(Deserialize)]
struct EditFile {
    path: PathBuf,
    edits: Option<BTreeMap<String, Option<String>>>,
    snapshot_id: Option<String>,
    #[serde(default)]
    line_hints: BTreeMap<String, usize>,
    #[serde(alias = "old_string")]
    old_text: Option<String>,
    #[serde(alias = "new_string")]
    new_text: Option<String>,
}

#[derive(Deserialize)]
struct PathArg {
    path: PathBuf,
}

#[derive(Deserialize)]
struct Empty {}

#[derive(Deserialize)]
struct Diff {
    #[serde(default)]
    staged: bool,
}

#[derive(Deserialize)]
struct Shell {
    command: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn command_timeout_terminates_windows_descendants() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let tools = Tools {
            ownership: None,
            workspace: &workspace,
            access: Access::Work,
            timeout: Duration::from_secs(3),
            output_bytes: 1024,
            key: None,
        };
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command", r#"
            Start-Process -FilePath powershell.exe -ArgumentList '-NoProfile -Command "New-Item -Name descendant-started -ItemType File; Start-Sleep -Seconds 5; New-Item -Name descendant-escaped -ItemType File"' -NoNewWindow
            Start-Sleep -Seconds 30
        "#]);
        let error = tools.run(&mut command).await.unwrap_err();
        assert!(error.to_string().contains("command timed out"), "{error:#}");
        assert!(workspace.join("descendant-started").exists());
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(!workspace.join("descendant-escaped").exists());
    }

    #[tokio::test]
    async fn read_ranges_keep_split_utf8_code_points() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        std::fs::write(workspace.join("text"), format!("{}é data", "a".repeat(127))).unwrap();
        let tools = Tools {
            ownership: None,
            workspace: &workspace,
            access: Access::Inspect,
            timeout: Duration::from_secs(1),
            output_bytes: 512,
            key: None,
        };
        let mut args = ReadFile {
            path: "text".into(),
            offset: Some(0),
            start_line: None,
            end_line: None,
            max_bytes: None,
        };
        assert!(
            tools
                .read(&args, &mut None)
                .await
                .unwrap()
                .contains("Bytes 0..127 of")
        );
        args.offset = Some(127);
        assert!(
            tools
                .read(&args, &mut None)
                .await
                .unwrap()
                .ends_with("é data")
        );
    }

    #[tokio::test]
    async fn rejects_stale_range_snapshot_after_interior_changes() {
        use crate::api::Function;
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let path = workspace.join("fixture");
        let original = "first\nmiddle\nlast\n";
        create_file(&path, original).unwrap();
        let anchors = line_tags::tags(
            &path.to_string_lossy(),
            &line_tags::lines(original).collect::<Vec<_>>(),
        );
        edit_file(&path, "middle", "new interior").unwrap();
        let tools = Tools {
            workspace: &workspace,
            ownership: None,
            access: Access::Work,
            timeout: Duration::from_secs(1),
            output_bytes: 4096,
            key: None,
        };
        let call = ToolCall { id: "range".into(), kind: "function".into(), function: Function {
            name: "edit_file".into(),
            arguments: json!({"path":"fixture","snapshot_id":snapshot_id(original),"edits":{format!("{}..{}", anchors[0], anchors[2]):"replacement"}}).to_string(),
        }};
        let result = tools.execute(&call).await;
        assert!(result.contains("current snapshot_id"), "{result}");
        assert_eq!(
            read_edit_file(&path).unwrap(),
            "first\nnew interior\nlast\n"
        );
    }

    #[test]
    fn edit_requires_one_exact_match_even_when_matches_overlap() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("text");
        create_file(&path, "aaa\nunique\n").unwrap();
        assert!(
            edit_file(&path, "aa", "b")
                .unwrap_err()
                .to_string()
                .contains("more than once")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa\nunique\n");
        edit_file(&path, "unique", "é").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa\né\n");
    }

    #[test]
    fn native_edits_preserve_independent_changes_to_one_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture");
        create_file(&path, "first\nsecond\n").unwrap();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|threads| {
            for (old, new) in [("first", "one"), ("second", "two")] {
                let path = &path;
                let start = &start;
                threads.spawn(move || {
                    start.wait();
                    edit_file(path, old, new).unwrap();
                });
            }
        });
        assert_eq!(read_edit_file(&path).unwrap(), "one\ntwo\n");
    }

    #[cfg(unix)]
    #[test]
    fn native_edit_requires_addressing_a_symlink_target_directly() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("target");
        let link = root.path().join("link");
        create_file(&path, "original").unwrap();
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(read_edit_file(&link).unwrap(), "original");
        assert!(
            edit_file(&link, "original", "changed")
                .unwrap_err()
                .to_string()
                .contains("symlink leaf")
        );
        assert_eq!(read_edit_file(&path).unwrap(), "original");
    }
}
