use std::path::{Component, Path};

use crate::context::clip;

const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Prompt budget for project instruction files, in bytes.
pub(crate) fn limit(context_window: usize) -> usize {
    (context_window / 8).clamp(512, 8192)
}

/// System-prompt section for the loaded instruction text.
pub(crate) fn prompt(text: &str) -> String {
    format!(
        "\nProject instructions from repository files. Follow them where they agree with the goal and constraints:\n{text}"
    )
}

pub(crate) fn valid_name(name: &str) -> bool {
    let path = Path::new(name);
    !name.trim().is_empty()
        && name.len() <= 256
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// Read the configured instruction files from the workspace root, in order,
/// until the byte budget is spent. Files that resolve outside the workspace,
/// are not UTF-8, or repeat earlier content are skipped.
pub(crate) fn load(workspace: &Path, files: &[String], limit: usize) -> Option<String> {
    let root = workspace.canonicalize().ok()?;
    let mut seen = Vec::new();
    let mut result = String::new();
    for name in files {
        let header = format!("--- {name} ---\n");
        let Some(room) = limit
            .checked_sub(result.len() + header.len() + 1)
            .filter(|room| *room >= 64)
        else {
            break;
        };
        let Ok(path) = root.join(name).canonicalize() else {
            continue;
        };
        if !path.starts_with(&root)
            || !std::fs::metadata(&path)
                .is_ok_and(|file| file.is_file() && file.len() <= MAX_FILE_BYTES)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() || seen.iter().any(|earlier| earlier == text) {
            continue;
        }
        result.push_str(&header);
        result.push_str(&clip(text, room));
        result.push('\n');
        seen.push(text.to_owned());
    }
    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::{limit, load, valid_name};

    #[test]
    fn loads_in_order_within_budget_and_skips_duplicates() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "Run the linter.\n").unwrap();
        std::fs::write(root.path().join("CLAUDE.md"), "Run the linter.").unwrap();
        std::fs::write(root.path().join("CONTRIBUTING.md"), "x".repeat(4000)).unwrap();
        std::fs::write(root.path().join("binary.md"), [0xff, 0xfe, 0x00]).unwrap();
        let files = [
            "missing.md",
            "AGENTS.md",
            "CLAUDE.md",
            "binary.md",
            "CONTRIBUTING.md",
        ]
        .map(String::from);
        let text = load(root.path(), &files, 512).unwrap();
        assert!(text.starts_with("--- AGENTS.md ---\nRun the linter.\n--- CONTRIBUTING.md ---\n"));
        assert!(!text.contains("CLAUDE.md") && !text.contains("binary.md"));
        assert!(text.len() <= 512 && text.contains("excerpt shortened"));
        assert_eq!(load(root.path(), &["missing.md".into()], 512), None);
    }

    #[cfg(unix)]
    #[test]
    fn skips_files_that_resolve_outside_the_workspace() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "outside").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.md"),
            root.path().join("AGENTS.md"),
        )
        .unwrap();
        assert_eq!(load(root.path(), &["AGENTS.md".into()], 512), None);
    }

    #[test]
    fn names_stay_inside_the_workspace_and_budget_scales() {
        assert!(valid_name("AGENTS.md") && valid_name("docs/AGENTS.md"));
        for name in ["", "../AGENTS.md", "/etc/passwd", "./AGENTS.md"] {
            assert!(!valid_name(name), "accepted {name:?}");
        }
        assert_eq!(
            (limit(1024), limit(32768), limit(1 << 20)),
            (512, 4096, 8192)
        );
    }
}
