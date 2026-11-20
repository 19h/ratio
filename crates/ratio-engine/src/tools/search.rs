//! Read-only workspace search: content search and file lookup.
//!
//! Git workspaces are listed through `git ls-files`, so ignore rules apply.
//! Other workspaces are walked directly. `.git` and `.ratio` are never read.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{read_text_snapshot, schema};
use crate::{context::clip, line_tags};

const MAX_PATTERN_BYTES: usize = 1024;
const MAX_LINE_BYTES: usize = 400;
const MAX_WALK_ENTRIES: usize = 200_000;
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
pub(super) struct SearchFiles {
    pattern: String,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    pub(super) path: Option<PathBuf>,
    glob: Option<String>,
    context: Option<usize>,
    max_results: Option<usize>,
    after: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct FindFiles {
    pattern: String,
    #[serde(default)]
    pub(super) path: Option<PathBuf>,
    max_results: Option<usize>,
    after: Option<String>,
}

pub(super) fn schemas() -> [Value; 2] {
    [
        schema(
            "search_files",
            "Regex search of file lines. Skips Git-ignored files. Rows use read_file tags.",
            json!({
                "pattern": {"type":"string"},
                "literal": {"type":"boolean"},
                "ignore_case": {"type":"boolean"},
                "path": {"type":"string"},
                "glob": {"type":"string"},
                "context": {"type":"integer","minimum":0,"maximum":5},
                "max_results": {"type":"integer","minimum":1,"maximum":200},
                "after": {"type":"string"}
            }),
            &["pattern"],
        ),
        schema(
            "find_files",
            "List files by glob, e.g. src/**/*.rs. Skips Git-ignored files.",
            json!({
                "pattern": {"type":"string"},
                "path": {"type":"string"},
                "max_results": {"type":"integer","minimum":1,"maximum":1000},
                "after": {"type":"string"}
            }),
            &["pattern"],
        ),
    ]
}

/// Search scope, resolved by the caller to a location inside the workspace.
pub(super) struct Scope {
    pub workspace: PathBuf,
    /// Workspace-relative directory or file. Empty selects the whole workspace.
    pub within: PathBuf,
    pub output_bytes: usize,
    pub timeout: Duration,
}

fn display(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn reserved(path: &Path) -> bool {
    path.components()
        .any(|part| matches!(part, Component::Normal(name) if name == ".git" || name == ".ratio"))
}

/// Translate a glob into an anchored regex. A pattern without a separator
/// matches the file name at any depth.
fn glob(pattern: &str) -> Result<Regex> {
    ensure!(
        !pattern.is_empty() && pattern.len() <= MAX_PATTERN_BYTES,
        "glob must be 1 to {MAX_PATTERN_BYTES} bytes"
    );
    let pattern = pattern.replace('\\', "/");
    let pattern = pattern.trim_start_matches("./");
    let mut regex = String::from(if pattern.contains('/') {
        "^"
    } else {
        "^(?:.*/)?"
    });
    let mut chars = pattern.chars().peekable();
    let mut group = 0usize;
    while let Some(next) = chars.next() {
        match next {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if chars.peek() == Some(&'/') {
                    chars.next();
                    regex.push_str("(?:.*/)?");
                } else {
                    regex.push_str(".*");
                }
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '{' => {
                group += 1;
                regex.push_str("(?:");
            }
            '}' if group > 0 => {
                group -= 1;
                regex.push(')');
            }
            ',' if group > 0 => regex.push('|'),
            other => regex.push_str(&regex::escape(other.encode_utf8(&mut [0; 4]))),
        }
    }
    ensure!(group == 0, "glob has an unclosed {{");
    regex.push('$');
    Regex::new(&regex).context("invalid glob")
}

/// Sorted workspace-relative regular files under the scope.
fn files(scope: &Scope) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    let spec = if scope.within.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        display(&scope.within)
    };
    match crate::staging::git(
        &scope.workspace,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            &spec,
        ],
    ) {
        Ok(listing) => paths.extend(
            listing
                .split(|byte| *byte == 0)
                .filter_map(|entry| std::str::from_utf8(entry).ok())
                .filter(|entry| !entry.is_empty())
                .map(PathBuf::from),
        ),
        // Not a Git workspace, or Git is unavailable.
        Err(_) => walk(&scope.workspace, &scope.within, &mut paths)?,
    }
    Ok(paths
        .into_iter()
        .filter(|path| {
            !reserved(path)
                // A symlink could lead outside the workspace.
                && std::fs::symlink_metadata(scope.workspace.join(path))
                    .is_ok_and(|entry| entry.is_file())
        })
        .collect())
}

