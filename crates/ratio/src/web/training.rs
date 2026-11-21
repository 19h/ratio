use super::dictionary::{compressor, directory};
use super::realtime::{CHUNK, Server, legacy_gzip};
use super::*;
use ratio_engine::audit::AuditEvent;
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use zstd::bulk::Decompressor;

const MIB: usize = 1024 * 1024;
const SIZES: [usize; 3] = [16 * 1024, 32 * 1024, 64 * 1024];

#[derive(Default)]
struct Samples {
    chunks: Vec<Vec<u8>>,
    bytes: usize,
}

impl Samples {
    fn record(&mut self, message: Server, limit: usize) -> Result<()> {
        let data = serde_json::to_vec(&message)?;
        let chunks = data.chunks(CHUNK);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            if index != 0 && index != count / 2 && index + 1 != count {
                continue;
            }
            if self.bytes + chunk.len() <= limit {
                self.chunks.push(chunk.to_vec());
                self.bytes += chunk.len();
            }
        }
        Ok(())
    }

    fn append(&mut self, source: Self) {
        self.bytes += source.bytes;
        self.chunks.extend(source.chunks);
    }
}

fn packet(run: &str, path: &str, data: Value, query: bool) -> Server {
    let values = BTreeMap::from([(
        format!("/api/run/{run}/{path}"),
        Arc::new(json!({"data":data})),
    )]);
    if query {
        Server::Query { id: 1, values }
    } else {
        Server::Data {
            scope: Some(run.into()),
            values,
        }
    }
}

