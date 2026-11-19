use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, ensure};

pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_terminator('\n')
}

fn hash(bytes: &[u8], seed: u64) -> u64 {
    bytes.iter().fold(seed, |value, byte| {
        (value ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

pub fn tags(path: &str, lines: &[&str]) -> Vec<String> {
    let seed = hash(path.as_bytes(), 0xcbf29ce484222325);
    let hashes = lines
        .iter()
        .map(|line| hash(line.as_bytes(), seed))
        .collect::<Vec<_>>();
    let mut counts = HashMap::new();
    for value in &hashes {
        *counts.entry(*value).or_insert(0) += 1;
    }
    let mut seen = HashMap::new();
    hashes
        .into_iter()
        .map(|value| {
            let count = counts[&value];
            if count == 1 {
                format!("{value:016x}")
            } else {
                let ordinal = seen.entry(value).or_insert(0);
                *ordinal += 1;
                format!("{value:016x}-{count}-{ordinal}")
            }
        })
        .collect()
}

pub fn excerpt(
    path: &str,
    text: &str,
    start: usize,
    end: Option<usize>,
    limit: usize,
) -> Result<String> {
    ensure!(
        start > 0 && end.is_none_or(|end| end >= start),
        "invalid 1-based line range"
    );
    let lines = lines(text).collect::<Vec<_>>();
    if lines.is_empty() {
        return Ok("Empty file.".into());
    }
    ensure!(start <= lines.len(), "start_line is past end of file");
    let tags = tags(path, &lines);
    let end = end.unwrap_or(lines.len()).min(lines.len());
    let mut output = String::new();
    let mut index = start - 1;
    while index < end {
        let line = lines[index].strip_suffix('\r').unwrap_or(lines[index]);
        let header = format!("{} {}|", index + 1, tags[index]);
        if output.len() + header.len() + line.len() + 1 > limit.saturating_sub(90) {
            if output.is_empty() {
                output.push_str(&header);
                output.push_str(&crate::context::clip(
                    line,
                    limit.saturating_sub(90 + header.len()),
                ));
                output.push('\n');
                index += 1;
            }
            break;
        }
        output.push_str(&header);
        output.push_str(line);
        output.push('\n');
        index += 1;
    }
    if index < end {
        output.push_str(&format!(
            "{} lines omitted. Continue with start_line={}.",
            end - index,
            index + 1
        ));
    }
    Ok(output)
}

pub fn apply(
    path: &str,
    original: &str,
    edits: &BTreeMap<String, Option<String>>,
    line_hints: &BTreeMap<String, usize>,
) -> Result<(String, usize)> {
    ensure!(
        !edits.is_empty(),
        "edits must contain at least one tagged change"
    );
    let lines = lines(original).collect::<Vec<_>>();
    let tags = tags(path, &lines);
    let indices = tags
        .iter()
        .enumerate()
        .map(|(i, tag)| (tag.as_str(), i))
        .collect::<HashMap<_, _>>();
    let index = |tag: &str, key: &str, other: Option<&str>| {
        if let Some(&index) = indices.get(tag) {
            return Ok(index);
        }
        let mut error = format!("stale or unknown line tag {tag:?} in edit {key:?}.");
        let base_tag = tag.split('-').next().unwrap_or(tag);
        let matches = tags
            .iter()
            .enumerate()
            .filter(|(_, current)| current.split('-').next() == Some(base_tag))
            .take(3)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let hint = line_hints.get(tag).or_else(|| line_hints.get(key)).copied();
        let nearby = hint
            .filter(|&line| (1..=lines.len()).contains(&line))
            .map(|line| line - 1)
            .or_else(|| other.and_then(|other| indices.get(other).copied()))
            .or_else(|| (lines.len() == 1).then_some(0));
        if !matches.is_empty() {
            error.push_str(" Current lines with the same content hash (untrusted):");
            for index in matches {
                append_current_line(&mut error, index, &tags, &lines);
            }
        } else if let Some(nearby) = nearby {
            error.push_str(
                " Current lines near the supplied position or surviving anchor (untrusted):",
            );
            for index in nearby.saturating_sub(1)..(nearby + 2).min(lines.len()) {
                append_current_line(&mut error, index, &tags, &lines);
            }
        } else if lines.is_empty() {
            error.push_str(" The file is now empty.");
        } else {
            error.push_str(" The hash does not identify a current line.");
            if let Some(line) = hint {
                error.push_str(&format!(
                    " Previous line {line} is outside the current {} lines.",
                    lines.len()
                ));
            }
        }
        error.push_str("\nRe-read the affected range and use its current tags.");
        Err(anyhow::anyhow!(error))
    };
    let mut plan = Vec::with_capacity(edits.len());
    for (key, text) in edits {
        let (first, last) = if let Some(tag) = key.strip_prefix('+') {
            ensure!(text.is_some(), "insertion requires text");
            let at = index(tag, key, None)?;
            (at, at)
        } else if let Some(tag) = key.strip_suffix('+') {
            ensure!(text.is_some(), "insertion requires text");
            let at = index(tag, key, None)? + 1;
            (at, at)
        } else if let Some((from, to)) = key.split_once("..") {
            (index(from, key, Some(to))?, index(to, key, Some(from))? + 1)
        } else {
            let at = index(key, key, None)?;
            (at, at + 1)
        };
        ensure!(
            first <= last && !(key.contains("..") && first == last),
            "line range runs backwards"
        );
        plan.push((first, last, text));
    }
    plan.sort_by_key(|&(first, last, _)| (first, last));
    for pair in plan.windows(2) {
        let (start, stop, _) = pair[0];
        let (next, next_stop, _) = pair[1];
        ensure!(
            next >= stop && !(start == stop && next == next_stop && stop == next),
            "overlapping or ambiguous tagged edits"
        );
    }
    let first_changed = plan[0].0 + 1;
    let mut result: Vec<std::borrow::Cow<'_, str>> = Vec::new();
    let mut cursor = 0;
    for (start, end, replacement) in plan {
        result.extend(lines[cursor..start].iter().copied().map(Into::into));
        if let Some(replacement) = replacement {
            let crlf = lines
                .get(start)
                .or_else(|| lines.last())
                .is_some_and(|line| line.ends_with('\r'));
            result.extend(replacement.split_terminator('\n').map(|line| {
                if crlf && !line.ends_with('\r') {
                    format!("{line}\r").into()
                } else {
                    line.into()
                }
            }));
        }
        cursor = end;
    }
    result.extend(lines[cursor..].iter().copied().map(Into::into));
    let mut updated = result.join("\n");
    if !result.is_empty() && original.ends_with('\n') {
        updated.push('\n');
    }
    ensure!(updated != original, "tagged edits leave file unchanged");
    Ok((updated, first_changed))
}

fn append_current_line(output: &mut String, index: usize, tags: &[String], lines: &[&str]) {
    use std::fmt::Write;
    let line = lines[index].strip_suffix('\r').unwrap_or(lines[index]);
    let _ = write!(
        output,
        "\n{} {}|{}",
        index + 1,
        tags[index],
        crate::context::clip(line, 160)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_survive_unrelated_lines_and_preserve_crlf() {
        let path = "sample";
        let original = "head\r\nstart\r\nend\r\ntail\r\n";
        let tags = tags(path, &lines(original).collect::<Vec<_>>());
        let moved = "new head\r\nhead\r\nstart\r\nend\r\ntail\r\n";
        let edits = BTreeMap::from([
            (
                format!("{}..{}", tags[1], tags[2]),
                Some("replacement\nsecond".into()),
            ),
            (format!("+{}", tags[0]), Some("before".into())),
            (format!("{}+", tags[3]), Some("after".into())),
        ]);
        let (updated, first) = apply(path, moved, &edits, &BTreeMap::new()).unwrap();
        assert_eq!(first, 2);
        assert_eq!(
            updated,
            "new head\r\nbefore\r\nhead\r\nreplacement\r\nsecond\r\ntail\r\nafter\r\n"
        );
        assert!(apply(path, &updated, &edits, &BTreeMap::new()).is_err());
    }

    #[test]
    fn changed_text_uses_a_line_hint_without_authorizing_the_edit() {
        let path = "sample";
        let original = "first\nold\nlast\n";
        let before = tags(path, &lines(original).collect::<Vec<_>>());
        let edits = BTreeMap::from([(before[1].clone(), Some("replacement".into()))]);
        let hints = BTreeMap::from([(before[1].clone(), 2)]);
        let current = "first\nchanged\nlast\n";
        let current_tags = tags(path, &lines(current).collect::<Vec<_>>());
        let error = apply(path, current, &edits, &hints)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&format!("2 {}|changed", current_tags[1])));
        let error = apply(path, current, &edits, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not identify a current line"));
        assert!(!error.contains("|changed"));
    }

    #[test]
    fn changed_duplicate_count_invalidates_old_anchors() {
        let path = "sample";
        let original = "same\nother\nsame\n";
        let tags = tags(path, &lines(original).collect::<Vec<_>>());
        assert_ne!(tags[0], tags[2]);
        let edits = BTreeMap::from([
            (tags[0].clone(), None),
            (tags[1].clone(), Some("updated".into())),
        ]);
        let error = apply(path, "other\nsame\n", &edits, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("2 "));
        assert!(error.to_string().contains("|same"));
        assert_eq!(
            apply(path, original, &edits, &BTreeMap::new()).unwrap().0,
            "updated\nsame\n"
        );
    }
}
