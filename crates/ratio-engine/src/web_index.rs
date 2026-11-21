//! Rebuildable projections for the web dashboard. The run journal remains authoritative.

use std::{path::Path, time::Duration};

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::RunReader;

const BATCH: u32 = 256;

pub struct DashboardIndex {
    cache: Connection,
    cursor: u64,
    marker: Option<String>,
}

impl DashboardIndex {
    pub fn open(directory: &Path, journal: &RunReader, run: &str) -> Result<Self> {
        ensure!(
            !run.is_empty()
                && run.len() <= 80
                && run
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b'-'),
            "Invalid web index run"
        );
        let cache = Connection::open(directory.join(format!("web-index-v1-{run}.sqlite3")))?;
        cache.busy_timeout(Duration::from_millis(100))?;
        cache.execute_batch(
            "PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS progress (singleton INTEGER PRIMARY KEY, cursor INTEGER, marker TEXT);
             CREATE TABLE IF NOT EXISTS windows (
               name TEXT NOT NULL, seq INTEGER NOT NULL,
               PRIMARY KEY(name,seq)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS messages (
               id TEXT PRIMARY KEY, delivery TEXT NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS routes (
               sender TEXT NOT NULL, recipient TEXT NOT NULL, delivery TEXT NOT NULL, count INTEGER NOT NULL,
               PRIMARY KEY(sender,recipient,delivery)
             ) WITHOUT ROWID;",
        )?;
        let stored = cache
            .query_row(
                "SELECT cursor,marker FROM progress WHERE singleton=1",
                [],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let (cursor, marker) = match stored {
            Some((cursor, Some(marker)))
                if journal.audit_marker(run, cursor)?.as_ref() == Some(&marker) =>
            {
                (cursor, Some(marker))
            }
            Some(_) => {
                cache.execute_batch("BEGIN IMMEDIATE; DELETE FROM progress; DELETE FROM messages; DELETE FROM routes; DELETE FROM windows; COMMIT;")?;
                (0, None)
            }
            None => (0, None),
        };
        Ok(Self {
            cache,
            cursor,
            marker,
        })
    }

    pub fn advance(&mut self, journal: &RunReader, run: &str, head: u64) -> Result<bool> {
        let changed_source =
            self.cursor != 0 && journal.audit_marker(run, self.cursor)? != self.marker;
        if self.cursor == head && !changed_source {
            return Ok(true);
        }
        if self.cursor > head || changed_source {
            let transaction = self
                .cache
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch("DELETE FROM progress; DELETE FROM messages; DELETE FROM routes; DELETE FROM windows;")?;
            transaction.commit()?;
            self.cursor = 0;
            self.marker = None;
        }
        let entries = journal.dashboard_index_events(run, self.cursor, head, BATCH)?;
        let cursor = if entries.len() == BATCH as usize {
            entries.last().expect("full audit batch").sequence
        } else {
            head
        };
        let marker = journal.audit_marker(run, cursor)?;
        let transaction = self
            .cache
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = transaction
            .query_row(
                "SELECT cursor,marker FROM progress WHERE singleton=1",
                [],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
            .unwrap_or_default();
        if stored.0 != self.cursor || stored.1 != self.marker {
            // Another dashboard projected this run while the source batch was read.
            self.cursor = stored.0;
            self.marker = stored.1;
            return Ok(false);
        }
        let mut touched = std::collections::BTreeSet::new();
        for event in entries {
            for (name, include) in [
                ("timeline", event.timeline),
                ("ipc", event.kind == "ipc"),
                ("tool", event.kind == "tool"),
                ("mcp", event.mcp),
                ("job", event.job),
                ("file", event.file),
            ] {
                if include {
                    transaction
                        .prepare_cached("INSERT OR IGNORE INTO windows VALUES (?1,?2)")?
                        .execute(params![name, event.sequence])?;
                    touched.insert(name);
                }
            }
            if event.kind != "ipc" {
                continue;
            }
            let Some(delivery) = event.data["delivery"].as_str() else {
                continue;
            };
            let Some(recipients) = event.data["to"].as_array() else {
                continue;
            };
            for recipient in recipients.iter().filter_map(Value::as_str) {
                let sender = &event.actor;
                let id = json!([
                    event.phase,
                    event.cycle,
                    sender,
                    recipient,
                    event.data["sequence"],
                    event.data["attempt"],
                    event.data["worker_request"]
                ])
                .to_string();
                let prior = transaction
                    .query_row("SELECT delivery FROM messages WHERE id=?1", [&id], |row| {
                        row.get::<_, String>(0)
                    })
                    .optional()?;
                if let Some(prior) = prior {
                    if prior == delivery || delivery == "queued" {
                        continue;
                    }
                    transaction.execute(
                        "UPDATE routes SET count=count-1 WHERE sender=?1 AND recipient=?2 AND delivery=?3",
                        params![sender, recipient, prior],
                    )?;
                    transaction.execute(
                        "DELETE FROM routes WHERE sender=?1 AND recipient=?2 AND delivery=?3 AND count=0",
                        params![sender, recipient, prior],
                    )?;
                }
                transaction.execute(
                    "INSERT INTO routes VALUES (?1,?2,?3,1)
                     ON CONFLICT(sender,recipient,delivery) DO UPDATE SET count=count+1",
                    params![sender, recipient, delivery],
                )?;
                transaction.execute(
                    "INSERT INTO messages VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET delivery=excluded.delivery",
                    params![id, delivery],
                )?;
            }
        }
        for name in touched {
            transaction
                .prepare_cached(
                    "DELETE FROM windows WHERE name=?1 AND seq <
                 (SELECT seq FROM windows WHERE name=?1 ORDER BY seq DESC LIMIT 1 OFFSET 59)",
                )?
                .execute([name])?;
        }
        transaction.execute(
            "INSERT INTO progress VALUES (1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET cursor=excluded.cursor,marker=excluded.marker",
            params![cursor, marker],
        )?;
        transaction.commit()?;
        self.cursor = cursor;
        self.marker = marker;
        Ok(cursor == head)
    }

    pub fn window(
        &self,
        journal: &RunReader,
        run: &str,
        name: &str,
    ) -> Result<Vec<crate::audit::AuditEvent>> {
        let sequences = self
            .cache
            .prepare_cached("SELECT seq FROM windows WHERE name=?1 ORDER BY seq DESC")?
            .query_map([name], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<u64>>>()?;
        Ok(journal.dashboard_index_rows(run, &sequences)?)
    }

    pub fn routes(&self) -> Result<Vec<Value>> {
        let mut statement = self.cache.prepare_cached(
            "SELECT sender,recipient,delivery,count FROM routes ORDER BY sender,recipient,delivery",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(
                    json!({"from":row.get::<_,String>(0)?,"to":row.get::<_,String>(1)?,
                "state":row.get::<_,String>(2)?,"count":row.get::<_,u64>(3)?}),
                )
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Journal,
        audit::{AuditFilter, Observation},
        journal::CheckpointDetails,
    };

    #[test]
    fn indexed_windows_and_routes_match_journal_after_batching_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let mut writer = Journal::open(&path).unwrap();
        let run = "1-1";
        let mut audit = (0..300)
            .map(|n| Observation {
                key: format!("chat-{n}"),
                kind: "chat",
                actor: "worker_1".into(),
                data: json!({"text":format!("event-{n}")}),
            })
            .collect::<Vec<_>>();
        audit.extend([
            Observation {
                key: "job-1".into(), kind: "tool", actor: "worker_1".into(),
                data: json!({"tool":"mcp_call","path":"example","arguments":{"private":"not in list"}}),
            },
            Observation {
                key: "job-1-completed".into(), kind: "tool_result", actor: "worker_1".into(),
                data: json!({"state":"completed","output":"full receipt"}),
            },
            Observation {
                key: "empty-chat".into(), kind: "chat", actor: "worker_1".into(),
                data: json!({"text":"  ","calls":[{"key":"job-1"}]}),
            },
        ]);
        for (i, delivery) in ["queued", "delivered", "queued"].into_iter().enumerate() {
            audit.push(Observation {
                key: format!("ipc-{i}"),
                kind: "ipc",
                actor: "worker_1".into(),
                data: json!({"sequence":1,"attempt":0,"to":["worker_2"],"delivery":delivery}),
            });
        }
        writer
            .commit_checkpoint(
                run,
                "Work",
                1,
                b"{}",
                CheckpointDetails {
                    audit,
                    ..Default::default()
                },
            )
            .unwrap();
        let reader = RunReader::open(&path).unwrap();
        let head = reader.audit_head(run).unwrap();
        let mut index = DashboardIndex::open(directory.path(), &reader, run).unwrap();
        let mut concurrent = DashboardIndex::open(directory.path(), &reader, run).unwrap();
        assert!(!index.advance(&reader, run, head).unwrap());
        assert!(!concurrent.advance(&reader, run, head).unwrap());
        assert!(concurrent.advance(&reader, run, head).unwrap());
        drop(index);
        let mut index = DashboardIndex::open(directory.path(), &reader, run).unwrap();
        assert!(index.advance(&reader, run, head).unwrap());
        for (bucket, kind, path) in [
            ("timeline", "timeline", None),
            ("ipc", "ipc", None),
            ("tool", "tool", None),
            ("mcp", "mcp", None),
            ("job", "job", None),
            ("file", "tool", Some("")),
        ] {
            let expected = reader
                .audit(
                    run,
                    AuditFilter {
                        kind: Some(kind),
                        path,
                        ..Default::default()
                    },
                    60,
                )
                .unwrap();
            assert_eq!(
                json!(index.window(&reader, run, bucket).unwrap()),
                json!(expected),
                "{bucket}"
            );
        }
        assert_eq!(
            index.routes().unwrap(),
            reader.ipc_routes(run, None, None, None).unwrap()
        );
        assert_eq!(index.routes().unwrap()[0]["state"], "delivered");
    }
}