fn walk(workspace: &Path, within: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    let mut pending = vec![within.to_owned()];
    let mut visited = 0usize;
    while let Some(relative) = pending.pop() {
        let location = workspace.join(&relative);
        if std::fs::symlink_metadata(&location).is_ok_and(|entry| entry.is_file()) {
            paths.insert(relative);
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&location) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            ensure!(
                visited <= MAX_WALK_ENTRIES,
                "workspace has more than {MAX_WALK_ENTRIES} entries. Narrow the search with path"
            );
            let child = relative.join(entry.file_name());
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && !reserved(&child) {
                pending.push(child);
            } else if kind.is_file() {
                paths.insert(child);
            }
        }
    }
    Ok(())
}

fn limit(requested: Option<usize>, default: usize, maximum: usize) -> usize {
    requested.unwrap_or(default).clamp(1, maximum)
}

pub(super) fn find(scope: &Scope, args: &FindFiles) -> Result<String> {
    let matcher = glob(&args.pattern)?;
    let limit = limit(args.max_results, 200, 1000);
    let budget = scope.output_bytes.saturating_sub(200);
    let mut output = String::from("Untrusted file list\n");
    let mut shown = 0usize;
    let mut last = None;
    let mut more = false;
    for path in files(scope)? {
        let name = display(&path);
        // Cursors compare as paths, the order the listing is sorted in.
        if args
            .after
            .as_deref()
            .is_some_and(|after| path.as_path() <= Path::new(after))
            || !matcher.is_match(&name)
        {
            continue;
        }
        if shown == limit || output.len() + name.len() + 1 > budget {
            more = true;
            break;
        }
        output.push_str(&name);
        output.push('\n');
        shown += 1;
        last = Some(name);
    }
    if shown == 0 {
        output.push_str("No matching files.");
    } else if let Some(last) = last.filter(|_| more) {
        let _ = write!(output, "More results: after={last:?}.");
    }
    Ok(output)
}

