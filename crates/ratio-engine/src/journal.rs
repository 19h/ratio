use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ratio_protocol::{Effect, IpcEvent, RunEvent, RunSummary, Workflow};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Result, TransactionBehavior, params};

const VERSION: u32 = 1;
const SCHEMA: &str = "
    CREATE TABLE checkpoint_blobs (
        digest TEXT PRIMARY KEY,
        content BLOB NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE events (
        sequence INTEGER PRIMARY KEY,
        timestamp_ms INTEGER NOT NULL,
        run_id TEXT NOT NULL,
        phase TEXT NOT NULL,
        cycle INTEGER NOT NULL,
        checkpoint TEXT NOT NULL REFERENCES checkpoint_blobs(digest),
        effects TEXT NOT NULL,
        ipc TEXT NOT NULL,
        workflow TEXT NOT NULL
    );
    CREATE INDEX events_by_run ON events(run_id, sequence);
    CREATE TABLE runs (
        run_id TEXT PRIMARY KEY,
        phase TEXT NOT NULL,
        cycle INTEGER NOT NULL,
        first_sequence INTEGER NOT NULL REFERENCES events(sequence),
        last_sequence INTEGER NOT NULL REFERENCES events(sequence)
    ) WITHOUT ROWID;
    CREATE TABLE budgets (
        run_id TEXT PRIMARY KEY,
        content BLOB NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE effect_state (
        run_id TEXT NOT NULL,
        phase TEXT NOT NULL,
        cycle INTEGER NOT NULL,
        actor TEXT NOT NULL,
        attempt INTEGER NOT NULL,
        round INTEGER NOT NULL,
        call_index INTEGER NOT NULL,
        state TEXT NOT NULL,
        PRIMARY KEY (run_id, phase, cycle, actor, attempt, round, call_index)
    ) WITHOUT ROWID;
    PRAGMA user_version = 1;
";

pub struct Journal(Connection);
pub struct RunReader(Connection);

fn connect(path: &Path, flags: OpenFlags) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

fn check_version(connection: &Connection, allow_new: bool) -> Result<u32> {
    let version = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == VERSION || (allow_new && version == 0) {
        return Ok(version);
    }
    Err(rusqlite::Error::InvalidParameterName(format!(
        "unsupported Ratio journal schema version {version}"
    )))
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let mut connection = connect(
            path,
            OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        check_version(&connection, true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if check_version(&transaction, true)? == 0 {
            transaction.execute_batch(SCHEMA)?;
        }
        transaction.commit()?;
        Ok(Self(connection))
    }

    pub fn commit(
        &mut self,
        run_id: &str,
        phase: &str,
        cycle: usize,
        content: &[u8],
    ) -> Result<u64> {
        self.commit_checkpoint(run_id, phase, cycle, content, CheckpointDetails::default())
    }

    pub(crate) fn commit_checkpoint(
        &mut self,
        run_id: &str,
        phase: &str,
        cycle: usize,
        content: &[u8],
        details: CheckpointDetails,
    ) -> Result<u64> {
        let digest: String = ring::digest::digest(&ring::digest::SHA256, content)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let transaction = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(u64, String, String, usize)> = transaction
            .query_row(
                "SELECT e.sequence, e.checkpoint, e.phase, e.cycle FROM runs r JOIN events e
                 ON e.sequence = r.last_sequence WHERE r.run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((sequence, previous_digest, previous_phase, previous_cycle)) = previous {
            if previous_digest == digest
                && previous_phase == phase
                && previous_cycle == cycle
                && details.ipc.is_empty()
            {
                return Ok(sequence);
            }
        }
        transaction.execute(
            "INSERT OR IGNORE INTO checkpoint_blobs(digest, content) VALUES (?1, ?2)",
            params![digest, content],
        )?;
        let mut changes = Vec::new();
        for effect in details.effects {
            let state = serde_json::to_string(&effect.state)
                .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
            let count = transaction.execute(
                "INSERT INTO effect_state VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT DO UPDATE SET state = excluded.state WHERE state != excluded.state",
                params![
                    run_id,
                    phase,
                    cycle,
                    effect.actor,
                    effect.attempt,
                    effect.round,
                    effect.index,
                    state
                ],
            )?;
            if count != 0 {
                changes.push(effect);
            }
        }
        let effects = encode_json(&changes)?;
        let ipc = encode_json(&details.ipc)?;
        let workflow = encode_json(&details.workflow)?;
        transaction.execute(
            "INSERT INTO events(run_id, phase, cycle, checkpoint, effects, ipc, workflow, timestamp_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![run_id, phase, cycle, digest, effects, ipc, workflow, timestamp_ms()],
        )?;
        let sequence = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO runs VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(run_id) DO UPDATE SET phase = excluded.phase, cycle = excluded.cycle,
                 last_sequence = excluded.last_sequence",
            params![run_id, phase, cycle, sequence],
        )?;
        transaction.commit()?;
        Ok(sequence as u64)
    }

    pub fn checkpoint(&self, run_id: Option<&str>) -> Result<Option<Vec<u8>>> {
        self.0
            .query_row(
                "SELECT b.content FROM runs r JOIN events e ON e.sequence = r.last_sequence
             JOIN checkpoint_blobs b ON b.digest = e.checkpoint
             WHERE ?1 IS NULL OR r.run_id = ?1 ORDER BY r.last_sequence DESC LIMIT 1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
    }

    pub(crate) fn budget(&self, run_id: &str) -> Result<Option<Vec<u8>>> {
        self.0
            .query_row(
                "SELECT content FROM budgets WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()
    }

    pub(crate) fn save_budget(&self, run_id: &str, content: &[u8]) -> Result<()> {
        self.0.execute(
            "INSERT INTO budgets VALUES (?1, ?2)
             ON CONFLICT(run_id) DO UPDATE SET content = excluded.content WHERE content != excluded.content",
            params![run_id, content],
        )?;
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct CheckpointDetails {
    pub effects: Vec<Effect>,
    pub ipc: Vec<IpcEvent>,
    pub workflow: Workflow,
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))
}

fn decode_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<T> {
    serde_json::from_str(&row.get::<_, String>(index)?).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
    })
}

const EVENT_COLUMNS: &str =
    "sequence, run_id, phase, cycle, checkpoint, effects, ipc, workflow, timestamp_ms";

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn decode_event(row: &rusqlite::Row<'_>) -> Result<RunEvent> {
    Ok(RunEvent {
        sequence: row.get(0)?,
        timestamp_ms: row.get(8)?,
        run_id: row.get(1)?,
        phase: row.get(2)?,
        cycle: row.get(3)?,
        checkpoint: row.get(4)?,
        effects: decode_column(row, 5)?,
        ipc: decode_column(row, 6)?,
        workflow: decode_column(row, 7)?,
    })
}

impl RunReader {
    pub(crate) fn ledger(&self, run_id: &str) -> Result<crate::budget::Ledger> {
        let data: Option<Vec<u8>> = self
            .0
            .query_row(
                "SELECT content FROM budgets WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        match data {
            Some(data) => serde_json::from_slice(&data).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Blob,
                    Box::new(err),
                )
            }),
            None => Ok(crate::budget::Ledger::default()),
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let connection = connect(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        check_version(&connection, false)?;
        Ok(Self(connection))
    }

    pub fn runs(&self) -> Result<Vec<RunSummary>> {
        let mut statement = self.0.prepare(
            "SELECT run_id, phase, cycle, first_sequence, last_sequence FROM runs
             ORDER BY last_sequence DESC",
        )?;
        statement
            .query_map([], |row| {
                Ok(RunSummary {
                    run_id: row.get(0)?,
                    phase: row.get(1)?,
                    cycle: row.get(2)?,
                    first_sequence: row.get(3)?,
                    last_sequence: row.get(4)?,
                })
            })?
            .collect()
    }

    pub fn run(&self, run_id: &str) -> Result<Option<RunSummary>> {
        self.0.query_row(
            "SELECT run_id, phase, cycle, first_sequence, last_sequence FROM runs WHERE run_id = ?1",
            [run_id],
            |row| Ok(RunSummary {
                run_id: row.get(0)?, phase: row.get(1)?, cycle: row.get(2)?,
                first_sequence: row.get(3)?, last_sequence: row.get(4)?,
            }),
        ).optional()
    }

    pub fn events(&self, run_id: &str, after: u64, limit: u32) -> Result<Vec<RunEvent>> {
        let mut statement = self.0.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events
             WHERE run_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3"
        ))?;
        statement
            .query_map(params![run_id, after, limit.min(1000)], decode_event)?
            .collect()
    }

    pub fn latest_event(&self, run_id: &str) -> Result<Option<RunEvent>> {
        self.0.query_row(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE run_id = ?1 ORDER BY sequence DESC LIMIT 1"
        ), [run_id], decode_event).optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_see_committed_history_and_resuming_an_old_run_makes_it_current() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let mut journal = Journal::open(&path).unwrap();
        let first = journal.commit("first", "Plan", 1, b"plan").unwrap();
        assert_eq!(journal.commit("first", "Plan", 1, b"plan").unwrap(), first);
        let reader = RunReader::open(&path).unwrap();
        journal.commit("second", "Plan", 1, b"second").unwrap();
        assert_eq!(journal.checkpoint(None).unwrap().unwrap(), b"second");
        journal.commit("first", "Work", 1, b"work").unwrap();
        assert_eq!(journal.checkpoint(None).unwrap().unwrap(), b"work");
        assert_eq!(reader.runs().unwrap()[0].run_id, "first");
        let remaining = reader.events("first", first, 1).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].phase, "Work");
        assert_eq!(
            journal.checkpoint(Some("second")).unwrap().unwrap(),
            b"second"
        );
    }
}
