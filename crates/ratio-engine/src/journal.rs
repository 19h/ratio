use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::audit::{AuditEvent, AuditFilter, Observation};
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

const AUDIT_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS audit_events (
        sequence INTEGER PRIMARY KEY, run_id TEXT NOT NULL, checkpoint INTEGER NOT NULL,
        timestamp_ms INTEGER NOT NULL, phase TEXT NOT NULL, cycle INTEGER NOT NULL,
        key TEXT NOT NULL, kind TEXT NOT NULL, actor TEXT NOT NULL, data TEXT NOT NULL,
        UNIQUE(run_id, key)
    );
    CREATE INDEX IF NOT EXISTS audit_run ON audit_events(run_id, sequence);
    CREATE INDEX IF NOT EXISTS audit_actor ON audit_events(run_id, actor, sequence);
    CREATE INDEX IF NOT EXISTS audit_kind ON audit_events(run_id, kind, sequence);
    CREATE VIRTUAL TABLE IF NOT EXISTS audit_search USING fts5(text);
    CREATE TABLE IF NOT EXISTS triage_inbox (
        id INTEGER PRIMARY KEY, run_id TEXT NOT NULL, text TEXT NOT NULL,
        state TEXT NOT NULL DEFAULT 'queued', decision TEXT, submitted_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS triage_deliveries (
        item INTEGER NOT NULL REFERENCES triage_inbox(id), actor TEXT NOT NULL, PRIMARY KEY(item,actor)
    );
    CREATE INDEX IF NOT EXISTS triage_run ON triage_inbox(run_id,id);";
const WORKER_AUDIT_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS worker_admission (
        request INTEGER PRIMARY KEY REFERENCES worker_requests(id), state TEXT NOT NULL,
        detail TEXT NOT NULL, plan TEXT, updated_ms INTEGER NOT NULL
    );";

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
    pub(crate) fn set_web_share(path: &Path, run: &str, digest: Option<&[u8]>) -> Result<()> {
        let connection = connect(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        check_version(&connection, false)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS web_shares (
                run_id TEXT PRIMARY KEY REFERENCES runs(run_id) ON DELETE CASCADE,
                digest BLOB NOT NULL
            ) WITHOUT ROWID;",
        )?;
        if let Some(digest) = digest {
            connection.execute(
                "INSERT INTO web_shares(run_id,digest) VALUES (?1,?2)
                 ON CONFLICT(run_id) DO UPDATE SET digest=excluded.digest",
                params![run, digest],
            )?;
        } else {
            connection.execute("DELETE FROM web_shares WHERE run_id=?1", [run])?;
        }
        Ok(())
    }

    pub(crate) fn record_control(&mut self, run: &str, action: &str, detail: &str) -> Result<()> {
        self.record_signal(run, "control", "user", action, detail)
    }

    pub(crate) fn record_signal(
        &mut self,
        run: &str,
        kind: &str,
        actor: &str,
        label: &str,
        detail: &str,
    ) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let data = encode_json(&serde_json::json!({"label":label,"text":detail}))?;
        let change = tx.execute("INSERT INTO audit_events(run_id,checkpoint,timestamp_ms,phase,cycle,key,kind,actor,data)
            SELECT r.run_id,r.last_sequence,?2,r.phase,r.cycle,
                'signal-'||(SELECT coalesce(max(sequence),0)+1 FROM audit_events),?3,?4,?5
            FROM runs r WHERE r.run_id=?1",params![run,timestamp_ms(),kind,actor,data])?;
        if change != 0 {
            tx.execute(
                "INSERT INTO audit_search(rowid,text) VALUES (?1,?2)",
                params![tx.last_insert_rowid(), format!("{label} {detail}")],
            )?;
        }
        tx.commit()
    }
    pub(crate) fn submit_triage(&self, run: &str, text: &str) -> Result<i64> {
        self.0.execute(
            "INSERT INTO triage_inbox(run_id,text,submitted_ms) VALUES (?1,?2,?3)",
            params![run, text, timestamp_ms()],
        )?;
        Ok(self.0.last_insert_rowid())
    }

    pub(crate) fn retry_triage(&self, run: &str, id: i64) -> Result<bool> {
        Ok(self.0.execute(
            "UPDATE triage_inbox SET state='queued',decision=NULL
             WHERE run_id=?1 AND id=?2 AND state IN ('failed','routed')
               AND NOT EXISTS (SELECT 1 FROM triage_deliveries WHERE item=?2)",
            params![run, id],
        )? != 0)
    }

    pub(crate) fn defer_triage(&self, run: &str) -> Result<()> {
        self.0.execute(
            "UPDATE triage_inbox SET state='deferred' WHERE run_id=?1 AND state IN ('queued','routed')",
            [run],
        )?;
        Ok(())
    }

    pub(crate) fn restore_deferred_triage(&self, run: &str) -> Result<()> {
        self.0.execute(
            "UPDATE triage_inbox SET state=CASE WHEN decision IS NULL THEN 'queued' ELSE 'routed' END
             WHERE run_id=?1 AND state='deferred'",
            [run],
        )?;
        Ok(())
    }

    pub(crate) fn next_triage(
        &self,
        run: &str,
        include_routed: bool,
    ) -> Result<Option<(i64, String, Option<String>)>> {
        self.0
            .query_row(
                "SELECT id,text,decision FROM triage_inbox WHERE run_id=?1
             AND (state='queued' OR (?2 AND state='routed')) ORDER BY id LIMIT 1",
                params![run, include_routed],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
    }

    pub(crate) fn triage_decision(
        &mut self,
        run: &str,
        id: i64,
        state: &str,
        decision: &serde_json::Value,
    ) -> Result<()> {
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE triage_inbox SET state=?3,decision=?4 WHERE run_id=?1 AND id=?2",
            params![run, id, state, encode_json(decision)?],
        )?;
        let data = encode_json(
            &serde_json::json!({"item":id,"state":state,"decision":decision,"reason":decision["reason"]}),
        )?;
        let changes = tx.execute("INSERT INTO audit_events(run_id,checkpoint,timestamp_ms,phase,cycle,key,kind,actor,data)
            SELECT r.run_id,r.last_sequence,?3,r.phase,r.cycle,
                'triage-route-'||?2||'-'||(SELECT coalesce(max(sequence),0)+1 FROM audit_events),'triage','lead_reviewer',?4
            FROM runs r WHERE r.run_id=?1",params![run,id,timestamp_ms(),data])?;
        if changes != 0 {
            tx.execute(
                "INSERT INTO audit_search(rowid,text) VALUES (?1,?2)",
                params![tx.last_insert_rowid(), data],
            )?;
        }
        tx.commit()
    }

    pub(crate) fn triage_delivered_to(&self, id: i64) -> Result<Vec<String>> {
        let mut statement = self
            .0
            .prepare("SELECT actor FROM triage_deliveries WHERE item=?1")?;
        statement.query_map([id], |row| row.get(0))?.collect()
    }

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
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS worker_requests (
            id INTEGER PRIMARY KEY, run_id TEXT NOT NULL, request TEXT NOT NULL, finished INTEGER NOT NULL DEFAULT 0
        ); CREATE UNIQUE INDEX IF NOT EXISTS one_worker_request ON worker_requests(run_id) WHERE finished=0;")?;
        transaction.execute_batch(AUDIT_SCHEMA)?;
        transaction.execute_batch(WORKER_AUDIT_SCHEMA)?;
        transaction.commit()?;
        Ok(Self(connection))
    }

    pub(crate) fn enqueue_worker(&self, run: &str, request: &str) -> Result<i64> {
        self.0.execute(
            "INSERT INTO worker_requests(run_id,request) VALUES (?1,?2)",
            params![run, request],
        )?;
        Ok(self.0.last_insert_rowid())
    }

    pub(crate) fn worker_stage(
        &self,
        id: i64,
        state: &str,
        detail: &str,
        plan: Option<&serde_json::Value>,
    ) -> Result<()> {
        self.0.execute("INSERT INTO worker_admission(request,state,detail,plan,updated_ms) VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(request) DO UPDATE SET state=excluded.state,detail=excluded.detail,
                plan=coalesce(excluded.plan,worker_admission.plan),updated_ms=excluded.updated_ms",
            params![id,state,detail,plan.map(encode_json).transpose()?,timestamp_ms()])?;
        Ok(())
    }

    pub(crate) fn worker_request(&self, run: &str) -> Result<Option<(i64, String)>> {
        self.0
            .query_row(
                "SELECT id, request FROM worker_requests WHERE run_id=?1 AND finished=0",
                [run],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    pub(crate) fn finish_worker_request(&self, id: i64) -> Result<()> {
        self.0
            .execute("UPDATE worker_requests SET finished=1 WHERE id=?1", [id])?;
        Ok(())
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
        if let Some((sequence, previous_digest, previous_phase, previous_cycle)) = &previous {
            if *previous_digest == digest
                && previous_phase == phase
                && *previous_cycle == cycle
                && details.ipc.is_empty()
                && details.audit.is_empty()
            {
                return Ok(*sequence);
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
        let stamp = timestamp_ms();
        let mut observations = details.audit;
        if previous
            .as_ref()
            .is_none_or(|(_, _, previous_phase, previous_cycle)| {
                previous_phase != phase || *previous_cycle != cycle
            })
        {
            observations.insert(
                0,
                Observation {
                    key: format!("phase-{sequence}"),
                    kind: "phase",
                    actor: String::new(),
                    data: serde_json::json!({"label": phase}),
                },
            );
        }
        for (index, message) in details.ipc.iter().enumerate() {
            let mut data = serde_json::to_value(message)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            data["attempt"] = serde_json::json!(details.workflow.plan_attempt);
            observations.push(Observation {
                key: format!("ipc-{sequence}-{index}"),
                kind: "ipc",
                actor: message.from.clone(),
                data,
            });
        }
        for mut observation in observations {
            if let Some(request) = details.workflow.active_worker_request {
                observation.data["worker_request"] = serde_json::json!(request);
            }
            if observation.kind == "triage_delivery" {
                let item = observation.data["item"].as_i64();
                transaction.execute(
                    "INSERT OR IGNORE INTO triage_deliveries(item,actor) VALUES (?1,?2)",
                    params![item, observation.actor],
                )?;
                transaction.execute("UPDATE triage_inbox SET state=CASE WHEN EXISTS (
                    SELECT 1 FROM json_each(decision,'$.recipients') recipient
                    WHERE NOT EXISTS(SELECT 1 FROM triage_deliveries WHERE item=?2 AND actor=recipient.value)
                    ) THEN 'routed' ELSE 'delivered' END WHERE run_id=?1 AND id=?2",params![run_id,item])?;
            }
            let data = encode_json(&observation.data)?;
            let added = transaction.execute(
                "INSERT OR IGNORE INTO audit_events(run_id,checkpoint,timestamp_ms,phase,cycle,key,kind,actor,data)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![run_id, sequence, stamp, phase, cycle, observation.key, observation.kind, observation.actor, data],
            )?;
            if added != 0 {
                let id = transaction.last_insert_rowid();
                let searchable = data.chars().take(32_768).collect::<String>();
                transaction.execute(
                    "INSERT INTO audit_search(rowid,text) VALUES (?1,?2)",
                    params![
                        id,
                        format!("{} {} {}", observation.actor, observation.kind, searchable)
                    ],
                )?;
            }
        }
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
    pub audit: Vec<Observation>,
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))
}

fn search_phrase(query: Option<&str>) -> Option<String> {
    query
        .filter(|query| !query.trim().is_empty())
        .map(|query| format!("\"{}\"", query.replace('"', "\"\"")))
}

fn decode_column<T: serde::de::DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<T> {
    serde_json::from_str(&row.get::<_, String>(index)?).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn decode_audit(row: &rusqlite::Row<'_>) -> Result<AuditEvent> {
    Ok(AuditEvent {
        sequence: row.get(0)?,
        checkpoint: row.get(1)?,
        timestamp_ms: row.get(2)?,
        phase: row.get(3)?,
        cycle: row.get(4)?,
        key: row.get(5)?,
        kind: row.get(6)?,
        actor: row.get(7)?,
        data: decode_column(row, 8)?,
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
    pub fn web_share_digest(&self, run: &str) -> Result<Option<Vec<u8>>> {
        let exists: bool = self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='web_shares' AND type='table')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        self.0
            .query_row(
                "SELECT digest FROM web_shares WHERE run_id=?1",
                [run],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn worker_admissions(&self, run: &str) -> Result<Vec<serde_json::Value>> {
        if !self.has_audit()? {
            return Ok(Vec::new());
        }
        let mut statement = self.0.prepare("SELECT r.id,r.request,
            coalesce(a.state,CASE WHEN r.finished=0 THEN 'queued' ELSE 'finished' END),a.detail,a.plan,a.updated_ms,
            EXISTS(SELECT 1 FROM audit_events e WHERE e.run_id=r.run_id AND e.phase='OnboardWorker'
                AND e.actor='lead_reviewer' AND e.kind IN ('proposal','contract_error')
                AND json_extract(e.data,'$.worker_request')=r.id),
            EXISTS(SELECT 1 FROM audit_events e WHERE e.run_id=r.run_id AND e.phase='OnboardWorker'
                AND e.kind='ipc' AND json_extract(e.data,'$.worker_request')=r.id)
            FROM worker_requests r LEFT JOIN worker_admission a ON r.id=a.request
            WHERE r.run_id=?1 ORDER BY r.id DESC LIMIT 60")?;
        statement.query_map([run], |row| {
            let plan: Option<String> = row.get(4)?;
            Ok(serde_json::json!({"id":row.get::<_,i64>(0)?,"request":row.get::<_,String>(1)?,"state":row.get::<_,String>(2)?,
                "detail":row.get::<_,Option<String>>(3)?,"plan":plan.and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok()),"timestamp_ms":row.get::<_,Option<u64>>(5)?,
                "has_proposal_activity":row.get::<_,bool>(6)?,"has_ipc":row.get::<_,bool>(7)?}))
        })?.collect()
    }
    pub fn inbox(&self, run: &str, before: Option<i64>) -> Result<Vec<serde_json::Value>> {
        if !self.has_audit()? {
            return Ok(Vec::new());
        }
        let mut statement = self.0.prepare(
            "SELECT id,text,state,decision,submitted_ms,
            (SELECT json_group_array(actor) FROM triage_deliveries WHERE item=id) FROM triage_inbox
            WHERE run_id=?1 AND (?2 IS NULL OR id<?2) ORDER BY id DESC LIMIT 60",
        )?;
        statement.query_map(params![run,before], |row| {
            let decision: Option<String> = row.get(3)?;
            let delivered: String = row.get(5)?;
            Ok(serde_json::json!({"id":row.get::<_,i64>(0)?,"text":row.get::<_,String>(1)?,
                "state":row.get::<_,String>(2)?,"decision":decision.and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok()),"submitted_ms":row.get::<_,u64>(4)?,
                "delivered":serde_json::from_str::<serde_json::Value>(&delivered).unwrap_or_default()}))
        })?.collect()
    }
    pub fn has_audit(&self) -> Result<bool> {
        self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='audit_events')",
            [],
            |row| row.get(0),
        )
    }

    pub fn audit(&self, run: &str, filter: AuditFilter<'_>, limit: u32) -> Result<Vec<AuditEvent>> {
        if !self.has_audit()? {
            return Ok(Vec::new());
        }
        let query = search_phrase(filter.query);
        let order = if filter.after.is_some() {
            "ASC"
        } else {
            "DESC"
        };
        let sql = format!("SELECT sequence,checkpoint,timestamp_ms,phase,cycle,key,kind,actor,
                CASE WHEN kind='tool' THEN json_patch(json_remove(data,'$.arguments'),coalesce(
                    (SELECT json_remove(result.data,'$.full','$.output') FROM audit_events result
                     WHERE result.run_id=?1 AND result.key IN (a.key||'-running',a.key||'-completed',
                       a.key||'-failed',a.key||'-refused',a.key||'-unknown') ORDER BY result.sequence DESC LIMIT 1),'{{}}'))
                WHEN kind='tool_result' THEN json_remove(data,'$.full','$.output')
                WHEN json_type(data,'$.text')='object' THEN json_set(data,'$.text.text',substr(json_extract(data,'$.text.text'),1,32000))
                ELSE data END
            FROM audit_events a WHERE run_id=?1 AND (?2 IS NULL OR sequence<?2) AND (?3 IS NULL OR sequence>?3)
            AND (?4 IS NULL OR actor=?4 OR (kind='ipc' AND EXISTS(SELECT 1 FROM json_each(data,'$.to') WHERE value=?4)))
            AND (?5 IS NULL OR kind=?5 OR (?5='timeline' AND kind<>'tool_result'
                AND NOT (kind='chat' AND coalesce(json_array_length(data,'$.calls'),0)>0
                    AND trim(coalesce(json_extract(data,'$.text.text'),json_extract(data,'$.text'),''),
                        char(9,10,11,12,13,32))=''))
                OR (?5='mcp' AND kind='tool' AND json_extract(data,'$.tool') IN ('mcp_list','tool_search','mcp_call')))
            AND (?6 IS NULL OR cycle=?6) AND (?7 IS NULL OR phase=?7)
            AND (?8 IS NULL OR sequence IN (SELECT rowid FROM audit_search WHERE audit_search MATCH ?8)
                OR (kind='tool' AND EXISTS (SELECT 1 FROM audit_events result
                    WHERE result.run_id=?1 AND result.key IN (a.key||'-completed',a.key||'-failed',a.key||'-refused',a.key||'-unknown')
                    AND result.sequence IN (SELECT rowid FROM audit_search WHERE audit_search MATCH ?8))))
            AND (?10 IS NULL OR actor=?10)
            AND (?11 IS NULL OR EXISTS(SELECT 1 FROM json_each(data,'$.to') WHERE value=?11))
            AND (?12 IS NULL OR json_extract(data,'$.worker_request')=?12)
            AND (?13 IS NULL OR json_extract(data,'$.path')=?13 OR (?13='' AND json_extract(data,'$.path') IS NOT NULL))
            ORDER BY sequence {order} LIMIT ?9");
        let mut statement = self.0.prepare(&sql)?;
        statement
            .query_map(
                params![
                    run,
                    filter.before,
                    filter.after,
                    filter.actor,
                    filter.kind,
                    filter.cycle,
                    filter.phase,
                    query,
                    limit.clamp(1, 200),
                    filter.from,
                    filter.to,
                    filter.worker_request,
                    filter.path
                ],
                decode_audit,
            )?
            .collect()
    }

    pub fn ipc_routes(
        &self,
        run: &str,
        cycle: Option<usize>,
        phase: Option<&str>,
        worker_request: Option<i64>,
    ) -> Result<Vec<serde_json::Value>> {
        self.ipc_routes_filtered(
            run,
            AuditFilter {
                cycle,
                phase,
                worker_request,
                ..AuditFilter::default()
            },
        )
    }

    pub fn ipc_routes_filtered(
        &self,
        run: &str,
        filter: AuditFilter<'_>,
    ) -> Result<Vec<serde_json::Value>> {
        if !self.has_audit()? {
            return Ok(Vec::new());
        }
        let mut statement = self.0.prepare("WITH deliveries AS (
            SELECT a.actor AS sender,d.value AS recipient,json_extract(a.data,'$.delivery') AS state,
                row_number() OVER (message ORDER BY json_extract(a.data,'$.delivery')='queued', a.sequence DESC) AS latest,
                max(CASE WHEN ?8 IS NULL THEN 1 ELSE a.sequence IN (
                    SELECT rowid FROM audit_search WHERE audit_search MATCH ?8
                ) END) OVER message AS matches_search
            FROM audit_events a,json_each(a.data,'$.to') d WHERE a.run_id=?1 AND a.kind='ipc'
            AND (?2 IS NULL OR a.cycle=?2) AND (?3 IS NULL OR a.phase=?3)
            AND (?4 IS NULL OR json_extract(a.data,'$.worker_request')=?4)
            AND (?5 IS NULL OR a.actor=?5 OR d.value=?5)
            AND (?6 IS NULL OR a.actor=?6) AND (?7 IS NULL OR d.value=?7)
            WINDOW message AS (PARTITION BY a.phase,a.cycle,a.actor,d.value,
                json_extract(a.data,'$.sequence'),json_extract(a.data,'$.attempt'),json_extract(a.data,'$.worker_request'))
            ) SELECT sender,recipient,state,count(*) FROM deliveries WHERE latest=1 AND matches_search
            GROUP BY sender,recipient,state")?;
        let query = search_phrase(filter.query);
        statement.query_map(params![run,filter.cycle,filter.phase,filter.worker_request,filter.actor,filter.from,filter.to,query], |row| {
            Ok(serde_json::json!({"from":row.get::<_,String>(0)?,"to":row.get::<_,String>(1)?,"state":row.get::<_,String>(2)?,"count":row.get::<_,usize>(3)?}))
        })?.collect()
    }

    pub fn ipc_origin(&self, reply: &AuditEvent, run: &str) -> Result<Option<AuditEvent>> {
        let (Some(actor), Some(sequence)) = (
            reply.data["reply_to"].as_str(),
            reply.data["reply_sequence"].as_u64(),
        ) else {
            return Ok(None);
        };
        self.0.query_row("SELECT sequence,checkpoint,timestamp_ms,phase,cycle,key,kind,actor,data FROM audit_events
            WHERE run_id=?1 AND kind='ipc' AND sequence<?2 AND phase=?3 AND cycle=?4 AND actor=?5
            AND json_extract(data,'$.sequence')=?6 AND json_extract(data,'$.delivery')='queued'
            AND json_extract(data,'$.attempt') IS json_extract(?7,'$.attempt')
            AND json_extract(data,'$.worker_request') IS json_extract(?7,'$.worker_request')
            AND EXISTS(SELECT 1 FROM json_each(data,'$.to') WHERE value=?8)
            ORDER BY sequence DESC LIMIT 1",
            params![run,reply.sequence,reply.phase,reply.cycle,actor,sequence,encode_json(&reply.data)?,reply.actor], decode_audit).optional()
    }

    pub fn ipc_transitions(&self, event: &AuditEvent, run: &str) -> Result<Vec<serde_json::Value>> {
        let Some(wire) = event.data["sequence"].as_u64() else {
            return Ok(Vec::new());
        };
        let mut statement = self.0.prepare("SELECT audit.key,recipient.value,json_extract(audit.data,'$.delivery'),audit.timestamp_ms
            FROM audit_events audit,json_each(audit.data,'$.to') recipient
            WHERE run_id=?1 AND kind='ipc' AND phase=?2 AND cycle=?3 AND actor=?4
                AND json_extract(audit.data,'$.sequence')=?5
                AND json_extract(audit.data,'$.attempt') IS json_extract(?6,'$.attempt')
                AND json_extract(audit.data,'$.worker_request') IS json_extract(?6,'$.worker_request')
            ORDER BY audit.sequence,recipient.value")?;
        statement.query_map(params![run,event.phase,event.cycle,event.actor,wire,encode_json(&event.data)?], |row| {
            Ok(serde_json::json!({"key":row.get::<_,String>(0)?,"recipient":row.get::<_,String>(1)?,
                "state":row.get::<_,String>(2)?,"timestamp_ms":row.get::<_,u64>(3)?}))
        })?.collect()
    }

    pub fn audit_event(&self, run: &str, key: &str) -> Result<Option<AuditEvent>> {
        if !self.has_audit()? {
            return Ok(None);
        }
        self.0
            .query_row(
                "SELECT sequence,checkpoint,timestamp_ms,phase,cycle,key,kind,actor,data
            FROM audit_events WHERE run_id=?1 AND key=?2",
                params![run, key],
                decode_audit,
            )
            .optional()
    }

    pub fn audit_outcome(&self, run: &str, key: &str) -> Result<Option<AuditEvent>> {
        if !self.has_audit()? {
            return Ok(None);
        }
        self.0.query_row("SELECT sequence,checkpoint,timestamp_ms,phase,cycle,key,kind,actor,data
            FROM audit_events WHERE run_id=?1 AND key IN (?2||'-running',?2||'-completed',?2||'-failed',
                ?2||'-refused',?2||'-unknown') ORDER BY sequence DESC LIMIT 1",
            params![run,key], decode_audit).optional()
    }

    pub fn tool_stages(&self, run: &str, key: &str) -> Result<Vec<serde_json::Value>> {
        let mut statement = self.0.prepare(
            "SELECT key,timestamp_ms,json_extract(data,'$.state')
             FROM audit_events WHERE run_id=?1 AND key IN
                 (?2,?2||'-running',?2||'-completed',?2||'-failed',?2||'-refused',?2||'-unknown')
             ORDER BY sequence",
        )?;
        statement
            .query_map(params![run, key], |row| {
                Ok(serde_json::json!({
                    "key": row.get::<_, String>(0)?,
                    "timestamp_ms": row.get::<_, u64>(1)?,
                    "state": row.get::<_, String>(2)?,
                }))
            })?
            .collect()
    }

    pub fn audit_head(&self, run: &str) -> Result<u64> {
        if !self.has_audit()? {
            return Ok(0);
        }
        self.0.query_row(
            "SELECT coalesce(max(sequence),0) FROM audit_events WHERE run_id=?1",
            [run],
            |row| row.get(0),
        )
    }

    pub fn audit_updates(&self, run: &str, after: u64) -> Result<Vec<serde_json::Value>> {
        if !self.has_audit()? {
            return Ok(Vec::new());
        }
        let mut statement = self.0.prepare("SELECT sequence,key,kind,actor FROM audit_events WHERE run_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 128")?;
        statement.query_map(params![run,after], |row| Ok(serde_json::json!({
            "sequence":row.get::<_,u64>(0)?,"key":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"actor":row.get::<_,String>(3)?
        })))?.collect()
    }

    pub fn snapshot(&self, run: &str, sequence: Option<u64>) -> Result<Option<Vec<u8>>> {
        self.0
            .query_row(
                "SELECT b.content FROM events e JOIN checkpoint_blobs b ON b.digest=e.checkpoint
            WHERE e.run_id=?1 AND (?2 IS NULL OR e.sequence=?2) ORDER BY e.sequence DESC LIMIT 1",
                params![run, sequence],
                |row| row.get(0),
            )
            .optional()
    }

    pub fn budget_resume_checkpoint(&self, run: &str) -> Result<Option<(u64, String)>> {
        // Budget exhaustion records the preceding checkpoint with only its phase and
        // assessment replaced. Review and plan exhaustion advance the phase machine.
        self.0
            .query_row(
                "SELECT prior.sequence, prior.phase FROM runs r
                 JOIN events stop ON stop.sequence=r.last_sequence
                 JOIN checkpoint_blobs terminal ON terminal.digest=stop.checkpoint
                 JOIN events prior ON prior.sequence=(
                     SELECT sequence FROM events WHERE run_id=r.run_id AND sequence<stop.sequence
                     AND phase!='Exhausted' ORDER BY sequence DESC LIMIT 1
                 )
                 JOIN checkpoint_blobs saved ON saved.digest=prior.checkpoint
                 WHERE r.run_id=?1 AND r.phase='Exhausted' AND prior.cycle=stop.cycle
                 AND prior.phase NOT IN ('Approved','Rejected','Publish')
                 AND json_extract(CAST(terminal.content AS TEXT),'$.assessment') IN (
                     'Run budget exhausted: active time',
                     'Run budget exhausted: provider attempts',
                     'Run budget exhausted: metered tokens'
                 )
                 AND json_remove(CAST(terminal.content AS TEXT),'$.phase','$.assessment')
                   = json_remove(CAST(saved.content AS TEXT),'$.phase','$.assessment')",
                [run],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }
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

    pub fn events_before(
        &self,
        run: &str,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<RunEvent>> {
        let mut statement = self.0.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE run_id=?1 AND (?2 IS NULL OR sequence<?2)
            ORDER BY sequence DESC LIMIT ?3"
        ))?;
        statement
            .query_map(params![run, before, limit.min(80)], decode_event)?
            .collect()
    }

    pub fn event_at(&self, run: &str, sequence: u64) -> Result<Option<RunEvent>> {
        self.0
            .query_row(
                &format!("SELECT {EVENT_COLUMNS} FROM events WHERE run_id=?1 AND sequence=?2"),
                params![run, sequence],
                decode_event,
            )
            .optional()
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