/// Returns the model-visible result and the audit result.
pub(super) fn search(scope: &Scope, args: &SearchFiles) -> Result<(String, String)> {
    ensure!(
        !args.pattern.is_empty() && args.pattern.len() <= MAX_PATTERN_BYTES,
        "pattern must be 1 to {MAX_PATTERN_BYTES} bytes"
    );
    let pattern = if args.literal {
        regex::escape(&args.pattern)
    } else {
        args.pattern.clone()
    };
    let matcher = RegexBuilder::new(&pattern)
        .case_insensitive(args.ignore_case)
        .size_limit(1 << 20)
        .build()
        .context("invalid search pattern")?;
    let filter = args.glob.as_deref().map(glob).transpose()?;
    let context = args.context.unwrap_or(0).min(5);
    let limit = limit(args.max_results, 50, 200);
    let budget = scope.output_bytes.saturating_sub(200);
    let after = match args.after.as_deref() {
        Some(cursor) => {
            let (path, line) = cursor
                .rsplit_once(':')
                .context("after must be \"path:line\" from an earlier result")?;
            Some((
                path.to_owned(),
                line.parse::<usize>().context("after has no line number")?,
            ))
        }
        None => None,
    };
    let started = Instant::now();
    let deadline = scope.timeout.min(DEADLINE);
    let mut output = String::from("Untrusted search results\n");
    let (mut matches, mut searched, mut skipped) = (0usize, 0usize, 0usize);
    let mut cursor = None;
    let mut more = false;
    'files: for path in files(scope)? {
        let name = display(&path);
        // Cursors compare as paths, the order the listing is sorted in.
        if after
            .as_ref()
            .is_some_and(|(file, _)| path.as_path() < Path::new(file))
            || filter.as_ref().is_some_and(|glob| !glob.is_match(&name))
        {
            continue;
        }
        if started.elapsed() >= deadline {
            more = true;
            break;
        }
        // Tags are seeded with the canonical path, exactly as read_file does.
        let Ok(location) = scope.workspace.join(&path).canonicalize() else {
            continue;
        };
        if !location.starts_with(&scope.workspace) {
            continue;
        }
        let text = match read_text_snapshot(&location, true) {
            Ok(Some(text)) => text,
            _ => {
                skipped += 1;
                continue;
            }
        };
        searched += 1;
        let resume = after
            .as_ref()
            .filter(|(file, _)| path.as_path() == Path::new(file))
            .map_or(0, |(_, line)| *line);
        let lines = line_tags::lines(&text).collect::<Vec<_>>();
        let hits = lines
            .iter()
            .enumerate()
            .filter(|(index, line)| *index + 1 > resume && matcher.is_match(line))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if hits.is_empty() {
            continue;
        }
        let tags = line_tags::tags(&location.to_string_lossy(), &lines);
        let mut block = format!("{name}\n");
        let mut printed = None::<usize>;
        for hit in hits {
            let first = hit
                .saturating_sub(context)
                .max(printed.map_or(0, |last| last + 1));
            let last = (hit + context).min(lines.len() - 1);
            let mut rows = String::new();
            if printed.is_some_and(|previous| first > previous + 1) {
                rows.push_str("--\n");
            }
            for index in first..=last {
                let line = lines[index].strip_suffix('\r').unwrap_or(lines[index]);
                let _ = writeln!(
                    rows,
                    "{} {}|{}",
                    index + 1,
                    tags[index],
                    clip(line, MAX_LINE_BYTES)
                );
            }
            if matches == limit || output.len() + block.len() + rows.len() > budget {
                more = true;
                if printed.is_some() {
                    output.push_str(&block);
                }
                break 'files;
            }
            block.push_str(&rows);
            printed = Some(last.max(printed.unwrap_or(0)));
            matches += 1;
            cursor = Some(format!("{name}:{}", hit + 1));
        }
        output.push_str(&block);
    }
    if matches == 0 {
        output.push_str("No matches.");
    }
    if let Some(cursor) = cursor.filter(|_| more) {
        let _ = write!(output, "More results: after={cursor:?}.");
    }
    if skipped > 0 {
        let _ = write!(output, "\nSkipped {skipped} binary or oversized files.");
    }
    let audit = format!(
        "{output}\n{matches} matches in {searched} files, {} ms.",
        started.elapsed().as_millis()
    );
    Ok((output, audit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(root: &Path, within: &str, output_bytes: usize) -> Scope {
        Scope {
            workspace: root.canonicalize().unwrap(),
            within: within.into(),
            output_bytes,
            timeout: Duration::from_secs(5),
        }
    }

    fn search_args(value: Value) -> SearchFiles {
        serde_json::from_value(value).unwrap()
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "src/lib.rs",
            "fn alpha() {}\nfn beta() {}\nfn alpha_two() {}\n",
        );
        write("src/deep/mod.rs", "// alpha in a comment\n");
        write("docs/guide.md", "Alpha is documented here.\n");
        write("data.bin", "al\0pha");
        write(".ratio/runs.txt", "alpha in the journal\n");
        write(".git/config", "alpha in git metadata\n");
        root
    }

    #[test]
    fn globs_match_names_at_any_depth_and_paths_when_anchored() {
        for (pattern, path, expected) in [
            ("*.rs", "src/deep/mod.rs", true),
            ("*.rs", "src/lib.rsx", false),
            ("src/*.rs", "src/lib.rs", true),
            ("src/*.rs", "src/deep/mod.rs", false),
            ("src/**/*.rs", "src/deep/mod.rs", true),
            ("src/**/*.rs", "src/lib.rs", true),
            ("**/*.{md,txt}", "docs/guide.md", true),
            ("*.{md,txt}", "notes.txt", true),
            ("lib.?s", "src/lib.rs", true),
            ("a+b.rs", "a+b.rs", true),
            ("a+b.rs", "aab.rs", false),
        ] {
            assert_eq!(
                glob(pattern).unwrap().is_match(path),
                expected,
                "{pattern} {path}"
            );
        }
        assert!(glob("{a,b").is_err() && glob("").is_err());
    }

    #[test]
    fn search_skips_reserved_and_binary_files_and_tags_rows_like_read_file() {
        let root = fixture();
        let scope = scope(root.path(), "", 4096);
        let (visible, audit) = search(&scope, &search_args(json!({"pattern":"alpha"}))).unwrap();
        assert!(
            !visible.contains(".ratio")
                && !visible.contains(".git")
                && !visible.contains("data.bin")
        );
        assert!(
            visible.contains("Skipped 1 binary or oversized files."),
            "{visible}"
        );
        assert!(audit.contains("3 matches in 3 files"), "{audit}");
        let location = scope.workspace.join("src/lib.rs");
        let text = std::fs::read_to_string(&location).unwrap();
        let expected =
            line_tags::excerpt(&location.to_string_lossy(), &text, 3, Some(3), 4096).unwrap();
        assert!(
            visible.contains(expected.trim_end()),
            "{visible}\n{expected}"
        );
        // Path order is stable, and the documentation match needs ignore_case.
        let files = visible
            .lines()
            .filter(|line| !line.contains('|'))
            .collect::<Vec<_>>();
        assert_eq!(files[1..3], ["src/deep/mod.rs", "src/lib.rs"]);
        let (visible, _) = search(
            &scope,
            &search_args(json!({"pattern":"ALPHA","ignore_case":true,"glob":"*.md"})),
        )
        .unwrap();
        assert!(visible.contains("docs/guide.md\n1 ") && !visible.contains("src/"));
    }

    #[test]
    fn search_supports_literals_context_scope_and_cursors() {
        let root = fixture();
        let whole = scope(root.path(), "", 4096);
        let (visible, _) = search(
            &whole,
            &search_args(json!({"pattern":"alpha()","literal":true,"context":1})),
        )
        .unwrap();
        assert!(
            visible.contains("1 ") && visible.contains("|fn beta() {}"),
            "{visible}"
        );
        assert!(!visible.contains("alpha_two"));
        let (first, _) = search(
            &whole,
            &search_args(json!({"pattern":"alpha","max_results":2})),
        )
        .unwrap();
        assert!(
            first.contains("fn alpha() {}\nMore results: after=\"src/lib.rs:1\".\n"),
            "{first}"
        );
        let (rest, _) = search(
            &whole,
            &search_args(json!({"pattern":"alpha","after":"src/lib.rs:1"})),
        )
        .unwrap();
        assert!(
            rest.contains("|fn alpha_two() {}") && !rest.contains("|fn alpha() {}"),
            "{rest}"
        );
        assert!(!rest.contains("More results"));
        let nested = scope(root.path(), "src/deep", 4096);
        let (visible, _) = search(&nested, &search_args(json!({"pattern":"alpha"}))).unwrap();
        assert!(visible.contains("src/deep/mod.rs") && !visible.contains("src/lib.rs"));
        let (none, _) = search(&whole, &search_args(json!({"pattern":"absent"}))).unwrap();
        assert!(none.starts_with("Untrusted search results\nNo matches."));
        assert!(search(&whole, &search_args(json!({"pattern":"("}))).is_err());
    }

    #[test]
    fn find_lists_matches_in_order_and_continues_from_a_cursor() {
        let root = fixture();
        let scope = scope(root.path(), "", 4096);
        let args = |value: Value| serde_json::from_value::<FindFiles>(value).unwrap();
        let all = find(&scope, &args(json!({"pattern":"**/*"}))).unwrap();
        assert_eq!(
            all.lines().skip(1).collect::<Vec<_>>(),
            ["data.bin", "docs/guide.md", "src/deep/mod.rs", "src/lib.rs"]
        );
        let first = find(&scope, &args(json!({"pattern":"*.rs","max_results":1}))).unwrap();
        assert!(
            first.ends_with("src/deep/mod.rs\nMore results: after=\"src/deep/mod.rs\"."),
            "{first}"
        );
        let rest = find(
            &scope,
            &args(json!({"pattern":"*.rs","after":"src/deep/mod.rs"})),
        )
        .unwrap();
        assert_eq!(rest, "Untrusted file list\nsrc/lib.rs\n");
        assert!(
            find(&scope, &args(json!({"pattern":"*.py"})))
                .unwrap()
                .ends_with("No matching files.")
        );
    }

    #[test]
    fn git_workspaces_honor_ignore_rules() {
        let root = fixture();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .current_dir(root.path())
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        // Replace the fixture's stand-in metadata directory with a real repository.
        std::fs::remove_dir_all(root.path().join(".git")).unwrap();
        git(&["init", "--quiet"]);
        std::fs::write(root.path().join(".gitignore"), "docs/\n").unwrap();
        let scope = scope(root.path(), "", 4096);
        let (visible, _) = search(
            &scope,
            &search_args(json!({"pattern":"lpha","ignore_case":true})),
        )
        .unwrap();
        assert!(
            visible.contains("src/lib.rs") && !visible.contains("docs/guide.md"),
            "{visible}"
        );
        assert!(!visible.contains(".git/") && !visible.contains("journal"));
    }
}
