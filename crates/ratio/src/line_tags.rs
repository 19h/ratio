use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, ensure};

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
    let index = |tag: &str| {
        indices
            .get(tag)
            .copied()
            .with_context(|| format!("stale or unknown line tag {tag:?}. Re-read affected lines"))
    };
    let mut plan = Vec::with_capacity(edits.len());
    for (key, text) in edits {
        let (first, last) = if let Some(tag) = key.strip_prefix('+') {
            ensure!(text.is_some(), "insertion requires text");
            let at = index(tag)?;
            (at, at)
        } else if let Some(tag) = key.strip_suffix('+') {
            ensure!(text.is_some(), "insertion requires text");
            let at = index(tag)? + 1;
            (at, at)
        } else if let Some((from, to)) = key.split_once("..") {
            (index(from)?, index(to)? + 1)
        } else {
            let at = index(key)?;
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
        let (updated, first) = apply(path, moved, &edits).unwrap();
        assert_eq!(first, 2);
        assert_eq!(
            updated,
            "new head\r\nbefore\r\nhead\r\nreplacement\r\nsecond\r\ntail\r\nafter\r\n"
        );
        assert!(apply(path, &updated, &edits).is_err());
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
        assert!(apply(path, "other\nsame\n", &edits).is_err());
        assert_eq!(apply(path, original, &edits).unwrap().0, "updated\nsame\n");
    }
}
