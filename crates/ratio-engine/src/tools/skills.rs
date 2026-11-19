use std::{
    collections::BTreeMap,
    io::SeekFrom,
    path::{Component, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::{api::ToolCall, config::Skill, context::clip};

#[derive(Deserialize)]
struct Request {
    name: Option<String>,
    path: Option<PathBuf>,
    #[serde(default)]
    offset: u64,
}

pub(crate) fn schema(skills: &BTreeMap<String, Skill>) -> Option<Value> {
    if skills.is_empty() {
        return None;
    }
    Some(super::schema(
        "read_skill",
        "List configured skills without a name. With a name, read SKILL.md or a relative reference.",
        json!({
            "name":{"type":"string","enum":skills.keys().collect::<Vec<_>>()},
            "path":{"type":"string"},
            "offset":{"type":"integer","minimum":0}
        }),
        &[],
    ))
}

pub(crate) async fn dispatch(
    skills: &BTreeMap<String, Skill>,
    call: &ToolCall,
    cap: usize,
    key: Option<&str>,
) -> String {
    let result = match read(skills, call, cap).await {
        Ok(text) => text,
        Err(error) => format!("Tool error: {error:#}"),
    };
    let result = match key {
        Some(key) => result.replace(key, "[redacted]"),
        None => result,
    };
    clip(&result, cap)
}

async fn read(skills: &BTreeMap<String, Skill>, call: &ToolCall, cap: usize) -> Result<String> {
    ensure!(call.kind == "function", "unsupported tool call type");
    let input: Request = crate::api::tool_arguments(&call.function.arguments)?;
    let Some(name) = &input.name else {
        ensure!(
            input.path.is_none() && input.offset == 0,
            "name is required for reading a specific skill file"
        );
        ensure!(!skills.is_empty(), "no skills configured");
        return Ok(skills
            .iter()
            .map(|(name, skill)| format!("{name}: {}", skill.description))
            .collect::<Vec<_>>()
            .join("\n"));
    };
    let skill = skills.get(name).context("unknown skill")?;
    let relative = input.path.unwrap_or_else(|| PathBuf::from("SKILL.md"));
    ensure!(
        !relative.as_os_str().is_empty()
            && relative.components().all(
                |part| matches!(part, Component::Normal(name) if name != ".git" && name != ".ratio")
            ),
        "skill file must be a relative path within its bundle"
    );
    let path = tokio::fs::canonicalize(skill.directory.join(&relative)).await?;
    ensure!(
        path.starts_with(&skill.directory),
        "skill path leaves the bundle"
    );
    let metadata = tokio::fs::metadata(&path).await?;
    ensure!(metadata.is_file(), "skill path must be a regular file");
    ensure!(
        input.offset <= metadata.len(),
        "skill offset exceeds file size"
    );
    let mut file = tokio::fs::File::open(&path).await?;
    file.seek(SeekFrom::Start(input.offset)).await?;
    let mut bytes = Vec::new();
    file.take(cap.saturating_sub(384).max(128) as u64)
        .read_to_end(&mut bytes)
        .await?;
    let valid = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error)
            if error.error_len().is_none()
                && input.offset + (bytes.len() as u64) < metadata.len() =>
        {
            error.valid_up_to()
        }
        Err(error) => {
            return Err(error).context("skill content must be UTF-8 at the requested offset");
        }
    };
    let next = input.offset + valid as u64;
    let label = if next < metadata.len() {
        format!("next offset {next}")
    } else {
        "end of file".into()
    };
    Ok(format!(
        "Operator-configured skill {} / {} ({label})\n{}",
        name,
        clip(&relative.display().to_string(), 128),
        std::str::from_utf8(&bytes[..valid])?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Function;

    #[tokio::test]
    async fn reads_bounded_skill_content_and_refuses_traversal() {
        let root = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(directory.join("SKILL.md"), "λ".repeat(900)).unwrap();
        let skills = BTreeMap::from([(
            "fixture".into(),
            Skill {
                directory,
                description: "fixture".into(),
            },
        )]);
        let call = |arguments: Value| ToolCall {
            id: "fixture".into(),
            kind: "function".into(),
            function: Function {
                name: "read_skill".into(),
                arguments: arguments.to_string(),
            },
        };
        let page = read(&skills, &call(json!({"name":"fixture"})), 513)
            .await
            .unwrap();
        assert!(page.contains("next offset 128"));
        let escaped = read(
            &skills,
            &call(json!({"name":"fixture", "path":"../secret"})),
            513,
        )
        .await;
        assert!(escaped.is_err());
    }
}