fn detail_in_range(
    reader: &RunReader,
    run: &str,
    detail: &Value,
    first: u64,
    last: u64,
) -> Result<bool> {
    let range = first..=last;
    for name in ["event", "outcome", "turn", "origin"] {
        if let Some(sequence) = detail[name]["sequence"].as_u64() {
            if !range.contains(&sequence) {
                return Ok(false);
            }
        }
    }
    for name in ["deliveries", "tool_stages"] {
        for row in detail[name].as_array().into_iter().flatten() {
            if let Some(key) = row["key"].as_str() {
                if !reader
                    .audit_event_sequence(run, key)?
                    .is_some_and(|sequence| range.contains(&sequence))
                {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn preview_in_range(reader: &RunReader, run: &str, row: &AuditEvent, last: u64) -> Result<bool> {
    if row.kind == "tool" {
        // The list projection can merge a result newer than the tool intent.
        for state in ["running", "completed", "failed", "refused", "unknown"] {
            let key = format!("{}-{state}", row.key);
            if reader
                .audit_event_sequence(run, &key)?
                .is_some_and(|sequence| sequence > last)
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn sample_run(
    reader: &RunReader,
    run: &str,
    samples: &mut Samples,
    shared_holdout: &mut Samples,
    held_out: bool,
    first: u64,
    last: u64,
) -> Result<()> {
    let limit = if held_out { 4 * MIB } else { 8 * MIB };
    let points = if held_out { 8 } else { 20 };
    let views = [
        None,
        Some("timeline"),
        Some("ipc"),
        Some("tool"),
        Some("mcp"),
        Some("job"),
    ];
    let mut details = std::collections::BTreeSet::new();
    for step in 1..=points {
        let end = first + (last - first) * step / points;
        let kind = views[step as usize % views.len()];
        reader.dashboard_deadline(Duration::from_secs(3))?;
        let rows = reader
            .audit(
                run,
                AuditFilter {
                    before: end.checked_add(1),
                    kind,
                    ..AuditFilter::default()
                },
                60,
            )
            .with_context(|| format!("Read {} training window", kind.unwrap_or("history")))?;
        let mut selected = Vec::with_capacity(rows.len());
        for row in rows {
            if row.sequence >= first && preview_in_range(reader, run, &row, last)? {
                selected.push(row);
            }
        }
        if selected.is_empty() {
            continue;
        }
        let path = match kind {
            None => "history".into(),
            Some("timeline") => "history?collapse_tools=1".into(),
            Some(kind) => format!("history?kind={kind}"),
        };
        for count in [1, 8, selected.len()] {
            let value = json!({"rows":&selected[..count.min(selected.len())]});
            samples.record(packet(run, &path, value.clone(), false), limit)?;
            if held_out {
                shared_holdout.record(packet(run, &path, value, false), limit)?;
            }
        }
        if let Some(row) = selected.iter().find(|row| {
            matches!(row.kind.as_str(), "tool" | "ipc" | "chat")
                && details.insert((row.kind.clone(), row.sequence))
        }) {
            let path = format!("entry/{}", row.key);
            reader.dashboard_deadline(Duration::from_secs(3))?;
            let value =
                select(reader, run, &path, &Query::new()).context("Read a training entry")?;
            if !detail_in_range(reader, run, &value, first, last)? {
                continue;
            }
            samples.record(packet(run, &path, value.clone(), true), limit)?;
            if held_out {
                shared_holdout.record(packet(run, &path, value, true), limit)?;
            }
        }
    }
    Ok(())
}

fn real_samples(dirs: &[PathBuf]) -> Result<(Samples, Samples, Samples)> {
    let (mut train, mut holdout, mut shared) =
        (Samples::default(), Samples::default(), Samples::default());
    let mut seen = std::collections::BTreeSet::new();
    for (index, dir) in dirs.iter().enumerate() {
        let path = dir.join("runs.sqlite3").canonicalize()?;
        ensure!(
            seen.insert(path.clone()),
            "A training journal was supplied more than once"
        );
        let reader = RunReader::open(&path)?;
        let (mut local_train, mut local_holdout, mut local_shared) =
            (Samples::default(), Samples::default(), Samples::default());
        for run in reader.runs()? {
            let run_id = &run.run_id;
            let head = reader.audit_head(run_id)?;
            let Some(first) = reader
                .audit(
                    run_id,
                    AuditFilter {
                        after: Some(0),
                        ..AuditFilter::default()
                    },
                    1,
                )?
                .first()
                .map(|row| row.sequence)
            else {
                continue;
            };
            if head <= first {
                continue;
            }
            let split = first + (head - first) * 4 / 5;
            sample_run(
                &reader,
                run_id,
                &mut local_train,
                &mut local_shared,
                false,
                first,
                split,
            )?;
            sample_run(
                &reader,
                run_id,
                &mut local_holdout,
                &mut local_shared,
                true,
                split + 1,
                head,
            )?;
            reader.dashboard_deadline(Duration::from_secs(3))?;
            let mut value = overview(
                &reader,
                run_id,
                Some(&ratio_protocol::RunMetrics::default()),
            )?;
            local_holdout.record(packet(run_id, "overview", value.clone(), false), 4 * MIB)?;
            value["parent_run"] = Value::Null;
            local_shared.record(packet(run_id, "overview", value, false), 4 * MIB)?;
        }
        ensure!(
            local_train.bytes != 0 && local_holdout.bytes != 0,
            "Training journal {} lacks usable training or holdout samples",
            index + 1
        );
        println!(
            "Journal {}: {} training chunks, {} holdout chunks",
            index + 1,
            local_train.chunks.len(),
            local_holdout.chunks.len()
        );
        train.append(local_train);
        holdout.append(local_holdout);
        shared.append(local_shared);
    }
    Ok((train, holdout, shared))
}

fn synthetic_samples(first: usize, length: usize) -> Result<Samples> {
    let mut samples = Samples::default();
    let tasks = [
        "Investigate the interface",
        "Read the fixture",
        "Compare the project files",
        "Verify the report",
    ];
    for i in first..first + length {
        let run = format!("1-{}", 1700000000000000000u64 + i as u64 * 100);
        let actor = format!("worker_{}", i % 8 + 1);
        let mut rows = Vec::new();
        for offset in 0..[1, 4, 12, 60][i % 4] {
            let key = format!("fixture-{}-{}", i, offset);
            let (kind, data) = match (i / 4 + offset) % 5 {
                0 => ("chat", json!({"text":{"text":tasks[i % 4]},"calls":[]})),
                1 => (
                    "ipc",
                    json!({"to":["lead_reviewer"],"sequence":i,"delivery":"delivered","text":tasks[i % 4],"attempt":1}),
                ),
                2 => (
                    "tool",
                    json!({"tool":"read_file","path":"example/input.txt","arguments":{"text":"{\"path\":\"example/input.txt\"}"},"state":"queued"}),
                ),
                3 => (
                    "tool_result",
                    json!({"tool":"read_file","state":"completed","elapsed_ms":42,
                    "output":{"text":format!("Example row {offset}. {}", tasks[i % 4])},"full":{"text":tasks[i % 4].repeat(offset + 1)}}),
                ),
                _ => (
                    "control",
                    json!({"text":"Guidance recorded","state":"accepted"}),
                ),
            };
            rows.push(json!({"key":key,"kind":kind,"actor":actor,"data":data,"sequence":i+offset,
                "checkpoint":i,"timestamp_ms":1700000000000u64+i as u64,"phase":"Work","cycle":i%3+1}));
        }
        samples.record(
            packet(
                &run,
                "history?collapse_tools=1",
                json!({"rows":rows}),
                false,
            ),
            usize::MAX,
        )?;
        let selected = &rows[0];
        let key = selected["key"].as_str().expect("synthetic event key");
        let outcome = json!({"event":selected,"outcome":null,"turn":null,"origin":null,
            "deliveries":[],"tool_stages":[{"key":key,"timestamp_ms":1700000000000u64+i as u64,"state":"queued"}]});
        samples.record(
            packet(&run, &format!("entry/{key}"), outcome, true),
            usize::MAX,
        )?;
        let value = json!({"paused":false,"generations":[{"actor":actor,"content":tasks[i%4]}],
            "token_usage":{"tokens":i,"estimated":true,"output_tokens_per_second":i%70}});
        samples.record(packet(&run, "live", value, false), usize::MAX)?;
    }
    Ok(samples)
}

#[derive(Default)]
struct Measure {
    wire: usize,
    buckets: [Bucket; 3],
}

#[derive(Default)]
struct Bucket {
    wire: usize,
    encode_us: Vec<u128>,
    decode_us: Vec<u128>,
}

fn measure(samples: &Samples, dictionary: Option<&[u8]>, legacy: bool) -> Result<Measure> {
    let mut encoder = compressor(dictionary.unwrap_or_default())?;
    let mut decoder = Decompressor::with_dictionary(dictionary.unwrap_or_default())?;
    let mut stats = Measure::default();
    for chunk in &samples.chunks {
        let bucket = match chunk.len() {
            0..=4096 => &mut stats.buckets[0],
            4097..=32768 => &mut stats.buckets[1],
            _ => &mut stats.buckets[2],
        };
        let start = Instant::now();
        let encoded = if legacy {
            legacy_gzip(chunk, true)
        } else {
            Some(encoder.compress(chunk)?)
        };
        bucket.encode_us.push(start.elapsed().as_micros());
        let start = Instant::now();
        let decoded = if legacy {
            if let Some(bytes) = &encoded {
                use std::io::Read;
                let mut result = Vec::with_capacity(chunk.len());
                flate2::read::GzDecoder::new(bytes.as_slice()).read_to_end(&mut result)?;
                result
            } else {
                chunk.clone()
            }
        } else {
            decoder.decompress(encoded.as_deref().expect("Zstandard frame"), chunk.len())?
        };
        bucket.decode_us.push(start.elapsed().as_micros());
        ensure!(decoded == *chunk, "Compression sample did not round-trip");
        let bytes = encoded.as_ref().map_or(chunk.len(), Vec::len) + if legacy { 9 } else { 13 };
        stats.wire += bytes;
        bucket.wire += bytes;
    }
    Ok(stats)
}

fn candidate(train: &Samples, holdout: &Samples) -> Result<Vec<u8>> {
    let mut candidates = Vec::new();
    for size in SIZES {
        let dictionary = zstd::dict::from_samples(&train.chunks, size)?;
        let stats = measure(holdout, Some(&dictionary), false)?;
        candidates.push((dictionary, stats.wire));
    }
    let best = candidates
        .iter()
        .map(|(_, bytes)| *bytes)
        .min()
        .expect("dictionary candidates");
    Ok(candidates
        .into_iter()
        .find(|(_, bytes)| *bytes <= best + best / 100)
        .expect("at least one dictionary within the selection threshold")
        .0)
}

fn p95(samples: &mut [u128]) -> u128 {
    samples.sort_unstable();
    samples[(samples.len() * 95).div_ceil(100).saturating_sub(1)]
}

fn check(name: &str, holdout: &Samples, dictionary: &[u8]) -> Result<()> {
    let gzip = measure(holdout, None, true)?;
    let plain = measure(holdout, None, false)?;
    let mut trained = measure(holdout, Some(dictionary), false)?;
    let bootstrap = compressor(&[])?.compress(dictionary)?.len() + 13;
    let assets = legacy_gzip(include_bytes!("dashboard-decoder.wasm"), true)
        .map_or(include_bytes!("dashboard-decoder.wasm").len(), |bytes| {
            bytes.len()
        })
        + legacy_gzip(include_bytes!("dashboard-decoder.js"), true)
            .map_or(include_bytes!("dashboard-decoder.js").len(), |bytes| {
                bytes.len()
            });
    let saved = gzip.wire.saturating_sub(trained.wire);
    println!(
        "{name}: {} holdout chunks, {} decoded bytes, gzip {} B, Zstandard {} B, dictionary {} B, dictionary size {} B",
        holdout.chunks.len(),
        holdout.bytes,
        gzip.wire,
        plain.wire,
        trained.wire,
        dictionary.len()
    );
    println!("{name}: bootstrap {bootstrap} B, decoder assets {assets} B");
    for (index, label) in ["up to 4 KiB", "4–32 KiB", "32–128 KiB"].iter().enumerate() {
        let bucket = &mut trained.buckets[index];
        if !bucket.encode_us.is_empty() {
            println!(
                "{name} {label}: {} chunks, gzip {} B, plain {} B, dictionary {} B, encode p95 {} us, decode p95 {} us",
                bucket.encode_us.len(),
                gzip.buckets[index].wire,
                plain.buckets[index].wire,
                bucket.wire,
                p95(&mut bucket.encode_us),
                p95(&mut bucket.decode_us)
            );
        }
    }
    if saved > 0 {
        let chunks = ((bootstrap + assets) * holdout.chunks.len()).div_ceil(saved);
        println!("{name}: estimated cold-load break-even after {chunks} representative chunks");
    }
    ensure!(
        trained.wire < gzip.wire && trained.buckets[0].wire < plain.buckets[0].wire,
        "{name} dictionary did not beat the gzip and small-frame Zstandard baselines"
    );
    Ok(())
}

fn install(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut destination =
        tempfile::NamedTempFile::new_in(path.parent().context("Dictionary parent directory")?)?;
    destination.write_all(bytes)?;
    destination.as_file().sync_all()?;
    destination
        .persist(path)
        .context("Install dashboard dictionary")?;
    Ok(())
}

pub fn train_dictionaries(dirs: &[PathBuf]) -> Result<()> {
    let (train, holdout, shared_holdout) = real_samples(dirs)?;
    let operator = candidate(&train, &holdout)?;
    let shared = candidate(&synthetic_samples(0, 2048)?, &synthetic_samples(2048, 512)?)?;
    check("operator", &holdout, &operator)?;
    check("shared", &shared_holdout, &shared)?;
    let destination = directory()?;
    std::fs::create_dir_all(&destination)?;
    install(&destination.join("operator.zdict"), &operator)?;
    install(&destination.join("shared.zdict"), &shared)?;
    println!("Installed dictionaries in {}", destination.display());
    Ok(())
}
